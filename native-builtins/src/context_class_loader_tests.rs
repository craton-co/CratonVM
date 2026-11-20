use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn explicit_null_context_class_loader_round_trips() {
    let mut ctx = mock_ctx();
    let thread = ctx.fresh_object_ref();

    native_thread_set_context_class_loader(
        &mut ctx,
        &[Value::Object(Some(thread)), Value::Object(None)],
    )
    .expect("setContextClassLoader(null) should not throw");

    let got = native_thread_get_context_class_loader(&mut ctx, &[Value::Object(Some(thread))])
        .expect("getContextClassLoader should not throw");
    assert_eq!(got, Some(Value::Object(None)));
}

/// The zero-quiet-period shutdown must NOT be registered against Netty's
/// event executor groups.
///
/// It was, on the premise that naming `MultiThreadIoEventLoopGroup`
/// restricted it to MongoDB Reactive Streams' driver group — but that is
/// the ordinary group every Netty 4.2 application constructs, so the
/// registration turned every `group.shutdownGracefully()` in the process
/// into `shutdownGracefully(0, 0, MILLISECONDS)`. A zero quiet period does
/// not drain the event loop, and Netty runs channel deregistration (and
/// therefore `handlerRemoved`) as a queued task: `PcapWriteHandler`'s
/// capture came out 522 bytes of 732, with every TCP close packet missing.
///
/// The Mongo lifecycle still gets its zero quiet period — its `destroy()`
/// bridge calls `native_netty_event_executor_group_shutdown_gracefully`
/// directly, which is the scoping that actually holds.
#[test]
fn essential_does_not_register_a_zero_quiet_period_netty_group_shutdown() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    for class_name in [
        "io/netty/util/concurrent/EventExecutorGroup",
        "io/netty/util/concurrent/AbstractEventExecutorGroup",
        "io/netty/channel/MultiThreadIoEventLoopGroup",
    ] {
        assert!(
            registry
                .find(
                    class_name,
                    "shutdownGracefully",
                    "()Lio/netty/util/concurrent/Future;"
                )
                .is_none(),
            "{class_name}.shutdownGracefully() must keep Netty's own quiet period"
        );
    }
    // The Mongo lifecycle bridge that DOES need it is still installed.
    assert!(registry
        .find(
            "org/springframework/boot/mongodb/autoconfigure/MongoReactiveAutoConfiguration$NettyDriverMongoClientSettingsBuilderCustomizer",
            "destroy",
            "()V"
        )
        .is_some());
}

/// JDK 21 declares `Class.getModifiers` and `Class.getSigners` `native`;
/// JDK 25 gives both bodies. A `--real-jdk` registry over a JDK 21 image
/// must keep a non-stub implementation of each (the default launcher's
/// boot died in `EnumSet` without `getModifiers`), while over JDK 25 (or
/// an unknown image) `getModifiers` still yields to the image's body.
#[test]
fn a_jdk21_image_keeps_the_class_natives_jdk25_turned_into_bytecode() {
    use cratonvm_types::compat::CompatibilityMode;
    let registry = |feature: Option<u32>| {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(CompatibilityMode::JdkOnly);
        r.set_real_jdk(true);
        r.set_jdk_feature(feature);
        register_essential_natives(&mut r);
        r
    };
    let jdk21 = registry(Some(21));
    for (name, desc) in [
        ("getModifiers", "()I"),
        ("getSigners", "()[Ljava/lang/Object;"),
    ] {
        let kind = jdk21.kind_of("java/lang/Class", name, desc);
        assert!(
            matches!(kind, Some(NativeKind::Bridge | NativeKind::Intrinsic)),
            "Class.{name} over JDK 21 under --real-jdk: {kind:?}"
        );
    }
    // Over JDK 25 `getSigners` stays a retired shadow: refused, so the
    // image's body runs.
    for feature in [Some(25), None] {
        assert!(
            registry(feature)
                .find("java/lang/Class", "getSigners", "()[Ljava/lang/Object;")
                .is_none(),
            "JDK {feature:?}: getSigners yields to the image's body"
        );
    }
    if lang_class::class_modifiers_jdk_body(&registry(None)) {
        for feature in [Some(25), None] {
            assert!(
                registry(feature)
                    .find("java/lang/Class", "getModifiers", "()I")
                    .is_none(),
                "JDK {feature:?}: getModifiers yields to the image's body"
            );
        }
    }
}

/// JIT round 19 wave 1, lane classshape (BUF18-6): `Class.isArray` /
/// `isInterface` / `isPrimitive` yield to JDK 25's bodies by default in
/// both modes, stay registered under `CRATONVM_CLASS_SHAPE_JDK_BODY=0`, and
/// never yield over a JDK 21 image (where they are `native`).
/// Lane classshape's CLASSSHAPE19-1: `getComponentType` / `componentType`
/// yield to JDK 25's bodies by default in both modes, and stay registered
/// under `CRATONVM_CLASS_COMPONENT_JDK_BODY=0` and over a JDK 21 image.
/// `descriptorString` is not part of the switch (kept on cost).
#[test]
fn class_component_accessors_yield_to_jdk25_bodies_unless_switched_off() {
    use cratonvm_types::compat::CompatibilityMode;
    const ROWS: [(&str, &str); 2] = [
        ("getComponentType", "()Ljava/lang/Class;"),
        ("componentType", "()Ljava/lang/Class;"),
    ];
    let registry = |mode: CompatibilityMode, feature: Option<u32>| {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(mode);
        r.set_real_jdk(true);
        r.set_jdk_feature(feature);
        register_essential_natives(&mut r);
        r
    };
    for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
        for (env, feature, registered) in [
            (None, Some(25), false),
            (Some("0"), Some(25), true),
            (None, Some(21), true),
        ] {
            let r = cratonvm_types::flags::with_thread_overrides(
                &[(lang_class::CLASS_COMPONENT_JDK_BODY_ENV, env)],
                || registry(mode, feature),
            );
            for (name, desc) in ROWS {
                assert_eq!(
                    r.find("java/lang/Class", name, desc).is_some(),
                    registered,
                    "{mode:?} JDK {feature:?} switch {env:?}: Class.{name}"
                );
            }
            // A `SyntheticStub`, which the strict registry refuses: the
            // control is the `--compatible` registry.
            if mode == CompatibilityMode::Compatible {
                assert!(
                    r.find(
                        "java/lang/Class",
                        "descriptorString",
                        "()Ljava/lang/String;"
                    )
                    .is_some(),
                    "{mode:?}: descriptorString is not part of the switch"
                );
            }
        }
    }
}

#[test]
fn class_shape_predicates_yield_to_jdk25_bodies_unless_switched_off() {
    use cratonvm_types::compat::CompatibilityMode;
    const SHAPE: [&str; 3] = ["isArray", "isInterface", "isPrimitive"];
    let registry = |mode: CompatibilityMode, feature: Option<u32>| {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(mode);
        r.set_real_jdk(true);
        r.set_jdk_feature(feature);
        register_essential_natives(&mut r);
        r
    };
    for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
        let off = cratonvm_types::flags::with_thread_overrides(
            &[(lang_class::CLASS_SHAPE_JDK_BODY_ENV, Some("0"))],
            || registry(mode, Some(25)),
        );
        for name in SHAPE {
            assert!(
                off.find("java/lang/Class", name, "()Z").is_some(),
                "{mode:?}: Class.{name} stays registered with the switch off"
            );
        }
        let on = cratonvm_types::flags::with_thread_overrides(
            &[(lang_class::CLASS_SHAPE_JDK_BODY_ENV, None)],
            || registry(mode, Some(25)),
        );
        for name in SHAPE {
            assert!(
                on.find("java/lang/Class", name, "()Z").is_none(),
                "{mode:?}: Class.{name} yields to JDK 25's body by default"
            );
        }
        // `isEnum` is a `SyntheticStub`, which the strict registry refuses:
        // the control is the `--compatible` registry.
        if mode == CompatibilityMode::Compatible {
            assert!(
                on.find("java/lang/Class", "isEnum", "()Z").is_some(),
                "{mode:?}: isEnum is not part of the switch"
            );
        }
        let jdk21 = cratonvm_types::flags::with_thread_overrides(
            &[(lang_class::CLASS_SHAPE_JDK_BODY_ENV, None)],
            || registry(mode, Some(21)),
        );
        for name in SHAPE {
            assert!(
                jdk21.find("java/lang/Class", name, "()Z").is_some(),
                "{mode:?}: Class.{name} is native on JDK 21 and must stay registered"
            );
        }
    }
}

#[test]
fn essential_define_class2_registration_uses_bytebuffer_handler() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    let callback = registry
        .find(
            "java/lang/ClassLoader",
            "defineClass2",
            "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/nio/ByteBuffer;IILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        )
        .expect("defineClass2 native should be registered");

    let mut ctx = mock_ctx();
    let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    let bb = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    ctx.set_field(bb, 0, Value::Object(Some(bytes)));
    ctx.set_field(bb, 1, Value::Int(0));
    ctx.set_field(bb, 2, Value::Int(4));
    ctx.set_field(bb, 3, Value::Int(4));
    let name = ctx.create_string("com/example/Foo");

    let result = callback(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(Some(name)),
            Value::Object(Some(bb)),
            Value::Int(0),
            Value::Int(4),
            Value::Object(None),
            Value::Object(None),
        ],
    );

    match result {
        Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Linkage(
                cratonvm_types::error::LinkageError::ClassFormatError { message, .. },
            ),
        )) => assert!(
            message.contains("defineClass2"),
            "expected defineClass2 ByteBuffer handler failure, got {message}"
        ),
        other => panic!("defineClass2 did not dispatch to the ByteBuffer handler: {other:?}"),
    }
}

#[test]
fn essential_natives_include_real_jdk_symbol_lookup_bridge() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    let sl = "java/lang/foreign/SymbolLookup";

    assert!(registry
        .find(sl, "find", "(Ljava/lang/String;)Ljava/util/Optional;")
        .is_some());
    assert!(registry
        .find(
            sl,
            "libraryLookup",
            "(Ljava/lang/String;Ljava/lang/foreign/Arena;)Ljava/lang/foreign/SymbolLookup;",
        )
        .is_some());
    assert!(registry
        .find(sl, "loaderLookup", "()Ljava/lang/foreign/SymbolLookup;")
        .is_some());
}

#[test]
fn essential_natives_include_stamped_lock_view_unlock_bridges() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    let stamped_lock = "java/util/concurrent/locks/StampedLock";

    assert!(registry
        .find(stamped_lock, "unstampedUnlockWrite", "()V")
        .is_some());
    assert!(registry
        .find(stamped_lock, "unstampedUnlockRead", "()V")
        .is_some());
}

#[test]
fn essential_natives_include_jdk17_stacktraceelements_bridge() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    let ste = "java/lang/StackTraceElement";

    assert!(registry
        .find(
            ste,
            "initStackTraceElements",
            "([Ljava/lang/StackTraceElement;Ljava/lang/Object;I)V",
        )
        .is_some());
    assert!(registry
        .find(
            ste,
            "initStackTraceElements",
            "([Ljava/lang/StackTraceElement;Ljava/lang/Throwable;)V",
        )
        .is_some());
}

/// `jdk/internal/misc/VM.latestUserDefinedLoader0()` must be registered in
/// a **default-feature** build. Real-JDK `ObjectInputStream.readObject()`
/// reaches it on every non-proxy class via `readClassDesc()` →
/// `resolveClass()` → `latestUserDefinedLoader()`, so an unregistered
/// native is an `UnsatisfiedLinkError` on every Java deserialization.
///
/// This is the runtime half of the guard: it proves the native is in the
/// registry that `register_essential_natives` actually builds, which a
/// source scan cannot. Its "default build" claim holds when the test is
/// compiled under a `-p cratonvm-native-builtins`-scoped resolve. Since
/// interpreter wave 99 (`i86-L2-synthetic-serialization-stand-in-is-not-hotspots`)
/// neither `libcratonvm` nor `cratonvm-embed`'s `vm-experimental` turns
/// `experimental-serialization` on, so a bare `cargo test --workspace` no
/// longer unifies it on either; `cargo test -p cratonvm-native-builtins
/// --lib` (CI's "Default-feature native registry surface" step) stays the
/// scoped signal against any other workspace member that enables it.
/// The cfg-detecting half lives in
/// `vm/tests/t14_system_conformance.rs::t14_vm_natives_not_feature_gated`,
/// which reads source text and is therefore feature-independent.
///
/// Note there is no `#[cfg(feature = ...)]` on this test — if one ever
/// appears, it stops proving anything.
#[test]
fn essential_registers_latest_user_defined_loader0_in_default_build() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);

    assert!(
        registry
            .find(
                "jdk/internal/misc/VM",
                "latestUserDefinedLoader0",
                "()Ljava/lang/ClassLoader;",
            )
            .is_some(),
        "VM.latestUserDefinedLoader0 is not registered in a default-feature \
         build; every ObjectInputStream.readObject() of an ordinary class \
         will throw UnsatisfiedLinkError. Check for a #[cfg(feature = ...)] \
         on the registration in register_essential_natives_with_shims.",
    );
}

/// `CallSite.setTargetNormal` / `setTargetVolatile` are JDK bytecode that
/// ends in these two bare natives, and concrete bytecode beats the
/// call-site Bridges, so nothing else can stand in for them: a missing
/// registration is `UnsatisfiedLinkError` on every `MutableCallSite` /
/// `VolatileCallSite` `setTarget` (Groovy `IndyInterface`).
#[test]
fn essential_registers_the_call_site_target_setters() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    for name in ["setCallSiteTargetNormal", "setCallSiteTargetVolatile"] {
        assert!(
            registry
                .find(
                    "java/lang/invoke/MethodHandleNatives",
                    name,
                    "(Ljava/lang/invoke/CallSite;Ljava/lang/invoke/MethodHandle;)V",
                )
                .is_some(),
            "MethodHandleNatives.{name} is not registered"
        );
    }
}

/// `String.regionMatches` is an `Intrinsic` leaf only in a VM without a JIT
/// (`startsWith`-class calls 205-330 ns native, 24-41 ns as compiled
/// bytecode); the `string-region-matches` row of
/// `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` registers it in a JIT VM too.
#[test]
fn region_matches_registers_without_a_jit_only() {
    let c = "java/lang/String";
    let rows = ["(ZILjava/lang/String;II)Z", "(ILjava/lang/String;II)Z"];
    let build = |jit: bool, row: &str| {
        let mut r = NativeMethodRegistry::new();
        r.set_jit_enabled(jit);
        r.set_intrinsic_body_compiles_rows(row);
        register_string_region_matches(&mut r);
        r
    };
    let (jit, nojit, with_row) = (build(true, ""), build(false, ""), build(true, "string-region-matches"));
    for d in rows {
        assert!(jit.find(c, "regionMatches", d).is_none(), "{d} registered with a JIT");
        assert_eq!(nojit.kind_of(c, "regionMatches", d), Some(NativeKind::Intrinsic), "{d} without a JIT");
        assert_eq!(with_row.kind_of(c, "regionMatches", d), Some(NativeKind::Intrinsic), "{d} with the row");
        assert!(with_row.is_body_compilable_intrinsic(c, "regionMatches", d), "{d} body-compilable");
    }
}

#[test]
fn replace_charseq_registers_without_a_jit_only() {
    let c = "java/lang/String";
    let d = "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;";
    let build = |jit: bool, row: &str| {
        let mut r = NativeMethodRegistry::new();
        r.set_jit_enabled(jit);
        r.set_intrinsic_body_compiles_rows(row);
        register_essential_natives(&mut r);
        r
    };
    let (jit, nojit, with_row) = (
        build(true, ""),
        build(false, ""),
        build(true, "string-replace-charseq"),
    );
    assert!(jit.find(c, "replace", d).is_none(), "registered with a JIT");
    assert_eq!(nojit.kind_of(c, "replace", d), Some(NativeKind::Intrinsic), "without a JIT");
    assert_eq!(with_row.kind_of(c, "replace", d), Some(NativeKind::Intrinsic), "with the row");
    assert!(with_row.is_body_compilable_intrinsic(c, "replace", d), "body-compilable");
}

/// The rest of the synthetic serialization read/write surface must STAY
/// opt-in. Ungating `latestUserDefinedLoader0` moved one helper into
/// `classloader.rs`; it must not have dragged `serialization.rs`'s
/// `ObjectInputStream`/`ObjectOutputStream` implementations into the
/// default build, where real JDK bytecode owns those classes.
#[cfg(not(any(feature = "experimental-serialization", feature = "synthetic-jdk")))]
#[test]
fn default_build_does_not_register_synthetic_object_stream_natives() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);

    for (class, method, descriptor) in [
        (
            "java/io/ObjectInputStream",
            "readObject",
            "()Ljava/lang/Object;",
        ),
        (
            "java/io/ObjectOutputStream",
            "writeObject",
            "(Ljava/lang/Object;)V",
        ),
    ] {
        assert!(
            registry.find(class, method, descriptor).is_none(),
            "{class}.{method}{descriptor} leaked into the default build - \
             the synthetic serialization path must remain feature-gated",
        );
    }
}

/// The real-JDK essential path must NOT override the `ReflectionFactory`
/// serialization surface. Those 18 methods are ordinary `java.base` /
/// `jdk.unsupported` *bytecode*, not natives, and a differential probe over
/// everything they cover — `readObject()` on a class with no no-arg
/// constructor, on one with private
/// `writeObject`/`readObject`/`readResolve`/`writeReplace` hooks, and on an
/// `Externalizable`; direct `newConstructorForSerialization` (both arities)
/// and `newConstructorForExternalization`; every private-hook accessor and
/// `hasStaticInitializerForSerialization` — is byte-identical to HotSpot
/// JDK 25 with the overrides absent.
///
/// This replaces a test that asserted the opposite under
/// `#[cfg(feature = "experimental-serialization")]`. That test could only
/// run in a resolve no shipping CLI build uses, so it green-lit a
/// registration that was compiled out of every suite binary while being
/// compiled in for CI. Note there is NO `#[cfg]` here: the assertion has to
/// hold in both resolves, which is the entire point.
#[test]
fn essential_path_does_not_override_reflection_factory_serialization() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);

    for (class, method, descriptor) in [
        (
            "sun/reflect/ReflectionFactory",
            "getReflectionFactory",
            "()Lsun/reflect/ReflectionFactory;",
        ),
        (
            "sun/reflect/ReflectionFactory",
            "readObjectForSerialization",
            "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        ),
        (
            "jdk/internal/reflect/ReflectionFactory",
            "newConstructorForSerialization",
            "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
        ),
        (
            "jdk/internal/reflect/ReflectionFactory",
            "hasStaticInitializerForSerialization",
            "(Ljava/lang/Class;)Z",
        ),
    ] {
        assert!(
            registry.find(class, method, descriptor).is_none(),
            "{class}.{method}{descriptor} is registered on the real-JDK \
             essential path; that path must run the JDK's own bytecode for \
             the ReflectionFactory serialization surface. If this override \
             is genuinely needed, it must be added ungated — see the \
             comment in register_essential_natives_with_shims.",
        );
    }
}
