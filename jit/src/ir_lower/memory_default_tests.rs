// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir::IrBuilder;

#[derive(Clone, Copy, Debug)]
enum Read {
    Length,
    Element(MemKind),
    StringChar(i64),
}

fn values(read: Read) -> Vec<i64> {
    match read {
        Read::Length => vec![0, 8, i32::MAX as i64],
        Read::Element(MemKind::Byte) => vec![-128, 0, 127],
        Read::Element(MemKind::Short) => vec![-32768, 0, 32767],
        Read::Element(MemKind::Char) => vec![0, 128, 65535],
        Read::Element(MemKind::Long) => vec![i64::MIN, 0, i64::MAX],
        Read::Element(_) => vec![i32::MIN as i64, -1, i32::MAX as i64],
        Read::StringChar(0) => vec![0, 128, 255],
        Read::StringChar(_) => vec![0, 0xd800, 0xffff],
    }
}

fn graph(read: Read) -> (Graph, NodeId, NodeId, NodeId) {
    let mut b = IrBuilder::new(2, 2);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    let opcode = match read {
        Read::Length => 0xbe,
        Read::Element(MemKind::Long) => 0x2f,
        Read::Element(MemKind::Byte) => 0x33,
        Read::Element(MemKind::Char) | Read::StringChar(_) => 0x34,
        Read::Element(MemKind::Short) => 0x35,
        _ => 0x2e,
    };
    let code = if matches!(read, Read::Length) {
        vec![0x2a, opcode, 0xac, 0, 0]
    } else {
        vec![
            0x2a,
            0x1b,
            opcode,
            if opcode == 0x2f { 0xad } else { 0xac },
            0,
            0,
        ]
    };
    let mut g = b.build(&code, code.len() - 2).expect("integer memory read");
    let id = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::ArrayLength | Op::ArrayLoad(_)))
        .unwrap() as NodeId;
    let base = g.nodes[id as usize].inputs[2];
    let index = if matches!(read, Read::Length) {
        base
    } else {
        g.nodes[id as usize].inputs[3]
    };
    if let Read::StringChar(coder) = read {
        let coder = g.add(Op::Const(coder), IrType::Int, vec![], None);
        g.nodes[id as usize].op = Op::StrCharDecode;
        g.nodes[id as usize].inputs.push(coder);
    }
    (g, id, base, index)
}

fn assert_direct_memory_producer(bytes: &[u8], read: Read, dst: u8) {
    use iced_x86::{Decoder, DecoderOptions, Mnemonic as M, OpKind, Register as R};
    let wide = matches!(
        read,
        Read::Element(MemKind::Byte | MemKind::Short | MemKind::Int | MemKind::Long)
    );
    let register = match (dst, wide) {
        (3, true) => R::RBX,
        (3, false) => R::EBX,
        (12, true) => R::R12,
        (12, false) => R::R12D,
        (10, true) => R::R10,
        (10, false) => R::R10D,
        (11, true) => R::R11,
        (11, false) => R::R11D,
        _ => unreachable!(),
    };
    let mnemonic = match read {
        Read::Length | Read::Element(MemKind::Long) => M::Mov,
        Read::Element(MemKind::Int) => M::Movsxd,
        Read::Element(MemKind::Byte | MemKind::Short) => M::Movsx,
        _ => M::Movzx,
    };
    let scales: &[u32] = match read {
        Read::Length => &[1],
        Read::StringChar(_) => &[1, 2],
        Read::Element(MemKind::Long) => &[8],
        Read::Element(MemKind::Int) => &[4],
        Read::Element(MemKind::Char | MemKind::Short) => &[2],
        _ => &[1],
    };
    let index = if matches!(read, Read::Length) {
        R::None
    } else {
        R::RCX
    };
    let displacement = if matches!(read, Read::Length) {
        cratonvm_types::ARRAY_LENGTH_OFFSET as u64
    } else {
        cratonvm_types::HEADER_SIZE as u64
    };
    let mut decoder = Decoder::new(64, bytes, DecoderOptions::NONE);
    let mut instructions = Vec::new();
    while decoder.can_decode() {
        instructions.push(decoder.decode());
    }
    for &scale in scales {
        let found = instructions.iter().any(|i| {
            i.mnemonic() == mnemonic
                && i.op0_register() == register
                && i.op1_kind() == OpKind::Memory
                && i.memory_base() == R::RAX
                && i.memory_index() == index
                && i.memory_index_scale() == scale
                && i.memory_displacement64() == displacement
        });
        assert!(
            found,
            "memory producer must target its assigned register: {read:?} dst={dst} scale={scale}"
        );
    }
}

fn check(read: Read, allocator: bool, dst: u8, poison: bool, dropped: bool) {
    let (g, id, base, index) = graph(read);
    let g = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
    let plan = Box::leak(Box::new(plan_slots(g, schedule, None)));
    let mut lo = super::tests::lowerer_over(g, schedule, plan, 16384);
    lo.reg_alloc_on = allocator;
    lo.operand_in_place_on = true;
    lo.publish_at_def_on = true;
    lo.mir_mode = MirMode::Off;
    lo.poison_int_high_on = poison;
    lo.gp_reg_of = vec![None; g.nodes.len()];
    lo.gp_reg_live = vec![false; g.nodes.len()];
    lo.gp_reg_of[id as usize] = Some(dst);
    lo.cur_def = Some(id);
    lo.cur_def_published = false;
    let slot = lo.alloc_slot(id);
    let base_slot = lo.alloc_slot(base);
    let index_slot = lo.alloc_slot(index);
    lo.home_dropped.resize(g.nodes.len(), false);
    lo.home_dropped[id as usize] = dropped;
    lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x48, 0x83, 0xec, 32]);
    lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
    let start = lo.buf.pos();
    lo.lower_data_node(id);
    assert!(
        lo.take_latched_bailout().is_none(),
        "the supplied memory graph must lower before checking selection"
    );
    // Value checks alone do not detect a redundant RAX-to-register shuffle.
    assert_direct_memory_producer(&lo.buf.as_slice()[start..], read, dst);
    assert!(lo.cur_def_published && lo.gp_reg_live[id as usize], "integer memory read must publish the assigned GPR without enabling home elision: {read:?} allocator={allocator}");
    lo.emit_mov_reg_reg64(RAX, dst);
    lo.buf
        .emit(&[0x48, 0x83, 0xc4, 32, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
    assert!(lo.take_latched_bailout().is_none());
    lo.buf.finalize();
    let run: unsafe extern "C" fn(*mut u64) -> u64 =
        unsafe { std::mem::transmute(lo.buf.as_ptr()) };
    for value in values(read) {
        let mut words = vec![0_u64; 32];
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), words.len() * 8)
        };
        let length = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let array_len = if matches!(read, Read::Length) {
            value as u32
        } else {
            8
        };
        bytes[length..length + 4].copy_from_slice(&array_len.to_le_bytes());
        let width = match read {
            Read::Element(MemKind::Long) => 8,
            Read::Element(MemKind::Char | MemKind::Short) | Read::StringChar(1) => 2,
            Read::Element(MemKind::Byte) | Read::StringChar(0) => 1,
            _ => 4,
        };
        let offset = cratonvm_types::HEADER_SIZE + 2 * width;
        bytes[offset..offset + width].copy_from_slice(&value.to_le_bytes()[..width]);
        let count = slot.max(base_slot).max(index_slot) as usize / 8 + 16;
        let mut frame = vec![0_u64; count];
        frame[count - base_slot as usize / 8] = words.as_ptr() as usize as u64;
        if !matches!(read, Read::Length) {
            frame[count - index_slot as usize / 8] = if poison { 0xa5a5a5a5_00000002 } else { 2 };
        }
        let mut expected = frame.clone();
        let value = value as u64;
        let value = if poison && g.nodes[id as usize].ty == IrType::Int && dst != R11 {
            value.wrapping_add(0xa5a5a5a5_00000000)
        } else {
            value
        };
        if !dropped {
            expected[count - slot as usize / 8] = value;
        }
        // The live supplied array and homes satisfy the unchanged null/bounds guards.
        assert_eq!(
            unsafe { run(frame.as_mut_ptr().add(count)) },
            value,
            "{read:?} allocator={allocator} dst={dst} poison={poison} dropped={dropped}"
        );
        assert_eq!(frame, expected, "exact supplied frame image");
    }
}

#[test]
fn integer_memory_outputs_keep_registers_with_or_without_home_elision() {
    for read in [
        Read::Length,
        Read::Element(MemKind::Byte),
        Read::Element(MemKind::Short),
        Read::Element(MemKind::Char),
        Read::Element(MemKind::Int),
        Read::Element(MemKind::Long),
        Read::StringChar(0),
        Read::StringChar(1),
    ] {
        for allocator in [false, true] {
            for dst in [3, 12, 10, 11] {
                for poison in [false, true] {
                    for dropped in [false, true] {
                        check(read, allocator, dst, poison, dropped);
                    }
                }
            }
        }
    }
}
