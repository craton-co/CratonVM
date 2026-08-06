// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ldc` / `ldc2_w` and loader-faithful `CONSTANT_Class` resolution.
//!
//! Moved verbatim out of `interpreter.rs`'s `Helper: LDC / LDC_W (load constant from pool)`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;


/// B5: convert a malformed-constant-pool `ClassFormatError` (produced by
/// `execute_ldc`/`execute_ldc2w` on a bad CP index or wrong-type entry) into a
/// catchable Java `java/lang/ClassFormatError`. The verifier normally prevents
/// this, so it is defense-in-depth for `skip_verification` + untrusted
/// classfiles: instead of hard-unwinding via an uncatchable `VmError::Internal`,
/// Java `catch (ClassFormatError)` / `catch (LinkageError)` / `catch (Throwable)`
/// can observe it. Non-`ClassFormatError` failures pass through unchanged.
#[cold]
pub(super) fn convert_ldc_class_format_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    err: MethodCallFailed,
) -> MethodCallFailed {
    if let MethodCallFailed::InternalError(VmError::Linkage(LinkageError::ClassFormatError {
        ref message,
        ..
    })) = err
    {
        match crate::runtime::exceptions::create_exception_object(
            shared,
            thread,
            "java/lang/ClassFormatError",
            Some(message),
        ) {
            Ok(obj_ref) => return MethodCallFailed::ExceptionThrown(obj_ref),
            // Heap-exhausted / rt.jar absent during exception construction —
            // fall through to the original error so we never lose the diagnostic.
            Err(_) => return err,
        }
    }
    err
}

pub(super) fn execute_ldc(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    let frame_class_id = thread.frames[frame_idx].class_id;
    // Resolve the constant pool entry while holding the class_manager read lock.
    // For string references, we extract the string value as an owned String
    // before releasing the lock, so we can then allocate on the heap.
    enum LdcValue {
        Int(i32),
        Float(f32),
        Str(String),
        /// String constant whose Utf8 entry contains lone surrogates — carried
        /// as exact UTF-16 units (a Rust `String` cannot hold them).
        WideStr(Vec<u16>),
        ClassRef(String),
        Dynamic {
            bsm_index: u16,
            name: String,
            descriptor: String,
        },
    }

    let ldc_val = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(frame_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "current class not found".to_string(),
            })?;

        let entry = class
            .constant_pool
            .get(index)
            // Malformed classfile (bad CP index): route to a catchable
            // `ClassFormatError` (B5) rather than an uncatchable
            // `VmError::Internal`, so unverified bytecode under
            // `skip_verification` does not hard-abort the VM.
            .ok_or_else(|| {
                VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: class.name.to_string(),
                    message: format!("ldc: invalid constant pool index {index}"),
                })
            })?;

        match entry {
            ConstantPoolEntry::Integer(v) => LdcValue::Int(*v),
            ConstantPoolEntry::Float(v) => LdcValue::Float(*v),
            ConstantPoolEntry::StringReference { string_index } => {
                // Surrogate-bearing constants (e.g. ANTLR `_serializedATN`)
                // carry exact UTF-16 units in the pool's side table.
                if let Some(units) = class.constant_pool.get_utf8_wide(*string_index) {
                    LdcValue::WideStr(units.to_vec())
                } else {
                    let s = class
                        .constant_pool
                        .get_utf8(*string_index)
                        .ok_or_else(|| {
                            VmError::Linkage(LinkageError::ClassFormatError {
                                class_name: class.name.to_string(),
                                message: format!("ldc: invalid string_index {string_index}"),
                            })
                        })?
                        .to_string();
                    LdcValue::Str(s)
                }
            }
            ConstantPoolEntry::ClassReference { name_index } => {
                let name = class
                    .constant_pool
                    .get_utf8(*name_index)
                    .ok_or_else(|| {
                        VmError::Linkage(LinkageError::ClassFormatError {
                            class_name: class.name.to_string(),
                            message: format!("ldc: invalid class name_index {name_index}"),
                        })
                    })?
                    .to_string();
                if cratonvm_types::flags::runtime_var_os("CRATONVM_LDC_CLASSREF_TRACE").is_some()
                    && (name.contains("ManagementContextAutoConfiguration")
                        || name.contains("ManagementPortType")
                        || name.contains("WebEndpointAutoConfiguration"))
                {
                    eprintln!(
                        "[LDC-CLASSREF-TRACE] name={} frame_class_id={:?} frame_class_name={}",
                        name, frame_class_id, class.name
                    );
                }
                LdcValue::ClassRef(name)
            }
            ConstantPoolEntry::Dynamic {
                bootstrap_method_attr_index,
                name_and_type_index,
            } => {
                let (name, descriptor) = class
                    .constant_pool
                    .get_name_and_type(*name_and_type_index)
                    .ok_or_else(|| {
                        VmError::Linkage(LinkageError::ClassFormatError {
                            class_name: class.name.to_string(),
                            message: format!(
                                "ldc: invalid condy name_and_type at #{name_and_type_index}"
                            ),
                        })
                    })?;
                LdcValue::Dynamic {
                    bsm_index: *bootstrap_method_attr_index,
                    name: name.to_string(),
                    descriptor: descriptor.to_string(),
                }
            }
            _ => {
                return Err(VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: class.name.to_string(),
                    message: format!("ldc: unsupported constant pool entry type at #{index}"),
                })
                .into());
            }
        }
        // cm dropped here
    };

    match ldc_val {
        LdcValue::Int(v) => thread.frames[frame_idx].stack.push(Value::Int(v))?,
        LdcValue::Float(v) => thread.frames[frame_idx].stack.push(Value::Float(v))?,
        LdcValue::Str(s) => {
            let obj_ref = create_java_string(shared, &s);
            if remap_trace_on() {
                push_prov_record(obj_ref.as_ptr() as usize, "ldc-str");
            }
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(obj_ref)))?;
        }
        LdcValue::WideStr(units) => {
            let obj_ref = create_java_string_from_units(shared, &units);
            if remap_trace_on() {
                push_prov_record(obj_ref.as_ptr() as usize, "ldc-str");
            }
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(obj_ref)))?;
        }
        LdcValue::ClassRef(class_name) => {
            if crate::runtime::env_cache::dbg_toarray() {
                eprintln!(
                    "[DBG_TOARRAY] LDC class={:?} in {}",
                    class_name,
                    thread.frames[frame_idx].class_name()
                );
            }
            let referencing_class_id = thread.frames[frame_idx].class_id;
            let class_id =
                resolve_class_loader_aware(shared, thread, referencing_class_id, &class_name)
                    .map_err(|e| convert_class_not_found(shared, thread, &class_name, e))?;
            let mirror = get_or_create_class_mirror(shared, class_id);
            if remap_trace_on() {
                push_prov_record(mirror.as_ptr() as usize, "ldc-classref");
            }
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(mirror)))?;
        }
        LdcValue::Dynamic {
            bsm_index,
            name,
            descriptor,
        } => {
            // Check condy cache first
            {
                let cache = shared.classes.resolution_cache.read();
                if let Some(val) = cache.get_condy(frame_class_id, index) {
                    if remap_trace_on() {
                        if let Value::Object(Some(o)) = val {
                            push_prov_record(o.as_ptr() as usize, "ldc-condy-cached");
                        }
                    }
                    thread.frames[frame_idx].stack.push(*val)?;
                    return Ok(());
                }
            }

            // Resolve the dynamic constant by invoking its bootstrap method.
            // The bootstrap method receives (Lookup, String name, Class type, extra_args...).
            // We resolve the bootstrap method handle, then dispatch based on known BSMs.
            let (bsm_class, bsm_method, bsm_extra_args) = {
                let cm = shared.classes.class_manager.read();
                let class = cm
                    .get_class(frame_class_id)
                    .ok_or_else(|| VmError::Internal {
                        message: "condy: current class not found".to_string(),
                    })?;
                let bsm = class
                    .bootstrap_methods
                    .get(bsm_index as usize) // Widening: index conversion
                    .ok_or_else(|| VmError::Internal {
                        message: format!("condy: bootstrap method index {bsm_index} out of bounds"),
                    })?;
                let handle = crate::runtime::invokedynamic::resolve_method_handle_full(
                    &class.constant_pool,
                    bsm.bootstrap_method_ref,
                )?;
                // Resolve bootstrap argument class names for getStaticFinal etc.
                let extra: Vec<String> = bsm
                    .bootstrap_arguments
                    .iter()
                    .filter_map(|&idx| {
                        crate::runtime::invokedynamic::resolve_string_constant(
                            &class.constant_pool,
                            idx,
                        )
                        .or_else(|| {
                            // Try resolving as ClassReference
                            match class.constant_pool.get(idx) {
                                Some(ConstantPoolEntry::ClassReference { name_index }) => class
                                    .constant_pool
                                    .get_utf8(*name_index)
                                    .map(|s| s.to_string()),
                                _ => None,
                            }
                        })
                    })
                    .collect();
                (handle.class_name.clone(), handle.member_name.clone(), extra)
            };

            // Compute the result based on common bootstrap methods.
            let result = resolve_condy_value(
                shared,
                &bsm_class,
                &bsm_method,
                &name,
                &descriptor,
                &bsm_extra_args,
            )?;

            // Cache the result
            shared
                .classes
                .resolution_cache
                .write()
                .put_condy(frame_class_id, index, result);
            if remap_trace_on() {
                if let Value::Object(Some(o)) = &result {
                    push_prov_record(o.as_ptr() as usize, "ldc-condy");
                }
            }
            thread.frames[frame_idx].stack.push(result)?;
        }
    }
    Ok(())
}

/// Resolve a CONSTANT_Dynamic value based on the bootstrap method.
pub(super) fn resolve_condy_value(
    shared: &SharedVm,
    bsm_class: &str,
    bsm_method: &str,
    name: &str,
    descriptor: &str,
    bsm_extra_args: &[String],
) -> Result<Value, MethodCallFailed> {
    match (bsm_class, bsm_method) {
        ("java/lang/invoke/ConstantBootstraps", "nullConstant") => Ok(Value::Object(None)),
        ("java/lang/invoke/ConstantBootstraps", "primitiveClass") => {
            // Return the Class mirror for the named primitive type
            let mirror = crate::vm::get_or_create_primitive_mirror(shared, name);
            Ok(Value::Object(Some(mirror)))
        }
        ("java/lang/invoke/ConstantBootstraps", "getStaticFinal") => {
            // Load the static final field value from the named class.
            // The declaring class may come from bsm_extra_args[0] (4-arg variant)
            // or from the type descriptor itself (3-arg variant).
            let declaring_class = bsm_extra_args
                .first()
                .map(|s| s.as_str())
                .unwrap_or_else(|| {
                    // Fall back to the type descriptor
                    descriptor
                        .strip_prefix('L')
                        .and_then(|s| s.strip_suffix(';'))
                        .unwrap_or(descriptor)
                });
            let result = (|| -> Result<Value, MethodCallFailed> {
                if declaring_class.is_empty() || declaring_class.len() <= 1 {
                    return Ok(default_for_descriptor(descriptor));
                }
                let class_id = shared.load_class_concurrent(declaring_class)?;
                let cm = shared.classes.class_manager.read();
                if let Some(class) = cm.get_class(class_id) {
                    for (i, f) in class.fields.iter().enumerate() {
                        if &*f.name == name && f.is_static() {
                            let val = get_static_shared(shared, class_id, i);
                            return Ok(val);
                        }
                    }
                }
                Ok(default_for_descriptor(descriptor))
            })();
            result.or_else(|_| Ok(default_for_descriptor(descriptor)))
        }
        ("java/lang/invoke/ConstantBootstraps", "enumConstant") => {
            // Return the enum constant with the given name.
            // descriptor is the enum class descriptor, e.g. "Ljava/example/Color;"
            let enum_class = descriptor
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or(descriptor);
            let result = (|| -> Result<Value, MethodCallFailed> {
                let class_id = shared.load_class_concurrent(enum_class)?;
                let cm = shared.classes.class_manager.read();
                if let Some(class) = cm.get_class(class_id) {
                    for (i, f) in class.fields.iter().enumerate() {
                        if &*f.name == name && f.is_static() {
                            let val = get_static_shared(shared, class_id, i);
                            return Ok(val);
                        }
                    }
                }
                Ok(Value::Object(None))
            })();
            match result {
                Ok(val) if !matches!(val, Value::Object(None)) => Ok(val),
                _ => {
                    tracing::debug!(
                        "condy: could not resolve enum constant '{name}' of type '{descriptor}'"
                    );
                    Ok(Value::Object(None))
                }
            }
        }
        ("java/lang/invoke/ConstantBootstraps", "invoke") => {
            // invoke BSM: calls a MethodHandle passed as a bootstrap argument.
            // Without the full BSM arg resolution, return type-appropriate default.
            // The native-level ConstantBootstraps.invoke handles the real invocation
            // when called through the standard path.
            tracing::debug!("condy invoke: name='{name}', descriptor='{descriptor}'");
            Ok(default_for_descriptor(descriptor))
        }
        ("java/lang/invoke/ConstantBootstraps", "fieldVarHandle")
        | ("java/lang/invoke/ConstantBootstraps", "staticFieldVarHandle") => {
            // VarHandle bootstraps: the name is the field name, descriptor is the
            // VarHandle type. Return a minimal VarHandle synthetic.
            tracing::debug!("condy VarHandle: bsm={bsm_method}, name='{name}'");
            Ok(Value::Object(None))
        }
        ("java/lang/invoke/ConstantBootstraps", "arrayVarHandle") => {
            tracing::debug!("condy arrayVarHandle: name='{name}'");
            Ok(Value::Object(None))
        }
        // ObjectMethods bootstrap — used by records for equals/hashCode/toString
        ("java/lang/runtime/ObjectMethods", "bootstrap") => {
            tracing::debug!("condy ObjectMethods.bootstrap: name='{name}'");
            Ok(Value::Object(None))
        }
        // SwitchBootstraps — used by pattern matching switch
        ("java/lang/runtime/SwitchBootstraps", _) => {
            tracing::debug!("condy SwitchBootstraps.{bsm_method}: name='{name}'");
            Ok(default_for_descriptor(descriptor))
        }
        _ => {
            // Unknown bootstrap method — log and return type-appropriate default.
            // This covers user-defined condy bootstraps and any future JDK additions.
            tracing::debug!(
                "condy: unhandled bootstrap {bsm_class}.{bsm_method}('{name}', '{descriptor}')"
            );
            Ok(default_for_descriptor(descriptor))
        }
    }
}

/// Return a default value for a field descriptor.
pub fn default_for_descriptor(descriptor: &str) -> Value {
    match descriptor.as_bytes().first() {
        Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => Value::Int(0),
        Some(b'J') => Value::Long(0),
        Some(b'F') => Value::Float(0.0),
        Some(b'D') => Value::Double(0.0),
        _ => Value::Object(None),
    }
}

pub(super) fn execute_ldc2w(shared: &SharedVm, frame: &mut Frame, index: u16) -> Result<(), MethodCallFailed> {
    let cm = shared.classes.class_manager.read();
    let class = cm
        .get_class(frame.class_id)
        .ok_or_else(|| VmError::Internal {
            message: "current class not found".to_string(),
        })?;

    // Bounds-check the constant-pool index.  `ConstantPool::get` already
    // returns `None` for out-of-range entries, so mapping the miss to
    // `ClassFormatError` (rather than a panic) satisfies JVMS §4.4.5 for
    // ldc2_w which only accepts CONSTANT_Long_info / CONSTANT_Double_info.
    let entry = class.constant_pool.get(index).ok_or_else(|| {
        VmError::Linkage(LinkageError::ClassFormatError {
            class_name: class.name.to_string(),
            message: format!("ldc2_w: constant-pool index {index} out of range"),
        })
    })?;

    // The constant pool already carries the Long/Double tag — use it to push
    // directly as a tagged CompactValue slot, avoiding any Value-enum
    // boundary that would collapse Long into the untagged Double bucket.
    match entry {
        ConstantPoolEntry::Long(v) => frame.stack.push_long(*v)?,
        ConstantPoolEntry::Double(v) => frame.stack.push_double(*v)?,
        _ => {
            return Err(VmError::Linkage(LinkageError::ClassFormatError {
                class_name: class.name.to_string(),
                message: format!("ldc2_w: expected Long or Double at cp#{index}"),
            })
            .into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: Loader-faithful CONSTANT_Class resolution
// ---------------------------------------------------------------------------

/// Standard JDK namespaces that a user-defined loader never *isolates*: `java/*`
/// is a JVMS-prohibited package for non-bootstrap loaders, and the platform
/// namespaces below are delegated to the parent by ~every real custom loader, so
/// global resolution already yields the loader-correct answer. Short-circuiting
/// them keeps the re-entrant `loadClass` invocation off the common hot path.
/// The object component type of an array descriptor, or `None` when `name`
/// is not an array or its element type is primitive. `[[Lcom/x/Y;` yields
/// `com/x/Y` -- the nesting depth is irrelevant, only the element class needs
/// loader-faithful resolution.
#[inline]
pub(super) fn array_component_class_name(name: &str) -> Option<&str> {
    let element = name.strip_prefix('[')?.trim_start_matches('[');
    let inner = element.strip_prefix('L')?.strip_suffix(';')?;
    if inner.is_empty() {
        None
    } else {
        Some(inner)
    }
}

#[inline]
pub(super) fn is_global_resolution_namespace(name: &str) -> bool {
    name.starts_with("java/")
        || name.starts_with("javax/")
        || name.starts_with("jdk/")
        || (name.starts_with("sun/") && name != "sun/reflect/misc/Trampoline")
        || name.starts_with("com/sun/")
}

/// Read-only loader-faithful lookup: the `ClassId` that `referencing_class_id`'s
/// *user-defined* defining loader resolves `name` to, **without** any re-entrant
/// `loadClass` call — i.e. only what that loader has already defined itself (its
/// own isolated copy) or previously initiated (memoised in
/// [`SharedVm::initiating_resolution_cache`]).
///
/// Returns `None` (so the caller keeps its existing global resolution) when the
/// gate is off, the loader is built-in, the name is a JDK/array name, or the
/// loader has not yet resolved `name`. A `Some` answer is always loader-correct:
/// a class a loader has itself defined, or a prior validated initiating result.
/// This is the half of loader-faithful resolution that needs no `&mut thread`,
/// so it is safe to call from the hot field/method-owner resolvers.
/// Whether `referencing_class_id`'s DEFINING loader is a `groovy.lang.
/// GroovyClassLoader` (or a subtype, e.g. `GroovyClassLoader$InnerLoader` —
/// the per-`parseClass`-call loader Groovy mints internally). Used to widen
/// the loader-initiated resolution trigger (below) to Groovy scripts
/// unconditionally, without touching the global `CRATONVM_LOADER_AWARE_
/// RESOLUTION` gate's default (which stays off pending the full Tomcat /
/// Hibernate / WildFly custom-loader soak it was written for — see
/// `fixed-suite-bugs/hibernate/hib-proxyclassreuse-loader-blind-class-resolution-FIXED.md`).
///
/// Rationale (context.groovy bug cluster): `GroovyShell.evaluate` compiles
/// each script through its own fresh `GroovyClassLoader$InnerLoader`
/// instance, and Groovy names every closure literal in a script
/// positionally (`<Script>$_run_closure1`, `$_run_closure2`, …) — so two
/// different scripts loaded in the same process routinely produce two
/// DIFFERENT classes sharing an identical name. With the gate off, `new
/// <closure>(...)` / `checkcast` / similar `CONSTANT_Class` resolution from
/// the SECOND script's own bytecode resolved through the flat global class
/// store (`load_class_concurrent`) and silently collapsed onto whichever
/// same-named closure a DIFFERENT script's `InnerLoader` had registered
/// first — no exception, just the wrong compiled bytecode running (e.g.
/// Spring's `GroovyBeanDefinitionReader` executing an earlier script's
/// closure body and registering none of the beans the current script
/// declared). A real type check here (subtype of `GroovyClassLoader`, not a
/// class-NAME string match) keeps the blast radius to Groovy's own loader
/// hierarchy: Hibernate's ByteBuddy/CGLIB isolating loaders, Tomcat's
/// `WebappClassLoader`, and WildFly's module loaders are never
/// `GroovyClassLoader` instances, so their resolution order is completely
/// unaffected by this check.
pub(super) fn is_groovy_class_loader(shared: &SharedVm, loader_obj: cratonvm_types::ObjectRef) -> bool {
    let cm = shared.classes.class_manager.read();
    let loader_class_id = shared.mem.heap.class_id_of(loader_obj);
    match cm.get_loaded_class_id("groovy/lang/GroovyClassLoader") {
        Some(groovy_cl_id) => {
            loader_class_id == groovy_cl_id || cm.is_subclass_of(loader_class_id, groovy_cl_id)
        }
        // `groovy/lang/GroovyClassLoader` not loaded at all in this process
        // (no Groovy on the classpath) => trivially not a Groovy loader.
        None => false,
    }
}

/// Whether `referencing_class_id`'s DEFINING loader is (exactly) Spring's
/// `org.springframework.core.test.tools.CompileWithForkedClassLoaderClassLoader`
/// -- the loader `@CompileWithForkedClassLoader` gives each annotated test
/// method its own fresh instance of. Same rationale and same narrow,
/// type-checked shape as [`is_groovy_class_loader`] just above: this
/// loader's `findClass` deliberately redefines any class (INCLUDING
/// framework classes like `MergedAnnotations$SearchStrategy`, not just the
/// test's own fixtures) it can pull bytes for via `testClassLoader.
/// getResourceAsStream(...)`, so code running inside its context has a
/// genuinely FRESH `Class`/enum-constant identity for those classes, by
/// design -- matches real CGLIB/Spring behavior, works fine on HotSpot.
/// With the global gate off, a `CONSTANT_Class`/field-ref resolution
/// reached from inside that forked context for a name the built-in
/// delegation chain can ALSO serve (e.g. any `org/springframework/*`
/// class -- `is_global_resolution_namespace` only excludes `java`/`javax`/
/// `jdk`/`sun`/`com.sun`) skipped the loader-initiated fast path entirely
/// and went straight to the global, loader-blind `load_class_concurrent`,
/// which can silently create a SECOND, distinct `ClassId` for a class the
/// fork's own loader already has its own copy of -- surfacing as
/// `IllegalStateException`/`ClassCastException`-shaped `==`/`instanceof`
/// failures wherever the two identities meet (see
/// `CRATONVM-SPRING-GENUINE-BUGLIST`'s
/// `searchEnclosingClass` writeup). The class is `final` with no
/// subtypes, so an exact-id match is sufficient -- no `is_subclass_of`
/// walk needed, unlike Groovy's.
pub(super) fn is_compile_with_forked_class_loader(
    shared: &SharedVm,
    loader_obj: cratonvm_types::ObjectRef,
) -> bool {
    let cm = shared.classes.class_manager.read();
    let loader_class_id = shared.mem.heap.class_id_of(loader_obj);
    match cm.get_loaded_class_id(
        "org/springframework/core/test/tools/CompileWithForkedClassLoaderClassLoader",
    ) {
        Some(forked_cl_id) => loader_class_id == forked_cl_id,
        // Not loaded at all in this process (spring-core-test not on the
        // classpath, or the annotation never used) => trivially not this loader.
        None => false,
    }
}

/// Whether loader-initiated (JVMS §5.4.3 initiating-loader) `CONSTANT_Class`
/// resolution should run for a reference from `referencing_class_id`: either
/// the global gate is on, or the referencing class was defined by a
/// `GroovyClassLoader` (see [`is_groovy_class_loader`]'s doc comment for the
/// full rationale — this is the narrow, type-checked carve-out for the
/// context.groovy bug cluster that does not touch the gate's default).
#[inline]
pub(super) fn should_use_loader_initiated_resolution(
    shared: &SharedVm,
    referencing_class_id: ClassId,
) -> bool {
    if crate::runtime::env_cache::loader_aware_resolution() {
        return true;
    }
    match cratonvm_native_builtins::classloader::defining_loader_for(shared.vm_identity, referencing_class_id.as_u32())
    {
        Some(loader_obj) => {
            is_groovy_class_loader(shared, loader_obj)
                || is_compile_with_forked_class_loader(shared, loader_obj)
        }
        None => false,
    }
}

#[inline]
pub(super) fn lookup_loader_initiated(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    hotpath_counts::bump(&hotpath_counts::LOOKUP_LOADER_INITIATED_CALLS);
    // PERF (h2-bnf-perf 2026-07-23): several callers (resolve_class_loader_aware,
    // called on every new/checkcast/instanceof/anewarray; also
    // resolved_private_invokevirtual_target) call this unconditionally, with
    // no gate check of their own -- confirmed via call-count instrumentation
    // this function alone accounted for ~90% of ALL executed bytecode
    // instructions on an H2 BNF-autocomplete-heavy workload. get_loader_id
    // below can only ever yield UserDefined(_) for a class that was assigned
    // that identity via a path that also calls register_defining_loader for
    // the same ClassId (see that function's invariant doc in
    // native-builtins/src/classloader.rs), so when NO user-defined loader has
    // ever defined ANY class in this process, class_manager.read() below is
    // guaranteed to return None regardless of referencing_class_id -- skip
    // straight to that outcome without taking the lock. Behavior-preserving:
    // identical to the pre-existing check, just short-circuited earlier.
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered() {
        return None;
    }
    let loader = match shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing_class_id)
    {
        Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => l,
        _ => return None,
    };
    if name.starts_with('[') || is_global_resolution_namespace(name) {
        return None;
    }
    // A class the loader defines itself is authoritative. In particular, a
    // forked Spring test loader can load its own copy after an earlier
    // parent-delegated initiating-resolution cache entry exists for the same
    // binary name. Consulting that cache first collapses the fork back onto
    // the application class and lets stale static invoke-cache entries mix
    // the two identities (for example MergedAnnotation$Adapt).
    if let Some(id) = shared
        .classes
        .class_manager
        .read()
        .class_defined_by_loader_exact(name, loader)
    {
        if crate::runtime::env_cache::dbg_loader_trace() && name.contains("RootReference") {
            eprintln!("[LOADER-TRACE] lookup_loader_initiated name={name} loader={loader:?} HIT class_defined_by_loader_exact id={id:?}");
        }
        return Some(id);
    }
    let cache_hit = shared
        .classes
        .initiating_resolution_cache
        .read()
        .get(&loader)
        .and_then(|m| m.get(name))
        .copied();
    if crate::runtime::env_cache::dbg_loader_trace() && name.contains("RootReference") {
        eprintln!("[LOADER-TRACE] lookup_loader_initiated name={name} loader={loader:?} initiating_resolution_cache={cache_hit:?}");
    }
    cache_hit
}

/// Per-loader hard cap for initiating-loader memoization. Entries are an
/// optimization, not VM state: eviction re-drives `loadClass` and therefore
/// preserves JVMS resolution semantics.
pub(super) const INITIATING_RESOLUTION_CACHE_CAP: usize = 4096;

/// Generation of everything a cached *symbolic-reference resolution* depends on
/// other than the class-name → `ClassId` mapping.
///
/// Sibling of `cratonvm_classloading::class_definition_epoch`. Together the two
/// counters cover the complete input set of a resolved field/method reference,
/// so a cache whose validity condition is "this reference still resolves to
/// this answer" can be revalidated with two atomic loads instead of re-running
/// the resolution. That is what
/// [`crate::runtime::interpreter::field_access::FieldSiteCache`] does.
///
/// Bumped by:
///
/// * [`cache_loader_initiated`] below and the unload sweep in `memory::gc` —
///   the two writers of the per-loader initiating-resolution memo. Memoizing a
///   parent-delegated resolution changes what a loader answers **without**
///   touching `loaded_classes`, so `class_definition_epoch` does not see it.
/// * `vm_init::resolution_invalidate_adapter`, the hook classloading fires
///   whenever cached resolutions must be dropped. Two of its four sites —
///   `upgrade_synthetic_class` and `recompute_subclass_layouts` — change a
///   class's **field layout in place**, keeping both its `ClassId` and its name.
///   Neither the definition epoch nor the redefine latch moves for those, so
///   without this bump a resolved-field cache would keep serving a field index
///   from the pre-upgrade layout.
static RESOLUTION_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Current [`RESOLUTION_EPOCH`]. One `Acquire` load.
#[inline]
pub fn resolution_epoch() -> u64 {
    RESOLUTION_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

/// Record that cached resolutions may no longer be valid.
#[inline]
pub(crate) fn bump_resolution_epoch() {
    RESOLUTION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
}

pub(super) fn cache_loader_initiated(
    shared: &SharedVm,
    loader: cratonvm_types::ClassLoaderId,
    name: &str,
    class_id: ClassId,
) {
    bump_resolution_epoch();
    let mut caches = shared.classes.initiating_resolution_cache.write();
    let cache = caches.entry(loader).or_default();
    let key = cratonvm_types::intern_arc(name);
    if !cache.contains_key(key.as_ref()) && cache.len() >= INITIATING_RESOLUTION_CACHE_CAP {
        if let Some(victim) = cache.keys().next().cloned() {
            cache.remove(victim.as_ref());
        }
    }
    cache.insert(key, class_id);
}

/// Read only the exact definitions that belong to `referencing_class_id`'s
/// user loader. Unlike [`lookup_loader_initiated`], this intentionally never
/// consults the initiating-resolution cache.
///
/// A platform-parented isolated URL loader has no application-loader
/// delegation path. A cached result for such a loader can nevertheless point
/// at an earlier application copy (recorded before the loader had defined its
/// own framework class), collapsing a later `CONSTANT_Class` literal back to
/// that copy. Spring's `ModifiedClassPathClassLoader` then compares an
/// application `ConditionalOnMissingBean.class` against child metadata and
/// loses the annotation by Class identity. Exact definitions are safe; cache
/// entries are not for this loader shape.
#[inline]
pub(super) fn lookup_loader_defined_exact(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered()
        || name.starts_with('[')
        || is_global_resolution_namespace(name)
    {
        return None;
    }
    let loader = match shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing_class_id)
    {
        Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => l,
        _ => return None,
    };
    shared
        .classes
        .class_manager
        .read()
        .class_defined_by_loader_exact(name, loader)
}

pub(super) fn is_isolated_url_loader_definition(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
) -> bool {
    use cratonvm_native_api::NativeContext as _;
    let Some(loader) =
        cratonvm_native_builtins::classloader::defining_loader_for(shared.vm_identity, referencing_class_id.as_u32())
    else {
        return false;
    };
    let ctx = crate::vm::NativeContextImpl { shared, thread };
    cratonvm_native_builtins::classloader::url_classloader_isolated_from_app(&ctx, loader)
}

pub(super) fn isolated_loader_class_not_found(
    shared: &SharedVm,
    thread: &mut JvmThread,
    name: &str,
) -> MethodCallFailed {
    use cratonvm_native_api::NativeContext as _;
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    let exception = cratonvm_native_builtins::jboss_module_loader::alloc_single_message_exception(
        &mut ctx,
        "java/lang/NoClassDefFoundError",
        1,
        &name.replace('/', "."),
    );
    MethodCallFailed::ExceptionThrown(exception)
}

/// Resolve a `CONSTANT_Class` reference (`ldc X.class`, `new`/`anewarray`,
/// `checkcast`/`instanceof`) in a *loader-faithful* way.
///
/// `referencing_class_id` is the class whose constant pool holds the reference
/// (the executing frame's class). When the loader-aware gate
/// (`CRATONVM_LOADER_AWARE_RESOLUTION`) is on **and** that class was defined by a
/// user-defined loader, the reference is resolved through that loader as the
/// JVMS §5.4.3 *initiating* loader — by invoking its `loadClass` — so that two
/// isolating loaders which each define their own copy of `X` resolve `X` to
/// their *own* copy (HotSpot semantics) instead of collapsing to the first
/// (application) copy in CratonVM's flat global store.
///
/// Every other case — gate off, a built-in defining loader, a JDK/array name, a
/// missing loader object, or *any* failure of the loader path — falls through to
/// the legacy global [`SharedVm::load_class_concurrent`]. The loader path can
/// therefore only ever return a *more* correct answer, never a worse failure
/// than the pre-gate behavior.
///
/// Contract §9 `requested_by`: the global fallbacks below go through
/// [`SharedVm::load_class_concurrent_for`] with [`requesting_frame`], so a
/// class fabricated to satisfy a constant-pool reference records *which method*
/// referenced it. The loader-drive branches deliberately do not — a class
/// resolved by running a user `loadClass` was produced by that loader, not
/// fabricated, so there is no violation to attribute.
pub(crate) fn resolve_class_loader_aware(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Result<ClassId, MethodCallFailed> {
    // (0-pre) `java.lang.instrument` transform-on-load. Every branch below ends
    //     in a `load_class_concurrent*` or a user `loadClass`, and neither can
    //     run a Java `ClassFileTransformer` — the first holds the class-manager
    //     write lock while it defines, the second is the loader's own business.
    //     This is the last point on the constant-pool resolution path where a
    //     Java thread is in hand and no class-manager lock is held, so it is
    //     where the chain gets its shot at the bytes. The hook stages the
    //     rewritten class file; whichever branch below actually defines the
    //     class picks it up. A VM with no registered transformer pays one
    //     relaxed atomic load and a not-taken branch.
    if crate::runtime::instrument::transformers_armed(shared.vm_identity) {
        crate::runtime::instrument::pre_transform_for_load(shared, thread, name, 0);
    }
    // (0) An array reference resolves its COMPONENT type with the same
    //     initiating loader as the array reference itself (JVMS 5.3.3: the
    //     array class is synthesised from the resolved component; no class
    //     file is consulted). Every loader-faithful branch below keys on
    //     `name` itself, and `drive_defining_loader_load` declines `[` names
    //     outright -- so an array name fell straight through to the flat
    //     global `load_class_concurrent`, which fabricated a synthetic stub
    //     for a component that exists only behind a custom loader AND
    //     registered that stub globally under `Application`, permanently
    //     poisoning the component for the real loader (a stub has no `Code`,
    //     so the first real `new` of it fails with
    //     "<init> ... has no Code attribute").
    //     Found on the Keycloak 26.6.1 / Quarkus fast-jar boot: an
    //     `ldc` of `[Lorg/jboss/threads/EnhancedQueueExecutor$TaskNode;`
    //     from `EnhancedQueueExecutor.<clinit>`, whose jar is reachable only
    //     through Quarkus's `RunnerClassLoader`.
    //     Strictly additive: it only pre-resolves a component whose global
    //     answer was going to be fabricated anyway.
    //
    //     Gated on THIS referencing class having a defining loader, not on the
    //     process-wide "some loader exists" latch. The whole pre-pass exists to
    //     feed `drive_defining_loader_load`, which bails immediately without a
    //     `defining_loader_for(referencing_class_id)` — so for an app- or
    //     bootstrap-defined referencing class the `would_fabricate_synthetic_stub`
    //     probe (a class-manager read lock plus a name lookup) could only ever
    //     lead to a no-op. Same predicate, evaluated per class instead of per
    //     process; see `class_may_have_defining_loader` for why the difference
    //     is worth 1.8x on a class-parsing workload.
    if let Some(component) = array_component_class_name(name) {
        if cratonvm_native_builtins::classloader::defining_loader_for(
            shared.vm_identity,
            referencing_class_id.as_u32(),
        )
        .is_some()
            && shared
                .classes
                .class_manager
                .read()
                .would_fabricate_synthetic_stub(component)
        {
            let _ = drive_defining_loader_load(shared, thread, referencing_class_id, component);
        }
    }
    // (0b) JVMS §5.3.3: an array class is defined by its COMPONENT's defining
    //      loader, not the bootstrap loader. Every loader-faithful branch below
    //      excludes `[` names, so without this an `X[]` resolved from one user
    //      loader was handed to the next loader that asked — type confusion,
    //      since array-class identity is what `checkcast` and the verifier
    //      consult.
    //
    //      Strictly additive: the helper answers `None` for a non-array name
    //      and for a built-in referencing loader.
    if name.starts_with('[') {
        if let Some(id) = crate::runtime::interpreter::resolve_array_class_loader_aware(
            shared,
            thread,
            referencing_class_id,
            name,
        ) {
            return Ok(id);
        }
    }
    // (1) Gate / built-in / JDK-name fast paths + already-known loader-local
    //     answer — none of which need a re-entrant call.
    let direct_loader = shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing_class_id);
    let direct_user_loader = matches!(
        direct_loader,
        Some(cratonvm_types::ClassLoaderId::UserDefined(_))
    );
    // A class-manager loader id can temporarily disagree with the defining
    // loader side table for forked loaders, so retain that authoritative
    // fallback.  Application/bootstrap classes have neither and must not pay
    // for initiating-resolution map probes merely because another loader was
    // registered elsewhere in the process.
    let has_registered_defining_loader =
        cratonvm_native_builtins::classloader::defining_loader_for(shared.vm_identity, referencing_class_id.as_u32())
            .is_some();
    let has_loader_namespace = direct_user_loader || has_registered_defining_loader;
    let isolated_url_definition = if has_registered_defining_loader {
        is_isolated_url_loader_definition(shared, thread, referencing_class_id)
    } else {
        false
    };
    let known = if has_loader_namespace {
        if isolated_url_definition {
            lookup_loader_defined_exact(shared, referencing_class_id, name)
        } else {
            lookup_loader_initiated(shared, referencing_class_id, name)
        }
    } else {
        None
    };
    if let Some(id) = known {
        return Ok(id);
    }
    // `lookup_loader_initiated` returned `None`, so either this is a legacy case
    // (gate off / built-in loader / JDK or array name) or the loader is
    // user-defined but has not yet resolved this name. Only the latter takes the
    // cold loadClass path below; everything else resolves globally.
    let dbg_trace = crate::runtime::env_cache::dbg_loader_trace()
        && (name.contains("EnvironmentPostProcessorsFactory")
            || name.contains("CloudFoundryVcapEnvironmentPostProcessor")
            || name.contains("ManagementContextAutoConfiguration")
            || name.contains("ManagementPortType")
            || name.contains("ChildManagementContextInitializerAotTests")
            || name.contains("SearchStrategy")
            || name.contains("MergedAnnotations")
            || name.contains("ConditionalOnMissingBean")
            || name.contains("ConditionalOnBean")
            || name.contains("OnBeanCondition")
            || name.contains("DataSourceConfiguration")
            || name.contains("RootReference")
            || name.contains("MVMap")
            || name.contains("org/h2/Driver")
            || name.contains("AotTestContextInitializers")
            || name.contains("AotMergedContextConfiguration")
            || name.contains("DefaultCacheAwareContextLoaderDelegate")
            || name.contains("SecurityFilterAutoConfigurationEarlyInitializationTests")
            || name.contains("PathRequestTests")
            || name.contains("ManagementWebSecurityAutoConfigurationTests")
            || name.contains("WebSocketMessaging")
            || name.contains("Jackson2WebSocketMessageConverterConfiguration"));
    if dbg_trace {
        let cm = shared.classes.class_manager.read();
        let ref_name = cm
            .get_class(referencing_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let ref_loader = cm.get_loader_id(referencing_class_id);
        eprintln!(
            "[LOADER-TRACE] resolve name={name} referencing_class_id={referencing_class_id:?} referencing_class={ref_name} referencing_loader={ref_loader:?}"
        );
        if name.contains("RootReference")
            && matches!(ref_loader, Some(cratonvm_types::ClassLoaderId::Application))
        {
            drop(cm);
            eprintln!(
                "[RCLA-STACK] full Java stack at resolve_class_loader_aware for name={name}:"
            );
            for (i, f) in thread.frames.iter().enumerate().rev() {
                eprintln!(
                    "[RCLA-STACK]   [{i}] {}.{}{} pc={}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc
                );
            }
        }
    }
    // An isolated URLClassLoader (including Spring Boot's
    // ModifiedClassPathClassLoader) has the same JVMS initiating-loader
    // requirement as the existing Groovy/forked-loader cases: class literals
    // and other CONSTANT_Class consumers inside one of its private framework
    // copies must resolve through that exact loader, never through the flat
    // application store. Otherwise `OnBeanCondition`'s
    // `ConditionalOnMissingBean.class` literal is app-defined while ASM
    // metadata is child-defined, and MergedAnnotations' Class-keyed lookup
    // misses an annotation that its name-keyed lookup just found.
    let loader_faithful = should_use_loader_initiated_resolution(shared, referencing_class_id)
        || isolated_url_definition;
    let user_loader = if loader_faithful
        && has_loader_namespace
        && !name.starts_with('[')
        && !is_global_resolution_namespace(name)
    {
        let direct_loader_id = shared
            .classes
            .class_manager
            .read()
            .get_loader_id(referencing_class_id);
        match direct_loader_id {
            Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => Some(l),
            // `should_use_loader_initiated_resolution` just confirmed (via the
            // `defining_loader_for` side table) that `referencing_class_id` WAS
            // defined by a recognized user loader (Groovy or
            // CompileWithForkedClassLoader) -- but `class_manager`'s own
            // `loader_id` field for that same ClassId can disagree (return
            // `Application`/`None`), silently discarding the gate's answer and
            // falling all the way through to the loader-BLIND global fallback
            // below. Observed for the `AotTestContextInitializers`/
            // `AotTestContextInitializersFactory`/
            // `DefaultCacheAwareContextLoaderDelegate`/
            // `AotMergedContextConfiguration` family under
            // `@CompileWithForkedClassLoader` (2026-07-22 AOT bean-override
            // double-context-refresh session, see
            // CRATONVM-SPRING-GENUINE-BUGLIST) -- a `new`
            // instruction referencing one of these classes resolved via the
            // global path instead of the fork's own already-loaded copy,
            // busting a `Class`-identity-keyed cache
            // (`AotMergedContextConfiguration.hashCode()`) and causing a
            // second, uncustomized `ApplicationContext` to be created. Trust
            // the side table `should_use_loader_initiated_resolution` already
            // consulted directly instead of silently downgrading to "not a
            // user loader" on a disagreement.
            _ => cratonvm_native_builtins::classloader::defining_loader_for(shared.vm_identity, referencing_class_id.as_u32(),
            )
            .map(|loader_obj| {
                let mut ctx = crate::vm::NativeContextImpl { shared, thread };
                cratonvm_types::ClassLoaderId::UserDefined(
                    cratonvm_native_builtins::classloader::loader_namespace_id(
                        &mut ctx, loader_obj,
                    ),
                )
            }),
        }
    } else {
        None
    };
    if dbg_trace {
        eprintln!("[LOADER-TRACE] name={name} user_loader={user_loader:?}");
    }
    if user_loader.is_some() {
        // Gate-on path: drive the user loader FIRST (initiating-loader
        // semantics), then fall back to the global store.
        let driven = drive_defining_loader_load(shared, thread, referencing_class_id, name);
        if dbg_trace {
            eprintln!("[LOADER-TRACE] name={name} drive_defining_loader_load={driven:?}");
        }
        if let Some(id) = driven {
            return Ok(id);
        }
        if isolated_url_definition {
            if crate::runtime::env_cache::dbg_isolated_cnf() {
                let (ref_name, ref_loader) = {
                    let cm = shared.classes.class_manager.read();
                    (
                        cm.get_class(referencing_class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default(),
                        cm.get_loader_id(referencing_class_id),
                    )
                };
                let global = shared
                    .classes
                    .class_manager
                    .read()
                    .resolve_fast_path_class_id(name);
                eprintln!(
                    "[ISOLATED-CNF] name={name} referencing={referencing_class_id:?}/{ref_name} (loader={ref_loader:?}) global_would_be={global:?}"
                );
                for (i, f) in thread.frames.iter().enumerate().rev().take(14) {
                    eprintln!(
                        "[ISOLATED-CNF]   [{i}] {}.{}{} pc={}",
                        f.class_name(),
                        f.method_name(),
                        f.method_descriptor(),
                        f.pc
                    );
                }
            }
            return Err(isolated_loader_class_not_found(shared, thread, name));
        }
        let fallback = shared.load_class_concurrent_for(name, requesting_frame(thread));
        if dbg_trace {
            let owner = fallback
                .as_ref()
                .ok()
                .and_then(|id| shared.classes.class_manager.read().get_loader_id(*id));
            eprintln!(
                "[LOADER-TRACE] name={name} GATE-ON global fallback after drive-miss result={fallback:?} owner_loader={owner:?}"
            );
        }
        return fallback.map_err(MethodCallFailed::from);
    }

    // Gate-off / built-in defining loader: resolve globally FIRST (the legacy
    // fast path), and only if that misses fall back to the referencing class's
    // defining loader. This rescues classes that live ONLY behind a custom
    // loader and are invisible to the global classpath — e.g. a webapp class
    // referencing another class in its own `/WEB-INF/lib` jar (Tomcat's
    // `WebappClassLoader` serves these from its `WebResourceRoot`, not the
    // classpath): JSTL's `JstlCoreTLV.getHandler()` does `new
    // JstlCoreTLV$Handler(...)`, whose inner class would otherwise surface as
    // `NoClassDefFoundError` (`TestScopedAttributeELResolver`). Strictly
    // additive — only fires on what would already be a resolution failure, so
    // it never changes a previously-successful (or differently-failing)
    // resolution.
    // A name the global path can only answer with a *fabricated synthetic
    // stub* must be offered to the referencing class's own loader FIRST.
    // `load_class_concurrent` would otherwise register that stub globally
    // under `Application`, after which the real loader can never define its
    // own copy -- and a stub has no `Code` and implements no interfaces, so
    // the first real use fails (`VerifyError`, or a `ClassCastException` on
    // an interface the real class does implement). Quarkus's fast-jar
    // `RunnerClassLoader` serving `lib/quarkus/generated-bytecode.jar` is the
    // case this was written for: `new ValueRegistry_..._Synthetic_Bean()` from
    // generated Arc bytecode resolved to a stub, which then could not be cast
    // to `io.quarkus.arc.InjectableBean`. Strictly additive -- it only
    // pre-empts an answer that was going to be fake.
    //
    // Reuses `has_registered_defining_loader` (computed once above) rather than
    // the process-wide "any loader exists" latch, for the same reason as the
    // (0) pre-pass: the body is a `drive_defining_loader_load`, which needs a
    // defining loader for THIS referencing class and returns `None` without
    // one. Every `new`/`checkcast`/`instanceof` of an app-loader class reaches
    // this line -- resolution is not cached per call site -- so the probe ran
    // on the hot path of every allocation once any custom loader had ever
    // defined a class.
    if has_registered_defining_loader
        && shared
            .classes
            .class_manager
            .read()
            .would_fabricate_synthetic_stub(name)
    {
        if let Some(id) = drive_defining_loader_load(shared, thread, referencing_class_id, name) {
            if dbg_trace {
                eprintln!("[LOADER-TRACE] name={name} resolved via would-stub loader drive {id:?}");
            }
            return Ok(id);
        }
    }
    match shared.load_class_concurrent_for(name, requesting_frame(thread)) {
        Ok(id) => {
            if dbg_trace {
                let cm = shared.classes.class_manager.read();
                let owner = cm.get_loader_id(id);
                eprintln!(
                    "[LOADER-TRACE] name={name} resolved via GLOBAL-FIRST fallback cid={id:?} owner_loader={owner:?}"
                );
            }
            Ok(id)
        }
        Err(e) => {
            if let Some(id) = drive_defining_loader_load(shared, thread, referencing_class_id, name)
            {
                return Ok(id);
            }
            // An array resolution fails when its COMPONENT cannot be resolved
            // globally, and `drive_defining_loader_load` declines `[` names --
            // so offer the component to the referencing class's own loader and
            // re-synthesise. The (0) pre-pass above only covers a component the
            // global path would answer with a fabricated STUB; a component that
            // is simply absent from the process class path lands here instead.
            // Keycloak 26.6.1 / Quarkus fast-jar:
            // `[Lorg/antlr/v4/runtime/atn/ATNConfig;` from
            // `ATNConfigSet$AbstractConfigHashSet.createBuckets`, whose jar is
            // reachable only through the `RunnerClassLoader`.
            if let Some(component) = array_component_class_name(name) {
                if drive_defining_loader_load(shared, thread, referencing_class_id, component)
                    .is_some()
                {
                    if let Ok(id) = shared.load_class_concurrent_for(name, requesting_frame(thread)) {
                        return Ok(id);
                    }
                }
            }
            Err(MethodCallFailed::from(e))
        }
    }
}

/// The `(owner, method, descriptor)` of the frame that is currently executing,
/// for contract §9's `requested_by` attribution.
///
/// Three borrowed `&str`s, so a caller that resolves a class it never
/// fabricates pays three pointer copies and no allocation; the requester is
/// only formatted at the recording site, and only when a violation was actually
/// produced. Call this at the load site rather than binding it early — the
/// result borrows `thread`, and the resolution paths around it need `thread`
/// mutably.
///
/// `None` on an empty frame stack, which is VM bootstrap: a fabrication there
/// has no Java requester and the census says so rather than inventing one.
#[inline]
pub(crate) fn requesting_frame(thread: &JvmThread) -> Option<(&str, &str, &str)> {
    thread
        .frames
        .last()
        .map(|f| (f.class_name(), f.method_name(), f.method_descriptor()))
}

/// Resolve `name` by invoking the `loadClass` of the loader that DEFINED
/// `referencing_class_id` (JVMS §5.4.3 initiating loader). Returns `None` when
/// there is no recorded defining-loader object, the name is array/JDK-global, a
/// re-entrant resolution for the same (class, name) is already in flight, or the
/// loader's `loadClass` does not produce a class — in every such case the caller
/// falls back to global resolution, so this can only ever resolve MORE classes,
/// never fail one that global resolution would have answered.
pub(crate) fn drive_defining_loader_load(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    if name.starts_with('[') || is_global_resolution_namespace(name) {
        return None;
    }
    let dbg_trace = crate::runtime::env_cache::dbg_loader_trace()
        && (name.contains("EnvironmentPostProcessorsFactory")
            || name.contains("CloudFoundryVcapEnvironmentPostProcessor"));
    let loader_obj_opt =
        cratonvm_native_builtins::classloader::defining_loader_for(shared.vm_identity, referencing_class_id.as_u32());
    if dbg_trace {
        eprintln!(
            "[LOADER-TRACE] drive_defining_loader_load name={name} referencing_class_id={referencing_class_id:?} defining_loader_for={:?}",
            loader_obj_opt.map(|o| o.as_ptr())
        );
    }
    let loader_obj = loader_obj_opt?;
    // A per-thread in-flight guard breaks pathological re-entry for the same
    // (class, name) by degrading to global resolution.
    thread_local! {
        static IN_FLIGHT: std::cell::RefCell<Vec<(u32, String)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    let key = referencing_class_id.as_u32();
    if IN_FLIGHT.with(|s| s.borrow().iter().any(|(c, n)| *c == key && n == name)) {
        if crate::runtime::env_cache::dbg_isolated_cnf() {
            eprintln!("[ISOLATED-CNF] drive declined: IN_FLIGHT re-entry name={name} cid={key}");
        }
        return None;
    }
    let cache_loader = shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing_class_id);
    IN_FLIGHT.with(|s| s.borrow_mut().push((key, name.to_string())));
    let depth = thread.frames.len();
    let dotted = name.replace('/', ".");
    let result = {
        // `create_string` / `invoke_virtual` are `NativeContext` trait methods —
        // bring the trait into scope to call them.
        use cratonvm_native_api::NativeContext as _;
        let mut ctx = crate::vm::NativeContextImpl { shared, thread };
        let name_obj = ctx.create_string(&dotted);
        let result = ctx.invoke_virtual(
            loader_obj,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(name_obj))],
        );
        // Elasticsearch's EmbeddedImplClassLoader can report a failed Java
        // `loadClass` while its provider archive still contains the requested
        // implementation class. This fallback belongs at the VM's
        // initiating-loader boundary, where bytecode references have no
        // ServiceLoader context. The archive helper preserves this loader's
        // namespace and defining-loader association.
        match result {
            Ok(Some(Value::Object(Some(_)))) => result,
            _ => cratonvm_native_builtins::service_loader::impl_jars_load_class(
                &mut ctx,
                Some(loader_obj),
                name,
            )
            .map(|mirror| Ok(Some(Value::Object(Some(mirror)))))
            .unwrap_or(result),
        }
    };
    // Defensive: a re-entrant call that unwound abnormally must not leave stray
    // frames on this thread's stack.
    if thread.frames.len() > depth {
        thread.frames.truncate(depth);
        // Root-snapshot cache correctness (see `pop_and_recycle_frame_with_reason`):
        // bulk-popping stray frames re-exposes `frames[depth-1]` as the resuming
        // top, so bump its `exec_epoch` to invalidate any stale cached roots.
        if let Some(top) = thread.frames.last_mut() {
            top.exec_epoch = top.exec_epoch.wrapping_add(1);
        }
    }
    IN_FLIGHT.with(|s| {
        s.borrow_mut().pop();
    });
    if dbg_trace {
        let mapped = if let Ok(Some(Value::Object(Some(mirror)))) = result {
            crate::vm::class_id_from_mirror(shared, mirror)
        } else {
            None
        };
        eprintln!(
            "[LOADER-TRACE] drive_defining_loader_load name={name} loadClass_result={result:?} mapped_cid={mapped:?}"
        );
    }
    if let Ok(Some(Value::Object(Some(mirror)))) = result {
        if let Some(id) = crate::vm::class_id_from_mirror(shared, mirror) {
            if let Some(l) = cache_loader {
                cache_loader_initiated(shared, l, name, id);
            }
            return Some(id);
        }
        if crate::runtime::env_cache::dbg_isolated_cnf() {
            eprintln!("[ISOLATED-CNF] drive declined: mirror had no ClassId name={name}");
        }
        return None;
    }
    if crate::runtime::env_cache::dbg_isolated_cnf() {
        let shape = match &result {
            Ok(Some(Value::Object(None))) => "Ok(null)".to_string(),
            Ok(None) => "Ok(void)".to_string(),
            Ok(Some(v)) => format!("Ok(non-object {v:?})"),
            Err(MethodCallFailed::ExceptionThrown(exc)) => {
                let cm = shared.classes.class_manager.read();
                let cid = Some(shared.mem.heap.class_id_of(*exc));
                let cname = cid
                    .and_then(|c| cm.get_class(c).map(|k| k.name.to_string()))
                    .unwrap_or_default();
                format!("Err(thrown {cname})")
            }
            Err(e) => format!("Err({e:?})"),
        };
        eprintln!("[ISOLATED-CNF] drive declined: loadClass -> {shape} name={name}");
    }
    None
}
