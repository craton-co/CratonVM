use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Interpreter round i4 wave 94, lane L3: every asking method of the
/// reflective doors is a frame the name list skips (the rule searches
/// only that leading run), and non-`@CallerSensitive` neighbours are not
/// asking frames.
#[test]
fn i94_reflective_door_cs_methods_are_listed_frames() {
    for (class, method) in [
        ("java/lang/reflect/Method", "invoke"),
        ("java/lang/reflect/Constructor", "newInstance"),
        ("java/lang/reflect/Field", "getInt"),
        ("java/lang/reflect/Field", "set"),
        ("java/lang/reflect/AccessibleObject", "trySetAccessible"),
        ("java/lang/Class", "getResourceAsStream"),
        ("java/lang/Class", "newInstance"),
    ] {
        assert!(is_reflective_door_cs_method(class, method), "{class}.{method}");
        assert!(crate::lang_class::is_reflection_internal_frame(class), "{class}");
    }
    assert!(!is_reflective_door_cs_method("java/lang/reflect/Field", "getName"));
    assert!(!is_reflective_door_cs_method("java/lang/reflect/AccessibleObject", "checkAccess"));
    assert!(!is_reflective_door_cs_method("java/lang/Class", "forName"));
    assert!(!is_reflective_door_cs_method("p/Method", "invoke"));
}

#[test]
fn get_instance_default_returns_non_null_walker() {
    let mut ctx = MockNativeContext::new();
    let result = native_get_instance_default(&mut ctx, &[])
        .expect("native should succeed")
        .expect("should return Some(Value)");
    match result {
        Value::Object(Some(_)) => {}
        other => panic!("expected non-null StackWalker, got {:?}", other),
    }
}

#[test]
fn get_instance_default_is_idempotent_in_shape() {
    // Idempotent meaning: repeated calls always yield valid non-null
    // walkers whose options and retainClassRef fields are consistent.
    let mut ctx = MockNativeContext::new();
    let r1 = native_get_instance_default(&mut ctx, &[]).unwrap().unwrap();
    let r2 = native_get_instance_default(&mut ctx, &[]).unwrap().unwrap();
    for r in [r1, r2] {
        if let Value::Object(Some(walker)) = r {
            match ctx.get_field(walker, FIELD_ESTIMATE_DEPTH) {
                Value::Int(n) => assert!(n > 0, "estimateDepth must be > 0"),
                other => panic!("expected Int for estimateDepth, got {:?}", other),
            }
            match ctx.get_field(walker, FIELD_RETAIN_CLASS_REF) {
                Value::Int(n) => assert_eq!(n, 0, "default should not retain class ref"),
                other => panic!("expected Int for retainClassRef, got {:?}", other),
            }
        } else {
            panic!("expected non-null walker");
        }
    }
}

#[test]
fn get_instance_set_depth_rejects_nonpositive() {
    let mut ctx = MockNativeContext::new();
    let set = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Set", 1).unwrap();
    let bad_args = [Value::Object(Some(set)), Value::Int(0)];
    let err = native_get_instance_set_depth(&mut ctx, &bad_args).unwrap_err();
    let s = format!("{:?}", err);
    assert!(
        s.contains("estimateDepth") || s.contains("IllegalArgument"),
        "expected IllegalArgument, got {}",
        s
    );

    let neg_args = [Value::Object(Some(set)), Value::Int(-1)];
    let err = native_get_instance_set_depth(&mut ctx, &neg_args).unwrap_err();
    let s = format!("{:?}", err);
    assert!(
        s.contains("estimateDepth") || s.contains("IllegalArgument"),
        "expected IllegalArgument, got {}",
        s
    );
}

#[test]
fn get_instance_set_depth_rejects_null_set() {
    let mut ctx = MockNativeContext::new();
    let args = [Value::Object(None), Value::Int(4)];
    let err = native_get_instance_set_depth(&mut ctx, &args).unwrap_err();
    let s = format!("{:?}", err);
    assert!(
        s.contains("Null") || s.contains("null"),
        "expected NPE, got {}",
        s
    );
}

#[test]
fn get_instance_set_depth_sets_fields_correctly() {
    let mut ctx = MockNativeContext::new();
    let set = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Set", 1).unwrap();
    let args = [Value::Object(Some(set)), Value::Int(16)];
    let result = native_get_instance_set_depth(&mut ctx, &args)
        .unwrap()
        .unwrap();
    if let Value::Object(Some(walker)) = result {
        match ctx.get_field(walker, FIELD_ESTIMATE_DEPTH) {
            Value::Int(n) => assert_eq!(n, 16),
            other => panic!("expected Int(16), got {:?}", other),
        }
        match ctx.get_field(walker, FIELD_RETAIN_CLASS_REF) {
            Value::Int(n) => assert_eq!(n, 1, "explicit option set implies retain"),
            other => panic!("expected Int(1), got {:?}", other),
        }
        match ctx.get_field(walker, FIELD_OPTIONS) {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null options Set, got {:?}", other),
        }
    } else {
        panic!("expected non-null walker");
    }
}

/// Stand a `StackWalker$Option` class up in the mock declaring exactly
/// `constants` (plus `$VALUES`), run the native clinit, and assert
/// `$VALUES` is those constants in that order — each carrying the
/// `name`/`ordinal` state `java.lang.Enum` declares.
///
/// The name/ordinal assertions are the point. Asserting only that
/// `$VALUES[i]` is the same object as the i-th static field — which is
/// what this test used to do — holds for ANY set of instances the native
/// invents, including the nameless ones that made `Enum.valueOf` match
/// nothing and took out every Mockito-backed netty test class. A test that
/// cannot fail on the original bug is not cover for it.
fn assert_option_clinit_shape(constants: &[&str]) {
    use cratonvm_native_api::FieldMetadata;

    let mut ctx = MockNativeContext::new();
    let option_class = ctx
        .ensure_class_initialized("java/lang/StackWalker$Option")
        .unwrap();
    let mut fields: Vec<FieldMetadata> = constants
        .iter()
        .map(|n| (*n, "Ljava/lang/StackWalker$Option;"))
        .chain(std::iter::once((
            "$VALUES",
            "[Ljava/lang/StackWalker$Option;",
        )))
        .enumerate()
        .map(|(slot_index, (name, descriptor))| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: option_class,
            is_static: true,
        })
        .collect();
    // The INSTANCE fields every enum inherits from `java.lang.Enum`, in
    // the JDK's slot order. Without them the mock falls back to a generic
    // name→slot table that puts `name` and `ordinal` wherever it likes,
    // and the assertions below would be measuring the mock rather than
    // the native.
    for (slot_index, (name, descriptor)) in [("name", "Ljava/lang/String;"), ("ordinal", "I")]
        .into_iter()
        .enumerate()
    {
        fields.push(FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: option_class,
            is_static: false,
        });
    }
    ctx.set_declared_fields(option_class, fields);

    native_option_clinit(&mut ctx, &[]).expect("clinit should succeed");

    let values_slot = ctx
        .static_field_index_by_name(option_class, "$VALUES")
        .unwrap();
    let values_array = match ctx.get_static_field(option_class, values_slot) {
        Value::Object(Some(array)) => array,
        other => panic!("expected non-null $VALUES array, got {:?}", other),
    };
    assert_eq!(
        ctx.array_length(values_array),
        constants.len(),
        "$VALUES must have one entry per constant the class declares"
    );

    for (idx, name) in constants.iter().enumerate() {
        let slot = ctx.static_field_index_by_name(option_class, name).unwrap();
        let static_value = ctx.get_static_field(option_class, slot);
        assert_eq!(
            ctx.get_array_element(values_array, idx),
            static_value,
            "$VALUES[{idx}] must be {name}"
        );
        let constant = match static_value {
            Value::Object(Some(o)) => o,
            other => panic!("{name} static is {other:?}, expected an object"),
        };
        // Non-null is NOT the contract; the name must be the RIGHT string.
        // Merged 2026-08-12 from the jdk-only campaign, which found the
        // weaker shape twice in one day: a doc froze "values() returns
        // length 3" as verified-good while all three constants were wrong,
        // and a sibling enum passed a non-null-and-named check while
        // `values()[0] == State.NEW` was false. A null-check and an
        // identity/equality check are different assertions, and only the
        // latter can see a fabricated constant.
        match ctx.get_field_by_name(constant, "name") {
            Value::Object(Some(s)) => assert_eq!(
                ctx.read_string(s).as_deref(),
                // `name` is `&&str` from `constants.iter()`; deref rather
                // than `.as_str()`, which resolves to the still-unstable
                // `str::as_str` and fails to compile the whole test target.
                Some(*name),
                "{name} carries the wrong Enum.name — a wrong or null name makes \
                 Enum.valueOf's constant directory match nothing"
            ),
            other => panic!(
                "{name} must carry an Enum.name string, got {other:?} — a null name \
                 makes Enum.valueOf's constant directory match nothing"
            ),
        }
        assert_eq!(
            ctx.get_field_by_name(constant, "ordinal"),
            Value::Int(idx as i32),
            "{name} must carry ordinal {idx}"
        );
    }
}

/// JDK 22+ declares four constants, with `DROP_METHOD_INFO` at ordinal 1.
#[test]
fn option_clinit_populates_enum_values_array() {
    assert_option_clinit_shape(&[
        "RETAIN_CLASS_REFERENCE",
        "DROP_METHOD_INFO",
        "SHOW_REFLECT_FRAMES",
        "SHOW_HIDDEN_FRAMES",
    ]);
}

/// JDK 9–21 has no `DROP_METHOD_INFO`; the native follows the class it
/// actually finds rather than publishing a constant that JDK never
/// declared.
#[test]
fn option_clinit_follows_a_pre_jdk22_three_constant_class() {
    assert_option_clinit_shape(&[
        "RETAIN_CLASS_REFERENCE",
        "SHOW_REFLECT_FRAMES",
        "SHOW_HIDDEN_FRAMES",
    ]);
}

#[test]
fn get_instance_one_option_rejects_null() {
    let mut ctx = MockNativeContext::new();
    let err = native_get_instance_one_option(&mut ctx, &[Value::Object(None)]).unwrap_err();
    let s = format!("{:?}", err);
    assert!(
        s.contains("Null") || s.contains("null"),
        "expected NPE, got {}",
        s
    );
}

#[test]
fn get_instance_one_option_sets_retain_flag() {
    let mut ctx = MockNativeContext::new();
    let option =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/StackWalker$Option", 1).unwrap();
    let args = [Value::Object(Some(option))];
    let result = native_get_instance_one_option(&mut ctx, &args)
        .unwrap()
        .unwrap();
    if let Value::Object(Some(walker)) = result {
        match ctx.get_field(walker, FIELD_RETAIN_CLASS_REF) {
            Value::Int(n) => assert_eq!(n, 1),
            other => panic!("expected Int(1), got {:?}", other),
        }
    } else {
        panic!("expected non-null walker");
    }
}

fn frame(class: &str, id: u32) -> cratonvm_native_api::StackTraceEntry {
    cratonvm_native_api::StackTraceEntry {
        class_name: std::sync::Arc::from(class),
        method_name: std::sync::Arc::from("m"),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: -1,
        class_id: Some(cratonvm_types::ClassId::new(id)),
        method_index: None,
    }
}

/// Interpreter round i1 wave 50, lane L3: HotSpot's `isMethodHandleFrame`
/// is the `java.lang.invoke` package itself, not its subpackages.
#[test]
fn method_handle_frames_are_the_java_lang_invoke_package_only() {
    assert!(is_method_handle_frame_class(
        "java/lang/invoke/MethodHandle"
    ));
    assert!(is_method_handle_frame_class(
        "java/lang/invoke/BootstrapMethodInvoker"
    ));
    assert!(!is_method_handle_frame_class("java/lang/invoke/sub/X"));
    assert!(!is_method_handle_frame_class("java/lang/Thread"));
    assert!(!is_method_handle_frame_class("p/java/lang/invoke/X"));
}

/// The finder rule (wave 50, lane L3): reflection and `java.lang.invoke`
/// frames are skipped, the second frame left answers, and a lone frame
/// throws HotSpot's `IllegalCallerException`.
#[test]
fn the_finder_rule_skips_reflection_and_invoke_frames_and_needs_a_caller() {
    let mut ctx = MockNativeContext::new();
    // OUTERMOST first: caller < Method.invoke < accessor < asker.
    let trace = [
        frame("p/Caller", 7),
        frame("java/lang/reflect/Method", 8),
        frame("jdk/internal/reflect/DirectMethodHandleAccessor", 9),
        frame("java/lang/invoke/LambdaForm$MH", 10),
        frame("p/Asker", 11),
    ];
    let answer = caller_class_by_finder_rule(&mut ctx, &trace)
        .unwrap()
        .unwrap();
    let Value::Object(Some(mirror)) = answer else {
        panic!("expected a mirror, got {answer:?}");
    };
    assert_eq!(ctx.get_field(mirror, 0), Value::Int(7));

    let lone = [frame("java/lang/reflect/Method", 8), frame("p/Main", 3)];
    let err = format!(
        "{:?}",
        caller_class_by_finder_rule(&mut ctx, &lone).unwrap_err()
    );
    assert!(err.contains("no caller frame: [p.Main , null]"), "{err}");
    let none: [cratonvm_native_api::StackTraceEntry; 0] = [];
    let err = format!(
        "{:?}",
        caller_class_by_finder_rule(&mut ctx, &none).unwrap_err()
    );
    assert!(err.contains("no caller frame: [null, null]"), "{err}");
}

fn named(class: &str, method: &str, id: u32) -> cratonvm_native_api::StackTraceEntry {
    let mut e = frame(class, id);
    e.method_name = std::sync::Arc::from(method);
    e
}

/// Stage 2 (wave 51, lane L3): `Reflection.getCallerClass` walks past
/// `Method.invoke`, the method accessors and the method-handle adapter
/// frames only -- not a constructor accessor, not a non-adapter
/// `java.lang.invoke` frame -- and answers `None` with no caller left.
#[test]
fn the_security_walk_skips_only_what_hotspot_ignores() {
    // OUTERMOST first; the innermost is the asking `@CallerSensitive` method.
    let trace = [
        named("p/Caller", "run", 1),
        named("java/lang/reflect/Method", "invoke", 2),
        named(
            "jdk/internal/reflect/DirectMethodHandleAccessor",
            "invoke",
            3,
        ),
        named("java/lang/invoke/LambdaForm$MH/0x1", "invoke", 4),
        named("java/lang/invoke/MethodHandle", "linkToStatic", 5),
        named("java/lang/invoke/MethodHandles", "lookup", 6),
    ];
    let accessor = [false, false, true, false, false, false];
    assert_eq!(reflection_caller_index(&trace, &[], &accessor), Some(0));

    // A constructor accessor, `Constructor.newInstance` and a plain
    // `java.lang.invoke` frame answer (HotSpot does not skip them).
    let ctor = [
        named("p/Caller", "run", 1),
        named(
            "jdk/internal/reflect/DirectConstructorHandleAccessor",
            "newInstance",
            2,
        ),
        named("p/Asks", "<init>", 3),
    ];
    assert_eq!(reflection_caller_index(&ctor, &[], &[false; 3]), Some(1));
    let invoke = [
        named("java/lang/invoke/BootstrapMethodInvoker", "invoke", 1),
        named("java/lang/invoke/MethodHandles", "lookup", 2),
    ];
    assert_eq!(reflection_caller_index(&invoke, &[], &[]), Some(0));
    // A hidden `java.lang.invoke` frame is a compiled lambda form.
    let hidden = [
        named("p/Caller", "run", 1),
        named("java/lang/invoke/Invokers", "invokeExact_MT", 2),
        named("p/Asks", "ask", 3),
    ];
    assert_eq!(
        reflection_caller_index(&hidden, &[false, true, false], &[]),
        Some(0)
    );
    // `Method` frames other than `invoke` are not ignored.
    let other = [
        named("java/lang/reflect/Method", "acquireMethodAccessor", 1),
        named("p/A", "a", 2),
    ];
    assert_eq!(reflection_caller_index(&other, &[], &[]), Some(0));
    // Nothing outward of the asking frame.
    assert_eq!(
        reflection_caller_index(&[named("p/A", "a", 1)], &[], &[]),
        None
    );
    assert_eq!(reflection_caller_index(&[], &[], &[]), None);
}

/// The older rule's residue: a capture whose innermost frame is stream
/// plumbing (no `@CallerSensitive` method there) skips the plumbing; one
/// whose innermost frame is `ServiceLoader` (which has caller-sensitive
/// methods) answers its caller as HotSpot does.
#[test]
fn the_security_walk_skips_plumbing_only_for_a_collapsed_capture() {
    let collapsed = [
        named("p/Test", "run", 1),
        named("java/util/ArrayList", "forEach", 2),
        named("java/util/stream/ForEachOps$ForEachOp$OfRef", "accept", 3),
        named("java/util/stream/ForEachOps$ForEachOp$OfRef", "accept", 4),
    ];
    assert_eq!(reflection_caller_index(&collapsed, &[], &[]), Some(1));
    let asked = [
        named("java/util/stream/ReferencePipeline", "forEach", 1),
        named("p/Lambda", "lambda$0", 2),
        named("java/util/ServiceLoader", "load", 3),
    ];
    assert_eq!(reflection_caller_index(&asked, &[], &[]), Some(1));
    assert!(is_method_accessor_class_name(
        "jdk/internal/reflect/CsMethodAccessorAdapter"
    ));
    assert!(!is_method_accessor_class_name(
        "jdk/internal/reflect/DirectConstructorHandleAccessor"
    ));
}

#[test]
fn get_caller_class_returns_non_null_when_classes_loaded() {
    // MockNativeContext will not have a populated trace, but we do
    // exercise the fall-back branch that returns java/lang/Object.
    let mut ctx = MockNativeContext::new();
    // Ensure Object is a loadable class in the mock.
    let _ = ctx.ensure_class_initialized("java/lang/Object");
    let result = native_get_caller_class(&mut ctx, &[]).unwrap().unwrap();
    // Either non-null (fallback fired) or null (Object class not
    // loadable in mock). Both are acceptable; the key invariant is
    // no panic / no error.
    match result {
        Value::Object(_) => {}
        other => panic!("expected Object(…), got {:?}", other),
    }
}

#[test]
fn register_stack_walker_boot_adds_getinstance_variants() {
    use cratonvm_native_api::NativeMethodRegistry;
    let mut r = NativeMethodRegistry::new();
    let before = r.len();
    register_stack_walker_boot(&mut r);
    let after = r.len();
    assert!(
        after >= before + 5,
        "expected at least 5 new registrations, got {}",
        after - before
    );
}
