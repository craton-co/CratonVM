// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 7 lowering tests: implicit-null field tails go out of line and file
//! their fault recovery pc; the per-definition epoch compare yields to a
//! dominating VM-wide epoch proof only for values read purely as receivers.

use super::w7_epoch_cover::value_read_only_as_field_receiver;
use super::*;

unsafe extern "C" fn field_answer(_ctx: usize, _base: usize, _index: u64) -> i64 {
    7
}

unsafe extern "C" fn no_field_exception() -> i64 {
    0
}

fn mk_lowerer(g: Graph) -> (&'static Graph, Lowerer<'static>) {
    let g: &'static Graph = Box::leak(Box::new(g));
    let sched: &'static Schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(g, sched, None)));
    let lo = tests::lowerer_over(g, sched, plan, 4096);
    (g, lo)
}

/// `Start -> ctrl, mem, Param(0): Ref`, returning the parameter.
fn tiny_graph() -> (Graph, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    (g, ctrl, mem, obj)
}

fn tail(implicit_nn_site: Option<usize>, legacy_patch: Option<usize>) -> DeferredGetfieldTail {
    DeferredGetfieldTail {
        receiver_gpr: 3,
        legacy_patch,
        slow: Vec::new(),
        done: 0,
        ref_node: false,
        type_tag: b'I',
        legacy_cell_off: legacy_cell_disp(0),
        node_ty: IrType::Int,
        base_src: SelfCallArgSource::Reg(3),
        arg2: 0,
        site_bci: 1,
        node_pc: Some(1),
        lazy_spills: Vec::new(),
        result_gpr: None,
        implicit_nn_site,
    }
}

fn tail_lowerer() -> Lowerer<'static> {
    let (mut g, ctrl, _mem, obj) = tiny_graph();
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, obj], None);
    let (_, mut lo) = mk_lowerer(g);
    lo.context_slot_off = 8;
    lo.getfield = field_answer as *const () as usize;
    lo.dispatch_threw = no_field_exception as *const () as usize;
    lo.cur_bci = 1;
    lo.cur_node_pc = Some(1);
    lo
}

#[test]
fn an_implicit_null_tail_files_its_helper_entry_as_the_fault_recovery_pc() {
    let mut lo = tail_lowerer();
    // Something in front, so the entry is not offset 0.
    lo.buf.emit(&[0x90, 0x90, 0x90]);
    let entry = lo.buf.pos();
    lo.deferred_getfield_tails.push(tail(Some(1), None));
    lo.deferred_getfield_tails.push(tail(None, None));
    lo.emit_deferred_getfield_tails_with_fault_entries();
    assert!(lo.deferred_getfield_tails.is_empty(), "every tail emitted");
    assert_eq!(
        lo.page_poll_sites,
        vec![(1, entry)],
        "only the implicit-null tail files a row, at its first byte"
    );
    assert!(
        lo.buf.pos() > entry,
        "the tails were emitted after the body"
    );
    assert!(lo.take_latched_bailout().is_none());
}

#[test]
fn an_implicit_null_tail_with_a_legacy_arm_refuses_the_compile() {
    let mut lo = tail_lowerer();
    let before = lo.buf.pos();
    lo.deferred_getfield_tails.push(tail(Some(1), Some(0)));
    lo.emit_deferred_getfield_tails_with_fault_entries();
    assert!(
        lo.page_poll_sites.is_empty(),
        "no recovery pc inside a legacy read"
    );
    assert_eq!(lo.buf.pos(), before, "nothing emitted for the refused tail");
    assert!(lo.take_latched_bailout().is_some());
}

#[test]
fn a_pointer_chase_value_is_read_only_as_a_field_receiver() {
    let (mut g, ctrl, mem, obj) = tiny_graph();
    let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    // p' = p.next; s += p'.x; loop while p' != null -- through a header phi.
    let next = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, mem, obj, f0],
        Some(4),
    );
    let p = g.add(Op::Phi, IrType::Ref, vec![ctrl, obj, next], None);
    let _x = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, p, f1],
        Some(8),
    );
    let _again = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![p, null], Some(12));
    assert!(value_read_only_as_field_receiver(&g, next));
    assert!(value_read_only_as_field_receiver(&g, p));
}

#[test]
fn a_value_stored_or_handed_to_another_reader_keeps_its_epoch_compare() {
    let (mut g, ctrl, mem, obj) = tiny_graph();
    let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let stored = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, mem, obj, f0],
        Some(4),
    );
    // `obj.f = stored`: the value escapes as a stored value (input 4).
    let _st = g.add(
        Op::Store(MemKind::Ref),
        IrType::Memory,
        vec![ctrl, mem, obj, f0, stored],
        Some(6),
    );
    assert!(!value_read_only_as_field_receiver(&g, stored));

    let through_phi = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, mem, obj, f0],
        Some(8),
    );
    let phi = g.add(Op::Phi, IrType::Ref, vec![ctrl, obj, through_phi], None);
    // A non-field reader behind the phi (an array length) wants its own proof.
    let _len = g.add(Op::ArrayLength, IrType::Int, vec![ctrl, mem, phi], Some(10));
    assert!(!value_read_only_as_field_receiver(&g, through_phi));
}
