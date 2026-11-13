// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Integer selection is independent of home elision and operand residency.
use super::*;
use crate::ir::IrBuilder;

fn check_allocated_results(a_live: bool, b_live: bool) {
    for ty in [IrType::Int, IrType::Long] {
        let wide = ty == IrType::Long;
        let values = if wide {
            [
                (17, 19),
                (0x1234_5678_0000_0011_i64, 0x7654_3210_0000_0013_i64),
            ]
        } else {
            [(17, 19), (i64::from(i32::MIN), i64::from(i32::MAX))]
        };
        for (av, bv) in values {
            for (bytecode, op) in [
                (0x60, Op::Add),
                (0x64, Op::Sub),
                (0x68, Op::Mul),
                (0x7e, Op::And),
                (0x80, Op::Or),
                (0x82, Op::Xor),
            ] {
                let want = match &op {
                    Op::Add => av.wrapping_add(bv),
                    Op::Sub => av.wrapping_sub(bv),
                    Op::Mul => av.wrapping_mul(bv),
                    Op::And => av & bv,
                    Op::Or => av | bv,
                    Op::Xor => av ^ bv,
                    _ => unreachable!(),
                };
                for (dst, ra, rb) in [
                    (3, 12, 13),
                    (8, 9, 10),
                    (8, 8, 9),
                    (9, 8, 9),
                    (11, 8, 9),
                    (9, 11, 8),
                    (8, 9, 11),
                ] {
                    // These forms would destroy a live second operand.
                    if (op == Op::Sub && dst == rb && b_live) || (!a_live && b_live && dst == rb) {
                        continue;
                    }
                    let mut builder = IrBuilder::new(2, if wide { 4 } else { 2 });
                    builder.set_param_types(&[ty, ty]);
                    let code = if wide {
                        [0x1e, 0x20, bytecode + 1, 0xad]
                    } else {
                        [0x1a, 0x1b, bytecode, 0xac]
                    };
                    let graph = Box::leak(Box::new(builder.build(&code, 4).expect("binary IR")));
                    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
                    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
                    let id = graph
                        .nodes
                        .iter()
                        .position(|n| n.op == op)
                        .expect("binary node") as NodeId;
                    let (a, b) = (
                        graph.nodes[id as usize].inputs[0],
                        graph.nodes[id as usize].inputs[1],
                    );
                    
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
                    lo.reg_alloc_on = false;
                    lo.operand_in_place_on = true;
                    lo.gp_reg_of = vec![None; graph.nodes.len()];
                    lo.gp_reg_live = vec![false; graph.nodes.len()];
                    for (v, r) in [(a, ra), (b, rb), (id, dst)] {
                        lo.gp_reg_of[v as usize] = Some(r);
                    }
                    if a_live {
                        lo.mark_gp_reg_live(a);
                    }
                    if b_live {
                        lo.mark_gp_reg_live(b);
                    }
                    lo.cur_def = Some(id);
                    let slot = lo.alloc_slot(id);
                    let a_slot = lo.alloc_slot(a);
                    let b_slot = lo.alloc_slot(b);
                    lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x41, 0x55]); // rbp/rbx/r12/r13
                    lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]); // supplied frame base
                    lo.emit_mov_reg_imm64(ra, av as u64);
                    lo.emit_mov_reg_imm64(rb, bv as u64);
                    assert!(
                        lo.reg_alloc_direct_alu(id, slot),
                        "{op:?} {ty:?} dst={dst} a={ra}/{a_live} b={rb}/{b_live}"
                    );
                    lo.emit_mov_reg_reg64(RAX, dst);
                    lo.buf.emit(&[0x41, 0x5d, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
                    lo.buf.finalize();
                    let mut frame =
                        vec![0x12345678_u64; slot.max(a_slot).max(b_slot) as usize / 8 + 16];
                    let a_word = frame.len() - a_slot as usize / 8;
                    let b_word = frame.len() - b_slot as usize / 8;
                    frame[a_word] = av as u64;
                    frame[b_word] = bv as u64;
                    let result_word = frame.len() - slot as usize / 8;
                    let mut expected = frame.clone();
                    expected[result_word] = if wide {
                        want as u64
                    } else {
                        u64::from(want as u32)
                    };
                    // SAFETY: the leaf preserves callee-saved registers and
                    // writes only the result home inside the supplied buffer.
                    let f: unsafe extern "C" fn(*mut u64) -> i64 =
                        unsafe { std::mem::transmute(lo.buf.as_ptr()) };
                    let got = unsafe { f(frame.as_mut_ptr().add(frame.len())) };
                    assert_eq!(
                        if wide { got } else { i64::from(got as i32) },
                        if wide { want } else { i64::from(want as i32) }
                    );
                    assert_eq!(frame, expected, "{op:?}: exact deopt home image");
                }
            }
        }
    }
}

#[test]
fn allocated_integer_results_do_not_need_the_opt_in_home_elision_allocator() {
    check_allocated_results(true, true);
}

#[test]
fn allocated_integer_results_accept_frame_operands() {
    for (a_live, b_live) in [(false, true), (true, false), (false, false)] {
        check_allocated_results(a_live, b_live);
    }
}

#[test]
fn directly_selected_single_use_values_pay_in_the_caller_saved_file() {
    cratonvm_types::flags::with_thread_overrides(
        &[
            ("CRATONVM_JIT_IR_REG_ALLOC", Some("0")),
            ("CRATONVM_JIT_IR_OPERAND_IN_PLACE", Some("1")),
        ],
        || {
            let mut graph = Graph::empty(32);
            let start = graph.add(Op::Start, IrType::Control, vec![], None);
            graph.entry = start;
            let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
            let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
            let mut terms = Vec::new();
            for k in 1..=4 {
                let k = graph.add(Op::Const(k), IrType::Int, vec![], None);
                terms.push(graph.add(Op::Add, IrType::Int, vec![a, k], None));
            }
            let left = graph.add(Op::Mul, IrType::Int, vec![terms[0], terms[1]], None);
            let right = graph.add(Op::Mul, IrType::Int, vec![terms[2], terms[3]], None);
            let value = graph.add(Op::Xor, IrType::Int, vec![left, right], None);
            graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, value], None);
            let schedule = crate::ir_schedule::schedule(&graph);
            let plan = plan_slots(&graph, &schedule, None);
            let mut live = crate::regalloc::build_live_model(&graph, &schedule);
            let uses = caller_saved_use_counts(&graph);
            let candidates: Vec<_> = (0..graph.nodes.len())
                .filter(|&id| {
                    r25_w7a_reg_alloc::single_use_candidate(&graph, &schedule, id, uses[id])
                })
                .collect();
            assert!(
                !candidates.is_empty(),
                "fixture has a single-use value outside the carry window"
            );
            let residency =
                plan_register_residency_with(&graph, &schedule, &plan, &mut live, false, None)
                    .expect("residency plan")
                    .expect("register allocation enabled");
            assert!(
                candidates.iter().any(|&id| residency.gp_reg_of[id]
                    .is_some_and(|r| IR_LS_CALLER_SAVED_GPRS.contains(&r))),
                "a directly selected definition has no publish cost to amortize"
            );
            let cm = lower_inner(
                &graph,
                &schedule,
                1,
                1,
                &JitRuntimeHelpers::default(),
                &HashMap::new(),
                &[],
                None,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &crate::ir::IrInlineFrameSites::default(),
            )
            .expect("lower");
            for n in [-17_i32, 0, 1, 57, i32::MIN, i32::MAX] {
                let want = n.wrapping_add(1).wrapping_mul(n.wrapping_add(2))
                    ^ n.wrapping_add(3).wrapping_mul(n.wrapping_add(4));
                // SAFETY: a pure int f(int), without helpers or safepoints.
                assert_eq!(
                    unsafe { cm.try_call(&[i64::from(n)]) }.expect("call") as i32,
                    want
                );
            }
        },
    );
}

#[test]
fn allocated_array_length_reads_the_resident_receiver_in_place() {
    let mut builder = IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    let graph = Box::leak(Box::new(
        builder.build(&[0x2a, 0xbe, 0xac], 3).expect("arraylength IR"),
    ));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let id = graph.nodes.iter().position(|n| n.op == Op::ArrayLength).unwrap() as NodeId;
    let array = graph.nodes[id as usize].inputs[2];
    
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
    lo.reg_alloc_on = true;
    lo.gp_reg_of = vec![None; graph.nodes.len()];
    lo.gp_reg_live = vec![false; graph.nodes.len()];
    lo.gp_reg_of[array as usize] = Some(13);
    lo.gp_reg_of[id as usize] = Some(12);
    lo.mark_gp_reg_live(array);
    lo.alloc_slot(array);
    lo.cur_def = Some(id);
    lo.lower_data_node(id);
    let disp = crate::x64::disp::disp8_const(ARRAY_LENGTH_OFFSET as i64) as u8;
    let code = lo.buf.as_slice();
    assert_eq!(&code[..3], &[0x4d, 0x85, 0xed], "TEST must read R13 directly");
    assert!(code.windows(4).any(|w| w == [0x45, 0x8b, 0x65, disp]),
        "MOV must load the length from R13 into R12D");
    assert!(lo.cur_def_published);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn allocated_string_decode_uses_the_resident_array_for_both_coders() {
    let mut graph = Graph::empty(8);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let value = graph.add(Op::Param(0), IrType::Ref, vec![start], None);
    let index = graph.add(Op::Param(1), IrType::Int, vec![start], None);
    let coder = graph.add(Op::Param(2), IrType::Int, vec![start], None);
    let ch = graph.add(
        Op::StrCharDecode,
        IrType::Int,
        vec![ctrl, mem, value, index, coder],
        Some(0),
    );
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, ch], Some(1));
    let graph = Box::leak(Box::new(graph));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
    lo.reg_alloc_on = true;
    lo.gp_reg_of = vec![None; graph.nodes.len()];
    lo.gp_reg_live = vec![false; graph.nodes.len()];
    lo.gp_reg_of[value as usize] = Some(13);
    lo.gp_reg_of[ch as usize] = Some(12);
    lo.mark_gp_reg_live(value);
    let value_slot = lo.alloc_slot(value);
    let index_slot = lo.alloc_slot(index);
    let coder_slot = lo.alloc_slot(coder);
    lo.buf.emit(&[0x55, 0x41, 0x54, 0x41, 0x55]); // Save RBP, R12 and R13.
    lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]); // RBP is the supplied frame.
    lo.emit_mov_reg_reg64(13, ENTRY_ABI_REGS[1]);
    lo.cur_def = Some(ch);
    lo.lower_data_node(ch);
    let result_slot = lo.home_offset_of(ch).expect("char home");
    let code = lo.buf.as_slice();
    for (opcode, sib) in [(0xb6, 0x0d), (0xb7, 0x4d)] {
        assert!(
            code.windows(6).any(|w| w == [0x45, 0x0f, opcode, 0x64, sib, ARRAY_DATA_DISP8]),
            "char load must address R13 and the zero-extended RCX index"
        );
    }
    assert!(lo.cur_def_published);
    lo.emit_mov_reg_reg64(RAX, 12);
    lo.buf.emit(&[0x41, 0x5d, 0x41, 0x5c, 0x5d, 0xc3]);
    lo.buf.finalize();
    let words = [value_slot, index_slot, coder_slot, result_slot]
        .into_iter().max().expect("frame slots") as usize / 8 + 16;
    let mut frame = vec![0u64; words];
    for (coder, bytes, expect) in [(0, [0, 0xe9, 0, 0], 0xe9), (1, [0, 0, 0x00, 0xd8], 0xd800)] {
        let array = crate::tests::synthetic_array(1, &bytes.map(i64::from));
        frame[words - index_slot as usize / 8] = 0xaced_feed_0000_0001;
        frame[words - coder_slot as usize / 8] = coder;
        // SAFETY: the leaf preserves the saved registers, accesses the supplied
        // frame words, and reads one in-bounds char from the synthetic array.
        let decode: unsafe extern "C" fn(*mut u64, *const u8) -> i64 =
            unsafe { std::mem::transmute(lo.buf.as_ptr()) };
        let actual = unsafe { decode(frame.as_mut_ptr().add(words), array.as_ptr()) };
        assert_eq!(actual as u32, expect);
        assert_eq!(frame[words - result_slot as usize / 8] as u32, expect);
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn allocated_array_load_uses_the_resident_base_through_checks_and_load() {
    for (kind, opcode, width, value, expected) in [
        (MemKind::Int, 0x2e, 4, -17_i64, -17_i64),
        (MemKind::Long, 0x2f, 8, -0x1234_5678_9_i64, -0x1234_5678_9_i64),
        (MemKind::Byte, 0x33, 1, -17, -17),
        (MemKind::Char, 0x34, 2, -17, 65519),
        (MemKind::Short, 0x35, 2, -17, -17),
    ] {
    for base in [3, 12, 13, 14, 15] {
        for dst in [Some(12), Some(13), None] {
            let mut builder = IrBuilder::new(2, 2);
            builder.set_param_types(&[IrType::Ref, IrType::Int]);
            let graph = Box::leak(Box::new(
                builder.build(&[0x2a, 0x1b, opcode, if width == 8 { 0xad } else { 0xac }], 4).expect("array load IR"),
            ));
            let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
            let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
            let id = graph.nodes.iter().position(|n| n.op == Op::ArrayLoad(kind))
                .expect("array load") as NodeId;
            let array = graph.nodes[id as usize].inputs[2];
            let index = graph.nodes[id as usize].inputs[3];
            
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
            lo.reg_alloc_on = true;
            lo.gp_reg_of = vec![None; graph.nodes.len()];
            lo.gp_reg_live = vec![false; graph.nodes.len()];
            lo.gp_reg_of[array as usize] = Some(base);
            lo.gp_reg_of[id as usize] = dst;
            lo.mark_gp_reg_live(array);
            let array_slot = lo.alloc_slot(array);
            let index_slot = lo.alloc_slot(index);
            lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57]);
            lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
            lo.emit_mov_reg_reg64(base, ENTRY_ABI_REGS[1]);
            let start = lo.buf.pos();
            lo.cur_def = Some(id);
            lo.lower_data_node(id);
            let code = &lo.buf.as_slice()[start..];
            let rex = 0x48 | if base >= 8 { 5 } else { 0 };
            let test = [rex, 0x85, 0xc0 | ((base & 7) << 3) | (base & 7)];
            assert!(code.windows(3).any(|w| w == test), "null guard must use r{base}");
            if dst.is_some() { assert!(lo.cur_def_published); }
            let result_slot = lo.home_offset_of(id).expect("result home");
            if let Some(dst) = dst { lo.emit_mov_reg_reg64(RAX, dst); }
            lo.buf.emit(&[0x41, 0x5f, 0x41, 0x5e, 0x41, 0x5d, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
            lo.buf.finalize();
            let words = array_slot.max(index_slot).max(result_slot) as usize / 8 + 16;
            let mut frame = vec![0u64; words];
            frame[words - index_slot as usize / 8] = 0xaced_feed_0000_0001;
            let array = crate::tests::synthetic_array(width, &[1, value, 45]);
            frame[words - array_slot as usize / 8] = array.as_ptr() as u64;
            // SAFETY: the leaf preserves nonvolatile registers, uses the supplied
            // frame and reads index 1 of a synthetic array with three elements.
            let read: unsafe extern "C" fn(*mut u64, *const u8) -> i64 =
                unsafe { std::mem::transmute(lo.buf.as_ptr()) };
            let actual = unsafe { read(frame.as_mut_ptr().add(words), array.as_ptr()) };
            assert_eq!(actual, expected, "{kind:?} base r{base}, dst {dst:?}");
            assert_eq!(frame[words - result_slot as usize / 8] as i64, expected);
        }
    }
}
}

#[test]
#[cfg(target_arch = "x86_64")]
fn allocated_array_index_shares_the_checked_register_for_load_and_decode() {
    for (poison, expect_resident) in [(false, true), (true, false)] {
        let mut graph = Graph::empty(8);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let array = graph.add(Op::Param(0), IrType::Ref, vec![start], None);
        let index = graph.add(Op::Param(1), IrType::Int, vec![start], None);
        let coder = graph.add(Op::Param(2), IrType::Int, vec![start], None);
        let load = graph.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, array, index],
            Some(0),
        );
        let ref_load = graph.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![ctrl, mem, array, index],
            Some(0),
        );
        let decode = graph.add(
            Op::StrCharDecode,
            IrType::Int,
            vec![ctrl, mem, array, index, coder],
            Some(1),
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, load], Some(2));
        let graph = Box::leak(Box::new(graph));
        let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
        let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        for node in [load, ref_load, decode] {
            if node == ref_load
                && (narrow_oops_enabled() || crate::x64::zgc_read_barrier_blocks_inline_fields())
            {
                continue;
            }
            
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
            lo.reg_alloc_on = true;
            lo.gp_reg_of = vec![None; graph.nodes.len()];
            lo.gp_reg_live = vec![false; graph.nodes.len()];
            lo.gp_reg_of[array as usize] = Some(13);
            lo.gp_reg_of[index as usize] = Some(3);
            if node != ref_load {
                lo.gp_reg_of[node as usize] = Some(12);
            }
            lo.mark_gp_reg_live(array);
            lo.mark_gp_reg_live(index);
            if !poison {
                lo.gp_zx32_mask |= 1u16 << 3;
            }
            let array_slot = lo.alloc_slot(array);
            let index_slot = lo.alloc_slot(index);
            let coder_slot = lo.alloc_slot(coder);
            lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x41, 0x55]);
            lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
            lo.emit_mov_reg_reg64(13, ENTRY_ABI_REGS[1]);
            lo.emit_mov_reg_imm64(3, if poison { 0xdead_beef_0000_0001 } else { 1 });
            let start_pos = lo.buf.pos();
            lo.cur_def = Some(node);
            lo.lower_data_node(node);
            let code = &lo.buf.as_slice()[start_pos..];
            let disp = crate::x64::disp::disp8_const(ARRAY_LENGTH_OFFSET as i64) as u8;
            if node == load {
                let cmp_rbx = code.windows(4).any(|w| w == [0x41, 0x3b, 0x5d, disp]);
                let load_rbx = code.windows(5).any(|w| w == [0x4d, 0x63, 0x64, 0x9d, ARRAY_DATA_DISP8]);
                assert_eq!(cmp_rbx && load_rbx, expect_resident, "ArrayLoad poison={poison}");
            } else if node == ref_load {
                let cmp = [0x41, 0x3b, if poison { 0x4d } else { 0x5d }, disp];
                let read = [
                    0x49, 0x8b, 0x44, if poison { 0xcd } else { 0xdd }, ARRAY_DATA_DISP8,
                ];
                assert!(code.windows(cmp.len()).any(|w| w == cmp), "aaload bounds index");
                assert!(code.windows(read.len()).any(|w| w == read), "aaload address");
            } else {
                let byte_rbx = code.windows(6).any(|w| w == [0x45, 0x0f, 0xb6, 0x64, 0x1d, ARRAY_DATA_DISP8]);
                let char_rbx = code.windows(6).any(|w| w == [0x45, 0x0f, 0xb7, 0x64, 0x5d, ARRAY_DATA_DISP8]);
                assert_eq!(byte_rbx && char_rbx, expect_resident, "StrCharDecode poison={poison}");
            }
            if node != ref_load {
                lo.emit_mov_reg_reg64(RAX, 12);
            }
            lo.buf.emit(&[0x41, 0x5d, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
            lo.buf.finalize();
            let result_slot = lo.home_offset_of(node).expect("result home");
            let words = [array_slot, index_slot, coder_slot, result_slot]
                .into_iter().max().expect("frame slots") as usize / 8 + 16;
            let mut frame = vec![0u64; words];
            frame[words - index_slot as usize / 8] = 0xdead_beef_0000_0001;
            frame[words - coder_slot as usize / 8] = 1;
            let referent = [17u64];
            let (width, expected) = if node == ref_load {
                (8, referent.as_ptr() as i64)
            } else if node == load {
                (4, 0x1234)
            } else {
                (2, 0x1234)
            };
            let syn = crate::tests::synthetic_array(width, &[11, expected, 33]);
            frame[words - array_slot as usize / 8] = syn.as_ptr() as u64;
            // SAFETY: leaf preserves nonvolatile registers and reads index 1 of synthetic array.
            let run: unsafe extern "C" fn(*mut u64, *const u8) -> i64 =
                unsafe { std::mem::transmute(lo.buf.as_ptr()) };
            let actual = unsafe { run(frame.as_mut_ptr().add(words), syn.as_ptr()) };
            assert_eq!(actual, expected);
            assert_eq!(frame[words - result_slot as usize / 8], expected as u64);
        }
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn allocated_array_store_uses_resident_base_index_and_source() {
    for (kind, width, val, expected_byte_arr, expected_bool_arr) in [
        (MemKind::Int, 4, -17_i64, -17_i64, -17_i64),
        (MemKind::Long, 8, -0x1234_5678_9_i64, -0x1234_5678_9_i64, -0x1234_5678_9_i64),
        (MemKind::Byte, 1, 3_i64, 3_i64, 1_i64),
        (MemKind::Char, 2, -17_i64, 65519_i64, 65519_i64),
        (MemKind::Short, 2, -17_i64, -17_i64, -17_i64),
    ] {
        for val_reg in [12u8, 7u8] {
            let mut graph = Graph::empty(8);
            let start = graph.add(Op::Start, IrType::Control, vec![], None);
            let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
            let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
            let array = graph.add(Op::Param(0), IrType::Ref, vec![start], None);
            let index = graph.add(Op::Param(1), IrType::Int, vec![start], None);
            let val_ty = if kind == MemKind::Long { IrType::Long } else { IrType::Int };
            let value = graph.add(Op::Param(2), val_ty, vec![start], None);
            let store = graph.add(
                Op::ArrayStore(kind),
                IrType::Memory,
                vec![ctrl, mem, array, index, value],
                Some(0),
            );
            graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl], Some(1));
            let graph = Box::leak(Box::new(graph));
            let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
            let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
            
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
            lo.reg_alloc_on = true;
            lo.gp_reg_of = vec![None; graph.nodes.len()];
            lo.gp_reg_live = vec![false; graph.nodes.len()];
            lo.gp_reg_of[array as usize] = Some(13);
            lo.gp_reg_of[index as usize] = Some(3);
            lo.gp_reg_of[value as usize] = Some(val_reg);
            lo.mark_gp_reg_live(array);
            lo.mark_gp_reg_live(index);
            lo.mark_gp_reg_live(value);
            lo.gp_zx32_mask |= 1u16 << 3;
            let array_slot = lo.alloc_slot(array);
            let index_slot = lo.alloc_slot(index);
            let value_slot = lo.alloc_slot(value);
            lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x41, 0x55]);
            lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
            lo.emit_mov_reg_reg64(13, ENTRY_ABI_REGS[1]);
            lo.emit_mov_reg_imm64(3, 1);
            lo.emit_mov_reg_imm64(val_reg, val as u64);
            let start_pos = lo.buf.pos();
            lo.cur_def = Some(store);
            lo.lower_data_node(store);
            let code = &lo.buf.as_slice()[start_pos..];
            let disp = crate::x64::disp::disp8_const(ARRAY_LENGTH_OFFSET as i64) as u8;
            assert!(
                code.windows(3).any(|w| w == [0x4d, 0x85, 0xed]),
                "ArrayStore null guard must test R13 directly"
            );
            assert!(
                code.windows(4).any(|w| w == [0x41, 0x3b, 0x5d, disp]),
                "ArrayStore bounds guard must compare EBX against [R13 + disp]"
            );
            if kind != MemKind::Byte {
                assert_eq!(lo.gp_reg_reads, 3, "{kind:?} must read base, index, and value from registers");
            } else {
                let kind_disp = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8;
                let bool_tag = cratonvm_types::primitive_array_kind_tags_byte("[Z").unwrap();
                assert!(
                    code.windows(5).any(|w| w == [0x41, 0x80, 0x7d, kind_disp, bool_tag]),
                    "Byte ArrayStore boolean tag check must address R13"
                );
            }
            lo.emit_mov_reg_reg64(RAX, val_reg);
            lo.buf.emit(&[0x41, 0x5d, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
            lo.buf.finalize();
            let words = array_slot.max(index_slot).max(value_slot) as usize / 8 + 16;
            let mut frame = vec![0u64; words];
            frame[words - index_slot as usize / 8] = 1;
            frame[words - value_slot as usize / 8] = val as u64;
            for (is_bool, expected) in [(false, expected_byte_arr), (true, expected_bool_arr)] {
                if is_bool && kind != MemKind::Byte {
                    continue;
                }
                let mut syn = crate::tests::synthetic_array(width, &[0, 0, 0]);
                if is_bool {
                    syn[cratonvm_types::KIND_TAGS_BYTE_OFFSET] = cratonvm_types::primitive_array_kind_tags_byte("[Z").unwrap();
                }
                frame[words - array_slot as usize / 8] = syn.as_ptr() as u64;
                // SAFETY: leaf preserves nonvolatile registers and writes index 1 of synthetic array.
                let run: unsafe extern "C" fn(*mut u64, *mut u8) -> i64 =
                    unsafe { std::mem::transmute(lo.buf.as_ptr()) };
                let preserved_val = unsafe { run(frame.as_mut_ptr().add(words), syn.as_mut_ptr()) };
                assert_eq!(preserved_val, val, "resident value register must not be clobbered by boolean mask");
                let data_off = HEADER_SIZE + width;
                let stored = match kind {
                    MemKind::Int => i32::from_le_bytes(syn[data_off..data_off + 4].try_into().unwrap()) as i64,
                    MemKind::Long => i64::from_le_bytes(syn[data_off..data_off + 8].try_into().unwrap()),
                    MemKind::Byte => (syn[data_off] as i8) as i64,
                    MemKind::Char => u16::from_le_bytes(syn[data_off..data_off + 2].try_into().unwrap()) as i64,
                    MemKind::Short => i16::from_le_bytes(syn[data_off..data_off + 2].try_into().unwrap()) as i64,
                    _ => unreachable!(),
                };
                assert_eq!(stored, expected, "{kind:?} val_reg={val_reg} is_bool={is_bool}");
            }
        }
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn allocated_prim_putfield_uses_resident_receiver_and_source_without_dead_helper() {
    unsafe extern "C" fn fake_putfield_int(_obj: i64, _idx: i64, _val: i64) {}
    let mut b = IrBuilder::new(2, 4);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    let mut fi = HashMap::new();
    fi.insert(2usize, (0usize, b'I'));
    fi.insert(7usize, (1usize, b'I'));
    b.set_field_info(fi);
    let code = [0x2a, 0x1b, 0xb5, 0x00, 0x02, 0x2a, 0x1b, 0xb5, 0x00, 0x03, 0xb1, 0, 0];
    let mut graph = b.build(&code, 11).expect("two int putfields build");
    graph.proven_compact_withdrawn_pcs = Some(std::collections::HashSet::new());
    let graph = Box::leak(Box::new(graph));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let stores: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| matches!(n.op, Op::Store(MemKind::Int)).then_some(i as NodeId))
        .collect();
    assert_eq!(stores.len(), 2);
    let base = graph.nodes[stores[0] as usize].inputs[2];
    let val = graph.nodes[stores[0] as usize].inputs[4];
    
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
    lo.reg_alloc_on = true;
    lo.putfield_int = fake_putfield_int as *const () as usize;
    let table: &'static [u64; 6] = Box::leak(Box::new([0u64; 6]));
    lo.read_bounds_addr = table.as_ptr() as usize;
    lo.compact_fields.insert((2usize, false), (8u32, false, b'I'));
    lo.compact_fields.insert((7usize, false), (12u32, false, b'I'));
    lo.gp_reg_of = vec![None; graph.nodes.len()];
    lo.gp_reg_live = vec![false; graph.nodes.len()];
    lo.gp_reg_of[base as usize] = Some(13);
    lo.gp_reg_of[val as usize] = Some(12);
    lo.mark_gp_reg_live(base);
    lo.mark_gp_reg_live(val);
    lo.alloc_slot(base);
    lo.alloc_slot(val);
    let start_pos = lo.buf.pos();
    for &s in &stores {
        lo.cur_def = Some(s);
        lo.lower_data_node(s);
    }
    let emitted = &lo.buf.as_slice()[start_pos..];
    assert!(
        !emitted.windows(2).any(|w| w == [0xff, 0xd0]),
        "proven compact primitive putfield must not emit a dead helper CALL"
    );
    assert!(
        emitted.windows(7).any(|w| w == [0x45, 0x89, 0xa5, 0x08, 0x00, 0x00, 0x00]),
        "first store must write [R13 + 8] directly from R12D"
    );
    assert!(
        emitted.windows(7).any(|w| w == [0x45, 0x89, 0xa5, 0x0c, 0x00, 0x00, 0x00]),
        "second store must write [R13 + 12] directly from R12D"
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn allocated_compact_ref_and_subword_getfield_uses_resident_receiver() {
    let mut b = IrBuilder::new(2, 4);
    b.set_param_types(&[IrType::Ref]);
    let mut fi = HashMap::new();
    fi.insert(1usize, (0usize, b'L'));
    fi.insert(6usize, (1usize, b'S'));
    b.set_field_info(fi);
    let code = [0x2a, 0xb4, 0x00, 0x02, 0x57, 0x2a, 0xb4, 0x00, 0x03, 0xac];
    let mut graph = b.build(&code, 10).expect("ref and short getfield build");
    graph.proven_compact_withdrawn_pcs = Some(std::collections::HashSet::new());
    let graph = Box::leak(Box::new(graph));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let loads: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| matches!(n.op, Op::Load(_)).then_some(i as NodeId))
        .collect();
    assert_eq!(loads.len(), 2);
    let base = graph.nodes[loads[0] as usize].inputs[2];
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
    lo.reg_alloc_on = true;
    unsafe extern "C" fn fake_getfield(_obj: i64, _idx: i64) -> i64 { 0 }
    lo.getfield = fake_getfield as *const () as usize;
    let table: &'static [u64; 6] = Box::leak(Box::new([0u64; 6]));
    lo.read_bounds_addr = table.as_ptr() as usize;
    lo.compact_fields.insert((1usize, true), (8u32, true, b'L'));
    lo.compact_fields.insert((6usize, false), (16u32, false, b'S'));
    lo.gp_reg_of = vec![None; graph.nodes.len()];
    lo.gp_reg_live = vec![false; graph.nodes.len()];
    lo.gp_reg_of[base as usize] = Some(13);
    lo.mark_gp_reg_live(base);
    lo.alloc_slot(base);
    let start_pos = lo.buf.pos();
    
    for &l in &loads {
        lo.cur_def = Some(l);
        lo.lower_data_node(l);
    }
    let emitted = &lo.buf.as_slice()[start_pos..];
    // Ref read: MOV RAX, [R13 + 8] -> [0x49, 0x8b, 0x45, 0x08] or [0x49, 0x8b, 0x85, 0x08, 0x00, 0x00, 0x00]
    assert!(
        emitted.windows(4).any(|w| w == [0x49, 0x8b, 0x45, 0x08])
            || emitted.windows(7).any(|w| w == [0x49, 0x8b, 0x85, 0x08, 0x00, 0x00, 0x00]),
        "ref load must read from [R13 + 8]"
    );
    // Short read: MOVSX EAX, word [R13 + 16] -> [0x66, 0x41, 0x0f, 0xbf, ...] or [0x41, 0x0f, 0xbf, ...]
    assert!(
        emitted.windows(3).any(|w| w == [0x0f, 0xbf, 0x45])
            || emitted.windows(3).any(|w| w == [0x0f, 0xbf, 0x85]),
        "short load must read from R13"
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn allocated_legacy_putfield_uses_resident_receiver_and_value() {
    unsafe extern "C" fn fake_putfield_int(_obj: i64, _idx: i64, _val: i64) {}
    let mut b = IrBuilder::new(2, 4);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    let mut fi = HashMap::new();
    fi.insert(2usize, (0usize, b'I'));
    b.set_field_info(fi);
    let code = [0x2a, 0x1b, 0xb5, 0x00, 0x02, 0xb1];
    let mut graph = b.build(&code, 6).expect("int putfield builds");
    graph.proven_compact_withdrawn_pcs = Some(std::collections::HashSet::new());
    let graph = Box::leak(Box::new(graph));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let store = graph
        .nodes
        .iter()
        .enumerate()
        .find_map(|(i, n)| matches!(n.op, Op::Store(MemKind::Int)).then_some(i as NodeId))
        .expect("putfield store node");
    let base = graph.nodes[store as usize].inputs[2];
    let val = graph.nodes[store as usize].inputs[4];
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
    lo.reg_alloc_on = true;
    lo.putfield_int = fake_putfield_int as *const () as usize;
    lo.gp_reg_of = vec![None; graph.nodes.len()];
    lo.gp_reg_live = vec![false; graph.nodes.len()];
    lo.gp_reg_of[base as usize] = Some(13);
    lo.gp_reg_of[val as usize] = Some(12);
    lo.mark_gp_reg_live(base);
    lo.mark_gp_reg_live(val);
    lo.alloc_slot(base);
    lo.alloc_slot(val);
    let start_pos = lo.buf.pos();
    lo.cur_def = Some(store);
    lo.lower_data_node(store);
    let emitted = &lo.buf.as_slice()[start_pos..];
    // Resident receiver in R13 used directly: MOV RAX, R13 -> [0x4c, 0x89, 0xe8]
    assert!(
        emitted.windows(3).any(|w| w == [0x4c, 0x89, 0xe8]),
        "legacy store must use resident receiver in R13"
    );
    // Resident value in R12 used directly: MOV RDX, R12 -> [0x4c, 0x89, 0xe2]
    assert!(
        emitted.windows(3).any(|w| w == [0x4c, 0x89, 0xe2]),
        "legacy store must use resident value in R12"
    );
    // Legacy word 0 store: MOV [RAX + 16], RCX -> [0x48, 0x89, 0x88, 0x10]
    assert!(
        emitted.windows(4).any(|w| w == [0x48, 0x89, 0x88, 0x10]),
        "legacy store must write packed tag/payload to [RAX + 16]"
    );
    // Legacy word 1 zeroing: MOV [RAX + 24], RCX -> [0x48, 0x89, 0x88, 0x18]
    assert!(
        emitted.windows(4).any(|w| w == [0x48, 0x89, 0x88, 0x18]),
        "legacy store must clear high qword at [RAX + 24]"
    );
    // On helper path: receiver in R13 moved directly to RDI: MOV RDI, R13 -> [0x4c, 0x89, 0xef]
    assert!(
        emitted.windows(3).any(|w| w == [0x4c, 0x89, 0xef]),
        "helper path must use resident receiver R13 directly into RDI"
    );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn allocated_fp_array_access_uses_resident_base_and_index() {
    let mut b = IrBuilder::new(3, 4);
    b.set_param_types(&[IrType::Ref, IrType::Int, IrType::Float]);
    let code = [0x2a, 0x1b, 0x30, 0x2a, 0x1b, 0x23, 0x51, 0xb1];
    let mut graph = b.build(&code, 8).expect("faload and fastore build");
    graph.proven_compact_withdrawn_pcs = Some(std::collections::HashSet::new());
    let graph = Box::leak(Box::new(graph));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let load = graph
        .nodes
        .iter()
        .enumerate()
        .find_map(|(i, n)| matches!(n.op, Op::ArrayLoad(MemKind::Float)).then_some(i as NodeId))
        .expect("faload node");
    let store = graph
        .nodes
        .iter()
        .enumerate()
        .find_map(|(i, n)| matches!(n.op, Op::ArrayStore(MemKind::Float)).then_some(i as NodeId))
        .expect("fastore node");
    let array = graph.nodes[load as usize].inputs[2];
    let index = graph.nodes[load as usize].inputs[3];
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
    lo.reg_alloc_on = true;
    lo.gp_reg_of = vec![None; graph.nodes.len()];
    lo.gp_reg_live = vec![false; graph.nodes.len()];
    lo.gp_reg_owner = [None; 16];
    lo.gp_reg_of[array as usize] = Some(13);
    lo.gp_reg_of[index as usize] = Some(12);
    lo.mark_gp_reg_live(array);
    lo.mark_gp_reg_live(index);
    lo.gp_reg_owner[12] = Some(index);
    lo.gp_zx32_mask |= 1u16 << 12;
    lo.alloc_slot(array);
    lo.alloc_slot(index);
    let start_pos = lo.buf.pos();
    lo.cur_def = Some(load);
    lo.lower_data_node(load);
    let emitted_load = &lo.buf.as_slice()[start_pos..];
    // SIB for scale 4 (*4 = 0b10 << 6 = 0x80), index 12 (12 & 7 = 4, << 3 = 0x20), base 13 (13 & 7 = 5):
    // 0x80 | 0x20 | 0x05 = 0xa5.
    // REX prefix has rex_x (index 12 >= 8 -> 2) | rex_b (base 13 >= 8 -> 1) = 0x43.
    // [0xF3, 0x43, 0x0F, 0x10, 0x44, 0xA5, 0x10]
    assert!(
        emitted_load.windows(7).any(|w| w == [0xF3, 0x43, 0x0F, 0x10, 0x44, 0xA5, 0x10]),
        "faload must emit MOVSS XMM0, [R13 + R12*4 + 16]"
    );

    let start_store = lo.buf.pos();
    lo.cur_def = Some(store);
    lo.lower_data_node(store);
    let emitted_store = &lo.buf.as_slice()[start_store..];
    assert!(
        emitted_store.windows(7).any(|w| w == [0xF3, 0x43, 0x0F, 0x11, 0x44, 0xA5, 0x10]),
        "fastore must emit MOVSS [R13 + R12*4 + 16], XMM0"
    );
}

#[test]
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
fn allocated_selected_checked_array_alu_executes_in_place() {
    use crate::x64::isel::{AddrSource, IrAddr, MInst, Op as MachineOp, Rule, Ty};

    for (wide, load_op, alu_op, ret_op, lhs_op, elem_ty, m_op, m_ty, width, sib_expected, lhs_val, elem_val, expected) in [
        (false, 0x2e, 0x60, 0xac, 0x1c, IrType::Int, MachineOp::Add, Ty::I32, 4usize, 0x9d_u8, 100i64, 25i64, 125i64),
        (true, 0x2f, 0x65, 0xad, 0x20, IrType::Long, MachineOp::Sub, Ty::I64, 8usize, 0xdd_u8, 1000i64, 420i64, 580i64),
    ] {
        let bytecode = [
            0x2a, 0xc6, 0, 13, 0x1b, 0x9b, 0, 9, 0x1b, 0x2a, 0xbe,
            0xa1, 0, 5, lhs_op, ret_op, lhs_op, 0x2a, 0x1b,
            load_op, alu_op, ret_op,
        ];
        let mut builder = IrBuilder::new(4, if wide { 5 } else { 4 });
        builder.set_param_types(&[IrType::Ref, IrType::Int, elem_ty]);
        let mut graph = builder.build(&bytecode, bytecode.len()).unwrap();
        crate::ea_ir_bridge::ir_prune_unconsumable_snapshots(&mut graph, &[]);
        let graph = Box::leak(Box::new(graph));
        let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
        let (load, load_node) = graph
            .nodes
            .iter()
            .enumerate()
            .find(|(_, n)| matches!(n.op, Op::ArrayLoad(_)))
            .unwrap();
        let load = load as NodeId;
        let array = load_node.inputs[2];
        let index = load_node.inputs[3];
        let (result, alu_node) = graph
            .nodes
            .iter()
            .enumerate()
            .find(|(_, n)| matches!(n.op, Op::Add | Op::Sub))
            .unwrap();
        let result = result as NodeId;
        let lhs = alu_node.inputs[0];

        let addr = IrAddr {
            base: Some(array),
            index: Some(index),
            scale: width as u8,
            disp: HEADER_SIZE as i64,
        };
        let instruction = MInst::AluRM {
            op: m_op,
            ty: m_ty,
            dst: result,
            lhs,
            addr: AddrSource::Expr(addr),
            load,
        };
        let checks = crate::ir_check_elim::analyze(graph, schedule);
        let covered = vec![load, result];
        let flow = crate::regalloc::ValueFlow::new_with_checks(
            graph,
            schedule,
            vec![crate::regalloc::ValueGroup {
                instructions: vec![instruction.clone()],
                covered: covered.clone(),
            }],
            &[],
            Some(&checks),
        )
        .unwrap();
        let plan = Box::leak(Box::new(
            plan_slots_with_value_flow(graph, schedule, None, &flow).0,
        ));
        let mut machine = build_mir_plan(graph, schedule).expect("machine plan");
        let (block, tile) = machine.tile_of[result as usize].expect("alu tile");
        machine.blocks[block as usize].tiles[tile as usize] = crate::x64::isel::Tile::for_test(
            result,
            covered,
            vec![instruction],
            Rule::AluFoldedLoad,
        );

        let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
        lo.set_mir(machine, MirMode::Off);
        lo.value_flow = flow;
        lo.reg_alloc_on = true;
        lo.gp_reg_of = vec![None; graph.nodes.len()];
        lo.gp_reg_live = vec![false; graph.nodes.len()];
        lo.gp_reg_owner = [None; 16];
        // base in R13, index in RBX (3), lhs in R12, result in R12
        lo.gp_reg_of[array as usize] = Some(13);
        lo.gp_reg_of[index as usize] = Some(3);
        lo.gp_reg_of[lhs as usize] = Some(12);
        lo.gp_reg_of[result as usize] = Some(12);
        lo.mark_gp_reg_live(array);
        lo.mark_gp_reg_live(index);
        lo.mark_gp_reg_live(lhs);
        lo.gp_reg_owner[13] = Some(array);
        lo.gp_reg_owner[3] = Some(index);
        lo.gp_reg_owner[12] = Some(lhs);
        lo.gp_zx32_mask |= 1u16 << 3;

        let array_slot = lo.alloc_slot(array);
        let index_slot = lo.alloc_slot(index);
        let lhs_slot = lo.alloc_slot(lhs);
        let result_slot = lo.alloc_slot(result);

        lo.buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x41, 0x55]);
        lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
        lo.emit_mov_reg_reg64(13, ENTRY_ABI_REGS[1]);
        lo.emit_mov_reg_imm64(3, 1);
        lo.emit_mov_reg_imm64(12, lhs_val as u64);

        let start_pos = lo.buf.pos();
        lo.cur_def = Some(result);
        assert!(
            lo.reg_alloc_direct_alu(result, result_slot),
            "reg_alloc_direct_alu must emit AluRM with AddrSource::Expr"
        );
        let emitted = &lo.buf.as_slice()[start_pos..];
        assert!(
            emitted.windows(3).any(|w| w[1] == sib_expected && w[2] == HEADER_SIZE as u8),
            "expected indexed SIB {sib_expected:#04x} disp8={} in {emitted:02x?}",
            HEADER_SIZE
        );

        lo.emit_mov_reg_reg64(RAX, 12);
        lo.buf.emit(&[0x41, 0x5d, 0x41, 0x5c, 0x5b, 0x5d, 0xc3]);
        lo.buf.finalize();

        let words = array_slot.max(index_slot).max(lhs_slot).max(result_slot) as usize / 8 + 16;
        let mut frame = vec![0u64; words];
        let mut syn = crate::tests::synthetic_array(width, &[10, elem_val, -7]);
        frame[words - array_slot as usize / 8] = syn.as_ptr() as u64;
        frame[words - index_slot as usize / 8] = 1;
        frame[words - lhs_slot as usize / 8] = lhs_val as u64;

        // SAFETY: leaf preserves nonvolatile registers and reads index 1 of synthetic array.
        let run: unsafe extern "C" fn(*mut u64, *mut u8) -> i64 =
            unsafe { std::mem::transmute(lo.buf.as_ptr()) };
        let got = unsafe { run(frame.as_mut_ptr().add(words), syn.as_mut_ptr()) };
        assert_eq!(got, expected, "wide={wide}");
    }
}
