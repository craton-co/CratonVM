use super::*;
use crate::ir::{Graph, IrType, MemKind, Op, SafepointSnapshot};

#[test]
fn a_moved_frame_retains_allocation_and_stores_but_still_forwards_loads() {
    for array in [false, true] {
        for preserve in [false, true] {
            let mut g = Graph::empty(0);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
            let one = g.add(Op::Const(1), IrType::Int, vec![], None);
            let op = if array {
                Op::NewArray {
                    element_type: 10,
                    component_class_id: 0,
                }
            } else {
                Op::New {
                    class_id: 7,
                    num_fields: 1,
                }
            };
            let mut inputs = vec![ctrl, mem];
            if array {
                inputs.push(one);
            }
            let allocation = g.add(op, IrType::Ref, inputs, Some(1));
            let store = g.add(
                if array {
                    Op::ArrayStore(MemKind::Int)
                } else {
                    Op::Store(MemKind::Int)
                },
                IrType::Memory,
                vec![ctrl, mem, allocation, zero, seven],
                Some(2),
            );
            let load = g.add(
                if array {
                    Op::ArrayLoad(MemKind::Int)
                } else {
                    Op::Load(MemKind::Int)
                },
                IrType::Int,
                vec![ctrl, store, allocation, zero],
                Some(3),
            );
            let ret = g.add(Op::Return, IrType::Int, vec![ctrl, load], Some(4));
            g.entry = start;
            g.exit = ret;
            let mut frame = SafepointSnapshot::new(3, vec![allocation], vec![], vec![]);
            frame.preserve_allocations = preserve;
            g.push_safepoint(frame);
            let guard = g.add(Op::Guard { bci: 3 }, IrType::Void, vec![ctrl, one], Some(3));
            assert!(g.set_node_frame_snapshot(guard, 0));
            let (ea, ids) = escape_analysis_from_ir(&g);
            let result = escape_analysis::analyze_escapes(&ea);
            assert_eq!(
                result.scalar_replaceable.len(),
                1,
                "array={array} preserve={preserve}"
            );
            let (map, plans) =
                build_scalar_replacement_map_with_plans(&g, &ids, &result, &Default::default());
            assert_eq!(plans.len(), 1, "still forwards the field read");
            assert_eq!(
                map.objects.contains_key(&allocation),
                !preserve,
                "concrete allocation has no virtual recipe"
            );
            apply_ea_to_ir_planned(&mut g, &ids, &result, &Default::default(), Some(plans));
            assert_eq!(
                g.nodes[allocation as usize].op == Op::Dead,
                !preserve,
                "retain original identity"
            );
            assert_eq!(
                g.nodes[store as usize].op == Op::Dead,
                !preserve,
                "retain initialized heap state"
            );
            assert_eq!(g.nodes[load as usize].op, Op::Dead);
            assert_eq!(g.nodes[ret as usize].inputs[1], seven);
        }
    }
}
