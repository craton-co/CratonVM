#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    pub(in crate::ir_lower) fn lower_terminator(&mut self, term: NodeId, block_idx: usize) {
        // Same net at the other end of the block: a terminator is never a
        // planned consumer.
        self.refuse_carry("the block's terminator was reached first");
        // The terminator's own pc BEFORE the back-edge poll (round 11,
        // irlower): the poll's safepoint map records `cur_bci` as the bci a
        // thread parked there is at (`sp_id_bcis` -- stack traces, thread
        // dumps), and that is the branch, not whichever data node was lowered
        // last in the block.
        self.cur_node_pc = self.graph.nodes[term as usize].bytecode_pc;
        self.note_row_census_node();
        if let Some(pc) = self.graph.nodes[term as usize].bytecode_pc {
            self.cur_bci = pc;
        }
        if self.schedule.blocks[block_idx]
            .successors
            .iter()
            .any(|&succ| succ <= block_idx)
        {
            // JIT round 25 item 1a (wave 3): a frame parked at this poll is
            // read through its homes; the site-named values its branch's frame
            // state names are stored first. `cur_term` is set around the poll
            // so a site resolved inside it (its mode exit) knows where it is.
            self.cur_term = Some(term);
            self.emit_inspection_spills(term, true, block_idx);
            // No mode exit: this poll runs before the branch decides whether
            // the header is next (see `emit_safepoint_poll`).
            self.emit_safepoint_poll(Some(block_idx), None);
            self.cur_term = None;
        }
        // Same reset discipline as `cur_node_pc`: a terminator carries no copy
        // identity of its own (the unroller clones no control node), so this
        // clears rather than inheriting the last data node's.
        self.cur_node_frame = self.graph.nodes[term as usize].frame_snapshot;
        // JIT round 19 wave 10 (lane tok10, i64-L2): the token-read guard's
        // `term` scope, around the terminator's own operand reads (after the
        // back-edge poll, which describes a frame rather than reading an
        // operand). Covers a fused compare's operands, read at the branch.
        // Nothing computed while the guard is off.
        let scope = if self.token_read_guard {
            let ids = self.term_token_scope_nodes(term);
            self.token_scope_enter(&ids, TokenScope::Term)
        } else {
            None
        };
        // JIT round 25 item 1a: a tail carry into this terminator is read by
        // its first operand load (`gp_load_value` checks `cur_term`); one
        // planned for anything else, or left unread, refuses the attempt.
        if matches!(self.tail_carry, Some((_, cons, _)) if cons != term) {
            self.refuse_tail_carry("the block's terminator was reached first");
        }
        self.cur_term = Some(term);
        self.lower_terminator_op(term, block_idx);
        self.cur_term = None;
        if self.tail_carry.is_some() {
            self.refuse_tail_carry("its terminator finished without reading it");
        }
        self.token_scope_exit(scope);
    }

    /// The op arm of [`Self::lower_terminator`]: everything after the poll.
    /// Split out (JIT round 19 wave 10) so the `term` token scope is restored
    /// on every one of its early returns.
    pub(in crate::ir_lower) fn lower_terminator_op(&mut self, term: NodeId, block_idx: usize) {
        let node = &self.graph.nodes[term as usize];
        match &node.op {
            // ── cov-07: athrow ─────────────────────────────────────────
            //
            // Same ABI as the single-pass backend's `0xbf` arm
            // (`x64/bytecode_walk.rs`): `jit_throw_exception(exc_ptr, bci) ->
            // i64::MIN` in RAX, always. No `vm_ptr` argument (see the field
            // doc on `throw_exception`), no post-call CMP — the helper never
            // returns anything but the sentinel, so this always takes the
            // exceptional exit. That reuses `emit_call_exc_stub`'s shared
            // bail stub exactly as every other exceptional `Op::Call`/
            // `Op::CheckCast` exit does: run the epilogue and propagate
            // `i64::MIN`, which `execute_jit_call`
            // (`vm/src/runtime/interpreter/jit_bridge.rs`) then routes
            // through `route_jit_exception_through_method` — the interpreter
            // resolves the handler (if any), never this compiled frame.
            //
            // `jit_throw_exception` stamps its OWN `bci` argument onto
            // `JitSignals::athrow_bci` before returning (`set_jit_pending_
            // exception_with_bci`), which is what gives the routing a real
            // throw pc for the range test against this method's own
            // exception table — exactly the RBC.6 fix the single-pass
            // backend already ships. This is the ONE terminator whose own
            // helper call does that job; a nested `Op::Call` /
            // `Op::CheckCast` exceptional exit needs the SEPARATE
            // `jit_set_throw_bci` stamp, which `emit_call_exc_stub` now emits
            // per distinct throw-site bci (cov-07 residual, closed — see that
            // function). The stub re-stamps this bci on the way out, which is
            // a no-op for this arm and keeps the stub's contract uniform.
            Op::Throw => {
                let exc = node.inputs[2];
                self.gp_load_value(CALL_ARG_REGS[0], exc);
                // `athrow` is refused inside a spliced body today; translating
                // anyway keeps the rule "a bci handed to the interpreter goes
                // through `resume_bci`" without an exception to remember.
                let bci = self.resume_bci(self.deopt_pc_of(node));
                self.emit_mov_reg_imm64(CALL_ARG_REGS[1], self.throw_stamp_word(bci as u64));
                self.emit_mov_reg_imm64(RAX, self.throw_exception as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                self.buf.emit_byte(0xE9); // JMP shared exception epilogue
                let exception_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.push_call_exc_patch(exception_patch);
            }
            Op::Return => {
                let mut returned_a_value = false;
                if node.inputs.len() > 1 {
                    // Has return value — move to RAX
                    let val_id = node.inputs[1];
                    if val_id != NO_NODE {
                        // An `int` result leaves sign-extended: see
                        // `gp_load_abi_value` (a single-pass CALLER pushes RAX as an
                        // int slot and its `i2l` is a no-op).
                        self.gp_load_abi_value(RAX, val_id);
                        returned_a_value = true;
                    }
                }
                if !returned_a_value {
                    // Zero RAX on the normal VOID-return path, exactly as the
                    // single-pass backend's `0xb1` arm has always done.
                    //
                    // `i64::MIN` in the return register is this VM's "the callee
                    // trapped" sentinel, and a void method has no return value —
                    // so without this, RAX carries whatever the method's last
                    // operation left there. Every consumer reads that raw
                    // register: the single-pass inline MIC/PIC cascade's
                    // `emit_inline_callee_deopt_check`, the megamorphic hashed
                    // stub's, `jit_invoke_virtual_mic`'s `rc == i64::MIN`, and
                    // the interpreter's post-JIT drain. A false positive is not
                    // a wasted branch: `handle_compiled_callee_deopt_sentinel`
                    // resumes a stashed callee frame and drains the thread's
                    // entire pending-signal record.
                    //
                    // Measured on the Hazelcast XSD failure: one `Config.load()`
                    // serviced 11 callee "deopts" and EVERY ONE was a void
                    // callee — `QName.setValues`, `XMLAttributesImpl
                    // .addAttributeNS`, `ValidatorHandlerImpl.fillXMLAttribute`
                    // and `.fillXMLAttributes2` — none of which can throw.
                    //
                    // XOR EAX, EAX (31 C0) — zero-extends to RAX.
                    self.buf.emit(&[0x31, 0xC0]);
                }
                self.emit_epilogue();
            }
            // ── tableswitch / lookupswitch ─────────────────────────────
            //
            // Three shapes, and the choice is made on the number of case
            // edges that go somewhere OTHER than the default:
            //
            //   * none, or few — a compare chain, which is the floor and the
            //     right answer for two or three cases: an indirect branch
            //     through a table is one the predictor gets wrong on anything
            //     but a repeating key, and four `CMP`/`JE` pairs are not;
            //   * dense — a jump table, one `SUB`, one `CMP`, one `JAE` and an
            //     indirect `JMP`, independent of the case count;
            //   * sparse but wide — a balanced binary search over the live
            //     keys, `O(log n)` signed compares and no table bytes. This is
            //     the `tableswitch` that javac padded with default slots, and
            //     the shape where the historical chain was worst.
            //
            // `ir::switch_is_dense` is the SAME predicate `IrBuilder` asked
            // before it built this node; asking it again here is what keeps
            // the builder's notion of dense and the lowerer's from drifting.
            // It is asked with the LIVE count rather than the span, because
            // what the table costs is one slot per span slot and what it buys
            // is one O(1) dispatch per key that goes somewhere.
            //
            // Every shape is total over the `int` range, and every shape's
            // "no case matched" path is the node's own default projection —
            // never a fall-through into whatever the layout put next.
            Op::Switch { low, high } => {
                let (low, high) = (*low, *high);
                let Some(edges) = self.switch_edge_blocks(term, low, high) else {
                    self.latch_bailout(Bailout::with_context(
                        BailoutReason::Internal(
                            "ir_lower: an Op::Switch edge names no scheduled block",
                        ),
                        format!("switch [{low}, {high}] at n{term}"),
                    ));
                    return;
                };
                // `switch_edge_blocks` answers `None` for an empty list, so
                // the default edge is always present.
                let Some((&default_block, case_blocks)) = edges.split_last() else {
                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                        "ir_lower: an Op::Switch has no default edge",
                    )));
                    return;
                };
                // A switch edge opens at its own `Op::Proj`, so it is never a
                // `Merge`/`Region` block and never carries phi copies of its
                // own — the copies belong to the edge from that `Proj` block
                // to the merge, which `lower_block`'s no-terminator path
                // emits. Nothing below calls `emit_phi_copies`, so that has to
                // be a fact rather than a belief: an edge that did need copies
                // would have them silently skipped, which is wrong code.
                if edges
                    .iter()
                    .any(|&b| self.edge_has_phi_copies(block_idx, b))
                {
                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                        "ir_lower: an Op::Switch edge carries phi copies",
                    )));
                    return;
                }
                let live = self.switch_live_cases(low, case_blocks, default_block);
                if live.is_empty() {
                    // Every key, listed or not, reaches the default target.
                    // Legal bytecode (`tableswitch` with every slot pointing at
                    // the default) and the only shape with no compare at all.
                    self.emit_jmp_to_block_or_fall_through(default_block, block_idx);
                    return;
                }
                // The key, as the JVMS switch opcodes popped it: a 32-bit
                // signed `int`. Every compare below is signed and every table
                // index is derived from it by a 32-bit `SUB`, which is what
                // makes a negative `lookupswitch` key work without a special
                // case.
                let Some(&key_id) = node.inputs.get(1) else {
                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                        "ir_lower: an Op::Switch has no key input",
                    )));
                    return;
                };
                self.gp_load_value(RAX, key_id);
                if live.len() <= crate::ir::SWITCH_CHAIN_MAX_CASES {
                    for &(key, target) in &live {
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&key.to_le_bytes());
                        self.buf.emit(&[0x0F, 0x84]); // JE rel32
                        let patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.branch_patches.push((patch, target));
                    }
                    // The trailing edge, and the only transfer in this arm
                    // with nothing emitted after it — so it is the only one
                    // allowed to become a fall-through. See
                    // `emit_jmp_to_block_or_fall_through`.
                    self.emit_jmp_to_block_or_fall_through(default_block, block_idx);
                } else if crate::ir::switch_is_dense(low, high, live.len()) {
                    self.emit_switch_jump_table(low, case_blocks, default_block);
                } else {
                    self.emit_switch_binary_search(&live, default_block);
                }
            }
            Op::If => {
                let cond_id = node.inputs[1];
                // Round nio21 wave 3 (lane L2): an ARMED call-site exception
                // edge (`plan_call_throw_edges`). Its condition is `0` on every
                // path that reaches this `If`; the true edge is entered only by
                // the call's exception pad, which jumps into that block. So the
                // normal path pays no TEST and no Jcc: a transfer to the false
                // edge, as for a constant condition. An unarmed site's `If`
                // takes the general lowering below and reads its zero flag.
                if let Some(normal) = self.armed_call_throw_normal_block(term, cond_id, block_idx) {
                    self.emit_phi_copies(block_idx, normal);
                    self.emit_jmp_to_block_or_fall_through(normal, block_idx);
                    return;
                }
                // Round 12 wave 4 (lane iropt3, W3-2): a constant condition
                // (a compare `ir_optimize::fold_range_decided_compares` or
                // constant folding decided) transfers to one edge. No TEST, no
                // Jcc, and the dead edge's phi copies are not emitted.
                if let Some(live) = self.constant_if_live_block(term, cond_id, block_idx) {
                    self.emit_phi_copies(block_idx, live);
                    self.emit_jmp_to_block_or_fall_through(live, block_idx);
                    return;
                }
                // RB25-1 step 2: the flag's store was left out on the promise
                // that this `If` folds; it did not, so it would read a word
                // nothing wrote.
                if self.flag_store_elided.get(cond_id as usize).copied().unwrap_or(false) {
                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                        "ir_lower: an If over an elided jump-entered flag store did not fold",
                    )));
                    return;
                }
                // 2026-09-02: a compare whose only consumer is this `If` is
                // lowered HERE as `cmp; jcc` instead of `setcc; movzx; store;
                // reload; test; jcc`. `fused_cc` is the Jcc condition byte
                // under which the branch is TAKEN (the `Op::Cmp` result is
                // nonzero); `None` keeps the boolean-in-RAX shape.
                let fused_cc: Option<u8> = match self.graph.nodes.get(cond_id as usize) {
                    Some(cmp_node)
                        if self
                            .fused_cmp
                            .get(cond_id as usize)
                            .copied()
                            .unwrap_or(false) =>
                    {
                        match cmp_node.op {
                            // A fused `fcmp`/`dcmp` under the compare (round 9
                            // wave 8, irl8; see `fused_fcmp`): one `UCOMIS` of
                            // the two FP values, in the order
                            // `fcmp_fused_branch` chose so that the NaN answer
                            // the `l`/`g` bias demands is the Jcc's own.
                            // JIT rx27 wave 4 (lane regalloc): `(x & mask) ==
                            // 0` as `TEST x, mask` (`plan_and_zero_tests`).
                            Op::Cmp(cc)
                                if self
                                    .test_fused_cond
                                    .get(cond_id as usize)
                                    .copied()
                                    .flatten()
                                    .is_some() =>
                            {
                                if let Some((x, imm, wide)) =
                                    self.test_fused_cond.get(cond_id as usize).copied().flatten()
                                {
                                    self.emit_and_zero_test(x, imm, wide);
                                }
                                Some(cc.x64_cc())
                            }
                            Op::Cmp(cc)
                                if self
                                    .fused_fcmp_branch(cc, cmp_node.inputs[0], cmp_node.inputs[1])
                                    .is_some() =>
                            {
                                match self.fused_fcmp_branch(
                                    cc,
                                    cmp_node.inputs[0],
                                    cmp_node.inputs[1],
                                ) {
                                    Some((fx, fy, is_d, jcc)) => {
                                        self.fp_load_value(XMM0, fx, is_d);
                                        self.fp_load_value(XMM1, fy, is_d);
                                        if is_d {
                                            // UCOMISD XMM0, XMM1
                                            self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]);
                                        } else {
                                            // UCOMISS XMM0, XMM1
                                            self.buf.emit(&[0x0F, 0x2E, 0xC1]);
                                        }
                                        Some(jcc)
                                    }
                                    None => None,
                                }
                            }
                            Op::Cmp(cc) => {
                                if let Some((recv, class_id, invert)) =
                                    self.fused_final_instanceof_cmp_info(cond_id)
                                {
                                    self.gp_load_value(RAX, recv);
                                    self.emit_exact_class_test_flags_for(recv, class_id);
                                    Some(if invert { 0x85 } else { 0x84 })
                                } else {
                                    // A fused `lcmp` under the compare: branch on
                                    // the two longs themselves (see `fused_lcmp`).
                                    // Their `Long` type makes the CMP below 64-bit.
                                    let (a, b) = self
                                        .fused_lcmp_operands(cmp_node.inputs[0], cmp_node.inputs[1])
                                        .unwrap_or((cmp_node.inputs[0], cmp_node.inputs[1]));
                                    self.emit_fused_int_compare(a, b);
                                    Some(cc.x64_cc())
                                }
                            }
                            // JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-1):
                            // a fused `ExactClassIs` (`prepare_fusion_tables`,
                            // `CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH`). ZF=1
                            // iff the class matches, so the branch is taken
                            // under `JE`.
                            Op::ExactClassIs { class_id } if cmp_node.inputs.len() == 1 => {
                                let recv = cmp_node.inputs[0];
                                self.gp_load_value(RAX, recv);
                                self.emit_exact_class_test_flags_for(recv, class_id);
                                Some(0x84)
                            }
                            Op::InstanceOf { .. } if cmp_node.inputs.len() >= 3 => {
                                if let Some(class_id) =
                                    Self::final_instanceof_class_id(&cmp_node.op)
                                {
                                    let recv = cmp_node.inputs[2];
                                    self.gp_load_value(RAX, recv);
                                    self.emit_exact_class_test_flags_for(recv, class_id);
                                    Some(0x84)
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if fused_cc.is_none() {
                    // Load condition into RAX
                    self.gp_load_value(RAX, cond_id);
                    // TEST EAX, EAX  (does not disturb RAX; sets ZF)
                    self.buf.emit(&[0x85, 0xC0]);
                }

                // Snapshot successor block indices (immutable borrow ends
                // here so `emit_phi_copies` can borrow `self` mutably).
                let succ0 = self.schedule.blocks[block_idx].successors.first().copied();
                let succ1 = self.schedule.blocks[block_idx].successors.get(1).copied();
                // JIT round 21, lane tcfuse (`CRATONVM_JIT_TYPECHECK_BRANCH_FUSION`):
                // where each edge's code begins -- `(block, None)` for an edge
                // the branch names directly, `(block, Some(pos))` for one
                // whose phi copies start at `pos` -- `[true, false]`, for a
                // fused `instanceof`'s inline answers (resolved below).
                let mut tc_edges: Option<[(usize, Option<usize>); 2]> = None;

                match (succ0, succ1) {
                    (Some(true_block), Some(false_block))
                        if self.fused_branch_on
                            && !self.edge_has_phi_copies(block_idx, true_block)
                            && !self.edge_has_phi_copies(block_idx, false_block) =>
                    {
                        // 2026-09-02: neither edge carries phi copies, so the
                        // branch needs no trampolines at all -- one Jcc to
                        // the far edge, and the near edge either falls
                        // through into the next emitted block or takes one
                        // JMP. Same polarity rule as the general layout below:
                        // the taken (true) edge is the near one unless the
                        // profile says this branch is usually not taken.
                        let hint = node
                            .bytecode_pc
                            .and_then(|pc| self.branch_hints.get(&pc).copied());
                        // ── Which edge FALLS THROUGH ─────────────────────
                        //
                        // The profile when there is one, the LAYOUT when there
                        // is not. Both were previously "the profile", and the
                        // profile is empty on a default run.
                        //
                        // # What the missing case cost
                        //
                        // `branch_hints` is populated only under
                        // `CRATONVM_TIER_PGO`, so with no hint this arm always
                        // made the `true` edge the near one. For a javac
                        // counted loop that is exactly backwards, and for a
                        // reason the range-BCE pass already wrote down:
                        // **javac puts the loop body on the FALSE edge.**
                        // `for (i = 0; i < n; i++)` compiles to
                        // `if_icmpge exit`, the builder preserves that
                        // polarity, so `Proj(0)` — this arm's `true_block` —
                        // is the loop EXIT.
                        //
                        // On `probes/FieldLoop.java` `sum` the result was
                        //
                        //     jl  +5      ; to the loop body, TAKEN every
                        //     jmp exit    ; iteration, skipping this
                        //
                        // where the loop body was already the next block the
                        // scheduler had laid out. One instruction and one
                        // TAKEN branch per iteration, to reach the block
                        // physically underneath.
                        //
                        // # Why the layout is the right tie-break, and only
                        //   the tie-break
                        //
                        // `ir_schedule::layout_hot_paths` is default-ON and
                        // needs no profile — `static_branch_probs` derives its
                        // probabilities from loop structure alone — and it has
                        // ALREADY decided which successor should be physically
                        // next. Emission order is block index order, so
                        // `block_idx + 1` IS that decision, and it is the same
                        // test `emit_jmp_to_block_or_fall_through` reads.
                        // At most one successor can be next, so this never has
                        // to choose between two layout facts.
                        //
                        // It does NOT override a hint that exists.
                        // `step4_ir_lower_consumes_branch_bias_hint` is the
                        // contract that says so, and it caught this arm
                        // overriding one: a profile is evidence about
                        // FREQUENCY, which a layout heuristic is only guessing
                        // at, and the two disagreeing is a fact worth leaving
                        // visible rather than silently resolving. Where they
                        // disagree the cost is one `JMP rel32` and no extra
                        // taken branch, so deferring to the profile is cheap.
                        let favor_false = match hint {
                            Some(h) => !h,
                            None if !ir_branch_layout_polarity_enabled() => false,
                            None if false_block == block_idx + 1 => true,
                            None if true_block == block_idx + 1 => false,
                            None => false,
                        };
                        if hint.is_none() && favor_false {
                            BRANCH_POLARITY_FROM_LAYOUT
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        // Jcc byte under which control goes to the FAR edge.
                        let (jcc_far, near_block, far_block) = match fused_cc {
                            Some(cc) if favor_false => (cc, false_block, true_block),
                            Some(cc) => (cc ^ 1, true_block, false_block),
                            None if favor_false => (0x85u8, false_block, true_block), // JNE
                            None => (0x84u8, true_block, false_block),                // JE
                        };
                        self.buf.emit(&[0x0F, jcc_far]);
                        let far_patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.branch_patches.push((far_patch, far_block));
                        if near_block != block_idx + 1 {
                            self.buf.emit_byte(0xE9); // JMP near_block
                            let near_patch = self.buf.pos();
                            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                            self.branch_patches.push((near_patch, near_block));
                        }
                        tc_edges = Some([(true_block, None), (false_block, None)]);
                    }
                    (Some(true_block), Some(false_block))
                        if self.fused_branch_on
                            && (!self.edge_has_phi_copies(block_idx, true_block)
                                || !self.edge_has_phi_copies(block_idx, false_block)) =>
                    {
                        // Round 9: exactly ONE edge carries phi copies (the arm
                        // above took the none-on-either case). The copy-free
                        // edge needs no trampoline — the Jcc can name its block
                        // directly — and the copies edge then runs straight on
                        // from the Jcc, ending in a fall-through or one JMP.
                        //
                        // This is the shape of every bottom-tested loop latch
                        // (`do { } while (c)`, and javac's `while` after loop
                        // rotation): the back edge carries the header phis'
                        // copies, the exit edge carries nothing. The general
                        // layout below paid a TEST-rebuild (`SETcc; TEST`), a
                        // Jcc and an unconditional JMP on the copy-free edge;
                        // here it is one Jcc, and the copies edge loses its
                        // Jcc-around-trampoline too.
                        //
                        // No polarity choice is left to make: the copy-free
                        // edge is the one the Jcc names and the copies edge is
                        // the one that runs on, whatever a profile hint says,
                        // because the copies have to sit on straight-line code
                        // after the branch. (Both outcomes lose a transfer, so
                        // a hint has nothing to trade here.)
                        //
                        // `to_true`: the Jcc byte under which control goes to
                        // the true edge. A fused compare's `cc` is "branch
                        // taken"; the boolean shape was `TEST EAX, EAX` above,
                        // so JNE is "true". `^ 1` inverts an x86 condition.
                        let to_true = fused_cc.unwrap_or(0x85);
                        let (jcc_direct, direct_block, copies_block) =
                            if !self.edge_has_phi_copies(block_idx, true_block) {
                                (to_true, true_block, false_block)
                            } else {
                                (to_true ^ 1, false_block, true_block)
                            };
                        self.buf.emit(&[0x0F, jcc_direct]);
                        let direct_patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.branch_patches.push((direct_patch, direct_block));
                        let copies_at = self.buf.pos();
                        tc_edges = Some(if direct_block == true_block {
                            [(true_block, None), (false_block, Some(copies_at))]
                        } else {
                            [(true_block, Some(copies_at)), (false_block, None)]
                        });
                        self.emit_phi_copies(block_idx, copies_block);
                        // Trailing transfer: nothing is emitted after it.
                        self.emit_jmp_to_block_or_fall_through(copies_block, block_idx);
                    }
                    (Some(true_block), Some(false_block)) => {
                        // A fused compare leaves the flags for `cc`; the
                        // general layout below wants "ZF set ⇔ condition
                        // false" (a TEST on the boolean). Rebuild that
                        // contract from the flags with one SETcc + TEST so
                        // the phi-copy trampolines stay exactly as they were.
                        if let Some(cc) = fused_cc {
                            self.buf.emit(&[0x0F, cc + 0x10, 0xC0]); // SETcc AL
                            self.buf.emit(&[0x84, 0xC0]); // TEST AL, AL
                        }
                        // BUG FIX [jit-irlower #2]: phi copies must execute on
                        // the edge actually taken, so the conditional branch
                        // splits the critical edges. Default layout:
                        //   TEST; JE around_true;
                        //   <true-edge phi copies>; JMP true_block;
                        //   around_true: <false-edge phi copies>; JMP false_block;
                        //
                        // wire-tiered-manager Step 4 (PGO handoff C1 → C2): pick
                        // the conditional-branch polarity from the profiled branch
                        // bias. `cmp` is nonzero exactly when the JVM branch is
                        // TAKEN (`successors[0]` = the taken edge), so the default
                        // layout makes the *taken* edge the fall-through — its
                        // forward `JE` is statically predicted not-taken. When the
                        // profile says this branch is usually NOT taken we invert
                        // to a `JNE` so the *not-taken* (false) edge becomes the
                        // fall-through instead. The two layouts are semantically
                        // identical — only the predicted/fall-through edge and the
                        // block order differ, and the phi copies stay attached to
                        // their own edge in both. With no hint for this PC (the
                        // default, and whenever profiling is off) `favor_false` is
                        // false and the emitted bytes are unchanged.
                        let favor_false = node
                            .bytecode_pc
                            .and_then(|pc| self.branch_hints.get(&pc).copied())
                            == Some(false);

                        // `(jcc, first_block, second_block)`: `jcc` skips the
                        // fall-through (`first_block`) to `second_block`.
                        //   default  (favor taken): JE  skips true→false; true first
                        //   inverted (favor !taken): JNE skips false→true; false first
                        let (jcc_second_byte, first_block, second_block) = if favor_false {
                            (0x85u8, false_block, true_block) // JNE
                        } else {
                            (0x84u8, true_block, false_block) // JE
                        };

                        // Jcc around_first (skip the fall-through edge).
                        self.buf.emit(&[0x0F, jcc_second_byte]);
                        let jcc_patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                        // Fall-through (favored) edge.
                        let first_at = self.buf.pos();
                        self.emit_phi_copies(block_idx, first_block);
                        self.buf.emit_byte(0xE9); // JMP first_block
                        let jmp_first = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.branch_patches.push((jmp_first, first_block));

                        // around_first: the jumped-to edge. Patch the Jcc here.
                        let around_first = self.buf.pos();
                        let rel = around_first as i32 - (jcc_patch as i32 + 4);
                        // codegen -- tolerated on an overflowed buffer; see
                        // `Self::patch_or_bail` / `patch_rel32_to_here`.
                        Self::patch_or_bail(&mut self.buf, jcc_patch, rel);
                        tc_edges = Some(if first_block == true_block {
                            [
                                (true_block, Some(first_at)),
                                (false_block, Some(around_first)),
                            ]
                        } else {
                            [
                                (true_block, Some(around_first)),
                                (false_block, Some(first_at)),
                            ]
                        });
                        self.emit_phi_copies(block_idx, second_block);
                        // The TRAILING edge of this arm, and the only one of
                        // the two that may fall through: nothing is emitted
                        // after it, whereas the first edge's `JMP` is followed
                        // by the `around_first` trampoline and so can never be
                        // dropped. See `emit_jmp_to_block_or_fall_through`.
                        self.emit_jmp_to_block_or_fall_through(second_block, block_idx);
                    }
                    (Some(only_block), None) => {
                        // Degenerate single-successor If: only one projection
                        // has a block, so the other outcome has no code to go
                        // to and control must reach `only_block` either way.
                        //
                        // Round 9: this used to be `JNE only_block` after
                        // unconditional copies, "preserving the taken-on-
                        // nonzero shape" — which is wrong in two ways. The
                        // lone edge may be `Proj(1)` (the schedule pushes
                        // whichever projections exist), so JNE took it on the
                        // WRONG outcome; and under a fused compare the flags
                        // are the CMP's, so JNE meant `a != b` whatever `cc`
                        // was (a fused `Eq` never took it). Either way the
                        // not-taken path fell into whatever block the layout
                        // put next. The copies were already unconditional;
                        // the transfer now is too.
                        self.emit_phi_copies(block_idx, only_block);
                        self.emit_jmp_to_block_or_fall_through(only_block, block_idx);
                    }
                    _ => {}
                }
                // JIT round 21, lane tcfuse: the fused `instanceof`'s inline
                // answers enter the edge they decide at its first byte, as the
                // branch above does. Anything else leaves the pending answers
                // unresolved, which `lower_block` refuses.
                let pending = match tc_edges {
                    Some(_) => self.typecheck_branch_pending.take(),
                    None => None,
                };
                if let (Some(edges), Some((cond, one, zero, one_is_true))) = (tc_edges, pending) {
                    if cond == cond_id {
                        let (one_edge, zero_edge) = if one_is_true {
                            (edges[0], edges[1])
                        } else {
                            (edges[1], edges[0])
                        };
                        for (patches, (block, at)) in [(one, one_edge), (zero, zero_edge)] {
                            for p in patches {
                                match at {
                                    Some(pos) => {
                                        // Cast: offsets inside one code buffer.
                                        let rel = pos as i32 - (p as i32 + 4);
                                        Self::patch_or_bail(&mut self.buf, p, rel);
                                    }
                                    None => self.branch_patches.push((p, block)),
                                }
                            }
                        }
                    } else {
                        self.typecheck_branch_pending = Some((cond, one, zero, one_is_true));
                    }
                }
            }
            _ => {}
        }
    }

    /// The block an `Op::If` at `term` always transfers to when its condition
    /// `cond_id` is an `Int` constant, or `None` (not a constant, the switch
    /// off, or a projection the schedule placed nowhere -- the ordinary
    /// two-edge lowering then runs, exactly as before).
    ///
    /// Round 12 wave 4 (lane iropt3, proposal W3-2,
    /// `r12w3-iropt2-constant-if-is-still-a-branch`). The two-edge arms load the
    /// condition into `RAX`, `TEST EAX, EAX`, and take `Proj(0)` (the true
    /// edge) on nonzero; this answers the same edge from the constant's low 32
    /// bits, which is what that `TEST` reads. Resolved from the GRAPH, like
    /// `switch_edge_blocks`, rather than from a position in the de-duplicated
    /// `successors` list; the answer must still be one of this block's
    /// successors, or it is refused.
    ///
    /// Why dropping the other edge is sound: it is never taken at run time, so
    /// nothing the block behind it assumes (its phi copies, its register state
    /// on entry, its deopt and safepoint maps) can be observed through this
    /// edge. That block is still emitted; if nothing else reaches it, it is
    /// dead code. The back-edge poll is decided before this, from the
    /// schedule's successor list, so a folded loop exit keeps its poll.
    pub(in crate::ir_lower) fn constant_if_live_block(
        &self,
        term: NodeId,
        cond_id: NodeId,
        block_idx: usize,
    ) -> Option<usize> {
        let cond = self.graph.nodes.get(cond_id as usize)?;
        let which: u16 = match cond.op {
            // JIT round 19 wave 5 (lane handler5, SPLICE4-1): a compiled
            // handler arm's flag is `0` wherever it executes; only its probe
            // enters the arm, by a jump. Folded whatever the switch below says.
            Op::CatchEntryFlag { .. } => 1,
            // JIT round 25 (RB25-1, `r25-constant-and-redundant-branches`): the
            // opaque OSR merge's flag is `0` wherever it executes too; the entry
            // stub jumps past the `If` into the arm (`emit_osr_entry_stubs`
            // targets the arm block's label, which is still emitted). Opt-in.
            Op::OsrEntryFlag { .. } if true => 1,
            Op::Const(v) => {
                if cond.ty != IrType::Int {
                    return None;
                }
                // Cast: the low 32 bits, exactly the `TEST EAX, EAX` the arm emits.
                if (v as i32) != 0 {
                    0
                } else {
                    1
                }
            }
            _ => return None,
        };
        let proj = self.graph.nodes.iter().position(|n| {
            matches!(n.op, Op::Proj(w) if w == which) && n.inputs.first().copied() == Some(term)
        })?;
        let live = *self.schedule.node_to_block.get(proj)?;
        self.schedule
            .blocks
            .get(block_idx)?
            .successors
            .contains(&live)
            .then_some(live)
    }

    /// The block of `Proj(which)` of the `If` `term`, when it is one of
    /// `block_idx`'s successors (round nio21 wave 3, lane L2). Resolved from
    /// the graph, as [`Self::constant_if_live_block`] does.
    pub(in crate::ir_lower) fn if_projection_block(&self, term: NodeId, which: u16, block_idx: usize) -> Option<usize> {
        let proj = self.graph.nodes.iter().position(|n| {
            matches!(n.op, Op::Proj(w) if w == which) && n.inputs.first().copied() == Some(term)
        })?;
        let block = *self.schedule.node_to_block.get(proj)?;
        self.schedule
            .blocks
            .get(block_idx)?
            .successors
            .contains(&block)
            .then_some(block)
    }

    /// The false-edge block of `term` when it is the `If` of an ARMED
    /// call-site exception edge (`cond_id` an `ir::Op::CallThrew` whose call
    /// [`Self::plan_call_throw_edges`] armed), else `None` -- and then the
    /// general `If` lowering runs, reading the flag's zero. Round nio21 wave
    /// 3, lane L2.
    pub(in crate::ir_lower) fn armed_call_throw_normal_block(
        &self,
        term: NodeId,
        cond_id: NodeId,
        block_idx: usize,
    ) -> Option<usize> {
        if self.call_throw_plans.is_empty() && self.implicit_throw_plans.is_empty() {
            return None;
        }
        let cond = self.graph.nodes.get(cond_id as usize)?;
        if !matches!(cond.op, Op::CallThrew { .. }) {
            return None;
        }
        let call = *cond.inputs.get(1)?;
        // nio21 wave 5 L2: or the trapping node of an armed implicit site.
        if !self.call_throw_plans.contains_key(&call)
            && !self.implicit_throw_plans.contains_key(&call)
        {
            return None;
        }
        self.if_projection_block(term, 1, block_idx)
    }

    /// Arm the call-site exception edges of this graph (round nio21 wave 3,
    /// lane L2; `ir::Op::CallThrew`), before the first block is lowered.
    ///
    /// A site is armed -- its call's exceptional rel32 goes to a pad that asks
    /// `jit_local_handler_lookup` and JUMPS into the true edge of the
    /// candidate it answers -- only when the jump lands in exactly the state
    /// the graph's true edge describes. That is checked here, on the schedule
    /// and the frame plan, never assumed ([`Self::call_throw_targets`]):
    ///
    /// * the flags are `index` 0..n of one site with n candidates, all on the
    ///   same `Op::Call`, which is not a direct self-call (its route pushes
    ///   its exit rows itself);
    /// * flag 0 is in its call's block, whose terminator is its `If`, and
    ///   each later flag's block is the false edge of the flag before it.
    ///   Everything else the pad jumps over -- the rest of the call's own
    ///   lowering (the result store, its `.done` joins) and every node
    ///   scheduled after the call in those blocks -- is a trap-free, call-free
    ///   value that DEPENDS ON THE CALL, and no node on a path through a true
    ///   edge reads one: nothing a handler can need is skipped;
    /// * each true edge's block has that `If` as its one predecessor, starts
    ///   at its `Proj(0)`, and holds only trap-free, call-free values before
    ///   the edge's `ir::Op::CaughtException` (if DCE kept it): no edge copy
    ///   and no safepoint between the helper's store of the throwable and its
    ///   read;
    /// * no split-residency plan, no level-2 machine list: every value keeps
    ///   one location for its whole range, so the registers and homes at the
    ///   call's return ARE the true edge's entry state (the values live into a
    ///   handler are all defined before the call, in callee-saved registers
    ///   or homes the callee and the helper preserve);
    /// * the helper is wired and the frame has its context word.
    ///
    /// A site that fails any of these keeps today's exit; its `If` reads the
    /// flag's zero and the true-edge block is dead code.
    ///
    /// nio21 wave 4 (lane L2): except a site inside a SPLICED callee's covered
    /// `try` range (`ir::Graph::splice_handler_covered`), whose old exit
    /// leaves through the caller's `invoke` and would skip the callee's
    /// `catch`: any such node left unarmed refuses the compile, naming the
    /// splice so the IR door rebuilds without it
    /// ([`Self::refuse_unarmed_splice_handler_exits`]).
    pub(in crate::ir_lower) fn plan_call_throw_edges(&mut self) {
        self.plan_call_throw_edges_armed();
        self.refuse_unarmed_splice_handler_exits();
    }

    /// nio21 wave 4 (lane L2): every node stamped inside a spliced callee's
    /// covered `try` range that can leave the frame must be an ARMED call
    /// (its pad asks the callee's rows) or a guard (it deopts into the
    /// callee's own frame through the chain). Anything else -- an unarmed
    /// call, an `athrow`, an access or allocation that raises through an exit
    /// of its own -- refuses the lowering and names the splice
    /// (`note_sync_window_refused_pc`, the RV3-1 rebuild route). Nodes the
    /// schedule placed nowhere never run and are skipped.
    pub(in crate::ir_lower) fn refuse_unarmed_splice_handler_exits(&mut self) {
        let graph = self.graph;
        if graph.splice_handler_covered.is_empty() {
            return;
        }
        let mut offender: Option<(usize, NodeId)> = None;
        for (id, n) in graph.nodes.iter().enumerate() {
            let Some(pc) = n.bytecode_pc else {
                continue;
            };
            if !graph
                .splice_handler_covered
                .iter()
                .any(|&(start, end)| start <= pc && pc < end)
            {
                continue;
            }
            if self
                .schedule
                .node_to_block
                .get(id)
                .is_none_or(|&b| b == usize::MAX)
            {
                continue;
            }
            // Cast: node ids index `graph.nodes`, which the builder bounds.
            let id = id as NodeId;
            let ok = match &n.op {
                Op::Call { .. } => self.call_throw_plans.contains_key(&id),
                Op::Guard { .. } | Op::Dead | Op::InstanceOf { .. } => true,
                Op::Unbox {
                    op: crate::ir::UnboxOp::IntValue | crate::ir::UnboxOp::LongValue,
                    ..
                } if crate::ir::ir_splice_handler_lower_proven_enabled() => true,
                // j27 wave 7 (lane T1L1, `CRATONVM_JIT_IR_SPLICE_HANDLER_LOWER_PROVEN`):
                // a field access or allocation the builder proved delivers
                // nothing a covering handler catches.
                Op::Load(_)
                | Op::Store(_)
                | Op::LoadStatic { .. }
                | Op::New { .. }
                | Op::NewArray { .. }
                | Op::ArrayLoad(_)
                | Op::ArrayLength
                | Op::ArrayStore(_)
            | Op::ByteArrayCopy
                | Op::Throw
                    if crate::ir::splice_handler_exit_proven(graph, id) =>
                {
                    true
                }
                op => !crate::ir::may_raise(op),
            };
            if !ok {
                offender = Some((pc, id));
                break;
            }
        }
        let Some((pc, id)) = offender else {
            return;
        };
        note_sync_window_refused_pc(pc);
        if crate::x64::dbg_jitc_enabled() {
            // j27 wave 7 (lane T1L1): the op too, so a census can tell an
            // unarmed call from a proven-but-unrecorded access.
            let op = graph.nodes.get(id as usize).map(|n| format!("{:?}", n.op));
            eprintln!(
                "[cratonvm-jitc] ir-splice-handler REFUSED combined pc={pc} node={id} op={}: an exit \
                 inside a spliced callee's try range is not an armed call-site edge",
                op.as_deref().unwrap_or("?")
            );
        }
        self.latch_bailout(Bailout::with_context(
            BailoutReason::UnsupportedShape(
                "ir_lower: an exit inside a spliced callee's try range is not an armed call-site edge",
            ),
            format!("combined pc {pc}, node {id}"),
        ));
    }

    pub(in crate::ir_lower) fn plan_call_throw_edges_armed(&mut self) {
        let graph = self.graph;
        if graph.call_throw_sites.is_empty() {
            return;
        }
        // No switch read here: the builder builds a flag only when `lib.rs`
        // handed it the catch-type names, which it does only under
        // `CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES`; a graph that has one
        // was built to be armed.
        if self.local_handler_lookup == 0
            || self.context_slot_off == 0
            || self.phi_copy_scratch_slot_off <= 0
            || self.mir_mode != MirMode::Off
            || !self.ls_block_start.is_empty()
            || !self.ls_edge_copies.is_empty()
        {
            return;
        }
        let mut by_site: std::collections::BTreeMap<u32, Vec<(u16, NodeId)>> =
            std::collections::BTreeMap::new();
        for (id, n) in graph.nodes.iter().enumerate() {
            if let Op::CallThrew { site, index } = n.op {
                // Cast: node ids index `graph.nodes`, which the builder bounds.
                by_site.entry(site).or_default().push((index, id as NodeId));
            }
        }
        for (site, mut flags) in by_site {
            flags.sort_unstable();
            let Some(desc) = graph.call_throw_sites.get(site as usize) else {
                continue;
            };
            if desc.candidates.is_empty()
                || desc.candidates.len() != flags.len()
                || flags
                    .iter()
                    .enumerate()
                    .any(|(k, &(index, _))| usize::from(index) != k)
            {
                continue;
            }
            // nio21 wave 5 L2: a site whose flags hang on a trapping node
            // rather than a call is an implicit site, armed (or not) here.
            if self.plan_implicit_throw_site(site, desc, &flags) {
                continue;
            }
            let Some((call, targets)) = self.call_throw_targets(&flags) else {
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir-call-throw site {site} bci={} NOT armed: \
                         the schedule does not place the edge as the pad needs",
                        desc.bci
                    );
                }
                continue;
            };
            if self.call_throw_plans.contains_key(&call) {
                continue;
            }
            let boxed = Box::new(crate::JitLocalHandlerSite {
                candidates: desc.candidates.clone(),
                declaring_class_id: desc.declaring_class_id,
                // nio21 wave 4 L2: a spliced site must be answered
                // (`JitLocalHandlerSite::SPLICED_SCOPE_BIT`).
                throw_bci: if desc.spliced {
                    desc.bci | crate::JitLocalHandlerSite::SPLICED_SCOPE_BIT
                } else {
                    desc.bci
                },
                cache: std::sync::atomic::AtomicU64::new(crate::JitLocalHandlerSite::CACHE_EMPTY),
                implicit_cache: crate::JitLocalHandlerSite::empty_implicit_cache(),
                // nio21 call-site edge probe: a single-pass-style lookup that may
                // collect, as before the round-19 `no_collect` field existed.
                no_collect: false,
            });
            let site_ptr = boxed.as_ref() as *const crate::JitLocalHandlerSite as u64;
            self.call_throw_sites.push(boxed);
            self.call_throw_entry_blocks.extend(targets.iter().copied());
            self.call_throw_plans.insert(
                call,
                CallThrowPlan {
                    site_ptr,
                    targets,
                    call,
                    spliced: desc.spliced,
                },
            );
        }
    }

    /// The call and the true-edge blocks of one site's flags (sorted by
    /// `index`), or `None` when the schedule does not have the shape
    /// [`Self::plan_call_throw_edges`] arms. Round nio21 wave 3, lane L2.
    pub(in crate::ir_lower) fn call_throw_targets(&self, flags: &[(u16, NodeId)]) -> Option<(NodeId, Vec<usize>)> {
        let graph = self.graph;
        let first = graph.nodes.get(flags.first()?.1 as usize)?;
        let call = *first.inputs.get(1)?;
        let call_node = graph.nodes.get(call as usize)?;
        if !matches!(call_node.op, Op::Call { .. }) || Self::is_direct_self_call(call_node) {
            return None;
        }
        self.throw_edge_targets(call, flags)
            .map(|targets| (call, targets))
    }

    /// Arm one IMPLICIT exception site (round nio21 wave 5, lane L2;
    /// `ir::ir_implicit_exception_edges_enabled`): a site of
    /// [`crate::ir::Graph::call_throw_sites`] whose flags hang on a trapping
    /// node -- an `ArrayLoad` / `ArrayStore` (null and bounds checks) or an
    /// integer division's zero-divisor `Op::Guard` -- instead of a call
    /// (`ir::IrBuilder::plant_implicit_throw_edges`). Returns `true` for such
    /// a site, armed or not, and `false` for anything else, which the
    /// call-site route then decides.
    ///
    /// Armed only when, besides the call-site route's own conditions
    /// (checked by the caller):
    ///
    /// * the node is the method's own instruction at the site's bci;
    /// * the three signal setters and `jit_set_throw_bci` are wired;
    /// * the bci is protected in this compile, so the pad's miss exit is the
    ///   reason-9 exceptional frame ([`Self::push_call_exc_patch`]) with the
    ///   frame AT the instruction, never the params-only sentinel exit;
    /// * the schedule places the flags as a call's ([`Self::throw_edge_targets`]);
    /// * an `aaload`'s result has a planned home the pad can null.
    ///
    /// What is armed here is the GRAPH half (each `If` is a plain transfer to
    /// its false edge). The guard emitter then decides, at the site, whether
    /// it can emit the pad ([`Self::implicit_pad_frame_ok`]); when it cannot,
    /// the check deopts exactly as before and the true-edge blocks are dead.
    pub(in crate::ir_lower) fn plan_implicit_throw_site(
        &mut self,
        site: u32,
        desc: &crate::ir::IrCallThrowSite,
        flags: &[(u16, NodeId)],
    ) -> bool {
        let graph = self.graph;
        let Some(anchor) = flags
            .first()
            .and_then(|&(_, f)| graph.nodes.get(f as usize))
            .and_then(|n| n.inputs.get(1).copied())
        else {
            return false;
        };
        let Some(anode) = graph.nodes.get(anchor as usize) else {
            return false;
        };
        let bci = desc.bci as usize;
        let implicit = match anode.op {
            Op::ArrayLoad(_) | Op::ArrayStore(_) => true,
            // nio21 wave 6 (L2, proposal L2W5-1): a `getfield` / `putfield`
            // receiver null check (the `Op::Load` arm's leading test and
            // `emit_store_receiver_null_check_for`).
            Op::Load(_) | Op::Store(_) => true,
            Op::Guard { bci: g } => g == bci,
            _ => false,
        };
        if !implicit {
            return false;
        }
        let refuse = |why: &str| {
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-implicit-throw site {site} bci={bci} NOT armed: {why}"
                );
            }
            true
        };
        if desc.spliced || anode.bytecode_pc != Some(bci) {
            return refuse("not the method's own instruction at the site");
        }
        if self.throw_aioobe == 0
            || self.throw_arithmetic == 0
            || self.npe_with_action == 0
            || self.set_throw_bci == 0
        {
            return refuse("a signal helper is not wired");
        }
        // JIT round 20 (lane throw): an unprotected bci's miss is the shared
        // sentinel exit (`ir::ir_implicit_edges_unprotected_miss_enabled`).
        if !self.bci_is_protected(bci) && !crate::ir::ir_implicit_edges_unprotected_miss_enabled() {
            return refuse("the bci is not protected in this compile (no reason-9 miss exit)");
        }
        if self.implicit_throw_plans.contains_key(&anchor) {
            return refuse("the trapping node already carries a site");
        }
        // The pad's helper calls clobber the caller-saved XMM file, which the
        // residency plan treats as preserved across a non-call node.
        if self.reg_of.iter().any(Option::is_some) {
            return refuse("the body keeps a value in an XMM register");
        }
        let Some(targets) = self.throw_edge_targets(anchor, flags) else {
            return refuse("the schedule does not place the edge as the pad needs");
        };
        // The trapping node dominates its handler entries (the flags sit
        // after it in its block), so a dominance-based proof that READS it --
        // `ir_check_elim`'s "a dominating dereference would have left the
        // method" and "a dominating check of the same pair passed" -- would
        // hold on the path this edge adds, where the check FAILED. Since
        // nio21 wave 6 (L2) that pass fences every fact created at or after
        // an `If(CallThrew)` anchor to the blocks the chain's normal
        // continuation dominates (`ir_check_elim::exception_edge_fences`), so
        // no handler-side elision rests on this node and the site arms.
        // `CRATONVM_JIT_IR_IMPLICIT_EDGE_FENCED_HANDLERS=0` restores wave 5's
        // blunt refusal (any elided check in code a true-edge block
        // dominates), for an A/B only: the fence, not this, is what makes
        // the edge sound, including for a handler two sites share (a merge
        // no single true-edge block dominates, which that refusal missed).
        if !crate::ir::ir_implicit_edge_fenced_handlers_enabled() {
            let schedule = self.schedule;
            let handler_elides = schedule.blocks.iter().enumerate().any(|(d, blk)| {
                targets.iter().any(|&t| schedule.dom.dominates(t, d))
                    && blk.nodes.iter().any(|&x| {
                        self.check_elision.null_elided(x) || self.check_elision.bounds_elided(x)
                    })
            });
            if handler_elides {
                return refuse(
                    "a check in the handler's code was elided by a proof the trap breaks",
                );
            }
        }
        let result_home = if anode.ty == IrType::Ref {
            match self.planned_slot_off(anchor) {
                Ok(off) => Some(off),
                Err(_) => return refuse("the loaded reference has no planned home"),
            }
        } else {
            None
        };
        let boxed = Box::new(crate::JitLocalHandlerSite {
            candidates: desc.candidates.clone(),
            declaring_class_id: desc.declaring_class_id,
            throw_bci: desc.bci,
            cache: std::sync::atomic::AtomicU64::new(crate::JitLocalHandlerSite::CACHE_EMPTY),
            implicit_cache: crate::JitLocalHandlerSite::empty_implicit_cache(),
            // nio21 call-site edge probe: a single-pass-style lookup that may
            // collect, as before the round-19 `no_collect` field existed.
            no_collect: false,
        });
        let site_ptr = boxed.as_ref() as *const crate::JitLocalHandlerSite as u64;
        self.call_throw_sites.push(boxed);
        self.call_throw_entry_blocks.extend(targets.iter().copied());
        self.implicit_throw_plans.insert(
            anchor,
            ImplicitThrowPlan {
                site_ptr,
                targets,
                result_home,
                candidates: desc.candidates.clone(),
            },
        );
        true
    }

    /// Can the implicit pad of the node being lowered leave through its miss
    /// exit at `bci` (nio21 wave 5 L2)? The reason-9 frame
    /// [`Self::plan_rethrow_pad`] would build must be publishable, or that
    /// call latches a refusal of the whole compile -- a method whose guard
    /// deopted fine would be lost to an edge. Asked at the site, where
    /// [`Self::resolve_frame_state_for_site`] sees the node's own snapshot.
    pub(in crate::ir_lower) fn implicit_pad_frame_ok(&self, bci: usize) -> bool {
        if !self.bci_is_protected(bci) {
            // JIT round 20 (lane throw): no reason-9 frame to publish; the
            // miss takes the shared sentinel exit (`push_call_exc_patch_route`,
            // unprotected arm), as this compile's unprotected calls do.
            return crate::ir::ir_implicit_edges_unprotected_miss_enabled();
        }
        let mut fs = self.resolve_frame_state_for_site(bci);
        fs.stack.clear();
        let osr_virtuals =
            self.graph.osr_compile && crate::deopt::osr_exception_exit_virtuals_enabled();
        rethrow_frame_refusal(&fs, osr_virtuals).is_none()
    }

    /// The JEP 358 action code of an array access's null check (nio21 wave 5
    /// L2), the fallback message `jit_npe_message` uses when it cannot rebuild
    /// the full one from the trap site's bytecode.
    pub(in crate::ir_lower) fn implicit_npe_action(op: &Op) -> u8 {
        use cratonvm_jit_api::npe_action as a;
        match op {
            Op::ArrayLoad(kind) => match kind {
                MemKind::Int => a::ALOAD_INT,
                MemKind::Long => a::ALOAD_LONG,
                MemKind::Float => a::ALOAD_FLOAT,
                MemKind::Double => a::ALOAD_DOUBLE,
                MemKind::Ref => a::ALOAD_OBJECT,
                MemKind::Byte => a::ALOAD_BYTE,
                MemKind::Char => a::ALOAD_CHAR,
                MemKind::Short => a::ALOAD_SHORT,
            },
            // `getfield` / `putfield` (nio21 wave 6 L2): no fixed action code
            // names them; `jit_npe_message` rebuilds the message from the
            // bytecode at the exact trap site the pad records.
            Op::Load(_) | Op::Store(_) => a::NONE,
            Op::ArrayStore(kind) => match kind {
                MemKind::Int => a::ASTORE_INT,
                MemKind::Long => a::ASTORE_LONG,
                MemKind::Float => a::ASTORE_FLOAT,
                MemKind::Double => a::ASTORE_DOUBLE,
                MemKind::Ref => a::ASTORE_OBJECT,
                MemKind::Byte => a::ASTORE_BYTE,
                MemKind::Char => a::ASTORE_CHAR,
                MemKind::Short => a::ASTORE_SHORT,
            },
            _ => a::NONE,
        }
    }

    /// `MOV r64, imm64` that touches none of the lowering's register
    /// bookkeeping (nio21 wave 5 L2): an implicit pad runs on a cold path that
    /// never falls back into the node it interrupted, so a write to RCX there
    /// must not kill a deferred carry the hot path still relies on
    /// ([`Self::note_rcx_written`]).
    pub(in crate::ir_lower) fn emit_pad_mov_imm64(&mut self, reg: u8, val: u64) {
        let rex = 0x48 | if reg >= 8 { 0x01 } else { 0 };
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0xB8 + (reg & 7));
        self.buf.emit(&val.to_le_bytes());
    }

    /// The failing branch of an ARMED implicit site's check (nio21 wave 5
    /// L2), with the pad INLINE because its safepoint map must be recorded
    /// while the lowering state is this node's. The flags of
    /// the check must be set by the caller; RAX / ECX hold what `kind` reads
    /// (the array and the index for an AIOOBE). `bci` is the node's own.
    ///
    /// nio21 wave 6 (lane L2, proposal L2W5-3): the branch is inverted --
    /// `J<!jcc_fail> over ; pad: ... ; over:` -- so the
    /// passing path is ONE taken `Jcc` rather than an untaken `Jcc` plus a
    /// taken `JMP`: the `JMP` goes, the pad is still inline (its map is
    /// recorded with this node's lowering state) and is entered by falling
    /// into it. `jcc_fail ^ 1` is the x86 negation of any `Jcc` condition.
    pub(in crate::ir_lower) fn emit_implicit_throw_branch(
        &mut self,
        jcc_fail: u8,
        kind: ImplicitKind,
        bci: usize,
        anchor: NodeId,
        plan: &ImplicitThrowPlan,
    ) {
        let over = self.emit_jcc_rel32(jcc_fail ^ 1);
        self.emit_implicit_throw_pad(kind, bci, anchor, plan);
        self.patch_rel32_to_here(over);
    }

    /// One implicit pad (nio21 wave 5 L2), entered from a failing check of an
    /// armed site; it never falls through. In order:
    ///
    /// 1. (AIOOBE) the index, sign-extended, and the length are saved on the
    ///    machine stack across step 2, which clobbers RCX/R10/R11;
    /// 2. the safepoint map for `bci` ([`Self::emit_safepoint_map`]): the
    ///    frame word names this bci, so the frame snapshot the setter takes
    ///    places this frame at the trapping instruction (the stack trace and
    ///    the JEP 358 message both read it), and the lookup's allocation and
    ///    class loading find every live reference published;
    /// 3. the signal setter the single-pass stub calls for the same trap:
    ///    `jit_npe_with_action(trap key << 8 | action)`,
    ///    `jit_throw_aioobe(index, length, 0, bci)` or `jit_set_throw_bci(bci)`
    ///    + `jit_throw_arithmetic()`;
    /// 4. `jit_local_handler_lookup(vm, site, &scratch)`, whose implicit arm
    ///    builds the throwable through `materialize_implicit_signal` on a
    ///    match and leaves every signal as it found it on a miss; then the
    ///    shadow reload (RAX survives it);
    /// 5. on `-1`: `MOV RAX, i64::MIN` and the site's exceptional exit
    ///    ([`Self::push_call_exc_patch`]: the reason-9 frame at `bci`, which
    ///    [`Self::implicit_pad_frame_ok`] proved publishable), the signal
    ///    still pending -- the interpreter's drain builds and routes it there;
    /// 6. on `k`: null an `aaload`'s home and `JMP` into candidate `k`'s
    ///    true-edge block, whose `CaughtException` reads the scratch word.
    ///
    /// Registers: the GP residency file is callee-saved, so the helpers keep
    /// it; the XMM file is not, and the register model does not know a
    /// non-call node clobbers it, so a site is never armed in a body with any
    /// XMM-resident value ([`Self::plan_implicit_throw_site`]).
    /// Returns the target block if an implicit trap at `bci` is statically
    /// caught by a forward handler in `plan` whose `Op::CaughtException` is
    /// dead (no live uses in the graph).
    fn direct_implicit_handler_target(
        &self,
        kind: ImplicitKind,
        bci: usize,
        plan: &ImplicitThrowPlan,
    ) -> Option<usize> {
        let (k, &(_, handler_bci)) = plan
            .candidates
            .iter()
            .enumerate()
            .take(plan.targets.len())
            .find(|(_, (catch, _))| implicit_kind_matches_catch(kind, catch))?;
        if (handler_bci as usize) <= bci {
            return None;
        }
        let target_block = plan.targets[k];
        let tblock = self.schedule.blocks.get(target_block)?;
        let has_live_caught_exc = tblock.nodes.iter().any(|&x| {
            matches!(
                self.graph.nodes.get(x as usize).map(|n| &n.op),
                Some(Op::CaughtException)
            ) && self
                .graph
                .nodes
                .iter()
                .any(|u| u.op != Op::Dead && u.inputs.contains(&x))
        });
        (!has_live_caught_exc).then_some(target_block)
    }

    pub(in crate::ir_lower) fn emit_implicit_throw_pad(
        &mut self,
        kind: ImplicitKind,
        bci: usize,
        anchor: NodeId,
        plan: &ImplicitThrowPlan,
    ) {
        // An `aaload`'s home is allocated (and so named by every map from here
        // on) before its guards run, and holds whatever the word last held:
        // null it BEFORE this pad's own map publishes it. A store touches no
        // register and no flag. The actual home when the arm has allocated
        // one, else the planned one.
        let ref_home = plan.result_home.map(|planned| {
            self.node_slot
                .get(anchor as usize)
                .copied()
                .flatten()
                .map_or(planned, |off| off.get() as i32)
        });
        if let Some(target_block) = self.direct_implicit_handler_target(kind, bci, plan) {
            let mut jvmti_misses = [None, None];
            if let Some([exc_flag, catch_flag]) = self.graph.exception_event_guard {
                for (slot, addr) in jvmti_misses.iter_mut().zip([exc_flag, catch_flag]) {
                    self.emit_pad_mov_imm64(R11, addr as u64);
                    self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP byte [R11], 0
                    *slot = Some(self.emit_jcc_rel32(0x85)); // JNZ .full_pad
                }
            }
            if let Some(off) = ref_home {
                // MOV qword [rbp - off], 0
                self.buf.emit(&[0x48, 0xC7, 0x85]);
                self.buf.emit(&(-off).to_le_bytes());
                self.buf.emit(&0i32.to_le_bytes());
            }
            let p = self.emit_jmp_rel32();
            self.branch_patches.push((p, target_block));
            self.implicit_throw_pads_emitted += 1;
            if self.graph.exception_event_guard.is_none() {
                return;
            }
            for patch in jvmti_misses.into_iter().flatten() {
                self.patch_rel32_to_here(patch);
            }
        }
        if let Some(off) = ref_home {
            // MOV qword [rbp - off], 0
            self.buf.emit(&[0x48, 0xC7, 0x85]);
            self.buf.emit(&(-off).to_le_bytes());
            self.buf.emit(&0i32.to_le_bytes());
        }
        if kind == ImplicitKind::Aioobe {
            // MOVSXD RCX, ECX: the index as the signed `int` the message names.
            self.buf.emit(&[0x48, 0x63, 0xC9]);
            // MOV EAX, [RAX + length]: the length (RAX is the array; the
            // bounds check just read the same word). The shared build-time
            // checked disp8, so the layout inventory counts no new site.
            self.buf.emit(&[0x8B, 0x40, OSR_ARRAY_LENGTH_DISP8 as u8]);
            self.buf.emit(&[0x51, 0x50]); // PUSH RCX ; PUSH RAX
        }
        let sp_live_hi = self.spill_high_water;
        let mut map = self.emit_safepoint_map(sp_live_hi);
        // JIT round 20 wave 18 (lane xThrow, THROW20-6): the lookup below
        // always follows the setter, with this frame and its map at `bci`
        // still standing, so the setter may leave the compiled-frame snapshot
        // to it, as the single-pass pads do (`ir_defer_trap_snapshot_bit`).
        let defer = ir_defer_trap_snapshot_bit();
        match kind {
            ImplicitKind::Npe => {
                let action = self
                    .graph
                    .nodes
                    .get(anchor as usize)
                    .map_or(cratonvm_jit_api::npe_action::NONE, |n| {
                        Self::implicit_npe_action(&n.op)
                    });
                let key = self.record_npe_trap_site(bci);
                let code = (u64::from(key) << 8) | u64::from(action);
                // JIT round 20 (lane throw): stamp the bci first, as the
                // single-pass pads and this pad's arithmetic arm do, so a miss
                // through the shared sentinel exit (an unprotected site) is
                // routed through this method's table at the trapping
                // instruction rather than by the pc-unknown search.
                self.emit_pad_mov_imm64(CALL_ARG_REGS[0], self.throw_stamp_word(bci as u64 | defer));
                self.emit_pad_mov_imm64(RAX, self.set_throw_bci as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                self.emit_pad_mov_imm64(CALL_ARG_REGS[0], code);
                self.emit_pad_mov_imm64(RAX, self.npe_with_action as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            }
            ImplicitKind::Aioobe => {
                // POP length ; POP index, straight into their argument
                // registers (REX.B for R8..R15).
                for reg in [CALL_ARG_REGS[1], CALL_ARG_REGS[0]] {
                    if reg >= 8 {
                        self.buf.emit_byte(0x41);
                    }
                    self.buf.emit_byte(0x58 + (reg & 7));
                }
                self.emit_pad_mov_imm64(CALL_ARG_REGS[2], 0);
                self.emit_pad_mov_imm64(CALL_ARG_REGS[3], self.throw_stamp_word(bci as u64 | defer));
                self.emit_pad_mov_imm64(RAX, self.throw_aioobe as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            }
            ImplicitKind::Arithmetic => {
                self.emit_pad_mov_imm64(CALL_ARG_REGS[0], self.throw_stamp_word(bci as u64 | defer));
                self.emit_pad_mov_imm64(RAX, self.set_throw_bci as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                self.emit_pad_mov_imm64(RAX, self.throw_arithmetic as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            }
        }
        // In the stamped arms, the last call is the signal helper. The
        // earlier TLS BCI stamp does not inspect the frame or collect.
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.emit_post_call_frame_record();
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_pad_mov_imm64(CALL_ARG_REGS[1], plan.site_ptr);
        self.lea_reg_from_frame(CALL_ARG_REGS[2], self.phi_copy_scratch_slot_off);
        self.emit_pad_mov_imm64(RAX, self.local_handler_lookup as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.emit_post_call_frame_record();
        self.emit_shadow_reload();
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        let miss = self.emit_jcc_rel32(0x88); // JS .miss
        if let Some(off) = ref_home {
            // MOV qword [rbp - off], 0: the load that never ran. Nulled before
            // the map above too; a moving collection during the helpers left
            // it null (the shadow reload copies back what it published).
            self.buf.emit(&[0x48, 0xC7, 0x85]);
            self.buf.emit(&(-off).to_le_bytes());
            self.buf.emit(&0i32.to_le_bytes());
        }
        let last = plan.targets.len().saturating_sub(1);
        for (k, &block) in plan.targets.iter().enumerate() {
            if k < last {
                // CMP RAX, imm8 k ; JE block. Cast: k < IR_CALL_THROW_MAX_CANDIDATES.
                self.buf.emit(&[0x48, 0x83, 0xF8, k as u8]);
                let p = self.emit_jcc_rel32(0x84);
                self.branch_patches.push((p, block));
            } else {
                let p = self.emit_jmp_rel32();
                self.branch_patches.push((p, block));
            }
        }
        // .miss: the exit the site would have without the edge's handler.
        self.patch_rel32_to_here(miss);
        self.emit_pad_mov_imm64(RAX, i64::MIN as u64);
        let exit = self.emit_jmp_rel32();
        self.push_call_exc_patch(exit);
        self.implicit_throw_pads_emitted += 1;
    }

    /// The true-edge blocks of one site's flags (sorted by `index`) whose
    /// ordering input is `call` -- an `Op::Call` (wave 3) or, since nio21
    /// wave 5 (lane L2), an implicit site's trapping node, whose failing
    /// check jumps over the rest of the block exactly as a call's exceptional
    /// return does: the node's own result is never produced, and everything
    /// scheduled after it in its block must depend on it. `None` when the
    /// schedule does not have the shape [`Self::plan_call_throw_edges`] arms.
    pub(in crate::ir_lower) fn throw_edge_targets(&self, call: NodeId, flags: &[(u16, NodeId)]) -> Option<Vec<usize>> {
        let graph = self.graph;
        let schedule = self.schedule;
        let call_block = *schedule.node_to_block.get(call as usize)?;
        let nblocks = schedule.blocks.len();
        // The values the pad skips: everything scheduled after the call in its
        // block and in the false-edge blocks the chain runs through. Each must
        // depend on the call (`skipped`), so no path into a handler can need
        // it: the true edges carry the locals from before the call.
        let mut skipped: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        skipped.insert(call);
        let mut targets = Vec::with_capacity(flags.len());
        let mut prev_if: Option<NodeId> = None;
        for &(_, flag) in flags {
            let fnode = graph.nodes.get(flag as usize)?;
            if fnode.inputs.len() != 2 || fnode.inputs[1] != call {
                return None;
            }
            let b = *schedule.node_to_block.get(flag as usize)?;
            let block = schedule.blocks.get(b)?;
            let iff = block.terminator?;
            let inode = graph.nodes.get(iff as usize)?;
            if inode.op != Op::If || inode.inputs.get(1).copied() != Some(flag) {
                return None;
            }
            let after = match prev_if {
                None => {
                    if b != call_block {
                        return None;
                    }
                    let call_pos = block.nodes.iter().position(|&x| x == call)?;
                    &block.nodes[call_pos + 1..]
                }
                Some(prev) => {
                    let ctrl = graph.nodes.get(*fnode.inputs.first()? as usize)?;
                    if ctrl.op != Op::Proj(1)
                        || ctrl.inputs.first().copied() != Some(prev)
                        || block.predecessors.len() != 1
                    {
                        return None;
                    }
                    &block.nodes[..]
                }
            };
            if !after.contains(&flag) {
                return None;
            }
            for &x in after {
                if x == flag {
                    continue;
                }
                let n = graph.nodes.get(x as usize)?;
                // A skipped `Ref` would leave a home a later map names
                // unwritten; only the call's own result is nulled by the pad.
                //
                // nio21 wave 5 L2: the one non-trap-free node admitted is the
                // integer division a zero-divisor guard licenses (its guard
                // token names the anchor): with that guard present its
                // lowering never traps (`emit_div_zero_guard` materialises a
                // placeholder), and the pad jumps over it like any value.
                let licensed_division = matches!(n.op, Op::Div | Op::Rem)
                    && n.inputs.contains(&call)
                    && matches!(
                        graph.nodes.get(call as usize).map(|c| &c.op),
                        Some(Op::Guard { .. })
                    );
                if n.ty == IrType::Ref
                    || !(op_cannot_deopt(&n.op) || licensed_division)
                    || !n.inputs.iter().any(|i| skipped.contains(i))
                {
                    return None;
                }
                skipped.insert(x);
            }
            skipped.insert(flag);
            let t = self.if_projection_block(iff, 0, b)?;
            // nio21 wave 4 L2 (L2W3-3): the false edge must resolve too, so an
            // armed `If` is always the plain transfer
            // (`armed_call_throw_normal_block`) and never reads its flag,
            // whose store the flag's lowering then drops.
            self.if_projection_block(iff, 1, b)?;
            let tblock = schedule.blocks.get(t)?;
            let head = graph.nodes.get(tblock.ctrl as usize)?;
            if head.op != Op::Proj(0)
                || head.inputs.first().copied() != Some(iff)
                || tblock.predecessors.len() != 1
                || tblock.predecessors[0] != b
            {
                return None;
            }
            // Before the caught throwable is read out of the scratch word:
            // nothing that calls, traps or reaches a safepoint (no edge copy
            // runs inside a block). With no reader (DCE took an unused
            // throwable) the word is simply never read.
            let caught_at = tblock.nodes.iter().position(|&x| {
                matches!(
                    graph.nodes.get(x as usize).map(|n| &n.op),
                    Some(Op::CaughtException)
                )
            });
            if let Some(at) = caught_at {
                for &x in &tblock.nodes[..at] {
                    if !op_cannot_deopt(&graph.nodes.get(x as usize)?.op) {
                        return None;
                    }
                }
            }
            targets.push(t);
            prev_if = Some(iff);
        }
        // And no skipped value is read on a path through a true edge (the
        // builder never makes one; an optimizer pass would have to invent it).
        // A φ reads its value on the edge from the predecessor its slot names
        // (`[region, v_per_pred…]`, slot `i` <-> the region's control input
        // `i - 1`), so a φ after the `try` that merges the call's result from
        // the normal path with the handler's value is fine. A MEMORY-token
        // read of the call (a handler's load or call ordered after it, a
        // memory φ) is not a read of anything the pad skips: the callee's
        // effects happened whether it returned or threw.
        let through_a_true_edge =
            |blk: usize| targets.iter().any(|&t| schedule.dom.dominates(t, blk));
        for (uid, u) in graph.nodes.iter().enumerate() {
            if u.op == Op::Dead {
                continue;
            }
            let reads: Vec<usize> = u
                .inputs
                .iter()
                .enumerate()
                .filter(|&(slot, v)| {
                    skipped.contains(v) && !crate::ir::is_memory_token_slot(u, slot)
                })
                .map(|(slot, _)| slot)
                .collect();
            if reads.is_empty() {
                continue;
            }
            let Some(&ub) = schedule.node_to_block.get(uid) else {
                continue;
            };
            if ub >= nblocks {
                continue;
            }
            if u.op != Op::Phi {
                if through_a_true_edge(ub) {
                    return None;
                }
                continue;
            }
            let region = graph.nodes.get(*u.inputs.first()? as usize)?;
            for slot in reads {
                if slot == 0 {
                    return None;
                }
                let ctrl = *region.inputs.get(slot - 1)?;
                let pred = *schedule.node_to_block.get(ctrl as usize)?;
                if pred >= nblocks || through_a_true_edge(pred) {
                    return None;
                }
            }
        }
        Some(targets)
    }

    /// Emit every call-site exception pad (round nio21 wave 3, lane L2;
    /// `ir::Op::CallThrew`), right after the body and before every reader of
    /// `call_exc_patches` / `rethrow_pads`, whose rows the pads' fallbacks
    /// add. Each pad, entered from its call's exceptional rel32 with RAX ==
    /// `i64::MIN` (after the J/D/F `dispatch_threw` peek and the callee-deopt
    /// service, exactly where today's exit is taken):
    ///
    /// 1. republishes this frame's identity and, when the site's shadow reload
    ///    had not run yet, copies the published words back WITHOUT retracting
    ///    `top` -- the cold prelude of `emit_callee_deopt_service_cold`, for
    ///    the same reason: the helper can load a class, i.e. run Java and
    ///    collect, and the frame word still names this call's map;
    /// 2. `CALL jit_local_handler_lookup(vm, site, &scratch)`, and the
    ///    identity again (the helper may have run compiled code);
    /// 3. on `-1` (no candidate takes it, a deopt or an IR-deopt frame is
    ///    pending, nothing is pending, a JVMTI exception client is armed):
    ///    `MOV RAX, i64::MIN` and `JMP` to the exit the site had before
    ///    (`CallThrowFallback`), with every signal as the helper found it;
    /// 4. on `k >= 0` (the throwable is taken, stored in the scratch word):
    ///    copies the shadow words back again and retracts `top`, which is the
    ///    site's skipped reload, nulls a `Ref` result's home (the skipped
    ///    result store), then `JMP`s into the block of candidate `k`'s true
    ///    edge, whose `CaughtException` reads the scratch word.
    ///
    /// The `i64::MIN` ambiguity costs the hot path nothing: every compare that
    /// tells "exception" from "deopt" from "a real `Long.MIN_VALUE`" runs on
    /// this cold side or before it.
    pub(in crate::ir_lower) fn emit_call_throw_pads(&mut self) {
        for mut pad in std::mem::take(&mut self.call_throw_pads) {
            self.patch_rel32_to_here(pad.entry_patch);
            self.emit_post_call_frame_record();
            if let Some((thread_slot_off, savebase_slot_off, offsets)) = &pad.shadow {
                crate::runtime_lowering::emit_shadow_copy_back(
                    &mut self.buf,
                    &crate::runtime_lowering::ShadowCopyBack {
                        thread_slot_off: *thread_slot_off,
                        savebase_slot_off: *savebase_slot_off,
                        offsets,
                    },
                );
            }
            self.load_context(CALL_ARG_REGS[0]);
            self.emit_mov_reg_imm64(CALL_ARG_REGS[1], pad.site_ptr);
            self.lea_reg_from_frame(CALL_ARG_REGS[2], self.phi_copy_scratch_slot_off);
            self.emit_mov_reg_imm64(RAX, self.local_handler_lookup as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            self.anchor_safepoint_return(pad.map.as_mut(), self.buf.pos());
            self.emit_post_call_frame_record();
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            let miss = self.emit_jcc_rel32(0x88); // JS .miss
            if let Some((thread_slot_off, savebase_slot_off, offsets)) = &pad.shadow {
                crate::runtime_lowering::emit_shadow_copy_back(
                    &mut self.buf,
                    &crate::runtime_lowering::ShadowCopyBack {
                        thread_slot_off: *thread_slot_off,
                        savebase_slot_off: *savebase_slot_off,
                        offsets,
                    },
                );
                self.emit_shadow_retract_to_savebase(*thread_slot_off, *savebase_slot_off);
            }
            if let Some(off) = pad.result_home {
                // MOV qword [rbp - off], 0: the skipped result store's word.
                self.buf.emit(&[0x48, 0xC7, 0x85]);
                self.buf.emit(&(-off).to_le_bytes());
                self.buf.emit(&0i32.to_le_bytes());
            }
            let last = pad.targets.len().saturating_sub(1);
            for (k, &block) in pad.targets.iter().enumerate() {
                if k < last {
                    // CMP RAX, imm8 k ; JE block. Cast: k < IR_CALL_THROW_MAX_CANDIDATES.
                    self.buf.emit(&[0x48, 0x83, 0xF8, k as u8]);
                    let p = self.emit_jcc_rel32(0x84);
                    self.branch_patches.push((p, block));
                } else {
                    // The helper answers an index of the site's own candidate
                    // list, so the last one needs no compare.
                    let p = self.emit_jmp_rel32();
                    self.branch_patches.push((p, block));
                }
            }
            // .miss: today's exit, with the sentinel the exit returns.
            self.patch_rel32_to_here(miss);
            self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
            let exit = self.emit_jmp_rel32();
            match pad.fallback {
                CallThrowFallback::Plain { bci } => self.call_exc_patches.push((exit, bci)),
                CallThrowFallback::Rethrow(mut rethrow) => {
                    rethrow.patch = exit;
                    self.rethrow_pads.push(rethrow);
                }
            }
            self.call_throw_pads_emitted += 1;
        }
    }

    /// Retract the shadow `top` to the base the site's push saved -- the tail
    /// of [`Self::emit_shadow_reload`], byte for byte, for a call-site
    /// exception pad that skips the site's own reload (round nio21 wave 3,
    /// lane L2). Clobbers R10/R11 only.
    pub(in crate::ir_lower) fn emit_shadow_retract_to_savebase(&mut self, thread_slot_off: i32, savebase_slot_off: i32) {
        let ss_top = self.shadow_off_in_thread;
        // MOV R10, [rbp - thread_slot] ; TEST R10, R10 ; JE skip
        self.buf.emit(&[0x4C, 0x8B, 0x95]);
        self.buf.emit(&(-thread_slot_off).to_le_bytes());
        self.buf.emit(&[0x4D, 0x85, 0xD2]);
        let skip = self.emit_jcc_rel32(0x84);
        // MOV R11, [rbp - savebase] ; AND R11, ~1 ; MOV [R10 + ss_top], R11
        self.buf.emit(&[0x4C, 0x8B, 0x9D]);
        self.buf.emit(&(-savebase_slot_off).to_le_bytes());
        self.buf.emit(&[0x49, 0x83, 0xE3, 0xFE]);
        self.buf.emit(&[0x4D, 0x89, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        self.patch_rel32_to_here(skip);
    }

    /// Transfer control to `target`, as a fall-through when the block layout
    /// already placed it physically next and as a `JMP rel32` otherwise.
    ///
    /// `lower_inner` emits blocks with `for block_idx in 0..blocks.len()`, so
    /// "physically next" is exactly `block_idx + 1` — the same test the fused-
    /// branch arm has always used for its near edge, lifted here so the two
    /// cannot drift apart. It is only sound at a site that emits NOTHING after
    /// this transfer, which is why the general two-successor arm may use it for
    /// its second (trailing) edge and not for its first.
    ///
    /// This is the half of the frequency-driven block layout that was missing.
    /// `ir_schedule`'s module header states it plainly: making the hot
    /// successor physically next is "necessary but not yet sufficient" while
    /// every edge still ends in an explicit `JMP`, because the taken-branch
    /// count and the byte count are then unchanged by any reordering. Measured
    /// before this, a counted loop's body carried five jumps — one of them the
    /// five-byte encoding `E9 00 00 00 00`, a jump to the very next
    /// instruction.
    ///
    /// Counted rather than asserted: `ir_fallthroughs_elided` against
    /// `ir_block_jmps_emitted` is what says whether a layout change moved
    /// anything, and a pass whose only evidence is "the code got smaller" is a
    /// pass nobody can tune.
    pub(in crate::ir_lower) fn emit_jmp_to_block_or_fall_through(&mut self, target: usize, block_idx: usize) {
        if self.fallthrough_on && target == block_idx + 1 && target < self.schedule.blocks.len() {
            FALLTHROUGHS_ELIDED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        BLOCK_JMPS_EMITTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.buf.emit_byte(0xE9); // JMP target
        let patch_pos = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.branch_patches.push((patch_pos, target));
    }

    // ── `Op::Switch` ─────────────────────────────────────────────────────

    /// The successor block of every edge of the `Op::Switch` at `term`, in
    /// PROJECTION order: entries `0 ..= high - low` are the cases for keys
    /// `low ..= high`, and the last entry is the default edge.
    ///
    /// `None` refuses the compile, and every refusal here is a graph that
    /// `ir_verify`'s projection lane should already have rejected: a missing
    /// edge, an edge whose `Proj` the scheduler placed in no block, a bound
    /// `ir::switch_is_dense` does not admit. Refusing costs the method its
    /// optimizing compile; guessing an edge costs a jump to the wrong arm.
    ///
    /// Resolved from the GRAPH rather than from `blocks[..].successors`: that
    /// list is de-duplicated, so an index into it would silently shift for
    /// every later case if two edges ever did name one block. Here a
    /// projection index is the index.
    pub(in crate::ir_lower) fn switch_edge_blocks(&self, term: NodeId, low: i32, high: i32) -> Option<Vec<usize>> {
        if high < low {
            return None;
        }
        let outputs = crate::ir::switch_projection_count(low, high);
        if outputs < 2 || outputs > crate::ir::SWITCH_MAX_PROJECTIONS {
            return None;
        }
        let mut edges = vec![usize::MAX; outputs];
        for (id, n) in self.graph.nodes.iter().enumerate() {
            let Op::Proj(which) = n.op else { continue };
            if n.inputs.first().copied() != Some(term) {
                continue;
            }
            let which = which as usize;
            // An index naming no output of this producer is a violation
            // `check_projections` reports by name; all this scan owes is not
            // to index an array with it. Likewise a duplicate: the FIRST one
            // wins, deterministically, and the verifier says so.
            if which >= outputs || edges[which] != usize::MAX {
                continue;
            }
            edges[which] = self
                .schedule
                .node_to_block
                .get(id)
                .copied()
                .unwrap_or(usize::MAX);
        }
        let nblocks = self.schedule.blocks.len();
        // `usize::MAX` — an edge no `Proj` filled in — fails this too.
        if edges.iter().any(|&b| b >= nblocks) {
            return None;
        }
        Some(edges)
    }

    /// The block a switch edge ultimately transfers to, for the purpose of
    /// deciding whether two edges are the SAME edge.
    ///
    /// A switch edge's own block is the `Op::Proj` that opens it. In the graph
    /// `IrBuilder` produces, that block is empty and its single successor is
    /// the merge the JVMS target pc names — so two case slots that name one
    /// target pc have two distinct `Proj` blocks and one shared successor, and
    /// it is the successor that has to be compared.
    ///
    /// When the block is not that shape (the scheduler is free to place data
    /// nodes into it, and a later pass is free to give it a terminator) the
    /// edge is its own destination. That answer is conservative in the safe
    /// direction: more distinct destinations means more live keys, more live
    /// keys means a jump table, and the table is total over `[low, high]`.
    pub(in crate::ir_lower) fn switch_edge_destination(&self, b: usize) -> usize {
        match self.schedule.blocks.get(b) {
            Some(blk)
                if blk.nodes.is_empty()
                    && blk.terminator.is_none()
                    && blk.successors.len() == 1 =>
            {
                blk.successors.first().copied().unwrap_or(b)
            }
            _ => b,
        }
    }

    /// The phi copies an edge owes, as a comparable signature.
    pub(in crate::ir_lower) fn switch_edge_copy_signature(
        &self,
        pred: usize,
        succ: usize,
    ) -> Vec<(NodeId, i32, NodeId, i32)> {
        if pred == succ {
            return Vec::new();
        }
        self.gather_phi_copies(pred, succ)
            .into_iter()
            .map(|c| (c.phi, c.dst, c.src, c.src_slot))
            .collect()
    }

    /// The `(key, block)` pairs a compare chain or a binary search has to
    /// test, in ascending key order: the case slots that do something the
    /// default edge would not already do.
    ///
    /// # Why a slot may be dropped, and what has to be true for it
    ///
    /// The dense form gives every slot in `[low, high]` a projection, and a
    /// `tableswitch` javac padded — `case 1: case 2: case 3: case 7:` compiles
    /// to a seven-slot table whose slots 4, 5 and 6 carry the default offset —
    /// spends most of them on the default target. Letting such a key reach the
    /// default edge instead of its own is only sound when the two edges are
    /// INTERCHANGEABLE, which is two separate facts:
    ///
    /// * they land on the same block ([`Self::switch_edge_destination`]), and
    /// * they perform the same phi copies on the way. The builder gives every
    ///   edge of one switch the same abstract-state snapshot, so this holds by
    ///   construction on a freshly built graph — but "holds by construction"
    ///   is what a pass that rewrites one predecessor's phi input is entitled
    ///   to break, and a dropped key would then take the wrong values into the
    ///   merge. Compared, not assumed.
    ///
    /// A slot that fails either test stays live, which costs a compare and is
    /// never wrong.
    pub(in crate::ir_lower) fn switch_live_cases(
        &self,
        low: i32,
        case_blocks: &[usize],
        default_block: usize,
    ) -> Vec<(i32, usize)> {
        let default_dest = self.switch_edge_destination(default_block);
        let default_copies = self.switch_edge_copy_signature(default_block, default_dest);
        let mut live = Vec::with_capacity(case_blocks.len());
        for (k, &b) in case_blocks.iter().enumerate() {
            let dest = self.switch_edge_destination(b);
            if dest == default_dest && self.switch_edge_copy_signature(b, dest) == default_copies {
                continue;
            }
            // `low + k` cannot overflow `i32`: `k < high - low + 1`, and that
            // span was bounded in `i64` by `ir::switch_is_dense` before this
            // node was ever built.
            live.push((low.wrapping_add(k as i32), b));
        }
        live
    }

    /// `SUB`/`CMP`/`JAE` bounds check and an indirect `JMP` through a table of
    /// code offsets — the O(1) shape, independent of the case count.
    ///
    /// ```text
    ///     SUB  EAX, low              ; omitted when low == 0
    ///     CMP  EAX, span             ; span = high - low + 1
    ///     JAE  default               ; UNSIGNED: catches both ends
    ///     LEA  RCX, [RIP + table]
    ///     MOVSXD RAX, [RCX + RAX*4]  ; table[index], a base-relative offset
    ///     ADD  RAX, RCX              ; back to an absolute code address
    ///     JMP  RAX
    /// ```
    ///
    /// # Why an out-of-range key cannot reach the indirect `JMP`
    ///
    /// This is indirect control flow, so "the index is in range" is not a
    /// property to hope for — it is the one the emitted bytes have to
    /// establish, and they establish it twice over:
    ///
    /// 1. **One unsigned compare covers both ends.** `SUB EAX, low` maps the
    ///    key onto `key - low`, so a key BELOW `low` becomes negative — and
    ///    read as unsigned, every negative `i32` is at least `2^31`, which is
    ///    far above any `span` this node can carry (`ir::SWITCH_MAX_SPAN` is
    ///    255). `JAE` (unsigned above-or-equal) therefore takes the default
    ///    edge for `key < low` and for `key > high` alike, and the surviving
    ///    fall-through has `0 <= index < span` with no second compare.
    /// 2. **The scaled index cannot address outside the table.** `SUB EAX`,
    ///    like every 32-bit x86-64 destination write, zero-extends into RAX,
    ///    so bits 63:32 are 0 on the fall-through path and `[RCX + RAX*4]`
    ///    reads `table_base + 4 * index` with `index < span`. The table is
    ///    emitted with exactly `span` entries ([`Self::emit_switch_tables`]),
    ///    so that is the last entry's address at worst.
    ///
    /// The table itself holds each target's offset from the TABLE BASE, not an
    /// absolute address: the buffer is copied into executable memory at an
    /// address the lowerer does not know, so an absolute entry would need a
    /// load-time relocation. `ADD RAX, RCX` is what turns the stored
    /// displacement back into an address, and it is the same convention the
    /// single-pass backend's `jump_table_patches` uses.
    pub(in crate::ir_lower) fn emit_switch_jump_table(&mut self, low: i32, case_blocks: &[usize], default_block: usize) {
        let Ok(span) = i32::try_from(case_blocks.len()) else {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: an Op::Switch span does not fit a CMP immediate",
            )));
            return;
        };
        if low != 0 {
            self.buf.emit(&[0x2D]); // SUB EAX, imm32
            self.buf.emit(&low.to_le_bytes());
        } else {
            // Round 9: the table read below scales all 64 bits of RAX while
            // the bounds check validates EAX. The `SUB EAX` above
            // zero-extends as a side effect; with `low == 0` nothing did, and
            // a key whose high half disagrees with its low half (the bug
            // class `emit_array_null_bounds_guards_for` closes with
            // `MOV ECX, ECX`) would pass the check and index far outside the
            // table. Two bytes, flags untouched.
            self.buf.emit(&[0x89, 0xC0]); // MOV EAX, EAX
        }
        self.buf.emit(&[0x3D]); // CMP EAX, imm32
        self.buf.emit(&span.to_le_bytes());
        self.buf.emit(&[0x0F, 0x83]); // JAE rel32 → the default edge
        let bounds_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.branch_patches.push((bounds_patch, default_block));

        self.buf.emit(&[0x48, 0x8D, 0x0D]); // LEA RCX, [RIP + disp32]
        let lea_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.buf.emit(&[0x48, 0x63, 0x04, 0x81]); // MOVSXD RAX, [RCX + RAX*4]
        self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX
        self.buf.emit(&[0xFF, 0xE0]); // JMP RAX

        self.switch_tables.push(SwitchTablePlan {
            lea_patch,
            targets: case_blocks.to_vec(),
        });
    }

    /// A balanced binary search over the live keys: `O(log n)` SIGNED compares
    /// and no table bytes at all.
    ///
    /// Signed (`JL`, not `JB`) because a JVMS switch key is an `int` and the
    /// keys are sorted as `int`s; an unsigned split would put every negative
    /// key above every positive one and send half of them to the default.
    /// Nothing is subtracted from the key first, which is what makes a
    /// `lookupswitch` over negative keys need no special case here.
    ///
    /// Every path out of this emission — including the one through an empty
    /// half — ends in an UNCONDITIONAL transfer, so nothing falls through into
    /// the code the caller emits next.
    pub(in crate::ir_lower) fn emit_switch_binary_search(&mut self, live: &[(i32, usize)], default_block: usize) {
        let mid = live.len() / 2;
        let Some(&(key, target)) = live.get(mid) else {
            // No key left on this path: everything here is the default.
            self.buf.emit_byte(0xE9); // JMP rel32 → the default edge
            let patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.branch_patches.push((patch, default_block));
            return;
        };
        self.buf.emit(&[0x3D]); // CMP EAX, imm32
        self.buf.emit(&key.to_le_bytes());
        self.buf.emit(&[0x0F, 0x84]); // JE rel32 → this case
        let eq_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.branch_patches.push((eq_patch, target));
        self.buf.emit(&[0x0F, 0x8C]); // JL rel32 → the lower half
        let lt_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // The UPPER half falls through — it is the half reached with no taken
        // branch, and the two halves differ by at most one key, so neither
        // ordering is better; falling through the upper one keeps the lower
        // half's `JL` a forward branch, which is the statically predicted
        // not-taken direction.
        self.emit_switch_binary_search(&live[mid + 1..], default_block);
        let lower = self.buf.pos();
        let rel = lower as i32 - (lt_patch as i32 + 4);
        // codegen -- tolerated on an overflowed buffer; see `patch_or_bail`.
        if !Self::patch_or_bail(&mut self.buf, lt_patch, rel) {
            return;
        }
        self.emit_switch_binary_search(&live[..mid], default_block);
    }

    /// Lay out the jump tables every `Op::Switch` site asked for, AFTER the
    /// last block. See [`SwitchTablePlan`] for why they are not inline.
    ///
    /// Called from `lower_inner_with_array_list` beside the other tail stubs and
    /// BEFORE `patch_branches`, because the entries it reserves are resolved
    /// there and the `LEA` it patches here is not a branch.
    pub(in crate::ir_lower) fn emit_switch_tables(&mut self) {
        for plan in std::mem::take(&mut self.switch_tables) {
            // An overflowed buffer drops every later write (the flag is
            // sticky), so `pos()` is frozen and the padding loop below would
            // never reach alignment. It spun forever on the compiling thread
            // whenever an overflow froze `pos()` at an unaligned offset: a
            // Spring test in Mockito's ByteBuddy self-attach hung in
            // `emit_switch_tables` for the watchdog's whole minute. The driver
            // discards this body anyway, so there is nothing to lay out.
            if self.buf.overflowed() {
                return;
            }
            // Every entry is an `i32` read with a scaled index, so a base that
            // is not 4-aligned costs a split load on every dispatch. Padded
            // with `INT3` rather than `NOP`: nothing executes these bytes, and
            // trapping is the right thing to do if anything ever does.
            //
            // `!overflowed()` (round 9 wave 2): an overflowed buffer drops the
            // write and does not advance `pos()`, so this loop never ended on
            // a method with a switch whose body had already outgrown its
            // buffer — a hung compiler thread instead of a refused (now:
            // retried) compile.
            while self.buf.pos() % 4 != 0 && !self.buf.overflowed() {
                self.buf.emit_byte(0xCC);
            }
            let table_base = self.buf.pos();
            let rel = table_base as i32 - (plan.lea_patch as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, plan.lea_patch, rel) {
                return;
            }
            for target in plan.targets {
                let entry = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                self.switch_table_patches.push((entry, table_base, target));
            }
        }
    }

    pub(in crate::ir_lower) fn patch_branches(&mut self) {
        for &(patch_pos, target_block) in &self.branch_patches {
            let target_offset = self.block_offsets[target_block];
            let rel32 = target_offset as i32 - (patch_pos as i32 + 4);
            // codegen -- tolerated on an overflowed buffer; see
            // `Self::patch_or_bail` / `patch_rel32_to_here`.
            Self::patch_or_bail(&mut self.buf, patch_pos, rel32);
        }
        // Jump-table entries. The stored value is the target's offset from its
        // TABLE BASE — not from the entry, and not absolute — because the
        // dispatch adds the base back (`ADD RAX, RCX`). Same convention as the
        // single-pass backend's `jump_table_patches`.
        for &(entry, table_base, target_block) in &self.switch_table_patches {
            let target_offset = self.block_offsets[target_block];
            let rel32 = target_offset as i32 - table_base as i32;
            Self::patch_or_bail(&mut self.buf, entry, rel32);
        }
    }

    // ── Deopt frame-state resolution (real-frame-deopt step 2) ───────────

    /// Resolve the machine location of a single IR value into a `FrameValue`.
    ///
    /// This naive single-scratch lowerer spills every node result to a frame
    /// slot (`node_slot`), so a live SSA value is found either as a constant
    /// (encoded directly) or at a frame spill slot — never in a register.
    /// That is exactly the cleanest first cut for deopt: every non-constant
    /// resolves to a `StackSlot`, and the VM reads it from the native stack.
    ///
    /// Convention: `StackSlot(off)` is read by the VM as `*(rbp + off)`
    /// (matching `deopt.rs`). The lowerer stores results at `[rbp - slot]`
    /// with `slot > 0`, so we encode the *negative* offset here.
    pub(in crate::ir_lower) fn frame_value_for(&self, node_id: NodeId) -> FrameValue {
        if node_id == NO_NODE {
            return FrameValue::Undefined;
        }
        // Checked, not indexed. A snapshot slot naming an id past the end of the
        // arena means the snapshot and the graph being lowered disagree — the
        // one situation where panicking is worst, because it takes the VM down
        // from the compiler thread. Answer with the honest "this slot's value is
        // gone and I cannot describe it", which is unresumable by construction
        // (`frame_state_is_resumable`) rather than a fabricated zero.
        let Some(node) = self.graph.nodes.get(node_id as usize) else {
            return FrameValue::MaterializationRequired(EliminatedValue::unknown(
                EliminationCause::Unclassified,
            ));
        };
        // An `lcmp` fused into its branch was never computed anywhere -- no
        // register, no home word (`fused_lcmp`). The fusion only happens when
        // no deopt that can actually happen names it; this is the check of
        // that prediction against the emission, with the same quieting as a
        // dropped home at a bci nothing transfers from.
        if self
            .fused_lcmp
            .get(node_id as usize)
            .copied()
            .unwrap_or(false)
            || self
                .fused_fcmp
                .get(node_id as usize)
                .copied()
                .unwrap_or(false)
            // JIT round 25 item 1a: a compare fused although a frame state
            // names it (`fused_cmp_unnamed`) has no location either; without
            // this it would fall through to `dominating_unemitted_slot` and be
            // described by a planned word nothing wrote.
            || self
                .fused_cmp_unnamed
                .get(node_id as usize)
                .copied()
                .unwrap_or(false)
            // JIT rx27 wave 4 (lane regalloc): an `And` answered by a `TEST`.
            || self
                .fused_test_and
                .get(node_id as usize)
                .copied()
                .unwrap_or(false)
        {
            if !self.quiet_dropped_home.get() {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: node_id },
                    format!("n{node_id} is an lcmp/fcmp fused into its branch and has no location"),
                ));
            }
            return FrameValue::Unsupported;
        }
        // A register beats a frame word, when the register is provably his.
        // Placed after the `Op::Dead` / out-of-range handling above and before
        // the constants below, because a CONSTANT needs no machine location at
        // all and describing one by register would be a strict loss.
        // A PARAMETER is named by every frame state from the method's entry
        // on, but it reaches its register only where the scheduler placed its
        // `Op::Param` node -- which can be after a deopt that names it. Until
        // that node has been emitted the register holds whatever the CALLER
        // left in it, so describe the parameter by its prologue word instead
        // (the `Op::Param` arm below), which the prologue always writes.
        //
        // Witness: H2 `TestBigResult.testSortingAndDistinct2`, whose IR body
        // traps at bci 2 (an `invokedynamic` string concat) with `partCount`
        // reserved a callee-saved register as a loop-carried value. Called
        // from a compiled caller, the precise resume rebuilt `partCount` from
        // that register -- the caller's value -- and `new BitSet(partCount)`
        // threw `NegativeArraySizeException: nbits < 0: -702434216`.
        let param_not_yet_published = matches!(node.op, Op::Param(_))
            && !self
                .defined_nodes
                .get(node_id as usize)
                .copied()
                .unwrap_or(false);
        if !matches!(node.op, Op::Const(_) | Op::ConstF(_)) && !param_not_yet_published {
            if let Some(v) = self.register_frame_value(node_id, node.ty) {
                self.deopt_reg_named.set(self.deopt_reg_named.get() + 1);
                return v;
            }
            // JIT round 25 item 1a (wave 2): a site-named resident value is its
            // register where the site proves it, and refuses everywhere else.
            if self
                .site_named
                .get(node_id as usize)
                .copied()
                .unwrap_or(false)
            {
                return self.site_named_frame_value(node_id, node.ty);
            }
            // A dropped home MUST have been describable by a register: that is
            // a clause of `phi_home_droppable`, and the two must not be able to
            // drift apart. If they ever do, refuse the compile rather than
            // describe the frame word nothing wrote — unless the point being
            // resolved is one no deopt can arrive at (`quiet_dropped_home`),
            // where the honest `Unsupported` is never read.
            if self
                .home_dropped
                .get(node_id as usize)
                .copied()
                .unwrap_or(false)
            {
                if !self.quiet_dropped_home.get() {
                    self.latch_bailout(Bailout::with_context(
                        BailoutReason::UnallocatedValue { node: node_id },
                        format!("n{node_id}'s home was dropped but no register describes it"),
                    ));
                }
                return FrameValue::Unsupported;
            }
        }
        match node.op {
            // Integer / long constants need no machine location. A cat-1 `int`
            // constant resolves to `Int`; a cat-2 `long` constant resolves to
            // `Long` (the resume builds a `Value::Long` with cat-2 two-slot
            // local placement — `real-frame-deopt` cat-2).
            Op::Const(v) => {
                if node.ty == IrType::Long {
                    FrameValue::Long(v)
                } else if node.ty == IrType::Ref && v == 0 && deopt_null_const_as_object() {
                    // JIT round 23 wave 5 (page
                    // `r23-w5-typecheck-in-place-trap-resumes-a-null-as-int`):
                    // the `Ref` null constant (`aconst_null`) is a reference.
                    // As `Int(0)` the resume rebuilt it as `Value::Int(0)`, and
                    // the interpreter's next reference use of the slot
                    // (`checkcast`, `invoke*`, `getfield`, `areturn`) failed
                    // with "not an object reference" -- for ANY deopt with a
                    // null constant live in a local or on the stack, not only
                    // the in-place type-check trap that exposed it.
                    FrameValue::Object(0)
                } else {
                    FrameValue::Int(v)
                }
            }
            // Float / double constant bits. A cat-2 `double` resolves to
            // `Double` (the resume builds a `Value::Double` with cat-2 two-slot
            // local placement); a cat-1 `float` resolves to `Float`.
            Op::ConstF(bits) => {
                if node.ty == IrType::Double {
                    FrameValue::Double(bits)
                } else {
                    FrameValue::Float(bits)
                }
            }
            Op::Param(idx) => {
                // The prologue stored param `idx` at `[rbp - (idx+1)*8]`. If
                // the Param node was also scheduled it has its own spill slot
                // holding the same value; prefer that, else the prologue slot.
                let off = match self.node_slot.get(node_id as usize).copied().flatten() {
                    Some(slot) => slot.get() as i32,
                    None => ((idx as i32) + 1) * 8,
                };
                Self::typed_stack_slot(-off, node.ty)
            }
            // JIT round 25 wave 3: a snapshot-ranged home is described only
            // where the slot plan made it readable.
            _ if !self.snapshot_ranged_home_ok(node_id) => {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: node_id },
                    format!(
                        "n{node_id}'s home is snapshot-ranged and this frame state reads it \
                         where the slot plan did not (bci {}); lowering again unranged",
                        self.cur_bci
                    ),
                ));
                FrameValue::Unsupported
            }
            _ => {
                match self.node_slot.get(node_id as usize).copied().flatten() {
                    Some(slot) => Self::typed_stack_slot(-(slot.get() as i32), node.ty),
                    // No machine location assigned. Two OPPOSITE situations wear
                    // the same shape here, and spelling them the same way is how
                    // a scalar-replaced reference local reconstructs as `null`
                    // with no error anywhere (every resume sink maps `Undefined`
                    // to `Value::Int(0)`):
                    //
                    //   * the producer is `Op::Dead` — some pass (escape
                    //     analysis' `apply_ea_to_ir`, DCE) DELETED the value the
                    //     interpreter will read. That is
                    //     `MaterializationRequired`: unresumable by construction,
                    //     so the deopt is refused and the method re-runs, instead
                    //     of resuming with a fabricated zero. The cause is
                    //     `Unclassified` because this generic slot resolver
                    //     cannot tell which pass retired the node —
                    //     `frame_value_for_object` knows, and says so.
                    //   * the producer is live but has no location HERE: not yet
                    //     emitted when this deopt is resolved (a value scheduled
                    //     after the site that names it), or never given a
                    //     home. This used to answer `Undefined`, which every
                    //     resume sink turns into `Value::Int(0)` / null — a
                    //     named local silently zeroed. Round 11 wave 2 (lane
                    //     lower): it is `Unsupported` (unresumable), and at a
                    //     bci a deopt can arrive at it refuses the compile, the
                    //     same rule as a dropped home above. `NO_NODE` (a local
                    //     no path wrote) keeps `Undefined`, at the top.
                    //
                    // Note this is a deopt *description*, not an emitted
                    // access: neither answer reads `[rbp - 0]`. That is why a
                    // missing location is tolerated here and refused in
                    // `slot_of`.
                    None if matches!(node.op, Op::Dead) => FrameValue::MaterializationRequired(
                        EliminatedValue::new(node_id, EliminationCause::Unclassified),
                    ),
                    // JIT round 20 wave 4 (lane loopopt; r21-loopreload-voidwriter):
                    // the value is not emitted YET in layout order, but its
                    // block strictly dominates the block of the node being
                    // lowered, so at run time it has executed -- and written
                    // its home, which is write-through and pinned for a value
                    // a frame names -- before control reaches this site. The
                    // layout simply emitted the site's block first. Describe
                    // it by the word the slot plan gives it.
                    None if self.dominating_unemitted_slot(node_id).is_some() => {
                        let off = self.dominating_unemitted_slot(node_id).unwrap_or(0);
                        Self::typed_stack_slot(-off, node.ty)
                    }
                    None => {
                        if !self.quiet_dropped_home.get() {
                            self.latch_bailout(Bailout::with_context(
                                BailoutReason::UnallocatedValue { node: node_id },
                                // JIT round 20 wave 3 (lane loopopt): say WHERE --
                                // the site's bci and node pc, whether the value
                                // was emitted yet, its scheduled block, and
                                // whether the slot plan gives it a word -- so a
                                // host trace separates "emitted after the site"
                                // (a layout / placement order bug) from "never
                                // given a home" (a slot-plan bug).
                                format!(
                                    "n{node_id} ({:?}) is named by a frame state but has no \
                                     location at the deopt [site bci {} node pc {:?}; \
                                     site block {:?}; emitted {}; block {:?}; planned slot {}]",
                                    node.op,
                                    self.cur_bci,
                                    self.cur_node_pc,
                                    self.cur_def
                                        .and_then(|d| self.schedule.node_to_block.get(d as usize))
                                        .copied(),
                                    self.defined_nodes
                                        .get(node_id as usize)
                                        .copied()
                                        .unwrap_or(false),
                                    self.schedule.node_to_block.get(node_id as usize).copied(),
                                    self.planned_slot_off(node_id).is_ok()
                                ),
                            ));
                        }
                        FrameValue::Unsupported
                    }
                }
            }
        }
    }

    /// JIT round 20 wave 4 (lane loopopt): the planned home offset of
    /// `node_id`, when it has not been emitted yet but is scheduled in a block
    /// that STRICTLY dominates the block of the data node being lowered
    /// (`cur_def`), and its home is neither dropped nor unplanned. `None`
    /// otherwise -- outside a data node's lowering, in the same block (where a
    /// later position really is later), or when the block does not dominate.
    pub(in crate::ir_lower) fn dominating_unemitted_slot(&self, node_id: NodeId) -> Option<i32> {
        if self
            .defined_nodes
            .get(node_id as usize)
            .copied()
            .unwrap_or(true)
            || self
                .home_dropped
                .get(node_id as usize)
                .copied()
                .unwrap_or(false)
        {
            return None;
        }
        let site = self.cur_def?;
        let sb = *self.schedule.node_to_block.get(site as usize)?;
        let vb = *self.schedule.node_to_block.get(node_id as usize)?;
        let nb = self.schedule.blocks.len();
        if sb >= nb || vb >= nb || sb == vb {
            return None;
        }
        if !self.schedule.dom.is_reachable(sb) || !self.schedule.dom.dominates(vb, sb) {
            return None;
        }
        // JIT round 20 wave 7 (lane revIR; correctness, unconditional): a
        // REFERENCE home is a GC root only while some map names it. The
        // per-GC-point maps (`emit_safepoint_map`) name the `Ref` homes of
        // `defined_nodes`, i.e. EMISSION order, so every safepoint laid out
        // before this value's block -- the site's own call, and any other GC
        // point of a block emitted earlier -- leaves its word out: a moving
        // collection there relocates the object and leaves this home stale,
        // and the deopt (or a later read of the home) hands the interpreter a
        // dangling pointer. Only the frame block (`plan_frame_block`, which
        // publishes EVERY reference colour once per activation) roots it
        // regardless of layout; without one, keep the old refusal for a `Ref`.
        if self
            .graph
            .nodes
            .get(node_id as usize)
            .is_some_and(|n| n.ty == IrType::Ref)
            && self.frame_block.is_empty()
        {
            return None;
        }
        self.planned_slot_off(node_id).ok()
    }

    /// Encode a spilled value at `off` (relative to `rbp`) as a typed
    /// `FrameValue` location, driven by the IR node's value type
    /// (`real-frame-deopt` type source). A `Ref` slot becomes `StackSlotRef`
    /// (resolves to a `Value::Object`); a cat-1 `Int` slot stays `StackSlot`
    /// (resolves to `Value::Int`); a cat-2 `Long` slot becomes `StackSlotLong`
    /// (resolves to a `Value::Long` with cat-2 two-slot placement). A cat-1
    /// `Float` slot becomes `StackSlotFloat` (resolves to a `Value::Float` from
    /// the spilled 32-bit bits); a cat-2 `Double` slot becomes `StackSlotDouble`
    /// (resolves to a `Value::Double` with cat-2 two-slot placement). With
    /// FP-slot resume wired (Slice C), an FP value may now be live at a deopt
    /// guard. `Void`/`Control`/`Memory` are never live data slots, so they map to
    /// `Unsupported`.
    pub(in crate::ir_lower) fn typed_stack_slot(off: i32, ty: IrType) -> FrameValue {
        match ty {
            IrType::Ref => FrameValue::StackSlotRef(off),
            IrType::Int => FrameValue::StackSlot(off),
            IrType::Long => FrameValue::StackSlotLong(off),
            IrType::Float => FrameValue::StackSlotFloat(off),
            IrType::Double => FrameValue::StackSlotDouble(off),
            _ => FrameValue::Unsupported,
        }
    }

    /// The refusal every [`Self::frame_value_for_object`] bail returns: "escape
    /// analysis deleted this object, and I could not describe how to rebuild it
    /// here".
    ///
    /// Carries the producer node and the allocated class so the compiler report
    /// can name *which* allocation at *which* site the deopt path cannot undo —
    /// the difference between an actionable finding and an anonymous "cannot
    /// resume". `cause` distinguishes the plain
    /// [`EliminationCause::ScalarReplacedObject`] bails (no deopt block, a
    /// non-dominating allocation/store, an unresolvable field) from
    /// [`EliminationCause::NestedVirtualObject`] (the object is describable, but
    /// one of its fields is itself virtual and v1 emits no nested graphs).
    pub(in crate::ir_lower) fn eliminated_object(
        new_id: NodeId,
        info: &VirtualObjectInfo,
        cause: EliminationCause,
    ) -> FrameValue {
        FrameValue::MaterializationRequired(EliminatedValue::allocation(
            new_id,
            info.class_id,
            cause,
        ))
    }

    /// The bci to hand the INTERPRETER for a node whose `bytecode_pc` may be a
    /// combined-buffer pc inside a relocated callee body.
    ///
    /// Identity outside a spliced region, and identity on every compile that
    /// splices nothing. Inside one it answers the enclosing `invoke`'s bci,
    /// which is the only pc in this method that describes where execution is:
    /// the call has not taken effect, so re-executing it re-runs the callee.
    ///
    /// Both consumers need this for correctness, not tidiness. A throw-site bci
    /// is range-checked against this method's own exception table
    /// (`jit_set_throw_bci` → `route_jit_exception_through_method`), and a pc
    /// past the end of the bytecode falls outside every `[start_pc, end_pc)` —
    /// which silently drops a `catch`-all, the exact defect that helper exists
    /// to fix. A guard's resume bci is worse: the interpreter would be parked at
    /// an instruction that does not exist.
    pub(in crate::ir_lower) fn resume_bci(&self, bci: usize) -> usize {
        for &(start, end, invoke_bci) in self.spliced_ranges {
            if bci >= start && bci < end {
                return invoke_bci;
            }
        }
        bci
    }

    /// Is `pc` a combined-buffer pc inside a RELOCATED callee body?
    ///
    /// The same scan [`Self::resume_bci`] does, asked for its predicate rather
    /// than its answer, and kept beside it so the two cannot come to disagree
    /// about what "inside a splice" means. Read by [`ir_blind_dispatch_census`]
    /// and by [`Self::new_class_proven_initialized`], for which it is
    /// load-bearing.
    pub(in crate::ir_lower) fn pc_is_in_a_spliced_body(&self, pc: usize) -> bool {
        self.spliced_ranges
            .iter()
            .any(|&(start, end, _)| pc >= start && pc < end)
    }

    /// A class every non-null value of `value` is an instance of, when the
    /// graph ALONE proves one (no hierarchy query): `new C` is exactly a `C`,
    /// and an element loaded from a `new C[n]` array is null or a `C` (each
    /// store into that array was covariance-checked against exactly `C`).
    /// `None` otherwise. Read by the `aastore` arm to skip
    /// `jit_aastore_type_check` for a store into another `new C[n]`.
    pub(in crate::ir_lower) fn aastore_value_class_bound(&self, value: NodeId) -> Option<u32> {
        aastore_value_class_bound_in(self.graph, value)
    }

    /// May the `Op::New` of `class_id` at `pc` take the inline TLAB bump, which
    /// runs no `<clinit>`? Only when the class is PROVEN initialised at compile
    /// time (r11w2-tier-ir-inline-tlab-new-runs-no-clinit).
    ///
    /// Three facts prove it:
    ///
    /// * the VM's own answer, `DirectHelperTable::class_is_initialized`
    ///   (`jit_class_initialized`, the `class_init_memo` the static-base
    ///   resolver also reads). This is the direct bit; the two below remain
    ///   for compiles whose table carries no VM (tests, the no-VM table);
    /// * `tlab_announce_classes` membership. `lib.rs` inserts a class only for
    ///   a `JitNewSite::Resolved` with `!needs_post_alloc_init && !has_finalizer`, and
    ///   the VM's `jit_new_site_flags` answers `(true, true)` for every class
    ///   not yet initialised -- so a member was initialised when the site was
    ///   resolved, and initialisation is monotone.
    /// * a `new` inside a spliced callee body. The splice resolver
    ///   (`jit_bridge.rs`, `ir-splice-new-site-class-init-pending` /
    ///   `-not-initialized`) refuses a body whose `new` names a class that is
    ///   not initialised, unless (al16, `CRATONVM_JIT_IR_SPLICE_NEW_SETTLED_CLASS`)
    ///   it is SETTLED: no `<clinit>` owed anywhere in its chain and linked, so
    ///   that skipping its initialisation is unobservable -- the rule the
    ///   single-pass tier's resolved `new` rows already compile under.
    ///
    /// Anything else -- a class with a `long`/`float`/`double` field or a
    /// finalizer in the caller's own body, or every caller-body site under
    /// `CRATONVM_JIT_IR_ZGC_ANNOUNCE=0` (the set is then empty) -- keeps the
    /// `jit_new_object` stub, which initialises. That costs throughput, never
    /// correctness. Since round 11 wave 4 the first fact closes those cases
    /// for every class already initialised at compile time
    /// (r11w4-lower-class-initialized-predicate-patch-FIXED-20260924.md).
    pub(in crate::ir_lower) fn new_class_proven_initialized(&self, class_id: u32, pc: Option<usize>) -> bool {
        if self.tlab_announce_classes.contains(&class_id)
            || pc.is_some_and(|pc| self.pc_is_in_a_spliced_body(pc))
            || self.direct_helpers.class_is_initialized(class_id)
        {
            return true;
        }
        crate::runtime_lowering::note_inline_tlab_decline(
            "class not proven initialised at compile time",
        );
        false
    }

    /// JIT r20 wave 19 (LOOPOPT20-6): does [`Self::resolve_frame_state_for_site`]
    /// at `bci` answer from the node's own named snapshot (not the flat scan)?
    pub(in crate::ir_lower) fn cur_node_frame_is_at(&self, bci: usize) -> bool {
        self.cur_node_frame
            .and_then(|si| self.graph.safepoints.get(si as usize))
            .is_some_and(|sp| sp.bci == bci)
    }

    /// The frame state for a deopt emitted while lowering the CURRENT node,
    /// resuming at `bci`.
    ///
    /// [`Self::resolve_frame_state_for_bci`] with one thing added: when the
    /// node being lowered names its own snapshot
    /// ([`crate::ir::Node::frame_snapshot`]), that snapshot wins over the
    /// by-bci scan. After an unroll there are `trip` nodes at each body bci and
    /// the scan finds copy 0's snapshot for every one of them; this is what
    /// makes copy `k`'s guard resume into copy `k`'s values.
    ///
    /// # Fail closed on a bci disagreement
    ///
    /// The named snapshot is used only when its `bci` is the one being resumed
    /// at. It should always be — `Graph::set_node_frame_snapshot` refuses to
    /// install a snapshot whose bci differs from the node's — but the bci
    /// reaching here has been through [`Self::resume_bci`], which rewrites a
    /// combined-buffer pc inside a spliced body to the enclosing `invoke`. A
    /// node that was both spliced and cloned would arrive with the two
    /// disagreeing, and the honest answer there is the caller's `invoke` frame
    /// the splice contract promises, not a callee frame from some iteration.
    /// Falling back rather than asserting keeps that a missed optimization
    /// instead of a refused compile.
    pub(in crate::ir_lower) fn resolve_frame_state_for_site(&self, bci: usize) -> FrameState {
        if let Some(si) = self.cur_node_frame {
            if let Some(sp) = self.graph.safepoints.get(si as usize) {
                if sp.bci == bci {
                    return self.resolve_frame_state(si as usize, sp);
                }
            }
        }
        self.resolve_frame_state_for_bci(bci)
    }

    /// For each bci, the index of the first `graph.safepoints` entry at that
    /// bci. For every bci this is the answer
    /// `graph.safepoints.iter().position(|s| s.bci == bci)` gives; it is
    /// built once in [`Lowerer::new`] as [`Lowerer::safepoint_first_by_bci`].
    pub(in crate::ir_lower) fn first_safepoint_index_by_bci(graph: &Graph) -> HashMap<usize, usize> {
        let mut first: HashMap<usize, usize> = HashMap::with_capacity(graph.safepoints.len());
        for (idx, sp) in graph.safepoints.iter().enumerate() {
            // A chain snapshot is not a frame of this method (round 13 wave 3).
            if sp.is_splice_state() {
                continue;
            }
            first.entry(sp.bci).or_insert(idx);
        }
        first
    }

    /// [`Self::first_safepoint_index_by_bci`] for the CHAIN snapshots
    /// (`ir::SafepointSnapshot::is_splice_state`) alone: their first index per
    /// combined pc. Empty on every build without them.
    pub(in crate::ir_lower) fn first_splice_state_index_by_pc(graph: &Graph) -> HashMap<usize, usize> {
        let mut first: HashMap<usize, usize> = HashMap::new();
        for (idx, sp) in graph.safepoints.iter().enumerate() {
            // A post-call snapshot (wave 47) is no guard's chain.
            if sp.is_guard_splice_state() {
                first.entry(sp.bci).or_insert(idx);
            }
        }
        first
    }

    /// Interpreter round i1 wave 47, lane L3: for each spliced `Op::Call` with
    /// exactly one post-call chain snapshot ([`ir::SafepointSnapshot::post_call_of`]),
    /// that snapshot's index. A call two snapshots name (an unrolled copy's)
    /// is left out: it gets no chain post-call exit. Empty on every build
    /// without them.
    pub(in crate::ir_lower) fn post_call_splice_states_by_call(graph: &Graph) -> HashMap<NodeId, usize> {
        let mut out: HashMap<NodeId, usize> = HashMap::new();
        let mut twice: Vec<NodeId> = Vec::new();
        for (idx, sp) in graph.safepoints.iter().enumerate() {
            if sp.post_call_of == NO_NODE || !sp.is_splice_state() {
                continue;
            }
            if out.insert(sp.post_call_of, idx).is_some() {
                twice.push(sp.post_call_of);
            }
        }
        for call in twice {
            out.remove(&call);
        }
        out
    }

    /// Round 13 wave 3 (lane framestate, W5-3 stage 2): the chain snapshot a
    /// deopt at RAW combined pc `raw_pc` publishes, and whether it is
    /// REQUIRED rather than preferred.
    ///
    /// * The node being lowered names a chain snapshot at `raw_pc` (an
    ///   unrolled copy's per-iteration frame): that one, and it is required --
    ///   the flat frame at the outermost `invoke` is found by bci and describes
    ///   the ORIGINAL iteration, not this copy.
    /// * It names a flat frame (LICM's corrected pre-header frame, which
    ///   `ir_optimize::install_preheader_deopt_frame` binds to a hoisted
    ///   spliced node precisely so this path does not answer): `None`, the
    ///   named frame wins exactly as before.
    /// * Otherwise the first chain snapshot at `raw_pc`, not required: the flat
    ///   frame is the pre-round-13 answer and still correct for it.
    pub(in crate::ir_lower) fn splice_chain_snapshot_for_site(&self, raw_pc: usize) -> Option<(usize, bool)> {
        if self.splice_state_first_by_pc.is_empty() {
            return None;
        }
        if let Some(si) = self.cur_node_frame {
            let si = si as usize;
            return match self.graph.safepoints.get(si) {
                Some(sp) if sp.is_splice_state() && sp.bci == raw_pc => Some((si, true)),
                Some(_) => None,
                None => self
                    .splice_state_first_by_pc
                    .get(&raw_pc)
                    .map(|&first| (first, false)),
            };
        }
        self.splice_state_first_by_pc
            .get(&raw_pc)
            .map(|&first| (first, false))
    }

    /// The inlined deopt chain of chain snapshot `si`: the callee's frame at
    /// its own bci, with every caller parked mid-`invoke` as
    /// [`FrameState::caller`] (innermost first, the compiling method last),
    /// or why it cannot be published.
    ///
    /// Every slot is resolved with the dropped-home latch QUIETED: a value
    /// with no location here makes the CHAIN unpublishable (the caller falls
    /// back to the flat frame, which the latch then judges as it always did)
    /// rather than refusing the compile. The result is also held to what the
    /// VM can materialise ([`crate::deopt::inlined_chain_refusal`]): a chain
    /// the VM refused at run time would cost a whole-method re-run where the
    /// flat frame resumed precisely.
    pub(in crate::ir_lower) fn splice_chain_frame_state(&self, si: usize) -> Result<FrameState, &'static str> {
        let sp = self
            .graph
            .safepoints
            .get(si)
            .ok_or("no snapshot at that index")?;
        let chain = sp.splice_scope.as_deref().ok_or("not a chain snapshot")?;
        let outer_key = &self.graph.scope_method_key;
        if outer_key.is_empty() {
            return Err("the compiling method's key is unknown");
        }
        let was_quiet = self.quiet_dropped_home.replace(true);
        let (locals, stack, monitors) = self.resolve_chain_frame_values(si, sp);
        self.quiet_dropped_home.set(was_quiet);
        self.link_splice_chain(chain, &locals, &stack, monitors)
    }

    /// The linking half of [`Self::splice_chain_frame_state`]: one
    /// [`FrameState`] per scope of `chain` from the resolved `locals` /
    /// `stack` of its snapshot, innermost first, each caller as the
    /// `caller` of the scope it called, `monitors` on the outermost (every
    /// held monitor is the compiling method's). Held to
    /// [`crate::deopt::inlined_chain_refusal`]. Split out in interpreter
    /// round i2 wave 57 (lane L2) for the spliced-header poll exits
    /// ([`Self::spliced_back_edge_mode_exit_state`]), whose values are the
    /// back edge's rather than the snapshot's own.
    pub(in crate::ir_lower) fn link_splice_chain(
        &self,
        chain: &crate::ir::SpliceScopeChain,
        locals: &[FrameValue],
        stack: &[FrameValue],
        monitors: Vec<crate::deopt::MonitorInfo>,
    ) -> Result<FrameState, &'static str> {
        let outer_key = &self.graph.scope_method_key;
        if outer_key.is_empty() {
            return Err("the compiling method's key is unknown");
        }
        let per_scope = chain
            .split(locals, stack)
            .ok_or("the chain's layout does not match its snapshot")?;
        let n = chain.scopes.len();
        if n < 2 {
            return Err("a chain names no caller");
        }
        let mut monitors = Some(monitors);
        let mut linked: Option<FrameState> = None;
        // Outermost first, so each scope can take the one built before it as
        // its caller.
        for (k, (scope, (scope_locals, scope_stack))) in
            chain.scopes.iter().zip(per_scope.iter()).enumerate().rev()
        {
            let outermost = k + 1 == n;
            let method_key = if outermost {
                outer_key.clone()
            } else if scope.method_key.is_empty() {
                return Err("a spliced scope has no method key");
            } else {
                scope.method_key.clone()
            };
            linked = Some(FrameState {
                class_id: if outermost { 0 } else { scope.class_id },
                method_key,
                bci: scope.bci,
                locals: scope_locals.to_vec(),
                stack: scope_stack.to_vec(),
                // Every held monitor is the compiling method's: a spliced
                // body never takes one (`ir::SpliceScopeChain`).
                monitors: if outermost {
                    monitors.take().unwrap_or_default()
                } else {
                    Vec::new()
                },
                caller: linked.map(Box::new),
            });
        }
        let fs = linked.ok_or("an empty chain")?;
        if let Some(why) = crate::deopt::inlined_chain_refusal(&fs) {
            return Err(why);
        }
        Ok(fs)
    }

    /// Round 13 wave 6 (lane chain2): [`Self::resolve_frame_values`] for the
    /// CHAIN snapshot `si`, whose `bci` is a raw combined-buffer pc.
    ///
    /// Identical to it unless a slot names a scalar-replaced object. Such an
    /// object is described at the program point of the deopt sites that
    /// publish THIS snapshot, and `resolve_frame_values` looks those up in
    /// [`Self::deopt_sites_by_bci`], which is keyed by RESUME bci (the
    /// outermost `invoke` for every spliced node) and publishes only flat
    /// frames ([`Self::snapshot_published_by_site`]): for a chain snapshot it
    /// found no site, answered `MaterializationRequired` for every object, and
    /// the chain was never publishable -- whatever escape analysis and the VM
    /// could do with it (`r13w5-resume-ea-chain-virtuals-producer-patch`).
    /// Here the sites are the ones [`Self::splice_chain_snapshot_for_site`]
    /// hands this snapshot, found at their raw pc
    /// ([`Self::chain_deopt_block_and_span`]); the per-object gates of
    /// [`Self::frame_value_for_object`] are unchanged, and one `emitted` set
    /// spans the whole chain, so ids stay chain-wide.
    pub(in crate::ir_lower) fn resolve_chain_frame_values(
        &self,
        si: usize,
        sp: &SafepointSnapshot,
    ) -> (
        Vec<FrameValue>,
        Vec<FrameValue>,
        Vec<crate::deopt::MonitorInfo>,
    ) {
        let Some(sr) = self.sr_map else {
            return self.resolve_frame_values(si, sp);
        };
        let names_object = sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .any(|n| *n != NO_NODE && sr.objects.contains_key(n));
        if !names_object {
            return self.resolve_frame_values(si, sp);
        }
        let (deopt_block, site_span) = self.chain_deopt_block_and_span(si, sp.bci);
        if deopt_block.is_none()
            && false
        {
            // Why no block: every node at this raw pc, and what the two
            // site tests and the schedule say about it.
            let mut why = Vec::new();
            for (id, n) in self.graph.nodes.iter().enumerate() {
                let pc = match n.op {
                    Op::Dead => continue,
                    Op::Guard { bci } => Some(bci),
                    _ => n.bytecode_pc,
                };
                if pc != Some(sp.bci) {
                    continue;
                }
                why.push(format!(
                    "{id}:{:?} fires={} publishes={} block={:?} names={:?}",
                    n.op,
                    self.chain_site_fires_at(n, sp.bci),
                    self.chain_site_publishes(n, si, sp.bci),
                    self.schedule.node_to_block.get(id),
                    n.frame_snapshot,
                ));
            }
            eprintln!(
                "[DBG_SCALAR_DEOPT] chain snapshot {si} at raw pc {} names an object and has no deopt block: post_call_of={} sites=[{}]",
                sp.bci,
                sp.post_call_of,
                why.join("; ")
            );
        }
        let mut emitted: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        let mut locals = Vec::with_capacity(sp.locals.len());
        for &n in &sp.locals {
            locals.push(if n != NO_NODE && sr.objects.contains_key(&n) {
                self.frame_value_for_object(n, deopt_block, site_span, sr, &mut emitted)
            } else {
                self.frame_value_for(n)
            });
        }
        let mut stack = Vec::with_capacity(sp.stack.len());
        for &n in &sp.stack {
            stack.push(if n != NO_NODE && sr.objects.contains_key(&n) {
                self.frame_value_for_object(n, deopt_block, site_span, sr, &mut emitted)
            } else {
                self.frame_value_for(n)
            });
        }
        let monitors = self.resolve_monitors(sp, |n| {
            if n != NO_NODE && sr.objects.contains_key(&n) {
                self.frame_value_for_object(n, deopt_block, site_span, sr, &mut emitted)
            } else {
                self.monitor_object_value(n)
            }
        });
        (locals, stack, monitors)
    }

    /// The deopt block and the `(first, last)` site ids of the sites that
    /// publish chain snapshot `si` at raw pc `raw_pc` -- the pair
    /// [`Self::deopt_block_for_snapshot`] / [`Self::deopt_site_span_in_block`]
    /// answer for a flat frame. `(None, None)` when no site publishes it, or
    /// its sites sit in more than one block (the objects are then refused, as
    /// for a flat frame).
    ///
    /// A site is a live node that deopts at `raw_pc` itself
    /// ([`Self::chain_site_fires_at`]) and whose lowering would publish `si`
    /// ([`Self::chain_site_publishes`]). Every emitter that can publish a chain
    /// (`emit_deopt_unless`, the `Op::Guard` arm) resolves it from the node's
    /// own raw pc, the premise [`Self::index_deopt_sites`] rests on for the
    /// flat frames too; a superset of the real sites only widens the span or
    /// makes the block ambiguous, which refuses more, never less.
    pub(in crate::ir_lower) fn chain_deopt_block_and_span(
        &self,
        si: usize,
        raw_pc: usize,
    ) -> (Option<usize>, Option<(NodeId, NodeId)>) {
        // The candidates at `raw_pc` (a superset of the nodes the two tests
        // below admit there), indexed once per lowering: the scan this
        // replaces walked every node per chain point. The answer does not
        // depend on visiting order (one block, a min/max span, and any
        // unscheduled site refuses).
        let candidates = self.chain_site_candidates_by_pc.get_or_init(|| {
            let mut by_pc: HashMap<usize, Vec<NodeId>> = HashMap::new();
            for (id, n) in self.graph.nodes.iter().enumerate() {
                let pc = match n.op {
                    Op::Dead => continue,
                    Op::Guard { bci } => Some(bci),
                    _ => n.bytecode_pc,
                };
                if let Some(pc) = pc {
                    by_pc.entry(pc).or_default().push(id as NodeId);
                }
            }
            by_pc
        });
        let mut block: Option<usize> = None;
        let mut span: Option<(NodeId, NodeId)> = None;
        for &site in candidates
            .get(&raw_pc)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let id = site as usize;
            let Some(n) = self.graph.nodes.get(id) else {
                continue;
            };
            if !self.chain_site_fires_at(n, raw_pc) || !self.chain_site_publishes(n, si, raw_pc) {
                continue;
            }
            let b = match self.schedule.node_to_block.get(id).copied() {
                Some(b) if b != usize::MAX => b,
                _ => return (None, None),
            };
            if block.is_some_and(|prev| prev != b) {
                return (None, None);
            }
            block = Some(b);
            span = Some(match span {
                None => (site, site),
                Some((lo, hi)) => (lo.min(site), hi.max(site)),
            });
        }
        (block, span)
    }

    /// [`Self::fires_deopt_at`] asked of a RAW pc inside a spliced body: an
    /// `Op::Guard` anchored there, an integer division there with no guard
    /// anchored beside it, or any other node there that can deopt.
    pub(in crate::ir_lower) fn chain_site_fires_at(&self, n: &Node, raw_pc: usize) -> bool {
        match n.op {
            Op::Guard { bci } => bci == raw_pc,
            Op::Div | Op::Rem => {
                n.bytecode_pc == Some(raw_pc)
                    && matches!(n.ty, IrType::Int | IrType::Long)
                    && !self.guard_bcis.contains(&raw_pc)
                    && !int_division_cannot_trap(self.graph, n)
            }
            _ => n.bytecode_pc == Some(raw_pc) && !self.node_cannot_deopt_here(n),
        }
    }

    /// Would lowering the site `n` at raw pc `raw_pc` publish chain snapshot
    /// `si`? The rule of [`Self::splice_chain_snapshot_for_site`], with the
    /// node's own `frame_snapshot` standing for `cur_node_frame`: a node naming
    /// a chain snapshot at `raw_pc` publishes that one, a node naming any other
    /// snapshot publishes the flat frame, and every other node the first chain
    /// snapshot at `raw_pc`.
    pub(in crate::ir_lower) fn chain_site_publishes(&self, n: &Node, si: usize, raw_pc: usize) -> bool {
        let first = self.splice_state_first_by_pc.get(&raw_pc).copied();
        match n.frame_snapshot {
            Some(sj) => match self.graph.safepoints.get(sj as usize) {
                Some(named) => named.is_splice_state() && named.bci == raw_pc && sj as usize == si,
                None => first == Some(si),
            },
            None => first == Some(si),
        }
    }

    /// The frame state a guard at RAW pc `raw_pc` publishes when that pc lies
    /// inside a spliced body and a chain snapshot describes it
    /// ([`Self::splice_chain_snapshot_for_site`] /
    /// [`Self::splice_chain_frame_state`]); `None` means "use the flat frame
    /// at the outermost `invoke`", the only answer before round 13.
    ///
    /// A REQUIRED chain that cannot be published latches a bailout: for an
    /// unrolled copy's named chain the flat alternative describes another
    /// iteration's program point, and under a chain-relaxed fence
    /// (`Graph::splice_chains_required`, W5-3 stage 3) it would re-run what the
    /// body committed. A compile that cannot describe a guard must not be
    /// installed.
    /// ARGWIN21-7: a deopt at spliced raw pc `raw_pc` resolves FLAT -- it
    /// re-executes the enclosing invoke, reading that invoke's operand stack
    /// (the call's arguments) -- where the keep set relied on a chain resume
    /// and so let the arguments' homes be cleared. Refuse the attempt; the
    /// ladder's keep-everything retry does not use the rule.
    pub(in crate::ir_lower) fn refuse_flat_at_chain_only_pc(&mut self, raw_pc: usize) {
        if self.keep_set_chain_only_pcs.contains(&raw_pc) {
            self.latch_bailout(Bailout::with_context(
                BailoutReason::Internal(
                    "ir_lower: a spliced deopt the keep set read as chain-resumed resumes flat",
                ),
                format!("combined pc {raw_pc}"),
            ));
        }
    }

    /// j27 wave 5 (lane STRPIN), debug only (`CRATONVM_DBG_JITC=1`): name each
    /// slot of chain snapshot `si` that [`crate::deopt::inlined_chain_refusal`]
    /// refuses as unrebuildable, with its node and resolved value -- the
    /// bailout line alone says only that one exists.
    #[cold]
    pub(in crate::ir_lower) fn report_unrebuildable_chain_slots(&self, si: usize, raw_pc: usize) {
        let Some(sp) = self.graph.safepoints.get(si) else {
            return;
        };
        let was_quiet = self.quiet_dropped_home.replace(true);
        let (locals, stack, _) = self.resolve_chain_frame_values(si, sp);
        self.quiet_dropped_home.set(was_quiet);
        let named = sp.locals.iter().zip(locals.iter()).map(|(n, v)| ("local", n, v));
        let stacked = sp.stack.iter().zip(stack.iter()).map(|(n, v)| ("stack", n, v));
        for (k, (kind, node, value)) in named.chain(stacked).enumerate() {
            if !crate::deopt::chain_value_unrebuildable(value) {
                continue;
            }
            let i = *node as usize;
            let op = self.graph.nodes.get(i).map(|n| format!("{:?}", n.op));
            let flag = |v: &Vec<bool>| v.get(i).copied().unwrap_or(false);
            let users: Vec<String> = self
                .graph
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.inputs.contains(node))
                .map(|(u, n)| format!("n{u}:{:?}", n.op))
                .collect();
            eprintln!(
                "[cratonvm-jitc] ir-chain-unrebuildable combined pc {raw_pc} snapshot {si} slot {k} \
                 ({kind}) n{node} op={} value={value:?} fused_cmp_unnamed={} home_dropped={} \
                 site_named={} defined={} slot={} block={:?} in_block={} site_block={:?} \
                 planned={} users=[{}]",
                op.as_deref().unwrap_or("?"),
                flag(&self.fused_cmp_unnamed),
                flag(&self.home_dropped),
                flag(&self.site_named),
                flag(&self.defined_nodes),
                self.node_slot.get(i).copied().flatten().is_some(),
                self.schedule.node_to_block.get(i).copied(),
                self.schedule.blocks.iter().any(|b| b.nodes.contains(node)),
                self.cur_def
                    .and_then(|d| self.schedule.node_to_block.get(d as usize))
                    .copied(),
                self.planned_slot_off(*node).is_ok(),
                users.join(" "),
            );
        }
    }

    pub(in crate::ir_lower) fn splice_chain_for_site(&mut self, raw_pc: usize) -> Option<FrameState> {
        // Round 15 wave 1 (lane chain, hand-back): a site inside a synchronized
        // splice's monitor window publishes no chain -- the flat frame, as in
        // the default arm -- and is therefore no chain point either (so
        // `CRATONVM_DEOPT_EAGER_CHAINS` never forces it). Before this, the
        // chain named the callee's frame without the window's monitor, and a
        // forced resume ran the rest of the synchronized body unlocked while
        // the compiled enter's hold was never released.
        if self.sync_window_site_publishes_no_chain() {
            self.sync_window_flat_sites += 1;
            return None;
        }
        // W5-3 stage 3: when the builder relaxed a replay fence on the strength
        // of the chains (`Graph::splice_chains_required`), the flat frame is no
        // fall-back for ANY guard in a spliced body -- it would re-run what the
        // body committed.
        // JIT round 23 wave 1, lane L2 (INLINE22-4): except inside a lambda
        // SAM forwarder's own body, which is spliced at depth 0 and commits
        // nothing before its one call: the flat frame re-runs only the
        // forwarder's own `invoke`; and (wave 2) anywhere under an opaque
        // forwarder, whose subtree relaxed no fence
        // (`IrInlineFrameSites::pc_resumes_flat_under_forwarder`).
        let graph_requires = self.graph.splice_chains_required
            && (self.pc_is_in_a_spliced_body(raw_pc)
                || self.splice_state_first_by_pc.contains_key(&raw_pc))
            && !self
                .inline_frame_sites
                .pc_resumes_flat_under_forwarder(raw_pc, &self.graph.opaque_forwarder_bases);
        let Some((si, named)) = self.splice_chain_snapshot_for_site(raw_pc) else {
            if graph_requires {
                self.splice_chain_fallbacks += 1;
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: a spliced guard after a chain-relaxed fence has no caller chain",
                    ),
                    format!("combined pc {raw_pc}"),
                ));
            }
            return None;
        };
        let required = named || graph_requires;
        match self.splice_chain_frame_state(si) {
            Ok(fs) => {
                self.splice_chain_points += 1;
                self.chain_transfer_pcs.insert(raw_pc);
                Some(fs)
            }
            Err(why) => {
                self.splice_chain_fallbacks += 1;
                if required {
                    if false {
                        self.report_unrebuildable_chain_slots(si, raw_pc);
                    }
                    self.latch_bailout(Bailout::with_context(
                        BailoutReason::Internal(
                            "ir_lower: a spliced guard's required caller chain cannot be published",
                        ),
                        format!("combined pc {raw_pc}: {why}"),
                    ));
                }
                None
            }
        }
    }

    /// Resolve the `FrameState` for `bci` from its recorded safepoint
    /// snapshot.
    ///
    /// # No snapshot at `bci`
    ///
    /// A graph the bytecode front end built records a snapshot at every bci it
    /// walked (at least the method entry's), and
    /// `ea_ir_bridge::ir_prune_unconsumable_snapshots` keeps every bci a
    /// trapping node resumes at. So a builder graph with no snapshot HERE is a
    /// broken invariant, and the empty frame below would resume the
    /// interpreter at `bci` with every local blank (0 / null) once the
    /// installer stamps the method key onto it. Round 11 wave 2 (lane lower):
    /// that case now latches a bailout and the compile is refused.
    ///
    /// A graph with NO snapshots at all is a hand-built fixture (the builder
    /// never produces one), and keeps the historical empty frame so the unit
    /// tests that build guards without frame states still lower.
    ///
    /// First match on `bci` wins (`safepoint_first_by_bci`, built once in
    /// `Lowerer::new`). It is a `position` rather than a `find` because the
    /// index used to bind the snapshot to its inlined scope; round 10 wave 9
    /// retired `ir::InlineScopeTable`, and the index is now only the map key.
    pub(in crate::ir_lower) fn resolve_frame_state_for_bci(&self, bci: usize) -> FrameState {
        let found = self
            .safepoint_first_by_bci
            .get(&bci)
            .and_then(|&idx| self.graph.safepoints.get(idx).map(|sp| (idx, sp)));
        match found {
            Some((idx, sp)) => self.resolve_frame_state(idx, sp),
            None => {
                if !self.graph.safepoints.is_empty() {
                    self.latch_bailout(Bailout::with_context(
                        BailoutReason::Internal(
                            "ir_lower: a deopt site has no frame state at its resume bci",
                        ),
                        format!("resume bci {bci}"),
                    ));
                }
                self.empty_frame_state(bci)
            }
        }
    }

    /// The frame a deopt at `bci` gets when no snapshot describes it: no
    /// locals, no stack, no monitors. Reached only for a hand-built graph with
    /// no snapshots, or after [`Self::resolve_frame_state_for_bci`] has
    /// latched the bailout that discards the artifact.
    pub(in crate::ir_lower) fn empty_frame_state(&self, bci: usize) -> FrameState {
        FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: bci as u32,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        }
    }

    /// The bytecode pc a deopt planted while lowering `node` resumes at (before
    /// [`Self::resume_bci`]).
    ///
    /// A node that can trap and carries no pc used to resume at bci 0: the
    /// method-ENTRY snapshot, which re-runs every side effect the compiled body
    /// had already committed. No builder or optimizer path is known to create
    /// such a node, so on a builder graph (one with snapshots) it is now a
    /// refused compile rather than a silent replay. A hand-built fixture with
    /// no snapshots keeps the historical 0 (round 11 wave 2, lane lower).
    pub(in crate::ir_lower) fn deopt_pc_of(&self, node: &Node) -> usize {
        match node.bytecode_pc {
            Some(pc) => pc,
            None => {
                if !self.graph.safepoints.is_empty() {
                    self.latch_bailout(Bailout::with_context(
                        BailoutReason::Internal(
                            "ir_lower: a node that can deopt carries no bytecode pc",
                        ),
                        format!("op {:?}", node.op),
                    ));
                }
                0
            }
        }
    }

    /// Lower integer division and remainder through the fixed-register table.
    /// The guards and IDIV consume the same bound divisor.
    pub(in crate::ir_lower) fn emit_integer_div_rem(&mut self, id: NodeId, node: &Node, is_rem: bool) {
        use crate::x64::isel::{integer_division_steps, MInst, Ty};

        let wide = node.ty == IrType::Long;
        let ty = if wide { Ty::I64 } else { Ty::I32 };
        let selected = self.mir.as_ref().and_then(|plan| plan.division(id));
        let steps = selected.unwrap_or_else(|| {
            integer_division_steps(ty, id, node.inputs[0], node.inputs[1], is_rem)
        });
        let [extend @ MInst::SignExtendDividend { dividend, .. },
             divide @ MInst::Idiv { divisor, remainder, .. }] = steps else {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "integer division has no fixed-register sequence",
            )));
            return;
        };
        if dividend != node.inputs[0] {
            self.gp_load_value(RAX, dividend);
        }
        let resident = if selected.is_some() && self.reg_alloc_on && !self.carry_names(divisor) {
            self.resident_gpr(divisor)
                .filter(|&reg| !matches!(reg, RAX | RDX))
        } else {
            None
        };
        let reg = resident.unwrap_or(RCX);
        let (Ok(extend), Ok(divide)) = (extend.probe(), divide.bind_divisor(reg)) else {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "integer division has no encoding for its fixed operands",
            )));
            return;
        };
        if resident.is_some() {
            self.gp_reg_reads += 1;
        } else {
            self.gp_load_value(RCX, divisor);
        }
        let zero = self.emit_div_zero_guard(node.ty, node.bytecode_pc, divisor, reg);
        let overflow = self.emit_div_overflow_guard(node.ty, remainder, divisor, reg);
        self.buf.emit(&extend.bytes);
        self.buf.emit(&divide.bytes);
        if remainder {
            self.emit_mov_reg_reg64(RAX, RDX);
        }
        for patch in [overflow, zero].into_iter().flatten() {
            self.patch_div_overflow_after(patch);
        }
        if selected.is_some() {
            self.mir_tiles += 1;
        }
    }

    /// Keep a zero divisor away from the raw `IDIV` (which would raise
    /// `#DE`/`SIGFPE`) for an `Op::Div`/`Op::Rem` whose dividend is in RAX and
    /// divisor in its bound register. A bci with no snapshot or guard can leave a
    /// hand-built variable division with a bare `IDIV`.
    ///
    /// Two shapes, chosen by whether the builder anchored an `Op::Guard` at
    /// this bci (see `ir::IrBuilder::add_div_zero_guard`):
    ///
    /// * **Guard present** (every graph the bytecode front end builds) — the
    ///   ArithmeticException is that guard's job, and the guard is
    ///   control-anchored, so it fires on exactly the paths the bytecode
    ///   reaches. This node, by contrast, is a *floating* one the scheduler may
    ///   have hoisted above the branch that guards the division, so it must not
    ///   trap: materialise a placeholder and `JMP` past the `IDIV`, exactly as
    ///   [`Self::emit_div_overflow_guard`] does for `MIN / -1`. The placeholder
    ///   is only ever read on a path where the division did not happen in the
    ///   source program, so its value is dead. Returns the position of the
    ///   forward `JMP` rel32 the caller must patch to the post-`IDIV`
    ///   continuation.
    /// * **No guard** (hand-built optimizer fixtures) — unchanged: deopt at
    ///   this bci and let the interpreter re-execute the division and throw.
    ///   Returns `None`.
    ///
    /// A divisor that is a non-zero constant at the division's width emits
    /// nothing at all and returns `None` (round 9 wave 2, iropt X1): the test
    /// could never be taken, and -- once `ir_optimize` has folded the builder's
    /// `Guard(Cmp(Ne)[c, 0])` away -- the "no guard" shape used to plant a
    /// `TEST; JZ` and a `DivByZero` deopt point for a literal `10`.
    /// [`graph_cannot_deopt`] predicts exactly this (see
    /// [`int_division_cannot_trap`]); the two must agree.
    pub(in crate::ir_lower) fn emit_div_zero_guard(
        &mut self,
        ty: IrType,
        bytecode_pc: Option<usize>,
        divisor: NodeId,
        divisor_reg: u8,
    ) -> Option<DivFixupPatch> {
        if const_int_divisor(self.graph, ty, divisor).is_some_and(|c| c != 0) {
            return None;
        }
        // A division by the constant 0 is never one the program executes: the
        // builder anchors a zero-divisor `Op::Guard` ahead of every division
        // whose divisor is not a non-zero constant, and a guard that decided
        // `0 != 0` leaves nothing after it. What reaches here is a floating
        // node the optimizer left behind when it pruned the arm that owned it
        // (its operands replaced by the zeros of the dead region), which the
        // scheduler may place in a block the method does run -- `Duration.plus`
        // spliced into `VertxConnection.close()` with `amountToAdd == 0` reached
        // a bare `IDIV` of 0 by 0 on every `NetSocket.close()` and died with
        // `#DE`. Skip the `IDIV` the way the anchored shape does: the
        // placeholder is only read on a path where the division did not happen
        // in the source program.
        if const_int_divisor(self.graph, ty, divisor) == Some(0) {
            self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            self.buf.emit_byte(0xE9);
            let after_patch = self.buf.pos();
            self.buf.emit(&[0, 0, 0, 0]);
            return Some(DivFixupPatch::Inline(after_patch));
        }
        // Round nio21 wave 10 (lane L2): or a division inside a SPLICED body
        // whose guard the builder let stand (`IrBuilder::splice_div_guard_resumes`,
        // `CRATONVM_JIT_IR_SPLICE_DIV_GUARD`). A combined pc has no flat
        // snapshot, so without this arm the division would lower to a bare
        // `IDIV` that a zero divisor turns into `#DE`; with it, the anchored
        // shape below materialises a placeholder and the guard deopts.
        let bci = match bytecode_pc {
            Some(b) if self.safepoint_bcis.contains(&b) || self.guard_bcis.contains(&b) => b,
            _ => return None,
        };
        // TEST at the Java operand width, so ZF means divisor == 0.
        let hi = u8::from(divisor_reg >= 8);
        let rex = if ty == IrType::Long { 0x48 } else { 0x40 } | (hi << 2) | hi;
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x85, 0xc0 | ((divisor_reg & 7) << 3) | (divisor_reg & 7)]);
        // Both questions are answered from sets `Lowerer::new` gathered once;
        // asking the graph here was a whole-graph scan per division.
        let anchored = self.guard_bcis.contains(&bci);
        if !anchored {
            self.emit_deopt_if_zero(bci, DeoptReason::DivByZero);
            return None;
        }
        // Round 16 (lane codegen, CG16-2): `JZ .cold` (not taken), the
        // placeholder after the body.
        if self.div_cold_fixups_on() {
            return Some(self.emit_div_cold_jcc(&[0x31, 0xC0])); // XOR EAX, EAX
        }
        // JNZ do_div (divisor != 0 → the real division).
        self.buf.emit(&[0x0F, 0x85]);
        let jnz = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // Divisor == 0: XOR EAX,EAX (zeroes the full RAX for both widths and
        // for both quotient and remainder) and jump past the `IDIV`.
        self.buf.emit(&[0x31, 0xC0]);
        self.buf.emit_byte(0xE9);
        let after_patch = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        let do_div = self.buf.pos();
        let rel = do_div as i32 - (jnz as i32 + 4);
        // div-zero JNZ -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, jnz, rel);
        Some(DivFixupPatch::Inline(after_patch))
    }

    /// May this division put its fixups after the body (round 16, lane
    /// codegen, CG16-2)? The switch, and never under a MIR mode.
    pub(in crate::ir_lower) fn div_cold_fixups_on(&self) -> bool {
        self.mir_mode == MirMode::Off && ir_div_cold_fixups_enabled()
    }

    /// `JE rel32 .cold` (= `JZ`), right after the division's `TEST` / `CMP`,
    /// and a [`DivColdFixup`] carrying `fix` (at most three bytes) for
    /// [`Self::emit_div_cold_fixups`]. Round 16 (lane codegen, CG16-2).
    ///
    /// The same program as the inline shape: on the rare side the fixup
    /// writes RAX exactly as the inline one does and lands on the same
    /// post-`IDIV` offset ([`Self::patch_div_overflow_after`] records it);
    /// nothing between the site and that offset reads a flag or a register
    /// the fixup leaves differently, because the inline fixup path skipped
    /// the same `IDIV` (and, for a remainder, the same `MOV RAX, RDX`). An
    /// unemitted fixup leaves the `rel32` zero, i.e. falls into the `IDIV`,
    /// so the flush refuses the compile rather than ever leaving one behind.
    pub(in crate::ir_lower) fn emit_div_cold_jcc(&mut self, fix: &[u8]) -> DivFixupPatch {
        self.buf.emit(&[0x0F, 0x84]); // JE rel32 .cold
        let jcc_patch = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        let mut bytes = [0u8; 3];
        let len = fix.len().min(bytes.len());
        bytes[..len].copy_from_slice(&fix[..len]);
        self.div_cold_fixups.push(DivColdFixup {
            jcc_patch,
            fix: bytes,
            // Cast: `len <= 3`.
            fix_len: len as u8,
            resume: None,
        });
        DivFixupPatch::Cold(self.div_cold_fixups.len() - 1)
    }

    /// The out-of-line halves of [`Self::emit_div_cold_jcc`], after the body:
    /// each site's `JE` lands on its fixup, which `JMP`s back to the
    /// post-`IDIV` offset. A fixup with no recorded continuation (a site whose
    /// caller never reached its `IDIV`) refuses the compile.
    pub(in crate::ir_lower) fn emit_div_cold_fixups(&mut self) {
        let fixups = std::mem::take(&mut self.div_cold_fixups);
        for f in fixups {
            let Some(resume) = f.resume else {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: a cold division fixup has no continuation",
                )));
                return;
            };
            // Cast: code offsets of one method, far below `i32::MAX`.
            let rel = self.buf.pos() as i32 - (f.jcc_patch as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, f.jcc_patch, rel) {
                return;
            }
            self.buf.emit(&f.fix[..usize::from(f.fix_len)]);
            self.buf.emit_byte(0xE9); // JMP rel32 back to the continuation
            let back_patch = self.buf.pos();
            // Cast: as above.
            let back = resume as i32 - (back_patch as i32 + 4);
            self.buf.emit(&back.to_le_bytes());
        }
    }

    /// Emit the JVMS `MIN_VALUE / -1` overflow guard for an `Op::Div`/`Op::Rem`
    /// whose dividend is in RAX and divisor in RCX (after `emit_div_zero_guard`).
    /// A raw `IDIV` on `MIN / -1` raises `#DE`, so a divisor of `-1` never
    /// reaches the `IDIV`: the spec result is materialised directly and the
    /// guard `JMP`s past it.
    ///
    /// Round 9 wave 2 (isel #2): this now tests the **divisor alone**, the
    /// shape `x64::arith::emit_safe_idiv` ("Guard 2") took in round 9 --
    ///
    /// ```text
    ///   CMP ECX/RCX, -1 ; JNE do_div
    ///   NEG EAX/RAX            (quotient: x / -1 == -x, and NEG MIN == MIN)
    ///   | XOR EAX, EAX         (remainder: x % -1 == 0 for every x)
    ///   JMP after
    /// do_div:
    /// ```
    ///
    /// -- instead of `CMP dividend, MIN; JNE; CMP divisor, -1; JNE` (plus a
    /// 10-byte `MOV R10, imm64` for `long`). One compare-and-branch fewer on
    /// every division, no R10 use, and no `IDIV` at all for a divisor of -1.
    /// `NEG EAX` zero-extends into RAX exactly as the 32-bit `IDIV` (and the
    /// `MOV EAX, 0x80000000` this replaced) did.
    ///
    /// Returns `None`, emitting nothing, when the divisor is a constant other
    /// than -1 at the division's width (round 9 wave 2, iropt X1). Otherwise
    /// returns the position of the forward `JMP` rel32 the caller must patch
    /// to the post-`IDIV` continuation.
    pub(in crate::ir_lower) fn emit_div_overflow_guard(
        &mut self,
        ty: IrType,
        is_rem: bool,
        divisor: NodeId,
        divisor_reg: u8,
    ) -> Option<DivFixupPatch> {
        if const_int_divisor(self.graph, ty, divisor).is_some_and(|c| c != -1) {
            return None;
        }
        self.emit_cmp_reg_imm(divisor_reg, -1, ty == IrType::Long);
        // Round 16 (lane codegen, CG16-2): `JE .cold` (not taken), the same
        // answer after the body.
        if self.div_cold_fixups_on() {
            let fix: &[u8] = if is_rem {
                &[0x31, 0xC0] // XOR EAX, EAX
            } else if ty == IrType::Int {
                &[0xF7, 0xD8] // NEG EAX
            } else {
                &[0x48, 0xF7, 0xD8] // NEG RAX
            };
            return Some(self.emit_div_cold_jcc(fix));
        }
        // JNE do_div (75 rel8) -- the skipped block is at most 8 bytes.
        self.buf.emit_byte(0x75);
        let jne = self.buf.pos();
        self.buf.emit_byte(0x00);
        if is_rem {
            // remainder == 0 -> XOR EAX, EAX (zeros the full RAX).
            self.buf.emit(&[0x31, 0xC0]);
        } else if ty == IrType::Int {
            // quotient == -dividend -> NEG EAX (zero-extends into RAX).
            self.buf.emit(&[0xF7, 0xD8]);
        } else {
            // quotient == -dividend -> NEG RAX.
            self.buf.emit(&[0x48, 0xF7, 0xD8]);
        }
        // JMP after (E9 rel32) -- patched by the caller after the IDIV.
        self.buf.emit_byte(0xE9);
        let after_patch = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // do_div: the IDIV the caller emits next.
        let do_div = self.buf.pos();
        // Range-checked, not truncated: see `ExecutableBuffer::patch_rel8_or_bail`.
        self.buf
            .patch_rel8_or_bail(jne, (do_div as i64) - (jne as i64) - 1);
        Some(DivFixupPatch::Inline(after_patch))
    }

    /// Patch the forward `JMP` emitted by [`emit_div_overflow_guard`] to the
    /// current position (the post-`IDIV` continuation, just before the result
    /// is stored). For an out-of-line fixup (round 16, CG16-2), record that
    /// position as where its `JMP` returns.
    pub(in crate::ir_lower) fn patch_div_overflow_after(&mut self, after: DivFixupPatch) {
        let cont = self.buf.pos();
        match after {
            DivFixupPatch::Inline(after_patch) => {
                let rel = cont as i32 - (after_patch as i32 + 4);
                // div-overflow JMP -- tolerated on an overflowed buffer; see
                // `Self::patch_or_bail` / `patch_rel32_to_here`.
                Self::patch_or_bail(&mut self.buf, after_patch, rel);
            }
            DivFixupPatch::Cold(i) => {
                if let Some(f) = self.div_cold_fixups.get_mut(i) {
                    f.resume = Some(cont);
                }
            }
        }
    }

    /// The divisor of an integer `Op::Div`/`Op::Rem` that
    /// [`Self::emit_div_by_const`] lowers without an `IDIV`, normalised to the
    /// division's width: a compile-time constant that is neither zero nor the
    /// width's `MIN` (whose magnitude has no positive twin; it keeps the plain
    /// `IDIV`, which cannot trap for it). `None` also when
    /// `CRATONVM_JIT_IR_CONST_DIV=0`.
    pub(in crate::ir_lower) fn reducible_const_divisor(&self, ty: IrType, divisor: NodeId) -> Option<i64> {
        let d = const_int_divisor(self.graph, ty, divisor)?;
        let min = if ty == IrType::Long {
            i64::MIN
        } else {
            i64::from(i32::MIN)
        };
        (d != 0 && d != min).then_some(d)
    }

    /// `REX.W` (when `wide`) followed by `bytes` -- one instruction whose
    /// 32- and 64-bit forms differ only in that prefix.
    pub(in crate::ir_lower) fn emit_w(&mut self, wide: bool, bytes: &[u8]) {
        if wide {
            self.buf.emit_byte(0x48);
        }
        self.buf.emit(bytes);
    }

    /// Signed division (`is_rem == false`) or remainder of the dividend in RAX
    /// by the constant `d`, with Java's semantics, result in RAX. Clobbers RCX
    /// and RDX -- the registers the `IDIV` form it replaces clobbers (both are
    /// this emitter's fixed scratch, see
    /// `the_gp_file_names_no_register_this_emitter_reserves`). No trap, no
    /// guard, no deopt point: `d` is neither 0 nor the width's `MIN`
    /// ([`Self::reducible_const_divisor`]).
    ///
    /// Round 9 wave 3 (irlower3), item 4 of
    /// `ir-constant-divisor-still-pays-the-zero-and-overflow-tests`. Wave 2
    /// removed the guards for a constant divisor, which made `long`-counted
    /// loops with a `/ 2` or `% 7` eligible for the optimizing OSR body -- and
    /// that body was then 2x SLOWER than the single-pass one it replaced,
    /// because it still paid a 40-90 cycle `IDIV r64` where
    /// `x64::arith` emits shifts and a multiply-high (`CratonBench
    /// arithmetic`: 4.4 s -> 8.5 s). The sequences are the same ones:
    ///
    /// * `|d| == 1`: `n`, `-n` (`NEG`; `NEG MIN == MIN`, Java's `MIN / -1`), or
    ///   remainder 0;
    /// * `|d| == 2^k`: `bias = n < 0 ? 2^k - 1 : 0` (`SAR` + `SHR` of a copy),
    ///   quotient `(n + bias) >> k`, remainder `((n + bias) & (2^k - 1)) - bias`
    ///   (the remainder takes the dividend's sign, as Java's does);
    /// * otherwise Granlund-Montgomery / Hacker's Delight 10-1 with the magic
    ///   of [`signed_div_magic`] for `|d|`: `int` multiplies the sign-extended
    ///   dividend by the (up to 32-bit, unsigned) magic in 64 bits and shifts
    ///   by `32 + s`; `long` takes the high half of a 128-bit `IMUL`, adds `n`
    ///   back when the magic wrapped negative, and shifts by `s`. Both add 1
    ///   for a negative dividend (round toward zero). The remainder is
    ///   `n - q * |d|`.
    ///
    /// A negative divisor negates the quotient of `|d|` (`n / d == -(n / |d|)`
    /// for truncating division, and `|n / |d|| <= 2^(w-2)` cannot overflow the
    /// `NEG`); the remainder ignores the divisor's sign (`n % d == n % -d`).
    ///
    /// An `int` result is left zero-extended in RAX (the last instruction is a
    /// 32-bit op), exactly like the `IDIV ECX` it replaces; the one exception
    /// is `n / 1`, which leaves the dividend register as loaded.
    pub(in crate::ir_lower) fn emit_div_by_const(&mut self, wide: bool, d: i64, is_rem: bool) {
        debug_assert!(d != 0);
        // RCX is written below by raw bytes; tell the deferred-carry tracker,
        // exactly as the `gp_load_value(RCX, ..)` this path skips would have.
        self.note_rcx_written();
        let width: u32 = if wide { 64 } else { 32 };
        let a: u64 = d.unsigned_abs();
        let negative = d < 0;
        if a == 1 {
            if is_rem {
                self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX (zeroes RAX)
            } else if negative {
                self.emit_w(wide, &[0xF7, 0xD8]); // NEG EAX/RAX
            }
            return;
        }
        if a.is_power_of_two() {
            let k = a.trailing_zeros(); // 1..=width-2: MIN is not reducible
                                        // RCX = n < 0 ? 2^k - 1 : 0
            self.emit_w(wide, &[0x89, 0xC1]); // MOV ECX/RCX, EAX/RAX
            if k > 1 {
                // Cast: width - 1 is 31 or 63.
                self.emit_w(wide, &[0xC1, 0xF9, (width - 1) as u8]); // SAR ECX/RCX, w-1
            }
            // Cast: 1 <= k <= width - 2, so width - k is in 2..=width-1.
            self.emit_w(wide, &[0xC1, 0xE9, (width - k) as u8]); // SHR ECX/RCX, w-k
            self.emit_w(wide, &[0x01, 0xC8]); // ADD EAX/RAX, ECX/RCX
            if !is_rem {
                // Cast: k <= 62.
                self.emit_w(wide, &[0xC1, 0xF8, k as u8]); // SAR EAX/RAX, k
                if negative {
                    self.emit_w(wide, &[0xF7, 0xD8]); // NEG EAX/RAX
                }
                return;
            }
            let mask = a - 1;
            if mask <= 0x7F {
                // Cast: mask <= 127, a positive sign-extended imm8.
                self.emit_w(wide, &[0x83, 0xE0, mask as u8]); // AND EAX/RAX, imm8
            } else if mask <= 0x7FFF_FFFF {
                // Cast: mask <= i32::MAX, so the sign-extended imm32 is itself.
                self.emit_w(wide, &[0x25]); // AND EAX/RAX, imm32
                self.buf.emit(&(mask as u32).to_le_bytes());
            } else {
                // `long` only (an `int` mask is at most 2^30 - 1).
                self.emit_mov_reg_imm64(RDX, mask);
                self.buf.emit(&[0x48, 0x21, 0xD0]); // AND RAX, RDX
            }
            self.emit_w(wide, &[0x29, 0xC8]); // SUB EAX/RAX, ECX/RCX
            return;
        }
        let (magic, shift) = signed_div_magic(a, width);
        if wide {
            self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX -- keep n
            self.emit_mov_reg_imm64(RDX, magic);
            self.buf.emit(&[0x48, 0xF7, 0xEA]); // IMUL RDX: RDX:RAX = n * magic
                                                // Cast: the magic's bit pattern; >= 2^63 means it wrapped negative
                                                // as the signed multiplier, and the true product needs n added back.
            if (magic as i64) < 0 {
                self.buf.emit(&[0x48, 0x01, 0xCA]); // ADD RDX, RCX
            }
            if shift > 0 {
                // Cast: shift <= 63.
                self.buf.emit(&[0x48, 0xC1, 0xFA, shift as u8]); // SAR RDX, s
            }
            self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
            self.buf.emit(&[0x48, 0xC1, 0xE8, 0x3F]); // SHR RAX, 63 -- n < 0
            self.buf.emit(&[0x48, 0x01, 0xD0]); // ADD RAX, RDX -- q = n / |d|
        } else {
            self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX -- keep n
            self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
            if magic <= 0x7FFF_FFFF {
                // Cast: magic <= i32::MAX, so the sign-extended imm32 is itself.
                self.buf.emit(&[0x48, 0x69, 0xC0]); // IMUL RAX, RAX, imm32
                self.buf.emit(&(magic as u32).to_le_bytes());
            } else {
                // magic < 2^32 and |n| <= 2^31: the product fits 63 bits.
                self.emit_mov_reg_imm64(RDX, magic);
                self.buf.emit(&[0x48, 0x0F, 0xAF, 0xC2]); // IMUL RAX, RDX
            }
            // Cast: 32 + shift <= 63.
            self.buf.emit(&[0x48, 0xC1, 0xF8, (32 + shift) as u8]); // SAR RAX, 32+s
            self.buf.emit(&[0x89, 0xCA]); // MOV EDX, ECX
            self.buf.emit(&[0xC1, 0xEA, 0x1F]); // SHR EDX, 31 -- n < 0
            self.buf.emit(&[0x01, 0xD0]); // ADD EAX, EDX -- q = n / |d|
        }
        if !is_rem {
            if negative {
                self.emit_w(wide, &[0xF7, 0xD8]); // NEG EAX/RAX
            }
            return;
        }
        // r = n - q * |d|, with n still in RCX.
        if a <= 0x7F {
            // Cast: a <= 127, a positive sign-extended imm8.
            self.emit_w(wide, &[0x6B, 0xC0, a as u8]); // IMUL EAX/RAX, EAX/RAX, imm8
        } else if a <= 0x7FFF_FFFF {
            // Cast: a <= i32::MAX, so the sign-extended imm32 is itself.
            self.emit_w(wide, &[0x69, 0xC0]); // IMUL EAX/RAX, EAX/RAX, imm32
            self.buf.emit(&(a as u32).to_le_bytes());
        } else {
            // `long` only (an `int` |d| is at most 2^31 - 1).
            self.emit_mov_reg_imm64(RDX, a);
            self.buf.emit(&[0x48, 0x0F, 0xAF, 0xC2]); // IMUL RAX, RDX
        }
        self.emit_w(wide, &[0x29, 0xC1]); // SUB ECX/RCX, EAX/RAX
        self.emit_w(wide, &[0x89, 0xC8]); // MOV EAX/RAX, ECX/RCX
    }

    /// nio20 wave 5, lane L2: the confined-session check of the scoped fold
    /// (see [`scoped_session_confined_rows`] for why it is sound), entered
    /// with RDX = a session whose class id is exactly `row.class_id`. Falls
    /// through when `checkValidStateRaw()` would pass; deopts
    /// (`ReceiverTypeChanged`, the fold's existing reason) otherwise, and the
    /// interpreter then raises `WrongThreadException` / `IllegalStateException`
    /// exactly. Writes RAX, RCX (the epoch guard's fallback, and a failing
    /// deopt edge on Windows), R10 and R11; RDX is preserved.
    ///
    /// ```text
    ///   [layout-epoch CMP ; deopt unless equal]
    ///   TEST BYTE [RDX+GC_FLAGS], COMPACT ; JZ legacy
    ///   MOV  EAX, [RDX+state_c] ; MOV R10, [RDX+owner_c] ; JMP have
    /// legacy:
    ///   CMP  DWORD [RDX+owner_tag], OBJECT ; deopt unless equal
    ///   MOV  EAX, [RDX+state_l] ; MOV R10, [RDX+owner_l]
    /// have:
    ///   CMP EAX, 1 ; deopt unless equal          (model)
    ///   | TEST EAX, EAX ; deopt if negative      (JDK)
    ///   MOV  RAX, TLS[JIT_THREAD] ; TEST ; deopt if zero
    ///   MOV  RAX, [RAX+thread_off] ; TEST ; deopt if zero
    ///   CMP  RAX, R10 ; deopt unless equal
    /// ```
    ///
    /// `shared` (JIT r20 wave 19, lane yFfmShared, MEM20-2; the window's row
    /// only -- since JIT round 22 the JDK's `SharedSession` when loaded, else
    /// the model): a session reading OPEN with a NULL owner -- a shared arena's --
    /// jumps to an edge appended there instead of failing the owner compare;
    /// the caller lowers it through the windowed tail
    /// ([`Self::emit_scoped_window_open`]). `None` keeps the deopt.
    pub(in crate::ir_lower) fn emit_scoped_confined_session_check(
        &mut self,
        row: ScopedConfinedRow,
        thread_off: i32,
        bci: usize,
        shared: Option<&mut Vec<usize>>,
    ) {
        let reason = DeoptReason::ReceiverTypeChanged;
        self.note_rcx_written();
        if self.emit_layout_epoch_cmp() {
            self.emit_deopt_unless(0x84, bci, reason); // JE: the baked layout holds
        }
        // TEST BYTE [RDX + GC_FLAGS], COMPACT (F6 /0 ib, disp8) ; JZ legacy
        self.buf.emit(&[
            0xF6,
            0x42,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        ]);
        let legacy = self.emit_jcc_rel32(0x84);
        self.buf.emit(&[0x8B, 0x82]); // MOV EAX, [RDX + disp32]
        self.buf.emit(&row.state_compact.to_le_bytes());
        self.buf.emit(&[0x4C, 0x8B, 0x92]); // MOV R10, [RDX + disp32]
        self.buf.emit(&row.owner_compact.to_le_bytes());
        let have = self.emit_jmp_rel32();
        self.patch_rel32_to_here(legacy);
        // CMP DWORD [RDX + disp32], imm32 (81 /7): the owner cell holds a
        // reference (a cell that does not is never "this thread").
        self.buf.emit(&[0x81, 0xBA]);
        self.buf.emit(&row.owner_legacy_tag.to_le_bytes());
        let object_tag = cratonvm_types::FIELD_CELL_TAG_OBJECT;
        self.buf.emit(&object_tag.to_le_bytes());
        self.emit_deopt_unless(0x84, bci, reason); // JE
        self.buf.emit(&[0x8B, 0x82]); // MOV EAX, [RDX + disp32]
        self.buf.emit(&row.state_legacy.to_le_bytes());
        self.buf.emit(&[0x4C, 0x8B, 0x92]); // MOV R10, [RDX + disp32]
        self.buf.emit(&row.owner_legacy.to_le_bytes());
        self.patch_rel32_to_here(have);
        // nio21 wave 2 (lane L3, W1L3-2): a MODEL session of the never-closing
        // kind (`state == 2`, `foreign_ffm.rs` `P67_SESSION_STATE_NONCLOSEABLE`:
        // `Arena.global()`, `Arena.ofAuto()`, `createImplicit`, `createHeap`)
        // with a null owner passes with no thread test: its
        // `checkValidStateRaw` cannot fail (null owner, non-zero state) and its
        // `justClose` refuses before writing anything, so no close can precede
        // or interleave with the access -- the `GlobalSession` /
        // `ImplicitSession` argument of `scoped_session_never_closing_class_ids`.
        //   CMP EAX, 2 ; JNE confined ; TEST R10, R10 ; JZ pass ; confined:
        let noncloseable_pass = if row.open == ScopedSessionOpen::EqualsOne
            && ir_scoped_noncloseable_model_fold_enabled()
        {
            self.buf
                .emit(&[0x83, 0xF8, SCOPED_MODEL_STATE_NONCLOSEABLE]); // CMP EAX, 2
            let not_kind = self.emit_jcc_rel32(0x85); // JNE
            self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
            let pass = self.emit_jcc_rel32(0x84); // JZ: owner == null
            self.patch_rel32_to_here(not_kind);
            Some(pass)
        } else {
            None
        };
        match row.open {
            ScopedSessionOpen::EqualsOne => {
                self.buf.emit(&[0x83, 0xF8, 0x01]); // CMP EAX, 1
                self.emit_deopt_unless(0x84, bci, reason); // JE: open
            }
            ScopedSessionOpen::NonNegative => {
                self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                self.emit_deopt_unless(0x89, bci, reason); // JNS: state >= OPEN
            }
        }
        // JIT r20 wave 19 (lane yFfmShared, MEM20-2): open and owner-less --
        // a SHARED model session (any thread may close it) -- leaves for the
        // windowed tail, which re-reads `state` inside this thread's access
        // window. This read only classifies; it proves nothing about liveness.
        if let Some(shared) = shared {
            self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
            shared.push(self.emit_jcc_rel32(0x84)); // JZ: owner == null
        }
        // Shared sessions only pass through the window; leave a non-null owner to the JDK.
        if row.shared_only {
            self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            self.emit_deopt_if_zero(bci, reason);
            return;
        }
        // RAX = the current `JvmThread*` (the TLS mirror; `scoped_session_
        // confined_rows` refused a compile without one), then its
        // `java.lang.Thread` word. Either zero: no mirror to compare, deopt.
        let tls = u32::try_from(crate::x64::jit_thread_tls_disp()).unwrap_or(0);
        self.emit_mov_rax_tls_disp32(tls);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        self.emit_deopt_unless(0x85, bci, reason); // JNZ
        self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
        self.buf.emit(&thread_off.to_le_bytes());
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        self.emit_deopt_unless(0x85, bci, reason); // JNZ
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        self.emit_deopt_unless(0x84, bci, reason); // JE: owner == current thread
        if let Some(pass) = noncloseable_pass {
            self.patch_rel32_to_here(pass);
        }
    }

    /// JIT round 23 close (`CRATONVM_JIT_IR_FFM_SESSION_PRECHECK`): lower
    /// [`crate::ir::UnboxOp::SessionPrecheck`], inputs `[ctrl, mem, session]`.
    ///
    /// The result is `null` when the session passes exactly the tests
    /// [`Self::lower_scoped_raw`] passes inline -- a never-closing class
    /// ([`scoped_session_never_closing_class_ids`]), a model session of the
    /// never-closing kind, or a confined row ([`scoped_session_confined_rows`])
    /// whose owner is this thread and whose state reads open -- and the
    /// session itself on every other path, including a null session (which
    /// the access then passes as before) and a layout the compile did not
    /// bake. Nothing here deopts: every failed test is a "no" (keep the
    /// session, so each access checks it). A shared model session is a "no"
    /// too (its close needs the access window, not a one-time read).
    ///
    /// Clobbers RAX, RCX, RDX, R10, R11 and the flags; the result goes
    /// through RAX to its home.
    pub(in crate::ir_lower) fn lower_session_precheck(&mut self, node: &Node, slot: i32) {
        let session = node.inputs[2];
        self.gp_load_value(RDX, session);
        let admitted = scoped_session_never_closing_class_ids(&self.direct_helpers);
        let (confined, thread_off) = scoped_session_confined_rows(&self.direct_helpers);
        let mut pass: Vec<usize> = Vec::new();
        let mut keep: Vec<usize> = Vec::new();
        self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        keep.push(self.emit_jcc_rel32(0x84)); // JZ: null (the access passes it anyway)
        if !admitted.is_empty() || !confined.is_empty() {
            // MOV EAX, [RDX] -- the session's class id (`ObjectHeader` offset 0).
            self.buf.emit(&[0x8B, 0x02]);
            for &cid in &admitted {
                self.buf.emit_byte(0x3D); // CMP EAX, imm32
                self.buf.emit(&cid.to_le_bytes());
                pass.push(self.emit_jcc_rel32(0x84)); // JE: never closes
            }
            // A shared-only row can never pass (its owner is null): a "no",
            // the fall-through, needs no test.
            for row in confined.iter().filter(|r| !r.shared_only) {
                self.buf.emit_byte(0x3D); // CMP EAX, imm32
                self.buf.emit(&row.class_id.to_le_bytes());
                let next = self.emit_jcc_rel32(0x85); // JNE: not this row
                self.emit_session_precheck_row(*row, thread_off, &mut pass, &mut keep);
                self.patch_rel32_to_here(next);
                // The row's reads overwrote EAX; re-read the class id for the
                // next compare.
                self.buf.emit(&[0x8B, 0x02]); // MOV EAX, [RDX]
            }
        }
        // Every "no" lands here: the result is the session (RDX).
        for p in keep {
            self.patch_rel32_to_here(p);
        }
        self.emit_mov_reg_reg64(RAX, RDX);
        let done = self.emit_jmp_rel32();
        for p in pass {
            self.patch_rel32_to_here(p);
        }
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX: proven for the loop
        self.patch_rel32_to_here(done);
        self.store_rax(slot);
        if crate::x64::dbg_jitc_enabled() {
            let confined_ids: Vec<u32> = confined.iter().map(|r| r.class_id).collect();
            eprintln!(
                "[cratonvm-jitc] ffm-session-precheck (ir) in={} admitted={admitted:?} \
                 confined={confined_ids:?}",
                self.graph.scope_method_key
            );
        }
    }

    /// One confined row of [`Self::lower_session_precheck`]: the tests of
    /// [`Self::emit_scoped_confined_session_check`], in its order, each
    /// failure a jump to `keep` instead of a deopt, the success a jump to
    /// `pass`. Entered with the session in RDX and its class id matched.
    pub(in crate::ir_lower) fn emit_session_precheck_row(
        &mut self,
        row: ScopedConfinedRow,
        thread_off: i32,
        pass: &mut Vec<usize>,
        keep: &mut Vec<usize>,
    ) {
        self.note_rcx_written();
        if self.emit_layout_epoch_cmp() {
            keep.push(self.emit_jcc_rel32(0x85)); // JNE: the baked layout moved
        }
        // TEST BYTE [RDX + GC_FLAGS], COMPACT ; JZ legacy
        self.buf.emit(&[
            0xF6,
            0x42,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        ]);
        let legacy = self.emit_jcc_rel32(0x84);
        self.buf.emit(&[0x8B, 0x82]); // MOV EAX, [RDX + disp32]
        self.buf.emit(&row.state_compact.to_le_bytes());
        self.buf.emit(&[0x4C, 0x8B, 0x92]); // MOV R10, [RDX + disp32]
        self.buf.emit(&row.owner_compact.to_le_bytes());
        let have = self.emit_jmp_rel32();
        self.patch_rel32_to_here(legacy);
        // The owner cell must hold a reference.
        self.buf.emit(&[0x81, 0xBA]); // CMP DWORD [RDX + disp32], imm32
        self.buf.emit(&row.owner_legacy_tag.to_le_bytes());
        self.buf.emit(&cratonvm_types::FIELD_CELL_TAG_OBJECT.to_le_bytes());
        keep.push(self.emit_jcc_rel32(0x85)); // JNE
        self.buf.emit(&[0x8B, 0x82]); // MOV EAX, [RDX + disp32]
        self.buf.emit(&row.state_legacy.to_le_bytes());
        self.buf.emit(&[0x4C, 0x8B, 0x92]); // MOV R10, [RDX + disp32]
        self.buf.emit(&row.owner_legacy.to_le_bytes());
        self.patch_rel32_to_here(have);
        // A model session of the never-closing kind with a null owner passes
        // (`emit_scoped_confined_session_check`'s W1L3-2 arm).
        if row.open == ScopedSessionOpen::EqualsOne && ir_scoped_noncloseable_model_fold_enabled() {
            self.buf
                .emit(&[0x83, 0xF8, SCOPED_MODEL_STATE_NONCLOSEABLE]); // CMP EAX, 2
            let not_kind = self.emit_jcc_rel32(0x85); // JNE
            self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
            pass.push(self.emit_jcc_rel32(0x84)); // JZ: owner == null
            self.patch_rel32_to_here(not_kind);
        }
        match row.open {
            ScopedSessionOpen::EqualsOne => {
                self.buf.emit(&[0x83, 0xF8, 0x01]); // CMP EAX, 1
                keep.push(self.emit_jcc_rel32(0x85)); // JNE: not open
            }
            ScopedSessionOpen::NonNegative => {
                self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                keep.push(self.emit_jcc_rel32(0x88)); // JS: closed
            }
        }
        // The current thread's `java.lang.Thread`, compared with the owner (a
        // null owner -- a shared model session -- never equals a non-null
        // thread word, so it keeps its per-access check).
        let tls = u32::try_from(crate::x64::jit_thread_tls_disp()).unwrap_or(0);
        self.emit_mov_rax_tls_disp32(tls);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        keep.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
        self.buf.emit(&thread_off.to_le_bytes());
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        keep.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        pass.push(self.emit_jcc_rel32(0x84)); // JE: owner == current thread
        keep.push(self.emit_jmp_rel32());
    }

    /// Lower a `ScopedMemoryAccess` raw read or write ([`crate::ir::UnboxOp::ScopedGet`] /
    /// [`crate::ir::UnboxOp::ScopedPut`]).
    ///
    /// Inputs `[ctrl, mem, sma, session, base, offset, (value,) (bigEndian)]`.
    /// The access is inline for exactly these shapes, and everything else deopts
    /// (the interpreter then re-executes the call through the real method, which
    /// owns the session check, the `Unsafe` arena slow path, the error paths):
    ///
    /// * `base` is a PRIMITIVE array and `[offset - 16, offset - 16 + width)` lies
    ///   inside its payload (`Unsafe.arrayBaseOffset` is `ARRAY_DATA_OFFSET`, so
    ///   `base + offset` IS the element address);
    /// * `base == null` and `offset` is a positive, untagged real address (bit
    ///   62, `unsafe_arena::ARENA_TAG`, marks an `Unsafe.allocateMemory` arena
    ///   handle, which is not a pointer);
    /// * (nio21 wave 1 L3) `base == null` and `offset` is a live SLOTTED arena
    ///   handle whose block holds `[offset, offset + width)`, resolved through
    ///   the JIT-readable arena table ([`Self::emit_scoped_arena_resolve`]),
    ///   when this compile knows the table ([`scoped_arena_jit_table`]).
    ///
    /// And only with a session the fold admits (null, a never-closing class, a
    /// confined session owned by this thread and open) and a non-null
    /// `ScopedMemoryAccess` receiver. A big-endian request byte-swaps; the
    /// machine is little-endian.
    ///
    /// Register use is RAX, RCX, RDX, R10, R11 only (never local-mapped). RCX is
    /// the shift count while the array length is scaled and `bigEndian` after
    /// the access; on Windows `DEOPT_ARG0` is RCX, which a guard's failing edge
    /// overwrites -- the passing edge never does, and nothing live in RCX
    /// crosses a guard.
    /// JIT round 24 B2: [`Self::lower_scoped_raw`] behind the loop's arena
    /// cache when `cache` (`[entry, key, base, delta]`,
    /// `ir_optimize::ffm_arena_cache`) is given:
    ///
    /// ```text
    ///   R8 = entry ; TEST R8, R8 ; JZ full          ; a miss at the loop entry
    ///   R9 = key ; CMP R9, [R8] ; JNE full          ; the block was freed / moved
    ///   RDX = base ; R11 = delta ; LEA RDX, [RDX + R11]
    ///   <the access at RDX> ; JMP done
    /// full: <lower_scoped_raw> ; done:
    /// ```
    ///
    /// The hit path skips the session check, the receiver test and the
    /// handle decode: the entry was resolved under a session pre-check that
    /// read null for a loop that runs no Java, for the whole range of the
    /// loop's accesses (see the pass notes); every other access state takes
    /// the full path, unchanged.
    pub(in crate::ir_lower) fn lower_scoped_raw_cached(
        &mut self,
        node: &Node,
        uop: crate::ir::UnboxOp,
        bci: usize,
        slot: i32,
        proven: bool,
        cache: Option<[NodeId; 4]>,
    ) {
        let Some([entry, key, base, delta]) = cache else {
            return self.lower_scoped_raw(node, uop, bci, slot, proven);
        };
        let (width, signed, ordered, is_put) = match uop {
            crate::ir::UnboxOp::ScopedGet {
                width,
                signed,
                ordered,
            } => (width, signed, ordered, false),
            crate::ir::UnboxOp::ScopedPut { width, ordered } => (width, false, ordered, true),
            _ => return self.lower_scoped_raw(node, uop, bci, slot, proven),
        };
        let put_value = if is_put { node.inputs.get(6).copied() } else { None };
        let order_input = if ordered {
            node.inputs.get(if is_put { 7 } else { 6 }).copied()
        } else {
            None
        };
        if is_put && put_value.is_none() {
            return self.lower_scoped_raw(node, uop, bci, slot, proven);
        }
        let order_const: Option<bool> =
            order_input.and_then(|o| match self.graph.nodes.get(o as usize).map(|n| &n.op) {
                Some(Op::Const(v)) => Some(*v as i32 != 0),
                _ => None,
            });
        self.gp_load_value(R8, entry);
        self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8, R8
        let miss_entry = self.emit_jcc_rel32(0x84); // JZ full
        // Real-pointer mode: a raw `min` has no slot whose key could move.
        let raw_hit = scoped_arena_real_pointers().then(|| {
            self.buf.emit(&[0x49, 0x83, 0xF8, ARENA_CACHE_RAW_HIT as u8]); // CMP R8, imm8
            self.emit_jcc_rel32(0x84) // JE hit
        });
        self.gp_load_value(R9, key);
        self.buf.emit(&[0x4D, 0x3B, 0x08]); // CMP R9, [R8]  (the slot's key)
        let miss_key = self.emit_jcc_rel32(0x85); // JNE full
        if let Some(hit) = raw_hit {
            self.patch_rel32_to_here(hit);
        }
        self.gp_load_value(RDX, base);
        self.gp_load_value(R11, delta);
        self.buf.emit(&[0x4A, 0x8D, 0x14, 0x1A]); // LEA RDX, [RDX + R11]
        self.emit_scoped_access_at_rdx(
            width,
            signed,
            put_value,
            order_input,
            order_const,
            slot,
            false,
        );
        let done = self.emit_jmp_rel32();
        self.patch_rel32_to_here(miss_entry);
        self.patch_rel32_to_here(miss_key);
        self.lower_scoped_raw(node, uop, bci, slot, proven);
        self.patch_rel32_to_here(done);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] scoped-arena-cached (ir) bci={bci} in={}",
                self.graph.scope_method_key
            );
        }
    }

    /// JIT round 24 B2: lower [`crate::ir::UnboxOp::ArenaCacheEntry`], inputs
    /// `[ctrl, mem, min, span, session]`: the table slot of `min`'s block, or
    /// 0. The scalar resolve's tests in its order (key, length, key), each
    /// failure a 0 instead of a deopt:
    ///
    /// ```text
    ///   R11 = session ; TEST R11, R11 ; JNZ miss          ; the pre-check kept it
    ///   R11 = min ; TEST R11, R11 ; JLE miss
    ///   MOV RAX, 1 << 62 ; TEST R11, RAX ; JZ miss        ; a raw address: full path
    ///                                                     ; (real-pointer mode: EAX = 1)
    ///   R8 = table + ((R11 >> 30) & mask) * 32
    ///   RAX = R11 & !(2^30 - 1) ; CMP RAX, [R8] ; JNE miss
    ///   ECX = R11D & (2^30 - 1) ; RDX = span ; ADD RDX, RCX ; CMP RDX, [R8+8] ; JA miss
    ///   R9 = R11 & !(2^30 - 1) ; CMP R9, [R8] ; JNE miss  ; the length was that block's
    ///   MOV RAX, R8 ; JMP done
    /// miss: XOR EAX, EAX
    /// ```
    pub(in crate::ir_lower) fn lower_arena_cache_entry(&mut self, node: &Node, slot: i32) {
        let (min, span, session) = (node.inputs[2], node.inputs[3], node.inputs[4]);
        let Some(table) = scoped_arena_jit_table(&self.direct_helpers) else {
            self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX: every access takes its full path
            self.store_rax(slot);
            return;
        };
        let mut miss: Vec<usize> = Vec::new();
        self.gp_load_value(R11, session);
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        miss.push(self.emit_jcc_rel32(0x85)); // JNZ miss
        self.gp_load_value(R11, min);
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        miss.push(self.emit_jcc_rel32(0x8E)); // JLE miss
        self.emit_mov_reg_imm64(RAX, 1u64 << 62);
        self.buf.emit(&[0x49, 0x85, 0xC3]); // TEST R11, RAX
        // A raw address: the full path, or in real-pointer mode the sentinel
        // (`scoped_arena_real_pointers`), which `lower_arena_cache_base` maps
        // to `min` itself.
        let raw = self.emit_jcc_rel32(0x84); // JZ miss / raw
        let real = scoped_arena_real_pointers();
        if !real {
            miss.push(raw);
        }
        let index_mask: u32 = (1u32 << SCOPED_ARENA_INDEX_BITS) - 1;
        let offset_mask: u32 = (1u32 << SCOPED_ARENA_SLOT_SHIFT) - 1;
        self.buf.emit(&[0x4D, 0x89, 0xD8]); // MOV R8, R11
        self.buf.emit(&[0x49, 0xC1, 0xE8, 0x1E]); // SHR R8, 30
        self.buf.emit(&[0x41, 0x81, 0xE0]); // AND R8D, imm32
        self.buf.emit(&index_mask.to_le_bytes());
        self.buf.emit(&[0x49, 0xC1, 0xE0, 0x05]); // SHL R8, 5
        self.emit_mov_reg_imm64(RAX, table);
        self.buf.emit(&[0x49, 0x01, 0xC0]); // ADD R8, RAX
        self.buf.emit(&[0x4C, 0x89, 0xD8]); // MOV RAX, R11
        self.buf.emit(&[0x48, 0xC1, 0xE8, 0x1E]); // SHR RAX, 30
        self.buf.emit(&[0x48, 0xC1, 0xE0, 0x1E]); // SHL RAX, 30
        self.buf.emit(&[0x49, 0x3B, 0x00]); // CMP RAX, [R8]  (key)
        miss.push(self.emit_jcc_rel32(0x85)); // JNE miss
        self.buf.emit(&[0x44, 0x89, 0xD9]); // MOV ECX, R11D
        self.buf.emit(&[0x81, 0xE1]); // AND ECX, imm32
        self.buf.emit(&offset_mask.to_le_bytes());
        self.gp_load_value(RDX, span);
        self.buf.emit(&[0x48, 0x01, 0xCA]); // ADD RDX, RCX
        self.buf.emit(&[0x49, 0x3B, 0x50, 0x08]); // CMP RDX, [R8 + 8]  (len)
        miss.push(self.emit_jcc_rel32(0x87)); // JA miss
        self.buf.emit(&[0x4D, 0x89, 0xD9]); // MOV R9, R11
        self.buf.emit(&[0x49, 0xC1, 0xE9, 0x1E]); // SHR R9, 30
        self.buf.emit(&[0x49, 0xC1, 0xE1, 0x1E]); // SHL R9, 30
        self.buf.emit(&[0x4D, 0x3B, 0x08]); // CMP R9, [R8]  (key again)
        miss.push(self.emit_jcc_rel32(0x85)); // JNE miss
        self.emit_mov_reg_reg64(RAX, R8);
        let done = self.emit_jmp_rel32();
        let raw_done = real.then(|| {
            self.patch_rel32_to_here(raw);
            self.buf.emit(&[0xB8]); // MOV EAX, imm32
            self.buf.emit(&ARENA_CACHE_RAW_HIT.to_le_bytes());
            self.emit_jmp_rel32()
        });
        for m in miss {
            self.patch_rel32_to_here(m);
        }
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        self.patch_rel32_to_here(done);
        if let Some(d) = raw_done {
            self.patch_rel32_to_here(d);
        }
        self.store_rax(slot);
        self.note_rcx_written();
    }

    /// JIT round 24 B2: lower [`crate::ir::UnboxOp::ArenaCacheBase`], inputs
    /// `[ctrl, mem, entry, min]`: `0` for a 0 entry, else `[entry + 16] + (min
    /// & (2^30 - 1))` -- the block's pointer plus `min`'s offset in it.
    pub(in crate::ir_lower) fn lower_arena_cache_base(&mut self, node: &Node, slot: i32) {
        let (entry, min) = (node.inputs[2], node.inputs[3]);
        let offset_mask: u32 = (1u32 << SCOPED_ARENA_SLOT_SHIFT) - 1;
        self.gp_load_value(R8, entry);
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8, R8
        let none = self.emit_jcc_rel32(0x84); // JZ done
        self.gp_load_value(R11, min);
        // Real-pointer mode's raw sentinel: `min` is the address.
        let raw = scoped_arena_real_pointers().then(|| {
            self.buf.emit(&[0x49, 0x83, 0xF8, ARENA_CACHE_RAW_HIT as u8]); // CMP R8, imm8
            let slotted = self.emit_jcc_rel32(0x85); // JNE slotted
            self.emit_mov_reg_reg64(RAX, R11);
            let done = self.emit_jmp_rel32();
            self.patch_rel32_to_here(slotted);
            done
        });
        self.buf.emit(&[0x44, 0x89, 0xD9]); // MOV ECX, R11D
        self.buf.emit(&[0x81, 0xE1]); // AND ECX, imm32
        self.buf.emit(&offset_mask.to_le_bytes());
        self.buf.emit(&[0x49, 0x8B, 0x40, 0x10]); // MOV RAX, [R8 + 16]  (ptr)
        self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX
        self.patch_rel32_to_here(none);
        if let Some(done) = raw {
            self.patch_rel32_to_here(done);
        }
        self.store_rax(slot);
        self.note_rcx_written();
    }

    pub(in crate::ir_lower) fn lower_scoped_raw(
        &mut self,
        node: &Node,
        uop: crate::ir::UnboxOp,
        bci: usize,
        slot: i32,
        proven: bool,
    ) {
        use crate::ir::UnboxOp;
        let (width, signed, ordered, is_put) = match uop {
            UnboxOp::ScopedGet {
                width,
                signed,
                ordered,
            } => (width, signed, ordered, false),
            UnboxOp::ScopedPut { width, ordered } => (width, false, ordered, true),
            _ => return,
        };
        // inputs: 2 sma, 3 session, 4 base, 5 offset, then value (put), then bigEndian.
        let put_value = if is_put {
            let Some(&value) = node.inputs.get(6) else {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: a scoped put carries no value input",
                )));
                return;
            };
            Some(value)
        } else {
            None
        };
        let order_input = if ordered {
            Some(node.inputs[if is_put { 7 } else { 6 }])
        } else {
            None
        };

        // Round nio20 wave 3 (lane L4, `r20-heapbb-twelve-x-residual-is-guards-not-calls`):
        // every check below that the graph already answers is left out. Each
        // one only ever removes a deopt, which is the direction the deopt
        // prediction tolerates (see `emit_store_receiver_null_check`).
        let sma = node.inputs[2];
        let session = node.inputs[3];
        let cse = true;

        // The receiver must be non-null (the original call would NPE: exact).
        // `SCOPED_MEMORY_ACCESS` is one `getstatic` per access, so a block that
        // already tested this node (or a node that cannot be null) skips it.
        // JIT r23 wave 8 (lane W8-L4): an FFM expansion's heap access, whose
        // segment-field reads null-tested `sma` ahead of it
        // (`Graph::ffm_proven_heap_accesses`).
        if store_receiver_cannot_be_null(self.graph, sma)
            || (cse && self.receiver_already_non_null(sma))
            || proven
        {
            crate::metrics::note_ir_receiver_null_check_elided();
        } else {
            self.gp_load_value(RAX, sma);
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
            if cse {
                // The deopt leaves on null: the fall-through proves `sma`.
                self.note_receiver_non_null(sma);
            }
        }
        // A memory session is a segment/arena whose liveness the real method
        // checks. A spliced `Buffer.session()` of a heap buffer whose cold
        // `segment != null` arm was masked folds to the literal null, which
        // needs no test.
        let session_is_null = self
            .graph
            .nodes
            .get(session as usize)
            .is_some_and(crate::ir::is_null_const)
            // JIT r23 wave 9 (lane L4, `CRATONVM_JIT_IR_FFM_HEAP_SESSION_HOIST`):
            // or a heap segment's scope a dominating loop-entry guard proved a
            // `HeapSession`, which never closes
            // (`Graph::ffm_never_closing_scopes`): no check either.
            || self.graph.ffm_never_closing_scopes.contains(&session);
        // JIT r20 wave 19 (lane yFfmShared, MEM20-2): the edges of a SHARED
        // model session's check, lowered through the windowed tail below.
        let mut shared_edges: Vec<usize> = Vec::new();
        let mut shared_window: Option<ScopedSharedWindow> = None;
        if !session_is_null {
            self.gp_load_value(RDX, session);
            self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
                                                // nio20 wave 4, lane L2 (phase A of
                                                // `r20-scoped-fold-deopts-on-a-non-null-session-20261007`): a
                                                // session whose EXACT class can never fail `checkValidStateRaw`
                                                // passes inline too; see `scoped_session_never_closing_class_ids`.
            let admitted = scoped_session_never_closing_class_ids(&self.direct_helpers);
            // nio20 wave 5, lane L2: and a CONFINED session (the JDK's
            // `ConfinedSession`, or the model with an owner) whose owner is
            // this thread and whose state reads open; see
            // `scoped_session_confined_rows`.
            let (confined, thread_off) = scoped_session_confined_rows(&self.direct_helpers);
            // JIT r20 wave 19 (lane yFfmShared, MEM20-2): and a SHARED model
            // session, under this thread's access window; see
            // `Self::scoped_shared_window_for`.
            shared_window = self.scoped_shared_window_for(node, &confined, put_value, order_input);
            // Every class compare's target, in emission order: `None` = the
            // never-closing pass, `Some(i)` = confined row `i`'s check.
            let candidates: Vec<(u32, Option<usize>)> = admitted
                .iter()
                .map(|&id| (id, None))
                .chain(
                    confined
                        .iter()
                        .enumerate()
                        .map(|(i, r)| (r.class_id, Some(i))),
                )
                .collect();
            if let Some((&(last_id, last_target), rest)) = candidates.split_last() {
                let mut pass: Vec<usize> = Vec::with_capacity(candidates.len() + 1);
                let mut to_row: Vec<(usize, usize)> = Vec::with_capacity(confined.len());
                pass.push(self.emit_jcc_rel32(0x84)); // JZ: session == null
                                                      // MOV EAX, [RDX] -- the session's class id (`ObjectHeader`
                                                      // offset 0); the parameter is typed `MemorySessionImpl`, so
                                                      // it is a plain instance, never an array.
                self.buf.emit(&[0x8B, 0x02]);
                for &(id, target) in rest {
                    self.buf.emit_byte(0x3D); // CMP EAX, imm32
                    self.buf.emit(&id.to_le_bytes());
                    let je = self.emit_jcc_rel32(0x84); // JE: admitted
                    match target {
                        None => pass.push(je),
                        Some(i) => to_row.push((je, i)),
                    }
                }
                self.buf.emit_byte(0x3D); // CMP EAX, imm32
                self.buf.emit(&last_id.to_le_bytes());
                self.emit_deopt_unless(0x84, bci, DeoptReason::ReceiverTypeChanged); // JE
                let mut rows_in_order: Vec<usize> = Vec::with_capacity(confined.len());
                // The last candidate falls through into its own target.
                match last_target {
                    None => {
                        if !confined.is_empty() {
                            pass.push(self.emit_jmp_rel32());
                        }
                    }
                    Some(i) => rows_in_order.push(i),
                }
                for i in 0..confined.len() {
                    if !rows_in_order.contains(&i) {
                        rows_in_order.push(i);
                    }
                }
                for (k, &i) in rows_in_order.iter().enumerate() {
                    // The fall-through row (k == 0 when the last compare was
                    // a row) needs no label; every other row's entries land here.
                    for &(je, _) in to_row.iter().filter(|&&(_, r)| r == i) {
                        self.patch_rel32_to_here(je);
                    }
                    let shared_out = match shared_window {
                        Some(win) if win.class_id == confined[i].class_id => {
                            Some(&mut shared_edges)
                        }
                        _ => None,
                    };
                    self.emit_scoped_confined_session_check(
                        confined[i],
                        thread_off,
                        bci,
                        shared_out,
                    );
                    if k + 1 < rows_in_order.len() {
                        pass.push(self.emit_jmp_rel32());
                    }
                }
                for p in pass {
                    self.patch_rel32_to_here(p);
                }
                if crate::x64::dbg_jitc_enabled() {
                    let confined_ids: Vec<u32> = confined.iter().map(|r| r.class_id).collect();
                    eprintln!(
                        "[cratonvm-jitc] scoped-session-fold (ir) bci={bci} admitted={admitted:?} \
                         confined={confined_ids:?} thread_off={thread_off}"
                    );
                    if let Some(win) = shared_window.filter(|_| !shared_edges.is_empty()) {
                        eprintln!(
                            "[cratonvm-jitc] scoped-shared-window (ir) bci={bci} \
                             word_disp={:#x} in={}",
                            win.disp, self.graph.scope_method_key
                        );
                    }
                }
            } else {
                self.emit_deopt_unless(0x84, bci, DeoptReason::ReceiverTypeChanged);
                // JZ: session == null
            }
        }

        // JIT r20 wave 19 (lane yFfmShared, MEM20-2): the tail twice when a
        // shared edge exists -- once as before (every other session, which
        // falls through to here), once under this thread's access window for
        // the shared edges. The second copy starts from the screened-base set
        // the first one started from: the two are disjoint paths that join
        // after the access, and both screen the same base.
        let shared_window = shared_window.filter(|_| !shared_edges.is_empty());
        let screened_before = shared_window.map(|_| self.scoped_screened_bases.clone());
        self.emit_scoped_raw_tail(
            node,
            width,
            signed,
            put_value,
            order_input,
            bci,
            slot,
            None,
            proven,
        );
        if let (Some(win), Some(screened)) = (shared_window, screened_before) {
            let done = self.emit_jmp_rel32();
            for edge in shared_edges {
                self.patch_rel32_to_here(edge);
            }
            self.scoped_screened_bases = screened;
            self.emit_scoped_raw_tail(
                node,
                width,
                signed,
                put_value,
                order_input,
                bci,
                slot,
                Some(win),
                proven,
            );
            self.patch_rel32_to_here(done);
        }
    }

}

fn implicit_kind_matches_catch(kind: ImplicitKind, name: &str) -> bool {
    match name {
        "" => true,
        "java/lang/RuntimeException" | "java/lang/Exception" | "java/lang/Throwable" => true,
        "java/lang/ArrayIndexOutOfBoundsException" | "java/lang/IndexOutOfBoundsException" => {
            kind == ImplicitKind::Aioobe
        }
        "java/lang/NullPointerException" => kind == ImplicitKind::Npe,
        "java/lang/ArithmeticException" => kind == ImplicitKind::Arithmetic,
        _ => false,
    }
}
