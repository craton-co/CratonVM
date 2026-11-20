/// Text pin: the settling door settles on a callee's throwable
/// and never on an internal error, and still asks the owner's
/// initialization first.
#[test]
fn a_static_target_that_throws_settles_its_owner() {
    let src = concat!(
        include_str!("../vm_exec/mod.rs"),
        "\n",
        include_str!("../vm_exec/method.rs"),
        "\n",
        include_str!("../vm_exec/report_pad_access.rs"),
        "\n",
        include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"),
        "\n",
        include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"),
        "\n",
        include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"),
        "\n",
        include_str!("../vm_exec/load_class.rs"),
        "\n",
        include_str!("../vm_exec/native_new_initializes_class_first.rs"),
        "\n",
        include_str!("../vm_exec/new_object.rs"),
        "\n",
        include_str!("../vm_exec/gen_referent_read_keep_alive.rs"),
        "\n",
        include_str!("../vm_exec/append_native_standin_frames.rs"),
        "\n",
        include_str!("../vm_exec/unloaded_native_libraries.rs"),
        "\n",
        include_str!("../vm_exec/extract_annotations_from_attributes.rs"),
        "\n",
        include_str!("../vm_exec/invoke_static_settling_with_first.rs"),
        "\n",
        include_str!("../vm_exec/invoke_special_shared.rs"),
        "\n",
        include_str!("../vm_exec/proxy_resolve_declaring_method.rs"),
        "\n",
        include_str!("../vm_exec/invoke_on_class_shared_inner.rs"),
        "\n",
        include_str!("../vm_exec/jni_table_native_call.rs")
    )
    .replace("\r\n", "\n");
    let start = src
        .find("\npub(in crate::vm::vm_exec) fn invoke_static_settling_with_first_class(")
        .expect("the door");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    let arm = body
        .find("Err(MethodCallFailed::ExceptionThrown(_)) =>")
        .expect("the throw arm");
    assert!(body[arm..].starts_with(
        "Err(MethodCallFailed::ExceptionThrown(_)) => {\n            true\n        }"
    ));
    assert!(body.contains("Err(_) => false,"));
    assert!(body.contains(
        "settles_on\n        && static_owner_init_pending(shared, owner, class_name, method_name, descriptor).is_none()"
    ));
}
