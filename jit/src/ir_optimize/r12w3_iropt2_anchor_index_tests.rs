use super::*;

fn scan(g: &Graph, ctrl: NodeId) -> Vec<NodeId> {
    (0..g.nodes.len() as NodeId)
        .filter(|&id| {
            g.nodes[id as usize].op != Op::Dead
                && g.nodes[id as usize].inputs.first() == Some(&ctrl)
        })
        .collect()
}

#[test]
fn the_index_answers_what_the_scan_answered_on_both_paths() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let pre = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let region = g.add(Op::Region, IrType::Control, vec![pre], None);
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    // `x` is an input of `region`'s phi at slot 1, which must NOT count.
    let phi = g.add(Op::Phi, IrType::Int, vec![region, x], None);
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![phi, x], None);
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], None);
    let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let dead = g.add(Op::Proj(0), IrType::Control, vec![region], None);
    g.ensure_use_lists();
    g.kill(dead);
    assert!(g.use_lists_valid(), "tracked edits keep the lists current");
    assert_eq!(anchored_at(&g, region), vec![phi, iff]);
    assert_eq!(anchored_at(&g, region), scan(&g, region));
    assert_eq!(anchored_at(&g, x), Vec::<NodeId>::new());
    assert_eq!(proj_user(&g, iff, 0), t);
    assert_eq!(proj_user(&g, iff, 1), f);
    assert_eq!(anchored_at(&g, NO_NODE), Vec::<NodeId>::new());

    // A raw edge write makes the lists stale; the scan path answers.
    g.nodes[f as usize].inputs.push(region);
    let guard = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![region, cond], None);
    assert_eq!(anchored_at(&g, region), scan(&g, region));
    assert!(anchored_at(&g, region).contains(&guard));
    assert!(control_token_has_guard(&g, region));
}

/// A hub past the cutoff takes the scan and still answers in id order.
#[test]
fn a_hub_with_many_users_is_scanned() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let hub = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let n = ANCHOR_INDEX_MAX_USERS as usize + 10;
    let ids: Vec<NodeId> = (0..n)
        .map(|i| g.add(Op::Param(i as u16), IrType::Int, vec![hub], None))
        .collect();
    g.ensure_use_lists();
    assert!(g.use_lists_valid() && g.use_count(hub) > ANCHOR_INDEX_MAX_USERS);
    assert_eq!(anchored_at(&g, hub), ids);
}
