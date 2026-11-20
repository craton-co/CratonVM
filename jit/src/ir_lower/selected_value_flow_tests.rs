// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir::IrBuilder;
use crate::regalloc::{ValueFlow, ValueGroup};
use crate::x64::isel::{ImmForm, IrAddr, MInst, Op as MachineOp, Ty};

fn fixture() -> (Graph, Schedule, NodeId, Vec<NodeId>, Vec<NodeId>) {
    let code = [0x1a, 0x1b, 0x05, 0x78, 0x60, 0x10, 7, 0x60, 0xac];
    let mut graph = IrBuilder::new(2, 2)
        .build(&code, code.len())
        .expect("x + (y << 2) + 7");
    // This pure fixture has no call or trap; frame-reader tests add their own snapshot.
    graph.safepoints.clear();
    let schedule = crate::ir_schedule::schedule(&graph);
    let root = graph.nodes.iter().rposition(|n| n.op == Op::Add).unwrap() as NodeId;
    let inputs = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(id, n)| matches!(n.op, Op::Param(_)).then_some(id as NodeId))
        .collect();
    let covered = schedule
        .blocks
        .iter()
        .flat_map(|b| b.nodes.iter().copied())
        .filter(|&id| {
            matches!(
                graph.nodes[id as usize].op,
                Op::Const(_) | Op::Shl | Op::Add
            )
        })
        .collect();
    (graph, schedule, root, inputs, covered)
}

fn group(root: NodeId, inputs: &[NodeId], covered: &[NodeId]) -> ValueGroup {
    let instruction = if inputs.len() == 1 {
        MInst::AluRI {
            op: MachineOp::Add,
            ty: Ty::I32,
            dst: root,
            lhs: inputs[0],
            imm: 7,
            form: ImmForm::Imm8,
        }
    } else {
        MInst::Lea {
            dst: root,
            ty: Ty::I32,
            addr: IrAddr {
                base: Some(inputs[0]),
                index: Some(inputs[1]),
                scale: 4,
                disp: 7,
            },
        }
    };
    ValueGroup {
        instructions: vec![instruction],
        covered: covered.to_vec(),
    }
}

#[test]
fn selected_flow_keeps_external_inputs_until_the_emitted_root() {
    let (graph, schedule, root, inputs, covered) = fixture();
    let flow =
        ValueFlow::new(&graph, &schedule, vec![group(root, &inputs, &covered)], &[]).unwrap();
    let (plan, _) = plan_slots_with_value_flow(&graph, &schedule, None, &flow);
    verify_slot_colouring_with_value_flow(&graph, &schedule, &plan, &flow).unwrap();
    let live = crate::regalloc::build_live_model_with_value_flow(&graph, &schedule, &flow);
    let pos = live.pos_of[root as usize].unwrap();
    for &input in &inputs {
        assert_eq!(live.uses[input as usize], vec![pos]);
        assert!(plan.range[input as usize].unwrap().hi >= pos);
    }
    for &id in &covered {
        if id == root {
            continue;
        }
        assert!(flow.is_absorbed(id));
        assert!(plan.node_color[id as usize].is_none());
        assert!(live.range[id as usize].is_none());
    }
    use crate::regalloc::{PhysReg, RegFile, RegSpec};
    let regs = RegFile::from_specs([RegSpec {
        reg: PhysReg::gp(3),
        caller_saved: false,
    }]);
    let model = crate::regalloc::MachineModel::for_graph(&graph, &schedule, &live, regs);
    let allocation = crate::regalloc::allocate_linear_scan(&graph, &live, &model).unwrap();
    crate::regalloc::verify_allocation(&graph, &live, &model, &allocation).unwrap();
    for &input in &inputs {
        assert!(allocation.location_at(input, pos).is_some());
    }
}

#[test]
fn graph_value_flow_preserves_both_existing_models() {
    let (graph, schedule, _, _, _) = fixture();
    let flow = ValueFlow::default();
    let (actual, _) = plan_slots_with_value_flow(&graph, &schedule, None, &flow);
    let expected = plan_slots(&graph, &schedule, None);
    assert_eq!(actual.node_color, expected.node_color);
    assert_eq!(actual.range, expected.range);
    assert_eq!(actual.class, expected.class);
    assert_eq!(actual.slots, expected.slots);
    let a = crate::regalloc::build_live_model_with_value_flow(&graph, &schedule, &flow);
    let b = crate::regalloc::build_live_model(&graph, &schedule);
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
}

#[test]
fn selected_flow_refuses_hidden_frame_readers_and_external_uses() {
    let (mut graph, schedule, root, inputs, covered) = fixture();
    let interior = covered
        .iter()
        .copied()
        .find(|&id| graph.nodes[id as usize].op == Op::Shl)
        .unwrap();
    assert!(ValueFlow::new(
        &graph,
        &schedule,
        vec![group(root, &inputs, &covered)],
        &[interior]
    )
    .is_err());
    graph.safepoints.push(crate::ir::SafepointSnapshot::new(
        0,
        vec![interior],
        vec![],
        vec![],
    ));
    assert!(ValueFlow::new(&graph, &schedule, vec![group(root, &inputs, &covered)], &[]).is_err());
    graph.safepoints.clear();
    let extra = graph.add(Op::Neg, IrType::Int, vec![interior], None);
    let sum = graph.add(Op::Add, IrType::Int, vec![root, extra], None);
    let ret = graph.nodes.iter().position(|n| n.op == Op::Return).unwrap();
    graph.nodes[ret].inputs[1] = sum;
    let schedule = crate::ir_schedule::schedule(&graph);
    assert!(ValueFlow::new(&graph, &schedule, vec![group(root, &inputs, &covered)], &[]).is_err());
}

#[test]
fn selected_flow_refuses_invalid_inputs_and_overlapping_ownership() {
    let (graph, schedule, root, inputs, covered) = fixture();
    assert!(ValueFlow::new(
        &graph,
        &schedule,
        vec![group(root, &[NO_NODE], &covered)],
        &[]
    )
    .is_err());
    assert!(ValueFlow::new(
        &graph,
        &schedule,
        vec![
            group(root, &inputs, &covered),
            group(root, &inputs, &covered)
        ],
        &[]
    )
    .is_err());
}

#[test]
fn selected_flow_fallback_keeps_absorbed_definitions_unlocated() {
    let (graph, schedule, root, inputs, covered) = fixture();
    let flow =
        ValueFlow::new(&graph, &schedule, vec![group(root, &inputs, &covered)], &[]).unwrap();
    let mut wants = crate::regalloc::live_model_wants_loc(&graph, &schedule);
    flow.apply_wants(&mut wants);
    let core = crate::regalloc::liveness_core_with_value_flow(
        &graph,
        &schedule,
        &wants,
        crate::regalloc::LivenessRules {
            token_edges_not_uses: false,
            phi_edge_interfere: false,
            collect_reads: true,
        },
        0,
        0,
        &flow,
    );
    assert!(!core.converged);
    let plan = plan_slots_from_core(&graph, &schedule, None, &wants, core.slot_view());
    verify_slot_colouring_with_value_flow(&graph, &schedule, &plan, &flow).unwrap();
    let live = crate::regalloc::live_model_from_core(&graph, &schedule, wants, core);
    for &id in &covered {
        if id != root {
            assert!(live.range[id as usize].is_none());
            assert!(plan.node_color[id as usize].is_none());
        }
    }
    for &id in &inputs {
        assert!(live.range[id as usize].is_some());
        assert!(plan.node_color[id as usize].is_some());
    }
}

#[test]
fn selected_flow_uses_declared_operands_instead_of_the_graph_frontier() {
    let (graph, schedule, root, inputs, covered) = fixture();
    let flow = ValueFlow::new(
        &graph,
        &schedule,
        vec![group(root, &inputs[..1], &covered)],
        &[],
    )
    .expect("declared selected operands are authoritative");
    let live = crate::regalloc::build_live_model_with_value_flow(&graph, &schedule, &flow);
    assert_eq!(
        live.uses[inputs[0] as usize],
        vec![live.pos_of[root as usize].unwrap()]
    );
    assert!(live.uses[inputs[1] as usize].is_empty());
}

#[test]
fn selected_flow_ignores_graph_users_that_are_not_emitted() {
    let (mut graph, schedule, root, inputs, covered) = fixture();
    let interior = covered
        .iter()
        .copied()
        .find(|&id| graph.nodes[id as usize].op == Op::Shl)
        .unwrap();
    graph.add(Op::Neg, IrType::Int, vec![interior], None);
    assert!(ValueFlow::new(&graph, &schedule, vec![group(root, &inputs, &covered)], &[]).is_ok());
}

#[test]
fn selected_flow_checks_the_two_address_copy_prefix() {
    let (graph, schedule, root, inputs, covered) = fixture();
    let alu = MInst::AluRR {
        op: MachineOp::Add,
        ty: Ty::I32,
        dst: root,
        lhs: inputs[0],
        rhs: inputs[1],
    };
    let group = |src| ValueGroup {
        instructions: vec![
            MInst::Move {
                dst: root,
                ty: Ty::I64,
                src,
            },
            alu,
        ],
        covered: covered.clone(),
    };
    assert!(ValueFlow::new(&graph, &schedule, vec![group(inputs[0])], &[]).is_ok());
    assert!(ValueFlow::new(&graph, &schedule, vec![group(inputs[1])], &[]).is_err());
}


#[test]
#[cfg(target_arch = "x86_64")]
fn absorbed_definitions_emit_nothing_and_carries_use_the_selected_inputs() {
    let (graph, schedule, root, inputs, covered) = fixture();
    let flow =
        ValueFlow::new(&graph, &schedule, vec![group(root, &inputs, &covered)], &[]).unwrap();
    let (slots, _) = plan_slots_with_value_flow(&graph, &schedule, None, &flow);
    let graph = Box::leak(Box::new(graph));
    let schedule = Box::leak(Box::new(schedule));
    let slots = Box::leak(Box::new(slots));
    let mut lowerer = super::tests::lowerer_over(graph, schedule, slots, 16384);
    lowerer.value_flow = flow;
    lowerer.prepare_fusion_tables();
    for &input in &inputs {
        assert_eq!(lowerer.use_count[input as usize], 1, "one machine reader");
    }
    for &id in &covered {
        if id == root {
            continue;
        }
        assert_eq!(lowerer.use_count[id as usize], 0);
        assert!(lowerer.carry_of[id as usize].is_none());
        assert!(lowerer.tail_carry_of[id as usize].is_none());
        lowerer.lower_data_node_tracked(id);
        assert_eq!(
            lowerer.buf.pos(),
            0,
            "no instruction or publication for n{id}"
        );
        assert!(
            lowerer.latched_bailout.borrow().is_none(),
            "no attempt to allocate an omitted home"
        );
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn a_folded_binding_refusal_never_emits_the_original_graph_operation() {
    let (graph, schedule, root, _, _) = fixture();
    let mir = build_mir_plan_with(&graph, &schedule, Default::default()).unwrap();
    let flow = mir.value_flow(&graph, &schedule, &[]).unwrap().unwrap();
    assert!(flow.requires_selected_emission(root));
    assert!(flow.has_absorbed_definitions());
    let (slots, _) = plan_slots_with_value_flow(&graph, &schedule, None, &flow);
    let graph = Box::leak(Box::new(graph));
    let schedule = Box::leak(Box::new(schedule));
    let slots = Box::leak(Box::new(slots));
    let mut lowerer = super::tests::lowerer_over(graph, schedule, slots, 16384);
    lowerer.set_mir(mir, MirMode::Off);
    lowerer.value_flow = flow;
    lowerer.reg_alloc_on = true;
    // An unsupported split binding must reject this whole attempt.
    lowerer.ls_managed.resize(graph.nodes.len(), false);
    lowerer.ls_managed[root as usize] = true;
    lowerer.lower_data_node_tracked(root);
    assert_eq!(lowerer.buf.pos(), 0);
    let refusal = lowerer.latched_bailout.borrow();
    assert!(matches!(
        &refusal.as_ref().expect("binding must refuse").reason,
        BailoutReason::UnsupportedShape(
            "folded integer instruction could not bind allocated operands"
        )
    ));
}

#[test]
fn hidden_scalar_frame_values_are_protected_before_selection() {
    let (graph, schedule, root, _, covered) = fixture();
    let interior = covered
        .iter()
        .copied()
        .find(|&id| graph.nodes[id as usize].op == Op::Shl)
        .unwrap();
    let sr = ScalarReplacementMap {
        objects: HashMap::from([(
            NO_NODE,
            VirtualObjectInfo {
                class_id: 1,
                array_element_type: None,
                num_fields: 1,
                field_values: vec![Some(interior)],
                new_ctrl: graph.entry,
                store_ctrls: vec![],
                store_nodes: vec![],
                store_fields: vec![],
                loop_phis: None,
                non_null_cond: None,
            },
        )]),
    };
    let protected = hidden_frame_values(&graph, &schedule, Some(&sr));
    assert_eq!(protected, [interior]);
    let mir =
        build_mir_plan_with_protected(&graph, &schedule, Default::default(), &protected, None).unwrap();
    let flow = mir
        .value_flow(&graph, &schedule, &protected)
        .unwrap()
        .unwrap();
    assert!(!flow.is_absorbed(interior));
    assert!(!mir.tile(root).unwrap().covered.contains(&interior));
    let (slots, _) = plan_slots_with_value_flow(&graph, &schedule, Some(&sr), &flow);
    assert!(slots.node_color[interior as usize].is_some());
    assert_eq!(slots.class[interior as usize], Some(SlotClass::Pinned));
    verify_slot_colouring_with_value_flow(&graph, &schedule, &slots, &flow).unwrap();
}


#[test]
fn selected_dying_input_and_result_share_one_gpr_in_read_write_order() {
    use crate::regalloc::{MachineModel, PhysReg, RegFile, RegSpec, ValueLoc};
    let code = [0x1a, 0x10, 7, 0x60, 0xac];
    let mut graph = IrBuilder::new(1, 1).build(&code, code.len()).unwrap();
    graph.safepoints.clear();
    let schedule = crate::ir_schedule::schedule(&graph);
    let mir = build_mir_plan_with(&graph, &schedule, Default::default()).unwrap();
    let flow = mir.value_flow(&graph, &schedule, &[]).unwrap().unwrap();
    let live = crate::regalloc::build_live_model_with_value_flow(&graph, &schedule, &flow);
    let root = graph.nodes.iter().position(|n| n.op == Op::Add).unwrap() as NodeId;
    let input = graph
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Param(_)))
        .unwrap() as NodeId;
    let at = live.pos_of[root as usize].unwrap();
    let mut model = MachineModel::for_graph(
        &graph,
        &schedule,
        &live,
        RegFile::from_specs([RegSpec {
            reg: PhysReg::gp(3),
            caller_saved: false,
        }]),
    );
    mir.add_allocated_requirements(&live, &mut model).unwrap();
    let allocation = crate::regalloc::allocate_linear_scan(&graph, &live, &model).unwrap();
    assert_eq!(
        allocation.location_at(input, at),
        Some(ValueLoc::Reg(PhysReg::gp(3))),
        "the instruction reads its dying source in RBX"
    );
    assert_eq!(
        allocation.location_at(root, at),
        Some(ValueLoc::Reg(PhysReg::gp(3))),
        "the result reuses RBX after the read"
    );
    assert_eq!((allocation.spills, allocation.reloads), (0, 0));
    crate::regalloc::verify_allocation(&graph, &live, &model, &allocation).unwrap();
}

