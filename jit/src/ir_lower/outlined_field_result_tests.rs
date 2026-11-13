// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::x64::zgc_read_gate_flip_test_lock;

unsafe extern "C" fn field_answer(_ctx: usize, _base: usize, _index: u64) -> i64 {
    -99
}

unsafe extern "C" fn no_field_exception() -> i64 {
    0
}
unsafe extern "C" fn field_min_answer(_ctx: usize, _base: usize, _index: u64) -> i64 {
    i64::MIN
}

fn field_paths(proven: bool, wide: bool, receiver_reg: Option<u8>) {
    let mut executed = 0;
    let _gate = zgc_read_gate_flip_test_lock();
    for allocator in [false, true] {
        for dst in [3_u8, 12, 10, 11] {
            for poison in [false, true] {
                for dropped in [false, true] {
                    let values: &[(u8, i64)] = if wide {
                        &[
                            (b'J', i64::MIN),
                            (b'J', 0x1234_5678_0000_0011),
                            (b'J', i64::MAX),
                        ]
                    } else {
                        &[
                            (b'I', i64::from(i32::MIN)),
                            (b'B', -123),
                            (b'S', -30000),
                            (b'C', 60000),
                            (b'Z', 1),
                        ]
                    };
                    for &(tag, value) in values {
                        let mut b = crate::ir::IrBuilder::new(1, 1);
                        b.set_param_types(&[IrType::Ref]);
                        let mut g = b.build(&[0x2a, 0xbe, 0xac], 3).unwrap();
                        let id = g
                            .nodes
                            .iter()
                            .position(|n| n.op == Op::ArrayLength)
                            .unwrap() as NodeId;
                        let base = g.nodes[id as usize].inputs[2];
                        let ty = if wide { IrType::Long } else { IrType::Int };
                        g.nodes[id as usize].op =
                            Op::Load(if wide { MemKind::Long } else { MemKind::Int });
                        g.nodes[id as usize].ty = ty;
                        let field = g.add(Op::Const(0), IrType::Int, vec![], None);
                        g.nodes[id as usize].inputs.push(field);
                        let g = Box::leak(Box::new(g));
                        let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
                        let plan = Box::leak(Box::new(plan_slots(g, schedule, None)));
                        let mut lo = super::tests::lowerer_over(g, schedule, plan, 8192);
                        lo.reg_alloc_on = allocator;
                        lo.operand_in_place_on = true;
                        lo.publish_at_def_on = true;
                        lo.poison_int_high_on = poison;
                        lo.context_slot_off = 8;
                        let helper_min = wide && value == i64::MIN;
                        lo.getfield = if helper_min {
                            field_min_answer as *const () as usize
                        } else {
                            field_answer as *const () as usize
                        };
                        lo.dispatch_threw = no_field_exception as *const () as usize;
                        lo.read_bounds_addr = 1; // trusted primitive receiver uses no table
                        lo.compact_fields.insert((1, false), (64, false, tag));
                        lo.proven_fields_on = false;
                        lo.gp_reg_of = vec![None; g.nodes.len()];
                        lo.gp_reg_live = vec![false; g.nodes.len()];
                        lo.gp_reg_of[id as usize] = Some(dst);
                        if let Some(reg) = receiver_reg {
                            lo.gp_reg_of[base as usize] = Some(reg);
                            lo.mark_gp_reg_live(base);
                        }
                        lo.home_dropped.resize(g.nodes.len(), false);
                        lo.home_dropped[id as usize] = dropped;
                        lo.cur_def = Some(id);
                        lo.cur_bci = 1;
                        lo.cur_node_pc = Some(1);
                        let slot = lo.alloc_slot(id);
                        let base_slot = lo.alloc_slot(base);
                        // Three saved words leave RSP aligned for a call; reserve Win64 shadow space.
                        lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x48, 0x83, 0xec, 32]);
                        lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
                        lo.store_abi_reg_raw(ENTRY_ABI_REGS[1], base_slot);
                        if let Some(reg) = receiver_reg {
                            lo.emit_mov_reg_reg64(reg, ENTRY_ABI_REGS[1]);
                        }
                        let start = lo.buf.pos();
                        if proven {
                            let block = schedule.node_to_block[id as usize];
                            lo.layout_guard_deopt_on = true;
                            lo.layout_proven.push((base, block));
                            lo.emit_proven_receiver_getfield(
                                ProvenReceiverMode::Proven,
                                1,
                                ty,
                                base,
                                0,
                                slot,
                                tag,
                                64,
                                false,
                                None,
                            );
                        } else {
                            cratonvm_types::flags::with_thread_overrides(
                                &[("CRATONVM_JIT_IR_GETFIELD_COLD_TAIL", Some("1"))],
                                || {
                                    assert!(lo.emit_inline_compact_getfield(
                                        Some(1),
                                        ty,
                                        base,
                                        0,
                                        slot,
                                        false
                                    ))
                                },
                            );
                        }
                        assert_eq!(lo.deferred_getfield_tails.len(), usize::from(!proven));
                        let address_reg = receiver_reg.unwrap_or(RAX);
                        let mut expected_load =
                            vec![0x48 | (u8::from(dst >= 8) << 2) | u8::from(address_reg >= 8)];
                        match tag {
                            b'B' => expected_load.extend([0x0f, 0xbe]),
                            b'S' => expected_load.extend([0x0f, 0xbf]),
                            b'C' => expected_load.extend([0x0f, 0xb7]),
                            b'Z' => expected_load.extend([0x0f, 0xb6]),
                            b'J' => expected_load.push(0x8b),
                            _ => expected_load.push(0x63),
                        }
                        expected_load.push(0x80 | ((dst & 7) << 3) | (address_reg & 7));
                        if address_reg & 7 == 4 {
                            expected_load.push(0x24);
                        }
                        expected_load.extend(64_i32.to_le_bytes());
                        assert!(lo.buf.as_slice()[start..].windows(expected_load.len()).any(|w| w == expected_load),
                        "outlined compact read must target its assigned register: dst={dst} tag={tag} base={address_reg}");
                        lo.emit_mov_reg_reg64(RAX, dst);
                        let epilogue = lo.buf.pos();
                        lo.buf
                            .emit(&[0x48, 0x83, 0xc4, 32, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
                        let tail_start = lo.buf.pos();
                        lo.emit_deferred_getfield_tails();
                        if !proven {
                            let mut legacy_load = vec![
                                expected_load[0],
                                if wide { 0x8b } else { 0x63 },
                                0x80 | ((dst & 7) << 3) | (address_reg & 7),
                            ];
                            if address_reg & 7 == 4 {
                                legacy_load.push(0x24);
                            }
                            let payload = if wide {
                                FIELD_CELL_PAYLOAD64_OFFSET
                            } else {
                                FIELD_CELL_PAYLOAD32_OFFSET
                            };
                            let displacement = legacy_cell_disp(0) + payload as i32;
                            legacy_load.extend(displacement.to_le_bytes());
                            assert!(lo.buf.as_slice()[tail_start..].starts_with(&legacy_load), "legacy tail must load directly to assigned register with its own payload width: dst={dst} tag={tag} base={address_reg}");
                        }
                        for (patch, _) in std::mem::take(&mut lo.call_exc_patches) {
                            Lowerer::patch_or_bail(
                                &mut lo.buf,
                                patch,
                                epilogue as i32 - (patch as i32 + 4),
                            );
                        }
                        assert!(lo.take_latched_bailout().is_none());
                        lo.buf.finalize();
                        for route in 0..if proven { 1 } else { 3 } {
                            let mut object = vec![0_u64; 32];
                            let bytes = unsafe {
                                std::slice::from_raw_parts_mut(
                                    object.as_mut_ptr().cast::<u8>(),
                                    object.len() * 8,
                                )
                            };
                            let legacy_value = if matches!(tag, b'B' | b'S' | b'C' | b'Z') {
                                value ^ 0x0040_0000
                            } else {
                                value
                            };
                            if route == 0 {
                                bytes[cratonvm_types::GC_FLAGS_BYTE_OFFSET] |=
                                    cratonvm_types::GC_FLAG_COMPACT;
                                let width = if wide { 8 } else { 4 };
                                bytes[64..64 + width]
                                    .copy_from_slice(&value.to_le_bytes()[..width]);
                            } else {
                                let payload = if wide {
                                    FIELD_CELL_PAYLOAD64_OFFSET
                                } else {
                                    FIELD_CELL_PAYLOAD32_OFFSET
                                };
                                let off = legacy_cell_disp(0) as usize + payload;
                                let width = if wide { 8 } else { 4 };
                                bytes[off..off + width]
                                    .copy_from_slice(&legacy_value.to_le_bytes()[..width]);
                            }
                            let receiver = if route == 2 {
                                0
                            } else {
                                object.as_ptr() as usize
                            };
                            let mut frame =
                                vec![0_u64; slot.max(base_slot).max(8) as usize / 8 + 16];
                            let frame_len = frame.len();
                            let mut expected = frame.clone();
                            expected[frame_len - base_slot as usize / 8] = receiver as u64;
                            let want = if route == 2 {
                                if helper_min {
                                    i64::MIN
                                } else {
                                    -99i64
                                }
                            } else if route == 1 {
                                legacy_value
                            } else {
                                value
                            } as u64;
                            // R11 is the existing poison emitter's reserved addend.
                            let want = if poison && !wide && dst != R11 {
                                want.wrapping_add(0xa5a5a5a5_00000000)
                            } else {
                                want
                            };
                            if !dropped {
                                expected[frame_len - slot as usize / 8] = want;
                            }
                            // SAFETY: aligned fake header/cell storage, bounded supplied homes,
                            // and a non-collecting helper that never dereferences the receiver.
                            let f: unsafe extern "C" fn(*mut u64, usize) -> u64 =
                                unsafe { std::mem::transmute(lo.buf.as_ptr()) };
                            let got = unsafe { f(frame.as_mut_ptr().add(frame_len), receiver) };
                            executed += 1;
                            assert_eq!(got, want, "dst={dst} tag={tag} poison={poison} dropped={dropped} route={route}");
                            assert_eq!(frame, expected, "exact frame image, route={route}");
                        }
                    }
                }
            }
        }
    }
    eprintln!("allocated field native cases: proven={proven} wide={wide} base={receiver_reg:?} count={executed}");
}

#[test]
fn outlined_primitive_field_paths_share_the_selected_register_and_home() {
    field_paths(false, false, None);
}

#[test]
fn proven_primitive_fields_use_the_selected_register_and_home() {
    field_paths(true, false, None);
}

#[test]
fn resolved_lambda_capture_uses_the_legacy_cell_directly() {
    let _gate = zgc_read_gate_flip_test_lock();
    cratonvm_types::flags::with_thread_overrides(
        &[
            ("CRATONVM_JIT_IR_OPERAND_IN_PLACE", Some("1")),
            ("CRATONVM_JIT_IR_LEGACY_FIELD_INLINE", Some("1")),
        ],
        || {
            let bounds = [0_u64; 6];
            let mut observed = Vec::new();
            for owner in [None, Some(7), Some(GETCLASS_WORDS_LAMBDA_BASE_ID + 1)] {
                let mut builder = crate::ir::IrBuilder::new(1, 1);
                builder.set_param_types(&[IrType::Ref]);
                builder.set_field_info(HashMap::from([(1, (1, b'I'))]));
                if let Some(owner) = owner {
                    builder.set_field_decl_classes(HashMap::from([(1, owner)]));
                }
                let graph = builder.build(&[0x2a, 0xb4, 0, 1, 0xac], 5).unwrap();
                let schedule = crate::ir_schedule::schedule(&graph);
                // SAFETY: the helper table holds integer addresses, and this
                // fixture wires the only helper its load may call.
                let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
                helpers.getfield = field_answer as *const () as usize;
                helpers.read_bounds_addr = bounds.as_ptr() as usize;
                let compiled = lower_inner(
                    &graph,
                    &schedule,
                    1,
                    1,
                    &helpers,
                    &HashMap::new(),
                    &[],
                    None,
                    &HashMap::new(),
                    &HashMap::new(),
                    &HashMap::new(),
                    &crate::ir::IrInlineFrameSites::default(),
                )
                .unwrap();
                let mut object = [0_u64; 8];
                let value = -1234567_i32;
                let off = legacy_cell_disp(1) as usize + FIELD_CELL_PAYLOAD32_OFFSET;
                // SAFETY: the aligned buffer contains a complete header and
                // two legacy cells with the second int payload at `off`.
                unsafe {
                    let bytes = object.as_mut_ptr().cast::<u8>();
                    bytes
                        .add(cratonvm_types::NUM_SLOTS_OFFSET)
                        .cast::<u32>()
                        .write(2);
                    bytes.add(off).cast::<i32>().write(value);
                }
                let before = object;
                // SAFETY: both inputs are handled by the inline load or the
                // fixture helper, which does not dereference null receivers.
                unsafe {
                    assert_eq!(
                        compiled.call_with_heap(0, &[object.as_ptr() as i64]),
                        i64::from(value)
                    );
                    assert_eq!(compiled.call_with_heap(0, &[0]), -99);
                }
                assert_eq!(object, before);
                let mut header_test = vec![0xF6, 0x80];
                header_test.extend_from_slice(
                    &(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes(),
                );
                header_test.push(cratonvm_types::GC_FLAG_COMPACT);
                observed.push(
                    compiled
                        .code_bytes()
                        .windows(header_test.len())
                        .filter(|w| *w == header_test)
                        .count(),
                );
            }
            assert_eq!(
                observed,
                [1, 1, 0],
                "only the resolved lambda capture skips the compact layout test"
            );
        },
    );
}

#[test]
fn outlined_long_fields_load_directly_to_the_assigned_register() {
    field_paths(false, true, None);
}

#[test]
fn proven_long_fields_load_directly_to_the_assigned_register() {
    field_paths(true, true, None);
}

#[test]
fn ordinary_fields_use_the_resident_receiver_without_an_accumulator_copy() {
    for reg in [3, 12] {
        field_paths(false, false, Some(reg));
        field_paths(false, true, Some(reg));
    }
}

#[test]
fn proven_fields_use_the_resident_receiver_without_an_accumulator_copy() {
    for reg in [3, 12] {
        field_paths(true, false, Some(reg));
        field_paths(true, true, Some(reg));
    }
}

#[test]
fn rowless_field_checks_the_resident_receiver_before_loading_to_rax() {
    let _gate = zgc_read_gate_flip_test_lock();
    let mut builder = crate::ir::IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    builder.set_field_info(HashMap::from([(1, (1, b'I'))]));
    let mut graph = builder.build(&[0x2a, 0xb4, 0, 1, 0xac], 5).unwrap();
    let id = graph
        .nodes
        .iter()
        .position(|n| n.op == Op::Load(MemKind::Int))
        .unwrap() as NodeId;
    let base = graph.nodes[id as usize].inputs[2];
    // Exercise the full containment route even when trusted-oop reads are on.
    graph.nodes[base as usize].ty = IrType::Long;
    let graph = Box::leak(Box::new(graph));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let mut object = [0_u64; 8];
    let address = object.as_ptr() as usize;
    let bounds = [
        0_usize,
        0,
        address,
        address + std::mem::size_of_val(&object),
        0,
        0,
    ];
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 8192);
    lo.operand_in_place_on = true;
    lo.context_slot_off = 8;
    lo.read_bounds_addr = bounds.as_ptr() as usize;
    lo.getfield = field_answer as *const () as usize;
    lo.dispatch_threw = no_field_exception as *const () as usize;
    lo.proven_fields_on = false;
    lo.gp_reg_of = vec![None; graph.nodes.len()];
    lo.gp_reg_live = vec![false; graph.nodes.len()];
    let receiver = 12;
    lo.gp_reg_of[base as usize] = Some(receiver);
    lo.mark_gp_reg_live(base);
    lo.cur_def = Some(id);
    lo.cur_bci = 1;
    lo.cur_node_pc = Some(1);
    let slot = lo.alloc_slot(id);
    let base_slot = lo.alloc_slot(base);

    lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x48, 0x83, 0xec, 32]);
    lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
    lo.store_abi_reg_raw(ENTRY_ABI_REGS[1], base_slot);
    lo.emit_mov_reg_reg64(receiver, ENTRY_ABI_REGS[1]);
    let start = lo.buf.pos();
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_IR_LEGACY_FIELD_INLINE", Some("1"))],
        || assert!(lo.emit_inline_compact_getfield(Some(1), IrType::Int, base, 1, slot, false)),
    );

    // CMP [r12 + num_slots], 1 must address the receiver even without a result
    // register. Its SIB byte is required because the base is R12.
    let mut slot_cmp = vec![0x41, 0x81, 0xbc, 0x24];
    slot_cmp.extend(legacy_num_slots_disp().to_le_bytes());
    slot_cmp.extend(1_i32.to_le_bytes());
    assert!(lo.buf.as_slice()[start..]
        .windows(slot_cmp.len())
        .any(|w| w == slot_cmp));

    let epilogue = lo.buf.pos();
    lo.buf
        .emit(&[0x48, 0x83, 0xc4, 32, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
    lo.emit_deferred_getfield_tails();
    for (patch, _) in std::mem::take(&mut lo.call_exc_patches) {
        Lowerer::patch_or_bail(&mut lo.buf, patch, epilogue as i32 - (patch as i32 + 4));
    }
    assert!(lo.take_latched_bailout().is_none());
    lo.buf.finalize();
    let mut frame = vec![0_u64; slot.max(base_slot).max(8) as usize / 8 + 16];
    let frame_len = frame.len();
    let f: unsafe extern "C" fn(*mut u64, usize) -> i64 =
        unsafe { std::mem::transmute(lo.buf.as_ptr()) };
    let value = -1234567_i32;
    let cases = [
        (address, 2, 0, i64::from(value)),
        (address, 1, 0, -99),
        (address, 2, cratonvm_types::GC_FLAG_COMPACT, -99),
        (address + 1, 2, 0, -99),
        (bounds[3], 2, 0, -99),
        (0, 2, 0, -99),
    ];
    for (arg, slots, flags, expected) in cases {
        // SAFETY: aligned backing storage contains the full header and two
        // legacy slots. Rejected receiver addresses reach a non-reading helper.
        unsafe {
            let bytes = object.as_mut_ptr().cast::<u8>();
            bytes
                .add(cratonvm_types::NUM_SLOTS_OFFSET)
                .cast::<u32>()
                .write(slots);
            bytes.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET).write(flags);
            bytes
                .add(legacy_cell_disp(1) as usize + FIELD_CELL_PAYLOAD32_OFFSET)
                .cast::<i32>()
                .write(value);
            assert_eq!(f(frame.as_mut_ptr().add(frame_len), arg), expected);
        }
        assert_eq!(frame[frame_len - slot as usize / 8] as i64, expected);
    }
    assert_eq!(std::hint::black_box(bounds)[2], address);
}

#[test]
fn legacy_only_field_load_falls_through_and_keeps_checked_fallbacks() {
    let _gate = zgc_read_gate_flip_test_lock();
    for kind in [MemKind::Int, MemKind::Long, MemKind::Ref] {
        let wide = kind != MemKind::Int;
        let reference = kind == MemKind::Ref;
        let mut b = crate::ir::IrBuilder::new(1, 1);
        b.set_param_types(&[IrType::Ref]);
        let mut g = b.build(&[0x2a, 0xbe, 0xac], 3).unwrap();
        let id = g.nodes.iter().position(|n| n.op == Op::ArrayLength).unwrap() as NodeId;
        let base = g.nodes[id as usize].inputs[2];
        let ty = if reference { IrType::Ref } else if wide { IrType::Long } else { IrType::Int };
        g.nodes[id as usize].op = Op::Load(kind);
        g.nodes[id as usize].ty = ty;
        let field = g.add(Op::Const(0), IrType::Int, vec![], None);
        g.nodes[id as usize].inputs.push(field);
        let graph = Box::leak(Box::new(g));
        let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
        let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let mut lo = super::tests::lowerer_over(graph, schedule, plan, 8192);
        lo.getfield = field_answer as *const () as usize;
        lo.dispatch_threw = no_field_exception as *const () as usize;
        let bounds = [0u64, u64::MAX, 0, 0, 0, 0];
        lo.read_bounds_addr = bounds.as_ptr() as usize;
        lo.context_slot_off = 8;
        lo.cur_def = Some(id);
        lo.cur_bci = 1;
        lo.cur_node_pc = Some(1);
        let slot = lo.alloc_slot(id);
        let base_slot = lo.alloc_slot(base);
        lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x48, 0x83, 0xec, 32]);
        lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
        lo.store_abi_reg_raw(ENTRY_ABI_REGS[1], base_slot);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_GETFIELD_COLD_TAIL", Some("1")),
              ("CRATONVM_JIT_IR_LEGACY_FIELD_INLINE", Some("1"))],
            || assert!(lo.emit_inline_compact_getfield(Some(1), ty, base, 0, slot, false)),
        );
        assert_eq!(lo.deferred_getfield_tails.len(), 1);
        assert!(lo.deferred_getfield_tails[0].legacy_patch.is_none());
        lo.load_reg_from_frame(RAX, slot);
        let epilogue = lo.buf.pos();
        lo.buf.emit(&[0x48, 0x83, 0xc4, 32, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
        lo.emit_deferred_getfield_tails();
        for (patch, _) in std::mem::take(&mut lo.call_exc_patches) {
            Lowerer::patch_or_bail(&mut lo.buf, patch, epilogue as i32 - (patch as i32 + 4));
        }
        assert!(lo.take_latched_bailout().is_none());
        lo.buf.finalize();
        let run: unsafe extern "C" fn(*mut u64, usize) -> u64 =
            unsafe { std::mem::transmute(lo.buf.as_ptr()) };
        let mut object = vec![0u64; 32];
        let bytes = unsafe { std::slice::from_raw_parts_mut(object.as_mut_ptr().cast::<u8>(), object.len() * 8) };
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        let payload = if wide { FIELD_CELL_PAYLOAD64_OFFSET } else { FIELD_CELL_PAYLOAD32_OFFSET };
        let off = legacy_cell_disp(0) as usize + payload;
        let value = if reference { 0x1234_5678u64 } else if wide { 0x1234_5678_8765_4321u64 } else { 0xffff_ffff_8765_4321u64 };
        if reference {
            let tag = legacy_cell_disp(0) as usize + cratonvm_types::FIELD_CELL_TAG_OFFSET;
            bytes[tag..tag + 4].copy_from_slice(&cratonvm_types::FIELD_CELL_TAG_OBJECT.to_le_bytes());
        }
        let width = if wide { 8 } else { 4 };
        bytes[off..off + width].copy_from_slice(&value.to_le_bytes()[..width]);
        let mut frame = vec![0u64; slot.max(base_slot).max(8) as usize / 8 + 16];
        let end = unsafe { frame.as_mut_ptr().add(frame.len()) };
        let ptr = object.as_ptr() as usize;
        assert_eq!(unsafe { run(end, ptr) }, value, "legacy field");
        if reference {
            let tag = legacy_cell_disp(0) as usize + cratonvm_types::FIELD_CELL_TAG_OFFSET;
            unsafe { object.as_mut_ptr().cast::<u8>().add(tag).cast::<u32>().write(0) };
            assert_eq!(unsafe { run(end, ptr) } as i64, -99, "wrong tag uses helper");
        }
        object[1] = 0;
        assert_eq!(unsafe { run(end, ptr) } as i64, -99, "short object uses helper");
        object[1] = 1;
        unsafe { object.as_mut_ptr().cast::<u8>().add(cratonvm_types::GC_FLAGS_BYTE_OFFSET).write(cratonvm_types::GC_FLAG_COMPACT) };
        assert_eq!(unsafe { run(end, ptr) } as i64, -99, "compact object uses helper");
        assert_eq!(unsafe { run(end, 0) } as i64, -99, "null uses helper");
    }
}

#[test]
fn proven_reference_read_falls_through_and_keeps_barrier_fallback() {
    let _gate = zgc_read_gate_flip_test_lock();
    for outlined in [false, true] {
        let mut b = crate::ir::IrBuilder::new(1, 1);
        b.set_param_types(&[IrType::Ref]);
        let mut g = b.build(&[0x2a, 0xbe, 0xac], 3).unwrap();
        let id = g.nodes.iter().position(|n| n.op == Op::ArrayLength).unwrap() as NodeId;
        let base = g.nodes[id as usize].inputs[2];
        g.nodes[id as usize].op = Op::Load(MemKind::Ref);
        g.nodes[id as usize].ty = IrType::Ref;
        let field = g.add(Op::Const(0), IrType::Int, vec![], None);
        g.nodes[id as usize].inputs.push(field);
        let graph = Box::leak(Box::new(g));
        let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
        let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let mut lo = super::tests::lowerer_over(graph, schedule, plan, 8192);
        let mut bounds = Box::new([0_u64, 1, 0, 0, 0, 0]);
        lo.read_bounds_addr = bounds.as_mut_ptr() as usize;
        lo.getfield = field_answer as *const () as usize;
        lo.dispatch_threw = no_field_exception as *const () as usize;
        lo.context_slot_off = 8;
        lo.cur_def = Some(id);
        lo.cur_bci = 1;
        lo.cur_node_pc = Some(1);
        lo.layout_guard_deopt_on = true;
        lo.layout_proven.push((base, schedule.node_to_block[id as usize]));
        let slot = lo.alloc_slot(id);
        let base_slot = lo.alloc_slot(base);
        lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x48, 0x83, 0xec, 32]);
        lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
        lo.store_abi_reg_raw(ENTRY_ABI_REGS[1], base_slot);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_GETFIELD_COLD_TAIL", Some(if outlined { "1" } else { "0" }))],
            || lo.emit_proven_receiver_getfield(
                ProvenReceiverMode::Proven, 1, IrType::Ref, base, 0, slot,
                b'L', 64, false, None,
            ),
        );
        assert_eq!(lo.deferred_getfield_tails.len(), usize::from(outlined));
        if outlined {
            let tail = &lo.deferred_getfield_tails[0];
            assert!(tail.legacy_patch.is_none());
            assert!(lo.buf.as_slice()[..tail.done].ends_with(&[0x48, 0x8b, 0x80, 64, 0, 0, 0]),
                "the compact reference load must fall straight through to the join");
        }
        lo.load_reg_from_frame(RAX, slot);
        let epilogue = lo.buf.pos();
        lo.buf.emit(&[0x48, 0x83, 0xc4, 32, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
        lo.emit_deferred_getfield_tails();
        for (patch, _) in std::mem::take(&mut lo.call_exc_patches) {
            Lowerer::patch_or_bail(&mut lo.buf, patch, epilogue as i32 - (patch as i32 + 4));
        }
        assert!(lo.take_latched_bailout().is_none());
        lo.buf.finalize();
        let run: unsafe extern "C" fn(*mut u64, usize) -> u64 =
            unsafe { std::mem::transmute(lo.buf.as_ptr()) };
        let mut object = [0_u64; 16];
        object[8] = 0x1234_5678;
        let mut frame = vec![0_u64; slot.max(base_slot).max(8) as usize / 8 + 16];
        let end = unsafe { frame.as_mut_ptr().add(frame.len()) };
        // SAFETY: the established receiver is non-null and points to aligned
        // storage containing the field; the fallback helper never dereferences it.
        assert_eq!(unsafe { run(end, object.as_ptr() as usize) }, object[8]);
        bounds[1] = 0;
        std::hint::black_box(&bounds);
        assert_eq!(unsafe { run(end, object.as_ptr() as usize) } as i64, -99);
    }
}

#[test]
fn field_reads_reuse_the_non_null_allocation_proof() {
    let _gate = zgc_read_gate_flip_test_lock();
    for allocated in [false, true] {
        let mut b = crate::ir::IrBuilder::new(1, 1);
        b.set_param_types(&[IrType::Ref]);
        let mut g = b.build(&[0x2a, 0xbe, 0xac], 3).unwrap();
        let id = g.nodes.iter().position(|n| n.op == Op::ArrayLength).unwrap() as NodeId;
        let base = g.nodes[id as usize].inputs[2];
        g.nodes[id as usize].op = Op::Load(MemKind::Int);
        g.nodes[id as usize].ty = IrType::Int;
        let field = g.add(Op::Const(0), IrType::Int, vec![], None);
        g.nodes[id as usize].inputs.push(field);
        if allocated {
            // This fixture emits the field read only; the caller supplies the
            // non-null result an actual successful New would have published.
            g.nodes[base as usize].op = Op::New { class_id: 7, num_fields: 1 };
            g.nodes[base as usize].inputs = vec![0, 2].into();
        }
        let graph = Box::leak(Box::new(g));
        let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
        let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let mut lo = super::tests::lowerer_over(graph, schedule, plan, 8192);
        lo.getfield = field_answer as *const () as usize;
        lo.dispatch_threw = no_field_exception as *const () as usize;
        lo.read_bounds_addr = 1; // trusted primitive receiver uses no table
        lo.context_slot_off = 8;
        lo.cur_def = Some(id);
        lo.cur_bci = 1;
        lo.cur_node_pc = Some(1);
        lo.compact_fields.insert((1, false), (64, false, b'I'));
        let slot = lo.alloc_slot(id);
        let base_slot = lo.alloc_slot(base);
        lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x48, 0x83, 0xec, 32]);
        lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
        lo.store_abi_reg_raw(ENTRY_ABI_REGS[1], base_slot);
        let start = lo.buf.pos();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_GETFIELD_COLD_TAIL", Some("1"))],
            || assert!(lo.emit_inline_compact_getfield(Some(1), IrType::Int, base, 0, slot, false)),
        );
        let field_code = &lo.buf.as_slice()[start..];
        assert_eq!(field_code.windows(3).any(|w| w == [0x48, 0x85, 0xc0]), !allocated,
            "only an SSA-proven non-null receiver may omit the null test");
        lo.load_reg_from_frame(RAX, slot);
        let epilogue = lo.buf.pos();
        lo.buf.emit(&[0x48, 0x83, 0xc4, 32, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
        lo.emit_deferred_getfield_tails();
        for (patch, _) in std::mem::take(&mut lo.call_exc_patches) {
            Lowerer::patch_or_bail(&mut lo.buf, patch, epilogue as i32 - (patch as i32 + 4));
        }
        assert!(lo.take_latched_bailout().is_none());
        lo.buf.finalize();
        let run: unsafe extern "C" fn(*mut u64, usize) -> i64 =
            unsafe { std::mem::transmute(lo.buf.as_ptr()) };
        let mut object = [0_u64; 16];
        unsafe { object.as_mut_ptr().cast::<u8>().add(cratonvm_types::GC_FLAGS_BYTE_OFFSET).write(cratonvm_types::GC_FLAG_COMPACT); }
        object[8] = 123;
        let mut frame = vec![0_u64; slot.max(base_slot).max(8) as usize / 8 + 16];
        let end = unsafe { frame.as_mut_ptr().add(frame.len()) };
        // SAFETY: the successful New arm is represented by the aligned,
        // non-null compact object; the other arm's helper accepts null.
        assert_eq!(unsafe { run(end, object.as_ptr() as usize) }, 123);
        if !allocated {
            assert_eq!(unsafe { run(end, 0) }, -99);
        }
    }
}
