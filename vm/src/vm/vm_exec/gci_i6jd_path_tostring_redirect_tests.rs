/// gci i6/jd: the by-name `toString` redirect to the carrier `Path` native
/// takes only this VM's carrier Paths, in BOTH modes; a real `JrtPath` /
/// `ZipPath` runs its own `toString()` (HotSpot's answer).
#[test]
fn only_carrier_paths_are_redirected_in_both_modes() {
    let src = concat!(include_str!("../vm_exec/mod.rs"), "\n", include_str!("../vm_exec/method.rs"), "\n", include_str!("../vm_exec/report_pad_access.rs"), "\n", include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../vm_exec/load_class.rs"), "\n", include_str!("../vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../vm_exec/new_object.rs"), "\n", include_str!("../vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../vm_exec/invoke_special_shared.rs"), "\n", include_str!("../vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../vm_exec/jni_table_native_call.rs"));
    let start = src
        .find("let (is_path, is_carrier) = {")
        .expect("the redirect block");
    let block = &src[start..start + 1600];
    assert!(block.contains("if is_carrier {"));
    assert!(!block.contains("is_jdk_only()"));
}
