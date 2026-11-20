#[allow(unused_imports)]
use super::*;

impl Compiler {
    /// Lower `invokevirtual`, `invokespecial` and `invokeinterface` at `pc`.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::x64::op_invoke) fn walk_invoke_instance(
        &mut self,
        _code: &[u8],
        _code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        _branch_targets: &[bool],
    ) -> WalkStep {
        match op {
            // invokevirtual / invokespecial / invokeinterface — direct call or dispatch helper
            0xb6 | 0xb7 | 0xb9 => {
                // Scalar replacement: skip <init>()V on scalar-replaced objects
                if op == 0xb7 && self.scalar_init_skips.contains(&pc) {
                    let _ = self.pop_stack(); // discard dup'd receiver
                    pc += 3;
                    return WalkStep::Next(pc);
                }
                // Elide `invokespecial java/lang/Object.<init>()V` — the
                // terminal of every constructor chain. The method body is
                // a bare `return` and the VM-side registration is
                // `native_noop_with_this`, so the call has no observable
                // effect. The site can never become a direct call or an
                // inline site (`<init>` + native-shadow compile gates), so
                // without this it falls to `jit_invoke_dispatch`'s
                // interpreter slow path once per object allocation —
                // dominant on allocation-heavy code (bintrees18: ~69M
                // dispatches of an empty method).
                if op == 0xb7 {
                    if let Some(&idx) = self.invoke_info_idx.get(&pc) {
                        // SAFETY: invoke_info pointers are owned by the
                        // enclosing try_compile scope and outlive codegen.
                        let info = unsafe { &*self.invoke_info[idx].1 };
                        if info.invoke_kind == 1
                            && info.descriptor == "()V"
                            && info.method_name == "<init>"
                            && info.class_name == "java/lang/Object"
                        {
                            let _ = self.pop_stack(); // discard receiver
                            pc += 3;
                            return WalkStep::Next(pc);
                        }
                    }
                }
                self.flush_scratch_registers();

                // Round 9 wave 10 (`call10`,
                // `perf-private-instance-self-recursion-dispatches-every-call-FIXED-20260918.md`):
                // a statically bound INSTANCE self-call, planned as a
                // `JIT_SELF_CALL_GUARD` marker row (and no `JitInvokeInfo`) by
                // `lib.rs`. JVMS 6.5 first: a null receiver raises NPE AT THE
                // INVOKE -- the same precise check a spliced `invokespecial`
                // body gets, emitted while the receiver is still on the
                // operand stack. Then the site is exactly a static self-call
                // whose first argument is the receiver (this method's own
                // entry ABI: context, `this`, parameters), so it is lowered by
                // `walk_invokestatic`'s self-recursive arm: stack guard, raw
                // `CALL` to offset 0, sentinel check, own return type.
                let self_call_row = self.direct_calls_idx.get(&pc).is_some_and(|&i| {
                    self.direct_calls[i].1.guard_class_id == crate::JIT_SELF_CALL_GUARD
                });
                if self_call_row {
                    let n = self.num_params;
                    if op == 0xb9
                        || n == 0
                        || self.stack.len() < n
                        || self.invoke_info_idx.contains_key(&pc)
                        || self.inline_sites.contains_key(&pc)
                    {
                        // A planner/emitter disagreement: the static arm would
                        // take an info or an inline site instead of the self
                        // call, or pop the wrong operands.
                        self.fail("singlepass-codegen/instance-self-call-shape");
                        return WalkStep::Return(false);
                    }
                    self.emit_spliced_receiver_null_check(false, false, n);
                    return self.walk_invokestatic(
                        _code,
                        _code_len,
                        0xb8,
                        pc,
                        _dead,
                        _branch_targets,
                    );
                }

                // Round 13 wave 1 (lane `sync`, page
                // `r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow`,
                // option 1): a caller-held synchronized INSTANCE direct CALL
                // (`crate::JIT_SYNC_DIRECT_INSTANCE_TAG` row, planned by
                // `build_single_pass_tables`). Lowered by `walk_invokestatic`'s
                // caller-held arm, which takes the RECEIVER's monitor around a
                // CALL of the monitor-free body, exactly as the instance
                // self-call above borrows its self-recursive arm. First the
                // receiver:
                //
                // * statically bound (exact class `0`): JVMS 6.5, a null
                //   receiver raises NPE AT THE INVOKE, before any monitor action,
                //   while the arguments are still on the operand stack;
                // * guarded: `null` or any class but the exact one misses to the
                //   ordinary dispatch below (the site's `JitInvokeInfo`, which
                //   throws the NPE or selects the override), and the hit joins it
                //   after, as a guarded inline's hit does
                //   (`reconcile_guarded_inline_join`).
                //
                // A site `sync_direct_instance_site` refuses keeps that dispatch
                // alone (the row is dropped from `direct` below).
                let mut sync_instance_hit: Option<(usize, Vec<StackSlot>)> = None;
                if matches!(op, 0xb6 | 0xb7) {
                    if let Some((n, exact_class_id)) = self.sync_direct_instance_site(pc) {
                        if exact_class_id == 0 {
                            self.emit_spliced_receiver_null_check(false, false, n);
                            return self.walk_invokestatic(
                                _code,
                                _code_len,
                                0xb8,
                                pc,
                                _dead,
                                _branch_targets,
                            );
                        }
                        let stack_checkpoint = self.stack.clone();
                        let oop_marks_checkpoint = self.stack_oop_marks.clone();
                        let marks_exact_checkpoint = self.stack_oop_marks_exact;
                        let scratch_xmm_checkpoint = self.scratch_xmm_in_use;
                        let spill_checkpoint = self.next_spill_offset;
                        let recv_slot = self.stack[self.stack.len() - n];
                        self.load_slot_to_reg(RAX, recv_slot);
                        self.emit_test_r64_r64(RAX);
                        let null_miss = self.emit_jcc_rel32_patch(0x84); // JZ
                                                                         // CMP DWORD [RAX+0], exact_class_id (the guarded
                                                                         // inline's encoding: 81 /7 id, ModRM 0x78 disp8 0).
                        self.buf.emit(&[0x81, 0x78, 0x00]);
                        self.buf.emit(&exact_class_id.to_le_bytes());
                        let class_miss = self.emit_jcc_rel32_patch(0x85); // JNE
                        match self.walk_invokestatic(
                            _code,
                            _code_len,
                            0xb8,
                            pc,
                            _dead,
                            _branch_targets,
                        ) {
                            WalkStep::Next(_) => {}
                            refused => return refused,
                        }
                        let hit_stack = self.stack.clone();
                        sync_instance_hit = Some((self.emit_jmp_rel32_patch(), hit_stack));
                        // The miss edge runs the dispatch below from the model
                        // the guard saw, whatever the hit path did to it.
                        self.stack = stack_checkpoint;
                        self.stack_oop_marks = oop_marks_checkpoint;
                        self.stack_oop_marks_exact = marks_exact_checkpoint;
                        self.scratch_xmm_in_use = scratch_xmm_checkpoint;
                        self.next_spill_offset = spill_checkpoint;
                        self.patch_rel32_to_here(null_miss);
                        self.patch_rel32_to_here(class_miss);
                        self.slot_mirror = None;
                    }
                }

                // Check for inline site (invokespecial only — virtual/interface not eligible)
                if op == 0xb7 && self.inline_sites.contains_key(&pc) {
                    // JVMS 6.5: a null `objectref` raises NPE AT THE INVOKE.
                    // The direct-call arm below learned this the hard way
                    // (`return 3;` / `return helper();` callees returned
                    // normally on a null receiver — see its comment); a
                    // SPLICED body is the same body with the CALL removed,
                    // and had no check at all. Emitted before the splice and
                    // kept if the splice rolls back: the fall-through arm's
                    // own check is then redundant, never wrong.
                    if let Some(site) = self.inline_sites.get(&pc) {
                        let (is_static, is_init, recv_depth) = (
                            site.callee_is_static,
                            site.method_name == "<init>",
                            site.callee_num_args,
                        );
                        self.emit_spliced_receiver_null_check(is_static, is_init, recv_depth);
                    }
                    if self.try_emit_inline(pc) {
                        pc += 3;
                        return WalkStep::Next(pc);
                    }
                }

                // PGO-02 (docs/feature-designs/profile-guided-inlining.md):
                // guarded MONOMORPHIC virtual/interface inline. `inline_sites`
                // + `inline_guard_variants` are populated TOGETHER, for an
                // admitted plan whose verdict is `InlineVerdict::Monomorphic`,
                // `Bimorphic`, or — since CHA landed (`NOTES-deopt2.md` M5) —
                // `UniqueConcreteMethod`. The third is lowered by exactly this
                // code and deliberately has no arm of its own: its guard is the
                // same exact class-id compare with the same
                // fall-through-to-dispatch miss edge, and only the EVIDENCE
                // behind the class id differs (the loaded hierarchy rather than
                // the receiver profile), which is a planner-side distinction
                // with no lowering consequence. `InlinePlan::is_speculative` is
                // the predicate that decides what lands in the map; keep it and
                // this comment in step. All three require
                // a `class_id_name_resolver` (see `InlineBackendCaps`
                // in jit/src/lib.rs). The VM threads one in unless
                // `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=0` — **default ON since
                // N8**, where it was default OFF before, so this block used to
                // cost one HashMap probe over an always-empty map and now
                // carries the tree's only devirtualization. Splices the callee
                // body via
                // the SAME `try_emit_inline` the invokespecial check above
                // already uses, behind a receiver class-id guard; the miss
                // edge falls through UNCHANGED to the normal dispatch code
                // below (MIC/PIC/`jit_invoke_dispatch`) — never a deopt (see
                // the design doc's §3 deopt-safety argument: no caller
                // scopes are populated, so this relies on — and does not
                // change — the existing guarantee that nothing inside an
                // inlined body publishes a deopt point).
                // Each hit path's `JMP done`, with the operand-stack model it
                // left behind. The join below reconciles that model with the
                // dispatch path's: see `reconcile_guarded_inline_join`.
                let mut guarded_virtual_done_patches: Vec<(usize, Vec<StackSlot>)> = Vec::new();
                // The synchronized instance twin's hit edge (round 13 wave 1)
                // joins the dispatch below exactly as a guarded splice's does.
                let sync_instance_joins = sync_instance_hit.is_some();
                guarded_virtual_done_patches.extend(sync_instance_hit.take());
                let skip_guarded_splice_for_method_invoke =
                    self.direct_helpers.reflect_method_invoke_cache != 0
                        && self.invoke_info_idx.get(&pc).map_or(false, |&i| {
                            // SAFETY: invoke-info entries are owned for the duration of this compile.
                            let info_ref = unsafe { &*self.invoke_info[i].1 };
                            info_ref.class_name == "java/lang/reflect/Method"
                                && info_ref.method_name == "invoke"
                                && info_ref.descriptor
                                    == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
                        });
                if op != 0xb7 && !skip_guarded_splice_for_method_invoke {
                    if let Some(variants) = self.inline_guard_variants.get(&pc).cloned() {
                        // Every variant is the SAME call site, so every
                        // body pops the same operand shape. A disagreement
                        // means the two were resolved from different
                        // descriptors, which this lowering has no model
                        // for — refuse the whole site rather than emit two
                        // guards over two different stack effects.
                        let recv_depth = variants
                            .first()
                            .map(|(_, site)| site.callee_num_args)
                            .unwrap_or(0);
                        let uniform = !variants.is_empty()
                            && variants
                                .iter()
                                .all(|(_, site)| site.callee_num_args == recv_depth);
                        // A planned, budget-charged guarded inline that fails
                        // this test evaporates with no code emitted for it. It
                        // used to do so with no `else`, no counter and no
                        // diagnostic line, so the site still reported as
                        // inlined; see the refusal at the bottom of this block
                        // for why that matters.
                        if !(uniform && recv_depth >= 1 && self.stack.len() >= recv_depth) {
                            crate::metrics::note_inline_call_arm(4);
                            if false {
                                eprintln!(
                                    "[cratonvm-jitc] guarded-virtual refused at pc={pc}: \
                                     uniform={uniform} recv_depth={recv_depth} \
                                     stack_depth={} variants={}",
                                    self.stack.len(),
                                    variants.len(),
                                );
                            }
                        }
                        if uniform && recv_depth >= 1 && self.stack.len() >= recv_depth {
                            let recv_slot = self.stack[self.stack.len() - recv_depth];

                            // Full state snapshot from BEFORE any guard byte
                            // is emitted — mirrors try_emit_inline's own
                            // checkpoint set exactly (see its comment on the
                            // groovyjarjarasm-asm-handler-getexceptiontablesize
                            // fix for why every one of these fields matters),
                            // so a rewind on either failure path below is
                            // indistinguishable from never having attempted
                            // this guard at all.
                            let buf_checkpoint = self.buf.mark();
                            let stack_checkpoint = self.stack.clone();
                            let oop_marks_checkpoint = self.stack_oop_marks.clone();
                            // The scratch-XMM mask rolls back with the stack
                            // (see `SpliceCheckpoint`): a hit body can leave
                            // bits set that no restored `StackSlot::Xmm` owns.
                            let scratch_xmm_checkpoint = self.scratch_xmm_in_use;
                            let spill_checkpoint = self.next_spill_offset;
                            let exception_check_stubs_checkpoint = self.exception_check_stubs.len();
                            let deopt_stubs_checkpoint = self.deopt_stubs.len();
                            let forward_patches_checkpoint = self.forward_patches.len();
                            let jump_table_patches_checkpoint = self.jump_table_patches.len();
                            let self_call_patches_checkpoint = self.self_call_patches.len();
                            let bounds_check_stubs_checkpoint = self.bounds_check_stubs.len();
                            let null_check_store_stubs_checkpoint =
                                self.null_check_store_stubs.len();
                            let late_patch_checkpoint = self.late_patch_checkpoint();

                            // The receiver is loaded and null-checked ONCE,
                            // ahead of the guard chain: `null` fails every
                            // guard, and re-testing it per variant would be
                            // pure code size. Peeked, not popped —
                            // try_emit_inline_site does its own popping on
                            // each hit path, and the miss tail needs the
                            // receiver+args untouched for the normal-dispatch
                            // code that runs next.
                            self.load_slot_to_reg(RAX, recv_slot);
                            self.emit_test_r64_r64(RAX);
                            let null_miss_patch = self.emit_jcc_rel32_patch(0x84); // JZ

                            // ARRAY-RECEIVER GUARD — the one the MIC/PIC
                            // cascades and the hashed stub already carry, and
                            // this chain did not. A reference array's header
                            // holds its COMPONENT's class id at offset 0, so
                            // a `Foo[]` receiver passes `CMP [RAX], Foo` and
                            // would run `Foo`'s spliced body on an array.
                            // Only a site whose static receiver type an array
                            // can inhabit needs it — `Object` itself and the
                            // two interfaces every array implements; the
                            // verifier keeps arrays away from any other
                            // receiver type — so every other guarded site
                            // stays byte-identical.
                            let recv_may_be_array = self
                                .invoke_info_idx
                                .get(&pc)
                                .map(|&i| self.invoke_info[i].1)
                                .map_or(true, |ip| {
                                    // SAFETY: `invoke_info` pointers are owned
                                    // by the enclosing compile and outlive
                                    // codegen (same as every other deref of
                                    // them in this file).
                                    let info = unsafe { &*ip };
                                    // An INTERFACE owner proves nothing: the
                                    // verifier treats interface types as
                                    // `Object` (JVMS 4.10.1.2), so a `C[]` can
                                    // reach an `invokeinterface I.m` whose
                                    // guard names an implementor `C`, and must
                                    // miss to the dispatch that throws ICCE.
                                    op == 0xb9
                                        || crate::x64::inlining::static_receiver_admits_arrays(
                                            info.class_name,
                                        )
                                });
                            let kind_miss_patch = if recv_may_be_array {
                                //   TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK (F6 /0 ib: kind bits only)
                                self.buf.emit(&[
                                    0xF6,
                                    0x40,
                                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                                    cratonvm_types::KIND_TAG_BYTE_MASK,
                                ]);
                                Some(self.emit_jcc_rel32_patch(0x85)) // JNE
                            } else {
                                None
                            };

                            // The guard chain. Variant k's mismatch edge
                            // lands at variant k+1's `CMP`; the last one's
                            // lands at the normal-dispatch code below. A hit
                            // jumps PAST that code entirely.
                            let mut pending_miss: Option<usize> = None;
                            let mut spliced_any = false;
                            for (guard_class_id, site) in &variants {
                                // Per-variant checkpoint: if THIS body cannot
                                // be spliced, only this variant's bytes are
                                // rewound — the ones already emitted for
                                // earlier variants stay.
                                let variant_buf_checkpoint = self.buf.mark();
                                let variant_exception_stubs = self.exception_check_stubs.len();
                                let variant_deopt_stubs = self.deopt_stubs.len();
                                let variant_forward_patches = self.forward_patches.len();
                                let variant_jump_table_patches = self.jump_table_patches.len();
                                let variant_self_call_patches = self.self_call_patches.len();
                                let variant_bounds_stubs = self.bounds_check_stubs.len();
                                let variant_null_store_stubs = self.null_check_store_stubs.len();
                                let variant_late_patches = self.late_patch_checkpoint();

                                // Land the previous variant's mismatch edge
                                // exactly here. If this variant then fails and
                                // rewinds, the same offset becomes the start of
                                // whatever is emitted next — the following
                                // variant's `CMP`, or the normal-dispatch code
                                // — which is the correct landing spot either
                                // way.
                                if let Some(prev) = pending_miss.take() {
                                    self.patch_rel32_to_here(prev);
                                }

                                // CMP DWORD [RAX+0], guard_class_id —
                                // identical encoding to the String/CRC32
                                // intrinsic guard above (81 /7 id, ModRM 0x78
                                // = mod00 /7 rm=RAX).
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                let this_miss = self.emit_jcc_rel32_patch(0x85); // JNE

                                if self.try_emit_inline_site(pc, site) {
                                    // Hit: skip every later guard AND the
                                    // normal-dispatch bytes entirely.
                                    let hit_stack = self.stack.clone();
                                    guarded_virtual_done_patches
                                        .push((self.emit_jmp_rel32_patch(), hit_stack));
                                    spliced_any = true;
                                    pending_miss = Some(this_miss);
                                    // The inline body consumed the
                                    // receiver+args and pushed its result via
                                    // the same push_from_rax /
                                    // push_from_rax_as_xmm0 convention the
                                    // normal-dispatch code below also uses.
                                    // Restore the compiler's SYMBOLIC state
                                    // (not the already-emitted bytes) to
                                    // exactly what it was before the guard
                                    // chain, so the NEXT variant sees the same
                                    // operand stack this one did, and so the
                                    // dispatch code — the only Rust-level
                                    // continuation from here, run
                                    // unconditionally — pops the SAME
                                    // receiver+args positions and pushes a
                                    // canonically-shaped result regardless of
                                    // which machine-code path a given
                                    // execution actually takes at runtime.
                                    self.stack = stack_checkpoint.clone();
                                    self.stack_oop_marks = oop_marks_checkpoint.clone();
                                    self.scratch_xmm_in_use = scratch_xmm_checkpoint;
                                    self.next_spill_offset = spill_checkpoint;
                                } else {
                                    // try_emit_inline_site already rolled back
                                    // its OWN side effects; rewind this
                                    // variant's guard bytes too, so the site is
                                    // byte-identical to never having offered
                                    // this variant.
                                    self.buf.rewind_to_mark(variant_buf_checkpoint);
                                    self.stack = stack_checkpoint.clone();
                                    self.stack_oop_marks = oop_marks_checkpoint.clone();
                                    self.scratch_xmm_in_use = scratch_xmm_checkpoint;
                                    self.next_spill_offset = spill_checkpoint;
                                    self.exception_check_stubs.truncate(variant_exception_stubs);
                                    self.deopt_stubs.truncate(variant_deopt_stubs);
                                    self.forward_patches.truncate(variant_forward_patches);
                                    self.jump_table_patches.truncate(variant_jump_table_patches);
                                    self.self_call_patches.truncate(variant_self_call_patches);
                                    self.bounds_check_stubs.truncate(variant_bounds_stubs);
                                    self.null_check_store_stubs
                                        .truncate(variant_null_store_stubs);
                                    self.restore_late_patches(variant_late_patches);
                                }
                            }

                            if spliced_any {
                                // Every remaining miss edge — the null check
                                // and the last guard — lands at the exact
                                // start of the UNCHANGED normal-dispatch code
                                // that runs next.
                                self.patch_rel32_to_here(null_miss_patch);
                                if let Some(kind_miss) = kind_miss_patch {
                                    self.patch_rel32_to_here(kind_miss);
                                }
                                if let Some(last) = pending_miss {
                                    self.patch_rel32_to_here(last);
                                }
                            } else {
                                // No variant could be spliced: rewind the
                                // shared receiver load and null check too, so
                                // the fall-through is byte-identical to never
                                // having attempted a guard.
                                //
                                // Named, not just rewound. The planner has
                                // already charged `plan.expansion_cost` against
                                // `inline_budget_remaining` and recorded the
                                // invalidation dependencies, so without this
                                // the site reports as inlined in
                                // `InlineDecisionTally` while emitting a plain
                                // dispatch — "did not help" and "could not be
                                // emitted" become the same number. Every other
                                // refusal in this area is named; these two were
                                // not.
                                crate::metrics::note_inline_call_arm(4);
                                if false {
                                    eprintln!(
                                        "[cratonvm-jitc] guarded-virtual refused at pc={pc}: \
                                         no variant of {} could be spliced",
                                        variants.len(),
                                    );
                                }
                                self.buf.rewind_to_mark(buf_checkpoint);
                                self.stack = stack_checkpoint;
                                self.stack_oop_marks = oop_marks_checkpoint;
                                self.scratch_xmm_in_use = scratch_xmm_checkpoint;
                                self.next_spill_offset = spill_checkpoint;
                                self.exception_check_stubs
                                    .truncate(exception_check_stubs_checkpoint);
                                self.deopt_stubs.truncate(deopt_stubs_checkpoint);
                                self.forward_patches.truncate(forward_patches_checkpoint);
                                self.jump_table_patches
                                    .truncate(jump_table_patches_checkpoint);
                                self.self_call_patches
                                    .truncate(self_call_patches_checkpoint);
                                self.bounds_check_stubs
                                    .truncate(bounds_check_stubs_checkpoint);
                                self.null_check_store_stubs
                                    .truncate(null_check_store_stubs_checkpoint);
                                self.restore_late_patches(late_patch_checkpoint);
                            }
                        }
                    }
                }

                // The direct exceptional-return service needs the same
                // invoke metadata as the normal dispatch fallback.
                let info_ptr = self
                    .invoke_info_idx
                    .get(&pc)
                    .map(|&i| self.invoke_info[i].1);
                // Check for direct call target (invokespecial with compiled callee)
                // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                let direct = self.direct_calls_idx.get(&pc).map(|&i| {
                    let dc = &self.direct_calls[i].1;
                    note_emit_direct(&self.method_key, pc, dc.entry);
                    (
                        dc.entry,
                        dc.needs_context,
                        dc.num_params,
                        dc.return_type,
                        dc.guard_class_id,
                    )
                });
                // Round 13 wave 1 (lane `sync`): an instance caller-held row is
                // lowered by the arm above or not at all. What reaches here --
                // its guard's miss edge, or a site that arm refused -- is the
                // ordinary dispatch through the site's `JitInvokeInfo`, never a
                // CALL of the row's monitor-free body.
                let direct =
                    direct.filter(|d| crate::sync_direct_instance_guard_class(d.4).is_none());
                // A cell row (`JIT_CYCLE_EDGE_CELL_GUARD` / `JIT_RETIRE_CELL_GUARD`)
                // carries a cell ADDRESS in `entry`. The planners place them on
                // `invokestatic` only; one reaching an instance site would be
                // CALLed as code, so refuse the method (round 9 wave 9, `evict9`).
                if direct.is_some_and(|d| {
                    d.4 == crate::JIT_CYCLE_EDGE_CELL_GUARD || d.4 == crate::JIT_RETIRE_CELL_GUARD
                }) {
                    self.fail("singlepass-codegen/cell-row-on-an-instance-site");
                    return WalkStep::Return(false);
                }
                // Round 11 wave 14 (lane `spsync`): a caller-held synchronized
                // row names a MONITOR-FREE body, and only `walk_invokestatic`
                // takes the monitor around it. The plain arm below reads no
                // guard word, so it would CALL that body unlocked. No planner
                // puts one here (both require `0xb8` and a static target);
                // refuse rather than trust that.
                if direct.is_some_and(|d| d.4 == crate::JIT_SYNC_DIRECT_GUARD) {
                    self.fail("singlepass-codegen/sync-direct-row-on-an-instance-site");
                    return WalkStep::Return(false);
                }

                // Receiver-type speculation consults the per-bci de-spec
                // registry. A call-site intrinsic with `guard_class_id != 0`
                // (the `java/lang/CharSequence` String family, the CRC32
                // family, the atomic-field family) is a SPECULATION: the
                // inline body it emits is valid for exactly that one
                // receiver class and every other receiver takes the
                // `ReceiverTypeChanged` deopt edge. On a site whose receiver
                // is NEVER that class the guard fails on every single call,
                // so the method deopts per invocation, is recompiled with
                // the identical guard, and is finally barred from
                // compilation altogether by `recommend_action`'s
                // `MakeNotCompilable` escalation -- for a speculation the
                // compiler chose, about a program that never satisfied it.
                // netty's `HttpHeaderValidationUtil.validateValidHeaderValue`
                // is that shape: its `CharSequence.length()` at bci 1 is
                // reached only with `AsciiString` and an anonymous
                // `CharSequence`, and the class measured 1.67 deopts per
                // loop iteration (docs/feature-designs/native-dispatch-architecture.md).
                //
                // `real_frame_deopt_resume_and_despeculate` already records
                // such a bci in the de-spec registry after
                // `PER_BCI_DESPEC_LIMIT` deopts and prints "speculation
                // suppressed on next compile" -- but until now nothing on
                // the receiver-guard path READ that registry (only the two
                // loop-hoist gates in `driver.rs` and the
                // `ArraycopyPrimitive` intrinsic below did), so the claim
                // was false and the next compile emitted the same guard.
                // The consult belongs at the RESOLVER, not here, and this
                // block counts rather than declines. Measured 2026-08-24,
                // and it cost most of a session: declining a registered
                // intrinsic in THIS filter does not send the site to the
                // `else` arm's MIC/PIC dispatch, because that arm needs
                // `invoke_info` at this pc and there is none. A site the
                // resolver registered as an intrinsic took
                // `direct_calls.push(..); continue;` in `try_compile_inner`
                // BEFORE the `invoke_info.push` below it, so the dispatch
                // metadata was never built. With both `direct` and
                // `info_ptr` `None` the emitter falls through to the
                // unconditional `UnreachedCode` trap at the bottom of this
                // arm -- so the "declined" site deopts on EVERY execution
                // instead of dispatching. The de-spec consult read as inert
                // (deopts 3502 -> 3055 on `HeaderValidationLoopRate`) while
                // its own `sites-declined` counter said it had fired 51
                // times; only correlating the decline trace against the
                // deopt stream separated the two -- 33 declines at
                // `oldHeaderValueValidationAlgorithm pc=6` and 1466 deopts
                // at that same bci AFTERWARDS.
                //
                // The pre-existing `ArraycopyPrimitive` and
                // `StringIndexOfChar` filters in the invokestatic ladder
                // above have the identical shape and therefore the identical
                // defect; they are left alone here because each needs its
                // own A/B, and are named on the known-issue page.
                // Round 15 wave 6 (lane vh8; VH7-1): a JUDGED thin `VarHandle`
                // row's `entry` is its `VarHandleJudgedSite`, not code. From
                // here on it is the unjudged row it replaces (the slot's
                // helper, no guard), except that `ARG_REGS[0]` carries the
                // tagged record instead of the context (see the CALL below).
                // A row that cannot be one refuses the method rather than CALL
                // a record.
                let mut vh_judged_site: usize = 0;
                let direct = match direct {
                    Some((record, true, params, ret, crate::JIT_VARHANDLE_JUDGED_GUARD)) => {
                        // SAFETY: a judged row's `entry` is a record the door
                        // owns for the whole compile, like every
                        // `JitInvokeInfo` pointer in these tables.
                        match unsafe { crate::varhandle_judged_site_helper(record) } {
                            Some(helper) => {
                                vh_judged_site = record;
                                Some((helper, true, params, ret, 0))
                            }
                            None => {
                                self.fail("singlepass-codegen/vh-judged-row-without-helper");
                                return WalkStep::Return(false);
                            }
                        }
                    }
                    Some((_, false, _, _, crate::JIT_VARHANDLE_JUDGED_GUARD)) => {
                        self.fail("singlepass-codegen/vh-judged-row-without-context");
                        return WalkStep::Return(false);
                    }
                    other => other,
                };
                let direct = direct.filter(|&(_, _, _, _, guard_class_id)| {
                    if guard_class_id == 0 {
                        crate::metrics::note_receiver_despec(
                            crate::metrics::RECEIVER_DESPEC_UNGUARDED,
                        );
                        return true;
                    }
                    crate::metrics::note_receiver_despec(
                        crate::metrics::RECEIVER_DESPEC_GUARD_EMITTED,
                    );
                    true
                });

                if let Some((
                    callee_entry,
                    callee_needs_ctx,
                    callee_params,
                    ret_type,
                    guard_class_id,
                )) = direct
                {
                    // --- invokevirtual/special/interface intrinsic ladder ---
                    // Instance-method call-site intrinsics (String / CRC32)
                    // are dispatched here BEFORE the plain direct-call
                    // handling below. Unlike the invokestatic ladder,
                    // instance intrinsics treat the deepest stack operand
                    // as the receiver (`this`): the JLS argument count is
                    // `callee_params`, and total operands popped is
                    // `callee_params + 1`.
                    //
                    // A follow-up agent for family <TAG> fills exactly one
                    // region with `if callee_entry ==
                    // crate::JitIntrinsic::Foo.as_entry() {
                    //     <emit inline code>; <handled = true>; }`.
                    // When every region is empty `intrinsic_handled`
                    // stays false and control falls through to the
                    // unchanged plain direct-call path.
                    #[allow(unused_mut)]
                    let mut intrinsic_handled = false;

                    // ===== INTRINSIC REGION BEGIN: FFM_SEGMENT =====
                    // `MemorySegment.getAtIndex`/`setAtIndex`, lowered to a
                    // CALL of the FFM element fast-path helper with a
                    // DECLINE edge that runs the site's ordinary native
                    // dispatch.
                    //
                    // Through that ordinary dispatch these measure ~1158
                    // ns/element against ~0.8 ns for a `short[]` element,
                    // and they are per-ELEMENT: any segment-backed array
                    // drives one per element.
                    //
                    // Not inline machine code, deliberately. The carrier's
                    // liveness model spans two synthetic classes owned by
                    // two different files, and a second copy of one of its
                    // slot indices has already made that check silently
                    // DEAD once (the W7-89 note on `PE_ARENA_CLASS`). The
                    // helper asks the NATIVE for a verdict instead of
                    // re-deriving one — see
                    // `cratonvm_native_builtins::ffm_fast`.
                    //
                    // The decline edge is what makes this safe to be wrong
                    // about: helper returns 0 and control falls into the
                    // same `invoke_dispatch` the site would have used
                    // anyway, so a heap-backed carrier, a closed scope, an
                    // out-of-bounds index or an unrecognised shape all keep
                    // today's behaviour AND today's exceptions. Nothing
                    // here has to reproduce an exception.
                    if !intrinsic_handled
                        && (callee_entry == crate::JitIntrinsic::FfmSegmentGetAtIndex.as_entry()
                            || callee_entry == crate::JitIntrinsic::FfmSegmentSetAtIndex.as_entry())
                    {
                        let is_get =
                            callee_entry == crate::JitIntrinsic::FfmSegmentGetAtIndex.as_entry();
                        let helper = if is_get {
                            self.helpers.ffm_segment_get
                        } else {
                            self.helpers.ffm_segment_set
                        };
                        // The site's own dispatch info: the decline edge's
                        // call target, and the source of the element kind
                        // (the descriptor NAMES the `ValueLayout` subtype,
                        // so the width is a compile-time constant).
                        let info_ptr = self
                            .invoke_info_idx
                            .get(&pc)
                            .map(|&i| self.invoke_info[i].1);
                        // SAFETY: the pointee is owned by this compile's
                        // `_jit_invoke_infos` arena and outlives the code.
                        let kind = info_ptr.and_then(|p| {
                            if p.is_null() {
                                None
                            } else {
                                // SAFETY: `p` is non-null here and owned by this compile's `_jit_invoke_infos` arena, which outlives the code.
                                crate::ffm_kind_for_descriptor(unsafe { (*p).descriptor })
                            }
                        });
                        match (info_ptr, kind) {
                            (Some(info), Some(kind)) if helper != 0 && !info.is_null() => {
                                // SAFETY: the pointee is owned by this
                                // compile's `_jit_invoke_infos` arena and
                                // outlives the code being emitted.
                                let ret_tag = unsafe { (*info).return_type };
                                if false {
                                    eprintln!("[ffm] EMITTED pc={pc} kind={kind} is_get={is_get}");
                                }
                                self.flush_scratch_registers();
                                // Operands, deepest first: receiver, layout,
                                // index, and (set only) the value.
                                let value_slot = if is_get { None } else { Some(self.pop_stack()) };
                                let index_slot = self.pop_stack();
                                let layout_slot = self.pop_stack();
                                let recv_slot = self.pop_stack();

                                // SBR4 lane segacc: a five-word block.
                                //   word 0  element index (converted, the helper's)
                                //   word 1  the value (set) / the out-parameter (get)
                                //   word 2  receiver     \
                                //   word 3  layout        } the decline edge's operands
                                //   word 4  index as pushed /
                                // The helper takes (vm_ptr, segment, layout |
                                // kind, &block) so it can serve a REAL
                                // segment (the JDK's own
                                // `NativeMemorySegmentImpl`) through a native
                                // context. The block is reserved AFTER the
                                // pops, so it can overlap the operand homes:
                                // every operand is therefore loaded into a
                                // register before any word is stored, and the
                                // decline edge reads its operands back from
                                // the block, never from the homes.
                                let block_base =
                                    match self.reserve_spill_slots(5, SpillReason::HelperArgs) {
                                        Some(b) => b,
                                        None => {
                                            self.fail("singlepass-codegen/ffm-out-spill-exhausted");
                                            return WalkStep::Return(false);
                                        }
                                    };

                                // The third helper argument is
                                // `layout | kind` (round 12 wave 5, lane
                                // ffm2): the helper needs the LAYOUT to
                                // decline a non-host byte order and to check
                                // its alignment, and a heap reference's
                                // 8-byte alignment leaves the low three bits
                                // for the kind (0..=6) with no ABI change.
                                // A kind that does not fit fails the compile.
                                let kind_bits = match i8::try_from(kind) {
                                    Ok(k) if (0..8).contains(&k) => k,
                                    _ => {
                                        self.fail("singlepass-codegen/ffm-kind-out-of-range");
                                        return WalkStep::Return(false);
                                    }
                                };

                                // A `get`/`set(layout, long offset)` site
                                // (round 12 wave 7, lane ffm4) passes a BYTE
                                // offset where the helper takes an element
                                // index. Only an offset that is a multiple of
                                // the element width is converted (`index =
                                // offset >> log2(width)`, so the helper's
                                // `index * width` is the offset again, and its
                                // bounds and alignment checks see the same
                                // address the native would); any other offset
                                // takes the decline edge. A negative offset
                                // shifts to an index whose `* width`
                                // overflows, which the helper also declines.
                                // SAFETY: `info` is non-null (the match guard)
                                // and owned by this compile's `_jit_invoke_infos`
                                // arena, which outlives the code.
                                let site_method: &str = unsafe { (*info).method_name };
                                let by_offset = matches!(site_method, "get" | "set");
                                let offset_shift: u8 = match kind {
                                    0 => 0,
                                    1 | 2 => 1,
                                    3 | 5 => 2,
                                    4 | 6 => 3,
                                    _ => {
                                        self.fail("singlepass-codegen/ffm-kind-width-unknown");
                                        return WalkStep::Return(false);
                                    }
                                };

                                // ---- fast path -------------------------
                                // No safepoint spill and no oop map: the
                                // helper neither allocates nor blocks, so no
                                // GC can run inside it and no oop it is
                                // handed can move.
                                // Frame offsets run DOWN in memory (`[rbp - off]`), so
                                // word `i` of the block sits at `block_base + 8 * (4 - i)`
                                // and the words ascend in memory from word 0.
                                let blk = |i: i32| block_base + 8 * (4 - i);
                                // Every operand into a register FIRST (the
                                // block may overlap their homes), then the
                                // block. RAX/R10 are no argument register.
                                self.load_slot_to_reg(ARG_REGS[1], recv_slot);
                                self.load_slot_to_reg(ARG_REGS[2], layout_slot);
                                self.load_slot_to_reg(RAX, index_slot);
                                match value_slot {
                                    Some(v) if !is_get => self.load_slot_to_reg(R10, v),
                                    None if is_get => {}
                                    _ => {
                                        self.fail("singlepass-codegen/ffm-missing-arg3-operand");
                                        return WalkStep::Return(false);
                                    }
                                }
                                self.emit_store_local(blk(2), ARG_REGS[1]);
                                self.emit_store_local(blk(3), ARG_REGS[2]);
                                self.emit_store_local(blk(4), RAX);
                                if !is_get {
                                    self.emit_store_local(blk(1), R10);
                                }
                                // The element index of word 0 is converted
                                // from a byte offset when the site is
                                // `get`/`set`; an offset that is not a
                                // multiple of the width takes the decline
                                // edge.
                                let mut unaligned_offset = None;
                                // JIT r23 wave 4 (lane L8,
                                // `CRATONVM_JIT_FFM_UNALIGNED_OFFSET_ACCESS`):
                                // an offset that is not a multiple of the
                                // width calls the helper too, with word 0 =
                                // `FFM_BYTE_OFFSET_MARK` (word 4 already holds
                                // the offset as pushed); the helper serves it
                                // as a byte offset or declines.
                                let mut unaligned_to_mark = None;
                                if by_offset && offset_shift > 0 {
                                    let low_bits = (1i32 << offset_shift) - 1;
                                    self.emit_test_r64_imm32(RAX, low_bits);
                                    if crate::ffm_unaligned_offset_access_enabled() {
                                        // JNZ -> MOV RAX, MARK; JMP store.
                                        unaligned_to_mark = Some(self.emit_jcc_rel32_patch(0x85));
                                    } else {
                                        // JNZ -> the decline edge.
                                        unaligned_offset = Some(self.emit_jcc_rel32_patch(0x85));
                                    }
                                    self.emit_shr_r64_imm8(RAX, offset_shift);
                                }
                                if let Some(patch) = unaligned_to_mark {
                                    let store = self.emit_jmp_rel32_patch();
                                    self.patch_rel32_to_here(patch);
                                    self.emit_mov_imm64(RAX, crate::FFM_BYTE_OFFSET_MARK);
                                    self.patch_rel32_to_here(store);
                                }
                                self.emit_store_local(blk(0), RAX);
                                // (vm_ptr, segment, layout | kind, block).
                                self.emit_load_vm_context(ARG_REGS[0]);
                                self.emit_or_r64_imm8(ARG_REGS[2], kind_bits);
                                self.emit_lea_frame_slot(ARG_REGS[3], blk(0));
                                // The FFM accessors, both reached through this one sequence.
                                // Both take (vm_ptr, segment, layout | kind, block pointer);
                                // the macro counts four integer argument registers, which is
                                // what this site writes.
                                cratonvm_jit_api::assert_helper_call_shape!(
                                    "ffm_segment_get",
                                    int_args = 4,
                                    returns_value = true
                                );
                                cratonvm_jit_api::assert_helper_call_shape!(
                                    "ffm_segment_set",
                                    int_args = 4,
                                    returns_value = true
                                );
                                self.emit_call_absolute(helper);
                                self.emit_test_r64_r64(RAX);
                                // RAX == 0 -> declined.
                                let declined = self.emit_jcc_rel32_patch(0x84);
                                if is_get {
                                    self.emit_load_local(RAX, blk(1));
                                }
                                let done = self.emit_jmp_rel32_patch();

                                // ---- decline edge: the unchanged dispatch
                                self.patch_rel32_to_here(declined);
                                if let Some(patch) = unaligned_offset {
                                    self.patch_rel32_to_here(patch);
                                }
                                let nargs = if is_get { 3 } else { 4 };
                                let args_base = match self
                                    .reserve_spill_slots(nargs, SpillReason::HelperArgs)
                                {
                                    Some(b) => b,
                                    None => {
                                        self.fail("singlepass-codegen/ffm-args-spill-exhausted");
                                        return WalkStep::Return(false);
                                    }
                                };
                                // `jit_invoke_dispatch`'s buffer runs
                                // arg[0] at the HIGHEST offset down to
                                // arg[n-1] at the lowest — the same layout
                                // the generic dispatch site builds. Load
                                // every operand into a distinct register
                                // before storing any of them: the buffer can
                                // overlap the operand homes, and a
                                // load-then-store per index would overwrite
                                // a home not yet read.
                                // (SBR4 segacc: from the block, whose words
                                // 2, 3, 4 and 1 hold receiver, layout, index
                                // as pushed and the value -- the homes may
                                // have been overwritten by the block.)
                                self.emit_load_local(RAX, blk(2));
                                self.emit_load_local(RCX, blk(3));
                                self.emit_load_local(RDX, blk(4));
                                if value_slot.is_some() {
                                    self.emit_load_local(R10, blk(1));
                                }
                                let top = args_base + (nargs as i32 - 1) * 8;
                                self.emit_store_local(top, RAX);
                                self.emit_store_local(top - 8, RCX);
                                self.emit_store_local(top - 16, RDX);
                                if value_slot.is_some() {
                                    self.emit_store_local(top - 24, R10);
                                }
                                self.emit_load_vm_context(ARG_REGS[0]);
                                self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64);
                                self.emit_lea_frame_slot(ARG_REGS[2], top);
                                self.emit_mov_imm32_sx(ARG_REGS[3], nargs as i32);
                                self.emit_pre_safepoint_spill_before_helper_call();
                                // (vm_ptr, info_ptr, args_ptr, num_args) -> result | sentinel.
                                cratonvm_jit_api::assert_helper_call_shape!(
                                    "invoke_dispatch",
                                    int_args = 4,
                                    returns_value = true
                                );
                                self.emit_call_absolute(self.helpers.invoke_dispatch);
                                self.emit_oop_map_for_safepoint();
                                // The site's REAL return tag, not `b'I'`.
                                // `emit_post_invoke_exception_check` has a
                                // separate arm for `J`/`D`/`F` because the
                                // pending-exception sentinel is `i64::MIN`,
                                // which is also a LEGITIMATE value for those
                                // widths — `-0.0` as a double is exactly
                                // that word. Passing `b'I'` took the plain
                                // `CMP RAX, i64::MIN; JE bail` arm and would
                                // have mistaken such a value for a throw.
                                self.emit_post_invoke_exception_check(if is_get {
                                    ret_tag
                                } else {
                                    b'V'
                                });

                                // ---- join ------------------------------
                                self.patch_rel32_to_here(done);
                                // Reclaim both the argument buffer and the
                                // out slot; RAX already carries whichever
                                // path ran.
                                self.next_spill_offset = block_base.min(args_base);
                                if is_get {
                                    // Both arms converge with the value's
                                    // RAW BITS in RAX — the helper returns
                                    // them that way and `invoke_dispatch`
                                    // already did — so one push serves the
                                    // fast path and the decline edge. A
                                    // float/double has to reach an XMM
                                    // stack slot, which is the same
                                    // `MOVQ XMM0, RAX` the generic dispatch
                                    // emits for those return types.
                                    if matches!(ret_tag, b'F' | b'D') {
                                        self.push_from_rax_as_xmm0();
                                    } else {
                                        self.push_from_rax();
                                    }
                                }
                                intrinsic_handled = true;
                            }
                            // Falling through here is NOT safe: the site
                            // carries an intrinsic SENTINEL as its
                            // `JitDirectCall::entry`, and the ordinary
                            // direct-call path would CALL that sentinel
                            // (measured: `EXCEPTION_ACCESS_VIOLATION at
                            // pc=0xFFFFFFFFFFFFFFB1`). Registration and
                            // emission are gated on the same
                            // `ffm_kind_for_descriptor`, so reaching this
                            // arm means they disagreed — bail the whole
                            // compile, which drops the method to the
                            // interpreter and is always safe.
                            _ => {
                                if false {
                                    eprintln!(
                                        "[ffm] UNHANDLED pc={pc} info={} kind={:?} helper={}",
                                        info_ptr.is_some(),
                                        kind,
                                        helper != 0
                                    );
                                }
                                self.fail("singlepass-codegen/ffm-unhandled-sentinel");
                                return WalkStep::Return(false);
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: FFM_SEGMENT =====

                    // ===== INTRINSIC REGION BEGIN: ATOMIC_INT =====
                    // `AtomicInteger` RMW family, emitted as ONE
                    // `LOCK XADD [value], ECX`.
                    //
                    // `XADD` atomically adds the source register to the
                    // destination and leaves the PRE-add value in the
                    // source, which is exactly `getAndAdd` semantics; the
                    // `*AndGet` forms add the delta back afterwards. That
                    // replaces a full native dispatch (~250 ns/op measured)
                    // with a single locked instruction.
                    //
                    // Soundness rests on three things:
                    //   * the registered native keeps its state in the SAME
                    //     memory (`get_field_volatile(this, 0)` /
                    //     `compare_and_swap_field(this, 0, ..)`), so an
                    //     interpreted caller and a compiled caller still
                    //     agree on one location;
                    //   * the receiver class-id guard below — AtomicInteger
                    //     is not final, so a subclass override must NOT take
                    //     this path;
                    //   * the per-object COMPACT/LEGACY branch, the same one
                    //     `emit_load_string_i32_field` uses, because a class
                    //     with a registered `CompactLayout` may still have
                    //     legacy-laid-out instances.
                    // Every uncertain case (null receiver, class mismatch)
                    // goes to the shared uncommon-trap stub and re-runs in
                    // the interpreter, which reproduces the NPE exactly.
                    if !intrinsic_handled {
                        // (delta_imm, return_post_add, delta_is_arg)
                        //
                        // `AtomicIntGet` is the one arm with no delta at
                        // all: it reads the same slot the RMW forms address
                        // and returns it. Everything before the final
                        // instruction -- null check, class guard, the
                        // per-object COMPACT/LEGACY branch, the deopt stub
                        // -- is shared, which is the whole reason it belongs
                        // in this block rather than beside it.
                        let is_load = callee_entry == crate::JitIntrinsic::AtomicIntGet.as_entry();
                        let plan: Option<(i32, bool, bool)> = if is_load {
                            Some((0, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicIntGetAndIncrement.as_entry()
                        {
                            Some((1, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicIntGetAndDecrement.as_entry()
                        {
                            Some((-1, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicIntIncrementAndGet.as_entry()
                        {
                            Some((1, true, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicIntDecrementAndGet.as_entry()
                        {
                            Some((-1, true, false))
                        } else if callee_entry == crate::JitIntrinsic::AtomicIntGetAndAdd.as_entry()
                        {
                            Some((0, false, true))
                        } else if callee_entry == crate::JitIntrinsic::AtomicIntAddAndGet.as_entry()
                        {
                            Some((0, true, true))
                        } else {
                            None
                        };
                        if let Some((delta_imm, return_post_add, delta_is_arg)) = plan {
                            // Recomputed from the same two inputs the
                            // matcher used; `None` here cannot happen for a
                            // registered site, and bailing keeps the plain
                            // direct-call path rather than emitting a CALL
                            // to an intrinsic sentinel.
                            if let Some(layout) =
                                crate::AtomicIntFieldLayout::new(0, guard_class_id)
                            {
                                self.flush_scratch_registers();
                                if crate::deopt_real_enabled() {
                                    self.snapshot_pre_intrinsic_call(
                                        pc,
                                        crate::deopt::DeoptReason::ReceiverTypeChanged,
                                    );
                                }
                                let mut bail: Vec<usize> = Vec::new();
                                // Operands: delta (if any) is shallower,
                                // the receiver is deepest.
                                let delta_slot = if delta_is_arg {
                                    Some(self.pop_stack())
                                } else {
                                    None
                                };
                                let recv_slot = self.pop_stack();
                                // The offsets below are a compile-time claim about the
                                // class layout; a layout REPLACEMENT bumps the process
                                // epoch and this site then deopts, as the class guard
                                // does. First, before any operand load: the guard's
                                // fallback form clobbers RCX and R11.
                                if jit_sp_field_layout_guard_enabled() {
                                    bail.extend(self.emit_layout_epoch_guard());
                                }

                                // RAX = receiver; null → deopt.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_test_r64_r64(RAX);
                                bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                // Exact receiver class guard:
                                // CMP DWORD [RAX + 0], guard_class_id ; JNE
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                // EDX = delta (kept for the *AndGet fixup,
                                // since XADD overwrites its source with the
                                // pre-add value).
                                if !is_load {
                                    match delta_slot {
                                        Some(slot) => self.load_slot_to_reg(RDX, slot),
                                        None => {
                                            self.buf.emit(&[0xBA]); // MOV EDX, imm32
                                            self.buf.emit(&delta_imm.to_le_bytes());
                                        }
                                    }
                                    self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX
                                }

                                // Per-object layout branch.
                                self.emit_test_mem8_imm8(
                                    RAX,
                                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                    cratonvm_types::GC_FLAG_COMPACT,
                                );
                                let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                if is_load {
                                    // MOV ECX, [RAX + compact]. A plain load
                                    // is the correct volatile/acquire read on
                                    // x86-64: loads are not reordered with
                                    // older loads, so nothing is owed here.
                                    self.buf.emit(&[0x8B, 0x88]);
                                    self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                } else {
                                    // LOCK XADD [RAX + compact], ECX
                                    self.buf.emit(&[0xF0, 0x0F, 0xC1, 0x88]);
                                    self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                }
                                let done = self.emit_jmp_rel32_patch();
                                self.patch_rel32_to_here(legacy);
                                if is_load {
                                    // MOV ECX, [RAX + legacy]
                                    self.buf.emit(&[0x8B, 0x88]);
                                    self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                } else {
                                    // LOCK XADD [RAX + legacy], ECX
                                    self.buf.emit(&[0xF0, 0x0F, 0xC1, 0x88]);
                                    self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                }
                                self.patch_rel32_to_here(done);

                                // ECX now holds the PRE-add value.
                                if return_post_add {
                                    self.buf.emit(&[0x01, 0xD1]); // ADD ECX, EDX
                                }
                                self.buf.emit(&[0x48, 0x63, 0xC1]); // MOVSXD RAX, ECX
                                self.push_from_rax();
                                // Counted HERE, where the site is emitted,
                                // not in the matcher (review #80).
                                crate::ATOMIC_INTRINSIC_SITES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                                for p in bail {
                                    self.deopt_stubs.push((p, pc, 6));
                                }
                                intrinsic_handled = true;
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: ATOMIC_INT =====

                    // ===== INTRINSIC REGION BEGIN: ATOMIC_INT_CAS =====
                    // `compareAndSet` / `weakCompareAndSet` as one
                    // `LOCK CMPXCHG [value], RDX`.
                    //
                    // Operand placement differs from the `XADD` arms above
                    // and has to: `CMPXCHG` compares the memory operand
                    // against RAX IMPLICITLY, so RAX belongs to the
                    // EXPECTED value and the receiver moves to RCX. Getting
                    // that backwards compiles and silently compares the
                    // object pointer against the field.
                    //
                    // The result is ZF, not the field, so the arm ends
                    // `SETZ AL` / `MOVZX EAX, AL` — and on failure RAX
                    // holds the value CMPXCHG read, which is deliberately
                    // discarded: `compareAndSet` returns only the boolean
                    // (`compareAndExchange`, which returns the witness, is
                    // NOT claimed here and keeps its native).
                    if !intrinsic_handled
                        && callee_entry == crate::JitIntrinsic::AtomicIntCompareAndSet.as_entry()
                    {
                        if let Some(layout) = crate::AtomicIntFieldLayout::new(0, guard_class_id) {
                            self.flush_scratch_registers();
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            let mut bail: Vec<usize> = Vec::new();
                            // Deepest first on the operand stack: receiver,
                            // expected, update. Popped in reverse.
                            let update_slot = self.pop_stack();
                            let expect_slot = self.pop_stack();
                            let recv_slot = self.pop_stack();
                            // The offsets below are a compile-time claim about the
                            // class layout; a layout REPLACEMENT bumps the process
                            // epoch and this site then deopts, as the class guard
                            // does. First, before any operand load: the guard's
                            // fallback form clobbers RCX and R11.
                            if jit_sp_field_layout_guard_enabled() {
                                bail.extend(self.emit_layout_epoch_guard());
                            }

                            // RCX = receiver; null -> deopt.
                            self.load_slot_to_reg(RCX, recv_slot);
                            self.emit_test_r64_r64(RCX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                            // Exact receiver class guard:
                            // CMP DWORD [RCX + 0], guard_class_id ; JNE
                            self.buf.emit(&[0x81, 0x79, 0x00]);
                            self.buf.emit(&guard_class_id.to_le_bytes());
                            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                            // RAX = expected (the implicit comparand),
                            // RDX = update. Loaded AFTER the guard so a
                            // deopt path does not depend on them.
                            self.load_slot_to_reg(RAX, expect_slot);
                            self.load_slot_to_reg(RDX, update_slot);

                            // Per-object layout branch, as the arms above.
                            self.emit_test_mem8_imm8(
                                RCX,
                                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                cratonvm_types::GC_FLAG_COMPACT,
                            );
                            let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                            self.buf.emit(&[0xF0, 0x0F, 0xB1, 0x91]);
                            self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                            let done = self.emit_jmp_rel32_patch();
                            self.patch_rel32_to_here(legacy);
                            self.buf.emit(&[0xF0, 0x0F, 0xB1, 0x91]);
                            self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                            self.patch_rel32_to_here(done);

                            // ZF = "the swap happened".
                            self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETZ AL
                            self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                            self.push_from_rax();
                            crate::ATOMIC_INTRINSIC_SITES
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                            for p in bail {
                                self.deopt_stubs.push((p, pc, 6));
                            }
                            intrinsic_handled = true;
                        }
                    }
                    // ===== INTRINSIC REGION END: ATOMIC_INT_CAS =====

                    // ===== INTRINSIC REGION BEGIN: ATOMIC_LONG =====
                    // `AtomicLong`, emitted as ONE REX.W `LOCK XADD
                    // [value], RCX`. Structurally identical to the 32-bit
                    // region above -- same null check, same exact class-id
                    // guard, same per-object COMPACT/LEGACY branch, same
                    // deopt stub for every uncertain case -- and different
                    // only in operand width.
                    //
                    // The width differences are the whole risk surface, so
                    // they are spelled out:
                    //   * every access carries REX.W (0x48), so it reads
                    //     and writes 8 bytes;
                    //   * the layout's LEGACY offset is the 64-bit payload
                    //     offset inside the `Value` cell, not the 32-bit
                    //     one, and a compact storage width other than 8 is
                    //     refused by `AtomicLongFieldLayout::new`;
                    //   * the delta immediate is `MOV RDX, imm32`
                    //     sign-extended, which is exact for the only
                    //     immediates this region uses, +1 and -1;
                    //   * no `MOVSXD` at the end -- the value is already
                    //     64-bit in RCX, and sign-extending it would be
                    //     both wrong and unnecessary.
                    //
                    // An aligned 8-byte `MOV` is atomic on x86-64 and is a
                    // correct volatile/acquire load, so the `get` arm owes
                    // no `LOCK` and no fence.
                    if !intrinsic_handled {
                        let is_load = callee_entry == crate::JitIntrinsic::AtomicLongGet.as_entry();
                        // (delta_imm, return_post_add, delta_is_arg)
                        let plan: Option<(i32, bool, bool)> = if is_load {
                            Some((0, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongGetAndIncrement.as_entry()
                        {
                            Some((1, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongGetAndDecrement.as_entry()
                        {
                            Some((-1, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongIncrementAndGet.as_entry()
                        {
                            Some((1, true, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongDecrementAndGet.as_entry()
                        {
                            Some((-1, true, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongGetAndAdd.as_entry()
                        {
                            Some((0, false, true))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongAddAndGet.as_entry()
                        {
                            Some((0, true, true))
                        } else {
                            None
                        };
                        if let Some((delta_imm, return_post_add, delta_is_arg)) = plan {
                            // Recomputed from the same two inputs the
                            // matcher used; `None` cannot happen for a
                            // registered site, and bailing keeps the plain
                            // direct-call path rather than emitting a CALL
                            // to an intrinsic sentinel.
                            if let Some(layout) =
                                crate::AtomicLongFieldLayout::new(0, guard_class_id)
                            {
                                self.flush_scratch_registers();
                                if crate::deopt_real_enabled() {
                                    self.snapshot_pre_intrinsic_call(
                                        pc,
                                        crate::deopt::DeoptReason::ReceiverTypeChanged,
                                    );
                                }
                                let mut bail: Vec<usize> = Vec::new();
                                // Operands: delta (if any) is shallower,
                                // the receiver is deepest.
                                let delta_slot = if delta_is_arg {
                                    Some(self.pop_stack())
                                } else {
                                    None
                                };
                                let recv_slot = self.pop_stack();
                                // The offsets below are a compile-time claim about the
                                // class layout; a layout REPLACEMENT bumps the process
                                // epoch and this site then deopts, as the class guard
                                // does. First, before any operand load: the guard's
                                // fallback form clobbers RCX and R11.
                                if jit_sp_field_layout_guard_enabled() {
                                    bail.extend(self.emit_layout_epoch_guard());
                                }

                                // RAX = receiver; null -> deopt.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_test_r64_r64(RAX);
                                bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                // Exact receiver class guard:
                                // CMP DWORD [RAX + 0], guard_class_id ; JNE
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                // RDX = delta (kept for the *AndGet fixup,
                                // since XADD overwrites its source with the
                                // pre-add value).
                                if !is_load {
                                    match delta_slot {
                                        Some(slot) => self.load_slot_to_reg(RDX, slot),
                                        None => {
                                            // MOV RDX, imm32 (sign-extended)
                                            self.buf.emit(&[0x48, 0xC7, 0xC2]);
                                            self.buf.emit(&delta_imm.to_le_bytes());
                                        }
                                    }
                                    self.buf.emit(&[0x48, 0x89, 0xD1]); // MOV RCX, RDX
                                }

                                // Per-object layout branch.
                                self.emit_test_mem8_imm8(
                                    RAX,
                                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                    cratonvm_types::GC_FLAG_COMPACT,
                                );
                                let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                if is_load {
                                    // MOV RCX, [RAX + compact]
                                    self.buf.emit(&[0x48, 0x8B, 0x88]);
                                    self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                } else {
                                    // LOCK XADD [RAX + compact], RCX
                                    self.buf.emit(&[0xF0, 0x48, 0x0F, 0xC1, 0x88]);
                                    self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                }
                                let done = self.emit_jmp_rel32_patch();
                                self.patch_rel32_to_here(legacy);
                                if is_load {
                                    // MOV RCX, [RAX + legacy]
                                    self.buf.emit(&[0x48, 0x8B, 0x88]);
                                    self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                } else {
                                    // LOCK XADD [RAX + legacy], RCX
                                    self.buf.emit(&[0xF0, 0x48, 0x0F, 0xC1, 0x88]);
                                    self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                }
                                self.patch_rel32_to_here(done);

                                // RCX now holds the PRE-add value.
                                if return_post_add {
                                    self.buf.emit(&[0x48, 0x01, 0xD1]); // ADD RCX, RDX
                                }
                                self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
                                self.push_from_rax();
                                // Counted at emission, not in the matcher
                                // (review #80).
                                crate::ATOMIC_LONG_INTRINSIC_SITES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                                for p in bail {
                                    self.deopt_stubs.push((p, pc, 6));
                                }
                                intrinsic_handled = true;
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: ATOMIC_LONG =====

                    // ===== INTRINSIC REGION BEGIN: ATOMIC_LONG_CAS =====
                    // `compareAndSet` / `weakCompareAndSet` as one
                    // REX.W `LOCK CMPXCHG [value], RDX`.
                    //
                    // Operand placement differs from the `XADD` arms above
                    // and has to: `CMPXCHG` compares the memory operand
                    // against RAX IMPLICITLY, so RAX belongs to the
                    // EXPECTED value and the receiver moves to RCX. Getting
                    // that backwards compiles and silently compares the
                    // object pointer against the field.
                    //
                    // The result is ZF, not the field, so the arm ends
                    // `SETZ AL` / `MOVZX EAX, AL` — and on failure RAX
                    // holds the value CMPXCHG read, which is deliberately
                    // discarded: `compareAndSet` returns only the boolean
                    // (`compareAndExchange`, which returns the witness, is
                    // NOT claimed here and keeps its native).
                    if !intrinsic_handled
                        && callee_entry == crate::JitIntrinsic::AtomicLongCompareAndSet.as_entry()
                    {
                        if let Some(layout) = crate::AtomicLongFieldLayout::new(0, guard_class_id) {
                            self.flush_scratch_registers();
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            let mut bail: Vec<usize> = Vec::new();
                            // Deepest first on the operand stack: receiver,
                            // expected, update. Popped in reverse.
                            let update_slot = self.pop_stack();
                            let expect_slot = self.pop_stack();
                            let recv_slot = self.pop_stack();
                            // The offsets below are a compile-time claim about the
                            // class layout; a layout REPLACEMENT bumps the process
                            // epoch and this site then deopts, as the class guard
                            // does. First, before any operand load: the guard's
                            // fallback form clobbers RCX and R11.
                            if jit_sp_field_layout_guard_enabled() {
                                bail.extend(self.emit_layout_epoch_guard());
                            }

                            // RCX = receiver; null -> deopt.
                            self.load_slot_to_reg(RCX, recv_slot);
                            self.emit_test_r64_r64(RCX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                            // Exact receiver class guard:
                            // CMP DWORD [RCX + 0], guard_class_id ; JNE
                            self.buf.emit(&[0x81, 0x79, 0x00]);
                            self.buf.emit(&guard_class_id.to_le_bytes());
                            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                            // RAX = expected (the implicit comparand),
                            // RDX = update. Loaded AFTER the guard so a
                            // deopt path does not depend on them.
                            self.load_slot_to_reg(RAX, expect_slot);
                            self.load_slot_to_reg(RDX, update_slot);

                            // Per-object layout branch, as the arms above.
                            self.emit_test_mem8_imm8(
                                RCX,
                                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                cratonvm_types::GC_FLAG_COMPACT,
                            );
                            let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                            self.buf.emit(&[0xF0, 0x48, 0x0F, 0xB1, 0x91]);
                            self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                            let done = self.emit_jmp_rel32_patch();
                            self.patch_rel32_to_here(legacy);
                            self.buf.emit(&[0xF0, 0x48, 0x0F, 0xB1, 0x91]);
                            self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                            self.patch_rel32_to_here(done);

                            // ZF = "the swap happened".
                            self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETZ AL
                            self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                            self.push_from_rax();
                            crate::ATOMIC_LONG_INTRINSIC_SITES
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                            for p in bail {
                                self.deopt_stubs.push((p, pc, 6));
                            }
                            intrinsic_handled = true;
                        }
                    }
                    // ===== INTRINSIC REGION END: ATOMIC_LONG_CAS =====

                    // ===== INTRINSIC REGION BEGIN: BOX_UNBOX =====
                    // `Long.longValue()J` and `Integer.intValue()I` — the
                    // UNBOX half of autoboxing, emitted as the same aligned
                    // `MOV` the two `*Get` arms above emit. `Long.value` /
                    // `Integer.value` are `private final` at field slot 0:
                    // a plain load, no `LOCK`, no fence, nothing to order.
                    //
                    // Structurally this is the `is_load` path of the two
                    // regions above with a different class guard, and it is
                    // written out rather than shared with them because the
                    // two differ in exactly one thing an abstraction would
                    // have to re-introduce anyway — the operand width, and
                    // with it the final sign-extension (`MOVSXD` for `I`,
                    // none for `J`).
                    //
                    // Null receiver deopts to the interpreter (bail reason
                    // 6), which re-runs the unbox and raises the same NPE
                    // `Long.longValue` on `null` raises today. Nothing is
                    // CALLed on the inline path.
                    if !intrinsic_handled {
                        let is_long = callee_entry == crate::JitIntrinsic::LongLongValue.as_entry();
                        let is_int =
                            callee_entry == crate::JitIntrinsic::IntegerIntValue.as_entry();
                        if is_long || is_int {
                            // Recomputed from the same two inputs the
                            // matcher used. `None` cannot happen for a
                            // registered site; bailing keeps the plain
                            // direct-call path rather than emitting a CALL
                            // to an intrinsic sentinel.
                            let offsets = if is_long {
                                crate::AtomicLongFieldLayout::new(0, guard_class_id)
                                    .map(|l| (l.value_compact_offset, l.value_legacy_offset))
                            } else {
                                crate::AtomicIntFieldLayout::new(0, guard_class_id)
                                    .map(|l| (l.value_compact_offset, l.value_legacy_offset))
                            };
                            if let Some((compact_off, legacy_off)) = offsets {
                                self.flush_scratch_registers();
                                if crate::deopt_real_enabled() {
                                    self.snapshot_pre_intrinsic_call(
                                        pc,
                                        crate::deopt::DeoptReason::ReceiverTypeChanged,
                                    );
                                }
                                let mut bail: Vec<usize> = Vec::new();
                                let recv_slot = self.pop_stack();
                                // The offsets below are a compile-time claim about the
                                // class layout; a layout REPLACEMENT bumps the process
                                // epoch and this site then deopts, as the class guard
                                // does. First, before any operand load: the guard's
                                // fallback form clobbers RCX and R11.
                                if jit_sp_field_layout_guard_enabled() {
                                    bail.extend(self.emit_layout_epoch_guard());
                                }

                                // RAX = receiver; null -> deopt.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_test_r64_r64(RAX);
                                bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                // Exact receiver class guard:
                                // CMP DWORD [RAX + 0], guard_class_id ; JNE
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                // Per-object layout branch, exactly as the
                                // `Atomic*` arms do it: a COMPACT instance
                                // stores the payload at the registered body
                                // offset, a LEGACY one inside its 16-byte
                                // `Value` cell.
                                self.emit_test_mem8_imm8(
                                    RAX,
                                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                    cratonvm_types::GC_FLAG_COMPACT,
                                );
                                let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                if is_long {
                                    // MOV RCX, [RAX + compact]
                                    self.buf.emit(&[0x48, 0x8B, 0x88]);
                                } else {
                                    // MOV ECX, [RAX + compact]
                                    self.buf.emit(&[0x8B, 0x88]);
                                }
                                self.buf.emit(&compact_off.to_le_bytes());
                                let done = self.emit_jmp_rel32_patch();
                                self.patch_rel32_to_here(legacy);
                                if is_long {
                                    // MOV RCX, [RAX + legacy]
                                    self.buf.emit(&[0x48, 0x8B, 0x88]);
                                } else {
                                    // MOV ECX, [RAX + legacy]
                                    self.buf.emit(&[0x8B, 0x88]);
                                }
                                self.buf.emit(&legacy_off.to_le_bytes());
                                self.patch_rel32_to_here(done);

                                if is_long {
                                    self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
                                } else {
                                    // MOVSXD RAX, ECX — an `int` is kept
                                    // sign-extended in the 64-bit operand
                                    // slot, the same way the `AtomicInt`
                                    // arm above ends.
                                    self.buf.emit(&[0x48, 0x63, 0xC1]);
                                }
                                self.push_from_rax();
                                // Counted at emission, not in the matcher
                                // (review #80).
                                if is_long {
                                    crate::LONG_LONG_VALUE_INTRINSIC_SITES
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                } else {
                                    crate::INTEGER_INT_VALUE_INTRINSIC_SITES
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }

                                for p in bail {
                                    self.deopt_stubs.push((p, pc, 6));
                                }
                                intrinsic_handled = true;
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: BOX_UNBOX =====

                    // ===== INTRINSIC REGION BEGIN: STRINGBUILDER_ACCESS =====
                    // java.lang.StringBuilder: length()I and append(C).
                    //
                    // MEASURED on this tree, and the reason this region
                    // exists: `StringBuilder.length()` is 194 ns/op and
                    // `append(char)` 349 ns, while `String.length()` — the
                    // same shape, already intrinsified two regions below —
                    // is 2 ns. `System.identityHashCode`, a trivial native
                    // with one object argument, is 120 ns, which is what
                    // the boundary alone costs. The builder was paying it
                    // on every call.
                    //
                    // # The slow edge is a CALL, not an uncommon trap
                    //
                    // Every other intrinsic in this file deopts on a failed
                    // guard, because its guards fail on genuinely uncommon
                    // things (a null receiver, an out-of-bounds index).
                    // `append`'s do not: a full payload is what every
                    // growing builder reaches O(log n) times, and a UTF16
                    // builder fails the coder guard on EVERY call. Deopting
                    // there would re-run the whole method in the
                    // interpreter each time, so these edges go to the same
                    // `invoke_dispatch` the site would have used anyway —
                    // the decline-edge shape the FFM region above
                    // established. Nothing here has to reproduce an
                    // exception, a growth, or a coder inflation: the native
                    // does all three, unchanged.
                    if !intrinsic_handled
                        && (callee_entry == crate::JitIntrinsic::StringBuilderLength.as_entry()
                            || callee_entry
                                == crate::JitIntrinsic::StringBuilderAppendChar.as_entry()
                            || callee_entry == crate::JitIntrinsic::StringBuilderCharAt.as_entry()
                            || callee_entry
                                == crate::JitIntrinsic::StringBuilderAppendString.as_entry()
                            || callee_entry
                                == crate::JitIntrinsic::StringBuilderAppendInt.as_entry()
                            || callee_entry
                                == crate::JitIntrinsic::StringBuilderToString.as_entry())
                    {
                        let is_append =
                            callee_entry == crate::JitIntrinsic::StringBuilderAppendChar.as_entry();
                        let is_char_at =
                            callee_entry == crate::JitIntrinsic::StringBuilderCharAt.as_entry();
                        let is_append_string = callee_entry
                            == crate::JitIntrinsic::StringBuilderAppendString.as_entry();
                        let is_append_int =
                            callee_entry == crate::JitIntrinsic::StringBuilderAppendInt.as_entry();
                        // JIT round 17 wave 1, lane strings (STR16-P1,
                        // `CRATONVM_JIT_SB_TOSTRING_INLINE`): no argument,
                        // and the answer is a FRESH String, not the receiver.
                        let is_to_string =
                            callee_entry == crate::JitIntrinsic::StringBuilderToString.as_entry();
                        let info_ptr = self
                            .invoke_info_idx
                            .get(&pc)
                            .map(|&i| self.invoke_info[i].1);
                        // The narrow-`value` refusal lives in
                        // `StringBuilderFieldLayout::new`, not here, so
                        // that registration and emission cannot disagree
                        // about it — see that constructor.
                        let layout = self.string_layout.and_then(|l| l.builder);
                        // The `append(String)` argument's OWN field layout —
                        // `java/lang/String`, not the builder — read from the
                        // same `self.string_layout` the builder half came
                        // from. Registration already refused this site when
                        // `has_coder` is false, so a `Some` layout here has
                        // one.
                        let outer_string_layout = self.string_layout;
                        match (info_ptr, layout) {
                            (Some(info), Some(b)) if !info.is_null() => {
                                self.flush_scratch_registers();
                                // Operands, deepest first: receiver, then
                                // (append/charAt/appendString/appendInt
                                // only) the one argument.
                                let arg_slot =
                                    if is_append || is_char_at || is_append_string || is_append_int
                                    {
                                        Some(self.pop_stack())
                                    } else {
                                        None
                                    };
                                let recv_slot = self.pop_stack();

                                let mut decline: Vec<usize> = Vec::new();

                                // Strings round 2 (`str17`): the appends'
                                // growth stubs (`emit_sb_grow_stub`). A full
                                // LATIN1 payload grows through
                                // `DirectHelperTable::sb_grow` and the body
                                // runs again; everything else declines.
                                let grow_helper = if sb_grow_enabled()
                                    && (is_append || is_append_string || is_append_int)
                                {
                                    self.direct_helpers.sb_grow
                                } else {
                                    0
                                };
                                let post_pop = self.next_spill_offset;
                                let mut grow_c: Vec<usize> = Vec::new();
                                let mut grow_l: Vec<usize> = Vec::new();

                                // The builder/String offsets below are a
                                // compile-time claim about two class layouts;
                                // a layout REPLACEMENT bumps the process epoch
                                // and this site then takes the ordinary call.
                                // First, before any operand load: the guard's
                                // fallback form clobbers RCX and R11.
                                if jit_sp_field_layout_guard_enabled() {
                                    decline.extend(self.emit_layout_epoch_guard());
                                }

                                // RAX = receiver; null takes the call.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_test_r64_r64(RAX);
                                decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                // The receiver guard, and it is exact:
                                // `StringBuilder` is final, so a header
                                // class-id match IS that class. This is
                                // what keeps `StringBuffer` — synchronized
                                // methods, a `toStringCache` to invalidate
                                // on every mutation — off a path that
                                // emits neither obligation.
                                //   CMP DWORD [RAX + 0], class_id
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&b.class_id.to_le_bytes());
                                decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                // Matched rather than `expect`ed: this file
                                // denies production panics
                                // (`hot_files_have_no_production_panics`),
                                // and `(is_append, is_char_at)` versus
                                // `arg_slot.is_some()` are the same fact —
                                // the operand is popped under exactly the
                                // condition one of the first two is true.
                                // The wildcard arm cannot be reached in
                                // practice, but the compile fails safely
                                // (drops to the interpreter) rather than
                                // panicking if it ever were.
                                let done = if is_append {
                                    let Some(ch) = arg_slot else {
                                        self.fail("singlepass-codegen/sb-append-no-arg-slot");
                                        return WalkStep::Return(false);
                                    };
                                    // RCX = the char. LATIN1 only: a wider
                                    // one inflates the payload to UTF16,
                                    // which is the native's job.
                                    self.load_slot_to_reg(RCX, ch);
                                    self.buf.emit(&[0x81, 0xF9]); // CMP ECX, imm32
                                    self.buf.emit(&0xFFi32.to_le_bytes());
                                    decline.push(self.emit_jcc_rel32_patch(0x87)); // JA
                                    let grow_save = if grow_helper != 0 {
                                        self.reserve_sb_grow_save(&[recv_slot, ch])
                                    } else {
                                        None
                                    };

                                    // One compact/legacy branch for the
                                    // whole body rather than one per field:
                                    // the body is ten instructions, and a
                                    // per-field test would emit four
                                    // branches over the same header bit.
                                    self.emit_test_mem8_imm8(
                                        RAX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                    let body_c = self.buf.pos();
                                    self.emit_sb_append_char_body(
                                        b.count_compact_offset,
                                        b.value_compact_offset,
                                        b.coder_compact_offset,
                                        b.coder_compact_is_byte,
                                        &mut decline,
                                        grow_save.is_some().then_some(&mut grow_c),
                                    );
                                    let joined = self.emit_jmp_rel32_patch();
                                    self.patch_rel32_to_here(legacy);
                                    let body_l = self.buf.pos();
                                    self.emit_sb_append_char_body(
                                        b.count_legacy_offset,
                                        b.value_legacy_offset,
                                        b.coder_legacy_offset,
                                        false,
                                        &mut decline,
                                        grow_save.is_some().then_some(&mut grow_l),
                                    );
                                    self.patch_rel32_to_here(joined);
                                    // `append` returns its receiver, which
                                    // is still in RAX and was never moved:
                                    // this path allocates nothing (the
                                    // growth stub's helper allocates, but
                                    // cannot collect).
                                    let done = self.emit_jmp_rel32_patch();
                                    if let Some(save) = grow_save {
                                        for (list, retry, count_offset, value_offset) in [
                                            (
                                                std::mem::take(&mut grow_c),
                                                body_c,
                                                b.count_compact_offset,
                                                b.value_compact_offset,
                                            ),
                                            (
                                                std::mem::take(&mut grow_l),
                                                body_l,
                                                b.count_legacy_offset,
                                                b.value_legacy_offset,
                                            ),
                                        ] {
                                            self.emit_sb_grow_stub(
                                                list,
                                                retry,
                                                count_offset,
                                                value_offset,
                                                false,
                                                save,
                                                grow_helper,
                                                &mut decline,
                                            );
                                        }
                                    }
                                    self.next_spill_offset = post_pop;
                                    done
                                } else if is_char_at {
                                    let Some(idx) = arg_slot else {
                                        self.fail("singlepass-codegen/sb-charat-no-arg-slot");
                                        return WalkStep::Return(false);
                                    };
                                    // R9 = the index. `emit_sb_char_at_body`
                                    // reads it and leaves it untouched, so
                                    // loading it once here serves both the
                                    // compact and legacy arm below — same
                                    // shape as `ch` above, one register load
                                    // shared by both layout bodies.
                                    self.load_slot_to_reg(RCX, idx);
                                    self.buf.emit(&[0x49, 0x89, 0xC9]); // MOV R9,RCX

                                    self.emit_test_mem8_imm8(
                                        RAX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                    self.emit_sb_char_at_body(
                                        b.count_compact_offset,
                                        b.value_compact_offset,
                                        b.coder_compact_offset,
                                        b.coder_compact_is_byte,
                                        &mut decline,
                                    );
                                    let joined = self.emit_jmp_rel32_patch();
                                    self.patch_rel32_to_here(legacy);
                                    self.emit_sb_char_at_body(
                                        b.count_legacy_offset,
                                        b.value_legacy_offset,
                                        b.coder_legacy_offset,
                                        false,
                                        &mut decline,
                                    );
                                    self.patch_rel32_to_here(joined);
                                    // `charAt` returns a char derived from
                                    // the receiver, already computed into
                                    // RAX by the body — nothing here
                                    // allocates or moves the receiver.
                                    self.emit_jmp_rel32_patch()
                                } else if is_append_string {
                                    let Some(src) = arg_slot else {
                                        self.fail("singlepass-codegen/sb-appendstring-no-arg-slot");
                                        return WalkStep::Return(false);
                                    };
                                    let Some(outer) = outer_string_layout else {
                                        self.fail(
                                            "singlepass-codegen/sb-appendstring-no-outer-layout",
                                        );
                                        return WalkStep::Return(false);
                                    };
                                    // RDX = src String ref; null takes the
                                    // call — `append(String)` on a null
                                    // argument appends the four characters
                                    // "null", which this path does not
                                    // reproduce.
                                    self.load_slot_to_reg(RDX, src);
                                    self.emit_test_r64_r64(RDX);
                                    decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                    // R8 = src.value ptr (the generic
                                    // String-layout emitter branches on
                                    // RDX's own compact/legacy bit
                                    // internally, so this needs no outer
                                    // branch of its own — unlike the
                                    // builder reads below, which are raw
                                    // per-offset loads).
                                    self.emit_load_string_value_ptr(
                                        R8,
                                        RDX,
                                        outer.value_compact_offset,
                                        outer.value_legacy_offset,
                                    );
                                    self.emit_test_r64_r64(R8);
                                    decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                    // R10D = src.coder.
                                    self.emit_load_string_i32_field(
                                        R10,
                                        RDX,
                                        outer.coder_compact_offset,
                                        outer.coder_compact_is_byte,
                                        outer.coder_legacy_offset,
                                    );

                                    // R9D = src.value.length (bytes) ->
                                    // src.count (chars). Branch-on-the-bit
                                    // rather than a variable `SHR ..,CL`:
                                    // coder is always exactly 0 or 1, and
                                    // this keeps RCX free for the rest of
                                    // this site instead of tying it up as
                                    // the one register a variable shift
                                    // count must live in.
                                    self.buf.emit(&[
                                        0x45,
                                        0x8B,
                                        0x48,
                                        cratonvm_types::ARRAY_LENGTH_OFFSET as u8,
                                    ]); // MOV R9D,[R8+ARRAY_LENGTH_OFFSET]
                                    self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
                                    let src_len_done = self.emit_jcc_rel32_patch(0x84); // JZ
                                    self.buf.emit(&[0x41, 0xD1, 0xE9]); // SHR R9D,1
                                    self.patch_rel32_to_here(src_len_done);
                                    let grow_save = if grow_helper != 0 {
                                        self.reserve_sb_grow_save(&[recv_slot, src])
                                    } else {
                                        None
                                    };

                                    // One compact/legacy branch for the
                                    // whole body, same shape as `append`
                                    // and `charAt` above.
                                    self.emit_test_mem8_imm8(
                                        RAX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                    let body_c = self.buf.pos();
                                    self.emit_sb_append_string_body(
                                        b.count_compact_offset,
                                        b.value_compact_offset,
                                        b.coder_compact_offset,
                                        b.coder_compact_is_byte,
                                        true, // src is a real String's heap byte[]
                                        &mut decline,
                                        grow_save.is_some().then_some(&mut grow_c),
                                    );
                                    let joined = self.emit_jmp_rel32_patch();
                                    self.patch_rel32_to_here(legacy);
                                    let body_l = self.buf.pos();
                                    self.emit_sb_append_string_body(
                                        b.count_legacy_offset,
                                        b.value_legacy_offset,
                                        b.coder_legacy_offset,
                                        false,
                                        true, // src is a real String's heap byte[]
                                        &mut decline,
                                        grow_save.is_some().then_some(&mut grow_l),
                                    );
                                    self.patch_rel32_to_here(joined);
                                    // `append` returns its receiver, still
                                    // in RAX and never moved: this path
                                    // allocates nothing that can collect (a
                                    // same-coder copy or a LATIN1-into-UTF16
                                    // widen, both in place, or a LATIN1
                                    // growth by the stub's TLAB-only helper;
                                    // a narrowing inflate declines).
                                    let done = self.emit_jmp_rel32_patch();
                                    if let Some(save) = grow_save {
                                        for (list, retry, count_offset, value_offset) in [
                                            (
                                                std::mem::take(&mut grow_c),
                                                body_c,
                                                b.count_compact_offset,
                                                b.value_compact_offset,
                                            ),
                                            (
                                                std::mem::take(&mut grow_l),
                                                body_l,
                                                b.count_legacy_offset,
                                                b.value_legacy_offset,
                                            ),
                                        ] {
                                            self.emit_sb_grow_stub(
                                                list,
                                                retry,
                                                count_offset,
                                                value_offset,
                                                true,
                                                save,
                                                grow_helper,
                                                &mut decline,
                                            );
                                        }
                                    }
                                    self.next_spill_offset = post_pop;
                                    done
                                } else if is_append_int {
                                    let Some(val) = arg_slot else {
                                        self.fail("singlepass-codegen/sb-appendint-no-arg-slot");
                                        return WalkStep::Return(false);
                                    };
                                    // A small stack scratch buffer for the
                                    // decimal digits — 2 slots (16 bytes) is
                                    // more than the 10 digits a 32-bit int
                                    // ever needs. Not an argument buffer for
                                    // any call (the fast path never calls
                                    // anything): reusing `HelperArgs`'
                                    // accounting is what keeps this bump
                                    // from colliding with the ACTUAL
                                    // helper-args reservation below, since
                                    // both share one cursor.
                                    //
                                    // Reserved ABOVE the popped operands'
                                    // own frame homes, the way the arraycopy
                                    // intrinsic places its scratch. The two
                                    // pops just rewound the cursor over those
                                    // homes, so a reservation taken at the
                                    // cursor IS them — and the decline edge
                                    // below re-reads `recv_slot`/`val` from
                                    // their homes AFTER the digit loop has
                                    // written the buffer (a capacity miss
                                    // inside the append body declines there),
                                    // so it would have dispatched on digit
                                    // bytes as a receiver.
                                    let post_pop_spill = self.next_spill_offset;
                                    let mut scratch_floor = post_pop_spill;
                                    for slot in [recv_slot, val] {
                                        if let StackSlot::Frame(off) = slot {
                                            scratch_floor =
                                                scratch_floor.max(off.saturating_add(8));
                                        }
                                    }
                                    self.next_spill_offset = scratch_floor;
                                    let Some(scratch_base) =
                                        self.reserve_spill_slots(2, SpillReason::HelperArgs)
                                    else {
                                        self.fail(
                                            "singlepass-codegen/sb-appendint-spill-exhausted",
                                        );
                                        return WalkStep::Return(false);
                                    };
                                    // One past the HIGHEST byte of the two
                                    // reserved words. A word at offset `o`
                                    // occupies `[rbp - o, rbp - o + 8)`, so
                                    // the pair `scratch_base`, `scratch_base
                                    // + 8` spans `[rbp - scratch_base - 8,
                                    // rbp - scratch_base + 8)` and its end is
                                    // the frame address of offset
                                    // `scratch_base - 8`. This used to be
                                    // `scratch_base + 16` — 24 bytes below
                                    // that end and 8 below the reservation's
                                    // bottom — so every digit landed in
                                    // UNRESERVED words beneath the cursor:
                                    // an open inline scope's locals, or, with
                                    // the spill area nearly full, the
                                    // callee-saved register save area that
                                    // sits directly past it
                                    // (`callee_saved_base`).
                                    let digits_end = scratch_base - 8;

                                    // Stash the receiver: RAX is the DIV
                                    // dividend/quotient register, and the
                                    // digit loop below runs one DIV per
                                    // digit. Without this, RAX comes out of
                                    // the loop holding the LAST quotient —
                                    // always 0, since the loop keeps
                                    // dividing until the quotient IS 0 —
                                    // and `emit_sb_append_string_body` below
                                    // would then dereference a null
                                    // receiver. (Measured: this was a real
                                    // bug, not a hypothetical one — SIGSEGV
                                    // addr=0xf, rax=0x0 at the crash, `0xf`
                                    // being one of the builder's own small
                                    // field offsets applied to a null base.)
                                    self.buf.emit(&[0x49, 0x89, 0xC0]); // MOV R8,RAX

                                    // ECX = the int. Negative declines: the
                                    // digit loop below is UNSIGNED divide,
                                    // so a negative value would read as its
                                    // huge unsigned reinterpretation rather
                                    // than as a negative number, and it is
                                    // the ordinary call's job to print the
                                    // sign anyway.
                                    self.load_slot_to_reg(RCX, val);
                                    // JIT round 16 (lane strings, STR16-3,
                                    // `CRATONVM_JIT_SB_APPEND_INT_NEGATIVE`):
                                    // serve a negative value as its magnitude
                                    // plus a sign byte instead of declining.
                                    // R10D keeps the original value for the
                                    // sign test after the loop (R10 is free
                                    // until the `XOR R10D,R10D` below, and the
                                    // decline edge re-reads `val` from its
                                    // home, so nothing else needs it).
                                    let serve_negative = true;
                                    if serve_negative {
                                        // MOV R10D,ECX ; TEST ECX,ECX ; JNS ;
                                        // NEG ECX — `Integer.MIN_VALUE` stays
                                        // 0x8000_0000, which the UNSIGNED
                                        // loop renders as 2147483648.
                                        self.buf.emit(&[0x41, 0x89, 0xCA]);
                                        self.buf.emit(&[0x85, 0xC9]);
                                        let non_negative = self.emit_jcc_rel32_patch(0x89);
                                        self.buf.emit(&[0xF7, 0xD9]);
                                        self.patch_rel32_to_here(non_negative);
                                    } else {
                                        // TEST ECX,ECX ; JS decline
                                        self.buf.emit(&[0x85, 0xC9]);
                                        decline.push(self.emit_jcc_rel32_patch(0x88));
                                    }

                                    // R11 = write cursor, one past the last
                                    // reserved byte; decremented before
                                    // every store so it always points at
                                    // the last-written (i.e. so-far-most-
                                    // significant) digit.
                                    self.emit_lea_frame_slot(R11, digits_end);
                                    // Strings round 2 (`str17`): the
                                    // quotient by reciprocal multiply, not
                                    // `DIV`. `q = (x * 0xCCCCCCCD) >> 35` is
                                    // `x / 10` for every unsigned 32-bit `x`
                                    // (the product fits 64 bits), and it puts
                                    // ~4 cycles a digit on the loop-carried
                                    // chain where `DIV r32` put ~12 (Zen 5):
                                    // two 7-digit ints an iteration of
                                    // `R16Bench strings`.
                                    // JIT r22 wave 1, lane intrinsics
                                    // (INTRINSICS21-3,
                                    // `CRATONVM_JIT_SB_APPEND_INT_PAIRS`,
                                    // opt-in): while `x >= 100`, TWO digits a
                                    // step -- `q = (x * 0x51EB851F) >> 37` is
                                    // `x / 100` for every unsigned 32-bit `x`,
                                    // `r = x - 100 q`, the tens digit
                                    // `(r * 103) >> 10` (exact for `r < 179`)
                                    // and the units `r - 10 * tens` -- so the
                                    // loop-carried chain pays one multiply
                                    // per two digits. The remaining 1..2
                                    // digits (or the single `0`) take the
                                    // one-digit loop below, so the bytes
                                    // written are the same.
                                    if crate::sb_append_int_pairs_enabled() {
                                        self.emit_mov_r32_imm32_zx(R9, 0x51EB_851F); // reciprocal of 100
                                        let pair_loop = self.buf.pos();
                                        self.buf.emit(&[0x83, 0xF9, 0x64]); // CMP ECX,100
                                        let pairs_done = self.emit_jcc_rel32_patch(0x82); // JB
                                        self.buf.emit(&[0x89, 0xC8]); // MOV EAX,ECX
                                        self.buf.emit(&[0x49, 0x0F, 0xAF, 0xC1]); // IMUL RAX,R9
                                        self.buf.emit(&[0x48, 0xC1, 0xE8, 0x25]); // SHR RAX,37 (EAX=q)
                                        self.buf.emit(&[0x6B, 0xD0, 0x64]); // IMUL EDX,EAX,100
                                        self.buf.emit(&[0x29, 0xD1]); // SUB ECX,EDX (ECX=r)
                                        self.buf.emit(&[0x6B, 0xD1, 0x67]); // IMUL EDX,ECX,103
                                        self.buf.emit(&[0xC1, 0xEA, 0x0A]); // SHR EDX,10 (tens)
                                        self.buf.emit(&[0x83, 0xC2, 0x30]); // ADD EDX,'0'
                                        self.buf.emit(&[0x41, 0x88, 0x53, 0xFE]); // MOV [R11-2],DL
                                        // ECX = r - 10*(tens + '0') + 10*'0' + '0'
                                        //     = units + '0'.
                                        self.buf.emit(&[0x8D, 0x14, 0x92]); // LEA EDX,[RDX+RDX*4]
                                        self.buf.emit(&[0x01, 0xD2]); // ADD EDX,EDX
                                        self.buf.emit(&[0x29, 0xD1]); // SUB ECX,EDX
                                        self.buf.emit(&[0x81, 0xC1, 0x10, 0x02, 0x00, 0x00]); // ADD ECX,0x210
                                        self.buf.emit(&[0x41, 0x88, 0x4B, 0xFF]); // MOV [R11-1],CL
                                        self.buf.emit(&[0x49, 0x83, 0xEB, 0x02]); // SUB R11,2
                                        self.buf.emit(&[0x89, 0xC1]); // MOV ECX,EAX
                                        self.emit_jmp_back_to(pair_loop);
                                        self.patch_rel32_to_here(pairs_done);
                                    }
                                    self.emit_mov_r32_imm32_zx(R9, 0xCCCC_CCCD); // reciprocal of 10

                                    let digit_loop = self.buf.pos();
                                    self.buf.emit(&[0x49, 0xFF, 0xCB]); // DEC R11
                                    self.buf.emit(&[0x89, 0xC8]); // MOV EAX,ECX
                                    self.buf.emit(&[0x49, 0x0F, 0xAF, 0xC1]); // IMUL RAX,R9
                                    self.buf.emit(&[0x48, 0xC1, 0xE8, 0x23]); // SHR RAX,35 (EAX=q)
                                    self.buf.emit(&[0x8D, 0x14, 0x80]); // LEA EDX,[RAX+RAX*4]
                                    self.buf.emit(&[0x01, 0xD2]); // ADD EDX,EDX (q*10)
                                    self.buf.emit(&[0x29, 0xD1]); // SUB ECX,EDX (ECX=r)
                                    self.buf.emit(&[0x83, 0xC1, 0x30]); // ADD ECX,0x30 ('0')
                                    self.buf.emit(&[0x41, 0x88, 0x0B]); // MOV [R11],CL
                                    self.buf.emit(&[0x89, 0xC1]); // MOV ECX,EAX
                                    self.buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
                                    let back = self.emit_jcc_rel32_patch(0x85); // JNZ
                                    let rel = digit_loop as i32 - (back as i32 + 4); // Cast: rel32 displacement
                                    self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails

                                    if serve_negative {
                                        // The sign, in front of the most
                                        // significant digit. Ten digits plus
                                        // one sign is 11 of the 16 reserved
                                        // bytes.
                                        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
                                        let unsigned_done = self.emit_jcc_rel32_patch(0x89); // JNS
                                        self.buf.emit(&[0x49, 0xFF, 0xCB]); // DEC R11
                                        self.buf.emit(&[0x41, 0xC6, 0x03, b'-']); // MOV BYTE [R11],'-'
                                        self.patch_rel32_to_here(unsigned_done);
                                    }

                                    // RAX = buffer end, recomputed (cheaper
                                    // than keeping it alive across the loop
                                    // above, which needed EAX itself for
                                    // every division). R9D = digit count =
                                    // buffer_end - R11 (both frame
                                    // addresses a few bytes apart, so the
                                    // 32-bit truncation is exact).
                                    self.emit_lea_frame_slot(RAX, digits_end);
                                    self.buf.emit(&[0x49, 0x89, 0xC1]); // MOV R9,RAX
                                    self.buf.emit(&[0x4D, 0x29, 0xD9]); // SUB R9,R11
                                                                        // RAX = the receiver, restored.
                                    self.buf.emit(&[0x4C, 0x89, 0xC0]); // MOV RAX,R8
                                                                        // R8 = the first (most-significant) digit.
                                    self.buf.emit(&[0x4D, 0x89, 0xD8]); // MOV R8,R11
                                                                        // R10D = 0 — the scratch buffer's
                                                                        // "coder" is always LATIN1: every
                                                                        // digit and the sign are pure ASCII.
                                    self.emit_xor_reg_self(R10);
                                    // Above the digit buffer, which R8
                                    // points into across the growth call.
                                    let grow_save = if grow_helper != 0 {
                                        self.reserve_sb_grow_save(&[])
                                    } else {
                                        None
                                    };

                                    // From here it is exactly `append(String)`'s
                                    // tail: R8/R9D/R10D already hold what
                                    // that body expects a source String's
                                    // value ptr/count/coder to be, and it
                                    // never re-reads the source object
                                    // itself, so a scratch buffer serves
                                    // just as well as a real `String`.
                                    self.emit_test_mem8_imm8(
                                        RAX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                    let body_c = self.buf.pos();
                                    self.emit_sb_append_string_body(
                                        b.count_compact_offset,
                                        b.value_compact_offset,
                                        b.coder_compact_offset,
                                        b.coder_compact_is_byte,
                                        false, // src is the stack scratch buffer, no header
                                        &mut decline,
                                        grow_save.is_some().then_some(&mut grow_c),
                                    );
                                    let joined = self.emit_jmp_rel32_patch();
                                    self.patch_rel32_to_here(legacy);
                                    let body_l = self.buf.pos();
                                    self.emit_sb_append_string_body(
                                        b.count_legacy_offset,
                                        b.value_legacy_offset,
                                        b.coder_legacy_offset,
                                        false,
                                        false, // src is the stack scratch buffer, no header
                                        &mut decline,
                                        grow_save.is_some().then_some(&mut grow_l),
                                    );
                                    self.patch_rel32_to_here(joined);
                                    let done = self.emit_jmp_rel32_patch();
                                    if let Some(save) = grow_save {
                                        for (list, retry, count_offset, value_offset) in [
                                            (
                                                std::mem::take(&mut grow_c),
                                                body_c,
                                                b.count_compact_offset,
                                                b.value_compact_offset,
                                            ),
                                            (
                                                std::mem::take(&mut grow_l),
                                                body_l,
                                                b.count_legacy_offset,
                                                b.value_legacy_offset,
                                            ),
                                        ] {
                                            self.emit_sb_grow_stub(
                                                list,
                                                retry,
                                                count_offset,
                                                value_offset,
                                                true,
                                                save,
                                                grow_helper,
                                                &mut decline,
                                            );
                                        }
                                    }
                                    self.next_spill_offset = post_pop_spill;
                                    done
                                } else if is_to_string {
                                    // JIT round 17 wave 1, lane strings
                                    // (STR16-P1): RAX = the fresh String on
                                    // the fall-through; every refusal is a
                                    // decline edge (see the emitter).
                                    let Some(outer) = outer_string_layout else {
                                        self.fail("singlepass-codegen/sb-tostring-no-outer-layout");
                                        return WalkStep::Return(false);
                                    };
                                    self.emit_sb_to_string_latin1_body(&b, &outer, &mut decline);
                                    self.emit_jmp_rel32_patch()
                                } else {
                                    // length() is `count`, sign-extended
                                    // into the 64-bit operand slot the same
                                    // way `String.length()` ends.
                                    self.emit_load_string_i32_field(
                                        RAX,
                                        RAX,
                                        b.count_compact_offset,
                                        false,
                                        b.count_legacy_offset,
                                    );
                                    self.emit_jmp_rel32_patch()
                                };

                                // ---- decline edge: the unchanged dispatch
                                for p in decline {
                                    self.patch_rel32_to_here(p);
                                }
                                let nargs = if arg_slot.is_some() { 2 } else { 1 };
                                let args_base = match self
                                    .reserve_spill_slots(nargs, SpillReason::HelperArgs)
                                {
                                    Some(base) => base,
                                    None => {
                                        self.fail("singlepass-codegen/sb-args-spill-exhausted");
                                        return WalkStep::Return(false);
                                    }
                                };
                                // `jit_invoke_dispatch`'s buffer runs
                                // arg[0] at the HIGHEST offset down. Both
                                // operands are loaded before either is
                                // stored: the buffer can overlap the
                                // operand homes.
                                self.load_slot_to_reg(RAX, recv_slot);
                                if let Some(arg) = arg_slot {
                                    self.load_slot_to_reg(RCX, arg);
                                }
                                // Cast: an argument count of 1 or 2.
                                let top = args_base + (nargs as i32 - 1) * 8;
                                self.emit_store_local(top, RAX);
                                if arg_slot.is_some() {
                                    self.emit_store_local(top - 8, RCX);
                                }
                                self.emit_load_vm_context(ARG_REGS[0]);
                                self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64);
                                self.emit_lea_frame_slot(ARG_REGS[2], top);
                                self.emit_mov_imm32_sx(ARG_REGS[3], nargs as i32);
                                self.emit_pre_safepoint_spill_before_helper_call();
                                // (vm_ptr, info_ptr, args_ptr, num_args) -> result | sentinel.
                                cratonvm_jit_api::assert_helper_call_shape!(
                                    "invoke_dispatch",
                                    int_args = 4,
                                    returns_value = true
                                );
                                self.emit_call_absolute(self.helpers.invoke_dispatch);
                                self.emit_oop_map_for_safepoint();
                                // The three `append`s return a reference;
                                // `length` and `charAt` return an
                                // int-category value (a `char` is widened to
                                // `I` on the operand stack). None of them
                                // shares the `i64::MIN` pending-exception
                                // sentinel with a legitimate value.
                                let returns_builder =
                                    is_append || is_append_string || is_append_int;
                                // `toString()` returns a reference too: a
                                // fresh String, on both edges (round 17).
                                let returns_ref = returns_builder || is_to_string;
                                self.emit_post_invoke_exception_check(if returns_ref {
                                    b'L'
                                } else if is_char_at {
                                    b'C'
                                } else {
                                    b'I'
                                });

                                // ---- join -----------------------------
                                self.patch_rel32_to_here(done);
                                self.next_spill_offset = args_base;
                                self.push_from_rax();
                                if returns_ref {
                                    // The builder it returns is the builder
                                    // it was handed (round 12 wave 2 hunter:
                                    // append(String) / append(int) too), or
                                    // `toString()`'s fresh String.
                                    self.mark_top_as_oop();
                                }
                                intrinsic_handled = true;
                            }
                            // Falling through is NOT safe: the site carries
                            // an intrinsic SENTINEL as its
                            // `JitDirectCall::entry`, and the ordinary
                            // direct-call path would CALL that sentinel.
                            // Registration and emission must agree, so a
                            // shape that cannot be emitted fails the
                            // compile and drops the method to the
                            // interpreter.
                            _ => {
                                self.fail("singlepass-codegen/sb-intrinsic-unemittable");
                                return WalkStep::Return(false);
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: STRINGBUILDER_ACCESS =====

                    // ===== INTRINSIC REGION BEGIN: CODE_POINT =====
                    // JIT r23 wave 1, lane L5 (INTRINSICS22-1):
                    // `String.codePointAt(int)` and
                    // `StringBuilder.appendCodePoint(int)`, both declining to
                    // the ordinary dispatch on a miss
                    // (`x64/string_code_point.rs`). Registration and emission
                    // must agree, so a shape that cannot be emitted fails the
                    // compile rather than falling through to `CALL` the
                    // sentinel.
                    if !intrinsic_handled {
                        match self.emit_sp_code_point_intrinsic(pc, callee_entry) {
                            None => {}
                            Some(Ok(())) => intrinsic_handled = true,
                            Some(Err(site)) => {
                                self.fail(site);
                                return WalkStep::Return(false);
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: CODE_POINT =====

                    // ===== INTRINSIC REGION BEGIN: STRING_ACCESS =====
                    // java.lang.String access intrinsics (Phase 3a):
                    // length()I, isEmpty()Z, charAt(I)C, hashCode()I.
                    //
                    // These are registered by `try_resolve_string_intrinsic`
                    // ONLY when a `StringFieldLayout` with a `coder` field
                    // resolved for this compilation; that same layout is in
                    // `self.string_layout`. The defensive `if let Some` below
                    // therefore always matches when a String sentinel is
                    // seen — but if it somehow does not (layout unexpectedly
                    // absent), `intrinsic_handled` stays false and control
                    // falls through to the normal direct-call path, so the
                    // intrinsic sentinel is never mis-`CALL`ed.
                    //
                    // String representation (compact): `value` is a `byte[]`,
                    // `coder` is 0 (LATIN1, 1 byte/char) or 1 (UTF16, 2 LE
                    // bytes/char). `length() == value.length >> coder`.
                    //
                    // Every uncertain case — null receiver, null `value`
                    // array, charAt index out of bounds — branches to a
                    // shared uncommon-trap deopt stub (reason 6) which
                    // re-runs the whole method in the interpreter; the
                    // native `lang_string.rs` impl then reproduces the
                    // exact NPE / StringIndexOutOfBoundsException / value
                    // semantics. No `CALL` is emitted on the inline path.
                    if let Some(layout) = self.string_layout {
                        let acc = if callee_entry == crate::JitIntrinsic::StringLength.as_entry() {
                            Some(0u8)
                        } else if callee_entry == crate::JitIntrinsic::StringIsEmpty.as_entry() {
                            Some(1)
                        } else if callee_entry == crate::JitIntrinsic::StringCharAt.as_entry() {
                            Some(2)
                        } else if callee_entry == crate::JitIntrinsic::StringHashCode.as_entry() {
                            Some(3)
                        } else {
                            None
                        };
                        if let Some(kind) = acc {
                            self.flush_scratch_registers();
                            // Step 6: snapshot before arg pops so a null-
                            // receiver / class-id guard bail resumes at the
                            // invokevirtual bci (receiver [+ index] on stack).
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            let mut bail: Vec<usize> = Vec::new();

                            // Pop operands. charAt has an index arg
                            // (shallower); the receiver is always deepest.
                            let index_slot = if kind == 2 {
                                Some(self.pop_stack())
                            } else {
                                None
                            };
                            let recv_slot = self.pop_stack();

                            // Compile-time String offsets: guard the layout
                            // epoch first (its fallback clobbers RCX/R11).
                            if jit_sp_field_layout_guard_enabled() {
                                bail.extend(self.emit_layout_epoch_guard());
                            }

                            // RAX = receiver. Null receiver → the NPE (JIT
                            // round 16, lane throwpath, TP-2), or a deopt.
                            self.load_slot_to_reg(RAX, recv_slot);
                            let null_arm = self.string_intrinsic_null_receiver_raises_npe(pc);
                            // Wave 12 (throw3): a protected site under precise
                            // frames raises through the reason-10 stub, which
                            // publishes this bci's frame; `false` (the stub
                            // could not be filed here) keeps the deopt below.
                            // Wave 13: with the site's trap key, so the NPE's
                            // snapshot names this bci (the JEP 358 message).
                            let precise_raised = null_arm == StringNullReceiver::PreciseFrame && {
                                let trap_key =
                                    crate::x64::inlining::record_npe_trap_site(self.orig_bci(pc));
                                self.emit_precise_npe_check_keyed(
                                    pc,
                                    npe_action::INVOKE_RECEIVER,
                                    trap_key,
                                )
                            };
                            if precise_raised {
                                // The reason-10 stub raises it; nothing more.
                            } else if null_arm == StringNullReceiver::SharedStub {
                                // The shared null-check stub: `jit_npe_with_action`
                                // with the site's trap key (the exact JEP 358
                                // text, `Cannot invoke "String.length()" ...`),
                                // then this method's own `catch` through the
                                // implicit local-handler probe, else the
                                // sentinel. The deopt below re-ran the rest of
                                // the activation in the interpreter for every
                                // null receiver (`ReceiverTypeChanged`, action
                                // `RecompileAndReinterpret`), and a recompiled
                                // body kept the same arm: a caught NPE in a
                                // compiled loop interpreted the loop's tail.
                                let trap_key =
                                    crate::x64::inlining::record_npe_trap_site(self.orig_bci(pc));
                                self.emit_null_check_array_load(
                                    npe_action::INVOKE_RECEIVER,
                                    trap_key,
                                    pc,
                                );
                            } else {
                                self.emit_test_r64_r64(RAX);
                                bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                            }

                            // Receiver class-id guard. For a
                            // `java/lang/String` call site (final →
                            // monomorphic) `guard_class_id == 0` and no
                            // guard is emitted. For a `java/lang/CharSequence`
                            // site the receiver may be any CharSequence, so
                            // this inline String-layout decode is valid only
                            // when the receiver is actually a String: compare
                            // the ObjectHeader class id at [recv+0] against
                            // the String class id and deopt (→ native
                            // dispatch) on a mismatch (e.g. a StringBuilder /
                            // StringBuffer receiver). Same guard the CRC32
                            // family uses; see its STRING_SEARCH-adjacent
                            // region below.
                            if guard_class_id != 0 {
                                // r11 x64gen: ARRAY RECEIVERS FIRST. The
                                // guarded sites include `Object.hashCode()`
                                // (`object_hashcode_string_guard`), whose
                                // receiver can be an array, and a reference
                                // array's header holds its COMPONENT's class
                                // id — so a `String[]` passed the class-id
                                // compare below and was hashed as a String
                                // (its element words read as `value`/`coder`/
                                // `hash`), where Java answers the identity
                                // hash. The same KIND_TAGS screen the MIC/PIC
                                // cascades and the inline typechecks make.
                                //   TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK (F6 /0 ib: kind bits only)
                                self.buf.emit(&[
                                    0xF6,
                                    0x40,
                                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                                    cratonvm_types::KIND_TAG_BYTE_MASK,
                                ]);
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                                            // CMP DWORD [RAX + 0], guard_class_id
                                                                            //   81 /7 id, ModRM 0x78 = mod01 (disp8 0) /7 rm=RAX.
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                            }

                            // RCX = value (byte[]) ref. Null → deopt.
                            self.emit_load_string_value_ptr(
                                RCX,
                                RAX,
                                layout.value_compact_offset,
                                layout.value_legacy_offset,
                            );
                            self.emit_test_r64_r64(RCX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                            // R10D = coder (0 LATIN1 / 1 UTF16).
                            self.emit_load_string_i32_field(
                                R10,
                                RAX,
                                layout.coder_compact_offset,
                                layout.coder_compact_is_byte,
                                layout.coder_legacy_offset,
                            );

                            if kind == 3 {
                                // hashCode(): first read the cached `hash`
                                // int. A non-zero cache is the result —
                                // matches the native lazy cache. (A zero
                                // cache, or an empty string, recomputes,
                                // and a non-zero result is written back
                                // below — STR16-1: the cache IS observable,
                                // by reflection and by an O(n) re-hash on
                                // every call of a compiled-only String.)
                                self.emit_load_string_i32_field(
                                    RAX,
                                    RAX,
                                    layout.hash_compact_offset,
                                    false,
                                    layout.hash_legacy_offset,
                                );
                                // TEST EAX,EAX ; JNZ cached_done
                                self.buf.emit(&[0x85, 0xC0]);
                                let cached_done = self.emit_jcc_rel32_patch(0x85); // JNZ

                                // JIT round 19 proposal wave 6, lane str6
                                // (STRGAP19-6, `CRATONVM_JIT_STRING_HASH_IS_ZERO`,
                                // opt-in): `String.hashCode`'s
                                // `h == 0 && !hashIsZero` test. A COMPACT
                                // receiver whose `hashIsZero` is set answers
                                // 0 (EAX already holds the zero `hash`)
                                // without the O(n) loop. RCX / R10 survive.
                                let hash_is_zero_off = layout
                                    .hash_is_zero_compact_offset
                                    .filter(|_| crate::string_hash_is_zero_enabled());
                                let mut hash_is_zero_done: Option<usize> = None;
                                if let Some(hz) = hash_is_zero_off {
                                    self.load_slot_to_reg(RDX, recv_slot);
                                    self.emit_test_mem8_imm8(
                                        RDX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                    self.buf.emit(&[0x80, 0xBA]); // CMP BYTE [RDX+disp32], 0
                                    self.buf.emit(&hz.to_le_bytes());
                                    self.buf.emit(&[0x00]);
                                    hash_is_zero_done = Some(self.emit_jcc_rel32_patch(0x85)); // JNE
                                    self.patch_rel32_to_here(legacy);
                                }

                                // Recompute: R11D = value.length (zero-extended), RCX = value.
                                // Hoist coder check out of the loop and run the 4-way unrolled
                                // polynomial fold for LATIN1 (`coder == 0`).
                                self.buf
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    .emit(&[0x44, 0x8B, 0x59, ARRAY_LENGTH_OFFSET as u8]); // MOV R11D,[RCX+len]
                                self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
                                let utf16_arm = self.emit_jcc_rel32_patch(0x85); // JNZ utf16
                                super::concat_emit::emit_latin1_hash_unrolled4(
                                    &mut self.buf,
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    HEADER_SIZE as u8,
                                );
                                let latin1_done = self.emit_jmp_rel32_patch();

                                // UTF16 arm: RDX = value, R11D = value.len >> coder.
                                self.patch_rel32_to_here(utf16_arm);
                                self.buf.emit(&[0x48, 0x89, 0xCA]); // MOV RDX,RCX
                                self.buf.emit(&[0x44, 0x89, 0xD1]); // MOV ECX,R10D
                                self.buf.emit(&[0x41, 0xD3, 0xEB]); // SHR R11D,CL
                                self.emit_xor_reg_self(RAX);
                                self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D
                                let loop_top = self.buf.pos();
                                self.buf.emit(&[0x45, 0x39, 0xD8]); // CMP R8D,R11D
                                let loop_done = self.emit_jcc_rel32_patch(0x8D); // JGE
                                self.buf.emit(&[
                                    0x42,
                                    0x0F,
                                    0xB7,
                                    0x4C,
                                    0x42,
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    HEADER_SIZE as u8,
                                ]);
                                self.buf.emit(&[0x6B, 0xC0, 0x1F]); // IMUL EAX,EAX,31
                                self.buf.emit(&[0x01, 0xC8]); // ADD EAX,ECX
                                self.buf.emit(&[0x41, 0xFF, 0xC0]); // INC R8D
                                let back = self.emit_jmp_rel32_patch();
                                // Cast: value to i32 (encoding immediate/displacement)
                                let rel = loop_top as i32 - (back as i32 + 4);
                                self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                self.patch_rel32_to_here(loop_done);
                                self.patch_rel32_to_here(latin1_done);
                                // JIT round 16 (lane strings, STR16-1): write
                                // a NON-ZERO computed hash back into the
                                // receiver's `hash` field, exactly as
                                // `String.hashCode()`'s own body does
                                // (`if (h == 0) hashIsZero = true; else hash
                                // = h;`). Without it a String hashed only by
                                // compiled code re-ran this O(n) loop on
                                // every call — MEASURED 3 648 ns/op against
                                // HotSpot's 3 for a fresh 1 000-char key
                                // (`R16StringsSplit` row `hashLong`) — and
                                // the field stayed 0 where HotSpot's holds
                                // the hash (reflection-visible:
                                // `R16StringsIdentity`). A plain `int` store:
                                // no reference, so no barrier. COMPACT
                                // instances only; a legacy 16-byte `Value`
                                // cell keeps the old recompute behaviour.
                                // `hashIsZero` is not written: a zero hash
                                // recomputes, as before.
                                if let Some(hz) = hash_is_zero_off {
                                    // STRGAP19-6: `if (h == 0) hashIsZero =
                                    // true; else hash = h;`, COMPACT receivers
                                    // only, both plain stores of a primitive
                                    // (no barrier), as the JDK body's.
                                    self.buf.emit(&[0x85, 0xC0]); // TEST EAX,EAX
                                    let nonzero = self.emit_jcc_rel32_patch(0x85); // JNZ
                                    self.load_slot_to_reg(RDX, recv_slot);
                                    self.emit_test_mem8_imm8(
                                        RDX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    let legacy_zero = self.emit_jcc_rel32_patch(0x84); // JZ
                                    self.buf.emit(&[0xC6, 0x82]); // MOV BYTE [RDX+disp32], 1
                                    self.buf.emit(&hz.to_le_bytes());
                                    self.buf.emit(&[0x01]);
                                    self.patch_rel32_to_here(legacy_zero);
                                    let zero_done = self.emit_jmp_rel32_patch();
                                    self.patch_rel32_to_here(nonzero);
                                    {
                                        self.load_slot_to_reg(RDX, recv_slot);
                                        self.emit_test_mem8_imm8(
                                            RDX,
                                            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                            cratonvm_types::GC_FLAG_COMPACT,
                                        );
                                        let legacy_hash = self.emit_jcc_rel32_patch(0x84);
                                        self.buf.emit(&[0x89, 0x82]);
                                        self.buf.emit(&layout.hash_compact_offset.to_le_bytes());
                                        self.patch_rel32_to_here(legacy_hash);
                                    }
                                    self.patch_rel32_to_here(zero_done);
                                } else if true {
                                    // TEST EAX,EAX ; JZ skip
                                    self.buf.emit(&[0x85, 0xC0]);
                                    let zero_hash = self.emit_jcc_rel32_patch(0x84);
                                    // RDX = receiver. The operands were flushed
                                    // to their frame homes at the top of this
                                    // site, so this reload is the receiver.
                                    self.load_slot_to_reg(RDX, recv_slot);
                                    self.emit_test_mem8_imm8(
                                        RDX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    // JZ (legacy) past the store:
                                    // MOV DWORD [RDX + hash_compact_offset], EAX
                                    //   89 /r, ModRM 0x82 = mod10 (disp32) reg=EAX rm=RDX.
                                    let legacy_hash = self.emit_jcc_rel32_patch(0x84);
                                    self.buf.emit(&[0x89, 0x82]);
                                    self.buf.emit(&layout.hash_compact_offset.to_le_bytes());
                                    self.patch_rel32_to_here(legacy_hash);
                                    self.patch_rel32_to_here(zero_hash);
                                }
                                self.patch_rel32_to_here(cached_done);
                                if let Some(p) = hash_is_zero_done {
                                    self.patch_rel32_to_here(p);
                                }
                                // Result (EAX) is sign-extended on push.
                                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                                self.push_from_rax();
                            } else if kind == 2 {
                                // charAt(I)C. Register plan:
                                //   R8  = value ptr   (RCX freed for CL)
                                //   R9  = index       (survives SHR)
                                //   R11 = char_count
                                //   R10 = coder
                                if let Some(idx) = index_slot {
                                    self.load_slot_to_reg(R9, idx);
                                }
                                // R11D = value.length (zero-extended) —
                                // read BEFORE freeing RCX.
                                self.buf
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    .emit(&[0x44, 0x8B, 0x59, ARRAY_LENGTH_OFFSET as u8]); // MOV R11D,[RCX+12]
                                                                                           // R8 = value ptr (so CL can use RCX).
                                self.buf.emit(&[0x49, 0x89, 0xC8]); // MOV R8,RCX
                                                                    // char_count = value.length >> coder.
                                                                    // MOV ECX,R10D ; SHR R11D,CL
                                self.buf.emit(&[0x44, 0x89, 0xD1]);
                                self.buf.emit(&[0x41, 0xD3, 0xEB]);
                                // Bounds: (unsigned) index >= char_count
                                // → deopt (also catches negative index).
                                // CMP R9D, R11D ; JAE deopt
                                self.buf.emit(&[0x45, 0x39, 0xD9]);
                                bail.push(self.emit_jcc_rel32_patch(0x83));
                                // Decode: coder branch. R10D = coder,
                                // R9 = index, R8 = value ptr.
                                self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
                                let utf16 = self.emit_jcc_rel32_patch(0x85);
                                // LATIN1: MOVZX EAX, BYTE [R8+R9*1+40]
                                self.buf.emit(&[
                                    0x43,
                                    0x0F,
                                    0xB6,
                                    0x44,
                                    0x08,
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    HEADER_SIZE as u8,
                                ]);
                                let dec_done = self.emit_jmp_rel32_patch();
                                // UTF16: MOVZX EAX, WORD [R8+R9*2+40]
                                self.patch_rel32_to_here(utf16);
                                self.buf.emit(&[
                                    0x43,
                                    0x0F,
                                    0xB7,
                                    0x44,
                                    0x48,
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    HEADER_SIZE as u8,
                                ]);
                                self.patch_rel32_to_here(dec_done);
                                // char result already zero-extended in EAX.
                                self.push_from_rax();
                            } else {
                                // length()I (kind 0) / isEmpty()Z (kind 1).
                                // EAX = value.length (zero-extended).
                                // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                self.buf.emit(&[0x8B, 0x41, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[RCX+12]
                                                                                         // MOV ECX,R10D ; SHR EAX,CL  → char count.
                                self.buf.emit(&[0x44, 0x89, 0xD1]);
                                self.buf.emit(&[0xD3, 0xE8]);
                                if kind == 1 {
                                    // isEmpty: EAX = (char_count == 0).
                                    // TEST EAX,EAX ; SETE AL ; MOVZX EAX,AL
                                    self.buf.emit(&[0x85, 0xC0]);
                                    self.buf.emit(&[0x0F, 0x94, 0xC0]);
                                    self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                                } else {
                                    // length: sign-extend the int result.
                                    self.buf.emit(&[0x48, 0x63, 0xC0]);
                                }
                                self.push_from_rax();
                            }

                            // Wire every bail edge to one shared
                            // uncommon-trap stub (reason 6).
                            for p in bail {
                                self.deopt_stubs.push((p, pc, 6));
                            }
                            intrinsic_handled = true;
                        }
                    }
                    // ===== INTRINSIC REGION END: STRING_ACCESS =====

                    // ===== INTRINSIC REGION BEGIN: STRING_SEARCH =====
                    // java.lang.String search intrinsics (Phase 3b):
                    // equals(Ljava/lang/Object;)Z, compareTo(String)I,
                    // indexOf(I)I and indexOf(String)I.
                    //
                    // `equals` strategy. The receiver is a `java/lang/String`
                    // (monomorphic — String is final). The emitted code:
                    //   * this == null              → deopt (E27-1 N3-obs:
                    //                                the intrinsic bypasses
                    //                                ordinary virtual
                    //                                dispatch, so nothing
                    //                                upstream has null-
                    //                                checked the receiver
                    //                                yet — `compareTo` and
                    //                                `indexOf` already guard
                    //                                this and `equals` must
                    //                                too, or `((String)
                    //                                null).equals(x)`
                    //                                dereferences the null
                    //                                page at [RAX] instead
                    //                                of raising an NPE)
                    //   * other == null            → result 0 (false)
                    //   * this.ptr == other.ptr    → result 1 (true)
                    //   * other's ObjectHeader class id != this's
                    //                              → deopt (non-String
                    //                                argument: native
                    //                                equals returns false)
                    //   * this.value/other.value null   → deopt
                    //   * this.coder != other.coder     → deopt (rare;
                    //                                native compares the
                    //                                decoded char slices)
                    //   * value-array lengths differ    → result 0
                    //   * else REP CMPSB over the bytes → 1 iff identical
                    // Same coder + identical backing bytes ⇒ identical
                    // decoded strings, so the raw byte compare is exact.
                    // No `CALL` on the inline path; every deopt edge re-runs
                    // the method in the interpreter (native `equals`).
                    if let (Some(layout), true) = (
                        self.string_layout,
                        callee_entry == crate::JitIntrinsic::StringEquals.as_entry(),
                    ) {
                        self.flush_scratch_registers();
                        // Step 6: snapshot (this, other) before pops.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        let mut bail: Vec<usize> = Vec::new();

                        // Operand stack (deepest first): this, other.
                        let other_slot = self.pop_stack();
                        let this_slot = self.pop_stack();

                        // Compile-time String offsets: guard the layout
                        // epoch first (its fallback clobbers RCX/R11).
                        if jit_sp_field_layout_guard_enabled() {
                            bail.extend(self.emit_layout_epoch_guard());
                        }

                        // RAX = this, RDX = other.
                        self.load_slot_to_reg(RAX, this_slot);
                        self.load_slot_to_reg(RDX, other_slot);

                        // this == null → deopt (re-run in the interpreter,
                        // which raises NPE through ordinary dispatch). Must
                        // come before the class-id check below dereferences
                        // RAX — see the N3-obs note above.
                        self.emit_test_r64_r64(RAX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                        // other == null → result 0.
                        self.emit_test_r64_r64(RDX);
                        let other_null = self.emit_jcc_rel32_patch(0x84); // JZ

                        // this.ptr == other.ptr → result 1.
                        // CMP RAX,RDX
                        self.buf.emit(&[0x48, 0x39, 0xD0]);
                        let same_ref = self.emit_jcc_rel32_patch(0x84); // JZ

                        // Class-id check: ObjectHeader.class_id is the i32
                        // at offset 0. `this` is a String, so [RAX] is
                        // String's class id; a differing [RDX] means a
                        // non-String argument → deopt.
                        //
                        // An ARRAY argument must be screened out first: a
                        // reference array's header carries its COMPONENT's
                        // class id at offset 0, so `"a".equals(new
                        // String[]{..})` passes the class-id compare below
                        // and the body then decodes the array as a String —
                        // reading a `value` pointer out of array memory.
                        // Java answers `false` for any array, so a non-plain
                        // object takes the result-0 path directly (no deopt:
                        // the answer is known). Same KIND_TAGS test the
                        // MIC/PIC cascades and the inline `checkcast` make.
                        //   TEST BYTE [RDX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK (F6 /0 ib: kind bits only)
                        self.buf.emit(&[
                            0xF6,
                            0x42,
                            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                            cratonvm_types::KIND_TAG_BYTE_MASK,
                        ]);
                        let other_not_plain = self.emit_jcc_rel32_patch(0x85); // JNE
                                                                               // MOV ECX,[RAX] ; CMP ECX,[RDX]
                        self.buf.emit(&[0x8B, 0x08]);
                        self.buf.emit(&[0x3B, 0x0A]);
                        bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                        // R8 = this.value, R9 = other.value (byte[] refs).
                        self.emit_load_string_value_ptr(
                            R8,
                            RAX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.emit_load_string_value_ptr(
                            R9,
                            RDX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        // Null value array on either side → deopt.
                        self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9,R9
                        bail.push(self.emit_jcc_rel32_patch(0x84));

                        // coder mismatch → deopt.
                        // MOV ECX,[RAX+coder] ; CMP ECX,[RDX+coder]
                        self.emit_load_string_i32_field(
                            RCX,
                            RAX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );
                        // other.coder may come from a legacy-laid-out `other`
                        // independently of `this` -- load it through the
                        // same compact/legacy-aware helper (R11 is free
                        // here) instead of a raw CMP-with-memory-operand,
                        // then compare register-to-register.
                        self.emit_load_string_i32_field(
                            R11,
                            RDX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );
                        self.emit_alu_r32_r32(0x39, RCX, R11); // CMP ECX,R11D
                        // JIT r22 wave 4, lane intrinsics
                        // (`CRATONVM_JIT_SP_STRING_EQUALS_CODER_FALSE`,
                        // opt-in): unequal coders answer `false`, which is
                        // JDK 25's `String.equals` exactly (`(!COMPACT_STRINGS
                        // || this.coder == aString.coder) && ..`: with
                        // compact strings off every String is UTF16, so the
                        // coders can only differ when they are on). The
                        // default deopts here, on EVERY call of a site that
                        // compares a LATIN1 and a UTF16 string (a mixed-script
                        // map key), and the interpreter answers the same
                        // `false`.
                        // Review w5 (lane loops) item 3: the `false` is only
                        // the JDK's answer while COMPACT_STRINGS is on (off,
                        // the JDK compares raw payloads whatever the coders),
                        // so the mismatch edge tests this VM's flag byte at
                        // run time and deopts as before when it is off; no
                        // flag address keeps the default deopt.
                        let coder_jne = self.emit_jcc_rel32_patch(0x85); // JNE
                        let compact_flag = if crate::sp_string_equals_coder_false_enabled() {
                            self.direct_helpers.compact_strings_flag_addr()
                        } else {
                            None
                        };
                        let coder_diff_false = match compact_flag {
                            Some(addr) => Some((coder_jne, addr)),
                            None => {
                                bail.push(coder_jne);
                                None
                            }
                        };

                        // value-array length mismatch → result 0.
                        // MOV ECX,[R8+12] ; CMP ECX,[R9+12]
                        self.buf
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            .emit(&[0x41, 0x8B, 0x48, ARRAY_LENGTH_OFFSET as u8]);
                        self.buf
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            .emit(&[0x41, 0x3B, 0x49, ARRAY_LENGTH_OFFSET as u8]);
                        let len_diff = self.emit_jcc_rel32_patch(0x85); // JNE

                        // Byte compare of ECX bytes from
                        // [R8+HEADER] vs [R9+HEADER] via REP CMPSB.
                        // RSI/RDI are callee-saved + may hold locals —
                        // bracket with PUSH/POP (no CALL in between).
                        // PUSH RSI ; PUSH RDI
                        self.buf.emit(&[0x56, 0x57]);
                        // RSI = R8 + HEADER ; RDI = R9 + HEADER
                        // LEA RSI,[R8+40]
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x49, 0x8D, 0x70, HEADER_SIZE as u8]);
                        // LEA RDI,[R9+40]
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x49, 0x8D, 0x79, HEADER_SIZE as u8]);
                        // RCX = length (ECX already holds it,
                        // zero-extended into RCX).
                        // REPE CMPSB  (F3 A6)
                        self.buf.emit(&[0xF3, 0xA6]);
                        // POP RDI ; POP RSI
                        self.buf.emit(&[0x5F, 0x5E]);
                        // SETE AL ; MOVZX EAX,AL → 1 iff all bytes equal
                        // (REPE stops on the first mismatch with ZF clear).
                        self.buf.emit(&[0x0F, 0x94, 0xC0]);
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                        let eq_done = self.emit_jmp_rel32_patch();

                        // Coder mismatch with the switch on: COMPACT_STRINGS
                        // off deopts (the stack is the coder site's: nothing
                        // pushed yet); on, fall into the result-0 path.
                        // R11 is free here, as at the coder compare.
                        if let Some((p, addr)) = coder_diff_false {
                            self.patch_rel32_to_here(p);
                            // Cast: the flag byte's address, baked as an
                            // immediate (stable for the VM's life).
                            self.emit_mov_imm64(R11, addr as i64);
                            self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11],0
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JE: compact strings off
                        }

                        // result 0 path (other null / an array / length
                        // mismatch / coder mismatch).
                        self.patch_rel32_to_here(other_null);
                        self.patch_rel32_to_here(other_not_plain);
                        self.patch_rel32_to_here(len_diff);
                        self.emit_xor_reg_self(RAX);
                        let false_done = self.emit_jmp_rel32_patch();

                        // result 1 path (same reference).
                        self.patch_rel32_to_here(same_ref);
                        // MOV EAX,1
                        self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]);

                        // join.
                        self.patch_rel32_to_here(eq_done);
                        self.patch_rel32_to_here(false_done);
                        self.push_from_rax();

                        for p in bail {
                            self.deopt_stubs.push((p, pc, 6));
                        }
                        intrinsic_handled = true;
                    }

                    // --- compareTo(Ljava/lang/String;)I ---------------
                    // Lexicographic decoded-char compare. Each side is
                    // decoded through ITS OWN `coder` byte, so every
                    // LATIN1/UTF16 combination (including mixed) is
                    // handled inline — no coder-mismatch deopt. The deopt
                    // stub is reached only for a null receiver, a null
                    // String argument (native throws NPE on the re-run),
                    // or a null backing `value` array. After those checks
                    // the result is fully determined: the unsigned-char
                    // difference at the first mismatch, else len1-len2.
                    // Identical to `native_string_compare_to`.
                    if let (Some(layout), true) = (
                        self.string_layout,
                        callee_entry == crate::JitIntrinsic::StringCompareTo.as_entry(),
                    ) {
                        self.flush_scratch_registers();
                        // Step 6: snapshot (this, other) before pops.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        let mut bail: Vec<usize> = Vec::new();

                        // Operand stack (deepest first): this, other.
                        let other_slot = self.pop_stack();
                        let this_slot = self.pop_stack();

                        // Compile-time String offsets: guard the layout
                        // epoch first (its fallback clobbers RCX/R11).
                        if jit_sp_field_layout_guard_enabled() {
                            bail.extend(self.emit_layout_epoch_guard());
                        }

                        // --- deopt checks + field reads happen BEFORE
                        // any PUSH, into CALLER-saved registers only.
                        // Two reasons: (1) the deopt stub's epilogue
                        // assumes RSP is at the post-prologue value, so
                        // the stack must stay balanced on every path that
                        // can reach a bail; (2) a `this`/`other` operand
                        // slot may itself be a callee-saved register that
                        // the loop is about to overwrite — reading it
                        // before the loop's registers are clobbered (and
                        // before the PUSH, while it is still the live
                        // local) is the only sound order.
                        //
                        // RAX = this; null receiver → deopt.
                        self.load_slot_to_reg(RAX, this_slot);
                        self.emit_test_r64_r64(RAX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                    // RDX = other; null argument → deopt.
                        self.load_slot_to_reg(RDX, other_slot);
                        self.emit_test_r64_r64(RDX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                    // R8 = this.value, R9 = other.value; null → deopt.
                        self.emit_load_string_value_ptr(
                            R8,
                            RAX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        self.emit_load_string_value_ptr(
                            R9,
                            RDX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9,R9
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        // R10 = this.coder, R11 = other.coder.
                        self.emit_load_string_i32_field(
                            R10,
                            RAX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );
                        self.emit_load_string_i32_field(
                            R11,
                            RDX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );

                        // --- past every deopt edge: save the callee-
                        // saved registers the loop uses, then park the
                        // pre-computed caller-saved values into them.
                        // PUSH RBX,RSI,RDI,R12,R13,R14,R15.
                        self.buf.emit(&[0x53, 0x56, 0x57]);
                        self.buf.emit(&[0x41, 0x54, 0x41, 0x55]);
                        self.buf.emit(&[0x41, 0x56, 0x41, 0x57]);
                        // RSI=this.value, RDI=other.value, R12=coder1,
                        // R13=coder2 (moves out of the caller-saved regs;
                        // the original callee-saved values are safely on
                        // the machine stack).
                        self.emit_mov_r64_r64(RSI, R8);
                        self.emit_mov_r64_r64(RDI, R9);
                        self.emit_mov_r64_r64(R12, R10);
                        self.emit_mov_r64_r64(R13, R11);
                        // len1 = this.value.length >> coder1  → R14D.
                        // Cast: fixed struct/layout offset to i32 instruction displacement
                        self.emit_mov_r32_mem_disp32(RAX, RSI, ARRAY_LENGTH_OFFSET as i32);
                        self.emit_alu_r32_r32(0x89, RCX, R12); // MOV ECX,R12D
                        self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                        self.emit_alu_r32_r32(0x89, R14, RAX); // MOV R14D,EAX
                                                               // len2 = other.value.length >> coder2 → R15D.
                                                               // Cast: fixed struct/layout offset to i32 instruction displacement
                        self.emit_mov_r32_mem_disp32(RAX, RDI, ARRAY_LENGTH_OFFSET as i32);
                        self.emit_alu_r32_r32(0x89, RCX, R13); // MOV ECX,R13D
                        self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                        self.emit_alu_r32_r32(0x89, R15, RAX); // MOV R15D,EAX
                                                               // min_len = min(len1,len2) → EBX.
                        self.emit_alu_r32_r32(0x89, RBX, R14); // MOV EBX,R14D
                        self.emit_alu_r32_r32(0x39, RBX, R15); // CMP EBX,R15D
                                                               // CMOVG EBX,R15D (EBX > R15D ⇒ keep R15D as min).
                        self.buf.emit(&[0x41, 0x0F, 0x4F, 0xDF]);

                        // i = 0 (R8D).
                        self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D
                        let loop_top = self.buf.pos();
                        // CMP R8D,EBX ; JGE loop_done (i >= min_len).
                        self.emit_alu_r32_r32(0x39, R8, RBX); // CMP R8D,EBX
                        let loop_done = self.emit_jcc_rel32_patch(0x8D);
                        // char_a = this[i]  → R9D ; char_b = other[i] → R10D.
                        self.emit_string_decode_char(R9, RSI, R8, R12);
                        self.emit_string_decode_char(R10, RDI, R8, R13);
                        // diff = char_a - char_b ; JNZ mismatch.
                        self.emit_alu_r32_r32(0x29, R9, R10); // SUB R9D,R10D
                        let mismatch = self.emit_jcc_rel32_patch(0x85);
                        // INC R8D ; JMP loop_top.
                        self.buf.emit(&[0x41, 0xFF, 0xC0]);
                        let back = self.emit_jmp_rel32_patch();
                        // Cast: value to i32 (encoding immediate/displacement)
                        let rel = loop_top as i32 - (back as i32 + 4);
                        self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                // loop_done: result = len1 - len2.
                        self.patch_rel32_to_here(loop_done);
                        self.emit_alu_r32_r32(0x89, RAX, R14); // MOV EAX,R14D
                        self.emit_alu_r32_r32(0x29, RAX, R15); // SUB EAX,R15D
                        let cmp_join = self.emit_jmp_rel32_patch();
                        // mismatch: result = diff (R9D).
                        self.patch_rel32_to_here(mismatch);
                        self.emit_alu_r32_r32(0x89, RAX, R9); // MOV EAX,R9D
                        self.patch_rel32_to_here(cmp_join);
                        // Sign-extend the int result for the push ABI.
                        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                                                            // POP R15,R14,R13,R12,RDI,RSI,RBX.
                        self.buf.emit(&[0x41, 0x5F, 0x41, 0x5E]);
                        self.buf.emit(&[0x41, 0x5D, 0x41, 0x5C]);
                        self.buf.emit(&[0x5F, 0x5E, 0x5B]);
                        self.push_from_rax();

                        for p in bail {
                            self.deopt_stubs.push((p, pc, 6));
                        }
                        intrinsic_handled = true;
                    }

                    // --- indexOf(I)I ----------------------------------
                    // LIVE again as of E27-1 N2b (2026-08-18), for constant
                    // BMP needles only. The `direct.filter` far above is
                    // what enforces that; by the time control reaches here
                    // the needle is known to be a compile-time constant in
                    // `0..=0xFFFF`.
                    //
                    // Scan the receiver for the first code unit equal to
                    // `(ch & 0xFFFF)`, from index 0. This comment used to
                    // call that "bit-identical to `native_string_index_of`,
                    // which likewise masks the argument". BOTH HALVES WERE
                    // FALSE — the JDK gates on `Character.isValidCodePoint`
                    // BEFORE any narrowing and matches a supplementary `ch`
                    // as a surrogate PAIR, and the native side stopped
                    // masking at E18-1, which put the rule in one place
                    // (`lang_string.rs`'s `code_point_needle`). It is
                    // spelled out rather than deleted because E27-1's
                    // finding is that code reading as a working
                    // implementation is how four copies of this rule
                    // survived.
                    //
                    // What makes the scan correct now is the RANGE, not the
                    // mask: on `0..=0xFFFF` the JDK scans for exactly one
                    // code unit, lone surrogates included, so the mask is
                    // the identity and this loop is `code_point_needle`'s
                    // answer. It is left in place rather than folded into a
                    // baked immediate to keep this change a gate change and
                    // nothing else — the emitted bytes here are unchanged.
                    //
                    // The deopt stub is reached only for a null receiver or
                    // a null backing `value` array, both genuinely
                    // once-per-program. It is NOT reached for an
                    // out-of-range needle: such a site is never
                    // intrinsified in the first place. That distinction is
                    // the whole of N2b — see `prev_insn_int_const`.
                    //
                    // Uses only caller-saved registers, so no PUSH/POP is
                    // needed.
                    if let (Some(layout), true) = (
                        self.string_layout,
                        callee_entry == crate::JitIntrinsic::StringIndexOfChar.as_entry(),
                    ) {
                        self.flush_scratch_registers();
                        // Step 6: snapshot (this, ch) before pops.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        let mut bail: Vec<usize> = Vec::new();

                        // Operand stack (deepest first): this, ch.
                        let ch_slot = self.pop_stack();
                        let this_slot = self.pop_stack();

                        // Compile-time String offsets: guard the layout
                        // epoch first (its fallback clobbers RCX/R11).
                        if jit_sp_field_layout_guard_enabled() {
                            bail.extend(self.emit_layout_epoch_guard());
                        }

                        // RAX = this; null receiver → deopt.
                        self.load_slot_to_reg(RAX, this_slot);
                        self.emit_test_r64_r64(RAX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                    // R8 = this.value; null → deopt.
                        self.emit_load_string_value_ptr(
                            R8,
                            RAX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        // R10 = this.coder.
                        self.emit_load_string_i32_field(
                            R10,
                            RAX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );
                        // R9D = needle = ch & 0xFFFF.
                        self.load_slot_to_reg(R9, ch_slot);
                        // THE CONSTANT-NEEDLE SCREEN NEVER REACHED THIS ARM.
                        // The comment above says "the `direct.filter` far
                        // above is what enforces that", and that filter sits
                        // in the INVOKESTATIC ladder — while `indexOf(I)` is
                        // an instance method and arrives here through
                        // `invokevirtual`, unscreened. So a variable needle
                        // was intrinsified too, and the mask below turned
                        // `"abc".indexOf(0x10061)` into a search for 'a'
                        // (answer 0; the JDK answers -1) and `indexOf(-1)`
                        // into a search for U+FFFF.
                        //
                        // A declined site cannot fall back to dispatch from
                        // here — the resolver registered it as an intrinsic,
                        // so it has no `invoke_info` — so a needle this
                        // compile cannot prove is in `0..=0xFFFF` gets a
                        // runtime screen whose miss takes the same
                        // uncommon trap as a null receiver: slow for an
                        // out-of-range needle, and right. A proven-constant
                        // site emits nothing extra.
                        //
                        // Round 9 wave 2: the resolver now screens
                        // (`try_compile_inner` declines the `StringIndexOfChar`
                        // registration unless
                        // `prev_insn_int_const_for_test` proves the needle is
                        // in `0..=0xFFFF`; see
                        // `string-indexof-char-screen-belongs-at-the-resolver-20260918.md`),
                        // so a variable-needle site gets `invoke_info` and never
                        // reaches this arm. This runtime screen is KEPT as the
                        // backstop (defence in depth: two instructions, and
                        // only when the two screens disagree about what a
                        // constant is).
                        // JIT r22 wave 3, lane intrinsics (review item 1 of
                        // `r22-review-w3-loops-on-intrinsics-FIXED-20261020.md`):
                        // under `CRATONVM_JIT_SP_STRING_INDEXOF_CHAR` a needle
                        // outside the BMP is ANSWERED inline (the JDK body,
                        // below the found path) instead of deopting on every
                        // call; without the switch the screen keeps its deopt.
                        let oor_inline = crate::sp_string_index_of_char_enabled();
                        let mut oor: Option<usize> = None;
                        if !matches!(prev_insn_int_const(_code, _code_len, pc), Some(0..=0xFFFF)) {
                            // CMP R9D, 0xFFFF (REX.B + 81 /7 id) ; JA bail
                            // (unsigned, so a negative needle bails too).
                            self.buf.emit(&[0x41, 0x81, 0xF9]);
                            self.buf.emit(&0xFFFFu32.to_le_bytes());
                            if oor_inline {
                                oor = Some(self.emit_jcc_rel32_patch(0x87)); // JA
                            } else {
                                bail.push(self.emit_jcc_rel32_patch(0x87)); // JA
                            }
                        }
                        // AND R9D, 0xFFFF  (REX.B + 81 /4 id).
                        self.buf.emit(&[0x41, 0x81, 0xE1]);
                        self.buf.emit(&0xFFFFu32.to_le_bytes());
                        // len = this.value.length >> coder → R11D.
                        // Cast: fixed struct/layout offset to i32 instruction displacement
                        let len_disp = ARRAY_LENGTH_OFFSET as i32;
                        self.emit_mov_r32_mem_disp32(RAX, R8, len_disp);
                        self.emit_alu_r32_r32(0x89, RCX, R10); // MOV ECX,R10D
                        self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                        self.emit_alu_r32_r32(0x89, R11, RAX); // MOV R11D,EAX
                                                               // i = 0 (EDX).
                        self.emit_xor_reg_self(RDX);
                        // JIT r22 wave 2, lane intrinsics (INTRINSICS21-2,
                        // `CRATONVM_JIT_SP_STRING_INDEXOF_CHAR`, opt-in):
                        // a 16-byte SSE2 stage before the scalar loop --
                        // `PCMPEQB` over a LATIN1 payload (a needle above
                        // 0xFF skips it: the scalar loop answers -1, as
                        // `StringLatin1.canEncode` does), `PCMPEQW` over a
                        // UTF16 one -- while 16 bytes remain; a hit `BSF`s
                        // to the first match, otherwise EDX is the first
                        // unscanned char and the scalar loop finishes.
                        let sse_found: Vec<usize> = if crate::sp_string_index_of_char_enabled() {
                            self.emit_sp_index_of_char_sse_stage()
                        } else {
                            Vec::new()
                        };
                        let loop_top = self.buf.pos();
                        // CMP EDX,R11D ; JGE not_found.
                        self.emit_alu_r32_r32(0x39, RDX, R11);
                        let not_found = self.emit_jcc_rel32_patch(0x8D);
                        // c = this[i] → ECX ; CMP ECX,R9D ; JE found.
                        self.emit_string_decode_char(RCX, R8, RDX, R10);
                        self.emit_alu_r32_r32(0x39, RCX, R9); // CMP ECX,R9D
                        let found = self.emit_jcc_rel32_patch(0x84);
                        // INC EDX ; JMP loop_top.
                        self.buf.emit(&[0xFF, 0xC2]);
                        let back = self.emit_jmp_rel32_patch();
                        // Cast: value to i32 (encoding immediate/displacement)
                        let rel = loop_top as i32 - (back as i32 + 4);
                        self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                // found: result = i.
                        self.patch_rel32_to_here(found);
                        self.emit_alu_r32_r32(0x89, RAX, RDX); // MOV EAX,EDX
                        let join = self.emit_jmp_rel32_patch();
                        // JIT r22 wave 3, lane intrinsics: a needle outside
                        // `0..=0xFFFF` (R9D still the raw int), JDK 25's
                        // `String.indexOf(int)` exactly. LATIN1:
                        // `StringLatin1.canEncode` fails -> -1. UTF16:
                        // `StringUTF16.indexOf` takes `indexOfChar` for a
                        // NEGATIVE needle (no unit equals it -> -1) and
                        // `indexOfSupplementary` otherwise: -1 unless
                        // `isValidCodePoint`, else the first `i < len - 1`
                        // with `value[i] == highSurrogate(ch)` and
                        // `value[i + 1] == lowSurrogate(ch)`.
                        let mut oor_minus_one: Vec<usize> = Vec::new();
                        let mut oor_found: Vec<usize> = Vec::new();
                        if let Some(oor) = oor {
                            let Ok(data) = u8::try_from(cratonvm_types::ARRAY_DATA_OFFSET) else {
                                self.fail("singlepass-codegen/indexof-char-data-offset-wide");
                                return WalkStep::Return(false);
                            };
                            let Some(data_hi) = data.checked_add(2).filter(|d| *d <= 0x7F) else {
                                self.fail("singlepass-codegen/indexof-char-data-offset-wide");
                                return WalkStep::Return(false);
                            };
                            self.patch_rel32_to_here(oor);
                            self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
                            oor_minus_one.push(self.emit_jcc_rel32_patch(0x84)); // JZ: LATIN1
                            self.buf.emit(&[0x41, 0x81, 0xF9]); // CMP R9D, 0x10FFFF
                            self.buf.emit(&0x10_FFFFu32.to_le_bytes());
                            oor_minus_one.push(self.emit_jcc_rel32_patch(0x87)); // JA: negative or invalid
                            self.buf.emit(&[0x44, 0x89, 0xC9]); // MOV ECX, R9D
                            self.buf.emit(&[0xC1, 0xE9, 0x0A]); // SHR ECX, 10
                            self.buf.emit(&[0x81, 0xC1]); // ADD ECX, 0xD7C0 (high surrogate)
                            self.buf.emit(&0xD7C0u32.to_le_bytes());
                            self.buf.emit(&[0x45, 0x89, 0xCB]); // MOV R11D, R9D
                            self.buf.emit(&[0x41, 0x81, 0xE3]); // AND R11D, 0x3FF
                            self.buf.emit(&0x3FFu32.to_le_bytes());
                            self.buf.emit(&[0x41, 0x81, 0xC3]); // ADD R11D, 0xDC00 (low surrogate)
                            self.buf.emit(&0xDC00u32.to_le_bytes());
                            self.emit_mov_r32_mem_disp32(RAX, R8, len_disp); // EAX = byte length
                            self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1 (chars)
                            self.buf.emit(&[0xFF, 0xC8]); // DEC EAX (the last start, len - 1)
                            self.emit_xor_reg_self(RDX);
                            let pair_top = self.buf.pos();
                            self.buf.emit(&[0x39, 0xC2]); // CMP EDX, EAX
                            oor_minus_one.push(self.emit_jcc_rel32_patch(0x8D)); // JGE
                            self.buf.emit(&[0x66, 0x41, 0x39, 0x4C, 0x50, data]); // CMP WORD [R8+RDX*2+data], CX
                            let next = self.emit_jcc_rel32_patch(0x85); // JNE
                            self.buf.emit(&[0x66, 0x45, 0x39, 0x5C, 0x50, data_hi]); // CMP WORD [R8+RDX*2+data+2], R11W
                            let hit = self.emit_jcc_rel32_patch(0x84); // JE
                            self.patch_rel32_to_here(next);
                            self.buf.emit(&[0xFF, 0xC2]); // INC EDX
                            self.emit_jmp_back_to(pair_top);
                            self.patch_rel32_to_here(hit);
                            self.emit_alu_r32_r32(0x89, RAX, RDX); // MOV EAX,EDX
                            oor_found.push(self.emit_jmp_rel32_patch());
                        }
                        // not_found: result = -1.
                        self.patch_rel32_to_here(not_found);
                        for p in oor_minus_one {
                            self.patch_rel32_to_here(p);
                        }
                        self.buf.emit(&[0xB8]);
                        self.buf.emit(&(-1i32).to_le_bytes());
                        self.patch_rel32_to_here(join);
                        // The SSE2 stage's hits arrive with EAX = the index.
                        for p in sse_found {
                            self.patch_rel32_to_here(p);
                        }
                        for p in oor_found {
                            self.patch_rel32_to_here(p);
                        }
                        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                        self.push_from_rax();

                        for p in bail {
                            self.deopt_stubs.push((p, pc, 6));
                        }
                        intrinsic_handled = true;
                    }

                    // --- indexOf(Ljava/lang/String;)I -----------------
                    // Naive O(n*m) substring search from index 0; an
                    // empty needle returns 0. Each haystack/needle char is
                    // decoded through its own `coder`, so all coder combos
                    // are handled inline. Bit-identical to
                    // `native_string_index_of_str`. The deopt stub is
                    // reached only for a null receiver or a null backing
                    // `value` array on either side; a null String argument
                    // also deopts — the native re-run then raises HotSpot's
                    // `NullPointerException` (`Cannot invoke "String.coder()"
                    // because "str" is null`, `native_string_index_of_str`).
                    // (This used to say the re-run "returns -1, which is the
                    // same answer": the JDK throws, and so does the native.)
                    if let (Some(layout), true) = (
                        self.string_layout,
                        callee_entry == crate::JitIntrinsic::StringIndexOfStr.as_entry(),
                    ) {
                        self.flush_scratch_registers();
                        // Step 6: snapshot (this, needle) before pops.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        let mut bail: Vec<usize> = Vec::new();

                        // Operand stack (deepest first): this, needle.
                        let needle_slot = self.pop_stack();
                        let this_slot = self.pop_stack();

                        // Compile-time String offsets: guard the layout
                        // epoch first (its fallback clobbers RCX/R11).
                        if jit_sp_field_layout_guard_enabled() {
                            bail.extend(self.emit_layout_epoch_guard());
                        }

                        // --- deopt checks + field reads BEFORE any PUSH,
                        // into CALLER-saved registers only (see the
                        // compareTo block for the rationale: balanced
                        // stack on deopt edges, and an operand slot may
                        // itself be a callee-saved register the loop is
                        // about to clobber).
                        self.load_slot_to_reg(RAX, this_slot);
                        self.emit_test_r64_r64(RAX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                        self.load_slot_to_reg(RDX, needle_slot);
                        self.emit_test_r64_r64(RDX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                        self.emit_load_string_value_ptr(
                            R8,
                            RAX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        self.emit_load_string_value_ptr(
                            R9,
                            RDX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9,R9
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        // R10 = haystack coder, R11 = needle coder.
                        self.emit_load_string_i32_field(
                            R10,
                            RAX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );
                        self.emit_load_string_i32_field(
                            R11,
                            RDX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );

                        // PUSH RBX,RSI,RDI,R12,R13,R14,R15.
                        self.buf.emit(&[0x53, 0x56, 0x57]);
                        self.buf.emit(&[0x41, 0x54, 0x41, 0x55]);
                        self.buf.emit(&[0x41, 0x56, 0x41, 0x57]);
                        // RSI=haystack value, RDI=needle value,
                        // R12=haystack coder, R13=needle coder.
                        self.emit_mov_r64_r64(RSI, R8);
                        self.emit_mov_r64_r64(RDI, R9);
                        self.emit_mov_r64_r64(R12, R10);
                        self.emit_mov_r64_r64(R13, R11);
                        // hlen → R14D, nlen → R15D.
                        // Cast: fixed struct/layout offset to i32 instruction displacement
                        self.emit_mov_r32_mem_disp32(RAX, RSI, ARRAY_LENGTH_OFFSET as i32);
                        self.emit_alu_r32_r32(0x89, RCX, R12); // MOV ECX,R12D
                        self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                        self.emit_alu_r32_r32(0x89, R14, RAX); // MOV R14D,EAX
                                                               // Cast: fixed struct/layout offset to i32 instruction displacement
                        self.emit_mov_r32_mem_disp32(RAX, RDI, ARRAY_LENGTH_OFFSET as i32);
                        self.emit_alu_r32_r32(0x89, RCX, R13); // MOV ECX,R13D
                        self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                        self.emit_alu_r32_r32(0x89, R15, RAX); // MOV R15D,EAX

                        // empty needle (nlen == 0) → result 0.
                        self.buf.emit(&[0x45, 0x85, 0xFF]); // TEST R15D,R15D
                        let needle_empty = self.emit_jcc_rel32_patch(0x84); // JZ
                                                                            // nlen > hlen → not_found.
                        self.emit_alu_r32_r32(0x39, R15, R14); // CMP R15D,R14D
                        let too_long = self.emit_jcc_rel32_patch(0x8F); // JG
                                                                        // max_start = hlen - nlen → EBX (inclusive bound).
                        self.emit_alu_r32_r32(0x89, RBX, R14); // MOV EBX,R14D
                        self.emit_alu_r32_r32(0x29, RBX, R15); // SUB EBX,R15D

                        // outer: i = 0 (R8D).
                        self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D
                        let outer_top = self.buf.pos();
                        // CMP R8D,EBX ; JG not_found (i > max_start).
                        self.emit_alu_r32_r32(0x39, R8, RBX);
                        let outer_done = self.emit_jcc_rel32_patch(0x8F);
                        // inner: j = 0 (R9D).
                        self.buf.emit(&[0x45, 0x31, 0xC9]); // XOR R9D,R9D
                        let inner_top = self.buf.pos();
                        // CMP R9D,R15D ; JGE match_found (j >= nlen).
                        self.emit_alu_r32_r32(0x39, R9, R15);
                        let match_found = self.emit_jcc_rel32_patch(0x8D);
                        // h = haystack[i+j]: R10D = i+j, decode → R11D.
                        self.emit_alu_r32_r32(0x89, R10, R8); // MOV R10D,R8D
                        self.emit_alu_r32_r32(0x01, R10, R9); // ADD R10D,R9D
                        self.emit_string_decode_char(R11, RSI, R10, R12);
                        // n = needle[j]: decode → R10D.
                        self.emit_string_decode_char(R10, RDI, R9, R13);
                        // CMP R11D,R10D ; JNE inner_break.
                        self.emit_alu_r32_r32(0x39, R11, R10);
                        let inner_break = self.emit_jcc_rel32_patch(0x85);
                        // INC R9D ; JMP inner_top.
                        self.buf.emit(&[0x41, 0xFF, 0xC1]);
                        let inner_back = self.emit_jmp_rel32_patch();
                        // Cast: value to i32 (encoding immediate/displacement)
                        let rel = inner_top as i32 - (inner_back as i32 + 4);
                        self.buf.try_patch_i32(inner_back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                      // inner_break: INC R8D ; JMP outer_top.
                        self.patch_rel32_to_here(inner_break);
                        self.buf.emit(&[0x41, 0xFF, 0xC0]); // INC R8D
                        let outer_back = self.emit_jmp_rel32_patch();
                        // Cast: value to i32 (encoding immediate/displacement)
                        let rel = outer_top as i32 - (outer_back as i32 + 4);
                        self.buf.try_patch_i32(outer_back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                      // match_found: result = i (R8D).
                        self.patch_rel32_to_here(match_found);
                        self.emit_alu_r32_r32(0x89, RAX, R8); // MOV EAX,R8D
                        let join = self.emit_jmp_rel32_patch();
                        // not_found (nlen>hlen or outer exhausted): -1.
                        self.patch_rel32_to_here(too_long);
                        self.patch_rel32_to_here(outer_done);
                        self.buf.emit(&[0xB8]);
                        self.buf.emit(&(-1i32).to_le_bytes());
                        let join2 = self.emit_jmp_rel32_patch();
                        // needle_empty: result = 0.
                        self.patch_rel32_to_here(needle_empty);
                        self.emit_xor_reg_self(RAX);
                        // join.
                        self.patch_rel32_to_here(join);
                        self.patch_rel32_to_here(join2);
                        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                                                            // POP R15,R14,R13,R12,RDI,RSI,RBX.
                        self.buf.emit(&[0x41, 0x5F, 0x41, 0x5E]);
                        self.buf.emit(&[0x41, 0x5D, 0x41, 0x5C]);
                        self.buf.emit(&[0x5F, 0x5E, 0x5B]);
                        self.push_from_rax();

                        for p in bail {
                            self.deopt_stubs.push((p, pc, 6));
                        }
                        intrinsic_handled = true;
                    }
                    // ===== INTRINSIC REGION END: STRING_SEARCH =====

                    // ===== INTRINSIC REGION BEGIN: CRC32 =====
                    // java.util.zip.CRC32 / CRC32C `update` call-site
                    // intrinsics (Phase 4c). Both classes hold a single
                    // `private int crc` at instance field slot 0 — the
                    // running (uncomplemented) CRC state — see
                    // crc_layout_contract.md. The two
                    // sentinels handled here (the IEEE `CRC32` variants
                    // were never registered and were deleted 2026-09-12):
                    //
                    //   Crc32cUpdateByte  : CRC32C.update(I)V
                    //   Crc32cUpdateBytes : CRC32C.update([BII)V
                    //
                    // Every variant:
                    //   1. pops the operand stack (receiver is the
                    //      deepest operand; `callee_params + 1` total),
                    //   2. emits a receiver class-id guard — `CMP
                    //      DWORD [recv+0], guard_class_id` — and deopts
                    //      to normal dispatch on a null receiver or a
                    //      class-id mismatch (a subclass could override
                    //      `update`),
                    //   3. loads the running crc from field cell slot 0
                    //      (`HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET`),
                    //   4. folds the input byte(s) into it,
                    //   5. writes the result back to the same cell
                    //      (Int tag word + payload word).
                    //
                    // CRC32C folds with the hardware `CRC32` instruction
                    // (it computes exactly the Castagnoli polynomial);
                    // CRC32 folds with an inline reflected-CRC bit loop
                    // (IEEE poly 0xEDB88320 — the hardware instruction is
                    // the wrong polynomial). Neither path emits a `CALL`.
                    //
                    // `update([BII)V` preserves null-array NPE and
                    // out-of-bounds AIOOBE by deopting on a null array or
                    // a range outside `[0, array.length]` — the
                    // interpreter then re-runs `update` via the native
                    // override, which raises the exact exception.
                    {
                        let crc32c_byte = crate::JitIntrinsic::Crc32cUpdateByte.as_entry();
                        let crc32c_bytes = crate::JitIntrinsic::Crc32cUpdateBytes.as_entry();
                        let is_crc32c = callee_entry == crc32c_byte || callee_entry == crc32c_bytes;
                        let is_byte_form = callee_entry == crc32c_byte;
                        let is_bytes_form = callee_entry == crc32c_bytes;

                        // `int crc` at field slot 0, addressed per OBJECT: a
                        // COMPACT instance (the default for a class with a
                        // registered layout -- every real-class-file class,
                        // `CRC32C` included) keeps it at its compact body
                        // offset, a LEGACY one inside a 16-byte `Value` cell.
                        // This arm used to address the legacy cell
                        // unconditionally, so on a compact receiver it read
                        // the state from the body's padding and wrote a zero
                        // tag over the real field: the compiled `update`s
                        // agreed with each other while `getValue()` (a
                        // compact-aware `getfield`/native) read the clobbered
                        // field (round 11 wave 2, lane x64gen). Same resolver
                        // and the same per-object branch as the
                        // `AtomicInteger` arms. `None` only for a guard id of
                        // 0 (never registered) or a non-4-byte storage, and
                        // then the region declines.
                        let crc_layout = if is_crc32c {
                            crate::AtomicIntFieldLayout::new(0, guard_class_id)
                        } else {
                            None
                        };
                        if let Some(crc_layout) = crc_layout {
                            // Instance field cell for the `int crc` at
                            // slot 0 on a LEGACY object: HEADER_SIZE +
                            // 0*SLOT_SIZE, then the tag word at +0 and the
                            // 32-bit payload at +FIELD_CELL_PAYLOAD32_OFFSET
                            // (see the inline-getfield codegen for 0xb4).
                            // Cast: fixed struct/layout offset to i32 instruction displacement
                            let cell_off = HEADER_SIZE as i32;
                            // Cast: fixed struct/layout offset to i32 instruction displacement
                            let tag_off = cell_off + FIELD_CELL_TAG_OFFSET as i32;
                            // Cast: fixed struct/layout offset to i32 instruction displacement
                            let pay_off = cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32;

                            self.flush_scratch_registers();
                            // Step 6: snapshot (receiver [, arr, off, len])
                            // before pops so null/bounds guard bails resume
                            // at the invokevirtual CRC32.update bci.
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::BoundsCheck,
                                );
                            }

                            // --- pop operands (deepest = receiver) ---
                            // update(I)V    : [receiver, b]
                            // update([BII)V : [receiver, arr, off, len]
                            let (b_or_len_slot, off_slot, arr_slot, recv_slot);
                            if is_byte_form {
                                let b = self.pop_stack();
                                let r = self.pop_stack();
                                b_or_len_slot = b;
                                off_slot = b; // unused
                                arr_slot = b; // unused
                                recv_slot = r;
                            } else {
                                let len = self.pop_stack();
                                let off = self.pop_stack();
                                let arr = self.pop_stack();
                                let r = self.pop_stack();
                                b_or_len_slot = len;
                                off_slot = off;
                                arr_slot = arr;
                                recv_slot = r;
                            }

                            // Pin operands into owned frame scratch slots
                            // below `next_spill_offset`. The call site
                            // had >= (callee_params+1) operand-stack
                            // entries, so these offsets are in-frame. The
                            // intrinsic pushes nothing (void return), so
                            // the next bytecode re-allocates spill slots
                            // from the same base.
                            //
                            // The pins start ABOVE every operand's own frame
                            // home — the arraycopy intrinsic's rule, for the
                            // same reason. The pops rewound the cursor over
                            // those homes, so pins based on the bare cursor
                            // alias them: with a register-homed receiver
                            // (`CalleeSaved`, no frame word) and a
                            // frame-resident array, `s_recv` IS the array's
                            // home, the receiver store overwrites it before
                            // it is read, and `s_a` then pins the CRC32C
                            // object as the "array". The deopt snapshot taken
                            // above also names those original homes and reads
                            // them when a guard bails.
                            let scratch_slots = if is_byte_form { 2 } else { 4 };
                            let mut crc_scratch_base = self.next_spill_offset;
                            for slot in [recv_slot, arr_slot, off_slot, b_or_len_slot] {
                                if let StackSlot::Frame(off) = slot {
                                    crc_scratch_base = crc_scratch_base.max(off.saturating_add(8));
                                }
                            }
                            if !self.spill_range_fits(crc_scratch_base, scratch_slots) {
                                self.fail("singlepass-codegen/intrinsic-pin-spill-exhausted");
                                return WalkStep::Return(false);
                            }
                            let s_recv = crc_scratch_base;
                            let s_a = crc_scratch_base + 8;
                            let s_b = crc_scratch_base + 16;
                            let s_c = crc_scratch_base + 24;
                            self.load_slot_to_reg(RAX, recv_slot);
                            self.emit_store_local(s_recv, RAX);
                            if is_byte_form {
                                self.load_slot_to_reg(RAX, b_or_len_slot);
                                self.emit_store_local(s_a, RAX);
                            } else {
                                self.load_slot_to_reg(RAX, arr_slot);
                                self.emit_store_local(s_a, RAX);
                                self.load_slot_to_reg(RAX, off_slot);
                                self.emit_store_local(s_b, RAX);
                                self.load_slot_to_reg(RAX, b_or_len_slot);
                                self.emit_store_local(s_c, RAX);
                            }

                            // Every "bail to interpreter" edge is wired
                            // to a shared uncommon-trap deopt stub
                            // (reason 2). The interpreter re-runs the
                            // method and dispatches `update` normally —
                            // preserving NPE / AIOOBE and any overriding
                            // subclass `update` exactly.
                            let mut bail_patches: Vec<usize> = Vec::new();

                            // The compact offset is a compile-time claim
                            // about `CRC32C`'s layout: a layout REPLACEMENT
                            // deopts here. First, while nothing is live in
                            // RCX/R11 (the guard's fallback form uses both).
                            if jit_sp_field_layout_guard_enabled() {
                                bail_patches.extend(self.emit_layout_epoch_guard());
                            }

                            // --- receiver class-id guard ---
                            // RAX = receiver. A null receiver bails
                            // (the interpreter NPEs on the virtual
                            // dispatch). Then CMP the class id at
                            // ObjectHeader+0 against the declared class.
                            self.emit_load_local(RAX, s_recv);
                            self.emit_test_r64_r64(RAX);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                                // CMP DWORD [RAX + 0], guard_class_id
                                                                                //   81 /7 ib? — use the imm32 form: 81 /7.
                                                                                //   ModRM 0x78 = mod00 reg=7(/7=CMP) rm=RAX.
                            self.buf.emit(&[0x81, 0x78, 0x00]);
                            self.buf.emit(&guard_class_id.to_le_bytes());
                            bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                            // --- load CRC state → ECX ---
                            // MOV ECX, DWORD [RAX + pay_off]. The slot
                            // holds a `Value::Int`. CRC32C stores the
                            // running (complemented) state, while real
                            // JDK CRC32 stores the public value. The IEEE
                            // folding helper consumes the former, so the
                            // CRC32 path complements on either side.
                            self.emit_test_mem8_imm8(
                                RAX,
                                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                cratonvm_types::GC_FLAG_COMPACT,
                            );
                            let crc_load_legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                            self.emit_mov_r32_mem_disp32(RCX, RAX, crc_layout.value_compact_offset);
                            let crc_load_done = self.emit_jmp_rel32_patch();
                            self.patch_rel32_to_here(crc_load_legacy);
                            self.emit_mov_r32_mem_disp32(RCX, RAX, pay_off);
                            self.patch_rel32_to_here(crc_load_done);

                            if is_byte_form {
                                // --- update(I)V: fold one byte ---
                                // EDX = arg byte & 0xFF.
                                self.emit_load_local(RDX, s_a);
                                // MOVZX EDX, DL  (0F B6 D2) — low 8 bits.
                                self.buf.emit(&[0x0F, 0xB6, 0xD2]);
                                // CRC32 ECX, DL — hardware Castagnoli
                                // fold of one byte. F2 0F 38 F0 /r,
                                // ModRM 0xCA = reg=ECX rm=EDX(=DL).
                                self.buf.emit(&[0xF2, 0x0F, 0x38, 0xF0, 0xCA]);
                            } else {
                                // --- update([BII)V: fold a range ---
                                // Guards (all bail to the deopt stub,
                                // matching the native override's NPE /
                                // AIOOBE semantics):
                                //   arr != null
                                //   off >= 0, len >= 0
                                //   off + len <= arr.length
                                //
                                // Register file held live across the
                                // guards into the fold loop:
                                //   R8  = array base pointer
                                //   R9  = current index (starts at off)
                                //   R11 = end index = off + len
                                //   RCX = running crc (already loaded)
                                // RDX/RAX are loop-body scratch (the
                                // IEEE helper consumes EDX and clobbers
                                // EAX), so they must NOT carry the index.
                                //
                                // R8 = array ptr; null-array → bail.
                                self.emit_load_local(R8, s_a);
                                self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                                bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ → null array
                                                                                    // RDX = off, sign-extended to 64-bit so
                                                                                    // the range arithmetic cannot overflow.
                                self.emit_load_local(RDX, s_b);
                                self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX,EDX
                                                                    // off < 0 ? TEST RDX,RDX; JS bail.
                                self.emit_test_r64_r64(RDX);
                                bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                                    // R11 = len, sign-extended.
                                self.emit_load_local(R11, s_c);
                                self.buf.emit(&[0x4D, 0x63, 0xDB]); // MOVSXD R11,R11D
                                                                    // len < 0 ? TEST R11,R11; JS bail.
                                self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11,R11
                                bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                                    // R11 = off + len  (the end index).
                                self.buf.emit(&[0x49, 0x01, 0xD3]); // ADD R11,RDX
                                                                    // RAX = arr.length (zero-extended 32-bit
                                                                    // load → non-negative 64-bit value).
                                self.buf
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    .emit(&[0x41, 0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[R8+ARRAY_LENGTH_OFFSET]
                                                                                           // off + len > arr.length ? CMP R11,RAX;
                                                                                           // JG bail (signed >).
                                self.buf.emit(&[0x49, 0x39, 0xC3]); // CMP R11,RAX
                                bail_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG
                                                                                    // R9 = current index = off (RDX).
                                self.buf.emit(&[0x49, 0x89, 0xD1]); // MOV R9,RDX

                                // --- fold loop ---
                                // .loop: CMP R9,R11 ; JGE .done
                                let loop_label = self.buf.pos();
                                self.buf.emit(&[0x4D, 0x39, 0xD9]); // CMP R9,R11
                                let done_patch = self.emit_jcc_rel32_patch(0x8D); // JGE
                                                                                  // EAX = byte = arr[R9].
                                                                                  // MOVZX EAX, BYTE [R8 + R9 + HDR]
                                                                                  //   43 0F B6 44 08 dd
                                                                                  //   (REX.X for R9 index, REX.B for
                                                                                  //    R8 base → 0x43; SIB scale=1).
                                self.buf.emit(&[
                                    0x43,
                                    0x0F,
                                    0xB6,
                                    0x44,
                                    0x08,
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    HEADER_SIZE as u8,
                                ]);
                                // CRC32 ECX, AL — hardware Castagnoli
                                // fold. F2 0F 38 F0 /r, ModRM 0xC8 =
                                // reg=ECX rm=EAX(=AL).
                                self.buf.emit(&[0xF2, 0x0F, 0x38, 0xF0, 0xC8]);
                                // INC R9 ; JMP .loop
                                self.buf.emit(&[0x49, 0xFF, 0xC1]); // INC R9
                                self.buf.emit_byte(0xE9); // JMP rel32
                                {
                                    let here = self.buf.pos();
                                    // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                    let rel = (loop_label as i64) - (here as i64 + 4);
                                    // Truncation: i64 -> i32 (rel32 branch displacement, range-checked)
                                    self.buf.emit(&(rel as i32).to_le_bytes());
                                }
                                // .done:
                                self.patch_rel32_to_here(done_patch);
                            }

                            // --- write CRC state back to slot 0 ---
                            // RAX = receiver again (reload — RAX was
                            // clobbered by the array-length load / loop).
                            self.emit_load_local(RAX, s_recv);
                            // Same per-object branch as the load.
                            self.emit_test_mem8_imm8(
                                RAX,
                                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                cratonvm_types::GC_FLAG_COMPACT,
                            );
                            let crc_store_legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                                                                    // COMPACT: the body word is the field; no tag.
                                                                                    // MOV DWORD [RAX + compact], ECX  (89 88 dd).
                            self.buf.emit_byte(0x89);
                            self.buf.emit_byte(0x88);
                            self.buf
                                .emit(&crc_layout.value_compact_offset.to_le_bytes());
                            let crc_store_done = self.emit_jmp_rel32_patch();
                            self.patch_rel32_to_here(crc_store_legacy);
                            // Tag word := 0  (Value::Int discriminant —
                            // pinned by `field_cell_layout_matches_
                            // value_enum` in cratonvm-types). Keeps the
                            // cell a well-formed Int even if a prior
                            // write left a stale tag.
                            self.emit_mov_dword_mem_disp32_imm32(RAX, tag_off, 0);
                            // Payload word := ECX (the class-specific
                            // state representation described above).
                            // MOV DWORD [RAX + pay_off], ECX  (89 88 dd).
                            self.buf.emit_byte(0x89);
                            self.buf.emit_byte(0x88);
                            self.buf.emit(&pay_off.to_le_bytes());
                            self.patch_rel32_to_here(crc_store_done);

                            // Wire every bail edge to the shared
                            // uncommon-trap stub (reason 2). Equal
                            // (bci, reason) pairs are coalesced by
                            // `emit_deopt_stubs`.
                            for patch in bail_patches {
                                self.deopt_stubs.push((patch, pc, 2));
                            }
                            // `update` is void — nothing is pushed.
                            let _ = ret_type;
                            intrinsic_handled = true;
                        }
                    }
                    // ===== INTRINSIC REGION END: CRC32 =====

                    // An intrinsic sentinel that no region above emitted.
                    // Several regions decline by leaving `intrinsic_handled`
                    // false — the `Atomic*` / box-unbox arms when their field
                    // layout does not resolve at emission, the String arms
                    // when `string_layout` is absent — each on a comment that
                    // says declining "keeps the plain direct-call path rather
                    // than emitting a CALL to an intrinsic sentinel". The
                    // plain path below IS a `CALL callee_entry`, i.e. to the
                    // sentinel. Refuse the method instead.
                    if !intrinsic_handled && crate::JitIntrinsic::is_sentinel_entry(callee_entry) {
                        self.fail("singlepass-codegen/unhandled-intrinsic-sentinel");
                        return WalkStep::Return(false);
                    }

                    if !intrinsic_handled {
                        // value-stack-usize-underflow-nio-worker-panic fix:
                        // snapshot the pre-pop operand stack here too (see
                        // the matching fix + comment on the MIC/PIC helper
                        // dispatch path below) — a direct call's callee can
                        // still throw/deopt, and `emit_post_invoke_exception_check`
                        // would otherwise be the first (and only) snapshot
                        // for this bci, taken AFTER the receiver/args are
                        // popped, which underflows on a `Reinterpret` resume.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        // Direct call: pop receiver + params, call compiled entry
                        // invokespecial has a receiver, so total args = callee_params + 1
                        let n = callee_params + 1; // receiver + params
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
                        // gcd d10/f (2026-09-28): the instance half of d9/d's
                        // callee-owned arguments (row 2 of the gc known-issues
                        // page `gcd-d9d-call-argument-copies-still-rooted-on-
                        // other-call-shapes-20260928.md`).
                        // A statically bound site (invoke kind 1: every
                        // `invokespecial`, and a private `invokevirtual` /
                        // `invokeinterface` pinned to its owner) is serviced
                        // exactly as an `invokestatic`: the service resolves the
                        // callee by name and reads no receiver class out of the
                        // copy (`jit_service_callee_deopt_body`'s
                        // `receiver_class_id` is asked only for kinds 0/2). So
                        // the same site and callee rule applies, and the site
                        // keeps no service copy. A guarded row (an intrinsic's
                        // receiver speculation) is not a plain bind: excluded.
                        // No retire cell or caller-held synchronized row reaches
                        // this arm (both are refused or lowered above).
                        // `CRATONVM_JIT_CALLEE_OWNED_ARGS=0` restores the copy.
                        let site_admits_owned_args = guard_class_id == 0
                            && sp_direct_call_args_owned_by_callee(
                                callee_owned_args_enabled(),
                                Some(op),
                                info_ptr.map(|ip| {
                                    // SAFETY: `invoke_info` owns every pointer it
                                    // hands out for the life of this compile.
                                    unsafe { (*ip).invoke_kind }
                                }),
                                false,
                                false,
                                self.pc_is_protected(pc),
                                || self.owned_args_handoff_replays_exactly(_code, _code_len, pc),
                            );
                        let is_method_invoke_direct = self.direct_helpers.reflect_method_invoke_cache != 0
                            && self.direct_helpers.reflect_method_invoke_sentinel != 0
                            && n == 3
                            && info_ptr.is_some_and(|p| {
                                // SAFETY: invoke-info entries are owned for the duration of this compile.
                                let ir = unsafe { &*(p as *const crate::JitInvokeInfo) };
                                ir.class_name == "java/lang/reflect/Method"
                                    && ir.method_name == "invoke"
                                    && ir.descriptor == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
                            });
                        let args_owned_by_callee = !is_method_invoke_direct
                            && site_admits_owned_args
                            && crate::direct_callee_owns_its_arguments(callee_entry);
                        let call_oop_arg_regs_before_pop = self.pending_call_oop_arg_regs;
                        let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                        if args_owned_by_callee {
                            // As the static arm: the registers holding the
                            // reference arguments are the callee's at the CALL.
                            self.pending_call_oop_arg_regs = call_oop_arg_regs_before_pop;
                        }
                        // JVMS 6.5: a null `objectref` raises NPE AT THE INVOKE,
                        // before the callee's first instruction. A baked direct
                        // call jumps straight into the compiled callee, so the
                        // only thing that ever raised it here was the callee
                        // body faulting on its own — which it does only if it
                        // dereferences `this`. MEASURED on three private callees
                        // behind one 50 000-call warming loop (HotSpot raises NPE
                        // for all three):
                        //
                        //     return 3;          NPE interpreted, NO-THROW(3) jit
                        //     return this.x;     NPE both
                        //     return helper();   NPE interpreted, NO-THROW(5) jit
                        //
                        // `invokevirtual`/`invokeinterface` are correct only
                        // incidentally: their inline cache tests the receiver's
                        // class, and a null fails every guard. `invokespecial`
                        // is statically bound, has no guard, and reaches here.
                        // This is the mechanism behind the bogus `Cannot read
                        // field "interfaces" because "rd" is null` at
                        // Class.java:1217 that `vm/tests/
                        // null_receiver_cached_invoke.rs` was written for.
                        //
                        // The receiver is argument 0 of every invoke that
                        // reaches this arm — 0xb6/0xb7/0xb9 all have one, which
                        // is why `n` above is `callee_params + 1`.
                        if let Some(receiver) = arg_slots.first() {
                            self.load_slot_to_reg(RAX, *receiver);
                            self.emit_precise_null_check_field_store();
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

                        // ===== INTRINSIC REGION BEGIN: SP_THIN_HELPER_PREFIX =====
                        // nio20 wave 2, lane L2: `Object.getClass()` as two
                        // loads (NIO19-5's single-pass/OSR half) and the
                        // `Unsafe` primitive-array accessors as one load or
                        // store (NIO19-2's single-pass twin), each a guarded
                        // PREFIX of this unchanged thin-helper CALL. Every
                        // decline lands on the first byte of the call below;
                        // the hit edge joins at the result push with the
                        // answer in RAX. See `sp_thin_helper_prefix_admitted`
                        // for why skipping the call's bookkeeping is sound.
                        // nio20 wave 3, lane L3 (W2L2-1/2): the rows come from
                        // `SP_THIN_PREFIX_ROWS`, shared with the static arm,
                        // which added the `Integer.intValue()` /
                        // `Long.longValue()` unbox prefixes.
                        let thin_prefix_hit = if self.sp_thin_helper_prefix_admitted(
                            info_ptr.is_some(),
                            guard_class_id,
                            vh_judged_site,
                            args_owned_by_callee,
                            callee_needs_ctx,
                        ) {
                            sp_thin_prefix_row(
                                &self.direct_helpers,
                                callee_entry,
                                true,
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
                        } else {
                            None
                        };
                        // nio21 wave 11 L5 (W9L2-2): a JUDGED thin `VarHandle`
                        // bind whose handle is a constant `static final` takes
                        // the constant-handle field access as the same kind of
                        // prefix. The judged record only shapes the CALL below
                        // (its tagged context word), which a hit skips; every
                        // other term of the contract is asked as for the plain
                        // binds. See `try_emit_sp_vh_const_thin_prefix`.
                        let thin_prefix_hit = if thin_prefix_hit.is_none()
                            && vh_judged_site != 0
                            && self.sp_thin_helper_prefix_admitted(
                                info_ptr.is_some(),
                                guard_class_id,
                                0,
                                args_owned_by_callee,
                                callee_needs_ctx,
                            ) {
                            self.try_emit_sp_vh_const_thin_prefix(
                                pc,
                                _code,
                                vh_judged_site,
                                ret_type,
                                &arg_slots,
                            )
                        } else {
                            thin_prefix_hit
                        };
                        // ===== INTRINSIC REGION END: SP_THIN_HELPER_PREFIX =====

                        // TAIL7-19-3 (lane tail8, opt-in): a tail-position
                        // thin helper with a leaf twin calls the twin first,
                        // frame up, and returns its answer; a decline falls
                        // through to the ordinary call below, unchanged. See
                        // `sp_tail_leaf_helpers_wide_enabled`.
                        self.try_emit_sp_instance_tail_leaf_twin(
                            _code,
                            _code_len,
                            pc,
                            if op == 0xb9 { 5 } else { 3 },
                            _branch_targets,
                            callee_entry,
                            callee_needs_ctx,
                            info_ptr.is_some(),
                            ret_type,
                            thin_prefix_hit.is_some(),
                            vh_judged_site,
                            args_owned_by_callee,
                            &arg_slots,
                        );

                        // Preserve Java arguments for the cold direct-callee
                        // exception-table service before call marshalling.
                        // None when the callee owns the arguments (gcd d10/f).
                        let service_info = info_ptr.filter(|_| !args_owned_by_callee);
                        let service_args_base = service_info.and_then(|_| {
                            // See `reserve_direct_call_service_slots`: this range MUST
                            // sit above the argument slots `pop_stack` just handed
                            // back, or the copy below reverses the arguments into
                            // themselves and the callee gets arg0 in every slot.
                            let base =
                                self.reserve_direct_call_service_slots(args_frame_top, &arg_slots)?;
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
                        // this frame's to name (gcd d10/f, as the static arm).
                        if !args_owned_by_callee
                            && arg_oops.iter().any(|&o| o)
                            && (!direct_call_arg_maps_enabled() || service_args_base.is_none())
                        {
                            self.pending_staged_args_unmapped = true;
                        }
                        // Round-8 wave-3 HIGH fix: stack-arg setup for
                        // invokespecial/virtual direct calls whose
                        // receiver+params exceed ARG_REGS.
                        let total_sub = self.emit_stack_arg_setup(&arg_slots, callee_needs_ctx);
                        // gci i1/sp (default ON since i2): as the static direct arm.
                        self.emit_zero_dead_arg_sources(
                            &arg_slots,
                            &arg_oops,
                            post_pop_spill,
                            args_frame_top,
                        );
                        // ARGWIN21-9: as the static direct arm.
                        self.emit_owned_call_dead_local_clears(args_owned_by_callee);
                        // Round 15 wave 6 (lane vh8; VH7-1): a judged thin
                        // `VarHandle` bind hands its helper the site record,
                        // tagged, in the context's register (the setup above
                        // loaded the context last; nothing between here and the
                        // CALL reads `ARG_REGS[0]` or treats it as an oop). The
                        // helper reads the `SharedVm` back out of the record.
                        if vh_judged_site != 0 {
                            self.emit_mov_imm64(
                                ARG_REGS[0],
                                // Cast: x86-64 immediate encoding of a heap address.
                                (vh_judged_site | crate::VARHANDLE_JUDGED_SITE_TAG) as i64,
                            );
                        }
                        // Round-8 wave-3: defensive callee-saved spill
                        // before any GC-triggering CALL -- see the
                        // invokestatic site above for why an oop-clean frame
                        // can publish the safepoint id alone.
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
                        // gcd d10/f, as the static arm: an owned argument in an
                        // ABI register is the callee's, which its prologue takes
                        // into its own frame before it can safepoint -- so it
                        // neither refuses the elision nor belongs in this
                        // frame's spill image. Unchanged when not owned.
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
                            // See the invokestatic twin: R11 stages, so only
                            // ARG_REGS are published, and only with slots --
                            // or when the callee owns the arguments.
                            self.emit_pre_safepoint_spill_args_published(
                                service_args_base.is_some() || args_owned_by_callee,
                                false,
                            );
                        }
                        if let (true, Some(info), Some(args_base)) = (
                            is_method_invoke_direct && total_sub == 0,
                            info_ptr,
                            service_args_base,
                        ) {
                            if std::env::var_os("DBG_MI").is_some() {
                                eprintln!("[DBG_MI_DC] method={} emit_method_invoke_cache_prefix in direct_calls", self.method_label);
                            }
                            let info_const = info as *const crate::JitInvokeInfo;
                            // SAFETY: invoke-info entries are owned for the duration of this compile.
                            let info_ref = unsafe { &*info_const };
                            let mut mi_done_patches: Vec<usize> = Vec::new();
                            self.emit_method_invoke_cache_prefix(
                                info_const,
                                info_ref,
                                args_base,
                                false,
                                &mut mi_done_patches,
                            );
                            // Miss: call reflect_method_invoke_sentinel(vm_ptr, info, args_ptr, 3)
                            self.emit_load_vm_context(ARG_REGS[0]);
                            self.emit_mov_imm64(ARG_REGS[1], info_const as i64);
                            self.emit_lea_frame_slot(ARG_REGS[2], args_base + 16);
                            self.emit_mov_imm32_sx(ARG_REGS[3], 3);
                            self.emit_call_absolute(self.direct_helpers.reflect_method_invoke_sentinel);
                            for p in mi_done_patches {
                                self.patch_rel32_to_here(p);
                            }
                        } else {
                            self.emit_call_absolute(callee_entry);
                        }
                        // gkr (OOM21-2): as the static arm; a judged `VarHandle`
                        // bind's helper is no Java callee.
                        if let (0, Some(base)) = (vh_judged_site, service_args_base) {
                            let ret = self.buf.pos();
                            self.note_service_arg_copies(ret, base, &arg_oops);
                        }
                        // MEGA619-1 (lane tail7, opt-in): a return-tail call
                        // owes no republish; see `sp_lazy_return_republish_enabled`.
                        let lazy_return_republish = self.sp_lazy_return_republish_admitted(
                            _code,
                            pc,
                            if op == 0xb9 { 5 } else { 3 },
                            _branch_targets,
                            false,
                            ret_type,
                        );
                        if !lazy_return_republish {
                            self.emit_post_call_rbp_republish();
                        }
                        // Stage A (precise oop maps, B-K fix) — a direct
                        // invokespecial/virtual call to a compiled callee is a
                        // GC-capable safepoint (the callee may allocate). Like
                        // the self-recursive site above, it historically spilled
                        // but recorded NO oop map (gap #1), so the precise path
                        // could not remap this frame. Gated behind `precise_maps`
                        // → byte-identical gate-OFF; gate-ON records the map and
                        // the paired post-safepoint register reload.
                        // Also under `shadow_enabled` (balance the shadow push/
                        // reload across this direct call — see the self-recursive
                        // site above).
                        if self.precise_maps || self.shadow_enabled {
                            self.emit_oop_map_for_safepoint();
                        }
                        self.emit_stack_arg_cleanup(total_sub);
                        if let (Some(info), Some(args_base)) = (info_ptr, service_args_base) {
                            self.emit_inline_callee_deopt_check(
                                info as *const crate::JitInvokeInfo,
                                arg_slots.len(),
                                args_base,
                            );
                        } else if let Some(info) = info_ptr.filter(|_| args_owned_by_callee) {
                            // gcd d10/f: serviced, with no buffer to read.
                            self.emit_inline_callee_deopt_check_owned_args(
                                info as *const crate::JitInvokeInfo,
                            );
                        } else {
                            self.dbg_unserviced_direct_call(
                                "invokespecial/virtual",
                                pc,
                                info_ptr.is_some(),
                                service_args_base.is_some(),
                            );
                            self.fail_unserviced_java_direct_call(info_ptr, service_args_base);
                        }

                        // A directly-called compiled callee that throws (or
                        // deopts) returns the `i64::MIN` sentinel. Propagate
                        // the deopt instead of running on with a bogus value.
                        self.emit_post_invoke_exception_check(ret_type);

                        // SP_THIN_HELPER_PREFIX hit edge: the answer is in RAX,
                        // exactly where the helper leaves its own, so the push
                        // below serves both. It made no call, so it owes
                        // neither the safepoint map / reload nor the sentinel
                        // checks above, and skips them (the VALUE_OF_INLINE_BOX
                        // contract of the static arm).
                        if let Some(hit) = thin_prefix_hit {
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
                            // Same obligation as the direct INVOKESTATIC arm
                            // (which has always tagged it) and the dispatch
                            // arm below: a reference return is a live oop and
                            // must not keep `push_from_rax`'s default `false`
                            // mark. Missing here, the reference is published
                            // to neither the precise oop map nor the shadow
                            // stack — see the self-recursive site above.
                            if matches!(ret_type, b'L' | b'[') {
                                self.mark_top_as_oop();
                            }
                        }
                    } // end `if !intrinsic_handled` (plain direct call)
                } else {
                    // Dispatch via helper (MIC-optimized for virtual/interface, plain for others)
                    // MED-4 / Fix 3 — O(1) pc-indexed lookups for invoke/MIC/PIC.
                    let info_ptr = self
                        .invoke_info_idx
                        .get(&pc)
                        .map(|&i| self.invoke_info[i].1);
                    if let Some(info) = info_ptr {
                        // SAFETY: info comes from self.invoke_info, which holds pointers to
                        // JitInvokeInfo structs kept alive by the caller for the duration of compilation.
                        let info_ref = unsafe { &*info };
                        let n = info_ref.num_jit_args;

                        // ===== INTRINSIC REGION BEGIN: ARRAYLIST_ACCESS =====
                        // Round 9 wave 8 (`arraylist8`): an exact-`ArrayList`
                        // `get(int)` / `size()` answered inline as a guarded
                        // PREFIX of this unchanged dispatch. Every decline edge
                        // lands on the first byte of the dispatch code below;
                        // the hit edge lands at the result push at the bottom
                        // of this arm with the answer in RAX. See
                        // `try_emit_arraylist_access_prefix`.
                        let arraylist_hit_patch =
                            self.try_emit_arraylist_access_prefix(pc, info_ref);
                        // ===== INTRINSIC REGION END: ARRAYLIST_ACCESS =====

                        // ===== INTRINSIC REGION BEGIN: BOX_HASH_EQUALS =====
                        // Round 11 wave 12 (lane `hashmap`): `Integer.hashCode()`,
                        // `Integer.equals(Object)` and a cached `String.hashCode()`
                        // answered inline at an `Object`-typed virtual site, as
                        // the same kind of guarded prefix. Never at a site the
                        // ArrayList prefix took (disjoint names; asked second so
                        // that stays true by construction). See
                        // `try_emit_box_hash_equals_prefix`.
                        let box_hash_equals_hits = if arraylist_hit_patch.is_none() {
                            self.try_emit_box_hash_equals_prefix(pc, info_ref)
                        } else {
                            Vec::new()
                        };
                        // ===== INTRINSIC REGION END: BOX_HASH_EQUALS =====
                        // nio20 wave 4, lane L3: `Unsafe.getChar` / `putChar`
                        // and the little-endian `*Unaligned` forms over a heap
                        // primitive array, the same kind of guarded prefix
                        // (disjoint names from both prefixes above). See
                        // `try_emit_sp_unsafe_dispatch_prefix`.
                        let unsafe_dispatch_hit =
                            if arraylist_hit_patch.is_none() && box_hash_equals_hits.is_empty() {
                                self.try_emit_sp_unsafe_dispatch_prefix(pc, info_ref)
                            } else {
                                None
                            };
                        // nio21 wave 1, lane L5: `System.identityHashCode` /
                        // `Class.hashCode()` read from the header, the
                        // optimizing tier's identity-hash prefix twin (names
                        // disjoint from the three prefixes above). See
                        // `try_emit_sp_identity_hash_dispatch_prefix`.
                        let identity_hash_hit = if arraylist_hit_patch.is_none()
                            && box_hash_equals_hits.is_empty()
                            && unsafe_dispatch_hit.is_none()
                        {
                            self.try_emit_sp_identity_hash_dispatch_prefix(pc, info_ref)
                        } else {
                            None
                        };
                        // nio21 wave 11 L5 (W9L2-2): a `VarHandle` access mode
                        // with no thin bind (getAndAdd / getAndSet / weak CAS /
                        // compareAndExchange / getAndBitwise, and every mode
                        // when the binds are off) through a constant handle,
                        // as the same kind of prefix; `VarHandle` sites are
                        // disjoint from the four prefixes above. See
                        // `try_emit_sp_vh_const_dispatch_prefix`.
                        let vh_const_hit = if arraylist_hit_patch.is_none()
                            && box_hash_equals_hits.is_empty()
                            && unsafe_dispatch_hit.is_none()
                            && identity_hash_hit.is_none()
                        {
                            self.try_emit_sp_vh_const_dispatch_prefix(pc, _code, info_ref)
                        } else {
                            None
                        };
                        let getclass_hit = if arraylist_hit_patch.is_none()
                            && box_hash_equals_hits.is_empty()
                            && unsafe_dispatch_hit.is_none()
                            && identity_hash_hit.is_none()
                            && vh_const_hit.is_none()
                            && crate::is_object_get_class_site(
                                info_ref.method_name,
                                info_ref.descriptor,
                            )
                            && crate::object_get_class_direct_enabled()
                            && n == 1
                        {
                            self.stack
                                .last()
                                .copied()
                                .and_then(|recv| self.try_emit_sp_getclass_prefix(pc, &[recv]))
                        } else {
                            None
                        };

                        // value-stack-usize-underflow-nio-worker-panic fix:
                        // snapshot the operand stack BEFORE popping this
                        // invoke's receiver/args, mirroring
                        // `snapshot_pre_intrinsic_call`'s "Step 6" pattern
                        // used by the String-intrinsic ladder above. Without
                        // this, the ONLY deopt point available at this bci is
                        // the one `emit_post_invoke_exception_check` builds
                        // AFTER the args are already popped (when this fix
                        // landed it reused `deopt_box_ptr_by_bci`; today it
                        // files its own reason-9 `PendingException` point in
                        // `exc_frame_box_ptr_by_bci`, keyed apart -- gci i2/sp) —
                        // that snapshot is fine for "resume after the call
                        // with an exception pending", but every such point is
                        // tagged `DeoptAction::Reinterpret` at THIS bci, which
                        // means "re-execute this same invoke bytecode from
                        // scratch" and therefore needs the receiver (+ args)
                        // still live on the operand stack. An empty
                        // post-pop snapshot underflows the moment the
                        // resumed interpreter re-fetches the receiver —
                        // reproduced as a `value_stack.rs` panic on a
                        // background NIO worker thread resuming
                        // `LinkedBlockingQueue.take()`'s `Condition.await()`
                        // interface dispatch after an inline-cache miss.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }

                        // Check for MIC slot at this PC
                        let mic_ptr = self.mic_slots_idx.get(&pc).map(|&i| self.mic_slots[i].1);
                        // Check for PIC slot at this PC. When both PIC
                        // and MIC are present (the adaptive recompiler
                        // promotes MIC → PIC and leaves the old MIC
                        // slot live as a fallback), PIC takes
                        // precedence: it caches a 4-entry superset.
                        let pic_ptr = self.pic_slots_idx.get(&pc).map(|&i| self.pic_slots[i].1);
                        if false {
                            eprintln!(
                                "[JIT_GEN_INVOKE_VS] pc={} op=0x{:02x} info_kind={} mic_present={} pic_present={} {}.{}{}",
                                pc, op, info_ref.invoke_kind, mic_ptr.is_some(), pic_ptr.is_some(),
                                info_ref.class_name, info_ref.method_name, info_ref.descriptor,
                            );
                        }

                        // Capture spill offset BEFORE popping to prevent
                        // the args buffer from overlapping source Frame slots.
                        let pre_pop_spill = self.next_spill_offset;
                        let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                        // Post-pop cursor — the restore point after the
                        // dispatch (see the invokestatic twin above for
                        // the Bug-4 frame-creep rationale).
                        let post_pop_spill = self.next_spill_offset;

                        let args_base_offset = pre_pop_spill;
                        if n > 0 {
                            let Some(args_end) = self.checked_spill_range_end(args_base_offset, n)
                            else {
                                return WalkStep::Return(false);
                            };
                            self.next_spill_offset = args_end;
                            // Store args in reverse offset order (same fix
                            // as invokestatic): higher offsets → lower addresses,
                            // so arg[0] at highest offset = lowest address.
                            for (i, slot) in arg_slots.iter().enumerate() {
                                let buf_offset = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                                self.load_slot_to_reg(RAX, *slot);
                                self.emit_store_local(buf_offset, RAX);
                                // See the invokestatic twin: the map below
                                // is the only remaining namer of this oop.
                                if arg_oops[i] {
                                    self.pending_staged_arg_oops.push(buf_offset);
                                }
                            }
                            // gci i1/sp (default ON since i2): see the invokestatic twin.
                            // Every later reader (the IC hit marshal, the
                            // `.miss` helper, the site-open deopt service)
                            // reads the buffer, never a source.
                            self.emit_zero_dead_arg_sources(
                                &arg_slots,
                                &arg_oops,
                                post_pop_spill,
                                pre_pop_spill,
                            );
                        }
                        // gcd d9/d row 2 at the inline-cache hits: the buffer
                        // words a hit's service copy window may clear
                        // (`ic_hit_window_arg_oops`).
                        let ic_hit_window_oops = ic_hit_window_arg_oops(&arg_oops);

                        // PRECISE-MAPS FIX (bug-03 layer C / B-K): the inline
                        // MIC/PIC cascade below calls the resolved compiled
                        // callee directly (`call r11`) on a class-id hit and
                        // then `jmp`s to the shared `.done` site, where
                        // `emit_oop_map_for_safepoint` emits the precise
                        // post-safepoint RELOAD of oop register-locals from
                        // their canonical frame slots. But the inline-hit path
                        // never reaches the slow-path `emit_pre_safepoint_spill`
                        // below — so under `precise_maps` the reload would load
                        // an UN-spilled (stale) slot back into a live oop
                        // register (e.g. the receiver `this`), which then
                        // faults on the next `getfield` (observed: compiled
                        // `String.codePointAt` → `this.isLatin1()` inline hit →
                        // reload corrupts `this` → SIGSEGV). Spill HERE, before
                        // the cascade, so the spill dominates BOTH the
                        // inline-hit and the slow/miss paths and pairs with the
                        // single shared reload. Gated on `precise_maps`, and the
                        // slow-path spill below is made `!precise_maps`, so the
                        // gate-OFF default path is byte-identical (exactly one
                        // conservative spill, on the slow path, as before).
                        //
                        // SB-CRASH-04: `safepoint_reg_spill` joins this hoist for
                        // the SAME reason — the inline-hit virtual dispatch is a
                        // GC-capable safepoint (the callee allocates), so the
                        // caller's register-only oops must be spilled BEFORE the
                        // cascade to be visible to the conservative scan. Hoisting
                        // here (vs the .miss slow path) also keeps the inline-hit
                        // `jmp .done` rel8 span from being widened by the spill.
                        //
                        // r11 x64gen: `shadow_enabled` joins too. The shared
                        // `.done` map calls `emit_shadow_reload`, which pops
                        // whatever push `pending_shadow` names; with the spill
                        // (and so the push) on the slow path only, an inline
                        // HIT ran that reload with no push. Unreachable on the
                        // default flags (both terms above are on), reachable
                        // with `CRATONVM_NO_PRECISE_JIT_MAPS` + no moving young +
                        // the precise reg-spill opt-out.
                        if self.precise_maps || self.safepoint_reg_spill || self.shadow_enabled {
                            // The hoisted spill dominates the inline-hit and
                            // the miss path and pairs with ONE shared reload
                            // at `.done`. Both halves of that pairing survive
                            // the elision: the predicate requires
                            // `precise_maps` and refuses any register-homed
                            // reference local, so the shared
                            // `emit_post_safepoint_reload` -- which walks
                            // `local_oop_masks[pc]`, oops only -- has nothing
                            // to reload and emits nothing. Every argument,
                            // receiver included, is already in the args
                            // buffer with its oops named in the map above.
                            if self.can_elide_direct_call_register_spill(&arg_oops, true, 3) {
                                self.emit_safepoint_metadata_only();
                            } else {
                                // Args are in the buffer and their oops are
                                // named in the map at `.done`; RAX only when
                                // the staging loop ran. See the dispatch twin.
                                self.emit_pre_safepoint_spill_args_published(true, n > 0);
                            }
                        }

                        // CRIT-8 — Inline MIC fast-path guard.
                        //
                        // Layout (verified by `test_jit_mic_slot_offsets` in
                        // jit/src/lib.rs; struct is `#[repr(C)]`):
                        //   offset  0  AtomicU32  cached_class_id
                        //   offset  8  AtomicU64  cached_entry_word
                        //              (entry | needs-context in bit 0)
                        //
                        // HIGH-7 follow-up — Inline 4-way PIC fast-path
                        // guard. `JitPICSlot` is now `#[repr(C)]` with
                        // hot atomic fields at the front (see
                        // `jit/src/lib.rs:1305` and the layout assertion
                        // `test_jit_pic_slot_offsets`):
                        //
                        //   CLASS_ID_OFFSETS      = [0, 4, 8, 12]
                        //   ENTRY_PTR_OFFSETS     = [16, 24, 32, 40]
                        //
                        // Each entry offset holds a TAGGED word: the
                        // target address with bit 0 set when it needs
                        // the VM context (`JIT_IC_NEEDS_CONTEXT_TAG`).
                        //
                        // The Mutex<Option<String>> array (`class_names`)
                        // is moved to the tail so its unstable layout
                        // cannot disturb these offsets.
                        //
                        // When a PIC slot is allocated at this PC, we
                        // emit a 4-way cascade in place of the MIC probe.
                        // PIC supersedes MIC (it is a 4-entry superset)
                        // so we do not emit BOTH guards.
                        //
                        // Hot-path sequence (PIC, ≈5 cycles on slot-0 hit):
                        //   mov   r10, imm64(pic)
                        //   mov   rax, [rbp - receiver_spill]
                        //   mov   eax, [rax]                       ; class_id @ ObjectHeader+0
                        //   ; --- per slot i in 0..4 ---
                        //   cmp   eax, [r10 + CLASS_ID_OFFSETS[i]]
                        //   jne   .try_{i+1}  (or .miss for the last)
                        //   mov   r11, [r10 + ENTRY_PTR_OFFSETS[i]] ; ONE load
                        //   test  r11, r11
                        //   jz    .miss
                        //   btr   r11, 0          ; CF = needs-context tag
                        //   jc    .call           ; context ABI already live
                        //   jmp   .noctx          ; (last slot falls through)
                        //   ; --- end per-slot ---
                        // .noctx:                 ; ONE shared tail (r9 wave 2)
                        //   <load context-free ABI: arg_slots[0..n]>
                        // .call:
                        //   call  r11
                        //   <rbp republish> <callee-deopt check>
                        //   jmp   .done
                        // .miss:
                        //   <existing helper-ABI setup>
                        //   call  jit_invoke_virtual_mic            ; same helper —
                        //                                            ; it consults the
                        //                                            ; underlying cache
                        //                                            ; (MIC or PIC via the
                        //                                            ; adaptive recompiler).
                        // .done:
                        //
                        // Raw memory loads of the atomics are equivalent
                        // to `Ordering::Relaxed` reads (no fences). A
                        // torn class_id or stale entry pointer at worst
                        // causes a miss → slow path; the helper
                        // revalidates and re-resolves authoritatively.
                        // Empty PIC entries hold `EMPTY_WAY_CLASS_ID`, which
                        // no receiver matches, so an empty slot fails its
                        // CMP and falls through. They used to hold 0, on the
                        // claim that ClassId 0 (`java.lang.Object`) is never
                        // a receiver here. It is one — a plain `new
                        // Object()` and the untyped ClassId(0) synthetic
                        // containers both carry it — and a class-0 receiver
                        // matched an empty way while that way was being
                        // published word-first. See `JitPICSlot::class_ids`.
                        // The MIC's empty slot now holds
                        // `JitMICSlot::EMPTY_CLASS_ID` (round 9 wave 2; see
                        // `mic-empty-slot-class-zero-window-20260918.md`),
                        // which is likewise no receiver's class id, so the
                        // MIC arm's single `CMP EAX,[R10]` below needs no
                        // emptiness test of its own: an empty MIC simply
                        // misses.
                        //
                        // Fast-path eligibility (same as MIC):
                        //   1. pic_ptr OR mic_ptr is Some.
                        //   2. The receiver exists (n >= 1).
                        //   3. The cached entry's `needs_context` bit is
                        //      checked inline and selects the matching
                        //      compiled-entry ABI.
                        //   4. Total callee-ABI arg count (1 vm_ptr + n)
                        //      fits in ARG_REGS.
                        let needs_ctx_arg_count = n + 1; // vm_ptr + n receiver/params
                        let args_fit = n >= 1 && needs_ctx_arg_count <= ARG_REGS.len();
                        // Round 11 wave 12 (lane `hashmap`, H11-2): a site whose
                        // arguments overflow the register file gets the MIC
                        // too, with the overflow on the stack in the entry ABI
                        // every compiled body reads (`emit_stack_arg_setup`'s
                        // layout). `None` for every site that fits, which keeps
                        // those byte-identical. PIC and the hashed stub stay
                        // register-only; see `ic_stack_arg_block`.
                        let mic_stack_block = if n >= 1 && !args_fit {
                            ic_stack_arg_block(n, ARG_REGS.len(), cfg!(target_os = "windows"))
                        } else {
                            None
                        };
                        // The inline MIC/PIC path emits a raw CALL into
                        // another compiled body.  That bypasses the
                        // interpreter-owned JitEntryGuard, leaving the
                        // active-RBP mirror pointing at the callee while
                        // the root-chain metadata still names the caller.
                        // That is NOT a reclaim hazard, which is what this
                        // comment used to claim: `chain_entry_rbp_is_foreign`
                        // detects it, the moving-young coverage proof comes
                        // back incomplete, and the cycle falls back to the
                        // non-moving sweep. It costs precision, not
                        // correctness — measured, nothing is reclaimed.
                        // `direct_jit_callee_calls_enabled()` gates the
                        // inline MIC path (default-ON — see its doc
                        // comment for the closing fixes and the
                        // regression this default avoids; opt out with
                        // `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`). The MIC
                        // and PIC both publish entry pointer + ABI before
                        // the release-store of class id. Generated guards
                        // also reject a zero entry pointer, covering
                        // class-only profile seeds.
                        // Cached direct entries cannot publish the caller's
                        // precise exception frame. A protected call in a method
                        // whose handler reads locals must take the dispatch path,
                        // where `emit_post_invoke_exception_check` records that
                        // complete caller state before entering its handler.
                        //
                        // Round 13 wave 11 (lane mega9, proposal M13-7): under
                        // `CRATONVM_JIT_SP_IC_PROTECTED_SITES` (default on since
                        // round 15 wave 4; `0` = helper calls) such a
                        // site keeps its caches where the pre-call spill is the
                        // HOISTED one below (`precise_maps`, `safepoint_reg_spill`
                        // or `shadow_enabled`): it then dominates the hit paths as
                        // it does the dispatch path, every hit joins `.done`,
                        // whose `emit_post_invoke_exception_check` records the
                        // same reason-9 frame, and a hit's callee sentinel is
                        // serviced by `jit_service_callee_deopt`, the routine the
                        // dispatch helper's own door runs. Without the hoist the
                        // dispatch path alone spills, which is what let a
                        // handler read a register-homed local back stale
                        // (`a523715a84`, Spring `invokeListener`).
                        let protected_precise_handler_call = self.precise_exception_frames
                            && self.pc_is_protected(pc)
                            && !(self.precise_maps
                                || self.safepoint_reg_spill
                                || self.shadow_enabled);
                        // Per-site bisect levers (`CRATONVM_JIT_SP_IC_ONLY`
                        // / `_DENY`). Inert unless one is set: the whole
                        // cascade is a program-wide switch otherwise, which
                        // localises a defect to this edge but not to a site.
                        let site_allowed = sp_ic_site_allowed(
                            &self.method_label,
                            info_ref.class_name,
                            info_ref.method_name,
                        );
                        let inline_virtual_ic_allowed = crate::direct_jit_callee_calls_enabled()
                            && sp_inline_ic_enabled()
                            && site_allowed
                            && !protected_precise_handler_call;
                        // JIT round 19 wave 2 (lane mega2, MEGA19-4,
                        // `CRATONVM_JIT_MEGA_SEEDED_SKIP_INLINE`): a slot the
                        // receiver profile seeded megamorphic never clears,
                        // so the PIC arm's gate would be taken on every call:
                        // emit neither the PIC arm nor the MIC arm, and let
                        // the site start at the hashed stub's full entry
                        // (`.miss`), which reloads the receiver itself. Only
                        // where that stub is emitted (the stub block's own
                        // condition) and the gate is admitted. A loop copy's
                        // fresh slot is unseeded but shares this code: it is
                        // a stub-only site, correct by the stub and helper.
                        let sp_seeded_skip_admitted = pic_ptr.is_some_and(|pic| {
                            inline_virtual_ic_allowed
                                && sp_inline_mega_enabled()
                                && crate::runtime_lowering::mega_seeded_skips_inline_ways(
                                    pic as usize,
                                    n,
                                )
                        });
                        // JIT round 19 wave 4 (lane megaosr4, MEGA2-3,
                        // `CRATONVM_JIT_SP_MEGA_SEEDED_KEEP_PROBE`): under
                        // `CRATONVM_JIT_SP_MEGA_STAGE_ONCE` such a site keeps
                        // its PIC arm (the hoisted ABI and the class-id load)
                        // with the gate tested FIRST, so its sticky gate goes
                        // straight to the register probe, whose hit takes the
                        // ways' own call tail. The ways are emitted but never
                        // run (the gate is set before the code exists). Only
                        // where the PIC arm itself is admitted.
                        let sp_seeded_keep_probe = sp_seeded_skip_admitted
                            && args_fit
                            && sp_inline_pic_enabled()
                            && crate::runtime_lowering::sp_mega_stage_once_enabled()
                            && crate::runtime_lowering::sp_mega_seeded_keep_probe_enabled();
                        let sp_seeded_skip = sp_seeded_skip_admitted && !sp_seeded_keep_probe;
                        let emit_method_invoke_cache = inline_virtual_ic_allowed
                            && (self.precise_maps || self.safepoint_reg_spill || self.shadow_enabled)
                            && info_ref.class_name == "java/lang/reflect/Method"
                            && info_ref.method_name == "invoke"
                            && info_ref.descriptor == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
                            && n == 3
                            && self.direct_helpers.reflect_method_invoke_cache != 0
                            && self.direct_helpers.reflect_method_invoke_sentinel != 0;
                        let pic_inline = inline_virtual_ic_allowed
                            && !emit_method_invoke_cache
                            && sp_inline_pic_enabled()
                            && pic_ptr.is_some()
                            && args_fit
                            && !sp_seeded_skip;
                        let mic_inline = inline_virtual_ic_allowed
                            && !emit_method_invoke_cache
                            && sp_inline_mic_enabled()
                            && !sp_seeded_skip
                            && !pic_inline
                            && mic_ptr.is_some()
                            && (args_fit || mic_stack_block.is_some());
                        // Can an ARRAY reach this site's MIC/PIC probe? Only
                        // then does the probe need its `KIND_TAGS` screen (an
                        // array header carries its COMPONENT's class id, so a
                        // `Foo[]` passes `CMP [recv], Foo`). An `invokevirtual`
                        // whose owner is a class other than `Object` (or an
                        // array type) has a receiver the verifier held to that
                        // class, so no array arrives and the screen is a compare
                        // that never fails -- the rule the guarded-inline chain
                        // above already applies. `invokeinterface` always
                        // screens: the verifier treats interface types as
                        // `Object` (x64gen proposal 6).
                        // j27 wave 7 (lane T1DISP21, DISPATCH21-4 twin,
                        // `CRATONVM_JIT_SP_IC_CAST_KIND_SCREEN_ELIDE`): nor
                        // where the `checkcast` right before a receiver-only
                        // call proved a plain object (`sp_cast_kind_screen`).
                        let ic_receiver_may_be_array = (op != 0xb6
                            || crate::x64::inlining::static_receiver_admits_arrays(
                                info_ref.class_name,
                            ))
                            && !self.sp_ic_receiver_cast_proves_plain(
                                _code,
                                _code_len,
                                pc,
                                n,
                                _branch_targets,
                            );
                        if sp_ic_site_trace() {
                            eprintln!(
                                "[SP_IC_SITE] {}||{}.{}{} pc={} pic={} mic={}",
                                self.method_label,
                                info_ref.class_name,
                                info_ref.method_name,
                                info_ref.descriptor,
                                pc,
                                pic_inline,
                                mic_inline,
                            );
                        }
                        // `.done` patches collected from each emitted
                        // fast-path. Multiple in PIC's case (one per
                        // slot), one in MIC's, none if neither inline
                        // fires. All are JMP rel32 (5 bytes) so the
                        // patch records a 4-byte signed displacement at
                        // `patch_pos`.
                        // MEGA619-1 (lane tail7, opt-in): the inline-cache
                        // HIT of a return-tail site owes no republish (see
                        // `sp_lazy_return_republish_enabled`). Asked once:
                        // every hit tail below (the PIC ways, the vtable
                        // column, the MIC) joins `.done`, whose only VM-capable
                        // edge is the exception check the rule screens.
                        let lazy_ic_hit_republish = self.sp_lazy_return_republish_admitted(
                            _code,
                            pc,
                            if op == 0xb9 { 5 } else { 3 },
                            _branch_targets,
                            false,
                            info_ref.return_type,
                        );
                        let mut done_patches32: Vec<usize> = Vec::new();
                        // `.done` patches that are JMP rel8 (single
                        // byte); MIC and the last PIC slot use these
                        // when the skip distance is small enough.
                        let mut done_patch: Option<usize> = None;
                        let mut mic_miss_patches32: Vec<usize> = Vec::new();
                        // Round 12 wave 7 (lane mega6, W7-1): the PIC arm's
                        // megamorphic gate, when it goes to the hashed stub's
                        // gate entry instead of `.miss`; patched once the
                        // stub below is emitted. Since round 17 wave 1 (lane
                        // mega, MW13-4) also every miss of the inline
                        // vtable-column probe, which takes the gate's place
                        // under `CRATONVM_JIT_MEGA_VTABLE_COLUMN`.
                        let mut mega_gate_patch: Vec<usize> = Vec::new();
                        // JIT round 19 (lane mega19, MEGA5-18-3): the register
                        // probe's misses under `CRATONVM_JIT_MEGA_PROBE_MISS_TO_HELPER`,
                        // patched to the resolving helper's setup (past the
                        // stub), which reads every argument from the frame.
                        let mut probe_helper_patches: Vec<usize> = Vec::new();
                        // Round 13 (lane crash): native-stack guard for the raw
                        // inline-cache CALL. An IC hit calls the compiled
                        // callee directly, so a recursion cycle closed through
                        // virtual/interface sites (Jackson's serializer
                        // recursion, reactive operator chains) never passes a
                        // stack check: it ran the native stack into its guard
                        // page -- a SIGSEGV where the JVMS requires a catchable
                        // StackOverflowError (`spring-framework-and-boot-...`
                        // signature 3). Below the floor the site takes the
                        // resolving helper (NOT `.miss`, whose hashed stub also
                        // calls raw), and the helper raises the error.
                        // Only where the frame caches the floor (every frame
                        // with an IC site since round 13 wave 2:
                        // `driver.rs::reserve_stack_floor`).
                        //
                        // Round 13 wave 2 (lane mega): also in front of a
                        // stub-only site (a PIC slot whose inline cascade is
                        // switched off still gets the hashed stub below, whose
                        // hits CALL raw), and the OSR placeholder refresh moved
                        // off the hot path: an OSR frame's `usize::MAX`
                        // placeholder fails the `JBE` like a low stack, and the
                        // cold side (emitted before the helper setup) fetches
                        // the real floor, compares again and comes back to
                        // `ic_cascade_start` when the stack is fine. The hot
                        // path is `CMP RSP,[rbp-floor]; JBE`, one fused pair.
                        let ic_stub_emitted = pic_ptr.is_some()
                            && inline_virtual_ic_allowed
                            && !emit_method_invoke_cache
                            && sp_inline_mega_enabled();
                        //
                        // Round 13 wave 4 (lane mega3, M13-3): where the VM
                        // publishes the thread's floor word
                        // (`ic_stack_floor_word_offset`), the site reads it
                        // through the `JIT_THREAD` mirror instead, and the
                        // frame reserves no floor slot for its IC sites
                        // (`driver.rs::reserve_stack_floor`), so no per-entry
                        // leaf CALL fetches one:
                        //
                        //   mov rax, <seg>:[mirror] ; test rax,rax ; jz .low
                        //   cmp rsp, [rax + floor_off] ; jbe .low
                        //
                        // RAX is free here: every register the cascade reads
                        // it loads from the frame, as its cold side's leaf
                        // call already relies on. A thread with no mirror
                        // (never expected while compiled code runs) takes the
                        // exact leaf-fetched compare on the cold side.
                        let mut ic_stack_low_patch: Option<usize> = None;
                        let mut ic_stack_no_thread_patch: Option<usize> = None;
                        let mut ic_floor_from_thread = false;
                        let mut ic_cascade_start = 0usize;
                        let ic_floor_word_off = if pic_inline || mic_inline || ic_stub_emitted || emit_method_invoke_cache {
                            crate::inline_cache_pic::ic_stack_floor_word_offset(&self.helpers)
                        } else {
                            None
                        };
                        if let (Some(floor_off), true) =
                            (ic_floor_word_off, ic_stack_guard_enabled())
                        {
                            // Cast: `ic_stack_floor_word_offset` answers only for
                            // a non-zero mirror displacement from the startup
                            // probe, a TEB/TLS offset well inside u32.
                            self.emit_mov_rax_tls_disp32(jit_thread_tls_disp() as u32);
                            self.emit_test_r64_r64(RAX);
                            ic_stack_no_thread_patch = Some(self.emit_jcc_rel32_patch(0x84)); // JZ → cold
                                                                                              // CMP RSP, [RAX + disp32]: REX.W 3B /r, ModRM
                                                                                              // mod=10 reg=RSP(4) r/m=RAX(0).
                            self.buf.emit(&[0x48, 0x3B, 0xA0]);
                            self.buf.emit(&floor_off.to_le_bytes());
                            ic_stack_low_patch = Some(self.emit_jcc_rel32_patch(0x86)); // JBE → cold
                            ic_floor_from_thread = true;
                            ic_cascade_start = self.buf.pos();
                        } else if (pic_inline || mic_inline || ic_stub_emitted || emit_method_invoke_cache)
                            && self.stack_floor_slot_off != 0
                            && self.helpers.native_stack_floor_fn != 0
                            && ic_stack_guard_enabled()
                        {
                            self.emit_cmp_r64_rbp_local(RSP, self.stack_floor_slot_off);
                            ic_stack_low_patch = Some(self.emit_jcc_rel32_patch(0x86)); // JBE → cold
                            ic_cascade_start = self.buf.pos();
                        }

                        if emit_method_invoke_cache {
                            self.emit_method_invoke_cache_prefix(
                                info,
                                info_ref,
                                args_base_offset,
                                lazy_ic_hit_republish,
                                &mut done_patches32,
                            );
                        }

                        if let (true, Some(pic)) = (pic_inline, pic_ptr) {
                            // Cache the layout constants locally so a
                            // future const-rename in lib.rs surfaces as
                            // a compile error here.
                            const CLASS_ID_OFFS: [u8; 4] = [0, 4, 8, 12];
                            const ENTRY_PTR_OFFS: [u8; 4] = [16, 24, 32, 40];

                            // Compile-time sanity: the byte offsets we
                            // hardcode in the encodings below must
                            // match the public constants exported by
                            // `JitPICSlot`. A mismatch here would
                            // silently dispatch to a stale entry_ptr.
                            const _: () = assert!(
                                crate::JitPICSlot::CLASS_ID_OFFSETS[0] == 0
                                    && crate::JitPICSlot::CLASS_ID_OFFSETS[1] == 4
                                    && crate::JitPICSlot::CLASS_ID_OFFSETS[2] == 8
                                    && crate::JitPICSlot::CLASS_ID_OFFSETS[3] == 12
                                    && crate::JitPICSlot::ENTRY_PTR_OFFSETS[0] == 16
                                    && crate::JitPICSlot::ENTRY_PTR_OFFSETS[1] == 24
                                    && crate::JitPICSlot::ENTRY_PTR_OFFSETS[2] == 32
                                    && crate::JitPICSlot::ENTRY_PTR_OFFSETS[3] == 40
                                    // `BTR R11, 0` below strips exactly bit 0.
                                    && crate::JIT_IC_NEEDS_CONTEXT_TAG == 1
                            );

                            // R10 = pic_ptr (imm64, fixed 10-byte form).
                            // Task #60: use the fixed-length form so the
                            // unroll duplicator can locate the imm64 at a
                            // deterministic offset (+2 from the MOV start)
                            // and patch it to a freshly-allocated PIC slot
                            // per unrolled copy. Without this, all copies
                            // would share the original slot — per-iteration
                            // cache hits would collide and miss across
                            // copies for any receiver-type-varying loop.
                            let ic_imm64_off = self.buf.pos() + 2;
                            self.emit_mov_imm64_full(R10, pic as *const _ as i64); // Cast: function pointer for JIT call target
                            self.ic_patches
                                .push((ic_imm64_off, 1, pic as *const _ as usize)); // 1 = PIC
                                                                                    // SECURITY FIX (V1) INVARIANT: R10 holds the
                                                                                    // PIC slot base pointer from here until each
                                                                                    // per-slot `MOV R11,[R10+disp]; CALL R11`
                                                                                    // below. Code emitted in this window (receiver
                                                                                    // load, NPE guard, class_id load, the per-slot
                                                                                    // CMP/JNE cascade) must touch only RAX and R10
                                                                                    // itself — it must NOT route through any helper
                                                                                    // that clobbers R10 (e.g. emit_bounds_check,
                                                                                    // SIMD lowering). The ABI marshalling below
                                                                                    // targets ARG_REGS only (RCX/RDX/RSI/R8/R9/RDI
                                                                                    // — never R10), so R10 stays the trusted base.

                            // ---- Hoist callee ABI marshalling out of
                            // the 4-way cascade. Previously each slot
                            // re-loaded `vm_ptr + n args` into
                            // ARG_REGS[0..=n] (~26 bytes per slot on
                            // x86-64 SysV with n=4), tripling the
                            // marshalling cost. Hoisting once:
                            //
                            //   * Cuts ~78 bytes per PIC site (≈26B
                            //     × 2 redundant copies).
                            //   * Keeps the per-slot body to: type
                            //     CMP + JNE, needs-ctx CMP + JE,
                            //     CALL [R10+disp], JMP rel32 .done.
                            //   * Args remain live across the inter-
                            //     slot CMP/JNE pairs because those
                            //     instructions touch only RAX and
                            //     R10 (neither is in ARG_REGS).
                            //   * On a successful CALL, ARG_REGS are
                            //     caller-saved and may be clobbered
                            //     by the callee — but we JMP to
                            //     .done immediately afterwards, so
                            //     no other slot's CALL needs them.
                            //   * On the miss path, the slow-path
                            //     prelude (~line 10709) overwrites
                            //     ARG_REGS with its helper-ABI
                            //     arguments (vm_ptr, info, buf,
                            //     n[, mic, pic]) before the helper
                            //     call. The hoisted values are
                            //     already dead at that point.
                            //
                            // We must materialize the receiver-load
                            // *first* (its source spill could alias
                            // ARG_REGS[0] in degenerate frames) and
                            // RAX holds the class_id used by every
                            // per-slot CMP, so RAX is loaded last.
                            self.emit_load_vm_context(ARG_REGS[0]);
                            for j in 0..n {
                                let spill_off = args_base_offset + ((n - 1 - j) as i32) * 8; // Cast: x86-64 immediate encoding
                                self.emit_load_local(ARG_REGS[j + 1], spill_off);
                            }
                            // ARG_REGS[1] now holds the receiver
                            // (arg_slots[0]). Reuse it as the source
                            // of the class_id load so we avoid an
                            // extra reload from the receiver spill —
                            // this saves an additional ~5 bytes vs
                            // the prior `emit_load_local(RAX, ...)`.
                            // MOV EAX, dword [ARG_REGS[1]] — load
                            // class_id (ObjectHeader+0). Encoding
                            // depends on whether the receiver reg is
                            // an extended register (R8+).
                            // round-7 audit (bug 4): ARG_REGS[1]
                            // here is the receiver register — the
                            // loop above (`emit_load_local(ARG_REGS[j+1], …)`)
                            // wrote `arg_slots[0]` (= the receiver
                            // by JVM invokevirtual/interface
                            // calling convention) into ARG_REGS[0+1].
                            // The debug_assert below is therefore
                            // checking the right register; verified.
                            let recv_reg = ARG_REGS[1];
                            // NPE guard: a null receiver must not be
                            // dereferenced by the class_id load below
                            // (`MOV EAX, [recv_reg]`) — that faults at
                            // address 0 and SIGSEGVs the VM (observed
                            // in Tomcat: JIT-compiled `Locale.hashCode`
                            // dispatching `BaseLocale.hashCode` on a
                            // null receiver). Route a null receiver to
                            // the `.miss` slow path, which decodes args
                            // and bails to the interpreter where the
                            // invokevirtual receiver null check raises
                            // a proper NullPointerException.
                            //   TEST recv_reg, recv_reg
                            if recv_reg >= 8 {
                                self.buf.emit(&[
                                    0x4D,
                                    0x85,
                                    0xC0 | ((recv_reg & 7) << 3) | (recv_reg & 7),
                                ]);
                            } else {
                                self.buf.emit(&[
                                    0x48,
                                    0x85,
                                    0xC0 | ((recv_reg & 7) << 3) | (recv_reg & 7),
                                ]);
                            }
                            //   JZ rel32 → .miss (patched at miss_off)
                            self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                            let pic_null_miss_patch = self.buf.pos() - 4;
                            // ARRAY-RECEIVER GUARD. `ObjectHeader.class_id` sits at offset 0 for objects AND for
                            // arrays, and a reference array stores its
                            // COMPONENT class id there — a `Foo[]` and a
                            // `Foo` present the SAME 4-byte guard word. A
                            // class-id-only guard therefore lets a site
                            // warmed on a `Foo` receiver dispatch a later
                            // `Foo[]` receiver straight into `Foo`'s own
                            // method body, where the first `checkcast Foo`
                            // throws `class [LFoo; cannot be cast to class
                            // Foo`. (The helper never INSTALLS an entry for
                            // an array receiver — `cacheable_receiver` is
                            // false for `ObjectKind::Array` — so only the
                            // consumption guard was ever wrong.)
                            // the `kind` bits of the mark word's byte 6
                            // (`KIND_TAGS_BYTE_OFFSET`) separate the two;
                            // anything that is not a plain object goes
                            // to the miss path, which resolves on the real
                            // receiver.
                            //   TEST BYTE [recv_reg + KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK
                            // mod=01 (disp8) / reg=/7 (CMP imm8) / rm=recv.
                            // `recv_reg & 7 != 4` is already asserted below
                            // (mod=00 would need a SIB there), and mod=01
                            // makes rm==5 an ordinary [reg+disp8].
                            //
                            // Skipped where no array can be the receiver
                            // (`ic_receiver_may_be_array`): the screen is
                            // then a compare that never fails.
                            let pic_kind_miss_patch = if ic_receiver_may_be_array {
                                if recv_reg >= 8 {
                                    self.buf.emit_byte(0x41); // REX.B
                                }
                                self.buf.emit(&[
                                    0xF6,
                                    0x40 | (recv_reg & 7),
                                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                                    cratonvm_types::KIND_TAG_BYTE_MASK,
                                ]);
                                //   JNE rel32 → .miss
                                self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                Some(self.buf.pos() - 4)
                            } else {
                                None
                            };
                            // MED (round-5 review): mod=00 encoding
                            // reuses the low-3 bits of the register as
                            // r/m, where r/m==4 (RSP/R12) means
                            // SIB-follows and r/m==5 (RBP/R13) means
                            // RIP-relative — both would mis-encode this
                            // displacement-free load. Safe today
                            // because RCX(.1)/RDX(.2)/RSI(.6) are all
                            // outside {4,5}, but any future ARG_REGS
                            // shuffle would silently corrupt this PIC
                            // slot. Trap brittleness in debug builds.
                            debug_assert!(
                                (recv_reg & 7) != 4 && (recv_reg & 7) != 5,
                                "PIC slot-0 mod=00 requires ARG_REGS[1] low3 not in {{4,5}} (got {})",
                                recv_reg,
                            );
                            if recv_reg >= 8 {
                                // REX.B + 8B /r, modrm = mod(00) reg(EAX=0) rm(recv&7)
                                self.buf.emit(&[0x41, 0x8B, recv_reg & 7]);
                            } else {
                                // 8B /r, modrm = mod(00) reg(EAX=0) rm(recv)
                                self.buf.emit(&[0x8B, recv_reg]);
                            }

                            // Per-slot cascade. Inter-slot `jne` jumps
                            // are rel8 and patched once we know the
                            // start of the next slot. Final slot's
                            // `jne` and every `je needs_ctx → .miss`
                            // jump to the shared `.miss` label.
                            //
                            // `slot_starts[i]` is the byte position of
                            // slot i's first emitted byte (the CMP
                            // opcode); used to resolve inter-slot
                            // `jne` rel8 patches once all slots are
                            // emitted.
                            let mut slot_starts: [usize; 4] = [0; 4];
                            // (patch_pos, target_slot_index) for each
                            // inter-slot `jne` rel8 that needs to land
                            // at the start of slot `target_slot_index`.
                            let mut next_slot_patches: Vec<(usize, usize)> = Vec::new();
                            // CRIT-3 — miss branches use rel32 form
                            // unconditionally. With n>=5 args on Linux
                            // the cumulative body across all three
                            // slots + slow-path prelude can exceed 127
                            // bytes, overflowing the previous rel8
                            // encoding (`0x74`/`0x75`). The rel32
                            // forms (`0x0F 0x84`/`0x0F 0x85` + 4-byte
                            // disp) always fit. `miss_patches_rel32`
                            // stores the byte offset of the 4-byte
                            // displacement immediate, patched at the
                            // shared `.miss` label below.
                            // Seeded with the receiver-null-check JZ
                            // emitted above the cascade so it is patched
                            // to the same shared `.miss` target.
                            let mut miss_patches_rel32: Vec<usize> = vec![pic_null_miss_patch];
                            miss_patches_rel32.extend(pic_kind_miss_patch);
                            // rel32 sites of each way's `JC → .call` and
                            // `JMP → .noctx`, patched at the shared tail
                            // emitted after the cascade.
                            let mut pic_call_patches: Vec<usize> =
                                Vec::with_capacity(crate::JIT_PIC_ENTRIES);
                            let mut pic_noctx_patches: Vec<usize> =
                                Vec::with_capacity(crate::JIT_PIC_ENTRIES);
                            // Round 12 wave 6 (lane mega5, W5-4): once the
                            // site has overflowed its ways, a way-0 miss goes
                            // straight to the hashed stub below (cell first)
                            // instead of down ways 1..3. Only in front of a
                            // stub this site really emits: the gate's target
                            // is `.miss`, where that stub starts.
                            let mega_gate = sp_inline_mega_enabled()
                                && crate::runtime_lowering::megamorphic_gate_admitted(
                                    pic as usize,
                                    n,
                                );
                            // Round 12 wave 6 (lane mega5, W5-6): this
                            // cascade reads the inline ways for every
                            // receiver, so the helper may publish a receiver
                            // inline-only until the site turns megamorphic.
                            {
                                // SAFETY: `pic` names a slot this compile's
                                // request owns (boxed, alive for the compile
                                // and then retained by the artifact), the
                                // same address the imm64 above bakes.
                                unsafe { (*pic).note_inline_ways_probed() };
                            }

                            // Round 12 wave 7 (lane mega6, W7-1): the gate
                            // enters the stub past its receiver reload, null
                            // test, kind screen and class-id reload. At the
                            // gate EAX holds the class id of a receiver this
                            // cascade null-tested and, where an array can
                            // reach the site (`ic_receiver_may_be_array`),
                            // kind-screened: the stub's gate-entry contract.
                            let mega_gate_fast = mega_gate
                                && crate::runtime_lowering::mega_gate_fast_entry_enabled();
                            // JIT round 17 wave 1 (lane mega, MW13-4): the
                            // gate goes to the inline vtable-column probe
                            // emitted after the shared call tail below; its
                            // misses take the gate's own destination.
                            //
                            // JIT round 19 (lane mega19, MEGA5-18-1): the
                            // single-pass twins of the optimizing tier's
                            // round-18 switches, each its own opt-in switch.
                            // `sp_stage_once` (`CRATONVM_JIT_SP_MEGA_STAGE_ONCE`):
                            // the gate goes to the stub's whole probe in
                            // RAX/R10/R11, whose hit takes the ways' own
                            // `.call` / `.noctx` tail with the hoisted ABI --
                            // the column probe's route, with the stub's three
                            // tables; it supersedes the column probe.
                            // `sp_gate_first` (`CRATONVM_JIT_SP_MEGA_GATE_FIRST`):
                            // the gate is tested ahead of way 0, so an
                            // overflowed site never runs way 0's
                            // receiver-dependent compare.
                            let sp_stage_once =
                                mega_gate && crate::runtime_lowering::sp_mega_stage_once_enabled();
                            // MEGA2-3: a seeded site kept for the probe tests
                            // its gate first whatever the gate-first switch.
                            let sp_gate_first = mega_gate
                                && (crate::runtime_lowering::sp_mega_gate_first_enabled()
                                    || sp_seeded_keep_probe);
                            // MEGA5-18-3 (`CRATONVM_JIT_MEGA_PROBE_MISS_TO_HELPER`):
                            // a register-probe miss goes to the resolving
                            // helper, past the stub's second probe of the same
                            // tables; patched where the helper's setup starts.
                            let probe_miss_to_helper = sp_stage_once
                                && crate::runtime_lowering::mega_probe_miss_to_helper_enabled();
                            if sp_gate_first
                                && crate::runtime_lowering::mega_gate_first_no_inline_publish_enabled()
                            {
                                // MEGA5-18-4: once the gate is set this site
                                // reads no inline way (and it has no MIC).
                                // SAFETY: as `note_inline_ways_probed` above.
                                unsafe { (*pic).note_gate_tested_first() };
                            }
                            let column_probe = mega_gate
                                && (sp_stage_once
                                    || crate::runtime_lowering::mega_vtable_column_admitted(
                                        pic as usize,
                                        n,
                                        op != 0xb6,
                                    ));
                            let mut column_gate: Option<usize> = None;
                            if column_probe && sp_ic_site_trace() {
                                eprintln!(
                                    "[SP_IC_SITE] {}||{}.{}{} pc={} {}",
                                    self.method_label,
                                    info_ref.class_name,
                                    info_ref.method_name,
                                    info_ref.descriptor,
                                    pc,
                                    if sp_stage_once {
                                        "mega-stage-once"
                                    } else {
                                        "mega-vtable-column"
                                    },
                                );
                            }

                            for i in 0..crate::JIT_PIC_ENTRIES {
                                slot_starts[i] = self.buf.pos();
                                // The gate sits after way 0 (round 12 wave
                                // 6), or ahead of it under `sp_gate_first`:
                                // the state is the same at both places (EAX
                                // the class id, R10 the PIC, the ABI hoisted),
                                // and nothing jumps to way 0's start.
                                let gate_here = if sp_gate_first { i == 0 } else { i == 1 };
                                if gate_here && mega_gate {
                                    // CMP dword [R10+MEGAMORPHIC_OFFSET], 0;
                                    // JNE rel32 -> .miss (or the stub's gate
                                    // entry). Flags only: EAX, R10 and the
                                    // hoisted ARG_REGS survive.
                                    let gate = crate::runtime_lowering::emit_megamorphic_gate(
                                        &mut self.buf,
                                    );
                                    if column_probe {
                                        column_gate = Some(gate);
                                    } else if mega_gate_fast {
                                        mega_gate_patch.push(gate);
                                    } else {
                                        miss_patches_rel32.push(gate);
                                    }
                                }

                                // CMP EAX, dword [R10 + CLASS_ID_OFFS[i]]
                                // For disp == 0 (slot 0 today) emit the
                                // mod=00 form with no displacement byte:
                                // saves 1 byte per JIT site on the hot
                                // slot-0 cascade. For disp != 0 use the
                                // mod=01 (disp8) form. R10 in mod=00 is
                                // ModRM 00_000_010 = 0x02.
                                if CLASS_ID_OFFS[i] == 0 {
                                    // 3 bytes: REX.B + 3B /r + ModRM(00,000,010).
                                    self.buf.emit(&[0x41, 0x3B, 0x02]);
                                } else {
                                    // 4 bytes: REX.B + 3B /r + ModRM(01,000,010) + disp8.
                                    self.buf.emit(&[0x41, 0x3B, 0x42, CLASS_ID_OFFS[i]]);
                                }

                                if i + 1 < crate::JIT_PIC_ENTRIES {
                                    // JNE rel32 → start of slot i+1
                                    // (patched below once slot i+1's
                                    // start position is known).
                                    //
                                    // This was rel8 on the claim that "a
                                    // single slot body is ~30 bytes for
                                    // n<=5". That stopped being true: a
                                    // slot body now also carries the
                                    // post-call innermost-RBP republish and
                                    // the callee-deopt service check, which
                                    // together put it well past 127 bytes.
                                    // The patch truncated the displacement
                                    // (`rel as u8`) behind a
                                    // `debug_assert!`, so release builds
                                    // silently got `JNE -128` — a branch
                                    // into the middle of the pre-call
                                    // shadow-stack push, which then ran as
                                    // an unguarded infinite push loop and
                                    // walked off the end of the thread's
                                    // shadow buffer (SIGSEGV ~170 MiB
                                    // later, at the arena end). Only
                                    // reachable with raw JIT-to-JIT calls
                                    // enabled, which is why the closed gate
                                    // hid it. rel32 has no such cliff.
                                    self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                    let patch = self.buf.pos() - 4;
                                    next_slot_patches.push((patch, i + 1));
                                } else {
                                    // Final slot: JNE rel32 → .miss
                                    // (6 bytes: 0x0F 0x85 + i32 disp).
                                    // The final slot's miss target sits past
                                    // the whole cascade and may be reachable
                                    // in rel8 but we keep rel32 for
                                    // consistency with slot 0/1 and
                                    // because the slow-path prelude
                                    // following the cascade can push
                                    // the distance over 127 bytes.
                                    self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                    miss_patches_rel32.push(self.buf.pos() - 4);
                                }

                                // Capture the way's target ONCE. The word
                                // carries the entry AND its calling
                                // convention, so the ABI chosen below and
                                // the CALL can never come from two
                                // different publications of this way.
                                // MOV R11, qword [R10 + ENTRY_PTR_OFFS[i]]
                                // 4 bytes: REX.WRB + 8B /r + ModRM(01,R11,R10) + disp8
                                self.buf.emit(&[0x4D, 0x8B, 0x5A, ENTRY_PTR_OFFS[i]]);
                                // A receiver profile may pre-populate only
                                // class_id, and a retired way's word is
                                // zeroed: never CALL a zero target.
                                // TEST R11, R11 (3 bytes)
                                self.buf.emit(&[0x4D, 0x85, 0xDB]);
                                // JZ rel32 → .miss
                                self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                                miss_patches_rel32.push(self.buf.pos() - 4);

                                // BTR R11, 0 — CF := the needs-context
                                // tag, R11 := the bare entry address.
                                // 5 bytes: REX.WB + 0F BA /6 + ModRM(11,110,R11) + imm8
                                self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]);

                                // SHARED POST-CALL TAIL (round 9 wave 2,
                                // `pic-cascade-repeats-the-post-call-tail-per-way`).
                                // Every way used to carry its own no-context
                                // marshalling block, `CALL R11`, innermost-RBP
                                // republish, callee-deopt service check and
                                // `JMP .done` — four copies of a ~100+ byte
                                // tail at every virtual/interface site, three
                                // of them dead at a monomorphic site. The ways
                                // differ only in which ABI shape the tag
                                // selects; the target is already in R11 and the
                                // context ABI is already live in ARG_REGS (the
                                // hoist above), so every way now branches to ONE
                                // `.noctx` / `.call` pair emitted after the
                                // cascade:
                                //
                                //   JC  rel32 → .call    ; tag set: context ABI live
                                //   JMP rel32 → .noctx   ; (the last way falls in)
                                //
                                // The hot path is unchanged in length: a way-0
                                // context hit takes one taken `JC` where it took
                                // a not-taken `JNC` plus a taken `JMP`; a
                                // no-context hit takes one `JMP` where it took a
                                // taken `JNC`. INVARIANT (unchanged): nothing
                                // between the entry-word capture above and the
                                // shared `CALL R11` writes R11 — the path is
                                // `BTR`, a jump, and (no-context only) the
                                // ARG_REGS reloads.
                                //
                                // JC rel32 → .call (6 bytes: 0F 82 + i32).
                                self.buf.emit(&[0x0F, 0x82, 0x00, 0x00, 0x00, 0x00]);
                                pic_call_patches.push(self.buf.pos() - 4);
                                if i + 1 < crate::JIT_PIC_ENTRIES {
                                    // JMP rel32 → .noctx (5 bytes: E9 + i32).
                                    self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                                    pic_noctx_patches.push(self.buf.pos() - 4);
                                }
                                // The last way falls through into `.noctx`.
                            }

                            // .noctx: Java args start at ARG_REGS[0].
                            let noctx_off = self.buf.pos();
                            for patch in &pic_noctx_patches {
                                // Widening: usize offset -> i64 (rel math)
                                let rel = (noctx_off as i64) - (*patch as i64 + 4);
                                debug_assert!((i32::MIN as i64..=i32::MAX as i64).contains(&rel));
                                // Cast: rel32 displacement; on Err the buffer
                                // is marked overflowed and the compile bails.
                                self.buf.try_patch_i32(*patch, rel as i32).ok();
                            }
                            for j in 0..n {
                                let spill_off = args_base_offset + ((n - 1 - j) as i32) * 8; // Cast: x86-64 immediate encoding
                                self.emit_load_local(ARG_REGS[j], spill_off);
                            }

                            // .call — the context ABI arrives here with the
                            // hoisted ARG_REGS still live (the cascade touched
                            // only RAX, R10 and R11 since the hoist).
                            let call_off = self.buf.pos();
                            for patch in &pic_call_patches {
                                // Widening: usize offset -> i64 (rel math)
                                let rel = (call_off as i64) - (*patch as i64 + 4);
                                debug_assert!((i32::MIN as i64..=i32::MAX as i64).contains(&rel));
                                // Cast: rel32 displacement (see above).
                                self.buf.try_patch_i32(*patch, rel as i32).ok();
                            }

                            // SECURITY FIX (V1): do NOT keep the call
                            // target live in memory addressed through
                            // R10 across an indirect CALL — R10 is the
                            // shared bounds-check / SIMD scratch
                            // register. The target was captured into
                            // R11 by the way that hit (a caller-saved
                            // scratch reg that is NOT in ARG_REGS /
                            // SCRATCH_REGS / LOCAL_REGS and is clobbered
                            // by the call anyway) and R10 is not read
                            // again.
                            //
                            // CALL R11  (3 bytes: REX.B + FF /2 + ModRM(11,/2,R11))
                            self.buf.emit(&[0x41, 0xFF, 0xD3]);
                            // gcd d9/d row 2: the hit's buffer is its service copy.
                            let ret = self.buf.pos();
                            self.note_service_arg_copies(
                                ret,
                                args_base_offset,
                                &ic_hit_window_oops,
                            );
                            // A compiled callee's prologue publishes
                            // ITS rbp into the innermost-RBP mirror the
                            // GC reads. Nothing on the return path of a
                            // raw JIT-to-JIT call restores this
                            // caller's, so without this the mirror
                            // names a DEAD frame from here on and the
                            // next GC applies THIS method's oop map to
                            // whatever has since reused that stack
                            // memory. See
                            // `conservative_roots::top_rbp_mirror_write`
                            // for the Rust-side analogue of the same
                            // contract.
                            // MEGA619-1 (opt-in): not at a return-tail site.
                            if !lazy_ic_hit_republish {
                                self.emit_post_call_rbp_republish();
                            }
                            // This call's map and shadow reload run at the
                            // shared `.done` below: the site is still open.
                            self.emit_inline_callee_deopt_check_site_open(
                                info,
                                n,
                                args_base_offset,
                            );

                            // JMP rel32 → .done (5 bytes: E9 + i32). The
                            // hashed stub and the slow path sit between
                            // here and `.done`.
                            self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                            done_patches32.push(self.buf.pos() - 4);

                            // JIT round 17 wave 1 (lane mega, MW13-4): the
                            // megamorphic vtable column, out of line behind
                            // the `JMP .done` above, reached only from the
                            // gate. EAX = the class id (zero-extended by the
                            // cascade's `MOV EAX, [recv]`), R10 = this PIC,
                            // the context ABI hoisted: the probe's contract.
                            // A hit leaves the cell's tagged word in R11 and
                            // takes the ways' own `.call` / `.noctx` tail,
                            // whose register state (ARG_REGS hoisted, only
                            // RAX/R10/R11 touched) it shares. Every miss keeps
                            // EAX = the class id and goes where the gate went.
                            if let Some(gate) = column_gate.take() {
                                // Widening: usize offsets -> i64 (rel math).
                                let rel = (self.buf.pos() as i64) - (gate as i64 + 4);
                                // Cast: rel32 displacement within one body;
                                // on Err the buffer is marked overflowed and
                                // the compile bails.
                                self.buf.try_patch_i32(gate, rel as i32).ok();
                                let misses = if sp_stage_once {
                                    // MEGA5-18-1: the stub's whole probe. It
                                    // re-bakes this PIC after its cell step;
                                    // those immediates follow the slot into a
                                    // loop copy as the stub's own does.
                                    let mut imm_ats: Vec<usize> = Vec::new();
                                    let cell_first =
                                        crate::runtime_lowering::mega_register_probe_cell_first();
                                    let misses =
                                        crate::runtime_lowering::emit_mega_register_probe_located(
                                            &mut self.buf,
                                            pic as usize,
                                            cell_first,
                                            &mut imm_ats,
                                        );
                                    {
                                        for imm_at in imm_ats {
                                            self.ic_patches.push((imm_at, 1, pic as usize));
                                        }
                                    }
                                    misses
                                } else if let Some(binding) =
                                    crate::runtime_lowering::mega_vtable_column_baked_for(
                                        pic as usize,
                                        op != 0xb6,
                                    )
                                {
                                    // JIT round 21 wave 4, lane ic (IC21-2):
                                    // the compile-time bind as immediates,
                                    // same contract. Not PIC addresses, so a
                                    // loop copy keeps them (they name the
                                    // site's resolution, not its slot).
                                    crate::runtime_lowering::emit_mega_vtable_column_probe_baked(
                                        &mut self.buf,
                                        binding,
                                    )
                                } else {
                                    crate::runtime_lowering::emit_mega_vtable_column_probe(
                                        &mut self.buf,
                                    )
                                };
                                if probe_miss_to_helper {
                                    probe_helper_patches.extend(misses);
                                } else if mega_gate_fast {
                                    mega_gate_patch.extend(misses);
                                } else {
                                    miss_patches_rel32.extend(misses);
                                }
                                // BTR R11, 0 — CF := needs-context, R11 := entry.
                                self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]);
                                // JC rel32 → .call (backward).
                                self.buf.emit(&[0x0F, 0x82, 0x00, 0x00, 0x00, 0x00]);
                                let to_call = self.buf.pos() - 4;
                                // Widening: usize offsets -> i64 (rel math).
                                let rel = (call_off as i64) - (to_call as i64 + 4);
                                // Cast: rel32 displacement (see above).
                                self.buf.try_patch_i32(to_call, rel as i32).ok();
                                // JMP rel32 → .noctx (backward).
                                self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                                let to_noctx = self.buf.pos() - 4;
                                // Widening: usize offsets -> i64 (rel math).
                                let rel = (noctx_off as i64) - (to_noctx as i64 + 4);
                                // Cast: rel32 displacement (see above).
                                self.buf.try_patch_i32(to_noctx, rel as i32).ok();
                            }

                            // Resolve inter-slot `jne` rel8 patches now
                            // that every slot's start is known.
                            for (jne_patch, target_slot) in &next_slot_patches {
                                let slot_start = slot_starts[*target_slot];
                                // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                let rel = (slot_start as i64) - (*jne_patch as i64 + 4);
                                debug_assert!(
                                    // Widening: i32 bound -> i64 (range comparison)
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                    "inline PIC inter-slot jne overflowed rel32 ({} bytes)",
                                    rel
                                );
                                // Cast: rel32 displacement. On Err,
                                // try_patch_i32 marks the buffer
                                // overflowed and the compile bails.
                                self.buf.try_patch_i32(*jne_patch, rel as i32).ok();
                            }

                            // .miss: patch all `je needs_ctx → .miss`
                            // and the final slot's `jne → .miss` to land
                            // HERE — the start of the slow-path block
                            // emitted below.  CRIT-3: these are all
                            // rel32 form, so the patch site holds a
                            // 4-byte signed displacement computed
                            // from the byte AFTER the immediate
                            // (patch + 4) to the target.
                            let miss_off = self.buf.pos();
                            for patch in &miss_patches_rel32 {
                                // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                let rel = (miss_off as i64) - (*patch as i64 + 4);
                                debug_assert!(
                                    // Widening: i32 bound -> i64 (range comparison)
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                    "inline PIC miss branch overflowed rel32 ({} bytes)",
                                    rel
                                );
                                self.buf
                                    .try_patch_i32(*patch, rel as i32) // Cast: rel32 displacement
                                    .ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                            }
                            // The MIC arm maintains its own rel32 miss
                            // patches; PIC uses the vector above.
                        } else if let (true, Some(mic)) = (mic_inline, mic_ptr) {
                            // R10 = mic_ptr (imm64, fixed 10-byte form).
                            // Task #60: same rationale as the PIC arm —
                            // fixed-length encoding gives the unroll
                            // duplicator a deterministic imm64 location
                            // to overwrite with a per-copy fresh MIC slot.
                            let ic_imm64_off = self.buf.pos() + 2;
                            self.emit_mov_imm64_full(R10, mic as *const _ as i64); // Cast: function pointer for JIT call target
                            self.ic_patches
                                .push((ic_imm64_off, 0, mic as *const _ as usize)); // 0 = MIC
                                                                                    // SECURITY FIX (V1) INVARIANT: R10 holds the
                                                                                    // MIC slot base pointer from here until the
                                                                                    // `MOV R11,[R10+8]; CALL R11` below. Every
                                                                                    // instruction emitted in this window (receiver
                                                                                    // load into RAX, NPE guard, class_id/needs-ctx
                                                                                    // checks, and the ARG_REGS marshalling loop)
                                                                                    // touches only RAX, R10, and ARG_REGS — never
                                                                                    // R10 as a destination — so the call target
                                                                                    // base cannot be perturbed before the CALL.

                            // Load receiver pointer into RAX. Receiver is
                            // arg_slots[0], spilled at the *highest* offset
                            // (lowest address) in the args buffer.
                            let receiver_spill = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                            self.emit_load_local(RAX, receiver_spill);

                            // NPE guard: a null receiver must not reach
                            // the `MOV EAX,[RAX]` class_id load below —
                            // dereferencing address 0 SIGSEGVs the VM.
                            // Route null to `.miss` (slow helper →
                            // interpreter), which raises a proper
                            // NullPointerException per JVMS invokevirtual.
                            //   TEST RAX, RAX  (48 85 C0)
                            self.buf.emit(&[0x48, 0x85, 0xC0]);
                            //   JZ rel32 → .miss. The dual-ABI
                            // marshalling blocks make the miss span larger
                            // than rel8 for otherwise tiny callees.
                            self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                            mic_miss_patches32.push(self.buf.pos() - 4);

                            // ARRAY-RECEIVER GUARD. `ObjectHeader.class_id` sits at offset 0 for objects AND for
                            // arrays, and a reference array stores its
                            // COMPONENT class id there — a `Foo[]` and a
                            // `Foo` present the SAME 4-byte guard word. A
                            // class-id-only guard therefore lets a site
                            // warmed on a `Foo` receiver dispatch a later
                            // `Foo[]` receiver straight into `Foo`'s own
                            // method body, where the first `checkcast Foo`
                            // throws `class [LFoo; cannot be cast to class
                            // Foo`. (The helper never INSTALLS an entry for
                            // an array receiver — `cacheable_receiver` is
                            // false for `ObjectKind::Array` — so only the
                            // consumption guard was ever wrong.)
                            // `ObjectHeader.kind` (offset 4) separates the
                            // two; anything that is not a plain object goes
                            // to the miss path, which resolves on the real
                            // receiver. Skipped where no array can be the
                            // receiver (`ic_receiver_may_be_array`).
                            //   TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK (F6 /0 ib: kind bits only)
                            if ic_receiver_may_be_array {
                                self.buf.emit(&[
                                    0xF6,
                                    0x40,
                                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                                    cratonvm_types::KIND_TAG_BYTE_MASK,
                                ]);
                                //   JNE rel32 → .miss
                                self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                mic_miss_patches32.push(self.buf.pos() - 4);
                            }

                            // MOV EAX, dword [RAX]  — load class_id (ObjectHeader+0).
                            // 2 bytes: 8B 00
                            self.buf.emit(&[0x8B, 0x00]);

                            // JIT round 19 wave 2 (lane mega2, MEGA19-2,
                            // `CRATONVM_JIT_SP_MEGA_MIC_GATE`): the MIC-only
                            // arm's megamorphic gate, ahead of its compare,
                            // and its screened misses to the hashed stub's
                            // gate entry. From here on EAX is the class id of
                            // a receiver this arm null-tested and, where an
                            // array can reach the site, kind-screened (the
                            // gate entry's contract, R10 free there); RSP is
                            // still the frame's (the outgoing block is
                            // reserved past the compare). Both slots are
                            // baked in the fixed form and filed in
                            // `ic_patches`, so a loop copy tests its own.
                            let gate_entry_admitted = |pic: usize| {
                                ic_stub_emitted
                                    && crate::runtime_lowering::mega_gate_fast_entry_enabled()
                                    && crate::runtime_lowering::megamorphic_gate_admitted(pic, n)
                            };
                            let mic_gate = pic_ptr.filter(|&pic| {
                                crate::runtime_lowering::sp_mega_mic_gate_enabled()
                                    && gate_entry_admitted(pic as usize)
                            });
                            // JIT round 19 wave 4 (lane megaosr4, MEGA2-4,
                            // `CRATONVM_JIT_SP_MEGA_MIC_MISS_GATE_ENTRY`): the
                            // two screened misses below go to the stub's gate
                            // entry under the gate's own admission, with or
                            // without the gate. Same EAX / RSP contract.
                            let miss_to_gate_entry = mic_gate.is_some()
                                || pic_ptr.is_some_and(|pic| {
                                    crate::runtime_lowering::sp_mega_mic_miss_gate_entry_enabled()
                                        && gate_entry_admitted(pic as usize)
                                });
                            if let Some(pic) = mic_gate {
                                let pic_imm = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(R10, pic as *const _ as i64); // Cast: slot address baked as an imm64
                                self.ic_patches.push((pic_imm, 1, pic as *const _ as usize)); // 1 = PIC
                                mega_gate_patch.push(
                                    crate::runtime_lowering::emit_megamorphic_gate(&mut self.buf),
                                );
                                let mic_imm = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(R10, mic as *const _ as i64); // Cast: slot address baked as an imm64
                                self.ic_patches.push((mic_imm, 0, mic as *const _ as usize));
                                // 0 = MIC
                            }

                            // CMP EAX, dword [R10 + 0]  — vs cached_class_id.
                            // 3 bytes: REX.B (0x41) + 3B /r + modrm(00 000 010)
                            self.buf.emit(&[0x41, 0x3B, 0x02]);

                            // JNE rel32 → .miss (MEGA19-2: the stub's gate entry).
                            self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                            if miss_to_gate_entry {
                                mega_gate_patch.push(self.buf.pos() - 4);
                            } else {
                                mic_miss_patches32.push(self.buf.pos() - 4);
                            }

                            // Capture the tagged target word ONCE: the
                            // entry address with the needs-context flag in
                            // bit 0, so the ABI and the call target come
                            // from the same publication.
                            // MOV R11, qword [R10 + 8]
                            // 4 bytes: REX.WRB + 8B /r + ModRM(01,R11,R10) + disp8
                            self.buf.emit(&[0x4D, 0x8B, 0x5A, 0x08]);
                            // A profiled MIC can publish the receiver class
                            // before the helper has installed a compiled
                            // target, and a withdrawal zeroes the word.
                            // Never CALL a zero target.
                            // TEST R11, R11 (3 bytes)
                            self.buf.emit(&[0x4D, 0x85, 0xDB]);
                            // JZ rel32 → .miss (MEGA19-2: EAX is still the
                            // class id, so the stub's gate entry).
                            self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                            if miss_to_gate_entry {
                                mega_gate_patch.push(self.buf.pos() - 4);
                            } else {
                                mic_miss_patches32.push(self.buf.pos() - 4);
                            }

                            // BTR R11, 0 — CF := needs-context, R11 := entry.
                            // 5 bytes: REX.WB + 0F BA /6 + ModRM(11,110,R11) + imm8
                            self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]);

                            // JNC rel32 → .noctx. Context-free compiled
                            // methods use Java arg0 in ARG_REGS[0], while
                            // context-using methods reserve that register
                            // for vm_ptr. The cache publishes this ABI bit;
                            // honoring both shapes is essential because
                            // small interface implementations almost
                            // always compile context-free.
                            self.buf.emit(&[0x0F, 0x83, 0x00, 0x00, 0x00, 0x00]);
                            let noctx_patch = self.buf.pos() - 4;

                            // ---- Set up callee ABI: (vm_ptr, arg_slots[0..n]) ----
                            // Stack-argument sites (`mic_stack_block`) reserve the
                            // outgoing block on each ABI path, sized for the
                            // context ABI -- the wider of the two -- so the ONE
                            // `ADD RSP` after the shared CALL balances both.
                            if let Some((total_sub, _)) = mic_stack_block {
                                self.emit_sub_rsp_imm(total_sub);
                            }
                            // vm_ptr → ARG_REGS[0], arg_slots[i] → ARG_REGS[i + 1]
                            // (the overflow → [RSP + stack_base + 8k]).
                            self.emit_ic_hit_arg_marshal(
                                n,
                                args_base_offset,
                                true,
                                mic_stack_block.map(|(_, base)| base),
                            );

                            // JMP rel32 → .call, skipping the no-context
                            // marshalling block.
                            self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                            let ctx_call_patch = self.buf.pos() - 4;

                            // .noctx: Java args begin at ARG_REGS[0].
                            let noctx_off = self.buf.pos();
                            let noctx_rel = (noctx_off as i64) - (noctx_patch as i64 + 4);
                            debug_assert!((i32::MIN as i64..=i32::MAX as i64).contains(&noctx_rel));
                            self.buf.try_patch_i32(noctx_patch, noctx_rel as i32).ok();
                            if let Some((total_sub, _)) = mic_stack_block {
                                // A label: the ctx block's loads say nothing here.
                                self.slot_mirror = None;
                                self.emit_sub_rsp_imm(total_sub);
                            }
                            self.emit_ic_hit_arg_marshal(
                                n,
                                args_base_offset,
                                false,
                                mic_stack_block.map(|(_, base)| base),
                            );

                            // .call
                            let call_off = self.buf.pos();
                            let call_rel = (call_off as i64) - (ctx_call_patch as i64 + 4);
                            debug_assert!((i32::MIN as i64..=i32::MAX as i64).contains(&call_rel));
                            self.buf.try_patch_i32(ctx_call_patch, call_rel as i32).ok();

                            // SECURITY FIX (V1): same hardening as the
                            // PIC arm — the target was captured into R11
                            // (caller-saved scratch, not in ARG_REGS /
                            // SCRATCH_REGS / LOCAL_REGS) before the ABI
                            // marshalling, which loads into ARG_REGS only,
                            // and R10 is not read again. INVARIANT: nothing
                            // that writes R11 may be emitted between the
                            // entry-word capture and this `CALL R11`.
                            //
                            // CALL R11  (3 bytes: REX.B + FF /2 + ModRM(11,/2,R11))
                            self.buf.emit(&[0x41, 0xFF, 0xD3]);
                            // gcd d9/d row 2: the hit's buffer is its service copy.
                            let ret = self.buf.pos();
                            self.note_service_arg_copies(
                                ret,
                                args_base_offset,
                                &ic_hit_window_oops,
                            );
                            // Release the outgoing block before anything else
                            // runs: RAX (the result) is untouched, and every
                            // later sequence expects the frame's own RSP.
                            if let Some((total_sub, _)) = mic_stack_block {
                                self.emit_add_rsp_imm(total_sub);
                            }
                            // MEGA619-1 (opt-in): not at a return-tail site.
                            if !lazy_ic_hit_republish {
                                self.emit_post_call_rbp_republish();
                            }
                            // This call's map and shadow reload run at the
                            // shared `.done` below: the site is still open.
                            self.emit_inline_callee_deopt_check_site_open(
                                info,
                                n,
                                args_base_offset,
                            );

                            // JMP rel32 → .done. The root-frame
                            // republish above makes the distance exceed
                            // the old rel8 budget.
                            self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                            done_patches32.push(self.buf.pos() - 4);

                            // .miss: patch both rel32 sites here.
                            let miss_off = self.buf.pos();
                            for patch in &mic_miss_patches32 {
                                // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                let rel = (miss_off as i64) - (*patch as i64 + 4);
                                debug_assert!(
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                    "inline MIC miss branch overflowed rel32 ({} bytes)",
                                    rel
                                );
                                self.buf.try_patch_i32(*patch, rel as i32).ok();
                                // on Err try_patch_byte set buf.overflowed; compile bails
                            }
                        }

                        // ---- Shared compact hashed/vtable stub ----
                        // Both the baseline and optimizing tiers lower
                        // megamorphic misses through this exact library.
                        // It reloads arg0, performs two lock-free probes,
                        // and falls through here only on a real miss.
                        if let Some(pic) = pic_ptr
                            .filter(|_| inline_virtual_ic_allowed && sp_inline_mega_enabled())
                        {
                            let arg_offsets: Vec<i32> = (0..n)
                                .map(|i| args_base_offset + ((n - 1 - i) as i32) * 8)
                                .collect();
                            // The stub's full entry is this position (`.miss`).
                            let stub_start = self.buf.pos();
                            // JIT round 19 (lane mega19, MEGA5-18-1,
                            // `CRATONVM_JIT_SP_MEGA_LEAN_STUB_HIT`): a stub hit
                            // republishes this compile's own identity and goes
                            // straight to `.done`, skipping the identity
                            // restore below; only on the inline-TLS shape,
                            // where that restore is the one store the lean
                            // republish already made.
                            let lean_identity =
                                (crate::runtime_lowering::sp_mega_lean_stub_hit_enabled()
                                    && self.precise_maps
                                    && self.helpers.frame_record != 0
                                    && self.inline_rbp_tls_disp != 0
                                    && self.inline_cm_tls_disp != 0)
                                    .then(|| self.compile_id.id());
                            // JIT round 21 wave 13 (lane trace,
                            // `CRATONVM_JIT_MEGA_STUB_SERVICE_IDENTITY`,
                            // default ON): without the lean protocol, the
                            // stub's callee-deopt service still runs under
                            // this compile's identity, not the hit's `0`.
                            let service_identity =
                                (crate::runtime_lowering::mega_stub_service_identity_enabled()
                                    && self.precise_maps
                                    && self.helpers.frame_record != 0
                                    && self.inline_rbp_tls_disp != 0
                                    && self.inline_cm_tls_disp != 0)
                                    .then(|| self.compile_id.id());
                            let (stub_hits, stub_pic_imm, stub_gate_entry, stub_lean_exits) =
                                crate::runtime_lowering::emit_hashed_vtable_stub_located_lean(
                                    &mut self.buf,
                                    pic as usize,
                                    self.heap_local_offset,
                                    &arg_offsets,
                                    if self.precise_maps {
                                        self.helpers.frame_record
                                    } else {
                                        0
                                    },
                                    self.helpers.service_callee_deopt,
                                    info as usize,
                                    // This backend stages its outgoing
                                    // arguments into one descending block,
                                    // so element 0 already names it.
                                    arg_offsets[0],
                                    !mega_gate_patch.is_empty(),
                                    lean_identity,
                                    service_identity,
                                );
                            // A lean exit has published this frame's RBP and
                            // id and found no sentinel: `.done` directly.
                            done_patches32.extend(stub_lean_exits);
                            // W7-1: the gate goes to the stub's gate entry,
                            // or to its full entry should none have been
                            // emitted (both are correct from the gate).
                            for gate in std::mem::take(&mut mega_gate_patch) {
                                let target = stub_gate_entry.unwrap_or(stub_start);
                                // Widening: usize offsets -> i64 (rel math).
                                let rel = (target as i64) - (gate as i64 + 4);
                                // Cast: rel32 displacement within one body;
                                // on Err the buffer is marked overflowed and
                                // the compile bails.
                                self.buf.try_patch_i32(gate, rel as i32).ok();
                            }
                            // Round 12 wave 3 (lane mega2): the stub's baked
                            // slot joins the cascade's and the helper
                            // arguments' in `ic_patches`, so a loop copy's
                            // stub probes the copy's own slot (its hashed
                            // ways and its shared-table selector) instead of
                            // the original's. `CRATONVM_JIT_HASHED_STUB_CLONE_PIC=0`
                            // leaves it on the original slot, as before.
                            if let Some(imm_at) = stub_pic_imm.filter(|_| {
                                true
                            }) {
                                self.ic_patches.push((imm_at, 1, pic as usize));
                            }
                            // The stub's hit exits republish RBP with a `0`
                            // identity (it is shared and cannot know this
                            // frame's). Route them through one restore of
                            // this compile's id on their way to `.done`; the
                            // miss fall-through jumps over it. Without an
                            // identity to restore, they go straight to `.done`.
                            if self.helpers.frame_record != 0
                                && self.inline_cm_tls_disp != 0
                                && !stub_hits.is_empty()
                            {
                                let over_restore = self.emit_jmp_rel32_patch();
                                for hit in stub_hits {
                                    self.patch_rel32_to_here(hit);
                                }
                                self.emit_identity_after_shared_stub();
                                self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]); // JMP rel32 → .done
                                done_patches32.push(self.buf.pos() - 4);
                                self.patch_rel32_to_here(over_restore);
                            } else {
                                done_patches32.extend(stub_hits);
                            }
                        }
                        // A gate left unpatched because the stub block above
                        // was not entered (not expected: the gate's admission
                        // includes the stub's) goes to the resolving helper
                        // below, where `.miss` leads when no stub is emitted.
                        // MEGA5-18-3's probe misses land here too: the
                        // helper's setup, past the stub.
                        for gate in std::mem::take(&mut mega_gate_patch)
                            .into_iter()
                            .chain(std::mem::take(&mut probe_helper_patches))
                        {
                            // Widening: usize offsets -> i64 (rel math).
                            let rel = (self.buf.pos() as i64) - (gate as i64 + 4);
                            // Cast: rel32 displacement (see above).
                            self.buf.try_patch_i32(gate, rel as i32).ok();
                        }

                        // ---- Slow path: helper ABI setup + call ----
                        // The IC stack guard's low-stack edge lands here,
                        // through its cold side (round 13 wave 2, lane mega):
                        //
                        //   jmp .helper                 ; the miss fall-through
                        // .low:
                        //   <OSR placeholder refresh>   ; leaf floor fetch once
                        //   cmp rsp,[rbp-floor] ; ja ic_cascade_start
                        // .helper:
                        //
                        // Every register the cascade reads it loads from the
                        // frame (the spill above dominates the guard), so
                        // re-entering it after the leaf call is the path an
                        // OSR frame's first call took before this change. A
                        // real low stack falls into the helper, whose
                        // `enter_jit_dispatch` raises `StackOverflowError`.
                        if let Some(p) = ic_stack_low_patch.take() {
                            let over_cold = self.emit_jmp_rel32_patch();
                            self.patch_rel32_to_here(p);
                            if let Some(no_thread) = ic_stack_no_thread_patch.take() {
                                self.patch_rel32_to_here(no_thread);
                            }
                            if ic_floor_from_thread {
                                // Round 13 wave 4 (lane mega3): the exact floor
                                // from the leaf (it never GCs), `CMP RSP, RAX`
                                // (REX.W 39 /r, ModRM 11 RAX RSP).
                                cratonvm_jit_api::assert_helper_call_shape!(
                                    "native_stack_floor_fn",
                                    int_args = 0,
                                    returns_value = true
                                );
                                self.emit_call_absolute(self.helpers.native_stack_floor_fn);
                                self.buf.emit(&[0x48, 0x39, 0xC4]);
                            } else {
                                self.emit_osr_placeholder_floor_refresh();
                                self.emit_cmp_r64_rbp_local(RSP, self.stack_floor_slot_off);
                            }
                            let back = self.emit_jcc_rel32_patch(0x87); // JA → cascade
                                                                        // Widening: usize offsets -> i64 (rel math).
                            let rel = (ic_cascade_start as i64) - (back as i64 + 4);
                            // Cast: rel32 displacement within one body; on Err
                            // the buffer is marked overflowed and the compile
                            // bails.
                            self.buf.try_patch_i32(back, rel as i32).ok();
                            self.patch_rel32_to_here(over_cold);
                        }
                        self.emit_load_vm_context(ARG_REGS[0]);
                        self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                        if n > 0 {
                            let buf_start = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                            self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                        } else {
                            self.emit_xor_reg_self(ARG_REGS[2]);
                        }
                        self.emit_mov_imm32_sx(ARG_REGS[3], n as i32); // Cast: x86-64 immediate encoding

                        // Round-8 wave-3: defensive callee-saved spill
                        // before any GC-triggering dispatch CALL. Under
                        // `precise_maps` (or SB-CRASH-04 `safepoint_reg_spill`)
                        // the spill was already emitted before the inline
                        // cascade (so it dominates the inline-hit path too — see
                        // the PRECISE-MAPS FIX comment above); emitting it again
                        // here would be a redundant double-spill (and, with the
                        // shadow stack, an unbalanced double push). Gate-OFF:
                        // unchanged — this is the single conservative spill on
                        // the slow path.
                        if !self.precise_maps && !self.safepoint_reg_spill && !self.shadow_enabled {
                            self.emit_pre_safepoint_spill();
                        }

                        if let Some(mic) = mic_ptr {
                            // MIC-optimized dispatch: pass MIC slot as 5th
                            // arg and (CRIT-1) PIC slot as 6th arg so the
                            // helper can populate the inline 4-way cascade
                            // via `JitPICSlot::install` on every successful
                            // resolution. A `pic_ptr == 0` tells the helper
                            // no PIC is installed for this site.
                            //
                            // On Windows x64, args 5 and 6 go on the stack
                            // at [RSP+32] and [RSP+40] (shadow + spill).
                            // RAX is caller-saved so we stage each
                            // pointer through it before storing — safe
                            // regardless of which call encoding
                            // `emit_call_absolute` picks (rel32 leaves
                            // RAX alone; the imm64-via-RAX fallback
                            // would overwrite RAX anyway, but that
                            // happens after we've already stored).
                            let pic_arg: i64 = pic_ptr.map(|p| p as *const _ as i64).unwrap_or(0); // Cast: function pointer for JIT call target
                            #[cfg(target_os = "windows")]
                            {
                                // 5th arg at [RSP + 32]
                                let mic_arg_imm64_off = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(RAX, mic as *const _ as i64);
                                self.ic_patches.push((
                                    mic_arg_imm64_off,
                                    0,
                                    mic as *const _ as usize,
                                ));
                                // MOV [RSP + 32], RAX
                                self.rex_w();
                                self.buf.emit(&[0x89, 0x44, 0x24, 0x20]);
                                // 6th arg at [RSP + 40]
                                let pic_arg_imm64_off = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(RAX, pic_arg);
                                if let Some(pic) = pic_ptr {
                                    self.ic_patches.push((
                                        pic_arg_imm64_off,
                                        1,
                                        pic as *const _ as usize,
                                    ));
                                }
                                // MOV [RSP + 40], RAX
                                self.rex_w();
                                self.buf.emit(&[0x89, 0x44, 0x24, 0x28]);
                            }
                            #[cfg(not(target_os = "windows"))]
                            {
                                // SysV: 5th arg in R8, 6th in R9.
                                let mic_arg_imm64_off = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(R8, mic as *const _ as i64);
                                self.ic_patches.push((
                                    mic_arg_imm64_off,
                                    0,
                                    mic as *const _ as usize,
                                ));
                                let pic_arg_imm64_off = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(R9, pic_arg);
                                if let Some(pic) = pic_ptr {
                                    self.ic_patches.push((
                                        pic_arg_imm64_off,
                                        1,
                                        pic as *const _ as usize,
                                    ));
                                }
                            }
                            // ABI-3 — the ONE hand-written helper call site that must not use
                            // `assert_helper_call_shape!`. `invoke_virtual_mic` takes six integer
                            // words; the macro asserts `win64_stack_args() == 0` because every
                            // other guarded site marshals into the register file alone, and this
                            // sequence writes arguments five and six itself — `[RSP+32]`/`[RSP+40]`
                            // on Win64, R8/R9 on SysV. The arity is instead pinned in the shape
                            // this site can honestly make, which is that it overflows by exactly
                            // the two words it hand-writes.
                            const _: () = {
                                let sig = cratonvm_jit_api::helpers_abi::helper_sig_of(
                                    "invoke_virtual_mic",
                                );
                                assert!(
                                    sig.arity == 6 && sig.float_args == 0 && sig.returns_value,
                                    "`invoke_virtual_mic` no longer takes six integer words; \
                                     the two-word hand-written overflow above is written \
                                     against that arity and nothing else would notice",
                                );
                                assert!(
                                    sig.win64_stack_args() == 2,
                                    "`invoke_virtual_mic` no longer overflows Win64's four \
                                     argument registers by exactly two words; this site writes \
                                     `[RSP+32]` and `[RSP+40]` by hand and would now be \
                                     writing the wrong slots",
                                );
                            };
                            self.emit_call_absolute(self.helpers.invoke_virtual_mic);
                        } else {
                            // Plain dispatch without MIC
                            // (vm_ptr, info_ptr, args_ptr, num_args) — the no-MIC arm, which
                            // DOES fit the register file and so can use the macro.
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "invoke_dispatch",
                                int_args = 4,
                                returns_value = true
                            );
                            self.emit_call_absolute(self.helpers.invoke_dispatch);
                        }

                        // .done: patch the fast-path forward JMP(s).
                        //   - `done_patch`     (rel8, MIC inline)
                        //   - `done_patches32` (rel32, PIC inline — one
                        //                       per cache slot)
                        let done_off = self.buf.pos();
                        if let Some(patch) = done_patch {
                            // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                            let rel = (done_off as i64) - (patch as i64 + 1);
                            debug_assert!(
                                (-128..=127).contains(&rel),
                                "inline MIC done jump overflowed rel8 ({} bytes)",
                                rel
                            );
                            Self::patch_rel8_or_bail(&mut self.buf, patch, rel);
                        }
                        for patch in &done_patches32 {
                            // `patch` points at the start of the rel32
                            // immediate (4 bytes); the JMP opcode (E9)
                            // precedes it by 1 byte. The displacement
                            // is computed from the byte AFTER the
                            // immediate (patch + 4) to the target.
                            // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                            let rel = (done_off as i64) - (*patch as i64 + 4);
                            debug_assert!(
                                // Widening: i32 bound -> i64 (range comparison)
                                (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                "inline PIC done jump overflowed rel32 ({} bytes)",
                                rel
                            );
                            self.buf
                                .try_patch_i32(*patch, rel as i32) // Cast: rel32 displacement
                                .ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                        }
                        // T1.1.2 — every virtual/interface dispatch is
                        // a full safepoint: the callee may allocate,
                        // throw, or block. Record the oop map before
                        // the return value is pushed so the GC root
                        // walker has precise coverage at the return PC.
                        self.emit_oop_map_for_safepoint();

                        // After the dispatch returns, check whether the
                        // callee threw a Java exception. The dispatch
                        // helpers return `i64::MIN` (and stash the
                        // exception object in `JIT_PENDING_EXCEPTION`)
                        // when the callee throws; without this guard the
                        // JIT would push the bogus return value and keep
                        // running, masking the real exception with a
                        // downstream secondary failure. The guard deopts
                        // out so the interpreter routes the exception
                        // through this method's exception table.
                        self.emit_post_invoke_exception_check(info_ref.return_type);

                        // ARRAYLIST_ACCESS hit edge: the inline answer is in
                        // RAX, exactly where the dispatch leaves its result, so
                        // the push below serves both paths. The hit path made
                        // no call, so it owes neither the safepoint map / reload
                        // nor the exception check above, and skips them — the
                        // shape of the guarded-virtual inline's hit edge.
                        if let Some(hit) = arraylist_hit_patch {
                            self.patch_rel32_to_here(hit);
                        }
                        // BOX_HASH_EQUALS hit edges: same contract, one per
                        // answered shape, each with the answer in RAX.
                        for hit in box_hash_equals_hits {
                            self.patch_rel32_to_here(hit);
                        }
                        // The `Unsafe` dispatch prefix's hit edge: same
                        // contract (a get's value in RAX; a put's RAX is dead,
                        // its return type `V` pushes nothing).
                        if let Some(hit) = unsafe_dispatch_hit {
                            self.patch_rel32_to_here(hit);
                        }
                        // The identity-hash prefix's hit edge (nio21 w1 L5):
                        // same contract, the int hash sign-extended in RAX.
                        if let Some(hit) = identity_hash_hit {
                            self.patch_rel32_to_here(hit);
                        }
                        // The constant-`VarHandle` prefix's hit edge (nio21 w11
                        // L5): same contract, the site's result in RAX (dead
                        // for a `V` set).
                        if let Some(hit) = vh_const_hit {
                            self.patch_rel32_to_here(hit);
                        }
                        if std::env::var_os("DBG_MI").is_some()
                            && (self.method_label.contains("TomcatHack")
                                || (info_ref.class_name == "java/lang/reflect/Method"
                                    && info_ref.method_name == "invoke"))
                        {
                            eprintln!(
                                "[DBG_MI_T1] method={} op=0x{:x} {}.{} n={} getclass_hit={}",
                                self.method_label,
                                op,
                                info_ref.class_name,
                                info_ref.method_name,
                                n,
                                getclass_hit.is_some(),
                            );
                        }
                        if let Some(hit) = getclass_hit {
                            self.patch_rel32_to_here(hit);
                        }

                        // Reclaim spill slots used for invoke args AND the
                        // popped arg values — restoring to `pre_pop_spill`
                        // here was the Bug-4 cursor creep (n slots per
                        // non-void dispatch).
                        self.next_spill_offset = post_pop_spill;

                        if info_ref.return_type != b'V' {
                            if matches!(info_ref.return_type, b'D' | b'F') {
                                self.push_from_rax_as_xmm0();
                            } else {
                                self.push_from_rax();
                            }
                            // Tag the return as an oop when the
                            // descriptor is L... or [....
                            if matches!(info_ref.return_type, b'L' | b'[') {
                                self.mark_top_as_oop();
                            }
                        }
                    } else {
                        // Neither a direct target nor dispatch metadata. This
                        // arm used to fall out here having emitted NOTHING —
                        // no call, and no pop of the receiver and arguments —
                        // so the simulated operand stack kept them and every
                        // later bytecode in the method was compiled against
                        // the wrong stack. The invokestatic arm's matching
                        // case is a self-recursive call; an instance invoke
                        // has no such meaning, so refuse the method rather
                        // than emit it mis-modelled.
                        self.fail("singlepass-codegen/instance-invoke-without-target");
                        return WalkStep::Return(false);
                    }
                }
                if !guarded_virtual_done_patches.is_empty() {
                    self.reconcile_guarded_inline_join(guarded_virtual_done_patches);
                }
                // The twin's hit edge arrives from its release sequence with RAX
                // clobbered, while the dispatch edge's last instruction stored
                // RAX to the result's word: the adjacent-reload elision must not
                // read "RAX holds that word" across this join (a splice's hit
                // stores its result from RAX last, so it never needed this).
                if sync_instance_joins {
                    self.slot_mirror = None;
                }
                if op == 0xb9 {
                    pc += 5;
                } else {
                    pc += 3;
                }
            }
            _ => {
                self.fail("singlepass-codegen/walk-family-misdispatch");
                return WalkStep::Return(false);
            }
        }
        WalkStep::Next(pc)
    }

}
