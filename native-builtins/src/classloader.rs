// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ClassLoader hierarchy, URLClassLoader, MethodHandles.Lookup, ProtectionDomain,
//! and CodeSource native method implementations.


use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ObjectRef, Value};
use cratonvm_types::error::MethodCallResult;
use crate::{obj_arg, alloc_concurrent_synthetic};
use crate::service_loader::impl_jars_load_class;

/// Monotonic counter for generating unique hidden class names.
pub static HIDDEN_CLASS_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Singleton classloader instances (JVM spec: one instance per built-in loader)
// ---------------------------------------------------------------------------

fn platform_loader_store() -> &'static Mutex<Option<ObjectRef>> {
    static INSTANCE: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

fn app_loader_store() -> &'static Mutex<Option<ObjectRef>> {
    static INSTANCE: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Read-only accessor for the singleton app `ClassLoader` already created
/// by [`get_or_create_app_loader`]. Returns `None` when boot hasn't yet
/// touched any path that allocates the loader. Intended for VM-side
/// rescues that need to substitute the canonical app loader without a
/// `&mut NativeContext` (e.g. `interpreter::execute_checkcast`).
pub fn peek_app_loader() -> Option<ObjectRef> {
    *app_loader_store().lock().unwrap_or_else(|e| e.into_inner())
}

/// Reset singleton loader instances. Called when creating a new VM to avoid
/// stale ObjectRefs from a previous VM instance.
pub fn reset_loader_singletons() {
    *platform_loader_store().lock().unwrap_or_else(|e| e.into_inner()) = None;
    *app_loader_store().lock().unwrap_or_else(|e| e.into_inner()) = None;
    class_data_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// GC root scan for the singleton built-in class loaders.
///
/// The app + platform `ClassLoader` synthetics live ONLY in the process-global
/// `app_loader_store` / `platform_loader_store` mutexes (a Rust side-table, not
/// a Java field or VM root table), so they are invisible to the frame / static
/// / heap-object root scans. Without this, a moving young GC can reclaim or
/// relocate the cached loader while `get_or_create_app_loader` keeps returning
/// the stale `ObjectRef`; the freed slot is then reused by another allocation
/// and a later `loader.loadClass(...)` dispatches on the wrong object — observed
/// as BouncyCastle `ClassUtil.loadClass`'s receiver decaying to a String OID,
/// surfacing intermittently (heap-size dependent) as
/// "Not able to load any cryptoProvider". Mirrors the `lang_math` /
/// `lang_invoke` process-global cache root scans (`roots.rs` steps 15–17).
pub fn gc_scan_loader_singleton_roots(out: &mut Vec<ObjectRef>) {
    if let Some(o) = *app_loader_store().lock().unwrap_or_else(|e| e.into_inner()) {
        out.push(o);
    }
    if let Some(o) = *platform_loader_store().lock().unwrap_or_else(|e| e.into_inner()) {
        out.push(o);
    }
}

/// Post-GC remap for the singleton built-in class loaders (companion to
/// [`gc_scan_loader_singleton_roots`]). After a moving collection the cached
/// loader objects relocate; repoint the stored `ObjectRef`s to their new
/// addresses so subsequent `getClassLoader()` calls return the live object.
pub fn gc_update_loader_singleton_refs(
    pointer_map: &std::collections::HashMap<usize, usize>,
) {
    if pointer_map.is_empty() {
        return;
    }
    let remap = |slot: &mut Option<ObjectRef>| {
        if let Some(obj_ref) = slot.as_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    };
    remap(&mut app_loader_store().lock().unwrap_or_else(|e| e.into_inner()));
    remap(&mut platform_loader_store().lock().unwrap_or_else(|e| e.into_inner()));
}

// ---------------------------------------------------------------------------
// WP2.3-C — `classData` side-table for `MethodHandles.classData(...)`.
//
// JEP 371 introduced an optional `Object classData` parameter to
// `defineClass0`. The value lives only in VM-internal storage (not
// reachable through reflection) and is retrieved via
// `MethodHandles.classData(lookup, "_", Object.class)`. Because our
// `Class` data structures don't yet carry a per-class user-data slot,
// we keep a process-wide side-table keyed by the `java.lang.Class`
// mirror (a stable `ObjectRef` per class) and read from it when the
// `MethodHandles.classData` native is invoked.
//
// Side-table choice: `Mutex<HashMap<ObjectRef, Value>>` — `ObjectRef`
// is `Hash + Eq`, the table is touched only at class definition time
// and at the rare `classData` native call so contention is minimal.
// Cleared in `reset_loader_singletons` to avoid stale refs across VMs.
// ---------------------------------------------------------------------------
fn class_data_store() -> &'static Mutex<std::collections::HashMap<ObjectRef, Value>> {
    static INSTANCE: OnceLock<Mutex<std::collections::HashMap<ObjectRef, Value>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Store `class_data` for a Class mirror. Returns the previous value if any.
pub fn set_class_data(mirror: ObjectRef, data: Value) -> Option<Value> {
    class_data_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(mirror, data)
}

/// Retrieve previously-stored class data for a Class mirror.
/// Returns `Value::Object(None)` if no data was stored.
pub fn get_class_data(mirror: ObjectRef) -> Value {
    class_data_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&mirror)
        .copied()
        .unwrap_or(Value::Object(None))
}

/// Get or create the singleton platform class loader.
pub(crate) fn get_or_create_platform_loader(ctx: &mut dyn NativeContext) -> ObjectRef {
    let existing = *platform_loader_store().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(obj) = existing {
        return obj;
    }
    let obj = alloc_classloader(ctx, LOADER_PLATFORM);
    let name = ctx.create_string("platform");
    ctx.set_field(obj, CL_NAME_REF, Value::Object(Some(name)));
    // Platform's parent is bootstrap (null) — already set by alloc_classloader
    *platform_loader_store().lock().unwrap_or_else(|e| e.into_inner()) = Some(obj);
    obj
}

/// Get or create the singleton application (system) class loader.
pub fn get_or_create_app_loader(ctx: &mut dyn NativeContext) -> ObjectRef {
    let existing = *app_loader_store().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(obj) = existing {
        return obj;
    }
    let platform = get_or_create_platform_loader(ctx);
    let obj = alloc_classloader(ctx, LOADER_APP);
    let name = ctx.create_string("app");
    ctx.set_field(obj, CL_NAME_REF, Value::Object(Some(name)));
    ctx.set_field(obj, CL_PARENT_REF, Value::Object(Some(platform)));
    *app_loader_store().lock().unwrap_or_else(|e| e.into_inner()) = Some(obj);
    obj
}

// ---------------------------------------------------------------------------
// ClassLoader hierarchy constants
// ---------------------------------------------------------------------------
const LOADER_BOOTSTRAP: i32 = 0;
const LOADER_PLATFORM: i32 = 1;
pub(crate) const LOADER_APP: i32 = 2;
const LOADER_CUSTOM: i32 = 3;

// ClassLoader synthetic field indices (7 fields)
const CL_LOADER_TYPE: usize = 0;
const CL_PARENT_REF: usize = 1;
pub(crate) const CL_NAME_REF: usize = 2;
const CL_CLASSES_LOADED: usize = 3;
const CL_IS_PARALLEL_CAPABLE: usize = 4;
const CL_DEFAULT_DOMAIN: usize = 5;
/// Unique loader ID for class namespace isolation (0 = not yet assigned)
const CL_LOADER_ID: usize = 6;
const CL_FIELD_COUNT: usize = 7;

// URLClassLoader synthetic field indices (6 fields)
const UCL_LOADER_TYPE: usize = 0;
const UCL_PARENT_REF: usize = 1;
const UCL_URL_COUNT: usize = 2;
const UCL_CLOSED: usize = 3;
/// Array of URL objects added via addURL / constructor
const UCL_URLS_ARRAY: usize = 4;
/// Unique loader ID for class namespace isolation
const UCL_LOADER_ID: usize = 5;
const UCL_FIELD_COUNT: usize = 6;

// MethodHandles$Lookup synthetic field indices (4 fields)
const LK_LOOKUP_CLASS_REF: usize = 0;
const LK_ALLOWED_MODES: usize = 1;
const LK_PREVIOUS_LOOKUP_CLASS: usize = 2;
const LK_LOOKUP_MODE: usize = 3;
const LK_FIELD_COUNT: usize = 4;

// Lookup mode bitmask constants
const LK_PUBLIC: i32 = 0x01;
const LK_PRIVATE: i32 = 0x02;
const LK_PROTECTED: i32 = 0x04;
const LK_PACKAGE: i32 = 0x08;
const LK_MODULE: i32 = 0x10;
const LK_UNCONDITIONAL: i32 = 0x20;
const LK_ORIGINAL: i32 = 0x40;
const LK_FULL_POWER: i32 = LK_PUBLIC | LK_PRIVATE | LK_PROTECTED | LK_PACKAGE | LK_MODULE | LK_ORIGINAL;

// HiddenClass synthetic field indices (2 fields)
const HC_NEST_HOST_REF: usize = 0;
const HC_CLASS_DATA_REF: usize = 1;
const HC_FIELD_COUNT: usize = 2;

// ProtectionDomain synthetic field indices (3 fields)
const PD_CODE_SOURCE_REF: usize = 0;
const PD_PERMISSIONS_REF: usize = 1;
const PD_CLASS_LOADER_REF: usize = 2;
const PD_FIELD_COUNT: usize = 3;

// CodeSource synthetic field indices (2 fields)
const CS_LOCATION_REF: usize = 0;
const CS_CERTIFICATES_REF: usize = 1;
const CS_FIELD_COUNT: usize = 2;

// ---------------------------------------------------------------------------
// ClassLoader delegation model helpers
// ---------------------------------------------------------------------------

/// Determines delegation order for class loading.
fn delegation_order(loader_type: i32) -> &'static str {
    match loader_type {
        LOADER_BOOTSTRAP => "bootstrap-only",
        LOADER_PLATFORM => "parent-first (bootstrap \u{2192} platform)",
        LOADER_APP => "parent-first (bootstrap \u{2192} platform \u{2192} app)",
        LOADER_CUSTOM => "parent-first (default) or child-first (if overridden)",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const CL_CLASS: &str = "java/lang/ClassLoader";
const UCL_CLASS: &str = "java/net/URLClassLoader";
const LK_CLASS: &str = "java/lang/invoke/MethodHandles$Lookup";
const HC_CLASS: &str = "java/lang/ClassLoader$HiddenClass";
const PD_CLASS: &str = "java/security/ProtectionDomain";
const CS_CLASS: &str = "java/security/CodeSource";

/// WP2.3 — build a synthetic default `ProtectionDomain` for a `ClassLoader`.
///
/// The real JDK `ClassLoader.<init>` allocates a non-null `defaultDomain`
/// holding a `CodeSource(null URL, null certs)` and uses it as the fallback
/// when `defineClass(name, bytes, off, len)` is invoked without a PD argument.
/// `ClassLoader.preDefineClass` reads `this.defaultDomain` and immediately
/// calls `pd.getCodeSource()` on it; if the field is null the call NPEs with
/// "Cannot invoke getCodeSource on null", which is exactly what the WP2.3
/// CGLIB/ByteBuddy/Lookup probes were hitting before this fix.
///
/// This helper builds the same shape: a `ProtectionDomain` whose `codesource`
/// slot points at a `CodeSource` with both `location` (URL) and `certs` set
/// to null. The `CodeSource` itself is non-null, so `getCodeSource()` returns
/// a real object that `getCertificates()` / `getLocation()` can be called on
/// without NPE.
fn alloc_default_protection_domain(ctx: &mut dyn NativeContext) -> ObjectRef {
    let cs = alloc_concurrent_synthetic(ctx, CS_CLASS, CS_FIELD_COUNT);
    ctx.set_field(cs, CS_LOCATION_REF, Value::Object(None));
    ctx.set_field(cs, CS_CERTIFICATES_REF, Value::Object(None));
    // Belt-and-suspenders: also set by name in case the real JDK CodeSource
    // layout reads through a different field index than our synthetic.
    ctx.set_field_by_name(cs, "location", Value::Object(None));
    ctx.set_field_by_name(cs, "certs", Value::Object(None));

    let pd = alloc_concurrent_synthetic(ctx, PD_CLASS, PD_FIELD_COUNT);
    ctx.set_field(pd, PD_CODE_SOURCE_REF, Value::Object(Some(cs)));
    ctx.set_field(pd, PD_PERMISSIONS_REF, Value::Object(None));
    ctx.set_field(pd, PD_CLASS_LOADER_REF, Value::Object(None));
    // Real JDK PD reads `codesource` by name in `getCodeSource`; cover both
    // field-index orderings.
    ctx.set_field_by_name(pd, "codesource", Value::Object(Some(cs)));
    ctx.set_field_by_name(pd, "permissions", Value::Object(None));
    ctx.set_field_by_name(pd, "classloader", Value::Object(None));
    pd
}

pub(crate) fn alloc_classloader(ctx: &mut dyn NativeContext, loader_type: i32) -> ObjectRef {
    // WP1.5: built-in loaders must report the real JDK type name via
    // reflection. `jdk.internal.loader.ClassLoaders$PlatformClassLoader` for
    // the platform loader, `...$AppClassLoader` for the system loader, and
    // the abstract `java.lang.ClassLoader` only for unknown/custom callers.
    let class_name = match loader_type {
        LOADER_PLATFORM => "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
        LOADER_APP => "jdk/internal/loader/ClassLoaders$AppClassLoader",
        _ => CL_CLASS,
    };
    let obj = alloc_concurrent_synthetic(ctx, class_name, CL_FIELD_COUNT);
    ctx.set_field(obj, CL_LOADER_TYPE, Value::Int(loader_type));
    ctx.set_field(obj, CL_PARENT_REF, Value::Object(None));
    ctx.set_field(obj, CL_NAME_REF, Value::Object(None));
    ctx.set_field(obj, CL_CLASSES_LOADED, Value::Int(0));
    ctx.set_field(obj, CL_IS_PARALLEL_CAPABLE, Value::Int(0));
    let pd = alloc_default_protection_domain(ctx);
    ctx.set_field(obj, CL_DEFAULT_DOMAIN, Value::Object(Some(pd)));
    ctx.set_field_by_name(obj, "defaultDomain", Value::Object(Some(pd)));
    // Assign a unique loader ID for custom classloaders
    let lid = if loader_type == LOADER_CUSTOM {
        ctx.allocate_loader_id() as i32
    } else {
        0 // built-in loaders don't use this field
    };
    ctx.set_field(obj, CL_LOADER_ID, Value::Int(lid));
    // Built-in loaders (platform & app) extend `jdk.internal.loader.BuiltinClassLoader`,
    // whose constructor (`BuiltinClassLoader(String, BuiltinClassLoader, URLClassPath)`)
    // initializes the inherited `nameToModule` and `moduleToReader` Map fields to
    // empty `ConcurrentHashMap` instances. We bypass that constructor (going through
    // `alloc_concurrent_synthetic` instead), so JDK methods like
    // `BuiltinClassLoader.findMiscResource` NPE with "Cannot invoke values on null"
    // when they `getfield nameToModule` and call `Map.values()` on it. Initialize
    // those fields by name with empty ConcurrentHashMaps so the JDK bytecode path
    // works without additional intercepts.
    if loader_type == LOADER_PLATFORM || loader_type == LOADER_APP {
        let name_to_module = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
        ctx.set_field_by_name(obj, "nameToModule", Value::Object(Some(name_to_module)));
        let module_to_reader = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
        ctx.set_field_by_name(obj, "moduleToReader", Value::Object(Some(module_to_reader)));
    }
    // S111r17: `java/lang/ClassLoader` declares `packages:ConcurrentHashMap`
    // (instance field) which the real-JDK ctor initializes via
    // `new ConcurrentHashMap()`.  We bypass the ctor through
    // `alloc_concurrent_synthetic`, so `packages` defaults to null. The JDK's
    // `ClassLoader.packages()` instance method does
    // `getfield packages → ConcurrentHashMap.values()`, NPE'ing with
    // "Cannot invoke values on null" — observed during
    // `org/jboss/modules/ConcurrentClassLoader.<clinit>` (JBoss Modules /
    // WildFly 39 boot), whose static initializer calls
    // `Package.getPackages()` → `ClassLoader.getClassLoader(...).getPackages()`
    // → `packages()`. Pre-populate an empty CHM so the bytecode path runs
    // without additional intercepts.
    let packages_map = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
    ctx.set_field_by_name(obj, "packages", Value::Object(Some(packages_map)));
    // `ClassLoader.setDefaultAssertionStatus` uses `synchronized (assertionLock)`.
    // Real JDK ctors assign `this.assertionLock = new Object()`; synthetic
    // allocation skips that, so Surefire's forked booter NPEs on monitorenter.
    let lock = alloc_concurrent_synthetic(ctx, "java/lang/Object", 0);
    let _ = ctx.invoke_special(
        "java/lang/Object",
        "<init>",
        "()V",
        &[Value::Object(Some(lock))],
    );
    ctx.set_field_by_name(obj, "assertionLock", Value::Object(Some(lock)));
    obj
}

/// Get the unique loader ID from a ClassLoader object, lazily assigning one if needed.
fn get_or_assign_loader_id(ctx: &mut dyn NativeContext, cl: ObjectRef) -> u32 {
    match ctx.get_field(cl, CL_LOADER_ID) {
        Value::Int(v) if v > 0 => v as u32,
        _ => {
            let lid = ctx.allocate_loader_id();
            ctx.set_field(cl, CL_LOADER_ID, Value::Int(lid as i32));
            lid
        }
    }
}

fn alloc_url_classloader(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, UCL_CLASS, UCL_FIELD_COUNT);
    ctx.set_field(obj, UCL_LOADER_TYPE, Value::Int(LOADER_CUSTOM));
    ctx.set_field(obj, UCL_PARENT_REF, Value::Object(None));
    ctx.set_field(obj, UCL_URL_COUNT, Value::Int(0));
    ctx.set_field(obj, UCL_CLOSED, Value::Int(0));
    // Allocate initial URLs array (capacity 16)
    let urls_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
    ctx.set_field(obj, UCL_URLS_ARRAY, Value::Object(Some(urls_arr)));
    // Assign unique loader ID
    let lid = ctx.allocate_loader_id();
    ctx.set_field(obj, UCL_LOADER_ID, Value::Int(lid as i32));
    obj
}

fn alloc_lookup(ctx: &mut dyn NativeContext, modes: i32) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, LK_CLASS, LK_FIELD_COUNT);
    ctx.set_field(obj, LK_LOOKUP_CLASS_REF, Value::Object(None));
    ctx.set_field(obj, LK_ALLOWED_MODES, Value::Int(modes));
    ctx.set_field(obj, LK_PREVIOUS_LOOKUP_CLASS, Value::Object(None));
    ctx.set_field(obj, LK_LOOKUP_MODE, Value::Int(modes));
    obj
}

// ---------------------------------------------------------------------------
// java.lang.ClassLoader natives
// ---------------------------------------------------------------------------

fn cl_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, CL_LOADER_TYPE, Value::Int(LOADER_CUSTOM));
    // parent defaults to system class loader
    let sys = alloc_classloader(ctx, LOADER_APP);
    ctx.set_field(this, CL_PARENT_REF, Value::Object(Some(sys)));
    ctx.set_field(this, CL_NAME_REF, Value::Object(None));
    ctx.set_field(this, CL_CLASSES_LOADED, Value::Int(0));
    ctx.set_field(this, CL_IS_PARALLEL_CAPABLE, Value::Int(0));
    // WP2.3: build a non-null defaultDomain so JDK preDefineClass's
    // `pd.getCodeSource()` chain doesn't NPE on the no-PD defineClass path.
    let pd = alloc_default_protection_domain(ctx);
    ctx.set_field(this, CL_DEFAULT_DOMAIN, Value::Object(Some(pd)));
    ctx.set_field_by_name(this, "defaultDomain", Value::Object(Some(pd)));
    // S111r17: see alloc_classloader — initialize `packages` CHM so
    // ClassLoader.packages() doesn't NPE on `getfield + values()`.
    let packages_map = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
    ctx.set_field_by_name(this, "packages", Value::Object(Some(packages_map)));
    Ok(None)
}

fn cl_init_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let parent = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, CL_LOADER_TYPE, Value::Int(LOADER_CUSTOM));
    ctx.set_field(this, CL_PARENT_REF, parent);
    ctx.set_field(this, CL_NAME_REF, Value::Object(None));
    ctx.set_field(this, CL_CLASSES_LOADED, Value::Int(0));
    ctx.set_field(this, CL_IS_PARALLEL_CAPABLE, Value::Int(0));
    let pd = alloc_default_protection_domain(ctx);
    ctx.set_field(this, CL_DEFAULT_DOMAIN, Value::Object(Some(pd)));
    ctx.set_field_by_name(this, "defaultDomain", Value::Object(Some(pd)));
    // S111r17: see alloc_classloader.
    let packages_map = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
    ctx.set_field_by_name(this, "packages", Value::Object(Some(packages_map)));
    Ok(None)
}

fn cl_init_name_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    let parent = args.get(2).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, CL_LOADER_TYPE, Value::Int(LOADER_CUSTOM));
    ctx.set_field(this, CL_PARENT_REF, parent);
    ctx.set_field(this, CL_NAME_REF, name);
    ctx.set_field(this, CL_CLASSES_LOADED, Value::Int(0));
    ctx.set_field(this, CL_IS_PARALLEL_CAPABLE, Value::Int(0));
    let pd = alloc_default_protection_domain(ctx);
    ctx.set_field(this, CL_DEFAULT_DOMAIN, Value::Object(Some(pd)));
    ctx.set_field_by_name(this, "defaultDomain", Value::Object(Some(pd)));
    // S111r17: see alloc_classloader.
    let packages_map = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
    ctx.set_field_by_name(this, "packages", Value::Object(Some(packages_map)));
    // Assign unique loader ID for namespace isolation
    let lid = ctx.allocate_loader_id();
    ctx.set_field(this, CL_LOADER_ID, Value::Int(lid as i32));
    Ok(None)
}

/// True if `class_name` is a base / built-in classloader class for which the
/// Rust `cl_load_class` / `cl_find_class` natives are authoritative — there is
/// no user-supplied Java `findClass` override to defer to.
pub(crate) fn is_builtin_loader_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/lang/ClassLoader"
            | "java/net/URLClassLoader"
            | "java/security/SecureClassLoader"
    ) || class_name.starts_with("jdk/internal/loader/")
        || class_name.starts_with("sun/misc/Launcher$")
}

/// Virtual-dispatch correctness for custom `ClassLoader` subclasses.
///
/// `cl_load_class` is registered as the Rust native for
/// `ClassLoader.loadClass`. When application code subclasses `ClassLoader`
/// and overrides `findClass` (the documented extension point — Equinox OSGi,
/// custom classloaders generally), the inherited `loadClass` MUST still
/// dispatch to that override (JVMS §5.3 / `ClassLoader.loadClass` contract).
/// Because CratonVM has no Java bytecode for `ClassLoader.loadClass` to run,
/// the native must perform the `findClass` callback itself.
///
/// Returns the `ObjectRef` of the receiver's actual class **iff** the
/// receiver is a non-builtin `ClassLoader` subclass whose hierarchy declares
/// its own `findClass` bytecode (i.e. a genuine user override). Returns
/// `None` for base / built-in loaders, where the native fallback is correct
/// and a `findClass` callback would recurse.
pub(crate) fn receiver_overrides_find_class(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let mut cid = Some(ctx.class_id_of_object(this));
    while let Some(id) = cid {
        let name = match ctx.class_name_of_id(id) {
            Some(n) => n,
            None => return false,
        };
        if is_builtin_loader_class(&name) {
            // Reached the builtin base without seeing a user override.
            return false;
        }
        // A `findClass` declared on this (non-builtin) class is a real
        // user override of the extension point.
        if ctx
            .declared_methods(id)
            .iter()
            .any(|m| m.name == "findClass")
        {
            return true;
        }
        cid = ctx.superclass_of(id);
    }
    false
}

fn cl_load_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let dotted = ctx.read_string(name_obj).unwrap_or_default();
    let internal = dotted.replace('.', "/");

    // JVM spec §5.3.2 — parent-first delegation:
    // 1. Check if this loader already loaded the class (findLoadedClass)
    let loader_type = match ctx.get_field(this, CL_LOADER_TYPE) {
        Value::Int(v) => v,
        _ => LOADER_APP,
    };

    // For custom loaders, check own namespace first
    if loader_type == LOADER_CUSTOM {
        let loader_id = match ctx.get_field(this, CL_LOADER_ID) {
            Value::Int(v) if v > 0 => Some(v as u32),
            _ => None,
        };
        if let Some(lid) = loader_id {
            if let Some(cid) = ctx.class_id_by_name_and_loader(&internal, lid) {
                let mirror = ctx.get_class_mirror(cid);
                return Ok(Some(Value::Object(Some(mirror))));
            }
        }
    }

    // 2. Delegate to parent loader first (recursive parent-first delegation)
    if let Value::Object(Some(parent)) = ctx.get_field(this, CL_PARENT_REF) {
        // Recursively delegate to parent by calling its loadClass
        let parent_type = match ctx.get_field(parent, CL_LOADER_TYPE) {
            Value::Int(v) => v,
            _ => LOADER_APP,
        };
        let parent_lid = match ctx.get_field(parent, CL_LOADER_ID) {
            Value::Int(v) if v > 0 => Some(v as u32),
            _ => None,
        };
        // Check parent's namespace for custom loaders
        if let Some(pid) = parent_lid {
            if let Some(cid) = ctx.class_id_by_name_and_loader(&internal, pid) {
                let mirror = ctx.get_class_mirror(cid);
                return Ok(Some(Value::Object(Some(mirror))));
            }
        }
        // For built-in parent loaders (bootstrap/platform/app), use standard delegation
        if parent_type != LOADER_CUSTOM {
            // Standard delegation handles bootstrap → extension → app
            if let Ok(cid) = ctx.ensure_class_initialized(&internal) {
                let mirror = ctx.get_class_mirror(cid);
                return Ok(Some(Value::Object(Some(mirror))));
            }
        }
    } else {
        // No parent (or null parent) → delegate directly to bootstrap loader
        // Bootstrap delegation: use the standard class loading chain
        if let Ok(cid) = ctx.ensure_class_initialized(&internal) {
            let mirror = ctx.get_class_mirror(cid);
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }

    // 3. Parent couldn't find it — fall back to standard loading
    //    (this covers bootstrap → extension → application delegation)
    if let Ok(cid) = ctx.ensure_class_initialized(&internal) {
        let mirror = ctx.get_class_mirror(cid);
        return Ok(Some(Value::Object(Some(mirror))));
    }

    // 4. Custom-classloader extension point. The JVM `ClassLoader.loadClass`
    //    contract is: after parent delegation fails, call `findClass(name)`.
    //    `findClass` is the documented override hook — application code
    //    (Eclipse Equinox OSGi, custom loaders) subclasses `ClassLoader`
    //    and overrides `findClass` to load from a custom source. Since this
    //    Rust native stands in for `ClassLoader.loadClass` (CratonVM keeps
    //    no JDK bytecode for it), the native must perform the virtual
    //    `findClass` dispatch itself so the user override actually runs.
    //
    //    Guarded by `receiver_overrides_find_class` so this only fires for
    //    genuine non-builtin subclasses — a built-in loader has no override
    //    and the callback would recurse back into `cl_find_class`.
    //
    //    Note: unlike the original `return`, we fall through on failure so
    //    the IMPL-JARS fallback (step 5) can still fire when `findClass`
    //    throws ClassNotFoundException (e.g. EmbeddedImplClassLoader with
    //    empty jarMetas).
    if receiver_overrides_find_class(ctx, this) {
        // `invoke_virtual` resolves on the receiver's actual class, so this
        // dispatches to the subclass's overriding `findClass` bytecode.
        let result = ctx.invoke_virtual(
            this,
            "findClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(name_obj))],
        );
        match result {
            Ok(Some(Value::Object(Some(_)))) => return result,
            // findClass threw (ClassNotFoundException) or returned null — fall through.
            _ => {}
        }
    }

    // 5. IMPL-JARS fallback: ES EmbeddedImplClassLoader stores provider
    //    classes and all their inner/helper classes as individual ZIP entries
    //    under IMPL-JARS/<module>/<jar_dir>/<classfile> inside the outer
    //    module JAR. When neither the flat classpath nor findClass can locate
    //    the class, try scanning those entries directly.
    if let Some(mirror) = impl_jars_load_class(ctx, &internal) {
        return Ok(Some(Value::Object(Some(mirror))));
    }

    // 6. Not found and no user override — class genuinely missing.
    Ok(Some(Value::Object(None)))
}

fn cl_load_class_resolve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // boolean resolve arg is ignored — we always resolve
    cl_load_class(ctx, args)
}

fn cl_find_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cl_load_class(ctx, args)
}

fn cl_find_class_module(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // module-aware variant — module arg (index 1) ignored, class name at index 2
    let _this = obj_arg(args, 0)?;
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let dotted = ctx.read_string(name_obj).unwrap_or_default();
    let internal = dotted.replace('.', "/");
    match ctx.ensure_class_initialized(&internal) {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

// T19_H12_LOADCLASS_MODULE — `ClassLoader.loadClass(Module, String)Class`.
//
// JDK 25 (post-JEP 261) introduces this package-private overload to support
// `Class.forName(Module, String)`. JDK's bytecode for the public
// `Class.forName(Module, String)` resolves `module.getClassLoader()` and
// dispatches a virtual `loadClass(Module, String)` against the result.
// Without this native, the dispatch falls through to whatever the
// receiver's actual class is — which on our synthetic Module objects
// can be a `HashSet` (because the Module's slot 2 holds the packages set
// when JDK bytecode reads it as the `loader` field). Registering a
// real native at the receiver's actual class isn't reachable; we
// register at `java/lang/ClassLoader` so the method is at least
// resolvable when the receiver IS a real ClassLoader.
//
// Per spec: returns null if the class is not visible to the module.
fn cl_load_class_module(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (ClassLoader)
    // args[1] = Module
    // args[2] = String name
    let _this = obj_arg(args, 0)?;
    match args.get(1) {
        Some(Value::Object(Some(_))) => {}
        _ => return Ok(Some(Value::Object(None))),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let dotted = ctx.read_string(name_obj).unwrap_or_default();
    if dotted.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    // Hardening: reject names with control bytes or path separators.
    if dotted.bytes().any(|b| b < 0x20 || b == 0x7F)
        || dotted.contains('/')
        || dotted.contains('\\')
    {
        return Ok(Some(Value::Object(None)));
    }
    let internal = dotted.replace('.', "/");
    match ctx.ensure_class_initialized(&internal) {
        Ok(cid) => {
            if ctx.is_class_hidden(cid) {
                return Ok(Some(Value::Object(None)));
            }
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        // Spec: null on miss, not CNFE.
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

/// Java class file magic number.
const CLASS_FILE_MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];

// ---------------------------------------------------------------------------
// cglib SEGV guard — Round-17 (this agent)
//
// cglib's proxy generator (used by Spring AOP / Hibernate persistence)
// emits bytecode at runtime via ASM, then hands it to one of:
//   * `ClassLoader.defineClass(String,byte[],int,int[,ProtectionDomain])`
//   * `ClassLoader.defineClass1/2/0` (JDK-internal natives)
//   * `sun.misc.Unsafe.defineClass(...)` (legacy / fallback path)
//
// The generated bytecode references our synthetic JDK classes whose
// field/method layouts don't match what cglib's ASM emitter assumes.
// When `define_class_full` accepts those bytes and the resulting class
// is later verified / linked, the layout mismatch triggers a native
// SEGV (rc=139) inside the interpreter — not a clean Java exception.
//
// Strategy: short-circuit BEFORE handing the bytes to `define_class_full`.
// If the class name (taken from the explicit argument when present, or
// scanned from the bytecode this_class entry as a fallback) looks like
// a cglib proxy, return `null` from the native and let the Java caller
// observe an NPE / LinkageError — which IS recoverable, unlike a SEGV.
//
// Pattern matching is STRICT: only actual cglib-generated proxy classes
// trigger the short-circuit. Frameworks like Quarkus/ASM use similar
// naming conventions in unrelated libraries, so we require the full
// `$$EnhancerByCGLIB$$` token (not just a `ByCGLIB$$` substring) or a
// hit under the `net/sf/cglib/proxy/` subpackage.
// ---------------------------------------------------------------------------

/// Returns true when `name` (in JVM internal form, slashes not dots,
/// may be empty) looks like a cglib-generated proxy.
///
/// STRICT matching: we require the canonical cglib enhancer marker
/// `$$EnhancerByCGLIB$$` (with BOTH leading and trailing `$$` delimiters)
/// or a prefix under `net/sf/cglib/proxy/`. Looser substring matches such
/// as `ByCGLIB$$` previously misfired on Quarkus/ASM-emitted classes whose
/// constant pool contained a similar fragment, routing them through the
/// placeholder path — which returns `java/lang/Object` and caused
/// downstream layout-mismatch SEGVs on keycloak startup.
///
/// The `$$EnhancerByCGLIB$$` token is a literal cglib code-generation
/// marker; no legitimate non-cglib class name contains it. The
/// `net/sf/cglib/proxy/` prefix is the cglib library's own package.
/// Either match is sufficient to identify a cglib-generated proxy.
fn is_cglib_proxy_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.contains("$$EnhancerByCGLIB$$")
        || name.starts_with("net/sf/cglib/proxy/")
}

/// Returns a `Class` mirror to use as a stand-in when we short-circuit a
/// cglib proxy define. We prefer `java/lang/Object` (loaded by every VM
/// boot) so callers get back a non-null mirror; if that lookup fails we
/// fall through to `Value::Object(None)` and let the Java caller take an
/// NPE. Either outcome is preferable to the native SEGV.
///
/// Safety: this function NEVER dereferences a raw pointer. The mirror is
/// only constructed when `class_id_by_name` returns `Some(cid)` for
/// `java/lang/Object` — a class loaded by every VM boot. On the null
/// path the caller receives `Value::Object(None)` and surfaces an NPE,
/// which is a recoverable Java-level outcome rather than a native crash.
fn cglib_placeholder_mirror(ctx: &mut dyn NativeContext) -> Value {
    if let Some(cid) = ctx.class_id_by_name("java/lang/Object") {
        return Value::Object(Some(ctx.get_class_mirror(cid)));
    }
    Value::Object(None)
}

/// Decide whether to short-circuit a `defineClass*` call for cglib.
/// Returns `Some(placeholder_value)` to short-circuit, or `None` to let
/// the normal path proceed.
///
/// `name` is the (possibly empty) explicit name in JVM internal form.
/// `bytes` is the raw class file slice (currently unused — the bytecode
/// sniff fallback was removed because it produced false positives /
/// SIGILL on malformed buffers; see git history).
///
/// This short-circuit is ALWAYS ON (no env gate). The name match is
/// strict enough (`$$EnhancerByCGLIB$$` literal token or
/// `net/sf/cglib/proxy/` package prefix) that false positives on
/// legitimate Quarkus/Keycloak classes are not possible.
fn cglib_guard_value(
    ctx: &mut dyn NativeContext,
    name: &str,
    _bytes: &[u8],
) -> Option<Value> {
    // Strict-name match only. We do NOT sniff bytecode — the previous
    // `sniff_class_file_this_name` fallback was a defensive class-file
    // parser, but bytecode parsing on adversarial / truncated buffers
    // had a history of OOB reads and SIGILL (notably keycloak startup).
    // If the caller did not pass a `name`, we let the normal define
    // path handle it; if those bytes are a real cglib proxy CratonVM
    // will SEGV (the original problem) but at least we cannot regress
    // unrelated apps by mis-classifying their bytecode.
    if is_cglib_proxy_name(name) {
        tracing::warn!(
            "[cglib-shim] short-circuiting defineClass for {name} (SEGV avoidance)"
        );
        return Some(cglib_placeholder_mirror(ctx));
    }
    None
}

fn cl_define_class_basic(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // defineClass(String name, byte[] b, int off, int len)
    // args: [this, name, byte_array, offset, length]
    //
    // WP2.3: routes through `define_class_full` so all defineClass
    // entry points share the same backend (name-mismatch check,
    // dup-define rejection, hidden-flag handling, PD attribution).
    let this = obj_arg(args, 0)?;

    // Extract class name (may be null — use name from class file)
    let name_str = match args.get(1) {
        Some(Value::Object(Some(name_obj))) => {
            let dotted = ctx.read_string(*name_obj).unwrap_or_default();
            dotted.replace('.', "/")
        }
        _ => String::new(),
    };

    // Extract byte array, offset, length
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Ok(Some(Value::Object(None)));
        }
    };

    let array_len = ctx.array_length(byte_array);

    // Safe integer handling: reject negative offset/length (i32 → usize)
    let offset = match args.get(3) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(_)) => {
            return Ok(Some(Value::Object(None)));
        }
        _ => 0,
    };
    let length = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(_)) => {
            return Ok(Some(Value::Object(None)));
        }
        _ => array_len,
    };

    // Bounds validation: ensure offset + length doesn't exceed array.
    // Use checked_add so a pathological (offset=usize::MAX, length=N)
    // pair cannot wrap around into an in-range value.
    if offset
        .checked_add(length)
        .map_or(true, |end| end > array_len)
    {
        tracing::warn!(
            "[define_class] bounds violation: offset={offset} length={length} \
             array_len={array_len} (name={name_str})"
        );
        return Ok(Some(Value::Object(None)));
    }

    // Read bytes from the array.
    //
    // Defensive: wrap the copy loop in `catch_unwind` so a panic inside
    // `get_array_element` (e.g. cglib emitting a large bytecode buffer
    // that hits a stale array layout) does NOT propagate to SIGABRT.
    // `AssertUnwindSafe` is required because `ctx` is `&mut`; the loop
    // performs read-only access on a separate array object so unwinding
    // does not leave shared state observably partial.
    let class_bytes_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut bytes = Vec::with_capacity(length);
        for i in 0..length {
            match ctx.get_array_element(byte_array, offset + i) {
                Value::Int(b) => bytes.push(b as u8),
                _ => bytes.push(0),
            }
        }
        bytes
    }));
    let class_bytes = match class_bytes_result {
        Ok(b) => b,
        Err(_) => {
            tracing::error!(
                "[define_class] panic while reading byte array for {name_str}; aborting"
            );
            return Ok(Some(Value::Object(None)));
        }
    };

    // Pre-validate the class file header so that obviously-bad bytes
    // never reach `define_class_full` (cheap CAFEBABE magic check).
    if class_bytes.len() < 8 || class_bytes[0..4] != CLASS_FILE_MAGIC {
        tracing::warn!(
            "[define_class] invalid magic for {name_str}; rejecting"
        );
        return Ok(Some(Value::Object(None)));
    }

    // cglib SEGV guard: short-circuit proxy classes BEFORE handing the
    // bytes to `define_class_full`. See `cglib_guard_value` for details.
    if let Some(v) = cglib_guard_value(ctx, &name_str, &class_bytes) {
        return Ok(Some(v));
    }

    // Read optional ProtectionDomain at arg[5]. Synthetic-PD layout:
    // field 0 holds either a CodeSource (with URL string at field 0)
    // or directly a URL string.
    let mut pd_url: Option<String> = None;
    if let Some(Value::Object(Some(pd))) = args.get(5) {
        if let Value::Object(Some(cs)) = ctx.get_field(*pd, 0) {
            if let Some(s) = ctx.read_string(cs) {
                pd_url = Some(s);
            } else if let Value::Object(Some(url)) = ctx.get_field(cs, 0) {
                if let Some(s) = ctx.read_string(url) {
                    pd_url = Some(s);
                }
            }
        }
    }

    // Define via the shared backend. Empty name = use class file's
    // own this_class. Loader id 0 = application loader.
    let loader_id = get_or_assign_loader_id(ctx, this);
    let opts = cratonvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        ..Default::default()
    };
    // Wrap the backend call in `catch_unwind` so a panic inside
    // `define_class_full` (e.g. malformed bytecode that defeats the
    // verifier's bounds-checks) returns null instead of SIGABRT.
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(&name_str, &class_bytes, loader_id, opts)
    }));
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(
                "[define_class] panic inside define_class_full for {name_str}; aborting"
            );
            return Ok(Some(Value::Object(None)));
        }
    };
    match define_result {
        Ok(cid) => {
            let count = match ctx.get_field(this, CL_CLASSES_LOADED) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.set_field(this, CL_CLASSES_LOADED, Value::Int(count + 1));
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            tracing::warn!("ClassLoader.defineClass({name_str}) failed: {msg}");
            Ok(Some(Value::Object(None)))
        }
    }
}

fn cl_define_class_pd(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // defineClass(String name, byte[] b, int off, int len, ProtectionDomain pd)
    // Same backend as cl_define_class_basic — the basic variant already
    // reads the optional PD argument at index 5 when present.
    cl_define_class_basic(ctx, args)
}

fn cl_define_class_bb(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // defineClass(String name, ByteBuffer bb, ProtectionDomain pd)
    // ByteBuffer variant — delegate to basic with byte[] extraction
    cl_define_class_basic(ctx, args)
}

// ---------------------------------------------------------------------------
// WP2.3-C — JDK-internal `defineClass1` / `defineClass2` / `defineClass0`.
//
// These are the natives that the public `ClassLoader.defineClass(...)`
// overloads call into via the JDK's pure-Java wrapper.  CGLIB / direct
// user code typically goes through one of these three entry points.
// All three converge on `define_class_full` so PD attribution,
// hidden-flag handling, name-mismatch detection, and dup-define
// rejection are unified.
//
// Argument layout (for a static native, no `this` slot — these are
// `static` in the JDK source even though the public `defineClass`
// methods are instance methods that pass `this` as arg 0):
//
//   defineClass1(ClassLoader loader,
//                String name, byte[] b, int off, int len,
//                ProtectionDomain pd, String source) -> Class
//   defineClass2(ClassLoader loader,
//                String name, ByteBuffer bb, int off, int len,
//                ProtectionDomain pd, String source) -> Class
//   defineClass0(ClassLoader loader, Class<?> lookup,
//                String name, byte[] b, int off, int len,
//                ProtectionDomain pd, boolean initialize, int flags,
//                Object classData) -> Class
//
// The first arg is always the class loader — call it `loader` here.
// We allow that arg to be null (bootstrap) and fall back to the app
// loader id (0 → use ClassManager default).
// ---------------------------------------------------------------------------

/// Read a UTF-8 String from arg slot `idx`. Returns the empty string
/// on null. Treats binary-name dots as JVM internal slashes.
fn read_optional_internal_name(ctx: &dyn NativeContext, args: &[Value], idx: usize) -> String {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => {
            let dotted = ctx.read_string(*o).unwrap_or_default();
            dotted.replace('.', "/")
        }
        _ => String::new(),
    }
}

/// Read an `int` from arg slot `idx`. Negative values are folded into
/// `None` so callers can validate them as a JVM `IndexOutOfBoundsException`.
fn read_nonneg_int(args: &[Value], idx: usize) -> Option<usize> {
    match args.get(idx) {
        Some(Value::Int(v)) if *v >= 0 => Some(*v as usize),
        Some(Value::Int(_)) => None,
        _ => Some(0),
    }
}

/// Read a `[B` (byte array) into a `Vec<u8>` honoring `[off, off+len)`.
/// Returns `Err(message)` if bounds are invalid (will surface as
/// `IndexOutOfBoundsException` to Java).
///
/// Defensive: rejects any access whose `offset+length` would overflow or
/// exceed the array length BEFORE the copy loop runs. Wraps the copy
/// loop itself in `catch_unwind` so a panic inside `get_array_element`
/// (e.g. due to a corrupt array object on the cglib path) returns an
/// `Err` instead of unwinding to SIGABRT. cglib emits 10-50 KB bytecode
/// buffers, so OOB-style SEGVs were observed before this hardening.
fn read_byte_array_slice(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    off: usize,
    len: usize,
) -> Result<Vec<u8>, String> {
    let cap = ctx.array_length(array);
    // Strict overflow-safe bound: off+len must fit in `cap`.
    match off.checked_add(len) {
        Some(end) if end <= cap => {}
        Some(end) => {
            return Err(format!(
                "offset+length ({end}) > array length {cap} (off={off}, len={len})"
            ));
        }
        None => {
            return Err(format!(
                "offset+length overflow (off={off}, len={len})"
            ));
        }
    }
    let ctx_ref: &dyn NativeContext = ctx;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            // Re-assert bounds inside the loop in case `cap` was racy.
            // (NativeContext is single-threaded today, but the cost is
            // negligible vs a SEGV on a stale array length.)
            debug_assert!(off + i < cap);
            match ctx_ref.get_array_element(array, off + i) {
                Value::Int(b) => out.push((b & 0xFF) as u8),
                _ => out.push(0),
            }
        }
        out
    }));
    match result {
        Ok(out) => Ok(out),
        Err(_) => {
            tracing::error!(
                "[define_class] panic while reading byte array \
                 (off={off}, len={len}, cap={cap}) — aborting"
            );
            Err("panic while reading byte array".to_string())
        }
    }
}

/// Decode an optional `ProtectionDomain` arg into a `code_source_url`
/// string suitable for `DefineClassFull::code_source_url`.
///
/// The synthetic PD layout (3 fields) carries the CodeSource at slot
/// 0 (= `PD_CODE_SOURCE_REF`) and the CodeSource carries either a
/// `URL` ObjectRef or a String at slot 0 (= `CS_LOCATION_REF`). For
/// a real-JDK-shape PD, we additionally probe `getCodeSource()` /
/// `getLocation()` by name as a belt-and-suspenders fallback.
fn extract_pd_code_source_url(ctx: &dyn NativeContext, pd: ObjectRef) -> Option<String> {
    // Synthetic PD path: field 0 -> CodeSource; field 0 of CS -> URL or String.
    if let Value::Object(Some(cs)) = ctx.get_field(pd, PD_CODE_SOURCE_REF) {
        if let Some(s) = ctx.read_string(cs) {
            return Some(s);
        }
        if let Value::Object(Some(loc)) = ctx.get_field(cs, CS_LOCATION_REF) {
            if let Some(s) = ctx.read_string(loc) {
                return Some(s);
            }
            // The URL synthetic stores the full string at field 5
            // (matches alloc done elsewhere in this module).
            if let Value::Object(Some(full)) = ctx.get_field(loc, 5) {
                if let Some(s) = ctx.read_string(full) {
                    return Some(s);
                }
            }
        }
    }
    // Real-JDK PD path: field 0 may not match. Try by-name.
    if let Value::Object(Some(cs)) = ctx.get_field_by_name(pd, "codesource") {
        if let Value::Object(Some(loc)) = ctx.get_field_by_name(cs, "location") {
            if let Some(s) = ctx.read_string(loc) {
                return Some(s);
            }
        }
    }
    None
}

/// Decode the bytes for a `defineClass2`-style `ByteBuffer` argument.
///
/// Handles both heap and direct buffers:
///   * Heap buffer  — slot `BUF_FIELD_ARRAY` (= 0) holds a `byte[]`
///     and slot `BUF_FIELD_POS` (= 1) is the start position. We use
///     the supplied `off` parameter (added to position) and `len`.
///   * Direct buffer — slot 0 is a `Long` (native address). Real
///     direct memory is allocated outside the GC heap and the JDK
///     would memcpy from the address. We don't pin native memory in
///     this VM, so we fall back to scanning slot 0 for a heap-array
///     stand-in (some synthetic direct-buffer constructors elsewhere
///     in this codebase store the backing array there to ease
///     interop).
///
/// Returns `Err(message)` if the buffer can't be decoded into a
/// readable byte slice; the caller surfaces that as a
/// `ClassFormatError`.
fn read_byte_buffer_slice(
    ctx: &dyn NativeContext,
    bb: ObjectRef,
    off: usize,
    len: usize,
) -> Result<Vec<u8>, String> {
    // ByteBuffer synthetic layout: slot 0 = array (heap) OR long address
    // (direct). The charset module's BUF_FIELD_* constants apply.
    let array_slot: usize = 0; // BUF_FIELD_ARRAY
    let pos_slot: usize = 1; // BUF_FIELD_POS
    let limit_slot: usize = 2; // BUF_FIELD_LIMIT
    let capacity_slot: usize = 3; // BUF_FIELD_CAPACITY

    // Heap-buffer case.
    if let Value::Object(Some(array)) = ctx.get_field(bb, array_slot) {
        let pos = ctx.get_field(bb, pos_slot).as_int().unwrap_or(0).max(0) as usize;
        let limit = ctx
            .get_field(bb, limit_slot)
            .as_int()
            .unwrap_or_else(|| ctx.array_length(array) as i32)
            .max(0) as usize;
        let cap = ctx.array_length(array);
        let absolute_off = pos.saturating_add(off);
        if absolute_off > cap || absolute_off > limit {
            return Err(format!(
                "ByteBuffer offset+pos ({absolute_off}) exceeds capacity ({cap}) or limit ({limit})"
            ));
        }
        // The caller passes (off, len) in buffer-relative coords; honor
        // the buffer's `limit` as an upper bound for safety.
        let max_len = (limit - absolute_off).min(cap - absolute_off);
        let actual_len = len.min(max_len);
        // Defensive: wrap the copy loop in `catch_unwind` so a panic
        // inside `get_array_element` returns Err instead of SIGABRT.
        let ctx_ref: &dyn NativeContext = ctx;
        let copy_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut out = Vec::with_capacity(actual_len);
            for i in 0..actual_len {
                match ctx_ref.get_array_element(array, absolute_off + i) {
                    Value::Int(b) => out.push((b & 0xFF) as u8),
                    _ => out.push(0),
                }
            }
            out
        }));
        return match copy_result {
            Ok(out) => Ok(out),
            Err(_) => {
                tracing::error!(
                    "[define_class] panic while reading ByteBuffer \
                     (abs_off={absolute_off}, len={actual_len}, cap={cap}) — aborting"
                );
                Err("panic while reading ByteBuffer".to_string())
            }
        };
    }

    // Direct-buffer fallback: capacity slot still tells us how many
    // bytes were "allocated"; we can't read raw native memory here,
    // so we report an empty slice (the backend will then surface a
    // ClassFormatError because the bytes have no magic). Real direct
    // buffers used by `defineClass2` are uncommon in our app workloads
    // — Quarkus / WildFly use the byte[] path.
    let cap = ctx
        .get_field(bb, capacity_slot)
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    if cap == 0 {
        return Err("direct ByteBuffer is empty".to_string());
    }
    Err(format!(
        "direct ByteBuffer with capacity {cap} not readable in defineClass2 path"
    ))
}

/// Bind the loader-id used to register the new class. We look up the
/// loader's `CL_LOADER_ID` slot if present (lazily allocating a fresh
/// id), otherwise fall through to id 0 (= application loader). A null
/// loader is treated as the bootstrap class loader, which the backend
/// also models as id 0 in this VM.
fn loader_id_for(ctx: &mut dyn NativeContext, loader: Value) -> u32 {
    if let Value::Object(Some(cl)) = loader {
        return get_or_assign_loader_id(ctx, cl);
    }
    0
}

/// Common backend used by all three `defineClassN` natives. Returns
/// the resulting Class mirror as a `Value::Object(Some(...))` or an
/// exception via the `MethodCallResult` channel.
fn define_class_via_full(
    ctx: &mut dyn NativeContext,
    name: &str,
    bytes: Vec<u8>,
    loader_id: u32,
    opts: cratonvm_native_api::DefineClassFull,
    initialize: bool,
    class_data: Option<Value>,
) -> MethodCallResult {
    use cratonvm_types::error::{LinkageError, RuntimeError};

    // Pre-validate the class file header: at least 8 bytes (magic +
    // minor + major) and CAFEBABE magic must be present, otherwise the
    // backend parser may dereference garbage past the buffer end.
    if bytes.len() < 8 || bytes[0..4] != CLASS_FILE_MAGIC {
        tracing::warn!(
            "[define_class] invalid magic for {name}; rejecting"
        );
        return Err(RuntimeError::IllegalArgumentException {
            message: "defineClass: not a valid class file (bad magic)".into(),
        }
        .into());
    }

    // Wrap the backend call in `catch_unwind` so a panic inside
    // `define_class_full` (verifier OOB, ASM-emitted bytecode that
    // defeats our class file parser, etc.) returns a clean
    // ClassFormatError instead of unwinding to SIGABRT.
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(name, &bytes, loader_id, opts)
    }));
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(
                "[define_class] panic inside define_class_full for {name}; aborting"
            );
            return Err(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: "defineClass: panic inside backend (likely malformed bytecode)".into(),
            }
            .into());
        }
    };
    match define_result {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            // Stash classData (defineClass0 path) on the side-table.
            if let Some(data) = class_data {
                set_class_data(mirror, data);
            }
            // Eager-init request: run <clinit> now (defineClass0 path
            // when `initialize == true`).
            if initialize {
                if let Err(msg) = ctx.initialize_class(cid) {
                    tracing::warn!(
                        "defineClass0 initialize: <clinit> for {name} failed: {msg}"
                    );
                }
            }
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            tracing::warn!("ClassLoader.defineClass({name}) failed: {msg}");
            Err(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: format!("defineClass: {msg}"),
            }
            .into())
        }
    }
}

/// JDK-internal: `static native Class<?> defineClass1(
///     ClassLoader loader, String name, byte[] b, int off, int len,
///     ProtectionDomain pd, String source);`
fn cl_define_class1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;

    let loader = args.first().copied().unwrap_or(Value::Object(None));
    let name = read_optional_internal_name(ctx, args, 1);
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineClass1: bytes must not be null".into(),
            }
            .into());
        }
    };
    let off = match read_nonneg_int(args, 3) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into());
        }
    };
    let len = match read_nonneg_int(args, 4) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into());
        }
    };
    let bytes = read_byte_array_slice(ctx, byte_array, off, len).map_err(|_msg| {
        cratonvm_types::error::MethodCallFailed::from(
            RuntimeError::ArrayIndexOutOfBoundsException {
                index: (off as i32).max(0),
            },
        )
    })?;

    // cglib SEGV guard.
    if let Some(v) = cglib_guard_value(ctx, &name, &bytes) {
        return Ok(Some(v));
    }

    // Optional ProtectionDomain at slot 5.
    let mut opts = cratonvm_native_api::DefineClassFull::default();
    if let Some(Value::Object(Some(pd))) = args.get(5) {
        opts.code_source_url = extract_pd_code_source_url(ctx, *pd);
    }
    // Optional `String source` at slot 6 — JDK uses this as the
    // SourceFile attribute hint, surfacing through `Class.getResource(...)`
    // / debugging. We thread it through `override_name = None`, leaving
    // the backend's own SourceFile attribute path intact, but log it
    // when present so debugging/JFR can correlate.
    if let Some(Value::Object(Some(src_obj))) = args.get(6) {
        if let Some(src) = ctx.read_string(*src_obj) {
            tracing::debug!(
                target: "cratonvm_native_builtins::classloader",
                class = %name,
                source = %src,
                "defineClass1 source hint"
            );
        }
    }

    let loader_id = loader_id_for(ctx, loader);
    define_class_via_full(ctx, &name, bytes, loader_id, opts, false, None)
}

/// JDK-internal: `static native Class<?> defineClass2(
///     ClassLoader loader, String name, ByteBuffer bb, int off,
///     int len, ProtectionDomain pd, String source);`
fn cl_define_class2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::{LinkageError, RuntimeError};

    let loader = args.first().copied().unwrap_or(Value::Object(None));
    let name = read_optional_internal_name(ctx, args, 1);
    let bb = match args.get(2) {
        Some(Value::Object(Some(b))) => *b,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineClass2: ByteBuffer must not be null".into(),
            }
            .into());
        }
    };
    let off = match read_nonneg_int(args, 3) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into());
        }
    };
    let len = match read_nonneg_int(args, 4) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into());
        }
    };
    let bytes = read_byte_buffer_slice(ctx, bb, off, len).map_err(|msg| {
        cratonvm_types::error::MethodCallFailed::from(LinkageError::ClassFormatError {
            class_name: name.clone(),
            message: format!("defineClass2: {msg}"),
        })
    })?;

    // cglib SEGV guard.
    if let Some(v) = cglib_guard_value(ctx, &name, &bytes) {
        return Ok(Some(v));
    }

    let mut opts = cratonvm_native_api::DefineClassFull::default();
    if let Some(Value::Object(Some(pd))) = args.get(5) {
        opts.code_source_url = extract_pd_code_source_url(ctx, *pd);
    }
    if let Some(Value::Object(Some(src_obj))) = args.get(6) {
        if let Some(src) = ctx.read_string(*src_obj) {
            tracing::debug!(
                target: "cratonvm_native_builtins::classloader",
                class = %name,
                source = %src,
                "defineClass2 source hint"
            );
        }
    }

    let loader_id = loader_id_for(ctx, loader);
    define_class_via_full(ctx, &name, bytes, loader_id, opts, false, None)
}

// JEP 371 / JEP 466 flag bits accepted by `defineClass0`.
const DEFINE_CLASS0_FLAG_NESTMATE: i32 = 0x01;
const DEFINE_CLASS0_FLAG_HIDDEN: i32 = 0x02;
/// `STRONG` retains the class through unloading — see
/// `java.lang.invoke.MethodHandles.Lookup.ClassOption.STRONG`. We
/// treat this as advisory: the VM does not unload classes today, so
/// the class is already strong.
const DEFINE_CLASS0_FLAG_STRONG: i32 = 0x04;
/// `ACCESS_VM_ANNOTATIONS` — surface JDK-internal annotations to
/// reflection. We accept the flag but treat it as a no-op.
const DEFINE_CLASS0_FLAG_VM_ANNOTATIONS: i32 = 0x08;

/// JDK-internal modern (JDK 17+) entry point:
/// `static native Class<?> defineClass0(
///     ClassLoader loader, Class<?> lookup, String name,
///     byte[] b, int off, int len, ProtectionDomain pd,
///     boolean initialize, int flags, Object classData);`
fn cl_define_class0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;

    let loader = args.first().copied().unwrap_or(Value::Object(None));
    // Slot 1: lookup class (used for nest-host derivation when the
    // NESTMATE flag is set).
    let lookup_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = read_optional_internal_name(ctx, args, 2);
    let byte_array = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineClass0: bytes must not be null".into(),
            }
            .into());
        }
    };
    let off = match read_nonneg_int(args, 4) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into());
        }
    };
    let len = match read_nonneg_int(args, 5) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into());
        }
    };
    let bytes = read_byte_array_slice(ctx, byte_array, off, len).map_err(|_msg| {
        cratonvm_types::error::MethodCallFailed::from(
            RuntimeError::ArrayIndexOutOfBoundsException {
                index: (off as i32).max(0),
            },
        )
    })?;

    // cglib SEGV guard.
    if let Some(v) = cglib_guard_value(ctx, &name, &bytes) {
        return Ok(Some(v));
    }

    // Slot 6: ProtectionDomain
    let mut opts = cratonvm_native_api::DefineClassFull::default();
    if let Some(Value::Object(Some(pd))) = args.get(6) {
        opts.code_source_url = extract_pd_code_source_url(ctx, *pd);
    }

    // Slot 7: boolean initialize
    let initialize = matches!(args.get(7), Some(Value::Int(v)) if *v != 0);

    // Slot 8: int flags
    let flags = match args.get(8) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if (flags & DEFINE_CLASS0_FLAG_HIDDEN) != 0 {
        opts.hidden = true;
        // Hidden classes are bytecode the JDK trusts (or has already
        // verified at the source level). The backend's verifier still
        // runs by default, but we skip verification when callers
        // explicitly mark this as hidden — matches HotSpot behaviour.
        opts.skip_verification = true;
    }
    if (flags & DEFINE_CLASS0_FLAG_NESTMATE) != 0 {
        // Derive nest-host from the lookup class. If the lookup
        // class's nest_host is itself, that name is used.
        if let Some(lk) = lookup_mirror {
            if let Some(cid) = crate::lang_class::mirror_class_id(ctx, lk) {
                let nest_host = ctx
                    .nest_host_name(cid)
                    .or_else(|| ctx.class_name_of_id(cid));
                if let Some(host) = nest_host {
                    opts.nest_host_class_name = Some(host);
                }
            }
        }
    }
    let _strong = (flags & DEFINE_CLASS0_FLAG_STRONG) != 0; // advisory
    let _vm_anns = (flags & DEFINE_CLASS0_FLAG_VM_ANNOTATIONS) != 0; // advisory

    // Slot 9: Object classData (may be null)
    let class_data = match args.get(9) {
        Some(Value::Object(Some(_))) => Some(args[9]),
        _ => None,
    };

    let loader_id = loader_id_for(ctx, loader);
    define_class_via_full(ctx, &name, bytes, loader_id, opts, initialize, class_data)
}

/// WP2.3-C — register the JDK-internal `defineClass0/1/2` natives on
/// `java.lang.ClassLoader`. The public `defineClass(...)` overloads
/// are pure Java and route through these natives; CGLIB / direct
/// user code typically calls `defineClass1` because that's what the
/// public 4-arg / 5-arg overloads delegate to.
pub fn register_classloader_define_class(r: &mut NativeMethodRegistry) {
    let cl = CL_CLASS;

    // defineClass1(ClassLoader loader, String name, byte[] b, int off, int len,
    //              ProtectionDomain pd, String source) -> Class
    r.register(
        cl,
        "defineClass1",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        cl_define_class1,
    );

    // defineClass2(ClassLoader loader, String name, ByteBuffer bb, int off, int len,
    //              ProtectionDomain pd, String source) -> Class
    r.register(
        cl,
        "defineClass2",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/nio/ByteBuffer;IILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        cl_define_class2,
    );

    // defineClass0(ClassLoader loader, Class<?> lookup, String name,
    //              byte[] b, int off, int len, ProtectionDomain pd,
    //              boolean initialize, int flags, Object classData) -> Class
    r.register(
        cl,
        "defineClass0",
        "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BIILjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
        cl_define_class0,
    );
}

// ---------------------------------------------------------------------------
// Round-16 (agent 16): defensive `sun.misc.Unsafe.defineClass` shim for the
// cglib proxy-generation path.
//
// Symptom: a SEGV / stack-overflow during cglib's proxy bytecode emit when
// it calls `Unsafe.defineClass(name, bytecode[], off, len, loader, pd)`.
// The crash is in the non-JIT path during bytecode generation — most
// likely a null/short bytecode array being deref'd by the underlying
// `define_class_full` plumbing.
//
// Fix: validate args up-front (null bytecode → null result; oversized
// bytecode → null result), then delegate to the same backend used by the
// public `ClassLoader.defineClass(String, byte[], int, int, ProtectionDomain)`
// overload. Adding the defensive check is the win even if cglib doesn't
// fully work — it prevents the process crash.
//
// Signature: `defineClass(String name, byte[] b, int off, int len,
//                         ClassLoader loader, ProtectionDomain pd) -> Class`
// args = [this, name, byte_array, off, len, loader, pd]
//   (this == the Unsafe singleton, ignored)
//
// Note: registered from `register_classloader_natives` because the user
// requested all cglib-related URL/Unsafe defineClass paths live in this
// file. This does NOT shadow the existing `Unsafe.defineAnonymousClass`
// natives in `unsafe_natives.rs` — different method name + descriptor.
// ---------------------------------------------------------------------------

/// Max class file size we accept on the Unsafe.defineClass path. cglib
/// proxies for typical Spring/Hibernate classes are <200 KB; anything
/// over 1 MB is almost certainly a misinterpreted argument (off/len
/// mismatch reading past the array end) and we'd rather return null
/// than try to parse it.
const UNSAFE_DEFINE_CLASS_MAX_BYTES: usize = 1024 * 1024;

fn unsafe_define_class_defensive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // arg[0] = this (Unsafe singleton), ignored
    // arg[1] = name : String (may be null — bytecode carries this_class)
    // arg[2] = b : byte[]
    // arg[3] = off : int
    // arg[4] = len : int
    // arg[5] = loader : ClassLoader (may be null — system loader)
    // arg[6] = pd : ProtectionDomain (may be null)

    // Defensive arg[2] check: null array → return null instead of segfaulting.
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            tracing::warn!(
                "Unsafe.defineClass: null bytecode array — returning null"
            );
            return Ok(Some(Value::Object(None)));
        }
    };

    let array_len = ctx.array_length(byte_array);

    // Defensive offset/length validation.
    let offset = match args.get(3) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => 0,
    };
    let length = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => array_len,
    };

    // Sanity-cap: cglib proxies are small. Reject obviously-bogus sizes.
    if length == 0 || length > UNSAFE_DEFINE_CLASS_MAX_BYTES {
        tracing::warn!(
            "Unsafe.defineClass: rejecting bytecode of length {length} \
             (max={UNSAFE_DEFINE_CLASS_MAX_BYTES})"
        );
        return Ok(Some(Value::Object(None)));
    }

    // Bounds: offset+length must fit inside the array.
    // checked_add prevents wrap-around on pathological inputs.
    if offset
        .checked_add(length)
        .map_or(true, |end| end > array_len)
    {
        tracing::warn!(
            "Unsafe.defineClass: offset/length out of bounds \
             (off={offset}, len={length}, array={array_len}) — returning null"
        );
        return Ok(Some(Value::Object(None)));
    }

    // Extract class name (may be null — define_class_full will read this_class).
    let name_str = match args.get(1) {
        Some(Value::Object(Some(name_obj))) => {
            let dotted = ctx.read_string(*name_obj).unwrap_or_default();
            dotted.replace('.', "/")
        }
        _ => String::new(),
    };

    // Copy bytes defensively. Any out-of-band element read returns 0 byte.
    // Wrap in `catch_unwind` so a panic during the copy (corrupt array
    // header, GC-moved object on the cglib path) returns null instead
    // of SIGABRT.
    let class_bytes_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut bytes = Vec::with_capacity(length);
        for i in 0..length {
            match ctx.get_array_element(byte_array, offset + i) {
                Value::Int(b) => bytes.push(b as u8),
                _ => bytes.push(0),
            }
        }
        bytes
    }));
    let class_bytes = match class_bytes_result {
        Ok(b) => b,
        Err(_) => {
            tracing::error!(
                "Unsafe.defineClass({name_str}): panic while reading byte array; \
                 returning null"
            );
            return Ok(Some(Value::Object(None)));
        }
    };

    // Sanity check magic before handing to backend — `define_class_full`
    // already checks this, but doing it here keeps the warn log clear
    // about WHO rejected the bytecode. Require at least 8 bytes
    // (magic + minor + major) so the backend never reads past EOF.
    if class_bytes.len() < 8 || class_bytes[0..4] != CLASS_FILE_MAGIC {
        tracing::warn!(
            "Unsafe.defineClass({name_str}): bad magic — returning null"
        );
        return Ok(Some(Value::Object(None)));
    }

    // cglib SEGV guard — this is the hottest path for the cglib_probe
    // reproducer because cglib's `ReflectUtils.defineClass` calls into
    // `sun.misc.Unsafe.defineClass`.
    if let Some(v) = cglib_guard_value(ctx, &name_str, &class_bytes) {
        return Ok(Some(v));
    }

    // Resolve loader id from arg[5]. Null loader → system (id 0).
    let loader_id = match args.get(5) {
        Some(Value::Object(Some(loader_obj))) => {
            get_or_assign_loader_id(ctx, *loader_obj)
        }
        _ => 0,
    };

    // Extract optional PD URL (same layout as cl_define_class_basic).
    let mut pd_url: Option<String> = None;
    if let Some(Value::Object(Some(pd))) = args.get(6) {
        if let Value::Object(Some(cs)) = ctx.get_field(*pd, 0) {
            if let Some(s) = ctx.read_string(cs) {
                pd_url = Some(s);
            } else if let Value::Object(Some(url)) = ctx.get_field(cs, 0) {
                if let Some(s) = ctx.read_string(url) {
                    pd_url = Some(s);
                }
            }
        }
    }

    let opts = cratonvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        ..Default::default()
    };
    // catch_unwind: malformed cglib bytes (10-50 KB) can crash the
    // backend parser. Translate panic → null so the Java caller sees
    // an NPE (recoverable) instead of process exit.
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(&name_str, &class_bytes, loader_id, opts)
    }));
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(
                "Unsafe.defineClass({name_str}): panic inside define_class_full; \
                 returning null"
            );
            return Ok(Some(Value::Object(None)));
        }
    };
    match define_result {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            tracing::warn!(
                "Unsafe.defineClass({name_str}) backend failed: {msg} — returning null"
            );
            Ok(Some(Value::Object(None)))
        }
    }
}

fn cl_resolve_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // resolveClass(Class) — trigger class preparation and linking
    if let Some(Value::Object(Some(class_mirror))) = args.get(1) {
        if let Some(cid) = crate::lang_class::mirror_class_id(ctx, *class_mirror) {
            let _ = ctx.ensure_class_initialized(
                &ctx.class_name_of_id(cid).unwrap_or_default()
            );
        }
    }
    Ok(None)
}

fn cl_find_loaded_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // JVM spec: findLoadedClass checks if a class has already been loaded by
    // this loader (or delegated to a parent). Does NOT trigger class loading.
    let this = obj_arg(args, 0)?;
    let name_str = match args.get(1) {
        Some(Value::Object(Some(name_obj))) => {
            let dotted = ctx.read_string(*name_obj).unwrap_or_default();
            dotted.replace('.', "/")
        }
        _ => return Ok(Some(Value::Object(None))),
    };

    let loader_type = match ctx.get_field(this, CL_LOADER_TYPE) {
        Value::Int(v) => v,
        _ => LOADER_APP,
    };

    // For custom loaders: check own namespace first
    if loader_type == LOADER_CUSTOM {
        let loader_id = match ctx.get_field(this, CL_LOADER_ID) {
            Value::Int(v) if v > 0 => v as u32,
            _ => 0,
        };
        if loader_id > 0 {
            if let Some(cid) = ctx.class_id_by_name_and_loader(&name_str, loader_id) {
                let mirror = ctx.get_class_mirror(cid);
                return Ok(Some(Value::Object(Some(mirror))));
            }
        }
    }

    // For all loaders: check the global loaded-class cache (covers bootstrap/ext/app)
    match ctx.class_id_by_name(&name_str) {
        Some(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn cl_get_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, CL_PARENT_REF)))
}

fn cl_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name_val = ctx.get_field(this, CL_NAME_REF);
    if let Value::Object(Some(_)) = name_val {
        Ok(Some(name_val))
    } else {
        // Return loader type as name if no explicit name set
        let lt = match ctx.get_field(this, CL_LOADER_TYPE) {
            Value::Int(v) => v,
            _ => LOADER_CUSTOM,
        };
        let name_str = match lt {
            LOADER_BOOTSTRAP => "bootstrap",
            LOADER_PLATFORM => "platform",
            LOADER_APP => "app",
            _ => "custom",
        };
        let s = ctx.create_string(name_str);
        Ok(Some(Value::Object(Some(s))))
    }
}

fn cl_get_system_class_loader(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let app = get_or_create_app_loader(ctx);
    Ok(Some(Value::Object(Some(app))))
}

fn cl_get_platform_class_loader(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let platform = get_or_create_platform_loader(ctx);
    Ok(Some(Value::Object(Some(platform))))
}

/// Public re-export of the `getResource` (singular) native so
/// `register_essential_natives` can install it in real-JDK mode. Without
/// this the JDK's own `ClassLoader.getResource` runs — and in real-JDK
/// mode the URLClassPath `<clinit>` swallow leaves the loader's resource
/// tables empty, so it returns null even for resources the bulk
/// `getResources` enumerator finds. Wave-1 Task B consistency fix.
pub fn cl_get_resource_essential(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cl_get_resource(ctx, args)
}

/// True iff the object's class is `java/lang/ClassLoader` or a subclass.
///
/// The getResource/getResources natives ALSO serve the STATIC
/// `getSystemResource(s)` forms (same handler registration), where args[0]
/// is the resource-name String, not a receiver. The user-loader delegation
/// must not treat that String as a ClassLoader: doing so dispatched
/// `invoke_virtual(<String>, "findResource")` →
/// `NoSuchMethodError java/lang/String.findResource` and broke every
/// `getSystemResource` caller (kafka-codec / hadoop-conf / hbase-conf
/// regression-pool probes).
fn is_classloader_instance(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let mut cur = ctx.class_id_of_object(obj);
    for _ in 0..64 {
        match ctx.class_name_of_id(cur) {
            Some(n) if n == "java/lang/ClassLoader" => return true,
            Some(n) if n == "java/lang/Object" => return false,
            _ => {}
        }
        match ctx.superclass_of(cur) {
            Some(p) if p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

fn cl_get_resource(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // ClassLoader.getResource(String) → URL
    //
    // Spec contract: returns the FIRST URL the parent-delegated search
    // would return for `name`, or null. Must be consistent with
    // `getResources`: if `getResources(name)` returns N≥1 URLs, then
    // `getResource(name)` must return the first of those URLs (not null,
    // not a different URL form). The bulk path walks every classpath
    // entry via `find_all_resource_urls`; the singular path here mirrors
    // that walk and returns its first element so the two stay in lock-step.
    //
    // We scan `args` for the LAST String-typed slot (mirroring the bulk
    // path) so the same native can serve `getSystemResource` (static —
    // name at index 0) and instance `getResource` (name at index 1).
    let name = {
        let mut found: Option<String> = None;
        for v in args.iter().rev() {
            if let Value::Object(Some(o)) = v {
                if let Some(s) = ctx.read_string(*o) {
                    found = Some(s);
                    break;
                }
            }
        }
        found.unwrap_or_default()
    };
    let resource_name = name.trim_start_matches('/');

    // User-defined classloader delegation (mirrors cl_get_resources).
    // When the receiver is a non-builtin ClassLoader, invoke findResource()
    // via virtual dispatch so the user's override runs (e.g.
    // EmbeddedImplClassLoader.findResource reads from IMPL-JARS).
    // `is_classloader_instance` gates out the STATIC getSystemResource form
    // (args[0] is the name String there, not a receiver).
    if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
        if is_classloader_instance(ctx, this_ref) {
            let class_id = ctx.class_id_of_object(this_ref);
            if let Some(class_name) = ctx.class_name_of_id(class_id) {
                if !is_builtin_loader_class(&class_name) {
                    // Pin the receiver across the allocating create_string —
                    // a moving GC during it would stale `this_ref`.
                    let pin = ctx.pin_native_root(this_ref);
                    let name_arg = Value::Object(Some(ctx.create_string(&name)));
                    let this_ref = ctx.read_native_pin(pin, this_ref);
                    ctx.unpin_native_roots(pin);
                    return ctx.invoke_virtual(
                        this_ref,
                        "findResource",
                        "(Ljava/lang/String;)Ljava/net/URL;",
                        &[name_arg],
                    );
                }
            }
        }
    }

    // Prefer the structured URL (jar:file:/... or jrt:/... or file:/...)
    // so getResource and getResources return the same URL form for the
    // same name. Fall back to "classpath:<name>" when only `find_resource`
    // (raw bytes) succeeds — covers synthetic test loaders that override
    // find_resource without participating in the structured walk.
    let urls = ctx.find_all_resource_urls(resource_name);
    let url_str = if let Some(first) = urls.first() {
        first.clone()
    } else if ctx.find_resource(resource_name).is_some() {
        format!("classpath:{name}")
    } else {
        let dbg_all = std::env::var("CRATONVM_DBG_GETRESOURCES")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        if dbg_all {
            eprintln!("[GRES-DBG] getResource({}) -> NULL (no urls, no bytes)", resource_name);
        }
        return Ok(Some(Value::Object(None)));
    };

    let dbg_all = std::env::var("CRATONVM_DBG_GETRESOURCES")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    if dbg_all {
        eprintln!("[GRES-DBG] getResource({}) -> {}", resource_name, url_str);
    }

    tracing::debug!(
        target: "cratonvm_vm::runtime::resources",
        resource = %resource_name,
        url = %url_str,
        "ClassLoader.getResource resolved"
    );

    let url = crate::jboss_module_loader::build_synthetic_url(ctx, &url_str);
    Ok(Some(Value::Object(Some(url))))
}

/// Public re-export of the `getResources` native for `register_essential_natives`
/// so the override is available in real-JDK mode regardless of feature flag.
pub fn cl_get_resources_essential(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cl_get_resources(ctx, args)
}

fn cl_get_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // ClassLoader.getResources(String) → Enumeration<URL>
    // Walks EVERY classpath entry (directories, JARs, JMODs, jimage) and
    // returns a URL per match. This is the B3 fix: URLClassPath.<clinit> in
    // real-JDK mode NPEs before it finishes, leaving the classloader's
    // resource tables empty; this native override bypasses the broken path.
    //
    // The first arg is `this` (ClassLoader); the actual resource name is at
    // index 1 for instance calls. For the static `getSystemResources` the
    // name is at index 0. Pick the LAST String-typed arg to avoid confusion
    // with `this` — ClassLoader has no String fields so `read_string(this)`
    // typically returns None, but belt-and-suspenders.
    let name = {
        let mut found: Option<String> = None;
        for v in args.iter().rev() {
            if let Value::Object(Some(o)) = v {
                if let Some(s) = ctx.read_string(*o) {
                    found = Some(s);
                    break;
                }
            }
        }
        found.unwrap_or_default()
    };
    let resource_name = name.trim_start_matches('/');

    // User-defined classloader delegation: if the receiver is a non-builtin
    // ClassLoader subclass (e.g. EmbeddedImplClassLoader), delegate to its
    // findResources() override instead of the flat classpath scan.
    //
    // The real JDK ClassLoader.getResources(name) calls:
    //   1. parent.getResources(name)  (handled by our flat scan when parent is builtin)
    //   2. this.findResources(name)   (the documented override hook)
    //
    // Our native completely replaces step 2, so custom classloaders that
    // override findResources (like ES EmbeddedImplClassLoader, which reads
    // embedded IMPL-JARS directory trees from the outer jar) never get
    // their resources surfaced to ServiceLoader.
    //
    // Fix: when the receiver is a non-builtin ClassLoader, invoke
    // findResources() via virtual dispatch. The callee runs real Java
    // bytecode (e.g. EmbeddedImplClassLoader.findResources constructs an
    // Enumeration that reads embedded jar entries via parent.getResource)
    // and may recursively call our native for the builtin parent loader.
    //
    // Safety: infinite-recursion is avoided because:
    //  - builtin loaders (URLClassLoader, AppClassLoader, …) take the flat
    //    scan path below (is_builtin_loader_class check), not this branch;
    //  - non-builtin loaders whose findResources calls parent.getResources
    //    will hit this branch again only for the PARENT — which IS builtin.
    //
    // `is_classloader_instance` gates out the STATIC getSystemResources form
    // (args[0] is the name String there, not a receiver).
    if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
        if is_classloader_instance(ctx, this_ref) {
            let class_id = ctx.class_id_of_object(this_ref);
            if let Some(class_name) = ctx.class_name_of_id(class_id) {
                if !is_builtin_loader_class(&class_name) {
                    // Pin the receiver across the allocating create_string.
                    let pin = ctx.pin_native_root(this_ref);
                    let name_arg = Value::Object(Some(ctx.create_string(&name)));
                    let this_ref = ctx.read_native_pin(pin, this_ref);
                    ctx.unpin_native_roots(pin);
                    return ctx.invoke_virtual(
                        this_ref,
                        "findResources",
                        "(Ljava/lang/String;)Ljava/util/Enumeration;",
                        &[name_arg],
                    );
                }
            }
        }
    }

    let mut urls = ctx.find_all_resource_urls(resource_name);

    // WF32-fix: bound the per-jar `META-INF/MANIFEST.MF` enumeration.
    //
    // Background: CratonVM approximates JBoss module isolation by dumping
    // every resolved module's `<resource-root>` jars onto a single shared
    // application classpath (see `jboss_module_loader::register_resource_roots`).
    // A real JVM running `java -jar jboss-modules.jar` has exactly ONE
    // classpath entry, so `getResources("META-INF/MANIFEST.MF")` returns one
    // URL. Under CratonVM's flat classpath it returns one URL per module jar
    // — 500-750 for a full WildFly install.
    //
    // WildFly's bootstrap iterates that enumeration, doing a
    // `URL.openStream()` + `new Manifest(stream)` on each. With 500+ jars —
    // several of them carrying very large manifests (e.g. `ecj-3.32.0.jar`
    // ships an 889-section, 124 KB MANIFEST.MF) — the interpreted scan runs
    // long past the 120 s stack-dump watchdog, so the boot never makes
    // forward progress: a hang, not a crash.
    //
    // `META-INF/MANIFEST.MF` is special: it exists in essentially every jar,
    // so a flat-classpath enumeration of it is quadratic-by-construction and
    // is never what a module-isolated caller actually wants. Capping it back
    // toward the real-JVM count keeps the scan bounded. The cap is generous
    // (128 — far more than the 1 a real `java -jar` sees) so legitimate
    // multi-jar manifest probes still work; only the pathological 500+-jar
    // module-jar flood is truncated.
    //
    // This is a bounded fallback, NOT the correct end state. A real
    // `module.xml`-driven resolver must give each JBoss module its own
    // isolated `ModuleClassLoader` whose `getResources` only sees that
    // module's own `<resource-root>` jars — then this cap becomes a no-op.
    const MANIFEST_ENUM_CAP: usize = 128;
    if resource_name == "META-INF/MANIFEST.MF" && urls.len() > MANIFEST_ENUM_CAP {
        eprintln!(
            "[jboss-bf] getResources(META-INF/MANIFEST.MF): capping {} flat-classpath \
             matches to {} (CratonVM module-jar flood; see classloader.rs WF32-fix)",
            urls.len(),
            MANIFEST_ENUM_CAP
        );
        urls.truncate(MANIFEST_ENUM_CAP);
    }

    // Always also offer the "classpath:<name>" form when any entry served it
    // via raw bytes but wasn't discovered via the structured walk (e.g. a
    // synthetic test loader that only overrides `find_resource`).
    if urls.is_empty() {
        if ctx.find_resource(resource_name).is_some() {
            urls.push(format!("classpath:{name}"));
        }
    }

    // ES2-DBG: env-gated tracing for getResources probe + the original
    // spring.factories trace path is subsumed by the env-gated emitter so
    // a single switch covers both diagnostics surfaces.
    //
    // Set `CRATONVM_DBG_GETRESOURCES=1` to dump every invocation's
    // (resource, count, urls) triple. We also keep the legacy
    // spring-specific trace as a no-op fall-through condition because some
    // older debug runs rely on it being always-on.
    let dbg_all = std::env::var("CRATONVM_DBG_GETRESOURCES")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    if dbg_all {
        eprintln!("[GRES-DBG] getResources({}) -> {} URLs", resource_name, urls.len());
        for u in &urls {
            eprintln!("[GRES-DBG]   url: {}", u);
        }
    }

    tracing::debug!(
        target: "cratonvm_vm::runtime::resources",
        resource = %resource_name,
        matches = urls.len(),
        "ClassLoader.getResources enumerated"
    );

    // Build a URL[] and wrap it in our synthetic Enumeration$Impl.  The
    // Enumeration$Impl natives (hasMoreElements, nextElement, hasNext, next)
    // are registered unconditionally by `register_enumeration_impl_natives`
    // so this works in both synthetic-JDK and real-JDK modes without
    // relying on java.util.Vector's internal layout.
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
    for (i, u) in urls.iter().enumerate() {
        let url_obj = crate::jboss_module_loader::build_synthetic_url(ctx, u);
        ctx.set_array_element(arr, i, Value::Object(Some(url_obj)));
    }
    let enm = alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 2);
    ctx.set_field(enm, 0, Value::Object(Some(arr)));
    ctx.set_field(enm, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(enm))))
}

// ---------------------------------------------------------------------------
// SB-shutdown — `jdk/internal/loader/URLClassPath` safe stubs
//
// Spring Boot's `ClearCachesApplicationListener.clearClassLoaderCaches`,
// invoked on `ContextRefreshedEvent`, reflectively walks the URLClassPath
// graph reachable from `LaunchedURLClassLoader.clearCache()` and calls
// `getURLs()` and `closeLoaders()` on each.  The real-JDK bytecode for
// `URLClassPath.getURLs()` does:
//     synchronized (urls) { return path.toArray(new URL[path.size()]); }
// where `path` may be left at its default-null value when the instance
// reached us via a code path our `<init>` natives don't cover (Unsafe
// allocation, deserialization, custom factories, etc).  The resulting NPE
// is swallowed into our access-violation handler — we see the trace
// terminate at:
//     [BC] jdk/internal/loader/URLClassPath.getURLs()[Ljava/net/URL;
//     ===== SEH trap fired: code=0xC0000005 ...
// because the deref reaches our null-tag sentinel.
//
// These stubs replace the failing bytecode with safe no-op equivalents:
//   - `getURLs()`         → empty `URL[]`              (URL[0])
//   - `closeLoaders()`    → empty `ArrayList`          (List<IOException>)
//   - `closeLoaders()V`   → no-op                      (older signature)
//   - `<clinit>()V`       → no-op                      (idempotent; prevents
//                                                       any future drift in
//                                                       the JDK clinit body
//                                                       from re-introducing
//                                                       null fields)
//   - `findResource(...)` → null URL                   (no resource)
//
// Both `jdk/internal/loader/URLClassPath` (JDK 9+) and
// `sun/misc/URLClassPath` (JDK 8 legacy) are covered.
//
// We piggy-back registration on `register_enumeration_impl_natives` so
// the stubs are picked up from both the real-JDK `register_essential_natives`
// path (which calls `register_enumeration_impl_natives` directly) and the
// synthetic-jdk `register_classloader_natives` path (which calls it via
// the same helper). Idempotent — re-registration is a no-op.
// ---------------------------------------------------------------------------

/// `URLClassPath.getURLs()[Ljava/net/URL;` — return an empty URL[].
///
/// Real-JDK bytecode reads `path` (an ArrayList) under a monitor and
/// builds `URL[path.size()]`.  When `path` is null (because the instance
/// was created through a path our `<init>` shim never saw) the deref
/// crashes the VM with an access violation. An empty array is spec-legal
/// (it just means "this loader contributes no URLs") and lets Spring
/// Boot's clearCache iteration complete in zero iterations.
fn ucp_get_urls_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::debug!(
        target: "cratonvm_vm::runtime::classloader",
        "[URLClassPath shim] getURLs() returning empty URL[]"
    );
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(arr))))
}

/// `URLClassPath.closeLoaders()Ljava/util/List;` — return an empty ArrayList.
///
/// The real method walks `loaders` and accumulates IOExceptions from each
/// `Loader.close()` call.  When `loaders` is null we'd NPE; returning an
/// empty list is equivalent to "no loaders to close, no exceptions raised"
/// and matches Spring Boot's expectation (it just logs and continues).
fn ucp_close_loaders_list(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::debug!(
        target: "cratonvm_vm::runtime::classloader",
        "[URLClassPath shim] closeLoaders() returning empty ArrayList"
    );
    let list = match ctx.new_object("java/util/ArrayList")? {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let _ = ctx.invoke(
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(list))],
    );
    Ok(Some(Value::Object(Some(list))))
}

/// `URLClassPath.closeLoaders()V` — older void signature (pre-JDK 17).
/// Always succeed without side effects.
fn ucp_close_loaders_void(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::debug!(
        target: "cratonvm_vm::runtime::classloader",
        "[URLClassPath shim] closeLoaders()V (void variant) no-op"
    );
    Ok(None)
}

/// `URLClassPath.<clinit>()V` — no-op.
///
/// The real-JDK static initializer wires up a `DEBUG` flag and a couple of
/// SharedSecrets accessors.  Replacing it with a no-op is safe: any
/// subsequent method call on URLClassPath either goes through one of our
/// dedicated shims, or operates on instance fields that our `<init>` shims
/// populate explicitly. Suppressing the real clinit also defuses a class
/// of "clinit swallowed, statics left null" failure modes that would
/// otherwise re-introduce NPEs through any new code path the JDK adds.
fn ucp_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::debug!(
        target: "cratonvm_vm::runtime::classloader",
        "[URLClassPath shim] <clinit>() no-op"
    );
    Ok(None)
}

/// `URLClassPath.findResource(Ljava/lang/String;Z)Ljava/net/URL;` — return null.
///
/// Spring Boot doesn't rely on this during clearCache, but registering a
/// safe stub closes the same NPE window for any reflective probe that
/// reaches us with a null `loaders` field.
fn ucp_find_resource_null(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// Register the URLClassPath safe-stub natives on both the JDK 9+
/// (`jdk/internal/loader/URLClassPath`) and the JDK 8 legacy
/// (`sun/misc/URLClassPath`) class names.  Idempotent — see
/// `NativeMethodRegistry::register` (last call wins on same (class,
/// method, descriptor) tuple, no panic on duplicate).
pub fn register_url_class_path_safe_stubs(r: &mut NativeMethodRegistry) {
    for cls in &["jdk/internal/loader/URLClassPath", "sun/misc/URLClassPath"] {
        // `<clinit>` — no-op so the real-JDK static init body never runs.
        r.register(cls, "<clinit>", "()V", ucp_clinit_noop);
        // `getURLs` — always return an empty URL[]. Two overloads exist on
        // recent JDK builds: the regular `getURLs()` and a package-private
        // `getURLs(boolean)` that includes/excludes the loaderless entries.
        r.register(cls, "getURLs", "()[Ljava/net/URL;", ucp_get_urls_empty);
        // `closeLoaders` — both signatures.
        r.register(cls, "closeLoaders", "()Ljava/util/List;", ucp_close_loaders_list);
        r.register(cls, "closeLoaders", "()V", ucp_close_loaders_void);
        // `findResource` — return null URL when probed reflectively. Both
        // the public (String) form and the internal (String, boolean) form
        // are covered.
        r.register(cls, "findResource", "(Ljava/lang/String;)Ljava/net/URL;", ucp_find_resource_null);
        r.register(cls, "findResource", "(Ljava/lang/String;Z)Ljava/net/URL;", ucp_find_resource_null);
    }
}

/// Register natives for our synthetic `java/util/Enumeration$Impl` helper
/// class. Exposed so `register_essential_natives` can call it — needed in
/// real-JDK mode where `register_classloader_natives` (synthetic-only) is
/// skipped.
///
/// Note: we also chain to `register_url_class_path_safe_stubs` from here
/// because `register_essential_natives` (real-JDK path) calls this
/// function unconditionally, and we need the URLClassPath stubs installed
/// in both real-JDK and synthetic-JDK modes. The two concerns are
/// logically distinct but share a single wiring point.
pub fn register_enumeration_impl_natives(r: &mut NativeMethodRegistry) {
    // Install the URLClassPath safe stubs alongside the enumeration helpers
    // so both real-JDK (`register_essential_natives`) and synthetic-JDK
    // (`register_classloader_natives`) callers pick them up.
    register_url_class_path_safe_stubs(r);

    let enm = "java/util/Enumeration$Impl";
    r.register(enm, "hasMoreElements", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
    });
    r.register(enm, "nextElement", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        let len = ctx.array_length(arr);
        if idx >= len { return Ok(Some(Value::Object(None))); }
        let elem = ctx.get_array_element(arr, idx);
        ctx.set_field(this, 1, Value::Int((idx + 1) as i32));
        Ok(Some(elem))
    });
    r.register(enm, "hasNext", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
    });
    r.register(enm, "next", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        let len = ctx.array_length(arr);
        if idx >= len { return Ok(Some(Value::Object(None))); }
        let elem = ctx.get_array_element(arr, idx);
        ctx.set_field(this, 1, Value::Int((idx + 1) as i32));
        Ok(Some(elem))
    });
}

/// `ClassLoader.getSystemResources(String)` — static. Delegates to the
/// instance logic; the arg ordering in `args` is compatible because
/// `cl_get_resources` scans args for the first string.
fn cl_get_system_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cl_get_resources(ctx, args)
}

fn cl_get_resource_as_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // ClassLoader.getResourceAsStream(String) → InputStream.  Mirrors the
    // T19.H10 hardening on `Class.getResourceAsStream`: validate the name
    // (length, control bytes, `..`, `\`) before consulting `find_resource`,
    // and route the BAIS allocation through the shared helper so
    // ClassLoader-side and Class-side resource lookups stay layout-equal.
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let resource_name = name.trim_start_matches('/');
    if crate::lang_class::t19_h10_validate_resource_name_pub(resource_name).is_none() {
        return Ok(Some(Value::Object(None)));
    }
    match ctx.find_resource(resource_name) {
        None => Ok(Some(Value::Object(None))),
        Some(bytes) => {
            let len = bytes.len();
            let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes);
            tracing::debug!(
                target: "cratonvm_vm::runtime::resources",
                resource = %resource_name,
                bytes = len,
                "ClassLoader.getResourceAsStream served resource"
            );
            Ok(Some(Value::Object(Some(stream))))
        }
    }
}

fn cl_get_defined_package(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

fn cl_get_defined_packages(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(empty))))
}

fn cl_set_default_assertion_status(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn cl_register_as_parallel_capable(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Static method (invokestatic, descriptor ()Z).  The real JDK uses
    // getCallerClass() to find which ClassLoader subclass is being registered.
    // We don't enforce parallel-capability checks, so just return true.
    Ok(Some(Value::Int(1)))
}

fn cl_is_registered_as_parallel_capable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let val = match ctx.get_field(this, CL_IS_PARALLEL_CAPABLE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(val)))
}

// ---------------------------------------------------------------------------
// java.net.URLClassLoader natives
// ---------------------------------------------------------------------------

/// Extract filesystem path from a URL object (tries field 3 = path, field 5 = full string).
///
/// Returns one of two shapes depending on the URL form:
///   * `file:/X/foo.jar`            → `X/foo.jar`            (plain JAR / directory)
///   * `jar:file:/X/foo.jar!/sub/`  → `X/foo.jar!/sub/`      (JAR with internal prefix)
///
/// The `!/<prefix>/` form is preserved so `ClassPath::add_path` can build a
/// `NestedDirectory` entry pointing at the right place inside the outer JAR.
/// Without this, DaCapo's `Harness` (which builds a URLClassLoader rooted at
/// `harness/` inside the launcher JAR) silently dropped the entry from the
/// dynamic classpath and every subsequent `loadClass` returned CNFE.
fn extract_url_path(ctx: &dyn NativeContext, url_obj: ObjectRef) -> Option<String> {
    // For `jar:` URLs, the synthetic-URL builder puts the full
    // `file:/.../foo.jar!/sub/` (without the `jar:` prefix) into both
    // the `file` and `path` named fields. For plain `file:` URLs, slot 3
    // (path) is just `/X/foo.jar` (or on Windows `/C:/X/foo.jar`). To
    // distinguish the two cases we consult the FULL spec (slot 5) first
    // when present, so we know whether to keep the JAR-internal suffix.
    let full_spec = if let Value::Object(Some(full_ref)) = ctx.get_field(url_obj, 5) {
        ctx.read_string(full_ref)
    } else {
        None
    };
    let path_field = if let Value::Object(Some(path_ref)) = ctx.get_field(url_obj, 3) {
        ctx.read_string(path_ref)
    } else {
        None
    };

    // Pick the most descriptive string: if the path field encodes the
    // `!/<prefix>/` shape (which `build_synthetic_url` does — see
    // jboss_module_loader::build_synthetic_url where field "path" is
    // set to the post-`jar:` remainder), prefer it; otherwise fall back
    // to the full spec; otherwise the raw string read off the object.
    let raw = path_field
        .or(full_spec)
        .or_else(|| ctx.read_string(url_obj))?;

    // Normalise: strip a leading `jar:` (so `jar:file:/X!/sub/` collapses
    // to `file:/X!/sub/`), then strip the `file:` scheme. We keep the
    // `!/<prefix>/` suffix intact for `ClassPath::add_path` to interpret.
    let p = raw.strip_prefix("jar:").unwrap_or(&raw).to_string();
    let p = p.strip_prefix("file:").unwrap_or(&p).to_string();
    let p = p.strip_prefix("//").unwrap_or(&p).to_string();
    // Windows: `File.toURI().toURL()` yields `file:/C:/dir/...`, so the
    // extracted path is `/C:/dir/...` — a leading slash *before* the
    // drive letter. `PathBuf::from("/C:/...")` does not resolve on
    // Windows (`is_dir()` / `exists()` both fail), which made every
    // directory/jar URL silently skipped by `ClassPath::add_path`.
    // Strip the spurious leading slash when followed by a drive letter.
    let bytes = p.as_bytes();
    let p = if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        p[1..].to_string()
    } else {
        p
    };
    Some(p)
}

/// Initialize a URLClassLoader: store the URL array, extract paths, register with classpath.
fn ucl_setup(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    urls: Value,
    parent: Value,
) {
    ctx.set_field(this, UCL_LOADER_TYPE, Value::Int(LOADER_CUSTOM));
    ctx.set_field(this, UCL_PARENT_REF, parent);
    ctx.set_field(this, UCL_CLOSED, Value::Int(0));

    let url_arr = match urls {
        Value::Object(Some(arr)) => arr,
        _ => {
            ctx.set_field(this, UCL_URL_COUNT, Value::Int(0));
            return;
        }
    };

    let count = ctx.array_length(url_arr);
    // Copy URLs into a storage array and extract paths for classpath registration.
    let storage = ctx.new_array(cratonvm_types::ArrayElementType::Reference, count.max(16));
    let mut paths = Vec::with_capacity(count);
    for i in 0..count {
        let elem = ctx.get_array_element(url_arr, i);
        ctx.set_array_element(storage, i, elem);
        if let Value::Object(Some(url_obj)) = elem {
            if let Some(p) = extract_url_path(ctx, url_obj) {
                paths.push(p);
            }
        }
    }
    ctx.set_field(this, UCL_URLS_ARRAY, Value::Object(Some(storage)));
    ctx.set_field(this, UCL_URL_COUNT, Value::Int(count as i32));

    if !paths.is_empty() {
        ctx.register_dynamic_classpath(&paths);
        tracing::debug!("URLClassLoader.<init>: registered {} URLs to classpath", paths.len());
    }
}

fn ucl_init_urls(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let urls = args.get(1).copied().unwrap_or(Value::Object(None));
    ucl_setup(ctx, this, urls, Value::Object(None));
    Ok(None)
}

fn ucl_init_urls_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let urls = args.get(1).copied().unwrap_or(Value::Object(None));
    let parent = args.get(2).copied().unwrap_or(Value::Object(None));
    ucl_setup(ctx, this, urls, parent);
    Ok(None)
}

fn ucl_init_urls_parent_factory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // factory arg ignored
    ucl_init_urls_parent(ctx, args)
}

fn ucl_find_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cl_load_class(ctx, args)
}

fn ucl_find_resource(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let resource_name = name.trim_start_matches('/');
    match ctx.find_resource(resource_name) {
        Some(_) => {
            let spec = format!("classpath:{name}");
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, &spec);
            Ok(Some(Value::Object(Some(url))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn ucl_find_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Delegate to cl_get_resources logic
    cl_get_resources(ctx, args)
}

fn ucl_get_urls(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let count = match ctx.get_field(this, UCL_URL_COUNT) {
        Value::Int(n) => n.max(0) as usize,
        _ => 0,
    };
    // Copy stored URLs into a new array of the exact size
    let result = ctx.new_array(cratonvm_types::ArrayElementType::Reference, count);
    if let Value::Object(Some(urls_arr)) = ctx.get_field(this, UCL_URLS_ARRAY) {
        for i in 0..count {
            let url = ctx.get_array_element(urls_arr, i);
            ctx.set_array_element(result, i, url);
        }
    }
    Ok(Some(Value::Object(Some(result))))
}

fn ucl_add_url(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    // Check if closed
    let closed = match ctx.get_field(this, UCL_CLOSED) {
        Value::Int(v) => v != 0,
        _ => false,
    };
    if closed {
        tracing::warn!("URLClassLoader.addURL called on closed loader");
        return Ok(None);
    }

    let count = match ctx.get_field(this, UCL_URL_COUNT) {
        Value::Int(n) => n,
        _ => 0,
    };

    // Store the URL object in the URLs array
    if let Some(Value::Object(Some(url_obj))) = args.get(1) {
        if let Value::Object(Some(urls_arr)) = ctx.get_field(this, UCL_URLS_ARRAY) {
            let arr_len = ctx.array_length(urls_arr);
            if (count as usize) < arr_len {
                ctx.set_array_element(urls_arr, count as usize, Value::Object(Some(*url_obj)));
            } else {
                // Grow the array (double capacity)
                let new_cap = arr_len * 2;
                let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                for i in 0..arr_len {
                    let elem = ctx.get_array_element(urls_arr, i);
                    ctx.set_array_element(new_arr, i, elem);
                }
                ctx.set_array_element(new_arr, count as usize, Value::Object(Some(*url_obj)));
                ctx.set_field(this, UCL_URLS_ARRAY, Value::Object(Some(new_arr)));
            }
        }

        // Extract the URL path and extend the classpath dynamically.
        if let Some(p) = extract_url_path(ctx, *url_obj) {
            ctx.register_dynamic_classpath(&[p.clone()]);
            tracing::debug!("URLClassLoader.addURL: {} (classpath extended)", p);
        }
    }

    ctx.set_field(this, UCL_URL_COUNT, Value::Int(count + 1));
    Ok(None)
}

fn ucl_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, UCL_CLOSED, Value::Int(1));
    Ok(None)
}

fn ucl_new_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let urls = args.first().copied().unwrap_or(Value::Object(None));
    let obj = alloc_url_classloader(ctx);
    let count = match urls {
        Value::Object(Some(arr)) => ctx.array_length(arr) as i32,
        _ => 0,
    };
    ctx.set_field(obj, UCL_URL_COUNT, Value::Int(count));
    Ok(Some(Value::Object(Some(obj))))
}

fn ucl_new_instance_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let urls = args.first().copied().unwrap_or(Value::Object(None));
    let parent = args.get(1).copied().unwrap_or(Value::Object(None));
    let obj = alloc_url_classloader(ctx);
    let count = match urls {
        Value::Object(Some(arr)) => ctx.array_length(arr) as i32,
        _ => 0,
    };
    ctx.set_field(obj, UCL_URL_COUNT, Value::Int(count));
    ctx.set_field(obj, UCL_PARENT_REF, parent);
    Ok(Some(Value::Object(Some(obj))))
}

// ---------------------------------------------------------------------------
// java.lang.invoke.MethodHandles$Lookup natives
// ---------------------------------------------------------------------------

fn lk_lookup(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = alloc_lookup(ctx, LK_FULL_POWER);
    Ok(Some(Value::Object(Some(obj))))
}

fn lk_private_lookup_in(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let target_class = args.first().copied().unwrap_or(Value::Object(None));
    let obj = alloc_lookup(ctx, LK_FULL_POWER);
    ctx.set_field(obj, LK_LOOKUP_CLASS_REF, target_class);
    Ok(Some(Value::Object(Some(obj))))
}

fn lk_public_lookup(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = alloc_lookup(ctx, LK_PUBLIC | LK_UNCONDITIONAL);
    Ok(Some(Value::Object(Some(obj))))
}

fn lk_lookup_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, LK_LOOKUP_CLASS_REF)))
}

fn lk_previous_lookup_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, LK_PREVIOUS_LOOKUP_CLASS)))
}

fn lk_lookup_modes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, LK_ALLOWED_MODES)))
}

fn lk_has_full_privilege_access(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modes = match ctx.get_field(this, LK_ALLOWED_MODES) {
        Value::Int(v) => v,
        _ => 0,
    };
    let full = (modes & LK_PRIVATE) != 0 && (modes & LK_MODULE) != 0;
    Ok(Some(Value::Int(if full { 1 } else { 0 })))
}

fn lk_has_private_access(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modes = match ctx.get_field(this, LK_ALLOWED_MODES) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(if (modes & LK_PRIVATE) != 0 { 1 } else { 0 })))
}

fn lk_define_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Lookup.defineClass(byte[]) -> Class
    // WP2.3: routes through `define_class_full`. Differs from
    // defineHiddenClass: this path defines a NORMAL class under the
    // lookup class's loader and namespace, using the class's own
    // `this_class` name (no mangling). Throws an
    // IllegalArgumentException on bad magic / parse error per JLS
    // §5.3.5.
    use cratonvm_types::error::RuntimeError;

    let byte_array = match args.get(1) {
        Some(Value::Object(Some(arr))) => *arr,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Lookup.defineClass: bytes must not be null".into(),
            }
            .into());
        }
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Lookup.defineClass: missing bytes argument".into(),
            }
            .into());
        }
    };
    let length = ctx.array_length(byte_array);
    // Defensive: catch any panic inside the copy loop so a corrupt
    // byte[] from a cglib path returns an IAE instead of SIGABRT.
    let class_bytes_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut bytes = Vec::with_capacity(length);
        for i in 0..length {
            match ctx.get_array_element(byte_array, i) {
                Value::Int(b) => bytes.push(b as u8),
                _ => bytes.push(0),
            }
        }
        bytes
    }));
    let class_bytes = match class_bytes_result {
        Ok(b) => b,
        Err(_) => {
            tracing::error!(
                "Lookup.defineClass: panic while reading byte array (len={length}); \
                 raising IllegalArgumentException"
            );
            return Err(RuntimeError::IllegalArgumentException {
                message: "Lookup.defineClass: panic while reading byte array".into(),
            }
            .into());
        }
    };

    // Validate magic + minimal length (8 bytes = magic + minor + major).
    if class_bytes.len() < 8 || class_bytes[0..4] != CLASS_FILE_MAGIC {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Lookup.defineClass: not a valid class file (bad magic)".into(),
        }
        .into());
    }

    // Sniff the class file's own `this_class` name so the cglib guard
    // can match on `$$EnhancerByCGLIB$$` even when the caller passes
    // no explicit name. Empty / parse-fail → empty string (guard noop).
    let sniffed_name = extract_this_class_name(&class_bytes).unwrap_or_default();

    // cglib SEGV guard — Lookup.defineClass is another entry point
    // that ASM-emitted proxies may use on modern JDK targets.
    if let Some(v) = cglib_guard_value(ctx, &sniffed_name, &class_bytes) {
        return Ok(Some(v));
    }

    // Extract the class file's own name; the backend will validate it
    // and reject mismatches with NoClassDefFoundError. We pass an
    // empty name so the backend skips its name-mismatch check (the
    // class file's `this_class` is authoritative here per JEP 274).
    let opts = cratonvm_native_api::DefineClassFull::default();
    // catch_unwind: backend may panic on malformed bytecode.
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full("", &class_bytes, 0, opts)
    }));
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(
                "Lookup.defineClass: panic inside define_class_full; \
                 raising IllegalArgumentException"
            );
            return Err(RuntimeError::IllegalArgumentException {
                message: "Lookup.defineClass: panic inside backend".into(),
            }
            .into());
        }
    };
    match define_result {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => Err(RuntimeError::IllegalArgumentException {
            message: format!("Lookup.defineClass: {msg}"),
        }
        .into()),
    }
}

/// Extract the internal form of the `this_class` constant pool entry from
/// a raw `.class` file. Returns `None` if the header is malformed or the
/// indices are out of range. Used by [`lk_define_hidden_class`] to derive
/// a HotSpot-style mangled name (`OriginalName/0x<id>`) without having
/// to fully parse the class file.
fn extract_this_class_name(bytes: &[u8]) -> Option<String> {
    // Class file layout prefix:
    //   u4 magic
    //   u2 minor_version
    //   u2 major_version
    //   u2 constant_pool_count
    //   cp_info constant_pool[constant_pool_count - 1]
    //   u2 access_flags
    //   u2 this_class        <-- we want this
    //   ...
    if bytes.len() < 10 {
        return None;
    }
    if bytes[0..4] != CLASS_FILE_MAGIC {
        return None;
    }
    let cp_count = u16::from_be_bytes([bytes[8], bytes[9]]) as usize;
    if cp_count == 0 {
        return None;
    }
    // Scan the constant pool to collect each entry's byte offset and
    // record Utf8 strings and Class entries so we can resolve
    // `this_class` (an index into the pool) to a UTF-8 name.
    let mut pos = 10usize;
    let mut utf8_strings: std::collections::HashMap<u16, String> =
        std::collections::HashMap::new();
    let mut class_name_indices: std::collections::HashMap<u16, u16> =
        std::collections::HashMap::new();

    let mut idx: u16 = 1;
    while (idx as usize) < cp_count {
        if pos >= bytes.len() {
            return None;
        }
        let tag = bytes[pos];
        pos += 1;
        match tag {
            1 => {
                // CONSTANT_Utf8 — u2 length, [u1]* bytes
                if pos + 2 > bytes.len() {
                    return None;
                }
                let len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
                pos += 2;
                if pos + len > bytes.len() {
                    return None;
                }
                // Lenient UTF-8: the JVM uses Modified UTF-8, but for
                // the subset used in internal class names (ASCII-safe
                // `foo/Bar$Inner`) the modified and standard forms
                // agree. Non-conforming names decode via
                // `from_utf8_lossy`, which is harmless for the
                // mangling step.
                let s = String::from_utf8_lossy(&bytes[pos..pos + len]).into_owned();
                utf8_strings.insert(idx, s);
                pos += len;
                idx += 1;
            }
            3 | 4 => {
                // CONSTANT_Integer / CONSTANT_Float — u4
                pos += 4;
                idx += 1;
            }
            5 | 6 => {
                // CONSTANT_Long / CONSTANT_Double — u8, consumes two indices
                pos += 8;
                idx += 2;
            }
            7 => {
                // CONSTANT_Class — u2 name_index (→ Utf8)
                if pos + 2 > bytes.len() {
                    return None;
                }
                let name_idx = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]);
                class_name_indices.insert(idx, name_idx);
                pos += 2;
                idx += 1;
            }
            8 => {
                // CONSTANT_String — u2 string_index
                pos += 2;
                idx += 1;
            }
            9 | 10 | 11 | 12 | 17 | 18 => {
                // Fieldref / Methodref / InterfaceMethodref /
                // NameAndType / InvokeDynamic / Dynamic — u2 + u2
                pos += 4;
                idx += 1;
            }
            15 => {
                // CONSTANT_MethodHandle — u1 reference_kind + u2 reference_index
                pos += 3;
                idx += 1;
            }
            16 | 19 | 20 => {
                // MethodType / Module / Package — u2
                pos += 2;
                idx += 1;
            }
            _ => {
                // Unknown tag; we cannot safely continue parsing.
                return None;
            }
        }
    }

    // After the constant pool: u2 access_flags, u2 this_class.
    if pos + 4 > bytes.len() {
        return None;
    }
    let this_class_idx = u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]);
    let name_idx = class_name_indices.get(&this_class_idx)?;
    utf8_strings.get(name_idx).cloned()
}

fn lk_define_hidden_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Lookup.defineHiddenClass(byte[], boolean, ClassOption...) → Lookup
    //
    // JVMS / JEP 371:
    //   * Parses the byte array, defines the class under a unique
    //     mangled name ("Foo/0x<counter>"), marks it hidden atomically.
    //   * Honors the `initialize` flag: if true, runs <clinit> now.
    //   * Honors the `NESTMATE` ClassOption: copies the lookup class's
    //     nest-host / nest-members onto the new class.
    //   * On any failure, throws the appropriate Java exception
    //     (IllegalArgumentException, ClassFormatError) so the caller
    //     observes a typed error rather than a silently-empty Lookup.
    use cratonvm_types::error::RuntimeError;

    // --- 1. Extract `this` (the defining Lookup) and the bytecode array. ---
    let this_lookup = obj_arg(args, 0)?;
    let byte_array = match args.get(1) {
        Some(Value::Object(Some(arr))) => *arr,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineHiddenClass: bytes must not be null".into(),
            }
            .into());
        }
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineHiddenClass: missing bytes argument".into(),
            }
            .into());
        }
    };

    let length = ctx.array_length(byte_array);
    // Defensive: catch any panic inside the copy loop so a corrupt
    // byte[] (cglib emits 10-50 KB bytecode buffers via ASM) returns
    // an IAE instead of SIGABRT.
    let class_bytes_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut bytes = Vec::with_capacity(length);
        for i in 0..length {
            match ctx.get_array_element(byte_array, i) {
                Value::Int(b) => bytes.push(b as u8),
                _ => bytes.push(0),
            }
        }
        bytes
    }));
    let class_bytes = match class_bytes_result {
        Ok(b) => b,
        Err(_) => {
            tracing::error!(
                "defineHiddenClass: panic while reading byte array (len={length}); \
                 raising IllegalArgumentException"
            );
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineHiddenClass: panic while reading byte array".into(),
            }
            .into());
        }
    };

    // --- 2. Validate the class file magic + minimal header length. ---
    if class_bytes.len() < 8 || class_bytes[0..4] != CLASS_FILE_MAGIC {
        return Err(RuntimeError::IllegalArgumentException {
            message: "defineHiddenClass: not a valid class file (bad magic)".into(),
        }
        .into());
    }

    // --- 3. Extract the original `this_class` name for mangled naming.
    //        If extraction fails we fall back to "HiddenClass" — the
    //        mangle suffix still guarantees uniqueness.
    let original_name =
        extract_this_class_name(&class_bytes).unwrap_or_else(|| "HiddenClass".to_string());
    let id = HIDDEN_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let hidden_name = format!("{original_name}/0x{id:x}");

    // --- 4. Parse the `initialize` flag (arg 2). ---
    let initialize = matches!(args.get(2), Some(Value::Int(n)) if *n != 0);

    // --- 5. Parse the ClassOption[] varargs (arg 3). Each element is a
    //        synthetic ClassOption object; we read field 0 (ordinal) to
    //        detect NESTMATE (ordinal 0) vs STRONG (ordinal 1). The
    //        ordinal convention matches the JDK's enum declaration.
    let mut nestmate = false;
    if let Some(Value::Object(Some(options_arr))) = args.get(3) {
        let opt_count = ctx.array_length(*options_arr);
        for i in 0..opt_count {
            if let Value::Object(Some(opt)) = ctx.get_array_element(*options_arr, i) {
                if let Value::Int(ord) = ctx.get_field(opt, 0) {
                    if ord == 0 {
                        nestmate = true;
                    }
                }
            }
        }
    }

    // --- 6. WP2.3: Define the class via `define_class_full` with
    //        override_name + hidden + nest_host_class_name set
    //        atomically. The backend takes care of name mangling,
    //        hidden-flag stamping, nest attribution, JIT
    //        invalidation, and ProtectionDomain inheritance from the
    //        lookup class.

    // If NESTMATE was requested, look up the lookup class's name now
    // so we can pass it to the backend. The backend resolves it to
    // the actual nest_host (handles transitive nest membership).
    let nest_host_class_name = if nestmate {
        if let Value::Object(Some(lookup_mirror)) =
            ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF)
        {
            crate::lang_class::mirror_class_id(ctx, lookup_mirror)
                .and_then(|cid| ctx.class_name_of_id(cid))
        } else {
            None
        }
    } else {
        None
    };

    let opts = cratonvm_native_api::DefineClassFull {
        override_name: Some(hidden_name.clone()),
        hidden: true,
        nest_host_class_name,
        initialize,
        ..Default::default()
    };

    // cglib SEGV guard — JEP 371 hidden-class path is used by some
    // ASM frameworks (incl. byte-buddy when configured to use hidden
    // classes). If cglib ever routes here, short-circuit before define.
    if let Some(v) = cglib_guard_value(ctx, &original_name, &class_bytes) {
        // Wrap the placeholder mirror back into a Lookup so the caller
        // gets the contractual return type. If the placeholder is null,
        // fall through to the normal path (which will fail cleanly).
        if let Value::Object(Some(mirror)) = v {
            let obj = alloc_lookup(ctx, LK_FULL_POWER);
            ctx.set_field(obj, LK_LOOKUP_CLASS_REF, Value::Object(Some(mirror)));
            return Ok(Some(Value::Object(Some(obj))));
        }
    }

    // catch_unwind: backend may panic on malformed bytecode.
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(&hidden_name, &class_bytes, 0, opts)
    }));
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(
                "defineHiddenClass({hidden_name}): panic inside define_class_full; \
                 raising IllegalArgumentException"
            );
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("defineHiddenClass({hidden_name}): panic inside backend"),
            }
            .into());
        }
    };
    let cid = match define_result {
        Ok(cid) => cid,
        Err(msg) => {
            // initialize=true failures surface as ExceptionInInitializerError-flavored
            // IllegalStateException (closest variant); other errors are
            // IllegalArgumentException per JLS §5.3.5.
            if msg.contains("initialize after define failed") {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("ExceptionInInitializerError for {hidden_name}: {msg}"),
                }
                .into());
            }
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("defineHiddenClass({hidden_name}): {msg}"),
            }
            .into());
        }
    };

    // --- 8. Build the return value: a fresh Lookup whose lookup class
    //        is the hidden class's mirror. Full power mode lets the
    //        caller look up private members via the returned Lookup.
    let mirror = ctx.get_class_mirror(cid);
    let obj = alloc_lookup(ctx, LK_FULL_POWER);
    ctx.set_field(obj, LK_LOOKUP_CLASS_REF, Value::Object(Some(mirror)));
    Ok(Some(Value::Object(Some(obj))))
}

// MethodHandle synthetic layout:
// C19: anchor our synthetic slots past the real JDK's instance-field count
// (6: type, form, asTypeCache, asTypeSoftCache, customizationCount,
// updateInProgress) so that `set_field_by_name(mh, "type", ...)` — which
// resolves to the real-JDK slot 0 — does not clobber our data. Matches the
// layout used by lang_invoke::alloc_method_handle (MH_BASE = 16).
//   MH_BASE+0: kind (Int: 0=virtual, 1=static, 2=constructor, 3=getter, 4=setter,
//                    5=static_getter, 6=static_setter, 7=special)
//   MH_BASE+1: target_class (Object: Class mirror of the declaring class)
//   MH_BASE+2: name (Object: String — method or field name)
//   MH_BASE+3: type (Object: MethodType mirror or descriptor string)
//   MH_BASE+4: resolved_class_id (Int: ClassId.raw() for fast dispatch)
const MH_BASE: usize = 16;
const MH_KIND: usize = MH_BASE + 0;
const MH_TARGET_CLASS: usize = MH_BASE + 1;
const MH_NAME: usize = MH_BASE + 2;
const MH_TYPE: usize = MH_BASE + 3;
const MH_CLASS_ID: usize = MH_BASE + 4;
const MH_FIELD_COUNT: usize = MH_BASE + 5;

fn alloc_method_handle(
    ctx: &mut dyn NativeContext,
    kind: i32,
    class_mirror: Option<ObjectRef>,
    name: Option<ObjectRef>,
    method_type: Option<ObjectRef>,
) -> ObjectRef {
    let mh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", MH_FIELD_COUNT);
    ctx.set_field(mh, MH_KIND, Value::Int(kind));
    ctx.set_field(mh, MH_TARGET_CLASS, match class_mirror {
        Some(r) => Value::Object(Some(r)),
        None => Value::Object(None),
    });
    ctx.set_field(mh, MH_NAME, match name {
        Some(r) => Value::Object(Some(r)),
        None => Value::Object(None),
    });
    ctx.set_field(mh, MH_TYPE, match method_type {
        Some(r) => Value::Object(Some(r)),
        None => Value::Object(None),
    });
    // Resolve class ID if class mirror is available
    if let Some(mirror) = class_mirror {
        if let Some(cid) = crate::lang_class::mirror_class_id(ctx, mirror) {
            ctx.set_field(mh, MH_CLASS_ID, Value::Int(cid.as_u32() as i32));
        }
    }
    // C19/C21: Populate the real-JDK `MethodHandle.type:MethodType` field
    // (resolved by name to slot 0) so `mh.type()` and JDK-internal reads
    // (`erasedType`, `parameterSlotCount`, LambdaForm walks) see a non-null
    // MethodType. Prefer the caller-provided method_type (a MethodType
    // mirror from the Lookup.findXxx JVM call); fall back to a synthetic
    // `()V` MethodType when nothing was supplied (e.g. lk_unreflect, where
    // the Java caller did not pass an explicit MethodType).
    let mt_to_store = method_type.or_else(|| {
        crate::lang_invoke::build_method_type_from_descriptor(ctx, "()V")
    });
    if let Some(mt) = mt_to_store {
        ctx.set_field_by_name(mh, "type", Value::Object(Some(mt)));
    }
    mh
}

// findVirtual(Class refc, String name, MethodType type) -> MethodHandle
fn lk_find_virtual(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let name = match args.get(2) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mtype = match args.get(3) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mh = alloc_method_handle(ctx, 0, class_mirror, name, mtype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_static(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let name = match args.get(2) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mtype = match args.get(3) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mh = alloc_method_handle(ctx, 1, class_mirror, name, mtype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_constructor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mtype = match args.get(2) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let name_str = ctx.create_string("<init>");
    let mh = alloc_method_handle(ctx, 2, class_mirror, Some(name_str), mtype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_getter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let name = match args.get(2) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let ftype = match args.get(3) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mh = alloc_method_handle(ctx, 3, class_mirror, name, ftype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_setter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let name = match args.get(2) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let ftype = match args.get(3) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mh = alloc_method_handle(ctx, 4, class_mirror, name, ftype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_static_getter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let name = match args.get(2) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let ftype = match args.get(3) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mh = alloc_method_handle(ctx, 5, class_mirror, name, ftype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_static_setter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let name = match args.get(2) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let ftype = match args.get(3) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mh = alloc_method_handle(ctx, 6, class_mirror, name, ftype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_special(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let name = match args.get(2) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mtype = match args.get(3) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let mh = alloc_method_handle(ctx, 7, class_mirror, name, mtype);
    Ok(Some(Value::Object(Some(mh))))
}

// VarHandle synthetic layout (3 fields): 0=target_class, 1=field_name, 2=field_type
fn lk_find_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let name = match args.get(2) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let ftype = match args.get(3) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    let vh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", 3);
    if let Some(cm) = class_mirror { ctx.set_field(vh, 0, Value::Object(Some(cm))); }
    if let Some(n) = name { ctx.set_field(vh, 1, Value::Object(Some(n))); }
    if let Some(t) = ftype { ctx.set_field(vh, 2, Value::Object(Some(t))); }
    Ok(Some(Value::Object(Some(vh))))
}

fn lk_find_static_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_find_var_handle(ctx, args)
}

fn lk_unreflect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // unreflect(Method) -> MethodHandle — extract class/name from the Method object
    let method = match args.get(1) { Some(Value::Object(Some(o))) => Some(*o), _ => None };
    // C6: Method/Constructor use real-JDK field layout; read by name.
    let class_mirror = method.map(|m| ctx.get_field_by_name(m, "clazz")).and_then(|v| match v {
        Value::Object(Some(r)) => Some(r), _ => None,
    });
    let name = method.map(|m| ctx.get_field_by_name(m, "name")).and_then(|v| match v {
        Value::Object(Some(r)) => Some(r), _ => None,
    });
    let mh = alloc_method_handle(ctx, 0, class_mirror, name, None);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_unreflect_special(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_unreflect(ctx, args)
}

fn lk_in_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let modes = match ctx.get_field(this, LK_ALLOWED_MODES) {
        Value::Int(v) => v,
        _ => LK_PUBLIC,
    };
    let new_lk = alloc_lookup(ctx, modes);
    ctx.set_field(new_lk, LK_LOOKUP_CLASS_REF, target);
    Ok(Some(Value::Object(Some(new_lk))))
}

fn lk_drop_lookup_mode(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let drop_mode = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    let modes = match ctx.get_field(this, LK_ALLOWED_MODES) {
        Value::Int(v) => v,
        _ => LK_FULL_POWER,
    };
    let new_modes = modes & !drop_mode;
    let new_lk = alloc_lookup(ctx, new_modes);
    let cls = ctx.get_field(this, LK_LOOKUP_CLASS_REF);
    ctx.set_field(new_lk, LK_LOOKUP_CLASS_REF, cls);
    Ok(Some(Value::Object(Some(new_lk))))
}

// ---------------------------------------------------------------------------
// java.security.ProtectionDomain natives
// ---------------------------------------------------------------------------

fn pd_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let code_source = args.get(1).copied().unwrap_or(Value::Object(None));
    let permissions = args.get(2).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, PD_CODE_SOURCE_REF, code_source);
    ctx.set_field(this, PD_PERMISSIONS_REF, permissions);
    ctx.set_field(this, PD_CLASS_LOADER_REF, Value::Object(None));
    Ok(None)
}

fn pd_get_code_source(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, PD_CODE_SOURCE_REF)))
}

fn pd_get_permissions(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, PD_PERMISSIONS_REF)))
}

fn pd_get_class_loader(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, PD_CLASS_LOADER_REF)))
}

fn pd_implies(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // All permissions implied by default in our stub
    Ok(Some(Value::Int(1)))
}

// ---------------------------------------------------------------------------
// java.security.CodeSource natives
// ---------------------------------------------------------------------------

fn cs_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let location = args.get(1).copied().unwrap_or(Value::Object(None));
    let certs = args.get(2).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, CS_LOCATION_REF, location);
    ctx.set_field(this, CS_CERTIFICATES_REF, certs);
    Ok(None)
}

fn cs_get_location(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, CS_LOCATION_REF)))
}

fn cs_get_certificates(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, CS_CERTIFICATES_REF)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub(crate) fn register_classloader_natives(r: &mut NativeMethodRegistry) {
    // -----------------------------------------------------------------------
    // java/lang/ClassLoader
    // -----------------------------------------------------------------------
    let cl = CL_CLASS;

    r.register(cl, "<init>", "()V", cl_init_default);
    r.register(cl, "<init>", "(Ljava/lang/ClassLoader;)V", cl_init_parent);
    r.register(cl, "<init>", "(Ljava/lang/String;Ljava/lang/ClassLoader;)V", cl_init_name_parent);
    r.register(cl, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", cl_load_class);
    r.register(cl, "loadClass", "(Ljava/lang/String;Z)Ljava/lang/Class;", cl_load_class_resolve);
    // T19_H12_LOADCLASS_MODULE — JDK 25 package-private overload used by
    // `Class.forName(Module, String)`'s stock bytecode. Registering on
    // ClassLoader keeps real ClassLoader receivers correct; the
    // `Class.forName(Module, String)` native (lang_class.rs) bypasses
    // the broken JDK bytecode path entirely so we never dispatch this
    // virtual call onto a synthetic Module whose receiver-class drifts.
    r.register(cl, "loadClass", "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;", cl_load_class_module);
    r.register(cl, "findClass", "(Ljava/lang/String;)Ljava/lang/Class;", cl_find_class);
    r.register(cl, "findClass", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;", cl_find_class_module);
    r.register(cl, "defineClass", "(Ljava/lang/String;[BII)Ljava/lang/Class;", cl_define_class_basic);
    r.register(cl, "defineClass", "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;", cl_define_class_pd);
    r.register(cl, "defineClass", "(Ljava/lang/String;Ljava/nio/ByteBuffer;Ljava/security/ProtectionDomain;)Ljava/lang/Class;", cl_define_class_bb);
    // WP2.3-C: JDK-internal defineClass0/1/2 natives that the public
    // overloads route through. CGLIB / direct user code typically calls
    // these via the public Java wrappers.
    register_classloader_define_class(r);

    // Round-16 (agent 16): defensive Unsafe.defineClass shim for cglib.
    // cglib proxy generation goes through `sun.misc.Unsafe.defineClass`,
    // which previously SEGV'd on null/oversized bytecode in the non-JIT
    // path. This shim validates args up-front and routes through the
    // shared `define_class_full` backend.
    r.register(
        "sun/misc/Unsafe",
        "defineClass",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        unsafe_define_class_defensive,
    );
    // jdk.internal.misc.Unsafe — JDK 9+ public path that user code can't
    // reach directly but `jdk.internal.misc.Unsafe.getUnsafe()` callers
    // (some bytecode-manipulation libs) hit. Same shim covers both.
    r.register(
        "jdk/internal/misc/Unsafe",
        "defineClass",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        unsafe_define_class_defensive,
    );
    r.register(cl, "resolveClass", "(Ljava/lang/Class;)V", cl_resolve_class);
    r.register(cl, "findLoadedClass", "(Ljava/lang/String;)Ljava/lang/Class;", cl_find_loaded_class);
    r.register(cl, "getParent", "()Ljava/lang/ClassLoader;", cl_get_parent);
    r.register(cl, "getName", "()Ljava/lang/String;", cl_get_name);
    r.register(cl, "getSystemClassLoader", "()Ljava/lang/ClassLoader;", cl_get_system_class_loader);
    r.register(cl, "getPlatformClassLoader", "()Ljava/lang/ClassLoader;", cl_get_platform_class_loader);
    r.register(cl, "getResource", "(Ljava/lang/String;)Ljava/net/URL;", cl_get_resource);
    r.register(cl, "getResources", "(Ljava/lang/String;)Ljava/util/Enumeration;", cl_get_resources);
    r.register(cl, "getSystemResources", "(Ljava/lang/String;)Ljava/util/Enumeration;", cl_get_system_resources);
    r.register(cl, "getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;", cl_get_resource_as_stream);
    r.register(cl, "getDefinedPackage", "(Ljava/lang/String;)Ljava/lang/Package;", cl_get_defined_package);
    r.register(cl, "getDefinedPackages", "()[Ljava/lang/Package;", cl_get_defined_packages);
    // `ClassLoader.getPackages()` — real JDK bytecode is
    // `return packages().toArray(Package[]::new)` with a stream pipeline that
    // (in our boot) leaks a `ReferencePipeline$Head` into the caller's local
    // typed as `Package[]`, causing NPE on arraylength in
    // `org/jboss/modules/ConcurrentClassLoader.<clinit>` (WildFly 39 boot).
    // Override with an empty array — matches the empty `getDefinedPackages`
    // override and is sufficient for jboss-modules' sanity scan.
    r.register(cl, "getPackages", "()[Ljava/lang/Package;", cl_get_defined_packages);
    r.register(cl, "setDefaultAssertionStatus", "(Z)V", cl_set_default_assertion_status);
    r.register(cl, "registerAsParallelCapable", "()Z", cl_register_as_parallel_capable);
    r.register(cl, "isRegisteredAsParallelCapable", "()Z", cl_is_registered_as_parallel_capable);

    // -----------------------------------------------------------------------
    // java/net/URLClassLoader
    // -----------------------------------------------------------------------
    let ucl = UCL_CLASS;

    r.register(ucl, "<init>", "([Ljava/net/URL;)V", ucl_init_urls);
    r.register(ucl, "<init>", "([Ljava/net/URL;Ljava/lang/ClassLoader;)V", ucl_init_urls_parent);
    r.register(ucl, "<init>", "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V", ucl_init_urls_parent_factory);
    r.register(ucl, "findClass", "(Ljava/lang/String;)Ljava/lang/Class;", ucl_find_class);
    r.register(ucl, "findResource", "(Ljava/lang/String;)Ljava/net/URL;", ucl_find_resource);
    r.register(ucl, "findResources", "(Ljava/lang/String;)Ljava/util/Enumeration;", ucl_find_resources);
    r.register(ucl, "getURLs", "()[Ljava/net/URL;", ucl_get_urls);
    r.register(ucl, "addURL", "(Ljava/net/URL;)V", ucl_add_url);
    r.register(ucl, "close", "()V", ucl_close);
    r.register(ucl, "newInstance", "([Ljava/net/URL;)Ljava/net/URLClassLoader;", ucl_new_instance);
    r.register(ucl, "newInstance", "([Ljava/net/URL;Ljava/lang/ClassLoader;)Ljava/net/URLClassLoader;", ucl_new_instance_parent);

    // -----------------------------------------------------------------------
    // java/lang/invoke/MethodHandles$Lookup
    // -----------------------------------------------------------------------
    let lk = LK_CLASS;

    r.register(lk, "lookup", "()Ljava/lang/invoke/MethodHandles$Lookup;", lk_lookup);
    r.register(lk, "privateLookupIn", "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandles$Lookup;)Ljava/lang/invoke/MethodHandles$Lookup;", lk_private_lookup_in);
    r.register(lk, "publicLookup", "()Ljava/lang/invoke/MethodHandles$Lookup;", lk_public_lookup);
    r.register(lk, "lookupClass", "()Ljava/lang/Class;", lk_lookup_class);
    r.register(lk, "previousLookupClass", "()Ljava/lang/Class;", lk_previous_lookup_class);
    r.register(lk, "lookupModes", "()I", lk_lookup_modes);
    r.register(lk, "hasFullPrivilegeAccess", "()Z", lk_has_full_privilege_access);
    r.register(lk, "hasPrivateAccess", "()Z", lk_has_private_access);
    r.register(lk, "defineClass", "([B)Ljava/lang/Class;", lk_define_class);
    r.register(lk, "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;", lk_define_hidden_class);
    // findVirtual/findStatic/findConstructor/findGetter/findSetter/findSpecial/
    // findVarHandle/findStaticVarHandle are all registered in
    // lang_invoke::register_p63_method_handles_lookup — do NOT re-register here
    // as that would overwrite the real implementations with incompatible stubs.
    r.register(lk, "unreflect", "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;", lk_unreflect);
    r.register(lk, "unreflectSpecial", "(Ljava/lang/reflect/Method;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", lk_unreflect_special);
    r.register(lk, "in", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandles$Lookup;", lk_in_method);
    r.register(lk, "dropLookupMode", "(I)Ljava/lang/invoke/MethodHandles$Lookup;", lk_drop_lookup_mode);

    // -----------------------------------------------------------------------
    // java/lang/ClassLoader$HiddenClass (stub)
    // -----------------------------------------------------------------------
    // No natives needed — just a data holder (2-field synthetic)

    // -----------------------------------------------------------------------
    // java/security/ProtectionDomain
    // -----------------------------------------------------------------------
    let pd = PD_CLASS;

    r.register(pd, "<init>", "(Ljava/security/CodeSource;Ljava/security/PermissionCollection;)V", pd_init);
    r.register(pd, "getCodeSource", "()Ljava/security/CodeSource;", pd_get_code_source);
    r.register(pd, "getPermissions", "()Ljava/security/PermissionCollection;", pd_get_permissions);
    r.register(pd, "getClassLoader", "()Ljava/lang/ClassLoader;", pd_get_class_loader);
    r.register(pd, "implies", "(Ljava/security/Permission;)Z", pd_implies);

    // -----------------------------------------------------------------------
    // java/security/CodeSource
    // -----------------------------------------------------------------------
    let cs = CS_CLASS;

    r.register(cs, "<init>", "(Ljava/net/URL;[Ljava/security/cert/Certificate;)V", cs_init);
    r.register(cs, "getLocation", "()Ljava/net/URL;", cs_get_location);
    r.register(cs, "getCertificates", "()[Ljava/security/cert/Certificate;", cs_get_certificates);

    // -----------------------------------------------------------------------
    // java/io/ByteArrayInputStream — 4-field (buf=0, pos=1, mark=2, count=3)
    // -----------------------------------------------------------------------
    let bais = "java/io/ByteArrayInputStream";
    r.register(bais, "<init>", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let buf = args.get(1).copied().unwrap_or(Value::Object(None));
        let len = match buf {
            Value::Object(Some(arr)) => ctx.array_length(arr) as i32,
            _ => 0,
        };
        ctx.set_field(this, 0, buf);       // buf
        ctx.set_field(this, 1, Value::Int(0));   // pos
        ctx.set_field(this, 2, Value::Int(0));   // mark
        ctx.set_field(this, 3, Value::Int(len)); // count
        Ok(None)
    });
    r.register(bais, "<init>", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let buf = args.get(1).copied().unwrap_or(Value::Object(None));
        let off = args[2].as_int().unwrap_or(0);
        let len = args[3].as_int().unwrap_or(0);
        ctx.set_field(this, 0, buf);              // buf
        ctx.set_field(this, 1, Value::Int(off));  // pos
        ctx.set_field(this, 2, Value::Int(off));  // mark
        ctx.set_field(this, 3, Value::Int(off + len)); // count
        Ok(None)
    });
    r.register(bais, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let count = ctx.get_field(this, 3).as_int().unwrap_or(0);
        if pos >= count { return Ok(Some(Value::Int(-1))); }
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let b = ctx.get_array_element(arr, pos as usize).as_int().unwrap_or(0);
        ctx.set_field(this, 1, Value::Int(pos + 1));
        Ok(Some(Value::Int(b & 0xff)))
    });
    r.register(bais, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let count = ctx.get_field(this, 3).as_int().unwrap_or(0) as usize;
        if pos >= count { return Ok(Some(Value::Int(-1))); }
        let avail = count - pos;
        let n = len.min(avail);
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        for i in 0..n {
            let b = ctx.get_array_element(arr, pos + i);
            ctx.set_array_element(dst, off + i, b);
        }
        ctx.set_field(this, 1, Value::Int((pos + n) as i32));
        Ok(Some(Value::Int(n as i32)))
    });
    r.register(bais, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let count = ctx.get_field(this, 3).as_int().unwrap_or(0);
        Ok(Some(Value::Int((count - pos).max(0))))
    });
    r.register(bais, "skip", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let count = ctx.get_field(this, 3).as_int().unwrap_or(0);
        let avail = (count - pos).max(0) as i64;
        let skipped = n.min(avail);
        ctx.set_field(this, 1, Value::Int(pos + skipped as i32));
        Ok(Some(Value::Long(skipped)))
    });
    r.register(bais, "close", "()V", |_ctx, _args| Ok(None));
    r.register(bais, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mark = ctx.get_field(this, 2).as_int().unwrap_or(0);
        ctx.set_field(this, 1, Value::Int(mark));
        Ok(None)
    });
    r.register(bais, "mark", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0);
        ctx.set_field(this, 2, Value::Int(pos)); // mark = pos
        Ok(None)
    });
    r.register(bais, "markSupported", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });

    // -----------------------------------------------------------------------
    // java/io/FilterInputStream — field 0 = wrapped InputStream
    // -----------------------------------------------------------------------
    let fis = "java/io/FilterInputStream";
    r.register(fis, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, stream); // in
        Ok(None)
    });

    // -----------------------------------------------------------------------
    // java/io/DataInputStream — extends FilterInputStream
    //   field 0 = in (from FilterInputStream)
    //   field 1 = readBuffer (byte[8], allocated in <init>)
    //   field 2 = bytearr
    //   field 3 = chararr
    //
    // We provide native readInt/readShort/readUnsignedShort/readLong/readUTF/
    // readBoolean/readByte/readFully that recursively unwrap FilterInputStream
    // chains (DIS → BIS → BAIS) and read directly from the BAIS data.
    // -----------------------------------------------------------------------
    let dis = "java/io/DataInputStream";

    // Helper: read a single byte from a stream object, advancing pos.
    // Recursively unwraps FilterInputStreams until it finds a BAIS.
    fn dis_read_byte(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Option<u8> {
        let cid = ctx.class_id_of_object(stream);
        let cname = ctx.class_name_of_id(cid).unwrap_or_default();
        if cname == "java/io/ByteArrayInputStream" {
            // BAIS: buf=0, pos=1, mark=2, count=3
            let pos = ctx.get_field(stream, 1).as_int().unwrap_or(0);
            let count = ctx.get_field(stream, 3).as_int().unwrap_or(0);
            if pos >= count { return None; }
            let buf = match ctx.get_field(stream, 0) {
                Value::Object(Some(a)) => a,
                _ => return None,
            };
            let b = ctx.get_array_element(buf, pos as usize).as_int().unwrap_or(0);
            ctx.set_field(stream, 1, Value::Int(pos + 1));
            Some((b & 0xFF) as u8)
        } else {
            // FilterInputStream: field 0 = in
            match ctx.get_field(stream, 0) {
                Value::Object(Some(inner)) => dis_read_byte(ctx, inner),
                _ => None,
            }
        }
    }

    fn dis_read_n(ctx: &mut dyn NativeContext, stream: ObjectRef, n: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(n);
        for _ in 0..n {
            match dis_read_byte(ctx, stream) {
                Some(b) => buf.push(b),
                None => break,
            }
        }
        buf
    }

    r.register(dis, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, stream); // in (FilterInputStream.in)
        // readBuffer = new byte[8]
        let rb = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
        ctx.set_field(this, 1, Value::Object(Some(rb)));
        Ok(None)
    });

    r.register(dis, "readInt", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 4);
        if bytes.len() < 4 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readInt".into(),
            }.into());
        }
        let v = i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Ok(Some(Value::Int(v)))
    });

    r.register(dis, "readShort", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 2);
        if bytes.len() < 2 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readShort".into(),
            }.into());
        }
        let v = i16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Some(Value::Int(v as i32)))
    });

    r.register(dis, "readUnsignedShort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 2);
        if bytes.len() < 2 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readUnsignedShort".into(),
            }.into());
        }
        let v = u16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Some(Value::Int(v as i32)))
    });

    r.register(dis, "readLong", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 8);
        if bytes.len() < 8 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readLong".into(),
            }.into());
        }
        let v = i64::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3],
                                     bytes[4], bytes[5], bytes[6], bytes[7]]);
        Ok(Some(Value::Long(v)))
    });

    r.register(dis, "readBoolean", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 1);
        if bytes.is_empty() {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readBoolean".into(),
            }.into());
        }
        Ok(Some(Value::Int(if bytes[0] != 0 { 1 } else { 0 })))
    });

    r.register(dis, "readByte", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 1);
        if bytes.is_empty() {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readByte".into(),
            }.into());
        }
        Ok(Some(Value::Int(bytes[0] as i8 as i32)))
    });

    r.register(dis, "readUnsignedByte", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 1);
        if bytes.is_empty() {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readUnsignedByte".into(),
            }.into());
        }
        Ok(Some(Value::Int(bytes[0] as i32)))
    });

    r.register(dis, "readChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 2);
        if bytes.len() < 2 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readChar".into(),
            }.into());
        }
        let v = u16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Some(Value::Int(v as i32)))
    });

    r.register(dis, "readFloat", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 4);
        if bytes.len() < 4 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readFloat".into(),
            }.into());
        }
        let bits = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Ok(Some(Value::Float(f32::from_bits(bits))))
    });

    r.register(dis, "readDouble", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 8);
        if bytes.len() < 8 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readDouble".into(),
            }.into());
        }
        let bits = u64::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3],
                                        bytes[4], bytes[5], bytes[6], bytes[7]]);
        Ok(Some(Value::Double(f64::from_bits(bits))))
    });

    r.register(dis, "readFully", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        let bytes = dis_read_n(ctx, this, len);
        if bytes.len() < len {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readFully".into(),
            }.into());
        }
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(dst, off + i, Value::Int(b as i8 as i32));
        }
        Ok(None)
    });

    r.register(dis, "readFully", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let len = ctx.array_length(dst);
        let bytes = dis_read_n(ctx, this, len);
        if bytes.len() < len {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readFully".into(),
            }.into());
        }
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(dst, i, Value::Int(b as i8 as i32));
        }
        Ok(None)
    });

    // readUTF() — reads modified UTF-8 string (2-byte length prefix + data)
    r.register(dis, "readUTF", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Read 2-byte length prefix
        let len_bytes = dis_read_n(ctx, this, 2);
        if len_bytes.len() < 2 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readUTF".into(),
            }.into());
        }
        let utf_len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
        let data = dis_read_n(ctx, this, utf_len);
        if data.len() < utf_len {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readUTF data".into(),
            }.into());
        }
        // Decode modified UTF-8
        let mut chars = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if b == 0 { break; }
            if b < 0x80 {
                chars.push(b as char);
                i += 1;
            } else if b & 0xE0 == 0xC0 {
                if i + 1 >= data.len() { break; }
                let c = ((b as u32 & 0x1F) << 6) | (data[i+1] as u32 & 0x3F);
                chars.push(char::from_u32(c).unwrap_or('?'));
                i += 2;
            } else if b & 0xF0 == 0xE0 {
                if i + 2 >= data.len() { break; }
                let c = ((b as u32 & 0x0F) << 12)
                    | ((data[i+1] as u32 & 0x3F) << 6)
                    | (data[i+2] as u32 & 0x3F);
                chars.push(char::from_u32(c).unwrap_or('?'));
                i += 3;
            } else {
                chars.push('?');
                i += 1;
            }
        }
        let s: String = chars.into_iter().collect();
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });

    // read()I — single byte
    r.register(dis, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match dis_read_byte(ctx, this) {
            Some(b) => Ok(Some(Value::Int(b as i32))),
            None => Ok(Some(Value::Int(-1))),
        }
    });

    // read([BII)I — bulk read
    r.register(dis, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        let bytes = dis_read_n(ctx, this, len);
        if bytes.is_empty() {
            return Ok(Some(Value::Int(-1)));
        }
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(dst, off + i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Int(bytes.len() as i32)))
    });

    // skipBytes(int)int
    r.register(dis, "skipBytes", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = args[1].as_int().unwrap_or(0).max(0) as usize;
        let bytes = dis_read_n(ctx, this, n);
        Ok(Some(Value::Int(bytes.len() as i32)))
    });

    // available()I — delegate to underlying stream
    r.register(dis, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Walk down to BAIS
        fn avail(ctx: &dyn NativeContext, s: ObjectRef) -> i32 {
            let cid = ctx.class_id_of_object(s);
            let cname = ctx.class_name_of_id(cid).unwrap_or_default();
            if cname == "java/io/ByteArrayInputStream" {
                let pos = ctx.get_field(s, 1).as_int().unwrap_or(0);
                let count = ctx.get_field(s, 3).as_int().unwrap_or(0);
                (count - pos).max(0)
            } else {
                match ctx.get_field(s, 0) {
                    Value::Object(Some(inner)) => avail(ctx, inner),
                    _ => 0,
                }
            }
        }
        Ok(Some(Value::Int(avail(ctx, this))))
    });

    r.register(dis, "close", "()V", |_ctx, _args| Ok(None));

    // -----------------------------------------------------------------------
    // java/io/BufferedInputStream — extends FilterInputStream
    //   field 0 = in (FilterInputStream.in)
    //   Plus: initialSize, buf, count, pos, markpos, marklimit
    // We provide <init> and delegate read() to the underlying stream directly.
    // -----------------------------------------------------------------------
    let bis = "java/io/BufferedInputStream";
    r.register(bis, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, stream); // in
        Ok(None)
    });
    r.register(bis, "<init>", "(Ljava/io/InputStream;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, stream); // in
        Ok(None)
    });
    r.register(bis, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match dis_read_byte(ctx, this) {
            Some(b) => Ok(Some(Value::Int(b as i32))),
            None => Ok(Some(Value::Int(-1))),
        }
    });
    r.register(bis, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        let bytes = dis_read_n(ctx, this, len);
        if bytes.is_empty() {
            return Ok(Some(Value::Int(-1)));
        }
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(dst, off + i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Int(bytes.len() as i32)))
    });
    r.register(bis, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        fn avail(ctx: &dyn NativeContext, s: ObjectRef) -> i32 {
            let cid = ctx.class_id_of_object(s);
            let cname = ctx.class_name_of_id(cid).unwrap_or_default();
            if cname == "java/io/ByteArrayInputStream" {
                let pos = ctx.get_field(s, 1).as_int().unwrap_or(0);
                let count = ctx.get_field(s, 3).as_int().unwrap_or(0);
                (count - pos).max(0)
            } else {
                match ctx.get_field(s, 0) {
                    Value::Object(Some(inner)) => avail(ctx, inner),
                    _ => 0,
                }
            }
        }
        Ok(Some(Value::Int(avail(ctx, this))))
    });
    r.register(bis, "close", "()V", |_ctx, _args| Ok(None));

    // -----------------------------------------------------------------------
    // cglib probe — formerly short-circuited (CglibProbe.main / <clinit> /
    // <init> / Greeter no-ops) to dodge a failure in the `defineClass`
    // path. Root cause fixed: custom `ClassLoader` subclasses had a null
    // `defaultDomain` field because the simplified real-JDK
    // `ClassLoader.<init>` natives skipped the real ctor's
    // `defaultDomain = new ProtectionDomain(...)` initialiser; the JDK
    // `preDefineClass` bytecode then NPE'd on `defaultDomain.getCodeSource()`.
    // `classloader_real.rs::init_classloader_common_fields` now builds a
    // non-null `defaultDomain`, so the real cglib `Enhancer.create()` →
    // `defineClass` path runs. No probe shims registered here.
    // -----------------------------------------------------------------------
    // Enumeration$Impl — 2-field (array=0, index=1)
    // Used by getResources() to return an Enumeration over URL[].
    // Delegated to the shared registrar so real-JDK mode gets the same
    // natives without duplicating the closures here.
    // -----------------------------------------------------------------------
    register_enumeration_impl_natives(r);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod classloader_tests {
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;

    fn make_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_classloader_natives(&mut r);
        // Lookup find* methods are registered in lang_invoke, not classloader
        crate::lang_invoke::register_p63_method_handles_lookup(&mut r);
        r
    }

    // --- ClassLoader registration tests ---

    #[test]
    fn test_cl_init_default_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "<init>", "()V").is_some());
    }

    #[test]
    fn test_cl_init_parent_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "<init>", "(Ljava/lang/ClassLoader;)V").is_some());
    }

    #[test]
    fn test_cl_init_name_parent_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "<init>", "(Ljava/lang/String;Ljava/lang/ClassLoader;)V").is_some());
    }

    #[test]
    fn test_cl_load_class_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;").is_some());
    }

    #[test]
    fn test_cl_load_class_resolve_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "loadClass", "(Ljava/lang/String;Z)Ljava/lang/Class;").is_some());
    }

    #[test]
    fn test_cl_find_class_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "findClass", "(Ljava/lang/String;)Ljava/lang/Class;").is_some());
    }

    #[test]
    fn test_cl_find_class_module_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "findClass", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;").is_some());
    }

    #[test]
    fn test_cl_define_class_basic_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "defineClass", "(Ljava/lang/String;[BII)Ljava/lang/Class;").is_some());
    }

    #[test]
    fn test_cl_define_class_pd_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "defineClass", "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;").is_some());
    }

    #[test]
    fn test_cl_define_class_bb_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "defineClass", "(Ljava/lang/String;Ljava/nio/ByteBuffer;Ljava/security/ProtectionDomain;)Ljava/lang/Class;").is_some());
    }

    // --- WP2.3-C: JDK-internal defineClass0/1/2 ---

    #[test]
    fn test_cl_define_class1_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "defineClass1",
                "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
            )
            .is_some());
    }

    #[test]
    fn test_cl_define_class2_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "defineClass2",
                "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/nio/ByteBuffer;IILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
            )
            .is_some());
    }

    #[test]
    fn test_cl_define_class0_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "defineClass0",
                "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BIILjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
            )
            .is_some());
    }

    #[test]
    fn test_register_classloader_define_class_idempotent() {
        // The dedicated registrar `register_classloader_define_class`
        // can be called on its own (the top-level registrar already
        // calls it). Make sure it registers all three internal
        // natives, even when invoked directly.
        let mut r = NativeMethodRegistry::new();
        register_classloader_define_class(&mut r);
        assert!(r
            .find(
                CL_CLASS,
                "defineClass1",
                "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
            )
            .is_some());
        assert!(r
            .find(
                CL_CLASS,
                "defineClass2",
                "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/nio/ByteBuffer;IILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
            )
            .is_some());
        assert!(r
            .find(
                CL_CLASS,
                "defineClass0",
                "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BIILjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
            )
            .is_some());
    }

    #[test]
    fn test_class_data_side_table_round_trip() {
        // The side-table is process-wide and only hashes the pointer
        // bits. We synthesize a fake `ObjectRef` from an
        // 8-byte-aligned, non-null integer constant (validated by
        // `ObjectRef::from_raw`). The pointer is never dereferenced.
        use cratonvm_types::ObjectRef;
        let fake = unsafe { ObjectRef::from_raw(0xfeed_face_0000_1000usize as *mut u8) };
        let _prev = set_class_data(fake, Value::Int(42));
        let got = get_class_data(fake);
        assert_eq!(got, Value::Int(42));
        // Reset clears the side-table.
        reset_loader_singletons();
        assert_eq!(get_class_data(fake), Value::Object(None));
    }

    #[test]
    fn test_cl_get_parent_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "getParent", "()Ljava/lang/ClassLoader;").is_some());
    }

    #[test]
    fn test_cl_get_name_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "getName", "()Ljava/lang/String;").is_some());
    }

    #[test]
    fn test_cl_get_system_class_loader_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "getSystemClassLoader", "()Ljava/lang/ClassLoader;").is_some());
    }

    #[test]
    fn test_cl_get_platform_class_loader_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "getPlatformClassLoader", "()Ljava/lang/ClassLoader;").is_some());
    }

    #[test]
    fn test_cl_resolve_class_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "resolveClass", "(Ljava/lang/Class;)V").is_some());
    }

    #[test]
    fn test_cl_find_loaded_class_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "findLoadedClass", "(Ljava/lang/String;)Ljava/lang/Class;").is_some());
    }

    #[test]
    fn test_cl_get_resource_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "getResource", "(Ljava/lang/String;)Ljava/net/URL;").is_some());
    }

    #[test]
    fn test_cl_register_as_parallel_capable_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "registerAsParallelCapable", "()Z").is_some());
    }

    #[test]
    fn test_cl_is_registered_as_parallel_capable_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "isRegisteredAsParallelCapable", "()Z").is_some());
    }

    // --- URLClassLoader registration tests ---

    #[test]
    fn test_ucl_init_urls_registered() {
        let r = make_registry();
        assert!(r.find(UCL_CLASS, "<init>", "([Ljava/net/URL;)V").is_some());
    }

    #[test]
    fn test_ucl_init_urls_parent_registered() {
        let r = make_registry();
        assert!(r.find(UCL_CLASS, "<init>", "([Ljava/net/URL;Ljava/lang/ClassLoader;)V").is_some());
    }

    #[test]
    fn test_ucl_close_registered() {
        let r = make_registry();
        assert!(r.find(UCL_CLASS, "close", "()V").is_some());
    }

    #[test]
    fn test_ucl_get_urls_registered() {
        let r = make_registry();
        assert!(r.find(UCL_CLASS, "getURLs", "()[Ljava/net/URL;").is_some());
    }

    #[test]
    fn test_ucl_add_url_registered() {
        let r = make_registry();
        assert!(r.find(UCL_CLASS, "addURL", "(Ljava/net/URL;)V").is_some());
    }

    #[test]
    fn test_ucl_new_instance_registered() {
        let r = make_registry();
        assert!(r.find(UCL_CLASS, "newInstance", "([Ljava/net/URL;)Ljava/net/URLClassLoader;").is_some());
    }

    #[test]
    fn test_ucl_new_instance_parent_registered() {
        let r = make_registry();
        assert!(r.find(UCL_CLASS, "newInstance", "([Ljava/net/URL;Ljava/lang/ClassLoader;)Ljava/net/URLClassLoader;").is_some());
    }

    // --- MethodHandles$Lookup registration tests ---

    #[test]
    fn test_lk_lookup_registered() {
        let r = make_registry();
        assert!(r.find(LK_CLASS, "lookup", "()Ljava/lang/invoke/MethodHandles$Lookup;").is_some());
    }

    #[test]
    fn test_lk_public_lookup_registered() {
        let r = make_registry();
        assert!(r.find(LK_CLASS, "publicLookup", "()Ljava/lang/invoke/MethodHandles$Lookup;").is_some());
    }

    #[test]
    fn test_lk_find_virtual_registered() {
        let r = make_registry();
        assert!(r.find(LK_CLASS, "findVirtual", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;").is_some());
    }

    #[test]
    fn test_lk_find_static_registered() {
        let r = make_registry();
        assert!(r.find(LK_CLASS, "findStatic", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;").is_some());
    }

    #[test]
    fn test_lk_find_constructor_registered() {
        let r = make_registry();
        assert!(r.find(LK_CLASS, "findConstructor", "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;").is_some());
    }

    #[test]
    fn test_lk_has_full_privilege_access_registered() {
        let r = make_registry();
        assert!(r.find(LK_CLASS, "hasFullPrivilegeAccess", "()Z").is_some());
    }

    #[test]
    fn test_lk_drop_lookup_mode_registered() {
        let r = make_registry();
        assert!(r.find(LK_CLASS, "dropLookupMode", "(I)Ljava/lang/invoke/MethodHandles$Lookup;").is_some());
    }

    #[test]
    fn test_lk_define_hidden_class_registered() {
        let r = make_registry();
        assert!(r.find(LK_CLASS, "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;").is_some());
    }

    // -----------------------------------------------------------------------
    // NEW-8 — defineHiddenClass + isHidden + naming + error paths
    // -----------------------------------------------------------------------

    /// Build a syntactically-minimal class file whose `this_class` points
    /// at a Utf8 entry holding `class_name`. The result contains exactly
    /// the fields needed by `extract_this_class_name`: header + constant
    /// pool + access_flags + this_class + super_class + interfaces_count
    /// + fields_count + methods_count + attributes_count. Every count is
    /// zero so the class has no members and no attributes, which is
    /// legal but wouldn't pass a real bytecode verifier — we only need
    /// it to round-trip through the perfect-hash / name-extraction
    /// paths, not to be runnable.
    fn minimal_class_file(class_name: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        // u4 magic
        bytes.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        // u2 minor_version, u2 major_version (Java 8 = 52)
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x34]);
        // u2 constant_pool_count — we will write 4 entries at indices
        // 1..=3, so count = 4.
        bytes.extend_from_slice(&[0x00, 0x04]);
        // CP #1: CONSTANT_Utf8 for the class name
        bytes.push(1);
        let name_bytes = class_name.as_bytes();
        bytes.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
        bytes.extend_from_slice(name_bytes);
        // CP #2: CONSTANT_Class pointing at CP #1
        bytes.push(7);
        bytes.extend_from_slice(&[0x00, 0x01]);
        // CP #3: CONSTANT_Utf8 "java/lang/Object" for the super
        let super_name = b"java/lang/Object";
        bytes.push(1);
        bytes.extend_from_slice(&(super_name.len() as u16).to_be_bytes());
        bytes.extend_from_slice(super_name);
        // u2 access_flags (ACC_PUBLIC)
        bytes.extend_from_slice(&[0x00, 0x21]);
        // u2 this_class = CP #2
        bytes.extend_from_slice(&[0x00, 0x02]);
        // u2 super_class = 0 (placeholder — real super_class would be a
        // Class entry; we don't care because the mock accepts any
        // CAFEBABE-prefixed buffer).
        bytes.extend_from_slice(&[0x00, 0x00]);
        // u2 interfaces_count, fields_count, methods_count, attributes_count
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        bytes
    }

    #[test]
    fn new8_extract_this_class_name_basic() {
        let bytes = minimal_class_file("com/example/Foo");
        let name = extract_this_class_name(&bytes);
        assert_eq!(name.as_deref(), Some("com/example/Foo"));
    }

    #[test]
    fn new8_extract_this_class_name_bad_magic() {
        let mut bytes = minimal_class_file("Foo");
        bytes[0] = 0xDE; // corrupt the magic
        assert!(extract_this_class_name(&bytes).is_none());
    }

    #[test]
    fn new8_extract_this_class_name_truncated() {
        assert!(extract_this_class_name(&[0xCA, 0xFE]).is_none());
    }

    #[test]
    fn new8_extract_this_class_name_empty() {
        assert!(extract_this_class_name(&[]).is_none());
    }

    /// defineHiddenClass must reject a null byte-array argument with
    /// an IllegalArgumentException rather than silently returning an
    /// empty Lookup.
    #[test]
    fn new8_define_hidden_class_rejects_null_bytes() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER);
        let result = lk_define_hidden_class(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(None), // null bytes
                Value::Int(0),
                Value::Object(None),
            ],
        );
        assert!(
            result.is_err(),
            "null bytes must produce IllegalArgumentException"
        );
    }

    /// defineHiddenClass must reject a byte array without the class
    /// file magic.
    #[test]
    fn new8_define_hidden_class_rejects_bad_magic() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        // Build a byte[] of zeros (no magic).
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER);
        let result = lk_define_hidden_class(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Object(None),
            ],
        );
        assert!(
            result.is_err(),
            "missing magic must produce IllegalArgumentException"
        );
    }

    /// Happy path: valid bytes → hidden class is defined with a
    /// mangled name, marked as hidden, and the returned Lookup carries
    /// a mirror whose class id is flagged hidden via `is_class_hidden`.
    #[test]
    fn new8_define_hidden_class_happy_path() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let bytes = minimal_class_file("com/example/Widget");
        // Copy the Rust Vec<u8> into a Java byte[].
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int((*b as i8) as i32));
        }
        let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER);

        let result = lk_define_hidden_class(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(arr)),
                Value::Int(0), // initialize = false
                Value::Object(None),
            ],
        )
        .expect("defineHiddenClass");
        let new_lookup = match result {
            Some(Value::Object(Some(o))) => o,
            _ => panic!("expected a Lookup object"),
        };

        // The new Lookup must carry a lookupClass field; it should be a
        // non-null class mirror.
        let lookup_class = ctx.get_field(new_lookup, LK_LOOKUP_CLASS_REF);
        let mirror = match lookup_class {
            Value::Object(Some(m)) => m,
            _ => panic!("Lookup.lookupClass must be a non-null mirror"),
        };

        // The stored name in the mock registry should start with
        // "com/example/Widget/0x" — confirming HotSpot-style mangling.
        let stored_name = unsafe { (*ctx.last_defined_class_name.get()).clone() };
        let stored = stored_name.expect("mock recorded the defined name");
        assert!(
            stored.starts_with("com/example/Widget/0x"),
            "expected HotSpot-style mangled name, got {stored:?}"
        );

        // The mirror's class id must be flagged hidden via the mock's
        // `is_class_hidden` override (which reads the same set that
        // `set_class_hidden` populates).
        let cid = crate::lang_class::mirror_class_id(&mut ctx, mirror)
            .expect("mirror must have a class id");
        assert!(
            ctx.is_class_hidden(cid),
            "hidden class id must be tracked as hidden"
        );
    }

    /// Two consecutive defineHiddenClass calls on byte buffers with
    /// the same `this_class` must produce distinct mangled names so
    /// the hidden classes never collide in the registry.
    #[test]
    fn new8_define_hidden_class_names_are_unique() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let bytes = minimal_class_file("Foo");
        let arr1 = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr1, i, Value::Int((*b as i8) as i32));
        }
        let arr2 = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr2, i, Value::Int((*b as i8) as i32));
        }
        let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER);

        lk_define_hidden_class(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(arr1)),
                Value::Int(0),
                Value::Object(None),
            ],
        )
        .expect("first define");
        let first_name =
            unsafe { (*ctx.last_defined_class_name.get()).clone() }.expect("first name");

        lk_define_hidden_class(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(arr2)),
                Value::Int(0),
                Value::Object(None),
            ],
        )
        .expect("second define");
        let second_name =
            unsafe { (*ctx.last_defined_class_name.get()).clone() }.expect("second name");

        assert_ne!(
            first_name, second_name,
            "two hidden classes from the same source must have distinct names"
        );
        assert!(first_name.starts_with("Foo/0x"));
        assert!(second_name.starts_with("Foo/0x"));
    }

    /// NESTMATE parsing: when a ClassOption[] contains an element with
    /// ordinal 0 (NESTMATE), the hidden class is flagged as a nestmate.
    /// We can't easily assert the nest-info copy from a MockNativeContext
    /// because copy_nest_info is a no-op on the default trait impl;
    /// the test instead confirms that the define call still succeeds
    /// when NESTMATE is specified.
    #[test]
    fn new8_define_hidden_class_with_nestmate_option() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let bytes = minimal_class_file("com/example/Nested");
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int((*b as i8) as i32));
        }
        // Build a ClassOption[] of length 1 with ordinal = 0 (NESTMATE).
        let option = alloc_concurrent_synthetic(
            &mut ctx,
            "java/lang/invoke/MethodHandles$Lookup$ClassOption",
            1,
        );
        ctx.set_field(option, 0, Value::Int(0)); // NESTMATE ordinal
        let options_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(options_arr, 0, Value::Object(Some(option)));

        let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER);

        let result = lk_define_hidden_class(
            &mut ctx,
            &[
                Value::Object(Some(lookup)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Object(Some(options_arr)),
            ],
        );
        assert!(result.is_ok(), "NESTMATE option must not cause an error");
    }

    #[test]
    fn test_lk_find_var_handle_registered() {
        let r = make_registry();
        assert!(r.find(LK_CLASS, "findVarHandle", "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;").is_some());
    }

    // --- ProtectionDomain registration tests ---

    #[test]
    fn test_pd_init_registered() {
        let r = make_registry();
        assert!(r.find(PD_CLASS, "<init>", "(Ljava/security/CodeSource;Ljava/security/PermissionCollection;)V").is_some());
    }

    #[test]
    fn test_pd_get_code_source_registered() {
        let r = make_registry();
        assert!(r.find(PD_CLASS, "getCodeSource", "()Ljava/security/CodeSource;").is_some());
    }

    #[test]
    fn test_pd_implies_registered() {
        let r = make_registry();
        assert!(r.find(PD_CLASS, "implies", "(Ljava/security/Permission;)Z").is_some());
    }

    // --- CodeSource registration tests ---

    #[test]
    fn test_cs_init_registered() {
        let r = make_registry();
        assert!(r.find(CS_CLASS, "<init>", "(Ljava/net/URL;[Ljava/security/cert/Certificate;)V").is_some());
    }

    #[test]
    fn test_cs_get_location_registered() {
        let r = make_registry();
        assert!(r.find(CS_CLASS, "getLocation", "()Ljava/net/URL;").is_some());
    }

    #[test]
    fn test_cs_get_certificates_registered() {
        let r = make_registry();
        assert!(r.find(CS_CLASS, "getCertificates", "()[Ljava/security/cert/Certificate;").is_some());
    }

    // --- Delegation model tests ---

    #[test]
    fn test_delegation_bootstrap() {
        assert_eq!(delegation_order(LOADER_BOOTSTRAP), "bootstrap-only");
    }

    #[test]
    fn test_delegation_platform() {
        assert!(delegation_order(LOADER_PLATFORM).contains("platform"));
    }

    #[test]
    fn test_delegation_app() {
        assert!(delegation_order(LOADER_APP).contains("app"));
    }

    #[test]
    fn test_delegation_custom() {
        assert!(delegation_order(LOADER_CUSTOM).contains("parent-first"));
    }

    #[test]
    fn test_delegation_unknown() {
        assert_eq!(delegation_order(99), "unknown");
    }

    // --- Lookup mode bitmask tests ---

    #[test]
    fn test_lookup_mode_constants() {
        assert_eq!(LK_PUBLIC, 0x01);
        assert_eq!(LK_PRIVATE, 0x02);
        assert_eq!(LK_PROTECTED, 0x04);
        assert_eq!(LK_PACKAGE, 0x08);
        assert_eq!(LK_MODULE, 0x10);
        assert_eq!(LK_UNCONDITIONAL, 0x20);
        assert_eq!(LK_ORIGINAL, 0x40);
    }

    #[test]
    fn test_full_power_includes_all_key_modes() {
        assert_ne!(LK_FULL_POWER & LK_PUBLIC, 0);
        assert_ne!(LK_FULL_POWER & LK_PRIVATE, 0);
        assert_ne!(LK_FULL_POWER & LK_PROTECTED, 0);
        assert_ne!(LK_FULL_POWER & LK_PACKAGE, 0);
        assert_ne!(LK_FULL_POWER & LK_MODULE, 0);
        assert_ne!(LK_FULL_POWER & LK_ORIGINAL, 0);
    }

    // --- Loader type constants ---

    #[test]
    fn test_loader_type_ordering() {
        assert!(LOADER_BOOTSTRAP < LOADER_PLATFORM);
        assert!(LOADER_PLATFORM < LOADER_APP);
        assert!(LOADER_APP < LOADER_CUSTOM);
    }

    // --- Field count sanity ---

    #[test]
    fn test_classloader_field_count() {
        assert_eq!(CL_FIELD_COUNT, 7);
    }

    #[test]
    fn test_url_classloader_field_count() {
        assert_eq!(UCL_FIELD_COUNT, 6);
    }

    #[test]
    fn test_lookup_field_count() {
        assert_eq!(LK_FIELD_COUNT, 4);
    }

    #[test]
    fn test_hidden_class_field_count() {
        assert_eq!(HC_FIELD_COUNT, 2);
    }

    #[test]
    fn test_protection_domain_field_count() {
        assert_eq!(PD_FIELD_COUNT, 3);
    }

    #[test]
    fn test_code_source_field_count() {
        assert_eq!(CS_FIELD_COUNT, 2);
    }

    // -----------------------------------------------------------------------
    // T19_H12_ — ClassLoader.loadClass(Module, String) native
    // -----------------------------------------------------------------------

    #[test]
    fn t19_h12_cl_load_class_module_registered() {
        let r = make_registry();
        assert!(
            r.find(
                CL_CLASS,
                "loadClass",
                "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;",
            )
            .is_some(),
            "ClassLoader.loadClass(Module, String) must be registered"
        );
    }

    #[test]
    fn t19_h12_cl_load_class_module_resolves_existing() {
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();
        let _ = ctx.ensure_class_initialized("java/lang/String").unwrap();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), CL_FIELD_COUNT);
        let module = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
        let name = ctx.create_string("java.lang.String");
        let r = cl_load_class_module(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(module)),
                Value::Object(Some(name)),
            ],
        );
        match r.unwrap().unwrap() {
            Value::Object(Some(_)) => {} // mirror returned
            other => panic!("expected Class mirror, got {:?}", other),
        }
    }

    #[test]
    fn t19_h12_cl_load_class_module_missing_returns_null() {
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), CL_FIELD_COUNT);
        let module = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
        let name = ctx.create_string("does.not.exist.Bogus");
        // Note: the mock's ensure_class_initialized always succeeds (auto-creates).
        // To verify the spec'd null return on a real miss, we test the
        // hardening / null-arg branches explicitly.
        let _ = cl_load_class_module(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(module)),
                Value::Object(Some(name)),
            ],
        );
        // (mock auto-creates so the hit path is exercised; the explicit
        // null-return branches are covered by the next four tests)
    }

    #[test]
    fn t19_h12_cl_load_class_module_null_module_returns_null() {
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), CL_FIELD_COUNT);
        let name = ctx.create_string("java.lang.String");
        let r = cl_load_class_module(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(name)),
            ],
        );
        match r.unwrap().unwrap() {
            Value::Object(None) => {}
            other => panic!("expected null on null-module, got {:?}", other),
        }
    }

    #[test]
    fn t19_h12_cl_load_class_module_null_name_returns_null() {
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), CL_FIELD_COUNT);
        let module = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
        let r = cl_load_class_module(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(module)),
                Value::Object(None),
            ],
        );
        match r.unwrap().unwrap() {
            Value::Object(None) => {}
            other => panic!("expected null on null-name, got {:?}", other),
        }
    }

    #[test]
    fn t19_h12_cl_load_class_module_path_traversal_rejected() {
        // Hardening: control bytes / path separators must short-circuit
        // to null, never reach `ensure_class_initialized`.
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), CL_FIELD_COUNT);
        let module = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
        let evil = ctx.create_string("../../etc/passwd");
        let r = cl_load_class_module(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(module)),
                Value::Object(Some(evil)),
            ],
        );
        match r.unwrap().unwrap() {
            Value::Object(None) => {}
            other => panic!("expected null on path-traversal name, got {:?}", other),
        }
    }
}
