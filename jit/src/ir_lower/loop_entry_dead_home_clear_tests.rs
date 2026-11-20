use super::*;
use crate::ir::{CmpOp, MemKind};

/// `for (i = 0; i < n; i++) s += h.k;`: header merge, exit test, the read
/// on the stay edge. Returns `(graph, the read)`.
fn loop_graph() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let h = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(10));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    let s = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(12));
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(12));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let k = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![stay, m0, h, one],
        Some(16),
    );
    let sum = g.add(Op::Add, IrType::Int, vec![s, k], Some(19));
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(22));
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[s as usize].inputs.push(sum);
    g.nodes[region as usize].inputs.push(stay);
    let ret = g.add(Op::Return, IrType::Int, vec![exit, s], None);
    g.exit = ret;
    (g, k)
}

#[test]
fn a_home_no_loop_block_writes_moves_to_the_entry_block() {
    let (g, k) = loop_graph();
    let schedule = crate::ir_schedule::schedule(&g);
    let site_block = schedule.node_to_block[k as usize];
    // The loop's header: the block whose predecessor it dominates.
    let header = (0..schedule.blocks.len())
        .find(|&h| {
            schedule.blocks[h]
                .predecessors
                .iter()
                .any(|&p| schedule.dom.dominates(h, p))
        })
        .expect("a loop");
    let entry = schedule.blocks[header]
        .predecessors
        .iter()
        .copied()
        .find(|&p| !schedule.dom.dominates(header, p))
        .expect("an entry edge");
    // Colour 5 is written before the loop only; colour 6 in the loop.
    let blocks: Vec<StaleBlock> = schedule
        .blocks
        .iter()
        .enumerate()
        .map(|(b, blk)| StaleBlock {
            events: if b == entry {
                vec![StaleEvent::Def(5)]
            } else if b == site_block {
                vec![StaleEvent::Def(6)]
            } else {
                Vec::new()
            },
            predecessors: blk.predecessors.clone(),
        })
        .collect();
    // Colour 7 is the header phis' entry value (the constant `0`), which
    // nothing in the loop writes either: the entry block's edge copies
    // read it after the clears would run, so it never moves.
    let zero = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Const(0) && n.ty == IrType::Int)
        .expect("the entry value");
    let mut node_color: Vec<Option<u32>> = vec![None; g.nodes.len()];
    node_color[zero] = Some(7);
    let colour_of_off: HashMap<i32, usize> = HashMap::from([(100, 5), (108, 6), (116, 7)]);
    let mut sites: HashMap<NodeId, Vec<i32>> = HashMap::from([(k, vec![100, 108, 116])]);
    let out = move_loop_entry_dead_home_clears(
        &g,
        &schedule,
        &node_color,
        &blocks,
        &mut sites,
        &colour_of_off,
    );
    assert_eq!(out.get(&entry), Some(&vec![100]), "{out:?}");
    assert_eq!(sites.get(&k), Some(&vec![108, 116]));
    // A word with no colour (a staging word) never moves.
    let mut sites: HashMap<NodeId, Vec<i32>> = HashMap::from([(k, vec![900])]);
    let out = move_loop_entry_dead_home_clears(
        &g,
        &schedule,
        &node_color,
        &blocks,
        &mut sites,
        &colour_of_off,
    );
    assert!(out.is_empty());
    assert_eq!(sites.get(&k), Some(&vec![900]));
}
