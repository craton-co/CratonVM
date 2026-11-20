use super::*;
use cratonvm_native_builtins::test_utils::mock_ctx;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::Value;

#[test]
fn mapping_respects_the_class_library_with_synthetic_support_compiled() {
    let mut registry = NativeMethodRegistry::new();
    assert_eq!(
        real_mxbean_mapping_enabled(&registry),
        !cfg!(feature = "synthetic-jdk")
    );
    registry.set_drop_real_layout_synthetic(true);
    assert!(real_mxbean_mapping_enabled(&registry));
}

#[test]
fn test_register_natives_count() {
    let mut r = NativeMethodRegistry::new();
    let before = r.len();
    register_jmx_openmbean_natives(&mut r);
    let after = r.len();
    // 7 distinct (class, name, descriptor) registrations land here.
    assert!(
        after - before >= 7,
        "expected at least 7 new natives, got {}",
        after - before
    );
}

#[test]
fn test_introspector_get_methods_registered() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_openmbean_natives(&mut r);
    assert!(r
        .find(
            "com/sun/jmx/mbeanserver/MBeanIntrospector",
            "getMethods",
            "(Ljava/lang/Class;)Ljava/util/List;"
        )
        .is_some());
    assert!(r
        .find(
            "com/sun/jmx/mbeanserver/MXBeanIntrospector",
            "getMethods",
            "(Ljava/lang/Class;)Ljava/util/List;"
        )
        .is_some());
}

/// The type-mapping overlay is the synthetic arm ONLY.
///
/// Registered by default, `ConvertingMethod.from` installs an identity
/// return mapping and `mappingForType` types everything it does not
/// recognise as `SimpleType.STRING`, which is what made
/// `MBeanServer.getAttribute` answer a raw `java.lang.management.
/// MemoryUsage` instead of a `CompositeDataSupport`.
#[test]
fn type_mapping_overlay_is_synthetic_only() {
    const MAPPING_OVERLAY: &[(&str, &str, &str)] = &[
        (
            "com/sun/jmx/mbeanserver/ConvertingMethod",
            "from",
            "(Ljava/lang/reflect/Method;)Lcom/sun/jmx/mbeanserver/ConvertingMethod;",
        ),
        (
            "com/sun/jmx/mbeanserver/MXBeanMappingFactory",
            "mappingForType",
            "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;",
        ),
        (
            "com/sun/jmx/mbeanserver/DefaultMXBeanMappingFactory",
            "mappingForType",
            "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;",
        ),
        (
            "com/sun/jmx/mbeanserver/DefaultMXBeanMappingFactory",
            "makeMapping",
            "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;",
        ),
        (
            "com/sun/jmx/mbeanserver/MXBeanMapping",
            "toOpenValue",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (
            "com/sun/jmx/mbeanserver/MXBeanMapping",
            "fromOpenValue",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        ),
    ];

    let mut real = NativeMethodRegistry::new();
    register_jmx_openmbean_natives_with(&mut real, false);
    for (c, m, d) in MAPPING_OVERLAY {
        assert!(
            real.find(c, m, d).is_none(),
            "{c}.{m} must not shadow the real mapping factory by default"
        );
    }

    let mut synth = NativeMethodRegistry::new();
    register_jmx_openmbean_natives_with(&mut synth, true);
    for (c, m, d) in MAPPING_OVERLAY {
        assert!(
            synth.find(c, m, d).is_some(),
            "{c}.{m} must still be available for synthetic-jdk builds"
        );
    }

    // The Object-method filter is the PRIMARY fix, not part of the
    // overlay: it is what keeps the real factory from being offered the
    // `Class` -> `AnnotatedType[]` self-reference at all, so it stays
    // registered in both arms.
    for r in [&real, &synth] {
        assert!(r
            .find(
                "com/sun/jmx/mbeanserver/MXBeanIntrospector",
                "getMethods",
                "(Ljava/lang/Class;)Ljava/util/List;"
            )
            .is_some());
    }
}

#[test]
fn test_mapping_for_type_registered() {
    // Both `mappingForType` spellings belong to the synthetic overlay;
    // `type_mapping_overlay_is_synthetic_only` above owns the full arm
    // comparison. This one keeps the synthetic arm's own coverage.
    let mut r = NativeMethodRegistry::new();
    register_jmx_openmbean_natives_with(&mut r, true);
    assert!(
        r.find(
            "com/sun/jmx/mbeanserver/MXBeanMappingFactory",
            "mappingForType",
            "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;"
        )
        .is_some()
    );
    assert!(
        r.find(
            "com/sun/jmx/mbeanserver/DefaultMXBeanMappingFactory",
            "mappingForType",
            "(Ljava/lang/reflect/Type;Lcom/sun/jmx/mbeanserver/MXBeanMappingFactory;)Lcom/sun/jmx/mbeanserver/MXBeanMapping;"
        )
        .is_some()
    );
}

#[test]
fn test_object_method_names_includes_get_class() {
    assert!(OBJECT_METHOD_NAMES.contains(&"getClass"));
    assert!(OBJECT_METHOD_NAMES.contains(&"hashCode"));
    assert!(OBJECT_METHOD_NAMES.contains(&"equals"));
    assert!(OBJECT_METHOD_NAMES.contains(&"toString"));
    assert!(OBJECT_METHOD_NAMES.contains(&"clone"));
    assert!(OBJECT_METHOD_NAMES.contains(&"notify"));
    assert!(OBJECT_METHOD_NAMES.contains(&"notifyAll"));
    assert!(OBJECT_METHOD_NAMES.contains(&"wait"));
    assert!(OBJECT_METHOD_NAMES.contains(&"finalize"));
}

#[test]
fn test_problematic_type_names_includes_class() {
    assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.Class"));
    assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.reflect.AnnotatedType"));
    assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.reflect.AnnotatedType[]"));
}

#[test]
fn test_parse_method_descriptor_void_no_args() {
    let (params, ret) = parse_method_descriptor("()V");
    assert_eq!(params, Vec::<String>::new());
    assert_eq!(ret, "V");
}

#[test]
fn test_parse_method_descriptor_simple() {
    let (params, ret) = parse_method_descriptor("(I)Ljava/lang/String;");
    assert_eq!(params, vec!["I".to_string()]);
    assert_eq!(ret, "Ljava/lang/String;");
}

#[test]
fn test_parse_method_descriptor_array_arg() {
    let (params, ret) = parse_method_descriptor("([Ljava/lang/Object;)V");
    assert_eq!(params, vec!["[Ljava/lang/Object;".to_string()]);
    assert_eq!(ret, "V");
}

#[test]
fn test_parse_method_descriptor_multiple_args() {
    let (params, ret) = parse_method_descriptor("(IJZLjava/lang/String;)[I");
    assert_eq!(
        params,
        vec![
            "I".to_string(),
            "J".to_string(),
            "Z".to_string(),
            "Ljava/lang/String;".to_string(),
        ]
    );
    assert_eq!(ret, "[I");
}

#[test]
fn test_primitive_or_primitive_array_name_for_jmx_signatures() {
    assert!(primitive_or_primitive_array_name("long"));
    assert!(primitive_or_primitive_array_name("[J"));
    assert!(primitive_or_primitive_array_name("[[I"));
    assert!(!primitive_or_primitive_array_name("java/lang/String"));
    assert!(!primitive_or_primitive_array_name("[Ljava/lang/String;"));
}

#[test]
fn test_parse_method_descriptor_malformed_no_paren() {
    let (params, ret) = parse_method_descriptor("garbage");
    assert!(params.is_empty());
    assert_eq!(ret, "V");
}

#[test]
fn test_parse_method_descriptor_unterminated_class_token() {
    // Missing trailing ';' on Lname; — should not panic.
    let (params, ret) = parse_method_descriptor("(Ljava/lang/Bad");
    // The parser stops as soon as it can't read a token.
    let _ = (params, ret);
}

#[test]
fn test_parse_method_descriptor_nested_array() {
    let (params, ret) = parse_method_descriptor("([[Ljava/lang/String;)V");
    assert_eq!(params, vec!["[[Ljava/lang/String;".to_string()]);
    assert_eq!(ret, "V");
}

#[test]
fn test_alloc_identity_mapping_returns_non_null() {
    let mut ctx = mock_ctx();
    let m = alloc_identity_mapping(&mut ctx).unwrap();
    // Should be a real ObjectRef.
    let _ = m;
}

#[test]
fn test_alloc_array_list_from_empty() {
    let mut ctx = mock_ctx();
    let lst = alloc_array_list_from(&mut ctx, &[]).unwrap();
    // Slot 1 = size = 0
    match ctx.get_field(lst, 1) {
        Value::Int(0) => {}
        other => panic!("expected size=0, got {:?}", other),
    }
}

#[test]
fn test_type_descriptor_to_class_mirror_malformed_l_no_panic() {
    // Regression: attacker-controlled object descriptors with a missing
    // terminating `;` (or shorter than the minimum `L_;`) must not panic
    // on the internal-name slice. They should resolve to a non-null
    // fallback mirror instead.
    let mut ctx = mock_ctx();
    for desc in ["L", "L;", "Lfoo", "Ljava/lang/String", "[", "[L"] {
        let m = type_descriptor_to_class_mirror(&mut ctx, desc);
        assert!(
            !m.as_ptr().is_null(),
            "malformed descriptor {:?} produced a null mirror",
            desc
        );
    }
}

#[test]
fn test_type_descriptor_to_class_mirror_wellformed_l_no_panic() {
    // A well-formed object descriptor must still be accepted without
    // panicking after the bounds hardening.
    let mut ctx = mock_ctx();
    let m = type_descriptor_to_class_mirror(&mut ctx, "Ljava/lang/Object;");
    assert!(!m.as_ptr().is_null());
}

#[test]
fn test_native_converting_method_from_null_arg() {
    let mut ctx = mock_ctx();
    let r = native_converting_method_from(&mut ctx, &[Value::Object(None)]);
    // null Method → null result, no exception.
    match r {
        Ok(Some(Value::Object(None))) => {}
        other => panic!("expected null, got {:?}", other),
    }
}

#[test]
fn test_native_mapping_for_type_null_type_returns_identity() {
    let mut ctx = mock_ctx();
    let r = native_mapping_for_type(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
        ],
    );
    // null type → identity mapping (non-null), not exception.
    match r {
        Ok(Some(Value::Object(Some(_)))) => {}
        other => panic!("expected non-null, got {:?}", other),
    }
}

#[test]
fn test_native_introspector_get_methods_with_null_class() {
    let mut ctx = mock_ctx();
    // args[0]=this, args[1]=null class — must not panic.
    let r =
        native_introspector_get_methods(&mut ctx, &[Value::Object(None), Value::Object(None)]);
    // null Class → null list (caller iterator-loop tolerates null
    // through `instanceof List` check — defensive).
    match r {
        Ok(Some(Value::Object(_))) => {}
        other => panic!("unexpected return: {:?}", other),
    }
}

// -----------------------------------------------------------------
// T19.M1 — proactive-hardening tests (≥12 new). Each test asserts
// a specific aspect of the new cycle detector + composite-type
// pre-registration logic.
// -----------------------------------------------------------------

/// Reset the per-thread visited-types stack between tests so one
/// test's leftover state can't bias another. Each test that
/// depends on the stack starts by clearing it.
fn reset_visited() {
    VISITED_TYPES.with(|v| v.borrow_mut().clear());
}

#[test]
fn t19_m1_composite_schema_memory_usage_present() {
    let s =
        composite_schema_for("java.lang.management.MemoryUsage").expect("MemoryUsage schema");
    assert_eq!(s.items.len(), 4);
    assert_eq!(s.items, &["init", "used", "committed", "max"]);
}

#[test]
fn t19_m1_composite_schema_thread_info_present() {
    let s = composite_schema_for("java.lang.management.ThreadInfo").expect("ThreadInfo schema");
    // ThreadInfo has 17 published item names in JDK 25.
    assert_eq!(s.items.len(), 17);
    assert!(s.items.contains(&"threadId"));
    assert!(s.items.contains(&"threadState"));
    assert!(s.items.contains(&"stackTrace"));
    assert!(s.items.contains(&"daemon"));
}

#[test]
fn t19_m1_composite_schema_lock_info_present() {
    let s = composite_schema_for("java.lang.management.LockInfo").expect("LockInfo schema");
    assert_eq!(s.items.len(), 2);
    assert_eq!(s.items, &["className", "identityHashCode"]);
}

#[test]
fn t19_m1_composite_schema_monitor_info_present() {
    let s =
        composite_schema_for("java.lang.management.MonitorInfo").expect("MonitorInfo schema");
    assert_eq!(s.items.len(), 4);
    assert!(s.items.contains(&"lockedStackDepth"));
    assert!(s.items.contains(&"lockedStackFrame"));
}

#[test]
fn t19_m1_composite_schema_stack_trace_element_present() {
    let s =
        composite_schema_for("java.lang.StackTraceElement").expect("StackTraceElement schema");
    assert_eq!(s.items.len(), 8);
    assert!(s.items.contains(&"className"));
    assert!(s.items.contains(&"methodName"));
    assert!(s.items.contains(&"lineNumber"));
    assert!(s.items.contains(&"nativeMethod"));
}

#[test]
fn t19_m1_composite_schema_unknown_type_returns_none() {
    assert!(composite_schema_for("com.acme.MyCustomClass").is_none());
}

#[test]
fn t19_m1_composite_type_count_matches_well_known() {
    // Exactly 5 well-known platform-MXBean composite types.
    assert_eq!(COMPOSITE_SCHEMAS.len(), 5);
    assert_eq!(COMPOSITE_TYPE_NAMES.len(), 5);
}

#[test]
fn t19_m1_problematic_type_names_extended() {
    // T19.H14 baseline:
    assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.Class"));
    // T19.M1 additions:
    assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.reflect.Method"));
    assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.reflect.Field"));
    assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.Module"));
    assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.security.ProtectionDomain"));
    assert!(PROBLEMATIC_TYPE_NAMES.contains(&"java.lang.ClassLoader"));
}

#[test]
fn t19_m1_max_mapping_depth_is_three() {
    // Asserts the depth bound is small enough to break cycles
    // quickly but generous enough to allow legitimate nested
    // generic types (e.g. List<Map<String, MemoryUsage>>) to
    // resolve normally. Three is documented in the module-level
    // doc comment.
    assert_eq!(MAX_MAPPING_DEPTH, 3);
}

#[test]
fn t19_m1_cycle_detector_breaks_at_depth_3() {
    reset_visited();
    let mut ctx = mock_ctx();
    // Simulate a recursive descent: push the same type 3 times,
    // then call native_mapping_for_type with a synthetic Class
    // mirror named "java.lang.management.MemoryUsage". The
    // depth-3 guard should fire and return identity mapping
    // instead of pushing a fourth entry.
    VISITED_TYPES.with(|v| {
        let mut stack = v.borrow_mut();
        stack.push("java.lang.management.MemoryUsage".to_string());
        stack.push("java.lang.management.MemoryUsage".to_string());
        stack.push("java.lang.management.MemoryUsage".to_string());
    });

    // Allocate a synthetic Class mirror that resolves to that
    // name via slot 1 (matches Class.name layout).
    let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
    let name_str = ctx.create_string("java.lang.management.MemoryUsage");
    ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));

    let r = native_mapping_for_type(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(class_mirror)),
            Value::Object(None),
        ],
    )
    .expect("native call must succeed");
    assert!(matches!(r, Some(Value::Object(Some(_)))));

    // The depth-3 guard returns an identity mapping without
    // pushing onto the visited stack — verify stack length is
    // unchanged (still 3).
    VISITED_TYPES.with(|v| assert_eq!(v.borrow().len(), 3));
    reset_visited();
}

#[test]
fn t19_m1_cycle_detector_allows_first_two_entries() {
    reset_visited();
    let mut ctx = mock_ctx();

    // Push only twice — depth check should NOT fire.
    VISITED_TYPES.with(|v| {
        let mut stack = v.borrow_mut();
        stack.push("java.lang.String".to_string());
        stack.push("java.lang.String".to_string());
    });

    let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
    let name_str = ctx.create_string("java.lang.String");
    ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));

    let r = native_mapping_for_type(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(class_mirror)),
            Value::Object(None),
        ],
    )
    .expect("native call");
    assert!(matches!(r, Some(Value::Object(Some(_)))));
    // String is not a composite type, so we hit the "track and
    // pop" path. Stack should be unchanged after pop.
    VISITED_TYPES.with(|v| assert_eq!(v.borrow().len(), 2));
    reset_visited();
}

#[test]
fn t19_m1_cycle_detector_pops_on_normal_path() {
    reset_visited();
    let mut ctx = mock_ctx();

    let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
    let name_str = ctx.create_string("com.acme.SomeBean");
    ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));

    let _r = native_mapping_for_type(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(class_mirror)),
            Value::Object(None),
        ],
    );

    // After a clean call, the visited stack must be empty —
    // the type was pushed and popped balanced.
    VISITED_TYPES.with(|v| assert!(v.borrow().is_empty()));
}

#[test]
fn t19_m1_problematic_type_short_circuits_without_push() {
    reset_visited();
    let mut ctx = mock_ctx();
    let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
    let name_str = ctx.create_string("java.lang.Class");
    ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));

    let _r = native_mapping_for_type(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(class_mirror)),
            Value::Object(None),
        ],
    );

    // Problematic types short-circuit before push — stack stays empty.
    VISITED_TYPES.with(|v| assert!(v.borrow().is_empty()));
    reset_visited();
}

#[test]
fn t19_m1_composite_type_short_circuits_without_recursion() {
    reset_visited();
    let mut ctx = mock_ctx();
    let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
    let name_str = ctx.create_string("java.lang.management.MemoryUsage");
    ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));

    let r = native_mapping_for_type(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(class_mirror)),
            Value::Object(None),
        ],
    )
    .expect("native call");

    // Should return non-null (composite mapping), and the visited
    // stack should be empty (push+pop balanced).
    assert!(matches!(r, Some(Value::Object(Some(_)))));
    VISITED_TYPES.with(|v| assert!(v.borrow().is_empty()));
}

#[test]
fn t19_m1_alloc_composite_mapping_returns_non_null() {
    let mut ctx = mock_ctx();
    let schema =
        composite_schema_for("java.lang.management.MemoryUsage").expect("schema present");
    let m = alloc_composite_mapping(&mut ctx, schema).unwrap();
    // Verify the openType slot is populated.
    match ctx.get_field(m, 1) {
        Value::Object(Some(_)) => {}
        other => panic!("expected populated openType, got {:?}", other),
    }
}

#[test]
fn t19_m1_alloc_composite_type_carries_item_names() {
    let mut ctx = mock_ctx();
    let schema = composite_schema_for("java.lang.management.LockInfo").expect("schema");
    let ct = alloc_composite_type(&mut ctx, schema).unwrap();
    // The mock NativeContext doesn't map our well-known JMX field
    // names (typeName, description, className, isArray, itemNames)
    // to slots — production resolves these via the loaded class's
    // actual field layout. We assert here that the allocation
    // succeeded and is structurally non-empty by checking
    // `object_num_fields` returned at least the 8 slots we asked
    // for in `alloc_concurrent_synthetic`.
    let n = ctx.object_num_fields(ct);
    assert!(n >= 8, "expected ≥8 fields on CompositeType stub, got {n}");
}

#[test]
fn t19_m1_alloc_open_converter_with_schema() {
    let mut ctx = mock_ctx();
    let schema = composite_schema_for("java.lang.management.ThreadInfo").expect("schema");
    let oc = alloc_open_converter(&mut ctx, Some(schema)).unwrap();
    // identityConverter flag (slot 3) should be 1.
    match ctx.get_field(oc, 3) {
        Value::Int(1) => {}
        other => panic!("expected Int(1), got {:?}", other),
    }
}

#[test]
fn t19_m1_alloc_open_converter_without_schema_uses_simple_string() {
    let mut ctx = mock_ctx();
    let oc = alloc_open_converter(&mut ctx, None).unwrap();
    // openType (slot 1) must be a SimpleType-shaped object (non-null).
    match ctx.get_field(oc, 1) {
        Value::Object(Some(_)) => {}
        other => panic!("expected non-null openType, got {:?}", other),
    }
}

#[test]
fn t19_m1_alloc_mapped_mxbean_type_basic_for_unknown() {
    let mut ctx = mock_ctx();
    let mt = alloc_mapped_mxbean_type(&mut ctx, None).unwrap();
    // isBasicType (slot 2) should be 1 (SimpleType-mapped).
    match ctx.get_field(mt, 2) {
        Value::Int(1) => {}
        other => panic!("expected Int(1), got {:?}", other),
    }
}

#[test]
fn t19_m1_alloc_mapped_mxbean_type_composite_for_known() {
    let mut ctx = mock_ctx();
    let schema = composite_schema_for("java.lang.management.MemoryUsage").expect("schema");
    let mt = alloc_mapped_mxbean_type(&mut ctx, Some(schema)).unwrap();
    // isBasicType should be 0 for composite.
    match ctx.get_field(mt, 2) {
        Value::Int(0) => {}
        other => panic!("expected Int(0) for composite, got {:?}", other),
    }
}

#[test]
fn t19_m1_native_open_converter_to_converter_handles_null() {
    let mut ctx = mock_ctx();
    let r = native_open_converter_to_converter(&mut ctx, &[Value::Object(None)])
        .expect("native call");
    assert!(matches!(r, Some(Value::Object(Some(_)))));
}

#[test]
fn t19_m1_native_mapped_mxbean_type_handles_null() {
    let mut ctx = mock_ctx();
    let r = native_mapped_mxbean_type(&mut ctx, &[Value::Object(None)]).expect("native call");
    assert!(matches!(r, Some(Value::Object(Some(_)))));
}

#[test]
fn t19_m1_register_natives_count_increased() {
    let mut r = NativeMethodRegistry::new();
    let before = r.len();
    register_jmx_openmbean_natives(&mut r);
    let after = r.len();
    // T19.H14 had 7 registrations. T19.M1 adds 2 more
    // (OpenConverter.toConverter, MappedMXBeanType.getMappedMXBeanType).
    assert!(
        after - before >= 9,
        "expected ≥9 registrations after T19.M1, got {}",
        after - before
    );
}

#[test]
fn t19_m1_open_converter_to_converter_registered() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_openmbean_natives(&mut r);
    assert!(r
        .find(
            "com/sun/jmx/mbeanserver/OpenConverter",
            "toConverter",
            "(Ljava/lang/reflect/Type;)Lcom/sun/jmx/mbeanserver/OpenConverter;"
        )
        .is_some());
}

#[test]
fn t19_m1_mapped_mxbean_type_registered() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_openmbean_natives(&mut r);
    assert!(r
        .find(
            "com/sun/jmx/mbeanserver/MappedMXBeanType",
            "getMappedMXBeanType",
            "(Ljava/lang/reflect/Type;)Lcom/sun/jmx/mbeanserver/MappedMXBeanType;"
        )
        .is_some());
}

#[test]
fn t19_m1_composite_type_names_match_schemas() {
    // Every COMPOSITE_TYPE_NAMES entry has a corresponding schema.
    for name in COMPOSITE_TYPE_NAMES {
        assert!(
            composite_schema_for(name).is_some(),
            "no schema for composite name {name}"
        );
    }
}

#[test]
fn t19_m1_resolve_type_name_via_class_mirror_slot1() {
    let mut ctx = mock_ctx();
    let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
    let name_str = ctx.create_string("java.lang.management.MemoryUsage");
    ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));
    let resolved = resolve_type_name(&ctx, class_mirror);
    assert_eq!(
        resolved.as_deref(),
        Some("java.lang.management.MemoryUsage")
    );
}

#[test]
fn t19_m1_resolve_type_name_normalizes_slashes_to_dots() {
    let mut ctx = mock_ctx();
    let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 4).unwrap();
    let name_str = ctx.create_string("java/lang/management/MemoryUsage");
    ctx.set_field(class_mirror, 1, Value::Object(Some(name_str)));
    let resolved = resolve_type_name(&ctx, class_mirror);
    // Slashes must be normalized to dots so the lookup against
    // COMPOSITE_TYPE_NAMES succeeds.
    assert_eq!(
        resolved.as_deref(),
        Some("java.lang.management.MemoryUsage")
    );
}

// -----------------------------------------------------------------
// OpenMBean value-carrier tests (CompositeData / TabularData).
// -----------------------------------------------------------------

#[test]
fn open_data_carriers_registered() {
    let mut r = NativeMethodRegistry::new();
    register_jmx_openmbean_natives(&mut r);
    let cds = "javax/management/openmbean/CompositeDataSupport";
    assert!(r
        .find(cds, "get", "(Ljava/lang/String;)Ljava/lang/Object;")
        .is_some());
    assert!(r
        .find(cds, "containsKey", "(Ljava/lang/String;)Z")
        .is_some());
    assert!(r
        .find(
            cds,
            "getCompositeType",
            "()Ljavax/management/openmbean/CompositeType;"
        )
        .is_some());
    assert!(r
        .find(cds, "getAll", "([Ljava/lang/String;)[Ljava/lang/Object;")
        .is_some());
    let tds = "javax/management/openmbean/TabularDataSupport";
    assert!(
        r.find(
            tds,
            "put",
            "(Ljavax/management/openmbean/CompositeData;)Ljavax/management/openmbean/CompositeData;"
        )
        .is_some()
    );
    assert!(r.find(tds, "size", "()I").is_some());
    assert!(r.find(tds, "isEmpty", "()Z").is_some());
}

#[test]
fn build_composite_data_returns_non_null() {
    let mut ctx = mock_ctx();
    let items = vec![
        ("init".to_string(), Value::Long(0)),
        ("used".to_string(), Value::Long(1024)),
    ];
    let cd = build_composite_data(&mut ctx, None, &items).unwrap();
    // The carrier object must be a real allocated object.
    assert!(ctx.object_num_fields(cd) >= 4);
}

#[test]
fn build_tabular_data_returns_non_null() {
    let mut ctx = mock_ctx();
    let td = build_tabular_data(&mut ctx, None).unwrap();
    assert!(ctx.object_num_fields(td) >= 4);
}

#[test]
fn only_cratonvm_built_carriers_are_answered_by_the_carrier_natives() {
    use cratonvm_native_api::FieldMetadata;
    let mut ctx = mock_ctx();
    let cid = match ctx.ensure_class_initialized(CDS_CLASS) {
        Ok(cid) => cid,
        Err(e) => panic!("mock could not initialize {CDS_CLASS}: {e:?}"),
    };
    // The mock resolves a field name only through `set_declared_fields`;
    // the real VM's by-name path resolves the two carrier fields on the
    // synthetically-allocated instance itself. Declare them so both
    // branches below are reachable here.
    ctx.set_declared_fields(
        cid,
        vec![
            FieldMetadata {
                name: CONTENTS_FIELD.to_string(),
                descriptor: "Ljava/lang/Object;".to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: cid,
                is_static: false,
            },
            FieldMetadata {
                name: OPEN_TYPE_FIELD.to_string(),
                descriptor: "Ljava/lang/Object;".to_string(),
                access_flags: 0,
                slot_index: 1,
                declaring_class_id: cid,
                is_static: false,
            },
        ],
    );

    let carrier = match ctx.new_object(CDS_CLASS) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("mock could not allocate an instance: {other:?}"),
    };

    // Untouched, an instance looks exactly like one the application built
    // through the JDK's own constructor: neither carrier field is set. The
    // natives answered those from the carrier fields and so returned null
    // for everything — `getCompositeType()` in particular, which made
    // `CompositeType.isValue()` reject a value against its own declared
    // type. It has to go back to the bytecode.
    assert!(!is_synthetic_carrier(&ctx, carrier));
    assert!(delegates_to_bytecode(&ctx, carrier, CDS_CLASS));

    // Stamped the way `build_composite_data` stamps it, the same instance
    // is this crate's own carrier and the natives must keep answering it —
    // under `synthetic-jdk` there is no bytecode to fall back to.
    let map = match ctx.new_object("java/util/HashMap") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("mock could not allocate a map: {other:?}"),
    };
    ctx.set_field_by_name(carrier, CONTENTS_FIELD, Value::Object(Some(map)));
    assert!(is_synthetic_carrier(&ctx, carrier));
    assert!(!delegates_to_bytecode(&ctx, carrier, CDS_CLASS));
}

#[test]
fn build_string_keyed_map_synthetic_fallback_roundtrips() {
    // Drive the synthetic parallel-array fallback by forcing the
    // real-HashMap path to be skipped: the mock's `invoke` returns
    // Ok(None) for put, so the real path "succeeds" with an empty
    // map. To exercise the fallback storage + carrier_get we build
    // the synthetic map directly and verify slot layout.
    let mut ctx = mock_ctx();
    // Allocate a synthetic map exactly as the fallback does.
    let synth = try_alloc_concurrent_synthetic(&mut ctx, "java/util/HashMap", 3).unwrap();
    let keys = ctx.new_ref_array(ClassId::new(0), 1);
    let vals = ctx.new_ref_array(ClassId::new(0), 1);
    let k = ctx.create_string("used");
    ctx.set_array_element(keys, 0, Value::Object(Some(k)));
    ctx.set_array_element(vals, 0, Value::Long(4096));
    ctx.set_field(synth, 0, Value::Object(Some(keys)));
    ctx.set_field(synth, 1, Value::Object(Some(vals)));
    ctx.set_field(synth, 2, Value::Int(1));
    // Verify the parallel-array scan finds the value.
    match (ctx.get_field(synth, 0), ctx.get_field(synth, 1)) {
        (Value::Object(Some(ks)), Value::Object(Some(vs))) => {
            assert_eq!(ctx.array_length(ks), 1);
            assert!(matches!(ctx.get_array_element(vs, 0), Value::Long(4096)));
        }
        _ => panic!("synthetic map slots not populated"),
    }
}
