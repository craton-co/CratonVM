fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find("\n\u{7d}\n").expect("end")]
}

fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle).unwrap_or_else(|| panic!("{needle}"))
}

/// The mark start reads the finalizer queue inside its pause (after the
/// world is stopped), gated by its switch, and the queue's words reach
/// the cycle's roots.
#[test]
fn zt23hunt5_the_mark_start_roots_the_finalizer_queue() {
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
    let start = body(
        &src,
        "\npub(in crate::runtime::interpreter) fn zgc_concurrent_mark_cycle(",
    );
    let stw = pos(
        start,
        "let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };",
    );
    let read = pos(start, "shared.mem.finalizer_thread.pending_addresses()");
    let chain = pos(start, ".chain(pending_finalizers.into_iter())");
    let open = pos(start, "zgc_start_concurrent_mark(&stw, &all_roots)");
    assert!(stw < read && read < chain && chain < open);
}

/// The last-ditch soft clear asks for a fresh mark only with a cycle open
/// at entry and the switch on, in both of its collecting loops, and
/// withdraws the request after each.
#[test]
fn zt23hunt5_the_last_ditch_clear_marks_from_scratch_under_an_open_cycle() {
    let src = concat!(include_str!("../gc_and_alloc/mod.rs"), "\n", include_str!("../gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../gc_and_alloc/reference_round.rs"), "\n", include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../gc_and_alloc/g1_force_full_cycle.rs")).replace("\r\n", "\n");
    let f = body(&src, "\npub(crate) fn last_ditch_clear_soft_refs(");
    let open = pos(
        f,
        "let zgc_conc_open = shared.mem.heap.zgc_concurrent_mark_active();",
    );
    let fresh = pos(
        f,
        "let zgc_fresh = zgc_conc_open;",
    );
    let ask = pos(f, "if generational || zgc_fresh \u{7b}");
    let first = pos(f, "with_last_ditch_soft_clear(");
    let withdraw = pos(f, "if (!collected && generational) || zgc_fresh \u{7b}");
    assert!(open < fresh && fresh < ask && ask < first && first < withdraw);
    assert_eq!(
        f.matches("cratonvm_gc::gc_quiescence::request_major_gc();")
            .count(),
        2
    );
    assert_eq!(
        f.matches("cratonvm_gc::gc_quiescence::take_major_gc_request();")
            .count(),
        2
    );
}
