use super::{agent_request_verdict, AgentRequestVerdict};

#[test]
fn the_bit_is_taken_only_by_the_records_frame() {
    // Nothing recorded: clear.
    assert_eq!(agent_request_verdict(None, 3, 2), AgentRequestVerdict::Clear);
    // The record's frame on top, its loop at its top: take.
    assert_eq!(agent_request_verdict(Some(3), 3, 2), AgentRequestVerdict::Take);
    // A loop entered above the record's frame (Java run between the
    // raise and that frame's top): keep the bit for it.
    assert_eq!(agent_request_verdict(Some(3), 5, 4), AgentRequestVerdict::Keep);
    // The record's frame count, but the loop runs another frame: keep.
    assert_eq!(agent_request_verdict(Some(3), 3, 1), AgentRequestVerdict::Keep);
    // The record's frame is gone: discard it.
    assert_eq!(agent_request_verdict(Some(4), 3, 2), AgentRequestVerdict::DiscardStale);
}

fn item(src: &str, head: &str) -> String {
    let start = src.find(head).unwrap_or_else(|| panic!("{head}"));
    let rest = &src[start..];
    rest[..rest.find("\n}\n").expect("its end")].to_string()
}

/// Source pins: the loop top reads the word through the checked reader;
/// the nested install posts at the entry-frame point (both arms); the
/// pushed-frame native stack trip drops the frame's control before it
/// builds the error.
#[test]
fn the_loop_and_the_nested_install_use_the_checked_doors() {
    let src = include_str!("../jvmti_events.rs").replace("\r\n", "\n");
    let tail = item(&src, "\nfn install_tail<const IN_LOOP: bool>(");
    assert!(tail.contains("fire_method_entry_after_push_as_loop_entry(shared, thread)"));
    assert!(!tail.contains("fire_entry_and_diagnostics_after_push("));
    let body = item(&src, "\nfn install_cached_frame_for<const IN_LOOP: bool>(");
    assert!(body.contains("push_frame_and_fire_entry_as_loop_entry(shared, thread, frame)"));
    let interp = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    assert!(interp.contains("take_method_entry_frame_control_checked(shared, thread, frame_idx)"));
    let run = item(&interp, "\npub(crate) fn run_pushed_frame_to_completion(\n");
    let drop_at = run
        .find("drop_frame_control_of_unrun_frames(shared, thread, frames_depth_before_push)")
        .expect("the trip drops the frame's control");
    let raise = run
        .find("raise_reentry_stack_overflow(shared, thread)")
        .expect("the trip's error");
    assert!(drop_at < raise);
}
