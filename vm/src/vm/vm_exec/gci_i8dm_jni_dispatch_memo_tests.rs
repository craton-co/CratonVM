use super::*;

fn row(vm: usize, cid: u32, m: &str, d: &str, generation: u64, redefinitions: u64) -> JniDispatchRow {
    JniDispatchRow {
        vm_identity: vm,
        class_id: cid,
        method_name: Box::from(m),
        descriptor: Box::from(d),
        generation,
        redefinitions,
        hit: JniDispatchHit {
            fn_ptr: 0x1000,
            declaring_class_id: ClassId::new(cid),
            is_static: true,
            is_synchronized: false,
            class_name: std::sync::Arc::from("Probe"),
            selected: None,
        },
    }
}

/// A row serves only its own key, and only while the JNI table's
/// generation and the redefinition count are the fill's.
#[test]
fn gci_i8dm_memo_serves_only_its_key_and_stamps() {
    let mut memo = NameOverrideMemo::default();
    jni_dispatch_memo_store(&mut memo, row(7, 0x4_0001, "noop", "(I)I", 11, 2));
    let serve = |memo: &NameOverrideMemo, vm, cid, m: &str, d: &str, g: u64, r: u64| {
        jni_dispatch_memo_serve(memo, vm, cid, m, d, || g, || r).map(|h| h.fn_ptr)
    };
    assert_eq!(serve(&memo, 7, 0x4_0001, "noop", "(I)I", 11, 2), Some(0x1000));
    assert_eq!(serve(&memo, 7, 0x4_0001, "noop", "(I)I", 12, 2), None, "re-registered");
    assert_eq!(serve(&memo, 7, 0x4_0001, "noop", "(I)I", 11, 3), None, "redefined");
    assert_eq!(serve(&memo, 8, 0x4_0001, "noop", "(I)I", 11, 2), None, "other VM");
    assert_eq!(serve(&memo, 7, 0x4_0002, "noop", "(I)I", 11, 2), None, "other class");
    assert_eq!(serve(&memo, 7, 0x4_0001, "noop", "(J)I", 11, 2), None, "other descriptor");
    assert_eq!(serve(&memo, 7, 0x4_0001, "noop2", "(I)I", 11, 2), None, "other name");
}

/// An empty table answers nothing, and an index stays in range for
/// degenerate names.
#[test]
fn gci_i8dm_memo_empty_table_and_index_range() {
    let memo = NameOverrideMemo::default();
    assert!(jni_dispatch_memo_find(&memo, 1, 1, "a", "()V").is_none());
    for (m, d) in [("", ""), ("a", ""), ("", "()V")] {
        assert!(jni_dispatch_memo_index(u32::MAX, m, d) < JNI_DISPATCH_MEMO_ROWS);
    }
}

/// The prefix's argument- or latch-dependent doors are never memoised.
#[test]
fn gci_i8dm_memo_refuses_the_prefix_families() {
    assert!(jni_dispatch_memo_admits_names("Gcd1JniCostProbe", "noop"));
    assert!(jni_dispatch_memo_admits_names("com/sun/jna/Native", "invokeInt"));
    for (c, m) in [
        ("Gcd1JniCostProbe", "toString"),
        ("Gcd1JniCostProbe", "accept"),
        ("Gcd1JniCostProbe", "createServerSocket"),
        ("Gcd1JniCostProbe", "<init>"),
        ("", "noop"),
        ("java/lang/Thread", "currentThread"),
        ("java/lang/invoke/MethodHandle", "invokeExact"),
        ("javax/net/ssl/SSLContext", "getInstance"),
        ("sun/security/ssl/SSLSocketImpl", "close"),
        ("cratonvm/internal/UnmodifiableList", "size"),
    ] {
        assert!(!jni_dispatch_memo_admits_names(c, m), "{c}.{m}");
    }
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    crate::native::builtins::app_shims::register_app_workarounds(&mut registry);
    for class in [
        "org/apache/tomcat/jni/SSL",
        "io/netty/internal/tcnative/SSL",
    ] {
        assert!(registry
            .has_app_workaround_prefix(cratonvm_native_api::AppWorkaroundKind::SkipJniMemo, class));
    }
}

/// Source pins: the hit is asked before the by-name prefix's first arm,
/// the fill sits in the table-bound JNI arm after `find_jni_native`, and
/// both the hit and the full path run the one arm body.
#[test]
fn gci_i8dm_hit_precedes_the_prefix_and_both_paths_share_the_arm() {
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
        .find("\npub(in crate::vm::vm_exec) fn invoke_on_class_shared_inner(")
        .expect("the door");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    let hit = body.find("jni_dispatch_memo_hit(").expect("hit");
    let lambda = body.find("is_lambda_proxy_class(recv_cid)").expect("lambda arm");
    assert!(hit < lambda);
    let find = body.find("crate::native::jni::find_jni_native(").expect("table lookup");
    let fill = body.find("jni_dispatch_memo_fill(").expect("fill");
    assert!(find < fill);
    assert_eq!(body.matches("jni_table_native_call(").count(), 2);
    assert!(!body.contains("JniImplicitFrameGuard::enter();\n            jni_census"));
}
