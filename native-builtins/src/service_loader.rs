// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RA.8 — real `java.util.ServiceLoader` implementation.
//!
//! Walks the classpath via the system ClassLoader's
//! `getResources("META-INF/services/<interface>")`, reads each provider
//! descriptor line-by-line, instantiates the listed classes, and
//! returns an iterator over them. This replaces the prior stub that
//! always returned an empty iterator and therefore blocked every JDK
//! feature that depends on service-provider lookup (Charset providers,
//! java.util.spi.* services, etc.).

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, VmError};
use cratonvm_types::Value;

/// `ServiceLoader.load(Class)` — use the thread context class loader.
fn native_sl_load_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let service = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "ServiceLoader.load(null)".to_string(),
            }));
        }
    };
    // GC-safety: the two `ctx.invoke` calls below (`Thread.currentThread()`,
    // `getContextClassLoader()`) can each trigger a moving GC; `service` (the
    // `Class` mirror for the requested service type) is reused afterward in
    // `build_service_loader`, unpinned otherwise. Same "Family 1"
    // stale-ObjectRef pattern as `native_module_load_service`/
    // `native_module_load_service_from_caller_module_loader` in
    // jboss_module_loader.rs (see
    // fixed-suite-bugs/wildfly/wildfly-parallel-boot-stale-objectref-residual.md) --
    // this is the equally-hot `ServiceLoader.load(Class)` static-factory path.
    let service_pin = ctx.pin_native_root(service);
    // Fetch the thread context class loader via Thread.currentThread().getContextClassLoader().
    let tcl = ctx
        .invoke(
            "java/lang/Thread",
            "currentThread",
            "()Ljava/lang/Thread;",
            &[],
        )
        .ok()
        .and_then(|v| v);
    let loader = match tcl {
        Some(Value::Object(Some(t))) => ctx
            .invoke(
                "java/lang/Thread",
                "getContextClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[Value::Object(Some(t))],
            )
            .ok()
            .and_then(|v| v)
            .unwrap_or(Value::Object(None)),
        _ => Value::Object(None),
    };
    let service = ctx.read_native_pin(service_pin, service);
    ctx.unpin_native_roots(service_pin);
    build_service_loader(ctx, Value::Object(Some(service)), loader)
}

/// `ServiceLoader.load(Class, ClassLoader)` — pass through caller's loader.
fn native_sl_load_class_loader(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let service = match args.first() {
        Some(Value::Object(Some(o))) => Value::Object(Some(*o)),
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "ServiceLoader.load(null, ...)".to_string(),
            }));
        }
    };
    let loader = args.get(1).copied().unwrap_or(Value::Object(None));
    build_service_loader(ctx, service, loader)
}

fn build_service_loader(
    ctx: &mut dyn NativeContext,
    service: Value,
    loader: Value,
) -> MethodCallResult {
    let obj = match ctx.ensure_class_initialized("java/util/ServiceLoader") {
        Ok(cid) => {
            let real = ctx.class_num_total_fields(cid);
            ctx.alloc_object(cid, real.max(2))
        }
        Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), 2),
    };
    let obj = initialize_real_service_loader_fields(ctx, obj, service, loader)?;
    Ok(Some(Value::Object(Some(obj))))
}

fn alloc_initialized_array_list(
    ctx: &mut dyn NativeContext,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let al_cls = "java/util/ArrayList";
    let al_cid = ctx.ensure_class_initialized(al_cls).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: "ArrayList: not loaded".to_string(),
        })
    })?;
    let list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    let list_pin = ctx.pin_native_root(list);
    let list = ctx.read_native_pin(list_pin, list);
    ctx.invoke(al_cls, "<init>", "()V", &[Value::Object(Some(list))])?;
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    Ok(list)
}

fn initialize_real_service_loader_fields(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    service: Value,
    loader: Value,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    // Synthetic compatibility slots used by this native file.
    ctx.set_field(obj, 0, service);
    ctx.set_field(obj, 1, loader);

    // Real-JDK ServiceLoader.load(...) is native-overridden here, so it bypasses
    // the private constructor and the field initializers. Fill the same fields
    // that JDK 25 reload()/iterator()/stream() expect; absent names are no-ops
    // on older or synthetic layouts.
    ctx.set_field_by_name(obj, "service", service);
    ctx.set_field_by_name(obj, "loader", loader);
    ctx.set_field_by_name(obj, "layer", Value::Object(None));
    ctx.set_field_by_name(obj, "lookupIterator1", Value::Object(None));
    ctx.set_field_by_name(obj, "lookupIterator2", Value::Object(None));
    ctx.set_field_by_name(obj, "loadedAllProviders", Value::Int(0));
    ctx.set_field_by_name(obj, "reloadCount", Value::Int(0));

    let obj_pin = ctx.pin_native_root(obj);
    let service_name = ctx
        .invoke(
            "java/lang/Class",
            "getName",
            "()Ljava/lang/String;",
            &[service],
        )?
        .unwrap_or(Value::Object(None));
    let mut obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field_by_name(obj, "serviceName", service_name);

    let instantiated = alloc_initialized_array_list(ctx)?;
    obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field_by_name(
        obj,
        "instantiatedProviders",
        Value::Object(Some(instantiated)),
    );

    let loaded = alloc_initialized_array_list(ctx)?;
    obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field_by_name(obj, "loadedProviders", Value::Object(Some(loaded)));
    ctx.unpin_native_roots(obj_pin);
    Ok(obj)
}

fn native_sl_reload(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sl = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let sl_pin = ctx.pin_native_root(sl);
    for field in ["instantiatedProviders", "loadedProviders"] {
        let sl_now = ctx.read_native_pin(sl_pin, sl);
        match ctx.get_field_by_name(sl_now, field) {
            Value::Object(Some(list)) => {
                let list_pin = ctx.pin_native_root(list);
                let list_now = ctx.read_native_pin(list_pin, list);
                ctx.invoke(
                    "java/util/List",
                    "clear",
                    "()V",
                    &[Value::Object(Some(list_now))],
                )?;
                ctx.unpin_native_roots(list_pin);
            }
            _ => {
                let list = alloc_initialized_array_list(ctx)?;
                let sl_now = ctx.read_native_pin(sl_pin, sl);
                ctx.set_field_by_name(sl_now, field, Value::Object(Some(list)));
            }
        }
    }
    let sl_now = ctx.read_native_pin(sl_pin, sl);
    ctx.set_field_by_name(sl_now, "lookupIterator1", Value::Object(None));
    ctx.set_field_by_name(sl_now, "lookupIterator2", Value::Object(None));
    ctx.set_field_by_name(sl_now, "loadedAllProviders", Value::Int(0));
    let reload_count = ctx
        .get_field_by_name(sl_now, "reloadCount")
        .as_int()
        .unwrap_or(0)
        .saturating_add(1);
    ctx.set_field_by_name(sl_now, "reloadCount", Value::Int(reload_count));
    ctx.unpin_native_roots(sl_pin);
    Ok(None)
}

/// Derive candidate IMPL-JARS module directory names from a service/class FQN.
///
/// ES stores provider JARs as `IMPL-JARS/<module>/<ver>.jar` inside the outer
/// module JAR. The module name is kebab-case but the Java package component is
/// camelCase/lowercase. We maintain a small known table for ES core modules and
/// fall back to the first package component (lowercase) for unknown packages.
///
/// Example: `org.elasticsearch.xcontent.spi.XContentProvider` → `["x-content"]`.
///
/// The x-content archive also embeds Jackson and SnakeYAML. Those implementation
/// classes do not share Elasticsearch's package prefix, but they must resolve
/// through the same archive once an x-content provider links against them.
pub(crate) fn derive_impl_jar_module_names(fqn: &str) -> Vec<String> {
    const KNOWN: &[(&str, &str)] = &[
        ("org.elasticsearch.xcontent", "x-content"),
        ("org.elasticsearch.xpack", "x-pack"),
        ("org.elasticsearch.transport", "transport"),
        ("org.elasticsearch.common", "common"),
        ("org.elasticsearch.core", "core"),
    ];
    for (prefix, module) in KNOWN {
        if fqn.starts_with(prefix) {
            return vec![module.to_string()];
        }
    }
    if fqn.starts_with("com.fasterxml.jackson.") || fqn.starts_with("org.yaml.snakeyaml.") {
        return vec!["x-content".to_string()];
    }
    // Generic fallback: strip "org.elasticsearch.", take first component.
    if let Some(rest) = fqn.strip_prefix("org.elasticsearch.") {
        let first = rest.split('.').next().unwrap_or("").to_lowercase();
        if !first.is_empty() {
            return vec![first];
        }
    }
    Vec::new()
}

/// Read a resource from an IMPL-JARS "inner JAR" path.
///
/// ES's IMPL-JARS layout stores inner JAR contents as individual ZIP entries
/// using a path prefix: `IMPL-JARS/<module>/<jar_name>/<resource_path>`.
/// The `<jar_name>` component (e.g. `x-content-impl-8.15.5.jar`) is a
/// directory prefix in the outer JAR, NOT a binary JAR blob.
pub(crate) fn try_read_from_inner_jar(
    ctx: &mut dyn NativeContext,
    module_name: &str,
    jar_name: &str,
    resource_path: &str,
) -> Option<Vec<u8>> {
    let direct_path = format!("IMPL-JARS/{module_name}/{jar_name}/{resource_path}");
    ctx.find_all_resource_bytes(&direct_path)
        .into_iter()
        .next()
        .or_else(|| ctx.find_resource(&direct_path))
}

/// Return the non-JDK class references recorded in a class file's constant
/// pool.  The embedded archive has no directory index, so these references
/// are the bounded, loader-faithful way to discover the provider dependency
/// closure without depending on the flat application class path.
fn embedded_class_references(bytes: &[u8]) -> Vec<String> {
    if bytes.len() < 10 || bytes[..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return Vec::new();
    }
    let mut pos = 8usize;
    let read_u16 = |offset: &mut usize| -> Option<u16> {
        let end = offset.checked_add(2)?;
        let value = u16::from_be_bytes(bytes.get(*offset..end)?.try_into().ok()?);
        *offset = end;
        Some(value)
    };
    let Some(count) = read_u16(&mut pos).map(usize::from) else {
        return Vec::new();
    };
    let mut utf8 = vec![None; count];
    let mut class_name_indices = Vec::new();
    let mut index = 1usize;
    while index < count {
        let Some(&tag) = bytes.get(pos) else {
            return Vec::new();
        };
        pos += 1;
        match tag {
            1 => {
                let Some(length) = read_u16(&mut pos).map(usize::from) else {
                    return Vec::new();
                };
                let Some(end) = pos.checked_add(length) else {
                    return Vec::new();
                };
                let Some(value) = bytes.get(pos..end) else {
                    return Vec::new();
                };
                utf8[index] = Some(String::from_utf8_lossy(value).into_owned());
                pos = end;
            }
            7 => {
                let Some(name_index) = read_u16(&mut pos) else {
                    return Vec::new();
                };
                class_name_indices.push(name_index as usize);
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => {
                pos = match pos.checked_add(4) {
                    Some(v) => v,
                    None => return Vec::new(),
                }
            }
            5 | 6 => {
                pos = match pos.checked_add(8) {
                    Some(v) => v,
                    None => return Vec::new(),
                };
                index += 1;
            }
            8 | 16 | 19 | 20 => {
                pos = match pos.checked_add(2) {
                    Some(v) => v,
                    None => return Vec::new(),
                }
            }
            15 => {
                pos = match pos.checked_add(3) {
                    Some(v) => v,
                    None => return Vec::new(),
                }
            }
            _ => return Vec::new(),
        }
        index += 1;
    }
    class_name_indices
        .into_iter()
        .filter_map(|index| utf8.get(index).and_then(Clone::clone))
        .filter(|name| {
            !name.starts_with('[')
                && !name.starts_with("java/")
                && !name.starts_with("javax/")
                && !name.starts_with("jdk/")
                && !name.starts_with("sun/")
                && !name.starts_with("org/w3c/")
                && !name.starts_with("org/xml/")
        })
        .collect()
}

fn preload_embedded_dependencies(
    ctx: &mut dyn NativeContext,
    loader: cratonvm_types::ObjectRef,
    bytes: &[u8],
) {
    for dependency in embedded_class_references(bytes) {
        if !derive_impl_jar_module_names(&dependency.replace('/', ".")).is_empty() {
            let _ = impl_jars_load_class(ctx, Some(loader), &dependency);
        }
    }
}

/// Try to load a class from the IMPL-JARS flat-directory layout.
///
/// ES stores `IMPL-JARS/<module>/<jar_name>/<classfile>` as individual entries
/// in the outer JAR. When a class is not on the flat classpath, this helper
/// derives the module name, reads LISTING.TXT, and scans each listed jar
/// directory for the class file. Returns the Class mirror on success.
///
/// Used from both `service_loader.rs` (load provider class) and
/// `classloader.rs` (class resolution fallback for inner / helper classes).
pub fn impl_jars_load_class(
    ctx: &mut dyn NativeContext,
    defining_loader: Option<cratonvm_types::ObjectRef>,
    internal_name: &str,
) -> Result<Option<cratonvm_types::ObjectRef>, MethodCallFailed> {
    let mut visited = std::collections::HashSet::new();
    Ok(impl_jars_load_class_inner(ctx, defining_loader, internal_name, &mut visited)?)
}

fn impl_jars_load_class_inner(
    ctx: &mut dyn NativeContext,
    defining_loader: Option<cratonvm_types::ObjectRef>,
    internal_name: &str,
    visited: &mut std::collections::HashSet<String>,
) -> Result<Option<cratonvm_types::ObjectRef>, MethodCallFailed> {
    if !visited.insert(internal_name.to_owned()) {
        return Ok(None);
    }
    let dotted = internal_name.replace('/', ".");
    let class_file = format!("{internal_name}.class");
    for module_name in derive_impl_jar_module_names(&dotted) {
        let listing_path = format!("IMPL-JARS/{module_name}/LISTING.TXT");
        let first_bytes = ctx
            .find_all_resource_bytes(&listing_path)
            .into_iter()
            .next();
        let Some(listing_bytes) = first_bytes.or_else(|| ctx.find_resource(&listing_path)) else {
            continue;
        };
        // A caller that supplied an existing loader needs classes defined by
        // that exact instance. Creating a fresh EmbeddedImplClassLoader here
        // would split the provider and its dependencies across two namespaces.
        if module_name == "x-content" && defining_loader.is_none() {
            let app_loader = crate::classloader::get_or_create_app_loader(ctx)?;
            // GC-safety: `create_string` below can trigger a moving GC;
            // `app_loader` (the shared application-classloader singleton) is
            // reused as an `invoke` argument afterward, unpinned otherwise.
            let app_loader_pin = ctx.pin_native_root(app_loader);
            let module_name_obj = ctx.create_string(&module_name);
            let app_loader = ctx.read_native_pin(app_loader_pin, app_loader);
            ctx.unpin_native_roots(app_loader_pin);
            if let Ok(Some(Value::Object(Some(loader)))) = ctx.invoke(
                "org/elasticsearch/core/internal/provider/EmbeddedImplClassLoader",
                "getInstance",
                "(Ljava/lang/ClassLoader;Ljava/lang/String;)Lorg/elasticsearch/core/internal/provider/EmbeddedImplClassLoader;",
                &[Value::Object(Some(app_loader)), Value::Object(Some(module_name_obj))],
            ) {
                let name_obj = ctx.create_string(&dotted);
                if let Ok(Some(Value::Object(Some(mirror)))) = ctx.invoke_virtual(
                    loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[Value::Object(Some(name_obj))],
                ) {
                    return Ok(Some(mirror));
                }
            }
        }
        let listing_text = String::from_utf8_lossy(&listing_bytes).into_owned();
        for jar_name in listing_text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && l.ends_with(".jar"))
        {
            if let Some(class_bytes) =
                try_read_from_inner_jar(ctx, &module_name, jar_name, &class_file)
            {
                // Link the dependency closure before defining this class. The
                // class manager resolves a superclass/interface as part of
                // `define_class_full`; for example ESJsonFactoryBuilder needs
                // Jackson's builder type before its own bytes can be accepted.
                let defining_loader = if let Some(loader) = defining_loader {
                    let loader_pin = ctx.pin_native_root(loader);
                    for dependency in embedded_class_references(&class_bytes) {
                        if dependency != internal_name
                            && !derive_impl_jar_module_names(&dependency.replace('/', "."))
                                .is_empty()
                        {
                            let current_loader = ctx.read_native_pin(loader_pin, loader);
                            let _ = impl_jars_load_class_inner(
                                ctx,
                                Some(current_loader),
                                &dependency,
                                visited,
                            );
                        }
                    }
                    let loader = ctx.read_native_pin(loader_pin, loader);
                    ctx.unpin_native_roots(loader_pin);
                    Some(loader)
                } else {
                    None
                };
                let opts = cratonvm_native_api::DefineClassFull {
                    skip_verification: true,
                    ..Default::default()
                };
                let loader_id = defining_loader
                    .map(|loader| crate::classloader::get_or_assign_loader_id(ctx, loader))
                    .unwrap_or(0);
                if let Ok(cid) = ctx.define_class_full(internal_name, &class_bytes, loader_id, opts)
                {
                    if let Some(loader) = defining_loader {
                        crate::classloader::register_defining_loader(ctx.vm_identity(), cid.as_u32(), loader);
                    }
                    return Ok(Some(ctx.get_class_mirror(cid)));
                }
            }
        }
    }
    Ok(None)
}

/// If the ServiceLoader carries a non-builtin ClassLoader, return it so the
/// caller can use `loader.loadClass(fqn)` to load provider classes that live
/// inside embedded JARs and are invisible to the flat `Class.forName(fqn)` scan.
fn sl_non_builtin_loader(
    ctx: &mut dyn NativeContext,
    sl: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    fn usable_loader(
        ctx: &mut dyn NativeContext,
        r: cratonvm_types::ObjectRef,
    ) -> Option<cratonvm_types::ObjectRef> {
        let name = ctx
            .class_name_of_id(ctx.class_id_of_object(r))
            .unwrap_or_default();
        if !name.contains("ClassLoader") || crate::classloader::is_builtin_loader_class(&name) {
            None
        } else {
            Some(r)
        }
    }

    if let Value::Object(Some(r)) = ctx.get_field_by_name(sl, "loader") {
        if let Some(loader) = usable_loader(ctx, r) {
            return Some(loader);
        }
    }
    // Synthetic ServiceLoader instances built by build_service_loader keep the
    // loader in legacy slot 1. Real JDK ServiceLoader objects use named fields;
    // with load(layer, service), slot 1 is not a ClassLoader and must not be
    // treated as one.
    if matches!(ctx.get_field_by_name(sl, "layer"), Value::Object(Some(_))) {
        return None;
    }
    match ctx.get_field(sl, 1) {
        Value::Object(Some(r)) => usable_loader(ctx, r),
        _ => None,
    }
}

fn load_provider_class_from_loader_jars(
    ctx: &mut dyn NativeContext,
    loader: cratonvm_types::ObjectRef,
    internal_name: &str,
) -> Option<cratonvm_types::ObjectRef> {
    if let Some(cid) = ctx.class_id_by_name(internal_name) {
        return Some(ctx.get_class_mirror(cid));
    }
    let class_file = format!("{internal_name}.class");
    let Value::Object(Some(jm_list_r)) = ctx.get_field_by_name(loader, "jarMetas") else {
        return None;
    };
    let size = match ctx.invoke(
        "java/util/List",
        "size",
        "()I",
        &[Value::Object(Some(jm_list_r))],
    ) {
        Ok(Some(Value::Int(n))) => n,
        _ => 0,
    };
    for i in 0..size {
        let jm = match ctx.invoke(
            "java/util/List",
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(jm_list_r)), Value::Int(i)],
        ) {
            Ok(Some(Value::Object(Some(r)))) => r,
            _ => continue,
        };
        let prefix = match ctx.invoke_virtual(jm, "prefix", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => continue,
        };
        if prefix.is_empty() {
            continue;
        }
        let path = format!("{prefix}/{class_file}");
        let mut candidates = ctx.find_all_resource_bytes(&path);
        if candidates.is_empty() {
            if let Some(bytes) = ctx.find_resource(&path) {
                candidates.push(bytes);
            }
        }
        for class_bytes in candidates {
            let opts = cratonvm_native_api::DefineClassFull {
                skip_verification: true,
                ..Default::default()
            };
            // This path is entered when the provider's own Java loader could
            // not materialize its class.  It is still a definition *by that
            // loader*, not by the application loader: the provider's nested
            // classes and dependencies must subsequently resolve through the
            // same EmbeddedImplClassLoader.
            let loader_id = crate::classloader::get_or_assign_loader_id(ctx, loader);
            if let Ok(cid) = ctx.define_class_full(internal_name, &class_bytes, loader_id, opts) {
                crate::classloader::register_defining_loader(ctx.vm_identity(), cid.as_u32(), loader);
                preload_embedded_dependencies(ctx, loader, &class_bytes);
                return Some(ctx.get_class_mirror(cid));
            }
            if let Some(cid) = ctx.class_id_by_name(internal_name) {
                return Some(ctx.get_class_mirror(cid));
            }
        }
    }
    None
}

/// Load a provider class by FQN. When a specific `loader` is known (the
/// common case — JBoss Modules' `ModuleClassLoader`, Elasticsearch's
/// `EmbeddedImplClassLoader`, etc.), try it FIRST via `loader.loadClass`/
/// `loader.findClass`, falling back to the context-free flat
/// `Class.forName(fqn)` scan only if the loader can't resolve it (or no
/// loader was given at all).
///
/// Order matters here: `Class.forName(fqn)` (1-arg) resolves against
/// whatever classloader CratonVM treats as the "caller" for a native-invoked
/// call — NOT `loader`. If that context-free attempt reaches far enough to
/// define the class but then fails `<clinit>` (e.g. because a dependency
/// only visible through `loader`'s own module-scoped resolution can't be
/// found from the wrong context), the class's `ClassState` is permanently
/// poisoned to `InitializationError` — and JVM class state never resets.
/// A LATER, correct resolution via `loader` then returns the mirror for that
/// SAME already-poisoned `ClassId` (`Class.forName`/`loadClass` return the
/// existing class once it's defined, regardless of which loader asks), so
/// `load_provider_class` reports success but `Constructor.newInstance()`
/// throws `NoClassDefFoundError` on first real use. Trying the correct
/// loader first avoids ever touching the wrong-context path when we already
/// know the right one.
///
/// `loadClass` MUST be tried before `findClass`, not after. Real
/// `java.util.ServiceLoader` resolves each provider via
/// `Class.forName(cn, false, loader)`, which is spec'd to invoke `loader`'s
/// PUBLIC `loadClass(String)` — never the `protected findClass(String)`
/// helper directly. `findClass` exists to be called BY a loader's own
/// `loadClass()` algorithm (typically after parent delegation fails), not by
/// external callers — a loader with custom `loadClass()` delegation logic
/// (e.g. spring-core-test's `CompileWithForkedClassLoaderClassLoader`, whose
/// `loadClass(String)` special-cases `org.junit`/`org.testng` names to load
/// them through a DIFFERENT loader instance rather than itself) has that
/// logic entirely in `loadClass()`; its `findClass()` override has no such
/// special case and unconditionally defines the class into itself. Calling
/// `findClass` first bypasses `loadClass()`'s delegation, so the SAME
/// class name (e.g. `TestNGTestEngine`) ends up defined under two different
/// loader identities depending on which internal path reached it first (one
/// via this synthetic ServiceLoader's `findClass`-first probe, one via a
/// later ordinary `Class.forName(name, false, loader)` that correctly went
/// through `loadClass()`) — a same-name loader-identity split that then
/// throws a spurious `IllegalAccessError: ... not public, different
/// package` the moment the wrongly-self-defined copy's `<clinit>`
/// cross-references a package-private sibling class resolved through the
/// correct loader.
fn load_provider_class(
    ctx: &mut dyn NativeContext,
    fqn: &str,
    loader: Option<cratonvm_types::ObjectRef>,
) -> Result<Option<cratonvm_types::ObjectRef>, MethodCallFailed> {
    if let Some(loader_r) = loader {
        // Both create_string calls can collect, so pin the module or custom
        // loader for the entire loadClass/findClass/fallback sequence.
        let loader_pin = ctx.pin_native_root(loader_r);
        let name2 = ctx.create_string(fqn);
        let loader_r = ctx.read_native_pin(loader_pin, loader_r);
        let load_result = ctx.invoke_virtual(
            loader_r,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(name2))],
        );
        if let Ok(Some(Value::Object(Some(c)))) = load_result {
            ctx.unpin_native_roots(loader_pin);
            return Ok(Some(c));
        }
        let find_name = ctx.create_string(fqn);
        let loader_r = ctx.read_native_pin(loader_pin, loader_r);
        if let Ok(Some(Value::Object(Some(c)))) = ctx.invoke_virtual(
            loader_r,
            "findClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(find_name))],
        ) {
            ctx.unpin_native_roots(loader_pin);
            return Ok(Some(c));
        }
        let loader_r = ctx.read_native_pin(loader_pin, loader_r);
        let from_loader_jars =
            load_provider_class_from_loader_jars(ctx, loader_r, &fqn.replace('.', "/"));
        ctx.unpin_native_roots(loader_pin);
        if let Some(c) = from_loader_jars {
            return Ok(Some(c));
        }
    }
    // No loader, or the loader couldn't resolve it — fall back to the
    // context-free flat scan.
    let name = ctx.create_string(fqn);
    if let Ok(Some(Value::Object(Some(c)))) = ctx.invoke(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(name))],
    ) {
        return Ok(Some(c));
    }
    // Final fallback: IMPL-JARS nested-JAR scan.
    Ok(impl_jars_load_class(ctx, None, &fqn.replace('.', "/"))?)
}

/// Read provider FQNs for `sl.service` from every
/// `META-INF/services/<fqcn>` resource visible on the classpath.
/// Each line in each resource file contributes one provider name.
///
/// WP1.8-narrow (session 94): the original implementation drove the
/// JDK chain `ClassLoader.getResources(String) -> Enumeration<URL> ->
/// URL.openStream() -> InputStreamReader.<init> -> BufferedReader.
/// <init>(Reader) -> readLine()`. That chain depends on synthetic-stub
/// surface (`URL.openStream`, `BufferedReader` constructor + readLine)
/// that is incomplete in the open-sourced revision and surfaces as
/// `NoSuchMethodError` at bytecode resolution before the proper natives
/// are reached. The WP7.1 commit (`245e996`) confirms this and works
/// around the gap in `jdbc.rs::collect_driver_providers` by walking the
/// classpath directly.
///
/// **Hybrid resolution (session 94 merge):** the WP1.8-narrow approach
/// (parsing `find_all_resource_urls` URL strings inline) and the
/// Wave 8 closure approach (a layered `find_all_resource_bytes` helper
/// across `ClassPath` / `ClassManager` / `NativeContext` / `Vm`) target
/// the same goal. The hybrid keeps WP1.8-narrow's structure and tests
/// but routes byte fetching through the layered helper — the URL-string
/// detour and the inline `read_resource_bytes` jar-cracker disappear,
/// leaving classpath-flavour handling owned by `class_path.rs` where
/// every classpath consumer (jdbc, here, anywhere else later) sees a
/// uniform implementation. Functionally identical to the prior
/// implementation when the JDK chain works (lex-sorted, dedup'd FQNs
/// from every classpath match); strictly more robust when it doesn't.
fn discover_providers(
    ctx: &mut dyn NativeContext,
    sl: cratonvm_types::ObjectRef,
) -> Result<Vec<String>, MethodCallFailed> {
    // This function repeatedly re-enters Java while inspecting the loader.
    // Keep both the ServiceLoader and its service Class rooted across the
    // initial getName call; either one can move during that dispatch.
    let sl_pin = ctx.pin_native_root(sl);
    let sl = ctx.read_native_pin(sl_pin, sl);
    let service_class = match ctx.get_field_by_name(sl, "service") {
        Value::Object(Some(c)) => c,
        _ => match ctx.get_field(sl, 0) {
            Value::Object(Some(c)) => c,
            _ => {
                ctx.unpin_native_roots(sl_pin);
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: "ServiceLoader: service class is null".to_string(),
                }));
            }
        },
    };
    let service_pin = ctx.pin_native_root(service_class);
    let service_name_val = match ctx.invoke(
        "java/lang/Class",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(service_class))],
    ) {
        Ok(value) => value,
        Err(err) => {
            ctx.unpin_native_roots(service_pin);
            ctx.unpin_native_roots(sl_pin);
            return Err(err);
        }
    };
    ctx.unpin_native_roots(service_pin);
    let service_name = match service_name_val {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if service_name.is_empty() {
        ctx.unpin_native_roots(sl_pin);
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: "ServiceLoader: service.getName() returned null".to_string(),
        }));
    }
    let sl = ctx.read_native_pin(sl_pin, sl);
    let resource = format!("META-INF/services/{}", service_name);

    let mut providers: Vec<String> = Vec::new();

    // JPMS layer-backed ServiceLoader.load(layer, service). Real JDK
    // ServiceLoader discovers these providers from ModuleLayer.servicesCatalog,
    // not from META-INF/services resources. CratonVM overrides iterator()/stream(),
    // so reproduce that source here and remember the module loader for provider
    // class instantiation below.
    if let Value::Object(Some(layer)) = ctx.get_field_by_name(sl, "layer") {
        let layer_pin = ctx.pin_native_root(layer);
        let layer = ctx.read_native_pin(layer_pin, layer);
        let _ = ctx.invoke(
            "java/lang/ModuleLayer",
            "modules",
            "()Ljava/util/Set;",
            &[Value::Object(Some(layer))],
        );
        let layer = ctx.read_native_pin(layer_pin, layer);
        if let Value::Object(Some(catalog)) = ctx.get_field_by_name(layer, "servicesCatalog") {
            let catalog_pin = ctx.pin_native_root(catalog);
            let service_name_obj = ctx.create_string(&service_name);
            let catalog = ctx.read_native_pin(catalog_pin, catalog);
            if let Ok(Some(Value::Object(Some(service_list)))) = ctx.invoke(
                "jdk/internal/module/ServicesCatalog",
                "findServices",
                "(Ljava/lang/String;)Ljava/util/List;",
                &[
                    Value::Object(Some(catalog)),
                    Value::Object(Some(service_name_obj)),
                ],
            ) {
                let list_pin = ctx.pin_native_root(service_list);
                let size = match ctx.invoke(
                    "java/util/List",
                    "size",
                    "()I",
                    &[Value::Object(Some(service_list))],
                ) {
                    Ok(Some(Value::Int(n))) => n,
                    _ => 0,
                };
                for i in 0..size {
                    let service_list = ctx.read_native_pin(list_pin, service_list);
                    let sp = match ctx.invoke(
                        "java/util/List",
                        "get",
                        "(I)Ljava/lang/Object;",
                        &[Value::Object(Some(service_list)), Value::Int(i)],
                    ) {
                        Ok(Some(Value::Object(Some(sp)))) => sp,
                        _ => continue,
                    };
                    if let Value::Object(Some(provider_name)) =
                        ctx.get_field_by_name(sp, "providerName")
                    {
                        let fqn = ctx.read_string(provider_name).unwrap_or_default();
                        if !fqn.is_empty() {
                            providers.push(fqn);
                        }
                    }
                    if let Value::Object(Some(module)) = ctx.get_field_by_name(sp, "module") {
                        if let Value::Object(Some(loader)) = ctx.get_field_by_name(module, "loader")
                        {
                            let loader_class = ctx
                                .class_name_of_id(ctx.class_id_of_object(loader))
                                .unwrap_or_default();
                            if loader_class.contains("ClassLoader")
                                && !crate::classloader::is_builtin_loader_class(&loader_class)
                            {
                                let sl_current = ctx.read_native_pin(sl_pin, sl);
                                ctx.set_field_by_name(
                                    sl_current,
                                    "loader",
                                    Value::Object(Some(loader)),
                                );
                            }
                        }
                    }
                }
                ctx.unpin_native_roots(list_pin);
            }
            ctx.unpin_native_roots(catalog_pin);
        }
        ctx.unpin_native_roots(layer_pin);
    }

    // When the ServiceLoader was created with a user-defined ClassLoader
    // (e.g. ES's EmbeddedImplClassLoader which stores providers inside
    // embedded jar trees like IMPL-JARS/<module>/<ver>.jar/<path>), the
    // flat scan below won't find the descriptor at the top-level path.
    //
    // Delegate to loader.getResources(resource) → Enumeration<URL>,
    // then for each URL extract the JAR-entry path and read bytes directly,
    // mirroring what the real JDK ServiceLoader does via
    // LazyClassPathLookupIterator → loader.getResources(name).
    let loader_ref_opt: Option<cratonvm_types::ObjectRef> = {
        let v = match ctx.get_field_by_name(sl, "loader") {
            Value::Object(Some(r)) => Some(r),
            // Legacy slot 1 is where `build_service_loader` puts the loader on a
            // SYNTHETIC ServiceLoader. On a real-JDK `ServiceLoader` slot 1 is
            // `serviceName`, a `String` — and this fallback used to accept it,
            // because the only filter was "is it a BUILT-IN loader class?" and
            // `java/lang/String` is not one. The result was
            // `invoke_virtual(<String>, "getResources")`, i.e. a
            // `NoSuchMethodError java/lang/String.getResources` and a silent
            // fall-through to the flat scan for every real `ServiceLoader` whose
            // `loader` field this VM left null. Seen on
            // `com.sun.tools.attach.spi.AttachProvider.providers()` and
            // `sun.jvmstat.monitor.MonitoredHost.getMonitoredHost`.
            //
            // `sl_non_builtin_loader` already had the missing check; this is the
            // second reader of the same two fields and it did not.
            _ => match ctx.get_field(sl, 1) {
                Value::Object(Some(r)) => Some(r),
                _ => None,
            },
        };
        match v {
            Some(r) => {
                let cid = ctx.class_id_of_object(r);
                let name = ctx.class_name_of_id(cid).unwrap_or_default();
                // "Actually a ClassLoader" is either answer being yes: the
                // hierarchy walk, OR the name heuristic `sl_non_builtin_loader`
                // has always used. Requiring the walk alone would be a real
                // tightening — a synthetic-jdk stub's superclass chain does not
                // always reach `java/lang/ClassLoader`, and rejecting a loader
                // this path used to accept would silently lose providers. Both
                // answers are no for the `java/lang/String` this guard exists
                // for.
                let is_loader = crate::classloader::is_classloader_instance(ctx, r)
                    || name.contains("ClassLoader");
                if crate::classloader::is_builtin_loader_class(&name) || !is_loader {
                    None
                } else {
                    Some(r)
                }
            }
            None => None,
        }
    };
    let loader_is_jboss_module = loader_ref_opt
        .map(|r| {
            ctx.class_name_arc_of_id(ctx.class_id_of_object(r)).as_deref()
                == Some("org/jboss/modules/ModuleClassLoader")
        })
        .unwrap_or(false);

    // Is the loader-scoped lookup below EXHAUSTIVE for this loader — i.e. may an
    // empty result be taken at face value?
    //
    // The flat classpath scan further down unions the `META-INF/services/<svc>`
    // descriptors of EVERY jar in the process, with no notion of any one
    // loader's classpath. For a loader built specifically to hide a jar
    // (Spring Boot's `ModifiedClassPathClassLoader` under
    // `@ClassPathExclusions`), that scan re-adds the very provider registration
    // the exclusion removed, while `loadClass` still refuses the class it names
    // — so `ServiceLoader` reads a registration it cannot honour and raises
    // `ServiceConfigurationError: <svc>: Provider <cn> not found` (the
    // `loaded == 0 && !missing.is_empty()` arm below) where HotSpot simply
    // discovers no providers.
    //
    // `loader_owns_complete_resource_view` is true only when the receiver is a
    // `URLClassLoader`-family loader whose own URL list CratonVM can enumerate in
    // full — exactly the condition under which `ucl_find_resources` returns an
    // authoritative (possibly empty) answer. Every other loader keeps the flat
    // scan, including one whose URLs were never recorded: there the loader-scoped
    // lookup could not run at all, and an empty result means nothing.
    let loader_view_is_exhaustive = loader_ref_opt.is_some_and(|r| {
        crate::classloader::loader_owns_complete_resource_view(ctx, r) && !loader_is_jboss_module
    });

    let diag_sl = crate::nbflags().diag_serviceloader;

    if loader_is_jboss_module {
        if let Some(loader_r) = loader_ref_opt {
            let loader_pin = ctx.pin_native_root(loader_r);
            let loader_r = ctx.read_native_pin(loader_pin, loader_r);
            if let Some(module_name) = crate::jboss_module_loader::module_name_of_mcl(ctx, loader_r)
            {
                providers.extend(crate::jboss_module_loader::module_service_provider_names(
                    &module_name,
                    &service_name,
                ));
                providers.sort();
                providers.dedup();
                if diag_sl {
                    eprintln!(
                        "[SL-LOADER-DBG] JBoss module service module={} service={} providers={} ({:?})",
                        module_name,
                        service_name,
                        providers.len(),
                        providers
                    );
                }
            } else if diag_sl {
                eprintln!("[SL-LOADER-DBG] JBoss ModuleClassLoader has no module backref");
            }
            ctx.unpin_native_roots(loader_pin);
        }
    }

    if let Some(loader_r) = loader_ref_opt {
        // Primary path: call loader.getResources(resource) → Enumeration<URL>,
        // then extract the entry path from each URL and read bytes directly.
        let loader_pin = ctx.pin_native_root(loader_r);
        let res_name_val = Value::Object(Some(ctx.create_string(&resource)));
        let loader_r = ctx.read_native_pin(loader_pin, loader_r);
        let enum_res = ctx.invoke_virtual(
            loader_r,
            "getResources",
            "(Ljava/lang/String;)Ljava/util/Enumeration;",
            &[res_name_val],
        );
        if diag_sl {
            match &enum_res {
                Err(e) => eprintln!("[SL-LOADER-DBG] getResources Err: {e:?}"),
                Ok(None) => eprintln!("[SL-LOADER-DBG] getResources -> Ok(None)"),
                Ok(Some(Value::Object(None))) => {
                    eprintln!("[SL-LOADER-DBG] getResources -> Ok(null)")
                }
                Ok(Some(v)) => eprintln!("[SL-LOADER-DBG] getResources -> Ok(Some({v:?}))"),
            }
        }
        let found_via_enum = if let Ok(Some(Value::Object(Some(mut enum_r)))) = enum_res {
            let enum_pin = ctx.pin_native_root(enum_r);
            let mut count = 0usize;
            loop {
                enum_r = ctx.read_native_pin(enum_pin, enum_r);
                let has_more = ctx.invoke_virtual(enum_r, "hasMoreElements", "()Z", &[]);
                if diag_sl {
                    eprintln!("[SL-LOADER-DBG] hasMoreElements -> {has_more:?}");
                }
                match has_more {
                    Ok(Some(Value::Int(1))) => {}
                    _ => break,
                }
                enum_r = ctx.read_native_pin(enum_pin, enum_r);
                let url_r =
                    match ctx.invoke_virtual(enum_r, "nextElement", "()Ljava/lang/Object;", &[]) {
                        Ok(Some(Value::Object(Some(r)))) => r,
                        other => {
                            if diag_sl {
                                eprintln!("[SL-LOADER-DBG] nextElement -> {other:?}");
                            }
                            break;
                        }
                    };
                // Get URL string and extract the JAR-entry path so we can
                // read bytes via the Rust classpath walker, avoiding the
                // JDK URL.openStream / InputStream chain.
                //   jar:file:/...outer.jar!/entry/path -> entry/path
                //   file:/path/to/file                -> path/to/file
                //   classpath:entry/path              -> entry/path
                let ext_str = match ctx.invoke_virtual(
                    url_r,
                    "toExternalForm",
                    "()Ljava/lang/String;",
                    &[],
                ) {
                    Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                    other => {
                        if diag_sl {
                            eprintln!("[SL-LOADER-DBG] toExternalForm -> {other:?}");
                        }
                        continue;
                    }
                };
                if diag_sl {
                    eprintln!("[SL-LOADER-DBG] URL: {ext_str}");
                }
                let entry_path = if let Some(pos) = ext_str.find("!/") {
                    ext_str[pos + 2..].to_string()
                } else if let Some(rest) = ext_str.strip_prefix("file:///") {
                    rest.to_string()
                } else if let Some(rest) = ext_str.strip_prefix("file://") {
                    rest.to_string()
                } else if let Some(rest) = ext_str.strip_prefix("file:/") {
                    rest.to_string()
                } else if let Some(rest) = ext_str.strip_prefix("classpath:") {
                    rest.to_string()
                } else {
                    ext_str.clone()
                };
                if !entry_path.is_empty() {
                    let mut got = false;
                    // A plain `file:` URL (no `!/` jar separator) names a real
                    if let Some(bytes) = read_jar_url_entry(&ext_str) {
                        parse_provider_lines(&bytes, &mut providers);
                        got = true;
                    }
                    // filesystem path, NOT a classpath-relative resource. The
                    // `find_*_resource_bytes` helpers only search the classpath, so
                    // an absolute path like `C:/…/META-INF/services/<spi>` misses
                    // and the provider list comes back empty. Read the file
                    // directly. This is the common case for a custom loader whose
                    // `getResources` override hands back a descriptor URL from a
                    // directory on disk (Hibernate's `ClassLoaderServiceImplTest`
                    // `TestClassLoader`, HHH-8363).
                    if ext_str.starts_with("file:") && !ext_str.contains("!/") {
                        // Keep the path component from the URL, not the
                        // classpath-relative `entry_path`: stripping `file:/`
                        // from `file:/tmp/...` loses its leading slash and
                        // changes an absolute dynamic-test resource into a
                        // relative path. Spring Boot's ResourcesClassLoader
                        // deliberately exposes method-scoped SPI descriptors
                        // this way.
                        let fs_path = percent_decode(
                            &file_url_path_to_fs_path(&ext_str).unwrap_or(entry_path.clone()),
                        );
                        if let Ok(bytes) = std::fs::read(&fs_path) {
                            parse_provider_lines(&bytes, &mut providers);
                            got = true;
                        }
                    }
                    // Reading the named URL failed, so fall back to resolving its
                    // entry path against the classpath. For an embedded/synthetic
                    // loader that is the only way in. For a URLClassLoader whose
                    // own URLs are known this is a process-wide union under a
                    // loader-scoped name — the same exclusion leak as the flat
                    // scan below — and its URLs are ordinary `jar:`/`file:` ones
                    // the two readers above already handle, so there is nothing
                    // here for it to recover.
                    if !got && !loader_view_is_exhaustive {
                        let bytes_list = ctx.find_all_resource_bytes(&entry_path);
                        for bytes in &bytes_list {
                            parse_provider_lines(bytes, &mut providers);
                        }
                        if bytes_list.is_empty() {
                            if let Some(bytes) = ctx.find_resource(&entry_path) {
                                parse_provider_lines(&bytes, &mut providers);
                            }
                        }
                    }
                    count += 1;
                }
            }
            ctx.unpin_native_roots(enum_pin);
            count > 0
        } else {
            false
        };

        // Fallback: if getResources failed or returned an empty Enumeration,
        // directly read the `jarMetas` field from the loader (an
        // EmbeddedImplClassLoader-like object) and call prefix() on each
        // JarMeta to construct the embedded resource path.  This bypasses the
        // URL/ClassLoader/invokedynamic chain entirely.
        if !found_via_enum {
            let loader_r = ctx.read_native_pin(loader_pin, loader_r);
            if let Value::Object(Some(jm_list_r)) = ctx.get_field_by_name(loader_r, "jarMetas") {
                let size = match ctx.invoke(
                    "java/util/List",
                    "size",
                    "()I",
                    &[Value::Object(Some(jm_list_r))],
                ) {
                    Ok(Some(Value::Int(n))) => n,
                    _ => 0,
                };
                if diag_sl {
                    eprintln!("[SL-LOADER-DBG] jarMetas.size() = {size}");
                }
                for i in 0..size {
                    let jm = match ctx.invoke(
                        "java/util/List",
                        "get",
                        "(I)Ljava/lang/Object;",
                        &[Value::Object(Some(jm_list_r)), Value::Int(i)],
                    ) {
                        Ok(Some(Value::Object(Some(r)))) => r,
                        _ => continue,
                    };
                    let prefix_str =
                        match ctx.invoke_virtual(jm, "prefix", "()Ljava/lang/String;", &[]) {
                            Ok(Some(Value::Object(Some(s)))) => {
                                ctx.read_string(s).unwrap_or_default()
                            }
                            _ => continue,
                        };
                    if prefix_str.is_empty() {
                        continue;
                    }
                    let prefixed = format!("{}/{}", prefix_str, resource);
                    if diag_sl {
                        eprintln!("[SL-LOADER-DBG] jarMeta prefix path: {prefixed}");
                    }
                    let bytes_list = ctx.find_all_resource_bytes(&prefixed);
                    for bytes in &bytes_list {
                        parse_provider_lines(bytes, &mut providers);
                    }
                    if bytes_list.is_empty() {
                        if let Some(bytes) = ctx.find_resource(&prefixed) {
                            parse_provider_lines(&bytes, &mut providers);
                        }
                    }
                }
            } else if diag_sl {
                eprintln!("[SL-LOADER-DBG] no jarMetas field found on loader");
            }
        }
        // IMPL-JARS fallback: runs whenever providers is still empty after the
        // getResources / jarMetas attempts. Covers:
        //   (a) getResources returned null/empty (found_via_enum=false, the common case
        //       for EmbeddedImplClassLoader when jarMetas is empty), and
        //   (b) getResources succeeded but the URL chain produced no bytes.
        //
        // Derive the module name from the service FQN, read LISTING.TXT via flat
        // classpath, then open each inner JAR as ZIP and look for the service
        // descriptor — bypassing the broken BufferedReader.lines().toList() path.
        if providers.is_empty() {
            let module_candidates = derive_impl_jar_module_names(&service_name);
            if diag_sl {
                eprintln!(
                    "[SL-LOADER-DBG] IMPL-JARS fallback module_candidates={module_candidates:?}"
                );
            }
            'modules: for module_name in &module_candidates {
                let listing_path = format!("IMPL-JARS/{module_name}/LISTING.TXT");
                if diag_sl {
                    eprintln!("[SL-LOADER-DBG] reading LISTING.TXT: {listing_path}");
                }
                let first_bytes = ctx
                    .find_all_resource_bytes(&listing_path)
                    .into_iter()
                    .next();
                let listing_bytes = match first_bytes.or_else(|| ctx.find_resource(&listing_path)) {
                    Some(b) => b,
                    None => continue,
                };
                if diag_sl {
                    eprintln!(
                        "[SL-LOADER-DBG] LISTING.TXT ({} bytes): {:?}",
                        listing_bytes.len(),
                        String::from_utf8_lossy(&listing_bytes)
                    );
                }
                let listing_text = String::from_utf8_lossy(&listing_bytes).into_owned();
                for jar_name in listing_text
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && l.ends_with(".jar"))
                {
                    if let Some(bytes) =
                        try_read_from_inner_jar(ctx, module_name, jar_name, &resource)
                    {
                        if diag_sl {
                            eprintln!(
                                "[SL-LOADER-DBG] found svc descriptor in {module_name}/{jar_name}"
                            );
                        }
                        parse_provider_lines(&bytes, &mut providers);
                    }
                }
                if !providers.is_empty() {
                    break 'modules;
                }
            }
        }
        ctx.unpin_native_roots(loader_pin);
    }

    // Flat classpath scan: providers listed directly at
    // META-INF/services/<svc> on the classpath (normal case for
    // non-embedded loaders and JDK built-in providers).
    //
    // Skipped when the loader already answered exhaustively for itself — see
    // `loader_view_is_exhaustive`. Note this is NOT gated on the loader-scoped
    // pass having FOUND anything: an empty authoritative answer is the whole
    // point, and supplementing it here is what leaked an excluded jar's
    // registration back in.
    let skip_flat_scan = loader_is_jboss_module || loader_view_is_exhaustive;
    let descriptors = if skip_flat_scan {
        Vec::new()
    } else {
        ctx.find_all_resource_bytes(&resource)
    };
    for bytes in &descriptors {
        parse_provider_lines(bytes, &mut providers);
    }
    // Test mocks may stub `find_resource` without populating the bytes list.
    if descriptors.is_empty() && !skip_flat_scan {
        if let Some(bytes) = ctx.find_resource(&resource) {
            parse_provider_lines(&bytes, &mut providers);
        }
    }

    // JPMS module `provides` declarations. The JDK declares service providers
    // in module-info — e.g. jdk.compiler has
    // `provides javax.tools.JavaCompiler with com.sun.tools.javac.api.JavacTool`
    // — NOT in META-INF/services. Without this source,
    // `ServiceLoader.load(JavaCompiler.class)` (the body of
    // `ToolProvider.getSystemJavaCompiler()`) finds nothing and returns null,
    // so the in-process javac is unavailable. `service_providers_from_modules`
    // takes the service name in binary/slash form and returns provider impl
    // names in slash form; normalise both to dot form to match the
    // META-INF/services FQNs. Providers that fail to load/instantiate are
    // skipped by the iterator, so a module-declared provider CratonVM cannot
    // construct is harmless.
    //
    // `service_providers_from_modules` reads ONE VM-global module registry, with
    // no notion of which loader is asking — the third flavour of the same
    // exclusion leak. It is also not what a real JVM does here: a modular jar
    // reached through the CLASS path is an unnamed-module citizen whose
    // `module-info` the JDK ignores outright, so `ServiceLoader` sees only its
    // `META-INF/services`. `logback-classic.jar` is exactly that — it declares
    // `provides SLF4JServiceProvider with LogbackServiceProvider`, and CratonVM
    // was handing that declaration to a loader built to exclude the jar.
    //
    // A loader with its own recorded URL list IS a class-path loader, so skip
    // the module source for it. Every other caller — notably the null/builtin
    // loader behind `ToolProvider.getSystemJavaCompiler()`, the case this source
    // exists for — is untouched.
    if !loader_view_is_exhaustive {
        let service_slash = service_name.replace('.', "/");
        for mp in ctx.service_providers_from_modules(&service_slash) {
            providers.push(mp.replace('/', "."));
        }
    }

    providers.sort();
    providers.dedup();
    if crate::nbflags().diag_serviceloader {
        eprintln!(
            "[SL-DBG] ServiceLoader service={} loader_delegation={} descriptors={} providers={} ({:?})",
            service_name,
            loader_ref_opt.is_some(),
            descriptors.len(),
            providers.len(),
            providers
        );
    }
    ctx.unpin_native_roots(sl_pin);
    Ok(providers)
}

/// Tokenize a `META-INF/services/<spi>` descriptor. Each non-comment,
/// non-blank line is a provider FQN; everything after `#` on a line is
/// a comment. Mirrors `is_valid_provider_name`'s validation so callers
/// that reuse the parsed list don't need to re-filter.
fn parse_provider_lines(bytes: &[u8], out: &mut Vec<String>) {
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return,
    };
    for raw in text.lines() {
        let token = raw.split('#').next().unwrap_or("").trim();
        if !token.is_empty() && is_valid_provider_name(token) {
            out.push(token.to_string());
        }
    }
}

/// Percent-decode a URL path component (`%20` → space, etc.) so a `file:` URL
/// derived path opens as a real filesystem path. Leaves malformed `%` escapes
/// and non-`%` bytes untouched. Kept intentionally small — descriptor URLs only
/// need the handful of escapes the JDK's `URLEncoder`/`sun.net.www` layer emits.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn file_url_path_to_fs_path(file_url: &str) -> Option<String> {
    let raw = if let Some(rest) = file_url.strip_prefix("file://") {
        rest
    } else if let Some(rest) = file_url.strip_prefix("file:/") {
        rest
    } else {
        return None;
    };
    if raw.is_empty() {
        return None;
    }
    let decoded = percent_decode(raw);
    if decoded.starts_with('/') || decoded.as_bytes().get(1).copied() == Some(b':') {
        Some(decoded)
    } else {
        Some(format!("/{decoded}"))
    }
}

fn read_jar_url_entry(ext_str: &str) -> Option<Vec<u8>> {
    let rest = ext_str.strip_prefix("jar:")?;
    let (jar_url, entry_path) = rest.split_once("!/")?;
    let jar_path = file_url_path_to_fs_path(jar_url)?;
    let file = std::fs::File::open(jar_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut entry = archive.by_name(entry_path).ok()?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    use std::io::Read;
    entry.read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn is_valid_provider_name(s: &str) -> bool {
    // JDK ServiceLoader spec: each line must be a fully-qualified
    // binary class name containing only Java identifier chars, `.`, and `$`.
    if s.is_empty() {
        return false;
    }
    s.chars()
        .all(|c| c.is_alphanumeric() || c == '.' || c == '_' || c == '$')
}

/// `ServiceLoader.iterator()` — scan META-INF/services and return an
/// Iterator<Object> over instantiated providers.
/// JDK `ServiceLoader` wraps a provider constructor failure in
/// `ServiceConfigurationError` instead of silently omitting that provider.
fn provider_construction_error(
    ctx: &mut dyn NativeContext,
    provider: &str,
    failure: MethodCallFailed,
) -> MethodCallFailed {
    let cause = match failure {
        MethodCallFailed::ExceptionThrown(cause) => cause,
        other => return other,
    };
    // `Constructor.newInstance` correctly reports the provider's throw as an
    // InvocationTargetException. ServiceLoader's contract exposes its target
    // as the ServiceConfigurationError cause instead.
    let original_cause_pin = ctx.pin_native_root(cause);
    let unwrapped = ctx.invoke(
        "java/lang/Throwable",
        "getCause",
        "()Ljava/lang/Throwable;",
        &[Value::Object(Some(cause))],
    );
    let cause = match unwrapped {
        Ok(Some(Value::Object(Some(target)))) => target,
        _ => ctx.read_native_pin(original_cause_pin, cause),
    };
    let cause_pin = ctx.pin_native_root(cause);
    let message = ctx.create_string(&format!("Provider {provider} could not be instantiated"));
    let message_pin = ctx.pin_native_root(message);
    let cause = ctx.read_native_pin(cause_pin, cause);
    let message = ctx.read_native_pin(message_pin, message);
    let wrapped = ctx.new_object_initialized(
        "java/util/ServiceConfigurationError",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        &[Value::Object(Some(message)), Value::Object(Some(cause))],
    );
    // Refresh the fallback before releasing the one LIFO scope rooted at the
    // original cause. Truncating `original_cause_pin` immediately after taking
    // `cause_pin` used to discard the latter and made the fallback stale.
    let cause = ctx.read_native_pin(cause_pin, cause);
    ctx.unpin_native_roots(original_cause_pin);
    match wrapped {
        Ok(Some(Value::Object(Some(error)))) => MethodCallFailed::ExceptionThrown(error),
        _ => MethodCallFailed::ExceptionThrown(cause),
    }
}

/// The service's binary name, for `ServiceConfigurationError` messages.
///
/// `serviceName` is a CratonVM-only field: `initialize_real_service_loader_fields`
/// writes it, but a real JDK 25 `ServiceLoader` does not declare it, so the write
/// is a no-op there and the read answers `Int(0)`. Fall back to the `service`
/// mirror (named field, then the legacy slot 0 the synthetic layout uses) and ask
/// it for its name, exactly as `discover_providers` does.
fn sl_service_name(ctx: &mut dyn NativeContext, sl: cratonvm_types::ObjectRef) -> String {
    if let Value::Object(Some(name_obj)) = ctx.get_field_by_name(sl, "serviceName") {
        let name = ctx.read_string(name_obj).unwrap_or_default();
        if !name.is_empty() {
            return name;
        }
    }
    let service = match ctx.get_field_by_name(sl, "service") {
        Value::Object(Some(c)) => c,
        _ => match ctx.get_field(sl, 0) {
            Value::Object(Some(c)) => c,
            _ => return String::new(),
        },
    };
    let service_pin = ctx.pin_native_root(service);
    let service = ctx.read_native_pin(service_pin, service);
    let name = match ctx.invoke(
        "java/lang/Class",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(service))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    ctx.unpin_native_roots(service_pin);
    name
}

/// The JDK's `fail(service, "Provider " + cn + " not found")`: a
/// `ServiceConfigurationError` with a message and NO cause, which is what
/// `LazyClassPathLookupIterator.nextProviderClass` raises when a descriptor
/// names a class the loader cannot resolve.
///
/// `None` when the error object itself cannot be built — a `ServiceLoader`
/// running on a class library that has no usable `ServiceConfigurationError`
/// must keep this file's historical behaviour (an empty iterator) rather than
/// gain a brand-new, uncatchable internal failure.
fn provider_not_found_error(
    ctx: &mut dyn NativeContext,
    service_name: &str,
    provider: &str,
) -> Option<MethodCallFailed> {
    service_configuration_error(ctx, &format!("{service_name}: Provider {provider} not found"))
}

/// `ServiceLoader.fail(service, msg)`: a cause-less `ServiceConfigurationError`.
///
/// `None` when the error object itself cannot be built — see
/// `provider_not_found_error`, whose historic contract this preserves.
fn service_configuration_error(
    ctx: &mut dyn NativeContext,
    text: &str,
) -> Option<MethodCallFailed> {
    let message = ctx.create_string(text);
    let message_pin = ctx.pin_native_root(message);
    let message = ctx.read_native_pin(message_pin, message);
    let built = ctx.new_object_initialized(
        "java/util/ServiceConfigurationError",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(message))],
    );
    ctx.unpin_native_roots(message_pin);
    match built {
        Ok(Some(Value::Object(Some(error)))) => Some(MethodCallFailed::ExceptionThrown(error)),
        _ => None,
    }
}

/// The `service` mirror of a `ServiceLoader`, named field first and the legacy
/// synthetic slot 0 second — the same two-step `discover_providers` uses.
fn sl_service_mirror(
    ctx: &mut dyn NativeContext,
    sl: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    match ctx.get_field_by_name(sl, "service") {
        Value::Object(Some(c)) => Some(c),
        _ => match ctx.get_field(sl, 0) {
            Value::Object(Some(c)) => Some(c),
            _ => None,
        },
    }
}

/// The provider FQNs a JPMS `module-info` `provides` clause declared for this
/// service, in dot form — i.e. the subset of `discover_providers`' result that
/// came from the module registry rather than a `META-INF/services` descriptor.
///
/// This is the `clazz.getModule().isNamed()` test of `ServiceLoader
/// .loadProvider`, asked of the *descriptor* instead of the loaded class: a
/// provider named by a module descriptor is by construction in a named module,
/// and answering from the registry costs no Java dispatch on the hot classpath
/// path (Spring/Tomcat boot walks hundreds of providers per run, none of which
/// are module-declared).
fn module_declared_providers(ctx: &mut dyn NativeContext, service_name: &str) -> Vec<String> {
    if service_name.is_empty() {
        return Vec::new();
    }
    ctx.service_providers_from_modules(&service_name.replace('.', "/"))
        .into_iter()
        .map(|s| s.replace('/', "."))
        .collect()
}

/// Grant the `setAccessible(true)` override on a `Method`/`Constructor`
/// WITHOUT calling `AccessibleObject.setAccessible`.
///
/// `setAccessible` is caller-sensitive, and this file is a Rust native that
/// pushes no `java.util.ServiceLoader` frame: `resolve_caller_class_id`
/// attributes the call to the *application* class that asked for the
/// `ServiceLoader`, so the wave-4 JEP 403 gate correctly refuses to open a
/// provider that lives in a package its module neither exports nor opens
/// (`com.cratonvm.jdkonly.svc.internal` in `regression-suite/modules/`). The
/// real JDK never meets that refusal because its `ServiceLoader` *is* java.base
/// and takes the JDK-internal bypass. The `setAccessible` return value was
/// being discarded here, so the refusal was invisible and the following
/// `newInstance` / `Method.invoke` was refused in turn, leaving zero providers.
///
/// Writing the flag is what `getConstructor`'s `if (inExplicitModule(clazz))
/// ctor.setAccessible(true)` and `findStaticProviderMethod`'s
/// `m.setAccessible(true)` achieve in the JDK; only the caller identity differs.
/// Both fields are written because a reflective object built with positional
/// slots (synthetic-jdk layout) has no named `override` at all — the same
/// belt-and-braces pair `lang_class::native_set_accessible` writes.
fn grant_reflective_override(ctx: &mut dyn NativeContext, obj: cratonvm_types::ObjectRef) {
    ctx.set_field_by_name(obj, "override", Value::Int(1));
    let cid = ctx.class_id_of_object(obj);
    let cname = ctx.class_name_of_id(cid);
    match cname.as_deref() {
        Some("java/lang/reflect/Constructor") => {
            crate::lang_class::write_constructor_accessible(ctx, obj, true);
        }
        Some("java/lang/reflect/Method") => {
            crate::lang_class::write_method_accessible(ctx, obj, true);
        }
        _ => {}
    }
}

/// `ServiceLoader.findStaticProviderMethod` — the JPMS `provider()` static
/// factory form.
///
/// A provider class in a NAMED module may expose `public static <S> provider()`
/// INSTEAD of a public no-arg constructor, and then need not implement the
/// service type at all. This file only ever did `getDeclaredConstructor()` +
/// `newInstance`, so such a provider was at best skipped and at worst
/// constructed through its private constructor and handed out as an instance
/// that is not of the service type (`FactoryGreeter` in
/// `regression-suite/modules/cratonvm.jdkonly.svc` is exactly that shape:
/// private constructor, implements nothing, reachable only via `provider()`).
///
/// Returns the `Method` when the class declares the JDK's exact shape
/// (declared, public, static, no-arg, named `provider`). The returned reference
/// is UNPINNED — pin it before the next allocation, like `load_provider_class`.
fn provider_factory_method(
    ctx: &mut dyn NativeContext,
    type_class: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    const ACC_PUBLIC: i32 = 0x0001;
    const ACC_STATIC: i32 = 0x0008;
    let type_pin = ctx.pin_native_root(type_class);
    let name = ctx.create_string("provider");
    let name_pin = ctx.pin_native_root(name);
    let empty_types = ctx.new_ref_array(
        ctx.class_id_by_name("java/lang/Class")
            .unwrap_or(cratonvm_types::ClassId::new(0)),
        0,
    );
    let empty_types_pin = ctx.pin_native_root(empty_types);
    let type_now = ctx.read_native_pin(type_pin, type_class);
    let name_now = ctx.read_native_pin(name_pin, name);
    let empty_now = ctx.read_native_pin(empty_types_pin, empty_types);
    // A `NoSuchMethodException` here is the ORDINARY constructor-form provider,
    // not an error: the JDK's `getDeclaredPublicMethods(clazz, "provider")`
    // simply answers an empty list and `findStaticProviderMethod` returns null.
    let found = ctx.invoke(
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &[
            Value::Object(Some(type_now)),
            Value::Object(Some(name_now)),
            Value::Object(Some(empty_now)),
        ],
    );
    ctx.unpin_native_roots(type_pin);
    let method = match found {
        Ok(Some(Value::Object(Some(m)))) => m,
        _ => return None,
    };
    let method_pin = ctx.pin_native_root(method);
    let method_now = ctx.read_native_pin(method_pin, method);
    let mods = match ctx.invoke(
        "java/lang/reflect/Method",
        "getModifiers",
        "()I",
        &[Value::Object(Some(method_now))],
    ) {
        Ok(Some(Value::Int(m))) => m,
        _ => 0,
    };
    let method = ctx.read_native_pin(method_pin, method);
    ctx.unpin_native_roots(method_pin);
    // `getDeclaredPublicMethods` pre-filters on public; a non-public or
    // non-static `provider()` is NOT the factory form and must fall through to
    // the constructor path rather than be invoked.
    if (mods & ACC_PUBLIC) == 0 || (mods & ACC_STATIC) == 0 {
        return None;
    }
    Some(method)
}

/// `factoryMethod.getReturnType()` — the class `ServiceLoader$ProviderImpl`
/// stores as `type`, i.e. what `Provider.type()` must answer for a factory
/// provider (`Greeter`, not `FactoryGreeter`). Returned reference is unpinned.
fn factory_return_type(
    ctx: &mut dyn NativeContext,
    method: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    let method_pin = ctx.pin_native_root(method);
    let method_now = ctx.read_native_pin(method_pin, method);
    let ret = ctx.invoke(
        "java/lang/reflect/Method",
        "getReturnType",
        "()Ljava/lang/Class;",
        &[Value::Object(Some(method_now))],
    );
    ctx.unpin_native_roots(method_pin);
    match ret {
        Ok(Some(Value::Object(Some(c)))) => Some(c),
        _ => None,
    }
}

/// `service.isAssignableFrom(candidate)`. An unreadable answer is reported as
/// `true` so a mirror this VM cannot interrogate keeps the historic behaviour
/// (accept the provider) instead of gaining a brand-new refusal.
fn service_accepts_type(
    ctx: &mut dyn NativeContext,
    service: cratonvm_types::ObjectRef,
    candidate: cratonvm_types::ObjectRef,
) -> bool {
    let service_pin = ctx.pin_native_root(service);
    let candidate_pin = ctx.pin_native_root(candidate);
    let service_now = ctx.read_native_pin(service_pin, service);
    let candidate_now = ctx.read_native_pin(candidate_pin, candidate);
    let answer = ctx.invoke(
        "java/lang/Class",
        "isAssignableFrom",
        "(Ljava/lang/Class;)Z",
        &[
            Value::Object(Some(service_now)),
            Value::Object(Some(candidate_now)),
        ],
    );
    ctx.unpin_native_roots(service_pin);
    match answer {
        Ok(Some(Value::Int(v))) => v != 0,
        _ => true,
    }
}

/// `factoryMethod.toString()` — the exact text `ServiceLoader.fail(service,
/// factoryMethod + " return type not a subtype")` interpolates, e.g.
/// `public static java.lang.Object com.example.Bad.provider()`.
///
/// Measured against HotSpot 25.0.3.9 rather than inferred; the whole message is
/// asserted verbatim by `regression-suite/src/RJdkModule.java`'s `Rejected`
/// service, so a drift here is a red vector rather than a silent divergence.
///
/// Falls back to `<fqn>.provider()` when `Method.toString()` cannot be driven.
/// That keeps the one part a reader has to have — which provider was refused —
/// without inventing modifiers or a return type this VM did not actually read.
fn factory_method_display(
    ctx: &mut dyn NativeContext,
    method: cratonvm_types::ObjectRef,
    fqn: &str,
) -> String {
    let method_pin = ctx.pin_native_root(method);
    let method_now = ctx.read_native_pin(method_pin, method);
    let rendered = match ctx.invoke(
        "java/lang/reflect/Method",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(method_now))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    ctx.unpin_native_roots(method_pin);
    if rendered.is_empty() {
        format!("{fqn}.provider()")
    } else {
        rendered
    }
}

/// The verdict of `ServiceLoader.loadProvider`'s factory-form subtype gate.
enum FactoryReturn {
    /// A legal `provider()`. Carries the return-type mirror, which is what
    /// `ProviderImpl` records as `type` and what `Provider.type()` answers.
    /// The reference is UNPINNED — pin it before the next allocation, the same
    /// contract `factory_return_type` and `load_provider_class` carry.
    Accepted(cratonvm_types::ObjectRef),
    /// `getReturnType()` could not be read at all. Historic behaviour is kept
    /// (treat the provider as legal) for the same reason `service_accepts_type`
    /// answers `true` on an unreadable `isAssignableFrom`: an interrogation
    /// this VM cannot drive must never MANUFACTURE a refusal.
    Unreadable,
    /// Not a subtype of the service. Carries the `ServiceConfigurationError` to
    /// raise, or `None` when the error object itself could not be built — the
    /// contract `service_configuration_error` has always had, where the caller
    /// drops the provider rather than pretending to have thrown.
    Rejected(Option<MethodCallFailed>),
}

/// `ServiceLoader.loadProvider`'s factory-form gate, shared by BOTH provider
/// paths:
///
/// ```text
/// Class<?> returnType = factoryMethod.getReturnType();
/// if (!service.isAssignableFrom(returnType))
///     fail(service, factoryMethod + " return type not a subtype");
/// ```
///
/// It is a function because it has to run in `native_sl_iterator` AND
/// `native_sl_stream`. It was written inline on the iterator only, and the
/// `stream()` path computed the return type without ever asking: an illegal
/// module-declared factory raised from `iterator()` and was handed out by
/// `stream()` as a `Provider` whose `get()` returns an object of the wrong type
/// (measured: a `String` for a service interface). Nothing downstream catches
/// that — `ProviderImpl.invokeFactoryMethod`'s `(S)` cast is erased — so this
/// gate is the only gate there is. See W7-85-serviceloader-stream-validation.md.
///
/// PIN ORDER, which is the delicate part: every step allocates and re-enters
/// Java, so no `ObjectRef` may be held raw across a call.
///
///  * `factory` is re-read through the caller's `factory_pin` before each use;
///  * the service mirror is fetched with the NON-allocating `sl_service_mirror`
///    AFTER `factory_return_type` has returned, never held across it;
///  * the return mirror takes its own pin for the duration of
///    `isAssignableFrom` and is read back through that pin before the pin
///    drops, so `Accepted` names the forwarded address, not the pre-GC one;
///  * `Method.toString()` and `sl_service_name` are driven while the caller's
///    pins are still standing, i.e. before any of them is released.
///
/// The caller's `sl_pin` and `factory_pin` are left exactly as they were found.
fn factory_return_is_subtype(
    ctx: &mut dyn NativeContext,
    sl_pin: usize,
    sl: cratonvm_types::ObjectRef,
    factory_pin: usize,
    factory: cratonvm_types::ObjectRef,
    fqn: &str,
) -> FactoryReturn {
    let factory_now = ctx.read_native_pin(factory_pin, factory);
    let ret = match factory_return_type(ctx, factory_now) {
        Some(ret) => ret,
        None => return FactoryReturn::Unreadable,
    };
    let ret_pin = ctx.pin_native_root(ret);
    let sl_now = ctx.read_native_pin(sl_pin, sl);
    let ret_now = ctx.read_native_pin(ret_pin, ret);
    let accepted = match sl_service_mirror(ctx, sl_now) {
        Some(service) => service_accepts_type(ctx, service, ret_now),
        // No readable service mirror: there is nothing to compare against, so
        // keep the historic accept rather than refuse on an unasked question.
        None => true,
    };
    if accepted {
        let ret_now = ctx.read_native_pin(ret_pin, ret);
        ctx.unpin_native_roots(ret_pin);
        return FactoryReturn::Accepted(ret_now);
    }
    let factory_now = ctx.read_native_pin(factory_pin, factory);
    let rendered = factory_method_display(ctx, factory_now, fqn);
    let sl_now = ctx.read_native_pin(sl_pin, sl);
    let service_name = sl_service_name(ctx, sl_now);
    ctx.unpin_native_roots(ret_pin);
    FactoryReturn::Rejected(service_configuration_error(
        ctx,
        &format!("{service_name}: {rendered} return type not a subtype"),
    ))
}

/// `String.valueOf(clazz)` — the exact interpolation `ServiceLoader
/// .loadProvider` performs in `fail(service, clazz + " not a subtype")`.
///
/// `Class.toString()` renders `class com.foo.Bar` / `interface com.foo.Bar`,
/// NOT the bare binary name: the CLASSPATH iterator spells the same refusal
/// `clazz.getName() + " not a subtype"`, so the JDK's two provider paths really
/// do print different text for one rule. This helper follows `loadProvider`,
/// which is the path being mirrored here.
///
/// Falls back to the FQN when `Class.toString()` cannot be driven — the one
/// part a reader has to have is *which* provider was refused, and inventing a
/// `class `/`interface ` prefix this VM did not read would be a fabrication.
fn provider_class_display(
    ctx: &mut dyn NativeContext,
    class: cratonvm_types::ObjectRef,
    fqn: &str,
) -> String {
    let class_pin = ctx.pin_native_root(class);
    let class_now = ctx.read_native_pin(class_pin, class);
    let rendered = match ctx.invoke(
        "java/lang/Class",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(class_now))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    ctx.unpin_native_roots(class_pin);
    if rendered.is_empty() {
        fqn.to_string()
    } else {
        rendered
    }
}

/// The verdict of `ServiceLoader.loadProvider`'s CONSTRUCTOR-form gates — the
/// two rules that apply once `findStaticProviderMethod` has answered `null`.
///
/// Two states, not [`FactoryReturn`]'s three, and deliberately so: an
/// interrogation this VM cannot drive is `Accepted`, folded into the same
/// variant as a genuine pass. That is the same refusal-of-a-refusal
/// `service_accepts_type` makes when `isAssignableFrom` is unreadable — **a
/// widening into a throw must never fire on a question that went unanswered.**
enum ConstructorForm {
    /// Legal — or unanswerable, which is treated as legal.
    Accepted,
    /// One of the two rules is positively broken. `None` when the
    /// `ServiceConfigurationError` itself could not be built; the caller then
    /// drops the provider rather than pretending to have thrown, which is the
    /// contract `service_configuration_error` has always carried.
    Rejected(Option<MethodCallFailed>),
}

/// `ServiceLoader.loadProvider`'s constructor-form subtype gate:
///
/// ```text
/// // no factory method so must be a subtype
/// if (!service.isAssignableFrom(clazz))
///     fail(service, clazz + " not a subtype");
/// ```
///
/// Absent on BOTH provider paths until now — `W6-2`'s last live row, and the
/// `none / none` row `W7-85`'s population sweep confirmed independently. It
/// lands on both paths in one change for the reason that record exists: a guard
/// installed on one of two siblings is validated by whichever fixture walks the
/// other one, and reads green forever.
///
/// PIN ORDER: identical in shape to [`factory_return_is_subtype`]. `sl` and
/// `class` are re-read through the caller's pins before *each* use, the service
/// mirror is fetched with the non-allocating [`sl_service_mirror`] and handed
/// straight to `service_accepts_type` (which takes its own pins), and the
/// message is rendered while the caller's pins still stand. The caller's pins
/// are left exactly as they were found.
fn constructor_form_is_subtype(
    ctx: &mut dyn NativeContext,
    sl_pin: usize,
    sl: cratonvm_types::ObjectRef,
    class_pin: usize,
    class: cratonvm_types::ObjectRef,
    fqn: &str,
) -> ConstructorForm {
    let sl_now = ctx.read_native_pin(sl_pin, sl);
    let service = match sl_service_mirror(ctx, sl_now) {
        Some(service) => service,
        // No readable service mirror: there is nothing to compare against, so
        // keep the historic accept rather than refuse on an unasked question.
        None => return ConstructorForm::Accepted,
    };
    let class_now = ctx.read_native_pin(class_pin, class);
    if service_accepts_type(ctx, service, class_now) {
        return ConstructorForm::Accepted;
    }
    let class_now = ctx.read_native_pin(class_pin, class);
    let rendered = provider_class_display(ctx, class_now, fqn);
    let sl_now = ctx.read_native_pin(sl_pin, sl);
    let service_name = sl_service_name(ctx, sl_now);
    ConstructorForm::Rejected(service_configuration_error(
        ctx,
        &format!("{service_name}: {rendered} not a subtype"),
    ))
}

/// `Class.getConstructor()` searches **public** members only; this file asks
/// `getDeclaredConstructor()`. So a provider whose no-arg constructor is
/// private or package-private was found here, opened by
/// [`grant_reflective_override`], and handed out — where `loadProvider` refuses
/// it.
///
/// Answers `true` on an unreadable `getModifiers()`, for [`ConstructorForm`]'s
/// stated reason.
fn constructor_is_public(
    ctx: &mut dyn NativeContext,
    ctor_pin: usize,
    ctor: cratonvm_types::ObjectRef,
) -> bool {
    const ACC_PUBLIC: i32 = 0x0001;
    let ctor_now = ctx.read_native_pin(ctor_pin, ctor);
    match ctx.invoke(
        "java/lang/reflect/Constructor",
        "getModifiers",
        "()I",
        &[Value::Object(Some(ctor_now))],
    ) {
        Ok(Some(Value::Int(mods))) => (mods & ACC_PUBLIC) != 0,
        _ => true,
    }
}

/// `ServiceLoader.getConstructor`'s failure:
///
/// ```text
/// try { ctor = clazz.getConstructor(); }
/// catch (Throwable x) {
///     fail(service, cn + " Unable to get public no-arg constructor", x);
/// }
/// ```
///
/// Both illegal shapes arrive here — *no* no-arg constructor and a *non-public*
/// one — because `getConstructor()` cannot see either, and both arrive carrying
/// the `NoSuchMethodException` it throws, whose message is `<fqn>.<init>()`.
///
/// This is the **three**-argument `fail`, so unlike every other refusal in this
/// file the error carries a CAUSE. Building it is therefore part of the fix, not
/// decoration: `RJdkModule` asserts the cause is present, and a cause-less
/// `ServiceConfigurationError` here would be a second, quieter divergence
/// standing in for the one being closed. If the `NoSuchMethodException` cannot
/// be built the cause-less form is raised anyway — losing the cause is better
/// than losing the refusal.
fn no_public_no_arg_ctor_error(
    ctx: &mut dyn NativeContext,
    sl_pin: usize,
    sl: cratonvm_types::ObjectRef,
    fqn: &str,
) -> Option<MethodCallFailed> {
    let sl_now = ctx.read_native_pin(sl_pin, sl);
    let service_name = sl_service_name(ctx, sl_now);
    let text = format!("{service_name}: {fqn} Unable to get public no-arg constructor");

    let detail = ctx.create_string(&format!("{fqn}.<init>()"));
    let detail_pin = ctx.pin_native_root(detail);
    let detail = ctx.read_native_pin(detail_pin, detail);
    let built_cause = ctx.new_object_initialized(
        "java/lang/NoSuchMethodException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    );
    ctx.unpin_native_roots(detail_pin);
    let cause = match built_cause {
        Ok(Some(Value::Object(Some(c)))) => c,
        _ => return service_configuration_error(ctx, &text),
    };
    let cause_pin = ctx.pin_native_root(cause);
    let message = ctx.create_string(&text);
    let message_pin = ctx.pin_native_root(message);
    let cause = ctx.read_native_pin(cause_pin, cause);
    let message = ctx.read_native_pin(message_pin, message);
    let built = ctx.new_object_initialized(
        "java/util/ServiceConfigurationError",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        &[Value::Object(Some(message)), Value::Object(Some(cause))],
    );
    // Truncate-to-base: releasing `cause_pin` releases `message_pin` with it.
    ctx.unpin_native_roots(cause_pin);
    match built {
        Ok(Some(Value::Object(Some(error)))) => Some(MethodCallFailed::ExceptionThrown(error)),
        _ => None,
    }
}

/// The JDK's own per-loader instance cache: `ServiceLoader.instantiatedProviders`.
///
/// `initialize_real_service_loader_fields` allocates this list and
/// `native_sl_reload` clears it, but until the caching arm of
/// `native_sl_iterator` existed nothing ever read or wrote it — see that
/// function's cache comment for what that cost.
///
/// Absent on a layout that does not declare the field (a fabricated
/// synthetic-JDK `ServiceLoader`), where `get_field_by_name` answers
/// `Int(0)` rather than an object; the caller then falls back to the
/// re-discovering path this file has always taken.
fn sl_instance_cache(
    ctx: &mut dyn NativeContext,
    sl: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    match ctx.get_field_by_name(sl, "instantiatedProviders") {
        Value::Object(Some(list)) => Some(list),
        _ => None,
    }
}

/// `true` once a completed `iterator()` has published every provider into
/// `instantiatedProviders`. Mirrors the real JDK field of the same name:
/// `initialize_real_service_loader_fields` seeds it to 0 and
/// `native_sl_reload` resets it to 0, so `reload()` still discards the cache.
///
/// A layout without the field answers `Int(0)` and therefore never reports a
/// complete cache, which is what keeps this change inert wherever the named
/// fields do not exist.
fn sl_cache_is_complete(ctx: &mut dyn NativeContext, sl: cratonvm_types::ObjectRef) -> bool {
    ctx.get_field_by_name(sl, "loadedAllProviders")
        .as_int()
        .unwrap_or(0)
        != 0
}

fn native_sl_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sl = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `java.util.ServiceLoader` is specified to CACHE: "Instances are cached
    // and instantiated lazily ... Iterating over a ServiceLoader a second time
    // yields the same instances, in the same order" — only `reload()` discards
    // them. This native re-discovered and re-instantiated every provider on
    // every `iterator()` call, so `sl.iterator().next() != sl.iterator().next()`
    // for the SAME loader (regression-suite `RJdkServices.java:121`,
    // "a single ServiceLoader caches its instances" — the check that failed in
    // `--real-jdk` while `--jdk-only`, which refuses this SyntheticStub and runs
    // the real `ServiceLoader` bytecode, passed).
    //
    // The cache the JDK uses for exactly this is `instantiatedProviders`, and
    // this file already allocated it (`initialize_real_service_loader_fields`)
    // and already cleared it on `reload()` (`native_sl_reload`) — it just had
    // no reader and no writer, so `reload()`'s clear was a no-op and the
    // "reload discards the cache" check only passed because there was no cache
    // to discard. The two arms below are that missing reader and writer.
    //
    // `sl` must be rooted for the whole body: the publish step at the bottom
    // writes two of its fields after the instantiation loop's many collections.
    let sl_pin = ctx.pin_native_root(sl);
    if sl_cache_is_complete(ctx, sl) {
        if let Some(cache) = sl_instance_cache(ctx, sl) {
            let cache_pin = ctx.pin_native_root(cache);
            let cache_now = ctx.read_native_pin(cache_pin, cache);
            // A non-empty cache is required, not just the flag. In the real JDK
            // `loadedAllProviders` guards `loadedProviders` (the `stream()`
            // Provider-wrapper cache), NOT `instantiatedProviders`; the real
            // iterator tracks its own position by index instead. Every
            // `ServiceLoader` that reaches this native has the flag written by
            // this file (0 at construction, 0 on `reload`, 1 only by the publish
            // step below), but requiring providers to actually be there means a
            // loader that somehow arrived with the JDK's meaning of the flag
            // re-discovers rather than reporting itself empty. A service with
            // genuinely zero providers just takes the discovery path every time,
            // which is what this native has always done.
            let cached_len = match ctx.invoke(
                "java/util/ArrayList",
                "size",
                "()I",
                &[Value::Object(Some(cache_now))],
            ) {
                Ok(Some(Value::Int(n))) => n,
                _ => 0,
            };
            if cached_len > 0 {
                let cache_now = ctx.read_native_pin(cache_pin, cache);
                let it = ctx.invoke(
                    "java/util/ArrayList",
                    "iterator",
                    "()Ljava/util/Iterator;",
                    &[Value::Object(Some(cache_now))],
                );
                ctx.unpin_native_roots(sl_pin);
                return it;
            }
            ctx.unpin_native_roots(cache_pin);
        }
    }
    let sl = ctx.read_native_pin(sl_pin, sl);
    // Extract and pin the non-builtin loader BEFORE discover_providers (which
    // triggers GC via invoke). If the loader is not re-pinned it becomes stale.
    let loader_pin_opt: Option<(_, cratonvm_types::ObjectRef)> =
        sl_non_builtin_loader(ctx, sl).map(|r| (ctx.pin_native_root(r), r));
    let providers = discover_providers(ctx, sl)?;
    // Which of these a JPMS `module-info` declared. The `provider()` static
    // factory form and the `setAccessible` grant on a provider constructor are
    // both module-path-only in the JDK (`ServiceLoader.loadProvider` looks for
    // the factory on the `ModuleServicesLookupIterator` path only, and
    // `getConstructor` calls `setAccessible` only `if (inExplicitModule)`), so
    // classpath discovery keeps its existing behaviour byte for byte.
    let sl_now = ctx.read_native_pin(sl_pin, sl);
    let origin_service_name = sl_service_name(ctx, sl_now);
    let module_declared = module_declared_providers(ctx, &origin_service_name);

    // Build an ArrayList and populate with load_provider_class(fqn).newInstance().
    let al_cls = "java/util/ArrayList";
    let al_cid = ctx.ensure_class_initialized(al_cls).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: "ArrayList: not loaded".to_string(),
        })
    })?;
    let mut list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    // Pin the providers list as a GC root: the loop below repeatedly calls into
    // Java (forName / newInstance / add), each of which can trigger a moving-GC
    // collection that relocates `list`. Without re-reading the forwarded
    // reference, `add` would mutate a stale (reused) object and the loader would
    // silently produce zero providers (the keycloak `CryptoIntegration` "Not
    // able to load any cryptoProvider" failure under real BouncyCastle).
    let list_pin = ctx.pin_native_root(list);
    // `<init>` is itself a Java invocation and can move the freshly allocated
    // list. The pin must therefore precede it, and the argument must be read
    // back through that pin before dispatch.
    list = ctx.read_native_pin(list_pin, list);
    ctx.invoke(al_cls, "<init>", "()V", &[Value::Object(Some(list))])?;

    let diag = crate::nbflags().diag_serviceloader;
    if diag {
        eprintln!(
            "[SL-DBG] iterator() entering loop with {} providers",
            providers.len()
        );
    }
    // Descriptor entries whose class could not be resolved at all. See the
    // all-or-nothing `ServiceConfigurationError` decision after the loop.
    let mut missing: Vec<String> = Vec::new();
    for fqn in providers {
        if diag {
            eprintln!("[SL-DBG]   instantiate provider={fqn}");
        }
        // Re-read the loader through the pin so it's valid after any GC triggered
        // by the previous iteration's forName/newInstance/add calls.
        let loader_cur = loader_pin_opt
            .as_ref()
            .map(|(pin, orig)| ctx.read_native_pin(*pin, *orig));
        // Bound outside the `match` so the shared borrow of `fqn` cannot outlive
        // the call into the arms, where `fqn` is moved into `missing`.
        let resolved = load_provider_class(ctx, &fqn, loader_cur);
        let class = match resolved? {
            Some(c) => c,
            None => {
                if diag {
                    eprintln!("[SL-DBG]   skip (class not found for {fqn})");
                }
                missing.push(fqn);
                continue;
            }
        };
        // newInstance via Class.getDeclaredConstructor() + Constructor.newInstance().
        let class_pin = ctx.pin_native_root(class);

        // --- JPMS `provider()` static factory --------------------------------
        // `ServiceLoader.loadProvider`: for a provider in a named module the
        // static factory, when present, REPLACES the constructor entirely. The
        // constructor path below would otherwise reach through `FactoryGreeter`'s
        // PRIVATE constructor and hand out an object that is not a `Greeter`.
        let mut built_via_factory = false;
        if module_declared.iter().any(|m| m == &fqn) {
            let class_now = ctx.read_native_pin(class_pin, class);
            if let Some(factory) = provider_factory_method(ctx, class_now) {
                let factory_pin = ctx.pin_native_root(factory);
                // The JDK fails the load when the factory's return type is not a
                // subtype of the service; a provider it may not legally hand out
                // must not be quietly dropped instead. `native_sl_stream` runs
                // the SAME call — see `factory_return_is_subtype`, which owns
                // the pin order both paths depend on.
                //
                // The error is built INSIDE that call, while `class_pin` and
                // `factory_pin` are still standing, because rendering the
                // message drives `Method.toString()`.
                if let FactoryReturn::Rejected(error) =
                    factory_return_is_subtype(ctx, sl_pin, sl, factory_pin, factory, &fqn)
                {
                    ctx.unpin_native_roots(class_pin);
                    if let Some(error) = error {
                        ctx.unpin_native_roots(sl_pin);
                        return Err(error);
                    }
                    continue;
                }
                // Caller-insensitive `setAccessible(true)` — see
                // `grant_reflective_override`.
                let factory_now = ctx.read_native_pin(factory_pin, factory);
                grant_reflective_override(ctx, factory_now);
                let empty_args = ctx.new_ref_array(
                    ctx.class_id_by_name("java/lang/Object")
                        .unwrap_or(cratonvm_types::ClassId::new(0)),
                    0,
                );
                let empty_args_pin = ctx.pin_native_root(empty_args);
                let factory_now = ctx.read_native_pin(factory_pin, factory);
                let empty_args = ctx.read_native_pin(empty_args_pin, empty_args);
                let invoked = ctx.invoke(
                    "java/lang/reflect/Method",
                    "invoke",
                    "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
                    &[
                        Value::Object(Some(factory_now)),
                        Value::Object(None),
                        Value::Object(Some(empty_args)),
                    ],
                );
                let inst = match invoked {
                    Ok(Some(Value::Object(Some(o)))) => {
                        ctx.unpin_native_roots(class_pin);
                        o
                    }
                    // `ProviderImpl.invokeFactoryMethod` fails the load on a null
                    // return. Skipping the provider here would be a fabricated
                    // success for a configuration the spec rejects.
                    //
                    // The `stream()` path reports this through the REAL
                    // `ProviderImpl.invokeFactoryMethod` bytecode at
                    // `Provider.get()`, so it already carried the JDK's exact
                    // wording. Render the same text here — the two paths
                    // disagreeing about the message for the same provider is
                    // the smaller sibling of the defect this whole record is
                    // about, and `RJdkModule` now asserts they are equal.
                    // `factory_pin` still stands at this point on purpose:
                    // `Method.toString()` is an allocating invoke and
                    // `class_pin` (taken first) would take it down with it.
                    Ok(_) => {
                        let factory_now = ctx.read_native_pin(factory_pin, factory);
                        let rendered = factory_method_display(ctx, factory_now, &fqn);
                        ctx.unpin_native_roots(class_pin);
                        let sl_now = ctx.read_native_pin(sl_pin, sl);
                        let service_name = sl_service_name(ctx, sl_now);
                        if let Some(error) = service_configuration_error(
                            ctx,
                            &format!("{service_name}: {rendered} returned null"),
                        ) {
                            ctx.unpin_native_roots(sl_pin);
                            return Err(error);
                        }
                        continue;
                    }
                    Err(failure) => {
                        ctx.unpin_native_roots(class_pin);
                        return Err(provider_construction_error(ctx, &fqn, failure));
                    }
                };
                list = ctx.read_native_pin(list_pin, list);
                let inst_pin = ctx.pin_native_root(inst);
                let inst = ctx.read_native_pin(inst_pin, inst);
                let add_result = ctx.invoke(
                    al_cls,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(list)), Value::Object(Some(inst))],
                );
                ctx.unpin_native_roots(inst_pin);
                add_result?;
                if diag {
                    eprintln!("[SL-DBG]   built via provider() factory: {fqn}");
                }
                built_via_factory = true;
            }
        }
        if built_via_factory {
            continue;
        }

        // --- JPMS constructor form -------------------------------------------
        // Once `findStaticProviderMethod` has answered null, `loadProvider`
        // applies two more rules before it constructs anything:
        //
        //     if (!service.isAssignableFrom(clazz))
        //         fail(service, clazz + " not a subtype");
        //     ctor = clazz.getConstructor();   // PUBLIC no-arg, or fail
        //
        // Neither was enforced on EITHER provider path — W6-2's last live row
        // and the sibling W7-85's sweep found beside it. Both are gated on
        // `module_declared` for the same reason the factory block above is:
        // this is `loadProvider`, which serves the MODULE path. The classpath
        // iterator (`LazyClassPathLookupIterator`) carries its own spelling of
        // both rules and that copy stays unarmed — arming it would change
        // Spring/Tomcat/Elasticsearch/WildFly boot, which walks hundreds of
        // classpath providers and none module-declared.
        let provider_is_module_declared = module_declared.iter().any(|m| m == &fqn);
        if provider_is_module_declared {
            if let ConstructorForm::Rejected(error) =
                constructor_form_is_subtype(ctx, sl_pin, sl, class_pin, class, &fqn)
            {
                ctx.unpin_native_roots(class_pin);
                if let Some(error) = error {
                    ctx.unpin_native_roots(sl_pin);
                    return Err(error);
                }
                continue;
            }
        }

        let empty_types = ctx.new_ref_array(
            ctx.class_id_by_name("java/lang/Class")
                .unwrap_or(cratonvm_types::ClassId::new(0)),
            0,
        );
        // `NativeContext::invoke` may resolve/initialize its target before it
        // has copied the supplied argument slice into a Java frame.  Keep both
        // freshly-created arguments rooted through that pre-dispatch window;
        // otherwise a collection relocates `class` or `empty_types` before
        // getDeclaredConstructor reads them.
        let empty_types_pin = ctx.pin_native_root(empty_types);
        let class = ctx.read_native_pin(class_pin, class);
        let empty_types = ctx.read_native_pin(empty_types_pin, empty_types);
        let ctor = ctx
            .invoke(
                "java/lang/Class",
                "getDeclaredConstructor",
                "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
                &[Value::Object(Some(class)), Value::Object(Some(empty_types))],
            )
            .ok()
            .and_then(|v| v);
        ctx.unpin_native_roots(empty_types_pin);
        ctx.unpin_native_roots(class_pin);
        let ctor = match ctor {
            Some(Value::Object(Some(c))) => c,
            _ => {
                if diag {
                    eprintln!("[SL-DBG]   skip (no zero-arg ctor): {fqn}");
                }
                // For a module-declared provider `getConstructor()` THROWING is
                // not a skip — `loadProvider` fails the whole load. A classpath
                // provider keeps the historic silent skip.
                if provider_is_module_declared {
                    if let Some(error) = no_public_no_arg_ctor_error(ctx, sl_pin, sl, &fqn) {
                        ctx.unpin_native_roots(sl_pin);
                        return Err(error);
                    }
                }
                continue;
            }
        };
        // GC-safety: `ctor` is held across two intervening allocating calls
        // (`setAccessible` invoke, `new_ref_array` for `empty_args`) before
        // its second use in `newInstance` below. Per the `pin_native_root`
        // contract, a moving GC in that window leaves `ctor` stale — reading
        // its `clazz` field then resolves to whatever now occupies the
        // reused slot, throwing "Constructor.newInstance: no declaring
        // class" for what looks like an entirely unrelated, arbitrary
        // provider each time (this was the residual behind the WildFly
        // DeferredExtensionContext "No META-INF/services/... found for
        // <extension>" flakiness even after `create_constructor_object`
        // itself was fixed — the Constructor object created there was fine;
        // it went stale HERE, one call site later).
        let ctor_pin = ctx.pin_native_root(ctor);
        // `getConstructor()` is public-only and this file asked
        // `getDeclaredConstructor()`, so a NON-public no-arg constructor got
        // this far, was opened by `grant_reflective_override` below, and was
        // handed out. `loadProvider` refuses it with the same error the absent
        // case raises, because the JDK cannot tell the two apart: both are
        // `getConstructor()` throwing `NoSuchMethodException`.
        if provider_is_module_declared && !constructor_is_public(ctx, ctor_pin, ctor) {
            ctx.unpin_native_roots(ctor_pin);
            if let Some(error) = no_public_no_arg_ctor_error(ctx, sl_pin, sl, &fqn) {
                ctx.unpin_native_roots(sl_pin);
                return Err(error);
            }
            continue;
        }
        // setAccessible(true). For a module-declared provider the constructor
        // lives in a package the module may neither export nor open, and the
        // caller-sensitive `setAccessible` invoke is REFUSED here (its result was
        // discarded, so the refusal was invisible and the `newInstance` below was
        // refused in turn — zero providers, silently). Write the override
        // directly instead; that is the JDK's `if (inExplicitModule(clazz))
        // ctor.setAccessible(true)` minus the caller identity this native cannot
        // supply. Classpath providers keep the historic invoke unchanged.
        if module_declared.iter().any(|m| m == &fqn) {
            grant_reflective_override(ctx, ctor);
        } else {
            let _ = ctx.invoke(
                "java/lang/reflect/AccessibleObject",
                "setAccessible",
                "(Z)V",
                &[Value::Object(Some(ctor)), Value::Int(1)],
            );
        }
        let empty_args = ctx.new_ref_array(
            ctx.class_id_by_name("java/lang/Object")
                .unwrap_or(cratonvm_types::ClassId::new(0)),
            0,
        );
        let empty_args_pin = ctx.pin_native_root(empty_args);
        let ctor = ctx.read_native_pin(ctor_pin, ctor);
        let empty_args = ctx.read_native_pin(empty_args_pin, empty_args);
        let inst_result = ctx.invoke(
            "java/lang/reflect/Constructor",
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(ctor)), Value::Object(Some(empty_args))],
        );
        ctx.unpin_native_roots(empty_args_pin);
        ctx.unpin_native_roots(ctor_pin);
        // An `InternalError` here (Linkage/NoClassDefFoundError, etc.) is a
        // genuine VM-side failure -- the class was resolved successfully
        // moments ago (`load_provider_class` above found it), so a linkage
        // failure now means something is actually broken (e.g. a poisoned
        // `ClassState` from an earlier failed resolution attempt through a
        // different code path), not a legitimately-missing/malformed
        // provider entry. Silently treating it as "provider not found"
        // (the pre-existing behavior) hides real bugs behind an empty
        // ServiceLoader result. Surface it loudly, unconditionally -- this
        // is cheap (one `tracing::warn!`) and the alternative is a silent
        // correctness gap that looks identical to a normal missing provider.
        if let Err(MethodCallFailed::InternalError(ref e)) = inst_result {
            tracing::warn!(
                provider = %fqn,
                error = ?e,
                "ServiceLoader: Constructor.newInstance failed with an internal VM error \
                 (not a provider-specific reflective failure) -- skipping this provider, \
                 but this likely indicates a real bug, not a missing/malformed provider"
            );
        }
        let inst = match inst_result {
            Ok(Some(Value::Object(Some(o)))) => o,
            Ok(_) => {
                if diag {
                    eprintln!("[SL-DBG]   skip (newInstance returned null): {fqn}");
                }
                continue;
            }
            Err(failure) => return Err(provider_construction_error(ctx, &fqn, failure)),
        };
        // Re-read the (possibly forwarded) list reference before mutating it.
        list = ctx.read_native_pin(list_pin, list);
        // `ArrayList.add` can resolve/initialize code before it has copied its
        // argument slice into a Java frame. The provider instance just returned
        // by Constructor.newInstance is therefore another native local that
        // must remain rooted through the call.
        let inst_pin = ctx.pin_native_root(inst);
        let inst = ctx.read_native_pin(inst_pin, inst);
        let add_result = ctx.invoke(
            al_cls,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(inst))],
        );
        ctx.unpin_native_roots(inst_pin);
        add_result?;
    }
    // Loop done: pick up the final forwarded list reference (still pinned).
    list = ctx.read_native_pin(list_pin, list);

    if diag {
        let size = ctx
            .invoke(al_cls, "size", "()I", &[Value::Object(Some(list))])
            .ok()
            .and_then(|v| v);
        eprintln!("[SL-DBG] iterator() final list size={:?}", size);
        list = ctx.read_native_pin(list_pin, list);
    }
    // A descriptor entry naming a class that cannot be resolved is a
    // `ServiceConfigurationError` in the JDK — `LazyClassPathLookupIterator.
    // nextProviderClass` catches `ClassNotFoundException` and calls
    // `fail(service, "Provider " + cn + " not found")`, message only, no cause.
    // This native instead skipped the entry silently, so
    // `ServiceLoader.load(Broken.class)` iterated empty and never threw
    // (regression-suite `RJdkServices.java:196`, reachable in `--real-jdk` only
    // once the caching defect at :121 stops aborting the class first; HotSpot
    // and `--jdk-only` both print `badProviderCause=none`, i.e. an SCE whose
    // `getCause()` is null — exactly this shape).
    //
    // NARROWED ON PURPOSE to "not one single provider could be built". Full JDK
    // parity would raise on the FIRST unresolvable entry, and this VM reaches
    // that arm routinely for reasons the JDK never would: the flat classpath
    // scan unions the descriptors of every jar on the path, so an optional
    // provider whose class this VM cannot yet load is common, and today every
    // caller of such a service gets the working subset. Raising there would
    // convert working partial discovery into a hard failure across the
    // Spring/Tomcat/Elasticsearch/WildFly suites in one step, with no way to
    // measure it from this lane. When nothing at all resolved, there is no
    // subset to protect and the caller's alternative is an empty iterator that
    // silently lies about the descriptor it just read.
    if !missing.is_empty() {
        let loaded = match ctx.invoke(al_cls, "size", "()I", &[Value::Object(Some(list))]) {
            Ok(Some(Value::Int(n))) => n,
            _ => 0,
        };
        list = ctx.read_native_pin(list_pin, list);
        if loaded == 0 {
            let sl_now = ctx.read_native_pin(sl_pin, sl);
            let service_name = sl_service_name(ctx, sl_now);
            if let Some(error) = provider_not_found_error(ctx, &service_name, &missing[0]) {
                ctx.unpin_native_roots(sl_pin);
                return Err(error);
            }
            list = ctx.read_native_pin(list_pin, list);
        }
    }
    // Publish the freshly instantiated providers into `instantiatedProviders`
    // and mark the cache complete, so the NEXT `iterator()` on this same
    // ServiceLoader replays these very objects instead of building new ones.
    // The iterator we hand back is the cache's own iterator on this path, which
    // is what makes the first and second iterations agree by construction
    // rather than by a copy that could drift.
    //
    // Every step degrades to today's behaviour rather than failing: no cache
    // field (synthetic layout) or a copy step that does not resolve leaves
    // `loadedAllProviders` at 0, so the next call simply re-discovers.
    //
    // The copy uses only calls this file already drives against these exact
    // objects — `List.clear` (`native_sl_reload`), `List.get`
    // (`discover_providers`' jarMetas walk), `ArrayList.size` (the diag branch
    // above) and `ArrayList.add` (the instantiation loop above) — rather than
    // `addAll`, which nothing here has ever exercised.
    let mut iterate_over = list;
    let mut published = false;
    let sl_now = ctx.read_native_pin(sl_pin, sl);
    if let Some(cache) = sl_instance_cache(ctx, sl_now) {
        let cache_pin = ctx.pin_native_root(cache);
        let cache_now = ctx.read_native_pin(cache_pin, cache);
        let mut copied = ctx
            .invoke(
                "java/util/List",
                "clear",
                "()V",
                &[Value::Object(Some(cache_now))],
            )
            .is_ok();
        list = ctx.read_native_pin(list_pin, list);
        let count = match ctx.invoke(al_cls, "size", "()I", &[Value::Object(Some(list))]) {
            Ok(Some(Value::Int(n))) => n,
            _ => {
                copied = false;
                0
            }
        };
        for i in 0..count {
            list = ctx.read_native_pin(list_pin, list);
            let element = match ctx.invoke(
                "java/util/List",
                "get",
                "(I)Ljava/lang/Object;",
                &[Value::Object(Some(list)), Value::Int(i)],
            ) {
                Ok(Some(value @ Value::Object(Some(_)))) => value,
                _ => {
                    copied = false;
                    break;
                }
            };
            // The element is a native local across `add`'s own pre-dispatch
            // resolve window, exactly like `inst` in the loop above.
            let element_pin = match element {
                Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
                _ => None,
            };
            let element = match (element, element_pin) {
                (Value::Object(Some(original)), Some(pin)) => {
                    Value::Object(Some(ctx.read_native_pin(pin, original)))
                }
                (value, _) => value,
            };
            let cache_now = ctx.read_native_pin(cache_pin, cache);
            let added = ctx.invoke(
                al_cls,
                "add",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(cache_now)), element],
            );
            if let Some(pin) = element_pin {
                ctx.unpin_native_roots(pin);
            }
            if added.is_err() {
                copied = false;
                break;
            }
        }
        if copied {
            let sl_now = ctx.read_native_pin(sl_pin, sl);
            ctx.set_field_by_name(sl_now, "loadedAllProviders", Value::Int(1));
            iterate_over = ctx.read_native_pin(cache_pin, cache);
            published = true;
        } else {
            // Leave the flag at 0: a half-copied cache must never be replayed.
            let cache_now = ctx.read_native_pin(cache_pin, cache);
            let _ = ctx.invoke(
                "java/util/List",
                "clear",
                "()V",
                &[Value::Object(Some(cache_now))],
            );
            ctx.unpin_native_roots(cache_pin);
            iterate_over = ctx.read_native_pin(list_pin, list);
        }
    }
    if diag {
        eprintln!("[SL-DBG] iterator() cache published={published}");
    }
    let it = ctx.invoke(
        al_cls,
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(iterate_over))],
    )?;
    // The returned iterator now keeps its backing list reachable via the Java
    // object graph, so the native pins can be released. `sl_pin` is the first
    // pin owned by this scope, so truncating to it also releases the loader,
    // list and cache pins taken after it.
    ctx.unpin_native_roots(list_pin);
    if let Some((pin, _)) = loader_pin_opt {
        ctx.unpin_native_roots(pin);
    }
    ctx.unpin_native_roots(sl_pin);
    Ok(it)
}

/// `ServiceLoader.stream()` — return a `Stream<Provider<S>>`-equivalent.
///
/// Root-cause note (session 95): the prior implementation routed through
/// `Spliterators.spliteratorUnknownSize(iterator(), 0)` followed by
/// `StreamSupport.stream(spliterator, false)`. In the open-sourced
/// non-synthetic-JDK build, `Spliterators.spliteratorUnknownSize` has NO
/// native registration (`register_p69_spliterator` only runs under
/// `register_synthetic_overrides`), so the call fell through to real-JDK
/// bytecode and produced a real `Spliterators$IteratorSpliterator`. The
/// `native_stream_support_stream_from_spliterator` override then read
/// field 0 of that real spliterator, found it was not our synthetic
/// backing `Object[]`, and fell into `drain_spliterator_to_stream` —
/// which is a give-up stub that always yields an EMPTY stream. Net
/// effect: `ServiceLoader.load(X).stream().count()` returned 0 even
/// though `iterator()` produced the providers correctly.
///
/// Fix (session 95): the synthetic stream backing is an `Object[]` in
/// field 0 — the layout every `Stream.*` native (`count`, `map`,
/// `filter`, `toList`, `forEach`, ...) already understands. No JDK
/// Spliterator middleman.
///
/// Element-type fix (this change): `ServiceLoader.stream()` must yield
/// `Stream<Provider<S>>` — i.e. each element is a
/// `java.util.ServiceLoader$Provider` *wrapper*, NOT an instantiated
/// service object. The prior body drained `iterator()` (which correctly
/// yields service *instances* `S`) straight into the stream, so callers
/// doing the canonical `.map(Provider::type)` / `.map(Provider::get)` /
/// `.filter(p -> p.type()...)` (JUnit5's `LauncherFactory`, Elasticsearch's
/// `CliToolProvider.load`, every SPI-stream framework) hit
/// `AbstractMethodError: Provider.type() has no Code attribute` — the
/// raw service instance has no `type()`/`get()` method. We now build a
/// real `ServiceLoader$ProviderImpl(service, type, ctor)` per provider so
/// `type()`/`get()` run real JDK bytecode and `instanceof Provider` holds.
fn native_sl_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sl = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        // Null receiver → empty stream (not null) so downstream
        // `.count()` / `.filter()` natives have a valid receiver.
        _ => return alloc_synthetic_stream(ctx, &[]),
    };
    let diag = crate::nbflags().diag_serviceloader;

    // Pin the ServiceLoader receiver — every `invoke` below can trigger a
    // moving GC that relocates it.
    let sl_pin = ctx.pin_native_root(sl);
    let sl_for_discover = ctx.read_native_pin(sl_pin, sl);
    let providers = discover_providers(ctx, sl_for_discover)?;
    if providers.is_empty() {
        ctx.unpin_native_roots(sl_pin);
        return alloc_synthetic_stream(ctx, &[]);
    }
    // See `native_sl_iterator`: the `provider()` factory form is module-path
    // only, and it changes what `Provider.type()` answers.
    let sl_now = ctx.read_native_pin(sl_pin, sl);
    let origin_service_name = sl_service_name(ctx, sl_now);
    let module_declared = module_declared_providers(ctx, &origin_service_name);

    // Resolve the JDK-internal wrapper class. If it is unavailable (e.g. a
    // stripped runtime), fall back to draining service instances so the
    // stream is at least non-empty rather than crashing.
    const PROVIDER_IMPL: &str = "java/util/ServiceLoader$ProviderImpl";
    let pi_cid = match ctx.ensure_class_initialized(PROVIDER_IMPL) {
        Ok(cid) => cid,
        Err(_) => {
            let sl = ctx.read_native_pin(sl_pin, sl);
            ctx.unpin_native_roots(sl_pin);
            if diag {
                eprintln!("[SL-DBG] stream(): ProviderImpl unavailable, draining instances");
            }
            return drain_instances_to_stream(ctx, &[Value::Object(Some(sl))]);
        }
    };
    let pi_fields = ctx.class_num_total_fields(pi_cid).max(4);

    // Accumulate the wrappers in a pinned ArrayList so each is a GC root
    // while the loop keeps re-entering Java (forName / getDeclaredConstructor
    // / <init> all can collect). Mirrors `native_sl_iterator`'s discipline.
    let al_cls = "java/util/ArrayList";
    let al_cid = ctx.ensure_class_initialized(al_cls).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: "ArrayList: not loaded".to_string(),
        })
    })?;
    let mut list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    let list_pin = ctx.pin_native_root(list);
    list = ctx.read_native_pin(list_pin, list);
    ctx.invoke(al_cls, "<init>", "()V", &[Value::Object(Some(list))])?;

    for fqn in &providers {
        // Re-read sl through the pin so we have a fresh reference after
        // GC may have moved it in a previous iteration.
        let sl_cur = ctx.read_native_pin(sl_pin, sl);
        // For embedded-JAR providers (e.g. ES EmbeddedImplClassLoader),
        // Class.forName(fqn) uses the flat classpath and won't find the class.
        // Pass the loader so load_provider_class can fall back to loadClass.
        let loader_cur = sl_non_builtin_loader(ctx, sl_cur);
        let type_class = match load_provider_class(ctx, fqn, loader_cur)? {
            Some(c) => c,
            None => {
                if diag {
                    eprintln!("[SL-DBG]   stream skip (class not found for {fqn})");
                }
                continue;
            }
        };
        // Pin `type` — it must survive getDeclaredConstructor + setAccessible
        // before being stored into the wrapper.
        let type_pin = ctx.pin_native_root(type_class);

        // --- JPMS `provider()` static factory --------------------------------
        // `ProviderImpl` has a distinct `(Class, Class, Method)` constructor for
        // this form, and it records `factoryMethod.getReturnType()` as `type` —
        // which is exactly what `Provider.type()` answers. Building the
        // constructor flavour here instead reports the FACTORY class
        // (`FactoryGreeter`) where the JDK reports the service type (`Greeter`),
        // and `get()` then reaches through a private constructor for an object
        // that does not implement the service at all.
        #[allow(clippy::type_complexity)]
        let mut factory: Option<(
            usize,
            cratonvm_types::ObjectRef,
            usize,
            cratonvm_types::ObjectRef,
        )> = None;
        if module_declared.iter().any(|m| m == fqn) {
            let type_now = ctx.read_native_pin(type_pin, type_class);
            if let Some(method) = provider_factory_method(ctx, type_now) {
                let method_pin = ctx.pin_native_root(method);
                // W7-85: the same `!service.isAssignableFrom(returnType)` gate
                // `native_sl_iterator` applies. It was absent here and nowhere
                // else, so an illegal module-declared factory raised
                // `ServiceConfigurationError` from `iterator()` and was quietly
                // handed out by `stream()` — a `Provider` whose `type()` is the
                // wrong class and whose `get()` returns an object that is not
                // of the service type. Nothing downstream catches that: the
                // `(S)` cast in `ProviderImpl.invokeFactoryMethod` is erased.
                match factory_return_is_subtype(ctx, sl_pin, sl, method_pin, method, fqn) {
                    FactoryReturn::Accepted(ret) => {
                        let ret_pin = ctx.pin_native_root(ret);
                        // Caller-insensitive `setAccessible(true)` so the real
                        // `ProviderImpl.invokeFactoryMethod` bytecode can call
                        // it — see `grant_reflective_override`. Granted only
                        // once the return type is ACCEPTED: opening a factory
                        // this loader is about to refuse would leave a door
                        // ajar for a caller that must never exist.
                        let method_now = ctx.read_native_pin(method_pin, method);
                        grant_reflective_override(ctx, method_now);
                        factory = Some((method_pin, method, ret_pin, ret));
                    }
                    // Unreadable return type: fall back to the constructor
                    // flavour rather than build a half-formed wrapper.
                    FactoryReturn::Unreadable => ctx.unpin_native_roots(method_pin),
                    FactoryReturn::Rejected(error) => {
                        // `type_pin` is this iteration's first pin, so
                        // truncating to it also releases `method_pin` and
                        // anything the gate took after it.
                        ctx.unpin_native_roots(type_pin);
                        match error {
                            Some(error) => {
                                ctx.unpin_native_roots(sl_pin);
                                return Err(error);
                            }
                            // The error object itself could not be built. Drop
                            // the provider rather than hand it out — handing it
                            // out is the one outcome this gate exists to stop.
                            None => continue,
                        }
                    }
                }
            }
        }

        // --- JPMS constructor form -------------------------------------------
        // The two rules `native_sl_iterator` now applies once the factory form
        // is out of the picture: `!service.isAssignableFrom(clazz)` and the
        // PUBLIC no-arg constructor. They land on both provider paths in one
        // change on purpose — installing a validation on one of two siblings,
        // and letting a fixture that only ever walks the other one call it
        // covered, is the defect W7-85 exists to document.
        let provider_is_module_declared = module_declared.iter().any(|m| m == fqn);
        if factory.is_none() && provider_is_module_declared {
            if let ConstructorForm::Rejected(error) =
                constructor_form_is_subtype(ctx, sl_pin, sl, type_pin, type_class, fqn)
            {
                // `type_pin` is this iteration's first pin, so truncating to it
                // releases everything this iteration has taken.
                ctx.unpin_native_roots(type_pin);
                match error {
                    Some(error) => {
                        ctx.unpin_native_roots(sl_pin);
                        return Err(error);
                    }
                    None => continue,
                }
            }
        }

        // type.getDeclaredConstructor() → the no-arg ctor used by get().
        // Skipped entirely for the factory form: `ServiceLoader.loadProvider`
        // never looks at a constructor once `findStaticProviderMethod` answered.
        let ctor_slot = if factory.is_some() {
            None
        } else {
            let empty_types = ctx.new_ref_array(
                ctx.class_id_by_name("java/lang/Class")
                    .unwrap_or(cratonvm_types::ClassId::new(0)),
                0,
            );
            let empty_types_pin = ctx.pin_native_root(empty_types);
            let type_now = ctx.read_native_pin(type_pin, type_class);
            let empty_types = ctx.read_native_pin(empty_types_pin, empty_types);
            let ctor = match ctx.invoke(
                "java/lang/Class",
                "getDeclaredConstructor",
                "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
                &[
                    Value::Object(Some(type_now)),
                    Value::Object(Some(empty_types)),
                ],
            ) {
                Ok(Some(Value::Object(Some(c)))) => c,
                other => {
                    if diag {
                        eprintln!("[SL-DBG]   stream skip (no no-arg ctor for {fqn} → {other:?})");
                    }
                    // Release this iteration's pins, keep sl + list.
                    ctx.unpin_native_roots(type_pin);
                    // Same rule as `native_sl_iterator`: for a module-declared
                    // provider this is `getConstructor()` throwing, which
                    // `loadProvider` turns into a failed load, not a skip.
                    if provider_is_module_declared {
                        if let Some(error) = no_public_no_arg_ctor_error(ctx, sl_pin, sl, fqn) {
                            ctx.unpin_native_roots(sl_pin);
                            return Err(error);
                        }
                    }
                    continue;
                }
            };
            ctx.unpin_native_roots(empty_types_pin);
            let ctor_pin = ctx.pin_native_root(ctor);
            // A non-public no-arg constructor is invisible to
            // `Class.getConstructor()`, so the JDK never reaches it; this file
            // asked `getDeclaredConstructor()` and then opened it below.
            if provider_is_module_declared && !constructor_is_public(ctx, ctor_pin, ctor) {
                // `type_pin` precedes `ctor_pin`; truncating to it takes both.
                ctx.unpin_native_roots(type_pin);
                if let Some(error) = no_public_no_arg_ctor_error(ctx, sl_pin, sl, fqn) {
                    ctx.unpin_native_roots(sl_pin);
                    return Err(error);
                }
                continue;
            }
            // setAccessible(true) so ProviderImpl.get()'s reflective newInstance
            // succeeds for non-public providers. A module-declared provider's
            // package may be neither exported nor opened, and the
            // caller-sensitive invoke is refused there — write the override
            // directly, as `native_sl_iterator` does.
            if module_declared.iter().any(|m| m == fqn) {
                let ctor_now = ctx.read_native_pin(ctor_pin, ctor);
                grant_reflective_override(ctx, ctor_now);
            } else {
                let ctor_now = ctx.read_native_pin(ctor_pin, ctor);
                let _ = ctx.invoke(
                    "java/lang/reflect/AccessibleObject",
                    "setAccessible",
                    "(Z)V",
                    &[Value::Object(Some(ctor_now)), Value::Int(1)],
                );
            }
            Some((ctor_pin, ctor))
        };

        // The member ProviderImpl stores, and the `type` it reports: for the
        // factory form those are the Method and its return type; otherwise the
        // constructor and the provider class itself. Both are read back through
        // their pins on every constructor attempt below rather than being held
        // raw across provider allocation/retries.
        let (member_pin, member_orig, type_read_pin, type_read_orig) = match (factory, ctor_slot) {
            (Some((mp, m, rp, r)), _) => (mp, m, rp, r),
            (None, Some((cp, c))) => (cp, c, type_pin, type_class),
            // Unreachable: `ctor_slot` is Some whenever `factory` is None.
            (None, None) => {
                ctx.unpin_native_roots(type_pin);
                continue;
            }
        };

        // new ServiceLoader$ProviderImpl(service, type, ctor[, acc]) — the
        // classpath-flavour constructor (factoryMethod = null, acc = null).
        // JDK 17–23: 4-arg form with a trailing AccessControlContext.
        // JDK 24+ (security-manager removal): the acc parameter is GONE —
        // 3-arg form. Hardcoding either breaks the other java-home (jdk-25
        // runs hit NoSuchMethodError on the 4-arg call, cascading into
        // "Log4j2 could not find a logging implementation"). Probe both and
        // remember which form this JDK has (0=unknown, 1=4-arg, 2=3-arg).
        // Tag 3 is the `(Class, Class, Method)` factory form, which has no
        // AccessControlContext variant and is never cached in `CTOR_FORM`.
        const CTOR_4ARG: &str =
            "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/reflect/Constructor;Ljava/security/AccessControlContext;)V";
        const CTOR_3ARG: &str =
            "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/reflect/Constructor;)V";
        const FACTORY_3ARG: &str =
            "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/reflect/Method;)V";
        static CTOR_FORM: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        // Pick the ctor by *querying* which one ProviderImpl declares, rather
        // than invoke-and-catch. A failed invoke of the absent form emits a
        // misleading `NoSuchMethodError ...ProviderImpl.<init>(...
        // AccessControlContext)` WARN on every JDK-24+ run (that arg was dropped
        // with the SecurityManager removal); querying first keeps the log clean
        // and avoids a wasted allocation/GC window. Cached process-wide.
        let mut form = CTOR_FORM.load(std::sync::atomic::Ordering::Relaxed);
        if form == 0 {
            form = if ctx.method_exists(PROVIDER_IMPL, "<init>", CTOR_4ARG) {
                1
            } else if ctx.method_exists(PROVIDER_IMPL, "<init>", CTOR_3ARG) {
                2
            } else {
                0
            };
            CTOR_FORM.store(form, std::sync::atomic::Ordering::Relaxed);
        }
        let provider = ctx.alloc_object(pi_cid, pi_fields);
        let provider_pin = ctx.pin_native_root(provider);
        let mut ctor_ok = false;
        let mut ctor_err = None;
        // Forms to try: the queried one first; fall back to the other only if the
        // query was inconclusive (form==0) or the invoke unexpectedly fails.
        let order: &[(u8, &str)] = if factory.is_some() {
            &[(3, FACTORY_3ARG)]
        } else {
            match form {
                2 => &[(2, CTOR_3ARG), (1, CTOR_4ARG)],
                _ => &[(1, CTOR_4ARG), (2, CTOR_3ARG)],
            }
        };
        for &(tag, desc) in order {
            if tag <= 2 && form != 0 && tag != form {
                continue;
            }
            // Re-read pins: a failed prior attempt may have allocated (GC).
            let provider_now = ctx.read_native_pin(provider_pin, provider);
            let type_now = ctx.read_native_pin(type_read_pin, type_read_orig);
            let member_now = ctx.read_native_pin(member_pin, member_orig);
            let sl_now = ctx.read_native_pin(sl_pin, sl);
            let service = match ctx.get_field_by_name(sl_now, "service") {
                v @ Value::Object(Some(_)) => v,
                _ => ctx.get_field(sl_now, 0),
            };
            let mut args = vec![
                Value::Object(Some(provider_now)),
                service,
                Value::Object(Some(type_now)),
                Value::Object(Some(member_now)),
            ];
            if tag == 1 {
                args.push(Value::Object(None)); // acc = null
            }
            match ctx.invoke(PROVIDER_IMPL, "<init>", desc, &args) {
                Ok(_) => {
                    if tag <= 2 {
                        CTOR_FORM.store(tag, std::sync::atomic::Ordering::Relaxed);
                    }
                    ctor_ok = true;
                    break;
                }
                Err(e) => ctor_err = Some(e),
            }
        }
        if !ctor_ok {
            if diag {
                eprintln!("[SL-DBG]   stream skip (ProviderImpl <init> {fqn} → {ctor_err:?})");
            }
            ctx.unpin_native_roots(type_pin);
            continue;
        }
        let provider = ctx.read_native_pin(provider_pin, provider);

        let list_now = ctx.read_native_pin(list_pin, list);
        ctx.invoke(
            al_cls,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list_now)), Value::Object(Some(provider))],
        )?;
        // Release the per-iteration pins (type/ctor/provider); sl + list stay.
        ctx.unpin_native_roots(type_pin);
    }

    // Materialise the wrappers as an `Object[]` (the synthetic stream
    // backing). `toArray()` returns an exactly-sized array.
    list = ctx.read_native_pin(list_pin, list);
    let arr_val = ctx.invoke(
        al_cls,
        "toArray",
        "()[Ljava/lang/Object;",
        &[Value::Object(Some(list))],
    )?;
    let stream = match arr_val {
        Some(Value::Object(Some(arr))) => {
            // GC-safety: `ensure_class_initialized`/`alloc_object` below can
            // trigger a moving GC; `arr` is stored into the new stream
            // afterward, unpinned otherwise.
            let arr_pin = ctx.pin_native_root(arr);
            let cid = ctx
                .ensure_class_initialized("java/util/stream/Stream")
                .unwrap_or(cratonvm_types::ClassId::new(0));
            let nfields = ctx.class_num_total_fields(cid).max(1);
            let s = ctx.alloc_object(cid, nfields);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            ctx.set_field(s, 0, Value::Object(Some(arr)));
            Some(Value::Object(Some(s)))
        }
        _ => Some(alloc_synthetic_stream(ctx, &[])?.unwrap()),
    };
    if diag {
        eprintln!(
            "[SL-DBG] stream() built Provider wrappers for {} providers",
            providers.len()
        );
    }
    ctx.unpin_native_roots(sl_pin);
    Ok(stream)
}

/// Fallback used only when `ServiceLoader$ProviderImpl` cannot be
/// resolved: drain `iterator()` (service instances) straight into the
/// synthetic stream. This loses the `Provider` wrapper semantics but
/// keeps a non-empty stream rather than crashing.
fn drain_instances_to_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let it = native_sl_iterator(ctx, args)?;
    let iter_obj = match it {
        Some(Value::Object(Some(o))) => o,
        _ => return alloc_synthetic_stream(ctx, &[]),
    };
    // GC-safety: each `invoke_virtual` call below can trigger a moving GC;
    // `iter_obj` is reused on every loop iteration, unpinned otherwise.
    let iter_pin = ctx.pin_native_root(iter_obj);
    let mut collected: Vec<(Value, Option<usize>)> = Vec::new();
    const SAFETY_CAP: usize = 1_000_000;
    loop {
        let iter_obj = ctx.read_native_pin(iter_pin, iter_obj);
        let has_next = ctx.invoke_virtual(iter_obj, "hasNext", "()Z", &[]);
        if !matches!(has_next, Ok(Some(Value::Int(1)))) {
            break;
        }
        let iter_obj = ctx.read_native_pin(iter_pin, iter_obj);
        let next = ctx.invoke_virtual(iter_obj, "next", "()Ljava/lang/Object;", &[]);
        match next {
            Ok(Some(v)) => {
                let pin = match v {
                    Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
                    _ => None,
                };
                collected.push((v, pin));
            }
            _ => break,
        }
        if collected.len() >= SAFETY_CAP {
            break;
        }
    }
    let collected: Vec<Value> = collected
        .into_iter()
        .map(|(value, pin)| match (value, pin) {
            (Value::Object(Some(original)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(pin, original)))
            }
            (value, _) => value,
        })
        .collect();
    let result = alloc_synthetic_stream(ctx, &collected);
    ctx.unpin_native_roots(iter_pin);
    result
}

fn native_sl_find_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let it = native_sl_iterator(ctx, args)?;
    let iter_obj = match it {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return empty_optional(ctx);
        }
    };
    // GC-safety: `invoke`'s `hasNext` call below can trigger a moving GC;
    // `iter_obj` is reused in the following `next` call, unpinned otherwise.
    let iter_pin = ctx.pin_native_root(iter_obj);
    let has_next = ctx.invoke(
        "java/util/Iterator",
        "hasNext",
        "()Z",
        &[Value::Object(Some(iter_obj))],
    )?;
    if matches!(has_next, Some(Value::Int(0)) | None) {
        ctx.unpin_native_roots(iter_pin);
        return empty_optional(ctx);
    }
    let iter_obj = ctx.read_native_pin(iter_pin, iter_obj);
    let first = ctx.invoke(
        "java/util/Iterator",
        "next",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(iter_obj))],
    )?;
    let first_obj = match first {
        Some(Value::Object(Some(o))) => o,
        _ => {
            ctx.unpin_native_roots(iter_pin);
            return empty_optional(ctx);
        }
    };
    let first_pin = ctx.pin_native_root(first_obj);
    let first_obj = ctx.read_native_pin(first_pin, first_obj);
    let opt = ctx.invoke(
        "java/util/Optional",
        "of",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        &[Value::Object(Some(first_obj))],
    )?;
    ctx.unpin_native_roots(iter_pin);
    Ok(opt)
}

fn empty_optional(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let empty = ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[])?;
    Ok(empty)
}

/// `ServiceLoader.spliterator()` — the JDK inherits `Iterable.spliterator()`'s
/// default body (`Spliterators.spliteratorUnknownSize(iterator(), 0)`), but
/// the default-method dispatch through our interpreter has historically not
/// propagated elements through to the resulting stream (the empty-stream
/// surfaces as Elasticsearch's `AssertionError: available names are []` from
/// `CliToolProvider.load` even though our native `ServiceLoader.iterator()`
/// returns 13 providers). Provide a direct native that builds a synthetic
/// 3-field Spliterator (array, pos, fence) backed by the freshly-discovered
/// providers so callers like `sl.spliterator().stream().filter(...)` see the
/// real provider list.
fn native_sl_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Drive the existing iterator() native to produce an Iterator over the
    // instantiated providers, then drain it into an Object[]. The whole
    // approach mirrors `Spliterators.spliteratorUnknownSize` but is wired
    // directly onto `ServiceLoader` so the call site doesn't depend on the
    // `Iterable.spliterator()` default-method machinery.
    let iter_val = native_sl_iterator(ctx, args)?;
    let iter = match iter_val {
        Some(Value::Object(Some(o))) => o,
        _ => {
            let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            let cid = ctx
                .ensure_class_initialized("java/util/Spliterator")
                .unwrap_or(cratonvm_types::ClassId::new(0));
            let n = ctx.class_num_total_fields(cid).max(3);
            let obj = ctx.alloc_object(cid, n);
            ctx.set_field(obj, 0, Value::Object(Some(empty)));
            ctx.set_field(obj, 1, Value::Int(0));
            ctx.set_field(obj, 2, Value::Int(0));
            return Ok(Some(Value::Object(Some(obj))));
        }
    };
    // Drain the iterator into a Vec<Value>.
    let mut collected: Vec<Value> = Vec::new();
    const SAFETY_CAP: usize = 1_000_000;
    // `iter` is held across every call in this loop, each of which is real
    // Java that can collect. Pin once, re-derive per use.
    let iter_pin = ctx.pin_native_root(iter);
    let mut iter = iter;
    loop {
        iter = ctx.read_native_pin(iter_pin, iter);
        let has_next = ctx.invoke_virtual(iter, "hasNext", "()Z", &[]);
        if !matches!(has_next, Ok(Some(Value::Int(1)))) {
            break;
        }
        iter = ctx.read_native_pin(iter_pin, iter);
        let next = ctx.invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[]);
        let val = match next {
            Ok(Some(v)) => v,
            _ => break,
        };
        collected.push(val);
        if collected.len() >= SAFETY_CAP {
            break;
        }
    }
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, collected.len());
    for (i, v) in collected.iter().enumerate() {
        ctx.set_array_element(arr, i, *v);
    }
    let cid = ctx
        .ensure_class_initialized("java/util/Spliterator")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let n = ctx.class_num_total_fields(cid).max(3);
    let obj = ctx.alloc_object(cid, n);
    ctx.set_field(obj, 0, Value::Object(Some(arr)));
    ctx.set_field(obj, 1, Value::Int(0));
    ctx.set_field(obj, 2, Value::Int(collected.len() as i32));
    Ok(Some(Value::Object(Some(obj))))
}

/// Late-binding override for `StreamSupport.stream(Spliterator, boolean)`.
///
/// Background — the prior `phases_late.rs::register_p69_spliterator`
/// registration of the same triple only runs in synthetic-jdk builds
/// (`register_builtins` is `cfg(feature = "synthetic-jdk")`), so in the
/// real-JDK CLI binary THIS registration is the only live implementation.
/// Re-registering here also ensures the closure is the LAST writer to the
/// `NativeMethodRegistry` HashMap for the
/// `(StreamSupport, stream, (Spliterator,Z)Stream)` triple.
///
/// Dispatch:
///  * CratonVM-synthetic spliterators — runtime class is the bare
///    `java/util/Spliterator` interface; built by our
///    `Collection.spliterator()` / `ServiceLoader.spliterator()` /
///    `Spliterators.spliterator(...)` natives — carry a fully-materialised
///    backing `Object[]` in field 0 plus `pos`/`fence` cursors. Snapshot
///    that slice directly.
///  * ANY other class is a real `Spliterator` implementation (JDK
///    `Spliterators$IteratorSpliterator`, Spring's
///    `TypeMappedAnnotations$AggregatesSpliterator`, log4j's
///    `ServiceLoaderUtil$ServiceLoaderSpliterator`, ...). Its private field
///    layout is not ours to read: the previous body speculatively treated
///    field 0 as the backing array whenever it happened to hold an array
///    (wrong elements + misread cursors) and otherwise gave up with an
///    EMPTY stream. The empty stream silently dropped every annotation
///    Spring's `MergedAnnotations.stream()` feeds through
///    `getAllAnnotationAttributes` → the `MultiValueMap` finisher mapped
///    empty→null → `ConditionEvaluator` found no condition classes →
///    `@Conditional`/`@Profile` beans registered unconditionally
///    (spring-boot bug report SB-04). Drain real spliterators through
///    their public `tryAdvance(Consumer)` contract instead.
fn native_stream_support_stream_from_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let spliterator = match args.first() {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            // Null spliterator → empty stream.
            return alloc_synthetic_stream(ctx, &[]);
        }
    };
    // GC-safety: `ensure_class_initialized`/`alloc_object` below (in both
    // branches) can trigger a moving GC; `spliterator` is reused afterward
    // (either stashed in the lazy slot, or re-read from field 0), unpinned
    // otherwise.
    let spliterator_pin = ctx.pin_native_root(spliterator);
    let spliterator = ctx.read_native_pin(spliterator_pin, spliterator);
    let spl_class = ctx.class_name_of_id(ctx.class_id_of_object(spliterator));
    if spl_class.as_deref() != Some("java/util/Spliterator") {
        // Real Spliterator implementation. DEFER draining: stash the spliterator
        // in the synthetic stream's lazy slot (field 2) so the terminal op can
        // drive it lazily. This makes
        //   `StreamSupport.stream(spliterator, false).forEach(consumer)`
        // consume each element AS it is produced (interleaved tryAdvance/accept),
        // matching the JDK — required for side-effecting consumers such as
        // Hibernate's `getResultStream().forEach(ld -> { …; em.flush(); em.clear(); })`,
        // where eager buffering detached a shared entity (DetachedPreviousRowStateTest).
        // Any non-forEach op materialises on demand — see native-collections
        // `materialize_lazy_stream` / `stream_lazy_spliterator`.
        let cid = ctx
            .ensure_class_initialized("java/util/stream/Stream")
            .unwrap_or(cratonvm_types::ClassId::new(0));
        // Force ≥3 fields so the lazy-spliterator slot (2) exists alongside
        // elements (0) and close-handlers (1).
        let nfields = ctx.class_num_total_fields(cid).max(3);
        let stream = ctx.alloc_object(cid, nfields);
        let stream_pin = ctx.pin_native_root(stream);
        // cce0079 hardening: keep the spliterator pinned through BOTH field
        // stores (refresh immediately before its own store), and unpin only
        // afterwards — closes any residual in-store GC window.
        let mut stream = ctx.read_native_pin(stream_pin, stream);
        ctx.set_field(stream, 0, Value::Object(None));
        stream = ctx.read_native_pin(stream_pin, stream);
        let spliterator = ctx.read_native_pin(spliterator_pin, spliterator);
        ctx.set_field(stream, 2, Value::Object(Some(spliterator)));
        stream = ctx.read_native_pin(stream_pin, stream);
        ctx.unpin_native_roots(spliterator_pin);
        return Ok(Some(Value::Object(Some(stream))));
    }
    // Synthetic spliterator: field 0 is the fully-materialised Object[]
    // (2-field variants carry (array, cursor); 3-field ones (array, pos,
    // fence)). A missing/non-array field 0 means an empty synthetic
    // spliterator.
    let spliterator = ctx.read_native_pin(spliterator_pin, spliterator);
    let field0 = ctx.get_field(spliterator, 0);
    let arr = match field0 {
        Value::Object(Some(a)) if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => a,
        _ => {
            ctx.unpin_native_roots(spliterator_pin);
            return alloc_synthetic_stream(ctx, &[]);
        }
    };
    let pos = match ctx.get_field(spliterator, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let fence = match ctx.get_field(spliterator, 2) {
        Value::Int(v) => v as usize,
        _ => ctx.array_length(arr),
    };
    // Snapshot the slice [pos, fence) into a fresh array so the resulting
    // Stream's lifetime is independent of the spliterator's cursor.
    let n = fence.saturating_sub(pos);
    // GC-safety: `new_array` below can trigger a moving GC; `arr` (the
    // source backing array) is read from afterward in the copy loop.
    let arr_pin = ctx.pin_native_root(arr);
    let snapshot = ctx.new_array(cratonvm_types::ArrayElementType::Reference, n);
    let snapshot_pin = ctx.pin_native_root(snapshot);
    for i in 0..n {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let snapshot = ctx.read_native_pin(snapshot_pin, snapshot);
        let v = ctx.get_array_element(arr, pos + i);
        let value_pin = match v {
            Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
            _ => None,
        };
        let v = match (v, value_pin) {
            (Value::Object(Some(object)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(pin, object)))
            }
            (value, _) => value,
        };
        ctx.set_array_element(snapshot, i, v);
        if let Some(pin) = value_pin {
            ctx.unpin_native_roots(pin);
        }
    }
    // GC-safety: `ensure_class_initialized`/`alloc_object` below can trigger
    // a moving GC; `snapshot` is stored into the new stream afterward.
    let cid = ctx
        .ensure_class_initialized("java/util/stream/Stream")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let nfields = ctx.class_num_total_fields(cid).max(1);
    let stream = ctx.alloc_object(cid, nfields);
    let stream_pin = ctx.pin_native_root(stream);
    let snapshot = ctx.read_native_pin(snapshot_pin, snapshot);
    let mut stream = ctx.read_native_pin(stream_pin, stream);
    ctx.set_field(stream, 0, Value::Object(Some(snapshot)));
    stream = ctx.read_native_pin(stream_pin, stream);
    ctx.unpin_native_roots(spliterator_pin);
    Ok(Some(Value::Object(Some(stream))))
}

fn alloc_synthetic_stream(ctx: &mut dyn NativeContext, elems: &[Value]) -> MethodCallResult {
    // `new_array` can collect before any input element has entered the Java
    // heap. Root every object-valued slice entry first, and refresh each one
    // immediately before its store.
    let mut elem_pin_base = None;
    let elem_pins: Vec<Option<usize>> = elems
        .iter()
        .map(|value| match value {
            Value::Object(Some(object)) => {
                let pin = ctx.pin_native_root(*object);
                elem_pin_base.get_or_insert(pin);
                Some(pin)
            }
            _ => None,
        })
        .collect();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, elems.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, v) in elems.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let value = match (*v, elem_pins[i]) {
            (Value::Object(Some(original)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(pin, original)))
            }
            (value, _) => value,
        };
        ctx.set_array_element(arr, i, value);
    }
    // GC-safety: `ensure_class_initialized`/`alloc_object` below can trigger
    // a moving GC; `arr` is stored into the new stream afterward.
    let cid = ctx
        .ensure_class_initialized("java/util/stream/Stream")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let nfields = ctx.class_num_total_fields(cid).max(1);
    let stream = ctx.alloc_object(cid, nfields);
    let stream_pin = ctx.pin_native_root(stream);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let mut stream = ctx.read_native_pin(stream_pin, stream);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    stream = ctx.read_native_pin(stream_pin, stream);
    ctx.unpin_native_roots(elem_pin_base.unwrap_or(arr_pin));
    Ok(Some(Value::Object(Some(stream))))
}

/// Synthetic consumer class used by [`drain_real_spliterator`]. Layout:
///   field 0: Object[] storage (capacity == array_length)
///   field 1: Int — current logical length
/// Its `accept(Object)V` native (registered in
/// `register_service_loader_natives`) appends, growing storage on demand.
const STREAM_COLLECTOR_CLASS: &str = "cratonvm/internal/StreamCollector";

fn native_stream_collector_accept(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    // The callback's Java arguments are rooted by safe_native_call, but the
    // backing array fetched from the collector is not. Root the complete
    // collector graph across every allocation and write barrier.
    let pin_base = ctx.pin_native_root(this);
    let elem_pin = match elem {
        Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
        _ => None,
    };
    let this = ctx.read_native_pin(pin_base, this);
    let mut len = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let storage = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16),
    };
    let storage_pin = ctx.pin_native_root(storage);
    let cap = ctx.array_length(storage);
    let (storage, active_storage_pin) = if len >= cap {
        let new_cap = (cap * 2).max(16);
        let bigger = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
        let bigger_pin = ctx.pin_native_root(bigger);
        for i in 0..len {
            let storage = ctx.read_native_pin(storage_pin, storage);
            let bigger = ctx.read_native_pin(bigger_pin, bigger);
            let v = ctx.get_array_element(storage, i);
            let value_pin = match v {
                Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
                _ => None,
            };
            let v = match (v, value_pin) {
                (Value::Object(Some(object)), Some(pin)) => {
                    Value::Object(Some(ctx.read_native_pin(pin, object)))
                }
                (value, _) => value,
            };
            ctx.set_array_element(bigger, i, v);
            if let Some(pin) = value_pin {
                ctx.unpin_native_roots(pin);
            }
        }
        let this = ctx.read_native_pin(pin_base, this);
        let bigger = ctx.read_native_pin(bigger_pin, bigger);
        ctx.set_field(this, 0, Value::Object(Some(bigger)));
        (bigger, bigger_pin)
    } else {
        (storage, storage_pin)
    };
    let storage = ctx.read_native_pin(active_storage_pin, storage);
    let elem = match (elem, elem_pin) {
        (Value::Object(Some(object)), Some(pin)) => {
            Value::Object(Some(ctx.read_native_pin(pin, object)))
        }
        (value, _) => value,
    };
    ctx.set_array_element(storage, len, elem);
    len += 1;
    let this = ctx.read_native_pin(pin_base, this);
    ctx.set_field(this, 1, Value::Int(len as i32));
    ctx.unpin_native_roots(pin_base);
    Ok(None)
}

/// Drain a real (non-synthetic) `Spliterator` implementation into a fresh
/// exactly-sized `Object[]` by repeatedly invoking its public
/// `tryAdvance(Consumer)` contract with a `cratonvm/internal/StreamCollector`
/// consumer. `tryAdvance` is a concrete method on every conforming
/// Spliterator implementation; `forEachRemaining` (frequently only the
/// interface default, whose default-method dispatch is less dependable from
/// native context) is kept as a fallback when the very first `tryAdvance`
/// dispatch fails outright. A mid-drain Java exception propagates — HotSpot
/// would surface it from the terminal stream operation too.
fn drain_real_spliterator(
    ctx: &mut dyn NativeContext,
    spliterator: cratonvm_types::ObjectRef,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    // The third of the three `StreamCollector` mints. Ask the policy first for
    // the same reason as the other two: an infallible fabrication anywhere
    // makes the refusal everywhere else depend on which path ran first.
    if ctx.try_ensure_synthetic_class(STREAM_COLLECTOR_CLASS, 2).is_err() {
        return cratonvm_native_collections::drain_spliterator_via_real_iterator(
            ctx,
            spliterator,
            SAFETY_CAP,
        );
    }
    let spl_pin = ctx.pin_native_root(spliterator);
    let collector = crate::try_alloc_concurrent_synthetic(ctx, STREAM_COLLECTOR_CLASS, 2)?;
    let col_pin = ctx.pin_native_root(collector);
    let initial = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
    let initial_pin = ctx.pin_native_root(initial);
    let mut collector = ctx.read_native_pin(col_pin, collector);
    let initial = ctx.read_native_pin(initial_pin, initial);
    ctx.set_field(collector, 0, Value::Object(Some(initial)));
    collector = ctx.read_native_pin(col_pin, collector);
    ctx.set_field(collector, 1, Value::Int(0));

    // Pin both objects — every `tryAdvance` re-enters Java and can trigger a
    // moving GC that relocates them (same discipline as `native_sl_stream`).
    const SAFETY_CAP: usize = 1_000_000;
    let mut produced = 0usize;
    let mut try_advance_dispatched = false;
    loop {
        let spl = ctx.read_native_pin(spl_pin, spliterator);
        let col = ctx.read_native_pin(col_pin, collector);
        match ctx.invoke_virtual(
            spl,
            "tryAdvance",
            "(Ljava/util/function/Consumer;)Z",
            &[Value::Object(Some(col))],
        ) {
            Ok(Some(Value::Int(v))) if v != 0 => {
                try_advance_dispatched = true;
                produced += 1;
                if produced >= SAFETY_CAP {
                    break;
                }
            }
            Ok(_) => {
                // false (or void-ish) → spliterator exhausted.
                break;
            }
            Err(e) => {
                if try_advance_dispatched {
                    // Genuine Java exception mid-drain — propagate.
                    ctx.unpin_native_roots(spl_pin);
                    return Err(e);
                }
                // First call failed (tryAdvance not dispatchable on this
                // receiver) — fall back to forEachRemaining.
                let spl = ctx.read_native_pin(spl_pin, spliterator);
                let col = ctx.read_native_pin(col_pin, collector);
                let _ = ctx.invoke_virtual(
                    spl,
                    "forEachRemaining",
                    "(Ljava/util/function/Consumer;)V",
                    &[Value::Object(Some(col))],
                );
                break;
            }
        }
    }

    // Snapshot to an exactly-sized array. Allocate the output FIRST, then
    // re-read the (pinned) collector — the allocation may move the heap.
    let col = ctx.read_native_pin(col_pin, collector);
    let len = match ctx.get_field(col, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let out = ctx.new_array(cratonvm_types::ArrayElementType::Reference, len);
    let out_pin = ctx.pin_native_root(out);
    let col = ctx.read_native_pin(col_pin, collector);
    let result = match ctx.get_field(col, 0) {
        Value::Object(Some(storage)) => {
            let storage_pin = ctx.pin_native_root(storage);
            for i in 0..len {
                let storage = ctx.read_native_pin(storage_pin, storage);
                let out = ctx.read_native_pin(out_pin, out);
                let v = ctx.get_array_element(storage, i);
                let value_pin = match v {
                    Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
                    _ => None,
                };
                let v = match (v, value_pin) {
                    (Value::Object(Some(object)), Some(pin)) => {
                        Value::Object(Some(ctx.read_native_pin(pin, object)))
                    }
                    (value, _) => value,
                };
                ctx.set_array_element(out, i, v);
                if let Some(pin) = value_pin {
                    ctx.unpin_native_roots(pin);
                }
            }
            ctx.read_native_pin(out_pin, out)
        }
        _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
    };
    // `spl_pin` is the first pin owned by this scope; one truncate also
    // releases the later collector pin without a redundant stale handle read.
    ctx.unpin_native_roots(spl_pin);
    Ok(result)
}

/// `Iterable.forEach(Consumer)` with a `ServiceLoader` receiver. Without this
/// registration the call lands on the generic Collection-interface bridge
/// (`native_al_for_each`), which only understands list-like field layouts and
/// silently iterates ZERO providers — JUnit 6's
/// `LauncherFactory.collectTestEngines` consumes the engine registry via
/// `Iterable.forEach(engines::add)` and then fails with "Cannot create
/// Launcher without at least one TestEngine" even though `iterator()` /
/// `stream()` on the same loader yield the provider. Drive the provider
/// iterator (the same machinery as `native_sl_iterator`) instead.
fn native_sl_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let action = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let action_pin = ctx.pin_native_root(action);
    let it = match native_sl_iterator(ctx, &args[..1])? {
        Some(Value::Object(Some(i))) => i,
        _ => {
            ctx.unpin_native_roots(action_pin);
            return Ok(None);
        }
    };
    let it_pin = ctx.pin_native_root(it);
    let result = loop {
        let it_cur = ctx.read_native_pin(it_pin, it);
        match ctx.invoke_virtual(it_cur, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(v))) if v != 0 => {}
            Ok(_) => break Ok(None),
            Err(e) => break Err(e),
        }
        let it_cur = ctx.read_native_pin(it_pin, it);
        let elem = match ctx.invoke_virtual(it_cur, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(v)) => v,
            Ok(None) => Value::Object(None),
            Err(e) => break Err(e),
        };
        let elem_pin = match elem {
            Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
            _ => None,
        };
        let action_cur = ctx.read_native_pin(action_pin, action);
        let elem = match (elem, elem_pin) {
            (Value::Object(Some(original)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(pin, original)))
            }
            (value, _) => value,
        };
        let accept = ctx.invoke_virtual(action_cur, "accept", "(Ljava/lang/Object;)V", &[elem]);
        if let Some(pin) = elem_pin {
            ctx.unpin_native_roots(pin);
        }
        if let Err(e) = accept {
            break Err(e);
        }
    };
    ctx.unpin_native_roots(it_pin);
    ctx.unpin_native_roots(action_pin);
    result
}

// JDK-ONLY-CLASSIFY: stub — stated for the whole registrar, not adjudicated
// per row. Every one of these was among the 200 registrations the real boot
// made with NO category scope over them, which `--dump-native-registry`
// could not report until `current_category` became an `Option`: the old
// `category_chosen` flag was set by the first `set_category` in boot and
// never cleared, so everything after it claimed to have been chosen.
// `SyntheticStub` is the kind these carried before and after — verified by
// a census A/B — and it is the right one on the merits: `java.util.ServiceLoader` is pure Java. These eleven
// were the sharp end of the measurement: with no scope over them the
// downgrade rule handed them the *previous* registration's `Bridge`, so
// `--jdk-only` would have started admitting a surface it had been
// refusing. Stating the kind is what keeps that from happening by
// accident in either direction.
pub fn register_service_loader_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let sl = "java/util/ServiceLoader";
    r.register(
        sl,
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        native_sl_load_class,
    );
    r.register(
        sl,
        "load",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
        native_sl_load_class_loader,
    );
    r.register(
        sl,
        "loadInstalled",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        native_sl_load_class,
    );
    r.register(sl, "iterator", "()Ljava/util/Iterator;", native_sl_iterator);
    r.register(
        sl,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_sl_for_each,
    );
    r.register(
        sl,
        "stream",
        "()Ljava/util/stream/Stream;",
        native_sl_stream,
    );
    r.register(
        sl,
        "spliterator",
        "()Ljava/util/Spliterator;",
        native_sl_spliterator,
    );
    r.register(
        sl,
        "findFirst",
        "()Ljava/util/Optional;",
        native_sl_find_first,
    );
    r.register(sl, "reload", "()V", native_sl_reload);

    // Re-register `StreamSupport.stream(Spliterator, boolean)` — see the
    // header comment on `native_stream_support_stream_from_spliterator`. This
    // must run LAST to win the registration race against the prior phase69
    // registration (which is synthetic-jdk-only and absent from real-JDK
    // builds anyway).
    r.register(
        "java/util/stream/StreamSupport",
        "stream",
        "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;",
        native_stream_support_stream_from_spliterator,
    );
    // The collecting consumer `drain_real_spliterator` hands to real
    // spliterators' `tryAdvance`/`forEachRemaining`. Must be registered here
    // (not only in phase69) so real-JDK builds can drain real spliterators.
    r.register(
        STREAM_COLLECTOR_CLASS,
        "accept",
        "(Ljava/lang/Object;)V",
        native_stream_collector_accept,
    );
    r.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_utils::mock_ctx;

    #[test]
    fn file_url_path_preserves_windows_drive_letter() {
        assert_eq!(
            file_url_path_to_fs_path(
                "file:/C:/craton/CratonVM/hibernate-core/META-INF/services/example.Service"
            ),
            Some("C:/craton/CratonVM/hibernate-core/META-INF/services/example.Service".to_string())
        );
        assert_eq!(
            file_url_path_to_fs_path("file:/tmp/service%20descriptor"),
            Some("/tmp/service descriptor".to_string())
        );
    }

    #[test]
    fn provider_name_validation_accepts_fqn() {
        assert!(is_valid_provider_name("com.acme.Foo"));
        assert!(is_valid_provider_name("a.b$Inner"));
        assert!(is_valid_provider_name("Foo_1"));
    }

    #[test]
    fn provider_name_validation_rejects_junk() {
        assert!(!is_valid_provider_name(""));
        assert!(!is_valid_provider_name("foo bar"));
        assert!(!is_valid_provider_name("foo;bar"));
        assert!(!is_valid_provider_name("!foo"));
    }

    #[test]
    fn comment_stripping_drops_hash_suffix() {
        let raw = "com.acme.Foo  # comment";
        let token = raw.split('#').next().unwrap().trim();
        assert_eq!(token, "com.acme.Foo");
    }

    #[test]
    fn embedded_x_content_dependencies_use_the_x_content_archive() {
        assert_eq!(
            derive_impl_jar_module_names(
                "com.fasterxml.jackson.core.util.JsonRecyclerPools$ThreadLocalPool"
            ),
            vec!["x-content"]
        );
        assert_eq!(
            derive_impl_jar_module_names("org.yaml.snakeyaml.Yaml"),
            vec!["x-content"]
        );
    }

    /// WP1.8-narrow: the descriptor parser strips comments and blanks,
    /// rejects malformed lines, and returns valid FQNs unchanged. This
    /// is the load-bearing parser the iterator now uses (replacing the
    /// JDK BufferedReader chain that the open-sourced revision can't
    /// drive end-to-end).
    #[test]
    fn parse_provider_lines_handles_comments_blanks_and_junk() {
        let body = b"# header comment\n  com.acme.A  \n\ncom.acme.B # trailing\nfoo bar\n!bad\nGood$Inner\n";
        let mut out = Vec::new();
        parse_provider_lines(body, &mut out);
        assert_eq!(out, vec!["com.acme.A", "com.acme.B", "Good$Inner"]);
    }

    /// WP1.8-narrow: invalid UTF-8 bytes do not panic the parser; they
    /// are silently dropped. ServiceLoader descriptors are spec'd as
    /// UTF-8 but a malformed JAR shouldn't kill discovery for the rest
    /// of the classpath.
    #[test]
    fn parse_provider_lines_silently_drops_non_utf8() {
        let body: &[u8] = &[0xff, 0xfe, b'\n'];
        let mut out = Vec::new();
        parse_provider_lines(body, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn read_jar_url_entry_reads_exact_jar_descriptor() {
        let tmp = tempfile::tempdir().unwrap();
        let jar_path = tmp.path().join("svc.jar");
        let file = std::fs::File::create(&jar_path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        zw.start_file(
            "META-INF/services/com.acme.Service",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        use std::io::Write;
        zw.write_all(b"com.acme.Provider\n").unwrap();
        zw.finish().unwrap();

        let raw_path = jar_path.to_string_lossy();
        let synthetic_linux_url = format!(
            "jar:file://{}!/META-INF/services/com.acme.Service",
            raw_path.trim_start_matches('/')
        );
        let bytes = read_jar_url_entry(&synthetic_linux_url).unwrap();
        assert_eq!(bytes, b"com.acme.Provider\n");
    }
    /// Build a `ModifiedClassPathClassLoader`-shaped receiver: a
    /// `URLClassLoader` SUBCLASS (so it is not a builtin loader) whose own URL
    /// list is a single directory — recorded, and deliberately not holding the
    /// service descriptor.
    fn modified_classpath_loader(
        ctx: &mut crate::test_utils::MockNativeContext,
        own_dir: &std::path::Path,
    ) -> cratonvm_types::ObjectRef {
        let url_cid = ctx
            .ensure_class_initialized("java/net/URLClassLoader")
            .expect("URLClassLoader class");
        let mcpcl_cid = ctx
            .ensure_class_initialized(
                "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader",
            )
            .expect("ModifiedClassPathClassLoader class");
        ctx.set_superclass(mcpcl_cid, url_cid);
        // The mock resolves field names off an exact-class-name table, so a
        // SUBCLASS of URLClassLoader needs its inherited `ucp` declared or
        // `set_field_by_name` silently no-ops and the fixture would present a
        // loader with no URLs at all.
        ctx.set_declared_fields(
            mcpcl_cid,
            vec![cratonvm_native_api::FieldMetadata {
                name: "ucp".to_string(),
                descriptor: "Ljdk/internal/loader/URLClassPath;".to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: mcpcl_cid,
                is_static: false,
            }],
        );
        let mut new_ref = |ctx: &mut crate::test_utils::MockNativeContext, name: &str| match ctx
            .new_object(name)
            .unwrap()
        {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected {name} object, got {other:?}"),
        };
        let loader = new_ref(
            ctx,
            "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader",
        );
        let ucp = new_ref(ctx, "jdk/internal/loader/URLClassPath");
        let url = new_ref(ctx, "java/net/URL");
        let path = ctx.create_string(&own_dir.to_string_lossy());
        ctx.set_field(url, 3, Value::Object(Some(path)));
        ctx.set_field_by_name(loader, "ucp", Value::Object(Some(ucp)));
        let urls = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(urls, 0, Value::Object(Some(url)));
        ctx.set_field(ucp, crate::classloader::UCP_STASHED_URLS, Value::Object(Some(urls)));
        loader
    }

    fn service_loader_for(
        ctx: &mut crate::test_utils::MockNativeContext,
        service: &str,
        loader: Value,
    ) -> cratonvm_types::ObjectRef {
        let service_id = ctx
            .ensure_class_initialized(service)
            .expect("create service class");
        let service_mirror = ctx.get_class_mirror(service_id);
        match build_service_loader(ctx, Value::Object(Some(service_mirror)), loader)
            .expect("build ServiceLoader")
        {
            Some(Value::Object(Some(sl))) => sl,
            other => panic!("expected ServiceLoader object, got {other:?}"),
        }
    }

    /// The flat classpath scan must not re-add a provider registration that the
    /// receiver's own (exclusion-filtered) URL list does not carry.
    ///
    /// This is the `@ClassPathExclusions` leak: the descriptor came back from a
    /// process-wide scan, `loadClass` then correctly refused the class it named,
    /// and `ServiceLoader` raised `ServiceConfigurationError: ... Provider ...
    /// not found` where HotSpot discovers no providers at all.
    #[test]
    fn discover_providers_skips_flat_scan_for_a_loader_with_its_own_urls() {
        const SVC: &str = "org.slf4j.spi.SLF4JServiceProvider";
        let resource = format!("META-INF/services/{SVC}");
        let dir = tempfile::tempdir().expect("tempdir");

        let mut ctx = mock_ctx();
        // The process-wide classpath DOES carry the descriptor — without this
        // the assertion below could not fail even if the scan still ran.
        ctx.set_resource(
            &resource,
            b"ch.qos.logback.classic.spi.LogbackServiceProvider\n".to_vec(),
        );
        // `logback-classic.jar` also DECLARES this provider in its `module-info`.
        // On a real JVM that declaration is invisible to a class-path loader; the
        // VM-global module registry offered it to every caller, so the same
        // provider leaked back through a second door after the flat scan closed.
        ctx.set_module_providers(
            "org/slf4j/spi/SLF4JServiceProvider",
            vec!["ch/qos/logback/classic/spi/LogbackServiceProvider"],
        );

        let loader = modified_classpath_loader(&mut ctx, dir.path());
        assert!(
            crate::classloader::object_extends(&ctx, loader, "java/net/URLClassLoader"),
            "fixture loader must extend URLClassLoader"
        );
        assert!(
            crate::classloader::loader_owns_complete_resource_view(&ctx, loader),
            "fixture must present a URLClassLoader-family loader with recorded URLs"
        );
        let sl = service_loader_for(&mut ctx, "org/slf4j/spi/SLF4JServiceProvider", Value::Object(Some(loader)));
        // `discover_providers` reads the named `loader` field, then legacy slot 1.
        assert!(
            matches!(ctx.get_field_by_name(sl, "loader"), Value::Object(Some(_)))
                || matches!(ctx.get_field(sl, 1), Value::Object(Some(_))),
            "fixture must put the loader where discover_providers reads it"
        );

        let providers = discover_providers(&mut ctx, sl).expect("discover providers");
        assert!(
            providers.is_empty(),
            "a loader whose own URL list excludes the jar must discover no \
             providers from it; got {providers:?}"
        );
    }

    /// Control for the test above: with no loader to answer for itself, the flat
    /// classpath scan is still the discovery mechanism.
    #[test]
    fn discover_providers_keeps_flat_scan_without_a_scoped_loader() {
        const SVC: &str = "com.acme.Service";
        let resource = format!("META-INF/services/{SVC}");

        let mut ctx = mock_ctx();
        ctx.set_resource(&resource, b"com.acme.Provider\n".to_vec());
        let sl = service_loader_for(&mut ctx, "com/acme/Service", Value::Object(None));

        let providers = discover_providers(&mut ctx, sl).expect("discover providers");
        assert_eq!(
            providers,
            vec!["com.acme.Provider"],
            "the flat classpath scan must still serve loaders CratonVM has no URL view of"
        );
    }

    #[test]
    fn discover_providers_includes_jpms_module_provides_entries() {
        let mut ctx = mock_ctx();
        ctx.set_module_providers(
            "org/apache/lucene/codecs/Codec",
            vec!["org/apache/lucene/codecs/lucene104/Lucene104Codec"],
        );

        let service_id = ctx
            .ensure_class_initialized("org/apache/lucene/codecs/Codec")
            .expect("create service class");
        let service_mirror = ctx.get_class_mirror(service_id);
        let sl = match build_service_loader(
            &mut ctx,
            Value::Object(Some(service_mirror)),
            Value::Object(None),
        )
        .expect("build ServiceLoader")
        {
            Some(Value::Object(Some(sl))) => sl,
            other => panic!("expected ServiceLoader object, got {other:?}"),
        };

        let providers = discover_providers(&mut ctx, sl).expect("discover providers");
        assert_eq!(
            providers,
            vec!["org.apache.lucene.codecs.lucene104.Lucene104Codec"],
            "module-info `provides Codec with Lucene104Codec` must participate in ServiceLoader discovery",
        );
    }
}
