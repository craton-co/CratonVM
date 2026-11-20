#[test]
pub fn a_null_special_receiver_is_refused_before_any_compiled_callee_arm() {
    let src = crate::jit::helpers::HELPERS_SRC;
    let wrapper = src
        .find("\npub unsafe fn jit_invoke_dispatch_body(")
        .expect("jit_invoke_dispatch_body");
    let inner = src[wrapper..]
        .find("\nunsafe fn jit_invoke_dispatch_body_inner(")
        .map(|offset| wrapper + offset)
        .expect("jit_invoke_dispatch_body_inner");
    assert!(src[wrapper..inner].contains("jit_invoke_dispatch_body_inner("));
    let end = src[inner..]
        .find("\n}\n")
        .map(|offset| inner + offset + 2)
        .expect("end of dispatch body");
    let body = &src[inner..end];
    let check = body
        .find("if info.invoke_kind == 1 && args_slice.first() == Some(&0) {")
        .expect("the kind-1 null-receiver test is gone from the dispatch door");
    let first_compiled_arm = body
        .find("DISPATCH_CACHE.with(")
        .expect("the dispatch cache arm");
    assert!(
        check < first_compiled_arm,
        "the kind-1 null-receiver test must precede every compiled-callee arm"
    );
}
