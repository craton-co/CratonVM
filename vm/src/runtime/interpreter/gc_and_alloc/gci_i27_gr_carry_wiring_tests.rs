use super::*;


fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find("\n\u{7d}\n").expect("end")]
}

#[test]
fn gci_i27_gr_the_round_carries_after_processing_and_the_remark_reads_before_the_prune() {
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
    let post = crate::runtime::interpreter::gc_and_alloc::process_references_text(&src);
    let round = post
        .find("ref_proc.process_references(&is_marked, free_mb, 0)")
        .expect("the round");
    let carry = post
        .find("ref_proc.carry_probe_undiscovered(pointer_map, shared.mem.heap.collection_count());")
        .expect("the carry");
    let update = post.find("ref_proc.update_after_gc(").expect("the remap");
    assert!(round < carry && carry < update);
    let rows = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn remark_reference_rows(",
    );
    let read = rows
        .find("ref_proc.carried_undiscovered_for(shared.mem.heap.collection_count())")
        .expect("the read");
    let prune = rows
        .find("ref_proc.remove_collected_reference_objects(&reference_is_live)")
        .expect("the pre-prune");
    let note = rows
        .find("ref_proc.note_undiscovered_references(&refs);")
        .expect("the note");
    assert!(read < prune && prune < note);
}
