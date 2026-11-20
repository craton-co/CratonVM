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

/// Finding 1: the `StackChunk` allocation reports heap exhaustion as
/// `InternalError(VmError::Runtime(OutOfMemoryError))`, not as a thrown
/// exception, so the yield arm swallows the `InternalError` shape too and
/// propagates nothing.
#[test]
fn gci_i21rv_the_yield_arm_swallows_every_tail_failure() {
    let src = read("src/vm/jdk_continuation.rs");
    let enter = body(&src, "pub(crate) fn enter_special(");
    let arm_at = enter
        .find("SliceOutcome::Yielded => match mark_tail(")
        .expect("the yield arm");
    let arm = &enter[arm_at..];
    let arm = &arm[..arm.find("SliceOutcome::Done").expect("the next arm")];
    assert!(arm.contains("Err(MethodCallFailed::ExceptionThrown(_)) => Ok(()),"));
    assert!(arm.contains("Err(MethodCallFailed::InternalError(_)) => Ok(()),"));
    assert!(
        !arm.contains("other => other"),
        "no failure of the tail write escapes"
    );
    // The allocation door's failure shape the arm must catch.
    let exec = read("src/vm/vm_exec/new_object.rs");
    let alloc_at = exec
        .find("    fn new_object_initialized(\n")
        .expect("NativeContextImpl::new_object_initialized");
    let alloc = &exec[alloc_at..];
    let alloc = &alloc[..alloc.find("\n    \u{7d}\n").expect("its end")];
    assert!(alloc.contains("MethodCallFailed::InternalError(VmError::Runtime("));
    assert!(alloc.contains("RuntimeError::OutOfMemoryError \u{7b}"));
}

/// Finding 2: an `OutOfMemoryError` has a Java form, so building the
/// `InternalError` under heap pressure falls back to it, not to the raw
/// internal error.
#[test]
fn gci_i21rv_a_failed_throwable_falls_back_to_its_java_form() {
    let oome = crate::error::RuntimeError::OutOfMemoryError {
        message: "Java heap space".to_string(),
    };
    assert!(oome.as_java_throwable().is_some());
    let src = read("src/vm/jdk_continuation.rs");
    let conv = body(&src, "fn slice_failure_as_java(");
    let thrown = conv
        .find("Err(thrown @ MethodCallFailed::ExceptionThrown(_)) => thrown,")
        .expect("a thrown error is kept");
    let runtime = conv
        .find("if error.as_java_throwable().is_some() =>")
        .expect("a Java-convertible runtime error is kept");
    let raw = conv.find("Err(_) => internal(message),").expect("the raw fallback");
    assert!(thrown < raw && runtime < raw, "the raw fallback is the last arm");
}

/// Finding 4: a failed slice whose thread the VM tore down marks the
/// continuation `done` (before the throwable is built); one whose state
/// was deposited back does not.
#[test]
fn gci_i21rv_an_ended_failed_slice_marks_the_continuation_done() {
    let src = read("src/vm/jdk_continuation.rs");
    let enter = body(&src, "pub(crate) fn enter_special(");
    let arm = enter
        .find("SliceOutcome::FailedEnded(message) => ")
        .expect("the ended arm");
    let arm = &enter[arm..];
    let done = arm
        .find("ctx.set_field_by_name(cont, \"done\", Value::Int(1));")
        .expect("done is set");
    let conv = arm
        .find("Err(slice_failure_as_java(ctx, &message))")
        .expect("then thrown");
    assert!(done < conv, "done before the allocation that may move `cont`");
    let deposited_arm = enter
        .find("SliceOutcome::Failed(message) => Err(slice_failure_as_java(ctx, &message)),")
        .expect("the deposited arm");
    assert!(!enter[deposited_arm..]
        .split("SliceOutcome::FailedEnded")
        .next()
        .expect("the deposited arm's text")
        .contains("\"done\""));
    let slice = body(&src, "fn run_slice(");
    assert!(slice.contains("Err(e) => SliceOutcome::FailedEnded(format!("));
    let runner = body(&src, "fn run_on_runner(");
    let retired = runner.find("end_manager_record(shared, tid);").expect("the retire");
    let ended = runner
        .find("return SliceOutcome::FailedEnded(message);")
        .expect("ended");
    let deposit = runner.find(".deposit_continuation(tid.0, thread);").expect("deposit");
    let failed = runner.find("return SliceOutcome::Failed(message);").expect("deposited");
    assert!(retired < ended && ended < deposit && deposit < failed);
}

/// Finding 3: the two other post-mount exits are Java throwables too.
#[test]
fn gci_i21rv_no_post_mount_exit_is_a_raw_internal_error() {
    let src = read("src/vm/jdk_continuation.rs");
    let enter = body(&src, "pub(crate) fn enter_special(");
    for message in [
        "\"Continuation.enterSpecial: the continuation has no frames\"",
        "\"Continuation.enterSpecial: the throwable was not kept\"",
    ] {
        let at = enter.find(message).unwrap_or_else(|| panic!("{message}"));
        let before = &enter[..at];
        let conv = before.rfind("slice_failure_as_java(").expect("converted");
        let raw = before.rfind("internal(").unwrap_or(0);
        assert!(conv > raw, "{message} goes through slice_failure_as_java");
    }
}
