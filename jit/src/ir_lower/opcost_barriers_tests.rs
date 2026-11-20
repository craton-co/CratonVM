// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir::{CmpOp, Graph, IrType, MemKind, Op, SafepointSnapshot, NO_NODE};

fn empty_loop_field_graph() -> (&'static mut Graph, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    let entry_ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let entry_mem = g.add(Op::Proj(1), IrType::Memory, vec![start], Some(0));
    let p_in = g.add(Op::Param(0), IrType::Ref, vec![], Some(0));
    let off_x = g.add(Op::Const(16), IrType::Int, vec![], Some(0));
    let off_next = g.add(Op::Const(24), IrType::Int, vec![], Some(0));

    let hdr = g.add(
        Op::Region,
        IrType::Control,
        vec![entry_ctrl, NO_NODE],
        Some(4),
    );
    let mem_phi = g.add(
        Op::Phi,
        IrType::Memory,
        vec![hdr, entry_mem, NO_NODE],
        Some(4),
    );
    let p_phi = g.add(Op::Phi, IrType::Ref, vec![hdr, p_in, NO_NODE], Some(4));

    let cond_zero = g.add(Op::Const(0), IrType::Int, vec![], Some(4));
    let cond_one = g.add(Op::Const(1), IrType::Int, vec![], Some(4));
    let cmp = g.add(
        Op::Cmp(CmpOp::Ne),
        IrType::Int,
        vec![cond_one, cond_zero],
        Some(4),
    );
    let iff = g.add(Op::If, IrType::Control, vec![hdr, cmp], Some(4));
    let body_ctrl = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(7));
    let exit_ctrl = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(15));

    let load_x = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![body_ctrl, mem_phi, p_phi, off_x],
        Some(7),
    );
    let load_next = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![body_ctrl, load_x, p_phi, off_next],
        Some(10),
    );
    g.set_input(hdr, 1, body_ctrl);
    g.set_input(mem_phi, 2, load_next);
    g.set_input(p_phi, 2, load_next);
    let ret = g.add(Op::Return, IrType::Void, vec![exit_ctrl, mem_phi], Some(15));
    g.entry = start;
    g.exit = ret;
    g.push_safepoint(SafepointSnapshot::new(0, vec![p_in], vec![], vec![]));
    g.push_safepoint(SafepointSnapshot::new(7, vec![p_phi], vec![], vec![]));
    let g_leak = Box::leak(Box::new(g));
    (g_leak, p_in, p_phi, load_next)
}

#[test]
fn entry_epoch_hoist_decouples_from_receiver_and_covers_loop() {
    let (g, p_in, p_phi, load_next) = empty_loop_field_graph();
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
    let plan = Box::leak(Box::new(plan_slots(g, schedule, None)));
    let mut lo = super::tests::lowerer_over(g, schedule, plan, 16384);
    lo.layout_guard_deopt_on = true;
    lo.proven_scope_on = true;
    lo.proven_preheader_on = true;
    let body_block = schedule.node_to_block[load_next as usize];

    let before = lo.buf.pos();
    lo.emit_entry_layout_epoch_guard_if_needed();
    assert!(
        lo.buf.pos() > before,
        "entry epoch guard must be emitted once for loop-bearing method"
    );
    assert!(
        lo.layout_proven.contains(&(NO_NODE, 0)),
        "hoisted entry guard records decoupled NO_NODE proof in block 0"
    );
    assert!(
        lo.epoch_proof_holds(body_block),
        "entry layout-epoch proof must dominate loop body"
    );
    assert!(
        !lo.field_proof_holds(&lo.layout_proven, p_in, body_block, false),
        "receiver-keyed field_proof_holds remains false for NO_NODE entry proof"
    );

    let loop_before = lo.buf.pos();
    lo.cur_def = Some(load_next);
    lo.cur_bci = 10;
    lo.cur_node_pc = Some(10);
    assert!(lo.emit_field_layout_guard(p_phi, 10, true).is_none());
    assert_eq!(
        lo.buf.pos(),
        loop_before,
        "loop getfield must elide layout epoch compare once entry guard dominates"
    );
}

#[test]
fn dominating_instanceof_proves_non_null_only_on_true_branch() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], Some(0));
    let v = g.add(Op::Param(0), IrType::Ref, vec![], Some(0));
    let cls_name = b"java/lang/String";
    let inst = g.add(
        Op::InstanceOf {
            name_ptr: cls_name.as_ptr() as usize,
            name_len: cls_name.len(),
            seen_cell: 0,
            header_only: true,
        },
        IrType::Int,
        vec![ctrl, mem, v],
        Some(2),
    );
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, inst], Some(5));
    let then_ctrl = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(8));
    let else_ctrl = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(12));
    let off = g.add(Op::Const(16), IrType::Int, vec![], Some(8));
    let load_then = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![then_ctrl, mem, v, off],
        Some(8),
    );
    let load_else = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![else_ctrl, mem, v, off],
        Some(12),
    );
    let merge = g.add(
        Op::Merge,
        IrType::Control,
        vec![then_ctrl, else_ctrl],
        Some(15),
    );
    let mphi = g.add(
        Op::Phi,
        IrType::Memory,
        vec![merge, load_then, load_else],
        Some(15),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![merge, mphi], Some(15));
    g.entry = start;
    g.exit = ret;

    let g_leak = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g_leak)));
    let plan = Box::leak(Box::new(plan_slots(g_leak, schedule, None)));
    let lo = super::tests::lowerer_over(g_leak, schedule, plan, 16384);

    let then_blk = schedule.node_to_block[load_then as usize];
    let else_blk = schedule.node_to_block[load_else as usize];
    assert!(lo.dominating_instanceof_proves_non_null(v, then_blk));
    assert!(!lo.dominating_instanceof_proves_non_null(v, else_blk));
}

#[test]
fn zgc_plain_ref_guard_and_flags_match_expected_modes_and_bytes() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], Some(0));
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, mem], Some(0));
    g.entry = start;
    g.exit = ret;

    let g_leak = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g_leak)));
    let plan = Box::leak(Box::new(plan_slots(g_leak, schedule, None)));
    let mut lo = super::tests::lowerer_over(g_leak, schedule, plan, 16384);

    assert!(!lo.gen_bounds_always_live());
    lo.ref_store_post_skip_mask = Some(0x04);
    lo.read_bounds_addr = 0x1000;
    lo.zgc_fused_gate = None;
    assert!(lo.gen_bounds_always_live());

    lo.zgc_fused_gate = Some(0x2000);
    assert!(!lo.gen_bounds_always_live());
    assert_eq!(
        lo.zgc_plain_inline_ref_store_ok(),
        !cratonvm_types::narrow_oops_enabled()
    );

    const R12: u8 = 12;
    let mut slow = Vec::new();
    let p0 = lo.buf.pos();
    lo.emit_zgc_plain_ref_word_guard(RAX, &mut slow);
    let rax_bytes = &lo.buf.as_slice()[p0..];
    assert!(rax_bytes.len() >= 23);
    assert_eq!(
        &rax_bytes[rax_bytes.len() - 9..rax_bytes.len() - 4],
        &[0x48, 0x85, 0xC0, 0x0F, 0x88]
    );

    let p1 = lo.buf.pos();
    lo.emit_zgc_plain_ref_word_guard(R12, &mut slow);
    let r12_bytes = &lo.buf.as_slice()[p1..];
    assert!(r12_bytes.len() >= 23);
    assert_eq!(
        &r12_bytes[r12_bytes.len() - 9..r12_bytes.len() - 4],
        &[0x4D, 0x85, 0xE4, 0x0F, 0x88]
    );
    assert_eq!(slow.len(), 4);
}

#[test]
fn instanceof_final_miss_fast_emits_row_status_check_and_shared_word_enabled() {
    assert!(crate::typecheck_rows::typecheck_shared_class_word_enabled());

    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], Some(0));
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, mem], Some(0));
    g.entry = start;
    g.exit = ret;

    let g_leak = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g_leak)));
    let plan = Box::leak(Box::new(plan_slots(g_leak, schedule, None)));
    let mut lo = super::tests::lowerer_over(g_leak, schedule, plan, 16384);

    let mut slow = Vec::new();
    let mut zero = Vec::new();
    assert!(!lo.emit_instanceof_final_miss_fast_with_rows(0, None, &mut slow, &mut zero));
    assert!(!lo.emit_instanceof_final_miss_fast_with_rows(63, None, &mut slow, &mut zero));

    let fake_rows = 0x1000_0000usize;
    let start_pos = lo.buf.pos();
    assert!(lo.emit_instanceof_final_miss_fast_with_rows(
        fake_rows,
        Some("java/lang/String"),
        &mut slow,
        &mut zero,
    ));
    let bytes = &lo.buf.as_slice()[start_pos..];
    assert!(bytes.windows(2).any(|w| w == [0x8B, 0x08]));
    assert!(bytes
        .windows(3)
        .any(|w| w == [0xC1, 0xE1, crate::typecheck_rows::ROW_SHIFT]));
    assert!(bytes.windows(5).any(|w| {
        w == [
            0x41,
            0x80,
            0x3C,
            0x0B,
            crate::typecheck_rows::STATUS_HIERARCHY_ONLY as u8,
        ]
    }));
    assert_eq!(slow.len(), 2);
    assert_eq!(zero.len(), 1);
}

/// A baked whole-heap card view over `cards` (64 cards from `hull_lo`), no
/// summary, as `cratonvm_gc::card_table::JitCardView` lays it out.
#[repr(align(16))]
struct FakeHullView([usize; 32]);

fn fake_hull_view(hull_lo: usize, cards: &mut [u8]) -> Box<FakeHullView> {
    let mut view = Box::new(FakeHullView([0; 32]));
    view.0[0] = 0x5745_4956_4452_4143; // CARD_VIEW_MAGIC
    view.0[32 / 8] = 8; // CARD_VIEW_FLAG_WHOLE_HEAP
    view.0[152 / 8] = hull_lo; // CARD_VIEW_HULL_LO
    view.0[160 / 8] = cards.len() << cratonvm_types::CARD_SHIFT; // CARD_VIEW_HULL_LEN
    view.0[168 / 8] = (cards.as_mut_ptr() as usize).wrapping_neg(); // CARD_VIEW_HULL_BUF_NEG
    view
}

fn bare_lowerer() -> Lowerer<'static> {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    g.entry = start;
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl], Some(0));
    let g_leak = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g_leak)));
    let plan = Box::leak(Box::new(plan_slots(g_leak, schedule, None)));
    super::tests::lowerer_over(g_leak, schedule, plan, 16384)
}

/// w7: behind the `GC_FLAG_OLD_GEN` receiver test the whole-heap card mark is
/// `MOV R10, RAX; SHR R10, 9; MOV R11, biased; CMP byte [R11 + R10], DIRTY;
/// JE` -- no `SUB R10, R11` and no hull `CMP/JAE` -- and keeps both under any
/// other skip mask, where the receiver test does not prove the mark old.
#[test]
fn whole_heap_card_mark_uses_the_biased_base_behind_the_old_gen_test() {
    let shift = cratonvm_types::CARD_SHIFT as u8;
    let hull_lo = 0x1000_0000usize;
    let mut cards = vec![0u8; 64];
    let view = fake_hull_view(hull_lo, &mut cards);
    let view_addr = (&*view as *const FakeHullView) as usize;
    let map = cards.as_ptr() as usize;
    let biased = ir_biased_card_base(map, hull_lo, shift);
    assert_eq!(biased.wrapping_add(hull_lo >> shift), map);
    assert_eq!(
        biased.wrapping_add((hull_lo + (5 << shift) + 40) >> shift),
        map + 5
    );

    let emit = |mask: u8| {
        let mut lo = bare_lowerer();
        lo.ref_store_post_skip_mask = Some(mask);
        let (mut to_helper, mut done) = (Vec::new(), Vec::new());
        let p0 = lo.buf.pos();
        assert!(lo.emit_ir_whole_heap_card_mark_sib_fast(
            view_addr,
            shift,
            None,
            &mut to_helper,
            &mut done
        ));
        (
            lo.buf.as_slice()[p0..lo.buf.pos()].to_vec(),
            to_helper.len(),
        )
    };

    // With GC_FLAG_OLD_GEN, `TEST byte [RAX + flags], mask ; JZ` (10 bytes) comes first;
    // `TEST RDX ; JZ` is deferred to the clean-card edge after the biased card check.
    let (b, helper_edges) = emit(cratonvm_types::GC_FLAG_OLD_GEN);
    let mut want = vec![0x49, 0x89, 0xC2, 0x49, 0xC1, 0xEA, shift, 0x49, 0xBB];
    want.extend_from_slice(&(biased as u64).to_le_bytes());
    want.extend_from_slice(&[
        0x43,
        0x80,
        0x3C,
        0x13,
        crate::x64::CARD_DIRTY_BYTE,
        0x0F,
        0x84,
    ]);
    assert_eq!(&b[10..10 + want.len()], want.as_slice());
    assert_eq!(
        helper_edges, 0,
        "no hull miss edge, so no write_barrier call"
    );

    let (b, _) = emit(0x04);
    let mut want = vec![0x49, 0x89, 0xC2, 0x49, 0xC1, 0xEA, shift, 0x49, 0xBB];
    want.extend_from_slice(&((hull_lo >> shift) as u64).to_le_bytes());
    want.extend_from_slice(&[0x4D, 0x29, 0xDA, 0x49, 0x81, 0xFA]);
    assert_eq!(
        &b[19..19 + want.len()],
        want.as_slice(),
        "another skip mask keeps SUB + the hull compare"
    );
    drop(view);
}

/// w7: the biased card mark, executed: a non-null store into an old holder
/// dirties exactly the holder's card through `[biased + (holder >> 9)]`; a
/// null value or a young holder leaves the map alone.
#[cfg(target_arch = "x86_64")]
#[test]
fn whole_heap_biased_card_mark_executes_against_a_fake_hull() {
    #[repr(C, align(64))]
    struct Obj([u8; 64]);
    let shift = cratonvm_types::CARD_SHIFT;
    let dirty = crate::x64::CARD_DIRTY_BYTE;
    let clean = dirty ^ 0xFF;
    let mut obj = Box::new(Obj([0; 64]));
    obj.0[cratonvm_types::GC_FLAGS_BYTE_OFFSET] = cratonvm_types::GC_FLAG_OLD_GEN;
    let holder = obj.0.as_ptr() as usize;
    let hull_lo = ((holder >> shift) << shift) - (8 << shift);
    let mut cards = vec![clean; 64];
    let view = fake_hull_view(hull_lo, &mut cards);
    let view_addr = (&*view as *const FakeHullView) as usize;

    let mut lo = bare_lowerer();
    lo.ref_store_post_skip_mask = Some(cratonvm_types::GC_FLAG_OLD_GEN);
    let start = lo.buf.pos();
    #[cfg(windows)]
    lo.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX (RDX is the value already)
    #[cfg(not(windows))]
    lo.buf.emit(&[0x48, 0x89, 0xF8, 0x48, 0x89, 0xF2]); // MOV RAX, RDI ; MOV RDX, RSI
    let (mut to_helper, mut done) = (Vec::new(), Vec::new());
    assert!(lo.emit_ir_whole_heap_card_mark_sib_fast(
        view_addr,
        shift as u8,
        None,
        &mut to_helper,
        &mut done
    ));
    assert!(to_helper.is_empty());
    for p in done {
        lo.patch_rel32_to_here(p);
    }
    lo.buf.emit(&[0xC3]); // RET
    assert!(!lo.buf.overflowed());
    lo.buf.finalize();
    // SAFETY: a complete `extern "C" fn(u64, u64)` that reads the holder's
    // flags byte and one byte of `cards`, writes that byte, and clobbers only
    // RAX, RDX, R10, R11 (caller-saved on both ABIs).
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64)>(lo.buf.as_ptr().add(start))
    };
    let mark = |h: usize, v: u64, cards: &mut Vec<u8>| {
        cards.iter_mut().for_each(|c| *c = clean);
        f(h as u64, v);
        cards.iter().position(|&c| c != clean)
    };
    assert_eq!(mark(holder, 0, &mut cards), None, "null value: no card");
    assert_eq!(
        mark(holder, 0x1234, &mut cards),
        Some(8),
        "old holder: its card"
    );
    assert_eq!(cards[8], dirty);
    assert_eq!(cards.iter().filter(|&&c| c == dirty).count(), 1);
    obj.0[cratonvm_types::GC_FLAGS_BYTE_OFFSET] = 0;
    assert_eq!(
        mark(holder, 0x1234, &mut cards),
        None,
        "young holder: no card"
    );
    drop(view);
}
