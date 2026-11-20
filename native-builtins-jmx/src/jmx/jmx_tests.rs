use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// zt t7/tc2 (row SM1): the `SoftMaxHeapSize` row is built with JDK 9+'s
/// real `Flag` constructor (the `VMOption.Origin` shape `javap` shows on
/// 25.0.3; the `int` shape does not exist there, which made every
/// `getFlags` answer 0), and its origins are the enum constants HotSpot
/// 25.0.3 prints: `ERGONOMIC` by default, `MANAGEMENT` after
/// `setVMOption`, `VM_CREATION` for the command line.
#[test]
fn zt7tc2_the_soft_max_row_uses_the_origin_enum_constructor() {
    assert_eq!(
        FLAG_INIT_DESC_ORIGIN,
        "(Ljava/lang/String;Ljava/lang/Object;ZZLcom/sun/management/VMOption$Origin;)V"
    );
    assert_ne!(FLAG_INIT_DESC, FLAG_INIT_DESC_ORIGIN);
    assert_eq!(VM_OPTION_ORIGIN_CLASS, "com/sun/management/VMOption$Origin");
    let row = |origin| SoftMaxRow {
        value: 1 << 30,
        max: 1 << 30,
        origin,
    };
    assert_eq!(row(0).origin_name(), "ERGONOMIC");
    assert_eq!(row(1).origin_name(), "VM_CREATION");
    assert_eq!(row(2).origin_name(), "MANAGEMENT");
}

fn registry_with_synthetic_mbean_server() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    register_mbean_server(&mut r);
    r
}

#[test]
fn test_management_factory_registration() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "java/lang/management/ManagementFactory";
    assert!(r.find(cls, "<init>", "()V").is_some());
    // `getPlatformMBeanServer` is intentionally NOT registered (see
    // KAFKA-MBEAN note in `register_management_factory`): the real JDK
    // bytecode constructs a concrete `JmxMBeanServer`, which is what
    // `invokeinterface MBeanServer.registerMBean` requires for correct
    // dispatch.
    assert!(
        r.find(
            cls,
            "getPlatformMBeanServer",
            "()Ljavax/management/MBeanServer;"
        )
        .is_none(),
        "getPlatformMBeanServer must not be a synthetic-stub native"
    );
    assert!(r
        .find(
            cls,
            "getRuntimeMXBean",
            "()Ljava/lang/management/RuntimeMXBean;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "getMemoryMXBean",
            "()Ljava/lang/management/MemoryMXBean;"
        )
        .is_none(), "the real JDK bytecode owns the public singleton path");
}

#[test]
fn test_runtime_mxbean_all_getters_registered() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "java/lang/management/RuntimeMXBean";
    let methods = [
        ("getName", "()Ljava/lang/String;"),
        ("getVmName", "()Ljava/lang/String;"),
        ("getVmVersion", "()Ljava/lang/String;"),
        ("getVmVendor", "()Ljava/lang/String;"),
        ("getSpecName", "()Ljava/lang/String;"),
        ("getSpecVersion", "()Ljava/lang/String;"),
        ("getSpecVendor", "()Ljava/lang/String;"),
        ("getStartTime", "()J"),
        ("getUptime", "()J"),
        ("getInputArguments", "()Ljava/util/List;"),
        ("getClassPath", "()Ljava/lang/String;"),
        ("getBootClassPath", "()Ljava/lang/String;"),
        ("isBootClassPathSupported", "()Z"),
    ];
    for (name, desc) in &methods {
        assert!(
            r.find(cls, name, desc).is_some(),
            "Missing RuntimeMXBean.{}{}",
            name,
            desc
        );
    }
}

#[test]
fn test_memory_mxbean_registration() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "java/lang/management/MemoryMXBean";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(
            cls,
            "getHeapMemoryUsage",
            "()Ljava/lang/management/MemoryUsage;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "getNonHeapMemoryUsage",
            "()Ljava/lang/management/MemoryUsage;"
        )
        .is_some());
    assert!(r
        .find(cls, "getObjectPendingFinalizationCount", "()I")
        .is_some());
    assert!(r.find(cls, "gc", "()V").is_some());
}

#[test]
fn test_memory_usage_registration() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "java/lang/management/MemoryUsage";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r.find(cls, "<init>", "(JJJJ)V").is_some());
    assert!(r.find(cls, "getInit", "()J").is_some());
    assert!(r.find(cls, "getUsed", "()J").is_some());
    assert!(r.find(cls, "getCommitted", "()J").is_some());
    assert!(r.find(cls, "getMax", "()J").is_some());
    assert_eq!(
        r.find(cls, "toString", "()Ljava/lang/String;").is_some(),
        cfg!(feature = "synthetic-jdk")
    );
}

/// The four getters stay native (they serve CratonVM-synthesised
/// `MemoryUsage` receivers); `toString` does not, because the real bytecode
/// reads the very slots those getters read and formats them the way every
/// other JVM does.
#[test]
fn memoryusage_tostring_shim_follows_the_class_library() {
    assert_eq!(memoryusage_tostring_shim_enabled(), cfg!(feature = "synthetic-jdk"));
}

#[test]
fn test_thread_mxbean_registration() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "java/lang/management/ThreadMXBean";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r.find(cls, "getThreadCount", "()I").is_some());
    assert!(r.find(cls, "getPeakThreadCount", "()I").is_some());
    assert!(r.find(cls, "getTotalStartedThreadCount", "()J").is_some());
    assert!(r.find(cls, "getDaemonThreadCount", "()I").is_some());
    assert!(r.find(cls, "getCurrentThreadCpuTime", "()J").is_some());
    assert!(r.find(cls, "getCurrentThreadUserTime", "()J").is_some());
    assert!(r.find(cls, "isThreadCpuTimeSupported", "()Z").is_some());
    assert!(r.find(cls, "isThreadCpuTimeEnabled", "()Z").is_some());
    assert!(r.find(cls, "getAllThreadIds", "()[J").is_some());
    assert!(r
        .find(cls, "getThreadInfo", "(J)Ljava/lang/management/ThreadInfo;")
        .is_some());
    assert!(r.find(cls, "findDeadlockedThreads", "()[J").is_some());
    assert!(r
        .find(cls, "findMonitorDeadlockedThreads", "()[J")
        .is_some());
    assert!(r
        .find(
            cls,
            "dumpAllThreads",
            "(ZZ)[Ljava/lang/management/ThreadInfo;"
        )
        .is_some());
}

#[test]
fn test_class_loading_mxbean_registration() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "java/lang/management/ClassLoadingMXBean";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r.find(cls, "getLoadedClassCount", "()I").is_some());
    assert!(r.find(cls, "getTotalLoadedClassCount", "()J").is_some());
    assert!(r.find(cls, "getUnloadedClassCount", "()J").is_some());
    assert!(r.find(cls, "isVerbose", "()Z").is_some());
    assert!(r.find(cls, "setVerbose", "(Z)V").is_some());
}

/// gci i12/ul (F1): `VMManagementImpl.getTotalClassCount` is the
/// cumulative count, live plus unloaded, so `ClassLoadingImpl`'s
/// `total - unloaded` is the live count and the total never falls.
#[test]
fn gci_i12_ul_total_class_count_is_live_plus_unloaded() {
    assert_eq!(total_loaded_class_count(0, 0), 0);
    assert_eq!(total_loaded_class_count(1200, 0), 1200);
    // 1200 live after 50 of 1250 unloaded: the total stays 1250, and the
    // JDK's derived loaded count is 1250 - 50 = 1200 (it read 1150).
    assert_eq!(total_loaded_class_count(1200, 50), 1250);
    assert_eq!(total_loaded_class_count(1200, 50) - 50, 1200);
    // Monotonic across an unload: 1250 live, then 50 unloaded -> 1200 live.
    assert!(total_loaded_class_count(1200, 50) >= total_loaded_class_count(1250, 0));
    assert_eq!(total_loaded_class_count(usize::MAX, u64::MAX), i64::MAX);
    // The registration answers through the helper, not the live count.
    let src = concat!(include_str!("../jmx/mod.rs"), "\n", include_str!("../jmx/jmx_ioex.rs"), "\n", include_str!("../jmx/register_garbage_collector_impl.rs"), "\n", include_str!("../jmx/alloc_basic_thread_info.rs"));
    let at = src
        .find("\"getTotalClassCount\",")
        .expect("getTotalClassCount registration");
    let window: String = src[at..].chars().take(240).collect();
    assert!(
        window.contains("total_loaded_class_count("),
        "getTotalClassCount must add the unloaded count"
    );
}

#[test]
fn test_operating_system_mxbean_registration() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "java/lang/management/OperatingSystemMXBean";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r.find(cls, "getName", "()Ljava/lang/String;").is_some());
    assert!(r.find(cls, "getArch", "()Ljava/lang/String;").is_some());
    assert!(r.find(cls, "getVersion", "()Ljava/lang/String;").is_some());
    assert!(r.find(cls, "getAvailableProcessors", "()I").is_some());
    assert!(r.find(cls, "getSystemLoadAverage", "()D").is_some());
}

#[test]
fn test_compilation_mxbean_registration() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "java/lang/management/CompilationMXBean";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r.find(cls, "getName", "()Ljava/lang/String;").is_some());
    assert!(r.find(cls, "getTotalCompilationTime", "()J").is_some());
    assert!(r
        .find(cls, "isCompilationTimeMonitoringSupported", "()Z")
        .is_some());
}

#[test]
fn test_gc_mxbean_registration() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "java/lang/management/GarbageCollectorMXBean";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r.find(cls, "getName", "()Ljava/lang/String;").is_some());
    assert!(r.find(cls, "getCollectionCount", "()J").is_some());
    assert!(r.find(cls, "getCollectionTime", "()J").is_some());
    assert!(r
        .find(cls, "getMemoryPoolNames", "()[Ljava/lang/String;")
        .is_some());
    assert!(r.find(cls, "isValid", "()Z").is_some());
}

#[test]
fn test_mbean_server_registration() {
    let r = registry_with_synthetic_mbean_server();
    let cls = "javax/management/MBeanServer";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(cls, "getDefaultDomain", "()Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(cls, "getMBeanCount", "()Ljava/lang/Integer;")
        .is_some());
    assert!(r.find(cls, "getDomains", "()[Ljava/lang/String;").is_some());
    assert!(r
        .find(cls, "isRegistered", "(Ljavax/management/ObjectName;)Z")
        .is_some());
    assert!(r
        .find(
            cls,
            "queryMBeans",
            "(Ljavax/management/ObjectName;Ljavax/management/QueryExp;)Ljava/util/Set;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "getAttribute",
            "(Ljavax/management/ObjectName;Ljava/lang/String;)Ljava/lang/Object;"
        )
        .is_some());
}

#[test]
fn test_mbean_server_flow_methods_registered() {
    // The full in-process JMX flow must expose register / unregister /
    // get / set / invoke / query so a basic round-trip works.
    let mut r = registry_with_synthetic_mbean_server();
    // `MBeanServerFactory.createMBeanServer`/`newMBeanServer` are
    // synthetic-Real-JDK (see `register_mbean_server_factory_synthetic`'s
    // doc for why they must never be called from the real-JDK
    // registration path — that was the actual bug this split fixed) and
    // so aren't part of the shared `registry_with_synthetic_mbean_server`
    // helper; opt in explicitly here since this test asserts on them.
    register_mbean_server_factory_synthetic(&mut r);
    let cls = "javax/management/MBeanServer";
    let methods = [
        ("registerMBean", "(Ljava/lang/Object;Ljavax/management/ObjectName;)Ljavax/management/ObjectInstance;"),
        ("unregisterMBean", "(Ljavax/management/ObjectName;)V"),
        ("getObjectInstance", "(Ljavax/management/ObjectName;)Ljavax/management/ObjectInstance;"),
        ("setAttribute", "(Ljavax/management/ObjectName;Ljavax/management/Attribute;)V"),
        ("invoke", "(Ljavax/management/ObjectName;Ljava/lang/String;[Ljava/lang/Object;[Ljava/lang/String;)Ljava/lang/Object;"),
        ("queryNames", "(Ljavax/management/ObjectName;Ljavax/management/QueryExp;)Ljava/util/Set;"),
        ("getMBeanInfo", "(Ljavax/management/ObjectName;)Ljavax/management/MBeanInfo;"),
    ];
    for (name, desc) in &methods {
        assert!(
            r.find(cls, name, desc).is_some(),
            "Missing MBeanServer.{}{}",
            name,
            desc
        );
    }
    // MBeanServerFactory must produce a server for getPlatformMBeanServer.
    assert!(r
        .find(
            "javax/management/MBeanServerFactory",
            "createMBeanServer",
            "()Ljavax/management/MBeanServer;"
        )
        .is_some());
}

#[test]
fn test_capitalize() {
    assert_eq!(capitalize("name"), "Name");
    assert_eq!(capitalize("X"), "X");
    assert_eq!(capitalize(""), "");
    assert_eq!(capitalize("alreadyCap"), "AlreadyCap");
}

#[test]
fn test_mbean_server_register_query_roundtrip() {
    // Build a server, register a bean under an ObjectName whose key
    // resolves via read_string (the mock returns None from
    // invoke_virtual, so object_name_key falls back to reading the
    // name object as a String). Verify isRegistered + count + lookup.
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    let server = alloc_mbean_server(&mut ctx).unwrap();

    // Name object: a mock String holding the canonical-name text.
    let name = ctx.create_string("com.acme:type=Widget");
    // Bean object: any allocated object.
    let bean = try_alloc_concurrent_synthetic(&mut ctx, "com/acme/Widget", 2).unwrap();

    // Not registered yet.
    assert!(mbs_find(&ctx, server, "com.acme:type=Widget").is_none());

    // Simulate registerMBean's registry-growth logic directly.
    let key = object_name_key(&mut ctx, Some(name));
    assert_eq!(key, "com.acme:type=Widget");

    // Grow registry by one (mirrors the native body).
    let (names_opt, beans_opt) = mbs_registry(&ctx, server);
    let old_len = names_opt.map(|n| ctx.array_length(n)).unwrap_or(0);
    let new_names = ctx.new_ref_array(ClassId::new(0), old_len + 1);
    let new_beans = ctx.new_ref_array(ClassId::new(0), old_len + 1);
    let key_str = ctx.create_string(&key);
    ctx.set_array_element(new_names, old_len, Value::Object(Some(key_str)));
    ctx.set_array_element(new_beans, old_len, Value::Object(Some(bean)));
    ctx.set_field(server, MBS_NAMES, Value::Object(Some(new_names)));
    ctx.set_field(server, MBS_BEANS, Value::Object(Some(new_beans)));
    let _ = beans_opt;

    // Now it's found, and the bean lookup returns our bean.
    assert_eq!(mbs_find(&ctx, server, "com.acme:type=Widget"), Some(0));
    assert_eq!(
        mbs_lookup_bean(&ctx, server, "com.acme:type=Widget"),
        Some(bean)
    );
    // An unregistered key is not found.
    assert!(mbs_lookup_bean(&ctx, server, "com.acme:type=Other").is_none());
}

#[test]
fn test_mbean_server_domains_follow_live_registry() {
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    let server = alloc_mbean_server(&mut ctx).unwrap();
    let names = ctx.new_ref_array(ClassId::new(0), 4);
    for (i, name) in [
        "org.springframework.integration:type=MessageChannel",
        "org.springframework.boot.integration.autoconfigure:type=Configurer",
        "org.springframework.integration:type=MessageHandler",
        "type=UsesDefaultDomain",
    ]
    .iter()
    .enumerate()
    {
        let name = ctx.create_string(name);
        ctx.set_array_element(names, i, Value::Object(Some(name)));
    }
    ctx.set_field(server, MBS_NAMES, Value::Object(Some(names)));

    assert_eq!(
        mbs_domains(&mut ctx, server),
        vec![
            "org.springframework.integration",
            "org.springframework.boot.integration.autoconfigure",
            "DefaultDomain",
        ]
    );
}

#[test]
fn test_object_name_key_falls_back_to_string() {
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    let name = ctx.create_string("java.lang:type=Memory");
    // invoke_virtual on the mock returns None, so object_name_key
    // falls through to read_string of the name object.
    assert_eq!(
        object_name_key(&mut ctx, Some(name)),
        "java.lang:type=Memory"
    );
    // Null name -> empty key, no panic.
    assert_eq!(object_name_key(&mut ctx, None), "");
}

#[test]
fn object_name_canonicalization_makes_key_order_semantic() {
    let registered = "cratonvm.lazy:name=dataSource,type=HikariDataSource";
    let queried = "cratonvm.lazy:type=HikariDataSource,name=dataSource";
    assert_eq!(
        canonical_object_name_text(registered),
        "cratonvm.lazy:name=dataSource,type=HikariDataSource"
    );
    assert_eq!(
        canonical_object_name_text(registered),
        canonical_object_name_text(queried)
    );
}

#[test]
fn object_name_canonicalization_preserves_quoted_commas_and_patterns() {
    assert_eq!(
        canonical_object_name_text("example:type=Cache,name=\"a,b\",*"),
        "example:name=\"a,b\",type=Cache,*"
    );
}

#[test]
fn test_object_name_new_natives_are_registered_bridge() {
    // RKC-ObjectName-01/02 regression: all accessor members that read
    // real ObjectName cache fields must be registered.  Otherwise they
    // fall through to real bytecode and NPE on the synthetic one-field
    // ObjectName's never-populated `_ca_array`/`_kp_array`.
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "javax/management/ObjectName";
    assert!(r
        .find(cls, "_getKeyPropertyList", "()Ljava/util/Map;")
        .is_some());
    assert!(r
        .find(cls, "getKeyPropertyList", "()Ljava/util/Hashtable;")
        .is_some());
    assert!(r
        .find(cls, "getKeyPropertyListString", "()Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(
            cls,
            "getCanonicalKeyPropertyListString",
            "()Ljava/lang/String;"
        )
        .is_some());
    assert!(r.find(cls, "isPattern", "()Z").is_some());
    assert!(r.find(cls, "isDomainPattern", "()Z").is_some());
    assert!(r.find(cls, "isPropertyPattern", "()Z").is_some());
    assert!(r.find(cls, "isPropertyListPattern", "()Z").is_some());
}

#[test]
fn test_object_name_get_canonical_key_property_list_string() {
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    let name = object_name_new(
        &mut ctx,
        "JMImplementation:type=MBeanServerDelegate".to_string(),
    )
    .unwrap();
    let result = native_object_name_get_canonical_key_property_list_string(
        &mut ctx,
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    let s = match result {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap(),
        other => panic!("expected a String, got {other:?}"),
    };
    assert_eq!(s, "type=MBeanServerDelegate");

    // Domain-only pattern ("d:*") has no key properties.
    let pattern_name = object_name_new(&mut ctx, "java.lang:*".to_string()).unwrap();
    let result = native_object_name_get_canonical_key_property_list_string(
        &mut ctx,
        &[Value::Object(Some(pattern_name))],
    )
    .unwrap()
    .unwrap();
    let s = match result {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap(),
        other => panic!("expected a String, got {other:?}"),
    };
    assert_eq!(s, "");

    // Property-list pattern ("d:k=v,*") strips the trailing ",*".
    let plist_pattern = object_name_new(&mut ctx, "d:k=v,*".to_string()).unwrap();
    let result = native_object_name_get_canonical_key_property_list_string(
        &mut ctx,
        &[Value::Object(Some(plist_pattern))],
    )
    .unwrap()
    .unwrap();
    let s = match result {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap(),
        other => panic!("expected a String, got {other:?}"),
    };
    assert_eq!(s, "k=v");    }

#[test]
fn test_object_name_key_property_accessors_preserve_quoted_source_text() {
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    let name = object_name_new(&mut ctx, "d:b=2,a=\"x,y\",c=3,*".to_string()).unwrap();
    let key = ctx.create_string("a");

    let result = native_object_name_get_key_property(
        &mut ctx,
        &[Value::Object(Some(name)), Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();
    let property = match result {
        Value::Object(Some(value)) => ctx.read_string(value).unwrap(),
        other => panic!("expected a String, got {other:?}"),
    };
    assert_eq!(property, "\"x,y\"");

    let result =
        native_object_name_get_key_property_list_string(&mut ctx, &[Value::Object(Some(name))])
            .unwrap()
            .unwrap();
    let list = match result {
        Value::Object(Some(value)) => ctx.read_string(value).unwrap(),
        other => panic!("expected a String, got {other:?}"),
    };
    assert_eq!(list, "b=2,a=\"x,y\",c=3");

    assert_eq!(
        object_name_parts(&object_name_text(&ctx, name)).1,
        vec![
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "\"x,y\"".to_string()),
            ("c".to_string(), "3".to_string()),
        ]
    );
}

#[test]
fn object_name_requires_a_property_list() {
    assert!(!object_name_has_required_structure(
        "integrationMbeanExporter"
    ));
    assert!(!object_name_has_required_structure("domain:"));
    assert!(object_name_has_required_structure(
        "domain:type=Exporter,name=bean"
    ));
    assert!(object_name_has_required_structure("domain:*"));
}

#[test]
fn test_object_name_is_pattern_family() {
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();

    let concrete = object_name_new(
        &mut ctx,
        "JMImplementation:type=MBeanServerDelegate".to_string(),
    )
    .unwrap();
    let is_pattern = |ctx: &mut dyn NativeContext,
                      f: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
                      obj: ObjectRef| {
        matches!(
            f(ctx, &[Value::Object(Some(obj))]).unwrap().unwrap(),
            Value::Int(1)
        )
    };
    assert!(!is_pattern(
        &mut ctx,
        native_object_name_is_pattern,
        concrete
    ));
    assert!(!is_pattern(
        &mut ctx,
        native_object_name_is_domain_pattern,
        concrete
    ));
    assert!(!is_pattern(
        &mut ctx,
        native_object_name_is_property_pattern,
        concrete
    ));
    assert!(!is_pattern(
        &mut ctx,
        native_object_name_is_property_list_pattern,
        concrete
    ));

    let domain_pattern = object_name_new(&mut ctx, "java.*:type=Memory".to_string()).unwrap();
    assert!(is_pattern(
        &mut ctx,
        native_object_name_is_pattern,
        domain_pattern
    ));
    assert!(is_pattern(
        &mut ctx,
        native_object_name_is_domain_pattern,
        domain_pattern
    ));
    assert!(!is_pattern(
        &mut ctx,
        native_object_name_is_property_list_pattern,
        domain_pattern
    ));

    let plist_pattern = object_name_new(&mut ctx, "d:k=v,*".to_string()).unwrap();
    assert!(is_pattern(
        &mut ctx,
        native_object_name_is_pattern,
        plist_pattern
    ));
    assert!(is_pattern(
        &mut ctx,
        native_object_name_is_property_pattern,
        plist_pattern
    ));
    assert!(is_pattern(
        &mut ctx,
        native_object_name_is_property_list_pattern,
        plist_pattern
    ));
    assert!(!is_pattern(
        &mut ctx,
        native_object_name_is_domain_pattern,
        plist_pattern
    ));

    let value_pattern = object_name_new(&mut ctx, "d:k=*".to_string()).unwrap();
    assert!(is_pattern(
        &mut ctx,
        native_object_name_is_pattern,
        value_pattern
    ));
    assert!(is_pattern(
        &mut ctx,
        native_object_name_is_property_pattern,
        value_pattern
    ));
    assert!(!is_pattern(
        &mut ctx,
        native_object_name_is_property_list_pattern,
        value_pattern
    ));
}

/// nio21 wave 3, lane L4: a context that cannot walk its heap (the trait
/// default) yields no census, so every snapshot keeps its own list; and a
/// census answers each thread its OWN rows, in heap order, through the
/// pins.
#[test]
fn nio21_census_rows_are_per_owner_in_heap_order() {
    let mut ctx = cratonvm_native_builtins::test_utils::MockNativeContext::new();
    assert!(synchronizer_census_rows(&mut ctx).is_none());
    assert!(synchronizer_census(&mut ctx).is_none());
    let a = ctx.alloc_object(ClassId::new(1), 0);
    let b = ctx.alloc_object(ClassId::new(1), 0);
    let c = ctx.alloc_object(ClassId::new(1), 0);
    let rows = vec![
        (5, ctx.pin_native_root(a), a),
        (6, ctx.pin_native_root(b), b),
        (5, ctx.pin_native_root(c), c),
    ];
    let five = census_synchronizers_of(&ctx, &rows, 5);
    assert_eq!(
        five.iter().map(|o| o.as_ptr()).collect::<Vec<_>>(),
        vec![a.as_ptr(), c.as_ptr()]
    );
    assert_eq!(census_synchronizers_of(&ctx, &rows, 6).len(), 1);
    assert!(census_synchronizers_of(&ctx, &rows, 7).is_empty());
}

#[test]
fn test_vm_start_time_initialization() {
    // Access the start time — should not panic and should be reasonable.
    // The mock reports no VM start instant, so this reads the
    // process-wide fallback origin.
    let ctx = cratonvm_native_builtins::test_utils::MockNativeContext::new();
    let ms = vm_start_epoch_ms(&ctx);
    assert!(ms > 0, "VM start epoch millis should be positive");
    let up = uptime_ms(&ctx);
    // Bounded by the test process's lifetime, not by this test's: since
    // gc-common w8-f the origin is latched when ANY test registers the
    // management natives (`register_vm_management_impl`), which in a full
    // `--lib` run can be minutes before this test is scheduled.
    assert!(up < 3_600_000, "Uptime should be < 1h in a test process");
}

#[test]
fn test_management_factory_returns_all_mxbeans() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let cls = "java/lang/management/ManagementFactory";
    // `getPlatformMBeanServer` is intentionally NOT in this list — the
    // real JDK bytecode supplies a concrete `JmxMBeanServer`, and a
    // synthetic-stub native here would break interface dispatch on the
    // returned receiver (see KAFKA-MBEAN note above).
    let factory_methods = [
        "getRuntimeMXBean",
        "getThreadMXBean",
        "getClassLoadingMXBean",
        "getOperatingSystemMXBean",
        "getCompilationMXBean",
        "getGarbageCollectorMXBeans",
    ];
    for method in &factory_methods {
        // Just check the method name is registered (any descriptor)
        // We already checked specific descriptors above; this ensures
        // all seven factory methods exist.
        let found = r.find(
            cls,
            method,
            match *method {
                "getRuntimeMXBean" => "()Ljava/lang/management/RuntimeMXBean;",
                "getThreadMXBean" => "()Ljava/lang/management/ThreadMXBean;",
                "getClassLoadingMXBean" => "()Ljava/lang/management/ClassLoadingMXBean;",
                "getOperatingSystemMXBean" => "()Ljava/lang/management/OperatingSystemMXBean;",
                "getCompilationMXBean" => "()Ljava/lang/management/CompilationMXBean;",
                "getGarbageCollectorMXBeans" => "()Ljava/util/List;",
                _ => unreachable!(),
            },
        );
        assert!(found.is_some(), "Missing factory method: {}", method);
    }
}

#[test]
fn test_total_registered_jmx_method_count() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_natives(&mut r);
    let count = r.len();
    assert!(
        count > 50,
        "Expected > 50 JMX methods registered, got {}",
        count
    );
}

#[test]
fn test_all_mxbean_classes_have_init() {
    let r = registry_with_synthetic_mbean_server();
    let classes = [
        "java/lang/management/ManagementFactory",
        "java/lang/management/RuntimeMXBean",
        "java/lang/management/MemoryMXBean",
        "java/lang/management/MemoryUsage",
        "java/lang/management/ThreadMXBean",
        "java/lang/management/ClassLoadingMXBean",
        "java/lang/management/OperatingSystemMXBean",
        "java/lang/management/CompilationMXBean",
        "java/lang/management/GarbageCollectorMXBean",
        "javax/management/MBeanServer",
    ];
    for cls in &classes {
        assert!(
            r.find(cls, "<init>", "()V").is_some(),
            "Missing <init> for {}",
            cls
        );
    }
}

#[test]
fn test_vm_management_impl_int_typed_thread_counters() {
    // RKC16N.12 regression: live/peak/daemon thread counts are `int`
    // in JDK 25's VMManagementImpl, not `long`. The earlier batch
    // registered them as `()J`, which caused the dispatcher to never
    // match the call site and the JVM to raise UnsatisfiedLinkError
    // during ManagementFactoryHelper.<clinit> -> new VMManagementImpl()
    // on Keycloak boot. Pin the correct descriptors here so the
    // mismatch can't silently regress.
    let mut r = NativeMethodRegistry::new();
    register_vm_management_impl(&mut r);
    let cls = "sun/management/VMManagementImpl";
    for name in [
        "getLiveThreadCount",
        "getPeakThreadCount",
        "getDaemonThreadCount",
    ] {
        assert!(
            r.find(cls, name, "()I").is_some(),
            "VMManagementImpl.{} should be registered with `()I` (int), \
             not `()J` (long) — see JDK 25 sun/management/VMManagementImpl.java",
            name
        );
        assert!(
            r.find(cls, name, "()J").is_none(),
            "VMManagementImpl.{} must NOT be registered with `()J`; \
             the JDK declares it `int` and the dispatcher matches by \
             full descriptor.",
            name
        );
    }
}

#[test]
fn test_vm_management_impl_uptime_and_processors() {
    // RKC16N.12: VMManagementImpl declares native getUptime0()J and
    // getAvailableProcessors()I. Both are reachable from
    // RuntimeImpl during JMM bootstrap; missing either surfaces as
    // an UnsatisfiedLinkError during ManagementFactory class init.
    let mut r = NativeMethodRegistry::new();
    register_vm_management_impl(&mut r);
    let cls = "sun/management/VMManagementImpl";
    assert!(
        r.find(cls, "getUptime0", "()J").is_some(),
        "VMManagementImpl.getUptime0 missing"
    );
    assert!(
        r.find(cls, "getAvailableProcessors", "()I").is_some(),
        "VMManagementImpl.getAvailableProcessors missing"
    );
}

#[test]
fn test_jdk25_internal_flag_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    register_flag_impl(&mut r);
    let cls = "com/sun/management/internal/Flag";
    for (name, desc) in [
        ("initialize", "()V"),
        ("getInternalFlagCount", "()I"),
        ("getAllFlagNames", "()[Ljava/lang/String;"),
        (
            "getFlags",
            "([Ljava/lang/String;[Lcom/sun/management/internal/Flag;I)I",
        ),
        ("setLongValue", "(Ljava/lang/String;J)V"),
        ("setDoubleValue", "(Ljava/lang/String;D)V"),
        ("setBooleanValue", "(Ljava/lang/String;Z)V"),
        ("setStringValue", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ] {
        assert!(
            r.find(cls, name, desc).is_some(),
            "missing {cls}.{name}{desc}"
        );
    }
}

#[test]
fn test_management_factory_load_native_lib_chain() {
    // Session 98: ManagementFactory.<clinit> -> loadNativeLib() ->
    // System.loadLibrary("management") -> Runtime.loadLibrary0(...) ->
    // ClassLoader.loadLibrary throws UnsatisfiedLinkError because
    // libmanagement.dll is genuinely absent (we ship JMM natives
    // in-process via NativeMethodRegistry).
    //
    // INVERTED (2026-07-28 stub sweep). This test used to also require
    // `System.loadLibrary` / `System.load` / `Runtime.loadLibrary0` /
    // `Runtime.load0` to be registered HERE, on the premise that
    // "register_runtime_natives is synthetic-mode-only". That premise is
    // false: `register_essential_natives_with_shims` calls
    // `lang_system::register_runtime_natives` unconditionally, and vm_init
    // runs it BEFORE `register_vm_management_impl` in both branches — so
    // the no-ops registered here silently replaced `lang_system`'s REAL
    // library loaders (last-registration-wins) and broke
    // `System.loadLibrary` process-wide. They are gone; the narrow
    // `loadNativeLib` no-op is the whole fix. Assert BOTH halves so the
    // shadowing cannot be reintroduced.
    let mut r = NativeMethodRegistry::new();
    register_vm_management_impl(&mut r);
    assert!(
        r.find(
            "java/lang/management/ManagementFactory",
            "loadNativeLib",
            "()V"
        )
        .is_some(),
        "ManagementFactory.loadNativeLib must be a no-op native; \
         without it, ManagementFactory.<clinit> calls into the \
         real-JDK loadLibrary chain which throws UnsatisfiedLinkError."
    );
    for (cls, name, desc) in [
        ("java/lang/System", "loadLibrary", "(Ljava/lang/String;)V"),
        ("java/lang/System", "load", "(Ljava/lang/String;)V"),
        (
            "java/lang/Runtime",
            "loadLibrary0",
            "(Ljava/lang/Class;Ljava/lang/String;)V",
        ),
        (
            "java/lang/Runtime",
            "load0",
            "(Ljava/lang/Class;Ljava/lang/String;)V",
        ),
    ] {
        assert!(
            r.find(cls, name, desc).is_none(),
            "{cls}.{name}{desc} must NOT be registered by \
             register_vm_management_impl: it runs AFTER \
             lang_system::register_runtime_natives in both run modes, so a \
             registration here shadows the real library loader and makes \
             System.loadLibrary a no-op for the whole VM."
        );
    }
}

#[test]
fn test_platform_field_counts_match() {
    // Verify our alloc functions use the expected field counts by
    // counting them in the registration functions. We simply check
    // that the constants embedded in alloc_* match expectations.
    // RuntimeMXBean = 10 fields
    // MemoryMXBean = 6 fields
    // MemoryUsage = 4 fields
    // ThreadMXBean = 6 fields
    // ClassLoadingMXBean = 4 fields (3 counters + the verbose flag)
    // OperatingSystemMXBean = 5 fields
    // CompilationMXBean = 3 fields
    // GarbageCollectorMXBean = 4 fields
    // MBeanServer = 2 fields
    //
    // We can verify this by looking at the alloc calls; since we
    // control the code, we just assert the expected counts are used
    // by calling the alloc functions via the factory and checking
    // the registry has the right init methods.
    let r = registry_with_synthetic_mbean_server();

    // All 10 classes should have <init>
    let expected_classes = 10;
    let classes = [
        "java/lang/management/ManagementFactory",
        "java/lang/management/RuntimeMXBean",
        "java/lang/management/MemoryMXBean",
        "java/lang/management/MemoryUsage",
        "java/lang/management/ThreadMXBean",
        "java/lang/management/ClassLoadingMXBean",
        "java/lang/management/OperatingSystemMXBean",
        "java/lang/management/CompilationMXBean",
        "java/lang/management/GarbageCollectorMXBean",
        "javax/management/MBeanServer",
    ];
    assert_eq!(classes.len(), expected_classes);
    for cls in &classes {
        assert!(
            r.find(cls, "<init>", "()V").is_some(),
            "Expected <init> for {} but not found",
            cls
        );
    }
}

/// gc-common w3-f: the non-heap `used` is the VM's measured class-metadata
/// figure when it has one — NOT `classes × 4 KiB` — and the class-count
/// estimate only for a context with no class store. Always positive.
#[test]
fn w3f_non_heap_used_prefers_the_measured_class_metadata() {
    assert_eq!(non_heap_used_bytes(Some(123_457), 10), 123_457);
    assert_eq!(non_heap_used_bytes(None, 10), 10 * AVG_CLASS_METADATA_BYTES);
    assert_eq!(non_heap_used_bytes(Some(0), 500), 1);
    assert_eq!(non_heap_used_bytes(None, 0), 1);
    assert_eq!(non_heap_used_bytes(Some(u64::MAX), 0), i64::MAX);
    assert_eq!(non_heap_used_bytes(None, usize::MAX), i64::MAX);
}

/// gc-common w3-f: both `MemoryUsage` derivations are well-formed pairs —
/// `0 <= used <= committed`, and `committed <= max` where `max` is
/// defined — which is what `MemoryUsage`'s own constructor checks. The
/// synthetic `getNonHeapMemoryUsage` used to answer `committed = -1`.
#[test]
fn w3f_memory_usage_quadruples_are_well_formed() {
    let ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    let [init, used, committed, max] = heap_memory_usage(&ctx);
    assert!(init > 0);
    assert!(
        used >= 0 && used <= committed,
        "used={used} committed={committed}"
    );
    assert!(committed <= max, "committed={committed} max={max}");
    let [init, used, committed, max] = non_heap_memory_usage(&ctx);
    assert_eq!(init, NON_HEAP_INIT_BYTES);
    assert!(used > 0);
    assert_eq!(committed, used);
    assert_eq!(max, -1);
}
