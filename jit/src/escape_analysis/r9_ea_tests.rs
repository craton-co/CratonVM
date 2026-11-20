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

/// A full unroll clones a loop's loads with HIGH ids into the preheader,
/// where a store that runs after the old loop has a LOWER id. The memory
/// token still says the load ran first; the two orders disagree, so the
/// load is refused rather than forwarded a value from its own future.
#[test]
fn a_load_the_memory_chain_orders_before_a_store_is_not_forwarded_that_store() {
    let mut g = Graph::new();
    // Stands for the memory state before the (unrolled) loop.
    let mem0 = g.add_node(Op::Other, vec![]);
    let alloc = one_field_alloc(&mut g);
    let c42 = g.add_node(Op::Const(42), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c42]);
    // Higher id than the store, but chained BEFORE it.
    let load = g.add_node(Op::Load(0), vec![alloc]);
    g.load_mem_tokens.insert(load, mem0);

    let result = analyze_escapes(&g);
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "id order and chain order disagree: refuse, never guess"
    );
    assert!(result
        .scalar_refusals
        .contains(&(alloc, ScalarRefusal::LoadNotAnswerable)));
}

/// The agreeing case is unchanged: a load whose token IS the store reads it.
#[test]
fn a_load_whose_memory_token_is_the_store_reads_that_store() {
    let mut g = Graph::new();
    let alloc = one_field_alloc(&mut g);
    let c42 = g.add_node(Op::Const(42), vec![]);
    let store = g.add_node(Op::Store(0), vec![alloc, c42]);
    let load = g.add_node(Op::Load(0), vec![alloc]);
    g.load_mem_tokens.insert(load, store);

    let result = analyze_escapes(&g);
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("the orders agree, so the object is replaced");
    assert_eq!(sr.load_value(load), LoadResolution::Value(c42));
}

/// And a load before every store, by both orders, still reads the zero
/// default.
#[test]
fn a_load_before_the_store_by_both_orders_reads_the_zero_default() {
    let mut g = Graph::new();
    let mem0 = g.add_node(Op::Other, vec![]);
    let alloc = one_field_alloc(&mut g);
    let load = g.add_node(Op::Load(0), vec![alloc]);
    g.load_mem_tokens.insert(load, mem0);
    let c42 = g.add_node(Op::Const(42), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c42]);

    let result = analyze_escapes(&g);
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("must replace");
    assert_eq!(sr.load_value(load), LoadResolution::ZeroDefault);
}

/// The restructured propagation still reaches the fixed point through a
/// chain of stores: `c` is returned, `b` is stored in `c`, `a` in `b` —
/// every one of them escapes globally, however the nodes are ordered.
#[test]
fn escape_propagates_down_a_chain_of_stores_written_in_reverse() {
    let mut g = Graph::new();
    let a = one_field_alloc(&mut g);
    let b = one_field_alloc(&mut g);
    let c = one_field_alloc(&mut g);
    // Written innermost-first, so a single forward sweep cannot finish it.
    let _s_ab = g.add_node(Op::Store(0), vec![b, a]);
    let _s_bc = g.add_node(Op::Store(0), vec![c, b]);
    g.nodes[g.exit].inputs.push(c);
    g.nodes[c].uses.push(g.exit);

    let result = analyze_escapes(&g);
    for alloc in [a, b, c] {
        assert_eq!(
            result.escape_states.get(&alloc).copied(),
            Some(EscapeState::GlobalEscape),
            "node {alloc} is reachable from the returned object"
        );
    }
}

/// A load that executes in a coarsening gap by its memory token, but was
/// given an id outside the gap's id range (a full unroll's clone), is still
/// held to the gap allowlist: here it reads a caller-supplied holder, so it
/// can throw, and the merge is refused.
#[test]
fn a_cloned_load_in_the_gap_is_seen_by_its_memory_token() {
    let mut g = Graph::new();
    let this = g.add_node(Op::Param(0), vec![]);
    let o = one_field_alloc(&mut g);
    let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let x1 = g.add_node(Op::MonitorExit, vec![o]);
    let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x2 = g.add_node(Op::MonitorExit, vec![o]);
    // Created after both regions, ordered right after the first exit.
    let late = g.add_node(Op::Load(0), vec![this]);
    g.load_mem_tokens.insert(late, x1);

    let r = analyze_escapes(&g);
    assert!(
        r.lock_coarsening.is_empty(),
        "a possibly-throwing load runs between the regions"
    );
    assert!(r.lock_refusals.contains(&(o, LockRefusal::ObservableGap)));
}
