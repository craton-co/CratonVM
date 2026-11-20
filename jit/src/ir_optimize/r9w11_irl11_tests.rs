use super::*;

fn empty_graph() -> Graph {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    g
}

/// Round 9 wave 11 (irl11): the unroller's clone predicate admits an
/// integer division or remainder by a NON-ZERO constant -- at the
/// division's own width -- and nothing else it did not admit before.
#[test]
fn a_division_is_clonable_exactly_when_it_cannot_trap() {
    let mut g = empty_graph();
    let x = g.add(Op::Param(0), IrType::Int, vec![], None);
    let lx = g.add(Op::Param(1), IrType::Long, vec![], None);
    let d = g.add(Op::Param(2), IrType::Int, vec![], None);
    let three = g.add(Op::Const(3), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let minus_one = g.add(Op::Const(-1), IrType::Int, vec![], None);
    // Non-zero as a `long`, ZERO in its low 32 bits.
    let hi_only = g.add(Op::Const(1i64 << 32), IrType::Long, vec![], None);

    let div3 = g.add(Op::Div, IrType::Int, vec![x, three], None);
    let rem3 = g.add(Op::Rem, IrType::Int, vec![x, three], None);
    let div_m1 = g.add(Op::Div, IrType::Int, vec![x, minus_one], None);
    let div0 = g.add(Op::Div, IrType::Int, vec![x, zero], None);
    let div_d = g.add(Op::Div, IrType::Int, vec![x, d], None);
    let ldiv_hi = g.add(Op::Div, IrType::Long, vec![lx, hi_only], None);
    let idiv_hi = g.add(Op::Div, IrType::Int, vec![x, hi_only], None);
    // A third edge (a guard token) keeps it out.
    let entry = g.entry;
    let tokened = g.add(Op::Div, IrType::Int, vec![x, three, entry], None);
    let mul = g.add(Op::Mul, IrType::Int, vec![x, three], None);
    let call_like = g.add(Op::ArrayLength, IrType::Int, vec![x], None);

    assert!(unroll_body_node_clonable(&g, div3));
    assert!(unroll_body_node_clonable(&g, rem3));
    assert!(
        unroll_body_node_clonable(&g, div_m1),
        "`x / -1` is materialised inline, it cannot trap"
    );
    assert!(!unroll_body_node_clonable(&g, div0));
    assert!(!unroll_body_node_clonable(&g, div_d));
    assert!(unroll_body_node_clonable(&g, ldiv_hi));
    assert!(
        !unroll_body_node_clonable(&g, idiv_hi),
        "an int division reads the low 32 bits of its divisor, which are zero"
    );
    assert!(!unroll_body_node_clonable(&g, tokened));
    assert!(
        unroll_body_node_clonable(&g, mul),
        "pure arithmetic stays clonable"
    );
    assert!(!unroll_body_node_clonable(&g, call_like));
    assert!(!unroll_body_node_clonable(&g, NO_NODE));
}
