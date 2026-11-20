use super::held_trace_is_stale;

/// W9-1: only a WAITING / TIMED_WAITING thread outside the GC-blocked
/// region (a lazy park or wait) whose trace was not written since before
/// the pause is stale; a blocked thread's deposit, a republished trace,
/// a running or BLOCKED thread, and an unknown count are not.
#[test]
fn w9_1_a_lazy_parker_not_republished_in_the_pause_is_stale() {
    assert!(held_trace_is_stale(3, false, Some(7), Some(7)));
    assert!(held_trace_is_stale(1, false, Some(7), Some(7)));
    assert!(
        !held_trace_is_stale(3, true, Some(7), Some(7)),
        "blocked: its deposit"
    );
    assert!(
        !held_trace_is_stale(3, false, Some(7), Some(8)),
        "republished"
    );
    assert!(!held_trace_is_stale(0, false, Some(7), Some(7)), "running");
    assert!(
        !held_trace_is_stale(2, false, Some(7), Some(7)),
        "BLOCKED on entry"
    );
    assert!(!held_trace_is_stale(3, false, None, None), "unknown thread");
}

/// W9-1 (source pin): the snapshot retries a stale held read, and the
/// held read takes the count inside the pause.
#[test]
fn w9_1_a_stale_held_read_is_taken_again() {
    let src =
        concat!(include_str!("gen_referent_read_keep_alive.rs"), "\n", include_str!("append_native_standin_frames.rs")).replace("\r\n", "\n");
    let start = src
        .find(concat!("fn jmx_thread_", "snapshot(\n"))
        .expect("snapshot");
    let rest = &src[start..];
    let snapshot = &rest[..rest.find("\n    }\n").expect("its end")];
    let before = snapshot
        .find("let seq_before = registry.trace_seq(registry_tid);")
        .expect("before");
    let pause = snapshot
        .find(concat!("stw_publish_frame_traces_", "with("))
        .expect("pause");
    let retry = snapshot
        .find("if !(stale || unshaped) || attempt == attempts")
        .expect("retry");
    assert!(before < pause && pause < retry);
    // W10-7: the attempt count is the switch's, read before the loop
    // (none when a `ThreadInfo` dump's own read answers, W10-7 review).
    let count = snapshot
        .find("let attempts = if held.is_some() { 0 } else { held_read_attempts() };")
        .expect("the attempt count");
    assert!(count < before);
    let held = &src[src.find("\nimpl HeldThreadRead {").expect("held")..];
    let held = &held[..held.find("\n}\n").expect("its end")];
    assert!(held.contains("registry.trace_seq(tid)"));
    assert!(held.contains("registry.is_blocked(tid)"));
}
