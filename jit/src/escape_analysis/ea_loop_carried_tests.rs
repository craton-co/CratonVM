use super::*;

fn alloc_fields(g: &mut Graph, n: usize) -> NodeId {
    g.add_node(
        Op::New {
            class_id: 1,
            num_fields: n,
        },
        vec![],
    )
}

fn replace(
    g: &Graph,
    loop_carried: bool,
) -> (Vec<ScalarReplacementInfo>, Vec<(NodeId, ScalarRefusal)>) {
    let mut cg = build_connection_graph(g);
    propagate_escape_states(&mut cg, g);
    let observed = live_identity_observed(g, &find_identity_observations(&cg, g));
    find_scalar_replacements(&cg, g, false, loop_carried, &observed)
}

fn chain(g: &mut Graph, node: NodeId, pred: NodeId) {
    g.mem_pred.insert(node, pred);
    if matches!(g.nodes[node].op, Op::Load(_)) {
        g.load_mem_tokens.insert(node, pred);
    }
}

/// `o = new C(); o.f = 0; loop { v = o.f; o.f = v + 1; } w = o.f;` -- the
/// shape of a for-each iterator's `cursor`. Both loads read a value φ at
/// the header of (entry store's value, back-edge store's value), and the
/// object is refused without the walk.
#[test]
fn a_field_carried_around_a_loop_is_a_value_phi() {
    let mut g = Graph::new();
    let mem0 = g.add_node(Op::Other, vec![]);
    let alloc = alloc_fields(&mut g, 1);
    chain(&mut g, alloc, mem0);
    let c0 = g.add_node(Op::Const(0), vec![]);
    let st0 = g.add_node(Op::Store(0), vec![alloc, c0]);
    chain(&mut g, st0, mem0);
    let entry = g.add_node(Op::Other, vec![]);
    let region = g.add_node(Op::Merge, vec![entry]);
    let mphi = g.add_node(Op::Phi, vec![]);
    let ld = g.add_node(Op::Load(0), vec![alloc]);
    chain(&mut g, ld, mphi);
    let one = g.add_node(Op::Const(1), vec![]);
    let add = g.add_node(Op::Add, vec![ld, one]);
    let st_b = g.add_node(Op::Store(0), vec![alloc, add]);
    chain(&mut g, st_b, ld);
    g.nodes[mphi].inputs = vec![region, st0, st_b];
    let ld2 = g.add_node(Op::Load(0), vec![alloc]);
    chain(&mut g, ld2, mphi);

    let (sr, refusals) = replace(&g, false);
    assert!(sr.is_empty());
    assert_eq!(refusals, vec![(alloc, ScalarRefusal::LoadNotAnswerable)]);

    let (sr, refusals) = replace(&g, true);
    assert!(refusals.is_empty(), "{refusals:?}");
    let info = &sr[0];
    assert_eq!(info.load_value(ld), LoadResolution::Phi(0));
    assert_eq!(info.load_value(ld2), LoadResolution::Phi(0));
    assert_eq!(
        info.value_phis,
        vec![ValuePhi {
            mem_phi: mphi,
            region,
            field: 0,
            inputs: vec![LoadResolution::Value(c0), LoadResolution::Value(add)],
        }]
    );
}

/// A field the loop never writes needs no φ: the header φ carries one value
/// (itself on the back edge) and folds to the entry store's value. A field
/// never stored at all is the zero default.
#[test]
fn a_field_the_loop_never_writes_folds_to_its_one_value() {
    let mut g = Graph::new();
    let mem0 = g.add_node(Op::Other, vec![]);
    let alloc = alloc_fields(&mut g, 3);
    chain(&mut g, alloc, mem0);
    let c0 = g.add_node(Op::Const(0), vec![]);
    let st0 = g.add_node(Op::Store(0), vec![alloc, c0]);
    chain(&mut g, st0, mem0);
    let c7 = g.add_node(Op::Const(7), vec![]);
    let st1 = g.add_node(Op::Store(1), vec![alloc, c7]);
    chain(&mut g, st1, st0);
    let entry = g.add_node(Op::Other, vec![]);
    let region = g.add_node(Op::Merge, vec![entry]);
    let mphi = g.add_node(Op::Phi, vec![]);
    let inv = g.add_node(Op::Load(1), vec![alloc]);
    chain(&mut g, inv, mphi);
    let never = g.add_node(Op::Load(2), vec![alloc]);
    chain(&mut g, never, inv);
    let ld = g.add_node(Op::Load(0), vec![alloc]);
    chain(&mut g, ld, never);
    let add = g.add_node(Op::Add, vec![ld, inv]);
    let st_b = g.add_node(Op::Store(0), vec![alloc, add]);
    chain(&mut g, st_b, ld);
    g.nodes[mphi].inputs = vec![region, st1, st_b];

    let (sr, refusals) = replace(&g, true);
    assert!(refusals.is_empty(), "{refusals:?}");
    let info = &sr[0];
    assert_eq!(info.load_value(inv), LoadResolution::Value(c7));
    assert_eq!(info.load_value(never), LoadResolution::ZeroDefault);
    assert_eq!(info.load_value(ld), LoadResolution::Phi(0));
    assert_eq!(info.value_phis.len(), 1);
    assert_eq!(info.value_phis[0].field, 0);
}

/// An allocation INSIDE the loop: the walk stops at the allocation's own
/// incoming token (the header memory φ) and answers the zero default. It
/// must never cross the back edge into the previous iteration's object.
#[test]
fn an_allocation_inside_the_loop_never_reads_the_last_iteration() {
    let mut g = Graph::new();
    let mem0 = g.add_node(Op::Other, vec![]);
    let entry = g.add_node(Op::Other, vec![]);
    let region = g.add_node(Op::Merge, vec![entry]);
    let mphi = g.add_node(Op::Phi, vec![]);
    let alloc = alloc_fields(&mut g, 1);
    chain(&mut g, alloc, mphi);
    let before = g.add_node(Op::Load(0), vec![alloc]);
    chain(&mut g, before, mphi);
    let c5 = g.add_node(Op::Const(5), vec![]);
    let st = g.add_node(Op::Store(0), vec![alloc, c5]);
    chain(&mut g, st, before);
    let after = g.add_node(Op::Load(0), vec![alloc]);
    chain(&mut g, after, st);
    g.nodes[mphi].inputs = vec![region, mem0, after];

    let (sr, refusals) = replace(&g, true);
    assert!(refusals.is_empty(), "{refusals:?}");
    let info = &sr[0];
    assert_eq!(info.load_value(before), LoadResolution::ZeroDefault);
    assert_eq!(info.load_value(after), LoadResolution::Value(c5));
    assert!(info.value_phis.is_empty());
}

/// `lastRet`: a field the loop stores but never loads. No load needs its
/// φ, but a deopt recipe inside the loop does, so a loop-carried object
/// is offered one at the header too.
#[test]
fn a_stored_only_field_of_a_loop_carried_object_gets_a_recipe_phi() {
    let mut g = Graph::new();
    let mem0 = g.add_node(Op::Other, vec![]);
    let alloc = alloc_fields(&mut g, 2);
    chain(&mut g, alloc, mem0);
    let c0 = g.add_node(Op::Const(0), vec![]);
    let st_c0 = g.add_node(Op::Store(0), vec![alloc, c0]);
    chain(&mut g, st_c0, mem0);
    let cm1 = g.add_node(Op::Const(-1), vec![]);
    let st_l0 = g.add_node(Op::Store(1), vec![alloc, cm1]);
    chain(&mut g, st_l0, st_c0);
    let entry = g.add_node(Op::Other, vec![]);
    let region = g.add_node(Op::Merge, vec![entry]);
    let mphi = g.add_node(Op::Phi, vec![]);
    let ld = g.add_node(Op::Load(0), vec![alloc]);
    chain(&mut g, ld, mphi);
    let one = g.add_node(Op::Const(1), vec![]);
    let add = g.add_node(Op::Add, vec![ld, one]);
    let st_c1 = g.add_node(Op::Store(0), vec![alloc, add]);
    chain(&mut g, st_c1, ld);
    let st_l1 = g.add_node(Op::Store(1), vec![alloc, ld]);
    chain(&mut g, st_l1, st_c1);
    g.nodes[mphi].inputs = vec![region, st_l0, st_l1];

    let (sr, refusals) = replace(&g, true);
    assert!(refusals.is_empty(), "{refusals:?}");
    let info = &sr[0];
    assert_eq!(info.load_value(ld), LoadResolution::Phi(0));
    let last_ret = info
        .value_phis
        .iter()
        .find(|p| p.field == 1)
        .expect("the recipe φ for the stored-only field");
    assert_eq!(last_ret.mem_phi, mphi);
    assert_eq!(
        last_ret.inputs,
        vec![LoadResolution::Value(cm1), LoadResolution::Value(ld)]
    );
}

/// A memory φ edge that ends anywhere but the allocation's token (an OSR
/// entry's opaque memory, say) cannot be answered: refuse.
#[test]
fn a_phi_edge_from_an_unknown_memory_root_refuses() {
    let mut g = Graph::new();
    let mem0 = g.add_node(Op::Other, vec![]);
    let osr_mem = g.add_node(Op::Other, vec![]);
    let alloc = alloc_fields(&mut g, 1);
    chain(&mut g, alloc, mem0);
    let c0 = g.add_node(Op::Const(0), vec![]);
    let st0 = g.add_node(Op::Store(0), vec![alloc, c0]);
    chain(&mut g, st0, mem0);
    let entry = g.add_node(Op::Other, vec![]);
    let region = g.add_node(Op::Merge, vec![entry]);
    let mphi = g.add_node(Op::Phi, vec![region, st0, osr_mem]);
    let ld = g.add_node(Op::Load(0), vec![alloc]);
    chain(&mut g, ld, mphi);

    let (sr, refusals) = replace(&g, true);
    assert!(sr.is_empty());
    assert_eq!(refusals, vec![(alloc, ScalarRefusal::LoadNotAnswerable)]);
}
