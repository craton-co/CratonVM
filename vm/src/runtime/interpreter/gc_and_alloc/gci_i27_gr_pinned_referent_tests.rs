use super::*;


#[test]
fn gci_i27_gr_candidates_skip_mapped_null_and_finalizable_addresses() {
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x2000, 0x9000); // moved
    map.insert(0x3000, 0x3000); // identity survivor
    let pairs = [(0x1000, 0x2000), (0x3000, 0x4000), (0x5000, 0), (0x1000, 0x6000)];
    let finalizable = [0x6000];
    assert_eq!(
        g1_pinned_referent_candidates(&pairs, &finalizable, &map),
        vec![0x1000, 0x4000, 0x5000]
    );
}

/// The verdict is read after the diagnostic short-circuit, and
/// `is_marked` asks it before the ordinary survival check. zt t31/zc11: it is a lock-free read of the
/// pause's relocation set, taken at the head of `reference_round`, after
/// `reference_round_prep` took the class manager and the processor lock.
/// gci i28/wt: it is read from the pause's relocation set, which
/// `run_collection_pause` gave it (`gci_i28_wt_the_door_takes_the_verdict_once_before_every_weak_consumer`).
#[test]
fn gci_i27_gr_the_verdict_is_wired_before_the_locks_and_into_is_marked() {
    let src = concat!(
        include_str!("../gc_and_alloc/finalizers_forced.rs"),
        "\n",
        include_str!("../gc_and_alloc/reference_round.rs")
    )
    .replace("\r\n", "\n");
    let body = crate::runtime::interpreter::gc_and_alloc::process_references_text(&src);
    let short = body
        .find("    if no_refproc() \u{7b}")
        .expect("short-circuit");
    let verdict = body
        .find("let g1_pinned_unheld = pause_relocations.g1_unheld_set();")
        .expect("the verdict");
    let classes = body
        .find("let class_manager = shared.classes.class_manager.read();")
        .expect("class manager");
    let lock = body
        .find("let ref_proc = shared.mem.ref_processor.lock();")
        .expect("processor lock");
    let marked = verdict
        + body[verdict..]
            .find("    let is_marked = |addr: usize| -> bool {")
            .expect("is_marked");
    let mark_body = &body[marked..];
    let verdict_check = mark_body
        .find("if g1_pinned_unheld.contains(&addr)")
        .expect("the G1 verdict");
    let survival = mark_body
        .find("watched_pre_gc_addr_survived(")
        .expect("the ordinary survival check");
    assert!(mark_body[..verdict_check].contains("if after_release"));
    assert!(mark_body[..verdict_check].contains("if shared.mem.heap.is_zgc()"));
    assert!(verdict_check < survival);
    assert!(mark_body[verdict_check..survival].contains("return false;"));
    assert!(short < classes && classes < lock && lock < verdict && verdict < marked);
}
