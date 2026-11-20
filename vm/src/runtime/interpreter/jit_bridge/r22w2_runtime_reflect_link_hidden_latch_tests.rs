/// Wave 3 (the w2 build's `method-s0` row): the latch is cleared when a
/// constant pool resolves `StackWalker$Option.SHOW_HIDDEN_FRAMES`, at the
/// top of the one resolution tail every field resolution (interpreter and
/// the JIT tiers' `MemberResolver` peeks) runs on a miss.
#[test]
fn r22w3_resolving_show_hidden_frames_clears_the_latch() {
    let src = include_str!("../field_access.rs");
    let tail = src
        .split("pub(super) fn resolve_field_in_class(")
        .nth(1)
        .expect("resolution tail");
    let tail = &tail[..tail.find("\n\u{7d}\n").expect("end")];
    let hook = tail
        .find("note_hidden_frames_option_resolved(shared, current_class_id, &field_class_name, &field_name)")
        .expect("the hook");
    let lock = tail.find("class_manager.read()").expect("the lookup");
    assert!(hook < lock, "asked before the lookup, so a failed lookup latches too");
    let body = src
        .split("fn note_hidden_frames_option_resolved(")
        .nth(1)
        .expect("hook body");
    let body = &body[..body.find("\n\u{7d}\n").expect("end")];
    assert!(body.contains("\"SHOW_HIDDEN_FRAMES\""));
    assert!(body.contains("\"java/lang/StackWalker$Option\""));
    assert!(body.contains("native_methods.note_hidden_frames_observed()"));
    // Review w3 item 1: a JDK referrer (`StackWalker$Option.<clinit>`'s own
    // `putstatic`, `StackWalker`'s option tests) never clears the latch.
    let skip = body.find("[\"java/\", \"jdk/\", \"sun/\", \"com/sun/\"]").expect("the JDK skip");
    let note = body.find("note_hidden_frames_observed()").expect("the note");
    assert!(skip < note, "the skip is asked before the note");
}

#[test]
fn r22w2_both_reflective_links_decline_per_call_once_hidden_frames_can_be_seen() {
    let helpers = crate::jit::helpers::HELPERS_SRC;
    for redirect in [
        "unsafe fn reflect_accessor_link_redirect(",
        "unsafe fn reflect_ctor_link_redirect(",
    ] {
        let body = helpers.split(redirect).nth(1).expect("redirect");
        let body = &body[..body.find("\n\u{7d}\n").expect("end")];
        let latch = body
            .find("reflect_link_declines_for_hidden_frames(vm)")
            .expect("the latch check");
        let target = body.find("_link_target(").expect("the target");
        assert!(latch < target, "{redirect}: the latch is asked first");
    }
    let vm = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    assert!(crate::jit::helpers::reflect_link_hidden_latch_enabled(&vm));
    assert!(crate::jit::helpers::reflect_link_hidden_standins_enabled(
        &vm
    ));
    let decline = helpers
        .split("fn reflect_link_declines_for_hidden_frames(")
        .nth(1)
        .expect("decline");
    let decline = &decline[..decline.find("\n\u{7d}\n").expect("end")];
    for term in [
        "hidden_frames_observed()",
        "config.show_hidden_frames",
        "jvmti_agent_options",
        "jdwp_port",
        "jdwp_agent",
        "any_listener_active()",
    ] {
        assert!(decline.contains(term), "{term}");
    }
}
