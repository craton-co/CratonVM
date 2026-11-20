use super::*;

/// `Object[] a = new Object[n]; a[0] = l; return a[0];` — the array's
/// length is not a constant, so no index is "in range" and the load used
/// to link to nothing: `l` came back to the caller while it read
/// `NoEscape`, and its monitors were elided.
#[test]
fn a_value_read_back_from_an_unsized_array_escapes_with_the_read() {
    let mut g = Graph::new();
    let arr = g.add_node(
        Op::NewArray {
            element_type: 10,
            length: None,
        },
        vec![],
    );
    let l = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    g.add_node(Op::Store(0), vec![arr, l]);
    let ld = g.add_node(Op::Load(0), vec![arr]);
    g.add_node(Op::Return, vec![ld]);
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&l),
        Some(&EscapeState::GlobalEscape),
        "the returned read may be `l`"
    );
}

/// `(c ? a : b).f = l; return a.f;` — the store edge is keyed by the φ,
/// and the read through `a` used to look up `(a, f)` only.
#[test]
fn a_store_through_a_phi_holder_is_seen_by_a_read_of_either_object() {
    let mut g = Graph::new();
    let a = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let b = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let h = g.add_node(Op::Phi, vec![a, b]);
    let l = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    g.add_node(Op::Store(0), vec![h, l]);
    let ld = g.add_node(Op::Load(0), vec![a]);
    g.add_node(Op::Return, vec![ld]);
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&l),
        Some(&EscapeState::GlobalEscape),
        "`a.f` may be the `l` stored through the φ"
    );
}

/// `h = c ? new Box() : GLOBAL; h.ref = l;` — the φ resolves to the
/// fresh `Box` alone because the `LoadStatic` leg (a `Call` here)
/// contributes no points-to entry, so the store looked like one into a
/// confined object and `l`'s monitors were elided while `l` was published.
#[test]
fn a_store_through_a_phi_with_an_unnamed_leg_publishes_the_value() {
    let mut g = Graph::new();
    let fresh = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let global = g.add_node(Op::Call, vec![]);
    let h = g.add_node(Op::Phi, vec![fresh, global]);
    let l = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    g.add_node(Op::Store(0), vec![h, l]);
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&l),
        Some(&EscapeState::GlobalEscape)
    );
    // Control: through the fresh object alone, nothing is published.
    let mut g = Graph::new();
    let fresh = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let l = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    g.add_node(Op::Store(0), vec![fresh, l]);
    let result = analyze_escapes(&g);
    assert_eq!(result.escape_states.get(&l), Some(&EscapeState::NoEscape));
}
