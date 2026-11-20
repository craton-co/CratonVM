use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Structural regression guard for `emit_bean_override`'s `fb_ref`
/// byte-splice: build one FactoryBean-typed and one plain `@Bean`
/// method on the same enhancer class, parse the result with the real
/// class-file reader (`cratonvm-reader`, no VM/NativeContext needed —
/// this is pure byte-emission logic), and check the FactoryBean method's
/// Code attribute is exactly 8 bytes longer, its `max_stack` one deeper,
/// and each of its two exception-table entries shifted by exactly the
/// amount the splice implies: the inner NoSuchBeanDefinitionException
/// handler wholesale (+8 on all three PCs, it lives after the splice
/// point), the outer SPR-8080 `finally` only on its end/handler PCs (its
/// TRY_START precedes the splice).
///
/// Everything is asserted as a DELTA against the plain method. Absolute
/// offsets were pinned here once and broke the moment an unrelated change
/// to `emit_bean_override`'s body moved them (the mismatch block added
/// 120 bytes and a second handler) — while the splice itself, which is all
/// this test exists to guard, was still correct.
#[test]
fn fb_ref_splice_shifts_exception_table_by_exactly_8_bytes() {
    let bean_methods = vec![
        BeanMethod {
            name: "plainBean".to_string(),
            descriptor: "()Lcom/example/Plain;".to_string(),
            return_internal: "com/example/Plain".to_string(),
            is_factory_bean: false,
        },
        BeanMethod {
            name: "factoryBean".to_string(),
            descriptor: "()Lcom/example/MyFactoryBean;".to_string(),
            return_internal: "com/example/MyFactoryBean".to_string(),
            is_factory_bean: true,
        },
    ];
    // Dedicated loader id so parallel test runs never share a
    // `next_config_enhancer_counter` bucket with another test.
    let (_, bytes) = build_enhancer_class(0, 999_001,"com/example/MyConfig", &bean_methods, &[]);
    let class_file = cratonvm_reader::read_class(&bytes).expect("generated class must parse");

    let mut plain = class_file
        .find_method("plainBean", "()Lcom/example/Plain;")
        .expect("plainBean method present")
        .clone();
    let mut factory = class_file
        .find_method("factoryBean", "()Lcom/example/MyFactoryBean;")
        .expect("factoryBean method present")
        .clone();
    for a in plain.attributes.iter_mut() {
        a.decode(&class_file.constant_pool)
            .expect("decode plainBean Code");
    }
    for a in factory.attributes.iter_mut() {
        a.decode(&class_file.constant_pool)
            .expect("decode factoryBean Code");
    }
    let plain_code = plain.code().expect("plainBean has a Code attribute");
    let factory_code = factory.code().expect("factoryBean has a Code attribute");

    // Everything asserted here is RELATIVE to the plain method. The
    // absolute byte offsets this test used to pin (code length 161,
    // exception-table PCs 94/109/142) describe `emit_bean_override`'s body
    // at the time the splice landed, so any later, unrelated edit to that
    // body breaks them while the splice itself is still correct — which is
    // exactly what happened. The splice contract is a delta, so test the
    // delta.
    assert_eq!(
        factory_code.max_stack,
        plain_code.max_stack + 1,
        "fb_ref splice briefly needs one more stack slot"
    );
    assert_eq!(plain_code.max_locals, factory_code.max_locals);

    assert_eq!(
        factory_code.code.len(),
        plain_code.code.len() + 8,
        "fb_ref splice must add exactly 8 bytes"
    );

    // `emit_bean_override` emits TWO handlers, and the splice moves them
    // differently — assert each against its plain-method counterpart.
    assert_eq!(plain_code.exception_table.len(), 2);
    assert_eq!(factory_code.exception_table.len(), 2);

    // Entry 0 — the inner NoSuchBeanDefinitionException handler around the
    // mismatch block. The whole block sits AFTER the splice point, so all
    // three of its PCs move by the full +8.
    let pe0 = &plain_code.exception_table[0];
    let fe0 = &factory_code.exception_table[0];
    assert_eq!(
        fe0.start_pc,
        pe0.start_pc + 8,
        "inner try2 starts after the splice point — shifts wholesale"
    );
    assert_eq!(fe0.end_pc, pe0.end_pc + 8);
    assert_eq!(fe0.handler_pc, pe0.handler_pc + 8);
    assert_eq!(fe0.catch_type, pe0.catch_type);

    // Entry 1 — the outer SPR-8080 any-Throwable `finally`. Its TRY_START
    // is before the splice point, so only its end/handler move.
    let pe1 = &plain_code.exception_table[1];
    let fe1 = &factory_code.exception_table[1];
    assert_eq!(
        fe1.start_pc, pe1.start_pc,
        "TRY_START is before the splice point — unaffected"
    );
    assert_eq!(fe1.end_pc, pe1.end_pc + 8);
    assert_eq!(fe1.handler_pc, pe1.handler_pc + 8);
    assert_eq!(
        fe1.catch_type, 0,
        "outer handler keeps `finally` (catch_type 0) semantics"
    );
}

/// Regression guard for the real-cglib-bookkeeping fields
/// (`emit_public_static_field`'s call site): every generated enhancer
/// class must declare all five, PUBLIC, with the exact names/types
/// real cglib's `Enhancer.wrapCachedClass`/`isEnhanced`/callback
/// machinery reflects on when it mistakes one of our classes for its
/// own (see `emit_cglib_factory_data_field`'s doc comment) — or the
/// `ConfigurationClassPostProcessorTests.genericsBasedInjectionWith*`
/// class of bug resurfaces one field at a time.
#[test]
fn enhancer_class_declares_all_cglib_bookkeeping_fields() {
    let (_, bytes) = build_enhancer_class(0, 999_002,"com/example/BookkeepingConfig", &[], &[]);
    let class_file = cratonvm_reader::read_class(&bytes).expect("generated class must parse");

    let expect_field = |name: &str, descriptor: &str| {
        let f = class_file
            .find_field(name)
            .unwrap_or_else(|| panic!("missing field {name}"));
        assert_eq!(&*f.descriptor, descriptor, "{name} has wrong descriptor");
        assert!(
            f.access_flags
                .contains(cratonvm_reader::class_access_flags::FieldAccessFlags::PUBLIC)
                && f.is_static(),
            "{name} must be public static"
        );
    };
    expect_field("CGLIB$FACTORY_DATA", "Ljava/lang/Object;");
    expect_field("CGLIB$CALLBACK_FILTER", "Ljava/lang/Object;");
    expect_field("CGLIB$THREAD_CALLBACKS", "Ljava/lang/Object;");
    expect_field("CGLIB$STATIC_CALLBACKS", "Ljava/lang/Object;");
    expect_field("CGLIB$BOUND", "Z");

    assert!(
        class_file
            .interfaces
            .iter()
            .any(|i| &**i == "org/springframework/cglib/proxy/Factory"),
        "generated class must implement org/springframework/cglib/proxy/Factory \
         (real cglib's defineClass-failure recovery path casts our class to it)"
    );
    let callback_desc = "Lorg/springframework/cglib/proxy/Callback;";
    let callback_arr_desc = "[Lorg/springframework/cglib/proxy/Callback;";
    for (name, desc) in [
        (
            "newInstance",
            format!("({callback_desc})Ljava/lang/Object;"),
        ),
        (
            "newInstance",
            format!("({callback_arr_desc})Ljava/lang/Object;"),
        ),
        (
            "newInstance",
            format!(
                "([Ljava/lang/Class;[Ljava/lang/Object;{callback_arr_desc})Ljava/lang/Object;"
            ),
        ),
        ("getCallback", format!("(I){callback_desc}")),
        ("setCallback", format!("(I{callback_desc})V")),
        ("getCallbacks", format!("(){callback_arr_desc}")),
        ("setCallbacks", format!("({callback_arr_desc})V")),
    ] {
        assert!(
            class_file.find_method(name, &desc).is_some(),
            "missing Factory method {name}{desc}"
        );
    }
}
