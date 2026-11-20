/// The switch is latched per thread and answers the same every time.
#[test]
fn the_ite_init_memo_switch_is_latched() {
    let first = super::ite_init_memo_enabled();
    assert_eq!(super::ite_init_memo_enabled(), first);
    assert_ne!(super::ITE_INIT_MEMO.with(std::cell::Cell::get), 0);
}

/// Text pin: the memoised route is taken after the synthetic-stub arm and
/// before the allocate-then-`invoke` route, with the same `(Throwable)`
/// constructor and the original as the fallback.
#[test]
fn the_wrapper_is_built_through_the_memoised_constructor() {
    let src = concat!(include_str!("../lang_class/mod.rs"), "\n", include_str!("../lang_class/dbg_bb_enabled.rs"), "\n", include_str!("../lang_class/wf7_synthesise_entry_class_if.rs"), "\n", include_str!("../lang_class/descriptor_to_class_mirror_via.rs"), "\n", include_str!("../lang_class/native_field_set.rs"), "\n", include_str!("../lang_class/executable_access_refusal_chain.rs"), "\n", include_str!("../lang_class/forget_lang_class_keys.rs"), "\n", include_str!("../lang_class/gc_update_annotation_proxy_refs.rs"), "\n", include_str!("../lang_class/native_class_is_annotation_present.rs"), "\n", include_str!("../lang_class/classloader_packages_synced.rs"), "\n", include_str!("../lang_class/declaring_class_loader_aware.rs")).replace("\r\n", "\n");
    let start = src
        .find("\npub fn wrap_as_invocation_target_exception(")
        .expect("the wrapper");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n}\n").expect("its end")];
    let stub = body
        .find("ctx.is_class_synthetic_stub(target_class)")
        .expect("stub arm");
    let memo = body.find("if ite_init_memo_enabled()").expect("memo arm");
    let invoke = body
        .find("let init_result = ctx.invoke(")
        .expect("the by-name route");
    assert!(stub < memo && memo < invoke);
    assert!(body[memo..invoke].contains("\"(Ljava/lang/Throwable;)V\""));
    assert!(body[memo..invoke].contains("_ => MethodCallFailed::ExceptionThrown(original)"));
}
