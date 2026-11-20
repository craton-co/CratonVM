use super::*;

#[test]
fn multiply_add_is_rewritten_to_shift_and_subtract() {
    let mut graph = Graph::empty(1);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let x = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let c31 = graph.add(Op::Const(31), IrType::Int, vec![], None);
    let c7 = graph.add(Op::Const(7), IrType::Int, vec![], None);
    let mul = graph.add(Op::Mul, IrType::Int, vec![x, c31], None);
    let add = graph.add(Op::Add, IrType::Int, vec![mul, c7], None);
    let ret = graph.add(Op::Return, IrType::Control, vec![start, add], None);
    graph.exit = ret;
    assert_eq!(strength_reduce_mul(&mut graph), 1);
    let result = graph.nodes[ret as usize].inputs[1];
    assert_eq!(graph.nodes[result as usize].op, Op::Sub);
    assert_eq!(graph.nodes[add as usize].op, Op::Dead);
}
