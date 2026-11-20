fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let rest = &src[at..];
    &rest[..rest.find("\n\u{7d}\n").expect("end")]
}

fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle).unwrap_or_else(|| panic!("{needle}"))
}

/// The laps wrap the pre-collection pass, the post-collection round and
/// the JNI weak sweep; the note follows them and precedes the release;
/// the hidden rows are forgotten right after the collection.
#[test]
fn zt22zc3_the_door_laps_the_round_and_forgets_the_hidden_rows() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let door = body(&src, "fn run_collection_pause(");
    let pre0 = pos(door, "let refs_pre_t0 = std::time::Instant::now();");
    let pre = pos(door, "weakref_null_referents_pre_gc(shared);");
    let pre1 = pos(door, "let refs_pre_us = refs_pre_t0.elapsed()");
    let collect = pos(door, "shared.mem.heap.collect_garbage_with_finalizers(");
    let forget = pos(door, "shared.mem.heap.zgc_clear_stw_reference_skip();");
    let post0 = pos(door, "let refs_post_t0 = std::time::Instant::now();");
    let round = pos(door, "    process_references_after_gc(\n");
    let post1 = pos(door, "let refs_post_us = refs_post_t0.elapsed()");
    let jni = pos(door, "sweep_weak_global_refs_with(");
    let note = pos(
        door,
        ".zgc_note_vm_reference_round(refs_pre_us, refs_post_us, refs_jni_us);",
    );
    let release = pos(door, ".complete_gc(result.pointer_map)");
    assert!(pre0 < pre && pre < pre1 && pre1 < collect && collect < forget);
    assert!(forget < post0 && post0 < round && round < post1 && post1 < jni);
    assert!(jni < note && note < release);
    // zt t31/zc11 (2c): a round run after the release is noted when it
    // ends, after the release.
    let late = pos(
        door,
        "shared.mem.heap.zgc_note_vm_reference_round_after_release(\n",
    );
    assert!(release < late);
    assert_eq!(door.matches("zgc_note_vm_reference_round(").count(), 1);
}

/// The pre-collection pass decides the arm before the processor lock,
/// pushes a row where it would null it (after the referent's class stamp,
/// so the round's restore bookkeeping is unchanged), and hands the set in
/// after the loop -- the empty set on the early return too.
#[test]
fn zt22zc3_the_pre_pass_hides_what_it_would_null() {
    let interp = concat!(
        include_str!("../../interpreter/mod.rs"),
        "\n",
        include_str!("../../interpreter/cached_invoke_target.rs"),
        "\n",
        include_str!("../../interpreter/execute.rs"),
        "\n",
        include_str!("../../interpreter/resume_continuation.rs"),
        "\n",
        include_str!("../../interpreter/execute_frame_from_index_loop.rs"),
        "\n",
        include_str!("../../interpreter/resolve_array_class_loader_aware.rs"),
        "\n",
        include_str!("../../interpreter/park_for_debugger_under.rs"),
        "\n",
        include_str!("../../interpreter/double_arith_apply.rs")
    )
    .replace("\r\n", "\n");
    let pre = body(&interp, "\npub fn weakref_null_referents_pre_gc(");
    let arm = pos(
        pre,
        "let keep_referents = zgc_young_cycle_keeps_references(shared);",
    );
    let mode = pos(
        pre,
        "let hide_referents = !keep_referents && shared.mem.heap.zgc_concurrent_refs_skip_mode();",
    );
    let lock = pos(pre, "let mut rp = shared.mem.ref_processor.lock();");
    assert!(arm < mode && mode < lock);
    let stamp = pos(pre, "referent_classes.push((");
    let push = pos(pre, "hidden.push(ref_obj_addr);");
    let null = pos(
        pre,
        ".set_field_suppress_satb(ref_obj, 0, Value::Object(None));",
    );
    let hand = pos(pre, "shared.mem.heap.zgc_set_stw_reference_skip(hidden);");
    assert!(stamp < push && push < null && null < hand);
    assert_eq!(pre.matches("zgc_set_stw_reference_skip(").count(), 2);
}

/// zt t22/zc3b (`zc1_S1_J`: a `System.gc()`-owned concurrent cycle left
/// every weak referent made since the last collection set): the mark
/// start binds the allocation-registered `Reference`s inside its pause,
/// before the roots, the frozen-peer referent screen and the skip
/// candidates read the rows; gated by its default-ON switch.
#[test]
fn zt22zc3b_the_mark_start_binds_references_before_the_skip_set() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let start = body(&src, "fn zgc_concurrent_mark_cycle(");
    let stw = pos(
        start,
        "let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };",
    );
    let in_pause = &start[stw..];
    let gate = pos(
        in_pause,
        "if zgc_mark_start_binds_references_enabled() \u{7b}",
    );
    let bind = pos(
        in_pause,
        "crate::runtime::interpreter::bind_allocated_references_pre_gc(shared);",
    );
    let roots = pos(in_pause, "collect_roots(shared, thread)");
    let rows = pos(in_pause, "reference_object_addresses()");
    let skip = pos(in_pause, "zgc_conc_reference_skip_candidates(shared)");
    assert!(gate < bind);
    assert!(bind < roots && bind < rows && bind < skip);
    let reader = body(&src, "fn zgc_mark_start_binds_references_enabled(");
    assert_eq!(reader.split_once('{').unwrap().1.trim(), "true");
}
