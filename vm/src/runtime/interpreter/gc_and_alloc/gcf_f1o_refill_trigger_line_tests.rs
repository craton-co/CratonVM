//! gcf f1/o: the compiled refill trigger's engagement line. The gce e1/o
//! tests of the unjudged arm went with `CRATONVM_GC_REFILL_TRIGGER_UNJUDGED`
//! (gcf f7/o).
use super::*;

/// The line names what the cycle did to the streak; the `arm=judged` word
/// the probe rows grep for is kept.
#[test]
fn gcf_f1o_refill_trigger_engagement_line_names_arm_and_effect() {
    let raised = refill_trigger_engagement_line(true, 3, 4);
    assert_eq!(
        raised,
        "[GC_OVERHEAD] refill-trigger: arm=judged wedged_at_entry=true \
         streak_before=3 streak_after=4 streak=raised"
    );
    assert!(refill_trigger_engagement_line(true, 5, 0).ends_with("streak=reset"));
    assert!(refill_trigger_engagement_line(false, 0, 0).ends_with("streak=kept"));
    assert!(!refill_trigger_engagement_line(false, 0, 1).contains("unjudged"));
}

/// gcf f7/o: the switch is gone from the source -- no reader is left
/// behind to be set by a stale environment.
#[test]
fn gcf_f7o_the_unjudged_refill_trigger_switch_is_deleted() {
    let src = concat!(include_str!("../gc_and_alloc/mod.rs"), "\n", include_str!("../gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../gc_and_alloc/reference_round.rs"), "\n", include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../gc_and_alloc/g1_force_full_cycle.rs"));
    let key = ["CRATONVM", "_GC_REFILL_TRIGGER_UNJUDGED\")"].concat();
    assert!(
        !src.contains(&key),
        "a reader of the deleted switch is back"
    );
}
