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

use cratonvm_classloading::module::ALL_UNNAMED_TARGET;
use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::try_alloc_concurrent_synthetic;
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
/// (measured: `regression-suite/src/RJdkModule.java:60` fails with "module must
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

    let layer =
        try_alloc_concurrent_synthetic(ctx, "java/lang/ModuleLayer", MODULE_LAYER_FIELD_COUNT)?;
    let layer_pin = ctx.pin_native_root(layer);

    let parents = new_initialized_object(ctx, "java/util/ArrayList", "()V", &[], "layer parents")?;
    let layer = ctx.read_native_pin(layer_pin, layer);
    ctx.set_field_by_name(layer, "parents", Value::Object(Some(parents)));

    let name_to_module =
        new_initialized_object(ctx, "java/util/HashMap", "()V", &[], "layer nameToModule")?;
    let layer = ctx.read_native_pin(layer_pin, layer);
    ctx.set_field_by_name(layer, "nameToModule", Value::Object(Some(name_to_module)));

    let modules = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "layer modules")?;
    let mut layer = ctx.read_native_pin(layer_pin, layer);
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
    populate_boot_layer_modules(ctx, &mut layer)?;
    Ok(layer)
}

/// Pins `layer` across [`populate_boot_layer_modules_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `layer` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
fn populate_boot_layer_modules(
    ctx: &mut dyn NativeContext,
    layer: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let w5_pin = ctx.pin_native_root(*layer);
    let w5_out = populate_boot_layer_modules_body(ctx, *layer);
    *layer = ctx.read_native_pin(w5_pin, *layer);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// Insert every registered module into the boot layer's `nameToModule` map and
/// `modules` set.
///
/// # Why this exists
///
/// The layer above is allocated with a freshly created, EMPTY `HashMap` and
/// nothing ever added to it — `build_module` sets the module's `layer` but the
/// back-edge was never written. That is invisible for most of `ModuleLayer`'s
/// surface, because CratonVM answers `findModule`, `getDescriptor` and the rest
/// from its own `ModuleRegistry` rather than from the map. It is NOT invisible
/// to services.
///
/// Real `ModuleLayer.getServicesCatalog()` self-populates: when the field is
/// null it calls `ServicesCatalog.create()` and loops `nameToModule.values()`
/// calling `catalog.register(m)`, which reads `m.getDescriptor().provides()`.
/// Over an empty map that produces an empty catalog, and `ServiceLoader` reads
/// providers from the catalog ONLY — never from a descriptor directly. So
/// `ServiceLoader.load(layer, Service.class)` found nothing:
/// `regression-suite/src/RJdkModule.java` failed with
/// `AssertionError: module service providers: []` under `--jdk-only`, where the
/// real `java.util.ServiceLoader` bytecode runs because the ServiceLoader
/// natives are `NativeKind::SyntheticStub` and strict mode refuses them at
/// registration. Diagnosis: `docs/known-issues/jdk-only/W6-11-*`, §3.
///
/// This also un-breaks `native_module_layer_modules`, which already built a
/// catalog by iterating `nameToModule.values()` and was a no-op for services in
/// BOTH modes for the same reason.
///
/// # Why it runs after the layer is published
///
/// `build_module` runs Java (`build_module_descriptor` allocates and
/// initialises), and that can re-enter `ModuleLayer.boot()`. Publishing the
/// layer to the memo and the global root table FIRST means such a re-entry gets
/// this same object — a partially populated map at worst — instead of recursing
/// into a second `build_boot_layer`. Population is idempotent: `HashMap.put`
/// re-keys by name and `build_module` returns the cached mirror for a
/// registered name.
///
/// A failure to build any one module is not fatal to the layer: the module is
/// skipped and the rest are inserted. A layer missing one module is strictly
/// better than no layer at all, which is what propagating would produce.
///
/// # What it does NOT populate
///
/// It walks the whole `ModuleRegistry` minus the modules whose only source is
/// the application CLASS path (`module_is_class_path_only`). Those are modular
/// jars on `-cp`, which a real JVM treats as unnamed-module citizens whose
/// `module-info` it ignores outright — so they are in neither `ModuleLayer.boot()`
/// nor the system loader's `ServicesCatalog` on HotSpot, and putting them in
/// either here handed `ServiceLoader` the same provider twice. The gate is at
/// the top of the loop with the measurement that motivated it.
fn populate_boot_layer_modules_body(
    ctx: &mut dyn NativeContext,
    layer: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let names = ctx.module_names();
    if names.is_empty() {
        return Ok(());
    }
    let layer_pin = ctx.pin_native_root(layer);
    for name in names {
        // A module whose ONLY source is the application CLASS path must not
        // enter the boot layer, and above all must not be registered in the
        // system loader's `ServicesCatalog` below.
        //
        // `ClassManager::new` scans the app class path for `module-info.class`
        // and registers what it finds (class_manager.rs, `automatic = true`);
        // `vm_init` re-registers genuine `--module-path` modules with
        // `automatic = false` right afterwards, so `automatic` here means
        // exactly "reached only through -cp". A real JVM ignores such a
        // `module-info` outright — a modular jar on the class path is an
        // unnamed-module citizen — and `service_loader.rs` already states that
        // rule as the reason ITS module source is skipped for a class-path
        // loader.
        //
        // Promoting one here gave `ServiceLoader` the SAME provider through
        // both of its doors: `ModuleServicesLookupIterator` reads this
        // catalog, and `LazyClassPathLookupIterator` reads
        // `META-INF/services`. The JDK's only cross-source guard is
        // `clazz.getModule().isNamed()`, which is FALSE for the class-path
        // copy, so nothing de-duplicates. Measured: four bc-java corpus
        // classes died with `JUnitException: Cannot create Launcher for
        // multiple engines with the same ID 'junit-jupiter'` before running a
        // test — junit-jupiter-engine.jar ships both a `provides` clause and a
        // `META-INF/services` descriptor naming one class.
        //
        // Skipping is the only arm that matches HotSpot in BOTH sub-cases. A
        // `-cp` jar that ships both forms must yield ONE provider (the
        // descriptor's); a `-cp` jar whose `module-info` declares `provides`
        // with no `META-INF/services` must yield ZERO. Suppressing the
        // descriptor side instead would answer 1 and 1 — right once, wrong
        // once — and would additionally require `getResources` to hide a
        // resource that genuinely is on the class path, which HotSpot returns.
        //
        // See docs/known-issues/jdk-only/D1-R11-SERVICELOADER-DOUBLE-SOURCE-20260813.md
        // and docs/known-issues/jdk-only/E4-R11-CLASS-PATH-MODULE-BOOT-LAYER-FIX-20260813.md.
        if ctx.module_is_class_path_only(&name) {
            continue;
        }
        let layer = ctx.read_native_pin(layer_pin, layer);
        let Ok(module) = build_module(ctx, &name, layer) else {
            continue;
        };
        let module_pin = ctx.pin_native_root(module);

        // `nameToModule.put(name, module)` — the map real
        // `getServicesCatalog()` iterates.
        let layer = ctx.read_native_pin(layer_pin, layer);
        if let Value::Object(Some(map)) = ctx.get_field_by_name(layer, "nameToModule") {
            let map_pin = ctx.pin_native_root(map);
            let key = ctx.create_string(&name);
            let map = ctx.read_native_pin(map_pin, map);
            let module = ctx.read_native_pin(module_pin, module);
            let _ = ctx.invoke_virtual(
                map,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(key)), Value::Object(Some(module))],
            );
            ctx.unpin_native_roots(map_pin);
        }

        // `modules.add(module)` — kept in step so `ModuleLayer.modules()` and
        // the map cannot disagree about the layer's contents.
        let layer = ctx.read_native_pin(layer_pin, layer);
        if let Value::Object(Some(set)) = ctx.get_field_by_name(layer, "modules") {
            let set_pin = ctx.pin_native_root(set);
            let set = ctx.read_native_pin(set_pin, set);
            let module = ctx.read_native_pin(module_pin, module);
            let _ = ctx.invoke_virtual(
                set,
                "add",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(module))],
            );
            ctx.unpin_native_roots(set_pin);
        }
        // `ServicesCatalog.getServicesCatalog(appLoader).register(module)` —
        // the OTHER of the two routes `ServiceLoader` takes, and the one the
        // no-arg `ServiceLoader.load(Service.class)` uses.
        //
        // `ModuleServicesLookupIterator.iteratorFor(loader)` asks
        // `ServicesCatalog.getServicesCatalogOrNull(loader)` — a per-loader
        // `ClassLoaderValue` — and NOT the layer. Populating `nameToModule`
        // above fixes only `ServiceLoader.load(layer, Service.class)`; without
        // this the plain overload still answers `[]`. Both are asserted, three
        // lines apart, in `RJdkModule.moduleServices()` (`:233` is this route,
        // `:242` the layer one), and fixing the layer alone moved the failure
        // by zero lines.
        //
        // Registering against the SYSTEM loader is what the real
        // `ModuleLayer.defineModules` does for boot-layer modules resolved from
        // `--module-path`: they are defined to the application loader, and its
        // catalog is what the lookup walks. `getServicesCatalog` creates the
        // catalog if absent, and `register(Module)` reads
        // `descriptor.provides()`, so a module that declares none is a no-op
        // rather than a special case.
        let module = ctx.read_native_pin(module_pin, module);
        register_module_in_loader_catalog(ctx, module);

        ctx.unpin_native_roots(module_pin);
    }
    ctx.unpin_native_roots(layer_pin);
    Ok(())
}

/// Add `module` to the system class loader's `ServicesCatalog`.
///
/// Best-effort by design: every step is a real-JDK call that a synthetic-JDK
/// build may not have, and a missing services catalog must not take the boot
/// layer down with it. A caller that gets no catalog is exactly where it was
/// before this existed.
fn register_module_in_loader_catalog(ctx: &mut dyn NativeContext, module: ObjectRef) {
    let module_pin = ctx.pin_native_root(module);
    let loader = match ctx.invoke(
        "java/lang/ClassLoader",
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(l)))) => l,
        _ => {
            ctx.unpin_native_roots(module_pin);
            return;
        }
    };
    let loader_pin = ctx.pin_native_root(loader);
    let loader = ctx.read_native_pin(loader_pin, loader);
    let catalog = match ctx.invoke(
        "jdk/internal/module/ServicesCatalog",
        "getServicesCatalog",
        "(Ljava/lang/ClassLoader;)Ljdk/internal/module/ServicesCatalog;",
        &[Value::Object(Some(loader))],
    ) {
        Ok(Some(Value::Object(Some(c)))) => c,
        _ => {
            ctx.unpin_native_roots(loader_pin);
            ctx.unpin_native_roots(module_pin);
            return;
        }
    };
    let catalog_pin = ctx.pin_native_root(catalog);
    let catalog = ctx.read_native_pin(catalog_pin, catalog);
    let module = ctx.read_native_pin(module_pin, module);
    let _ = ctx.invoke(
        "jdk/internal/module/ServicesCatalog",
        "register",
        "(Ljava/lang/Module;)V",
        &[Value::Object(Some(catalog)), Value::Object(Some(module))],
    );
    ctx.unpin_native_roots(catalog_pin);
    ctx.unpin_native_roots(loader_pin);
    ctx.unpin_native_roots(module_pin);
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
fn build_package_set(
    ctx: &mut dyn NativeContext,
    packages: &[&str],
) -> Result<ObjectRef, MethodCallFailed> {
    let keys: Vec<ObjectRef> = packages.iter().map(|pkg| ctx.create_string(pkg)).collect();
    Ok(crate::build_real_layout_string_hashset(ctx, &keys)?)
}

/// The package set to record for `name` in `module_packages_table`.
///
/// **The VM's `ModuleRegistry` is the source; the hand-maintained lists below
/// are a FLOOR for names it does not know.** That order used to be the other
/// way round for `java.base` and `java.xml`, on the stated premise that "the
/// registry does not enumerate jimage packages". Measured against
/// jdk-25.0.3.9-hotspot (`probes/L5ModuleInvokeSweep.java`), it does: the
/// registry answers >100 packages for `java.base` where `BOOT_JDK_PACKAGES`
/// has 63, and `build_module_descriptor` — which has always read the registry
/// — was already publishing the larger set through
/// `getDescriptor().packages()`. So the two answers DISAGREED IN BOTH
/// DIRECTIONS on the same VM: `getPackages()` was missing packages the
/// descriptor exported (54 of them, `jdk.internal.loader` among them), and
/// carried names the descriptor did not. HotSpot has
/// `m.getPackages().equals(m.getDescriptor().packages())`.
///
/// A premise in a comment is not a link to the thing it describes: this one
/// outlived the day the registry learned to enumerate the boot modules, and
/// the special case kept the stale answer alive for exactly the two names the
/// registry knows best.
///
/// The fallback direction is unchanged and still deliberate — an OVER-inclusive
/// package set is safe for the `getPackages().contains(pn)` callers this file
/// serves, an under-inclusive one is not — so an empty registry answer still
/// falls back to the hand list rather than to nothing.
fn module_package_names(ctx: &mut dyn NativeContext, name: &str) -> Vec<String> {
    let from_registry: Vec<String> = ctx
        .module_packages(name)
        .iter()
        .map(|p| dotted(p))
        .collect();
    if !from_registry.is_empty() {
        return from_registry;
    }
    match name {
        "java.base" => BOOT_JDK_PACKAGES.iter().map(|s| s.to_string()).collect(),
        "java.xml" => JAVA_XML_PACKAGES.iter().map(|s| s.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Wrap `set` in `java.util.Collections.unmodifiableSet`.
///
/// Every `Module`/`ModuleDescriptor` collection accessor is immutable on
/// HotSpot — `getPackages()`, and the descriptor's `packages()`, `uses()`,
/// `exports()`, `opens()`, `requires()` and `provides()` all raise
/// `UnsupportedOperationException` from `add`/`clear` and from
/// `iterator().remove()`. This file handed back the live `HashSet` it had just
/// built, so a caller could edit the VM's published view of the module graph
/// and hand it on; nothing downstream could tell the edited set from a real
/// one.
///
/// On failure the plain set is returned: an immutability wrapper is not worth
/// turning a correct answer into a thrown exception. Note the `--synthetic-jdk`
/// caveat recorded on `jca::provider_chain::wrap_unmodifiable` — in that mode
/// `Collections.unmodifiableSet` is bound to the identity function, so this
/// wrapper is inert there and a probe will still report a mutable set.
fn wrap_unmodifiable_set(ctx: &mut dyn NativeContext, set: ObjectRef) -> ObjectRef {
    match ctx.invoke(
        "java/util/Collections",
        "unmodifiableSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        &[Value::Object(Some(set))],
    ) {
        Ok(Some(Value::Object(Some(view)))) => view,
        _ => set,
    }
}

/// Record `name`'s package set against `module` for `native_module_get_packages`.
fn record_module_packages(ctx: &mut dyn NativeContext, module: ObjectRef, name: &str) {
    let packages = module_package_names(ctx, name);
    let id = ctx.identity_hash_code(module);
    let mut t = module_packages_table().lock().unwrap();
    module_packages_evict_if_needed(&mut t, id);
    t.insert(id, packages);
}

/// Build a Module for `name`, bound to the boot layer.
///
/// For a module the VM actually has a descriptor for, this returns THE
/// canonical mirror — the same object `Class.getModule()` publishes through
/// `NativeContext::{get,cache}_cached_module_mirror`. `java.lang.Module` does
/// not override `equals`, so every JDK comparison of two Modules is `==`; a
/// fresh Module per `findModule` call made
/// `Greeter.class.getModule() == ModuleLayer.boot().findModule(m).get()` false
/// (measured: `regression-suite/src/RJdkModule.java:139`, HotSpot passes).
///
/// Fabricated stand-ins for names the registry does NOT know are deliberately
/// left out of that cache: they are a permissive fallback, not a fact about the
/// module graph, and `cache_module_mirror` installs a permanent GC root.
fn build_module(
    ctx: &mut dyn NativeContext,
    name: &str,
    layer: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let registered = ctx.module_is_registered(name);
    // `layer` arrives as a bare `ObjectRef`. Everything below it —
    // `try_alloc_concurrent_synthetic`, `create_string`,
    // `build_module_descriptor` — can run a moving young collection, and a
    // relocated `layer` then gets STORED into the new Module's slot 0.
    //
    // That is not hypothetical and it is not benign: it is the defect
    // `docs/known-issues/springboot/bindabletests-local-holds-an-interior-word-
    // of-a-retired-tlab-filler-20260909.md` chased for two days. Under
    // `CRATONVM_DBG_GC_STRESS` the pre-move address is handed back to the
    // allocator within a cycle or two, so `Module.getLayer()` returns whatever
    // was minted there next, and `[rset-verify]` reported the resulting
    // `java/lang/Module slot=0 -> <dead young address>` edge on 3 560 of 3 594
    // moving cycles. The caller already pins `layer` across
    // `populate_boot_layer_modules` for exactly this reason; the pin has to
    // extend through this function too, because this is where the allocations
    // are.
    let layer_pin = ctx.pin_native_root(layer);
    if registered {
        if let Some(cached) = ctx.get_cached_module_mirror(Some(name)) {
            // `Class.getModule()`'s builder never sets `layer`; seed it so
            // `getLayer() == ModuleLayer.boot()` holds on the shared mirror.
            if !matches!(
                ctx.get_field_by_name(cached, "layer"),
                Value::Object(Some(_))
            ) {
                let layer = ctx.read_native_pin(layer_pin, layer);
                ctx.set_field_by_name(cached, "layer", Value::Object(Some(layer)));
            }
            record_module_packages(ctx, cached, name);
            ctx.unpin_native_roots(layer_pin);
            return Ok(cached);
        }
    }
    let module = match try_alloc_concurrent_synthetic(ctx, "java/lang/Module", MODULE_FIELD_COUNT) {
        Ok(m) => m,
        Err(e) => {
            ctx.unpin_native_roots(layer_pin);
            return Err(e);
        }
    };
    let pin = ctx.pin_native_root(module);
    let name_str = ctx.create_string(name);
    let module = ctx.read_native_pin(pin, module);
    ctx.set_field_by_name(module, "name", Value::Object(Some(name_str)));
    // NB: no raw slot write here. Slot 0 of a REAL `java.lang.Module` is
    // `layer`, not `name` — see the `MODULE_FIELD_COUNT` doc comment for the
    // Elasticsearch failure a slot-indexed write on this object already cost.
    let layer = ctx.read_native_pin(layer_pin, layer);
    ctx.set_field_by_name(module, "layer", Value::Object(Some(layer)));
    let desc = match build_module_descriptor(ctx, name) {
        Ok(d) => d,
        Err(e) => {
            ctx.unpin_native_roots(layer_pin);
            return Err(e);
        }
    };
    let module = ctx.read_native_pin(pin, module);
    ctx.set_field_by_name(module, "descriptor", Value::Object(Some(desc)));
    ctx.unpin_native_roots(layer_pin);
    // Recorded off-object in `module_packages_table` (see its doc comment)
    // instead of a field slot; `native_module_get_packages` reads it back
    // the same way.
    record_module_packages(ctx, module, name);
    if registered {
        ctx.cache_module_mirror(Some(name), module);
    }
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
fn wrap_optional_present(
    ctx: &mut dyn NativeContext,
    value: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
    ctx.set_field(opt, 0, Value::Object(Some(value)));
    Ok(opt)
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

    // A layer built by `ModuleLayer.defineModules` is its OWN authority, and
    // the boot `ModuleRegistry` knows nothing about it.
    //
    // `bootLayer.defineModules(cf, ...)` runs real JDK bytecode and populates
    // the layer's canonical `nameToModule` map. Answering such a receiver from
    // the boot registry is simply asking the wrong object: the name is absent
    // there by construction, so `findModule` returned `Optional.empty()` for a
    // module that the layer itself holds. Measured 2026-08-10 with
    // `probes/MLProbe.java`, which replicates
    // `com.sun.org.apache.xalan.internal.xsltc.trax.TemplatesImpl.createModule`
    // line for line:
    //
    //   HotSpot   nameToModule = {cratonvm.dyn.translet=module …}
    //             findModule(…) = Optional[module cratonvm.dyn.translet]
    //   CratonVM  nameToModule = {cratonvm.dyn.translet=module …}   <- populated
    //             findModule(…) = Optional.empty                    <- only this
    //
    // `TemplatesImpl.createModule` ends in `layer.findModule(mn).get()`, so the
    // empty Optional surfaced as `NoSuchElementException: No value present` out
    // of XSLTC — taking every `javax.xml.transform` consumer with it (8 classes
    // in the 2026-08-10 Spring sweep: the XMLUnit comparison family plus
    // `XsltViewTests`).
    //
    // `ModuleLayer.modules()` already reads this same map
    // (`native_module_layer_modules`), which is why `layer.modules()` listed the
    // module that `layer.findModule` could not find. This makes the two agree.
    //
    // Authoritative in BOTH directions: when the receiver carries the map, a
    // miss is a real absence and must answer empty rather than falling through
    // to the permissive fabrication below. Our synthetic boot layer has no such
    // field, so it takes none of this path and keeps its existing behaviour.
    if let Some(Value::Object(Some(layer))) = args.first() {
        if let Value::Object(Some(name_to_module)) = ctx.get_field_by_name(*layer, "nameToModule") {
            let map_pin = ctx.pin_native_root(name_to_module);
            let name_pin = ctx.pin_native_root(name_obj);
            let map = ctx.read_native_pin(map_pin, name_to_module);
            let key = ctx.read_native_pin(name_pin, name_obj);
            let got = ctx.invoke_virtual(
                map,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(key))],
            );
            ctx.unpin_native_roots(map_pin);
            ctx.unpin_native_roots(name_pin);
            return match got? {
                Some(Value::Object(Some(module))) => {
                    let opt = wrap_optional_present(ctx, module)?;
                    Ok(Some(Value::Object(Some(opt))))
                }
                _ => ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]),
            };
        }
    }

    // An ABSENT module must answer `Optional.empty()`.
    //
    // This used to fabricate a Module for any syntactically valid name, so
    // `ModuleLayer.boot().findModule(anything).isPresent()` was unconditionally
    // true. Two costs, both measured: `RJdkFailure.java:274`
    // (`findModule("cratonvm.no.such.module").isEmpty()`) failed outright, and
    // `RJdkModule.java:51`'s "was --module-path passed?" check passed
    // VACUOUSLY — which is why the real module defect only surfaced several
    // checks downstream.
    //
    // Absence is only evidence of absence once the boot `ModuleRegistry` has
    // actually been populated, and `java.base` is the one name that is always
    // in a populated registry (`ClassManager`'s boot `module-info` scan, plus
    // `vm_init`'s explicit real-JDK fallback registration). When it is missing
    // the registry has told us nothing, so the legacy permissive fabrication
    // stands — that keeps synthetic-jdk and any unpopulated-registry embedder
    // on exactly its previous behaviour.
    let registry_populated = ctx.module_is_registered("java.base");
    if registry_populated && !ctx.module_is_registered(&name) && name != "java.xml" {
        return ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]);
    }

    let layer_ref = match args.first() {
        Some(Value::Object(Some(l))) => *l,
        _ => build_boot_layer(ctx)?,
    };
    let module = build_module(ctx, &name, layer_ref)?;
    let opt = wrap_optional_present(ctx, module);
    Ok(Some(Value::Object(Some(opt?))))
}

/// `Module.getResourceAsStream(String)`.
///
/// Registered nowhere before this — `RJdkModule.java:202/208/214/218` are the
/// four checks that need it, and real JDK bytecode for this method routes
/// through `BuiltinClassLoader.findResourceAsStream` / a `ModuleReader`, neither
/// of which CratonVM models. Companion entry required in
/// `vm/src/runtime/interpreter/native_override.rs::force_native_over_real_jdk_bytecode`
/// or the real bytecode shadows this in real-JDK mode.
///
/// Encapsulation. Transcribed from the JDK 25 source
/// (`lib/src.zip!java.base/java/lang/Module.java`, method body reproduced here
/// because it is the whole specification of this native):
///
/// ```text
/// if (name.startsWith("/")) name = name.substring(1);
/// if (isNamed() && Resources.canEncapsulate(name)) {
///     Module caller = getCallerModule(Reflection.getCallerClass());
///     if (caller != this && caller != Object.class.getModule()) {
///         String pn = Resources.toPackageName(name);
///         if (getPackages().contains(pn)) {
///             if (caller == null) { if (!isOpen(pn)) return null; }
///             else if (!isOpen(pn, caller)) return null;
///         }
///     }
/// }
/// ```
///
/// The load-bearing point, and the reason this is NOT the rule the constructor
/// gate uses: the test is `isOpen`, never `isExported`. `exports` grants
/// compile/link access to the package's public API; it grants NOTHING for
/// resources. `java.base` exports `java.util` without opening it, so a public
/// `new ArrayList()` must be allowed while `java.base
/// .getResourceAsStream("java/util/x.properties")` must not — the two gates
/// answer opposite questions and must stay separate. `jdk.internal.module
/// .Resources.canEncapsulate` supplies the `.class` carve-out
/// (`len > 6 && endsWith(".class")` ⇒ never encapsulated) and
/// `Resources.toPackageName` the package derivation (text before the LAST
/// `'/'`; `""` when there is no `'/'` or the name ends in one — which is why
/// `META-INF/MANIFEST.MF` and top-level names are never encapsulated).
///
/// Deviations from that source, both deliberate:
///
///   * `Checks.isPackageName(pn)` is not re-implemented. The membership test
///     against the module's own package set is strictly stronger: a string
///     that is not a legal package name cannot be a registered package.
///   * `Reflection.getCallerClass()` has no equivalent here, so the caller
///     module is recovered from the interpreter frame stack
///     (`resource_caller_module`). When the stack names no caller at all the
///     UNQUALIFIED `isOpen(pn)` is used as the fallback, matching the JDK's
///     own `caller == null` arm.
///
/// Resolution of a non-encapsulated name is delegated to
/// `classloader::module_get_resource_as_stream` rather than re-done here: that
/// is the callback this triple actually dispatched to before this fix (see the
/// registration note below), and it carries the T19.H10 name validation, the
/// leading-`/` strip, the dynamically-DEFINED-class `.class` fallback and the
/// SB-15 kotlin-reflect classpath behaviour. Re-implementing the byte fetch
/// here would have been a second copy of a rule that already exists — the same
/// mistake that let the `reads` bug survive three waves.
pub(crate) fn native_module_get_resource_as_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Module.getResourceAsStream: receiver must not be null".to_string()),
            }
            .into());
        }
    };
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Module.getResourceAsStream: name must not be null".to_string()),
            }
            .into());
        }
    };

    // `name.startsWith("/") -> substring(1)` happens BEFORE the package is
    // derived, so a caller spelling the resource `"/com/x/secret.txt"` is
    // encapsulated exactly like `"com/x/secret.txt"`.
    let res_name: &str = name.strip_prefix('/').unwrap_or(name.as_str());

    // `isNamed()` — the registry's unnamed-module sentinel is the empty name.
    let module_name = module_registry_name(ctx, this);
    if !module_name.is_empty() && resource_can_encapsulate(res_name) {
        let pkg = resource_package_name(res_name);
        // `getPackages().contains(pn)`: a name outside the module's own
        // packages is not encapsulated. `ModuleRegistry` keys packages in
        // internal (slash) form and so does `pkg`, so this compares like with
        // like — `module_package_names` is the one that dots them, for Java.
        if !pkg.is_empty() && ctx.module_packages(&module_name).iter().any(|p| p == pkg) {
            // Bound to a local first: a `&mut ctx` reborrow inside a match
            // scrutinee is live for the whole match, and the arms below need
            // `ctx` again.
            let caller_module = resource_caller_module(ctx);
            let encapsulated = match caller_module {
                // `caller != Object.class.getModule()`: java.base reads
                // everything, and CratonVM's own JDK-internal resource
                // plumbing runs there.
                Some(caller) if caller == "java.base" => false,
                // `isOpen(pn, caller)`. This also supplies the JDK's
                // `caller != this` exemption: `is_package_open_to` answers
                // true whenever the two module names are equal.
                Some(caller) => !ctx.is_package_open_to(&module_name, pkg, &caller),
                // The JDK's `caller == null` arm: unqualified `isOpen(pn)`.
                None => !ctx.is_package_open_unqualified(&module_name, pkg),
            };
            if encapsulated {
                return Ok(Some(Value::Object(None)));
            }
        }
    }

    crate::classloader::module_get_resource_as_stream(ctx, args)
}

/// `jdk.internal.module.Resources.canEncapsulate` — a name ending in `.class`
/// (and longer than `".class"` itself) is never encapsulated.
fn resource_can_encapsulate(name: &str) -> bool {
    !(name.len() > 6 && name.ends_with(".class"))
}

/// `jdk.internal.module.Resources.toPackageName`, kept in slash form because
/// that is how `ModuleRegistry` keys packages. Empty when the name has no
/// `'/'` or ends with one (a directory), which is the JDK's "not in a package,
/// therefore not encapsulated" answer.
fn resource_package_name(name: &str) -> &str {
    match name.rfind('/') {
        Some(pos) if pos + 1 < name.len() => &name[..pos],
        _ => "",
    }
}

/// The calling module's registry name, i.e. what the JDK gets from
/// `getCallerModule(Reflection.getCallerClass())`.
///
/// Frame walk copied from `lang_class::class_for_name_one_arg_caller_loader`:
/// `frame_class_ids` is innermost-first, and the innermost frames can be
/// `java/lang/Module` itself (this native is force-dispatched from there), which
/// is not the caller the JDK means.
///
/// `None` and `Some(String::new())` are DIFFERENT answers and the caller relies
/// on it: `None` is "the stack named nobody" (fall back to the unqualified
/// open test), while an empty string is the unnamed module — a real, ordinary
/// classpath caller, which is what this vector's `RJdkModule` is.
fn resource_caller_module(ctx: &mut dyn NativeContext) -> Option<String> {
    for cid in ctx.frame_class_ids() {
        if matches!(
            ctx.class_name_arc_of_id(cid).as_deref(),
            Some("java/lang/Module")
        ) {
            continue;
        }
        return Some(ctx.module_name_of_class(cid).unwrap_or_default());
    }
    None
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
    let names = match recorded {
        Some(names) => names,
        None => {
            // No row: this Module was not built here — e.g. the canonical
            // mirror `Class.getModule()` publishes. Ask the registry for the
            // module's real package set before falling back to the permissive
            // whole-of-java.base default.
            let module_name = module_registry_name(ctx, this);
            let from_registry = if module_name.is_empty() {
                Vec::new()
            } else {
                module_package_names(ctx, &module_name)
            };
            if from_registry.is_empty() {
                BOOT_JDK_PACKAGES.iter().map(|s| s.to_string()).collect()
            } else {
                from_registry
            }
        }
    };
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let set = build_package_set(ctx, &refs)?;
    let set = wrap_unmodifiable_set(ctx, set);
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
    build_export_like(
        ctx,
        "java/lang/module/ModuleDescriptor$Exports",
        package_name,
        &[],
    )
}

// ---------------------------------------------------------------------------
// ModuleDescriptor from the VM's own ModuleRegistry
// ---------------------------------------------------------------------------
//
// Everything below builds the Java-side `java.lang.module.ModuleDescriptor`
// graph for a module name out of the descriptor CratonVM already parsed from
// that module's `module-info.class` (`classloading::module::parse_module_info`
// → `ClassManager::module_registry`).
//
// It replaces a fabrication: `build_synthetic_module_descriptor` used to set
// `requires`/`exports`/`opens`/`provides`/`packages` to EMPTY sets
// unconditionally, for every module, in every mode — so
// `ModuleLayer.boot().findModule("m").get().getDescriptor().exports()` answered
// `[]` no matter what `m` actually declares. Measured against HotSpot 25 with
// `regression-suite/src/RJdkModule.java` (`--module-path build-modules
// --add-modules cratonvm.jdkonly.svc`): HotSpot reports
// `exports=[com.cratonvm.jdkonly.svc, com.cratonvm.jdkonly.svc.open]`, CratonVM
// reported `[]` and the vector died at `RJdkModule.java:72`.
//
// The registry stores names in INTERNAL (slash) form; every `java.lang.module`
// API speaks BINARY (dot) form, so each name crosses `dotted` on the way out.
// `requires`' module names are already dot-form module names and must NOT be
// rewritten.

/// Registry (slash) form → Java (dot) form.
fn dotted(name: &str) -> String {
    name.replace('/', ".")
}

/// Append `value` to a `java.util.List` (the `providers()` list is a List, not
/// a Set — `ModuleDescriptor.Provides.providers()` is spec'd ordered).
fn list_add(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    value: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    ctx.invoke(
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(value))],
    )?;
    Ok(())
}

/// Build a `java.util.HashSet<String>` from `items` through the REAL
/// constructor + `add`, never through a hand-laid slot triple.
///
/// `phases_late::build_string_set` (which `build_synthetic_module_descriptor`
/// used for `uses`) writes the legacy synthetic `(array, size, capacity)`
/// HashSet shape; real `java.util.HashSet` has a single `map` field, so real
/// `size()`/`iterator()`/`stream()` bytecode reads an `Object[]` where it
/// expects a `HashMap`. Same bug class as the `build_package_set` doc comment
/// above.
fn build_string_hash_set(
    ctx: &mut dyn NativeContext,
    items: &[String],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let set = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "descriptor strings")?;
    let pin = ctx.pin_native_root(set);
    for item in items {
        let s = ctx.create_string(item);
        let set = ctx.read_native_pin(pin, set);
        collection_add(ctx, set, s)?;
    }
    let set = ctx.read_native_pin(pin, set);
    ctx.unpin_native_roots(pin);
    Ok(set)
}

/// Build one `ModuleDescriptor$Exports` or `$Opens`.
///
/// Both classes have the identical private shape `(Set mods, String source,
/// Set<String> targets)` — verified with
/// `javap -p java.lang.module.ModuleDescriptor$Exports` on JDK 25 — and their
/// `source()` / `isQualified()` are plain field reads (`isQualified` is
/// `!targets.isEmpty()`), so populating `targets` is what makes a QUALIFIED
/// export report itself as qualified.
///
/// `mods` is an empty set: the registry keeps no `ACC_SYNTHETIC`/`ACC_MANDATED`
/// bit per exports entry (`ModuleExportsEntry` has only package + targets), so
/// there is no data source for `modifiers()`. Empty is the answer for every
/// `javac`-emitted export anyway, and it keeps real `Exports.hashCode()`
/// (`modsHashCode(mods)`) off a null.
fn build_export_like(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    package_name: &str,
    targets: &[String],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, class_name, 4)?;
    let pin = ctx.pin_native_root(obj);

    let mods = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "export mods")?;
    let obj = ctx.read_native_pin(pin, obj);
    ctx.set_field_by_name(obj, "mods", Value::Object(Some(mods)));

    let targets_set = build_string_hash_set(ctx, targets)?;
    let obj = ctx.read_native_pin(pin, obj);
    ctx.set_field_by_name(obj, "targets", Value::Object(Some(targets_set)));

    let source = ctx.create_string(package_name);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.set_field_by_name(obj, "source", Value::Object(Some(source)));

    ctx.unpin_native_roots(pin);
    Ok(obj)
}

/// Build the `Set<Exports>` / `Set<Opens>` for a whole module.
///
/// Each element is created and immediately handed to `HashSet.add` — element
/// refs are never held in a Rust local across another allocating call, because
/// `add` re-enters Java (`hashCode`) and can move the heap.
fn build_export_like_set(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    entries: &[(String, Vec<String>)],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let set = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "exports/opens")?;
    let pin = ctx.pin_native_root(set);
    for (package_name, targets) in entries {
        let element = build_export_like(ctx, class_name, package_name, targets)?;
        let set = ctx.read_native_pin(pin, set);
        collection_add(ctx, set, element)?;
    }
    let set = ctx.read_native_pin(pin, set);
    ctx.unpin_native_roots(pin);
    Ok(set)
}

/// Build one `ModuleDescriptor$Provides` — `(String service, List<String>
/// providers)`.
fn build_provides(
    ctx: &mut dyn NativeContext,
    service: &str,
    providers: &[String],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor$Provides", 2)?;
    let pin = ctx.pin_native_root(obj);

    let list = new_initialized_object(ctx, "java/util/ArrayList", "()V", &[], "providers")?;
    let list_pin = ctx.pin_native_root(list);
    for p in providers {
        let s = ctx.create_string(p);
        let list = ctx.read_native_pin(list_pin, list);
        list_add(ctx, list, s)?;
    }
    let list = ctx.read_native_pin(list_pin, list);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.set_field_by_name(obj, "providers", Value::Object(Some(list)));
    ctx.unpin_native_roots(list_pin);

    let service_str = ctx.create_string(service);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.set_field_by_name(obj, "service", Value::Object(Some(service_str)));
    ctx.unpin_native_roots(pin);
    Ok(obj)
}

/// Build the `Set<Provides>` for a whole module.
fn build_provides_set(
    ctx: &mut dyn NativeContext,
    entries: &[(String, Vec<String>)],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let set = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "provides")?;
    let pin = ctx.pin_native_root(set);
    for (service, providers) in entries {
        let element = build_provides(ctx, service, providers)?;
        let set = ctx.read_native_pin(pin, set);
        collection_add(ctx, set, element)?;
    }
    let set = ctx.read_native_pin(pin, set);
    ctx.unpin_native_roots(pin);
    Ok(set)
}

/// The binary name of the `requires` modifier enum.
///
/// Spelling a fixed JDK type as a literal is not the "bind by NAME" hazard this
/// campaign keeps hitting — that one is a *decision* keyed on a rendered class /
/// module / package name that varies with the code under test. This is the same
/// kind of constant as the `java/util/HashSet` literals throughout this file:
/// one specific JDK class, resolved once, compared against nothing.
const REQUIRES_MODIFIER_ENUM: &str = "java/lang/module/ModuleDescriptor$Requires$Modifier";

/// Read one enum constant out of an already-initialized enum's statics.
///
/// Pure reads only — `static_field_index_by_name` and `get_static_field` neither
/// allocate nor re-enter Java, so a caller may hold an unpinned `ObjectRef`
/// across this. Initializing the enum (which DOES run Java) is the caller's job,
/// deliberately hoisted out of the allocation-bearing loop below.
///
/// `None` means the constant could not be produced — field absent, or a null
/// static because the enum never really initialized (a synthetic-JDK stand-in).
/// The caller must then leave that modifier out rather than write a null into a
/// set whose `hashCode()` real JDK bytecode will walk.
fn enum_constant(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    constant: &str,
) -> Option<ObjectRef> {
    let index = ctx.static_field_index_by_name(class_id, constant)?;
    match ctx.get_static_field(class_id, index) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// The binary name of the module-level modifier enum. Same constant-literal
/// rationale as [`REQUIRES_MODIFIER_ENUM`].
const MODULE_MODIFIER_ENUM: &str = "java/lang/module/ModuleDescriptor$Modifier";

/// Build `ModuleDescriptor.modifiers()`.
///
/// This set was previously empty for every module unconditionally, which is a
/// false positive claim for an open module: the javadoc for
/// `ModuleDescriptor.isOpen()` is "Returns true if this is an open module", and
/// `newOpenModule` is specified to build a descriptor whose modifiers contain
/// `Modifier.OPEN`, so `modifiers().contains(OPEN)` and `isOpen()` are two
/// spellings of one fact. `isOpen()` was already answered truthfully from
/// `module_is_open`, so `modifiers()` disagreeing with it was an internal
/// contradiction, not merely a missing feature — and it is fixed here from data
/// the `NativeContext` already exposes.
///
/// The other three constants are NOT fabricated:
///
/// * `AUTOMATIC` — the registry knows this (`ModuleDescriptor::automatic`) but
///   no `NativeContext` accessor surfaces it, so this native cannot ask. Note
///   the same gap makes `isAutomatic()` itself a hardcoded `false` in
///   `build_module_descriptor`; both want one new accessor, and the patch is in
///   the W2-3 known-issues record.
/// * `SYNTHETIC` / `MANDATED` — no module-level flag word survives
///   `descriptor_from_module_attribute`, which extracts only `ACC_MODULE_OPEN`
///   from the `Module` attribute's `flags`.
///
/// Leaving those out understates the set rather than inventing membership,
/// which is the safe direction: a caller testing `contains(X)` gets a false
/// negative, never a false positive.
fn build_module_modifier_set(
    ctx: &mut dyn NativeContext,
    is_open: bool,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    // Hoisted for the same reason as in `build_requires_set`: `<clinit>` runs
    // Java and can move the heap, so it must not run while `set` is live.
    let modifier_enum = if is_open {
        ctx.ensure_class_initialized(MODULE_MODIFIER_ENUM).ok()
    } else {
        None
    };
    let set = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "modifiers")?;
    if let Some(enum_id) = modifier_enum {
        if let Some(value) = enum_constant(ctx, enum_id, "OPEN") {
            collection_add(ctx, set, value)?;
        }
    }
    Ok(set)
}

/// Build the `Set<Requires>` for a whole module.
///
/// `Requires` is `(Set mods, String name, Version compiledVersion, String
/// rawCompiledVersion)`. `name()` is a plain field read, which is the accessor
/// every caller in the corpus uses, and `modifiers()` is a plain field read of
/// `mods`.
///
/// `mods` used to be unconditionally empty, discarding the transitive/static
/// bits `module_requires` already hands us — the registry has carried them the
/// whole time (`ModuleRegistry::build_readability_graph` consumes them on the
/// Rust side); they were simply dropped on the way into the Java mirror. They
/// are now minted from the REAL enum's static constants, because `Enum.equals`
/// is identity: a caller's `mods.contains(Requires.Modifier.TRANSITIVE)` can
/// only answer true if the set holds the genuine singleton, and a fabricated
/// stand-in would compare unequal and read as "not transitive" — a wrong answer
/// dressed as a right one.
///
/// Still absent: `MANDATED` / `SYNTHETIC`. Those bits ARE parsed now
/// (`ModuleRequiresEntry::{is_mandated,is_synthetic}`), but `module_requires`'
/// `(String, bool, bool)` tuple has no room to carry them and widening it means
/// editing `native-api` and `vm`, outside this lane's files — the exact patch is
/// recorded in the W2-3 known-issues record. Until it lands `requires java.base`
/// reports `[]` where HotSpot reports `[MANDATED]`: a NARROWING of the existing
/// gap, not a new one.
fn build_requires_set(
    ctx: &mut dyn NativeContext,
    entries: &[(String, bool, bool)],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    // Resolve the modifier enum ONCE, before the first allocation below.
    // `ensure_class_initialized` runs the enum's `<clinit>`, i.e. arbitrary
    // Java, which can move the heap — doing it inside the loop would expose a
    // live ref to a collection between a pin and its read. Skipped entirely
    // when no entry carries a modifier, which is the overwhelmingly common
    // shape (`requires <plain>`), so the ordinary path never drags the enum
    // through initialization at all.
    //
    // An initialization failure is swallowed rather than propagated: this
    // enum's `<clinit>` is four `new Modifier(int)` calls and an array, so it
    // cannot realistically throw, and a descriptor missing its modifier set is
    // a far better outcome than `getDescriptor()` itself failing.
    let modifier_enum = if entries
        .iter()
        .any(|(_, transitive, is_static)| *transitive || *is_static)
    {
        ctx.ensure_class_initialized(REQUIRES_MODIFIER_ENUM).ok()
    } else {
        None
    };

    let set = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "requires")?;
    let pin = ctx.pin_native_root(set);
    for (name, transitive, is_static) in entries {
        let element =
            try_alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor$Requires", 4)?;
        let element_pin = ctx.pin_native_root(element);

        let mut mods =
            new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "requires mods")?;
        let mods_pin = ctx.pin_native_root(mods);
        if let Some(enum_id) = modifier_enum {
            for (wanted, constant) in [(*transitive, "TRANSITIVE"), (*is_static, "STATIC")] {
                if !wanted {
                    continue;
                }
                // Re-read the pin first: a previous `collection_add` re-entered
                // Java (`Enum.hashCode`) and may have moved `mods`. Assigned
                // back into the outer binding rather than shadowed inside the
                // loop body, so the second iteration re-reads from the updated
                // ref instead of handing `read_native_pin` a stale fallback.
                mods = ctx.read_native_pin(mods_pin, mods);
                if let Some(value) = enum_constant(ctx, enum_id, constant) {
                    collection_add(ctx, mods, value)?;
                }
            }
        }
        let mods = ctx.read_native_pin(mods_pin, mods);
        ctx.unpin_native_roots(mods_pin);
        let element = ctx.read_native_pin(element_pin, element);
        ctx.set_field_by_name(element, "mods", Value::Object(Some(mods)));

        let name_str = ctx.create_string(name);
        let element = ctx.read_native_pin(element_pin, element);
        ctx.set_field_by_name(element, "name", Value::Object(Some(name_str)));
        ctx.unpin_native_roots(element_pin);
        let set = ctx.read_native_pin(pin, set);
        collection_add(ctx, set, element)?;
    }
    let set = ctx.read_native_pin(pin, set);
    ctx.unpin_native_roots(pin);
    Ok(set)
}

/// Build a `java.lang.module.ModuleDescriptor` for `module_name`, answering
/// from the VM's `ModuleRegistry` instead of fabricating empty collections.
///
/// This is the single implementation behind
/// `crate::build_synthetic_module_descriptor`, i.e. behind every
/// `Module.getDescriptor()` / `Class.getModule().getDescriptor()` /
/// `ModuleLayer.findModule(..).get().getDescriptor()` path.
///
/// A module the registry does not know still gets the old all-empty answer —
/// that is the correct shape for a fabricated stand-in and keeps every
/// synthetic-jdk / unpopulated-registry caller on exactly its previous
/// behaviour. The only thing that changes is that a module the VM DID parse now
/// reports what it parsed.
pub(crate) fn build_module_descriptor(
    ctx: &mut dyn NativeContext,
    module_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // Snapshot every registry answer BEFORE the first allocation: these are
    // `&self` reads on the class manager and must not interleave with the Java
    // re-entry below.
    let is_open = ctx.module_is_open(module_name);
    let uses: Vec<String> = ctx
        .module_uses(module_name)
        .iter()
        .map(|s| dotted(s))
        .collect();
    let packages: Vec<String> = ctx
        .module_packages(module_name)
        .iter()
        .map(|s| dotted(s))
        .collect();
    let exports: Vec<(String, Vec<String>)> = ctx
        .module_exports(module_name)
        .into_iter()
        .map(|(pkg, targets)| (dotted(&pkg), targets))
        .collect();
    let opens: Vec<(String, Vec<String>)> = ctx
        .module_opens(module_name)
        .into_iter()
        .map(|(pkg, targets)| (dotted(&pkg), targets))
        .collect();
    let provides: Vec<(String, Vec<String>)> = ctx
        .module_provides(module_name)
        .into_iter()
        .map(|(service, with)| {
            (
                dotted(&service),
                with.iter().map(|s| dotted(s)).collect::<Vec<String>>(),
            )
        })
        .collect();
    let requires = ctx.module_requires(module_name);

    let desc = try_alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor", 16)?;
    let pin = ctx.pin_native_root(desc);
    let name = ctx.create_string(module_name);
    let desc = ctx.read_native_pin(pin, desc);
    let name_val = Value::Object(Some(name));
    // Slots 0/1 are the LEGACY SYNTHETIC contract (`name`, flags word) that
    // `native_module_descriptor_is_open`'s fallback reads when the named fields
    // are absent. Write them only when this really is a fabricated class: on
    // the real JDK's `ModuleDescriptor`, slot 1 is `version`
    // (a `ModuleDescriptor$Version` reference), and the unconditional
    // `set_field(desc, 1, Value::Int(0))` this replaces was parking an int in
    // an object cell — the same "assumes a private synthetic layout on a class
    // that is actually real bytecode" shape as the Module/HashSet field bugs
    // documented above.
    let has_named_layout = ctx
        .resolve_field_index("java/lang/module/ModuleDescriptor", "open")
        .is_some();
    if !has_named_layout {
        ctx.set_field(desc, 0, name_val);
        ctx.set_field(desc, 1, Value::Int(if is_open { 1 } else { 0 }));
    }
    ctx.set_field_by_name(desc, "name", name_val);
    ctx.set_field_by_name(desc, "open", Value::Int(if is_open { 1 } else { 0 }));
    // UNSOURCED, and knowingly so. The registry does record whether a module is
    // automatic (`ModuleDescriptor::automatic`, set for a `module-info.class`
    // found on the CLASS path), but no `NativeContext` accessor surfaces it, so
    // this native cannot ask and writes the majority answer instead. That makes
    // `RJdkModule`'s `check(!d.isAutomatic(), ...)` pass for the wrong reason —
    // it would pass against a hardcoded `false` whatever the module really is.
    // One accessor fixes this and `Modifier.AUTOMATIC` together; the patch is in
    // the W2-3 known-issues record.
    ctx.set_field_by_name(desc, "automatic", Value::Int(0));

    // `version`, `rawVersionString` and `mainClass` are deliberately LEFT NULL
    // rather than set to anything. Real `ModuleDescriptor.version()` /
    // `rawVersion()` / `mainClass()` are `Optional.ofNullable(field)`, so a null
    // field already renders as `Optional.empty()` — the honest "nothing was
    // recorded" answer, and the correct one for a module-info that carries no
    // version. It is NOT correct for one that does: `descriptor_from_module_attribute`
    // has always parsed the module version into `ModuleDescriptor::version`, and
    // `requires`' compiled version is parsed as of this change, but neither
    // crosses `NativeContext`. Writing a fabricated value here would turn a
    // truthful empty into a false claim, so nothing is written.

    // `modifiers` carries OPEN when the module is open; the remaining three
    // constants have no data source reachable from here. See
    // `build_module_modifier_set` for what is deliberately NOT fabricated.
    let modifiers = build_module_modifier_set(ctx, is_open)?;
    // Immutable: see `wrap_unmodifiable_set`. Placed BEFORE the pin re-read
    // because the wrapper allocates and can therefore move `desc`.
    let modifiers = wrap_unmodifiable_set(ctx, modifiers);
    let desc = ctx.read_native_pin(pin, desc);
    ctx.set_field_by_name(desc, "modifiers", Value::Object(Some(modifiers)));

    let uses_set = build_string_hash_set(ctx, &uses)?;
    // Immutable: see `wrap_unmodifiable_set`. Placed BEFORE the pin re-read
    // because the wrapper allocates and can therefore move `desc`.
    let uses_set = wrap_unmodifiable_set(ctx, uses_set);
    let desc = ctx.read_native_pin(pin, desc);
    ctx.set_field_by_name(desc, "uses", Value::Object(Some(uses_set)));

    let packages_set = build_string_hash_set(ctx, &packages)?;
    // Immutable: see `wrap_unmodifiable_set`. Placed BEFORE the pin re-read
    // because the wrapper allocates and can therefore move `desc`.
    let packages_set = wrap_unmodifiable_set(ctx, packages_set);
    let desc = ctx.read_native_pin(pin, desc);
    ctx.set_field_by_name(desc, "packages", Value::Object(Some(packages_set)));

    let exports_set =
        build_export_like_set(ctx, "java/lang/module/ModuleDescriptor$Exports", &exports)?;
    // Immutable: see `wrap_unmodifiable_set`. Placed BEFORE the pin re-read
    // because the wrapper allocates and can therefore move `desc`.
    let exports_set = wrap_unmodifiable_set(ctx, exports_set);
    let desc = ctx.read_native_pin(pin, desc);
    ctx.set_field_by_name(desc, "exports", Value::Object(Some(exports_set)));

    let opens_set = build_export_like_set(ctx, "java/lang/module/ModuleDescriptor$Opens", &opens)?;
    // Immutable: see `wrap_unmodifiable_set`. Placed BEFORE the pin re-read
    // because the wrapper allocates and can therefore move `desc`.
    let opens_set = wrap_unmodifiable_set(ctx, opens_set);
    let desc = ctx.read_native_pin(pin, desc);
    ctx.set_field_by_name(desc, "opens", Value::Object(Some(opens_set)));

    let provides_set = build_provides_set(ctx, &provides)?;
    // Immutable: see `wrap_unmodifiable_set`. Placed BEFORE the pin re-read
    // because the wrapper allocates and can therefore move `desc`.
    let provides_set = wrap_unmodifiable_set(ctx, provides_set);
    let desc = ctx.read_native_pin(pin, desc);
    ctx.set_field_by_name(desc, "provides", Value::Object(Some(provides_set)));

    let requires_set = build_requires_set(ctx, &requires)?;
    // Immutable: see `wrap_unmodifiable_set`. Placed BEFORE the pin re-read
    // because the wrapper allocates and can therefore move `desc`.
    let requires_set = wrap_unmodifiable_set(ctx, requires_set);
    let desc = ctx.read_native_pin(pin, desc);
    ctx.set_field_by_name(desc, "requires", Value::Object(Some(requires_set)));

    ctx.unpin_native_roots(pin);
    Ok(desc)
}

fn build_boot_resolved_module(
    ctx: &mut dyn NativeContext,
    cfg: ObjectRef,
    name: &str,
    package_names: &[&str],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let cfg_pin = ctx.pin_native_root(cfg);
    let md = try_alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor", 16)?;
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

    let mref = try_alloc_concurrent_synthetic(ctx, "jdk/internal/module/ModuleReferenceImpl", 8)?;
    let mref_pin = ctx.pin_native_root(mref);
    let md = ctx.read_native_pin(md_pin, md);
    ctx.set_field_by_name(mref, "descriptor", Value::Object(Some(md)));

    let resolved = try_alloc_concurrent_synthetic(ctx, "java/lang/module/ResolvedModule", 2)?;
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

    let cfg = try_alloc_concurrent_synthetic(ctx, "java/lang/module/Configuration", 5)?;
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

/// `java.lang.Module.addExportsToAll0(Module from, String pkg)` — record an
/// unqualified dynamic export (`exports pkg;`, reaching every module).
fn native_module_add_exports_to_all0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_module_add_exports_to0(ctx, args, "")
}

/// `java.lang.Module.addExportsToAllUnnamed0(Module from, String pkg)` —
/// record an export qualified to the unnamed module.
///
/// Not the same edge as `addExportsToAll0`, which is what this used to share.
/// `ALL-UNNAMED` reaches unnamed modules only, and HotSpot reports it as a
/// qualified export: `Module.isExported(pkg)` stays **false** afterwards.
fn native_module_add_exports_to_all_unnamed0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_module_add_exports_to0(ctx, args, ALL_UNNAMED_TARGET)
}

fn native_module_add_exports_to0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    target: &str,
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
    ctx.module_add_exports(&from_name, &pkg.replace('.', "/"), target);
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
    // `Module.getResourceAsStream(String)` had NO registration at all: in
    // real-JDK mode the real bytecode ran and dead-ended in
    // `BuiltinClassLoader`/`ModuleReader` machinery CratonVM does not model, and
    // in synthetic mode there was nothing to call. Needs the companion entry in
    // `force_native_over_real_jdk_bytecode` to win over the real bytecode.
    //
    // THIS REGISTRATION LOSES, and must stay anyway. `register()` is
    // last-registration-wins, and `native-builtins/src/lib.rs` re-registers the
    // very same triple later in the SAME function
    // (`register_essential_natives_with_shims` calls
    // `register_jboss_jdkspecific` at ~:9578 and registers
    // `java/lang/Module.getResourceAsStream` again at ~:18208). Until that site
    // was pointed at this callback, the encapsulation check above was DEAD CODE
    // and every module resource was served unconditionally — `RJdkModule.java:208`
    // ("a resource in a non-open package must NOT be readable from another
    // module") failed in both jdk modes while the three permissive checks around
    // it passed. Keeping this row means the gate is installed even if the lib.rs
    // registrar is ever reordered or dropped.
    registry.register(
        m,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        native_module_get_resource_as_stream,
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
    // Export to the unnamed module (`--add-exports …=ALL-UNNAMED`).
    //
    // This shared `addExportsToAll0`'s implementation until 2026-08-10, i.e. it
    // recorded an UNQUALIFIED export: the unnamed module's registry name is the
    // empty string (`classloading::module::UNNAMED_MODULE`) and `add_exports`
    // read an empty target as "to all modules". That was a deliberate widening
    // at the time, because the only alternative considered was dropping the
    // edge entirely, which produced spurious IllegalAccessErrors.
    //
    // The choice stopped being binary when `ALL_UNNAMED_TARGET` landed (it was
    // added for the `--add-exports`/`--add-opens` CLI path, which had the
    // identical conflation and was caught diffing `Module.isOpen` against
    // Temurin 25 — probes/AddOpensFlagProbe.java). It resolves to a QUALIFIED
    // edge naming the unnamed module: classpath code still gets its grant, so
    // the IllegalAccessError vector stays closed, while named modules stop
    // receiving one and `Module.isExported(pkg)` keeps answering false the way
    // HotSpot does.
    registry.register_with_kind(
        m,
        "addExportsToAllUnnamed0",
        "(Ljava/lang/Module;Ljava/lang/String;)V",
        native_module_add_exports_to_all_unnamed0,
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
            // The field is unset. THREE builders mint `java.lang.Module`
            // mirrors -- `build_module` here, `Class.getModule()`'s real-JDK
            // registration in lib.rs, and its synthetic twin in
            // `phases_late/reflect_invoke.rs` -- and whichever ran first wins
            // the `cache_module_mirror` slot. A module's loader is a property
            // of the JDK's own table, not of that race, so the fallback is
            // asked HERE, at the single read, rather than trusted to three
            // writes.
            //
            // Boot modules keep the spec-correct null this override was
            // written for; a PLATFORM module answers the platform loader,
            // matching `Class.getClassLoader()` for every class in it
            // (`java.sql.Connection` and `java.sql` must not disagree).
            let module_name = match ctx.get_field_by_name(this, "name") {
                Value::Object(Some(n)) => ctx.read_string(n),
                _ => match ctx.get_field(this, 0) {
                    Value::Object(Some(n)) => ctx.read_string(n),
                    _ => None,
                },
            };
            if let Some(name) = module_name {
                if let Some(platform) =
                    crate::classloader::platform_loader_for_module(ctx, &name)
                {
                    return Ok(Some(Value::Object(Some(platform))));
                }
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
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

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
        let module = build_module(&mut ctx, "java.base", layer)
            .expect("build_module must succeed for a synthetic layer");
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
        let module = build_module(&mut ctx, "java.sql", layer)
            .expect("build_module must succeed for a synthetic layer");
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
        let module = build_module(&mut ctx, "java.base", layer)
            .expect("build_module must succeed for a synthetic layer");
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
        let module = build_module(&mut ctx, "java.base", layer)
            .expect("build_module must succeed for a synthetic layer");
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
        let module = build_module(&mut ctx, "java.base", layer)
            .expect("build_module must succeed for a synthetic layer");
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
