/// gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
/// item 4): `walking_from` takes ONE `&mut` of the calling thread, after
/// the pause is held, and the sizer and `work` both share it; no second
/// borrow is built from the raw pointer inside the sizer or for `work`.
/// (Source pin: Stacked Borrows cannot be checked in a unit test.)
#[test]
fn gci_i11ja_heap_walk_borrows_the_thread_once() {
    let src = include_str!("../heap.rs").replace("\r\n", "\n");
    let start = src.find("\nfn walking_from<").expect("walking_from");
    let rest = &src[start + 1..];
    let from = &rest[..rest.find("\n}\n").expect("its end")];
    assert_eq!(
        from.matches("&mut *current").count(),
        1,
        "one exclusive borrow"
    );
    assert_eq!(
        from.matches("NativeContextImpl {").count(),
        1,
        "one context"
    );
    assert!(
        !from.contains("&*current"),
        "no shared re-derive for `work`"
    );
    assert!(from.contains("work(&mut walk, &*ctx.thread, prepared)"));
    let sizer = from.find("let sizer =").expect("sizer");
    let ctx = from.find("let ctx =").expect("ctx");
    let pause = from.find("break (pause, prepared)").expect("pause");
    assert!(
        pause < ctx && ctx < sizer,
        "borrow taken after the pause, before the sizer"
    );
    let sizer_body = &from[sizer..sizer + from[sizer..].find("};").expect("its end")];
    assert!(
        sizer_body.contains("object_size_of(&ctx, obj)"),
        "{sizer_body}"
    );
    assert!(!sizer_body.contains("unsafe"), "{sizer_body}");
}
