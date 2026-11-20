fn body<'a>(src: &'a str, head: &str) -> &'a str {
    let start = src.find(head).expect(head);
    let rest = &src[start..];
    &rest[..rest.find("\n\u{7d}\n").expect("its end")]
}

/// r25 W6-F fix 6 (source pin): another thread's `ThreadInfo` is read
/// at ONE instant -- the held read runs INSIDE the frame-trace pause --
/// and that read takes the state, the trace with its block state, the
/// attribution and the lock book from the registry alone.
#[test]
fn r25_w6f_a_thread_info_is_read_inside_one_pause() {
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
    let start = src
        .find(concat!(
            "    pub(in crate::vm::vm_exec) fn jmx_thread_",
            "snapshot(\n"
        ))
        .expect("the snapshot");
    let rest = &src[start..];
    let snapshot = &rest[..rest.find("\n    }\n").expect("its end")];
    let pause = snapshot
        .find(concat!("stw_publish_frame_traces_", "with("))
        .expect("the pause");
    let read = snapshot
        .find(concat!("|| HeldThreadRead::", "read("))
        .expect("the held read");
    assert!(pause < read);
    let held = body(&src, "\nimpl HeldThreadRead {");
    for read in [
        "HeldTrace::read(registry, tid)",
        "registry.jmx_frame_monitors(tid, trace.trace.len())",
        "registry_run_state_for_block(registry, tid, thread_obj, block)",
        "registry.jmx_lock_snapshot(tid)",
    ] {
        assert!(held.contains(read), "{read}");
    }
    assert!(!held.contains(concat!("class_", "manager")));
    // W6-F fix 7: the book keeps only the monitors the thread holds.
    assert!(held.contains("drop_monitors_not_held(&shared.threads.monitors, tid, &mut book.2)"));
    let trace = body(&src, "\nimpl HeldTrace {");
    assert!(trace.contains("registry.frame_trace_of(tid)"));
    assert!(trace.contains("ParkedSample::read(registry, tid)"));
}

/// r25 W6-F (source pin): the parked leaf screen reads no block state of
/// its own, and every published-trace reader hands it the one read with
/// the trace.
#[test]
fn r25_w6f_the_screen_takes_the_state_read_with_the_trace() {
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
    for head in [
        "\npub(in crate::vm::vm_exec) fn append_parked_standin_entries(",
        "\npub(crate) fn parked_standin_frames_for_thread(",
    ] {
        let screen = body(&src, head);
        assert!(!screen.contains(concat!("java_block_", "state(")), "{head}");
    }
    let finish = body(
        &src,
        "\npub(in crate::vm::vm_exec) fn finish_published_trace(",
    );
    assert!(finish.contains("let HeldTrace { mut trace, parked } = held;"));
    assert!(finish.contains("append_parked_standin_entries(ctx.shared, &cm, parked,"));
    let served = body(
        &src,
        "\npub(in crate::vm::vm_exec) fn thread_stack_trace_served(",
    );
    assert!(served.contains("HeldTrace::read(registry, tid)"));
    let dump = body(&src, "\npub(crate) fn thread_dump_stack(");
    assert!(dump.contains("HeldTrace::read(registry, tid)"));
}
