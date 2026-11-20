use super::*;

fn read(rel: &str) -> String {
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .replace("\r\n", "\n")
}

fn at(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
}

#[test]
fn gci_i26rv_a_park_inside_run_continuation_is_not_past_after_yield() {
    // The carrier parks in `schedule(..)` called by `afterYield`.
    assert!(frames_are_inside_run_continuation([
        ("java/util/concurrent/ForkJoinWorkerThread", "run"),
        ("java/lang/VirtualThread", "runContinuation"),
        ("java/lang/VirtualThread", "afterYield"),
        ("java/util/concurrent/ForkJoinPool", "spinLockRunState"),
        ("java/util/concurrent/locks/LockSupport", "parkNanos"),
    ]));
    // An inlined `afterYield` alone is enough.
    assert!(frames_are_inside_run_continuation([(
        "java/lang/VirtualThread",
        "afterYield"
    )]));
    // `awaitWork`: the task returned, the window may be resolved.
    assert!(!frames_are_inside_run_continuation([
        ("java/util/concurrent/ForkJoinWorkerThread", "run"),
        ("java/util/concurrent/ForkJoinPool", "runWorker"),
        ("java/util/concurrent/ForkJoinPool", "awaitWork"),
        ("java/util/concurrent/locks/LockSupport", "park"),
    ]));
    // Another class's method of the same name does not count.
    assert!(!frames_are_inside_run_continuation([(
        "com/example/VirtualThread",
        "runContinuation"
    )]));
    assert!(!frames_are_inside_run_continuation(std::iter::empty()));
}

#[test]
fn gci_i26rv_the_park_seam_tests_the_stack_before_resolving() {
    let src = read("src/vm/jdk_continuation.rs");
    let start = at(
        &src,
        "pub(crate) fn resolve_post_yield_window_at_park(",
        "park seam",
    );
    let body = &src[start..];
    let body = &body[..at(body, "\n\u{7d}\n", "end of the park seam")];
    let gate = at(body, "post_yield_window()", "bit gate");
    let peek = at(body, ".get(&ctx.thread.thread_id.0)", "peek, not remove");
    let state = at(body, "after_yield_allocates(", "state filter");
    let stack = at(body, "carrier_inside_run_continuation(ctx)", "stack test");
    let resolve = at(body, "resolve_post_yield_window(ctx);", "resolution");
    assert!(gate < peek && peek < state && state < stack && stack < resolve);
    assert!(
        !body.contains(".remove("),
        "the park seam never takes the window itself"
    );
    let exec = read("src/vm/vm_exec/invoke_static_settling_with_first.rs");
    assert!(exec.contains("super::jdk_continuation::resolve_post_yield_window_at_park(self);"));
    assert!(!exec.contains("super::jdk_continuation::resolve_post_yield_window(self);"));
}
