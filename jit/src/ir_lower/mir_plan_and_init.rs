#[allow(unused_imports)]
use super::*;

impl MirPlan {
    pub(in crate::ir_lower) fn tile(&self, id: NodeId) -> Option<&crate::x64::isel::Tile> {
        let (block, tile) = (*self.tile_of.get(id as usize)?)?;
        self.blocks.get(block as usize)?.tiles.get(tile as usize)
    }

    /// The pure integer instruction whose operands drive allocation and emission.
    pub(in crate::ir_lower) fn allocated_instruction(
        &self,
        id: NodeId,
    ) -> Option<crate::x64::isel::MInst> {
        use crate::x64::isel::MInst;
        let tile = self.tile(id)?;
        tile.insts
            .iter()
            .rev()
            .find(|instruction| {
                matches!(
                    instruction,
                    MInst::AluRR { .. }
                        | MInst::AluRI { .. }
                        | MInst::Lea { .. }
                        | MInst::Move { .. }
                        | MInst::AluRM {
                            addr: crate::x64::isel::AddrSource::Expr(_),
                            ..
                        }
                )
            })
            .copied()
    }

    /// A materialized comparison with no absorbed producers. Other tiles
    /// keep the existing emitter until their value flow is allocated too.
    pub(in crate::ir_lower) fn materialized_cmp(
        &self,
        id: NodeId,
    ) -> Option<(crate::x64::isel::MInst, CmpOp)> {
        use crate::x64::isel::MInst;
        let tile = self.tile(id)?;
        if tile.covered.as_slice() != [id] {
            return None;
        }
        match tile.insts.as_slice() {
            [cmp @ (MInst::CmpRR { .. } | MInst::CmpRI { .. } | MInst::TestRR { .. }), MInst::SetCc { dst, cc }]
                if *dst == id =>
            {
                Some((*cmp, *cc))
            }
            _ => None,
        }
    }

    pub(in crate::ir_lower) fn division(&self, id: NodeId) -> Option<[crate::x64::isel::MInst; 2]> {
        use crate::x64::isel::MInst;
        match self.tile(id)?.insts.as_slice() {
            [extend @ MInst::SignExtendDividend { .. }, divide @ MInst::Idiv { dst, .. }]
                if *dst == id =>
            {
                Some([*extend, *divide])
            }
            _ => None,
        }
    }

    #[cfg(test)]
    pub(in crate::ir_lower) fn value_flow(
        &self,
        graph: &Graph,
        schedule: &Schedule,
        protected: &[NodeId],
    ) -> CompileResult<Option<crate::regalloc::ValueFlow>> {
        self.value_flow_with_checks(graph, schedule, protected, None)
    }

    pub(in crate::ir_lower) fn value_flow_with_checks(
        &self,
        graph: &Graph,
        schedule: &Schedule,
        protected: &[NodeId],
        checks: Option<&crate::ir_check_elim::CheckElision>,
    ) -> CompileResult<Option<crate::regalloc::ValueFlow>> {
        use crate::regalloc::{ValueFlow, ValueGroup};
        let groups: Vec<_> = self
            .blocks
            .iter()
            .flat_map(|b| &b.tiles)
            .filter_map(|tile| {
                let node = graph.nodes.get(tile.root as usize)?;
                if !matches!(node.ty, IrType::Int | IrType::Long) {
                    return None;
                }
                Some(ValueGroup {
                    instructions: {
                        self.allocated_instruction(tile.root)?;
                        tile.insts.clone()
                    },
                    covered: tile.covered.clone(),
                })
            })
            .collect();
        if groups.is_empty() {
            Ok(None)
        } else {
            ValueFlow::new_with_checks(graph, schedule, groups, protected, checks).map(Some)
        }
    }

    pub(in crate::ir_lower) fn add_allocated_requirements(
        &self,
        live: &crate::regalloc::LiveModel,
        model: &mut crate::regalloc::MachineModel,
    ) -> CompileResult<()> {
        use crate::regalloc::{FixedConstraint, PhysReg};
        use crate::x64::isel::{pattern, MInst, Role};
        let mut fixed_added = false;
        for tile in self.blocks.iter().flat_map(|block| &block.tiles) {
            let Some(pos) = live.pos_of.get(tile.root as usize).copied().flatten() else {
                continue;
            };
            for instruction in &tile.insts {
                if matches!(instruction, MInst::Generic { .. } | MInst::SetCc { .. }) {
                    continue;
                }
                let row = instruction
                    .pattern_name()
                    .and_then(pattern)
                    .ok_or_else(|| {
                        Bailout::new(BailoutReason::Internal(
                            "selected instruction has no encoding requirements",
                        ))
                    })?;
                for fixed in row.fixed {
                    let value = match (*instruction, fixed.role, fixed.reg) {
                        (MInst::AluRR { rhs, .. }, Role::ShiftCount, RCX) => {
                            // The binder can stage a count into CL before the shift.
                            model.add_clobbers(pos, &[PhysReg::gp(RCX)]);
                            rhs
                        }
                        (MInst::SignExtendDividend { dividend, .. }, Role::ImplicitSrc, RAX) => {
                            dividend
                        }
                        // Sign extension and IDIV share the staged RAX. The
                        // high word in RDX is defined within the same tile.
                        (MInst::Idiv { .. }, Role::ImplicitSrc, RAX | RDX) => continue,
                        _ => return Err(Bailout::new(BailoutReason::UnsupportedShape(
                            "selected fixed-register instruction has no allocated operand binding",
                        ))),
                    };
                    let reg = PhysReg::gp(fixed.reg);
                    if model.regs.contains(reg) {
                        let constraint = FixedConstraint {
                            node: value,
                            pos,
                            reg,
                        };
                        if !model.fixed.contains(&constraint) {
                            model.fixed.push(constraint);
                            fixed_added = true;
                        }
                    }
                }
                let clobbers: Vec<_> = row
                    .clobbers
                    .iter()
                    .copied()
                    .map(crate::regalloc::PhysReg::gp)
                    .collect();
                model.add_clobbers(pos, &clobbers);
                if matches!(instruction, MInst::Idiv { .. }) {
                    // A homed divisor can require staging into RCX.
                    model.add_clobbers(pos, &[PhysReg::gp(RCX)]);
                }
            }
        }
        for tile in self.blocks.iter().flat_map(|block| &block.tiles) {
            let Some(instruction) = self.allocated_instruction(tile.root) else {
                continue;
            };
            let Some(pos) = live.pos_of.get(tile.root as usize).copied().flatten() else {
                continue;
            };
            let mut inputs = match instruction {
                MInst::AluRR { lhs, rhs, .. } => vec![lhs, rhs],
                // The allocator may hand off the left operand. The base and
                // index remain inputs at this position through ValueFlow.
                MInst::AluRI { lhs, .. } | MInst::AluRM { lhs, .. } => vec![lhs],
                MInst::Lea { addr, .. } => addr.base.into_iter().chain(addr.index).collect(),
                MInst::Move { src, .. } => vec![src],
                _ => continue,
            };
            inputs.retain(|&input| {
                input != tile.root
                    && live
                        .uses
                        .get(input as usize)
                        .is_some_and(|u| u.binary_search(&pos).is_ok())
            });
            if !inputs.is_empty() {
                model
                    .read_before_write
                    .push(crate::regalloc::ReadBeforeWrite {
                        result: tile.root,
                        inputs,
                        pos,
                    });
            }
        }
        model.read_before_write.sort_by_key(|w| w.pos);
        if fixed_added {
            model.fixed.sort_by_key(|f| (f.pos, f.reg, f.node));
        }
        Ok(())
    }
}

impl<'a> Lowerer<'a> {
    pub(in crate::ir_lower) fn new(
        graph: &'a Graph,
        schedule: &'a Schedule,
        buf: ExecutableBuffer,
        num_params: usize,
        num_locals: usize,
        slot_plan: &'a SlotPlan,
        helpers: &JitRuntimeHelpers,
        branch_hints: &'a HashMap<usize, bool>,
        spliced_ranges: &'a [(usize, usize, usize)],
        sr_map: Option<&'a ScalarReplacementMap>,
        direct_calls: &'a HashMap<usize, (usize, bool)>,
        ic_slots: &'a HashMap<usize, (usize, usize)>,
        compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
        inline_frame_sites: &'a crate::ir::IrInlineFrameSites,
    ) -> Self {
        // Frame homes of the reference PARAMETERS, in `[rbp - off]` form. Every
        // safepoint map republishes these; see `emit_safepoint_map`.
        let ref_param_homes: Vec<i16> = graph
            .nodes
            .iter()
            .filter_map(|n| match n.op {
                Op::Param(idx) if n.ty == IrType::Ref => {
                    let off = ((idx as i32) + 1) * 8;
                    if off > 0 && off <= i16::MAX as i32 {
                        Some(off as i16)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect();
        // Gap B: scan for `Op::Call` to size the call-related frame regions.
        // `needs_context` ⇒ the method takes the VM ptr as a hidden first arg
        // and reserves a context slot. `max_call_args` sizes the Java-argument
        // staging region a call marshals its args into before dispatching.
        //
        // Shared with `estimate_frame_bytes`, which `lower_inner` uses to
        // refuse an over-budget frame BEFORE this constructor reserves it; the
        // two must see the same needs or the check would bound a different
        // frame than the one built here.
        let mut needs = scan_frame_needs(graph, helpers);
        needs.vector_xmm_saves = vector_xmm_saves_wanted(graph, schedule);
        let needs_context = needs.needs_context;
        let max_call_args = needs.max_call_args;

        // Frame layout (rbp downward): locals, [context slot], five bookkeeping
        // words, spills, [callee-saved XMM save area], [args staging], 16-byte
        // stack-arg reserve, 32-byte shadow. Reserve slots for
        // locals + one per COLOUR + shadow. The 16-byte tail above the shadow
        // region holds in-frame stack args for any helper called without
        // `emit_stack_arg_setup`; see the matching comment in `x64.rs`
        // (Compiler::new) for the worst-case 6-arg `jit_invoke_virtual_mic` site.
        let locals_size = (num_locals as i32) * 8;
        let context_size = if needs_context { 8 } else { 0 };
        // Five extra reserved slots: the safepoint id, the cached
        // `*mut JvmThread`, the shadow stack's base `top` for this push, the
        // shadow `top` watermark captured at method entry, and the phi
        // parallel-copy scratch word (`emit_phi_copies`, which needs a location
        // other than RAX to park a value in while it unwinds a cycle).
        let bookkeeping_size = 8i32 * 5;
        // Liveness-based slot reuse: one 8-byte slot per *colour*, not per
        // graph node. [`plan_slots`] has already packed every value whose live
        // range does not overlap another's into a shared slot, so this term is
        // the peak simultaneous live count (plus the pinned classes), not
        // `graph.nodes.len()`. A value dead since block 0 no longer owns a slot
        // for the rest of the method.
        let spill_size = (slot_plan.slots as i32) * 8;
        let args_stage_size = (max_call_args as i32) * 8;
        let shadow = 32i32;
        let stack_arg_reserve = 16i32;
        // The frame is SIZED by the same estimator `lower_inner` bounds against
        // (`estimate_frame_bytes` + `check_frame_size`), so "the frame we
        // checked" and "the frame we build" are the same number by
        // construction rather than by two hand-kept-in-sync expressions. The
        // check has already refused anything above `DEFAULT_MAX_FRAME_BYTES`
        // (32 KiB), so every i32 term below is small and cannot overflow —
        // previously `(graph.nodes.len() as i32) * 8` on a pathological graph
        // could.
        let frame_size = estimate_frame_bytes(num_locals, slot_plan.slots, &needs) as i32;
        let saved_xmm_bytes = ir_saved_xmm_bytes_with(needs.vector_xmm_saves);
        let saved_gpr_bytes = ir_saved_gpr_bytes();
        // Hoisted above the mirror check because the check has to COUNT it.
        // The region itself is placed further down, off this same binding.
        let deopt_regs_bytes = ir_deopt_regs_bytes();
        debug_assert_eq!(
            frame_size,
            ((locals_size
                + context_size
                + bookkeeping_size
                + spill_size
                + saved_xmm_bytes
                + saved_gpr_bytes
                // The deopt register image. `estimate_frame_bytes` has counted
                // it since the region was introduced; this mirror never did, so
                // the two disagreed by exactly its 256 bytes whenever
                // `CRATONVM_JIT_IR_DEOPT_REGS` was on -- 464 against 208 in
                // `a_deopt_frame_reads_a_register_the_stub_spilled` and its two
                // siblings, which were the only tests that turned the flag on
                // while it was opt-in (it has been DEFAULT ON since 2026-09-05,
                // so every IR frame now reserves the 256-byte image).
                //
                // The ESTIMATOR was right: it is what sizes the frame, and
                // `deopt_regs_base` below is placed off the same reservation
                // (`spill_cap_off` subtracts it, and the abutment assertion two
                // screens down pins the result against the staging region). So
                // the frame was always laid out correctly and only the
                // statement of the invariant was wrong -- which is why this was
                // a debug-only red and never a wrong-code bug.
                + deopt_regs_bytes
                + args_stage_size
                + shadow
                + stack_arg_reserve)
                + 15)
                & !15,
            "estimate_frame_bytes must mirror the frame this constructor lays out",
        );

        // The context slot is the first slot after the locals; spills start
        // after it.
        //
        // BUG FIX (jit-inlining-and-ir-calls): `arg[0]` of the staging region
        // used to sit at `frame_size - shadow`, i.e. at address `RSP + 32` —
        // which is exactly where the Win64 ABI places a callee's FIFTH stack
        // argument, and `RSP + 40` (staged `arg[1]`) the sixth. That was
        // latent while every helper the IR lowerer called took at most four
        // arguments, so no stack argument was ever written. The inline-cache
        // miss path calls `jit_invoke_virtual_mic`, which takes SIX
        // (`vm, info, args_ptr, num_args, mic, pic`), and would have written
        // `mic`/`pic` straight over staged `arg[0]`/`arg[1]` — the receiver
        // and first argument of the very call being dispatched.
        //
        // The 16-byte `stack_arg_reserve` was already budgeted into
        // `frame_size` for exactly this purpose (see the layout comment above,
        // which describes the reserve as sitting between the staging region and
        // the shadow space); it simply was not being SKIPPED when placing the
        // staging region. Skip it now, so the documented layout and the actual
        // one agree: `[RSP, RSP+32)` shadow, `[RSP+32, RSP+48)` stack args 5-6,
        // staging from `RSP + 48` up, spills above that. Every consumer reads
        // `args_stage_top_off` symmetrically (`store_rax` to place, then
        // `lea_reg_from_frame` to pass `args_ptr`), so relocating the region is
        // transparent to the generic dispatch path.
        let context_slot_off = if needs_context {
            (num_locals as i32 + 1) * 8
        } else {
            0
        };
        // Relocation contract: one reserved slot holding the id of the
        // safepoint this frame is currently stopped at. `conservative_roots`
        // reads `[rbp - cm.sp_id_slot_off]` and matches it against `oop_maps`
        // to find the map for the ACTIVE safepoint — a union-of-all-maps is
        // unsound for relocation. Zero would mean "no slot", so ids start at 1.
        let base = num_locals as i32 + 1 + if needs_context { 1 } else { 0 };
        let sp_id_slot_off = base * 8;
        // Cached thread pointer, fetched once in the prologue. The shadow push
        // and reload both need it and neither may call out to get it.
        let shadow_thread_slot_off = (base + 1) * 8;
        // The `top` this push started from. The reload restores from exactly
        // here, so an intervening unbalanced push cannot drift it (the
        // single-pass backend's spring-bug-10 fix; same hazard applies here).
        let shadow_savebase_slot_off = (base + 2) * 8;
        // The shadow `top` at METHOD ENTRY. Every exit restores it, unwinding
        // any push this activation did not pop. Without it an abnormal exit
        // (the `i64::MIN` exception/deopt sentinel, which jumps straight to the
        // shared bail stub and skips the matching reload) leaks its push
        // FOREVER: nothing else retracts `top` once a raw JIT-to-JIT call has
        // removed the Rust boundary whose `restore_jit_thread` used to heal it.
        // The single-pass backend has always done this (`emit_epilogue`'s
        // savetop-restore); this tier never did, and leaked ~4 slots per
        // sentinel return until the 256K-slot stack ran off its end and the
        // unguarded push stored past the mapping. See
        // `x64.rs::Compiler::emit_epilogue` for the mechanism this mirrors.
        let shadow_savetop_slot_off = (base + 3) * 8;
        // One word for the phi parallel copy's `Save`/`Restore` scratch. Every
        // phi has its OWN frame word (`prealloc_phi_slots`), so an edge whose
        // copies form a cycle — the swap loop, whose two loop-header phis are
        // each other's back-edge value — cannot be sequentialised through RAX
        // alone: RAX is the move temporary and is dead the instant the next
        // `Move` loads through it. Parking one element of the cycle here frees
        // its predecessor and the rest unwinds with plain moves
        // (`regalloc::resolve_parallel_copy`). One word, once per method,
        // whether or not any edge ever needs it — the alternative was refusing
        // the compile and dropping a tier on every swap-shaped loop.
        //
        // It is a BOOKKEEPING word, below `first_spill`, so `plan_slots` can
        // never colour a value onto it and `verify_slot_colouring` /
        // `verify_data_locations` see an unchanged spill band.
        let phi_copy_scratch_slot_off = (base + 4) * 8;
        let first_spill = (base + 5) * 8;
        let args_stage_top_off = frame_size - shadow - stack_arg_reserve;
        // The callee-saved XMM save area sits directly ABOVE the spill band and
        // directly BELOW the outgoing-argument staging, so `callee_saved_lo`
        // stays exactly `spill_cap_off` and the band the GC verifier skips
        // (`conservative_roots::band_slot_is_verifiable`) grows to cover it
        // without any change on the reader side. That placement is not
        // cosmetic: a register image is precisely "storage this frame does not
        // resume from", which is what that band means.
        //
        // Register `i` occupies offsets `(xmm_saved_lo + 16*i, xmm_saved_lo +
        // 16*(i+1)]`, so its `MOVUPS` base is `[rbp - (spill_cap_off +
        // 16*(i+1))]` and the 16 bytes it writes run up to `rbp -
        // spill_cap_off` exclusive — inside the reservation, never over a
        // spill.
        // The GPR band sits below the XMM one and is excluded from the spill
        // range on exactly the same footing: a spill that overlapped it would
        // be silently destroyed by the prologue save.
        // The deopt register image sits DEEPER than both prologue save bands
        // and directly below the outgoing-argument staging, which is the one
        // placement that leaves `callee_saved_lo == spill_cap_off` and the two
        // bands' `spill_cap_off + ...` arithmetic untouched: the region simply
        // extends the non-resumable band, and "storage this frame does not
        // resume from" is exactly what a register image is.
        //
        // `gpr[0]` is its LOWEST address, at `[rbp - deopt_regs_base]`, and the
        // struct ascends from there — `gpr[r]` at `[rbp - (base - 8r)]`,
        // `xmm[n]` at `[rbp - (base - 128 - 8n)]`. Same convention as the
        // single-pass backend's `deopt_regs_base`, deliberately, so the two
        // stubs can be read against each other.
        let spill_cap_off = frame_size
            - shadow
            - stack_arg_reserve
            - args_stage_size
            - saved_xmm_bytes
            - saved_gpr_bytes
            - deopt_regs_bytes;
        // Which lands the region's deep end exactly at the bottom of the
        // staging region, leaving no gap and no overlap:
        //   spill_cap_off + saved_xmm + saved_gpr + deopt_regs
        //     == frame_size - shadow - stack_arg_reserve - args_stage_size
        //
        // ZERO when nothing was reserved, and that is not cosmetic: the stub
        // reads `deopt_regs_base > 0` as "there is a region to spill into", and
        // an arithmetic base that happened to be non-zero with no reservation
        // behind it sent 32 stores over the argument staging area and past it.
        // This is the mirror image of the hazard the single-pass backend
        // records in `deopt_spill_region_reserved` -- there an unreserved
        // region made the base 0 and the stores walked UP over the saved RBP
        // and the return address. Same class, opposite sign.
        let deopt_regs_base = if deopt_regs_bytes > 0 {
            spill_cap_off + saved_xmm_bytes + saved_gpr_bytes + deopt_regs_bytes
        } else {
            0
        };
        debug_assert!(
            deopt_regs_bytes == 0 || deopt_regs_base == args_stage_top_off - args_stage_size,
            "the deopt register image must abut the argument staging region"
        );
        // JIT round 25 item 3 and wave 2: every switch of the per-body claim
        // is read ONCE here, so the clear (prologue and OSR entry stubs) and
        // the publication leave out and claim one list
        // ([`Lowerer::body_claim_offsets_with`]). The two wave-2 classes ride
        // on item 3's claim and are off without it.
        let prim_colour_claim = ir_primitive_colour_claim_enabled();
        let pinned_prim_slot_offsets: Vec<i16> =
            if prim_colour_claim && ir_pinned_prim_colour_claim_enabled() {
                let claimable = claimable_pinned_nodes(graph, slot_plan);
                colour_frame_offsets(&slot_plan.claimable_pinned_colors(&claimable), first_spill)
            } else {
                Vec::new()
            };
        let deopt_image_claim = prim_colour_claim && ir_deopt_image_claim_enabled();
        let helper_area_claim =
            prim_colour_claim && !cfg!(target_os = "windows") && ir_helper_area_claim_enabled();

        Lowerer {
            direct_helpers: crate::DirectHelperTable::EMPTY,
            graph,
            schedule,
            buf,
            node_slot: vec![None; graph.nodes.len()],
            unallocated_slot_use: std::cell::Cell::new(false),
            latched_bailout: std::cell::RefCell::new(None),
            prim_slot_offsets: {
                // Same arithmetic as `planned_slot_off`: colour `c` lives at
                // `[rbp - (first_spill + 8c)]`. An offset past `i16` is dropped
                // rather than truncated -- the map's own slot list has the same
                // bound, and a silently wrong offset would accuse the wrong
                // slot.
                let mut offs: Vec<i16> = slot_plan
                    .prim_colors()
                    .into_iter()
                    .filter_map(|c| {
                        i32::try_from(u64::from(c).saturating_mul(8))
                            .ok()
                            .and_then(|d| first_spill.checked_add(d))
                            .and_then(|o| i16::try_from(o).ok())
                    })
                    .collect();
                offs.sort_unstable();
                offs
            },
            prim_colour_claim,
            prim_colour_clear_bytes: (0, 0),
            pinned_prim_slot_offsets,
            deopt_image_claim,
            helper_area_claim,
            frame_clear_bytes: (0, 0),
            snapshot_range_osr_bci: std::cell::Cell::new(None),
            slot_plan,
            dead_ref_clear: None,
            dead_ref_clear_site: None,
            dead_ref_clear_site_args: 0,
            dead_ref_clear_stores: false,
            frame_liveness_on: false,
            dead_home_clears: HashMap::new(),
            loop_entry_dead_home_clears: HashMap::new(),
            dead_home_clears_complete: false,
            prefix_slow_path_clears: false,
            alloc_site_clears: false,
            other_site_clears: false,
            poll_edge_pos: Vec::new(),
            poll_site_dense: false,
            ref_param_home_zero: Vec::new(),
            lazy_param_homes: Vec::new(),
            shadow_live_param_homes: Vec::new(),
            shadow_live_dbg: false,
            block_lazy_clear_allowed: false,
            block_lazy_clear: Vec::new(),
            block_lazy_writers: Vec::new(),
            own_home_written: Vec::new(),
            lazy_ref_home: Vec::new(),
            lazy_ref_sites: HashMap::new(),
            lazy_hook_regions: Vec::new(),
            lazy_ref_planned: 0,
            lazy_ref_spill_stores: 0,
            ref_def_home_stores: 0,
            home_keep_census: [0; HOME_KEEP_CLASSES],
            flag_store_elided: Vec::new(),
            home_keep_freq: [0.0; HOME_KEEP_CLASSES],
            home_keep_shapes: HashMap::new(),
            lazy_ref_blocked_by_calls: 0,
            lazy_ref_blocked_other: 0,
            pending_call_clears: None,
            pending_arg_stage_clears: false,
            alloc_dead_home_clear: false,
            helper_site_dead_home_clear: false,
            owned_args_bytecode: None,
            keep_set_chain_only_pcs: std::collections::HashSet::new(),
            direct_call_args_owned: false,
            direct_call_arg_window: Vec::new(),
            direct_call_service_copies: Vec::new(),
            service_arg_copies: Vec::new(),
            back_edge_dead_home_clear: false,
            dead_home_plan_converged: false,
            frame_liveness_covered: Vec::new(),
            frame_liveness_records: Vec::new(),
            frame_liveness_poisoned: false,
            spill_high_water: first_spill,
            block_offsets: vec![0; schedule.blocks.len()],
            branch_patches: Vec::new(),
            switch_tables: Vec::new(),
            switch_table_patches: Vec::new(),
            num_params,
            num_locals,
            frame_size,
            bci_native: HashMap::new(),
            bci_anchor_value_blocks: true.then(|| bci_anchor_value_blocks(graph, schedule)),
            bci_native_dominated: HashMap::new(),
            snapshot_native: HashMap::new(),
            cur_node_frame: None,
            cur_bci: 0,
            deopt_stub_patches: Vec::new(),
            deopt_pads: Vec::new(),
            const_imm_on: true,
            publish_at_def_on: ir_publish_at_def_enabled(),
            alu_imm_on: ir_alu_imm_enabled(),
            fused_branch_on: true,
            fallthrough_on: true,
            phi_copy_regs_on: ir_phi_copy_regs_enabled(),
            receiver_cse_on: true,
            ref_residency_on: ir_ref_residency_enabled(),
            inline_prim_putfield_on: true,
            reloc_emit_on: true,
            dbg_ir_reloc_on: false,
            volatile_load_fence_on: crate::runtime_lowering::volatile_load_fence_enabled(),
            ref_node_ids: graph
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.ty == IrType::Ref)
                .map(|(id, _)| id)
                .collect(),
            sp_map_seen: Vec::new(),
            rcx_twin_on: true,
            poison_int_high_on: false,
            rcx_twin: None,
            rcx_twins: 0,
            transfer_bcis: std::collections::HashSet::new(),
            by_bci_transfer_bcis: std::collections::HashSet::new(),
            by_bci_transfer_why: std::collections::HashMap::new(),
            table_transfer_bcis: std::collections::HashSet::new(),
            chain_transfer_pcs: std::collections::HashSet::new(),
            deopt_boxes: Vec::new(),
            vthread_boundaries_emitted: Vec::new(),
            trap_cause_points: Vec::new(),
            invoke_dispatch: helpers.invoke_dispatch,
            lambda_int_to_double: helpers.lambda_int_to_double,
            dispatch_threw: helpers.dispatch_threw,
            self_call_stack_guard: helpers.self_call_stack_guard,
            frem: helpers.jit_frem,
            drem: helpers.jit_drem,
            getfield: helpers.getfield,
            putfield_int: helpers.putfield_int,
            putfield_object: helpers.putfield_object,
            aastore: helpers.aastore,
            aastore_type_check: helpers.aastore_type_check,
            aaload: helpers.aaload,
            check_elision: crate::ir_check_elim::CheckElision::default(),
            putfield_long: helpers.putfield_long,
            putfield_float: helpers.putfield_float,
            putfield_double: helpers.putfield_double,
            new_object: helpers.new_object,
            // Casts: both are byte offsets inside `JvmThread`, far below i32::MAX.
            tlab_cursor_off: helpers.tlab_cursor_offset_in_thread as i32,
            tlab_end_off: helpers.tlab_end_offset_in_thread as i32,
            tlab_post_init: helpers.tlab_post_init,
            tlab_tls_thread_plans: 0,
            tlab_frame_thread_plans: 0,
            newarray: helpers.newarray,
            anewarray_object: helpers.anewarray_object,
            multianewarray_n: helpers.multianewarray_n,
            new_object_cp: helpers.new_object_cp,
            monitor_enter: helpers.monitor_enter,
            monitor_exit: helpers.monitor_exit,
            monitor_block_off: helpers.monitor_block_offset_in_thread,
            ldc_string_cp: helpers.ldc_string_cp,
            ldc_class_cp: helpers.ldc_class_cp,
            getstatic: helpers.getstatic,
            putstatic: [
                helpers.putstatic_int,
                helpers.putstatic_long,
                helpers.putstatic_float,
                helpers.putstatic_double,
                helpers.putstatic_object,
            ],
            instanceof_check: helpers.instanceof_check,
            checkcast: helpers.checkcast,
            throw_exception: helpers.throw_exception,
            set_throw_bci: helpers.set_throw_bci,
            safepoint_flag_addr: helpers.safepoint_flag_addr,
            safepoint_slow_path: helpers.safepoint_slow_path,
            needs_context,
            context_slot_off,
            sp_id_slot_off,
            shadow_thread_slot_off,
            ref_param_homes,
            shadow_savebase_slot_off,
            shadow_savetop_slot_off,
            phi_copy_scratch_slot_off,
            get_current_thread: helpers.get_current_thread,
            shadow_off_in_thread: helpers.shadow_stack_offset_in_thread as i32,
            stack_floor_off_in_thread: crate::inline_cache_pic::ic_stack_floor_word_offset(helpers)
                .unwrap_or(0),
            pending_shadow: Vec::new(),
            thread_fetch_span: None,
            savetop_restore_spans: Vec::new(),
            shadow_pushed_any: false,
            frame_block: Vec::new(),
            frame_block_shared: false,
            defer_frame_block_ensure: false,
            frame_block_call_patches: Vec::new(),
            frame_block_stub_at: None,
            compact_fields: compact_fields.clone(),
            read_bounds_addr: helpers.read_bounds_addr,
            ref_store_gates: crate::x64::ref_store_gates_of(helpers),
            zgc_fused_gate: crate::x64::zgc_fused_ref_store_gate_of(helpers),
            zgc_fresh_stores: if (crate::x64::zgc_fused_ref_store_gate_of(helpers).is_some()
                && crate::x64::zgc_fresh_receiver_store_enabled())
                || gen_fresh_store_no_pre_gate_for(helpers)
                || (ir_g1_ref_store_enabled()
                    && crate::x64::g1_inline_barrier_enabled()
                    && crate::x64::g1_barrier_table_live(helpers.g1_barrier_addr)
                    && helpers.putfield_object != 0)
            {
                let fresh = zgc_fresh_receiver_stores_logged(graph, schedule, |v| {
                    sr_map.is_some_and(|sr| sr.objects.contains_key(&v))
                });
                if crate::x64::dbg_jitc_enabled() {
                    let materialized_values = fresh
                        .iter()
                        .filter(|&&id| {
                            graph
                                .nodes
                                .get(id as usize)
                                .and_then(|n| n.inputs.get(4))
                                .is_some_and(|v| {
                                    sr_map.is_some_and(|sr| sr.objects.contains_key(v))
                                })
                        })
                        .count();
                    let materialized_windows = fresh
                        .iter()
                        .filter_map(|&id| {
                            let n = graph.nodes.get(id as usize)?;
                            let value = n.inputs.get(4)?;
                            sr_map
                                .is_some_and(|sr| sr.objects.contains_key(value))
                                .then(|| n.inputs[2])
                        })
                        .collect::<std::collections::HashSet<_>>()
                        .len();
                    eprintln!("[cratonvm-jitc] zgc fresh-receiver census: stores={} scalar-replaced-values={materialized_values} scalar-replaced-value-windows={materialized_windows}", fresh.len());
                }
                fresh
            } else {
                std::collections::HashSet::new()
            },
            g1_barrier_addr: helpers.g1_barrier_addr,
            // Cast: a small header constant.
            concat_class_id_off: helpers.class_id_offset_in_obj as i32,
            concat_region_bounds_addr: helpers.region_bounds_addr,
            satb_pre_gate: helpers.ref_store_pre_gate,
            g1_post_write_barrier: helpers.g1_post_write_barrier,
            card_write_barrier: helpers.write_barrier,
            ref_store_post_skip_mask: crate::x64::ref_store_post_skip_mask_of(helpers),
            ir_card_view: crate::x64::gen_card_view_of(helpers),
            shadow_pushes: 0,
            shadow_reloads: 0,
            locals_size,
            first_spill,
            oop_maps: Vec::new(),
            defined_nodes: vec![false; graph.nodes.len()],
            next_sp_id: 1,
            sp_id_bcis: Vec::new(),
            ir_monitor_homes: Vec::new(),
            return_site_rows: Vec::new(),
            defer_next_sp_id_store: false,
            deferred_self_call_sp_id: None,
            self_call_id_stores_deferred: 0,
            self_call_sample_dominated: Vec::new(),
            self_call_skip_sample: false,
            self_call_sentinel_lean_on: crate::self_call_sentinel_lean_enabled(),
            self_call_result_narrow: false,
            service_lean_bit_pending: false,
            direct_call_sentinel_lean_on: crate::direct_call_sentinel_lean_enabled(),
            direct_call_lean_site: false,
            self_call_sites_planned: 0,
            self_call_sites_lowered: 0,
            self_call_samples_elided: 0,
            entry_fast_return: None,
            entry_fold: None,
            self_call_answer: None,
            param_register_fills: Vec::new(),
            deferred_self_call_tails: Vec::new(),
            deferred_zgc_alloc_tails: Vec::new(),
            deferred_getfield_tails: Vec::new(),
            deferred_putfield_tails: Vec::new(),
            deferred_gen_alloc_tails: Vec::new(),
            deferred_checkcast_tails: Vec::new(),
            preheader_epoch_guards: HashMap::new(),
            cross_call_cold_tails: (0, 0),
            ic_hit_exit_census: (0, 0),
            args_stage_top_off,
            args_stage_words: max_call_args,
            spill_cap_off,
            deopt_regs_base,
            deopt_regs_bytes,
            saved_xmm_bytes,
            saved_xmm_file: if needs.vector_xmm_saves {
                IR_LOWER_SAVED_XMMS_VECTOR
            } else {
                IR_LOWER_SAVED_XMMS
            },
            saved_gpr_bytes,
            save_bands_packed: false,
            call_exc_patches: Vec::new(),
            protected_ranges: Vec::new(),
            osr_handler_reachable: Some(Vec::new()),
            rethrow_pads: Vec::new(),
            local_handler_lookup: helpers.local_handler_lookup,
            catch_entry_on: crate::ir::ir_compiled_handler_entry_enabled(),
            cur_catch_arm: None,
            catch_probes: Vec::new(),
            catch_site_boxes: Vec::new(),
            catch_site_index: HashMap::new(),
            rethrow_points: Vec::new(),
            propagating_exit_holds_monitor: false,
            self_lock: graph.method_self_lock.clone(),
            self_lock_unheld_exc_patches: Vec::new(),
            classified_guard_exits: 0,
            deopt_stub_off: None,
            poll_mode_exits: IrPollModeExits::Off,
            optimizer_dropped_snapshot_values: false,
            mode_exit_census: [0; 9],
            poll_exit_boxes: Vec::new(),
            poll_exit_pads: Vec::new(),
            promoted_pending: std::cell::OnceCell::new(),
            pad_writebacks: None,
            pad_writebacks_attached: 0,
            deopt_pad_writebacks: HashMap::new(),
            covered_pad_points: std::collections::HashSet::new(),
            pending_ls_range: Vec::new(),
            pending_ls_pos: Vec::new(),
            poll_exit_bytecode: None,
            poll_exit_liveness: std::cell::OnceCell::new(),
            outlined_polls: Vec::new(),
            div_cold_fixups: Vec::new(),
            exit_poll_sites: Vec::new(),
            post_call_exits_pending: Vec::new(),
            post_call_exit_sites: Vec::new(),
            self_call_patches: Vec::new(),
            direct_calls,
            ic_slots,
            cycle_cells: HashMap::new(),
            tlab_announce_classes: std::collections::HashSet::new(),
            array_list_layout: None,
            string_layout: None,
            call_prefix_join: None,
            zgc_note_tlab_object: helpers.zgc_note_tlab_object,
            invoke_virtual_mic: helpers.invoke_virtual_mic,
            frame_record: helpers.frame_record,
            // Reserved here, at the start of this compilation: the prologue has
            // to encode the id as an immediate, and the `CompiledMethod` that
            // will own it does not exist until this buffer is filled.
            inline_cm_tls_disp: if crate::x64::inline_rbp_tls_disp() != 0 {
                crate::x64::inline_cm_tls_disp()
            } else {
                0
            },
            compile_id: if crate::x64::inline_rbp_tls_disp() != 0
                && crate::x64::inline_cm_tls_disp() != 0
            {
                crate::CompileIdReservation::reserve()
            } else {
                crate::CompileIdReservation::none()
            },
            service_callee_deopt: helpers.service_callee_deopt,
            branch_hints,
            spliced_ranges,
            sr_map,
            safepoint_bcis: graph
                .safepoints
                .iter()
                .filter(|s| !s.is_splice_state())
                .map(|s| s.bci)
                .collect(),
            safepoint_first_by_bci: Self::first_safepoint_index_by_bci(graph),
            splice_state_first_by_pc: Self::first_splice_state_index_by_pc(graph),
            post_call_splice_state_by_call: Self::post_call_splice_states_by_call(graph),
            post_call_chain_sites: 0,
            spliced_header_poll_exits: 0,
            poll_exits_naming_virtuals: 0,
            splice_chain_points: 0,
            splice_chain_fallbacks: 0,
            sync_window_nodes: std::cell::OnceCell::new(),
            sync_window_flat_sites: 0,
            chain_site_candidates_by_pc: std::cell::OnceCell::new(),
            forced_chain_deopts: crate::ir::ir_forced_chain_deopts_enabled(),
            forced_chain_deopt_counters: Vec::new(),
            forced_chain_deopt_points: Vec::new(),
            poll_stress_period: dbg_poll_stress().0,
            poll_stress_seed: dbg_poll_stress().1,
            poll_stress_cells: Vec::new(),
            dead_home_poison: dbg_dead_home_poison_enabled(),
            guard_bcis: graph
                .nodes
                .iter()
                .filter_map(|n| match n.op {
                    Op::Guard { bci } => Some(bci),
                    _ => None,
                })
                .collect(),
            deopt_sites_by_bci: Self::index_deopt_sites(graph, spliced_ranges),
            live_monitor_operands: graph
                .nodes
                .iter()
                .filter(|n| matches!(n.op, Op::MonitorEnter | Op::MonitorExit))
                .filter_map(|n| n.input_opt(2))
                .collect(),
            phis_by_merge: {
                let mut by_merge: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
                for (id, n) in graph.nodes.iter().enumerate() {
                    // The exact filter `gather_phi_copies` applied per edge.
                    if !matches!(n.op, Op::Phi)
                        || matches!(n.ty, IrType::Memory | IrType::Control | IrType::Void)
                    {
                        continue;
                    }
                    if let Some(&merge) = n.inputs.first() {
                        by_merge.entry(merge).or_default().push(id as NodeId);
                    }
                }
                by_merge
            },
            inline_frame_sites,
            inline_frame_rows: Vec::new(),
            row_census: None,
            inline_frame_sp_id_rows: Vec::new(),
            sp_chain_pc_override: None,
            npe_trap_sites: Vec::new(),
            npe_trap_next_id: 0,
            cur_node_pc: None,
            // Off by default; `lower_inner_with_array_list` installs a plan when
            // `CRATONVM_JIT_IR_LINEAR_SCAN` is on. Empty vectors, not
            // node-sized ones: `resident_xmm` reads through `get`, so an
            // absent plan costs one bounds check and no allocation.
            reg_of: Vec::new(),
            reg_live: Vec::new(),
            poll_fp_reloads: HashMap::new(),
            gp_reg_of: Vec::new(),
            gp_reg_owner: [None; 16],
            gp_zx32_mask: 0,
            gp_reg_live: Vec::new(),
            ls_caller_saved_on: false,
            ls_caller_saved_values: 0,
            gp_home_reloads: 0,
            gp_reg_reads: 0,
            gp_home_reloads_ref: 0,
            gp_home_reloads_assigned: 0,
            gp_home_reloads_unassigned: 0,
            cs_republish_after: HashMap::new(),
            cs_republishes: 0,
            cs_republish_skipped: 0,
            // Split residency: empty unless `CRATONVM_JIT_IR_LS_SPLITS` is on
            // and the plan placed something. An empty `ls_at` makes
            // `ls_apply_transitions` a bounds check per node and nothing else.
            ls_at: Vec::new(),
            ls_block_start: Vec::new(),
            ls_managed: Vec::new(),
            ls_origin: (Vec::new(), Vec::new()),
            ls_saved_fp: vector_saved_fp(needs.vector_xmm_saves),
            ls_saved_gp: [false; 16],
            ls_split_emitted: [0; 4],
            ls_edge_copies: Vec::new(),
            ls_edge_copies_done: Vec::new(),
            ls_edge_loads: 0,
            phi_copy_reg_reads: 0,
            phi_copy_const_imms: 0,
            const_homes_dropped: 0,
            alu_reg_operands: 0,
            prologue_zero_rax: false,
            frame_prezeroed: false,
            fresh_clear_skip: Vec::new(),
            prologue_zero_stretch: false,
            zero_rax_end: None,
            home_reload_forward_on: ir_home_reload_forward_enabled(),
            operand_in_place_on: ir_operand_in_place_enabled(),
            operand_in_place_cmps: 0,
            operand_in_place_alus: 0,
            rax_home_mirror: None,
            publish_reload_elide_on: ir_publish_reload_elide_enabled(),
            rax_reg_mirror: None,
            publish_reload_elides: Vec::new(),
            publish_reload_elided: Vec::new(),
            publish_reload_excluded: Vec::new(),
            publish_reload_refused: 0,
            home_reload_forwards: Vec::new(),
            phi_copy_reg_publishes: 0,
            phi_copy_publish_deferred: 0,
            phi_home_publish_skipped: 0,
            phi_home_publish_refused: 0,
            fp_phi_reg_publishes: 0,
            fp_phi_reg_stores: 0,
            fp_in_place_ops: 0,
            cur_def: None,
            synthetic_store_row: None,
            cur_def_published: false,
            reg_publishes_at_def: 0,
            homes_dropped_at_def: 0,
            carry_of: Vec::new(),
            live_carry: None,
            deferred_rcx: None,
            index_zx_at: None,
            vec_plans: None,
            vec_loops_emitted: 0,
            carry_deferred: Vec::new(),
            carry_deferred_planned: 0,
            carry_deferred_read: 0,
            deferred_skips: [0; 8],
            deferred_mid_foldable: 0,
            deferred_candidates: 0,
            rcx_writes: 0,
            cmp_in_place: 0,
            cmp_in_place_frame: 0,
            cmp_imm: 0,
            cmp_imm_frame: 0,
            add_lea: 0,
            add_lea_acc: 0,
            carries_taken: 0,
            carries_read: 0,
            carries_refused: 0,
            carry_stores_dropped: 0,
            elide_unnamed_homes_on: false,
            elide_reach: Vec::new(),
            elide_held: Vec::new(),
            tail_carry_of: Vec::new(),
            tail_carry_seen: Vec::new(),
            tail_carry: None,
            cur_term: None,
            tail_carry_points: Vec::new(),
            site_named: Vec::new(),
            site_named_count: 0,
            site_live: None,
            building_table_points: std::cell::Cell::new(false),
            post_collect_ref_mask: std::cell::Cell::new(None),
            table_point_bci: std::cell::Cell::new(None),
            spill_at_inspection_on: false,
            site_spill_bcis: std::collections::HashSet::new(),
            site_spill_named: HashMap::new(),
            site_spills: HashMap::new(),
            cur_lower_block: 0,
            inspection_spills_emitted: 0,
            inspection_spills_deduped: 0,
            site_named_loop_on: false,
            poll_slow_spills: Vec::new(),
            outlined_poll_spills: HashMap::new(),
            outlined_poll_stw_patches: HashMap::new(),
            page_poll_sites: Vec::new(),
            page_polls_outlined: Vec::new(),
            page_polls_outlined_paired: Vec::new(),
            cha_leaf_classes: graph.cha_leaf_classes.clone(),
            fallthrough_poll_block: None,
            poll_slow_spills_emitted: 0,
            site_named_phis: 0,
            reg_alloc_on: false,
            reg_alloc_coalesced_named: 0,
            reg_alloc_publish_at: None,
            reg_alloc_routed: Vec::new(),
            reg_alloc_direct_defs: 0,
            reg_alloc_kept_phi_stages: 0,
            home_keep_why: Vec::new(),
            home_why_static: [0; HK_N],
            home_why_weighted: [0.0; HK_N],
            phi_edge_home_stores: 0,
            phi_edge_home_weighted: 0.0,
            elide_excluded: Vec::new(),
            tail_carry_site_ends: Vec::new(),
            post_call_exit_backs: Vec::new(),
            fused_cmp_unnamed: Vec::new(),
            def_home_stores: 0,
            tail_carries_planned: 0,
            tail_prefix_kept: 0,
            tail_backedge_skipped: 0,
            prefix_rax_proofs: Vec::new(),
            cmp_unnamed_refused: [0; 4],
            tail_stores_elided: 0,
            tail_carries_refused: 0,
            fused_cmp_unnamed_count: 0,
            graph_trap_free: std::cell::OnceCell::new(),
            mask_proofs: std::cell::OnceCell::new(),
            unreachable_homes: 0,
            unreachable_points_skipped: std::cell::Cell::new(0),
            alu_imms_folded: 0,
            carry_skips: [0; 6],
            carry_named: 0,
            deopt_nameable: Vec::new(),
            phi_coalesced_src: Vec::new(),
            phi_coalesced: 0,
            deopt_reg_named: std::cell::Cell::new(0),
            quiet_dropped_home: std::cell::Cell::new(false),
            home_dropped: Vec::new(),
            osr_entries: Vec::new(),
            osr_entry_kinds: Vec::new(),
            osr_opaque_entry_bcis: Vec::new(),
            osr_refusals: Vec::new(),
            home_stores_dropped: 0,
            home_read_refusals: std::cell::Cell::new(0),
            token_read_guard: ir_lower_token_read_guard_enabled(),
            token_only_inputs: Vec::new(),
            token_read_nodes: 0,
            token_read_refusals: std::cell::Cell::new(0),
            token_read_pinned_hits: std::cell::Cell::new(0),
            token_scope: TokenScope::Data,
            token_scope_reader: NO_NODE,
            token_scope_nodes: [0; 3],
            token_scope_refusals: std::cell::Cell::new([0; 3]),
            token_scope_pinned_hits: std::cell::Cell::new([0; 3]),
            token_guard_named: Vec::new(),
            token_guard_point_range: ir_lower_token_read_guard_point_range_enabled(),
            token_point_stale: std::cell::Cell::new(0),
            deopt_point_range_check: ir_deopt_point_range_check_enabled(),
            deopt_point_stale: std::cell::Cell::new(0),
            use_count: Vec::new(),
            fused_cmp: Vec::new(),
            typecheck_branch_pending: None,
            fused_lcmp: Vec::new(),
            fused_fcmp: Vec::new(),
            fused_test_and: Vec::new(),
            fused_i2l: Vec::new(),
            test_fused_cond: Vec::new(),
            test_and_remat: Vec::new(),
            test_guard_cond: Vec::new(),
            deopt_pad_remats: HashMap::new(),
            and_zero_guard_planned: 0,
            and_zero_widened: 0,
            deopt_named: Vec::new(),
            deopt_named_reachable: Vec::new(),
            carried_homes_dropped: 0,
            guarded_receivers: Vec::new(),
            block_inline_field_sites: 0,
            layout_proven: Vec::new(),
            // JIT round 25 (item 4): the proven-receiver switch brings the
            // deopting layout guard with it -- an elided epoch compare is half
            // of what a proven receiver's later reads skip.
            layout_guard_deopt_on: ir_layout_guard_deopt_enabled()
                || ir_proven_receiver_fields_enabled(),
            layout_guard_cross_block: (ir_layout_guard_deopt_enabled()
                || ir_proven_receiver_fields_enabled())
                && !schedule_block_cfg_has_cycle(schedule),
            layout_guard_lone_pcs: if ir_layout_guard_deopt_enabled()
                || ir_proven_receiver_fields_enabled()
            {
                let mut lone = layout_guard_lone_pcs(graph);
                lone.extend(pure_once_shared_pcs(graph, schedule));
                lone
            } else {
                std::collections::HashSet::new()
            },
            layout_guard_lone_sites: ((ir_layout_guard_deopt_enabled()
                || ir_proven_receiver_fields_enabled())
                && ir_layout_guard_lone_per_frame_enabled())
            .then(|| layout_guard_lone_sites(graph)),
            proven_fields_on: ir_proven_receiver_fields_enabled(),
            compact_proven: Vec::new(),
            proven_field_census: ProvenFieldCensus::default(),
            proven_spliced_on: ir_proven_receiver_fields_enabled(),
            proven_scope_on: ir_proven_receiver_fields_enabled()
                && ir_proven_receiver_fields_dominance_enabled(),
            proven_preheader_on: ir_proven_receiver_fields_enabled()
                && ir_proven_receiver_fields_dominance_enabled()
                && ir_proven_receiver_fields_preheader_enabled(),
            proven_catch_arms_on: ir_proven_receiver_fields_enabled()
                && ir_proven_receiver_fields_dominance_enabled(),
            field_proof_cycle_blocks: std::cell::OnceCell::new(),
            field_proof_entries: std::cell::OnceCell::new(),
            field_proof_scope_cache: std::cell::RefCell::new(HashMap::new()),
            field_proof_osr_obligations: std::cell::RefCell::new(Vec::new()),
            field_proof_canon_cache: std::cell::RefCell::new(HashMap::new()),
            proven_coestablish_on: ir_proven_receiver_fields_enabled()
                && ir_proven_receiver_fields_coestablish_enabled(),
            null_proven_receivers: Vec::new(),
            unbox_class_proven: Vec::new(),
            checkcast_null_or_exact_proven: Vec::new(),
            unsigned_byte_array_loads: Vec::new(),
            elided_byte_mask_ands: HashMap::new(),
            fused_index_and: Vec::new(),
            extra_dead_add_remats: HashMap::new(),
            extra_pad_remats: HashMap::new(),
            block_cse: std::cell::Cell::new(crate::ir_lower::block_cse::BlockCseState::default()),
            scoped_screened_bases: Vec::new(),
            ls_spills: 0,
            ls_reloads: 0,
            mir: None,
            value_flow: crate::regalloc::ValueFlow::default(),
            mir_mode: MirMode::Off,
            mir_tiles: 0,
            mir_mismatches: 0,
            mir_shadow_tiles: 0,
            mir_arm_bytes: 0,
            mir_enc_bytes: 0,
            call_throw_plans: HashMap::new(),
            call_throw_entry_blocks: std::collections::HashSet::new(),
            unfenced_fresh_stores: None,
            fence_sink_in: None,
            fence_pending: false,
            fence_sink_deferred_any: false,
            cur_call_throw: None,
            call_throw_publication: None,
            call_throw_stack_guard_exit: false,
            leaf_entry: LeafEntryState::default(),
            tail_sink: TailRepublishSink::default(),
            call_throw_pads: Vec::new(),
            call_throw_sites: Vec::new(),
            call_throw_pads_emitted: 0,
            implicit_throw_plans: HashMap::new(),
            throw_aioobe: helpers.throw_aioobe,
            throw_arithmetic: helpers.throw_arithmetic,
            npe_with_action: helpers.jit_npe_with_action,
            implicit_throw_pads_emitted: 0,
        }
    }

    /// JIT round 23 wave 10 (lane L7, [`ir_deopt_name_past_preloop_enabled`]):
    /// the values that own their register for every program point a frame
    /// state can name them at, although `holders` counts more than one value
    /// in it.
    ///
    /// The register `r` is shared, but every other holder is a PRE-LOOP value
    /// relative to the one in-loop holder `v`:
    ///
    /// * `v` (not a coalesced back-edge source) is the only holder of `r`
    ///   whose definition block is inside a loop, and that block's loop
    ///   (depth >= 1) is entered from blocks the others live in;
    /// * every other holder `u` (coalesced back-edge sources aside, which
    ///   share `v`'s register by construction) is defined in a block outside
    ///   every loop (depth 0) that dominates `v`'s block, and every reader of
    ///   `u` is in such a block too -- or is a loop phi reading `u` on an edge
    ///   from such a block.
    ///
    /// So each `u` is written and read only before control first reaches
    /// `v`'s block, and a depth-0 block that dominates it never runs again
    /// after it: from `v`'s definition on, `r` holds `v` (or, for a loop phi,
    /// its coalesced back-edge source, the value the phi takes next) at every
    /// position, and a frame state, which can name `v` only at or after its
    /// definition, finds `v` there. Managed (split) registers stay poisoned
    /// (`usize::MAX`) and are never considered.
    pub(in crate::ir_lower) fn preloop_exclusive_owners(
        &self,
        holders: &[usize; 16],
    ) -> Vec<usize> {
        let schedule = self.schedule;
        let graph = self.graph;
        let (depth, _) = crate::ir_schedule::loop_nest(&schedule.blocks, &schedule.dom);
        let block_of = |id: usize| -> Option<usize> {
            schedule
                .node_to_block
                .get(id)
                .copied()
                .filter(|&b| b < schedule.blocks.len())
        };
        // Every value reading `u` (data inputs), with the slot it reads in.
        let mut users: Vec<Vec<(usize, usize)>> = vec![Vec::new(); graph.nodes.len()];
        for (uid, n) in graph.nodes.iter().enumerate() {
            if n.op == Op::Dead {
                continue;
            }
            for (k, &inp) in n.inputs.iter().enumerate() {
                if let Some(v) = users.get_mut(inp as usize) {
                    v.push((uid, k));
                }
            }
        }
        let mut out = Vec::new();
        for reg in 0..16u8 {
            let count = holders.get(reg as usize).copied().unwrap_or(0);
            if count < 2 || count == usize::MAX || !IR_LOWER_LS_GPRS.contains(&reg) {
                continue;
            }
            let owners: Vec<usize> = (0..self.gp_reg_of.len())
                .filter(|&id| self.gp_reg_of[id] == Some(reg))
                .filter(|&id| !self.phi_coalesced_src.get(id).copied().unwrap_or(false))
                .collect();
            let in_loop: Vec<usize> = owners
                .iter()
                .copied()
                .filter(|&id| block_of(id).is_none_or(|b| depth.get(b).copied().unwrap_or(1) > 0))
                .collect();
            let [v] = in_loop.as_slice() else {
                continue;
            };
            let Some(bv) = block_of(*v) else {
                continue;
            };
            let preloop_block = |b: usize| {
                depth.get(b).copied().unwrap_or(1) == 0
                    && b != bv
                    && schedule.dom.is_reachable(b)
                    && schedule.dom.dominates(b, bv)
            };
            let all_preloop = owners.iter().filter(|&&u| u != *v).all(|&u| {
                let Some(bu) = block_of(u) else {
                    return false;
                };
                if !preloop_block(bu) {
                    return false;
                }
                users.get(u).is_some_and(|us| {
                    us.iter().all(|&(uid, k)| {
                        let un = &graph.nodes[uid];
                        if matches!(un.op, Op::Phi) {
                            // Read on the edge from the merge's control input
                            // `k - 1`: that edge's block must be pre-loop.
                            return k >= 1
                                && un
                                    .inputs
                                    .first()
                                    .and_then(|&m| graph.nodes.get(m as usize))
                                    .and_then(|m| m.inputs.get(k - 1).copied())
                                    .and_then(|c| block_of(c as usize))
                                    .is_some_and(preloop_block);
                        }
                        block_of(uid).is_some_and(preloop_block)
                    })
                })
            });
            if !all_preloop {
                continue;
            }
            out.push(*v);
            // The phi's coalesced back-edge source shares `reg` by design and
            // holds the phi's next value: nameable with it.
            for id in 0..self.gp_reg_of.len() {
                if self.gp_reg_of[id] == Some(reg)
                    && self.phi_coalesced_src.get(id).copied().unwrap_or(false)
                {
                    out.push(id);
                }
            }
        }
        out
    }

    /// Install the linear-scan register plan. Called once, after construction
    /// and before any emission, only when the path is enabled.
    pub(in crate::ir_lower) fn set_residency(&mut self, residency: RegResidency) {
        self.reg_live = vec![false; residency.reg_of.len()];
        self.reg_of = residency.reg_of;
        self.poll_fp_reloads = residency.poll_fp_reloads;
        self.gp_reg_live = vec![false; residency.gp_reg_of.len()];
        self.gp_reg_owner = [None; 16];
        self.gp_zx32_mask = 0;
        self.gp_reg_of = residency.gp_reg_of;
        // ── The split plan, installed before anything reads the file ─────
        //
        // Order matters twice below: the save-set snapshot has to see every
        // register a managed value passes through, and the droppability pass
        // has to see `ls_managed` so it can refuse exactly that population.
        self.ls_at = residency.splits.at;
        self.ls_block_start = residency.splits.block_start;
        self.ls_managed = residency.splits.managed;
        self.ls_edge_copies_done = vec![false; residency.splits.edges.len()];
        self.ls_edge_copies = residency.splits.edges;
        // Round 16 wave 3 (lane codegen, CG16-6): a loop phi's back-edge
        // source into the phi's own register, before anything below snapshots
        // or counts `gp_reg_of`.
        self.coalesce_back_edge_phi_sources();
        // Snapshot the definition-time registers NOW. `gp_reg_of`/`reg_of`
        // still hold what the plan assigned at each definition; from the first
        // `ls_apply_transitions` onward they hold what the value has moved to.
        self.ls_origin = (self.gp_reg_of.clone(), self.reg_of.clone());
        // The registers this frame owes its caller, frozen now. Both halves:
        // what the plan assigned for a whole range, and every register any
        // managed value visits. See the field comment for the failure a
        // prologue and an epilogue that disagree about this produce.
        //
        // Rebuilt rather than accumulated: this function's contract is "called
        // once", and a snapshot that ORed a second plan onto a first would keep
        // saving a register no plan hands out — harmless in this direction, but
        // it would make the snapshot stop being a function of the plan, which
        // is the property the tests below rest on.
        // VEC21-7: the vector registers a pre-loop may take are saved as if
        // the plan handed them out.
        self.ls_saved_fp = vector_saved_fp(self.saved_xmm_file == IR_LOWER_SAVED_XMMS_VECTOR);
        self.ls_saved_gp = [false; 16];
        for reg in self.reg_of.iter().flatten() {
            if let Some(cell) = self.ls_saved_fp.get_mut(*reg as usize) {
                *cell = true;
            }
        }
        for reg in self.gp_reg_of.iter().flatten() {
            if let Some(cell) = self.ls_saved_gp.get_mut(*reg as usize) {
                *cell = true;
            }
        }
        for transitions in &self.ls_at {
            for t in transitions {
                let Some((reg, gp)) = t.destination() else {
                    continue;
                };
                let bank = if gp {
                    &mut self.ls_saved_gp
                } else {
                    &mut self.ls_saved_fp
                };
                if let Some(cell) = bank.get_mut(reg as usize) {
                    *cell = true;
                }
            }
        }
        // And every register an edge-resolution copy writes (round 9 wave 6):
        // it is a write this frame performs, so the caller's value in it has
        // to be saved and restored like any other.
        for edge in &self.ls_edge_copies {
            for &(_, reg, gp) in &edge.copies {
                let bank = if gp {
                    &mut self.ls_saved_gp
                } else {
                    &mut self.ls_saved_fp
                };
                if let Some(cell) = bank.get_mut(reg as usize) {
                    *cell = true;
                }
            }
        }
        // Exclusive ownership, computed once: how many values share each
        // register, and a value is nameable only if the answer for its own is
        // one. See the field comment for why "resident" is not enough.
        self.deopt_nameable = vec![false; self.gp_reg_of.len()];
        self.home_dropped = vec![false; self.gp_reg_of.len()];
        // Filled at the bottom of this function, once `deopt_nameable` is
        // known. **Up front, not lazily at the first edge copy**: `slot_of` has
        // to refuse from the first instruction emitted, or a read that happens
        // to precede the copy would silently take the unwritten word.
        if ir_deopt_regs_enabled() {
            let mut holders = [0usize; 16];
            for (id, reg) in self.gp_reg_of.iter().enumerate() {
                let Some(reg) = reg else {
                    continue;
                };
                // CG16-6: a coalesced back-edge source shares its phi's
                // register and is not a second holder (see
                // `coalesce_back_edge_phi_sources` for why naming either one
                // by that register is right wherever a frame can name it).
                if self.phi_coalesced_src.get(id).copied().unwrap_or(false) {
                    continue;
                }
                if let Some(cell) = holders.get_mut(*reg as usize) {
                    *cell += 1;
                }
            }
            // A register a managed value passes through holds DIFFERENT values
            // at different positions by construction, so no value is its
            // exclusive owner and none of them may be named in a frame state.
            // Poisoning the count rather than filtering the owner is deliberate:
            // it also takes the register away from the UNMANAGED value that
            // happens to hold it for its whole range, which is the one a naive
            // "skip managed ids" filter would wrongly still call exclusive.
            for id in 0..self.ls_managed.len() {
                if !self.ls_managed[id] {
                    continue;
                }
                for reg in self.ls_split_registers(id as NodeId, true) {
                    if let Some(cell) = holders.get_mut(reg as usize) {
                        *cell = usize::MAX;
                    }
                }
            }
            for id in 0..self.gp_reg_of.len() {
                let Some(reg) = self.gp_reg_of[id] else {
                    continue;
                };
                // `IR_LOWER_LS_GPRS` and nothing else: `DEOPT_ARG0` is spilled
                // holding the point pointer rather than its trapping value
                // (see `emit_deopt_stub`), and no register outside the file can
                // be reasoned about here at all.
                if !IR_LOWER_LS_GPRS.contains(&reg) {
                    continue;
                }
                if holders.get(reg as usize).copied().unwrap_or(0) == 1 {
                    self.deopt_nameable[id] = true;
                }
            }
            // JIT round 23 wave 10 (lane L7, `CRATONVM_JIT_IR_DEOPT_NAME_PAST_PRELOOP`):
            // a register shared only with values that die before the loop.
            if ir_deopt_name_past_preloop_enabled() {
                for id in self.preloop_exclusive_owners(&holders) {
                    if let Some(cell) = self.deopt_nameable.get_mut(id) {
                        *cell = true;
                    }
                }
            }
        }
        // Which values a REACHABLE deopt can name. Computed before the
        // droppability pass below, which consults it.
        self.deopt_named_reachable = self.compute_deopt_named_reachable();
        // Two passes: the droppability predicates borrow `&self`.
        let droppable: Vec<usize> = (0..self.home_dropped.len())
            // A managed value's home is not optional. Its transitions RELOAD
            // from that word, and the reload is the whole mechanism — a dropped
            // home would make `ls_apply_transitions` read bytes nothing wrote,
            // which for an `IrType::Ref` is a fabricated object pointer handed
            // to the collector. The predicates below could answer `true` here
            // through either of their two widening clauses (a body that cannot
            // deopt, or a value no reachable deopt names), so the refusal is
            // stated as its own conjunct rather than left to them.
            .filter(|id| !self.ls_managed.get(*id).copied().unwrap_or(false))
            // Cast: an index into the node arena is a `NodeId`.
            .filter(|id| {
                let id = *id as NodeId;
                match self.graph.nodes.get(id as usize).map(|n| &n.op) {
                    Some(crate::ir::Op::Phi) => self.phi_home_droppable(id),
                    Some(_) => self.value_home_droppable(id),
                    None => false,
                }
            })
            .collect();
        for id in droppable {
            self.home_dropped[id] = true;
        }
    }

    /// Round 16 wave 3 (lane codegen, CG16-6;
    /// `r16-codegen-loop-and-straight-line-anatomy-FIXED-20261005.md`): give a loop
    /// phi's back-edge source the PHI's register, so `i = i + 1` is one
    /// `LEA r, [r + 1]` and the edge copy `MOV r_phi, r_src` (or, for a source
    /// the residency rules refused, its home store and reload) disappears.
    ///
    /// The single-range linear scan cannot do this itself: the phi's range is
    /// one interval over the linear layout, so a phi read after the loop (an
    /// accumulator returned at exit) occupies its register across the whole
    /// latch, and the scan hands the source another one. The argument here is
    /// the CFG's instead, and every clause is checked:
    ///
    /// * the source `v` is an `int`/`long` `Add`/`Sub` read exactly once -- by
    ///   this phi, on the edge from `v`'s own block `B`, which has no
    ///   terminator and ONE successor, the phi's header, and feeds exactly one
    ///   merge input. So from `v`'s definition every path runs the rest of `B`
    ///   and then the edge copy, which makes `v` the phi again;
    /// * no node of `B` after `v` reads the phi, and no other phi of the header
    ///   reads it on that edge: the phi is dead from `v`'s definition on, so
    ///   its register is free for `v` there. The arms that define an
    ///   `Add`/`Sub` read every operand before they write the result's
    ///   register (`LEA dst, [..]`, or the accumulator in RAX and then the
    ///   publish), so `v` may read the phi it replaces;
    /// * every node of `B` after `v` is `op_cannot_deopt`: no deopt frame can
    ///   name the phi while its register holds `v`. The back-edge poll's mode
    ///   exit names the header's phis by their edge sources
    ///   (`back_edge_mode_exit_state`), i.e. `v`, which IS in that register;
    /// * no other value assigned the register is live anywhere in `v`'s range
    ///   (`slot_plan.range`, the colourer's model the scan agreed with), and no
    ///   other phi holds it (a phi's register is also written by edge copies
    ///   outside its range);
    /// * no split residency, no edge-resolution copies, no MIR mode.
    ///
    /// The pair is then ONE holder of the register for `deopt_nameable`, which
    /// is what keeps both homes droppable where they were.
    pub(in crate::ir_lower) fn coalesce_back_edge_phi_sources(&mut self) {
        let n = self.gp_reg_of.len();
        self.phi_coalesced_src = vec![false; n];
        self.phi_coalesced = 0;
        if !ir_phi_coalesce_back_edge_enabled(self.graph)
            || self.mir_mode != MirMode::Off
            || self.ls_managed.iter().any(|&m| m)
            || !self.ls_edge_copies.is_empty()
        {
            return;
        }
        let graph: &'a Graph = self.graph;
        let mut use_count = vec![0u32; graph.nodes.len()];
        for node in &graph.nodes {
            for &input in &node.inputs {
                if input != NO_NODE {
                    if let Some(c) = use_count.get_mut(input as usize) {
                        *c = c.saturating_add(1);
                    }
                }
            }
        }
        for p in 0..graph.nodes.len().min(n) {
            let Some(pn) = graph.nodes.get(p) else {
                continue;
            };
            if pn.op != Op::Phi || !matches!(pn.ty, IrType::Int | IrType::Long) {
                continue;
            }
            let Some(reg) = self.gp_reg_of.get(p).copied().flatten() else {
                continue;
            };
            if !IR_LOWER_LS_GPRS.contains(&reg) {
                continue;
            }
            let Some(&merge_ctrl) = pn.inputs.first() else {
                continue;
            };
            let Some(merge) = graph.nodes.get(merge_ctrl as usize) else {
                continue;
            };
            if !matches!(merge.op, Op::Merge | Op::Region) {
                continue;
            }
            let Some(header) = self.block_of_ctrl(merge_ctrl) else {
                continue;
            };
            for k in 0..merge.inputs.len() {
                let Some(&v) = pn.inputs.get(k + 1) else {
                    continue;
                };
                let Some(&ctrl_in) = merge.inputs.get(k) else {
                    continue;
                };
                let Some(b) = self.block_of_ctrl(ctrl_in) else {
                    continue;
                };
                if v == NO_NODE
                    || !self.phi_source_coalesces(p, v as usize, b, header, reg, &use_count)
                {
                    continue;
                }
                if let Some(cell) = self.gp_reg_of.get_mut(v as usize) {
                    *cell = Some(reg);
                }
                if let Some(cell) = self.phi_coalesced_src.get_mut(v as usize) {
                    *cell = true;
                }
                self.phi_coalesced += 1;
            }
        }
        if self.phi_coalesced > 0 && false {
            eprintln!(
                "[ir-ls] back-edge phi sources coalesced: {}",
                self.phi_coalesced
            );
        }
    }

    /// One candidate of [`Self::coalesce_back_edge_phi_sources`]: may phi `p`'s
    /// source `v`, arriving on the edge from block `b` to `header`, take the
    /// phi's register `reg`? Every clause that method's doc states, checked
    /// against the CURRENT `gp_reg_of` (earlier coalesced sources included).
    pub(in crate::ir_lower) fn phi_source_coalesces(
        &self,
        p: usize,
        v: usize,
        b: usize,
        header: usize,
        reg: u8,
        use_count: &[u32],
    ) -> bool {
        let (Some(pn), Some(vn)) = (self.graph.nodes.get(p), self.graph.nodes.get(v)) else {
            return false;
        };
        if !matches!(vn.op, Op::Add | Op::Sub) || vn.ty != pn.ty {
            return false;
        }
        if use_count.get(v).copied() != Some(1)
            || self.phi_coalesced_src.get(v).copied().unwrap_or(true)
            || self.gp_reg_of.get(v).is_none()
        {
            return false;
        }
        if self.schedule.node_to_block.get(v).copied() != Some(b) {
            return false;
        }
        let Some(block) = self.schedule.blocks.get(b) else {
            return false;
        };
        if block.terminator.is_some() || block.successors.as_slice() != [header] {
            return false;
        }
        // Exactly one merge input from `b`: one copy, of `v`.
        let Some(&merge_ctrl) = pn.inputs.first() else {
            return false;
        };
        let Some(merge) = self.graph.nodes.get(merge_ctrl as usize) else {
            return false;
        };
        let from_b = merge
            .inputs
            .iter()
            .filter(|&&c| self.block_of_ctrl(c) == Some(b))
            .count();
        if from_b != 1 {
            return false;
        }
        // Cast: an index into the node arena is a `NodeId`.
        let phi = p as NodeId;
        // The rest of `b`: pure, and not reading the phi.
        let Some(at) = block.nodes.iter().position(|&x| x as usize == v) else {
            return false;
        };
        for &x in block.nodes.iter().skip(at + 1) {
            let Some(xn) = self.graph.nodes.get(x as usize) else {
                return false;
            };
            if !op_cannot_deopt(&xn.op) || xn.inputs.contains(&phi) {
                return false;
            }
        }
        // No other phi of the header reads this phi on any edge from `b`.
        if let Some(phis) = self.phis_by_merge.get(&merge_ctrl) {
            for &q in phis {
                if q == phi {
                    continue;
                }
                let Some(qn) = self.graph.nodes.get(q as usize) else {
                    return false;
                };
                for (k, &ctrl_in) in merge.inputs.iter().enumerate() {
                    if self.block_of_ctrl(ctrl_in) == Some(b) && qn.inputs.get(k + 1) == Some(&phi)
                    {
                        return false;
                    }
                }
            }
        }
        // No other holder of `reg` live in `v`'s range; no other phi holds it.
        let Some(vr) = self.slot_plan.range.get(v).copied().flatten() else {
            return false;
        };
        // JIT round 23 wave 5b (lane L1, `CRATONVM_JIT_IR_PHI_COALESCE_FOREIGN_PHI`):
        // read once for the loop below.
        let foreign_phis_admitted = ir_phi_coalesce_foreign_phi_enabled();
        for (w, r) in self.gp_reg_of.iter().enumerate() {
            if *r != Some(reg) || w == p || w == v {
                continue;
            }
            match self.graph.nodes.get(w) {
                Some(wn) if wn.op != Op::Phi => {}
                // A phi of ANOTHER merge sharing `reg` (an OSR body's opaque
                // entry-merge phi feeding this header phi, given the same
                // register so its copy is a no-op): admitted when none of its
                // edge copies lands in `b` (see `foreign_phi_spares_block`)
                // and, below, its range misses `v`'s.
                Some(wn)
                    if foreign_phis_admitted
                        && self.foreign_phi_spares_block(wn, b, merge_ctrl) => {}
                _ => return false,
            }
            let Some(wr) = self.slot_plan.range.get(w).copied().flatten() else {
                return false;
            };
            if !(wr.hi < vr.lo || wr.lo > vr.hi) {
                return false;
            }
        }
        true
    }

    /// JIT round 23 wave 5b (lane L1, `CRATONVM_JIT_IR_PHI_COALESCE_FOREIGN_PHI`):
    /// may a phi `wn` of ANOTHER merge share register `reg` with a back-edge
    /// source `v` defined in block `b` (the edge from `b` into `header_merge`)?
    ///
    /// The one way such a phi writes its register outside its own range is an
    /// edge copy, emitted at the end of a predecessor block of ITS merge. So
    /// when `wn` is a phi of a different merge than the header's, and no
    /// predecessor of its merge is `b`, no copy of it can land between `v`'s
    /// definition and `b`'s edge copy; with its range also disjoint from `v`'s
    /// (the caller's check), nothing else reads or writes the register while
    /// `v` holds it. The case this admits is an OSR body's opaque entry-merge
    /// phi (`IrBuilder::build_opaque_osr_merge`), which the scan gives the
    /// header phi's own register: before this the induction variable of every
    /// optimizing OSR loop kept its back-edge source in RAX, stored and
    /// reloaded across the poll (`PF.plain`'s OSR body on `n23-w4`), while the
    /// accumulator, whose entry phi had no register, coalesced. `false` for a
    /// non-phi, a phi of the header itself, or anything not locatable.
    pub(in crate::ir_lower) fn foreign_phi_spares_block(
        &self,
        wn: &Node,
        b: usize,
        header_merge: NodeId,
    ) -> bool {
        if wn.op != Op::Phi {
            return false;
        }
        let Some(&w_merge) = wn.inputs.first() else {
            return false;
        };
        if w_merge == header_merge {
            return false;
        }
        let Some(mn) = self.graph.nodes.get(w_merge as usize) else {
            return false;
        };
        if !matches!(mn.op, Op::Merge | Op::Region) {
            return false;
        }
        mn.inputs
            .iter()
            .all(|&c| matches!(self.block_of_ctrl(c), Some(pb) if pb != b))
    }

    /// Every register in the `gp` bank that node `id`'s residency names, when
    /// `id` is managed: the one it starts in plus every transition destination.
    ///
    /// Read by `set_residency` for the exclusive-ownership poison above, and by
    /// the tests. A scan of the whole position table rather than a stored set:
    /// the table is the authority on where a managed value goes, and a second
    /// copy of that fact is a second thing to keep in step.
    ///
    /// **Banked, and it is not tidiness.** `holders` above is indexed by GP
    /// number; an XMM 3 leaking into it would poison RBX for a value that has
    /// nothing to do with the FP file. `PhysReg` carries its class for exactly
    /// this reason and a bare `u8` does not.
    pub(in crate::ir_lower) fn ls_split_registers(&self, id: NodeId, gp: bool) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        // The DEFINITION's register, not the current one -- see `ls_origin`.
        let bank = if gp {
            &self.ls_origin.0
        } else {
            &self.ls_origin.1
        };
        let start = bank.get(id as usize).copied().flatten();
        if let Some(reg) = start {
            out.push(reg);
        }
        for transitions in &self.ls_at {
            for t in transitions {
                if t.node() != id {
                    continue;
                }
                if let Some((reg, is_gp)) = t.destination() {
                    if is_gp == gp {
                        out.push(reg);
                    }
                }
            }
        }
        // An edge-resolution copy puts the value in a register too (round 9
        // wave 6).
        for edge in &self.ls_edge_copies {
            for &(node, reg, is_gp) in &edge.copies {
                if node == id && is_gp == gp {
                    out.push(reg);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Why each PROMOTED value that kept its home kept it, per cause:
    /// `[switch, deopt, type,
    /// op]`, in the order [`Self::value_home_droppable`] tests them.
    ///
    /// # Why a per-cause census and not a count
    ///
    /// `plan_register_residency` used to print a `home: droppable=… ` line
    /// computed from a NAIVE rule — "some safepoint's locals or stack names
    /// it" — that predates `ir-deopt-regs`, `ir-reg-authoritative` and
    /// `ir-drop-phi-home`. By 2026-09-10 it read `droppable=0` on
    /// `probes/FieldLoop.java` `sum` while the emission on the SAME compile
    /// dropped three homes, and
    /// `internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`
    /// quoted the zero and built an argument on it. A diagnostic that is wrong
    /// in the direction of "nothing works" is worse than none: it retires
    /// levers that are in fact engaged.
    ///
    /// This one is computed from the predicates that actually decide, at the
    /// moment they decide, so it cannot drift from them — and the four causes
    /// call for four different next steps, which a bare count cannot say:
    ///
    ///   * `switch` — a flag in the conjunction is off. Nothing to chase.
    ///   * `deopt`  — a reachable frame state names it and the register is not
    ///     exclusively its own. **This is the one the architecture change is
    ///     for**: it wants a register bank the frame reconstructor can read at
    ///     an arbitrary bci, not another heuristic.
    ///   * `type`   — `Ref` (needs a register bank in the OOP MAP, a different
    ///     and larger change) or FP (a different file).
    ///   * `op`     — the defining arm does not write its home through one
    ///     `store_rax`, so there is no single store to drop.
    ///
    /// Deliberately counts only values the residency plan actually promoted: a
    /// value with no register has no home to drop and would swamp the census
    /// with a cause nobody can act on. Called at the END of lowering, after
    /// [`Self::extend_home_drops_to_carried_values`] has had its say, so
    /// "kept" means what the emission really did rather than what the plan
    /// intended.
    ///
    /// # The accounting identity, which is what stops this one drifting
    ///
    /// Every promoted value is either dropped or attributed to exactly one
    /// cause, and the assertion at the bottom says so. That is not decoration:
    /// the diagnostic it replaces went wrong precisely by computing its own
    /// answer instead of reading the one the emission used, and an identity
    /// that ties the census to `home_dropped` cannot do that. A future clause
    /// added to `value_home_droppable` and not here fails this immediately
    /// under `cargo test`, because `lower_inner_with_array_list` calls this on
    /// every debug lowering and not only when the diagnostic is on.
    pub(in crate::ir_lower) fn census_home_blocks(&self) -> [usize; 4] {
        let (mut switch, mut deopt, mut ty, mut op) = (0usize, 0, 0, 0);
        let mut promoted = 0usize;
        let mut dropped = 0usize;
        for id in 0..self.home_dropped.len() {
            if self.gp_reg_of.get(id).copied().flatten().is_none() {
                continue;
            }
            promoted += 1;
            if self.home_dropped[id] {
                dropped += 1;
                continue;
            }
            // Cast: an index into the node arena is a `NodeId`.
            let nid = id as NodeId;
            // No node behind a promoted index is not a shape this can
            // attribute, and dropping it silently would break the identity
            // below. `op` is the honest bucket: whatever it is, its home store
            // is not one this pass can reason about.
            let Some(node) = self.graph.nodes.get(id) else {
                op += 1;
                continue;
            };
            // Round 9: `set_residency` refuses a dropped home to every
            // split-MANAGED value before either predicate is asked (its
            // transitions reload from that word), and this census had no bucket
            // for that refusal — a managed `Int`/`Long` with a one-store home
            // that ended the body in a register fell through every clause below
            // and tripped the identity under `CRATONVM_JIT_IR_LS_SPLITS=1`.
            // `op` is the bucket whose meaning fits: its home store is one this
            // pass may not reason about.
            if self.ls_managed.get(id).copied().unwrap_or(false) {
                op += 1;
                continue;
            }
            // `install_caller_saved_residency` puts back the home of every value
            // in a caller-saved register (write-through is that item's
            // contract), after the drop pass has run. Same bucket: its home
            // store is not one this pass may drop.
            if self.ls_caller_saved_on
                && self.gp_reg_of[id].is_some_and(|r| IR_LS_CALLER_SAVED_GPRS.contains(&r))
            {
                op += 1;
                continue;
            }
            let is_phi = matches!(node.op, crate::ir::Op::Phi);
            let switches_on = if is_phi {
                ir_deopt_regs_enabled() && self.phi_copy_regs_on && ir_skip_live_republish_enabled()
            } else {
                ir_publish_at_def_enabled()
                    && ir_deopt_regs_enabled()
                    && ir_skip_live_republish_enabled()
                    && self.phi_copy_regs_on
            };
            if !switches_on {
                switch += 1;
                continue;
            }
            // The frame-state obligation, in the same widening order the
            // predicates use: nameable in a register, or named by no deopt
            // that can happen (a phi's own publisher makes the trap-free arm
            // redundant for it, which is why the two spellings differ).
            let named_reachable = self.deopt_named_reachable.get(id).copied().unwrap_or(true);
            let nameable = self.deopt_nameable.get(id).copied().unwrap_or(false);
            let frame_ok = if is_phi {
                nameable || !named_reachable
            } else {
                nameable
                    || (self.graph_cannot_deopt() && self.assigned_gpr(nid).is_some())
                    || (!named_reachable && self.assigned_gpr(nid).is_some())
            };
            if !frame_ok {
                deopt += 1;
                continue;
            }
            if !matches!(node.ty, IrType::Int | IrType::Long) {
                ty += 1;
                continue;
            }
            // A phi has no defining arm to certify; reaching here means the
            // predicate said yes, so this is unreachable for one. Ordinary
            // values fall to the store-shape clause.
            if is_phi || !self.home_is_one_store_rax(node) {
                op += 1;
            }
        }
        debug_assert_eq!(
            dropped + switch + deopt + ty + op,
            promoted,
            "the home census lost a promoted value: {dropped} dropped, [{switch}, {deopt}, {ty}, {op}] kept, against {promoted} promoted. Every clause of `value_home_droppable` / `phi_home_droppable` needs a cause here, or this line reads low for a reason nobody can chase -- which is exactly how the census it replaced came to report zero on a compile that dropped three homes."
        );
        [switch, deopt, ty, op]
    }

    /// May this phi's home word go unwritten?
    ///
    /// **Every clause is load-bearing, and the point of writing them as one
    /// conjunction is that no combination of switches can satisfy some of them
    /// and not the rest.** In order:
    ///
    /// * the switch, and the register image the deopt path reads;
    /// * the edge copies publish the register (otherwise nothing puts the value
    ///   there at all, since the publish this replaces was a load OF the home);
    /// * a value already in its register is not re-published (otherwise the
    ///   generic publish site reloads the unwritten home once per iteration and
    ///   overwrites the register with whatever the frame word holds — the same
    ///   reload this whole line of work removed, now actively destructive);
    /// * the register is EXCLUSIVELY this value's, so a deopt frame can name it
    ///   at any bci (see `deopt_nameable`);
    /// * and it is an `Int` or a `Long`. A `Ref` is never in this file, and an
    ///   FP value's home is written by a different path.
    ///
    /// **And it HAS a register** (round 11 wave 12, lane fib). The widening
    /// below (no reachable deopt names it) answered `true` for a phi the
    /// residency plan gave no GPR at all -- spilled, split, or never allocated
    /// -- so `emit_copy_op` dropped its only store, published nothing (no
    /// register to publish into), and the dropped-home guard in
    /// `emit_phi_copies` refused the whole method at the first edge
    /// (`UnallocatedValue`: "home was dropped and its register is not live at
    /// a publish edge"). That was every recursively spliced `CratonBench.fib`
    /// body: a multi-return splice's result phi is live across the nested
    /// splice's calls, loses its register to pressure, and is named by no
    /// frame state a reachable deopt reads. `value_home_droppable` has always
    /// required `assigned_gpr` on the same widening; this is the phi arm
    /// catching up. The census counts promoted values only, so it is unmoved.
    pub(in crate::ir_lower) fn phi_home_droppable(&self, phi: NodeId) -> bool {
        if !(ir_deopt_regs_enabled() && self.phi_copy_regs_on && ir_skip_live_republish_enabled()) {
            return false;
        }
        // No register, no second location: the home word is all there is.
        if self.assigned_gpr(phi).is_none() {
            return false;
        }
        // Same widening as `value_home_droppable`: a phi named by no deopt that
        // can actually happen is not pinned by a frame state either. Its
        // register is published by the edge copies (`emit_copy_op`), which the
        // `ir_phi_copy_regs_enabled` clause above already requires.
        if !self
            .deopt_nameable
            .get(phi as usize)
            .copied()
            .unwrap_or(false)
            && self
                .deopt_named_reachable
                .get(phi as usize)
                .copied()
                .unwrap_or(true)
        {
            return false;
        }
        // The register is the ONLY location a dropped home leaves, so a phi
        // with none keeps its home -- the conjunct `value_home_droppable`'s
        // widening clauses carry (`assigned_gpr(id).is_some()`) and this one
        // lacked. Without it a phi no reachable deopt names, but that the
        // allocator left in memory, had no location at all, and
        // `emit_phi_copies` refused the compile at its first publish edge
        // (bounded self-recursive inlining's `itemCheck`, whose spliced
        // copies' result phis are exactly that).
        if self.assigned_gpr(phi).is_none() {
            return false;
        }
        matches!(
            self.graph.nodes.get(phi as usize).map(|n| n.ty),
            Some(IrType::Int) | Some(IrType::Long)
        )
    }

    /// Can NOTHING in this graph transfer to the interpreter?
    ///
    /// The PREDICTION half of [`ir_drop_unreachable_homes_enabled`]. Computed
    /// from the graph because the decision it feeds — which homes to drop — is
    /// made before a byte is emitted; `lower_inner` checks the answer against
    /// the emission afterwards.
    pub(in crate::ir_lower) fn graph_cannot_deopt(&self) -> bool {
        if !ir_drop_unreachable_homes_enabled() {
            return false;
        }
        *self.graph_trap_free.get_or_init(|| {
            self.graph
                .nodes
                .iter()
                .all(|n| self.node_cannot_deopt_here(n))
        })
    }

    /// nio21 wave 7 (lane L5): does a pre-header mask guard prove this
    /// access (`ir_optimize::mask_guard_proves_access`, under
    /// `CRATONVM_JIT_IR_MASK_GUARD_ELIDE`)? The per-compile cached form: the
    /// graph does not change during lowering, so the proofs are computed once
    /// instead of a node scan, a users map and a loop forest per question.
    pub(in crate::ir_lower) fn mask_guard_proven(&self, n: &Node) -> bool {
        self.mask_proofs
            .get_or_init(|| {
                crate::ir_optimize::mask_guard_elide_enabled()
                    .then(|| crate::ir_optimize::MaskGuardProofs::of(self.graph))
            })
            .as_ref()
            .is_some_and(|p| p.proves(self.graph, n))
    }

    /// [`node_cannot_deopt`] over this compile's cached mask proofs: the same
    /// clauses (`node_cannot_deopt_with`), the masked-load one answered by
    /// [`Self::mask_guard_proven`].
    pub(in crate::ir_lower) fn node_cannot_deopt_here(&self, n: &Node) -> bool {
        if guard_const_true_cannot_trap(self.graph, n) {
            return true;
        }
        node_cannot_deopt_with(self.graph, n, |m| {
            matches!(m.op, Op::ArrayLoad(_)) && self.mask_guard_proven(m)
        })
    }

    /// Is this value pinned to its home only by frame states nothing can
    /// reach?
    ///
    /// `deopt_named` says a frame state mentions it. That is a reason to keep
    /// the home word only if some deopt can consult that frame state.
    pub(in crate::ir_lower) fn deopt_naming_is_reachable(&self, id: NodeId) -> bool {
        if !self.deopt_named.get(id as usize).copied().unwrap_or(true) {
            return false;
        }
        !self.graph_cannot_deopt()
    }

    /// Can a deopt that can actually happen name `id`?
    ///
    /// # Why this exists — the real limit on register residency
    ///
    /// A value's home store is dropped only when a deopt frame could describe
    /// it some other way, and the test for that was
    /// [`Self::deopt_nameable`]: the value must own its register
    /// **exclusively, for the whole method**. With a five-register file and a
    /// linear-scan allocator whose entire job is to give one register to
    /// several values with disjoint ranges, that is nearly never true — so
    /// nearly every promoted value kept its home store, and the register file
    /// stayed a write-through read CACHE rather than the authoritative
    /// location. That, not the allocator, is why this tier stores every SSA
    /// value to the frame.
    ///
    /// The only escape was `graph_cannot_deopt()` — whole-body trap freedom,
    /// which one `getfield` anywhere in the method destroys.
    ///
    /// This is the per-value form of the same question. A frame state can only
    /// ever be read at a bci something can actually trap at, so a value named
    /// exclusively by frame states at NON-trapping bcis is not pinned by any of
    /// them, whatever the rest of the method does.
    ///
    /// # Why it is safe without touching the GC
    ///
    /// Strictly a widening of the existing escape: `graph_cannot_deopt()`
    /// implies this for every value, so nothing that dropped a home before
    /// stops doing so. And the callers restrict it to `Int` and `Long` —
    /// `IrType::Ref` is refused by `value_home_droppable` and
    /// `phi_home_droppable` exactly as before, so no reference becomes
    /// register-resident and `OopMapEntry` needs no register bank. That
    /// remains the blocker for reference promotion and is untouched here.
    ///
    /// # Why a wrong prediction cannot produce a wrong answer
    ///
    /// This is a PREDICTION from the graph, made before emission, in the same
    /// shape and with the same net as `graph_cannot_deopt`: if a trap does
    /// occur at a bci predicted safe, `frame_value_for` meets a dropped home
    /// with no register describing it and REFUSES THE COMPILE. The method
    /// falls to the single-pass backend — a coverage loss, never a wrong
    /// frame.
    ///
    /// # The loop-header exception
    ///
    /// Every loop header is treated as trapping whether or not its ops can
    /// trap. An optimizing OSR entry seeds this tier's locals at a loop header
    /// from the interpreter's frame, and it seeds HOME WORDS; a value whose
    /// home was dropped there would be seeded into a word nothing reads. The
    /// door was implicated in one miscompile for exactly this reason
    /// (`ir-osr-entry-miscompiles-a-spliced-merge-FIXED-20260909.md`: the stub
    /// jumped past the block that writes a constant's home word). That one is
    /// fixed, by MAKING the stub write the words it skipped rather than by
    /// refusing anything — so this clause stands on its own argument, not on
    /// that page: a dropped home is a word the seeding cannot write at all,
    /// which is a different problem from a word it merely forgot to.
    pub(in crate::ir_lower) fn compute_deopt_named_reachable(&self) -> Vec<bool> {
        let n = self.graph.nodes.len();
        let mut out = vec![false; n];
        let widened = ir_register_authoritative_enabled();
        // bcis a deopt can arrive at: every node whose op is outside the
        // cannot-trap allowlist, plus every loop header.
        // Round 9: BOTH the raw pc and its `resume_bci`. A trap inside a
        // spliced callee body resumes at the enclosing `invoke`
        // (`emit_deopt_unless` and the `Op::Guard` arm resolve their frame
        // state at `self.resume_bci(bci)`), so the caller snapshot AT that
        // invoke is the one such a trap reads. Keyed by the raw combined-buffer
        // pc alone, the invoke's bci was missing from this set whenever the
        // spliced call node itself was gone, its values' homes were dropped,
        // and the inlined null/bounds check then latched `UnallocatedValue`
        // and refused the whole method. Adding a bci only KEEPS more homes, so
        // this can widen what is kept and never what is dropped.
        let mut trapping: std::collections::HashSet<usize> = if widened {
            let mut set = std::collections::HashSet::new();
            for pc in self
                .graph
                .nodes
                .iter()
                // Round 9 wave 3: `node_cannot_deopt`, the graph-aware form
                // `graph_cannot_deopt` already uses -- a division by a non-zero
                // constant emits no deopt point, so its bci is not a trap.
                .filter(|node| !self.node_cannot_deopt_here(node))
                // Round 11 wave 13 (lane fib): a direct self-call outside every
                // protected range transfers nothing -- see
                // `self_call_reads_no_frame_state`. Its exits are recorded to
                // match (`push_self_call_exc_patch`), so `build_deopt_points`'
                // latch still checks this prediction against the emission.
                .filter(|node| !self.self_call_reads_no_frame_state(node))
                .filter_map(|node| node.bytecode_pc)
            {
                set.insert(pc);
                set.insert(self.resume_bci(pc));
            }
            set
        } else {
            std::collections::HashSet::new()
        };
        if widened {
            for (b, block) in self.schedule.blocks.iter().enumerate() {
                if block.successors.iter().any(|&s| s <= b) {
                    for &succ in &block.successors {
                        if succ <= b {
                            if let Some(header) = self.schedule.blocks.get(succ) {
                                if let Some(pc) = self
                                    .graph
                                    .nodes
                                    .get(header.ctrl as usize)
                                    .and_then(|node| node.bytecode_pc)
                                {
                                    trapping.insert(pc);
                                }
                            }
                        }
                    }
                }
            }
        }
        for sp in &self.graph.safepoints {
            // Unwidened: every naming counts, which is the pre-2026-09-09
            // behaviour and leaves `graph_cannot_deopt` as the only escape.
            if widened && !trapping.contains(&sp.bci) {
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
                if v != NO_NODE {
                    if let Some(cell) = out.get_mut(v as usize) {
                        *cell = true;
                    }
                }
            }
        }
        for v in loop_carried_recipe_values(self.sr_map) {
            if let Some(cell) = out.get_mut(v as usize) {
                *cell = true;
            }
        }
        // JIT round 19 wave 5 (lane osr5, OSR2-3): a promoted loop's deopt
        // pads read these (`promoted_pending_pin_values`; wave 7, lane osr7,
        // OSR6-1 / OSR5-3: `promoted_pending_deopt_named`).
        for v in promoted_pending_deopt_named(self.graph, self.schedule) {
            if let Some(cell) = out.get_mut(v as usize) {
                *cell = true;
            }
        }
        out
    }

    /// May this ORDINARY value's home word go unwritten?
    ///
    /// The same conjunction as [`Self::phi_home_droppable`] with the phi's
    /// publisher swapped for this one's. A phi is published by `emit_copy_op`
    /// at every incoming edge; every other value is published by `store_rax`
    /// at its single home write, which is what `ir_publish_at_def_enabled`
    /// arranges and what `op_home_is_one_store_rax` certifies exists.
    ///
    /// `ir_skip_live_republish_enabled` is required for the same reason it is
    /// there: without it the generic publish at the end of `lower_data_node`
    /// would reload the home word nothing wrote, over the top of the register
    /// this value's definition had just published correctly.
    pub(in crate::ir_lower) fn value_home_droppable(&self, id: NodeId) -> bool {
        if !(ir_publish_at_def_enabled()
            && ir_deopt_regs_enabled()
            && ir_skip_live_republish_enabled()
            // `gather_phi_copies` is the ONE reader that is exempt from
            // `slot_of`'s refusal: it takes the would-be offset from
            // `slot_of_unwritten` to identify a copy destination. `emit_copy_op`
            // then reads the SOURCE out of its register -- but only under this
            // switch. With it off, a dropped-home value used as a phi source
            // would be loaded from the word nothing wrote, which is the one
            // silent miscompile this area can produce.
            && self.phi_copy_regs_on)
        {
            return false;
        }
        // `deopt_nameable` asks "can a deopt frame NAME this in a register".
        // A body nothing can deopt from never asks, so an assigned register is
        // enough — the readers need it, the frame states do not exist to be
        // wrong. Verified against the emission in `lower_inner`.
        // Three ways to satisfy the frame-state obligation, in widening order:
        //
        //   1. a deopt can NAME this value in its register (exclusive owner);
        //   2. the whole body cannot deopt, so no frame state is ever read;
        //   3. no deopt that can actually happen names THIS value.
        //
        // (3) subsumes (2) and is the one that makes the register file
        // authoritative on ordinary methods — see
        // `compute_deopt_named_reachable` for why one `getfield` used to be
        // enough to force every promoted value in the method back to memory.
        if !self
            .deopt_nameable
            .get(id as usize)
            .copied()
            .unwrap_or(false)
            && !(self.graph_cannot_deopt() && self.assigned_gpr(id).is_some())
            && !(!self
                .deopt_named_reachable
                .get(id as usize)
                .copied()
                .unwrap_or(true)
                && self.assigned_gpr(id).is_some())
        {
            return false;
        }
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return false;
        };
        // `Ref` is named by frame slots in the oop map and FP lives in the
        // other file -- the same two exclusions the phi predicate makes.
        if !matches!(node.ty, IrType::Int | IrType::Long) {
            return false;
        }
        self.home_is_one_store_rax(node)
    }

    /// [`op_home_is_one_store_rax`], plus the one route-specific shape the op
    /// alone cannot certify: the result of a DIRECT self-recursive call.
    ///
    /// Round 11 wave 13 (lane fib). `Op::Call` has several routes and most do
    /// not end in one `store_rax`, so the op is not on the allowlist. The
    /// `invoke_kind == 4` route always does: the `Op::Call` arm sends it to
    /// [`Self::emit_self_recursive_call`] before anything else can claim it
    /// (both call prefixes decline kind 4), which writes the result once, at
    /// `.keep`, through `store_rax(slot)` -- `publish_def_at_store` publishes
    /// the register there -- and then returns past the generic publish. Every
    /// other path out of that route leaves through the call-exception stub
    /// and reads no home. Shared with [`Self::census_home_blocks`] so the two
    /// cannot disagree about which values have a single home write.
    pub(in crate::ir_lower) fn home_is_one_store_rax(&self, node: &Node) -> bool {
        op_home_is_one_store_rax(&node.op)
            || (Self::is_direct_self_call(node) && ir_self_call_states_unread_enabled())
    }

    /// Is `node` an `Op::Call` bound to the direct self-recursive route
    /// (`JitInvokeInfo::invoke_kind == 4`)? The same read the `Op::Call` arm
    /// makes to choose [`Self::emit_self_recursive_call`].
    pub(in crate::ir_lower) fn is_direct_self_call(node: &Node) -> bool {
        let Op::Call { info_ptr } = node.op else {
            return false;
        };
        if info_ptr == 0 {
            return false;
        }
        // SAFETY: as the `Op::Call` arm's own `invoke_kind` read -- a non-zero
        // `info_ptr` names a live `JitInvokeInfo` owned by `ir_call_infos` for
        // the whole compile.
        unsafe { (*(info_ptr as *const JitInvokeInfo)).invoke_kind == 4 }
    }

    /// JIT round 23 wave 3d (lane L1, proxy proposal 4e): the receiver input of
    /// a RECEIVER-CARRYING direct self-call (a statically bound instance
    /// self-call planned under `CRATONVM_JIT_IR_DIRECT_INSTANCE_SELF_CALL`),
    /// `None` for every other node, the static self-call included.
    pub(in crate::ir_lower) fn direct_self_call_receiver(node: &Node) -> Option<NodeId> {
        if !Self::is_direct_self_call(node) {
            return None;
        }
        let Op::Call { info_ptr } = node.op else {
            return None;
        };
        // SAFETY: as `is_direct_self_call`'s read of the same pointer.
        let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !crate::self_call_info_has_receiver(info) {
            return None;
        }
        node.inputs.get(2).copied()
    }

    /// Does this direct self-call's receiver need its JVMS 6.5 null check?
    /// Every receiver-carrying self-call does, except one whose receiver is
    /// this method's own `this` ([`Graph::receiver_param`], never null; under
    /// [`ir_this_nonnull_enabled`], the switch the field-access seed obeys).
    /// A checked site's bci keeps its frame state
    /// ([`Self::self_call_reads_no_frame_state`]) and its frame id store
    /// (the lazy store is refused), because the check's deopt reads both.
    pub(in crate::ir_lower) fn self_call_receiver_needs_check(&self, node: &Node) -> bool {
        let Some(recv) = Self::direct_self_call_receiver(node) else {
            return false;
        };
        let is_this = ir_this_nonnull_enabled()
            && self.graph.receiver_param.is_some_and(|p| {
                matches!(
                    self.graph.nodes.get(recv as usize).map(|n| &n.op),
                    Some(Op::Param(q)) if *q == p
                )
            });
        !is_this
    }

    /// The row the callee-deopt service is handed for the direct self-call
    /// `node` baked with `info_ptr`: `info_ptr` itself for the static
    /// self-call, the kind-1 twin `lib.rs` planned for a receiver-carrying one
    /// ([`Graph::self_call_service_infos`]). A receiver-carrying call with no
    /// twin (a graph no planner prepared) refuses the compile rather than let
    /// the VM decode its receiver as a declared argument.
    pub(in crate::ir_lower) fn self_call_service_info(
        &self,
        node: &Node,
        info_ptr: usize,
    ) -> usize {
        if Self::direct_self_call_receiver(node).is_none() {
            return info_ptr;
        }
        match self
            .graph
            .self_call_service_infos
            .iter()
            .find(|&&(ir_row, _)| ir_row == info_ptr)
        {
            Some(&(_, vm_row)) => vm_row,
            None => {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: a receiver-carrying direct self-call has no VM row",
                )));
                info_ptr
            }
        }
    }

    /// Can a frame state at this node's bci be READ because of this node?
    /// `true` means no: `node` is a direct self-call whose exits publish
    /// nothing.
    ///
    /// Round 11 wave 13 (lane fib; calls proposal 2, "parameters kept in
    /// registers", generalised). Every value a self-recursive body keeps live
    /// across its own recursion -- `fib`'s `n`, the pending `fib(n-1)`, both
    /// arguments -- is named by the snapshots at the recursive invokes, and a
    /// call is not on `op_cannot_deopt`'s allowlist, so every one of them kept
    /// a home store it never needed. What the kind-4 route actually emits:
    ///
    /// * the sampled stack-guard trip and the `dispatch_threw` exit both go to
    ///   the call-exception stub, which stamps the bci and returns the
    ///   sentinel -- it publishes no frame (`propagating_exit_holds_monitor`'s
    ///   doc: "a call-exception exit that publishes NOTHING");
    /// * the callee-deopt service rebuilds the CALLEE's frame from the
    ///   callee's own stash and staged arguments (read through
    ///   `gp_load_value`, i.e. from registers when that is where they are);
    /// * no guard, no null check (`invokestatic`), no deopt stub.
    ///
    /// The one transfer that does read this frame at a call is the reason-9
    /// rethrow pad, emitted only inside a protected range
    /// ([`Self::push_rethrow_pad`]), so a protected call keeps its bci
    /// trapping. A handler outside the precise-frame route resumes on the
    /// incoming arguments (`run_jit_callee_handler`'s params-only tier), not
    /// on a by-bci point, and the by-bci consumers of `deopt_points` ask about
    /// bci and reason only.
    ///
    /// Soundness does not rest on this list being complete:
    /// [`Self::push_self_call_exc_patch`] leaves the bci out of
    /// `transfer_bcis` under exactly this predicate, so if any other site at
    /// the same bci can transfer, it records the bci itself and
    /// `build_deopt_points` refuses the compile on the first dropped home it
    /// names (`UnallocatedValue`) -- a lost compile, never a wrong frame.
    /// [`ir_self_call_states_unread_enabled`] is the switch.
    pub(in crate::ir_lower) fn self_call_reads_no_frame_state(&self, node: &Node) -> bool {
        if !Self::is_direct_self_call(node) || !ir_self_call_states_unread_enabled() {
            return false;
        }
        // JIT round 23 wave 3d: a receiver null check's deopt reads the state.
        if self.self_call_receiver_needs_check(node) {
            return false;
        }
        match node.bytecode_pc {
            Some(pc) => !self.bci_is_protected(self.resume_bci(pc)),
            None => false,
        }
    }

    /// [`Self::push_call_exc_patch`] for the direct self-recursive route: the
    /// same row and the same monitor fact, but the bci is recorded as a
    /// transfer site only when a reason-9 pad makes it one. See
    /// [`Self::self_call_reads_no_frame_state`], which is the prediction this
    /// must match.
    pub(in crate::ir_lower) fn push_self_call_exc_patch(&mut self, patch: usize) {
        let bci = self.resume_bci(self.cur_bci);
        if !ir_self_call_states_unread_enabled() || self.bci_is_protected(bci) {
            self.push_call_exc_patch(patch);
            return;
        }
        if !self.propagating_exit_holds_monitor && self.site_snapshot_may_hold_monitor(bci) {
            self.propagating_exit_holds_monitor = true;
        }
        self.call_exc_patches.push((patch, bci));
    }

    /// Describe `id` as living in its register, if that is provably where it
    /// is at every bci a frame state could name it.
    ///
    /// `None` for everything else, and the caller then falls through to the
    /// home word exactly as before — this is additive, and with
    /// `CRATONVM_JIT_IR_DEOPT_REGS` off it answers `None` for every value.
    pub(in crate::ir_lower) fn register_frame_value(
        &self,
        id: NodeId,
        ty: IrType,
    ) -> Option<FrameValue> {
        if !self
            .deopt_nameable
            .get(id as usize)
            .copied()
            .unwrap_or(false)
        {
            return None;
        }
        let reg = self.gp_reg_of.get(id as usize).copied().flatten()?;
        // Round 16 wave 11 (lane frames2): a register the sized image does not
        // reserve is never named. Unreachable by construction (the image holds
        // every register the plan hands out, `size_deopt_image`); if it ever
        // fires, the caller falls through to the home word, and a dropped home
        // refuses the compile there -- fail closed, never a read of a word the
        // stub did not write.
        if !self.deopt_image_holds_gpr(reg) {
            return None;
        }
        match ty {
            // A cat-1 `int`: `Register` resolves it truncated to 32 bits.
            IrType::Int => Some(FrameValue::Register(reg)),
            // A cat-2 `long` keeps all 64.
            IrType::Long => Some(FrameValue::RegisterLong(reg)),
            IrType::Ref if self.ref_register_frame_value_ok(id, reg) => {
                Some(FrameValue::RegisterRef(reg))
            }
            _ => None,
        }
    }

    pub(in crate::ir_lower) fn ref_register_frame_value_ok(&self, id: NodeId, reg: u8) -> bool {
        if !self.ref_residency_on {
            return false;
        }
        if self
            .lazy_ref_home
            .get(id as usize)
            .copied()
            .unwrap_or(false)
        {
            return self.building_table_points.get() || self.lazy_resident(id) == Some(reg);
        }
        !self.building_table_points.get()
            && self.lazy_resident(id) == Some(reg)
            && self
                .post_collect_ref_mask
                .get()
                .is_none_or(|mask| mask & (1u16 << reg) != 0)
    }

    /// Does this frame's deopt register image reserve `gpr[reg]` -- is the
    /// word the stub stores `reg` to inside the frame, and stored by it?
    pub(in crate::ir_lower) fn deopt_image_holds_gpr(&self, reg: u8) -> bool {
        self.deopt_regs_base > 0 && i32::from(reg) < (self.deopt_regs_bytes / 8).min(16)
    }

    /// The image's XMM half as the published `[lo, hi)` range, or `None` when
    /// the frame does not reserve it (no image at all, or a sized one, which
    /// never does: this tier names no XMM register in a frame state).
    pub(in crate::ir_lower) fn deopt_xmm_image(&self) -> Option<(i32, i32)> {
        if self.deopt_regs_bytes < DEOPT_IMAGE_FULL_BYTES {
            return None;
        }
        deopt_xmm_image_range(self.deopt_regs_base)
    }

    /// The image's GPR half as the published `[lo, hi)` range (`deopt_gpr_lo`
    /// / `deopt_gpr_hi`), or `(0, 0)` with no image. `gpr[0]` is the deepest
    /// word, at `deopt_regs_base`, so `hi == deopt_regs_base + 8` whatever the
    /// size -- the convention `conservative_roots` (`deopt_image_rbp_slot`,
    /// `ir_tail_word`) derives `gpr[0]` and the RBP image `gpr[5]` from.
    pub(in crate::ir_lower) fn deopt_gpr_image(&self) -> (i32, i32) {
        if self.deopt_regs_base == 0 {
            return (0, 0);
        }
        let words = (self.deopt_regs_bytes / 8).clamp(1, 16);
        (
            self.deopt_regs_base - (words - 1) * 8,
            self.deopt_regs_base + 8,
        )
    }

    /// Round 16 wave 11 (lane frames2, FR16P-1 / backlog FR-B2;
    /// `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED`, default OFF): shrink the 256-byte
    /// deopt register image to the words a deopt exit of this body can name.
    ///
    /// Called once, after the residency plan is installed (`set_residency`)
    /// and before anything is emitted, because the plan is what decides which
    /// registers a frame state can name and the prologue is what carves the
    /// frame. This tier names only general-purpose registers, and only ones a
    /// value is assigned to (`register_frame_value`: an exclusively owned
    /// `gp_reg_of`; the mode-exit polls: a resident `gp_reg_of`), all of which
    /// `ls_saved_gp` (the frame's whole-method register set) records. So the
    /// image keeps `gpr[0..=k]`, `k` the highest such register and at least 5
    /// (the RBP image the collector's residue test reads, `gpr[5]`), and drops
    /// the XMM half; with no register in the plan it keeps nothing.
    ///
    /// Only the image changes size: it is the deepest region below the save
    /// bands and above the argument staging, so the cut (a multiple of 16)
    /// comes off `frame_size` and the staging/shadow offsets, while
    /// `spill_cap_off`, every spill, save and bookkeeping offset stay where
    /// `Lowerer::new` put them. The image still abuts the staging region.
    pub(in crate::ir_lower) fn size_deopt_image(&mut self, on: bool) {
        if !on || self.deopt_regs_base <= 0 || self.deopt_regs_bytes <= 0 {
            return;
        }
        let want = sized_deopt_image_bytes(&deopt_image_named_gprs(&self.ls_saved_gp));
        if want >= self.deopt_regs_bytes {
            return;
        }
        let cut = self.deopt_regs_bytes - want;
        debug_assert_eq!(cut % 16, 0, "the cut must keep the frame 16-aligned");
        if cut % 16 != 0 {
            return;
        }
        self.frame_size -= cut;
        self.args_stage_top_off -= cut;
        // Round 16 wave 17 (lane frames2): the planned staging clears move
        // with the staging region.
        self.rebase_planned_stage_clears(cut);
        self.deopt_regs_bytes = want;
        self.deopt_regs_base = if want > 0 {
            self.spill_cap_off + self.saved_xmm_bytes + self.saved_gpr_bytes + want
        } else {
            0
        };
        // Cast: the widest call's argument count, far below i32::MAX.
        let stage = self.args_stage_words as i32 * 8;
        debug_assert!(
            want == 0 || self.deopt_regs_base == self.args_stage_top_off - stage,
            "the sized deopt image must still abut the argument staging region"
        );
        debug_assert!(
            (0u8..16)
                .filter(|r| deopt_image_named_gprs(&self.ls_saved_gp)[*r as usize])
                .all(|r| self.deopt_image_holds_gpr(r)),
            "the sized image must hold every register the plan hands out"
        );
    }

    /// Round 16 wave 17 (lane frames2, PC-1 of
    /// `jit-r16-polyclear-proposals-RETIRED-20261005.md`;
    /// `CRATONVM_JIT_IR_SAVE_BANDS_SIZED`, default OFF): size the callee-saved
    /// XMM and GPR save bands to the registers this body saves, and take the
    /// rest out of the frame. Returns the bytes cut (0 when nothing changed).
    ///
    /// `Lowerer::new` reserves one slot per register of each file (XMM6/XMM7
    /// on Win64, none on SysV; five or seven GPRs), but a body saves only what
    /// its residency plan hands out (`ls_saved_fp` / `ls_saved_gp`, the
    /// snapshot `saved_xmm_regs` / `saved_gpr_regs` already filter on). An
    /// unused slot is never written by this activation, so the band scan roots
    /// whatever the previous occupant of the stack left there, and the
    /// fresh-frame clear zeroes it on every call. `R16Bench poly`'s `area()`
    /// saves one GPR of five; `fib` two.
    ///
    /// Called once, after `set_residency` (the plan decides what is saved)
    /// and before `size_deopt_image` and the prologue. The bands sit between
    /// the spill cap and the deopt image, so the cut comes off everything
    /// deeper: `frame_size`, the image base and the staging top move by it;
    /// `spill_cap_off` (`callee_saved_lo`), every spill, local and bookkeeping
    /// offset stay. The used slots are packed from the shallow end
    /// (`save_bands_packed`), so every reader of a save slot -- the prologue,
    /// every epilogue, the OSR entry stubs, the fresh-frame clear's
    /// `_SKIP_WRITTEN` and the published `ir_saved_gpr_slots` -- reads the
    /// packed offset through the same two iterators. The XMM band shrinks by
    /// 16 per unused register; the GPR band by the unused bytes rounded DOWN
    /// to 16, so the frame keeps its alignment (a slot that rounding keeps is
    /// cleared as before).
    ///
    /// The collector loses nothing it scanned: the removed words are no
    /// longer in the frame (`osr_frame_size` is `frame_size`), the clear sizes
    /// itself from `frame_size`, and the published layout derives the image
    /// and staging geometry from `callee_saved_lo`, `deopt_gpr_*` and
    /// `frame_size` (`ir_tail_word`), never from the file sizes.
    pub(in crate::ir_lower) fn size_save_bands(&mut self, on: bool) -> i32 {
        if !on || self.save_bands_packed {
            return 0;
        }
        let xmm_used = if self.saved_xmm_bytes > 0 {
            self.saved_xmm_file
                .iter()
                .filter(|&&r| self.ls_saved_fp.get(r as usize).copied().unwrap_or(true))
                .count()
        } else {
            0
        };
        let gpr_used = if self.saved_gpr_bytes > 0 {
            ir_gp_file()
                .iter()
                .filter(|&&r| self.ls_saved_gp.get(r as usize).copied().unwrap_or(true))
                .count()
        } else {
            0
        };
        // Casts: at most seven registers per file.
        let xmm_cut = (self.saved_xmm_bytes - xmm_used as i32 * 16).max(0);
        let gpr_cut = (self.saved_gpr_bytes - gpr_used as i32 * 8).max(0) & !15;
        let cut = xmm_cut + gpr_cut;
        if cut <= 0 || cut % 16 != 0 {
            return 0;
        }
        let before = (self.frame_size, self.saved_xmm_bytes, self.saved_gpr_bytes);
        self.saved_xmm_bytes -= xmm_cut;
        self.saved_gpr_bytes -= gpr_cut;
        self.save_bands_packed = true;
        self.frame_size -= cut;
        self.args_stage_top_off -= cut;
        if self.deopt_regs_base > 0 {
            self.deopt_regs_base -= cut;
        }
        self.rebase_planned_stage_clears(cut);
        debug_assert!(
            self.deopt_regs_base == 0
                || self.deopt_regs_base
                    == self.spill_cap_off
                        + self.saved_xmm_bytes
                        + self.saved_gpr_bytes
                        + self.deopt_regs_bytes,
            "the deopt image must still begin where the save bands end"
        );
        // Cast: the widest call's argument count, far below i32::MAX.
        let stage = self.args_stage_words as i32 * 8;
        debug_assert!(
            self.deopt_regs_base == 0 || self.deopt_regs_base == self.args_stage_top_off - stage,
            "the deopt image must still abut the argument staging region"
        );
        debug_assert!(
            self.saved_xmm_regs()
                .chain(self.saved_gpr_regs())
                .all(|(_, off)| off > self.spill_cap_off
                    && off <= self.spill_cap_off + self.saved_xmm_bytes + self.saved_gpr_bytes),
            "every save slot must lie inside the sized bands"
        );
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-save-bands sized: xmm {} -> {} gpr {} -> {} bytes (frame {} -> {} bytes)",
                before.1,
                self.saved_xmm_bytes,
                before.2,
                self.saved_gpr_bytes,
                before.0,
                self.frame_size
            );
        }
        cut
    }

    /// Round 16 wave 17 (lane frames2; kill switch
    /// `CRATONVM_JIT_IR_STAGE_CLEAR_REBASE=0`): move the argument-staging
    /// words [`Self::plan_dead_home_clears`] recorded by `cut` bytes, after a
    /// layout step moved the staging region that much shallower.
    ///
    /// The plan runs before the residency plan, so it records offsets against
    /// `Lowerer::new`'s layout. Its two kinds of word are told apart by the
    /// spill cap, which no layout step moves: a reference home is at or above
    /// it (`home_off`), a staging word strictly below (`stage_off`). Only the
    /// second kind moves, and every layout step cuts bytes between the cap and
    /// the staging region, so each staging word moves by exactly `cut` and
    /// stays strictly below the cap.
    pub(in crate::ir_lower) fn rebase_planned_stage_clears(&mut self, cut: i32) {
        if cut == 0 || self.dead_home_clears.is_empty() || !ir_stage_clear_rebase_enabled() {
            return;
        }
        let cap = self.spill_cap_off;
        for offs in self.dead_home_clears.values_mut() {
            for off in offs.iter_mut() {
                if *off > cap {
                    *off -= cut;
                    debug_assert!(
                        *off > cap,
                        "a rebased staging word stays below the spill cap"
                    );
                }
            }
        }
    }

    /// Can no exit of this body ever read the deopt register image? Round 16
    /// wave 12 (lane polyclear, FR2P-1). A prediction made before the first
    /// byte is emitted, after the residency plan (`set_residency`), from the
    /// same tables the home-drop decisions already trust:
    ///
    /// * **loop-free** (no cycle in the block graph): no back-edge poll, so no
    ///   poll mode exit (`back_edge_mode_exit_state`, the only poll exit that
    ///   names a resident register; the method-entry poll never has one), no
    ///   loop-header state and no OSR entry stub with its re-check exit;
    /// * **no `Op::Call` and no OSR node**: no post-call exit site
    ///   (`post_call_exit_state`, which names resident registers) and no
    ///   frame state of a Java call's exception exit;
    /// * **no spliced callee body**: every frame state is the method's own, so
    ///   `compute_deopt_named_reachable` (which reads `graph.safepoints`) sees
    ///   every snapshot a deopt point is built from;
    /// * **every `Int`/`Long` value a deopt could name by its register
    ///   (`deopt_nameable`) AND a snapshot a deopt can reach names
    ///   (`deopt_named_reachable`) kept its home word** (`home_dropped` is
    ///   false). Without an image `register_frame_value`, the one remaining
    ///   producer of a `FrameValue::Register*`, answers `None`, and
    ///   `frame_value_for` describes such a value by its home word -- the
    ///   description every value that does NOT own its register exclusively
    ///   (most of them) already gets at every reachable deopt: a kept home is
    ///   written at the value's definition (`publish_at_def` / `store_rax`;
    ///   the carries skip it only for a value no reachable snapshot names, and
    ///   then mark the home dropped), before any snapshot naming it. A value
    ///   whose home was dropped BECAUSE it was nameable has no other location,
    ///   so it keeps the image. (`Ref` and FP values are never named by
    ///   register on this tier.)
    /// * **and no such value is a monitor object** (a live monitor operand, or
    ///   named in a snapshot's `monitors`). Round 16 wave 13 (lane polyclear):
    ///   `monitor_object_value` re-spells a monitor's word as
    ///   `StackSlotRef`, so the same frame word described by its home in the
    ///   locals or the stack (`StackSlot`, `int`) is a `SlotTypeConflict` --
    ///   and an `int` slot the active map names -- to the deopt-metadata
    ///   verifier, which refuses the compile. With the image the local is
    ///   `Register(r)`, which names no frame word. Witness: the hand-built
    ///   `aload_0; monitorenter; aload_0; monitorexit` bodies (their `this`
    ///   is an `int`-typed `Op::Param`), which stopped lowering when the
    ///   switch went default ON (w13a).
    ///
    /// Why a wrong answer cannot produce a wrong frame: every reader of the
    /// image fails closed without one. `register_frame_value` answers `None`
    /// (`deopt_image_holds_gpr`), so `frame_value_for` falls to the home word,
    /// and a dropped home there latches `UnallocatedValue` and refuses the
    /// compile; the poll and post-call exits refuse themselves
    /// (`names_a_register && deopt_regs_base <= 0`); the stub stores nothing
    /// and hands `ir_deopt_entry` NULL. A misprediction costs a compile or an
    /// exit, never a value.
    pub(in crate::ir_lower) fn deopt_image_never_read(&self) -> bool {
        if schedule_block_cfg_has_cycle(self.schedule) || !self.spliced_ranges.is_empty() {
            return false;
        }
        if self.graph.nodes.iter().any(|n| {
            matches!(
                n.op,
                Op::Call { .. }
                    | Op::OsrEntryFlag { .. }
                    | Op::OsrLocal(_)
                    | Op::OsrMemory
                    | Op::CatchEntryFlag { .. }
                    | Op::CaughtException
            )
        }) {
            return false;
        }
        (0..self.deopt_nameable.len()).all(|id| {
            let nameable = self.deopt_nameable[id];
            let reachable = self.deopt_named_reachable.get(id).copied().unwrap_or(true);
            let primitive = matches!(
                self.graph.nodes.get(id).map(|n| n.ty),
                Some(IrType::Int) | Some(IrType::Long)
            );
            if !(nameable && reachable && primitive) {
                return true;
            }
            let dropped = self.home_dropped.get(id).copied().unwrap_or(true);
            let Ok(nid) = NodeId::try_from(id) else {
                return false;
            };
            let monitor = self.live_monitor_operands.contains(&nid)
                || self
                    .graph
                    .safepoints
                    .iter()
                    .any(|sp| sp.monitors.contains(&nid));
            !dropped && !monitor
        })
    }

    /// Round 16 wave 12 (lane polyclear, FR2P-1;
    /// `CRATONVM_JIT_IR_DEOPT_IMAGE_EXITLESS`, default OFF): reserve no deopt
    /// register image in a body [`Self::deopt_image_never_read`] says no exit
    /// can read. Called right after [`Self::size_deopt_image`] and before the
    /// prologue, on whatever that left (the full image or a sized one).
    ///
    /// The layout is the one `size_deopt_image` already produces for an empty
    /// residency plan: the image is the deepest region above the argument
    /// staging, so its whole size (a multiple of 16) comes off `frame_size`
    /// and `args_stage_top_off`, `deopt_regs_base` becomes 0 (the stub's and
    /// the publication's "no image"), and every shallower offset stays. The
    /// published layout then names no image (`deopt_gpr_image` answers
    /// `(0, 0)`, `deopt_xmm_image` `None`).
    ///
    /// The collector loses nothing it scanned: the removed words are no
    /// longer in the frame's band (`[rbp - frame_size, rbp)`), and every word
    /// still in it is cleared exactly as before by the fresh-frame clear,
    /// which sizes itself from `frame_size`. Returns whether it dropped one.
    /// Round 17 (CG16P-4 of `jit-r16-codegen-proposals-DONE-20261005.md`;
    /// [`ir_const_home_drop_enabled`]): drop the home word of every `int` /
    /// `long` constant that holds no register, so `Op::Const` emits nothing.
    ///
    /// Every reader of such a constant already folds it -- `gp_load_value`
    /// and the ALU arms take an immediate, a frame state describes it as one
    /// (`frame_value_for`), an OSR entry need not seed it, and an edge copy
    /// takes it as `MOV r, imm` (`emit_copy_op`) -- so the stores feed nothing
    /// on the common path (`bigA` wrote ~100 per call). A reader that still
    /// wants the word goes through `slot_of`, which refuses a dropped home and
    /// fails THIS attempt; `lower_inner_with_mode_exit_polls` then lowers once
    /// more with every home kept (the deopt-image fallback's retry), so a
    /// misprediction costs a second lowering, never a wrong answer.
    ///
    /// Needs the register-read phi copies and the constant immediates (the
    /// two readers that take the immediate instead of the word). Returns
    /// whether anything was dropped.
    pub(in crate::ir_lower) fn drop_const_homes(&mut self, on: bool) -> bool {
        if !on || !self.phi_copy_regs_on || !self.const_imm_on || self.mir_mode != MirMode::Off {
            return false;
        }
        let mut dropped = 0usize;
        for id in 0..self.home_dropped.len() {
            let Some(node) = self.graph.nodes.get(id) else {
                continue;
            };
            let drop_ref_null = node.ty == IrType::Ref
                && matches!(node.op, Op::Const(0))
                && self.const_has_no_stack_readers(id as NodeId);
            if !matches!(node.op, Op::Const(_))
                || (!matches!(node.ty, IrType::Int | IrType::Long) && !drop_ref_null)
            {
                continue;
            }
            if self.gp_reg_of.get(id).copied().flatten().is_some()
                || self.ls_managed.get(id).copied().unwrap_or(false)
                || self.home_dropped[id]
                || self.elide_excluded.get(id).copied().unwrap_or(false)
            {
                continue;
            }
            self.home_dropped[id] = true;
            dropped += 1;
        }
        self.const_homes_dropped = dropped;
        dropped > 0
    }

    /// Is `id` a constant whose home [`Self::drop_const_homes`] dropped?
    pub(in crate::ir_lower) fn is_dropped_const(&self, id: NodeId) -> bool {
        self.home_dropped.get(id as usize).copied().unwrap_or(false)
            && self.gp_reg_of.get(id as usize).copied().flatten().is_none()
            && self.graph.nodes.get(id as usize).is_some_and(|n| {
                matches!(n.op, Op::Const(_))
                    && (matches!(n.ty, IrType::Int | IrType::Long)
                        || (n.ty == IrType::Ref && matches!(n.op, Op::Const(0))))
            })
    }

    pub(in crate::ir_lower) fn drop_unread_deopt_image(&mut self, on: bool) -> bool {
        if !on || self.deopt_regs_base <= 0 || self.deopt_regs_bytes <= 0 {
            return false;
        }
        let cut = self.deopt_regs_bytes;
        if cut % 16 != 0 || !self.deopt_image_never_read() {
            return false;
        }
        let before = self.frame_size;
        self.frame_size -= cut;
        self.args_stage_top_off -= cut;
        // Round 16 wave 17 (lane frames2): a no-op today (an exitless body has
        // no `Op::Call`, so no planned staging clear), kept so the rule "every
        // layout cut moves the planned staging clears" has no exception.
        self.rebase_planned_stage_clears(cut);
        self.deopt_regs_bytes = 0;
        self.deopt_regs_base = 0;
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-deopt-image dropped: no exit can read it (frame {before} -> {} bytes)",
                self.frame_size
            );
        }
        true
    }

    /// Install the level-2 machine list. Called once, after construction and
    /// before any emission, only when a machine-level flag is on.
    pub(in crate::ir_lower) fn set_mir(&mut self, plan: MirPlan, mode: MirMode) {
        self.mir = Some(plan);
        self.mir_mode = mode;
        if mode != MirMode::Off {
            self.home_reload_forward_on = false;
            // A tile reproduces the arms' RCX shape; no second byte form here.
            self.operand_in_place_on = false;
            // Nor a skipped read: the tile list expects the arm's bytes.
            self.publish_reload_elide_on = false;
        }
    }

    /// The XMM register `id`'s value is CURRENTLY resident in, if any.
    ///
    /// Gated on `reg_live`, not on `reg_of`: a value is only readable from a
    /// register once its definition site has published it there. See the field
    /// comment for why that direction is the safe one.
    #[track_caller]
    pub(in crate::ir_lower) fn resident_xmm(&self, id: NodeId) -> Option<u8> {
        // Wave 64 (lane L2): the token-read guard; a no-op while it is off.
        self.token_read_check(id);
        if !self.reg_live.get(id as usize).copied().unwrap_or(false) {
            return None;
        }
        self.reg_of.get(id as usize).copied().flatten()
    }

    /// MOVAPS `dst`, `src` — a full 128-bit register copy.
    ///
    /// `MOVAPS` rather than `MOVSS`/`MOVSD` so the copy has no false
    /// dependency on `dst`'s previous contents and one encoding serves both
    /// widths: the low 32 or 64 bits are what every consumer reads, and a bit
    /// copy preserves them exactly (NaN payloads included — this must never be
    /// an arithmetic move).
    pub(in crate::ir_lower) fn fp_reg_move(&mut self, dst: u8, src: u8) {
        if dst == src {
            return;
        }
        // REX.R for dst >= 8, REX.B for src >= 8. `IR_LOWER_LS_XMMS` is
        // XMM2–XMM7 and the value tier is XMM0/XMM1, so no REX is emitted
        // today; encoding it anyway keeps the helper correct if the file grows.
        // `MOVAPS` has no mandatory SSE prefix, so REX is the first byte here —
        // see the prefix-order note above `fp_load`.
        let rex = 0x40u8 | (((dst >= 8) as u8) << 2) | ((src >= 8) as u8);
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf
            .emit(&[0x0F, 0x28, 0xC0 | ((dst & 7) << 3) | (src & 7)]);
    }

    /// `Math.sqrt(D)` and FP `Math.min`/`max` (JIT round 19, lane irops19,
    /// R15B-31), into XMM0 and out through `fp_store_value`. Uses only the
    /// value tier's XMM0/XMM1: XMM2-XMM7 belong to the register cache.
    ///
    /// `MINSD`/`MAXSD` alone are NOT `Math.min`/`max`: they return the SECOND
    /// operand when either is NaN or both are zero. The JDK body returns the
    /// NaN ARGUMENT (`a` first), and orders -0.0 strictly below +0.0. So:
    ///
    /// ```text
    ///   UCOMIS a, b ; JP unordered ; JNE ordered
    ///   equal:     min: a OR b  / max: a AND b  -- +-0.0 sign rule; identical
    ///              bits otherwise, so the identity for every other equal pair
    ///   ordered:   MINS/MAXS a, b                -- exact for unequal operands
    ///   unordered: UCOMIS a, a ; JP done (a is NaN: keep it) ; a = b
    /// ```
    ///
    /// The same answers, bit for bit, as the single-pass
    /// `MATH_MIN_FLOAT_INTRINSIC` arm (`x64/op_invoke.rs`), which needs two
    /// more XMM registers this tier does not have.
    pub(in crate::ir_lower) fn lower_fp_math_scalar(
        &mut self,
        id: NodeId,
        slot: i32,
        sop: ScalarOp,
        a: NodeId,
        b: Option<NodeId>,
    ) {
        if matches!(sop, ScalarOp::SqrtD) {
            self.fp_load_value(XMM0, a, true);
            self.buf.emit(&[0xF2, 0x0F, 0x51, 0xC0]); // SQRTSD XMM0, XMM0
            self.fp_store_value(id, slot, XMM0, true);
            return;
        }
        // JIT round 19 wave 2 (lane irops2, IROPS19-2): `Math.floor`/`ceil`/
        // `rint` -- SSE4.1 `ROUNDSD XMM0, XMM0, imm8` with the single-pass
        // arm's immediates (bit 3 suppresses the precision exception, which
        // MXCSR masks anyway). The recognizer admitted the row only with
        // SSE4.1 present (`try_ir_scalar_intrinsic`).
        if let Some(imm8) = match sop {
            ScalarOp::FloorD => Some(0x09u8), // toward -inf
            ScalarOp::CeilD => Some(0x0Au8),  // toward +inf
            ScalarOp::RintD => Some(0x08u8),  // nearest, ties to even
            _ => None,
        } {
            self.fp_load_value(XMM0, a, true);
            self.buf.emit(&[0x66, 0x0F, 0x3A, 0x0B, 0xC0, imm8]); // ROUNDSD XMM0, XMM0, imm8
            self.fp_store_value(id, slot, XMM0, true);
            return;
        }
        let (dbl, is_min) = match sop {
            ScalarOp::MinF => (false, true),
            ScalarOp::MaxF => (false, false),
            ScalarOp::MinD => (true, true),
            ScalarOp::MaxD => (true, false),
            _ => {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: lower_fp_math_scalar given a non-FP-math family",
                )));
                return;
            }
        };
        let Some(b) = b else {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: an FP min/max node with one input",
            )));
            return;
        };
        // JIT round 19 wave 4 (lane irops3, IROPS19-5), opt-in: both operands
        // register-resident and the host has AVX -- the branch-free form
        // (`fp_minmax_avx_bytes`), which reads them in place.
        if ir_fp_minmax_avx_enabled() {
            if let (Some(ra), Some(rb)) = (self.resident_xmm(a), self.resident_xmm(b)) {
                if ![ra, rb].iter().any(|r| *r == XMM0 || *r == XMM1) {
                    let bytes = fp_minmax_avx_bytes(dbl, is_min, ra, rb);
                    self.buf.emit(&bytes);
                    self.fp_store_value(id, slot, XMM0, dbl);
                    return;
                }
            }
        }
        self.fp_load_value(XMM0, a, dbl);
        self.fp_load_value(XMM1, b, dbl);
        // 66-prefixed (PD / SD-compare) forms for double, bare (PS / SS) for float.
        let pd = |dbl: bool, op: &[u8]| -> Vec<u8> {
            let mut v = Vec::with_capacity(op.len() + 1);
            if dbl {
                v.push(0x66);
            }
            v.extend_from_slice(op);
            v
        };
        let ucomis_ab = pd(dbl, &[0x0F, 0x2E, 0xC1]); // UCOMIS XMM0, XMM1
        self.buf.emit(&ucomis_ab);
        let to_unordered = self.emit_jcc_rel32(0x8A); // JP
        let to_ordered = self.emit_jcc_rel32(0x85); // JNE
                                                    // Equal (both zeros included): OR keeps a -0.0 sign from either side
                                                    // (min), AND keeps it only when both are -0.0 (max).
        let bitop = if is_min { 0x56 } else { 0x54 }; // ORP / ANDP
        let eq_op = pd(dbl, &[0x0F, bitop, 0xC1]);
        self.buf.emit(&eq_op);
        let done_eq = self.emit_jmp_rel32();
        // Ordered and unequal: the SSE instruction is exactly Java's answer.
        self.patch_rel32_to_here(to_ordered);
        let prefix = if dbl { 0xF2 } else { 0xF3 };
        let minmax = if is_min { 0x5D } else { 0x5F };
        self.buf.emit(&[prefix, 0x0F, minmax, 0xC1]); // MIN/MAX S{S,D} XMM0, XMM1
        let done_ord = self.emit_jmp_rel32();
        // Unordered: return the NaN argument itself, `a` first.
        self.patch_rel32_to_here(to_unordered);
        let ucomis_aa = pd(dbl, &[0x0F, 0x2E, 0xC0]); // UCOMIS XMM0, XMM0
        self.buf.emit(&ucomis_aa);
        let a_is_nan = self.emit_jcc_rel32(0x8A); // JP: keep a
        self.buf.emit(&[0x0F, 0x28, 0xC1]); // MOVAPS XMM0, XMM1 (b, exact bits)
        self.patch_rel32_to_here(a_is_nan);
        self.patch_rel32_to_here(done_eq);
        self.patch_rel32_to_here(done_ord);
        self.fp_store_value(id, slot, XMM0, dbl);
    }

    /// `Math.fma(a, b, c)` (JIT round 19 wave 2, lane irops2, IROPS19-2):
    /// FMA3 `VFMADD213S{D,S} XMM0, XMM1, src` = `XMM1 * XMM0 + src` =
    /// `b * a + c`, rounded once -- the single-pass inline arm's instruction
    /// (`x64/op_invoke.rs`, `MATH_FMA_*_INTRINSIC`).
    ///
    /// The single-pass arm stages `c` in XMM2; this tier may not write XMM2
    /// (XMM2-XMM7 are the register cache), so the third operand is READ in
    /// place instead: `c`'s resident register when it has one, else its home
    /// word as the `m64`/`m32` operand (`fp_load_value`'s two sources, with
    /// no copy). Nothing but XMM0/XMM1 is written. The recognizer admitted
    /// the row only where the inline FMA3 form is enabled.
    pub(in crate::ir_lower) fn lower_fp_math_c_call(
        &mut self,
        id: NodeId,
        slot: i32,
        fn_ptr: usize,
        arity: usize,
        inputs: &[NodeId],
    ) {
        self.fp_load_value(XMM0, inputs[0], true);
        if arity == 2 {
            self.fp_load_value(XMM1, inputs[1], true);
        }
        self.emit_mov_reg_imm64(RAX, fn_ptr as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.fp_store_value(id, slot, XMM0, true);
    }

    pub(in crate::ir_lower) fn lower_fp_fma_scalar(
        &mut self,
        id: NodeId,
        slot: i32,
        sop: ScalarOp,
        inputs: &[NodeId],
    ) {
        let &[a, b, c] = inputs else {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: an FMA node without three inputs",
            )));
            return;
        };
        let dbl = match sop {
            ScalarOp::FmaD => true,
            ScalarOp::FmaF => false,
            _ => {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: lower_fp_fma_scalar given a non-FMA family",
                )));
                return;
            }
        };
        self.fp_load_value(XMM0, a, dbl);
        self.fp_load_value(XMM1, b, dbl);
        // VEX3: C4 | R X B mmmmm=00010 (0F38) | W vvvv=~XMM1 L=0 pp=01 (66).
        // W=1 is the SD form, W=0 the SS form.
        let wvvvv = if dbl { 0xF1u8 } else { 0x71u8 };
        match self.resident_xmm(c) {
            Some(r) => {
                // B̄ is clear (bit 5) to reach XMM8-XMM15 through r/m.
                let rxb = if r >= 8 { 0xC2u8 } else { 0xE2u8 };
                self.buf.emit(&[0xC4, rxb, wvvvv, 0xA9, 0xC0 | (r & 7)]);
            }
            None => {
                let off = self.slot_of(c);
                self.buf.emit(&[0xC4, 0xE2, wvvvv, 0xA9]);
                self.emit_rbp_modrm_disp(XMM0, off);
            }
        }
        self.fp_store_value(id, slot, XMM0, dbl);
    }

    // Load `id`'s value into `xmm`, from its resident register when it has
    // one and from its home word otherwise.
    //
    // The read half of the cache. Every FP operand read in `lower_data_node`
    // goes through here; a site left calling `fp_load(self.slot_of(id))`
    // directly is simply not accelerated.
    // JIT round 19 wave 2 (lane irops2): this doc and `#[track_caller]` were
    // stranded above `lower_fp_math_scalar` by the wave-1 insertion.
    #[track_caller]
    pub(in crate::ir_lower) fn fp_load_value(&mut self, xmm: u8, id: NodeId, is_double: bool) {
        // Bound out of the scrutinee position: every read of `self` here must
        // finish before the emitters take `&mut self`.
        let resident = self.resident_xmm(id);
        match resident {
            Some(src) => self.fp_reg_move(xmm, src),
            None => {
                let off = self.slot_of(id);
                self.fp_load(xmm, off, is_double);
            }
        }
    }

    /// The register `id`'s value has been ASSIGNED, whether or not its
    /// definition has published it yet. Only the two publishing sites may use
    /// this; every reader goes through [`Self::resident_xmm`].
    pub(in crate::ir_lower) fn assigned_xmm(&self, id: NodeId) -> Option<u8> {
        self.reg_of.get(id as usize).copied().flatten()
    }

    /// Mark `id` readable from its assigned register.
    pub(in crate::ir_lower) fn mark_reg_live(&mut self, id: NodeId) {
        if let Some(cell) = self.reg_live.get_mut(id as usize) {
            *cell = true;
        }
    }

    /// Write `id`'s result from `xmm`: ALWAYS to the home word, and
    /// additionally into its resident register.
    ///
    /// The write-through invariant lives here. The home store is emitted
    /// unconditionally and first, so the frame image is complete at every
    /// instruction boundary — which is what lets `emit_safepoint_map`,
    /// `build_deopt_points` and `emit_phi_copies` stay untouched.
    pub(in crate::ir_lower) fn fp_store_value(
        &mut self,
        id: NodeId,
        slot: i32,
        xmm: u8,
        is_double: bool,
    ) {
        self.fp_store(slot, xmm, is_double);
        let dst = self.assigned_xmm(id);
        if let Some(dst) = dst {
            self.fp_reg_move(dst, xmm);
            self.ls_spills += 1;
            self.mark_reg_live(id);
        }
    }

    /// An FP `ADD`/`SUB`/`MUL`/`DIV` (`op`, the SSE opcode byte) of `dst_in`
    /// and `src_in` into `id`. Under `CRATONVM_JIT_IR_LOOP_CARRIED_FP_REGS`,
    /// when `id` has a register and `dst_in` is resident, it is computed in
    /// place -- `MOVAPS d, r; OPSS d, s; MOVSS [home], d` -- instead of through
    /// the XMM0/XMM1 scratch pair and a copy out, which put two more moves on
    /// a loop's FP recurrence. Operand order is the caller's, unchanged (it
    /// decides which NaN an all-NaN pair answers). Declined, to the scratch
    /// path, when `id`'s register is `src_in`'s and not `dst_in`'s: the copy
    /// into it would overwrite the second operand before the op reads it.
    pub(in crate::ir_lower) fn fp_binop_value(
        &mut self,
        id: NodeId,
        slot: i32,
        op: u8,
        dst_in: NodeId,
        src_in: NodeId,
        is_double: bool,
    ) {
        let in_place = ir_loop_carried_fp_regs_enabled()
            .then(|| Some((self.assigned_xmm(id)?, self.resident_xmm(dst_in)?)))
            .flatten();
        if let Some((d, r)) = in_place {
            let s_reg = self.resident_xmm(src_in);
            if s_reg != Some(d) || r == d {
                let src = match s_reg {
                    Some(s_reg) => s_reg,
                    None => {
                        self.fp_load_value(XMM1, src_in, is_double);
                        XMM1
                    }
                };
                self.fp_reg_move(d, r);
                self.fp_binop(op, d, src, is_double);
                self.fp_store(slot, d, is_double);
                self.mark_reg_live(id);
                self.fp_in_place_ops += 1;
                return;
            }
        }
        self.fp_load_value(XMM0, dst_in, is_double);
        self.fp_load_value(XMM1, src_in, is_double);
        self.fp_binop(op, XMM0, XMM1, is_double);
        self.fp_store_value(id, slot, XMM0, is_double);
    }

    /// Publish a result that was computed in RAX and already stored to its home
    /// word (`Op::ConstF`, FP `Op::Neg`, FP `Op::Param`) into its register.
    ///
    /// A memory → register transition, and counted as one: the value's only
    /// materialisation is the home word this reads back. It is still a win in
    /// a loop, where the alternative is that load once per use.
    pub(in crate::ir_lower) fn publish_fp_from_slot(
        &mut self,
        id: NodeId,
        slot: i32,
        is_double: bool,
    ) {
        let dst = self.assigned_xmm(id);
        if let Some(dst) = dst {
            self.fp_load(dst, slot, is_double);
            self.ls_reloads += 1;
            self.mark_reg_live(id);
        }
    }

    // ── The general-purpose half of the same cache ──────────────────────
    //
    // Identical contract to the FP accessors above, over `IR_LOWER_LS_GPRS`
    // rather than `IR_LOWER_LS_XMMS`: write-through, gated on `gp_reg_live` so
    // a value is readable from a register only once its definition published it
    // there, and `None` everywhere means the read falls back to the home word.
    //
    // This is the half that reaches an `int` loop counter — the gap the FP-only
    // wiring's own doc comment named, and the reason this backend's bodies
    // measured slower than the single-pass bodies they supersede.

    /// The GP register `id`'s value is CURRENTLY resident in, if any.
    #[track_caller]
    pub(in crate::ir_lower) fn resident_gpr(&self, id: NodeId) -> Option<u8> {
        // Wave 64 (lane L2): the token-read guard; a no-op while it is off.
        self.token_read_check(id);
        if !self.gp_reg_live.get(id as usize).copied().unwrap_or(false) {
            return None;
        }
        self.gp_reg_of.get(id as usize).copied().flatten()
    }

    /// The register `id`'s value has been ASSIGNED, whether or not its
    /// definition has published it yet. Only publishing sites may use this;
    /// every reader goes through [`Self::resident_gpr`].
    pub(in crate::ir_lower) fn assigned_gpr(&self, id: NodeId) -> Option<u8> {
        self.gp_reg_of.get(id as usize).copied().flatten()
    }

    /// Mark `id` readable from its assigned GP register — and mark whoever held
    /// that register before UNREADABLE.
    ///
    /// The second half is the whole of the interlock. Setting the per-node bit
    /// says "id's value is in its register"; without dropping the previous
    /// owner's bit, `resident_gpr` goes on answering that register for a node
    /// whose value this very instruction has just overwritten. See
    /// [`Self::gp_reg_owner`] for the edge that makes two nodes share one
    /// register while both are still being read, and for what it produced.
    ///
    /// A node re-published into its own register is not a transfer and clears
    /// nothing; that is the ordinary definition-site case and the common path.
    pub(in crate::ir_lower) fn mark_gp_reg_live(&mut self, id: NodeId) {
        if let Some(reg) = self.assigned_gpr(id) {
            if let Some(slot) = self.gp_reg_owner.get_mut(reg as usize) {
                self.gp_zx32_mask &= !(1u16 << reg);
                let previous = slot.replace(id);
                if let Some(previous) = previous {
                    if previous != id {
                        if let Some(cell) = self.gp_reg_live.get_mut(previous as usize) {
                            *cell = false;
                        }
                    }
                }
            }
        }
        if let Some(cell) = self.gp_reg_live.get_mut(id as usize) {
            *cell = true;
        }
    }

    /// Drop every cached REFERENCE copy, because a collector may have run.
    ///
    /// The register file here is a write-through read cache over a frame slot.
    /// For an `int` that cache never goes stale — nothing but this body writes
    /// the slot. For a reference it does: a moving collector rewrites the slot
    /// under a frame it did not stop, and it cannot rewrite a copy in a
    /// register no oop map names. Reading that copy afterwards dereferences the
    /// object's OLD address, which is the failure class
    /// `fix/netty-close-ordering-20260905` was.
    ///
    /// So every point at which control can leave this body and come back drops
    /// the copies, and the next read reloads the word the collector updated.
    /// `except` is the node the site itself just defined — an `Op::Call`'s
    /// result is produced AFTER the collection, so it is fresh, and dropping it
    /// would take back the one promotion a call site can offer.
    ///
    /// Cheap enough to be unconditional in shape: the loop runs over the
    /// registers (`gp_reg_owner` has one slot per architectural GPR, sixteen),
    /// not over the graph.
    pub(in crate::ir_lower) fn invalidate_ref_residency(&mut self, except: Option<NodeId>) {
        self.invalidate_ref_residency_except_regs(except, 0);
    }

    pub(in crate::ir_lower) fn invalidate_ref_residency_except_regs(
        &mut self,
        except: Option<NodeId>,
        regs: u16,
    ) {
        if !self.ref_residency_on {
            return;
        }
        for slot in 0..self.gp_reg_owner.len() {
            let Some(owner) = self.gp_reg_owner[slot] else {
                continue;
            };
            if Some(owner) == except || regs & (1u16 << slot) != 0 {
                continue;
            }
            if self.graph.nodes.get(owner as usize).map(|n| n.ty) != Some(IrType::Ref) {
                continue;
            }
            if let Some(cell) = self.gp_reg_live.get_mut(owner as usize) {
                if *cell {
                    *cell = false;
                    REF_RESIDENCY_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    /// tcm21 (TCMISS21-2 census): the number of cached REFERENCE copies
    /// [`Self::invalidate_ref_residency`] would drop right now with the same
    /// `except`. Read-only; used only by a `CRATONVM_DBG_JITC` line.
    pub(in crate::ir_lower) fn live_ref_copies(&self, except: Option<NodeId>) -> usize {
        self.gp_reg_owner
            .iter()
            .flatten()
            .filter(|&&owner| {
                Some(owner) != except
                    && self.graph.nodes.get(owner as usize).map(|n| n.ty) == Some(IrType::Ref)
                    && self
                        .gp_reg_live
                        .get(owner as usize)
                        .copied()
                        .unwrap_or(false)
            })
            .count()
    }

    /// JIT round 25 (item 1b, `CRATONVM_JIT_IR_LS_CALLER_SAVED`): install the
    /// caller-saved half of the plan [`Self::set_residency`] just installed.
    /// Called once, straight after it, before anything is emitted.
    ///
    /// Two things, both only when the plan names a caller-saved register:
    ///
    /// * arms [`Self::invalidate_caller_saved_residency`], which keeps
    ///   `resident_gpr` meaning "the value is in that register HERE" for a
    ///   register a call, a poll or a helper sequence destroys without
    ///   publishing anything into it (the owner interlock in
    ///   `mark_gp_reg_live` only sees publishes);
    /// * keeps every such value's home word WRITTEN. `value_home_droppable`'s
    ///   widening clauses (no deopt can name it; the body cannot deopt) would
    ///   otherwise drop it on the strength of "an assigned register", and a
    ///   caller-saved value IS read from its home after a clobber: a fused
    ///   compare reads its operands at the branch, past nodes that may call.
    ///   A dropped home there is a `slot_of` refusal (the compile is lost),
    ///   never a wrong read, but write-through is the contract this item keeps
    ///   (item 1a owns relaxing it).
    ///
    /// Round 25 wave 2: also installs `republish` (`(after, value)` pairs from
    /// the planner, see `CallerSavedPlan::republish`) as the table
    /// [`Self::republish_caller_saved_after`] reads.
    pub(in crate::ir_lower) fn install_caller_saved_residency(
        &mut self,
        republish: Vec<(NodeId, NodeId)>,
    ) {
        self.cs_republish_after.clear();
        for (after, value) in republish {
            self.cs_republish_after
                .entry(after)
                .or_default()
                .push(value);
        }
        self.ls_caller_saved_values = self
            .gp_reg_of
            .iter()
            .flatten()
            .filter(|&&r| IR_LS_CALLER_SAVED_GPRS.contains(&r))
            .count();
        self.ls_caller_saved_on = self.ls_caller_saved_values > 0;
        if !self.ls_caller_saved_on {
            return;
        }
        for id in 0..self.gp_reg_of.len() {
            let caller_saved =
                self.gp_reg_of[id].is_some_and(|r| IR_LS_CALLER_SAVED_GPRS.contains(&r));
            if caller_saved {
                if let Some(cell) = self.home_dropped.get_mut(id) {
                    *cell = false;
                }
            }
        }
    }

    /// Forget every value cached in a caller-saved register
    /// ([`IR_LS_CALLER_SAVED_GPRS`]): the next read of it reloads its home
    /// word, which [`Self::install_caller_saved_residency`] keeps written.
    ///
    /// Called by `lower_block` at every block start, before every node that is
    /// not [`ir_op_caller_saved_transparent`] (BEFORE it, so a frame state that
    /// node describes cannot name a register it is about to destroy), before a
    /// terminator that may poll and at every block end. Between two such
    /// points only transparent nodes are lowered, and those write none of these
    /// registers, so `resident_gpr` answering one of them is a fact about the
    /// machine state at that instruction -- the meaning item 1a's queries rely
    /// on. A no-op unless the plan assigned one.
    pub(in crate::ir_lower) fn invalidate_caller_saved_residency(&mut self) {
        if !self.ls_caller_saved_on {
            return;
        }
        for &r in IR_LS_CALLER_SAVED_GPRS {
            let owner = self.gp_reg_owner.get_mut(r as usize).and_then(Option::take);
            if let Some(owner) = owner {
                if let Some(cell) = self.gp_reg_live.get_mut(owner as usize) {
                    *cell = false;
                }
            }
        }
    }

    /// Round 25 wave 2: after node `after` is lowered (every route of its
    /// arm, a call prefix's hit edge and its post-call exit site included),
    /// reload each value the planner listed for it from its home word into its
    /// caller-saved register, and mark it readable there.
    ///
    /// Two cases, one mechanism. `after` is the value's own OPAQUE definition:
    /// its arm may publish the register and then clobber it (a helper call
    /// after the store, a slow path), so the register is filled again here,
    /// after the node's last instruction, from the home the arm wrote. Or
    /// `after` is an opaque node inside the value's range, and what follows it
    /// up to the next opaque node is a run of reads the planner gave the
    /// register to (`plan_caller_saved_splits`). Either way the home word is
    /// current: write-through, and `install_caller_saved_residency` keeps the
    /// home of every caller-saved value.
    ///
    /// Skipped (the reads then go to the home, as without the plan) when a
    /// carry out of `after` is still in flight: an RAX carry or a tail carry
    /// is proved by "nothing emitted since", which this load would break.
    pub(in crate::ir_lower) fn republish_caller_saved_after(&mut self, after: NodeId) {
        let Some(values) = self.cs_republish_after.get(&after).cloned() else {
            return;
        };
        let rax_carry = matches!(self.live_carry, Some((_, _, reg, _)) if reg == RAX);
        if rax_carry || self.tail_carry.is_some() {
            self.cs_republish_skipped += values.len();
            return;
        }
        for v in values {
            let Some(reg) = self
                .assigned_gpr(v)
                .filter(|r| IR_LS_CALLER_SAVED_GPRS.contains(r))
            else {
                continue;
            };
            let Some(off) = self.node_slot.get(v as usize).copied().flatten() else {
                continue;
            };
            let off = off.get() as i32;
            // `v`'s own home store from `reg` is the last instruction emitted:
            // `reg` still holds the word (`gp_load_value`'s publish-reload elide).
            let held = self.publish_reload_elide_on
                && !self
                    .publish_reload_excluded
                    .get(v as usize)
                    .copied()
                    .unwrap_or(true)
                && self.rax_home_mirror.is_some_and(|(src, home, _, end)| {
                    src == reg && home == off && end == self.buf.pos()
                });
            if held {
                self.publish_reload_elides.push((self.buf.pos(), v));
                if let Some(cell) = self.publish_reload_elided.get_mut(v as usize) {
                    *cell = true;
                }
            } else {
                self.load_reg_from_frame(reg, off);
            }
            self.mark_gp_reg_live(v);
            self.cs_republishes += 1;
        }
    }

    /// Classify one `gp_load_value` home read for the `ir-ls-gp` line (round 25
    /// wave 2). Counts only; emits nothing.
    pub(in crate::ir_lower) fn note_gp_home_reload(&mut self, id: NodeId) {
        self.gp_home_reloads += 1;
        let ty = self.graph.nodes.get(id as usize).map(|n| n.ty);
        if ty == Some(IrType::Ref) {
            self.gp_home_reloads_ref += 1;
        } else if self.assigned_gpr(id).is_some() {
            self.gp_home_reloads_assigned += 1;
        } else {
            self.gp_home_reloads_unassigned += 1;
        }
    }

    /// Does a caller-saved value survive into block `block_idx`'s terminator?
    /// Only an `Op::If` on a block with no back edge: its arm reads the
    /// condition (or the fused compare's operands) before anything else, and
    /// no safepoint poll precedes it. The planner admits a range ending there
    /// on exactly the same test ([`caller_saved_transparent_positions`]).
    pub(in crate::ir_lower) fn caller_saved_survive_terminator(&self, block_idx: usize) -> bool {
        let Some(block) = self.schedule.blocks.get(block_idx) else {
            return false;
        };
        let Some(term) = block.terminator else {
            return false;
        };
        matches!(
            self.graph.nodes.get(term as usize).map(|n| &n.op),
            Some(Op::If)
        ) && !block.successors.iter().any(|&s| s <= block_idx)
    }

    // ── Split residency: applying one position's transitions ─────────

    /// Point `id`'s FP assignment at `reg`, or at no register at all.
    ///
    /// The ONE site that makes `reg_of` change after installation. Everything
    /// that reads `reg_of` as a whole-method property reads `ls_saved_fp`
    /// instead (see `saved_xmm_regs`), and everything that reads it per node
    /// goes through `resident_xmm` / `assigned_xmm` exactly as before — which
    /// is what makes widening the vector's meaning from "for its whole life" to
    /// "right now" an additive change rather than a new contract.
    pub(in crate::ir_lower) fn ls_set_fp_reg(&mut self, id: NodeId, reg: Option<u8>) {
        if let Some(cell) = self.reg_of.get_mut(id as usize) {
            *cell = reg;
        }
    }

    /// The GP twin of [`Self::ls_set_fp_reg`].
    pub(in crate::ir_lower) fn ls_set_gp_reg(&mut self, id: NodeId, reg: Option<u8>) {
        if let Some(cell) = self.gp_reg_of.get_mut(id as usize) {
            *cell = reg;
        }
    }

    /// `id`'s value has left its register: the home word is where it lives from
    /// this position on.
    ///
    /// State only — no instruction. The home word already holds the value
    /// (write-through stores it at the definition, and `set_residency` refuses
    /// a dropped home to every managed value), so a `SpillKind::Store` event is
    /// a no-op for this consumer and the only thing a drop has to do is stop
    /// readers believing the register.
    ///
    /// Clearing the OWNER as well as the per-node bit is the half that is easy
    /// to miss: `gp_reg_owner` is what `mark_gp_reg_live` uses to evict the
    /// previous holder, and an owner entry left naming a node whose value is no
    /// longer there would make the next publish into that register clear a bit
    /// that is already clear and, worse, leave a stale owner for a register the
    /// plan hands to somebody else.
    pub(in crate::ir_lower) fn ls_drop_residency(&mut self, id: NodeId, gp: bool) {
        if gp {
            if let Some(reg) = self.assigned_gpr(id) {
                if self.gp_reg_owner.get(reg as usize).copied().flatten() == Some(id) {
                    self.gp_reg_owner[reg as usize] = None;
                }
            }
            if let Some(cell) = self.gp_reg_live.get_mut(id as usize) {
                *cell = false;
            }
            self.ls_set_gp_reg(id, None);
        } else {
            if let Some(cell) = self.reg_live.get_mut(id as usize) {
                *cell = false;
            }
            self.ls_set_fp_reg(id, None);
        }
    }

    /// Publish `id` into `reg` and make every reader see it there.
    ///
    /// The FP bank has no `gp_reg_owner`, so the eviction the GP bank gets from
    /// `mark_gp_reg_live` is done here by hand: any OTHER node currently
    /// pointed at `reg` has its publication bit cleared, because the
    /// instruction that just ran overwrote the value it was reading. Without
    /// that, `resident_xmm` keeps answering `reg` for a value whose bits are
    /// gone — the identical hazard `gp_reg_owner`'s field comment records, with
    /// the identical consequence (a wrong value, not a crash) and none of the
    /// machinery.
    ///
    /// The scan is over the whole node arena rather than over `ls_managed`.
    /// Restricting it to managed values would be defensible — the allocator
    /// proves no managed value is published into a register an unmanaged one
    /// holds while that one is live — but it makes the safety of this function
    /// depend on a proof in another module, and the scan runs only at a
    /// transition, of which a method has a handful.
    pub(in crate::ir_lower) fn ls_publish(&mut self, id: NodeId, reg: u8, gp: bool) {
        if gp {
            // Round 9: a `Move` vacates its source register. Leaving the OLD
            // register's owner naming `id` made the next publish into it (the
            // allocator vacated it precisely to reuse it) run
            // `mark_gp_reg_live(other)`, see `previous == id` and clear `id`'s
            // bit although `id` is correctly in `reg` — every later read of
            // `id` then fell back to its home word. Never wrong code (managed
            // homes are never dropped), but it cancelled the Move's benefit.
            if let Some(old) = self.assigned_gpr(id) {
                if old != reg && self.gp_reg_owner.get(old as usize).copied().flatten() == Some(id)
                {
                    self.gp_reg_owner[old as usize] = None;
                }
            }
            self.ls_set_gp_reg(id, Some(reg));
            self.mark_gp_reg_live(id);
            return;
        }
        for other in 0..self.reg_live.len() {
            let other_id = other as NodeId;
            if other_id == id {
                continue;
            }
            if self.assigned_xmm(other_id) == Some(reg) {
                if let Some(cell) = self.reg_live.get_mut(other) {
                    *cell = false;
                }
            }
        }
        self.ls_set_fp_reg(id, Some(reg));
        self.mark_reg_live(id);
    }

    /// Materialise every transition the plan puts at linear position `pos`,
    /// immediately before the node at that position is lowered.
    ///
    /// # Ordering, in three parts
    ///
    /// 1. **Drops first.** They emit nothing and only take a register away from
    ///    a reader, so doing them first means no instruction below can be read
    ///    out of a location the allocator says is already stale.
    /// 2. **Then the copies, in the order the plan computed.** Several moves at
    ///    one position are a PARALLEL copy — `a ← b` beside `b ← a` is a swap,
    ///    and emitting the two in sequence makes both `b`. `plan_register_
    ///    residency` has already run them through `regalloc::resolve_parallel_
    ///    copy`, the same sequencer the phi edges use, and has REFUSED the whole
    ///    split plan for the method if the answer needed a scratch register
    ///    (this position is between two nodes, where RAX may be carrying a value
    ///    and RCX a deferred one, so there is no register here that is free by
    ///    construction).
    /// 3. **A move reads its planned source or it reads memory, never a third
    ///    thing.** `resident_*` is asked whether the source register really
    ///    holds the value right now; when it does not — a definition arm that
    ///    never published, or a `Ref` copy `invalidate_ref_residency` dropped
    ///    because a collector could have run — the home word is read instead.
    ///    Falling back to memory can only REMOVE a register read, so the
    ///    sequence the plan computed stays valid. Falling back to *some other*
    ///    register would not, which is why this does not reuse the generic
    ///    operand readers.
    ///
    /// # The safepoint rule
    ///
    /// A `Load` into a register is a new cached copy of a word the oop map
    /// names, exactly like the publish at a definition site — so it lives under
    /// the same audit and needs no new argument: `lower_block`'s per-node loop
    /// invalidates after every node that is not `op_cannot_deopt`, and this runs
    /// strictly INSIDE that loop, before the node whose lowering the copy exists
    /// to serve. `plan_register_residency` refuses any managed value with a
    /// transition at a terminator or edge position precisely so that this
    /// remains true — those two positions are outside the loop that owns the
    /// invariant, and a `Ref` published there would survive the back-edge poll's
    /// own call with nothing to drop it.
    pub(in crate::ir_lower) fn ls_apply_transitions(&mut self, pos: usize) {
        let Some(at) = self.ls_at.get(pos) else {
            return;
        };
        if at.is_empty() {
            return;
        }
        // Cloned out of `self`: every arm below takes `&mut self` to emit.
        let at = at.clone();
        for t in &at {
            if let LsTransition::Drop { node, gp } = *t {
                self.ls_drop_residency(node, gp);
                self.ls_split_emitted[3] += 1;
            }
        }
        for t in &at {
            match *t {
                LsTransition::Drop { .. } => {}
                LsTransition::Load { node, reg, gp } => {
                    self.ls_emit_home_load(node, reg, gp);
                    self.ls_split_emitted[0] += 1;
                }
                LsTransition::RematConst { node, reg, value } => {
                    // No memory access at all: the point of a remat. `Op::Const`
                    // is the only shape taken here — see `ls_split_transitions`
                    // for why `ConstF` and `Param` are lowered as home loads.
                    self.emit_mov_reg_imm_smart(reg, value);
                    self.ls_publish(node, reg, true);
                    self.ls_split_emitted[1] += 1;
                }
                // j27 wave 4: recomputed from the resident base, or the
                // planned home load when that is not provable here.
                LsTransition::RematAdd { node, reg, remat } => {
                    if self.ls_emit_remat_add(node, reg, remat) {
                        self.ls_split_emitted[1] += 1;
                    } else {
                        self.ls_emit_home_load(node, reg, true);
                        self.ls_split_emitted[0] += 1;
                    }
                }
                LsTransition::Move { node, from, to, gp } => {
                    let readable = if gp {
                        self.resident_gpr(node) == Some(from)
                    } else {
                        self.resident_xmm(node) == Some(from)
                    };
                    if readable {
                        if gp {
                            self.emit_mov_reg_reg64(to, from);
                        } else {
                            self.fp_reg_move(to, from);
                        }
                        self.ls_publish(node, to, gp);
                        self.ls_split_emitted[2] += 1;
                    } else {
                        self.ls_emit_home_load(node, to, gp);
                        self.ls_split_emitted[0] += 1;
                    }
                }
            }
        }
    }

    /// `reg ← [home(node)]`, in whichever bank, then publish.
    ///
    /// `slot_of` rather than `slot_of_unwritten`: a managed value's home is
    /// never dropped (`set_residency` withholds droppability from exactly this
    /// population), so the word this reads is one the definition wrote — and if
    /// that ever stops being true, `slot_of` refusing and latching a bailout is
    /// the outcome this file wants, rather than an offset nothing wrote.
    pub(in crate::ir_lower) fn ls_emit_home_load(&mut self, node: NodeId, reg: u8, gp: bool) {
        let off = self.slot_of(node);
        if gp {
            self.load_reg_from_frame(reg, off);
            self.ls_reloads += 1;
        } else {
            let is_double = self
                .graph
                .nodes
                .get(node as usize)
                .is_some_and(|n| matches!(n.ty, IrType::Double));
            self.fp_load(reg, off, is_double);
            self.ls_reloads += 1;
        }
        self.ls_publish(node, reg, gp);
    }

    /// The `imm32` this operand denotes, if it is an integer constant that
    /// fits one.
    ///
    /// A wider `long` constant falls back to the register form, which is
    /// correct rather than merely conservative: every immediate form below
    /// SIGN-EXTENDS its `imm32` to 64 bits, so a constant outside `i32` cannot
    /// be expressed by one.
    pub(in crate::ir_lower) fn alu_imm32(&self, id: NodeId) -> Option<i32> {
        if !self.alu_imm_on {
            return None;
        }
        match self.graph.nodes.get(id as usize)?.op {
            Op::Const(val) => i32::try_from(val).ok(),
            _ => None,
        }
    }

    // Does lowering THIS NODE leave RCX exactly as it found it?
    //
    // [`op_preserves_rcx`] asked of the op, widened by the one fact an op
    // cannot express: a binary arm reaches RCX only through the REGISTER form
    // of its second operand, and an operand that folds into an immediate never
    // takes that branch. The three fold helpers (`emit_alu_acc_imm`,
    // `emit_imul_imm`, `emit_shift_imm`) all decide on `alu_imm32(id)`, so
    // asking that here is asking the arm's own question rather than a proxy
    // for it — and `every_folded_arm_reaches_rcx_only_in_its_register_form`
    // is what holds the two together.
    //
    // `alu_imm32` already answers `None` when `alu_imm_on` (`ir_alu_imm_enabled`) is off,
    // so the two flags cannot disagree: with folding off nothing here is
    // eligible and this collapses back to `op_preserves_rcx`.
    //
    // A prediction, and treated as one: `lower_data_node_tracked` compares
    // `rcx_writes` across the node and refuses the compile if the arm wrote
    // RCX anyway.
    // RR aliases and spilled LEA indexes may use the reserved RCX scratch.
    pub(in crate::ir_lower) fn selected_preserves_rcx(&self, id: NodeId) -> Option<bool> {
        if !self.reg_alloc_on {
            return None;
        }
        self.mir
            .as_ref()?
            .allocated_instruction(id)
            .map(|instruction| matches!(instruction, crate::x64::isel::MInst::AluRI { .. }))
    }

    pub(in crate::ir_lower) fn node_preserves_rcx(&self, id: NodeId) -> bool {
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return false;
        };
        if op_preserves_rcx(&node.op) {
            return true;
        }
        if !ir_carry_rcx_folded_enabled() {
            return false;
        }
        // The FP forms of these ops go through `fp_load_value` into XMM0/XMM1
        // and never reach the arm this reasons about.
        if !matches!(node.ty, IrType::Int | IrType::Long) {
            return false;
        }
        if let Some(preserves) = self.selected_preserves_rcx(id) {
            return preserves;
        }
        if !matches!(
            node.op,
            Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor | Op::Shl | Op::Shr | Op::UShr
        ) {
            return false;
        }
        node.inputs
            .get(1)
            .is_some_and(|&b| self.alu_imm32(b).is_some())
    }

    /// Would `node_preserves_rcx` admit `id` if the folded widening were on?
    ///
    /// Census only, and deliberately independent of both flags: a run with
    /// `CRATONVM_JIT_IR_CARRY_RCX_FOLDED=0` still reports how many triples the
    /// widening would have converted, which is the number that prices it
    /// WITHOUT a second run.
    pub(in crate::ir_lower) fn mid_would_fold(&self, id: NodeId) -> bool {
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return false;
        };
        if let Some(preserves) = self.selected_preserves_rcx(id) {
            return preserves;
        }
        matches!(node.ty, IrType::Int | IrType::Long)
            && matches!(
                node.op,
                Op::Add
                    | Op::Sub
                    | Op::Mul
                    | Op::And
                    | Op::Or
                    | Op::Xor
                    | Op::Shl
                    | Op::Shr
                    | Op::UShr
            )
            && node.inputs.get(1).is_some_and(|&b| {
                matches!(
                    self.graph.nodes.get(b as usize).map(|c| &c.op),
                    Some(Op::Const(v)) if i32::try_from(*v).is_ok()
                )
            })
    }

    /// `<op> EAX/RAX, imm32` in the one-byte accumulator form, if `id` is a
    /// constant. Answers whether it emitted, so the caller skips its
    /// register-to-register form.
    ///
    /// `acc` is the `AL/eAX, imm` opcode: ADD 0x05, OR 0x0D, AND 0x25,
    /// SUB 0x2D, XOR 0x35 -- the same column of the opcode map, which is why
    /// one helper covers five arms.
    pub(in crate::ir_lower) fn emit_alu_acc_imm(
        &mut self,
        id: NodeId,
        acc: u8,
        wide: bool,
    ) -> bool {
        let Some(imm) = self.alu_imm32(id) else {
            return false;
        };
        if wide {
            self.buf.emit_byte(0x48); // REX.W
        }
        if let Ok(imm8) = i8::try_from(imm) {
            // Round 9: `83 /digit ib`, three bytes (four with REX.W) instead
            // of five (six). The accumulator opcodes this is handed are
            // `(digit << 3) | 5` (ADD 05, OR 0D, AND 25, SUB 2D, XOR 35), so
            // the ModRM for `rm = RAX` is `0xC0 | (acc & 0x38)`. The imm8 is
            // sign-extended to the operand width exactly as the imm32 was, so
            // result and flags are identical. `i` loops (`i + 1`, `x & 0xF`)
            // are overwhelmingly in this range.
            self.buf.emit(&[0x83, 0xC0 | (acc & 0x38), imm8 as u8]);
        } else {
            self.buf.emit_byte(acc);
            self.buf.emit(&imm.to_le_bytes());
        }
        self.alu_imms_folded += 1;
        true
    }

    /// Round 17 ([`ir_alu_reg_operand_enabled`]): `op EAX/RAX, r` straight
    /// from the register `id` is resident in, instead of `MOV RCX, r; op EAX,
    /// ECX`. `acc` is the accumulator opcode [`Self::emit_alu_acc_imm`] takes;
    /// the `r/m, r` form of the same operation is `acc - 4` (ADD 01, OR 09,
    /// AND 21, SUB 29, XOR 31). Declines -- emitting nothing -- for a value
    /// one of the two carry slots names (its read is the carry's, through
    /// `gp_load_value`), a constant (the immediate form's), a value with no
    /// resident register, and one in RAX or RCX. RCX is not written, so no
    /// carry is disturbed; the caller skips the RCX-twin offer, which needs
    /// the operand in RCX.
    pub(in crate::ir_lower) fn emit_alu_acc_reg(
        &mut self,
        id: NodeId,
        acc: u8,
        wide: bool,
    ) -> bool {
        if !ir_alu_reg_operand_enabled() || self.mir_mode != MirMode::Off {
            return false;
        }
        if matches!(self.live_carry, Some((prod, _, _, _)) if prod == id)
            || matches!(self.deferred_rcx, Some((prod, _, _)) if prod == id)
        {
            return false;
        }
        if self
            .graph
            .nodes
            .get(id as usize)
            .is_some_and(|n| matches!(n.op, Op::Const(_)))
        {
            return false;
        }
        let Some(r) = self.resident_gpr(id) else {
            return false;
        };
        if r == RAX || r == RCX {
            return false;
        }
        let mut rex = 0x40u8;
        if wide {
            rex |= 0x08; // REX.W
        }
        if r >= 8 {
            rex |= 0x04; // REX.R
        }
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        // ModRM: mod 11, reg = r (the source), rm = RAX.
        self.buf.emit(&[acc - 4, 0xC0 | ((r & 7) << 3)]);
        self.alu_reg_operands += 1;
        true
    }

    /// JIT rx27 wave 8 (lane regalloc, [`ir_operand_in_place_enabled`]): the
    /// ALU op whose accumulator-immediate opcode is `acc` (`0x05` ADD, `0x2D`
    /// SUB, `0x25` AND, `0x0D` OR, `0x35` XOR), with its second operand `id`
    /// read straight from its home word: `op eax/rax, [rbp - off]` (`acc - 2`,
    /// the `r, r/m` form). Asked after the immediate and register forms
    /// declined, in place of `MOV RCX, [rbp - off]; op eax, ecx`. Declines (and
    /// emits nothing) for a value a carry holds (it is in RAX/RCX, not its
    /// home), a constant (the immediate form's), a value with no readable home
    /// (`slot_of_checked`), or with the switch off. Only the read moves: the
    /// home word is the one `gp_load_value` would have read.
    pub(in crate::ir_lower) fn emit_alu_acc_frame(
        &mut self,
        id: NodeId,
        acc: u8,
        wide: bool,
    ) -> bool {
        if !self.operand_in_place_on || self.carry_names(id) {
            return false;
        }
        if self
            .graph
            .nodes
            .get(id as usize)
            .is_some_and(|n| matches!(n.op, Op::Const(_)))
        {
            return false;
        }
        let Ok(off) = self.slot_of_checked(id) else {
            return false;
        };
        self.token_read_check(id);
        if wide {
            self.buf.emit_byte(0x48); // REX.W
        }
        self.buf.emit_byte(acc - 2);
        self.emit_rbp_modrm_disp(RAX, off);
        self.operand_in_place_alus += 1;
        true
    }

    /// JIT rx27 wave 8: with the first operand of a compare already in RAX,
    /// set the flags for `RAX <cc> b` reading `b` where it lives: an
    /// immediate (`CMP EAX, imm`), its register (`CMP EAX, r`) or its home word
    /// (`CMP EAX, [rbp - off]`). `false` (nothing emitted) when the switch is
    /// off or `b` is a value a carry holds; the caller then loads RCX as
    /// before.
    pub(in crate::ir_lower) fn emit_cmp_rax_with(&mut self, b: NodeId, wide: bool) -> bool {
        if !self.operand_in_place_on || self.carry_names(b) {
            return false;
        }
        if let Some(imm) = self.alu_imm32(b) {
            self.emit_cmp_reg_imm(RAX, imm, wide);
        } else if let Some(rb) = self.resident_gpr(b).filter(|&r| r != RAX && r != RCX) {
            self.emit_cmp_reg_reg(RAX, rb, wide);
        } else if let Ok(off) = self.slot_of_checked(b) {
            self.token_read_check(b);
            self.emit_cmp_reg_frame(RAX, off, wide);
        } else {
            return false;
        }
        self.operand_in_place_cmps += 1;
        true
    }

    /// `IMUL EAX/RAX, EAX/RAX, imm32` (0x69 /r id), if `id` is a constant.
    pub(in crate::ir_lower) fn emit_imul_imm(&mut self, id: NodeId, wide: bool) -> bool {
        let Some(imm) = self.alu_imm32(id) else {
            return false;
        };
        if wide {
            self.buf.emit_byte(0x48);
        }
        if let Ok(imm8) = i8::try_from(imm) {
            // Round 9: `6B /r ib` — IMUL with a sign-extended imm8, three
            // bytes instead of six; same product, same flags.
            self.buf.emit(&[0x6B, 0xC0, imm8 as u8]); // dst=rax, src=rax
        } else {
            self.buf.emit(&[0x69, 0xC0]); // dst=rax, src=rax
            self.buf.emit(&imm.to_le_bytes());
        }
        self.alu_imms_folded += 1;
        true
    }

    /// `IMUL EAX/RAX, r, imm` (`6B /r ib` / `69 /r id`) -- the THREE-operand
    /// form, reading the first operand straight from its resident register --
    /// when `b` folds and `a` is resident. Answers whether it emitted; when it
    /// did, RAX holds `a * b` exactly as `gp_load_value(RAX, a)` followed by
    /// [`Self::emit_imul_imm`] would have left it, one `MOV` shorter.
    ///
    /// Round 9 wave 10 (irl10),
    /// `perf-long-counted-loops-lose-the-optimizing-osr-body`: `s += i * 3`
    /// spent `mov rax,r15; imul rax,3` per iteration on a counter that was
    /// already in R15.
    ///
    /// Declines, EMITTING NOTHING, unless all of:
    ///
    /// * `b` folds on exactly the predicate the other fold helpers use
    ///   (`alu_imm32`), so `node_preserves_rcx`' prediction for the arm is
    ///   unchanged: when this fires the arm never reaches RCX, and when `b`
    ///   does not fold this declines and the arm's register form runs as before;
    /// * `a` is resident in the GP file and is not a value a carry is holding
    ///   (a carried value must be read through `gp_load_value`, which is what
    ///   retires the carry; a resident value is never carried, so this is the
    ///   belt to `plan_carries`' braces, as in `emit_add_lea`).
    ///
    /// Writes RAX only; reads `a`'s register. The GP file names neither RAX nor
    /// RCX (asserted in `every_folded_arm_reaches_rcx_only_in_its_register_form`).
    pub(in crate::ir_lower) fn emit_imul_reg_imm3(
        &mut self,
        a: NodeId,
        b: NodeId,
        wide: bool,
    ) -> bool {
        let Some(imm) = self.alu_imm32(b) else {
            return false;
        };
        if self.carry_names(a) || self.carry_names(b) {
            return false;
        }
        let Some(src) = self.resident_gpr(a) else {
            return false;
        };
        let (bytes, len) = imul_rax_reg_imm_bytes(src, imm, wide);
        self.buf.emit(&bytes[..len]);
        self.alu_imms_folded += 1;
        true
    }

    /// JIT round 23 wave 11 (lane L7, [`ir_imul_const_first_enabled`]):
    /// `k * x` with the CONSTANT first -- javac's order for `31 * h`, every
    /// `hashCode` loop -- as `IMUL EAX/RAX, r, imm` from `x`'s resident
    /// register, or `IMUL EAX/RAX, EAX/RAX, imm` after loading `x` into RAX,
    /// instead of `MOV EAX, k; MOV RCX, x; IMUL EAX, ECX`. Multiplication
    /// commutes exactly in two's complement (both widths wrap the same way),
    /// so RAX ends holding the same bits.
    ///
    /// Declines, EMITTING NOTHING, unless `a` folds (`alu_imm32`) and `b` does
    /// not (the second-operand forms keep that case), and `b` is no carry's
    /// value: a carried `b` was planned for this arm's `gp_load_value(RCX, ..)`
    /// and must be read there. RCX is never written; `node_preserves_rcx`
    /// answers `false` for this shape (its second operand does not fold), so
    /// no deferred carry is crossing.
    pub(in crate::ir_lower) fn emit_imul_const_first(
        &mut self,
        a: NodeId,
        b: NodeId,
        wide: bool,
    ) -> bool {
        if !ir_imul_const_first_enabled() || self.alu_imm32(b).is_some() {
            return false;
        }
        let Some(imm) = self.alu_imm32(a) else {
            return false;
        };
        if self.carry_names(a) || self.carry_names(b) {
            return false;
        }
        if let Some(src) = self.resident_gpr(b) {
            let (bytes, len) = imul_rax_reg_imm_bytes(src, imm, wide);
            self.buf.emit(&bytes[..len]);
        } else {
            self.gp_load_value(RAX, b);
            let (bytes, len) = imul_rax_reg_imm_bytes(RAX, imm, wide);
            self.buf.emit(&bytes[..len]);
        }
        self.alu_imms_folded += 1;
        true
    }

    /// `<shift> EAX/RAX, imm8` (0xC1 /digit ib), if `id` is a constant.
    ///
    /// The count is masked here to the width the JVM specifies -- 5 bits for
    /// `ishl`/`ishr`/`iushr`, 6 for the `l` forms. x86 masks a shift count the
    /// same way, which is what makes the existing `CL` form correct without a
    /// mask; doing it explicitly keeps the immediate form from depending on
    /// that coincidence.
    ///
    /// `modrm` is the WHOLE ModRM byte (`0xE0` SHL, `0xF8` SAR, `0xE8` SHR —
    /// `0xC0 | digit << 3`, rm = RAX), not the `/digit`: a bare digit here
    /// would encode a memory operand. (Named `digit` before round 9.)
    pub(in crate::ir_lower) fn emit_shift_imm(
        &mut self,
        id: NodeId,
        modrm: u8,
        wide: bool,
    ) -> bool {
        let Some(imm) = self.alu_imm32(id) else {
            return false;
        };
        let count = (imm as u32) & if wide { 63 } else { 31 };
        if wide {
            self.buf.emit_byte(0x48);
        }
        self.buf.emit(&[0xC1, modrm, count as u8]);
        self.alu_imms_folded += 1;
        true
    }

    /// Load `id` into `dst` for a value that LEAVES this tier through the
    /// compiled-entry ABI (a call argument or a return value): an `int`
    /// sign-extended to 64 bits.
    ///
    /// The backend ABI is "all JVM values are passed as i64, ints sign-extended"
    /// (`x64.rs`), and the single-pass tier's callees and callers RELY on it: its
    /// `i2l` is a no-op, so a callee that widens a parameter, or an inlined body
    /// that does, reads all 64 bits. This tier's own `int` convention is the
    /// opposite -- a consumer reads only the LOW 32 bits, and a 32-bit ALU
    /// result (`neg eax`, `add eax, ecx`, `mov eax, eax` for `l2i`) leaves the
    /// high half ZERO -- so a negative `int` computed here and handed on
    /// unextended reached a single-pass `flog10pow2` as 4294967244 where it
    /// meant -52, and `Double.toString` indexed `MathUtils.g` with garbage
    /// (`ArrayIndexOutOfBoundsException: Index 2137324 out of bounds for length
    /// 1234`, `Sbr3UnsafecasOsr`). A constant is already sign-extended by its
    /// own materialisation and is left alone.
    pub(in crate::ir_lower) fn gp_load_abi_value(&mut self, dst: u8, id: NodeId) {
        self.gp_load_value(dst, id);
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return;
        };
        if node.ty == IrType::Int && !matches!(node.op, Op::Const(_)) {
            // MOVSXD dst, dst32
            let hi = u8::from(dst >= 8);
            self.buf.emit(&[
                0x48 | (hi << 2) | hi,
                0x63,
                0xC0 | ((dst & 7) << 3) | (dst & 7),
            ]);
        }
    }

    /// Load `id`'s value into `dst`, from its resident register when it has one
    /// and from its home word otherwise.
    ///
    /// The read half of the GP cache. A site that still calls
    /// `load_to_rax(self.slot_of(id))` directly is simply not accelerated,
    /// which is a missed optimization and never wrong code: the home word is
    /// written unconditionally.
    #[track_caller]
    pub(in crate::ir_lower) fn gp_load_value(&mut self, dst: u8, id: NodeId) {
        // Wave 64 (lane L2): the token-read guard, checked before the carry
        // paths below, which return without reaching `resident_gpr`. A read
        // that falls through to the register or home path is checked again
        // there; the hit count is "hits", not distinct reads.
        self.token_read_check(id);
        // ── JIT round 25 item 1a: a TAIL carry, whose home was never written ──
        //
        // Read only by its planned consumer (the data node or the terminator
        // being lowered), only into RAX, and only with nothing emitted since
        // the store that was not: `buf.pos()` is the proof that RAX is still
        // the value on the fall-through path, and `verify_tail_carry_points`
        // proves after the body that no branch enters here on another path.
        // Anything else refuses the attempt and falls through to the home read,
        // which `slot_of` refuses too.
        if let Some((prod, cons, pos)) = self.tail_carry {
            if prod == id {
                if dst == RAX
                    && pos == self.buf.pos()
                    && (self.cur_def == Some(cons) || self.cur_term == Some(cons))
                {
                    self.tail_carry = None;
                    self.tail_carry_points.push((pos, prod));
                    return;
                }
                self.refuse_tail_carry("read other than by its consumer, in RAX, right after it");
            }
        }
        // ── A carried value is already in `dst`, and only in `dst` ────
        //
        // Every clause below is checked rather than argued, because a carried
        // value exists in exactly one place and a wrong answer here would read
        // a frame word nothing wrote:
        //
        // * this really is the consumer the carry was planned for;
        // * it is asking for the register the plan chose;
        // * and for an RAX carry, NOTHING has been emitted since the producer
        //   left the value there — `buf.pos()` proves it, and no audit of what
        //   the arms emit can be wrong about it. An RCX carry cannot use that
        //   proof (the consumer's first read is emitted in between) and rests
        //   on `op_reads_rax_then_rcx` instead: that read writes RAX only.
        if let Some((prod, cons, reg, pos)) = self.live_carry {
            if prod == id {
                if self.cur_def != Some(cons) {
                    self.refuse_carry("read outside its planned consumer");
                } else if dst != reg {
                    self.refuse_carry("read into a register the plan did not choose");
                } else if reg == RAX && pos != self.buf.pos() {
                    self.refuse_carry("something was emitted between the two");
                } else {
                    self.live_carry = None;
                    self.carries_read += 1;
                    // The value is in `dst`. That is the whole optimization:
                    // no store, no load, and for an RAX carry no instruction
                    // at all.
                    return;
                }
            }
        }
        // The DEFERRED carry, read the same way and on the same three checks
        // as the adjacent one, minus the `buf.pos()` proof — which is exactly
        // what this slot cannot offer, since a whole arm was emitted in
        // between. What stands in for it is `op_preserves_rcx`, checked when
        // that arm was lowered (`lower_data_node_tracked`), plus the allowance
        // running out if more than one arm ever gets between the two.
        if let Some((prod, cons, _)) = self.deferred_rcx {
            if prod == id {
                if self.cur_def != Some(cons) {
                    self.refuse_deferred("read outside its planned consumer");
                } else if dst != RCX {
                    self.refuse_deferred("read into a register other than RCX");
                } else {
                    self.deferred_rcx = None;
                    self.carry_deferred_read += 1;
                    self.carries_read += 1;
                    return;
                }
            }
        }
        // Past both carry slots, so this read is not the one either was
        // planned for. If it names RCX it is about to overwrite whatever a
        // deferred carry left there.
        if dst == RCX {
            self.note_rcx_written();
        }
        // 2026-09-02: a constant is an IMMEDIATE, not a frame word. The
        // `Op::Const` arm still writes its home (a deopt frame may name it,
        // and some sites still read slots directly), but no reader of a
        // constant has to wait on that store any more: `fib` materialised
        // `1` and `2` into frame words and reloaded them on every call.
        if self.const_imm_on {
            if let Some(node) = self.graph.nodes.get(id as usize) {
                if let Op::Const(val) = &node.op {
                    let val = *val;
                    self.emit_mov_reg_imm_smart(dst, val);
                    return;
                }
            }
        }
        match self.resident_gpr(id) {
            // JIT rx27 wave 9 (`CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE`): the
            // value's own publish is the last thing emitted, so RAX already
            // is it on the fall-through path; no branch lands here
            // (`verify_publish_reload_elides`).
            Some(src)
                if dst == RAX
                    && self.publish_reload_elide_on
                    && self.rax_reg_mirror == Some((id, src, self.buf.pos())) =>
            {
                self.gp_reg_reads += 1;
                self.publish_reload_elides.push((self.buf.pos(), id));
                if let Some(cell) = self.publish_reload_elided.get_mut(id as usize) {
                    *cell = true;
                }
            }
            Some(src) => {
                self.gp_reg_reads += 1;
                self.emit_mov_reg_reg64(dst, src);
            }
            None => {
                let off = self.slot_of(id);
                // The write-through home remains GC-visible. With no emitted
                // instruction since its RAX store, the immediate read can use
                // RAX too; the finished-body branch audit proves that no other
                // path enters after the store. A refused value keeps its load
                // on the ordinary per-value retry.
                // Any register, not only RAX: the store left `dst` holding the word.
                let in_rax = self.publish_reload_elide_on
                    && !self
                        .publish_reload_excluded
                        .get(id as usize)
                        .copied()
                        .unwrap_or(true)
                    && self.rax_home_mirror.is_some_and(|(src, home, _, end)| {
                        src == dst && home == off && end == self.buf.pos()
                    });
                if in_rax {
                    self.gp_reg_reads += 1;
                    self.publish_reload_elides.push((self.buf.pos(), id));
                    if let Some(cell) = self.publish_reload_elided.get_mut(id as usize) {
                        *cell = true;
                    }
                } else {
                    self.note_gp_home_reload(id);
                    self.load_reg_from_frame(dst, off);
                }
            }
        }
    }

    /// Load an array INDEX into RCX already zero-extended, so the bounds
    /// guard's `MOV ECX, ECX` has nothing left to do (backlog MX-B2 of
    /// `r16-matrix-2d-inner-loop-rereads-rows`).
    ///
    /// Every 32-bit write of a GPR zero-extends through the full register in
    /// hardware, so `MOV ECX, r32`, `MOV ECX, [rbp - off]` and `MOV ECX,
    /// imm32` each leave RCX with a zero high half and the low half the index
    /// node defines -- exactly what `MOV ECX, ECX` would have made of the
    /// 64-bit copy. A carried value is read through [`Self::gp_load_value`]
    /// (it is already in RCX, with whatever high half its producer left) and
    /// is not marked. The mark is a code position, consumed by
    /// [`Self::emit_array_null_bounds_guards_for`] only when NOTHING has been
    /// emitted since: the same `buf.pos()` proof the RAX carry uses.
    pub(in crate::ir_lower) fn gp_load_index_rcx(&mut self, id: NodeId) {
        if let Some((x, mask)) = self.fused_index_and.get(id as usize).copied().flatten() {
            self.gp_load_index_rcx(x);
            if let Ok(imm8) = i8::try_from(mask as i32) {
                self.buf.emit(&[0x83, 0xE1, imm8 as u8]); // AND ECX, imm8
            } else {
                self.buf.emit(&[0x81, 0xE1]); // AND ECX, imm32
                self.buf.emit(&mask.to_le_bytes());
            }
            self.index_zx_at = Some(self.buf.pos());
            return;
        }
        let carried = matches!(self.live_carry, Some((prod, ..)) if prod == id)
            || matches!(self.deferred_rcx, Some((prod, ..)) if prod == id);
        if carried {
            self.gp_load_value(RCX, id);
            return;
        }
        self.note_rcx_written();
        if self.const_imm_on {
            if let Some(Op::Const(val)) = self.graph.nodes.get(id as usize).map(|n| &n.op) {
                // MOV ECX, imm32 (the low half; the high half becomes zero).
                let v = *val as u32;
                self.buf.emit_byte(0xB9);
                self.buf.emit(&v.to_le_bytes());
                self.index_zx_at = Some(self.buf.pos());
                return;
            }
        }
        match self.resident_gpr(id) {
            Some(src) => {
                // MOV ECX, r32 (`89 /r`, REX.R for an extended source).
                if src >= 8 {
                    self.buf.emit_byte(0x44);
                }
                self.buf.emit(&[0x89, 0xC0 | ((src & 7) << 3) | (RCX & 7)]);
            }
            None => {
                // MOV ECX, dword [rbp - off]: the home word's low half.
                let off = self.slot_of(id);
                self.buf.emit_byte(0x8B);
                self.emit_rbp_modrm_disp(RCX, off);
            }
        }
        self.index_zx_at = Some(self.buf.pos());
    }

    /// An array address consumes the full register. Only an observed
    /// zero-extending write can let the selected index bypass staging.
    pub(in crate::ir_lower) fn load_array_index(&mut self, id: NodeId, allow_resident: bool) -> u8 {
        if allow_resident {
            if let Some(reg) = self.resident_array_index(id) {
                self.gp_reg_reads += 1;
                return reg;
            }
        }
        self.gp_load_index_rcx(id);
        RCX
    }

    pub(in crate::ir_lower) fn resident_array_index(&self, id: NodeId) -> Option<u8> {
        if !self.reg_alloc_on
            || self.carry_names(id)
            || self
                .tail_carry
                .is_some_and(|(producer, _, _)| producer == id)
        {
            return None;
        }
        self.resident_gpr(id).filter(|&reg| {
            !matches!(reg, RAX | RCX | RDX | RSP)
                && self.gp_reg_owner.get(reg as usize).copied().flatten() == Some(id)
                && self.gp_zx32_mask & (1u16 << reg) != 0
        })
    }

    pub(in crate::ir_lower) fn normalize_rcx_index(&mut self) {
        if self.index_zx_at.take() != Some(self.buf.pos()) {
            self.buf.emit(&[0x89, 0xC9]); // MOV ECX, ECX
        }
    }
}
