use super::split_registered_from_queued;

#[test]
fn gcj3x2_r1_a_queued_finalizable_is_never_a_candidate() {
    let queued = vec![0x200, 0x300];
    assert_eq!(
        split_registered_from_queued(vec![0x400, 0x100, 0x300, 0x100], &queued),
        vec![0x100, 0x400],
        "registered only, sorted, deduplicated, queued ones dropped"
    );
    assert_eq!(
        split_registered_from_queued(Vec::new(), &queued),
        Vec::<usize>::new()
    );
}

/// Source pin: the door pushes the queued roots into `roots` before the
/// root layout is published and hands only the candidates to the probe.
#[test]
fn gcj3x2_r1_the_door_roots_the_queue_before_the_layout() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let at = src
        .find("fn run_collection_pause(")
        .expect("run_collection_pause");
    let body = &src[at..];
    let body = &body[..body.find("\n\u{7d}\n").expect("end")];
    let pos = |needle: &str| body.find(needle).expect(needle);
    let split = pos("let (fin_roots, queued_fin_roots) = pause_finalizable_inputs(shared);");
    let push = pos("roots.extend(queued_fin_roots);");
    let layout = pos("publish_old_pin_root_layout_for_pause(");
    let collect = pos(".collect_garbage_with_finalizers(");
    assert!(split < push && push < layout && layout < collect);
}
