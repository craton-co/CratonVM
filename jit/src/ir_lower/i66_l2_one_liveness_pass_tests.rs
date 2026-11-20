use super::i65_l2_shared_liveness_tests::assert_plans_equal;
use super::*;
use crate::regalloc::linear_scan_tests::{i65_l2_assert_live_models_equal, i65_l2_graphs};
use crate::regalloc::{build_live_model_with_token_edges, LiveModel, LivenessRules};




/// When the consumers ask different questions the compile keeps two
/// passes, and the plan is the one `plan_slots_with` gives.
#[test]
fn i66_l2_different_rules_decline_and_keep_the_plan() {
    let model = |token, phi| LivenessRules {
        token_edges_not_uses: token,
        phi_edge_interfere: phi,
        collect_reads: true,
    };
    // (slot token, slot liveness, model rules): the φ rule only on the
    // model's side (today's default), and the allocator-only token rule.
    let cases = [
        (false, SlotLiveness::Legacy, model(false, true)),
        (
            false,
            SlotLiveness::Core {
                phi_edge_interfere: false,
            },
            model(false, true),
        ),
        (
            false,
            SlotLiveness::Core {
                phi_edge_interfere: true,
            },
            model(true, true),
        ),
        (
            true,
            SlotLiveness::Core {
                phi_edge_interfere: true,
            },
            model(false, true),
        ),
    ];
    for (i, (graph, schedule)) in i65_l2_graphs().iter().enumerate() {
        for (slot_token, slot_liveness, rules) in cases {
            assert_eq!(
                one_liveness_pass_rules(slot_token, slot_liveness, rules),
                None
            );
            let (plan, pass) = plan_slots_one_pass_with(
                graph,
                schedule,
                None,
                slot_token,
                slot_liveness,
                rules,
            );
            assert_eq!(
                pass.err(),
                Some(OneLivenessPassDecline::RulesDiffer),
                "graph {i}"
            );
            let direct = plan_slots_with(graph, schedule, None, slot_token, slot_liveness);
            assert_plans_equal(&plan, &direct, &format!("graph {i} {slot_liveness:?}"));
        }
    }
}

/// The shared rules are the model's, with the read list on.
#[test]
fn i66_l2_the_shared_rules_keep_the_reads() {
    let rules = one_liveness_pass_rules(
        true,
        SlotLiveness::Core {
            phi_edge_interfere: true,
        },
        LivenessRules {
            token_edges_not_uses: true,
            phi_edge_interfere: true,
            collect_reads: false,
        },
    )
    .expect("same rules");
    assert!(rules.collect_reads && rules.token_edges_not_uses && rules.phi_edge_interfere);
}


