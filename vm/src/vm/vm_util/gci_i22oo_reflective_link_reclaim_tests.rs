use super::reflection_link_refusal_may_reclaim;
use crate::error::{LinkageError, MethodCallFailed, RuntimeError, VmError};
use cratonvm_types::ObjectRef;

fn heap_refusal() -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
        message: "Java heap space (prepare_class C.F ConstantValue String)".to_string(),
    }))
}

/// The enforced reflective link goes through the reclaiming door, and the
/// door asks its rule before it collects.
#[test]
fn gci_i22oo_the_reflective_link_asks_before_it_collects() {
    let src = include_str!("../vm_util.rs").replace("\r\n", "\n");
    let entry = src
        .split("pub fn link_class_for_reflection(")
        .nth(1)
        .and_then(|rest| rest.split("\nfn ").next())
        .expect("link_class_for_reflection body");
    assert!(
        entry.contains("return link_class_for_reflection_reclaiming(shared, thread, class_id);")
    );
    let body = src
        .split("fn link_class_for_reflection_reclaiming(")
        .nth(1)
        .and_then(|rest| rest.split("\nfn ").next())
        .expect("link_class_for_reflection_reclaiming body");
    let rule = body
        .find("reflection_link_refusal_may_reclaim(&err)")
        .expect("the rule");
    let reclaim = body
        .find("native_reclaim_before_alloc_retry_graced(")
        .expect("the collection");
    let relink = body
        .rfind("link_class_on_thread(shared, thread, class_id)")
        .expect("the second link");
    assert!(rule < reclaim && reclaim < relink);
}

/// The collection is safe only because every caller of
/// `NativeContext::link_class_for_reflection` holds no heap reference it
/// reads again across it. Exactly the three reflective listings call it;
/// a new caller must be checked for that and counted here.
#[test]
fn gci_i22oo_the_reflective_link_callers_hold_no_reference_across_it() {
    let lang_class = concat!(
        include_str!("../../../../native-builtins/src/lang_class/mod.rs"),
        "\n",
        include_str!("../../../../native-builtins/src/lang_class/dbg_bb_enabled.rs"),
        "\n",
        include_str!("../../../../native-builtins/src/lang_class/wf7_synthesise_entry_class_if.rs"),
        "\n",
        include_str!(
            "../../../../native-builtins/src/lang_class/descriptor_to_class_mirror_via.rs"
        ),
        "\n",
        include_str!("../../../../native-builtins/src/lang_class/native_field_set.rs"),
        "\n",
        include_str!(
            "../../../../native-builtins/src/lang_class/executable_access_refusal_chain.rs"
        ),
        "\n",
        include_str!("../../../../native-builtins/src/lang_class/forget_lang_class_keys.rs"),
        "\n",
        include_str!(
            "../../../../native-builtins/src/lang_class/gc_update_annotation_proxy_refs.rs"
        ),
        "\n",
        include_str!(
            "../../../../native-builtins/src/lang_class/native_class_is_annotation_present.rs"
        ),
        "\n",
        include_str!("../../../../native-builtins/src/lang_class/classloader_packages_synced.rs"),
        "\n",
        include_str!("../../../../native-builtins/src/lang_class/declaring_class_loader_aware.rs")
    )
    .replace("\r\n", "\n");
    assert_eq!(
        lang_class.matches("ctx.link_class_for_reflection(").count(),
        3
    );
    for head in [
        "pub fn native_class_get_declared_fields(",
        "pub fn native_class_get_declared_methods(",
        "pub fn native_class_get_declared_constructors(",
    ] {
        let body = lang_class
            .split(head)
            .nth(1)
            .and_then(|rest| rest.split("\n\u{7d}\n").next())
            .expect("listing body");
        assert_eq!(
            body.matches("ctx.link_class_for_reflection(").count(),
            1,
            "{head}"
        );
    }
    let lib = include_str!("../../../../native-builtins/src/lib.rs");
    assert_eq!(lib.matches("ctx.link_class_for_reflection(").count(), 0);
}
