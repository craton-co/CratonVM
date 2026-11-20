#[allow(unused_imports)]
use super::*;


/// [`plan_register_residency`] over a liveness model the caller built and
/// keeps. The only mutation is to `live.pinned` (`pin_values` and the two pin
/// releases), so on return every other field is still exactly
/// `build_live_model(graph, schedule)`'s answer, and `lower_inner_attempt`
/// hands the same model to `emit_osr_entry_stubs` (which reads `converged`,
/// `span` and `range` only) instead of building it a second time
/// (`r11w4-ircore-live-model-built-twice-per-osr-compile`).
pub(in crate::ir_lower) fn plan_register_residency_with(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    live: &mut crate::regalloc::LiveModel,
    // JIT round 25 wave 9 (W9-2): the allocator's effective switch
    // (`r25_w7a_reg_alloc::effective`), the one the lowerer reads too.
    reg_alloc: bool,
    selected: Option<&MirPlan>,
) -> CompileResult<Option<RegResidency>> {
    use crate::regalloc::{
        allocate_linear_scan, verify_allocation, PhysReg, RegClass, RegFile, RegSpec,
    };

    if !live.converged {
        // Every range is the whole method; nothing is promotable and the scan
        // would only burn compile time proving it.
        return Ok(ls_refuse("liveness model did not converge"));
    }

    // ── Release the deopt pins, and why that is legal HERE ───────────
    //
    // Without this the wiring is inert on every real method. `IrBuilder`
    // records a `SafepointSnapshot` of the locals and the operand stack at
    // EVERY bytecode boundary, so every temporary is named by some frame state
    // and `build_live_model` pins the entire value set — `allocate_linear_scan`
    // then promotes nothing at all, on any graph that came from bytecode.
    //
    // The pin's premise is "the home word must hold the value at any recorded
    // bci, and an allocator cannot establish that". This consumer establishes
    // it by construction rather than by analysis: `fp_store_value` emits the
    // home store at EVERY definition, promoted or not, so a deopt frame read
    // out of home words is bit-identical to the one the colourer-only path
    // produces. `build_deopt_points` is not modified and does not need to be.
    //
    // The price, per `release_deopt_pins`' contract, is `Allocation::stack_slot`
    // — which this file must not read, and does not: every home offset comes
    // from `plan_slots`, which keeps every pin, through `alloc_slot_checked`.
    // Since R7 the prohibition is also enforced structurally: the
    // `adopt_home_colouring` call below overwrites `stack_slot` with
    // `plan_slots`' layout, so there is no longer a released-pin colouring in
    // the allocation for a future reader to find.
    // The third pin population `plan_slots` has and `build_live_model` cannot
    // see: scalar-replacement field values (a rematerialising deopt reads them
    // from their home words). Pinned FIRST so the model's pin set is exactly
    // `plan_slots`' before anything is released -- for this consumer the
    // release below takes them straight back off, by the same write-through
    // argument as every deopt pin, so this changes no allocation; it keeps the
    // two pin sets identical by construction for any reader of `live.pinned`
    // in between (`regalloc-live-model-misses-scalar-replacement-pins`).
    live.pin_values(plan.sr_field_values.iter().copied());
    let released = live.release_deopt_pins(graph);
    // Phis too, when their edges publish them (see `emit_phi_copies`).
    if ir_phi_residency_enabled() {
        live.release_phi_pins(graph);
    }
    // Round 13 wave 11 (lane callcost5, proposal CC4-3): the values the
    // refusal loop below is CERTAIN to refuse never enter the scan, so they
    // cannot push a value that IS kept into a register nobody else needed.
    // See `ir_ls_prepin_refused_enabled`.
    if ir_ls_prepin_refused_enabled() {
        let refused = ls_certain_refusals_with(graph, schedule, live, reg_alloc);
        live.pin_values(refused);
    }
    // JIT round 25 (item 1b, `CRATONVM_JIT_IR_LS_CALLER_SAVED`): block-local
    // values that cross no call take a caller-saved register, decided HERE and
    // pinned out of the scan so the callee-saved file goes to the values that
    // do cross one. Installed after every other pass, below. Asked only once
    // the position model is known to match `lower_block`'s numbering.
    let mut caller_saved = if ir_ls_caller_saved_admitted()
        && live.converged
        && live.total_positions
            == schedule
                .blocks
                .iter()
                .map(|b| b.nodes.len() + usize::from(b.terminator.is_some()) + 1)
                .sum::<usize>()
    {
        plan_caller_saved_residency(graph, schedule, plan, live)
    } else {
        CallerSavedPlan::default()
    };
    live.pin_values(caller_saved.assigned.iter().map(|&(id, _)| id as NodeId));
    let mut pre_carried = carried_reservation(
        graph,
        plan,
        live,
        &[],
        &vec![None; graph.nodes.len()],
        &[],
    );
    let pre_cap = 2.min(ir_gp_file().len().saturating_sub(1));
    pre_carried.truncate(pre_cap);
    live.pin_values(pre_carried.iter().map(|&(id, _)| id as NodeId));
    let pre_carried_regs: Vec<u8> = pre_carried.iter().map(|&(_, r)| r).collect();
    // Read-only from here on.
    let live: &crate::regalloc::LiveModel = live;

    // ── Agreement check: two liveness models, one program ────────────
    //
    // `build_live_model` re-implements `plan_slots`' position model rather than
    // calling it (`SlotPlan` and `LiveRange` are private to this file). Two
    // implementations of one model drifting apart is precisely how a register
    // allocator comes to alias two live values, so the agreement is CHECKED
    // before either is used, not documented and hoped for.
    //
    // A disagreement disables promotion entirely. It is not a bailout: the
    // colourer's answer is still correct on its own terms, and it is the one
    // the frame is built from.
    let expected_positions: usize = schedule
        .blocks
        .iter()
        .map(|b| b.nodes.len() + usize::from(b.terminator.is_some()) + 1)
        .sum();
    if live.total_positions != expected_positions {
        return Ok(ls_refuse("position models disagree (live vs schedule)"));
    }
    // Two enumerations of "does this op define a value" have to agree here:
    // `regalloc::ir_op_defines_value` (via `wants_loc`) and `ir_lower`'s own
    // `op_defines_result_slot` (via `node_color`). They are the second and
    // third of the three enumerations the module comment at
    // `the_three_ir_op_enumerations` names, and a disagreement declines the
    // whole method.
    //
    // Name the OP that disagrees, not just the fact. "Declined" sends the
    // reader looking through fifty variants; "declined because `Op::X` is in
    // one enumeration and not the other" is a one-line fix with a test.
    let disagreement = if live.wants_loc.len() != plan.node_color.len() {
        Some("node-count mismatch".to_string())
    } else {
        live.wants_loc
            .iter()
            .zip(plan.node_color.iter())
            .enumerate()
            .find(|(_, (wants, color))| **wants != color.is_some())
            .map(|(id, (wants, _))| {
                let op = graph
                    .nodes
                    .get(id)
                    .map(|node| format!("{:?}", node.op))
                    .unwrap_or_else(|| "<out of range>".to_string());
                let (yes, no) = if *wants {
                    ("liveness", "colourer")
                } else {
                    ("colourer", "liveness")
                };
                format!("n{id} ({op}): {yes} says it wants a home, {no} says it does not")
            })
    };
    if let Some(what) = disagreement {
        return Ok(ls_refuse(&format!(
            "liveness and colourer disagree about which values want a home -- {what}"
        )));
    }

    // ── The register file ────────────────────────────────────────────
    //
    // Two banks, not one. The GP half is what makes an `int` loop counter
    // register-resident; the FP half is unchanged.
    let gp_specs = ir_gp_file().iter().filter(|n| !pre_carried_regs.contains(n)).map(|&n| RegSpec {
        reg: PhysReg::gp(n),
        // RBX and R12–R15 are callee-saved on BOTH ABIs, and this prologue
        // saves every one it hands out (`IR_LOWER_SAVED_GPRS` is the whole
        // file), so `caller_saved` is uniformly false and a value may live
        // across a call. That is exactly the property the GP file was selected
        // for: this backend emits calls constantly and has no reload
        // machinery, so a value whose register did not survive a call could
        // not be promoted at all.
        caller_saved: !IR_LOWER_SAVED_GPRS.contains(&n),
    });
    let xmm_specs = IR_LOWER_LS_XMMS.iter().map(|&n| RegSpec {
        reg: PhysReg::xmm(n),
        // Not "the ABI says so" but "does THIS frame save it", which is the
        // property that matters: a value may stay in a register across a call
        // exactly when the callee is obliged to give it back.
        //
        // XMM2–XMM5 are volatile on both ABIs, so a value in one dies at every
        // call and `caller_saved: true` is what makes the allocator split it.
        // XMM6/XMM7 are volatile on System V too — `IR_LOWER_SAVED_XMMS` is
        // empty there, so they come out `true` as well and nothing changes. On
        // Windows they are non-volatile AND this prologue saves them, so they
        // come out `false` and a value CAN live across a call. That is the
        // whole return on the save area, and it is expressed here rather than
        // in a platform `cfg` because the two facts that make it true — the
        // ABI's and this frame's — are both already in `IR_LOWER_SAVED_XMMS`.
        caller_saved: !IR_LOWER_SAVED_XMMS.contains(&n),
    });
    let regs = RegFile::from_specs(gp_specs.chain(xmm_specs));
    let mut model = ir_lower_machine_model(graph, schedule, live, regs, selected);
    if let Some(selected) = selected {
        selected.add_allocated_requirements(live, &mut model)?;
    }

    let mut alloc = match allocate_linear_scan(graph, live, &model) {
        Ok(alloc) => alloc,
        // Register pressure, an unsatisfiable fixed constraint or an exhausted
        // split budget is a property of the input program, not a compiler bug.
        // Decline to promote and keep the frame layout the colourer already
        // produced; there is no reason to lose the whole optimized body over
        // an optimization that did not fit.
        Err(_) => {
            return Ok(ls_refuse(
                "linear scan declined (pressure, fixed constraint or split budget)",
            ))
        }
    };
    // `allocate_linear_scan` verifies its own result. Re-verify anyway: this
    // call site's guarantee must not rest on an internal detail of another
    // module, and the check is the only thing standing between a bug in the
    // scan and machine code that reads a register two values are in.
    //
    // THIS one propagates. A verifier failure is a compiler bug, and emitting
    // against an allocation nothing proved is exactly the trade that has
    // produced silent heap corruption in this VM before.
    verify_allocation(graph, live, &model, &alloc)?;

    // ── R7: adopt this frame's colouring, do not compare against it ──
    //
    // `ls_color_homes` and `plan_slots` both colour home words and only the
    // second one lays out a frame anybody addresses. They cannot be compared —
    // `NOTES-regalloc.md` §4's proposed `stack_slot[id] == node_color[id]`
    // fires on every SUCCESSFUL allocation, because a value promoted for its
    // whole range consumes no `ls_color_homes` colour and shifts every colour
    // after it, and that shift is the frame-size win rather than a drift. The
    // full argument is at `Allocation::stack_slot`.
    //
    // So hand the emitter's layout over instead. From here every
    // `ValueLoc::Slot` payload `alloc.location_at` produces names a word
    // `emit_safepoint_map` and this file's encoder can address, rather than a
    // word in a parallel numbering that happens to be unread today (the one
    // consumer, `ls_split_candidate`, keeps only the `Reg` arm).
    //
    // That "unread today" is the whole reason to do it now rather than when
    // something breaks. `release_deopt_pins` above is paid for with exactly
    // this: `ls_color_homes` ran against a model whose deopt pins were
    // released, so `alloc.stack_slot` may alias two values a deopt frame names
    // and is NOT a layout anything may emit from. The note at that call says
    // this file "must not read" it. A prohibition is the weakest possible form
    // of that guarantee — it holds until someone adds a reader. Replacing the
    // contents removes the thing that must not be read.
    //
    // The `?` is correct: `adopt_home_colouring` refuses only a layout with no
    // word for a value that spends time in memory, which is a compiler bug in
    // exactly the sense the `verify_allocation` above propagates. It should not
    // be reachable — the agreement check above has already established that
    // `plan.node_color` is `Some` for precisely the nodes `live.wants_loc`
    // marks, and every node with a memory segment is one of those.
    //
    // **`verify_allocation` is deliberately NOT re-run afterwards**, although
    // `adopt_home_colouring`'s own doc comment offers that as the payoff. Proof
    // 6 asks whether two values sharing a home word have disjoint ranges, and
    // it asks it of `live.range` — the ALLOCATOR's liveness, after the pin
    // releases above. `plan_slots` colours against its own ranges and its own
    // (unreleased, therefore larger) pinned set. The agreement check compares
    // the two models' POSITION COUNT and their wants-a-home sets; it does not
    // compare their ranges, so "disjoint under `plan.range`" does not imply
    // "disjoint under `live.range`" by anything checked here. Re-verifying
    // would therefore risk turning a method that compiles correctly today into
    // a `BailoutReason::Internal`, which is a worse trade than leaving proof 6
    // where it is. `verify_slot_colouring` already proves the no-aliasing
    // property of this colouring against the ranges it was computed from.
    alloc.adopt_home_colouring(&plan.node_color, plan.slots)?;

    let n = graph.nodes.len();
    let mut reg_of: Vec<Option<u8>> = vec![None; n];
    let mut gp_reg_of: Vec<Option<u8>> = vec![None; n];
    let mut demoted = 0usize;
    // Per-CAUSE refusal census. A bare "promoted 0 of N" is a count and cannot
    // be acted on: the causes below call for different next steps. `skip_phi`
    // used to be the one that decided whether this file could ever reach a LOOP
    // COUNTER -- which is a phi, at every loop header -- because it meant
    // "refused for BEING a phi". Since phis are admitted it means only "the
    // sub-flag is off", and the question it used to stand for is answered in
    // full by `PhiCensus` below.
    let (mut skip_split, mut skip_bank, mut skip_home, mut skip_phi) = (0usize, 0, 0, 0);
    // The phi-scoped half of the census, RECORDED here and TALLIED at the
    // bottom of this function. See `PhiCensus` for why the tally cannot happen
    // at the `continue`s below.
    let mut phi_refusal: Vec<Option<PhiRefusal>> = vec![None; n];
    // Split apart from `skip_split` on 2026-09-03: see the refusal below.
    let (mut skip_no_alloc, mut skip_spilled) = (0usize, 0usize);
    // Parameters copied into a callee-saved register by the prologue.
    let mut param_copies = 0usize;

    // ── R4: the split values, and the two gates in front of them ─────
    //
    // `skip_split` goes on counting exactly what it counted before — values the
    // scan split and this file then refused — so the census measures the
    // before/after population with no new instrumentation: turning
    // `CRATONVM_JIT_IR_LS_SPLITS` on moves nodes out of `split_or_spilled=` and
    // into `split_taken=`, and `split_unplaceable=` says how many of them this
    // file looked at and could not prove.
    let ls_splits = ir_ls_splits_enabled();
    // **The second-opinion gate, and it is load-bearing.** Two passes below
    // re-prove the allocation's aliasing property against `plan_slots`'
    // independently computed ranges, by asking "do two values sharing a
    // register have overlapping WHOLE ranges". For a split value that question
    // has the wrong answer by construction — a value is split precisely so that
    // another value can use its register while it is elsewhere — so the pass
    // would demote every one of them, and a `drop_ids` hit that cleared the
    // register while the transition table still named it would be worse than
    // useless.
    //
    // The pass is therefore skipped for managed values and replaced by a
    // stronger, simpler fact: the two liveness models agree about EVERY value
    // in the method. If they do, `verify_allocation`'s proof — which is over
    // `live` — is a proof over `plan` as well, and there is no second opinion
    // left to take. If they do not agree anywhere, no value is split-promoted
    // at all, which is exactly today's behaviour and costs nothing that was not
    // already being lost.
    //
    // Interpreter round i2 wave 65 (lane L2): the φ ranges agree only under
    // `CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES`
    // (`i64-L2-ranges-agree-is-false-for-every-graph-with-a-phi`).
    let ranges_agree = slot_and_live_ranges_agree(plan, live);
    let node_pos = ls_node_positions(schedule, live);
    // A split timeline is in LAYOUT order and the allocator does no edge
    // resolution; a value whose register claim at a block top is not honoured
    // on some other incoming edge cannot be followed position by position.
    // Such a value is refused below exactly like every other `ls_split_place`
    // refusal (round 9 wave 2, regalloc request A;
    // `linear-scan-split-timeline-has-no-edge-resolution-20260918.md`).
    // `alloc` is the post-adopt allocation; adoption does not touch
    // `segments`, which is all the detector reads.
    let edge_conflicts = crate::regalloc::split_edge_conflicts(schedule, live, &alloc);
    // Round 9 wave 6 (irlower6): the resolution half. With
    // `CRATONVM_JIT_IR_LS_EDGE_RESOLUTION` on, a flagged value is admitted
    // after all when the allocator can name a repair for every edge it is wrong
    // on (`!unresolvable`), every such edge leaves a block with an
    // edge-specific copy site, and the value is not a reference (a `Ref`
    // loaded on an edge would be a register copy of a root published outside
    // `lower_block`'s invalidation audit; the argument is not written, so it
    // stays refused). The copies themselves are collected after the demotion
    // passes, for the values that survive them (`ls_edges` below).
    let edge_resolution = if ls_splits && ir_ls_edge_resolution_enabled() {
        Some(crate::regalloc::split_edge_resolution(
            schedule, live, &alloc,
        ))
    } else {
        None
    };
    let edge_repairable: Vec<bool> = match &edge_resolution {
        None => vec![false; n],
        Some(res) => {
            let mut ok: Vec<bool> = (0..n)
                .map(|id| {
                    edge_conflicts.get(id).copied().unwrap_or(false)
                        && !res.unresolvable.get(id).copied().unwrap_or(true)
                        && matches!(
                            graph.nodes.get(id).map(|node| node.ty),
                            Some(IrType::Int | IrType::Long | IrType::Float | IrType::Double)
                        )
                })
                .collect();
            for edge in &res.edges {
                if !ls_edge_copy_site_exists(graph, schedule, edge.pred) {
                    for &v in &edge.nodes {
                        if let Some(cell) = ok.get_mut(v as usize) {
                            *cell = false;
                        }
                    }
                }
            }
            ok
        }
    };
    let ls_admitted = ls_splits && ranges_agree;
    // `(node, gp, transitions)` for every value this pass placed. Collected
    // here and turned into a position table at the bottom, after the demotion
    // passes have had their say.
    let mut split_taken: Vec<(usize, bool, Vec<(usize, LsTransition)>)> = Vec::new();
    let mut split_managed = vec![false; n];
    let mut skip_split_unplaceable = 0usize;
    // LV11-3 refusals (counted inside `unplaceable=` as well).
    let mut split_no_gain = 0usize;
    // 2026-09-02, measured: promotion is not free, and two populations pay for
    // it without ever collecting.
    //
    // A register costs a PROLOGUE SAVE and a restore at every exit (the file is
    // callee-saved by construction), plus one publish load at the definition.
    // It repays that at each READ it turns into a register move. So a value
    // read once breaks even at best, and on a small call-heavy method the
    // saves dominate: `FibProbe.fib` promoted five registers, was measured
    // 145-149 ms against 110-133 ms for the same binary with the file off, and
    // the disassembly showed why -- four of the five registers were published
    // and never read.
    //
    // A CONSTANT is worse than break-even: since `ir_const_imm_enabled` every
    // reader materialises it as an immediate, so its register can never be
    // read at all.
    let mut use_count = vec![0u32; n];
    for node in &graph.nodes {
        for &input in &node.inputs {
            if input != NO_NODE {
                if let Some(c) = use_count.get_mut(input as usize) {
                    *c = c.saturating_add(1);
                }
            }
        }
    }
    let (mut skip_const, mut skip_single_use) = (0usize, 0usize);
    // What `ir_reserve_carried_enabled`'s pass will want, computed with ITS
    // predicate so the two cannot disagree about who is a candidate. Its own
    // `gp_reg_of[id].is_none()` filter is the one thing left out -- nothing is
    // assigned yet, and the point here is to reserve room BEFORE that happens.
    // See `ir_residency_crossblock_budget_enabled`.
    let mut carried_w: Vec<u64> = (0..n)
        .filter(|&id| live.carried_and_read.get(id).copied().unwrap_or(false))
        .filter(|&id| plan.node_color.get(id).copied().flatten().is_some())
        .filter(|&id| match graph.nodes.get(id) {
            Some(node) => {
                matches!(node.ty, IrType::Int | IrType::Long)
                    && !matches!(node.op, Op::Const(_))
                    && (ir_phi_residency_enabled() || !matches!(node.op, Op::Phi))
            }
            None => false,
        })
        .map(|id| live.weight.get(id).copied().unwrap_or(0))
        .collect();
    // Descending, because the carried pass serves its own candidates in that
    // order (`cands.sort_unstable_by(|a, b| b.0.cmp(&a.0)...)`) and the register
    // a crossblock value takes comes off the BOTTOM of that list.
    carried_w.sort_unstable_by(|a, b| b.cmp(a));
    let carried_demand = carried_w.len();
    let (mut crossblock_taken, mut crossblock_over) = (0usize, 0usize);
    // CC5-1 (`ir_residency_call_crossing_enabled`): its admissions are charged
    // to `crossblock_taken` too (they spend the same registers), and counted
    // on their own for the census.
    let call_pos = ls_call_positions(graph, live);
    let mut call_crossing_taken = 0usize;
    // JIT r23 wave 10 (lane L7, `CRATONVM_JIT_IR_RESIDENCY_FRAME_PINNED`).
    let frame_named = frame_named_mask(graph);
    // `CRATONVM_JIT_IR_LOOP_CARRIED_FP_REGS` (FP half of L1W3-1): read once
    // per plan; empty when off.
    let fp_readers = if ir_loop_carried_fp_regs_enabled() {
        single_readers(graph)
    } else {
        Vec::new()
    };
    let mut fp_recurrence_taken = 0usize;
    for id in 0..n {
        let Some(segs) = alloc.segments.get(id) else {
            continue;
        };
        // ── Is this a phi, and does the phi mechanism apply at all? ──────
        //
        // Hoisted to the TOP of the loop body, ahead of every other refusal.
        // It used to sit at the bottom, after the bank and home checks, which
        // made the general census double-count: with the mechanism off a phi
        // was tallied under `no_alloc`/`split`/`wrong_bank_or_type` on its way
        // past those checks and then again under `phi`, so `phi=` read as a
        // number of phis while the other fields read as "candidates lost to
        // register pressure" and counted the same nodes. Asking first makes
        // every cause below disjoint, which is the property the phi census's
        // accounting identity rests on.
        //
        // Emission is unchanged by the move: both arms `continue`.
        //
        // **`is_value_ty`, and it is not a formality.** `IrBuilder` builds a
        // MEMORY phi at every merge it builds a value phi at — a counted loop
        // has one at its header — and `build_live_model` sets `wants_loc` for
        // every `Op::Phi` regardless of type, so a memory phi arrives here
        // looking exactly like a candidate. It is not one: `RegClass::of`
        // answers `None` for `Memory`, `allocate_linear_scan` skips it, and it
        // reaches the `[]` arm below with an empty segment list. Counting it as
        // a phi that lost would put one guaranteed refusal per loop into the
        // census, which is precisely how a census stops being read.
        //
        // So a memory phi is not a phi *for this file's purposes*: it takes the
        // unchanged no-candidate path and appears in no phi field.
        let phi_candidate = graph
            .nodes
            .get(id)
            .is_some_and(|node| matches!(node.op, Op::Phi) && is_value_ty(node.ty));
        // 2026-09-02: with `ir_phi_residency_enabled`, `emit_phi_copies` IS
        // the publishing site — every incoming edge writes the home word and
        // then moves the value into the phi's register — so a phi is admitted
        // like any other value, and the allocator's structural pin was
        // released by `release_phi_pins` above on the same condition. With the
        // sub-flag off there is no site that could publish one, and the
        // refusal below is the whole of the pre-2026-09-02 behaviour.
        if phi_candidate && !ir_phi_residency_enabled() {
            skip_phi += 1;
            phi_refusal[id] = Some(PhiRefusal::Disabled);
            continue;
        }
        // ── The safepoint obligation, refused BY TYPE ────────────────────
        //
        // A GC root walk reads a frame it did not stop, through RBP, and
        // `OopMapEntry` names frame slots only — so no reference may be
        // register-resident at a safepoint unless something drops the copy at
        // every point a collector could have run. For ordinary values that
        // something exists (`invalidate_ref_residency`, behind
        // `CRATONVM_JIT_IR_REF_RESIDENCY`, itself default OFF) and its audit is
        // `lower_block`'s per-node loop: every node that is not
        // `op_cannot_deopt` invalidates immediately after it is lowered, so a
        // register copy published by a definition arm cannot survive a GC point
        // it precedes.
        //
        // **A phi is published somewhere that audit does not reach.** Its
        // publish site is `emit_phi_copies`, which runs on the EDGE — after the
        // predecessor's terminator, after the back-edge poll's own
        // invalidation, and before the successor block's node loop starts. That
        // is a publish outside the loop that owns the invariant. It may well be
        // sound; what does not exist is the argument, and a `Ref` phi is the
        // one shape where being wrong is a collector reading a stale pointer
        // rather than a wrong `int`.
        //
        // A `Ref` phi is also the shape with the most to lose: `zero_ref_phi_slots`
        // zeroes every `Ref` phi's home word in the prologue precisely so that
        // word is a publishable GC root from the first safepoint on, before any
        // edge copy has run. The home is therefore a root the collector both
        // READS and WRITES, and a register copy of it is a copy of something
        // evacuation moves.
        //
        // So: refused, unconditionally, **by `IrType`** and not by which bank
        // the type happens to land in. Stating it as a type test is the point —
        // the bank match below would refuse a `Ref` today only because
        // `ir_ref_residency_enabled()` is off, and a future default flip there
        // would silently admit `Ref` phis through an arm written for values
        // with a different publish site. Admitting them needs the edge-publish
        // audit written and tested first, and an owner: nobody has one, so this
        // is a refusal rather than a `TODO`.
        if phi_candidate && matches!(graph.nodes.get(id).map(|node| node.ty), Some(IrType::Ref)) {
            // Counted in the general aggregate too, where it has always
            // landed, so `wrong_bank_or_type` does not change meaning.
            skip_bank += 1;
            phi_refusal[id] = Some(PhiRefusal::RefType);
            continue;
        }
        // A value the carry window cannot reach, and whether the budget still
        // has room for it. `crossblock_candidate` is what it IS;
        // `crossblock_admit` is what this method can still afford.
        let crossblock_candidate = ir_residency_crossblock_enabled()
            && use_count.get(id).copied().unwrap_or(0) == 1
            && !single_use_is_in_the_carry_window(graph, schedule, id);
        let crossblock_admit = crossblock_candidate
            && (!ir_residency_crossblock_budget_enabled()
                || live.weight.get(id).copied().unwrap_or(0)
                    >= carried_displacement_price(&carried_w, crossblock_taken));
        if crossblock_candidate && !crossblock_admit {
            crossblock_over += 1;
        }
        let call_crossing_candidate = !crossblock_candidate
            && single_use_crosses_a_call(
                graph,
                live,
                &call_pos,
                id,
                use_count.get(id).copied().unwrap_or(0),
            );
        let call_crossing_admit = call_crossing_candidate
            && (!ir_residency_crossblock_budget_enabled()
                || live.weight.get(id).copied().unwrap_or(0)
                    >= carried_displacement_price(&carried_w, crossblock_taken));
        // JIT r23 wave 10 (lane L7): a frame-named single-use loop value,
        // under the same reservation price.
        let frame_pinned_candidate = !crossblock_candidate
            && !call_crossing_candidate
            && single_use_frame_pinned_in_loop(
                graph,
                schedule,
                live,
                &frame_named,
                id,
                use_count.get(id).copied().unwrap_or(0),
            );
        // Wave 10b: a recurrence step pays at double weight
        // (`frame_pinned_admission_weight`).
        let frame_pinned_admit = frame_pinned_candidate
            && (!ir_residency_crossblock_budget_enabled()
                || frame_pinned_admission_weight(graph, live, id)
                    >= carried_displacement_price(&carried_w, crossblock_taken));
        // JIT round 25 wave 7 (lane W7-A, `CRATONVM_JIT_IR_REG_ALLOC`): every
        // other single-use `int` / `long` the carry window cannot reach and
        // whose home a site-named plan can drop, under the same reservation
        // price (`r25_w7a_reg_alloc::single_use_candidate`).
        let reg_alloc_candidate = reg_alloc
            && !crossblock_candidate
            && !call_crossing_candidate
            && !frame_pinned_candidate
            && r25_w7a_reg_alloc::single_use_candidate(
                graph,
                schedule,
                id,
                use_count.get(id).copied().unwrap_or(0),
            );
        let reg_alloc_admit = reg_alloc_candidate
            && (!ir_residency_crossblock_budget_enabled()
                || live.weight.get(id).copied().unwrap_or(0)
                    >= carried_displacement_price(&carried_w, crossblock_taken));
        // `CRATONVM_JIT_IR_LOOP_CARRIED_FP_REGS`: a single-use FP step of a
        // phi's recurrence. Not charged to the carried reservation, which
        // prices the GP file only (`carried_w` is `int` / `long`).
        let fp_recurrence_admit = single_use_fp_recurrence_in_loop(
            graph,
            schedule,
            live,
            &fp_readers,
            id,
            use_count.get(id).copied().unwrap_or(0),
        );
        if ir_residency_pays_enabled() {
            match graph.nodes.get(id).map(|node| &node.op) {
                Some(Op::Const(_)) => {
                    skip_const += 1;
                    continue;
                }
                // A phi is exempt: its reads are inside the loop it carries a
                // value around, which is the whole population this file exists
                // for, and its "definition" is an edge copy that is already
                // paying the store.
                Some(Op::Phi) => {}
                // The `< 2` rule below counts STATIC graph edges. That is the
                // right comparison only when the definition and the uses run
                // equally often, and in a loop they do not: a value defined at
                // method entry and read once per iteration is one publish
                // against N reads, and the static count sees 1 and refuses.
                //
                // Measured on the loop the 2026-09-03 tier comparison found
                // inverted: `Param(0)` (the receiver) and `Param(1)` (the loop
                // bound) both read `static_uses=1 loop_weight=10` — refused by
                // a rule that could not see the ten. The disassembly showed
                // exactly that, `mov rax,[rbp-58h]` and `mov rcx,[rbp-60h]`
                // reloaded every iteration, while the baseline tier held both
                // in callee-saved registers.
                //
                // The loop-aware form compares the uses' frequency against the
                // DEFINITION's: residency pays when the reads happen at least
                // twice as often as the single publish. It reduces exactly to
                // `use_count >= 2` when everything sits at depth 0, so a
                // method with no loop is byte-identical.
                // The single-use refusal, and the one population it must not
                // refuse: a value the carry window cannot reach. See
                // `ir_residency_crossblock_enabled` -- for those the
                // alternative to a register is a home store AND a reload, not
                // the free RAX hand-off the refusal assumes.
                _ if !ir_residency_pays_here(
                    live,
                    schedule,
                    id,
                    use_count.get(id).copied().unwrap_or(0),
                ) && !crossblock_admit
                    && !call_crossing_admit
                    && !frame_pinned_admit
                    && !reg_alloc_admit
                    && !fp_recurrence_admit =>
                {
                    skip_single_use += 1;
                    continue;
                }
                _ => {}
            }
        }
        // ONE segment, and it holds a register -- the whole-range case, which
        // needs no transition machinery at all. A spill is refused. A SPLIT
        // goes to the `_` arm below, which since R4 places it position by
        // position when `CRATONVM_JIT_IR_LS_SPLITS` is on and declines it
        // exactly as before when it is off.
        //
        // **The NO-SEGMENT case is counted separately, and that distinction is
        // not cosmetic.** A node the scan produced no interval for was never a
        // candidate — every control and memory node in the graph lands here,
        // `Start` and `Proj` included — and folding it into the split count
        // makes the file look like it is losing values to register pressure
        // when it is only being handed nodes that hold no value at all.
        //
        // It read that way for real. On the loop the 2026-09-03 tier
        // comparison found inverted, this line reported
        // `split_or_spilled=5` beside the allocator's `splits=4`, and the two
        // together said: four candidates lost to live-range splits. They were
        // not. Four of the five had EMPTY segment lists and one was genuinely
        // spilled; the method had no split value for the file to reclaim at
        // all. A day of work aimed at split residency followed from that
        // reading, and its engagement counter read zero — which is how the
        // miscount was found.
        let reg = match segs.as_slice() {
            [] => {
                skip_no_alloc += 1;
                if phi_candidate {
                    phi_refusal[id] = Some(PhiRefusal::NoRegister);
                }
                continue;
            }
            [seg] => match seg.reg {
                Some(reg) => reg,
                None => {
                    skip_spilled += 1;
                    if phi_candidate {
                        phi_refusal[id] = Some(PhiRefusal::NoRegister);
                    }
                    continue;
                }
            },
            _ => {
                // ── R4: the split arm, which used to decline outright ────
                //
                // Everything this arm needs is decided here rather than falling
                // through to the checks below, because those are written around
                // ONE register for a whole range and a split value has several.
                // The bank test is the same one, made once per segment through
                // `ls_bank_for`, so the two cannot drift.
                //
                // A PHI is not admitted. Its publish site is `emit_phi_copies`,
                // which runs on the edge — outside the per-node loop that owns
                // the invalidation audit, and with its own register discipline
                // for reading sources and staging destinations. Admitting one
                // needs that argument written; it is not, so this is a refusal
                // and not a `TODO`.
                let edges_ok = !edge_conflicts.get(id).copied().unwrap_or(true)
                    || edge_repairable.get(id).copied().unwrap_or(false);
                let placed = if ls_admitted && !phi_candidate && edges_ok {
                    ls_split_place(graph, plan, live, &alloc, id, segs.as_slice(), &node_pos)
                } else {
                    None
                };
                // JIT round 19 proposal wave 12, lane lv12 (LV11-3,
                // `CRATONVM_JIT_IR_LS_SPLITS_REFUSE_NO_GAIN`): a placement whose
                // register segments serve no more (loop-weighted) reads than
                // the loads that fill them buys nothing over the memory
                // operand, and still withholds its registers from the carried
                // pass. Refused like every other `ls_split_place` refusal.
                let placed = match placed {
                    Some((_, _, ref transitions))
                        if ir_ls_splits_refuse_no_gain_enabled()
                            && split_register_benefit(live, id, segs.as_slice(), transitions)
                                <= 0 =>
                    {
                        split_no_gain += 1;
                        None
                    }
                    other => other,
                };
                match placed {
                    Some((gp, first, transitions)) => {
                        split_managed[id] = true;
                        if gp {
                            gp_reg_of[id] = first;
                        } else {
                            reg_of[id] = first;
                        }
                        split_taken.push((id, gp, transitions));
                    }
                    None => {
                        if ls_admitted && !phi_candidate {
                            skip_split_unplaceable += 1;
                        }
                        skip_split += 1;
                        if phi_candidate {
                            phi_refusal[id] = Some(PhiRefusal::NoRegister);
                        }
                    }
                }
                continue;
            }
        };
        let ty = graph.nodes.get(id).map(|node| node.ty);
        // Which bank, and is the type one that bank may hold?
        //
        // **`IrType::Ref` is admitted by neither, and that is the safepoint
        // obligation, not a tuning choice.** A GC root walk reads a frame it
        // did not stop, through RBP, and `OopMapEntry` names frame slots only —
        // there is no register a collector could be told about, walk, or update
        // on an evacuation. The XMM file discharged this structurally (no
        // register a `Ref` could occupy); the GP file has to discharge it by
        // refusing the type here, which is what this match does and what
        // `a_reference_is_never_promoted_into_the_gp_file` pins.
        //
        // Everything else in the match is defensive: a register outside its own
        // file, or a type outside its bank, means the file or `RegClass::of`
        // changed under this code.
        let is_gp = match (reg.class, ty) {
            (RegClass::Xmm, Some(IrType::Float) | Some(IrType::Double))
                if IR_LOWER_LS_XMMS.contains(&reg.num) =>
            {
                false
            }
            (RegClass::Gp, Some(IrType::Int) | Some(IrType::Long))
                if IR_LOWER_LS_GPRS.contains(&reg.num) =>
            {
                true
            }
            // A REFERENCE, under `CRATONVM_JIT_IR_REF_RESIDENCY`. **A `Ref`
            // PHI never reaches this arm** -- it was refused by type above,
            // because its publish site is the edge copy and this arm's
            // invalidation argument is about definition arms. Read the
            // paragraph above and then this one: what that paragraph states is
            // the obligation, and the obligation is discharged rather than
            // waived. The home word is still written at the definition (a
            // `Ref` is refused by `value_home_droppable` and
            // `phi_home_droppable` exactly as before), so `emit_safepoint_map`
            // publishes a slot the collector can read AND update; the register
            // is a copy, and `invalidate_ref_residency` drops it at every point
            // a collector could have run, so the next read reloads the word the
            // collector just rewrote. No reference is ever a root the map does
            // not name, and `OopMapEntry` needs no register bank.
            (RegClass::Gp, Some(IrType::Ref))
                if ir_ref_residency_enabled() && IR_LOWER_LS_GPRS.contains(&reg.num) =>
            {
                REF_RESIDENCY_ADMITTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            _ => {
                skip_bank += 1;
                if phi_candidate {
                    // NOT `RefType`: the `Ref` refusal above already took every
                    // `Ref` phi, so anything reaching here is a phi whose type
                    // and bank disagree for some other reason — which means the
                    // file or `RegClass::of` moved under this match.
                    phi_refusal[id] = Some(PhiRefusal::WrongBank);
                }
                continue;
            }
        };
        // Write-through needs a home to write to.
        if plan.node_color.get(id).copied().flatten().is_none() {
            skip_home += 1;
            if phi_candidate {
                phi_refusal[id] = Some(PhiRefusal::NoHome);
            }
            continue;
        }
        // The home check above is deliberately NOT `SlotClass::Pinned`. That
        // class means "never shares a frame word", and after
        // `release_deopt_pins` it covers exactly the deopt-named values — i.e.,
        // on a bytecode graph, nearly all of them. Testing it here would undo
        // the release and make this wiring inert again. A dedicated home word
        // is if anything the safer case for write-through, and for a phi it is
        // what `prealloc_phi_slots` guarantees outright.
        if is_gp {
            gp_reg_of[id] = Some(reg.num);
            // Charged HERE rather than at the guard: everything between the two
            // can still `continue`, and a budget spent on a value that never
            // got a register would starve the carried pass for nothing.
            if crossblock_candidate {
                crossblock_taken += 1;
            }
            // JIT r23 wave 10 (lane L7): charged like a crossblock value.
            if frame_pinned_admit {
                crossblock_taken += 1;
            }
            // W7-A: likewise, when the pays rule would have refused it.
            if reg_alloc_admit
                && ir_residency_pays_enabled()
                && !ir_residency_pays_here(
                    live,
                    schedule,
                    id,
                    use_count.get(id).copied().unwrap_or(0),
                )
            {
                crossblock_taken += 1;
            }
            // Only when the pays rule would have refused it: a value that
            // pays on its own spends nothing of the reservation it did not
            // spend before.
            if call_crossing_admit
                && ir_residency_pays_enabled()
                && !ir_residency_pays_here(
                    live,
                    schedule,
                    id,
                    use_count.get(id).copied().unwrap_or(0),
                )
            {
                crossblock_taken += 1;
                call_crossing_taken += 1;
            }
        } else {
            reg_of[id] = Some(reg.num);
            if fp_recurrence_admit {
                fp_recurrence_taken += 1;
            }
        }
    }

    // ── Second opinion on the aliasing property ──────────────────────
    //
    // `verify_allocation` proves "no two values hold one register at one
    // position" against the ALLOCATOR's liveness model. Re-prove it against
    // `plan_slots`' independently computed ranges. If the two models disagree
    // about an overlap, both values lose the register — the disagreement
    // itself is the reason not to trust either answer for that pair.
    //
    // Keyed by `(bank, number)` now that there are two files: `Gp(3)` and
    // `Xmm(3)` are different registers, and a map keyed by the number alone
    // would report them as one holder and demote a healthy pair. That is the
    // same aliasing hazard `PhysReg` carries its class for.
    //
    // **A managed split value is not in this pass**, and the reason is in the
    // `ranges_agree` comment above: the overlap question is asked of WHOLE
    // ranges, and a split value shares its register with another value by
    // construction, so the pass would demote every one of them. What replaces
    // it is the stronger fact `ls_admitted` already requires — that the two
    // liveness models agree about every value in the method, which makes
    // `verify_allocation`'s proof a proof over `plan_slots`' ranges too and
    // leaves no second opinion to take. Nothing is skipped without that.
    let mut holders: HashMap<(bool, u8), Vec<usize>> = HashMap::new();
    for (id, reg) in gp_reg_of.iter().enumerate() {
        if let Some(reg) = reg {
            if !split_managed[id] {
                holders.entry((true, *reg)).or_default().push(id);
            }
        }
    }
    for (id, reg) in reg_of.iter().enumerate() {
        if let Some(reg) = reg {
            if !split_managed[id] {
                holders.entry((false, *reg)).or_default().push(id);
            }
        }
    }
    let mut drop_ids: Vec<usize> = Vec::new();
    for ids in holders.values() {
        for (i, &a) in ids.iter().enumerate() {
            let Some(ra) = plan.range.get(a).copied().flatten() else {
                drop_ids.push(a);
                continue;
            };
            for &b in &ids[i + 1..] {
                match plan.range.get(b).copied().flatten() {
                    Some(rb) if !ra.overlaps(rb)
                        || model.permits_register_handoff(live, a as NodeId, b as NodeId,
                            crate::regalloc::PosRange { lo: ra.lo, hi: ra.hi }, crate::regalloc::PosRange { lo: rb.lo, hi: rb.hi })
                        || model.permits_register_handoff(live, b as NodeId, a as NodeId,
                            crate::regalloc::PosRange { lo: rb.lo, hi: rb.hi }, crate::regalloc::PosRange { lo: ra.lo, hi: ra.hi }) => {}
                    _ => {
                        drop_ids.push(a);
                        drop_ids.push(b);
                    }
                }
            }
        }
    }
    // ── Second opinion on the ABI property ───────────────────────────
    //
    // The same idea for `verify_allocation`'s clobber check: re-run it against
    // `plan_slots`' range for the value rather than the allocator's.
    for (bank, file) in [(false, &reg_of), (true, &gp_reg_of)] {
        for (id, reg) in file.iter().enumerate() {
            let Some(reg) = reg else { continue };
            // Asked per SEGMENT for a managed value, below: the whole-range
            // form would demote a value whose register is given up at exactly
            // the clobber, which is the shape splitting exists to produce.
            if split_managed[id] {
                continue;
            }
            let Some(range) = plan.range.get(id).copied().flatten() else {
                continue;
            };
            let me = if bank {
                PhysReg::gp(*reg)
            } else {
                PhysReg::xmm(*reg)
            };
            if clobbered_within_for_value(&model, live, id, range.lo, range.hi, me) {
                drop_ids.push(id);
            }
        }
    }
    // The same ABI question for a managed value, asked of each segment over the
    // positions that segment actually covers. This one is a re-assertion rather
    // than a second opinion — `verify_allocation` proves it already — and it is
    // here because a clobber the emission does not respect is a value destroyed
    // by a call it was supposed to survive, and one cheap loop is a better
    // guard than the assumption that another module's proof covers this file's
    // use of it.
    for &(id, gp, _) in &split_taken {
        let Some(segs) = alloc.segments.get(id) else {
            continue;
        };
        let bad = segs.iter().any(|seg| {
            let Some(reg) = seg.reg else { return false };
            let me = if gp {
                PhysReg::gp(reg.num)
            } else {
                PhysReg::xmm(reg.num)
            };
            clobbered_within_for_value(&model, live, id, seg.range.lo, seg.range.hi, me)
        });
        if bad {
            drop_ids.push(id);
        }
    }
    for id in drop_ids {
        // A node lives in at most one bank (its type picks the bank), so
        // clearing both is clearing the one it is in.
        let had_register = reg_of[id].take().is_some() || gp_reg_of[id].take().is_some();
        // A demoted split value must lose its TRANSITIONS as well as its
        // register. Leaving the table naming it would publish into a register
        // the rest of this function believes is free, which is the one way this
        // pass could produce wrong code rather than slow code. It is also the
        // only demotion that has to be counted separately: a split value whose
        // allocator has it in MEMORY at its definition holds no register to
        // take away, so `had_register` alone would report it as untouched.
        let was_managed = split_managed[id];
        split_managed[id] = false;
        // `drop_ids` carries duplicates (the aliasing pass pushes both sides of
        // every overlapping pair), and both tests above are idempotent, so a
        // second visit adds nothing to the count.
        if had_register || was_managed {
            demoted += 1;
        }
    }
    split_taken.retain(|&(id, _, _)| split_managed[id]);

    // ── JIT round 19 proposal wave 11 (lane lv11): carried values first ──
    //
    // Every register a split value visits is withheld from the loop-carried
    // reservation below (`split_gp_regs`), so on a loop whose carried set is
    // larger than what the scan left, admitting a split value can cost a
    // carried accumulator its register for the WHOLE loop -- a store and a
    // reload of a loop-carried chain on every iteration -- in exchange for a
    // value whose reads were memory reads before and are register loads now
    // (`R19Lv10SplitGate.pressure`: two split values took r3 / r14 and the
    // reservation fell from 5 to 3; `lv11_tests`). Under
    // `CRATONVM_JIT_IR_LS_SPLITS_CARRIED_FIRST` a dry run of that reservation
    // with every split value demoted names the registers it would hand out,
    // and a GP split value that visits one of them yields: it is demoted to
    // its home (as every refusal here is), before its edge copies are
    // collected, so nothing is half-kept. Only ever removes split values.
    let mut split_yielded = 0usize;
    // JIT round 19 proposal wave 12, lane lv12 (LV11-2): the priced form,
    // `CRATONVM_JIT_IR_LS_SPLITS_CARRIED_PRICED`, keeps a colliding split value
    // when its register reads outweigh the carried values it displaces.
    let carried_priced = ir_ls_splits_carried_priced_enabled();
    if (ir_ls_splits_carried_first_enabled() || carried_priced)
        && split_taken.iter().any(|&(_, gp, _)| gp)
    {
        let mut dry = gp_reg_of.clone();
        for &(id, gp, _) in &split_taken {
            if gp {
                dry[id] = None;
            }
        }
        let mut taken: Vec<u8> = dry.iter().flatten().copied().collect();
        taken.sort_unstable();
        taken.dedup();
        let assigned = carried_reservation(graph, plan, live, &model.clobbers, &dry, &taken);
        let wanted: Vec<u8> = assigned.iter().map(|&(_, reg)| reg).collect();
        for &(id, gp, ref transitions) in &split_taken {
            if !gp {
                continue;
            }
            let mut visited: Vec<u8> = gp_reg_of[id].into_iter().collect();
            for (_, t) in transitions {
                if let Some((reg, true)) = t.destination() {
                    visited.push(reg);
                }
            }
            let visits_wanted = visited.iter().any(|r| wanted.contains(r));
            let displaced: u64 = assigned
                .iter()
                .filter(|(_, reg)| visited.contains(reg))
                .map(|&(cid, _)| live.weight.get(cid).copied().unwrap_or(0))
                .fold(0u64, u64::saturating_add);
            let segs = alloc.segments.get(id).map(Vec::as_slice).unwrap_or(&[]);
            let benefit = split_register_benefit(live, id, segs, transitions);
            let yields = visits_wanted && (!carried_priced || benefit <= i128::from(displaced));
            // Wave 12 (lane lv12): one line per GP split value under
            // `CRATONVM_DBG_IR_LINEAR_SCAN`, so a host run of the probe names
            // the split values that SURVIVE the yield and what they cost
            // (the i22 A/B: E stayed near C on the real, optimised graph).
            if false {
                let op = graph
                    .nodes
                    .get(id)
                    .map(|n| format!("{:?}", n.op))
                    .unwrap_or_default();
                let op = op.split([' ', '{', '(']).next().unwrap_or("?").to_string();
                let loads = transitions
                    .iter()
                    .filter(|(_, t)| matches!(t, LsTransition::Load { .. }))
                    .count();
                eprintln!(
                    "[ir-ls] split-value: n{id} {op} regs={visited:?} loads={loads} \
                     transitions={} benefit={benefit} displaced={displaced} \
                     collides={visits_wanted} yields={yields}",
                    transitions.len(),
                );
            }
            if yields {
                split_managed[id] = false;
                gp_reg_of[id] = None;
                split_yielded += 1;
            }
        }
        split_taken.retain(|&(id, _, _)| split_managed[id]);
    }

    // ── Round 9 wave 6: the edge copies the SURVIVING managed values owe ──
    //
    // Filtered to `split_managed` AFTER the demotion passes: a copy for a value
    // that lost its register would write a register the rest of this function
    // believes it no longer owns. Every copy of a surviving value is kept --
    // it was admitted precisely because all of its mismatches have one, and
    // dropping any of them would reintroduce the wrong-code shape the gate
    // exists for. A destination that is not a register is not something
    // `split_edge_resolution` produces; if it ever did, that value is demoted
    // here like any other refusal rather than left half-repaired.
    let mut ls_edges: Vec<LsEdgeCopies> = Vec::new();
    if let Some(res) = &edge_resolution {
        let mut unexpressible: Vec<usize> = Vec::new();
        for edge in &res.edges {
            for (&(dst, _), &v) in edge.copies.iter().zip(edge.nodes.iter()) {
                let v = v as usize;
                if split_managed.get(v).copied().unwrap_or(false)
                    && !matches!(dst, crate::regalloc::ValueLoc::Reg(_))
                {
                    unexpressible.push(v);
                }
            }
        }
        for v in unexpressible {
            if split_managed[v] {
                split_managed[v] = false;
                reg_of[v] = None;
                gp_reg_of[v] = None;
                demoted += 1;
            }
        }
        split_taken.retain(|&(id, _, _)| split_managed[id]);
        for edge in &res.edges {
            let mut copies: Vec<(NodeId, u8, bool)> = Vec::new();
            for (&(dst, _), &v) in edge.copies.iter().zip(edge.nodes.iter()) {
                if !split_managed.get(v as usize).copied().unwrap_or(false) {
                    continue;
                }
                if let crate::regalloc::ValueLoc::Reg(reg) = dst {
                    copies.push((v, reg.num, matches!(reg.class, RegClass::Gp)));
                }
            }
            if !copies.is_empty() {
                ls_edges.push(LsEdgeCopies {
                    pred: edge.pred,
                    succ: edge.succ,
                    copies,
                });
            }
        }
    }

    // Every GP register a surviving managed value passes through.
    //
    // The two passes below hand out registers the ALLOCATOR did not use, and
    // they identify those by scanning `gp_reg_of` — which for a managed value
    // names only the register it starts in. Without this, a register a managed
    // value moves into halfway through the method reads as free, and the
    // carried pass would give it to a loop-carried value that is live across
    // exactly the positions the managed one occupies it. That is an aliasing
    // bug, not a heuristic one: both values would be published into one
    // register while both are live, and the later publish wins.
    let mut split_gp_regs: Vec<u8> = Vec::new();
    for (_, gp, transitions) in &split_taken {
        if !*gp {
            continue;
        }
        for (_, transition) in transitions {
            if let Some((reg, true)) = transition.destination() {
                split_gp_regs.push(reg);
            }
        }
    }
    // An edge copy's destination is a register the value visits too (round 9
    // wave 6). It is also a timeline register of that value at a node position,
    // so this should add nothing new; it is listed anyway, because a register
    // left out of this set is one the passes below may hand to somebody else.
    for edge in &ls_edges {
        for &(_, reg, gp) in &edge.copies {
            if gp {
                split_gp_regs.push(reg);
            }
        }
    }
    split_gp_regs.sort_unstable();
    split_gp_regs.dedup();

    // ── The loop-carried set, reserved out of what the scan left ─────
    //
    // Same door the parameter copy below uses, and the same safety argument:
    // **a register in this file that the accepted set does not name is written
    // by nothing.** Emission is driven by `gp_reg_of` alone, so a register the
    // scan used only for values this file then REFUSED is not written by the
    // emitted code either, and taking it here cannot alias anything.
    //
    // What it fixes is not a bad decision by the scan but a bad QUESTION put to
    // it. The scan allocates the carried values in competition with every
    // transient in the loop body, under `peak_live` far above the file size, so
    // it splits them — and `plan_register_residency` refuses a split value
    // outright, because this backend has no reload machinery. Reserving first
    // asks instead: these few values are needed on every iteration, give each
    // one a register and let everything else have the rest.
    //
    // Ordered by loop-weighted use count so that, when the carried set is
    // larger than the file, the registers go to the values read most often
    // inside the loop.
    //
    // Publishing is already handled and needs nothing new: a phi is published
    // by `emit_phi_copies` at every incoming edge, and any other value by the
    // generic publish at the end of its own `lower_data_node`. Both are keyed
    // on `assigned_gpr`, which is what this writes.
    let mut carried_reserved = 0usize;
    for &(id, reg) in &pre_carried {
        gp_reg_of[id] = Some(reg);
        carried_reserved += 1;
    }
    {
        let mut taken: Vec<u8> = gp_reg_of.iter().flatten().copied().collect();
        // Plus every register a SPLIT value moves into later in the method; see
        // `split_gp_regs` for what handing one of those out again would cost.
        taken.extend_from_slice(&split_gp_regs);
        taken.sort_unstable();
        taken.dedup();
        for (id, reg) in carried_reservation(graph, plan, live, &model.clobbers, &gp_reg_of, &taken)
        {
            gp_reg_of[id] = Some(reg);
            carried_reserved += 1;
        }
    }

    // ── Entry parameters, which the scan usually declines ────────────
    //
    // **Corrected 2026-09-16.** This paragraph used to open: "`MachineModel::
    // pin_entry_params` pins every `Param` to its INCOMING ABI register, and
    // `allocate_linear_scan` skips a pinned value outright […] so a parameter
    // can never be promoted into the callee-saved file however the heuristics
    // are tuned." **Nothing in this file calls `pin_entry_params`** — its only
    // mentions in the crate are its own definition and two doc references — so
    // a `Param` is an ordinary candidate here, and one read twice inside a loop
    // is promoted by the scan like any other value.
    //
    // What is true, and is what the disassembly below actually shows, is
    // narrower: a parameter read ONCE is refused by `ir_residency_pays_here`
    // (`skip_single_use`), and a loop bound is exactly that shape — one static
    // read at the loop test, executed every iteration. The static rule cannot
    // see the frequency; `ir_residency_loop_weight_enabled` is the rule that
    // can, and it is default OFF. So the copy below is not a workaround for a
    // structural impossibility but for a heuristic that, at its default,
    // declines the one population it most wants.
    //
    // That is what the 2026-09-03 disassembly of the inverted `fieldloop`
    // showed: `mov rcx,[rbp-60h]` reloading the loop bound on every iteration,
    // while the baseline tier had copied it into a callee-saved register in its
    // prologue (`mov r14,rdx`) and read it from there.
    //
    // So the copy is made here instead, out of a register the ALLOCATOR DID NOT
    // USE. That is the whole safety argument: an unassigned register in this
    // file is written by nothing else — only the residency machinery publishes
    // into it — and every register in the file is callee-saved, so a call
    // cannot destroy it either. The parameter is SSA and never redefined, so
    // one publish at entry is good for the whole method.
    //
    // `IrType::Ref` is excluded for the reason the bank match above gives, and
    // it is not a tuning choice: `OopMapEntry` names frame slots only, so a
    // reference in a register is invisible to a root walk and cannot be
    // updated on evacuation. The receiver therefore stays in its frame slot,
    // and closing that needs oop maps that can name a register.
    // W8-4: implied by `CRATONVM_JIT_IR_REG_ALLOC` unless its own key is set.
    if ir_param_prologue_copy_enabled()
        || r25_w7a_reg_alloc::implied(reg_alloc, r25_w7a_reg_alloc::PARAM_COPY_KEY)
    {
        let mut taken: Vec<u8> = gp_reg_of.iter().flatten().copied().collect();
        // Plus every register a SPLIT value moves into later in the method; see
        // `split_gp_regs` for what handing one of those out again would cost.
        taken.extend_from_slice(&split_gp_regs);
        taken.sort_unstable();
        taken.dedup();
        let mut free: Vec<u8> = ir_gp_file()
            .iter()
            .copied()
            .filter(|r| !taken.contains(r))
            .collect();
        for id in 0..n {
            if free.is_empty() {
                break;
            }
            if gp_reg_of.get(id).copied().flatten().is_some() {
                continue;
            }
            let Some(node) = graph.nodes.get(id) else {
                continue;
            };
            if !matches!(node.op, Op::Param(_)) {
                continue;
            }
            if !matches!(node.ty, IrType::Int | IrType::Long) {
                continue;
            }
            // Worth a register only if the reads outnumber the one publish.
            // Loop-weighted, because a parameter's uses are typically inside a
            // loop its definition is not.
            if live.weight.get(id).copied().unwrap_or(0) < 2 {
                continue;
            }
            if let Some(reg) = free.pop() {
                gp_reg_of[id] = Some(reg);
                param_copies += 1;
            }
        }
    }
    assign_free_callee_saved_loop_invariants(
        graph,
        schedule,
        plan,
        live,
        &model.clobbers,
        &split_gp_regs,
        &mut gp_reg_of,
        &reg_of,
        &split_managed,
    );

    // ── JIT round 25 (item 1b): the caller-saved half, installed last ──
    //
    // Every value here was pinned out of the scan above, so no pass gave it a
    // register; the guard is belt-and-braces, and a value it skips simply
    // stays in memory (its register is then written by nothing).
    let gp_before_caller_saved = gp_reg_of.iter().filter(|r| r.is_some()).count();
    let mut caller_saved_taken = 0usize;
    for &(id, reg) in &caller_saved.assigned {
        if gp_reg_of[id].is_none() && reg_of[id].is_none() && !split_managed[id] {
            gp_reg_of[id] = Some(reg);
            caller_saved_taken += 1;
        }
    }
    // Round 25 wave 2: split runs for values that ended up in no register at
    // all, out of what the whole-range values left (`plan_caller_saved_splits`).
    let split_taken_cs = if caller_saved.active && ir_ls_caller_saved_splits_enabled() {
        plan_caller_saved_splits(
            graph,
            schedule,
            plan,
            live,
            &mut gp_reg_of,
            &reg_of,
            &split_managed,
            &mut caller_saved,
        )
    } else {
        0
    };
    if caller_saved.active
        && false
    {
        eprintln!(
            "[ir-ls] caller-saved: file={IR_LS_CALLER_SAVED_GPRS:?} candidates={} \
             taken={caller_saved_taken} no_room={} opaque_defs={} \
             split_values={} split_runs={} split_no_room={} \
             republish={} gp_resident_before={gp_before_caller_saved} gp_resident_now={}",
            caller_saved.candidates,
            caller_saved.candidates - caller_saved.assigned.len(),
            caller_saved
                .republish
                .iter()
                .filter(|(after, value)| after == value)
                .count(),
            caller_saved.split_values,
            caller_saved.split_runs,
            caller_saved.split_no_room,
            caller_saved.republish.len(),
            gp_before_caller_saved + caller_saved_taken + split_taken_cs,
        );
    }

    // ── The split plan, sequentialised ───────────────────────────────
    //
    // Everything above decided WHICH values are managed and WHAT each one owes.
    // This turns that into the position table the emission reads, and it is the
    // last place the whole feature can still be refused.
    //
    // **Several moves at one position are a PARALLEL copy.** `a ← b` beside
    // `b ← a` is a swap, and emitting the two in sequence makes both `b`. The
    // sequencer for that is `regalloc::resolve_parallel_copy`, already written,
    // already proved and already used for the phi edges, so it is what runs
    // here — loads included, because a load into a register another value is
    // still being moved out of has exactly the same hazard as a move.
    //
    // **A cycle refuses the feature for the whole method.** Breaking one costs
    // a scratch register, and between two nodes there is none that is free by
    // construction: RAX may be carrying a value to the next node and RCX a
    // deferred one (see `plan_carries`). Refusing is a handful of lines and
    // costs only an optimization; guessing at a scratch is a wrong value in a
    // register.
    let mut split_plan = LsSplitPlan::default();
    if !split_taken.is_empty() {
        match ls_sequence_split_plan(&split_taken, live.total_positions) {
            Some(at) => {
                split_plan.transitions = at.iter().map(|v| v.len()).sum();
                split_plan.at = at;
                split_plan.block_start = live.span.iter().map(|&(start, _)| start).collect();
                split_plan.managed = split_managed.clone();
                split_plan.values = split_taken.len();
                split_plan.edges = std::mem::take(&mut ls_edges);
            }
            None => {
                // Every managed value goes back to no register at all. The
                // registers they were given are then written by nothing, which
                // is the same position this file is in for every value the scan
                // promoted and it refused.
                for &(id, _, _) in &split_taken {
                    reg_of[id] = None;
                    gp_reg_of[id] = None;
                    split_managed[id] = false;
                }
                skip_split += split_taken.len();
                split_taken.clear();
            }
        }
    }

    // Round 11 wave 6: what each back-edge poll reloads, and the values that
    // may not be reloaded (see `poll_fp_reload_sets`) sent back to memory
    // BEFORE anything below counts or tallies `reg_of`.
    let (mut poll_fp_reloads, poll_demoted) =
        poll_fp_reload_sets(schedule, live, plan, &alloc, &mut reg_of, &split_managed);
    demoted += poll_demoted;
    // JIT round 21, lane tcmiss (TCPERF21-1): and what each type check's slow
    // path reloads, under keys no block index reaches.
    let (typecheck_fp_reloads, typecheck_demoted) =
        typecheck_fp_reload_sets(graph, live, plan, &alloc, &mut reg_of, &split_managed);
    demoted += typecheck_demoted;
    poll_fp_reloads.extend(typecheck_fp_reloads);
    let fp_promoted = reg_of.iter().filter(|r| r.is_some()).count();
    let gp_promoted = gp_reg_of.iter().filter(|r| r.is_some()).count();
    // A managed value whose allocator has it in MEMORY at its own definition
    // appears in neither vector yet — its first register arrives at a
    // transition — so it has to be counted here, or the `promoted == 0` refusal
    // below would throw away a plan that does real work.
    let split_only = split_taken
        .iter()
        .filter(|&&(id, gp, _)| {
            if gp {
                gp_reg_of[id].is_none()
            } else {
                reg_of[id].is_none()
            }
        })
        .count();
    let promoted = fp_promoted + gp_promoted + split_only;

    // ── The phi census, tallied against the FINAL assignment ─────────
    //
    // Every phi in the graph lands in exactly one field. `resident` is read off
    // `reg_of`/`gp_reg_of` after the reserve and parameter passes have run, so
    // a phi the scan refused and `ir_reserve_carried_enabled` then rescued is
    // counted as resident and its recorded refusal is discarded — which is the
    // point: this reports what the emission does, not what the scan intended.
    // See `PhiCensus`.
    let mut phi_census = PhiCensus::default();
    let mut phi_total = 0usize;
    for id in 0..n {
        // The same `is_value_ty` filter the scan loop applies; see the comment
        // on `phi_candidate` there for why a memory phi is not counted.
        if !graph
            .nodes
            .get(id)
            .is_some_and(|node| matches!(node.op, Op::Phi) && is_value_ty(node.ty))
        {
            continue;
        }
        phi_total += 1;
        if reg_of[id].is_some() || gp_reg_of[id].is_some() {
            phi_census.resident += 1;
            continue;
        }
        match phi_refusal[id] {
            Some(PhiRefusal::Disabled) => phi_census.disabled += 1,
            Some(PhiRefusal::RefType) => phi_census.ref_type += 1,
            Some(PhiRefusal::NoRegister) => phi_census.no_register += 1,
            Some(PhiRefusal::NoHome) => phi_census.no_home += 1,
            Some(PhiRefusal::WrongBank) => phi_census.wrong_bank += 1,
            // The scan refused it for no recorded reason, so the only thing
            // that can have taken its register is one of the two
            // second-opinion passes above.
            None => phi_census.demoted += 1,
        }
    }
    debug_assert_eq!(
        phi_census.total(),
        phi_total,
        "the phi census lost a phi: {phi_census:?} against {phi_total} phis in the graph. \
         Every refusal in the scan loop needs a `PhiRefusal` here, or this census reads \
         low for a reason nobody can chase -- which is the exact failure mode the \
         single `phi=` field it replaces had."
    );

    // ── Could the home slot be dropped? The census that ASKED, retired ──
    //
    // `docs/feature-designs/jit-machine-level-and-instruction-selection.md` argues that a value which
    // lives in a register for its whole range needs no frame word, and rested
    // that on `LiveModel::pinned` covering every deopt-named value. On this
    // path it does not: `release_deopt_pins` above deliberately releases
    // exactly those pins. So a promoted value here may very well be named by a
    // deopt frame, and — as the design stood — its home was what that frame
    // read.
    //
    // A census printed here counted that, under the only rule available at the
    // time: "promoted, and named by NO safepoint, and not a phi". It answered
    // `droppable=0` on real bytecode, and the answer was acted on — the next
    // move it named, deopt metadata that can describe a REGISTER, is
    // `ir-deopt-regs`, and `ir-reg-authoritative` / `ir-drop-phi-home` /
    // `ir-phi-copy-regs` followed it.
    //
    // **Which is why the line is gone.** Those flags widened the rule and the
    // census did not follow: on 2026-09-10 it read `droppable=0` on
    // `probes/FieldLoop.java` `sum` while the emission on that same compile
    // dropped THREE homes, and
    // `internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`
    // quoted the zero as evidence that home elimination "does not yet reach"
    // this loop. It does. A stale diagnostic reading zero is not a neutral
    // gap — it retires levers that are engaged.
    //
    // The replacement is computed from the predicates that actually decide,
    // where they decide, so it cannot drift from them again: `home_dropped`
    // (`[ir-ls] homes: dropped_values=`) is the outcome, and
    // `Lowerer::census_home_blocks` (`[ir-ls] homes kept:`) is the per-cause
    // breakdown of what is left.
    if false {
        eprintln!(
            "[ir-ls] nodes={n} positions={} peak_live={} deopt_pins_released={released} \
             scan_promoted={} resident={promoted} (fp={fp_promoted} gp={gp_promoted}) \
             demoted={demoted} splits={} scan_spills={} scan_reloads={}",
            live.total_positions,
            live.peak_live,
            alloc.promoted,
            alloc.splits,
            alloc.spills,
            alloc.reloads,
        );
    }
    if false {
        eprintln!(
            "[ir-ls] skipped: split_or_spilled={skip_split} \
             wrong_bank_or_type={skip_bank} no_home={skip_home} phi={skip_phi} \
             const={skip_const} single_use={skip_single_use} param_copies={param_copies} \
             spilled={skip_spilled} no_alloc={skip_no_alloc} carried_reserved={carried_reserved} \
             | crossblock: price={} taken={crossblock_taken} \
             over={crossblock_over} carried_demand={carried_demand} carried_w={carried_w:?} \
             call_crossing={call_crossing_taken} fp_recurrence={fp_recurrence_taken}",
            carried_displacement_price(&carried_w, crossblock_taken)
        );
        // R4's own line. `split_or_spilled=` above goes on meaning what it
        // always meant — values the scan split and this file refused — so the
        // two lines together are the before/after the staging note asked for:
        // with the flag off `taken=0` and `split_or_spilled=` is the whole
        // population; with it on the population moves across, and
        // `unplaceable=` says how many this file looked at and could not prove.
        eprintln!(
            "[ir-ls] splits: flag={ls_splits} ranges_agree={ranges_agree} \
             taken={} transitions={} unplaceable={skip_split_unplaceable} \
             scan_splits={} scan_remats={} scan_moves={}{}{}",
            split_plan.values,
            split_plan.transitions,
            alloc.splits,
            alloc.remats,
            alloc.reg_moves,
            // JIT round 19 proposal wave 11 (lane lv11): only under its switch,
            // so the default line is unchanged.
            if ir_ls_splits_carried_first_enabled() || ir_ls_splits_carried_priced_enabled() {
                format!(" yielded_to_carried={split_yielded}")
            } else {
                String::new()
            },
            // Wave 12 (lane lv12, LV11-3): only under its switch.
            if ir_ls_splits_refuse_no_gain_enabled() {
                format!(" no_gain={split_no_gain}")
            } else {
                String::new()
            },
        );
        // JIT round 19 wave 10 (lane lv10, i65-L2 measurement): when the gate
        // is shut, name what shut it -- how many values the two models give
        // different ranges, split φ / non-φ, and the first few -- so a host
        // run of `CRATONVM_JIT_IR_LS_SPLITS=1 ..._SLOTS_PHI_EDGE_RANGES=1`
        // that still reads `ranges_agree=false` names its next blocker.
        // Diagnostic only; nothing below reads it.
        if !ranges_agree {
            eprintln!("{}", ranges_disagreement_census(graph, plan, live));
        }
        // The phi half, printed on its own line because it answers a different
        // question from the one above: not "how many candidates did the file
        // lose" but "did the loop-carried population -- a loop counter and a
        // loop accumulator are `Op::Phi` at every loop header -- reach a
        // register, and if not, which refusal took it".
        eprintln!(
            "[ir-ls] phis={phi_total}: resident={} disabled={} ref_type={} \
             no_register={} no_home={} wrong_bank={} demoted={}",
            phi_census.resident,
            phi_census.disabled,
            phi_census.ref_type,
            phi_census.no_register,
            phi_census.no_home,
            phi_census.wrong_bank,
            phi_census.demoted,
        );
        eprintln!("[ir-ls] safepoints={}", graph.safepoints.len());
    }
    if promoted == 0 {
        return Ok(ls_refuse(
            "the scan promoted nothing this file could take -- every candidate \
             was split, spilled, homeless, single-use, a constant, or of a type \
             neither bank holds; see the `skipped:` and `phis:` census lines \
             above for which",
        ));
    }
    Ok(Some(RegResidency {
        reg_of,
        gp_reg_of,
        promoted,
        demoted,
        peak_live: live.peak_live,
        splits: split_plan,
        phi_census,
        poll_fp_reloads,
        caller_saved_republish: caller_saved.republish,
    }))
}

/// The FP values each back-edge block's poll must reload after its slow-path
/// call: an accepted, unsplit value (`reg_of`, one segment) in a caller-saved
/// XMM whose segment covers the poll.
///
/// The poll is emitted before the terminator (at `edge - 1`) or, in a block
/// without one, after its last node, so "covers the poll" is "its segment
/// contains `edge - 1`" -- or `edge` in a block with no position before it. A
/// value live across the poll contains that position by interval continuity;
/// a value that starts at `edge` is written after the poll. Segments of one
/// register are disjoint (`verify_allocation`), so each register gets at most
/// one value per block and the reloads cannot overwrite one another.
///
/// The reload reads the HOME word, so the home must still be this value's at
/// the poll: `plan_slots`' own range (`plan.range`, which the home colouring
/// was built from) has to cover the poll too. The two liveness models agree
/// on ordinary compiles; a value on which they disagree here is DEMOTED --
/// its `reg_of` entry cleared, which only sends it back to memory -- and
/// counted in the second return value.
pub(in crate::ir_lower) fn poll_fp_reload_sets(
    schedule: &Schedule,
    live: &crate::regalloc::LiveModel,
    plan: &SlotPlan,
    alloc: &crate::regalloc::Allocation,
    reg_of: &mut [Option<u8>],
    split_managed: &[bool],
) -> (HashMap<usize, Vec<NodeId>>, usize) {
    let mut out: HashMap<usize, Vec<NodeId>> = HashMap::new();
    // With the XMM clobber still modelled nothing needs a reload, and the
    // outlined arm (one of the two configurations that keep it) refuses a
    // non-empty set -- so do not compute one.
    if !ir_poll_preserves_fp() {
        return (out, 0);
    }
    // Candidates first: usually none, and never more than the values the
    // plan put in the XMM file.
    let cands: Vec<(usize, crate::regalloc::PosRange)> = reg_of
        .iter()
        .enumerate()
        .filter(|&(id, reg)| {
            reg.is_some_and(|r| !IR_LOWER_SAVED_XMMS.contains(&r))
                && !split_managed.get(id).copied().unwrap_or(false)
        })
        .filter_map(|(id, _)| match alloc.segments.get(id).map(Vec::as_slice) {
            Some([seg]) if seg.reg.is_some() => Some((id, seg.range)),
            _ => None,
        })
        .collect();
    if cands.is_empty() {
        return (out, 0);
    }
    let mut unsafe_ids: Vec<usize> = Vec::new();
    for (b, block) in schedule.blocks.iter().enumerate() {
        if !block.successors.iter().any(|&s| s <= b) {
            continue;
        }
        let Some(&(lo, edge)) = live.span.get(b) else {
            continue;
        };
        let at = if edge > lo { edge - 1 } else { edge };
        let mut ids: Vec<NodeId> = Vec::new();
        for &(id, r) in &cands {
            if !(r.lo <= at && at <= r.hi) {
                continue;
            }
            let home_covers = plan
                .range
                .get(id)
                .copied()
                .flatten()
                .is_some_and(|h| h.lo <= at && at <= h.hi);
            if home_covers {
                // Cast: a node index back to its id.
                ids.push(id as NodeId);
            } else {
                unsafe_ids.push(id);
            }
        }
        if !ids.is_empty() {
            out.insert(b, ids);
        }
    }
    unsafe_ids.sort_unstable();
    unsafe_ids.dedup();
    let mut demoted = 0usize;
    for &id in &unsafe_ids {
        if let Some(cell) = reg_of.get_mut(id) {
            if cell.take().is_some() {
                demoted += 1;
            }
        }
    }
    if demoted > 0 {
        // A demoted value is no longer resident anywhere, so no block reloads it.
        for ids in out.values_mut() {
            ids.retain(|&id| !unsafe_ids.contains(&(id as usize)));
        }
        out.retain(|_, ids| !ids.is_empty());
    }
    (out, demoted)
}

/// JIT round 21, lane tcmiss (TCPERF21-1): the key under which
/// `RegResidency::poll_fp_reloads` holds a TYPE CHECK's reload set -- this bit
/// ORed with the check's node id, a key space no block index reaches -- so the
/// sets ride the existing map and `Lowerer::emit_poll_fp_reloads` emits them.
pub(in crate::ir_lower) const TYPECHECK_FP_RELOAD_KEY: usize = 1 << 62;

/// JIT round 21, lane tcmiss (TCPERF21-1): the positions (with their node)
/// of the `Op::InstanceOf` / `Op::CheckCast` nodes that are the ONLY node at
/// their position, sorted by position -- where `ir_lower_machine_model` may
/// leave the XMM bank unclobbered (`CRATONVM_JIT_TYPECHECK_NONCALL`). A
/// position another node shares keeps that node's clobbers whole.
pub(in crate::ir_lower) fn typecheck_noncall_positions(
    graph: &Graph,
    live: &crate::regalloc::LiveModel,
) -> Vec<(usize, NodeId)> {
    let mut checks: Vec<(usize, NodeId)> = Vec::new();
    let mut others: Vec<usize> = Vec::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        let Some(pos) = live.pos_of.get(id).copied().flatten() else {
            continue;
        };
        if matches!(node.op, Op::InstanceOf { .. } | Op::CheckCast { .. }) {
            // Cast: a node index back to its id.
            checks.push((pos, id as NodeId));
        } else {
            others.push(pos);
        }
    }
    if checks.is_empty() {
        return checks;
    }
    others.sort_unstable();
    checks.sort_unstable();
    let mut out: Vec<(usize, NodeId)> = Vec::with_capacity(checks.len());
    for (i, &(pos, id)) in checks.iter().enumerate() {
        let shared_with_check =
            (i > 0 && checks[i - 1].0 == pos) || checks.get(i + 1).is_some_and(|&(p, _)| p == pos);
        if !shared_with_check && others.binary_search(&pos).is_err() {
            out.push((pos, id));
        }
    }
    out
}

/// JIT round 21, lane tcmiss (TCPERF21-1, `CRATONVM_JIT_TYPECHECK_NONCALL`):
/// per type check at a [`typecheck_noncall_positions`] position, the FP values
/// resident in a caller-saved XMM across it, which its slow path reloads from
/// their home words after the helper call -- [`poll_fp_reload_sets`]' rule,
/// at the check's position instead of a back edge's poll, keyed
/// `TYPECHECK_FP_RELOAD_KEY | id`. A value whose home the slot plan does not
/// keep at that position is DEMOTED (counted in the second return value).
///
/// Empty whenever the model clobbered the XMM bank there (the switch off, or
/// split residency): the allocator then gave no such value a caller-saved XMM
/// across the check, so the candidate filter finds none and nothing is
/// demoted -- the switch-off emission is byte-identical.
pub(in crate::ir_lower) fn typecheck_fp_reload_sets(
    graph: &Graph,
    live: &crate::regalloc::LiveModel,
    plan: &SlotPlan,
    alloc: &crate::regalloc::Allocation,
    reg_of: &mut [Option<u8>],
    split_managed: &[bool],
) -> (HashMap<usize, Vec<NodeId>>, usize) {
    let mut out: HashMap<usize, Vec<NodeId>> = HashMap::new();
    if !ir_poll_preserves_fp() {
        return (out, 0);
    }
    let cands: Vec<(usize, crate::regalloc::PosRange)> = reg_of
        .iter()
        .enumerate()
        .filter(|&(id, reg)| {
            reg.is_some_and(|r| !IR_LOWER_SAVED_XMMS.contains(&r))
                && !split_managed.get(id).copied().unwrap_or(false)
        })
        .filter_map(|(id, _)| match alloc.segments.get(id).map(Vec::as_slice) {
            Some([seg]) if seg.reg.is_some() => Some((id, seg.range)),
            _ => None,
        })
        .collect();
    if cands.is_empty() {
        return (out, 0);
    }
    let mut unsafe_ids: Vec<usize> = Vec::new();
    for (at, check) in typecheck_noncall_positions(graph, live) {
        let mut ids: Vec<NodeId> = Vec::new();
        for &(id, r) in &cands {
            if !(r.lo <= at && at <= r.hi) {
                continue;
            }
            let home_covers = plan
                .range
                .get(id)
                .copied()
                .flatten()
                .is_some_and(|h| h.lo <= at && at <= h.hi);
            if home_covers {
                // Cast: a node index back to its id.
                ids.push(id as NodeId);
            } else {
                unsafe_ids.push(id);
            }
        }
        if !ids.is_empty() {
            out.insert(TYPECHECK_FP_RELOAD_KEY | check as usize, ids);
        }
    }
    unsafe_ids.sort_unstable();
    unsafe_ids.dedup();
    let mut demoted = 0usize;
    for &id in &unsafe_ids {
        if let Some(cell) = reg_of.get_mut(id) {
            if cell.take().is_some() {
                demoted += 1;
            }
        }
    }
    if demoted > 0 {
        for ids in out.values_mut() {
            ids.retain(|&id| !unsafe_ids.contains(&(id as usize)));
        }
        out.retain(|_, ids| !ids.is_empty());
    }
    (out, demoted)
}

/// JIT round 20 (lane loopopt; `r21-loopreload-voidwriter-gets-no-ir-body`,
/// `r21-row-driver-loop-is-never-compiled`): under `CRATONVM_DBG_IR_BAILOUT`,
/// name the node an `UnallocatedValue` latch refused on -- its op, type, pc,
/// whether it is a multi-input merge's phi, and its users' ops -- so a host
/// trace says WHICH value had no location instead of only its id (the w16 and
/// w18b traces stopped at `value n26 read before a location was assigned`).
/// Diagnostic only; prints nothing for any other bailout.
pub(in crate::ir_lower) fn describe_unallocated_value(graph: &Graph, bailout: &Bailout) {
    let BailoutReason::UnallocatedValue { node } = &bailout.reason else {
        return;
    };
    let node = *node;
    if !false {
        return;
    }
    let Some(n) = graph.nodes.get(node as usize) else {
        return;
    };
    let anchor = n
        .inputs
        .first()
        .and_then(|&a| graph.nodes.get(a as usize))
        .map(|a| format!("{:?}/{} input(s)", a.op, a.inputs.len()));
    let users: Vec<String> = graph
        .users_of(node)
        .into_iter()
        .filter_map(|u| {
            graph
                .nodes
                .get(u as usize)
                .map(|un| format!("n{u} {:?}", un.op))
        })
        .take(8)
        .collect();
    eprintln!(
        "[ir-bailout] unallocated n{node}: {:?} ty {:?} pc {:?} anchor {:?} users [{}]",
        n.op,
        n.ty,
        n.bytecode_pc,
        anchor,
        users.join(", ")
    );
}

/// Funnel every refusal in this file through one place: count it, then return
/// the `None` the public entry points have always returned, which is the
/// caller's signal to run the method in a lower tier. A compiler refusal must
/// never terminate the VM.
pub(in crate::ir_lower) fn refuse(bailout: Bailout) -> Option<CompiledMethod> {
    if false {
        eprintln!("[ir-bailout] {bailout}");
    }
    record_bailout(&bailout);
    // Attribution, not duplication — the same split `verify_or_bail` makes:
    // `record_bailout` owns the process-wide category counters, and this
    // attaches the bailout to *this* method in the per-compilation report.
    //
    // Without it the report's `bailouts` field is STRUCTURALLY empty for every
    // lowering refusal: a census over CratonBench read `bailouts:[]` on all 105
    // compilations, including the 17 that fell through to single-pass, so the
    // one record that names both the method and the reason named neither. The
    // counters were moving the whole time, which is what made the hole hard to
    // see — the totals looked alive while every per-method row was blank.
    crate::metrics::note_current_bailout(&bailout, "lower");
    // Name the refusal for `last_lower_bail` too, unless the site already did
    // (a site's own label is more specific than the reason's). Before round 9
    // the two channels were disjoint: `refuse` sites counted but read back as
    // "unlabelled" in the `[ir] lower_inner refused (..)` line, and the
    // `note_lower_bail` sites were labelled but never counted.
    if last_lower_bail().is_none() {
        note_lower_bail(match &bailout.reason {
            BailoutReason::UnsupportedShape(what) | BailoutReason::Internal(what) => *what,
            other => other.category(),
        });
    }
    None
}

/// A labelled refusal: [`note_lower_bail`] plus [`refuse`], so the site is both
/// named (for `last_lower_bail`) and counted (`record_bailout` and the
/// per-method report). The label doubles as the `UnsupportedShape` text.
pub(in crate::ir_lower) fn refuse_labelled(label: &'static str) -> Option<CompiledMethod> {
    note_lower_bail(label);
    refuse(Bailout::new(BailoutReason::UnsupportedShape(label)))
}

/// Smallest executable buffer this backend will ask for.
///
/// Two pages. Raised from 4096 with the rest of the sizing below: under the old
/// model the floor WAS the capacity for most compiles (the formula fell under
/// it for any modest graph), so 4096 is where the overflows piled up.
///
/// Raising the floor ALONE does not work, and the census says so: `nodes*32 +
/// calls*448 + 1024` with a 16 KiB floor still overflows 5 of 1664 while
/// reserving 26 MiB — worse on both axes than fixing the formula and keeping
/// the floor at 8 KiB.
pub(in crate::ir_lower) const IR_CODE_BUFFER_FLOOR: usize = 8 * 1024;

/// How many bytes to reserve for a lowered graph of `nodes` nodes, `call_nodes`
/// of which are calls.
///
/// **This is a capacity, not a prediction.** Getting it too high wastes address
/// space in a region already capped by `COMMITTED_JIT_CODE_BYTES`; getting it
/// too low does NOT produce a bug — `ExecutableBuffer::emit` is non-panicking
/// and the caller discards an overflowed body — it silently drops the method to
/// the single-pass backend. That failure is invisible except as throughput, so
/// the bias here is deliberately toward over-reserving.
///
/// ## Where these numbers come from
///
/// Measured, not reasoned. `CRATONVM_DBG_IR_BUFSIZE=1` (below) censused
/// **1664 IR compiles** across three workloads on 2026-08-01 — Spring Boot's
/// `BasicErrorControllerDirectMockMvcTests` (595) and
/// `BasicErrorControllerIntegrationTests` (1067), plus the whole
/// `bench/CratonBench` CPU suite (2) — recording `wanted` against `capacity`
/// for every one.
///
/// The old `nodes*32 + call_nodes*448 + 1024` **overflowed 155 of those 1664**
/// (9.3%), under-shooting by up to **2.65x**. It was never measured; it was
/// reasoned from "an arithmetic node emits well under 32 bytes", and that
/// premise is fine — the error is entirely in the other two terms:
///
/// * **the per-call term.** 448 budgeted, 1141 actually required at worst. The
///   278 call-free compiles in the census never wanted more than 1545 bytes
///   total, so nodes were never the problem; calls always were.
/// * **the constant.** 1024 does not pay for a prologue, an epilogue and the
///   frame setup, so every small graph fell through to the floor and inherited
///   whatever the floor happened to be.
///
/// This model overflows **0 of 1664**, and its tightest fit still has **1.24x**
/// headroom, so it is not sitting on a knife edge. It reserves 13.8 MiB across
/// that census against the old 6.6 MiB.
///
/// **That 2.1x mattered when it was written.** [`ExecutableBuffer::new`] adds
/// the full CAPACITY to [`COMMITTED_JIT_CODE_BYTES`], the quantity the
/// code-cache cap bounds, so over-reserving bought headroom with code cache.
/// Since `ExecutableBuffer::trim_unused_tail` (run by `CompiledMethod::new`)
/// the whole pages past the emitted code go back before the body is
/// published, so an over-estimate now costs at most the last page per
/// published body, plus address space during codegen.
///
/// [`ExecutableBuffer::wanted`] measures this exactly for this backend: it
/// counts every byte codegen ASKED to emit, including writes dropped after an
/// overflow, and this lowerer calls neither `rewind_to` nor `emit_checked`, so
/// nothing inflates it or hides from it.
pub(in crate::ir_lower) fn ir_code_buffer_estimate(nodes: usize, call_nodes: usize) -> usize {
    if legacy_ir_code_buffer_estimate() {
        // The floor is part of the arm: 4096 was doing most of the work in the
        // old sizing, so an A/B that kept the new floor would not be measuring
        // the old behaviour.
        return nodes
            .saturating_mul(32)
            .saturating_add(call_nodes.saturating_mul(448))
            .saturating_add(1024)
            .max(4096);
    }
    nodes
        .saturating_mul(IR_BYTES_PER_NODE)
        .saturating_add(call_nodes.saturating_mul(IR_BYTES_PER_CALL_NODE))
        .saturating_add(IR_CODE_BUFFER_BASE)
        .max(IR_CODE_BUFFER_FLOOR)
}

/// Bytes one GC point's root publication spends per reference home, for
/// [`ir_ref_home_publication_reserve`]: the per-call push (`MOV RCX,[rbp-d]` /
/// `MOV [R11],RCX` / `LEA R11,[R11+8]`, 14 bytes) plus its copy-back after the
/// call, or the frame block's inline ensure (14 bytes per home), rounded up.
pub(in crate::ir_lower) const IR_BYTES_PER_PUBLISHED_HOME: usize = 32;

/// Extra first-attempt buffer for a body with more reference homes than the
/// frame block takes by default (round 14 wave 1, lane chain) -- **default
/// ON**, `CRATONVM_JIT_IR_BUFFER_REF_HOME_RESERVE=0` restores the fitted
/// estimate alone.
///
/// [`ir_code_buffer_estimate`] was fitted on a census where every body stayed
/// under `FRAME_BLOCK_MAX_HOMES`. Past it, each GC point publishes every
/// reference home inline, so the code grows as GC points x homes and the
/// estimate (linear in nodes and calls) is overrun: the chain arm's
/// `HashMap.putVal` (76 847 bytes) was lowered twice, the second time at the
/// exact size (`lower_inner_with_array_list`'s overflow retry). Reserving that
/// term up front lowers such a body once. Capacity only: the emitted bytes
/// are unchanged, and a body at or under the default cap reserves nothing
/// extra (byte-identical sizing).
///
/// Round 15 wave 1 (lane chain): a body over the default cap that still gets
/// a frame block (a raised `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES`, or
/// `CRATONVM_JIT_IR_FRAME_BLOCK_LOOP_CAP`) reserves for the form it will
/// emit: the shared subroutine once, or the inline ensure (14 bytes per home)
/// at every GC point. Before, a raised knob reserved NOTHING for the inline
/// ensure, so such a body overran the estimate and was lowered twice.
pub(in crate::ir_lower) fn ir_ref_home_publication_reserve(graph: &Graph, schedule: &Schedule, plan: &SlotPlan) -> usize {
    let mut seen = vec![false; plan.slots];
    let mut homes = 0usize;
    for (id, node) in graph.nodes.iter().enumerate() {
        if node.ty != IrType::Ref {
            continue;
        }
        if let Some(Some(colour)) = plan.node_color.get(id).copied() {
            if let Some(cell) = seen.get_mut(colour as usize) {
                if !*cell {
                    *cell = true;
                    homes += 1;
                }
            }
        }
    }
    // At or under the DEFAULT cap (and the knob) the fitted estimate covers
    // the publication. Round 15 wave 1: not the knob alone -- a body under a
    // raised `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES` gets a frame block, whose
    // inline ensure is 14 bytes per home at EVERY GC point (below).
    let knob = Lowerer::<'static>::frame_block_max_homes();
    if homes <= knob.min(Lowerer::<'static>::FRAME_BLOCK_MAX_HOMES) {
        return 0;
    }
    let gc_points = graph
        .nodes
        .iter()
        .filter(|n| ir_op_is_gc_point(&n.op))
        .count();
    // The block also names the reference-parameter homes.
    let block_homes = homes.saturating_add(
        graph
            .nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Param(_)) && n.ty == IrType::Ref)
            .count(),
    );
    if block_homes <= Lowerer::<'static>::frame_block_cap_for(graph, schedule, block_homes, knob)
        
    {
        // One subroutine (14 bytes per home plus its frame), and 15 bytes per
        // GC point, which the fitted estimate already covers.
        return block_homes.saturating_mul(IR_BYTES_PER_PUBLISHED_HOME);
    }
    gc_points
        .saturating_mul(block_homes)
        .saturating_mul(IR_BYTES_PER_PUBLISHED_HOME)
}

/// Extra first-attempt buffer for the straight-line fresh-frame clear of
/// `CRATONVM_JIT_IR_FRESH_CLEAR_COVER` (JIT round 25 wave 6 residual, closed
/// 2026-10-25): [`ir_code_buffer_estimate`] was fitted on the run-wise clear,
/// whose loop keeps a big frame's clear at a few dozen bytes, while the cover
/// can emit up to [`FRESH_CLEAR_COVER_MAX_STORES`] stores of at most 8 bytes
/// each (`emit_clear_store` with a disp32), plus `VPXOR` / `VZEROUPPER`, in the
/// prologue and in every OSR entry stub (at most one per loop header). A body
/// near its estimate was lowered a second time at the exact size. Capacity
/// only: the emitted bytes are unchanged, and nothing is reserved with the
/// switch off.
pub(in crate::ir_lower) fn fresh_clear_cover_reserve(loop_headers: usize) -> usize {
    loop_headers
        .saturating_add(1)
        .saturating_mul(FRESH_CLEAR_COVER_ENTRY_BYTES)
}

/// The most bytes one cover clear emits: [`FRESH_CLEAR_COVER_MAX_STORES`]
/// stores of at most 8 bytes, `VPXOR` (4) and `VZEROUPPER` (3).
/// The run-wise clear's cost on `skip` (`Lowerer::emit_zero_frame_words`,
/// which with an empty mask and `avx` off is `emit_zero_frame_sse` byte for
/// byte): `(runs, executed instructions)` per entry, the `PXOR` / `VPXOR`, the
/// `VZEROUPPER` and every loop iteration included. `(0, 0)` when nothing is
/// cleared or the frame size is invalid. Pure; mirrors `emit_zero_run`.
pub(in crate::ir_lower) fn fresh_clear_runwise_cost(frame_size: i32, skip: &[bool], avx: bool) -> (u32, u32) {
    if frame_size <= 0 || frame_size % 8 != 0 {
        return (0, 0);
    }
    // Cast: a positive frame size over 8.
    let words = (frame_size / 8) as usize;
    let skipped = |w: usize| skip.get(w).copied().unwrap_or(false);
    let wide: i32 = if avx { 32 } else { 16 };
    let step = 4 * wide;
    let (mut runs, mut instrs) = (0u32, 0u32);
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
        runs += 1;
        // Cast: word indices below 4096.
        let len = 8 * ((w - start) as i32);
        let looped = if len > ZERO_FRAME_SSE_STRAIGHT_BYTES {
            (len / step) * step
        } else {
            0
        };
        if looped > 0 {
            // Two setup instructions; per iteration SUB, four stores, CMP, JA.
            // Cast: a small non-negative iteration count.
            instrs += 2 + 7 * ((looped / step) as u32);
        }
        let mut rest = len - looped;
        // Cast: a small non-negative store count.
        instrs += (rest / wide) as u32;
        rest %= wide;
        if rest >= 16 {
            instrs += 1;
            rest -= 16;
        }
        if rest == 8 {
            instrs += 1;
        }
    }
    if runs > 0 {
        instrs += if avx { 2 } else { 1 };
    }
    (runs, instrs)
}

pub(in crate::ir_lower) fn fresh_clear_cover_instructions(stores: &[(i32, i32)], avx: bool) -> u32 {
    if stores.is_empty() {
        return 0;
    }
    // Cast: at most `FRESH_CLEAR_COVER_MAX_STORES` stores.
    stores.len() as u32 + if avx { 2 } else { 1 }
}

pub(in crate::ir_lower) const FRESH_CLEAR_REGIONS: [&str; 18] = [
    "param-locals",
    "locals",
    "context",
    "bookkeeping",
    "ref-block",
    "ref",
    "pinned-ref-block",
    "pinned-ref",
    "prim",
    "pinned-prim",
    "colour-unused",
    "spill-other",
    "saves",
    "deopt-gpr",
    "deopt-xmm",
    "staging",
    "helper-area",
    "other",
];

#[derive(Clone, Debug, PartialEq, Eq)]

pub(in crate::ir_lower) struct FreshClearCensus {
    /// Words of the frame.
    pub(in crate::ir_lower) words: u32,
    /// Words the clear must zero (not in the skip mask).
    pub(in crate::ir_lower) cleared: u32,
    /// `cleared` by region, one count per [`FRESH_CLEAR_REGIONS`] entry.
    pub(in crate::ir_lower) by_region: [u32; 18],
    /// The run-wise clear: maximal runs and executed instructions.
    pub(in crate::ir_lower) runwise: (u32, u32),
    /// The cover: stores and executed instructions, `None` when refused.
    pub(in crate::ir_lower) cover: Option<(u32, u32)>,
    /// Whether this clear was emitted as the cover.
    pub(in crate::ir_lower) cover_on: bool,
}

impl Lowerer<'_> {
    /// JIT round 25 wave 6 (W6-A): the cover's emission -- `PXOR XMM5`
    /// (`VPXOR` with `avx`), one store per `stores` entry, `VZEROUPPER` with
    /// `avx`. Nothing for an empty list. Writes XMM5 (YMM5), the flags and
    /// the stores' words, nothing else.
    pub(in crate::ir_lower) fn emit_zero_frame_cover(buf: &mut ExecutableBuffer, stores: &[(i32, i32)], avx: bool) {
        if stores.is_empty() {
            return;
        }
        if avx {
            buf.emit(&[0xC5, 0xD1, 0xEF, 0xED]); // VPXOR XMM5, XMM5, XMM5
        } else {
            buf.emit(&[0x66, 0x0F, 0xEF, 0xED]); // PXOR XMM5, XMM5
        }
        for &(disp, width) in stores {
            Self::emit_clear_store(buf, false, disp, width, avx);
        }
        if avx {
            buf.emit(&[0xC5, 0xF8, 0x77]); // VZEROUPPER
        }
    }

    /// Which region of this frame the word `[rbp - off]` belongs to, for the
    /// census. `colour_kind[c]`: 0 no occupant, 1 `Prim`, 2 `Pinned`
    /// non-reference, 3 `Ref`, 4 `Pinned` reference (as `frame_anatomy`
    /// counts them).
    pub(in crate::ir_lower) fn fresh_clear_word_region(&self, off: i32, colour_kind: &[u8]) -> &'static str {
        // Cast: a JVM parameter count times 8.
        let params_hi = (self.num_params as i32) * 8;
        let in_block = || i16::try_from(off).is_ok_and(|o| self.frame_block.contains(&o));
        let bookkeeping = [
            self.sp_id_slot_off,
            self.shadow_thread_slot_off,
            self.shadow_savebase_slot_off,
            self.shadow_savetop_slot_off,
            self.phi_copy_scratch_slot_off,
        ];
        // Cast: at most a few thousand colours.
        let colours_hi = self.first_spill + 8 * (self.slot_plan.slots as i32);
        let saves_hi = self.spill_cap_off + self.saved_xmm_bytes + self.saved_gpr_bytes;
        let (gpr_lo, gpr_hi) = self.deopt_gpr_image();
        // Cast: a call's argument count.
        let staging_lo = self.args_stage_top_off - 8 * (self.args_stage_words as i32);
        if off <= params_hi {
            "param-locals"
        } else if off <= self.locals_size {
            "locals"
        } else if self.needs_context && off == self.context_slot_off {
            "context"
        } else if bookkeeping.contains(&off) {
            "bookkeeping"
        } else if off >= self.first_spill && off < colours_hi {
            // Cast: a non-negative colour index.
            let c = ((off - self.first_spill) / 8) as usize;
            match colour_kind.get(c).copied().unwrap_or(0) {
                1 => "prim",
                2 => "pinned-prim",
                3 if in_block() => "ref-block",
                3 => "ref",
                4 if in_block() => "pinned-ref-block",
                4 => "pinned-ref",
                _ => "colour-unused",
            }
        } else if off <= self.spill_cap_off {
            "spill-other"
        } else if off <= saves_hi {
            "saves"
        } else if gpr_hi > gpr_lo && off >= gpr_lo && off < gpr_hi {
            "deopt-gpr"
        } else if self
            .deopt_xmm_image()
            .is_some_and(|(lo, hi)| off >= lo && off < hi)
        {
            "deopt-xmm"
        } else if off > self.args_stage_top_off {
            "helper-area"
        } else if off > staging_lo {
            "staging"
        } else {
            "other"
        }
    }

}

pub(in crate::ir_lower) const FRESH_CLEAR_COVER_MAX_STORES: usize = 40;
pub(in crate::ir_lower) const FRESH_CLEAR_COVER_ENTRY_BYTES: usize = FRESH_CLEAR_COVER_MAX_STORES * 8 + 7;

/// Is some GC point ([`ir_op_is_gc_point`]) scheduled in a block the
/// frequency model places inside a loop (`BlockFrequencies::depth > 0`)?
pub(in crate::ir_lower) fn ir_gc_point_in_loop(graph: &Graph, schedule: &Schedule) -> bool {
    let depth = &schedule.freq.depth;
    schedule.blocks.iter().enumerate().any(|(b, block)| {
        depth.get(b).copied().unwrap_or(0) > 0
            && block
                .nodes
                .iter()
                .any(|&id| graph.node_opt(id).is_some_and(|n| ir_op_is_gc_point(&n.op)))
    })
}

/// `CRATONVM_JIT_IR_SYNC_WINDOW_NO_CHAIN_POINTS` (round 15 wave 1, lane chain,
/// hand-back) -- **default ON**; `0` lets a deopt site inside a synchronized
/// splice's monitor window publish a caller chain again (the chain arm's
/// behaviour before, wrong under `CRATONVM_DEOPT_EAGER_CHAINS`). See
/// `Lowerer::sync_window_site_publishes_no_chain`. Read per deopt site of a
/// graph that has a window.
pub(in crate::ir_lower) fn ir_sync_window_no_chain_points_enabled() -> bool {
    true
}

/// The ops whose arms call `emit_safepoint_map` (a superset is harmless: both
/// readers, [`ir_ref_home_publication_reserve`] and [`ir_gc_point_in_loop`],
/// only size or choose a publication form).
pub(in crate::ir_lower) fn ir_op_is_gc_point(op: &Op) -> bool {
    matches!(
        op,
        Op::Call { .. }
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
            | Op::LoadStatic { .. }
            | Op::StoreStatic { .. }
            | Op::CheckCast { .. }
            | Op::InstanceOf { .. }
            | Op::MonitorEnter
            | Op::MonitorExit
            | Op::LambdaIntToDouble
            | Op::IndyBridge { .. }
            | Op::MultiNewArray { .. }
            | Op::NewCp { .. }
    )
}

/// Per-node reservation. See [`ir_code_buffer_estimate`].
pub(in crate::ir_lower) const IR_BYTES_PER_NODE: usize = 64;
/// Per-call-node reservation, on top of [`IR_BYTES_PER_NODE`]. This is the term
/// the old estimate got badly wrong: it budgeted 448, and the census puts the
/// true worst-case marginal cost of a call node at **1141 bytes**. A MIC plus a
/// 4-way PIC dual-ABI inline-cache site, its deopt/guard stub and its safepoint
/// spill run all hang off one call node.
pub(in crate::ir_lower) const IR_BYTES_PER_CALL_NODE: usize = 1536;
/// Fixed per-method reservation: prologue, epilogue, frame setup and the
/// per-method stubs, none of which are nodes.
pub(in crate::ir_lower) const IR_CODE_BUFFER_BASE: usize = 4096;

/// A/B opt-out (`CRATONVM_JIT_IR_LEGACY_BUFFER_ESTIMATE=1`): restore the
/// pre-2026-08-01 `nodes*32 + calls*448 + 1024` sizing and the 4096 floor.
///
/// It exists so the estimate change can be measured on ONE binary, both arms.
/// Buffer sizing decides which methods the optimizing tier produces bodies for
/// at all, and that tier is not uniformly faster than single-pass — a
/// same-binary A/B is the only honest way to tell a throughput change caused by
/// this from one caused by whatever else moved between two builds.
pub(in crate::ir_lower) fn legacy_ir_code_buffer_estimate() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    false
}

/// `CRATONVM_DBG_IR_BUFSIZE=1` — one line per IR compile that reaches
/// emission, reporting what the sizing model predicted against what codegen
/// actually asked for.
///
/// This is the instrument the estimate above was fitted with, and the one to
/// re-run before changing it again. It reports EVERY compile, not just the
/// overflowing ones: an estimate is only as good as its headroom on the
/// compiles that succeeded, and a census of failures alone cannot show that.
///
/// `[ir-bufsize] nodes=N calls=C wanted=W capacity=C' ratio=… overflow=bool`
pub(in crate::ir_lower) fn report_ir_buffer_size(nodes: usize, call_nodes: usize, wanted: usize, capacity: usize) {
    if !false {
        return;
    }
    // Integer permille, so the line stays greppable and needs no float
    // formatting in a path that may run thousands of times.
    let permille = if capacity == 0 {
        0
    } else {
        wanted.saturating_mul(1000) / capacity
    };
    eprintln!(
        "[ir-bufsize] nodes={nodes} calls={call_nodes} wanted={wanted} capacity={capacity} \
         permille={permille} overflow={}",
        wanted > capacity
    );
}

// ── Public entry point ───────────────────────────────────────────────

/// Lower the scheduled IR graph to x86-64 machine code.
///
/// # The load-hoisting TODO that used to live here is DONE (2026-09-16)
///
/// This doc comment carried a `TODO(round-8, HIGH from round-7 jit #9)`
/// saying there was "no Loop-Invariant Code Motion (LICM) for
/// `Op::Load`/`Op::LoadStatic` on a loop-invariant base", and prescribed a
/// four-step plan "implemented in `ir_optimize.rs` so this lowerer sees a
/// pre-hoisted graph".
///
/// That is exactly what `ir_optimize::licm` is, it is **default-ON**
/// (`licm_enabled`, opt out with `CRATONVM_JIT_LICM=0`), and it runs the
/// prescribed steps: loop headers from a back-edge scan (`loop_headers`),
/// invariance marking (`is_loop_invariant_d`), a memory-clobber /
/// alias test for the read (`loop_store_clobber`, `load_safe_past_clobber`,
/// `Graph::may_alias`), and a pre-header re-anchor. The complementary
/// within-block form is `ir_optimize::eliminate_redundant_loads`.
///
/// The TODO stayed for two releases after the pass landed, and the cost of
/// that is not cosmetic: `x64/single_pass_only.rs` decides whether the
/// optimizing tier may take a method by asking which transforms this tier
/// HAS, and it cites `ir_optimize::licm` by name as the reason the
/// single-pass `aaload`/FP hoists are not on its veto list. A stale "the IR
/// tier has no LICM" here argues for adding them to it, which would decline
/// the optimizing tier on every loop with an invariant load.
///
/// If this needs re-opening, the live gap is named at
/// `ir_optimize::optimize`: LICM runs AFTER the unroller by default, so the
/// unroller never sees the re-anchored form (`CRATONVM_JIT_IR_LICM_BEFORE_UNROLL=1`
/// flips the order). That is an ordering question, not a missing pass.
pub fn lower(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
) -> Option<CompiledMethod> {
    // No profile → empty branch hints; no scalar-deopt → None. Both default to
    // the historical byte-for-byte layout. An empty `HashMap` performs no
    // allocation until first insert.
    let empty: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    lower_inner(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        &empty,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
        &crate::ir::IrInlineFrameSites::default(),
    )
}

/// `lower` with profile-guided conditional-branch layout (wire-tiered-manager
/// Step 4 — PGO handoff C1 → C2). `branch_hints` maps a conditional-branch
/// instruction's bytecode PC to its bias (`true` = usually taken, `false` =
/// usually not taken); see `Lowerer::branch_hints`. An empty map reproduces
/// [`lower`] exactly.
pub fn lower_with_branch_hints(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
) -> Option<CompiledMethod> {
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    lower_inner(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        branch_hints,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
        &crate::ir::IrInlineFrameSites::default(),
    )
}

/// As [`lower`], but with an optional [`ScalarReplacementMap`] enabling the
/// guard-surviving scalar-replacement deopt producer (a deopt slot holding a
/// scalar-replaced `Op::New` lowers to a `FrameValue::VirtualObject`). The
/// production caller passes `Some(map)` only when `CRATONVM_SCALAR_DEOPT` and
/// `CRATONVM_DEOPT_REAL` are both set; `None` is byte-identical to the prior
/// `lower`.
pub fn lower_with_scalar_deopt(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    sr_map: Option<&ScalarReplacementMap>,
) -> Option<CompiledMethod> {
    let empty: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    lower_inner(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        &empty,
        &[],
        sr_map,
        &no_direct,
        &no_ic,
        &no_compact,
        &crate::ir::IrInlineFrameSites::default(),
    )
}

thread_local! {
/// The reason the most recent `lower_inner` on this thread refused.
///
/// A bail that names itself is the difference between "the optimizing tier
/// declined" and a fact you can act on: the C2 task pays for an IR build,
/// optimize and schedule before any of these fire, and whether that cost can be
/// avoided up front depends entirely on WHICH of them fired.
///
/// Thread-local and overwritten per compile; read it immediately after the
/// lowering call, on the same thread.
///
/// The second half (round 14 wave 3, lane fixup, RV3-1) is the `bytecode_pc`
/// of the first synchronized-splice monitor op whose window the lowerer
/// refused ([`Lowerer::sync_splice_window_refusal`]): the IR door retries the
/// build without that splice, as it does for a builder refusal (W5-2). Shared
/// with the reason rather than its own `thread_local!` for the JIT statics
/// ratchet.
pub(in crate::ir_lower) static LOWER_BAIL_REASON: std::cell::Cell<(Option<&'static str>, Option<usize>)> =
    const { std::cell::Cell::new((None, None)) };
}

pub(crate) fn note_lower_bail(reason: &'static str) {
    LOWER_BAIL_REASON.with(|c| c.set((Some(reason), c.get().1)));
}

/// Clear the reason before a lowering attempt, so a stale one cannot be read
/// as this attempt's.
pub(crate) fn clear_lower_bail() {
    LOWER_BAIL_REASON.with(|c| c.set((None, None)));
}

/// The reason the last lowering attempt on this thread refused, if it did.
pub fn last_lower_bail() -> Option<&'static str> {
    LOWER_BAIL_REASON.with(|c| c.get().0)
}

/// Record the first refused synchronized-splice window's pc (RV3-1); a later
/// refusal in the same attempt keeps the first, like `latch_bailout`.
pub(in crate::ir_lower) fn note_sync_window_refused_pc(pc: usize) {
    LOWER_BAIL_REASON.with(|c| {
        let (reason, first) = c.get();
        if first.is_none() {
            c.set((reason, Some(pc)));
        }
    });
}

/// The combined-buffer pc of the synchronized splice whose monitor window the
/// last lowering attempt on this thread refused, if it refused one
/// (round 14 wave 3, lane fixup, RV3-1). Read by the IR door right after the
/// lowering call, to rebuild without that splice.
pub(crate) fn last_sync_window_refused_pc() -> Option<usize> {
    LOWER_BAIL_REASON.with(|c| c.get().1)
}


/// Shared lowering body: profile-guided branch hints, the optional
/// guard-surviving scalar-replacement map, and the two per-call-site lowering
/// tables all flow in here. `pub(crate)` so the production compile path
/// (`lib.rs`) can supply all of them at once.
///
/// **The code buffer is sized by estimate, and an overflow is retried ONCE at
/// the exact size** (round 9 wave 2, in [`lower_inner_with_array_list`]; round 9
/// wave 1 had corrected this paragraph from "Sizes the code buffer by
/// retrying", which was then untrue — the `lib.rs` retry keyed on
/// `CODE_BUFFER_TOO_SMALL_SITE` serves the single-pass backend, not this one).
/// The measured `ir_code_buffer_estimate` still decides the first attempt; the
/// history below is why that estimate exists. The original estimate
/// (`nodes * 32 + calls * 448 + 1024`) budgeted one number for a call site
/// whose real cost swings by several hundred bytes depending on which lowering
/// it selects — a MIC + 4-way-PIC dual-ABI inline cache is the expensive end,
/// and widening the PIC's inter-slot branch from `rel8` to `rel32` pushed it
/// past the budget. The result was a silent de-optimization: `emit` drops the
/// write, sets the sticky `overflowed` flag, and the method quietly stays
/// interpreted forever. A single Spring Boot suite class produced **8072** such
/// warnings in one run, every one of them from this estimate (the report that
/// first noticed the flood,
/// `basicerrorcontroller-jit-only-failure-20260731-FIXED.md`,
/// attributed them to the single-pass backend's estimate — that one accounted
/// for 10).
///
/// `ExecutableBuffer::wanted()` counts every byte codegen asked for, including
/// the writes dropped after the overflow, so a retry at that size is exact
/// rather than another guess — and since round 9 wave 2 that retry happens,
/// once, in [`lower_inner_with_array_list`]. Raising the constant instead would
/// have to over-reserve every ordinary method to cover the worst one, and
/// every reserved byte counts against the code-cache cap.
#[allow(clippy::too_many_arguments)]
/// `CRATONVM_JIT_IR_DIRECT_CALL_IMM_ALIGN` (JIT r23 wave 10, lane L6; perf,
/// **default ON**, `=0` kill): optimizing-tier direct cross calls
/// (`emit_direct_cross_call`) and patchable single-pass calls to compiled code
/// (`emit_patchable_abs64_call`) use `MOV RAX, imm64 ; CALL RAX`, NOP-padded so
/// the imm64 starts on an 8-byte boundary. Only such a site can be rebound in
/// place when its callee republishes (`JitCache::rebind_stale_call_site`,
/// `CRATONVM_JIT_STALE_CALL_REBIND_ABS64`). Every other site takes a
/// full recompile of the caller (`stale-caller recompile`), which for
/// `Pattern.compile()V` (a 110 KB body) happened 2-6 times per run. HotSpot
/// aligns its patchable call displacements the same way (`align_call`).
/// Read per compile.
pub(crate) fn ir_direct_call_imm_align_enabled() -> bool {
    true
}

/// NOP bytes to emit at absolute address `at` so that a following `REX.W B8`
/// (two bytes) puts its imm64 on an 8-byte boundary: `0..=7`.
pub(crate) fn direct_call_imm_align_pad(at: usize) -> usize {
    (8 - (at.wrapping_add(2) & 7)) & 7
}

/// Emit `n` (`0..=7`) bytes of padding as ONE recommended multi-byte NOP
/// (Intel SDM vol. 2B, `NOP`), so the pad costs one decoded instruction.
pub(crate) fn emit_multibyte_nop(buf: &mut ExecutableBuffer, n: usize) {
    const NOPS: [&[u8]; 8] = [
        &[],
        &[0x90],
        &[0x66, 0x90],
        &[0x0F, 0x1F, 0x00],
        &[0x0F, 0x1F, 0x40, 0x00],
        &[0x0F, 0x1F, 0x44, 0x00, 0x00],
        &[0x66, 0x0F, 0x1F, 0x44, 0x00, 0x00],
        &[0x0F, 0x1F, 0x80, 0x00, 0x00, 0x00, 0x00],
    ];
    if let Some(bytes) = NOPS.get(n) {
        if !bytes.is_empty() {
            buf.emit(bytes);
        }
    }
}


pub(crate) fn lower_inner(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    spliced_ranges: &[(usize, usize, usize)],
    sr_map: Option<&ScalarReplacementMap>,
    direct_calls: &HashMap<usize, (usize, bool)>,
    ic_slots: &HashMap<usize, (usize, usize)>,
    compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
    inline_frame_sites: &crate::ir::IrInlineFrameSites,
) -> Option<CompiledMethod> {
    lower_inner_with_direct_helpers(
        &crate::DirectHelperTable::EMPTY,
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        branch_hints,
        spliced_ranges,
        sr_map,
        direct_calls,
        ic_slots,
        compact_fields,
        inline_frame_sites,
    )
}

/// [`lower_inner`] for a compile that carries the VM's direct-helper table,
/// which the IR `getstatic` lowering asks for a static slot's base.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_inner_with_direct_helpers(
    direct_helpers: &crate::DirectHelperTable,
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    spliced_ranges: &[(usize, usize, usize)],
    sr_map: Option<&ScalarReplacementMap>,
    direct_calls: &HashMap<usize, (usize, bool)>,
    ic_slots: &HashMap<usize, (usize, usize)>,
    compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
    inline_frame_sites: &crate::ir::IrInlineFrameSites,
) -> Option<CompiledMethod> {
    // The inline FRAME sites are forwarded, not emptied: they are a different
    // table with a different consumer (stack traces, not deopt).
    lower_inner_with_array_list(
        direct_helpers,
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        branch_hints,
        spliced_ranges,
        sr_map,
        direct_calls,
        ic_slots,
        compact_fields,
        inline_frame_sites,
        &HashMap::new(),
        &std::collections::HashSet::new(),
        None,
    )
}

// [`lower_inner_with_direct_helpers`] plus the compile-time recursion-cycle
// closing edges this compile planned as [`crate::JitCycleEdgeCell`] sites,
// `pc -> cell address` (round 9 wave 8, irl8; the IR half of
// `perf-mutual-recursion-forces-every-call-through-invoke-dispatch`).
//
// Contract for the producer (`lib.rs`'s IR door): a row is planned exactly as
// `build_single_pass_tables` plans the single-pass one -- an `invokestatic`
// closing edge (`note_jit_recursive_compile_cycle` answered `true`), not
// self-recursive, `jit_cycle_edge_cell_enabled()`,
// `jit_cycle_direct_bind_enabled()`, `direct_jit_callee_calls_enabled()`,
// the target not sticky, `helpers.self_call_stack_guard != 0`,
// `helpers.invoke_dispatch != 0`, and the VM's `cycle_edge_admission`
// admitting the target -- with a context-ABI cell
// (`JitCycleEdgeCell::new(.., true)`) whose `Arc` the resulting
// `CompiledMethod` owns in `_jit_cycle_cells` before it is published. The
// site keeps its `Op::Call` and `JitInvokeInfo` (the dispatch fallback and
// the deopt service read it) and gets NO `direct_calls` row. A row the
// lowerer cannot serve (an argument past the entry-ABI register file, a
// missing helper, a non-static `invoke_kind`) silently keeps dispatch.
// `lib.rs`'s IR door plans the cells (round 9 wave 8b, NOTES-w8-irl8.md
// cross-lane request 2).
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_inner_with_cycle_cells(
    direct_helpers: &crate::DirectHelperTable,
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    spliced_ranges: &[(usize, usize, usize)],
    sr_map: Option<&ScalarReplacementMap>,
    direct_calls: &HashMap<usize, (usize, bool)>,
    ic_slots: &HashMap<usize, (usize, usize)>,
    compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
    inline_frame_sites: &crate::ir::IrInlineFrameSites,
    cycle_cells: &HashMap<usize, usize>,
    // Round 9 wave 8b (irl8 request 3): classes whose inline-TLAB `new`
    // announces to ZGC instead of calling `tlab_post_init`. Empty keeps the
    // historical call.
    tlab_announce_classes: &std::collections::HashSet<u32>,
) -> Option<CompiledMethod> {
    lower_inner_with_array_list(
        direct_helpers,
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        branch_hints,
        spliced_ranges,
        sr_map,
        direct_calls,
        ic_slots,
        compact_fields,
        inline_frame_sites,
        cycle_cells,
        tlab_announce_classes,
        None,
    )
}

/// [`lower_inner_with_cycle_cells`] plus THIS compile's `java/util/ArrayList`
/// layout (round 9 wave 9, irl9), which arms the optimizing tier's inline
/// `ArrayList.get(int)` / `size()` prefix on every `Op::Call` site
/// `crate::try_resolve_arraylist_intrinsic` names
/// (`Lowerer::try_emit_ir_arraylist_access_prefix`).
///
/// Contract for the producer (`lib.rs`'s IR door): pass
/// `resolved_string_layout.and_then(|l| l.array_list)` -- the layout the same
/// compile hands the single-pass tier, never a process-wide latch
/// (the class id and compact offsets are per VM). `None` is byte-identical to
/// [`lower_inner_with_cycle_cells`].
///
/// It absorbed `lower_inner_with_scopes` when round 10 retired
/// `ir::InlineScopeTable` (that function's only extra parameter was the scope
/// table, which every caller passed empty — see the retirement note in
/// `ir.rs`). Since round 11 wave 10 the FULL form, and the retry logic below,
/// live one level down in [`lower_inner_with_protected_ranges`]; this passes
/// it no protected ranges.
///
/// # A code-buffer overflow is retried once (round 9 wave 2)
///
/// The buffer is sized by [`ir_code_buffer_estimate`]; a method that outgrows
/// it used to be refused (`CodeBufferExhausted`) and so lost its optimizing
/// body for good. `ExecutableBuffer::wanted()` is the EXACT requirement — this
/// backend never rewinds, so it counts every byte codegen asked for, dropped
/// writes included — so the first attempt's overflow is not refused: the
/// method is lowered again, once, into a buffer of `wanted()` plus a little
/// slack (the one size-varying encoding is the RIP-relative poll, whose reach
/// depends on where the buffer lands). Only a second overflow refuses.
///
/// What a discarded first attempt leaves behind: its compile-id reservation is
/// dropped (released) with its `Lowerer`; the unroller's thread-local flag is
/// taken ONCE, here, and handed to both attempts; the process-wide diagnostic
/// census counters (`IR_POLLS_*`, `FALLTHROUGHS_ELIDED`, the MIR totals, ...)
/// do count the discarded attempt too — diagnostic skew on a rare path,
/// accepted rather than threading a rollback through every counter.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_inner_with_array_list(
    direct_helpers: &crate::DirectHelperTable,
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    spliced_ranges: &[(usize, usize, usize)],
    sr_map: Option<&ScalarReplacementMap>,
    direct_calls: &HashMap<usize, (usize, bool)>,
    ic_slots: &HashMap<usize, (usize, usize)>,
    compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
    inline_frame_sites: &crate::ir::IrInlineFrameSites,
    cycle_cells: &HashMap<usize, usize>,
    tlab_announce_classes: &std::collections::HashSet<u32>,
    array_list: Option<crate::ArrayListFieldLayout>,
) -> Option<CompiledMethod> {
    lower_inner_with_protected_ranges(
        direct_helpers,
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        branch_hints,
        spliced_ranges,
        sr_map,
        direct_calls,
        ic_slots,
        compact_fields,
        inline_frame_sites,
        cycle_cells,
        tlab_announce_classes,
        array_list,
        &[],
        Some(&[][..]),
    )
}

/// [`lower_inner_with_array_list`] plus the method's protected ranges
/// `[start_pc, end_pc)` (round 11 wave 10, lane irexc) — the FULL form every
/// other `lower_inner*` wrapper now funnels into, and the one `lib.rs`'s IR
/// door calls.
///
/// Contract for the producer: pass the exception table's ranges ONLY on the
/// precise-frame route (an RBC.6 method admitted under
/// `CRATONVM_JIT_IR_PRECISE_HANDLER_FRAMES=1`). A non-empty list puts a
/// reason-9 rethrow pad on every call-exception exit inside it
/// (`Lowerer::push_rethrow_pad`); empty is byte-identical to
/// [`lower_inner_with_array_list`].
///
/// `osr_handler_reachable` (round 11 wave 11, lane irexc) is
/// `deopt::pcs_reachable_from_handlers` over the same method: the bcis at
/// which a body with a rethrow pad may NOT offer an OSR entry
/// (`Lowerer::osr_entry_closed_by_a_handler`). `None` withholds every entry
/// of such a body; `Some(&[])` with empty ranges is byte-identical to the
/// form without it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_inner_with_protected_ranges(
    direct_helpers: &crate::DirectHelperTable,
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    spliced_ranges: &[(usize, usize, usize)],
    sr_map: Option<&ScalarReplacementMap>,
    direct_calls: &HashMap<usize, (usize, bool)>,
    ic_slots: &HashMap<usize, (usize, usize)>,
    compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
    inline_frame_sites: &crate::ir::IrInlineFrameSites,
    cycle_cells: &HashMap<usize, usize>,
    tlab_announce_classes: &std::collections::HashSet<u32>,
    array_list: Option<crate::ArrayListFieldLayout>,
    protected_ranges: &[(u32, u32)],
    osr_handler_reachable: Option<&[u32]>,
) -> Option<CompiledMethod> {
    lower_inner_with_mode_exit_polls(
        direct_helpers,
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        branch_hints,
        spliced_ranges,
        sr_map,
        direct_calls,
        ic_slots,
        compact_fields,
        inline_frame_sites,
        cycle_cells,
        tlab_announce_classes,
        array_list,
        protected_ranges,
        osr_handler_reachable,
        IrPollModeExits::Off,
        None,
        None,
    )
}

/// [`lower_inner_with_protected_ranges`] with the back-edge polls' mode exits
/// (interpreter round i1 wave 15, lane L2): `poll_mode_exits` other than
/// [`IrPollModeExits::Off`] lets an unconditional back-edge poll leave at its
/// loop header when its slow path says the frame (`OsrDoor`, from
/// `CompileRequest::osr_mode_exit_polls`) or the body (`MethodEntry`, wave
/// 18) must run interpreted (`Lowerer::back_edge_mode_exit_state`). `Off` is
/// byte-identical to the form without it. The one `lib.rs` IR door calls
/// this.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_inner_with_mode_exit_polls(
    direct_helpers: &crate::DirectHelperTable,
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    spliced_ranges: &[(usize, usize, usize)],
    sr_map: Option<&ScalarReplacementMap>,
    direct_calls: &HashMap<usize, (usize, bool)>,
    ic_slots: &HashMap<usize, (usize, usize)>,
    compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
    inline_frame_sites: &crate::ir::IrInlineFrameSites,
    cycle_cells: &HashMap<usize, usize>,
    tlab_announce_classes: &std::collections::HashSet<u32>,
    array_list: Option<crate::ArrayListFieldLayout>,
    protected_ranges: &[(u32, u32)],
    osr_handler_reachable: Option<&[u32]>,
    poll_mode_exits: IrPollModeExits,
    poll_exit_bytecode: Option<PollExitBytecode<'_>>,
    // gen r5w2/oomjit6: the method's own bytecode, unpadded, for the opt-in
    // dead-reference-home clearing (`DeadRefSlotClear`). `None` clears
    // nothing, and is what every caller but `lib.rs`'s IR door passes.
    // gen r5w6/oomjit10: with what its local liveness needs.
    dead_ref_clear_code: Option<DeadRefClearBytecode<'_>>,
) -> Option<CompiledMethod> {
    // Consume the unroller's thread-local "I relied on unreachable frame
    // states" flag FIRST, before any refusal below can return: it describes the
    // graph this call was handed, and a refusal that left it set would hand it
    // to whatever compile runs next on this thread. Read at the deopt-point
    // check near the end of each attempt.
    let unroll_used_unreachable_frames = crate::ir_optimize::take_unroll_used_unreachable_frames();
    // Round nio20 wave 5 (lane L3): the flat bcis a pure-body full unroll
    // stranded, taken here for the same reason; checked against the emission's
    // transfer bcis near the end of each attempt.
    let unroll_stranded_bcis = crate::ir_optimize::take_unroll_stranded_frames();
    // The acceptance gate's per-compile cost record, as it stood before either
    // attempt charged a blind dispatch to it. See the retry below.
    let blind_dispatch_charge = crate::ir_evidence::blind_dispatch_charge();
    // nio21 wave 3 (lane L5, proposal L5W2-4): the retry ladder below lowers
    // the SAME graph up to four times (estimated size, exact size, and both
    // again with the deopt image kept). Each extra lowering is named under
    // `CRATONVM_DBG_IR_COMPILES` / `CRATONVM_DBG_JITC`, with the graph size,
    // so a compile-cost census can tell a retry from a second compile request
    // (the `ir-post-call-exit site` lines print once per LOWERING).
    let dbg_ladder = crate::x64::dbg_jitc_enabled()
        || false;
    if dbg_ladder {
        eprintln!(
            "[ir] lowering-ladder start: {} graph node(s), {} block(s)",
            graph.nodes.len(),
            schedule.blocks.len()
        );
    }
    // One lowering at the estimated size, and the one exact-size retry an
    // overflow earns. `keep_image` / `dropped`: see `lower_inner_attempt`.
    let run = |keep_image: bool, dropped: &mut bool| -> Option<CompiledMethod> {
        let mut overflow_wanted: Option<usize> = None;
        *dropped = false;
        let first = lower_inner_attempt(
            direct_helpers,
            graph,
            schedule,
            num_params,
            num_locals,
            helpers,
            branch_hints,
            spliced_ranges,
            sr_map,
            direct_calls,
            ic_slots,
            compact_fields,
            inline_frame_sites,
            cycle_cells,
            tlab_announce_classes,
            array_list,
            protected_ranges,
            osr_handler_reachable,
            poll_mode_exits,
            poll_exit_bytecode,
            dead_ref_clear_code,
            unroll_used_unreachable_frames,
            &unroll_stranded_bcis,
            None,
            Some(&mut overflow_wanted),
            keep_image,
            dropped,
        );
        if first.is_some() {
            return first;
        }
        // `None` with no overflow recorded is an ordinary refusal, already counted.
        let wanted = overflow_wanted?;
        IR_BUFFER_OVERFLOW_RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Round 11 (irlower): the discarded attempt already charged its blind
        // dispatches to THIS compile's evidence record, and the retry lowers the
        // same graph and charges them again. The census counters may double-count
        // (see above); the gate's input may not -- a doubled charge priced a large
        // spliced body as a regression and refused it.
        if let Some(charge) = blind_dispatch_charge {
            crate::ir_evidence::restore_blind_dispatch_charge(charge);
        }
        let retry_capacity = wanted
            .saturating_add(wanted / 16)
            .saturating_add(IR_BUFFER_RETRY_SLACK);
        if dbg_ladder {
            eprintln!(
                "[ir] lowering-ladder retry: buffer overflow (wanted {wanted} bytes), \
                 lowering again at {retry_capacity} (image kept: {keep_image})"
            );
        }
        *dropped = false;
        lower_inner_attempt(
            direct_helpers,
            graph,
            schedule,
            num_params,
            num_locals,
            helpers,
            branch_hints,
            spliced_ranges,
            sr_map,
            direct_calls,
            ic_slots,
            compact_fields,
            inline_frame_sites,
            cycle_cells,
            tlab_announce_classes,
            array_list,
            protected_ranges,
            osr_handler_reachable,
            poll_mode_exits,
            poll_exit_bytecode,
            dead_ref_clear_code,
            unroll_used_unreachable_frames,
            &unroll_stranded_bcis,
            Some(retry_capacity),
            None,
            keep_image,
            dropped,
        )
    };
    let mut dropped = false;
    // JIT round 25 item 1a (wave 2): the values a refused attempt's elisions
    // were refused for are excluded one by one (`ElideRetryScope`), so one
    // misprediction costs that value its elision, not the body every one.
    let elide_scope = ElideRetryScope::enter();
    let mut lowered = run(false, &mut dropped);
    let mut value_retries = 0usize;
    while lowered.is_none() && dropped && value_retries < ELIDE_VALUE_RETRIES_MAX {
        if !elide_scope.exclude_refused() {
            break;
        }
        value_retries += 1;
        if let Some(charge) = blind_dispatch_charge {
            crate::ir_evidence::restore_blind_dispatch_charge(charge);
        }
        if dbg_ladder {
            eprintln!(
                "[cratonvm-jitc] ir-home-elide refused for {} value(s); lowering again without them",
                elide_scope.excluded_len()
            );
        }
        lowered = run(false, &mut dropped);
    }
    drop(elide_scope);
    if lowered.is_some() || !dropped {
        return lowered;
    }
    // Round 16 wave 13 (lane polyclear): the refused attempt had dropped the
    // deopt register image (`CRATONVM_JIT_IR_DEOPT_IMAGE_EXITLESS`), whose
    // prediction (`Lowerer::deopt_image_never_read`) fails closed: an exit that
    // did need the image refuses the compile. Lower once more with the image
    // kept, so a misprediction costs a second lowering instead of the body.
    // The refused attempt charged its blind dispatches like the overflow
    // retry's discarded one; restore the record the same way.
    if let Some(charge) = blind_dispatch_charge {
        crate::ir_evidence::restore_blind_dispatch_charge(charge);
    }
    if dbg_ladder {
        eprintln!(
            "[cratonvm-jitc] ir-deopt-image dropped and the attempt was refused; lowering again with the image kept"
        );
    }
    run(true, &mut dropped)
}

thread_local! {
    /// The String layout of the compile currently lowering on this thread;
    /// see [`CompileStringLayoutScope`].
    pub(in crate::ir_lower) static COMPILE_STRING_LAYOUT: std::cell::Cell<Option<crate::StringFieldLayout>> =
        const { std::cell::Cell::new(None) };
}

/// nio20 wave 3, lane L2: hands THIS compile's `java/lang/String` layout to
/// the lowerer ([`Lowerer::try_emit_ir_string_search_prefix`]) without a
/// new parameter on [`lower_inner_with_mode_exit_polls`] and its dozen test
/// callers.
///
/// A per-THREAD, per-compile value, not a process latch: `lib.rs`'s IR door
/// enters the scope immediately around its one lowering call (on the
/// compiling thread), `lower_inner_attempt` reads it on that same thread, and
/// `Drop` restores whatever was there before -- so a nested compile on the
/// same thread (a callee compiled from inside a compile) cannot leak its
/// layout into, or take it from, the outer one. Outside a scope the value is
/// `None`, which emits no prefix: every unit test and every other door lowers
/// byte-for-byte as before.
pub(crate) struct CompileStringLayoutScope {
    pub(in crate::ir_lower) previous: Option<crate::StringFieldLayout>,
}

impl CompileStringLayoutScope {
    /// Install `layout` for the rest of this scope on this thread.
    pub(crate) fn enter(layout: Option<crate::StringFieldLayout>) -> Self {
        let previous = COMPILE_STRING_LAYOUT.with(|c| c.replace(layout));
        CompileStringLayoutScope { previous }
    }

    /// The layout of the innermost open scope on this thread, if any.
    pub(in crate::ir_lower) fn current() -> Option<crate::StringFieldLayout> {
        COMPILE_STRING_LAYOUT.with(std::cell::Cell::get)
    }
}

impl Drop for CompileStringLayoutScope {
    fn drop(&mut self) {
        let previous = self.previous;
        COMPILE_STRING_LAYOUT.with(|c| c.set(previous));
    }
}

thread_local! {
    /// Whether a debugger or a JVMTI agent may read the locals of the compile
    /// currently lowering on this thread; see [`CompileLocalsObservedScope`].
    pub(in crate::ir_lower) static COMPILE_LOCALS_OBSERVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// JIT round 21 wave 24 (lane flipprep, item 2 of
/// `r21-optin-flip-review-FIXED-20261021.md`): hands THIS compile's "an agent may
/// read locals" answer (`CompileRequest::debugger_observes_locals` or
/// `CompileRequest::jvmti_locals_accessible`) to
/// [`Lowerer::volatile_store_fence_elided`], which then computes the
/// fresh-allocation unfenced volatile-store set empty
/// (`CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED`): an agent that reads the
/// new object from a local at a safepoint between the allocation and the
/// store is an observer the escape rule does not see. HotSpot (JDK 16+,
/// JDK-8227745) keeps its escape analysis under `can_access_local_variables`
/// and deoptimizes the frame on access through escape barriers; with no such
/// barrier here, the elision declines instead.
///
/// Per thread and per compile, as [`CompileStringLayoutScope`]: `lib.rs`'s IR
/// door enters it around its one lowering call, `Drop` restores the previous
/// value (a nested compile cannot leak its answer), and outside a scope the
/// answer is `false`, so every unit test and every other door lowers exactly
/// as before. With the switch off the answer is never read.
pub(crate) struct CompileLocalsObservedScope {
    pub(in crate::ir_lower) previous: bool,
}

impl CompileLocalsObservedScope {
    /// Install `observed` for the rest of this scope on this thread.
    pub(crate) fn enter(observed: bool) -> Self {
        let previous = COMPILE_LOCALS_OBSERVED.with(|c| c.replace(observed));
        CompileLocalsObservedScope { previous }
    }

    /// The answer of the innermost open scope on this thread (`false` when
    /// none is open).
    pub(in crate::ir_lower) fn current() -> bool {
        COMPILE_LOCALS_OBSERVED.with(std::cell::Cell::get)
    }
}

impl Drop for CompileLocalsObservedScope {
    fn drop(&mut self) {
        let previous = self.previous;
        COMPILE_LOCALS_OBSERVED.with(|c| c.set(previous));
    }
}

/// Bytes added to `wanted()` for the one retry [`lower_inner_with_array_list`]
/// makes after an overflow. See the note there on why the second attempt can
/// differ at all.
pub(in crate::ir_lower) const IR_BUFFER_RETRY_SLACK: usize = 256;

/// Compiles that overflowed their first buffer and were lowered again at the
/// exact size (round 9 wave 2). Diagnostic; read by
/// [`ir_buffer_overflow_retries`].
pub(in crate::ir_lower) static IR_BUFFER_OVERFLOW_RETRIES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many IR compiles were retried after a code-buffer overflow. A non-zero
/// count is the signal to refit `IR_BYTES_PER_NODE` / `IR_BYTES_PER_CALL_NODE`
/// with `CRATONVM_DBG_IR_BUFSIZE`.
#[allow(dead_code)]
pub fn ir_buffer_overflow_retries() -> u64 {
    IR_BUFFER_OVERFLOW_RETRIES.load(std::sync::atomic::Ordering::Relaxed)
}

/// One lowering of the graph into one buffer — [`lower_inner_with_array_list`]'s
/// body. `capacity_override` replaces the estimate (the retry passes the first
/// attempt's `wanted()`); `overflow_wanted`, when present, turns an overflow
/// into "record `wanted()` there and return `None` WITHOUT refusing", which is
/// what lets the caller retry without counting a bailout it then avoided.
#[allow(clippy::too_many_arguments)]
/// One compile's inline-frame-row census (interpreter round i1 wave 49, lane
/// L3; [`Lowerer::begin_inline_row_census`]).
#[derive(Debug, Default)]
pub(in crate::ir_lower) struct InlineRowCensus {
    /// `(native offset, Some(node pc) when the node lies inside a spliced
    /// body)` where each lowered node's code starts, ascending.
    pub(in crate::ir_lower) marks: Vec<(u32, Option<usize>)>,
    /// Where the last node's code ended (the out-of-line code starts).
    pub(in crate::ir_lower) closed_at: Option<u32>,
}

/// What [`classify_inline_row_census`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::ir_lower) struct InlineRowCensusVerdict {
    /// Calls emitted for a node inside a spliced body.
    pub(in crate::ir_lower) spliced: usize,
    /// Those of them whose return address has no inline-frame row, as
    /// `(return address, node pc)`.
    pub(in crate::ir_lower) rowless: Vec<(u32, usize)>,
    /// Calls emitted after the last node (stubs, pads, tails): not attributed.
    pub(in crate::ir_lower) out_of_line: usize,
}

/// Attribute each call of `calls` (return addresses, ascending) to the node
/// whose code it lies in -- the last mark at or before the call's first byte,
/// i.e. strictly before its return address -- and count the ones inside a
/// spliced body whose return address `filed` (the rows' native offsets) does
/// not name.
pub(in crate::ir_lower) fn classify_inline_row_census(
    census: &InlineRowCensus,
    calls: &[u32],
    filed: &std::collections::HashSet<u32>,
) -> InlineRowCensusVerdict {
    let mut verdict = InlineRowCensusVerdict::default();
    for &ret in calls {
        if census.closed_at.is_some_and(|end| ret > end) {
            verdict.out_of_line += 1;
            continue;
        }
        let k = census.marks.partition_point(|&(at, _)| at < ret);
        let Some(&(_, Some(pc))) = k.checked_sub(1).and_then(|i| census.marks.get(i)) else {
            continue;
        };
        verdict.spliced += 1;
        if !filed.contains(&ret) {
            verdict.rowless.push((ret, pc));
        }
    }
    verdict
}

