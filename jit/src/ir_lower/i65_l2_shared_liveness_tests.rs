use super::*;
use crate::regalloc::linear_scan_tests::{i65_l2_graphs, swap_loop};
use crate::regalloc::{liveness_core, LivenessRules};


/// Field-for-field equality of two plans (`SlotPlan` has no `PartialEq`).
pub(crate) fn assert_plans_equal(a: &SlotPlan, b: &SlotPlan, what: &str) {
    assert_eq!(a.node_color, b.node_color, "{what}: node_color");
    assert_eq!(a.range, b.range, "{what}: range");
    assert_eq!(a.class, b.class, "{what}: class");
    assert_eq!(a.slots, b.slots, "{what}: slots");
    assert_eq!(a.peak_live, b.peak_live, "{what}: peak_live");
    assert_eq!(
        a.sr_field_values, b.sr_field_values,
        "{what}: sr_field_values"
    );
    assert_eq!(a.pos_of, b.pos_of, "{what}: pos_of");
    assert_eq!(
        a.phi_code_unused, b.phi_code_unused,
        "{what}: phi_code_unused"
    );
    assert_eq!(a.colour_range, b.colour_range, "{what}: colour_range");
    assert_eq!(a.chain_ranged, b.chain_ranged, "{what}: chain_ranged");
}

/// The legacy liveness half of `plan_slots` and `regalloc::liveness_core`
/// (no φ rule) give the same answer, every field, both token rules.
#[test]
fn i65_l2_the_core_slot_liveness_is_the_legacy_slot_liveness() {
    for (i, (graph, schedule)) in i65_l2_graphs().iter().enumerate() {
        let wants = slot_plan_wants(graph, schedule);
        for token in [false, true] {
            let legacy = plan_slots_liveness_legacy(graph, schedule, &wants, token);
            let core = liveness_core(
                graph,
                schedule,
                &wants,
                LivenessRules {
                    token_edges_not_uses: token,
                    phi_edge_interfere: false,
                    collect_reads: false,
                },
                SLOT_PLAN_WORK_BUDGET,
                SLOT_PLAN_MAX_ITERATIONS,
            );
            assert!(legacy.converged, "graph {i}: a small graph converges");
            assert_eq!(legacy, core, "graph {i} token {token}");
        }
    }
}

/// So the whole plan is unchanged by the switch, and still verifies.
#[test]
fn i65_l2_the_core_slot_plan_is_the_legacy_slot_plan() {
    for (i, (graph, schedule)) in i65_l2_graphs().iter().enumerate() {
        for token in [false, true] {
            let legacy = plan_slots_with(graph, schedule, None, token, SlotLiveness::Legacy);
            let core = plan_slots_with(
                graph,
                schedule,
                None,
                token,
                SlotLiveness::Core {
                    phi_edge_interfere: false,
                },
            );
            assert_plans_equal(&legacy, &core, &format!("graph {i} token {token}"));
            verify_slot_colouring(graph, schedule, &core).expect("a sound colouring");
        }
    }
}


