/// gci i18/sw (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 4):
/// the lock path of `compare_and_swap_field_shared` logs the overwritten
/// value explicitly only off G1 (G1's accessors log it themselves), and
/// still before the store; the hardware reference CAS keeps its explicit
/// pre-barrier on `expected` (it stores through no accessor).
#[test]
fn gci_i18sw_the_cas_lock_path_leaves_g1s_pre_barrier_to_its_accessors() {
    let src = concat!(include_str!("../vm_exec/mod.rs"), "\n", include_str!("../vm_exec/method.rs"), "\n", include_str!("../vm_exec/report_pad_access.rs"), "\n", include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../vm_exec/load_class.rs"), "\n", include_str!("../vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../vm_exec/new_object.rs"), "\n", include_str!("../vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../vm_exec/invoke_special_shared.rs"), "\n", include_str!("../vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n");
    let fn_body = |sig: &str| -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("{sig} moved"));
        let body = &src[start..];
        body[..body.find("\n\u{7d}\n").expect("function end")].to_string()
    };
    // JIT round 23 wave 1, lane L4: the lock path is its own function
    // (`compare_and_swap_field_locked`), and the hardware reference CAS
    // moved to `hw_cas_reference`.
    let body = fn_body("fn compare_and_swap_field_locked(");
    let body = body.as_str();
    assert!(fn_body("fn hw_cas_reference(")
        .contains("shared.mem.heap.satb_barrier(expected);"));
    let guarded = "if !shared.mem.heap.is_g1() \u{7b}\n                shared.mem.heap.satb_barrier(current);";
    let at = body.find(guarded).expect("the G1-guarded lock-path pre-barrier");
    let store = body
        .find("let _ = shared.mem.heap.set_array_element(obj, index, new_val);")
        .expect("the lock path's array store");
    assert!(at < store, "the pre-barrier still precedes the store");
    assert_eq!(
        body.matches("shared.mem.heap.satb_barrier(current);").count(),
        1,
        "no unguarded copy of the lock-path pre-barrier is left"
    );
}
