use super::*;

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

/// Blocks: 0 entry -> 1 header -> {2 body, 3 exit}; 2 -> 1 (back edge).
/// `a = 31 * h` and `b = k + 1` both sank from the header (1) to the body
/// (2), where `x = a + b` reads them. A snapshot at bci 7 names `a`, and a
/// trapping node `p` at bci 7 left in the header anchors it there: `a` may not
/// leave the header, `b` may. Returns `(graph, dom, original, placed, [a, b])`.
fn shape() -> (Graph, Dominators, Vec<usize>, Vec<usize>, [NodeId; 2]) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let h = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let k = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let c31 = g.add(Op::Const(31), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let a = g.add(Op::Mul, IrType::Int, vec![c31, h], None);
    // A division by a variable: it can deopt, so it anchors (a pure
    // node no longer does, `CRATONVM_JIT_IR_ANCHOR_TRAP_FREE_SKIP`).
    let p = g.add(Op::Div, IrType::Int, vec![k, h], Some(7));
    let b = g.add(Op::Add, IrType::Int, vec![k, one], None);
    let x = g.add(Op::Add, IrType::Int, vec![a, b], None);
    g.push_safepoint(crate::ir::SafepointSnapshot::new(7, vec![], vec![a], vec![]));
    let blocks = vec![
        block(0, vec![1], vec![]),
        block(1, vec![2, 3], vec![0, 2]),
        block(2, vec![1], vec![1]),
        block(3, vec![], vec![1]),
    ];
    let dom = Dominators::compute(&blocks);
    let n = g.nodes.len();
    let mut original = vec![usize::MAX; n];
    for id in [h, k, c31, one] {
        original[id as usize] = 0;
    }
    for id in [a, p, b] {
        original[id as usize] = 1;
    }
    original[x as usize] = 2;
    let mut placed = original.clone();
    placed[a as usize] = 2;
    placed[b as usize] = 2;
    (g, dom, original, placed, [a, b])    }