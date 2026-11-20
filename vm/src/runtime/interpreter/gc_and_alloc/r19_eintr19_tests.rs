use super::*;

/// JIT round 19 residual (`r19-an-unwind-out-of-a-pause-leaks-its-lazy-native-claims-FIXED-20261007`):
/// every pause driver arms an unwind guard BEFORE its take-over -- the
/// non-moving ones reopen the world (`NonMovingPause`, `GenConcPauseGuard`),
/// the moving collection aborts -- so no unwind out of a pause leaves the
/// world closed with every mutator parked. Source pin: a panic inside a
/// pause cannot be staged from a test.
#[test]
fn every_take_over_runs_under_an_unwind_guard() {
    let prod = concat!(
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
    let call = "stw_take_over_and_wait(";
    let def = "pub(in crate::runtime::interpreter) fn stw_take_over_and_wait(";
    let mut sites = 0;
    let mut from = 0;
    while let Some(at) = prod[from..].find(call).map(|i| i + from) {
        from = at + call.len();
        // The definition, and this module's own literals.
        if prod[..at + call.len()].ends_with(def) || prod[..at].ends_with('"') {
            continue;
        }
        sites += 1;
        // The guard sits above the call, in the same driver.
        let before = &prod[at.saturating_sub(3000)..at];
        assert!(
            before.contains("GenConcPauseGuard::arm(shared)")
                || before.contains("MovingPauseUnwindAbort::arm()")
                || before.contains("let mut pause = NonMovingPause {"),
            "a take-over without an unwind guard at byte {at}"
        );
    }
    // gci i15/ca: +1, `g1_abort_open_cycle_pause` (under `GenConcPauseGuard`).
    // JIT round 20 wave 21: +1, `g1_stw_full_mark_cycle` (guarded). 9
    // since the 2026-10-15 merge of origin/dev (each side counted 8).
    assert_eq!(sites, 9, "a new take-over site needs an unwind guard");
    let run = &prod[prod
        .find("fn run_collection_pause(")
        .expect("run_collection_pause")..];
    let release = run
        .find("shared.mem.gc_barrier.complete_gc(result.pointer_map);")
        .expect("the release");
    let disarm = run.find("unwind_abort.disarm();").expect("the disarm");
    assert!(release < disarm, "disarmed only once the world is released");
}

/// The three new switches are correctness rules, on by default, each with
/// its `=0` kill switch.

/// The frame-trace scope is active while claimed peers are traced.
#[test]
fn the_frame_trace_scope_claims_compiled_and_interpreted_peers() {
    assert!(!no_roots_lazy_claim_active());
    {
        let _g = NoRootsLazyClaim::enter();
        assert!(no_roots_lazy_claim_active());
    }
    assert!(!no_roots_lazy_claim_active());
}

/// Source witnesses: the frame-trace pause publishes the claimed
/// threads' traces before its `Drop` releases them, and the take-over's
/// excusal filters blocked peers before it counts.
#[test]
fn the_frame_trace_pause_publishes_before_it_releases() {
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
    let start = src
        .find("pub(crate) fn stw_publish_frame_traces_with<R>(")
        .expect("the frame-trace pause");
    let b = &src[start..];
    let publish = b
        .find("publish_lazy_claimed_traces(shared, &pause)")
        .expect("publishes the claimed traces");
    let drop_at = b.find("drop(pause);").expect("drop");
    assert!(publish < drop_at);
    let take = &src[src
        .find("fn stw_take_over_and_wait(")
        .expect("the take-over")..];
    // gcj j2/cm2: both membership tests are binary searches over sorted
    // copies (item 6 of the j1-cm registry residual page).
    let filter = take
        .find("blocked_now.binary_search(&tid).is_ok()")
        .expect("filter");
    let counted = take
        .find("counted_sorted.binary_search(&tid).is_ok()")
        .expect("counted");
    assert!(filter < counted);
    assert!(!take[..counted].contains("counted_os_tids.contains(&tid)"));
}
