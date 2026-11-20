use super::*;

#[test]
fn repeated_inputs_do_not_ready_a_clone_before_its_other_input() {
    let mut graph = Graph::empty(3);
    let a = graph.add(Op::ConstF(0), IrType::Double, vec![], None);
    let user = graph.add(
        Op::ScalarIntrinsic(crate::ir::ScalarOp::FmaD),
        IrType::Double,
        vec![a, a, NO_NODE],
        None,
    );
    let b = graph.add(Op::ConstF(1), IrType::Double, vec![], None);
    graph.set_input(user, 2, b);
    let order = clone_order(&graph, &build_users(&graph), &[a, b, user].into_iter().collect());
    assert_eq!(order, Some(vec![a, b, user]));
}
