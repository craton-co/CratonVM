/// gci i15/hx (`gci-i15hx-jvmti-tag-agent-hangs-with-native-transitions-FIXED-20261014`):
/// a `RunAgentThread` start function runs in native -- the in-native
/// bracket opens after this thread's JNI context, `JvmThread` and local
/// frame are installed, around exactly the `proc` call, and closes before
/// the context is restored (its leave needs the `JvmThread`).
#[test]
fn gci_i15hx_the_agent_start_function_runs_in_native() {
    let src = include_str!("../agent_functions.rs").replace("\r\n", "\n");
    let start = src
        .find("pub(crate) fn run_agent_thread_body(")
        .expect("run_agent_thread_body");
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").expect("the function's end")];
    let pos = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle}"));
    let enter = pos("jni::JniNativeCall::enter(jni::raw_local_escapes())");
    assert!(pos("jni::push_local_frame(") < enter);
    assert!(pos("jni::replace_jni_context(shared)") < enter);
    assert!(pos("jni::replace_jni_thread(") < enter);
    assert!(pos("jni::RawLocalsDispatch::enter()") < enter);
    // gci i21/fx: rustfmt lays the call out one argument per line, so the
    // call is the transmuted `proc` followed by its first argument.
    let call = pos("std::mem::transmute::<usize, StartFunction>(body.proc)");
    assert!(call < pos("body.env as *mut JvmtiNativeEnv"));
    let leave = pos("drop(in_native);");
    assert!(enter < call && call < leave);
    assert!(leave < pos("drop(raw_jni_locals);"));
    assert!(leave < pos("jni::restore_jni_thread(prev_thread)"));
    assert!(leave < pos("jni::restore_jni_context(prev_vm)"));
}
