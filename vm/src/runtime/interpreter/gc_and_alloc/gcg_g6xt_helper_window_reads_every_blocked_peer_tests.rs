/// No reader of the deleted switch and no depth filter between the
/// blocked roster and the helper-window pass: every blocked peer's band
/// is read, which is what feeds the blocked-wake stack-slot repair.
#[test]
fn gcg_g6xt_every_blocked_peer_reaches_the_helper_window_pass() {
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
    );
    let prod = src;
    let key = concat!("CRATON", "VM_XT_HELPER_WINDOW_JIT_ONLY");
    assert!(
        !prod.contains(key),
        "a reader of the deleted switch is back"
    );
    assert!(!prod.contains(concat!("fn helper_window_peers_with_", "compiled_frames")));
    let start = prod
        .find("fn stw_take_over_and_wait(")
        .expect("the take-over");
    let b = &prod[start..];
    let roster = b.find("thread_registry.blocked_os_tids()").expect("roster");
    let pass = b.find("xt::helper_window_pass(").expect("pass");
    assert!(roster < pass);
    assert!(
        !b[roster..pass].contains("jit_depth_of_tid"),
        "no depth filter between the roster and the pass"
    );
}
