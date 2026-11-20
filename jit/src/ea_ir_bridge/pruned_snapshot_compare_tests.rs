use super::*;
use crate::ir::{CmpOp, Graph, IrType, Op, SafepointSnapshot};

#[test]
fn a_compare_loses_only_its_unconsumable_frame_reader() {
    for reader in 0..8 {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let arg = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![arg, zero], Some(1));
        g.exit = g.add(Op::Return, IrType::Void,
            vec![ctrl, if reader == 2 { cmp } else { arg }], Some(3));
        let mut sp = SafepointSnapshot::new(2, vec![], vec![cmp], vec![]);
        if reader == 6 {
            sp.stack.clear();
            sp.monitors.push(cmp);
        }
        g.push_safepoint(sp);
        if reader == 1 || reader == 6 {
            g.add(Op::Guard { bci: 2 }, IrType::Void, vec![ctrl, arg], Some(2));
        }
        let mut map = ir_lower::ScalarReplacementMap::default();
        if (3..=5).contains(&reader) {
            map.objects.insert(zero, ir_lower::VirtualObjectInfo {
                class_id: 1, array_element_type: None, num_fields: 1,
                field_values: if reader == 3 { vec![Some(cmp)] } else { vec![] },
                new_ctrl: ctrl, store_ctrls: vec![], store_nodes: vec![],
                store_fields: if reader == 4 { vec![Some((0, Some(cmp)))] } else { vec![] },
                loop_phis: (reader == 5).then_some(vec![(0, cmp)]),
                non_null_cond: None,
            });
        }
        if reader == 7 { g.wide_entry.insert(arg, cmp); }
        let removed = ir_prune_unconsumable_snapshots_described(&mut g, &[], Some(&map));
        assert_eq!(removed, usize::from(reader != 1 && reader != 6));
        assert_eq!(g.nodes[cmp as usize].op == Op::Dead, reader == 0,
            "a comparison whose only reader was a dropped frame must disappear; actual data, frame, recipe and entry readers retain it (reader={reader})");
        assert!(g.verify_use_lists().is_ok());
    }
}
