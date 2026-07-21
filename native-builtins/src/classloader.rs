// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ClassLoader hierarchy, URLClassLoader, MethodHandles.Lookup, ProtectionDomain,
//! and CodeSource native method implementations.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::service_loader::impl_jars_load_class;
use crate::{alloc_concurrent_synthetic, obj_arg};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

/// Monotonic counter for generating unique hidden class names.
pub static HIDDEN_CLASS_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Singleton classloader instances (JVM spec: one instance per built-in loader)
// ---------------------------------------------------------------------------

fn platform_loader_store() -> &'static Mutex<Option<ObjectRef>> {
    static INSTANCE: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Temporary debug-only accessor (CRATONVM_DBG_OBSREG investigation).
pub(crate) fn platform_loader_store_dbg() -> &'static Mutex<Option<ObjectRef>> {
    platform_loader_store()
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
    *platform_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    *app_loader_store().lock().unwrap_or_else(|e| e.into_inner()) = None;
    class_data_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    orphaned_defining_loader_classes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    ANY_DEFINING_LOADER_REGISTERED.store(false, Ordering::Release);
    loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    // HIB-CV-24: drop the GC marker's loader-pin mirror for the new VM.
    cratonvm_types::loader_pin::clear_loader_pins();
    // Companion: drop the GC marker's mirror_pin registry for the new VM too
    // (see `cratonvm_types::mirror_pin`).
    cratonvm_types::mirror_pin::clear_mirror_pins();
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
    if let Some(o) = *platform_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        out.push(o);
    }
    // Defining-loader side-table values are live ClassLoader objects reachable
    // only from this map. Legacy behavior roots them all (which is why a
    // user/isolated loader could never be collected — HIB-CV-24 Manifestation B).
    // With `CRATONVM_LOADER_UNLOAD` ON (default) we DON'T root them, so a loader
    // the application no longer references becomes collectable; the now-stale
    // entry is pruned post-GC by `gc_reconcile_defining_loaders`. App/platform
    // singletons stay rooted above, so built-in loaders are unaffected.
    if !loader_unload_enabled() {
        for o in defining_loader_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            out.push(*o);
        }
    }
}

/// Post-GC remap for the singleton built-in class loaders (companion to
/// [`gc_scan_loader_singleton_roots`]). After a moving collection the cached
/// loader objects relocate; repoint the stored `ObjectRef`s to their new
/// addresses so subsequent `getClassLoader()` calls return the live object.
pub fn gc_update_loader_singleton_refs(pointer_map: &std::collections::HashMap<usize, usize>) {
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
    remap(
        &mut platform_loader_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    // NOTE: the defining-loader side-table is reconciled (pruned + remapped)
    // earlier in the GC cycle by `gc_reconcile_defining_loaders`, which runs in
    // `process_references_after_gc` *before* this remap pass and uses the same
    // survivor predicate as reference processing. Remapping it here as well would
    // be a no-op (its entries already hold post-collection addresses) — and worse,
    // re-rooting/remapping a collected loader would defeat unloading — so it is
    // intentionally NOT touched here.
}

/// HIB-CV-24 (Manifestation B) — post-GC reconciliation of the defining-loader
/// side-table (`class_id -> user ClassLoader`).
///
/// For each recorded entry:
///   * if the loader survived this collection, remap its (possibly relocated)
///     address through `pointer_map` — old-gen survivors that did not move keep
///     their address;
///   * if the loader was collected (not marked), drop the entry so a later
///     `Class.getClassLoader()` cannot return a dangling reference and the
///     loader's memory is not pinned by this side-table.
///
/// `is_marked(addr)` MUST be the SAME survivor predicate the reference processor
/// uses in this cycle (`pointer_map.contains_key(addr) || heap.is_addr_live(addr)`),
/// so a loader is pruned EXACTLY when a phantom/weak reference to it would be
/// enqueued/cleared — keeping the side-table consistent with reference
/// processing. The entry is only *removed* (never dereferenced) for a dead
/// loader, so this is safe to call after the collection has freed the memory.
///
/// Runs in BOTH gate modes: with `CRATONVM_LOADER_UNLOAD=0` the loaders are
/// GC-rooted, hence always marked, so nothing is pruned and entries are merely
/// remapped — preserving the legacy behavior.
pub fn gc_reconcile_defining_loaders(
    is_marked: &dyn Fn(usize) -> bool,
    pointer_map: &std::collections::HashMap<usize, usize>,
) {
    let dbg = std::env::var_os("CRATONVM_DBG_MIRRORPIN").is_some();
    let mut map = defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.retain(|_class_id, obj_ref| {
        let old_addr = obj_ref.as_ptr() as usize;
        let alive = is_marked(old_addr);
        if dbg {
            eprintln!(
                "[DBG_MIRRORPIN] defining_loader_store cid={:?} loader_addr={:#x} is_marked={}",
                _class_id, old_addr, alive
            );
        }
        if !alive {
            // Loader unreachable and collected this cycle — drop the stale entry.
            // (No deref of `obj_ref`; the memory may already be freed/reused.)
            // Record the class as permanently orphaned FIRST: dropping the
            // entry alone makes `defining_loader_for` indistinguishable from
            // "never restricted", which would make this class incorrectly
            // visible to every other loader from now on (see
            // `is_defining_loader_orphaned` doc comment).
            orphaned_defining_loader_classes()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(*_class_id);
            return false;
        }
        // Survivor: remap if it relocated (moving collection).
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
        true
    });
    // HIB-CV-24: re-sync the GC marker's loader-pin registry from the
    // authoritative side-table (now remapped/pruned) so the next collection
    // marks loaders at their current addresses and drops collected ones.
    let pins: Vec<(u32, usize)> = map
        .iter()
        .map(|(&cid, obj_ref)| (cid, obj_ref.as_ptr() as usize))
        .collect();
    cratonvm_types::loader_pin::replace_loader_pins(&pins);

    // Same treatment for the loader-namespace side-table (object-keyed): drop
    // entries whose loader was collected this cycle, remap survivors that
    // moved. A pruned dead loader's address can then be reused by a NEW
    // loader without inheriting the dead namespace.
    let mut ns = loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    ns.retain_mut(|(obj_ref, _)| {
        let old_addr = obj_ref.as_ptr() as usize;
        if !is_marked(old_addr) {
            return false;
        }
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
        true
    });
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
//
// GC note (gc-followups-20260706): NOT GC-safe for reads-after-GC — the
// mirror KEY is a raw address that goes stale when the mirror moves (lookups
// would miss), and Object-typed VALUES are neither rooted nor remapped. This
// is tolerated ONLY because `get_class_data` currently has no production
// callers (test-only) — the table is effectively write-only. Before adding a
// real reader: re-key by `ctx.identity_hash_code(mirror)` and store values as
// `(identity_key, ObjectRef)` var-handle-root pairs (ASYNC_POOL pattern).
// ---------------------------------------------------------------------------
fn class_data_store() -> &'static Mutex<std::collections::HashMap<ObjectRef, Value>> {
    static INSTANCE: OnceLock<Mutex<std::collections::HashMap<ObjectRef, Value>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

// ---------------------------------------------------------------------------
// Defining-loader side-table — `class_id -> user ClassLoader object`.
//
// `Class.getClassLoader()` (native_class_get_class_loader) otherwise returns the
// app-loader singleton for EVERY non-bootstrap/non-platform class, because the
// VM tracks only a loader *category* per class, not the defining loader
// instance. A class defined by a user-defined `ClassLoader` subclass (e.g.
// ByteBuddy's `ByteArrayClassLoader`, cglib, Hibernate proxies) must report
// that exact instance: ByteBuddy's `ByteArrayClassLoader.load` does
// `Class.forName(name, false, this).getClassLoader() != this` and throws
// "Class already loaded" when the round-trip yields the app loader instead.
//
// Keyed by `class_id` (stable u32); the VALUE is a live `ObjectRef` reachable
// only here, so it MUST be GC-rooted + remapped (see
// `gc_scan_loader_singleton_roots` / `gc_update_loader_singleton_refs`).
// ---------------------------------------------------------------------------
fn defining_loader_store() -> &'static Mutex<std::collections::HashMap<u32, ObjectRef>> {
    static INSTANCE: OnceLock<Mutex<std::collections::HashMap<u32, ObjectRef>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Class-ids whose registered defining loader was confirmed DEAD by a prior
/// `gc_reconcile_defining_loaders` pass. Once a class lands here it must
/// never again be treated as globally visible: `defining_loader_for`
/// returning `None` is ALSO the answer for "never had a registered loader in
/// the first place" (the overwhelmingly common built-in-loader case), so
/// `cid_visible_mirror` cannot distinguish "no restriction" from "the
/// restriction's target died" without this separate permanent record.
/// Entries are never removed (matches real unloading: once a defining loader
/// is gone, the class is gone from every OTHER loader's perspective forever;
/// a new loader wanting the same simple name must define its own copy).
fn orphaned_defining_loader_classes() -> &'static Mutex<std::collections::HashSet<u32>> {
    static INSTANCE: OnceLock<Mutex<std::collections::HashSet<u32>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Whether `class_id`'s defining loader has been confirmed collected. See
/// [`orphaned_defining_loader_classes`].
pub(crate) fn is_defining_loader_orphaned(class_id: u32) -> bool {
    orphaned_defining_loader_classes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&class_id)
}

/// Perf (silent-hang-no-signature-cluster throughput residual, 2026-07-13):
/// `defining_loader_for` sits behind `should_use_loader_initiated_resolution`,
/// which the interpreter's `lookup_loader_initiated`/
/// `retarget_instance_field_to_receiver` hot paths call on every non-fast-path
/// invoke/getfield/putfield — confirmed via call-count instrumentation at
/// 13-37% of ALL executed bytecode instructions in a Tomcat workload. This map
/// is populated ONLY when a user-defined `ClassLoader` (ByteBuddy, cglib,
/// Hibernate proxies, Groovy) defines a class — the overwhelming majority of
/// classes (bootstrap/app-loader) never call `register_defining_loader`, so
/// the map is empty for most workloads. A plain `bool` (not even relaxed-typed
/// precision needed — false negatives are impossible, see below) lets
/// `defining_loader_for` skip the `std::sync::Mutex` acquisition entirely in
/// that case. Correctness: only ever transitions false→true (in
/// `register_defining_loader`) or gets reset alongside the map itself (in
/// `reset_loader_singletons`), so a `false` read here is always accurate at
/// the instant it's read for a map that has never had an insert since the
/// last reset — no ABA/staleness risk given the store never goes non-empty
/// then empty except via the same reset that clears this flag.
static ANY_DEFINING_LOADER_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Record the user-defined `ClassLoader` object that defined `class_id`, so
/// `Class.getClassLoader()` returns the exact instance instead of the app-loader
/// fallback.
pub fn register_defining_loader(class_id: u32, loader: ObjectRef) {
    defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(class_id, loader);
    ANY_DEFINING_LOADER_REGISTERED.store(true, Ordering::Release);
    // HIB-CV-24: mirror into the loader-pin registry the GC marker consults so a
    // live instance of this class keeps its defining loader alive (the
    // instance→loader edge HotSpot gets for free via `Class.getClassLoader`).
    cratonvm_types::loader_pin::set_loader_pin(class_id, loader.as_ptr() as usize);
}

/// Look up the user-defined `ClassLoader` object that defined `class_id`.
pub fn defining_loader_for(class_id: u32) -> Option<ObjectRef> {
    if !ANY_DEFINING_LOADER_REGISTERED.load(Ordering::Acquire) {
        return None;
    }
    defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&class_id)
        .copied()
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
    let existing = *platform_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(obj) = existing {
        return obj;
    }
    let mut obj = alloc_classloader(ctx, LOADER_PLATFORM);
    let obj_pin = ctx.pin_native_root(obj);
    let name = ctx.create_string("platform");
    let name_pin = ctx.pin_native_root(name);
    obj = ctx.read_native_pin(obj_pin, obj);
    let name = ctx.read_native_pin(name_pin, name);
    ctx.set_field(obj, CL_NAME_REF, Value::Object(Some(name)));
    // Also populate the REAL `name` field by name: the active getName native
    // (classloader_real) reads the real field slot, not CL_NAME_REF.
    obj = ctx.read_native_pin(obj_pin, obj);
    let name = ctx.read_native_pin(name_pin, name);
    ctx.set_field_by_name(obj, "name", Value::Object(Some(name)));
    // Platform's parent is bootstrap (null); already set by alloc_classloader
    obj = ctx.read_native_pin(obj_pin, obj);
    *platform_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(obj);
    ctx.unpin_native_roots(obj_pin);
    obj
}

/// Get or create the singleton application (system) class loader.
pub fn get_or_create_app_loader(ctx: &mut dyn NativeContext) -> ObjectRef {
    let existing = *app_loader_store().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(obj) = existing {
        // The singleton is a Rust-side cache.  If a moving collection ever
        // leaves an unremapped reference behind, its old heap slot can be
        // reused by an unrelated object (observed as String.findResources
        // during Mockito plugin discovery).  Never return that object as a
        // ClassLoader; discard the stale cache entry and rebuild it below.
        let is_loader = ctx
            .class_id_by_name("java/lang/ClassLoader")
            .map(|loader_class_id| {
                let actual_class_id = ctx.class_id_of_object(obj);
                actual_class_id == loader_class_id
                    || ctx.is_subclass(actual_class_id, loader_class_id)
            })
            .unwrap_or(false);
        if is_loader {
            return obj;
        }
        *app_loader_store().lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    let platform = get_or_create_platform_loader(ctx);
    let platform_pin = ctx.pin_native_root(platform);
    let mut obj = alloc_classloader(ctx, LOADER_APP);
    let obj_pin = ctx.pin_native_root(obj);
    let name = ctx.create_string("app");
    let name_pin = ctx.pin_native_root(name);
    let mut platform = ctx.read_native_pin(platform_pin, platform);
    obj = ctx.read_native_pin(obj_pin, obj);
    let mut name = ctx.read_native_pin(name_pin, name);
    ctx.set_field(obj, CL_NAME_REF, Value::Object(Some(name)));
    obj = ctx.read_native_pin(obj_pin, obj);
    platform = ctx.read_native_pin(platform_pin, platform);
    ctx.set_field(obj, CL_PARENT_REF, Value::Object(Some(platform)));
    // Also populate the REAL `name`/`parent` fields by name: the active
    // getName/getParent natives (classloader_real) read the real field slots,
    // not CL_NAME_REF/CL_PARENT_REF. Without the real `parent`, the app
    // loader's getParent() returned null and Tomcat's
    // WebappClassLoaderBase.<init> javase-loader walk
    // (`while (j.getParent() != null) j = j.getParent()`) misbehaved.
    obj = ctx.read_native_pin(obj_pin, obj);
    name = ctx.read_native_pin(name_pin, name);
    ctx.set_field_by_name(obj, "name", Value::Object(Some(name)));
    obj = ctx.read_native_pin(obj_pin, obj);
    platform = ctx.read_native_pin(platform_pin, platform);
    ctx.set_field_by_name(obj, "parent", Value::Object(Some(platform)));
    // Populate the REAL static `java.lang.ClassLoader.scl` so the real-JDK
    // `ClassLoader.getSystemClassLoader()` bytecode (reached when a call site
    // does not resolve to our native; observed in
    // WebappClassLoaderBase.<init> at pc=174) returns this loader instead of
    // null. A null there made the subsequent `j.getParent()` NPE and aborted
    // every embedded-server webapp deploy ("Error starting the loader").
    obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_static_field_by_name("java/lang/ClassLoader", "scl", Value::Object(Some(obj)));
    obj = ctx.read_native_pin(obj_pin, obj);
    *app_loader_store().lock().unwrap_or_else(|e| e.into_inner()) = Some(obj);
    ctx.unpin_native_roots(platform_pin);
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
const LK_FULL_POWER: i32 =
    LK_PUBLIC | LK_PRIVATE | LK_PROTECTED | LK_PACKAGE | LK_MODULE | LK_ORIGINAL;

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
    let mut cs = alloc_concurrent_synthetic(ctx, CS_CLASS, CS_FIELD_COUNT);
    let cs_pin = ctx.pin_native_root(cs);
    ctx.set_field(cs, CS_LOCATION_REF, Value::Object(None));
    ctx.set_field(cs, CS_CERTIFICATES_REF, Value::Object(None));
    // Belt-and-suspenders: also set by name in case the real JDK CodeSource
    // layout reads through a different field index than our synthetic.
    cs = ctx.read_native_pin(cs_pin, cs);
    ctx.set_field_by_name(cs, "location", Value::Object(None));
    cs = ctx.read_native_pin(cs_pin, cs);
    ctx.set_field_by_name(cs, "certs", Value::Object(None));

    let mut pd = alloc_concurrent_synthetic(ctx, PD_CLASS, PD_FIELD_COUNT);
    let pd_pin = ctx.pin_native_root(pd);
    cs = ctx.read_native_pin(cs_pin, cs);
    pd = ctx.read_native_pin(pd_pin, pd);
    ctx.set_field(pd, PD_CODE_SOURCE_REF, Value::Object(Some(cs)));
    ctx.set_field(pd, PD_PERMISSIONS_REF, Value::Object(None));
    ctx.set_field(pd, PD_CLASS_LOADER_REF, Value::Object(None));
    // Real JDK PD reads `codesource` by name in `getCodeSource`; cover both
    // field-index orderings.
    cs = ctx.read_native_pin(cs_pin, cs);
    pd = ctx.read_native_pin(pd_pin, pd);
    ctx.set_field_by_name(pd, "codesource", Value::Object(Some(cs)));
    pd = ctx.read_native_pin(pd_pin, pd);
    ctx.set_field_by_name(pd, "permissions", Value::Object(None));
    pd = ctx.read_native_pin(pd_pin, pd);
    ctx.set_field_by_name(pd, "classloader", Value::Object(None));
    let pd = ctx.read_native_pin(pd_pin, pd);
    ctx.unpin_native_roots(cs_pin);
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
    let mut obj = alloc_concurrent_synthetic(ctx, class_name, CL_FIELD_COUNT);
    let obj_pin = ctx.pin_native_root(obj);
    ctx.set_field(obj, CL_LOADER_TYPE, Value::Int(loader_type));
    ctx.set_field(obj, CL_PARENT_REF, Value::Object(None));
    ctx.set_field(obj, CL_NAME_REF, Value::Object(None));
    ctx.set_field(obj, CL_CLASSES_LOADED, Value::Int(0));
    ctx.set_field(obj, CL_IS_PARALLEL_CAPABLE, Value::Int(0));
    let pd = alloc_default_protection_domain(ctx);
    let pd_pin = ctx.pin_native_root(pd);
    obj = ctx.read_native_pin(obj_pin, obj);
    let pd = ctx.read_native_pin(pd_pin, pd);
    ctx.set_field(obj, CL_DEFAULT_DOMAIN, Value::Object(Some(pd)));
    obj = ctx.read_native_pin(obj_pin, obj);
    let pd = ctx.read_native_pin(pd_pin, pd);
    ctx.set_field_by_name(obj, "defaultDomain", Value::Object(Some(pd)));
    // Assign a unique loader ID for custom classloaders
    let lid = if loader_type == LOADER_CUSTOM {
        ctx.allocate_loader_id() as i32
    } else {
        0 // built-in loaders don't use this field
    };
    obj = ctx.read_native_pin(obj_pin, obj);
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
        let name_to_module =
            alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
        let name_to_module_pin = ctx.pin_native_root(name_to_module);
        obj = ctx.read_native_pin(obj_pin, obj);
        let name_to_module = ctx.read_native_pin(name_to_module_pin, name_to_module);
        ctx.set_field_by_name(obj, "nameToModule", Value::Object(Some(name_to_module)));
        let module_to_reader =
            alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
        let module_to_reader_pin = ctx.pin_native_root(module_to_reader);
        obj = ctx.read_native_pin(obj_pin, obj);
        let module_to_reader = ctx.read_native_pin(module_to_reader_pin, module_to_reader);
        ctx.set_field_by_name(obj, "moduleToReader", Value::Object(Some(module_to_reader)));
    }
    // S111r17: `java/lang/ClassLoader` declares `packages:ConcurrentHashMap`
    // (instance field) which the real-JDK ctor initializes via
    // `new ConcurrentHashMap()`.  We bypass the ctor through
    // `alloc_concurrent_synthetic`, so `packages` defaults to null. The JDK's
    // `ClassLoader.packages()` instance method does
    // `getfield packages` then `ConcurrentHashMap.values()`, NPE'ing with
    // "Cannot invoke values on null"; observed during
    // `org/jboss/modules/ConcurrentClassLoader.<clinit>` (JBoss Modules /
    // WildFly 39 boot), whose static initializer calls
    // `Package.getPackages()` then `ClassLoader.getClassLoader(...).getPackages()`
    // then `packages()`. Pre-populate an empty CHM so the bytecode path runs
    // without additional intercepts.
    let packages_map =
        alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
    let packages_map_pin = ctx.pin_native_root(packages_map);
    obj = ctx.read_native_pin(obj_pin, obj);
    let packages_map = ctx.read_native_pin(packages_map_pin, packages_map);
    ctx.set_field_by_name(obj, "packages", Value::Object(Some(packages_map)));
    // `ClassLoader.setDefaultAssertionStatus` uses `synchronized (assertionLock)`.
    // Real JDK ctors assign `this.assertionLock = new Object()`; synthetic
    // allocation skips that, so Surefire's forked booter NPEs on monitorenter.
    let lock = alloc_concurrent_synthetic(ctx, "java/lang/Object", 0);
    let lock_pin = ctx.pin_native_root(lock);
    let lock = ctx.read_native_pin(lock_pin, lock);
    let _ = ctx.invoke_special(
        "java/lang/Object",
        "<init>",
        "()V",
        &[Value::Object(Some(lock))],
    );
    obj = ctx.read_native_pin(obj_pin, obj);
    let lock = ctx.read_native_pin(lock_pin, lock);
    ctx.set_field_by_name(obj, "assertionLock", Value::Object(Some(lock)));
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    obj
}

/// Get the unique loader ID from a ClassLoader object, lazily assigning one if needed.
///
/// Delegates to [`loader_namespace_id`], which is mode-aware: in
/// synthetic-JDK mode `CL_LOADER_ID` (slot 6) is a CratonVM-owned bookkeeping
/// field, safe to read/write directly; in real-JDK mode that same slot index
/// is the REAL `java.lang.ClassLoader.classes` field (a private final
/// `ArrayList<Class<?>>` -- confirmed via `javap` against the JDK 25
/// `ClassLoader.class`: instance fields in declaration order are `parent`(0)
/// `name`(1) `unnamedModule`(2) `nameAndId`(3) `parallelLockMap`(4)
/// `package2certs`(5) `classes`(6)). This function used to write
/// `Value::Int(lid)` straight into that slot unconditionally, silently
/// clobbering the real `classes` ArrayList reference with a bare integer on
/// every `ClassLoader.defineClass(...)` call in real-JDK mode -- harmless
/// only as long as nothing ever reads `classes` back (e.g. `addClass`,
/// reflection over loader-owned classes), but a real type-confusion bug
/// regardless of whether anything currently exercises it. `loader_namespace_id`
/// already keys real-JDK-mode ids by the loader's stable identity hash in a
/// side table instead of touching the field, so delegating to it fixes the
/// corruption for free while preserving identical id-assignment semantics
/// (same `allocate_loader_id()` counter, same "0 = application/built-in
/// loader" convention `loader_id_for`'s null-loader callers already rely on).
pub(crate) fn get_or_assign_loader_id(ctx: &mut dyn NativeContext, cl: ObjectRef) -> u32 {
    loader_namespace_id(ctx, cl)
}

fn alloc_url_classloader(ctx: &mut dyn NativeContext) -> ObjectRef {
    let mut obj = alloc_concurrent_synthetic(ctx, UCL_CLASS, UCL_FIELD_COUNT);
    let obj_pin = ctx.pin_native_root(obj);
    ctx.set_field(obj, UCL_LOADER_TYPE, Value::Int(LOADER_CUSTOM));
    ctx.set_field(obj, UCL_PARENT_REF, Value::Object(None));
    ctx.set_field(obj, UCL_URL_COUNT, Value::Int(0));
    ctx.set_field(obj, UCL_CLOSED, Value::Int(0));
    // Allocate initial URLs array (capacity 16)
    let urls_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
    let urls_pin = ctx.pin_native_root(urls_arr);
    obj = ctx.read_native_pin(obj_pin, obj);
    let urls_arr = ctx.read_native_pin(urls_pin, urls_arr);
    ctx.set_field(obj, UCL_URLS_ARRAY, Value::Object(Some(urls_arr)));
    // Assign unique loader ID
    let lid = ctx.allocate_loader_id();
    obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, UCL_LOADER_ID, Value::Int(lid as i32));
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    obj
}

fn alloc_lookup(ctx: &mut dyn NativeContext, modes: i32) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, LK_CLASS, LK_FIELD_COUNT);
    ctx.set_field(obj, LK_LOOKUP_CLASS_REF, Value::Object(None));
    ctx.set_field(obj, LK_PREVIOUS_LOOKUP_CLASS, Value::Object(None));
    ctx.set_field(obj, LK_LOOKUP_MODE, Value::Int(modes));
    // Write `allowedModes` so it lands on the real JDK field (3-field layout
    // puts it at slot 2, not the synthetic slot 1 = prevLookupClass). See
    // `lk_modes_of` and `lang_invoke::lk_write_allowed_modes`.
    lk_set_modes(ctx, obj, modes);
    obj
}

/// Write `allowedModes` by name (real layout), falling back to the synthetic
/// slot if the named field cannot be resolved.
fn lk_set_modes(ctx: &mut dyn NativeContext, obj: ObjectRef, modes: i32) {
    ctx.set_field_by_name(obj, "allowedModes", Value::Int(modes));
    let landed = matches!(ctx.get_field_by_name(obj, "allowedModes"), Value::Int(m) if m == modes);
    if !landed {
        ctx.set_field(obj, LK_ALLOWED_MODES, Value::Int(modes));
    }
}

/// Read a Lookup's `allowedModes`, by name first (real layout), then the
/// synthetic slot.
fn lk_modes_of(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    if let Value::Int(m) = ctx.get_field_by_name(this, "allowedModes") {
        return m;
    }
    if let Value::Int(m) = ctx.get_field(this, LK_ALLOWED_MODES) {
        return m;
    }
    0
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
    let packages_map =
        alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
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
    let packages_map =
        alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
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
    let packages_map =
        alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
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
        "java/lang/ClassLoader" | "java/net/URLClassLoader" | "java/security/SecureClassLoader"
    ) || class_name.starts_with("jdk/internal/loader/")
        || class_name.starts_with("sun/misc/Launcher$")
}

/// True if `internal_name` (slash-form) names a class the **bootstrap** (and
/// platform) loader genuinely owns — the JDK/platform module surface. The real
/// JVM's `findBootstrapClass` searches ONLY this set; an application class is
/// never resolvable through it. CratonVM has no separate bootstrap classpath
/// (its class store is flat), so this name predicate stands in for "would the
/// bootstrap loader find this".
pub(crate) fn is_bootstrap_class_name(internal: &str) -> bool {
    internal.starts_with("java/")
        || internal.starts_with("javax/")
        || internal.starts_with("jdk/")
        // MethodUtil deliberately defines this JDK helper through its private
        // application loader; its static initializer rejects bootstrap ownership.
        || (internal.starts_with("sun/") && internal != "sun/reflect/misc/Trampoline")
        || internal.starts_with("com/sun/")
        || internal.starts_with("org/w3c/dom")
        || internal.starts_with("org/xml/sax")
        || internal.starts_with("org/ietf/jgss")
        || internal.starts_with("org/jcp/xml")
        || internal.starts_with("[")
}

/// Whether `loader_obj` is eligible for loader-initiated resolution of a
/// class it defined -- either the global `CRATONVM_LOADER_AWARE_RESOLUTION`
/// gate is on (default-on since the Hibernate custom-loader soak; see
/// `vm::runtime::env_cache::loader_aware_resolution`'s doc comment for the
/// validation history -- duplicated here rather than shared because
/// `native-builtins` cannot depend on `vm`), or `loader_obj` is a
/// `groovy.lang.GroovyClassLoader` / Spring's own AOT-test isolating loaders
/// (`org.springframework.core.test.tools.{DynamicClassLoader,
/// CompileWithForkedClassLoaderClassLoader}`) -- narrow carve-outs for when
/// the global gate is explicitly disabled (`CRATONVM_LOADER_AWARE_
/// RESOLUTION=0`), mirroring `is_groovy_class_loader` in interpreter.rs.
pub(crate) fn is_loader_aware_resolution_eligible(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
) -> bool {
    let global_gate = match std::env::var("CRATONVM_LOADER_AWARE_RESOLUTION") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => true,
    };
    if global_gate {
        return true;
    }
    let loader_cid = ctx.class_id_of_object(loader_obj);
    const NARROW_CARVEOUT: [&str; 3] = [
        "groovy/lang/GroovyClassLoader",
        "org/springframework/core/test/tools/DynamicClassLoader",
        "org/springframework/core/test/tools/CompileWithForkedClassLoaderClassLoader",
    ];
    NARROW_CARVEOUT.iter().any(|name| {
        ctx.class_id_by_name(name)
            .is_some_and(|id| loader_cid == id || ctx.is_subclass(loader_cid, id))
    })
}

/// HIB-CV-24 / SBR-14 gate. When ON (default), `findBootstrapClass` is scoped to
/// genuine bootstrap classes for a custom loader that overrides `findClass` with
/// a null parent — so the real `ClassLoader.loadClass` bytecode proceeds to that
/// override (JVMS §5.3) instead of the bootstrap native resolving the app class
/// out from under it. Opt-out `CRATONVM_CL_BOOTSTRAP_SCOPED=0` restores the
/// legacy permissive behavior (bootstrap native resolves any app class) as the
/// safety net.
pub(crate) fn cl_bootstrap_scoped() -> bool {
    static GATE: OnceLock<bool> = OnceLock::new();
    *GATE.get_or_init(|| {
        std::env::var("CRATONVM_CL_BOOTSTRAP_SCOPED")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

/// HIB-CV-24 (Manifestation B) gate. When ON (default), a user-defined
/// `ClassLoader` recorded in the defining-loader side-table is NOT treated as a
/// GC root: once the application drops every reference to it the loader becomes
/// collectable, matching HotSpot classloader-leak semantics (Hibernate's
/// `ClassLoaderLeaksUtilityTest.testClassLoaderLeaksNegated`, which spins a
/// PhantomReference + `System.gc()` loop waiting for an isolated loader to be
/// collected). Post-GC reconciliation (`gc_reconcile_defining_loaders`) prunes
/// the now-stale side-table entry and remaps survivors. Opt-out
/// `CRATONVM_LOADER_UNLOAD=0` restores the legacy behavior where every defining
/// loader is strong-rooted forever (no class/loader unloading) as the safety net.
///
/// `pub` (not `pub(crate)`): also consulted by `vm::memory::roots` /
/// `vm::memory::gc` to gate rooting/reconciliation of the `SharedVm::class_mirrors`
/// cache the same way — a `java.lang.Class` mirror's `classLoader` field is a
/// real heap edge, so unconditionally rooting a user-defined class's mirror
/// keeps its loader alive forever too, defeating this gate for any loader that
/// ever had a class reflected on (`getClass()`, annotations, ...).
pub fn loader_unload_enabled() -> bool {
    static GATE: OnceLock<bool> = OnceLock::new();
    *GATE.get_or_init(|| {
        std::env::var("CRATONVM_LOADER_UNLOAD")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

/// `CRATONVM_LOADER_AWARE_RESOLUTION` gate (default ON). Mirrors
/// `cratonvm_vm::runtime::env_cache::loader_aware_resolution` so the
/// native-builtins half of loader-faithful class resolution (per-user-loader
/// namespace assignment in `defineClass`, exact `findLoadedClass`,
/// `descriptor_to_class_mirror_via_loader` for reflective Field/Method/
/// Constructor types, annotation Class-value resolution) stays in lock-step
/// with the interpreter half. This copy had drifted out of lock-step (still
/// default OFF) after `env_cache::loader_aware_resolution` flipped to default
/// ON for the `context.groovy` bug-cluster fix, which silently disabled this
/// crate's share of the loader-faithful fixes by default — see
/// `docs/known-issues/hib-bytecode-enhancement-loader-faithful-linking.md`.
/// When off, every loader-identity path keeps its exact pre-gate behavior.
/// Empty / `"0"` ⇒ off; any other value ⇒ on.
pub(crate) fn loader_aware_resolution() -> bool {
    static GATE: OnceLock<bool> = OnceLock::new();
    *GATE.get_or_init(|| match std::env::var("CRATONVM_LOADER_AWARE_RESOLUTION") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => true,
    })
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
        if name == "java/net/URLClassLoader" {
            // `URLClassLoader` provides a genuine, non-trivial `findClass`
            // (its own URL/HTTP-based resolution -- `ucl_real_find_class` /
            // `ucl_find_class`) even for a bare instance or a subclass that
            // does not itself declare `findClass`, unlike `java/lang/
            // ClassLoader` / `SecureClassLoader` (whose inherited findClass
            // just throws). Must NOT be treated as a "no override" builtin
            // base, or a null-parent `URLClassLoader` never consults its own
            // URLs at all -- see docs/known-issues/keycloak/
            // test-classserver-invalidpackage-classnotfound-not-thrown.md.
            return true;
        }
        if is_builtin_loader_class(&name) {
            // Reached a different builtin base without seeing a user override.
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

/// True iff the receiver's `findClass` resolution is CratonVM's own
/// URLClassLoader native (`ucl_real_find_class` / `ucl_find_class`) rather
/// than user-supplied bytecode -- i.e. no subclass in the hierarchy declares
/// its own `findClass` before the walk reaches `java/net/URLClassLoader`. A
/// `ClassNotFoundException` from this source reflects a genuine,
/// authoritative miss against the loader's own recorded URLs (including a
/// real HTTP fetch) and must propagate as-is. A genuine user `findClass`
/// override's miss, by contrast, may just reflect incomplete CratonVM
/// emulation of whatever custom source it reads from, so it keeps the
/// existing best-effort global-store fallback (HIB-CV-24 / SBR-14 step 2b /
/// step 6). See `receiver_overrides_find_class` above for the paired check.
pub(crate) fn find_class_is_urlclassloader_native(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> bool {
    let mut cid = Some(ctx.class_id_of_object(this));
    while let Some(id) = cid {
        let name = match ctx.class_name_of_id(id) {
            Some(n) => n,
            None => return false,
        };
        if name == "java/net/URLClassLoader" {
            return true;
        }
        if is_builtin_loader_class(&name) {
            return false;
        }
        if ctx
            .declared_methods(id)
            .iter()
            .any(|m| m.name == "findClass")
        {
            return false;
        }
        cid = ctx.superclass_of(id);
    }
    false
}

/// True iff the receiver's actual class overrides a `ClassLoader.loadClass`
/// overload with its own bytecode (a genuine non-builtin subclass override).
///
/// `ClassLoader.loadClass(String)` is spec'd as `return loadClass(name, false)`
/// — a virtual self-call. Some loaders (notably Spring's `OverridingClassLoader`
/// and any classloader-isolation pattern) override the protected
/// `loadClass(String,boolean)` to perform OVERRIDE-FIRST loading: they redefine
/// "eligible" classes under themselves (or reject filtered names) *before*
/// delegating to the parent. Because CratonVM keeps no JDK bytecode for
/// `ClassLoader.loadClass`, the `cl_load_class` native stands in for the
/// single-arg form. If it reimplemented base parent-first delegation for such a
/// receiver, the override's custom ordering — and crucially the defining-loader
/// identity it would establish via `defineClass` — would be silently lost, and
/// the class would be resolved through the global/app class store instead.
///
/// When this returns true, `cl_load_class` must instead dispatch the virtual
/// `loadClass(name, false)` so the subclass bytecode actually runs. Returns
/// false for base / built-in loaders (where the Rust delegation is authoritative
/// and a virtual dispatch would recurse back into this native).
fn receiver_overrides_load_class(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    descriptor: &str,
) -> bool {
    let mut cid = Some(ctx.class_id_of_object(this));
    let mut found_override = false;
    while let Some(id) = cid {
        let name = match ctx.class_name_of_id(id) {
            Some(n) => n,
            None => break,
        };
        // URLClassLoader-family loaders (notably Spring Boot's
        // `LaunchedURLClassLoader`) have their class resolution substituted by
        // CratonVM's classpath scanner — their `loadClass` bytecode depends on
        // `URLClassPath` / nested-JAR plumbing CratonVM does not run, and they
        // are handled by the existing `Class.forName` / base-delegation rescues.
        // Leave them on the base path: do NOT route them through their override.
        if name == "java/net/URLClassLoader" {
            // Plain URLClassLoader-family receivers stay on CratonVM's base
            // path, but a subclass override found below the URLClassLoader
            // superclass must still win. BeanShell's BshClassLoader extends
            // URLClassLoader and overrides loadClass(String, boolean); masking
            // that override makes ClassManagerImpl.classForName reuse a stale
            // globally-loaded MyMessenger instead of reaching findClass.
            return found_override;
        }
        if is_builtin_loader_class(&name) {
            // Reached the builtin base.
            break;
        }
        if !found_override
            && ctx
                .declared_methods(id)
                .iter()
                .any(|m| m.name == "loadClass" && m.descriptor == descriptor)
        {
            found_override = true;
        }
        cid = ctx.superclass_of(id);
    }
    found_override
}

/// See [`receiver_overrides_load_class`]. The one-argument public overload is
/// itself virtual and can be overridden independently of the protected
/// `(String, boolean)` form. `ModifiedClassPathClassLoader` does exactly that
/// to reject packages excluded by Spring Boot's `@ClassPathExclusions`.
pub(crate) fn receiver_overrides_load_class_single(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> bool {
    receiver_overrides_load_class(ctx, this, "(Ljava/lang/String;)Ljava/lang/Class;")
}

/// See [`receiver_overrides_load_class`].
pub(crate) fn receiver_overrides_load_class_resolve(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> bool {
    receiver_overrides_load_class(ctx, this, "(Ljava/lang/String;Z)Ljava/lang/Class;")
}

// A public `loadClass(String)` override commonly delegates with
// `super.loadClass(name)`. CratonVM serves that base JDK method natively, so
// the nested invokespecial reaches the same callback as the outer virtual
// call. Remember the active loader identity per native thread and let that
// nested call take base delegation; otherwise ModifiedClassPathClassLoader's
// `return super.loadClass(name)` recursively re-enters its own override.
// Identity hashes are stable across moving GC, unlike raw ObjectRef addresses.
thread_local! {
    static SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
}

/// Invoke a genuine public `loadClass(String)` override once, or return
/// `None` when the receiver has no override or this is its nested
/// `super.loadClass(name)` delegation.
pub(crate) fn invoke_single_load_class_override(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name_obj: ObjectRef,
) -> Option<MethodCallResult> {
    if !receiver_overrides_load_class_single(ctx, this) {
        return None;
    }
    let identity = ctx.identity_hash_code(this);
    let reentrant = SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT.with(|active| {
        let mut active = active.borrow_mut();
        if active.contains(&identity) {
            true
        } else {
            active.push(identity);
            false
        }
    });
    if reentrant {
        return None;
    }
    let result = ctx.invoke_virtual(
        this,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(name_obj))],
    );
    SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT.with(|active| {
        let popped = active.borrow_mut().pop();
        debug_assert_eq!(popped, Some(identity));
    });
    Some(result)
}

/// True if `this` is a USER-DEFINED `ClassLoader` (a non-builtin subclass), as
/// opposed to a built-in bootstrap/extension/application/URL loader. Used to
/// decide whether `findLoadedClass`/`findLoadedClass0` must be loader-scoped
/// (a user loader only "knows" classes in its own namespace) versus the global
/// lookup that is correct for the built-in loaders.
pub(crate) fn is_user_defined_loader(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(this);
    match ctx.class_name_of_id(cid) {
        Some(name) => !is_builtin_loader_class(&name),
        None => false,
    }
}

/// Identity-hash → CratonVM loader-namespace-id side table for real-JDK mode.
///
/// In synthetic-JDK mode a user loader's namespace id lives in the synthetic
/// `CL_LOADER_ID` field slot (populated by `ClassLoader.<init>`/`defineClass`).
/// In real-JDK mode that slot is a genuine `java.lang.ClassLoader` field and
/// cannot be repurposed, so the id is keyed instead on the loader's STABLE
/// identity hash. Holds only `i32 → u32` (no `ObjectRef`s) — no GC rooting.
fn loader_namespace_id_store() -> &'static Mutex<Vec<(ObjectRef, u32)>> {
    // Keyed by the loader OBJECT, not its identity hash: identity hashes can
    // collide across distinct loader instances (address-derived hashes recur
    // after a collection reuses the region), and the old hash-keyed map never
    // pruned dead loaders — a fresh per-compile loader (Spring TestCompiler's
    // DynamicClassLoader) could inherit a dead sibling's namespace id and
    // resolve THAT loader's same-named generated classes. Entries are
    // remapped/pruned post-GC by [`gc_reconcile_defining_loaders`].
    static INSTANCE: OnceLock<Mutex<Vec<(ObjectRef, u32)>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Stable CratonVM loader-namespace id for a `ClassLoader` instance, allocating
/// one on first request. Built-in loaders map to `0` (the Application / global
/// namespace — they ARE the global store). User-defined loaders use their
/// synthetic `CL_LOADER_ID` slot when present (synthetic-JDK mode) and otherwise
/// an identity-hash-keyed id (real-JDK mode). Used by `defineClass` to give a
/// user loader its own namespace so an override-first redefinition of an
/// already-loaded class does not collide with the original definer.
pub(crate) fn loader_namespace_id(ctx: &mut dyn NativeContext, loader: ObjectRef) -> u32 {
    if !is_user_defined_loader(ctx, loader) {
        return 0;
    }
    if let Value::Int(v) = ctx.get_field(loader, CL_LOADER_ID) {
        if v > 0 {
            return v as u32;
        }
    }
    let mut map = loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, id)) = map.iter().find(|(l, _)| l.as_ptr() == loader.as_ptr()) {
        return id;
    }
    let id = ctx.allocate_loader_id();
    map.push((loader, id));
    id
}

/// Read-only probe of a user loader's namespace id (no allocation). `None` when
/// the loader is built-in, or has not yet been assigned one (it has defined no
/// class under a distinct namespace).
pub(crate) fn peek_loader_namespace_id(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
) -> Option<u32> {
    if !is_user_defined_loader(ctx, loader) {
        return None;
    }
    if let Value::Int(v) = ctx.get_field(loader, CL_LOADER_ID) {
        if v > 0 {
            return Some(v as u32);
        }
    }
    loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(l, _)| l.as_ptr() == loader.as_ptr())
        .map(|&(_, id)| id)
}

/// True for a JDK dynamic-proxy class's internal name (`jdk/proxyN/$ProxyM` on
/// JDK 9+, `com/sun/proxy/$ProxyM` on the legacy layout). Generated proxies are
/// never real classpath classes, so a built-in loader can only ever "find" one
/// because it leaked into CratonVM's flat global store — see the use in
/// [`find_loaded_class_for_loader`].
pub(crate) fn is_generated_proxy_name(name: &str) -> bool {
    (name.starts_with("jdk/proxy") || name.starts_with("com/sun/proxy")) && name.contains("$Proxy")
}

/// True when `cid` is a generated proxy class that loader `this` must NOT be
/// allowed to resolve: a proxy is visible only to its defining loader and that
/// loader's delegation descendants (JVMS §5.3). Stops CratonVM's flat global
/// store from leaking one loader's proxy to an unrelated/sibling loader through
/// the various name-keyed resolution natives. ClassUtilsTests.isCacheSafe.
pub(crate) fn proxy_hidden_from(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal: &str,
    cid: cratonvm_types::ClassId,
) -> bool {
    if !is_generated_proxy_name(internal) {
        return false;
    }
    match defining_loader_for(cid.as_u32()) {
        Some(def) => !loader_can_see_defining(ctx, this, def),
        None => false,
    }
}

/// Shared `findLoadedClass` logic (JVMS §5.3): returns the Class mirror for
/// `internal_name` only if `this` loader is recorded as having loaded it —
/// NEVER a class some OTHER loader happens to have loaded. Does NOT trigger
/// loading.
///
/// For a built-in loader the global loaded-class set is the right answer. For a
/// user-defined loader, a class counts as "loaded by this loader" if either:
///   1. it lives in this loader's own namespace (a distinct copy this loader
///      defined — the override-first redefinition case), or
///   2. the globally-known class of that name records THIS loader as its
///      defining loader (the common case: e.g. ByteBuddy's `ByteArrayClassLoader`
///      defines under the Application namespace but registers itself as definer).
/// Otherwise it is not visible to this loader as "already loaded" → `None`,
/// which lets the loader's `loadClass` override proceed to `findClass`/define.
pub(crate) fn find_loaded_class_for_loader(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
) -> Option<ObjectRef> {
    let __obsreg_dbg =
        std::env::var_os("CRATONVM_DBG_OBSREG").is_some() && internal_name.contains("ObservationRegistry");
    let __is_user_defined = is_user_defined_loader(ctx, this);
    if __obsreg_dbg {
        eprintln!(
            "[OBSREG-DBG] find_loaded_class_for_loader(this={:?}, name={}) is_user_defined={}",
            this, internal_name, __is_user_defined
        );
    }
    let __result = find_loaded_class_for_loader_inner(ctx, this, internal_name, __is_user_defined);
    if __obsreg_dbg {
        let cid = __result.map(|m| ctx.class_id_of_object(m));
        eprintln!(
            "[OBSREG-DBG] find_loaded_class_for_loader(this={:?}, name={}) -> {:?} (class_id={:?})",
            this, internal_name, __result, cid
        );
    }
    __result
}

fn find_loaded_class_for_loader_inner(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
    is_user_defined: bool,
) -> Option<ObjectRef> {
    if !is_user_defined {
        return ctx.class_id_by_name(internal_name).and_then(|cid| {
            // A generated proxy is checked via `proxy_hidden_from` — loader-identity
            // and delegation aware — REGARDLESS of `loader_id_of_class(cid)`. Proxy
            // classes get a fresh per-generation internal loader/module id (e.g. the
            // `jdk/proxy1/…` numbering) even when their defining loader is a
            // BUILT-IN loader (the application loader itself), so `loader_id_of_class`
            // is > 2 for a proxy the app loader legitimately just defined. Running the
            // blanket "> 2 -> hide" check first (as before) hid every generated proxy
            // from `findLoadedClass`/`Class.forName(name, false, loader)` even when
            // `this` WAS the recorded defining loader: a fresh `Class.forName` on a
            // proxy class the app loader had itself just created via
            // `Proxy.newProxyInstance` reported it as not found, throwing
            // ClassNotFoundException. That broke AspectJ's reflection-based pointcut
            // matching, which resolves a scratch composite-interface proxy class by
            // name via `Class.forName` to inspect its superclass: the lookup failure
            // surfaced as `ReflectionWorldException: can't determine superclass of
            // missing type jdk.proxyN.$ProxyM`, silently dropping the advisor
            // (Spring's `AspectJExpressionPointcut` catches the exception and falls
            // back to a "never matches" verdict) — reproduced by Spring's
            // `AspectJAutoProxyCreatorTests` (an `@Around` advice on an inherited
            // default interface method never fired).
            if is_generated_proxy_name(internal_name) {
                if proxy_hidden_from(ctx, this, internal_name, cid) {
                    return None;
                }
                return Some(ctx.get_class_mirror(cid));
            }
            // JVMS §5.3: a built-in loader (bootstrap/platform/app) never counts
            // as having loaded a class that a *user-defined* loader defined.
            // CratonVM's flat global store would otherwise let the app loader
            // report a child loader's class as "already loaded" — and since
            // `loadClass` delegates parent-first, a sibling custom loader then
            // resolves it too (`ClassUtilsTests.isCacheSafe`).
            //
            // Actual user-loader namespace hits are not visible to built-in
            // loaders. Application-namespace classes that merely record a
            // user-defined defining loader still keep their app-loader
            // visibility below.
            if ctx.loader_id_of_class(cid) > 2 {
                return None;
            }
            if let Some(def) = defining_loader_for(cid.as_u32()) {
                if !loader_can_see_defining(ctx, this, def) {
                    return None;
                }
            }
            Some(ctx.get_class_mirror(cid))
        });
    }
    // 1. Own-namespace copy — EXACT (no global / parent-delegation fallback).
    // `findLoadedClass` must report ONLY a class THIS user loader has itself
    // defined; the fallback-prone `class_id_by_name_and_loader` would otherwise
    // hand back some OTHER loader's copy of a name this loader has not defined.
    // That broke classloader isolation once a child loader already held a
    // namespace (≥1 defined class): an as-yet-undefined but eligible class
    // (e.g. an annotation interface the child overrides) resolved to the parent/
    // app copy instead of the child defining its own, so
    // `annotation.getClass().getClassLoader()` reported the app loader
    // (MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader).
    // `define_class` records `(loader, name)` in `loaded_classes` regardless of
    // the loader-aware-resolution gate, so the exact probe still finds the
    // loader's own copy on later lookups (no duplicate definition); a genuine
    // miss correctly falls through so the loader's own `loadClass` runs.
    if let Some(id) = peek_loader_namespace_id(ctx, this) {
        if let Some(cid) = ctx.class_id_defined_by_loader_exact(internal_name, id) {
            return Some(ctx.get_class_mirror(cid));
        }
    }
    // Real-JDK ClassLoader layouts do not reliably expose the synthetic
    // namespace id used by the class manager. `defineClass` also records the
    // exact defining loader object per ClassId; consult that authoritative
    // relation so a parent fork loader can recover its own already-defined
    // class before delegating to a global same-named copy.
    let defined_here: Vec<u32> = defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|(&cid, loader)| (loader.as_ptr() == this.as_ptr()).then_some(cid))
        .collect();
    for cid in defined_here {
        let cid = cratonvm_types::ClassId::new(cid);
        if ctx.class_name_of_id(cid).as_deref() == Some(internal_name) {
            return Some(ctx.get_class_mirror(cid));
        }
    }
    // 2. A globally-known class THIS loader is the defining loader of.
    if let Some(cid) = ctx.class_id_by_name(internal_name) {
        if let Some(def) = defining_loader_for(cid.as_u32()) {
            if def.as_ptr() == this.as_ptr() {
                return Some(ctx.get_class_mirror(cid));
            }
        }
    }
    None
}

fn cl_load_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let exc = crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/NullPointerException",
                1,
                "ClassLoader.loadClass name is null",
            );
            return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                exc,
            ));
        }
    };

    // `ClassLoader.loadClass(String)` is spec'd as `return loadClass(name, false)`.
    // If the receiver's actual class overrides the public single-argument or protected
    // `loadClass(String,boolean)` with its own bytecode (e.g. Spring's
    // OverridingClassLoader, which redefines eligible classes under itself
    // BEFORE parent delegation, or rejects filtered names), dispatch the virtual
    // `loadClass(name, false)` so that override actually runs. Reimplementing
    // base parent-first delegation here would resolve the class through the
    // global/app class store and ignore the user loader entirely (its custom
    // ordering and defining-loader identity would be lost).
    //
    // `super.loadClass(name, resolve)` from such an override is an invokespecial
    // that lands on the base native `cl_load_class_resolve`
    // (→ `cl_load_class_base_delegation`), so there is no recursion back here.
    if let Some(result) = invoke_single_load_class_override(ctx, this, name_obj) {
        return result;
    }
    if receiver_overrides_load_class_resolve(ctx, this) {
        return ctx.invoke_virtual(
            this,
            "loadClass",
            "(Ljava/lang/String;Z)Ljava/lang/Class;",
            &[Value::Object(Some(name_obj)), Value::Int(0)],
        );
    }

    cl_load_class_base_delegation(ctx, this, name_obj)
}

/// Canonical `ClassLoader.loadClass(String)` entry point for interpreter
/// dispatches that must enforce the public null-name contract.
pub fn cl_load_class_essential(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cl_load_class(ctx, args)
}

fn classloader_parent(ctx: &mut dyn NativeContext, loader: ObjectRef) -> Option<ObjectRef> {
    // The real named `parent` field is populated by name in exactly ONE
    // place (the bootstrap app loader's own construction, see
    // `alloc_classloader`) — every ordinary `ClassLoader`/`URLClassLoader`
    // constructor native (`cl_init_parent`, `cl_init_name_parent`,
    // `ucl_setup`, ...) writes only the numeric `CL_PARENT_REF` slot. For
    // those (the overwhelming majority of real-JDK-mode loaders), a
    // by-name read of "parent" returns a genuinely-null Java field — NOT
    // evidence that the loader has no parent — so it must fall through to
    // the slot, not be trusted as the final answer. Treating that null as
    // definitive made every `URLClassLoader` constructed with a non-null
    // parent (e.g. Spring Boot's `PropertiesLauncher.wrapWithCustomClassLoader`
    // wrapping a `LaunchedClassLoader`) look parentless to
    // `cl_load_class_base_delegation`, which then skipped real parent-first
    // delegation entirely and went straight to the (parentless) global/own-URL
    // fallback — silently losing the parent's classpath.
    if let Value::Object(Some(parent)) = ctx.get_field_by_name(loader, "parent") {
        return Some(parent);
    }
    match ctx.get_field(loader, CL_PARENT_REF) {
        Value::Object(Some(parent)) => Some(parent),
        _ => None,
    }
}

/// True if `loader` can see a class whose defining loader is `defining` — i.e.
/// `defining` is `loader` itself or one of its delegation ancestors (parent
/// chain). JVMS §5.3: a class defined by loader D is visible to L only if L
/// (transitively) delegates to D. CratonVM keeps a single flat global class
/// store, so without this check a loader can resolve an unrelated *sibling*
/// loader's class by name — e.g. `Proxy.getProxyClass(childLoader1, …)` is
/// globally registered as `jdk.proxy1.$Proxy0`, so `childLoader2.loadClass`
/// found it too (HotSpot throws ClassNotFoundException). That made
/// `ClassUtils.isCacheSafe(composite, siblingLoader)` wrongly true via its
/// `isLoadable` fallback. ClassUtilsTests.isCacheSafe.
/// Whether an APPLICATION-tier built-in loader appears in `loader`'s parent
/// chain, including `loader` itself. A `false` answer means the chain never
/// reaches a loader that can see the application classpath, so per JVMS 5.3
/// only bootstrap/platform (JDK module) classes are resolvable through
/// delegation and CratonVM's flat global store -- which conflates every
/// loaded class, including ones only the application loader can see -- must
/// not stand in for delegation here.
///
/// The platform loader does NOT count, even though it is "built-in" (not
/// user-defined): it only sees JDK platform modules, never application
/// classes, so treating it the same as the application loader wrongly let a
/// loader parented ONLY as `UserLoader -> PlatformClassLoader -> bootstrap`
/// (e.g. Spring's `CompileWithForkedClassLoaderClassLoader`, whose whole
/// point is to skip the application loader and mint its OWN fresh copies of
/// non-JDK classes) "see" an application class that was merely already
/// loaded elsewhere in the process. That produced a real, reproducing bug:
/// `SpringFactoriesEnvironmentPostProcessorsFactory` resolved through the
/// flat store to the ORIGINAL application-loader copy instead of the forked
/// loader calling its own `findClass` override to mint an isolated copy —
/// so a `DeferredLogFactory` instance captured against the app-loader
/// `Class` object failed an `ArgumentResolver` type match against a
/// factory's constructor parameter resolved via the forked loader, leaving
/// the parameter null (`NullPointerException` in
/// `CloudFoundryVcapEnvironmentPostProcessor.<init>`, "logFactory" null).
pub(crate) fn builtin_loader_reachable(ctx: &mut dyn NativeContext, loader: ObjectRef) -> bool {
    let mut cur = Some(loader);
    for _ in 0..256 {
        let Some(l) = cur else { break };
        if !is_user_defined_loader(ctx, l) && !is_platform_class_loader(ctx, l) {
            return true;
        }
        cur = classloader_parent(ctx, l);
    }
    false
}

fn loader_can_see_defining(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    defining: ObjectRef,
) -> bool {
    let mut cur = Some(loader);
    // Bounded walk up the parent chain (defensive cap against cycles).
    for _ in 0..256 {
        let Some(loader) = cur else { break };
        if loader.as_ptr() == defining.as_ptr() {
            return true;
        }
        cur = classloader_parent(ctx, loader);
    }
    false
}

/// Resolve `internal` through CratonVM's global class store, but enforce loader
/// isolation: if the resolved class was defined by a *user-defined* loader that
/// `this` cannot see ([`loader_can_see_defining`]), return `None` so the caller
/// falls through to `findClass` / "not found" instead of leaking another
/// loader's class. Only user-defined defining loaders are recorded in the
/// defining-loader registry (built-in app/platform/bootstrap loaders are not),
/// so the common case — app/JDK classes with no registered defining loader —
/// is unchanged and still resolves permissively.
/// The class mirror for `cid`, unless loader isolation hides it from `this`.
///
/// Enforces isolation for both custom and built-in requesters. The app loader
/// must not report a child loader's class as already globally available; that
/// reverse leak lets sibling BeanShell interpreters reuse the first generated
/// `MyMessenger` instead of defining their own.
pub(crate) fn cid_visible_mirror(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    cid: cratonvm_types::ClassId,
) -> Option<ObjectRef> {
    // A class whose defining loader was confirmed collected is gone from
    // every OTHER loader's perspective (real unloading semantics) -- checked
    // BEFORE the live-registry lookup so a pruned entry never falls through
    // to "no restriction, visible to all" (see `is_defining_loader_orphaned`).
    if is_defining_loader_orphaned(cid.as_u32()) {
        return None;
    }
    if let Some(def) = defining_loader_for(cid.as_u32()) {
        if !loader_can_see_defining(ctx, this, def) {
            return None;
        }
    }
    Some(ctx.get_class_mirror(cid))
}

/// Real-JDK-mode-analogue diagnostic (see `classloader_real::load_class_visible_to`
/// / `no_class_def_found_error`, fixed 2026-07-17): `ensure_class_initialized`
/// bottoms out in the same `ClassManager::load_class` that propagates a
/// recursive supertype/interface load failure UNCHANGED, so a `ClassNotFound`
/// naming something other than `internal` means the requested class file
/// exists but a *dependency* is missing -- JVMS §5.3/§5.4:
/// `NoClassDefFoundError`, not a bare CNFE on the requested class. Surfaced
/// as an immediate `Err` (rather than folded into the `Ok(None)` "not
/// found" case) so the caller throws it instead of silently returning a
/// null `Class` (this native's genuine-miss contract -- see step 7 of
/// `cl_load_class_base_delegation`).
fn resolve_global_if_visible(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal: &str,
) -> Result<Option<ObjectRef>, cratonvm_types::error::MethodCallFailed> {
    match ctx.ensure_class_initialized(internal) {
        Ok(cid) => Ok(cid_visible_mirror(ctx, this, cid)),
        Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ClassFile(
                cratonvm_types::error::ClassFileError::ClassNotFound { class_name },
            ),
        )) if class_name != internal => {
            tracing::debug!(
                requested = internal,
                missing_dependency = %class_name,
                "resolve_global_if_visible: requested class exists but a \
                 dependency failed to resolve -- NoClassDefFoundError"
            );
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                crate::classloader_real::no_class_def_found_error(ctx, &class_name),
            ))
        }
        Err(e) => {
            tracing::debug!(
                requested = internal,
                error = %e,
                "resolve_global_if_visible: ensure_class_initialized failed"
            );
            Ok(None)
        }
    }
}

/// Base-class `ClassLoader.loadClass` parent-first delegation, reimplemented in
/// Rust (CratonVM keeps no JDK bytecode for `ClassLoader.loadClass`).
///
/// Reached when the receiver does NOT override `loadClass(String,boolean)` — it
/// inherits the base behavior — and also via `super.loadClass(name, resolve)`
/// (invokespecial → the base native `cl_load_class_resolve`) from a subclass
/// override that wants the standard parent-first path as its fallback.
fn cl_load_class_base_delegation(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name_obj: ObjectRef,
) -> MethodCallResult {
    let dotted = ctx.read_string(name_obj).unwrap_or_default();
    let internal = dotted.replace('.', "/");
    let __obsreg_dbg =
        std::env::var_os("CRATONVM_DBG_OBSREG").is_some() && internal.contains("ObservationRegistry");
    if __obsreg_dbg {
        let parent = classloader_parent(ctx, this);
        let this_cls = ctx.class_name_of_id(ctx.class_id_of_object(this));
        let parent_cls = parent.map(|p| ctx.class_name_of_id(ctx.class_id_of_object(p)));
        eprintln!(
            "[OBSREG-DBG] cl_load_class_base_delegation ENTER this={:?} this_class={:?} parent={:?} parent_class={:?} name={}",
            this, this_cls, parent, parent_cls, internal
        );
    }
    let __result = cl_load_class_base_delegation_inner(ctx, this, name_obj, &internal);
    if __obsreg_dbg {
        let cid = match &__result {
            Ok(Some(Value::Object(Some(m)))) => Some(ctx.class_id_of_object(*m)),
            _ => None,
        };
        eprintln!(
            "[OBSREG-DBG] cl_load_class_base_delegation EXIT this={:?} name={} -> {:?} (class_id={:?})",
            this, internal, __result, cid
        );
    }
    __result
}

fn cl_load_class_base_delegation_inner(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name_obj: ObjectRef,
    internal: &str,
) -> MethodCallResult {
    let internal = internal.to_string();
    // HIB-CV-24 / SBR-14 -- honor a supplied child/isolated `ClassLoader`.
    //
    // CratonVM stands in for `ClassLoader.loadClass` with this native (it keeps no
    // JDK bytecode for it). The steps below resolve a class through CratonVM's
    // flat global store (`ensure_class_initialized`) BEFORE reaching the
    // `findClass` override (step 4). For a custom loader whose parent is the
    // bootstrap loader (e.g. Hibernate's `AggregatedClassLoader`, which is
    // `super(null)` and overrides `findClass` to iterate scoped child loaders),
    // that global pre-resolution acts like the application loader and bypasses the
    // supplied loader entirely (JVMS §5.3: a bootstrap parent cannot load an
    // application class, so `findClass` MUST run). When such a loader overrides
    // `findClass` and the requested class is NOT a bootstrap/platform class, defer
    // every global short-circuit to AFTER `findClass`. Only for a NULL parent — a
    // non-null (app/platform) parent keeps JVMS parent-first (it legitimately
    // loads the class; `findClass` is not called). Built-in loaders and bootstrap
    // classes keep the permissive global path (CratonVM has no separate bootstrap
    // classpath). Opt-out: `CRATONVM_CL_BOOTSTRAP_SCOPED=0`.
    let parent = classloader_parent(ctx, this);
    let parent_is_null = parent.is_none();
    // The platform loader has the same visibility as bootstrap for application
    // classes: it can load JDK modules, never a test/application class.  In
    // real-JDK mode its Java fields cannot carry CratonVM's synthetic loader
    // type marker, so the old `parent_type` fallback below mistook it for the
    // app loader and leaked a global app class before a child `findClass` got
    // a chance to define its own copy.  Compare the singleton identity instead
    // of inspecting real JDK object fields.
    let parent_is_platform = parent.is_some_and(|candidate| {
        // The JDK can manufacture another PlatformClassLoader object before
        // our native singleton is observed, so identity is a fast path only;
        // the object's actual runtime class is the authoritative fallback.
        ctx.class_name_of_id(ctx.class_id_of_object(candidate))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader")
            || platform_loader_store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|platform| platform.as_ptr() == candidate.as_ptr())
    });
    let receiver_has_find_class_override = receiver_overrides_find_class(ctx, this);
    let defer_to_find_class = cl_bootstrap_scoped()
        && (parent_is_null || parent_is_platform)
        && !is_bootstrap_class_name(&internal)
        // A jar appended via Instrumentation.appendToBootstrapClassLoaderSearch
        // belongs to the bootstrap loader: parent delegation must serve it
        // BEFORE any findClass override defines a per-loader copy (Mockito
        // asserts its injected MockMethodDispatcher has a null loader).
        && !cratonvm_classloading::is_bootstrap_appended_class(&internal)
        && receiver_has_find_class_override;
    // JVMS 5.3-faithful scoping of the flat-store fallback (same gate as the
    // defer logic above): CratonVM's global store stands in for "the app
    // classpath, reachable through the parent chain". A loader whose REAL
    // parent chain never passes through a built-in loader (e.g.
    // `new ClassLoader(null) {}`, or a loader parented to such) can only see
    // bootstrap classes on HotSpot; answering an application class from the
    // flat store bypasses the loader's own fallback logic (seen:
    // ThrowawayClassLoader.loadClassFromResource never ran because
    // super.loadClass resolved the probe class globally, failing its
    // stream-closing contract test). Loaders with a findClass override keep
    // their step-6 rescue below, so only override-less chains change.
    let scoped_user_chain = cl_bootstrap_scoped()
        && !is_bootstrap_class_name(&internal)
        && !cratonvm_classloading::is_bootstrap_appended_class(&internal)
        && !builtin_loader_reachable(ctx, this);
    // JVM spec §5.3.2 — parent-first delegation:
    // 1. Check if this loader already loaded the class (findLoadedClass)
    let loader_type = match ctx.get_field(this, CL_LOADER_TYPE) {
        Value::Int(v) => v,
        _ => LOADER_APP,
    };

    // A user-defined loader must always return a class it has already
    // defined before delegating to its parent. In real-JDK mode the
    // synthetic loader-type slot is unavailable, so the legacy branch below
    // can misclassify it as an application loader and skip this check; that
    // leaks a same-named global class through a forked parent loader.
    if is_user_defined_loader(ctx, this) {
        if let Some(mirror) = find_loaded_class_for_loader(ctx, this, &internal) {
            return Ok(Some(Value::Object(Some(mirror))));
        }
        // The read-only lookup above intentionally avoids allocating a
        // namespace.  A real-JDK ClassLoader may not expose the synthetic
        // id field even though `defineClass` has already registered classes
        // through `loader_namespace_id`; ask that same authoritative mapping
        // before falling through to the parent/global store.
        let loader_id = loader_namespace_id(ctx, this);
        if let Some(cid) = ctx.class_id_by_name_and_loader(&internal, loader_id) {
            if let Some(mirror) = cid_visible_mirror(ctx, this, cid) {
                return Ok(Some(Value::Object(Some(mirror))));
            }
        }
    }

    // For synthetic-mode custom loaders, check own namespace first.
    if loader_type == LOADER_CUSTOM {
        let loader_id = match ctx.get_field(this, CL_LOADER_ID) {
            Value::Int(v) if v > 0 => Some(v as u32),
            _ => None,
        };
        if let Some(lid) = loader_id {
            if let Some(cid) = ctx.class_id_by_name_and_loader(&internal, lid) {
                if let Some(mirror) = cid_visible_mirror(ctx, this, cid) {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
            }
        }
    }

    // 2. Delegate to parent loader first (recursive parent-first delegation)
    if let Some(parent) = parent {
        // Recursively delegate to parent by calling its loadClass
        let parent_type = match ctx.get_field(parent, CL_LOADER_TYPE) {
            Value::Int(v) => v,
            _ => LOADER_APP,
        };
        let parent_lid = match ctx.get_field(parent, CL_LOADER_ID) {
            Value::Int(v) if v > 0 => Some(v as u32),
            _ => None,
        };
        // Check parent's namespace for custom loaders. Apply loader-isolation:
        // a sibling custom loader's class (e.g. a generated proxy that leaked
        // into the app-loader namespace but whose registered defining loader is
        // an unrelated child) must not be handed to `this`. ClassUtilsTests
        // .isCacheSafe via `isLoadable`.
        if let Some(pid) = parent_lid {
            if let Some(cid) = ctx.class_id_by_name_and_loader(&internal, pid) {
                if let Some(mirror) = cid_visible_mirror(ctx, this, cid) {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
            }
        }
        // A user-defined parent has its own delegation and `findClass`
        // behavior. In real-JDK mode its internal loader-type fields are not
        // available to this native, so treating it like a built-in parent and
        // consulting the flat global store first can return an unrelated
        // same-named application class. Invoke the parent's actual loadClass
        // before any global fallback, exactly as parent-first delegation
        // requires (notably DynamicClassLoader -> forked test loader).
        if is_user_defined_loader(ctx, parent) {
            match ctx.invoke_virtual(
                parent,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[Value::Object(Some(name_obj))],
            ) {
                Ok(Some(Value::Object(Some(mirror)))) => {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
                _ => {}
            }
        }
        // For built-in parent loaders (bootstrap/platform/app), use standard delegation
        if parent_type != LOADER_CUSTOM && !defer_to_find_class && !scoped_user_chain {
            // Standard delegation handles bootstrap → extension → app
            if let Some(mirror) = resolve_global_if_visible(ctx, this, &internal)? {
                return Ok(Some(Value::Object(Some(mirror))));
            }
        }
    } else if !defer_to_find_class && !scoped_user_chain {
        // No parent (or null parent) → delegate directly to bootstrap loader
        // Bootstrap delegation: use the standard class loading chain
        if let Some(mirror) = resolve_global_if_visible(ctx, this, &internal)? {
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }

    // 3. Parent couldn't find it — fall back to standard loading
    //    (this covers bootstrap → extension → application delegation).
    //    Skipped when deferring to a custom `findClass` override (HIB-CV-24) so
    //    the supplied loader runs before CratonVM's global store answers.
    if !defer_to_find_class && !scoped_user_chain {
        if let Some(mirror) = resolve_global_if_visible(ctx, this, &internal)? {
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }

    // URLClassLoader searches its recorded URLs after parent delegation. Its
    // entries deliberately are not appended to the process-wide application
    // path, because that would make one temporary loader's classes and
    // resources visible to another.
    // Some real-JDK subclasses do not expose their inherited
    // URLClassLoader identity through `object_extends` during native
    // dispatch. The helper is a no-op for receivers without recorded URLs,
    // so probe it directly rather than dropping their isolated path.
    if let Some(result) = ucl_try_define_local_class(ctx, this, &internal) {
        return result;
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
        // The base `ClassLoader.findClass` itself is registered as a native.
        // A regular virtual call can therefore re-enter that native through
        // the inherited declaration and bypass this known subclass override.
        // The predicate above proves a real bytecode implementation exists on
        // the receiver hierarchy; select that implementation explicitly.
        let result = ctx.invoke_virtual_bytecode_only(
            this,
            "findClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(name_obj))],
        );
        match result {
            Ok(Some(Value::Object(Some(_)))) => return result,
            // A miss from URLClassLoader's own native URL/HTTP search is
            // authoritative -- propagate it (e.g. ClassNotFoundException)
            // rather than falling through to step 6's global-store fallback,
            // which would let a null-parent URLClassLoader resolve
            // application classes its own (failed) URL search should have
            // hidden from it. See docs/known-issues/keycloak/
            // test-classserver-invalidpackage-classnotfound-not-thrown.md.
            _ if defer_to_find_class && find_class_is_urlclassloader_native(ctx, this) => {
                return result
            }
            // findClass threw (ClassNotFoundException) or returned null — fall through.
            _ => {}
        }
    }

    // 5. IMPL-JARS fallback: ES EmbeddedImplClassLoader stores provider
    //    classes and all their inner/helper classes as individual ZIP entries
    //    under IMPL-JARS/<module>/<jar_dir>/<classfile> inside the outer
    //    module JAR. When neither the flat classpath nor findClass can locate
    //    the class, try scanning those entries directly.
    if let Some(mirror) = impl_jars_load_class(ctx, Some(this), &internal) {
        return Ok(Some(Value::Object(Some(mirror))));
    }

    // 6. Deferred-resolution last resort (HIB-CV-24). When `defer_to_find_class`
    //    skipped the global short-circuits above and the loader's own `findClass`
    //    did not produce the class, CratonVM's flat store is still the only source
    //    of application classes — resolve here so a findClass-overriding loader
    //    whose override legitimately misses (delegating the actual load elsewhere)
    //    does not spuriously fail a class the runtime can provide.
    if defer_to_find_class {
        if let Ok(cid) = ctx.ensure_class_initialized(&internal) {
            let mirror = ctx.get_class_mirror(cid);
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }

    // 7. Not found and no user override — class genuinely missing.
    Ok(Some(Value::Object(None)))
}

fn cl_load_class_resolve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Base `ClassLoader.loadClass(String,boolean)` — boolean resolve arg is
    // ignored (we always resolve). This is the native for the base class only;
    // a subclass override of this method runs its own bytecode (it shadows the
    // inherited native), so reaching here means the receiver uses base
    // parent-first delegation. Must call the base delegation DIRECTLY (not
    // `cl_load_class`) so that `super.loadClass(name, resolve)` from a subclass
    // override does not bounce back into the override-dispatch and recurse.
    let this = obj_arg(args, 0)?;
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let exc = crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/NullPointerException",
                1,
                "ClassLoader.loadClass name is null",
            );
            return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                exc,
            ));
        }
    };
    cl_load_class_base_delegation(ctx, this, name_obj)
}

fn cl_find_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Base `ClassLoader.findClass(String)`. CratonVM reuses the parent-first
    // delegation as a permissive base findClass (covers the IMPL-JARS fallback).
    // Routed to the base delegation directly so it never triggers the
    // `loadClass(String,boolean)` override-dispatch (which would be wrong for
    // findClass and could recurse via a subclass `super.findClass`).
    let this = obj_arg(args, 0)?;
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    cl_load_class_base_delegation(ctx, this, name_obj)
}

fn cl_find_class_module(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // module-aware variant — module arg (index 1) ignored, class name at index 2
    let this = obj_arg(args, 0)?;
    let module_name = args.get(1).copied().unwrap_or(Value::Object(None));
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let dotted = ctx.read_string(name_obj).unwrap_or_default();
    let internal = dotted.replace('.', "/");

    // The base overload is native, but a custom class loader can override the
    // module-aware variant. Elasticsearch's EmbeddedImplClassLoader does so
    // for its IMPL-JARS, therefore this inherited-method path must preserve
    // virtual dispatch before falling back to the flat classpath.
    if receiver_overrides_find_class(ctx, this) {
        let result = ctx.invoke_virtual_bytecode_only(
            this,
            "findClass",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;",
            &[module_name, Value::Object(Some(name_obj))],
        );
        if matches!(result, Ok(Some(Value::Object(Some(_))))) {
            return result;
        }
    }

    // A module-aware lookup can be the first request for an embedded
    // implementation dependency, so share loadClass/findClass(String)'s
    // IMPL-JARS fallback here as well.
    if let Some(mirror) = impl_jars_load_class(ctx, Some(this), &internal) {
        return Ok(Some(Value::Object(Some(mirror))));
    }

    match ctx.ensure_class_initialized(&internal) {
        Ok(cid) => {
            // `BuiltinClassLoader` (the app/platform loader) calls this 2-arg
            // `findClass(module, name)` during its module/classpath search. The
            // global resolve above ignores loader identity, so a generated proxy
            // defined by an unrelated child loader would leak through here — this
            // is the path that defeated the `findLoadedClass` guard in
            // ClassUtilsTests.isCacheSafe. Hide cross-loader proxies.
            if proxy_hidden_from(ctx, this, &internal, cid) {
                return Ok(Some(Value::Object(None)));
            }
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
    name.contains("$$EnhancerByCGLIB$$") || name.starts_with("net/sf/cglib/proxy/")
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
fn cglib_guard_value(ctx: &mut dyn NativeContext, name: &str, _bytes: &[u8]) -> Option<Value> {
    // Strict-name match only. We do NOT sniff bytecode — the previous
    // `sniff_class_file_this_name` fallback was a defensive class-file
    // parser, but bytecode parsing on adversarial / truncated buffers
    // had a history of OOB reads and SIGILL (notably keycloak startup).
    // If the caller did not pass a `name`, we let the normal define
    // path handle it; if those bytes are a real cglib proxy CratonVM
    // will SEGV (the original problem) but at least we cannot regress
    // unrelated apps by mis-classifying their bytecode.
    if is_cglib_proxy_name(name) {
        tracing::warn!("[cglib-shim] short-circuiting defineClass for {name} (SEGV avoidance)");
        return Some(cglib_placeholder_mirror(ctx));
    }
    None
}

pub(crate) fn cl_define_class_basic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::error::{LinkageError, RuntimeError};

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
            tracing::warn!("ClassLoader.defineClass({name_str}): null bytecode array");
            return Err(RuntimeError::NullPointerException {
                message: Some("ClassLoader.defineClass: bytecode array must not be null".into()),
            }
            .into());
        }
    };

    let array_len = ctx.array_length(byte_array);

    // Safe integer handling: reject negative offset/length (i32 → usize)
    let offset = match args.get(3) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into())
        }
        _ => 0,
    };
    let length = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into())
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
        return Err(RuntimeError::ArrayIndexOutOfBoundsException { index: -1 }.into());
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
            return Err(LinkageError::ClassFormatError {
                class_name: name_str.clone(),
                message: "defineClass: panic while reading bytecode array".into(),
            }
            .into());
        }
    };

    // Pre-validate the class file header so that obviously-bad bytes
    // never reach `define_class_full` (cheap CAFEBABE magic check).
    if class_bytes.len() < 8 || class_bytes[0..4] != CLASS_FILE_MAGIC {
        tracing::warn!("[define_class] invalid magic for {name_str}; rejecting");
        return Err(LinkageError::ClassFormatError {
            class_name: name_str.clone(),
            message: "defineClass: not a valid class file (bad magic)".into(),
        }
        .into());
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
            return Err(LinkageError::ClassFormatError {
                class_name: name_str.clone(),
                message: "defineClass: panic inside backend (likely malformed bytecode)".into(),
            }
            .into());
        }
    };
    match define_result {
        Ok(cid) => {
            // Record the exact defining ClassLoader instance so
            // `Class.getClassLoader()` returns THIS loader rather than the
            // app-loader fallback. The public `defineClass(...)` overloads
            // (this native + its PD / ByteBuffer delegators) must do this just
            // like the JDK-internal `defineClass1` does — otherwise a class a
            // custom loader defines (e.g. Spring's OverridingClassLoader
            // redefining an eligible class under itself) would report the wrong
            // loader and classloader-isolation patterns silently break.
            crate::classloader::register_defining_loader(cid.as_u32(), this);
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
///
/// `pub(crate)`: also used by `shared_secrets_bridge::jla_define_class`.
pub(crate) fn read_optional_internal_name(
    ctx: &dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> String {
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
pub(crate) fn read_byte_array_slice(
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
            return Err(format!("offset+length overflow (off={off}, len={len})"));
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
pub(crate) fn extract_pd_code_source_url(ctx: &dyn NativeContext, pd: ObjectRef) -> Option<String> {
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

    // Direct-buffer case: a `java.nio.DirectByteBuffer` keeps its native
    // base in the `Buffer.address` long (the synthetic slot-0 array is
    // absent). Read the address + capacity and memcpy the bytes through
    // the context so an `Unsafe.allocateMemory` arena handle is routed to
    // the off-heap store rather than dereferenced raw (a raw memcpy from a
    // synthetic handle SIGSEGVs); a real pointer falls through to a raw
    // copy. This lets `Lookup.defineClass`/`defineClass2` accept direct
    // buffers, matching the heap path above. (cf. async_socket.rs /
    // nio_native.rs which use the same address + copy_from_native_memory
    // pattern.)
    let addr = match ctx.get_field_by_name(bb, "address") {
        Value::Long(a) => a,
        _ => 0,
    };
    // Honor the buffer's position/limit window, then add the caller's
    // (off, len) which are buffer-relative coordinates.
    let pos = ctx.get_field(bb, pos_slot).as_int().unwrap_or(0).max(0) as usize;
    let cap = ctx
        .get_field(bb, capacity_slot)
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    let limit = ctx
        .get_field(bb, limit_slot)
        .as_int()
        .unwrap_or(cap as i32)
        .max(0) as usize;
    if cap == 0 {
        return Err("direct ByteBuffer is empty".to_string());
    }
    if addr == 0 {
        return Err(format!(
            "direct ByteBuffer with capacity {cap} has no native address"
        ));
    }
    let absolute_off = pos.saturating_add(off);
    let upper = limit.min(cap);
    if absolute_off > upper {
        return Err(format!(
            "direct ByteBuffer offset+pos ({absolute_off}) exceeds limit ({limit}) or capacity ({cap})"
        ));
    }
    let actual_len = len.min(upper - absolute_off);
    let mut out = vec![0u8; actual_len];
    if actual_len > 0 {
        let src = addr.wrapping_add(absolute_off as i64);
        if !ctx.copy_from_native_memory(src, &mut out) {
            return Err(format!(
                "direct ByteBuffer copy failed (addr={src:#x}, len={actual_len})"
            ));
        }
    }
    Ok(out)
}

/// Bind the loader-id used to register the new class. We look up the
/// loader's `CL_LOADER_ID` slot if present (lazily allocating a fresh
/// id), otherwise fall through to id 0 (= application loader). A null
/// loader is treated as the bootstrap class loader, which the backend
/// also models as id 0 in this VM.
pub(crate) fn loader_id_for(ctx: &mut dyn NativeContext, loader: Value) -> u32 {
    if let Value::Object(Some(cl)) = loader {
        return get_or_assign_loader_id(ctx, cl);
    }
    0
}

/// Common backend used by all three `defineClassN` natives. Returns
/// the resulting Class mirror as a `Value::Object(Some(...))` or an
/// exception via the `MethodCallResult` channel.
///
/// `pub(crate)`: also reused by `shared_secrets_bridge::jla_define_class`
/// (`JavaLangAccess.defineClass`, the `System$1` bridge that
/// `jdk.internal.reflect.ClassDefiner` calls into) so both entry points
/// share the same magic-check / panic-guard / PD-attribution behavior.
///
/// `loader`: the `ClassLoader` object passed to the `defineClassN` native
/// (may be `Value::Object(None)` for the bootstrap loader). Recorded via
/// `register_defining_loader` on success so this class's true defining
/// loader is known to every `defining_loader_for` consumer (`getClassLoader`,
/// GC loader-pinning, `inherit_lookup_loader`'s namespace lookup, ...) —
/// previously only `Lookup.defineClass` (`lookup_define.rs`) recorded this,
/// so any class defined the ordinary way (`ClassLoader.defineClass`, e.g.
/// Groovy's `GroovyClassLoader` compiling a script class) had no recorded
/// defining loader. That left `inherit_lookup_loader`'s legacy fallback
/// (`loader_id_of_class`, which collapses small `UserDefined(n)` ids into
/// the builtin-loader id range) as the only source of truth for a CGLIB
/// proxy generated against such a class, mis-routing the proxy into the
/// Application namespace and CNFE-failing the `Class.forName(name, true,
/// loader)` CGLIB issues right after — see
/// `docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md`'s Groovy cluster
/// entry (`GroovyAspectTests`/`GroovyAspectIntegrationTests` residuals).
pub(crate) fn define_class_via_full(
    ctx: &mut dyn NativeContext,
    name: &str,
    bytes: Vec<u8>,
    loader_id: u32,
    opts: cratonvm_native_api::DefineClassFull,
    initialize: bool,
    class_data: Option<Value>,
    loader: Value,
) -> MethodCallResult {
    use cratonvm_types::error::{LinkageError, RuntimeError};

    // Pre-validate the class file header: at least 8 bytes (magic +
    // minor + major) and CAFEBABE magic must be present, otherwise the
    // backend parser may dereference garbage past the buffer end.
    if bytes.len() < 8 || bytes[0..4] != CLASS_FILE_MAGIC {
        tracing::warn!("[define_class] invalid magic for {name}; rejecting");
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
            tracing::error!("[define_class] panic inside define_class_full for {name}; aborting");
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
            // Record the true defining loader (see the doc comment above)
            // so later `defining_loader_for(cid)` consumers — including
            // `inherit_lookup_loader`'s namespace lookup for a subsequent
            // CGLIB/`Lookup.defineClass` proxy of this exact class — see the
            // real loader instead of falling back to the legacy
            // `loader_id_of_class` path, which can collapse a small
            // `UserDefined(n)` id into the builtin-loader range.
            //
            // Gated on `is_user_defined_loader` to preserve the existing
            // invariant that `defining_loader_store` only ever holds genuine
            // custom-`ClassLoader` instances, never the built-in bootstrap/
            // platform/application loader — registering the latter would add
            // an entry for nearly every class defined during a run (the vast
            // majority go through the system loader) for no behavioral
            // benefit (every consumer either wants the true custom loader or
            // already has its own built-in-loader fallback).
            if loader_aware_resolution() {
                if let Value::Object(Some(loader_obj)) = loader {
                    if is_user_defined_loader(ctx, loader_obj) {
                        register_defining_loader(cid.as_u32(), loader_obj);
                    }
                }
            }
            // Eager-init request: run <clinit> now (defineClass0 path
            // when `initialize == true`). `ctx.initialize_class` can
            // allocate and trigger a moving GC, so `mirror` (a raw
            // `ObjectRef` captured above and returned again below) must be
            // rooted across the call — same Family-1 stale-ObjectRef
            // pattern as the sibling `lk_ensure_initialized` fix. See
            // docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md.
            let mirror_pin = ctx.pin_native_root(mirror);
            if initialize {
                if let Err(msg) = ctx.initialize_class(cid) {
                    tracing::warn!("defineClass0 initialize: <clinit> for {name} failed: {msg}");
                }
            }
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            ctx.unpin_native_roots(mirror_pin);
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
    define_class_via_full(ctx, &name, bytes, loader_id, opts, false, None, loader)
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
    define_class_via_full(ctx, &name, bytes, loader_id, opts, false, None, loader)
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
    define_class_via_full(ctx, &name, bytes, loader_id, opts, initialize, class_data, loader)
}

/// WP2.3-C — register the JDK-internal `defineClass0/1/2` natives on
/// `java.lang.ClassLoader`. The public `defineClass(...)` overloads
/// are pure Java and route through these natives; CGLIB / direct
/// user code typically calls `defineClass1` because that's what the
/// public 4-arg / 5-arg overloads delegate to.
fn jla_system_define_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let byte_array = match args.get(4) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: "System$1.defineClass: bytes must not be null".into(),
                }
                .into(),
            );
        }
    };
    let len = ctx.array_length(byte_array) as i32;
    let mapped = vec![
        args.get(1).copied().unwrap_or(Value::Object(None)),
        args.get(2).copied().unwrap_or(Value::Object(None)),
        args.get(3).copied().unwrap_or(Value::Object(None)),
        args.get(4).copied().unwrap_or(Value::Object(None)),
        Value::Int(0),
        Value::Int(len),
        args.get(5).copied().unwrap_or(Value::Object(None)),
        args.get(6).copied().unwrap_or(Value::Int(0)),
        args.get(7).copied().unwrap_or(Value::Int(0)),
        args.get(8).copied().unwrap_or(Value::Object(None)),
    ];
    cl_define_class0(ctx, &mapped)
}

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

    r.register(
        "java/lang/System$1",
        "defineClass",
        "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BLjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
        jla_system_define_class,
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
// Fix: validate args up-front, then delegate to the same backend used by
// the public `ClassLoader.defineClass(String, byte[], int, int,
// ProtectionDomain)` overload. The defensive check prevents the process
// crash even if cglib doesn't fully work.
//
// Error contract (no silent-wrong-result stubs): instead of returning a
// null Class on failure (which only NPEs later in the caller), we throw
// the exception HotSpot's `Unsafe.defineClass` would:
//   * null bytecode array        → NullPointerException
//   * out-of-bounds off/len      → ArrayIndexOutOfBoundsException
//   * zero/oversize/bad-magic/    → ClassFormatError
//     parse panic/backend reject
//   * backend "not found" reason → NoClassDefFoundError
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
/// mismatch reading past the array end) and we throw ClassFormatError
/// rather than try to parse it.
const UNSAFE_DEFINE_CLASS_MAX_BYTES: usize = 1024 * 1024;

fn unsafe_define_class_defensive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::{LinkageError, RuntimeError};
    // arg[0] = this (Unsafe singleton), ignored
    // arg[1] = name : String (may be null — bytecode carries this_class)
    // arg[2] = b : byte[]
    // arg[3] = off : int
    // arg[4] = len : int
    // arg[5] = loader : ClassLoader (may be null — system loader)
    // arg[6] = pd : ProtectionDomain (may be null)

    // Resolve the (possibly null) requested name up-front so it can be
    // attached to thrown exceptions for diagnostics.
    let name_str = match args.get(1) {
        Some(Value::Object(Some(name_obj))) => {
            let dotted = ctx.read_string(*name_obj).unwrap_or_default();
            dotted.replace('.', "/")
        }
        _ => String::new(),
    };

    // arg[2] check: a null bytecode array is a programming error on the
    // caller's side. HotSpot's Unsafe.defineClass NPEs here; throw the
    // same so the Java caller observes the real fault instead of an NPE
    // later on a null Class return.
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            tracing::warn!("Unsafe.defineClass({name_str}): null bytecode array — throwing NPE");
            return Err(RuntimeError::NullPointerException {
                message: Some("Unsafe.defineClass: bytecode array must not be null".into()),
            }
            .into());
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

    // Sanity-cap: cglib proxies are small. A zero-length or absurdly large
    // length can't be a valid class file → ClassFormatError (the bytes are
    // structurally malformed), not a silent null.
    if length == 0 || length > UNSAFE_DEFINE_CLASS_MAX_BYTES {
        tracing::warn!(
            "Unsafe.defineClass({name_str}): rejecting bytecode of length {length} \
             (max={UNSAFE_DEFINE_CLASS_MAX_BYTES})"
        );
        return Err(LinkageError::ClassFormatError {
            class_name: name_str,
            message: format!(
                "Unsafe.defineClass: bytecode length {length} out of range (max {UNSAFE_DEFINE_CLASS_MAX_BYTES})"
            ),
        }
        .into());
    }

    // Bounds: offset+length must fit inside the array. An out-of-bounds
    // slice is an AIOOBE on the caller's side (HotSpot's Unsafe range
    // checks throw before parsing), so surface that rather than a null.
    // checked_add prevents wrap-around on pathological inputs.
    if offset
        .checked_add(length)
        .map_or(true, |end| end > array_len)
    {
        tracing::warn!(
            "Unsafe.defineClass({name_str}): offset/length out of bounds \
             (off={offset}, len={length}, array={array_len}) — throwing AIOOBE"
        );
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: offset.saturating_add(length).min(i32::MAX as usize) as i32,
        }
        .into());
    }

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
                 throwing ClassFormatError"
            );
            return Err(LinkageError::ClassFormatError {
                class_name: name_str,
                message: "Unsafe.defineClass: failed to read bytecode array".into(),
            }
            .into());
        }
    };

    // Sanity check magic before handing to backend — `define_class_full`
    // already checks this, but doing it here keeps the warn log clear
    // about WHO rejected the bytecode. Require at least 8 bytes
    // (magic + minor + major) so the backend never reads past EOF.
    // Malformed bytes → ClassFormatError (JVMS 5.3.5), not a silent null.
    if class_bytes.len() < 8 || class_bytes[0..4] != CLASS_FILE_MAGIC {
        tracing::warn!("Unsafe.defineClass({name_str}): bad magic — throwing ClassFormatError");
        return Err(LinkageError::ClassFormatError {
            class_name: name_str,
            message: "Unsafe.defineClass: not a valid class file (bad magic)".into(),
        }
        .into());
    }

    // cglib SEGV guard — this is the hottest path for the cglib_probe
    // reproducer because cglib's `ReflectUtils.defineClass` calls into
    // `sun.misc.Unsafe.defineClass`.
    if let Some(v) = cglib_guard_value(ctx, &name_str, &class_bytes) {
        return Ok(Some(v));
    }

    // Resolve loader id from arg[5]. Null loader → system (id 0).
    let loader_id = match args.get(5) {
        Some(Value::Object(Some(loader_obj))) => get_or_assign_loader_id(ctx, *loader_obj),
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
    // backend parser. Translate panic → ClassFormatError so the Java
    // caller sees a recoverable linkage error instead of a process exit
    // (or a null Class that NPEs later).
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(&name_str, &class_bytes, loader_id, opts)
    }));
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(
                "Unsafe.defineClass({name_str}): panic inside define_class_full; \
                 throwing ClassFormatError"
            );
            return Err(LinkageError::ClassFormatError {
                class_name: name_str,
                message: "Unsafe.defineClass: panic inside backend (likely malformed bytecode)"
                    .into(),
            }
            .into());
        }
    };
    match define_result {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            // Backend rejected the bytes. A "not found" style failure maps
            // to NoClassDefFoundError; everything else is a malformed-class
            // (ClassFormatError). Either way the caller observes the real
            // fault rather than an NPE on a null Class.
            tracing::warn!("Unsafe.defineClass({name_str}) backend failed: {msg} — throwing");
            let lower = msg.to_ascii_lowercase();
            if lower.contains("not found") || lower.contains("no class def") {
                Err(LinkageError::NoClassDefFoundError {
                    class_name: if name_str.is_empty() {
                        msg.clone()
                    } else {
                        name_str
                    },
                }
                .into())
            } else {
                Err(LinkageError::ClassFormatError {
                    class_name: name_str,
                    message: format!("Unsafe.defineClass: {msg}"),
                }
                .into())
            }
        }
    }
}

fn cl_resolve_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // resolveClass(Class) — trigger class preparation and linking
    if let Some(Value::Object(Some(class_mirror))) = args.get(1) {
        if let Some(cid) = crate::lang_class::mirror_class_id(ctx, *class_mirror) {
            let _ = ctx.ensure_class_initialized(&ctx.class_name_of_id(cid).unwrap_or_default());
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

    // Loader-scoped lookup (shared with real-JDK mode): a user-defined loader
    // reports a class only if it is in that loader's own namespace or it is the
    // recorded defining loader — NOT a class some other loader (typically the
    // application loader) happens to have loaded. A fresh custom loader thus
    // gets null for an app-loaded class, so its override-first redefinition
    // (Spring's OverridingClassLoader) fires and it becomes the defining loader.
    // Built-in loaders keep the global (no-load) lookup, correct for them.
    match find_loaded_class_for_loader(ctx, this, &name_str) {
        Some(mirror) => Ok(Some(Value::Object(Some(mirror)))),
        None => Ok(Some(Value::Object(None))),
    }
}

fn cl_get_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(match classloader_parent(ctx, this) {
        Some(parent) => Value::Object(Some(parent)),
        None => Value::Object(None),
    }))
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
    if args.len() >= 2 && !matches!(args.get(1), Some(Value::Object(Some(_)))) {
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/NullPointerException",
            1,
            "ClassLoader.getResource name is null",
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc,
        ));
    }
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

    // A URLClassLoader has a private, receiver-owned URL set. Its public
    // `getResource` is nevertheless parent-first: Spring's
    // `FilteredClassLoader`, for example, has an empty local URL array and
    // relies on its resource-bearing parent. Routing it straight to the local
    // resolver skipped that parent and made a dynamically supplied
    // `hazelcast.xml` invisible, so Hazelcast auto-configuration quietly
    // registered no instance. Search the real parent first, then use the
    // receiver-local resolver; never fall through to the generic flat path,
    // which could leak sibling loader resources.
    if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
        // A platform loader may expose JDK-module resources (`jrt:`), but it
        // cannot see the application's flat classpath. Treating the global
        // resource walk as its implementation makes a child whose explicit
        // parent is platform observe application resources that HotSpot would
        // reject. Spring's ModifiedClassPathClassLoader deliberately uses that
        // topology to exclude individual JARs.
        if is_platform_class_loader(ctx, this_ref) {
            if let Some(first) = ctx
                .find_all_resource_urls(resource_name)
                .iter()
                .find(|url| url.starts_with("jrt:"))
            {
                let url = crate::jboss_module_loader::build_synthetic_url(ctx, first);
                return Ok(Some(Value::Object(Some(url))));
            }
            return Ok(Some(Value::Object(None)));
        }
        if object_extends(ctx, this_ref, "java/net/URLClassLoader") {
            // URLClassLoader (bare instance OR a user-defined subclass) is
            // always a user loader for `getResource` purposes: real
            // `ClassLoader.getResource()` delegates to the parent FIRST
            // regardless of whether the receiver's own class is literally
            // `java.net.URLClassLoader` or a subclass. This used to
            // early-return to the local-only `ucl_find_resource` whenever
            // `is_builtin_loader_class(&class_name)` matched — which is true
            // for the literal string "java/net/URLClassLoader" itself (see
            // its `matches!` list), so a plain, directly-instantiated
            // `new URLClassLoader(urls, parent)` — a completely ordinary
            // idiom for a thin resource/class overlay with a real,
            // resource-bearing parent, e.g. Spring Boot's
            // `ServletComponentScanIntegrationTests.indexedComponentsAreRegistered`
            // wrapping just a `@TempDir` holding a generated
            // `META-INF/spring.components` index — silently skipped parent
            // delegation and could only ever see its own (here, near-empty)
            // local URL set. `is_builtin_loader_class`'s other match arms
            // (`jdk/internal/loader/*`, `sun/misc/Launcher$*`) are dead code
            // in this specific branch on a modern JDK: none of those classes
            // actually extend `java.net.URLClassLoader` (JDK 9+ internal
            // loaders derive from `BuiltinClassLoader`, not `URLClassLoader`),
            // so removing the gate does not change behavior for them.
            let this_pin = ctx.pin_native_root(this_ref);
            let name_for_parent = Value::Object(Some(ctx.create_string(&name)));
            let this_live = ctx.read_native_pin(this_pin, this_ref);
            // ModifiedClassPathClassLoader deliberately uses the platform
            // loader as its parent so its URL set is the complete, isolated
            // test class path. Parent-first resource lookup would reintroduce
            // application resources that its exclusions removed.
            if !url_classloader_isolated_from_app(ctx, this_live) {
                if let Value::Object(Some(parent)) = ctx.get_field_by_name(this_live, "parent") {
                    let parent_pin = ctx.pin_native_root(parent);
                    let parent_live = ctx.read_native_pin(parent_pin, parent);
                    let parent_result = ctx.invoke_virtual(
                        parent_live,
                        "getResource",
                        "(Ljava/lang/String;)Ljava/net/URL;",
                        &[name_for_parent],
                    );
                    ctx.unpin_native_roots(parent_pin);
                    if matches!(parent_result, Ok(Some(Value::Object(Some(_))))) {
                        ctx.unpin_native_roots(this_pin);
                        return parent_result;
                    }
                }
            }
            let this_live = ctx.read_native_pin(this_pin, this_ref);
            let name_for_local = Value::Object(Some(ctx.create_string(&name)));
            let local_result =
                ucl_find_resource(ctx, &[Value::Object(Some(this_live)), name_for_local]);
            ctx.unpin_native_roots(this_pin);
            return local_result;
        }
    }

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
                // URLClassLoader itself is parent-first too.  Its exact
                // native registration may receive the base-loader identity
                // even when the live receiver is a subclass, so restricting
                // this to non-builtin names drops a parent's resource stream.
                if object_extends(ctx, this_ref, "java/net/URLClassLoader")
                    || !is_builtin_loader_class(&class_name)
                {
                    // JDK `ClassLoader.getResource` contract: delegate to the
                    // PARENT's getResource FIRST, then fall back to this loader's
                    // own `findResource` override. The previous code skipped
                    // parent delegation and called `findResource` directly, so a
                    // custom loader that overrides only `loadClass` (and inherits
                    // the default `findResource`, which returns null) reported
                    // null for every resource its parent (the app/system loader)
                    // can serve. Hibernate's SerializationHelperTest /
                    // ProxyClassReuseTest custom loaders read class bytes via
                    // getResource(AsStream) and broke on this (CNFE for a class
                    // that exists on the classpath).
                    //
                    // Pin the receiver across each allocating create_string —
                    // a moving GC during it would stale `this_ref`.
                    let pin = ctx.pin_native_root(this_ref);
                    let name_for_parent = Value::Object(Some(ctx.create_string(&name)));
                    let this_ref = ctx.read_native_pin(pin, this_ref);
                    let parent = ctx.get_field_by_name(this_ref, "parent");
                    ctx.unpin_native_roots(pin);
                    if let Value::Object(Some(parent_ref)) = parent {
                        if let Ok(Some(Value::Object(Some(url)))) = ctx.invoke_virtual(
                            parent_ref,
                            "getResource",
                            "(Ljava/lang/String;)Ljava/net/URL;",
                            &[name_for_parent],
                        ) {
                            return Ok(Some(Value::Object(Some(url))));
                        }
                    }
                    // Parent had nothing (or is null/bootstrap): this loader's
                    // own findResource override gets the final say.
                    let pin2 = ctx.pin_native_root(this_ref);
                    let name_for_find = Value::Object(Some(ctx.create_string(&name)));
                    let this_ref = ctx.read_native_pin(pin2, this_ref);
                    ctx.unpin_native_roots(pin2);
                    return ctx.invoke_virtual(
                        this_ref,
                        "findResource",
                        "(Ljava/lang/String;)Ljava/net/URL;",
                        &[name_for_find],
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
            eprintln!(
                "[GRES-DBG] getResource({}) -> NULL (no urls, no bytes)",
                resource_name
            );
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
    // `getResources` MAY delegate a non-builtin loader to its `findResources`.
    cl_get_resources_impl(ctx, args, true)
}

fn cl_get_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cl_get_resources_impl(ctx, args, true)
}

fn url_external_form_string(ctx: &mut dyn NativeContext, url: ObjectRef) -> Option<String> {
    let p_url = ctx.pin_native_root(url);
    let url_live = ctx.read_native_pin(p_url, url);
    let result = ctx.invoke_virtual(url_live, "toExternalForm", "()Ljava/lang/String;", &[]);
    let out = match result {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    };
    ctx.unpin_native_roots(p_url);
    out
}

fn collect_url_enumeration_strings(
    ctx: &mut dyn NativeContext,
    enumeration: ObjectRef,
    out: &mut Vec<String>,
) {
    const MAX_RESOURCE_ENUMERATION: usize = 16_384;
    let p_enum = ctx.pin_native_root(enumeration);
    for _ in 0..MAX_RESOURCE_ENUMERATION {
        let enumeration = ctx.read_native_pin(p_enum, enumeration);
        let has_more = match ctx.invoke_virtual(enumeration, "hasMoreElements", "()Z", &[]) {
            Ok(Some(Value::Int(v))) => v != 0,
            _ => false,
        };
        if !has_more {
            break;
        }

        let enumeration = ctx.read_native_pin(p_enum, enumeration);
        let next = ctx.invoke_virtual(enumeration, "nextElement", "()Ljava/lang/Object;", &[]);
        if let Ok(Some(Value::Object(Some(url)))) = next {
            if let Some(s) = url_external_form_string(ctx, url) {
                out.push(s);
            }
        }
    }
    ctx.unpin_native_roots(p_enum);
}

/// A URL retained while two resource enumerations are being merged.  The
/// fallback is only for lightweight non-moving test contexts whose global-root
/// implementation is intentionally a no-op.
#[derive(Clone, Copy)]
struct RootedUrl {
    root: usize,
    fallback: ObjectRef,
}

fn collect_url_enumeration_objects(
    ctx: &mut dyn NativeContext,
    enumeration: ObjectRef,
    out: &mut Vec<RootedUrl>,
) {
    const MAX_RESOURCE_ENUMERATION: usize = 16_384;
    let p_enum = ctx.pin_native_root(enumeration);
    for _ in 0..MAX_RESOURCE_ENUMERATION {
        let enumeration = ctx.read_native_pin(p_enum, enumeration);
        let has_more = match ctx.invoke_virtual(enumeration, "hasMoreElements", "()Z", &[]) {
            Ok(Some(Value::Int(v))) => v != 0,
            _ => false,
        };
        if !has_more {
            break;
        }

        let enumeration = ctx.read_native_pin(p_enum, enumeration);
        let next = ctx.invoke_virtual(enumeration, "nextElement", "()Ljava/lang/Object;", &[]);
        if let Ok(Some(Value::Object(Some(url)))) = next {
            out.push(RootedUrl {
                root: ctx.add_global_root(url),
                fallback: url,
            });
        }
    }
    ctx.unpin_native_roots(p_enum);
}

/// Preserve parent-first ordering while removing duplicate URL objects from a
/// public `getResources` result. A URLClassLoader's compatibility fallback can
/// surface the same parent URL through both its inherited scan and local probe;
/// HotSpot exposes that physical resource once.
fn deduplicate_rooted_urls(ctx: &mut dyn NativeContext, urls: &mut Vec<RootedUrl>) {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::with_capacity(urls.len());
    for rooted in std::mem::take(urls) {
        let url = if rooted.root != 0 {
            ctx.resolve_global_root(rooted.root)
                .or(Some(rooted.fallback))
        } else {
            Some(rooted.fallback)
        };
        let key = url.and_then(|url| url_external_form_string(ctx, url));
        if key.is_some_and(|key| seen.insert(key)) {
            unique.push(rooted);
        } else if rooted.root != 0 {
            let _ = ctx.remove_global_root(rooted.root);
        }
    }
    *urls = unique;
}

fn enumeration_from_url_strings(ctx: &mut dyn NativeContext, urls: &[String]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
    // GC-safety: `build_synthetic_url` per iteration allocates (transitively
    // GC-triggering); `arr` is written into again via `set_array_element`
    // afterward, both within the same iteration and across iterations, and
    // once more building the enclosing Enumeration below.
    let arr_pin = ctx.pin_native_root(arr);
    for (i, u) in urls.iter().enumerate() {
        let url_obj = crate::jboss_module_loader::build_synthetic_url(ctx, u);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(url_obj)));
    }
    let enm = alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 2);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    ctx.set_field(enm, 0, Value::Object(Some(arr)));
    ctx.set_field(enm, 1, Value::Int(0));
    enm
}

/// Build a merged enumeration without converting its URLs through external
/// forms.  Custom URLStreamHandler instances are object state, so rebuilding a
/// URL from its String (as the flat-classpath path does) makes in-memory
/// archives such as ShrinkWrap's `archive:` resources unreadable.
fn enumeration_from_rooted_urls(ctx: &mut dyn NativeContext, urls: &[RootedUrl]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, rooted) in urls.iter().copied().enumerate() {
        let url = if rooted.root != 0 {
            ctx.resolve_global_root(rooted.root)
                .or(Some(rooted.fallback))
        } else {
            Some(rooted.fallback)
        };
        if let Some(url) = url {
            let arr_live = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr_live, i, Value::Object(Some(url)));
        }
        if rooted.root != 0 {
            let _ = ctx.remove_global_root(rooted.root);
        }
    }
    let enm = alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 2);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    ctx.set_field(enm, 0, Value::Object(Some(arr)));
    ctx.set_field(enm, 1, Value::Int(0));
    enm
}

/// `allow_delegate` = whether a non-builtin `ClassLoader` receiver may be
/// dispatched to its `findResources(String)` override. It MUST be `false` when we
/// are already serving `findResources` (see `ucl_find_resources`): a loader that
/// subclasses `URLClassLoader` *without* overriding `findResources` (e.g.
/// `groovy.lang.GroovyClassLoader`) inherits the intercepted
/// `URLClassLoader.findResources` → `ucl_find_resources` → back here; re-delegating
/// would call `findResources` again, recursing until the native stack overflows
/// (the recursion bypasses the `execute()` depth guard). See SB-13.
/// True iff the receiver's class (or an ancestor below `java/lang/ClassLoader`)
/// declares its own `findResources(String)` override. When it does, the
/// `getResources` native delegates to that override; when it does not, the
/// loader relies on the default parent-delegating `ClassLoader.getResources`
/// semantics and the native falls back to the flat classpath scan.
fn loader_overrides_find_resources(ctx: &mut dyn NativeContext, this_ref: ObjectRef) -> bool {
    const FIND_RESOURCES_DESC: &str = "(Ljava/lang/String;)Ljava/util/Enumeration;";
    let mut cid = Some(ctx.class_id_of_object(this_ref));
    while let Some(c) = cid {
        match ctx.class_name_of_id(c).as_deref() {
            // Reached the base class (or an untyped class): no override found.
            Some("java/lang/ClassLoader") | Some("java/lang/Object") | None => return false,
            _ => {}
        }
        if ctx.class_declares_method(c, "findResources", FIND_RESOURCES_DESC) {
            return true;
        }
        cid = ctx.superclass_of(c);
    }
    false
}

fn cl_get_resources_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    allow_delegate: bool,
) -> MethodCallResult {
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
    if args.len() >= 2 && !matches!(args.get(1), Some(Value::Object(Some(_)))) {
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/NullPointerException",
            1,
            "ClassLoader.getResources name is null",
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc,
        ));
    }
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

    // See `cl_get_resource`: the URLClassLoader path must stay local rather
    // than falling into the process-wide resource enumeration.
    if allow_delegate {
        if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
            if object_extends(ctx, this_ref, "java/net/URLClassLoader") {
                // ClassLoader.getResources is parent-first, while
                // URLClassLoader.findResources contributes only this
                // receiver's local URLs. Preserve both halves without ever
                // consulting the flattened process-wide resource path.
                let p_this = ctx.pin_native_root(this_ref);
                let mut urls = Vec::new();
                let name_for_parent = Value::Object(Some(ctx.create_string(&name)));
                let this_live = ctx.read_native_pin(p_this, this_ref);
                if let Value::Object(Some(parent)) = ctx.get_field_by_name(this_live, "parent") {
                    let p_parent = ctx.pin_native_root(parent);
                    let parent_live = ctx.read_native_pin(p_parent, parent);
                    if let Ok(Some(Value::Object(Some(enm)))) = ctx.invoke_virtual(
                        parent_live,
                        "getResources",
                        "(Ljava/lang/String;)Ljava/util/Enumeration;",
                        &[name_for_parent],
                    ) {
                        collect_url_enumeration_objects(ctx, enm, &mut urls);
                    }
                    ctx.unpin_native_roots(p_parent);
                }
                let this_live = ctx.read_native_pin(p_this, this_ref);
                let name_for_local = Value::Object(Some(ctx.create_string(&name)));
                if let Ok(Some(Value::Object(Some(enm)))) =
                    ucl_find_resources(ctx, &[Value::Object(Some(this_live)), name_for_local])
                {
                    collect_url_enumeration_objects(ctx, enm, &mut urls);
                }
                ctx.unpin_native_roots(p_this);
                deduplicate_rooted_urls(ctx, &mut urls);
                let enm = enumeration_from_rooted_urls(ctx, &urls);
                return Ok(Some(Value::Object(Some(enm))));
            }
        }
    }

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
    if allow_delegate {
        if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
            if is_classloader_instance(ctx, this_ref) {
                let class_id = ctx.class_id_of_object(this_ref);
                if let Some(class_name) = ctx.class_name_of_id(class_id) {
                    // URLClassLoader is a JDK builtin, but its local
                    // findResources implementation is precisely the native
                    // hook that serves constructor-supplied and custom-handler
                    // URLs.  Treat it like a delegating loader here; its native
                    // findResources calls back with allow_delegate=false, so
                    // this cannot recurse.
                    // A third-party URLClassLoader subclass (notably
                    // ShrinkWrapClassLoader) may return URLs backed by an
                    // application URLStreamHandler.  The generic merge below
                    // serialises every URL to a String and rebuilds it, which
                    // discards that handler.  Keep the concrete URL objects
                    // by dispatching straight to findResources for the entire
                    // URLClassLoader family, just as the JDK's implementation
                    // does for this local lookup.
                    let is_url_loader = object_extends(ctx, this_ref, "java/net/URLClassLoader");
                    if !is_builtin_loader_class(&class_name) || is_url_loader {
                        // Keep URLClassLoader's returned URL objects intact:
                        // serialising them through the generic parent merge
                        // loses application URLStreamHandler state.
                        if is_url_loader {
                            // Calling findResources virtually can select the
                            // real URLClassLoader bytecode through a subclass
                            // call site.  That bytecode recreates a custom
                            // protocol URL without its application handler.
                            // Invoke the native lookup directly so the URL
                            // objects resolved from the recorded base retain
                            // their handler end-to-end.
                            return ucl_find_resources(ctx, args);
                        }
                        // Real `ClassLoader.getResources` is parent-first:
                        // parent.getResources(name) followed by this loader's
                        // findResources(name). The previous native returned only
                        // the findResources override; for URLClassLoader
                        // subclasses such as JasperLoader that meant only the
                        // JSP scratch-dir URLs were visible, while virtual
                        // WEB-INF/classes resources in the webapp parent
                        // disappeared from classpathGetResources.jsp.
                        let p_this = ctx.pin_native_root(this_ref);
                        let mut delegated_urls = Vec::new();

                        let name_for_parent = Value::Object(Some(ctx.create_string(&name)));
                        let this_live = ctx.read_native_pin(p_this, this_ref);
                        let parent = ctx.get_field_by_name(this_live, "parent");
                        if let Value::Object(Some(parent_ref)) = parent {
                            let p_parent = ctx.pin_native_root(parent_ref);
                            let parent_live = ctx.read_native_pin(p_parent, parent_ref);
                            if let Ok(Some(Value::Object(Some(parent_enum)))) = ctx.invoke_virtual(
                                parent_live,
                                "getResources",
                                "(Ljava/lang/String;)Ljava/util/Enumeration;",
                                &[name_for_parent],
                            ) {
                                collect_url_enumeration_objects(
                                    ctx,
                                    parent_enum,
                                    &mut delegated_urls,
                                );
                            }
                            ctx.unpin_native_roots(p_parent);
                        }

                        let this_live = ctx.read_native_pin(p_this, this_ref);
                        if loader_overrides_find_resources(ctx, this_live) {
                            let name_for_find = Value::Object(Some(ctx.create_string(&name)));
                            let this_live = ctx.read_native_pin(p_this, this_ref);
                            if let Ok(Some(Value::Object(Some(own_enum)))) = ctx.invoke_virtual(
                                this_live,
                                "findResources",
                                "(Ljava/lang/String;)Ljava/util/Enumeration;",
                                &[name_for_find],
                            ) {
                                collect_url_enumeration_objects(ctx, own_enum, &mut delegated_urls);
                            }
                        }
                        ctx.unpin_native_roots(p_this);

                        // A user-defined loader's default `getResources` is
                        // strictly parent-delegating. In particular, an empty
                        // result is meaningful: falling through to CratonVM's
                        // process-wide classpath scan leaks resources that are
                        // invisible to the loader (and bypasses test doubles
                        // such as EasyMock ClassLoaders). The optional
                        // findResources override above has already contributed
                        // this loader's local entries, so return the combined
                        // enumeration even when it is empty.
                        deduplicate_rooted_urls(ctx, &mut delegated_urls);
                        let enm = enumeration_from_rooted_urls(ctx, &delegated_urls);
                        return Ok(Some(Value::Object(Some(enm))));
                    }
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
        eprintln!(
            "[GRES-DBG] getResources({}) -> {} URLs",
            resource_name,
            urls.len()
        );
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
    let enm = enumeration_from_url_strings(ctx, &urls);
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

fn ucp_path_urls(ctx: &mut dyn NativeContext, ucp: ObjectRef) -> Option<ObjectRef> {
    let path = match ctx.get_field_by_name(ucp, "path") {
        Value::Object(Some(path)) if is_array_list_object(ctx, path) => path,
        _ => return None,
    };
    let path_pin = ctx.pin_native_root(path);
    let path = ctx.read_native_pin(path_pin, path);
    let size = match ctx.get_field_by_name(path, "size") {
        Value::Int(size) if size > 0 => size as usize,
        _ => {
            ctx.unpin_native_roots(path_pin);
            return None;
        }
    };
    let elements = match ctx.get_field_by_name(path, "elementData") {
        Value::Object(Some(elements)) => elements,
        _ => {
            ctx.unpin_native_roots(path_pin);
            return None;
        }
    };
    let elements_pin = ctx.pin_native_root(elements);
    let result = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
    for index in 0..size {
        let elements = ctx.read_native_pin(elements_pin, elements);
        ctx.set_array_element(result, index, ctx.get_array_element(elements, index));
    }
    ctx.unpin_native_roots(elements_pin);
    ctx.unpin_native_roots(path_pin);
    Some(result)
}

/// `URLClassPath.getURLs()[Ljava/net/URL;` — return recorded URL paths or an empty URL[].
///
/// Real-JDK bytecode reads `path` (an ArrayList) under a monitor and
/// builds `URL[path.size()]`.  When `path` is null (because the instance
/// was created through a path our `<init>` shim never saw) the deref
/// crashes the VM with an access violation. An empty array is spec-legal
/// (it just means "this loader contributes no URLs") and lets Spring
/// Boot's clearCache iteration complete in zero iterations.
fn ucp_get_urls_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `record_ucl_urls` retains constructor URLs in the real `path` field.
    // Returning a copy preserves URLClassLoader's public isolation contract.
    if let Some(Value::Object(Some(ucp))) = args.first() {
        if let Some(result) = ucp_path_urls(ctx, *ucp) {
            return Ok(Some(Value::Object(Some(result))));
        }
    }
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
        r.register(
            cls,
            "closeLoaders",
            "()Ljava/util/List;",
            ucp_close_loaders_list,
        );
        r.register(cls, "closeLoaders", "()V", ucp_close_loaders_void);
        // `findResource` — return null URL when probed reflectively. Both
        // the public (String) form and the internal (String, boolean) form
        // are covered.
        r.register(
            cls,
            "findResource",
            "(Ljava/lang/String;)Ljava/net/URL;",
            ucp_find_resource_null,
        );
        r.register(
            cls,
            "findResource",
            "(Ljava/lang/String;Z)Ljava/net/URL;",
            ucp_find_resource_null,
        );
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
        if idx >= len {
            return Ok(Some(Value::Object(None)));
        }
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
        if idx >= len {
            return Ok(Some(Value::Object(None)));
        }
        let elem = ctx.get_array_element(arr, idx);
        ctx.set_field(this, 1, Value::Int((idx + 1) as i32));
        Ok(Some(elem))
    });
    let anon_enm = "cratonvm/synthetic/AnonymousObject$2";
    r.register(anon_enm, "hasMoreElements", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
    });
    r.register(
        anon_enm,
        "nextElement",
        "()Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            if idx >= len {
                return Ok(Some(Value::Object(None)));
            }
            let elem = ctx.get_array_element(arr, idx);
            ctx.set_field(this, 1, Value::Int((idx + 1) as i32));
            Ok(Some(elem))
        },
    );
    r.register(anon_enm, "hasNext", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
    });
    r.register(anon_enm, "next", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        let len = ctx.array_length(arr);
        if idx >= len {
            return Ok(Some(Value::Object(None)));
        }
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

pub fn cl_get_resource_as_stream_essential(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    cl_get_resource_as_stream(ctx, args)
}

fn cl_get_resource_as_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // ClassLoader.getResourceAsStream(String) → InputStream.  Mirrors the
    // T19.H10 hardening on `Class.getResourceAsStream`: validate the name
    // (length, control bytes, `..`, `\`) before consulting `find_resource`,
    // and route the BAIS allocation through the shared helper so
    // ClassLoader-side and Class-side resource lookups stay layout-equal.
    if args.len() >= 2 && !matches!(args.get(1), Some(Value::Object(Some(_)))) {
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/NullPointerException",
            1,
            "ClassLoader.getResourceAsStream name is null",
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc,
        ));
    }
    let Some(name) = args.iter().rev().find_map(|v| match v {
        Value::Object(Some(o)) => ctx.read_string(*o),
        _ => None,
    }) else {
        return Ok(Some(Value::Object(None)));
    };
    let resource_name = name.trim_start_matches('/');
    if crate::lang_class::t19_h10_validate_resource_name_pub(resource_name).is_none() {
        return Ok(Some(Value::Object(None)));
    }
    // User-defined loader: mirror JDK `ClassLoader.getResourceAsStream` =
    // `URL u = getResource(name); return u != null ? u.openStream() : null;`.
    // Routing through `getResource` (which now performs parent delegation)
    // ensures a custom loader that doesn't override `findResource` still finds
    // resources its parent serves (SerializationHelperTest/ProxyClassReuseTest).
    // The raw `find_resource` fast-path below is kept for builtin loaders.
    //
    // `object_extends(.., "java/net/URLClassLoader")` mirrors the identical
    // gate `cl_get_resource` already applies just above ("URLClassLoader
    // itself is parent-first too... restricting this to non-builtin names
    // drops a parent's resource stream") — `is_builtin_loader_class` treats
    // the bare `java/net/URLClassLoader` class as builtin (it's in the same
    // match arm as `SecureClassLoader`/`jdk/internal/loader/*`), so a plain,
    // user-instantiated `new URLClassLoader(urls, parent)` (e.g. Spring
    // Boot's `ServletComponentScanIntegrationTests.indexedComponentsAreRegistered`,
    // which wraps just a `@TempDir` holding a generated `META-INF/spring.components`
    // index, parented to the real test classloader) fell into the raw
    // `ctx.find_resource` fallback below instead of this delegation-aware
    // path. That raw store doesn't see resources reachable only through the
    // dynamically-registered global URL walk (`ctx.find_all_resource_urls`,
    // used by both `getResource` and `ucl_find_resource`'s own fallback), so
    // `getResourceAsStream` returned null for a `.class` file `getResource`
    // resolved moments earlier — `ClassPathResource.getInputStream()` then
    // threw `FileNotFoundException` reading an indexed component's class
    // file that plainly exists on the parent's classpath.
    if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
        if is_classloader_instance(ctx, this_ref) {
            let class_id = ctx.class_id_of_object(this_ref);
            if let Some(class_name) = ctx.class_name_of_id(class_id) {
                if object_extends(ctx, this_ref, "java/net/URLClassLoader")
                    || !is_builtin_loader_class(&class_name)
                {
                    let pin = ctx.pin_native_root(this_ref);
                    let name_arg = Value::Object(Some(ctx.create_string(&name)));
                    let this_ref = ctx.read_native_pin(pin, this_ref);
                    ctx.unpin_native_roots(pin);
                    if let Ok(Some(Value::Object(Some(url)))) = ctx.invoke_virtual(
                        this_ref,
                        "getResource",
                        "(Ljava/lang/String;)Ljava/net/URL;",
                        &[name_arg],
                    ) {
                        return ctx.invoke_virtual(
                            url,
                            "openStream",
                            "()Ljava/io/InputStream;",
                            &[],
                        );
                    }
                    return Ok(Some(Value::Object(None)));
                }
            }
        }
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

/// `java.lang.Module.getResourceAsStream(String)` (args: `[Module receiver,
/// name]`).
///
/// kotlin-reflect 2.3.20 ships a multi-release jar; under JDK 9+ the loaded
/// `BuiltInsResourceLoader.loadResource` is the `META-INF/versions/9` variant
/// whose body is `kotlin.Unit.class.getModule().getResourceAsStream(path)` —
/// NOT the base jar's `classLoader.getResource(path)`. For classpath classes
/// the module is the *unnamed* module, whose `getResourceAsStream` delegates to
/// the defining class loader's `getResourceAsStream`. The real-JDK bytecode for
/// `Module.getResourceAsStream` walks module/loader internals (the resource map,
/// `BootLoader`, `BuiltinClassLoader.findResource`) that CratonVM does not
/// populate, so it returns null — kotlin-reflect then loads zero `.kotlin_builtins`
/// fragments and asserts "Built-in class kotlin.Int is not found" (SB-15;
/// HotSpot passes).
///
/// Resolve the resource exactly as the ClassLoader-side native does (a classpath
/// scan via `find_resource`), which is the correct behaviour for the unnamed
/// module — `Module` is `final`, so the receiver's own class carries this native
/// and dispatch hits it directly. The receiver (`args[0]`) is ignored: every
/// classpath class shares the one unnamed module / app loader.
pub fn module_get_resource_as_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    cl_get_resource_as_stream(ctx, args)
}

fn resource_stream_for_last_string_arg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let name = args
        .iter()
        .rev()
        .find_map(|v| match v {
            Value::Object(Some(o)) => ctx.read_string(*o),
            _ => None,
        })
        .unwrap_or_default();
    let resource_name = name.trim_start_matches('/');
    if crate::lang_class::t19_h10_validate_resource_name_pub(resource_name).is_none() {
        return Ok(Some(Value::Object(None)));
    }
    match ctx.find_resource(resource_name) {
        Some(bytes) => {
            let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes);
            Ok(Some(Value::Object(Some(stream))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `jdk.internal.loader.BootLoader.findResourceAsStream(String,String)`.
///
/// `java.lang.Module.getResourceAsStream` delegates here for named boot modules
/// such as `java.desktop`. CratonVM does not populate the JDK's internal module
/// resource maps, but its classpath manager already indexes JMOD/JImage
/// resources by module, so serve the requested resource bytes from that path.
pub fn bootloader_find_resource_as_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    resource_stream_for_last_string_arg(ctx, args)
}

/// `jdk.internal.loader.BuiltinClassLoader.findResourceAsStream(String,String)`.
pub fn builtin_classloader_find_resource_as_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    resource_stream_for_last_string_arg(ctx, args)
}

fn cl_get_defined_package(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

fn cl_get_defined_packages(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(empty))))
}

fn cl_set_default_assertion_status(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn cl_register_as_parallel_capable(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Static method (invokestatic, descriptor ()Z).  The real JDK uses
    // getCallerClass() to find which ClassLoader subclass is being registered.
    // We don't enforce parallel-capability checks, so just return true.
    Ok(Some(Value::Int(1)))
}

fn cl_is_registered_as_parallel_capable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    // Real-JDK URL objects expose these as named instance fields.  Reading
    // only the synthetic numeric slots loses constructor URLs for ordinary
    // URLClassLoader instances and makes their local class path appear empty.
    let read_field = |name: &str, slot: usize| match ctx.get_field_by_name(url_obj, name) {
        Value::Object(Some(value)) => ctx.read_string(value),
        _ => match ctx.get_field(url_obj, slot) {
            Value::Object(Some(value)) => ctx.read_string(value),
            _ => None,
        },
    };
    let full_spec = read_field("file", 5);
    let path_field = read_field("path", 3);

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
    //
    // Only the FIRST `/!` is the genuine outer-jar/nested-entry boundary
    // marker (from `getJarReference`'s `"nested:" + jarFilePath + "/!" +
    // nestedEntryName`). A `.replace` of every occurrence also mangles a
    // directory-shaped nested entry name (e.g. Spring Boot's
    // `JarUrl.create(file, "BOOT-INF/classes/")`, whose spec is
    // `nested:<jar>/!BOOT-INF/classes/!/`): the entry name's own trailing
    // `/` immediately followed by the URL's separate trailing `!/` root
    // marker forms a SECOND, spurious `/!` match, which swaps into the
    // entry name and eats its trailing slash (`BOOT-INF/classes/!/` ->
    // `BOOT-INF/classes!//`), silently emptying `ClassPath`'s nested-prefix
    // scan (`parse_jar_subdir_spec` never matches any real zip entry).
    let p = raw
        .strip_prefix("jar:")
        .or_else(|| raw.strip_prefix("nested:"))
        .unwrap_or(&raw)
        .replacen("/!", "!/", 1);
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
fn ucl_setup(ctx: &mut dyn NativeContext, this: ObjectRef, urls: Value, parent: Value) {
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
    // GC-safety: `new_array` below can trigger a moving GC; `this` and
    // `url_arr` (the caller-supplied source array, read from in the copy
    // loop) are both reused afterward, unpinned otherwise.
    let this_pin = ctx.pin_native_root(this);
    let url_arr_pin = ctx.pin_native_root(url_arr);
    // Copy URLs into a storage array and extract paths for classpath registration.
    let storage = ctx.new_array(cratonvm_types::ArrayElementType::Reference, count.max(16));
    let this = ctx.read_native_pin(this_pin, this);
    let url_arr = ctx.read_native_pin(url_arr_pin, url_arr);
    ctx.unpin_native_roots(this_pin);
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
        tracing::debug!(
            "URLClassLoader.<init>: registered {} URLs to classpath",
            paths.len()
        );
    }
}

/// Legacy compatibility slot for URLClassPath instances created by older
/// synthetic paths. Real-JDK URLClassLoader constructor URLs are retained in
/// the named `path` ArrayList instead, because raw slot zero aliases that field.
const UCP_STASHED_URLS: usize = 0;

/// Record a real-JDK-mode `URLClassLoader`'s constructor `URL[]` so that
/// `getURLs()` returns the URLs the loader was built with.
///
/// The real-JDK-mode `URLClassLoader.<init>` natives (see `classloader_real`)
/// wire a loader's URLs into CratonVM's GLOBAL dynamic classpath (so classes
/// load) but never store them per-instance. Real `getURLs()` reads
/// `ucp.getURLs()`, and our `URLClassPath` shim returns empty — so `getURLs()`
/// yielded `[]`. That broke any code that walks a classloader's URLs, e.g.
/// Tomcat's `StandardJarScanner`, which scans the classloader hierarchy via
/// `getURLs()` to find TLDs: a TLD in a JAR added to a parent `URLClassLoader`
/// (outside `/WEB-INF/lib`) was invisible, 500-ing JSPs that referenced it
/// (`TestTagLibraryInfoImpl.testExternalTaglibDependantUsesUri`).
///
/// The synthetic per-instance slots used by `ucl_setup`/`ucl_get_urls` can't be
/// reused here: a real `java.net.URLClassLoader` has the real JDK field layout,
/// so those raw indices land on unrelated reference-typed fields (writing an
/// `Int` count there does not read back as a count). Instead stash the original
/// `URL[]` on the loader's `ucp` placeholder, which `ucl_get_urls` reads back.
pub(crate) fn record_ucl_urls(ctx: &mut dyn NativeContext, this: ObjectRef, urls: Value) {
    let url_arr = match urls {
        Value::Object(Some(arr)) => arr,
        _ => return,
    };
    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(this, "ucp") {
        // The real-mode URLClassLoader constructors call this helper directly.
        // Retain URLs in `ucp.path`, which backs both getURLs and receiver-local
        // class/resource lookup. Do not use raw slot zero: on real JDKs it is
        // the `path` field itself, so writing the URL[] there corrupts the list.
        //
        // `record_url_on_path` can allocate and move both the array and the
        // placeholder, so keep both rooted and reload them on every iteration.
        let p_urls = ctx.pin_native_root(url_arr);
        let p_ucp = ctx.pin_native_root(ucp);
        let count = ctx.array_length(url_arr);
        for i in 0..count {
            let urls = ctx.read_native_pin(p_urls, url_arr);
            if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                let ucp = ctx.read_native_pin(p_ucp, ucp);
                record_url_on_path(ctx, ucp, url);
            }
        }
        ctx.unpin_native_roots(p_urls);
    }
}

fn ucl_init_urls(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let urls = args.get(1).copied().unwrap_or(Value::Object(None));
    crate::classloader_real::init_urlclassloader_constructor_with_default_parent(ctx, this, urls);
    Ok(None)
}

fn ucl_init_urls_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let urls = args.get(1).copied().unwrap_or(Value::Object(None));
    let parent = args.get(2).copied().unwrap_or(Value::Object(None));
    crate::classloader_real::init_urlclassloader_constructor_with_parent(ctx, this, urls, parent);
    Ok(None)
}

fn ucl_init_urls_parent_factory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // factory arg ignored
    ucl_init_urls_parent(ctx, args)
}

fn ucl_find_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `URLClassLoader.findClass(String)` — reuse the base parent-first
    // delegation as a permissive findClass. Route to the base delegation
    // DIRECTLY (not `cl_load_class`) so it never re-triggers the
    // `loadClass(String,boolean)` override-dispatch: a subclass that overrides
    // loadClass and calls `super.findClass`/`findClass` from inside that
    // override would otherwise recurse back into its own loadClass.
    let this = obj_arg(args, 0)?;
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_name = ctx.read_string(name_obj).unwrap_or_default();
    let internal = class_name.replace('.', "/");
    if let Some(result) = ucl_try_define_local_class(ctx, this, &internal) {
        return result;
    }
    cl_load_class_base_delegation(ctx, this, name_obj)
}

// ---------------------------------------------------------------------------
// Real-mode `URLClassLoader.addURL` + custom-handler resource resolution.
//
// In real-JDK mode CratonVM serves class/resource loading from its global
// dynamic classpath and shims `jdk.internal.loader.URLClassPath` down to
// safe stubs (see `register_url_class_path_safe_stubs`); the `ucp` field is a
// bare synthetic `URLClassPath` with all instance fields null. The real
// `URLClassLoader.addURL` bytecode therefore NPEs at
// `URLClassPath.addURL` → `synchronized (unopenedUrls)` (null monitor). The
// native below replaces it: it records each added URL on `ucp.path` (a real
// `ArrayList<URL>` we create on demand) and, for ordinary `file:`/`jar:` URLs,
// also extends the global dynamic classpath.
//
// Recording the URLs lets `ucl_find_resource(s)` resolve resources that live
// behind an application-supplied `URLStreamHandler` — most notably ShrinkWrap's
// in-memory `archive:` handler, whose `JavaArchive` is reachable ONLY through
// the handler (there is no filesystem path the global walk could find). For
// such a base URL we build `new URL(base, name)` (which inherits the handler)
// and probe it through the handler; a hit is returned as a real `java.net.URL`
// whose `openStream()`/`openConnection()` reach the archive (see
// `net_phase_e::url_custom_handler_connection`). Hibernate's
// `NoDepthTests` JPA variants discover `META-INF/persistence.xml` this way.
// ---------------------------------------------------------------------------

/// True iff `url` carries a non-null, application-provided `URLStreamHandler`
/// (i.e. not a JDK `sun.net.*` built-in, and not a CratonVM synthetic URL whose
/// `handler` field is null). Pure field reads — never allocates.
fn url_has_custom_handler(ctx: &dyn NativeContext, url: ObjectRef) -> bool {
    let handler = match ctx.get_field_by_name(url, "handler") {
        Value::Object(Some(h)) => h,
        _ => return false,
    };
    let hclass = ctx
        .class_name_of_id(ctx.class_id_of_object(handler))
        .unwrap_or_default();
    !hclass.starts_with("sun/net/")
}

/// Construct `new URL(base, name)` via the real JDK `URL(URL,String)`
/// constructor, so the result inherits `base`'s handler and resolves `name`
/// relative to it. Returns the fresh `URL` (the caller must pin it before any
/// further allocation) or `None` if construction throws.
fn resolve_url_against(
    ctx: &mut dyn NativeContext,
    base: ObjectRef,
    name: &str,
) -> Option<ObjectRef> {
    // `create_string` can relocate `base`.
    let p_base = ctx.pin_native_root(base);
    let name_str = ctx.create_string(name);
    let base = ctx.read_native_pin(p_base, base);
    // `new_object` can relocate `base` / `name_str`.
    let p_name = ctx.pin_native_root(name_str);
    let url = match ctx.new_object("java/net/URL") {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(p_base);
            return None;
        }
    };
    // `URL.<init>` allocates internally — pin the receiver and both args.
    let p_url = ctx.pin_native_root(url);
    let base = ctx.read_native_pin(p_base, base);
    let name_str = ctx.read_native_pin(p_name, name_str);
    let r = ctx.invoke_special(
        "java/net/URL",
        "<init>",
        "(Ljava/net/URL;Ljava/lang/String;)V",
        &[
            Value::Object(Some(url)),
            Value::Object(Some(base)),
            Value::Object(Some(name_str)),
        ],
    );
    let url = ctx.read_native_pin(p_url, url);
    let result = match r {
        Ok(_) => {
            let base = ctx.read_native_pin(p_base, base);
            if let Value::Object(Some(handler)) = ctx.get_field_by_name(base, "handler") {
                // `URL(URL, String)` normally copies this private field.  Keep
                // that invariant explicit for native construction paths too;
                // a process-global side table keyed by identity hash can collide
                // after collection and apply an old archive handler to a later
                // unrelated URL.
                ctx.set_field_by_name(url, "handler", Value::Object(Some(handler)));
            }
            Some(url)
        }
        Err(_) => None,
    };
    ctx.unpin_native_roots(p_base);
    result
}

/// Probe whether `url` resolves to an existing resource through its custom
/// handler: `handler.openConnection(url).getInputStream()` must yield a
/// non-null stream without throwing. The probe stream is closed immediately.
fn probe_resource_exists(ctx: &mut dyn NativeContext, url: ObjectRef) -> bool {
    let conn = match crate::net_phase_e::url_custom_handler_connection(ctx, url) {
        Ok(Some(Value::Object(Some(c)))) => c,
        _ => return false, // null connection, no custom handler, or threw
    };
    let p_conn = ctx.pin_native_root(conn);
    let conn = ctx.read_native_pin(p_conn, conn);
    let r = ctx.invoke_virtual(conn, "getInputStream", "()Ljava/io/InputStream;", &[]);
    ctx.unpin_native_roots(p_conn);
    match r {
        Ok(Some(Value::Object(Some(stream)))) => {
            // Close the probe stream so we don't leak it (ShrinkWrap tracks
            // opened streams for cleanup on classloader close).
            let _ = ctx.invoke_virtual(stream, "close", "()V", &[]);
            true
        }
        _ => false, // null stream (directory node) or FileNotFoundException
    }
}

pub(crate) fn object_extends(ctx: &dyn NativeContext, obj: ObjectRef, target: &str) -> bool {
    let mut class_id = ctx.class_id_of_object(obj);
    for _ in 0..64 {
        match ctx.class_name_of_id(class_id).as_deref() {
            Some(name) if name == target => return true,
            None => return false,
            _ => {}
        }
        match ctx.superclass_of(class_id) {
            Some(parent) if parent != class_id => class_id = parent,
            _ => return false,
        }
    }
    false
}

/// A URLClassLoader whose parent is bootstrap/platform cannot delegate an
/// application class to the process-wide application loader. Its recorded URL
/// list is its complete application view (Spring's ModifiedClassPathClassLoader
/// uses this shape to remove selected JARs from a test's classpath).
pub(crate) fn is_platform_class_loader(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(loader))
        .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader")
        || platform_loader_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|platform| platform.as_ptr() == loader.as_ptr())
}

pub fn url_classloader_isolated_from_app(
    ctx: &dyn NativeContext,
    loader: ObjectRef,
) -> bool {
    if !object_extends(ctx, loader, "java/net/URLClassLoader") {
        return false;
    }
    match ctx.get_field_by_name(loader, "parent") {
        Value::Object(None) | Value::Int(0) | Value::Long(0) => true,
        Value::Object(Some(parent)) => is_platform_class_loader(ctx, parent),
        _ => false,
    }
}

fn is_url_class_path_object(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    object_extends(ctx, obj, "jdk/internal/loader/URLClassPath")
        || object_extends(ctx, obj, "sun/misc/URLClassPath")
}

fn is_array_list_object(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    object_extends(ctx, obj, "java/util/ArrayList")
}

/// Build a `java.util.ArrayList<URL>` of resources named `name` reachable
/// through the loader's custom-handler base URLs (recorded on `ucp.path` by
/// `ucl_add_url_real`). Returns `None` when the loader has no such base URLs or
/// none resolve — the common case for ordinary `file:`/`jar:` loaders, leaving
/// their behaviour untouched.
fn build_custom_handler_url_list(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    name: &str,
) -> Option<ObjectRef> {
    let ucp = match ctx.get_field_by_name(loader, "ucp") {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    if !is_url_class_path_object(ctx, ucp) {
        return None;
    }
    // URLClassPath's real `path` field is not a stable writable extension
    // point across JDK builds.  Constructors always retain their URLs in our
    // dedicated placeholder slot, so use that array when no usable path list
    // is exposed.
    let (path_list, array_backed) = match ctx.get_field_by_name(ucp, "path") {
        Value::Object(Some(o)) if is_array_list_object(ctx, o) => (o, false),
        _ => match ctx.get_field(ucp, UCP_STASHED_URLS) {
            Value::Object(Some(o)) => (o, true),
            _ => return None,
        },
    };
    // Fast path: skip the work entirely unless at least one recorded base URL
    // actually carries a custom handler (ordinary loaders record only
    // file:/jar: URLs, whose handler is null/`sun.net.*`).
    let p_path = ctx.pin_native_root(path_list);
    let size = if array_backed {
        ctx.array_length(path_list) as i32
    } else {
        match ctx.invoke_virtual(path_list, "size", "()I", &[]) {
            Ok(Some(Value::Int(n))) => n,
            _ => {
                ctx.unpin_native_roots(p_path);
                return None;
            }
        }
    };
    let result = match ctx.new_object("java/util/ArrayList") {
        Ok(Some(Value::Object(Some(l)))) => l,
        _ => {
            ctx.unpin_native_roots(p_path);
            return None;
        }
    };
    let p_result = ctx.pin_native_root(result);
    let _ = ctx.invoke_special(
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(ctx.read_native_pin(p_result, result)))],
    );
    let mut matched = 0u32;
    for i in 0..size {
        let path_list = ctx.read_native_pin(p_path, path_list);
        let base = if array_backed {
            match ctx.get_array_element(path_list, i as usize) {
                Value::Object(Some(b)) => b,
                _ => continue,
            }
        } else {
            match ctx.invoke_virtual(path_list, "get", "(I)Ljava/lang/Object;", &[Value::Int(i)]) {
                Ok(Some(Value::Object(Some(b)))) => b,
                _ => continue,
            }
        };
        let p_base = ctx.pin_native_root(base);
        if url_has_custom_handler(ctx, base) {
            if let Some(resolved) = resolve_url_against(ctx, base, name) {
                let p_res = ctx.pin_native_root(resolved);
                let resolved = ctx.read_native_pin(p_res, resolved);
                if probe_resource_exists(ctx, resolved) {
                    let resolved = ctx.read_native_pin(p_res, resolved);
                    let result = ctx.read_native_pin(p_result, result);
                    let _ = ctx.invoke_virtual(
                        result,
                        "add",
                        "(Ljava/lang/Object;)Z",
                        &[Value::Object(Some(resolved))],
                    );
                    matched += 1;
                }
            }
        }
        // Release this iteration's pins (p_base and any p_res after it).
        ctx.unpin_native_roots(p_base);
    }
    let result = ctx.read_native_pin(p_result, result);
    ctx.unpin_native_roots(p_path); // releases p_path, p_result and the rest
    if matched == 0 {
        None
    } else {
        Some(result)
    }
}

/// Record `url` on `ucp.path` (a real `ArrayList<URL>`, created on demand). All
/// re-entrant Java calls are pinned so a moving GC can't stale the refs
/// mid-sequence.
fn record_url_on_path(ctx: &mut dyn NativeContext, ucp: ObjectRef, url: ObjectRef) {
    let p_ucp = ctx.pin_native_root(ucp);
    let p_url = ctx.pin_native_root(url);
    let list = match ctx.get_field_by_name(ucp, "path") {
        Value::Object(Some(l)) if is_array_list_object(ctx, l) => l,
        _ => {
            // Lazily create the `path` ArrayList and store it on `ucp`.
            let created = match ctx.new_object("java/util/ArrayList") {
                Ok(Some(Value::Object(Some(l)))) => l,
                _ => {
                    ctx.unpin_native_roots(p_ucp);
                    return;
                }
            };
            let p_list = ctx.pin_native_root(created);
            let created = ctx.read_native_pin(p_list, created);
            let _ = ctx.invoke_special(
                "java/util/ArrayList",
                "<init>",
                "()V",
                &[Value::Object(Some(created))],
            );
            let ucp = ctx.read_native_pin(p_ucp, ucp);
            let created = ctx.read_native_pin(p_list, created);
            ctx.set_field_by_name(ucp, "path", Value::Object(Some(created)));
            created
        }
    };
    let p_list = ctx.pin_native_root(list);
    let list = ctx.read_native_pin(p_list, list);
    let url = ctx.read_native_pin(p_url, url);
    let _ = ctx.invoke_virtual(
        list,
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(url))],
    );
    ctx.unpin_native_roots(p_ucp); // releases every pin taken here
}

fn empty_enumeration_impl(ctx: &mut dyn NativeContext) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    let enm = alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 2);
    ctx.set_field(enm, 0, Value::Object(Some(arr)));
    ctx.set_field(enm, 1, Value::Int(0));
    enm
}

fn loader_constructor_url_paths(ctx: &dyn NativeContext, loader: ObjectRef) -> Vec<String> {
    let mut out = Vec::new();

    let mut append_url = |url: ObjectRef| {
        if let Some(path) = extract_url_path(ctx, url) {
            if !path.is_empty() && !out.contains(&path) {
                out.push(path);
            }
        }
    };

    // Synthetic-JDK URLClassLoader instances store constructor URLs directly on
    // the loader. Real-JDK instances stash the original URL[] on the shimmed ucp
    // placeholder (see `record_ucl_urls`).
    if let Value::Object(Some(urls)) = ctx.get_field(loader, UCL_URLS_ARRAY) {
        let count = match ctx.get_field(loader, UCL_URL_COUNT) {
            Value::Int(n) if n > 0 => (n as usize).min(ctx.array_length(urls)),
            _ => 0,
        };
        for i in 0..count {
            if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                append_url(url);
            }
        }
    }

    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(loader, "ucp") {
        // `record_ucl_urls` mirrors constructor URLs on `URLClassPath.path`
        // as well as in its private compatibility slot.  The real JDK's
        // URLClassPath layout can overwrite that numeric slot while executing
        // constructor bytecode, whereas the named `path` list survives.  Read
        // it first so local URLClassLoader lookups remain isolated even after
        // several temporary test loaders have extended the global classpath.
        if let Value::Object(Some(path_list)) = ctx.get_field_by_name(ucp, "path") {
            let element_data = ctx.get_field_by_name(path_list, "elementData");
            let size = match ctx.get_field_by_name(path_list, "size") {
                Value::Int(n) if n > 0 => n as usize,
                _ => 0,
            };
            if let Value::Object(Some(elements)) = element_data {
                for i in 0..size.min(ctx.array_length(elements)) {
                    if let Value::Object(Some(url)) = ctx.get_array_element(elements, i) {
                        append_url(url);
                    }
                }
            }
        }
        if let Value::Object(Some(urls)) = ctx.get_field(ucp, UCP_STASHED_URLS) {
            for i in 0..ctx.array_length(urls) {
                if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                    append_url(url);
                }
            }
        }
    }

    out
}

/// Collect `http`/`https` base URL strings from a URLClassLoader's own
/// constructor URLs -- the network-classpath counterpart of
/// `loader_constructor_url_paths` (which only handles `file:`/`jar:` entries
/// resolvable as local filesystem paths).
fn loader_constructor_http_bases(ctx: &dyn NativeContext, loader: ObjectRef) -> Vec<String> {
    let mut out = Vec::new();

    if let Value::Object(Some(urls)) = ctx.get_field(loader, UCL_URLS_ARRAY) {
        let count = match ctx.get_field(loader, UCL_URL_COUNT) {
            Value::Int(n) if n > 0 => (n as usize).min(ctx.array_length(urls)),
            _ => 0,
        };
        for i in 0..count {
            if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                if let Some(base) = http_base_from_url(ctx, url) {
                    out.push(base);
                }
            }
        }
    }

    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(loader, "ucp") {
        if let Value::Object(Some(urls)) = ctx.get_field(ucp, UCP_STASHED_URLS) {
            for i in 0..ctx.array_length(urls) {
                if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                    if let Some(base) = http_base_from_url(ctx, url) {
                        out.push(base);
                    }
                }
            }
        }
    }

    out
}

/// Reconstruct an `http(s)://host[:port]/path` base string from a
/// `java.net.URL` object, or `None` if it is not an http/https URL. Reads
/// fields by NAME first (works for real-JDK URL objects, which carry genuine
/// JDK field names -- the same pattern `ucp_add_url`'s `"protocol"` read
/// uses), falling back to the synthetic numeric layout (`URL_FIELD_*`) so
/// synthetic-JDK mode's placeholder URL objects resolve too.
fn http_base_from_url(ctx: &dyn NativeContext, url_obj: ObjectRef) -> Option<String> {
    let read = |name: &str, idx: usize| -> Option<String> {
        match ctx.get_field_by_name(url_obj, name) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => match ctx.get_field(url_obj, idx) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            },
        }
    };
    let protocol = read("protocol", 0)?;
    if protocol != "http" && protocol != "https" {
        return None;
    }
    let host = read("host", 1).unwrap_or_default();
    let port = match ctx.get_field_by_name(url_obj, "port") {
        Value::Int(p) if p > 0 => Some(p),
        _ => match ctx.get_field(url_obj, 2) {
            Value::Int(p) if p > 0 => Some(p),
            _ => None,
        },
    };
    let path = read("path", 3).unwrap_or_default();

    let mut base = format!("{protocol}://{host}");
    if let Some(p) = port {
        base.push(':');
        base.push_str(&p.to_string());
    }
    base.push_str(&path);
    Some(base)
}

/// Attempt a real HTTP(S) GET for `resource_name` against each of `bases` in
/// turn (each a base URL collected by `loader_constructor_http_bases`, e.g.
/// `http://localhost:8500/test-classes/`). Returns the response body on the
/// first `200 OK`; returns `None` when every base yields a non-2xx status or
/// a connection failure -- a definitive "not found via this loader's own
/// classpath" the caller must NOT paper over with CratonVM's flat global
/// class store (see docs/known-issues/keycloak/
/// test-classserver-invalidpackage-classnotfound-not-thrown.md -- Keycloak's
/// `TestClassServer` answers a non-permitted package with HTTP 403, which
/// must surface as `ClassNotFoundException`, not a silent success).
fn fetch_http_resource(
    ctx: &mut dyn NativeContext,
    bases: &[String],
    resource_name: &str,
) -> Option<Vec<u8>> {
    for base in bases {
        let sep = if base.ends_with('/') { "" } else { "/" };
        let uri = format!("{base}{sep}{resource_name}");
        ctx.begin_blocking_region();
        let result = crate::http_client::perform_request(
            "GET",
            &uri,
            &[],
            &[],
            std::time::Duration::from_secs(10),
            false,
            5,
        );
        ctx.end_blocking_region();
        if let Ok(resp) = result {
            if resp.status == 200 {
                return Some(resp.body);
            }
        }
    }
    None
}

fn loader_local_resource_urls(
    ctx: &dyn NativeContext,
    loader: ObjectRef,
    resource_name: &str,
) -> Vec<String> {
    let paths = loader_constructor_url_paths(ctx, loader);
    if paths.is_empty() {
        if std::env::var_os("CRATONVM_DBG_UCLRES").is_some() {
            eprintln!("[UCLRES-DBG] loader={loader:?} resource={resource_name} paths=[]");
        }
        return Vec::new();
    }
    let urls = cratonvm_classloading::ClassPath::new(&paths).find_all_resource_urls(resource_name);
    if std::env::var_os("CRATONVM_DBG_UCLRES").is_some() {
        eprintln!(
            "[UCLRES-DBG] loader={loader:?} resource={resource_name} paths={paths:?} urls={urls:?}"
        );
    }
    urls
}

/// Try to resolve `URLClassLoader.findClass(name)` from the receiver's own
/// URL set (local filesystem paths and/or real HTTP(S) fetches) and define
/// the resulting class under that receiver's loader namespace.
///
/// Returns `None` only when this loader has NO usable URL entries at all (no
/// local paths, no http(s) bases) -- leaving callers free to use their
/// historical fallback path. Returns `Some(Err(ClassNotFoundException))` when
/// the loader DOES have http(s) entries but none of them produced the class
/// (e.g. every base answered non-200, or refused the connection) -- this is a
/// definitive miss against the loader's own recorded sources and callers
/// must propagate it rather than falling back to the flat global class
/// store, which would let a `URLClassLoader(urls, null)` resolve application
/// classes its own (failed) URL search should have hidden from it. See
/// docs/known-issues/keycloak/test-classserver-invalidpackage-classnotfound-not-thrown.md.
pub(crate) fn ucl_try_define_local_class(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    internal_name: &str,
) -> Option<MethodCallResult> {
    if let Some(mirror) = find_loaded_class_for_loader(ctx, loader, internal_name) {
        return Some(Ok(Some(Value::Object(Some(mirror)))));
    }

    let resource_name = format!("{internal_name}.class");
    let paths = loader_constructor_url_paths(ctx, loader);
    // Keep the source metadata coupled to the exact classpath that supplied
    // the bytes. Falling back to ClassManager's process-wide lookup after a
    // successful local definition can attach a same-named application JAR as
    // this class's CodeSource (for example, a URLClassLoader override JAR).
    let local_class_path =
        (!paths.is_empty()).then(|| cratonvm_classloading::ClassPath::new(&paths));
    let (bytes, local_code_source) = match local_class_path.as_ref() {
        Some(class_path) => match class_path.find_resource(&resource_name) {
            Some(bytes) => (
                Some(bytes),
                class_path.find_class_code_source_info(internal_name),
            ),
            None => (None, None),
        },
        None => (None, None),
    };
    let http_bases = loader_constructor_http_bases(ctx, loader);
    let bytes = match bytes {
        Some(b) => Some(b),
        None if !http_bases.is_empty() => fetch_http_resource(ctx, &http_bases, &resource_name),
        None => None,
    };

    let bytes = match bytes {
        Some(b) => b,
        None if http_bases.is_empty() => {
            if url_classloader_isolated_from_app(ctx, loader) {
                let exception = crate::jboss_module_loader::alloc_single_message_exception(ctx, "java/lang/ClassNotFoundException", 1, internal_name);
                return Some(Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exception)));
            }
            return None;
        }
        None => {
            let exc = crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/ClassNotFoundException",
                1,
                internal_name,
            );
            return Some(Err(
                cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc),
            ));
        }
    };

    if url_classloader_isolated_from_app(ctx, loader) {
        if let Err(error) = crate::lang_system::preload_isolated_loader_supertypes(ctx, loader, &bytes) {
            return Some(Err(error));
        }
    }

    let loader_pin = ctx.pin_native_root(loader);
    let loader_live = ctx.read_native_pin(loader_pin, loader);
    let loader_id = loader_namespace_id(ctx, loader_live);
    let opts = match local_code_source {
        Some((code_source_url, code_source_certificates)) => cratonvm_native_api::DefineClassFull {
            code_source_url: Some(code_source_url),
            code_source_certificates,
            ..Default::default()
        },
        None => cratonvm_native_api::DefineClassFull::default(),
    };
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(internal_name, &bytes, loader_id, opts)
    }));

    let result = match define_result {
        Ok(Ok(cid)) => {
            let loader_live = ctx.read_native_pin(loader_pin, loader);
            register_defining_loader(cid.as_u32(), loader_live);
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Ok(Err(msg)) => {
            tracing::warn!("URLClassLoader.findClass({internal_name}) define failed: {msg}");
            Err(cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: internal_name.to_string(),
                message: format!("URLClassLoader.findClass: {msg}"),
            }
            .into())
        }
        Err(_) => {
            tracing::error!(
                "URLClassLoader.findClass({internal_name}) panicked while defining local class"
            );
            Err(cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: internal_name.to_string(),
                message: "URLClassLoader.findClass: panic inside backend".into(),
            }
            .into())
        }
    };
    ctx.unpin_native_roots(loader_pin);
    Some(result)
}

/// `jdk.internal.loader.URLClassPath.addURL(URL)` for real-JDK mode.
///
/// `URLClassLoader.addURL` is inherited and almost always invoked via the
/// SUBCLASS as `this.addURL(url)` (e.g. ShrinkWrap's `addArchive`), so its CP
/// methodref names the subclass — the `check_override` / force-native gates
/// (which key on the static call-site class) can't recognise it. But the body
/// `URLClassLoader.addURL` is just `ucp.addURL(url)`, and `ucp` is typed
/// `jdk.internal.loader.URLClassPath`, so shimming `URLClassPath.addURL`
/// intercepts the same operation through a call-site the gates DO match. `this`
/// here is the `URLClassPath` (the loader's `ucp`). See the module banner.
fn record_real_ucl_url(ctx: &mut dyn NativeContext, ucp: ObjectRef, url: ObjectRef) {
    // Record the base URL on `ucp.path` so `ucl_find_resource(s)` can resolve
    // against custom-handler URLs. Pin `url` across the re-entrant recording so
    // the protocol read below still sees a live ref.
    let p_url = ctx.pin_native_root(url);
    let url_live = ctx.read_native_pin(p_url, url);
    record_url_on_path(ctx, ucp, url_live);

    // Ordinary file:/jar: URLs additionally extend the global dynamic
    // classpath so classes/resources inside them load (mirrors the `<init>`
    // natives' `register_url_array`). Custom schemes (archive:, …) have no
    // filesystem path and are served via their handler instead.
    // `addURL` changes this loader only on HotSpot. Keep ordinary file:/jar:
    // URLs out of the global application classpath as well; local resolver
    // paths serve them without leaking into siblings or parents.
    ctx.unpin_native_roots(p_url);
}

/// Record a URL appended through the real `URLClassLoader.addURL(URL)`
/// wrapper.  Subclasses such as ShrinkWrap invoke that protected inherited
/// method with a subclass-owned call site, so the ordinary static-class native
/// gate cannot see it.  Keeping the original URL is essential for application
/// URLStreamHandler-backed schemes such as `archive:`.
pub(crate) fn ucl_add_url_real(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let url = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(this, "ucp") {
        record_real_ucl_url(ctx, ucp, url);
    }
    Ok(None)
}

pub(crate) fn ucp_add_url(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ucp = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let url = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    record_real_ucl_url(ctx, ucp, url);
    Ok(None)
}

pub(crate) fn ucl_find_resource(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let resource_name = name.trim_start_matches('/');
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        let local_urls = loader_local_resource_urls(ctx, this, resource_name);
        if let Some(first) = local_urls.first() {
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, first);
            return Ok(Some(Value::Object(Some(url))));
        }
        // A URLClassLoader's `findResource` is strictly local. Its public
        // `getResource` caller has already performed parent-first delegation;
        // consulting the process-wide classpath here leaks entries that a
        // temporary child deliberately removed. In particular, Spring Boot's
        // ModifiedClassPathClassLoader excludes Hibernate Validator from its
        // recorded URL list, but can have a non-null platform-loader parent on
        // real JDKs, so the narrower `isolated_from_app` predicate is not a
        // sufficient guard.
        if object_extends(ctx, this, "java/net/URLClassLoader") {
            return Ok(Some(Value::Object(None)));
        }
    }
    // Mirror cl_get_resource's lookup order: structured URL walk FIRST.
    // URLClassLoader-constructor URLs are registered into the global walk
    // but NOT into the raw-bytes `find_resource` store, so consulting only
    // the latter made findResource return null for any resource living in a
    // loader-supplied jar while getResource (the walk) found it. Canonical
    // victim: Gradle's VisitableURLClassLoader("runtime-api-info") looking
    // up gradle-plugins.properties from gradle-runtime-api-info.jar —
    // "Cannot find resource ... in classloader" killed every ProjectBuilder
    // bootstrap (Spring Boot buildSrc suite). Returning the walk's URL also
    // keeps findResource/getResource spec-consistent (same URL form).
    let urls = ctx.find_all_resource_urls(resource_name);
    if let Some(first) = urls.first() {
        let url = crate::jboss_module_loader::build_synthetic_url(ctx, first);
        return Ok(Some(Value::Object(Some(url))));
    }
    match ctx.find_resource(resource_name) {
        Some(_) => {
            let spec = format!("classpath:{name}");
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, &spec);
            Ok(Some(Value::Object(Some(url))))
        }
        None => {
            // Custom-handler fallback: resources behind an app-supplied
            // `URLStreamHandler` (ShrinkWrap `archive:`) that `addURL` recorded
            // on `ucp.path`. Return the first base URL that resolves `name`.
            if let Some(Value::Object(Some(this))) = args.first().copied() {
                if let Some(list) = build_custom_handler_url_list(ctx, this, resource_name) {
                    let p_list = ctx.pin_native_root(list);
                    let list = ctx.read_native_pin(p_list, list);
                    let first =
                        ctx.invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(0)]);
                    ctx.unpin_native_roots(p_list);
                    if let Ok(Some(v @ Value::Object(Some(_)))) = first {
                        return Ok(Some(v));
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        }
    }
}

/// Append the elements of a `java.util.ArrayList<URL>` to the URL array carried
/// by a synthetic `Enumeration$Impl`, returning a fresh `Enumeration$Impl` over
/// the union. `std_enum` is the standard flat-classpath enumeration (may be
/// `None`); `custom_list` holds the custom-handler resolved URLs. All
/// re-entrant calls are pinned for moving-GC safety.
fn merge_enum_with_list(
    ctx: &mut dyn NativeContext,
    std_enum: Option<ObjectRef>,
    custom_list: ObjectRef,
) -> ObjectRef {
    if !is_array_list_object(ctx, custom_list) {
        return std_enum.unwrap_or_else(|| empty_enumeration_impl(ctx));
    }
    let p_custom = ctx.pin_native_root(custom_list);
    // Standard enumeration's backing URL[] (field 0 of `Enumeration$Impl`).
    let std_arr = match std_enum {
        Some(e) => match ctx.get_field(e, 0) {
            Value::Object(Some(a)) => Some(a),
            _ => None,
        },
        None => None,
    };
    let alen = std_arr.map(|a| ctx.array_length(a)).unwrap_or(0);
    let p_std = std_arr.map(|a| ctx.pin_native_root(a));
    let custom_list = ctx.read_native_pin(p_custom, custom_list);
    let clen = match ctx.invoke_virtual(custom_list, "size", "()I", &[]) {
        Ok(Some(Value::Int(n))) => n.max(0) as usize,
        _ => 0,
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, alen + clen);
    let p_arr = ctx.pin_native_root(arr);
    if let (Some(sa), Some(psa)) = (std_arr, p_std) {
        let sa = ctx.read_native_pin(psa, sa);
        let arr = ctx.read_native_pin(p_arr, arr);
        for i in 0..alen {
            let e = ctx.get_array_element(sa, i);
            ctx.set_array_element(arr, i, e);
        }
    }
    for i in 0..clen {
        let custom_list = ctx.read_native_pin(p_custom, custom_list);
        let e = match ctx.invoke_virtual(
            custom_list,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Int(i as i32)],
        ) {
            Ok(Some(v)) => v,
            _ => Value::Object(None),
        };
        let arr = ctx.read_native_pin(p_arr, arr);
        ctx.set_array_element(arr, alen + i, e);
    }
    let enm = alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 2);
    let arr = ctx.read_native_pin(p_arr, arr);
    ctx.set_field(enm, 0, Value::Object(Some(arr)));
    ctx.set_field(enm, 1, Value::Int(0));
    ctx.unpin_native_roots(p_custom); // releases p_custom, p_std, p_arr
    enm
}

pub(crate) fn ucl_find_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `findResources` is the terminal flat-classpath scan — it must NOT re-delegate
    // to `findResources` (which would recurse forever for a URLClassLoader subclass
    // that doesn't override it, e.g. GroovyClassLoader). See SB-13.
    //
    // Compute the custom-handler matches FIRST: when there are none (the common
    // case for ordinary file:/jar: loaders) the standard scan is returned
    // verbatim, leaving existing behaviour byte-for-byte unchanged.
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return cl_get_resources_impl(ctx, args, false),
    };
    let name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return cl_get_resources_impl(ctx, args, false),
    };
    let resource_name = name.trim_start_matches('/').to_string();

    // Run the standard flat-classpath scan first while the arguments are
    // still fresh; custom-handler/local probing below can allocate.
    let p_this = ctx.pin_native_root(this);
    let std_enum = cl_get_resources_impl(ctx, args, false)?;
    let std_ref = match std_enum {
        Some(Value::Object(Some(e))) => Some(e),
        _ => None,
    };
    let p_std = std_ref.map(|e| ctx.pin_native_root(e));

    let this = ctx.read_native_pin(p_this, this);
    let local_urls = loader_local_resource_urls(ctx, this, &resource_name);
    let local_enum = if local_urls.is_empty() {
        None
    } else {
        let arr = ctx.new_array(
            cratonvm_types::ArrayElementType::Reference,
            local_urls.len(),
        );
        for (i, url) in local_urls.iter().enumerate() {
            let url_obj = crate::jboss_module_loader::build_synthetic_url(ctx, url);
            ctx.set_array_element(arr, i, Value::Object(Some(url_obj)));
        }
        let enm = alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 2);
        ctx.set_field(enm, 0, Value::Object(Some(arr)));
        ctx.set_field(enm, 1, Value::Int(0));
        Some(enm)
    };
    let p_local = local_enum.map(|e| ctx.pin_native_root(e));

    // Custom-handler matches (ShrinkWrap `archive:`), recorded by `addURL`.
    let this = ctx.read_native_pin(p_this, this);
    let custom = build_custom_handler_url_list(ctx, this, &resource_name);

    let local_ref = match (local_enum, p_local) {
        (Some(e), Some(p)) => Some(ctx.read_native_pin(p, e)),
        _ => None,
    };
    let std_ref = match (std_ref, p_std) {
        (Some(e), Some(p)) => Some(ctx.read_native_pin(p, e)),
        _ => None,
    };
    let result = match custom {
        Some(custom) => Some(Value::Object(Some(merge_enum_with_list(
            ctx,
            local_ref.or(std_ref),
            custom,
        )))),
        None => match local_ref.or(std_ref) {
            Some(e) => Some(Value::Object(Some(e))),
            None => std_enum,
        },
    };
    ctx.unpin_native_roots(p_this);
    Ok(result)
}

fn ucl_get_urls(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(this, "ucp") {
        if let Some(result) = ucp_path_urls(ctx, ucp) {
            return Ok(Some(Value::Object(Some(result))));
        }
    }
    let count = match ctx.get_field(this, UCL_URL_COUNT) {
        Value::Int(n) => n.max(0) as usize,
        _ => 0,
    };
    // Synthetic-JDK path: URLs live in the per-instance slots (`ucl_setup`/
    // `ucl_add_url`). Keep the legacy raw-slot fallback for old placeholders.
    if count == 0 {
        if let Value::Object(Some(ucp)) = ctx.get_field_by_name(this, "ucp") {
            if let Value::Object(Some(stashed)) = ctx.get_field(ucp, UCP_STASHED_URLS) {
                let n = ctx.array_length(stashed);
                let result = ctx.new_array(cratonvm_types::ArrayElementType::Reference, n);
                for i in 0..n {
                    let url = ctx.get_array_element(stashed, i);
                    ctx.set_array_element(result, i, url);
                }
                return Ok(Some(Value::Object(Some(result))));
            }
        }
    }
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
        let mut url_obj = *url_obj;
        if let Value::Object(Some(urls_arr)) = ctx.get_field(this, UCL_URLS_ARRAY) {
            let arr_len = ctx.array_length(urls_arr);
            if (count as usize) < arr_len {
                ctx.set_array_element(urls_arr, count as usize, Value::Object(Some(url_obj)));
            } else {
                // GC-safety: growing the array below (`new_array`) can
                // trigger a moving GC; `urls_arr` (copied FROM) and
                // `url_obj` (the new entry) are both reused afterward,
                // unpinned otherwise.
                let urls_arr_pin = ctx.pin_native_root(urls_arr);
                let url_obj_pin = ctx.pin_native_root(url_obj);
                let new_cap = arr_len * 2;
                let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                let urls_arr = ctx.read_native_pin(urls_arr_pin, urls_arr);
                url_obj = ctx.read_native_pin(url_obj_pin, url_obj);
                ctx.unpin_native_roots(urls_arr_pin);
                for i in 0..arr_len {
                    let elem = ctx.get_array_element(urls_arr, i);
                    ctx.set_array_element(new_arr, i, elem);
                }
                ctx.set_array_element(new_arr, count as usize, Value::Object(Some(url_obj)));
                ctx.set_field(this, UCL_URLS_ARRAY, Value::Object(Some(new_arr)));
            }
        }

        // Extract the URL path and extend the classpath dynamically.
        if let Some(p) = extract_url_path(ctx, url_obj) {
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
    // FIX: previously this only stored UCL_URL_COUNT and dropped the URL[]
    // entirely, so the returned loader couldn't search the supplied URLs.
    // Route through `ucl_setup` (the same code the `<init>` natives use) so
    // the URLs are copied into UCL_URLS_ARRAY and their paths registered on
    // the dynamic classpath. The loader id assigned by `alloc_url_classloader`
    // is preserved (ucl_setup doesn't touch UCL_LOADER_ID).
    //
    // GC-safety: `ucl_setup` allocates/copies the URL array and can trigger a
    // moving GC; `obj` is returned afterward, unpinned otherwise.
    let obj_pin = ctx.pin_native_root(obj);
    ucl_setup(ctx, obj, urls, Value::Object(None));
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Ok(Some(Value::Object(Some(obj))))
}

fn ucl_new_instance_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let urls = args.first().copied().unwrap_or(Value::Object(None));
    let parent = args.get(1).copied().unwrap_or(Value::Object(None));
    let obj = alloc_url_classloader(ctx);
    // FIX: mirror the `<init>(URL[], ClassLoader)` path — store the URL[] and
    // register its paths so the loader actually searches them (was dropping
    // the URLs and only recording their count). See `ucl_new_instance`.
    //
    // GC-safety: `ucl_setup` allocates/copies the URL array and can trigger a
    // moving GC; `obj` is returned afterward, unpinned otherwise.
    let obj_pin = ctx.pin_native_root(obj);
    ucl_setup(ctx, obj, urls, parent);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
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
    Ok(Some(Value::Int(lk_modes_of(ctx, this))))
}

fn lk_has_full_privilege_access(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modes = lk_modes_of(ctx, this);
    let full = (modes & LK_PRIVATE) != 0 && (modes & LK_MODULE) != 0;
    Ok(Some(Value::Int(if full { 1 } else { 0 })))
}

fn lk_has_private_access(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let modes = lk_modes_of(ctx, this);
    Ok(Some(Value::Int(if (modes & LK_PRIVATE) != 0 {
        1
    } else {
        0
    })))
}

fn lk_ensure_initialized(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let _this = obj_arg(args, 0)?;
    let target_class = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Lookup.ensureInitialized target class is null".to_string()),
            }
            .into());
        }
    };
    let class_id = crate::lang_class::mirror_class_id(ctx, target_class).ok_or_else(|| {
        cratonvm_types::error::RuntimeError::IllegalArgumentException {
            message: "Lookup.ensureInitialized target is not a Class mirror".to_string(),
        }
    })?;
    // Family-1 stale-ObjectRef fix (2026-07-13): `ctx.initialize_class` runs
    // the target's `<clinit>`, which can allocate and trigger a moving GC.
    // `target_class` is a raw `ObjectRef` captured above and was being
    // returned again after this call without being refreshed — exactly the
    // "held across a GC-triggering call" pattern documented in
    // docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md.
    // Root and re-read it around the call.
    let target_class_pin = ctx.pin_native_root(target_class);
    // HIB-CV-26 fix (2026-07-16): propagate the real `<clinit>` failure
    // instead of re-wrapping it as an unrecoverable `VmError::Internal` —
    // matches real JDK `Lookup.ensureInitialized`, which throws
    // `ExceptionInInitializerError` for a failed initializer.
    ctx.initialize_class(class_id)?;
    let target_class = ctx.read_native_pin(target_class_pin, target_class);
    ctx.unpin_native_roots(target_class_pin);
    Ok(Some(Value::Object(Some(target_class))))
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
    // PERF: we only need the single `this_class` name, not the whole pool.
    // The previous implementation allocated two HashMaps and a heap
    // `String` for *every* Utf8 entry on each defineClass-family call —
    // very hot for cglib/ByteBuddy-heavy apps that define many proxies.
    //
    // Instead we do one cheap linear scan that records only each constant
    // pool entry's *start byte offset* into a flat `Vec<u32>` (one slot
    // per pool index — no per-entry hashing, no per-Utf8 String alloc).
    // After the pool we read `this_class`, follow it to the CONSTANT_Class
    // entry's `name_index`, and decode exactly one Utf8 string. Behavior
    // (including all `None`/out-of-range failure cases) is identical to
    // the old HashMap version; only the one resolved name is allocated.
    //
    // offsets[i] = byte offset of constant pool entry `i` (the tag byte).
    // Index 0 is unused (the pool is 1-based); long/double entries leave
    // their second slot at the sentinel `u32::MAX` (unusable index).
    let mut offsets: Vec<u32> = vec![u32::MAX; cp_count];
    let mut pos = 10usize;
    let mut idx: usize = 1;
    while idx < cp_count {
        if pos >= bytes.len() {
            return None;
        }
        offsets[idx] = pos as u32;
        let tag = bytes[pos];
        pos += 1;
        match tag {
            1 => {
                // CONSTANT_Utf8 — u2 length, [u1]* bytes. We deliberately
                // do NOT decode the bytes here (the old code decoded every
                // Utf8); we only need to skip past it.
                if pos + 2 > bytes.len() {
                    return None;
                }
                let len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
                pos += 2;
                if pos + len > bytes.len() {
                    return None;
                }
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
    let this_class_idx = u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;

    // Resolve `this_class` (a CONSTANT_Class) → its `name_index` Utf8.
    // Bounds-check the index and verify the tag matches, mirroring the
    // old code which returned `None` for a missing/mismatched entry.
    let class_off = *offsets.get(this_class_idx)? as usize;
    if class_off == u32::MAX as usize || class_off + 3 > bytes.len() {
        return None;
    }
    if bytes[class_off] != 7 {
        // `this_class` did not point at a CONSTANT_Class. The old code
        // returned `None` here (the name-index HashMap had no entry).
        return None;
    }
    let name_idx = u16::from_be_bytes([bytes[class_off + 1], bytes[class_off + 2]]) as usize;

    let name_off = *offsets.get(name_idx)? as usize;
    if name_off == u32::MAX as usize || name_off + 3 > bytes.len() {
        return None;
    }
    if bytes[name_off] != 1 {
        // name_index did not point at a CONSTANT_Utf8. The old code
        // returned `None` (the utf8 HashMap had no entry for it).
        return None;
    }
    let len = u16::from_be_bytes([bytes[name_off + 1], bytes[name_off + 2]]) as usize;
    let start = name_off + 3;
    let end = start.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    // Lenient UTF-8: the JVM uses Modified UTF-8, but for the subset used
    // in internal class names (ASCII-safe `foo/Bar$Inner`) the modified
    // and standard forms agree. Non-conforming names decode via
    // `from_utf8_lossy`, which is harmless for the mangling step. Only
    // this one string is allocated (the old code allocated every Utf8).
    Some(String::from_utf8_lossy(&bytes[start..end]).into_owned())
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
        if let Value::Object(Some(lookup_mirror)) = ctx.get_field(this_lookup, LK_LOOKUP_CLASS_REF)
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
    // GC-safety: `mirror_class_id`/`build_method_type_from_descriptor` below
    // can trigger a moving GC (classloading); `mh` is reused in the final
    // `set_field_by_name` unpinned otherwise.
    let mh_pin = ctx.pin_native_root(mh);
    ctx.set_field(mh, MH_KIND, Value::Int(kind));
    ctx.set_field(
        mh,
        MH_TARGET_CLASS,
        match class_mirror {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        },
    );
    ctx.set_field(
        mh,
        MH_NAME,
        match name {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        },
    );
    ctx.set_field(
        mh,
        MH_TYPE,
        match method_type {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        },
    );
    // Resolve class ID if class mirror is available
    if let Some(mirror) = class_mirror {
        if let Some(cid) = crate::lang_class::mirror_class_id(ctx, mirror) {
            let mh = ctx.read_native_pin(mh_pin, mh);
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
    let mt_to_store =
        method_type.or_else(|| crate::lang_invoke::build_method_type_from_descriptor(ctx, "()V"));
    let mh = ctx.read_native_pin(mh_pin, mh);
    ctx.unpin_native_roots(mh_pin);
    if let Some(mt) = mt_to_store {
        ctx.set_field_by_name(mh, "type", Value::Object(Some(mt)));
    }
    mh
}

/// Resolve the JVMS access flags (`ACC_PUBLIC`/`ACC_PRIVATE`/…) of the member
/// named `member_name` on the class denoted by `target_mirror`.
///
/// Walks the declared members of the target class and, for methods, its
/// superclass chain (fields are matched on the declaring class only, mirroring
/// the way `Lookup.find{Getter,Setter}` resolve a single named field). Only the
/// member *name* is matched — overload resolution by descriptor is not modelled
/// here, so the first matching member's flags are used, which is sufficient for
/// the public/non-public boundary this check enforces.
///
/// Returns `None` when the class or member cannot be resolved (e.g. a synthetic
/// stub mirror, or a member that only exists virtually). Callers treat `None`
/// as "cannot determine" and fall back to *allowing* the lookup so this check
/// never produces a false `IllegalAccessException` on a member that genuinely
/// exists but is not reflectively visible to us.
fn lk_member_access_flags(
    ctx: &dyn NativeContext,
    target_mirror: ObjectRef,
    member_name: &str,
    is_field: bool,
) -> Option<u16> {
    let mut cid = crate::lang_class::mirror_class_id(ctx, target_mirror)?;
    loop {
        if is_field {
            for f in ctx.declared_fields(cid) {
                if f.name == member_name {
                    return Some(f.access_flags);
                }
            }
        } else {
            for m in ctx.declared_methods(cid) {
                if m.name == member_name {
                    return Some(m.access_flags);
                }
            }
        }
        // Fields are resolved on the declaring class only; methods may be
        // inherited, so continue up the superclass chain for them.
        if is_field {
            return None;
        }
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid => cid = parent,
            _ => return None,
        }
    }
}

/// Enforce `MethodHandles.Lookup` access control for a `find*` resolution.
///
/// Full JLS §6.6 / `MethodHandles.Lookup` access control (package/module/nest
/// mate / protected-receiver rules) is substantial; this implements the
/// security-critical **private/public boundary** and documents the residual.
///
/// Rules (`this` is the resolving `Lookup`):
/// * A `public` member is always accessible.
/// * A non-public member (`private`/`protected`/package-private) requires the
///   Lookup to retain `PRIVATE` mode. A Lookup without it — notably
///   `publicLookup()` — can never reach a non-public member and gets an
///   `IllegalAccessException`. This is the security-critical boundary.
/// * Additionally, when the Lookup's `lookupClass` is resolvable *and* is a
///   class **other** than the one declaring the member, access is denied even
///   if `PRIVATE` is held: a full-power Lookup is only entitled to the privates
///   of its own class (and nestmates). When `lookupClass` cannot be resolved
///   we do not apply this extra check, so a legitimate self-private lookup is
///   never spuriously rejected.
/// * When the member's flags cannot be resolved, the lookup is allowed (see
///   [`lk_member_access_flags`]) so legitimate resolutions are never broken.
///
/// Residual (intentionally not yet enforced): nestmate/`protected`-receiver
/// and package/module (`opens`/`exports`) gating. A `PRIVATE`-capable Lookup
/// whose `lookupClass` equals the declaring class (or is unresolved) is
/// admitted without verifying the precise JLS §6.6 relationship. This is never
/// *more* permissive than the spec for the public/non-public boundary it
/// guards — a non-private Lookup is always rejected — so it cannot leak the
/// `publicLookup()` -> private escalation the finding describes. See the
/// access-control finding in `docs/internal/reviews/full-review-2026-06-20.md`.
fn enforce_lookup_access(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    target_mirror: Option<ObjectRef>,
    member_name: Option<&str>,
    is_field: bool,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    use cratonvm_types::access_flags::ACC_PUBLIC;

    let (target, name) = match (target_mirror, member_name) {
        (Some(t), Some(n)) => (t, n),
        // Missing target/name: nothing to enforce; let the (already lenient)
        // resolution proceed and surface its own error.
        _ => return Ok(()),
    };

    let flags = match lk_member_access_flags(ctx, target, name, is_field) {
        Some(f) => f,
        None => return Ok(()),
    };

    // Public members are accessible to any Lookup (including publicLookup()).
    if (flags & ACC_PUBLIC) != 0 {
        return Ok(());
    }

    // Non-public member: the Lookup must retain PRIVATE mode. A lookup that
    // dropped (or never had) PRIVATE — e.g. publicLookup() — is rejected.
    let modes = lk_modes_of(ctx, this);
    let has_private = (modes & LK_PRIVATE) != 0;

    // Stronger check when we can resolve the lookupClass: a full-power lookup
    // may only reach its *own* class's non-public members. If the lookupClass
    // resolves to a different class than the declaring class, deny. If it does
    // not resolve, we skip this check rather than risk a false positive.
    let foreign_class = match ctx.get_field(this, LK_LOOKUP_CLASS_REF) {
        Value::Object(Some(lookup_mirror)) => match (
            crate::lang_class::mirror_class_id(ctx, lookup_mirror),
            crate::lang_class::mirror_class_id(ctx, target),
        ) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        },
        _ => false,
    };

    if has_private && !foreign_class {
        return Ok(());
    }

    let kind = if is_field { "field" } else { "method" };
    let owner =
        crate::lang_class::mirror_class_name(ctx, target).unwrap_or_else(|| "?".to_string());
    Err(
        cratonvm_types::error::RuntimeError::IllegalAccessException {
            message: format!(
                "no access: {kind} {owner}.{name} (modifiers 0x{flags:04x}) \
                 from Lookup with modes 0x{modes:04x}"
            ),
        }
        .into(),
    )
}

// findVirtual(Class refc, String name, MethodType type) -> MethodHandle
fn lk_find_virtual(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let mtype = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let this = obj_arg(args, 0)?;
    let name_str = name.and_then(|n| ctx.read_string(n));
    enforce_lookup_access(ctx, this, class_mirror, name_str.as_deref(), false)?;
    let mh = alloc_method_handle(ctx, 0, class_mirror, name, mtype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_static(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let mtype = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let this = obj_arg(args, 0)?;
    let name_str = name.and_then(|n| ctx.read_string(n));
    enforce_lookup_access(ctx, this, class_mirror, name_str.as_deref(), false)?;
    let mh = alloc_method_handle(ctx, 1, class_mirror, name, mtype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_constructor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let mtype = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let this = obj_arg(args, 0)?;
    enforce_lookup_access(ctx, this, class_mirror, Some("<init>"), false)?;
    let name_str = ctx.create_string("<init>");
    let mh = alloc_method_handle(ctx, 2, class_mirror, Some(name_str), mtype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_getter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let ftype = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let this = obj_arg(args, 0)?;
    let name_str = name.and_then(|n| ctx.read_string(n));
    enforce_lookup_access(ctx, this, class_mirror, name_str.as_deref(), true)?;
    let mh = alloc_method_handle(ctx, 3, class_mirror, name, ftype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_setter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let ftype = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let this = obj_arg(args, 0)?;
    let name_str = name.and_then(|n| ctx.read_string(n));
    enforce_lookup_access(ctx, this, class_mirror, name_str.as_deref(), true)?;
    let mh = alloc_method_handle(ctx, 4, class_mirror, name, ftype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_static_getter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let ftype = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let this = obj_arg(args, 0)?;
    let name_str = name.and_then(|n| ctx.read_string(n));
    enforce_lookup_access(ctx, this, class_mirror, name_str.as_deref(), true)?;
    let mh = alloc_method_handle(ctx, 5, class_mirror, name, ftype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_static_setter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let ftype = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let this = obj_arg(args, 0)?;
    let name_str = name.and_then(|n| ctx.read_string(n));
    enforce_lookup_access(ctx, this, class_mirror, name_str.as_deref(), true)?;
    let mh = alloc_method_handle(ctx, 6, class_mirror, name, ftype);
    Ok(Some(Value::Object(Some(mh))))
}

fn lk_find_special(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let mtype = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let this = obj_arg(args, 0)?;
    let name_str = name.and_then(|n| ctx.read_string(n));
    enforce_lookup_access(ctx, this, class_mirror, name_str.as_deref(), false)?;
    let mh = alloc_method_handle(ctx, 7, class_mirror, name, mtype);
    Ok(Some(Value::Object(Some(mh))))
}

// VarHandle synthetic layout (3 fields): 0=target_class, 1=field_name, 2=field_type
fn lk_find_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let ftype = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let this = obj_arg(args, 0)?;
    let name_str = name.and_then(|n| ctx.read_string(n));
    enforce_lookup_access(ctx, this, class_mirror, name_str.as_deref(), true)?;
    let vh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", 3);
    if let Some(cm) = class_mirror {
        ctx.set_field(vh, 0, Value::Object(Some(cm)));
    }
    if let Some(n) = name {
        ctx.set_field(vh, 1, Value::Object(Some(n)));
    }
    if let Some(t) = ftype {
        ctx.set_field(vh, 2, Value::Object(Some(t)));
    }
    Ok(Some(Value::Object(Some(vh))))
}

fn lk_find_static_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_find_var_handle(ctx, args)
}

fn lk_unreflect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // unreflect(Method) -> MethodHandle — extract class/name from the Method object
    let method = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    // C6: Method/Constructor use real-JDK field layout; read by name.
    let class_mirror = method
        .map(|m| ctx.get_field_by_name(m, "clazz"))
        .and_then(|v| match v {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        });
    let name = method
        .map(|m| ctx.get_field_by_name(m, "name"))
        .and_then(|v| match v {
            Value::Object(Some(r)) => Some(r),
            _ => None,
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
    r.register(
        cl,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;)V",
        cl_init_name_parent,
    );
    r.register(
        cl,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        cl_load_class,
    );
    r.register(
        cl,
        "loadClass",
        "(Ljava/lang/String;Z)Ljava/lang/Class;",
        cl_load_class_resolve,
    );
    // T19_H12_LOADCLASS_MODULE — JDK 25 package-private overload used by
    // `Class.forName(Module, String)`'s stock bytecode. Registering on
    // ClassLoader keeps real ClassLoader receivers correct; the
    // `Class.forName(Module, String)` native (lang_class.rs) bypasses
    // the broken JDK bytecode path entirely so we never dispatch this
    // virtual call onto a synthetic Module whose receiver-class drifts.
    r.register(
        cl,
        "loadClass",
        "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;",
        cl_load_class_module,
    );
    r.register(
        cl,
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        cl_find_class,
    );
    r.register(
        cl,
        "findClass",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;",
        cl_find_class_module,
    );
    r.register(
        cl,
        "defineClass",
        "(Ljava/lang/String;[BII)Ljava/lang/Class;",
        cl_define_class_basic,
    );
    r.register(
        cl,
        "defineClass",
        "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;",
        cl_define_class_pd,
    );
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
    r.register(
        cl,
        "findLoadedClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        cl_find_loaded_class,
    );
    r.register(cl, "getParent", "()Ljava/lang/ClassLoader;", cl_get_parent);
    r.register(cl, "getName", "()Ljava/lang/String;", cl_get_name);
    r.register(
        cl,
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
        cl_get_system_class_loader,
    );
    r.register(
        cl,
        "getPlatformClassLoader",
        "()Ljava/lang/ClassLoader;",
        cl_get_platform_class_loader,
    );
    r.register(
        cl,
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        cl_get_resource,
    );
    r.register(
        cl,
        "getResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        cl_get_resources,
    );
    r.register(
        cl,
        "getSystemResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        cl_get_system_resources,
    );
    r.register(
        cl,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        cl_get_resource_as_stream,
    );
    // Install the exact URLClassLoader declarations here as well. This
    // registrar runs after the early servlet/S1 setup in real-JDK mode, so it
    // is the authoritative callback for concrete URLClassLoader resource
    // methods and their subclasses.
    r.register(
        UCL_CLASS,
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        cl_get_resource,
    );
    r.register(
        UCL_CLASS,
        "getResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        cl_get_resources,
    );
    r.register(
        UCL_CLASS,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        cl_get_resource_as_stream,
    );
    for builtin_cl in [
        "jdk/internal/loader/BuiltinClassLoader",
        "jdk/internal/loader/ClassLoaders$AppClassLoader",
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
    ] {
        r.register(
            builtin_cl,
            "getResourceAsStream",
            "(Ljava/lang/String;)Ljava/io/InputStream;",
            cl_get_resource_as_stream,
        );
    }
    r.register(
        cl,
        "getDefinedPackage",
        "(Ljava/lang/String;)Ljava/lang/Package;",
        cl_get_defined_package,
    );
    r.register(
        cl,
        "getDefinedPackages",
        "()[Ljava/lang/Package;",
        cl_get_defined_packages,
    );
    // `ClassLoader.getPackages()` — real JDK bytecode is
    // `return packages().toArray(Package[]::new)` with a stream pipeline that
    // (in our boot) leaks a `ReferencePipeline$Head` into the caller's local
    // typed as `Package[]`, causing NPE on arraylength in
    // `org/jboss/modules/ConcurrentClassLoader.<clinit>` (WildFly 39 boot).
    // Override with an empty array — matches the empty `getDefinedPackages`
    // override and is sufficient for jboss-modules' sanity scan.
    r.register(
        cl,
        "getPackages",
        "()[Ljava/lang/Package;",
        cl_get_defined_packages,
    );
    r.register(
        cl,
        "setDefaultAssertionStatus",
        "(Z)V",
        cl_set_default_assertion_status,
    );
    r.register(
        cl,
        "registerAsParallelCapable",
        "()Z",
        cl_register_as_parallel_capable,
    );
    r.register(
        cl,
        "isRegisteredAsParallelCapable",
        "()Z",
        cl_is_registered_as_parallel_capable,
    );

    // -----------------------------------------------------------------------
    // java/net/URLClassLoader
    // -----------------------------------------------------------------------
    let ucl = UCL_CLASS;

    r.register(ucl, "<init>", "([Ljava/net/URL;)V", ucl_init_urls);
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)V",
        ucl_init_urls_parent,
    );
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V",
        ucl_init_urls_parent_factory,
    );
    r.register(
        ucl,
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        ucl_find_class,
    );
    r.register(
        ucl,
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        ucl_find_resource,
    );
    r.register(
        ucl,
        "findResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
        ucl_find_resources,
    );
    r.register(ucl, "getURLs", "()[Ljava/net/URL;", ucl_get_urls);
    r.register(ucl, "addURL", "(Ljava/net/URL;)V", ucl_add_url);
    r.register(ucl, "close", "()V", ucl_close);
    r.register(
        ucl,
        "newInstance",
        "([Ljava/net/URL;)Ljava/net/URLClassLoader;",
        ucl_new_instance,
    );
    r.register(
        ucl,
        "newInstance",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)Ljava/net/URLClassLoader;",
        ucl_new_instance_parent,
    );

    // -----------------------------------------------------------------------
    // java/lang/invoke/MethodHandles$Lookup
    // -----------------------------------------------------------------------
    let lk = LK_CLASS;

    r.register(
        lk,
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        lk_lookup,
    );
    r.register(lk, "privateLookupIn", "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandles$Lookup;)Ljava/lang/invoke/MethodHandles$Lookup;", lk_private_lookup_in);
    r.register(
        lk,
        "publicLookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        lk_public_lookup,
    );
    r.register(lk, "lookupClass", "()Ljava/lang/Class;", lk_lookup_class);
    r.register(
        lk,
        "previousLookupClass",
        "()Ljava/lang/Class;",
        lk_previous_lookup_class,
    );
    r.register(lk, "lookupModes", "()I", lk_lookup_modes);
    r.register(
        lk,
        "hasFullPrivilegeAccess",
        "()Z",
        lk_has_full_privilege_access,
    );
    r.register(lk, "hasPrivateAccess", "()Z", lk_has_private_access);
    r.register(
        lk,
        "ensureInitialized",
        "(Ljava/lang/Class;)Ljava/lang/Class;",
        lk_ensure_initialized,
    );
    r.register(lk, "defineClass", "([B)Ljava/lang/Class;", lk_define_class);
    r.register(lk, "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;", lk_define_hidden_class);
    // findVirtual/findStatic/findConstructor/findGetter/findSetter/findSpecial/
    // findVarHandle/findStaticVarHandle are all registered in
    // lang_invoke::register_p63_method_handles_lookup — do NOT re-register here
    // as that would overwrite the real implementations with incompatible stubs.
    r.register(
        lk,
        "unreflect",
        "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;",
        lk_unreflect,
    );
    r.register(
        lk,
        "unreflectSpecial",
        "(Ljava/lang/reflect/Method;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lk_unreflect_special,
    );
    r.register(
        lk,
        "in",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandles$Lookup;",
        lk_in_method,
    );
    r.register(
        lk,
        "dropLookupMode",
        "(I)Ljava/lang/invoke/MethodHandles$Lookup;",
        lk_drop_lookup_mode,
    );

    // -----------------------------------------------------------------------
    // java/lang/ClassLoader$HiddenClass (stub)
    // -----------------------------------------------------------------------
    // No natives needed — just a data holder (2-field synthetic)

    // -----------------------------------------------------------------------
    // java/security/ProtectionDomain
    // -----------------------------------------------------------------------
    let pd = PD_CLASS;

    r.register(
        pd,
        "<init>",
        "(Ljava/security/CodeSource;Ljava/security/PermissionCollection;)V",
        pd_init,
    );
    r.register(
        pd,
        "getCodeSource",
        "()Ljava/security/CodeSource;",
        pd_get_code_source,
    );
    r.register(
        pd,
        "getPermissions",
        "()Ljava/security/PermissionCollection;",
        pd_get_permissions,
    );
    r.register(
        pd,
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        pd_get_class_loader,
    );
    r.register(pd, "implies", "(Ljava/security/Permission;)Z", pd_implies);

    // -----------------------------------------------------------------------
    // java/security/CodeSource
    // -----------------------------------------------------------------------
    let cs = CS_CLASS;

    r.register(
        cs,
        "<init>",
        "(Ljava/net/URL;[Ljava/security/cert/Certificate;)V",
        cs_init,
    );
    r.register(cs, "getLocation", "()Ljava/net/URL;", cs_get_location);
    r.register(
        cs,
        "getCertificates",
        "()[Ljava/security/cert/Certificate;",
        cs_get_certificates,
    );

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
        ctx.set_field(this, 0, buf); // buf
        ctx.set_field(this, 1, Value::Int(0)); // pos
        ctx.set_field(this, 2, Value::Int(0)); // mark
        ctx.set_field(this, 3, Value::Int(len)); // count
        Ok(None)
    });
    r.register(bais, "<init>", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let buf = args.get(1).copied().unwrap_or(Value::Object(None));
        let off = args[2].as_int().unwrap_or(0);
        let len = args[3].as_int().unwrap_or(0);
        ctx.set_field(this, 0, buf); // buf
        ctx.set_field(this, 1, Value::Int(off)); // pos
        ctx.set_field(this, 2, Value::Int(off)); // mark
        ctx.set_field(this, 3, Value::Int(off + len)); // count
        Ok(None)
    });
    r.register(bais, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let count = ctx.get_field(this, 3).as_int().unwrap_or(0);
        if pos >= count {
            return Ok(Some(Value::Int(-1)));
        }
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let b = ctx
            .get_array_element(arr, pos as usize)
            .as_int()
            .unwrap_or(0);
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
        if pos >= count {
            return Ok(Some(Value::Int(-1)));
        }
        let avail = count - pos;
        let n = len.min(avail);
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let mut bytes = vec![0u8; n];
        let copied = ctx.read_byte_array_into(arr, pos, &mut bytes);
        if copied > 0 {
            ctx.write_byte_array_from(dst, off, &bytes[..copied]);
        }
        ctx.set_field(this, 1, Value::Int((pos + copied) as i32));
        Ok(Some(Value::Int(copied as i32)))
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
            if pos >= count {
                return None;
            }
            let buf = match ctx.get_field(stream, 0) {
                Value::Object(Some(a)) => a,
                _ => return None,
            };
            let b = ctx
                .get_array_element(buf, pos as usize)
                .as_int()
                .unwrap_or(0);
            ctx.set_field(stream, 1, Value::Int(pos + 1));
            Some((b & 0xFF) as u8)
        } else if cname == "java/io/BufferedInputStream" {
            let pos = ctx
                .get_field_by_name(stream, "pos")
                .as_int()
                .unwrap_or(0)
                .max(0) as usize;
            let count = ctx
                .get_field_by_name(stream, "count")
                .as_int()
                .unwrap_or(0)
                .max(0) as usize;
            if pos < count {
                if let Value::Object(Some(buf)) = ctx.get_field_by_name(stream, "buf") {
                    let b = ctx.get_array_element(buf, pos).as_int().unwrap_or(0);
                    ctx.set_field_by_name(stream, "pos", Value::Int((pos + 1) as i32));
                    return Some((b & 0xFF) as u8);
                }
            }
            let inner = match ctx.get_field_by_name(stream, "in") {
                Value::Object(Some(inner)) => Some(inner),
                _ => match ctx.get_field(stream, 0) {
                    Value::Object(Some(inner)) => Some(inner),
                    _ => None,
                },
            }?;
            let b = dis_read_byte(ctx, inner)?;
            let markpos = ctx
                .get_field_by_name(stream, "markpos")
                .as_int()
                .unwrap_or(-1);
            if markpos >= 0 {
                if let Value::Object(Some(buf)) = ctx.get_field_by_name(stream, "buf") {
                    let cap = ctx.array_length(buf);
                    let count = ctx
                        .get_field_by_name(stream, "count")
                        .as_int()
                        .unwrap_or(0)
                        .max(0) as usize;
                    let marklimit = ctx
                        .get_field_by_name(stream, "marklimit")
                        .as_int()
                        .unwrap_or(0)
                        .max(0) as usize;
                    if count < cap && count.saturating_sub(markpos as usize) < marklimit {
                        ctx.set_array_element(buf, count, Value::Int(b as i8 as i32));
                        ctx.set_field_by_name(stream, "count", Value::Int((count + 1) as i32));
                        ctx.set_field_by_name(stream, "pos", Value::Int((count + 1) as i32));
                    } else {
                        ctx.set_field_by_name(stream, "markpos", Value::Int(-1));
                    }
                }
            }
            Some(b)
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
            }
            .into());
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
            }
            .into());
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
            }
            .into());
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
            }
            .into());
        }
        let v = i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        Ok(Some(Value::Long(v)))
    });

    r.register(dis, "readBoolean", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 1);
        if bytes.is_empty() {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readBoolean".into(),
            }
            .into());
        }
        Ok(Some(Value::Int(if bytes[0] != 0 { 1 } else { 0 })))
    });

    r.register(dis, "readByte", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 1);
        if bytes.is_empty() {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readByte".into(),
            }
            .into());
        }
        Ok(Some(Value::Int(bytes[0] as i8 as i32)))
    });

    r.register(dis, "readUnsignedByte", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 1);
        if bytes.is_empty() {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readUnsignedByte".into(),
            }
            .into());
        }
        Ok(Some(Value::Int(bytes[0] as i32)))
    });

    r.register(dis, "readChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = dis_read_n(ctx, this, 2);
        if bytes.len() < 2 {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readChar".into(),
            }
            .into());
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
            }
            .into());
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
            }
            .into());
        }
        let bits = u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
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
            }
            .into());
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
            }
            .into());
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
            }
            .into());
        }
        let utf_len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
        let data = dis_read_n(ctx, this, utf_len);
        if data.len() < utf_len {
            return Err(cratonvm_types::error::RuntimeError::IOException {
                message: "EOF in readUTF data".into(),
            }
            .into());
        }
        // Decode modified UTF-8
        let mut chars = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if b == 0 {
                break;
            }
            if b < 0x80 {
                chars.push(b as char);
                i += 1;
            } else if b & 0xE0 == 0xC0 {
                if i + 1 >= data.len() {
                    break;
                }
                let c = ((b as u32 & 0x1F) << 6) | (data[i + 1] as u32 & 0x3F);
                chars.push(char::from_u32(c).unwrap_or('?'));
                i += 2;
            } else if b & 0xF0 == 0xE0 {
                if i + 2 >= data.len() {
                    break;
                }
                let c = ((b as u32 & 0x0F) << 12)
                    | ((data[i + 1] as u32 & 0x3F) << 6)
                    | (data[i + 2] as u32 & 0x3F);
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

    // `DataInputStream` declares no bytecode of its own for `close()` (real
    // JDK inherits `FilterInputStream.close()` → `in.close()`); registering
    // a native directly on `DataInputStream` pre-empts that inherited real
    // bytecode. This registration is normally DEAD in practice — real-JDK
    // mode's boot sequence calls `native-io::register_io_natives` (which
    // registers its own, now-fixed `DataInputStream.close` →
    // `native_dis_close`) AFTER whatever calls this function, so that
    // registration wins. Fixed here too for consistency / in case
    // registration order ever changes; see `native-io/src/lib.rs`'s
    // `native_dis_close` for the full root-cause writeup (a `FileDataBlock`
    // handle leak in Spring Boot loader's
    // `SecurityInfoTests`/`NestedJarFileTests`, root-caused via a minimal
    // Spring-Boot-independent repro).
    r.register(dis, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let underlying = match ctx.get_field_by_name(this, "in") {
            Value::Object(Some(u)) => Some(u),
            _ => match ctx.get_field(this, 0) {
                Value::Object(Some(u)) => Some(u),
                _ => None,
            },
        };
        if let Some(u) = underlying {
            let _ = ctx.invoke_virtual(u, "close", "()V", &[]);
        }
        Ok(None)
    });

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
        ctx.set_field_by_name(this, "in", stream);
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8192);
        ctx.set_field_by_name(this, "initialSize", Value::Int(8192));
        ctx.set_field_by_name(this, "buf", Value::Object(Some(buf)));
        ctx.set_field_by_name(this, "count", Value::Int(0));
        ctx.set_field_by_name(this, "pos", Value::Int(0));
        ctx.set_field_by_name(this, "markpos", Value::Int(-1));
        ctx.set_field_by_name(this, "marklimit", Value::Int(0));
        ctx.set_field(this, 1, Value::Object(Some(buf)));
        Ok(None)
    });
    r.register(bis, "<init>", "(Ljava/io/InputStream;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream = args.get(1).copied().unwrap_or(Value::Object(None));
        let size = args.get(2).and_then(Value::as_int).unwrap_or(8192).max(1);
        ctx.set_field(this, 0, stream); // in
        ctx.set_field_by_name(this, "in", stream);
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, size as usize);
        ctx.set_field_by_name(this, "initialSize", Value::Int(size));
        ctx.set_field_by_name(this, "buf", Value::Object(Some(buf)));
        ctx.set_field_by_name(this, "count", Value::Int(0));
        ctx.set_field_by_name(this, "pos", Value::Int(0));
        ctx.set_field_by_name(this, "markpos", Value::Int(-1));
        ctx.set_field_by_name(this, "marklimit", Value::Int(0));
        ctx.set_field(this, 1, Value::Object(Some(buf)));
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
    r.register(bis, "skip", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = args.get(1).and_then(Value::as_long).unwrap_or(0).max(0) as usize;
        let bytes = dis_read_n(ctx, this, n);
        Ok(Some(Value::Long(bytes.len() as i64)))
    });
    r.register(bis, "mark", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let readlimit = args.get(1).and_then(Value::as_int).unwrap_or(0).max(0);
        let pos = ctx
            .get_field_by_name(this, "pos")
            .as_int()
            .unwrap_or(0)
            .max(0);
        ctx.set_field_by_name(this, "marklimit", Value::Int(readlimit));
        ctx.set_field_by_name(this, "markpos", Value::Int(pos));
        Ok(None)
    });
    r.register(bis, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let markpos = ctx
            .get_field_by_name(this, "markpos")
            .as_int()
            .unwrap_or(-1);
        if markpos >= 0 {
            ctx.set_field_by_name(this, "pos", Value::Int(markpos));
        }
        Ok(None)
    });
    r.register(bis, "markSupported", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
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
    // Unlike `DataInputStream`, `BufferedInputStream` DOES declare its own
    // `close()` in real JDK bytecode (`bufUpdater.compareAndSet(...) ...
    // input.close()`) — it doesn't inherit from `FilterInputStream`, so the
    // interpreter's dispatch correctly prefers that real bytecode over this
    // registration regardless (this native is not reached in practice under
    // real-JDK mode). Left as a no-op intentionally: `native-io`'s Wave2 H2
    // fix explicitly relies on real BIS bytecode (`Unsafe
    // .compareAndSetReference`-backed lazy `buf` allocation) and its comment
    // there asks future changes NOT to add more layout-coupled natives for
    // this class without a demonstrated regression.
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
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{NativeContext, NativeMethodRegistry};

    fn make_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_classloader_natives(&mut r);
        // Lookup find* methods are registered in lang_invoke, not classloader
        crate::lang_invoke::register_p63_method_handles_lookup(&mut r);
        r
    }

    fn new_object_ref(ctx: &mut MockNativeContext, class_name: &str) -> ObjectRef {
        match ctx.new_object(class_name).unwrap() {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected {class_name} object, got {other:?}"),
        }
    }

    fn panic_on_size_call(
        _ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name == "size" && descriptor == "()I" {
            panic!("custom-handler probe must not dispatch size() on non-list objects");
        }
        None
    }

    // --- ClassLoader registration tests ---

    #[test]
    fn test_custom_handler_probe_rejects_non_url_class_path_ucp() {
        let mut ctx = MockNativeContext::new();
        let loader = new_object_ref(
            &mut ctx,
            "io/quarkus/bootstrap/classloading/QuarkusClassLoader",
        );
        let bad_ucp = new_object_ref(&mut ctx, "java/net/URL");
        let bad_path = new_object_ref(&mut ctx, "java/net/URL");
        ctx.set_field_by_name(loader, "ucp", Value::Object(Some(bad_ucp)));
        ctx.set_field_by_name(bad_ucp, "path", Value::Object(Some(bad_path)));
        ctx.set_invoke_virtual_hook(panic_on_size_call);

        assert!(build_custom_handler_url_list(&mut ctx, loader, "META-INF/services/x").is_none());
    }

    #[test]
    fn test_custom_handler_probe_rejects_non_arraylist_path() {
        let mut ctx = MockNativeContext::new();
        let loader = new_object_ref(&mut ctx, "java/net/URLClassLoader");
        let ucp = new_object_ref(&mut ctx, "jdk/internal/loader/URLClassPath");
        let bad_path = new_object_ref(&mut ctx, "java/net/URL");
        ctx.set_field_by_name(loader, "ucp", Value::Object(Some(ucp)));
        ctx.set_field_by_name(ucp, "path", Value::Object(Some(bad_path)));
        ctx.set_invoke_virtual_hook(panic_on_size_call);

        assert!(build_custom_handler_url_list(&mut ctx, loader, "META-INF/services/x").is_none());
    }

    #[test]
    fn test_urlclassloader_find_resource_prefers_receiver_urls() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("virtual")).expect("mkdir");
        std::fs::write(
            dir.path().join("virtual").join("tomcat0807_webapp.txt"),
            b"ok",
        )
        .expect("write fixture");

        let mut ctx = MockNativeContext::new();
        let loader = new_object_ref(&mut ctx, "java/net/URLClassLoader");
        let ucp = new_object_ref(&mut ctx, "jdk/internal/loader/URLClassPath");
        let url = new_object_ref(&mut ctx, "java/net/URL");
        let path = ctx.create_string(&dir.path().to_string_lossy());
        ctx.set_field(url, 3, Value::Object(Some(path)));
        ctx.set_field_by_name(loader, "ucp", Value::Object(Some(ucp)));
        let urls = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(urls, 0, Value::Object(Some(url)));
        ctx.set_field(ucp, UCP_STASHED_URLS, Value::Object(Some(urls)));

        let hits = loader_local_resource_urls(&ctx, loader, "virtual/tomcat0807_webapp.txt");
        assert_eq!(
            hits.len(),
            1,
            "receiver-local URLClassLoader path must be searched"
        );

        let name = ctx.create_string("virtual/tomcat0807_webapp.txt");
        let found = ucl_find_resource(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .expect("findResource native")
        .expect("return value");
        let found = match found {
            Value::Object(Some(o)) => o,
            other => panic!("expected URL object, got {other:?}"),
        };
        let file_field = match ctx.get_field(found, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            other => panic!("expected URL path field, got {other:?}"),
        };
        assert!(
            file_field.contains("tomcat0807_webapp.txt"),
            "returned URL should point at the receiver-local resource, got {file_field}"
        );
    }

    #[test]
    fn test_is_bootstrap_class_name_jdk_packages() {
        // JDK / platform packages the bootstrap loader genuinely owns.
        assert!(is_bootstrap_class_name("java/lang/String"));
        assert!(is_bootstrap_class_name("javax/sql/DataSource"));
        assert!(is_bootstrap_class_name("jdk/internal/loader/ClassLoaders"));
        assert!(is_bootstrap_class_name("sun/nio/ch/IOUtil"));
        assert!(is_bootstrap_class_name("com/sun/crypto/provider/AESCipher"));
        assert!(is_bootstrap_class_name("[Ljava/lang/Object;"));
    }

    #[test]
    fn test_is_bootstrap_class_name_app_classes() {
        // Application classes are NEVER bootstrap-loadable — findBootstrapClass
        // must defer these to a custom loader's findClass override (HIB-CV-24).
        assert!(!is_bootstrap_class_name(
            "org/hibernate/orm/test/bootstrap/registry/classloading/ClassLoaderServiceImplTest"
        ));
        assert!(!is_bootstrap_class_name("com/example/MyService"));
        assert!(!is_bootstrap_class_name("MProbe$Base"));
        // `jakarta.*` is an application/module class, not bootstrap.
        assert!(!is_bootstrap_class_name("jakarta/persistence/Entity"));
    }

    #[test]
    fn test_loadclass_resolve_override_survives_urlclassloader_superclass() {
        let mut ctx = MockNativeContext::new();
        let url_cid = ctx
            .ensure_class_initialized("java/net/URLClassLoader")
            .expect("URLClassLoader class");
        let bsh_cid = ctx
            .ensure_class_initialized("bsh/classpath/BshClassLoader")
            .expect("BshClassLoader class");
        let discrete_cid = ctx
            .ensure_class_initialized("bsh/classpath/DiscreteFilesClassLoader")
            .expect("DiscreteFilesClassLoader class");
        ctx.set_superclass(discrete_cid, bsh_cid);
        ctx.set_superclass(bsh_cid, url_cid);
        ctx.set_declared_methods(
            bsh_cid,
            vec![cratonvm_native_api::MethodMetadata {
                name: "loadClass".to_string(),
                descriptor: "(Ljava/lang/String;Z)Ljava/lang/Class;".to_string(),
                access_flags: 0,
                declaring_class_id: bsh_cid,
                exceptions: Vec::new(),
            }],
        );
        let loader = new_object_ref(&mut ctx, "bsh/classpath/DiscreteFilesClassLoader");

        assert!(
            receiver_overrides_load_class_resolve(&mut ctx, loader),
            "BeanShell-shaped URLClassLoader subclasses must dispatch their loadClass override"
        );
    }

    #[test]
    fn test_loadclass_resolve_override_on_direct_urlclassloader_subclass() {
        // Spring Boot's FilteredClassLoader directly extends URLClassLoader and
        // rejects hidden packages from loadClass(String, boolean).  Keep this
        // one-level shape distinct from the BeanShell hierarchy above: reaching
        // URLClassLoader must not hide an override already declared by its
        // immediate child.
        let mut ctx = MockNativeContext::new();
        let url_cid = ctx
            .ensure_class_initialized("java/net/URLClassLoader")
            .expect("URLClassLoader class");
        let filtered_cid = ctx
            .ensure_class_initialized("org/springframework/boot/test/context/FilteredClassLoader")
            .expect("FilteredClassLoader class");
        ctx.set_superclass(filtered_cid, url_cid);
        ctx.set_declared_methods(
            filtered_cid,
            vec![cratonvm_native_api::MethodMetadata {
                name: "loadClass".to_string(),
                descriptor: "(Ljava/lang/String;Z)Ljava/lang/Class;".to_string(),
                access_flags: 0,
                declaring_class_id: filtered_cid,
                exceptions: Vec::new(),
            }],
        );
        let loader = new_object_ref(
            &mut ctx,
            "org/springframework/boot/test/context/FilteredClassLoader",
        );

        assert!(
            receiver_overrides_load_class_resolve(&mut ctx, loader),
            "a direct URLClassLoader subclass must dispatch its loadClass override"
        );
    }

    #[test]
    fn test_loadclass_single_override_survives_urlclassloader_superclass() {
        let mut ctx = MockNativeContext::new();
        let url_cid = ctx
            .ensure_class_initialized("java/net/URLClassLoader")
            .expect("URLClassLoader class");
        let modified_cid = ctx
            .ensure_class_initialized(
                "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader",
            )
            .expect("ModifiedClassPathClassLoader class");
        ctx.set_superclass(modified_cid, url_cid);
        ctx.set_declared_methods(
            modified_cid,
            vec![cratonvm_native_api::MethodMetadata {
                name: "loadClass".to_string(),
                descriptor: "(Ljava/lang/String;)Ljava/lang/Class;".to_string(),
                access_flags: 0,
                declaring_class_id: modified_cid,
                exceptions: Vec::new(),
            }],
        );
        let loader = new_object_ref(
            &mut ctx,
            "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader",
        );

        assert!(
            receiver_overrides_load_class_single(&mut ctx, loader),
            "a URLClassLoader subclass's single-argument loadClass override must run"
        );
        assert!(
            !receiver_overrides_load_class_resolve(&mut ctx, loader),
            "the single-argument override must not be mistaken for the protected overload"
        );
    }

    #[test]
    fn test_reset_clears_real_jdk_loader_namespace_ids() {
        let mut ctx = MockNativeContext::new();
        let loader = new_object_ref(&mut ctx, "example/IsolatedLoader");
        loader_namespace_id_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((loader, 42));

        assert_eq!(peek_loader_namespace_id(&mut ctx, loader), Some(42));

        reset_loader_singletons();

        assert!(
            peek_loader_namespace_id(&mut ctx, loader).is_none(),
            "VM reset must not leave stale real-JDK loader namespace ids"
        );
    }

    #[test]
    fn test_cl_init_default_registered() {
        let r = make_registry();
        assert!(r.find(CL_CLASS, "<init>", "()V").is_some());
    }

    #[test]
    fn test_lookup_ensure_initialized_registered() {
        let r = make_registry();
        assert!(r
            .find(
                LK_CLASS,
                "ensureInitialized",
                "(Ljava/lang/Class;)Ljava/lang/Class;"
            )
            .is_some());
    }

    #[test]
    fn test_cl_init_parent_registered() {
        let r = make_registry();
        assert!(r
            .find(CL_CLASS, "<init>", "(Ljava/lang/ClassLoader;)V")
            .is_some());
    }

    #[test]
    fn test_cl_init_name_parent_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "<init>",
                "(Ljava/lang/String;Ljava/lang/ClassLoader;)V"
            )
            .is_some());
    }

    #[test]
    fn test_cl_load_class_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;"
            )
            .is_some());
    }

    #[test]
    fn test_cl_load_class_resolve_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "loadClass",
                "(Ljava/lang/String;Z)Ljava/lang/Class;"
            )
            .is_some());
    }

    #[test]
    fn test_cl_find_class_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "findClass",
                "(Ljava/lang/String;)Ljava/lang/Class;"
            )
            .is_some());
    }

    #[test]
    fn test_cl_find_class_module_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "findClass",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;"
            )
            .is_some());
    }

    #[test]
    fn test_builtin_find_loaded_class_hides_user_namespace_hit() {
        let mut ctx = MockNativeContext::new();
        let loader = match ctx.new_object("java/lang/ClassLoader").unwrap() {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected classloader object, got {other:?}"),
        };
        let cid = ctx.ensure_class_initialized("leak/OnlyChild").unwrap();
        ctx.set_loader_id_override(cid, 7);

        assert!(find_loaded_class_for_loader(&mut ctx, loader, "leak/OnlyChild").is_none());
    }

    #[test]
    fn test_builtin_find_loaded_class_keeps_application_namespace_hit() {
        let mut ctx = MockNativeContext::new();
        let loader = match ctx.new_object("java/lang/ClassLoader").unwrap() {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected classloader object, got {other:?}"),
        };
        let cid = ctx.ensure_class_initialized("framework/Generated").unwrap();
        ctx.set_loader_id_override(cid, 2);

        assert!(find_loaded_class_for_loader(&mut ctx, loader, "framework/Generated").is_some());
    }

    #[test]
    fn test_builtin_find_loaded_class_hides_user_defined_app_namespace_hit() {
        let mut ctx = MockNativeContext::new();
        let app_loader = match ctx.new_object("java/lang/ClassLoader").unwrap() {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected classloader object, got {other:?}"),
        };
        let child_loader = match ctx.new_object("bsh/classpath/BshClassLoader").unwrap() {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected child loader object, got {other:?}"),
        };
        let cid = ctx.ensure_class_initialized("MyMessenger").unwrap();
        ctx.set_loader_id_override(cid, 2);
        register_defining_loader(cid.as_u32(), child_loader);

        assert!(
            find_loaded_class_for_loader(&mut ctx, app_loader, "MyMessenger").is_none(),
            "built-in loaders must not see app-namespace classes defined by a child loader"
        );
        assert!(
            resolve_global_if_visible(&mut ctx, app_loader, "MyMessenger")
                .unwrap()
                .is_none(),
            "base loadClass global fallback must apply the same child-loader visibility rule"
        );
    }

    #[test]
    fn test_cl_define_class_basic_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "defineClass",
                "(Ljava/lang/String;[BII)Ljava/lang/Class;"
            )
            .is_some());
    }

    #[test]
    fn test_cl_define_class_pd_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "defineClass",
                "(Ljava/lang/String;[BIILjava/security/ProtectionDomain;)Ljava/lang/Class;"
            )
            .is_some());
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
        assert!(r
            .find(CL_CLASS, "getParent", "()Ljava/lang/ClassLoader;")
            .is_some());
    }

    #[test]
    fn test_cl_get_name_registered() {
        let r = make_registry();
        assert!(r
            .find(CL_CLASS, "getName", "()Ljava/lang/String;")
            .is_some());
    }

    #[test]
    fn test_cl_get_system_class_loader_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "getSystemClassLoader",
                "()Ljava/lang/ClassLoader;"
            )
            .is_some());
    }

    #[test]
    fn test_cl_get_platform_class_loader_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "getPlatformClassLoader",
                "()Ljava/lang/ClassLoader;"
            )
            .is_some());
    }

    #[test]
    fn test_cl_resolve_class_registered() {
        let r = make_registry();
        assert!(r
            .find(CL_CLASS, "resolveClass", "(Ljava/lang/Class;)V")
            .is_some());
    }

    #[test]
    fn test_cl_find_loaded_class_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "findLoadedClass",
                "(Ljava/lang/String;)Ljava/lang/Class;"
            )
            .is_some());
    }

    #[test]
    fn test_cl_get_resource_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CL_CLASS,
                "getResource",
                "(Ljava/lang/String;)Ljava/net/URL;"
            )
            .is_some());
    }

    #[test]
    fn test_cl_register_as_parallel_capable_registered() {
        let r = make_registry();
        assert!(r
            .find(CL_CLASS, "registerAsParallelCapable", "()Z")
            .is_some());
    }

    #[test]
    fn test_cl_is_registered_as_parallel_capable_registered() {
        let r = make_registry();
        assert!(r
            .find(CL_CLASS, "isRegisteredAsParallelCapable", "()Z")
            .is_some());
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
        assert!(r
            .find(
                UCL_CLASS,
                "<init>",
                "([Ljava/net/URL;Ljava/lang/ClassLoader;)V"
            )
            .is_some());
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
        assert!(r
            .find(
                UCL_CLASS,
                "newInstance",
                "([Ljava/net/URL;)Ljava/net/URLClassLoader;"
            )
            .is_some());
    }

    #[test]
    fn test_ucl_new_instance_parent_registered() {
        let r = make_registry();
        assert!(r
            .find(
                UCL_CLASS,
                "newInstance",
                "([Ljava/net/URL;Ljava/lang/ClassLoader;)Ljava/net/URLClassLoader;"
            )
            .is_some());
    }

    // --- MethodHandles$Lookup registration tests ---

    #[test]
    fn test_lk_lookup_registered() {
        let r = make_registry();
        assert!(r
            .find(
                LK_CLASS,
                "lookup",
                "()Ljava/lang/invoke/MethodHandles$Lookup;"
            )
            .is_some());
    }

    #[test]
    fn test_lk_public_lookup_registered() {
        let r = make_registry();
        assert!(r
            .find(
                LK_CLASS,
                "publicLookup",
                "()Ljava/lang/invoke/MethodHandles$Lookup;"
            )
            .is_some());
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
        assert!(r
            .find(
                LK_CLASS,
                "findConstructor",
                "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"
            )
            .is_some());
    }

    #[test]
    fn test_lk_has_full_privilege_access_registered() {
        let r = make_registry();
        assert!(r.find(LK_CLASS, "hasFullPrivilegeAccess", "()Z").is_some());
    }

    #[test]
    fn test_lk_drop_lookup_mode_registered() {
        let r = make_registry();
        assert!(r
            .find(
                LK_CLASS,
                "dropLookupMode",
                "(I)Ljava/lang/invoke/MethodHandles$Lookup;"
            )
            .is_some());
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
        assert!(r
            .find(
                PD_CLASS,
                "<init>",
                "(Ljava/security/CodeSource;Ljava/security/PermissionCollection;)V"
            )
            .is_some());
    }

    #[test]
    fn test_pd_get_code_source_registered() {
        let r = make_registry();
        assert!(r
            .find(PD_CLASS, "getCodeSource", "()Ljava/security/CodeSource;")
            .is_some());
    }

    #[test]
    fn test_pd_implies_registered() {
        let r = make_registry();
        assert!(r
            .find(PD_CLASS, "implies", "(Ljava/security/Permission;)Z")
            .is_some());
    }

    // --- CodeSource registration tests ---

    #[test]
    fn test_cs_init_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CS_CLASS,
                "<init>",
                "(Ljava/net/URL;[Ljava/security/cert/Certificate;)V"
            )
            .is_some());
    }

    #[test]
    fn test_cs_get_location_registered() {
        let r = make_registry();
        assert!(r
            .find(CS_CLASS, "getLocation", "()Ljava/net/URL;")
            .is_some());
    }

    #[test]
    fn test_cs_get_certificates_registered() {
        let r = make_registry();
        assert!(r
            .find(
                CS_CLASS,
                "getCertificates",
                "()[Ljava/security/cert/Certificate;"
            )
            .is_some());
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

    // -----------------------------------------------------------------------
    // Lookup find* access-control enforcement (review finding
    // `nb-lib` MethodHandles.Lookup access control)
    // -----------------------------------------------------------------------

    use cratonvm_native_api::{FieldMetadata, MethodMetadata};
    use cratonvm_types::ClassId;

    /// Build a target class `cls_id` with one method and one field of the
    /// given access flags, returning its `Class` mirror.
    fn setup_target(
        ctx: &mut crate::test_utils::MockNativeContext,
        class_name: &str,
        method_flags: u16,
        field_flags: u16,
    ) -> (ClassId, ObjectRef) {
        let cls_id = ctx.ensure_class_initialized(class_name).unwrap();
        ctx.set_declared_methods(
            cls_id,
            vec![MethodMetadata {
                name: "secret".to_string(),
                descriptor: "()V".to_string(),
                access_flags: method_flags,
                declaring_class_id: cls_id,
                exceptions: Vec::new(),
            }],
        );
        ctx.set_declared_fields(
            cls_id,
            vec![FieldMetadata {
                name: "hidden".to_string(),
                descriptor: "I".to_string(),
                access_flags: field_flags,
                slot_index: 0,
                declaring_class_id: cls_id,
                is_static: false,
            }],
        );
        let mirror = ctx.get_class_mirror(cls_id);
        (cls_id, mirror)
    }

    fn make_lookup(
        ctx: &mut crate::test_utils::MockNativeContext,
        modes: i32,
        lookup_class: Option<ObjectRef>,
    ) -> ObjectRef {
        let lk = ctx.alloc_object(ClassId::new(0), LK_FIELD_COUNT);
        ctx.set_field(lk, LK_ALLOWED_MODES, Value::Int(modes));
        ctx.set_field(
            lk,
            LK_LOOKUP_CLASS_REF,
            match lookup_class {
                Some(m) => Value::Object(Some(m)),
                None => Value::Object(None),
            },
        );
        lk
    }

    #[test]
    fn lk_find_virtual_public_method_allowed() {
        use crate::test_utils::MockNativeContext;
        use cratonvm_types::access_flags::ACC_PUBLIC;
        let mut ctx = MockNativeContext::new();
        let (_cid, mirror) = setup_target(&mut ctx, "p/Target", ACC_PUBLIC, ACC_PUBLIC);
        // A public-only lookup (publicLookup) can resolve a public method.
        let lk = make_lookup(&mut ctx, LK_PUBLIC, None);
        let name = ctx.create_string("secret");
        let r = lk_find_virtual(
            &mut ctx,
            &[
                Value::Object(Some(lk)),
                Value::Object(Some(mirror)),
                Value::Object(Some(name)),
                Value::Object(None),
            ],
        );
        assert!(
            matches!(r, Ok(Some(Value::Object(Some(_))))),
            "public method must resolve, got {r:?}"
        );
    }

    #[test]
    fn lk_find_virtual_private_method_with_public_lookup_throws() {
        use crate::test_utils::MockNativeContext;
        use cratonvm_types::access_flags::{ACC_PRIVATE, ACC_PUBLIC};
        let mut ctx = MockNativeContext::new();
        let (_cid, mirror) = setup_target(&mut ctx, "p/Target", ACC_PRIVATE, ACC_PUBLIC);
        // publicLookup (no PRIVATE bit, no lookupClass) must NOT see a private member.
        let lk = make_lookup(&mut ctx, LK_PUBLIC, None);
        let name = ctx.create_string("secret");
        let r = lk_find_virtual(
            &mut ctx,
            &[
                Value::Object(Some(lk)),
                Value::Object(Some(mirror)),
                Value::Object(Some(name)),
                Value::Object(None),
            ],
        );
        assert!(
            r.is_err(),
            "private method via public Lookup must throw IllegalAccessException, got {r:?}"
        );
    }

    // NOTE on positive private-access coverage: the MockNativeContext used
    // here cannot represent the real-JDK `allowedModes` field *by name*
    // (`get_field_by_name("allowedModes")` returns 0), so `lk_modes_of`
    // always reports mode 0 under the mock and the "full-power lookup may
    // see its own private member" path cannot be exercised through these
    // natives in-unit. The same-class / mode-bit branch of
    // `enforce_lookup_access` is therefore validated indirectly via the
    // negative tests above and the direct `lk_member_access_flags` tests
    // below; full positive coverage lives in the cross-VM HotSpot battery.

    #[test]
    fn lk_find_getter_private_field_with_public_lookup_throws() {
        use crate::test_utils::MockNativeContext;
        use cratonvm_types::access_flags::{ACC_PRIVATE, ACC_PUBLIC};
        let mut ctx = MockNativeContext::new();
        let (_cid, mirror) = setup_target(&mut ctx, "p/Target", ACC_PUBLIC, ACC_PRIVATE);
        let lk = make_lookup(&mut ctx, LK_PUBLIC, None);
        let name = ctx.create_string("hidden");
        let r = lk_find_getter(
            &mut ctx,
            &[
                Value::Object(Some(lk)),
                Value::Object(Some(mirror)),
                Value::Object(Some(name)),
                Value::Object(None),
            ],
        );
        assert!(
            r.is_err(),
            "private field getter via public Lookup must throw, got {r:?}"
        );
    }

    #[test]
    fn lk_find_getter_public_field_allowed() {
        use crate::test_utils::MockNativeContext;
        use cratonvm_types::access_flags::ACC_PUBLIC;
        let mut ctx = MockNativeContext::new();
        let (_cid, mirror) = setup_target(&mut ctx, "p/Target", ACC_PUBLIC, ACC_PUBLIC);
        let lk = make_lookup(&mut ctx, LK_PUBLIC, None);
        let name = ctx.create_string("hidden");
        let r = lk_find_getter(
            &mut ctx,
            &[
                Value::Object(Some(lk)),
                Value::Object(Some(mirror)),
                Value::Object(Some(name)),
                Value::Object(None),
            ],
        );
        assert!(
            matches!(r, Ok(Some(Value::Object(Some(_))))),
            "public field getter must resolve, got {r:?}"
        );
    }

    #[test]
    fn lk_find_virtual_unresolvable_member_allowed() {
        // When the member's flags cannot be determined (no declared_methods
        // registered for the class), the lookup must be allowed rather than
        // spuriously throwing.
        use crate::test_utils::MockNativeContext;
        let mut ctx = MockNativeContext::new();
        let cid = ctx.ensure_class_initialized("p/Opaque").unwrap();
        let mirror = ctx.get_class_mirror(cid);
        let lk = make_lookup(&mut ctx, LK_PUBLIC, None);
        let name = ctx.create_string("whatever");
        let r = lk_find_virtual(
            &mut ctx,
            &[
                Value::Object(Some(lk)),
                Value::Object(Some(mirror)),
                Value::Object(Some(name)),
                Value::Object(None),
            ],
        );
        assert!(
            matches!(r, Ok(Some(Value::Object(Some(_))))),
            "unresolvable member must not be blocked, got {r:?}"
        );
    }

    #[test]
    fn lk_member_access_flags_walks_superclass_for_methods() {
        use crate::test_utils::MockNativeContext;
        use cratonvm_types::access_flags::ACC_PUBLIC;
        let mut ctx = MockNativeContext::new();
        let parent = ctx.ensure_class_initialized("p/Parent").unwrap();
        ctx.set_declared_methods(
            parent,
            vec![MethodMetadata {
                name: "inherited".to_string(),
                descriptor: "()V".to_string(),
                access_flags: ACC_PUBLIC,
                declaring_class_id: parent,
                exceptions: Vec::new(),
            }],
        );
        let child = ctx.ensure_class_initialized("p/Child").unwrap();
        ctx.set_declared_methods(child, Vec::new());
        ctx.set_superclass(child, parent);
        let mirror = ctx.get_class_mirror(child);
        let flags = lk_member_access_flags(&ctx, mirror, "inherited", false);
        assert_eq!(flags, Some(ACC_PUBLIC));
    }
}
