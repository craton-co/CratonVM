fn item(src: &str, head: &str) -> String {
    let start = src.find(head).unwrap_or_else(|| panic!("{head}"));
    let rest = &src[start..];
    rest[..rest.find("\n}\n").expect("its end")].to_string()
}

/// Source pin: the entry-frame hook reaches the callback point only with
/// both switches on, at the frame's own index; `execute` pushes through it.
#[test]
fn the_entry_frame_hook_posts_at_the_frames_own_point() {
    let src = include_str!("../jvmti_events.rs").replace("\r\n", "\n");
    let hook = item(&src, "pub(crate) fn fire_method_entry_after_push_as_loop_entry(");
    assert!(hook.contains("Some((shared, true))"));
    let in_loop = item(&src, "pub(crate) fn fire_method_entry_after_push_in_loop(");
    assert!(in_loop.contains("Some((shared, false))"));
    let with = item(&src, "\nfn fire_method_entry_after_push_with(");
    assert!(with.contains("true"));
    assert!(with.contains("(!entry_frame || true)"));
    let point = item(&src, "\nfn fire_method_entry_at_callback_point(");
    let entry = point.find("depth.saturating_sub(1)").expect("the frame's own index");
    let caller = point.find("depth.saturating_sub(2)").expect("the caller's index");
    assert!(entry < caller);
    let interp = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let execute = item(&interp, "\npub fn execute(\n");
    assert!(execute.contains("push_frame_and_fire_entry_as_loop_entry(shared, thread, frame)"));
}
