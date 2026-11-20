use super::*;

/// Helper: build a trivial graph with a local allocation that is only
/// loaded/stored but never returned or passed to a call.
fn graph_local_alloc() -> Graph {
    let mut g = Graph::new();
    // node 2: New { class_id: 1, num_fields: 2 }
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 2,
        },
        vec![],
    );
    // node 3: Const(42)
    let c42 = g.add_node(Op::Const(42), vec![]);
    // node 4: Store field 0 <- 42
    let _store = g.add_node(Op::Store(0), vec![alloc, c42]);
    // node 5: Load field 0
    let _load = g.add_node(Op::Load(0), vec![alloc]);
    g
}

// ---- Basic escape classification ----

#[test]
fn test_local_alloc_no_escape() {
    let g = graph_local_alloc();
    let result = analyze_escapes(&g);
    assert_eq!(result.escape_states.get(&2), Some(&EscapeState::NoEscape));
}

#[test]
fn test_returned_alloc_global_escape() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    // Wire alloc as input to Return.
    g.nodes[1].inputs.push(alloc); // Return node
    g.nodes[alloc].uses.push(1);
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&alloc),
        Some(&EscapeState::GlobalEscape)
    );
}

#[test]
fn test_call_arg_escape() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let _call = g.add_node(Op::Call, vec![alloc]);
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&alloc),
        Some(&EscapeState::ArgEscape)
    );
}

#[test]
fn test_stored_to_escaping_object_global_escape() {
    let mut g = Graph::new();
    // Outer object escapes via return.
    let outer = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    g.nodes[1].inputs.push(outer);
    g.nodes[outer].uses.push(1);
    // Inner object is stored into outer's field.
    let inner = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    let _store = g.add_node(Op::Store(0), vec![outer, inner]);
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&outer),
        Some(&EscapeState::GlobalEscape)
    );
    // Inner should also be GlobalEscape because it's stored into a
    // globally-escaping object.
    assert_eq!(
        result.escape_states.get(&inner),
        Some(&EscapeState::GlobalEscape)
    );
}

#[test]
fn test_value_stored_into_param_field_escapes() {
    // Regression for a lazy-init getter shape first audited during the
    // keycloak `CredentialModelTest` investigation:
    //
    //   Map getAdditionalParameters() {
    //       if (additionalParameters == null)
    //           additionalParameters = new MultivaluedHashMap<>();
    //       return additionalParameters;
    //   }
    //
    // `this` is `Param(0)`.  The freshly allocated map is published into a
    // field of `this`, so the caller can reach it after the method returns.
    // It MUST be treated as escaping and MUST NOT be scalar-replaced —
    // scalar-replacing it would elide the `putfield`, leaving the instance
    // field null and the getter returning `null` (a value that can never
    // legitimately be null on a correct JVM). The real keycloak failure
    // later proved to be outside this IR scalar-replacement path, but the
    // invariant remains valid.
    let mut g = Graph::new();
    let this = g.add_node(Op::Param(0), vec![]);
    let map = g.add_node(
        Op::New {
            class_id: 7,
            num_fields: 0,
        },
        vec![],
    );
    let _store = g.add_node(Op::Store(0), vec![this, map]);
    // Reload the field and return it, as the real getter does.
    let load = g.add_node(Op::Load(0), vec![this]);
    g.nodes[1].inputs.push(load); // Return node
    g.nodes[load].uses.push(1);

    let result = analyze_escapes(&g);
    assert_ne!(
        result.escape_states.get(&map),
        Some(&EscapeState::NoEscape),
        "object published into a parameter's field must escape"
    );
    assert!(
        result.scalar_replaceable.is_empty(),
        "a published object must not be scalar-replaced (would drop the putfield)"
    );
}

// ---- Scalar replacement ----

#[test]
fn test_scalar_replacement_single_field() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 5,
            num_fields: 1,
        },
        vec![],
    );
    let c10 = g.add_node(Op::Const(10), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c10]);
    let _load = g.add_node(Op::Load(0), vec![alloc]);
    let result = analyze_escapes(&g);
    assert_eq!(result.scalar_replaceable.len(), 1);
    assert_eq!(result.scalar_replaceable[0].class_id, 5);
    assert_eq!(result.scalar_replaceable[0].field_values[0], Some(c10));
}

#[test]
fn test_scalar_replacement_multi_field() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 3,
            num_fields: 3,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let c2 = g.add_node(Op::Const(2), vec![]);
    let c3 = g.add_node(Op::Const(3), vec![]);
    let _s0 = g.add_node(Op::Store(0), vec![alloc, c1]);
    let _s1 = g.add_node(Op::Store(1), vec![alloc, c2]);
    let _s2 = g.add_node(Op::Store(2), vec![alloc, c3]);
    let _l1 = g.add_node(Op::Load(1), vec![alloc]);
    let result = analyze_escapes(&g);
    assert_eq!(result.scalar_replaceable.len(), 1);
    let sr = &result.scalar_replaceable[0];
    assert_eq!(sr.num_fields, 3);
    assert_eq!(sr.field_values, vec![Some(c1), Some(c2), Some(c3)]);
    assert_eq!(sr.replaced_loads.len(), 1);
    assert_eq!(sr.eliminated_stores.len(), 3);
}

#[test]
fn test_load_after_store_returns_stored_value() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c99 = g.add_node(Op::Const(99), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c99]);
    let _load = g.add_node(Op::Load(0), vec![alloc]);
    let result = analyze_escapes(&g);
    let sr = &result.scalar_replaceable[0];
    assert_eq!(sr.field_values[0], Some(c99));
}

#[test]
fn test_uninitialized_field_returns_none() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 2,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    // Field 1 is never stored to.
    let _load = g.add_node(Op::Load(1), vec![alloc]);
    let result = analyze_escapes(&g);
    let sr = &result.scalar_replaceable[0];
    assert_eq!(sr.field_values[1], None);
}

// ---- Lock elision ----

#[test]
fn test_lock_on_non_escaping_elided() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    let _enter = g.add_node(Op::MonitorEnter, vec![alloc]);
    let _exit = g.add_node(Op::MonitorExit, vec![alloc]);
    let result = analyze_escapes(&g);
    assert_eq!(result.elide_locks.len(), 2);
    assert_eq!(result.stats.locks_elided, 2);
}

#[test]
fn test_lock_on_escaping_object_kept() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    g.nodes[1].inputs.push(alloc); // Return
    g.nodes[alloc].uses.push(1);
    let _enter = g.add_node(Op::MonitorEnter, vec![alloc]);
    let result = analyze_escapes(&g);
    assert!(result.elide_locks.is_empty());
}

// ---- Partial escape ----

#[test]
fn test_partial_escape_detection() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    // Also passed to a call (ArgEscape).
    let _call = g.add_node(Op::Call, vec![alloc]);
    let cg = build_connection_graph(&g);
    assert!(is_partial_escape(&cg, &g, alloc));
}

#[test]
fn test_materialization_point_found() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c7 = g.add_node(Op::Const(7), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c7]);
    let _call = g.add_node(Op::Call, vec![alloc]);

    let cg = build_connection_graph(&g);
    let mps = find_materialization_points(&cg, &g, alloc);
    assert_eq!(mps.len(), 1);
    assert_eq!(mps[0].escape_point, _call);
    assert_eq!(mps[0].fields_at_escape[0], Some(c7));
}

// ---- Connection graph internals ----

#[test]
fn test_phi_deferred_edges() {
    let mut g = Graph::new();
    let a1 = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    let a2 = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    let phi = g.add_node(Op::Phi, vec![a1, a2]);
    let _ = phi; // just used for CG building
    let cg = build_connection_graph(&g);
    let pts = cg.resolve_points_to(phi);
    assert!(pts.contains(&a1));
    assert!(pts.contains(&a2));
}

#[test]
fn test_points_to_propagation_through_copies() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    // Phi acts as a "copy".
    let phi1 = g.add_node(Op::Phi, vec![alloc]);
    let phi2 = g.add_node(Op::Phi, vec![phi1]);
    let cg = build_connection_graph(&g);
    let pts = cg.resolve_points_to(phi2);
    assert!(pts.contains(&alloc));
}

#[test]
fn test_field_edge_tracking() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 2,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let c2 = g.add_node(Op::Const(2), vec![]);
    let _s0 = g.add_node(Op::Store(0), vec![alloc, c1]);
    let _s1 = g.add_node(Op::Store(1), vec![alloc, c2]);
    let cg = build_connection_graph(&g);
    assert!(cg.field_edges.get(&(alloc, 0)).unwrap().contains(&c1));
    assert!(cg.field_edges.get(&(alloc, 1)).unwrap().contains(&c2));
}

#[test]
fn test_fixed_point_convergence() {
    // Build a graph where propagation needs multiple iterations.
    let mut g = Graph::new();
    let a1 = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    let phi1 = g.add_node(Op::Phi, vec![a1]);
    let phi2 = g.add_node(Op::Phi, vec![phi1]);
    // Return phi2 -> should propagate GlobalEscape back to a1.
    g.nodes[1].inputs.push(phi2);
    g.nodes[phi2].uses.push(1);
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&a1),
        Some(&EscapeState::GlobalEscape)
    );
}

// ---- Empty / edge cases ----

#[test]
fn test_no_allocations_empty_result() {
    let mut g = Graph::new();
    let _c = g.add_node(Op::Const(1), vec![]);
    let _add = g.add_node(Op::Add, vec![]);
    let result = analyze_escapes(&g);
    assert!(result.escape_states.is_empty());
    assert_eq!(result.stats.total_allocations, 0);
}

#[test]
fn test_array_returned_global_escape() {
    let mut g = Graph::new();
    let arr = g.add_node(
        Op::NewArray {
            element_type: 10,
            length: None,
        },
        vec![],
    );
    g.nodes[1].inputs.push(arr);
    g.nodes[arr].uses.push(1);
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&arr),
        Some(&EscapeState::GlobalEscape)
    );
}

#[test]
fn test_array_local_no_escape() {
    let mut g = Graph::new();
    let arr = g.add_node(
        Op::NewArray {
            element_type: 5,
            length: None,
        },
        vec![],
    );
    let _len = g.add_node(Op::ArrayLength, vec![arr]);
    let result = analyze_escapes(&g);
    assert_eq!(result.escape_states.get(&arr), Some(&EscapeState::NoEscape));
}

#[test]
fn test_multiple_allocations() {
    let mut g = Graph::new();
    let a1 = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    let a2 = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    let a3 = g.add_node(
        Op::New {
            class_id: 3,
            num_fields: 0,
        },
        vec![],
    );
    // a1 returned.
    g.nodes[1].inputs.push(a1);
    g.nodes[a1].uses.push(1);
    // a2 passed to call.
    let _call = g.add_node(Op::Call, vec![a2]);
    // a3 purely local.
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&a1),
        Some(&EscapeState::GlobalEscape)
    );
    assert_eq!(result.escape_states.get(&a2), Some(&EscapeState::ArgEscape));
    assert_eq!(result.escape_states.get(&a3), Some(&EscapeState::NoEscape));
    assert_eq!(result.stats.total_allocations, 3);
}

#[test]
fn test_stats_counting() {
    let mut g = Graph::new();
    let _a1 = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let a2 = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    let _call = g.add_node(Op::Call, vec![a2]);
    // a1 is local -> scalar replaceable, a2 is ArgEscape -> stack alloc.
    let result = analyze_escapes(&g);
    assert_eq!(result.stats.total_allocations, 2);
    assert_eq!(result.stats.no_escape, 1);
    assert_eq!(result.stats.arg_escape, 1);
    assert_eq!(result.stats.scalar_replaced, 1);
    assert_eq!(result.stats.stack_allocated, 1);
}

// ---- Graph transformations ----

#[test]
fn test_apply_scalar_replacement_marks_dead() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c10 = g.add_node(Op::Const(10), vec![]);
    let store = g.add_node(Op::Store(0), vec![alloc, c10]);
    let load = g.add_node(Op::Load(0), vec![alloc]);

    let info = ScalarReplacementInfo {
        alloc_node: alloc,
        class_id: 1,
        num_fields: 1,
        field_values: vec![Some(c10)],
        field_stores: vec![vec![FieldStore { store, value: c10 }]],
        replaced_loads: vec![load],
        load_values: vec![(load, LoadResolution::Value(c10))],
        eliminated_stores: vec![store],
        array_element_type: None,
        dominance_proved: true,
        value_phis: Vec::new(),
    };

    apply_scalar_replacement(&mut g, &info);
    assert_eq!(g.nodes[alloc].op, Op::Dead);
    assert_eq!(g.nodes[store].op, Op::Dead);
    assert_eq!(g.nodes[load].op, Op::Dead);
}

#[test]
fn test_apply_lock_elision_marks_dead() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    let enter = g.add_node(Op::MonitorEnter, vec![alloc]);
    let exit = g.add_node(Op::MonitorExit, vec![alloc]);

    apply_lock_elision(&mut g, &[enter, exit]);
    assert_eq!(g.nodes[enter].op, Op::Dead);
    assert_eq!(g.nodes[exit].op, Op::Dead);
}

#[test]
fn test_deferred_edge_propagation() {
    let mut cg = ConnectionGraph::new();
    cg.add_points_to(10, 10); // alloc 10 points to itself
    cg.add_deferred(20, 10); // node 20 copies from 10
    cg.add_deferred(30, 20); // node 30 copies from 20

    let pts = cg.resolve_points_to(30);
    assert!(pts.contains(&10));
}

#[test]
fn test_nested_object_stored_to_escaping() {
    let mut g = Graph::new();
    let outer = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let inner = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    let _store = g.add_node(Op::Store(0), vec![outer, inner]);
    // Outer escapes via call.
    let _call = g.add_node(Op::Call, vec![outer]);
    let result = analyze_escapes(&g);
    // Outer: ArgEscape (passed to call).
    assert!(result.escape_states.get(&outer).unwrap() >= &EscapeState::ArgEscape);
    // Inner: stored into outer which escapes, so inner escapes too.
    // At minimum ArgEscape via propagation.
    let inner_state = result.escape_states.get(&inner);
    assert!(inner_state.is_some());
}

#[test]
fn test_connection_graph_empty_for_arithmetic() {
    let mut g = Graph::new();
    let c1 = g.add_node(Op::Const(1), vec![]);
    let c2 = g.add_node(Op::Const(2), vec![]);
    let _add = g.add_node(Op::Add, vec![c1, c2]);
    let cg = build_connection_graph(&g);
    assert!(cg.points_to.is_empty());
    assert!(cg.field_edges.is_empty());
    assert!(cg.deferred_edges.is_empty());
}

#[test]
fn test_merge_point_escape_states_join_is_max() {
    // Two allocations, one NoEscape and one GlobalEscape, merged
    // through a phi. The phi should be GlobalEscape.
    let mut g = Graph::new();
    let a1 = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    let a2 = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    // a2 escapes globally.
    g.nodes[1].inputs.push(a2);
    g.nodes[a2].uses.push(1);
    let phi = g.add_node(Op::Phi, vec![a1, a2]);

    let mut cg = build_connection_graph(&g);
    propagate_escape_states(&mut cg, &g);

    // The phi merges a NoEscape and a GlobalEscape.
    // Since a2 is GlobalEscape, the phi should be at least GlobalEscape.
    let phi_state = cg.get_escape(phi);
    assert!(phi_state >= EscapeState::GlobalEscape);
}

#[test]
fn test_escape_state_join_lattice() {
    assert_eq!(
        EscapeState::NoEscape.join(EscapeState::NoEscape),
        EscapeState::NoEscape
    );
    assert_eq!(
        EscapeState::NoEscape.join(EscapeState::ArgEscape),
        EscapeState::ArgEscape
    );
    assert_eq!(
        EscapeState::ArgEscape.join(EscapeState::GlobalEscape),
        EscapeState::GlobalEscape
    );
    assert_eq!(
        EscapeState::NoEscape.join(EscapeState::GlobalEscape),
        EscapeState::GlobalEscape
    );
}

#[test]
fn test_scalar_replacement_not_applied_to_escaping() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    // Return the allocation -> GlobalEscape.
    g.nodes[1].inputs.push(alloc);
    g.nodes[alloc].uses.push(1);
    let result = analyze_escapes(&g);
    assert!(result.scalar_replaceable.is_empty());
}

#[test]
fn test_apply_scalar_replacement_redirects_uses() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c42 = g.add_node(Op::Const(42), vec![]);
    let store = g.add_node(Op::Store(0), vec![alloc, c42]);
    let load = g.add_node(Op::Load(0), vec![alloc]);
    // An Add that uses the load result.
    let add = g.add_node(Op::Add, vec![load, load]);

    let info = ScalarReplacementInfo {
        alloc_node: alloc,
        class_id: 1,
        num_fields: 1,
        field_values: vec![Some(c42)],
        field_stores: vec![vec![FieldStore { store, value: c42 }]],
        replaced_loads: vec![load],
        load_values: vec![(load, LoadResolution::Value(c42))],
        eliminated_stores: vec![store],
        array_element_type: None,
        dominance_proved: true,
        value_phis: Vec::new(),
    };

    apply_scalar_replacement(&mut g, &info);
    // The add should now reference c42 instead of the dead load.
    assert_eq!(g.nodes[add].inputs, vec![c42, c42]);
}

#[test]
fn test_no_escape_state_for_non_alloc_node() {
    let g = graph_local_alloc();
    let result = analyze_escapes(&g);
    // Const node (id=3) should not appear in escape_states.
    assert!(!result.escape_states.contains_key(&3));
}

#[test]
fn test_param_ref_in_phi_does_not_crash() {
    let mut g = Graph::new();
    let param = g.add_node(Op::Param(0), vec![]);
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    let _phi = g.add_node(Op::Phi, vec![param, alloc]);
    // Should not panic.
    let _ = analyze_escapes(&g);
}

#[test]
fn test_multiple_stores_to_same_field_last_wins() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let c2 = g.add_node(Op::Const(2), vec![]);
    let _s1 = g.add_node(Op::Store(0), vec![alloc, c1]);
    let _s2 = g.add_node(Op::Store(0), vec![alloc, c2]);
    let result = analyze_escapes(&g);
    // The last store wins in the field_values vector.
    let sr = &result.scalar_replaceable[0];
    assert_eq!(sr.field_values[0], Some(c2));
}

#[test]
fn test_new_array_without_constant_length_not_replaced() {
    // A `newarray` whose length the producer could not prove constant has no
    // slot count, so there is nothing to map onto `field_values`. It is
    // still NoEscape -- the refusal is about VALUE, not reachability.
    let mut g = Graph::new();
    let _arr = g.add_node(
        Op::NewArray {
            element_type: 4,
            length: None,
        },
        vec![],
    );
    let result = analyze_escapes(&g);
    assert!(result.scalar_replaceable.is_empty());
    assert_eq!(result.stats.no_escape, 1);
    assert_eq!(
        result.scalar_refusals,
        vec![(
            2,
            ScalarRefusal::ArrayNotReplaceable(ArrayRefusal::LengthNotConstant)
        )],
        "the refusal must name the missing length, not a generic bail"
    );
}

/// The `Short2` shape: a `short[2]`, both slots stored then both read.
///
/// This is the half of the per-voxel wrapper that `Op::New` scalar
/// replacement could never reach -- `docs/feature-designs/jit-ir-optimizations.md`
/// recorded it as `test_new_array_local_scalar_not_replaced`.
#[test]
fn test_constant_length_array_is_scalar_replaced() {
    let mut g = Graph::new();
    let arr = g.add_node(
        Op::NewArray {
            element_type: 9, // short
            length: Some(2),
        },
        vec![],
    );
    let v0 = g.add_node(Op::Const(7), vec![]);
    let v1 = g.add_node(Op::Const(9), vec![]);
    g.add_node(Op::Store(0), vec![arr, v0]);
    g.add_node(Op::Store(1), vec![arr, v1]);
    let l0 = g.add_node(Op::Load(0), vec![arr]);
    let l1 = g.add_node(Op::Load(1), vec![arr]);
    g.add_node(Op::Add, vec![l0, l1]);

    let result = analyze_escapes(&g);
    let info = result
        .scalar_replaceable
        .iter()
        .find(|i| i.alloc_node == arr)
        .expect("a two-slot array with constant indices is replaceable");
    assert_eq!(info.array_element_type, Some(9));
    assert_eq!(info.num_fields, 2);
    assert_eq!(info.field_values, vec![Some(v0), Some(v1)]);
    assert_eq!(info.load_value(l0), LoadResolution::Value(v0));
    assert_eq!(info.load_value(l1), LoadResolution::Value(v1));
}

/// JIT r20 lane zCp (`zcp20`, ZCP-5; page
/// `zcp20-code-point-latin1-host-gates-20261102.md`): the
/// shape `Character.toChars` leaves once its supplementary arm is pruned
/// -- `new char[1]` (`atype` 5), `cs[0] = (char) cp`, `cs[0]` read back --
/// and the two-unit surrogate pair of the other arm. The page's first step
/// ("does EA scalar-replace a non-escaping constant-length `newarray` of
/// 1 / 2 today") is answered by this test and
/// `test_constant_length_array_is_scalar_replaced` ("derived, not
/// measured": the analysis only, no splice of `toChars` was run).
#[test]
fn zcp20_char_arrays_of_one_and_two_are_scalar_replaced() {
    // The BMP arm: char[1].
    let mut g = Graph::new();
    let arr = g.add_node(
        Op::NewArray {
            element_type: 5, // char
            length: Some(1),
        },
        vec![],
    );
    let cp = g.add_node(Op::Const(0x41), vec![]);
    g.add_node(Op::Store(0), vec![arr, cp]);
    let rd = g.add_node(Op::Load(0), vec![arr]);
    g.add_node(Op::Add, vec![rd, cp]);
    let result = analyze_escapes(&g);
    let info = result
        .scalar_replaceable
        .iter()
        .find(|i| i.alloc_node == arr)
        .expect("a char[1] stored and read at constant index 0 is replaceable");
    assert_eq!(info.array_element_type, Some(5));
    assert_eq!(info.num_fields, 1);
    assert_eq!(info.field_values, vec![Some(cp)]);
    assert_eq!(info.load_value(rd), LoadResolution::Value(cp));

    // The supplementary arm: char[2], high then low surrogate.
    let mut g = Graph::new();
    let arr = g.add_node(
        Op::NewArray {
            element_type: 5,
            length: Some(2),
        },
        vec![],
    );
    let hi = g.add_node(Op::Const(0xD83D), vec![]);
    let lo = g.add_node(Op::Const(0xDE00), vec![]);
    g.add_node(Op::Store(0), vec![arr, hi]);
    g.add_node(Op::Store(1), vec![arr, lo]);
    let r0 = g.add_node(Op::Load(0), vec![arr]);
    let r1 = g.add_node(Op::Load(1), vec![arr]);
    g.add_node(Op::Add, vec![r0, r1]);
    let result = analyze_escapes(&g);
    let info = result
        .scalar_replaceable
        .iter()
        .find(|i| i.alloc_node == arr)
        .expect("a char[2] stored and read at constant indices is replaceable");
    assert_eq!(info.array_element_type, Some(5));
    assert_eq!(info.num_fields, 2);
    assert_eq!(info.field_values, vec![Some(hi), Some(lo)]);
    assert_eq!(info.load_value(r0), LoadResolution::Value(hi));
    assert_eq!(info.load_value(r1), LoadResolution::Value(lo));
}


#[test]
fn test_small_reference_array_is_replaceable() {
    // `anewarray` -> element_type 0: a varargs `Object[]` that never escapes
    // is replaced like a primitive array; its never-stored slot answers null.
    let mut g = Graph::new();
    let arr = g.add_node(
        Op::NewArray {
            element_type: 0,
            length: Some(2),
        },
        vec![],
    );
    let result = analyze_escapes(&g);
    let info = result
        .scalar_replaceable
        .iter()
        .find(|i| i.alloc_node == arr)
        .expect("an unescaped Object[2] is replaceable");
    assert_eq!(info.array_element_type, Some(0));
    assert_eq!(info.num_fields, 2);
}

#[test]
fn test_array_index_out_of_range_refuses_the_array() {
    // A constant index outside `0..len` throws `ArrayIndexOutOfBounds` at
    // run time; deleting the array would delete the throw.
    let mut g = Graph::new();
    let arr = g.add_node(
        Op::NewArray {
            element_type: 9,
            length: Some(2),
        },
        vec![],
    );
    let v = g.add_node(Op::Const(1), vec![]);
    g.add_node(Op::Store(5), vec![arr, v]);
    let result = analyze_escapes(&g);
    assert!(result.scalar_replaceable.is_empty());
    assert_eq!(
        result.scalar_refusals,
        vec![(arr, ScalarRefusal::FieldIndexOutOfRange)],
    );
}

#[test]
fn test_arraylength_on_a_replaced_array_is_refused() {
    // Nothing folds `arraylength` to the constant, so replacing the array
    // would leave a live node reading a deleted one.
    let mut g = Graph::new();
    let arr = g.add_node(
        Op::NewArray {
            element_type: 9,
            length: Some(2),
        },
        vec![],
    );
    g.add_node(Op::ArrayLength, vec![arr]);
    let result = analyze_escapes(&g);
    assert!(result.scalar_replaceable.is_empty());
    assert_eq!(
        result.scalar_refusals,
        vec![(
            arr,
            ScalarRefusal::ArrayNotReplaceable(ArrayRefusal::LengthReadNotFolded)
        )],
    );
}

/// The refusal instrument is only worth having if it names the fact that
/// actually stopped the object. This is the shape the per-voxel page hit:
/// a graph with a branch, an object whose accesses are all in one block.
#[test]
fn test_one_block_dominance_answers_a_load_a_branchy_graph_could_not() {
    // Build a graph with a branch somewhere else entirely, so
    // `program_order_proves_dominance` is false for the whole graph.
    let build = |same_block: bool| {
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
        g.blocks.insert(ld, if same_block { block } else { other });
        (g, obj, v, ld)
    };

    let (g, obj, v, ld) = build(true);
    assert!(
        !program_order_proves_dominance(&g),
        "precondition: the graph has a Merge, so the whole-graph stand-in fails"
    );
    let result = analyze_escapes(&g);
    let info = result
        .scalar_replaceable
        .iter()
        .find(|i| i.alloc_node == obj)
        .expect("every access is in one block, so the load is answerable");
    assert!(info.dominance_proved);
    assert_eq!(info.load_value(ld), LoadResolution::Value(v));

    // Move the load to another block and the proof must fail closed.
    let (g, obj, _, _) = build(false);
    let result = analyze_escapes(&g);
    assert!(result.scalar_replaceable.is_empty());
    assert_eq!(
        result.scalar_refusals,
        vec![(obj, ScalarRefusal::LoadNotAnswerable)],
        "a load in another block is not ordered by program order"
    );
}

#[test]
fn test_partial_escape_false_for_no_escape() {
    let g = graph_local_alloc();
    let cg = build_connection_graph(&g);
    // Alloc is node 2 and fully NoEscape.
    assert!(!is_partial_escape(&cg, &g, 2));
}

#[test]
fn test_materialization_empty_for_no_escape() {
    let g = graph_local_alloc();
    let cg = build_connection_graph(&g);
    let mps = find_materialization_points(&cg, &g, 2);
    assert!(mps.is_empty());
}

#[test]
fn test_connection_graph_resolve_cycle() {
    // Ensure cycles in deferred edges don't cause infinite loops.
    let mut cg = ConnectionGraph::new();
    cg.add_points_to(10, 10);
    cg.add_deferred(20, 10);
    cg.add_deferred(10, 20); // cycle!
    let pts = cg.resolve_points_to(20);
    assert!(pts.contains(&10));
}

#[test]
fn test_graph_new_helper() {
    let g = Graph::new();
    assert_eq!(g.nodes.len(), 2);
    assert_eq!(g.entry, 0);
    assert_eq!(g.exit, 1);
    assert_eq!(g.nodes[0].op, Op::Start);
    assert_eq!(g.nodes[1].op, Op::Return);
}

// ---- Load/Store input + field-index modeling (landmine pins) ----
//
// These tests pin the CANONICAL EA-internal Load/Store layout
// (`Store: [holder, value]`, `Load: [holder]`, payload = FIELD INDEX) so a
// future builder/lowering change that emits these nodes cannot silently
// miscompile the escape lattice.  They are the regression fence for the
// bugs described in the layout-contract comment near `Op::Load`/`Op::Store`.

/// Helper accessors must read holder/value from the documented positions.
#[test]
fn test_store_load_layout_accessors() {
    let mut g = Graph::new();
    let holder = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let value = g.add_node(Op::Const(7), vec![]);
    let store = g.add_node(Op::Store(0), vec![holder, value]);
    let load = g.add_node(Op::Load(0), vec![holder]);

    assert_eq!(store_holder(&g.nodes[store]), Some(holder));
    assert_eq!(store_value(&g.nodes[store]), Some(value));
    assert_eq!(load_holder(&g.nodes[load]), Some(holder));
}

/// A malformed store (holder only, no value) must NOT fabricate a value.
#[test]
fn test_store_missing_value_is_none() {
    let mut g = Graph::new();
    let holder = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let store = g.add_node(Op::Store(0), vec![holder]); // no value operand
    assert_eq!(store_value(&g.nodes[store]), None);
    // Build must not panic and must record no field edge for the missing value.
    let cg = build_connection_graph(&g);
    assert!(cg.field_edges.get(&(holder, 0)).is_none());
}

/// SOUNDNESS: a value stored into an *ArgEscape* holder must escape too.
/// (The pre-fix code only handled GlobalEscape, leaving this value
/// spuriously NoEscape — unsound for stack-allocation / lock elision.)
#[test]
fn test_value_stored_into_arg_escaping_holder_escapes() {
    let mut g = Graph::new();
    let holder = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let inner = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    // inner is stored into holder's field 0 ...
    let _store = g.add_node(Op::Store(0), vec![holder, inner]);
    // ... and holder is passed to a call (ArgEscape, NOT GlobalEscape).
    let _call = g.add_node(Op::Call, vec![holder]);

    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&holder),
        Some(&EscapeState::ArgEscape)
    );
    // inner is reachable through holder by the callee, so it must escape
    // at least as much as holder.
    let inner_state = *result.escape_states.get(&inner).unwrap();
    assert!(
        inner_state >= EscapeState::ArgEscape,
        "value stored into ArgEscape holder must escape >= ArgEscape, got {:?}",
        inner_state
    );
}

/// A load reads the FIELD's contents, not the holder: the load's
/// provenance/escape comes from the stored value, and the load must NOT
/// alias the holder allocation (the old `load -> holder` deferred edge).
#[test]
fn test_load_is_field_sensitive_not_holder_alias() {
    let mut g = Graph::new();
    let holder = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let stored = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    let _store = g.add_node(Op::Store(0), vec![holder, stored]);
    let load = g.add_node(Op::Load(0), vec![holder]);

    let mut cg = build_connection_graph(&g);
    propagate_escape_states(&mut cg, &g);

    let load_pts = cg.resolve_points_to(load);
    // The load points to what was stored into the field ...
    assert!(
        load_pts.contains(&stored),
        "field-sensitive load must point to the stored value"
    );
    // ... and crucially NOT to the holder allocation itself.
    assert!(
        !load_pts.contains(&holder),
        "load must not alias its holder object"
    );
}

/// Escaping the loaded value must escape the STORED value (through the
/// field), but must NOT, by itself, escape the holder via aliasing.
#[test]
fn test_returned_load_escapes_stored_value_only() {
    let mut g = Graph::new();
    let holder = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let stored = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    let _store = g.add_node(Op::Store(0), vec![holder, stored]);
    let load = g.add_node(Op::Load(0), vec![holder]);
    // Return the loaded value -> GlobalEscape flows to the stored value.
    g.nodes[1].inputs.push(load);
    g.nodes[load].uses.push(1);

    let result = analyze_escapes(&g);
    // The value read out of the field and returned escapes globally.
    assert_eq!(
        result.escape_states.get(&stored),
        Some(&EscapeState::GlobalEscape)
    );
}

/// An out-of-range field index (e.g. a `MemKind` tag mistakenly used as a
/// field index) must NOT mis-index a field: scalar replacement bails, and
/// the value still escapes conservatively if the holder escapes.
#[test]
fn test_out_of_range_field_index_is_conservative() {
    let mut g = Graph::new();
    let holder = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1, // only field 0 exists
        },
        vec![],
    );
    let value = g.add_node(Op::Const(3), vec![]);
    // field index 4 is out of range (mimics a leaked MemKind::Ref tag).
    let _store = g.add_node(Op::Store(4), vec![holder, value]);
    let _load = g.add_node(Op::Load(4), vec![holder]);

    let result = analyze_escapes(&g);
    // Holder cannot be scalar-replaced because of the out-of-range access.
    assert!(
        result.scalar_replaceable.is_empty(),
        "out-of-range field access must block scalar replacement"
    );
    // field_in_range rejects index 4 for a 1-field object.
    let allocs: HashSet<NodeId> = std::iter::once(holder).collect();
    assert!(!field_in_range(&g, &allocs, 4));
    assert!(field_in_range(&g, &allocs, 0));
}

/// A NoEscape holder must NOT escape values stored into it.
#[test]
fn test_value_stored_into_local_holder_does_not_escape() {
    let mut g = Graph::new();
    let holder = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let inner = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    let _store = g.add_node(Op::Store(0), vec![holder, inner]);
    // Neither holder nor inner escapes anywhere.
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&holder),
        Some(&EscapeState::NoEscape)
    );
    assert_eq!(
        result.escape_states.get(&inner),
        Some(&EscapeState::NoEscape)
    );
}

// ---- Widened scalar-replacement shapes (increment 1) ----

/// WIDENING: an allocation whose field load is reached *through* a
/// NoEscape phi that resolves solely to this allocation must now be
/// scalar-replaced.  The pre-widening shape rejected ANY phi use via the
/// catch-all, so this object stayed heap-allocated.
#[test]
fn test_scalar_replacement_through_transparent_phi() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        vec![],
    );
    let c5 = g.add_node(Op::Const(5), vec![]);
    // Store 5 into field 0 of the allocation.
    let _store = g.add_node(Op::Store(0), vec![alloc, c5]);
    // A phi that carries ONLY this allocation (e.g. `o = o` across a
    // merge): points-to set is the singleton {alloc}.
    let phi = g.add_node(Op::Phi, vec![alloc]);
    // Load field 0 THROUGH the phi (holder = phi, not alloc directly).
    let _load = g.add_node(Op::Load(0), vec![phi]);

    let result = analyze_escapes(&g);
    // The allocation does not escape ...
    assert_eq!(
        result.escape_states.get(&alloc),
        Some(&EscapeState::NoEscape)
    );
    // ... and is now scalar-replaceable despite the intervening phi.
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("allocation reached only through a transparent phi must scalar-replace");
    assert_eq!(sr.field_values[0], Some(c5));
    // The load through the phi is recorded for replacement.
    assert!(sr.replaced_loads.contains(&_load));
}

/// SOUNDNESS GUARD on the widening: a phi that merges this allocation
/// with a DIFFERENT allocation must NOT be treated as transparent — a
/// load through it could be reading the other object's field. Scalar
/// replacement must bail for both allocations.
#[test]
fn test_phi_merging_two_allocs_blocks_scalar_replacement() {
    let mut g = Graph::new();
    let a1 = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let a2 = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let c2 = g.add_node(Op::Const(2), vec![]);
    let _s1 = g.add_node(Op::Store(0), vec![a1, c1]);
    let _s2 = g.add_node(Op::Store(0), vec![a2, c2]);
    // phi merges two DISTINCT allocations.
    let phi = g.add_node(Op::Phi, vec![a1, a2]);
    // Load field 0 through the ambiguous phi.
    let _load = g.add_node(Op::Load(0), vec![phi]);

    let result = analyze_escapes(&g);
    // Neither allocation may be scalar-replaced: folding the load to one
    // object's field value would miscompile the other control path.
    assert!(
        result.scalar_replaceable.iter().all(|s| s.alloc_node != a1),
        "a1 must not scalar-replace through an ambiguous phi"
    );
    assert!(
        result.scalar_replaceable.iter().all(|s| s.alloc_node != a2),
        "a2 must not scalar-replace through an ambiguous phi"
    );
}

/// SOUNDNESS GUARD: a phi merging this allocation with a `Param` (unknown
/// provenance) resolves to the singleton {alloc} in the points-to set —
/// because the Param contributes no points-to entry — yet it genuinely
/// carries a foreign object on the Param path.  The widening must NOT
/// treat it as transparent; scalar replacement must bail.
#[test]
fn test_phi_merging_alloc_and_param_blocks_scalar_replacement() {
    let mut g = Graph::new();
    let param = g.add_node(Op::Param(0), vec![]);
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    // phi merges the allocation with an opaque parameter reference.
    let phi = g.add_node(Op::Phi, vec![alloc, param]);
    let _load = g.add_node(Op::Load(0), vec![phi]);

    let result = analyze_escapes(&g);
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "a phi merging the allocation with a Param must block scalar replacement"
    );
}

/// WIDENING: a stale `Op::Dead` node left in the allocation's use list
/// must be skipped, not treated as an unknown use that blocks SR.
#[test]
fn test_scalar_replacement_skips_dead_use() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c9 = g.add_node(Op::Const(9), vec![]);
    let store = g.add_node(Op::Store(0), vec![alloc, c9]);
    let _load = g.add_node(Op::Load(0), vec![alloc]);
    // Simulate an earlier pass having killed a former user of `alloc`
    // while leaving the use-edge dangling.
    let dead = g.add_node(Op::Other, vec![alloc]);
    g.nodes[dead].op = Op::Dead;

    let result = analyze_escapes(&g);
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("a stale Dead use must not block scalar replacement");
    assert_eq!(sr.field_values[0], Some(c9));
    assert!(sr.eliminated_stores.contains(&store));
}

/// An allocation used as a stored VALUE (published into another object's
/// field) must not be scalar-replaced.
#[test]
fn test_published_value_not_scalar_replaced() {
    let mut g = Graph::new();
    let outer = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let inner = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    let _store = g.add_node(Op::Store(0), vec![outer, inner]);
    // outer escapes via return -> inner is published into an escaping field.
    g.nodes[1].inputs.push(outer);
    g.nodes[outer].uses.push(1);

    let result = analyze_escapes(&g);
    // inner must not be in the scalar-replacement set.
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|sr| sr.alloc_node != inner),
        "a value published into an escaping object's field is not SR-eligible"
    );
}

// -- non-convergence must fail closed ---------------------------------

/// `escalate_all_to_global` has to cover nodes that were never entered in
/// the escape map at all: `get_escape` reports an absent node as
/// `NoEscape`, so leaving the map alone would keep exactly the unreached
/// nodes looking optimisable.
#[test]
fn escalation_covers_nodes_with_no_map_entry() {
    let mut g = Graph::new();
    let a = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let b = g.add_node(Op::Const(7), vec![]);
    let mut cg = ConnectionGraph::new();
    // Deliberately populate nothing: every node reads back as NoEscape.
    assert_eq!(cg.get_escape(a), EscapeState::NoEscape);
    assert_eq!(cg.get_escape(b), EscapeState::NoEscape);

    escalate_all_to_global(&mut cg, &g);

    for id in 0..g.nodes.len() {
        assert_eq!(
            cg.get_escape(id),
            EscapeState::GlobalEscape,
            "node {id} must be escalated"
        );
    }
}

/// After escalation neither live consumer may offer a candidate — that is
/// the whole point of failing closed.
#[test]
fn escalation_disables_scalar_replacement_and_lock_elision() {
    let mut g = Graph::new();
    let obj = g.add_node(
        Op::New {
            class_id: 3,
            num_fields: 1,
        },
        vec![],
    );
    let c = g.add_node(Op::Const(1), vec![]);
    let _st = g.add_node(Op::Store(0), vec![obj, c]);
    let _me = g.add_node(Op::MonitorEnter, vec![obj]);
    let _mx = g.add_node(Op::MonitorExit, vec![obj]);

    // Baseline: this shape IS optimisable when the fixed point converges.
    let converged = analyze_escapes(&g);
    assert!(
        !converged.scalar_replaceable.is_empty() || !converged.elide_locks.is_empty(),
        "fixture must be optimisable before escalation, else the test proves nothing"
    );

    // Now the failed-fixed-point answer.
    let mut cg = build_connection_graph(&g);
    escalate_all_to_global(&mut cg, &g);
    assert!(
        find_scalar_replacements_with(&cg, &g, ea_cross_block_loads_enabled())
            .0
            .is_empty(),
        "no allocation may be scalar-replaced after escalation"
    );
    assert!(
        find_lock_elisions(&cg, &g).is_empty(),
        "no monitor may be elided after escalation"
    );
}

/// The escalation must NOT fire on an ordinary graph — a fixed point that
/// converges keeps its precise answer.
#[test]
fn converging_graph_is_not_escalated() {
    let mut g = Graph::new();
    let obj = g.add_node(
        Op::New {
            class_id: 4,
            num_fields: 1,
        },
        vec![],
    );
    let c = g.add_node(Op::Const(42), vec![]);
    let _st = g.add_node(Op::Store(0), vec![obj, c]);
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&obj).copied(),
        Some(EscapeState::NoEscape),
        "a purely local allocation must stay NoEscape"
    );
    assert!(!result.scalar_replaceable.is_empty());
}

/// Documented, deliberately unwired output: `stack_allocatable` is
/// computed but no caller reads it. Pin the shape so a future consumer
/// knows what it is getting (ArgEscape allocations only — never NoEscape,
/// which goes to `scalar_replaceable`, and never GlobalEscape).
#[test]
fn stack_allocatable_holds_only_arg_escaping_allocations() {
    let mut g = Graph::new();
    let obj = g.add_node(
        Op::New {
            class_id: 5,
            num_fields: 0,
        },
        vec![],
    );
    // Passing the object to a call is ArgEscape, not GlobalEscape.
    let _call = g.add_node(Op::Call, vec![obj]);
    let result = analyze_escapes(&g);
    for &id in &result.stack_allocatable {
        assert_eq!(
            result.escape_states.get(&id).copied(),
            Some(EscapeState::ArgEscape),
            "stack_allocatable must contain exactly the ArgEscape allocations"
        );
    }
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|sr| sr.alloc_node != obj),
        "an ArgEscape allocation is not scalar-replaceable"
    );
}

// ══════════════════════════════════════════════════════════════════
// Positional (dominance-aware) store tracking
// ══════════════════════════════════════════════════════════════════
//
// The defect these fence: `field_values` records only the LAST store per
// field, and forwarding every load to it hands a load a value from its own
// future. `apply_ea_to_ir` currently *refuses* such an object; the analysis
// now answers it instead.

/// THE REGRESSION TEST. `Foo o = new Foo(); int a = o.x; o.x = 42;` must
/// fold `a` to the zero default. Folding it to `42` was the miscompilation.
#[test]
fn load_before_store_reads_the_zero_default_not_the_later_store() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    // `int a = o.x;` — BEFORE any store.
    let load = g.add_node(Op::Load(0), vec![alloc]);
    let c42 = g.add_node(Op::Const(42), vec![]);
    // `o.x = 42;` — after the read.
    let store = g.add_node(Op::Store(0), vec![alloc, c42]);

    let result = analyze_escapes(&g);
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("a purely local object is still replaceable");

    assert_eq!(
        sr.load_value(load),
        LoadResolution::ZeroDefault,
        "a load that precedes every store to its field reads the freshly \
         allocated object's zero value"
    );
    assert_ne!(
        sr.load_value(load),
        LoadResolution::Value(c42),
        "forwarding the load to a store in its own future is THE \
         miscompilation this test exists for"
    );
    // The last-write-wins map still says 42 — which is precisely why it is
    // the wrong forwarding source and `load_values` exists.
    assert_eq!(sr.field_values[0], Some(c42));
    assert_eq!(sr.field_stores[0], vec![FieldStore { store, value: c42 }]);
}

/// The paired POSITIVE: a load after the store reads that store.
#[test]
fn load_after_store_reads_that_store() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c42 = g.add_node(Op::Const(42), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c42]);
    let load = g.add_node(Op::Load(0), vec![alloc]);

    let result = analyze_escapes(&g);
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("must replace");
    assert_eq!(sr.load_value(load), LoadResolution::Value(c42));
    assert!(sr.dominance_proved);
}

/// Two stores straddling two loads: each load keeps the value that was in
/// the field when *it* ran. A single last-write-wins slot cannot express
/// this at all.
#[test]
fn interleaved_stores_and_loads_each_resolve_positionally() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let s1 = g.add_node(Op::Store(0), vec![alloc, c1]);
    let load_a = g.add_node(Op::Load(0), vec![alloc]);
    let c2 = g.add_node(Op::Const(2), vec![]);
    let s2 = g.add_node(Op::Store(0), vec![alloc, c2]);
    let load_b = g.add_node(Op::Load(0), vec![alloc]);

    let result = analyze_escapes(&g);
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("must replace");
    assert_eq!(sr.load_value(load_a), LoadResolution::Value(c1));
    assert_eq!(sr.load_value(load_b), LoadResolution::Value(c2));
    assert_eq!(
        sr.field_stores[0],
        vec![
            FieldStore {
                store: s1,
                value: c1
            },
            FieldStore {
                store: s2,
                value: c2
            }
        ],
        "the positional record keeps BOTH stores, in program order"
    );
    assert_eq!(
        sr.field_values[0],
        Some(c2),
        "the legacy last-write-wins map is unchanged for its remaining \
         consumer (the deopt recipe builder)"
    );
}

/// End-to-end through the in-module applier: the rewritten graph must give
/// each consumer the value its own load read.
#[test]
fn apply_scalar_replacement_forwards_each_load_to_its_own_store() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _s1 = g.add_node(Op::Store(0), vec![alloc, c1]);
    let load_a = g.add_node(Op::Load(0), vec![alloc]);
    let c2 = g.add_node(Op::Const(2), vec![]);
    let _s2 = g.add_node(Op::Store(0), vec![alloc, c2]);
    let load_b = g.add_node(Op::Load(0), vec![alloc]);
    // `a + b` — the consumer that would observe the wrong fold.
    let add = g.add_node(Op::Add, vec![load_a, load_b]);

    let result = analyze_escapes(&g);
    let info = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("must replace");
    apply_scalar_replacement(&mut g, info);

    assert_eq!(
        g.nodes[add].inputs,
        vec![c1, c2],
        "forwarding both loads to `field_values[0]` would make this \
         `[c2, c2]` — i.e. `a` folded to 2"
    );
}

/// An unresolvable load must leave the graph completely untouched — no
/// half-applied object.
#[test]
fn apply_scalar_replacement_refuses_an_unresolvable_load() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let store = g.add_node(Op::Store(0), vec![alloc, c1]);
    let load = g.add_node(Op::Load(0), vec![alloc]);

    let info = ScalarReplacementInfo {
        alloc_node: alloc,
        class_id: 1,
        num_fields: 1,
        field_values: vec![Some(c1)],
        field_stores: vec![vec![FieldStore { store, value: c1 }]],
        replaced_loads: vec![load],
        load_values: vec![(load, LoadResolution::Unknown)],
        eliminated_stores: vec![store],
        array_element_type: None,
        dominance_proved: false,
        value_phis: Vec::new(),
    };
    apply_scalar_replacement(&mut g, &info);

    assert!(
        matches!(g.nodes[alloc].op, Op::New { .. }),
        "the allocation must survive an unresolvable load"
    );
    assert!(matches!(g.nodes[load].op, Op::Load(_)));
    assert!(matches!(g.nodes[store].op, Op::Store(_)));
}

// ── The dominance gate ────────────────────────────────────────────

/// `if (c) o.x = 1; else o.x = 2; int a = o.x;` — both stores precede the
/// load in program order and NEITHER dominates it. Program order is not
/// dominance here, so the object must be refused rather than folded to the
/// textually-last store.
#[test]
fn a_branch_makes_program_order_stop_proving_dominance() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let cond = g.add_node(Op::Const(0), vec![]);
    let _iff = g.add_node(Op::If, vec![cond]);
    let c1 = g.add_node(Op::Const(1), vec![]);
    let c2 = g.add_node(Op::Const(2), vec![]);
    let _s1 = g.add_node(Op::Store(0), vec![alloc, c1]);
    let _s2 = g.add_node(Op::Store(0), vec![alloc, c2]);
    let _load = g.add_node(Op::Load(0), vec![alloc]);

    assert!(!program_order_proves_dominance(&g));
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&alloc),
        Some(&EscapeState::NoEscape),
        "the object still does not escape — this is a VALUE question, not \
         a reachability one"
    );
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "a load that could read either of two conditional stores must not \
         be folded to one of them"
    );
}

/// The paired POSITIVE for the gate: a field with **no** store anywhere
/// resolves even in a branchy graph — no control flow can change what a
/// never-written field holds.
#[test]
fn a_never_stored_field_resolves_even_without_dominance() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 2,
        },
        vec![],
    );
    let cond = g.add_node(Op::Const(0), vec![]);
    let _iff = g.add_node(Op::If, vec![cond]);
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _s0 = g.add_node(Op::Store(0), vec![alloc, c1]);
    // Field 1 is never stored, anywhere.
    let load = g.add_node(Op::Load(1), vec![alloc]);

    let result = analyze_escapes(&g);
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("a never-stored field is answerable with or without a CFG");
    assert!(!sr.dominance_proved);
    assert_eq!(sr.load_value(load), LoadResolution::ZeroDefault);
}

/// The three shapes that retract the dominance stand-in, and the one that
/// does not (a single-input φ is a copy, not a join).
#[test]
fn program_order_dominance_predicate_shapes() {
    let mut branch = Graph::new();
    let c = branch.add_node(Op::Const(0), vec![]);
    branch.add_node(Op::If, vec![c]);
    assert!(!program_order_proves_dominance(&branch));

    let mut join = Graph::new();
    join.add_node(Op::Merge, vec![]);
    assert!(!program_order_proves_dominance(&join));

    let mut merge_phi = Graph::new();
    let a = merge_phi.add_node(Op::Const(1), vec![]);
    let b = merge_phi.add_node(Op::Const(2), vec![]);
    merge_phi.add_node(Op::Phi, vec![a, b]);
    assert!(!program_order_proves_dominance(&merge_phi));

    let mut copy_phi = Graph::new();
    let a = copy_phi.add_node(Op::Const(1), vec![]);
    copy_phi.add_node(Op::Phi, vec![a]);
    assert!(
        program_order_proves_dominance(&copy_phi),
        "a single-input φ is a degenerate copy and implies no divergence"
    );
}

// ══════════════════════════════════════════════════════════════════
// Identity sensitivity
// ══════════════════════════════════════════════════════════════════

/// An object whose reference is compared with `==` has its ADDRESS
/// observed. It does not escape, and it still may not be replaced.
#[test]
fn object_compared_by_identity_is_not_scalar_replaced() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    let other = g.add_node(Op::Param(0), vec![]);
    let cmp = g.add_node(Op::RefCompare, vec![alloc, other]);

    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&alloc),
        Some(&EscapeState::NoEscape),
        "an acmp does not publish the object — escape and identity are \
         different questions"
    );
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "an object with a live identity comparison must not be replaced: \
         a bag of scalars has no address to compare"
    );
    assert!(result.identity_observations.contains(&(alloc, cmp)));
    assert_eq!(result.stats.identity_blocked, 1);
}

/// The paired POSITIVE: the same object without the comparison replaces.
#[test]
fn object_not_compared_by_identity_is_scalar_replaced() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    let load = g.add_node(Op::Load(0), vec![alloc]);

    let result = analyze_escapes(&g);
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("must replace");
    assert_eq!(sr.load_value(load), LoadResolution::Value(c1));
    assert!(result.identity_observations.is_empty());
    assert_eq!(result.stats.identity_blocked, 0);
}

/// An identity hash is a header read: same rule as `==`.
#[test]
fn identity_hash_blocks_scalar_replacement() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    let ihash = g.add_node(Op::IdentityHash, vec![alloc]);

    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&alloc),
        Some(&EscapeState::NoEscape)
    );
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "a scalar-replaced object has no header to hash"
    );
    assert!(result.identity_observations.contains(&(alloc, ihash)));
}

/// An identity observation reached through a transparent φ still counts —
/// the alias is the same address.
#[test]
fn identity_observed_through_a_phi_alias_blocks_replacement() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    let phi = g.add_node(Op::Phi, vec![alloc]);
    let _cmp = g.add_node(Op::RefCompare, vec![phi]);

    let result = analyze_escapes(&g);
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "the φ resolves to this allocation, so comparing the φ compares \
         this object's address"
    );
}

/// SYNCHRONIZATION is an identity observation: the monitor *is* the object
/// header. A live `monitorenter` blocks replacement even though the lock is
/// simultaneously offered for elision — because the consumer may refuse
/// that elision (`apply_ea_to_ir` does, for a monitor a safepoint names).
///
/// The paired POSITIVE is in the same test: once the monitors are actually
/// eliminated, a second run replaces the object. That two-phase order is
/// the supported way to get both.
#[test]
fn synchronized_object_is_not_replaced_until_the_monitor_is_eliminated() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 1,
        },
        vec![],
    );
    let c7 = g.add_node(Op::Const(7), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c7]);
    let _enter = g.add_node(Op::MonitorEnter, vec![alloc]);
    let _exit = g.add_node(Op::MonitorExit, vec![alloc]);
    let load = g.add_node(Op::Load(0), vec![alloc]);

    let first = analyze_escapes(&g);
    assert_eq!(
        first.escape_states.get(&alloc),
        Some(&EscapeState::NoEscape),
        "locking a local object does not make it escape"
    );
    assert!(
        first
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "MUST NOT: a live monitor observes the object's identity"
    );
    assert_eq!(
        first.elide_locks.len(),
        2,
        "the elision is still OFFERED — it is sound on its own terms"
    );
    assert_eq!(first.stats.identity_blocked, 1);

    // Phase two: actually eliminate the observation, then re-analyse.
    apply_lock_elision(&mut g, &first.elide_locks);
    let second = analyze_escapes(&g);
    let sr = second
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("MUST: with the monitors dead, nothing observes the identity");
    assert_eq!(sr.load_value(load), LoadResolution::Value(c7));
    assert_eq!(second.stats.identity_blocked, 0);
}

// ══════════════════════════════════════════════════════════════════
// The lattice: partial escape and the global-escape rules
// ══════════════════════════════════════════════════════════════════

#[test]
fn lattice_order_places_partial_between_no_escape_and_arg_escape() {
    assert!(EscapeState::NoEscape < EscapeState::PartialEscape);
    assert!(EscapeState::PartialEscape < EscapeState::ArgEscape);
    assert!(EscapeState::ArgEscape < EscapeState::GlobalEscape);
    // `may_escape`, not `>= ArgEscape`, is the "escapes" predicate.
    assert!(!EscapeState::NoEscape.may_escape());
    assert!(EscapeState::PartialEscape.may_escape());
    assert!(EscapeState::NoEscape.is_confined());
    assert!(!EscapeState::PartialEscape.is_confined());
}

/// An object that escapes only through a node the producer marked cold is
/// classified `PartialEscape` — and that classification licenses nothing.
#[test]
fn object_escaping_only_on_a_cold_path_is_classified_partial() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 3,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    // The only escape: a slow-path helper the producer knows is cold.
    let cold_call = g.add_node(Op::Call, vec![alloc]);
    g.mark_cold(cold_call);

    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&alloc),
        Some(&EscapeState::PartialEscape)
    );
    let pe = result
        .partial_escapes
        .iter()
        .find(|p| p.alloc_node == alloc)
        .expect("the refinement records what it refined");
    assert_eq!(pe.without_refinement, EscapeState::ArgEscape);
    assert_eq!(pe.escape_sites, vec![cold_call]);
    assert_eq!(result.stats.partial_escape, 1);
    assert_eq!(result.stats.arg_escape, 0);

    // A refinement is NOT a licence. Nothing may act on it.
    assert!(
        result
            .scalar_replaceable
            .iter()
            .all(|s| s.alloc_node != alloc),
        "a partially-escaping object is still not scalar-replaceable"
    );
    assert!(
        !result.stack_allocatable.contains(&alloc),
        "nor stack-allocatable — the cold path would have to copy it out"
    );
}

/// The paired NEGATIVE control: the same escape on a HOT path stays
/// `ArgEscape`, and one hot site among cold ones is enough to keep it.
#[test]
fn a_single_hot_escape_site_defeats_the_partial_classification() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 3,
            num_fields: 0,
        },
        vec![],
    );
    let cold_call = g.add_node(Op::Call, vec![alloc]);
    let _hot_call = g.add_node(Op::Call, vec![alloc]);
    g.mark_cold(cold_call);

    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&alloc),
        Some(&EscapeState::ArgEscape),
        "one escape site that is not cold means the object escapes on the \
         hot path"
    );
    assert!(result.partial_escapes.is_empty());
    assert_eq!(result.stats.partial_escape, 0);
    assert_eq!(result.stats.arg_escape, 1);
}

/// With no cold information at all — the default — the classification is
/// exactly what it was before partial escape existed.
#[test]
fn no_cold_information_means_no_partial_classification() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 3,
            num_fields: 0,
        },
        vec![],
    );
    let _call = g.add_node(Op::Call, vec![alloc]);
    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&alloc),
        Some(&EscapeState::ArgEscape)
    );
    assert!(result.partial_escapes.is_empty());
}

/// A value written into a holder this analysis cannot NAME — a `putstatic`
/// base, or any opaque reference with no points-to entry — must escape
/// globally.
///
/// Before this rule the store-escape check read the holder's state with
/// `get_escape`, which reports an unseen node as `NoEscape`, so the rule
/// simply did not fire: an object published into a **static field** stayed
/// `NoEscape`, hence scalar-replaceable and lock-elidable, while being
/// reachable from every thread in the VM.
#[test]
fn object_stored_into_an_unnameable_holder_is_global_escape() {
    let mut g = Graph::new();
    // The static-area base: not an allocation, not a Param — opaque.
    let statics = g.add_node(Op::Other, vec![]);
    let obj = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    let _putstatic = g.add_node(Op::Store(0), vec![statics, obj]);

    let result = analyze_escapes(&g);
    assert_eq!(
        result.escape_states.get(&obj),
        Some(&EscapeState::GlobalEscape),
        "a write to an unnameable destination is a write to the global heap"
    );
    assert!(result.scalar_replaceable.is_empty());
    assert!(result.elide_locks.is_empty());
}

/// An object passed to an unknown call escapes. It is `ArgEscape`, not
/// `GlobalEscape` — that is what the middle lattice element is FOR (the
/// callee can reach it; the global heap cannot, unless the callee publishes
/// it, which is the callee's own analysis). Either way it is not
/// replaceable.
#[test]
fn object_passed_to_an_unknown_call_escapes_and_is_not_replaced() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    let _call = g.add_node(Op::Call, vec![alloc]);

    let result = analyze_escapes(&g);
    let state = *result.escape_states.get(&alloc).expect("classified");
    assert!(state.may_escape());
    assert_eq!(state, EscapeState::ArgEscape);
    assert!(result
        .scalar_replaceable
        .iter()
        .all(|s| s.alloc_node != alloc));
}

/// The paired POSITIVE for both escape rules: an object that is neither
/// published nor passed anywhere stays confined and IS replaced.
#[test]
fn a_purely_local_object_stays_confined_and_is_replaced() {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c1 = g.add_node(Op::Const(1), vec![]);
    let _store = g.add_node(Op::Store(0), vec![alloc, c1]);
    let load = g.add_node(Op::Load(0), vec![alloc]);

    let result = analyze_escapes(&g);
    assert!(result.escape_states[&alloc].is_confined());
    let sr = result
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == alloc)
        .expect("MUST replace");
    assert_eq!(sr.load_value(load), LoadResolution::Value(c1));
}

/// The four state counters partition the allocations — a `PartialEscape`
/// object is counted once, in its own bucket.
#[test]
fn state_counters_partition_the_allocations() {
    let mut g = Graph::new();
    let local = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        vec![],
    );
    let arg = g.add_node(
        Op::New {
            class_id: 2,
            num_fields: 0,
        },
        vec![],
    );
    let cold = g.add_node(
        Op::New {
            class_id: 3,
            num_fields: 0,
        },
        vec![],
    );
    let global = g.add_node(
        Op::New {
            class_id: 4,
            num_fields: 0,
        },
        vec![],
    );
    let _ = local;
    let _arg_call = g.add_node(Op::Call, vec![arg]);
    let cold_call = g.add_node(Op::Call, vec![cold]);
    g.mark_cold(cold_call);
    g.nodes[1].inputs.push(global); // Return
    g.nodes[global].uses.push(1);

    let s = analyze_escapes(&g).stats;
    assert_eq!(s.total_allocations, 4);
    assert_eq!(s.no_escape, 1);
    assert_eq!(s.partial_escape, 1);
    assert_eq!(s.arg_escape, 1);
    assert_eq!(s.global_escape, 1);
    assert_eq!(
        s.no_escape + s.partial_escape + s.arg_escape + s.global_escape,
        s.total_allocations
    );
}

// ══════════════════════════════════════════════════════════════════
// Lock elimination and coarsening
// ══════════════════════════════════════════════════════════════════
//
// Every "MUST NOT" below is paired with the "MUST" that proves the refusal
// is a refusal and not an inability.

/// A fresh confined object with `num_fields` fields.
fn alloc(g: &mut Graph, class_id: u32, num_fields: usize) -> NodeId {
    g.add_node(
        Op::New {
            class_id,
            num_fields,
        },
        vec![],
    )
}

/// The monitor nodes of `object`, ascending — what the graph *still* holds
/// after a transform, independent of any plan.
fn live_monitors_of(g: &Graph, object: NodeId) -> Vec<NodeId> {
    (0..g.nodes.len())
        .filter(|&id| matches!(g.nodes[id].op, Op::MonitorEnter | Op::MonitorExit))
        .filter(|&id| monitor_object(g, id) == Some(object))
        .collect()
}

// ── (a) Lock elision ──────────────────────────────────────────────

/// MUST: a confined object's monitor is elided, and it is offered as ONE
/// all-or-nothing plan naming every monitor on the object.
#[test]
fn confined_object_lock_is_elided_as_one_complete_plan() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 1);
    let c = g.add_node(Op::Const(1), vec![]);
    let enter = g.add_node(Op::MonitorEnter, vec![o]);
    let _st = g.add_node(Op::Store(0), vec![o, c]);
    let exit = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert_eq!(r.escape_states.get(&o), Some(&EscapeState::NoEscape));
    assert_eq!(r.lock_elisions.len(), 1);
    assert_eq!(r.lock_elisions[0].object, o);
    assert_eq!(
        r.lock_elisions[0].monitors,
        vec![enter, exit],
        "the plan names EVERY monitor on the object — that is what makes \
         it all-or-nothing"
    );
    assert_eq!(r.elide_locks, vec![enter, exit]);
    assert_eq!(r.stats.locks_elided, 2);
    assert_eq!(r.stats.lock_objects_elided, 1);
    assert!(r.lock_refusals.is_empty());
}

/// t1 wave 2, lane M: under a monitor observer the offer is withdrawn
/// whole, and the object stays unreplaceable (its monitor ops are live
/// identity observations), so allocation and lock both stay real.
#[test]
fn t1m_withheld_lock_elision_keeps_object_and_lock() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 1);
    let c = g.add_node(Op::Const(1), vec![]);
    let _enter = g.add_node(Op::MonitorEnter, vec![o]);
    let _st = g.add_node(Op::Store(0), vec![o, c]);
    let _exit = g.add_node(Op::MonitorExit, vec![o]);

    let mut r = analyze_escapes(&g);
    assert_eq!(withhold_lock_elisions(&mut r), 1);
    assert!(r.lock_elisions.is_empty());
    assert!(r.elide_locks.is_empty());
    assert_eq!(r.stats.locks_elided, 0);
    assert_eq!(r.stats.lock_objects_elided, 0);
    assert!(
        !r.scalar_replaceable.iter().any(|s| s.alloc_node == o),
        "a locked object is never replaced while its monitor ops are live"
    );
    // Idempotent: a second round withholds nothing more.
    assert_eq!(withhold_lock_elisions(&mut r), 0);
}

/// t1 wave 3, lane M: under an object observer a confined, replaceable
/// allocation is withdrawn and stays real.
#[test]
fn t1m_withheld_scalar_replacement_keeps_the_allocation() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 1);
    let c = g.add_node(Op::Const(1), vec![]);
    let _st = g.add_node(Op::Store(0), vec![o, c]);
    let _ld = g.add_node(Op::Load(0), vec![o]);

    let mut r = analyze_escapes(&g);
    let before = r.scalar_replaceable.len();
    assert_eq!(withhold_scalar_replacements(&mut r), before);
    assert!(r.scalar_replaceable.is_empty());
    assert_eq!(r.stats.scalar_replaced, 0);
    assert_eq!(withhold_scalar_replacements(&mut r), 0);
}

/// MUST NOT: `wait()` requires the monitor to actually be held. Eliding it
/// turns a legal wait into `IllegalMonitorStateException`, and there is no
/// depth left to release to or reacquire at.
///
/// Paired MUST: the identical graph without the `wait` elides.
#[test]
fn object_with_wait_is_never_elided() {
    let build = |with_wait: bool| {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let _enter = g.add_node(Op::MonitorEnter, vec![o]);
        if with_wait {
            g.add_node(Op::MonitorWait, vec![o]);
        }
        let _exit = g.add_node(Op::MonitorExit, vec![o]);
        (g, o)
    };

    let (g, o) = build(true);
    let r = analyze_escapes(&g);
    assert_eq!(
        r.escape_states.get(&o),
        Some(&EscapeState::NoEscape),
        "waiting on a local object does not publish it — this is not an \
         escape question"
    );
    assert!(
        r.elide_locks.is_empty(),
        "MUST NOT elide a waited-on monitor"
    );
    assert!(r.lock_elisions.is_empty());
    assert!(r.lock_coarsening.is_empty());
    assert!(r.lock_refusals.contains(&(o, LockRefusal::WaitOrNotify)));

    let (g, o) = build(false);
    let r = analyze_escapes(&g);
    assert_eq!(
        r.lock_elisions.len(),
        1,
        "MUST elide once nothing waits on the monitor"
    );
    assert_eq!(r.lock_elisions[0].object, o);
}

/// `notify`/`notifyAll` are the same rule: they too throw when the monitor
/// is not held, and a thread they could wake is a thread that reached the
/// object.
#[test]
fn object_with_notify_is_never_elided() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let _enter = g.add_node(Op::MonitorEnter, vec![o]);
    let _notify = g.add_node(Op::MonitorNotify, vec![o]);
    let _exit = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert!(r.elide_locks.is_empty());
    assert!(r.lock_refusals.contains(&(o, LockRefusal::WaitOrNotify)));
}

/// A `wait` on a **caller-supplied** reference cannot be a wait on an
/// object this frame allocated and never published, so it must not block
/// that object's elision. The `Foreign` half of `OperandOrigin` is what
/// keeps `synchronized (local) {} … this.wait();` optimisable.
#[test]
fn a_wait_on_a_foreign_reference_does_not_block_a_local_object() {
    let mut g = Graph::new();
    let this = g.add_node(Op::Param(0), vec![]);
    let o = alloc(&mut g, 1, 0);
    let enter = g.add_node(Op::MonitorEnter, vec![o]);
    let exit = g.add_node(Op::MonitorExit, vec![o]);
    let _wait = g.add_node(Op::MonitorWait, vec![this]);

    let r = analyze_escapes(&g);
    assert_eq!(r.elide_locks, vec![enter, exit]);
    assert!(!r
        .lock_refusals
        .iter()
        .any(|&(_, why)| why == LockRefusal::WaitOrNotify));
}

/// MUST NOT: a monitor whose operand may be either a local allocation or a
/// caller-supplied reference is attributable to neither, and it poisons
/// **every** lock plan in the method — including the unrelated object's,
/// whose monitors would otherwise be elided.
///
/// A singleton points-to set would have said "this is `a`" and elided a
/// lock that, on the parameter path, guards an object other threads share.
///
/// Paired MUST: replace the parameter input with the allocation itself and
/// both objects elide.
#[test]
fn an_unattributable_monitor_operand_poisons_every_lock_plan() {
    let build = |mix_in_param: bool| {
        let mut g = Graph::new();
        let p = g.add_node(Op::Param(0), vec![]);
        let a = alloc(&mut g, 1, 0);
        let b = alloc(&mut g, 2, 0);
        let other = if mix_in_param { p } else { a };
        let phi = g.add_node(Op::Phi, vec![a, other]);
        let amb = g.add_node(Op::MonitorEnter, vec![phi]);
        let _ambx = g.add_node(Op::MonitorExit, vec![phi]);
        let _be = g.add_node(Op::MonitorEnter, vec![b]);
        let _bx = g.add_node(Op::MonitorExit, vec![b]);
        (g, a, b, amb)
    };

    let (g, _a, b, amb) = build(true);
    let r = analyze_escapes(&g);
    assert!(
        r.elide_locks.is_empty(),
        "MUST NOT elide anything while one monitor is unattributable — \
         including object {b}'s own, perfectly confined, lock"
    );
    assert!(r.lock_coarsening.is_empty());
    assert!(r
        .lock_refusals
        .contains(&(amb, LockRefusal::AmbiguousMonitorOperand)));

    let (g, a, b, _) = build(false);
    let r = analyze_escapes(&g);
    let objects: Vec<NodeId> = r.lock_elisions.iter().map(|p| p.object).collect();
    assert!(
        objects.contains(&a) && objects.contains(&b),
        "MUST elide both once every operand names exactly one allocation"
    );
}

/// MUST NOT: an escaping object's monitor is real. Another thread can
/// contend on it, so both transforms are refused.
#[test]
fn escaping_object_lock_is_neither_elided_nor_coarsened() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    g.nodes[1].inputs.push(o); // Return
    g.nodes[o].uses.push(1);
    let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x1 = g.add_node(Op::MonitorExit, vec![o]);
    let _c = g.add_node(Op::Const(0), vec![]);
    let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x2 = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert_eq!(r.escape_states.get(&o), Some(&EscapeState::GlobalEscape));
    assert!(r.elide_locks.is_empty());
    assert!(
        r.lock_coarsening.is_empty(),
        "MUST NOT coarsen an escaping object: extending its critical \
         section can block a thread that wanted the monitor in the gap"
    );
    assert!(r.lock_refusals.contains(&(o, LockRefusal::ObjectEscapes)));
}

// ── Recursive locking ─────────────────────────────────────────────

/// A reentrant `synchronized (o) { synchronized (o) { … } }` round-trips:
/// the regions pair at the right depths, the elision offer names all four
/// monitors, and applying it leaves an object with no monitors at all.
#[test]
fn recursive_locking_round_trips() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let x1 = g.add_node(Op::MonitorExit, vec![o]);
    let x2 = g.add_node(Op::MonitorExit, vec![o]);

    let regions = lock_regions(&g, o).expect("a balanced nest pairs up");
    assert_eq!(regions.len(), 2);
    assert_eq!(
        regions[0],
        LockRegion {
            enter: e1,
            exit: x2,
            depth: 0
        },
        "the OUTER region is e1..x2"
    );
    assert_eq!(
        regions[1],
        LockRegion {
            enter: e2,
            exit: x1,
            depth: 1
        },
        "the INNER region is e2..x1 — pairing by nesting, not by order"
    );

    let r = analyze_escapes(&g);
    assert_eq!(r.lock_elisions.len(), 1);
    assert_eq!(r.lock_elisions[0].monitors, vec![e1, e2, x1, x2]);

    let plan = r.lock_elisions[0].clone();
    assert!(apply_lock_elision_plan(&mut g, &plan));
    assert!(
        live_monitors_of(&g, o).is_empty(),
        "the whole nest goes, so the depth is 0 before and after"
    );
    assert!(lock_regions(&g, o).expect("still balanced").is_empty());
}

/// Removing the INNER pair of a reentrant nest is legal (one level of
/// reentry disappears, the outer region is untouched). Removing `e2` and
/// `x2` — which are not a pair — is not: it would silently narrow the outer
/// region. Both requests are "balanced" by a naive depth count; only the
/// region-pairing check tells them apart.
#[test]
fn partial_elision_is_accepted_only_when_whole_regions_go() {
    let build = || {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let x1 = g.add_node(Op::MonitorExit, vec![o]);
        let x2 = g.add_node(Op::MonitorExit, vec![o]);
        (g, o, e1, e2, x1, x2)
    };

    let (mut g, o, e1, e2, x1, x2) = build();
    assert!(
        apply_lock_elision(&mut g, &[e2, x1]),
        "MUST: the inner region is a whole region"
    );
    assert_eq!(live_monitors_of(&g, o), vec![e1, x2]);

    let (mut g, o, e1, e2, x1, x2) = build();
    assert!(
        !apply_lock_elision(&mut g, &[e2, x2]),
        "MUST NOT: e2 pairs with x1, not x2"
    );
    assert_eq!(
        live_monitors_of(&g, o),
        vec![e1, e2, x1, x2],
        "a refused request leaves the graph completely untouched"
    );
}

/// THE PARTIAL-APPLICATION GUARD. `apply_ea_to_ir` filters `elide_locks`
/// node by node and can hand back a strict subset; a lone `monitorexit`
/// throws `IllegalMonitorStateException` and a lone `monitorenter` leaks the
/// monitor. The applier refuses rather than obeys.
#[test]
fn a_half_applied_elision_request_is_refused_whole() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let enter = g.add_node(Op::MonitorEnter, vec![o]);
    let exit = g.add_node(Op::MonitorExit, vec![o]);

    assert!(
        !apply_lock_elision(&mut g, &[enter]),
        "MUST NOT drop the enter and keep the exit"
    );
    assert!(!apply_lock_elision(&mut g, &[exit]));
    assert_eq!(g.nodes[enter].op, Op::MonitorEnter);
    assert_eq!(g.nodes[exit].op, Op::MonitorExit);

    assert!(
        apply_lock_elision(&mut g, &[enter, exit]),
        "MUST apply the complete pair"
    );
    assert_eq!(g.nodes[enter].op, Op::Dead);
    assert_eq!(g.nodes[exit].op, Op::Dead);
}

/// An unbalanced monitor sequence is not a sequence this analysis
/// understands, and `lock_regions` says so rather than guessing.
#[test]
fn an_unbalanced_monitor_sequence_is_refused() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let _e = g.add_node(Op::MonitorEnter, vec![o]);
    assert_eq!(lock_regions(&g, o), Err(LockRefusal::UnbalancedMonitors));

    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let _x = g.add_node(Op::MonitorExit, vec![o]);
    assert_eq!(lock_regions(&g, o), Err(LockRefusal::UnbalancedMonitors));
}

// ── (b) Lock coarsening ───────────────────────────────────────────

/// MUST: two adjacent regions on a confined object, separated by pure
/// arithmetic, merge — and applying the plan leaves exactly one balanced
/// region.
#[test]
fn two_adjacent_regions_on_a_confined_object_are_coarsened() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 1);
    let c1 = g.add_node(Op::Const(1), vec![]);
    let e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _s1 = g.add_node(Op::Store(0), vec![o, c1]);
    let x1 = g.add_node(Op::MonitorExit, vec![o]);
    // The gap: pure, total, no safepoint, no throw.
    let c2 = g.add_node(Op::Const(2), vec![]);
    let sum = g.add_node(Op::Add, vec![c1, c2]);
    let e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let _s2 = g.add_node(Op::Store(0), vec![o, sum]);
    let x2 = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert_eq!(r.lock_coarsening.len(), 1);
    assert_eq!(r.stats.locks_coarsened, 1);
    let plan = r.lock_coarsening[0];
    assert_eq!(plan.object, o);
    assert_eq!(plan.surviving_enter(), e1);
    assert_eq!(plan.removed_exit(), x1);
    assert_eq!(plan.removed_enter(), e2);
    assert_eq!(plan.surviving_exit(), x2);

    assert!(apply_lock_coarsening(&mut g, &plan));
    assert_eq!(g.nodes[x1].op, Op::Dead);
    assert_eq!(g.nodes[e2].op, Op::Dead);
    assert_eq!(
        lock_regions(&g, o).expect("still balanced"),
        vec![LockRegion {
            enter: e1,
            exit: x2,
            depth: 0
        }],
        "one region, entered once and exited once"
    );
}

/// MUST NOT: a throw between the two regions. In the source program the
/// monitor is NOT held at the throw, so unwinding releases nothing;
/// coarsening would make the compiled frame hold it there.
///
/// The preservation argument is by exclusion, and this test states it as
/// such: the plan is refused, the two regions survive untouched, and the
/// throw still lies in the unlocked window between them.
///
/// Paired MUST: replace the throw with an `Add` and the merge happens.
#[test]
fn a_throw_between_two_regions_refuses_the_merge_and_still_unlocks() {
    let build = |throwing: bool| {
        let mut g = Graph::new();
        let p = g.add_node(Op::Param(0), vec![]);
        let o = alloc(&mut g, 1, 0);
        let e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let x1 = g.add_node(Op::MonitorExit, vec![o]);
        let gap = if throwing {
            g.add_node(Op::Throw, vec![p])
        } else {
            g.add_node(Op::Add, vec![p, p])
        };
        let e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let x2 = g.add_node(Op::MonitorExit, vec![o]);
        (g, o, e1, x1, gap, e2, x2)
    };

    let (mut g, o, e1, x1, throw, e2, x2) = build(true);
    let r = analyze_escapes(&g);
    assert!(
        r.lock_coarsening.is_empty(),
        "MUST NOT coarsen across a node that can unwind monitors"
    );
    assert!(r.lock_refusals.contains(&(o, LockRefusal::MayThrowInGap)));

    // Nothing to apply, so unlock-on-throw is bit-for-bit the original's:
    // the throw sits strictly inside the window where `o` is unlocked.
    for plan in &r.lock_coarsening {
        apply_lock_coarsening(&mut g, plan);
    }
    let regions = lock_regions(&g, o).expect("balanced");
    assert_eq!(
        regions,
        vec![
            LockRegion {
                enter: e1,
                exit: x1,
                depth: 0
            },
            LockRegion {
                enter: e2,
                exit: x2,
                depth: 0
            }
        ]
    );
    assert!(
        throw > regions[0].exit && throw < regions[1].enter,
        "the throw is in the unlocked gap, exactly where the source put it"
    );

    let (g, _o, _e1, _x1, _gap, _e2, _x2) = build(false);
    let r = analyze_escapes(&g);
    assert_eq!(
        r.lock_coarsening.len(),
        1,
        "MUST coarsen once the gap cannot throw"
    );
}

/// MUST NOT: a deopt point in the gap. The interpreter frame rebuilt there
/// must NOT hold the monitor (the original had already run the first
/// `monitorexit`), while the coarsened compiled frame does. That
/// reconstruction is provably wrong, not merely unproven, so the merge is
/// refused outright rather than repaired.
///
/// Paired MUST: safepoints *inside* either region are fine — coarsening
/// changes the held-monitor set only in the gap, so those frames
/// reconstruct exactly the monitor set they always did.
#[test]
fn a_deopt_point_in_the_gap_refuses_the_merge_but_one_inside_a_region_does_not() {
    // Safepoint in the GAP.
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x1 = g.add_node(Op::MonitorExit, vec![o]);
    let _sp = g.add_node(Op::Safepoint, vec![]);
    let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x2 = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert!(
        r.lock_coarsening.is_empty(),
        "MUST NOT coarsen across a deopt point whose monitor set would be \
         wrong after the merge"
    );
    assert!(r.lock_refusals.contains(&(o, LockRefusal::SafepointInGap)));

    // Safepoints INSIDE both regions, gap clean.
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _sp1 = g.add_node(Op::Safepoint, vec![]);
    let _x1 = g.add_node(Op::MonitorExit, vec![o]);
    let _c = g.add_node(Op::Const(0), vec![]);
    let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let _sp2 = g.add_node(Op::Safepoint, vec![]);
    let x2 = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert_eq!(
        r.lock_coarsening.len(),
        1,
        "MUST coarsen: a deopt inside a region holds the same monitor set \
         before and after the merge"
    );
    assert_eq!(r.lock_coarsening[0].surviving_enter(), e1);
    assert_eq!(r.lock_coarsening[0].surviving_exit(), x2);
}

/// MUST NOT: a call in the gap. It is a safepoint, it can throw, and it is
/// how `Thread.holdsLock` and every blocking operation reach the gap — the
/// three ways the unlocked state is observable.
#[test]
fn a_call_in_the_gap_refuses_the_merge() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x1 = g.add_node(Op::MonitorExit, vec![o]);
    let _call = g.add_node(Op::Call, vec![]);
    let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x2 = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert!(r.lock_coarsening.is_empty());
    assert!(r.lock_refusals.contains(&(o, LockRefusal::MayThrowInGap)));
}

/// A `monitorexit`/`monitorenter` on a DIFFERENT object in the gap is an
/// observation of the lock state, and it is also the shape where a naive
/// merge could invert two lock orders. Refused.
#[test]
fn a_foreign_monitor_in_the_gap_refuses_the_merge() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let p = alloc(&mut g, 2, 0);
    let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x1 = g.add_node(Op::MonitorExit, vec![o]);
    let _pe = g.add_node(Op::MonitorEnter, vec![p]);
    let _px = g.add_node(Op::MonitorExit, vec![p]);
    let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x2 = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert!(r.lock_coarsening.iter().all(|plan| plan.object != o));
    assert!(r.lock_refusals.contains(&(o, LockRefusal::ObservableGap)));
}

/// Without a dominance proof "adjacent" and "between" are not answerable,
/// so coarsening is refused wholesale — while elision, which removes every
/// monitor and therefore needs no ordering, still fires.
#[test]
fn a_branch_refuses_coarsening_but_not_elision() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let cond = g.add_node(Op::Const(0), vec![]);
    let _iff = g.add_node(Op::If, vec![cond]);
    let e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let x1 = g.add_node(Op::MonitorExit, vec![o]);
    let e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let x2 = g.add_node(Op::MonitorExit, vec![o]);

    assert!(!program_order_proves_dominance(&g));
    let r = analyze_escapes(&g);
    assert!(r.lock_coarsening.is_empty());
    assert!(r
        .lock_refusals
        .contains(&(o, LockRefusal::NoDominanceProof)));
    assert_eq!(
        r.elide_locks,
        vec![e1, x1, e2, x2],
        "removing EVERY monitor is balance-preserving on every path, so \
         elision needs no ordering proof"
    );
}

/// Coarsening merges only *outermost* regions. An inner recursive region is
/// not adjacent to anything: merging across it would change the reentry
/// depth the interpreter has to rebuild.
#[test]
fn coarsening_pairs_only_outermost_regions() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _e2 = g.add_node(Op::MonitorEnter, vec![o]); // depth 1
    let _x1 = g.add_node(Op::MonitorExit, vec![o]);
    let xo = g.add_node(Op::MonitorExit, vec![o]);
    let _c = g.add_node(Op::Const(0), vec![]);
    let e3 = g.add_node(Op::MonitorEnter, vec![o]);
    let x3 = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert_eq!(r.lock_coarsening.len(), 1);
    let plan = r.lock_coarsening[0];
    assert_eq!(plan.surviving_enter(), e1);
    assert_eq!(plan.removed_exit(), xo, "the OUTER exit, not the inner one");
    assert_eq!(plan.removed_enter(), e3);
    assert_eq!(plan.surviving_exit(), x3);
}

/// Three adjacent regions produce two chained plans that SHARE a region.
/// Each plan is individually balance-preserving, and the re-verification in
/// [`apply_lock_coarsening`] makes overlapping plans mutually exclusive
/// rather than compounding: whichever runs first wins, the other is dropped.
/// The graph is balanced after every subset, in every order.
///
/// Merging all three is reached the same way a synchronized object reaches
/// scalar replacement — by re-running the analysis on the mutated graph.
#[test]
fn chained_coarsening_plans_are_safe_in_any_subset() {
    let build = || {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        for _ in 0..3 {
            g.add_node(Op::MonitorEnter, vec![o]);
            g.add_node(Op::MonitorExit, vec![o]);
            g.add_node(Op::Const(0), vec![]);            }
        (g, o)
    };

    let (g, _o) = build();
    let plans = analyze_escapes(&g).lock_coarsening;
    assert_eq!(plans.len(), 2, "three regions ⇒ two adjacent pairs");

    // Either plan alone: three regions become two.
    for which in 0..2 {
        let (mut g, o) = build();
        assert!(apply_lock_coarsening(&mut g, &plans[which]));
        assert_eq!(
            lock_regions(&g, o).expect("balanced").len(),
            2,
            "applying one of two plans leaves two balanced regions"
        );
    }

    // Both, in either order: the second overlaps the first and is dropped.
    for order in [[0usize, 1usize], [1, 0]] {
        let (mut g, o) = build();
        assert!(apply_lock_coarsening(&mut g, &plans[order[0]]));
        assert!(
            !apply_lock_coarsening(&mut g, &plans[order[1]]),
            "the two plans share a region; the second no longer describes \
             the graph and is refused rather than half-applied"
        );
        let regions = lock_regions(&g, o).expect("balanced");
        assert_eq!(regions.len(), 2);

        // Re-analysing the mutated graph offers the now-adjacent pair.
        let again = analyze_escapes(&g).lock_coarsening;
        assert_eq!(again.len(), 1);
        assert!(apply_lock_coarsening(&mut g, &again[0]));
        assert_eq!(
            lock_regions(&g, o).expect("balanced").len(),
            1,
            "one region, entered once and exited once"
        );
    }
}

/// Coarsening depends on NO elision having landed, and re-verifies before
/// mutating. A plan whose pair an elision already removed is a no-op; a
/// plan whose *outer* monitor a partial elision removed is refused, so
/// coarsening never compounds an imbalance it did not create.
#[test]
fn coarsening_is_safe_under_partial_application_of_elision() {
    let build = || {
        let mut g = Graph::new();
        let o = alloc(&mut g, 1, 0);
        let e1 = g.add_node(Op::MonitorEnter, vec![o]);
        let x1 = g.add_node(Op::MonitorExit, vec![o]);
        let _c = g.add_node(Op::Const(0), vec![]);
        let e2 = g.add_node(Op::MonitorEnter, vec![o]);
        let x2 = g.add_node(Op::MonitorExit, vec![o]);
        (g, o, e1, x1, e2, x2)
    };

    let (g, _o, _e1, _x1, _e2, _x2) = build();
    let plan = analyze_escapes(&g).lock_coarsening[0];

    // The elision landed in full: the pair is already gone.
    let (mut g, o, _e1, _x1, _e2, _x2) = build();
    let r = analyze_escapes(&g);
    assert!(apply_lock_elision_plan(&mut g, &r.lock_elisions[0]));
    assert!(
        !apply_lock_coarsening(&mut g, &plan),
        "no monitors left: the plan is dropped, not applied twice"
    );
    assert!(live_monitors_of(&g, o).is_empty());

    // A partial elision took the OUTER enter only — the hazard case.
    let (mut g, _o, e1, x1, e2, x2) = build();
    g.nodes[e1].op = Op::Dead;
    g.nodes[e1].inputs.clear();
    assert!(
        !apply_lock_coarsening(&mut g, &plan),
        "MUST NOT merge onto an enter that is no longer there"
    );
    assert_eq!(g.nodes[x1].op, Op::MonitorExit);
    assert_eq!(g.nodes[e2].op, Op::MonitorEnter);
    assert_eq!(g.nodes[x2].op, Op::MonitorExit);
}

/// Field accesses on confined allocations of this graph ARE admitted in a
/// gap: they cannot fault (the holder is a `New` of this frame, so never
/// null) and no other thread can observe them.
#[test]
fn a_gap_of_confined_field_accesses_is_transparent() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 1);
    let other = alloc(&mut g, 2, 1);
    let c = g.add_node(Op::Const(7), vec![]);
    let e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x1 = g.add_node(Op::MonitorExit, vec![o]);
    let _st = g.add_node(Op::Store(0), vec![other, c]);
    let _ld = g.add_node(Op::Load(0), vec![other]);
    let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let x2 = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    let plan = r
        .lock_coarsening
        .iter()
        .find(|p| p.object == o)
        .expect("a confined-only gap is transparent");
    assert_eq!(plan.surviving_enter(), e1);
    assert_eq!(plan.surviving_exit(), x2);
}

/// …but a field access on a **caller-supplied** holder is not: it can raise
/// `NullPointerException`, which is a monitor-unwinding event, and other
/// threads can see the write.
#[test]
fn a_gap_touching_a_foreign_holder_is_not_transparent() {
    let mut g = Graph::new();
    let this = g.add_node(Op::Param(0), vec![]);
    let o = alloc(&mut g, 1, 0);
    let c = g.add_node(Op::Const(7), vec![]);
    let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x1 = g.add_node(Op::MonitorExit, vec![o]);
    let _st = g.add_node(Op::Store(0), vec![this, c]);
    let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x2 = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert!(r.lock_coarsening.is_empty());
    assert!(r.lock_refusals.contains(&(o, LockRefusal::ObservableGap)));
}

/// Coarsening does not disturb the identity rule: a live monitor is still
/// an identity observation, so the object is not scalar-replaced until the
/// monitors are actually gone. The documented two-phase route still works
/// through the coarsened graph.
#[test]
fn coarsening_then_elision_still_unlocks_scalar_replacement() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 1);
    let c = g.add_node(Op::Const(9), vec![]);
    let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _st = g.add_node(Op::Store(0), vec![o, c]);
    let _x1 = g.add_node(Op::MonitorExit, vec![o]);
    let _k = g.add_node(Op::Const(0), vec![]);
    let _e2 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x2 = g.add_node(Op::MonitorExit, vec![o]);
    let load = g.add_node(Op::Load(0), vec![o]);

    let first = analyze_escapes(&g);
    assert!(
        first.scalar_replaceable.iter().all(|s| s.alloc_node != o),
        "MUST NOT replace while a monitor observes the identity"
    );
    assert_eq!(first.stats.identity_blocked, 1);

    let plan = first.lock_coarsening[0];
    assert!(apply_lock_coarsening(&mut g, &plan));
    let second = analyze_escapes(&g);
    assert!(
        second.scalar_replaceable.iter().all(|s| s.alloc_node != o),
        "two monitors survive the merge — still observed"
    );

    assert!(apply_lock_elision_plan(&mut g, &second.lock_elisions[0]));
    let third = analyze_escapes(&g);
    let sr = third
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == o)
        .expect("MUST replace once every monitor is dead");
    assert_eq!(sr.load_value(load), LoadResolution::Value(c));
}

/// The refusal counters are the measurement that says what failing closed
/// costs, exactly as `identity_blocked` does for scalar replacement.
#[test]
fn lock_refusals_are_counted_and_deduplicated() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    g.nodes[1].inputs.push(o); // Return: escapes
    g.nodes[o].uses.push(1);
    let _e1 = g.add_node(Op::MonitorEnter, vec![o]);
    let _x1 = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert_eq!(
        r.lock_refusals,
        vec![(o, LockRefusal::ObjectEscapes)],
        "the elision and coarsening finders both refuse it; the record is \
         deduplicated"
    );
    assert_eq!(r.stats.locks_refused, 1);
    assert_eq!(r.stats.locks_elided, 0);
    assert_eq!(r.stats.locks_coarsened, 0);
}

// ── Alias-analysis audit (wave: alias soundness) ──────────────────

/// SELF-REFERENCE. `o.f0 = o;` makes the allocation recoverable by loading
/// its own field, and the use walk in `find_scalar_replacements` never
/// visits uses made through that recovered reference — it only walks the
/// allocation's own uses and the transparent φ copies it accepts.
///
/// Before the self-reference gate this object was reported replaceable:
/// `apply_scalar_replacement` would delete the allocation and leave
/// `p.f1 = 7` writing through an `Op::Dead` holder.
#[test]
fn an_object_stored_into_its_own_field_is_not_scalar_replaced() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 2);
    let c7 = g.add_node(Op::Const(7), vec![]);
    // o.f0 = o
    let _self_store = g.add_node(Op::Store(0), vec![o, o]);
    // p = o.f0   — `p` is `o`, but it is a Load node, not a use of `o`
    let p = g.add_node(Op::Load(0), vec![o]);
    // p.f1 = 7   — a real store into the object, invisible to the walk
    let _via_p = g.add_node(Op::Store(1), vec![p, c7]);

    let r = analyze_escapes(&g);
    assert_eq!(
        r.escape_states.get(&o),
        Some(&EscapeState::NoEscape),
        "the object genuinely does not escape — this is an ALIAS question, \
         not a reachability one, which is why the escape lattice alone \
         cannot refuse it"
    );
    assert!(
        r.scalar_replaceable.iter().all(|s| s.alloc_node != o),
        "a reference to the object is recoverable from its own field, so \
         there are uses of it this analysis never saw"
    );
}

/// The paired NEGATIVE control: an allocation stored into a *different*
/// object's field is still refused (it is published), and an allocation
/// whose fields hold only non-references is still replaced. The gate must
/// not have swallowed the ordinary case.
#[test]
fn the_self_reference_gate_does_not_refuse_an_ordinary_object() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 1);
    let c9 = g.add_node(Op::Const(9), vec![]);
    let _st = g.add_node(Op::Store(0), vec![o, c9]);
    let load = g.add_node(Op::Load(0), vec![o]);

    let r = analyze_escapes(&g);
    let sr = r
        .scalar_replaceable
        .iter()
        .find(|s| s.alloc_node == o)
        .expect("a field holding a constant is not a self-reference");
    assert_eq!(sr.load_value(load), LoadResolution::Value(c9));
}

/// UNMODELLED USE. `Op::Other` is the bridge's catch-all, and two of the
/// `ir::Op`s that land in it — `LambdaIntToDouble` and `Guard` — publish or
/// republish the reference they are handed.
///
/// `find_scalar_replacements` refuses such an object at its use-walk
/// catch-all, but `find_lock_elision_plans` walks no uses: its E2 test is
/// `get_escape(object).is_confined()`. Before this rule the object looked
/// confined and both monitors were elided.
#[test]
fn an_unmodelled_use_blocks_lock_elision() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 0);
    let _opaque = g.add_node(Op::Other, vec![o]);
    let _enter = g.add_node(Op::MonitorEnter, vec![o]);
    let _exit = g.add_node(Op::MonitorExit, vec![o]);

    let r = analyze_escapes(&g);
    assert!(
        r.escape_states
            .get(&o)
            .copied()
            .unwrap_or(EscapeState::NoEscape)
            .may_escape(),
        "a reference handed to a node this module cannot model must not be \
         reported confined"
    );
    assert!(
        r.elide_locks.is_empty(),
        "eliding every monitor on an object an unmodelled node may have \
         published drops a happens-before edge the JMM argument assumes \
         cannot exist"
    );
    assert!(r.scalar_replaceable.iter().all(|s| s.alloc_node != o));
}

/// The `Op::Other` rule must key off the *reference* operands only: an
/// unmodelled node with no reference input (the static-area base shape) is
/// unchanged, and a node an earlier pass already killed observes nothing.
#[test]
fn the_unmodelled_use_rule_ignores_dead_and_referenceless_nodes() {
    let mut g = Graph::new();
    let o = alloc(&mut g, 1, 1);
    let c = g.add_node(Op::Const(3), vec![]);
    let _st = g.add_node(Op::Store(0), vec![o, c]);
    // A referenceless unmodelled node, and a stale dead use-edge.
    let _bare = g.add_node(Op::Other, vec![c]);
    let stale = g.add_node(Op::Other, vec![o]);
    g.nodes[stale].op = Op::Dead;

    let r = analyze_escapes(&g);
    assert_eq!(r.escape_states.get(&o), Some(&EscapeState::NoEscape));
    assert!(r.scalar_replaceable.iter().any(|s| s.alloc_node == o));
}

/// A reference array stored in its allocation block and read in a later
/// block (a varargs `Object[]` read by a spliced callee past a branch) is
/// answered by its block's last store, with no opt-in.
#[test]
fn test_reference_array_read_in_another_block_is_answered_by_its_block_store() {
    let mut g = Graph::new();
    let block = g.add_node(Op::Merge, vec![]);
    let other = g.add_node(Op::Merge, vec![]);
    let arr = g.add_node(Op::NewArray { element_type: 0, length: Some(1) }, vec![]);
    let v = g.add_node(Op::Param(0), vec![]);
    let st = g.add_node(Op::Store(0), vec![arr, v]);
    let ld = g.add_node(Op::Load(0), vec![arr]);
    g.add_node(Op::Add, vec![ld, ld]);
    g.blocks.insert(arr, block);
    g.blocks.insert(st, block);
    g.blocks.insert(ld, other);
    let result = analyze_escapes(&g);
    let info = result
        .scalar_replaceable
        .iter()
        .find(|i| i.alloc_node == arr)
        .expect("the store's block holds every store and dominates the load");
    assert_eq!(info.load_value(ld), LoadResolution::Value(v));
}
