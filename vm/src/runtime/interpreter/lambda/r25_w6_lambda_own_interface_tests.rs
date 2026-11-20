use super::{lambda_arg_cast_verdict, ClassId, SharedVm};
use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
use std::sync::Arc;

#[test]
fn an_argument_of_the_proxys_own_interface_name_passes_in_both_forms() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Ok(object) = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Object")
    else {
        return; // Stripped VM (no class library on the test classpath).
    };
    let proxy: ClassId = shared.alloc_lambda_proxy_id();
    shared.classes.lambda_proxies.write().insert(
        proxy,
        Arc::new(LambdaCallSite {
            functional_interface: Arc::from("java/lang/Runnable"),
            // Resolved, but not to the class a global lookup of the name finds.
            functional_interface_id: Some(object),
            sam_method_name: Arc::from("run"),
            sam_descriptor: Arc::from("()V"),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeStatic,
                class_name: Arc::from("cratonvm/test/W6RFix2Host"),
                member_name: Arc::from("lambda$0"),
                descriptor: Arc::from("()V"),
            },
            instantiated_descriptor: Arc::from("()V"),
            capture_types: vec![],
            proxy_class_id: proxy,
            serializable_flag: false,
        }),
    );
    let obj = shared.mem.heap.alloc_object(proxy, 0);
    for may_load in [false, true] {
        assert_eq!(
            lambda_arg_cast_verdict(&shared, &|| obj, "Ljava/lang/Runnable;", may_load),
            Some(false),
            "may_load={may_load}: the proxy's own interface name is not a refusal"
        );
    }
}

#[test]
fn the_bootstrap_resolves_the_functional_interface_through_the_host() {
    let src = concat!(
        include_str!("../../invokedynamic/mod.rs"),
        "\n",
        include_str!("../../invokedynamic/dbg_indy_all.rs"),
        "\n",
        include_str!("../../invokedynamic/execute_invokedynamic_at_uncensused.rs"),
        "\n",
        include_str!("../../invokedynamic/object_methods_getter_refusal.rs"),
        "\n",
        include_str!("../../invokedynamic/descriptor_param_count_and_last_is.rs")
    );
    let start = src.find("fn bootstrap_lambda(").unwrap_or(0);
    let rest = &src[start..];
    let end = rest.find("let alt_flags: i32").unwrap_or(0);
    assert!(start > 0 && end > 0, "bootstrap_lambda moved");
    let body = &rest[..end];
    let resolve = body.find("resolve_class_loader_aware(");
    let lookup = body.find("get_loaded_class_id_for_requester(&functional_interface");
    assert!(
        matches!((resolve, lookup), (Some(r), Some(l)) if r < l),
        "resolve first, the loaded-class lookup only as the fallback"
    );
}

#[test]
fn superinterface_cast_of_lambda_proxy_resolves_target_in_host_loader() {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use crate::JvmThread;
    use cratonvm_classloading::ClassOrigin;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_types::Value;

    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(crate::ThreadId(1), "test");
    let l1 = ClassLoaderId::UserDefined(101);
    let l2 = ClassLoaderId::UserDefined(102);

    let define_test_class = |name: &str,
                             loader: ClassLoaderId,
                             flags: ClassAccessFlags,
                             ifaces: Vec<ClassId>|
     -> ClassId {
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: loader,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: flags,
            superclass: None,
            interfaces: ifaces,
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        cm.register_class_name(loader, name, id);
        id
    };

    let iface_flags =
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;
    let wp_name = "cratonvm/test/QuteWithPriority";
    let tl_name = "cratonvm/test/QuteTemplateLocator";

    // L2 loads WithPriority first, then L1 overwrites global get_loaded_class_id(wp_name) with wp_1.
    let wp_2 = define_test_class(wp_name, l2, iface_flags, vec![]);
    let _wp_1 = define_test_class(wp_name, l1, iface_flags, vec![]);
    let tl_2 = define_test_class(tl_name, l2, iface_flags, vec![wp_2]);
    let host_1 = define_test_class("cratonvm/test/Host1", l1, ClassAccessFlags::PUBLIC, vec![]);
    let host_2 = define_test_class("cratonvm/test/Host2", l2, ClassAccessFlags::PUBLIC, vec![]);

    let proxy_arg = shared.alloc_lambda_proxy_id();
    let proxy_in_l2 = shared.alloc_lambda_proxy_id();
    let proxy_in_l1 = shared.alloc_lambda_proxy_id();
    {
        let mut proxies = shared.classes.lambda_proxies.write();
        proxies.insert(
            proxy_arg,
            Arc::new(LambdaCallSite {
                functional_interface: Arc::from(tl_name),
                functional_interface_id: Some(tl_2),
                sam_method_name: Arc::from("locate"),
                sam_descriptor: Arc::from("(Ljava/lang/String;)Ljava/lang/Object;"),
                impl_handle: MethodHandle {
                    kind: MethodHandleKind::InvokeStatic,
                    class_name: Arc::from("cratonvm/test/Host2"),
                    member_name: Arc::from("lambda$locate"),
                    descriptor: Arc::from("(Ljava/lang/String;)Ljava/lang/Object;"),
                },
                instantiated_descriptor: Arc::from("(Ljava/lang/String;)Ljava/lang/Object;"),
                capture_types: vec![],
                proxy_class_id: proxy_arg,
                serializable_flag: false,
            }),
        );
    }
    let make_sorter = |proxy_id: ClassId| LambdaCallSite {
        functional_interface: Arc::from("java/util/function/ToIntFunction"),
        functional_interface_id: None,
        sam_method_name: Arc::from("applyAsInt"),
        sam_descriptor: Arc::from("(Ljava/lang/Object;)I"),
        impl_handle: MethodHandle {
            kind: MethodHandleKind::InvokeInterface,
            class_name: Arc::from(wp_name),
            member_name: Arc::from("getPriority"),
            descriptor: Arc::from("()I"),
        },
        instantiated_descriptor: Arc::from("(Lcratonvm/test/QuteWithPriority;)I"),
        capture_types: vec![],
        proxy_class_id: proxy_id,
        serializable_flag: false,
    };
    let site_l2 = make_sorter(proxy_in_l2);
    let site_l1 = make_sorter(proxy_in_l1);
    {
        let mut hosts = shared.classes.lambda_proxy_hosts.write();
        hosts.insert(proxy_in_l2, host_2);
        hosts.insert(proxy_in_l1, host_1);
    }

    let obj = shared.mem.heap.alloc_object(proxy_arg, 0);
    let mut args_ok = vec![Value::Object(Some(obj))];
    let res_ok = super::coerce_lambda_args_at_site(
        &shared,
        &mut thread,
        &site_l2,
        "(Ljava/lang/Object;)I",
        &mut args_ok,
        false,
        0,
    );
    assert!(
        res_ok.is_ok(),
        "same-loader superinterface cast must pass: {res_ok:?}"
    );

    let mut args_bad = vec![Value::Object(Some(obj))];
    let res_bad = super::coerce_lambda_args_at_site(
        &shared,
        &mut thread,
        &site_l1,
        "(Ljava/lang/Object;)I",
        &mut args_bad,
        false,
        0,
    );
    assert!(
        res_bad.is_ok(),
        "proxy functional interface loader owns target superinterface: {res_bad:?}"
    );
}
