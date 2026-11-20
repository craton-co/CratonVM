//! gci i47w lane oz47: a `Thread.start()` worker binds its class-load
//! attribution (`cratonvm_classloading::set_current_thread_id`) to its own
//! id before any Java runs and before its `ThreadStart`, so a C agent's
//! `ClassLoad` for a class it defines is posted on its own `JvmThread`, not
//! on thread 0's (`main`'s).

/// Source pin: in the platform thread body the binding follows the OS-tid
/// publication and precedes the C `ThreadStart` post.
#[test]
fn gci_i47w_oz47_started_thread_binds_its_class_load_thread_id() {
    let src = concat!(include_str!("../vm_exec/mod.rs"), "\n", include_str!("../vm_exec/method.rs"), "\n", include_str!("../vm_exec/report_pad_access.rs"), "\n", include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../vm_exec/load_class.rs"), "\n", include_str!("../vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../vm_exec/new_object.rs"), "\n", include_str!("../vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../vm_exec/invoke_special_shared.rs"), "\n", include_str!("../vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n");
    let os_tid = src
        .find("shared_arc.threads.thread_registry.set_os_tid_current(tid);")
        .expect("the OS-tid publication");
    let rest = &src[os_tid..];
    let bind = rest
        .find("cratonvm_classloading::set_current_thread_id(\n                shared_arc.classes.class_layout_domain,\n                tid.0,\n            );")
        .expect("the class-load attribution binding");
    let start = rest
        .find(concat!(
            "crate::jvmti::native_env::post_thread_event",
            "(&shared_arc, tid.0 as u64, true)"
        ))
        .expect("the C ThreadStart post");
    assert!(bind < start, "bound before ThreadStart and before run()");
    let ready = rest.find("mark_stw_ready(tid)").expect("the ready mark");
    assert!(bind < ready, "bound before the thread runs anything");
}
