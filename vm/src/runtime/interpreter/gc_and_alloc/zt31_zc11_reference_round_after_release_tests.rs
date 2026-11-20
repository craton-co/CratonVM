//! zt t31/zc11 (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`): the door's order.
//! The heap half (the switch, the hold, the census) is unit-tested in
//! `gc/src/zgc/concurrent_ref_round.rs`.

/// In the pause: the decision after the collection, the head alone when
/// the round moves, the round's locks after the seal and the productivity
/// sample (nothing after them takes a ranked lock) and before the peers
/// resume; after the release: the round, then the parked sweep.
#[test]
fn zt31zc11_the_round_runs_after_the_release_before_the_install() {
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
        .find("\npub(in crate::runtime::interpreter::gc_and_alloc) fn run_collection_pause(")
        .expect("the door");
    let rest = &src[start + 1..];
    let door = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    let at = |needle: &str| {
        door.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` in the door"))
    };
    let decide = at("zgc_reference_round_after_release(shared, &result.pointer_map)");
    let head = at("if process_references_head(");
    let whole = at("        process_references_after_gc(\n");
    let seal = at("gc_event_seal(shared, &mut gc_event);");
    let sample = at("let mut productivity_after =");
    let prep = at("reference_round_prep(shared, &result.pointer_map),");
    let resume = at("retire_skip_spans_and_resume(shared, taken);");
    let release = at("shared.mem.gc_barrier.complete_gc(result.pointer_map);");
    let round = at("reference_round(shared, &no_moves, &targets, prep, true);");
    let unhold = at("hold.release(round_us.max(1));");
    let sweep = at("shared.mem.heap.zgc_run_pending_sweep_after_release()");
    assert!(decide < head && head < whole);
    assert!(seal < sample && sample < prep && prep < resume && resume < release);
    assert!(release < round && round < unhold && unhold < sweep);
    assert!(
        door.find("zgc_hold_sweep_install()").expect("the hold") < prep,
        "the install is held before the locks are taken"
    );
}
