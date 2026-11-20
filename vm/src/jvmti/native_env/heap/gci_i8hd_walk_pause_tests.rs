/// The body of `fn <name>` in this file, up to the next item at column 0.
fn body<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src.find(&format!("\nfn {name}")).expect("the function");
    let rest = &src[start + 1..];
    &rest[..rest.find("\n}\n").expect("its end")]
}

/// gci i8/hd (`gci-i3hd-heap-walks-and-diagnostics-review-findings-FIXED-20261017`
/// item 7): every walk's pause is asked for in the retry loop of
/// `walking_from`, a refusal waits GC-safe and the limit answers
/// `JVMTI_ERROR_INTERNAL`; no walk takes the pause's `Option` and proceeds
/// on `None`. (Source pin: the JVMTI C table cannot be driven against a
/// competing pause in a unit test.)
#[test]
fn gci_i8hd_a_declined_walk_pause_is_retried_gc_safe() {
    let src = include_str!("../heap.rs").replace("\r\n", "\n");
    let from = body(&src, "walking_from<");
    assert_eq!(from.matches("NonMovingPause::request(").count(), 1);
    assert!(from.contains(
        "if let Some(pause) = crate::runtime::interpreter::NonMovingPause::request("
    ));
    assert!(
        from.contains("super::gc_safe(shared, || std::thread::sleep(DECLINED_PAUSE_SLICE))")
    );
    assert!(from.contains("return Err(ERR_INTERNAL);"));
    // The tag index is refreshed again under the pause, its frees posted after.
    let late = from.find("let late_freed").expect("in-pause refresh");
    assert!(late > from.find("break (pause, prepared)").expect("the pause"));
    assert!(
        from.rfind("post_object_free(e.addr(), &late_freed)")
            .expect("posted")
            > from.find("drop(pause);").expect("end")
    );
    // `walking` is `walking_from` with nothing to prepare.
    let walking = body(&src, "walking<");
    assert!(walking.contains("walking_from("));
    assert!(!walking.contains("NonMovingPause"));
    // The handle-taking walks decode again per request.
    for f in [
        "follow_references(",
        "iterate_over_objects_reachable_from_object(",
    ] {
        let start = src
            .find(&format!("pub(super) extern \"C\" fn {f}"))
            .expect(f);
        let rest = &src[start..];
        let b = &rest[..rest.find("\n}\n").expect("end")];
        assert!(b.contains("walking_from(e, &shared, reinitial,"), "{f}");
    }
}
