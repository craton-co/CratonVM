#[allow(unused_imports)]
use super::*;

#[path = "collection_intrinsics.rs"]
mod collection_intrinsics;

impl IrBuilder {
    /// Create a new builder for a method with `num_params` parameter slots
    /// and `num_locals` total local variable slots.
    pub fn new(num_params: usize, num_locals: usize) -> Self {
        let mut graph = Graph::empty(256);
        // Round nio20 wave 4 (lane L1, W3L4-1): the scoped fold's raw class.
        graph.scoped_raw_alias = ir_scoped_raw_alias_enabled();
        // The builder appends: every edge it writes goes through the tracked
        // mutators, so the def-use edges are maintained from an empty graph
        // rather than derived by a scan once optimization starts.
        graph.rebuild_use_lists();

        // Node 0: Start
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        // Node 1: Proj(0) = initial control
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        // Node 2: Proj(1) = initial memory
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);

        // Create Param nodes for each parameter
        //
        // `num_locals` is the class file's declared `max_locals`, which "some
        // classfiles in the wild" get wrong (`vm/src/runtime/frame.rs`'
        // `effective_max_locals` widens it for the interpreter). Indexing
        // `locals[i]` then panicked on the compile thread; a short frame is now
        // a build refusal instead (round 11).
        let mut locals = vec![NO_NODE; num_locals];
        let mut short_locals = false;
        for i in 0..num_params {
            let param = graph.add(Op::Param(i as u16), IrType::Int, vec![start], None);
            match locals.get_mut(i) {
                Some(slot) => *slot = param,
                None => short_locals = true,
            }
        }

        IrBuilder {
            graph,
            stack: Vec::with_capacity(16),
            locals,
            ctrl,
            mem,
            monitors: Vec::new(),
            unstructured_monitors: false,
            build_refused: short_locals.then_some((line!(), 0)),
            _num_locals: num_locals,
            merges: HashMap::new(),
            loop_headers: HashSet::new(),
            loop_phis: HashMap::new(),
            field_info: HashMap::new(),
            volatile_field_pcs: HashSet::new(),
            final_field_pcs: HashSet::new(),
            byte_array_field_pcs: HashSet::new(),
            field_final_forward_on: ir_field_final_forward_enabled(),
            volatile_loads_admitted: false,
            volatile_stores_admitted: false,
            new_info: HashMap::new(),
            anewarray_info: HashMap::new(),
            multianewarray_sites: HashMap::new(),
            new_deferred_sites: HashMap::new(),
            trivial_init_pcs: HashSet::new(),
            uninit_new_trap_pcs: HashSet::new(),
            field_trap_sites: HashMap::new(),
            uninit_new_guard_sites: HashMap::new(),
            object_init_pcs: HashSet::new(),
            first_call_sites: HashMap::new(),
            invoke_info: HashMap::new(),
            unloaded_static_call_pcs: HashSet::new(),
            sync_direct_calls: HashMap::new(),
            sync_splice_sites: HashMap::new(),
            sync_splice_mirrors: HashMap::new(),
            sync_splice_held_sites: HashSet::new(),
            ldc_self_class_holds_lock: HashMap::new(),
            getclass_exact_compares: HashMap::new(),
            splice_getclass_compares: HashMap::new(),
            method_monitor_class_cps: HashSet::new(),
            method_monitor_slot0_loops_below: None,
            method_self_lock_requested: false,
            method_self_lock_mirror: None,
            root_caller_mirror_slot: 0,
            root_security_walk_ignored: false,
            splice_caller_frames: HashMap::new(),
            scalar_intrinsics: HashMap::new(),
            pure_expansions: HashMap::new(),
            unbox_intrinsics: HashMap::new(),
            scoped_offheap_fold: false,
            vh_field_plans: HashMap::new(),
            ffm_inline_sites: HashMap::new(),
            ffm_bimorphic_seconds: HashMap::new(),
            ffm_unreached_sites: HashMap::new(),
            unreached_call_sites: HashMap::new(),
            ffm_layout_facts: HashMap::new(),
            indy_trap_sites: HashMap::new(),
            indy_bridge_sites: HashMap::new(),
            indy_bridge_refusal_only: false,
            indy_default_trap_shapes: false,
            indy_published_word_pcs: HashSet::new(),
            spliced_indy_wired: false,
            indy_direct_sites: HashMap::new(),
            indy_mutable_guards: HashMap::new(),
            indy_lambda_words: HashMap::new(),
            indy_capture_news: HashMap::new(),
            indy_record_folds: HashMap::new(),
            sb_chain_fusions: HashMap::new(),
            sb_chain_active: None,
            lambda_impl_sites: HashMap::new(),
            indy_capture_new_values: HashMap::new(),
            lambda_impl_phi_checks: Vec::new(),
            lambda_impl_captures_need_splice: false,
            mh_constant_sites: HashMap::new(),
            mh_constant_interface_sites: HashSet::new(),
            mh_constant_interface_seen_cells: HashMap::new(),
            mh_constant_bound_sites: HashMap::new(),
            mh_constant_bound_static_sites: HashSet::new(),
            mh_constant_filter_sites: HashMap::new(),
            mh_constant_convert_sites: HashMap::new(),
            inline_sites: HashMap::new(),
            exact_receiver_sites: HashMap::new(),
            exact_receiver_single_store_slots: HashSet::new(),
            exact_receiver_flow: HashMap::new(),
            loop_written_slots: HashMap::new(),
            exact_receiver_locals: HashMap::new(),
            exact_receiver_relied: HashSet::new(),
            exact_receiver_nodes: HashMap::new(),
            splice_bases_keeping_this: HashSet::new(),
            splice_bases_written_locals: HashMap::new(),
            guarded_receiver_sites: HashSet::new(),
            cha_watermark_sites: HashMap::new(),
            cha_class_set_sites: HashMap::new(),
            guarded_hot_cap_sites: HashSet::new(),
            nested_guarded_sites: HashSet::new(),
            guarded_splice_pending: None,
            bimorphic_sites: HashMap::new(),
            polymorphic_sites: HashMap::new(),
            max_splice_depth: ir_max_splice_depth(),
            guarded_trap_sites: HashSet::new(),
            trap_exact_facts: HashMap::new(),
            field_load_memo: HashMap::new(),
            field_memo_in_splices: true,
            field_load_memo_scan_from: 0,
            field_memo_deferred_calls: Vec::new(),
            field_memo_join_snapshots: HashMap::new(),
            field_memo_defer_miss_call: true,
            field_memo_precise_joins: ir_field_memo_precise_joins_enabled(),
            field_load_memo_mode: FieldLoadMemoMode::from_flags(),
            field_store_forward_on: ir_field_store_forward_enabled(),
            field_load_memo_census: FieldLoadMemoCensus::default(),
            static_load_memo: HashMap::new(),
            static_load_memo_scan_from: 0,
            static_load_memo_mode: StaticLoadMemoMode::from_flags(),
            static_initialized_classes: HashSet::new(),
            static_load_memo_census: StaticLoadMemoCensus::default(),
            field_decl_class_by_pc: HashMap::new(),
            param_slot_count: num_params,
            splice: Vec::new(),
            splice_multi_return: HashSet::new(),
            splice_block_walks: HashMap::new(),
            splices_done: 0,
            splice_local: HashSet::new(),
            splice_tainted: HashSet::new(),
            splice_committed_bases: HashSet::new(),
            splice_ctor_receivers: HashSet::new(),
            splice_ctor_receiver_read: HashSet::new(),
            splice_guard_seen: false,
            splice_guard_pc: None,
            splice_refusal_sink: None,
            splice_call_committed: false,
            splice_fence_relax: SpliceFenceRelax::NONE,
            splice_store_fence_forward_branch: false,
            splice_frame_states: false,
            chain_snapshots_lazy: false,
            splice_call_states: false,
            pending_post_call: None,
            post_call_join_value: None,
            splice_state_words: 0,
            splice_chain_fences: false,
            splice_null_facts: HashSet::new(),
            pure_splice_sites: HashSet::new(),
            convert_member_pure: HashSet::new(),
            convert_box_after: HashMap::new(),
            pending_splice_jump: None,
            root_code_len: 0,
            splice_edge_null_facts: HashMap::new(),
            pruned_always_taken: std::collections::HashSet::new(),
            branches_pruned: 0,
            pruned_dead_until: 0,
            pruned_never_taken: std::collections::HashSet::new(),
            splice_never_taken: std::collections::HashSet::new(),
            splice_never_taken_dead: std::collections::HashSet::new(),
            splice_always_taken: std::collections::HashSet::new(),
            splice_always_taken_dead: std::collections::HashSet::new(),
            splice_pruned_switch_edges: HashMap::new(),
            never_taken_dead_from: usize::MAX,
            never_taken_pruned: 0,
            never_taken_edges: Vec::new(),
            never_taken_live: None,
            inline_cold_athrow: false,
            cold_throw_arms_by_base: HashMap::new(),
            cold_throw_dead: (0, 0),
            local_throw_rows: Vec::new(),
            local_throw_subclass: HashMap::new(),
            local_throw_handlers: HashSet::new(),
            local_throw_optional: HashSet::new(),
            local_throw_edges: 0,
            local_throw_catch_targets: Vec::new(),
            spliced_throw_edges: 0,
            exception_event_guard: None,
            call_throw_names: Vec::new(),
            call_throw_declaring_class: 0,
            call_throw_planned: HashMap::new(),
            call_throw_edges: 0,
            implicit_throw_planned: HashMap::new(),
            implicit_throw_edges: 0,
            implicit_throw_edges_withheld: false,
            splice_handler_rows: HashMap::new(),
            splice_handler_edges: false,
            splice_handler_entries: HashSet::new(),
            splice_handler_dead: HashSet::new(),
            splice_handler_planted_calls: HashSet::new(),
            splice_handler_mark: None,
            invoke_labels: HashMap::new(),
            method_label: None,
            spliced_bodies_pure: true,
            osr_door_traps_in_place: false,
            typecheck_traps_resume_in_place: false,
            last_trap_in_place: false,
            in_place_trap_placeholders: HashSet::new(),
            uninitialized_news: HashSet::new(),
            return_narrow: None,
            tdigest_scalar_kernel: false,
            ldc2w_info: HashMap::new(),
            wide_field_long: false,
            wide_field_fp: false,
            ldc_info: HashMap::new(),
            ldc_string_info: HashMap::new(),
            ldc_class_info: HashMap::new(),
            ldc_slot_info: HashMap::new(),
            ldc_condy_slot_info: HashMap::new(),
            static_field_info: HashMap::new(),
            static_final_exact_classes: HashMap::new(),
            frozen_facts: None,
            frozen_nodes: HashMap::new(),
            frozen_expected: HashMap::new(),
            frozen_known: Vec::new(),
            frozen_keeps_this: HashSet::new(),
            frozen_despec: None,
            static_guess_despec: None,
            object_class_id: None,
            const_folds: ConstFoldTables::default(),
            const_ref_slots: HashMap::new(),
            const_ref_exact_classes: HashMap::new(),
            const_folded_dead_skip: false,
            const_folded_dead_targets: HashSet::new(),
            instanceof_info: HashMap::new(),
            checkcast_info: HashMap::new(),
            string_layout: None,
            string_access_withdrawn_pcs: HashSet::new(),
            thread_local_layout: None,
            nio_charset_layout: None,
            vthread_guard: None,
            refers_to_referent_index: None,
            refers_to_referent_row: 0,
            spliced_referent_row: 0,
            reference_get_layout: None,
            tl_site_plan: None,
            tl_license: NO_NODE,
            vh_fresh_helpers: None,
            const_intern: HashMap::new(),
            const_intern_upto: 0,
            block_walk: false,
            osr_opaque_liveness: None,
            osr_requested_header: None,
            osr_opaque_locals: HashMap::new(),
            catch_arm_rows: Vec::new(),
            catch_arm_class_id: 0,
            catch_arm_calls: HashMap::new(),
            catch_arm_handlers: HashMap::new(),
            catch_arm_walkable: HashSet::new(),
            osr_imported_exact: HashMap::new(),
            osr_profiled_slot_classes: HashMap::new(),
            osr_profiled_admitted: HashMap::new(),
            osr_profiled_exact_nodes: HashMap::new(),
            osr_arm_exact_checks: false,
            pending_osr_arm_checks: Vec::new(),
        }
    }

    /// Round 11 wave 14 (lane osrmerge): make the loop headers of this build
    /// OSR-open (`r11w11-irexc-osr-entry-as-a-real-predecessor`, `## Wave 13`).
    /// `rows`/`covered`/`words` are `regalloc::live_locals_per_pc_all` over the
    /// method's own code WITH its exception handlers, which is what decides
    /// which locals the OSR arm reads: a local live at the header gets an
    /// `Op::OsrLocal`, a dead one `NO_NODE` on the arm's edge. Called by
    /// `lib.rs` only under `CRATONVM_JIT_OSR_OPAQUE_ENTRY`; a builder that is
    /// never handed it builds exactly what it built before.
    /// al16 (2026-10-05): the header the optimizing OSR route was asked to
    /// enter (`None` for a method-entry compile). Every OTHER header declines
    /// its opaque arm when a live local's
    /// forward value there is an `Op::New` of this compile
    /// (`CRATONVM_JIT_IR_OSR_ARM_SPARES_FRESH_NEW`): the arm's `OsrLocal` in
    /// that local's phi is an unknown object, so escape analysis can no longer
    /// scalar-replace the allocation (the for-each iterator behind a fill
    /// loop). The header keeps no entry in this body; an OSR request there
    /// builds its own body, which keeps the arm.
    pub fn set_osr_requested_header(&mut self, header: Option<usize>) {
        self.osr_requested_header = header;
        self.graph.osr_compile = header.is_some();
    }

    /// OSR round (2026-10-06): let the requested header's arm test the
    /// imported locals the walk proves exact (`guard_osr_arm_exact_locals`).
    /// `lib.rs` passes `false` under `CRATONVM_JIT_OSR_ARM_EXACT_CHECK=0` and
    /// once the header's bci was de-specialised (a declined entry's charges),
    /// which leaves the slot unproven and its sites guarded by profile.
    pub fn set_osr_arm_exact_checks(&mut self, enabled: bool) {
        self.osr_arm_exact_checks = enabled;
    }

    /// `CRATONVM_JIT_IR_OSR_PROFILED_SLOT_CHECK` (**default ON**, read by the
    /// planner): caller local slots written once in the whole method whose
    /// value an in-loop call's receiver profile says is one class,
    /// `slot -> class`. At the requested OSR header both entries -- the arm
    /// and the walked prologue -- test that class once (a failure deopts to
    /// the interpreter at the header, which runs the loop as before), and
    /// the slot is then exactly that class inside the loop: its calls splice
    /// unguarded and `loop_field_forward` may read its fields once
    /// (`ByteBuffer buffer = ByteBuffer.wrap(array)` in
    /// `HttpHeaderValidationUtilTest`, a `HeapByteBuffer` behind a factory
    /// no proof sees through).
    pub fn set_osr_profiled_slot_classes(&mut self, classes: HashMap<usize, u32>) {
        self.osr_profiled_slot_classes = classes;
    }

    pub fn set_osr_opaque_liveness(&mut self, rows: Vec<u64>, covered: Vec<bool>, words: usize) {
        self.osr_opaque_liveness = Some((rows, covered, words));
    }

    pub fn has_unloaded_static_splice_calls(&self) -> bool {
        !self.unloaded_static_call_pcs.is_empty()
    }

    /// Is JVM local `i` live at bytecode `pc` by the liveness handed to
    /// [`Self::set_osr_opaque_liveness`]? `None` when there is none, or it
    /// does not cover `pc` (a spliced callee's pc, dead bytecode) — the caller
    /// then builds an ordinary header.
    pub(in crate::ir) fn osr_opaque_local_live(&self, pc: usize, i: usize) -> Option<bool> {
        let (rows, covered, words) = self.osr_opaque_liveness.as_ref()?;
        if !covered.get(pc).copied().unwrap_or(false) || i / 64 >= *words {
            return None;
        }
        let word = rows.get(pc.checked_mul(*words)?.checked_add(i / 64)?)?;
        Some(word & (1u64 << (i % 64)) != 0)
    }

    /// Build the opaque OSR entry merge in front of the loop header at
    /// `target_pc`, or answer `None` and build nothing (an ordinary header).
    ///
    /// Round 11 wave 14 (lane osrmerge), the `## Wave 13` shape of
    /// `r11w11-irexc-osr-entry-as-a-real-predecessor`:
    ///
    /// ```text
    /// If(ctrl_entry_0, OsrEntryFlag) ── Proj(1) ─┐
    ///                                └─ Proj(0) ─┤  (the OSR arm)
    ///             ctrl_entry_1.. ────────────────┤
    ///                                  pre = Merge(Proj(1), ctrl_entry_1.., Proj(0))
    /// ```
    ///
    /// with one φ per local `[pre, entry values.., OsrLocal(i) | NO_NODE]` and
    /// a memory φ `[pre, entry mems.., OsrMemory]`. The caller makes `pre` the
    /// header's single forward input. Returns `(pre, memory φ, locals)`.
    ///
    /// Declined (no node is added) when the header carries an operand stack
    /// or a held monitor on any entry (the stub seeds neither), inside a
    /// splice (a callee's header is not a pc the door enters), or when the
    /// liveness does not cover the header.
    pub(in crate::ir) fn build_opaque_osr_merge(
        &mut self,
        target_pc: usize,
        state: &MergeState,
    ) -> Option<(NodeId, NodeId, Vec<NodeId>)> {
        if self.osr_opaque_liveness.is_none() || !self.splice.is_empty() {
            return None;
        }
        // An uncovered header gets no arm even when it has no local to ask
        // about (the liveness loop below then never answers `None`). Round 13
        // wave 8 (lane irexc2): `lib.rs` uncovers every header no handler
        // reaches under `CRATONVM_JIT_OSR_OPAQUE_HANDLER_HEADERS`.
        let header_covered = self
            .osr_opaque_liveness
            .as_ref()
            .is_some_and(|(_, covered, _)| covered.get(target_pc).copied().unwrap_or(false));
        if !header_covered {
            return None;
        }
        if state.stack_snapshots.iter().any(|s| !s.is_empty())
            || state.monitor_snapshots.iter().any(|m| !m.is_empty())
        {
            return None;
        }
        let &entry0 = state.ctrl_inputs.first()?;
        let num_locals = state.local_snapshots.first().map_or(0, |s| s.len());
        if state.local_snapshots.len() != state.ctrl_inputs.len()
            || state.mem_inputs.len() != state.ctrl_inputs.len()
        {
            return None;
        }
        // Liveness first, so a header it does not cover builds nothing.
        let mut live = vec![false; num_locals];
        for (i, slot) in live.iter_mut().enumerate() {
            *slot = self.osr_opaque_local_live(target_pc, i)?;
        }
        // al16: spare a fresh allocation the arm would merge with an unknown
        // object, at a header the OSR route was not asked to enter (every
        // header of a method-entry compile).
        if self.osr_requested_header != Some(target_pc) && ir_osr_arm_spares_fresh_new_enabled() {
            let entry_locals = state.local_snapshots.first()?;
            let fresh = live.iter().enumerate().any(|(i, &is_live)| {
                is_live
                    && entry_locals.get(i).is_some_and(|&v| {
                        matches!(
                            self.graph.nodes.get(v as usize).map(|n| &n.op),
                            Some(Op::New { .. })
                        )
                    })
            });
            if fresh {
                return None;
            }
        }
        // No bytecode pc on the flag, the `If` or its projections: none of
        // them is a bytecode, and a pc on a data node would anchor the
        // header's bci at the arm (`Lowerer::note_bci_anchor`).
        let flag = self.graph.add(
            Op::OsrEntryFlag {
                header_bci: target_pc,
            },
            IrType::Int,
            vec![entry0],
            None,
        );
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![entry0, flag], None);
        // The entry-side projection first, so the arm is the later block in
        // creation order as well.
        let from_entry = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], None);
        let arm = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], None);
        let mut pre_ctrls = Vec::with_capacity(state.ctrl_inputs.len() + 1);
        pre_ctrls.push(from_entry);
        pre_ctrls.extend(state.ctrl_inputs.iter().skip(1).copied());
        pre_ctrls.push(arm);
        let pre = self
            .graph
            .add(Op::Merge, IrType::Control, pre_ctrls, Some(target_pc));

        let osr_mem = self
            .graph
            .add(Op::OsrMemory, IrType::Memory, vec![arm], None);
        let mut mem_inputs = Vec::with_capacity(state.mem_inputs.len() + 2);
        mem_inputs.push(pre);
        mem_inputs.extend_from_slice(&state.mem_inputs);
        mem_inputs.push(osr_mem);
        let pre_mem = self
            .graph
            .add(Op::Phi, IrType::Memory, mem_inputs, Some(target_pc));

        let mut pre_locals = vec![NO_NODE; num_locals];
        let mut osr_locals: Vec<(usize, NodeId)> = Vec::new();
        for (i, pre_local) in pre_locals.iter_mut().enumerate() {
            // Uninitialised on every entry edge: not a local of the loop (the
            // header activation clears it), and not one the arm can supply.
            if state
                .local_snapshots
                .iter()
                .all(|s| s.get(i).copied().and_then(node_id_opt).is_none())
            {
                continue;
            }
            let mut inputs = Vec::with_capacity(state.local_snapshots.len() + 2);
            inputs.push(pre);
            for snap in &state.local_snapshots {
                inputs.push(snap.get(i).copied().unwrap_or(NO_NODE));
            }
            // Typed from the entry values alone: for a LIVE local that is the
            // verifier's type at the header, which every incoming path —
            // the handler's included — agrees on.
            let ty = self.phi_data_type(&inputs);
            let is_live = live.get(i).copied().unwrap_or(true);
            if is_live {
                let Ok(idx) = u16::try_from(i) else {
                    self.refuse(line!(), target_pc);
                    return None;
                };
                let osr = self.graph.add(Op::OsrLocal(idx), ty, vec![arm], None);
                inputs.push(osr);
                osr_locals.push((i, osr));
            } else {
                // Dead at the header: no path from it reads the slot before
                // writing it, so the interpreter's word (which may hold a
                // different type there) is never read. `NO_NODE` is
                // "undefined on this edge"; the stub zeroes the φ's home when
                // it is a reference.
                inputs.push(NO_NODE);
            }
            *pre_local = self.graph.add(Op::Phi, ty, inputs, Some(target_pc));
        }
        self.guard_osr_arm_exact_locals(target_pc, arm, from_entry, state, &osr_locals, num_locals);
        self.osr_opaque_locals.insert(target_pc, osr_locals);
        Some((pre, pre_mem, pre_locals))
    }

    /// OSR round (2026-10-06): at the header the OSR route was asked to enter,
    /// test on the arm that each imported local the walk proves EXACT keeps
    /// that class, so the proof survives the arm
    /// ([`Self::activate_loop_header`] withdraws every other imported slot's
    /// fact).
    ///
    /// The population is the for-each iterator: a single-store slot
    /// ([`Self::exact_receiver_single_store_slots`]) whose one store wrote a
    /// spliced factory's `new C`, live at the header. The interpreter frame
    /// the arm reads ran that same store after its OWN call, which dispatched
    /// on whatever receiver it had (`FerHdrBugA`: an `ArrayList` subclass whose
    /// `iterator()` returns another class), so the slot is `C` only when the
    /// arm checks it. The test is `Guard(ExactClassIs C)` on the arm (null
    /// fails it too), stamped with a snapshot of the arm's OWN values at the
    /// header bci: a failure transfers the interpreter's frame back unchanged
    /// and resumes it at the header, which declines the entry. An OSR-door
    /// body's entries are all opaque, so its guard-exit fact asks no
    /// classification of this guard (`osr_exit::guard_exit_fact_opaque`).
    ///
    /// With the slot proven exact, `hasNext()`/`next()` splice as exact sites
    /// and escape analysis sees one class on every input of the iterator's φ.
    /// Off with `CRATONVM_JIT_OSR_ARM_EXACT_CHECK=0`, and per header once its
    /// bci was de-specialised (`IrBuilder::set_osr_arm_exact_checks`).
    pub(in crate::ir) fn guard_osr_arm_exact_locals(
        &mut self,
        target_pc: usize,
        arm: NodeId,
        from_entry: NodeId,
        state: &MergeState,
        osr_locals: &[(usize, NodeId)],
        num_locals: usize,
    ) {
        if self.osr_requested_header != Some(target_pc) || !self.osr_arm_exact_checks {
            return;
        }
        let Some(entry_locals) = state.local_snapshots.first() else {
            return;
        };
        // `CRATONVM_JIT_IR_OSR_PROFILED_SLOT_CHECK`: a profiled write-once
        // slot, tested on BOTH entries (`Self::set_osr_profiled_slot_classes`).
        // Only with a single walked entry edge, whose value is the slot's one
        // store's.
        let mut walked_checks: Vec<(NodeId, u32)> = Vec::new();
        if !self.osr_profiled_slot_classes.is_empty() && state.local_snapshots.len() == 1 {
            let profiled: Vec<(usize, u32)> = self
                .osr_profiled_slot_classes
                .iter()
                .map(|(&k, &c)| (k, c))
                .collect();
            for (k, class_id) in profiled {
                // A slot the bytecode fact already proves (its one store a
                // `new`) is handled below as before; another class's skips.
                if self.exact_receiver_locals.contains_key(&k) {
                    continue;
                }
                if !osr_locals.iter().any(|&(slot, _)| slot == k) {
                    continue;
                }
                let Some(walked) = entry_locals.get(k).copied().filter(|&v| v != NO_NODE) else {
                    continue;
                };
                if self.graph.nodes.get(walked as usize).map(|n| n.ty) != Some(IrType::Ref) {
                    continue;
                }
                // Wave 107 (lane L2): a node fact of the header's φ, made by
                // `activate_loop_header` -- never the slot-wide facts, which
                // would also answer for sites the checks do not dominate.
                self.osr_profiled_admitted.insert(k, class_id);
                walked_checks.push((walked, class_id));
            }
        }
        let mut checks: Vec<(usize, NodeId, u32)> = Vec::new();
        for &(k, osr) in osr_locals {
            if let Some(&class_id) = self.osr_profiled_admitted.get(&k) {
                // Its walked check is in `walked_checks` (same slot, same
                // class); the arm tests the imported value the same way.
                checks.push((k, osr, class_id));
                continue;
            }
            let Some(&class_id) = self.exact_receiver_locals.get(&k) else {
                continue;
            };
            if !self.exact_receiver_single_store_slots.contains(&k) {
                continue;
            }
            let fresh = entry_locals.get(k).is_some_and(|&v| {
                matches!(
                    self.graph.nodes.get(v as usize).map(|n| &n.op),
                    Some(&Op::New { class_id: c, .. }) if c == class_id
                )
            });
            if fresh
                || walked_checks
                    .iter()
                    .any(|&(w, c)| c == class_id && entry_locals.get(k) == Some(&w))
            {
                checks.push((k, osr, class_id));
            }
        }
        if !walked_checks.is_empty() {
            // The walked entry's own frame at the header: its locals, no
            // stack, no monitor (an entry holding either declines the merge).
            self.pending_osr_arm_checks.push((
                target_pc,
                from_entry,
                entry_locals.clone(),
                walked_checks,
            ));
        }
        if checks.is_empty() {
            return;
        }
        // The arm's own frame: every imported local, nothing on the stack, no
        // monitor (the arm is declined with either, `build_opaque_osr_merge`).
        let mut locals = vec![NO_NODE; num_locals];
        for &(k, osr) in osr_locals {
            if let Some(slot) = locals.get_mut(k) {
                *slot = osr;
            }
        }
        // The fact now, for the sites the walk splices next; the test after
        // the walk (`Self::finish_osr_arm_checks`), whose snapshot must follow
        // the header's own at this bci.
        for &(k, _, class_id) in &checks {
            // Wave 107 (lane L2): a profiled slot has no slot-wide fact to
            // keep; its header φ gets a node fact instead.
            if self.osr_profiled_admitted.contains_key(&k) {
                continue;
            }
            self.osr_imported_exact.insert(k, class_id);
        }
        let checks: Vec<(NodeId, u32)> = checks.into_iter().map(|(_, osr, c)| (osr, c)).collect();
        self.pending_osr_arm_checks
            .push((target_pc, arm, locals, checks));
    }

    /// Build the tests [`Self::guard_osr_arm_exact_locals`] promised, once the
    /// walk is done. Each arm gets one snapshot of its own values at the
    /// header bci, appended after the header's own snapshot there: the IR
    /// verifier's duplicate-bci rule lets the FIRST snapshot at a bci go
    /// unnamed (every by-bci consumer finds it), and this one is named by the
    /// guards that use it (`Node::frame_snapshot`). The `ExactClassIs` carries
    /// no pc, so no node of the arm anchors the header's bci there
    /// (`Lowerer::note_bci_anchor` skips a node naming its own snapshot).
    pub(in crate::ir) fn finish_osr_arm_checks(&mut self) {
        for (target_pc, arm, locals, checks) in std::mem::take(&mut self.pending_osr_arm_checks) {
            let snapshot = self.graph.safepoints.len() as u32;
            self.graph.push_safepoint(SafepointSnapshot::new(
                target_pc,
                locals,
                Vec::new(),
                Vec::new(),
            ));
            for (osr, class_id) in checks {
                let is =
                    self.graph
                        .add(Op::ExactClassIs { class_id }, IrType::Int, vec![osr], None);
                let guard = self.graph.add(
                    Op::Guard { bci: target_pc },
                    IrType::Void,
                    vec![arm, is],
                    Some(target_pc),
                );
                if !self.graph.set_node_frame_snapshot(guard, snapshot) {
                    // Unreachable: the guard's pc is the snapshot's bci.
                    // Without its frame it would deopt on the header's, whose
                    // φs the arm has not written: refuse the build.
                    self.refuse(line!(), target_pc);
                    return;
                }
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[ir] osr-arm-exact header={target_pc} class={class_id}: the arm tests the imported value"
                    );
                }
            }
        }
    }

    /// inc 26: supply resolved `ldc2_w` long-constant values (`pc → i64`). Must
    /// be called before [`Self::build`]; an `ldc2_w` pc not present bails.
    pub fn set_ldc2w_info(&mut self, info: HashMap<usize, (i64, bool)>) {
        self.ldc2w_info = info;
    }

    /// JVMS §6.5: narrow this method's `ireturn`s to its declared int-category
    /// return type (`Z` as if by `& 1`, `B`/`C`/`S` by truncation and
    /// extension). `descriptor` is the method's own descriptor; a method key
    /// works too (see [`crate::narrowed_int_return_tag`]). Must be called
    /// before [`Self::build`]. A builder that never calls it narrows nothing.
    pub fn set_return_descriptor(&mut self, descriptor: &str) {
        self.return_narrow = crate::narrowed_int_return_tag(descriptor);
    }

    /// COV-03: admit `J` / `F`+`D` instance-field accesses, from the same two
    /// flags the `lib.rs` admission chain evaluates (`ir_emit_long`,
    /// `ir_emit_fp`). Must be called before [`Self::build`]; both default off,
    /// which is the pre-lane behaviour (every wide field bails to single-pass).
    pub fn set_wide_field_gates(&mut self, long: bool, fp: bool) {
        self.wide_field_long = long;
        self.wide_field_fp = fp;
    }

    /// The `IrType` a field of descriptor tag `type_tag` loads/stores as, or
    /// `None` when this builder must refuse the access.
    ///
    /// One place decides it for both the `getfield` (0xb4) and `putfield`
    /// (0xb5) arms. COV-03 exists because those two arms disagreed: `getfield`
    /// learned about reference fields and `putfield`, twenty lines below it,
    /// did not — a fix applied to one of two sites. Sharing the classifier is
    /// what stops that recurring.
    pub(in crate::ir) fn field_access_type(&self, type_tag: u8) -> Option<IrType> {
        match type_tag {
            b'I' | b'Z' | b'B' | b'C' | b'S' => Some(IrType::Int),
            b'L' | b'[' | 0 => Some(IrType::Ref),
            b'J' if self.wide_field_long => Some(IrType::Long),
            b'F' if self.wide_field_fp => Some(IrType::Float),
            b'D' if self.wide_field_fp => Some(IrType::Double),
            _ => None,
        }
    }

    /// `value` narrowed the way `putfield` narrows it into a field of
    /// descriptor `type_tag`: `B` sign-extends the low 8 bits, `S` the low 16,
    /// `C` zero-extends the low 16, `Z` keeps bit 0 (JVMS §6.5; the
    /// interpreter's `narrow_int_to_field_type`). Every other tag, and a value
    /// that is not an `Int` node, is returned unchanged.
    ///
    /// Built in exactly the shapes the `i2b`/`i2s`/`i2c` arms build —
    /// `(x << 24) >> 24`, `(x << 16) >> 16`, `x & 0xFFFF` — plus `x & 1` for
    /// `Z`, because `Op::I2B`/`I2S`/`I2C` have no lowering (`ir_lower` lists
    /// them `Unlowerable`).
    ///
    /// No node is added when the value is already narrow: the shape javac's
    /// own `i2b`/`i2s`/`i2c` produced, an array element of that width, an
    /// in-range constant, or — for `Z` — a comparison or `instanceof`, whose
    /// result is 0 or 1. So javac-compiled code builds the graph it built
    /// before; only bytecode that stores an unnarrowed int pays the
    /// conversion.
    pub(in crate::ir) fn narrow_to_field_type(
        &mut self,
        value: NodeId,
        type_tag: u8,
        pc: usize,
    ) -> NodeId {
        if !matches!(type_tag, b'B' | b'S' | b'C' | b'Z') {
            return value;
        }
        let Some(node) = self.graph.node_opt(value) else {
            return value;
        };
        if node.ty != IrType::Int {
            return value;
        }
        let konst = match node.op {
            Op::Const(c) => Some(c as i32),
            _ => None,
        };
        // `Some(bits)` when `value` is `(x << (32 - bits)) >> (32 - bits)`,
        // i.e. already sign-extended from `bits` bits.
        let sext_from = self.sign_extended_from_bits(value);
        // The constant mask of `value = x & mask`, if it is one.
        let and_mask = match node.op {
            Op::And => {
                node.inputs
                    .iter()
                    .find_map(|&i| match self.graph.node_opt(i).map(|n| &n.op) {
                        Some(Op::Const(m)) => Some(*m as i32),
                        _ => None,
                    })
            }
            _ => None,
        };
        let already_narrow = match type_tag {
            b'B' => {
                sext_from == Some(8)
                    || matches!(node.op, Op::ArrayLoad(MemKind::Byte))
                    || konst.is_some_and(|c| c == i32::from(c as i8))
            }
            b'S' => {
                matches!(sext_from, Some(8) | Some(16))
                    || matches!(
                        node.op,
                        Op::ArrayLoad(MemKind::Short) | Op::ArrayLoad(MemKind::Byte)
                    )
                    || konst.is_some_and(|c| c == i32::from(c as i16))
            }
            b'C' => {
                and_mask.is_some_and(|m| m & !0xFFFF == 0)
                    || matches!(node.op, Op::ArrayLoad(MemKind::Char) | Op::StrCharDecode)
                    || konst.is_some_and(|c| c == i32::from(c as u16))
            }
            _ => {
                and_mask.is_some_and(|m| m & !1 == 0)
                    || matches!(node.op, Op::Cmp(_) | Op::InstanceOf { .. })
                    || konst.is_some_and(|c| c == 0 || c == 1)
            }
        };
        if already_narrow {
            return value;
        }
        match type_tag {
            b'B' | b'S' => {
                let shift = self.iconst(if type_tag == b'B' { 24 } else { 16 });
                let shl = self.add_data(Op::Shl, IrType::Int, vec![value, shift], pc);
                self.add_data(Op::Shr, IrType::Int, vec![shl, shift], pc)
            }
            b'C' => {
                let mask = self.iconst(0xFFFF);
                self.add_data(Op::And, IrType::Int, vec![value, mask], pc)
            }
            _ => {
                let one = self.iconst(1);
                self.add_data(Op::And, IrType::Int, vec![value, one], pc)
            }
        }
    }

    /// `Some(bits)` when `id` is `Shr(Shl(x, k), k)` with the same constant
    /// `k` in `1..32` on both — the shape the `i2b`/`i2s` arms build — i.e.
    /// `x` sign-extended from its low `32 - k` bits.
    pub(in crate::ir) fn sign_extended_from_bits(&self, id: NodeId) -> Option<u32> {
        let const_of = |n: NodeId| match self.graph.node_opt(n).map(|n| &n.op) {
            Some(Op::Const(c)) => Some(*c),
            _ => None,
        };
        let shr = self.graph.node_opt(id)?;
        if shr.op != Op::Shr || shr.inputs.len() != 2 {
            return None;
        }
        let shl = self.graph.node_opt(shr.inputs[0])?;
        if shl.op != Op::Shl || shl.inputs.len() != 2 {
            return None;
        }
        let k = const_of(shr.inputs[1])?;
        if const_of(shl.inputs[1])? != k || !(1..32).contains(&k) {
            return None;
        }
        Some(32 - k as u32)
    }

    /// cov-01: supply resolved `ldc` / `ldc_w` IMMEDIATE constants
    /// (`pc → (bits, is_float)`). Must be called before [`Self::build`]; a pc
    /// not present makes that `ldc` bail the method to single-pass.
    pub fn set_ldc_info(&mut self, info: HashMap<usize, (i64, bool)>) {
        self.ldc_info = info;
    }

    /// cov-01 increment 2: supply resolved `ldc <String>` sites
    /// (`pc → (bytes, len)`). The caller must keep the bytes alive for the
    /// artifact's lifetime — the lowered body bakes their address.
    pub fn set_ldc_string_info(&mut self, info: HashMap<usize, (u32, u16)>) {
        self.ldc_string_info = info;
    }

    /// cov-01 increment 3: supply resolved `ldc <Class>` sites
    /// (`pc → (holder_class_id, cp_idx)`). Only sites whose
    /// `helpers.ldc_class_cp` is wired may be supplied.
    pub fn set_ldc_class_info(&mut self, info: HashMap<usize, (u32, u16)>) {
        self.ldc_class_info = info;
    }

    /// JIT round 20 wave 12 (lane qMisc, NIO20-L3-1): supply the
    /// `x.getClass() == C.class` pairs [`Self::try_fold_getclass_exact_compare`]
    /// may fold (`lib.rs` `ir_getclass_exact_compares`).
    pub fn set_getclass_exact_compares(&mut self, pairs: HashMap<usize, u32>) {
        self.getclass_exact_compares = pairs;
    }

    /// Round 9 wave 8 (irl8): supply the GC-maintained constant slots of
    /// resolved `ldc <String>` / `ldc <Class>` sites,
    /// `(holder_class_id, cp_idx) -> slot address` (the `slot_addr` of
    /// `JitLdcConstant::StringSlot` / `ClassMirrorSlot`). A site with no entry,
    /// or an entry of `0`, keeps the plain helper call. Only a site that is
    /// also in [`Self::set_ldc_string_info`] / [`Self::set_ldc_class_info`] is
    /// built at all.
    pub fn set_ldc_slot_info(&mut self, info: HashMap<(u32, u16), usize>) {
        self.ldc_slot_info = info;
    }

    /// JIT r23 wave 4b, lane L2: supply [`Self::ldc_condy_slot_info`].
    pub fn set_ldc_condy_slot_info(&mut self, info: HashMap<usize, usize>) {
        self.ldc_condy_slot_info = info;
    }

    /// cov-01 increment 4: supply resolved `getstatic` sites
    /// (`pc → (class_id, field_index, type_tag, is_volatile)`). Must be called
    /// before [`Self::build`]; an absent pc bails that `getstatic` to
    /// single-pass.
    pub fn set_static_field_info(&mut self, info: HashMap<usize, (u32, usize, u8, bool)>) {
        self.static_field_info = info;
    }

    /// The class a resolved `new` at `pc` allocates ([`Self::set_new_info`]).
    pub fn new_class_at(&self, pc: usize) -> Option<u32> {
        self.new_info.get(&pc).map(|&(class_id, _)| class_id)
    }

    /// The resolved `getstatic` row at `pc` ([`Self::set_static_field_info`]).
    pub fn static_field_row(&self, pc: usize) -> Option<(u32, usize, u8, bool)> {
        self.static_field_info.get(&pc).copied()
    }

    /// The non-volatile reference static fields this build reads, its own
    /// sites and every spliced body's, as `(class id, field index)` -- the
    /// keys [`Self::set_static_final_exact_classes`] is asked about.
    pub fn static_reference_field_keys(&self) -> Vec<(u32, u32)> {
        let mut keys: Vec<(u32, u32)> = self
            .static_field_info
            .values()
            .filter(|&&(_, _, tag, is_volatile)| tag == b'L' && !is_volatile)
            .filter_map(|&(class_id, index, _, _)| Some((class_id, u32::try_from(index).ok()?)))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    /// `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: the class of the object
    /// each trusted `static final` reference field this build reads holds, as
    /// the VM filed it at the end of the declaring class's `<clinit>`. An
    /// `Op::LoadStatic` of such a field is then exactly that class
    /// ([`Self::exact_receiver_node_proof`]): the field holds that one object
    /// forever (its class is initialised, and no method but `<clinit>` writes
    /// it) and an object never changes class, so the proof adds no node and no
    /// speculation, as a `new` does not. Empty: nothing is proven.
    pub fn set_static_final_exact_classes(&mut self, classes: HashMap<(u32, u32), u32>) {
        self.graph.static_final_non_null = classes
            .iter()
            .filter(|&(_, &c)| c != 0)
            .map(|(&k, _)| k)
            .collect();
        self.static_final_exact_classes = classes;
    }

    /// The resolved catch classes of [`Self::set_local_throw_rows`], for the
    /// subclass answers a spliced throw edge may use
    /// ([`Self::add_local_throw_subclass_facts`]).
    pub fn local_throw_catch_classes(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .local_throw_rows
            .iter()
            .filter_map(|r| match r.catch {
                IrCatchClass::Class(c) => Some(c),
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// More `(thrown, catch) -> caught` answers on top of
    /// [`Self::set_local_throw_subclass_facts`]'s: those for the classes of
    /// the trusted `static final` references this build reads, which a
    /// spliced throw edge decides without a type test.
    pub fn add_local_throw_subclass_facts(&mut self, facts: HashMap<(u32, u32), bool>) {
        self.local_throw_subclass.extend(facts);
    }

    /// `CRATONVM_FROZEN_PAGES` (stage 1b of
    /// `docs/feature-designs/jit-ir-optimizations.md`): the snapshot a
    /// `getfield` on a frozen object may fold from ([`Self::fold_frozen_field`]).
    pub fn set_frozen_facts(&mut self, facts: std::sync::Arc<crate::frozen_facts::FrozenFacts>) {
        self.frozen_facts = Some(facts);
    }

    /// Stage 2 of `docs/feature-designs/jit-ir-optimizations.md` (heap-specializing tier):
    /// `by_class` names, per class, where the receiver the interpreter calls
    /// that class's methods on nearly always lives, and per call site
    /// (`"class.name:descriptor@bci"`) the same for the calls made there: a
    /// class used through two objects can still have one per site. Call after
    /// [`Self::set_frozen_facts`]. Where the snapshot holds an object there,
    /// a field read of a receiver of that class (the compiling method's own,
    /// or a spliced body's) is folded behind `receiver == address` and the
    /// object's page guards ([`Self::fold_frozen_field`]). `method_key` is
    /// `"class.name:descriptor"`, the compiling method's `despec` key.
    pub fn set_frozen_receivers(
        &mut self,
        by_class: Option<&HashMap<String, usize>>,
        despec: Option<std::sync::Arc<crate::deopt::DespecRegistry>>,
        method_key: String,
    ) {
        if let (Some(facts), Some(by_class)) = (self.frozen_facts.as_ref(), by_class) {
            self.frozen_expected = by_class
                .iter()
                .filter_map(|(class, addr)| Some((class.clone(), facts.object_id(*addr)?)))
                .collect();
            // Freezing on demand: the VM is still adding an object this
            // method's receiver or own call sites nearly always see. Miss it,
            // so the method is built again once the snapshot holds it.
            // shortcut: the sites of spliced bodies do not miss; extend if a
            // row shows its fold only after a recompile it never gets.
            let own_class = method_key.split_once('.').map(|(class, _)| class);
            let own_sites = format!("{method_key}@");
            let mut misses: Vec<usize> = by_class
                .iter()
                .filter(|(key, addr)| {
                    (Some(key.as_str()) == own_class || key.starts_with(&own_sites))
                        && facts.object_id(**addr).is_none()
                })
                .map(|(_, &addr)| addr)
                .collect();
            misses.sort_unstable();
            for addr in misses {
                self.note_frozen_miss(crate::frozen_facts::FrozenMiss::Object(addr));
            }
        }
        self.frozen_despec = despec.map(|d| (d, method_key));
    }

    /// The resolved instance-field row `(field index, type tag)` of the
    /// `getfield` / `putfield` at `pc`, for the planner's operand reading.
    pub fn field_row(&self, pc: usize) -> Option<(usize, u8)> {
        self.field_info.get(&pc).copied()
    }

    /// The object `node` is expected to be, when it is a receiver: the
    /// compiling method's own, or that of a spliced body the walk is in.
    /// Evidence, not proof.
    pub(in crate::ir) fn frozen_receiver(&self, node: NodeId) -> Option<u32> {
        if self.frozen_expected.is_empty() {
            return None;
        }
        let own = self.graph.receiver_param == Some(0)
            && self
                .graph
                .nodes
                .get(node as usize)
                .is_some_and(|n| matches!(n.op, Op::Param(0)) && n.ty == IrType::Ref);
        let top_key = || self.frozen_despec.as_ref().map(|(_, key)| key.as_str());
        // The body whose receiver `node` is: `None` for the compiling method's.
        let frame = if own {
            None
        } else if let Some(at) = self.splice.iter().rposition(|frame| frame.receiver == node) {
            Some(at)
        } else if self.locals.first() == Some(&node) {
            // Local 0 of the body the walk is in, which never writes it.
            match self.splice.last() {
                Some(frame)
                    if frame.receiver != NO_NODE
                        && self.frozen_keeps_this.contains(&frame.base) =>
                {
                    Some(self.splice.len() - 1)
                }
                None if self.graph.receiver_param == Some(0)
                    && self.frozen_keeps_this.contains(&0) =>
                {
                    None
                }
                _ => return None,
            }
        } else {
            return None;
        };
        // A spliced body: first the object its call site is called on.
        if let Some(id) = frame
            .and_then(|at| self.frozen_site_object(at.checked_sub(1), self.splice[at].invoke_pc))
        {
            return Some(id);
        }
        let key = match frame {
            Some(at) => Some(self.splice[at].method_key.as_str()),
            None => top_key(),
        };
        // `"class.name:descriptor"`; a class name has no `.` in it.
        self.frozen_expected.get(key?.split_once('.')?.0).copied()
    }

    /// The object the interpreter made the virtual call at combined pc
    /// `invoke_pc` on nearly always, by its site row: the call is in the
    /// body of splice frame `scope` (`None`: the compiling method's code).
    pub(in crate::ir) fn frozen_site_object(
        &self,
        scope: Option<usize>,
        invoke_pc: usize,
    ) -> Option<u32> {
        let (caller, base) = match scope {
            Some(at) => (
                self.splice.get(at)?.method_key.as_str(),
                self.splice[at].base,
            ),
            None => (self.frozen_despec.as_ref()?.1.as_str(), 0),
        };
        let bci = invoke_pc.checked_sub(base)?;
        self.frozen_expected
            .get(&format!("{caller}@{bci}"))
            .copied()
    }

    /// Per-object clones by call site: the planner planned the exact body of
    /// `class_id` for the call at `pc` because the interpreter made it on one
    /// snapshot object of that class nearly always. Plant `receiver == slot`
    /// via [`Op::IndyRootRef`] so the receiver is that object on the path from
    /// here. Builds nothing where the guard could not resume or was withdrawn.
    pub(in crate::ir) fn frozen_prove_site_receiver(&mut self, pc: usize, class_id: u32) {
        let Some(receiver) = self.exact_receiver_operand(pc) else {
            return;
        };
        if self.frozen_object_of(receiver).is_some() {
            return;
        }
        let Some(id) = self.frozen_site_object(self.splice.len().checked_sub(1), pc) else {
            return;
        };
        let Some(facts) = self.frozen_facts.clone() else {
            return;
        };
        let Some(object) = facts.live(id).filter(|o| {
            o.class_id == class_id && o.slot_addr != 0 && o.fields.iter().any(Option::is_some)
        }) else {
            return;
        };
        let identity_ok = self.frozen_identity_admitted(pc);
        if !identity_ok
            && object.class_id != 0
            && !self.graph.frozen_failed_classes.contains(&object.class_id)
        {
            self.graph.frozen_failed_classes.push(object.class_id);
        }
        if !self.memo_depth_admitted() || !identity_ok {
            return;
        }
        let resumable = if self.splice.is_empty() {
            self.has_frame_state_at(pc)
        } else {
            !self.sync_splice_open() && self.relax_fence_by_chain(pc)
        };
        let Some(ctrl) = self.ctrl_opt().filter(|_| resumable) else {
            return;
        };
        let want = self.graph.add(
            Op::IndyRootRef {
                slot_addr: object.slot_addr,
            },
            IrType::Ref,
            vec![ctrl],
            Some(pc),
        );
        let same = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![receiver, want], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, same],
            Some(pc),
        );
        self.graph.frozen_identity_guards += 1;
        let depth = self.splice.len();
        self.frozen_known.push((receiver, id, depth));
        if object.class_id != 0 && !self.graph.frozen_folded_classes.contains(&object.class_id) {
            self.graph.frozen_folded_classes.push(object.class_id);
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] frozen site receiver at combined pc {pc} in {}: object {id} of class {class_id}",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
    }

    /// May an identity guard go at `pc`? Not where one was withdrawn: at
    /// `pc` itself in the method's own code, at the outermost call site
    /// inside a splice (where a chain trap is charged).
    pub(in crate::ir) fn frozen_identity_admitted(&self, pc: usize) -> bool {
        let Some((despec, key)) = self.frozen_despec.as_ref() else {
            return false;
        };
        let bci = self.splice.first().map_or(pc, |outer| outer.invoke_pc);
        u32::try_from(bci).is_ok_and(|b| !despec.contains(key, b))
    }

    /// A join: what was guarded at splice depth `depth` or deeper may be on
    /// one incoming edge only.
    pub(in crate::ir) fn frozen_scope_clear_from(&mut self, depth: usize) {
        self.frozen_known.retain(|&(_, _, d)| d < depth);
    }

    /// Definition-time snapshot object of `node` (holds on every path where
    /// `node` is defined; excludes flow-scoped `frozen_known` identity guards).
    pub(in crate::ir) fn frozen_def_object_of(&self, node: NodeId) -> Option<u32> {
        let facts = self.frozen_facts.as_ref()?;
        if let Some(&id) = self.frozen_nodes.get(&node) {
            return Some(id);
        }
        if self.locals.first() == Some(&node) {
            if let Some(frame) = self.splice.last() {
                if frame.receiver != NO_NODE
                    && frame.receiver != node
                    && self.frozen_keeps_this.contains(&frame.base)
                {
                    return self.frozen_def_object_of(frame.receiver);
                }
            }
        }
        match self.graph.nodes.get(node as usize).map(|n| &n.op) {
            Some(&Op::LoadStatic {
                class_id,
                field_index,
                type_tag: b'L' | b'[',
                is_volatile: false,
            }) if self
                .static_final_exact_classes
                .get(&(class_id, field_index))
                .is_some_and(|&c| c != 0) =>
            {
                facts.roots.get(&(class_id, field_index)).copied()
            }
            _ => None,
        }
    }

    /// Every-path exact class of `node` from definition-time snapshot facts.
    pub(in crate::ir) fn frozen_def_class_of(&self, node: NodeId) -> Option<u32> {
        let id = self.frozen_def_object_of(node)?;
        let class_id = self
            .frozen_facts
            .as_ref()?
            .objects
            .get(id as usize)?
            .class_id;
        (class_id != 0).then_some(class_id)
    }

    /// The snapshot object `node` is at the current walk point (definition-time
    /// or dominated by an active `frozen_known` identity guard).
    pub(in crate::ir) fn frozen_object_of(&self, node: NodeId) -> Option<u32> {
        let _ = self.frozen_facts.as_ref()?;
        if let Some(id) = self.frozen_def_object_of(node) {
            return Some(id);
        }
        if let Some(&(_, id, _)) = self.frozen_known.iter().find(|&&(n, _, _)| n == node) {
            return Some(id);
        }
        if self.locals.first() == Some(&node) {
            if let Some(frame) = self.splice.last() {
                if frame.receiver != NO_NODE
                    && frame.receiver != node
                    && self.frozen_keeps_this.contains(&frame.base)
                {
                    return self.frozen_object_of(frame.receiver);
                }
            }
        }
        None
    }

    /// The exact class of the snapshot object `node` is at the current point.
    pub(in crate::ir) fn frozen_class_of(&self, node: NodeId) -> Option<u32> {
        let id = self.frozen_object_of(node)?;
        let class_id = self
            .frozen_facts
            .as_ref()?
            .objects
            .get(id as usize)?
            .class_id;
        (class_id != 0).then_some(class_id)
    }

    /// Resolve `base` to `(object_id, already_known, snapshot_arc)`, recording
    /// any `LoadStatic` holder dependency on `frozen_cha_deps` when `base` is a
    /// `static final` snapshot root.
    fn resolve_frozen_object(
        &mut self,
        base: NodeId,
    ) -> Option<(u32, bool, std::sync::Arc<crate::frozen_facts::FrozenFacts>)> {
        let facts = std::sync::Arc::clone(self.frozen_facts.as_ref()?);
        let known = self.frozen_object_of(base);
        if let Some(id) = known.or_else(|| self.frozen_receiver(base)) {
            if let Some(&Op::LoadStatic {
                class_id,
                field_index,
                type_tag: b'L' | b'[',
                is_volatile: false,
            }) = self.graph.nodes.get(base as usize).map(|n| &n.op)
            {
                if let Some(owner) = facts.root_owners.get(&(class_id, field_index)) {
                    let dep = format!("{}{owner}", crate::frozen_facts::CHA_FIELD_DEP_PREFIX);
                    if !self.graph.frozen_cha_deps.contains(&dep) {
                        self.graph.frozen_cha_deps.push(dep);
                    }
                }
            }
            return Some((id, known.is_some(), facts));
        }
        // A trusted root the snapshot does not hold yet.
        if let Some(&Op::LoadStatic {
            class_id,
            field_index: root,
            type_tag: b'L' | b'[',
            is_volatile: false,
        }) = self.graph.nodes.get(base as usize).map(|n| &n.op)
        {
            if self
                .static_final_exact_classes
                .get(&(class_id, root))
                .is_some_and(|&c| c != 0)
            {
                self.note_frozen_miss(crate::frozen_facts::FrozenMiss::Root(class_id, root));
            }
        }
        None
    }

    fn frozen_live_object<'a>(
        &mut self,
        facts: &'a crate::frozen_facts::FrozenFacts,
        id: u32,
    ) -> Option<&'a crate::frozen_facts::FrozenObject> {
        let Some(object) = facts.live(id) else {
            if let Some(addr) = facts
                .objects
                .get(id as usize)
                .map(|o| o.addr)
                .filter(|&a| a != 0)
            {
                self.note_frozen_miss(crate::frozen_facts::FrozenMiss::Object(addr));
            }
            return None;
        };
        Some(object)
    }

    fn emit_frozen_guards(
        &mut self,
        code_len: usize,
        pc: usize,
        base: NodeId,
        id: u32,
        object: &crate::frozen_facts::FrozenObject,
        known: bool,
        cha_owner: Option<&str>,
        slot_index: usize,
    ) -> bool {
        let identity_due = !known;
        if identity_due {
            if object.slot_addr == 0 {
                self.frozen_refusal(pc, id, slot_index, "no root slot minted for receiver");
                return false;
            }
            let Some(ctrl) = self.ctrl_opt() else {
                return false;
            };
            let resumable = if self.splice.is_empty() {
                pc < code_len && self.has_frame_state_at(pc)
            } else {
                !self.sync_splice_open() && self.relax_fence_by_chain(pc)
            };
            if !resumable || !self.frozen_identity_admitted(pc) {
                if resumable
                    && object.class_id != 0
                    && !self.graph.frozen_failed_classes.contains(&object.class_id)
                {
                    self.graph.frozen_failed_classes.push(object.class_id);
                }
                self.frozen_refusal(
                    pc,
                    id,
                    slot_index,
                    if resumable {
                        "the identity guard was withdrawn here"
                    } else {
                        "no frame to resume at"
                    },
                );
                return false;
            }
            let want = self.graph.add(
                Op::IndyRootRef {
                    slot_addr: object.slot_addr,
                },
                IrType::Ref,
                vec![ctrl],
                Some(pc),
            );
            let same = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![base, want], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, same],
                Some(pc),
            );
            self.graph.frozen_identity_guards += 1;
            if self.memo_depth_admitted() {
                self.frozen_known.push((base, id, self.splice.len()));
            }
        }
        if let Some(owner) = cha_owner.filter(|s| !s.is_empty()) {
            let dep = format!("{}{owner}", crate::frozen_facts::CHA_FIELD_DEP_PREFIX);
            if !self.graph.frozen_cha_deps.contains(&dep) {
                self.graph.frozen_cha_deps.push(dep);
            }
        }
        if object.class_id != 0 && !self.graph.frozen_folded_classes.contains(&object.class_id) {
            self.graph.frozen_folded_classes.push(object.class_id);
        }
        true
    }

    pub(in crate::ir) fn frozen_value_to_const(
        &mut self,
        ty: IrType,
        val: crate::frozen_facts::FrozenValue,
    ) -> Option<NodeId> {
        use crate::frozen_facts::FrozenValue;
        match (ty, val) {
            (IrType::Int, FrozenValue::Int(v)) => Some(self.iconst(i64::from(v))),
            (IrType::Long, FrozenValue::Long(v)) => {
                Some(self.graph.add(Op::Const(v), IrType::Long, vec![], None))
            }
            (IrType::Float, FrozenValue::FloatBits(b)) => {
                Some(
                    self.graph
                        .add(Op::ConstF(u64::from(b)), IrType::Float, vec![], None),
                )
            }
            (IrType::Double, FrozenValue::DoubleBits(b)) => {
                Some(self.graph.add(Op::ConstF(b), IrType::Double, vec![], None))
            }
            _ => None,
        }
    }

    /// Materialize a folded primitive constant or GC-relocated `Op::IndyRootRef`.
    pub(in crate::ir) fn materialize_frozen_value(
        &mut self,
        pc: usize,
        ty: IrType,
        val: crate::frozen_facts::FrozenValue,
    ) -> Option<NodeId> {
        use crate::frozen_facts::FrozenValue;
        if let (IrType::Ref, FrozenValue::Ref(target_id)) = (ty, val) {
            let (slot_addr, exact_cid) = {
                let target = self
                    .frozen_facts
                    .as_ref()?
                    .objects
                    .get(target_id as usize)?;
                if target.slot_addr == 0 {
                    return None;
                }
                (target.slot_addr, target.class_id)
            };
            let ctrl = self.ctrl_opt()?;
            let node = self.graph.add(
                Op::IndyRootRef { slot_addr },
                IrType::Ref,
                vec![ctrl],
                Some(pc),
            );
            self.frozen_nodes.insert(node, target_id);
            self.const_ref_slots.insert(node, slot_addr);
            if exact_cid != 0 {
                self.const_ref_exact_classes.insert(node, exact_cid);
            }
            return Some(node);
        }
        self.frozen_value_to_const(ty, val)
    }

    fn frozen_value_matches_type(
        facts: &crate::frozen_facts::FrozenFacts,
        ty: IrType,
        val: crate::frozen_facts::FrozenValue,
    ) -> bool {
        use crate::frozen_facts::FrozenValue;
        match (ty, val) {
            (IrType::Ref, FrozenValue::Ref(id)) => facts
                .objects
                .get(id as usize)
                .is_some_and(|t| t.slot_addr != 0),
            (IrType::Int, FrozenValue::Int(_))
            | (IrType::Long, FrozenValue::Long(_))
            | (IrType::Float, FrozenValue::FloatBits(_))
            | (IrType::Double, FrozenValue::DoubleBits(_)) => true,
            _ => false,
        }
    }

    /// Fold `arraylength` when `array` is a snapshot array.
    pub(in crate::ir) fn fold_frozen_array_length(
        &mut self,
        code_len: usize,
        pc: usize,
        array: NodeId,
    ) -> Option<NodeId> {
        let (id, known, facts) = self.resolve_frozen_object(array)?;
        let len = if known {
            facts.objects.get(id as usize).and_then(|o| o.array_len)?
        } else {
            let object = self.frozen_live_object(&facts, id)?;
            let len = object.array_len?;
            if !self.emit_frozen_guards(code_len, pc, array, id, object, false, None, 0) {
                return None;
            }
            len
        };
        self.note_splice_forwarded_read(array);
        Some(self.iconst(i64::from(len)))
    }

    /// Fold a constant-index load from a bounded `@Stable` array.
    pub(in crate::ir) fn fold_frozen_array_load(
        &mut self,
        code_len: usize,
        pc: usize,
        array: NodeId,
        index: NodeId,
        ty: IrType,
    ) -> Option<crate::frozen_facts::FrozenValue> {
        let idx = self
            .graph
            .nodes
            .get(index as usize)
            .and_then(|n| match (n.ty, &n.op) {
                (IrType::Int, Op::Const(c)) => usize::try_from(*c as i32).ok(),
                _ => None,
            })?;
        let (id, known, facts) = self.resolve_frozen_object(array)?;
        let object = self.frozen_live_object(&facts, id)?;
        let value = object.elements.get(idx).copied().flatten()?;
        if !Self::frozen_value_matches_type(&facts, ty, value) {
            return None;
        }
        let owner = object.elem_cha_owner.as_deref();
        if !self.emit_frozen_guards(code_len, pc, array, id, object, known, owner, idx) {
            return None;
        }
        self.note_splice_forwarded_read(array);
        Some(value)
    }

    /// Fold a CHA effectively-final or non-default `@Stable` instance field read.
    pub(in crate::ir) fn fold_frozen_field(
        &mut self,
        code_len: usize,
        pc: usize,
        base: NodeId,
        field_index: usize,
        ty: IrType,
    ) -> Option<crate::frozen_facts::FrozenValue> {
        let (id, known, facts) = self.resolve_frozen_object(base)?;
        let object = self.frozen_live_object(&facts, id)?;
        let Some(value) = object.fields.get(field_index).copied().flatten() else {
            self.frozen_refusal(
                pc,
                id,
                field_index,
                "the snapshot has no fact for the field",
            );
            return None;
        };
        if !Self::frozen_value_matches_type(&facts, ty, value) {
            return None;
        }
        let owner = object
            .field_cha_owners
            .get(field_index)
            .and_then(|o| o.as_deref());
        if !self.emit_frozen_guards(code_len, pc, base, id, object, known, owner, field_index) {
            return None;
        }
        self.note_splice_forwarded_read(base);
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] frozen fold at combined pc {pc} in {}: object {id} field {field_index} = {value:?}",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
        Some(value)
    }

    /// Under `CRATONVM_DBG_JITC`: a call whose receiver is a known frozen
    /// object was not spliced.
    pub(in crate::ir) fn note_frozen_receiver_call(&self, pc: usize, num_args: usize) {
        if !crate::x64::dbg_jitc_enabled() || self.frozen_facts.is_none() {
            return;
        }
        let receiver = self
            .stack
            .len()
            .checked_sub(num_args)
            .and_then(|at| self.stack.get(at));
        if let Some(class) = receiver.and_then(|&r| self.frozen_class_of(r)) {
            eprintln!(
                "[ir] frozen receiver of class {class} at combined pc {pc} in {}: the site stays a call",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
    }

    /// Under `CRATONVM_DBG_JITC`: why a fold on a live frozen object was not made.
    pub(in crate::ir) fn frozen_refusal(&self, pc: usize, id: u32, field_index: usize, why: &str) {
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] frozen fold REFUSED at combined pc {pc} in {}: object {id} field {field_index}: {why}",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
    }

    pub(in crate::ir) fn note_frozen_miss(&mut self, miss: crate::frozen_facts::FrozenMiss) {
        if self.graph.frozen_misses.len() < 64 && !self.graph.frozen_misses.contains(&miss) {
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[ir] frozen miss in {}: {miss:?}",
                    self.method_label.as_deref().unwrap_or("<unknown>")
                );
            }
            self.graph.frozen_misses.push(miss);
        }
    }

    /// The class [`Self::set_static_final_exact_classes`] or
    /// `const_folds.static_final_const_refs` proves `node` (an
    /// `Op::LoadStatic` or folded `Op::IndyRootRef` of a trusted
    /// `static final` reference) to be.
    pub(in crate::ir) fn static_final_exact_class_of(&self, node: NodeId) -> Option<u32> {
        if let Some(&c) = self.const_ref_exact_classes.get(&node) {
            return Some(c);
        }
        match self.graph.nodes.get(node as usize).map(|n| &n.op) {
            Some(&Op::LoadStatic {
                class_id,
                field_index,
                type_tag: b'L' | b'[',
                is_volatile: false,
            }) => self
                .static_final_exact_classes
                .get(&(class_id, field_index))
                .copied()
                // `0` files a null value (see the `getstatic` arm).
                .filter(|&c| c != 0 && c != STATIC_FINAL_OBJECT_FILED),
            Some(&Op::IndyRootRef { slot_addr }) => self
                .graph
                .const_ref_meta
                .get(&slot_addr)
                .and_then(|m| m.exact_class_id)
                .or_else(|| {
                    self.const_folds
                        .static_final_const_refs
                        .values()
                        .find(|&&(addr, cid)| {
                            addr == slot_addr && cid != 0 && cid != STATIC_FINAL_OBJECT_FILED
                        })
                        .map(|&(_, cid)| cid)
                }),
            _ => None,
        }
    }

    /// Round 16 wave 14 (lane osrir, MON3-2): the classes the compile's
    /// static-base resolver reports initialised. Only
    /// [`Self::static_load_memo_scan`] reads it: a plain `getstatic` or
    /// `putstatic` of such a class runs no `<clinit>`, so it cannot write a
    /// static another memo entry names. An empty set only costs forwarding.
    pub fn set_static_initialized_classes(&mut self, classes: HashSet<u32>) {
        self.static_initialized_classes = classes;
    }

    /// Extend [`Self::static_initialized_classes`] with the spliced bodies'
    /// static classes that the compile's resolver reports initialised
    /// (monotone: a class once initialised stays so). `lib.rs` feeds this
    /// after [`Self::apply_inline_tables`].
    pub fn add_static_initialized_classes(&mut self, classes: impl IntoIterator<Item = u32>) {
        self.static_initialized_classes.extend(classes);
    }

    /// Round nio20 wave 6 (lane L1): `(class_id, field_index)` statics whose
    /// `getstatic` the lowering serves as the direct read
    /// ([`Graph::plain_static_reads`], [`plain_static_read_effect`]). Merged,
    /// not replaced: `lib.rs` feeds the compiling method's own rows and then
    /// every spliced body's. Must be called before [`Self::build`] so the
    /// builder's field memo sees the answer too. The caller vouches for each
    /// row (`DirectHelperTable::resolve_static_base` answered, the inline
    /// route on, the field not volatile).
    pub fn add_plain_static_reads(&mut self, rows: impl IntoIterator<Item = (u32, u32)>) {
        self.graph.plain_static_reads.extend(rows);
    }

    /// cov-05 increment 1: supply resolved `instanceof` sites whose target
    /// class is already loaded (`pc → (name_ptr, name_len)`). Must be called
    /// before [`Self::build`]; an absent pc bails that `instanceof` to
    /// single-pass — including every not-yet-loaded target, which the caller
    /// must not insert here (see [`Op::InstanceOf`]).
    pub fn set_instanceof_info(&mut self, info: HashMap<usize, (usize, usize)>) {
        self.instanceof_info = info;
    }

    /// cov-05: supply resolved `checkcast` sites whose target class is
    /// already loaded (`pc → (name_ptr, name_len)`). Must be called before
    /// [`Self::build`]; an absent pc bails that `checkcast` to single-pass —
    /// including every not-yet-loaded target, which the caller must not
    /// insert here (see [`Op::CheckCast`]).
    pub fn set_checkcast_info(&mut self, info: HashMap<usize, (usize, usize)>) {
        self.checkcast_info = info;
    }

    /// Supply THIS compile's resolved `java/lang/String` field layout for the
    /// String access expander. Must be called before [`Self::build`]; it is
    /// the ONLY way a builder gets a layout (round 11 wave 3 retired the
    /// process-wide seed [`IrBuilder::new`] used to take, which in a process
    /// with two VMs over different class libraries could carry another VM's
    /// field indices).
    ///
    /// `None` — no layout resolved for this compile, or a builder nobody
    /// called this on — leaves every String call site dispatching as before
    /// and counts it under `no_layout` in [`ir_string_intrinsic_census`].
    /// `lib.rs`'s IR build calls it with the same layout it hands the
    /// single-pass backend
    /// (`docs/feature-designs/jit-ir-optimizations.md`).
    /// nio21 wave 19 (lane L3): the layout the `ThreadLocal.get()` expansion
    /// bakes ([`ThreadLocalIrLayout::from_table`]). Never calling it (or
    /// `None`) builds every such site as the call it was.
    pub fn set_thread_local_layout(&mut self, layout: Option<ThreadLocalIrLayout>) {
        self.thread_local_layout = layout;
    }

    pub fn set_nio_charset_layout(&mut self, layout: Option<NioCharsetIrLayout>) {
        self.nio_charset_layout = layout;
    }

    /// JIT round 21 (lane refs2; re-landed t1 wave 9, lane REFS2): the
    /// referent index the `refersTo` expansion uses when this compile has no
    /// `ThreadLocal` layout ([`refers_to_referent_index_from_table`]). `None`
    /// (or never calling it) keeps tl3's rule: no `ThreadLocal` layout, no
    /// expansion.
    pub fn set_refers_to_referent_index(&mut self, index: Option<u32>) {
        self.refers_to_referent_index = index;
    }

    /// t1 wave 12 (lane REFS4, `CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW`): the
    /// VM's `1 + compact displacement` of `Reference.referent`
    /// ([`refers_to_referent_row_from_table`]), which the `refersTo`
    /// expansions' referent reads carry as their compact row. `0` (or never
    /// calling it) leaves those reads row-less, as before.
    pub fn set_refers_to_referent_row(&mut self, row: u32) {
        self.refers_to_referent_row = row;
    }

    /// j27 wave 1 (lane TYPECHK, `CRATONVM_JIT_IR_REFERENT_SPLICE_COMPACT_ROW`):
    /// the VM's `1 + compact displacement` of `Reference.referent`
    /// ([`spliced_referent_row_from_table`]), which the spliced
    /// `Reference.get` referent reads carry as their compact row. `0` (or
    /// never calling it) leaves those reads row-less, as before.
    pub fn set_spliced_referent_row(&mut self, row: u32) {
        self.spliced_referent_row = row;
    }

    /// j27 wave 1 (lane TYPECHK): give a spliced `Reference.get` referent
    /// read `load` (a `getfield` of slot 0 at a `Graph::referent_load_pcs`
    /// pc) the referent's compact row ([`Self::set_spliced_referent_row`]).
    ///
    /// Without it a COMPACT receiver -- since the TLAB fast path stamps
    /// compact headers, nearly every `Reference` -- took the checked
    /// `jit_getfield` helper on every read (page
    /// `weakref-get-and-refersto-read-a-compact-referent-through-jit-getfield-FIXED-20261029.md`).
    ///
    /// Sound on the terms of [`Self::note_referent_row`] (the displacement is
    /// `Reference`'s own compact prefix, which every subclass repeats; the
    /// splice's guard admits subclasses such as `WeakHashMap$Entry`), plus
    /// what a HANDED-OUT referent owes, which the lowering keeps for every
    /// `referent_load_pcs` read with a row (`emit_inline_compact_getfield`):
    /// the SATB pre-gate byte tested after the read, the checked helper (its
    /// keep-alive) whenever it is armed, and never the proven-receiver form
    /// (which has no such test). The VM names the row only on Gen and G1
    /// (`jit_thread_local_ir_layout`'s referent-row query): under ZGC the
    /// helper's resurrection gate is owed, so a ZGC compile keeps the helper.
    pub(in crate::ir) fn note_spliced_referent_row(&mut self, load: NodeId) {
        let row = self.spliced_referent_row;
        if row == 0 || !cratonvm_types::compact_ref_fields_enabled() {
            return;
        }
        self.graph.field_rows.insert(load, (row - 1, true, b'L'));
    }

    /// t1 wave 12 (lane REFS4): give a `refersTo` referent read `load` the
    /// referent's compact row ([`Self::set_refers_to_referent_row`]).
    ///
    /// Sound for every receiver the expansions admit: `referent` is
    /// `Reference`'s first declared instance field and `Reference` extends
    /// `Object`, so its displacement is `Reference`'s own compact prefix,
    /// which every subclass's compact layout repeats (the parent-prefix
    /// property of `ClassStore::build_compact_layout`). A legacy receiver
    /// still reads through the legacy arm, a replaced layout takes the epoch
    /// guard's helper, and the lowering checks the row's reference-ness
    /// against the node's type.
    pub(in crate::ir) fn note_referent_row(&mut self, load: NodeId) {
        let row = self.refers_to_referent_row;
        if row == 0
            || !ir_refers_to_compact_row_enabled()
            || !cratonvm_types::compact_ref_fields_enabled()
        {
            return;
        }
        self.graph.field_rows.insert(load, (row - 1, true, b'L'));
    }

    /// JIT round 21 (lane g1vhsoft, `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`):
    /// the `WeakReference.get()` / `SoftReference.get()` expansion's layout
    /// ([`ReferenceGetIrLayout::from_table`]). `None` (or never calling it)
    /// builds every such site exactly as before.
    pub fn set_reference_get_layout(&mut self, layout: Option<ReferenceGetIrLayout>) {
        self.reference_get_layout = layout;
    }

    /// JIT round 21 (lane tlsub): the `ThreadLocal` expansion's site plan
    /// ([`ThreadLocalSitePlan::for_compile`]). `None` (or never calling it)
    /// builds every site exactly as before.
    pub fn set_thread_local_site_plan(&mut self, plan: Option<ThreadLocalSitePlan>) {
        self.tl_site_plan = plan.map(Box::new);
    }

    /// JIT round 23 wave 9 (lane W9-L2, CF-W9-3): the helper table the
    /// constant-`VarHandle` fresh-receiver build asks its plans through
    /// ([`Self::try_vh_const_fresh_field_access`]). Never calling it (or
    /// `None`) builds every such site as the call it was.
    pub fn set_vh_fresh_helpers(&mut self, helpers: Option<crate::DirectHelperTable>) {
        self.vh_fresh_helpers = helpers;
    }

    /// JIT round 21 (lane strintr2, STRINTR21-1,
    /// `CRATONVM_JIT_IR_STRING_ACCESS_TRAP_WITHDRAW`): the `invokevirtual` pcs
    /// of this method whose String-accessor guard the VM's de-spec registry
    /// withdrew (`lib.rs::string_access_withdrawn_pcs`). The accessor expander
    /// refuses those sites, so each is built as its `Op::Call` (which the
    /// lowering fronts with the declining `StrLength` / `StrIsEmpty` /
    /// `StrCharAt` prefix) instead of deopting at every miss again.
    pub fn set_string_access_withdrawn_pcs(&mut self, pcs: HashSet<usize>) {
        self.string_access_withdrawn_pcs = pcs;
    }

    pub fn set_string_layout(&mut self, layout: Option<crate::StringFieldLayout>) {
        self.string_layout = layout;
        // JIT round 19 proposal wave 5, lane concat5 (STRGAP4-1): the lowering's
        // inline concat reads it from the graph (`Graph::string_layout`).
        self.graph.string_layout = layout;
    }

    /// Is the 14-byte run at `pc` the `Dist` adapter
    /// `Integer.valueOf(int)`, `Function.apply(Object)`, `checkcast Double`,
    /// `Double.doubleValue()` that [`Op::LambdaIntToDouble`] stands for?
    ///
    /// Round 11 wave 2. The rewrite used to fire on the four OPCODE bytes
    /// alone, while `jit_lambda_int_to_double` hard-codes the four MEMBERS, so a
    /// `Dist` whose body kept the shape but changed a member (another boxing
    /// call, another functional interface, another cast) compiled to the wrong
    /// arithmetic. Mirrors the single-pass
    /// `Compiler::tdigest_adapter_members_match`, and refuses on each of:
    ///
    /// * the run not lying wholly in the compiling method's own code, or the
    ///   walk being inside a splice (a relocated callee is not `Dist`'s body);
    /// * a branch landing on `pc + 3`, `pc + 8` or `pc + 11` — those bcis are
    ///   never built, so a jump there would have no merge to land on;
    /// * a resolved `invoke_info` row at `pc`/`pc + 3`/`pc + 11`, or a
    ///   `checkcast_info` row at `pc + 8`, naming a different member; an
    ///   `inline_sites` plan (the planner chose to splice the call) whose
    ///   `method_key` names a different member is refused the same way. A site
    ///   with neither row (the planner registered it another way) is accepted,
    ///   exactly as the single-pass tier accepts it;
    /// * the operand stack not holding `(Ref lambda, Int index)` on top.
    pub(in crate::ir) fn tdigest_adapter_run_matches(
        &self,
        code: &[u8],
        code_len: usize,
        pc: usize,
    ) -> bool {
        if !self.splice.is_empty() || pc.saturating_add(14) > code_len.min(code.len()) {
            return false;
        }
        let byte = |at: usize| code.get(at).copied();
        let shape = [(pc + 3, 0xb9u8), (pc + 8, 0xc0), (pc + 11, 0xb6)];
        if shape.iter().any(|&(at, opcode)| byte(at) != Some(opcode)) {
            return false;
        }
        if [pc + 3, pc + 8, pc + 11]
            .iter()
            .any(|p| self.merges.contains_key(p))
        {
            return false;
        }
        let invoke_names = |at: usize, class: &str, method: &str| -> bool {
            match self.invoke_info.get(&at) {
                Some(&(info_ptr, _, _)) if info_ptr != 0 => {
                    // SAFETY: a non-zero `invoke_info` pointer addresses a
                    // `JitInvokeInfo` the caller (`lib.rs`'s invoke-planning
                    // loop) boxed and keeps alive for the life of the compiled
                    // method — the same read `try_string_access_intrinsic`
                    // performs on the rows of this map.
                    let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
                    info.class_name == class && info.method_name == method
                }
                // No dispatch row because the planner chose to SPLICE the
                // site: the inline plan names its callee, so check that name
                // instead of accepting the site unseen (round 11 wave 5).
                _ => match self.inline_sites.get(&at) {
                    Some(site) => site
                        .method_key
                        .strip_prefix(class)
                        .and_then(|rest| rest.strip_prefix('.'))
                        .and_then(|rest| rest.strip_prefix(method))
                        .is_some_and(|rest| rest.starts_with(':')),
                    None => true,
                },
            }
        };
        let cast_ok = match self.checkcast_info.get(&(pc + 8)) {
            Some(&(name_ptr, name_len)) => {
                // SAFETY: `checkcast_info` rows are `(name_ptr, name_len)`
                // pairs from `lib.rs::intern_typecheck_target`, which leaks its
                // names for the life of the process — the contract
                // `typecheck_site_name` states.
                match unsafe { crate::typecheck_site_name(name_ptr as *const u8, name_len) } {
                    Some(name) => name == "java/lang/Double",
                    None => true,
                }
            }
            None => true,
        };
        let top_ty = |depth: usize| -> Option<IrType> {
            let idx = self.stack.len().checked_sub(depth + 1)?;
            let id = *self.stack.get(idx)?;
            self.graph.node_opt(id).map(|n| n.ty)
        };
        cast_ok
            && top_ty(0) == Some(IrType::Int)
            && top_ty(1) == Some(IrType::Ref)
            && invoke_names(pc, "java/lang/Integer", "valueOf")
            && invoke_names(pc + 3, "java/util/function/Function", "apply")
            && invoke_names(pc + 11, "java/lang/Double", "doubleValue")
    }

    /// Expand a `String.length()` / `isEmpty()` / `charAt(int)` call site into
    /// ordinary IR nodes, or leave the site alone.
    ///
    /// `true` means the site was CONSUMED — operands popped, result pushed —
    /// and the caller must not build an `Op::Call` for it. `false` leaves the
    /// abstract stack exactly as it found it, so the caller's dispatch path is
    /// unaffected; every refusal below returns before the first `pop`.
    ///
    /// The representation, the deopt discipline, the receiver-guard rule and
    /// both switches are all documented in the section above [`IrBuilder`].
    pub(in crate::ir) fn try_string_access_intrinsic(
        &mut self,
        pc: usize,
        info_ptr: usize,
        num_args: usize,
    ) -> bool {
        // Round 14 wave 4 (lane calls2): a site with no invoke row (`0`, as a
        // builder driven without `lib.rs`'s planner hands it) is refused, not
        // dereferenced. Nothing is popped yet, so the caller's dispatch path
        // sees the stack untouched.
        if info_ptr == 0 {
            return false;
        }
        // SAFETY: `info_ptr` is non-zero here and addresses a `JitInvokeInfo` the caller (`lib.rs`'s
        // invoke-planning loop) boxed and keeps alive on the artifact's
        // `_jit_invoke_infos` for the life of the compiled method. It is the
        // same pointer this builder is about to bake into an `Op::Call`, and
        // the same read that loop already performs on its own boxes to
        // recognise `java/lang/Object.<init>()V`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if self.splice.is_empty()
            && super::ir_arraylist_ir_intrinsics_enabled()
            && self.try_collection_access_intrinsic(pc, info, num_args)
        {
            return true;
        }
        // Screen on the declared class before touching any counter: every
        // other invoke in the program would otherwise show up as a "site".
        if info.class_name != "java/lang/String" && info.class_name != "java/lang/CharSequence" {
            return false;
        }
        note_string_intrinsic(SI_SITES_SEEN);
        if !ir_string_intrinsics_enabled() {
            note_string_intrinsic(SI_FLAG_OFF);
            return false;
        }
        let Some(layout) = self.string_layout else {
            note_string_intrinsic(SI_NO_LAYOUT);
            return false;
        };
        // Which sites are accessors, which need a receiver guard, and which
        // are refused for want of a `coder` field (a legacy `char[]` String)
        // or of a String class id, is `try_resolve_string_intrinsic`'s
        // decision — called, not re-derived, so this tier cannot drift from
        // the one the single-pass backend registers against.
        let Some((entry, num_params, _ret, guard_class_id)) = crate::try_resolve_string_intrinsic(
            info.class_name,
            info.method_name,
            info.descriptor,
            Some(layout),
        ) else {
            note_string_intrinsic(SI_NOT_ACCESSOR);
            return false;
        };
        // Compare against the sentinels rather than `JitIntrinsic::from_entry`:
        // that recovery function only classifies the CRC32 family, and the
        // single-pass region identifies these three the same way.
        let kind = if entry == crate::JitIntrinsic::StringLength.as_entry() {
            SI_EMITTED_LENGTH
        } else if entry == crate::JitIntrinsic::StringIsEmpty.as_entry() {
            SI_EMITTED_IS_EMPTY
        } else if entry == crate::JitIntrinsic::StringCharAt.as_entry() {
            SI_EMITTED_CHAR_AT
        } else {
            // `hashCode`, `equals`, `compareTo` and the two `indexOf` forms
            // resolve through the same function and are deliberately NOT
            // expanded here: each is a loop or a lazy-cache protocol rather
            // than a field decode, so each needs control flow this expansion
            // cannot open (see the section header). They keep their dispatch.
            note_string_intrinsic(SI_NOT_ACCESSOR);
            return false;
        };
        // A guarded (CharSequence-declared) site needs a header class-id
        // compare, and there is no IR node that reads an `ObjectHeader`. See
        // the section header for why `Op::InstanceOf` is not a substitute and
        // why emitting the decode unguarded is a wrong-value bug.
        if guard_class_id != 0 {
            note_string_intrinsic(SI_GUARDED_REFUSED);
            return false;
        }
        // A spliced region records no safepoint snapshot, so a guard built
        // inside one would resolve an EMPTY deopt frame at a combined-buffer
        // pc. Refuse; the site keeps its dispatch inside the splice.
        if !self.splice.is_empty() {
            note_string_intrinsic(SI_IN_SPLICE);
            return false;
        }
        // JIT round 21 (lane strintr2, STRINTR21-1): the VM withdrew this
        // site's guard after `PER_BCI_DESPEC_LIMIT` charged traps (a `charAt`
        // whose callers catch the SIOOBE, a null receiver that keeps
        // arriving). The guards below would deopt -- and evict the artifact --
        // on every such arrival again; the call raises the exception itself.
        if self.string_access_withdrawn_pcs.contains(&pc) {
            note_string_intrinsic(SI_TRAP_WITHDRAWN);
            if crate::ir_stage_reporting() {
                eprintln!(
                    "[ir] string-access-withdrawn pc={pc} {}.{}{}: kept as a call",
                    info.class_name, info.method_name, info.descriptor
                );
            }
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            note_string_intrinsic(SI_SHAPE_REFUSED);
            return false;
        };
        // The receiver is one slot and every argument of these three
        // signatures is one slot, so the caller's `num_args` must be exactly
        // `num_params + 1`. A disagreement means the resolved descriptor and
        // the planned site are not describing the same call; refuse rather
        // than pop a depth nothing agreed on.
        if num_args != num_params + 1 || self.stack.len() < num_args {
            note_string_intrinsic(SI_SHAPE_REFUSED);
            return false;
        }

        // ── Past this point the site is consumed. ──
        // Recorded BEFORE the loads are built, so the site is on the list
        // whatever the expansion does after this point — there is no
        // refusal past here, and a list that could disagree with what was
        // emitted would be worse than no list.
        note_string_access_site(pc);
        let index = if kind == SI_EMITTED_CHAR_AT {
            Some(self.pop())
        } else {
            None
        };
        let recv = self.pop();

        // 1. Null receiver -> deopt at this bci. The interpreter re-executes
        //    the `invoke` and the native implementation raises the NPE, with
        //    this method's own handler semantics. `Op::Cmp` widens to a 64-bit
        //    compare because one operand is `Ref`-typed, so a pointer whose
        //    low word happens to be zero is not mistaken for null.
        let null = self.aconst_null();
        let recv_nonnull = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![recv, null], pc);
        let recv_guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, recv_nonnull],
            Some(pc),
        );

        // 2. `coder` first, then `value` — the `Op::Load` helper arm is a
        //    `CALL`, and loading the reference last is the shortest window in
        //    which a live oop sits across one.
        //
        //    Both name `recv_guard` as their trailing guard token (see
        //    [`with_guard_token`]): these two reads are legal only because the
        //    guard above ran, and until the token edge existed that fact lived
        //    in nothing but their position in this function.
        let coder_index = self.iconst(layout.coder_field_index as i64);
        let coder = self.graph.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            with_guard_token(vec![ctrl, self.mem, recv, coder_index], recv_guard),
            Some(pc),
        );
        self.graph.final_java_field_reads.insert(coder);
        self.mem = coder;
        let value_index = self.iconst(layout.value_field_index as i64);
        let value = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            with_guard_token(vec![ctrl, self.mem, recv, value_index], recv_guard),
            Some(pc),
        );
        self.graph.final_java_field_reads.insert(value);
        self.mem = value;

        // 3. A real String's `value` is never null, but this expansion may not
        //    ASSUME it — the whole point of the deopt discipline is that an
        //    uncertain case leaves for the interpreter rather than reading a
        //    fabricated address. `Op::ArrayLength` and `Op::ArrayLoad` each
        //    emit their own null check as well; the explicit guard states the
        //    intent and keeps the `length` and `charAt` paths identical.
        let value_nonnull = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![value, null], pc);
        let value_guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, value_nonnull],
            Some(pc),
        );

        // 4. char_count = value.length >> coder. `Op::ArrayLength` does not
        //    advance the memory token (nothing writes an array's length), so
        //    it takes the current one as an input only. It DOES name
        //    `value_guard`: the length word it reads is at a fixed offset from
        //    a reference the guard above is what proves non-null.
        let byte_len = self.graph.add(
            Op::ArrayLength,
            IrType::Int,
            with_guard_token(vec![ctrl, self.mem, value], value_guard),
            Some(pc),
        );
        let char_count = self.add_data(Op::UShr, IrType::Int, vec![byte_len, coder], pc);

        let result = if let Some(index) = index {
            // charAt(I)C.
            //
            // Bounds: `0 <= index < char_count`, as two SIGNED compares rather
            // than one unsigned one because `Op::Cmp` has no unsigned form.
            // `char_count` is an `arraylength` shifted right and so is never
            // negative, which makes the pair exactly the unsigned test the
            // single-pass region spells `CMP; JAE`.
            let zero = self.iconst(0);
            let non_negative =
                self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![index, zero], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, non_negative],
                Some(pc),
            );
            let in_bounds =
                self.add_data(Op::Cmp(CmpOp::Lt), IrType::Int, vec![index, char_count], pc);
            let bounds_guard = self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, in_bounds],
                Some(pc),
            );

            // JIT round 21, lane charat (STRINTR21-5): one unchecked char
            // read instead of the ten nodes below. The two guards above are
            // its whole proof (`index < value.length >>> coder` puts byte
            // `index` and bytes `2 * index`, `2 * index + 1` inside `value`),
            // and it names the last one as its token like the byte loads did.
            // It does not advance `self.mem`: it writes nothing, like
            // `Op::ArrayLength` above.
            if ir_string_char_decode_enabled() {
                let ch = self.graph.add(
                    Op::StrCharDecode,
                    IrType::Int,
                    with_guard_token(vec![ctrl, self.mem, value, index, coder], bounds_guard),
                    Some(pc),
                );
                self.push(ch);
                note_string_intrinsic(kind);
                return true;
            }

            // Branchless LATIN1/UTF16 decode. See the section header for why
            // the second byte's address is `off + coder` and why both reads
            // are in bounds for every index this guard admits.
            //
            // Both element reads name `bounds_guard` — the LAST of the two
            // bounds guards — as their token. The slot holds one guard, so the
            // model it expresses is "this node may not precede that guard";
            // the `index >= 0` guard in front of it keeps its place through
            // the rule that already ordered every guard, namely that
            // `ir_schedule::priority_sort_block` leaves impure nodes in their
            // relative order. Naming either one is enough for
            // `ir_optimize::LoopGuards`, which intersects against the UNION of
            // every covering guard's cone.
            let off = self.add_data(Op::Shl, IrType::Int, vec![index, coder], pc);
            let lo_raw = self.graph.add(
                Op::ArrayLoad(MemKind::Byte),
                IrType::Int,
                with_guard_token(vec![ctrl, self.mem, value, off], bounds_guard),
                Some(pc),
            );
            self.mem = lo_raw;
            let hi_off = self.add_data(Op::Add, IrType::Int, vec![off, coder], pc);
            let hi_raw = self.graph.add(
                Op::ArrayLoad(MemKind::Byte),
                IrType::Int,
                with_guard_token(vec![ctrl, self.mem, value, hi_off], bounds_guard),
                Some(pc),
            );
            self.mem = hi_raw;
            // `baload` SIGN-extends (JVMS), and a compact String byte is an
            // unsigned half of a code unit — mask both halves before combining
            // or every byte >= 0x80 decodes as a negative char.
            let byte_mask = self.iconst(0xFF);
            let lo = self.add_data(Op::And, IrType::Int, vec![lo_raw, byte_mask], pc);
            let hi = self.add_data(Op::And, IrType::Int, vec![hi_raw, byte_mask], pc);
            let eight = self.iconst(8);
            let hi_shifted = self.add_data(Op::Shl, IrType::Int, vec![hi, eight], pc);
            // `0 - coder` is all-ones for UTF16 and zero for LATIN1, which is
            // what selects between the two decodes without a branch.
            let coder_mask = self.add_data(Op::Sub, IrType::Int, vec![zero, coder], pc);
            let hi_selected = self.add_data(Op::And, IrType::Int, vec![hi_shifted, coder_mask], pc);
            self.add_data(Op::Or, IrType::Int, vec![lo, hi_selected], pc)
        } else if kind == SI_EMITTED_IS_EMPTY {
            let zero = self.iconst(0);
            self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![char_count, zero], pc)
        } else {
            char_count
        };
        self.push(result);
        note_string_intrinsic(kind);
        true
    }

    /// The data type for a merge / loop-carried `Op::Phi`, derived from its
    /// value inputs (`inputs[0]` is the region/merge control) by folding the
    /// [`join_data_type`] lattice — see [`Graph::phi_data_type_checked`].
    ///
    /// inc 27 typed a φ `Long`/`Double` when it saw one of those and defaulted
    /// to `Int` for everything else, "to avoid perturbing `Ref`/`Float` phi
    /// handling". That default is the bug this replaces: a *reference* merge
    /// typed `Int` is invisible to `ir_lower::zero_ref_phi_slots` and to the
    /// oop-map/`StackSlotRef` classification in `frame_value_for`, so the
    /// merged oop is neither zero-initialised nor reported as a GC root, and a
    /// deopt frame rebuilt from it carries an `Int` where the interpreter
    /// expects a reference. A `Float` merge typed `Int` misresolves the same
    /// way (`StackSlot` instead of `StackSlotFloat`).
    ///
    /// Machine codegen is unaffected by the widening: φ edge copies are
    /// unconditionally 64-bit `MOV`s through RAX (`ir_lower::emit_phi_copies`
    /// switches on nothing but `Memory`/`Control`/`Void`), and every consumer
    /// picks its width from its own `node.ty`.
    ///
    /// Infallible by construction so no existing caller has to change: an
    /// untypeable merge is *reported* and falls back to [`PHI_TYPE_FALLBACK`]
    /// rather than silently answering `Int`. Callers that want the diagnosis
    /// use [`Self::phi_data_type_checked`].
    pub(in crate::ir) fn phi_data_type(&self, inputs: &[NodeId]) -> IrType {
        match self.graph.phi_data_type_checked(inputs) {
            Ok(ty) => ty,
            Err(why) => {
                report_phi_type_fallback(&why);
                PHI_TYPE_FALLBACK
            }
        }
    }

    /// [`Graph::phi_data_type_checked`] for the graph under construction: the
    /// φ type these `inputs` prove, or a description of why they prove none.
    pub fn phi_data_type_checked(&self, inputs: &[NodeId]) -> Result<IrType, String> {
        self.graph.phi_data_type_checked(inputs)
    }

    /// Re-derive a φ's type after an input was appended *after* creation.
    ///
    /// A loop-carried φ is created at the header with only its entry value(s)
    /// (the back-edge value does not exist yet — see
    /// [`Self::activate_loop_header`]), so its type is a judgement made on
    /// partial information. [`Self::patch_loop_backedge`] appends the back-edge
    /// value and then calls this so the recorded type describes the *whole*
    /// merge — e.g. a slot whose entry value was undefined (`NO_NODE`, no type
    /// information, so the φ fell back to `Int`) but whose back-edge value is a
    /// reference is retyped `Ref` and becomes a GC root.
    ///
    /// Memory/control φs are bookkeeping tokens and are left alone.
    ///
    /// When the widened input list no longer joins, the φ takes
    /// [`PHI_TYPE_FALLBACK`], exactly as a conflict at creation does, and so
    /// does every φ that merges it. It used to KEEP the entry-edge type, which
    /// was not the conservative choice when that type was `Ref`: javac reuses
    /// a slot for a dead `Object` before the loop and an `int`/`long`/`double`
    /// inside it, the header φ stayed `Ref`, and the lowering then published
    /// the slot as a GC root at every safepoint and tagged it `StackSlotRef` in
    /// deopt frames — a raw long or double handed to the collector as an oop.
    /// A conflicting merge is legal only for a slot that is dead at the header
    /// (verified bytecode cannot read it), so the fallback costs nothing.
    ///
    /// # Propagation runs on BOTH outcomes (2026-09-17)
    ///
    /// Until 2026-09-17 the dependant re-typing above lived only in the `Err`
    /// arm: a φ whose type *succeeded* in changing — the exact case this doc
    /// opens with, `Int` → `Ref` once the back edge arrived — updated itself and
    /// stopped. Any φ built between header activation and the back-edge patch
    /// that consumed the loop φ had been typed from the **old** `Int`, was
    /// never revisited, and therefore carried an oop under `IrType::Int`:
    /// excluded from `ir_lower::zero_ref_phi_slots`, excluded from
    /// `emit_safepoint_map`'s `Ref`-typed scan (so the collector neither scans
    /// nor relocates the slot), and compared at 32 bits by `ir_lower`'s
    /// `Op::Cmp` width selection. The successful join is propagated now, which
    /// is what the first paragraph always claimed happened.
    ///
    /// # Worklist, not recursion
    ///
    /// The `Err` arm used to recurse once per φ link, allocating a `Vec` of
    /// dependants at every level. A deep loop nest with many live locals makes
    /// that a stack-depth risk on a background compiler thread, where a
    /// stack overflow is a VM crash rather than a failed compile. The explicit
    /// worklist below has the same reachability and a flat frame.
    ///
    /// Only a node whose type actually **changed** enqueues its dependants, so
    /// a fixpoint is reached as soon as the join stops moving. The step budget
    /// is a backstop for a lattice that someone later makes non-monotone: it
    /// latches [`Self::build_refused`] rather than returning a half-propagated
    /// graph, because a φ whose type does not describe its inputs is precisely
    /// the GC-unsound shape this function exists to prevent.
    pub(in crate::ir) fn retype_phi(&mut self, phi: NodeId, pc: usize) {
        // Generous: one visit per φ per type change, and the lattice admits at
        // most a handful of changes per node. Anything past this is a bug in
        // the lattice, not a large method.
        let mut budget = self.graph.nodes.len().saturating_mul(4).saturating_add(64);
        let mut work = vec![phi];
        while let Some(cur) = work.pop() {
            if budget == 0 {
                self.refuse(line!(), pc);
                return;
            }
            budget -= 1;

            let current = match self.graph.node_opt(cur) {
                Some(node) => node.ty,
                None => continue,
            };
            // Memory/control φs are bookkeeping tokens and are left alone.
            if matches!(current, IrType::Memory | IrType::Control) {
                continue;
            }
            let inputs = self.graph.nodes[cur as usize].inputs.clone();
            let joined = match self.graph.phi_data_type_checked(&inputs) {
                Ok(ty) => ty,
                Err(why) => {
                    report_phi_type_fallback(&why);
                    PHI_TYPE_FALLBACK
                }
            };
            if joined == current {
                continue;
            }
            self.graph.nodes[cur as usize].ty = joined;
            // Every φ that merges this one was typed from the OLD type, on
            // either outcome. Re-derive theirs too.
            //
            // Through the def-use lists rather than an arena scan: the scan
            // was O(nodes) per CHANGED φ, and `patch_loop_backedge` retypes
            // every local φ of a loop, so a method with many loops and many
            // live locals paid nodes × φs at build time. `users_of` falls back
            // to exactly that scan when the lists are not current, so the
            // answer is the same either way. A φ's slot 0 is its merge, never
            // another φ, so "any input" and "any value input" agree here.
            for user in self.graph.users_of(cur) {
                if user != cur && self.graph.node_opt(user).is_some_and(|n| n.op == Op::Phi) {
                    work.push(user);
                }
            }
        }
    }

    /// Latch a refusal from a helper that cannot `return ir_build_bail(..)`.
    ///
    /// The FIRST refusal wins: it is the one that describes the state the
    /// builder was still consistent in, and every later one is a consequence of
    /// continuing to walk after it. See [`Self::build_refused`].
    pub(in crate::ir) fn refuse(&mut self, site: u32, pc: usize) {
        if self.build_refused.is_none() {
            self.build_refused = Some((site, pc));
        }
    }

    /// Re-lay-out the parameter locals with the JVM category-2 two-slot
    /// convention and type each `Param` node. inc 25: [`Self::new`] packs every
    /// parameter one-per-slot typed `Int`, which is only correct for an
    /// all-category-1 signature. A `long`/`double` parameter occupies **two**
    /// JVM local slots, so a following parameter's value node sits two slots
    /// higher — e.g. `(long a, long b)` puts `a` at slot 0 and `b` at slot 2,
    /// matching `lload_2` for `b`. The `Param` node *index* stays the JIT-arg
    /// index (the lowerer reads param `i` from prologue slot `(i+1)*8`, one
    /// register per parameter); only the `locals` placement and the node type
    /// change. `param_types` is in JIT-arg order (one entry per parameter).
    /// Must be called before [`Self::build`].
    pub fn set_param_types(&mut self, param_types: &[IrType]) {
        let cat2 = |t: &IrType| matches!(t, IrType::Long | IrType::Double);
        // Clear the parameter region so a stale one-per-slot placement (from
        // `new`) cannot shadow a now-two-slot-wide layout.
        let total: usize = param_types
            .iter()
            .map(|t| if cat2(t) { 2 } else { 1 })
            .sum();
        for s in 0..total.min(self.locals.len()) {
            self.locals[s] = NO_NODE;
        }
        let mut jvm_slot = 0usize;
        for (i, &ty) in param_types.iter().enumerate() {
            if let Some(pid) = self
                .graph
                .nodes
                .iter()
                .position(|n| matches!(n.op, Op::Param(idx) if idx as usize == i))
            {
                self.graph.nodes[pid].ty = ty;
                if jvm_slot < self.locals.len() {
                    self.locals[jvm_slot] = pid as NodeId;
                }
            }
            jvm_slot += if cat2(&ty) { 2 } else { 1 };
        }
    }

    /// Supply the resolved instance-field layout (`pc → (field_index,
    /// type_tag)`) the builder uses to lower `getfield` into an `Op::Load`.
    /// Must be called before [`Self::build`]; absent / non-int-category
    /// entries make the corresponding `getfield` bail to single-pass.
    pub fn set_field_info(&mut self, info: HashMap<usize, (usize, u8)>) {
        self.field_info = info;
    }

    /// Round 15 wave 3 (lane chain2, CHN15-2): supply `pc -> declaring class
    /// id` for `getfield` / `putfield` sites whose field the resolver resolved
    /// (`ResolvedField::declaring_class_id`, the class resolution FOUND the
    /// field in, not the class the constant pool names). Merged, not replaced,
    /// so the compiling method's rows and a spliced body's (combined-buffer
    /// pcs, disjoint by construction) can arrive in either order. Must be
    /// called before [`Self::build`]. Reads
    /// `CRATONVM_JIT_IR_FIELD_IDENTITY_ALIAS` once, into
    /// [`Graph::field_identity_alias`]; off, the rows only feed the census.
    pub fn set_field_decl_classes(&mut self, rows: HashMap<usize, u32>) {
        self.graph.field_identity_alias = ir_field_identity_alias_enabled();
        self.field_decl_class_by_pc.extend(rows);
    }

    /// Record the declaring class of the field `node` (built at `pc`)
    /// accesses, when the resolver gave one (round 15 wave 3, lane chain2,
    /// CHN15-2).
    pub(in crate::ir) fn note_field_decl_class(&mut self, pc: usize, node: NodeId) {
        if let Some(&decl) = self.field_decl_class_by_pc.get(&pc) {
            self.graph.field_decl_class.insert(node, decl);
        }
    }

    /// Supply the `getfield` / `putfield` pcs whose field is `volatile`. Must
    /// be called before [`Self::build`]; a field access at any of them makes
    /// `build` bail, so the method compiles single-pass.
    ///
    /// Refused rather than modelled. A volatile access has no `Op` of its own:
    /// the arms below would emit an ordinary `Op::Load(MemKind)` /
    /// `Op::Store(MemKind)`, whose memory effect is a plain read / write, so
    /// GVN may merge two reads of a volatile flag and LICM may hoist one out of
    /// a loop whose body only reads — `while (!this.stop) {}` then never sees
    /// another thread's write and spins forever — and the lowerer emits no
    /// StoreLoad fence after the store. [`MemEffect::volatile_read`] /
    /// [`MemEffect::volatile_write`] already describe the right ordering, but
    /// `MemKind` is matched in every IR pass and both backends' lowerers, so
    /// carrying the flag through them is a change to many files for a tier
    /// the single-pass backend already covers correctly: it fences every
    /// volatile `putfield` and has no field-load hoist
    /// (`volatile-instance-fields-are-plain-accesses-in-compiled-code`).
    ///
    /// The method's own pcs. A spliced callee's volatile pcs arrive through
    /// [`Self::add_spliced_volatile_field_pcs`] (round 9 wave 10); until the
    /// VM's inline resolver feeds it, that resolver keeps refusing such a
    /// callee in IR mode (`ir-splice-volatile-field`).
    ///
    /// # Modelled, behind a flag (round 9 wave 6)
    ///
    /// `CRATONVM_JIT_IR_VOLATILE_FIELDS` ([`ir_volatile_fields_enabled`],
    /// default ON since round 9 wave 7; `=0` refuses) builds the access instead, as the ordinary `Op::Load` /
    /// `Op::Store` marked [`Node::volatile_access`]. The mark gives the node
    /// [`MemEffect::volatile_read`] / [`MemEffect::volatile_write`], and every
    /// `ir_optimize` pass that moves, merges or deletes memory operations
    /// refuses to do so across or to it (redundant-load elimination, both
    /// dead-store phases, both LICM load arms); the builder's memory-token
    /// chain, which every read and write already advances, keeps the rest in
    /// program order through the scheduler. A volatile `putfield` is admitted
    /// only when [`IR_LOWER_FENCES_VOLATILE_STORES`] also says the lowerer
    /// fences it.
    pub fn set_volatile_field_pcs(&mut self, pcs: HashSet<usize>) {
        // Merged, not replaced (round 9 wave 10, irl10): a spliced callee's
        // volatile pcs may be installed first by
        // [`Self::add_spliced_volatile_field_pcs`], and the two key spaces are
        // disjoint (a spliced pc is a combined-buffer pc at or past
        // `code_len`). A builder is fresh per compile, so for the method's own
        // pcs this is the same as the assignment it replaces.
        self.volatile_field_pcs.extend(pcs);
        let admit = ir_volatile_fields_enabled();
        self.volatile_loads_admitted = admit;
        self.volatile_stores_admitted = admit && IR_LOWER_FENCES_VOLATILE_STORES;
    }

    /// Supply the volatile `getfield` / `putfield` pcs of SPLICED callees, in
    /// combined-buffer coordinates (callee pc + the site's `base`, exactly as
    /// `append_ir_inline_site` rebases the `field_info` rows). Round 9 wave 10
    /// (irl10), `perf-ir-refuses-volatile-instance-field-methods`: until now
    /// the VM's inline resolver refused every callee with a volatile instance
    /// access in IR mode (`ir-splice-volatile-field`), because nothing could
    /// tell the builder which spliced access was volatile -- it would have
    /// built an unordered `Op::Load` that LICM may hoist out of a spin loop.
    /// With these pcs the spliced access takes the same arm as the method's
    /// own: an ordered `Op::Load` / `Op::Store` marked
    /// [`Node::volatile_access`], or a bail when admission is off.
    ///
    /// Additive and order-free with respect to [`Self::set_volatile_field_pcs`]
    /// and [`Self::apply_inline_tables`]. Sets the admission bits from
    /// [`ir_volatile_fields_enabled`] the same way
    /// [`Self::set_volatile_field_pcs`] does, so a compile with no caller-side
    /// volatile resolver still models (or, with the flag off, refuses) the
    /// spliced access instead of bailing on a pc whose admission was never set.
    /// An empty iterator changes nothing.
    pub fn add_spliced_volatile_field_pcs<I: IntoIterator<Item = usize>>(&mut self, pcs: I) {
        let before = self.volatile_field_pcs.len();
        self.volatile_field_pcs.extend(pcs);
        if self.volatile_field_pcs.len() == before {
            return;
        }
        let admit = ir_volatile_fields_enabled();
        self.volatile_loads_admitted = admit;
        self.volatile_stores_admitted = admit && IR_LOWER_FENCES_VOLATILE_STORES;
    }

    /// nio21 wave 2 (lane L3): supply `getfield` pcs whose resolved field is
    /// `final` -- the method's own pcs (from
    /// `CompileRequest::cp_field_final_resolver`) or a spliced body's, rebased
    /// to combined-buffer coordinates ([`IrInlineTables::final_field_pcs`]).
    /// Additive and order-free, like [`Self::add_spliced_volatile_field_pcs`]:
    /// the two key spaces are disjoint. A pc that is also volatile is never
    /// forwarded (the memo refuses every volatile access first), so a resolver
    /// that answered both bits for one field could not make this unsound.
    pub fn add_final_field_pcs<I: IntoIterator<Item = usize>>(&mut self, pcs: I) {
        self.final_field_pcs.extend(pcs);
    }

    /// nio21 wave 7 (lane L4, W5L4-3): `getfield` pcs whose value is a
    /// `byte[]` or null (`crate::InlineSite::byte_array_field_pcs`, rebased).
    /// Additive. The node a read at one of them yields -- its `Op::Load`, or
    /// the value the field memo forwards for it (the same field, so the same
    /// type fact) -- joins [`Graph::byte_array_nodes`].
    pub fn add_byte_array_field_pcs<I: IntoIterator<Item = usize>>(&mut self, pcs: I) {
        self.byte_array_field_pcs.extend(pcs);
    }

    /// nio21 wave 7 (lane L4): record `node` as a `byte[]`-or-null value when
    /// it was read at a listed pc ([`Self::add_byte_array_field_pcs`]).
    pub(in crate::ir) fn note_byte_array_field_read(&mut self, pc: usize, node: NodeId) {
        if self.byte_array_field_pcs.contains(&pc) {
            self.graph.byte_array_nodes.insert(node);
        }
    }

    /// Test hook: set the two admission bits directly, bypassing the flag and
    /// the lowering contract, so a unit test can build and inspect the graph
    /// either way. Call AFTER [`Self::set_volatile_field_pcs`]. Not
    /// test-gated on purpose: the panic-free ratchet stops scanning a file
    /// at its first test-gate attribute line, and this item sits mid-file.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set_volatile_field_admission(&mut self, loads: bool, stores: bool) {
        self.volatile_loads_admitted = loads;
        self.volatile_stores_admitted = stores;
    }

    /// Supply the allocation layout (`pc → (class_id, num_fields)`) and the set
    /// of elidable trivial-`<init>` pcs the builder uses to lower `new` into an
    /// `Op::New` (for escape-analysis scalar replacement). Must be called before
    /// [`Self::build`]; an absent `new`/`invokespecial` pc bails to single-pass.
    pub fn set_new_info(
        &mut self,
        new_info: HashMap<usize, (u32, usize)>,
        trivial_init_pcs: HashSet<usize>,
    ) {
        self.new_info = new_info;
        self.trivial_init_pcs = trivial_init_pcs;
    }

    /// JIT round 20 wave 12 (lane qLoop, W4L3-6): record classes whose `new`
    /// resolved with no finalizer ([`Graph::finalizer_free_new_classes`]).
    /// Merged, not replaced. Must be called before [`Self::build`].
    pub fn note_finalizer_free_new_classes(&mut self, classes: impl IntoIterator<Item = u32>) {
        self.graph.finalizer_free_new_classes.extend(classes);
    }

    /// JIT round 20 wave 12 (lane hFin): record classes whose `new` resolved
    /// WITH a finalizer or reference registration
    /// ([`Graph::finalizable_new_classes`]). Merged, not replaced. Must be
    /// called before [`Self::build`].
    pub fn note_finalizable_new_classes(&mut self, classes: impl IntoIterator<Item = u32>) {
        self.graph.finalizable_new_classes.extend(classes);
    }

    /// EAGER19-1: supply the deferred `new` pcs the VM admits to an
    /// uninitialized-class uncommon trap ([`Self::uninit_new_trap_pcs`]).
    pub fn set_uninit_new_trap_pcs(&mut self, pcs: HashSet<usize>) {
        self.uninit_new_trap_pcs = pcs;
    }

    /// Interpreter round i4 wave 94, lane L2: supply the row-less field sites
    /// the VM admits to an unresolved-field uncommon trap
    /// ([`Self::field_trap_sites`]).
    pub fn set_field_trap_sites(&mut self, sites: HashMap<usize, u8>) {
        self.field_trap_sites = sites;
    }

    /// Interpreter round i4 wave 94, lane L2 (`CRATONVM_JIT_IR_FIELD_SITE_TRAP`):
    /// the field site at `pc` (opcode `op`, `0xb2..=0xb5`) has no field row.
    /// When the VM admitted it ([`Self::field_trap_sites`]: its owner was not
    /// loaded when the method compiled, so the site has never run) plant an
    /// unconditional uncommon trap there ([`TrapCause::UnresolvedField`]) and
    /// fix the abstract stack the way the access would have: pop its
    /// operands, push a typed zero for a read. Everything after the trap is
    /// unreachable at run time (see [`Self::plant_uncommon_trap`]). HotSpot
    /// C2's answer to an unloaded field holder (`Deoptimization::Reason_unloaded`).
    ///
    /// The trap's first firing is served as the uninitialized-`new` trap is
    /// (the VM records the site in the same one-shot map): the body is
    /// evicted, the interpreter runs the site (loading and initialising the
    /// owner, or throwing), and the late-bind recompile sees the owner loaded.
    /// A spent site is not admitted again, so a recompile bails as before.
    ///
    /// `false`, having changed nothing, when the site is not admitted, sits in
    /// a splice (the map is keyed by this method's own pcs), has a tag this
    /// builder refuses, or the trap is refused; the caller then bails.
    pub(in crate::ir) fn plant_field_site_trap(
        &mut self,
        code: &[u8],
        code_len: usize,
        pc: usize,
        op: u8,
    ) -> bool {
        if !self.splice.is_empty() || pc >= code_len {
            return false;
        }
        let Some(&tag) = self.field_trap_sites.get(&pc) else {
            return false;
        };
        let Some(ty) = self.field_access_type(tag) else {
            self.note_field_site_trap_census(pc, "refused (field type gate)");
            return false;
        };
        if !self.plant_uncommon_trap(code, code_len, pc, TrapCause::UnresolvedField) {
            // The replay rule (`trap_replay_is_safe`), the site-trap switch or
            // no control: the method bails to single-pass.
            self.note_field_site_trap_census(pc, "refused (plant_uncommon_trap)");
            return false;
        }
        self.note_field_site_trap_census(pc, "planted");
        match op {
            // getstatic: no operand.
            0xb2 => {}
            // putstatic: the value; getfield: the objectref.
            0xb3 | 0xb4 => {
                let _ = self.pop();
            }
            // putfield: the value, then the objectref.
            _ => {
                let _ = self.pop();
                let _ = self.pop();
            }
        }
        if matches!(op, 0xb2 | 0xb4) {
            let zero = match ty {
                // Wave 8b: fresh after an in-place trap (`trap_placeholder_ref`).
                IrType::Ref => self.trap_placeholder_ref(),
                IrType::Long => self.lconst(0),
                IrType::Float => self.fconst(0.0),
                IrType::Double => self.dconst(0.0),
                _ => self.iconst(0),
            };
            self.push(zero);
        }
        true
    }

    /// The refusal of a row-less field site at `pc` (opcode `op`) that
    /// [`Self::plant_field_site_trap`] did not trap: [`ir_build_bail`], and,
    /// for a site of the method's own code (no splice open), the fall-through
    /// cause `crate::IrFallThroughCause::RowlessFieldSite` the background
    /// worker reads (interpreter round i6 wave 115, lane L2,
    /// `i100-L2-proposal-fall-through-carries-its-bail-site`). A spliced
    /// callee's row-less site is not classified: the splice's caller may
    /// rebuild without the splice, and its pcs are not this method's.
    #[cold]
    #[inline(never)]
    pub(in crate::ir) fn rowless_field_site_bail<T>(
        &self,
        site: u32,
        pc: usize,
        op: u8,
    ) -> Option<T> {
        if self.splice.is_empty() {
            crate::note_ir_rowless_field_site_bail(pc, op);
        }
        ir_build_bail(site, pc)
    }

    /// Interpreter round i4 wave 95, lane L2: the builder's half of the
    /// unresolved-field trap census (`CRATONVM_DBG_JITC=1`), one line per
    /// ADMITTED site: `planted` or why not. The VM's admission line
    /// (`unresolved-field TRAP admitted ... door=`) and `lib.rs`'s `ir
    /// field-site traps admitted` say the site was offered; this says whether
    /// the optimizing body kept it, which is what wave 94 could not tell.
    #[cold]
    #[inline(never)]
    pub(in crate::ir) fn note_field_site_trap_census(&self, pc: usize, what: &str) {
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir field-site trap {what} at pc={pc} in {}",
                self.method_label.as_deref().unwrap_or("<method>"),
            );
        }
    }

    /// EAGER2-3: supply the `new` pcs the VM admits to a class-initialized
    /// guard ([`Self::uninit_new_guard_sites`]), each with its class's
    /// init-word address and initialized value.
    pub fn set_uninit_new_guard_sites(&mut self, sites: HashMap<usize, (usize, u8)>) {
        self.uninit_new_guard_sites = sites;
    }

    /// Supply the `invokespecial java/lang/Object.<init>()V` pcs — see
    /// [`Self::object_init_pcs`]. Independent of [`Self::set_new_info`]: the
    /// site that matters is the `super()` call inside a constructor, and a
    /// constructor contains no `new`.
    pub fn set_object_init_pcs(&mut self, pcs: HashSet<usize>) {
        self.object_init_pcs = pcs;
    }

    /// Supply this method's unresolved call sites, `pc -> cell` (see
    /// [`Self::first_call_sites`]; interpreter round i1 wave 50, lane L1).
    pub fn set_first_call_sites(&mut self, sites: HashMap<usize, usize>) {
        self.first_call_sites = sites;
    }

    /// JIT round 19 wave 5 (lane handler5, SPLICE4-1): supply this method's
    /// exception table, in table order, with every catch type named (the
    /// caller stages nothing when one does not resolve), and the declaring
    /// class the names resolve in. An empty table (the default) builds no
    /// compiled handler entry; [`Self::plan_catch_arms`] decides the rest.
    pub fn set_catch_arm_table(&mut self, rows: Vec<CatchArmRow>, declaring_class_id: u32) {
        self.catch_arm_rows = rows;
        self.catch_arm_class_id = declaring_class_id;
    }

    /// SPLICE4-1: did the plan admit at least one compiled handler entry?
    /// Read by `lib.rs` after a refused build, to retry once without them.
    pub fn catch_arms_planned(&self) -> bool {
        !self.catch_arm_calls.is_empty()
    }

    /// SPLICE4-1: decide, before the walk, which `invoke`s get a compiled
    /// handler entry ([`Self::note_catch_arm`] builds it).
    ///
    /// The simplest sound shape, and only it:
    ///
    /// * the walk is the plain pc walk (no rotated-loop block walk) and the
    ///   `invoke` is normally reachable code of the method itself;
    /// * the row is the FIRST row of the table, in table order, covering the
    ///   `invoke` (JVMS 2.10: the first match wins; the arm's lookup asks that
    ///   row alone, and a throwable it does not catch keeps today's exit,
    ///   which routes it through the whole table);
    /// * the handler lies past its range and past the `invoke` (a forward
    ///   exception edge, javac's shape, so no replay extent and no pc-walk
    ///   order changes), and only exception edges reach it;
    /// * the handler's region -- what normal flow reaches from it before it
    ///   rejoins normally reachable code -- has no loop header, no `jsr`/`ret`,
    ///   rejoins only at a merge the walk activates, and shares no pc with
    ///   another admitted handler's region.
    ///
    /// Under `CRATONVM_JIT_IR_CATCH_ARM_LADDER` (JIT round 19 wave 6, lane
    /// handler6, HANDLER19-3) the second clause widens to the longest
    /// table-order PREFIX of covering rows whose handlers all pass the last
    /// two, and the invokes of every admitted region are planned the same way
    /// (until no new region is admitted).
    ///
    /// Everything else keeps the exit it has today.
    pub(in crate::ir) fn plan_catch_arms(
        &mut self,
        code: &[u8],
        code_len: usize,
        verified: &cratonvm_reader::VerifiedCode,
        reachable: &HashSet<usize>,
    ) {
        self.catch_arm_calls.clear();
        self.catch_arm_handlers.clear();
        self.catch_arm_walkable.clear();
        if self.catch_arm_rows.is_empty() || self.block_walk {
            return;
        }
        let merge_targets: HashSet<usize> = verified
            .merge_targets()
            .iter()
            .map(|&t| t as usize)
            .collect();
        let headers: HashSet<usize> = verified
            .loop_headers()
            .iter()
            .map(|&h| h as usize)
            .collect();
        let mut refused: HashSet<usize> = HashSet::new();
        let mut claimed: HashSet<usize> = HashSet::new();
        let mut invokes: Vec<usize> = reachable
            .iter()
            .copied()
            .filter(|&p| p < code_len && matches!(code.get(p), Some(0xb6..=0xb9)))
            .collect();
        invokes.sort_unstable();
        // JIT round 19 wave 6 (lane handler6, HANDLER19-3): under the ladder
        // switch, every covering row of the prefix, and (below) the calls of
        // the admitted regions themselves. Off, the first covering row alone,
        // exactly as wave 5 planned it.
        let ladder = ir_catch_arm_ladder_enabled();
        let mut planned_regions: HashSet<usize> = HashSet::new();
        loop {
            for pc in std::mem::take(&mut invokes) {
                if self.catch_arm_calls.contains_key(&pc) {
                    continue;
                }
                let covering: Vec<usize> = self
                    .catch_arm_rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.start_pc <= pc && pc < r.end_pc)
                    .map(|(i, _)| i)
                    .collect();
                let limit = if ladder { covering.len() } else { 1 };
                let mut prefix: Vec<usize> = Vec::new();
                for &row_idx in covering.iter().take(limit) {
                    let (end_pc, handler) = {
                        let row = &self.catch_arm_rows[row_idx];
                        (row.end_pc, row.handler_pc)
                    };
                    if handler < end_pc
                        || handler <= pc
                        || handler >= code_len
                        || reachable.contains(&handler)
                        || refused.contains(&handler)
                    {
                        break;
                    }
                    if !self.catch_arm_handlers.contains_key(&handler) {
                        let region = catch_handler_region(
                            code,
                            code_len,
                            verified,
                            reachable,
                            &merge_targets,
                            &headers,
                            &self.merges,
                            handler,
                        );
                        match region {
                            Some(region) if region.pcs.iter().all(|p| !claimed.contains(p)) => {
                                claimed.extend(region.pcs.iter().copied());
                                self.catch_arm_handlers.insert(handler, region);
                            }
                            _ => {
                                refused.insert(handler);
                                break;
                            }
                        }
                    }
                    prefix.push(row_idx);
                }
                if !prefix.is_empty() {
                    self.catch_arm_calls.insert(pc, prefix);
                }
            }
            if !ladder {
                break;
            }
            // The calls inside admitted regions not planned yet. A region is
            // walked only once an arm opens it, so planning a call no walk
            // reaches costs nothing; every arm it gets is a forward edge into
            // another, disjoint region (the checks above), so this ends.
            let mut more: Vec<usize> = Vec::new();
            for (&handler, region) in &self.catch_arm_handlers {
                if !planned_regions.insert(handler) {
                    continue;
                }
                more.extend(
                    region
                        .pcs
                        .iter()
                        .copied()
                        .filter(|&p| p < code_len && matches!(code.get(p), Some(0xb6..=0xb9))),
                );
            }
            if more.is_empty() {
                break;
            }
            more.sort_unstable();
            invokes = more;
        }
        if crate::x64::dbg_jitc_enabled() && !self.catch_arm_calls.is_empty() {
            let mut calls: Vec<(usize, Vec<usize>)> = self
                .catch_arm_calls
                .iter()
                .map(|(&pc, rows)| {
                    (
                        pc,
                        rows.iter()
                            .map(|&r| self.catch_arm_rows[r].handler_pc)
                            .collect(),
                    )
                })
                .collect();
            calls.sort_unstable();
            if ladder {
                eprintln!(
                    "[cratonvm-jitc] ir-catch-arm plan {} calls={calls:?} refused-handlers={:?}",
                    self.graph.scope_method_key, refused
                );
            } else {
                // Wave 5's line, unchanged: one handler per call.
                let calls: Vec<(usize, usize)> = calls
                    .iter()
                    .filter_map(|(pc, hs)| hs.first().map(|&h| (*pc, h)))
                    .collect();
                eprintln!(
                    "[cratonvm-jitc] ir-catch-arm plan {} calls={calls:?} refused-handlers={:?}",
                    self.graph.scope_method_key, refused
                );
            }
        }
    }

    /// The in-frame handler edges of the call `call` at `pc`: nio21 wave 3
    /// L2's call-site exception edges ([`Self::plant_call_throw_edges`],
    /// `CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES`) when they armed the
    /// call, else JIT round 19 wave 5's compiled handler arm
    /// ([`Self::note_catch_arm`], `CRATONVM_JIT_IR_COMPILED_HANDLER_ENTRY`).
    /// Never both on one call: each hangs its own flag `If` off the call's
    /// control, and the lowering pairs one exception pad with one of them.
    pub(in crate::ir) fn plant_call_handler_entry(
        &mut self,
        pc: usize,
        call: NodeId,
        code_len: usize,
    ) {
        let before = self.graph.call_throw_sites.len();
        self.plant_call_throw_edges(pc, call, code_len);
        if self.graph.call_throw_sites.len() == before {
            self.note_catch_arm(pc, call);
        }
    }

    /// SPLICE4-1: hang the compiled handler entry off the plain call `call`
    /// just built at `pc`, when [`Self::plan_catch_arms`] admitted the site.
    ///
    /// ```text
    /// call ── If(ctrl, CatchEntryFlag) ── Proj(1) ── (the walk continues)
    ///                                  └─ Proj(0) ── handler merge:
    ///                                       locals = the locals at the call,
    ///                                       stack  = [CaughtException],
    ///                                       memory = the call
    /// ```
    ///
    /// The arm records the locals the call left (an `invoke` writes no local),
    /// which is what the interpreter's handler frame holds at the throw, and an
    /// operand stack of the throwable alone (JVMS 2.10). No monitor may be
    /// held: a held monitor would need the handler's release accounted against
    /// the compiled code's own exit releases, and that is not this shape.
    pub(in crate::ir) fn note_catch_arm(&mut self, pc: usize, call: NodeId) {
        if self.catch_arm_calls.is_empty() || !self.splice.is_empty() {
            return;
        }
        let Some(prefix) = self.catch_arm_calls.get(&pc).cloned() else {
            return;
        };
        if !self.monitors.is_empty() {
            return;
        }
        let Some(mut ctrl) = self.ctrl_opt() else {
            return;
        };
        let Ok(call_bci) = u32::try_from(pc) else {
            return;
        };
        // Every site index this call can take must fit before any region
        // opens: an opened region with no arm would leave input-less merges.
        let first_site = self.graph.catch_arm_sites.len();
        if u16::try_from(first_site + prefix.len()).is_err() {
            return;
        }
        // The prefix's rows in table order, and its distinct handlers in
        // first-appearance order (javac's multi-catch is several rows naming
        // one handler: one arm serves them all). A handler whose region does
        // not open ends the prefix there, so the lookup never matches a row
        // past an unarmed one (it would take the throwable for a row no arm
        // can enter). Wave 5's shape is a one-row prefix.
        let mut candidates: Vec<(&'static str, usize)> = Vec::with_capacity(prefix.len());
        let mut handlers: Vec<(usize, &'static str)> = Vec::new();
        for row_idx in prefix {
            let Some(row) = self.catch_arm_rows.get(row_idx).cloned() else {
                break;
            };
            if !handlers.iter().any(|&(h, _)| h == row.handler_pc) {
                if !self.open_catch_region(row.handler_pc) {
                    break;
                }
                handlers.push((row.handler_pc, row.catch_name));
            }
            candidates.push((row.catch_name, row.handler_pc));
        }
        if handlers.is_empty() {
            return;
        }
        let saved_stack = std::mem::take(&mut self.stack);
        let saved_mem = self.mem;
        for (k, &(handler_pc, catch_name)) in handlers.iter().enumerate() {
            let site = (first_site + k) as u16;
            self.graph.catch_arm_sites.push(CatchArmSite {
                call_bci: pc,
                handler_pc,
                catch_name,
                declaring_class_id: self.catch_arm_class_id,
                candidates: candidates.clone(),
            });
            // No bytecode pc on the flag, the `If` or its projections, as on
            // the opaque OSR arm: none of them is a bytecode of its own. A
            // ladder chains its `If`s on the normal projections, so every arm
            // sees the call's state and the walk continues after the last.
            let flag = self.graph.add(
                Op::CatchEntryFlag { call_bci, site },
                IrType::Int,
                vec![ctrl],
                None,
            );
            let iff = self
                .graph
                .add(Op::If, IrType::Control, vec![ctrl, flag], None);
            // The normal projection first, so the arm is the later block in
            // creation order as well.
            let normal = self
                .graph
                .add(Op::Proj(1), IrType::Control, vec![iff], None);
            let arm = self
                .graph
                .add(Op::Proj(0), IrType::Control, vec![iff], None);
            let exc = self
                .graph
                .add(Op::CaughtException, IrType::Ref, vec![arm], None);
            self.stack = vec![exc];
            self.ctrl = arm;
            self.mem = call;
            self.add_merge_predecessor(handler_pc);
            ctrl = normal;
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-catch-arm {} call pc={pc} -> handler pc={handler_pc} catch={:?}{}",
                    self.graph.scope_method_key,
                    if catch_name.is_empty() {
                        "<any>"
                    } else {
                        catch_name
                    },
                    if candidates.len() > 1 {
                        format!(" ladder={}/{}", k + 1, handlers.len())
                    } else {
                        String::new()
                    },
                );
            }
        }
        self.stack = saved_stack;
        self.mem = saved_mem;
        self.ctrl = ctrl;
    }

    /// SPLICE4-1: make the region of the admitted handler at `handler_pc`
    /// walkable, creating its merges, the first time an arm enters it. A
    /// region no arm enters stays skipped, exactly as today, so a plan whose
    /// calls all ended up spliced or elided leaves no input-less merge.
    pub(in crate::ir) fn open_catch_region(&mut self, handler_pc: usize) -> bool {
        let (pcs, merges) = match self.catch_arm_handlers.get_mut(&handler_pc) {
            Some(region) if region.opened => return true,
            Some(region) => {
                region.opened = true;
                (region.pcs.clone(), region.merges.clone())
            }
            None => return false,
        };
        self.ensure_merge(handler_pc);
        for m in merges {
            self.ensure_merge(m);
        }
        self.catch_arm_walkable.extend(pcs);
        true
    }

    /// The first-call cell of the invoke at `pc` when it is a call site of
    /// THIS method the compile door found unresolved: never inside a spliced
    /// body, whose pcs are not the caller's.
    pub(in crate::ir) fn first_call_cell_at(&self, pc: usize) -> Option<usize> {
        if self.first_call_sites.is_empty() || !self.splice.is_empty() {
            return None;
        }
        self.first_call_sites.get(&pc).copied()
    }

    /// Record that the `Op::Call` built for `info_ptr` at `pc` guards its
    /// first-call cell, if the site has one ([`Graph::first_call_cells`]).
    pub(in crate::ir) fn note_first_call(&mut self, pc: usize, info_ptr: usize) {
        if let Some(cell) = self.first_call_cell_at(pc) {
            if !self.graph.first_call_cells.contains(&(info_ptr, cell)) {
                self.graph.first_call_cells.push((info_ptr, cell));
            }
        }
    }

    /// cov-06: supply the resolved allocation layout (`pc → component_class_id`)
    /// for `anewarray` sites the builder lowers into `Op::NewArray`. Must be
    /// called before [`Self::build`]; an absent `anewarray` pc bails to
    /// single-pass — see [`Self::anewarray_info`].
    pub fn set_anewarray_info(&mut self, info: HashMap<usize, u32>) {
        self.anewarray_info = info;
    }

    /// Round 16 (lane matrix): supply the resolved `multianewarray` sites
    /// (`pc → packed site word`, see [`Op::MultiNewArray`]). Only a compile
    /// door that also wires `multianewarray_n` into the lowering may set this
    /// (`ir_lower` refuses an `Op::MultiNewArray` whose helper is `0`).
    pub fn set_multianewarray_sites(&mut self, sites: HashMap<usize, i64>) {
        self.multianewarray_sites = sites;
    }

    /// nio21 wave 15 (lane L3): supply the deferred `new` sites
    /// (`pc -> (holder_class_id, cp_idx)`) the `0xbb` arm builds as
    /// [`Op::NewCp`] instead of refusing the method. The caller passes only
    /// sites of the compiling method's own code, and only with
    /// `helpers.new_object_cp` wired.
    pub fn set_new_deferred_sites(&mut self, sites: HashMap<usize, (u32, u16)>) {
        self.new_deferred_sites = sites;
    }

    /// Record the call sites the planner will lower as an inline unboxing
    /// accessor. Keyed by caller pc, exactly like [`Self::set_invoke_info`],
    /// and deliberately a SEPARATE map: a site here has no `JitInvokeInfo` box
    /// and never will.
    pub fn set_unbox_intrinsics(&mut self, sites: HashMap<usize, (UnboxOp, u32)>) {
        self.unbox_intrinsics = sites;
    }

    /// JIT r23 wave 9 (lane L4): the `GlobalSession$HeapSession` class id of
    /// this compile's VM ([`Graph::ffm_heap_session_class`]; `0` = unknown).
    pub fn set_ffm_unreached_sites(
        &mut self,
        sites: HashMap<usize, crate::ffm_element_inline::FfmElementAccessor>,
    ) {
        self.ffm_unreached_sites = sites;
    }

    /// JIT r23 wave 12 (lane L7, `CRATONVM_JIT_IR_UNREACHED_CALL_TRAP`): the
    /// never-reached call-site intrinsics ([`Self::unreached_call_sites`]).
    /// Never calling it builds what every earlier build built.
    pub fn set_unreached_call_sites(&mut self, sites: HashMap<usize, (usize, u8)>) {
        self.unreached_call_sites = sites;
    }

    /// JIT r23 wave 11 (lane L4): an element site the planner found never
    /// reached (`lib.rs` `ir_ffm_element_unreached_site`) becomes an
    /// always-failing guard at its own bci, listed in
    /// [`Graph::unstable_if_trap_pcs`]: its deopt is an `UnstableIf`, which
    /// the VM withdraws after ONE charge and recompiles; by then the
    /// interpreter has run the site and its receiver profile exists, so the
    /// recompile plans the inline expansion. HotSpot's `Reason_unreached`.
    ///
    /// The guard reads nothing and the frame at `pc` is the call's own (the
    /// operands still on the stack), so the interpreter re-executes exactly
    /// the `invokeinterface`. What follows is unreachable at run time and is
    /// built only to keep the walk consistent: the operands are popped and a
    /// zero of the result type pushed, as [`Self::plant_uncommon_trap`]'s
    /// callers do.
    ///
    /// `None`: not such a site. `Some(true)`: planted. `Some(false)`: the
    /// site is one, but no trap could be planted (inside a splice, no
    /// control, no frame at `pc`, a short stack): the caller refuses the
    /// method, the route it had before.
    ///
    /// JIT r23 wave 12 (lane L7, `CRATONVM_JIT_IR_UNREACHED_CALL_TRAP`): also
    /// a generic never-reached call-site intrinsic
    /// ([`Self::unreached_call_sites`]), whose stack effect the planner
    /// recorded from its descriptor; the same guard, the same pops, and a
    /// zero of its return type.
    pub(in crate::ir) fn try_emit_ffm_unreached_trap(&mut self, pc: usize) -> Option<bool> {
        // `(operand entries, result tag)`. The element accessor's are exactly
        // what this function always popped and pushed (an `int` zero for every
        // carrier but `long`).
        let (n, ret) = match self.ffm_unreached_sites.get(&pc) {
            Some(acc) if acc.is_put => (4, b'V'),
            Some(acc) => (3, if acc.carrier == b'J' { b'J' } else { b'I' }),
            None => *self.unreached_call_sites.get(&pc)?,
        };
        if !self.splice.is_empty() {
            return None;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return Some(false);
        };
        if !self.has_frame_state_at(pc) {
            return Some(false);
        }
        if self.stack.len() < n {
            return Some(false);
        }
        let zero = self.iconst(0);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, zero],
            Some(pc),
        );
        if !self.graph.unstable_if_trap_pcs.contains(&pc) {
            self.graph.unstable_if_trap_pcs.push(pc);
        }
        for _ in 0..n {
            self.pop();
        }
        if ret != b'V' {
            let v = match ret {
                b'J' => self.lconst(0),
                b'F' => self.fconst(0.0),
                b'D' => self.dconst(0.0),
                b'L' | b'[' => self.aconst_null(),
                _ => self.iconst(0),
            };
            self.push(v);
        }
        if ir_bail_reporting() {
            eprintln!(
                "[ir] ffm element site UNREACHED-TRAP planted at bytecode pc {pc} in {} -- \
                 the first arrival deopts (UnstableIf) and the recompile profiles it",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        Some(true)
    }

    pub fn set_ffm_heap_session_class(&mut self, class_id: u32) {
        self.graph.ffm_heap_session_class = class_id;
    }

    /// JIT r23 wave 4 (lane L8): the `MemorySegment` element sites to expand
    /// inline. Unlike [`Self::set_unbox_intrinsics`], each site KEEPS its
    /// `invoke_info` row: a decline leaves the call (bound or dispatched), or
    /// refuses the method when the site says `bail_on_decline`.
    pub fn set_ffm_inline_sites(
        &mut self,
        sites: HashMap<usize, crate::ffm_element_inline::FfmElementInlineSite>,
    ) {
        self.ffm_inline_sites = sites;
    }

    /// JIT round 24 (`CRATONVM_JIT_IR_FFM_ELEMENT_BIMORPHIC`): the root
    /// element sites planned bimorphic, `pc -> the second class's shape`
    /// (the first is the site's own, [`Self::set_ffm_inline_sites`]).
    pub fn set_ffm_bimorphic_seconds(
        &mut self,
        seconds: HashMap<usize, crate::ffm_element_inline::FfmSegmentShape>,
    ) {
        self.ffm_bimorphic_seconds = seconds;
    }

    /// JIT round 24 B3: add the element sites of the spliced bodies (combined
    /// pcs, disjoint from every root pc) beside the root ones.
    pub fn add_ffm_inline_sites(
        &mut self,
        sites: HashMap<usize, crate::ffm_element_inline::FfmElementInlineSite>,
    ) {
        self.ffm_inline_sites.extend(sites);
    }

    /// JIT r23 wave 4 (lane L8): the layout facts of this method's
    /// `getstatic`s of filed `static final` `ValueLayout`s.
    pub fn add_ffm_layout_facts(
        &mut self,
        facts: impl IntoIterator<Item = ((u32, u32), crate::ffm_element_inline::FfmLayoutFacts)>,
    ) {
        self.ffm_layout_facts.extend(facts);
    }

    /// nio21 wave 1 L3: admit a `ScopedMemoryAccess` fold whose base is the
    /// literal null (see [`Self::scoped_offheap_fold`]). The compile door
    /// passes `ir_lower::scoped_arena_jit_table(helpers).is_some()`, the same
    /// predicate the lowering asks, so a site admitted here is never lowered
    /// without its arena arm.
    pub fn set_scoped_offheap_fold(&mut self, on: bool) {
        self.scoped_offheap_fold = on;
    }

    /// Register constant `VarHandle` fields (see [`Self::vh_field_plans`]).
    /// Merged, not replaced: the compiling method's own `getstatic` sites and
    /// every spliced body's contribute.
    pub fn add_vh_field_plans(
        &mut self,
        plans: impl IntoIterator<Item = ((u32, u32), crate::StaticVarHandleField)>,
    ) {
        self.vh_field_plans.extend(plans);
    }

    /// Record the call sites the planner will lower as arithmetic. Same
    /// contract as [`Self::set_unbox_intrinsics`]; see
    /// [`Self::scalar_intrinsics`].
    pub fn set_scalar_intrinsics(&mut self, sites: HashMap<usize, ScalarOp>) {
        self.scalar_intrinsics = sites;
    }

    /// nio21 wave 1, lane L5 (L3W6-1): ADD scalar-intrinsic sites at
    /// combined-buffer pcs of spliced bodies (`lib.rs`
    /// `ir_plan_spliced_scalar_intrinsics`), after the own sites were set.
    /// Merged, never replacing: a combined pc of a spliced body is never one
    /// of the compiling method's own pcs. The site's interned call row stays
    /// in `invoke_info`, unused: [`Self::try_emit_scalar_intrinsic`] is asked
    /// first at every invoke.
    pub fn add_scalar_intrinsics(&mut self, sites: impl IntoIterator<Item = (usize, ScalarOp)>) {
        self.scalar_intrinsics.extend(sites);
    }

    /// Record the `invokedynamic` sites this tier may replace with a trap.
    /// See [`IrBuilder::indy_trap_sites`].
    pub fn set_indy_trap_sites(&mut self, sites: HashMap<usize, (usize, u8)>) {
        self.indy_trap_sites = sites;
    }

    /// Round 16 (lane indy, SY4-1): record the `invokedynamic` sites this
    /// tier lowers to an [`Op::IndyBridge`] call, `pc -> bridge-site pointer`.
    /// A site in here must ALSO have its stack effect in
    /// [`Self::set_indy_trap_sites`]; one without it is still refused. Only
    /// the compile door that also wires `DirectHelperTable::indy_bridge` into
    /// the lowering may set this (`ir_lower` refuses an `Op::IndyBridge`
    /// whose helper is `0`). See [`IrBuilder::indy_bridge_sites`].
    pub fn set_indy_bridge_sites(&mut self, sites: HashMap<usize, usize>) {
        self.indy_bridge_sites = sites;
    }

    /// t1 wave 7, lane NBB (TLOCAL21-2, port of jr21 lambdatrap `05f65c02d5`):
    /// ADD bridged sites to the ones [`Self::set_indy_bridge_sites`] recorded,
    /// keeping any row already there. Same contract: each pc must also have
    /// its stack effect in [`Self::set_indy_trap_sites`].
    ///
    /// t1 wave 8, lane LAMBDATRAP: the only caller is the published-word
    /// bridge block of `ir_tier_attempt`, so every pc added here is also
    /// recorded as a published-word site (`indy_published_word_pcs`), which
    /// the refusal-only plan keeps bridged.
    ///
    /// t1 wave 11, lane JLAM: the late-lambda bridge block
    /// ([`ir_indy_late_lambda_bridge_enabled`]) is the second caller. Its
    /// sites are the same instructions before their word is published (the
    /// bridge publishes it), so they are kept bridged for the same reason.
    pub fn add_indy_bridge_sites(&mut self, sites: impl IntoIterator<Item = (usize, usize)>) {
        for (pc, site) in sites {
            self.indy_bridge_sites.entry(pc).or_insert(site);
            self.indy_published_word_pcs.insert(pc);
        }
    }

    /// JIT round 22 (lane throws, churn-loop J1): does this build already
    /// carry `invokedynamic` stack shapes (from any door block)? Asked by the
    /// compile door BEFORE the bridge block, to tell the refusal-only plan
    /// whether the default arm would have trapped or bailed.
    pub fn has_indy_trap_sites(&self) -> bool {
        !self.indy_trap_sites.is_empty()
    }

    /// JIT round 22 (lane throws, churn-loop J1;
    /// [`ir_indy_bridge_on_trap_refusal_enabled`]): the bridge sites set by
    /// [`Self::set_indy_bridge_sites`] are a fallback only -- used at a site
    /// whose trap the default arm would not plant (`default_had_shapes ==
    /// false`, or the replay guard refuses it). See
    /// [`Self::indy_bridge_refusal_only`].
    pub fn set_indy_bridge_refusal_only(&mut self, refusal_only: bool, default_had_shapes: bool) {
        self.indy_bridge_refusal_only = refusal_only;
        self.indy_default_trap_shapes = default_had_shapes;
    }

    /// Would the default `0xba` arm plant its uncommon trap at `pc` (rather
    /// than bail the build)? The same gates `plant_uncommon_trap` asks first,
    /// without planting or counting anything: the trap switch, the shapes
    /// the default door gave, and the replay guard. Asked only under
    /// [`Self::indy_bridge_refusal_only`].
    pub(in crate::ir) fn indy_default_trap_would_plant(
        &self,
        code: &[u8],
        code_len: usize,
        pc: usize,
    ) -> bool {
        self.indy_default_trap_shapes
            && ir_site_trap_enabled()
            && (!ir_trap_replay_guard_enabled() || self.trap_resumable_here(code, code_len, pc))
    }

    /// nio21 wave 11, lane L3 (W10L3-1): the door wired the bridge helper
    /// (`DirectHelperTable::indy_bridge != 0`), so a spliced constant-word
    /// lambda site may build as its `Op::IndyBridge` (whose cold path calls
    /// it). `false` (the default, and every synthetic table) refuses the build
    /// at such a site rather than planting a trap inside the splice.
    pub fn set_spliced_indy_wired(&mut self, wired: bool) {
        self.spliced_indy_wired = wired;
    }

    /// JIT round 16 wave 10 (lane indyir, IN16-8): record the `invokedynamic`
    /// sites the door planned as a plain call, `pc -> 0xb8 | 0xb6`. See
    /// [`IrBuilder::indy_direct_sites`].
    pub fn set_indy_direct_sites(&mut self, sites: HashMap<usize, u8>) {
        self.indy_direct_sites = sites;
    }

    /// JIT round 20 wave 11 (lane pIndy, SPLICE19-1): record the
    /// `invokedynamic` sites of a mutable call site the door planned as a
    /// guarded plain call, `pc -> guard`. Each pc must also have its plain
    /// call's `invoke_info` row (the plan joins it to the invoke plan) and its
    /// stack effect in [`Self::set_indy_trap_sites`]; one without either keeps
    /// the `0xba` arm's answer. See [`IrBuilder::try_indy_mutable_guarded_call`].
    pub fn set_indy_mutable_guards(&mut self, guards: HashMap<usize, IndyMutableGuard>) {
        self.indy_mutable_guards = guards;
    }

    /// JIT round 20 wave 11 (lane pIndy, W10L3-3 / W11L3-3): record the
    /// method's own non-capturing lambda sites whose singleton slot is
    /// published, `pc -> (bridge site, slot address)`. Each pc must also have
    /// its stack effect in [`Self::set_indy_trap_sites`]. See
    /// [`IrBuilder::try_indy_lambda_word`].
    pub fn set_indy_lambda_words(&mut self, words: HashMap<usize, (usize, usize)>) {
        self.indy_lambda_words = words;
    }

    /// JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2): record the
    /// method's own linked capturing lambda sites, `pc -> what to allocate`.
    /// Each pc must also have its stack effect in [`Self::set_indy_trap_sites`].
    /// See [`IrBuilder::try_indy_capture_new`].
    pub fn set_indy_capture_news(&mut self, sites: HashMap<usize, IndyCaptureNew>) {
        self.indy_capture_news = sites;
    }

    /// JIT round 21 wave 17 (lane records, IN16-6): record the method's record
    /// `ObjectMethods` sites planned as a field-wise fold, `pc -> fold`. Each
    /// pc must also have its stack effect in [`Self::set_indy_trap_sites`] and
    /// its `Objects` call row in the invoke plan. See
    /// [`IrBuilder::try_indy_record_fold`].
    pub fn set_indy_record_folds(&mut self, folds: HashMap<usize, IndyRecordFold>) {
        self.indy_record_folds = folds;
    }

    /// JIT round 21 wave 20 (lane sbfuse, STRINTR21-4): record the method's
    /// `StringBuilder` chains planned as one concat, `new pc -> fusion`
    /// (`lib.rs::ir_sb_chain_fusion_plan`). See [`Self::sb_chain_fuse_step`].
    pub fn set_sb_chain_fusions(
        &mut self,
        fusions: HashMap<usize, crate::sb_chain_fuse::SbChainFusion>,
    ) {
        self.sb_chain_fusions = fusions;
    }

    /// JIT round 21 (lane lambda2, TLOCAL21-1 / LAMBDATRAP21-3): record the
    /// SAM calls the door planned as an `invokestatic` of the lambda's impl,
    /// `invoke pc -> (invokedynamic pc, capture count, impl parameter
    /// count)`; each pc's `invoke_info` row is the impl's. See
    /// [`IrBuilder::lambda_impl_opcode`].
    pub fn set_lambda_impl_sites(&mut self, sites: HashMap<usize, (usize, usize, usize)>) {
        self.lambda_impl_sites = sites;
    }

    /// JIT round 21 (lane lambda3): a capturing planned SAM call is built
    /// only as a splice of its impl (see the field).
    pub fn set_lambda_impl_captures_need_splice(&mut self, on: bool) {
        self.lambda_impl_captures_need_splice = on;
    }

    /// JIT round 16 wave 17 (lane mh7): record the constant-handle sites the
    /// door planned as an `invokestatic`, `invoke pc -> getstatic pc`. See
    /// [`IrBuilder::mh_constant_sites`].
    pub fn set_mh_constant_sites(&mut self, sites: HashMap<usize, usize>) {
        self.mh_constant_sites = sites;
    }

    /// JIT round 21 wave 19 (lane mhbind2; MHBIND21-1): record which of the
    /// constant-handle sites are a bound handle's, `invoke pc -> argL1 slot`.
    /// See [`IrBuilder::mh_constant_bound_sites`].
    pub fn set_mh_constant_bound_sites(&mut self, sites: HashMap<usize, u32>) {
        self.mh_constant_bound_sites = sites;
    }

    /// j27 wave 6 (lane T1LAM2): record which bound constant-handle sites are
    /// a STATIC member's. See [`IrBuilder::mh_constant_bound_static_sites`].
    pub fn set_mh_constant_bound_static_sites(&mut self, sites: HashSet<usize>) {
        self.mh_constant_bound_static_sites = sites;
    }

    /// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): record which of the
    /// constant-handle sites are a filter species', `invoke pc -> its row`.
    /// See [`IrMhFilterSite`].
    pub fn set_mh_constant_filter_sites(&mut self, sites: HashMap<usize, IrMhFilterSite>) {
        self.mh_constant_filter_sites = sites;
    }

    pub fn set_mh_constant_convert_sites(&mut self, sites: HashMap<usize, IrMhConvertSite>) {
        self.mh_constant_convert_sites = sites;
    }

    /// JIT round 19 wave 1 (lane mhiface; MHRESID18-4): record which of the
    /// constant-handle sites are an interface handle's. See
    /// [`IrBuilder::mh_constant_interface_sites`].
    pub fn set_mh_constant_interface_sites(&mut self, sites: HashSet<usize>) {
        self.mh_constant_interface_sites = sites;
    }

    /// JIT round 20 (MHIFACE19-1): the seen-class cells of the interface
    /// handle sites, by pc. See
    /// [`IrBuilder::mh_constant_interface_seen_cells`].
    pub fn set_mh_constant_interface_seen_cells(&mut self, cells: HashMap<usize, usize>) {
        self.mh_constant_interface_seen_cells = cells;
    }

    /// Emit an unboxing accessor inline. One input (the receiver), and the
    /// node needs `ctrl` and `mem` because it can DEOPT and it reads the heap.
    pub(in crate::ir) fn try_emit_unbox_intrinsic(&mut self, pc: usize) -> bool {
        let Some(&(uop, class_id)) = self.unbox_intrinsics.get(&pc) else {
            return false;
        };
        // A constant-`VarHandle` field read is a CANDIDATE row: the handle, and
        // with it the slot and the receiver class, are decided here from the
        // site's first operand.
        if uop.is_varhandle_field_read() {
            return self.try_emit_vh_field_read(pc, uop);
        }
        if uop.is_scoped() {
            return self.try_emit_scoped_raw(pc, uop);
        }
        if self.ctrl_opt().is_none() {
            return false;
        }
        // A guard needs a snapshot to resume from, exactly as `plant_uncommon_trap`
        // does: without a safepoint at this bci the deopt would rebuild a frame
        // from nothing. Refusing here leaves the site to the fallback path.
        //
        // JIT round 16 wave 7 (lane callbind): inside an OPEN splice (a row
        // `IrInlineTables::unbox_intrinsics` carried in, under
        // `CRATONVM_JIT_IR_SPLICE_UNBOX_INTRINSIC`) the combined pc has no frame
        // of this method; the guard resumes either through the chain snapshot
        // recorded at `pc` or at the outermost `invoke` (`ir_lower`'s
        // `resume_bci`), re-running the whole call. The second is never sound
        // here: the body holding this site contains an `invoke` (this one), so
        // `spliced_bodies_pure` is false by construction
        // (`opcode_commits_side_effect`), and the receiver null guard's rule in
        // `begin_splice` would raise the graph's fence. This site has a
        // fallback instead -- its own call row, which the planner keeps -- so
        // without a chain snapshot at `pc` it declines. Checked before any pop,
        // like every refusal here.
        // Nor inside a synchronized splice's monitor window, which may not
        // trap at all (checked whole at its exit) and publishes no chain.
        //
        // JIT r23 wave 5, lane L2 (`CRATONVM_JIT_IR_FORWARDER_FLAT_UNBOX`): the
        // one exception is an OPAQUE lambda forwarder at depth 0 that has
        // committed nothing yet ([`Self::unbox_resumes_flat_in_forwarder`]).
        // Its guards resume at the outermost `invoke` by design -- its
        // `checkcast` already does -- and re-running that `invoke` re-runs a
        // forwarder prefix that only read and cast its arguments.
        let resumes_flat_in_forwarder = self.unbox_resumes_flat_in_forwarder(pc);
        let resumes_by_chain = !self.splice.is_empty() && !resumes_flat_in_forwarder;
        if resumes_flat_in_forwarder {
            // Checked whole by the predicate.
        } else if resumes_by_chain {
            if self.sync_splice_open() || !self.chain_state_at(pc) {
                return false;
            }
        } else if !self.has_frame_state_at(pc) {
            return false;
        }
        // The receiver is one abstract-stack entry and so is the one argument
        // any of these takes (`getAndAdd(J)J` -- a category-2 value is ONE entry
        // in this builder, see `static_call_shape`). A stack shallower than that
        // means the planned site and the abstract state disagree; refuse rather
        // than pop a depth nothing agreed on. Every refusal above and here
        // returns before the first `pop`, so the caller's dispatch path finds
        // the stack exactly as it left it.
        if self.stack.len() < uop.arity() + 1 {
            return false;
        }
        // Committed from here on: the graph now RELIES on the chain snapshot
        // (`Graph::splice_chains_required`), as every other spliced guard that
        // the chain admits.
        if resumes_by_chain && !self.relax_fence_by_chain(pc) {
            return false;
        }
        // Operands come off deepest-last: the delta is shallower, the receiver
        // is deepest, so the delta pops first.
        let delta = if uop.arity() == 1 {
            Some(self.pop())
        } else {
            None
        };
        let obj = self.pop();
        let mut inputs = vec![self.ctrl, self.mem, obj];
        if let Some(d) = delta {
            inputs.push(d);
        }
        let node = self.graph.add(
            Op::Unbox { op: uop, class_id },
            uop.result_type(),
            inputs,
            Some(pc),
        );
        // The node IS the new memory token as well as the result -- the
        // convention `Op::Load` and the String expander use. Three of the six
        // families WRITE the field with a `LOCK XADD`, so dropping this would
        // let a later read of the same object float above the write. It is set
        // for the pure loads too: the cost is ordering a load this tier could
        // otherwise move, and the alternative is a rule with an exception in it.
        self.mem = node;
        self.push(node);
        note_unbox_lowered();
        true
    }

    /// JIT r23 wave 5, lane L2 (`CRATONVM_JIT_IR_FORWARDER_FLAT_UNBOX`): may
    /// the unbox guard at `pc` resume at the outermost `invoke` rather than
    /// through a chain snapshot? Only inside an OPAQUE lambda SAM forwarder
    /// spliced at depth 0 (LambdaMetafactory's unboxing around the
    /// implementation, `SpliceFrame::opaque`), before anything in the splice
    /// committed (no surviving call, no committed store), outside any
    /// synchronized window, and with the compiling method's own frame state
    /// at that `invoke`. That is exactly where the forwarder's `checkcast`
    /// guard already resumes: a re-executed `invoke` re-runs a prefix that
    /// only read the captures and loaded and cast the arguments.
    ///
    /// Before this, the forwarder's `intValue` declined to its call row (no
    /// chain is recorded under an opaque forwarder), the call raised the
    /// replay fence, and the implementation's `invokestatic` after it refused
    /// the whole build (`BoxRef.boxed`: `IrBuilder::build refused at
    /// ir.rs:23613 (bytecode pc 24)`, the method kept its single-pass body and
    /// the SAM call its ~600 ns Rust lambda arm).
    pub(in crate::ir) fn unbox_resumes_flat_in_forwarder(&self, pc: usize) -> bool {
        let [only] = self.splice.as_slice() else {
            return false;
        };
        only.frameless
            && only.opaque
            && pc >= only.base
            && pc < only.end
            && !self.splice_call_committed
            && self.splice_committed_bases.is_empty()
            && !self.sync_splice_open()
            && self.has_frame_state_at(only.invoke_pc)
            && ir_forwarder_flat_unbox_enabled()
    }

    /// Fold `VarHandle.get*(receiver)` over a constant instance-field handle
    /// into the guarded field load [`Op::Unbox`] already lowers.
    ///
    /// The site's operand stack is `[.., handle, receiver]`. It folds only
    /// when `handle` is an [`Op::LoadStatic`] of a field in
    /// [`Self::vh_field_plans`] whose value kind agrees with the candidate
    /// row's (the site's return kind), and only where the same snapshot rules
    /// as [`Self::try_emit_unbox_intrinsic`] hold, because the guards deopt
    /// and the interpreter then re-runs the whole `VarHandle.get` (null
    /// receiver: the VarHandle's own NPE; another class: its own CCE or the
    /// subclass's field). Every refusal returns before the first pop, so the
    /// caller's call path finds the stack as it left it.
    ///
    /// `LoadStatic` of the handle stays in the graph as a (dead) operand; it
    /// is the class-initialisation obligation the original `getstatic` owed.
    pub(in crate::ir) fn try_emit_vh_field_read(&mut self, pc: usize, candidate: UnboxOp) -> bool {
        if self.ctrl_opt().is_none() {
            return false;
        }
        let resumes_by_chain = !self.splice.is_empty();
        if resumes_by_chain {
            if self.sync_splice_open() || !self.chain_state_at(pc) {
                return false;
            }
        } else if !self.has_frame_state_at(pc) {
            return false;
        }
        let len = self.stack.len();
        if len < 2 {
            return false;
        }
        let handle = self.stack[len - 2];
        let Some(node) = self.graph.nodes.get(handle as usize) else {
            return false;
        };
        let Op::LoadStatic {
            class_id,
            field_index,
            ..
        } = &node.op
        else {
            return false;
        };
        let Some(&plan) = self.vh_field_plans.get(&(*class_id, *field_index)) else {
            return false;
        };
        let wide = candidate.is_wide();
        if plan.receiver_class_id == 0 || (plan.value_desc == b'J') != wide {
            return false;
        }
        let uop = if wide {
            UnboxOp::FieldLong(plan.slot)
        } else {
            UnboxOp::FieldInt(plan.slot)
        };
        // Asked now as well as at lowering, exactly as the planner of the other
        // families does: a layout the access cannot address is a refusal here,
        // not an artifact the lowerer discards.
        if unbox_offsets(uop, plan.receiver_class_id).is_none() {
            return false;
        }
        if resumes_by_chain && !self.relax_fence_by_chain(pc) {
            return false;
        }
        let receiver = self.pop();
        let _handle = self.pop();
        let node = self.graph.add(
            Op::Unbox {
                op: uop,
                class_id: plan.receiver_class_id,
            },
            uop.result_type(),
            vec![self.ctrl, self.mem, receiver],
            Some(pc),
        );
        self.mem = node;
        self.push(node);
        note_unbox_lowered();
        true
    }

    /// Lower a `ScopedMemoryAccess` accessor call to [`Op::Unbox`] with a
    /// [`UnboxOp::ScopedGet`] / [`UnboxOp::ScopedPut`] family.
    ///
    /// The operand stack is `[.., sma, session, base, offset, (value,)
    /// (bigEndian)]`; the node takes them as `[ctrl, mem, sma, session, base,
    /// offset, (value,) (bigEndian)]`. Same snapshot rules as
    /// [`Self::try_emit_unbox_intrinsic`] (the guards deopt and the interpreter
    /// re-executes the whole call), and every refusal returns before the first
    /// pop so the caller's call row, kept as the fallback, sees the stack intact.
    pub(in crate::ir) fn try_emit_scoped_raw(&mut self, pc: usize, uop: UnboxOp) -> bool {
        if self.ctrl_opt().is_none() {
            return false;
        }
        let resumes_by_chain = !self.splice.is_empty();
        if resumes_by_chain {
            if self.sync_splice_open() || !self.chain_state_at(pc) {
                return false;
            }
        } else if !self.has_frame_state_at(pc) {
            return false;
        }
        let n = uop.arity() + 1;
        if self.stack.len() < n {
            return false;
        }
        let at = self.stack.len() - n;
        // A literal-null base is an off-heap access (`DirectByteBuffer`). Its
        // address is an `Unsafe.allocateMemory` arena HANDLE (bit 62 set), which
        // inline code cannot resolve, so the access would take the guard's deopt
        // on every call. Leave the call row to the helper instead.
        //
        // Round nio20 wave 3 (lane L4): so is a session the graph knows is NOT
        // null (an allocation, a constant-pool object): the lowering deopts on
        // every non-null session. And both refusals now come BEFORE
        // `relax_fence_by_chain`, which commits the graph to its chain
        // snapshots (`Graph::splice_chains_required`); the null-base one used
        // to follow it, leaving the requirement set for a site that kept its
        // call.
        //
        // nio21 wave 1 L3 (`r20-directbb-every-access-is-two-calls-and-an-arena-lookup`):
        // the literal-null base folds when the compile knows the JIT-readable
        // arena table (`scoped_offheap_fold`): the lowering then resolves a
        // live arena handle inline and deopts only on a legacy / freed /
        // resized block or an access that does not fit it. That replaces the
        // compiled public accessor, its `*Internal` intrinsic helper and the
        // locked `ArenaStore::copy_out` per access. The session rules are the
        // heap arm's: a shared session still deopts, so a shared close never
        // frees memory under a compiled access.
        let operand_op = |k: usize| self.graph.nodes.get(self.stack[at + k] as usize);
        if (!self.scoped_offheap_fold && operand_op(2).is_some_and(is_null_const))
            || operand_op(1).is_some_and(|s| crate::ir_check_elim::definitely_non_null(&s.op))
        {
            return false;
        }
        if resumes_by_chain && !self.relax_fence_by_chain(pc) {
            return false;
        }
        let operands: Vec<NodeId> = self.stack[at..].to_vec();
        self.stack.truncate(at);
        let mut inputs = vec![self.ctrl, self.mem];
        inputs.extend(operands);
        let node = self.graph.add(
            Op::Unbox {
                op: uop,
                class_id: 0,
            },
            uop.result_type(),
            inputs,
            Some(pc),
        );
        self.mem = node;
        if matches!(uop, UnboxOp::ScopedGet { .. }) {
            self.push(node);
        }
        note_unbox_lowered();
        // ACCEPT21-2 (t1 w8 lane ACCEPT, opt-in): an ALIGNED accessor has no
        // single-pass twin (`ir_evidence::BUILDER_EXPANSION_PARITY`).
        if matches!(
            uop,
            UnboxOp::ScopedGet { ordered: false, .. } | UnboxOp::ScopedPut { ordered: false, .. }
        ) && crate::ir_evidence::scoped_aligned_fold_is_evidence()
        {
            crate::ir_evidence::note_builder_expansion();
        }
        true
    }

    /// JIT r23 wave 4 (lane L8, L8-8 stage 5; `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE`,
    /// opt-in): expand a `MemorySegment` element accessor
    /// (`getAtIndex`/`setAtIndex`/`get`/`set`, integral carrier) into
    ///
    /// ```text
    ///   length = Unbox FieldLong(length)   recv     ; null + exact class + layout epoch
    ///   off    = idx * width  (index form) | idx    ; long, wrapping like the JDK
    ///   Guard off >= 0 ; Guard off <= length - width
    ///   [store] Guard Unbox FieldBool(readOnly) == 0
    ///   scope  = Unbox FieldRef(scope)
    ///   heap:   base = Unbox FieldRef(base) ; addr = Unbox FieldLong(offset) + off
    ///   native: base = null                 ; addr = Unbox FieldLong(min) + off
    ///   [align > 1] Guard (addr & (align - 1)) == 0
    ///   Unbox ScopedGet/ScopedPut { width, ordered } (recv, scope, base, addr[, value], be)
    /// ```
    ///
    /// Every guard deopts at the call's bci with the call's own frame state,
    /// so the interpreter re-runs the JDK accessor, which raises whatever it
    /// raises (IOOBE, the read-only IAE, the alignment IAE, a closed
    /// session's ISE) with the JDK's own frames; the scoped access does the
    /// session fold. The layout operand must be a `getstatic` of a filed
    /// `static final` layout ([`Self::ffm_layout_facts`]).
    ///
    /// `None`: not a planned site. `Some(true)`: emitted, the call consumed.
    /// `Some(false)`: declined before touching the stack; the caller keeps
    /// the call, or refuses the method when the site says `bail_on_decline`.
    pub(in crate::ir) fn try_emit_ffm_element_inline(&mut self, pc: usize) -> Option<bool> {
        use crate::ffm_element_inline::{ffm_inline_access_plan, FfmSegmentBase};
        // A spliced body's site is planned only under
        // `CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT` (JIT round 24 B3), keyed by its
        // combined pc (`plan_spliced_ffm_element_sites`): combined pcs lie past
        // the root body, so no root site is ever matched here (the r23 w5
        // review's concern). Without the switch the map holds root pcs only.
        let in_splice = !self.splice.is_empty();
        if in_splice && !crate::ir_splice_ffm_element_enabled() {
            return None;
        }
        let site = *self.ffm_inline_sites.get(&pc)?;
        // The guards' frame: the flat snapshot at a root site; inside a splice
        // the CHAIN snapshot the walk recorded for this pc, which a guard here
        // publishes (`ir_lower::splice_chain_for_site`) and which resumes the
        // callee at the accessor call with its callers parked -- re-running no
        // store the splice committed. Without one the site stays the call row
        // the splice kept (never a method refusal).
        if in_splice {
            if self.ctrl_opt().is_none() || !self.chain_snapshot_is_last_at(pc) {
                return None;
            }
        } else if self.ctrl_opt().is_none() || !self.has_frame_state_at(pc) {
            return Some(false);
        }
        // JIT round 24 (`CRATONVM_JIT_IR_FFM_ELEMENT_BIMORPHIC`): a root site
        // whose receivers split between two segment classes.
        if !in_splice {
            if let Some(&second) = self.ffm_bimorphic_seconds.get(&pc) {
                return self.try_emit_ffm_element_bimorphic(pc, site, second);
            }
        }
        let acc = site.accessor;
        // L8-8: an FP carrier moves through its raw bits (below), which needs
        // the raw-bits rows lowered.
        let fp = matches!(acc.carrier, b'F' | b'D');
        if fp && !ir_fp_raw_bits_enabled() {
            return Some(false);
        }
        let n = if acc.is_put { 4 } else { 3 };
        if self.stack.len() < n {
            return Some(false);
        }
        let at = self.stack.len() - n;
        let layout = self.stack[at + 1];
        let facts = match self.graph.nodes.get(layout as usize).map(|n| &n.op) {
            Some(Op::LoadStatic {
                class_id,
                field_index,
                ..
            }) => self
                .ffm_layout_facts
                .get(&(*class_id, *field_index))
                .copied(),
            _ => None,
        };
        let Some(facts) = facts else {
            return Some(false);
        };
        let Some(plan) = ffm_inline_access_plan(&site, facts) else {
            return Some(false);
        };
        let shape = site.shape;
        // r23 w5 (lane L8): a native segment's `min` is an `Unsafe` arena handle
        // the scoped lowering resolves only through the JIT-readable arena
        // table; without it the literal-null-base access deopts on every
        // call (the rule `try_emit_scoped_raw` keeps for a direct buffer).
        if matches!(shape.base, FfmSegmentBase::Native { .. }) && !self.scoped_offheap_fold {
            return Some(false);
        }
        let cid = shape.class_id;
        // The guarded reads, asked of the layout NOW, as every other Unbox
        // planner does: a slot the access cannot address is a decline here.
        let length_op = UnboxOp::FieldLong(shape.length);
        let scope_op = UnboxOp::FieldRef(shape.scope);
        let ro_op = UnboxOp::FieldBool(shape.read_only);
        let (addr_op, base_op) = match shape.base {
            FfmSegmentBase::Heap { base, offset, .. } => {
                (UnboxOp::FieldLong(offset), Some(UnboxOp::FieldRef(base)))
            }
            FfmSegmentBase::Native { min } => (UnboxOp::FieldLong(min), None),
        };
        if unbox_offsets(length_op, cid).is_none()
            || unbox_offsets(scope_op, cid).is_none()
            || unbox_offsets(addr_op, cid).is_none()
            || (acc.is_put && unbox_offsets(ro_op, cid).is_none())
            || base_op.is_some_and(|op| unbox_offsets(op, cid).is_none())
        {
            return Some(false);
        }

        // ── Past this point the site is consumed. ──
        let value = if acc.is_put { Some(self.pop()) } else { None };
        // An FP value stores its raw bits (`Float.floatToRawIntBits` /
        // `Double.doubleToRawLongBits`), as the JDK accessor's `Unsafe` put.
        let value = match (value, acc.carrier) {
            (Some(v), b'F') => Some(self.add_data(
                Op::ScalarIntrinsic(ScalarOp::FloatToRawIntBits),
                IrType::Int,
                vec![v],
                pc,
            )),
            (Some(v), b'D') => Some(self.add_data(
                Op::ScalarIntrinsic(ScalarOp::DoubleToRawLongBits),
                IrType::Long,
                vec![v],
                pc,
            )),
            (v, _) => v,
        };
        let index = self.pop();
        let _layout = self.pop();
        let recv = self.pop();
        let out = self.emit_ffm_shape_access(pc, &site, plan, recv, index, value, true);
        if !acc.is_put {
            self.push(out);
        }
        note_unbox_lowered();
        Some(true)
    }

    /// JIT round 24 (`CRATONVM_JIT_IR_FFM_ELEMENT_BIMORPHIC`, default ON since j27 2026-10-28): a
    /// root element site whose receiver profile splits between TWO segment
    /// classes the VM describes (`lib.rs` `ir_ffm_element_bimorphic_plan`):
    ///
    /// ```text
    ///   If(ExactClassIs(recv, first))
    ///     hit:  the expansion for `site.shape`
    ///     miss: the expansion for `second`
    ///   Merge ; Phi(mem) ; Phi(value)
    /// ```
    ///
    /// Each arm is the monomorphic expansion ([`Self::emit_ffm_shape_access`])
    /// for its class: its segment-field reads guard that EXACT class (null
    /// included) and deopt at the call's bci with the call's frame, so a null
    /// receiver or a third class leaves through the miss arm's first read,
    /// and the interpreter runs the JDK accessor -- the monomorphic site's
    /// rule. The arms' reads are not filed for the loop-entry hoist or the
    /// session proofs: they hold only on their own edge.
    ///
    /// Before this a bimorphic site had no plan, and its method stayed off
    /// the optimizing tier (`R26SegVecPut bi-put-add`: about 5400 ns per
    /// heap + native pair against HotSpot's 60).
    pub(in crate::ir) fn try_emit_ffm_element_bimorphic(
        &mut self,
        pc: usize,
        site: crate::ffm_element_inline::FfmElementInlineSite,
        second: crate::ffm_element_inline::FfmSegmentShape,
    ) -> Option<bool> {
        use crate::ffm_element_inline::ffm_inline_access_plan;
        let acc = site.accessor;
        if matches!(acc.carrier, b'F' | b'D') && !ir_fp_raw_bits_enabled() {
            return Some(false);
        }
        let n = if acc.is_put { 4 } else { 3 };
        if self.stack.len() < n {
            return Some(false);
        }
        let layout = self.stack[self.stack.len() - n + 1];
        let facts = match self.graph.nodes.get(layout as usize).map(|n| &n.op) {
            Some(Op::LoadStatic {
                class_id,
                field_index,
                ..
            }) => self
                .ffm_layout_facts
                .get(&(*class_id, *field_index))
                .copied(),
            _ => None,
        };
        let Some(facts) = facts else {
            return Some(false);
        };
        let site2 = crate::ffm_element_inline::FfmElementInlineSite {
            shape: second,
            ..site
        };
        let (Some(plan1), Some(plan2)) = (
            ffm_inline_access_plan(&site, facts),
            ffm_inline_access_plan(&site2, facts),
        ) else {
            return Some(false);
        };
        if site.shape.class_id == second.class_id
            || !self.ffm_shape_addressable(&site)
            || !self.ffm_shape_addressable(&site2)
        {
            return Some(false);
        }
        // ── Past this point the site is consumed. ──
        let value = if acc.is_put { Some(self.pop()) } else { None };
        let value = match (value, acc.carrier) {
            (Some(v), b'F') => Some(self.add_data(
                Op::ScalarIntrinsic(ScalarOp::FloatToRawIntBits),
                IrType::Int,
                vec![v],
                pc,
            )),
            (Some(v), b'D') => Some(self.add_data(
                Op::ScalarIntrinsic(ScalarOp::DoubleToRawLongBits),
                IrType::Long,
                vec![v],
                pc,
            )),
            (v, _) => v,
        };
        let index = self.pop();
        let _layout = self.pop();
        let recv = self.pop();
        let mem0 = self.mem;
        let test = self.add_data(
            Op::ExactClassIs {
                class_id: site.shape.class_id,
            },
            IrType::Int,
            vec![recv],
            pc,
        );
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![self.ctrl, test], Some(pc));
        self.graph.ffm_bimorphic_tests.insert(iff);
        let hit = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let miss = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
        self.ctrl = hit;
        let out1 = self.emit_ffm_shape_access(pc, &site, plan1, recv, index, value, false);
        let (ctrl1, mem1) = (self.ctrl, self.mem);
        self.ctrl = miss;
        self.mem = mem0;
        let out2 = self.emit_ffm_shape_access(pc, &site2, plan2, recv, index, value, false);
        let (ctrl2, mem2) = (self.ctrl, self.mem);
        let join = self
            .graph
            .add(Op::Merge, IrType::Control, vec![ctrl1, ctrl2], Some(pc));
        self.ctrl = join;
        self.mem = self
            .graph
            .add(Op::Phi, IrType::Memory, vec![join, mem1, mem2], Some(pc));
        if !acc.is_put {
            let ty = self.graph.nodes[out1 as usize].ty;
            let result = self
                .graph
                .add(Op::Phi, ty, vec![join, out1, out2], Some(pc));
            self.push(result);
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-ffm-element-inline BIMORPHIC at pc={pc}: classes {} / {} in {}",
                site.shape.class_id,
                second.class_id,
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        note_unbox_lowered();
        Some(true)
    }

    /// Can the expansion address every segment field `site.shape` needs (and
    /// a native shape's handle, through the JIT-readable arena table)?
    pub(in crate::ir) fn ffm_shape_addressable(
        &self,
        site: &crate::ffm_element_inline::FfmElementInlineSite,
    ) -> bool {
        use crate::ffm_element_inline::FfmSegmentBase;
        let shape = site.shape;
        if matches!(shape.base, FfmSegmentBase::Native { .. }) && !self.scoped_offheap_fold {
            return false;
        }
        let cid = shape.class_id;
        let (addr_op, base_op) = match shape.base {
            FfmSegmentBase::Heap { base, offset, .. } => {
                (UnboxOp::FieldLong(offset), Some(UnboxOp::FieldRef(base)))
            }
            FfmSegmentBase::Native { min } => (UnboxOp::FieldLong(min), None),
        };
        unbox_offsets(UnboxOp::FieldLong(shape.length), cid).is_some()
            && unbox_offsets(UnboxOp::FieldRef(shape.scope), cid).is_some()
            && unbox_offsets(addr_op, cid).is_some()
            && (!site.accessor.is_put
                || unbox_offsets(UnboxOp::FieldBool(shape.read_only), cid).is_some())
            && base_op.is_none_or(|op| unbox_offsets(op, cid).is_some())
    }

    /// The expansion of one planned element site for ONE segment shape
    /// (`site.shape`), from the current control and memory: the guarded
    /// segment-field reads, the bounds / read-only / alignment guards and the
    /// scoped access. Returns the access's value (a get, FP bits
    /// reinterpreted) or the put node. `hoistable`: file the reads for the
    /// loop-entry hoist and the session proofs (a monomorphic site), or not (an
    /// arm of a bimorphic one, JIT round 24, whose reads hold only on its own
    /// edge of the class test).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir) fn emit_ffm_shape_access(
        &mut self,
        pc: usize,
        site: &crate::ffm_element_inline::FfmElementInlineSite,
        plan: crate::ffm_element_inline::FfmInlineAccess,
        recv: NodeId,
        index: NodeId,
        value: Option<NodeId>,
        hoistable: bool,
    ) -> NodeId {
        use crate::ffm_element_inline::FfmSegmentBase;
        let acc = site.accessor;
        let shape = site.shape;
        let cid = shape.class_id;
        let length_op = UnboxOp::FieldLong(shape.length);
        let scope_op = UnboxOp::FieldRef(shape.scope);
        let ro_op = UnboxOp::FieldBool(shape.read_only);
        let (addr_op, base_op) = match shape.base {
            FfmSegmentBase::Heap { base, offset, .. } => {
                (UnboxOp::FieldLong(offset), Some(UnboxOp::FieldRef(base)))
            }
            FfmSegmentBase::Native { min } => (UnboxOp::FieldLong(min), None),
        };
        let ctrl = self.ctrl;
        let read = |b: &mut Self, op: UnboxOp| {
            let node = b.graph.add(
                Op::Unbox { op, class_id: cid },
                op.result_type(),
                vec![b.ctrl, b.mem, recv],
                Some(pc),
            );
            b.mem = node;
            // JIT r23 wave 8 (lane W8-L4): a final segment field, for the
            // loop-entry hoist (`ir_optimize::ffm_field_hoist`) -- not an arm
            // of a bimorphic site, whose reads hold only on their own edge.
            if hoistable {
                b.graph.ffm_final_field_reads.insert(node);
            }
            node
        };
        let guard = |b: &mut Self, cond: NodeId| {
            b.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, cond],
                Some(pc),
            );
        };
        let width = i64::from(plan.width);

        let length = read(self, length_op);
        let off = if acc.by_index {
            let w = self.lconst(width);
            self.add_data(Op::Mul, IrType::Long, vec![index, w], pc)
        } else {
            index
        };
        let zero = self.lconst(0);
        let non_negative = self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![off, zero], pc);
        guard(self, non_negative);
        let w = self.lconst(width);
        let last = self.add_data(Op::Sub, IrType::Long, vec![length, w], pc);
        let in_bounds = self.add_data(Op::Cmp(CmpOp::Le), IrType::Int, vec![off, last], pc);
        guard(self, in_bounds);
        if acc.is_put {
            let ro = read(self, ro_op);
            let izero = self.iconst(0);
            let writable = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![ro, izero], pc);
            guard(self, writable);
        }
        let scope = read(self, scope_op);
        // JIT r23 wave 9 (lane L4): a heap segment's scope, for the loop-entry
        // session proof (`ir_optimize::ffm_field_hoist`).
        if !hoistable {
            // A bimorphic arm: no loop-entry proof for a read of one edge.
        } else if matches!(shape.base, FfmSegmentBase::Heap { .. }) {
            self.graph.ffm_heap_scope_reads.insert(scope);
        } else {
            // JIT round 23 close: a native segment's, for the loop-entry
            // session pre-check (`CRATONVM_JIT_IR_FFM_SESSION_PRECHECK`).
            self.graph.ffm_native_scope_reads.insert(scope);
        }
        let base = match base_op {
            Some(op) => {
                let base = read(self, op);
                // JIT r23 wave 7 (lane L8): a `HeapMemorySegmentImpl$OfByte`
                // (the exact class the reads above guard) is built only over a
                // `byte[]` (`SegmentFactories.fromArray(byte[])`, a final
                // field), so the scoped access may skip its primitive-array
                // screen (`Graph::byte_array_nodes`), as for `ByteBuffer.hb`.
                if matches!(shape.base, FfmSegmentBase::Heap { element: b'B', .. }) {
                    self.graph.byte_array_nodes.insert(base);
                }
                base
            }
            None => self.aconst_null(),
        };
        let start = read(self, addr_op);
        let addr = self.add_data(Op::Add, IrType::Long, vec![start, off], pc);
        if plan.align_mask != 0 {
            let mask = self.lconst(plan.align_mask);
            // JIT r23 wave 8 (lane W8-L4, `CRATONVM_JIT_IR_FFM_INDEX_ALIGN_INVARIANT`):
            // `idx * width` has no bit under the mask (`align` divides
            // `width`), so the index form tests `start` alone.
            let tested = if acc.by_index
                && width & plan.align_mask == 0
                && ir_ffm_index_align_invariant_enabled()
            {
                start
            } else {
                addr
            };
            let low = self.add_data(Op::And, IrType::Long, vec![tested, mask], pc);
            let lzero = self.lconst(0);
            let aligned = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![low, lzero], pc);
            guard(self, aligned);
        }
        let be = self.iconst(i64::from(plan.big_endian));
        let uop = if acc.is_put {
            UnboxOp::ScopedPut {
                width: plan.width,
                ordered: true,
            }
        } else {
            UnboxOp::ScopedGet {
                width: plan.width,
                signed: plan.signed,
                ordered: true,
            }
        };
        let mut inputs = vec![self.ctrl, self.mem, recv, scope, base, addr];
        inputs.extend(value);
        inputs.push(be);
        let node = self.graph.add(
            Op::Unbox {
                op: uop,
                class_id: 0,
            },
            uop.result_type(),
            inputs,
            Some(pc),
        );
        self.mem = node;
        // JIT r23 wave 8 (lane W8-L4, `CRATONVM_JIT_IR_FFM_HEAP_ACCESS_PROVEN`):
        // the reads and guards above prove the heap arm's null tests, array
        // screen and bounds (`Graph::ffm_proven_heap_accesses`).
        if base_op.is_some() && ir_ffm_heap_access_proven_enabled() {
            self.graph.ffm_proven_heap_accesses.insert(node);
        }
        // JIT round 24 B1: the length field the guards above read.
        self.graph
            .ffm_scoped_length_fields
            .insert(node, (cid, length_op));
        if acc.is_put {
            return node;
        }
        {
            // An FP load is the bits read, reinterpreted.
            match acc.carrier {
                b'F' => self.add_data(
                    Op::ScalarIntrinsic(ScalarOp::IntBitsToFloat),
                    IrType::Float,
                    vec![node],
                    pc,
                ),
                b'D' => self.add_data(
                    Op::ScalarIntrinsic(ScalarOp::LongBitsToDouble),
                    IrType::Double,
                    vec![node],
                    pc,
                ),
                _ => node,
            }
        }
    }

    /// Emit the arithmetic for a scalar-intrinsic call site, if this pc is one.
    ///
    /// Returns `true` when it consumed the site. The operands come off the
    /// abstract stack in reverse order, which is the same convention every
    /// binary arm in this builder uses; the receiver is never popped because
    /// every family in [`ScalarOp`] is declared `static`.
    pub(in crate::ir) fn try_emit_scalar_intrinsic(&mut self, pc: usize) -> bool {
        let Some(&sop) = self.scalar_intrinsics.get(&pc) else {
            return false;
        };
        let inputs = match sop.arity() {
            // JIT round 19 wave 2 (lane irops2): `Math.fma`'s three operands.
            3 => {
                let c = self.pop();
                let b = self.pop();
                let a = self.pop();
                vec![a, b, c]
            }
            2 => {
                let b = self.pop();
                let a = self.pop();
                vec![a, b]
            }
            _ => vec![self.pop()],
        };
        let node = self.add_data(Op::ScalarIntrinsic(sop), sop.result_type(), inputs, pc);
        if let Some(cell) = self.first_call_cell_at(pc) {
            self.graph.scalar_first_call_cells.push((pc, sop, cell));
        }
        self.push(node);
        note_scalar_intrinsic_lowered();
        true
    }

    /// Gap B: supply the resolved call sites (`pc → (info_ptr, num_args,
    /// ret_type)`) the builder lowers into `Op::Call`. Must be called before
    /// [`Self::build`]; an invoke pc not present bails the build to
    /// single-pass. `ret_type` is the JVM return-type byte (see the field doc).
    pub fn set_invoke_info(&mut self, info: HashMap<usize, (usize, usize, u8)>) {
        self.invoke_info = info;
    }

    /// Round 11 wave 10 (lane `synccall`): the `invokestatic` pcs to lower as
    /// a caller-held synchronized direct CALL, `pc -> (holder class id, class
    /// cp index, mirror slot)`. A pc here must also have an `invoke_info` row;
    /// the lowerer serves it only through the tagged `ir_direct_calls` row
    /// `lib.rs` plans beside it. See the `0xb6 | 0xb8` arm. An INSTANCE row
    /// (`invokevirtual` / `invokespecial`) carries `(exact receiver class id or
    /// 0, 0, 0)`; see `enter_receiver_monitor`.
    pub fn set_sync_direct_calls(&mut self, sites: HashMap<usize, (u32, u16, usize)>) {
        self.sync_direct_calls = sites;
    }

    /// Round 14 wave 2 (lane syncsplice, SR-1): the body `lib.rs` plans at
    /// caller pc `pc` is `synchronized`; `static_class_id` is the class a
    /// `static` callee declares, `0` for an instance callee. See
    /// [`SyncSpliceWindow`] for the shape and its soundness argument. Must be
    /// called for EVERY synchronized body the plan carries: an unregistered
    /// one would be spliced with no monitor at all.
    pub fn add_sync_splice_site(&mut self, pc: usize, static_class_id: u32) {
        self.sync_splice_sites.insert(pc, static_class_id);
    }

    /// Round 14 wave 3 (lane sync, SS-2): the class mirror a `static
    /// synchronized` splice at `pc` locks when the site carries no caller-held
    /// CALL row (the callee's published body is self-locking, or there is
    /// none): `(holder class id, class cp index, mirror slot)`, exactly the
    /// row's static shape -- the Methodref's `CONSTANT_Class` in the
    /// compiling method's pool, whose class the VM checked declares the
    /// callee (JVMS 2.11.10). A row, when present, still wins.
    pub fn add_sync_splice_mirror(&mut self, pc: usize, mirror: (u32, u16, usize)) {
        self.sync_splice_mirrors.insert(pc, mirror);
    }

    /// Round 14 wave 7 (lane sync7, RV6-1): the compiling `static
    /// synchronized` method's own `CONSTANT_Class` entries, `(holder class id,
    /// cp index)`, that the VM resolved (without loading) to the class the
    /// graph claims in [`Graph::method_monitor_class`]. An `ldc` of one is
    /// that class's mirror whenever it produces a value (JVMS 5.4.3: an entry
    /// resolves once, to one class), so `Thread.holdsLock` of it folds to `1`
    /// in [`Self::try_fold_holds_lock`]. The `ConstClass` node itself stays
    /// (its resolution may still throw). Replaces any earlier set.
    pub fn set_method_monitor_class_cps(&mut self, cps: HashSet<(u32, u16)>) {
        self.method_monitor_class_cps = cps;
    }

    /// Round 15 wave 1 (lane sync, SS8-2): build this `ACC_SYNCHRONIZED`
    /// INSTANCE method as a SELF-LOCKING body ([`MethodSelfLock`]). `lib.rs`
    /// asks only for a method `self_lock_admits_method` admits (no exception
    /// table, no monitor bytecode, no store to local 0, no `invokedynamic`);
    /// the build re-checks what it relies on and is REFUSED -- never built
    /// without the lock -- when the shape cannot carry it
    /// ([`Self::begin_method_self_lock`]).
    pub fn set_method_self_lock(&mut self, on: bool) {
        self.method_self_lock_requested = on;
    }

    /// Round 15 wave 3 (lane sync2, proposal S15-3 of
    /// `jit-r15-sync-proposals-RETIRED-20261003.md`): the self-locking body asked for with
    /// [`Self::set_method_self_lock`] is a `static synchronized` method's,
    /// whose monitor is its declaring class's mirror (JVMS 2.11.10):
    /// `class_id`, and `mirror_slot`, the VM's GC-maintained word holding that
    /// mirror (`CompileRequest::self_lock_mirror_slot`, the word the
    /// single-pass static body reads). A `0` in either refuses the build.
    pub fn set_method_self_lock_mirror(&mut self, class_id: u32, mirror_slot: usize) {
        self.method_self_lock_mirror = Some((class_id, mirror_slot));
    }

    pub fn set_root_caller_frame(&mut self, mirror_slot: usize, security_walk_ignored: bool) {
        self.root_caller_mirror_slot = mirror_slot;
        self.root_security_walk_ignored = security_walk_ignored;
    }

    pub fn set_object_class_id(&mut self, id: Option<u32>) {
        self.object_class_id = id;
    }

    pub fn set_const_folds(&mut self, folds: ConstFoldTables) {
        self.const_folds = folds;
    }

    pub(in crate::ir) fn try_fold_reflection_get_caller_class(&mut self, info_ptr: usize) -> bool {
        if self.splice.is_empty() || !is_reflection_get_caller_class_info_row(info_ptr) {
            return false;
        }
        let mut slot_addr = 0usize;
        let mut found_frame = false;
        for frame in self.splice[..self.splice.len() - 1].iter().rev() {
            if !frame.security_walk_ignored {
                slot_addr = frame.caller_mirror_slot;
                found_frame = true;
                break;
            }
        }
        if !found_frame && !self.root_security_walk_ignored {
            slot_addr = self.root_caller_mirror_slot;
            found_frame = true;
        }
        if !found_frame || slot_addr == 0 {
            return false;
        }
        let v = self.graph.add(
            Op::IndyRootRef { slot_addr },
            IrType::Ref,
            vec![self.ctrl],
            None,
        );
        self.const_ref_slots.insert(v, slot_addr);
        self.push(v);
        true
    }

    /// The compiling method's de-speculation registry and its key in it, for
    /// [`Self::plant_static_guess_guard`].
    pub fn set_static_guess_despec(
        &mut self,
        despec: Option<std::sync::Arc<crate::deopt::DespecRegistry>>,
        method_key: String,
    ) {
        self.static_guess_despec = despec.map(|d| (d, method_key));
    }

    /// The guessed-handle slot of `node`: a plain read of a non-final static
    /// `ConstFoldTables::static_guess_const_refs` names, or a phi whose every
    /// input is one with the same slot.
    pub(in crate::ir) fn static_guess_slot_of(&self, node: NodeId, depth: u8) -> Option<usize> {
        let n = self.graph.nodes.get(node as usize)?;
        match n.op {
            Op::LoadStatic {
                class_id,
                field_index,
                type_tag: b'L',
                is_volatile: false,
            } => self
                .const_folds
                .static_guess_const_refs
                .get(&(class_id, field_index))
                .map(|&(slot, _)| slot)
                .filter(|&slot| slot != 0),
            Op::Phi if depth < 4 => {
                let mut agreed = None;
                for &inp in n.inputs.iter().skip(1) {
                    if inp == NO_NODE || inp == node {
                        continue;
                    }
                    let slot = self.static_guess_slot_of(inp, depth + 1)?;
                    if agreed.is_some_and(|prev| prev != slot) {
                        return None;
                    }
                    agreed = Some(slot);
                }
                agreed
            }
            _ => None,
        }
    }

    /// Guard `handle == [slot]` at the call `pc`, where a failure deopts to
    /// the interpreter's call (inside a splice, through the chain snapshot at
    /// `pc`, like the fold's own guards) and, past the trap limit,
    /// de-speculates the charged bci -- `pc`, or the outermost `invoke` of a
    /// splice -- so the next build keeps the handle call. `false` (nothing
    /// planted) without a frame state or a registry, or at a de-speculated bci.
    pub(in crate::ir) fn plant_static_guess_guard(
        &mut self,
        pc: usize,
        handle: NodeId,
        slot_addr: usize,
    ) -> bool {
        let charged = self.splice.first().map_or(pc, |outer| outer.invoke_pc);
        let admitted = self
            .static_guess_despec
            .as_ref()
            .is_some_and(|(d, key)| u32::try_from(charged).is_ok_and(|b| !d.contains(key, b)))
            && if self.splice.is_empty() {
                self.has_frame_state_at(pc)
            } else {
                ir_site_trap_enabled() && !self.sync_splice_open() && self.relax_fence_by_chain(pc)
            };
        let Some(ctrl) = self.ctrl_opt().filter(|_| admitted) else {
            return false;
        };
        let guess = self.graph.add(
            Op::IndyRootRef { slot_addr },
            IrType::Ref,
            vec![self.ctrl],
            None,
        );
        let same = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![handle, guess], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, same],
            Some(pc),
        );
        true
    }

    pub(in crate::ir) fn const_ref_slot_of(&self, node: NodeId) -> Option<usize> {
        self.const_ref_slot_of_inner(node, 0)
    }

    pub(in crate::ir) fn const_ref_slot_of_inner(&self, node: NodeId, depth: u8) -> Option<usize> {
        if let Some(&slot) = self.const_ref_slots.get(&node) {
            return Some(slot);
        }
        if depth >= 4 {
            return None;
        }
        let n = self.graph.nodes.get(node as usize)?;
        match n.op {
            Op::IndyRootRef { slot_addr } => Some(slot_addr),
            Op::Phi => {
                // Skip input[0] (Region/Merge control) and any self-loop input.
                let mut agreed: Option<usize> = None;
                for &inp in n.inputs.iter().skip(1) {
                    if inp == NO_NODE || inp == node {
                        continue;
                    }
                    let slot = self.const_ref_slot_of_inner(inp, depth + 1)?;
                    match agreed {
                        None => agreed = Some(slot),
                        Some(prev) if prev == slot => {}
                        Some(_) => return None,
                    }
                }
                agreed
            }
            _ => None,
        }
    }

    pub(in crate::ir) fn try_fold_reflection_get_class_access_flags(
        &mut self,
        info_ptr: usize,
    ) -> bool {
        if !is_reflection_get_class_access_flags_info_row(info_ptr) {
            return false;
        }
        let Some(arg) = self.peek_opt() else {
            return false;
        };
        let Some(slot_addr) = self.const_ref_slot_of(arg) else {
            return false;
        };
        let Some(&flags) = self.const_folds.const_class_access_flags.get(&slot_addr) else {
            return false;
        };
        self.pop();
        let v = self.iconst(flags as i64);
        self.push(v);
        true
    }

    /// Round 15 wave 1 (lane sync, SS8-2): when a self-locking body was asked
    /// for, emit `MonitorEnter(this)` on the entry control and memory, before
    /// the walk records its first snapshot, and push `this` on the monitor
    /// stack as the method monitor no bytecode pops. Every snapshot then names
    /// the hold (the hand-over shape the VM recognises), including the one at
    /// bci 0: the enter is on the entry edge, outside any loop whose header is
    /// bci 0, and no node that can transfer is placed ahead of it (`ir_lower`
    /// checks the schedule). `false` refuses the build: no receiver
    /// `Op::Param(0)` in local 0, a store to local 0 anywhere in the method's
    /// own code, or state that is not the method entry's.
    ///
    /// Round 15 wave 3 (lane sync2, S15-3): a `static synchronized` body
    /// ([`Self::set_method_self_lock_mirror`]) locks its class mirror instead:
    /// `m = ConstClass { class, SELF_LOCK_MIRROR_CP_IDX, slot }` on the entry
    /// edge (the memory token, as a static synchronized splice's mirror is),
    /// then `MonitorEnter(m)`, and `m` is the monitor every snapshot names.
    /// `ir_lower` lowers that one `ConstClass` as a plain read of the slot --
    /// never the resolution helper, so nothing ahead of the enter can
    /// transfer -- and every exceptional exit re-reads the slot to release
    /// (the node's home is not trusted there). Refused: a receiver claim, no
    /// class or slot.
    pub(in crate::ir) fn begin_method_self_lock(&mut self, code: &[u8], code_len: usize) -> bool {
        if !self.method_self_lock_requested {
            return true;
        }
        if self.graph.method_self_lock.is_some()
            || !self.splice.is_empty()
            || !self.monitors.is_empty()
            || !self.stack.is_empty()
        {
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        let monitor = if let Some((class_id, mirror_slot)) = self.method_self_lock_mirror {
            if self.graph.receiver_param.is_some() || class_id == 0 || mirror_slot == 0 {
                return false;
            }
            let m = self.graph.add(
                Op::ConstClass {
                    holder_class_id: class_id,
                    cp_idx: SELF_LOCK_MIRROR_CP_IDX,
                    slot_addr: mirror_slot,
                },
                IrType::Ref,
                vec![ctrl, self.mem],
                Some(0),
            );
            self.mem = m;
            m
        } else {
            if self.graph.receiver_param != Some(0) || body_writes_local_zero(code, code_len) {
                return false;
            }
            let Some(&this) = self.locals.first() else {
                return false;
            };
            let is_receiver = self
                .graph
                .nodes
                .get(this as usize)
                .is_some_and(|n| matches!(n.op, Op::Param(0)) && n.ty == IrType::Ref);
            if !is_receiver {
                return false;
            }
            this
        };
        let enter = self.graph.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![ctrl, self.mem, monitor],
            Some(0),
        );
        self.mem = enter;
        self.monitors.push(monitor);
        self.graph.method_self_lock = Some(MethodSelfLock {
            enter,
            monitor,
            exits: Vec::new(),
        });
        note_sync_splice_census(SYNC_CENSUS_IR_SELF_LOCK_BUILT);
        if self.method_self_lock_mirror.is_some() {
            note_sync_splice_census(SYNC_CENSUS_IR_SELF_LOCK_STATIC_BUILT);
        }
        true
    }

    /// Round 15 wave 1 (lane sync, SS8-2): is the only monitor this frame
    /// holds the self-locking method's own ([`Graph::method_self_lock`])?
    pub(in crate::ir) fn holds_only_method_self_lock(&self) -> bool {
        match (&self.graph.method_self_lock, self.monitors.as_slice()) {
            (Some(lock), [held]) => *held == lock.monitor,
            _ => false,
        }
    }

    /// Round 15 wave 1 (lane sync, SS8-2): at a method-exit `*return` of a
    /// self-locking body, `MonitorExit(this)` at the return's bci, BEFORE the
    /// `Op::Return` (JVMS 2.11.10: the release happens as the method
    /// completes). The monitor stack is not popped: nothing after a return
    /// reads it, and a join the walk reaches next restores its own. Anything
    /// the return arm builds after this (the `ireturn` narrowing) is pure
    /// arithmetic. `false` refuses the build: a return inside a splice, or one
    /// with a monitor held that is not the method's own.
    pub(in crate::ir) fn release_method_self_lock_at_return(&mut self, pc: usize) -> bool {
        if !self.splice.is_empty() || !self.holds_only_method_self_lock() {
            return false;
        }
        let (Some(ctrl), Some(monitor)) = (
            self.ctrl_opt(),
            self.graph
                .method_self_lock
                .as_ref()
                .map(|lock| lock.monitor),
        ) else {
            return false;
        };
        let exit = self.graph.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![ctrl, self.mem, monitor],
            Some(pc),
        );
        self.mem = exit;
        if let Some(lock) = self.graph.method_self_lock.as_mut() {
            lock.exits.push(exit);
        }
        true
    }

    /// Does the synchronized splice planned at `pc` have a class mirror to
    /// lock -- a static caller-held row or an [`Self::add_sync_splice_mirror`]
    /// entry? The planner asks it for a `static` callee before it commits the
    /// body; `sync_splice_plan` reads the same two sources.
    pub fn sync_splice_static_monitor_known(&self, pc: usize) -> bool {
        self.sync_splice_row_admits(pc, true)
            || self
                .sync_splice_mirrors
                .get(&pc)
                .is_some_and(|&(holder, cp_idx, _)| holder != 0 && cp_idx != 0)
    }

    /// Does the caller-held synchronized CALL row at `pc`
    /// ([`Self::set_sync_direct_calls`]) have the shape of a `static` (or an
    /// unguarded instance) callee's? A synchronized splice of a static callee
    /// takes its class mirror from that row. The planner asks before it
    /// commits the body; `sync_splice_plan` asks again at the site.
    ///
    /// A static row names the class mirror (`holder`, `cp_idx`, the mirror
    /// slot); an instance row is `(0, 0, 0)` -- a statically bound target,
    /// whose monitor is the receiver. A CHA-bound row (a non-zero exact class)
    /// needs a class guard before the enter, which a splice does not build.
    pub fn sync_splice_row_admits(&self, pc: usize, is_static: bool) -> bool {
        match self.sync_direct_calls.get(&pc) {
            Some(&(holder_class_id, cp_idx, _)) if is_static => holder_class_id != 0 && cp_idx != 0,
            Some(&(exact_class_id, cp_idx, slot_addr)) => {
                exact_class_id == 0 && cp_idx == 0 && slot_addr == 0
            }
            None => false,
        }
    }

    /// How the synchronized splice planned at `pc` takes its monitor, or
    /// `None` when it cannot be built HERE -- the site then keeps its call
    /// (the caller-held route when it has a row, else dispatch). Asked by the
    /// invoke arms before they splice
    /// ([`Self::sync_splice_site_admits`]) and by [`Self::begin_splice`].
    ///
    /// * only an OUTERMOST splice: a nested body's pcs are not the caller's,
    ///   and no frame of an enclosing splice could describe the lock (a
    ///   same-receiver synchronized body nested in the window is spliced with
    ///   NO pair instead, round 14 wave 6, SS8-1 a -- see
    ///   [`Self::sync_splice_held_site_proven`]; it never comes here);
    /// * inside a region this frame holds (`synchronized (o) { o.leaf(); }`)
    ///   only under [`ir_sync_splice_in_region_enabled`] (round 14 wave 3,
    ///   SS-3): the nested-lock elision (`ir_optimize::elide_nested_monitors`)
    ///   classifies a monitor op by its bci's snapshot, and an enter at an
    ///   `invoke` inside a held region would read as nested while its exit
    ///   would not -- since wave 3 it skips the recorded window ops by node;
    /// * a body walked in pc order (no block walk), not an exact-receiver
    ///   site: with one return the exit goes on its return edge
    ///   (`end_splice`); with several (round 14 wave 5, SS8-1 b, under
    ///   [`ir_sync_splice_multi_return_enabled`]) after the returns' join
    ///   (`finish_multi_return_splice`);
    /// * the snapshot at `pc` exists (the enter's own exits resume there).
    ///
    /// Static sites take the class mirror from the caller-held CALL's row, or,
    /// without one, from [`Self::add_sync_splice_mirror`] (SS-2).
    pub(in crate::ir) fn sync_splice_plan(&self, pc: usize) -> Option<SyncMonitorPlan> {
        let &static_class_id = self.sync_splice_sites.get(&pc)?;
        if !ir_sync_splice_enabled()
            || !self.splice.is_empty()
            || (!self.monitors.is_empty() && !ir_sync_splice_in_region_enabled())
            || self.exact_receiver_sites.contains_key(&pc)
            || !self.has_frame_state_at(pc)
        {
            return None;
        }
        let site = self.inline_sites.get(&pc)?;
        // Round 14 wave 5 (lane sync5, SS8-1 b): a body with several returns
        // takes its exit after `finish_multi_return_splice`'s join.
        if (self.splice_multi_return.contains(&site.base) && !ir_sync_splice_multi_return_enabled())
            || self
                .splice_block_walks
                .get(&site.base)
                .is_some_and(|ranges| !ranges.is_empty())
        {
            return None;
        }
        let is_static = !site.receiver_is_arg0;
        if is_static != (static_class_id != 0) {
            return None;
        }
        // An instance callee locks its receiver, which the resolver bound
        // statically (a static, private or final target, or a final class);
        // only a static one needs the row, for the mirror.
        if !is_static {
            return Some(SyncMonitorPlan::Instance);
        }
        let &(holder_class_id, cp_idx, slot_addr) = if self.sync_splice_row_admits(pc, true) {
            self.sync_direct_calls.get(&pc)?
        } else {
            self.sync_splice_mirrors.get(&pc)?
        };
        if holder_class_id == 0 || cp_idx == 0 {
            return None;
        }
        Some(SyncMonitorPlan::Static {
            holder_class_id,
            cp_idx,
            slot_addr,
            class_id: static_class_id,
        })
    }
}
