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
                                &[],
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
                            // SAFETY: the exclusive byte view covers only this live, initialized u64 buffer.
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
                            // SAFETY: this offset stays within the live fixture buffer used as the leaf frame.
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

