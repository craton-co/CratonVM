// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// The native bytes are inspected without entering the synthetic helper.
unsafe extern "C" fn fixture_poll(_flag: usize, _body_id: u32) -> i64 {
    0
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
            candidates: vec![],
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


#[cfg(target_arch = "x86_64")]




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

#[test]
fn implicit_pad_folds_dead_caught_exception_to_direct_branch() {
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
    lo.set_throw_bci = target;
    lo.npe_with_action = target;
    lo.throw_aioobe = target;
    lo.throw_arithmetic = target;
    lo.local_handler_lookup = target;

    // Matching catch with dead/absent CaughtException -> direct branch, zero calls.
    let plan = ImplicitThrowPlan {
        site_ptr: 0,
        targets: vec![0],
        result_home: None,
        candidates: vec![("java/lang/ArrayIndexOutOfBoundsException", 10)],
    };
    lo.buf.begin_call_census();
    lo.emit_implicit_throw_pad(ImplicitKind::Aioobe, 1, id, &plan);
    let returns = lo.buf.take_call_census();
    assert!(returns.is_empty(), "expected direct branch with zero calls, got {returns:?}");
    assert_eq!(lo.branch_patches.len(), 1);
    assert_eq!(lo.branch_patches[0].1, 0);

    // Non-matching catch (NPE only) -> must not fold AIOOBE.
    lo.buf.begin_call_census();
    lo.emit_implicit_throw_pad(ImplicitKind::Npe, 1, id, &plan);
    let returns_npe = lo.buf.take_call_census();
    assert_eq!(returns_npe.len(), 3);
}
