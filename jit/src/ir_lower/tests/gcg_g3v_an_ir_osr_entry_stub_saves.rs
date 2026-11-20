#[allow(unused_imports)]
use super::*;

/// gcg g3/v (`gcg-g1f-ir-lower-osr-save-area-...`): the invariant the
/// removed `debug_assert!` was written for, stated where it actually
/// holds. This tier's OSR entry stub saves the caller's GPRs and XMMs from
/// the same iterators `emit_prologue` saves from (so every exit restores
/// what was saved), which is also why `CompiledMethod::ir_saved_gpr_slots`
/// is published for OSR bodies. A source pin: an edit that drops either
/// save from the stub, or moves the prologue off these iterators, fails
/// here.
#[test]
pub(in crate::ir_lower::tests) fn gcg_g3v_an_ir_osr_entry_stub_saves_what_the_prologue_saves() {
    let mut src = String::new();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ir_lower");
    for entry in std::fs::read_dir(&dir)
        .expect("read jit/src/ir_lower")
        .flatten()
    {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "rs")
            && p.file_name().is_some_and(|n| {
                let s = n.to_string_lossy();
                s != "tests.rs" && !s.ends_with("_tests.rs")
            })
        {
            src.push_str(
                &std::fs::read_to_string(&p)
                    .expect("read ir_lower part")
                    .replace("\r\n", "\n"),
            );
            src.push('\n');
        }
    }
    let body = |name: &str| -> String {
        let head = format!("fn {name}(");
        let start = src
            .find(&head)
            .unwrap_or_else(|| panic!("{name} not found"));
        let rest = &src[start + head.len()..];
        let end = rest.find("\n    pub").unwrap_or(rest.len());
        rest[..end].to_string()
    };
    for f in ["emit_prologue", "emit_osr_entry_stubs"] {
        let b = body(f);
        assert!(b.contains("self.saved_gpr_regs()"), "{f} saves the GPRs");
        assert!(b.contains("self.saved_xmm_regs()"), "{f} saves the XMMs");
    }
    // And the publication no longer reads the single-pass table.
    let finish = src
        .split("cm.ir_saved_gpr_slots =")
        .nth(1)
        .and_then(|s| s.split(';').next())
        .expect("the save-list publication");
    assert!(!finish.contains("osr_pc_to_native"), "{finish}");
}

/// With the linear-scan path off — the default, and every production
/// compile today — the frame pays nothing.
///
/// A save area that widened every IR frame by 32 bytes for a register
/// nothing can hand out is a regression with no upside, and `frame_size`
/// feeds `DEFAULT_MAX_FRAME_BYTES`, so it would also decline methods that
/// used to compile.
///
/// 2026-09-02: the file is default-ON now, so this pins the OFF arm
/// (`CRATONVM_JIT_IR_LINEAR_SCAN=0`) rather than the process default.
#[test]
pub(in crate::ir_lower::tests) fn the_default_configuration_reserves_no_save_area() {
    let _off = LsForce::off();
    assert_eq!(
        ir_saved_xmm_bytes(),
        0,
        "the flag is off, so the frame must not pay for the save area",
    );
    let lo = lowerer_with_resident_xmm(4096, Some(6));
    assert_eq!(lo.saved_xmm_bytes, 0);
    assert_eq!(lo.saved_xmm_regs().count(), 0);
}

/// The publication interlock, asserted on the source because it is a
/// property of *which accessor* every read uses, and a behavioural test
/// cannot reach a lowering that forgot to convert a definition site.
///
/// `reg_of` says which register a value was ASSIGNED; `reg_live` says the
/// definition has actually written it. Every read must go through
/// `resident_xmm` (which checks `reg_live`) or `assigned_xmm` (which is
/// only for the two publishing sites). A definition arm nobody converted
/// must therefore cost an optimization, never a read of a register that
/// was never written.
#[test]
pub(in crate::ir_lower::tests) fn the_register_read_path_is_gated_on_publication() {
    // Only the emitter, not this module: the assertion below quotes the
    // very strings it looks for.
    //
    // The boundary is the test module, NAMED. It used to be "everything up
    // to the first `#[cfg(test)]`", which is a position rather than a
    // boundary: the first test-only helper added anywhere above
    // `set_residency` truncates the scanned region and the count silently
    // collapses to zero — a gate that fails open. (`FrameAccessList::
    // corrupt_last_byte` is the one that did it.) The precondition below is
    // the other half: a scan whose corpus went missing must say so.
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    );
    let body = src.split("\nmod tests {").next().unwrap_or(src);
    assert!(
        body.contains("fn resident_xmm"),
        "the scanned region no longer contains the emitter — the test \
             module boundary moved"
    );
    let reads: Vec<String> = body
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| l.contains("self.reg_of") && !l.starts_with("//"))
        .collect();
    let allowed = [
        "self.reg_of = residency.reg_of;",
        "self.reg_of.get(id as usize).copied().flatten()",
        // `set_residency`, building the save-set SNAPSHOT. The prologue and
        // every epilogue used to read `self.reg_of` directly here, which
        // was correct only while `reg_of` never changed after installation
        // — and split residency is exactly the thing that changes it. A
        // prologue that saved XMM6 followed by an exit that no longer found
        // XMM6 in `reg_of` returns to its caller with XMM6 destroyed, so
        // the question "which registers does this frame owe its caller" is
        // now answered once, here, and read from `ls_saved_fp` everywhere
        // else. That is why `saved_xmm_regs` no longer appears in this
        // list.
        "for reg in self.reg_of.iter().flatten() {",
        // nio21 wave 5 (implicit exception edges, `plan_implicit_throw_site`):
        // asks only WHETHER any value has an XMM assignment, to refuse a pad
        // whose helper calls would clobber it. It reads no register.
        "if self.reg_of.iter().any(Option::is_some) {",
        // `ls_set_fp_reg` — the ONE site that makes the assignment change
        // after installation, and the reason every whole-method reader had
        // to move to a snapshot. It is a WRITE, not a read, and it is
        // deliberately the only one: a second would mean two places that
        // can move a value's register out from under `resident_xmm`.
        "if let Some(cell) = self.reg_of.get_mut(id as usize) {",
        // `set_residency`, building the ORIGIN snapshot, and admitted for
        // the same reason as the save-set one above: it reads the file at
        // installation, before anything can have moved.
        //
        // `ls_split_registers` used to derive a value's starting register
        // from `assigned_xmm` / `assigned_gpr`, i.e. from where the value
        // is NOW. That is stable only while residency is, and split
        // residency is precisely what makes it move -- so after the first
        // transition the register a value started in stopped being
        // reported as one it passes through. Harmless for the save set,
        // which is built before any transition; NOT harmless for the other
        // caller, where the answer decides which registers may be named in
        // a deopt frame state. A register that dropped out of it would be
        // called exclusively owned while a managed value was still
        // visiting it.
        "self.ls_origin = (self.gp_reg_of.clone(), self.reg_of.clone());",
    ];
    for line in &reads {
        assert!(
            allowed.contains(&line.as_str()),
            "new read of the register assignment outside `resident_xmm` / \
                 `assigned_xmm` / `ls_set_fp_reg` / the save-area snapshot: {line}"
        );
    }
    assert_eq!(
        reads.len(),
        7,
        "expected exactly the install site, the two accessors, the \
             save-area snapshot, the ORIGIN snapshot, the one writer and the \
             implicit-throw pad's any-assignment question"
    );
}

/// The headline. A value the allocator gave a register everywhere except
/// across one call holds that register on BOTH sides of the call, and is
/// read from its home word only at the call itself.
///
/// Before this, `plan_register_residency`'s `_` arm declined the value
/// outright and it paid a home store plus a reload at *every* use -- the
/// emission it had before the allocator existed.
#[test]
pub(in crate::ir_lower::tests) fn a_value_split_across_a_call_keeps_a_register_on_both_sides() {
    let (graph, schedule, sum, call) = value_live_across_a_call_graph();
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    assert!(
        live.converged,
        "the fixture's liveness fixed point converges"
    );
    let plan = plan_slots(&graph, &schedule, None);
    let node_pos = ls_node_positions(&schedule, &live);
    let def = live.pos_of[sum as usize].expect("the split value is scheduled");
    let call_pos = live.pos_of[call as usize].expect("the call is scheduled");
    let range = live.range[sum as usize].expect("the split value wants a location");
    assert!(
        range.lo <= def && def < call_pos && call_pos < range.hi,
        "the fixture must define the value before the call and read it \
             after: def={def} call={call_pos} range=[{}, {}]",
        range.lo,
        range.hi,
    );
    let reload_at = call_pos + 1;
    let alloc = split_allocation(
        graph.nodes.len(),
        sum,
        range,
        call_pos,
        crate::regalloc::PhysReg::gp(RBX),
    );

    let (gp, first, transitions) = ls_split_place(
        &graph,
        &plan,
        &live,
        &alloc,
        sum as usize,
        alloc.segments[sum as usize].as_slice(),
        &node_pos,
    )
    .expect("a value split across one call is placeable");
    assert!(gp, "an `int` belongs to the general-purpose file");
    assert_eq!(
        first,
        Some(RBX),
        "the definition publishes into the register the allocator gave it",
    );
    assert_eq!(
        transitions,
        vec![
            (
                call_pos,
                LsTransition::Drop {
                    node: sum,
                    gp: true
                }
            ),
            (
                reload_at,
                LsTransition::Load {
                    node: sum,
                    reg: RBX,
                    gp: true
                }
            ),
        ],
        "the register goes away at the call and comes back at the next \
             use. The `Store` event emits NOTHING: this backend is \
             write-through, so the home word was written at the definition.",
    );

    // ── And now what the emission does with it ──────────────────
    let graph: &'static Graph = Box::leak(Box::new(graph));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan));
    let mut lo = lowerer_with_split(graph, schedule, plan, &live, sum, first, transitions);
    assert_eq!(
        lo.resident_gpr(sum),
        Some(RBX),
        "before the call the value is read out of its register",
    );

    let before = lo.buf.pos();
    lo.ls_apply_transitions(call_pos);
    assert_eq!(
        lo.buf.pos(),
        before,
        "giving up a register is state, not an instruction: the home word \
             already holds the value",
    );
    assert_eq!(
        lo.resident_gpr(sum),
        None,
        "across the call the value is read out of its home word",
    );

    lo.ls_apply_transitions(reload_at);
    let code = &lo.buf.as_slice()[before..];
    assert_eq!(
        lo.resident_gpr(sum),
        Some(RBX),
        "after the call it is back in the SAME register, and the uses that \
             follow read it there",
    );
    assert_eq!(
        code.first().copied(),
        Some(0x48),
        "REX.W on a 64-bit load into RBX: {code:02x?}",
    );
    assert_eq!(
        code.get(1).copied(),
        Some(0x8B),
        "`MOV r64, r/m64` -- a reload from the home word: {code:02x?}",
    );
    assert_eq!(lo.ls_split_emitted, [1, 0, 0, 1], "one load, one drop");
}

/// The other half of the safepoint argument: a managed value keeps its home
/// word even where every switch that would drop one is on, and no deopt
/// frame names its register.
///
/// Both refusals are about the same thing -- a split value is in a given
/// register only at SOME positions, and neither a dropped home nor a frame
/// state carries a position to qualify the claim with.
/// Several moves at one position are a PARALLEL copy. `n1`'s source is
/// `n0`'s destination, so emitting them in the order the plan collected
/// them would destroy `n1`'s value.
#[test]
pub(in crate::ir_lower::tests) fn two_moves_at_one_position_are_ordered_so_no_source_is_read_after_it_is_written(
) {
    let taken = vec![
        (
            0usize,
            true,
            vec![(
                4usize,
                LsTransition::Move {
                    node: 0,
                    from: RBX,
                    to: R12,
                    gp: true,
                },
            )],
        ),
        (
            1usize,
            true,
            vec![(
                4usize,
                LsTransition::Move {
                    node: 1,
                    from: R12,
                    to: R13,
                    gp: true,
                },
            )],
        ),
    ];
    let at = ls_sequence_split_plan(&taken, 8).expect("an acyclic copy needs no scratch");
    assert_eq!(
        at[4],
        vec![
            LsTransition::Move {
                node: 1,
                from: R12,
                to: R13,
                gp: true
            },
            LsTransition::Move {
                node: 0,
                from: RBX,
                to: R12,
                gp: true
            },
        ],
        "the move whose destination nobody else still reads goes first",
    );
    assert!(
        at.iter().enumerate().all(|(p, v)| p == 4 || v.is_empty()),
        "and nothing lands anywhere else",
    );
}

/// A cycle needs a scratch register, and between two nodes this backend has
/// none: RAX may be carrying a value to the next node and RCX a deferred
/// one. So the whole method's split promotion is refused rather than a
/// register picked and hoped for.
#[test]
pub(in crate::ir_lower::tests) fn a_copy_cycle_at_one_position_refuses_the_whole_split_plan() {
    let taken = vec![
        (
            0usize,
            true,
            vec![(
                4usize,
                LsTransition::Move {
                    node: 0,
                    from: RBX,
                    to: R12,
                    gp: true,
                },
            )],
        ),
        (
            1usize,
            true,
            vec![(
                4usize,
                LsTransition::Move {
                    node: 1,
                    from: R12,
                    to: RBX,
                    gp: true,
                },
            )],
        ),
    ];
    assert!(
        ls_sequence_split_plan(&taken, 8).is_none(),
        "a swap is a cycle, and `CopyOp::Save` names no scratch because \
             the backend is supposed to supply one -- this backend cannot",
    );
}

/// Two transitions publishing into ONE register at one position is a state
/// in which this file's model of the allocation has come apart from the
/// allocation. Refused, not resolved.
#[test]
pub(in crate::ir_lower::tests) fn two_transitions_into_one_register_at_one_position_refuse_the_plan(
) {
    let taken = vec![
        (
            0usize,
            true,
            vec![(
                4usize,
                LsTransition::Load {
                    node: 0,
                    reg: RBX,
                    gp: true,
                },
            )],
        ),
        (
            1usize,
            true,
            vec![(
                4usize,
                LsTransition::Load {
                    node: 1,
                    reg: RBX,
                    gp: true,
                },
            )],
        ),
    ];
    assert!(ls_sequence_split_plan(&taken, 8).is_none());
}

/// A transition at a TERMINATOR or an EDGE position is refused, and the
/// refusal is the safepoint argument rather than a simplification: both are
/// outside `lower_block`'s per-node loop, so a `Ref` published at either
/// would survive the back-edge poll's own call with nothing to drop it.
#[test]
pub(in crate::ir_lower::tests) fn a_transition_outside_the_node_positions_is_refused() {
    let (graph, schedule, sum, _call) = value_live_across_a_call_graph();
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let node_pos = ls_node_positions(&schedule, &live);
    let def = live.pos_of[sum as usize].expect("scheduled");
    let (_, edge) = live.span[0];
    assert!(!node_pos[edge], "an outgoing edge is not a node position");
    let alloc = crate::regalloc::Allocation {
        segments: vec![Vec::new(); graph.nodes.len()],
        stack_slot: vec![None; graph.nodes.len()],
        events: vec![crate::regalloc::SpillEvent {
            node: sum,
            pos: edge,
            kind: crate::regalloc::SpillKind::Load,
            from: None,
            to: Some(crate::regalloc::PhysReg::gp(RBX)),
        }],
        ..crate::regalloc::Allocation::default()
    };
    assert!(
        ls_split_transitions(
            &graph,
            &alloc,
            sum as usize,
            true,
            def,
            Some(RBX),
            &node_pos
        )
        .is_none(),
        "a reload on an edge is outside the invalidation audit",
    );
}

/// A transition BEFORE the definition is refused. Liveness is widened to
/// whole block spans, so a loop-carried value's range can begin before the
/// node that defines it -- and a load from the home word there reads bytes
/// nothing has written.
#[test]
pub(in crate::ir_lower::tests) fn a_transition_before_the_definition_is_refused() {
    let (graph, schedule, sum, _call) = value_live_across_a_call_graph();
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let node_pos = ls_node_positions(&schedule, &live);
    let def = live.pos_of[sum as usize].expect("scheduled");
    assert!(def > 0, "the fixture defines its value after block entry");
    let alloc = crate::regalloc::Allocation {
        segments: vec![Vec::new(); graph.nodes.len()],
        stack_slot: vec![None; graph.nodes.len()],
        events: vec![crate::regalloc::SpillEvent {
            node: sum,
            pos: def - 1,
            kind: crate::regalloc::SpillKind::Load,
            from: None,
            to: Some(crate::regalloc::PhysReg::gp(RBX)),
        }],
        ..crate::regalloc::Allocation::default()
    };
    assert!(
        ls_split_transitions(
            &graph,
            &alloc,
            sum as usize,
            true,
            def,
            Some(RBX),
            &node_pos
        )
        .is_none(),
        "for an `int` that is a garbage value; for a `Ref` it is a \
             fabricated pointer handed to the collector",
    );
}

/// The prologue's save set and every exit's restore set must be ONE set,
/// and split residency is exactly the thing that makes `gp_reg_of` change
/// between them. The snapshot covers every register a managed value passes
/// through and does not move when the value does.
#[test]
pub(in crate::ir_lower::tests) fn the_save_set_is_a_snapshot_over_every_register_a_split_value_visits(
) {
    let (graph, schedule, sum, call) = value_live_across_a_call_graph();
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let plan = plan_slots(&graph, &schedule, None);
    let call_pos = live.pos_of[call as usize].expect("the call is scheduled");
    let graph: &'static Graph = Box::leak(Box::new(graph));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan));
    // Starts in RBX, and a transition moves it to R12.
    let transitions = vec![(
        call_pos,
        LsTransition::Move {
            node: sum,
            from: RBX,
            to: R12,
            gp: true,
        },
    )];
    let mut lo = lowerer_with_split(graph, schedule, plan, &live, sum, Some(RBX), transitions);
    assert!(
        lo.ls_saved_gp[RBX as usize] && lo.ls_saved_gp[R12 as usize],
        "the frame owes its caller both registers the value passes through",
    );
    let before = lo.ls_saved_gp;
    lo.ls_apply_transitions(call_pos);
    assert_eq!(
        lo.assigned_gpr(sum),
        Some(R12),
        "the value has moved, which is the whole point",
    );
    assert_eq!(
        lo.ls_saved_gp, before,
        "and the save set has NOT: a prologue that saved RBX and an \
             epilogue that no longer saw RBX in `gp_reg_of` would return to \
             its caller with RBX destroyed",
    );
    assert_eq!(
        lo.ls_split_registers(sum, true),
        vec![RBX, R12],
        "and both are named as this value's, in the GP bank only",
    );
}

/// A `Move` reads its planned source register, or it reads memory -- never
/// a third thing. When the source is not readable (a definition arm that
/// never published, or a `Ref` copy `invalidate_ref_residency` dropped) the
/// home word is read instead, which can only REMOVE a register read and so
/// leaves the sequenced order valid.
#[test]
pub(in crate::ir_lower::tests) fn a_move_whose_source_is_not_readable_falls_back_to_the_home_word()
{
    let (graph, schedule, sum, call) = value_live_across_a_call_graph();
    let live = crate::regalloc::build_live_model(&graph, &schedule);
    let plan = plan_slots(&graph, &schedule, None);
    let call_pos = live.pos_of[call as usize].expect("the call is scheduled");
    let graph: &'static Graph = Box::leak(Box::new(graph));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan));
    let transitions = vec![(
        call_pos,
        LsTransition::Move {
            node: sum,
            from: RBX,
            to: R12,
            gp: true,
        },
    )];
    let mut lo = lowerer_with_split(graph, schedule, plan, &live, sum, Some(RBX), transitions);
    // Take the source away, exactly as the reference invalidation does.
    lo.gp_reg_live[sum as usize] = false;
    let before = lo.buf.pos();
    lo.ls_apply_transitions(call_pos);
    let code = &lo.buf.as_slice()[before..];
    assert_eq!(
        code.first().copied(),
        Some(0x4C),
        "REX.W + REX.R for a load into R12, i.e. a MEMORY read and not the \
             `MOV R12, RBX` the plan asked for: {code:02x?}",
    );
    assert_eq!(
        code.get(1).copied(),
        Some(0x8B),
        "`MOV r64, r/m64`: {code:02x?}",
    );
    assert_eq!(
        lo.resident_gpr(sum),
        Some(R12),
        "and the value is published where the plan said it would be",
    );
    assert_eq!(lo.ls_split_emitted, [1, 0, 0, 0], "counted as a load");
}

// ── Code-buffer sizing ───────────────────────────────────────────
//
// `ir_code_buffer_estimate` decides which methods the optimizing tier gets
// to produce a body for at all: too small and the compile is silently
// discarded to single-pass, with nothing but a throughput change to show
// for it. These pin the model against the census that produced it, so a
// future "tighten this up" has to argue with the data rather than with a
// comment.

/// The extremes of the 2026-08-01 census: 1664 IR compiles across
/// `BasicErrorControllerDirectMockMvcTests`,
/// `BasicErrorControllerIntegrationTests` and `bench/CratonBench`, as
/// `(nodes, call_nodes, wanted)`. These four are the largest consumers plus
/// the tightest call-heavy small graph; every one of them OVERFLOWED the
/// pre-2026-08-01 estimate.
pub(in crate::ir_lower::tests) const CENSUS_EXTREMES: &[(usize, usize, usize)] = &[
    (78, 15, 25969),
    (36, 14, 22378),
    (72, 9, 16688),
    (9, 3, 4508),
];

#[test]
pub(in crate::ir_lower::tests) fn code_buffer_estimate_keeps_headroom_over_the_measured_worst_case()
{
    // Covering the census exactly would be a knife edge — the next
    // workload's worst case is not in it. The measured tightest fit is
    // 1.24x; require at least 1.1x on the extremes so a change that
    // technically still "covers" them but removes all margin fails here.
    for &(nodes, calls, wanted) in CENSUS_EXTREMES {
        let cap = ir_code_buffer_estimate(nodes, calls);
        assert!(
            cap * 10 >= wanted * 11,
            "nodes={nodes} calls={calls}: {cap} bytes for a measured {wanted} is \
                 under 1.1x headroom"
        );
    }
}

#[test]
pub(in crate::ir_lower::tests) fn a_call_node_is_budgeted_above_its_measured_worst_marginal_cost() {
    // The term the old estimate got wrong: it budgeted 448 bytes per call
    // node, and the census puts the worst-case marginal cost at 1141.
    // Derived from the model rather than asserted on the constant, so
    // moving cost between the terms is allowed and starving calls is not.
    //
    // Measured well ABOVE the floor. The first draft of this test compared
    // 1 call against 2, where both results are still clamped by
    // `IR_CODE_BUFFER_FLOOR` — the marginal cost read as 0 and the test
    // failed on a model that was perfectly fine. A floor hides exactly the
    // term this is trying to pin.
    let ten = ir_code_buffer_estimate(0, 10);
    let eleven = ir_code_buffer_estimate(0, 11);
    assert!(
        ten > IR_CODE_BUFFER_FLOOR && eleven > IR_CODE_BUFFER_FLOOR,
        "measure the marginal cost above the floor, not through it"
    );
    assert!(
        eleven - ten >= 1141,
        "a call node is budgeted {} bytes; the census measured 1141 worst-case",
        eleven - ten
    );
}

#[test]
pub(in crate::ir_lower::tests) fn the_floor_alone_does_not_substitute_for_the_formula() {
    // `nodes*32 + calls*448 + 1024` with a raised floor was the tempting
    // one-line fix. It does not work: this shape wants 25969 bytes, which
    // no plausible floor covers, so the per-call term has to carry it.
    let (nodes, calls, wanted) = (78usize, 15usize, 25969usize);
    assert!(
        ir_code_buffer_estimate(nodes, calls) >= wanted,
        "the formula, not the floor, has to cover the call-heavy tail"
    );
    let legacy_shape = nodes * 32 + calls * 448 + 1024;
    assert!(
        legacy_shape < wanted,
        "sanity: the old formula really was short here ({legacy_shape} < {wanted})"
    );
}

/// Round 9 wave 4 (irlower4, `regalloc-live-model-misses-scalar-replacement-pins`):
/// `plan_slots` pins, records, and `regalloc` pins the SAME scalar-replacement
/// field values, because all three read `scalar_replacement_field_values`.
#[test]
pub(in crate::ir_lower::tests) fn r9w4_scalar_replacement_pins_come_from_one_walk() {
    let (g, sr, newo) = build_sr_deopt_graph(false, false);
    let walked: Vec<NodeId> = scalar_replacement_field_values(&sr).collect();
    assert_eq!(walked.len(), 1, "the fixture's one field value");
    let schedule = ir_schedule::schedule(&g);
    let plan = plan_slots(&g, &schedule, Some(&sr));
    assert_eq!(plan.sr_field_values, walked);
    let v = walked[0] as usize;
    if plan.node_color[v].is_some() {
        assert_eq!(plan.class[v], Some(SlotClass::Pinned));
    }
    assert!(plan_slots(&g, &schedule, None).sr_field_values.is_empty());

    // `None` and `NO_NODE` are skipped, a repeat is kept (both consumers
    // are idempotent).
    let mut objects = HashMap::new();
    objects.insert(
        newo,
        VirtualObjectInfo {
            loop_phis: None,
            class_id: 7,
            array_element_type: None,
            num_fields: 4,
            field_values: vec![None, Some(NO_NODE), Some(walked[0]), Some(walked[0])],
            new_ctrl: NO_NODE,
            store_ctrls: Vec::new(),
            store_nodes: Vec::new(),
            store_fields: Vec::new(),
            non_null_cond: None,
        },
    );
    let odd = ScalarReplacementMap { objects };
    assert_eq!(
        scalar_replacement_field_values(&odd).collect::<Vec<_>>(),
        vec![walked[0], walked[0]]
    );
}

/// A guarded OSR value can flow through a loop Phi; any nullable arm keeps
/// the store's deopt point.
#[test]
pub(in crate::ir_lower::tests) fn a_store_through_a_non_null_phi_has_no_null_check() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let param = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let fresh = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    let exact = g.add(
        Op::ExactClassIs { class_id: 7 },
        IrType::Int,
        vec![param],
        None,
    );
    g.add(
        Op::Guard { bci: 10 },
        IrType::Void,
        vec![ctrl, exact],
        Some(10),
    );
    let region = g.add(Op::Merge, IrType::Control, vec![ctrl, ctrl], Some(10));
    let proven = g.add(Op::Phi, IrType::Ref, vec![region, fresh, param], Some(10));
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let nullable = g.add(Op::Phi, IrType::Ref, vec![region, fresh, null], Some(10));
    let offset = g.add(Op::Const(0), IrType::Int, vec![], None);
    let value = g.add(Op::Const(1), IrType::Int, vec![], None);
    let safe_store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, proven, offset, value],
        Some(31),
    );
    let nullable_store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, nullable, offset, value],
        Some(32),
    );

    assert!(store_cannot_deopt(&g, &g.nodes[safe_store as usize]));
    assert!(!store_cannot_deopt(&g, &g.nodes[nullable_store as usize]));
}

/// Round 9 wave 4: fresh-allocation stores need no receiver null check;
/// stores through an unguarded parameter still do.
#[test]
pub(in crate::ir_lower::tests) fn r9w4_a_store_into_a_fresh_allocation_emits_no_null_check() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let param = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let newo = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![c0, mem],
        Some(0),
    );
    let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let v7 = g.add(Op::Const(7), IrType::Int, vec![], None);
    let fresh = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![c0, mem, newo, f0, v7],
        Some(4),
    );
    let through_param = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![c0, fresh, param, f0, v7],
        Some(8),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![c0], Some(12));

    assert!(store_cannot_deopt(&g, &g.nodes[fresh as usize]));
    assert!(node_cannot_deopt(&g, &g.nodes[fresh as usize]));
    assert!(!store_cannot_deopt(&g, &g.nodes[through_param as usize]));
    assert!(!node_cannot_deopt(&g, &g.nodes[through_param as usize]));
    assert!(
        !op_cannot_deopt(&g.nodes[fresh as usize].op),
        "the op-only allowlist still counts a store as a call site"
    );

    let schedule = ir_schedule::schedule(&g);
    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let mut lo = lowerer_over(graph, schedule, plan, 4096);
    assert!(
        lo.deopt_sites_by_bci.get(&4).is_none(),
        "the fresh store is not indexed as a deopt site"
    );
    assert!(lo.deopt_sites_by_bci.get(&8).is_some());

    let before = lo.buf.pos();
    lo.emit_store_receiver_null_check(newo, 4);
    assert_eq!(
        lo.buf.pos(),
        before,
        "no TEST, no deopt for a fresh receiver"
    );
    assert!(!lo.transfer_bcis.contains(&4));

    lo.emit_store_receiver_null_check(param, 8);
    assert!(
        lo.buf.pos() > before,
        "a parameter receiver is still checked"
    );
    assert!(lo.transfer_bcis.contains(&8));
}

/// Round 9 wave 9 (irl9): the optimizing tier's inline `instanceof` final
/// miss keeps its own copy of the single-pass final-boot-class list
/// (`x64::op_object::instanceof_miss_is_final_boot_class` is `pub(in crate::ir_lower)`).
/// A class added to one list and not the other would make the two tiers
/// disagree on which misses are answered inline -- harmless for the list
/// that is shorter, a wrong answer for an entry that is not final. So pin
/// the copy against the original's SOURCE.
#[test]
pub(in crate::ir_lower::tests) fn ir_instanceof_final_boot_list_matches_the_single_pass_list() {
    let names = |src: &str, anchor: &str| -> Vec<String> {
        let body = src
            .split(anchor)
            .nth(1)
            .expect("the list function is in its file")
            .split("\n}")
            .next()
            .expect("the function ends")
            .to_string();
        let mut out: Vec<String> = body
            .split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();
        out.sort();
        out
    };
    let sp = names(
        include_str!("../../x64/op_object.rs"),
        "fn instanceof_miss_is_final_boot_class(name: &str) -> bool {",
    );
    let ir = names(
        concat!(
            include_str!("../../ir_lower/mod.rs"),
            "\n",
            include_str!("../../ir_lower/virtual_object_info.rs"),
            "\n",
            include_str!("../../ir_lower/mir_plan_and_init.rs"),
            "\n",
            include_str!("../../ir_lower/vector_preloop.rs"),
            "\n",
            include_str!("../../ir_lower/value_phi_copies.rs"),
            "\n",
            include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
            "\n",
            include_str!("../../ir_lower/int_expr_bounds.rs"),
            "\n",
            include_str!("../../ir_lower/character_to_chars_prefix.rs"),
            "\n",
            include_str!("../../ir_lower/getfield_legacy_read.rs"),
            "\n",
            include_str!("../../ir_lower/post_call_exit_site_for.rs"),
            "\n",
            include_str!("../../ir_lower/entry_fold_node.rs"),
            "\n",
            include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
            "\n",
            include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
            "\n",
            include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
            "\n",
            include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
            "\n",
            include_str!("../../ir_lower/scoped_raw_tail.rs"),
            "\n",
            include_str!("../../ir_lower/frame_needs.rs"),
            "\n",
            include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
            "\n",
            include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
            "\n",
            include_str!("../../ir_lower/promoted_pending_mark.rs"),
            "\n",
            include_str!("../../ir_lower/site_value_live_at.rs"),
            "\n",
            include_str!("../../ir_lower/plan_register_residency_with.rs"),
            "\n",
            include_str!("../../ir_lower/inner_attempt.rs"),
            "\n",
            include_str!("../../ir_lower/g1_aastore_screen.rs"),
            "\n",
            include_str!("../../ir_lower/scoped_session_confined_rows.rs")
        ),
        "fn ir_instanceof_miss_is_final_boot_class(name: &str) -> bool {",
    );
    assert!(!sp.is_empty(), "the single-pass list scan found nothing");
    assert_eq!(sp, ir, "the two final-boot-class lists drifted apart");
    for n in &ir {
        assert!(super::ir_instanceof_miss_is_final_boot_class(n), "{n}");
    }
    assert!(!super::ir_instanceof_miss_is_final_boot_class(
        "java/lang/Object"
    ));
    assert!(!super::ir_instanceof_miss_is_final_boot_class(
        "java/lang/Number"
    ));
}

/// Round 9 wave 9 (irl9): the optimizing tier's inline `ArrayList.get` /
/// `size()` prefix (`try_emit_ir_arraylist_access_prefix`), EXECUTED over
/// fake heap shapes. The dispatch stub answers a marker no fast path can
/// produce, so every assertion says which edge ran. The same bodies
/// lowered with no layout must dispatch every time.
#[test]
pub(in crate::ir_lower::tests) fn r9w9_ir_arraylist_get_and_size_answer_inline_or_dispatch() {
    crate::x64::set_moving_young_override(Some(false));
    if crate::arraylist_intrinsics_disabled() || crate::x64::zgc_read_barrier_blocks_inline_fields()
    {
        return;
    }
    const MARKER: i64 = 0x5EED_0000;
    extern "C" fn dispatch(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        MARKER
    }
    extern "C" fn guard_ok(_vm: i64, _ctor_receiver: i64) -> i64 {
        0
    }
    const AL_ID: u32 = 0x0004_4401;
    let hs = cratonvm_types::HEADER_SIZE;
    let ss = cratonvm_types::SLOT_SIZE;
    let cell = |i: usize| (hs + i * ss) as i32; // Cast: small test offsets
    let tag_off = cratonvm_types::FIELD_CELL_TAG_OFFSET as i32; // Cast: small header constant
    let layout = crate::ArrayListFieldLayout {
        data_field_index: 0,
        size_field_index: 1,
        size_compact_offset: hs as i32, // Cast: small header constant
        size_legacy_offset: cell(1) + cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET as i32, // Cast: small header constant
        size_legacy_tag_offset: cell(1) + tag_off,
        data_compact_offset: hs as i32 + 8, // Cast: small header constant
        data_legacy_offset: cell(0) + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET as i32, // Cast: small header constant
        data_legacy_tag_offset: cell(0) + tag_off,
        min_num_slots: 2,
        class_id: AL_ID,
        inline_get_size: true,
        mod_count_compact_offset: None,
        object_class_id: 0,
    };
    assert!(hs + 2 * ss <= 128, "the fake receiver is 128 bytes");

    fn put(buf: &mut [u64], off: usize, bytes: &[u8]) {
        assert!(off + bytes.len() <= buf.len() * 8);
        let base = buf.as_mut_ptr() as *mut u8; // Cast: byte view of the test buffer
                                                // SAFETY: in range by the assert above.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(off), bytes.len()) };
    }
    let kt = cratonvm_types::KIND_TAGS_BYTE_OFFSET;
    let gf = cratonvm_types::GC_FLAGS_BYTE_OFFSET;
    // elementData: a reference array of length 4 holding 3 elements.
    let ref_array_tags = (cratonvm_types::ObjectKind::Array as u8)
        | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);
    let mut elems = vec![0u64; 16];
    put(&mut elems, kt, &[ref_array_tags]);
    put(
        &mut elems,
        cratonvm_types::ARRAY_LENGTH_OFFSET,
        &4u32.to_le_bytes(),
    );
    let elem_val = |i: usize| 0x1111_0000u64 + i as u64; // Cast: small test index
    for i in 0..3 {
        put(
            &mut elems,
            cratonvm_types::ARRAY_DATA_OFFSET + 8 * i,
            &elem_val(i).to_le_bytes(),
        );
    }
    let elems_ptr = elems.as_ptr() as u64; // Cast: fake reference
                                           // A legacy receiver: two tagged 16-byte cells.
    let legacy_recv = |class_id: u32, size: i32, data: u64| -> Vec<u64> {
        let mut r = vec![0u64; 16];
        put(&mut r, 0, &class_id.to_le_bytes());
        put(
            &mut r,
            cratonvm_types::NUM_SLOTS_OFFSET,
            &2u32.to_le_bytes(),
        );
        put(&mut r, (cell(1) + tag_off) as usize, &0u32.to_le_bytes());
        put(
            &mut r,
            layout.size_legacy_offset as usize,
            &size.to_le_bytes(),
        );
        put(
            &mut r,
            (cell(0) + tag_off) as usize,
            &cratonvm_types::FIELD_CELL_TAG_OBJECT.to_le_bytes(),
        );
        put(
            &mut r,
            layout.data_legacy_offset as usize,
            &data.to_le_bytes(),
        );
        r
    };
    // A compact receiver: a bare int and a bare pointer.
    let compact_recv = |size: i32, data: u64| -> Vec<u64> {
        let mut r = vec![0u64; 16];
        put(&mut r, 0, &AL_ID.to_le_bytes());
        put(&mut r, gf, &[cratonvm_types::GC_FLAG_COMPACT]);
        put(
            &mut r,
            layout.size_compact_offset as usize,
            &size.to_le_bytes(),
        );
        put(
            &mut r,
            layout.data_compact_offset as usize,
            &data.to_le_bytes(),
        );
        r
    };

    let lower = |code: &[u8],
                 code_len: usize,
                 params: &[IrType],
                 info: &'static JitInvokeInfo,
                 num_args: usize,
                 ret: u8,
                 al: Option<crate::ArrayListFieldLayout>| {
        let n = params.len();
        let mut builder = IrBuilder::new(n, n);
        builder.set_param_types(params);
        let mut invoke_info = HashMap::new();
        let call_pc = code_len - 4; // `invokevirtual #2` sits just before the return
        invoke_info.insert(
            call_pc,
            (info as *const JitInvokeInfo as usize, num_args, ret),
        );
        builder.set_invoke_info(invoke_info);
        let graph = builder.build(code, code_len).expect("IR build of the call");
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.invoke_dispatch = dispatch as *const () as usize;
        helpers.self_call_stack_guard = guard_ok as *const () as usize;
        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        lower_inner_with_array_list(
            &crate::DirectHelperTable::EMPTY,
            &graph,
            &schedule,
            n,
            n,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
            &crate::ir::IrInlineFrameSites::default(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            al,
        )
        .expect("the call site lowers")
    };
    let run = |cm: &CompiledMethod, args: &[i64]| -> i64 {
        // SAFETY: a body whose only reachable helper is `dispatch`, over
        // live fake objects the prefix only reads.
        unsafe { cm.try_call_with_context(0, args) }.expect("call")
    };

    // static Object f(ArrayList l, int i) { return l.get(i); }
    let get_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/util/ArrayList",
        method_name: "get",
        descriptor: "(I)Ljava/lang/Object;",
        num_jit_args: 2,
        return_type: b'L',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let get_code = [0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xb0, 0x00, 0x00];
    let params = [IrType::Ref, IrType::Int];
    let get = lower(&get_code, 6, &params, get_info, 2, b'L', Some(layout));
    let get_plain = lower(&get_code, 6, &params, get_info, 2, b'L', None);

    let legacy = legacy_recv(AL_ID, 3, elems_ptr);
    let compact = compact_recv(3, elems_ptr);
    let lp = legacy.as_ptr() as i64; // Cast: object address as JIT argument
    let cp = compact.as_ptr() as i64; // Cast: object address as JIT argument
    for i in 0..3i64 {
        let want = elem_val(i as usize) as i64; // Cast: small test value
        assert_eq!(run(&get, &[lp, i]), want, "legacy get({i}) inline");
        assert_eq!(run(&get, &[cp, i]), want, "compact get({i}) inline");
        assert_eq!(run(&get_plain, &[lp, i]), MARKER, "no layout: dispatch");
    }
    assert_eq!(run(&get, &[lp, 3]), MARKER, "index == size dispatches");
    assert_eq!(run(&get, &[lp, -1]), MARKER, "a negative index dispatches");
    assert_eq!(run(&get, &[lp, 4]), MARKER, "index == length dispatches");
    let other = legacy_recv(AL_ID + 1, 3, elems_ptr);
    assert_eq!(
        run(&get, &[other.as_ptr() as i64, 0]), // Cast: object address as JIT argument
        MARKER,
        "another class id dispatches"
    );
    let null_data = legacy_recv(AL_ID, 3, 0);
    assert_eq!(
        run(&get, &[null_data.as_ptr() as i64, 0]), // Cast: object address as JIT argument
        MARKER,
        "a null elementData dispatches"
    );
    let negative = compact_recv(-1, elems_ptr);
    assert_eq!(
        run(&get, &[negative.as_ptr() as i64, 0]), // Cast: object address as JIT argument
        MARKER,
        "a negative size dispatches"
    );
    // A stale-view marker: a non-null value in the last spare slot.
    let mut marked = elems.clone();
    put(
        &mut marked,
        cratonvm_types::ARRAY_DATA_OFFSET + 8 * 3,
        &0xDEADu64.to_le_bytes(),
    );
    let marked_recv = compact_recv(3, marked.as_ptr() as u64); // Cast: fake reference
    assert_eq!(
        run(&get, &[marked_recv.as_ptr() as i64, 0]), // Cast: object address as JIT argument
        MARKER,
        "a view marker in the spare slot dispatches"
    );
    // An `ArrayList[]` receiver: its header carries the component's id.
    let mut arr_recv = compact_recv(3, elems_ptr);
    put(&mut arr_recv, kt, &[ref_array_tags]);
    assert_eq!(
        run(&get, &[arr_recv.as_ptr() as i64, 0]), // Cast: object address as JIT argument
        MARKER,
        "an array receiver dispatches"
    );

    // static int g(ArrayList l) { return l.size(); }
    let size_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/util/ArrayList",
        method_name: "size",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let size_code = [0x2a, 0xb6, 0x00, 0x02, 0xac, 0x00, 0x00];
    let size = lower(
        &size_code,
        5,
        &[IrType::Ref],
        size_info,
        1,
        b'I',
        Some(layout),
    );
    assert_eq!(run(&size, &[lp]), 3, "legacy size() inline");
    assert_eq!(run(&size, &[cp]), 3, "compact size() inline");
    assert_eq!(
        run(&size, &[other.as_ptr() as i64]), // Cast: object address as JIT argument
        MARKER,
        "size() on another class dispatches"
    );
    drop((elems, marked));
}

/// JIT round 21, lane strings (ZCP-4): the `StringLatin1.compareToCI`
/// prefix (`try_emit_ir_latin1_compare_to_ci_prefix`), EXECUTED over fake
/// `byte[]`s against a Rust model of the JDK body's two-step fold, every
/// Latin-1 byte against a set of partners (both folds), across the 8-byte
/// step and the byte tail; a null or non-`byte[]` operand declines.
#[test]
pub(in crate::ir_lower::tests) fn jr21_ir_latin1_compare_to_ci_prefix_answers_inline_or_dispatches()
{
    crate::x64::set_moving_young_override(Some(false));
    if crate::x64::narrow_oops_block_inline_fields()
        || crate::ir_string_search_prefix_kind("java/lang/StringLatin1", "compareToCI", "([B[B)I")
            .is_none()
    {
        return;
    }
    const MARKER: i32 = 0x5EED_0000;
    extern "C" fn dispatch(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::from(MARKER)
    }
    extern "C" fn guard_ok(_vm: i64, _ctor_receiver: i64) -> i64 {
        0
    }
    const STR_ID: u32 = 0x0004_5501;
    let layout = crate::tests::synthetic_string_layout(STR_ID);
    let kt = cratonvm_types::KIND_TAGS_BYTE_OFFSET;
    let b_tag = cratonvm_types::primitive_array_kind_tags_byte("[B").expect("[B tag");
    let c_tag = cratonvm_types::primitive_array_kind_tags_byte("[C").expect("[C tag");
    let bytes = |s: &[u8]| -> Vec<u8> {
        let elems: Vec<i64> = s.iter().map(|&b| i64::from(b)).collect();
        let mut a = crate::tests::synthetic_array(1, &elems);
        a[kt] = b_tag;
        a
    };
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/StringLatin1",
        method_name: "compareToCI",
        descriptor: "([B[B)I",
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    // static int C(byte[] a, byte[] b) { return StringLatin1.compareToCI(a, b); }
    let code = [0x2a, 0x2b, 0xb8, 0x00, 0x02, 0xac];
    let params = [IrType::Ref, IrType::Ref];
    let n = params.len();
    let mut builder = IrBuilder::new(n, n);
    builder.set_param_types(&params);
    let mut invoke_info = HashMap::new();
    invoke_info.insert(2usize, (info as *const JitInvokeInfo as usize, n, b'I'));
    builder.set_invoke_info(invoke_info);
    let graph = builder
        .build(&code, code.len())
        .expect("IR build of the call");
    let schedule = ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.invoke_dispatch = dispatch as *const () as usize;
    helpers.self_call_stack_guard = guard_ok as *const () as usize;
    let empty_hints: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let cm = {
        let _scope = super::CompileStringLayoutScope::enter(Some(layout));
        lower_inner_with_array_list(
            &crate::DirectHelperTable::EMPTY,
            &graph,
            &schedule,
            n,
            n,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
            &crate::ir::IrInlineFrameSites::default(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            None,
        )
        .expect("the call site lowers")
    };
    let run = |a: i64, b: i64| -> i32 {
        // SAFETY: a body whose only reachable helper is `dispatch`, over
        // live fake arrays the prefix only reads.
        let r = unsafe { cm.try_call_with_context(0, &[a, b]) }.expect("call");
        r as i32 // Cast: the int result's low half
    };
    let ptr = |a: &[u8]| a.as_ptr() as i64; // Cast: object address as JIT argument

    // The JDK body, with `CharacterDataLatin1.toUpperCase` /
    // `Character.toLowerCase` written out for the Latin-1 range.
    let upper = |b: u8| -> u32 {
        match b {
            b'a'..=b'z' => u32::from(b) - 0x20,
            0xE0..=0xFE if b != 0xF7 => u32::from(b) - 0x20,
            0xB5 => 0x39C,
            0xFF => 0x178,
            _ => u32::from(b),
        }
    };
    let lower = |c: u32| -> u32 {
        match c {
            0x41..=0x5A => c + 0x20,
            0xC0..=0xDE if c != 0xD7 => c + 0x20,
            0x39C => 0x3BC,
            0x178 => 0xFF,
            _ => c,
        }
    };
    let model = |x: &[u8], y: &[u8]| -> i32 {
        for k in 0..x.len().min(y.len()) {
            if x[k] != y[k] {
                let (c1, c2) = (upper(x[k]), upper(y[k]));
                if c1 != c2 {
                    let (l1, l2) = (lower(c1), lower(c2));
                    if l1 != l2 {
                        return i32::try_from(l1).expect("small")
                            - i32::try_from(l2).expect("small");
                    }
                }
            }
        }
        i32::try_from(x.len()).expect("small") - i32::try_from(y.len()).expect("small")
    };
    // Every byte at index 0 (byte path) and 9 (after one 8-byte step)
    // against partners that exercise each fold class.
    let partners: [u8; 14] = [
        b'a', b'A', b'z', b'[', 0xB5, 0xC0, 0xE0, 0xD7, 0xF7, 0xDF, 0xFF, 0x78, 0x9F, 0xDE,
    ];
    for b in 0..=255u8 {
        for &p in &partners {
            for at in [0usize, 9] {
                let mut x = b"Content-Type-x".to_vec();
                let mut y = b"CONTENT-type-y".to_vec();
                x[at] = b;
                y[at] = p;
                let (vx, vy) = (bytes(&x), bytes(&y));
                assert_eq!(
                    run(ptr(&vx), ptr(&vy)),
                    model(&x, &y),
                    "compareToCI({b:#x} vs {p:#x} at {at})"
                );
            }
        }
    }
    for (x, y) in [
        (&b""[..], &b""[..]),
        (&b"abc"[..], &b"ABCD"[..]),
        (&b"ABCDEFGHIJKLMNOPQR"[..], &b"abcdefghijklmnopq"[..]),
        (&b"\xB5"[..], &b"\xFF"[..]),
        (&b"\xFF"[..], &b"\xDF"[..]),
    ] {
        let (vx, vy) = (bytes(x), bytes(y));
        assert_eq!(
            run(ptr(&vx), ptr(&vy)),
            model(x, y),
            "compareToCI({x:?}, {y:?})"
        );
    }
    let v = bytes(b"abc");
    assert_eq!(run(0, ptr(&v)), MARKER, "a null value declines (its NPE)");
    assert_eq!(run(ptr(&v), 0), MARKER, "a null other declines");
    let mut not_bytes = bytes(b"abc");
    not_bytes[kt] = c_tag;
    assert_eq!(
        run(ptr(&not_bytes), ptr(&v)),
        MARKER,
        "a non-byte[] declines"
    );
}

/// JIT r21 wave 13 (lane jit-tests): a lowered body whose calls are each
/// held to ONE value of the process-wide layout-replacement epoch.
///
/// The String prefixes open with `emit_layout_epoch_guard`, which bakes
/// `cratonvm_types::layout_replace_epoch()` and declines to the call
/// once it has moved. Any test running concurrently in this binary that
/// re-registers a class layout moves it (several in this crate do so on
/// purpose), and a declined site answers the dispatch stub's marker --
/// which an "answers inline" assertion then reads as a wrong answer. That
/// is why the r21 strings tests failed only in parallel runs. A call here
/// is repeated, against a body recompiled at the current epoch, until one
/// runs start to finish with the epoch unchanged since that body's
/// compile (the idiom of the inline-store tests' "the layout epoch kept
/// moving" loops). No answer is weakened: the call whose epoch held is
/// reported exactly as it ran.
pub(in crate::ir_lower::tests) struct EpochPinnedBody<'a> {
    compile: Box<dyn Fn() -> CompiledMethod + 'a>,
    /// The body and the epoch it baked.
    body: std::cell::RefCell<(CompiledMethod, u32)>,
}

impl<'a> EpochPinnedBody<'a> {
    const TRIES: usize = 64;

    fn new(compile: impl Fn() -> CompiledMethod + 'a) -> Self {
        let compile: Box<dyn Fn() -> CompiledMethod + 'a> = Box::new(compile);
        let body = std::cell::RefCell::new(Self::compile_at_one_epoch(&*compile));
        Self { compile, body }
    }

    /// A body lowered while the epoch stood still, so the value its
    /// guards baked is the one returned with it.
    fn compile_at_one_epoch(compile: &dyn Fn() -> CompiledMethod) -> (CompiledMethod, u32) {
        for _ in 0..Self::TRIES {
            let epoch = cratonvm_types::layout_replace_epoch();
            let cm = compile();
            if cratonvm_types::layout_replace_epoch() == epoch {
                return (cm, epoch);
            }
        }
        panic!("the layout epoch kept moving under the compile");
    }

    /// # Safety
    ///
    /// The caller's obligations for `CompiledMethod::try_call_with_context`
    /// with a null context, for every recompilation of the body.
    unsafe fn call(&self, args: &[i64]) -> i64 {
        let mut body = self.body.borrow_mut();
        for _ in 0..Self::TRIES {
            if cratonvm_types::layout_replace_epoch() != body.1 {
                *body = Self::compile_at_one_epoch(&*self.compile);
            }
            // SAFETY: this function's own contract.
            let r = unsafe { body.0.try_call_with_context(0, args) }.expect("call");
            if cratonvm_types::layout_replace_epoch() == body.1 {
                return r;
            }
        }
        panic!("the layout epoch kept moving under the test");
    }
}

/// JIT r23 wave 2b, lane L5 (INTRINSICS22-5): the `Arrays.equals` prefix,
/// EXECUTED for every integral overload over fake arrays of every length
/// around the 8- and 16-byte steps, a difference at the first, a middle
/// and the last element, unequal lengths, nulls and identity -- against
/// Rust's own slice equality -- and a wrong element type reaching the
/// dispatch stub.
#[test]
pub(in crate::ir_lower::tests) fn r23_l5_ir_arrays_equals_prefix_answers_inline_or_dispatches() {
    crate::x64::set_moving_young_override(Some(false));
    let named = cratonvm_types::flags::with_thread_overrides(&[], || {
        crate::ir_string_search_prefix_kind("java/util/Arrays", "equals", "([B[B)Z")
    });
    if crate::x64::narrow_oops_block_inline_fields() || named.is_none() {
        return;
    }
    const MARKER: i32 = 0x5EED_0000;
    extern "C" fn dispatch(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::from(MARKER)
    }
    extern "C" fn guard_ok(_vm: i64, _ctor_receiver: i64) -> i64 {
        0
    }
    let layout = crate::tests::synthetic_string_layout(0x0004_5531);
    let tagged = |desc: &str, width: usize, elems: &[i64]| -> Vec<u8> {
        let mut v = crate::tests::synthetic_array(width, elems);
        v[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
            cratonvm_types::primitive_array_kind_tags_byte(desc).expect("primitive");
        v
    };
    // static boolean E(X[] a, X[] b) { return Arrays.equals(a, b); }
    let code = [0x2a, 0x2b, 0xb8, 0x00, 0x02, 0xac];
    let lower = |info: &'static JitInvokeInfo| {
        let params = [IrType::Ref, IrType::Ref];
        let mut builder = IrBuilder::new(2, 2);
        builder.set_param_types(&params);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(
            2usize,
            (info as *const JitInvokeInfo as usize, 2usize, b'Z'),
        );
        builder.set_invoke_info(invoke_info);
        let graph = builder
            .build(&code, code.len())
            .expect("IR build of the call");
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.invoke_dispatch = dispatch as *const () as usize;
        helpers.self_call_stack_guard = guard_ok as *const () as usize;
        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let _scope = super::CompileStringLayoutScope::enter(Some(layout));
        lower_inner_with_array_list(
            &crate::DirectHelperTable::EMPTY,
            &graph,
            &schedule,
            2,
            2,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
            &crate::ir::IrInlineFrameSites::default(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            None,
        )
        .expect("the call site lowers")
    };
    for (desc, elem, width, other) in [
        ("([B[B)Z", "[B", 1usize, "[C"),
        ("([Z[Z)Z", "[Z", 1, "[B"),
        ("([C[C)Z", "[C", 2, "[S"),
        ("([S[S)Z", "[S", 2, "[C"),
        ("([I[I)Z", "[I", 4, "[F"),
        ("([J[J)Z", "[J", 8, "[D"),
    ] {
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "java/util/Arrays",
            method_name: "equals",
            descriptor: desc,
            num_jit_args: 2,
            return_type: b'Z',
            invoke_kind: 3,
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        let cm = EpochPinnedBody::new(|| lower(info));
        let run = |a: i64, b: i64| -> i32 {
            // SAFETY: a body whose only reachable helper is `dispatch`,
            // over live fake arrays the prefix only reads.
            unsafe { cm.call(&[a, b]) as i32 } // Cast: the int result's low half
        };
        let ptr = |v: &Vec<u8>| v.as_ptr() as i64; // Cast: object address as JIT argument
        let elems = |n: usize| -> Vec<i64> {
            (0..n)
                .map(|k| (k as i64).wrapping_mul(0x0101_0101_0101_0101) ^ 0x5A)
                .collect()
        };
        for n in [0usize, 1, 2, 3, 4, 7, 8, 9, 15, 16, 17, 31, 32, 33, 100] {
            let xs = elems(n);
            let a = tagged(elem, width, &xs);
            let b = tagged(elem, width, &xs);
            assert_eq!(run(ptr(&a), ptr(&b)), 1, "{desc} n={n} equal");
            assert_eq!(run(ptr(&a), ptr(&a)), 1, "{desc} n={n} identity");
            for k in [0, n / 2, n.saturating_sub(1)] {
                if n == 0 {
                    break;
                }
                let mut ys = xs.clone();
                // A difference in the element's top byte, so every width
                // sees it.
                ys[k] ^= 1i64 << (8 * width - 1);
                let c = tagged(elem, width, &ys);
                let want = i32::from(xs == ys);
                assert_eq!(run(ptr(&a), ptr(&c)), want, "{desc} n={n} differ at {k}");
            }
            let longer = tagged(elem, width, &elems(n + 1));
            assert_eq!(run(ptr(&a), ptr(&longer)), 0, "{desc} n={n} lengths");
            assert_eq!(run(ptr(&a), 0), 0, "{desc} null second");
            assert_eq!(run(0, ptr(&a)), 0, "{desc} null first");
            let wrong = tagged(other, width, &xs);
            assert_eq!(
                run(ptr(&a), ptr(&wrong)),
                MARKER,
                "{desc} wrong element type"
            );
        }
        assert_eq!(run(0, 0), 1, "{desc} two nulls");
    }
}

pub(in crate::ir_lower::tests) fn ref_arraycopy_prefix_body() {
    if crate::x64::narrow_oops_block_inline_fields() || cratonvm_types::zgc_read_barrier_armed() {
        return;
    }
    extern "C" fn dispatch(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        0
    }
    extern "C" fn guard_ok(_vm: i64, _ctor_receiver: i64) -> i64 {
        0
    }
    // The plan's gate bytes, flipped between runs.
    let pre: *mut u8 = Box::leak(Box::new(0u8));
    let post: *mut u8 = Box::leak(Box::new(0u8));
    const OLD: u8 = 0x01;
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/System",
        method_name: "arraycopy",
        descriptor: "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        num_jit_args: 5,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    // static void C(Object s, int sp, Object d, int dp, int n) {
    //     System.arraycopy(s, sp, d, dp, n); }
    let code = [0x2a, 0x1b, 0x2c, 0x1d, 0x15, 0x04, 0xb8, 0x00, 0x02, 0xb1];
    let params = [
        IrType::Ref,
        IrType::Int,
        IrType::Ref,
        IrType::Int,
        IrType::Int,
    ];
    let mut builder = IrBuilder::new(5, 5);
    builder.set_param_types(&params);
    let mut invoke_info = HashMap::new();
    invoke_info.insert(
        6usize,
        (info as *const JitInvokeInfo as usize, 5usize, b'V'),
    );
    builder.set_invoke_info(invoke_info);
    let graph = builder
        .build(&code, code.len())
        .expect("IR build of the call");
    let schedule = ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.invoke_dispatch = dispatch as *const () as usize;
    helpers.self_call_stack_guard = guard_ok as *const () as usize;
    helpers.ref_store_pre_gate = pre as usize;
    helpers.ref_store_post_gate = post as usize;
    helpers.ref_store_post_skip_mask = usize::from(OLD);
    let empty_hints: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let cm = lower_inner_with_array_list(
        &crate::DirectHelperTable::EMPTY,
        &graph,
        &schedule,
        5,
        5,
        &helpers,
        &empty_hints,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
        &crate::ir::IrInlineFrameSites::default(),
        &HashMap::new(),
        &std::collections::HashSet::new(),
        None,
    )
    .expect("the call site lowers");
    let ref_tag = cratonvm_types::ObjectKind::Array as u8; // Cast: a 2-bit kind
    let int_tag = cratonvm_types::primitive_array_kind_tags_byte("[J").expect("[J");
    let array = |component: u32, tag: u8, flags: u8, elems: &[i64]| -> Vec<u8> {
        let mut v = crate::tests::synthetic_array(8, elems);
        v[0..4].copy_from_slice(&component.to_le_bytes());
        v[cratonvm_types::KIND_TAGS_BYTE_OFFSET] = tag;
        v[cratonvm_types::GC_FLAGS_BYTE_OFFSET] = flags;
        v
    };
    let words = |v: &Vec<u8>| -> Vec<i64> {
        v[cratonvm_types::ARRAY_DATA_OFFSET..]
            .chunks_exact(8)
            .map(|c| i64::from_le_bytes(c.try_into().unwrap_or([0; 8])))
            .collect()
    };
    let ptr = |v: &Vec<u8>| v.as_ptr() as i64; // Cast: object address as JIT argument
    let run = |s: i64, sp: i64, d: i64, dp: i64, n: i64| {
        // SAFETY: a body whose only reachable helpers are the two stubs,
        // over live fake arrays the prefix reads and writes in range.
        unsafe { cm.try_call_with_context(0, &[s, sp, d, dp, n]) }.expect("call");
    };
    let src_words: Vec<i64> = (1..=10).map(|k| 0x1000 + k).collect();
    let dst_words: Vec<i64> = (1..=10).map(|k| 0x2000 + k).collect();
    // Long ranges: every count of the unrolled loop's tail (0..=3) after
    // zero, one and several four-word steps, both directions.
    let long_src: Vec<i64> = (1..=40).map(|k| 0x3000 + k).collect();
    for n in [1usize, 3, 4, 5, 7, 8, 9, 12, 15, 16, 17, 31, 40] {
        let long_dst: Vec<i64> = long_src.iter().map(|w| w + 0x100).collect();
        let s = array(7, ref_tag, 0, &long_src);
        let d = array(7, ref_tag, 0, &long_dst);
        run(ptr(&s), 0, ptr(&d), 0, n as i64); // Cast: a small test count
        let mut want = long_dst.clone();
        want[..n].copy_from_slice(&long_src[..n]);
        assert_eq!(words(&d), want, "long copy n={n}");
        for (sp, dp) in [(0usize, 40 - n), (40 - n, 0)] {
            let a = array(7, ref_tag, 0, &long_src);
            run(ptr(&a), sp as i64, ptr(&a), dp as i64, n as i64); // Cast: small test ints
            let mut want = long_src.clone();
            want.copy_within(sp..sp + n, dp);
            assert_eq!(words(&a), want, "long overlap sp={sp} dp={dp} n={n}");
        }
    }
    // Two arrays: the model is a plain range copy.
    for (sp, dp, n) in [(0, 0, 10), (2, 5, 5), (9, 0, 1), (0, 0, 0), (3, 3, 7)] {
        for (sc, dc) in [(7u32, 7u32), (7, 0)] {
            let s = array(sc, ref_tag, 0, &src_words);
            let d = array(dc, ref_tag, 0, &dst_words);
            run(ptr(&s), sp, ptr(&d), dp, n);
            let mut want = dst_words.clone();
            let (sp, dp, n) = (sp as usize, dp as usize, n as usize); // Cast: small test ints
            want[dp..dp + n].copy_from_slice(&src_words[sp..sp + n]);
            assert_eq!(
                words(&d),
                want,
                "copy sp={sp} dp={dp} n={n} components {sc}/{dc}"
            );
        }
    }
    // One array, overlapping both ways: memmove.
    for (sp, dp, n) in [(0, 1, 9), (1, 0, 9), (2, 5, 5), (5, 2, 5), (4, 4, 6)] {
        let a = array(7, ref_tag, 0, &src_words);
        run(ptr(&a), sp, ptr(&a), dp, n);
        let mut want = src_words.clone();
        let (sp, dp, n) = (sp as usize, dp as usize, n as usize); // Cast: small test ints
        want.copy_within(sp..sp + n, dp);
        assert_eq!(words(&a), want, "overlap sp={sp} dp={dp} n={n}");
    }
    // Every decline leaves the destination as it was.
    let declines: [(&str, u32, u8, u8, u8, u8, (i64, i64, i64)); 10] = [
        ("component mismatch", 8, ref_tag, 0, 0, 0, (0, 0, 3)),
        ("primitive destination", 7, int_tag, 0, 0, 0, (0, 0, 3)),
        ("marking window", 7, ref_tag, 0, 1, 0, (0, 0, 3)),
        (
            "old destination, old objects",
            7,
            ref_tag,
            OLD,
            0,
            1,
            (0, 0, 3),
        ),
        ("src range", 7, ref_tag, 0, 0, 0, (8, 0, 3)),
        ("dst range", 7, ref_tag, 0, 0, 0, (0, 8, 3)),
        ("negative srcPos", 7, ref_tag, 0, 0, 0, (-1, 0, 3)),
        ("negative dstPos", 7, ref_tag, 0, 0, 0, (0, -1, 3)),
        ("negative len", 7, ref_tag, 0, 0, 0, (0, 0, -1)),
        ("len past both", 7, ref_tag, 0, 0, 0, (0, 0, 11)),
    ];
    // The mismatch path may inspect each element's header before it
    // declines. Give it live, non-matching class-7 objects, not integers
    // masquerading as pointers.
    let mismatch_objects = vec![[7u64, 0]; src_words.len()];
    let mismatch_words: Vec<i64> = mismatch_objects
        .iter()
        .map(|object| object.as_ptr() as i64)
        .collect();
    for (what, dc, dtag, dflags, pre_b, post_b, (sp, dp, n)) in declines {
        let elems = if what == "component mismatch" {
            &mismatch_words
        } else {
            &src_words
        };
        let s = array(7, ref_tag, 0, elems);
        let d = array(dc, dtag, dflags, &dst_words);
        // SAFETY: the two leaked gate bytes; no compiled code runs concurrently.
        unsafe {
            std::ptr::write_volatile(pre, pre_b);
            std::ptr::write_volatile(post, post_b);
        }
        run(ptr(&s), sp, ptr(&d), dp, n);
        assert_eq!(words(&d), dst_words, "{what} declines");
    }
    // An old destination with no old objects copies.
    // SAFETY: as above.
    unsafe {
        std::ptr::write_volatile(pre, 0);
        std::ptr::write_volatile(post, 0);
    }
    let s = array(7, ref_tag, 0, &src_words);
    let d = array(7, ref_tag, OLD, &dst_words);
    run(ptr(&s), 0, ptr(&d), 0, 10);
    assert_eq!(words(&d), src_words, "old destination, no old objects");
    // Nulls reach the call.
    let d = array(7, ref_tag, 0, &dst_words);
    run(0, 0, ptr(&d), 0, 3);
    assert_eq!(words(&d), dst_words, "null source declines");
    let s = array(7, ref_tag, 0, &src_words);
    run(ptr(&s), 0, 0, 0, 3);
    assert_eq!(words(&s), src_words, "null destination declines");
}

/// JIT r23 wave 1, lane L5 (STRINGS21-4): the `String.codePointBefore(int)`
/// and `codePointCount(int, int)` prefixes, EXECUTED over fake Strings of
/// both coders and both layouts. Every answer is checked against a literal
/// transcription of the JDK 25 bodies over the same units (whose pair test
/// is Rust's own `char::decode_utf16`); every argument the JDK body throws
/// on reaches the dispatch stub.
#[test]
pub(in crate::ir_lower::tests) fn r23_l5_ir_code_point_before_and_count_prefixes_answer_inline_or_dispatch(
) {
    crate::x64::set_moving_young_override(Some(false));
    let named = cratonvm_types::flags::with_thread_overrides(&[], || {
        (
            crate::ir_string_search_prefix_kind("java/lang/String", "codePointBefore", "(I)I"),
            crate::ir_string_search_prefix_kind("java/lang/String", "codePointCount", "(II)I"),
        )
    });
    if crate::x64::narrow_oops_block_inline_fields() || named.0.is_none() || named.1.is_none() {
        return;
    }
    const MARKER: i32 = 0x5EED_0000;
    extern "C" fn dispatch(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::from(MARKER)
    }
    extern "C" fn guard_ok(_vm: i64, _ctor_receiver: i64) -> i64 {
        0
    }
    const STR_ID: u32 = 0x0004_5523;
    let layout = crate::tests::synthetic_string_layout(STR_ID);
    fn put(buf: &mut [u64], off: usize, bytes: &[u8]) {
        assert!(off + bytes.len() <= buf.len() * 8);
        let base = buf.as_mut_ptr() as *mut u8; // Cast: byte view of the test buffer
                                                // SAFETY: in range by the assert above.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(off), bytes.len()) };
    }
    let gf = cratonvm_types::GC_FLAGS_BYTE_OFFSET;
    let string = |value: &[u8], coder: u8, compact: bool| -> Vec<u64> {
        let mut r = vec![0u64; 16];
        put(&mut r, 0, &STR_ID.to_le_bytes());
        let ptr = value.as_ptr() as u64; // Cast: fake reference
        if compact {
            put(&mut r, gf, &[cratonvm_types::GC_FLAG_COMPACT]);
            put(
                &mut r,
                layout.value_compact_offset as usize,
                &ptr.to_le_bytes(),
            );
            put(&mut r, layout.coder_compact_offset as usize, &[coder]);
        } else {
            put(
                &mut r,
                layout.value_legacy_offset as usize,
                &ptr.to_le_bytes(),
            );
            put(
                &mut r,
                layout.coder_legacy_offset as usize,
                &u32::from(coder).to_le_bytes(),
            );
        }
        r
    };
    let bytes = |s: &[u8]| -> Vec<u8> {
        let elems: Vec<i64> = s.iter().map(|&b| i64::from(b)).collect();
        crate::tests::synthetic_array(1, &elems)
    };
    let utf16 = |units: &[u16]| -> Vec<u8> {
        let raw: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        bytes(&raw)
    };
    let before_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/String",
        method_name: "codePointBefore",
        descriptor: "(I)I",
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let count_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/String",
        method_name: "codePointCount",
        descriptor: "(II)I",
        num_jit_args: 3,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    // static int B(String s, int i) { return s.codePointBefore(i); }
    let before_code = [0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac];
    // static int C(String s, int b, int e) { return s.codePointCount(b, e); }
    let count_code = [0x2a, 0x1b, 0x1c, 0xb6, 0x00, 0x02, 0xac];
    let lower =
        |code: &[u8], info: &'static JitInvokeInfo, sl: Option<crate::StringFieldLayout>| {
            let nargs = info.num_jit_args; // 2 or 3, the receiver included
            let params: Vec<IrType> = std::iter::once(IrType::Ref)
                .chain(std::iter::repeat(IrType::Int).take(nargs - 1))
                .collect();
            let mut builder = IrBuilder::new(nargs, nargs);
            builder.set_param_types(&params);
            let invoke_pc = nargs; // after aload_0 and the iloads
            let mut invoke_info = HashMap::new();
            invoke_info.insert(
                invoke_pc,
                (info as *const JitInvokeInfo as usize, nargs, b'I'),
            );
            builder.set_invoke_info(invoke_info);
            let graph = builder
                .build(code, code.len())
                .expect("IR build of the call");
            let schedule = ir_schedule::schedule(&graph);
            let mut helpers = no_helpers();
            helpers.invoke_dispatch = dispatch as *const () as usize;
            helpers.self_call_stack_guard = guard_ok as *const () as usize;
            let empty_hints: HashMap<usize, bool> = HashMap::new();
            let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
            let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
            let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
            let _scope = super::CompileStringLayoutScope::enter(sl);
            lower_inner_with_array_list(
                &crate::DirectHelperTable::EMPTY,
                &graph,
                &schedule,
                nargs,
                nargs,
                &helpers,
                &empty_hints,
                &[],
                None,
                &no_direct,
                &no_ic,
                &no_compact,
                &crate::ir::IrInlineFrameSites::default(),
                &HashMap::new(),
                &std::collections::HashSet::new(),
                None,
            )
            .expect("the call site lowers")
        };
    let before_cm = EpochPinnedBody::new(|| lower(&before_code[..], before_info, Some(layout)));
    let count_cm = EpochPinnedBody::new(|| lower(&count_code[..], count_info, Some(layout)));
    let plain = EpochPinnedBody::new(|| lower(&before_code[..], before_info, None));
    let run = |cm: &EpochPinnedBody<'_>, s: &Vec<u64>, args: &[i32]| -> i32 {
        let mut a = vec![s.as_ptr() as i64]; // Cast: object address as JIT argument
        a.extend(args.iter().map(|&x| i64::from(x)));
        // SAFETY: a body whose only reachable helper is `dispatch`, over
        // live fake objects the prefix only reads.
        let r = unsafe { cm.call(&a) };
        r as i32 // Cast: the int result's low half
    };
    let is_pair = |hi: u16, lo: u16| matches!(char::decode_utf16([hi, lo]).next(), Some(Ok(_)));
    // `StringUTF16.codePointBefore(value, index)`.
    let expect_before = |units: &[u16], index: usize| -> i32 {
        let i = index - 1;
        let c2 = units[i];
        if (0xDC00..0xE000).contains(&c2) && i > 0 && is_pair(units[i - 1], c2) {
            let hi = i32::from(units[i - 1]);
            return ((hi - 0xD800) << 10) + (i32::from(c2) - 0xDC00) + 0x1_0000;
        }
        i32::from(c2)
    };
    // `StringUTF16.codePointCount(value, begin, end)`.
    let expect_count = |units: &[u16], begin: usize, end: usize| -> i32 {
        let mut n = (end - begin) as i32; // Cast: small test range
        let mut i = begin;
        while i < end {
            let c = units[i];
            i += 1;
            if (0xD800..0xDC00).contains(&c) && i < end && is_pair(c, units[i]) {
                n -= 1;
                i += 1;
            }
        }
        n
    };
    for compact in [true, false] {
        // LATIN1.
        let v = bytes(&[b'c', 0xE9, 0xFF, 0x00]);
        let s = string(&v, 0, compact);
        assert_eq!(run(&before_cm, &s, &[1]), i32::from(b'c'));
        assert_eq!(run(&before_cm, &s, &[2]), 0xE9, "zero-extended");
        assert_eq!(run(&before_cm, &s, &[3]), 0xFF, "zero-extended");
        assert_eq!(run(&before_cm, &s, &[4]), 0);
        for bad in [0, 5, -1, i32::MIN, i32::MAX] {
            assert_eq!(
                run(&before_cm, &s, &[bad]),
                MARKER,
                "LATIN1 before({bad}) calls"
            );
        }
        for (b, e) in [(0, 0), (0, 4), (1, 3), (4, 4), (2, 4)] {
            assert_eq!(run(&count_cm, &s, &[b, e]), e - b, "LATIN1 count({b}, {e})");
        }
        for (b, e) in [
            (-1, 2),
            (3, 1),
            (0, 5),
            (5, 5),
            (i32::MIN, 0),
            (0, i32::MAX),
        ] {
            assert_eq!(
                run(&count_cm, &s, &[b, e]),
                MARKER,
                "LATIN1 count({b}, {e}) calls"
            );
        }
        // UTF16: pairs, lone surrogates of both kinds, a reversed pair, a
        // high surrogate last, BMP units on both sides.
        let units: [u16; 11] = [
            0x0041, 0xD83D, 0xDE00, 0xDE00, 0xD800, 0x0042, 0xDBFF, 0xDFFF, 0xDC00, 0xD800, 0xD83D,
        ];
        let w = utf16(&units);
        let s16 = string(&w, 1, compact);
        for index in 1..=units.len() {
            assert_eq!(
                run(&before_cm, &s16, &[index as i32]), // Cast: small test index
                expect_before(&units, index),
                "UTF16 codePointBefore({index}) compact={compact}"
            );
        }
        for bad in [0, 12, -1, i32::MIN] {
            assert_eq!(
                run(&before_cm, &s16, &[bad]),
                MARKER,
                "UTF16 before({bad}) calls"
            );
        }
        for b in 0..=units.len() {
            for e in b..=units.len() {
                assert_eq!(
                    run(&count_cm, &s16, &[b as i32, e as i32]), // Cast: small test indices
                    expect_count(&units, b, e),
                    "UTF16 codePointCount({b}, {e}) compact={compact}"
                );
            }
        }
        for (b, e) in [(-1, 2), (3, 1), (0, 12), (12, 12)] {
            assert_eq!(
                run(&count_cm, &s16, &[b, e]),
                MARKER,
                "UTF16 count({b}, {e}) calls"
            );
        }
        // An odd trailing byte is not a unit.
        let odd = bytes(&[0x00, 0xDC, 0x3D]);
        let s_odd = string(&odd, 1, compact);
        assert_eq!(
            run(&before_cm, &s_odd, &[1]),
            0xDC00,
            "a lone low surrogate"
        );
        assert_eq!(run(&before_cm, &s_odd, &[2]), MARKER);
        assert_eq!(run(&count_cm, &s_odd, &[0, 1]), 1);
        assert_eq!(run(&count_cm, &s_odd, &[0, 2]), MARKER);
        // No String layout in scope: always the call.
        assert_eq!(run(&plain, &s, &[1]), MARKER, "no layout, no prefix");
        drop((v, w, odd));
    }
}

#[test]
pub(in crate::ir_lower::tests) fn opcost_object_array_covariance_elimination() {
    crate::clear_ir_collection_layouts();
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let len = g.add(Op::Const(4), IrType::Int, vec![], None);
    let param_arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let param_val = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let obj_arr = g.add(
        Op::NewArray {
            element_type: 0,
            component_class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, len],
        Some(0),
    );
    let obj_arr_b = g.add(
        Op::NewArray {
            element_type: 0,
            component_class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, len],
        Some(4),
    );
    let str_arr = g.add(
        Op::NewArray {
            element_type: 0,
            component_class_id: 77,
        },
        IrType::Ref,
        vec![ctrl, mem, len],
        Some(8),
    );
    let exact_str = g.add(
        Op::New {
            class_id: 77,
            num_fields: 0,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(12),
    );
    let obj_cid_arr = g.add(
        Op::NewArray {
            element_type: 0,
            component_class_id: 1,
        },
        IrType::Ref,
        vec![ctrl, mem, len],
        Some(16),
    );
    let region = g.add(Op::Region, IrType::Control, vec![ctrl, ctrl], None);
    let obj_phi = g.add(Op::Phi, IrType::Ref, vec![region, obj_arr, obj_arr_b], None);
    let mixed_phi = g.add(Op::Phi, IrType::Ref, vec![region, obj_arr, param_arr], None);
    let root_obj_arr = g.add(
        Op::IndyRootRef {
            slot_addr: 0xABCD_0001,
        },
        IrType::Ref,
        vec![ctrl],
        None,
    );
    let root_other_arr = g.add(
        Op::IndyRootRef {
            slot_addr: 0xABCD_0002,
        },
        IrType::Ref,
        vec![ctrl],
        None,
    );
    g.const_ref_meta.insert(
        0xABCD_0001,
        crate::ir::ConstRefMeta {
            non_null: true,
            exact_class_id: Some(901),
            prim_array_tag: None,
            array_length: Some(4),
            static_field: None,
        },
    );
    g.const_ref_meta.insert(
        0xABCD_0002,
        crate::ir::ConstRefMeta {
            non_null: true,
            exact_class_id: Some(902),
            prim_array_tag: None,
            array_length: Some(4),
            static_field: None,
        },
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl], None);

    assert!(
        aastore_covariance_known_safe(&g, obj_arr, param_val),
        "exact NewArray Object[] accepts any reference value"
    );
    assert!(
        aastore_covariance_known_safe(&g, obj_phi, param_val),
        "Phi of exact NewArray Object[] accepts any reference value"
    );
    assert!(
        !aastore_covariance_known_safe(&g, mixed_phi, param_val),
        "Phi merging exact Object[] and unknown Param must not elide check"
    );
    assert!(
        !aastore_covariance_known_safe(&g, param_arr, param_val),
        "declared Object[] Param could dynamically hold String[]"
    );
    assert!(
        !aastore_covariance_known_safe(&g, str_arr, param_val),
        "non-Object NewArray requires exact component bound"
    );
    assert!(
        aastore_covariance_known_safe(&g, str_arr, exact_str),
        "exact component New matches NewArray component class"
    );
    assert!(!aastore_covariance_known_safe(&g, root_obj_arr, param_val));

    crate::register_ir_collection_layouts_with_objects(1, None, None, 901, 1);
    assert!(
        aastore_covariance_known_safe(&g, obj_cid_arr, param_val),
        "anewarray java/lang/Object (with resolved object_class_id) accepts any reference"
    );
    assert!(
        aastore_covariance_known_safe(&g, root_obj_arr, param_val),
        "IndyRootRef with exact [Ljava/lang/Object; class_id accepts any reference"
    );
    assert!(
        !aastore_covariance_known_safe(&g, root_other_arr, param_val),
        "IndyRootRef of another array class still needs covariance check"
    );
    crate::clear_ir_collection_layouts();
}

#[test]
pub(in crate::ir_lower::tests) fn opcost_sb_prefix_unlocks_home_dropped_and_compact_proven() {
    let Some(kind) =
        crate::ir_string_search_prefix_kind("java/lang/StringBuilder", "length", "()I")
    else {
        return;
    };
    const STR_CID: u32 = 0x0045_0001;
    const SB_CID: u32 = 0x0045_0002;
    let sb = crate::tests::synthetic_string_builder_layout(SB_CID);
    let layout = crate::tests::synthetic_string_layout(STR_CID).with_builder(Some(sb));
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/StringBuilder",
        method_name: "length",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));

    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let sb_new = g.add(
        Op::New {
            class_id: SB_CID,
            num_fields: 3,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    let len_call = g.add(
        Op::Call {
            info_ptr: info as *const JitInvokeInfo as usize,
        },
        IrType::Int,
        vec![ctrl, mem, sb_new],
        Some(4),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, len_call], Some(8));

    let g_leak = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(ir_schedule::schedule(g_leak)));
    let plan = Box::leak(Box::new(plan_slots(g_leak, schedule, None)));
    let mut lo = lowerer_over(g_leak, schedule, plan, 16384);
    let n = g_leak.nodes.len();
    lo.home_dropped = vec![false; n];
    lo.home_dropped[len_call as usize] = true;
    lo.gp_reg_of = vec![None; n];
    lo.gp_reg_of[len_call as usize] = Some(12); // R12 resident result
    lo.alloc_slot_checked(sb_new)
        .expect("sb_new has a planned spill slot");
    lo.cur_lower_block = 0;
    lo.cur_def = Some(len_call);
    lo.layout_proven.push((sb_new, 0));
    lo.compact_proven.push((sb_new, 0));

    let start_pos = lo.buf.pos();
    lo.try_emit_ir_string_builder_prefix(len_call, info, kind, layout);
    assert!(
        lo.call_prefix_join.is_some(),
        "home_dropped length() with resident GPR must emit SB prefix"
    );
    let bytes = &lo.buf.as_slice()[start_pos..lo.buf.pos()];
    let compact_test = [
        0xF6,
        0x40,
        cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8,
        cratonvm_types::GC_FLAG_COMPACT,
    ];
    assert!(
        !bytes.windows(4).any(|w| w == compact_test),
        "compact_proven StringBuilder prefix must skip GC_FLAG_COMPACT check"
    );
    // Mov to R12 (`REX = 0x49, 0x89, 0xC4`) must be emitted even with home_dropped.
    assert!(
        bytes.windows(3).any(|w| w == [0x49, 0x89, 0xC4]),
        "home_dropped prefix must publish result into assigned GPR R12"
    );
}

#[test]
pub(in crate::ir_lower::tests) fn opcost_card_mark_sib_fast_checks_null_and_young_first() {
    #[repr(align(16))]
    struct FakeView([usize; 32]);
    let mut card_buf = vec![0u8; 64];
    let mut view = Box::new(FakeView([0; 32]));
    let map_addr = card_buf.as_mut_ptr() as usize;
    view.0[0] = 0x5745_4956_4452_4143; // CARD_VIEW_MAGIC
    view.0[32 / 8] = 8; // CARD_VIEW_FLAG_WHOLE_HEAP
    view.0[152 / 8] = 0x1000_0000; // CARD_VIEW_HULL_LO
    view.0[160 / 8] = 64usize << cratonvm_types::CARD_SHIFT; // CARD_VIEW_HULL_LEN
    view.0[168 / 8] = map_addr.wrapping_neg(); // CARD_VIEW_HULL_BUF_NEG
    let view_addr = (&*view as *const FakeView) as usize;

    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    g.entry = start;
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl], Some(0));
    let g_leak = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(ir_schedule::schedule(g_leak)));
    let plan = Box::leak(Box::new(plan_slots(g_leak, schedule, None)));
    let mut lo = lowerer_over(g_leak, schedule, plan, 16384);
    lo.ref_store_post_skip_mask = Some(cratonvm_types::GC_FLAG_OLD_GEN);

    let mut to_helper = Vec::new();
    let mut done = Vec::new();
    let p0 = lo.buf.pos();
    let emitted = lo.emit_ir_whole_heap_card_mark_sib_fast(
        view_addr,
        cratonvm_types::CARD_SHIFT as u8,
        None,
        &mut to_helper,
        &mut done,
    );
    assert!(emitted, "whole-heap baked view emits sib fast path");
    let bytes = &lo.buf.as_slice()[p0..lo.buf.pos()];
    // With GC_FLAG_OLD_GEN, starts with TEST BYTE [RAX + off], mask (4 bytes) + JZ rel32 (6 bytes),
    // and defers TEST RDX, RDX (3 bytes) + JZ rel32 (6 bytes) to the clean-card edge.
    let old_test = [
        0xF6,
        0x40,
        cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8,
        cratonvm_types::GC_FLAG_OLD_GEN,
    ];
    assert_eq!(&bytes[0..4], &old_test);
    assert_eq!(&bytes[4..6], &[0x0F, 0x84]);
    assert!(
        bytes[10..]
            .windows(5)
            .any(|w| w == [0x48, 0x85, 0xD2, 0x0F, 0x84]),
        "deferred TEST RDX, RDX; JZ must appear on clean-card edge"
    );
    // Verify no duplicate OLD_GEN check later at `miss`.
    assert_eq!(
        bytes.windows(4).filter(|w| *w == old_test).count(),
        1,
        "OLD_GEN test must appear once at entry and not be duplicated at miss"
    );
    drop(view);
}

#[test]
pub(in crate::ir_lower::tests) fn opcost_unbox_dominating_exact_class_and_epoch_cse() {
    let uop = crate::ir::UnboxOp::IntValue;
    if crate::ir::unbox_offsets(uop, UNBOX_TEST_CLASS_ID).is_none() {
        return;
    }
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let r0 = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let r1 = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let exact = g.add(
        Op::ExactClassIs {
            class_id: UNBOX_TEST_CLASS_ID,
        },
        IrType::Int,
        vec![r0],
        Some(0),
    );
    let _guard = g.add(
        Op::Guard { bci: 0 },
        IrType::Void,
        vec![ctrl, exact],
        Some(0),
    );
    let u0 = g.add(
        Op::Unbox {
            op: uop,
            class_id: UNBOX_TEST_CLASS_ID,
        },
        IrType::Int,
        vec![ctrl, mem, r0],
        Some(1),
    );
    let u1 = g.add(
        Op::Unbox {
            op: uop,
            class_id: UNBOX_TEST_CLASS_ID,
        },
        IrType::Int,
        vec![ctrl, mem, r1],
        Some(2),
    );
    let sum = g.add(Op::Add, IrType::Int, vec![u0, u1], Some(3));
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(4));

    let g_leak = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(ir_schedule::schedule(g_leak)));
    let plan = Box::leak(Box::new(plan_slots(g_leak, schedule, None)));
    let mut lo = lowerer_over(g_leak, schedule, plan, 16384);
    lo.cur_lower_block = 0;

    lo.cur_def = Some(u0);
    assert!(
        lo.unbox_exact_class_proven(r0, UNBOX_TEST_CLASS_ID),
        "dominating Guard(ExactClassIs) proves exact class for u0"
    );
    assert!(
        !lo.unbox_exact_class_proven(r1, UNBOX_TEST_CLASS_ID),
        "unguarded Param r1 must still require runtime null/class check"
    );
    lo.block_cse_mark_layout_epoch_checked();

    lo.cur_def = Some(u1);
    assert!(
        lo.unbox_layout_epoch_proven(),
        "second Op::Unbox in same safepoint-free block reuses layout-epoch check"
    );
}

#[test]
pub(in crate::ir_lower::tests) fn opcost_block_cse_invalidates_on_store_call_and_slot_reuse() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let recv = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let idx = g.add(Op::Const(1), IrType::Int, vec![], None);
    let l1 = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, mem, recv, idx],
        Some(1),
    );
    let add = g.add(Op::Add, IrType::Int, vec![idx, idx], Some(2));
    let l2 = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, mem, recv, add],
        Some(3),
    );
    let st = g.add(
        Op::Store(MemKind::Ref),
        IrType::Memory,
        vec![ctrl, mem, recv, idx, l2],
        Some(4),
    );
    let l3 = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, st, recv, idx],
        Some(5),
    );
    let call = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![ctrl, st, l3],
        Some(6),
    );
    let l4 = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, call, recv, idx],
        Some(7),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, l1, l4], Some(8));

    let g_leak = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(ir_schedule::schedule(g_leak)));
    let plan = Box::leak(Box::new(plan_slots(g_leak, schedule, None)));
    let mut lo = lowerer_over(g_leak, schedule, plan, 16384);
    let n = g_leak.nodes.len();
    lo.defined_nodes = vec![false; n];
    lo.home_dropped = vec![false; n];
    lo.cur_lower_block = 0;
    assert_eq!(schedule.node_to_block[l1 as usize], 0);
    assert_eq!(schedule.node_to_block[l2 as usize], 0);
    assert_eq!(schedule.node_to_block[l3 as usize], 0);
    assert_eq!(schedule.node_to_block[l4 as usize], 0);

    // 1. l1 loads and sets epoch + read-bounds + field load cache.
    lo.cur_def = Some(l1);
    lo.defined_nodes[l1 as usize] = true;
    lo.block_cse_record_field_load(recv, 0, IrType::Ref, 16, b'L', false);
    lo.block_cse_mark_layout_epoch_checked();
    lo.block_cse_mark_read_bounds_live_checked();

    // 2. Across pure `Add`, l2 must hit l1 and retain epoch/read-bounds proofs.
    lo.cur_def = Some(l2);
    assert!(lo.block_cse_layout_epoch_checked());
    assert!(lo.block_cse_read_bounds_live_checked());
    assert_eq!(
        lo.block_cse_find_field_load(recv, 0, IrType::Ref, 16, b'L'),
        Some(l1)
    );

    // 3. After `Store`, l3 must miss and both proofs must be invalidated.
    lo.cur_def = Some(l3);
    assert!(!lo.block_cse_layout_epoch_checked());
    assert!(!lo.block_cse_read_bounds_live_checked());
    assert_eq!(
        lo.block_cse_find_field_load(recv, 0, IrType::Ref, 16, b'L'),
        None
    );

    // 4. Re-record at l3, then advance past `Call` to l4 -> must invalidate again.
    lo.defined_nodes[l3 as usize] = true;
    lo.block_cse_record_field_load(recv, 0, IrType::Ref, 16, b'L', false);
    lo.block_cse_mark_layout_epoch_checked();
    lo.cur_def = Some(l4);
    assert!(!lo.block_cse_layout_epoch_checked());
    assert_eq!(
        lo.block_cse_find_field_load(recv, 0, IrType::Ref, 16, b'L'),
        None
    );
}
