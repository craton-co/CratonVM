use super::*;

/// A branchy graph (so the whole-graph stand-in fails) with an object
/// allocated and stored in `block` and read in `other` — the IrEscapeProbe
/// shape once `consume(mine)` is spliced: `mine.v = depth` in the entry
/// block, the read after the `down(...)` call in a later one.
///
/// With `late_store`, a second store to the field sits in `other` too.
fn build(late_store: bool) -> (Graph, NodeId, NodeId, NodeId) {
    let mut g = Graph::new();
    let block = g.add_node(Op::Merge, vec![]);
    let other = g.add_node(Op::Merge, vec![]);
    let obj = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let v = g.add_node(Op::Const(42), vec![]);
    let st = g.add_node(Op::Store(0), vec![obj, v]);
    let ld = g.add_node(Op::Load(0), vec![obj]);
    g.add_node(Op::Add, vec![ld, ld]);
    g.blocks.insert(obj, block);
    g.blocks.insert(st, block);
    g.blocks.insert(ld, other);
    if late_store {
        let w = g.add_node(Op::Const(7), vec![]);
        let st2 = g.add_node(Op::Store(0), vec![obj, w]);
        g.blocks.insert(st2, other);
    }
    (g, obj, v, ld)
}

fn analyse(
    g: &Graph,
    cross: bool,
) -> (Vec<ScalarReplacementInfo>, Vec<(NodeId, ScalarRefusal)>) {
    let mut cg = build_connection_graph(g);
    propagate_escape_states(&mut cg, g);
    find_scalar_replacements_with(&cg, g, cross)
}

#[test]
fn a_load_outside_the_block_holding_every_store_is_answered_under_the_opt_in() {
    let (g, obj, v, ld) = build(false);
    assert!(
        !program_order_proves_dominance(&g),
        "precondition: branchy graph"
    );

    // Default: refused, exactly as before.
    let (found, refusals) = analyse(&g, false);
    assert!(found.is_empty());
    assert_eq!(refusals, vec![(obj, ScalarRefusal::LoadNotAnswerable)]);

    // Opt-in: the block's last store answers it.
    let (found, _) = analyse(&g, true);
    let info = found
        .iter()
        .find(|i| i.alloc_node == obj)
        .expect("every store is in the allocation block");
    assert!(info.dominance_proved);
    assert_eq!(info.load_value(ld), LoadResolution::Value(v));
}

/// A store OUTSIDE the allocation block breaks the premise: the load's
/// answer then depends on the path, which this graph cannot see.
#[test]
fn a_store_outside_the_allocation_block_still_refuses() {
    let (g, obj, _, _) = build(true);
    let (found, refusals) = analyse(&g, true);
    assert!(found.is_empty());
    assert_eq!(refusals, vec![(obj, ScalarRefusal::LoadNotAnswerable)]);
}

/// A load with no recorded block is never answered by the cross-block
/// rule — a missing fact is not agreement.
#[test]
fn a_load_with_no_block_is_not_answered() {
    let (mut g, obj, _, ld) = build(false);
    g.blocks.remove(&ld);
    let (found, refusals) = analyse(&g, true);
    assert!(found.is_empty());
    assert_eq!(refusals, vec![(obj, ScalarRefusal::LoadNotAnswerable)]);
}

/// A field with no store at all reads the zero default from any block.
#[test]
fn a_never_stored_field_is_the_zero_default_across_blocks() {
    let stores: Vec<FieldStore> = Vec::new();
    assert_eq!(
        resolve_cross_block_load(&stores, None),
        LoadResolution::ZeroDefault
    );
    let stores = vec![
        FieldStore { store: 5, value: 3 },
        FieldStore { store: 8, value: 4 },
    ];
    assert_eq!(
        resolve_cross_block_load(&stores, None),
        LoadResolution::Value(4)
    );
    // The memory chain places the load after store 5 only: disagreement
    // refuses rather than forwarding the later store.
    assert_eq!(
        resolve_cross_block_load(&stores, Some(6)),
        LoadResolution::Unknown
    );
    assert_eq!(
        resolve_cross_block_load(&stores, Some(9)),
        LoadResolution::Value(4)
    );
}
