#[allow(unused_imports)]
use super::super::*;

/// proxy-real-classfile increment 6 — `InvocationHandler.invokeDefault(Object
/// proxy, Method method, Object... args)` (JDK 16+, a `static` interface
/// method).
///
/// The real JDK body drives `Proxy.invokeDefault`, which reflects the generated
/// proxy class's `proxyClassLookup` accessor + a full-power
/// `MethodHandles.Lookup` to bind an `invokespecial` MethodHandle to the
/// interface's default body. CratonVM's generated `$ProxyN` emits no
/// `proxyClassLookup` accessor (and the proxy model has no real per-class
/// Lookup), so the real bytecode throws
/// `InternalError: NoSuchMethodException: proxyClassLookup`. This native is
/// force-dispatched over the real bytecode (see
/// `interpreter::force_native_over_real_jdk_bytecode`) and runs the default body
/// directly via [`NativeContext::invoke_special`] — the same super-call dispatch
/// (`Lookup.findSpecial` semantics) the JDK ultimately performs, minus the
/// Lookup plumbing. Nested *virtual* calls on the proxy inside the default body
/// still route back through the `InvocationHandler` (invoke_special only
/// bypasses the retarget for the single *resolved* method).
///
/// Args (static method): `[0]` proxy, `[1]` `Method`, `[2]` `Object[]` args
/// (may be `null` for a no-arg default method).
pub fn native_invocation_handler_invoke_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;

    let proxy = match args.first() {
        Some(Value::Object(Some(p))) => *p,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("InvocationHandler.invokeDefault: proxy is null".to_string()),
            }
            .into());
        }
    };
    let method_obj = match args.get(1) {
        Some(Value::Object(Some(m))) => *m,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("InvocationHandler.invokeDefault: method is null".to_string()),
            }
            .into());
        }
    };

    // JDK contract: the receiver must be a proxy instance, i.e.
    // `Proxy.isProxyClass(proxy.getClass())`: a user class that itself extends
    // `java.lang.reflect.Proxy` is refused (round 13 wave 6, lane proxy4;
    // `proxy_class_is_generated` carries `CRATONVM_PROXY_USER_SUBCLASS_ORDINARY`).
    if !crate::reflect_annotations::proxy_class_is_generated(ctx, ctx.class_id_of_object(proxy)) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "'proxy' is not a proxy instance".to_string(),
        }
        .into());
    }

    // JDK contract: the method must be a default method — public, non-abstract,
    // non-static, declared on an interface (mirrors `Method.isDefault()`).
    const ACC_PUBLIC: i32 = 0x0001;
    const ACC_STATIC: i32 = 0x0008;
    const ACC_ABSTRACT: i32 = 0x0400;
    let modifiers = match ctx.get_field_by_name(method_obj, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let declaring_mirror = match ctx.get_field_by_name(method_obj, "clazz") {
        Value::Object(Some(m)) => m,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "InvocationHandler.invokeDefault: method has no declaring class"
                    .to_string(),
            }
            .into());
        }
    };
    let decl_is_interface = ctx
        .class_id_from_mirror(declaring_mirror)
        .map(|cid| ctx.is_interface_class(cid))
        .unwrap_or(false);
    let is_default =
        decl_is_interface && (modifiers & (ACC_ABSTRACT | ACC_PUBLIC | ACC_STATIC)) == ACC_PUBLIC;
    if !is_default {
        // The JDK's message is `"\"" + method + "\" is not a default method"`
        // (SBR wave 6, lane tail: the native said `method is not a default
        // method`, which HotSpot never does; measured against HotSpot 25).
        let described =
            match ctx.invoke_virtual(method_obj, "toString", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                _ => None,
            };
        return Err(RuntimeError::IllegalArgumentException {
            message: match described {
                Some(m) => format!("\"{m}\" is not a default method"),
                None => "method is not a default method".to_string(),
            },
        }
        .into());
    }

    let iface_name = match crate::lang_class::mirror_class_name(ctx, declaring_mirror) {
        Some(n) => n,
        None => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "InvocationHandler.invokeDefault: cannot resolve declaring interface"
                    .to_string(),
            }
            .into());
        }
    };
    let method_name = match ctx.get_field_by_name(method_obj, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let descriptor = crate::lang_class::method_descriptor_for_invoke(ctx, method_obj);
    let (param_descs, ret_desc) = crate::lang_class::parse_descriptor_param_and_return(&descriptor);

    // Unbox the `Object[]` args to match the default method's primitive /
    // reference parameter types, prepending the proxy receiver (which is
    // `invoke_special`'s `args[0]`).
    let args_arr = match args.get(2) {
        Some(Value::Object(Some(a))) => Some(*a),
        _ => None,
    };
    let supplied = match args_arr {
        Some(a) => ctx.array_length(a),
        None => 0,
    };
    if supplied != param_descs.len() {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!(
                "InvocationHandler.invokeDefault: wrong number of arguments for {}.{}: \
                 expected {}, got {}",
                iface_name,
                method_name,
                param_descs.len(),
                supplied
            ),
        }
        .into());
    }
    let mut invoke_args: Vec<Value> = Vec::with_capacity(param_descs.len() + 1);
    invoke_args.push(Value::Object(Some(proxy)));
    for (i, pdesc) in param_descs.iter().enumerate() {
        let raw = match args_arr {
            Some(a) => ctx.get_array_element(a, i),
            None => Value::Object(None),
        };
        invoke_args.push(crate::lang_class::unbox_arg(ctx, raw, pdesc));    }

    // Run the interface's default body as an invokespecial super-call — exactly
    // `I.m(...)`, with no virtual retarget back to the proxy's intercepted
    // dispatch. A thrown exception propagates verbatim (invokeDefault declares
    // `throws Throwable`; it does NOT apply UndeclaredThrowableException
    // wrapping — that is only on the proxy-method dispatch path).
    let result = ctx.invoke_special(&iface_name, &method_name, &descriptor, &invoke_args)?;

    // `invokeDefault` returns `Object` — box a primitive result per the return
    // type; reference / void results pass through unchanged.
    match result {
        Some(val) => Ok(Some(crate::lang_class::box_value(ctx, val, &ret_desc))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// real-cdi-bean-container — Agroal datasource shim gate (Keycloak Gap 8).
///
/// When ON, the `agroal_pool.rs` native shim is **not** registered, so
/// Quarkus/Keycloak run the **real** `io.agroal.pool.DataSource` /
/// `ConnectionPool` / `AgroalDataSourceConfigurationSupplier` bytecode over the
/// (working) real `org.h2.Driver`. The shim's
/// `AgroalDataSourceConfigurationSupplier.get()` native returns a synthetic
/// object typed as the bare interface `AgroalDataSourceConfiguration` (no
/// method bodies); the real downstream `DataSourceProvider.getDataSource`
/// bytecode then does `invokeinterface config.dataSourceImplementation()` and
/// hits the abstract method → `AbstractMethodError`. So the shim and the real
/// container bytecode are mutually exclusive on this path.
///
/// Currently **opt-in** (`CRATONVM_REAL_AGROAL=1`), default = the legacy shim,
/// while the real pool's concurrency (housekeeping executor + AQS synchronizer)
/// is being validated. Once the real path is green by default this flips to
/// opt-out (`CRATONVM_SYNTHETIC_AGROAL`), mirroring the AQS / Spring-startup
/// precedents.
pub fn real_agroal() -> bool {
    crate::nbflags().real_agroal && !crate::nbflags().synthetic_agroal
}

/// Vert.x / Netty event loop (Keycloak Gap 9). Under `CRATONVM_REAL_VERTX` the
/// synthetic `vertx_eventloop` natives (`VertxImpl.init`, `NioEventLoop.run` /
/// `execute` / `schedule` / ...) are suppressed so the REAL Netty
/// `NioEventLoopGroup` / `SingleThreadEventExecutor` / `NioEventLoop.run()`
/// bytecode runs, driving `Selector.select()` over CratonVM's own selector (see
/// the `WEPollSelectorProvider` routing in `native-io`). The synthetic loop runs
/// only tasks/timers and does NOT poll the selector, so a real Vert.x HTTP server
/// binds but never accepts. Opt-in while the real path is validated; flips to
/// opt-out (`CRATONVM_SYNTHETIC_VERTX`) once green by default, mirroring `real_agroal`.
pub fn real_vertx() -> bool {
    crate::nbflags().real_vertx && !crate::nbflags().synthetic_vertx
}

/// Outcome of [`define_or_get_proxy_class`]. Distinguishes the *intended*
/// gate-off degrade (use the synthetic shim) from a *genuine* generation
/// failure, so STRICT mode ([`real_proxy_strict`]) can surface the latter as a
/// real JDK exception rather than masking it with the shim.
pub enum ProxyClassOutcome {
    /// The real generated `$ProxyN` class (canonical path).
    Real(cratonvm_types::ClassId),
    /// `CRATONVM_REAL_PROXY=0` — the synthetic shim is the intended path.
    Degrade,
    /// A concrete spec/emit/define failure. `&'static str` is the stage, for
    /// the exception message + the `CRATONVM_DBG_PROXY` audit.
    Failed(&'static str),
}

/// WP2.5 v3 — parse a JVMS method descriptor's parameter list into
/// internal class names. Primitives map to their wrapper internal
/// names (the emitter then rewrites those to `<Wrapper>.TYPE` GETSTATIC
/// so `Class.getMethod` matches the primitive `Class<?>`).
///
///   `I` → `java/lang/Integer`         `J` → `java/lang/Long`
///   `Z` → `java/lang/Boolean`         `F` → `java/lang/Float`
///   `B` → `java/lang/Byte`            `D` → `java/lang/Double`
///   `C` → `java/lang/Character`
///   `S` → `java/lang/Short`
///   `Lfoo/Bar;`     → `foo/Bar`       (plain reference)
///   `[I`            → `[I`            (array — kept as descriptor)
///   `[[Lfoo/Bar;`   → `[[Lfoo/Bar;`
pub fn parse_param_class_names(descriptor: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = descriptor.as_bytes();
    let mut i = match bytes.first() {
        Some(b'(') => 1,
        _ => return out,
    };
    while i < bytes.len() && bytes[i] != b')' {
        let start = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'L' => {
                let name_start = i + 1;
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                if i >= bytes.len() {
                    break;
                }
                if start == name_start - 1 {
                    out.push(descriptor[name_start..i].to_string());
                } else {
                    out.push(descriptor[start..=i].to_string());
                }
                i += 1;
            }
            b'I' | b'Z' | b'B' | b'C' | b'S' | b'J' | b'F' | b'D' => {
                if start == i {
                    let wrapper = match bytes[i] {
                        b'I' => "java/lang/Integer",
                        b'Z' => "java/lang/Boolean",
                        b'B' => "java/lang/Byte",
                        b'C' => "java/lang/Character",
                        b'S' => "java/lang/Short",
                        b'J' => "java/lang/Long",
                        b'F' => "java/lang/Float",
                        b'D' => "java/lang/Double",
                        _ => unreachable!(),
                    };
                    out.push(wrapper.to_string());
                } else {
                    out.push(descriptor[start..=i].to_string());
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

// ===========================================================================
// Phases 49-50: Enterprise Final — AtomicBoolean, Modifier extras, Class extras,
// CompletableFuture, Executors, Formatter
// ===========================================================================

pub fn register_enterprise_final_natives(registry: &mut NativeMethodRegistry) {
    // --- AtomicBoolean (1-field synthetic: value=0 Int) ---
    register_atomic_boolean_natives(registry);

    // --- AtomicStampedReference (2-field: ref=0, stamp=1) ---
    register_atomic_stamped_ref_natives(registry);

    // --- AtomicMarkableReference (2-field: ref=0, mark=1) ---
    register_atomic_markable_ref_natives(registry);

    // --- Modifier extras (remaining bit checks) ---
    let m = "java/lang/reflect/Modifier";
    registry.register(m, "isSynchronized", "(I)Z", native_modifier_is_synchronized, NativeKind::SyntheticStub);
    registry.register(m, "isVolatile", "(I)Z", native_modifier_is_volatile, NativeKind::SyntheticStub);
    registry.register(m, "isTransient", "(I)Z", native_modifier_is_transient, NativeKind::SyntheticStub);
    registry.register(m, "isNative", "(I)Z", native_modifier_is_native, NativeKind::SyntheticStub);
    registry.register(m, "isInterface", "(I)Z", native_modifier_is_interface, NativeKind::SyntheticStub);
    registry.register(m, "isStrict", "(I)Z", native_modifier_is_strict, NativeKind::SyntheticStub);
    registry.register(
        m,
        "toString",
        "(I)Ljava/lang/String;",
        native_modifier_to_string, NativeKind::SyntheticStub,
    );
    registry.register(m, "classModifiers", "()I", native_modifier_class_modifiers, NativeKind::SyntheticStub);
    registry.register(
        m,
        "interfaceModifiers",
        "()I",
        native_modifier_interface_modifiers, NativeKind::SyntheticStub,
    );
    registry.register(m, "fieldModifiers", "()I", native_modifier_field_modifiers, NativeKind::SyntheticStub);
    registry.register(
        m,
        "methodModifiers",
        "()I",
        native_modifier_method_modifiers, NativeKind::SyntheticStub,
    );
    registry.register(
        m,
        "constructorModifiers",
        "()I",
        native_modifier_constructor_modifiers, NativeKind::SyntheticStub,
    );

    // --- Class extras (additional methods not already registered) ---
    let c = "java/lang/Class";
    registry.register(
        c,
        "getComponentType",
        "()Ljava/lang/Class;",
        native_class_get_component_type, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "componentType",
        "()Ljava/lang/Class;",
        native_class_get_component_type, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "getPackageName",
        "()Ljava/lang/String;",
        native_class_get_package_name, NativeKind::SyntheticStub,
    );
    // T19_H10_GET_PACKAGE: real `Class.getPackage()` — synthesises a
    // `java.lang.Package` whose `name`, `implementation*`, and
    // `specification*` fields are populated from the class's source jar
    // manifest. Was `native_return_null`, which broke any code that did
    // `Foo.class.getPackage().getImplementationVersion()` on `<clinit>`.
    registry.register(
        c,
        "getPackage",
        "()Ljava/lang/Package;",
        native_class_get_package, NativeKind::SyntheticStub,
    );

    // T19_H11 — moved to `register_essential_natives` so the override
    // is active in real-JDK mode (this `register_synthetic_overrides`
    // function is only called under the `synthetic-jdk` feature gate).
    registry.register(c, "isEnum", "()Z", native_class_is_enum, NativeKind::SyntheticStub);
    registry.register(c, "isRecord", "()Z", native_class_is_record, NativeKind::SyntheticStub);
    registry.register(c, "isSealed", "()Z", native_class_is_sealed, NativeKind::SyntheticStub);
    registry.register(c, "isSynthetic", "()Z", native_class_is_synthetic, NativeKind::SyntheticStub);
    // The nesting predicates are derived from the class's own `InnerClasses`
    // entry (see `lang_class::class_nesting_kind`). They were `native_return_false`,
    // so under `synthetic-jdk` every nested class reported itself top-level —
    // `isMemberClass()` in particular is false for NO nested class on HotSpot.
    // Real-JDK mode never reaches here: it runs `java.lang.Class`'s own
    // bytecode, which was already correct on all 19 shapes of the
    // nesting/enclosing differential probe.
    registry.register(
        c,
        "isAnonymousClass",
        "()Z",
        native_class_is_anonymous_class, NativeKind::SyntheticStub,
    );
    registry.register(c, "isLocalClass", "()Z", native_class_is_local_class, NativeKind::SyntheticStub);
    registry.register(c, "isMemberClass", "()Z", native_class_is_member_class, NativeKind::SyntheticStub);
    // NEW-8: Class.isHidden real impl — was native_return_false which
    // shadowed the registration at lib.rs:~325. Both now consult the
    // class's hidden flag through NativeContext::is_class_hidden.
    registry.register(c, "isHidden", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let hidden = match lang_class::mirror_class_id(ctx, this) {
            // Lambda proxies (id >= 0x8000_0000) are JVM hidden classes in HotSpot
            // (JDK 15+). Reporting them as hidden makes `Class.getCanonicalName()`
            // return null (matching HotSpot) instead of leaking the synthesized
            // lambda name. bug-06 fam5 #1.
            Some(cid) if cid.as_u32() >= 0x8000_0000 && ctx.lambda_proxy_host(cid).is_some() => {
                true
            }
            Some(cid) => ctx.is_class_hidden(cid),
            None => false,
        };
        Ok(Some(Value::Int(if hidden { 1 } else { 0 })))
    }, NativeKind::SyntheticStub);
    registry.register(
        c,
        "getCanonicalName",
        "()Ljava/lang/String;",
        native_class_get_canonical_name, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "getTypeName",
        "()Ljava/lang/String;",
        lang_class::native_class_get_type_name_exact,
        NativeKind::Intrinsic,
    );
    // WP2.1-class-modern: route the public `getEnclosing*` accessors to
    // their real impls in `lang_reflect`. Previously these were
    // `native_return_null` — fine for "doesn't crash" but breaks
    // ByteBuddy / Hibernate scanners that rely on
    // `Inner.class.getEnclosingClass() == Outer.class`.
    // The proper natives consult the InnerClasses / EnclosingMethod
    // attributes via `NativeContext::inner_classes` / `enclosing_method`
    // (see `native_class_get_enclosing_class` in lang_reflect.rs:205).
    //
    // `register_wp2_1_natives` (called inside `register_essential_natives`)
    // installs these earlier; the assignments here would clobber them
    // back to null-stubs because `register_synthetic_overrides` runs
    // *after* essential. Keep both consistent.
    registry.register(
        c,
        "getEnclosingClass",
        "()Ljava/lang/Class;",
        lang_reflect::native_class_get_enclosing_class, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "getEnclosingMethod",
        "()Ljava/lang/reflect/Method;",
        lang_reflect::native_class_get_enclosing_method_public, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "getEnclosingConstructor",
        "()Ljava/lang/reflect/Constructor;",
        lang_reflect::native_class_get_enclosing_constructor_public, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        lang_class::native_class_get_declaring_class, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "getNestHost",
        "()Ljava/lang/Class;",
        native_class_get_nest_host, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "getEnumConstants",
        "()[Ljava/lang/Object;",
        native_class_get_enum_constants,
        NativeKind::Intrinsic,
    );
    // Three JDK enums whose constants the synthetic model did not carry.
    // `--synthetic-jdk` only, for the reason `native_enum_value_of` states: in
    // real-JDK mode the real `<clinit>` and `values()` bytecode run and a
    // registration here would only shadow them. `<clinit>` is additionally
    // runtime-gated inside `publish_synthetic_enum_constants` on
    // `is_class_synthetic_stub`, so a feature-enabled binary in real-JDK mode
    // leaves the JDK's own interned constants alone.
    //
    // The constant lists live beside the publisher in `lang_class`, and must
    // stay identical to `class_manager.rs`'s `enum_constant_fields` rows — that
    // side declares the statics, this side fills them, and declared-but-unfilled
    // is a NULL constant, which is worse than the `NoSuchFieldError` it
    // replaces.
    #[cfg(feature = "synthetic-jdk")]
    {
        registry.register(
            "java/time/DayOfWeek",
            "<clinit>",
            "()V",
            crate::lang_class::day_of_week_clinit, NativeKind::SyntheticStub,
        );
        registry.register(
            "java/time/DayOfWeek",
            "values",
            "()[Ljava/time/DayOfWeek;",
            crate::lang_class::day_of_week_values, NativeKind::SyntheticStub,
        );
        registry.register(
            "java/net/http/HttpClient$Version",
            "<clinit>",
            "()V",
            crate::lang_class::http_version_clinit, NativeKind::SyntheticStub,
        );
        registry.register(
            "java/net/http/HttpClient$Version",
            "values",
            "()[Ljava/net/http/HttpClient$Version;",
            crate::lang_class::http_version_values, NativeKind::SyntheticStub,
        );
        registry.register(
            "java/net/http/HttpClient$Redirect",
            "<clinit>",
            "()V",
            crate::lang_class::http_redirect_clinit, NativeKind::SyntheticStub,
        );
        registry.register(
            "java/net/http/HttpClient$Redirect",
            "values",
            "()[Ljava/net/http/HttpClient$Redirect;",
            crate::lang_class::http_redirect_values, NativeKind::SyntheticStub,
        );
    }

    // `Enum.valueOf(Class, String)` — synthetic-JDK only. See
    // `lang_class::native_enum_value_of`: `java/lang/Enum`'s natives were
    // retired against the real-JDK suite, where the real bytecode serves them,
    // and this mode has none. Registered here beside the constant source it
    // shares so the two stay together.
    #[cfg(feature = "synthetic-jdk")]
    registry.register(
        "java/lang/Enum",
        "valueOf",
        "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Enum;",
        crate::lang_class::native_enum_value_of, NativeKind::SyntheticStub,
    );
    // Package-private `Class.getEnumConstantsShared()` — backs
    // `EnumMap.getKeyUniverse` via `SharedSecrets.getJavaLangAccess()`.
    // The real JDK implements it in Java using reflection (getMethod +
    // invoke on `values()`), but our reflection stack still returns null
    // for synthetic enum `values()` methods, so `EnumMap.<init>(Class)`
    // sees a null array and throws NPE on `arraylength`.  Override with
    // the same native so EnumMap / StreamOpFlag.<clinit> succeed.
    registry.register(
        c,
        "getEnumConstantsShared",
        "()[Ljava/lang/Object;",
        native_class_get_enum_constants,
        NativeKind::Intrinsic,
    );
    // See `lang_class::reflection_jdk_bodies` (the JDK body runs).
    if !lang_class::reflection_jdk_bodies(registry) {
        registry.register(c, "desiredAssertionStatus", "()Z", native_assertion_status, NativeKind::SyntheticStub);
    }
    registry.register(
        c,
        "cast",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_class_cast, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "asSubclass",
        "(Ljava/lang/Class;)Ljava/lang/Class;",
        native_class_as_subclass, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        lang_class::native_class_get_class_loader, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "toGenericString",
        "()Ljava/lang/String;",
        native_class_get_type_name, NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "descriptorString",
        "()Ljava/lang/String;",
        native_class_descriptor_string, NativeKind::SyntheticStub,
    );

    // --- Formatter (basic) ---
    register_formatter_natives(registry);

    // Note: StringWriter and StringReader already registered in io.rs

    // --- Collections extras: unmodifiable wrappers ---
    register_collections_extras_natives(registry);
    register_core_stdlib_extras(registry);
    // `register_scanner_natives` used to be called here. Its twelve
    // `java/util/Scanner` registrations were overwritten by `native-io`'s two
    // lines later in the boot sequence, in every configuration; see the note
    // where it used to live in `phases_early.rs`.

    // Note: CompletableFuture, Executors, Locale, Charset already registered in earlier phases;
}

// ===========================================================================
// AtomicBoolean — 1-field synthetic (value=0 Int, 0 or 1)
// ===========================================================================

pub const AB_FIELD_VALUE: usize = 0;

// ===========================================================================
// AtomicStampedReference — JDK-faithful single `pair` field (slot 0) that
// holds an `AtomicStampedReference$Pair { reference@0, stamp@1 }`.
//
// The real class declares exactly ONE instance field (`private volatile
// Pair<V> pair`) plus a static `PAIR` VarHandle, so the allocated object has
// num_slots==1. The old intrinsic stored `stamp` directly at slot 1, which is
// out-of-bounds on a 1-slot object → the GC guard dropped the write and
// `getStamp` read garbage. Worse, every public ASR method (`getReference`,
// `getStamp`, `get`, `compareAndSet`, `set`, `attemptStamp`) reads
// `this.pair.reference` / `this.pair.stamp`, so any of these methods that the
// intrinsic does NOT intercept (e.g. `casPair`, or a future method) runs real
// bytecode against slot 0 expecting a Pair — finding a bare reference instead
// and reading bogus interior fields (the source of the stray `index=3`
// out-of-bounds writes during boot). Modelling the real Pair object keeps the
// intrinsic and any real bytecode perfectly consistent.
// ===========================================================================

pub const ASR_PAIR_CLASS: &str = "java/util/concurrent/atomic/AtomicStampedReference$Pair";

/// Allocate a real-layout `pair_class` (`AtomicStampedReference$Pair` /
/// `AtomicMarkableReference$Pair`) holding `(reference, second)` and install
/// it as `this`'s single `pair` field (slot 0). Slot 0 of the Pair = reference
/// (Object), slot 1 = stamp (int) / mark (boolean as Int 0/1), matching the
/// JDK field order so real bytecode reading `pair.reference` / `pair.stamp` /
/// `pair.mark` stays consistent with the intrinsic.
///
/// gc-common w21-a: the receiver and the reference are ROOTED across the
/// Pair's allocation and stored at their current addresses. The old
/// `asr_alloc_pair` / `amr_alloc_pair` allocated the Pair and then stored the
/// reference -- a raw copy out of `args` or out of the old Pair -- into it at
/// its pre-allocation address, and every caller then stored the Pair through
/// the receiver's pre-allocation address.
pub fn install_atomic_ref_pair(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    pair_class: &str,
    reference: Value,
    second: Value,
) -> Result<(), MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let reference_h = crate::lang_misc::w19e_root_value(&mut scope, reference);
    // `alloc_concurrent_synthetic` resolves the real Pair class (2 fields) when
    // loadable and falls back to a 2-field synthetic class otherwise, so the
    // header's declared field count always matches the 2 slots we write.
    let pair = try_alloc_concurrent_synthetic(&mut *scope, pair_class, 2)?;
    let reference = crate::lang_misc::w19e_rooted_value(&scope, reference, reference_h.as_ref());
    scope.set_field(pair, 0, reference);
    scope.set_field(pair, 1, second);
    let this = scope.get(&this_h);
    scope.set_field(this, 0, Value::Object(Some(pair)));
    Ok(())
}

/// [`install_atomic_ref_pair`] for `AtomicStampedReference`.
pub fn asr_install_pair(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    reference: Value,
    stamp: i32,
) -> Result<(), MethodCallFailed> {
    install_atomic_ref_pair(ctx, this, ASR_PAIR_CLASS, reference, Value::Int(stamp))
}

/// Read `(reference, stamp)` out of the `pair` stored in `this.field(0)`.
/// Returns `(Value::Object(None), 0)` if the pair slot is null or somehow not
/// a 2-field object (defensive — should not happen after construction).
pub fn asr_read_pair(ctx: &mut dyn NativeContext, this: ObjectRef) -> (Value, i32) {
    match ctx.get_field(this, 0) {
        Value::Object(Some(pair)) => {
            let reference = ctx.get_field(pair, 0);
            let stamp = match ctx.get_field(pair, 1) {
                Value::Int(v) => v,
                _ => 0,
            };
            (reference, stamp)        }
        _ => (Value::Object(None), 0),    }}

// ===========================================================================
// AtomicMarkableReference — JDK-faithful single `pair` field (slot 0) that
// holds an `AtomicMarkableReference$Pair { reference@0, mark@1 }`.
//
// Same bug class as AtomicStampedReference above (see its comment): the real
// class declares exactly ONE instance field (`private volatile Pair<V>
// pair`) plus a static `PAIR` VarHandle, so the allocated object has
// num_slots==1. The old intrinsic stored `mark` directly at slot 1, which is
// out-of-bounds on a 1-slot object -> the GC guard silently dropped the
// write and every subsequent `isMarked`/`compareAndSet`/`attemptMark` read a
// stale mark forever. A caller spinning on `compareAndSet(ref, ref, false,
// true)` waiting for the mark to flip to `true` never observes it and
// busy-loops/never completes — this was the root cause of ES
// `RestClientSingleHostIntegTests` (`testManyAsyncRequests`) timing out:
// Apache httpasyncclient's reactor session-request tracking uses
// `AtomicMarkableReference`. Also: `get(boolean[])` was never registered as
// a native at all, so real bytecode ran unintercepted against slot 0
// expecting a `Pair` — finding the bare corrupted reference stored there by
// `native_amr_set`/`native_amr_init` instead and throwing
// `ClassCastException`. Modelling the real Pair object (as
// AtomicStampedReference already does) keeps the intrinsic and any real
// bytecode perfectly consistent.
// ===========================================================================

pub const AMR_PAIR_CLASS: &str = "java/util/concurrent/atomic/AtomicMarkableReference$Pair";

/// [`install_atomic_ref_pair`] for `AtomicMarkableReference`: the mark is
/// stored as Int 0/1.
pub fn amr_install_pair(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    reference: Value,
    mark: bool,
) -> Result<(), MethodCallFailed> {
    install_atomic_ref_pair(
        ctx,
        this,
        AMR_PAIR_CLASS,
        reference,
        Value::Int(i32::from(mark)),
    )
}

/// Read `(reference, mark)` out of the `pair` stored in `this.field(0)`.
/// Returns `(Value::Object(None), false)` if the pair slot is null or somehow
/// not a 2-field object (defensive — should not happen after construction).
pub fn amr_read_pair(ctx: &mut dyn NativeContext, this: ObjectRef) -> (Value, bool) {
    match ctx.get_field(this, 0) {
        Value::Object(Some(pair)) => {
            let reference = ctx.get_field(pair, 0);
            let mark = match ctx.get_field(pair, 1) {
                Value::Int(v) => v != 0,
                _ => false,
            };
            (reference, mark)
        }
        _ => (Value::Object(None), false),
    }
}

pub fn native_amr_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let r = args.get(1).copied().unwrap_or(Value::Object(None));
    let mark = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    amr_install_pair(ctx, this, r, mark)?;
    Ok(None)
}

pub fn native_amr_get_ref(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (reference, _mark) = amr_read_pair(ctx, this);
    Ok(Some(reference))
}

pub fn native_amr_is_marked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_reference, mark) = amr_read_pair(ctx, this);
    Ok(Some(Value::Int(if mark { 1 } else { 0 })))
}

/// `V get(boolean[] markHolder)` — store the current mark into
/// `markHolder[0]` and return the current reference (matches JDK semantics).
pub fn native_amr_get_with_holder(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (reference, mark) = amr_read_pair(ctx, this);
    if let Some(Value::Object(Some(holder))) = args.get(1) {
        ctx.set_array_element(*holder, 0, Value::Int(if mark { 1 } else { 0 }));
    }
    Ok(Some(reference))
}

pub fn native_amr_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let r = args.get(1).copied().unwrap_or(Value::Object(None));
    let mark = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    // JDK `set` allocates a fresh Pair only when reference or mark differ; we
    // always install a fresh Pair (semantically identical, slightly simpler).
    amr_install_pair(ctx, this, r, mark)?;
    Ok(None)
}

pub fn native_amr_attempt_mark(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected_ref = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_mark = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    let (current_ref, _current_mark) = amr_read_pair(ctx, this);
    if values_ref_equal(current_ref, expected_ref) {
        // Swap in a new Pair carrying the (unchanged) reference + new mark.
        amr_install_pair(ctx, this, current_ref, new_mark)?;
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

pub fn native_amr_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected_ref = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_ref = args.get(2).copied().unwrap_or(Value::Object(None));
    let expected_mark = matches!(args.get(3), Some(Value::Int(v)) if *v != 0);
    let new_mark = matches!(args.get(4), Some(Value::Int(v)) if *v != 0);
    let (current_ref, current_mark) = amr_read_pair(ctx, this);
    if values_ref_equal(current_ref, expected_ref) && current_mark == expected_mark {
        // Install a new Pair only if reference or mark actually changes (the
        // JDK fast-path: when both are identical it skips the casPair entirely).
        if !values_ref_equal(current_ref, new_ref) || current_mark != new_mark {
            amr_install_pair(ctx, this, new_ref, new_mark)?;        }
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

/// Reference equality helper for atomic CAS operations
pub fn values_ref_equal(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Object(Some(ra)), Value::Object(Some(rb))) => ra == rb,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        _ => false,
    }
}

// ===========================================================================
// Modifier extras
// ===========================================================================

// ===========================================================================
// Class extras
// ===========================================================================

// ===========================================================================
// Formatter (basic) — 2-field (dest=0 StringBuilder ref, locale=1)
// ===========================================================================

fn register_formatter_natives(r: &mut NativeMethodRegistry) {
    // census-tag: java.util.Formatter fast-path replicating real JDK bytecode
    // (delegates to the same String.format path) → Intrinsic.
            let c = "java/util/Formatter";
    r.register(c, "<init>", "()V", native_formatter_init, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "<init>",
        "(Ljava/lang/Appendable;)V",
        native_formatter_init_appendable, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // `native_formatter_init` served BOTH `()V` and `(Locale)V` and wrote null
    // into field 1 either way, so a locale handed to the constructor was
    // dropped at construction and `format` had nothing to read. Splitting them
    // is the second half of the W7-34 locale fix; the first half is
    // `native_formatter_format` reading field 1.
    r.register(
        c,
        "<init>",
        "(Ljava/util/Locale;)V",
        native_formatter_init_locale, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "<init>",
        "(Ljava/lang/Appendable;Ljava/util/Locale;)V",
        native_formatter_init_appendable_locale, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/util/Formatter;",
        native_formatter_format, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "toString",
        "()Ljava/lang/String;",
        native_formatter_to_string, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(c, "close", "()V", |ctx, args| {
        // Formatter is 1-field with the underlying Appendable. Delegate close() if it's
        // a Closeable; ignore the result if the target doesn't implement close().
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(target)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(target, "close", "()V", &[]);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "flush", "()V", |ctx, args| {
        // Same: delegate flush() if the underlying Appendable implements Flushable.
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(target)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(target, "flush", "()V", &[]);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "out", "()Ljava/lang/Appendable;", native_formatter_out, cratonvm_native_api::NativeKind::Intrinsic);
    }

/// The `Locale` a `java.util.Formatter` constructed WITHOUT one carries — for
/// the four natives that shadow those constructors.
///
/// # Why the slot may not be left null
///
/// `Formatter()` is `this(Locale.getDefault(Locale.Category.FORMAT), new
/// StringBuilder())` and `Formatter(Appendable)` is
/// `this(Locale.getDefault(Locale.Category.FORMAT), a)`. Both natives wrote
/// `null` there instead, and `format` reads that slot and hands it to
/// `lang_string::native_string_format_locale`, which sees an EXPLICIT null —
/// a request `java.util.Formatter` answers differently from "no locale given"
/// in three separate places (ASCII separators, `Locale.US` date names,
/// `Locale.US` zone names). Two different requests collapsed onto one value, so
/// no amount of correctness at the reading end can separate them.
///
/// Measured on HotSpot 25 with `-Duser.language=tr -Duser.country=TR`, which is
/// what makes the two visible at all:
///
/// | expression | HotSpot 25 |
/// |---|---|
/// | `new Formatter(sb).format("%tb", d)` | `Kas` |
/// | `new Formatter(sb, (Locale) null).format("%tb", d)` | `Nov` |
/// | `new Formatter().format("%,d", 1234567)` | `1.234.567` |
/// | `String.format((Locale) null, "%,d", 1234567)` | `1,234,567` |
/// | `new Formatter(sb).locale()` | `tr_TR` |
/// | `new Formatter(sb, (Locale) null).locale()` | `null` |
///
/// The last pair is the proof that the JDK stores a real `Locale` in the field
/// rather than deciding late: `locale()` is a plain field read.
///
/// # Three rungs, each degrading to the one below
///
/// 1. `Locale.getDefault(Locale.Category.FORMAT)` — the category the JDK's own
///    constructors ask for, and the one that can differ from the base default
///    when `user.language.format` is set or `setDefault(FORMAT, l)` has run.
///    The `Locale$Category.FORMAT` constant is READ as a static field and the
///    overload is called only if that read produced an object: real
///    `Locale.getDefault(Category)` opens with `Objects.requireNonNull`, so
///    handing it a null would trade a wrong locale for an NPE.
/// 2. `Locale.getDefault()` — the base default.
/// 3. `null`, i.e. exactly the previous behaviour. A VM with no reachable
///    `java.util.Locale` (synthetic-JDK configurations that do not register
///    one) keeps the answers it had rather than failing construction.
///
/// Nothing here FABRICATES a `Locale`. `locale_bootstrap` already overrides
/// both `getDefault` overloads with a cached, stable-identity object resolved
/// from `user.language`/`user.country`, so rung 1 runs a native rather than the
/// JDK adapter chain — which is also why it cannot re-enter `String.format`.
pub fn formatter_default_locale(ctx: &mut dyn NativeContext) -> Value {
    let mut category: Option<ObjectRef> = None;
    if let Some(cid) = ctx.class_id_by_name("java/util/Locale$Category") {
        if let Some(idx) = ctx.static_field_index_by_name(cid, "FORMAT") {
            if let Value::Object(Some(o)) = ctx.get_static_field(cid, idx) {
                category = Some(o);
            }
        }
    }
    if let Some(cat) = category {
        if let Ok(Some(v @ Value::Object(Some(_)))) = ctx.invoke(
            "java/util/Locale",
            "getDefault",
            "(Ljava/util/Locale$Category;)Ljava/util/Locale;",
            &[Value::Object(Some(cat))],
        ) {
            return v;
        }
    }
    match ctx.invoke(
        "java/util/Locale",
        "getDefault",
        "()Ljava/util/Locale;",
        &[],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        _ => Value::Object(None),
    }
}

pub fn native_formatter_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Store content as a plain string (field 0)
    let empty = ctx.create_string("");
    ctx.set_field(this, 0, Value::Object(Some(empty)));
    // Slot 1 is the DEFAULT format locale, not null — see
    // [`formatter_default_locale`]. Resolving it runs Java code, so the
    // receiver is pinned across the call: `format` two functions down pins for
    // exactly this reason. Slot 0 is written FIRST so the string just created
    // is reachable from `this` (and therefore rooted) while that runs.
    let pin = ctx.pin_native_root(this);
    let locale = formatter_default_locale(ctx);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    ctx.set_field(this, 1, locale);
    Ok(None)
}

pub fn native_formatter_init_appendable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let appendable = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, 0, appendable);
    // See the `()V` constructor above: slot 1 is the DEFAULT format locale.
    let pin = ctx.pin_native_root(this);
    let locale = formatter_default_locale(ctx);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    ctx.set_field(this, 1, locale);
    Ok(None)
}

/// `Formatter(Locale)` — the locale-carrying no-sink constructor.
///
/// Split out of [`native_formatter_init`], which served this descriptor too
/// and wrote `null` into field 1 regardless, so the locale never survived
/// construction. W7-34-formatter-family-residuals.md.
pub fn native_formatter_init_locale(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: a reference held in a Rust local across an allocating or Java-re-entering
    // call goes stale under a moving collector, and under the Generational
    // non-moving young sweep an unrooted object is ZEROED in place. Pin and
    // re-read. `safe_native_call_impl` truncates `native_pin_roots` when the native
    // returns, so an unmatched pin costs nothing on an error path. See
    // `internal/audits/wide-tranche-triage-20260907.md`.
    // `create_string` allocates, so BOTH the receiver and the locale argument
    // are pre-call addresses at the two stores below.
    let this_pin = ctx.pin_native_root(this);
    let locale_pin = match args.get(1) {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    let empty = ctx.create_string("");
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 0, Value::Object(Some(empty)));
    let locale = match locale_pin {
        Some((p, o)) => Value::Object(Some(ctx.read_native_pin(p, o))),
        None => args.get(1).copied().unwrap_or(Value::Object(None)),
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, locale);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

/// `Formatter(Appendable, Locale)` — declared by neither registrar before
/// W7-34, so `new Formatter(sb, Locale.GERMANY)` ran real JDK bytecode over
/// this two-field synthetic layout.
pub fn native_formatter_init_appendable_locale(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    ctx.set_field(this, 1, args.get(2).copied().unwrap_or(Value::Object(None)));
    Ok(None)
}

pub fn native_formatter_format(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // args[1] = format string, args[2] = Object[] varargs
    let fmt_obj = args.get(1).copied().unwrap_or(Value::Object(None));
    let arr_obj = args.get(2).copied().unwrap_or(Value::Object(None));

    // Field 1 is the receiver's `Locale`; `format(String, Object[])` has no
    // locale argument, so this is the only place one can come from. A null
    // (the `()V` / `(Appendable)V` ctors) means "root defaults", which is
    // exactly what `native_string_format` did unconditionally — so the
    // no-locale path is unchanged. W7-34-formatter-family-residuals.md.
    //
    // GC: same obligation as the real-JDK registrar's copy — resolving a
    // non-null locale runs Java and allocates, so the receiver is pinned across
    // the format call and re-derived from its pin.
    let locale = ctx.get_field(this, 1);
    let this_pin = ctx.pin_native_root(this);
    let format_result = lang_string::native_string_format_locale(ctx, &[locale, fmt_obj, arr_obj])?;
    let formatted_str = match format_result {
        Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap_or_default(),
        _ => String::new(),
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);

    // Append to internal StringBuilder (field 0)
    let sb = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let existing = ctx.read_string(sb).unwrap_or_default();
    let combined = format!("{}{}", existing, formatted_str);
    // gc-common w20-c: the string's allocation can move the receiver, which
    // is written and returned after it.
    let this_pin = ctx.pin_native_root(this);
    let new_str = ctx.create_string(&combined);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(this, 0, Value::Object(Some(new_str)));
    Ok(Some(Value::Object(Some(this))))
}

/// `Formatter.toString()`'s body, which is `a.toString()` on the sink in
/// slot 0 — for the two registrars that each carried a copy of it.
///
/// # `read_string` alone is a defaulting reader
///
/// Both copies were `ctx.read_string(sink).unwrap_or_default()`.
/// `NativeContext::read_string` returns `None` for any object whose class is
/// known and is not `java/lang/String` (vm_exec.rs, a deliberate guard against
/// the structural reader decoding a `StringBuilder`'s full char[] capacity), so
/// `unwrap_or_default()` turned every non-`String` sink into the EMPTY STRING —
/// a plausible answer, silently wrong, and reached by the single most ordinary
/// use of the class:
///
/// | expression | HotSpot 25 | CratonVM before |
/// |---|---|---|
/// | `new Formatter(sb).format("%d",42).toString()` | `42` | `` (empty) |
/// | `new Formatter().format("%d",1).toString()` | `1` | `1` |
///
/// The second row is why it survived: the `()V` native writes a real `String`
/// into slot 0, so only the `Appendable`-taking constructors — the ones whose
/// sink is the caller's — could show it. `format` itself was never affected;
/// its own `read_string` miss falls through to `invoke_virtual(sink, "append",
/// …)`, which works, so the text really is in the caller's `StringBuilder` and
/// only the `Formatter`'s own view of it was empty.
///
/// The fallback is `toString()` on the sink, which is what the JDK does and
/// what every `Appendable` implements. A sink that answers nothing at all still
/// degrades to the empty string, which is the previous behaviour.
pub fn formatter_sink_text(ctx: &mut dyn NativeContext, sink: ObjectRef) -> Value {
    if let Some(s) = ctx.read_string(sink) {
        return Value::Object(Some(ctx.create_string(&s)));
    }
    match ctx.invoke_virtual(sink, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        _ => Value::Object(Some(ctx.create_string(""))),
    }
}

pub fn native_formatter_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let sb = ctx.get_field(this, 0);
    match sb {
        Value::Object(Some(o)) => Ok(Some(formatter_sink_text(ctx, o))),
        _ => Ok(Some(Value::Object(Some(ctx.create_string(""))))),
    }
}

pub fn native_formatter_out(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))}

/// Simple Java format string processor
pub fn simple_java_format(ctx: &mut dyn NativeContext, fmt: &str, args: &[Value]) -> String {
    let mut result = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    let mut arg_idx = 0;

    while i < chars.len() {
        if chars[i] == '%' && i + 1 < chars.len() {
            i += 1;
            // Skip width/precision modifiers
            while i < chars.len()
                && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == '-')
            {
                i += 1;
            }
            if i >= chars.len() {
                break;
            }
            match chars[i] {
                's' | 'S' => {
                    if let Some(val) = args.get(arg_idx) {
                        result.push_str(&value_to_format_string(ctx, *val));
                    }
                    arg_idx += 1;
                }
                'd' => {
                    if let Some(val) = args.get(arg_idx) {
                        result.push_str(&value_to_int_string(*val));
                    }
                    arg_idx += 1;
                }
                'f' => {
                    if let Some(val) = args.get(arg_idx) {
                        result.push_str(&value_to_float_string(*val));
                    }
                    arg_idx += 1;
                }
                'b' | 'B' => {
                    if let Some(val) = args.get(arg_idx) {
                        let b = !matches!(val, Value::Object(None) | Value::Int(0));
                        result.push_str(if b { "true" } else { "false" });
                    }
                    arg_idx += 1;
                }
                'c' | 'C' => {
                    if let Some(Value::Int(v)) = args.get(arg_idx) {
                        if let Some(ch) = char::from_u32(*v as u32) {
                            result.push(ch);
                        }
                    }
                    arg_idx += 1;
                }
                'n' => {
                    result.push_str(if cfg!(windows) { "\r\n" } else { "\n" });
                }
                '%' => {
                    result.push('%');
                }
                'x' | 'X' => {
                    if let Some(val) = args.get(arg_idx) {
                        let v = match val {
                            Value::Int(i) => *i as i64,
                            Value::Long(l) => *l,
                            Value::Object(Some(o)) => match ctx.get_field(*o, 0) {
                                Value::Int(i) => i as i64,
                                Value::Long(l) => l,
                                _ => 0,
                            },
                            _ => 0,
                        };
                        if chars[i] == 'X' {
                            result.push_str(&format!("{:X}", v));
                        } else {
                            result.push_str(&format!("{:x}", v));
                        }
                    }
                    arg_idx += 1;
                }
                'o' => {
                    if let Some(val) = args.get(arg_idx) {
                        let v = match val {
                            Value::Int(i) => *i as i64,
                            Value::Long(l) => *l,
                            _ => 0,
                        };
                        result.push_str(&format!("{:o}", v));
                    }
                    arg_idx += 1;
                }
                _ => {
                    // Unknown specifier, consume arg
                    if let Some(val) = args.get(arg_idx) {
                        result.push_str(&value_to_format_string(ctx, *val));
                    }
                    arg_idx += 1;
                }
            }
        } else {
            result.push(chars[i]);
        }
        i += 1;
    }
    result
}

pub fn value_to_format_string(ctx: &mut dyn NativeContext, val: Value) -> String {
    match val {
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        // Java-spec float/double rendering (matches Double.toString), not raw
        // Rust `{}` which omits ".0", prints "inf"/"-0", and never uses E-notation.
        Value::Float(v) => format_float(v),
        Value::Double(v) => format_double(v),
        Value::Object(None) => "null".to_string(),
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_else(|| "object".to_string()),
        _ => "?".to_string(),
    }
}

pub fn value_to_int_string(val: Value) -> String {
    match val {
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        Value::Float(v) => (v as i64).to_string(),
        Value::Double(v) => (v as i64).to_string(),
        Value::Object(None) => "null".to_string(),
        _ => "0".to_string(),
    }
}

pub fn value_to_float_string(val: Value) -> String {
    match val {
        Value::Float(v) => format!("{:.6}", v),
        Value::Double(v) => format!("{:.6}", v),
        Value::Int(v) => format!("{:.6}", v as f64),
        Value::Long(v) => format!("{:.6}", v as f64),
        _ => "0.000000".to_string(),
    }
}

// Note: StringWriter and StringReader are registered in io.rs — removed duplicate code

// ===========================================================================
// Collections extras: unmodifiable wrappers
// ===========================================================================

pub(crate) fn register_phase_d_natives(registry: &mut NativeMethodRegistry) {
    register_pd_stream_gatherers(registry);
    register_pd_scoped_values(registry);
    register_pd_structured_concurrency(registry);
}

// -----------------------------------------------------------------------------
// Stream Gatherers (JEP 473)
// -----------------------------------------------------------------------------

/// Gatherer layout: 5-field synthetic.
/// `[0]=initializer, [1]=integrator, [2]=combiner, [3]=finisher, [4]=kind`
/// kind: 0=custom, 1=windowFixed, 2=windowSliding, 3=fold, 4=scan, 5=mapConcurrent
pub const GATHERER_KIND_CUSTOM: i32 = 0;
pub const GATHERER_KIND_WINDOW_FIXED: i32 = 1;
pub const GATHERER_KIND_WINDOW_SLIDING: i32 = 2;
pub const GATHERER_KIND_FOLD: i32 = 3;
pub const GATHERER_KIND_SCAN: i32 = 4;
pub const GATHERER_KIND_MAP_CONCURRENT: i32 = 5;

pub fn register_pd_stream_gatherers(r: &mut NativeMethodRegistry) {
    let stream = "java/util/stream/Stream";

    r.register(
        stream,
        "gather",
        "(Ljava/util/stream/Gatherer;)Ljava/util/stream/Stream;",
        pd_stream_gather, NativeKind::SyntheticStub,
    );

    let gatherer = "java/util/stream/Gatherer";
    r.register(
        gatherer,
        "initializer",
        "()Ljava/util/function/Supplier;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        gatherer,
        "integrator",
        "()Ljava/util/stream/Gatherer$Integrator;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        gatherer,
        "combiner",
        "()Ljava/util/function/BinaryOperator;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        gatherer,
        "finisher",
        "()Ljava/util/function/BiConsumer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        }, NativeKind::SyntheticStub,
    );

    r.register(gatherer, "ofSequential", "(Ljava/util/function/Supplier;Ljava/util/stream/Gatherer$Integrator;Ljava/util/function/BiConsumer;)Ljava/util/stream/Gatherer;", |ctx, args| {
        let initializer = args.first().copied().unwrap_or(Value::Object(None));
        let integrator = args.get(1).copied().unwrap_or(Value::Object(None));
        let finisher = args.get(2).copied().unwrap_or(Value::Object(None));
        let g = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 5)?;
        ctx.set_field(g, 0, initializer);
        ctx.set_field(g, 1, integrator);
        ctx.set_field(g, 2, Value::Object(None));
        ctx.set_field(g, 3, finisher);
        ctx.set_field(g, 4, Value::Int(GATHERER_KIND_CUSTOM));
        Ok(Some(Value::Object(Some(g))))
    }, NativeKind::SyntheticStub);

    // The TWO-argument `ofSequential(Supplier, Integrator)` — added because its
    // absence here was an out-of-bounds read, not a missing feature.
    //
    // `java/util/stream/Gatherer` has NO `synthetic_stub_fields` arm, so
    // `class_num_total_fields` answers 0 and `try_alloc_concurrent_synthetic`'s
    // `num_fields.max(real)` leaves the caller's request as the LITERAL object
    // width. Two registrars model this class at two widths with two different
    // slot maps:
    //
    //   this file            5 slots: initializer 0, integrator 1, combiner 2,
    //                        finisher 3, KIND 4
    //   phases_late/streams.rs  3 slots: initializer 0, integrator 1, finisher 2
    //
    // `register_phase_d_natives` (this one) runs AFTER `register_phase67_natives`
    // inside `register_synthetic_overrides`, and `register()` is
    // last-registration-wins, so this file's accessors and `Stream.gather` win
    // for every triple both files spell. Every 3-slot factory in `streams.rs`
    // was therefore shadowed by a 5-slot one here — EXCEPT this overload, which
    // only `streams.rs` registered (`streams.rs:4541`). Its 3-slot product then
    // reached `pd_stream_gather`, whose first act is `ctx.get_field(gatherer, 4)`:
    // `Heap::get_field` asserts `index < num_slots`, so `stream.gather(g)` on a
    // gatherer from this overload aborts the VM with "field index 4 out of
    // bounds (num_slots=3)". `finisher()` (slot 3) is the same panic one slot
    // lower, and `combiner()` (slot 2) would have returned the FINISHER.
    //
    // Registering it here shadows the 3-slot factory with the same 5-slot shape
    // the sibling overloads already use, which is the whole fix; the alternative
    // (widening `streams.rs` to 5) would leave two copies of one slot map.
    // Verified against the oracle: `javap -p java.util.stream.Gatherer` on
    // JDK 25.0.3 declares all four of `ofSequential`'s/`of`'s arities, of which
    // this VM serves three — see
    // docs/feature-designs/native-dispatch-architecture.md §3.1.
    r.register(gatherer, "ofSequential", "(Ljava/util/function/Supplier;Ljava/util/stream/Gatherer$Integrator;)Ljava/util/stream/Gatherer;", |ctx, args| {
        let initializer = args.first().copied().unwrap_or(Value::Object(None));
        let integrator = args.get(1).copied().unwrap_or(Value::Object(None));
        let g = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 5)?;
        ctx.set_field(g, 0, initializer);
        ctx.set_field(g, 1, integrator);
        ctx.set_field(g, 2, Value::Object(None));
        ctx.set_field(g, 3, Value::Object(None));
        ctx.set_field(g, 4, Value::Int(GATHERER_KIND_CUSTOM));
        Ok(Some(Value::Object(Some(g))))
    }, NativeKind::SyntheticStub);

    r.register(
        gatherer,
        "of",
        "(Ljava/util/stream/Gatherer$Integrator;)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            let integrator = args.first().copied().unwrap_or(Value::Object(None));
            let g = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 5)?;
            ctx.set_field(g, 0, Value::Object(None));
            ctx.set_field(g, 1, integrator);
            ctx.set_field(g, 2, Value::Object(None));
            ctx.set_field(g, 3, Value::Object(None));
            ctx.set_field(g, 4, Value::Int(GATHERER_KIND_CUSTOM));
            Ok(Some(Value::Object(Some(g))))
        }, NativeKind::SyntheticStub,
    );

    // NOTE: `Gatherer$Integrator.integrate(...)Z` is deliberately NOT
    // registered. It used to answer a constant `true` ("keep going") while
    // pushing nothing downstream — i.e. it consumed the element and dropped
    // it, so any gather() whose integrator reached this native produced an
    // EMPTY result instead of the user's. `integrate` is the SAM of an
    // interface: a lambda implementation is served by `try_lambda_dispatch`
    // before any native lookup, and a class implementation declares its own
    // `integrate`, so the real body is what the gather loop below
    // (`ctx.invoke_virtual(integrator, "integrate", ...)`) needs to reach.
    // Nothing in the tree allocates a synthetic receiver whose class IS
    // `Gatherer$Integrator`, so there is no caller left for a native here.
    let downstream = "java/util/stream/Gatherer$Downstream";
    r.register(downstream, "push", "(Ljava/lang/Object;)Z", |ctx, args| {
        // Downstream is a 2-field synthetic: [0]=backing array, [1]=size
        let this = obj_arg(args, 0)?;
        let value = args.get(1).copied().unwrap_or(Value::Object(None));
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let arr_len = ctx.array_length(arr);
            if size < arr_len {
                ctx.set_array_element(arr, size, value);
                ctx.set_field(this, 1, Value::Int((size + 1) as i32));            }        }
        Ok(Some(Value::Int(1)))
    }, NativeKind::SyntheticStub);

    let gatherers = "java/util/stream/Gatherers";

    r.register(
        gatherers,
        "fold",
        "(Ljava/util/function/Supplier;Ljava/util/function/BiFunction;)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            let initial = args.first().copied().unwrap_or(Value::Object(None));
            let folder = args.get(1).copied().unwrap_or(Value::Object(None));
            let g = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 5)?;
            ctx.set_field(g, 0, initial);
            ctx.set_field(g, 1, folder);
            ctx.set_field(g, 2, Value::Object(None));
            ctx.set_field(g, 3, Value::Object(None));
            ctx.set_field(g, 4, Value::Int(GATHERER_KIND_FOLD));
            Ok(Some(Value::Object(Some(g))))
        }, NativeKind::SyntheticStub,
    );

    r.register(
        gatherers,
        "scan",
        "(Ljava/util/function/Supplier;Ljava/util/function/BiFunction;)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            let initial = args.first().copied().unwrap_or(Value::Object(None));
            let scanner = args.get(1).copied().unwrap_or(Value::Object(None));
            let g = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 5)?;
            ctx.set_field(g, 0, initial);
            ctx.set_field(g, 1, scanner);
            ctx.set_field(g, 2, Value::Object(None));
            ctx.set_field(g, 3, Value::Object(None));
            ctx.set_field(g, 4, Value::Int(GATHERER_KIND_SCAN));
            Ok(Some(Value::Object(Some(g))))
        }, NativeKind::SyntheticStub,
    );

    r.register(
        gatherers,
        "windowFixed",
        "(I)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            let window_size = match args.first() {
                Some(Value::Int(n)) => *n,
                _ => 1,
            };
            let g = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 5)?;
            ctx.set_field(g, 0, Value::Int(window_size));
            ctx.set_field(g, 1, Value::Object(None));
            ctx.set_field(g, 2, Value::Object(None));
            ctx.set_field(g, 3, Value::Object(None));
            ctx.set_field(g, 4, Value::Int(GATHERER_KIND_WINDOW_FIXED));
            Ok(Some(Value::Object(Some(g))))
        }, NativeKind::SyntheticStub,
    );

    r.register(
        gatherers,
        "windowSliding",
        "(I)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            let window_size = match args.first() {
                Some(Value::Int(n)) => *n,
                _ => 1,
            };
            let g = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 5)?;
            ctx.set_field(g, 0, Value::Int(window_size));
            ctx.set_field(g, 1, Value::Object(None));
            ctx.set_field(g, 2, Value::Object(None));
            ctx.set_field(g, 3, Value::Object(None));
            ctx.set_field(g, 4, Value::Int(GATHERER_KIND_WINDOW_SLIDING));
            Ok(Some(Value::Object(Some(g))))
        }, NativeKind::SyntheticStub,
    );

    r.register(
        gatherers,
        "mapConcurrent",
        "(ILjava/util/function/Function;)Ljava/util/stream/Gatherer;",
        |ctx, args| {
            let function = args.get(1).copied().unwrap_or(Value::Object(None));
            let g = try_alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer", 5)?;
            ctx.set_field(g, 0, Value::Object(None));
            ctx.set_field(g, 1, function);
            ctx.set_field(g, 2, Value::Object(None));
            ctx.set_field(g, 3, Value::Object(None));
            ctx.set_field(g, 4, Value::Int(GATHERER_KIND_MAP_CONCURRENT));
            Ok(Some(Value::Object(Some(g))))
        }, NativeKind::SyntheticStub,
    );
}

/// Stream.gather(Gatherer) — dispatches by gatherer kind for correct semantics.
pub fn pd_stream_gather(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let gatherer = obj_arg(args, 1)?;
    let elems = p56_read_stream_elems(ctx, this);

    let kind = match ctx.get_field(gatherer, 4) {
        Value::Int(k) => k,
        _ => GATHERER_KIND_CUSTOM,
    };

    // gc-common w21-a: the gatherer and every element are pinned HERE, once,
    // before any helper runs Java or allocates, and every helper reads them
    // back through these pins. The helpers used to pin the elements themselves
    // -- `pd_gather_fold` / `_scan` / `_custom` only AFTER the gatherer's
    // `initializer().get()` (Java) and, for `_custom`, two allocations, i.e. at
    // possibly stale addresses -- and `pd_gather_map` not at all, across one
    // `apply` (Java) per element. Every pin a helper leaves (early `?` exits
    // included) is released with these.
    let gatherer_pin = ctx.pin_native_root(gatherer);
    let elem_pins = pd_pin_elems(ctx, &elems);
    let pinned = PdElems {
        elems: &elems,
        pins: &elem_pins,
    };
    let result = match kind {
        GATHERER_KIND_FOLD => pd_gather_fold(ctx, pinned, gatherer),
        GATHERER_KIND_SCAN => pd_gather_scan(ctx, pinned, gatherer),
        GATHERER_KIND_WINDOW_FIXED => pd_gather_window_fixed(ctx, pinned, gatherer),
        GATHERER_KIND_WINDOW_SLIDING => pd_gather_window_sliding(ctx, pinned, gatherer),
        GATHERER_KIND_MAP_CONCURRENT => pd_gather_map(ctx, pinned, gatherer),
        _ => pd_gather_custom(ctx, pinned, gatherer),
    };
    // `result` is current: each helper re-reads what it returns after its
    // last GC point. `p56_build_stream` pins its own input.
    ctx.unpin_native_roots(gatherer_pin);
    let stream = p56_build_stream(ctx, result?, "java/util/stream/Stream");
    Ok(Some(Value::Object(Some(stream?))))
}

/// The upstream elements of a `Stream.gather`, with the pins
/// [`pd_stream_gather`] took for them ([`pd_pin_elems`]).
#[derive(Clone, Copy)]
pub struct PdElems<'a> {
    pub elems: &'a [Value],
    pub pins: &'a [usize],
}

impl PdElems<'_> {
    pub fn len(&self) -> usize {
        self.elems.len()
    }

    /// Element `i` at its current address.
    pub fn get(&self, ctx: &dyn NativeContext, i: usize) -> Value {
        pd_read_elem(ctx, self.pins, i, self.elems[i])
    }
}

/// Pin every reference in a `Value` slice, returning one handle per element
/// (`usize::MAX` for the non-reference ones).
///
/// The `pd_gather_*` bodies each run an arbitrary user lambda once per element,
/// and the caller's slice is a bare Rust local: nothing rewrites it, so element
/// `i` is a pre-GC address for every turn after the one that first allocated.
pub fn pd_pin_elems(ctx: &mut dyn NativeContext, elems: &[Value]) -> Vec<usize> {
    elems
        .iter()
        .map(|v| match v {
            Value::Object(Some(o)) => ctx.pin_native_root(*o),
            _ => usize::MAX,
        })
        .collect()
}

/// Read element `i` back through the handle [`pd_pin_elems`] took for it.
pub fn pd_read_elem(ctx: &dyn NativeContext, pins: &[usize], i: usize, orig: Value) -> Value {
    match (orig, pins.get(i).copied().unwrap_or(usize::MAX)) {
        (Value::Object(Some(o)), pin) if pin != usize::MAX => {
            Value::Object(Some(ctx.read_native_pin(pin, o)))
        }
        (other, _) => other,
    }
}

/// The gatherer's initial state (`initializer().get()`, Java) and the
/// gatherer's current address after it.
///
/// gc-common w20-c: `pd_gather_fold` / `pd_gather_scan` read the gatherer's
/// folder / scanner slot through the address they held before this call.
pub fn pd_gatherer_after_init(
    ctx: &mut dyn NativeContext,
    gatherer: ObjectRef,
) -> Result<(ObjectRef, Value), MethodCallFailed> {
    let Value::Object(Some(init)) = ctx.get_field(gatherer, 0) else {
        return Ok((gatherer, Value::Object(None)));
    };
    let gatherer_pin = ctx.pin_native_root(gatherer);
    let state = ctx.invoke_virtual(init, "get", "()Ljava/lang/Object;", &[]);
    let gatherer = ctx.read_native_pin(gatherer_pin, gatherer);
    ctx.unpin_native_roots(gatherer_pin);
    Ok((gatherer, state?.unwrap_or(Value::Object(None))))
}

pub fn pd_gather_fold(
    ctx: &mut dyn NativeContext,
    elems: PdElems<'_>,
    gatherer: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let gatherer = pd_gatherer_after_init(ctx, gatherer);
    let (gatherer, mut state) = gatherer?;
    // GC-safety: `state` is reassigned from each call's own return value, so it
    // is current by construction -- the folder and the elements are not.
    // `apply` is an arbitrary user lambda: it allocates, and both the receiver
    // read once from `gatherer` and every element of the caller's slice are
    // bare Rust locals from the second turn on (the elements are read through
    // the caller's pins).
    if let Value::Object(Some(folder)) = ctx.get_field(gatherer, 1) {
        let folder_pin = ctx.pin_native_root(folder);
        for i in 0..elems.len() {
            let folder = ctx.read_native_pin(folder_pin, folder);
            let elem = elems.get(ctx, i);
            state = ctx
                .invoke_virtual(
                    folder,
                    "apply",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[state, elem],
                )?
                .unwrap_or(Value::Object(None));
        }
        ctx.unpin_native_roots(folder_pin);
    }
    Ok(vec![state])
}

pub fn pd_gather_scan(
    ctx: &mut dyn NativeContext,
    elems: PdElems<'_>,
    gatherer: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let gatherer = pd_gatherer_after_init(ctx, gatherer);
    let (gatherer, mut state) = gatherer?;
    let mut result = Vec::with_capacity(elems.len());
    // GC-safety: see `pd_gather_fold`. `result` additionally accumulates
    // references across later allocations, so each is pinned as it is pushed
    // and read back through its pin before the vector is returned.
    if let Value::Object(Some(scanner)) = ctx.get_field(gatherer, 1) {
        let scanner_pin = ctx.pin_native_root(scanner);
        let mut result_pins: Vec<usize> = Vec::with_capacity(elems.len());
        for i in 0..elems.len() {
            let scanner = ctx.read_native_pin(scanner_pin, scanner);
            let elem = elems.get(ctx, i);
            state = ctx
                .invoke_virtual(
                    scanner,
                    "apply",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[state, elem],
                )?
                .unwrap_or(Value::Object(None));
            result_pins.push(match state {
                Value::Object(Some(o)) => ctx.pin_native_root(o),
                _ => usize::MAX,
            });
            result.push(state);
        }
        for i in 0..result.len() {
            result[i] = pd_read_elem(ctx, &result_pins, i, result[i]);
        }
        ctx.unpin_native_roots(scanner_pin);
    }
    Ok(result)
}

pub fn pd_gather_window_fixed(
    ctx: &mut dyn NativeContext,
    elems: PdElems<'_>,
    gatherer: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let window_size = match ctx.get_field(gatherer, 0) {
        Value::Int(n) => n.max(1) as usize,
        _ => 1,
    };
    // Each window becomes an ArrayList in the output stream
    let ranges: Vec<(usize, usize)> = (0..elems.len())
        .step_by(window_size)
        .map(|start| (start, (start + window_size).min(elems.len())))
        .collect();
    pd_windows_to_lists(ctx, elems, &ranges)
}

pub fn pd_gather_window_sliding(
    ctx: &mut dyn NativeContext,
    elems: PdElems<'_>,
    gatherer: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let window_size = match ctx.get_field(gatherer, 0) {
        Value::Int(n) => n.max(1) as usize,
        _ => 1,
    };
    let ranges: Vec<(usize, usize)> = if elems.len() >= window_size {
        (0..=(elems.len() - window_size))
            .map(|start| (start, start + window_size))
            .collect()
    } else {
        Vec::new()
    };
    pd_windows_to_lists(ctx, elems, &ranges)
}

/// One synthetic `ArrayList` per `[start, end)` range of `elems`.
///
/// gc-common w20-c: the two window gatherers allocated a list and then its
/// array (so the list was written through a possibly moved address), filled
/// the array from the caller's element slice after both allocations, and
/// returned lists allocated before later windows' allocations. Every list is
/// pinned here and read back at its use; the elements are read through the
/// pins [`pd_stream_gather`] took (gc-common w21-a).
pub fn pd_windows_to_lists(
    ctx: &mut dyn NativeContext,
    elems: PdElems<'_>,
    ranges: &[(usize, usize)],
) -> Result<Vec<Value>, MethodCallFailed> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    let mut list_pins: Vec<usize> = Vec::with_capacity(ranges.len());
    let mut lists: Vec<ObjectRef> = Vec::with_capacity(ranges.len());
    for &(start, end) in ranges {
        let al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
        let al_pin = ctx.pin_native_root(al);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, end - start);
        let al: ObjectRef = ctx.read_native_pin(al_pin, al);
        for i in start..end {
            let v = elems.get(ctx, i);
            ctx.set_array_element(arr, i - start, v);
        }
        ctx.set_field(al, 0, Value::Object(Some(arr)));
        ctx.set_field(al, 1, Value::Int((end - start) as i32));
        list_pins.push(al_pin);
        lists.push(al);
    }
    let mut result: Vec<Value> = Vec::with_capacity(lists.len());
    for (&al, &pin) in lists.iter().zip(&list_pins) {
        result.push(Value::Object(Some(ctx.read_native_pin(pin, al))));
    }
    // Every list pin was taken after the first one; the element pins belong
    // to the caller.
    ctx.unpin_native_roots(list_pins[0]);
    Ok(result)
}

/// gc-common w21-a: `apply` is an arbitrary user lambda (Java). The function
/// was dispatched on its first-read address, every element was passed at its
/// pre-loop address, and the mapped results accumulated in `result` across
/// the later calls; all three are now read through pins, as `pd_gather_scan`
/// does.
pub fn pd_gather_map(
    ctx: &mut dyn NativeContext,
    elems: PdElems<'_>,
    gatherer: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let mut result = Vec::with_capacity(elems.len());
    if let Value::Object(Some(func)) = ctx.get_field(gatherer, 1) {
        let func_pin = ctx.pin_native_root(func);
        let mut result_pins: Vec<usize> = Vec::with_capacity(elems.len());
        for i in 0..elems.len() {
            let func = ctx.read_native_pin(func_pin, func);
            let elem = elems.get(ctx, i);
            let mapped = ctx
                .invoke_virtual(
                    func,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[elem],
                )?
                .unwrap_or(Value::Object(None));
            result_pins.push(match mapped {
                Value::Object(Some(o)) => ctx.pin_native_root(o),
                _ => usize::MAX,
            });
            result.push(mapped);
        }
        for i in 0..result.len() {
            result[i] = pd_read_elem(ctx, &result_pins, i, result[i]);
        }
        ctx.unpin_native_roots(func_pin);
    }
    Ok(result)
}

pub fn pd_gather_custom(
    ctx: &mut dyn NativeContext,
    elems: PdElems<'_>,
    gatherer: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    // gc-common w21-a: the integrator and the finisher are read from the
    // gatherer AFTER `initializer().get()` (Java), at the gatherer's current
    // address; they were read before it and used after it. The finisher is
    // pinned across the integrate loop, and the downstream collector across
    // its backing array's allocation (it was written through its
    // pre-allocation address).
    let gatherer = pd_gatherer_after_init(ctx, gatherer);
    let (gatherer, state) = gatherer?;
    let integrator_val = ctx.get_field(gatherer, 1);
    let finisher = ctx.get_field(gatherer, 3);
    let has_integrator = integrator_val != Value::Object(None);

    // GC-safety: unlike `pd_gather_fold`/`_scan`, `state` here is NOT
    // reassigned -- the same reference is handed to every `integrate` call and
    // then to the finisher. `integrate` is an arbitrary user lambda that
    // allocates, so `state`, the freshly built `downstream` collector, the
    // integrator, the finisher and every element are all pre-GC addresses from
    // the second turn on.
    let state_pin = match state {
        Value::Object(Some(o)) => ctx.pin_native_root(o),
        _ => usize::MAX,
    };
    let finisher_pin = match finisher {
        Value::Object(Some(o)) => ctx.pin_native_root(o),
        _ => usize::MAX,
    };
    let integrator_pin = match integrator_val {
        Value::Object(Some(o)) => ctx.pin_native_root(o),
        _ => usize::MAX,
    };

    // Create a downstream collector: [0]=backing array, [1]=size
    let downstream =
        try_alloc_concurrent_synthetic(ctx, "java/util/stream/Gatherer$Downstream", 2)?;
    let downstream_pin = ctx.pin_native_root(downstream);
    let downstream_arr = ctx.new_array(
        cratonvm_types::ArrayElementType::Reference,
        elems.len() + 16,
    );
    let downstream = ctx.read_native_pin(downstream_pin, downstream);
    ctx.set_field(downstream, 0, Value::Object(Some(downstream_arr)));
    ctx.set_field(downstream, 1, Value::Int(0));

    let mut state = state;
    if let Value::Object(Some(integrator)) = integrator_val {
        for i in 0..elems.len() {
            let integrator = ctx.read_native_pin(integrator_pin, integrator);
            let elem = elems.get(ctx, i);
            state = pd_read_elem(ctx, &[state_pin], 0, state);
            let downstream = ctx.read_native_pin(downstream_pin, downstream);
            let cont = ctx.invoke_virtual(
                integrator,
                "integrate",
                "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/stream/Gatherer$Downstream;)Z",
                &[state, elem, Value::Object(Some(downstream))],
            );
            if let Ok(Some(Value::Int(0))) = cont {
                break;
            }
        }
    }
    let downstream = ctx.read_native_pin(downstream_pin, downstream);
    let state = pd_read_elem(ctx, &[state_pin], 0, state);

    if let Value::Object(Some(finisher_ref)) = pd_read_elem(ctx, &[finisher_pin], 0, finisher) {
        let _ = ctx.invoke_virtual(
            finisher_ref,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[state, Value::Object(Some(downstream))],
        );
    }
    let downstream = ctx.read_native_pin(downstream_pin, downstream);
    // Every pin this function took was taken at or after the first of these.
    let base = [state_pin, finisher_pin, integrator_pin]
        .into_iter()
        .find(|&p| p != usize::MAX)
        .unwrap_or(downstream_pin);
    ctx.unpin_native_roots(base);

    // Read collected downstream values
    let ds_size = match ctx.get_field(downstream, 1) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let ds_arr = ctx.get_field(downstream, 0);
    let mut result = Vec::with_capacity(ds_size);
    if let Value::Object(Some(arr)) = ds_arr {
        for i in 0..ds_size {
            result.push(ctx.get_array_element(arr, i));
        }
    }

    if result.is_empty() && elems.len() > 0 && !has_integrator {
        // The elements at their current addresses (the caller's pins).
        result = (0..elems.len()).map(|i| elems.get(ctx, i)).collect();
    }

    Ok(result)
}

// -----------------------------------------------------------------------------
// Scoped Values (JEP 446, finalized in Java 25)
// -----------------------------------------------------------------------------

pub fn register_pd_scoped_values(r: &mut NativeMethodRegistry) {
    let sv = "java/lang/ScopedValue";

    r.register(
        sv,
        "newInstance",
        "()Ljava/lang/ScopedValue;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/ScopedValue", 2)?;
            Ok(Some(Value::Object(Some(obj))))
        }, NativeKind::SyntheticStub,
    );

    r.register(sv, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match pd_scoped_value_binding(ctx, this) {
            Some(val) => Ok(Some(val)),
            None => Err(RuntimeError::IllegalStateException {
                message: "ScopedValue is not bound".to_string(),
            }
            .into()),
        }
    }, NativeKind::SyntheticStub);

    r.register(sv, "isBound", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            if pd_scoped_value_binding(ctx, this).is_some() {
                1            } else {
                0            },
        )))
    }, NativeKind::SyntheticStub);

    r.register(
        sv,
        "orElse",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = args.get(1).copied().unwrap_or(Value::Object(None));
            Ok(Some(pd_scoped_value_binding(ctx, this).unwrap_or(other)))
        }, NativeKind::SyntheticStub,
    );

    r.register(
        sv,
        "orElseThrow",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match pd_scoped_value_binding(ctx, this) {
                Some(val) => Ok(Some(val)),
                None => Err(RuntimeError::IllegalStateException {
                    message: "ScopedValue is not bound".to_string(),
                }
                .into()),
            }
        }, NativeKind::SyntheticStub,
    );

    r.register(
        sv,
        "where",
        "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
        |ctx, args| {
            let scoped_val = args.first().copied().unwrap_or(Value::Object(None));
            let value = args.get(1).copied().unwrap_or(Value::Object(None));
            let carrier = pd_new_carrier(ctx, [scoped_val, value, Value::Object(None)])?;
            Ok(Some(Value::Object(Some(carrier))))
        }, NativeKind::SyntheticStub,
    );

    let carrier = "java/lang/ScopedValue$Carrier";

    r.register(
        carrier,
        "where",
        "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let scoped_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let value = args.get(2).copied().unwrap_or(Value::Object(None));
            let new_carrier = pd_new_carrier(ctx, [scoped_val, value, Value::Object(Some(this))])?;
            Ok(Some(Value::Object(Some(new_carrier))))
        }, NativeKind::SyntheticStub,
    );

    // Carrier.run(Runnable) — push bindings, run, pop (exception-safe)
    r.register(carrier, "run", "(Ljava/lang/Runnable;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let runnable = obj_arg(args, 1)?;
        let bindings = pd_collect_carrier_bindings(ctx, this);
        let binding_count = bindings.len();
        for (key_id, key_ref, value) in &bindings {
            // Round-9 GC fix: pass the ScopedValue key ObjectRef so the
            // GC root scanner pins it for the binding's lifetime.
            ctx.push_scoped_value_with_key(*key_id, *key_ref, *value);
        }
        let result = ctx.invoke_virtual(runnable, "run", "()V", &[]);
        // Always pop bindings, even on error (exception safety)
        for _ in 0..binding_count {
            ctx.pop_scoped_value();
        }
        result?;
        Ok(None)
    }, NativeKind::SyntheticStub);

    // Carrier.call(Callable) → T — push bindings, call, pop (exception-safe)
    r.register(
        carrier,
        "call",
        "(Ljava/util/concurrent/Callable;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let callable = obj_arg(args, 1)?;
            let bindings = pd_collect_carrier_bindings(ctx, this);
            let binding_count = bindings.len();
            for (key_id, key_ref, value) in &bindings {
                ctx.push_scoped_value_with_key(*key_id, *key_ref, *value);
            }
            let result = ctx.invoke_virtual(callable, "call", "()Ljava/lang/Object;", &[]);
            for _ in 0..binding_count {
                ctx.pop_scoped_value();
            }
            Ok(result?.or(Some(Value::Object(None))))
        }, NativeKind::SyntheticStub,
    );

    r.register(
        carrier,
        "get",
        "(Ljava/lang/ScopedValue;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let scoped_val = obj_arg(args, 1)?;
            // The carrier's own key slots hold the ScopedValues themselves:
            // compare them (gc-common w29-b; this compared identity hashes, so
            // a colliding ScopedValue read the other's binding). Nothing below
            // collects, so both references stay current.
            let mut current = Some(this);
            while let Some(c) = current {
                if let Value::Object(Some(sv_ref)) = ctx.get_field(c, 0) {
                    if sv_ref == scoped_val {
                        return Ok(Some(ctx.get_field(c, 1)));
                    }
                }
                current = match ctx.get_field(c, 2) {
                    Value::Object(Some(prev)) => Some(prev),
                    _ => None,
                };
            }
            Err(RuntimeError::IllegalStateException {
                message: "ScopedValue not in carrier".to_string(),
            }
            .into())
        }, NativeKind::SyntheticStub,
    );
}

/// A synthetic `ScopedValue$Carrier` holding `slots` (key, value, previous
/// carrier).
///
/// gc-common w20-c: the two `where` natives stored their arguments into the
/// carrier after its allocation, which can collect and move each of them; the
/// object slots are pinned across it and read back.
pub fn pd_new_carrier(
    ctx: &mut dyn NativeContext,
    slots: [Value; 3],
) -> Result<ObjectRef, MethodCallFailed> {
    let pins = pd_pin_elems(ctx, &slots);
    let carrier = try_alloc_concurrent_synthetic(ctx, "java/lang/ScopedValue$Carrier", 3);
    let mut current: Vec<Value> = Vec::with_capacity(slots.len());
    for (i, slot) in slots.iter().enumerate() {
        current.push(pd_read_elem(ctx, &pins, i, *slot));
    }
    if let Some(&base) = pins.iter().find(|&&p| p != usize::MAX) {
        ctx.unpin_native_roots(base);
    }
    let carrier = carrier?;
    for (i, v) in current.into_iter().enumerate() {
        ctx.set_field(carrier, i, v);
    }
    Ok(carrier)
}

/// The `key_id` a binding of ScopedValue `sv` is pushed under on the thread's
/// `scoped_values` stack (`NativeContext::push_scoped_value_with_key`), and
/// looked up by (`get_scoped_value`): its weak lock key, minted on first use.
///
/// gc-common w29-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 14): it was `identity_hash_code(sv) as u64`, and `get_scoped_value`
/// matches the stack by `key_id` alone, so a ScopedValue that shared an
/// identity hash with a bound one (two live objects do once the heap's counter
/// wraps) read the other's binding -- `get()` answered another value, and
/// `isBound()` true for an unbound one. The weak lock key is unique per live
/// object and move-stable, so the VM-side match by `key_id` is exact without
/// consulting the `key_ref` each entry carries. A bound ScopedValue is rooted
/// through that `key_ref` (`vm/src/memory/gc.rs`), so its key is not freed
/// while any binding names it; the key of a ScopedValue that dies unbound is
/// freed like any other.
pub fn pd_scoped_value_key_id(ctx: &dyn NativeContext, sv: ObjectRef) -> u64 {
    lib_side_key(ctx, sv) as u64
}

/// The innermost binding of ScopedValue `sv` on this thread, or `None` when it
/// is unbound. A ScopedValue that was never bound has no key, so the lookup
/// leaves no registry slot behind ([`existing_weak_lock_key`]).
pub fn pd_scoped_value_binding(ctx: &mut dyn NativeContext, sv: ObjectRef) -> Option<Value> {
    let key_id = existing_weak_lock_key(&*ctx, sv)? as u64;
    ctx.get_scoped_value(key_id)
}

pub fn pd_collect_carrier_bindings(
    ctx: &mut dyn NativeContext,
    carrier: ObjectRef,
) -> Vec<(u64, Option<ObjectRef>, Value)> {
    // Round-9 GC fix: also carry the ScopedValue key ObjectRef so the
    // GC root scanner can pin it for the lifetime of the binding (was
    // missing — the JEP 446 key could be reclaimed while bound).
    let mut bindings = Vec::new();
    let mut current = Some(carrier);
    while let Some(c) = current {
        if let Value::Object(Some(sv_ref)) = ctx.get_field(c, 0) {
            let key_id = pd_scoped_value_key_id(ctx, sv_ref);
            let value = ctx.get_field(c, 1);
            bindings.push((key_id, Some(sv_ref), value));
        }
        current = match ctx.get_field(c, 2) {
            Value::Object(Some(prev)) => Some(prev),
            _ => None,
        };
    }
    bindings
}

// -----------------------------------------------------------------------------
// Structured Concurrency (JEP 462, finalized in Java 25)
// -----------------------------------------------------------------------------

/// Shared helper: initialize a StructuredTaskScope (or subclass) with subtask list.
pub fn pd_init_scope(ctx: &mut dyn NativeContext, this: ObjectRef) -> Result<(), MethodCallFailed> {
    // gc-common w20-c: two allocations, and the scope (then the list) was
    // written after them; the array goes first, and the scope and the array
    // are pinned across the list's allocation.
    let this_pin = ctx.pin_native_root(this);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 64);
    let arr_pin = ctx.pin_native_root(arr);
    let subtasks: ObjectRef = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    let arr: ObjectRef = ctx.read_native_pin(arr_pin, arr);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(subtasks, 0, Value::Object(Some(arr)));
    ctx.set_field(subtasks, 1, Value::Int(0));
    ctx.set_field(this, 0, Value::Object(Some(subtasks)));
    ctx.set_field(this, 1, Value::Int(0)); // OPEN
    ctx.set_field(this, 2, Value::Long(ctx.thread_id() as i64));
    Ok(())
}

/// Shared helper: fork a callable synchronously into a scope's subtask list.
pub fn pd_fork_callable(
    ctx: &mut dyn NativeContext,
    scope: ObjectRef,
    callable: Value,
) -> MethodCallResult {
    // gc-common w20-c: the subtask's allocation, the callable (Java) and the
    // error message's allocation are GC points; the scope, the callable and
    // the subtask were used raw after them. Each lives in a handle.
    let mut handles = NativeHandleScope::new(ctx);
    let scope_h = handles.root(scope);
    let callable_h = match callable {
        Value::Object(Some(c)) => Some(handles.root(c)),
        _ => None,
    };
    let subtask = try_alloc_concurrent_synthetic(
        &mut *handles,
        "java/util/concurrent/StructuredTaskScope$Subtask",
        5,
    )?;
    let subtask_h = handles.root(subtask);
    let callable = match &callable_h {
        Some(h) => Value::Object(Some(handles.get(h))),
        None => callable,
    };
    handles.set_field(subtask, 0, callable);
    handles.set_field(subtask, 3, Value::Int(0)); // UNAVAILABLE

    if let Value::Object(Some(callable_ref)) = callable {
        match handles.invoke_virtual(callable_ref, "call", "()Ljava/lang/Object;", &[]) {
            Ok(result) => {
                let subtask = handles.get(&subtask_h);
                handles.set_field(subtask, 1, result.unwrap_or(Value::Object(None)));
                handles.set_field(subtask, 3, Value::Int(1)); // SUCCESS
            }
            Err(e) => {
                // Store error description for Subtask.exception()
                let msg = handles.create_string(&format!("{e}"));
                let subtask = handles.get(&subtask_h);
                handles.set_field(subtask, 2, Value::Object(Some(msg)));
                handles.set_field(subtask, 3, Value::Int(2)); // FAILED
            }
        }
    }
    let scope = handles.get(&scope_h);
    let subtask = handles.get(&subtask_h);
    let ctx = &mut *handles;

    // Add subtask to scope's list (grow-safe: array is 64 slots)
    if let Value::Object(Some(subtasks)) = ctx.get_field(scope, 0) {
        if let Value::Object(Some(arr)) = ctx.get_field(subtasks, 0) {
            let size = match ctx.get_field(subtasks, 1) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let arr_len = ctx.array_length(arr);
            if size < arr_len {
                ctx.set_array_element(arr, size, Value::Object(Some(subtask)));
                ctx.set_field(subtasks, 1, Value::Int((size + 1) as i32));
            }
        }
    }

    Ok(Some(Value::Object(Some(subtask))))
}

/// Shared helper: find first successful subtask result.
pub fn pd_first_success(ctx: &dyn NativeContext, scope: ObjectRef) -> Option<Value> {
    if let Value::Object(Some(subtasks)) = ctx.get_field(scope, 0) {
        let size = match ctx.get_field(subtasks, 1) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        if let Value::Object(Some(arr)) = ctx.get_field(subtasks, 0) {
            for i in 0..size {
                if let Value::Object(Some(st)) = ctx.get_array_element(arr, i) {
                    if matches!(ctx.get_field(st, 3), Value::Int(1)) {
                        return Some(ctx.get_field(st, 1));                    }                }            }        }    }
    None
}

/// Check if any subtask failed.
pub fn pd_has_failure(ctx: &dyn NativeContext, scope: ObjectRef) -> bool {
    if let Value::Object(Some(subtasks)) = ctx.get_field(scope, 0) {
        let size = match ctx.get_field(subtasks, 1) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        if let Value::Object(Some(arr)) = ctx.get_field(subtasks, 0) {
            for i in 0..size {
                if let Value::Object(Some(st)) = ctx.get_array_element(arr, i) {
                    if matches!(ctx.get_field(st, 3), Value::Int(2)) {
                        return true;
                    }
                }
            }
        }
    }
    false
}


// Tomcat's connector response keeps the media type and charset in distinct real fields. The generic servlet response stubs are only for synthetic test objects, and must not clobber this state.

// Real JDK 25 Buffer(int mark, int position, int limit, int capacity,
// MemorySegment) initializes inherited private fields. Field-index writes are
// unsafe here because CharBuffer subclasses add their own layout; use the
// resolved names so duplicate()/asReadOnlyBuffer() preserve mark semantics.
//
// The constructor is not just four field writes: it VALIDATES, and several
// callers depend on that rather than checking themselves. The JDK body is
//
//     if (cap < 0) throw createCapacityException(cap);
//     this.capacity = cap; this.segment = segment;
//     limit(lim);       // IllegalArgumentException if lim > cap or lim < 0
//     position(pos);    // IllegalArgumentException if pos > limit or pos < 0
//     if (mark >= 0) { if (mark > pos) throw ...; this.mark = mark; }
//
// and `CharBuffer.wrap(csq, start, end)`, `CharBuffer.wrap(char[], off, len)`
// and `StringCharBuffer.subSequence` are all written as
// `try { new ...(...) } catch (IllegalArgumentException x) { throw new
// IndexOutOfBoundsException(); }` — the range check IS the constructor's.
// Writing the fields without checking them made
// `CharBuffer.wrap("Hello, World").subSequence(3, 2)` return a buffer with
// position 3 and limit 2, and `CharBuffer.wrap(new char[4], 3, 2)` a buffer
// running two limbs past its own array, where HotSpot throws.
//
// See `buffer-constructor-does-not-validate-position-and-limit-FIXED-20260806.md`
// (retired) — and note that the validation is NOT missing from
// `Buffer.limit(int)`/`position(int)` themselves, which are correct and do
// fire for every direct caller. It was missing only here, because this native
// stands in front of the constructor that would have called them.
pub fn register_real_buffer_constructor_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/nio/Buffer",
        "<init>",
        "(IIIILjava/lang/foreign/MemorySegment;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mark = args.get(1).and_then(Value::as_int).unwrap_or(-1);
            let position = args.get(2).and_then(Value::as_int).unwrap_or(0);
            let limit = args.get(3).and_then(Value::as_int).unwrap_or(0);
            let capacity = args.get(4).and_then(Value::as_int).unwrap_or(0);
            let segment = args.get(5).copied().unwrap_or(Value::Object(None));
            // Messages are `createCapacityException` / `createLimitException` /
            // `createPositionException` verbatim: callers that convert this to
            // an `IndexOutOfBoundsException` discard them, but anything that
            // lets the `IllegalArgumentException` out shows them to the user.
            let bad = |message: String| -> cratonvm_types::error::MethodCallFailed {
                cratonvm_types::error::RuntimeError::IllegalArgumentException { message }.into()
            };
            if capacity < 0 {
                return Err(bad(format!("capacity < 0: ({capacity} < 0)")));
            }
            if limit > capacity {
                return Err(bad(format!("newLimit > capacity: ({limit} > {capacity})")));
            }
            if limit < 0 {
                return Err(bad(format!("newLimit < 0: ({limit} < 0)")));
            }
            if position > limit {
                return Err(bad(format!("newPosition > limit: ({position} > {limit})")));
            }
            if position < 0 {
                return Err(bad(format!("newPosition < 0: ({position} < 0)")));
            }
            if mark >= 0 && mark > position {
                return Err(bad(format!("mark > position: ({mark} > {position})")));
            }
            ctx.set_field_by_name(this, "mark", Value::Int(mark));
            ctx.set_field_by_name(this, "position", Value::Int(position));
            ctx.set_field_by_name(this, "limit", Value::Int(limit));
            ctx.set_field_by_name(this, "capacity", Value::Int(capacity));
            ctx.set_field_by_name(this, "segment", segment);
            Ok(None)
        }, NativeKind::SyntheticStub,
    );
}
/// `Charset.forName` results, normalized name -> global-root handle.
///
/// gc-common w21-a (`common-w20f-process-wide-caches-of-vm-local-global-root-handles-FIXED-20260923`):
/// one map per VM, dropped by `forget_vm_native_root_stores`. It was one
/// process-wide map keyed by the name alone, so a second VM resolved VM A's
/// handle -- an index into A's global-ref table -- in its OWN table and handed
/// whatever object sat there back as a `Charset`; its reader also held the
/// (`unwrap`-on-poison) guard across `resolve_global_root`.
pub static REAL_CHARSET_CACHE: cratonvm_native_api::vm_scoped::VmScoped<
    std::collections::HashMap<String, usize>,
> = cratonvm_native_api::vm_scoped::VmScoped::new();

/// Per-VM teardown: drop `vm`'s `Charset.forName` rows. The handles index
/// `vm`'s own global-ref table, which goes with it, so they are dropped, never
/// released. Idempotent.
pub fn forget_vm_real_charset_cache(vm: usize) {
    REAL_CHARSET_CACHE.forget(vm);
}


// ───────────────────────────────────────────────────────────────────────────
// Typed flag access (T3.5b)
// ───────────────────────────────────────────────────────────────────────────

/// The whole typed configuration.
///
/// Used for the handful of flags this crate shares with `gc`, `classloading`
/// and `native-io`, which already carry fields on their own sub-structs; the
/// native-builtins-only flags live behind [`nbflags`].
#[inline(always)]
pub fn vmflags() -> &'static cratonvm_types::flags::VmFlags {
    cratonvm_types::flags()
}

/// The `native-builtins`-only slice of the typed configuration.
///
/// Replaces ~360 direct `cratonvm_types::flags::runtime_var`/`var_os` probes. A field read here is
/// one relaxed-acquire load of an already-initialised `OnceLock` plus a load
/// from a static — cheaper than the per-flag `OnceLock<bool>` helpers it
/// subsumes, and *far* cheaper than `getenv`, which takes the process environ
/// lock and linearly scans `environ`. That matters: several of these probes sit
/// on the interpreter's `new`-opcode and native-invoke paths, where an uncached
/// probe cost 130M `getenv` calls per CratonBench `hashmap` run before
/// `c258662e4`. Keep these reads as the LEFT operand of any `&&` whose right
/// operand is a string compare, exactly as that fix required.

/// Volume and cost of the NATIVE proxy-dispatch entry
/// (`Proxy$Dispatch.invokeProxy`), armed by `CRATONVM_DBG=ann-proxy-prof`.
///
/// The companion counter in `annotation_proxy_dispatch_impl` covers only the
/// interpreter's hook. This is the other half of the funnel, and on a workload
/// whose annotations are reached through generated `$ProxyN` bodies it is the
/// half that carries the traffic. Printed on the same flag so one run reports
/// both.
pub static PROXY_DISPATCH_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PROXY_DISPATCH_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn proxy_dispatch_prof_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok())}

pub fn note_proxy_dispatch_ns(ns: u64) {
    use std::sync::atomic::Ordering;
    PROXY_DISPATCH_NS.fetch_add(ns, Ordering::Relaxed);
    let n = PROXY_DISPATCH_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    if n % 100_000 == 0 {
        eprintln!(
            "[PROXY-DISPATCH-PROF] native invokeProxy calls={n} total={}ns/call cumulative={}ms",
            PROXY_DISPATCH_NS.load(Ordering::Relaxed) / n,
            PROXY_DISPATCH_NS.load(Ordering::Relaxed) / 1_000_000,
        );
    }
}

/// Final tally, for the exit dump — the number that turns "this call is 1000x
/// too slow" into "and it is worth N seconds of the run".
pub fn proxy_dispatch_prof_summary() -> Option<(u64, u64)> {
    use std::sync::atomic::Ordering;
    let calls = PROXY_DISPATCH_CALLS.load(Ordering::Relaxed);
    if calls == 0 {
        return None;
    }
    Some((calls, PROXY_DISPATCH_NS.load(Ordering::Relaxed)))
}

#[inline(always)]
pub fn nbflags() -> &'static cratonvm_types::flags::NativeFlags {
    &cratonvm_types::flags().natives
}


/// `Thread.tid` -> global root of that thread's scoped-value cache array (per VM).
pub static SCOPED_VALUE_CACHES: cratonvm_native_api::vm_scoped::VmScoped<
    std::collections::HashMap<i64, usize>,
> = cratonvm_native_api::vm_scoped::VmScoped::new();

/// Drop the ending thread's scoped-value cache (interpreter round i5 wave
/// 102, lane L6; `L6W102ScopedValueCacheDiesWithThread`). HotSpot keeps the
/// cache in the `JavaThread` (`_scopedValueCache`), so it dies with the
/// thread; here a platform thread that ended with a cache installed left its
/// [`SCOPED_VALUE_CACHES`] entry, and the global root kept the array and
/// every value it cached reachable for the life of the VM (one entry per dead
/// thread that ever read a bound `ScopedValue`). A JDK-driven virtual thread
/// clears its own at each unmount (`Continuation.run` sets the cache to null
/// unless `PRESERVE_SCOPED_VALUE_CACHE`). Called from
/// `phases_early::release_current_thread_locals`, which every thread end runs
/// (platform exit, JNI detach, a continuation's death). Mints no mirror.
pub fn forget_current_thread_scoped_value_cache(ctx: &mut dyn NativeContext) {
    let Some(thread) = ctx.current_thread_object_if_any() else {
        return;
    };
    let Value::Long(tid) = ctx.get_field_by_name(thread, "tid") else {
        return;
    };
    let vm = ctx.vm_identity();
    if let Some(handle) = SCOPED_VALUE_CACHES.with(vm, |m| m.remove(&tid)) {
        ctx.remove_global_root(handle);
    }
}


/// `Thread.dumpThreads(Thread[])` (`Thread.getAllStackTraces()`): one
/// `StackTraceElement[]` per input thread. Named (zr z11/nd) so the native
/// funnel can list it as restartable after an allocation refusal
/// (`oom_restartable`).
pub fn native_thread_dump_threads(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // For each input Thread, return its Java call stack. Cross-thread stack
    // walking reads each target's published frame snapshot (live frames for
    // the current thread); for a parked thread this shows where it is stuck.
    // Backs Thread.getAllStackTraces(), which thread-leak detectors use.
    let input_arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC-safety: `build_stack_trace_element_array` allocates once per
    // thread, and both the input array being read and the output array
    // being written are carried across every turn. The input is pinned
    // BEFORE the first allocation (i7-L2: it used to be pinned after
    // `outer` was allocated, i.e. possibly at a stale address).
    let input_pin = ctx.pin_native_root(input_arr);
    // `StackTraceElement[][]`, the declared type (i7-L2), not
    // `Object[]`; the lookup may load, so it runs with only pins live.
    let outer_component = crate::lang_system::stack_trace_element_array_class_id(ctx);
    let input_arr = ctx.read_native_pin(input_pin, input_arr);
    let len = ctx.array_length(input_arr);
    // zr z8/nr, gci i48/zl2: on every collector a refusal collects
    // first unless `CRATONVM_ZGC_NATIVE_DOORS_COLLECT=0` (HotSpot's
    // `ThreadService::dump_stack_traces` allocates through a
    // collecting door); `input_arr` is pinned and re-read below, so
    // the native holds no unpinned reference across the ladder.
    let outer = if ctx.native_doors_collect_on_refusal() {
        match ctx.try_new_ref_array(outer_component, len) {
            Some(outer) => outer,
            None => crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
                ctx.try_new_ref_array(outer_component, len)
            })
            .unwrap_or_else(|| ctx.new_ref_array(outer_component, len)),
        }
    } else {
        ctx.new_ref_array(outer_component, len)
    };
    let outer_pin = ctx.pin_native_root(outer);
    // Interpreter round i2 wave 61, lane L6
    // (`CRATONVM_THREAD_DUMP_ONE_PAUSE`): one frame-trace pause for
    // every running target, taken before the loop allocates; the
    // references are read for it and not kept.
    {
        let input_arr = ctx.read_native_pin(input_pin, input_arr);
        let targets: Vec<_> = (0..len)
            .filter_map(|i| match ctx.get_array_element(input_arr, i) {
                Value::Object(Some(t)) => Some(t),
                _ => None,
            })
            .collect();
        ctx.begin_thread_dump(&targets);
    }
    // zr z11/nd: the loop runs under `catch_unwind` so the dump marker
    // `begin_thread_dump` raised is lowered on EVERY exit, the allocators'
    // `NativeAllocOom` unwind included. That unwind used to leave the marker
    // set (a later capture on this thread then took the stale published
    // traces), and the funnel's replay of this native after a collection
    // (`oom_restartable`) must start from a clean marker.
    let looped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dump_threads_fill(ctx, input_pin, input_arr, outer_pin, outer, len)
    }));
    ctx.end_thread_dump();
    let filled = match looped {
        Ok(filled) => filled,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    if let Err(e) = filled {
        // gen r4w3/rooting: release the pins on the error path.
        ctx.unpin_native_roots(input_pin);
        return Err(e);
    }
    // gen r4w3/rooting: return the current address, then release pins.
    let outer = ctx.read_native_pin(outer_pin, outer);
    ctx.unpin_native_roots(input_pin);
    Ok(Some(Value::Object(Some(outer))))
}

/// The per-thread fill loop of [`native_thread_dump_threads`]: every
/// reference is re-read from its pin after each allocating step.
pub fn dump_threads_fill(
    ctx: &mut dyn NativeContext,
    input_pin: usize,
    input_arr: ObjectRef,
    outer_pin: usize,
    outer: ObjectRef,
    len: usize,
) -> Result<(), MethodCallFailed> {
    for i in 0..len {
        let input_arr = ctx.read_native_pin(input_pin, input_arr);
        let inner = match ctx.get_array_element(input_arr, i) {
            Value::Object(Some(t)) => {
                // JIT round 19 wave 11, lane gc11: a dump lists the
                // hidden `Thread.runWith` / lambda-proxy frames
                // (`thread_dump_stack_trace`), as HotSpot's does.
                let trace = ctx.thread_dump_stack_trace(t);
                // Round 14 wave 4 (lane trace3): HotSpot's dumped
                // elements are unformatted (`format == 0`).
                let built = crate::lang_system::build_stack_trace_element_array(ctx, &trace);
                if let Ok(inner) = &built {
                    crate::lang_misc::clear_dumped_stack_trace_element_formats(ctx, *inner);
                }
                built
            }
            _ => {
                let component = crate::lang_system::stack_trace_element_component_id(ctx);
                Ok(ctx.new_ref_array(component, 0))
            }
        };
        let inner = inner?;
        // gen r4w3/rooting: re-read `outer` AFTER the allocating build
        // above, not before it.
        let outer = ctx.read_native_pin(outer_pin, outer);
        ctx.set_array_element(outer, i, Value::Object(Some(inner)));
    }
    Ok(())
}

/// `Thread.getThreads()`: the live platform threads. Named (zr z11/nd), see
/// [`native_thread_dump_threads`].
pub fn native_thread_get_threads(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Return all live thread objects via the VM's enumerate_threads API.
    // `Thread[]`, the declared type (i6-L2), not an untyped `Object[]`.
    let component = crate::lang_class::reflection_component_id(ctx, "java/lang/Thread");
    // Platform threads only, as HotSpot's `JVM_GetAllThreads`
    // (interpreter round i2 wave 63, lane L1; MEASURED on HotSpot
    // 25.0.3: `Thread.getAllStackTraces()` holds no virtual thread,
    // `L1W63CarrierThreadsListed`).
    let threads: Vec<ObjectRef> = ctx
        .enumerate_threads(usize::MAX)
        .into_iter()
        .filter(|&t| !is_virtual_thread_object(ctx, t))
        .collect();
    // i7-L2: the thread references are raw addresses held in a Rust
    // `Vec` across the array allocation, which can move them; root
    // each one and store the current address.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let handles: Vec<_> = threads.into_iter().map(|t| scope.root(t)).collect();
    // zr z8/nr, gci i48/zl2: on every collector a refusal collects
    // first unless `CRATONVM_ZGC_NATIVE_DOORS_COLLECT=0`; every thread
    // reference is rooted by the scope and re-read below.
    let len = handles.len();
    let arr = if scope.native_doors_collect_on_refusal() {
        match scope.try_new_ref_array(component, len) {
            Some(arr) => arr,
            None => crate::util_concurrent_ext::retry_after_reclaim(&mut *scope, |ctx| {
                ctx.try_new_ref_array(component, len)
            })
            .unwrap_or_else(|| scope.new_ref_array(component, len)),
        }
    } else {
        scope.new_ref_array(component, len)
    };
    for (i, h) in handles.iter().enumerate() {
        let t = scope.get(h);
        scope.set_array_element(arr, i, Value::Object(Some(t)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `Integer.toString(int)` (`--compatible` override). Named (zr z11/nd) so the
/// native funnel can list it as restartable (`oom_restartable`): a pure
/// function of its primitive argument.
pub fn native_integer_to_string_compat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Object(Some(
        ctx.create_string(&val.to_string()),
    ))))
}

/// `Integer.toHexString(int)` (`--compatible` override); see
/// [`native_integer_to_string_compat`].
pub fn native_integer_to_hex_string_compat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Object(Some(
        ctx.create_string(&format!("{:x}", val as u32)),
    ))))
}

/// `Long.toString(long)` (`--compatible` override); see
/// [`native_integer_to_string_compat`].
pub fn native_long_to_string_compat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Object(Some(
        ctx.create_string(&val.to_string()),
    ))))
}

/// `Double.toString(double)` (`--compatible` override); see
/// [`native_integer_to_string_compat`].
pub fn native_double_to_string_compat(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(d)) => *d,
        _ => 0.0,
    };
    Ok(Some(Value::Object(Some(
        ctx.create_string(&format_double(v)),
    ))))
}

/// `CRATONVM_PROXY_MODULE_PER_LOADER` (default on; zs s5/nd5): `Class.getModule()`
/// of a user loader's generated proxy class answers that loader's dynamic
/// module (the JDK's own, recorded by `Module.defineModule0`), and a module
/// minted for one is filed per loader rather than in the VM's permanent
/// by-name module table. `0` restores the by-name mint (which kept every such
/// loader alive). Read uncached in unit tests (per-thread overrides).
pub fn proxy_module_per_loader_enabled() -> bool {
    // zs s9/nd8: latched once, but an active flag override (a vm unit test's
    // thread override) is read through, so a test outside this crate reaches
    // the kill arm; one relaxed load otherwise.
    if cratonvm_types::flags::overrides_active() {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true    })}

