// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// The native bytes are inspected without entering the synthetic helper.
unsafe extern "C" fn fixture_poll(_flag: usize, _body_id: u32) -> i64 {
    0
}

fn check_poll_returns(outlined: bool) {
    // int count(int n) { int i = 0; while (i < n) i++; return i; }
    let bytecode = [
        0x03, 0x3c, 0x1b, 0x1a, 0xa2, 0, 9, 0x84, 1, 1, 0xa7, 0xff, 0xf8, 0x1b, 0xac,
    ];
    let mut builder = crate::ir::IrBuilder::new(2, 2);
    builder.set_param_types(&[IrType::Int]);
    let graph = Box::leak(Box::new(
        builder
            .build(&bytecode, bytecode.len())
            .expect("count loop builds"),
    ));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let (latch, header_pc) = (0..schedule.blocks.len())
        .find_map(|block| back_edge_poll_header(graph, schedule, block).map(|(_, pc)| (block, pc)))
        .expect("fixture has a real back edge");
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 8192);
    let flag = 1_u8;
    lo.safepoint_flag_addr = (&flag as *const u8) as usize;
    lo.safepoint_slow_path = fixture_poll as *const () as usize;
    lo.sp_id_slot_off = 24;
    lo.reloc_emit_on = true;
    // A prior publication does not give this poll permission to reuse its map.
    lo.emit_safepoint_map(0);
    lo.buf.begin_call_census();
    let outline = if outlined { "1" } else { "0" };
    cratonvm_types::flags::with_thread_overrides(
        &[
            ("CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE", Some(outline)),
            ("CRATONVM_JIT_IR_BACK_EDGE_POLL_OUTLINE", Some(outline)),
            ("CRATONVM_JIT_IR_PAGE_POLL", Some("0")),
        ],
        || {
            lo.cur_bci = 0;
            lo.emit_safepoint_poll(None, None);
            lo.cur_bci = header_pc;
            lo.emit_safepoint_poll(Some(latch), None);
            assert_eq!(lo.outlined_polls.len(), if outlined { 2 } else { 0 });
            lo.cur_bci = bytecode.len() - 1;
            lo.emit_outlined_polls();

            lo.reloc_emit_on = false;
            lo.emit_safepoint_poll(None, None);
            lo.emit_outlined_polls();
        },
    );
    assert!(lo.take_latched_bailout().is_none());
    let returns = lo.buf.take_call_census();
    assert_eq!(returns.len(), 3, "two mapped polls and an unmapped control");
    let maps = &lo.oop_maps;
    assert_eq!(maps.len(), 3, "one earlier map and two mapped polls");
    assert_eq!(maps[0].native_pc_offset, 0, "the older map is unrelated");
    let code = lo.buf.as_slice();
    let mut previous_return = 0;
    for (map, ret, pc) in [(&maps[1], returns[0], 0), (&maps[2], returns[1], header_pc)] {
        assert_eq!(
            map.native_pc_offset, ret,
            "map must name the exact poll return"
        );
        assert!(lo.sp_id_bcis.contains(&(map.bytecode_pc, pc as u32)));
        let mut stamp = vec![0x48, 0xc7, 0x85];
        stamp.extend((-lo.sp_id_slot_off).to_le_bytes());
        stamp.extend(map.bytecode_pc.to_le_bytes());
        let ret = ret as usize;
        assert_eq!(&code[ret - 2..ret], &[0xff, 0xd0]);
        assert!(
            code[previous_return..ret]
                .windows(stamp.len())
                .any(|w| w == stamp)
        );
        previous_return = ret;
    }
    assert_ne!(maps[1].bytecode_pc, maps[2].bytecode_pc);
    assert!(!maps.iter().any(|m| m.native_pc_offset == returns[2]));
}

#[test]
fn inline_polls_anchor_their_own_maps() {
    check_poll_returns(false);
}

#[test]
fn outlined_polls_anchor_their_own_maps() {
    check_poll_returns(true);
}

fn map_fixture(graph: Graph) -> Lowerer<'static> {
    let graph = Box::leak(Box::new(graph));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 8192);
    lo.context_slot_off = 32;
    lo.sp_id_slot_off = 24;
    lo.reloc_emit_on = true;
    lo
}

#[test]
fn allocation_map_uses_the_stub_return_before_frame_republication() {
    let code = [0x04, 0xbc, 0x0a, 0xb0];
    let graph = crate::ir::IrBuilder::new(0, 1)
        .build(&code, code.len())
        .unwrap();
    let mut lo = map_fixture(graph);
    let target = fixture_poll as *const () as usize;
    lo.newarray = target;
    lo.frame_record = target;
    let id = lo
        .graph
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::NewArray { .. }))
        .unwrap() as NodeId;
    let length = lo.graph.nodes[id as usize].inputs[2];
    lo.lower_data_node(length);
    lo.buf.begin_call_census();
    cratonvm_types::flags::with_thread_overrides(
        &[
            ("CRATONVM_JIT_IR_INLINE_TLAB", Some("0")),
            ("CRATONVM_JIT_IR_NEWARRAY_MAP_SINK", Some("0")),
        ],
        || lo.lower_data_node(id),
    );
    let returns = lo.buf.take_call_census();
    let ret = returns[0];
    assert!(lo.buf.pos() > ret as usize);
    assert!(lo.take_latched_bailout().is_none());
    assert_eq!(lo.oop_maps.len(), 1);
    assert_eq!(
        lo.oop_maps[0].native_pc_offset, ret,
        "allocation map names the actual stub return"
    );
    assert!(lo.sp_id_bcis.contains(&(lo.oop_maps[0].bytecode_pc, 1)));
}

#[test]
fn implicit_pad_maps_cover_signal_and_lookup_returns_under_one_id() {
    for kind in [
        ImplicitKind::Npe,
        ImplicitKind::Aioobe,
        ImplicitKind::Arithmetic,
    ] {
        let code = [0x2a, 0xbe, 0xac];
        let mut builder = crate::ir::IrBuilder::new(1, 1);
        builder.set_param_types(&[IrType::Ref]);
        let mut lo = map_fixture(builder.build(&code, code.len()).unwrap());
        let id = lo
            .graph
            .nodes
            .iter()
            .position(|n| n.op == Op::ArrayLength)
            .unwrap() as NodeId;
        let target = fixture_poll as *const () as usize;
        lo.ref_param_homes = vec![8];
        lo.cur_bci = 1;
        lo.cur_node_pc = Some(1);
        lo.set_throw_bci = target;
        lo.npe_with_action = target;
        lo.throw_aioobe = target;
        lo.throw_arithmetic = target;
        lo.local_handler_lookup = target;
        let plan = ImplicitThrowPlan {
            site_ptr: 0,
            targets: vec![],
            result_home: None,
        };
        lo.buf.begin_call_census();
        lo.emit_implicit_throw_pad(kind, 1, id, &plan);
        let returns = lo.buf.take_call_census();
        let leaves = usize::from(kind != ImplicitKind::Aioobe);
        assert_eq!(returns.len(), 2 + leaves);
        assert!(lo.take_latched_bailout().is_none());
        let maps = &lo.oop_maps;
        assert_eq!(
            maps.iter().map(|m| m.native_pc_offset).collect::<Vec<_>>(),
            &returns[leaves..],
            "{kind:?}: both non-leaf calls need exact returns"
        );
        let id = maps[0].bytecode_pc;
        assert_eq!(lo.sp_id_bcis, [(id, 1)]);
        assert!(
            maps.iter()
                .all(|m| m.bytecode_pc == id && m.frame_slot_offsets == [8])
        );
        let mut stamp = vec![0x48, 0xc7, 0x85];
        stamp.extend((-lo.sp_id_slot_off).to_le_bytes());
        stamp.extend(id.to_le_bytes());
        assert_eq!(
            lo.buf.as_slice()[..returns[0] as usize]
                .windows(stamp.len())
                .filter(|w| *w == stamp)
                .count(),
            1
        );
    }
}

#[test]
fn monitor_stubs_anchor_the_acquire_and_release_maps() {
    let code = [0x2a, 0xc2, 0x2a, 0xc3, 0xb1];
    let mut builder = crate::ir::IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    let mut lo = map_fixture(
        builder
            .build(&code, code.len())
            .expect("balanced monitor graph"),
    );
    let target = fixture_poll as *const () as usize;
    lo.monitor_enter = target;
    lo.monitor_exit = target;
    lo.frame_record = target;
    let ids: Vec<_> = lo
        .graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::MonitorEnter | Op::MonitorExit))
        .map(|(id, _)| id as NodeId)
        .collect();
    assert_eq!(ids.len(), 2);
    for id in ids {
        let receiver = lo.graph.nodes[id as usize].inputs[2];
        lo.alloc_slot(receiver);
        let before = lo.oop_maps.len();
        lo.buf.begin_call_census();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MONITOR_FASTPATH", Some("0"))],
            || lo.lower_data_node(id),
        );
        let returns = lo.buf.take_call_census();
        assert!(lo.take_latched_bailout().is_none());
        assert_eq!(lo.oop_maps.len(), before + 1);
        assert_eq!(
            lo.oop_maps[before].native_pc_offset, returns[0],
            "monitor map names its helper's return before frame republication"
        );
    }
    assert_ne!(lo.oop_maps[0].bytecode_pc, lo.oop_maps[1].bytecode_pc);
}

#[test]
fn constant_resolution_maps_anchor_the_shared_stub_return() {
    for is_string in [false, true] {
        let code = [0x12, 0x09, 0xb0];
        let mut builder = crate::ir::IrBuilder::new(0, 1);
        if is_string {
            builder.set_ldc_string_info(HashMap::from([(0, (7, 9))]));
        } else {
            builder.set_ldc_class_info(HashMap::from([(0, (7, 9))]));
        }
        let mut lo = map_fixture(
            builder
                .build(&code, code.len())
                .expect("reference ldc graph"),
        );
        let id = lo
            .graph
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::ConstString { .. } | Op::ConstClass { .. }))
            .unwrap() as NodeId;
        let target = fixture_poll as *const () as usize;
        lo.ldc_string_cp = target;
        lo.ldc_class_cp = target;
        lo.frame_record = target;
        lo.buf.begin_call_census();
        lo.lower_data_node(id);
        let returns = lo.buf.take_call_census();
        assert!(lo.take_latched_bailout().is_none());
        assert_eq!(lo.oop_maps.len(), 1);
        assert_eq!(
            lo.oop_maps[0].native_pc_offset, returns[0],
            "constant resolution map names the helper's exact return"
        );
        assert!(lo.sp_id_bcis.contains(&(lo.oop_maps[0].bytecode_pc, 0)));
    }
}

#[test]
fn post_monitor_twin_does_not_inherit_the_acquires_native_return() {
    let mut builder = crate::ir::IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    let mut lo = map_fixture(builder.build(&[0x2a, 0xb0], 2).unwrap());
    let mut map = lo.emit_safepoint_map(0);
    lo.buf.emit(&[0xff, 0xd0]);
    let ret = lo.buf.pos();
    lo.anchor_safepoint_return(map.as_mut(), ret);
    let id = lo.oop_maps[0].bytecode_pc;
    let twin = lo
        .record_safepoint_twin(id)
        .expect("the map can be twinned");
    assert_ne!(id, twin);
    assert_eq!(lo.oop_maps[0].native_pc_offset, ret as u32);
    assert_eq!(lo.oop_maps[1].native_pc_offset, 0);
}

#[test]
fn exceptional_monitor_release_anchors_its_map_without_the_failed_result() {
    let code = [0x04, 0xbc, 0x0a, 0xb0];
    let graph = crate::ir::IrBuilder::new(0, 1)
        .build(&code, code.len())
        .unwrap();
    let mut lo = map_fixture(graph);
    let call = lo
        .graph
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::NewArray { .. }))
        .unwrap() as NodeId;
    let result_home = lo.alloc_slot(call);
    assert!(lo.defined_nodes[call as usize]);
    lo.monitor_exit = fixture_poll as *const () as usize;
    lo.frame_record = fixture_poll as *const () as usize;
    lo.buf.begin_call_census();
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_MONITOR_FASTPATH", Some("0"))],
        || lo.emit_sync_exit_then_throw(8, call),
    );
    let returns = lo.buf.take_call_census();
    assert!(lo.take_latched_bailout().is_none());
    assert_eq!(lo.oop_maps.len(), 1);
    assert_eq!(lo.oop_maps[0].native_pc_offset, returns[0]);
    assert!(
        !lo.oop_maps[0]
            .frame_slot_offsets
            .contains(&(result_home as i16))
    );
    assert!(lo.defined_nodes[call as usize]);
}

#[test]
fn ordinary_call_maps_bind_the_dispatch_or_direct_callee_return() {
    for direct in [false, true] {
        let info = Box::leak(Box::new(JitInvokeInfo {
            class_name: "CallMapProbe",
            method_name: "callee",
            descriptor: "(I)I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        let ptr = info as *const JitInvokeInfo as usize;
        let code = [0x1a, 0xb8, 0, 1, 0xac];
        let mut builder = crate::ir::IrBuilder::new(1, 1);
        builder.set_param_types(&[IrType::Int]);
        builder.set_invoke_info(HashMap::from([(1, (ptr, 1, b'I'))]));
        let mut lo = map_fixture(builder.build(&code, code.len()).expect("static call graph"));
        let id = lo
            .graph
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Call { .. }))
            .unwrap() as NodeId;
        let input = lo.graph.nodes[id as usize].inputs[2];
        lo.lower_data_node(input);
        let target = fixture_poll as *const () as usize;
        lo.invoke_dispatch = target;
        lo.frame_record = target;
        if direct {
            lo.direct_calls = Box::leak(Box::new(HashMap::from([(1, (target, true))])));
        }
        lo.buf.begin_call_census();
        lo.lower_data_node(id);
        let returns = lo.buf.take_call_census();
        assert!(lo.take_latched_bailout().is_none());
        assert_eq!(lo.oop_maps.len(), 1);
        assert_eq!(
            lo.oop_maps[0].native_pc_offset, returns[0],
            "ordinary call must anchor its actual callee return: direct={direct}"
        );
        assert!(lo.sp_id_bcis.contains(&(lo.oop_maps[0].bytecode_pc, 1)));
    }
}

#[test]
fn alternate_call_routes_keep_one_publication_at_each_callee_return() {
    for (route, arity) in [
        ("cycle", 1),
        ("cycle", 8),
        ("ic", 1),
        ("ic", 8),
        ("ffm", 2),
        ("segment", 2),
    ] {
        let mut builder = crate::ir::IrBuilder::new(arity, arity);
        let mut types = vec![IrType::Int; arity];
        types[0] = IrType::Ref;
        builder.set_param_types(&types);
        let mut lo = map_fixture(builder.build(&[0x2a, 0xb0], 2).unwrap());
        let mut inputs = vec![0, 0];
        for index in 0..arity {
            let id = lo
                .graph
                .nodes
                .iter()
                .position(|n| n.op == Op::Param(index as u16))
                .expect("parameter") as NodeId;
            lo.lower_data_node(id);
            inputs.push(id);
        }
        let target = fixture_poll as *const () as usize;
        let info = JitInvokeInfo {
            class_name: "CallMapProbe",
            method_name: "callee",
            descriptor: "()I",
            num_jit_args: arity,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: 0,
            owner_class_id: 0,
        };
        let ptr = &info as *const JitInvokeInfo as usize;
        let slot = lo.slot_of(inputs[2]);
        lo.invoke_dispatch = target;
        lo.invoke_virtual_mic = target;
        lo.service_callee_deopt = target + 64;
        lo.self_call_stack_guard = target + 96;
        lo.frame_record = target + 32;
        lo.args_stage_top_off = 256;
        let mut map = lo.emit_safepoint_map(0);
        let published = lo.oop_maps[0].clone();
        lo.buf.begin_call_census();
        match route {
            "cycle" => {
                lo.emit_cycle_edge_cell_call(&inputs, slot, arity, 16, ptr, IrType::Int, &mut map)
            }
            "ic" => {
                let mic = crate::JitMICSlot::default();
                let pic = crate::JitPICSlot::new();
                lo.emit_inline_cache_call(
                    &inputs,
                    slot,
                    arity,
                    &mic as *const _ as usize,
                    &pic as *const _ as usize,
                    ptr,
                    IrType::Int,
                    &mut map,
                );
            }
            _ => {
                let row = FfmConstRow {
                    fn_addr: target as u64,
                    arity: 1,
                    params: [0; 6],
                    ret: 0,
                    flags: 0,
                    guard_words: [0; 2],
                    guard_values: [0; 2],
                    call_helper: target as u64,
                    desc_ptr: 0,
                    desc_len: 0,
                    seg_call_helper: target as u64,
                    seg_param: 0,
                };
                let row_addr = &row as *const _ as usize;
                if route == "ffm" {
                    lo.emit_ffm_lazy_call(
                        &inputs,
                        slot,
                        arity,
                        row_addr,
                        &row,
                        ptr,
                        IrType::Int,
                        &mut map,
                    );
                } else {
                    let lay = FfmSegInlineLayout {
                        segment_class: 1,
                        min: 16,
                        length: 24,
                        scope: 32,
                        confined_class: 2,
                        state: 16,
                        owner: 24,
                        acquire: 32,
                        global_class: 3,
                        thread_off: 16,
                    };
                    lo.emit_ffm_seg_call(
                        &inputs,
                        slot,
                        arity,
                        row_addr,
                        &row,
                        ptr,
                        IrType::Int,
                        lay,
                        &mut map,
                    );
                }
            }
        }
        let returns = lo.buf.take_call_census();
        assert!(lo.take_latched_bailout().is_none(), "{route}/{arity}");
        let bytes = lo.buf.as_slice();
        let expected: Vec<_> = returns
            .iter()
            .copied()
            .filter(|&ret| {
                let at = ret as usize;
                (at >= 3 && bytes[at - 3..at] == [0x41, 0xff, 0xd3])
                    || (at >= 12
                        && bytes[at - 12..at - 10] == [0x48, 0xb8]
                        && [target, target + 64, target + 96]
                            .iter()
                            .any(|&callee| bytes[at - 10..at - 2] == (callee as u64).to_le_bytes())
                        && bytes[at - 2..at] == [0xff, 0xd0])
            })
            .collect();
        assert!(expected.len() >= 2, "{route}/{arity}: {expected:?}");
        for ret in expected {
            assert!(
                lo.oop_maps.iter().any(|m| m.native_pc_offset == ret),
                "{route}/{arity}: unanchored callee {ret}"
            );
        }
        for map in &lo.oop_maps {
            assert!(returns.contains(&map.native_pc_offset), "{route}/{arity}");
            assert_eq!(map.bytecode_pc, published.bytecode_pc);
            assert_eq!(map.frame_slot_offsets, published.frame_slot_offsets);
            assert_eq!(map.reg_oop_mask, published.reg_oop_mask);
        }
    }
}

#[test]
fn recursive_maps_cover_caller_guard_and_service_with_lazy_ids() {
    for refs in [false, true] {
        for lazy in [false, true] {
            for cold in [false, true] {
                let arity = if refs { 2 } else { 1 };
                let info = Box::leak(Box::new(JitInvokeInfo {
                    class_name: "CallMapProbe",
                    method_name: "recursive",
                    descriptor: if refs {
                        "(Ljava/lang/Object;I)Ljava/lang/Object;"
                    } else {
                        "(I)I"
                    },
                    num_jit_args: arity,
                    return_type: if refs { b'L' } else { b'I' },
                    invoke_kind: 4,
                    declaring_class_id: 0,
                    owner_class_id: 0,
                }));
                let ptr = info as *const JitInvokeInfo as usize;
                let code: &[u8] = if refs {
                    &[0x2a, 0x1b, 0xb8, 0, 1, 0xb0]
                } else {
                    &[0x1a, 0xb8, 0, 1, 0xac]
                };
                let mut builder = crate::ir::IrBuilder::new(arity, arity);
                builder.set_param_types(if refs {
                    &[IrType::Ref, IrType::Int]
                } else {
                    &[IrType::Int]
                });
                builder.set_invoke_info(HashMap::from([(
                    arity,
                    (ptr, arity, if refs { b'L' } else { b'I' }),
                )]));
                let mut lo = map_fixture(builder.build(code, code.len()).expect("recursive graph"));
                let call = lo
                    .graph
                    .nodes
                    .iter()
                    .position(|n| matches!(n.op, Op::Call { .. }))
                    .unwrap() as NodeId;
                let params = lo.graph.nodes[call as usize].inputs[2..].to_vec();
                for input in params {
                    lo.lower_data_node(input);
                }
                let target = fixture_poll as *const () as usize;
                lo.self_call_stack_guard = target;
                lo.service_callee_deopt = target + 64;
                lo.frame_record = target + 128;
                let on = |v| if v { "1" } else { "0" };
                cratonvm_types::flags::with_thread_overrides(
                    &[
                        ("CRATONVM_JIT_IR_SELF_CALL_LAZY_SP_ID", Some(on(lazy))),
                        ("CRATONVM_JIT_IR_SELF_CALL_COLD_TAILS", Some(on(cold))),
                    ],
                    || {
                        lo.buf.begin_call_census();
                        lo.lower_data_node(call);
                        let hot_end = lo.buf.pos();
                        let recursive_return = lo.self_call_patches[0] + 4;
                        assert_eq!(lo.buf.as_slice()[recursive_return - 5], 0xe8);
                        let id = lo.oop_maps[0].bytecode_pc;
                        let mut stamp = vec![0x48, 0xc7, 0x85];
                        stamp.extend((-lo.sp_id_slot_off).to_le_bytes());
                        stamp.extend(id.to_le_bytes());
                        if lazy && !refs && cold {
                            assert_eq!(lo.deferred_self_call_tails.len(), 2);
                            assert!(
                                !lo.buf.as_slice()[..hot_end]
                                    .windows(stamp.len())
                                    .any(|w| w == stamp),
                                "reference-free recursion must retain the hot ID-store elision"
                            );
                        }
                        lo.emit_deferred_self_call_tails();
                        assert!(lo.take_latched_bailout().is_none());
                        let returns = lo.buf.take_call_census();
                        let bytes = lo.buf.as_slice();
                        let mut expected = vec![recursive_return as u32];
                        expected.extend(returns.into_iter().filter(|&ret| {
                            let at = ret as usize;
                            at >= 12
                                && bytes[at - 12..at - 10] == [0x48, 0xb8]
                                && [target, target + 64].iter().any(|&callee| {
                                    bytes[at - 10..at - 2] == (callee as u64).to_le_bytes()
                                })
                                && bytes[at - 2..at] == [0xff, 0xd0]
                        }));
                        assert_eq!(expected.len(), 3);
                        assert_eq!(lo.oop_maps.len(), 3);
                        for ret in expected {
                            assert!(
                                lo.oop_maps
                                    .iter()
                                    .any(|m| m.bytecode_pc == id && m.native_pc_offset == ret),
                                "recursive return must carry its map: refs={refs} lazy={lazy} cold={cold} ret={ret}"
                            );
                        }
                        assert_eq!(
                            bytes.windows(stamp.len()).filter(|w| *w == stamp).count(),
                            if lazy && !refs { 2 } else { 1 }
                        );
                    },
                );
            }
        }
    }
}

#[test]
fn helper_return_rejects_an_unpublished_id() {
    let graph = crate::ir::IrBuilder::new(0, 0)
        .build(&[0x03, 0xac], 2)
        .unwrap();
    let mut lo = map_fixture(graph);
    lo.defer_next_sp_id_store = true;
    let mut map = lo.emit_safepoint_map(0);
    lo.buf.emit(&[0xff, 0xd0]);
    lo.anchor_safepoint_return(map.as_mut(), lo.buf.pos());
    assert!(lo.take_latched_bailout().is_some());
    assert_eq!(lo.oop_maps[0].native_pc_offset, 0);
}

#[test]
fn unpublished_recursive_caller_rejects_reference_roots() {
    let mut builder = crate::ir::IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    let mut lo = map_fixture(builder.build(&[0x2a, 0xb0], 2).unwrap());
    let mut map = lo.emit_safepoint_map(0);
    map.as_mut().unwrap().id_stored = false;
    lo.buf.emit(&[0xff, 0xd0]);
    lo.anchor_recursive_caller_return(map.as_mut(), lo.buf.pos());
    assert!(lo.take_latched_bailout().is_some());
    assert_eq!(lo.oop_maps[0].native_pc_offset, 0);
}

#[test]
fn delayed_id_mismatch_rejects_before_emitting_a_store() {
    let graph = crate::ir::IrBuilder::new(0, 0)
        .build(&[0x03, 0xac], 2)
        .unwrap();
    let mut lo = map_fixture(graph);
    lo.defer_next_sp_id_store = true;
    let mut map = lo.emit_safepoint_map(0);
    let wrong_id = map.as_ref().unwrap().id + 1;
    let before = lo.buf.pos();
    lo.emit_deferred_safepoint_id_store(Some(wrong_id), &mut map);
    assert!(lo.take_latched_bailout().is_some());
    assert_eq!(lo.buf.pos(), before);
    assert!(!map.unwrap().id_stored);
}

#[test]
fn type_and_static_helpers_bind_their_published_maps() {
    static NAME: &str = "example/Target";
    let target = fixture_poll as *const () as usize;
    let static_load = Op::LoadStatic {
        class_id: 3,
        field_index: 1,
        type_tag: b'L',
        is_volatile: true,
    };
    let static_store = Op::StoreStatic {
        class_id: 3,
        field_index: 1,
        type_tag: b'L',
        is_volatile: true,
    };
    for (kind, op, ty) in [
        (
            "cast",
            Op::CheckCast {
                name_ptr: NAME.as_ptr() as usize,
                name_len: NAME.len(),
            },
            IrType::Ref,
        ),
        (
            "instanceof",
            Op::InstanceOf {
                name_ptr: NAME.as_ptr() as usize,
                name_len: NAME.len(),
                seen_cell: 0,
                header_only: false,
            },
            IrType::Int,
        ),
        ("static-load", static_load, IrType::Ref),
        ("static-store", static_store, IrType::Memory),
    ] {
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let param = graph.add(Op::Param(0), IrType::Ref, vec![start], Some(0));
        let mut inputs = vec![ctrl, mem];
        if kind != "static-load" {
            inputs.push(param);
        }
        let id = graph.add(op, ty, inputs, Some(1));
        let result = if ty == IrType::Memory { param } else { id };
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, result], Some(4));
        let mut lo = map_fixture(graph);
        if kind != "static-load" {
            lo.lower_data_node(param);
        }
        lo.checkcast = target;
        lo.instanceof_check = target;
        lo.getstatic = target;
        lo.putstatic.fill(target);
        lo.frame_record = target + 64;
        lo.buf.begin_call_census();
        lo.lower_data_node(id);
        assert!(lo.take_latched_bailout().is_none(), "{kind}");
        let returns = lo.buf.take_call_census();
        assert_eq!(lo.oop_maps.len(), 1, "{kind}");
        assert_eq!(
            lo.oop_maps[0].native_pc_offset, returns[0],
            "{kind} must bind the helper return ahead of frame republication"
        );
    }
}

#[test]
fn array_store_returns_bind_shared_or_separate_publications() {
    let gates = [1u8, 1];
    let target = fixture_poll as *const () as usize;
    for gated in [false, true] {
        let code = [0x2a, 0x1b, 0x2c, 0x53, 0xb1];
        let mut builder = crate::ir::IrBuilder::new(3, 3);
        builder.set_param_types(&[IrType::Ref, IrType::Int, IrType::Ref]);
        let mut lo = map_fixture(builder.build(&code, code.len()).expect("array store graph"));
        let id = lo
            .graph
            .nodes
            .iter()
            .position(|n| n.op == Op::ArrayStore(MemKind::Ref))
            .expect("reference store") as NodeId;
        for param in lo.graph.nodes[id as usize].inputs[2..].to_vec() {
            lo.lower_data_node(param);
        }
        lo.aastore_type_check = target;
        lo.aastore = target + 64;
        if gated {
            lo.ref_store_gates = Some((
                &gates[0] as *const u8 as usize,
                &gates[1] as *const u8 as usize,
                0,
            ));
            lo.ref_store_post_skip_mask = Some(cratonvm_types::GC_FLAG_OLD_GEN);
        }
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_AASTORE", Some("1")),
                ("CRATONVM_JIT_IR_REF_STORE", Some("1")),
                ("CRATONVM_JIT_GATED_REF_STORE", Some("1")),
            ],
            || {
                assert_eq!(lo.gated_ir_aastore_gates().is_some(), gated);
                lo.buf.begin_call_census();
                lo.lower_data_node(id);
            },
        );
        assert!(lo.take_latched_bailout().is_none());
        let returns = lo.buf.take_call_census();
        assert_eq!(returns.len(), 2);
        assert_eq!(lo.oop_maps.len(), 2);
        assert_eq!(lo.oop_maps[0].native_pc_offset, returns[0]);
        assert_eq!(lo.oop_maps[1].native_pc_offset, returns[1]);
        assert_eq!(
            lo.oop_maps[0].bytecode_pc != lo.oop_maps[1].bytecode_pc,
            gated,
            "the two helper paths have independent ID stores only with inline gates"
        );
    }
}

#[test]
fn dynamic_helpers_bind_their_published_maps() {
    let target = fixture_poll as *const () as usize;
    for (op, ty) in [
        (Op::IndyBridge { site: 16 }, IrType::Ref),
        (Op::LambdaIntToDouble, IrType::Double),
    ] {
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let receiver = graph.add(Op::Param(0), IrType::Ref, vec![start], Some(0));
        let input = graph.add(Op::Param(1), IrType::Int, vec![start], Some(0));
        let id = graph.add(op, ty, vec![ctrl, mem, receiver, input], Some(1));
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, id], Some(4));
        let mut lo = map_fixture(graph);
        lo.lower_data_node(receiver);
        lo.lower_data_node(input);
        lo.lambda_int_to_double = target;
        lo.direct_helpers.indy_bridge = target;
        lo.dispatch_threw = target + 64;
        lo.args_stage_top_off = 128;
        lo.args_stage_words = 4;
        lo.buf.begin_call_census();
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_CONCAT_INLINE", Some("0")),
                ("CRATONVM_JIT_INDY_CONSTANT_LAMBDA", Some("0")),
            ],
            || lo.lower_data_node(id),
        );
        assert!(lo.take_latched_bailout().is_none());
        let returns = lo.buf.take_call_census();
        assert_eq!(lo.oop_maps.len(), 1);
        assert_eq!(lo.oop_maps[0].native_pc_offset, returns[0]);
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn back_edge_keeps_a_resident_reference_and_refreshes_it_after_the_poll() {
    #[repr(C)]
    struct Poll {
        requested: u8,
        root: *mut u64,
    }

    unsafe extern "C" fn move_root(flag: usize, _body_id: u32) -> i64 {
        // The flag is the first field of the test's live Poll structure.
        let poll = unsafe { &*(flag as *const Poll) };
        unsafe { poll.root.write(29) };
        0
    }

    // Object loop(Object[] a, int n) {
    //     Object x = a[0];
    //     for (int i = 0; i < n; i++) x = a[i];
    //     return x;
    // }
    let bytecode = [
        0x2a, 0x03, 0x32, 0x4d, 0x03, 0x3e, 0x1d, 0x1b, 0xa2, 0, 13,
        0x2a, 0x1d, 0x32, 0x4d, 0x84, 3, 1, 0xa7, 0xff, 0xf4, 0x2c, 0xb0,
    ];
    for outline in ["0", "1"] {
        let mut builder = crate::ir::IrBuilder::new(2, 4);
        builder.set_param_types(&[IrType::Ref, IrType::Int]);
        let mut lo = map_fixture(builder.build(&bytecode, bytecode.len()).unwrap());
        let (latch, (header, bci)) = (0..lo.schedule.blocks.len())
            .find_map(|block| {
                back_edge_poll_header(lo.graph, lo.schedule, block).map(|site| (block, site))
            })
            .expect("reference loop has a back edge");
        let value = lo
            .graph
            .nodes
            .iter()
            .position(|node| {
                node.op == Op::ArrayLoad(MemKind::Ref) && node.bytecode_pc == Some(13)
            })
            .expect("latch defines a reference") as NodeId;
        lo.prealloc_phi_slots().unwrap();
        let header_ctrl = lo.schedule.blocks[header].ctrl;
        let pred = lo.graph.nodes[header_ctrl as usize]
            .inputs
            .iter()
            .position(|&ctrl| lo.block_of_ctrl(ctrl) == Some(latch))
            .unwrap();
        let sources: Vec<_> = lo.phis_by_merge[&header_ctrl]
            .iter()
            .map(|&phi| lo.graph.nodes[phi as usize].inputs[pred + 1])
            .collect();
        for source in sources {
            lo.alloc_slot_checked(source).unwrap();
        }
        let home = lo.home_offset_of(value).unwrap();
        assert!(
            lo.gather_phi_copies(latch, header)
                .iter()
                .any(|copy| copy.src == value)
        );
        let bailout = lo.take_latched_bailout();
        assert!(bailout.is_none(), "{bailout:?}");

        lo.ref_residency_on = true;
        lo.reg_alloc_on = true;
        lo.phi_copy_regs_on = true;
        lo.gp_reg_of = vec![None; lo.graph.nodes.len()];
        lo.gp_reg_live = vec![false; lo.graph.nodes.len()];
        lo.gp_reg_of[value as usize] = Some(12);
        lo.mark_gp_reg_live(value);
        lo.cur_bci = bci;
        lo.sp_id_slot_off = lo.frame_size + 8;
        let mut frame = vec![0u64; lo.sp_id_slot_off as usize / 8 + 8];
        let end = unsafe { frame.as_mut_ptr().add(frame.len()) };
        let root = unsafe { end.sub(home as usize / 8) };
        let mut poll = Poll { requested: 0, root };
        lo.safepoint_flag_addr = std::ptr::addr_of!(poll.requested) as usize;
        lo.safepoint_slow_path = move_root as *const () as usize;
        // Save RBP and R12, align the call, and leave Win64 argument space.
        lo.buf.emit(&[0x55, 0x41, 0x54, 0x48, 0x83, 0xec, 0x28]);
        lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]); // RBP = test frame
        lo.emit_mov_reg_imm64(12, 17);
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_BACK_EDGE_POLL_OUTLINE", Some(outline)),
                ("CRATONVM_JIT_IR_PAGE_POLL", Some("0")),
                ("CRATONVM_JIT_IR_POLL_SLOW_PATH_DEAD_HOME_CLEAR", Some("1")),
                ("CRATONVM_JIT_POLL_THREAD_WORD", Some("0")),
            ],
            || {
                lo.emit_safepoint_poll(Some(latch), Some(header));
                assert_eq!(lo.resident_gpr(value), Some(12), "resident phi source after poll");
                lo.emit_mov_reg_reg64(RAX, 12);
                lo.buf.emit(&[0x48, 0x83, 0xc4, 0x28, 0x41, 0x5c, 0x5d, 0xc3]);
                lo.emit_outlined_polls();
            },
        );
        let bailout = lo.take_latched_bailout();
        assert!(bailout.is_none(), "{bailout:?}");
        lo.buf.finalize();
        // The leaf consumes the test frame and returns the reference in R12.
        let run: unsafe extern "C" fn(*mut u64) -> u64 =
            unsafe { std::mem::transmute(lo.buf.as_ptr()) };
        unsafe { root.write(17) };
        assert_eq!(unsafe { run(end) }, 17);
        unsafe { std::ptr::addr_of_mut!(poll.requested).write_volatile(1) };
        assert_eq!(unsafe { run(end) }, 29);
        assert_eq!(unsafe { root.read() }, 29);
    }
}

#[test]
fn outlined_poll_uses_register_owners_from_its_branch_site() {
    // static Object identity(Object o) { return o; }
    let code = [0x2a, 0xb0];
    let mut builder = crate::ir::IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    let graph = builder.build(&code, code.len()).expect("identity builds");
    let reference = graph.nodes.iter().position(|n| n.ty == IrType::Ref)
        .expect("reference parameter") as NodeId;
    let mut lo = map_fixture(graph);
    let flag = 1u8;
    lo.safepoint_flag_addr = (&flag as *const u8) as usize;
    lo.safepoint_slow_path = fixture_poll as *const () as usize;
    lo.ref_residency_on = true;
    lo.gp_reg_owner[13] = Some(reference);
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE", Some("1")),
          ("CRATONVM_JIT_IR_PAGE_POLL", Some("0"))],
        || {
            lo.emit_safepoint_poll(None, None);
            assert_eq!(lo.outlined_polls.len(), 1);
            // Code emitted after the poll may reuse R13 and use R12 for a ref.
            lo.gp_reg_owner[13] = None;
            lo.gp_reg_owner[12] = Some(reference);
            lo.emit_outlined_polls();
        },
    );
    assert!(lo.take_latched_bailout().is_none());
    let map = lo.oop_maps.last().expect("outlined poll map");
    assert_ne!(map.native_pc_offset, 0);
    assert_eq!(map.reg_oop_mask, Some(crate::x64::reg_oop_mask_bit(13)));
    assert_eq!(lo.gp_reg_owner[12], Some(reference), "restore end-of-body state");
    assert_eq!(lo.gp_reg_owner[13], None);
}

#[test]
fn live_resident_reference_is_named_as_register_ref_at_guard_and_verified() {
    // static Object identity(Object o) { return o; }
    let code = [0x2a, 0xb0];
    let mut builder = crate::ir::IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    let graph = builder.build(&code, code.len()).expect("identity builds");
    let reference = graph
        .nodes
        .iter()
        .position(|n| n.ty == IrType::Ref)
        .expect("reference parameter") as NodeId;
    let mut lo = map_fixture(graph);
    let n = lo.graph.nodes.len();
    lo.deopt_regs_base = 8 * 16;
    lo.deopt_regs_bytes = super::DEOPT_IMAGE_FULL_BYTES;
    lo.ref_residency_on = true;
    lo.defined_nodes = vec![true; n];
    let mut gp_reg_of = vec![None; n];
    gp_reg_of[reference as usize] = Some(13);
    lo.set_residency(super::RegResidency {
        reg_of: vec![None; n],
        gp_reg_of,
        promoted: 1,
        demoted: 0,
        peak_live: 1,
        splits: super::LsSplitPlan::default(),
        phi_census: super::PhiCensus::default(),
        poll_fp_reloads: std::collections::HashMap::new(),
        caller_saved_republish: Vec::new(),
    });
    lo.gp_reg_live[reference as usize] = true;
    lo.gp_reg_owner[13] = Some(reference);
    assert_eq!(
        lo.frame_value_for(reference),
        crate::deopt::FrameValue::RegisterRef(13)
    );
    lo.emit_safepoint_map(lo.spill_high_water);
    let map = lo.oop_maps.last().expect("safepoint map");
    assert_eq!(map.reg_oop_mask, Some(crate::x64::reg_oop_mask_bit(13)));
}
