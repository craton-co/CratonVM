use super::*;

#[test]
fn a_deferred_entry_names_only_its_own_top_frame() {
    assert!(deferred_entry_names_top(3, 3));
    assert!(!deferred_entry_names_top(0, 0), "no record");
    assert!(!deferred_entry_names_top(3, 4), "a frame above it");
    assert!(!deferred_entry_names_top(3, 2), "its frame is gone");
}

/// Source pin: the in-loop door falls back to the plain post with either
/// switch off, and otherwise posts at the callback point.
#[test]
fn the_in_loop_door_posts_at_the_callback_point() {
    let src = include_str!("../jvmti_events.rs").replace("\r\n", "\n");
    let item = |head: &str| -> String {
        let start = src.find(head).unwrap_or_else(|| panic!("{head}"));
        let rest = &src[start..];
        rest[..rest.find("\n}\n").expect("its end")].to_string()
    };
    // Wave 135 (lane L1): both deferred doors share one body, the
    // entry-frame one under `CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL` too.
    assert!(item("pub(crate) fn fire_deferred_method_entry_in_loop(")
        .contains("fire_deferred_method_entry_at_point(shared, thread, false)"));
    assert!(item("pub(crate) fn fire_deferred_method_entry_as_loop_entry(")
        .contains("fire_deferred_method_entry_at_point(shared, thread, true)"));
    let body = item("\nfn fire_deferred_method_entry_at_point(");
    let gate = body
        .find("true")
        .expect("the switch");
    let nested = body
        .find("true")
        .expect("the nested switch");
    let plain = body
        .find("fire_deferred_method_entry(shared.vm_identity, thread)")
        .expect("the fallback");
    let point = body
        .find("fire_method_entry_at_callback_point(shared, thread, entry_frame)")
        .expect("the point");
    assert!(gate < plain && nested < plain && plain < point);
    let sync = concat!(include_str!("../../../vm/vm_exec/mod.rs"), "\n", include_str!("../../../vm/vm_exec/method.rs"), "\n", include_str!("../../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../../vm/vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n");
    assert!(sync.contains("fire_deferred_method_entry_in_loop(shared, thread)"));
    assert!(sync.contains("fire_deferred_method_entry_as_loop_entry(shared, thread)"));
}
