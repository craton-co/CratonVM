use super::*;
use crate::ir::{MemKind, SafepointSnapshot};

fn block(id: usize, successors: Vec<usize>, predecessors: Vec<usize>) -> Block {
    Block {
        id,
        ctrl: NO_NODE,
        nodes: Vec::new(),
        terminator: None,
        successors,
        predecessors,
    }
}

/// Blocks 0 pre-header -> 1 header -> {2 body, 3 exit}; 2 -> 1.
/// * `n19 = 31 * h`, `n21 = (long) k`: header values, SUNK into the body;
/// * `n20 = getstatic LE_INT` (pc 14) and the invariant `n28 = length - 4`
///   (pc 19), hoisted to the pre-header with their in-loop pcs;
/// * `n39`, the segment access (pc 19), in the body: it can deopt -- a
///   field `Load` stands in for the `Unbox ScopedGet`;
/// * snapshot [1] at bci 14 names `[31*h, s]`, snapshot [2] at bci 19 names
///   `[31*h, s, LE_INT, (long)k]`.
///
/// `trapping_hoist`: the pre-header also holds a field read at pc 14 that
/// can trap (a hoisted load with its null test). Returns `(graph, dom,
/// original, placed, n19, n21)`.
fn heap_get(
    trapping_hoist: bool,
) -> (Graph, Dominators, Vec<usize>, Vec<usize>, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let s = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let h = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let k = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let len = g.add(Op::Param(3), IrType::Long, vec![start], None);
    let c31 = g.add(Op::Const(31), IrType::Int, vec![], None);
    let c4 = g.add(Op::Const(4), IrType::Long, vec![], None);
    let n19 = g.add(Op::Mul, IrType::Int, vec![c31, h], Some(12));
    let n20 = g.add(
        Op::LoadStatic {
            class_id: 579,
            field_index: 0,
            type_tag: b'L',
            is_volatile: false,
        },
        IrType::Ref,
        vec![c0, m0],
        Some(14),
    );
    g.direct_static_fields.push((579, 0));
    let n21 = g.add(Op::I2L, IrType::Long, vec![k], Some(18));
    let n28 = g.add(Op::Sub, IrType::Long, vec![len, c4], Some(19));
    let n39 = g.add(Op::Load(MemKind::Int), IrType::Int, vec![c0, m0, s, c4], Some(19));
    let hoisted = if trapping_hoist {
        Some(g.add(Op::Load(MemKind::Int), IrType::Int, vec![c0, m0, s, c4], Some(14)))
    } else {
        None
    };
    g.push_safepoint(SafepointSnapshot::new(4, vec![s, h, k], vec![], vec![]));
    g.push_safepoint(SafepointSnapshot::new(14, vec![s], vec![n19, s], vec![]));
    g.push_safepoint(SafepointSnapshot::new(19, vec![s], vec![n19, s, n20, n21], vec![]));
    let blocks = vec![
        block(0, vec![1], vec![]),
        block(1, vec![2, 3], vec![0, 2]),
        block(2, vec![1], vec![1]),
        block(3, vec![], vec![1]),
    ];
    let dom = Dominators::compute(&blocks);
    let n = g.nodes.len();
    let mut original = vec![usize::MAX; n];
    for id in [s, h, k, len, c31, c4, n20, n28] {
        original[id as usize] = 0;
    }
    original[n19 as usize] = 1;
    original[n21 as usize] = 1;
    original[n39 as usize] = 2;
    if let Some(x) = hoisted {
        original[x as usize] = 0;
    }
    let mut placed = original.clone();
    placed[n19 as usize] = 2;
    placed[n21 as usize] = 2;
    (g, dom, original, placed, n19, n21)    }