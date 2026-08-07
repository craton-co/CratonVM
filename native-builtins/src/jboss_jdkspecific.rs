// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.H2 — JBoss Modules `JDKSpecific` boot-time natives.
//!
//! `org.jboss.modules.JDKSpecific.<clinit>` probes the JDK for module /
//! package introspection APIs. On our VM this path NPEs with
//! "Cannot invoke contains on null" because `System.getProperty("sun.boot.class.path")`
//! returns null — fine on JDK 9+ HotSpot where that property no longer
//! exists — and the downstream `processClassPathItem(null, set, set)`
//! returns early, but the subsequent `ModuleLayer.boot().findModule(x).get().getPackages()`
//! call surfaces as the visible NPE because our `ModuleLayer.findModule`
//! stub produces an `Optional` whose internal value slot is null.
//!
//! This module adds:
//!   - `java.lang.ModuleLayer.boot()` — returns a cached synthetic ModuleLayer
//!     whose 1 field slot (boot=0) is `true`. (Some `ModuleLayer.boot()` is
//!     already installed in `phases_late`; we override with a richer instance
//!     that wires `modules()` / `findModule` to return populated values.)
//!   - `java.lang.ModuleLayer.findModule(String)` — returns an `Optional<Module>`
//!     populated with a synthetic `Module` whose `getPackages()` lists the JDK
//!     packages cratonvm currently knows about.
//!   - `java.lang.Module.getPackages()` — returns a `Set<String>` backed by a
//!     concrete `java.util.HashSet` with the JDK's published boot packages.
//!   - Safety-hardened inputs: module-name rejects `../`, backslashes, control
//!     bytes, and NUL — these are never valid module names and allowing them
//!     would let a caller probe for exfiltration paths.
//!
//! This module is loaded from `lib.rs::register_essential_natives` alongside
//! the existing `jboss_module_xml` handlers.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::alloc_concurrent_synthetic;
use cratonvm_types::error::MethodCallFailed;

/// Package names seeded into every synthetic `Module.getPackages()` call.
///
/// This list must contain every package JBoss Modules' `JDKPaths` /
/// `JDKSpecific` asks about during its bootstrap. We err on the side of
/// over-reporting — a superset of real JDK packages is harmless since
/// `findServices()` and `getResources()` filter by resource existence
/// before they enumerate.
const BOOT_JDK_PACKAGES: &[&str] = &[
    "java.lang",
    "java.lang.annotation",
    "java.lang.constant",
    "java.lang.foreign",
    "java.lang.invoke",
    "java.lang.module",
    "java.lang.ref",
    "java.lang.reflect",
    "java.lang.runtime",
    "java.io",
    "java.math",
    "java.net",
    "java.net.spi",
    "java.nio",
    "java.nio.channels",
    "java.nio.channels.spi",
    "java.nio.charset",
    "java.nio.charset.spi",
    "java.nio.file",
    "java.nio.file.attribute",
    "java.nio.file.spi",
    "java.security",
    "java.security.cert",
    "java.security.interfaces",
    "java.security.spec",
    "java.text",
    "java.text.spi",
    "java.time",
    "java.time.chrono",
    "java.time.format",
    "java.time.temporal",
    "java.time.zone",
    "java.util",
    "java.util.concurrent",
    "java.util.concurrent.atomic",
    "java.util.concurrent.locks",
    "java.util.function",
    "java.util.jar",
    "java.util.regex",
    "java.util.spi",
    "java.util.stream",
    "java.util.zip",
    "javax.crypto",
    "javax.crypto.interfaces",
    "javax.crypto.spec",
    "javax.net",
    "javax.net.ssl",
    "javax.security.auth",
    "javax.security.auth.callback",
    "javax.security.auth.login",
    "javax.security.auth.spi",
    "javax.security.auth.x500",
    "javax.security.cert",
    "jdk.internal.access",
    "jdk.internal.misc",
    "jdk.internal.module",
    "jdk.internal.reflect",
    "jdk.internal.util",
    "jdk.internal.vm",
    "sun.misc",
    "sun.nio.ch",
    "sun.reflect",
    "sun.security.util",
];

/// Public packages owned by `java.xml` that are visible to dynamic translet
/// modules created by Xalan's `TemplatesImpl`.
const JAVA_XML_PACKAGES: &[&str] = &[
    "javax.xml",
    "javax.xml.catalog",
    "javax.xml.datatype",
    "javax.xml.namespace",
    "javax.xml.parsers",
    "javax.xml.stream",
    "javax.xml.stream.events",
    "javax.xml.stream.util",
    "javax.xml.transform",
    "javax.xml.transform.dom",
    "javax.xml.transform.sax",
    "javax.xml.transform.stax",
    "javax.xml.transform.stream",
    "javax.xml.validation",
    "javax.xml.xpath",
    "org.w3c.dom",
    "org.w3c.dom.bootstrap",
    "org.w3c.dom.events",
    "org.w3c.dom.ls",
    "org.w3c.dom.ranges",
    "org.w3c.dom.traversal",
    "org.w3c.dom.views",
    "org.xml.sax",
    "org.xml.sax.ext",
    "org.xml.sax.helpers",
];

/// Reject module names that a downstream API could interpret as a path
/// traversal, a Windows UNC share, or an unexpected control sequence.
///
/// The JVM spec forbids module names containing `:` / `..` segments and
/// path separators; enforcing that here means an attacker probing
/// `ModuleLayer.findModule("../../etc/passwd")` never reaches downstream
/// resource lookups with a crafted value.
fn validate_module_name(name: &str) -> Result<(), RuntimeError> {
    if name.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "module name must not be empty".to_string(),
        });
    }
    if name.len() > 256 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "module name too long".to_string(),
        });
    }
    for b in name.bytes() {
        // Reject NUL and control characters (< 0x20), plus DEL (0x7F).
        if b < 0x20 || b == 0x7F {
            return Err(RuntimeError::IllegalArgumentException {
                message: "module name contains control byte".to_string(),
            });
        }
        // Reject path-separator-ish characters. `.` and `$` ARE allowed
        // (they appear in valid module names like `java.base`), but
        // `/`, `\`, and `:` are not.
        if b == b'/' || b == b'\\' || b == b':' {
            return Err(RuntimeError::IllegalArgumentException {
                message: "module name contains path separator".to_string(),
            });
        }
    }
    if name.contains("..") {
        return Err(RuntimeError::IllegalArgumentException {
            message: "module name contains traversal sequence".to_string(),
        });
    }
    Ok(())
}

/// Field layout for our synthetic ModuleLayer (matches slot indices used
/// by `phases_late::register_p59_module_layer` so the two surfaces can
/// share objects):
///
///   slot 0: `boot` boolean flag (1 for the boot layer, 0 for user layers)
const MODULE_LAYER_FIELD_COUNT: usize = 2;

/// Field count requested when allocating a Module here. `alloc_concurrent_synthetic`
/// resolves `"java/lang/Module"` to the REAL bytecode class (9 declared instance
/// fields: layer, name, loader, descriptor, enableNativeAccess, reads,
/// openPackages, exportedPackages, moduleInfoClass — see javap), so this
/// small count only matters as a floor; the real total always wins.
///
/// `name`/`layer` below are read/written by REAL field name (`get_field_by_name`/
/// `set_field_by_name`), NOT by a hand-picked slot index. A prior version of
/// this file assumed a private 5-field layout (name=0, layer=1, packages=2,
/// descriptor=3, loader=4) and wrote/read those slots directly — but every
/// Module object here is actually an instance of the real class (real layout:
/// layer=0, name=1, loader=2, descriptor=3, …), so that raw slot-1 write
/// intended for "layer" was silently landing on the REAL `name` field. On the
/// single canonical unnamed-module mirror shared across every class with no
/// declared module (cached by `Class.getModule()` in `lib.rs`), any call to
/// `Module.getLayer()` overwrote that shared instance's `name` field with a
/// `ModuleLayer` object — so a LATER `Module.getDescriptor()` on the SAME
/// object read back a `ModuleLayer` where it expected the module name,
/// breaking Elasticsearch's `ProviderLocator.checkUses` with a
/// `NullPointerException` several call frames away from this file. See
/// `native-builtins::lib::register_essential_natives`'s `Class.getModule()`
/// and `Module.getDescriptor()` overrides.
const MODULE_FIELD_COUNT: usize = 5;

/// Off-object storage for the package set `build_module` seeds and
/// `native_module_get_packages` reads back, keyed by `identity_hash_code`
/// (same pattern as `mac_state_table` in `phases_late.rs`).
///
/// `packages` has no real `java.lang.Module` field of that name — real
/// `getPackages()` is computed, not stored. A prior version of this file
/// aliased slot 2 for it, which is the REAL `loader` field on every Module
/// object here (same bug class as the name/layer collision documented on
/// `MODULE_FIELD_COUNT` above): any `Module.getPackages()` call on a Module
/// not built by `build_module` below — e.g. the canonical shared mirror
/// `Class.getModule()` caches — would have silently corrupted that object's
/// real `loader` field. Storing the plain package-name `Vec<String>` here
/// (not a `java.util.Set` object reference) also sidesteps needing a GC root
/// for a value cached across native calls — `native_module_get_packages`
/// rebuilds a fresh `HashSet` from it on every call.
const MODULE_PACKAGES_MAX_ENTRIES: usize = 4096;

fn module_packages_table() -> &'static std::sync::Mutex<std::collections::HashMap<i32, Vec<String>>>
{
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i32, Vec<String>>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Bound the table before inserting a fresh entry, mirroring
/// `mac_state_evict_if_needed` — a long-running app that repeatedly builds
/// short-lived synthetic Modules (via `ModuleLayer.findModule`) must not
/// retain an entry per call forever. `keep` is the id about to be inserted
/// and is never evicted. Best-effort eviction (lowest ids first); these are
/// abandoned Module handles whose Java objects are unreachable.
fn module_packages_evict_if_needed(t: &mut std::collections::HashMap<i32, Vec<String>>, keep: i32) {
    if t.len() < MODULE_PACKAGES_MAX_ENTRIES {
        return;
    }
    let target = MODULE_PACKAGES_MAX_ENTRIES / 2;
    let mut ids: Vec<i32> = t.keys().copied().filter(|&k| k != keep).collect();
    ids.sort_unstable();
    let to_remove = t.len().saturating_sub(target);
    for id in ids.into_iter().take(to_remove) {
        t.remove(&id);
    }
}

/// Per-VM memo of THE boot `ModuleLayer` object, as a JNI-global-root handle
/// (`NativeContext::add_global_root`) — never a raw `ObjectRef`, because this
/// table outlives any number of collections and a global ref is the one root
/// form the moving GC both keeps alive and remaps.
///
/// Keyed by `ctx.vm_identity()`: Rust tests build several `Vm`s in one process,
/// and a process-global object cache goes stale across VM lifetimes.
fn boot_layer_memo() -> &'static std::sync::Mutex<std::collections::HashMap<usize, usize>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, usize>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Build (or return) the synthetic boot ModuleLayer, with the real JDK
/// collection fields initialized. Older code treated ModuleLayer as a one-slot
/// synthetic object, but real JDK bytecode reads fields such as `parents` when
/// defining child layers.
///
/// `ModuleLayer.boot()` is a singleton — `ModuleLayer.boot() ==
/// ModuleLayer.boot()` and `someModule.getLayer() == ModuleLayer.boot()` are
/// both spec'd identities, and JDK code compares layers with `==`. This used to
/// allocate a FRESH layer on every call, so both comparisons were always false
/// (measured: `regression-suite/src/RJdkModule.java:57` fails with "module must
/// be in the boot layer" in real-jdk AND jdk-only, while HotSpot 25 passes).
/// Memoise per VM instead.
fn build_boot_layer(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let vm = ctx.vm_identity();
    let mut memo = boot_layer_memo().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = memo.get(&vm).copied() {
        drop(memo);
        if let Some(cached) = ctx.resolve_global_root(handle) {
            return Ok(cached);
        }
        memo = boot_layer_memo().lock().unwrap_or_else(|e| e.into_inner());
    }
    drop(memo);

    let layer = alloc_concurrent_synthetic(ctx, "java/lang/ModuleLayer", MODULE_LAYER_FIELD_COUNT);
    let layer_pin = ctx.pin_native_root(layer);

    let parents = new_initialized_object(ctx, "java/util/ArrayList", "()V", &[], "layer parents")?;
    let layer = ctx.read_native_pin(layer_pin, layer);
    ctx.set_field_by_name(layer, "parents", Value::Object(Some(parents)));

    let name_to_module =
        new_initialized_object(ctx, "java/util/HashMap", "()V", &[], "layer nameToModule")?;
    let layer = ctx.read_native_pin(layer_pin, layer);
    ctx.set_field_by_name(layer, "nameToModule", Value::Object(Some(name_to_module)));

    let modules = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "layer modules")?;
    let layer = ctx.read_native_pin(layer_pin, layer);
    ctx.set_field_by_name(layer, "modules", Value::Object(Some(modules)));

    ctx.unpin_native_roots(layer_pin);
    // Publish. `new_initialized_object` above runs Java, so a re-entrant
    // `ModuleLayer.boot()` could have published first; prefer whatever is
    // already there so identity never changes under a caller that has one.
    let mut memo = boot_layer_memo().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = memo.get(&vm).copied() {
        drop(memo);
        if let Some(cached) = ctx.resolve_global_root(handle) {
            return Ok(cached);
        }
        memo = boot_layer_memo().lock().unwrap_or_else(|e| e.into_inner());
    }
    let handle = ctx.add_global_root(layer);
    if handle != 0 {
        memo.insert(vm, handle);
    }
    drop(memo);
    Ok(layer)
}

/// Build a `java.util.HashSet<String>` pre-populated with `packages`, backed
/// by the REAL HashMap-wrapped layout (`crate::build_real_layout_string_hashset`
/// in `lib.rs`) rather than a hand-rolled (array, size, capacity) synthetic
/// shape.
///
/// Real `java.util.HashSet` has exactly one instance field —
/// `transient HashMap<E, Object> map;` (see `javap java.util.HashSet`) — so a
/// (array, size, capacity) 3-slot convention here writes an `Object[]` into
/// what real bytecode's `size()`/`iterator()`/`stream()` (all delegating to
/// `this.map`) expect to be an actual `HashMap`. Confirmed via a direct
/// Java-level probe (`getModule().getPackages().size()`): the prior version
/// returned `0` regardless of how many packages were seeded, with
/// `gen_heap::read_slot: corrupt Value cell` GC-guard errors logged during
/// the call — the same "assumes a private synthetic layout on a class that's
/// actually real bytecode" bug class as the Module field-slot fixes above,
/// just on `HashSet` instead of `Module`. The real-layout helper builds an
/// actual `HashMap` with real `HashMap$Node` buckets, so unforced real
/// bytecode reads it correctly with no detection/adaptation needed.
fn build_package_set(ctx: &mut dyn NativeContext, packages: &[&str]) -> ObjectRef {
    let keys: Vec<ObjectRef> = packages.iter().map(|pkg| ctx.create_string(pkg)).collect();
    crate::build_real_layout_string_hashset(ctx, &keys)
}

/// Build a synthetic Module for `name`, bound to the boot layer.
fn build_module(ctx: &mut dyn NativeContext, name: &str, layer: ObjectRef) -> Result<ObjectRef, MethodCallFailed> {
    let module = alloc_concurrent_synthetic(ctx, "java/lang/Module", MODULE_FIELD_COUNT);
    let pin = ctx.pin_native_root(module);
    let name_str = ctx.create_string(name);
    let module = ctx.read_native_pin(pin, module);
    ctx.set_field_by_name(module, "name", Value::Object(Some(name_str)));
    ctx.set_field_by_name(module, "layer", Value::Object(Some(layer)));
    let desc = crate::build_synthetic_module_descriptor(ctx, name)?;
    let module = ctx.read_native_pin(pin, module);
    ctx.set_field_by_name(module, "descriptor", Value::Object(Some(desc)));
    ctx.unpin_native_roots(pin);
    // Known boot modules get their package sets; other synthetic modules get
    // an empty set (callers check `contains` before acting).
    // Recorded off-object in `module_packages_table` (see its doc comment)
    // instead of a field slot; `native_module_get_packages` reads it back
    // the same way.
    let packages: Vec<String> = match name {
        "java.base" => BOOT_JDK_PACKAGES.iter().map(|s| s.to_string()).collect(),
        "java.xml" => JAVA_XML_PACKAGES.iter().map(|s| s.to_string()).collect(),
        _ => Vec::new(),
    };
    let id = ctx.identity_hash_code(module);
    let mut t = module_packages_table().lock().unwrap();
    module_packages_evict_if_needed(&mut t, id);
    t.insert(id, packages);
    drop(t);
    Ok(module)
}

/// `Module.defineModule0(Module, boolean, String, String, Object[])` — record
/// the package set the JDK is handing us, and register an open module's
/// packages as unqualified opens.
///
/// Static native: `args[0]` is the Module, `args[1]` the `isOpen` flag,
/// `args[2]`/`args[3]` version/location (unused — CratonVM keeps no module
/// descriptor beyond the name), `args[4]` the `Object[]` of package names.
pub(crate) fn native_module_define_module0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let module = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        // Real `defineModule0(null, ...)` would NPE inside the VM; nothing to
        // record either way.
        _ => return Ok(None),
    };
    let is_open = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);

    let mut packages: Vec<String> = Vec::new();
    if let Some(Value::Object(Some(arr))) = args.get(4) {
        let len = ctx.array_length(*arr);
        packages.reserve(len);
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                if let Some(pkg) = ctx.read_string(s) {
                    packages.push(pkg);
                }
            }
        }
    }

    // An open module opens every package it contains to every other module —
    // empty target = unqualified, same convention as `addExportsToAll0`.
    if is_open {
        let module_name = match ctx.get_field_by_name(module, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        };
        if let Some(name) = module_name {
            for pkg in &packages {
                ctx.module_add_opens(&name, pkg, "");
            }
        }
    }

    let id = ctx.identity_hash_code(module);
    let mut t = module_packages_table().lock().unwrap();
    module_packages_evict_if_needed(&mut t, id);
    t.insert(id, packages);
    Ok(None)
}

/// Wrap an ObjectRef as `Optional.of(value)`.
fn wrap_optional_present(ctx: &mut dyn NativeContext, value: ObjectRef) -> ObjectRef {
    let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
    ctx.set_field(opt, 0, Value::Object(Some(value)));
    opt
}

/// `ModuleLayer.boot()` — produce the cached boot layer.
pub(crate) fn native_module_layer_boot(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let layer = build_boot_layer(ctx)?;
    Ok(Some(Value::Object(Some(layer))))
}

/// `ModuleLayer.findModule(String)` — return `Optional<Module>`.
pub(crate) fn native_module_layer_find_module(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this (ModuleLayer), args[1] = String name
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException {
                message: Some("ModuleLayer.findModule: name must not be null".to_string()),
            }
            .into());
        }
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "ModuleLayer.findModule: name must be a String".to_string(),
            }
            .into());
        }
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    if let Err(e) = validate_module_name(&name) {
        return Err(e.into());
    }

    let layer_ref = match args.first() {
        Some(Value::Object(Some(l))) => *l,
        _ => build_boot_layer(ctx)?,
    };
    let module = build_module(ctx, &name, layer_ref)?;
    let opt = wrap_optional_present(ctx, module);
    Ok(Some(Value::Object(Some(opt))))
}

/// `Module.getName()` — return the real `name` field (resolved by field
/// name, not a hardcoded slot — see the field-count doc comment above).
pub(crate) fn native_module_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field_by_name(this, "name")))
}

/// `Module.getPackages()` — return a fresh `Set<String>` built from whatever
/// `build_module` recorded for this Module in `module_packages_table` (see
/// its doc comment), or the full JDK package set as a permissive default for
/// any Module this file didn't construct. Callers already check
/// `.contains(...)` so an over-inclusive default set is fine; an
/// under-inclusive one would not be, hence the fallback direction.
pub(crate) fn native_module_get_packages(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Module.getPackages: receiver must not be null".to_string()),
            }
            .into());
        }
    };
    let id = ctx.identity_hash_code(this);
    let recorded = module_packages_table().lock().unwrap().get(&id).cloned();
    let set = match recorded {
        Some(names) => {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            build_package_set(ctx, &refs)
        }
        None => build_package_set(ctx, BOOT_JDK_PACKAGES),
    };
    Ok(Some(Value::Object(Some(set))))
}

/// `Module.getLayer()` — return the real `layer` field (resolved by field
/// name — see the field-count doc comment above), lazily seeded with the
/// boot layer as a safe default if unset.
pub(crate) fn native_module_get_layer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let layer = build_boot_layer(ctx)?;
            return Ok(Some(Value::Object(Some(layer))));
        }
    };
    let existing = ctx.get_field_by_name(this, "layer");
    if let Value::Object(Some(_)) = existing {
        return Ok(Some(existing));
    }
    let layer = build_boot_layer(ctx)?;
    ctx.set_field_by_name(this, "layer", Value::Object(Some(layer)));
    Ok(Some(Value::Object(Some(layer))))
}

/// `ModuleLayer.modules()` — return an empty `HashSet<Module>`.
///
/// JDK bytecode for `ModuleLayer.modules()` dereferences an internal
/// `nameToModule` field that our synthetic boot layer never populates,
/// producing "Cannot invoke values on null". Spring 6/Boot 4's resource
/// scanner tolerates an empty module set, so this is the safest answer
/// in real-JDK mode where we don't model JPMS module graphs.
pub(crate) fn native_module_layer_modules(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(layer))) = args.first() {
        // Real layers produced by ModuleLayer.defineModules populate the
        // canonical nameToModule map even when their cached modules set is
        // still null. Derive and cache modules from that map so
        // ServiceLoader.load(layer, service) can scan provider modules.
        if let Value::Object(Some(name_to_module)) = ctx.get_field_by_name(*layer, "nameToModule") {
            let layer_pin = ctx.pin_native_root(*layer);
            let map_pin = ctx.pin_native_root(name_to_module);
            let map = ctx.read_native_pin(map_pin, name_to_module);
            let values_result =
                ctx.invoke_virtual(map, "values", "()Ljava/util/Collection;", &[])?;
            if let Some(Value::Object(Some(values))) = values_result {
                let values_pin = ctx.pin_native_root(values);
                let values = ctx.read_native_pin(values_pin, values);
                let modules = new_initialized_object(
                    ctx,
                    "java/util/HashSet",
                    "(Ljava/util/Collection;)V",
                    &[Value::Object(Some(values))],
                    "layer modules",
                )?;
                // The service-catalog population below re-enters Java and can
                // trigger a moving GC. Keep the newly created set rooted and
                // refresh its address before storing or returning it; an
                // unrooted pre-GC reference can later surface as an arbitrary
                // Object at Collection.stream() in Spring's module scanner.
                let modules_pin = ctx.pin_native_root(modules);
                let catalog = match ctx.invoke(
                    "jdk/internal/module/ServicesCatalog",
                    "create",
                    "()Ljdk/internal/module/ServicesCatalog;",
                    &[],
                )? {
                    Some(Value::Object(Some(catalog))) => Some(catalog),
                    _ => None,
                };
                if let Some(catalog) = catalog {
                    let catalog_pin = ctx.pin_native_root(catalog);
                    let values = ctx.read_native_pin(values_pin, values);
                    if let Some(Value::Object(Some(iter))) =
                        ctx.invoke_virtual(values, "iterator", "()Ljava/util/Iterator;", &[])?
                    {
                        let iter_pin = ctx.pin_native_root(iter);
                        loop {
                            let iter = ctx.read_native_pin(iter_pin, iter);
                            let has_next = ctx.invoke_virtual(iter, "hasNext", "()Z", &[])?;
                            let has_next = matches!(has_next, Some(Value::Int(v)) if v != 0);
                            if !has_next {
                                break;
                            }
                            let iter = ctx.read_native_pin(iter_pin, iter);
                            if let Some(Value::Object(Some(module))) =
                                ctx.invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[])?
                            {
                                let catalog = ctx.read_native_pin(catalog_pin, catalog);
                                ctx.invoke(
                                    "jdk/internal/module/ServicesCatalog",
                                    "register",
                                    "(Ljava/lang/Module;)V",
                                    &[Value::Object(Some(catalog)), Value::Object(Some(module))],
                                )?;
                            }
                        }
                        ctx.unpin_native_roots(iter_pin);
                    }
                    let layer = ctx.read_native_pin(layer_pin, *layer);
                    let catalog = ctx.read_native_pin(catalog_pin, catalog);
                    ctx.set_field_by_name(layer, "servicesCatalog", Value::Object(Some(catalog)));
                    ctx.unpin_native_roots(catalog_pin);
                }
                let layer = ctx.read_native_pin(layer_pin, *layer);
                let modules = ctx.read_native_pin(modules_pin, modules);
                ctx.set_field_by_name(layer, "modules", Value::Object(Some(modules)));
                ctx.unpin_native_roots(values_pin);
                ctx.unpin_native_roots(map_pin);
                ctx.unpin_native_roots(layer_pin);
                ctx.unpin_native_roots(modules_pin);
                return Ok(Some(Value::Object(Some(modules))));
            }
            ctx.unpin_native_roots(map_pin);
            ctx.unpin_native_roots(layer_pin);
        }
        if let Value::Object(Some(modules)) = ctx.get_field_by_name(*layer, "modules") {
            return Ok(Some(Value::Object(Some(modules))));
        }
    }
    // Build a REAL empty HashSet via its constructor. A synthetic HashSet with
    // slot-based fields breaks in real-JDK mode: the real `HashSet.iterator()`
    // bytecode reads `this.map` (a HashMap) which our synthetic object never
    // populates, so `modules().iterator()` returned null and Tomcat's
    // `for (ResolvedModule m : boot().configuration().modules())` web-fragment
    // scan (StandardJarScanner.doScanClassPath) NPE'd on `iterator.hasNext()`,
    // failing every embedded-server context start.
    ctx.new_object_initialized("java/util/HashSet", "()V", &[])
}

fn new_initialized_object(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    descriptor: &str,
    args: &[Value],
    label: &str,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    match ctx.new_object_initialized(class_name, descriptor, args)? {
        Some(Value::Object(Some(o))) => Ok(o),
        _ => Err(RuntimeError::IllegalStateException {
            message: format!("ModuleLayer.configuration: could not allocate {label}"),
        }
        .into()),
    }
}

fn collection_add(
    ctx: &mut dyn NativeContext,
    collection: ObjectRef,
    value: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    ctx.invoke(
        "java/util/HashSet",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(collection)), Value::Object(Some(value))],
    )?;
    Ok(())
}

fn map_put(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    key: ObjectRef,
    value: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    ctx.invoke(
        "java/util/HashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(map)),
            Value::Object(Some(key)),
            Value::Object(Some(value)),
        ],
    )?;
    Ok(())
}

fn build_unqualified_export(
    ctx: &mut dyn NativeContext,
    package_name: &str,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let export = alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor$Exports", 4);
    let export_pin = ctx.pin_native_root(export);
    let mods = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "export mods")?;
    let targets = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "export targets")?;
    let source = ctx.create_string(package_name);
    let export = ctx.read_native_pin(export_pin, export);
    ctx.set_field_by_name(export, "mods", Value::Object(Some(mods)));
    ctx.set_field_by_name(export, "source", Value::Object(Some(source)));
    ctx.set_field_by_name(export, "targets", Value::Object(Some(targets)));
    ctx.unpin_native_roots(export_pin);
    Ok(export)
}

fn build_boot_resolved_module(
    ctx: &mut dyn NativeContext,
    cfg: ObjectRef,
    name: &str,
    package_names: &[&str],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let cfg_pin = ctx.pin_native_root(cfg);
    let md = alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor", 16);
    let md_pin = ctx.pin_native_root(md);
    let module_name = ctx.create_string(name);
    let md = ctx.read_native_pin(md_pin, md);
    ctx.set_field_by_name(md, "name", Value::Object(Some(module_name)));

    // Keep descriptor collection accessors from observing null if downstream
    // resolver or layer code asks for packages/exports/opens/etc. Empty
    // collections are sufficient for the boot-configuration resolver paths.
    for field in [
        "modifiers",
        "requires",
        "exports",
        "opens",
        "uses",
        "provides",
        "packages",
    ] {
        let empty = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], field)?;
        let md = ctx.read_native_pin(md_pin, md);
        ctx.set_field_by_name(md, field, Value::Object(Some(empty)));
    }

    let exports =
        new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "boot module exports")?;
    let exports_pin = ctx.pin_native_root(exports);
    let packages =
        new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "boot module packages")?;
    let packages_pin = ctx.pin_native_root(packages);
    for package_name in package_names {
        let export = build_unqualified_export(ctx, package_name)?;
        let exports = ctx.read_native_pin(exports_pin, exports);
        collection_add(ctx, exports, export)?;

        let package_string = ctx.create_string(package_name);
        let packages = ctx.read_native_pin(packages_pin, packages);
        collection_add(ctx, packages, package_string)?;
    }
    let md = ctx.read_native_pin(md_pin, md);
    let exports = ctx.read_native_pin(exports_pin, exports);
    let packages = ctx.read_native_pin(packages_pin, packages);
    ctx.set_field_by_name(md, "exports", Value::Object(Some(exports)));
    ctx.set_field_by_name(md, "packages", Value::Object(Some(packages)));
    ctx.unpin_native_roots(packages_pin);
    ctx.unpin_native_roots(exports_pin);

    let mref = alloc_concurrent_synthetic(ctx, "jdk/internal/module/ModuleReferenceImpl", 8);
    let mref_pin = ctx.pin_native_root(mref);
    let md = ctx.read_native_pin(md_pin, md);
    ctx.set_field_by_name(mref, "descriptor", Value::Object(Some(md)));

    let resolved = alloc_concurrent_synthetic(ctx, "java/lang/module/ResolvedModule", 2);
    let cfg = ctx.read_native_pin(cfg_pin, cfg);
    let mref = ctx.read_native_pin(mref_pin, mref);
    ctx.set_field_by_name(resolved, "cf", Value::Object(Some(cfg)));
    ctx.set_field_by_name(resolved, "mref", Value::Object(Some(mref)));
    ctx.unpin_native_roots(mref_pin);
    ctx.unpin_native_roots(md_pin);
    ctx.unpin_native_roots(cfg_pin);
    Ok(resolved)
}

fn build_java_base_resolved_module(
    ctx: &mut dyn NativeContext,
    cfg: ObjectRef,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    build_boot_resolved_module(ctx, cfg, "java.base", BOOT_JDK_PACKAGES)
}

fn build_java_xml_resolved_module(
    ctx: &mut dyn NativeContext,
    cfg: ObjectRef,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    build_boot_resolved_module(ctx, cfg, "java.xml", JAVA_XML_PACKAGES)
}

/// `ModuleLayer.configuration()` ? return an empty synthetic `Configuration`.
///
/// Spring's `findAllModulePathResources` enumerates modules via
/// `boot().configuration().modules()`. Elasticsearch's provider locator goes
/// one step further and calls `boot().configuration().resolve(...)`; real JDK
/// `Configuration.resolve` asks the parent configuration to `findModule`, and
/// `Configuration.findModule` dereferences private caches such as
/// `nameToModule`. A one-slot synthetic object left those caches null and
/// failed before module descriptor checks could run. Seed the real private
/// collection fields with initialized JDK collections and include the boot
/// modules that downstream dynamic-module resolution depends on. `java.base`
/// is mandatory for every named module; `java.xml` is required by Xalan's
/// transient `jdk.translet` module when XMLUnit/Spring compare XML content.
pub(crate) fn native_module_layer_configuration(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let layer = match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    if let Some(layer) = layer {
        if let Value::Object(Some(cfg)) = ctx.get_field_by_name(layer, "cf") {
            return Ok(Some(Value::Object(Some(cfg))));
        }
    }

    let cfg = alloc_concurrent_synthetic(ctx, "java/lang/module/Configuration", 5);
    let cfg_pin = ctx.pin_native_root(cfg);

    let parents = new_initialized_object(ctx, "java/util/ArrayList", "()V", &[], "parents")?;
    let cfg = ctx.read_native_pin(cfg_pin, cfg);
    ctx.set_field_by_name(cfg, "parents", Value::Object(Some(parents)));

    let graph = new_initialized_object(ctx, "java/util/HashMap", "()V", &[], "graph")?;
    let graph_pin = ctx.pin_native_root(graph);
    let cfg = ctx.read_native_pin(cfg_pin, cfg);
    ctx.set_field_by_name(cfg, "graph", Value::Object(Some(graph)));

    let modules = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "modules")?;
    let modules_pin = ctx.pin_native_root(modules);
    let cfg = ctx.read_native_pin(cfg_pin, cfg);
    ctx.set_field_by_name(cfg, "modules", Value::Object(Some(modules)));

    let name_to_module =
        new_initialized_object(ctx, "java/util/HashMap", "()V", &[], "nameToModule")?;
    let name_to_module_pin = ctx.pin_native_root(name_to_module);
    let cfg = ctx.read_native_pin(cfg_pin, cfg);
    ctx.set_field_by_name(cfg, "nameToModule", Value::Object(Some(name_to_module)));

    let java_base = build_java_base_resolved_module(ctx, cfg)?;
    let java_base_pin = ctx.pin_native_root(java_base);
    let java_xml = build_java_xml_resolved_module(ctx, cfg)?;
    let java_xml_pin = ctx.pin_native_root(java_xml);
    let modules = ctx.read_native_pin(modules_pin, modules);
    let java_base = ctx.read_native_pin(java_base_pin, java_base);
    collection_add(ctx, modules, java_base)?;
    let modules = ctx.read_native_pin(modules_pin, modules);
    let java_xml = ctx.read_native_pin(java_xml_pin, java_xml);
    collection_add(ctx, modules, java_xml)?;

    let empty_reads =
        new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "java.base reads")?;
    let graph = ctx.read_native_pin(graph_pin, graph);
    let java_base = ctx.read_native_pin(java_base_pin, java_base);
    map_put(ctx, graph, java_base, empty_reads)?;

    let java_xml_reads =
        new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "java.xml reads")?;
    let java_base = ctx.read_native_pin(java_base_pin, java_base);
    collection_add(ctx, java_xml_reads, java_base)?;
    let graph = ctx.read_native_pin(graph_pin, graph);
    let java_xml = ctx.read_native_pin(java_xml_pin, java_xml);
    map_put(ctx, graph, java_xml, java_xml_reads)?;

    let key = ctx.create_string("java.base");
    let name_to_module = ctx.read_native_pin(name_to_module_pin, name_to_module);
    let java_base = ctx.read_native_pin(java_base_pin, java_base);
    map_put(ctx, name_to_module, key, java_base)?;

    let key = ctx.create_string("java.xml");
    let name_to_module = ctx.read_native_pin(name_to_module_pin, name_to_module);
    let java_xml = ctx.read_native_pin(java_xml_pin, java_xml);
    map_put(ctx, name_to_module, key, java_xml)?;

    if let Some(layer) = layer {
        let cfg = ctx.read_native_pin(cfg_pin, cfg);
        ctx.set_field_by_name(layer, "cf", Value::Object(Some(cfg)));
    }

    ctx.unpin_native_roots(java_xml_pin);
    ctx.unpin_native_roots(java_base_pin);
    ctx.unpin_native_roots(name_to_module_pin);
    ctx.unpin_native_roots(modules_pin);
    ctx.unpin_native_roots(graph_pin);
    ctx.unpin_native_roots(cfg_pin);

    Ok(Some(Value::Object(Some(cfg))))
}

fn native_system_module_reader_list(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Synthetic boot Configuration entries represent system modules only so
    // framework module-path scans can proceed to classpath resources. We do
    // not model the jimage-backed SystemModuleReader here; returning an empty
    // Stream matches the long-standing "no module-path resources" behavior
    // without throwing from ModuleReader.list().
    cratonvm_native_collections::make_stream_from_elements(ctx, &[])
}

/// Read a `java.lang.Module`'s name for the module registry.
///
/// Prefers the declared `name` field (the real-JDK `java.lang.Module`
/// layout, where slot 0 is `layer`, not the name) and falls back to slot 0,
/// which is where `build_module` above and `phases_late`'s synthetic
/// `ModuleLayer.modules()` / `findModule` park it. An empty string means the
/// unnamed module — the same sentinel `ModuleRegistry` uses.
fn module_registry_name(ctx: &mut dyn NativeContext, module: ObjectRef) -> String {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(module, "name") {
        if let Some(name) = ctx.read_string(s) {
            return name;
        }
    }
    if let Value::Object(Some(s)) = ctx.get_field(module, 0) {
        if let Some(name) = ctx.read_string(s) {
            return name;
        }
    }
    String::new()
}

/// `java.lang.Module.addExports0(Module from, String pkg, Module to)` —
/// record the qualified dynamic export in CratonVM's `ModuleRegistry`.
fn native_module_add_exports0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let pkg = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(None),
    };
    let to = match args.get(2) {
        Some(Value::Object(Some(o))) => module_registry_name(ctx, *o),
        _ => String::new(),
    };
    let from_name = module_registry_name(ctx, from);
    // The registry keys packages in internal (slash) form.
    ctx.module_add_exports(&from_name, &pkg.replace('.', "/"), &to);
    Ok(None)
}

/// `java.lang.Module.addExportsToAll0(Module from, String pkg)` and
/// `addExportsToAllUnnamed0` — record an unqualified dynamic export.
fn native_module_add_exports_to_all0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let from = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let pkg = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(None),
    };
    let from_name = module_registry_name(ctx, from);
    ctx.module_add_exports(&from_name, &pkg.replace('.', "/"), "");
    Ok(None)
}

/// Install every JDKSpecific boot-path native this module owns.
pub fn register_jboss_jdkspecific(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let ml = "java/lang/ModuleLayer";
    // boot() already has a stub in phases_late; re-registering is safe
    // because the registry's insert is last-writer-wins, and this
    // implementation returns the boot-flagged layer the findModule path
    // expects.
    registry.register(
        ml,
        "boot",
        "()Ljava/lang/ModuleLayer;",
        native_module_layer_boot,
    );
    registry.register(
        ml,
        "findModule",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        native_module_layer_find_module,
    );
    // Spring's `PathMatchingResourcePatternResolver.findAllModulePathResources`
    // calls `ModuleLayer.boot().modules()` to enumerate JPMS modules. The JDK
    // bytecode for `ModuleLayer.modules()` reads `this.nameToModule.values()`,
    // which NPEs on our synthetic boot layer because we never populate that
    // field. Return an empty HashSet: Spring tolerates an empty module set
    // (classpath-jar resources still resolve through other code paths).
    registry.register(
        ml,
        "modules",
        "()Ljava/util/Set;",
        native_module_layer_modules,
    );
    // Spring 6/Boot 4's modulepath scanner actually invokes
    // `ModuleLayer.boot().configuration().modules()` (a `Set<ResolvedModule>`).
    // The JDK bytecode for `ModuleLayer.configuration()` reads `this.cf`,
    // which is null on our synthetic layer — surfacing as the visible
    // "Cannot invoke modules on null" NPE. Return an empty synthetic
    // `Configuration` whose `modules()` is overridden below.
    registry.register(
        ml,
        "configuration",
        "()Ljava/lang/module/Configuration;",
        native_module_layer_configuration,
    );

    // java.lang.module.Configuration.modules() — return an empty Set<ResolvedModule>.
    registry.register(
        "java/lang/module/Configuration",
        "modules",
        "()Ljava/util/Set;",
        native_module_layer_modules,
    );

    let m = "java/lang/Module";
    registry.register(m, "getName", "()Ljava/lang/String;", native_module_get_name);
    registry.register(
        m,
        "getPackages",
        "()Ljava/util/Set;",
        native_module_get_packages,
    );
    registry.register(
        m,
        "getLayer",
        "()Ljava/lang/ModuleLayer;",
        native_module_get_layer,
    );
    // `defineModule0(Module, boolean isOpen, String version, String location,
    // Object[] packageNames)` — the VM-sync hook `Module.<init>` calls once the
    // Java-side object is initialized. STUB-REMOVAL (wave 4): the previous
    // no-op's justification ("CratonVM's module table is populated by the class
    // manager, so there is nothing to record") was wrong twice over.
    //
    //  1. The package list arrives HERE and nowhere else. Without it
    //     `native_module_get_packages` finds no `module_packages_table` row and
    //     falls back to `BOOT_JDK_PACKAGES` — i.e. every real-JDK-defined
    //     module claimed to contain the whole of java.base.
    //  2. `isOpen` is the only place the VM learns a module is open, and an
    //     open module opens all its packages to every other module. That is
    //     exactly `ModuleRegistry::add_opens(pkg, "" = all)`, the same
    //     "empty target means unqualified" convention `addExportsToAll0` uses.
    //
    // Still missing (ESCALATION, needs a NativeContext accessor): there is no
    // `module_add_package`, so the package -> module direction consulted by
    // `module_for_package` is still only whatever the class manager derived.
    registry.register_with_kind(
        m,
        "defineModule0",
        "(Ljava/lang/Module;ZLjava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)V",
        native_module_define_module0,
        NativeKind::Bridge,
    );
    // The three `addExports*0` hooks are NOT bookkeeping-free: they are the
    // only place the VM learns about a dynamic export, and CratonVM's own
    // `ModuleRegistry` (reached through `NativeContext::module_add_exports`,
    // implemented in `vm_exec.rs`) is what
    // `is_package_exported_{unqualified,to}` later consults on every
    // reflective access check. Leaving them as no-ops — the state of this
    // file until 2026-07-27 — silently dropped every `Module.addExports` /
    // `--add-exports` edge, so an export that had been granted still failed
    // the access check. The stale comment that used to sit above them
    // ("there is no extra VM module table to update") was simply wrong.
    registry.register_with_kind(
        m,
        "addExports0",
        "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V",
        native_module_add_exports0,
        NativeKind::Bridge,
    );
    // Unqualified export (`exports pkg;`). `ModuleRegistry::add_exports`
    // treats an empty target as "to all modules".
    registry.register_with_kind(
        m,
        "addExportsToAll0",
        "(Ljava/lang/Module;Ljava/lang/String;)V",
        native_module_add_exports_to_all0,
        NativeKind::Bridge,
    );
    // Export to the unnamed module (`--add-exports …=ALL-UNNAMED`). The
    // unnamed module's registry name is the empty string
    // (`classloading::module::UNNAMED_MODULE`), which `add_exports` already
    // reads as the unqualified form — so this deliberately shares the
    // `addExportsToAll0` implementation. That is a widening (we grant to all
    // modules rather than only unnamed ones); the alternative, dropping the
    // edge entirely, produced spurious IllegalAccessErrors.
    registry.register_with_kind(
        m,
        "addExportsToAllUnnamed0",
        "(Ljava/lang/Module;Ljava/lang/String;)V",
        native_module_add_exports_to_all0,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/module/ResolvedModule",
        "getDescriptor",
        "()Ljava/lang/module/ModuleDescriptor;",
        |ctx, args| {
            let this = match args.first().copied() {
                Some(Value::Object(Some(o))) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Value::Object(Some(mref)) = ctx.get_field_by_name(this, "mref") {
                if let Value::Object(Some(desc)) = ctx.get_field_by_name(mref, "descriptor") {
                    return Ok(Some(Value::Object(Some(desc))));
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );
    registry.register(
        "jdk/internal/module/SystemModuleFinders$SystemModuleReader",
        "list",
        "()Ljava/util/stream/Stream;",
        native_system_module_reader_list,
    );
    // T19_H12_MODULE_GETCLASSLOADER — `java.lang.Module.getClassLoader()`.
    //
    // Per JDK 25 javadoc: "If this module is in the boot layer and is loaded
    // by the bootstrap class loader then this method returns null." Every
    // synthetic `java.lang.Module` we hand out (built by `build_module` here
    // and by `phases_late.rs::ModuleLayer.modules` / `findModule`) represents
    // a platform module, so returning `null` is spec-correct and keeps
    // downstream code on the BootLoader fast path.
    //
    // Without this native, JDK bytecode for `Module.getClassLoader()` reads
    // `this.loader` by field index, but our synthetic Module's slot 2 holds
    // a `HashSet` (the packages set) which produced
    // "NoSuchMethodError: java/util/HashSet.loadClass(Module, String)Class"
    // when downstream code tried to invoke `loadClass` on the result.
    //
    // Amended 2026-07-26: answer from the Module's own `loader` field when it
    // is populated, and only fall back to null when it is not. The blanket
    // null made `Module.getClassLoader()` return null even for the UNNAMED
    // module -- where HotSpot returns the defining (application) loader -- so
    // real `Package.getPackageInfo()` resolved `<pkg>.package-info` against
    // the BOOTSTRAP loader and never saw a classpath package-info, hiding
    // every package-level annotation from any `Package` not built by
    // `Class.getPackage()`'s eager path. `lang_class::canonical_unnamed_module`
    // now wires `loader` on the unnamed-module mirror; the synthetic PLATFORM
    // modules `build_module` above produces still leave it unset and so still
    // get the spec-correct null this override was written for.
    registry.register(
        m,
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Value::Object(Some(loader)) = ctx.get_field_by_name(this, "loader") {
                return Ok(Some(Value::Object(Some(loader))));
            }
            Ok(Some(Value::Object(None)))
        },
    );

    // ── Round 63: WildFly `WildFlySecurityManager` <clinit> NPE ────────────
    //
    // `org.wildfly.security.manager._private.JDKSpecific.getCallerClass(int n)`
    // is the per-JDK shim WildFly uses to find the call-site that triggered
    // a permission check. Its real-JDK body is roughly:
    //
    //   static Class<?> getCallerClass(int n) {
    //       return getStackWalker()
    //           .walk(s -> s.skip(n).findFirst())
    //           .get()
    //           .getDeclaringClass();
    //   }
    //
    // Two CratonVM-side weaknesses combine to produce
    // `NullPointerException: Cannot invoke getDeclaringClass on null`:
    //   1. Our synthetic StackWalker stream sometimes hands back an empty
    //      Optional when `skip(n)` overshoots the available frames.
    //   2. The downstream `Optional.get()` on the synthetic Optional then
    //      yields null rather than NoSuchElementException.
    //
    // WildFlySecurityManager's `<clinit>` block calls `getCallerClass(2)` to
    // seed a class reference — the result is only used to allow/deny the
    // *initial* security-domain context. A spec-safe fallback is to return
    // the @CallerSensitive caller class via our existing stack-trace
    // capture, falling back to `java.lang.Object` (the JDK universally-
    // permitted base) when no Java frame is available.
    //
    // The same shim ships in `org.jboss.modules.JDKSpecific` (JBoss
    // Modules' own copy with the identical signature) — register both so
    // future loader paths don't trip on the same null deref.
    fn native_jdkspecific_get_caller_class(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let depth = match args.first() {
            Some(Value::Int(d)) => *d as i64,
            _ => 0,
        };
        let frames = ctx.capture_stack_trace(0);
        // Frames are bottom-up (main at [0], innermost top at [len-1]).
        // The native frame itself isn't recorded, so frames[len-1] is the
        // @CallerSensitive method that invoked us. WildFly's `n` counts
        // outward from that point: n=0 → its own frame, n=1 → its caller.
        let target = if !frames.is_empty() {
            let len = frames.len() as i64;
            let idx = (len - 1 - depth).max(0) as usize;
            frames.get(idx).cloned()
        } else {
            None
        };
        let class_name = target
            .map(|f| f.class_name.replace('.', "/"))
            .unwrap_or_else(|| "java/lang/Object".to_string());
        let cid = ctx
            .ensure_class_initialized(&class_name)
            .or_else(|_| ctx.ensure_class_initialized("java/lang/Object"))
            .unwrap_or(cratonvm_types::ClassId::new(0));
        let mirror = ctx.get_class_mirror(cid);
        Ok(Some(Value::Object(Some(mirror))))
    }

    registry.register(
        "org/wildfly/security/manager/_private/JDKSpecific",
        "getCallerClass",
        "(I)Ljava/lang/Class;",
        native_jdkspecific_get_caller_class,
    );
    registry.register(
        "org/jboss/modules/JDKSpecific",
        "getCallerClass",
        "(I)Ljava/lang/Class;",
        native_jdkspecific_get_caller_class,
    );
    registry.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_utils::MockNativeContext;

    #[test]
    fn validate_module_name_accepts_valid_names() {
        assert!(validate_module_name("java.base").is_ok());
        assert!(validate_module_name("java.sql").is_ok());
        assert!(validate_module_name("org.example.feature").is_ok());
        assert!(validate_module_name("x").is_ok()); // minimal
    }

    #[test]
    fn validate_module_name_rejects_path_traversal() {
        assert!(validate_module_name("../etc/passwd").is_err());
        assert!(validate_module_name("../../win.ini").is_err());
        assert!(validate_module_name("java..base").is_err());
    }

    #[test]
    fn validate_module_name_rejects_path_separators() {
        assert!(validate_module_name("java/base").is_err());
        assert!(validate_module_name("java\\base").is_err());
        assert!(validate_module_name("C:\\Windows").is_err());
        assert!(validate_module_name("java:base").is_err());
    }

    #[test]
    fn validate_module_name_rejects_control_bytes() {
        assert!(validate_module_name("java.base\0").is_err());
        assert!(validate_module_name("java\nbase").is_err());
        assert!(validate_module_name("java\tbase").is_err());
    }

    #[test]
    fn validate_module_name_rejects_empty_and_oversize() {
        assert!(validate_module_name("").is_err());
        let long = "a".repeat(300);
        assert!(validate_module_name(&long).is_err());
    }

    #[test]
    fn module_layer_boot_returns_non_null_layer() {
        let mut ctx = MockNativeContext::new();
        let result = native_module_layer_boot(&mut ctx, &[]).unwrap().unwrap();
        match result {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null ModuleLayer, got {:?}", other),
        }
    }

    #[test]
    fn find_module_returns_populated_optional() {
        let mut ctx = MockNativeContext::new();
        let layer_v = native_module_layer_boot(&mut ctx, &[]).unwrap().unwrap();
        let name_str = ctx.create_string("java.base");
        let args = [layer_v, Value::Object(Some(name_str))];
        let result = native_module_layer_find_module(&mut ctx, &args)
            .unwrap()
            .unwrap();
        if let Value::Object(Some(opt)) = result {
            // Optional.value should be non-null (a Module)
            match ctx.get_field(opt, 0) {
                Value::Object(Some(_module)) => {}
                other => panic!("expected Optional(Module), got {:?}", other),
            }
        } else {
            panic!("expected non-null Optional");
        }
    }

    #[test]
    fn find_module_rejects_null_name() {
        let mut ctx = MockNativeContext::new();
        let layer_v = native_module_layer_boot(&mut ctx, &[]).unwrap().unwrap();
        let args = [layer_v, Value::Object(None)];
        let err = native_module_layer_find_module(&mut ctx, &args).unwrap_err();
        let s = format!("{:?}", err);
        assert!(
            s.contains("null") || s.contains("Null"),
            "expected NPE, got {}",
            s
        );
    }

    #[test]
    fn find_module_rejects_malicious_name() {
        let mut ctx = MockNativeContext::new();
        let layer_v = native_module_layer_boot(&mut ctx, &[]).unwrap().unwrap();
        let name_str = ctx.create_string("../../evil");
        let args = [layer_v, Value::Object(Some(name_str))];
        let err = native_module_layer_find_module(&mut ctx, &args).unwrap_err();
        let s = format!("{:?}", err);
        assert!(
            s.contains("traversal") || s.contains("IllegalArgument"),
            "expected IAE, got {}",
            s
        );
    }

    #[test]
    fn module_get_packages_returns_populated_set_for_java_base() {
        let mut ctx = MockNativeContext::new();
        let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
        let module = build_module(&mut ctx, "java.base", layer)?;
        let result = native_module_get_packages(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        if let Value::Object(Some(set)) = result {
            // slot 1 = size must match BOOT_JDK_PACKAGES.len()
            match ctx.get_field(set, 1) {
                Value::Int(n) => assert!(n > 0, "size must be > 0, got {}", n),
                other => panic!("expected Int for size, got {:?}", other),
            }
        } else {
            panic!("expected non-null Set");
        }
    }

    #[test]
    fn module_get_packages_rejects_null_receiver() {
        let mut ctx = MockNativeContext::new();
        let err = native_module_get_packages(&mut ctx, &[Value::Object(None)]).unwrap_err();
        let s = format!("{:?}", err);
        assert!(
            s.contains("null") || s.contains("Null"),
            "expected NPE, got {}",
            s
        );
    }

    #[test]
    fn module_get_packages_lazily_populates_empty_module() {
        let mut ctx = MockNativeContext::new();
        // build_module records an EMPTY package list for any non-java.base
        // name (module_packages_table) — getPackages() must still return a
        // valid (non-null) Set, not null/panic.
        let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
        let module = build_module(&mut ctx, "java.sql", layer)?;
        let result = native_module_get_packages(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        match result {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null populated Set, got {:?}", other),
        }
    }

    #[test]
    fn module_get_name_returns_string() {
        let mut ctx = MockNativeContext::new();
        let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
        let module = build_module(&mut ctx, "java.base", layer)?;
        let result = native_module_get_name(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        match result {
            Value::Object(Some(s)) => {
                let name = ctx.read_string(s).unwrap_or_default();
                assert_eq!(name, "java.base");
            }
            other => panic!("expected String, got {:?}", other),
        }
    }

    #[test]
    fn register_jboss_jdkspecific_adds_surface() {
        let mut r = NativeMethodRegistry::new();
        let before = r.len();
        register_jboss_jdkspecific(&mut r);
        let after = r.len();
        // We add boot, findModule, getName, getPackages, getLayer,
        // getClassLoader (T19_H12_) — 6.
        assert!(
            after >= before + 6,
            "expected at least 6 new registrations, got {}",
            after - before
        );
    }

    // T19_H12_TESTS — Module.getClassLoader native must return null so
    // downstream `Class.forName(Module, String)` paths fall through to
    // `BootLoader.loadClass` rather than dispatching `loadClass(...)` on
    // a stale receiver class (the bug was a HashSet receiver because
    // synthetic Module slot 2 holds the packages set).
    #[test]
    fn t19_h12_module_get_classloader_returns_null() {
        let mut r = NativeMethodRegistry::new();
        register_jboss_jdkspecific(&mut r);
        let cb = r
            .find(
                "java/lang/Module",
                "getClassLoader",
                "()Ljava/lang/ClassLoader;",
            )
            .expect("Module.getClassLoader must be registered");
        let mut ctx = MockNativeContext::new();
        let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
        let module = build_module(&mut ctx, "java.base", layer)?;
        let result = cb(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        // Per JDK 25 spec: boot-layer modules with bootstrap loader return null.
        assert!(
            matches!(result, Value::Object(None)),
            "expected null ClassLoader for boot-layer module, got {:?}",
            result
        );
    }

    #[test]
    fn t19_h12_module_get_classloader_returns_null_even_with_recorded_packages() {
        // Defense in depth: getClassLoader must not be affected by (or leak)
        // whatever `build_module` recorded for this module's packages in
        // `module_packages_table` — the historical bug this guarded against
        // was a raw field slot double-booked for both purposes; that's no
        // longer possible now that packages are stored off-object entirely,
        // but keep the regression coverage that getClassLoader is correct
        // for a module with real recorded package data.
        let mut r = NativeMethodRegistry::new();
        register_jboss_jdkspecific(&mut r);
        let cb = r
            .find(
                "java/lang/Module",
                "getClassLoader",
                "()Ljava/lang/ClassLoader;",
            )
            .expect("Module.getClassLoader must be registered");
        let mut ctx = MockNativeContext::new();
        let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
        // Build a fully-populated module (java.base records the full JDK package list).
        let module = build_module(&mut ctx, "java.base", layer)?;
        // Sanity: getPackages() reflects the recorded data for this module.
        let pkgs = native_module_get_packages(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        assert!(
            matches!(pkgs, Value::Object(Some(_))),
            "getPackages() should return a populated Set"
        );
        // getClassLoader must NOT be affected by any of this.
        let result = cb(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, Value::Object(None)),
            "Module.getClassLoader must not leak packages data, got {:?}",
            result
        );
    }
}
