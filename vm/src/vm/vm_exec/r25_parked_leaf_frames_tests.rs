fn body<'a>(src: &'a str, head: &str) -> &'a str {
    let start = src.find(head).expect(head);
    let rest = &src[start..];
    &rest[..rest.find("\n\u{7d}\n").expect("its end")]
}

#[test]
fn r25_the_parked_state_is_read_before_the_pause() {
    let src = concat!(
        include_str!("../vm_exec/mod.rs"),
        "\n",
        include_str!("../vm_exec/method.rs"),
        "\n",
        include_str!("../vm_exec/report_pad_access.rs"),
        "\n",
        include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"),
        "\n",
        include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"),
        "\n",
        include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"),
        "\n",
        include_str!("../vm_exec/load_class.rs"),
        "\n",
        include_str!("../vm_exec/native_new_initializes_class_first.rs"),
        "\n",
        include_str!("../vm_exec/new_object.rs"),
        "\n",
        include_str!("../vm_exec/gen_referent_read_keep_alive.rs"),
        "\n",
        include_str!("../vm_exec/append_native_standin_frames.rs"),
        "\n",
        include_str!("../vm_exec/unloaded_native_libraries.rs"),
        "\n",
        include_str!("../vm_exec/extract_annotations_from_attributes.rs"),
        "\n",
        include_str!("../vm_exec/invoke_static_settling_with_first.rs"),
        "\n",
        include_str!("../vm_exec/invoke_special_shared.rs"),
        "\n",
        include_str!("../vm_exec/proxy_resolve_declaring_method.rs"),
        "\n",
        include_str!("../vm_exec/invoke_on_class_shared_inner.rs"),
        "\n",
        include_str!("../vm_exec/jni_table_native_call.rs")
    )
    .replace("\r\n", "\n");
    let reader = body(
        &src,
        "\npub(in crate::vm::vm_exec) fn thread_stack_trace_served(",
    );
    let state = reader
        .find("let parked_at_start = ")
        .expect("the early read");
    let no_handshake = reader
        .find("&& !parked_at_start\n            && frame_trace_by_handshake(")
        .expect("a parked target takes the pause, not the poll-served handshake");
    assert!(state < no_handshake);
    let pause = reader.find("stw_publish_frame_traces(").expect("the pause");
    // W6-F merge: the screen takes the state before the pause for a
    // thread parked then, else the one read with the trace.
    let parked = reader
        .find("let parked = if parked_at_start {")
        .expect("the state the leaf frames are screened by");
    let leaf = reader
        .find("finish_published_trace(")
        .expect("the leaf frames");
    assert!(state < pause && pause < parked && parked < leaf);
    let append = body(
        &src,
        "\npub(in crate::vm::vm_exec) fn append_parked_standin_entries(",
    );
    assert!(append.contains("matches!(sample.block, 1 | 3)"));
    let gc = concat!(
        include_str!("../../runtime/interpreter/gc_and_alloc/mod.rs"),
        "\n",
        include_str!("../../runtime/interpreter/gc_and_alloc/mtroots_on.rs"),
        "\n",
        include_str!("../../runtime/interpreter/gc_and_alloc/gc_notifications_go_to_delivery.rs"),
        "\n",
        include_str!("../../runtime/interpreter/gc_and_alloc/zgc_refusal_line.rs"),
        "\n",
        include_str!("../../runtime/interpreter/gc_and_alloc/finalizers_forced.rs"),
        "\n",
        include_str!("../../runtime/interpreter/gc_and_alloc/reference_round.rs"),
        "\n",
        include_str!("../../runtime/interpreter/gc_and_alloc/on_out_of_memory_error_commands.rs"),
        "\n",
        include_str!("../../runtime/interpreter/gc_and_alloc/frozen_peer_referents.rs"),
        "\n",
        include_str!("../../runtime/interpreter/gc_and_alloc/g1_force_full_cycle.rs")
    )
    .replace("\r\n", "\n");
    let excused = body(
        &gc,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn publish_excused_native_traces(",
    );
    let chain = excused
        .find("parked_standin_frames_for_thread(shared, &cm, thread_obj, top)")
        .expect("the excused chain");
    let extend = excused.find("entries.extend(head);").expect("the head");
    assert!(chain < extend);
    assert!(excused.contains("let head = if standin.is_some() { None } else { head };"));
    // The attribution of the trace without its top stand-in rows, when
    // the thread's own deposit replaced the pause's.
    assert!(
        src.contains(".find_map(|k| registry.jmx_trace_monitors(registry_tid, captured_len - k))")
    );
    let rows = body(
        &src,
        "\npub(in crate::vm::vm_exec) fn published_standin_rows(",
    );
    assert!(rows.contains("parked_standin_frames_for_thread(shared, cm, Some(thread_obj), below)"));
    // A pause's publication for the native call stands through the
    // thread's own deposit inside it.
    assert!(src.contains("&& !self.thread.gc_block_state.native_excuse.pause_published_trace()"));
    assert!(excused.contains("x.state.native_excuse.note_pause_published_trace();"));
    // A busy JIT chain publishes nothing, not the interpreter frames alone.
    let busy = excused.find("JIT chain busy").expect("the busy-chain arm");
    assert!(excused[..busy].contains("None => {"));
    assert!(excused[busy..].contains("continue;"));
    // Within a dump, the state its one pause was preceded by.
    assert!(reader.contains(".find(|&&(t, _)| t == tid.0)"));
    // And the state the trace describes goes out with it.
    assert!(reader.contains("let state_at_trace = if appended > 0 || standin_rows != 0 {"));
    assert!(
        src.contains("registry.jmx_trace_monitors(registry_tid, captured_len + appended_rows)")
    );
}
