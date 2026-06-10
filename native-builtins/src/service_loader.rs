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
fn native_sl_load_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let service = match args.first() {
        Some(Value::Object(Some(o))) => Value::Object(Some(*o)),
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "ServiceLoader.load(null)".to_string(),
            }));
        }
    };
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
        Some(Value::Object(Some(t))) => {
            ctx.invoke(
                "java/lang/Thread",
                "getContextClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[Value::Object(Some(t))],
            )
            .ok()
            .and_then(|v| v)
            .unwrap_or(Value::Object(None))
        }
        _ => Value::Object(None),
    };
    build_service_loader(ctx, service, loader)
}

/// `ServiceLoader.load(Class, ClassLoader)` — pass through caller's loader.
fn native_sl_load_class_loader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    // Synthetic slots: [0]=serviceClass, [1]=loader.
    ctx.set_field(obj, 0, service);
    ctx.set_field(obj, 1, loader);
    // Dual-write for real-JDK ServiceLoader field names.
    ctx.set_field_by_name(obj, "service", service);
    ctx.set_field_by_name(obj, "loader", loader);
    Ok(Some(Value::Object(Some(obj))))
}

/// Derive candidate IMPL-JARS module directory names from a service/class FQN.
///
/// ES stores provider JARs as `IMPL-JARS/<module>/<ver>.jar` inside the outer
/// module JAR. The module name is kebab-case but the Java package component is
/// camelCase/lowercase. We maintain a small known table for ES core modules and
/// fall back to the first package component (lowercase) for unknown packages.
///
/// Example: `org.elasticsearch.xcontent.spi.XContentProvider` → `["x-content"]`
pub(crate) fn derive_impl_jar_module_names(fqn: &str) -> Vec<String> {
    const KNOWN: &[(&str, &str)] = &[
        ("org.elasticsearch.xcontent",   "x-content"),
        ("org.elasticsearch.xpack",      "x-pack"),
        ("org.elasticsearch.transport",  "transport"),
        ("org.elasticsearch.common",     "common"),
        ("org.elasticsearch.core",       "core"),
    ];
    for (prefix, module) in KNOWN {
        if fqn.starts_with(prefix) {
            return vec![module.to_string()];
        }
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

/// Try to load a class from the IMPL-JARS flat-directory layout.
///
/// ES stores `IMPL-JARS/<module>/<jar_name>/<classfile>` as individual entries
/// in the outer JAR. When a class is not on the flat classpath, this helper
/// derives the module name, reads LISTING.TXT, and scans each listed jar
/// directory for the class file. Returns the Class mirror on success.
///
/// Used from both `service_loader.rs` (load provider class) and
/// `classloader.rs` (class resolution fallback for inner / helper classes).
pub(crate) fn impl_jars_load_class(
    ctx: &mut dyn NativeContext,
    internal_name: &str,
) -> Option<cratonvm_types::ObjectRef> {
    let dotted = internal_name.replace('/', ".");
    let class_file = format!("{internal_name}.class");
    for module_name in derive_impl_jar_module_names(&dotted) {
        let listing_path = format!("IMPL-JARS/{module_name}/LISTING.TXT");
        let first_bytes = ctx.find_all_resource_bytes(&listing_path).into_iter().next();
        let Some(listing_bytes) = first_bytes.or_else(|| ctx.find_resource(&listing_path)) else {
            continue;
        };
        let listing_text = String::from_utf8_lossy(&listing_bytes).into_owned();
        for jar_name in listing_text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && l.ends_with(".jar"))
        {
            if let Some(class_bytes) =
                try_read_from_inner_jar(ctx, &module_name, jar_name, &class_file)
            {
                let opts =
                    cratonvm_native_api::DefineClassFull { skip_verification: true, ..Default::default() };
                if let Ok(cid) = ctx.define_class_full(internal_name, &class_bytes, 0, opts) {
                    return Some(ctx.get_class_mirror(cid));
                }
            }
        }
    }
    None
}

/// If the ServiceLoader carries a non-builtin ClassLoader, return it so the
/// caller can use `loader.loadClass(fqn)` to load provider classes that live
/// inside embedded JARs and are invisible to the flat `Class.forName(fqn)` scan.
fn sl_non_builtin_loader(
    ctx: &mut dyn NativeContext,
    sl: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    let v = match ctx.get_field_by_name(sl, "loader") {
        Value::Object(Some(r)) => Some(r),
        _ => match ctx.get_field(sl, 1) {
            Value::Object(Some(r)) => Some(r),
            _ => return None,
        },
    };
    v.and_then(|r| {
        let name = ctx.class_name_of_id(ctx.class_id_of_object(r)).unwrap_or_default();
        if crate::classloader::is_builtin_loader_class(&name) {
            None
        } else {
            Some(r)
        }
    })
}

/// Load a provider class by FQN, first via the flat `Class.forName(fqn)` scan
/// and, if that fails, via `loader.loadClass(fqn)` for non-builtin loaders
/// (e.g. ES's `EmbeddedImplClassLoader` which stores classes inside embedded
/// JAR trees invisible to the flat classpath scan).
fn load_provider_class(
    ctx: &mut dyn NativeContext,
    fqn: &str,
    loader: Option<cratonvm_types::ObjectRef>,
) -> Option<cratonvm_types::ObjectRef> {
    let name = ctx.create_string(fqn);
    if let Ok(Some(Value::Object(Some(c)))) = ctx.invoke(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(name))],
    ) {
        return Some(c);
    }
    // forName failed — try loader.loadClass(fqn) if we have a custom loader
    // (e.g. EmbeddedImplClassLoader for embedded-JAR provider classes).
    if let Some(loader_r) = loader {
        let name2 = ctx.create_string(fqn);
        if let Ok(Some(Value::Object(Some(c)))) = ctx.invoke_virtual(
            loader_r,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(name2))],
        ) {
            return Some(c);
        }
    }
    // Final fallback: IMPL-JARS nested-JAR scan.
    impl_jars_load_class(ctx, &fqn.replace('.', "/"))
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
    let service_class = match ctx.get_field_by_name(sl, "service") {
        Value::Object(Some(c)) => c,
        _ => match ctx.get_field(sl, 0) {
            Value::Object(Some(c)) => c,
            _ => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: "ServiceLoader: service class is null".to_string(),
                }));
            }
        },
    };
    let service_name_val = ctx.invoke(
        "java/lang/Class",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(service_class))],
    )?;
    let service_name = match service_name_val {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if service_name.is_empty() {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: "ServiceLoader: service.getName() returned null".to_string(),
        }));
    }
    let resource = format!("META-INF/services/{}", service_name);

    let mut providers: Vec<String> = Vec::new();

    // When the ServiceLoader was created with a user-defined ClassLoader
    // (e.g. ES's EmbeddedImplClassLoader which stores providers inside
    // embedded jar trees like IMPL-JARS/<module>/<ver>.jar/<path>), the
    // flat scan below won't find the descriptor at the top-level path.
    //
    // Delegate to loader.findResources(resource) → Enumeration<URL>,
    // then for each URL extract the JAR-entry path and read bytes directly,
    // mirroring what the real JDK ServiceLoader does via
    // LazyClassPathLookupIterator → loader.getResources(name).
    let loader_ref_opt: Option<cratonvm_types::ObjectRef> = {
        let v = match ctx.get_field_by_name(sl, "loader") {
            Value::Object(Some(r)) => Some(r),
            _ => match ctx.get_field(sl, 1) {
                Value::Object(Some(r)) => Some(r),
                _ => None,
            },
        };
        match v {
            Some(r) => {
                let cid = ctx.class_id_of_object(r);
                let name = ctx.class_name_of_id(cid).unwrap_or_default();
                if crate::classloader::is_builtin_loader_class(&name) {
                    None
                } else {
                    Some(r)
                }
            }
            None => None,
        }
    };

    if let Some(loader_r) = loader_ref_opt {
        let diag_sl = matches!(
            std::env::var("CRATONVM_DIAG_SERVICELOADER").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        );

        // Primary path: call loader.findResources(resource) → Enumeration<URL>,
        // then extract the entry path from each URL and read bytes directly.
        let res_name_val = Value::Object(Some(ctx.create_string(&resource)));
        let enum_res = ctx.invoke_virtual(
            loader_r,
            "findResources",
            "(Ljava/lang/String;)Ljava/util/Enumeration;",
            &[res_name_val],
        );
        if diag_sl {
            match &enum_res {
                Err(e) => eprintln!("[SL-LOADER-DBG] findResources Err: {e:?}"),
                Ok(None) => eprintln!("[SL-LOADER-DBG] findResources -> Ok(None)"),
                Ok(Some(Value::Object(None))) => {
                    eprintln!("[SL-LOADER-DBG] findResources -> Ok(null)")
                }
                Ok(Some(v)) => eprintln!("[SL-LOADER-DBG] findResources -> Ok(Some({v:?}))"),
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
                let url_r = match ctx.invoke_virtual(
                    enum_r,
                    "nextElement",
                    "()Ljava/lang/Object;",
                    &[],
                ) {
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
                    let bytes_list = ctx.find_all_resource_bytes(&entry_path);
                    for bytes in &bytes_list {
                        parse_provider_lines(bytes, &mut providers);
                    }
                    if bytes_list.is_empty() {
                        if let Some(bytes) = ctx.find_resource(&entry_path) {
                            parse_provider_lines(&bytes, &mut providers);
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

        // Fallback: if findResources failed or returned an empty Enumeration,
        // directly read the `jarMetas` field from the loader (an
        // EmbeddedImplClassLoader-like object) and call prefix() on each
        // JarMeta to construct the embedded resource path.  This bypasses the
        // URL/ClassLoader/invokedynamic chain entirely.
        if !found_via_enum {
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
                    let prefix_str = match ctx.invoke_virtual(
                        jm,
                        "prefix",
                        "()Ljava/lang/String;",
                        &[],
                    ) {
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
        // findResources / jarMetas attempts.  Covers:
        //   (a) findResources returned null/empty (found_via_enum=false, the common case
        //       for EmbeddedImplClassLoader when jarMetas is empty), and
        //   (b) findResources succeeded but the URL chain produced no bytes.
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
                let first_bytes =
                    ctx.find_all_resource_bytes(&listing_path).into_iter().next();
                let listing_bytes =
                    match first_bytes.or_else(|| ctx.find_resource(&listing_path)) {
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
    }

    // Flat classpath scan: providers listed directly at
    // META-INF/services/<svc> on the classpath (normal case for
    // non-embedded loaders and JDK built-in providers).
    let descriptors = ctx.find_all_resource_bytes(&resource);
    for bytes in &descriptors {
        parse_provider_lines(bytes, &mut providers);
    }
    // Test mocks may stub `find_resource` without populating the bytes list.
    if descriptors.is_empty() {
        if let Some(bytes) = ctx.find_resource(&resource) {
            parse_provider_lines(&bytes, &mut providers);
        }
    }

    providers.sort();
    providers.dedup();
    if matches!(
        std::env::var("CRATONVM_DIAG_SERVICELOADER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    ) {
        eprintln!(
            "[SL-DBG] ServiceLoader service={} loader_delegation={} descriptors={} providers={} ({:?})",
            service_name,
            loader_ref_opt.is_some(),
            descriptors.len(),
            providers.len(),
            providers
        );
    }
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

fn is_valid_provider_name(s: &str) -> bool {
    // JDK ServiceLoader spec: each line must be a fully-qualified
    // binary class name containing only Java identifier chars, `.`, and `$`.
    if s.is_empty() {
        return false;
    }
    s.chars().all(|c| {
        c.is_alphanumeric() || c == '.' || c == '_' || c == '$'
    })
}

/// `ServiceLoader.iterator()` — scan META-INF/services and return an
/// Iterator<Object> over instantiated providers.
fn native_sl_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sl = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Extract and pin the non-builtin loader BEFORE discover_providers (which
    // triggers GC via invoke). If the loader is not re-pinned it becomes stale.
    let loader_pin_opt: Option<(_, cratonvm_types::ObjectRef)> = sl_non_builtin_loader(ctx, sl)
        .map(|r| (ctx.pin_native_root(r), r));
    let providers = discover_providers(ctx, sl)?;

    // Build an ArrayList and populate with load_provider_class(fqn).newInstance().
    let al_cls = "java/util/ArrayList";
    let al_cid = ctx
        .ensure_class_initialized(al_cls)
        .map_err(|_| MethodCallFailed::InternalError(VmError::Internal {
            message: "ArrayList: not loaded".to_string(),
        }))?;
    let mut list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    ctx.invoke(al_cls, "<init>", "()V", &[Value::Object(Some(list))])?;
    // Pin the providers list as a GC root: the loop below repeatedly calls into
    // Java (forName / newInstance / add), each of which can trigger a moving-GC
    // collection that relocates `list`. Without re-reading the forwarded
    // reference, `add` would mutate a stale (reused) object and the loader would
    // silently produce zero providers (the keycloak `CryptoIntegration` "Not
    // able to load any cryptoProvider" failure under real BouncyCastle).
    let list_pin = ctx.pin_native_root(list);

    let diag = matches!(
        std::env::var("CRATONVM_DIAG_SERVICELOADER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    if diag {
        eprintln!("[SL-DBG] iterator() entering loop with {} providers", providers.len());
    }
    for fqn in providers {
        if diag {
            eprintln!("[SL-DBG]   instantiate provider={fqn}");
        }
        // Re-read the loader through the pin so it's valid after any GC triggered
        // by the previous iteration's forName/newInstance/add calls.
        let loader_cur = loader_pin_opt
            .as_ref()
            .map(|(pin, orig)| ctx.read_native_pin(*pin, *orig));
        let class = match load_provider_class(ctx, &fqn, loader_cur) {
            Some(c) => c,
            None => {
                if diag {
                    eprintln!("[SL-DBG]   skip (class not found for {fqn})");
                }
                continue;
            }
        };
        // newInstance via Class.getDeclaredConstructor() + Constructor.newInstance().
        let empty_types = ctx.new_ref_array(
            ctx.class_id_by_name("java/lang/Class")
                .unwrap_or(cratonvm_types::ClassId::new(0)),
            0,
        );
        let ctor = ctx
            .invoke(
                "java/lang/Class",
                "getDeclaredConstructor",
                "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
                &[Value::Object(Some(class)), Value::Object(Some(empty_types))],
            )
            .ok()
            .and_then(|v| v);
        let ctor = match ctor {
            Some(Value::Object(Some(c))) => c,
            _ => {
                if diag {
                    eprintln!("[SL-DBG]   skip (no zero-arg ctor): {fqn}");
                }
                continue;
            }
        };
        // setAccessible(true)
        let _ = ctx.invoke(
            "java/lang/reflect/AccessibleObject",
            "setAccessible",
            "(Z)V",
            &[Value::Object(Some(ctor)), Value::Int(1)],
        );
        let empty_args = ctx.new_ref_array(
            ctx.class_id_by_name("java/lang/Object")
                .unwrap_or(cratonvm_types::ClassId::new(0)),
            0,
        );
        let inst = ctx
            .invoke(
                "java/lang/reflect/Constructor",
                "newInstance",
                "([Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(ctor)), Value::Object(Some(empty_args))],
            )
            .ok()
            .and_then(|v| v);
        let inst = match inst {
            Some(Value::Object(Some(o))) => o,
            _ => {
                if diag {
                    eprintln!("[SL-DBG]   skip (newInstance returned null): {fqn}");
                }
                continue;
            }
        };
        // Re-read the (possibly forwarded) list reference before mutating it.
        list = ctx.read_native_pin(list_pin, list);
        ctx.invoke(
            al_cls,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(inst))],
        )?;
    }
    // Loop done: pick up the final forwarded list reference (still pinned).
    list = ctx.read_native_pin(list_pin, list);

    if diag {
        let size = ctx
            .invoke(al_cls, "size", "()I", &[Value::Object(Some(list))])
            .ok()
            .and_then(|v| v);
        eprintln!(
            "[SL-DBG] iterator() final list size={:?}",
            size
        );
        list = ctx.read_native_pin(list_pin, list);
    }
    let it = ctx.invoke(
        al_cls,
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(list))],
    )?;
    // The returned iterator now keeps `list` reachable via the Java object
    // graph, so the native pins can be released.
    ctx.unpin_native_roots(list_pin);
    if let Some((pin, _)) = loader_pin_opt {
        ctx.unpin_native_roots(pin);
    }
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
fn native_sl_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sl = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        // Null receiver → empty stream (not null) so downstream
        // `.count()` / `.filter()` natives have a valid receiver.
        _ => return alloc_synthetic_stream(ctx, &[]),
    };
    let diag = matches!(
        std::env::var("CRATONVM_DIAG_SERVICELOADER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );

    // Pin the ServiceLoader receiver — every `invoke` below can trigger a
    // moving GC that relocates it.
    let sl_pin = ctx.pin_native_root(sl);
    let sl_for_discover = ctx.read_native_pin(sl_pin, sl);
    let providers = discover_providers(ctx, sl_for_discover)?;
    if providers.is_empty() {
        ctx.unpin_native_roots(sl_pin);
        return alloc_synthetic_stream(ctx, &[]);
    }

    // Resolve the JDK-internal wrapper class. If it is unavailable (e.g. a
    // stripped runtime), fall back to draining service instances so the
    // stream is at least non-empty rather than crashing.
    const PROVIDER_IMPL: &str = "java/util/ServiceLoader$ProviderImpl";
    let pi_cid = match ctx.ensure_class_initialized(PROVIDER_IMPL) {
        Ok(cid) => cid,
        Err(_) => {
            ctx.unpin_native_roots(sl_pin);
            if diag {
                eprintln!("[SL-DBG] stream(): ProviderImpl unavailable, draining instances");
            }
            return drain_instances_to_stream(ctx, args);
        }
    };
    let pi_fields = ctx.class_num_total_fields(pi_cid).max(4);

    // Accumulate the wrappers in a pinned ArrayList so each is a GC root
    // while the loop keeps re-entering Java (forName / getDeclaredConstructor
    // / <init> all can collect). Mirrors `native_sl_iterator`'s discipline.
    let al_cls = "java/util/ArrayList";
    let al_cid = ctx
        .ensure_class_initialized(al_cls)
        .map_err(|_| MethodCallFailed::InternalError(VmError::Internal {
            message: "ArrayList: not loaded".to_string(),
        }))?;
    let mut list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    ctx.invoke(al_cls, "<init>", "()V", &[Value::Object(Some(list))])?;
    let list_pin = ctx.pin_native_root(list);

    for fqn in &providers {
        // Re-read sl through the pin so we have a fresh reference after
        // GC may have moved it in a previous iteration.
        let sl_cur = ctx.read_native_pin(sl_pin, sl);
        // For embedded-JAR providers (e.g. ES EmbeddedImplClassLoader),
        // Class.forName(fqn) uses the flat classpath and won't find the class.
        // Pass the loader so load_provider_class can fall back to loadClass.
        let loader_cur = sl_non_builtin_loader(ctx, sl_cur);
        let type_class = match load_provider_class(ctx, fqn, loader_cur) {
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

        // type.getDeclaredConstructor() → the no-arg ctor used by get().
        let empty_types = ctx.new_ref_array(
            ctx.class_id_by_name("java/lang/Class")
                .unwrap_or(cratonvm_types::ClassId::new(0)),
            0,
        );
        let type_now = ctx.read_native_pin(type_pin, type_class);
        let ctor = match ctx.invoke(
            "java/lang/Class",
            "getDeclaredConstructor",
            "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
            &[Value::Object(Some(type_now)), Value::Object(Some(empty_types))],
        ) {
            Ok(Some(Value::Object(Some(c)))) => c,
            other => {
                if diag {
                    eprintln!("[SL-DBG]   stream skip (no no-arg ctor for {fqn} → {other:?})");
                }
                // Release this iteration's pins, keep sl + list.
                ctx.unpin_native_roots(type_pin);
                continue;
            }
        };
        let ctor_pin = ctx.pin_native_root(ctor);
        // setAccessible(true) so ProviderImpl.get()'s reflective newInstance
        // succeeds for non-public providers.
        let ctor_now = ctx.read_native_pin(ctor_pin, ctor);
        let _ = ctx.invoke(
            "java/lang/reflect/AccessibleObject",
            "setAccessible",
            "(Z)V",
            &[Value::Object(Some(ctor_now)), Value::Int(1)],
        );

        // Read everything back post-GC for the wrapper construction.
        let sl_now = ctx.read_native_pin(sl_pin, sl);
        let service = match ctx.get_field_by_name(sl_now, "service") {
            v @ Value::Object(Some(_)) => v,
            _ => ctx.get_field(sl_now, 0),
        };
        let type_final = ctx.read_native_pin(type_pin, type_class);
        let ctor_final = ctx.read_native_pin(ctor_pin, ctor);

        // new ServiceLoader$ProviderImpl(service, type, ctor, acc) — the
        // classpath-flavour constructor (factoryMethod = null, acc = null).
        // JDK 22: descriptor includes AccessControlContext as the 4th param.
        let provider = ctx.alloc_object(pi_cid, pi_fields);
        let provider_pin = ctx.pin_native_root(provider);
        if let Err(e) = ctx.invoke(
            PROVIDER_IMPL,
            "<init>",
            "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/reflect/Constructor;Ljava/security/AccessControlContext;)V",
            &[
                Value::Object(Some(provider)),
                service,
                Value::Object(Some(type_final)),
                Value::Object(Some(ctor_final)),
                Value::Object(None), // acc = null (deprecated in JDK 17+)
            ],
        ) {
            if diag {
                eprintln!("[SL-DBG]   stream skip (ProviderImpl <init> {fqn} → {e:?})");
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
            let cid = ctx
                .ensure_class_initialized("java/util/stream/Stream")
                .unwrap_or(cratonvm_types::ClassId::new(0));
            let nfields = ctx.class_num_total_fields(cid).max(1);
            let s = ctx.alloc_object(cid, nfields);
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
fn drain_instances_to_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let it = native_sl_iterator(ctx, args)?;
    let iter_obj = match it {
        Some(Value::Object(Some(o))) => o,
        _ => return alloc_synthetic_stream(ctx, &[]),
    };
    let mut collected: Vec<Value> = Vec::new();
    const SAFETY_CAP: usize = 1_000_000;
    loop {
        let has_next = ctx.invoke_virtual(iter_obj, "hasNext", "()Z", &[]);
        if !matches!(has_next, Ok(Some(Value::Int(1)))) {
            break;
        }
        let next = ctx.invoke_virtual(iter_obj, "next", "()Ljava/lang/Object;", &[]);
        match next {
            Ok(Some(v)) => collected.push(v),
            _ => break,
        }
        if collected.len() >= SAFETY_CAP {
            break;
        }
    }
    alloc_synthetic_stream(ctx, &collected)
}

fn native_sl_find_first(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let it = native_sl_iterator(ctx, args)?;
    let iter_obj = match it {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return empty_optional(ctx);
        }
    };
    let has_next = ctx
        .invoke(
            "java/util/Iterator",
            "hasNext",
            "()Z",
            &[Value::Object(Some(iter_obj))],
        )?;
    if matches!(has_next, Some(Value::Int(0)) | None) {
        return empty_optional(ctx);
    }
    let first = ctx.invoke(
        "java/util/Iterator",
        "next",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(iter_obj))],
    )?;
    let first_obj = match first {
        Some(Value::Object(Some(o))) => o,
        _ => return empty_optional(ctx),
    };
    let opt = ctx.invoke(
        "java/util/Optional",
        "of",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        &[Value::Object(Some(first_obj))],
    )?;
    Ok(opt)
}

fn empty_optional(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let empty = ctx.invoke(
        "java/util/Optional",
        "empty",
        "()Ljava/util/Optional;",
        &[],
    )?;
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
fn native_sl_spliterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
            let cid = ctx.ensure_class_initialized("java/util/Spliterator")
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
    loop {
        let has_next = ctx.invoke_virtual(iter, "hasNext", "()Z", &[]);
        if !matches!(has_next, Ok(Some(Value::Int(1)))) {
            break;
        }
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
    let cid = ctx.ensure_class_initialized("java/util/Spliterator")
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
/// registration of the same triple is, for reasons specific to this binary
/// (very large source file, incremental-compile interaction) not making it
/// into the final `cratonvm.exe`: stress-checking the binary with `grep -ao`
/// over the literal string `"[STREAM-SUPPORT-DBG]"` shows it absent, and
/// runtime traces of `ServiceLoader.load(...).spliterator().stream()
/// .filter(...)` from Elasticsearch's `CliToolProvider.load` never trigger
/// the registered native — `Stream.filter` runs on real-JDK ReferencePipeline
/// bytecode against an empty stream, surfacing as
///   `AssertionError: CliToolProvider [server] not found, available names are []`
/// even though our native `ServiceLoader.iterator()` had just yielded
/// 13 providers.
///
/// Re-registering here (from a small, less-noisy translation unit) ensures
/// the closure is the LAST writer to the `NativeMethodRegistry` HashMap for
/// the `(StreamSupport, stream, (Spliterator,Z)Stream)` triple. The body
/// drains the supplied synthetic spliterator's backing Object[] into a fresh
/// synthetic Stream so the downstream `Stream.filter` / `Stream.toList`
/// natives see the real provider list.
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
    // Read field 0 of the spliterator. Our `Spliterators.spliteratorUnknownSize`
    // and `ServiceLoader.spliterator()` natives both place a fully-materialised
    // Object[] in field 0 — read it directly. For real-JDK Spliterator subclasses
    // whose field 0 isn't an array, fall back to draining via
    // `forEachRemaining(Consumer)`.
    let field0 = ctx.get_field(spliterator, 0);
    let arr = match field0 {
        Value::Object(Some(a))
            if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => a,
        _ => return drain_spliterator_to_stream(ctx, spliterator),
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
    let snapshot = ctx.new_array(cratonvm_types::ArrayElementType::Reference, n);
    for i in 0..n {
        let v = ctx.get_array_element(arr, pos + i);
        ctx.set_array_element(snapshot, i, v);
    }
    let cid = ctx.ensure_class_initialized("java/util/stream/Stream")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let nfields = ctx.class_num_total_fields(cid).max(1);
    let stream = ctx.alloc_object(cid, nfields);
    ctx.set_field(stream, 0, Value::Object(Some(snapshot)));
    Ok(Some(Value::Object(Some(stream))))
}

fn alloc_synthetic_stream(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
) -> MethodCallResult {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, elems.len());
    for (i, v) in elems.iter().enumerate() {
        ctx.set_array_element(arr, i, *v);
    }
    let cid = ctx.ensure_class_initialized("java/util/stream/Stream")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let nfields = ctx.class_num_total_fields(cid).max(1);
    let stream = ctx.alloc_object(cid, nfields);
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(stream))))
}

fn drain_spliterator_to_stream(
    ctx: &mut dyn NativeContext,
    spliterator: cratonvm_types::ObjectRef,
) -> MethodCallResult {
    // Best-effort drain via `tryAdvance(Consumer)` — bounded.
    let mut collected: Vec<Value> = Vec::new();
    const SAFETY_CAP: usize = 1_000_000;
    // We can't pass a closure to JDK code; instead, repeatedly call
    // `tryAdvance` and rely on the side-effect of advancing the spliterator's
    // cursor while a no-op consumer absorbs the element. Since we don't have a
    // way to inject a side-channel consumer here, fall through to an empty
    // stream rather than risk infinite-looping a misbehaving spliterator.
    let _ = (spliterator, &mut collected, SAFETY_CAP);
    alloc_synthetic_stream(ctx, &[])
}

pub fn register_service_loader_natives(r: &mut NativeMethodRegistry) {
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
    r.register(sl, "stream", "()Ljava/util/stream/Stream;", native_sl_stream);
    r.register(sl, "spliterator", "()Ljava/util/Spliterator;", native_sl_spliterator);
    r.register(sl, "findFirst", "()Ljava/util/Optional;", native_sl_find_first);

    // Re-register `StreamSupport.stream(Spliterator, boolean)` — see the
    // header comment on `native_stream_support_stream_from_spliterator`. This
    // must run LAST to win the registration race against the prior phase69
    // registration that doesn't survive linking in this binary.
    r.register(
        "java/util/stream/StreamSupport",
        "stream",
        "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;",
        native_stream_support_stream_from_spliterator,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
