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
use cratonvm_native_api::{LayerModuleEdgeTarget, NativeContext, NativeKind, NativeMethodRegistry};
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

/// `(vm_identity, weak lock key of the Module)`.
///
/// gc-common w22-a: the key used to be the identity hash alone, so VM B's
/// `Module.getPackages()` on a Module whose hash collided with one VM A had
/// recorded answered A's package set (identity hashes are small integers and
/// collide across VMs readily), and A's rows outlived A. Per VM now, and
/// dropped at VM teardown ([`forget_vm_jboss_module_state`]).
///
/// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 19): the second half was the Module's identity hash, which two LIVE
/// Modules of one VM can share (every heap mints it from one 32-bit counter
/// that wraps): B's `getPackages()` answered A's package set, and B's
/// `defineModule0` replaced A's row. It is the Module's weak lock key
/// (`crate::gc_stable_weak_lock_key`) now -- minted by the record paths,
/// looked up and never minted by `getPackages()` -- which names one live
/// object and follows it across moves. A dead Module's row goes when the
/// lock-key sweep frees its key ([`forget_module_packages_keys`]), so the
/// 4,096-row bound now only guards against that many LIVE recorded Modules.
type ModulePackagesKey = (usize, usize);

fn module_packages_table(
) -> &'static std::sync::Mutex<std::collections::HashMap<ModulePackagesKey, Vec<String>>> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<ModulePackagesKey, Vec<String>>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The minting form, for the record paths. `module` must be CURRENT; call it
/// before taking the table guard.
fn module_packages_key(ctx: &dyn NativeContext, module: ObjectRef) -> Option<ModulePackagesKey> {
    let key = crate::gc_stable_weak_lock_key(ctx, module).ok()?;
    Some((ctx.vm_identity(), key))
}

/// [`module_packages_key`] without minting: `None` for a Module nothing was
/// recorded for (the `getPackages()` read path).
fn existing_module_packages_key(
    ctx: &dyn NativeContext,
    module: ObjectRef,
) -> Option<ModulePackagesKey> {
    let key = crate::existing_weak_lock_key(ctx, module)?;
    Some((ctx.vm_identity(), key))
}

/// gc-common w29-e: drop the package rows filed under a weak lock key the
/// registry has just FREED -- the Module died (`lib.rs::sweep_lock_keys`) or
/// its VM went (`lib.rs::forget_vm_lock_keys`). `keys` holds every weak key
/// the sweep freed, of every kind. One lock, never under the lock-key
/// registry's; nothing here reaches Java. Answers the rows dropped.
pub(crate) fn forget_module_packages_keys(keys: &[usize]) -> usize {
    if keys.is_empty() {
        return 0;
    }
    let mut t = module_packages_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if t.is_empty() {
        return 0;
    }
    let before = t.len();
    // addr-keyed: not an address -- freed lock-key registry keys, local.
    let set: std::collections::HashSet<usize> = keys.iter().copied().collect();
    t.retain(|&(_, key), _| !set.contains(&key));
    before - t.len()
}

/// Record `packages` for the Module keyed `key` (bounded; see below).
fn module_packages_record(key: ModulePackagesKey, packages: Vec<String>) {
    let mut t = module_packages_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    module_packages_evict_if_needed(&mut t, key);
    t.insert(key, packages);
}

/// Bound the table before inserting a fresh entry, mirroring
/// `mac_state_evict_if_needed` — a long-running app that repeatedly builds
/// short-lived synthetic Modules (via `ModuleLayer.findModule`) must not
/// retain an entry per call forever. `keep` is the key about to be inserted
/// and is never evicted. Best-effort eviction (lowest keys first); these are
/// abandoned Module handles whose Java objects are unreachable.
fn module_packages_evict_if_needed(
    t: &mut std::collections::HashMap<ModulePackagesKey, Vec<String>>,
    keep: ModulePackagesKey,
) {
    if t.len() < MODULE_PACKAGES_MAX_ENTRIES {
        return;
    }
    let target = MODULE_PACKAGES_MAX_ENTRIES / 2;
    let mut ids: Vec<ModulePackagesKey> = t.keys().copied().filter(|&k| k != keep).collect();
    ids.sort_unstable();
    let to_remove = t.len().saturating_sub(target);
    for id in ids.into_iter().take(to_remove) {
        t.remove(&id);
    }
}

/// VM teardown (`lib.rs::forget_vm_native_root_stores`): drop `vm`'s boot
/// `ModuleLayer` memo row and its recorded Module package sets, releasing
/// nothing (the VM's global-ref table dies with it). Idempotent.
///
/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 5): the boot-layer row was keyed by VM but never dropped, unlike its
/// twin `phases_late::p60_current_handle_memo`.
pub fn forget_vm_jboss_module_state(vm: usize) {
    boot_layer_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&vm);
    modload_state().lock().remove(&vm);
    module_packages_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|&(owner, _), _| owner != vm);
}

/// Per-VM memo of THE boot `ModuleLayer` object, as a JNI-global-root handle
/// (`NativeContext::add_global_root`) — never a raw `ObjectRef`, because this
/// table outlives any number of collections and a global ref is the one root
/// form the moving GC both keeps alive and remaps.
///
/// Keyed by `ctx.vm_identity()`: Rust tests build several `Vm`s in one process,
/// and a process-global object cache goes stale across VM lifetimes.
// addr-keyed: not a heap address -- VM identity -> global-root handle; dropped by forget_vm_jboss_module_state.
fn boot_layer_memo() -> &'static std::sync::Mutex<std::collections::HashMap<usize, usize>> {
    // addr-keyed: the accessor's table (see above).
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
pub fn build_boot_layer(
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
    // SBR2 wave 7 (module), `--jdk-only`: HotSpot's boot layer has ONE parent,
    // `ModuleLayer.empty()` (`boot.parents().size() == 1`); the layer built here
    // had none. The real `ModuleLayer.findModule` walks the parents.
    let mut parents = parents;
    if ctx.aligned(cratonvm_types::HotSpotAlignment::JdkModuleGraph) {
        // SBR2 wave 9: initialise `ModuleLayer` first (`EMPTY_LAYER` is its
        // `<clinit>`'s). The launcher used to reach it through the invoked
        // `ModuleLayer.boot()`, which initialised the class; it now calls this
        // builder directly, and the first `Class.getModule()` could always
        // arrive before anything touched `ModuleLayer`.
        let layer_class = ctx
            .ensure_class_initialized("java/lang/ModuleLayer")
            .ok()
            .or_else(|| ctx.class_id_by_name("java/lang/ModuleLayer"));
        if let Some(cid) = layer_class {
            if let Some(idx) = ctx.static_field_index_by_name(cid, "EMPTY_LAYER") {
                if let Value::Object(Some(empty)) = ctx.get_static_field(cid, idx) {
                    let parents_pin = ctx.pin_native_root(parents);
                    let _ = ctx.invoke(
                        "java/util/ArrayList",
                        "add",
                        "(Ljava/lang/Object;)Z",
                        &[Value::Object(Some(parents)), Value::Object(Some(empty))],
                    );
                    parents = ctx.read_native_pin(parents_pin, parents);
                    ctx.unpin_native_roots(parents_pin);
                }
            }
        }
    }
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
    //
    // gc-common w22-a: no `ctx` call under the memo's guard. This used to
    // hold it across `ctx.add_global_root` (the VM's JNI global table lock),
    // the re-entrant shape `lock_discipline_ratchet.rs` names for this memo.
    // The root is taken first and the row published in one short critical
    // section; a racing publisher that won keeps its layer and ours is
    // released.
    let existing = boot_layer_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&vm)
        .copied();
    if let Some(handle) = existing {
        if let Some(cached) = ctx.resolve_global_root(handle) {
            return Ok(cached);
        }
    }
    let handle = ctx.add_global_root(layer);
    if handle != 0 {
        let winner = {
            let mut memo = boot_layer_memo().lock().unwrap_or_else(|e| e.into_inner());
            match memo.get(&vm).copied() {
                Some(other) if Some(other) != existing => Some(other),
                _ => {
                    memo.insert(vm, handle);
                    None
                }
            }
        };
        if let Some(other) = winner {
            if let Some(cached) = ctx.resolve_global_root(other) {
                let _ = ctx.remove_global_root(handle);
                return Ok(cached);
            }
            boot_layer_memo()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(vm, handle);
        }
    }
    // SBR2 wave 7 (module): `System.bootLayer` is what the JDK's own
    // `ModuleLayer.boot()` reads (`getstatic`); nothing ever set it. Published
    // with the layer, before it is populated, exactly as the memo above is.
    ctx.set_static_field_by_name("java/lang/System", "bootLayer", Value::Object(Some(layer)));
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
    let mut built: Vec<String> = Vec::new();
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
        // See docs/internal/jdk-only/D1-R11-SERVICELOADER-DOUBLE-SOURCE-20260813.md
        // and docs/internal/jdk-only/E4-R11-CLASS-PATH-MODULE-BOOT-LAYER-FIX-20260813.md.
        if ctx.module_is_class_path_only(&name) {
            continue;
        }
        // A system module the boot configuration does not resolve (an
        // aggregator such as `java.se`, an incubator module without
        // `--add-modules`) is not in HotSpot's boot layer either
        // (interpreter round i4 wave 86, lane L5).
        if ctx.module_outside_boot_configuration(&name) {
            continue;
        }
        let layer = ctx.read_native_pin(layer_pin, layer);
        let Ok(module) = build_module(ctx, &name, layer) else {
            continue;
        };
        built.push(name.clone());
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
        // Registering against the application loader is what the real
        // `ModuleLayer.defineModules` does for boot-layer modules resolved from
        // `--module-path`: they are defined to the application loader, and its
        // catalog is what the lookup walks. That is true of `--module-path`
        // modules and NOT of the JDK's own, which the real JVM defines to the
        // platform loader (`jdk.localedata`, `jdk.zipfs`) or the boot loader
        // (`java.base`) -- so the callee asks each module which loader it
        // belongs to rather than sending every one to the system loader, which
        // is what it used to do and what left
        // `ServiceLoader.loadInstalled` empty for every service.
        // `getServicesCatalog` creates the catalog if absent, and
        // `register(Module)` reads `descriptor.provides()`, so a module that
        // declares none is a no-op rather than a special case.
        let module = ctx.read_native_pin(module_pin, module);
        register_module_in_loader_catalog(ctx, module);

        ctx.unpin_native_roots(module_pin);
    }
    ctx.unpin_native_roots(layer_pin);
    // SBR2 wave 7 (module): the JDK's own `Module.canRead` / `isExported` /
    // `isOpen` bytecode reads three fields nothing ever filled. Fill them here,
    // once every boot module has its mirror (qualified targets need them).
    fill_boot_module_tables(ctx, &built);
    Ok(())
}

// ---------------------------------------------------------------------------
// SBR2 wave 9 (lane MODLOAD): the JDK's own module-system objects for the boot
// modules, `--jdk-only`.
//
// `build_boot_layer` above is this VM's `ModuleBootstrap.boot()`: it mints the
// `Module` mirrors and the layer from the registry. It never ran the rest of
// what the JDK's boot does, so the bytecode that reads those results found
// nothing: `BuiltinClassLoader.loadModule` was never called (the loaders'
// `nameToModule` / `packageToModule` stayed empty, so
// `Class.forName(Module,String)` and a module-path class's `loadClass` failed),
// `ModuleLayer.configuration()` was a hand-made two-module object, and
// `ModuleFinder.ofSystem()` a fresh stand-in per call. `finish_boot_layer` runs
// ONCE per VM, from the launcher right after the layer is built (every Java
// class it needs is up there; `build_boot_layer` itself can be reached from the
// first `Class.getModule()`, far earlier), and builds:
//
//   * one real `ModuleReferenceImpl` per module (the registry's descriptor, a
//     `jrt:/<name>` location, the JDK's own reader supplier);
//   * the real `SystemModuleFinders$SystemModuleFinder` over the image's
//     modules, which is what `ModuleFinder.ofSystem()` answers (its `find` and
//     `findAll` are then the JDK's bytecode over real maps);
//   * the boot `Configuration`, through the JDK's own boot-layer constructor
//     `Configuration(ModuleFinder, Map<String, Set<String>>)` and the registry's
//     `reads` relation, stored in `ModuleLayer.cf`;
//   * `BuiltinClassLoader.loadModule(mref)` for the modules the JDK's
//     `ModuleLoaderMap` gives each loader (`BootLoader.loadModule` for the boot
//     ones). The classes themselves are still served from the VM's class table
//     (`classloader::lazy_define_for_builtin_loader`): the loaders never read
//     class bytes through a module reader.
//
// Best effort per step, never fatal: a step that fails leaves the previous
// behaviour for what depends on it.
// ---------------------------------------------------------------------------

const MODULE_REFERENCE_IMPL_CTOR: &str = "(Ljava/lang/module/ModuleDescriptor;Ljava/net/URI;Ljava/util/function/Supplier;Ljdk/internal/module/ModulePatcher;Ljdk/internal/module/ModuleTarget;Ljdk/internal/module/ModuleHashes;Ljdk/internal/module/ModuleHashes$HashSupplier;Ljdk/internal/module/ModuleResolution;)V";

/// Per VM: whether `finish_boot_layer` ran, and the global-root handle of the
/// `ModuleFinder` that `ModuleFinder.ofSystem()` answers (`0` for none).
fn modload_state(// addr-keyed: not a heap address -- VM identity -> (ran, global-root handle); dropped by forget_vm_jboss_module_state.
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<
    // addr-keyed: not a heap address -- VM identity -> (ran, global-root handle); dropped by forget_vm_jboss_module_state.
    std::collections::HashMap<usize, (bool, usize)>,
> {
    static T: std::sync::OnceLock<
        // addr-keyed: the accessor's table (see above).
        cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashMap<usize, (bool, usize)>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// The real system `ModuleFinder` `finish_boot_layer` built, if it ran.
pub(crate) fn system_module_finder(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let handle = modload_state()
        .lock()
        .get(&ctx.vm_identity())
        .map(|s| s.1)?;
    if handle == 0 {
        return None;
    }
    ctx.resolve_global_root(handle)
}

fn modload_dbg() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_MODLOAD")
}

fn modload_err(label: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: format!("finish_boot_layer: {label}"),
    }
    .into()
}

/// Is the boot-layer module `name` one of the BOOTED run-time image's (its
/// reference reads through the image's `SystemModuleReader`), not a
/// `--module-path` module? The JDK 25 name list
/// (`reflect_annotations::is_listed_system_module`) first; else any module
/// this VM did not resolve from the module path
/// (`module_path_resource_url` answers `None` for those). Interpreter round
/// i5 wave 102, lane L5: over a JDK 21 library the image's modules that JDK
/// 25 dropped (`jdk.internal.vm.compiler`, `jdk.internal.vm.compiler
/// .management`, ...) were taken for module-path modules, got a
/// `ModuleReferenceImpl` with a NULL `readerSupplier`, and the first
/// `findMiscResource` of their built-in loader died in
/// `ModuleReferenceImpl.open` (`NullPointerException: Cannot invoke
/// "java.util.function.Supplier.get()" because "this.readerSupplier" is
/// null`; `L5W102MrResourcePath` `first-app-as-stream`,
/// `L5W100MultiReleaseFeature` `resource=null`). Over JDK 25 every image
/// module is listed, so its answers are unchanged.
pub(crate) fn is_image_module(ctx: &dyn NativeContext, name: &str) -> bool {
    crate::reflect_annotations::is_listed_system_module(name)
        || ctx
            .module_path_resource_url(name, "module-info.class")
            .is_none()
}

/// Run the JDK-object half of the boot (see the section comment). `--jdk-only`
/// only; `CRATONVM_MODLOAD=0` turns it off. Idempotent per VM.
pub fn finish_boot_layer(ctx: &mut dyn NativeContext) -> Result<(), MethodCallFailed> {
    // `--jdk-only`, or `--compatible` with the `JdkModuleGraph` row (SBR round
    // 4, lane compat): the boot `Configuration` and `ModuleReference`s are the
    // JDK's own. The loader-definition step below stays `--jdk-only`'s
    // (`real_builtin_loaders_enabled`).
    if !ctx.aligned(cratonvm_types::HotSpotAlignment::JdkModuleGraph)
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MODLOAD")
    {
        return Ok(());
    }
    let vm = ctx.vm_identity();
    {
        let mut state = modload_state().lock();
        let slot = state.entry(vm).or_insert((false, 0));
        if slot.0 {
            return Ok(());
        }
        slot.0 = true;
    }
    let out = finish_boot_layer_body(ctx);
    if modload_dbg() {
        if let Err(e) = &out {
            eprintln!("[modload] finish_boot_layer failed: {e:?}");
        }
    }
    out
}

/// One real `ModuleReferenceImpl` for `name`. `system`: an image module (its
/// reader is the JDK's own `SystemModuleReader`); a module-path module gets no
/// supplier (`ModuleReferenceImpl.open` answers a reader for it by name).
/// Pure-bytecode lever: the module-path module `name` as the JDK's own
/// `ModuleFinder.of(<jdk.module.path>)` finds it, so its `readerSupplier` is the
/// real exploded / jar reader instead of the null the registered
/// `ModuleReferenceImpl.open` native used to paper over. `None` when the lever
/// is off, the property is unset, or the JDK's finder does not know the module.
pub(crate) fn module_path_reference_via_jdk(
    scope: &mut cratonvm_native_api::NativeHandleScope<'_>,
    name: &str,
) -> Option<cratonvm_native_api::NativeHandle> {
    // SBR3 niobuf: no longer gated on the pure-bytecode lever. A boot-layer
    // module-path module must carry the JDK's own reference (real
    // `readerSupplier`, real `location()`), or the real `ModuleReferenceImpl.open`
    // (now retired) has no supplier to call and the module's reader answers
    // `NullPointerException` (`Sbr3NioBufMod`).
    let path_prop = scope.get_system_property("jdk.module.path")?;
    let sep = if cfg!(windows) { ';' } else { ':' };
    let entries: Vec<&str> = path_prop.split(sep).filter(|e| !e.is_empty()).collect();
    if entries.is_empty() {
        return None;
    }
    let string_cid = crate::lang_class::loading_component_id(&mut **scope, "java/lang/String");
    let path_cid = crate::lang_class::loading_component_id(&mut **scope, "java/nio/file/Path");
    let paths = scope.new_ref_array(path_cid, entries.len());
    let paths_h = scope.root(paths);
    for (i, entry) in entries.iter().enumerate() {
        let text = scope.create_string_uninterned(entry);
        let text_h = scope.root(text);
        let no_more = scope.new_ref_array(string_cid, 0);
        let text = scope.get(&text_h);
        let path = match scope
            .invoke(
                "java/nio/file/Path",
                "of",
                "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
                &[Value::Object(Some(text)), Value::Object(Some(no_more))],
            )
            .ok()?
        {
            Some(Value::Object(Some(p))) => p,
            _ => return None,
        };
        let path = match scope
            .invoke_virtual(path, "toAbsolutePath", "()Ljava/nio/file/Path;", &[])
            .ok()?
        {
            Some(Value::Object(Some(p))) => p,
            _ => return None,
        };
        let paths = scope.get(&paths_h);
        scope.set_array_element(paths, i, Value::Object(Some(path)));
    }
    let paths = scope.get(&paths_h);
    let finder = match scope
        .invoke(
            "java/lang/module/ModuleFinder",
            "of",
            "([Ljava/nio/file/Path;)Ljava/lang/module/ModuleFinder;",
            &[Value::Object(Some(paths))],
        )
        .ok()??
    {
        Value::Object(Some(f)) => f,
        _ => return None,
    };
    let finder_h = scope.root(finder);
    let name_str = scope.create_string_uninterned(name);
    let finder = scope.get(&finder_h);
    let found = scope
        .invoke_virtual(
            finder,
            "find",
            "(Ljava/lang/String;)Ljava/util/Optional;",
            &[Value::Object(Some(name_str))],
        )
        .ok()??;
    let Value::Object(Some(optional)) = found else {
        return None;
    };
    match scope
        .invoke_virtual(
            optional,
            "orElse",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(None)],
        )
        .ok()??
    {
        Value::Object(Some(mref)) => Some(scope.root(mref)),
        _ => None,
    }
}

/// Which `SystemModuleFinders` anonymous class supplies an image module's
/// `ModuleReader`, its constructor descriptor, and the arguments for it (see
/// the call in [`make_module_reference`]). The JDK 25 shape is the answer
/// when the image declares neither, so a failure names the shape this VM
/// was written against.
fn system_reader_supplier(
    scope: &mut cratonvm_native_api::NativeHandleScope<'_>,
    name_h: &cratonvm_native_api::NativeHandle,
    uri_h: &cratonvm_native_api::NativeHandle,
) -> (&'static str, &'static str, Vec<Value>) {
    const JDK25: (&str, &str) = (
        "jdk/internal/module/SystemModuleFinders$1",
        "(Ljava/lang/String;)V",
    );
    const JDK21: (&str, &str) = (
        "jdk/internal/module/SystemModuleFinders$2",
        "(Ljava/lang/String;Ljava/net/URI;)V",
    );
    fn declares(
        scope: &mut cratonvm_native_api::NativeHandleScope<'_>,
        (class, desc): (&str, &str),
    ) -> bool {
        crate::lang_class::loading_component_id(&mut **scope, class);
        scope.method_exists(class, "<init>", desc)
    }
    // Loading may collect, so the handles are read only after both checks.
    let jdk21 = !declares(scope, JDK25) && declares(scope, JDK21);
    let name = Value::Object(Some(scope.get(name_h)));
    if jdk21 {
        let uri = Value::Object(Some(scope.get(uri_h)));
        return (JDK21.0, JDK21.1, vec![name, uri]);
    }
    (JDK25.0, JDK25.1, vec![name])
}

/// [`make_module_reference_unpatched`], then `--patch-module`: the JDK's own
/// `jdk.internal.module.ModulePatcher(Map<String, List<String>>)
/// .patchIfNeeded(mref)` over the module's patch paths, as `ModuleBootstrap`
/// patches every module reference of the boot configuration -- the reference
/// a built-in loader's `loadModule` then reads classes and resources through,
/// patch paths first. (The boot loader's classes also come from the VM's boot
/// class path, where `vm_init` put the patch paths in front.) A failure keeps
/// the unpatched reference. Interpreter round i4 wave 87, lane L5.
fn make_module_reference(
    scope: &mut cratonvm_native_api::NativeHandleScope<'_>,
    name: &str,
    system: bool,
) -> Result<cratonvm_native_api::NativeHandle, MethodCallFailed> {
    let mref_h = make_module_reference_unpatched(scope, name, system)?;
    let paths = scope.module_patch_paths(name);
    if paths.is_empty() {
        return Ok(mref_h);
    }
    let patched = (|| -> Result<Option<ObjectRef>, MethodCallFailed> {
        let list = new_initialized_object(&mut **scope, "java/util/ArrayList", "()V", &[], "patch paths")?;
        let list_h = scope.root(list);
        for path in &paths {
            let s = scope.create_string_uninterned(path);
            let list = scope.get(&list_h);
            collection_add(&mut **scope, list, s)?;
        }
        let map = new_initialized_object(&mut **scope, "java/util/HashMap", "()V", &[], "patch map")?;
        let map_h = scope.root(map);
        let key = scope.create_string_uninterned(name);
        let (map, list) = (scope.get(&map_h), scope.get(&list_h));
        map_put(&mut **scope, map, key, list)?;
        let map = scope.get(&map_h);
        let patcher = match scope.new_object_initialized(
            "jdk/internal/module/ModulePatcher",
            "(Ljava/util/Map;)V",
            &[Value::Object(Some(map))],
        )? {
            Some(Value::Object(Some(p))) => p,
            _ => return Ok(None),
        };
        let mref = scope.get(&mref_h);
        match scope.invoke_virtual(
            patcher,
            "patchIfNeeded",
            "(Ljava/lang/module/ModuleReference;)Ljava/lang/module/ModuleReference;",
            &[Value::Object(Some(mref))],
        )? {
            Some(Value::Object(Some(p))) => Ok(Some(p)),
            _ => Ok(None),
        }
    })();
    match patched {
        Ok(Some(p)) => Ok(scope.root(p)),
        _ => Ok(mref_h),
    }
}

fn make_module_reference_unpatched(
    scope: &mut cratonvm_native_api::NativeHandleScope<'_>,
    name: &str,
    system: bool,
) -> Result<cratonvm_native_api::NativeHandle, MethodCallFailed> {
    if !system {
        if let Some(h) = module_path_reference_via_jdk(scope, name) {
            return Ok(h);
        }
    }
    let mirror = scope
        .get_cached_module_mirror(Some(name))
        .ok_or_else(|| modload_err("no module mirror"))?;
    let desc = match scope.get_field_by_name(mirror, "descriptor") {
        Value::Object(Some(d)) => d,
        _ => build_module_descriptor(&mut **scope, name)?,
    };
    let desc_h = scope.root(desc);
    let name_str = scope.create_string_uninterned(name);
    let name_h = scope.root(name_str);
    let uri_text = if system {
        format!("jrt:/{name}")
    } else {
        format!("file:///{name}")
    };
    let uri_str = scope.create_string_uninterned(&uri_text);
    let uri = match scope.invoke(
        "java/net/URI",
        "create",
        "(Ljava/lang/String;)Ljava/net/URI;",
        &[Value::Object(Some(uri_str))],
    )? {
        Some(Value::Object(Some(u))) => u,
        _ => return Err(modload_err("URI.create")),
    };
    let uri_h = scope.root(uri);
    let supplier_h = if system {
        // The image's own reader supplier, whose anonymous-class number and
        // constructor differ by release: JDK 25 has `$1(String mn)`, JDK
        // 17/21 have `$2(String mn, URI uri)` (their `$1` is a
        // `ModuleFinder`). Ask the image which one it declares rather than
        // keying on a version.
        let (class, descriptor, args) = system_reader_supplier(scope, &name_h, &uri_h);
        match scope.new_object_initialized(class, descriptor, &args)? {
            Some(Value::Object(Some(s))) => Some(scope.root(s)),
            _ => return Err(modload_err("reader supplier")),
        }
    } else {
        None
    };
    // `java.base`'s recorded `ModuleHashes`, as the JDK's own boot reference
    // carries them: `StackTraceElement.HashedModules` reads
    // `recordedHashes().names()` of the boot layer's `java.base` reference to
    // decide which modules `computeFormat` prints without a version. Without
    // them it is empty and `java.base@25.0.4/...` is printed where HotSpot
    // prints `java.base/...` (SBR3 `refmisc`; the boot-layer builder below,
    // `build_boot_resolved_module`, already did this for its synthetic
    // reference). Best effort: a failure leaves the field null.
    let hashes_h = if system && name == "java.base" {
        match build_recorded_hashes(&mut **scope, name) {
            Ok(Some(h)) => Some(scope.root(h)),
            _ => None,
        }
    } else {
        None
    };
    let supplier = match &supplier_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => Value::Object(None),
    };
    let hashes = match &hashes_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => Value::Object(None),
    };
    let args = [
        Value::Object(Some(scope.get(&desc_h))),
        Value::Object(Some(scope.get(&uri_h))),
        supplier,
        Value::Object(None),
        Value::Object(None),
        hashes,
        Value::Object(None),
        Value::Object(None),
    ];
    match scope.new_object_initialized(
        "jdk/internal/module/ModuleReferenceImpl",
        MODULE_REFERENCE_IMPL_CTOR,
        &args,
    )? {
        Some(Value::Object(Some(m))) => Ok(scope.root(m)),
        _ => Err(modload_err("ModuleReferenceImpl")),
    }
}

/// The JDK's own `SystemModuleFinders$SystemModuleFinder(Set, Map)` over `entries`.
fn build_module_finder(
    scope: &mut cratonvm_native_api::NativeHandleScope<'_>,
    entries: &[(&str, &cratonvm_native_api::NativeHandle)],
) -> Result<cratonvm_native_api::NativeHandle, MethodCallFailed> {
    let set = new_initialized_object(&mut **scope, "java/util/HashSet", "()V", &[], "finder set")?;
    let set_h = scope.root(set);
    let map = new_initialized_object(&mut **scope, "java/util/HashMap", "()V", &[], "finder map")?;
    let map_h = scope.root(map);
    for (name, mref_h) in entries {
        let key = scope.create_string_uninterned(name);
        let key_h = scope.root(key);
        let (set, mref) = (scope.get(&set_h), scope.get(mref_h));
        collection_add(&mut **scope, set, mref)?;
        let (map, key, mref) = (scope.get(&map_h), scope.get(&key_h), scope.get(mref_h));
        map_put(&mut **scope, map, key, mref)?;
    }
    let args = [
        Value::Object(Some(scope.get(&set_h))),
        Value::Object(Some(scope.get(&map_h))),
    ];
    match scope.new_object_initialized(
        "jdk/internal/module/SystemModuleFinders$SystemModuleFinder",
        "(Ljava/util/Set;Ljava/util/Map;)V",
        &args,
    )? {
        Some(Value::Object(Some(f))) => Ok(scope.root(f)),
        _ => Err(modload_err("SystemModuleFinder")),
    }
}

/// `Map<String, Set<String>>`: module name -> the names it reads, from the
/// registry's own `reads` relation (the one `fill_boot_module_tables` fills the
/// `Module.reads` tables from, so the two cannot disagree).
fn build_reads_map(
    scope: &mut cratonvm_native_api::NativeHandleScope<'_>,
    names: &[String],
) -> Result<cratonvm_native_api::NativeHandle, MethodCallFailed> {
    let map = new_initialized_object(&mut **scope, "java/util/HashMap", "()V", &[], "reads map")?;
    let map_h = scope.root(map);
    for name in names {
        let set =
            new_initialized_object(&mut **scope, "java/util/HashSet", "()V", &[], "reads set")?;
        let set_h = scope.root(set);
        for other in names {
            if other != name && scope.reads_module(name, other) {
                let s = scope.create_string_uninterned(other);
                let set = scope.get(&set_h);
                collection_add(&mut **scope, set, s)?;
            }
        }
        let key = scope.create_string_uninterned(name);
        let (map, set) = (scope.get(&map_h), scope.get(&set_h));
        map_put(&mut **scope, map, key, set)?;
    }
    Ok(map_h)
}

fn finish_boot_layer_body(ctx: &mut dyn NativeContext) -> Result<(), MethodCallFailed> {
    let layer = build_boot_layer(ctx)?;
    // SBR2 PUREMOD: the module objects exist now; fill `Class.module` of the
    // mirrors made before them (mirrors made later get it at creation), so the
    // JDK's own `Class.getModule()` (`return module;`) answers without a native.
    let fixed = ctx.fixup_class_module_fields();
    if modload_dbg() {
        eprintln!("[modload] Class.module filled on {fixed} existing mirrors");
    }
    let dbg = modload_dbg();
    let mut names: Vec<String> = ctx
        .module_names()
        .into_iter()
        .filter(|n| {
            !ctx.module_is_class_path_only(n)
                && ctx.get_cached_module_mirror(Some(n.as_str())).is_some()
        })
        .collect();
    names.sort();
    let vm = ctx.vm_identity();
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let layer_h = scope.root(layer);

    // 1. a real ModuleReferenceImpl per module.
    let mut mrefs: Vec<(String, bool, cratonvm_native_api::NativeHandle)> = Vec::new();
    for name in &names {
        let system = is_image_module(&*scope, name);
        match make_module_reference(&mut scope, name, system) {
            Ok(h) => mrefs.push((name.clone(), system, h)),
            Err(e) => {
                if dbg {
                    eprintln!("[modload] no reference for {name}: {e:?}");
                }
            }
        }
    }
    let ref_names: Vec<String> = mrefs.iter().map(|(n, _, _)| n.clone()).collect();

    // 2. the system finder (image modules only), the answer of
    //    `ModuleFinder.ofSystem()`.
    let system_entries: Vec<(&str, &cratonvm_native_api::NativeHandle)> = mrefs
        .iter()
        .filter(|(_, system, _)| *system)
        .map(|(n, _, h)| (n.as_str(), h))
        .collect();
    match build_module_finder(&mut scope, &system_entries) {
        Ok(finder_h) => {
            let finder = scope.get(&finder_h);
            let handle = scope.add_global_root(finder);
            modload_state().lock().entry(vm).or_insert((true, 0)).1 = handle;
        }
        Err(e) => {
            if dbg {
                eprintln!("[modload] system finder failed: {e:?}");
            }
        }
    }

    // 3. the boot Configuration, through the JDK's own boot-layer constructor.
    let all_entries: Vec<(&str, &cratonvm_native_api::NativeHandle)> =
        mrefs.iter().map(|(n, _, h)| (n.as_str(), h)).collect();
    let cfg = (|| -> Result<(), MethodCallFailed> {
        let finder_h = build_module_finder(&mut scope, &all_entries)?;
        let reads_h = build_reads_map(&mut scope, &ref_names)?;
        let args = [
            Value::Object(Some(scope.get(&finder_h))),
            Value::Object(Some(scope.get(&reads_h))),
        ];
        let cfg = match scope.new_object_initialized(
            "java/lang/module/Configuration",
            "(Ljava/lang/module/ModuleFinder;Ljava/util/Map;)V",
            &args,
        )? {
            Some(Value::Object(Some(c))) => c,
            _ => return Err(modload_err("Configuration")),
        };
        let layer = scope.get(&layer_h);
        scope.set_field_by_name(layer, "cf", Value::Object(Some(cfg)));
        Ok(())
    })();
    if let Err(e) = cfg {
        if dbg {
            eprintln!("[modload] boot Configuration failed: {e:?}");
        }
    }

    // 4. define each module to the loader the JDK's ModuleLoaderMap names.
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MODLOAD_DEFINE")
        && crate::classloader::real_builtin_loaders_enabled(&*scope)
    {
        let define_boot = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MODLOAD_BOOT");
        let platform = crate::classloader::get_or_create_platform_loader(&mut *scope);
        let platform_h = platform.ok().map(|p| scope.root(p));
        let app = crate::classloader::get_or_create_app_loader(&mut *scope);
        let app_h = app.ok().map(|a| scope.root(a));
        // Interpreter round i5 wave 100, lane L3 (`L3W99NullParentResources`
        // `--jdk-only`: HotSpot 21 boot-loader `module-info.class` URLs, first
        // `jdk.management.jfr`; here 22, first `java.rmi`). Each built-in
        // loader's `nameToModule` is a `ConcurrentHashMap` iterated by bin,
        // then by INSERTION order inside a bin, and `findMiscResource` walks
        // it. Two differences from HotSpot's archived boot layer:
        //  * the set: only the modules the boot configuration resolves
        //    (`boot_configuration_module_names`; the registry keeps e.g.
        //    `jdk.internal.vm.ci` in the boot layer's mirrors, which HotSpot
        //    does not resolve on a class-path launch);
        //  * the order: `java.base` first, then DESCENDING name, the same
        //    rule `classloader::builtin_loader_module_info_urls` answers with
        //    (8 of Temurin 25.0.3's 10 shared bins, including the boot loader's
        //    bin 0), so `--jdk-only` and `--compatible` agree.
        // `CRATONVM_MODLOAD_HOTSPOT_SET=0` defines every module, sorted by
        // name, as before.
        let hotspot_set = cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_MODLOAD_HOTSPOT_SET",
        );
        let boot_configuration: std::collections::HashSet<String> = if hotspot_set {
            scope.boot_configuration_module_names().into_iter().collect()
        } else {
            std::collections::HashSet::new()
        };
        let mut define_order: Vec<usize> = (0..mrefs.len())
            .filter(|&i| boot_configuration.is_empty() || boot_configuration.contains(&mrefs[i].0))
            .collect();
        if hotspot_set {
            define_order.sort_by(|&a, &b| {
                let (na, nb) = (mrefs[a].0.as_str(), mrefs[b].0.as_str());
                (na != "java.base", std::cmp::Reverse(na))
                    .cmp(&(nb != "java.base", std::cmp::Reverse(nb)))
            });
        }
        for (name, _, mref_h) in define_order.into_iter().map(|i| &mrefs[i]) {
            let ordinal = crate::classloader::builtin_loader_ordinal_of_module(&mut *scope, name);
            let mref = Value::Object(Some(scope.get(mref_h)));
            let desc = "(Ljava/lang/module/ModuleReference;)V";
            let r = match ordinal {
                Some(0) if define_boot => scope
                    .invoke(
                        "jdk/internal/loader/BootLoader",
                        "loadModule",
                        desc,
                        &[mref],
                    )
                    .map(|_| ()),
                Some(1) => match &platform_h {
                    Some(h) => {
                        let loader = scope.get(h);
                        scope
                            .invoke_virtual(loader, "loadModule", desc, &[mref])
                            .map(|_| ())
                    }
                    None => Ok(()),
                },
                Some(2) => match &app_h {
                    Some(h) => {
                        let loader = scope.get(h);
                        scope
                            .invoke_virtual(loader, "loadModule", desc, &[mref])
                            .map(|_| ())
                    }
                    None => Ok(()),
                },
                _ => Ok(()),
            };
            if let Err(e) = r {
                if dbg {
                    eprintln!("[modload] loadModule({name}) failed: {e:?}");
                }
            }
        }
        crate::classloader::set_modules_defined_to_loaders(
            crate::classloader::loader_tables(&*scope),
            true,
        );
    }
    Ok(())
}

/// Fill `reads`, `exportedPackages` and `openPackages` of every boot-layer
/// module in `names`, from the VM's own `ModuleRegistry` -- the tables the JDK's
/// `Module.defineModules` (`initExports` / `initExportsAndOpens`) fills from a
/// resolved `Configuration`, with the same rules: no table for an open or
/// automatic module, a package open to everyone is not also exported, a
/// qualified edge keeps only targets that are modules of the layer (and, for an
/// export, not already open to that target), an empty table stays `null`.
///
/// Unlike the JDK's it does NOT call `addExports0`/`addExportsToAll0`: the
/// registry already holds every edge it is reading.
///
/// Best-effort per module: a failure leaves that module's fields as they were
/// (the state before this existed), never the layer's.
fn fill_boot_module_tables(ctx: &mut dyn NativeContext, names: &[String]) {
    for name in names {
        let _ = fill_one_module_tables(ctx, name, names);
    }
}

/// `(package, targets)` with `None` = every module (`Module.EVERYONE_SET`).
type TableEdges = Vec<(String, Option<Vec<String>>)>;

fn fill_one_module_tables(
    ctx: &mut dyn NativeContext,
    name: &str,
    layer_names: &[String],
) -> Result<(), MethodCallFailed> {
    let Some(module) = ctx.get_cached_module_mirror(Some(name)) else {
        return Ok(());
    };
    // Registry answers first: they are `&self` reads and must not interleave
    // with the Java re-entry below.
    let readable: Vec<String> = layer_names
        .iter()
        .filter(|other| other.as_str() != name && ctx.reads_module(name, other))
        .cloned()
        .collect();
    let skip_edges = ctx.module_is_open(name) || ctx.module_is_automatic(name);
    // Edges recorded after the module-info was parsed (`--add-exports`,
    // `--add-opens`, `--add-reads`, an earlier `Module.addExports`): the JDK
    // files them in `ReflectionData`, so they are replayed through the real
    // `implAddExportsOrOpens` / `implAddReads` below.
    let dyn_exports = ctx.module_dynamic_exports(name);
    let dyn_opens = ctx.module_dynamic_opens(name);
    let dyn_reads = ctx.module_dynamic_reads(name);
    let in_layer = |t: &str| t == ALL_UNNAMED_TARGET || layer_names.iter().any(|n| n.as_str() == t);
    let mut open_edges: TableEdges = Vec::new();
    let mut export_edges: TableEdges = Vec::new();
    if !skip_edges {
        for (pkg, targets) in ctx.module_opens(name) {
            if targets.is_empty() {
                open_edges.push((dotted(&pkg), None));
            } else {
                let kept: Vec<String> = targets.into_iter().filter(|t| in_layer(t)).collect();
                if !kept.is_empty() {
                    open_edges.push((dotted(&pkg), Some(kept)));
                }
            }
        }
        for (pkg, targets) in ctx.module_exports(name) {
            let pkg = dotted(&pkg);
            let open_to = open_edges
                .iter()
                .rev()
                .find(|(p, _)| *p == pkg)
                .map(|(_, t)| t);
            if matches!(open_to, Some(None)) {
                continue;
            }
            if targets.is_empty() {
                export_edges.push((pkg, None));
            } else {
                let kept: Vec<String> = targets
                    .into_iter()
                    .filter(|t| in_layer(t))
                    .filter(|t| match open_to {
                        Some(Some(open)) => !open.contains(t),
                        _ => true,
                    })
                    .collect();
                if !kept.is_empty() {
                    export_edges.push((pkg, Some(kept)));
                }
            }
        }
    }

    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let module_h = scope.root(module);

    // reads
    let reads = new_initialized_object(&mut *scope, "java/util/HashSet", "()V", &[], "reads")?;
    let reads_h = scope.root(reads);
    for other in &readable {
        let Some(m2) = scope.get_cached_module_mirror(Some(other.as_str())) else {
            continue;
        };
        let reads = scope.get(&reads_h);
        collection_add(&mut *scope, reads, m2)?;
    }
    let (module, reads) = (scope.get(&module_h), scope.get(&reads_h));
    scope.set_field_by_name(module, "reads", Value::Object(Some(reads)));

    for (field, edges) in [
        ("openPackages", &open_edges),
        ("exportedPackages", &export_edges),
    ] {
        if edges.is_empty() {
            continue;
        }
        let map = new_initialized_object(&mut *scope, "java/util/HashMap", "()V", &[], field)?;
        let map_h = scope.root(map);
        for (pkg, targets) in edges {
            let set = match targets {
                None => match module_static(&mut *scope, "EVERYONE_SET") {
                    Some(s) => s,
                    None => continue,
                },
                Some(targets) => {
                    let set = new_initialized_object(
                        &mut *scope,
                        "java/util/HashSet",
                        "()V",
                        &[],
                        field,
                    )?;
                    let set_h = scope.root(set);
                    for t in targets {
                        let m2 = if t.as_str() == ALL_UNNAMED_TARGET {
                            module_static(&mut *scope, "ALL_UNNAMED_MODULE")
                        } else {
                            scope.get_cached_module_mirror(Some(t.as_str()))
                        };
                        let Some(m2) = m2 else { continue };
                        let set = scope.get(&set_h);
                        collection_add(&mut *scope, set, m2)?;
                    }
                    scope.get(&set_h)
                }
            };
            let set_h = scope.root(set);
            let key = scope.create_string_uninterned(pkg);
            let key_h = scope.root(key);
            let (map, key, set) = (scope.get(&map_h), scope.get(&key_h), scope.get(&set_h));
            map_put(&mut *scope, map, key, set)?;
        }
        let (module, map) = (scope.get(&module_h), scope.get(&map_h));
        scope.set_field_by_name(module, field, Value::Object(Some(map)));
    }

    // Dynamic edges. `syncVM` false: the registry already holds them.
    for (edges, open) in [(&dyn_exports, false), (&dyn_opens, true)] {
        for (pkg, to) in edges {
            let target = match to {
                None => module_static(&mut *scope, "EVERYONE_MODULE"),
                Some(t) if t.is_empty() => module_static(&mut *scope, "ALL_UNNAMED_MODULE"),
                Some(t) => scope.get_cached_module_mirror(Some(t.as_str())),
            };
            let Some(target) = target else { continue };
            let target_h = scope.root(target);
            let pn = scope.create_string_uninterned(&dotted(pkg));
            let pn_h = scope.root(pn);
            let (module, pn, target) =
                (scope.get(&module_h), scope.get(&pn_h), scope.get(&target_h));
            let _ = scope.invoke(
                "java/lang/Module",
                "implAddExportsOrOpens",
                "(Ljava/lang/String;Ljava/lang/Module;ZZ)V",
                &[
                    Value::Object(Some(module)),
                    Value::Object(Some(pn)),
                    Value::Object(Some(target)),
                    Value::Int(i32::from(open)),
                    Value::Int(0),
                ],
            );
        }
    }
    for provider in &dyn_reads {
        let other = if provider.is_empty() {
            module_static(&mut *scope, "ALL_UNNAMED_MODULE")
        } else {
            scope.get_cached_module_mirror(Some(provider.as_str()))
        };
        let Some(other) = other else { continue };
        let other_h = scope.root(other);
        let (module, other) = (scope.get(&module_h), scope.get(&other_h));
        let _ = scope.invoke(
            "java/lang/Module",
            "implAddReads",
            "(Ljava/lang/Module;)V",
            &[Value::Object(Some(module)), Value::Object(Some(other))],
        );
    }
    Ok(())
}

/// `--jdk-only` (and, since JIT round 20, `--compatible` with the
/// `JdkModuleAccess` row, which retires the same `Module` natives): the
/// JDK-visible edges of a dynamic proxy module
/// (`jdk.proxyN`), through the JDK's own `Module` methods, so the retired
/// `canRead` / `isExported` / `isOpen` bodies answer as HotSpot's do.
///
/// `Proxy` (`ProxyBuilder.getDynamicModule`) gives the module: an unqualified
/// export of `exported_pkg`, an opening of it and of `non_exported_pkg` to
/// `java.base` only, and a read edge to `java.base` plus each interface's
/// module. The mirror is minted by the `Class.getModule()` builder, not by
/// `defineModules`, so nothing else writes these. `syncVM` is false for the
/// exports/opens (the builder wrote the registry); `implAddReads` syncs as the
/// JDK's does.
pub(crate) fn fill_proxy_module_tables(
    ctx: &mut dyn NativeContext,
    module: ObjectRef,
    exported_pkg: &str,
    non_exported_pkg: &str,
    proxy_mirror: ObjectRef,
) {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let module_h = scope.root(module);
    let mirror_h = scope.root(proxy_mirror);
    let base = scope.get_cached_module_mirror(Some("java.base"));
    let base_h = base.map(|b| scope.root(b));
    let everyone = module_static(&mut *scope, "EVERYONE_MODULE");
    let everyone_h = everyone.map(|e| scope.root(e));

    // exports: `exported_pkg` to everyone
    if let Some(eh) = &everyone_h {
        let pn = scope.create_string_uninterned(exported_pkg);
        let pn_h = scope.root(pn);
        let (m, pn, e) = (scope.get(&module_h), scope.get(&pn_h), scope.get(eh));
        let _ = scope.invoke(
            "java/lang/Module",
            "implAddExportsOrOpens",
            "(Ljava/lang/String;Ljava/lang/Module;ZZ)V",
            &[
                Value::Object(Some(m)),
                Value::Object(Some(pn)),
                Value::Object(Some(e)),
                Value::Int(0),
                Value::Int(0),
            ],
        );
    }
    // opens: both packages to java.base
    if let Some(bh) = &base_h {
        for pkg in [exported_pkg, non_exported_pkg] {
            let pn = scope.create_string_uninterned(pkg);
            let pn_h = scope.root(pn);
            let (m, pn, b) = (scope.get(&module_h), scope.get(&pn_h), scope.get(bh));
            let _ = scope.invoke(
                "java/lang/Module",
                "implAddExportsOrOpens",
                "(Ljava/lang/String;Ljava/lang/Module;ZZ)V",
                &[
                    Value::Object(Some(m)),
                    Value::Object(Some(pn)),
                    Value::Object(Some(b)),
                    Value::Int(1),
                    Value::Int(0),
                ],
            );
        }
        // reads: java.base
        let (m, b) = (scope.get(&module_h), scope.get(bh));
        let _ = scope.invoke(
            "java/lang/Module",
            "implAddReads",
            "(Ljava/lang/Module;)V",
            &[Value::Object(Some(m)), Value::Object(Some(b))],
        );
    }
    // reads: the module of every interface
    let mirror = scope.get(&mirror_h);
    let Ok(Some(Value::Object(Some(itfs)))) =
        scope.invoke_virtual(mirror, "getInterfaces", "()[Ljava/lang/Class;", &[])
    else {
        return;
    };
    let itfs_h = scope.root(itfs);
    let n = {
        let itfs = scope.get(&itfs_h);
        scope.array_length(itfs)
    };
    for i in 0..n {
        let itfs = scope.get(&itfs_h);
        let Value::Object(Some(itf)) = scope.get_array_element(itfs, i) else {
            continue;
        };
        let Ok(Some(Value::Object(Some(im)))) =
            scope.invoke_virtual(itf, "getModule", "()Ljava/lang/Module;", &[])
        else {
            continue;
        };
        let im_h = scope.root(im);
        let (m, im) = (scope.get(&module_h), scope.get(&im_h));
        if m == im {
            continue;
        }
        let _ = scope.invoke(
            "java/lang/Module",
            "implAddReads",
            "(Ljava/lang/Module;)V",
            &[Value::Object(Some(m)), Value::Object(Some(im))],
        );
    }
}

/// Add `module` to the `ServicesCatalog` of the loader the module ACTUALLY
/// belongs to.
///
/// This used to register every module against
/// `ClassLoader.getSystemClassLoader()` unconditionally. That is right for the
/// case it was written for -- boot-layer modules resolved from
/// `--module-path`, which a real `ModuleLayer.defineModules` does define to the
/// application loader -- and wrong for every JDK module, which the real JVM
/// defines to the PLATFORM loader (`jdk.localedata`, `jdk.zipfs`,
/// `jdk.charsets`) or to the BOOT loader (`java.base`).
///
/// The consequence was that `ServiceLoader.loadInstalled(S)` -- which is
/// `load(S, ClassLoader.getPlatformClassLoader())` -- returned **nothing, for
/// every service**, because nothing had ever been registered in the platform
/// loader's catalog. `ServiceLoader.load(S)` kept working because the
/// thread-context loader is the application loader, i.e. exactly the catalog
/// everything had been dumped into. Measured before this change:
///
/// ```text
///                                     load()   loadInstalled()
///   HotSpot 21  LocaleDataMetaInfo       2           2
///   HotSpot 21  FileSystemProvider       2           2
///   CratonVM 21 LocaleDataMetaInfo       2           0
///   CratonVM 21 FileSystemProvider       2           0
/// ```
///
/// Nothing throws on that path: an empty `ServiceLoader` iteration is
/// indistinguishable from a service that genuinely has no providers, so every
/// caller silently takes its no-provider arm. The visible symptom was
/// `CLDRLocaleProviderAdapter`, whose static initialiser uses `loadInstalled`
/// to find the supplementary `LocaleDataMetaInfo`: it got null, kept only
/// `java.base`'s 5 base language tags instead of 1063, truthfully reported that
/// it does not support `de-DE`, and `LocaleProviderAdapter.getAdapter` fell
/// through to `FallbackLocaleProviderAdapter` -- whose root/English data IS the
/// US separators every non-English locale was answering with.
/// See docs/internal/retired/serviceloader-loadinstalled-finds-nothing-so-every-platform-loader-service-is-empty-FIXED-20260909.md
///
/// The boot loader is NOT `getServicesCatalog(null)`. `ServiceLoader` reads
/// boot-module providers from `BootLoader.getServicesCatalog()` specifically
/// (`ModuleServicesLookupIterator` branches on `loader == null` before it ever
/// consults `ServicesCatalog`), so a plain `Module.getClassLoader()` swap would
/// have silently dropped `java.base`'s own providers instead of moving them.
/// That branch is why this is not a one-line change.
///
/// Best-effort by design, unchanged: every step is a real-JDK call that a
/// synthetic-JDK build may not have, and a missing services catalog must not
/// take the boot layer down with it. A caller that gets no catalog is exactly
/// where it was before this existed.
fn register_module_in_loader_catalog(ctx: &mut dyn NativeContext, module: ObjectRef) {
    let module_pin = ctx.pin_native_root(module);

    // The module's OWN loader. `Module.getClassLoader()` answers null for a
    // boot-loader module, which is a real answer here and not a failure.
    let loader =
        match ctx.invoke_virtual(module, "getClassLoader", "()Ljava/lang/ClassLoader;", &[]) {
            Ok(Some(Value::Object(l))) => l,
            // Could not ask. Fall back to the previous behaviour rather than
            // skipping the module: the app-loader catalog is where `--module-path`
            // modules belong, and half a catalog beats none.
            _ => match ctx.invoke(
                "java/lang/ClassLoader",
                "getSystemClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[],
            ) {
                Ok(Some(Value::Object(Some(l)))) => Some(l),
                _ => {
                    ctx.unpin_native_roots(module_pin);
                    return;
                }
            },
        };

    let loader_pin = loader.map(|l| (ctx.pin_native_root(l), l));
    let loader = loader_pin.map(|(pin, l)| ctx.read_native_pin(pin, l));

    let catalog = match loader {
        // A named loader (platform or app): its own catalog.
        Some(l) => ctx.invoke(
            "jdk/internal/module/ServicesCatalog",
            "getServicesCatalog",
            "(Ljava/lang/ClassLoader;)Ljdk/internal/module/ServicesCatalog;",
            &[Value::Object(Some(l))],
        ),
        // Boot loader. `ServiceLoader` looks here and nowhere else for
        // boot-module providers.
        None => ctx.invoke(
            "jdk/internal/loader/BootLoader",
            "getServicesCatalog",
            "()Ljdk/internal/module/ServicesCatalog;",
            &[],
        ),
    };
    // If the module's own catalog could not be obtained, fall back to the
    // system loader's rather than registering the module NOWHERE. Before this
    // change every module went into the system catalog, so returning here would
    // make a `BootLoader.getServicesCatalog` that this build cannot call a
    // REGRESSION -- `java.base`'s providers would stop being visible to
    // `ServiceLoader.load` as well, which is strictly worse than the defect
    // being fixed. Best-effort means degrading to the old behaviour, not to
    // nothing.
    let catalog = match catalog {
        Ok(Some(Value::Object(Some(c)))) => Some(c),
        _ => match ctx.invoke(
            "java/lang/ClassLoader",
            "getSystemClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(sys)))) => {
                let sys_pin = ctx.pin_native_root(sys);
                let sys = ctx.read_native_pin(sys_pin, sys);
                let c = match ctx.invoke(
                    "jdk/internal/module/ServicesCatalog",
                    "getServicesCatalog",
                    "(Ljava/lang/ClassLoader;)Ljdk/internal/module/ServicesCatalog;",
                    &[Value::Object(Some(sys))],
                ) {
                    Ok(Some(Value::Object(Some(c)))) => Some(c),
                    _ => None,
                };
                ctx.unpin_native_roots(sys_pin);
                c
            }
            _ => None,
        },
    };
    let catalog = match catalog {
        Some(c) => c,
        None => {
            if let Some((pin, _)) = loader_pin {
                ctx.unpin_native_roots(pin);
            }
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
    if let Some((pin, _)) = loader_pin {
        ctx.unpin_native_roots(pin);
    }
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
    // Keyed while `module` is current (nothing below allocates, but the key
    // does not depend on that).
    let Some(key) = module_packages_key(&*ctx, module) else {
        return;
    };
    let packages = module_package_names(ctx, name);
    module_packages_record(key, packages);
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
    // SBR wave 6 (module): seed the real `loader` and `enableNativeAccess`
    // fields exactly as the `Class.getModule()` builder does (`lib.rs`), so a
    // Module reached through the LAYER (`ModuleLayer.findModule`, `modules()`)
    // answers the same as one reached through a class. `Module.getClassLoader()`
    // is `getfield loader` in the JDK's own body: a platform module (`java.sql`)
    // minted here without it answered `null` where HotSpot answers the platform
    // loader (measured, `SbrW6Module` row `module.sql.loader`).
    if let Some(platform) = crate::classloader::platform_loader_for_module(ctx, name) {
        let module = ctx.read_native_pin(pin, module);
        ctx.set_field_by_name(module, "loader", Value::Object(Some(platform)));
    }
    if crate::lang_class::module_starts_with_native_access(name) {
        let module = ctx.read_native_pin(pin, module);
        ctx.set_field_by_name(module, "enableNativeAccess", Value::Int(1));
    }
    let module = ctx.read_native_pin(pin, module);
    // `unpin_native_roots` of the OLDER pin truncates every newer one, so `pin` (the
    // module) is read for the last time on the line above and the layer pin goes now.
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
    // Keyed while `module` (from `args`) is current, before anything below.
    let key = module_packages_key(&*ctx, module);
    // The descriptor's version (`null` for none), read while `args` is
    // current: a layer module keeps it for its stack trace elements.
    let version = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };

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

    // Both modes (`--jdk-only` since wave 43, `--compatible` since wave 48,
    // lane L6): HotSpot's `defineModule0` files each package in the
    // defining loader's package-to-module map, so a class that loader defines
    // in the package is a member of THIS module. Record it for a user-defined
    // loader (the loader of a non-boot `ModuleLayer`: `jdk.internal.loader.
    // Loader`, or the one a `defineModules` function names); `Class.getModule()`
    // and the VM's access control answer from it, by the module's identity
    // (its openness included). The built-in loaders' modules keep the name
    // registry. (The name is read first: recording may allocate.)
    let open_module_name = if is_open {
        match ctx.get_field_by_name(module, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        }
    } else {
        None
    };
    let layer_module = if !packages.is_empty() {
        record_layer_module(ctx, module, &packages, is_open, version.as_deref())
    } else {
        false
    };

    // An open module opens every package it contains to every other module —
    // empty target = unqualified, same convention as `addExportsToAll0`. By
    // name, for a module of the name registry; a layer module's openness is
    // recorded by identity above (a name-keyed edge would open a same-named
    // module of every other layer as well).
    if let (Some(name), false) = (open_module_name, layer_module) {
        for pkg in &packages {
            ctx.module_add_opens(&name, pkg, "");
        }
    }

    if let Some(key) = key {
        module_packages_record(key, packages);
    }
    Ok(None)
}

/// `CRATONVM_DBG=access` (`CRATONVM_DBG_ACCESS`): trace the layer-module
/// records and the `Class.getModule()` answers taken from them
/// (`[LAYER-MODULE]` lines). Read per use: both sites are cold (a module
/// definition; a `getModule()` a record answered).
pub(crate) fn dbg_layer_modules() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_ACCESS")
}

/// The boot `ModuleLayer` memo of this VM, without building it.
fn boot_layer_peek(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let handle = boot_layer_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&ctx.vm_identity())
        .copied()?;
    ctx.resolve_global_root(handle)
}

/// Whether `module` is a named `java.lang.Module` of a non-boot
/// `ModuleLayer` (it has a name, a descriptor and a layer that is not the boot
/// layer) in a VM where a layer module was recorded
/// (`record_layer_module_packages`, both modes since wave 48). Such a Module was
/// built by the JDK's own `Module.defineModules`, whose `reads`,
/// `exportedPackages` and `openPackages` fields -- with `ReflectionData` for
/// the run-time edges -- are the whole answer to `canRead` / `isExported` /
/// `isOpen`; the name-keyed `ModuleRegistry` the natives consult knows no
/// such module (an unknown reader "reads everything") and cannot tell two
/// layers' same-named modules apart. The natives hand these receivers to the
/// JDK's bodies. One atomic load for every other receiver in every mode.
pub(crate) fn is_user_layer_module(ctx: &mut dyn NativeContext, module: ObjectRef) -> bool {
    if !ctx.has_layer_module_packages() {
        return false;
    }
    if !matches!(
        ctx.get_field_by_name(module, "name"),
        Value::Object(Some(_))
    ) || !matches!(
        ctx.get_field_by_name(module, "descriptor"),
        Value::Object(Some(_))
    ) {
        return false;
    }
    let Value::Object(Some(layer)) = ctx.get_field_by_name(module, "layer") else {
        return false;
    };
    !matches!(boot_layer_peek(&*ctx), Some(boot) if boot == layer)
}

/// A `java.lang.Module` static (`EVERYONE_MODULE`, `ALL_UNNAMED_MODULE`).
pub(crate) fn module_static(ctx: &mut dyn NativeContext, field: &str) -> Option<ObjectRef> {
    let cid = ctx.class_id_by_name("java/lang/Module")?;
    let index = ctx.static_field_index_by_name(cid, field)?;
    match ctx.get_static_field(cid, index) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// `isExported` / `isOpen` of an [`is_user_layer_module`] receiver: the
/// JDK's `implIsExportedOrOpen(pn, other, open)`, `other` `None` meaning
/// `EVERYONE_MODULE` (the unqualified forms). `None` when the static is
/// missing (the caller keeps its registry answer).
pub(crate) fn user_layer_module_is_exported_or_open(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    pn: Value,
    other: Option<ObjectRef>,
    open: bool,
) -> Option<MethodCallResult> {
    let other = match other {
        Some(o) => o,
        None => module_static(ctx, "EVERYONE_MODULE")?,
    };
    Some(ctx.invoke(
        "java/lang/Module",
        "implIsExportedOrOpen",
        "(Ljava/lang/String;Ljava/lang/Module;Z)Z",
        &[
            Value::Object(Some(this)),
            pn,
            Value::Object(Some(other)),
            Value::Int(i32::from(open)),
        ],
    ))
}

/// Which module an `implAdd{Exports,Opens}*` native names as its target.
pub(crate) enum LayerEdgeTarget {
    Everyone,
    AllUnnamed,
    Module(ObjectRef),
}

/// `implAddExports*` / `implAddOpens*` of an [`is_user_layer_module`]
/// receiver: the JDK's `implAddExportsOrOpens(pn, target, open, true)`, which
/// records the edge in `ReflectionData` (what `implIsExportedOrOpen` reads)
/// and syncs the VM through `addExports0` / `addExportsToAll0` /
/// `addExportsToAllUnnamed0`, whose natives write the registry as before.
pub(crate) fn user_layer_module_add_edge(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    pn: Value,
    target: LayerEdgeTarget,
    open: bool,
) -> Option<MethodCallResult> {
    let target = match target {
        LayerEdgeTarget::Everyone => module_static(ctx, "EVERYONE_MODULE")?,
        LayerEdgeTarget::AllUnnamed => module_static(ctx, "ALL_UNNAMED_MODULE")?,
        LayerEdgeTarget::Module(m) => m,
    };
    Some(
        ctx.invoke(
            "java/lang/Module",
            "implAddExportsOrOpens",
            "(Ljava/lang/String;Ljava/lang/Module;ZZ)V",
            &[
                Value::Object(Some(this)),
                pn,
                Value::Object(Some(target)),
                Value::Int(i32::from(open)),
                Value::Int(1),
            ],
        )
        .map(|_| None),
    )
}

/// `canRead` of an [`is_user_layer_module`] receiver: the JDK's `canRead`
/// body (the receiver is named), read through its fields: `other == this`,
/// `this.reads` holds a named `other`, or `ReflectionData.reads` has the pair
/// (or, for an unnamed `other`, the pair with `ALL_UNNAMED_MODULE`).
pub(crate) fn user_layer_module_can_read(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    other: ObjectRef,
) -> MethodCallResult {
    fn truthy(r: Option<Value>) -> bool {
        matches!(r, Some(Value::Int(v)) if v != 0)
    }
    if this == other {
        return Ok(Some(Value::Int(1)));
    }
    let other_named = matches!(ctx.get_field_by_name(other, "name"), Value::Object(Some(_)));
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let other_h = scope.root(other);
    if other_named {
        if let Value::Object(Some(reads)) = scope.get_field_by_name(this, "reads") {
            let reads_h = scope.root(reads);
            let found = scope.invoke_virtual(
                reads,
                "contains",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(other))],
            )?;
            if truthy(found) {
                return Ok(Some(Value::Int(1)));
            }
            // A boot-layer module has more than one CratonVM mirror (the
            // boot layer's `nameToModule` and `Class.getModule()`'s cache are
            // filled by different builders), so the `reads` the JDK computed
            // from the boot layer may hold another object for the module
            // `other` names. Match a boot module by name; a layer module's
            // identity is unique and stays an identity test.
            let other_now = scope.get(&other_h);
            if !is_user_layer_module(&mut *scope, other_now) {
                let other_name = match scope.get_field_by_name(other_now, "name") {
                    Value::Object(Some(s)) => scope.read_string(s),
                    _ => None,
                };
                let reads = scope.get(&reads_h);
                if let (Some(other_name), Some(Value::Object(Some(arr)))) = (
                    other_name,
                    scope.invoke_virtual(reads, "toArray", "()[Ljava/lang/Object;", &[])?,
                ) {
                    for i in 0..scope.array_length(arr) {
                        let Value::Object(Some(m)) = scope.get_array_element(arr, i) else {
                            continue;
                        };
                        if let Value::Object(Some(s)) = scope.get_field_by_name(m, "name") {
                            if scope.read_string(s).as_deref() == Some(other_name.as_str()) {
                                return Ok(Some(Value::Int(1)));
                            }
                        }
                    }
                }
            }
        }
    }
    let rd = scope.ensure_class_initialized("java/lang/Module$ReflectionData")?;
    let Some(index) = scope.static_field_index_by_name(rd, "reads") else {
        return Ok(Some(Value::Int(0)));
    };
    let Value::Object(Some(map)) = scope.get_static_field(rd, index) else {
        return Ok(Some(Value::Int(0)));
    };
    let map_h = scope.root(map);
    let this_now = scope.get(&this_h);
    let other_now = scope.get(&other_h);
    let found = scope.invoke_virtual(
        map,
        "containsKeyPair",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        &[
            Value::Object(Some(this_now)),
            Value::Object(Some(other_now)),
        ],
    )?;
    if truthy(found) {
        return Ok(Some(Value::Int(1)));
    }
    if !other_named {
        if let Some(all_unnamed) = module_static(&mut *scope, "ALL_UNNAMED_MODULE") {
            let map = scope.get(&map_h);
            let this_now = scope.get(&this_h);
            let found = scope.invoke_virtual(
                map,
                "containsKeyPair",
                "(Ljava/lang/Object;Ljava/lang/Object;)Z",
                &[
                    Value::Object(Some(this_now)),
                    Value::Object(Some(all_unnamed)),
                ],
            )?;
            if truthy(found) {
                return Ok(Some(Value::Int(1)));
            }
        }
    }
    Ok(Some(Value::Int(0)))
}

/// The namespace under which the dynamic modules of the built-in loaders are
/// recorded for `Class.getModule()` (the built-in loaders share the global
/// store, namespace 0; interpreter round i4 wave 85, lane L4).
const BUILT_IN_LOADER_NS: u32 = 0;

/// `Class.getModule()`'s answer for a class `loader` defined outside every
/// named module the registry knows: the Module of a non-boot layer that holds
/// the class's package for `loader` (`record_layer_module_packages`), or
/// `None`. Allocates nothing.
pub(crate) fn layer_module_of_class(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    mirror: ObjectRef,
) -> Option<ObjectRef> {
    // A built-in loader has no namespace id of its own; its dynamic modules
    // are recorded under [`BUILT_IN_LOADER_NS`] (wave 85, see
    // `record_layer_module`).
    let loader_ns = match crate::classloader::peek_loader_namespace_id(ctx, loader) {
        Some(ns) => ns,
        None if !crate::classloader::is_user_defined_loader(ctx, loader) => BUILT_IN_LOADER_NS,
        None => return None,
    };
    let class_id = ctx.class_id_from_mirror(mirror)?;
    let name = ctx.class_name_of_id(class_id)?;
    // An array class is in its element class's module (`[Lp/C;` → `p/C`); a
    // primitive array is java.base's and never reaches here with a user
    // loader.
    let element = match name.trim_start_matches('[') {
        e if e.len() == name.len() => e,
        e => e.strip_prefix('L')?.strip_suffix(';')?,
    };
    // A hidden class's name may carry its `/0x..` (or `+0x..`) suffix; the
    // package is that of the name before it.
    let element = match element.rsplit_once(|c: char| c == '/' || c == '+') {
        Some((head, tail)) if tail.starts_with("0x") => head,
        _ => element,
    };
    let slash = element.rfind('/')?;
    let module = ctx.layer_module_of_package(loader_ns, &element[..slash])?;
    if dbg_layer_modules() {
        eprintln!("[LAYER-MODULE] getModule {name} loader_ns={loader_ns} -> layer module");
    }
    Some(module)
}

/// See `native_module_define_module0`. `module` is current (nothing between
/// the native's entry and this call allocates). `true` when the module was
/// recorded as a user-defined loader's layer module.
fn record_layer_module(
    ctx: &mut dyn NativeContext,
    module: ObjectRef,
    packages: &[String],
    is_open: bool,
    version: Option<&str>,
) -> bool {
    let Value::Object(Some(loader)) = ctx.get_field_by_name(module, "loader") else {
        return false;
    };
    if !crate::classloader::is_user_defined_loader(ctx, loader) {
        // Interpreter round i4 wave 85, lane L4: a DYNAMIC module (no layer)
        // that `jdk.internal.module.Modules.defineModule` defines to a
        // built-in loader -- `MethodHandleProxies`' `jdk.MHProxyN`,
        // `java.lang.reflect.Proxy`'s `jdk.proxyN` -- holds the classes that
        // loader defines in its packages, as HotSpot's `defineModule0` files
        // them. Only `Class.getModule()` answers from this record
        // ([`layer_module_of_class`], namespace [`BUILT_IN_LOADER_NS`]); the
        // VM's access control keeps its answers. A boot-layer module of a
        // built-in loader (it has a layer) keeps the name registry.
        let in_layer = matches!(
            ctx.get_field_by_name(module, "layer"),
            Value::Object(Some(_))
        );
        if !in_layer && !packages.is_empty() {
            let internal: Vec<String> = packages.iter().map(|p| p.replace('.', "/")).collect();
            if dbg_layer_modules() {
                eprintln!("[LAYER-MODULE] defineModule0 dynamic built-in packages={internal:?}");
            }
            ctx.record_layer_module_packages(BUILT_IN_LOADER_NS, &internal, module);
        }
        return false;
    }
    // Only a module of a `ModuleLayer` enters the VM's identity-keyed view: a
    // dynamic module `Modules.defineModule` makes with no layer (a proxy's
    // `jdk.proxyN`) keeps the access answers it had.
    let in_layer = matches!(
        ctx.get_field_by_name(module, "layer"),
        Value::Object(Some(_))
    );
    let module_pin = ctx.pin_native_root(module);
    // The id the loader's defines use (allocated now when it has defined
    // nothing yet, as its first define would).
    let loader_ns = crate::classloader::loader_namespace_id(ctx, loader);
    let module = ctx.read_native_pin(module_pin, module);
    ctx.unpin_native_roots(module_pin);
    if loader_ns < 3 {
        return false;
    }
    let internal: Vec<String> = packages.iter().map(|p| p.replace('.', "/")).collect();
    let name = match ctx.get_field_by_name(module, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if dbg_layer_modules() {
        eprintln!(
            "[LAYER-MODULE] defineModule0 {name} loader_ns={loader_ns} packages={internal:?} open={is_open}"
        );
    }
    // The VM's own view, by identity: access control judges a class the
    // loader defines in these packages as a member of this module, with the
    // exports `addExports0` & co. record for it (interpreter round i1 wave
    // 44, lane L5).
    let identity_keyed = in_layer && !name.is_empty();
    if identity_keyed {
        ctx.layer_module_define(loader_ns, &name, is_open, &internal);
        if let Some(version) = version {
            ctx.layer_module_set_version(loader_ns, &name, version);
        }
    }
    ctx.record_layer_module_packages(loader_ns, &internal, module);
    identity_keyed
}

/// The identity `(loader namespace, name)` of `module` when it
/// is a named module of a non-boot layer defined to a user-defined loader
/// ([`is_user_layer_module`] and a loader with a namespace id), else `None`.
/// Allocates nothing.
pub(crate) fn user_layer_module_identity(
    ctx: &mut dyn NativeContext,
    module: ObjectRef,
) -> Option<(u32, String)> {
    if !is_user_layer_module(ctx, module) {
        return None;
    }
    let Value::Object(Some(loader)) = ctx.get_field_by_name(module, "loader") else {
        return None;
    };
    let loader_ns = crate::classloader::peek_loader_namespace_id(ctx, loader)?;
    let name = match ctx.get_field_by_name(module, "name") {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    Some((loader_ns, name))
}

/// The VM half of an `addExports*0` native for a layer module `from` (see
/// [`user_layer_module_identity`]): records the export by identity and answers
/// `true`, or answers `false` and the caller records it by name as before.
/// `to` is `addExports0`'s target module, `None` for the two unqualified
/// natives, whose `unnamed_only` says which one.
fn layer_module_add_export(
    ctx: &mut dyn NativeContext,
    from: ObjectRef,
    pkg: &str,
    to: Option<ObjectRef>,
    unnamed_only: bool,
) -> bool {
    if !ctx.has_layer_module_packages() {
        return false;
    }
    let Some((loader_ns, name)) = user_layer_module_identity(ctx, from) else {
        return false;
    };
    let pkg = pkg.replace('.', "/");
    // The target's identity, read before the call (nothing here allocates).
    let target_layer = to.and_then(|t| user_layer_module_identity(ctx, t));
    let target_name = match to {
        Some(t) if target_layer.is_none() => match ctx.get_field_by_name(t, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
        _ => None,
    };
    let target_unnamed_ns = match to {
        Some(t) if target_layer.is_none() && target_name.is_none() => {
            match ctx.get_field_by_name(t, "loader") {
                Value::Object(Some(l)) => {
                    match crate::classloader::peek_loader_namespace_id(ctx, l) {
                        Some(ns) => Some(ns),
                        // A built-in loader: the bootstrap / platform /
                        // application id `loader_namespace_id` answers without
                        // allocating for them.
                        None => Some(crate::classloader::loader_namespace_id(ctx, l)),
                    }
                }
                _ => Some(cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP),
            }
        }
        _ => None,
    };
    let target = match (to, &target_layer, &target_name, target_unnamed_ns) {
        (None, ..) if unnamed_only => LayerModuleEdgeTarget::AllUnnamed,
        (None, ..) => LayerModuleEdgeTarget::Everyone,
        (Some(_), Some((ns, n)), _, _) => LayerModuleEdgeTarget::Layer {
            loader_ns: *ns,
            name: n,
        },
        (Some(_), None, Some(n), _) => LayerModuleEdgeTarget::Named(n),
        (Some(_), None, None, Some(ns)) => LayerModuleEdgeTarget::Unnamed { loader_ns: ns },
        (Some(_), None, None, None) => return false,
    };
    if dbg_layer_modules() {
        eprintln!("[LAYER-MODULE] addExports {name} loader_ns={loader_ns} {pkg} -> {target:?}");
    }
    ctx.layer_module_add_export(loader_ns, &name, &pkg, target);
    true
}

/// The VM half of `Module.addReads0(from, to)` for a layer module `from` (see
/// [`user_layer_module_identity`]): records the read by
/// identity and answers `true`, or answers `false` and the caller records it
/// by name as before. `to` `None` is `ALL_UNNAMED_MODULE` (`implAddReads`
/// passes `null` for it). `Module.defineModules` calls `addReads0` for every
/// module the configuration resolved `from` to read, so the record is the
/// readability clause of JVMS §5.4.4 for a layer class's `CONSTANT_Class`
/// resolutions (`access_control::class_export_denial`; interpreter round i1
/// wave 45, lane L5).
pub(crate) fn layer_module_add_read(
    ctx: &mut dyn NativeContext,
    from: ObjectRef,
    to: Option<ObjectRef>,
) -> bool {
    if !ctx.has_layer_module_packages() {
        return false;
    }
    let Some((loader_ns, name)) = user_layer_module_identity(ctx, from) else {
        return false;
    };
    // The target's identity, read before the call (nothing here allocates).
    let target_layer = to.and_then(|t| user_layer_module_identity(ctx, t));
    let target_name = match to {
        Some(t) if target_layer.is_none() => match ctx.get_field_by_name(t, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
        _ => None,
    };
    let target_unnamed_ns = match to {
        Some(t) if target_layer.is_none() && target_name.is_none() => {
            match ctx.get_field_by_name(t, "loader") {
                Value::Object(Some(l)) => {
                    match crate::classloader::peek_loader_namespace_id(ctx, l) {
                        Some(ns) => Some(ns),
                        None => Some(crate::classloader::loader_namespace_id(ctx, l)),
                    }
                }
                _ => Some(cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP),
            }
        }
        _ => None,
    };
    let target = match (to, &target_layer, &target_name, target_unnamed_ns) {
        (None, ..) => LayerModuleEdgeTarget::AllUnnamed,
        (Some(_), Some((ns, n)), _, _) => LayerModuleEdgeTarget::Layer {
            loader_ns: *ns,
            name: n,
        },
        (Some(_), None, Some(n), _) => LayerModuleEdgeTarget::Named(n),
        (Some(_), None, None, Some(ns)) => LayerModuleEdgeTarget::Unnamed { loader_ns: ns },
        (Some(_), None, None, None) => return false,
    };
    if dbg_layer_modules() {
        eprintln!("[LAYER-MODULE] addReads {name} loader_ns={loader_ns} -> {target:?}");
    }
    ctx.layer_module_add_read(loader_ns, &name, target);
    true
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
            return Err(RuntimeError::NullPointerException { message: None }.into());
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
    if registry_populated
        && (!ctx.module_is_registered(&name) || ctx.module_outside_boot_configuration(&name))
        && name != "java.xml"
    {
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

    // A module of a non-boot layer runs the JDK's own steps
    // (its opens are the JDK's, its resources are its loader's
    // `findResource(mn, name)`); the name registry and the flat class-path
    // scan below know no such module.
    if is_user_layer_module(ctx, this) {
        let caller = resource_caller_class_id(ctx);
        return layer_module_resource(ctx, this, res_name, caller, LayerResourceDoor::ModuleStream);
    }

    // `isNamed()` — the registry's unnamed-module sentinel is the empty name.
    let module_name = module_registry_name(ctx, this);
    if resource_is_encapsulated_in_module(ctx, &module_name, res_name) {
        return Ok(Some(Value::Object(None)));
    }

    // JIT round 20 wave 7 (lane revNat): a named SYSTEM module answers its
    // own resources only (the JDK body's `BootLoader` / `BuiltinClassLoader`
    // `findResourceAsStream(mn, name)`); the flat scan below served another
    // module's and the class path's (`R20RevNatModuleResourceScope`).
    if !module_name.is_empty() {
        if let Some(scoped) =
            crate::classloader::system_module_scoped_resource(ctx, &module_name, res_name)
        {
            return match scoped {
                Some(bytes) => {
                    let stream =
                        crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes)?;
                    Ok(Some(Value::Object(Some(stream))))
                }
                None => Ok(Some(Value::Object(None))),
            };
        }
    }

    // An unnamed module of a USER loader: the JDK body's last step,
    // `loader.findResource(null, name)` (the loader's own `findResource`, no
    // parent) then `url.openStream()`. The flat scan below answered the
    // application class path instead: a `URLClassLoader`'s own jar entry was
    // null and an application-only resource was served (interpreter round i2
    // wave 57, lane L5; probe `L5W57UserModuleResources`). Where the JDK's
    // `ClassLoader` bodies run (`jdk_url_class_loaders`).
    if module_name.is_empty() && ctx.jdk_url_class_loaders() {
        if let Value::Object(Some(loader)) = ctx.get_field_by_name(this, "loader") {
            if !crate::classloader::object_extends(
                &*ctx,
                loader,
                "jdk/internal/loader/BuiltinClassLoader",
            ) {
                return user_loader_module_resource_stream(ctx, loader, res_name);
            }
        }
    }

    crate::classloader::module_get_resource_as_stream(ctx, args)
}

/// Is the resource `res_name` (no leading `/`) of the named module
/// `module_name` encapsulated from the calling frame's module: in one of the
/// module's own packages, not a `.class`, and the package not open to the
/// caller? The unnamed module (the empty name) encapsulates nothing.
///
/// The one rule behind `Module.getResourceAsStream` and, over a boot-layer
/// module, `Class.getResourceAsStream`'s named-module arm
/// (`lang_class::native_class_get_resource_as_stream`; SBR round 4, lane
/// compat, probe `Sbr3NioBufMod` `boot.class.res`).
pub(crate) fn resource_is_encapsulated_in_module(
    ctx: &mut dyn NativeContext,
    module_name: &str,
    res_name: &str,
) -> bool {
    if module_name.is_empty() || !resource_can_encapsulate(res_name) {
        return false;
    }
    let pkg = resource_package_name(res_name);
    // `getPackages().contains(pn)`: a name outside the module's own
    // packages is not encapsulated. `ModuleRegistry` keys packages in
    // internal (slash) form and so does `pkg`, so this compares like with
    // like — `module_package_names` is the one that dots them, for Java.
    if pkg.is_empty() || !ctx.module_packages(module_name).iter().any(|p| p == pkg) {
        return false;
    }
    // Bound to a local first: a `&mut ctx` reborrow inside a match
    // scrutinee is live for the whole match, and the arms below need
    // `ctx` again.
    let caller_module = resource_caller_module(ctx);
    match caller_module {
        // `caller != Object.class.getModule()`: java.base reads
        // everything, and CratonVM's own JDK-internal resource
        // plumbing runs there.
        Some(caller) if caller == "java.base" => false,
        // `isOpen(pn, caller)`. This also supplies the JDK's
        // `caller != this` exemption: `is_package_open_to` answers
        // true whenever the two module names are equal.
        Some(caller) => !ctx.is_package_open_to(module_name, pkg, &caller),
        // The JDK's `caller == null` arm: unqualified `isOpen(pn)`.
        None => !ctx.is_package_open_unqualified(module_name, pkg),
    }
}

/// [`resource_is_encapsulated_in_module`] for the module of the class a
/// `Class` mirror stands for.
pub(crate) fn class_resource_is_encapsulated(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    res_name: &str,
) -> bool {
    // Under `--compatible` this is the `JdkModuleGraph` row's (the native it is
    // called from is retired, so dropped, under `--jdk-only`).
    if !ctx.aligned(cratonvm_types::HotSpotAlignment::JdkModuleGraph) {
        return false;
    }
    let Some(cid) = ctx.class_id_from_mirror(mirror) else {
        return false;
    };
    let module_name = ctx.module_name_of_class(cid).unwrap_or_default();
    resource_is_encapsulated_in_module(ctx, &module_name, res_name)
}

/// [`native_module_get_resource_as_stream`] for an unnamed module of a user
/// loader: `URL url = loader.findResource(null, name); return url != null ?
/// url.openStream() : null;` (a `SecurityException` from the open is `null`).
#[inline(never)]
fn user_loader_module_resource_stream(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    res_name: &str,
) -> MethodCallResult {
    if !ctx.is_jdk_only() {
        ctx.note_alignment_divergence(cratonvm_types::HotSpotAlignment::JdkUrlClassLoaders);
    }
    let pin = ctx.pin_native_root(loader);
    let name = ctx.create_string_uninterned(res_name);
    let loader = ctx.read_native_pin(pin, loader);
    ctx.unpin_native_roots(pin);
    let url = ctx.invoke_virtual(
        loader,
        "findResource",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/URL;",
        &[Value::Object(None), Value::Object(Some(name))],
    )?;
    let Some(Value::Object(Some(url))) = url else {
        return Ok(Some(Value::Object(None)));
    };
    match ctx.invoke_virtual(url, "openStream", "()Ljava/io/InputStream;", &[]) {
        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc))
            if crate::classloader::object_extends(&*ctx, exc, "java/lang/SecurityException") =>
        {
            Ok(Some(Value::Object(None)))
        }
        other => other,
    }
}

/// `jdk.internal.module.Resources.canEncapsulate` — a name ending in `.class`
/// (and longer than `".class"` itself) is never encapsulated.
fn resource_can_encapsulate(name: &str) -> bool {
    !(name.len() > 6 && name.ends_with(".class"))
}

/// Is `name` (no leading `/`) invisible to the built-in loaders'
/// `getResource` / `getResources` / `getResourceAsStream`?
///
/// HotSpot's `BuiltinClassLoader.findResource(name)` looks a name in a package
/// of a named module up in that module only, and returns it only for a
/// `.class`, a directory, or a package the module's descriptor opens to
/// everyone (`isOpen`). Every other copy -- the run-time image's own, or a
/// class-path jar's -- is answered by no built-in loader: the boot loader
/// refuses it and the platform and application loaders do not own the
/// package. So `ClassLoader.getSystemResource(
/// "jdk/internal/icu/impl/data/icudt76b/uprops.icu")` is `null` on HotSpot,
/// while `java.base`'s own `ICUBinary` still reads it through
/// `Module.getResourceAsStream`. A name outside every named module's packages
/// (`META-INF/...`) stays visible. Probe `tools/probes/HriIcuResources.java`
/// (`loader.*`, `platform.*`, `ccl.*`); see
/// `docs/internal/fixed-suite-bugs/hibernate/hib-reactive-compatible-mode-icu-resource-loading-gap-20261013.md`.
pub(crate) fn builtin_loaders_hide_resource(ctx: &mut dyn NativeContext, name: &str) -> bool {
    if !resource_can_encapsulate(name) {
        return false;
    }
    let pkg = resource_package_name(name);
    if pkg.is_empty() {
        return false;
    }
    match ctx.named_module_for_package(pkg) {
        Some(module) => !ctx.is_package_open_unqualified(&module, pkg),
        None => false,
    }
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

/// The class `Reflection.getCallerClass()` names for
/// `Module.getResourceAsStream`: the innermost frame that is not
/// `java/lang/Module` itself (as [`resource_caller_module`] walks).
fn resource_caller_class_id(ctx: &mut dyn NativeContext) -> Option<cratonvm_types::ClassId> {
    ctx.frame_class_ids().into_iter().find(|cid| {
        !matches!(
            ctx.class_name_arc_of_id(*cid).as_deref(),
            Some("java/lang/Module")
        )
    })
}

/// Which JDK body [`layer_module_resource`] follows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayerResourceDoor {
    /// `Module.getResourceAsStream`: a `java.base` caller is not
    /// encapsulated, an `IOException` of `findResource` propagates, and only a
    /// `SecurityException` of `openStream` is swallowed.
    ModuleStream,
    /// `Class.getResourceAsStream`'s named-module arm: an `IOException` or a
    /// `SecurityException` is `null`.
    ClassStream,
    /// `Class.getResource`'s named-module arm: the URL; an `IOException` of
    /// `findResource` is `null`.
    ClassUrl,
}

/// Is the thrown `exc` one the door answers `null` for?
fn layer_resource_swallows(
    ctx: &dyn NativeContext,
    exc: ObjectRef,
    door: LayerResourceDoor,
    at_open: bool,
) -> bool {
    let io = crate::classloader::object_extends(ctx, exc, "java/io/IOException");
    let security = crate::classloader::object_extends(ctx, exc, "java/lang/SecurityException");
    match door {
        LayerResourceDoor::ModuleStream => at_open && security,
        LayerResourceDoor::ClassStream => io || security,
        LayerResourceDoor::ClassUrl => io,
    }
}

/// The named-module resource lookup of a module of a non-boot
/// `ModuleLayer` ([`is_user_layer_module`]), as the JDK's
/// `Module.getResourceAsStream` and `Class.getResource[AsStream]` make it
/// (JDK 25 `java/lang/Module.java`, `java/lang/Class.java`
/// `isOpenToCaller`):
///
/// 1. a name `Resources.canEncapsulate` in one of the module's packages is
///    `null` unless the caller's module is the module itself, or (the
///    `Module` door) `java.base`, or the module opens the package to it
///    (`isOpen(pn, caller)`; the unqualified `isOpen(pn)` when no caller);
/// 2. the resource is the module's loader's `findResource(mn, name)` -- only
///    this module's reader, never the parent loader or the class path --
///    and a stream is that URL's `openStream()`.
///
/// Before this (interpreter round i1 wave 46, lane L5) `Module
/// .getResourceAsStream` of such a module scanned the VM's flat class path
/// (`null` for a module read from a jar of its own), and `Class.getResource
/// [AsStream]` of its class asked the loader's `getResource[AsStream]`,
/// which also searches the parent loader and serves a package of the module
/// only when it is open to everyone -- so a class could not read its own
/// module's resource in a package not opened to all. The module's packages are
/// the loader's package map `defineModule0` recorded
/// (`layer_module_of_package`), the module's opens are the JDK's
/// `implIsExportedOrOpen(pn, caller, true)` (the body `isOpen` runs for
/// such a module since wave 43).
///
/// `module` must be current; `name` is resolved (no leading `/`).
pub(crate) fn layer_module_resource(
    ctx: &mut dyn NativeContext,
    module: ObjectRef,
    name: &str,
    caller: Option<cratonvm_types::ClassId>,
    door: LayerResourceDoor,
) -> MethodCallResult {
    let null = Ok(Some(Value::Object(None)));
    let Value::Object(Some(loader)) = ctx.get_field_by_name(module, "loader") else {
        return null;
    };
    let loader_ns = crate::classloader::peek_loader_namespace_id(ctx, loader);
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let module_h = scope.root(module);
    let loader_h = scope.root(loader);
    let pkg = resource_package_name(name);
    let member = !pkg.is_empty()
        && loader_ns.is_some_and(|ns| {
            let module = scope.get(&module_h);
            scope.layer_module_of_package(ns, pkg) == Some(module)
        });
    if member && resource_can_encapsulate(name) {
        let java_base_caller = door == LayerResourceDoor::ModuleStream
            && caller
                .is_some_and(|cid| scope.module_name_of_class(cid).as_deref() == Some("java.base"));
        let caller_module = match caller {
            Some(cid) if !java_base_caller => {
                let mirror = scope.get_class_mirror(cid);
                match scope.invoke_virtual(mirror, "getModule", "()Ljava/lang/Module;", &[])? {
                    Some(Value::Object(Some(m))) => Some(m),
                    _ => None,
                }
            }
            _ => None,
        };
        let same = caller_module.is_some_and(|m| m == scope.get(&module_h));
        if !java_base_caller && !same {
            let caller_h = caller_module.map(|m| scope.root(m));
            let pn = scope.create_string_uninterned(&pkg.replace('/', "."));
            let module = scope.get(&module_h);
            let caller_module = caller_h.as_ref().map(|h| scope.get(h));
            let open = match user_layer_module_is_exported_or_open(
                &mut *scope,
                module,
                Value::Object(Some(pn)),
                caller_module,
                true,
            ) {
                Some(answer) => !matches!(answer?, Some(Value::Int(0))),
                // No `EVERYONE_MODULE` to ask with: keep the lookup.
                None => true,
            };
            if dbg_layer_modules() {
                eprintln!(
                    "[LAYER-MODULE] resource {name} of a layer module, caller {} -> {}",
                    match caller {
                        Some(cid) => scope.class_name_of_id(cid).unwrap_or_default(),
                        None => "<none>".to_string(),
                    },
                    if open { "open" } else { "encapsulated" }
                );
            }
            if !open {
                return null;
            }
        }
    }
    let module = scope.get(&module_h);
    let mn = scope.get_field_by_name(module, "name");
    let mn_h = match mn {
        Value::Object(Some(s)) => scope.root(s),
        _ => return null,
    };
    let name_str = scope.create_string_uninterned(name);
    let loader = scope.get(&loader_h);
    let mn = scope.get(&mn_h);
    let url = match scope.invoke_virtual(
        loader,
        "findResource",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/URL;",
        &[Value::Object(Some(mn)), Value::Object(Some(name_str))],
    ) {
        Ok(Some(Value::Object(Some(url)))) => url,
        Ok(_) => return null,
        Err(MethodCallFailed::ExceptionThrown(exc))
            if layer_resource_swallows(&*scope, exc, door, false) =>
        {
            return null;
        }
        Err(e) => return Err(e),
    };
    if dbg_layer_modules() {
        eprintln!("[LAYER-MODULE] resource {name} -> the module loader's findResource(mn, name)");
    }
    if door == LayerResourceDoor::ClassUrl {
        return Ok(Some(Value::Object(Some(url))));
    }
    match scope.invoke_virtual(url, "openStream", "()Ljava/io/InputStream;", &[]) {
        Err(MethodCallFailed::ExceptionThrown(exc))
            if layer_resource_swallows(&*scope, exc, door, true) =>
        {
            null
        }
        other => other,
    }
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
    // Never mints: a Module nothing was recorded for has no row.
    let recorded = existing_module_packages_key(&*ctx, this).and_then(|key| {
        module_packages_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    });
    // Round 13 wave 10 (lane misc10): a dynamic proxy module (`jdk.proxyN`) is
    // not in the registry, so the fallback below answered the whole of
    // `java.base`. HotSpot's `getPackages()` of a named module is
    // `descriptor.packages()`, and `Class.getModule()` gave this Module
    // `getDynamicModule`'s real descriptor. `CRATONVM_PROXY_MODULE_DESCRIPTOR=0`
    // restores the fallback.
    // The UNNAMED module of a user-defined loader (`--jdk-only` since wave
    // 45, `--compatible` since wave 48, lane L6). The JDK's
    // body answers the packages of `loader.packages()` whose module is this
    // one -- the loader's `packages` map, which a define fills
    // (`postDefineClass` -> `getNamedPackage`, wave 42) -- so run it. The
    // registry fallback below answered the whole of `java.base` for it
    // (`L5W44LoaderReview`, `unnamed-module`; interpreter round i1 wave 45,
    // lane L5). The built-in loaders' unnamed modules too, over a real JDK
    // with `CRATONVM_BUILTIN_LOADER_PACKAGE_SYNC` (interpreter round i2 wave
    // 55, lane L5): the VM defines their classes without the JDK's
    // `BuiltinClassLoader.defineClass`, and the `ClassLoader.packages()`
    // intrinsic the body reaches puts their packages in the map first
    // (`lang_class::sync_builtin_loader_packages`). The fallback below
    // answered the registry's 63 packages for the application loader's
    // unnamed module, without the class path's (probe
    // `L5W55UnnamedModulePackages`).
    if recorded.is_none() && !matches!(ctx.get_field_by_name(this, "name"), Value::Object(Some(_)))
    {
        if let Value::Object(Some(loader)) = ctx.get_field_by_name(this, "loader") {
            if crate::classloader::is_user_defined_loader(ctx, loader)
                || ctx.builtin_loader_package_sync()
            {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "getPackages",
                    "()Ljava/util/Set;",
                    &[],
                );
            }
        }
    }
    if recorded.is_none() {
        let module_name = module_registry_name(ctx, this);
        if crate::reflect_annotations::proxy_module_answers_from_descriptor(&module_name) {
            if let Value::Object(Some(desc)) = ctx.get_field_by_name(this, "descriptor") {
                if let Value::Object(Some(packages)) = ctx.get_field_by_name(desc, "packages") {
                    return Ok(Some(Value::Object(Some(packages))));
                }
            }
        }
    }
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
    // NULL for an UNNAMED module, and that is the JDK's own first line rather
    // than a special case:
    //
    //     public ModuleLayer getLayer() {
    //         if (isNamed()) { ... }
    //         return null;
    //     }
    //
    // with `isNamed()` being `name != null`. Seeding the boot layer for EVERY
    // receiver made this the ONE row of `apps/probes/ClassModuleSweep.java`
    // that disagreed with HotSpot 25: 31 of 32 byte-identical, and row 32
    // `layer of unnamed is null` answering `false` where HotSpot answers
    // `true`. The retired `the-system-props-static-field-is-null` write-up
    // recorded that deviation, named it as `getLayer`'s defect rather than
    // `Class.getModule()`'s, and left it standing so the checked-in sweep
    // would go red the day it was repaired. This is the repair.
    //
    // Read by NAME, like `native_module_get_name` beside it. Every `Module`
    // this file sees is an instance of the REAL class, whose layout is
    // `layer`(0) `name`(1) `loader`(2) `descriptor`(3) -- so a slot-0 fallback
    // would read the `layer` this very function seeds and then call a module
    // named by its own previous call. `MODULE_FIELD_COUNT`'s doc comment
    // above records what that collision cost the last time it was assumed.
    //
    // The NAMED path is untouched, which is what the held
    // `Module.getLayer` row of `retired_shadow.rs` depends on
    // (`L0ClassModuleSurface` row 103: yielding answers `false` on
    // `java.base` where HotSpot says `true`, so the native stays).
    if !matches!(ctx.get_field_by_name(this, "name"), Value::Object(Some(_))) {
        return Ok(Some(Value::Object(None)));
    }
    let existing = ctx.get_field_by_name(this, "layer");
    if let Value::Object(Some(_)) = existing {
        return Ok(Some(existing));
    }
    // Round 13 wave 10 (lane misc10): a dynamic proxy module (`jdk.proxyN`,
    // `Proxy.ProxyBuilder.getDynamicModule`) is defined with a null layer, and
    // the JDK special-cases only `java.base`, so HotSpot answers null. Never
    // seed the boot layer into it. `CRATONVM_PROXY_MODULE_DESCRIPTOR=0`
    // restores the seeding.
    let module_name = module_registry_name(ctx, this);
    if crate::reflect_annotations::proxy_module_answers_from_descriptor(&module_name) {
        return Ok(Some(Value::Object(None)));
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
    // Both modes (`--compatible` since wave 48, lane L6): a layer the JDK's
    // own `ModuleLayer.defineModules*` built
    // answers as `ModuleLayer.modules()` does -- ONE unmodifiable
    // `Set.copyOf(nameToModule.values())`, cached in `modules` -- instead of a
    // fresh mutable `HashSet` per call (interpreter round i1 wave 43, lane
    // L5, `L5W43UserLayerModule` row `layer-modules`).
    if let Some(Value::Object(Some(layer))) = args.first().copied() {
        if is_user_module_layer(ctx, layer) {
            return user_module_layer_modules(ctx, layer);
        }
    }
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

/// A `java.lang.ModuleLayer` the JDK's own code built (not the synthetic boot
/// layer): its class is `ModuleLayer`, it has a configuration, and it is not
/// this VM's boot-layer memo.
fn is_user_module_layer(ctx: &mut dyn NativeContext, layer: ObjectRef) -> bool {
    if ctx
        .class_name_of_id(ctx.class_id_of_object(layer))
        .as_deref()
        != Some("java/lang/ModuleLayer")
    {
        return false;
    }
    if !matches!(ctx.get_field_by_name(layer, "cf"), Value::Object(Some(_))) {
        return false;
    }
    !matches!(boot_layer_peek(&*ctx), Some(boot) if boot == layer)
}

/// `ModuleLayer.modules()` of an [`is_user_module_layer`] layer, as the JDK's
/// body answers it. The layer's `servicesCatalog` is filled first by the
/// JDK's own `getServicesCatalog()`, because `service_loader.rs` calls this
/// method for that side effect before it reads the field.
fn user_module_layer_modules(ctx: &mut dyn NativeContext, layer: ObjectRef) -> MethodCallResult {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let layer_h = scope.root(layer);
    scope.invoke(
        "java/lang/ModuleLayer",
        "getServicesCatalog",
        "()Ljdk/internal/module/ServicesCatalog;",
        &[Value::Object(Some(layer))],
    )?;
    let layer = scope.get(&layer_h);
    if let Value::Object(Some(modules)) = scope.get_field_by_name(layer, "modules") {
        return Ok(Some(Value::Object(Some(modules))));
    }
    let Value::Object(Some(map)) = scope.get_field_by_name(layer, "nameToModule") else {
        return scope.invoke("java/util/Set", "of", "()Ljava/util/Set;", &[]);
    };
    let Some(Value::Object(Some(values))) =
        scope.invoke_virtual(map, "values", "()Ljava/util/Collection;", &[])?
    else {
        return scope.invoke("java/util/Set", "of", "()Ljava/util/Set;", &[]);
    };
    let copied = scope.invoke(
        "java/util/Set",
        "copyOf",
        "(Ljava/util/Collection;)Ljava/util/Set;",
        &[Value::Object(Some(values))],
    )?;
    if let Some(Value::Object(Some(set))) = copied {
        let layer = scope.get(&layer_h);
        scope.set_field_by_name(layer, "modules", Value::Object(Some(set)));
    }
    Ok(copied)
}

/// `java.lang.module.Configuration.modules()`. The
/// configuration's own `modules` set when it has one -- every configuration
/// the JDK's `Configuration.resolve*` built (an unmodifiable set, the same
/// object on every call) and the synthetic boot one alike; the layer native
/// this used to share answered a fresh mutable `HashSet` of the
/// `nameToModule` values per call and ran a `ServicesCatalog` over the
/// `ResolvedModule`s as if they were `Module`s (interpreter round i1 wave 43,
/// lane L5, `L5W43UserLayerModule` row `cf-modules`). Both modes since wave
/// 48 (lane L6).
pub(crate) fn native_configuration_modules(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(cf))) = args.first().copied() {
        if let Value::Object(Some(modules)) = ctx.get_field_by_name(cf, "modules") {
            return Ok(Some(Value::Object(Some(modules))));
        }
    }
    native_module_layer_modules(ctx, args)
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
    is_automatic: bool,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    // Hoisted for the same reason as in `build_requires_set`: `<clinit>` runs
    // Java and can move the heap, so it must not run while `set` is live.
    let modifier_enum = if is_open || is_automatic {
        ctx.ensure_class_initialized(MODULE_MODIFIER_ENUM).ok()
    } else {
        None
    };
    let mut set = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "modifiers")?;
    if let Some(enum_id) = modifier_enum {
        // Pinned across the loop for the same reason `build_requires_set`
        // pins `mods`: `collection_add` re-enters Java (`Enum.hashCode`) and
        // can move `set` between the two possible additions.
        let pin = ctx.pin_native_root(set);
        for (wanted, constant) in [(is_open, "OPEN"), (is_automatic, "AUTOMATIC")] {
            if !wanted {
                continue;
            }
            set = ctx.read_native_pin(pin, set);
            if let Some(value) = enum_constant(ctx, enum_id, constant) {
                collection_add(ctx, set, value)?;
            }
        }
        set = ctx.read_native_pin(pin, set);
        ctx.unpin_native_roots(pin);
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
/// `MANDATED` / `SYNTHETIC` and `compiledVersion`/`rawCompiledVersion` now
/// come from [`crate::NativeContext::module_requires_full`] (W2-3's
/// out-of-file patch, landed). `requires java.base` now reports `[MANDATED]`,
/// matching HotSpot.
fn build_requires_set(
    ctx: &mut dyn NativeContext,
    entries: &[(String, bool, bool, bool, bool, Option<String>)],
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
    let modifier_enum =
        if entries
            .iter()
            .any(|(_, transitive, is_static, is_synthetic, is_mandated, _)| {
                *transitive || *is_static || *is_synthetic || *is_mandated
            })
        {
            ctx.ensure_class_initialized(REQUIRES_MODIFIER_ENUM).ok()
        } else {
            None
        };

    let set = new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "requires")?;
    let pin = ctx.pin_native_root(set);
    for (name, transitive, is_static, is_synthetic, is_mandated, compiled_version) in entries {
        let element =
            try_alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor$Requires", 4)?;
        let element_pin = ctx.pin_native_root(element);

        let mut mods =
            new_initialized_object(ctx, "java/util/HashSet", "()V", &[], "requires mods")?;
        let mods_pin = ctx.pin_native_root(mods);
        if let Some(enum_id) = modifier_enum {
            for (wanted, constant) in [
                (*transitive, "TRANSITIVE"),
                (*is_static, "STATIC"),
                (*is_mandated, "MANDATED"),
                (*is_synthetic, "SYNTHETIC"),
            ] {
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

        // `compiledVersion`/`rawCompiledVersion` stay null when the producer
        // recorded no `requires_version` — the same "absence is the truthful
        // answer" rule `build_module_descriptor` applies to the module's own
        // `version`/`rawVersionString`.
        if let Some(raw) = compiled_version {
            let raw_str = ctx.create_string(raw);
            let raw_pin = ctx.pin_native_root(raw_str);
            // `Version.parse` runs real JDK bytecode and can move the heap —
            // `raw_str` is re-derived from its pin, and `element` from its
            // own pin, once it returns.
            let parsed = ctx.invoke(
                "java/lang/module/ModuleDescriptor$Version",
                "parse",
                "(Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Version;",
                &[Value::Object(Some(raw_str))],
            );
            let raw_str = ctx.read_native_pin(raw_pin, raw_str);
            ctx.unpin_native_roots(raw_pin);
            let element = ctx.read_native_pin(element_pin, element);
            ctx.set_field_by_name(element, "rawCompiledVersion", Value::Object(Some(raw_str)));
            if let Ok(Some(Value::Object(Some(version_obj)))) = parsed {
                let element = ctx.read_native_pin(element_pin, element);
                ctx.set_field_by_name(element, "compiledVersion", Value::Object(Some(version_obj)));
            }
        }
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
    // A module path's AUTOMATIC module is registered open (every package
    // exported and open, it reads every module), but its descriptor is
    // `ModuleDescriptor.newAutomaticModule`'s: `isAutomatic()` true,
    // `isOpen()` false, modifiers `[AUTOMATIC]` (interpreter round i5 wave 101,
    // lane L5; `L5W100AutomaticModulePackages` printed `automatic=false
    // open=true`).
    let module_path_automatic = ctx.module_is_module_path_automatic(module_name);
    let is_open = ctx.module_is_open(module_name) && !module_path_automatic;
    let is_automatic = ctx.module_is_automatic(module_name) || module_path_automatic;
    let version = ctx.module_version(module_name);
    let main_class = ctx.module_main_class(module_name).map(|s| dotted(&s));
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
    let requires = ctx.module_requires_full(module_name);

    let desc = try_alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor", 16)?;
    let pin = ctx.pin_native_root(desc);
    let name = ctx.create_string(module_name);
    // `mut`: the `version`/`mainClass` blocks below reassign it from inside a
    // nested `if let`, which a shadowing `let` cannot make visible past the
    // block's end.
    let mut desc = ctx.read_native_pin(pin, desc);
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
    // Sourced from `ctx.module_is_automatic` (W2-3's out-of-file patch,
    // landed): true for a `module-info.class` found on the CLASS path. Was a
    // hardcoded `false`, which made `RJdkModule`'s
    // `check(!d.isAutomatic(), ...)` pass for the wrong reason — it would have
    // passed against the hardcode whatever the module really was.
    ctx.set_field_by_name(
        desc,
        "automatic",
        Value::Int(if is_automatic { 1 } else { 0 }),
    );

    // `version`/`rawVersionString` stay null when `ctx.module_version` answers
    // `None` — real `ModuleDescriptor.version()`/`rawVersion()` are
    // `Optional.ofNullable(field)`, so a null field already renders as
    // `Optional.empty()`, the honest "nothing was recorded" answer and the
    // correct one for a module-info that carries no version. Writing a
    // fabricated value on the `None` arm would turn a truthful empty into a
    // false claim, so nothing is written there.
    if let Some(raw) = version {
        let raw_str = ctx.create_string(&raw);
        let raw_pin = ctx.pin_native_root(raw_str);
        // `Version.parse` runs real JDK bytecode and can move the heap.
        let parsed = ctx.invoke(
            "java/lang/module/ModuleDescriptor$Version",
            "parse",
            "(Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Version;",
            &[Value::Object(Some(raw_str))],
        );
        let raw_str = ctx.read_native_pin(raw_pin, raw_str);
        ctx.unpin_native_roots(raw_pin);
        desc = ctx.read_native_pin(pin, desc);
        ctx.set_field_by_name(desc, "rawVersionString", Value::Object(Some(raw_str)));
        if let Ok(Some(Value::Object(Some(version_obj)))) = parsed {
            desc = ctx.read_native_pin(pin, desc);
            ctx.set_field_by_name(desc, "version", Value::Object(Some(version_obj)));
        }
    }

    // `mainClass` stays null on the `None` arm for the same reason: absence is
    // the truthful answer for a module-info with no `ModuleMainClass`
    // attribute, not a gap to paper over.
    if let Some(main_class) = main_class {
        let main_class_str = ctx.create_string(&main_class);
        desc = ctx.read_native_pin(pin, desc);
        ctx.set_field_by_name(desc, "mainClass", Value::Object(Some(main_class_str)));
    }

    // `modifiers` carries OPEN when the module is open and AUTOMATIC when it
    // is automatic; SYNTHETIC/MANDATED are never legal on a `module`
    // directive itself (only on `requires`/`exports`/`opens`), so no
    // module-level source for them exists. See `build_module_modifier_set`.
    let modifiers = build_module_modifier_set(ctx, is_open, is_automatic)?;
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

/// A `jdk.internal.module.ModuleHashes` whose `names()` are the module names
/// the VM read from `module`'s `ModuleHashes` attribute (hash bytes are not
/// kept: nothing in the VM verifies them, and the empty array stands in).
/// `None` when the module-info recorded none.
fn build_recorded_hashes(
    ctx: &mut dyn NativeContext,
    module: &str,
) -> Result<Option<ObjectRef>, cratonvm_types::error::MethodCallFailed> {
    let Some(names) = ctx.module_recorded_hash_names(module) else {
        return Ok(None);
    };
    let map = new_initialized_object(ctx, "java/util/HashMap", "()V", &[], "recorded hashes")?;
    let map_pin = ctx.pin_native_root(map);
    for n in &names {
        let key = ctx.create_string_uninterned(n);
        let key_pin = ctx.pin_native_root(key);
        let empty = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
        let key = ctx.read_native_pin(key_pin, key);
        let map = ctx.read_native_pin(map_pin, map);
        ctx.invoke(
            "java/util/HashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(map)),
                Value::Object(Some(key)),
                Value::Object(Some(empty)),
            ],
        )?;
        ctx.unpin_native_roots(key_pin);
    }
    let alg = ctx.create_string("SHA-256");
    let alg_pin = ctx.pin_native_root(alg);
    let map = ctx.read_native_pin(map_pin, map);
    let alg = ctx.read_native_pin(alg_pin, alg);
    let hashes = new_initialized_object(
        ctx,
        "jdk/internal/module/ModuleHashes",
        "(Ljava/lang/String;Ljava/util/Map;)V",
        &[Value::Object(Some(alg)), Value::Object(Some(map))],
        "ModuleHashes",
    );
    ctx.unpin_native_roots(alg_pin);
    ctx.unpin_native_roots(map_pin);
    Ok(hashes.ok())
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

    // `java.base`'s recorded `ModuleHashes`, as the JDK's own boot reference
    // carries them: `StackTraceElement.HashedModules` reads
    // `recordedHashes().names()` of this reference to decide which modules
    // `computeFormat` prints without a version (SBR2 W9 SMALL2). Best effort:
    // a failure leaves the field null, as before.
    if name == "java.base" {
        if let Some(hashes) = build_recorded_hashes(ctx, name)? {
            let mref = ctx.read_native_pin(mref_pin, mref);
            ctx.set_field_by_name(mref, "recordedHashes", Value::Object(Some(hashes)));
        }
    }

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
    args: &[Value],
) -> MethodCallResult {
    // `module_ref_open` (`reflect_annotations.rs`) stamps every
    // `SystemModuleReader` it mints with the owning module's name in the
    // `module` field. Reading it back here is what makes this native able to
    // answer per-module, unlike the old unconditional-empty stub (see this
    // function's own history above for why that stub stopped being safe).
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return cratonvm_native_collections::make_stream_from_elements(ctx, &[]),
    };
    // The real `SystemModuleReader.list()` starts with `ensureOpen()`: a closed
    // reader answers `IOException("ModuleReader is closed")`. The reader's real
    // `closed` flag is what `close()` (JDK bytecode) writes (SBR3 niobuf).
    if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(v) if v != 0) {
        return Err(RuntimeError::IOException {
            message: "ModuleReader is closed".to_string(),
        }
        .into());
    }
    let module_name = match ctx.get_field_by_name(this, "module") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if module_name.is_empty() {
        return cratonvm_native_collections::make_stream_from_elements(ctx, &[]);
    }
    let java_home = ctx.get_system_property("java.home").unwrap_or_default();
    let names = crate::phases_late::nio_file::jrt_module_resource_list(&java_home, &module_name);
    let elements: Vec<Value> = names
        .iter()
        .map(|n| Value::Object(Some(ctx.create_string(n))))
        .collect();
    cratonvm_native_collections::make_stream_from_elements(ctx, &elements)
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
    // A layer module's export is recorded by its identity.
    let to_module = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    if to_module.is_some() && layer_module_add_export(ctx, from, &pkg, to_module, false) {
        return Ok(None);
    }
    // A name-map module's export TO a module of a user `ModuleLayer` is
    // recorded with the target's identity (its loader namespace), so the
    // layer questions under `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION` tell
    // it from an edge to the boot layer's (or another layer's) module of the
    // same name (interpreter round i6 wave 125, lane L5). Recorded in every
    // mode and with the switch off too: only the switch's readers consult the
    // identity, and the name-keyed questions see the edge as before.
    let target_layer = to_module.and_then(|t| user_layer_module_identity(ctx, t));
    let to = match args.get(2) {
        Some(Value::Object(Some(o))) => module_registry_name(ctx, *o),
        _ => String::new(),
    };
    let from_name = module_registry_name(ctx, from);
    // The registry keys packages in internal (slash) form.
    let pkg = pkg.replace('.', "/");
    match target_layer {
        Some((target_ns, _)) => {
            ctx.module_add_exports_to_layer_module(&from_name, &pkg, &to, target_ns)
        }
        None => ctx.module_add_exports(&from_name, &pkg, &to),
    }
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
    // A layer module's export is recorded by its identity.
    if layer_module_add_export(ctx, from, &pkg, None, target == ALL_UNNAMED_TARGET) {
        return Ok(None);
    }
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
        native_configuration_modules,
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
                if let Some(platform) = crate::classloader::platform_loader_for_module(ctx, &name) {
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
        let frames = ctx.capture_stack_trace();
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

    /// `Module.getPackages()` returns a set in the REAL `HashSet` shape: one
    /// field, `map`, holding a populated `HashMap`.
    ///
    /// The layout is declared to the mock so `build_real_layout_string_hashset`
    /// takes its real branch, which is the branch a real-JDK run takes and the
    /// one `build_package_set`'s doc is about. This assertion used to read
    /// `ctx.get_field(set, 1)` and expect an element COUNT there, which is the
    /// fabricated `(array, size, capacity)` shape -- on a class whose one real
    /// instance field is `map`, so slot 1 is off the end of it. That shape is
    /// what `getPackages().size()` answering 0 came from in the first place,
    /// and the last unconditional write of it (the helper's fallback arm) went
    /// with the synthetic slot floor on 2026-09-12. Asserting it here would
    /// have required putting that write back.
    #[test]
    fn module_get_packages_returns_populated_set_for_java_base() {
        let mut ctx = MockNativeContext::new();
        // The real JDK 25 layouts, so `build_real_layout_string_hashset` takes
        // its REAL branch -- the branch a real-JDK run takes. `declare` below
        // registers the class id the way `ensure_class_initialized` does and
        // then attaches the field metadata `resolve_field_index` reads.
        fn declare(ctx: &mut MockNativeContext, class_name: &str, fields: &[(&str, &str)]) {
            let cid = ctx
                .ensure_class_initialized(class_name)
                .expect("mock class registration cannot fail");
            let metadata = fields
                .iter()
                .enumerate()
                .map(
                    |(slot_index, (name, descriptor))| cratonvm_native_api::FieldMetadata {
                        name: (*name).to_string(),
                        descriptor: (*descriptor).to_string(),
                        access_flags: 0,
                        slot_index,
                        declaring_class_id: cid,
                        is_static: false,
                    },
                )
                .collect();
            ctx.set_declared_fields(cid, metadata);
        }
        declare(
            &mut ctx,
            "java/util/HashMap",
            &[
                ("table", "[Ljava/util/HashMap$Node;"),
                ("size", "I"),
                ("threshold", "I"),
                ("loadFactor", "F"),
                ("entrySet", "Ljava/util/Set;"),
            ],
        );
        declare(
            &mut ctx,
            "java/util/HashMap$Node",
            &[
                ("hash", "I"),
                ("key", "Ljava/lang/Object;"),
                ("value", "Ljava/lang/Object;"),
                ("next", "Ljava/util/HashMap$Node;"),
            ],
        );
        declare(
            &mut ctx,
            "java/util/HashSet",
            &[("map", "Ljava/util/HashMap;")],
        );

        let layer = build_boot_layer(&mut ctx).expect("boot layer should build");
        let module = build_module(&mut ctx, "java.base", layer)
            .expect("build_module must succeed for a synthetic layer");
        let result = native_module_get_packages(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        let Value::Object(Some(set)) = result else {
            panic!("expected non-null Set, got {:?}", result);
        };
        // Slot 0 is `HashSet.map`, the class's ONLY instance field.
        let Value::Object(Some(map)) = ctx.get_field(set, 0) else {
            panic!(
                "HashSet.map must hold the backing HashMap, got {:?}",
                ctx.get_field(set, 0)
            );
        };
        // Slot 1 of the map is `HashMap.size`, and it is where the package
        // count actually lives.
        match ctx.get_field(map, 1) {
            Value::Int(n) => assert!(n > 0, "backing map size must be > 0, got {}", n),
            other => panic!("expected Int for HashMap.size, got {:?}", other),
        }
        // The table is a real bucket array, not the element array the
        // fabricated shape used to put at slot 0 of the SET.
        match ctx.get_field(map, 0) {
            Value::Object(Some(_)) => {}
            other => panic!("expected a bucket array in HashMap.table, got {:?}", other),
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

#[cfg(test)]
mod w22a_module_state_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Private VM identities: no other test uses them.
    const W22A_VM_A: usize = 0x0A22_6A08;
    const W22A_VM_B: usize = 0x0A22_6B08;

    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            for vm in [W22A_VM_A, W22A_VM_B] {
                forget_vm_jboss_module_state(vm);
                crate::forget_vm_lock_keys(vm);
            }
        }
    }

    fn packages_of(key: ModulePackagesKey) -> Option<Vec<String>> {
        module_packages_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    /// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
    /// item 5, and the package table found beside it): a Module's recorded
    /// package set is keyed by its VM (and, since gc-common w29-e, its weak
    /// lock key rather than its identity hash), so the same object address in
    /// another VM finds nothing of it; a VM's teardown drops its boot-layer
    /// memo row and its package rows, and no other VM's.
    #[test]
    fn w22a_module_state_is_per_vm_and_forgotten_with_the_vm() {
        let _forget = Forget;
        let mut a = crate::test_utils::mock_ctx();
        a.set_vm_identity(W22A_VM_A);
        let b = crate::test_utils::mock_ctx();
        b.set_vm_identity(W22A_VM_B);
        let module = a.fresh_object_ref();
        let key_a = module_packages_key(&a, module).expect("minted");
        assert_eq!(key_a.0, W22A_VM_A);
        module_packages_record(key_a, vec!["w22a.a".to_string()]);
        assert_eq!(
            existing_module_packages_key(&b, module),
            None,
            "the same address in VM B is not VM A's Module"
        );
        let key_b = module_packages_key(&b, module).expect("minted");
        assert_ne!(key_a, key_b);
        module_packages_record(key_b, vec!["w22a.b".to_string()]);
        assert_eq!(packages_of(key_a), Some(vec!["w22a.a".to_string()]));

        {
            let mut memo = boot_layer_memo().lock().unwrap_or_else(|e| e.into_inner());
            memo.insert(W22A_VM_A, 11);
            memo.insert(W22A_VM_B, 12);
        }
        forget_vm_jboss_module_state(W22A_VM_A);
        assert_eq!(packages_of(key_a), None);
        assert_eq!(packages_of(key_b), Some(vec!["w22a.b".to_string()]));
        {
            let memo = boot_layer_memo().lock().unwrap_or_else(|e| e.into_inner());
            assert!(!memo.contains_key(&W22A_VM_A));
            assert_eq!(memo.get(&W22A_VM_B), Some(&12));
        }
        forget_vm_jboss_module_state(W22A_VM_B);
        assert_eq!(packages_of(key_b), None);
        assert!(!boot_layer_memo()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&W22A_VM_B));
    }
}

/// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 19): two live Modules of ONE VM that share an identity hash keep their
/// own package sets, and a dead one's row goes with its freed lock key.
#[cfg(test)]
mod w29e_module_packages_collider_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Private VM identity: no other test uses it.
    const VM: usize = 0x29E0_1908;

    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_jboss_module_state(VM);
            crate::forget_vm_lock_keys(VM);
        }
    }

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the mock hashes the address.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn recorded(ctx: &dyn NativeContext, module: ObjectRef) -> Option<Vec<String>> {
        let key = existing_module_packages_key(ctx, module)?;
        module_packages_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    #[test]
    fn same_hash_modules_keep_their_own_packages() {
        let _forget = Forget;
        let ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        // 4 GiB apart: one mock identity hash (address truncated to i32).
        let (a, b) = (at(0x1_29E0_1900), at(0x2_29E0_1900));
        assert_eq!(ctx.identity_hash_code(a), ctx.identity_hash_code(b));

        module_packages_record(
            module_packages_key(&ctx, a).expect("minted"),
            vec!["w29e.a".to_string()],
        );
        assert_eq!(recorded(&ctx, b), None, "B was never recorded");
        module_packages_record(
            module_packages_key(&ctx, b).expect("minted"),
            vec!["w29e.b".to_string()],
        );
        assert_eq!(recorded(&ctx, a), Some(vec!["w29e.a".to_string()]));
        assert_eq!(recorded(&ctx, b), Some(vec!["w29e.b".to_string()]));

        // A dies: the lock-key sweep frees its key and drops its row only.
        let b_addr = b.as_ptr() as usize;
        crate::gc_sweep_lock_keys(VM, &|x| x == b_addr);
        assert_eq!(recorded(&ctx, a), None);
        assert_eq!(recorded(&ctx, b), Some(vec!["w29e.b".to_string()]));
        assert!(!module_packages_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(&(vm, _), v)| vm == VM && v == &vec!["w29e.a".to_string()]));
    }
}
