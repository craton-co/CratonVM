use super::{
    pad_writebacks_all_static, pending_cell_of, promoted_pending_pin_values, CellWriteBack,
    CellWriteBackSrc, StaticCellWriteBack,
};
use crate::ir::{Graph, IrType, MemKind, Op};

/// `(graph, field exit store, static exit store, static silent store,
/// its value, the header value, the entry value, the field base)`.
fn cells() -> (Graph, u32, u32, u32, u32, u32, u32, u32) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let base = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let init = g.add(Op::Param(1), IrType::Int, vec![], None);
    let v = g.add(Op::Param(2), IrType::Int, vec![], None);
    let w = g.add(Op::Param(3), IrType::Int, vec![], None);
    let cond = g.add(
        Op::Cmp(crate::ir::CmpOp::Ne),
        IrType::Int,
        vec![v, init],
        None,
    );
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, cond], None);
    let taken = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let idx = g.add(Op::Const(2), IrType::Int, vec![], None);
    let field_exit = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![taken, mem, base, idx, v],
        None,
    );
    let put = Op::StoreStatic {
        class_id: 9,
        field_index: 3,
        type_tag: b'I',
        is_volatile: false,
    };
    let static_exit = g.add(put.clone(), IrType::Memory, vec![taken, mem, v], None);
    let silent = g.add(put, IrType::Memory, vec![ctrl, mem, w], None);
    (g, field_exit, static_exit, silent, w, v, init, base)
}

#[test]
fn a_static_cell_reads_slot_two_and_has_no_base() {
    let (g, field_exit, static_exit, _, _, v, init, base) = cells();
    let f = pending_cell_of(&g, field_exit).expect("a field cell");
    assert_eq!((f.value, f.init, f.base), (v, init, Some(base)));
    assert_eq!((f.field_index, f.value_slot), (2, 4));
    let s = pending_cell_of(&g, static_exit).expect("a static cell");
    assert_eq!((s.value, s.init, s.base), (v, init, None));
    assert_eq!(s.value_slot, 2);
}

#[test]
fn the_pins_read_a_static_silent_stores_value() {
    let (mut g, _, static_exit, silent, w, v, init, base) = cells();
    g.promoted_sunk_stores.push((static_exit, silent));
    let pins = promoted_pending_pin_values(&g);
    let mut want = vec![w, v, init];
    want.sort_unstable();
    assert_eq!(pins, want, "no base, and slot 2's value");
    assert!(!pins.contains(&base));
}

#[test]
fn only_an_all_static_pad_needs_no_xmm_refusal() {
    let st = |static_cell: bool| CellWriteBack {
        base_off: 0,
        field_index: 1,
        long: false,
        value: CellWriteBackSrc::Imm(1),
        init: CellWriteBackSrc::Imm(0),
        helper: 0,
        static_cell: static_cell.then_some(StaticCellWriteBack {
            base_cell: 0,
            cell_off: 16,
            hi_off: 24,
            long: false,
        }),
    };
    assert!(pad_writebacks_all_static(Some(&[st(true), st(true)][..])));
    assert!(!pad_writebacks_all_static(Some(&[st(true), st(false)][..])));
    assert!(!pad_writebacks_all_static(Some(&[][..])));
    assert!(!pad_writebacks_all_static(None));
}
