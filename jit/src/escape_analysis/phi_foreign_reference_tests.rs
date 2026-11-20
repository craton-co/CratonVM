use super::*;

/// `Foo p = c ? new Foo() : null; return p.x;` — the null literal reaches
/// the EA graph as `Const(0)`. Treating it as "not a reference" folded the
/// load to the field default (0) instead of throwing NPE.
#[test]
fn a_phi_merging_the_allocation_with_null_blocks_scalar_replacement() {
    let mut g = Graph::new();
    let null = g.add_node(Op::Const(0), vec![]);
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    let phi = g.add_node(Op::Phi, vec![alloc, null]);
    let _load = g.add_node(Op::Load(0), vec![phi]);

    let result = analyze_escapes(&g);
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "a phi merging the allocation with the null literal must block scalar replacement"
    );
}

/// `Foo p = c ? new Foo() : (Foo) o; return p.x;` — the `checkcast` reaches
/// the EA graph as `Other`.
#[test]
fn a_phi_merging_the_allocation_with_a_checkcast_blocks_scalar_replacement() {
    let mut g = Graph::new();
    let param = g.add_node(Op::Param(0), vec![]);
    let cast = g.add_node(Op::Other, vec![param]);
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    let phi = g.add_node(Op::Phi, vec![alloc, cast]);
    let _load = g.add_node(Op::Load(0), vec![phi]);

    let result = analyze_escapes(&g);
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "a phi merging the allocation with a checkcast result must block scalar replacement"
    );
}

/// The null literal merged into a lock operand is a reference too.
#[test]
fn a_constant_is_not_assumed_to_produce_no_reference() {
    assert!(!produces_no_reference(&Op::Const(0)));
}
