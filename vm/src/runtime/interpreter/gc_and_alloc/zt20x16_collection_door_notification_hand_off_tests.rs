fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find("\n\u{7d}\n").expect("end")]
}

fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle).unwrap_or_else(|| panic!("{needle}"))
}

#[test]
fn zt20x16_the_explicit_doors_hand_off_before_the_inline_drain() {
    let src = concat!(
        include_str!("../gc_and_alloc/mod.rs"),
        "\n",
        include_str!("../gc_and_alloc/mtroots_on.rs"),
        "\n",
        include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"),
        "\n",
        include_str!("../gc_and_alloc/zgc_refusal_line.rs"),
        "\n",
        include_str!("../gc_and_alloc/finalizers_forced.rs"),
        "\n",
        include_str!("../gc_and_alloc/reference_round.rs"),
        "\n",
        include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"),
        "\n",
        include_str!("../gc_and_alloc/frozen_peer_referents.rs"),
        "\n",
        include_str!("../gc_and_alloc/g1_force_full_cycle.rs")
    )
    .replace("\r\n", "\n");
    let door = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn full_gc_door(",
    );
    let rule = pos(
        door,
        "!gc_notifications_go_to_delivery_thread(shared, thread)",
    );
    let hand = pos(
        door,
        "!zgc_collection_door_hands_off_gc_notifications(shared, thread)",
    );
    let inline = pos(door, "run_gc_notifications(shared, thread, true);");
    assert!(rule < hand && hand < inline);
    let jvmti = body(&src, "\npub fn force_gc_for_jvmti(");
    assert!(jvmti.contains("force_full_collection(shared, thread, GcDoor::JvmtiForceGc)"));
    assert!(!jvmti.contains("run_gc_notifications("));
    let helper = body(&src, "\npub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_collection_door_hands_off_gc_notifications(");
    let pending = pos(helper, "gc_notifications_pending(shared)");
    let gate = pos(helper, "zgc_undeliverable_door_hands_off_for(");
    let switch = pos(helper, "zgc_system_gc_notifications_off_thread_enabled()");
    let hand = pos(
        helper,
        "hand_gc_notifications_to_delivery_thread(shared, thread)",
    );
    assert!(pending < gate && gate < switch && switch < hand);
    assert!(super::zgc_system_gc_notifications_off_thread_enabled());
}

/// The ZGC census line: nothing queued, no line; off ZGC, never.
#[test]
fn zt20x16_the_zgc_notification_line_needs_zgc_and_a_queued_record() {
    use crate::config::{GcAlgorithm, VmConfig};
    #[cfg(feature = "zgc")]
    {
        let zgc = super::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Zgc,
            ..VmConfig::default()
        });
        assert_eq!(super::zgc_gc_notifications_line(&zgc), None);
    }
    let other = super::SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    assert_eq!(super::zgc_gc_notifications_line(&other), None);    }