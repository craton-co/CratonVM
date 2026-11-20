use super::*;

fn folds(g: &Graph) -> Vec<(NodeId, NodeId)> {
    let mut cg = build_connection_graph(g);
    propagate_escape_states(&mut cg, g);
    find_confined_ref_compare_folds(&cg, g)
}

fn new_obj(g: &mut Graph) -> NodeId {
    g.add_node(
        Op::New {
            class_id: 3,
            num_fields: 1,
        },
        vec![],
    )
}

#[test]
fn two_distinct_confined_allocations_never_compare_equal() {
    let mut g = Graph::new();
    let a = new_obj(&mut g);
    let b = new_obj(&mut g);
    let cmp = g.add_node(Op::RefCompare, vec![a, b]);
    assert_eq!(folds(&g), vec![(cmp, a)]);
}

#[test]
fn a_confined_allocation_is_never_a_parameter() {
    let mut g = Graph::new();
    let p = g.add_node(Op::Param(1), vec![]);
    let a = new_obj(&mut g);
    // `p == a`: the proof works from either side.
    let cmp = g.add_node(Op::RefCompare, vec![p, a]);
    assert_eq!(folds(&g), vec![(cmp, a)]);
}

#[test]
fn a_phi_that_may_carry_the_allocation_is_not_folded() {
    let mut g = Graph::new();
    let merge = g.add_node(Op::Merge, vec![]);
    let a = new_obj(&mut g);
    let b = new_obj(&mut g);
    // `prev = c ? a : b; prev == a` may be true. Both stay confined, so it
    // is the points-to set, not an escape, that refuses.
    let phi = g.add_node(Op::Phi, vec![merge, a, b]);
    let _cmp = g.add_node(Op::RefCompare, vec![a, phi]);
    let mut cg = build_connection_graph(&g);
    propagate_escape_states(&mut cg, &g);
    assert_eq!(cg.get_escape(a), EscapeState::NoEscape);
    assert!(folds(&g).is_empty());
    // The control: compared with `b`'s sibling allocation instead, `a`
    // folds.
    let c = new_obj(&mut g);
    let cmp2 = g.add_node(Op::RefCompare, vec![a, c]);
    assert_eq!(folds(&g), vec![(cmp2, a)]);
}

#[test]
fn a_load_that_reads_the_allocation_back_is_not_folded() {
    let mut g = Graph::new();
    let holder = new_obj(&mut g);
    let a = new_obj(&mut g);
    let _st = g.add_node(Op::Store(0), vec![holder, a]);
    let back = g.add_node(Op::Load(0), vec![holder]);
    // `h.f = a; h.f == a` is true.
    let _cmp = g.add_node(Op::RefCompare, vec![back, a]);
    assert!(folds(&g).is_empty());
}

#[test]
fn an_escaping_allocation_is_not_folded() {
    let mut g = Graph::new();
    let a = new_obj(&mut g);
    // `r = f(a); r == a` may be true: the callee can return its argument.
    let call = g.add_node(Op::Call, vec![a]);
    let _cmp = g.add_node(Op::RefCompare, vec![a, call]);
    assert!(folds(&g).is_empty());
}

#[test]
fn a_compare_of_an_allocation_with_itself_is_not_folded() {
    let mut g = Graph::new();
    let a = new_obj(&mut g);
    let _cmp = g.add_node(Op::RefCompare, vec![a, a]);
    assert!(folds(&g).is_empty());
}

#[test]
fn an_operand_merged_with_null_proves_nothing_from_its_side() {
    let mut g = Graph::new();
    let merge = g.add_node(Op::Merge, vec![]);
    let null = g.add_node(Op::Const(0), vec![]);
    let a = new_obj(&mut g);
    let maybe = g.add_node(Op::Phi, vec![merge, a, null]);
    let p = g.add_node(Op::Param(1), vec![]);
    // `(c ? a : null) == p`: may be `null == null`.
    let _cmp = g.add_node(Op::RefCompare, vec![maybe, p]);
    assert!(folds(&g).is_empty());
}
