// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir::IrBuilder;
use crate::x64::isel::{IrAddr, MInst, Operand, Req, Rule, Ty};

/// Bind an operation different from the source graph and execute it. This
/// proves emission consumes the selection for both register and frame values.
#[test]
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn allocated_alu_emission_consumes_the_selected_instruction() {
    let mut executed = 0;
    for (folded, wide) in [(false, false), (true, false), (true, true)] {
        let ty = if wide { Ty::I64 } else { Ty::I32 };
        for result_reg in [Some(13), Some(3), Some(12), None] {
            for lhs_reg in [Some(3), Some(0), Some(1), None] {
                for rhs_reg in [Some(12), Some(0), Some(1), None] {
                    if lhs_reg.is_some() && lhs_reg == rhs_reg {
                        continue;
                    }
                    for (form, shift) in [
                        (Rule::AluReg, false),
                        (Rule::AluImm, false),
                        (Rule::Lea, false),
                        (Rule::AluReg, true),
                    ] {
                        if shift && [lhs_reg, rhs_reg].into_iter().flatten().any(|r| r <= RCX) {
                            continue;
                        }
                        let mut builder = IrBuilder::new(2, if wide { 4 } else { 2 });
                        builder
                            .set_param_types(&[if wide { IrType::Long } else { IrType::Int }; 2]);
                        let code: &[u8] = match (folded, wide) {
                            (false, false) => &[0x1a, 0x1b, 0x60, 0xac],
                            (true, false) => &[0x1a, 0x1b, 0x05, 0x78, 0x60, 0x10, 7, 0x60, 0xac],
                            (true, true) => &[0x1e, 0x20, 0x05, 0x79, 0x61, 0x0a, 0x61, 0xad],
                            _ => unreachable!(),
                        };
                        let mut graph = builder.build(code, code.len()).expect("add graph");
                        if folded {
                            graph.safepoints.clear();
                        }
                        let graph: &'static Graph = Box::leak(Box::new(graph));
                        let schedule: &'static Schedule =
                            Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
                        let result = graph
                            .nodes
                            .iter()
                            .rposition(|n| n.op == Op::Add)
                            .expect("sum") as NodeId;
                        let params: Vec<_> = graph
                            .nodes
                            .iter()
                            .enumerate()
                            .filter_map(|(id, n)| {
                                matches!(n.op, Op::Param(_)).then_some(id as NodeId)
                            })
                            .collect();
                        let (lhs, rhs) = (params[0], params[1]);
                        let instruction = match form {
                            Rule::AluReg => MInst::AluRR {
                                op: if shift {
                                    crate::x64::isel::Op::Shl
                                } else {
                                    crate::x64::isel::Op::Sub
                                },
                                ty,
                                dst: result,
                                lhs,
                                rhs,
                            },
                            Rule::AluImm => MInst::AluRI {
                                op: crate::x64::isel::Op::Sub,
                                ty,
                                dst: result,
                                lhs,
                                imm: 5,
                                form: crate::x64::isel::ImmForm::Imm8,
                            },
                            Rule::Lea => MInst::Lea {
                                dst: result,
                                ty,
                                addr: IrAddr {
                                    base: Some(lhs),
                                    index: Some(rhs),
                                    scale: 1,
                                    disp: -10,                                },                            },
                            _ => unreachable!(),
                        };
                        let covered = if folded {
                            schedule
                                .blocks
                                .iter()
                                .flat_map(|b| b.nodes.iter().copied())
                                .filter(|&id| {
                                    matches!(
                                        graph.nodes[id as usize].op,
                                        Op::Const(_) | Op::Shl | Op::Add
                                    )
                                })
                                .collect()
                        } else {
                            vec![result]
                        };
                        let flow = if folded {
                            crate::regalloc::ValueFlow::new(
                                graph,
                                schedule,
                                vec![crate::regalloc::ValueGroup {
                                    instructions: vec![instruction],
                                    covered: covered.clone(),
                                }],
                                &[],
                            )
                            .unwrap()
                        } else {
                            Default::default()
                        };
                        let slots: &'static SlotPlan = Box::leak(Box::new(
                            plan_slots_with_value_flow(graph, schedule, None, &flow).0,
                        ));
                        let mut machine = build_mir_plan(graph, schedule).expect("machine plan");
                        let (block, tile) = machine.tile_of[result as usize].expect("sum tile");
                        machine.blocks[block as usize].tiles[tile as usize] =
                            crate::x64::isel::Tile::for_test(
                                result,
                                covered,
                                vec![instruction],
                                form,
                            );
                        let mut lowerer = super::tests::lowerer_over(graph, schedule, slots, 16384);
                        lowerer.set_mir(machine, MirMode::Off);
                        lowerer.value_flow = flow;
                        lowerer.reg_alloc_on = true;
                        lowerer.gp_reg_of.resize(graph.nodes.len(), None);
                        lowerer.gp_reg_live.resize(graph.nodes.len(), false);
                        lowerer.gp_reg_of[lhs as usize] = lhs_reg.filter(|&reg| reg > 1);
                        lowerer.gp_reg_of[rhs as usize] = rhs_reg.filter(|&reg| reg > 1);
                        lowerer.gp_reg_of[result as usize] = result_reg;
                        lowerer.mark_gp_reg_live(lhs);
                        lowerer.mark_gp_reg_live(rhs);
                        let lhs_slot = lowerer.alloc_slot(lhs);
                        let rhs_slot = lowerer.alloc_slot(rhs);
                        lowerer.cur_def = Some(result);
                        let slot = lowerer.alloc_slot(result);
                        for (input, reg) in [(lhs, lhs_reg), (rhs, rhs_reg)] {
                            if form == Rule::AluImm && input == rhs {
                                continue;
                            }
                            match reg {
                                Some(0) => {
                                    lowerer.live_carry = Some((input, result, 0, lowerer.buf.pos()))
                                }
                                Some(1) => lowerer.deferred_rcx = Some((input, result, 1)),
                                _ => {}
                            }
                        }
                        assert!(
                        lowerer.reg_alloc_direct_alu(result, slot),
                        "{form:?} folded={folded} wide={wide} shift={shift} result={result_reg:?} lhs={lhs_reg:?}@{lhs_slot} rhs={rhs_reg:?}@{rhs_slot}"
                    );
                        assert!(
                            lowerer.live_carry.is_none(),
                            "selected instruction consumes its RAX source"
                        );
                        assert!(
                            lowerer.deferred_rcx.is_none(),
                            "selected instruction consumes its RCX source"
                        );
                        let mut code = vec![
                            0x53, 0x41, 0x54, 0x41, 0x55, 0x55, 0x48, 0x89, 0xE5, 0x48, 0x81, 0xEC,
                            0x00, 0x40, 0x00, 0x00,
                        ];
                        for (input, offset, value) in [(lhs, lhs_slot, 17), (rhs, rhs_slot, 5)] {
                            if form == Rule::AluImm && input == rhs {
                                continue;
                            }
                            code.extend(
                                crate::x64::isel::select(&Req::new(
                                    crate::x64::isel::Op::Mov,
                                    Ty::I64,
                                    Operand::Gpr(0),
                                    Operand::Imm(value),
                                ))
                                .expect("argument value")
                                .encoded
                                .bytes,
                            );
                            code.extend(
                                crate::x64::isel::select(&Req::new(
                                    crate::x64::isel::Op::Mov,
                                    Ty::I64,
                                    Operand::Mem(crate::x64::isel::Mem::base_disp(
                                        5,
                                        -i64::from(offset),
                                    )),
                                    Operand::Gpr(0),
                                ))
                                .expect("argument home")
                                .encoded
                                .bytes,
                            );
                        }
                        for (input, reg, offset) in
                            [(lhs, lhs_reg, lhs_slot), (rhs, rhs_reg, rhs_slot)]
                        {
                            if form == Rule::AluImm && input == rhs {
                                continue;
                            }
                            if let Some(reg) = reg {
                                code.extend(
                                    crate::x64::isel::select(&Req::new(
                                        crate::x64::isel::Op::Mov,
                                        Ty::I64,
                                        Operand::Gpr(reg),
                                        Operand::Mem(crate::x64::isel::Mem::base_disp(
                                            5,
                                            -i64::from(offset),
                                        )),
                                    ))
                                    .expect("argument register")
                                    .encoded
                                    .bytes,
                                );
                            }
                        }
                        code.extend_from_slice(lowerer.buf.as_slice());
                        code.extend(
                            crate::x64::isel::select(&Req::new(
                                crate::x64::isel::Op::Mov,
                                Ty::I64,
                                Operand::Gpr(0),
                                Operand::Gpr(result_reg.unwrap_or(0)),
                            ))
                            .expect("return location")
                            .encoded
                            .bytes,
                        );
                        code.extend_from_slice(&[
                            0x48, 0x81, 0xC4, 0x00, 0x40, 0x00, 0x00, 0x5D, 0x41, 0x5D, 0x41, 0x5C,
                            0x5B, 0xC3,
                        ]);
                        let mut buffer = ExecutableBuffer::new(16384).expect("native fixture");
                        buffer.emit(&code);
                        buffer.finalize();
                        // SAFETY: the leaf saves nonvolatile registers, accesses
                        // its own 16 KiB frame and returns the result in RAX.
                        let run = unsafe {
                            std::mem::transmute::<*const u8, extern "C" fn() -> u64>(
                                buffer.as_ptr(),
                            )
                        };
                        executed += 1;
                        assert_eq!(
                        run(),
                        if shift { 17 << 5 } else { 12 },
                        "{form:?} folded={folded} wide={wide} shift={shift} result={result_reg:?} lhs={lhs_reg:?}@{lhs_slot} rhs={rhs_reg:?}@{rhs_slot}"
                    );
                    }
                }
            }
        }
    }
    eprintln!("selected operand native cases={executed}");
}

#[test]
fn selected_variable_shift_constrains_its_count_in_the_machine_model() {
    use crate::regalloc::{
        build_live_model, FixedConstraint, MachineModel, PhysReg, RegFile, RegSpec,
    };
    let graph = IrBuilder::new(2, 2)
        .build(&[0x1a, 0x1b, 0x60, 0xac], 4)
        .expect("add graph");
    let schedule = crate::ir_schedule::schedule(&graph);
    let result = graph
        .nodes
        .iter()
        .position(|n| n.op == Op::Add)
        .expect("sum") as NodeId;
    let lhs = graph.nodes[result as usize].inputs[0];
    let rhs = graph.nodes[result as usize].inputs[1];
    let mut plan = build_mir_plan(&graph, &schedule).expect("machine plan");
    let (block, tile) = plan.tile_of[result as usize].expect("sum tile");
    plan.blocks[block as usize].tiles[tile as usize] = crate::x64::isel::Tile::for_test(
        result,
        vec![result],
        vec![MInst::AluRR {
            op: crate::x64::isel::Op::Shl,
            ty: Ty::I32,
            dst: result,
            lhs,
            rhs,
        }],
        Rule::AluReg,
    );
    let live = build_live_model(&graph, &schedule);
    let mut model = MachineModel::for_graph(
        &graph,
        &schedule,
        &live,
        RegFile::from_specs([
            RegSpec {
                reg: PhysReg::gp(RCX),
                caller_saved: true,
            },
            RegSpec {
                reg: PhysReg::gp(3),
                caller_saved: false,
            },
        ]),
    );
    plan.add_allocated_requirements(&live, &mut model)
        .expect("selected constraints");
    let pos = live.pos_of[result as usize].expect("sum position");
    assert!(model.fixed.contains(&FixedConstraint {
        node: rhs,
        pos,
        reg: PhysReg::gp(RCX)
    }));
    assert!(model
        .clobbers
        .iter()
        .any(|(at, regs)| *at == pos && regs.contains(&PhysReg::gp(RCX))));
}

#[test]
fn selected_move_and_setcc_are_accepted_in_machine_model() {
    use crate::regalloc::{
        build_live_model, MachineModel, PhysReg, RegFile, RegSpec,
    };
    let graph = IrBuilder::new(2, 2)
        .build(&[0x1a, 0x1b, 0x60, 0xac], 4)
        .expect("add graph");
    let schedule = crate::ir_schedule::schedule(&graph);
    let result = graph
        .nodes
        .iter()
        .position(|n| n.op == Op::Add)
        .expect("sum") as NodeId;
    let lhs = graph.nodes[result as usize].inputs[0];
    let mut plan = build_mir_plan(&graph, &schedule).expect("machine plan");
    let (block, tile) = plan.tile_of[result as usize].expect("sum tile");
    plan.blocks[block as usize].tiles[tile as usize] = crate::x64::isel::Tile::for_test(
        result,
        vec![result],
        vec![
            MInst::Move {
                dst: result,
                ty: Ty::I32,
                src: lhs,
            },
            MInst::SetCc {
                dst: result,
                cc: crate::ir::CmpOp::Eq,
            },
        ],
        Rule::Generic,
    );
    let live = build_live_model(&graph, &schedule);
    let mut model = MachineModel::for_graph(
        &graph,
        &schedule,
        &live,
        RegFile::from_specs([
            RegSpec {
                reg: PhysReg::gp(3),
                caller_saved: false,
            },
        ]),
    );
    plan.add_allocated_requirements(&live, &mut model)
        .expect("move and setcc accepted");
    let pos = live.pos_of[result as usize].expect("sum position");
    assert!(model.read_before_write.iter().any(|w| w.result == result && w.pos == pos && w.inputs == vec![lhs]));
}

/// Execute the selected comparison, rather than the graph's deliberately
/// different equality, with stack and resident operands and both JVM widths.
#[test]
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn selected_materialized_comparisons_use_the_plan_and_locations() {
    use crate::x64::isel::{ImmForm, MInst, Rule, Ty};
    for kind in 0..3 {
        let wide = kind != 0;
        let mut builder = if kind == 1 { IrBuilder::new(2, 4) } else { IrBuilder::new(2, 2) };
        let code: &[u8] = if kind == 1 { &[0x1e, 0x20, 0x94, 0xac] }
                          else { &[0x1a, 0x1b, 0x60, 0xac] };
        builder.set_param_types(if kind == 1 { &[IrType::Long, IrType::Long] }
                                else { &[IrType::Int, IrType::Int] });
        let mut graph = builder.build(code, code.len()).unwrap();
        let result = graph.nodes.iter().position(|n| n.op == if kind == 1 { Op::LCmp } else { Op::Add }).unwrap() as NodeId;
        graph.nodes[result as usize].op = Op::Cmp(CmpOp::Eq);
        let a = graph.nodes[result as usize].inputs[0];
        let b = graph.nodes[result as usize].inputs[1];
        if kind == 2 {
            graph.nodes[a as usize].ty = IrType::Ref;
            graph.nodes[b as usize].ty = IrType::Ref;
        }
        let graph = Box::leak(Box::new(graph));
        let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
        let slots = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        for lhs_location in [None, Some(3u8), Some(12u8)] {
            let resident = lhs_location.is_some();
            let lhs_reg = lhs_location.unwrap_or(3);
            let rhs_reg = if lhs_reg == 3 { 12 } else { 3 };
            for form in 0..3 {
                for cc in [CmpOp::Eq, CmpOp::Ne, CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
                    let ty = if wide { Ty::I64 } else { Ty::I32 };
                    let selected = match form {
                        0 => MInst::CmpRR { ty, lhs: a, rhs: b },
                        1 => MInst::CmpRI { ty, lhs: a, imm: -1, form: ImmForm::Imm8 },
                        _ => MInst::TestRR { ty, reg: a },
                    };
                    let mut machine = build_mir_plan(graph, schedule).unwrap();
                    let (block, tile) = machine.tile_of[result as usize].unwrap();
                    machine.blocks[block as usize].tiles[tile as usize] =
                        crate::x64::isel::Tile::for_test(result, vec![result],
                            vec![selected, MInst::SetCc { dst: result, cc }], Rule::CmpSetCc);
                    let mut lo = super::tests::lowerer_over(graph, schedule, slots, 8192);
                    lo.set_mir(machine, MirMode::Off);
                    lo.reg_alloc_on = true;
                    lo.gp_reg_of = vec![None; graph.nodes.len()];
                    lo.gp_reg_live = vec![false; graph.nodes.len()];
                    if resident {
                        lo.gp_reg_of[a as usize] = Some(lhs_reg);
                        lo.gp_reg_of[b as usize] = Some(rhs_reg);
                        // Exercise a destructive result alias, as well as both
                        // low and extended byte-register encodings.
                        lo.gp_reg_of[result as usize] = Some(lhs_reg);
                        lo.publish_at_def_on = true;
                        lo.mark_gp_reg_live(a);
                        lo.mark_gp_reg_live(b);
                    }
                    lo.cur_def = Some(result);
                    let a_slot = lo.alloc_slot(a);
                    let b_slot = lo.alloc_slot(b);
                    let result_slot = lo.alloc_slot(result);
                    // Save RBP/RBX/R12 and reserve ABI shadow space.
                    lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x48, 0x83, 0xec, 32]);
                    lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
                    if resident {
                        lo.load_reg_from_frame(lhs_reg, a_slot);
                        lo.load_reg_from_frame(rhs_reg, b_slot);
                    }
                    let compare_start = lo.buf.pos();
                    assert!(lo.emit_selected_materialized_cmp(result, result_slot));
                    if resident && form != 0 {
                        let expected = if form == 1 {
                            let (bytes, len) = cmp_reg_imm_bytes(lhs_reg, -1, wide);
                            bytes[..len].to_vec()
                        } else {
                            crate::x64::isel::select(&Req::new(
                                crate::x64::isel::Op::Test, ty,
                                Operand::Gpr(lhs_reg), Operand::Gpr(lhs_reg),
                            )).unwrap().encoded.bytes
                        };
                        assert!(lo.buf.as_slice()[compare_start..].starts_with(&expected),
                            "resident operand must be compared directly");
                    }
                    assert_eq!(lo.mir_tiles, 1);
                    if resident {
                        assert!(lo.cur_def_published);
                        assert_eq!(lo.resident_gpr(result), Some(lhs_reg));
                    }
                    lo.load_reg_from_frame(RAX, result_slot);
                    lo.buf
                        .emit(&[0x48, 0x83, 0xc4, 32, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
                    assert!(lo.take_latched_bailout().is_none());
                    lo.buf.finalize();
                    // SAFETY: the finalized fixture emits this C ABI and its executable buffer remains live.
                    let run: unsafe extern "C" fn(*mut u64) -> u64 =
                        unsafe { std::mem::transmute(lo.buf.as_ptr()) };
                    let cases = [
                        (0u64, 0u64),
                        (1, 2),
                        (u64::MAX, 1),
                        (0x1_0000_0000, 0),
                        (i64::MIN as u64, i64::MAX as u64),
                    ];
                    for (av, bv) in cases {
                        let mut frame =
                            vec![0_u64; a_slot.max(b_slot).max(result_slot) as usize / 8 + 16];
                        let end = frame.len();
                        frame[end - a_slot as usize / 8] = av;
                        frame[end - b_slot as usize / 8] = bv;
                        let lhs = if wide {
                            av as i64
                        } else {
                            i64::from(av as i32)
                        };
                        let rhs = if form == 1 {
                            -1
                        } else if form == 2 {
                            0
                        } else if wide {
                            bv as i64
                        } else {
                            i64::from(bv as i32)
                        };
                        let want = match cc {
                            CmpOp::Eq => lhs == rhs,
                            CmpOp::Ne => lhs != rhs,
                            CmpOp::Lt => lhs < rhs,
                            CmpOp::Le => lhs <= rhs,
                            CmpOp::Gt => lhs > rhs,
                            CmpOp::Ge => lhs >= rhs,
                        };
                        // SAFETY: the generated leaf reads only the supplied frame
                        // and preserves the callee-saved registers it uses.
                        assert_eq!(unsafe { run(frame.as_mut_ptr().add(end)) }, u64::from(want),
                            "kind={kind} resident={resident} form={form} cc={cc:?} a={av:x} b={bv:x}");                    }                }            }        }    }}

#[test]
#[cfg(target_arch = "x86_64")]
fn poll_reference_reload_requires_every_home_in_the_published_map() {
    let mut builder = IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Int]);
    let graph = Box::leak(Box::new(builder.build(&[0x1a, 0xac], 2).unwrap()));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let slots = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let token = SafepointMapToken { index: 0, id: 1, id_stored: true, last_return: 0 };
    let map = |homes| crate::OopMapEntry {
        native_pc_offset: 0,
        bytecode_pc: 1,
        frame_slot_offsets: homes,
        moving_young_coverage_complete: true,
        reg_oop_mask: Some(0),
        live_frame_hi: 0,
        local_oop_mask: None,
        num_locals: 0,
        inline_local_scopes: Vec::new(),
        non_oop_stack_slots: Vec::new(),
        stack_marks_exact: true,
        shadow_pushed: 0,
        spill_written: None,
    };
    for homes in [None, Some(vec![16]), Some(vec![16, 24])] {
        let mut lo = super::tests::lowerer_over(graph, schedule, slots, 4096);
        if let Some(homes) = homes.as_ref() {
            lo.oop_maps.push(map(homes.clone()));
        }
        let token = homes.as_ref().map(|_| &token);
        lo.emit_poll_ref_phi_reloads(&[(3, 16), (12, 24)], token);
        if homes.as_ref().is_some_and(|homes| homes.len() == 2) {
            assert!(lo.take_latched_bailout().is_none());
            // MOV RBX,[RBP-16]; MOV R12,[RBP-24]. Verify the REX.R bit
            // independently: the frame-only encoder declines high registers.
            assert_eq!(lo.buf.as_slice(), &[0x48, 0x8b, 0x5d, 0xf0,
                                           0x4c, 0x8b, 0x65, 0xe8]);
        } else {
            assert!(lo.take_latched_bailout().is_some());
            assert_eq!(lo.buf.pos(), 0, "refusal must emit no partial reload");
        }
    }
}

#[test]
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn compact_reference_load_can_write_an_allocated_register() {
    let mut builder = IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    let graph = Box::leak(Box::new(builder.build(&[0x2a, 0xb0], 2).unwrap()));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let slots = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    for receiver in [3u8, 12] {
        for result in [3u8, 12, 13] {
            let mut lo = super::tests::lowerer_over(graph, schedule, slots, 4096);
            lo.buf.emit(&[0x53, 0x41, 0x54, 0x41, 0x55]); // save RBX,R12,R13
            lo.emit_mov_reg_reg64(receiver, ENTRY_ABI_REGS[0]);
            let mut slow = Vec::new();
            assert!(lo.emit_compact_reference_field_load(receiver, 8, None, &mut slow, result).is_empty());
            assert!(slow.is_empty());
            lo.emit_mov_reg_reg64(RAX, result);
            lo.buf.emit(&[0x41, 0x5d, 0x41, 0x5c, 0x5b, 0xc3]);
            lo.buf.finalize();
            // SAFETY: this leaf reads one live two-word array and preserves its
            // nonvolatile registers before returning the second word in RAX.
            let run: unsafe extern "C" fn(*const u64) -> u64 =
                unsafe { std::mem::transmute(lo.buf.as_ptr()) };
            for value in [0, 1, u64::MAX, 0x1234_5678_9abc_def0] {
                let object = [42, value];
                assert_eq!(
                    // SAFETY: the live frame and object buffers satisfy this non-collecting leaf's C ABI.
                    unsafe { run(object.as_ptr()) },
                    value,
                    "receiver={receiver} result={result}"
                );
            }
        }
    }
}

#[test]
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn legacy_reference_load_writes_assigned_register_and_checks_tag() {
    let mut builder = IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    let graph = Box::leak(Box::new(builder.build(&[0x2a, 0xb0], 2).unwrap()));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let slots = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    for receiver in [RAX, 3, 12] {
        for result in [RAX, 3, 12] {
            let mut lo = super::tests::lowerer_over(graph, schedule, slots, 4096);
            lo.buf.emit(&[0x53, 0x41, 0x54]); // save RBX and R12
            lo.emit_mov_reg_reg64(receiver, ENTRY_ABI_REGS[0]);
            let mut slow = Vec::new();
            lo.emit_getfield_legacy_read(true, b'L', 0, &mut slow, result, receiver);
            lo.emit_mov_reg_reg64(RAX, result);
            let done = lo.emit_jmp_rel32();
            for patch in slow { lo.patch_rel32_to_here(patch); }
            lo.emit_mov_reg_imm64(RAX, 0x1234);
            lo.patch_rel32_to_here(done);
            lo.buf.emit(&[0x41, 0x5c, 0x5b, 0xc3]);
            lo.buf.finalize();
            // SAFETY: the code reads a live cell, restores the saved registers,
            // and returns either the payload or the test slow-path sentinel.
            let run: unsafe extern "C" fn(*const u8) -> u64 =
                unsafe { std::mem::transmute(lo.buf.as_ptr()) };
            let mut cell = [0u8; 32];
            let tag = cratonvm_types::FIELD_CELL_TAG_OFFSET as usize;
            let payload = FIELD_CELL_PAYLOAD64_OFFSET as usize;
            cell[payload..payload + 8].copy_from_slice(&0x1234_5678_9abc_def0u64.to_le_bytes());
            cell[tag..tag + 4]
                .copy_from_slice(&(cratonvm_types::FIELD_CELL_TAG_OBJECT as u32).to_le_bytes());
            assert_eq!(
                // SAFETY: the live frame and object buffers satisfy this non-collecting leaf's C ABI.
                unsafe { run(cell.as_ptr()) },
                0x1234_5678_9abc_def0,
                "receiver={receiver} result={result}"
            );
            cell[tag..tag + 4]
                .copy_from_slice(&(cratonvm_types::FIELD_CELL_TAG_OBJECT as u32 ^ 1).to_le_bytes());
            assert_eq!(
                // SAFETY: the live frame and object buffers satisfy this non-collecting leaf's C ABI.
                unsafe { run(cell.as_ptr()) },
                0x1234,
                "invalid tag receiver={receiver} result={result}"
            );
        }
    }
}

#[test]
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn plain_reference_array_load_uses_allocated_address_and_result() {
    let mut builder = IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    let graph = Box::leak(Box::new(builder.build(&[0x2a, 0xb0], 2).unwrap()));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let slots = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    for (base, index, result) in [(3, 12, 13), (12, 3, 13), (13, 12, 3)] {
        let mut lo = super::tests::lowerer_over(graph, schedule, slots, 4096);
        lo.buf.emit(&[0x53, 0x41, 0x54, 0x41, 0x55]);
        lo.emit_mov_reg_reg64(base, ENTRY_ABI_REGS[0]);
        lo.emit_mov_reg_reg64(index, ENTRY_ABI_REGS[1]);
        lo.emit_plain_ref_array_elem_load_to(result, base, index);
        lo.emit_mov_reg_reg64(RAX, result);
        lo.buf.emit(&[0x41, 0x5d, 0x41, 0x5c, 0x5b, 0xc3]);
        lo.buf.finalize();
        // SAFETY: the fixture supplies a live array-shaped buffer and an
        // in-range index; the leaf restores the nonvolatile registers it uses.
        let run: unsafe extern "C" fn(*const u8, u64) -> u64 =
            unsafe { std::mem::transmute(lo.buf.as_ptr()) };
        let mut array = [0u8; 80];
        let offset = ARRAY_DATA_DISP8 as usize;
        for i in 0..4 {
            let value = 0x1234_5678_0000_0000u64 + i as u64;
            array[offset + 8 * i..offset + 8 * i + 8].copy_from_slice(&value.to_le_bytes());
            // SAFETY: the live frame and object buffers satisfy this non-collecting leaf's C ABI.
            assert_eq!(unsafe { run(array.as_ptr(), i as u64) }, value);
        }
    }
}
