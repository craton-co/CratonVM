use super::reentry_stack_margin;

#[test]
fn the_margin_is_bounded_and_never_more_than_half_the_stack() {
    const K: usize = 1024;
    assert_eq!(reentry_stack_margin(128 * K * K), 512 * K);
    assert_eq!(reentry_stack_margin(8 * K * K), 512 * K);
    assert_eq!(reentry_stack_margin(K * K), 128 * K);
    assert_eq!(reentry_stack_margin(128 * K), 64 * K);
}

/// Both nested entries ask the guard before running Java.
#[test]
fn both_nested_entries_ask_the_guard() {
    let src = concat!(include_str!("../interpreter/mod.rs"), "\n", include_str!("../interpreter/cached_invoke_target.rs"), "\n", include_str!("../interpreter/execute.rs"), "\n", include_str!("../interpreter/resume_continuation.rs"), "\n", include_str!("../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    for head in [
        "\npub fn execute(\n",
        "\npub(crate) fn run_pushed_frame_to_completion(\n",
    ] {
        let at = src.find(head).expect("entry");
        let body = &src[at..at + src[at..].find("\n\u{7d}\n").expect("body end")];
        assert!(
            body.contains("reentry_native_stack_exhausted()"),
            "{head:?} must ask the native stack guard"
        );
    }
}

// `#[cfg(test)]` keeps this helper out of the NEW-7 production panic scan.
#[cfg(test)]
fn body_of<'a>(src: &'a str, head: &str) -> &'a str {
    let at = src.find(head).expect("entry");
    &src[at..at + src[at..].find("\n\u{7d}\n").expect("body end")]
}

/// Wave 134, lane L4 (review of w133 item 4): `execute`'s trip offers the
/// lazy error, like the `EXEC_DEPTH` trip, before any in-place build.
#[test]
fn execute_trip_returns_the_lazy_error_first() {
    let src = concat!(
        include_str!("../interpreter/mod.rs"),
        "\n",
        include_str!("../interpreter/cached_invoke_target.rs"),
        "\n",
        include_str!("../interpreter/execute.rs"),
        "\n",
        include_str!("../interpreter/resume_continuation.rs"),
        "\n",
        include_str!("../interpreter/execute_frame_from_index_loop.rs"),
        "\n",
        include_str!("../interpreter/resolve_array_class_loader_aware.rs"),
        "\n",
        include_str!("../interpreter/park_for_debugger_under.rs"),
        "\n",
        include_str!("../interpreter/double_arith_apply.rs")
    )
    .replace("\r\n", "\n");
    let body = body_of(&src, "\npub fn execute(\n");
    assert!(
        body.contains("RuntimeError::StackOverflowError"),
        "execute() must return the lazy StackOverflowError"
    );
}

/// Wave 134, lane L4 (review of w133 item 3): the pushed-frame trip
/// applies to an unrun frame only, drops a deferred `MethodEntry` record
/// of the pushed frame, and pops as an exception unwind.
#[test]
fn pushed_frame_trip_cleans_up_like_the_frame_limit_trip() {
    let src = concat!(include_str!("../interpreter/mod.rs"), "\n", include_str!("../interpreter/cached_invoke_target.rs"), "\n", include_str!("../interpreter/execute.rs"), "\n", include_str!("../interpreter/resume_continuation.rs"), "\n", include_str!("../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let body = body_of(&src, "\npub(crate) fn run_pushed_frame_to_completion(\n");
    let trip = &body[body.find("reentry_native_stack_exhausted()").expect("trip")..];
    let trip = &trip[..trip.find("return result;").expect("trip end")];
    assert!(body.contains("reentry_trip_applies(thread, frames_depth_before_push)"));
    assert!(trip.contains("thread.deferred_method_entry_depth = 0;"));
    assert!(trip.contains("pop_unrun_frames_by_exception_keeping_result("));
    assert!(!trip.contains("pop_root_frames_keeping_result("));
    let pop = body_of(
        &src,
        "\npub(in crate::runtime::interpreter) fn pop_unrun_frames_by_exception_keeping_result(\n",
    );
    assert!(pop.contains("pop_and_recycle_frame_with_reason(shared, thread, true)"));
}
