use super::*;

fn one_field_alloc(g: &mut Graph) -> NodeId {
    g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    )
}

/// The store has the HIGHEST id but sits on the chain before `other`, the
/// load's token. Id order (`store < load`) and the old token-id check
/// (`store <= token`) both miss it and agree on the zero default; the chain
/// walk finds the store and refuses.
#[test]
fn a_store_numbered_after_the_token_but_chained_before_it_refuses_the_load() {
    let mut g = Graph::new();
    let mem0 = g.add_node(Op::Other, vec![]);
    let alloc = one_field_alloc(&mut g);
    let other = g.add_node(Op::Other, vec![]);
    let load = g.add_node(Op::Load(0), vec![alloc]);
    let c42 = g.add_node(Op::Const(42), vec![]);
    let store = g.add_node(Op::Store(0), vec![alloc, c42]);
    g.mem_pred.insert(store, mem0);
    g.mem_pred.insert(other, store);
    g.mem_pred.insert(load, other);
    g.load_mem_tokens.insert(load, other);

    let result = analyze_escapes(&g);
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "the chain says the load reads 42, id order says 0: refuse"
    );
    assert!(result
        .scalar_refusals
        .contains(&(alloc, ScalarRefusal::LoadNotAnswerable)));
}

/// When the chain agrees, nothing changes.
#[test]
fn a_chain_that_agrees_keeps_the_forwarded_value() {
    let mut g = Graph::new();
    let mem0 = g.add_node(Op::Other, vec![]);
    let alloc = one_field_alloc(&mut g);
    let c42 = g.add_node(Op::Const(42), vec![]);
    let store = g.add_node(Op::Store(0), vec![alloc, c42]);
    let other = g.add_node(Op::Other, vec![]);
    let load = g.add_node(Op::Load(0), vec![alloc]);
    g.mem_pred.insert(store, mem0);
    g.mem_pred.insert(other, store);
    g.mem_pred.insert(load, other);
    g.load_mem_tokens.insert(load, other);

    let result = analyze_escapes(&g);
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("all three orders agree");
    assert_eq!(sr.load_value(load), LoadResolution::Value(c42));
}

/// A chain that ends at a φ cannot answer, so it refuses nothing.
#[test]
fn a_chain_ending_at_a_phi_cannot_answer() {
    let mut g = Graph::new();
    let phi = g.add_node(Op::Phi, vec![]);
    let alloc = one_field_alloc(&mut g);
    let c42 = g.add_node(Op::Const(42), vec![]);
    let store = g.add_node(Op::Store(0), vec![alloc, c42]);
    let stores = [FieldStore { store, value: c42 }];
    let other = g.add_node(Op::Other, vec![]);
    g.mem_pred.insert(other, phi);
    assert_eq!(chain_last_store(&g, &stores, other), None);
    assert_eq!(chain_last_store(&g, &stores, store), Some(Some(stores[0])));
}
