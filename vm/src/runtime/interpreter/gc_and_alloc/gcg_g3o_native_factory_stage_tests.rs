use super::{native_factory_attempt_ladder_enabled, native_young_stage_applies};

/// gcg g3/o: the staged native factory door applies only with its switch,
/// with the overhead-progress ladder on, and on an unlatched streak (the
/// latched arm decides without an attempt in between).
#[test]
fn gcg_g3o_the_staged_factory_door_applies_only_unlatched_and_opted_in() {
    assert!(native_young_stage_applies(true, true, false));
    assert!(!native_young_stage_applies(false, true, false));
    assert!(!native_young_stage_applies(true, false, false));
    assert!(!native_young_stage_applies(true, true, true));
}


/// gcg g3/o (d10o step 1): the unified JIT verdict equals the JIT's own
/// (`jit_overhead_limit_verdict`, copied here as `old`) in every cell but
/// the two the d10o page names, and there it attempts instead of throwing.
#[test]
fn gcg_g3o_the_unified_jit_verdict_differs_only_in_the_two_named_cells() {
    use super::jit_overhead_limit_throws_unified as unified;
    let old = |judge: bool, soft: bool, major: bool, latched: bool| -> bool {
        if !judge {
            return !soft;
        }
        if soft {
            return latched;
        }
        major && latched
    };
    let mut differing = Vec::new();
    for bits in 0u8..16 {
        let (j, s, m, l) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
        if old(j, s, m, l) != unified(j, s, m, l) {
            assert!(
                old(j, s, m, l) && !unified(j, s, m, l),
                "only throw -> attempt"
            );
            differing.push((j, s, m, l));
        }
    }
    // Cell 1: unjudged, the soft rung collected nothing (any m, l).
    // Cell 2: judged, soft collected, the major lost its race, latched.
    let cell1 = |&(j, s, _, _): &(bool, bool, bool, bool)| !j && !s;
    let cell2 = |&(j, s, m, l): &(bool, bool, bool, bool)| j && s && !m && l;
    assert!(
        differing.iter().all(|c| cell1(c) || cell2(c)),
        "{differing:?}"
    );
    assert_eq!(differing.iter().filter(|c| cell1(*c)).count(), 4);
    assert_eq!(differing.iter().filter(|c| cell2(*c)).count(), 1);
}


/// The classic door and the staged door share one tail, so the rungs after
/// the young collection cannot drift between them.
#[test]
fn gcg_g3o_both_factory_doors_share_the_ladder_tail() {
    let src = concat!(include_str!("../gc_and_alloc/mod.rs"), "\n", include_str!("../gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../gc_and_alloc/reference_round.rs"), "\n", include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../gc_and_alloc/g1_force_full_cycle.rs"));
    let body = |name: &str| -> String {
        src.split(name)
            .nth(1)
            // `\u{7d}` is a closing brace, spelled so the brace-counting
            // production-panic scanner sees this line balanced.
            .and_then(|rest| rest.split("\n\u{7d}\n").next())
            .unwrap_or_default()
            .to_string()
    };
    // gci i13/oa: the classic door's body is its sized twin.
    let classic = body("pub(crate) fn native_reclaim_before_alloc_retry_graced_for(");
    let staged = body("pub(crate) fn native_reclaim_after_young_stage(");
    assert!(classic.contains("native_reclaim_ladder_tail(shared, thread, before_live)"));
    assert!(staged.contains("native_reclaim_ladder_tail(shared, thread, before_live)"));
    assert!(
        !staged.contains("maybe_gc_forced_at("),
        "no second young collection"
    );
}
