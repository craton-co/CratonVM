fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find("\n\u{7d}\n").expect("end")]
}

fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle).unwrap_or_else(|| panic!("{needle}"))
}

/// The roots are added AFTER the pre-collection pass (which arms the
/// young cycle the helper reads), after the frozen-JIT roots (the tail of
/// the vector, read back by no index) and BEFORE the collection; the
/// helper answers nothing unless the heap is armed.
#[test]
fn zt11gn3_the_jni_weak_roots_follow_the_arming_and_precede_the_collection() {
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
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn run_collection_pause(",
    );
    let frozen = pos(door, "roots.extend(xt_roots);");
    let arm = pos(door, "weakref_null_referents_pre_gc(shared);");
    let add = pos(
        door,
        "roots.extend(zgc_young_cycle_jni_weak_roots(shared));",
    );
    let collect = pos(door, "shared.mem.heap.collect_garbage_with_finalizers(");
    let sweep = pos(door, "crate::native::jni::sweep_weak_global_refs_with(");
    assert!(frozen < arm && arm < add && add < collect && collect < sweep);
    let helper = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_young_cycle_jni_weak_roots(",
    );
    let armed = pos(helper, "zgc_young_refs_strong_armed()");
    let read = pos(helper, "weak_global_referent_addrs(shared)");
    assert!(
        armed < read,
        "the heap must be armed before reading weak roots"
    );
}
