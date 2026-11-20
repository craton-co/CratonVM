#[allow(unused_imports)]
use super::*;


// ---------------------------------------------------------------------------
// Test harness: tempPrint
// ---------------------------------------------------------------------------
// Reflection helpers
// ---------------------------------------------------------------------------

/// Convert a JVM type descriptor to a Class mirror object.
///
/// Handles primitives ("I" в†’ int.class), object types ("Ljava/lang/String;" в†’ String.class),
/// array types ("[I" в†’ int[].class), and void ("V" в†’ void.class).
/// Loader-faithful variant of [`descriptor_to_class_mirror`]: for an `L`-form
/// reference type, resolve the class through the loader that DEFINED
/// `declaring_class_id` (its own namespace) when the loader-aware gate is on, so
/// reflective `Method`/`Field`/`Constructor` type accessors on a class defined
/// by a child / bytecode-enhancing loader report THAT loader's copy of the type
/// вЂ” matching `Class.forName` and the entity metamodel.
///
/// Without this, a bytecode-enhanced Hibernate entity (`Country`, defined by the
/// `EnhancingClassLoader`) had `getContinent().getReturnType()` resolve the
/// *un-enhanced* `Continent` through the flat global store, while the to-one
/// target type (resolved via `Class.forName` through the same enhancing loader)
/// was the *enhanced* copy вЂ” so `ToOneAttributeMapping`'s
/// `declaredType.isAssignableFrom(targetType)` validation failed with a
/// "mapped with targetEntity=`X`, but the attribute is declared as `X`" error
/// (same name, divergent loader identity).
///
/// It first uses an exact `(loader, name)` namespace probe. If a recorded
/// user-defined loader has not resolved the reference yet, it drives that
/// loader's `loadClass` before falling back to the global
/// [`descriptor_to_class_mirror`] path. This is required because reflection
/// resolves a method descriptor just as bytecode does: returning the global
/// same-named class makes `Method.getReturnType()` disagree with the method's
/// own `new` and `ldc` instructions.
pub fn descriptor_to_class_mirror_via_loader(
    ctx: &mut dyn NativeContext,
    desc: &str,
    declaring_class_id: ClassId,
) -> cratonvm_types::ObjectRef {
    let loader_faithful = crate::classloader::loader_aware_resolution()
        || crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), declaring_class_id.as_u32())
            .is_some_and(|loader| {
                crate::classloader::url_classloader_isolated_from_app(ctx, loader)
            });
    // Arrays inherit the defining loader of their reference component
    // (JVMS 5.3.3). ClassManager currently interns array ClassIds globally, so
    // its canonical array mirror would lose a child loader's component identity.
    // For reflective method metadata, return a descriptor-backed mirror that
    // carries that child loader. native_class_get_component_type below then
    // resolves the component in the same namespace. This preserves the
    // observable Class API without changing the existing VM-wide array layout.
    if loader_faithful && desc.starts_with('[') {
        let mut component = desc;
        while let Some(rest) = component.strip_prefix('[') {
            component = rest;
        }
        if let Some(inner) = component
            .strip_prefix('L')
            .and_then(|s| s.strip_suffix(';'))
        {
            let mut loader_id = ctx.loader_id_of_class(declaring_class_id);
            // A class whose NAME is new to the VM is defined in the global
            // namespace even when a user loader defined it (see
            // `defineClass1`), so its manager entry says "application". The
            // defining-loader side table still knows the real loader, and the
            // array's component is that loader's copy (JVMS 5.3.3) -- the same
            // way the plain `L...;` branch below asks the loader. Without this
            // a CGLIB subclass defined in an `OverridingClassLoader` reported
            // `Callback[]` of the application loader for its
            // `CGLIB$SET_THREAD_CALLBACKS(Callback[])`, so
            // `getDeclaredMethod(.., Callback[].class)` matched where HotSpot
            // throws `NoSuchMethodException` (and Spring's
            // `ConfigurationClassEnhancer` then never fell back).
            if loader_id < 3 {
                if let Some(loader) = crate::classloader::defining_loader_for(
                    crate::classloader::loader_tables(ctx),
                    declaring_class_id.as_u32(),
                ) {
                    if crate::classloader::is_user_defined_loader(ctx, loader) {
                        let ns = crate::classloader::loader_namespace_id(ctx, loader);
                        if ns >= 3 {
                            loader_id = ns as _;
                        }
                    }
                }
            }
            // The component is resolved lazily, so the loader may not have been
            // asked for it yet: ask now (this defines the loader's own copy,
            // exactly as resolving a plain `L...;` parameter does), then look
            // again. A loader that answers with its parent's class leaves the
            // exact lookup empty and the global array below is the right one.
            if loader_id >= 3
                && ctx
                    .class_id_defined_by_loader_exact(inner, loader_id as u32)
                    .is_none()
            {
                let component_desc = format!("L{inner};");
                let _ =
                    descriptor_to_class_mirror_via_loader(ctx, &component_desc, declaring_class_id);
            }
            if loader_id >= 3
                && ctx
                    .class_id_defined_by_loader_exact(inner, loader_id as u32)
                    .is_some()
            {
                // The loader-faithful array class -- the very `ClassId` the
                // class's own `ldc` / `anewarray` of this descriptor resolves
                // to. A descriptor-backed stand-in mirror (below) is a DIFFERENT
                // object, so `field.getType() == X[].class` was false for every
                // class a child loader defined -- and with it
                // `AtomicReferenceFieldUpdater.newUpdater(.., X[].class, ..)`
                // threw `ClassCastException` (log4j's `AppenderControlArraySet`
                // under Spring's forked test class loader).
                if let Some(cid) = ctx.array_class_id_for_loader(desc, loader_id as u32) {
                    return ctx.get_class_mirror(cid);
                }
                if let Some(loader) = crate::classloader::defining_loader_for(
                    crate::classloader::loader_tables(ctx),
                    declaring_class_id.as_u32(),
                ) {
                    return synthetic_class_mirror_for_loader(ctx, desc, loader);
                }
            }
        }
    }
    if loader_faithful {
        if let Some(inner) = desc.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
            let loader_id = ctx.loader_id_of_class(declaring_class_id);
            // Only user-defined namespaces (>= 3) can hold a per-loader copy;
            // built-in loaders (0/1/2) ARE the global store, so the legacy path
            // is already correct for them.
            if loader_id >= 3 {
                if let Some(cid) = ctx.class_id_defined_by_loader_exact(inner, loader_id as u32) {
                    return ctx.get_class_mirror(cid);
                }
            }
            // A real-JDK ClassLoader can define a class whose manager entry
            // still reports the pre-existing application namespace. The
            // defining-loader side table is authoritative for that case. Use
            // it for an exact already-loaded lookup first, then initiate the
            // descriptor through that loader if necessary.
            if let Some(loader) = crate::classloader::defining_loader_for(
                crate::classloader::loader_tables(ctx),
                declaring_class_id.as_u32(),
            ) {
                if let Some(mirror) =
                    crate::classloader::find_loaded_class_for_loader(ctx, loader, inner)
                {
                    return mirror;
                }
                // Native calls are not represented by an interpreter frame,
                // so pin both objects across the potentially allocating /
                // re-entrant loader call. A failure deliberately retains the
                // historical global fallback below.
                let loader_pin = ctx.pin_native_root(loader);
                let name = ctx.create_string(&inner.replace('/', "."));
                let name_pin = ctx.pin_native_root(name);
                let loader = ctx.read_native_pin(loader_pin, loader);
                let name = ctx.read_native_pin(name_pin, name);
                let loaded =
                    crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, name);
                ctx.unpin_native_roots(name_pin);
                ctx.unpin_native_roots(loader_pin);
                if let Ok(Some(Value::Object(Some(mirror)))) = loaded {
                    return mirror;
                }
            }
        }
    }
    // A declaring class in a BUILT-IN loader must resolve its descriptors
    // through the BUILT-IN delegation chain, not through the global by-name
    // store below.
    //
    // The gate above declines for these ("built-in loaders (0/1/2) ARE the
    // global store, so the legacy path is already correct for them"), and that
    // premise holds only while nobody else has defined the same name. Once a
    // USER-DEFINED loader defines its own copy — which is routine: Spring Boot's
    // `@ClassPathExclusions` re-loads the world under a
    // `ModifiedClassPathClassLoader`, and every test using it does this — the
    // global lookup can hand that CHILD's copy to a method whose declaring
    // class is in the application loader.
    //
    // MEASURED, `probes/ReflectiveLoaderFidelityProbe.java`, after one such
    // test has run in the same JVM:
    //
    //     declaringClass  …WebEndpointServletConfiguration  loader=AppClassLoader
    //     paramTypes[1]   …ObjectProvider                   loader=ModifiedClassPathClassLoader
    //
    // i.e. `Method.getParameterTypes()` answered a type from a loader the
    // declaring class cannot even see. Spring's `resolveDependency` decides
    // "is this an ObjectProvider" with `ObjectProvider.class == type`, so the
    // mismatched copy turned an always-satisfiable provider into a failing bean
    // lookup and killed the context — one test poisoning a later one
    // (`boot.jersey…JerseyWebEndpointManagementContextConfigurationTests`).
    // `ResolvableType.resolve()` stayed correct throughout, because it goes
    // through the generic-signature path rather than this one, which is what
    // made the two disagree.
    //
    // Delegation order, and a built-in loader never delegates DOWN: an
    // application-declared name is looked for in bootstrap, then extension,
    // then application, and the first hit wins — the same order
    // `get_loaded_class_id_for_requester` walks. A miss falls through to the
    // global path below unchanged, which still owns arrays, primitives,
    // not-yet-loaded names and synthetics.
    if let Some(inner) = desc.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        let loader_id = ctx.loader_id_of_class(declaring_class_id);
        if (0..=2).contains(&loader_id) {
            // (a) Already loaded somewhere in the built-in chain? Cheap, and the
            // common case.
            for builtin in 0..=(loader_id as u32) {
                if let Some(cid) = ctx.class_id_defined_by_loader_exact(inner, builtin) {
                    return ctx.get_class_mirror(cid);
                }
            }
            // (b) NOT loaded there yet — and this is the half a lookup cannot
            // fix. The global fallback below is a by-name probe with no loader
            // context, so it answers with whichever copy exists; if a
            // user-defined loader got there first, that is the copy it returns,
            // and the mirror caches it forever.
            //
            // The ORDER is what makes this reachable, and it is not exotic.
            // MEASURED with `CRATONVM_DBG_DEFINE_FILTER=beans/factory/ObjectProvider`
            // over the Spring Boot pair that exposed it:
            //
            //     insert name=…/ObjectProvider loader=UserDefined(3) id=2095  <- child, FIRST
            //     insert name=…/ObjectProvider loader=Application    id=2972  <- app, LATER
            //
            // The `Method` mirror is built between those two, when the ONLY
            // definition is the child's. So (a) misses, the fallback collapses
            // onto `UserDefined(3)`, and `Method.getParameterTypes()` reports a
            // class the declaring class cannot see. It is order-dependent in
            // exactly the way that makes it look flaky: resolve the name in the
            // application namespace first and the same code is correct.
            //
            // Initiate through the declaring class's own loader instead —
            // `class_id_by_name_via_referencing_class` is JVMS §5.4.3
            // initiating-loader resolution, and its own contract names this
            // failure ("collapse to whichever loader defined `name` FIRST
            // process-wide"). It RESOLVES rather than initialises, which is the
            // right strength here: building a reflective mirror must not run a
            // parameter type's `<clinit>`.
            //
            // Gated on the fallback actually being about to answer wrong, so
            // the ordinary "not loaded anywhere yet" case keeps its existing
            // cheap path and no Java loader call is added to it.
            let foreign_copy_would_win = user_copy_may_shadow(ctx, inner);
            if foreign_copy_would_win {
                if let Ok(cid) =
                    ctx.class_id_by_name_via_referencing_class(declaring_class_id, inner)
                {
                    return ctx.get_class_mirror(cid);
                }
            }
        }
    }
    // An array's component is resolved by the declaring class's loader too
    // (JVMS 5.3.3), and `descriptor_to_class_mirror` interns arrays by NAME: once
    // a user-defined loader has defined its own copy of the component, the
    // global `[Lp/X;` is that child's array. `XmlSchema.xmlns()` of the
    // application loader's `XmlSchema` then reported `XmlNs[]` of the fork's
    // `XmlNs`, and the JDK's `AnnotationParser` could not build the annotation
    // proxy (`XmlNs referenced from a method is not visible from class loader:
    // 'app'`) -- the `package-info` annotations JAXB reads for `ehcache` lost
    // their namespace.
    if desc.starts_with('[') {
        let loader_id = ctx.loader_id_of_class(declaring_class_id);
        if (0..=2).contains(&loader_id) {
            let inner = desc
                .trim_start_matches('[')
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'));
            if let Some(inner) = inner {
                let foreign_copy_exists = user_copy_may_shadow(ctx, inner);
                if foreign_copy_exists {
                    let defined_by_builtin = (0..=(loader_id as u32))
                        .any(|b| ctx.class_id_defined_by_loader_exact(inner, b).is_some());
                    let component_known = defined_by_builtin
                        || ctx
                            .class_id_by_name_via_referencing_class(declaring_class_id, inner)
                            .is_ok();
                    if component_known {
                        if let Some(cid) = ctx.array_class_id_for_loader(desc, loader_id as u32) {
                            return ctx.get_class_mirror(cid);
                        }
                    }
                }
            }
        }
    }
    descriptor_to_class_mirror(ctx, desc)
}

/// Can a by-name lookup of `inner` (or of an array of it) answer a
/// USER-defined loader's class to a built-in loader's caller?
///
/// `class_id_by_name` fails closed on an ambiguous name, so "the global answer
/// is a user loader's copy" is not the only way a user copy shadows the
/// built-in one: once the application loader has its own copy beside the
/// user's, the name is ambiguous, `class_id_by_name` answers `None`, and a
/// guard reading that as "no foreign copy" let the by-name ARRAY lookup answer
/// the user loader's `[Lp/X;`, the only array class of that name
/// (`R19StandinComponent` / `R19SicDbg`: the application copy's
/// `Method.getReturnType()` was the child loader's array;
/// `docs/feature-designs/native-dispatch-architecture.md`, "Left").
pub fn user_copy_may_shadow(ctx: &dyn NativeContext, inner: &str) -> bool {
    match ctx.classify_class_name(inner) {
        cratonvm_native_api::NameLookup::Unique(cid) => ctx.loader_id_of_class(cid) >= 3,
        cratonvm_native_api::NameLookup::Ambiguous { .. } => true,
        cratonvm_native_api::NameLookup::Absent => false,
    }
}

pub fn descriptor_to_class_mirror(
    ctx: &mut dyn NativeContext,
    desc: &str,
) -> cratonvm_types::ObjectRef {
    match desc {
        "I" => ctx.primitive_class_mirror("int"),
        "Z" => ctx.primitive_class_mirror("boolean"),
        "B" => ctx.primitive_class_mirror("byte"),
        "C" => ctx.primitive_class_mirror("char"),
        "S" => ctx.primitive_class_mirror("short"),
        "J" => ctx.primitive_class_mirror("long"),
        "F" => ctx.primitive_class_mirror("float"),
        "D" => ctx.primitive_class_mirror("double"),
        "V" => ctx.primitive_class_mirror("void"),
        s if s.starts_with('L') && s.ends_with(';') => {
            let class_name = &s[1..s.len() - 1];
            if let Some(class_id) = ctx.class_id_by_name(class_name) {
                return ctx.get_class_mirror(class_id);
            }
            match ctx.load_class(class_name) {
                Ok(Some(Value::Object(Some(mirror)))) => mirror,
                _ => {
                    // `load_class`'s return shape didn't match the expected
                    // Class-object-in-hand variant, but the class may still
                    // have been registered as a side effect of the attempt
                    // (e.g. a loader that registers first, surfaces its
                    // result differently on some paths). Re-check the
                    // canonical registry before minting an uncached
                    // `synthetic_class_mirror` — that mirror is a fresh,
                    // never-registered `ObjectRef` each call, which would
                    // break identity-based comparisons (`HashMap<Class<?>,
                    // V>`, `==`) against the SAME class resolved through
                    // any other path (`ldc`, `Class.forName`, `getClass()`).
                    if let Some(class_id) = ctx.class_id_by_name(class_name) {
                        ctx.get_class_mirror(class_id)
                    } else {
                        synthetic_class_mirror(ctx, class_name)
                    }
                }
            }
        }
        s if s.starts_with('[') => {
            // WP2.2-X: Array type вЂ” return the *canonical* Class mirror
            // (same one ldc/anewarray/etc. produce). This is required for
            // reference identity used by JDK Class.java's `arrayContentsEq`
            // in `searchMethods` (compares `Class<?>` references with `!=`).
            // Without this, a Method whose param type is `int[]` will not
            // match the user's `int[].class` and `getDeclaredMethod`
            // throws NSME. Falls back to a synthetic mirror only if the
            // class manager refuses to register the array class.
            if let Some(class_id) = ctx.class_id_by_name(s) {
                return ctx.get_class_mirror(class_id);
            }
            match ctx.load_class(s) {
                Ok(Some(Value::Object(Some(mirror)))) => mirror,
                _ => synthetic_class_mirror(ctx, s),
            }
        }
        _ => {
            // Unknown descriptor вЂ” return a synthetic mirror.
            synthetic_class_mirror(ctx, desc)
        }
    }
}

/// Create a synthetic java/lang/Class mirror carrying `name` in slot 1.
///
/// C37: the mirror MUST be allocated with `class_id = java/lang/Class` (not
/// `ClassId(0)` / java.lang.Object).  Otherwise `invokevirtual Class.isArray`
/// on the returned mirror walks up Object's superclass chain and raises
/// `NoSuchMethodError: java/lang/Object.isArray()Z`.
pub fn synthetic_class_mirror(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> cratonvm_types::ObjectRef {
    synthetic_class_mirror_in(ctx, name, None)
}

/// [`synthetic_class_mirror`] for a stand-in that belongs to `loader` (a
/// user-defined loader's array type): `classLoader` is written BEFORE the
/// shape fields, so a stand-in array's `componentType` is that loader's
/// component, not whichever same-named class a by-name lookup answers -- and
/// minting the stand-in never makes the application loader load its own copy
/// of the component (lane classshape's CLASSSHAPE19-2; probe
/// `R19StandinComponent`). GC: `loader` is pinned across the allocations.
pub fn synthetic_class_mirror_for_loader(
    ctx: &mut dyn NativeContext,
    name: &str,
    loader: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    synthetic_class_mirror_in(ctx, name, Some(loader))
}

pub fn synthetic_class_mirror_in(
    ctx: &mut dyn NativeContext,
    name: &str,
    loader: Option<cratonvm_types::ObjectRef>,
) -> cratonvm_types::ObjectRef {
    let loader_pin = loader.map(|l| (ctx.pin_native_root(l), l));
    let mirror = synthetic_class_mirror_alloc(ctx, name);
    let loader = loader_pin.map(|(p, l)| ctx.read_native_pin(p, l));
    if let Some(l) = loader {
        ctx.set_field_by_name(mirror, "classLoader", Value::Object(Some(l)));
    }
    // Lane classshape (round 19): with the shape natives yielding to JDK 25's
    // bodies, the stand-in must carry the fields those bodies read.
    let mirror = if cratonvm_types::flags::runtime_flag_default_on(CLASS_SHAPE_JDK_BODY_ENV)
        && primitive_field_slot(ctx).is_some()
    {
        synthetic_mirror_shape_fields(ctx, mirror, name, loader)
    } else {
        mirror
    };
    if let Some((p, _)) = loader_pin {
        ctx.unpin_native_roots(p);
    }
    mirror
}

pub fn synthetic_class_mirror_alloc(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> cratonvm_types::ObjectRef {
    // Resolve java/lang/Class вЂ” ensure it's loaded so we get its real ClassId.
    let class_class_id = ctx
        .ensure_class_initialized("java/lang/Class")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    // Match the real-JDK java.lang.Class field count (19 slots).  Use the
    // loaded class's layout when available so our allocation isn't smaller
    // than what bytecode expects.
    let num_fields = {
        let n = ctx.class_num_total_fields(class_class_id);
        if n >= 19 {
            n
        } else {
            19
        }
    };
    let mirror = ctx.alloc_object(class_class_id, num_fields);
    // GC-safety: `create_string` below can trigger a collection that
    // relocates `mirror`; pin it and re-read the forwarded reference before
    // the `set_field` calls that populate it (mirrors `create_field_object`).
    let mirror_pin = ctx.pin_native_root(mirror);
    let name_str = ctx.create_string(name);
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    ctx.unpin_native_roots(mirror_pin);
    ctx.set_field(mirror, 0, Value::Object(None));
    ctx.set_field(mirror, 1, Value::Object(Some(name_str)));
    ctx.set_field(mirror, 12, Value::Int(0)); // classRedefinedCount
    mirror
}

/// Parse a method descriptor into (parameter type descriptors, return type descriptor).
///
/// e.g. "(ILjava/lang/String;)V" в†’ (["I", "Ljava/lang/String;"], "V")
pub fn parse_descriptor_param_and_return(desc: &str) -> (Vec<String>, String) {
    let mut params = Vec::new();
    // Tolerant: an empty / malformed descriptor (no leading `(`) yields an
    // empty param list and `"V"` return вЂ” the same shape Spring's
    // `SerializableTypeWrapper` proxy handler expects when it has no
    // descriptor cached for an internally-synthesised method.
    if desc.is_empty() || !desc.starts_with('(') {
        return (params, "V".to_string());
    }
    let inner = &desc[1..]; // skip '('
    let mut i = 0;
    let bytes = inner.as_bytes();
    while i < bytes.len() && bytes[i] != b')' {
        let start = i;
        match bytes[i] {
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1; // skip ';'
                params.push(inner[start..i].to_string());
            }
            b'[' => {
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'L' {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else {
                    i += 1; // primitive array element
                }
                params.push(inner[start..i].to_string());
            }
            _ => {
                i += 1;
                params.push(inner[start..i].to_string());
            }
        }
    }
    // Skip ')' to get return type
    let ret_start = i + 1; // after ')'
    let ret_type = inner[ret_start..].to_string();
    (params, ret_type)
}

/// Box a VM Value into a **FRESH** wrapper object.
///
/// e.g. `Value::Int(42)` with type `"I"` produces a `new Integer(42)`-shaped
/// object. It is **not** `Integer.valueOf(42)`, whatever this comment used to
/// claim: the body below calls [`alloc_wrapper`] on every arm and has never
/// consulted a cache. The distinction is observable (`==` on two boxes of the
/// same in-cache value) and it is the entire reason [`box_value_canonical`]
/// exists beside this function.
///
/// This is the correct helper for a caller whose HotSpot counterpart allocates
/// unconditionally — HotSpot's `java_lang_boxing_object::create`, which is what
/// `Reflection::array_get` and the `MethodHandleNatives` VM-info shapes reach.
/// `Array.get` is the archetype and is measured fresh on **both** VMs (it does
/// not call through here at all; `lib.rs::native_array_get` inlines its own
/// `alloc_wrapper` per component type, which is why routing this function
/// wholesale through the caches would *not* have broken it — and why "it
/// would break `Array.get`" is not by itself the reason for the split).
///
/// `"V"` boxes to `null`; anything else is already a reference and passes
/// through untouched.
pub fn box_value(ctx: &mut dyn NativeContext, value: Value, type_desc: &str) -> Value {
    match type_desc {
        "I" => {
            let obj = alloc_wrapper(ctx, "java/lang/Integer");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "J" => {
            let obj = alloc_wrapper(ctx, "java/lang/Long");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "F" => {
            let obj = alloc_wrapper(ctx, "java/lang/Float");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "D" => {
            let obj = alloc_wrapper(ctx, "java/lang/Double");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "Z" => {
            let obj = alloc_wrapper(ctx, "java/lang/Boolean");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "B" => {
            let obj = alloc_wrapper(ctx, "java/lang/Byte");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "S" => {
            let obj = alloc_wrapper(ctx, "java/lang/Short");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "C" => {
            let obj = alloc_wrapper(ctx, "java/lang/Character");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
        "V" => Value::Object(None), // void в†’ null
        _ => value,                 // already an object reference
    }
}

/// Box a VM Value the way the JDK's **reflective accessors** do: through
/// `X.valueOf`, i.e. through the wrapper caches.
///
/// [`box_value`]'s cached sibling. The two are NOT interchangeable and the
/// choice per call site is a measurement, not a preference. Every row below
/// was measured on Microsoft OpenJDK 25.0.3+9 with `scratchpad/f11/
/// BoxCallers.java` / `BoxCallers2.java`, which compare the object a path
/// hands back against `X.valueOf(v)` by `==` (all values inside every bound:
/// int 7, char 'a', byte 3, short 9, long 5, `true`):
///
/// | reflective path | HotSpot 25 | helper |
/// |---|---|---|
/// | `Field.get` (instance + static, I J Z B S C) | CANONICAL | this one |
/// | `Method.invoke` primitive return | CANONICAL | this one |
/// | `MethodHandle` return adaptation (`asType`/`invoke`/`invokeWithArguments`) | CANONICAL | this one |
/// | `VarHandle.get` — field, array element, byte-array view, FFM layout | CANONICAL | this one |
/// | `SerializedLambda.getCapturedArg` | CANONICAL | this one |
/// | `InvocationHandler` `args[]` (proxy parameter boxing) | CANONICAL | this one |
/// | `Array.get` | **FRESH** | [`box_value`] |
/// | `Field.get` / `Method.invoke` of `float`/`double` | **FRESH** | [`box_value`] |
/// | any value outside its type's cache bound | **FRESH** | either — the natives' own uncached arm |
///
/// The JDK's reason for the asymmetry is an implementation detail that is
/// nonetheless observable, and therefore is behaviour: `Field.get` and
/// `Method.invoke` run through `MethodHandle`-based accessors
/// (`MethodHandleIntegerFieldAccessorImpl` etc.), whose boxing step is a
/// direct handle to `Integer.valueOf` — so they inherit the cache. `Array.get`
/// is a VM native (`Reflection::array_get`) that boxes with
/// `java_lang_boxing_object::create`, which allocates and never looks at
/// `IntegerCache`. **Do not "unify" them.** Both directions are asserted by
/// tests below, and the fresh direction is the one no equality-shaped check
/// can see (every "fresh" row above is still `.equals`-equal).
///
/// `F` and `D` deliberately fall through to [`box_value`]: `Float`/`Double`
/// have no cache at all on HotSpot (`Float.valueOf(0f) == Float.valueOf(0f)`
/// is `false`, measured), so "completing the family" to eight is a regression.
///
/// Three implementation notes:
///
/// 1. The descriptor arm is entered only when the `Value` variant MATCHES the
///    descriptor. `native_long_value_of` reads `Some(Value::Long(v))` and
///    **defaults to 0** for anything else, so handing it a `Value::Int` would
///    convert an identity bug into a wrong-value bug. On a mismatch this falls
///    back to [`box_value`], which stores the raw slot verbatim — i.e. exactly
///    today's behaviour for those shapes.
/// 2. A failing or empty native result also falls back to [`box_value`],
///    never to `Value::Object(None)`. Mapping a boxing failure onto `null` is
///    the defect already recorded above `create_method_object` — a reference
///    return silently becoming `null` — and there is no reason to reintroduce
///    it here.
/// 3. No new cached storage is added: this routes callers into the SIX caches
///    that already live in `lang_math.rs` and are already reported by
///    `gc_scan_value_of_cache_roots` **and** remapped by
///    `gc_update_value_of_cache_refs` (both hooks list the same six
///    `*_cache()` accessors, and `vm/src/memory/native_roots.rs` registers
///    them as one `VmRootSource { scan, remap }` pair, so the two cannot be
///    wired independently). A cache that is rooted but not remapped is a
///    use-after-move.
pub fn box_value_canonical(
    ctx: &mut dyn NativeContext,
    value: Value,
    type_desc: &str,
) -> Value {
    // Pure-bytecode lever: `Integer.valueOf` etc. run the JDK's own bytecode, so
    // the cache a program's `Integer.valueOf(7)` hits is the JDK's
    // `IntegerCache`, not the VM-side cache below. Box through the JDK's
    // `valueOf` so a reflective / method-handle return is the SAME object.
    if cratonvm_native_api::unretire::pure_bytecode() {
        let (class, desc) = match (type_desc, value) {
            ("I", Value::Int(_)) => ("java/lang/Integer", "(I)Ljava/lang/Integer;"),
            ("J", Value::Long(_)) => ("java/lang/Long", "(J)Ljava/lang/Long;"),
            ("Z", Value::Int(_)) => ("java/lang/Boolean", "(Z)Ljava/lang/Boolean;"),
            ("B", Value::Int(_)) => ("java/lang/Byte", "(B)Ljava/lang/Byte;"),
            ("S", Value::Int(_)) => ("java/lang/Short", "(S)Ljava/lang/Short;"),
            ("C", Value::Int(_)) => ("java/lang/Character", "(C)Ljava/lang/Character;"),
            _ => ("", ""),
        };
        if !class.is_empty() {
            if let Ok(Some(v @ Value::Object(Some(_)))) =
                ctx.invoke_static(class, "valueOf", desc, &[value])
            {
                return v;
            }
        }
    }
    // Note 1: the `Value` variant is part of the match. A `("J", Value::Int)`
    // shape must NOT reach `native_long_value_of`.
    let cached = match (type_desc, value) {
        ("I", Value::Int(_)) => crate::lang_math::native_integer_value_of(ctx, &[value]),
        ("J", Value::Long(_)) => crate::lang_math::native_long_value_of(ctx, &[value]),
        ("Z", Value::Int(_)) => crate::lang_math::native_boolean_value_of(ctx, &[value]),
        ("B", Value::Int(_)) => crate::lang_math::native_byte_value_of(ctx, &[value]),
        ("S", Value::Int(_)) => crate::lang_math::native_short_value_of(ctx, &[value]),
        ("C", Value::Int(_)) => crate::lang_math::native_character_value_of(ctx, &[value]),
        // "F" / "D" (no cache on HotSpot), "V" (void в†’ null), reference
        // descriptors (pass through), and every mismatched variant.
        _ => return box_value(ctx, value, type_desc),
    };
    // Note 2: only a real object is taken from the native; anything else
    // degrades to the allocating path rather than to `null`.
    match cached {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        _ => box_value(ctx, value, type_desc),
    }
}

/// Unbox a wrapper object to a primitive Value.
///
/// e.g. Integer object в†’ Value::Int(42)
pub fn unbox_value(ctx: &dyn NativeContext, obj: cratonvm_types::ObjectRef) -> Value {
    let class_id = ctx.class_id_of_object(obj);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    match class_name.as_str() {
        "java/lang/Integer"
        | "java/lang/Byte"
        | "java/lang/Short"
        | "java/lang/Character"
        | "java/lang/Boolean" => ctx.get_field(obj, 0),
        "java/lang/Long" => ctx.get_field(obj, 0),
        "java/lang/Float" => ctx.get_field(obj, 0),
        "java/lang/Double" => ctx.get_field(obj, 0),
        _ => Value::Object(Some(obj)), // not a wrapper, return as-is
    }
}

/// Unbox a Value::Object to a primitive based on the expected descriptor.
///
/// If the value is Object(None) (null), returns a default for the type.
/// If the value is already the right primitive type, returns it as-is.
pub fn unbox_arg(ctx: &dyn NativeContext, value: Value, expected_desc: &str) -> Value {
    match expected_desc {
        "I" | "Z" | "B" | "S" | "C" => match value {
            Value::Int(_) => value,
            Value::Object(Some(obj)) => unbox_value(ctx, obj),
            _ => Value::Int(0),
        },
        "J" => match value {
            Value::Long(_) => value,
            Value::Object(Some(obj)) => unbox_value(ctx, obj),
            _ => Value::Long(0),
        },
        "F" => match value {
            Value::Float(_) => value,
            Value::Object(Some(obj)) => unbox_value(ctx, obj),
            _ => Value::Float(0.0),
        },
        "D" => match value {
            Value::Double(_) => value,
            Value::Object(Some(obj)) => unbox_value(ctx, obj),
            _ => Value::Double(0.0),
        },
        _ => value, // object type, no unboxing needed
    }
}

// ---------------------------------------------------------------------------
// Strict reflective coercion вЂ” used by Method.invoke / Constructor.newInstance
// / Field.set to enforce IllegalArgumentException on type mismatches, as
// specified by JLS В§15.12.4.2 and `java.lang.reflect` Javadoc.
// ---------------------------------------------------------------------------

/// Test whether a class name (slash-form, e.g. "java/lang/Integer") refers
/// to the wrapper for a given primitive descriptor.
pub fn wrapper_matches_primitive(wrapper: &str, prim_desc: &str) -> bool {
    matches!(
        (prim_desc, wrapper),
        ("I", "java/lang/Integer")
            | ("J", "java/lang/Long")
            | ("F", "java/lang/Float")
            | ("D", "java/lang/Double")
            | ("Z", "java/lang/Boolean")
            | ("B", "java/lang/Byte")
            | ("S", "java/lang/Short")
            | ("C", "java/lang/Character")
    )
}

/// Widening-primitive-conversion rules (JLS В§5.1.2) for reflective
/// `Field.setInt` / `Method.invoke` coercion. Returns true iff a value
/// whose runtime primitive tag is `src` can be silently widened to `dst`.
pub fn widening_allowed(src: &str, dst: &str) -> bool {
    if src == dst {
        return true;
    }
    match (src, dst) {
        // byte в†’ short, int, long, float, double
        ("B", "S" | "I" | "J" | "F" | "D") => true,
        // short в†’ int, long, float, double
        ("S", "I" | "J" | "F" | "D") => true,
        // char в†’ int, long, float, double
        ("C", "I" | "J" | "F" | "D") => true,
        // int в†’ long, float, double
        ("I", "J" | "F" | "D") => true,
        // long в†’ float, double
        ("J", "F" | "D") => true,
        // float в†’ double
        ("F", "D") => true,
        _ => false,
    }
}

/// Infer the primitive descriptor for a wrapper class name.
pub fn wrapper_to_prim_desc(wrapper: &str) -> Option<&'static str> {
    Some(match wrapper {
        "java/lang/Integer" => "I",
        "java/lang/Long" => "J",
        "java/lang/Float" => "F",
        "java/lang/Double" => "D",
        "java/lang/Boolean" => "Z",
        "java/lang/Byte" => "B",
        "java/lang/Short" => "S",
        "java/lang/Character" => "C",
        _ => return None,
    })
}

/// Convert a primitive-bearing wrapper value to `Value::Int/Long/Float/Double`
/// after widening from `src_prim` to `dst_prim`. Returns None when the
/// requested widening is not a valid primitive conversion.
pub fn widen_primitive_value(value: Value, src_prim: &str, dst_prim: &str) -> Option<Value> {
    if !widening_allowed(src_prim, dst_prim) {
        return None;
    }
    // Extract the underlying numeric bits as i64/f64 so we can widen uniformly.
    let (as_long, as_double): (Option<i64>, Option<f64>) = match value {
        Value::Int(v) => (Some(v as i64), None),
        Value::Long(v) => (Some(v), None),
        Value::Float(v) => (None, Some(v as f64)),
        Value::Double(v) => (None, Some(v)),
        _ => (None, None),
    };
    Some(match dst_prim {
        "B" | "S" | "C" | "I" | "Z" => {
            let v = as_long.or_else(|| as_double.map(|d| d as i64))?;
            Value::Int(v as i32)
        }
        "J" => {
            let v = as_long.or_else(|| as_double.map(|d| d as i64))?;
            Value::Long(v)
        }
        "F" => {
            let v = as_double.or_else(|| as_long.map(|l| l as f64))?;
            Value::Float(v as f32)
        }
        "D" => {
            let v = as_double.or_else(|| as_long.map(|l| l as f64))?;
            Value::Double(v)
        }
        _ => return None,
    })
}

/// Values whose interface set is acquired at RUNTIME and is invisible to any
/// static walk of the loaded hierarchy.
///
/// A lambda / method-reference proxy is not in the hierarchy at all (its
/// `ClassId` is `>= 0x8000_0000`); a `java.lang.reflect.Proxy` and an
/// annotation proxy implement their interfaces by construction; a VM-minted
/// stand-in carries its relationships in the interpreter's
/// `synthetic_implements` table. Refusing any of these on the strength of a
/// hierarchy walk is a FALSE `IllegalArgumentException` on code that works --
/// `hibernate-smoke` storing an `AnnotationProxy` is the canonical one -- so
/// they are never judged.
///
/// `typecheck::aastore_element_assignable` carries the same three hatches and
/// can be more precise about the middle one, because by the time it asks it has
/// the RECORDED interface set of the generated `$ProxyN` in hand. This caller
/// does not, so it declines rather than guesses.
pub fn reflective_value_is_runtime_interfaced(ctx: &dyn NativeContext, arg_cid: ClassId) -> bool {
    if arg_cid.as_u32() >= 0x8000_0000 {
        // A REGISTERED lambda proxy is not imprecise: `is_subclass` (asked
        // first by the caller) routes it through the interpreter's
        // `lambda_proxy_satisfies` (functional interface, markers,
        // serializability), so a refusal there is HotSpot's refusal -- a
        // `Runnable` lambda passed for a `Comparable` formal is `argument type
        // mismatch` (interpreter round i4 wave 85, lane L2). Only an id no
        // proxy is registered under (the autobox sentinel) stays hatched.
        return ctx.lambda_proxy_host(arg_cid).is_none();
    }
    match ctx.class_name_of_id(arg_cid) {
        Some(n) => reflective_name_is_runtime_proxy(&n) || ctx.is_class_synthetic_stub(&n),
        None => true,
    }
}

/// The name screen the reflective and `MethodHandle` argument checks share
/// (`reflective_value_is_runtime_interfaced` above and `lang_invoke`'s
/// `reference_arg_admitted`): a generated `$Proxy<digits>`, the synthetic
/// `Proxy$Instance` shim (the degrade fallback, whose interfaces live in its
/// slot-1 `Class[]`), or an annotation carrier. The older substring test
/// `contains("$Proxy")` also declined a user nested class such as
/// `Outer$ProxyConfig`, so a mistyped argument reached the callee where HotSpot
/// throws `IllegalArgumentException` / `ClassCastException`; and it judged the
/// shim, whose class declares none of its interfaces (round 13 wave 7, lane
/// predicates). `CRATONVM_PROXY_USER_SUBCLASS_ORDINARY=0` restores the
/// substring test.
pub fn reflective_name_is_runtime_proxy(name: &str) -> bool {
    if !crate::reflect_annotations::proxy_user_subclass_is_ordinary() {
        return name.contains("$Proxy") || name.ends_with("AnnotationProxy");
    }
    // The annotation carrier by its exact name: the suffix test
    // (`ends_with("AnnotationProxy")`) also declined a user class such as
    // `com.acme.MyAnnotationProxy`, so a mistyped argument of that class
    // reached the callee where HotSpot throws (JIT round 21, lane p161).
    crate::reflect_annotations::proxy_name_has_generated_shape(name)
        || name == "java/lang/reflect/Proxy$Instance"
        || name == "java/lang/annotation/AnnotationProxy"
}

/// Whether the argument reaches the expected type **by NAME** — one binary
/// name carrying two `ClassId`s, which is a loader split and not a type error.
///
/// This is the reflective-coercion twin of the `aastore` predicate's by-name
/// walk, and it exists for the same reason: CratonVM materialises a second
/// "world" for a library when a user-defined loader has already loaded it, and
/// that world can end up partly aliased to the first one. HotSpot never
/// produces the mix, so there is no HotSpot behaviour to diverge from here —
/// only our own imprecision to avoid manufacturing an exception out of.
///
/// Measured 2026-08-10 on spring-framework's
/// `TestContextAotGeneratorIntegrationTests`: log4j's plugin builder ran in the
/// APPLICATION world (receiver, declaring class and field type all
/// `ClassLoaderId::Application`) and was handed a `Level` from a
/// `@CompileWithForkedClassLoader` world, because `TypeConverterRegistry`,
/// `TypeConverters` and `PluginAttributeVisitor` exist ONLY in the forked
/// worlds — the application loader never got copies of its own. Every
/// `<Logger>` element of `log4j2-test.xml` was then dropped with
/// `Could not create plugin of type … LoggerConfig: argument type mismatch`,
/// 28 times per run, and the suite ran at a log level HotSpot never selects.
///
/// **Deliberately narrow.** It fires only when the argument's own superclass
/// chain carries the expected type's exact binary name. A genuine mismatch —
/// an `Integer` handed to a `String` field — has no such name anywhere in its
/// chain, so the refusal that the rest of this function exists for is
/// untouched. `expected_cid` is passed so a same-`ClassId` argument (already
/// accepted by `is_subclass` above) can never reach here and make the walk
/// look load-bearing when it is not.
pub fn argument_reaches_expected_by_name(
    ctx: &dyn NativeContext,
    arg_cid: ClassId,
    expected_cid: ClassId,
    expected_internal: &str,
) -> bool {
    if arg_cid == expected_cid {
        return false;
    }
    let mut cur = Some(arg_cid);
    for _ in 0..32 {
        let Some(c) = cur else { return false };
        if ctx.class_name_of_id(c).as_deref() == Some(expected_internal) {
            return true;
        }
        cur = ctx.superclass_of(c);
    }
    false
}

/// Strict version of `unbox_arg`: coerces an incoming Object reference to
/// the primitive expected by `expected_desc`, validating that the wrapper
/// class matches (possibly after widening). On mismatch throws
/// `IllegalArgumentException` as per `java.lang.reflect.Method.invoke`.
///
/// For reference-typed parameters, performs an assignability check against
/// `expected_desc`.
///
/// `context` is a short operation label used to construct the exception
/// message (e.g. "argument" or "Field.set").
pub fn coerce_arg_strict(
    ctx: &dyn NativeContext,
    value: Value,
    expected_desc: &str,
    context: &str,
    near: Option<cratonvm_types::ClassId>,
) -> Result<Value, MethodCallFailed> {
    coerce_arg_strict_msg(ctx, value, expected_desc, context, near, None)
}

/// [`coerce_arg_strict`] with the mismatch message chosen by the caller.
///
/// `argument type mismatch` is JDK-faithful for `Method.invoke` and
/// `Constructor.newInstance` -- and ONLY for those. `Field.set` refuses the same
/// values with a sentence that names the field and the value
/// (`Can not set int field P$H.nf to java.lang.String`), so it passes its own in
/// rather than inheriting a message written for a different caller. The note
/// below is still right about where it came from; it was just being applied one
/// caller too widely, which is the same shape as `G66-1` and `G68-1` section 2.
pub fn coerce_arg_strict_msg(
    ctx: &dyn NativeContext,
    value: Value,
    expected_desc: &str,
    context: &str,
    near: Option<cratonvm_types::ClassId>,
    mismatch_message: Option<&str>,
) -> Result<Value, MethodCallFailed> {
    let mismatch = || {
        illegal_arg_exc(
            mismatch_message
                .unwrap_or("argument type mismatch")
                .to_string(),
        )
    };
    // WP2.1-field вЂ” operand-stack tag-erasure recovery for J/D.
    //
    // `CompactValue::to_value()` on an untagged 64-bit slot cannot tell
    // a small-magnitude long from a denormal double вЂ” both round-trip
    // identical bits.  When the operand stack pops a J-typed slot whose
    // bit pattern doesn't match a NaN-tagged subform, the resulting
    // `Value` is tagged `Double` even though the slot holds a long
    // (and vice-versa for a D slot whose bits collide with a tagged
    // subform).  See `types/src/compact_value.rs::to_value` and the
    // descriptor-aware sibling `decode_by_descriptor`.
    //
    // For typed setters, the method descriptor tells us the declared
    // type вЂ” reinterpret the bits accordingly so widening checks below
    // see the right tag.  This sits at the native edge so the
    // VM-internal call-frame plumbing stays untouched.
    let value = match (expected_desc, value) {
        ("J", Value::Double(d)) => Value::Long(d.to_bits() as i64),
        ("D", Value::Long(n)) => Value::Double(f64::from_bits(n as u64)),
        (_, v) => v,
    };
    match expected_desc {
        // Primitive expected
        "I" | "J" | "F" | "D" | "Z" | "B" | "S" | "C" => {
            match value {
                Value::Int(_) if matches!(expected_desc, "I" | "Z" | "B" | "S" | "C") => {
                    // boolean/byte/short/char/int share the same VM stack
                    // representation (Value::Int) вЂ” the caller already holds
                    // a legal payload for any of these slots, so pass it
                    // through without re-checking widening. Typed setter
                    // natives (e.g. setByte) mask the input to the field's
                    // storage width before reaching here.
                    Ok(value)
                }
                Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
                    // Cross-category coercion (e.g. int в†’ long, int в†’ double):
                    // honour JLS В§5.1.2 widening; narrowing is rejected.
                    let src = primitive_tag_of(value);
                    // JDK faithful: `jdk.internal.reflect` accessors throw
                    // `IllegalArgumentException("argument type mismatch")` for ALL
                    // reflective arg-coercion failures (Method.invoke /
                    // Constructor.newInstance). Hibernate's HHH-20261 error message
                    // appends this verbatim ("вЂ¦due to: argument type mismatch"), so
                    // a descriptive message would diverge from HotSpot.
                    let _ = src;
                    widen_primitive_value(value, src, expected_desc).ok_or_else(|| mismatch())
                }
                Value::Object(Some(obj)) => {
                    let wrapper_cid = ctx.class_id_of_object(obj);
                    // The interned name (a refcount bump), not a fresh
                    // `String` (interpreter round i3 wave 76, lane L3).
                    let wrapper_arc = ctx.class_name_arc_of_id(wrapper_cid);
                    let wrapper_name: &str = wrapper_arc.as_deref().unwrap_or("");
                    let src_prim = wrapper_to_prim_desc(wrapper_name).ok_or_else(|| {
                        // JDK-faithful "argument type mismatch" (see note above).
                        mismatch()
                    })?;
                    if !wrapper_matches_primitive(wrapper_name, expected_desc)
                        && !widening_allowed(src_prim, expected_desc)
                    {
                        return Err(mismatch());
                    }
                    // WP2.2 fix: read the wrapper's `value` field by name.
                    // Slot 0 is unreliable when the real JDK Byte/Short/Integer
                    // class has inherited fields ahead of `value` in the layout
                    // (Object header / Number padding). `get_field_by_name`
                    // resolves the descriptor-typed slot regardless of layout.
                    let by_name = ctx.get_field_by_name(obj, "value");
                    let by_slot0 = ctx.get_field(obj, 0);
                    let raw = match by_name {
                        Value::Object(None) => by_slot0,
                        v => v,
                    };
                    widen_primitive_value(raw, src_prim, expected_desc).ok_or_else(|| mismatch())
                }
                Value::Object(None) => Err(mismatch()),
                _ => Err(illegal_arg_exc(format!(
                    "{context}: unexpected VM value for primitive {expected_desc}"
                ))),
            }
        }
        // Reference type expected вЂ” null and assignable references are OK.
        _ => match value {
            // null is assignable to any reference type.
            Value::Object(None) => Ok(value),
            Value::Object(Some(obj)) if expected_desc.starts_with('[') => {
                // An ARRAY formal (interpreter round i1 wave 47, lane L3):
                // passed through unjudged until then, so `strings(String[])`
                // invoked with an `Integer[]` (or a `String`, or an `int[]`)
                // RAN with it, where HotSpot's method handle casts every
                // argument first and refuses with the same "argument type
                // mismatch" (`L3W47ReflectiveArrayArgument.java`).
                // A non-array is never assignable; an array is judged by
                // `checkcast`'s own predicate, and a context that cannot
                // answer (`None`) stays an allow, as for the class formals.
                //
                // Array-ness is the HEAP's answer (`object_is_array`), never a
                // class-name prefix: a reference array's header carries its
                // COMPONENT class id, so `class_name_of_id(class_id_of_object)`
                // of a `String[]` is `java/lang/String`. The first cut of this
                // check tested the prefix and refused every correctly typed
                // array (the wave-47 host run: `L3W47ReflectiveArrayArgument`'s
                // `*-ok` rows, `L4W42ReflectiveSwitchBootstraps`,
                // `L2W27EagerDoorHandlerLocal`, the JaCoCo agent's premain).
                let refuse = if ctx.object_is_array(obj) {
                    ctx.array_value_casts_to_desc(obj, expected_desc) == Some(false)
                } else {
                    // Not an array: refuse, when the context can name its
                    // class at all (a nameless one stays an allow).
                    ctx.class_name_of_id(ctx.class_id_of_object(obj))
                        .is_some_and(|name| !name.starts_with('['))
                };
                if refuse {
                    return Err(mismatch());
                }
                Ok(value)
            }
            Value::Object(Some(obj)) => {
                // JDK contract: `Method.invoke` / `Constructor.newInstance` /
                // `Field.set` throw `IllegalArgumentException("argument type
                // mismatch")` BEFORE dispatch when a reference argument is not
                // assignable to the formal parameter (field) type. CratonVM
                // dispatches without a per-arg `checkcast`, so an unassignable
                // reference would otherwise flow silently into the callee вЂ” e.g.
                // an `Integer` handed to a `String` parameter runs the body and
                // returns a bogus value instead of throwing. (Spring's
                // `SimpleInstantiationStrategy` relies on this IAE to surface
                // "Illegal arguments to factory method".)
                //
                // Be conservative to avoid false rejections: only REJECT when we
                // can positively resolve the formal type to a concrete
                // (non-interface) class and prove the argument is not a subtype.
                // Pass through whenever the formal type is `Object`, an
                // interface, an array, or unresolvable вЂ” those are either always
                // assignable or cannot be checked safely here. This mirrors the
                // receiver assignability check in `native_method_invoke`.
                if expected_desc.starts_with('L') && expected_desc.ends_with(';') {
                    let internal = &expected_desc[1..expected_desc.len() - 1];
                    // t1 lane H: an ARRAY value for a class or interface formal
                    // other than `Object` / `Cloneable` / `Serializable` (JLS
                    // 10.8). The walk below cannot judge it: a reference
                    // array's `class_id_of_object` is its COMPONENT's, so a
                    // `String[]` passed for a `CharSequence` formal (HotSpot:
                    // `IllegalArgumentException`).
                    //
                    // t1 lane V2: and an array for one of those three formals
                    // is ASSIGNABLE, answered here. It used to fall through to
                    // the walk below, which judges the COMPONENT's class id:
                    // a `String[]` for a `Cloneable` formal (String is not
                    // Cloneable) or a `Thread[]` for a `Serializable` one was
                    // refused with "argument type mismatch", where HotSpot
                    // accepts every array for all three.
                    if ctx.object_is_array(obj) {
                        if matches!(
                            internal,
                            "java/lang/Object" | "java/lang/Cloneable" | "java/io/Serializable"
                        ) {
                            return Ok(value);
                        }
                        return Err(mismatch());
                    }
                    if internal != "java/lang/Object" {
                        // Resolve the formal type in the SAME classloader as
                        // the declaring Field/Method/Constructor (`near`)
                        // when known, not a plain global name search. A
                        // class can legitimately be loaded twice under
                        // different loaders with the JVM-spec (loader, name)
                        // identity model -- e.g. Hibernate ORM's bytecode
                        // enhancement reloads an `@EmbeddedId` class under
                        // its own private loader. A global by-name lookup
                        // can then resolve `expected_cid` to the FIRST-ever
                        // loaded (often stale) variant while the actual
                        // value being assigned is a perfectly valid instance
                        // of the variant the declaring class really sees --
                        // rejecting a correctly-typed value as a "mismatch".
                        let resolved = match near {
                            Some(n) => ctx.class_id_by_name_near(internal, n),
                            None => ctx.class_id_by_name(internal),
                        };
                        if let Some(expected_cid) = resolved {
                            // An INTERFACE formal used to be passed through
                            // unjudged, with the note above ("cannot be checked
                            // safely here"). It can be, by the same walk the
                            // `checkcast`/`aastore` path uses --
                            // `class_assignable_to_name` compares NAMES over
                            // supers and interfaces transitively, so it answers
                            // an interface unaided and survives a forked loader
                            // that gives the two sides different `ClassId`s.
                            //
                            // Measured, `probes/ReflectArgTypeSweep.java`:
                            //   takeNamed(Named) invoked with an Integer
                            //     HotSpot   IllegalArgumentException
                            //     CratonVM  InvocationTargetException
                            // -- i.e. the callee RAN, and failed inside itself.
                            // The class-typed row beside it (`m.wrongRef`) was
                            // already correct, which is what made the hole
                            // specifically interface-shaped.
                            //
                            // The three populations whose interfaces are not in
                            // the hierarchy are hatched out by
                            // `reflective_value_is_runtime_interfaced`, and a
                            // context that cannot answer (`None`) stays an
                            // allow.
                            {
                                let arg_cid = ctx.class_id_of_object(obj);
                                let reaches = ctx.is_subclass(arg_cid, expected_cid)
                                    // The interpreter's `synthetic_implements`
                                    // table -- the door built for the VM-minted
                                    // stand-ins whose relationship exists
                                    // nowhere a hierarchy walk can find it.
                                    //
                                    // REGRESSION, caught by re-running the
                                    // family's own probes rather than only the
                                    // new one: judging interface formals (below)
                                    // without this refused
                                    // `Linker.downcallHandle(MemorySegment, ...)`
                                    // with `argument type mismatch`, because the
                                    // `strlen` address is a
                                    // `cratonvm.internal.foreign.MemorySegmentImpl`
                                    // and `is_assignable_to_name` cannot see an
                                    // interface it does not declare.
                                    // `probes/P1RemainingSweep.java`'s
                                    // `ffm.downcallHandle.strlen` went from
                                    // `(MemorySegment)long` to an
                                    // IllegalArgumentException.
                                    //
                                    // `is_class_synthetic_stub` is NOT this
                                    // question and does not answer it -- it is
                                    // about registered synthetic-stub natives.
                                    // Writing a second hatch list instead of
                                    // asking the existing door is what made this
                                    // a regression rather than a non-event.
                                    || ctx.synthetic_implements_declared(arg_cid, internal)
                                    || argument_reaches_expected_by_name(
                                        ctx,
                                        arg_cid,
                                        expected_cid,
                                        internal,
                                    );
                                let refuse = if reaches {
                                    false
                                } else if ctx.is_interface_class(expected_cid) {
                                    !reflective_value_is_runtime_interfaced(ctx, arg_cid)
                                        && ctx.class_assignable_to_name(arg_cid, internal)
                                            == Some(false)
                                } else {
                                    true
                                };
                                if refuse {
                                    if crate::nbflags().dbg_coerce {
                                        let arg_name =
                                            ctx.class_name_of_id(arg_cid).unwrap_or_default();
                                        let near_name = near
                                            .and_then(|n| ctx.class_name_of_id(n))
                                            .unwrap_or_default();
                                        // The superclass chain is the actual
                                        // evidence: a rejected value whose
                                        // chain contains the expected NAME but
                                        // under a different ClassId is a
                                        // loader split, not a type error.
                                        let mut chain = String::new();
                                        let mut cur = Some(arg_cid);
                                        let mut hops = 0;
                                        while let Some(c) = cur {
                                            if hops > 16 {
                                                chain.push_str(" -> ...");
                                                break;
                                            }
                                            hops += 1;
                                            let n = ctx.class_name_of_id(c).unwrap_or_default();
                                            chain.push_str(&format!(
                                                " -> {n}(cid={c:?},loader={:?})",
                                                ctx.loader_id_of_class(c)
                                            ));
                                            cur = ctx.superclass_of(c);
                                        }
                                        eprintln!(
                                            "[DBG_COERCE] {context}: rejecting -- expected={internal} (cid={expected_cid:?}, loader={:?}) arg_class={arg_name} (cid={arg_cid:?}, loader={:?}) near={near:?}/{near_name} (loader={:?}) chain:{chain}",
                                            ctx.loader_id_of_class(expected_cid),
                                            ctx.loader_id_of_class(arg_cid),
                                            near.map(|n| ctx.loader_id_of_class(n)),
                                        );
                                    }
                                    return Err(mismatch());
                                }
                            }
                        }
                    }
                }
                Ok(value)
            }
            // A primitive `Value` reached a reference parameter.
            // JDK-faithful "argument type mismatch" (see note above).
            _ => Err(mismatch()),
        },
    }
}

pub fn primitive_tag_of(v: Value) -> &'static str {
    match v {
        Value::Int(_) => "I",
        Value::Long(_) => "J",
        Value::Float(_) => "F",
        Value::Double(_) => "D",
        _ => "?",
    }
}

/// Build an `IllegalArgumentException` VmError carrying `msg`.
pub fn illegal_arg_exc(msg: String) -> MethodCallFailed {
    cratonvm_types::error::RuntimeError::IllegalArgumentException { message: msg }.into()
}

/// Build a materialized `IllegalArgumentException(msg, cause)` whose cause is
/// a fresh `NullPointerException`, matching HotSpot's
/// `jdk.internal.reflect.*Accessor` behaviour when a reflective
/// `Method.invoke`/`Constructor.newInstance` call passes `null` for a
/// primitive formal parameter: HotSpot's argument unboxing calls
/// `Number.doubleValue()` (etc.) on the null argument, and the resulting
/// `NullPointerException` becomes the `IllegalArgumentException`'s cause
/// (message text is HotSpot's own NPE helper message, not "argument type
/// mismatch" -- but callers only key off the exception TYPE hierarchy, e.g.
/// Spring's `InvocableHandlerMethod.doInvoke`:
///   `(ex.getMessage() == null || ex.getCause() instanceof NullPointerException)
///       ? "Illegal argument" : ex.getMessage()`
/// -- so attaching an NPE cause here (keeping our own descriptive message) is
/// sufficient to match that branch without needing byte-for-byte NPE text).
/// Since interpreter round i1 wave 46 (lane L3) the builder gives HotSpot's
/// texts too: the cause's helpful message, and the cause's `toString()` as
/// the `IllegalArgumentException`'s message; `msg` is the fallback's.
///
/// Falls back to the plain message-only `illegal_arg_exc` if the exception
/// object can't be materialized (e.g. exotic classloader state) -- losing the
/// cause is preferable to losing the exception entirely.
/// Give a reflective coercion refusal the CAUSE HotSpot gives it.
///
/// `Method.invoke` and `Constructor.newInstance` both refuse a bad argument
/// with `IllegalArgumentException("argument type mismatch")` — but the JDK
/// arrives at that by CATCHING something, so the IAE carries a cause, and
/// callers branch on it: Spring's `InvocableHandlerMethod.doInvoke` tests
/// `getCause() instanceof NullPointerException` to choose a friendlier
/// message. A bare message-only IAE matches on TYPE and diverges on that test.
///
/// Extracted from the inline block in `Method.invoke`, which had it, and
/// called from `Constructor.newInstance`, which did not — measured: null into
/// a primitive carried an NPE cause through `invoke` and no cause at all
/// through `newInstance`. Two spellings of one rule was how they came to
/// disagree, so there is now one.
pub fn attach_reflective_coercion_cause(
    ctx: &mut dyn NativeContext,
    arg_val: Value,
    pdesc: &str,
    e: MethodCallFailed,
) -> MethodCallFailed {
    let is_null_into_primitive = matches!(arg_val, Value::Object(None))
        && matches!(pdesc, "I" | "J" | "F" | "D" | "Z" | "B" | "S" | "C");
    let is_plain_iae = matches!(
        &e,
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            cratonvm_types::error::RuntimeError::IllegalArgumentException { .. }
        ))
    );
    if is_null_into_primitive && is_plain_iae {
        return illegal_arg_exc_null_to_primitive(ctx, "argument type mismatch".to_string(), pdesc);
    }
    e
}

/// A reflective call's failed argument check, raised as HotSpot raises it:
/// the check is noted on the call's record
/// (`NativeExceptionAccess::note_reflective_check`) and the failure `build`
/// returns is built by its real constructor HERE, inside the call, so the
/// throwable's trace lists the JDK frames HotSpot lists for that check
/// (`DirectMethodHandleAccessor.checkArgumentCount`, `checkReceiver`, the
/// accessor's `new IllegalArgumentException`, ...; the VM's
/// `stackwalker::reflective_check_entries`). Until wave 46 the check's
/// `RuntimeError` was materialized by the interpreter after the native had
/// returned and left the record, and the trace started at the caller.
/// A failure `build` already built (`ExceptionThrown`), or one that is not a
/// Java exception, is returned as it is; a throwable that cannot be built
/// leaves the `RuntimeError` to the interpreter, as before. Interpreter round
/// i1 wave 46, lane L3 (item 1 of
/// `docs/feature-designs/native-dispatch-architecture.md`).
pub fn raise_reflective_check(
    ctx: &mut dyn NativeContext,
    check: cratonvm_native_api::ReflectiveCheck,
    build: impl FnOnce(&mut dyn NativeContext) -> MethodCallFailed,
) -> MethodCallFailed {
    ctx.note_reflective_check(check);
    // Owned, as `wrap_as_invocation_target_exception` holds it: `re` goes
    // back into the failure on every path that does not build.
    let re = match build(&mut *ctx) {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(re)) => re,
        other => return other,
    };
    let Some((class_name, message)) = re.as_java_throwable() else {
        return MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(re));
    };
    // Own the message so `re` is free again for the failure path below.
    let message = message.map(|m| m.into_owned());
    let built = match &message {
        Some(m) => {
            let msg_obj = ctx.create_string_uninterned(m);
            ctx.new_object_initialized(
                class_name,
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(msg_obj))],
            )
        }
        // No detail message: `getMessage()` is null, so the no-arg constructor.
        None => ctx.new_object_initialized(class_name, "()V", &[]),
    };
    match built {
        Ok(Some(Value::Object(Some(obj)))) => MethodCallFailed::ExceptionThrown(obj),
        _ => MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(re)),
    }
}

/// [`attach_reflective_coercion_cause`] for a failed argument coercion of a
/// reflective call, raised through [`raise_reflective_check`]: a plain
/// `IllegalArgumentException` is the accessor's type arm, or its null arm
/// for a null argument to a primitive parameter. Any other failure is
/// returned as it is, as before.
///
/// `constructor`: the call is `Constructor.newInstance`, whose accessor keeps
/// an argument's `ClassCastException` as the `IllegalArgumentException`'s
/// cause (`DirectConstructorHandleAccessor.newInstance:65`); `Method.invoke`'s
/// twin arm drops it (interpreter round i1 wave 47, lane L3).
pub fn raise_reflective_coercion_failure(
    ctx: &mut dyn NativeContext,
    arg_val: Value,
    pdesc: &str,
    e: MethodCallFailed,
    constructor: bool,
) -> MethodCallFailed {
    use cratonvm_native_api::ReflectiveCheck;
    let is_plain_iae = matches!(
        &e,
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            cratonvm_types::error::RuntimeError::IllegalArgumentException { .. }
        ))
    );
    if !is_plain_iae {
        return e;
    }
    let check = match (arg_val, pdesc.as_bytes()) {
        (Value::Object(None), [p @ (b'I' | b'J' | b'F' | b'D' | b'Z' | b'B' | b'S' | b'C')]) => {
            ReflectiveCheck::NullToPrimitive(char::from(*p))
        }
        (Value::Object(Some(obj)), _) if constructor => {
            match reflective_argument_cast(ctx, obj, pdesc) {
                Some((cast, message)) => {
                    return raise_reflective_check(
                        ctx,
                        ReflectiveCheck::ArgumentCast(cast),
                        |ctx| illegal_arg_exc_with_cast_cause(ctx, &message, e),
                    );
                }
                None => ReflectiveCheck::ArgumentType,
            }
        }
        _ => ReflectiveCheck::ArgumentType,
    };
    raise_reflective_check(ctx, check, |ctx| {
        attach_reflective_coercion_cause(ctx, arg_val, pdesc, e)
    })
}

/// Where and with which message HotSpot's reflection raises the
/// `ClassCastException` of the argument `obj` refused for a parameter of
/// descriptor `pdesc` (`ValueConversions.primitiveConversion` for a primitive,
/// the method handle's `Class.cast` for a reference; JDK 25.0.3, measured by
/// `L3W47ConstructorArgCause.java`), or `None` when the
/// parameter is neither a primitive nor a class or array type. Interpreter
/// round i1 wave 47, lane L3.
pub fn reflective_argument_cast(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    pdesc: &str,
) -> Option<(cratonvm_native_api::ReflectiveArgumentCast, String)> {
    use cratonvm_native_api::ReflectiveArgumentCast as Cast;
    let arg_cid = ctx.class_id_of_object(obj);
    let is_array = ctx.object_is_array(obj);
    let arg_name = reflective_class_display_name(ctx, obj, arg_cid)?;
    let cannot_cast = |to: &str| format!("Cannot cast {arg_name} to {to}");
    let primitive = match pdesc.as_bytes() {
        [p @ (b'I' | b'J' | b'F' | b'D' | b'Z' | b'B' | b'S' | b'C')] => char::from(*p),
        [b'L', .., b';'] => {
            let to = pdesc[1..pdesc.len() - 1].replace('/', ".");
            return Some((Cast::Reference, cannot_cast(&to)));
        }
        [b'[', ..] => return Some((Cast::Reference, cannot_cast(&pdesc.replace('/', ".")))),
        _ => return None,
    };
    // `primitiveConversion`: a `Number`, `Boolean` or `Character` passes its
    // `(Number) x` (or is converted), anything else fails it.
    // An array is none of them (its header's class id is its COMPONENT's, so
    // an `Integer[]` must not read as a `Number`).
    let is_number = !is_array && class_or_super_named(ctx, arg_cid, "java/lang/Number");
    let boxes_a_primitive = is_number
        || (!is_array
            && matches!(
                arg_name.as_str(),
                "java.lang.Boolean" | "java.lang.Character"
            ));
    if !boxes_a_primitive {
        let message = ctx
            .class_id_by_name("java/lang/Number")
            .and_then(|number| ctx.class_cast_message(arg_cid, &arg_name, number))
            .unwrap_or_else(|| {
                format!("class {arg_name} cannot be cast to class java.lang.Number")
            });
        return Some((Cast::NotANumber(primitive), message));
    }
    let wrapper = match primitive {
        'I' => "java.lang.Integer",
        'J' => "java.lang.Long",
        'F' => "java.lang.Float",
        'D' => "java.lang.Double",
        'Z' => "java.lang.Boolean",
        'B' => "java.lang.Byte",
        'S' => "java.lang.Short",
        _ => "java.lang.Character",
    };
    Some((Cast::Unconvertible(primitive), cannot_cast(wrapper)))
}

/// `Class.getName()` of `obj` (whose header class id is `class_id`), arrays
/// included: a reference array's header carries its COMPONENT's class id, so
/// its name is rebuilt from the heap's element kind (`[I`) or the component
/// (`[Ljava.lang.Integer;`, `[[I`), as the interpreter's
/// `typecheck::array_descriptor_of` builds it. Interpreter round i1 wave 47,
/// lane L3.
pub fn reflective_class_display_name(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    class_id: cratonvm_types::ClassId,
) -> Option<String> {
    use cratonvm_types::ArrayElementType as E;
    let internal = match ctx.array_shape(obj) {
        None => ctx.class_name_of_id(class_id)?,
        Some((element, _)) => match element {
            E::Boolean => "[Z".to_string(),
            E::Char => "[C".to_string(),
            E::Float => "[F".to_string(),
            E::Double => "[D".to_string(),
            E::Byte => "[B".to_string(),
            E::Short => "[S".to_string(),
            E::Int => "[I".to_string(),
            E::Long => "[J".to_string(),
            E::Reference => match ctx.class_name_of_id(class_id) {
                Some(component) if component.starts_with('[') => format!("[{component}"),
                Some(component) => format!("[L{component};"),
                None => "[Ljava/lang/Object;".to_string(),
            },
        },
    };
    Some(reflect_text_class_name(ctx, &internal))
}

/// Is `class_id`, or a superclass of it, named `internal`?
pub fn class_or_super_named(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    internal: &str,
) -> bool {
    let mut cur = Some(class_id);
    for _ in 0..64 {
        let Some(c) = cur else { return false };
        if ctx.class_name_of_id(c).as_deref() == Some(internal) {
            return true;
        }
        cur = ctx.superclass_of(c);
    }
    false
}

/// `new IllegalArgumentException("argument type mismatch", new
/// ClassCastException(message))`, as `DirectConstructorHandleAccessor.newInstance`
/// throws it; `fallback` (the plain refusal) when either cannot be built.
pub fn illegal_arg_exc_with_cast_cause(
    ctx: &mut dyn NativeContext,
    message: &str,
    fallback: MethodCallFailed,
) -> MethodCallFailed {
    let cce_msg = ctx.create_string_uninterned(message);
    let cce = match cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/lang/ClassCastException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(cce_msg))],
    ) {
        Ok(Some(obj)) => obj,
        Err(thrown) => return thrown,
        Ok(None) => return fallback,
    };
    // The message's allocation may move the cause.
    let pin = ctx.pin_native_root(cce);
    let iae_msg = ctx.create_string_uninterned("argument type mismatch");
    let cce = ctx.read_native_pin(pin, cce);
    ctx.unpin_native_roots(pin);
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/lang/IllegalArgumentException",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        &[Value::Object(Some(iae_msg)), Value::Object(Some(cce))],
        || fallback,
    )
}

pub fn illegal_arg_exc_null_to_primitive(
    ctx: &mut dyn NativeContext,
    msg: String,
    pdesc: &str,
) -> MethodCallFailed {
    // HotSpot's cause is the `NullPointerException` its
    // `ValueConversions.unbox<Wrapper>` raises at `primitiveConversion(...)
    // .<x>Value()`, with its helpful message, and the accessor wraps it as
    // `new IllegalArgumentException(e)`, whose message is the cause's
    // `toString()` (measured, HotSpot 25.0.3,
    // `L3W46ReflectiveChecks.java` `*-null-to-primitive-kind`;
    // interpreter round i1 wave 46, lane L3). Until wave 46 the message was
    // `msg` and the cause had none.
    let value = match pdesc {
        "J" => "longValue",
        "F" => "floatValue",
        "D" => "doubleValue",
        "B" => "byteValue",
        "S" => "shortValue",
        // `I`, and `Z` / `C`, which `ValueConversions` reads through `intValue`.
        _ => "intValue",
    };
    let npe_msg = ctx.create_string_uninterned(&format!(
        "Cannot invoke \"java.lang.Number.{value}()\" because the return value of \
         \"sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, \
         Object, boolean)\" is null"
    ));
    let npe = match cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/lang/NullPointerException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(npe_msg))],
    ) {
        Ok(Some(obj)) => obj,
        Err(thrown) => return thrown,
        Ok(None) => return illegal_arg_exc(msg),
    };
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/lang/IllegalArgumentException",
        "(Ljava/lang/Throwable;)V",
        &[Value::Object(Some(npe))],
        || illegal_arg_exc(msg),
    )
}

thread_local! {
    /// `CRATONVM_REFLECT_ITE_INIT_MEMO`, latched per thread (`0` unread, `1`
    /// on, `2` off): no per-wrap flag read and no process static.
    pub static ITE_INIT_MEMO: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// nio21 w21 L2: does [`wrap_as_invocation_target_exception`] build its
/// wrapper through `new_object_initialized` (the memoised constructor
/// dispatch)? Default ON.
pub fn ite_init_memo_enabled() -> bool {
    ITE_INIT_MEMO.with(|state| {
        if state.get() == 0 {
            let on =
                true;
            state.set(if on { 1 } else { 2 });
        }
        state.get() == 1
    })
}


/// Wrap a propagated Java exception from a reflective call into
/// `java.lang.reflect.InvocationTargetException(cause)`, matching HotSpot's
/// behaviour for `Method.invoke` and `Constructor.newInstance`.
///
/// If `failure` is an `InternalError` (VM bug or non-Java runtime error),
/// propagate it unchanged вЂ” wrapping would hide the defect.
///
/// If `failure` is an `ExceptionThrown`, allocate a new
/// `InvocationTargetException`, set its `cause` / target via Throwable's
/// `<init>(Throwable)` constructor, and return an `ExceptionThrown` holding
/// that new wrapper. This gives catching code the standard
/// `InvocationTargetException -> getCause() -> original` idiom.
pub fn wrap_as_invocation_target_exception(
    ctx: &mut dyn NativeContext,
    failure: MethodCallFailed,
) -> MethodCallFailed {
    let original = match failure {
        MethodCallFailed::ExceptionThrown(obj) => obj,
        // A native callee raises its Java exception as a `RuntimeError`
        // rather than as a materialized `Throwable` -- `Iterator.remove()`'s
        // `UnsupportedOperationException`, a collection's
        // `ConcurrentModificationException`, an `IllegalStateException` from a
        // half-initialised shim, and so on. HotSpot wraps ANY Throwable the
        // callee throws (Errors included), so these must be materialized and
        // wrapped too -- which is what lets SpEL's `catch (Exception)` turn a
        // constructor's `OutOfMemoryError` into `CONSTRUCTOR_INVOCATION_PROBLEM`
        // (ArrayConstructorTests.errorCases).
        //
        // This arm used to name `OutOfMemoryError` alone, because that was the
        // one variant somebody had hit; every other native-raised exception fell
        // through to `other => return other` and reached the caller RAW.
        // `TestBase.assertThrows` (H2 `TestMVStore.testIterate`) is the shape
        // that finds it: it wraps the receiver in a `Proxy` whose handler
        // catches `InvocationTargetException`, so an unwrapped
        // `UnsupportedOperationException: remove` sails past that catch and out
        // of the test. `RuntimeError::as_java_throwable` is the same table the
        // interpreter's own throw site uses, so the two cannot drift.
        //
        // Only the *dispatch of the callee* is wrapped (see this function's
        // four call sites); argument coercion has already happened by then, so
        // an `IllegalArgumentException` from marshalling still surfaces
        // unwrapped, as `Method.invoke` specifies.
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(re)) => {
            let Some((class_name, message)) = re.as_java_throwable() else {
                // `NotImplemented` -- a VM gap, not something the callee threw.
                return MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                    re,
                ));
            };
            // Own the message so `re` is free again for the failure path below.
            // `into_owned` covers both halves of the `Cow`: a borrowed message
            // is copied out of `re`, a synthesised one is moved.
            let message = message.map(|m| m.into_owned());
            let built = match &message {
                Some(m) => {
                    let msg_obj = ctx.create_string_uninterned(m);
                    ctx.new_object_initialized(
                        class_name,
                        "(Ljava/lang/String;)V",
                        &[Value::Object(Some(msg_obj))],
                    )
                }
                // `None` means "no detail message": `getMessage()` must be
                // null, so the no-arg constructor, not `("")`.
                None => ctx.new_object_initialized(class_name, "()V", &[]),
            };
            match built {
                Ok(Some(Value::Object(Some(obj)))) => obj,
                // Couldn't materialize the throwable -- propagate the original
                // rather than swallow it.
                _ => {
                    return MethodCallFailed::InternalError(
                        cratonvm_types::error::VmError::Runtime(re),
                    )
                }
            }
        }
        // Every other `InternalError` kind (`VmError::Internal` -- real VM
        // defects) still propagates unchanged so we don't mask implementation
        // bugs behind a reflective wrapper.
        other => return other,
    };
    // The wrapped class, for the capture of a wrapper whose constructor
    // pushes no frame (interpreter round i1 wave 48, lane L3;
    // `L3W45ReflectiveRaise.java`, `--compatible` row
    // `ite-npe`). No allocation; a no-op outside a reflective call.
    ctx.note_reflective_raise_target(original);

    // When `InvocationTargetException` is still a synthetic JDK stub (no
    // real `<init>` bytecode), allocate and wire the `target` field without
    // going through `invoke` вЂ” the stub's Throwable-shaped native `<init>`
    // list does not model JDK 7+ `InvocationTargetException(Throwable)`.
    let target_class = "java/lang/reflect/InvocationTargetException";
    if ctx.is_class_synthetic_stub(target_class) {
        // GC-safety: `new_object` can trigger a collection that relocates
        // `original` (materialized well before this point); pin it before
        // allocating `wrapper`.
        let original_pin = ctx.pin_native_root(original);
        let new_result = ctx.new_object(target_class);
        let original = ctx.read_native_pin(original_pin, original);
        let wrapper = match new_result {
            Ok(Some(Value::Object(Some(obj)))) => obj,
            _ => return MethodCallFailed::ExceptionThrown(original),
        };
        // `invoke_special` runs a real Java `<init>` body, which can itself
        // allocate; pin `wrapper` too (the earlier `original_pin` handle
        // still covers it once released together below) and re-read both
        // forwarded references before their next use.
        let wrapper_pin = ctx.pin_native_root(wrapper);
        let _ = ctx.invoke_special(
            "java/lang/ReflectiveOperationException",
            "<init>",
            "()V",
            &[Value::Object(Some(wrapper))],
        );
        let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
        let original = ctx.read_native_pin(original_pin, original);
        ctx.unpin_native_roots(original_pin);
        let _ = ctx.set_field_by_name(wrapper, "target", Value::Object(Some(original)));
        return MethodCallFailed::ExceptionThrown(wrapper);
    }

    // nio21 w21 L2 (`r21-ite-reflective-invoke-pays-the-by-name-ladder-twice-CLOSED-20261012.md`
    // item 2): the wrapper through `new_object_initialized`, whose `<init>`
    // dispatch goes through the VM's native->Java callback memo
    // (`native_callee_memo`): the first wrap resolves the constructor through
    // the whole by-name ladder and, when that ended in the ordinary bytecode
    // tail, every later wrap replays the tail's `interpreter::execute` on the
    // same declaring class. The `<init>` frame, its argument and so the
    // reflective raise splice (`attach_reflective_raise`) are the ones the
    // by-name route builds; a failure falls back to the original exactly as
    // below. Kill switch `CRATONVM_REFLECT_ITE_INIT_MEMO=0` restores the
    // allocate-then-`invoke` route.
    if ite_init_memo_enabled() {
        let original_pin = ctx.pin_native_root(original);
        let built = ctx.new_object_initialized(
            target_class,
            "(Ljava/lang/Throwable;)V",
            &[Value::Object(Some(original))],
        );
        let original = ctx.read_native_pin(original_pin, original);
        ctx.unpin_native_roots(original_pin);
        return match built {
            Ok(Some(Value::Object(Some(wrapper)))) => MethodCallFailed::ExceptionThrown(wrapper),
            _ => MethodCallFailed::ExceptionThrown(original),
        };
    }

    // Attempt to allocate and initialise
    // `java.lang.reflect.InvocationTargetException(Throwable)`. If any step
    // fails, fall back to the original exception rather than masking it.
    //
    // GC-safety: `new_object` below can relocate `original`; pin it first
    // and re-read the forwarded reference before it's used as the `invoke`
    // argument / fallback return value.
    let original_pin = ctx.pin_native_root(original);
    let new_result = ctx.new_object(target_class);
    let original = ctx.read_native_pin(original_pin, original);
    let wrapper = match new_result {
        Ok(Some(Value::Object(Some(obj)))) => obj,
        _ => return MethodCallFailed::ExceptionThrown(original),
    };

    // Preferred path: InvocationTargetException(Throwable target).
    // GC-safety: `invoke` runs the exception's real Java `<init>`, which can
    // itself allocate; pin `wrapper` across it too and re-read both
    // forwarded references before their next use (both are read again in
    // the `match` below).
    let wrapper_pin = ctx.pin_native_root(wrapper);
    let init_result = ctx.invoke(
        target_class,
        "<init>",
        "(Ljava/lang/Throwable;)V",
        &[Value::Object(Some(wrapper)), Value::Object(Some(original))],
    );
    let wrapper = ctx.read_native_pin(wrapper_pin, wrapper);
    let original = ctx.read_native_pin(original_pin, original);
    ctx.unpin_native_roots(original_pin);
    match init_result {
        Ok(_) => MethodCallFailed::ExceptionThrown(wrapper),
        // If the two-arg constructor is unavailable, fall back to the
        // original вЂ” still catchable by Java code, just not wrapped.
        Err(_) => MethodCallFailed::ExceptionThrown(original),
    }
}

/// Which reflective accessor a natively served call stands for: JDK 25's
/// `DirectMethodHandleAccessor` (`Method.invoke`) or
/// `DirectConstructorHandleAccessor` (`Constructor.newInstance`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReflectiveAccessor {
    Method,
    Constructor,
}

impl ReflectiveAccessor {
    /// The accessor's internal class name.
    pub fn class_name(self) -> &'static str {
        match self {
            ReflectiveAccessor::Method => "jdk/internal/reflect/DirectMethodHandleAccessor",
            ReflectiveAccessor::Constructor => {
                "jdk/internal/reflect/DirectConstructorHandleAccessor"
            }
        }
    }

    /// The other accessor's internal class name (`AccessorUtils.
    /// isIllegalArgument`: a frame of it means the exception came from the
    /// reflected body).
    pub fn other_class_name(self) -> &'static str {
        match self {
            ReflectiveAccessor::Method => ReflectiveAccessor::Constructor.class_name(),
            ReflectiveAccessor::Constructor => ReflectiveAccessor::Method.class_name(),
        }
    }
}

/// JDK 25's `AccessorUtils.isIllegalArgument(accessorType, e)` over `trace`
/// (INNERMOST first, hidden frames already left out as the capture leaves
/// them out): is the exception's top visible frame the accessor itself (past
/// `java.base` reflection / method-handle implementation frames)?
/// `java_base(entry)` answers `frame.getModuleName().equals("java.base")`.
/// Pure; interpreter round i1 wave 52, lane L4.
pub fn accessor_reads_illegal_argument(
    trace: &[cratonvm_native_api::registry::StackTraceEntry],
    accessor: ReflectiveAccessor,
    java_base: impl Fn(&cratonvm_native_api::registry::StackTraceEntry) -> bool,
) -> bool {
    const IMPL_PACKAGES: [&str; 4] = [
        "java/lang/reflect",
        "java/lang/invoke",
        "jdk/internal/reflect",
        "sun/invoke/util",
    ];
    let Some(first) = trace.first() else {
        return false;
    };
    let internal = |name: &str| name.replace('.', "/");
    let mut i = 0;
    let (c, m) = (internal(&first.class_name), &*first.method_name);
    // `Class::cast` and (sic) `Objects::requiresNonNull` are skipped as the
    // JDK skips them; the second names no method, as in the JDK.
    if (c == "java/lang/Class" && m == "cast")
        || (c == "java/util/Objects" && m == "requiresNonNull")
    {
        i = 1;
    }
    for frame in &trace[i..] {
        let cname = internal(&frame.class_name);
        if cname == accessor.class_name() {
            return true;
        }
        if !java_base(frame) {
            return false;
        }
        // A hidden class's name ends in `/0x…`; its package is the part
        // before its simple name, as `lastIndexOf('.')` reads `p.C/0x…`.
        let base = match cname.rsplit_once('/') {
            Some((head, last)) if last.starts_with("0x") => head,
            _ => cname.as_str(),
        };
        let pn = base.rsplit_once('/').map_or("", |(p, _)| p);
        if !IMPL_PACKAGES.contains(&pn) {
            return false;
        }
        if cname.starts_with(accessor.other_class_name()) {
            return false;
        }
    }
    false
}

/// [`wrap_as_invocation_target_exception`] for the callee failure of a
/// natively served `Method.invoke` / `Constructor.newInstance`, with the JDK
/// accessor's `catch` arms in front of it: a `NullPointerException`,
/// `ClassCastException` or `WrongMethodTypeException` whose top visible frame
/// is the accessor ([`accessor_reads_illegal_argument`]) is an
/// `IllegalArgumentException` -- `new IllegalArgumentException(e)` for the
/// NPE, `("argument type mismatch")` for `Method.invoke`'s CCE / WMTE and
/// `("argument type mismatch", e)` for the constructor's.
///
/// That is what an exception raised in a HIDDEN callee's own frame looks like
/// on HotSpot, whose backtrace leaves the hidden frame out (measured on
/// 25.0.3: a `Lookup.defineHiddenClass` method's `o.hashCode()` of a null
/// argument, `Method.invoke` -> `IllegalArgumentException:
/// java.lang.NullPointerException`; probes `L4W51HiddenFrameNpe`,
/// `L4W52HiddenReflectiveFailure`; page
/// `interpreter-L4-an-npe-in-a-hidden-frame-is-reported-as-the-frames-own-FIXED-20261016.md`).
/// Judged only when one of `callee_classes` (the declaring class, the
/// receiver's class) is hidden, because a callee the VM serves natively
/// pushes no frame and its exception's top frame would read as the
/// accessor's; and only for a failure that is already a thrown object (a
/// `RuntimeError` a native raised is the native's own). Interpreter round i1
/// wave 52, lane L4.
pub fn wrap_reflective_callee_failure(
    ctx: &mut dyn NativeContext,
    failure: MethodCallFailed,
    accessor: ReflectiveAccessor,
    callee_classes: [Option<ClassId>; 2],
) -> MethodCallFailed {
    if let MethodCallFailed::ExceptionThrown(exc) = failure {
        if callee_classes
            .iter()
            .flatten()
            .any(|&cid| ctx.is_class_hidden(cid))
        {
            // Building the `IllegalArgumentException` may collect: the
            // original is read back from its pin for the wrapping path.
            let pin = ctx.pin_native_root(exc);
            let iae = hidden_callee_illegal_argument(ctx, exc, accessor);
            let exc = ctx.read_native_pin(pin, exc);
            ctx.unpin_native_roots(pin);
            return match iae {
                Some(iae) => iae,
                None => {
                    wrap_as_invocation_target_exception(ctx, MethodCallFailed::ExceptionThrown(exc))
                }
            };
        }
    }
    wrap_as_invocation_target_exception(ctx, failure)
}

/// The `IllegalArgumentException` [`wrap_reflective_callee_failure`] raises
/// for `exc`, or `None` when the accessor would wrap it. Cold.
#[cold]
#[inline(never)]
pub fn hidden_callee_illegal_argument(
    ctx: &mut dyn NativeContext,
    exc: ObjectRef,
    accessor: ReflectiveAccessor,
) -> Option<MethodCallFailed> {
    let class = ctx.class_id_of_object(exc);
    let is = |ctx: &dyn NativeContext, name: &str| {
        ctx.class_assignable_to_name(class, name) == Some(true)
    };
    let npe = is(&*ctx, "java/lang/NullPointerException");
    if !npe
        && !is(&*ctx, "java/lang/ClassCastException")
        && !is(&*ctx, "java/lang/invoke/WrongMethodTypeException")
    {
        return None;
    }
    // Stored OUTERMOST first.
    let mut trace = ctx.get_throwable_stack_trace(exc)?;
    trace.reverse();
    let java_base = |e: &cratonvm_native_api::registry::StackTraceEntry| match e.class_id {
        Some(cid) => ctx.module_name_of_class(cid).as_deref() == Some("java.base"),
        // A frame the VM synthesized for a call it serves (the accessor's,
        // `Method.invoke`'s): a JDK frame.
        None => {
            let n = e.class_name.replace('.', "/");
            n.starts_with("java/") || n.starts_with("jdk/internal/") || n.starts_with("sun/")
        }
    };
    if !accessor_reads_illegal_argument(&trace, accessor, java_base) {
        return None;
    }
    if crate::nbflags().iae_trace {
        eprintln!(
            "[Method.invoke] hidden callee's exception read as an argument error ({accessor:?})"
        );
    }
    let built = if npe || accessor == ReflectiveAccessor::Constructor {
        let pin = ctx.pin_native_root(exc);
        let built = if npe {
            cratonvm_native_api::throwable_builder::construct_new(
                ctx,
                "java/lang/IllegalArgumentException",
                "(Ljava/lang/Throwable;)V",
                &[Value::Object(Some(exc))],
            )
        } else {
            let msg = ctx.create_string_uninterned("argument type mismatch");
            let exc = ctx.read_native_pin(pin, exc);
            cratonvm_native_api::throwable_builder::construct_new(
                ctx,
                "java/lang/IllegalArgumentException",
                "(Ljava/lang/String;Ljava/lang/Throwable;)V",
                &[Value::Object(Some(msg)), Value::Object(Some(exc))],
            )
        };
        ctx.unpin_native_roots(pin);
        built
    } else {
        // `Method.invoke`'s arm: no cause, "to be consistent with the old
        // behavior".
        let msg = ctx.create_string_uninterned("argument type mismatch");
        cratonvm_native_api::throwable_builder::construct_new(
            ctx,
            "java/lang/IllegalArgumentException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(msg))],
        )
    };
    match built {
        Ok(Some(iae)) => Some(MethodCallFailed::ExceptionThrown(iae)),
        Err(thrown) => Some(thrown),
        Ok(None) => None,
    }
}


// ---------------------------------------------------------------------------
// setAccessible implementations
// ---------------------------------------------------------------------------

/// Shared body of `{Field,Method,Constructor}.setAccessible(boolean)`.
///
/// When `flag == true` this enforces JEP 403 strong encapsulation:
///
///   1. Read the declaring class mirror from field 0 of the AccessibleObject.
///   2. Resolve the caller class by walking the Java stack, skipping
///      reflection-internal frames.
///   3. Delegate to `NativeContext::check_deep_reflection_access` for the
///      `accessor в†’ target` edge.
///   4. On failure, throw `java.lang.reflect.InaccessibleObjectException`
///      with a message identifying the denied module/package вЂ” matching
///      HotSpot's message format closely enough to be recognizable.
///
/// When `flag == false` the check is skipped (clearing the override cannot
/// fail).
pub fn set_accessible_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    flag_field_index: usize,
    member_label: &'static str,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None), // Java would have NPE'd earlier вЂ” be lenient
    };
    let flag = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Only the `true` case needs the deep-reflection check.
    enforce_set_accessible_gate(ctx, this, flag, member_label)?;
    ctx.set_field(this, flag_field_index, Value::Int(flag));
    Ok(None)
}

/// `java.base` does not `opens java.lang`, so classpath (unnamed-module) code
/// cannot `setAccessible(true)` the protected `ClassLoader.defineClass`
/// overloads. HotSpot enforces this; CratonVM's general module gate cannot,
/// because classes loaded from the runtime image carry no `module_name` and so
/// look like the unnamed module to
/// [`check_deep_reflection_access`] — every `java.lang` target is then an
/// unnamed→unnamed edge and is allowed.
///
/// Populating `module_name` for the whole boot image is the real repair, and it
/// is a much larger change: it would start denying every `setAccessible` into
/// `java.lang` that this VM currently permits. This encodes the ONE edge that
/// has been measured against HotSpot and that demonstrably changes behaviour.
///
/// Why it matters. Spring CGLIB's `ReflectUtils` tries, in order: `Lookup
/// .defineClass` when the requested loader equals the neighbour class's loader;
/// then the reflective `ClassLoader.defineClass`; then `Lookup.defineClass` on
/// the neighbour class *even though the loaders differ*. On HotSpot the middle
/// option is unavailable, so an AOP proxy for a class in another loader lands
/// in **its superclass's** loader — the same runtime package — and its
/// package-private overrides really override. CratonVM allowed the middle
/// option, so the proxy landed in the requested loader instead; a
/// package-private method then cannot be overridden across the runtime-package
/// boundary (JVMS 5.4.5), and `invokevirtual` on the proxy silently ran the
/// superclass body. That is what made `@MockitoSpyBean` stubs vanish behind a
/// Spring AOP proxy in AOT replay
/// (`MockitoSpyBeanAndSpringAopProxyIntegrationTests`, 4/4 fail): no
/// interceptor frame, no Mockito advice, just the real method.
///
/// Returns `Err(message)` when the access must be denied. JDK-internal callers
/// keep their access — this is the same trust rule
/// `check_reflection_module_access_with_target_id` applies.
/// `CRATONVM_DBG_SETACC=1` — report who asked to make
/// `ClassLoader.defineClass` accessible, and whether it was denied.
pub fn dbg_set_accessible() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok())}

pub fn check_class_loader_define_class_is_encapsulated(
    ctx: &mut dyn NativeContext,
    accessible_object: ObjectRef,
) -> Result<(), String> {
    // Read the declaring class BY NAME: the real-JDK `Method` layout starts
    // with `AccessibleObject.override`, not with `clazz`, so slot 0 is only the
    // declaring-class mirror in the synthetic layout.
    //
    // Every `setAccessible(true)` in the process reaches this, so the
    // declaring-class test comes first and nothing else is read until it hits.
    let declaring = match ctx.get_field_by_name(accessible_object, "clazz") {
        Value::Object(Some(m)) => Some(m),
        _ => match ctx.get_field(accessible_object, 0) {
            Value::Object(Some(m)) => Some(m),
            _ => None,
        },
    };
    let Some(declaring) = declaring else {
        return Ok(());
    };
    match mirror_class_name(ctx, declaring).as_deref() {
        Some("java/lang/ClassLoader") => {}
        _ => return Ok(()),
    }
    let member = match ctx.get_field_by_name(accessible_object, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if member != "defineClass" {
        return Ok(());
    }
    let trace = dbg_set_accessible();
    // Trust the JDK's own frames, and VM-internal calls with no Java caller.
    let Some(accessor_cid) = resolve_caller_class_id(ctx) else {
        return Ok(());
    };
    let accessor_name = ctx.class_name_of_id(accessor_cid);
    let accessor_loader_id = ctx.loader_id_of_class(accessor_cid);
    if trace {
        eprintln!(
            "[setacc] ClassLoader.defineClass requested by {accessor_name:?} \
             loader={accessor_loader_id}"
        );
    }
    if caller_is_jdk_internal(accessor_name.as_deref(), accessor_loader_id) {
        return Ok(());
    }
    // `--add-opens=java.base/java.lang=ALL-UNNAMED` is precisely the grant the
    // denial message below tells the user to add, and HotSpot honours it. This
    // check used to deny unconditionally, so the flag was a no-op: Spring's own
    // Gradle test task passes it to EVERY test JVM (buildSrc
    // `TestConventions.java`), and without the resulting
    // `ClassLoader.defineClass` access Spring-CGLIB cannot define any generated
    // class at all — `BshScriptFactoryTests` 5/18 and `Spr15042Tests` 0/1 under
    // CratonVM, both 18/18 and 1/1 on HotSpot *because of* the flag.
    //
    // A runtime `java.lang.Module.addOpens(...)` lands in the same registry, so
    // an agent that opens the package at runtime is honoured too. With neither,
    // nothing changes: the registry has no `java.base` open edge and the denial
    // stands, which is what keeps CratonVM matching a bare `java` invocation.
    // Ask the module registry the SAME question the general gate asks: the
    // last thing `check_reflection_module_access_with_target_id` does is this
    // call, and it is what already makes `setAccessible` on `String.value`
    // track HotSpot exactly - denied bare, allowed under
    // `--add-opens=java.base/java.lang=ALL-UNNAMED`. Routing this carve-out
    // through the same query is what keeps the two from disagreeing about a
    // grant the user did make.
    //
    // The two narrower queries below could not see that grant. `ALL-UNNAMED`
    // is not an unqualified open, so `is_package_open_unqualified` says no;
    // and a classpath caller has no module name at all, so
    // `module_name_of_class` answers `None` and the second arm never runs.
    // Both escape hatches were therefore dead code and the flag was a no-op
    // here, which is the whole of `BshScriptFactoryTests` 5/18 and
    // `Spr15042Tests` 0/1. They are kept as a fallback for the one input the
    // delegation cannot take: a mirror with no resolvable `ClassId`.
    if let Some(target_cid) = mirror_class_id(ctx, declaring) {
        if ctx
            .check_deep_reflection_access(accessor_cid, target_cid)
            .is_ok()
        {
            if trace {
                eprintln!("[setacc] allowed: java.base opens java.lang to the accessor");
            }
            return Ok(());
        }
    }
    if ctx.is_package_open_unqualified("java.base", "java/lang") {
        if trace {
            eprintln!("[setacc] allowed: java.base/java.lang is open (unqualified)");
        }
        return Ok(());
    }
    if let Some(accessor_module) = ctx.module_name_of_class(accessor_cid) {
        if ctx.is_package_open_to("java.base", "java/lang", &accessor_module) {
            if trace {
                eprintln!("[setacc] allowed: java.base/java.lang opens to {accessor_module}");
            }
            return Ok(());
        }
    }
    Err(
        "Unable to make protected final java.lang.Class java.lang.ClassLoader.defineClass(\
         java.lang.String,byte[],int,int,java.security.ProtectionDomain) throws \
         java.lang.ClassFormatError accessible: module java.base does not \"opens java.lang\" \
         to unnamed module"
            .to_string(),
    )
}

/// Access flags consulted by the `setAccessible` gate. The JDK's
/// `AccessibleObject.checkCanSetAccessible` reads the *member's* modifiers and
/// the *declaring class's* modifiers, not only the module edge.
pub const SA_ACC_PUBLIC: i32 = 0x0001;
pub const SA_ACC_STATIC: i32 = 0x0008;
pub const SA_ACC_PROTECTED: i32 = 0x0004;

/// Read the declaring-class mirror off a `Field`/`Method`/`Constructor`.
///
/// By name first: the real-JDK `Method` layout starts with
/// `AccessibleObject.override`, so slot 0 holds the mirror only in the
/// synthetic layout. Slot 0 is the fallback for objects built with positional
/// slots and no named fields.
pub fn set_accessible_declaring_mirror(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<ObjectRef> {
    if let Value::Object(Some(m)) = ctx.get_field_by_name(this, "clazz") {
        return Some(m);
    }
    match ctx.get_field(this, 0) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    }
}

/// The carve-out JDK 17+ keeps for *exported* (as opposed to *opened*)
/// packages.
///
/// `AccessibleObject.checkCanSetAccessible` does not stop at `opens`. Once that
/// test fails it asks a second question:
///
/// ```text
/// if (declaringClass is public && declaringModule.isExported(pkg, callerModule)) {
///     if (member is public)                        return;   // allowed
///     if (member is protected && member is static
///         && isSubclassOf(caller, declaringClass)) return;   // allowed
/// }
/// ```
///
/// So `setAccessible(true)` on `java.lang.String.length()` succeeds on HotSpot
/// with no `--add-opens`, while the same call on the private field
/// `java.lang.String.hash` throws. A gate that consults only `opens` gets the
/// first of those wrong, and it is by far the more common call - which is why
/// this is a step of its own rather than a footnote.
///
/// The `isSubclassOf` conjunct is the JDK's, not an embellishment, and it
/// applies to the SECOND arm only - a public member needs no relationship to its
/// declaring class. Measured A/B on the `protected static final int`
/// `java.io.PipedInputStream.PIPE_SIZE`, java.io being exported and not opened:
///
/// ```text
/// caller extends PipedInputStream            setAccessible(true)  OK
/// caller extends a subclass of it            setAccessible(true)  OK
/// caller unrelated to it                     InaccessibleObjectException
/// caller unrelated, but member public        setAccessible(true)  OK
/// caller unrelated, protected INSTANCE field InaccessibleObjectException
/// ```
///
/// The subclass walk is [`crate::lang_reflect::is_subclass_or_unreadable`] and
/// NOT `NativeContext::is_subclass`, deliberately. This conjunct only ever
/// produces a NEW `InaccessibleObjectException`, and `is_subclass` is
/// two-valued: a `--synthetic-jdk` fabricated stand-in has no modelled supertype
/// chain and would answer "not a subclass", earning a refusal HotSpot does not
/// raise, on a gate whose vm suite is blocking at zero failures. Failing open on
/// an unreadable chain restores exactly the pre-conjunct answer for those.
///
/// Returns `true` only where the JDK would allow the access. Every unreadable
/// input (no modifiers slot, unresolvable declaring class) answers `false`, so
/// the caller's refusal stands.
pub fn set_accessible_export_carve_out(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    target_cid: ClassId,
) -> bool {
    let member_mods = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => return false,
    };
    let member_is_public = (member_mods & SA_ACC_PUBLIC) != 0;
    let member_is_protected_static =
        (member_mods & SA_ACC_PROTECTED) != 0 && (member_mods & SA_ACC_STATIC) != 0;
    if !member_is_public && !member_is_protected_static {
        return false;
    }
    if (i32::from(ctx.class_access_flags(target_cid)) & SA_ACC_PUBLIC) == 0 {
        return false;
    }
    // No resolvable caller frame means the VM itself is driving the reflection.
    // `check_reflection_module_access` allows that case outright, so reaching
    // here without a caller would be a contradiction; answer permissively to
    // keep the two in agreement rather than inventing a third policy.
    let Some(accessor_cid) = resolve_caller_class_id(ctx) else {
        return true;
    };
    // The protected-static arm alone carries the subclass conjunct, and only
    // when the member is not ALSO public (a public member takes the first arm
    // and never reaches this test).
    if !member_is_public
        && !crate::lang_reflect::is_subclass_or_unreadable(ctx, accessor_cid, target_cid)
    {
        return false;
    }
    ctx.reflective_export_to_accessor(accessor_cid, target_cid)
}

/// A served `setAccessible(true)` refuses: HotSpot 25 raises the
/// `InaccessibleObjectException` four hops below the served method --
/// `<Member>.setAccessible` at its call of `checkCanSetAccessible(Class)`,
/// that at its call of `AccessibleObject.checkCanSetAccessible(Class, Class)`,
/// that at its call of the three-argument overload, that at its LAST call of
/// `throwInaccessibleObjectException` (the module refusal; the earlier one is
/// a null caller's), which constructs the throwable (measured, JDK 25.0.3;
/// probe `L3W60CompatibleServedReflectiveFrames`, row `direct-setAccessible`).
/// The refusal notes that chain for the funnel's mark
/// (`NativeExceptionAccess::note_served_raise_chain`), as
/// `lang_stackwalker::note_descriptor_retain_refusal_chain` does. Only for a
/// receiver of the three member classes (each declares the first two hops
/// with this shape). Interpreter round i2 wave 61, lane L3.
#[cold]
#[inline(never)]
pub fn note_set_accessible_refusal_chain(ctx: &mut dyn NativeContext, this: ObjectRef) {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const AO: &str = "java/lang/reflect/AccessibleObject";
    const ONE_CLASS: &str = "(Ljava/lang/Class;)V";
    const TWO_CLASSES: &str = "(Ljava/lang/Class;Ljava/lang/Class;)V";
    const WITH_FLAG: &str = "(Ljava/lang/Class;Ljava/lang/Class;Z)Z";
    let member: &'static str = match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
    {
        Some("java/lang/reflect/Method") => "java/lang/reflect/Method",
        Some("java/lang/reflect/Field") => "java/lang/reflect/Field",
        Some("java/lang/reflect/Constructor") => "java/lang/reflect/Constructor",
        _ => return,
    };
    let call = |name: &'static str, descriptor: &'static str| ServedRaiseHopSite::LastCall {
        name,
        descriptor: Some(descriptor),
    };
    ctx.note_served_raise_chain(ServedRaiseChain {
        throwable_class: "java/lang/reflect/InaccessibleObjectException",
        hops: vec![
            ServedRaiseHop {
                class: member,
                name: "setAccessible",
                descriptor: "(Z)V",
                site: call("checkCanSetAccessible", ONE_CLASS),
            },
            ServedRaiseHop {
                class: member,
                name: "checkCanSetAccessible",
                descriptor: ONE_CLASS,
                site: call("checkCanSetAccessible", TWO_CLASSES),
            },
            ServedRaiseHop {
                class: AO,
                name: "checkCanSetAccessible",
                descriptor: TWO_CLASSES,
                site: call("checkCanSetAccessible", WITH_FLAG),
            },
            ServedRaiseHop {
                class: AO,
                name: "checkCanSetAccessible",
                descriptor: WITH_FLAG,
                site: call("throwInaccessibleObjectException", TWO_CLASSES),
            },
            ServedRaiseHop {
                class: AO,
                name: "throwInaccessibleObjectException",
                descriptor: TWO_CLASSES,
                site: ServedRaiseHopSite::Raise,
            },
        ],
    });
}

/// The one JEP 403 gate every `setAccessible(true)` native goes through.
///
/// Before 2026-08-06 there were two. The typed natives below carried the module
/// check; `native_set_accessible_write_override` carried none, and because it
/// is registered LATER in `register_essential_natives_with_shims` (the registry
/// is last-writer-wins) it was the one real-JDK mode actually ran. Every
/// `setAccessible(true)` into `java.base` therefore succeeded, where HotSpot 25
/// throws `InaccessibleObjectException` - `probes/ThreadGroupLayoutProbe.java`,
/// the `ref *` lines, is the measurement. Routing all of them here is what
/// keeps a future duplicate registration from silently re-opening the hole.
///
/// `flag == false` is never checked: clearing the override cannot fail.
pub fn enforce_set_accessible_gate(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    flag: i32,
    member_label: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if flag == 0 {
        return Ok(());
    }
    if let Err(msg) = check_class_loader_define_class_is_encapsulated(ctx, this) {
        note_set_accessible_refusal_chain(ctx, this);
        return Err(
            cratonvm_types::error::RuntimeError::InaccessibleObjectException { message: msg }
                .into(),
        );
    }
    let Some(mirror) = set_accessible_declaring_mirror(ctx, this) else {
        return Ok(());
    };
    let Some(target_class_name) = mirror_class_name(ctx, mirror) else {
        return Ok(());
    };
    // Carry the mirror's own ClassId, never just its binary name. Two child
    // loaders may define the same name, and the name lookup then fails and
    // hits the fail-closed "cannot resolve target class" arm — which turns a
    // legitimate same-loader `setAccessible` into a spurious refusal
    // (`regression-suite/src/RFieldSiteCache.java`, `twoLoadersOneName`).
    let target_cid = mirror_class_id(ctx, mirror);
    // JEP 403: setAccessible(true) is where the check is paid, so
    // `accessible_override` is false even though the caller is trying to
    // *become* accessible.
    let verdict =
        check_reflection_module_access_with_target_id(ctx, &target_class_name, target_cid, false);
    let msg = match verdict {
        // The VM knows only a layer module's exports; its `opens` are the
        // JDK's (see `layer_target_not_open`).
        Ok(()) => match layer_target_not_open(ctx, target_cid)? {
            None => return Ok(()),
            Some(msg) => msg,
        },
        Err(msg) => msg,
    };
    if target_cid.is_some_and(|cid| set_accessible_export_carve_out(ctx, this, cid)) {
        return Ok(());
    }
    note_set_accessible_refusal_chain(ctx, this);
    Err(
        cratonvm_types::error::RuntimeError::InaccessibleObjectException {
            message: format!(
                "Unable to make {member_label} accessible: {msg} \
                 (use --add-opens to grant access)"
            ),
        }
        .into(),
    )
}

/// [`enforce_set_accessible_gate`]'s verdict for `setAccessible(true)` on
/// `this`, without raising or recording a refusal: what
/// `AccessibleObject.trySetAccessible` answers (`false` exactly where the
/// throwing form throws). Interpreter round i4 wave 86, lane L5 (probe
/// `L5W86ModuleLayers`, `trySetAccessible A.count`): the `trySetAccessible`
/// stand-ins asked only the `ClassLoader.defineClass` edge and answered `true`
/// for a private member of a package its module exports but does not open.
pub fn set_accessible_allowed(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<bool, cratonvm_types::error::MethodCallFailed> {
    if check_class_loader_define_class_is_encapsulated(ctx, this).is_err() {
        return Ok(false);
    }
    let Some(mirror) = set_accessible_declaring_mirror(ctx, this) else {
        return Ok(true);
    };
    let Some(target_class_name) = mirror_class_name(ctx, mirror) else {
        return Ok(true);
    };
    let target_cid = mirror_class_id(ctx, mirror);
    let refused =
        match check_reflection_module_access_with_target_id(ctx, &target_class_name, target_cid, false)
        {
            Ok(()) => layer_target_not_open(ctx, target_cid)?.is_some(),
            Err(_) => true,
        };
    if !refused {
        return Ok(true);
    }
    Ok(target_cid.is_some_and(|cid| set_accessible_export_carve_out(ctx, this, cid)))
}

/// `--real-jdk`: the `opens` half of `AccessibleObject.checkCanSetAccessible`
/// for a member of a class of a non-boot `ModuleLayer`'s module, asked after
/// the VM's own gate admitted it: `Some(message)` when the JDK's
/// `declaringModule.isOpen(pn, callerModule)` is false, `None` when it is
/// true or the question is not this one (no layer module recorded, a target
/// of no layer module, the target's own class or a JDK caller, which the
/// VM's gate already admits).
///
/// The VM's record of a layer module holds its EXPORTS only: the JDK syncs
/// an `opens` directive and `addOpens` through the same `addExports0` /
/// `addExportsToAll0` / `addExportsToAllUnnamed0` natives, as HotSpot's VM
/// keeps no opens either, so the gate admitted `setAccessible` on a private
/// member of a package the module exports but does not open. The JDK's
/// `Module` fields (`openPackages`, `ReflectionData.exports`) are the
/// answer, and `Module.implIsExportedOrOpen(pn, other, true)` -- the body
/// `isOpen` runs for such a module (wave 43) -- reads them (interpreter
/// round i1 wave 45, lane L5). One flag load for every other
/// `setAccessible`.
pub fn layer_target_not_open(
    ctx: &mut dyn NativeContext,
    target_cid: Option<ClassId>,
) -> Result<Option<String>, MethodCallFailed> {
    // Both modes since wave 48 (lane L6).
    if !ctx.has_layer_module_packages() {
        return Ok(None);
    }
    let Some(target_cid) = target_cid else {
        return Ok(None);
    };
    // Interpreter round i6 wave 123, lane L5: the one question by identity.
    if cratonvm_types::flags().loader.module_access_export_one_question {
        return layer_target_open_refusal_by_identity(ctx, target_cid);
    }
    // 3+ is a user-defined loader's namespace (the loaders a layer module is
    // recorded for).
    let Ok(loader_ns) = u32::try_from(ctx.loader_id_of_class(target_cid)) else {
        return Ok(None);
    };
    if loader_ns < 3 {
        return Ok(None);
    }
    let Some(package) = package_of_class_id(&*ctx, target_cid) else {
        return Ok(None);
    };
    let Some(target_module) = ctx.layer_module_of_package(loader_ns, &package) else {
        return Ok(None);
    };
    if !crate::jboss_jdkspecific::is_user_layer_module(ctx, target_module) {
        return Ok(None);
    }
    let Some(caller) = resolve_caller_class_id(ctx) else {
        return Ok(None);
    };
    if caller == target_cid {
        return Ok(None);
    }
    let caller_name = ctx.class_name_of_id(caller);
    if caller_is_jdk_internal(caller_name.as_deref(), ctx.loader_id_of_class(caller)) {
        return Ok(None);
    }
    let caller_ident = ctx.module_identity_of_class(caller);
    let target_ident = ctx.module_identity_of_class(target_cid);
    let mut scope = NativeHandleScope::new(ctx);
    let target_h = scope.root(target_module);
    let caller_mirror = scope.get_class_mirror(caller);
    let caller_module =
        match scope.invoke_virtual(caller_mirror, "getModule", "()Ljava/lang/Module;", &[])? {
            Some(Value::Object(Some(m))) => m,
            _ => return Ok(None),
        };
    let caller_h = scope.root(caller_module);
    let dotted = package.replace('/', ".");
    let pn = scope.create_string_uninterned(&dotted);
    let target_module = scope.get(&target_h);
    let caller_module = scope.get(&caller_h);
    let Some(open) = crate::jboss_jdkspecific::user_layer_module_is_exported_or_open(
        &mut *scope,
        target_module,
        Value::Object(Some(pn)),
        Some(caller_module),
        true,
    ) else {
        return Ok(None);
    };
    let refused = matches!(open?, Some(Value::Int(0)));
    if crate::jboss_jdkspecific::dbg_layer_modules() {
        eprintln!(
            "[LAYER-MODULE] setAccessible opens {dotted} of {} to {} -> {}",
            target_ident.describe(),
            caller_ident.describe(),
            if refused { "refused" } else { "open" }
        );
    }
    if !refused {
        return Ok(None);
    }
    Ok(Some(format!(
        "{} does not \"opens {dotted}\" to {}",
        target_ident.describe(),
        caller_ident.describe()
    )))
}

/// [`layer_target_not_open`] under `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION`
/// (default off; interpreter round i6 wave 123, lane L5,
/// `i45-L5-proposal-one-module-access-question-by-identity`): the `opens`
/// question asked by the modules' identities. The VM's one question
/// (`NativeClassAccess::module_access`, `ModuleQuestion::Open`) answers first;
/// where it has no answer -- a layer module's opens, which live only in the
/// JDK's `Module` fields -- the target class's OWN `Module` (its mirror's
/// `getModule()`, not the loader's package-to-module map) is asked
/// `implIsExportedOrOpen(pn, callerModule, true)`. The target must be a
/// layer module's class by identity (`ClassModuleIdentity::Layer`); every
/// other target was answered by the VM's gate. Same exemptions and message as
/// the package-keyed path. Probe `L5W123LayerOpensByIdentity`.
pub fn layer_target_open_refusal_by_identity(
    ctx: &mut dyn NativeContext,
    target_cid: ClassId,
) -> Result<Option<String>, MethodCallFailed> {
    let target_ident = ctx.module_identity_of_class(target_cid);
    if !matches!(
        target_ident,
        cratonvm_native_api::ClassModuleIdentity::Layer { .. }
    ) {
        return Ok(None);
    }
    let Some(caller) = resolve_caller_class_id(ctx) else {
        return Ok(None);
    };
    if caller == target_cid {
        return Ok(None);
    }
    let caller_name = ctx.class_name_of_id(caller);
    if caller_is_jdk_internal(caller_name.as_deref(), ctx.loader_id_of_class(caller)) {
        return Ok(None);
    }
    let Some(package) = package_of_class_id(&*ctx, target_cid) else {
        return Ok(None);
    };
    let dotted = package.replace('/', ".");
    let caller_ident = ctx.module_identity_of_class(caller);
    let (open, source) =
        match ctx.module_access(caller, target_cid, cratonvm_native_api::ModuleQuestion::Open) {
            Some(answer) => (answer, "registry"),
            None => {
                let mut scope = NativeHandleScope::new(ctx);
                let target_mirror = scope.get_class_mirror(target_cid);
                let target_module = match scope.invoke_virtual(
                    target_mirror,
                    "getModule",
                    "()Ljava/lang/Module;",
                    &[],
                )? {
                    Some(Value::Object(Some(m))) => m,
                    _ => return Ok(None),
                };
                if !crate::jboss_jdkspecific::is_user_layer_module(&mut *scope, target_module) {
                    return Ok(None);
                }
                let target_h = scope.root(target_module);
                let caller_mirror = scope.get_class_mirror(caller);
                let caller_module = match scope.invoke_virtual(
                    caller_mirror,
                    "getModule",
                    "()Ljava/lang/Module;",
                    &[],
                )? {
                    Some(Value::Object(Some(m))) => m,
                    _ => return Ok(None),
                };
                let caller_h = scope.root(caller_module);
                let pn = scope.create_string_uninterned(&dotted);
                let target_module = scope.get(&target_h);
                let caller_module = scope.get(&caller_h);
                let Some(answer) = crate::jboss_jdkspecific::user_layer_module_is_exported_or_open(
                    &mut *scope,
                    target_module,
                    Value::Object(Some(pn)),
                    Some(caller_module),
                    true,
                ) else {
                    return Ok(None);
                };
                (!matches!(answer?, Some(Value::Int(0))), "Module.implIsExportedOrOpen")
            }
        };
    if crate::jboss_jdkspecific::dbg_layer_modules() {
        eprintln!(
            "[LAYER-MODULE] setAccessible opens {dotted} of {} to {} by identity ({source}) -> {}",
            target_ident.describe(),
            caller_ident.describe(),
            if open { "open" } else { "refused" }
        );
    }
    if open {
        return Ok(None);
    }
    Ok(Some(format!(
        "{} does not \"opens {dotted}\" to {}",
        target_ident.describe(),
        caller_ident.describe()
    )))
}

pub const SA_ACC_PRIVATE: i32 = 0x0002;

/// Is the `setAccessible` override already granted on this reflective object?
///
/// The JDK-inherited `override` field first — that is what real-JDK bytecode
/// and our own natives write — then the CratonVM extra slot, because a
/// `Field`/`Method`/`Constructor` built with positional slots has no named
/// `override` at all.
///
/// The extra-slot fallback dispatches on the receiver's actual class instead of
/// trying all three readers in turn. Each reader derives its slot index from
/// its own layout (`method_extra_base` and friends), so asking the Method
/// reader about a Field object reads whatever Field slot happens to sit at that
/// offset — a false positive that would silently re-grant the access this gate
/// exists to refuse. All three classes are `final`, so the name is exact.
pub fn accessible_override_is_set(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    if let Value::Int(v) = ctx.get_field_by_name(obj, "override") {
        if v != 0 {
            return true;
        }
    }
    match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
    {
        Some("java/lang/reflect/Field") => read_field_accessible(ctx, obj),
        Some("java/lang/reflect/Method") => read_method_accessible(ctx, obj),
        Some("java/lang/reflect/Constructor") => read_constructor_accessible(ctx, obj),
        _ => false,
    }
}

/// Runtime package of a class, or `None` when the name cannot be read.
pub fn package_of_class_id(ctx: &dyn NativeContext, cid: ClassId) -> Option<String> {
    let name = ctx.class_name_of_id(cid)?;
    Some(match name.rfind('/') {
        Some(i) => name[..i].to_string(),
        None => String::new(),
    })
}

/// JEP 181 nestmate test: do `a` and `b` belong to the same nest?
///
/// `Reflection.areNestMates(currentClass, memberClass)` is
/// `currentClass.getNestHost() == memberClass.getNestHost()`, and a class with
/// no `NestHost` attribute is its own nest host — which is exactly the shape
/// [`NativeContext::nest_host_name`] reports (`None` == "I am my own host").
///
/// Two defining loaders are two run-time packages, so never one nest. Two
/// classes neither of which names another host (nor is a lambda proxy, whose
/// host `getNestHost` finds) are two nests. Otherwise each side's host is
/// [`native_class_get_nest_host`]'s: the claimed host LOADED through the
/// class's defining loader and validated (same run-time package, listed in
/// its `NestMembers`), as HotSpot's `InstanceKlass::nest_host` does. Until
/// JIT round 20 wave 16 (lane nest) the claimed host NAMES were compared, so
/// two classes naming a host that does not exist, or one that lists neither,
/// were nestmates (`canAccess` answered `true` for each other's private
/// members; `jr20-private-access-admitted-when-the-common-nest-host-is-unloaded`).
///
/// GC: may run the loader's `loadClass`; the only caller
/// ([`verify_member_access`], `canAccess`) holds no object reference across it.
pub fn are_nestmates(ctx: &mut dyn NativeContext, a: ClassId, b: ClassId) -> bool {
    if a == b {
        return true;
    }
    if ctx.loader_id_of_class(a) != ctx.loader_id_of_class(b) {
        return false;
    }
    fn names_a_host(ctx: &dyn NativeContext, id: ClassId) -> bool {
        is_lambda_proxy_id(id)
            || ctx
                .nest_host_name(id)
                .is_some_and(|host| ctx.class_name_of_id(id).as_deref() != Some(host.as_str()))
    }
    if !names_a_host(&*ctx, a) && !names_a_host(&*ctx, b) {
        return false;
    }
    validated_nest_host_id(ctx, a) == validated_nest_host_id(ctx, b)
}

/// `class_id`'s nest host as `Class.getNestHost` answers it
/// ([`native_class_get_nest_host`]: loaded through the class's defining
/// loader and validated), or `class_id` itself. A GC point (JIT round 20
/// wave 16, lane nest).
pub fn validated_nest_host_id(ctx: &mut dyn NativeContext, class_id: ClassId) -> ClassId {
    let mirror = ctx.get_class_mirror(class_id);
    match native_class_get_nest_host(ctx, &[Value::Object(Some(mirror))]) {
        Ok(Some(Value::Object(Some(host)))) => mirror_class_id(ctx, host).unwrap_or(class_id),
        _ => class_id,
    }
}

/// Does `class_id` claim a nest host (non-hidden, not a lambda proxy, a
/// `NestHost` naming another class) that its namespace does not know yet?
/// `lang_reflect::confirmed_nest_host_name` would then answer "own host"
/// without asking the loader, where HotSpot's `getNestHost` loads it.
/// Metadata reads only.
pub fn claimed_nest_host_unloaded(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    if is_lambda_proxy_id(class_id) || ctx.is_class_hidden(class_id) {
        return false;
    }
    let Some(own) = ctx.class_name_of_id(class_id) else {
        return false;
    };
    match ctx.nest_host_name(class_id) {
        Some(host) if host != own => ctx.class_id_by_name_near(&host, class_id).is_none(),
        _ => false,
    }
}

/// HotSpot's reflective private-access check (`Reflection.verifyMemberAccess`
/// -> `areNestMates` -> `Class.getNestHost`) LOADS each side's claimed nest
/// host through its defining loader; `lang_reflect::caller_may_access_member`
/// only looks it up. Called by a reflective door BEFORE it asks
/// `caller_may_access_member` (JIT round 20 wave 16, lane nest;
/// `jr20-reflective-private-access-check-does-not-load-the-nest-host`): for a
/// PRIVATE member of `declaring` reached from `caller`, every claimed host
/// the namespace does not know yet is loaded (as `getNestHost` does, a failed
/// load leaving the class its own host), so the check that follows judges the
/// loaded host (`NestMembers`, package, defining loader).
///
/// A GC point only when a host is loaded. The door hands over every
/// reference it keeps using: `refs` / `opt_refs` are pinned and written back
/// (`pin_native_root` convention), and `args` (the door's own argument slice)
/// is pinned and returned refreshed: `Some(args')` exactly when a load ran,
/// and the door must read its arguments from `args'` afterwards. `None`: no
/// load, nothing moved.
pub fn settle_reflective_nest_hosts(
    ctx: &mut dyn NativeContext,
    caller: ClassId,
    declaring: ClassId,
    modifiers: i32,
    refs: &mut [&mut ObjectRef],
    opt_refs: &mut [&mut Option<ObjectRef>],
    args: &[Value],
) -> Option<Vec<Value>> {
    if modifiers & SA_ACC_PRIVATE == 0 || caller == declaring {
        return None;
    }
    let load: Vec<ClassId> = [caller, declaring]
        .into_iter()
        .filter(|&id| claimed_nest_host_unloaded(&*ctx, id))
        .collect();
    if load.is_empty() {
        return None;
    }
    // Pins are a stack: unpinning the first handle releases them all.
    let mut base: Option<usize> = None;
    let mut ref_pins: Vec<usize> = Vec::with_capacity(refs.len());
    for r in refs.iter() {
        let handle = ctx.pin_native_root(**r);
        base.get_or_insert(handle);
        ref_pins.push(handle);
    }
    let mut opt_pins: Vec<Option<usize>> = Vec::with_capacity(opt_refs.len());
    for r in opt_refs.iter() {
        let handle = match **r {
            Some(obj) => {
                let handle = ctx.pin_native_root(obj);
                base.get_or_insert(handle);
                Some(handle)
            }
            None => None,
        };
        opt_pins.push(handle);
    }
    let mut arg_pins: Vec<Option<usize>> = Vec::with_capacity(args.len());
    for v in args {
        let handle = match v {
            Value::Object(Some(obj)) => {
                let handle = ctx.pin_native_root(*obj);
                base.get_or_insert(handle);
                Some(handle)
            }
            _ => None,
        };
        arg_pins.push(handle);
    }
    for id in load {
        validated_nest_host_id(ctx, id);
    }
    for (r, handle) in refs.iter_mut().zip(&ref_pins) {
        **r = ctx.read_native_pin(*handle, **r);
    }
    for (r, handle) in opt_refs.iter_mut().zip(&opt_pins) {
        if let (Some(obj), Some(handle)) = (**r, handle) {
            **r = Some(ctx.read_native_pin(*handle, obj));
        }
    }
    let refreshed: Vec<Value> = args
        .iter()
        .zip(&arg_pins)
        .map(|(v, handle)| match (v, handle) {
            (Value::Object(Some(obj)), Some(handle)) => {
                Value::Object(Some(ctx.read_native_pin(*handle, *obj)))
            }
            _ => *v,
        })
        .collect();
    if let Some(base) = base {
        ctx.unpin_native_roots(base);
    }
    Some(refreshed)
}

/// Would the current caller reach a member with these `modifiers`, declared by
/// `declaring_id`, WITHOUT a `setAccessible(true)` override?
///
/// This is `Reflection.verifyMemberAccess` composed with
/// `Reflection.verifyModuleAccess` — the pair `AccessibleObject.canAccess`
/// falls back on once `isAccessible()` is false. The module half comes first
/// because it is the half that is not expressible in Java modifiers: a public
/// method of a public class in a package its module does not export
/// (`jdk.internal.misc.Unsafe.getUnsafe`) is unreachable however public it
/// looks.
///
/// No resolvable caller frame means the VM itself is asking; answer `true`,
/// matching every other reflection gate here.
///
/// `receiver` is the JLS §6.6.2.1 target type, the same third argument
/// `check_field_access` takes and `reflective_target_class_id` computes:
/// `Modifier.isStatic(modifiers) ? null : obj.getClass()`. `canAccess` must
/// answer the same question `get`/`set` will, and without it this returned
/// `true` for a receiver the read then refused. Measured on Temurin 25.0.3 from
/// a classpath subclass of `java.io.ByteArrayOutputStream`:
/// `buf.canAccess(ownInstance)` is `true` while `buf.canAccess(bareBaos)` and
/// `buf.canAccess(siblingSubclass)` are `false`, matching the three
/// `Field.get` rows exactly.
///
/// `None` is the no-refinement value (a `static` member, or a call site with no
/// receiver in hand) and reproduces this function's pre-receiver behaviour.
pub fn verify_member_access(
    ctx: &mut dyn NativeContext,
    declaring_id: ClassId,
    modifiers: i32,
    receiver: Option<ClassId>,
) -> bool {
    let Some(caller_id) = resolve_caller_class_id(ctx) else {
        return true;
    };
    if caller_id == declaring_id {
        return true;
    }
    // Module half. `reflective_export_to_accessor` is the `exports` edge;
    // `check_deep_reflection_access` additionally accepts `opens` and the
    // same-module / unnamed-target cases, so either one passing is enough.
    if !ctx.reflective_export_to_accessor(caller_id, declaring_id)
        && ctx
            .check_deep_reflection_access(caller_id, declaring_id)
            .is_err()
    {
        return false;
    }
    // Class half (JLS 6.6.1's first clause, `Reflection.verifyMemberAccess`'s
    // `if (!Modifier.isPublic(getClassAccessFlags(memberClass)))` block). This
    // runs BEFORE any member modifier is consulted and it asks ONE question:
    // when the declaring class is not public, is the caller in its runtime
    // package? A non-public declaring class is not a denial by itself — it is
    // a denial only from another package.
    //
    // Previously the public-member arm answered `class is public` directly,
    // which refused a `public static` member of a package-private class from a
    // class in the SAME package. `jakarta.el.TestStaticFieldELResolver`'s
    // `testGetValue09`/`testGetType09` are the witness: the enum constant
    // `MethodUnderTest.GET_TYPE` is `public static final` on a *private nested*
    // enum, and `jakarta.el.Util.canAccess(null, field)` answered `false` while
    // `Field.get(null)` — which asks the same question through
    // `public_member_class_is_reachable`, and asks it correctly — answered with
    // the value. `StaticFieldELResolver.getValue` consults `canAccess`, so the
    // disagreement surfaced as `PropertyNotFoundException` with a null cause.
    // See teststaticfieldelresolver-get-type-field-not-found-CLOSED.md.
    let declaring_is_public =
        (i32::from(ctx.class_access_flags(declaring_id)) & SA_ACC_PUBLIC) != 0;

    // A public member of a public class needs no package at all, and it is the
    // overwhelmingly common answer. `package_of_class_id` allocates two Strings
    // per call, and `jakarta.el.Util.canAccess` sits on EL's method-resolution
    // path — computing the package before this test made every EL invocation
    // pay for a question it never asks.
    if declaring_is_public && (modifiers & SA_ACC_PUBLIC) != 0 {
        return true;
    }

    // Package NAME only, deliberately, and not `Reflection.isSameClassPackage`'s
    // name-plus-defining-loader pair. This is the same comparison the sibling
    // gate `public_member_class_is_reachable` (the one `Field.get` asks) already
    // makes, and the two must not disagree — that disagreement is the whole
    // defect this reordering fixes. Two classes sharing a package NAME under
    // different loaders are distinct run-time packages to HotSpot and would be
    // refused there; tightening both gates together needs its own witness, so
    // it is not done here.
    //
    // Under the per-VM `CRATONVM_REFLECT_CLASS_ACCESS` (interpreter round i2
    // wave 59, lane L3) both gates tighten together to the RUNTIME package --
    // the served `invoke` / `newInstance` / `get` doors ask
    // `member_class_is_reachable_by_runtime_package`, and `canAccess` must give
    // the answer they give, or the Spring/JUnit `makeAccessible` idiom (set the
    // override exactly when `canAccess` says no) skips the override and the
    // call is refused (`L3W59ReflectClassAccess` row `make-accessible-idiom`).
    let runtime_packages = ctx.reflect_class_access();
    let same_package = if runtime_packages {
        same_runtime_package(ctx, caller_id, declaring_id).unwrap_or(false)
    } else {
        match (
            package_of_class_id(ctx, caller_id),
            package_of_class_id(ctx, declaring_id),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    };
    if !declaring_is_public
        && !same_package
        && !(runtime_packages
            && member_class_is_reachable_by_runtime_package(
                ctx,
                declaring_id,
                Some(Some(caller_id)),
            ))
    {
        return false;
    }

    // Modifier half (JLS 6.6.1). The caller can reach the declaring class
    // itself by the time we get here, so a public member is reachable.
    if (modifiers & SA_ACC_PUBLIC) != 0 {
        return true;
    }
    if (modifiers & SA_ACC_PRIVATE) != 0 {
        // JEP 181: a private member is reachable from a NESTMATE, which is how
        // an outer class reaches its nested class's private members (and vice
        // versa) without `setAccessible`. `Reflection.verifyMemberAccess` runs
        // exactly this test (`areNestMates`) before falling through to the
        // package/protected arms, and a private member never reaches those.
        return are_nestmates(ctx, caller_id, declaring_id);
    }
    if same_package {
        return true;
    }
    if (modifiers & SA_ACC_PROTECTED) == 0 || !ctx.is_subclass(caller_id, declaring_id) {
        return false;
    }
    // JLS §6.6.2 admits a foreign-package subclass, and §6.6.2.1 then narrows
    // WHICH objects it may reach the member on. `is_subclass_or_unreadable` is
    // the tri-state walk: this conjunct only ever produces a `false`, so a
    // fabricated stand-in with no modelled supertype chain must not be caught
    // by it.
    match receiver {
        Some(r) => crate::lang_reflect::is_subclass_or_unreadable(ctx, r, caller_id),
        None => true,
    }
}

/// JIT round 21 wave 1, lane updaters: `ReflectUtil.ensureMemberAccess(caller,
/// tclass, null, modifiers)` as the JDK 25 `Atomic*FieldUpdaterImpl`
/// constructors run it, for the `--compatible` `newUpdater` natives
/// (`atomic_updater::build_updater`). `Ok(Some(caller))` when the caller (the
/// `Reflection.getCallerClass()` of `newUpdater`: [`resolve_caller_class_id`])
/// may access the member, `Err(text)` with `Reflection.newIllegalAccessException`'s
/// text when it may not ([`verify_member_access`] / `hotspot_illegal_access_message`,
/// the gate and text `Field.get` uses). `Ok(None)` -- no check, the old
/// behaviour -- when no caller resolves (the VM itself is asking) or the
/// innermost frame's class is not exact (a compiled activation that inlined
/// the call may report its outer class: a guessed caller must not refuse).
pub fn field_updater_member_access(
    ctx: &mut dyn NativeContext,
    member: ClassId,
    modifiers: i32,
) -> Result<Option<ClassId>, String> {
    if !ctx.innermost_frame_class_exact() {
        return Ok(None);
    }
    let Some(caller) = resolve_caller_class_id(ctx) else {
        return Ok(None);
    };
    if caller == member || verify_member_access(ctx, member, modifiers, None) {
        return Ok(Some(caller));
    }
    Err(hotspot_illegal_access_message(ctx, caller, member, modifiers))
}

/// JIT round 21 wave 1, lane updaters: the JDK 25 field updaters'
/// `isSamePackage(class1, class2)` (same defining loader, same package name),
/// `true` when either name is unknown (the lenient answer: `cclass` stays
/// `tclass`).
pub fn field_updater_same_package(
    ctx: &mut dyn NativeContext,
    a: ClassId,
    b: ClassId,
) -> bool {
    same_runtime_package(ctx, a, b).unwrap_or(true)
}

/// Field.setAccessible(boolean) вЂ” writes the accessible flag.
/// Throws `InaccessibleObjectException` when the caller's module is not
/// granted deep-reflection access to the declaring class's package.
///
/// Unlike Method/Constructor (whose accessible flag we stash at a fixed
/// synthetic slot), Field uses the real JDK layout plus an extra slot,
/// so the flag is persisted via [`write_field_accessible`] and also
/// mirrored onto the JDK `override` inherited field.
pub fn native_field_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let flag = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    enforce_set_accessible_gate(ctx, this, flag, "field")?;

    write_field_accessible(ctx, this, flag != 0);
    // Also mirror to the JDK `override` inherited field so Java-side code
    // that consults `AccessibleObject.override` directly agrees.
    ctx.set_field_by_name(this, "override", Value::Int(flag));
    Ok(None)
}

/// Method.setAccessible(boolean) вЂ” sets the accessible flag.
/// Throws `InaccessibleObjectException` when the caller's module is not
/// granted deep-reflection access to the declaring class's package.
///
/// C6: accessible flag lives in a CratonVM extra slot (not a real JDK
/// field); the declaring-class mirror is read via `get_field_by_name`.
pub fn native_method_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    set_method_like_accessible_impl(ctx, args, "method", |c, o, v| {
        write_method_accessible(c, o, v)
    })
}

/// Constructor.setAccessible(boolean) вЂ” sets the accessible flag.
/// Throws `InaccessibleObjectException` when the caller's module is not
/// granted deep-reflection access to the declaring class's package.
///
/// C6: Constructor accessible flag lives in a CratonVM extra slot (not a
/// real JDK field); the declaring-class mirror is read via
/// `get_field_by_name`.
pub fn native_constructor_set_accessible(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    set_method_like_accessible_impl(ctx, args, "constructor", |c, o, v| {
        write_constructor_accessible(c, o, v)
    })
}

/// Shared setAccessible helper for Method / Constructor (both go through
/// extra-slot storage for the accessible flag after the C6 layout fix).
pub fn set_method_like_accessible_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    member_label: &'static str,
    mut write_flag: impl FnMut(&mut dyn NativeContext, cratonvm_types::ObjectRef, bool),
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let flag = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    enforce_set_accessible_gate(ctx, this, flag, member_label)?;

    write_flag(ctx, this, flag != 0);
    // Mirror the JDK inherited `override` field as well (JEP 403).
    ctx.set_field_by_name(this, "override", Value::Int(flag));
    Ok(None)
}

// ---------------------------------------------------------------------------
// java.lang.reflect.Field вЂ” object layout and natives
// ---------------------------------------------------------------------------

/// Number of "extra" slots appended after the JDK Field layout to hold
/// CratonVM-specific metadata that doesn't exist on real JDK Field:
///   +0 в†’ String (raw descriptor, e.g. "J" or "Ljava/lang/String;")
///   +1 в†’ Int (CratonVM slot_index вЂ” absolute heap index for instance
///            fields, or field_index for static fields)
///   +2 в†’ Int (accessible flag, 0 or 1)
pub const FIELD_EXTRA_SLOTS: usize = 3;
pub const FIELD_EXTRA_OFFSET_DESC: usize = 0;
pub const FIELD_EXTRA_OFFSET_RJ_SLOT: usize = 1;
pub const FIELD_EXTRA_OFFSET_ACCESSIBLE: usize = 2;

/// Legacy synthetic Field width вЂ” kept as a floor so older code paths
/// that still read slots 0..=6 directly don't stumble over a too-small
/// heap object. The real JDK layout is always в‰Ґ 14, so in practice this
/// floor is only meaningful for synthetic stub classes where
/// `class_num_total_fields` returns 0.
pub const FIELD_NUM_FIELDS_LEGACY_FLOOR: usize = 7;

/// Helper: absolute slot for the first extra-metadata slot on a Field obj.
pub fn field_extra_base(ctx: &dyn NativeContext, class_id: ClassId) -> usize {
    core::cmp::max(
        FIELD_NUM_FIELDS_LEGACY_FLOOR,
        ctx.class_num_total_fields(class_id),
    )
}

/// Create a Field reflection object from metadata.
///
/// C5 fix: Previously the object was allocated with only 7 slots and the
/// synthetic layout was written at slots 0..=6 by index. But
/// `java.lang.reflect.Field` extends `AccessibleObject`, so its real JDK
/// instance-field layout puts `clazz` at absolute slot 2, `slot` at 3,
/// `name` at 4, `type` at 5, `modifiers` at 6, etc. When Java-layer code
/// (e.g. `MethodHandles.Lookup.unreflectField`) did `Getfield clazz` /
/// `Getfield modifiers`, the interpreter resolved those real JDK offsets
/// and read our synthetic values вЂ” so `modifiers` came back as our
/// `slot_index` and `isStatic()` silently returned false, producing the
/// `expected a static field: ... from class java.lang.Object (null)`
/// IllegalAccessException for every `unreflectGetter(serialVersionUID)`.
///
/// Fix: populate the real JDK-named fields via `set_field_by_name` and
/// store CratonVM-specific metadata (descriptor string, slot_index,
/// accessible flag) in extra slots appended *after* the JDK layout. All
/// native readers go via `set_field_by_name` / `get_field_by_name` for
/// JDK fields and via the extra-slot helpers for our metadata.
/// GC-safe construction of a reference array whose `len` elements are produced
/// by an allocating factory (`create_field_object` / `create_method_object` /
/// `descriptor_to_class_mirror` / вЂ¦).
///
/// The freshly-allocated array is held only in a Rust local, so it is invisible
/// to the GC root scan. Every per-element factory call allocates and can trigger
/// a moving young GC that relocates the array, leaving the raw `ObjectRef` stale
/// вЂ” it then resolves to a reused, usually `java/lang/Object`, slot. That is the
/// WildFly bug-06 family: a reflective `Field[]`/`Method[]`/`Class[]` corrupted
/// mid-build, surfacing later as `NoSuchMethodError Object.getName()`,
/// `PreconditionViolationException: annotationType/Member must not be null`, or
/// `ClassCastException: java/lang/Object cannot be cast to [LвЂ¦;`.
///
/// Fix: pin the array as a GC root across the fill loop and re-read the
/// forwarded reference before each store. Each element, once stored into the
/// pinned array, stays reachable and is remapped by subsequent collections.
/// There is no allocation between `make` returning an element and the store, so
/// the just-built element cannot be collected before it is rooted by the array.
pub fn build_mirror_array<F>(
    ctx: &mut dyn NativeContext,
    len: usize,
    make: F,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed>
where
    F: FnMut(&mut dyn NativeContext, usize) -> Result<cratonvm_types::ObjectRef, MethodCallFailed>,
{
    build_mirror_array_comp(ctx, cratonvm_types::ClassId::new(0), len, make)
}

/// As [`build_mirror_array`], but with an explicit array component class id
/// (e.g. the `Annotation`/`Class` component for `getDeclaredAnnotations`).
pub fn build_mirror_array_comp<F>(
    ctx: &mut dyn NativeContext,
    comp: cratonvm_types::ClassId,
    len: usize,
    mut make: F,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed>
where
    F: FnMut(&mut dyn NativeContext, usize) -> Result<cratonvm_types::ObjectRef, MethodCallFailed>,
{
    let mut arr = ctx.new_ref_array(comp, len);
    let pin = ctx.pin_native_root(arr);
    for i in 0..len {
        let elem = make(ctx, i)?;
        let elem_pin = ctx.pin_native_root(elem);
        // `make` may have moved `arr` (and the already-stored elements, which
        // are remapped through it); re-read the forwarded array reference.
        arr = ctx.read_native_pin(pin, arr);
        let elem_cur = ctx.read_native_pin(elem_pin, elem);
        ctx.set_array_element(arr, i, Value::Object(Some(elem_cur)));
        ctx.unpin_native_roots(elem_pin);
    }
    arr = ctx.read_native_pin(pin, arr);
    ctx.unpin_native_roots(pin);
    Ok(arr)
}

/// The `java/lang/Class` ClassId, for typing reflective `Class[]` results
/// (`Method`/`Constructor` `getParameterTypes`/`getExceptionTypes`,
/// `Class.getInterfaces`). HotSpot returns `[Ljava/lang/Class;` for all of
/// these; allocating the backing array with an `Object` component made
/// `Class[].isAssignableFrom(result.getClass())` false. That was masked while
/// array assignability was (incorrectly) symmetric, but with the covariance
/// fix it surfaced as a `ClassCastException` assigning the `Object[]` into a
/// `Class[]` field during deserialization (ResolvableTypeTests.serialize) and
/// would break any `(Class[])` cast. `java/lang/Class` is always loaded by the
/// time reflection runs, so the fallback is defensive only.
/// Resolve the component class for a typed reflection array. Returning
/// `Object[]` from a `Class` reflection API violates the Java contract even
/// when every element happens to be a valid reflection mirror.
pub fn reflection_component_id(
    ctx: &mut dyn NativeContext,
    component_name: &str,
) -> cratonvm_types::ClassId {
    ctx.class_id_by_name(component_name)
        .unwrap_or_else(|| cratonvm_types::ClassId::new(0))
}

/// [`reflection_component_id`] for a component that may not be loaded yet
/// (i7-L2): on a lookup miss it LOADS the class through the caller's loader
/// (`load_class`) without initialising it — `anewarray` does not initialise
/// its component either (JVMS 6.5) — and falls back to `Object` only when
/// that fails, the pre-i7 answer. Loading can run Java code and collect, so
/// call it only where no raw `ObjectRef` is live (pin first, re-read after).
pub fn loading_component_id(
    ctx: &mut dyn NativeContext,
    component_name: &str,
) -> cratonvm_types::ClassId {
    if let Some(id) = ctx.class_id_by_name(component_name) {
        return id;
    }
    if let Ok(Some(Value::Object(Some(mirror)))) = ctx.load_class(component_name) {
        if let Some(id) = ctx.class_id_from_mirror(mirror) {
            return id;
        }
    }
    ctx.class_id_by_name(component_name)
        .unwrap_or_else(|| cratonvm_types::ClassId::new(0))
}

/// The component of a reflection LISTING's array (`Field[]`, `Method[]`,
/// `Constructor[]`), zs s3/nd3 (`zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md`
/// §6): HotSpot preloads `Field` / `Method` / `Constructor` (vmClasses), so its
/// `JVM_GetClassDeclaredFields` always answers `[Ljava/lang/reflect/Field;`.
/// The by-name lookup ([`reflection_component_id`]) answered `None` on a VM
/// that had not loaded the element class yet, and the array was made over
/// `ClassId(0)`: the FIRST listing of such a VM was not a `Field[]` (only the
/// `create_*_object` calls inside the fill loaded the class). This one loads
/// the class first ([`loading_component_id`]). Call it only where no raw
/// `ObjectRef` is live (loading may collect). `CRATONVM_REFLECTION_LISTING_LOADS_COMPONENT=0`
/// restores the by-name lookup. Every collector, both modes.
///
/// zt t2/rv (`zgc-zs4rv3-wave-s3-review-residuals-CLOSED-20261019.md` item 10): the
/// by-name lookup runs first and the flag is read only on its miss. Both arms
/// begin with that same lookup, so a hit answers the same on either arm, and
/// the common call (the element class is loaded) no longer reads a flag.
pub fn listing_component_id(
    ctx: &mut dyn NativeContext,
    component_name: &str,
) -> cratonvm_types::ClassId {
    if let Some(id) = ctx.class_id_by_name(component_name) {
        return id;
    }
    if true
    {
        loading_component_id(ctx, component_name)
    } else {
        reflection_component_id(ctx, component_name)
    }
}

/// A copy of the reference array `src` whose component is `component`
/// (i7-L2), for a native whose element source only hands back `Object[]`
/// (`ArrayList.toArray()`) but whose declared return type is `T[]`. The
/// elements are copied unchecked: the caller guarantees each is a `T`. `src`
/// is pinned across the allocation.
pub fn retyped_ref_array(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    component: cratonvm_types::ClassId,
) -> ObjectRef {
    let len = ctx.array_length(src);
    let src_pin = ctx.pin_native_root(src);
    let dst = ctx.new_ref_array(component, len);
    let src = ctx.read_native_pin(src_pin, src);
    ctx.unpin_native_roots(src_pin);
    for i in 0..len {
        let element = ctx.get_array_element(src, i);
        ctx.set_array_element(dst, i, element);
    }
    dst
}


/// Allocate an empty reflection array without borrowing the runtime context
/// twice in one expression.
pub fn empty_reflection_array(
    ctx: &mut dyn NativeContext,
    component_name: &str,
) -> cratonvm_types::ObjectRef {
    let component = reflection_component_id(ctx, component_name);
    ctx.new_ref_array(component, 0)
}

/// [`empty_reflection_array`] for a declared listing's element class (zs
/// s3/nd3, [`listing_component_id`]): loads the class first. No raw
/// `ObjectRef` may be live at the call.
pub fn empty_listing_array(
    ctx: &mut dyn NativeContext,
    component_name: &str,
) -> cratonvm_types::ObjectRef {
    let component = listing_component_id(ctx, component_name);
    ctx.new_ref_array(component, 0)
}

/// The `java/lang/Class` ClassId, for typing reflective `Class[]` results
/// (`Method`/`Constructor` `getParameterTypes`/`getExceptionTypes`,
/// `Class.getInterfaces`).
pub fn class_component_id(ctx: &mut dyn NativeContext) -> cratonvm_types::ClassId {
    reflection_component_id(ctx, "java/lang/Class")
}

/// A `java.lang.reflect.Type[]` of `len` (i6-L2): what the generic-signature
/// branches of the reflection natives return, as HotSpot's reifier allocates
/// `new Type[n]`. The raw (no Signature) branches return `Class[]` instead.
pub fn new_reflect_type_array(ctx: &mut dyn NativeContext, len: usize) -> ObjectRef {
    let component = reflection_component_id(ctx, "java/lang/reflect/Type");
    ctx.new_ref_array(component, len)
}

/// An empty `Class[]` (i6-L2), for the raw-type early returns of the generic
/// reflection natives, which HotSpot answers with `getInterfaces()` /
/// `getParameterTypes()` -- both `Class[]`.
pub fn empty_class_array(ctx: &mut dyn NativeContext) -> ObjectRef {
    let component = class_component_id(ctx);
    ctx.new_ref_array(component, 0)
}

/// SBR wave 5 (reflmeta): one of a member's annotation attributes as the
/// `byte[]` HotSpot stores in the reflection object (`annotations`,
/// `parameterAnnotations`, `annotationDefault`), so the real
/// `AnnotationParser` -- `Executable.declaredAnnotations()`,
/// `Field.declaredAnnotations()`, `Method.getDefaultValue()`,
/// `Executable.sharedGetParameterAnnotations()` -- parses what the class file
/// says against the `ConstantPool` `Class.getConstantPool` returns.
///
/// `None` when the member has no such attribute: HotSpot's `null`, which the
/// parsers read as "none" (an empty array would underflow their first
/// `getShort`, the Mockito failure `create_method_object` records). Under
/// `--compatible` also `None`, so that mode's mints stay byte-for-byte what
/// they were: its annotation getters are the natives, which never read these
/// fields.
///
/// Each array is PINNED as soon as it is allocated (the next allocation can
/// move it) and handed back with its pin; the caller re-reads it through the
/// pin before storing it and releases it with its own pins.
pub fn member_annotation_bytes(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    name: &str,
    desc: &str,
    is_method: bool,
    attrs: &[cratonvm_native_api::RawAnnotationAttr],
) -> Vec<Option<(ObjectRef, usize)>> {
    // Interpreter round i2 wave 58, lane L3: `--compatible` too under
    // `CRATONVM_JDK_DECLARATION_ANNOTATIONS`, whose annotation getters are the
    // JDK's bodies, which read these fields.
    if !(ctx.is_jdk_only() || ctx.jdk_declaration_annotations()) {
        return vec![None; attrs.len()];
    }
    member_annotation_bytes_ungated(ctx, class_id, name, desc, is_method, attrs)
}

/// [`member_annotation_bytes`] in either mode: for a caller whose own gate
/// already decided (`native_member_type_annotation_bytes`, which answers
/// `--compatible` too under `CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`; until
/// interpreter round i2 wave 57, lane L3, this function's `--real-jdk` gate
/// still answered `null` there, so every member's type annotations read
/// empty).
pub fn member_annotation_bytes_ungated(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    name: &str,
    desc: &str,
    is_method: bool,
    attrs: &[cratonvm_native_api::RawAnnotationAttr],
) -> Vec<Option<(ObjectRef, usize)>> {
    ctx.member_raw_annotation_attributes(class_id, name, desc, is_method, attrs)
        .into_iter()
        .map(|bytes| {
            let bytes = bytes?;
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
            if !ctx.write_byte_array_from(arr, 0, &bytes) {
                return None;
            }
            Some((arr, ctx.pin_native_root(arr)))
        })
        .collect()
}

/// The declaring class, name and JVM descriptor of a `Field`, `Method` or
/// `Constructor`, read from the JDK's own fields only (`clazz`, `name`,
/// `type` / `parameterTypes` / `returnType`) -- so it answers for a real
/// `copy()` exactly as for a mint.
pub fn reflect_member_key(
    ctx: &dyn NativeContext,
    member: ObjectRef,
) -> Option<(ClassId, String, String, bool)> {
    let holder = match ctx.get_field_by_name(member, "clazz") {
        Value::Object(Some(m)) => mirror_class_id(ctx, m)?,
        _ => return None,
    };
    let member_class = ctx.class_name_of_id(ctx.class_id_of_object(member))?;
    match member_class.as_str() {
        "java/lang/reflect/Field" => {
            let name = match ctx.get_field_by_name(member, "name") {
                Value::Object(Some(s)) => ctx.read_string(s)?,
                _ => return None,
            };
            let desc = match ctx.get_field_by_name(member, "type") {
                Value::Object(Some(t)) => mirror_to_jvm_descriptor_token(ctx, t),
                _ => return None,
            };
            Some((holder, name, desc, false))        }
        "java/lang/reflect/Method" => {
            let name = match ctx.get_field_by_name(member, "name") {
                Value::Object(Some(s)) => ctx.read_string(s)?,
                _ => return None,
            };
            Some((
                holder,
                name,
                compose_method_descriptor_from_type_fields(ctx, member),
                true,
            ))
        }
        "java/lang/reflect/Constructor" => Some((
            holder,
            "<init>".to_string(),
            compose_init_descriptor_from_parameter_types(ctx, member),
            true,
        )),
        _ => None,
    }
}

/// `Field.getTypeAnnotationBytes0()[B` / `Executable.getTypeAnnotationBytes0()[B`
/// (ACC_NATIVE): the member's `RuntimeVisibleTypeAnnotations` bytes, which
/// the real `TypeAnnotationParser` reads for `getAnnotatedType`,
/// `getAnnotatedParameterTypes` and a method type variable's annotations.
///
/// SBR wave 5 (reflmeta): `--real-jdk` answers HotSpot's bytes now that
/// `ConstantPool` can decode them; `--compatible` keeps its `null` (its
/// annotated-type getters are the natives; byte-for-byte unchanged).
pub fn native_member_type_annotation_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `--compatible` too under `CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`: a
    // method's type parameter is a JDK `TypeVariableImpl` there, whose
    // annotations are these bytes (wave 56, lane L3; see
    // `native_class_get_raw_type_annotations`).
    if !(ctx.is_jdk_only() || ctx.compatible_reflect_type_impls()) {
        return Ok(Some(Value::Object(None)));
    }
    let member = match args.first() {
        Some(Value::Object(Some(m))) => *m,
        _ => return Ok(Some(Value::Object(None))),
    };
    let Some((holder, name, desc, is_method)) = reflect_member_key(ctx, member) else {
        return Ok(Some(Value::Object(None)));
    };
    let pin_base = ctx.pin_native_root(member);
    // The inner `--real-jdk` gate answered `null` under `--compatible` anyway;
    // `CRATONVM_JDK_TYPE_ANNOTATIONS` (wave 57, lane L3) hands it the bytes.
    let attrs = [cratonvm_native_api::RawAnnotationAttr::TypeAnnotations];
    let arr = if ctx.jdk_type_annotations() {
        member_annotation_bytes_ungated(ctx, holder, &name, &desc, is_method, &attrs)
    } else {
        member_annotation_bytes(ctx, holder, &name, &desc, is_method, &attrs)
    }
    .pop()
    .flatten()
    .map(|(a, pin)| ctx.read_native_pin(pin, a));
    ctx.unpin_native_roots(pin_base);
    Ok(Some(Value::Object(arr)))
}

/// HotSpot's `JVM_RECOGNIZED_FIELD_MODIFIERS`: the only `access_flags` bits a
/// field keeps (`ClassFileParser::parse_fields` masks the rest away).
pub const RECOGNIZED_FIELD_MODIFIERS: i32 = 0x50DF;

/// HotSpot's `JVM_RECOGNIZED_METHOD_MODIFIERS` (`parse_method`'s mask), which
/// `Reflection::new_method` / `new_constructor` hand to `getModifiers()`.
pub const RECOGNIZED_METHOD_MODIFIERS: i32 = 0x1DFF;

pub fn create_field_object(
    ctx: &mut dyn NativeContext,
    meta: &FieldMetadata,
) -> cratonvm_types::ObjectRef {
    let class_id = ctx
        .ensure_class_initialized("java/lang/reflect/Field")
        .unwrap_or(ClassId::new(0));

    // Allocate JDK-layout width + our extra metadata slots.
    let jdk_layout_fields = ctx.class_num_total_fields(class_id);
    let base = core::cmp::max(FIELD_NUM_FIELDS_LEGACY_FLOOR, jdk_layout_fields);
    let num_fields = base + FIELD_EXTRA_SLOTS;
    let obj = ctx.alloc_object(class_id, num_fields);
    // GC-safety: `get_class_mirror`/`create_string`/`descriptor_to_class_mirror_via_loader`
    // below can all trigger classloading (and therefore GC). A moving collection
    // between `alloc_object` and the `set_field_by_name` block leaves these raw
    // `ObjectRef`s stale — per `pin_native_root`'s contract, a stale ref then
    // resolves to whatever now occupies the reused slot (typically a bare
    // `java.lang.Object`), so the Field would silently get a wrong/missing
    // `clazz`/`name`/`type`. Pin everything now and re-read the forwarded
    // reference right before use (mirrors `create_method_object`).
    let obj_pin = ctx.pin_native_root(obj);

    let class_mirror = ctx.get_class_mirror(meta.declaring_class_id);
    let class_mirror_pin = ctx.pin_native_root(class_mirror);
    let name_str = ctx.intern_string(&meta.name);
    let name_str_pin = ctx.pin_native_root(name_str);
    // Loader-faithful field type (gated): a field on a bytecode-enhanced /
    // child-loader class reports that loader's copy of the field type.
    let type_mirror =
        descriptor_to_class_mirror_via_loader(ctx, &meta.descriptor, meta.declaring_class_id);
    let type_mirror_pin = ctx.pin_native_root(type_mirror);
    let desc_str = ctx.create_string(&meta.descriptor);
    let desc_str_pin = ctx.pin_native_root(desc_str);
    // SBR wave 3 (reflect): the JVMS §4.7.9 `Signature` of a generic field,
    // exactly as `create_method_object` has carried its method's since WP2.1.
    // The real `Field.getGenericType()` / `toGenericString()` read this field
    // (`getGenericSignature()`); left null, every generic field reads back as
    // its erasure once the native getter is retired. `None` for a non-generic
    // field, which is HotSpot's `null`.
    let sig_str = ctx
        .field_signature(meta.declaring_class_id, &meta.name)
        .map(|s| ctx.create_string_uninterned(&s));
    let sig_str_pin = sig_str.map(|s| ctx.pin_native_root(s));
    // SBR wave 3 (reflect): HotSpot's `fieldDescriptor::is_trusted_final()` --
    // `final && (static || holder is hidden || holder is a record)`. The real
    // `ReflectionFactory.newFieldAccessor` builds a READ-ONLY accessor from it
    // even under `setAccessible(true)`, which is what makes
    // `setInt(null, ..)` on an overridden `static final` field throw
    // `IllegalAccessException`. Hard-coded `false` let that write through.
    let trusted_final = (meta.access_flags & 0x0010) != 0
        && (meta.is_static
            || ctx.is_class_hidden(meta.declaring_class_id)
            || ctx.is_record_class(meta.declaring_class_id)
            || lambda_proxy_class_name(ctx, meta.declaring_class_id).is_some());
    // SBR wave 5 (reflmeta): the field's `RuntimeVisibleAnnotations` bytes,
    // HotSpot's `annotations` (see `member_annotation_bytes`).
    let ann = member_annotation_bytes(
        ctx,
        meta.declaring_class_id,
        &meta.name,
        &meta.descriptor,
        false,
        &[cratonvm_native_api::RawAnnotationAttr::Annotations],
    )
    .pop()
    .flatten();

    let obj = ctx.read_native_pin(obj_pin, obj);
    let class_mirror = ctx.read_native_pin(class_mirror_pin, class_mirror);
    let name_str = ctx.read_native_pin(name_str_pin, name_str);
    let type_mirror = ctx.read_native_pin(type_mirror_pin, type_mirror);
    let desc_str = ctx.read_native_pin(desc_str_pin, desc_str);
    let sig_str = match (sig_str, sig_str_pin) {
        (Some(s), Some(pin)) => Some(ctx.read_native_pin(pin, s)),
        _ => None,
    };

    // --- Real JDK Field layout (visible to Java bytecode via Getfield) ---
    ctx.set_field_by_name(obj, "clazz", Value::Object(Some(class_mirror)));
    ctx.set_field_by_name(obj, "name", Value::Object(Some(name_str)));
    ctx.set_field_by_name(obj, "type", Value::Object(Some(type_mirror)));
    // `JVM_RECOGNIZED_FIELD_MODIFIERS`: HotSpot's parser drops every other
    // bit of a field's `access_flags` (JVMS 4.5: reserved bits are ignored),
    // so `Field.getModifiers()` never shows one (i4 wave 84, lane L3;
    // `L3W84ReservedModifierBits`).
    ctx.set_field_by_name(
        obj,
        "modifiers",
        Value::Int(meta.access_flags as i32 & RECOGNIZED_FIELD_MODIFIERS),
    );
    ctx.set_field_by_name(obj, "slot", Value::Int(meta.slot_index as i32));
    ctx.set_field_by_name(obj, "trustedFinal", Value::Int(i32::from(trusted_final)));
    if let Some(sig) = sig_str {
        ctx.set_field_by_name(obj, "signature", Value::Object(Some(sig)));
    }
    if let Some((a, pin)) = ann {
        let a = ctx.read_native_pin(pin, a);
        ctx.set_field_by_name(obj, "annotations", Value::Object(Some(a)));
    }

    // --- CratonVM extra metadata (append after JDK layout) ---
    ctx.set_field(
        obj,
        base + FIELD_EXTRA_OFFSET_DESC,
        Value::Object(Some(desc_str)),
    );
    ctx.set_field(
        obj,
        base + FIELD_EXTRA_OFFSET_RJ_SLOT,
        Value::Int(meta.slot_index as i32),
    );
    ctx.set_field(obj, base + FIELD_EXTRA_OFFSET_ACCESSIBLE, Value::Int(0));

    ctx.unpin_native_roots(obj_pin);
    obj
}

/// Helper: read the static-flag, declaring ClassId, slot_index, and descriptor
/// from a Field reflection object's fields.
///
/// Uses `get_field_by_name` for the JDK-layout fields so the read lands on
/// the right slot regardless of class hierarchy offsets, and falls back to
/// the CratonVM extra-slot for the descriptor and slot_index (which don't
/// exist on real JDK Field).
pub fn read_field_meta(
    ctx: &dyn NativeContext,
    field_obj: cratonvm_types::ObjectRef,
) -> (bool, cratonvm_types::ClassId, usize, String) {
    let modifiers = match ctx.get_field_by_name(field_obj, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let is_static = (modifiers & ACC_STATIC) != 0;

    let class_id = {
        let mirror = match ctx.get_field_by_name(field_obj, "clazz") {
            Value::Object(Some(m)) => m,
            _ => {
                return (false, cratonvm_types::ClassId::new(0), 0, String::new());
            }
        };
        mirror_class_id(ctx, mirror).unwrap_or(cratonvm_types::ClassId::new(0))
    };

    // `slot_index` is stashed in our CratonVM extra slots (not a real JDK
    // field вЂ” the JDK `slot` field has different semantics).
    let field_class_id = {
        // Determine the object's class to locate the extra-slot base.
        // We don't have a direct `class_id_of` on `NativeContext`; fall
        // back to reading our custom `slot` field by name first, which
        // we intentionally populate both in JDK layout AND in the extra
        // slot. If neither is available we return 0.
        cratonvm_types::ClassId::new(0)
    };
    let _ = field_class_id; // suppress unused

    // Prefer the extra-slot int (reliable for static fields, where JDK's
    // `slot` is an opaque vmindex we don't control). Fall back to JDK's
    // `slot` field if the extra slot isn't populated.
    let slot = match read_field_rj_slot(ctx, field_obj) {
        Some(v) => v,
        None => match ctx.get_field_by_name(field_obj, "slot") {
            Value::Int(v) => v as usize,
            _ => 0,
        },
    };

    let descriptor = match read_field_descriptor(ctx, field_obj) {
        Some(s) => s,
        None => String::new(),
    };

    (is_static, class_id, slot, descriptor)
}

/// Read the CratonVM-specific `slot_index` extra slot from a Field object.
/// Returns `None` if the extra slot is unreadable or zero-uninitialised.
pub fn read_field_rj_slot(
    ctx: &dyn NativeContext,
    field_obj: cratonvm_types::ObjectRef,
) -> Option<usize> {
    let class_id = ctx.class_id_of_object(field_obj);
    // SBR wave 5 (reflmeta): a `Field` with no CratonVM tail -- a real
    // `Field.copy()` built through `<init>`, which carries exactly the JDK
    // layout -- has nothing to read here, and the defensive scan below would
    // read one of ITS OWN fields (candidate 7 + 1 lands inside the JDK layout)
    // as a slot index. The caller falls back to the JDK `slot` field, which
    // the mint fills with the same index and `copy()` copies.
    if !reflect_object_has_tail(ctx, field_obj, class_id) {
        return None;
    }
    let base = field_extra_base(ctx, class_id);
    let val_at_base = ctx.get_field(field_obj, base + FIELD_EXTRA_OFFSET_RJ_SLOT);
    match val_at_base {
        Value::Int(v) => Some(v as usize),
        // T19.H1 вЂ” defensive scan. Historically the base used at creation
        // time (Field.class not yet linked в†’ class_num_total_fields returns
        // 0 в†’ base=7) could differ from the base used at read time
        // (Field.class now linked в†’ base=14). Rather than always storing at
        // the larger fixed base, we scan the likely extra-slot range for a
        // plausible slot_index value. Finding a non-negative Int in the
        // extra-slot range beats livelocking every CAS loop that depends on
        // Unsafe.objectFieldOffset(Field).
        _ => {
            // The Field heap object is allocated as
            //   num_fields = max(7, jdk_layout_fields) + FIELD_EXTRA_SLOTS
            // so any of the candidate bases below point at valid storage.
            // Walk the two known bases (7 for the legacy floor and the
            // current JDK layout width) and return the first Int we find.
            for candidate_base in [FIELD_NUM_FIELDS_LEGACY_FLOOR, 14, 15, 16] {
                if let Value::Int(v) =
                    ctx.get_field(field_obj, candidate_base + FIELD_EXTRA_OFFSET_RJ_SLOT)
                {
                    if v >= 0 {
                        return Some(v as usize);
                    }
                }
            }
            None
        }
    }
}

/// Read the CratonVM-specific `descriptor` extra slot from a Field object.
pub fn read_field_descriptor(
    ctx: &dyn NativeContext,
    field_obj: cratonvm_types::ObjectRef,
) -> Option<String> {
    let class_id = ctx.class_id_of_object(field_obj);
    // SBR wave 5 (reflmeta): no tail (a real `copy()`): the JDK `type` field.
    if !reflect_object_has_tail(ctx, field_obj, class_id) {
        return match ctx.get_field_by_name(field_obj, "type") {
            Value::Object(Some(t)) => Some(mirror_to_jvm_descriptor_token(ctx, t)),
            _ => None,
        };
    }
    let base = field_extra_base(ctx, class_id);
    match ctx.get_field(field_obj, base + FIELD_EXTRA_OFFSET_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// SBR wave 5 (reflmeta): does this reflection object carry the CratonVM
/// metadata tail past the JDK layout? A mint does (`create_*_object`
/// allocates `max(floor, layout) + EXTRA_SLOTS`); a real `copy()` -- `new
/// Field/Method/Constructor(..)` through `<init>` once the copy door is
/// retired -- carries exactly the JDK layout and does not. Every tail reader
/// asks this first, so a copy is read through its real fields instead of
/// through whatever JDK field sits at a tail index.
pub fn reflect_object_has_tail(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    class_id: ClassId,
) -> bool {
    ctx.object_num_fields(obj) > ctx.class_num_total_fields(class_id)
}

/// SBR wave 5 (reflmeta): the JDK `root` of a reflection object -- the mint a
/// real `copy()` was taken from (`res.root = this`). Descriptor and parameter
/// count are the root's (a copy has the same member); the accessible flag is
/// per object and never follows `root`.
pub fn reflect_root(ctx: &dyn NativeContext, obj: cratonvm_types::ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(obj, "root") {
        Value::Object(Some(r)) if r != obj => Some(r),
        _ => None,
    }
}

/// Read the CratonVM-specific `accessible` extra slot from a Field object.
pub fn read_field_accessible(ctx: &dyn NativeContext, field_obj: cratonvm_types::ObjectRef) -> bool {
    if let Value::Int(v) = ctx.get_field_by_name(field_obj, "override") {
        if v != 0 {
            return true;
        }
    }
    let class_id = ctx.class_id_of_object(field_obj);
    // SBR wave 5 (reflmeta): a real `copy()` has no tail; `override` is all.
    if !reflect_object_has_tail(ctx, field_obj, class_id) {
        return false;
    }
    let base = field_extra_base(ctx, class_id);
    match ctx.get_field(field_obj, base + FIELD_EXTRA_OFFSET_ACCESSIBLE) {
        Value::Int(v) => v != 0,
        _ => false,
    }
}

/// Write the CratonVM-specific `accessible` extra slot on a Field object.
pub fn write_field_accessible(
    ctx: &mut dyn NativeContext,
    field_obj: cratonvm_types::ObjectRef,
    value: bool,
) {
    let class_id = ctx.class_id_of_object(field_obj);
    // SBR wave 5 (reflmeta): a real `copy()` has no tail to write; its flag
    // is the JDK `override` field, which is what the readers consult first.
    if !reflect_object_has_tail(ctx, field_obj, class_id) {
        ctx.set_field_by_name(field_obj, "override", Value::Int(i32::from(value)));
        return;
    }
    let base = field_extra_base(ctx, class_id);
    ctx.set_field(
        field_obj,
        base + FIELD_EXTRA_OFFSET_ACCESSIBLE,
        Value::Int(if value { 1 } else { 0 }),
    );
}

/// Public wrapper used by `lang_reflect::native_field_try_set_accessible`.
/// Same as `write_field_accessible` but kept as a stable cross-module name.
pub fn write_field_accessible_external(
    ctx: &mut dyn NativeContext,
    field_obj: cratonvm_types::ObjectRef,
    value: bool,
) {
    write_field_accessible(ctx, field_obj, value);
}

// --- Field getters (simple field reads) ---

// `Field.getName` is registered `()Ljava/lang/String;`, `Field.getType`
// `()Ljava/lang/Class;` and `Field.getDeclaringClass` `()Ljava/lang/Class;`,
// so none of them may return a primitive `Value`. A plain `get_field_by_name`
// can: it is not descriptor-aware, so an unwritten reference slot on a Field
// mirror answers `Value::Int(0)` rather than `Object(None)`, and that tag then
// reaches bytecode about to `areturn`/`checkcast` a reference. `ref_field`
// reads by resolved index (descriptor-decoded) and degrades any non-reference
// tag to null. `getModifiers` below is `()I` and stays on the by-name read —
// there `Int(0)` is the correct answer for an unwritten slot.
// See `docs/feature-designs/by-name-field-reads.md`.
pub fn native_field_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(crate::field_read::ref_field(ctx, this, "name")))}

pub fn native_field_get_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(crate::field_read::ref_field(ctx, this, "type")))
}

pub fn native_field_get_modifiers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field_by_name(this, "modifiers")))}

pub fn native_field_get_declaring_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(crate::field_read::ref_field(ctx, this, "clazz")))}

pub fn ensure_static_field_declaring_class_initialized(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Reflective access to a static field is an active use of the declaring
    // class. HotSpot runs <clinit> before Field.get/set returns the value.
    // Use the exact Field mirror ClassId so loader-private classes are
    // initialized as themselves, not as a same-named global class.
    ctx.ensure_class_initialized_with_class_id(class_id)
}

pub fn reject_array_field_receiver(
    ctx: &dyn NativeContext,
    receiver: cratonvm_types::ObjectRef,
    operation: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if ctx.heap_kind_of(receiver) == cratonvm_types::ObjectKind::Array {
        return Err(illegal_arg_exc(format!(
            "{operation}: object is not an instance of declaring class"
        )));
    }
    Ok(())
}

pub fn dbg_field_get_enabled() -> bool {
    crate::nbflags().dbg_field_get
}

pub fn dbg_field_get(
    ctx: &mut dyn NativeContext,
    operation: &str,
    field_obj: cratonvm_types::ObjectRef,
    is_static: bool,
    class_id: cratonvm_types::ClassId,
    slot: usize,
    descriptor: &str,
    receiver: Option<cratonvm_types::ObjectRef>,
) {
    if !dbg_field_get_enabled() {
        return;
    }
    let class_name = ctx
        .class_name_of_id(class_id)
        .unwrap_or_else(|| format!("<class-id:{}>", class_id.as_u32()));
    let field_name = match ctx.get_field_by_name(field_obj, "name") {
        Value::Object(Some(name_obj)) => ctx
            .read_string(name_obj)
            .unwrap_or_else(|| "<unreadable>".to_string()),
        Value::Object(None) => "<null>".to_string(),
        other => format!("<non-string:{other:?}>"),
    };
    let recv_text = match receiver {
        Some(recv) => format!(
            " receiver_kind={:?} receiver_cid={:?}",
            ctx.heap_kind_of(recv),
            ctx.class_id_of_object(recv)
        ),
        None => String::new(),
    };
    eprintln!(
        "[FIELDGET] op={} static={} class={} cid={} field={} slot={} desc={}{}",
        operation,
        is_static,
        class_name,
        class_id.as_u32(),
        field_name,
        slot,
        descriptor,
        recv_text
    );
}

// --- Field.get(Object) / Field.set(Object, Object) ---

pub fn native_field_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Field.get: null Field".to_string()),
            }
            .into())
        }
    };
    let receiver = match args.get(1) {
        Some(Value::Object(obj_opt)) => *obj_opt,
        _ => None,
    };

    let (is_static, class_id, slot, descriptor) = read_field_meta(ctx, this);

    // Access control: read modifiers from JDK layout, accessible from
    // the CratonVM extra slot.
    let modifiers = match ctx.get_field_by_name(this, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let accessible = read_field_accessible(ctx, this);
    // A private field's nest hosts are loaded before the access check judges
    // them, as `Field.checkAccess` -> `Reflection.verifyMemberAccess` ->
    // `getNestHost` does (JIT round 20 wave 16, lane nest;
    // `settle_reflective_nest_hosts`): a GC point only when a host is
    // loaded, and every reference this door keeps is refreshed there.
    let (mut this, mut receiver) = (this, receiver);
    if !accessible && modifiers & SA_ACC_PRIVATE != 0 {
        if let Some(caller) = resolve_caller_class_id(ctx) {
            settle_reflective_nest_hosts(
                ctx,
                caller,
                class_id,
                modifiers,
                &mut [&mut this],
                &mut [&mut receiver],
                &[],
            );
        }
    }
    // Read once, then threaded down every rank: what the refusal NAMES cannot
    // drift between ranks if they all print the same struct.
    let ident = field_ident(ctx, this, class_id, slot, &descriptor, modifiers);
    // RANKS 1-2. `Field.get` has no RANK 3: the generic accessor reads every
    // descriptor, so the null receiver the override skips past is not noticed
    // until rank 4 either way, and both spellings answer
    // `NullPointerException`.
    field_access_phase(
        ctx,
        this,
        modifiers,
        accessible,
        is_static,
        class_id,
        receiver,
        &format!("Field.get({})", descriptor),
        Some(FIELD_GET),
    )?;
    // Wave 58, lane L3: building the accessor initializes the class.
    field_door_initialize(ctx, class_id, Some(FIELD_GET), accessible)?;
    // RANK 4 (with the JDK accessor's frames of its raise, wave 54).
    field_receiver_rank(
        ctx,
        FIELD_GET,
        false,
        &descriptor,
        accessible,
        is_static,
        class_id,
        receiver,
        &ident,
        "get",
        "on",
    )?;

    // WP2.1-field вЂ” volatile-aware read fence: matches what the JDK does
    // internally via `Unsafe.getReferenceVolatile`/`getIntVolatile`. No-op
    // for non-volatile fields so the plain-read path stays cheap.
    volatile_load_fence(modifiers);

    dbg_field_get(
        ctx,
        "Field.get",
        this,
        is_static,
        class_id,
        slot,
        &descriptor,
        receiver,
    );

    let raw_value = if is_static {
        ensure_static_field_declaring_class_initialized(ctx, class_id)?;
        ctx.get_static_field(class_id, slot)
    } else {
        let recv =
            receiver.ok_or_else(
                || cratonvm_types::error::RuntimeError::NullPointerException { message: None },
            )?;
        reject_array_field_receiver(ctx, recv, "Field.get")?;
        // zt t2/nd: `Reference.referent` read reflectively is kept alive.
        let value = ctx.get_field(recv, slot);
        crate::unsafe_natives::slot0_read_keep_alive(ctx, recv, slot, value)
    };

    // Box primitive values for the generic Object return. `NativeContext`
    // field access intentionally exposes the raw slot so synthetic overlays
    // can manage their own layouts; reflection, however, has the real field
    // descriptor and must honour it. In particular, a long field whose slot
    // currently carries an Int must be widened before it is placed in a Long
    // wrapper. Otherwise Long.longValue() later receives the raw compact-Int
    // bits (0xfffc...) as a supposed long.
    let boxed_value = coerce_reflective_field_value(raw_value, &descriptor);
    // CANONICAL, not fresh. Measured on HotSpot 25.0.3+9: `f.get(o) ==
    // Character.valueOf('a')` is true for a `char` field holding 'a', and the
    // same for I/J/Z/B/S in their cache bounds — `Field.get` boxes through a
    // `MethodHandle` field accessor whose boxing step IS `X.valueOf`. The
    // `float`/`double` and out-of-bound arms stay fresh; `box_value_canonical`
    // routes those back to `box_value` itself.
    let result = box_value_canonical(ctx, boxed_value, &descriptor);
    Ok(Some(result))
}

/// Normalize a raw field slot to the type promised by reflection metadata.
///
/// This is deliberately limited to the category-2 primitive shapes that can
/// otherwise cross `Field.get(Object)` as the wrong `Value` variant. The
/// ordinary numeric-wrapper paths already handle the one-word primitive
/// descriptors directly.
///
/// # Two callers, one rule
///
/// `pub` because the VarHandle field path in `lang_invoke.rs` needs
/// exactly this and nothing weaker. `ctx.get_static_field` and `ctx.get_field`
/// are as raw there as they are here, and MEASURED on OpenJDK 25.0.3+9
/// (`scratchpad/f29/VhLong.java`, identical under `-Xint`) the two paths agree
/// row for row:
///
/// ```text
/// field.long        = java.lang.Long,   value 5,   id true
/// vh.fieldLong      = java.lang.Long,   value 5,   id true
/// vh.getAndSetLong  = java.lang.Long,   value 5,   id true
/// vh.fieldDouble    = java.lang.Double, value 1.5, id FALSE
/// ```
///
/// The direction is this one: **widen first, box canonically second.** The
/// reverse has no meaning, and "box canonically without widening" produces a
/// `Long` wrapper whose slot holds compact-`Int` bits — a wrong ANSWER rather
/// than a wrong identity.
///
/// # The lookalike in `lang_invoke.rs` that must NOT be substituted
///
/// That file has its own `widen_primitive_to_descriptor`, and the two are not
/// interchangeable. It converts NUMERICALLY —
/// `(DESC_DOUBLE, Value::Long(l)) => Value::Double(l as f64)` — where a field
/// slot must be REINTERPRETED: a `double` field holding `1.5` presents its
/// raw bits as `Value::Long(4_609_434_218_613_702_656)`, which this function
/// answers `1.5` and the numeric widener answers `4.609e18`. Same shape,
/// different operation, and the wrong one is silent. That is why the VarHandle
/// arms call across to here instead of growing a second widener locally.
pub fn coerce_reflective_field_value(value: Value, descriptor: &str) -> Value {
    match descriptor.as_bytes().first().copied() {
        Some(b'J') => match value {
            Value::Long(_) => value,
            Value::Int(v) => Value::Long(v as i64),
            Value::Double(v) => Value::Long(v.to_bits() as i64),
            Value::Float(v) => Value::Long(v.to_bits() as i64),
            Value::Object(None) | Value::Uninitialized => Value::Long(0),
            // A reference decoded out of a `long` slot: answer the slot's RAW
            // bits, which is what `getfield J` on the same slot reads
            // (`CompactValue::decode_by_descriptor(b'J')`), not the bare
            // payload address. The address was an unminted heap pointer in a
            // Java `long` (see
            // `docs/feature-designs/native-dispatch-architecture.md`);
            // the NaN-boxed bits are not a pointer to anything.
            Value::Object(Some(_)) => {
                Value::Long(cratonvm_types::CompactValue::from_value(value).to_bits() as i64)
            }
            Value::ReturnAddress(v) => Value::Long(v as i64),
        },
        Some(b'D') => match value {
            Value::Double(_) => value,
            Value::Long(v) => Value::Double(f64::from_bits(v as u64)),
            Value::Int(v) => Value::Double(v as f64),
            Value::Float(v) => Value::Double(v as f64),
            Value::Object(None) | Value::Uninitialized => Value::Double(0.0),
            // Same raw-bits rule as the `J` arm above.
            Value::Object(Some(_)) => Value::Double(f64::from_bits(
                cratonvm_types::CompactValue::from_value(value).to_bits(),
            )),
            Value::ReturnAddress(v) => Value::Double(v as f64),
        },
        _ => value,
    }
}

