fn read(rel: &str) -> String {
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .replace("\r\n", "\n")
}

fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

fn at(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
}

/// A `false` call that finds a state with frames resumes it (not a first
/// slice), and only a frameless first slice posts the start events.
#[test]
fn gci_i20st_a_yielded_state_with_no_tail_is_resumed_not_restarted() {
    let src = read("src/vm/jdk_continuation.rs");
    let enter = body(&src, "pub(crate) fn enter_special(");
    let check = at(&enter, "if thread.frames.is_empty() {", "frames check");
    let post = at(&enter, "post_thread_started(shared, thread.thread_id", "start events");
    let heal = at(&enter, "resume_untailed = true;", "resume flag");
    assert!(check < post && post < heal, "the start events belong to the frameless arm only");
    assert!(
        enter.contains("first: !is_continue && !resume_untailed,"),
        "a resumed untailed state must not run as a first slice"
    );
}

/// The first yield's failed `StackChunk` does not escape as a throwable,
/// and a failed slice reaches Java as a `java.lang.InternalError`.
#[test]
fn gci_i20st_failure_exits_reach_java_as_throwables() {
    let src = read("src/vm/jdk_continuation.rs");
    let enter = body(&src, "pub(crate) fn enter_special(");
    let yielded = at(&enter, "SliceOutcome::Yielded => match mark_tail(", "yield arm");
    let swallow = at(&enter, "Err(MethodCallFailed::ExceptionThrown(_)) => Ok(()),", "chunk OOM");
    assert!(yielded < swallow);
    assert!(enter.contains("SliceOutcome::Failed(message) => Err(slice_failure_as_java(ctx, &message)),"));
    let conv = body(&src, "fn slice_failure_as_java(");
    assert!(conv.contains("\"java/lang/InternalError\""));
    assert!(conv.contains("MethodCallFailed::ExceptionThrown(exc)"));
}
