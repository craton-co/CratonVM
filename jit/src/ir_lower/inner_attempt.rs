#[allow(unused_imports)]
use super::*;


pub(in crate::ir_lower) fn lower_inner_attempt(
    direct_helpers: &crate::DirectHelperTable,
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    spliced_ranges: &[(usize, usize, usize)],
    sr_map: Option<&ScalarReplacementMap>,
    // IR direct-call lowering: `pc → (callee_entry, callee_needs_context)` for
    // every statically-bound call site whose callee was eagerly compiled. Empty
    // ⇒ every `Op::Call` keeps the historical `jit_invoke_dispatch` lowering.
    direct_calls: &HashMap<usize, (usize, bool)>,
    // IR inline-cache lowering: `pc → (mic_slot_addr, pic_slot_addr)` for every
    // virtual / interface site served from an inline cache. Empty ⇒ every such
    // site keeps the historical helper dispatch.
    ic_slots: &HashMap<usize, (usize, usize)>,
    // Guarded inline field reads: `(pc, is_reference) → (packed body offset,
    // is_reference, descriptor tag)` for every resolved compact instance field,
    // plus the two rows the IR String-access expansion needs at its invoke pc.
    // Empty ⇒ every `Op::Load` takes the checked helper, as it always did.
    compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
    // Combined-buffer pc → enclosing spliced callees. Empty ⇒ this compile
    // spliced nothing and the artifact's inline frame map stays empty, exactly
    // as it was before 2026-09-08.
    inline_frame_sites: &crate::ir::IrInlineFrameSites,
    // Round 9 wave 8 (irl8): `pc -> JitCycleEdgeCell` for the closing edges
    // of a compile-time recursion cycle. Empty => every such site dispatches.
    cycle_cells: &HashMap<usize, usize>,
    // Round 9 wave 8b (irl8 request 3): see `Lowerer::tlab_announce_classes`.
    tlab_announce_classes: &std::collections::HashSet<u32>,
    // Round 9 wave 9 (irl9): see `Lowerer::array_list_layout`.
    array_list: Option<crate::ArrayListFieldLayout>,
    // Round 11 wave 10 (lane irexc): see `Lowerer::protected_ranges`.
    protected_ranges: &[(u32, u32)],
    // Round 11 wave 11 (lane irexc): see `Lowerer::osr_handler_reachable`.
    osr_handler_reachable: Option<&[u32]>,
    // Interpreter round i1 wave 15 (lane L2): see `Lowerer::poll_mode_exits`.
    poll_mode_exits: IrPollModeExits,
    // Wave 18 (lane L2): see `Lowerer::poll_exit_bytecode`.
    poll_exit_bytecode: Option<PollExitBytecode<'_>>,
    // gen r5w2/oomjit6: see `Lowerer::dead_ref_clear`.
    dead_ref_clear_code: Option<DeadRefClearBytecode<'_>>,
    // Taken once by the caller (see there) and read at the deopt-point check
    // near the end.
    unroll_used_unreachable_frames: bool,
    // Round nio20 wave 5 (lane L3): taken once by the caller, like the flag
    // above; see the pure-body unroll's net near the end.
    unroll_stranded_bcis: &crate::ir_optimize::StrandedFrames,
    capacity_override: Option<usize>,
    overflow_wanted: Option<&mut Option<usize>>,
    // Round 16 wave 13 (lane polyclear): `true` keeps the deopt register image
    // whatever `CRATONVM_JIT_IR_DEOPT_IMAGE_EXITLESS` says (the fallback
    // attempt), and `image_dropped` reports whether this attempt dropped it.
    keep_deopt_image: bool,
    image_dropped: &mut bool,
) -> Option<CompiledMethod> {
    // A monitor whose helper is absent must REFUSE the compile.
    //
    // History, because the shape of it is the reason `lower_data_node`'s final
    // arm now refuses too. `ir::Op::MonitorEnter`/`MonitorExit` were added for
    // escape analysis (lock elision) BEFORE either a lowering arm or a
    // `JitRuntimeHelpers` monitor entry existed. `lower_data_node`'s catch-all
    // was `_ => {}` at the time, so an unguarded monitor compiled to *nothing*
    // — the lock silently gone, an unbalanced `monitorexit` left behind, a data
    // race rather than a missed optimization. This guard was written to keep
    // that unreachable.
    //
    // Both halves have since landed: `lower_data_node` has a real
    // `Op::MonitorEnter | Op::MonitorExit` arm that calls the helper, and
    // `a_synchronized_region_lowers_through_the_monitor_helper` drives it. So
    // what remains here is narrower than the comment this replaces claimed: a
    // graph with monitor ops and a helper table that has no monitor entry (in
    // practice a synthetic unit-test table). Refuse rather than call through a
    // zero pointer.
    //
    // The general form of the original hazard — an `ir::Op` variant with no arm
    // at all — is now handled where it arises, by that final arm, instead of
    // needing a new hand-written guard here per op. See
    // `docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
    // "The cheap alternative".
    if (helpers.monitor_enter == 0 || helpers.monitor_exit == 0)
        && graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::MonitorEnter | Op::MonitorExit))
    {
        return refuse(Bailout::with_context(
            BailoutReason::UnsupportedShape("monitor helper absent"),
            "graph contains monitor ops but the helper table has no monitor \
             entry; refusing rather than emitting nothing and dropping the lock",
        ));
    }
    // A live object allocation is now supported by the common allocation
    // stub. A zero helper pointer is only possible in synthetic unit-test
    // tables; reject it instead of emitting a call through address zero.
    if helpers.new_object == 0
        && graph
            .nodes
            .iter()
            .any(|node| matches!(node.op, Op::New { .. }))
    {
        return refuse_labelled("new-without-alloc-helper");
    }
    // cov-06. Same reasoning, split per `Op::NewArray` shape: a PRIMITIVE
    // array (`element_type != 0`) needs `helpers.newarray` wired; a
    // REFERENCE array (`element_type == 0`) needs `helpers.anewarray_object`.
    // Only a synthetic unit-test table leaves either at 0.
    if graph
        .nodes
        .iter()
        .any(|node| matches!(node.op, Op::NewArray { element_type, .. } if element_type != 0))
        && helpers.newarray == 0
    {
        return refuse_labelled("newarray-primitive-without-helper");
    }
    if graph
        .nodes
        .iter()
        .any(|node| matches!(node.op, Op::NewArray { element_type, .. } if element_type == 0))
        && helpers.anewarray_object == 0
    {
        return refuse_labelled("anewarray-without-helper");
    }
    // nio21 wave 15 (lane L3): a deferred `new` calls `new_object_cp`, and a
    // zero there is a CALL to address 0. The arm latches the same refusal; this
    // one names it before any code is emitted.
    if helpers.new_object_cp == 0 && graph.nodes.iter().any(|n| matches!(n.op, Op::NewCp { .. })) {
        return refuse_labelled("new-cp-without-helper");
    }
    // cov-01. The same reasoning as the two guards above, for the three
    // constant-pool nodes: each lowers to a `CALL` through a helper address,
    // and a zero there is a call to address 0. Only a synthetic unit-test table
    // can produce one — `build_helpers` always wires all three — but "only a
    // test can hit it" is what the monitor guard's history says not to rely on.
    //
    // `Op::LoadStatic` is guarded on the helper even though its usual route is
    // the direct load: which route a site takes is decided per site inside
    // `emit_inline_getstatic`, and any site the resolver declines falls back to
    // this helper. Refusing the graph is the only answer that does not depend
    // on a runtime resolver's answer at emission time.
    for (helper, present, what) in [
        (
            helpers.ldc_string_cp,
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::ConstString { .. })),
            "ldc_string_cp",
        ),
        (
            helpers.ldc_class_cp,
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::ConstClass { .. })),
            "ldc_class_cp",
        ),
        (
            helpers.getstatic,
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::LoadStatic { .. })),
            "getstatic",
        ),
        // A WIDE static's helper route peeks `jit_dispatch_threw` to tell a
        // legitimate `Long.MIN_VALUE` from the exception sentinel, so that
        // helper is load-bearing for exactly the `J`/`D`/`F` tags and for
        // nothing else. `Op::Call` reaches the same peek and does not guard it
        // — its wide-return sites are gated elsewhere — but a wide static
        // arrives here through a table, so state the requirement rather than
        // inherit an assumption.
        (
            helpers.dispatch_threw,
            graph.nodes.iter().any(|n| {
                matches!(n.op, Op::LoadStatic { type_tag, .. } if matches!(type_tag, b'J' | b'D' | b'F'))
            }),
            "dispatch_threw (needed by a J/D/F getstatic)",
        ),
        // `Op::StoreStatic` has no route but its helper, one per width.
        (
            helpers.putstatic_int,
            graph.nodes.iter().any(|n| {
                matches!(n.op, Op::StoreStatic { type_tag, .. }
                    if !matches!(type_tag, b'J' | b'F' | b'D' | b'L' | b'['))
            }),
            "putstatic_int",
        ),
        (
            helpers.putstatic_long,
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::StoreStatic { type_tag: b'J', .. })),
            "putstatic_long",
        ),
        (
            helpers.putstatic_float,
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::StoreStatic { type_tag: b'F', .. })),
            "putstatic_float",
        ),
        (
            helpers.putstatic_double,
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::StoreStatic { type_tag: b'D', .. })),
            "putstatic_double",
        ),
        (
            helpers.putstatic_object,
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::StoreStatic { type_tag: b'L' | b'[', .. })),
            "putstatic_object",
        ),
    ] {
        if helper == 0 && present {
            return refuse(Bailout::with_context(
                BailoutReason::UnsupportedShape("constant-pool helper absent"),
                format!(
                    "graph contains a cov-01 constant-pool node but the helper table has no \
                     `{what}` entry; refusing rather than emitting a CALL through address zero"
                ),
            ));
        }
    }
    // Compact field layout (default ON) packs field offsets, so `Op::Load` and
    // `Op::Store`'s inline `HEADER_SIZE + field_index*SLOT_SIZE` displacements
    // are wrong for a compact object. Both have a compact-correct alternative —
    // the `jit_getfield` / `jit_putfield_int` helpers — so the only unlowerable
    // combination is "compact layout on AND the helper address absent", which
    // in practice means a synthetic unit-test helper table.
    //
    // This check used to live in `IrBuilder::build` as an unconditional bail on
    // the getfield/putfield opcodes themselves, which refused every
    // field-accessing method — most of real Java — from the optimizing tier
    // regardless of which lowering would have been chosen. Keeping it here
    // states the actual constraint: it is a property of one lowering, not of
    // the opcode.
    if cratonvm_types::compact_ref_fields_enabled() {
        let needs_getfield_helper =
            helpers.getfield == 0 && graph.nodes.iter().any(|n| matches!(n.op, Op::Load(_)));
        // Only an INT store has an inline lowering to fall back to, so only an
        // int store is what this compact-layout clause is about. Every other
        // `MemKind` is helper-only in both layouts and is refused
        // unconditionally below — checking `putfield_int` for them would refuse
        // a reference store for the absence of a helper it never calls.
        let needs_putfield_helper = helpers.putfield_int == 0
            && graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::Store(MemKind::Int)));
        if needs_getfield_helper || needs_putfield_helper {
            return refuse_labelled("getfield-or-putfield-helper-required");
        }
    }
    // A REFERENCE or WIDE field read has only one correct lowering: the helper.
    // The inline displacement fallback decodes a 16-byte int cell at
    // `HEADER_SIZE + index*SLOT_SIZE`, which for a reference slot yields the
    // discriminant word rather than the pointer — a fabricated address the
    // frame would then publish as a root — and for a `J`/`F`/`D` slot yields a
    // sign-extended half of the payload. Refuse the graph outright rather than
    // emit it, independently of the compact-layout switch above.
    //
    // A wide read additionally needs `jit_dispatch_threw`: `Long.MIN_VALUE`,
    // and the `-0.0` bit pattern, are bit-identical to the helper's deopt/NPE
    // sentinel, and without the out-of-band peek the lowering must either drop
    // a real NPE or bail on a legitimate value. Neither is acceptable, so
    // refuse instead. `Float` is included for the same reason the `J`/`D`/`F`
    // branch of `emit_call_return_check` includes it: the disambiguation is one
    // shape, keyed on the value's WIDTH CLASS rather than on a claim about what
    // `jit_getfield` happens to zero-extend today.
    if graph.nodes.iter().any(|n| {
        matches!(
            n.op,
            Op::Load(MemKind::Ref | MemKind::Long | MemKind::Float | MemKind::Double)
        )
    }) && helpers.getfield == 0
    {
        return refuse_labelled("wide-load-without-getfield-helper");
    }
    if helpers.dispatch_threw == 0
        && graph.nodes.iter().any(|n| {
            matches!(
                n.op,
                Op::Load(MemKind::Long | MemKind::Float | MemKind::Double)
            )
        })
    {
        return refuse_labelled("wide-load-shape-unsupported");
    }
    // COV-03 — every non-int field STORE is helper-only, and each width has its
    // own helper. A reference store's helper is the one that carries the write
    // barrier; emitting the store without it is the failure this lane is most
    // careful about (invisible until a concurrent or generational collection,
    // and it surfaces as a lost object, not as a fault at the store). Refuse
    // rather than substitute.
    for n in &graph.nodes {
        let missing = match n.op {
            Op::Store(MemKind::Ref) => helpers.putfield_object == 0,
            Op::Store(MemKind::Long) => helpers.putfield_long == 0,
            Op::Store(MemKind::Float) => helpers.putfield_float == 0,
            Op::Store(MemKind::Double) => helpers.putfield_double == 0,
            _ => false,
        };
        if missing {
            return refuse_labelled("putfield-helper-missing-for-width");
        }
    }
    // Every field access carries its slot index as a `Const` offset input, and
    // both lowerings read that index out of the node the edge points at. If it
    // is not a `Const`, neither has an index to use — and both used to fall
    // back to a silent `0`, i.e. to accessing SOME OTHER FIELD of the receiver.
    //
    // A silent `0` is the worst available answer for exactly the defect family
    // this lane keeps meeting: a store lands in a slot the class declares a
    // reference, the cell then holds a primitive under a reference's name, and
    // the fault appears much later in whatever dereferences it — a compiled
    // `arraylength` on `Int(1)`, faulting at `addr=0x5`
    // (`punned-sqlchar-rawdata-was-a-direct-call-pinned-by-address-FIXED-20260828.md`).
    // Nothing in the report points back here, because a wrong-slot write leaves
    // no trace of having chosen the wrong slot.
    //
    // The builder only ever emits `iconst(field_index)` into this edge, so this
    // is unreachable today and refusing costs nothing measurable. That is the
    // point: it closes the hazard while it is still unreachable, rather than
    // after some later optimisation makes the offset node non-constant and the
    // fallback starts silently corrupting objects.
    if graph.nodes.iter().any(|n| {
        matches!(n.op, Op::Load(_) | Op::Store(_))
            && n.inputs
                .get(3)
                .and_then(|&o| graph.nodes.get(o as usize))
                .map_or(true, |o| !matches!(o.op, Op::Const(_)))
    }) {
        return refuse_labelled("non-const-store-operand");
    }

    // ── Resource bounds, decided before anything is reserved ─────────
    //
    // Both checks precede `ExecutableBuffer::new` and `Lowerer::new`, so an
    // over-budget method costs neither an executable mapping nor an enormous
    // frame. `estimate_frame_bytes` is the same function `Lowerer::new` sizes
    // the frame from, so passing here guarantees the constructor's `i32`
    // arithmetic stays in range.
    if let Err(bailout) = check_graph_size(graph.nodes.len(), DEFAULT_MAX_NODES) {
        return refuse(bailout);
    }
    let mut frame_needs = scan_frame_needs(graph, helpers);
    frame_needs.vector_xmm_saves = vector_xmm_saves_wanted(graph, schedule);
    // Liveness-based frame-slot reuse: values whose live ranges do not overlap
    // share one 8-byte word, so the spill reservation is sized by the peak
    // number of simultaneously live values instead of by the node count. The
    // colouring is *checked* before it is used — an aliased pair of live values
    // is silent wrong code, so it is not taken on trust.
    //
    // Interpreter round i2 wave 66 (lane L2): under
    // `CRATONVM_JIT_IR_ONE_LIVENESS_PASS` the liveness answer behind this plan
    // is also the one the allocator's model is built from below (residency,
    // OSR entry stubs), instead of a second run of the same fixed point.
    // JIT round 19 wave 10 (lane lv10, `CRATONVM_JIT_IR_LIVENESS_SHARED_SCRATCH`,
    // opt-in): one liveness scratch for the legacy copies this compile runs
    // (the slot plan here, the dead-home value ranges and the live model below).
    let mut liveness_scratch = crate::regalloc::liveness_shared_scratch_enabled()
        .then(crate::regalloc::LivenessScratch::new);
    let protected = if linear_scan_enabled() && r25_w7a_reg_alloc::effective(keep_deopt_image) {
        hidden_frame_values(graph, schedule, sr_map)
    } else { Vec::new() };
    let check_elision = crate::ir_check_elim::analyze(graph, schedule);
    let checked_arrays = (protected_ranges.is_empty()
        && !graph.nodes.iter().any(|node| matches!(node.op, Op::OsrLocal(_) | Op::OsrEntryFlag { .. })))
        .then_some(&check_elision);
    let allocated_mir = if linear_scan_enabled() && r25_w7a_reg_alloc::effective(keep_deopt_image) {
        match build_allocated_mir_plan(graph, schedule, &protected, checked_arrays) {
            Some(plan) => Some(plan),
            None => return refuse(Bailout::new(BailoutReason::Internal(
                "allocated instruction selection did not cover a block"))),
        }
    } else { None };
    let value_flow = match allocated_mir.as_ref() {
        Some(mir) => match mir.value_flow_with_checks(graph, schedule, &protected, checked_arrays) {
            Ok(flow) => flow,
            Err(bailout) => return refuse(bailout),
        },
        None => None,
    };
    // Any failed folded plan retries with fresh graph selection and all definitions.
    *image_dropped |= value_flow.as_ref().is_some_and(|flow| flow.has_absorbed_definitions());
    let (mut slot_plan, one_liveness_pass) = match value_flow.as_ref() {
        Some(flow) => plan_slots_with_value_flow(graph, schedule, sr_map, flow),
        None => plan_slots_one_pass_scratch(graph, schedule, sr_map, liveness_scratch.as_mut()),
    };
    // JIT round 19 wave 8 (lane osr8, OSR7-1,
    // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_LIVE`): the pending values a
    // promoted loop's pads name are live at those pads, in this plan and in
    // the allocator's model below alike. Empty (no change) unless the switch
    // and `..._PENDING_RESIDENT` are on and the graph keeps a silent store.
    let pending_live = pending_deopt_live_widening(graph, schedule);
    if !pending_live.rows.is_empty() {
        let grew = widen_slot_plan_for_pending(&mut slot_plan, &pending_live);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] loop field promote: {} pending value(s) live to their pads, {grew} slot range(s) widened",
                pending_live.rows.len()
            );
        }
    }
    // JIT round 25 (item 3, `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`, default ON):
    // every `Prim` colour before every rootable one, so the words the
    // fresh-frame clear leaves out under the claim are one run. Before the
    // verification, which re-derives the renumbered plan's soundness.
    // JIT round 25 wave 3 (`CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS`, default ON since j27):
    // snapshot-named non-reference values coloured by range instead of
    // pinned. Never on the keep-everything retry, which is where an attempt
    // whose frame states read a ranged home outside the plan's reads lands
    // (`image_dropped` below).
    // JIT round 25 wave 8 (W8-6, `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE`, default ON since j27):
    // the same ranging for the splice-local values of either class, so the
    // frame words of disjoint splices are shared. One pass for both switches:
    // the candidate sets are a union, read against one reader model.
    // Wave 9 (W9-4): `..._SPLICE_SLOT_SHARE_ALL_REFS` adds every reference.
    let range_select = splice_slot_share_select(!spliced_ranges.is_empty());
    // Wave 9 (W9-4): the `[ir-splice-slot-share]` census, one line per
    // lowering attempt under `CRATONVM_DBG_JITC` whenever a ranging switch is
    // on or the compile spliced something; counted before the ranging.
    let share_census = (crate::x64::dbg_jitc_enabled()
        && (range_select.all_prims
            || range_select.all_refs
            || ir_splice_slot_share_enabled()
            || !spliced_ranges.is_empty()))
    .then(|| pinned_origins(graph, &slot_plan, spliced_ranges));
    let mut share_outcome: &'static str = if keep_deopt_image { "retry" } else { "off" };
    if !keep_deopt_image
        && (range_select.all_prims || range_select.splice_local || range_select.all_refs)
    {
        let why = apply_snapshot_range_colours_why(
            graph,
            schedule,
            spliced_ranges,
            &mut slot_plan,
            range_select,
        );
        share_outcome = match why {
            Ok(()) => "engaged",
            Err(skip) => skip.label(),
        };
        let ranged = why.is_ok();
        // Any refusal of this attempt from here on -- the colouring check
        // below included -- earns the keep-everything retry, which plans every
        // snapshot-named value pinned again.
        *image_dropped |= ranged;
        if ranged && crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-snapshot-range-colours: {} value(s) ranged ({} ref), \
                 colours {} -> {}{}",
                slot_plan.snapshot_ranged_count(),
                slot_plan.snapshot_ranged_ref_count(graph),
                slot_plan.slots_before_ranging(),
                slot_plan.slots,
                if range_select.splice_local || range_select.all_refs {
                    " (splice slot share)"
                } else {
                    ""
                }
            );
        }
    }
    // JIT round 25 wave 2 (`CRATONVM_JIT_IR_PINNED_PRIM_COLOUR_CLAIM`): and
    // the claimable `Pinned` colours with them, so the claimed words of the
    // band stay one run.
    if ir_primitive_colour_claim_enabled() {
        if ir_pinned_prim_colour_claim_enabled() {
            let claimable = claimable_pinned_nodes(graph, &slot_plan);
            slot_plan.group_claimed_colours_first(&claimable);
        } else {
            slot_plan.group_prim_colours_first();
        }
    }
    let slot_plan = slot_plan;
    let slot_verdict = match value_flow.as_ref() {
        Some(flow) => verify_slot_colouring_with_value_flow(graph, schedule, &slot_plan, flow),
        None => verify_slot_colouring(graph, schedule, &slot_plan),
    };
    if let Err(bailout) = slot_verdict { return refuse(bailout); }
    if crate::regalloc::one_liveness_pass_enabled()
        && false
    {
        eprintln!(
            "[ir-ls] one liveness pass: {}",
            match &one_liveness_pass {
                Ok(_) => "shared",
                Err(why) => why.label(),
            }
        );
    }
    let mut one_liveness_pass = one_liveness_pass.ok();
    // JIT round 25 wave 2 (`CRATONVM_JIT_IR_FRAME_PARAM_LOCALS_ONLY`, default ON since j27):
    // the Java-local words the frame reserves. The bound below and the
    // lowerer are built from this one answer, so "the frame we checked" is
    // still "the frame we build". `frame_bytes_before` (debug only) is the
    // frame with every local reserved.
    let frame_locals = ir_frame_locals(
        graph,
        num_params,
        num_locals,
        ir_frame_param_locals_only_enabled(),
    );
    let all_locals_bytes =
        estimate_frame_bytes(num_locals, slot_plan.slots_before_ranging(), &frame_needs);
    let frame_bytes_before = i32::try_from(all_locals_bytes).unwrap_or(i32::MAX);
    let frame_estimate = estimate_frame_bytes(frame_locals, slot_plan.slots, &frame_needs);
    if let Some(c) = share_census {
        // JIT round 25 wave 9 (W9-4): `grep '\[ir-splice-slot-share\]'`.
        // `frame` is this attempt's frame before -> after the ranging, both
        // with this attempt's locals; `attempt=retry` is the keep-everything
        // lowering after a refusal (the ranging of the attempt before it
        // was given up).
        let key = if graph.scope_method_key.is_empty() {
            "-"
        } else {
            graph.scope_method_key.as_str()
        };
        eprintln!(
            "[ir-splice-slot-share] method={key} attempt={} outcome={share_outcome} \
             switches=prims:{} splice-local:{} all-refs:{} splices={} chain-ranged={} \
             pinned: root-ref={} root-prim={} splice-ref={} splice-prim={} phi={} param-const={}; \
             splice-local={} (ref {}); ranged={} (ref {}); colours {} -> {}; frame {} -> {} bytes",
            if keep_deopt_image { "retry" } else { "first" },
            u8::from(range_select.all_prims),
            u8::from(range_select.splice_local),
            u8::from(range_select.all_refs),
            spliced_ranges.len(),
            c.chain_ranged,
            c.root_ref,
            c.root_prim,
            c.splice_ref,
            c.splice_prim,
            c.phi,
            c.param_const,
            c.splice_local,
            c.splice_local_ref,
            slot_plan.snapshot_ranged_count(),
            slot_plan.snapshot_ranged_ref_count(graph),
            slot_plan.slots_before_ranging(),
            slot_plan.slots,
            estimate_frame_bytes(frame_locals, slot_plan.slots_before_ranging(), &frame_needs),
            frame_estimate,
        );
    }
    if let Err(bailout) = check_frame_size(frame_estimate, DEFAULT_MAX_FRAME_BYTES) {
        return refuse(bailout);
    }
    if false {
        eprintln!(
            "[ir-slots] nodes={} blocks={} slots={} peak_live={} frame_bytes={frame_estimate} \
             chain_ranged={}",
            graph.nodes.len(),
            schedule.blocks.len(),
            slot_plan.slots,
            slot_plan.peak_live,
            slot_plan.chain_ranged,
        );
        for (id, colour) in slot_plan.node_color.iter().enumerate() {
            if let Some(c) = colour {
                eprintln!(
                    "[ir-slots]   node {id:3} -> colour {c} class={:?}",
                    slot_plan.class.get(id).copied().flatten(),
                );
            }
        }
    }

    // ── Every emitted value has a location, decided before emission ──
    //
    // The acceptance test for the `[rbp - 0]` defect: drop a node from the
    // schedule and this refuses the compile here, with no code emitted at all,
    // instead of emitting a read of the saved caller frame pointer and
    // detecting it afterwards with a sticky bit.
    if let Err(bailout) = verify_data_locations(graph, schedule) {
        return refuse(bailout);
    }

    // Buffer sizing. The historical estimate (`nodes * 32 + 256`) predates call
    // lowering: an arithmetic node emits well under 32 bytes, but a single
    // MIC + 4-way-PIC dual-ABI inline-cache site emits ~360 and a direct call
    // ~60. With
    // `ir_compatible`'s invoke budget raised to `ir::IR_MAX_INVOKES`,
    // under-estimating here would silently TRUNCATE the body —
    // `ExecutableBuffer::emit` sets a sticky `overflowed` flag and skips the
    // write rather than panicking — so budget every call node explicitly, with
    // the same saturating arithmetic the single-pass sizing uses.
    let call_nodes = graph
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.op,
                Op::Call { .. }
                    | Op::IndyBridge { .. }
                    | Op::MultiNewArray { .. }
                    | Op::NewCp { .. }
            )
            // JIT round 20 (MHIFACE19-1): a seen-class `instanceof` adds its
            // two cell compares and post-call write (~110 bytes; ~180 with
            // round 21's verdict test before the write) to the arm.
            || matches!(n.op, Op::InstanceOf { seen_cell, .. } if seen_cell != 0)
        })
        .count();
    let estimate = ir_code_buffer_estimate(graph.nodes.len(), call_nodes)
        .saturating_add(ir_ref_home_publication_reserve(graph, schedule, &slot_plan))
        .saturating_add(fresh_clear_cover_reserve(
            schedule.freq.is_loop_header.iter().filter(|&&h| h).count(),
        ));
    #[cfg(test)]
    let estimate = if capacity_override.is_none() {
        IR_FIRST_ATTEMPT_CAPACITY_FOR_TEST
            .with(|c| c.get())
            .unwrap_or(estimate)
    } else {
        estimate
    };
    // The retry never goes BELOW the estimate: `wanted()` is exact for the
    // first attempt's bytes, and the estimate is what every other compile gets.
    let capacity = match capacity_override {
        Some(c) => c.max(estimate),
        None => estimate,
    };
    // `new_in`, not `new`: the process-wide JIT code arena carves this body
    // out of a shared executable reservation instead of taking its own
    // `VirtualAlloc`/`mmap`. Behaviour is byte-identical while
    // `CRATONVM_JIT_CODE_ARENA` is unset, because `new_in` falls straight
    // through to `new` then — so this call site is what makes the flag
    // reachable, not what changes the default.
    // A failed reservation (code cache at its cap, arena exhausted) is a
    // refusal like any other: route it through `refuse` so it is counted and
    // labelled instead of vanishing through a bare `?`.
    let Some(mut buf) =
        ExecutableBuffer::new_in(crate::platform::shared_jit_code_arena(), capacity)
    else {
        return refuse_labelled("executable-buffer-reservation-failed");
    };
    buf.set_tag("ir-lower");

    let mut lowerer = Lowerer::new(
        graph,
        schedule,
        buf,
        num_params,
        frame_locals,
        &slot_plan,
        helpers,
        branch_hints,
        // NOT `&[]`. This argument was dropped on the floor from the day
        // splicing landed: the parameter arrived, `Lowerer::new` got an
        // empty slice, and `Lowerer::resume_bci` was therefore the identity
        // in every production lowering. Every deopt inside a spliced body
        // then recorded its RELOCATED bci — a program point that does not
        // exist in the method's own bytecode — so the interpreter's sink
        // found no matching deopt point, defaulted the reason to
        // `UnreachedCode`, and refused the replay against a bci nothing
        // could resume at. See
        // `ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828.md`.
        spliced_ranges,
        sr_map,
        direct_calls,
        ic_slots,
        compact_fields,
        inline_frame_sites,
    );
    // Delimit attempts so a census reader can reject reused code addresses
    // instead of mixing an abandoned attempt with a later accepted body.
    let location_census_base = (crate::x64::dbg_jitc_enabled()
        && false)
        .then(|| lowerer.buf.as_ptr() as usize);
    if let Some(base) = location_census_base {
        eprintln!("[ir-location-begin] base={base:#x} method={}", graph.scope_method_key);
    }
    lowerer.direct_helpers = *direct_helpers;
    lowerer.cycle_cells = cycle_cells.clone();
    lowerer.tlab_announce_classes = tlab_announce_classes.clone();
    lowerer.array_list_layout = array_list;
    lowerer.string_layout = CompileStringLayoutScope::current();
    lowerer.protected_ranges = protected_ranges.to_vec();
    lowerer.osr_handler_reachable = osr_handler_reachable.map(<[u32]>::to_vec);
    // Not when the unroller relied on a trap-free body (wave 18, lane L2):
    // its net below refuses a compile that built any deopt point, but the
    // poll exits are appended after that check, and a header state naming a
    // value the unroller killed would resume with an `Undefined` local.
    //
    // Shape 2 (b) of the i41 page, its last member: under
    // `CRATONVM_JIT_IR_EXITS_BESIDE_DROPPED_SNAPSHOT_VALUES` the exits are
    // kept, and each one whose header state lost a live value refuses on its
    // own (`Lowerer::state_lost_a_value_to_the_optimizer`). The full unroller
    // leaves a killed node in a snapshot, which `current` already refuses; a
    // strength-reduced multiply leaves `NO_NODE`, which the state would
    // otherwise describe as `Undefined`.
    lowerer.optimizer_dropped_snapshot_values = unroll_used_unreachable_frames;
    if unroll_used_unreachable_frames
        && poll_mode_exits != IrPollModeExits::Off
        && false
    {
        eprintln!(
            "[ir-mode-exit] optimizer-dropped-snapshot-values {} method={}",
            if poll_mode_exits.is_method_entry() {
                "method-entry"
            } else {
                "osr-door"
            },
            graph.scope_method_key,
        );
    }
    lowerer.poll_mode_exits =
        if unroll_used_unreachable_frames && false {
            IrPollModeExits::Off
        } else {
            poll_mode_exits
        };
    lowerer.poll_exit_bytecode = poll_exit_bytecode;
    // gen r5w2/oomjit6: opt-in, and only with the method's bytecode in hand.
    // gen r5w3/live7: the same facts feed the store-free claim
    // (`CRATONVM_JIT_PRECISE_FRAME_LIVENESS`); with both flags off nothing is
    // built and every byte below is unchanged.
    let clear_stores = true;
    let precise_liveness = true;
    // gen r5w5/oomjit9: the default-on dead-home clears need the same facts.
    let dead_home_clears = true;
    // gci i1/fr (opt-in): allocation nodes are armed sites too. Read here and
    // by the two keep-set builders (`dead_ref_clear_arms`), once per compile.
    lowerer.alloc_site_clears = true;
    // gci i4/ir (opt-in): the pinned non-call GC points too, read the same way.
    lowerer.other_site_clears = true;

    // JIT round 21, lane argwin (OOM21-1, default ON): the owned-arguments
    // admission reads the same bytecode and table; `None` (the door withheld
    // it for a debugger, or the switch is off) owns no site.
    lowerer.owned_args_bytecode = dead_ref_clear_code.filter(|_| ir_callee_owned_args_enabled());
    lowerer.dead_ref_clear = match dead_ref_clear_code {
        Some(bytecode) if clear_stores || precise_liveness || dead_home_clears => {
            plan_dead_ref_slot_clear(
                graph,
                schedule,
                &slot_plan,
                bytecode,
                spliced_ranges,
                true,
                liveness_scratch.as_mut(),
                // ARGWIN21-7: never on the keep-everything retry, which is
                // where a flat deopt at a pc the rule read as chained lands.
                !keep_deopt_image && ir_keep_set_spliced_invoke_args_enabled(),
            )
        }
        _ => None,
    };
    lowerer.keep_set_chain_only_pcs = lowerer
        .dead_ref_clear
        .as_ref()
        .map(|d| d.chain_only_pcs.clone())
        .unwrap_or_default();
    *image_dropped |= !lowerer.keep_set_chain_only_pcs.is_empty();
    lowerer.dead_ref_clear_stores = clear_stores && lowerer.dead_ref_clear.is_some();
    lowerer.frame_liveness_on = precise_liveness && lowerer.dead_ref_clear.is_some();
    // gci i5/ir (opt-in): the loop back-edge polls are armed sites too; the
    // keep-set builders above recorded their after-sets under the same switch.
    if lowerer.dead_ref_clear.is_some() && ir_poll_gc_point_clears_enabled() {
        lowerer.poll_edge_pos =
            block_edge_positions(schedule, &lowerer.slot_plan.pos_of).unwrap_or_default();
    }
    if dead_home_clears {
        lowerer.prefix_slow_path_clears = true;
        // JIT round 20 wave 17, lane gcDead (I-9): read before the plan,
        // which arms allocation sites with it (and drops it on no plan).
        lowerer.alloc_dead_home_clear = true;
        // JIT round 21, lane oom: the remaining helper GC points, armed only
        // with the allocation sites (the plan drops both on no convergence:
        // `dead_ref_clear_site_for` reads `alloc_dead_home_clear` first).
        lowerer.helper_site_dead_home_clear =
            lowerer.alloc_dead_home_clear;
        lowerer.dead_home_clears_complete = lowerer.plan_dead_home_clears();
    }
    // gci i3/ir (opt-in): the reference parameters whose prologue word is
    // zeroed after their copy. Only with the dead-reference facts (they are
    // withheld under a debugger) and the default dead-home clears, which are
    // what then clear the copy where it is dead.
    if dead_home_clears && lowerer.dead_ref_clear.is_some() {
        // gci i4/ir: `node_to_block[graph.entry]` is `usize::MAX` on every
        // real schedule (`Op::Start` is never placed), which made i3's plan
        // admit nothing (vi3-ir A VOID); the entry block is found by its head.
        let entry_block = scheduled_entry_block(graph, schedule).unwrap_or(usize::MAX);
        let entry_has_predecessors = !schedule
            .blocks
            .first()
            .is_some_and(|b| b.predecessors.is_empty());
        // gci i4/ir: parameter 0 is a reference parameter like any other in a
        // STATIC method (`receiver_param` is `None` exactly for those in
        // `lib.rs`'s builder); it stays out for an instance method (the
        // receiver: self-lock exits, `held_monitors.rs`) and whenever a
        // method-monitor fact names it.
        let param0_admitted =
            graph.receiver_param.is_none() && graph.method_monitor_param.is_none();
        lowerer.ref_param_home_zero = ref_param_home_zero_candidates(
            graph,
            &schedule.node_to_block,
            entry_block,
            entry_has_predecessors,
            param0_admitted,
        );
        if false {
            eprintln!(
                "[ir-slots] ref param home zero plan: method={} osr={} entry_block={entry_block} preds={entry_has_predecessors} param0={param0_admitted} candidates={:?}",
                graph.scope_method_key,
                graph.osr_compile,
                lowerer.ref_param_home_zero
            );
        }
        // JIT round 25 item 2 (default ON since R25-FLIP, `=0` kills): the
        // whole candidate set, before the lazy plan below takes its members
        // out of `ref_param_home_zero`.
        {
            // r25 item 2, proposal 1 (`CRATONVM_JIT_IR_SHADOW_LIVE_RECEIVER`):
            // an instance method's receiver too, for the drop rule only (the
            // eager / lazy zeroing keep their own admission).
            let receiver = shadow_live_receiver_admitted(
                graph.receiver_param,
                graph.method_synchronized,
                lowerer.self_lock.is_some(),
                true,
            );
            let drop_candidates = if receiver {
                ref_param_home_zero_candidates(
                    graph,
                    &schedule.node_to_block,
                    entry_block,
                    entry_has_predecessors,
                    true,
                )
            } else {
                lowerer.ref_param_home_zero.clone()
            };
            lowerer.shadow_live_param_homes = drop_candidates
                .iter()
                .filter_map(|&id| match graph.nodes.get(id as usize).map(|n| &n.op) {
                    Some(&Op::Param(idx)) => i16::try_from((i32::from(idx) + 1) * 8)
                        .ok()
                        .map(|home| (id, home)),
                    _ => None,
                })
                .collect();
            lowerer.shadow_live_dbg = false;
        }
        // gci i5/ir (opt-in, the cost of the eager store): the zeroing moves to
        // the planned clears of each parameter's own colour.
        {
            lowerer.plan_lazy_ref_param_home_zero();
        }
    }
    // JIT round 20 wave 21 (lane pollroot): independent of the plan above --
    // a back-edge poll's clears are argued from dominance, not from it.
    lowerer.back_edge_dead_home_clear = true;

    // Null-check and bounds-check elimination. Runs on the SCHEDULED graph
    // because both facts are dominance facts and dominance is over the block
    // CFG the scheduler built -- so it must be after `schedule` and before any
    // array site is emitted, which is exactly here.
    lowerer.check_elision = check_elision;

    // Gap B: only `abi_regs.len()` integer registers carry incoming args (4 on
    // Win64, 6 on SysV), and a `needs_context` method spends the first of them
    // on the hidden VM pointer; anything past that arrives on the caller's
    // stack. `emit_prologue` loads those stack-resident arguments now, exactly
    // as the single-pass prologue does, so there is no refusal here any more.
    // History, because it cost a lot to learn: when the prologue could only
    // read registers, a guard here once sat inside `if lowerer.needs_context`
    // and left LEAF methods unguarded — `probes/EntryAbiArgSlotProbe.java`
    // measured `i4`/`s5` (5 slots) wrong and silently emptied Spring Boot's
    // property binding (`webflux-defaultpathcontainer-defaultseparator-classcast-FIXED.md`).
    // (The refusal that used to be here — "incoming arg slots exceed the entry
    // ABI registers" — is gone: `emit_prologue` loads stack-resident arguments
    // now, exactly as the single-pass prologue does. It was the commonest
    // whole-method refusal an ordinary accessor hit; `VolumeShort2.getIndex(III)I`
    // is `this` + three ints = four incoming slots, and touching one field turns
    // `needs_context` on, which is the fifth. Two separate investigations
    // reached that line by bisecting the lowerer rather than by reading a log.)

    // BUG FIX [jit-irlower #2]: reserve phi destination slots before any
    // block is lowered — a forward branch's edge copies (emit_phi_copies)
    // reference slots of phis in not-yet-lowered merge blocks.
    if let Err(bailout) = lowerer.prealloc_phi_slots() {
        return refuse(bailout);
    }

    // ── Linear-scan register residency (default ON since 2026-09-02) ─
    //
    // The production call site for `regalloc::allocate_linear_scan`. Installed
    // BEFORE the prologue and never touched again, so residency is a property
    // of the whole emission rather than something that changes mid-body.
    //
    // A verifier failure is the one hard stop. `plan_register_residency`
    // returns `Err` only when `verify_allocation` rejects the allocation — a
    // compiler bug — and emitting against an allocation nothing proved is
    // exactly the trade that has produced silent heap corruption here before.
    // Refuse the body; the method runs in a lower tier, which is always valid.
    // Everything else it can decline (register pressure, a liveness-model
    // disagreement, a value it cannot prove) comes back as `Ok(None)` and
    // leaves the colourer's memory layout in charge.
    //
    // The level-2 selector and this cache are MUTUALLY EXCLUSIVE, and the
    // exclusion belongs here rather than in either of them. `isel`'s encoder is
    // anchored byte-for-byte against the per-opcode arms under the assumption
    // that "the frame-homed allocation the encoder assumes IS this backend's
    // allocation" — which residency makes false, because an arm then reads its
    // operand out of a register while the tiler still emits a frame load.
    //
    // Neither outcome is unsound (write-through keeps the home word correct, so
    // the tiler's frame read gets the right value, and `Verify` mode is
    // fail-closed), but the two would silently disagree about bytes, which is
    // the one property the level-2 lane exists to be able to check. So while a
    // MIR mode is on, residency is off and the selector is compared against the
    // emission it was anchored to.
    // Interpreter round i2 wave 63 (lane L2, the i58-L2 proposal's first
    // measurement): `CRATONVM_DBG_IR_LS_TOKEN_TRIM_CENSUS=1` prints how many
    // intervals `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` would shorten in
    // this graph (`regalloc::TokenTrimCensus`). Diagnostic only: it builds
    // two throwaway models and changes nothing below. One line per attempt
    // (a buffer-overflow retry prints again).
    if crate::regalloc::ls_token_trim_census_enabled() {
        match crate::regalloc::token_trim_census(graph, schedule) {
            Some(mut census) => {
                // Wave 64 (lane L2): the frame-home column
                // (`CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES`).
                census.slot_plan = Some(token_trim_slot_column(graph, schedule, sr_map));
                eprintln!("{}", census.render());
            }
            None => eprintln!("[ir-ls] token-trim census: not converged"),
        }
    }
    if let Some(selected) = allocated_mir { lowerer.set_mir(selected, MirMode::Off); }
    let mut ls_active = false;
    // The residency planner's liveness model, kept for `emit_osr_entry_stubs`
    // below when this body may carry OSR entries (see the note there).
    let mut residency_live: Option<crate::regalloc::LiveModel> = None;
    if linear_scan_enabled() && !isel_emit_enabled() && !isel_verify_enabled() {
        let mut live = match one_liveness_pass.take() {
            Some(pass) => pass.into_live_model(graph, schedule),
            None => match value_flow.as_ref() {
                Some(flow) => crate::regalloc::build_live_model_with_value_flow(graph, schedule, flow),
                None => match liveness_scratch.as_mut() {
                    Some(scratch) => crate::regalloc::build_live_model_scratch(graph, schedule, scratch),
                    None => crate::regalloc::build_live_model(graph, schedule),
                },
            },
        };
        // OSR7-1 (lane osr8): the same widening as the slot plan's above.
        widen_live_model_for_pending(&mut live, &pending_live);
        // W9-2: one effective allocator switch for the plan, the site facts
        // and the lowerer (`reg_alloc_on` below).
        let reg_alloc_eff = r25_w7a_reg_alloc::effective(keep_deopt_image);
        let planned = plan_register_residency_with(
            graph,
            schedule,
            &slot_plan,
            &mut live,
            reg_alloc_eff,
            lowerer.mir.as_ref(),
        );
        // JIT round 25 item 1a (wave 2): the liveness facts the site-named
        // homes are checked against, for the values the plan made resident.
        // Copied before `live` moves; only when the switch can use them.
        // W7-A (`CRATONVM_JIT_IR_REG_ALLOC`): the facts of every `int` /
        // `long` value, since a back-edge source the plan left unassigned can
        // take its phi's register (`coalesce_back_edge_phi_sources`).
        let reg_alloc_live = reg_alloc_eff;
        let site_live = match &planned {
            Ok(Some(residency))
                if !keep_deopt_image
                    && (ir_elide_unnamed_homes_enabled()
                        || r25_w7a_reg_alloc::implied(
                            reg_alloc_live,
                            r25_w7a_reg_alloc::ELIDE_KEY,
                        )) =>
            {
                if reg_alloc_live {
                    r25_w7a_reg_alloc::extract_every_int(&live, schedule, graph)
                } else {
                    SiteLive::extract(&live, schedule, graph, &residency.gp_reg_of)
                }
            }
            _ => None,
        };
        // JIT round 19 wave 7 (lane osr7, OSR6-1): the allocator's ranges, for
        // the pads' residency descriptions (`Lowerer::pending_resident_reg`).
        // Copied before `live` moves; only when they can be read.
        let pending_ls = (ir_loop_field_promote_pending_resident_enabled()
            && !graph.promoted_sunk_stores.is_empty()
            && live.converged)
            .then(|| {
                (
                    live.range
                        .iter()
                        .map(|r| r.as_ref().map(|r| (r.lo, r.hi)))
                        .collect::<Vec<_>>(),
                    live.pos_of.clone(),
                )
            });
        if ir_osr_entry_enabled() {
            residency_live = Some(live);
        }
        match planned {
            Ok(Some(mut residency)) => {
                let cs_republish = std::mem::take(&mut residency.caller_saved_republish);
                if crate::ir_stage_reporting() {
                    // The phi count is reported beside the total because it is
                    // the one that answers "did the loop-carried values move",
                    // and a total alone cannot: nineteen resident transients
                    // beside a frame-homed loop counter reads the same as the
                    // shape this file was built to produce.
                    eprintln!(
                        "[ir] linear scan: {} values resident, {} demoted, {} of {} phis resident",
                        residency.promoted,
                        residency.demoted,
                        residency.phi_census.resident,
                        residency.phi_census.total(),
                    );
                }
                lowerer.set_residency(residency);
                // Round 25 (item 1b): a no-op unless the plan names a
                // caller-saved register.
                lowerer.install_caller_saved_residency(cs_republish);
                lowerer.site_live = site_live;
                if let Some((range, pos)) = pending_ls {
                    lowerer.pending_ls_range = range;
                    lowerer.pending_ls_pos = pos;
                }
                ls_active = true;
            }
            Ok(None) => {}
            Err(bailout) => return refuse(bailout),
        }
    }

    // ── Shadow instruction selection (default OFF, emits nothing) ────
    //
    // Placed here, after every refusal above, so the population it measures is
    // exactly the population that gets a compiled body — measuring methods the
    // lowerer then refuses would inflate the figure with code nobody runs.
    //
    // Deliberately NOT fail-closed, which is the one place in this file that is
    // true. See `isel::shadow_select_method`: a flag whose documented effect is
    // a count must not decide what compiles, or the count describes a different
    // program. A `covers()` violation is counted and printed, not raised.
    if isel_shadow_enabled() {
        let stats = crate::x64::isel::shadow_select_method(graph, schedule);
        if isel_shadow_reporting() {
            eprintln!("[ir-isel] {}", stats.summary_line());
        }
    }

    // ── The level-2 machine list (default OFF) ───────────────────────
    //
    // Increment 2. Unlike the shadow pass above this one IS fail-closed, in
    // both its modes: a method whose blocks the selector cannot cover, or whose
    // tiles disagree with the per-opcode arms, loses its optimized body rather
    // than getting one nobody checked.
    let mir_mode = if isel_emit_enabled() {
        Some(MirMode::Emit)
    } else if isel_verify_enabled() {
        Some(MirMode::Verify)
    } else {
        None
    };
    lowerer.value_flow = value_flow.unwrap_or_default();
    if let Some(mode) = mir_mode {
        match build_mir_plan(graph, schedule) {
            Some(plan) => {
                // Increment 2: the machine list gets an allocation, and the
                // allocation gets `verify_allocation`. Runs BEFORE `set_mir`,
                // so a plan the verifier rejects is never installed and the
                // encoder never sees it.
                match verify_mir_allocation(graph, schedule, &slot_plan, &plan) {
                    Ok(verdict) => mir_totals::record_alloc(verdict),
                    Err(bailout) => {
                        mir_totals::record_alloc_rejected();
                        // `Verify` mode's contract is that it changes no
                        // emitted byte, so it must not change which methods
                        // compile either: record the rejection, report it with
                        // `CRATONVM_DBG=ir-isel`, and leave the per-opcode arms
                        // to emit exactly what they always did. `Emit` mode has
                        // no such licence — its bytes ARE the allocation's, so
                        // an allocation nothing proved is refused outright.
                        if mode == MirMode::Emit {
                            return refuse(bailout);
                        }
                    }
                }
                lowerer.set_mir(plan, mode)
            }
            None => {
                return refuse(Bailout::new(BailoutReason::Internal(
                    "ir_lower: instruction selection did not cover a block",
                )))
            }
        }
    }

    // Round 16 wave 11 (lane frames2, `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED`,
    // default OFF): the deopt register image sized to the registers the plan
    // above hands out. After the residency plan (it decides what a frame
    // state can name) and before the first byte of the prologue (which
    // carves the frame).
    // Round 16 wave 17 (lane frames2, PC-1, `CRATONVM_JIT_IR_SAVE_BANDS_SIZED`,
    // default OFF): the callee-saved save bands sized to what the plan saves.
    // First, because the image is placed below the bands.
    lowerer.size_save_bands(ir_save_bands_sized_enabled());
    lowerer.size_deopt_image(ir_deopt_image_sized_enabled());
    // Round 16 wave 12 (lane polyclear, FR2P-1,
    // `CRATONVM_JIT_IR_DEOPT_IMAGE_EXITLESS`, default OFF): and no image at
    // all when no exit of this body can read one.
    // `|=`, not `=` (JIT round 25 wave 3): the slot plan's snapshot ranging may
    // already have asked for the retry; `run` resets the flag per attempt.
    *image_dropped |=
        lowerer.drop_unread_deopt_image(!keep_deopt_image && ir_deopt_image_exitless_enabled());
    // Round 17 (CG16P-4): constant homes, on the same fallback.
    *image_dropped |= lowerer.drop_const_homes(!keep_deopt_image && ir_const_home_drop_enabled());
    if keep_deopt_image && ir_const_home_drop_enabled() {
        lowerer.drop_guaranteed_unused_const_homes();
    }
    // The final retry has left the per-value scope; keep skipped reads too.
    lowerer.publish_reload_elide_on &= !keep_deopt_image;
    // JIT round 25 item 1a: unnamed homes, on the same fallback (planned in
    // `prepare_fusion_tables`; reported as dropped right after it).
    // JIT round 25 wave 7 (lane W7-A, `CRATONVM_JIT_IR_REG_ALLOC`): the
    // register-allocator increment implies the three switches below, each
    // unless its own key is set (`r25_w7a_reg_alloc::implied`).
    // W9-2: the same effective switch the plan read (`effective` folds in
    // the retry, the MIR modes and the three implied switches).
    lowerer.reg_alloc_on =
        lowerer.mir_mode == MirMode::Off && r25_w7a_reg_alloc::effective(keep_deopt_image);
    lowerer.elide_unnamed_homes_on = !keep_deopt_image
        && lowerer.mir_mode == MirMode::Off
        && (ir_elide_unnamed_homes_enabled()
            || r25_w7a_reg_alloc::implied(lowerer.reg_alloc_on, r25_w7a_reg_alloc::ELIDE_KEY));
    // Wave 3: store-at-inspection, a sub-switch of the above.
    lowerer.spill_at_inspection_on = lowerer.elide_unnamed_homes_on
        && (ir_elide_homes_spill_at_inspection_enabled()
            || r25_w7a_reg_alloc::implied(lowerer.reg_alloc_on, r25_w7a_reg_alloc::SPILL_KEY));
    // JIT round 25 wave 6 (lane W6-B): loop values, a sub-switch of both.
    lowerer.site_named_loop_on = lowerer.spill_at_inspection_on
        && (ir_site_named_loop_values_enabled()
            || r25_w7a_reg_alloc::implied(lowerer.reg_alloc_on, r25_w7a_reg_alloc::LOOP_KEY));
    // W7-A: the coalesced back-edge sources need the whole stack.
    lowerer.reg_alloc_on &= lowerer.site_named_loop_on;
    // JIT round 25 item 2, wave 3: the frame block's lazy clear, never on the
    // keep-everything retry (a refused plan is lowered once more without it).
    lowerer.block_lazy_clear_allowed = !keep_deopt_image
        && lowerer.mir_mode == MirMode::Off
        && ir_shadow_frame_block_lazy_clear_enabled();

    // The patchable entry pad, as the single-pass driver emits it (interpreter
    // round i1 wave 22, lane L6; `jit/src/not_entrant.rs`): a redefinition
    // makes this body not entrant with one atomic store over it.
    if lowerer.buf.pos() == 0 {
        lowerer.buf.emit(&crate::ENTRY_PATCH_PAD);
    }
    // JIT round 22 wave 12, lane runtime (page
    // `r22-w7-code-cache-sweep-phases-bistable-FIXED-20261020.md`): the entry counter,
    // in an optimizing body too, but only while the cold-body sweeper is armed.
    let sweep_entry_counter = lowerer.emit_sweep_entry_counter();
    // Round 12 wave 8 (lane callcost): which self-call samples are dominated,
    // and the entry fast return (after the pad, before the prologue). See
    // `r12w8-callcost-fib-call-anatomy-FIXED-20261001.md`.
    lowerer.plan_self_call_sample_elision();
    lowerer.emit_entry_fast_return();
    // JIT round 25 item 2, wave 4: before the prologue, whose lazy-clear plan
    // must not count a lazy reference's definition as a write.
    if !keep_deopt_image && lowerer.mir_mode == MirMode::Off && ir_lazy_ref_homes_enabled() {
        lowerer.plan_lazy_ref_homes();
    }
    // JIT round 25 wave 6 (W6-D): the leaf-entry plan, before the prologue it
    // shapes. A leaf frame is verified after the body
    // (`verify_leaf_entry`), and a refusal lowers once more without it (the
    // keep-everything attempt never plans one). Planned the same under every
    // MIR mode: the prologue is no tile, its verification reads only what the
    // emission recorded, and a mode-dependent frame shape would break the
    // level-2 byte oracle (verify mode changes no emitted byte).
    lowerer.plan_leaf_entry(!keep_deopt_image);
    *image_dropped |= lowerer.leaf_entry.plan.leaf_frame;
    // JIT round 25 wave 7 (W7-C): which calls' hot-path frame republish may
    // move to their sentinel side (`plan_tail_republish_sink`).
    lowerer.plan_tail_republish_sink();
    lowerer.emit_prologue();
    if !lowerer.leaf_entry.plan.elide_entry_poll {
        lowerer.emit_safepoint_poll(None, None);
    }

    // Emit blocks in order
    lowerer.prepare_fusion_tables();
    lowerer.emit_entry_layout_epoch_guard_if_needed();
    // JIT round 25 item 1a: a refusal of an attempt that elided an unnamed
    // home is retried with every home kept (`plan_tail_carries`).
    *image_dropped |= lowerer.elided_unnamed_homes();
    // JIT rx27 wave 6 (lane regalloc): a refused attempt that answered a guard
    // by a `TEST` is lowered again with every home kept (the plan is off then).
    *image_dropped |= lowerer.and_zero_guard_planned > 0 || lowerer.and_zero_widened > 0;
    // JIT rx27 wave 9: a refused skipped read is retried without that value.
    *image_dropped |= lowerer.publish_reload_elide_on;
    // JIT round 25 item 2, wave 3: a refused lazy-clear plan is retried
    // without it (`verify_block_lazy_clear_writes`).
    *image_dropped |= !lowerer.block_lazy_clear.is_empty();
    // Wave 4: a refused lazy-reference plan is retried without the values it
    // was refused for (`note_elide_refusal`), then without the plan.
    *image_dropped |= lowerer.lazy_ref_planned > 0;
    // Round nio21 wave 3 (lane L2): which call-site exception edges the pads
    // can enter (`ir::Op::CallThrew`; nothing unless the graph has one).
    lowerer.plan_call_throw_edges();
    // Interpreter round i1 wave 49, lane L3: the inline-frame-row census
    // (`CRATONVM_DBG_INLINE_ROW_CENSUS=1`; nothing without it).
    lowerer.begin_inline_row_census();
    for block_idx in 0..schedule.blocks.len() {
        lowerer.lower_block(block_idx);
    }
    // W7-C: nothing emitted after the body is a sunk call's hot path.
    lowerer.end_tail_republish_sink_body();
    // JIT round 19 wave 5 (lane osr5, OSR2-3): nothing without silent stores.
    lowerer.check_promoted_pending_coverage();
    lowerer.close_inline_row_census();
    // JIT round 19 wave 5 (lane handler5, SPLICE4-1): the compiled handler
    // probes, first after the body (every arm block's offset is known) and
    // before every reader of `call_exc_patches` and the rethrow pads.
    lowerer.emit_catch_probes();
    // Round 13 wave 8 (lane callcost3, CC2-1): the direct self-calls' cold
    // halves, first after the body and before every reader of
    // `call_exc_patches` below (the OSR facts, `emit_call_exc_stub`).
    lowerer.emit_deferred_self_call_tails();
    // zs s9/ca4: the split inline ZGC allocations' cold halves, for the same
    // reason and in the same place (each pushes its exit row).
    lowerer.emit_deferred_zgc_alloc_tails();
    // JIT round 23 wave 9 (lane W9-L7): the inline field reads' legacy reads
    // and checked-helper calls, same place, same reason (each pushes its exit
    // row). An implicit-null site's tail also registers its fault recovery pc.
    lowerer.emit_deferred_getfield_tails_with_fault_entries();
    lowerer.emit_deferred_putfield_tails();
    lowerer.emit_deferred_gen_alloc_tails();
    lowerer.emit_deferred_checkcast_tails();
    // Round nio21 wave 3 (lane L2): the call-site exception pads, also before
    // every reader of `call_exc_patches` and before `emit_rethrow_pads`: a
    // pad's fallback adds a row to one of the two.
    lowerer.emit_call_throw_pads();
    if crate::x64::dbg_jitc_enabled() && !lowerer.call_throw_plans.is_empty() {
        eprintln!(
            "[cratonvm-jitc] ir-call-throw sites_armed={} pads={} (of {} site(s) in the graph)",
            lowerer.call_throw_plans.len(),
            lowerer.call_throw_pads_emitted,
            graph.call_throw_sites.len(),
        );
    }
    // Round nio21 wave 5 (lane L2): the implicit sites' census.
    if crate::x64::dbg_jitc_enabled() && !lowerer.implicit_throw_plans.is_empty() {
        eprintln!(
            "[cratonvm-jitc] ir-implicit-throw sites_armed={} pads={}",
            lowerer.implicit_throw_plans.len(),
            lowerer.implicit_throw_pads_emitted,
        );
    }
    // Round 14 wave 3 (lane calls, CC3-1): the cold-tail census.
    if crate::x64::dbg_jitc_enabled()
        && (lowerer.cross_call_cold_tails != (0, 0) || lowerer.ic_hit_exit_census != (0, 0))
    {
        eprintln!(
            "[cratonvm-jitc] ir-call-cold-tails cross_deferred={} cross_inline={} \
             ic_hit_exits={} ic_admissible={}",
            lowerer.cross_call_cold_tails.0,
            lowerer.cross_call_cold_tails.1,
            lowerer.ic_hit_exit_census.0,
            lowerer.ic_hit_exit_census.1,
        );
    }
    // Round 9 wave 6: `ls_edge_copy_site_exists` PREDICTED that every edge
    // carrying split-residency resolution copies reaches `emit_phi_copies`.
    // An edge whose copies never ran would leave its target reading a
    // register that holds something else, so a wrong prediction refuses the
    // method here rather than emitting it.
    if let Some(i) = lowerer.ls_edge_copies_done.iter().position(|done| !*done) {
        let (p, s) = lowerer
            .ls_edge_copies
            .get(i)
            .map_or((usize::MAX, usize::MAX), |e| (e.pred, e.succ));
        lowerer.latch_bailout(Bailout::with_context(
            BailoutReason::Internal("ir_lower: a split-residency edge copy was never emitted"),
            format!("block {p} -> {s}"),
        ));
    }

    // Soundness bail (ES SortingDigestTests -Jit on): some emitted use asked
    // for the frame slot of a node that was never allocated one (scheduler
    // gap — the node sits in no emitted block), or a slot allocation ran past
    // the frame's spill cap. Discard the artifact and let the caller fall back
    // to the single-pass backend.
    //
    // Both are now UNREACHABLE — `verify_data_locations` and the frame-size
    // check above refuse those graphs before emission begins — so this is a
    // net, not a mechanism. It is kept, and upgraded from one sticky bit to a
    // structured reason, so that if a future lowering finds a path the
    // pre-checks miss, the compile still fails closed *and* says why.
    // The byte-equality oracle's verdict, for the whole method.
    //
    // Reported before it is acted on, because a mismatch that only ever shows
    // up as "this method did not compile" is a mismatch nobody can diagnose.
    if lowerer.mir_mode != MirMode::Off {
        // Only the disagreements, and only under the debug switch: a corpus run
        // compiles thousands of methods, and a per-compile line for each would
        // bury the one line that matters. The totals are printed once at exit
        // (`vm-cli::maybe_dump_shutdown_reports`).
        if lowerer.mir_mismatches != 0 && isel_shadow_reporting() {
            eprintln!(
                "[ir-isel] mir mode={:?} tiles={} MISMATCHES={}",
                lowerer.mir_mode, lowerer.mir_tiles, lowerer.mir_mismatches
            );
        }
        mir_totals::record(
            lowerer.mir_tiles,
            lowerer.mir_mismatches,
            lowerer.mir_shadow_tiles,
            lowerer.mir_arm_bytes,
            lowerer.mir_enc_bytes,
        );
        if lowerer.mir_mismatches != 0 {
            return refuse(Bailout::new(BailoutReason::Internal(
                "ir_lower: the level-2 encoder disagreed with the per-opcode lowering",
            )));
        }
    }

    // Interpreter round i2 wave 64 (lane L2): the token-read guard's
    // per-compile census, beside the token-trim census line. `refusals=0`
    // over a run is the measured form of "no arm reads through a token slot".
    if lowerer.token_read_guard && crate::regalloc::ls_token_trim_census_enabled() {
        // JIT round 19 wave 10 (lane tok10, i64-L2): `scope=` splits the
        // three counts per lowering scope as `name:nodes/refusals/pinned-hits`.
        // JIT round 19 proposal wave 11 (lane lv11, TOK10-3): `point_stale=`
        // only under the point-range switch, so the default line is unchanged.
        let point_stale = if lowerer.token_guard_point_range {
            format!(" point_stale={}", lowerer.token_point_stale.get())
        } else {
            String::new()
        };
        eprintln!(
            "[ir-ls] token-read guard: nodes={} refusals={} pinned-hits={} scope={}{point_stale}",
            lowerer.token_read_nodes,
            lowerer.token_read_refusals.get(),
            lowerer.token_read_pinned_hits.get(),
            lowerer.token_scope_census(),
        );
    }
    // JIT round 25 (item 4): how this body's inline field accesses treated
    // their receivers' compactness. Under the switch only, so the default
    // `CRATONVM_DBG_JITC` stream is unchanged.
    if lowerer.proven_fields_on && crate::x64::dbg_jitc_enabled() {
        let c = lowerer.proven_field_census;
        eprintln!(
            "[cratonvm-jitc] ir proven-receiver-fields: loads full={} establish={} proven={} \
             stores full={} establish={} proven={} (nodes={}) spliced_establish={} \
             ref_stores_proven={} gated_ref_store_establish={} preheader_proofs={} \
             osr_entry_rechecks={} coestablished={} method={}",
            c.load_full,
            c.load_establish,
            c.load_proven,
            c.store_full,
            c.store_establish,
            c.store_proven,
            graph.nodes.len(),
            c.establish_spliced,
            c.ref_store_proven,
            c.gated_ref_store_establish,
            lowerer.field_proof_osr_obligations.borrow().len(),
            c.osr_entry_rechecks,
            c.coestablished,
            graph.scope_method_key,
        );
    }
    if let Some(bailout) = lowerer.take_latched_bailout() {
        describe_unallocated_value(graph, &bailout);
        return refuse(bailout);
    }
    if lowerer.unallocated_slot_use.get() {
        return refuse(Bailout::new(BailoutReason::Internal(
            "ir_lower: unallocated slot latched without a reason",
        )));
    }

    // Soundness bail: every emitted shadow PUSH must have exactly one emitted
    // RELOAD. The push advances the thread's shadow `top`; only the reload
    // retracts it. An unmatched push leaks a few slots per execution of that
    // site, so a recursive method walks `top` off the end of the shadow stack
    // and the next push writes into whatever follows the mapping. That
    // presents as heap corruption in an unrelated allocator, arbitrarily far
    // from the JIT — the self-recursive call route did it, and it took a
    // gdb backtrace showing ASCII bytes in a mimalloc free-list header to
    // attribute. A static count is enough (both are emitted per site, not per
    // path), and refusing the body is strictly better than shipping it.
    if lowerer.shadow_pushes != lowerer.shadow_reloads {
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] lower_inner refused: {} shadow pushes vs {} reloads — \
                 an unmatched push leaks the thread's shadow top",
                lowerer.shadow_pushes, lowerer.shadow_reloads
            );
        }
        note_lower_bail("shadow-push-reload-imbalance");
        return refuse(Bailout::new(BailoutReason::Internal(
            "shadow-push-reload-imbalance",
        )));
    }

    // real-frame-deopt (step 3): emit the shared deopt stub after the method
    // body so failed guards can jump to it, then patch in-method branches.
    // Captured BEFORE the stubs are emitted: `emit_deopt_stub` takes its patch
    // list, so afterwards the question cannot be asked.
    // The guards' out-of-line deopt pads, first: each ends in a `JMP` to the
    // deopt stub, recorded in `deopt_stub_patches`, which the line below reads.
    lowerer.emit_deopt_pads();
    // Round 11 wave 10 (lane irexc): no GUARD exit, read before the rethrow
    // pads add their own `deopt_stub_patches` rows. A pad is an exceptional
    // exit, not a guard; tier proposal 27's OSR fact below tells them apart.
    let guard_exit_free = lowerer.deopt_stub_patches.is_empty();
    // The reason-9 rethrow pads, before `osr_sentinel_free` is read: a pad's
    // `JMP` makes the body not sentinel-free, which keeps that bit meaning "no
    // exit at all" for every reader that relies on it.
    lowerer.emit_rethrow_pads();
    // Round 15 wave 1 (lane sync, SS8-2): a self-locking body's own monitor
    // edges are exits too (`self_lock_unheld_exc_patches`).
    let osr_sentinel_free = lowerer.deopt_stub_patches.is_empty()
        && lowerer.call_exc_patches.is_empty()
        && lowerer.self_lock_unheld_exc_patches.is_empty();
    // Tier proposal 27 (round 11 wave 10, lane irexc): the body's exits when
    // none of them is a guard — see `CompiledMethod::ir_osr_exception_exit_bcis`.
    // Taken here, before `emit_call_exc_stub` consumes `call_exc_patches`.
    let osr_exception_exit_bcis: Option<Box<[u32]>> =
        if guard_exit_free && !lowerer.propagating_exit_holds_monitor {
            lowerer
                .call_exc_patches
                .iter()
                .map(|&(_, bci)| u32::try_from(bci).ok())
                .collect::<Option<Vec<u32>>>()
                .map(|mut bcis| {
                    bcis.sort_unstable();
                    bcis.dedup();
                    bcis.into_boxed_slice()
                })
        } else {
            None
        };
    // Tier proposal 27 phase 3 (round 11 wave 12, lane irexc): the same list
    // plus the guard exits, when every guard is exact or value-equivalent and
    // resumable in place — see `CompiledMethod::ir_osr_guard_exit_bcis`. Every
    // guard box was pushed while the body was lowered, so the list is whole.
    //
    // Round 11 wave 14 (lane osrmerge): under `CRATONVM_JIT_OSR_OPAQUE_ENTRY`,
    // when the strict fact is refused, also the fact WITHOUT the
    // classification count (`osr_exit::guard_exit_fact_opaque`) — published
    // below only if every entry `emit_osr_entry_stubs` emits is opaque. `None`
    // with the switch off, so nothing below changes.
    let (mut osr_guard_exit_bcis, mut osr_guard_exit_bcis_opaque): (
        Option<Box<[u32]>>,
        Option<Box<[u32]>>,
    ) = {
        let call_exc_bcis: Vec<usize> = lowerer
            .call_exc_patches
            .iter()
            .map(|&(_, bci)| bci)
            .collect();
        let strict = crate::osr_exit::guard_exit_fact(
            &lowerer.deopt_boxes,
            lowerer.classified_guard_exits,
            &call_exc_bcis,
            lowerer.propagating_exit_holds_monitor,
        );
        let opaque = if strict.is_none()
            && (crate::osr_exit::osr_opaque_entry_enabled()
                || poll_mode_exits == IrPollModeExits::OsrDoor
                || graph.unloaded_static_call_traps)
        {
            crate::osr_exit::guard_exit_fact_opaque(
                &lowerer.deopt_boxes,
                &call_exc_bcis,
                lowerer.propagating_exit_holds_monitor,
            )
        } else {
            None
        };
        (strict, opaque)
    };
    // JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-2): the strict fact was
    // refused (a SPECULATIVE guard: its interpreter continuation may falsify a
    // header fact) and the relaxed one admits every exit. Such a body's guard
    // exits are sound only when EVERY entry the door can take is an opaque
    // merge, which is what commit 44df838b5f gave every OSR-door build. Taken
    // here, before the entry block below may publish (and so `take`) the
    // relaxed fact. `CompiledMethod::ir_osr_guard_fact_needs_opaque_entries`.
    let osr_guard_fact_wants_opaque =
        osr_guard_exit_bcis.is_none() && osr_guard_exit_bcis_opaque.is_some();
    // The two counts `osr_sentinel_free` is computed FROM, kept for the
    // diagnostic below: `emit_deopt_stub` / `emit_call_exc_stub` take both
    // vectors a few lines down, so after them the question cannot be asked.
    let osr_nonfree_dbg = (
        lowerer.deopt_stub_patches.len(),
        lowerer.call_exc_patches.len(),
    );
    // W6-D: the same two counts, for `verify_leaf_entry` after the stubs.
    lowerer.leaf_entry.stub_exits = osr_nonfree_dbg.0 + osr_nonfree_dbg.1;
    // Round 12 wave 8 (lane callcost): the entry fast return's `MOV RAX; RET`,
    // the first thing after the body.
    lowerer.emit_entry_fast_return_stub();
    // Round 16 (lane codegen, CG16-2): the integer divisions' out-of-line
    // fixups. RAX-only code reached by a forward `JE` from the body and
    // returning by `JMP`; no exit, so no fact above or below reads it.
    lowerer.emit_div_cold_fixups();
    // The outlined safepoint slow paths, before the other tail stubs: each is
    // reached by a forward `JNZ` from the body and returns with a `JMP`, so it
    // only needs to be after the body, and putting it first keeps both hops
    // short.
    lowerer.emit_outlined_polls();
    // The back-edge polls' mode-exit pads (interpreter round i1 wave 15, lane
    // L2), after the outlined polls (which request them too) and after every
    // OSR fact above was read: such an exit resumes at a state the graph
    // itself reaches and is admitted by the point it carries, not by those
    // facts, so it must not change them. Before `emit_deopt_stub`, which takes
    // the `deopt_stub_patches` rows these add.
    lowerer.emit_poll_exit_pads();
    // Interpreter round i1 wave 28, lane L6: the post-call exit sites' stubs,
    // for the same reason and in the same place (after every OSR fact, before
    // `emit_deopt_stub`, which takes the `deopt_stub_patches` rows they add).
    lowerer.emit_post_call_exit_stubs();
    // The `Op::Switch` jump tables. Data, not code — see `SwitchTablePlan` for
    // why they cannot sit inline behind their own indirect `JMP`. Before
    // `patch_branches`, which resolves the entries this reserves.
    lowerer.emit_switch_tables();
    lowerer.emit_deopt_stub();
    // Gap B: emit the shared call-exception bail stub after the body so each
    // dispatch site's sentinel `JE` reaches it.
    lowerer.emit_call_exc_stub();
    // The optimizing tier's own OSR entry points. After the body, because each
    // stub ends in a `JMP` to a native offset the body has already produced;
    // before `patch_branches`, because a stub's own `JMP` is patched here and
    // must not be mistaken for an in-body branch.
    //
    // The liveness model is the one `plan_register_residency_with` built when
    // residency ran (it only changed `pinned`, which the stubs never read), and
    // a fresh `build_live_model` otherwise. It is a pure function of the graph
    // and the schedule either way, so the OSR decision does not depend on
    // whether residency ran; only the second build is saved
    // (`r11w4-ircore-live-model-built-twice-per-osr-compile`).
    //
    // ── Not emitted into a body no door can enter (2026-09-16) ────────
    //
    // The ONLY consumer is `vm::runtime::interpreter::jit_bridge::try_osr`,
    // and its admission is `ir_osr_entry_addr(pc).is_some() &&
    // ir_osr_sentinel_free` — BOTH. So a body that is not sentinel-free is one
    // the door will refuse whatever entries it carries, and every stub emitted
    // into it is dead weight: the bytes, the `JMP` patch, the published
    // `(bci, offset, needed)` triple, and this `build_live_model` pass.
    //
    // `ir_osr_sentinel_free` means the body emits no deopt stub AND no
    // call-exception stub, so — as `OSR_OPTIMIZING_REFUSED`'s doc in
    // `jit_bridge.rs` states outright — **any method containing a call fails
    // it**. That is most methods, which makes this the common case rather than
    // a corner. The test is free: `osr_sentinel_free` is computed twenty lines
    // above, deliberately before the stubs that would destroy the evidence.
    //
    // `CRATONVM_JIT_IR_OSR_ENTRY_ALWAYS=1` restores unconditional emission, so
    // one binary can A/B the skip and so a test can exercise a stub in a body
    // that would otherwise carry none.
    //
    // Tier proposal 27 (round 11 wave 10, lane irexc): a body whose only exits
    // are exceptional (`osr_exception_exit_bcis`) is one the door CAN enter
    // under `CRATONVM_JIT_OSR_OPTIMIZING_EXC_EXITS=1`, so it gets its stubs
    // under that switch. Off, the stubs are exactly what they were.
    //
    // Phase 3 (round 11 wave 12, lane irexc): a body whose guard exits the
    // door can resume without a plan (`osr_guard_exit_bcis`) gets them under
    // `CRATONVM_JIT_OSR_OPTIMIZING_GUARD_EXITS=1`.
    //
    // Round 15 wave 1 (lane sync, SS8-2): never into a self-locking body. An
    // OSR entry runs it inside the interpreter frame that already holds the
    // method monitor, and its exits would release a hold it never took.
    let osr_entries_wanted = lowerer.self_lock.is_none()
        && ir_osr_entry_enabled()
        && (osr_sentinel_free
            || ir_osr_entry_always_enabled()
            || (osr_exception_exit_bcis.is_some() && ir_osr_exception_exit_entries_enabled())
            || ((osr_guard_exit_bcis.is_some() || osr_guard_exit_bcis_opaque.is_some())
                && ir_osr_guard_exit_entries_enabled()));
    if ir_osr_entry_enabled() && !osr_entries_wanted {
        note_osr_entry_skipped_not_sentinel_free();
        // WHICH stub cost the body its OSR entries, and where it came from.
        //
        // Round 9 wave 12. The counter above says how OFTEN this happens; it
        // has never said WHY, and "why is this loop not sentinel-free" is the
        // question `perf-long-counted-loops-lose-the-optimizing-osr-body`
        // re-asked in five consecutive waves, each time by rebuilding the
        // binary with a print in it. Two numbers and the guards' own
        // `(bci, reason)` list answer it from a released binary: a non-zero
        // `call_exc_patches` is a CALL, a non-zero `deopt_stub_patches` is a
        // guard and the list names it.
        if false {
            let pts: Vec<String> = lowerer
                .deopt_boxes
                .iter()
                .map(|p| format!("{}@{:?}", p.bci, p.reason))
                .collect();
            eprintln!(
                "[ir-ls] osr entries SKIPPED (not sentinel-free): \
                 deopt_stub_patches={} call_exc_patches={} points={:?}",
                osr_nonfree_dbg.0, osr_nonfree_dbg.1, pts,
            );
        }
    }
    if osr_entries_wanted {
        let osr_live = match (residency_live.take(), one_liveness_pass.take()) {
            (Some(live), _) => live,
            // Wave 66 (lane L2): residency did not run, so the compile's one
            // liveness answer is still unused.
            (None, Some(pass)) => {
                let mut live = pass.into_live_model(graph, schedule);
                // OSR7-1 (lane osr8): as the residency model's.
                widen_live_model_for_pending(&mut live, &pending_live);
                live
            }
            (None, None) => {
                let mut live = crate::regalloc::build_live_model(graph, schedule);
                widen_live_model_for_pending(&mut live, &pending_live);
                live
            }
        };
        let non_opaque_entries = lowerer.emit_osr_entry_stubs(&osr_live);
        // JIT round 25 wave 3: no stub is being emitted any more.
        lowerer.snapshot_range_osr_bci.set(None);
        // Round 11 wave 14 (lane osrmerge): the relaxed fact holds only for a
        // body the door can enter nowhere but through an opaque merge.
        if non_opaque_entries == 0 && osr_guard_exit_bcis.is_none() {
            osr_guard_exit_bcis = osr_guard_exit_bcis_opaque.take();
        }
    }
    // JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-2): see
    // `osr_guard_fact_wants_opaque` above. Read nowhere but the narrowed OSR
    // door (`ir_tier`); asking it changes no byte of this body.
    let osr_guard_fact_needs_opaque_entries = osr_entries_wanted && osr_guard_fact_wants_opaque;
    drop(residency_live);

    let lowerer_osr_entries = std::mem::take(&mut lowerer.osr_entries);
    // Round 12 wave 8 (lane callcost): an elided self-call sample relies on a
    // dominating site's sample having run in the same activation. An OSR entry
    // enters past it, and a planned site that never reached the self-call
    // route emitted no sample of its own. Planned only for loop-free bodies,
    // which get no OSR entry, and every kind-4 node takes that route; refuse
    // rather than trust either if it ever fails.
    if lowerer.self_call_samples_elided > 0
        && (!lowerer_osr_entries.is_empty()
            || lowerer.self_call_sites_lowered != lowerer.self_call_sites_planned)
    {
        return refuse(Bailout::new(BailoutReason::Internal(
            "ir_lower: a dominated self-call sample was elided without its dominator",
        )));
    }
    let lowerer_osr_entry_kinds = std::mem::take(&mut lowerer.osr_entry_kinds);
    let lowerer_osr_opaque_entry_bcis = {
        let mut opaque = std::mem::take(&mut lowerer.osr_opaque_entry_bcis);
        opaque.sort_unstable();
        opaque.dedup();
        opaque.into_boxed_slice()
    };
    if osr_entries_wanted {
        // OD16-4 (round 16, lane osrdoor). Cast: a count of entries fits u64.
        note_osr_entry_census_n(OSR_ENTRY_CENSUS_EMITTED, lowerer_osr_entries.len() as u64);
    }
    if osr_entries_wanted && false {
        eprintln!(
            "[ir-ls] osr entries: emitted={} refused={:?}",
            lowerer_osr_entries.len(),
            lowerer.osr_refusals,
        );
    }
    // Fail closed: a guard emitted by a tail stub after `emit_deopt_pads` ran
    // would leave its `Jcc` pointing at the next instruction — a deopt that
    // silently never happens. No tail emitter guards today; this keeps it so.
    if !lowerer.deopt_pads.is_empty() {
        return refuse(Bailout::new(BailoutReason::Internal(
            "ir_lower: a deopt pad was requested after the pads were emitted",
        )));
    }
    // The same net for the reason-9 rethrow pads (round 11 wave 10): a pad
    // requested by a tail stub would leave its exceptional branch unpatched.
    if !lowerer.rethrow_pads.is_empty() {
        return refuse(Bailout::new(BailoutReason::Internal(
            "ir_lower: a rethrow pad was requested after the pads were emitted",
        )));
    }
    // And for the poll mode exits (wave 15): no poll is emitted after them.
    if !lowerer.poll_exit_pads.is_empty() {
        return refuse(Bailout::new(BailoutReason::Internal(
            "ir_lower: a poll exit pad was requested after the pads were emitted",
        )));
    }
    // Round 15 wave 1 (lane chain): the frame block's shared publication
    // subroutine, after every tail emitter that can lower a safepoint (each
    // `CALL` to it so far is patched here; a later one would call backwards).
    lowerer.emit_frame_block_publish_stub();
    lowerer.finish_lazy_thread_fetch();
    lowerer.patch_branches();
    // fib44-fix follow-up: patch direct self-recursive calls to the method entry.
    lowerer.patch_self_calls();
    // Every branch of the body is in place: the reloads no branch reaches.
    lowerer.apply_home_reload_forwards();
    // JIT round 25 item 1a: and no branch reaches a tail carry's read (latches;
    // the second latch read below refuses the attempt).
    lowerer.verify_tail_carry_points();
    // JIT rx27 wave 9: and no branch lands on a skipped read (latches; the
    // retry excludes the value).
    lowerer.verify_publish_reload_elides();
    if lowerer.publish_reload_elide_on && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] ir-publish-reload-elide {}: skipped={} refused={} excluded={}",
            lowerer.graph.scope_method_key,
            lowerer.publish_reload_elides.len(),
            lowerer.publish_reload_refused,
            lowerer.publish_reload_excluded.iter().filter(|e| **e).count(),
        );
    }
    if lowerer.operand_in_place_on && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] ir-operand-in-place {}: compares={} alu ops={}",
            lowerer.graph.scope_method_key,
            lowerer.operand_in_place_cmps,
            lowerer.operand_in_place_alus,
        );
    }
    if lowerer.reg_alloc_on && crate::x64::dbg_jitc_enabled() {
        if let Some(plan) = &lowerer.mir {
            let selected = plan.blocks.iter().flat_map(|block| &block.tiles)
                .filter(|tile| plan.allocated_instruction(tile.root).is_some() || plan.division(tile.root).is_some() || plan.materialized_cmp(tile.root).is_some())
                .count();
            eprintln!("[cratonvm-jitc] ir-allocated-selection {}: selected={} emitted={}",
                lowerer.graph.scope_method_key, selected, lowerer.mir_tiles);
        }
    }
    // W8-4: and no branch enters a site-named call or static read past its
    // register publish (latches; the retry excludes the value).
    lowerer.verify_reg_alloc_routed_defs();
    // JIT round 25 item 2, wave 3: every write the lazy clear counted on was
    // emitted (latches; refused below and retried without the plan).
    lowerer.verify_block_lazy_clear_writes();
    // JIT round 25 wave 6 (W6-D): a leaf frame emitted no GC point, call or
    // exit (latches; refused below and retried without the plan).
    lowerer.verify_leaf_entry();
    // Wave 4: the lazy reference homes' census line.
    lowerer.report_lazy_ref_homes();
    if crate::x64::dbg_jitc_enabled() {
        // Per body: definition home stores `store_rax` wrote, the ones the
        // existing drops skipped (residency publish, carried values), and the
        // ones this round's switch elided. Printed with the switch off too, so
        // both arms of an A/B carry the line.
        eprintln!(
            "[cratonvm-jitc] ir-home-stores: emitted={} dropped={} elided={} \
             tail_planned={} tail_refused={} exit_sites_crossed={} cmp_fused_unnamed={} \
             site_named={} excluded={} spills={} spills_deduped={} spill_bcis={} \
             prefix_kept={} backedge_skipped={} prefix_proven={} cmp_unnamed_refused={:?} elide={} body={}",
            lowerer.def_home_stores,
            lowerer.homes_dropped_at_def + lowerer.carry_stores_dropped,
            lowerer.tail_stores_elided,
            lowerer.tail_carries_planned,
            lowerer.tail_carries_refused,
            lowerer.tail_carry_site_ends.len(),
            lowerer.fused_cmp_unnamed_count,
            lowerer.site_named_count,
            lowerer.elide_excluded.iter().filter(|e| **e).count(),
            lowerer.inspection_spills_emitted,
            lowerer.inspection_spills_deduped,
            lowerer.site_spill_bcis.len(),
            lowerer.tail_prefix_kept,
            lowerer.tail_backedge_skipped,
            lowerer.prefix_rax_proofs.len(),
            lowerer.cmp_unnamed_refused,
            if lowerer.elide_unnamed_homes_on { "on" } else { "off" },
            if graph.scope_method_key.is_empty() { "?" } else { graph.scope_method_key.as_str() },
        );
        // JIT round 25 wave 6 (lane W6-B): why the emitted home stores were
        // kept, `reason=static/frequency-weighted`, per body and attempt.
        eprintln!(
            "[cratonvm-jitc] ir-home-why method={} {} phi_edge={}/{:.0} \
             slow_spills={} site_named_phis={} loop={}",
            lowerer.graph.scope_method_key,
            lowerer.home_why_fields(),
            lowerer.phi_edge_home_stores,
            lowerer.phi_edge_home_weighted,
            lowerer.poll_slow_spills_emitted,
            lowerer.site_named_phis,
            if lowerer.site_named_loop_on { "on" } else { "off" },
        );
        // JIT round 25 wave 7 (lane W7-A): the register-allocator census.
        eprintln!("{}", lowerer.reg_alloc_census_line());
        let keep: Vec<String> = HOME_KEEP_NAMES
            .iter()
            .zip(lowerer.home_keep_census.iter())
            .zip(lowerer.home_keep_freq.iter())
            .map(|((name, n), f)| format!("{name}={n}/{f:.0}"))
            .collect();
        eprintln!("[cratonvm-jitc] ir-home-keep: {}", keep.join(" "));
        let mut shapes: Vec<(&String, &(usize, f64, NodeId))> =
            lowerer.home_keep_shapes.iter().collect();
        shapes.sort_by(|a, b| b.1 .1.partial_cmp(&a.1 .1).unwrap_or(std::cmp::Ordering::Equal));
        for (k, (n, f, ex)) in shapes.iter().take(12) {
            eprintln!(
                "[cratonvm-jitc] ir-home-keep-shape: {k} n={n} f={f:.0} ex=n{ex} body={}",
                graph.scope_method_key
            );
        }
    }
    #[cfg(test)]
    IR_CENSUS_THIS_THREAD.with(|c| {
        let mut v = c.get();
        v[4] += lowerer.tail_carries_planned as u64;
        v[5] += lowerer.tail_stores_elided as u64;
        v[6] += lowerer.tail_carries_refused as u64;
        v[7] += lowerer.fused_cmp_unnamed_count as u64;
        v[8] += lowerer.site_named_count as u64;
        v[9] += lowerer.inspection_spills_emitted as u64;
        c.set(v);
    });

    // real-frame-deopt (step 2): resolve recorded safepoint snapshots into
    // native-offset-keyed DeoptimizationPoints before the buffer is consumed.
    // Emit-and-discard: nothing reads these yet, so codegen is unchanged.
    let mut deopt_points = lowerer.build_deopt_points(osr_sentinel_free);
    // j27 wave 4 (lane IRROWS, IRLOWER17-3): count the anchors that stayed
    // undominated; `CRATONVM_JIT_IR_REFUSE_UNDOMINATED_ANCHOR=1` latches a
    // refusal, read by the second latch read below.
    lowerer.check_undominated_anchors(
        crate::x64::dbg_jitc_enabled()
            || false,
    );
    // The reason-9 points the rethrow pads publish, by value beside the by-bci
    // resume points — the single-pass layout, where the OSR exception transfer
    // finds them (`osr_exit::exceptional_reason_at_bci`) and the by-bci resume
    // lookups skip them (`deopt_reason_at_bci`). Each is anchored at its
    // site's exceptional branch, which no by-bci point shares (that is never
    // an instruction start); re-sorted so the verifier's order check holds.
    // Empty off the precise-frame route (round 11 wave 10, lane irexc).
    if !lowerer.rethrow_points.is_empty() {
        deopt_points.append(&mut lowerer.rethrow_points);
        deopt_points.sort_by_key(|p| p.native_offset);
    }
    // The SECOND read of the latch (round 9 wave 2). Everything after the first
    // read — the deopt pads, outlined polls, tail stubs, OSR entry stubs and,
    // above all, `build_deopt_points` — can latch too, and nothing read it
    // again, so a by-bci point whose dropped home no register describes became
    // a silent `Unsupported` (unresumable at run time) instead of a refused
    // compile. `build_deopt_points` quiets the one latch that is routine and
    // harmless (points at bcis nothing transfers from), so what reaches here
    // is a real disagreement.
    if let Some(bailout) = lowerer.take_latched_bailout() {
        describe_unallocated_value(graph, &bailout);
        return refuse(bailout);
    }
    // The unroller's net. It relaxed its safepoint refusal on the strength of
    // the GRAPH being trap-free; that is only sound while the points it
    // orphaned are never built. If they were built, the killed body nodes now
    // resolve to `FrameValue::Undefined` and a deopt would silently lose
    // interpreter locals -- so refuse the compile and let the single-pass
    // backend take the method, which is what happened before the relaxation.
    //
    // Read ONCE, at the TOP of `lower_inner_with_array_list`
    // (`unroll_used_unreachable_frames`, passed to every attempt): `take_`
    // clears it, and taking it here — after a dozen earlier refusals that
    // `return` first — left a refused compile's flag set on the thread for the
    // NEXT compile to inherit (round 9).
    if unroll_used_unreachable_frames && !deopt_points.is_empty() {
        return refuse(Bailout::with_context(
            BailoutReason::UnsupportedShape(
                "ir_lower: the unroller relied on unreachable frame states and points were built",
            ),
            format!(
                "{} deopt point(s) built for a graph the unroller treated as trap-free",
                deopt_points.len()
            ),
        ));
    }
    // Round nio20 wave 5 (lane L3): the pure-body unroll's net. It left the
    // frames at these bcis naming killed body nodes on the prediction that
    // nothing in the kept graph transfers at them
    // (`ir_optimize::pure_body_frames_unconsultable`); the emission's record of
    // where it CAN transfer is the check of that prediction. A disagreement
    // refuses the compile rather than publish a resume with `Undefined` locals.
    // JIT r20 wave 19 (LOOPOPT20-6, `CRATONVM_JIT_IR_UNROLL_NAMED_FRAME_PCS`):
    // only a transfer that reads the stranded FLAT frame counts; one through
    // its node's own named frame (a loop-entry predicate at the header bci)
    // never reads it. `=0`: every transfer, as before.
    let named_frame_pcs = crate::ir_optimize::unroll_named_frame_pcs_enabled();
    if let Some(&bci) = unroll_stranded_bcis.flat.iter().find(|&b| {
        if named_frame_pcs {
            lowerer.by_bci_transfer_bcis.contains(b)
        } else {
            lowerer.transfer_bcis.contains(b)
        }
    }) {
        return refuse(Bailout::with_context(
            BailoutReason::UnsupportedShape(
                "ir_lower: a pure-body unroll stranded a frame the emission transfers at",
            ),
            format!("bci {bci} is a transfer bci but its frame names an unrolled body node"),
        ));
    }
    // JIT r22 LOOPS22-2: the same net for a retired SPLICED loop, whose
    // stranded frames are chain snapshots keyed by their combined pc
    // (`loop_retire::frames_after_retire`). A site that published one of them
    // would resume the callee with a loop value the rewrite killed.
    if let Some(&pc) = unroll_stranded_bcis
        .chain
        .iter()
        .find(|&p| lowerer.chain_transfer_pcs.contains(p))
    {
        return refuse(Bailout::with_context(
            BailoutReason::UnsupportedShape(
                "ir_lower: a retired spliced loop stranded a chain frame the emission publishes",
            ),
            format!("combined pc {pc} publishes a chain snapshot naming a retired loop value"),
        ));
    }
    let mut deopt_boxes = std::mem::take(&mut lowerer.deopt_boxes);
    // JIT round 19 wave 5 (lane handler5, SPLICE4-1): the probes' sites, owned
    // by the artifact like the single-pass tier's local-handler sites.
    let catch_site_boxes = std::mem::take(&mut lowerer.catch_site_boxes);
    let trap_cause_points = std::mem::take(&mut lowerer.trap_cause_points);
    // Round 13 wave 11 (lane chain5, CH-1): baked like the boxes, owned like them.
    let forced_chain_deopt_counters = std::mem::take(&mut lowerer.forced_chain_deopt_counters);
    // Round 15 wave 4 (lane chain4): and the point each cell belongs to.
    let forced_chain_deopt_points = std::mem::take(&mut lowerer.forced_chain_deopt_points);
    // JIT round 21 lane stress (FUZZ21-1 / FUZZ21-7): baked like the chain
    // cells, owned like them.
    let poll_stress_cells = std::mem::take(&mut lowerer.poll_stress_cells);
    // Read for the monitor-block vouch at the end (wave 19, lane L2).
    let poll_exits_emitted = !lowerer.poll_exit_boxes.is_empty();
    // nio21 wave 4 L3: which exits this body carries, per compile, so a host
    // A/B of `CRATONVM_JIT_IR_BACK_EDGE_VIRTUAL_OBJECT_EXITS` /
    // `CRATONVM_JIT_IR_EXITS_BESIDE_DROPPED_SNAPSHOT_VALUES` can name the
    // methods an arm changes (`r21-buffer-relative-le-be-rows-...` page). The
    // exits' code is in the polls' outlined slow blocks and the 5-byte
    // post-call `NOP`s, so an arm changes a body's fast path only through
    // those `NOP`s; the line says how many there are.
    if poll_exits_emitted && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] ir-exit-cost-census method={} mode={:?} poll_exits={} \
             naming_virtuals={} spliced_header={} post_call_sites={} beside_dropped_values={}",
            graph.scope_method_key,
            lowerer.poll_mode_exits,
            lowerer.mode_exit_census[0],
            lowerer.poll_exits_naming_virtuals,
            lowerer.spliced_header_poll_exits,
            lowerer.post_call_exit_sites.len(),
            lowerer.optimizer_dropped_snapshot_values,
        );
    }
    // Interpreter round i2 wave 53, lane L2: every box a named-only poll exit
    // (no guard or rethrow pad among the boxes so far, and the mode puts no
    // post-call exit), which the VM's caller-held binding reads.
    let named_poll_exits_only = poll_exits_emitted
        && deopt_boxes.is_empty()
        && lowerer.post_call_exit_sites.is_empty()
        && lowerer.poll_mode_exits == IrPollModeExits::MethodEntryNamedOnly;
    // Interpreter round i2 wave 57, lane L2: a poll exit leaves through a
    // caller chain, which the VM's verdict checks before telling it to leave.
    let chain_poll_exits = lowerer.spliced_header_poll_exits > 0;
    // The poll mode exits' points (wave 15), kept out of the guard facts above
    // and owned by the artifact from here like every other box.
    deopt_boxes.append(&mut lowerer.poll_exit_boxes);
    // Round 15 wave 1 (lane sync, SS8-2): a self-locking body is published
    // only with every exit closed (`Lowerer::self_lock_refusal`); anything
    // else falls back to the single-pass tier, which never leaks the hold.
    if let Some((cause, why)) = lowerer.self_lock_refusal(&deopt_boxes, &deopt_points) {
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir self-lock REFUSED ({}): {why}",
                cause.name()
            );
        }
        // Round 15 wave 3 (lane sync2, S15-2's first half): on the
        // synchronized-splice census line, beside the builds.
        crate::ir::note_sync_splice_census(crate::ir::SYNC_CENSUS_IR_SELF_LOCK_REFUSED);
        // Round 15 wave 4 (lane sync3, SY2-6): and by cause.
        crate::ir::note_sync_splice_census(cause.census_row());
        note_lower_bail("ir-self-lock-exits-not-closed");
        return refuse(Bailout::with_context(
            BailoutReason::UnsupportedShape("ir self-locking body: an exit is not closed"),
            why,
        ));
    }
    let self_locks_monitor = lowerer.self_lock.is_some();
    if self_locks_monitor {
        crate::ir::note_sync_splice_census(crate::ir::SYNC_CENSUS_IR_SELF_LOCK_CLOSED);
    }
    // JIT round 19 proposal wave 6, lane sync6 (SYNCDOOR618-5): the hold's
    // kind for the VM's locked-monitor depths; this tier publishes no frame
    // word for it.
    let self_lock_hold = self_locks_monitor.then(|| {
        if lowerer.self_lock_mirror_slot().is_some() {
            crate::SelfLockHold::Static {
                mirror_word_rbp_off: None,
            }
        } else {
            // JIT round 19 proposal wave 8, lane gc8 (GC719-1): the receiver's
            // prologue home `[rbp - 8]`, which every safepoint map republishes
            // (`Lowerer::self_lock_obj_home`; metadata only, the VM reads it
            // only where the active map names it).
            crate::SelfLockHold::Instance {
                obj_rbp_off: lowerer.self_lock_obj_home(),
            }
        }
    });
    // Gap B: a method containing an `Op::Call` takes the VM context pointer as a
    // hidden first arg, so it must be invoked via `try_call_with_context`.
    let needs_context = lowerer.needs_context;

    // Relocation-contract values, read off the lowerer before `buf` moves out.
    let sp_id_slot_off = lowerer.sp_id_slot_off;
    let shadow_thread_slot_off = lowerer.shadow_thread_slot_off;
    let shadow_savebase_slot_off = lowerer.shadow_savebase_slot_off;
    // Whether any safepoint of this body can publish the frame block. Read by
    // the collector's frame walk, which leaves a published block's homes to the
    // shadow-stack remap (`CompiledMethod::shadow_frame_block`).
    let shadow_frame_block = !lowerer.frame_block.is_empty() && lowerer.shadow_pushed_any;
    let oop_maps = std::mem::take(&mut lowerer.oop_maps);
    // gen r5w3/live7: empty unless `CRATONVM_JIT_PRECISE_FRAME_LIVENESS`.
    // GC719-1 (round 19 wave 8, lane gc8): the blocks held per safepoint id.
    let ir_monitor_homes = std::mem::take(&mut lowerer.ir_monitor_homes);
    let frame_liveness = lowerer.take_frame_liveness();
    let sp_id_bcis = std::mem::take(&mut lowerer.sp_id_bcis);
    // The reference-free frame contract (round 11 wave 14, lane fib): the same
    // prediction every row was recorded under, read before the lowerer goes.
    let body_keeps_no_references = lowerer.body_publishes_no_roots();
    // Round 11 wave 15 (lane fib): a direct self-call that deferred its id
    // store relied on the frame being published `frame_holds_no_references`
    // below, the one contract under which no collector reads that slot. The
    // prediction it was made under is `body_keeps_no_references` itself; the
    // map clause is re-checked here because a map naming a slot would clear
    // the flag, and a stale id is then trusted. Cannot fire while
    // `emit_safepoint_map` builds its slots from exactly the two lists that
    // prediction reads; refuse rather than publish if it ever does.
    if lowerer.self_call_id_stores_deferred > 0
        && !(body_keeps_no_references && oop_maps.iter().all(|m| m.frame_slot_offsets.is_empty()))
    {
        return refuse(Bailout::new(BailoutReason::Internal(
            "ir_lower: a self-call deferred its safepoint-id store in a frame that keeps references",
        )));
    }
    // Round 11 wave 16 (lane irback): the same rule for the lean prologue
    // (`Lowerer::lean_ref_free_prologue`), which left the unset locals and the
    // reserved tail unzeroed on the promise that the collector will neither
    // verify nor scan this band. Cannot fire for the reason above.
    if lowerer.lean_ref_free_prologue() && !oop_maps.iter().all(|m| m.frame_slot_offsets.is_empty())
    {
        return refuse(Bailout::new(BailoutReason::Internal(
            "ir_lower: a lean prologue skipped GC zeroing in a frame whose maps name a slot",
        )));
    }
    let return_site_rows = std::mem::take(&mut lowerer.return_site_rows);
    // Wave 25 (lane L6): the polls a redefinition that withdraws this body
    // forces, and the flag they test.
    let exit_poll_sites = std::mem::take(&mut lowerer.exit_poll_sites);
    // j27 wave 5 (lane UNROLLRES): the page-form polls' fault sites
    // (`ir_page_poll`), registered once the body has its address.
    let page_poll_sites = std::mem::take(&mut lowerer.page_poll_sites);
    // Wave 28 (lane L6): and the post-call exit sites it forces with them.
    let post_call_exit_sites = std::mem::take(&mut lowerer.post_call_exit_sites);
    let exit_poll_flag = lowerer.safepoint_flag_addr;
    let inline_frame_rows = std::mem::take(&mut lowerer.inline_frame_rows);
    lowerer.report_inline_row_census(&inline_frame_rows);
    let inline_frame_sp_id_rows = std::mem::take(&mut lowerer.inline_frame_sp_id_rows);
    let npe_trap_sites = std::mem::take(&mut lowerer.npe_trap_sites);
    let locals_size = lowerer.locals_size;
    let first_spill = lowerer.first_spill;
    let spill_cap_off = lowerer.spill_cap_off;
    let saved_xmm_bytes = lowerer.saved_xmm_bytes;
    // gen r5w6/oomjit10: read before `buf` moves out of the lowerer; see
    // `CompiledMethod::ir_saved_gpr_slots`.
    let ir_saved_gpr_slots: Vec<(u8, i32)> = lowerer.saved_gpr_regs().collect();
    let frame_size = lowerer.frame_size;
    // Round 13 wave 3 (lane framestate): how many guards inside spliced bodies
    // publish a caller chain, and how many fell back to the flat frame at the
    // outermost `invoke` (`CRATONVM_JIT_IR_SPLICE_FRAME_STATES`).
    if (lowerer.splice_chain_points > 0
        || lowerer.splice_chain_fallbacks > 0
        || lowerer.sync_window_flat_sites > 0)
        && crate::ir_stage_reporting()
    {
        eprintln!(
            "[ir] splice-frame-states: {} guard(s) publish a caller chain, {} fell back to the \
             outermost invoke's frame, {} inside a synchronized splice window kept it",
            lowerer.splice_chain_points,
            lowerer.splice_chain_fallbacks,
            lowerer.sync_window_flat_sites,
        );
    }
    // Interpreter round i1 wave 47, lane L3: post-call exits after a call
    // inside a splice, whose frame is a caller chain
    // (`CRATONVM_JIT_IR_POST_CALL_SPLICE_EXITS`); the positive control of
    // `L3W47PostCallExitInsideASplice`.
    if (lowerer.post_call_chain_sites > 0 || !lowerer.post_call_splice_state_by_call.is_empty())
        && crate::ir_stage_reporting()
    {
        eprintln!(
            "[ir] post-call-chain-sites: {} of {} spliced call(s) with a post-call chain snapshot",
            lowerer.post_call_chain_sites,
            lowerer.post_call_splice_state_by_call.len(),
        );
    }

    // ── Install-time deopt-metadata verification ─────────────────────
    //
    // The metadata is checked BEFORE the artifact becomes a `CompiledMethod`,
    // because a frame that cannot be reconstructed is only discoverable at
    // runtime otherwise — at a guard, on a VM thread, with the frame already
    // half torn down. Bailing here is always semantically valid (the method
    // takes the single-pass backend or the interpreter); installing code whose
    // deopt map disagrees with its oop map is not.
    //
    // Which lanes are active is decided by which data this backend actually
    // has, so every check that fires is one this file can be held to:
    //
    //   * **scope** — `resolve_frame_state` records `method_key: String::new()`
    //     (the lowerer does not know the method's identity; deopt resume keys on
    //     the running `CompiledMethod`), so the limits are registered under that
    //     same empty key. `code_len`/`max_locals`/`max_stack` are saturated: the
    //     bytecode length and `max_stack` never reach this function, and
    //     `num_locals` is not a bound a hand-built graph's snapshots respect.
    //     What the lane does buy is the `UnknownMethod` check — the day inlined
    //     caller scopes start carrying real method keys, they must arrive with
    //     limits rather than silently skipping every per-scope check.
    //   * **removed-node** — every `Op::Dead` id, cross-checked against the
    //     scalar-replacement map's keys as the ones a `VirtualObject` recipe is
    //     allowed to name. This is the lane that catches a recipe built from a
    //     stale graph.
    //   * **oop-map agreement** — one `OopCoverage` per emitted `OopMapEntry`,
    //     keyed by native offset. Polls, allocation stubs and implicit
    //     exception pads record their exact call returns.
    //     For sites with no native anchor, relocation still looks up the map
    //     by the safepoint ID in the frame. Register only the exact anchors
    //     with this verifier.
    //   * **structural** — always on.
    let mut verifier = DeoptVerifier::new()
        .with_method(MethodFrameLimits::new(
            String::new(),
            u32::MAX,
            u16::MAX,
            u16::MAX,
        ))
        .with_removed_nodes(
            graph
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| matches!(n.op, Op::Dead))
                .map(|(id, _)| id as u32),
        );
    if let Some(sr) = sr_map {
        verifier = verifier.with_materializable_nodes(sr.objects.keys().copied());
    }
    // Round 13 wave 3 (lane framestate): a guard in a spliced body may carry a
    // caller chain whose scopes name REAL method keys -- the spliced callees and,
    // outermost, the compiling method (`Graph::scope_method_key`). Each arrives
    // with its limits, so the scope lane checks every level rather than
    // reporting `UnknownMethod`. A callee's `code_len` / `max_locals` come from
    // its splice site; the compiling method's are saturated like the empty
    // key's above (a self-recursive copy registers the same key again with
    // its real limits, which then hold for both).
    if !graph.scope_method_key.is_empty() && graph.safepoints.iter().any(|s| s.is_splice_state()) {
        verifier = verifier.with_method(MethodFrameLimits::new(
            graph.scope_method_key.clone(),
            u32::MAX,
            u16::MAX,
            u16::MAX,
        ));
        let mut registered: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for chain in graph
            .safepoints
            .iter()
            .filter_map(|s| s.splice_scope.as_deref())
        {
            for scope in &chain.scopes {
                if scope.method_key.is_empty() || !registered.insert(scope.method_key.as_str()) {
                    continue;
                }
                let code_len = if scope.code_len == 0 {
                    u32::MAX
                } else {
                    scope.code_len
                };
                let max_locals = u16::try_from(scope.max_locals)
                    .ok()
                    .filter(|&m| m != 0)
                    .unwrap_or(u16::MAX);
                verifier = verifier.with_method(MethodFrameLimits::new(
                    scope.method_key.clone(),
                    code_len,
                    max_locals,
                    u16::MAX,
                ));
            }
        }
    }
    for entry in &oop_maps {
        if entry.native_pc_offset != 0 {
            let mut covered_regs = Vec::new();
            if let Some(mask) = entry.reg_oop_mask {
                for &reg in &crate::x64::ALL_SPILL_GPRS {
                    if mask & crate::x64::reg_oop_mask_bit(reg) != 0 {
                        covered_regs.push(reg);
                    }
                }
            }
            verifier = verifier.with_oop_map(
                entry.native_pc_offset,
                OopCoverage {
                    frame_slot_offsets: entry.frame_slot_offsets.clone(),
                    registers: covered_regs,
                    moving_young_coverage_complete: entry.moving_young_coverage_complete,
                },
            );
        }
    }
    // Both sets are checked. `deopt_points` is the sorted, native-offset-keyed
    // list the verifier's structural lane checks the order of; `deopt_boxes` are the boxes baked
    // into the emitted guards, and those are the frame states a *live* deopt
    // actually reconstructs from. Each box is verified on its own — they are not
    // one sorted sequence, so checking them as a slice would report a bogus
    // ordering violation.
    for point in &deopt_boxes {
        if let Err(bailout) = verifier.verify(std::slice::from_ref(&**point)) {
            return refuse(bailout);
        }
    }
    if let Err(bailout) = verifier.verify(&deopt_points) {
        return refuse(bailout);
    }

    // The liveness colouring's peak — the number of values simultaneously live
    // at the busiest program point — is exactly what
    // `CompilationReport::peak_live_values` asks for, and this is the only place
    // in the compiler that computes it. Recorded through the thread-local hook
    // rather than a parameter because `lower_inner`'s signature is pinned (see
    // `metrics::note_current_peak_live_values`); a no-op when metrics are off.
    crate::metrics::note_current_peak_live_values(slot_plan.peak_live);

    // `CompilationReport::spills` / `::reloads` are reported ONLY when the
    // linear-scan path actually ran, and they count what this backend EMITTED,
    // not what `regalloc::Allocation` planned — the plan is a full
    // spill/reload/split schedule, and this wiring executes only the subset it
    // can prove (see `plan_register_residency`). Reporting the plan's numbers
    // would describe machine code that was never generated.
    //
    // The colourer-only path leaves both `NotMeasured`, which is the truth: it
    // allocates no registers, so it has no register↔memory transitions to
    // count, and a reported `0` would be indistinguishable from "measured, and
    // it spilled nothing".
    if ls_active {
        crate::metrics::note_current_spills(lowerer.ls_spills);
        crate::metrics::note_current_reloads(lowerer.ls_reloads);
    }
    // JIT round 25 (item 1b): per body, how many GP values are resident and in
    // which half of the file, and how many operand reads still went to a home
    // word. Printed with `CRATONVM_JIT_IR_LS_CALLER_SAVED` on AND off, so an
    // A/B of that flag reads "resident before vs now" and "reloads before vs
    // now" method by method. `gp_home_reloads` counts `gp_load_value` reads
    // only (the accumulator / RCX operand path); in-place forms that read a
    // frame word directly (`CMP r, [rbp-x]`, ...) are not in it.
    if crate::x64::dbg_jitc_enabled() {
        let (mut callee, mut caller) = (0usize, 0usize);
        for reg in lowerer.gp_reg_of.iter().flatten() {
            if IR_LS_CALLER_SAVED_GPRS.contains(reg) {
                caller += 1;
            } else {
                callee += 1;
            }
        }
        eprintln!(
            "[cratonvm-jitc] ir-ls-gp method={} caller_saved_flag={} resident={} \
             (callee_saved={callee} caller_saved={caller} planned_caller_saved={}) \
             gp_reg_reads={} gp_home_reloads={} (ref={} assigned_not_live={} unassigned={}) \
             in_place: alu_reg={} lea={} cmp_reg={} cmp_frame={} | cs_republish={} \
             cs_republish_skipped={}",
            graph.scope_method_key,
            ir_ls_caller_saved_enabled(),
            callee + caller,
            lowerer.ls_caller_saved_values,
            lowerer.gp_reg_reads,
            lowerer.gp_home_reloads,
            lowerer.gp_home_reloads_ref,
            lowerer.gp_home_reloads_assigned,
            lowerer.gp_home_reloads_unassigned,
            lowerer.alu_reg_operands,
            lowerer.add_lea,
            lowerer.cmp_in_place,
            lowerer.cmp_in_place_frame + lowerer.cmp_imm_frame,
            lowerer.cs_republishes,
            lowerer.cs_republish_skipped,
        );
    }

    // ENGAGEMENT, not effect. Both numbers answer "did the wiring fire on this
    // method", which is the question a null timing result cannot answer on its
    // own -- and this session spent three A/B arms on `CratonBench` before
    // noticing that the optimizing tier plans no residency there at all, so
    // both arms had been running identical machine code.
    if ls_active
        && false
        && (lowerer.phi_copy_reg_reads > 0
            || lowerer.phi_copy_reg_publishes > 0
            || lowerer.phi_copy_publish_deferred > 0
            || lowerer.phi_home_publish_skipped > 0
            || lowerer.phi_home_publish_refused > 0)
    {
        // `home_publish_skipped` is the engagement census for the dropped-home
        // publish guard, and `home_publish_refused` the compiles it turned into
        // an interpreter fallback rather than a read of an unwritten word.
        eprintln!(
            "[ir-ls] phi copies: reg_reads={} reg_publishes={} publish_deferred={} home_publish_skipped={} home_publish_refused={}",
            lowerer.phi_copy_reg_reads,
            lowerer.phi_copy_reg_publishes,
            lowerer.phi_copy_publish_deferred,
            lowerer.phi_home_publish_skipped,
            lowerer.phi_home_publish_refused,
        );
    }
    // `CRATONVM_JIT_IR_LOOP_CARRIED_FP_REGS`: its own line, for the same reason.
    if ls_active
        && (lowerer.fp_phi_reg_publishes > 0
            || lowerer.fp_phi_reg_stores > 0
            || lowerer.fp_in_place_ops > 0)
        && false
    {
        eprintln!(
            "[ir-ls] fp loop-carried: phi_publishes={} phi_home_stores={} in_place_ops={}",
            lowerer.fp_phi_reg_publishes, lowerer.fp_phi_reg_stores, lowerer.fp_in_place_ops
        );
    }
    // Round 16 wave 3 (lane codegen, CG16-7): its own line, so the one above
    // keeps its format.
    if lowerer.phi_copy_const_imms > 0
        && false
    {
        eprintln!(
            "[ir-ls] phi copies: const_imms={}",
            lowerer.phi_copy_const_imms
        );
    }
    if ls_active
        && ir_deopt_regs_enabled()
        && false
    {
        eprintln!(
            "[ir-ls] deopt regs: nameable={} frame_slots_named_by_register={} regs_base={}",
            lowerer.deopt_nameable.iter().filter(|n| **n).count(),
            lowerer.deopt_reg_named.get(),
            lowerer.deopt_regs_base,
        );
        eprintln!(
            "[ir-ls] homes: dropped_values={} stores_skipped={} read_refusals={} \
             def_publishes={} def_stores_skipped={}",
            lowerer.home_dropped.iter().filter(|d| **d).count(),
            lowerer.home_stores_dropped,
            lowerer.home_read_refusals.get(),
            lowerer.reg_publishes_at_def,
            lowerer.homes_dropped_at_def,
        );
        // The counterpart to `dropped_values`, and the line to read when it is
        // low: of the values this compile PROMOTED, why each one still writes
        // its home. See `Lowerer::census_home_blocks`.
        let c = lowerer.census_home_blocks();
        eprintln!(
            "[ir-ls] homes kept: switch={} deopt={} type={} op={}",
            c[0], c[1], c[2], c[3],
        );
        // What the split plan actually FIRED, as opposed to what it planned.
        // The two are printed from different places on purpose: the plan's line
        // (`[ir-ls] splits:`) is what `plan_register_residency` decided, and
        // this is what the emission did with it. A plan with transitions and an
        // emission with none is the shape every stale diagnostic in this file
        // has taken, and one number cannot show it.
        let e = lowerer.ls_split_emitted;
        eprintln!(
            "[ir-ls] splits emitted: load={} remat={} move={} drop={} edge_resolution: edges={} loads={}",
            e[0],
            e[1],
            e[2],
            e[3],
            lowerer.ls_edge_copies.len(),
            lowerer.ls_edge_loads,
        );
    }
    // The accounting identity inside `census_home_blocks` is the thing that
    // keeps this census tied to `home_dropped`, and a check that only runs
    // under a diagnostic env var is a check no test performs — which is the
    // shape of the failure it exists to prevent. So a debug build pays for one
    // extra pass and gets the assertion on every lowering, diagnostics or not.
    #[cfg(debug_assertions)]
    {
        let _ = lowerer.census_home_blocks();
    }
    // Unconditional, and cheap: four relaxed adds per compiled method. The
    // per-method lines below are behind a diagnostic flag; this total is what a
    // probe-set census reads, and a census you have to switch on is one nobody
    // has for the run they already did.
    note_deferred_census(
        lowerer.deferred_candidates,
        lowerer.carry_deferred_planned,
        lowerer.deferred_skips[7],
        lowerer.deferred_mid_foldable,
    );
    if false {
        eprintln!(
            "[ir-ls] carries: planned={} taken={} read={} refused={} \
             stores_dropped={} still_deopt_named={} deferred={}/{} cmp_in_place={}+{} \
             cmp_imm={}+{}",
            lowerer.carry_of.iter().filter(|c| c.is_some()).count(),
            lowerer.carries_taken,
            lowerer.carries_read,
            lowerer.carries_refused,
            lowerer.carry_stores_dropped,
            lowerer.carry_named,
            lowerer.carry_deferred_planned,
            lowerer.carry_deferred_read,
            lowerer.cmp_in_place,
            lowerer.cmp_in_place_frame,
            lowerer.cmp_imm,
            lowerer.cmp_imm_frame,
        );
        let s = lowerer.carry_skips;
        eprintln!(
            "[ir-ls] carry skips: multi_use={} wrong_type={} producer_arm={} \
             already_resident={} consumer_arm={} operand_position={}",
            s[0], s[1], s[2], s[3], s[4], s[5],
        );
        let d = lowerer.deferred_skips;
        eprintln!(
            "[ir-ls] deferred candidates={} taken={} | declined: \
             prod_carrying={} prod_multi_use={} prod_type_or_arm={} \
             prod_resident={} operand_position={} consumer={} \
             mid_writes_rcx={} (of which foldable={}) | non_candidate_windows={}",
            lowerer.deferred_candidates,
            lowerer.carry_deferred_planned,
            d[1],
            d[2],
            d[3],
            d[4],
            d[5],
            d[6],
            d[7],
            lowerer.deferred_mid_foldable,
            d[0],
        );
        eprintln!(
            "[ir-ls] alu immediates folded: {} add_lea={}+{} rcx_twins={}",
            lowerer.alu_imms_folded, lowerer.add_lea, lowerer.add_lea_acc, lowerer.rcx_twins,
        );
        // Wave 17: which unconditional back-edge polls of an OSR-door compile
        // can pull the loop back into the interpreter, and why the rest
        // cannot (`Lowerer::back_edge_mode_exit_state`). Wave 18: the same for
        // a method-entry compile, on a line of its own.
        let tier = match lowerer.poll_mode_exits {
            IrPollModeExits::Off => None,
            IrPollModeExits::OsrDoor => Some(""),
            IrPollModeExits::MethodEntry => Some("entry "),
            // Wave 53 (lane L2): the positive control of the named-only
            // exits; counted on the method-entry census row.
            IrPollModeExits::MethodEntryNamedOnly => Some("entry named-only "),
        };
        if let Some(tier) = tier {
            let m = lowerer.mode_exit_census;
            let refused: Vec<String> = ModeExitRefusal::CAUSE_NAMES
                .iter()
                .zip(&m[1..])
                .map(|(name, n)| format!("{name}={n}"))
                .collect();
            eprintln!(
                "[ir-ls] {tier}poll mode exits: given={} | refused: {}",
                m[0],
                refused.join(" ")
            );
        }
        // The SCHEDULER's half of the same question. `carry skips:
        // operand_position` above says how often the emitter wanted a triple
        // the scheduler had not formed; this says why it had not.
        let pc = schedule.pairing;
        eprintln!(
            "[ir-ls] operand pairing: candidates={} paired={} | declined: \
             multi_use={} producer_arm={} other_block={} after_consumer={} \
             already_adjacent={} deopt_between={} memory_order={} no_node={} too_far={}",
            pc.candidates,
            pc.paired,
            pc.multi_use,
            pc.producer_arm,
            pc.other_block,
            pc.after_consumer,
            pc.already_adjacent,
            pc.deopt_between,
            // `PairCensus` gained this bucket when the scheduler's step 5b
            // became `check_memory_order`'s business, and this printer
            // enumerates the fields by hand -- so it silently omitted the new
            // one, and the LINE stopped adding up while the identity
            // `PairCensus::closes` asserts went on holding. A census that reads
            // low retires levers that are engaged. See `NOTES-regalloc.md` S1.
            pc.memory_order,
            pc.no_node,
            pc.too_far,
        );
        eprintln!(
            "[ir-ls] carried homes dropped: {}",
            lowerer.carried_homes_dropped,
        );
        eprintln!(
            "[ir-ls] unreachable frame states: graph_trap_free={} homes_freed={} points_skipped={}",
            lowerer.graph_cannot_deopt(),
            lowerer.unreachable_homes,
            lowerer.unreachable_points_skipped.get(),
        );
    }

    // Carry the identity the prologue encoded, so publication can bind it to
    // the artifact this buffer becomes. It stays a reservation until the
    // artifact exists: the overflow refusal below drops it, releasing the id.
    // Read before `lowerer.compile_id` / `lowerer.buf` are moved out below
    // (wave 11, frames2).
    let deopt_gpr_image = lowerer.deopt_gpr_image();
    let deopt_xmm_image = lowerer.deopt_xmm_image();
    // JIT round 25 (item 3, `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`): the
    // per-body claim, under the same once-per-compile answer the fresh-frame
    // clear left these words out under (`Lowerer::prim_colour_claim`), so a
    // word the clear skipped is always a word the collector is told about.
    // JIT round 25 wave 2: the whole per-body list, through the one function
    // the clear's skip also asks (`Lowerer::body_claim_offsets_with`); with the
    // wave-2 switches off it is exactly `prim_slot_offsets`.
    let ir_prim_colour_claim: Box<[i16]> = if lowerer.prim_colour_claim {
        lowerer.body_claim_offsets_with(true).into_boxed_slice()
    } else {
        Box::new([])
    };
    let ir_prim_colour_clear_bytes = lowerer.prim_colour_clear_bytes;
    // JIT round 25 wave 2: the `ir-frame-anatomy` line's text (`lib.rs`
    // prints it, named), `CRATONVM_DBG_JITC` only.
    let ir_frame_anatomy = if crate::x64::dbg_jitc_enabled() {
        lowerer.frame_anatomy(frame_bytes_before)
    } else {
        String::new()
    };
    // Round nio21 wave 3 (lane L2): the sites the call-site exception pads
    // bake, owned by the artifact from here on (`_jit_local_handler_sites`).
    let call_throw_sites = std::mem::take(&mut lowerer.call_throw_sites);
    let mut compile_id = lowerer.compile_id;
    let mut buf = lowerer.buf;
    // Soundness bail (jit-inlining-and-ir-calls). `ExecutableBuffer::emit` is
    // non-panicking: on capacity exhaustion it sets a sticky `overflowed` flag
    // and DROPS the write, so an under-estimated buffer yields a silently
    // truncated body — execution runs off the end of the emitted code. The
    // single-pass backend has always checked this; the IR lowerer never did,
    // which was harmless only while its per-node emission was tiny and bounded.
    // Inline caches (~250 bytes/site) plus the widened invoke budget make the
    // estimate materially harder, so check it here and let the caller fall back
    // to single-pass, exactly like the unallocated-slot latch above.
    report_ir_buffer_size(graph.nodes.len(), call_nodes, buf.wanted(), capacity);
    if buf.overflowed() {
        // `needed` is `wanted()`, NOT `pos()`. `pos()` is the write cursor,
        // which STOPS at capacity the moment the buffer overflows — so it
        // reported `needed == capacity` on every single exhausted compile and
        // told you nothing about how much more the method actually wanted. The
        // whole 2026-08-01 census read `needed 4096 bytes, capacity 4096`
        // seventeen times over and looked like a tie. `wanted()` keeps counting
        // through the dropped writes, and this backend never calls `rewind_to`
        // or `emit_checked`, so it is the exact requirement rather than a
        // bound.
        //
        // On a first attempt that exactness is used, not just reported: the
        // caller lowers again at this size (round 9 wave 2). Not a refusal,
        // so nothing is counted here.
        // Only a genuine CAPACITY overflow: a `mark_codegen_unencodable`
        // discard also sets `overflowed`, and no bigger buffer fixes that.
        if let Some(out) = overflow_wanted {
            if buf.codegen_failure_reason().is_none() {
                *out = Some(buf.wanted());
                return None;
            }
        }
        return refuse(Bailout::new(BailoutReason::CodeBufferExhausted {
            needed: buf.wanted(),
            capacity,
        }));
    }
    let _code_size = buf.pos();

    // Round 9 wave 3 (runtime2 #2): the fallible constructor. A refused
    // RW -> RX flip (`VirtualProtect`/`mprotect` failing under commit
    // pressure) is a compile this method cannot finish, not a reason to abort
    // the process: `try_new` records it (`note_code_buffer_protect_failure`)
    // and the method stays on the tier below.
    let Some(mut cm) = CompiledMethod::try_new(buf) else {
        return refuse(Bailout::new(BailoutReason::Internal(
            "code-buffer-protect-failed",
        )));
    };
    // j27 wave 5 (lane UNROLLRES): an unregistered page-form poll would turn
    // a pause into a crash, so a declined site refuses the body; dropping
    // `cm` retires every site of its mapping.
    if let Err(why) = ir_page_poll::register_page_poll_sites(&cm, &page_poll_sites) {
        return refuse(Bailout::new(BailoutReason::Internal(why)));
    }
    cm.compile_id = compile_id.hand_off();
    // Wave 12 (lane runtime): the counter box lives as long as the body that
    // bakes its address.
    if sweep_entry_counter.is_some() {
        cm._jit_entry_counter = sweep_entry_counter;
    }
    cm.deopt_points = deopt_points;
    cm.named_poll_exits_only = named_poll_exits_only;
    cm.chain_poll_exits = chain_poll_exits;
    cm.exit_poll_sites = exit_poll_sites;
    cm.post_call_exit_sites = post_call_exit_sites;
    cm.exit_poll_flag = exit_poll_flag;
    // Published unconditionally since 2026-09-09.
    //
    // A merge inside a relocated callee body used to withhold this vector
    // wholesale (`ir::SPLICED_MERGE_SEEN`), because entering such an artifact
    // at a loop header produced a wrong, run-to-run varying answer. The cause
    // was never the splice: `emit_osr_entry_stubs` jumped past the block that
    // writes a CONSTANT's home word, and a merge store reads its arms from
    // their home words. `probes/Min0.java` reproduces it with no call at all.
    // The stub seeds those constants now — see the `const_seeds` loop there —
    // and the containment comes off with them.
    cm.ir_osr_entries = lowerer_osr_entries;
    cm.ir_osr_entry_kinds = lowerer_osr_entry_kinds;
    cm.ir_osr_opaque_entry_bcis = lowerer_osr_opaque_entry_bcis;
    cm.ir_osr_sentinel_free = osr_sentinel_free;
    cm.ir_osr_exception_exit_bcis = osr_exception_exit_bcis;
    cm.ir_osr_guard_exit_bcis = osr_guard_exit_bcis;
    cm.ir_osr_guard_fact_needs_opaque_entries = osr_guard_fact_needs_opaque_entries;
    cm._deopt_point_boxes = deopt_boxes;
    // nio21 wave 3 L2: forward-only candidates (`ir::call_site_throw_plan`
    // admits no handler at or before its call), so
    // `deopt::local_handler_replay_edges` reads no row out of them. The
    // SPLICE4-1 compiled handler arms' sites (JIT round 19 wave 5) own the
    // same kind of boxed `JitLocalHandlerSite`; both lists bake their boxes'
    // addresses, which a move of the `Box`es does not change.
    let mut local_handler_sites = call_throw_sites;
    local_handler_sites.extend(catch_site_boxes);
    cm._jit_local_handler_sites = local_handler_sites;
    // JIT round 19 wave 3 (lane eager3, EAGER2-4): the causes beside them.
    cm.trap_cause_points = trap_cause_points.into_boxed_slice();
    cm._forced_chain_deopt_counters = forced_chain_deopt_counters;
    cm._forced_chain_deopt_points = forced_chain_deopt_points;
    cm._poll_stress_cells = poll_stress_cells;
    // Round 15 wave 1 (lane sync, SS8-2): closed-exits checked above;
    // `JitCache::put` then publishes it unwrapped.
    cm.self_locks_monitor = self_locks_monitor;
    cm.self_lock_hold = self_lock_hold;

    // ── Moving-young relocation contract ────────────────────────────────
    //
    // Publish what `conservative_roots` needs to decide, per frame, whether a
    // relocating young collection may proceed while this frame is live. Before
    // this the IR backend published NOTHING — no maps, no frame layout, no
    // safepoint-id slot — and an empty `oop_maps` reads as "no precise
    // coverage", so every live IR frame forced the non-moving sweep. That is
    // also why the optimizing tier was gated off under moving-young.
    //
    // The frame the lowerer builds, from `rbp` downward:
    //
    //     locals | [context] | bookkeeping ×5 | spills … | arg staging | argrsv | shadow
    //     ^0       ^locals    ^                 ^first_spill           ^spill_cap_off
    //
    // The five bookkeeping words are the safepoint id, the cached thread
    // pointer, the shadow save-base, the shadow save-top and the phi
    // parallel-copy scratch. They are all below `first_spill`, so the spill
    // band the maps and the deopt verifier describe is unchanged by them.
    //
    // `callee_saved_lo` names the start of the region the verifier must NOT
    // inspect: the callee-saved XMM save area (`IR_LOWER_SAVED_XMMS`, empty on
    // System V and on every compile with the linear-scan path off), then the
    // outgoing-argument staging, the stack-arg reserve and the ABI shadow
    // space. Register images, dead argument words and scratch — storage this
    // frame never resumes from, which is exactly the role the field plays on
    // the reader side (`band_slot_is_verifiable` skips everything at or above
    // it).
    //
    // The save area was placed at `spill_cap_off` precisely so this bound did
    // not have to move: it grew the skipped band from below without changing
    // either endpoint's meaning, and `conservative_roots` needed no edit. An
    // XMM save area could never hold a reference in any case — the file is
    // FP-only — but the band is the right home for it regardless, because
    // "this word is a register image" is the property the reader is testing.
    cm.sp_id_slot_off = sp_id_slot_off;
    cm.oop_maps = oop_maps;
    // JIT round 19 proposal wave 8, lane gc8 (GC719-1): the blocks held per
    // safepoint id, for the VM's locked-monitor depths. Only ids a map is
    // recorded at (every row is pushed beside its map); metadata only.
    cm.ir_monitor_homes = {
        let mut rows = ir_monitor_homes;
        rows.sort_unstable_by_key(|&(id, _)| id);
        rows.dedup_by_key(|(id, _)| *id);
        rows.retain(|(id, _)| cm.oop_maps.iter().any(|m| m.bytecode_pc == *id));
        rows.into_boxed_slice()
    };
    cm.frame_liveness = frame_liveness;
    // Ascending by construction (ids come from a counter), which is what
    // `CompiledMethod::safepoint_bci` binary-searches on. Sorted rather than
    // asserted: a future lowerer that emits a safepoint out of order would
    // otherwise turn a diagnostic into a wrong line, and the vector is one
    // entry per GC-capable point.
    // The inlined callees this tier spliced, as a stack trace needs them.
    //
    // `code_len()` is load-bearing exactly as it is on the single-pass side:
    // `from_rows` uses it to bound the offsets it will keep. An artifact that
    // spliced nothing pushes no rows and gets `InlineFrameMap::default()` —
    // two empty `Vec`s, the state every IR artifact was in before 2026-09-08,
    // when `compiled_frame_inline_chain` returned on its `is_empty()` guard and
    // IR-tier inlining contributed no frames at all.
    cm.inline_frame_map = crate::x64::InlineFrameMap::from_rows(inline_frame_rows, cm.code_len());
    // Round 14 wave 2 (lane trace): the innermost frame's exact key -- see
    // `InlineFrameMap::chain_for_safepoint_id`. `CRATONVM_JIT_IR_INLINE_CHAIN_BY_SP_ID=0`
    // attaches nothing, and the walk keeps the bci key (read once per compile).
    if !inline_frame_sp_id_rows.is_empty()
    {
        cm.inline_frame_map
            .set_safepoint_id_rows(inline_frame_sp_id_rows);
    }
    // No `code_len` screen, for the reason the single-pass twin states at its
    // own publication site: the keys are monotonic ids rather than code
    // offsets, so a row nothing carries a key for is simply unreachable.
    cm.npe_trap_map = crate::x64::NpeTrapMap::from_rows(npe_trap_sites);
    cm.safepoint_bci_table = {
        let mut t = sp_id_bcis;
        t.sort_unstable_by_key(|(id, _)| *id);
        t.dedup_by_key(|(id, _)| *id);
        t
    };
    // gkr (OOM21-2): both halves of `crate::service_copy_window_enabled` --
    // this body's refused direct calls' service copies, and the safepoint ids
    // past its last exact re-run point. A safepoint inside a splice is placed
    // at its enclosing invoke's bci, which is where a chain's re-run rule
    // (the whole body's) is never more permissive than.
    if crate::service_copy_window_enabled() {
        let mut copies = std::mem::take(&mut lowerer.service_arg_copies);
        copies.sort_unstable_by_key(|&(off, _)| off);
        copies.dedup_by_key(|&mut (off, _)| off);
        cm.service_arg_copies = copies;
        if let Some(bc) = dead_ref_clear_code {
            let first: Vec<u32> = graph
                .site_trap_pcs
                .iter()
                .map(|&pc| resolve_spliced_pc(spliced_ranges, pc))
                .filter_map(|pc| u32::try_from(pc).ok())
                .collect();
            if let Some(closed) =
                crate::deopt::rerun_window_closed_at(bc.code, bc.code.len(), bc.handlers, &first)
            {
                cm.rerun_window_closed_sp_ids = cm
                    .safepoint_bci_table
                    .iter()
                    .filter(|&&(_, bci)| {
                        usize::try_from(bci).is_ok_and(|b| {
                            closed
                                .get(resolve_spliced_pc(spliced_ranges, b))
                                .copied()
                                .unwrap_or(false)
                        })
                    })
                    .map(|&(id, _)| id)
                    .collect();
            }
        }
    }
    // Round 11 wave 14 (lane fib, `r11w14-rt-ir-lower-oop-free-frame-patch`):
    // a body that keeps no reference in its frame -- no `Ref` node, no
    // reference parameter, no frame block, so every map it published names no
    // slot -- tells the collector so, and the collector then neither scans nor
    // verifies its band (`CompiledMethod::frame_holds_no_references`). Such a
    // frame suspended in a direct JIT-to-JIT call is placed by its return
    // address (`return_site_bci_table`), recorded at every call
    // `note_inline_frame_return_site_at` saw -- the same set of calls the
    // inline-frame map is keyed on, for the same walk. The safepoint id is
    // stored before every innermost helper; a reference-free recursive caller
    // retains its lazy ID store and is placed by the exact return address.
    cm.frame_holds_no_references =
        body_keeps_no_references && cm.oop_maps.iter().all(|m| m.frame_slot_offsets.is_empty());
    // JIT round 25 (item 3): see `CompiledMethod::ir_prim_colour_claim`.
    cm.ir_prim_colour_claim = ir_prim_colour_claim;
    cm.ir_prim_colour_clear_bytes = ir_prim_colour_clear_bytes;
    cm.ir_frame_anatomy = ir_frame_anatomy;
    // Interpreter round i5 wave 102, lane L2: kept for every body under
    // `CRATONVM_JIT_RETURN_SITE_LINE_FALLBACK` (the walk reads it only after
    // the slot refused, except for a reference-free frame as before).
    cm.return_site_bci_table = if cm.frame_holds_no_references
        || crate::return_site_line_fallback_enabled()
    {
        let mut t = return_site_rows;
        t.sort_unstable_by_key(|(off, _)| *off);
        t.dedup_by_key(|(off, _)| *off);
        t
    } else {
        Vec::new()
    };
    // Stage A.2 (precise oop maps, B-K fix) parity with the x64 fast-tier
    // driver (`x64/driver.rs`'s `cm.fully_oop_covered = compiler.precise_maps
    // && ... && compiler.safepoint_pcs.is_subset(&compiler.mapped_safepoint_pcs)`).
    // This backend never computed the field at all -- it stayed at the
    // `CompiledMethod` default of `false` for every method this tier compiled,
    // OSR or not, which blanket-failed `moving_young_osr_method_needs_fallback`'s
    // precise-map check on every OSR artifact this backend ever produced.
    // Measured 2026-08-22 on `TestKillProcessWhileWriting`: this is the
    // disjunct that actually fires (`osr_reason=(... map_coverage=N ...)`),
    // not the shadow layout and not a missing exact RBP -- both of which this
    // backend gets right already.
    //
    // Unlike the fast tier, this backend needs no PC-based subset check: each
    // `OopMapEntry` above already carries its own `moving_young_coverage_complete`
    // (the shadow-push `coverable && (published || slots.is_empty())` verdict,
    // computed per safepoint at the point it is emitted), and `cm.oop_maps` is
    // the complete set this compilation ever pushed to -- so the aggregate is a
    // straight AND over data already relied on elsewhere: it is the exact
    // per-entry flag `gc_quiescence`'s per-cycle proof already consults for
    // every non-OSR moving-young collection. An empty `oop_maps` makes this
    // vacuously true, which is safe: `has_precise_oop_maps()` (`!oop_maps.is_empty()`)
    // is what actually gates the fast tier's OR-branch on "no maps at all," so a
    // vacuous true here never overrides that check.
    cm.fully_oop_covered = cm.oop_maps.iter().all(|m| m.moving_young_coverage_complete);
    // The same aggregate under the name that says what it measures — see
    // `CompiledMethod::fully_shadow_covered`. The two coincide on THIS backend
    // and deliberately differ on the fast tier, where `fully_oop_covered` is
    // the frame-slot subset test; the OSR fallback reads `fully_shadow_covered`
    // so it gets the same question from both.
    cm.fully_shadow_covered =
        !cm.oop_maps.is_empty() && cm.oop_maps.iter().all(|m| m.moving_young_coverage_complete);
    cm.osr_frame_size = frame_size;
    // OSR and the save area, stated where the artifact is published.
    //
    // gcg g3/v (`gcg-g1f-ir-lower-osr-save-area-comment-and-assertion-read-the-wrong-osr-table`):
    // this block used to assert `cm.osr_pc_to_native.is_none()` and say "this
    // backend has no OSR". Both were stale: `osr_pc_to_native` is the
    // SINGLE-PASS trampoline's table (`x64/osr.rs`), which this backend never
    // sets, so the assertion could not fire; and this backend HAS OSR entries,
    // published in `cm.ir_osr_entries` and entered through its own stubs
    // (`emit_osr_entry_stubs`). The invariant the assertion was written for
    // holds by construction there: each stub builds the frame and saves the
    // caller's GPRs and XMMs into the SAME slots `emit_prologue` uses
    // (`saved_gpr_regs` / `saved_xmm_regs`), so every exit restores what was
    // saved. The single-pass trampoline (`osr_trampoline`, which enters past
    // `emit_prologue`) is never used for an artifact of this backend
    // (`an_ir_artifact_never_offers_an_osr_entry_the_save_area_would_break`).
    // Where the reader finds what the emission side published. Without these
    // three, `shadow_window_from_frame` cannot even locate the shadow stack —
    // it returns `None`, `published_shadow_values` yields the empty set, and
    // EVERY published word then reads back as unpublished. That is not a
    // partial failure that shows up as a smaller win: it makes a fully correct
    // publication indistinguishable from no publication at all, and it is what
    // `compiled-frame-oop-not-published` meant on `IrEscapeProbe` after the
    // maps, the frame record and the parameter homes were all in place.
    cm.shadow_thread_slot_off = shadow_thread_slot_off;
    cm.shadow_savebase_slot_off = shadow_savebase_slot_off;
    cm.shadow_frame_block = shadow_frame_block;
    cm.shadow_off_in_thread = helpers.shadow_stack_offset_in_thread as i32;
    // gen r5w6/oomjit10: the prologue's GPR save list, for the collector's
    // callee-saved-image rule (`CompiledMethod::ir_saved_gpr_slots`). The
    // same snapshot the prologue and every epilogue emit from.
    // gcg g3/v: published for OSR bodies too -- an OSR entry stub saves the
    // caller's registers into these same slots (see the block above). The old
    // guard tested the single-pass `osr_pc_to_native`, always `None` here, so
    // this is behaviour-identical.
    cm.ir_saved_gpr_slots = Some(ir_saved_gpr_slots);
    cm.frame_layout = crate::FrameLayout {
        // Round 16 wave 11 (lane frames2, FR-B1): the EXCLUSIVE end of the
        // Java-local words, `(num_locals + 1) * 8` -- local `i` is at
        // `[rbp - (i + 1) * 8]`, so the last one is AT `num_locals * 8`, which
        // this used to publish. Every `FrameLayout` range is `lo..hi` with `hi`
        // exclusive, the single-pass producer publishes `(n + 1) * 8`, and
        // `FrameLayout::is_non_oop_reserved_tail_slot` reads `off >=
        // java_locals_hi` as the reserved tail: so the band scan (both its
        // root and pin halves) never read the last Java-local word of an
        // optimizing-tier frame, which is the prologue's home of the last
        // parameter when that parameter is the last local -- `this` of a
        // one-slot instance method, the reference argument of
        // `static f(Object)`. `ir_published_java_locals_hi`, kill switch
        // `CRATONVM_JIT_IR_PUBLISH_LAST_LOCAL=0`.
        java_locals_hi: ir_published_java_locals_hi(locals_size, ir_publish_last_local_enabled()),
        // No LICM hoists, no scalar-replacement slots and no per-safepoint
        // blind GPR spill in this backend: empty ranges (`hi == lo`), which
        // `is_register_image` and `region_name` both read as "absent".
        ref_hoist_lo: 0,
        ref_hoist_hi: 0,
        ref_hoist_mapped: false,
        arith_lo: 0,
        arith_hi: 0,
        scalar_lo: 0,
        scalar_hi: 0,
        locals_hi: first_spill,
        spill_lo: first_spill,
        spill_hi: spill_cap_off,
        callee_saved_lo: spill_cap_off,
        callee_saved_hi: frame_size,
        callee_saved_shallow: false,
        // Named separately as well as covered by the band above. Not for
        // `FrameLayout::region_name`: its ladder tests `callee_saved_*` first,
        // and on this backend that band spans the whole tail, so these words
        // still report as `callee-saved-gpr-image`. What the range decides is
        // the remap: `conservative_roots::is_callee_saved_gpr_image` excludes
        // it, so the register-image remap never rewrites an XMM word (a
        // double's bits are not a reference). `hi == lo == 0` when nothing was
        // reserved, which `is_register_image` already reads as "absent".
        xmm_saved_lo: if saved_xmm_bytes > 0 {
            spill_cap_off
        } else {
            0
        },
        // gci i4/ir (i4/so open item 1): `saved_xmm_regs` writes register
        // `k` at `MOVUPS [rbp - (spill_cap_off + 16*(k+1))]`, i.e. the words
        // at offsets `spill_cap_off + 8 ..= spill_cap_off + saved_xmm_bytes`,
        // and the reader's range is `[lo, hi)`: `hi` is one word past the
        // top save, or the low qword of the last saved XMM sat outside the
        // range and was admitted to the GPR-image remap (a double whose bits
        // equal a moved object's old address would have been rewritten). The
        // first GPR save is at `spill_cap_off + saved_xmm_bytes + 8`
        // (`saved_gpr_regs`), exactly `hi`, so it stays out. `lo` stays at
        // `spill_cap_off` on purpose: that word is the last spill colour
        // (`planned_slot_off` admits `off == spill_cap_off`), and keeping it
        // out of the remap keeps its present pin-not-rewrite treatment (a
        // `long` / `double` colour must never be rewritten); i4/so's
        // `ir_last_colour_is_dead_above_cursor` drops it when it is dead.
        xmm_saved_hi: if saved_xmm_bytes > 0 {
            spill_cap_off + saved_xmm_bytes + 8
        } else {
            0
        },
        reg_spill_lo: 0,
        reg_spill_hi: 0,
        // No outgoing reserve this backend can name; see
        // `FrameLayout::outgoing_lo`, where 0 means "no claim".
        outgoing_lo: 0,
        // The GPR half of this backend's `SavedRegisters` region, on the same
        // convention as the single-pass backend: `gpr[0]` is its LOWEST
        // address, at `[rbp - deopt_regs_base]`, so `deopt_regs_base` is the
        // DEEPEST of the sixteen offsets.
        //
        // On this tier the GPR half changes nothing: this backend's
        // `callee_saved_lo..callee_saved_hi` already spans the whole tail, and
        // both `region_name`'s ladder and `register_image_remap_admits` test
        // that band FIRST, so the GPR half already reports as
        // `callee-saved-gpr-image` and is already rewritten by
        // `remap_register_image_words`. The XMM half below is NOT inert here:
        // `is_callee_saved_gpr_image` subtracts `deopt_xmm_*` from the band, so
        // those words are withdrawn from the remap and let go by the pin
        // decision (no reference is ever resumed from them; see
        // `FrameLayout::deopt_xmm_lo`). It still reports as
        // `callee-saved-gpr-image` in `region_name`.
        //
        // Round 16 wave 11 (lane frames2): the words the image actually
        // reserves (`Lowerer::deopt_gpr_image`) -- all sixteen for the full
        // image, `gpr[0..=k]` for a sized one -- with `hi == base + 8` either
        // way, the convention the collector derives `gpr[0]` and the RBP image
        // from. With the switch off these are the literal `base - 120` /
        // `base + 8` this replaced.
        deopt_gpr_lo: deopt_gpr_image.0,
        deopt_gpr_hi: deopt_gpr_image.1,
        // The XMM half of the same region; see `FrameLayout::deopt_xmm_lo`.
        // Round 16 wave 7 (lane frames): through `deopt_xmm_image_range`,
        // which the fresh-frame clear's `_SKIP_UNROOTED` reads too. Same
        // values as the literal arithmetic this replaced. Wave 11 (frames2):
        // `(0, 0)` when a sized image reserves no XMM half.
        deopt_xmm_lo: deopt_xmm_image.map_or(0, |r| r.0),
        deopt_xmm_hi: deopt_xmm_image.map_or(0, |r| r.1),
        frame_size,
    };
    if needs_context {
        cm.needs_context = true;
    }
    if graph.vthread_boundaries.as_ref().is_some_and(|expected| {
        expected
            .iter()
            .all(|bci| lowerer.vthread_boundaries_emitted.contains(bci))
    }) {
        cm.vthread_boundary_guards = true;
        cm.can_deopt_resume = true;
    }
    // Guard-surviving scalar replacement: if any deopt frame carries a
    // scalar-replaced object as a `FrameValue::VirtualObject`, route this method
    // through the interpreter's precise-resume + materialize path
    // (`resume_real_ir_deopt` → `materialize_virtual_objects`) instead of the
    // no-materialize int-only path / whole-method re-run. Without this the
    // `can_deopt_resume` gate stays off for the IR backend and the emitted
    // VirtualObject is never consumed. Sound to enable: the per-slot mapper and
    // the materializer each bail to a safe whole-method re-run on any slot they
    // cannot reconstruct. Only reachable with `sr_map` set.
    //
    // A method that takes a monitor is no longer excluded. Until 2026-09-12
    // every `FrameState` this lowerer built hard-coded `monitors: Vec::new()`,
    // so a precise resume would have rebuilt a frame that believed it held no
    // lock, and this condition refused any graph with a monitor op. The
    // builder now records its monitor stack in every snapshot, and
    // `resolve_monitors` lowers it with a `relock` marker: a lock the compiled
    // code took is still held and is left alone, a lock escape analysis elided
    // is re-acquired by the resume after the object is materialized
    // (`build_deopt_frame_inner`). See
    // `ir-frame-states-carry-no-monitor-stack-FIXED-20260912.md`.
    if sr_map.is_some()
        && cm
            ._deopt_point_boxes
            .iter()
            .any(|p| crate::deopt::count_virtual_objects(&p.frame_state) > 0)
    {
        cm.can_deopt_resume = true;
    }
    // A body that takes a monitor vouches for its own frames too (round 11
    // wave 2, lane lower; `r11-tier-ir-monitor-body-trap-reruns-from-entry`).
    //
    // The sinks' OTHER precise-resume arm, `sink_precise_resume_allowed`,
    // refuses every body whose bytecode holds a monitor, because it cannot tell
    // this backend from the single-pass one — and the single-pass backend may
    // elide a lock without describing it. So an optimizing body with a
    // `synchronized` block and no scalar-replaced object answered neither arm,
    // and its trap re-ran the method from entry, repeating every side effect
    // committed before the trapping bci.
    //
    // This backend describes every lock: the builder records its monitor stack
    // in every snapshot (and refuses unstructured locking and any monitor in a
    // spliced body), and `resolve_monitors` publishes it with a `relock` marker
    // on each elided lock, which `build_deopt_frame_inner` honours. That is the
    // same promise the single-pass backend makes when it sets this flag for
    // every body with deopt points, `ACC_SYNCHRONIZED` ones included; the VM's
    // resume doors hand the method monitor to the resumed frame
    // (`JitSynchronizedMonitorGuard::transfer_to_resumed_frame`).
    if !cm.can_deopt_resume
        && !cm._deopt_point_boxes.is_empty()
        && graph.safepoints.iter().any(|sp| !sp.monitors.is_empty())
    {
        cm.can_deopt_resume = true;
    }
    // Round 12 wave 3 (lane replay): the same promise when the graph takes a
    // monitor but no surviving snapshot sits inside its region (a
    // `synchronized` block with no deopt point in it). Every point is then
    // outside every lock and its frame, which records the builder's monitor
    // stack, correctly holds none. Without the vouch the sinks' other arm
    // refuses the body for its monitor BYTECODE and the only fallback is a
    // whole-method replay, which for a conditional deopt in a loop that
    // stores after it commits the store again (or, with the loop-closed
    // replay rule, raises `InternalError`). `CRATONVM_JIT_IR_MONITOR_OP_VOUCH=0`
    // restores the snapshot-only rule.
    if !cm.can_deopt_resume
        && !cm._deopt_point_boxes.is_empty()
        && graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::MonitorEnter | Op::MonitorExit))
    {
        cm.can_deopt_resume = true;
    }
    // The same promise for a METHOD-ENTRY body of a method with monitor
    // bytecode that gave a back-edge poll an exit (interpreter round i1 wave
    // 19, lane L2; `ir_entry_mode_exits_admitted` admits such a method on this
    // arm). No surviving snapshot need carry a monitor (a `synchronized` block
    // with no safepoint inside leaves none), and then the rule above does not
    // fire, so the interpreter and first-call doors, whose other arm refuses
    // any monitor bytecode, would re-run the exit from entry. Every snapshot
    // still records the monitor stack, so the vouch holds for every point.
    if !cm.can_deopt_resume
        && poll_exits_emitted
        && lowerer.poll_mode_exits.is_method_entry()
        && poll_exit_bytecode.is_some_and(|b| {
            b.code_len <= b.code.len() && crate::bytecode_holds_monitor(b.code, b.code_len)
        })
    {
        cm.can_deopt_resume = true;
    }
    // gkr OOM21-2: at a precise-resume point after every incoming reference
    // parameter is dead, the callee no longer needs the caller's service copies to rebuild
    // its current frame. Publish those points separately from the ordinary
    // "entry replay is impossible" set: the VM will refuse entry replay if a
    // precise rebuild unexpectedly declines after the collector transfers the
    // copies to the callee.
    if crate::service_copy_window_enabled()
        && crate::deopt_sink_resume_enabled()
        && Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_err()
    {
        if let Some(bc) = dead_ref_clear_code.filter(|bc| {
            bc.handlers.is_empty()
                && !bc.method_synchronized
                && !crate::bytecode_holds_monitor(bc.code, bc.code.len())
        }) {
            let param_slots: Vec<usize> = (0..bc.num_params).collect();
            let live = crate::regalloc::live_locals_per_pc_all(
                bc.code,
                bc.code.len(),
                bc.num_params,
                &param_slots,
                bc.handlers,
                bc.num_locals,
            );
            let reference_params =
                reference_parameter_slots(bc.method_descriptor, bc.num_params).unwrap_or_default();
            let resume_only_decisions: Vec<(u32, u32, usize, Vec<bool>)> = cm
                .safepoint_bci_table
                .iter()
                .filter_map(|&(id, raw_bci)| {
                    let bci = usize::try_from(raw_bci).ok()?;
                    let resolved_bci = resolve_spliced_pc(spliced_ranges, bci);
                    let dead = reference_params
                        .iter()
                        .map(|&slot| local_dead_at(&live, bc.num_locals, resolved_bci, slot))
                        .collect();
                    Some((id, raw_bci, resolved_bci, dead))
                })
                .collect();
            cm.service_arg_resume_only_sp_ids = resume_only_decisions
                .iter()
                .filter_map(|(id, _, _, dead)| {
                    (!reference_params.is_empty() && dead.iter().all(|&is_dead| is_dead))
                        .then_some(*id)
                })
                .collect();
            cm.service_arg_resume_only_sp_ids.sort_unstable();
            cm.service_arg_resume_only_sp_ids.dedup();
        }
    }
    if let Some(base) = location_census_base {
        eprintln!("[ir-location-end] base={base:#x} method={}", graph.scope_method_key);
    }
    Some(cm)
}

// ── Tests ────────────────────────────────────────────────────────────

/// A definition whose value is already live in its register is not published
/// again -- **default ON** since 2026-09-05; `CRATONVM_JIT_IR_SKIP_REPUBLISH=0`
/// is the kill switch.
///
/// Off is the historical unconditional reload of the home word at every
/// definition site, phis included.
pub(in crate::ir_lower) fn ir_skip_live_republish_enabled() -> bool {
    // 2026-09-05: DEFAULT ON. `=0` is the kill switch.
    true
}

/// An edge's phi copies move register to register where both ends are
/// resident -- **default ON** since 2026-09-05;
/// `CRATONVM_JIT_IR_PHI_COPY_REGS=0` is the kill switch.
///
/// Off is the historical memory-to-memory copy through RAX followed by a
/// reload of the word just written.
pub(in crate::ir_lower) fn ir_phi_copy_regs_enabled() -> bool {
    // 2026-09-05: DEFAULT ON. `=0` is the kill switch.
    true
}

/// Admit `Op::Phi` to the residency plan — **default ON** *within*
/// [`linear_scan_enabled`]; `CRATONVM_JIT_IR_PHI_RESIDENCY=0` is the kill
/// switch. Declared as `jit/ir-phi-residency`, so `-XX:` options and
/// `flags::with_thread_overrides` reach it.
///
/// # Why a sub-flag rather than riding the outer one
///
/// The outer flag turns on a *register file*. This turns on one *population's*
/// access to it, and the two fail differently: the file's risk is a value read
/// from a register nothing published, and the phi half's risk is the edge-copy
/// layer and the register layer disagreeing about an order (which is not
/// hypothetical — see `emit_phi_copies`' `defer_publish`, written after
/// `java.time.Duration.toNanos()` returned `seconds * 1e9 + seconds` on 198,356
/// calls in 200,000). A single flag makes a bisect unable to say which layer
/// moved; two flags let `CRATONVM_JIT_IR_PHI_RESIDENCY=0` keep the whole
/// pre-phi residency plan intact while taking the phi half out, which is what
/// named that layer as the owner of that bug in the first place.
///
/// It is also the A/B the doc asks for: with it off, the census reports
/// `[ir-ls] phis=N: ... disabled=N`, i.e. exactly how much a given workload has
/// riding on this, before anyone argues about a measurement.
///
/// # What it is off
///
/// Off is the pre-2026-09-02 emission for phis and nothing else: the allocator
/// keeps their structural pin (`release_phi_pins` is not called), the scan loop
/// refuses them under `PhiRefusal::Disabled`, and `emit_phi_copies` writes only
/// the home word at each incoming edge. Every other value's residency is
/// unchanged.
///
/// # What it is NOT
///
/// It is not a waiver of the safepoint obligation. A `Ref`-typed phi is refused
/// by type whichever way this flag is set — see the refusal in
/// [`plan_register_residency`], and
/// `a_reference_typed_phi_is_refused_by_type_whatever_the_flags_say`.
pub(in crate::ir_lower) fn ir_phi_residency_enabled() -> bool {
    true
}

/// Promote only where a register can repay its prologue save -- **default
/// ON**, opt out with `CRATONVM_JIT_IR_RESIDENCY_PAYS=0` to promote every
/// candidate the allocator hands back, which is what this file did before
/// 2026-09-02 and what made `FibProbe.fib` slower with the file on than off.
pub(in crate::ir_lower) fn ir_residency_pays_enabled() -> bool {
    true
}

/// What the NEXT register taken from the loop-carried reservation costs, in
/// loop-weighted uses, once `taken` have already been taken.
///
/// `carried_w` is every carried candidate's `live.weight`, descending. The
/// carried pass serves its list in that same order and stops when the file runs
/// out, so the value a crossblock admission actually displaces is the weakest
/// one that would still have been served -- `carried_w[file_len - 1]` -- and
/// each further admission displaces the next one up. The bar therefore RISES as
/// the file empties, which is what stops a run of cheap single-use values from
/// evicting the whole carried set one register at a time.
///
/// Returns `u64::MAX` once the file is spent, refusing everything after that,
/// and `0` when there are fewer carried candidates than registers -- nothing is
/// displaced then, so nothing has to be outbid.
///
/// The caller compares with `>=`, not `>`, and that is MEASURED rather than
/// chosen. On `probes/FieldLoop.java` the carried weights are
/// `[20, 11, 10, 10, 10, 10]`, so the price is 10 -- and the induction variable
/// the whole transform exists to serve weighs exactly 10 too. Under `>` all ten
/// candidates are refused and the body reverts to its slow 1052 bytes. A tie
/// means "these two values are read equally often", and at that point the
/// crossblock value is the better holder: the carried pass's own candidates are
/// phis and loop-carried values whose home word is written by
/// `emit_phi_copies` at every incoming edge regardless, so their register saves
/// reads only, while the crossblock value's register removes a store AND a
/// reload. `probes/RegPressure.java` is unaffected by the tie-break -- its price
/// is 21 against candidates weighing 10, which `>=` refuses just as `>` did.
///
/// This replaces a COUNT-based budget, which was built first and measured wrong:
/// `ir_gp_file().len() - carried_demand` is zero for any loop with five or more
/// carried candidates, which is most loops including `probes/FieldLoop.java` --
/// so it removed the regression on `RegPressure` by removing the transform
/// everywhere, reverting `FieldLoop.sum` to its slow 1052-byte body. A count
/// cannot tell "displacing something worth less than me" from "displacing
/// something worth more".
pub(in crate::ir_lower) fn carried_displacement_price(carried_w: &[u64], taken: usize) -> u64 {
    let file_len = ir_gp_file().len();
    if carried_w.len() < file_len {
        return 0;
    }
    match file_len.checked_sub(1 + taken) {
        Some(i) => carried_w.get(i).copied().unwrap_or(0),
        None => u64::MAX,
    }
}

/// Leave the loop-carried reservation its registers before the crossblock arm
/// spends them -- **default ON**, opt out with
/// `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK_BUDGET=0`.
///
/// [`ir_residency_crossblock_enabled`] admits single-use values in the MAIN
/// residency loop. `ir_reserve_carried_enabled`'s pass runs after it, over
/// `free = ir_gp_file() - taken`, so on a contended method the crossblock arm
/// can spend the whole file before the values read on every iteration are ever
/// considered. That is measured, on `probes/RegPressure.java`: 5 loop-carried
/// values resident with the flag off, 2 with it on, and 5-12% SLOWER.
///
/// The budget is the cheapest form of the occupancy term that result asks for.
/// Count what the carried pass will want -- the same predicate it uses -- and
/// let the crossblock arm have only what is left over:
///
/// ```text
/// budget = ir_gp_file().len() - |{ id : live.carried_and_read[id] && has home
///                                      && Int|Long && !Const && (phi ok) }|
/// ```
///
/// It is deliberately a COUNT and not a priority queue. The carried pass
/// already orders its own candidates by loop-weighted use count; this only has
/// to stop the main loop from emptying the file first, and a count does that
/// without duplicating the ordering in two places that could disagree.
pub(in crate::ir_lower) fn ir_residency_crossblock_budget_enabled() -> bool {
    true
}

/// Let a SINGLE-USE value be register-resident when the carry window cannot
/// reach it -- **default OFF**, opt in with
/// `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK=1`.
///
/// `ir_residency_pays_here` reduces to `static_uses >= 2` in a default build
/// (`ir_residency_loop_weight_enabled` is off), so a value read ONCE never gets
/// a register. The reasoning behind that rule is sound where it applies: a
/// single-use value costs one publish and saves one reload, which is a wash --
/// **and `plan_carries` already serves those, for free, by handing the
/// producer's RAX straight to the consumer.**
///
/// But `plan_carries` scans a window exactly one node wide:
///
/// ```text
/// for w in 1..block.nodes.len() {
///     let prod = block.nodes[w - 1];
///     let cons = block.nodes[w];
/// ```
///
/// so it reaches a single-use value only when its consumer is the very next
/// node in the same block. For every other single-use value the trade is not a
/// wash at all: the alternative to a register is a home STORE and a RELOAD, two
/// memory operations, and the publish that replaces them is a register move.
///
/// `FibCall.fib` is the shape that names it. `n-1` and `n-2` are computed in
/// the entry block and read at call sites blocks later, so neither can be
/// carried and neither is resident:
///
/// ```asm
/// lea eax,[rbx-1] ; mov [rbp-50h],rax    ; … stored in the entry block
/// mov rdx,[rbp-50h]                      ; … and reloaded at the call
/// ```
///
/// Four memory operations per call for two values a callee-saved register would
/// have held across it, which is what the single-pass tier does and what
/// `c2-fib-per-call-budget-20260912.md` counts as the largest term in that
/// method's per-call budget.
///
/// This admits exactly the complement of the carry window, so the two
/// mechanisms partition the single-use population instead of competing for it:
/// `plan_carries` already declines a value residency took
/// (`assigned_gpr(prod).is_some()`, `carry_skips[3]`).
///
/// **MEASURED** (2026-09-12, one binary, arms interleaved with a control arm;
/// `c2-the-per-frame-contract-residency-20260912.md`):
///
/// ```text
/// FieldLoop.sum  reps=3000 n=20000   A 99.0 | C 99.0 | B 91.5 ms
///                floor 0.0%   effect -7.6%   ratio 0.924x   FASTER
/// FibCall.fib    n=30                A 179.0 | C 177.0 | B 178.0 ms
///                floor 1.1%   effect +0.0%   ratio 1.000x   UNMEASURABLE
/// ```
///
/// The loop win is one value: `i + 1` is read exactly once, by the phi at the
/// back edge, so it was stored to its home and reloaded on every iteration.
/// The optimizing body SHRINKS there, 1052 -> 1039 bytes.
///
/// `fib` is the cost side. It admits four more values (`single_use` 13 -> 6,
/// `resident` 2 -> 6) and grows the body 788 -> 883, because every register in
/// [`ir_gp_file`] is callee-saved and `fib` has FOUR epilogues, each restoring
/// the whole set -- twelve restores EMITTED against three removed reloads.
///
/// That static count is not the cost, and `probes/ManyExits.java` was written
/// to prove it was: loop-free with SIX exits, the shape an epilogue-scaled cost
/// would have to lose on. It is neutral twice (1.007x, 1.002x). **Exactly one
/// epilogue executes per call**, so the file is saved once and restored once
/// whatever the static exit count:
///
/// ```text
///   dynamic cost = 2 memory ops per promoted register, PER CALL
///                  (one save, one restore) -- NOT scaled by exit count
///   static cost  = 1 + N_epilogues per register  (code size only)
///   benefit      = reloads/stores removed, TIMES how often they execute
/// ```
///
/// All three probes follow from that: `fib` and `ManyExits` each pay ~6 ops and
/// recover ~6 (a wash, at four exits and at six alike), while `FieldLoop` pays
/// ZERO -- its body already saves all five registers in both arms -- and
/// recovers a load and a store on each of 20 000 iterations.
///
/// `static_uses >= 2` misses both sides: it counts static edges rather than
/// executions, and it prices a register-to-register publish as costing what a
/// memory reload costs.
///
/// **DEFAULT OFF, and the losing shape is now KNOWN.** `probes/RegPressure.java`
/// oversubscribes the five-register file -- six loop-carried accumulators plus
/// three cross-block single-use values -- and this flag is SLOWER there, three
/// runs out of three, each above its own floor:
///
/// ```text
///   mix       (6 accumulators)   floor 0.9%  +12.5%  ratio 1.125x  SLOWER
///   mix       (confirmation)     floor 5.5%   +9.7%  ratio 1.097x  SLOWER
///   mixNarrow (3 accumulators)   floor 2.3%   +5.4%  ratio 1.054x  SLOWER
/// ```
///
/// The census names it: 25 more values admitted buys ONE more resident, while
/// `split_or_spilled` goes 12 -> 28 and `carried_reserved` 5 -> 2. The model
/// above prices the register as free to take; under pressure its real price is
/// whatever the value that would otherwise have held it was worth, and on that
/// probe that value is read every iteration.
///
/// [`ir_residency_crossblock_budget_enabled`] is that occupancy term, and it is
/// **default ON**. With it, `mix` compiles byte-identically to this flag being
/// off (3164) so its 12.5% is structurally gone, and `FieldLoop`'s win is kept
/// (1039). It is still not enough to flip THIS flag on: `mixNarrow` remains
/// 1.067x slower over a 0.0% floor, and it cannot be fixed by any rule of that
/// shape -- it and `FieldLoop.sum` present identical carried weights, identical
/// displacement price and identical candidate weights, and diverge anyway. Loop
/// weight is not the discriminating variable; the allocator's own outcome under
/// the added pressure is. See section 6d.
///
/// Correctness is not the objection -- section 6b: `jit-flag-soak` divergent=0,
/// every `CratonBench` / `CratonBenchC2` checksum bit-identical.
///
/// **Measuring this flag on a new probe: check `CRATONVM_DBG=jit-disasm` prints
/// a `full/ir` body at the A/B's settings, and that the two arms' `len=` DIFFER.**
/// The OSR door is single-pass only (`osr ir-eligibility: ... INERT at this
/// door`) while still running the IR pipeline, so a linear-scan census can differ
/// between arms that execute byte-identical code. The first run of `RegPressure`
/// read 1.000x over a 0.5% floor for exactly that reason.
pub(in crate::ir_lower) fn ir_residency_crossblock_enabled() -> bool {
    matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

/// Is `id`'s single use the node immediately after it in its own block -- the
/// window [`Lowerer::plan_carries`] scans?
///
/// Mirrors that scan deliberately, including its "consumer is `nodes[w]`,
/// producer is `nodes[w - 1]`" shape, so the two cannot drift into either
/// double-serving a value or leaving one to neither. It does NOT re-check the
/// carry's type and arm conditions: a value that clears this window but fails
/// those is one the carry declines and residency would then have to serve, and
/// answering `true` here would leave it with nothing. Erring toward `false`
/// hands it to residency, which is the side that always has an answer.
pub(in crate::ir_lower) fn single_use_is_in_the_carry_window(graph: &Graph, schedule: &Schedule, id: usize) -> bool {
    for block in &schedule.blocks {
        let Some(w) = block.nodes.iter().position(|&n| n as usize == id) else {
            continue;
        };
        let Some(&cons) = block.nodes.get(w + 1) else {
            return false;
        };
        return graph
            .nodes
            .get(cons as usize)
            .is_some_and(|cn| cn.inputs.iter().any(|&i| i != NO_NODE && i as usize == id));
    }
    false
}





/// `CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH=1` (JIT round 18 wave 11, lane
/// osrfollow, OSRPOLY18-1; **default ON** since 2026-11-09, `=0` kills it: `regex-matches` -341 instructions/op): an
/// `Op::ExactClassIs` whose only use is an `Op::If` -- each class test of the
/// guarded receiver splices (mono-, bi- and polymorphic) -- is tested on the
/// flags at the branch ([`Lowerer::emit_exact_class_test_flags`]) instead of
/// `MOV EAX, 1/0; store; reload; TEST`. Same answer for every input (null and
/// arrays still answer "no"); per test it saves the 0/1 materialisation, a
/// frame store and a reload. Only asked when `CRATONVM_JIT_IR_FUSED_BRANCH` is
/// on too. Read once per lowering; no `static`.
pub(in crate::ir_lower) fn ir_fused_class_test_branch_enabled() -> bool {
    true
}

/// The bytes of [`Lowerer::emit_exact_class_test_flags`] (see there for the
/// layout): ZF=1 iff RAX is non-null, a plain object and of exactly
/// `class_id`. RAX-only.
pub(in crate::ir_lower) fn exact_class_test_flags_non_null_bytes(class_id: u32) -> [u8; 13] {
    let full = exact_class_test_flags_bytes(class_id);
    let mut out = [0u8; 13];
    out.copy_from_slice(&full[5..18]);
    // Skip the 7-byte `CMP dword [RAX + 0], class_id` when non-plain (JNE rel8 = +7).
    out[5] = 7;
    out
}

pub(in crate::ir_lower) fn exact_class_test_flags_bytes(class_id: u32) -> [u8; 23] {
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    let id = class_id.to_le_bytes();
    [
        0x48,
        0x85,
        0xC0, // TEST RAX, RAX
        0x74,
        0x0F, // JZ null (+15 -> 20)
        0xF6,
        0x40,
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
        cratonvm_types::KIND_TAG_BYTE_MASK, // TEST BYTE [RAX+kind], mask
        0x75,
        0x0C, // JNE done (+12 -> 23), ZF=0
        0x81,
        0x78,
        0x00,
        id[0],
        id[1],
        id[2],
        id[3], // CMP DWORD [RAX+0], class_id
        0xEB,
        0x03, // JMP done (+3 -> 23)
        0x83,
        0xC8,
        0x01, // null: OR EAX, 1 (ZF=0)
    ]
}

/// JIT round 19 proposal wave (lane osr19, OSRPOLY19-2): the bytes of the
/// `Op::ClassWordOf` arm and of its OSR recomputation
/// ([`OsrRematInsn::ClassWordOf`]). In: a reference in RAX. Out: RAX = the
/// sign-extended 32-bit header class word of a plain object, `0` for a null
/// (no load) or an array (whose header names its component). RAX-only; every
/// jump is rel8 because the sequence is fixed-length:
///
/// ```text
///   +0  TEST RAX, RAX                 48 85 C0
///   +3  JZ done (+13): RAX is the 0   74 0D
///   +5  TEST BYTE [RAX+kind], mask    F6 40 kk mm
///   +9  JNE zero (+5)                 75 05
///  +11  MOVSXD RAX, DWORD [RAX+0]     48 63 00
///  +14  JMP done (+2)                 EB 02
///  +16  zero: XOR EAX, EAX            31 C0
///  +18  done:
/// ```
///
/// Sign-extended so the word is a canonical `int` in a 64-bit home: the
/// chain's `Cmp(Eq, word, Const(id))` compares it with `id as i32`.
pub(in crate::ir_lower) fn class_word_of_bytes() -> [u8; 18] {
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    [
        0x48,
        0x85,
        0xC0, // TEST RAX, RAX
        0x74,
        0x0D, // JZ done (+13 -> 18)
        0xF6,
        0x40,
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
        cratonvm_types::KIND_TAG_BYTE_MASK, // TEST BYTE [RAX+kind], mask
        0x75,
        0x05, // JNE zero (+5 -> 16)
        0x48,
        0x63,
        0x00, // MOVSXD RAX, DWORD [RAX]
        0xEB,
        0x02, // JMP done (+2 -> 18)
        0x31,
        0xC0, // zero: XOR EAX, EAX
    ]
}

/// JIT round 19 wave 3 (lane eager3, EAGER2-3): the `Op::ClassInitTest` arm's
/// machine code. RAX = `1` when the byte at `state_addr` (a class's
/// `init_state`, kept alive by the artifact) equals `initialized`, else `0`.
///
/// ```text
///   +0  MOV RAX, imm64(state_addr)    48 B8 <8>
///  +10  MOVZX EAX, BYTE [RAX]         0F B6 00
///  +13  CMP EAX, imm32(initialized)   3D <4>
///  +18  SETE AL                       0F 94 C0
///  +21  MOVZX EAX, AL                 0F B6 C0
///  +24
/// ```
///
/// A plain byte load: x86-64 loads are acquire, so a `1` here orders every
/// later read after the initializing thread's publication of the class, as
/// `vm_util::is_class_initialized_fast`'s `Acquire` load does.
pub(in crate::ir_lower) fn class_init_test_bytes(state_addr: u64, initialized: u8) -> [u8; 24] {
    let a = state_addr.to_le_bytes();
    let v = u32::from(initialized).to_le_bytes();
    [
        0x48, 0xB8, a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], // MOV RAX, imm64
        0x0F, 0xB6, 0x00, // MOVZX EAX, BYTE [RAX]
        0x3D, v[0], v[1], v[2], v[3], // CMP EAX, imm32
        0x0F, 0x94, 0xC0, // SETE AL
        0x0F, 0xB6, 0xC0, // MOVZX EAX, AL
    ]
}



/// How `FCmp(a, b) <cc> 0` branches as ONE `UCOMIS; Jcc`:
/// `Some((swap, taken_jcc))` means `UCOMIS a, b` (or `UCOMIS b, a` when
/// `swap`) followed by the near-Jcc byte `taken_jcc` is taken exactly when the
/// JVMS three-way result satisfies `<cc> 0` -- NaN included. `None` for
/// `Eq`/`Ne`, which need ZF and PF together.
///
/// `UCOMIS p, q` sets CF on `p < q` OR unordered, ZF on `p == q` OR
/// unordered, neither on `p > q`. So `JA` (CF=0, ZF=0) is "`p > q`, NaN
/// false", `JAE` "`p >= q`, NaN false", `JB` "`p < q`, NaN true", `JBE`
/// "`p <= q`, NaN true". `fcmpl`/`dcmpl` answer -1 on NaN, so `< 0`/`<= 0` are
/// NaN-true and `> 0`/`>= 0` NaN-false: that is exactly `UCOMIS a, b` with
/// `JB/JBE/JA/JAE`. `fcmpg`/`dcmpg` answer +1, flipping which side NaN lands
/// on; swapping the operands (`UCOMIS b, a`) and mirroring the condition
/// gives it. Every answer is a single flags predicate, so its negation (the
/// `cc ^ 1` the branch layout takes) is the exact complement, NaN included.
pub(in crate::ir_lower) fn fcmp_fused_branch(cc: CmpOp, nan_greater: bool) -> Option<(bool, u8)> {
    const JB: u8 = 0x82;
    const JAE: u8 = 0x83;
    const JBE: u8 = 0x86;
    const JA: u8 = 0x87;
    let jcc = match (cc, nan_greater) {
        (CmpOp::Gt, false) => JA,
        (CmpOp::Ge, false) => JAE,
        (CmpOp::Lt, false) => JB,
        (CmpOp::Le, false) => JBE,
        // `UCOMIS b, a`: `a > b` is `b < a`, and NaN must answer true.
        (CmpOp::Gt, true) => JB,
        (CmpOp::Ge, true) => JBE,
        // ...and here NaN must answer false.
        (CmpOp::Lt, true) => JA,
        (CmpOp::Le, true) => JAE,
        (CmpOp::Eq | CmpOp::Ne, _) => return None,
    };
    Some((nan_greater, jcc))
}

/// `(cell, payload32, payload64)`: the byte offsets, within its class's
/// statics block, of static field `field_index`'s 16-byte `Value` cell and of
/// the cell's 32- and 64-bit payload words; `None` past a disp32. The one place
/// the inline `getstatic` and `putstatic` arms (`Lowerer::emit_inline_getstatic`,
/// `Lowerer::emit_inline_putstatic`) derive them, so the two cannot disagree
/// and the layout-constant inventory (`layout_const_inventory.rs`) counts one
/// site for both, as it did for the `getstatic` arm alone. A statics block
/// has no object header, so no `HEADER_SIZE` term.
pub(in crate::ir_lower) fn static_cell_offsets(field_index: u32) -> Option<(i32, i32, i32)> {
    let cell = i32::try_from((field_index as usize).saturating_mul(SLOT_SIZE)).ok()?;
    Some((
        cell,
        cell.checked_add(FIELD_CELL_PAYLOAD32_OFFSET as i32)?,
        cell.checked_add(FIELD_CELL_PAYLOAD64_OFFSET as i32)?,
    ))
}



/// Round 16 (lane fieldguard, FG16-1): the bytecode pcs that exactly one live
/// node outside [`op_cannot_deopt`] carries. A deopt at such a pc resumes the
/// interpreter at an instruction this body has done nothing of yet, when the
/// guard precedes the node's own effects; a pc two such nodes share (an
/// intrinsic expansion of one `invoke`, an unrolled copy) is left out.
pub(in crate::ir_lower) fn op_is_readonly_trapping(op: &Op) -> bool {
    matches!(
        op,
        Op::Load(_)
            | Op::ArrayLength
            | Op::ArrayLoad(_)
            | Op::Guard { .. }
            | Op::CheckCast { .. }
            | Op::Div
            | Op::Rem
            | Op::InstanceOf {
                header_only: true,
                ..
            }
    ) || matches!(op, Op::Unbox { op, .. } if op.is_load())
}

pub(in crate::ir_lower) fn layout_guard_lone_pcs(
    graph: &Graph,
) -> std::collections::HashSet<usize> {
    let mut owners: HashMap<usize, (u32, u32)> = HashMap::new();
    for n in &graph.nodes {
        if n.op == Op::Dead || op_cannot_deopt(&n.op) || guard_const_true_cannot_trap(graph, n) {
            continue;
        }
        if let Some(pc) = n.bytecode_pc {
            let entry = owners.entry(pc).or_insert((0, 0));
            entry.0 += 1;
            if !op_is_readonly_trapping(&n.op) {
                entry.1 += 1;
            }
        }
    }
    owners
        .into_iter()
        .filter(|&(_, (total, side_effects))| total == 1 || (total >= 1 && side_effects == 0))
        .map(|(pc, _)| pc)
        .collect()
}

/// The pcs every owner of which (outside [`op_cannot_deopt`]) commits nothing
/// -- a field or array read, a length, a guard -- and runs at most once per
/// activation (its block is on no cycle). A deopt at one of them re-runs no
/// effect the body committed, which is all [`layout_guard_lone_pcs`] asks of
/// a lone pc; the once rule keeps out unrolled copies, whose shared by-bci
/// frame names another trip. The shape: a loop pre-header's hoisted reads and
/// class predicates, all at the header's pc.
pub(in crate::ir_lower) fn pure_once_shared_pcs(graph: &Graph, schedule: &Schedule) -> std::collections::HashSet<usize> {
    let cycle = blocks_on_a_cycle(schedule);
    let mut pure_once: HashMap<usize, bool> = HashMap::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        if n.op == Op::Dead || op_cannot_deopt(&n.op) {
            continue;
        }
        let Some(pc) = n.bytecode_pc else { continue };
        let pure = matches!(
            n.op,
            Op::Load(_) | Op::ArrayLoad(_) | Op::ArrayLength | Op::Guard { .. }
        );
        let once = schedule
            .node_to_block
            .get(id)
            .is_some_and(|&b| !cycle.get(b).copied().unwrap_or(true));
        *pure_once.entry(pc).or_insert(true) &= pure && once;
    }
    pure_once
        .into_iter()
        .filter(|&(_, ok)| ok)
        .map(|(pc, _)| pc)
        .collect()
}

/// `cycle[x]`: block `x` can reach itself through its successors.
pub(in crate::ir_lower) fn blocks_on_a_cycle(schedule: &Schedule) -> Vec<bool> {
    let blocks = &schedule.blocks;
    let n = blocks.len();
    (0..n)
        .map(|x| {
            let mut seen = vec![false; n];
            let mut work: Vec<usize> = blocks[x].successors.clone();
            while let Some(s) = work.pop() {
                if s == x {
                    return true;
                }
                if s >= n || seen[s] {
                    continue;
                }
                seen[s] = true;
                work.extend(blocks[s].successors.iter().copied());
            }
            false
        })
        .collect()
}



/// Per bci, the distinct blocks defining a value that a BY-BCI snapshot there
/// names: a snapshot that is not a splice chain (`build_deopt_points` skips
/// those) and that no live node claims through [`Node::frame_snapshot`] (a
/// claimed one anchors through `snapshot_native`, never `bci_native`). A value
/// with no scheduled block (a parameter, an unscheduled constant) adds none.
/// A bci whose snapshots name no scheduled value maps to an empty list, which
/// every owner satisfies.
pub(in crate::ir_lower) fn bci_anchor_value_blocks(graph: &Graph, schedule: &Schedule) -> HashMap<usize, Vec<usize>> {
    let nb = schedule.blocks.len();
    let mut claimed = vec![false; graph.safepoints.len()];
    for n in &graph.nodes {
        if n.op == Op::Dead {
            continue;
        }
        if let Some(c) = n.frame_snapshot.and_then(|si| claimed.get_mut(si as usize)) {
            *c = true;
        }
    }
    let mut out: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, sp) in graph.safepoints.iter().enumerate() {
        if sp.is_splice_state() || claimed.get(i).copied().unwrap_or(false) {
            continue;
        }
        let blocks = out.entry(sp.bci).or_default();
        for &v in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
        {
            if v == NO_NODE {
                continue;
            }
            let Some(vn) = graph.nodes.get(v as usize).filter(|n| n.op != Op::Dead) else {
                continue;
            };
            // A φ is defined at its merge's block (the scheduler places no φ
            // in a block's node list); every other value at its own block.
            let at = if matches!(vn.op, Op::Phi) {
                vn.input_opt(0).unwrap_or(NO_NODE)
            } else {
                v
            };
            let Some(b) = schedule.node_to_block.get(at as usize).copied() else {
                continue;
            };
            if b < nb && !blocks.contains(&b) {
                blocks.push(b);
            }
        }
    }
    out
}

/// `CRATONVM_JIT_IR_LAYOUT_GUARD_LONE_PER_FRAME=1` (opt-in; JIT round 17 wave
/// 3, lane irlower; item 3 of
/// `r17-monitor2-by-bci-readers-that-stay-by-bci-FIXED-20261005.md`): count the
/// deopting layout guard's lone sites per `(pc, own frame)`
/// ([`layout_guard_lone_sites`]) instead of per pc. Inert unless
/// `CRATONVM_JIT_IR_LAYOUT_GUARD_DEOPT` is on. Read once per compile.
pub(in crate::ir_lower) fn ir_layout_guard_lone_per_frame_enabled() -> bool {
    true
}

/// [`layout_guard_lone_pcs`] counted per `(pc, own frame)`: a node outside
/// [`op_cannot_deopt`] that names its OWN frame at its pc
/// ([`node_names_own_frame_at_its_pc`]: a per-copy frame of an unrolled or
/// coarsened body, an LICM pre-header frame) is keyed by that frame, every
/// other node by `(pc, None)`. A key one node owns is lone.
///
/// Why that is the lone-pc rule's own argument, per copy. The rule exists so
/// that the deopt re-executes nothing but the site itself: no other effectful
/// node of the SAME program point ran before the guard. Each copy's frame
/// describes a distinct program point (that copy's iteration), and the guard
/// deopts with that frame (`emit_deopt_unless` resolves through
/// `cur_node_frame` first), so what the interpreter re-executes is the copy's
/// own instruction; earlier copies' effects are complete and their results
/// are in the frame. Two effectful nodes of one copy at one pc (an intrinsic
/// expansion inside a copy) share a key and stay non-lone, as two unnamed
/// nodes at one pc always did. An unnamed node keeps the by-bci frame, which
/// describes the original's program point, and is lone when it is the only
/// unnamed owner.
pub(in crate::ir_lower) fn layout_guard_lone_sites(
    graph: &Graph,
) -> std::collections::HashSet<(usize, Option<u32>)> {
    let mut owners: HashMap<(usize, Option<u32>), (u32, u32)> = HashMap::new();
    for n in &graph.nodes {
        if n.op == Op::Dead || op_cannot_deopt(&n.op) || guard_const_true_cannot_trap(graph, n) {
            continue;
        }
        if let Some(pc) = n.bytecode_pc {
            let own = n
                .frame_snapshot
                .filter(|_| node_names_own_frame_at_its_pc(graph, n));
            let entry = owners.entry((pc, own)).or_insert((0, 0));
            entry.0 += 1;
            if !op_is_readonly_trapping(&n.op) {
                entry.1 += 1;
            }
        }
    }
    owners
        .into_iter()
        .filter(|&(_, (total, side_effects))| total == 1 || (total >= 1 && side_effects == 0))
        .map(|(key, _)| key)
        .collect()
}



/// JIT round 21 (lane tl3, TL221-1 of `jit-r21-tl2-proposals.md`; page
/// `jr21-tl2-weakref-refersto-and-get-cost-FIXED-20261015.md`):
/// may a spliced `Reference.referent` read (`Graph::referent_load_pcs`, the
/// body of `Reference.get()`) read inline, behind the SATB pre-gate byte
/// `gate` (`helpers.ref_store_pre_gate`, `Lowerer::satb_pre_gate`), instead of
/// always calling the checked `jit_getfield` helper?
///
/// # Why a clear gate proves the keep-alive owes nothing
///
/// The helper's keep-alive (`jit_getfield_body`'s slot-0 arm) is
/// `if heap.satb_armed() { heap.satb_barrier(referent) }`: an SATB enqueue of
/// the referent, a no-op unless a concurrent mark is running. Every collector
/// that can run one publishes this byte with the arm-before / disarm-after
/// protocol the inline reference stores already rely on:
///
/// * Generational and G1: the process-wide `JIT_REF_STORE_GATES.pre_active`
///   (`gc::gen_heap::jit_satb_pre_gate_addr`), counted up by a `SatbQueue`
///   BEFORE it stores ACTIVE and down AFTER it stores INACTIVE (`satb.rs`
///   `arm_jit_gate` / `disarm_jit_gate`); `satb_barrier` returns at once for
///   an inactive queue;
/// * ZGC: the heap's own `JitRefStoreGates::pre_active`, raised before
///   `mark_active` and lowered after (`ZgcHeap::set_mark_active`);
///   `satb_pre_barrier` returns at once while `mark_active` is false.
///
/// The byte is tested AFTER the read, which is HotSpot's own shape for
/// `Reference.get` under G1 (`G1BarrierSetC2`: the referent load, then the
/// SATB enqueue only while marking is active), and it leaves no window:
///
/// * a mark that starts before the read (cooperatively: there is no poll
///   between the read and the test; or by a take-over that froze this peer
///   before the read) has armed the byte, so the test sends the read to the
///   helper, which re-reads and logs;
/// * a mark that starts after the read finds the referent in this peer's
///   registers or frame: a root of its snapshot (the take-over scans a frozen
///   peer's registers conservatively, `stw_take_over_and_wait`), so it is
///   marked whatever the program then does with it.
///
/// The gated reference stores test BEFORE their store, which is why the ZGC
/// mark start needs its take-over cover (`gc_and_alloc::zgc_satb_window_refuses`)
/// and G1 its frozen-peer field scan; this read needs neither.
///
/// What stays as it was: the read is still volatile-ordered (re-read per
/// iteration, never forwarded), the back-edge dead-home clears still name it,
/// a compact receiver (no row: the legacy-only arm) and every other inline
/// guard still take the helper. A reference node only, and only with a wired
/// gate (`0`: every site keeps the helper, as before).
pub(in crate::ir_lower) fn referent_gated_inline_admitted(node_ty: IrType, gate: usize) -> bool {
    node_ty == IrType::Ref
        && gate != 0
}

/// j27 wave 5 (lane REFS; GC correctness, no switch; page
/// `j27-referent-legacy-arm-skips-the-zgc-resurrection-gate-FIXED-20261029.md`):
/// must a referent read -- `handed_out` (a `Graph::referent_load_pcs` read:
/// `Reference.get`) or `compare_only` (a `Graph::no_keepalive_referent_loads`
/// read: `refersTo`) -- take the checked `jit_getfield` helper because this
/// heap's ZGC referent read gate may arm (`gate_may_arm`,
/// `DirectHelperTable::referent_gate_may_arm`)? Only the helper runs that
/// gate (`gate_referent_read`); every inline arm, the legacy-only one
/// included, would read a referent the pause found dead: handed out, it is
/// freed by the parked sweep while the program holds it; compared,
/// `refersTo(null)` answers `false` where HotSpot answers `true`. The gate
/// can arm only under the opt-in `CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`,
/// which arms no load barrier of its own, so nothing else closes the arms.
///
/// A compare-only read whose every compare is against one value the read
/// sends to the helper when it is null (`compared_value_tested`;
/// `AccessibleObject`'s access-cache `callerRef.refersTo(caller)`) answers the
/// gate's answer inline: the gate only ever answers null for a referent the
/// pause found dead, and a referent equal to a non-null value the program
/// holds is that live value. Only `refersTo(null)` tells a dead referent from
/// a live one, and that one still takes the helper.
pub fn referent_read_owes_the_checked_helper(
    gate_may_arm: bool,
    handed_out: bool,
    compare_only: bool,
    compared_value_tested: bool,
) -> bool {
    gate_may_arm && (handed_out || (compare_only && !compared_value_tested))
}

/// Round 16 (lane fieldguard, FG16-7): the descriptor tag a legacy-only inline
/// READ uses for a node of type `ty` (`0` refuses the site).
///
/// # Why a site with no compact row may read inline at all
///
/// `ir_compact_fields` carries a row only when the field's declaring class has
/// a registered compact layout (`compact_field_disp`). The JDK's
/// `java/util/ArrayList$Itr` has none: its synthetic-stub table pads it to five
/// slots, and `build_compact_layout` refuses any padded class
/// (`r16-arraylist-jdk-arraylist-field-sites-take-the-checked-helper`), so
/// every `cursor`/`lastRet`/`this$0` access was a `jit_getfield` /
/// `jit_putfield_int` CALL. The compact row is what the inline arms need only
/// for their COMPACT half; the legacy half reads the uniform 16-byte `Value`
/// cell at `HEADER_SIZE + index * SLOT_SIZE`, which depends on no class layout
/// at all. So without a row the site emits that half alone, guarded by:
///
/// * the per-object compact bit -- set means "compact layout registered after
///   this compile resolved its rows", which has no displacement here, so the
///   checked helper;
/// * `num_slots > index` on the legacy header -- natives mint instances with an
///   explicit slot count (`alloc_concurrent_synthetic`), and a short one goes
///   to the helper, which bounds-checks, instead of reading past the object;
/// * for a reference, the cell's tag (`FIELD_CELL_TAG_OBJECT`), exactly as the
///   compact-row arm's legacy half checks it.
///
/// No layout-epoch guard: nothing baked depends on a layout. The width comes
/// from the node's type, which the builder took from the field descriptor:
/// every int-category legacy payload is the narrowed `Value::Int`, read back
/// sign-extended exactly as the helper answers.
pub(in crate::ir_lower) fn legacy_only_field_tag(ty: IrType) -> u8 {
    match ty {
        IrType::Ref => b'L',
        IrType::Int => b'I',
        IrType::Long => b'J',
        IrType::Float => b'F',
        IrType::Double => b'D',
        _ => 0,
    }
}

/// [`legacy_only_field_tag`] for a STORE of `kind` (the value is already
/// narrowed to a sub-int field's width by the builder).
pub(in crate::ir_lower) fn legacy_only_store_tag(kind: MemKind) -> u8 {
    match kind {
        MemKind::Int => b'I',
        MemKind::Long => b'J',
        MemKind::Float => b'F',
        MemKind::Double => b'D',
        _ => 0,
    }
}

/// The legacy (long) header's field-count word: the one place the legacy-arm
/// bounds checks take `NUM_SLOTS_OFFSET` from (the layout-constant inventory
/// counts it once for both).
pub(in crate::ir_lower) fn legacy_num_slots_disp() -> i32 {
    cratonvm_types::NUM_SLOTS_OFFSET as i32
}

/// The legacy (long-header) displacement of field `field_index`'s 16-byte
/// `Value` cell, `HEADER_SIZE + field_index * SLOT_SIZE` -- the inline
/// `getfield`'s legacy arm's, compact row or not (FG16-7).
pub(in crate::ir_lower) fn legacy_cell_disp(field_index: usize) -> i32 {
    (HEADER_SIZE + field_index * SLOT_SIZE) as i32
}



/// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS` (JIT round 25, item 4 of the r24
/// regex-rows redesign) -- **default ON** since R25-FLIP, `=0` kills; it makes an inline field access
/// on a `Ref`-typed receiver prove the receiver's compactness ONCE, with a
/// deopting `GC_FLAG_COMPACT` guard, and lets every access that proof
/// dominates read or write the field with one plain `MOV` -- no compact test,
/// no legacy arm, no null test, and (through the deopting layout guard it
/// brings with it, `CRATONVM_JIT_IR_LAYOUT_GUARD_DEOPT`'s form) no epoch
/// compare. See `Lowerer::proven_receiver_mode`. Read once per compile; no
/// static. The optimizer reads it too, to fill
/// `Graph::proven_compact_withdrawn_pcs` from the de-spec registry.
pub(crate) fn ir_proven_receiver_fields_enabled() -> bool {
    // R25-FLIP: default ON; `=0` is the kill switch.
    true
}



/// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_DOMINANCE` (JIT round 25, item 4
/// wave 2) -- **default ON** since R25-FLIP, `=0` kills; inert without
/// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS` (also default ON): field proofs (compactness and
/// layout epoch) are read across blocks by dominance in bodies with loops and
/// exception entries too, under `Lowerer::field_proof_reaches`' entry and
/// identity rules, instead of only in loop-free bodies with no exception
/// entry. Read once per compile.
pub(in crate::ir_lower) fn ir_proven_receiver_fields_dominance_enabled() -> bool {
    // R25-FLIP: default ON; `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_COESTABLISH` (JIT round 25 wave 6,
/// lane W6-E) -- **default ON since j27 2026-10-28** (`=0` kills); inert without
/// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`. An establishing inline `getfield`
/// on another value, in a block on no loop, also proves the compiling
/// method's RECEIVER (`this`, never null) compact with a second deopting test
/// at the same bci, when a field site on the receiver lies in a block it
/// dominates: `this.f` reads that used to establish their own proof inside a
/// loop body (one test per iteration) read with one `MOV`. See
/// `Lowerer::coestablish_receiver_proof`. Read once per compile.
pub(in crate::ir_lower) fn ir_proven_receiver_fields_coestablish_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_PREHEADER` (JIT round 25, item 4
/// residual 2) -- **default ON**, `=0` kills; inert without
/// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_DOMINANCE`: a field proof put down
/// before a loop covers the loop's body. Each ordinary OSR entry that jumps
/// past the guard re-checks it in its stub from the local that names the
/// receiver (`Lowerer::field_proof_reaches`,
/// `Lowerer::osr_entry_field_proof_checks`), or is refused. Read once per
/// compile.
pub(in crate::ir_lower) fn ir_proven_receiver_fields_preheader_enabled() -> bool {
    true
}

/// Item 4 residual 2: how many φs [`Lowerer::field_proof_canon`] walks
/// before it gives up and answers the value itself.
pub(in crate::ir_lower) const FIELD_PROOF_CANON_MAX_WEB: usize = 16;



/// How an inline field access treats the receiver's compactness under
/// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`; see
/// `Lowerer::proven_receiver_mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum ProvenReceiverMode {
    /// A dominating deopting compactness guard covers the site.
    Proven,
    /// The site puts the deopting guard down itself.
    Establish,
}

/// Per-body census of the inline field accesses under
/// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS` (item 4), printed as
/// `[cratonvm-jitc] ir proven-receiver-fields: ...` under `CRATONVM_DBG_JITC`.
/// `*_full` sites emitted the whole sequence (compact test, legacy arm,
/// helper); `*_establish` put the deopting compactness guard down;
/// `*_proven` read or wrote with no compactness test at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ir_lower) struct ProvenFieldCensus {
    pub(in crate::ir_lower) load_full: usize,
    pub(in crate::ir_lower) load_establish: usize,
    pub(in crate::ir_lower) load_proven: usize,
    pub(in crate::ir_lower) store_full: usize,
    pub(in crate::ir_lower) store_establish: usize,
    pub(in crate::ir_lower) store_proven: usize,
    /// Wave 2: how many of the `*_establish` guards sit in a spliced body.
    pub(in crate::ir_lower) establish_spliced: usize,
    /// Wave 2: reference stores whose epoch compare or compactness test a
    /// proof removed (`emit_gated_ir_ref_putfield`, `emit_g1_ir_ref_putfield`).
    pub(in crate::ir_lower) ref_store_proven: usize,
    /// Item 4 residual 3: gated (non-G1) reference stores that put the
    /// deopting compactness guard down (also counted in `store_establish`).
    pub(in crate::ir_lower) gated_ref_store_establish: usize,
    /// Item 4 residual 2: OSR entries whose stub re-checks a field proof it
    /// jumps past.
    pub(in crate::ir_lower) osr_entry_rechecks: usize,
    /// W6-E: receiver proofs put down beside another value's establishing
    /// read (`Lowerer::coestablish_receiver_proof`).
    pub(in crate::ir_lower) coestablished: usize,
}

/// `CRATONVM_JIT_IR_LAYOUT_GUARD_DEOPT` (round 16, lane fieldguard, FG16-1) --
/// **default OFF**; `1` makes the first inline field site of a receiver guard
/// the layout epoch with a DEOPT exit instead of a rejoining helper branch, and
/// lets later sites that guard dominates drop their epoch compare
/// (`Lowerer::emit_field_layout_guard`). Read per compile; no static.
pub(in crate::ir_lower) fn ir_layout_guard_deopt_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_INLINE_PUTSTATIC` (round 16, lane fieldguard) -- **default
/// ON since round 16** (measured: static stores 5-13x, `static-calls`
/// 16.1→1.7 ns, SyncM sp 29→7 ns); `0`/`false`/`off`/`no` turns it off. On,
/// it emits a primitive `putstatic` of an initialized class as a
/// direct store into its statics cell in BOTH tiers (this file's
/// `Lowerer::emit_inline_putstatic`, the single-pass
/// `x64/op_field.rs` `try_emit_inline_putstatic`) instead of a
/// `jit_putstatic_*` call. Read per site at compile time only; no static.
pub(crate) fn inline_putstatic_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_INLINE_PUTSTATIC_REF` (JIT r20 wave 17, lane pAlloc2;
/// **default ON**, `=0` keeps the `jit_putstatic_object` call): the
/// optimizing tier stores a reference static inline while the SATB pre-gate
/// is clear (`Lowerer::emit_inline_putstatic_ref`). Needs
/// `CRATONVM_JIT_INLINE_PUTSTATIC` too. Read per site at compile time.
pub(crate) fn ir_inline_putstatic_ref_enabled() -> bool {
    true
}





/// JIT round 19 proposal wave 5 (lane mega5, MEGA19-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`): `CRATONVM_JIT_IR_MEGA_RETIRE_INT_HOME_EXTENSION=1`.
/// `emit_inline_cache_call` no longer walks the SBR4 in-place `int` home
/// extension loop before the hashed stub, which finds nothing to extend since
/// round 19's staging-word fix
/// (`docs/feature-designs/jit-ir-optimizations.md`):
/// every non-constant `int` the stub reads comes from its sign-extended staging
/// word (`ic_stub_arg_reads_staging_word`). Byte-identical code in both arms;
/// a debug assertion keeps the reason checked. default on (a switch-off-able
/// retirement); read once per hashed-stub site.
pub(crate) fn ir_mega_retire_int_home_extension_enabled() -> bool {
    true
}

/// Let a megamorphic call site name a homeless argument's STAGING slot.
///
/// Default-ON. Opt out with `CRATONVM_NO_JIT_STAGED_ARG_SLOT=1`, which
/// restores the `slot_of` read — and with it the `unallocated_value` bail that
/// dropped any method with a register-resident argument at a megamorphic site
/// to the single-pass backend. A bisection lever, not a tuning knob: what it
/// restores is the refusal, so the before and after are two runs of one binary.
pub(crate) fn staged_arg_slot_enabled() -> bool {
    // j27 MEMOSLOT: a `MemoSlot`, so a test override reaches it.
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    crate::memo_flag::memo_bool(&G, || {
        !false
    })
}





/// Inline TLAB bump for the optimizing tier's `Op::New` — **default ON** since
/// round 9 wave 4 (irlower4); `CRATONVM_JIT_IR_INLINE_TLAB=0` (or `false`/
/// `off`/`no`) is the kill switch and restores `emit_new_object_stub` alone,
/// which is what this arm emitted before the bump existed, so the two stay
/// A/B-able in one binary.
///
/// # Why it flipped (measured, `NOTES-w4-irlower4.md`)
///
/// Until wave 3 the bump was unreachable by default anyway:
/// `c2_alloc_upgrade_enabled()` is opt-in, so no allocation-bearing method
/// reached this tier. The default-on entry counter (`crate::entry_counter`,
/// wave 3) changed that. Hot allocating methods now reach C2 through the
/// hotness door, so this flag decides between a stub CALL and an inline bump
/// in every such method.
///
/// Measured on the wave-3 binary, `=0` against `=1`, interleaved, 3 reps, with
/// engagement confirmed per run (`inline_tlab_bump=N stub_only=0` against
/// `inline_tlab_bump=0 stub_only=N`, the census in
/// `runtime_lowering::ir_alloc_site_counts`):
///
/// * every output matched HotSpot, across IrEscapeProbe, ScalarReplProbe,
///   SpliceCastProbe, CratonBenchC2, CratonBench `bintrees`/`hashmap`, a new
///   six-thread allocation probe (`MtTlabProbe`) and `RJitMapTierDiff`;
/// * soak with the bump on: `RJitMapTierDiff` 30/30, `MtTlabProbe` 10/10 on
///   ZGC, and 4/4 on the generational collector. That is 0 failures in 90+
///   engaged runs since the 4-in-10 of 2026-09-02, which the
///   `IN_OWN_TLAB_RETIRE` double-free fix most likely explains;
/// * time: IrEscapeProbe and CratonBenchC2 slightly faster, everything else
///   within noise (tiering3 measured -8% on IrEscapeProbe on top of the entry
///   counter).
///
/// On a registering collector (ZGC) the fast path calls `tlab_post_init` (or
/// the thin announce helper), which announces the object. Elsewhere a class
/// with no post-init work skips the call ([`ir_tlab_skip_post_init_enabled`]).
///
/// # History: why it was opt-in
///
/// It flipped back to opt-in on 2026-09-02 after `RJitMapTierDiff` SIGSEGV'd
/// 4 runs in 10 once ZGC mutators got TLAB chunks. The body below keeps that
/// record. A doc line once called it "default ON" for two days after the body
/// went opt-in, and cost a census. This line and the body now say the same
/// thing, and `r9w4_the_ir_inline_tlab_bump_is_default_on_with_a_kill_switch`
/// pins it.
pub(in crate::ir_lower) fn ir_inline_tlab_enabled() -> bool {
    // 2026-09-02, the eight-finding pass: BACK TO OPT-IN, with a repro.
    //
    // This bump had never executed. Its own gate was shut, and underneath that
    // `VmHeap::refill_tlab` answered `None` on the default collector, so
    // `thread.tlab` was always empty and the limit compare always failed.
    // Giving ZGC mutators a chunk made it live for the first time, and
    // `regression-suite` vector `RJitMapTierDiff` then SIGSEGVs 4 runs in 10,
    // inside VM code, on a reference read back as a small integer (`0x2800`)
    // — i.e. an object whose contents are not what its allocator promised.
    //
    // Three switches each take the crash rate to zero, and they are the three
    // that gate this sequence executing at all: `CRATONVM_ZGC_MUTATOR_TLAB=0`
    // (no chunk to bump), this flag (the stub instead), and
    // `CRATONVM_JIT_C2_ALLOC_UPGRADE=0` (no allocating method reaches this
    // tier). It is NOT the `Op::New` header writes, which were read out of a
    // disassembly and match the single-pass sequence field for field, and it
    // is NOT relocation (4/4 with `CRATONVM_ZGC_RELOCATE=0`). One real defect
    // WAS found and fixed on the way (`IN_OWN_TLAB_RETIRE`, a double free of
    // the reserved tail); it is not this one.
    //
    // So: the capability stayed, behind its switch, with the repro written
    // down — and `c2_alloc_upgrade_enabled` stayed opt-in with it.
    //
    // Round 9 wave 4: DEFAULT ON. The repro has not reproduced in 90+ engaged
    // runs since (see the doc above), and the entry counter now brings hot
    // allocating methods to this tier by default, where the stub is a CALL
    // per allocation. `=0` restores the stub.
    true
}





/// `CRATONVM_JIT_IR_NEWARRAY_MAP_SINK` (JIT round 22 wave 1, lane memory;
/// ALLOC21-1 of `jit-r21-alloc-tier1-proposals-RETIRED-20261025.md`; perf, **default on**): on a
/// collector that parses the chunk (Generational, G1) an inline `Op::NewArray`
/// bump emits its safepoint map on the slow path only, as `Op::New` does under
/// `CRATONVM_JIT_IR_ALLOC_MAP_SINK`. See `Lowerer::try_lower_newarray_map_sink`.
/// Read per array site, at compile time.
pub(in crate::ir_lower) fn ir_newarray_map_sink_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_ALLOC_MAP_SINK_ZGC` (JIT round 22 wave 2, lane memory;
/// MEMORY22-1 of `jit-r22-memory-proposals.md`; perf, **default on**): on ZGC
/// the optimizing tier's announced inline `new` (with
/// `CRATONVM_JIT_IR_ALLOC_MAP_SINK`) and inline `newarray` / `anewarray` (with
/// `CRATONVM_JIT_IR_NEWARRAY_MAP_SINK`) emit their safepoint map on the bump's
/// slow path only. The fast path's one call is the thin announce helper, a
/// leaf; see the `Op::New` arm's `deferred_zgc`. The split arm
/// (`CRATONVM_JIT_ZGC_INLINE_ARM_FALL_THROUGH`) keeps its map. Read per site,
/// at compile time.
pub(in crate::ir_lower) fn ir_alloc_map_sink_zgc_enabled() -> bool {
    true
}

/// JIT round 21, lanes alloc + oom: where an `Op::New` site's held dead-home
/// clears (I-9) are emitted, given its route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum NewSiteClears {
    /// A sunk route (the deferred `skip_post_init` bump, or the ZGC split):
    /// on each slow arm, in front of that arm's map
    /// (`Lowerer::emit_alloc_sunk_slow_arms` / the deferred slow path). The
    /// fast path makes no call and emits none.
    SlowArms,
    /// Before the route's one map, which precedes the bump and so covers both
    /// paths: the stub-only route, and the post-init bump under
    /// `CRATONVM_JIT_IR_HELPER_SITE_DEAD_HOME_CLEAR` (lane oom).
    FrontOfMap,
    /// The post-init bump with the oom switch off: the site is given up
    /// (`Lowerer::disarm_alloc_site`), byte for byte the I-9 behaviour.
    Disarm,
}

/// The [`NewSiteClears`] rule. `sunk`: the deferred or ZGC split route was
/// taken (its slow arms own every GC point); `bump_offered`: the post-init
/// inline bump will be emitted in place (`ir_inline_tlab_enabled()` and a
/// class proven initialised); `helper_site_clear`: lane oom's
/// `Lowerer::helper_site_dead_home_clear`.
pub(in crate::ir_lower) fn new_site_clear_placement(
    sunk: bool,
    bump_offered: bool,
    helper_site_clear: bool,
) -> NewSiteClears {
    if sunk {
        NewSiteClears::SlowArms
    } else if bump_offered && !helper_site_clear {
        NewSiteClears::Disarm
    } else {
        NewSiteClears::FrontOfMap
    }
}

/// Round rx27 wave 1 (lane alloc): the compile-time facts of one `Op::New`
/// site that decide whether its FAST path calls into Rust, for the
/// `ir new-site ... call=` census ([`new_site_call_reason`]).
#[derive(Clone, Copy, Debug)]
pub(in crate::ir_lower) struct NewSiteFacts {
    /// `ir_inline_tlab_enabled()` (`CRATONVM_JIT_IR_INLINE_TLAB`).
    pub(in crate::ir_lower) inline_tlab: bool,
    /// `Lowerer::new_class_proven_initialized` (or a lambda proxy).
    pub(in crate::ir_lower) proven_initialized: bool,
    /// Some inline bump was emitted (sunk, split, or in place); `false` is the
    /// stub-only route.
    pub(in crate::ir_lower) bump_emitted: bool,
    /// The site's class owes `jit_post_tlab_init` nothing.
    pub(in crate::ir_lower) post_init_noop: bool,
    /// The `new` sits in a spliced body.
    pub(in crate::ir_lower) spliced: bool,
    /// `InlineTlabPlan::skip_post_init`.
    pub(in crate::ir_lower) skip_post_init: bool,
    /// `InlineTlabPlan::announce != 0` (ZGC thin announce).
    pub(in crate::ir_lower) announce: bool,
    /// `x64::ir_zgc_inline_announce_enabled()`: the announce is the inline
    /// start-bit store, with the helper only on its refusals.
    pub(in crate::ir_lower) inline_start_bit: bool,
}

/// Why a compiled `new` site pays a Rust call on EVERY allocation, or `none`.
///
/// * `inline-tlab-off`, `class-not-proven-initialised`, `bump-declined`: the
///   stub route, `jit_new_object` per allocation (which runs the class-init
///   memo, the collector's start checks, the young-headroom probe, a TLAB
///   allocation and `jit_post_alloc_init`). A `bump-declined` site is preceded
///   by an `ir inline-TLAB bump DECLINED: <why>` line.
/// * `class-owes-post-init` (own site: the VM's `jit_new_site_flags` reports a
///   legacy-layout reference / wide field, a finalizer or a `java.lang.ref`
///   class) and `spliced-class-owes-post-init` (a spliced site without a
///   compact layout, or with the finalizable-splice refusal off): the bump,
///   then `jit_post_tlab_init` -> `jit_post_alloc_init` per allocation.
/// * `zgc-inline-start-bit-off`: the bump, then `zgc_note_tlab_object` per
///   allocation (`CRATONVM_JIT_IR_ZGC_INLINE_ANNOUNCE=0` or
///   `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE=0`).
/// * `post-init-elision-off`: a no-op class still calls `jit_post_tlab_init`
///   (`CRATONVM_JIT_IR_ZGC_ANNOUNCE=0` on ZGC, `CRATONVM_JIT_IR_TLAB_SKIP_POST_INIT=0`
///   elsewhere).
/// * `none`: the fast path makes no call. On ZGC the start-bit store's helper
///   arm still runs when the store refuses (the table `blocked` while marking
///   or generational, a stale ownership epoch, a word outside the owned
///   window), and every route calls `jit_new_object` when the TLAB is full.
pub(in crate::ir_lower) fn new_site_call_reason(f: NewSiteFacts) -> &'static str {
    if !f.inline_tlab {
        "inline-tlab-off"
    } else if !f.proven_initialized {
        "class-not-proven-initialised"
    } else if !f.bump_emitted {
        "bump-declined"
    } else if f.skip_post_init || (f.announce && f.inline_start_bit) {
        "none"
    } else if f.announce {
        "zgc-inline-start-bit-off"
    } else if !f.post_init_noop && f.spliced {
        "spliced-class-owes-post-init"
    } else if !f.post_init_noop {
        "class-owes-post-init"
    } else {
        "post-init-elision-off"
    }
}

/// Round rx27 wave 1 (lane alloc): the [`NewSiteFacts`] of an
/// `Op::NewArray` site, for the `ir newarray-site ... call=` census
/// ([`newarray_site_call_reason`]).
#[derive(Clone, Copy, Debug)]
pub(in crate::ir_lower) struct NewArraySiteFacts {
    /// The `Op::NewArray` arm's `inline_offered`.
    pub(in crate::ir_lower) offered: bool,
    /// An `anewarray` (`element_type == 0`).
    pub(in crate::ir_lower) reference: bool,
    /// `cratonvm_types::jit_tlab_registration_required()` (ZGC).
    pub(in crate::ir_lower) registration: bool,
    /// `runtime_lowering::ir_inline_anewarray_any_gc_enabled()`.
    pub(in crate::ir_lower) anewarray_any_gc: bool,
    /// The bump was emitted (split, fast join or in place).
    pub(in crate::ir_lower) bump_emitted: bool,
    /// `x64::ir_zgc_inline_announce_enabled()`.
    pub(in crate::ir_lower) inline_start_bit: bool,
}

/// [`new_site_call_reason`] for an array site. `anewarray-any-gc-off`: a
/// reference array on Generational / G1 with
/// `CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC=0`; `inline-array-off`: a
/// kill switch of the array bump (`CRATONVM_JIT_IR_INLINE_TLAB`,
/// `CRATONVM_JIT_INLINE_TLAB_ANEWARRAY`, `CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE`,
/// `CRATONVM_JIT_IR_INLINE_TLAB_NEWARRAY`) or no announce helper wired. Both
/// call `jit_newarray` / `jit_anewarray_object` per allocation, as does a
/// `bump-declined` site. `none` still takes the stub for a length past
/// `IR_INLINE_ARRAY_TLAB_MAX_ALLOC` (32 KiB) or a full TLAB.
pub(in crate::ir_lower) fn newarray_site_call_reason(f: NewArraySiteFacts) -> &'static str {
    if !f.offered {
        if f.reference && !f.registration && !f.anewarray_any_gc {
            "anewarray-any-gc-off"
        } else {
            "inline-array-off"
        }
    } else if !f.bump_emitted {
        "bump-declined"
    } else if f.registration && !f.inline_start_bit {
        "zgc-inline-start-bit-off"
    } else {
        "none"
    }
}



/// Is `jit_post_tlab_init` a no-op for an inline-bumped instance of
/// `class_id` allocated by a `new` in a SPLICED body (round 13 wave 8, lane
/// iropt7)?
///
/// `lib.rs` admits a caller's own site to `tlab_announce_classes` on the VM's
/// `jit_new_site_flags` answer: initialised, no `long`/`float`/`double`
/// instance field anywhere in the hierarchy (the only fields whose default
/// needs a non-zero `Value` tag), no finalizer. A spliced site's class comes
/// from the splice resolver's `ir_new_info`, which carries no flags, so the
/// same three facts are established here instead:
///
/// * initialised: the lowerer's own `new_class_proven_initialized` gates the
///   bump at all (the splice resolver refuses an uninitialised class);
/// * no finalizer: the splice resolver refuses a finalizable class
///   (`ir-splice-new-site-finalizable`) under
///   `CRATONVM_JIT_IR_SPLICE_REFUSES_FINALIZABLE_NEW`, read here too, so
///   turning that off turns this off;
/// * no wide primitive field: read off the class's CURRENT compact layout,
///   which lists every instance field (inherited ones included) with its
///   storage kind. Only when the emitter bakes that layout (the same
///   `compact_tlab_total_size` answer it sizes the bump by, which also
///   refuses a multi-domain process); a legacy-shaped class keeps the call.
///   zki: under `CRATONVM_JIT_COMPACT_NEW_ZERO_PRIM` (default ON) a wide
///   field is no obstacle either: its compact cell's zero bytes are the
///   typed default (`jit_post_alloc_init_is_noop_compact`).
///
/// With all three the helper only re-stores the flags byte the bump wrote
/// and registers nothing (`jit_post_alloc_init_is_noop`), exactly the case
/// the own-site `skip_post_init` already relies on.
pub(in crate::ir_lower) fn spliced_new_post_init_is_noop(class_id: u32, num_fields: usize) -> bool {
    use cratonvm_types::field_layout::FieldStorageKind;
    // zki (page `zgc-zt25hunt7b-zr2cp-grow-cpu-*` item A): a compact body's
    // long / float / double cells read zero bytes as the typed default, as
    // the single-pass tier's own sites already count them
    // (`CRATONVM_JIT_COMPACT_NEW_ZERO_PRIM`, `jit_new_site_flags`); a spliced
    // `Double.valueOf` paid the whole `jit_post_tlab_init` call per box.
    let compact_zero_prim =
        true;
    cratonvm_types::compact_tlab_total_size(class_id, num_fields).is_some()
        && cratonvm_types::field_layout::with_current_class_layout(class_id, |l| {
            l.field_count() == num_fields
                && l.field_kinds.len() == num_fields
                && (compact_zero_prim
                    || l.field_kinds.iter().all(|k| {
                        !matches!(
                            k,
                            FieldStorageKind::Long
                                | FieldStorageKind::Float
                                | FieldStorageKind::Double
                        )
                    }))
        })
        .unwrap_or(false)
}

/// The G1 inline reference store (`Lowerer::emit_g1_ir_ref_putfield`),
/// `CRATONVM_JIT_IR_G1_REF_STORE`, default on; `0` restores the helper call
/// for every reference store under G1.
pub(in crate::ir_lower) fn ir_g1_ref_store_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_G1_GATED_REF_STORE` (g1put, 2026-10-05) -- default on,
/// `=0` off: the G1 inline reference store keeps an overwrite of a non-null
/// reference inline while the SATB pre-gate is clear, and a cross-region store
/// takes the lean remembered-set barrier instead of the whole helper. See
/// `Lowerer::emit_g1_ir_ref_putfield`. Read per site; no static.
pub(in crate::ir_lower) fn ir_g1_gated_ref_store_enabled() -> bool {
    true
}

/// `JIT_G1_BARRIER` word offsets, as `x64::objects`' `G1B_*` (arena base,
/// region mask). The table is `[AtomicUsize; 5]`; see
/// `x64::licm::g1_barrier_table_live`.
pub(in crate::ir_lower) const G1B_ARENA_BASE: i32 = 0;
pub(in crate::ir_lower) const G1B_REGION_MASK: i32 = 16;

/// r21 barriers (BARRIERS21-2) -- `CRATONVM_JIT_IR_G1_GATED_AASTORE`
/// (default ON since the r23 A/B, `=0` off). Under G1 this tier's reference `aastore`, after its
/// null, bounds and covariance guards, stores inline while the SATB pre-gate
/// byte is clear. A null or same-region value needs no barrier. A
/// cross-region value calls the lean `jit_g1_post_write_barrier`. The default
/// is the whole `jit_aastore` call. Every other case (an armed gate, a site
/// whose covariance is not discharged, narrow oops) keeps that call. See
/// [`Lowerer::emit_g1_ir_aastore`]. Read per site; no static.
pub(crate) fn ir_g1_gated_aastore_enabled() -> bool {
    // R23-AB-FLIP: default ON since the JIT r23 triage A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

