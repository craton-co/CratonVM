use super::{lambda_accepts_descriptor, lambda_args_sam_compatible, ClassId, SharedVm, Value};
use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
use std::sync::Arc;

fn site(proxy: ClassId, iface: &str, name: &str, sam: &str) -> LambdaCallSite {
    LambdaCallSite {
        functional_interface: Arc::from(iface),
        functional_interface_id: None,
        sam_method_name: Arc::from(name),
        sam_descriptor: Arc::from(sam),
        impl_handle: MethodHandle {
            kind: MethodHandleKind::InvokeStatic,
            class_name: Arc::from("cratonvm/test/L6BridgeHost"),
            member_name: Arc::from("lambda$main$0"),
            descriptor: Arc::from(sam),
        },
        instantiated_descriptor: Arc::from(sam),
        capture_types: vec![],
        proxy_class_id: proxy,
        serializable_flag: false,
    }
}

/// The exact SAM descriptor and a recorded `FLAG_BRIDGES` descriptor enter
/// the body; a same-named default overload, another name, and another
/// proxy's bridge do not.
#[test]
fn a_recorded_bridge_is_accepted_beside_the_exact_sam_descriptor() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let proxy = shared.alloc_lambda_proxy_id();
    let other = shared.alloc_lambda_proxy_id();
    let cs = site(proxy, "cratonvm/test/StrM", "m", "()Ljava/lang/String;");
    let accepts = |id: ClassId, name: &str, desc: &str| {
        lambda_accepts_descriptor(&shared, id, &cs, name, desc)
    };
    assert!(accepts(proxy, "m", "()Ljava/lang/String;"));
    assert!(!accepts(proxy, "m", "()Ljava/lang/Object;"));
    crate::runtime::invokedynamic::record_lambda_proxy_bridges(
        shared.vm_identity,
        proxy,
        &[Arc::from("()Ljava/lang/Object;")],
    );
    assert!(accepts(proxy, "m", "()Ljava/lang/Object;"));
    // A same-named overload that is not a bridge (a857ee34c).
    assert!(!accepts(
        proxy,
        "m",
        "(Ljava/lang/Object;)Ljava/lang/String;"
    ));
    // The bridge belongs to the SAM name only.
    assert!(!accepts(proxy, "n", "()Ljava/lang/Object;"));
    // And to its own proxy only.
    assert!(!accepts(other, "m", "()Ljava/lang/Object;"));
    crate::runtime::invokedynamic::forget_vm_generic_indy_sites(shared.vm_identity);
}

/// The SAM compatibility screen is safepoint-free: a lambda-proxy argument
/// whose functional interface would have to be LOADED to answer is
/// compatible (undecidable), where it used to drive that load and answer
/// "incompatible" from a raw address.
#[test]
fn the_sam_compatibility_screen_does_not_load_for_a_lambda_argument() {
    const NEVER: &str = "cratonvm/test/L6NeverLoadableIface";
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Ok(string) = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/String")
    else {
        return; // Stripped VM (no class library on the test classpath).
    };
    let proxy = shared.alloc_lambda_proxy_id();
    shared
        .classes
        .lambda_proxies
        .write()
        .insert(proxy, Arc::new(site(proxy, NEVER, "run", "()V")));
    let arg = shared.mem.heap.alloc_object(proxy, 0);
    if shared.mem.heap.class_id_of(arg) != proxy || string == ClassId::new(0) {
        return;
    }
    assert!(lambda_args_sam_compatible(
        &shared,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(arg))]
    ));
    assert!(shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(NEVER)
        .is_none());
}

/// Structural: the three lambda-dispatch copies ask the one predicate, and
/// the stale-receiver recovery holds no `lambda_proxies` guard across it.
#[test]
fn every_lambda_dispatch_copy_asks_the_shared_descriptor_predicate() {
    let lambda_src = include_str!("../lambda.rs");
    let start = lambda_src
        .find("pub(crate) fn try_lambda_dispatch(")
        .unwrap_or(0);
    assert!(start > 0, "try_lambda_dispatch moved");
    let head = &lambda_src[start..];
    let head = &head[..head.find("// Read captured values").unwrap_or(0)];
    assert!(head.contains("lambda_accepts_descriptor("));

    let exec_src = concat!(include_str!("../../../vm/vm_exec/mod.rs"), "\n", include_str!("../../../vm/vm_exec/method.rs"), "\n", include_str!("../../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../../vm/vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n");
    let start = exec_src
        .find("fn recover_stale_lambda_receiver_from_native_pins(")
        .unwrap_or(0);
    assert!(start > 0, "the stale-receiver recovery moved");
    let rest = &exec_src[start..];
    let recovery = &rest[..rest.find("\n\x7d\n").unwrap_or(0)];
    assert!(recovery.contains("lambda_accepts_descriptor("));
    assert!(!recovery.contains("lambda_proxies.read()"));
    assert!(!exec_src.contains("descriptor == &*lcs.sam_descriptor"));
    assert!(
        exec_src
            .matches("interpreter::lambda_accepts_descriptor(")
            .count()
            >= 2
    );
}
