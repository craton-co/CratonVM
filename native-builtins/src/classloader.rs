// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ClassLoader hierarchy, URLClassLoader, MethodHandles.Lookup, ProtectionDomain,
//! and CodeSource native method implementations.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::obj_arg;
use crate::service_loader::impl_jars_load_class;
use crate::util_concurrent_ext::try_alloc_concurrent_synthetic;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
use cratonvm_types::{ObjectRef, Value};

/// Monotonic counter for generating unique hidden class names.
pub static HIDDEN_CLASS_COUNTER: AtomicU64 = AtomicU64::new(0);

/// URLClassLoader resource and class lookups can occur thousands of times for
/// one immutable URL list. Reopening every archive for each lookup is already
/// wasteful; a manifest-only pathing JAR makes it catastrophic because each
/// lookup must also open all of its `Class-Path` dependencies. Keep a small,
/// process-local cache keyed by the complete URL list. It intentionally owns
/// no Java `ObjectRef`, so it cannot keep a loader alive.
///
/// The bound of [`cached_class_path_for_paths`] (gc-common w9-a). This
/// constant used to bound a second, identical cache beside it that no code
/// path read any more; that one was removed, and the live one, which had no
/// bound at all, now uses it.
const LOCAL_URL_CLASS_PATH_CACHE_LIMIT: usize = 64;

// Real JDK URLClassLoader instances have the JDK's own object layout, so the
// synthetic UCL_CLOSED slot is not available there. Identity hashes survive a
// moving collection and do not keep the loader alive; use one to carry the
// close state for both layout families.
//
// Keyed by `(vm_identity, identity hash)` (gc-common w9-a). Every heap numbers
// identity hashes from the same seed, so a bare hash key made VM A's
// `close()` report VM B's unrelated loader with the same hash as closed (its
// `findResource` / `findClass` then answered null / `ClassNotFoundException`).
// And `reset_loader_singletons` wiped the whole set whenever ANY VM was
// created, reopening a concurrently live VM's closed loaders. The set is now
// dropped per VM by `forget_vm_loader_singletons` instead.
//
// Each row's value is the loader's address, kept current by
// `gc_reconcile_defining_loaders` (gc-common w11-e). A closed loader used to
// keep its row until its VM was torn down: one row per closed loader, which a
// redeploy loop closes one of each time, and a stale "closed" answer for any
// later loader of the same VM that drew the same identity hash. The reconcile
// now drops the row when the loader dies and follows it when the loader
// moves, like the loader-meta table beside it.
//
// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
// rank 20): the hash is a BUCKET now, and every lookup compares the loader's
// current address with the rows'. Two live loaders of one VM can share an
// identity hash (every heap mints it from one 32-bit counter that wraps), and
// the hash-only lookup answered an OPEN loader that collided with a closed one
// as closed (`findClass` / `findResource` then refused), while `ucl_mark_open`
// on one reopened the other. The address is exact: every move happens in a
// stop-the-world pause whose epilogue runs the reconcile before any mutator
// resumes, so a row always names its loader's current address when a native
// looks.
//
// gc-common w33-a: since the 8-byte object header, a compact instance's
// identity hash is 20 counter bits plus 11 class bits
// (`ObjectHeader::short_hash_value`), so two loaders of ONE class share a
// hash every 2^20 hash mints rather than every 2^31. The bucket-plus-address
// scheme above does not depend on how often that happens: a bucket simply
// holds more rows (one per closed loader of that hash), and every answer is
// still decided by the address.
fn closed_url_classloader_ids(
) -> &'static Mutex<std::collections::HashMap<(usize, i32), Vec<usize>>> {
    static INSTANCE: OnceLock<Mutex<std::collections::HashMap<(usize, i32), Vec<usize>>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// `loader` must be CURRENT (it is matched by address).
pub(crate) fn ucl_is_closed(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    let key = (ctx.vm_identity(), ctx.identity_hash_code(loader));
    let addr = loader.as_ptr() as usize;
    closed_url_classloader_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .is_some_and(|rows| rows.contains(&addr))
}

fn ucl_mark_open(ctx: &dyn NativeContext, loader: ObjectRef) {
    let key = (ctx.vm_identity(), ctx.identity_hash_code(loader));
    let addr = loader.as_ptr() as usize;
    let mut set = closed_url_classloader_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(rows) = set.get_mut(&key) {
        rows.retain(|&a| a != addr);
        if rows.is_empty() {
            set.remove(&key);
        }
    }
}
/// Record that `URLClassLoader.close()` ran on `loader`. Both `close()`
/// natives write here: the synthetic `ucl_close` and, since gc-common w12-d,
/// the real-JDK bridge (`servlet::register_url_classloader_close_bridge`),
/// which kept its own process-wide, bare-hash, never-pruned set before
/// (`common-w11e-real-mode-closed-url-loader-set-is-one-per-process`).
pub(crate) fn ucl_mark_closed(ctx: &dyn NativeContext, loader: ObjectRef) {
    let key = (ctx.vm_identity(), ctx.identity_hash_code(loader));
    let addr = loader.as_ptr() as usize;
    let mut set = closed_url_classloader_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rows = set.entry(key).or_default();
    if !rows.contains(&addr) {
        rows.push(addr);
    }
}

// ---------------------------------------------------------------------------
// Singleton classloader instances (JVM spec: one instance per built-in loader)
// ---------------------------------------------------------------------------

/// The built-in loader singletons of ONE VM.
///
/// "One instance per built-in loader" is a *per-VM* invariant, not a
/// per-process one. These were process-global `Mutex<Option<ObjectRef>>`
/// cells, reset by `Vm::new`, which is correct only while VMs are created and
/// disposed of strictly in sequence. A Rust test binary runs its `#[test]`
/// functions on several threads, so two `Vm`s are routinely *concurrently*
/// live — and then VM B's `Vm::new` wiped the cell VM A was using, VM A
/// re-created its loader into the shared cell, and whichever VM read it next
/// got a `ClassLoader` object allocated in the OTHER VM's heap. Reading a
/// field off it (`classloader_parent` → `class_id_of`) then dereferenced a
/// foreign, possibly freed, address: the SIGSEGV that made
/// `cargo test --test interpreter_tests` unrunnable without
/// `--test-threads=1`.
///
/// Keyed by [`NativeContext::vm_identity`], the same scheme
/// `security_manager`'s `VmSecurityState` uses, and torn down from
/// `release_vm_native_state`.
#[derive(Default)]
struct VmLoaderSingletons {
    platform: Option<ObjectRef>,
    app: Option<ObjectRef>,
    /// `--jdk-only`: the one `ProtectionDomain` per class-path code base of
    /// the application loader, as `SecureClassLoader`'s `pdcache` keeps one
    /// per `CodeSource`. Built for the load-time transformers before the class
    /// exists and answered again by its `getProtectionDomain()`
    /// (`lang_class::app_class_path_domain`). Rooted with the loader it names,
    /// which never unloads; bounded by the class path's entries.
    app_code_source_domains: std::collections::HashMap<String, ObjectRef>,
}

static LOADER_SINGLETONS: OnceLock<Mutex<std::collections::HashMap<usize, VmLoaderSingletons>>> =
    OnceLock::new();

/// Run `f` with the singleton table locked.
///
/// The lock is never held across a Java allocation: `get_or_create_*_loader`
/// allocates first and publishes afterwards, exactly as
/// `security_manager::with_security_state` requires, because
/// [`gc_scan_loader_singleton_roots`] takes this same lock at a safepoint.
fn with_loader_singletons<R>(
    f: impl FnOnce(&mut std::collections::HashMap<usize, VmLoaderSingletons>) -> R,
) -> R {
    let mut guard = LOADER_SINGLETONS
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

fn platform_loader_of(vm: usize) -> Option<ObjectRef> {
    with_loader_singletons(|table| table.get(&vm).and_then(|row| row.platform))
}

fn set_platform_loader(vm: usize, value: Option<ObjectRef>) {
    with_loader_singletons(|table| table.entry(vm).or_default().platform = value);
}

fn app_loader_of(vm: usize) -> Option<ObjectRef> {
    with_loader_singletons(|table| table.get(&vm).and_then(|row| row.app))
}

fn set_app_loader(vm: usize, value: Option<ObjectRef>) {
    with_loader_singletons(|table| {
        let row = table.entry(vm).or_default();
        row.app = value;
        // Each cached domain names the loader it was built for.
        row.app_code_source_domains.clear();
    });
}

/// The application loader's cached `ProtectionDomain` for `code_base`, if one
/// was built (see `VmLoaderSingletons::app_code_source_domains`).
pub(crate) fn app_code_source_domain(vm: usize, code_base: &str) -> Option<ObjectRef> {
    with_loader_singletons(|table| {
        table
            .get(&vm)
            .and_then(|row| row.app_code_source_domains.get(code_base).copied())
    })
}

/// Publish `domain` as the application loader's `ProtectionDomain` for
/// `code_base` and answer the published one: a domain another thread published
/// first wins, so every caller hands out the same object.
pub(crate) fn publish_app_code_source_domain(
    vm: usize,
    code_base: &str,
    domain: ObjectRef,
) -> ObjectRef {
    with_loader_singletons(|table| {
        *table
            .entry(vm)
            .or_default()
            .app_code_source_domains
            .entry(code_base.to_string())
            .or_insert(domain)
    })
}

/// Which built-in loader `this` is, as the `ClassLoaderId` wire ordinal
/// (`NATIVE_EXTENSION` for platform, `NATIVE_APPLICATION` for app), or `None`
/// when `this` is neither singleton (in practice: a user-defined loader, or
/// bootstrap — which has no `ClassLoader` object to be `this` in the first
/// place). Used by `find_loaded_class_for_loader_inner` to bound a built-in
/// loader's `findLoadedClass` visibility to itself and its own ancestors
/// (Bootstrap -> Extension -> Application is a strict chain, not a mutually
/// visible group). Mirrors `parent_is_platform`'s identity check: the
/// singleton reference is the fast path, the class name is the real-JDK
/// fallback (the JDK can manufacture another loader object of the same kind
/// before our singleton is observed).
fn builtin_loader_ordinal(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<u32> {
    let vm = ctx.vm_identity();
    if platform_loader_of(vm).is_some_and(|p| p.as_ptr() == this.as_ptr())
        || ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader")
    {
        return Some(cratonvm_types::ClassLoaderId::NATIVE_EXTENSION);
    }
    if app_loader_of(vm).is_some_and(|p| p.as_ptr() == this.as_ptr())
        || ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$AppClassLoader")
    {
        return Some(cratonvm_types::ClassLoaderId::NATIVE_APPLICATION);
    }
    None
}

/// Temporary debug-only accessor (CRATONVM_DBG_OBSREG investigation).
pub(crate) fn platform_loader_dbg(vm: usize) -> Option<ObjectRef> {
    platform_loader_of(vm)
}

/// Per-VM teardown: drop the built-in loader singletons held for
/// `vm_identity`.
///
/// Called from `release_vm_native_state` when the last `Arc<SharedVm>` goes
/// away. Without it the row — and the raw heap addresses in it — outlives the
/// heap that produced them, and a later VM that reused the identity would
/// inherit a dead `ClassLoader`.
pub fn forget_vm_loader_singletons(vm_identity: usize) {
    with_loader_singletons(|table| table.remove(&vm_identity));
    {
        let mut map = defining_loader_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dropped_ids = map.forget_vm(vm_identity);
        // gc-common w9-a: the key-set bits of the ids this VM held used to
        // stay set forever after its teardown (a stale `true` costs every
        // later `defining_loader_for` on that id a mutex acquisition). Clear
        // them, under the map lock, for the ids no other VM still holds —
        // the same rule `gc_reconcile_defining_loaders` applies to the ids it
        // prunes.
        if !dropped_ids.is_empty() {
            let still_held: std::collections::HashSet<u32> =
                map.rows.keys().map(|&(_, cid)| cid).collect();
            for id in dropped_ids {
                if !still_held.contains(&id) {
                    clear_defining_loader_bit(id);
                }
            }
        }
    }
    orphaned_defining_loader_classes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(vm, _)| *vm != vm_identity);
    // gc-common w18-d: the non-strong hidden rows hold this heap's mirror
    // addresses. (`forget_vm_mirror_pins` below drops the exclusions.)
    if ANY_NON_STRONG_HIDDEN.load(Ordering::Acquire) {
        non_strong_hidden_store()
            .lock()
            .retain(|&(vm, _), _| vm != vm_identity);
    }
    // gc-common w9-a: the loader-namespace and loader-meta tables, and the
    // closed-URLClassLoader set, carry the owning VM too. Their raw heap
    // addresses (and identity hashes) mean nothing once this heap is gone, and
    // a VM that later reuses the identity must not inherit them.
    loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(vm, _, _)| *vm != vm_identity);
    loader_meta_store()
        .lock()
        .retain(|(vm, _, _)| *vm != vm_identity);
    closed_url_classloader_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(vm, _), _| *vm != vm_identity);
    // gc-common w10-f: the JDK boot/platform module tables and the
    // platform-package memo are read out of THIS VM's image, so they are per
    // VM too. One statement each, nothing else held (both are `Scratch`).
    builtin_module_sets_cache().lock().remove(&vm_identity);
    platform_package_memo().lock().remove(&vm_identity);
    // The GC marker's three liveness-pin registries hold raw heap addresses
    // from the heap that is going away. They used to be wiped wholesale when
    // the NEXT VM was created, which is both too late (the addresses were
    // stale in between) and too broad (it took a concurrently-live VM's rows
    // with them). Dropping them here, per VM, is neither.
    cratonvm_types::loader_pin::forget_vm_loader_pins(vm_identity);
    cratonvm_types::mirror_pin::forget_vm_mirror_pins(vm_identity);
    cratonvm_types::metadata_pin::forget_vm_metadata_pins(vm_identity);
}

/// Reset the process-wide (not yet VM-scoped) classloader side-tables.
///
/// Called when creating a new VM to avoid stale ObjectRefs from a previous VM
/// instance. The built-in loader singletons are NOT reset here any more — they
/// are keyed by `vm_identity` (see [`VmLoaderSingletons`]) and a fresh VM
/// starts with an empty row by construction, so there is nothing to clear and
/// nothing of a concurrently-live VM's to destroy.
pub fn reset_loader_singletons() {
    // NOT cleared here any more (gc-common w9-a): `closed_url_classloader_ids`
    // is keyed by `(vm_identity, identity hash)` and dropped per VM by
    // `forget_vm_loader_singletons`, for the reason the note below gives.
    //
    // The process-wide, mirror-keyed `classData` side table this used to
    // clear is gone (gc-common w10-f): `classData` now lives in the mirror's
    // own `java.lang.Class.classData` field. See [`attach_class_data`].
    //
    // NOT cleared here any more: `defining_loader_store` /
    // `orphaned_defining_loader_classes` are keyed by `(vm_identity,
    // class_id)`, so a fresh VM has no rows to clear and a blanket wipe would
    // destroy a CONCURRENTLY LIVE VM's. `forget_vm_loader_singletons` drops
    // this VM's rows at teardown instead. The key-set bits and the
    // `ANY_DEFINING_LOADER_REGISTERED` latch stay set for the same reason:
    // both are conservative "may have" signals, so a stale `true` only costs a
    // `Mutex` acquisition, while a wrongly-cleared one is a false negative.
    //
    // NOT cleared here any more either (gc-common w9-a):
    // `loader_namespace_id_store` and `loader_meta_store`. Both used to be,
    // on the grounds that a heap-address key carried into a fresh VM would
    // hand a brand-new loader an old one's namespace id and loader type once
    // an address was reused. That risk is real, but the wipe was the wrong
    // cure: a fresh VM has no rows, so the only rows it could reach were a
    // CONCURRENTLY LIVE VM's, which then minted a second namespace id for a
    // loader that already had one. Every row now carries its
    // `vm_identity`, lookups match it, and `forget_vm_loader_singletons`
    // drops a VM's rows at teardown — before its heap (and its addresses) can
    // be reused.
    //
    // NOT cleared here any more either, for exactly the reason just above —
    // these four were the same mistake, sixteen lines below the note that
    // explains it:
    //
    //   * `loader_pin` / `mirror_pin` / `metadata_pin` are the GC marker's
    //     liveness-pin registries. Every row now carries the `vm_identity` that
    //     wrote it, and `forget_vm_loader_singletons` drops this VM's rows at
    //     teardown. A fresh VM has none, so the only rows a wipe here could
    //     reach were a CONCURRENTLY LIVE VM's — and losing a pin is the
    //     dangerous direction: the marker drops a root for a loader that is
    //     still reachable.
    //   * `jit_activation` needs no wipe at all. A slot is owned by the thread
    //     running the compiled frame and cleared by that same thread's `exit`;
    //     a foreign wipe is the only way to lose a record whose frame is still
    //     running. A record stranded by a thread that died mid-frame
    //     over-retains one loader for one collection, and the reader
    //     (`vm::memory::roots`) already filters every id it finds through
    //     `defining_loader_for(vm_identity, ..)`, so another VM's class id
    //     cannot resolve to a root here.
    //
    // The URL-classpath cache is not cleared either. It holds no `ObjectRef`
    // (only `ClassPath`s keyed by path lists), so no VM's rows can go stale,
    // and it is bounded by `LOCAL_URL_CLASS_PATH_CACHE_LIMIT`. The cache that
    // used to be cleared here had no reader left (gc-common w9-a).
}

/// GC root scan for the singleton built-in class loaders.
///
/// The app + platform `ClassLoader` synthetics live ONLY in the
/// [`LOADER_SINGLETONS`] side-table (not a Java field or VM root table), so
/// they are invisible to the frame / static / heap-object root scans. Without
/// this, a moving young GC can reclaim or relocate the cached loader while
/// `get_or_create_app_loader` keeps returning the stale `ObjectRef`; the freed
/// slot is then reused by another allocation and a later
/// `loader.loadClass(...)` dispatches on the wrong object — observed as
/// BouncyCastle `ClassUtil.loadClass`'s receiver decaying to a String OID,
/// surfacing intermittently (heap-size dependent) as
/// "Not able to load any cryptoProvider". Mirrors the `lang_math` /
/// `lang_invoke` process-global cache root scans (`roots.rs` steps 15–17).
///
/// `vm_identity` scopes the scan to the collecting VM's own row: handing one
/// VM's loader address to another VM's collector as a root is exactly the
/// cross-heap confusion the keying exists to prevent.
pub fn gc_scan_loader_singleton_roots(vm_identity: usize, out: &mut Vec<ObjectRef>) {
    with_loader_singletons(|table| {
        if let Some(row) = table.get(&vm_identity) {
            out.extend(row.app);
            out.extend(row.platform);
            out.extend(row.app_code_source_domains.values().copied());
        }
    });
    // Defining-loader side-table values are live ClassLoader objects reachable
    // only from this map. Legacy behavior roots them all (which is why a
    // user/isolated loader could never be collected — HIB-CV-24 Manifestation B).
    // With `CRATONVM_LOADER_UNLOAD` ON (default) we DON'T root them, so a loader
    // the application no longer references becomes collectable; the now-stale
    // entry is pruned post-GC by `gc_reconcile_defining_loaders`. App/platform
    // singletons stay rooted above, so built-in loaders are unaffected.
    if !loader_unload_enabled() {
        for (_, o) in defining_loader_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rows
            .iter()
            .filter(|((vm, _), _)| *vm == vm_identity)
        {
            out.push(*o);
        }
    }
}

/// Post-GC remap for the singleton built-in class loaders (companion to
/// [`gc_scan_loader_singleton_roots`]). After a moving collection the cached
/// loader objects relocate; repoint the stored `ObjectRef`s to their new
/// addresses so subsequent `getClassLoader()` calls return the live object.
pub fn gc_update_loader_singleton_refs(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
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
    with_loader_singletons(|table| {
        if let Some(row) = table.get_mut(&vm_identity) {
            remap(&mut row.app);
            remap(&mut row.platform);
            for domain in row.app_code_source_domains.values_mut() {
                let mut slot = Some(*domain);
                remap(&mut slot);
                *domain = slot.unwrap_or(*domain);
            }
        }
    });
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
/// `exact_results`, when present, is a pre-slide mark-bitmap-exact verdict
/// (see `gc::zgc_reconcile_watch_map`/`VmHeap::zgc_set_reconcile_watch`) for
/// specific watched addresses, captured BEFORE relocation/compaction could
/// slide an unrelated survivor into a dead loader's just-vacated span. HIB-CV-24
/// (ZGC manifestation): `is_marked` alone, called post-collection, cannot
/// distinguish "this loader survived" from "compaction reused its old
/// address" on backends whose liveness predicate is a post-slide, pure
/// address-keyed registry lookup (ZGC's `is_addr_live`). Consulted first for
/// each row; `is_marked` remains the fallback for any address not in the map
/// (e.g. G1/Generational callers, which never populate one).
pub fn gc_reconcile_defining_loaders(
    vm_identity: usize,
    is_marked: &dyn Fn(usize) -> bool,
    pointer_map: &cratonvm_types::PointerMap,
    exact_results: Option<&std::collections::HashMap<usize, bool>>,
) -> Vec<u32> {
    let dbg = crate::vmflags().gc.dbg_mirrorpin;
    // Every table below is judged in three phases: read its addresses under
    // its lock, run `is_marked` with NO lock held, then apply the verdicts
    // under the lock again. gc-common w8-e moved the first two tables to this
    // form. Before that, `is_marked` (`pointer_map` plus `heap.is_addr_live`,
    // which takes heap-interior locks) ran inside the process-global
    // `defining_loader_store` mutex, once per row. That mutex was then held to
    // the END of this function, so the loader-meta pass's claim below to run
    // its predicate "with NO lock held" was not true either. Every other live
    // VM's `defining_loader_for` (an interpreter hot path once any user loader
    // exists) waited out this VM's whole reconcile.
    //
    // A row added between the phases is not in the dead set and is kept. The
    // pass only removes addresses it judged dead, so a race can delay a prune
    // by one collection and cannot drop a live row. Within one VM this runs
    // stop-the-world, and another VM's rows are never judged here.

    // gc-common w18-d: the non-strong hidden classes first. A class whose
    // MIRROR died is an unload hint whatever its loader's state; its
    // defining-loader row (if any) goes below exactly like a dead loader's.
    // A live one's `loader_pin` row keeps naming its (remapped) mirror.
    let (hidden_live, hidden_dead) =
        reconcile_non_strong_hidden(vm_identity, is_marked, pointer_map);

    // Phase 1: this VM's defining-loader rows.
    let ours: Vec<(u32, usize)> = defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rows
        .iter()
        .filter(|((vm, _), _)| *vm == vm_identity)
        .map(|(&(_, cid), obj_ref)| (cid, obj_ref.as_ptr() as usize))
        .collect();
    // Phase 2: verdicts, no lock held.
    let mut dead_rows: std::collections::HashSet<(u32, usize)> =
        std::collections::HashSet::new();
    for &(class_id, addr) in &ours {
        let alive = exact_results
            .and_then(|m| m.get(&addr).copied())
            .unwrap_or_else(|| is_marked(addr));
        if dbg {
            eprintln!(
                "[DBG_MIRRORPIN] defining_loader_store cid={class_id:?} loader_addr={addr:#x} is_marked={alive}"
            );
        }
        if !alive {
            dead_rows.insert((class_id, addr));
        }
    }
    drop(ours);
    // Phase 3: apply.
    let mut dead_class_ids: Vec<u32> = hidden_dead.iter().copied().collect();
    let mut map = defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !dead_rows.is_empty() || !hidden_dead.is_empty() {
        let mut orphans = orphaned_defining_loader_classes()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.rows.retain(|&(vm, class_id), obj_ref| {
            // Another VM's row: not ours to mark, remap or prune. Its own
            // collection will reconcile it, and this VM's relocation map says
            // nothing about an address in that VM's heap.
            let addr = obj_ref.as_ptr() as usize;
            if vm != vm_identity
                || !(dead_rows.contains(&(class_id, addr)) || hidden_dead.contains(&class_id))
            {
                return true;
            }
            // Loader unreachable and collected this cycle — drop the stale
            // entry. (No deref of `obj_ref`; the memory may already be
            // freed/reused.) Record the class as permanently orphaned: dropping
            // the entry alone makes `defining_loader_for` indistinguishable
            // from "never restricted", which would make this class incorrectly
            // visible to every other loader from now on (see
            // `is_defining_loader_orphaned` doc comment).
            orphans.insert((vm, class_id));
            dead_class_ids.push(class_id);
            false
        });
    }
    // Survivors: remap the ones that relocated (moving collection).
    if !pointer_map.is_empty() {
        for (&(vm, _), obj_ref) in map.rows.iter_mut() {
            if vm != vm_identity {
                continue;
            }
            if let Some(&new_addr) = pointer_map.get(&(obj_ref.as_ptr() as usize)) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
    // gc-common w10-f: the loader -> classes index follows the rows it
    // indexes, in this same critical section: a dead loader's entry goes, and
    // a moved loader's entry is re-keyed to its new address.
    if !dead_rows.is_empty() || !hidden_dead.is_empty() || !pointer_map.is_empty() {
        map.reindex_vm(vm_identity, pointer_map);
    }
    // The lock-free key-set mirror is keyed by `class_id` alone and is
    // deliberately CONSERVATIVE: another live VM may still hold a row for the
    // same id, and a false negative would make its `defining_loader_for`
    // answer `None` for a class that does have a defining loader. Clear a bit
    // only for an id no VM holds any more. One pass over the map builds the
    // held set. The per-id `map.keys().any(..)` scan this replaced was
    // O(pruned x rows) under the mutex, and a webapp redeploy prunes
    // thousands of ids at once.
    if !dead_class_ids.is_empty() {
        let still_held: std::collections::HashSet<u32> =
            map.rows.keys().map(|&(_, cid)| cid).collect();
        for &id in &dead_class_ids {
            if !still_held.contains(&id) {
                clear_defining_loader_bit(id);
            }
        }
    }
    // HIB-CV-24: re-sync the GC marker's loader-pin registry from the
    // authoritative side-table (now remapped/pruned) so the next collection
    // marks loaders at their current addresses and drops collected ones.
    //
    // gc-common w18-d: a live non-strong hidden class's row names its mirror
    // instead of its loader (see `register_non_strong_hidden_mirror`).
    let hidden_ids: std::collections::HashSet<u32> =
        hidden_live.iter().map(|&(cid, _)| cid).collect();
    let mut pins: Vec<(u32, usize)> = map
        .rows
        .iter()
        .filter(|((vm, cid), _)| *vm == vm_identity && !hidden_ids.contains(cid))
        .map(|(&(_vm, cid), obj_ref)| (cid, obj_ref.as_ptr() as usize))
        .collect();
    pins.extend(hidden_live.iter().copied());
    cratonvm_types::loader_pin::replace_loader_pins(vm_identity, &pins);
    drop(map);

    // Same treatment for the loader-namespace side-table (object-keyed): drop
    // entries whose loader was collected this cycle, remap survivors that
    // moved. A pruned dead loader's address can then be reused by a NEW
    // loader without inheriting the dead namespace. Same three phases.
    //
    // Only THIS VM's rows are collected, judged and remapped (gc-common w9-a).
    // This table and the loader-meta table below used to carry no VM, so this
    // VM's `is_marked` judged every other live VM's rows too — and the
    // production predicate answers "dead" for every address outside this
    // VM's heap, so each collection deleted every other VM's rows. Another
    // VM's rows are left untouched, exactly as the defining-loader pass above
    // leaves them.
    let ns_addrs: Vec<usize> = loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(vm, _, _)| *vm == vm_identity)
        .map(|(_, obj_ref, _)| obj_ref.as_ptr() as usize)
        .collect();
    let ns_dead: std::collections::HashSet<usize> =
        ns_addrs.into_iter().filter(|addr| !is_marked(*addr)).collect();
    loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain_mut(|(vm, obj_ref, _)| {
            if *vm != vm_identity {
                return true;
            }
            let old_addr = obj_ref.as_ptr() as usize;
            if ns_dead.contains(&old_addr) {
                return false;
            }
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
            true
        });

    // L1: identical treatment for the VM-internal loader bookkeeping table.
    // It holds `loader_type` / `classes_loaded` / `parallel_capable` /
    // `loader_id`, keyed by the loader object, for exactly the reason the
    // namespace store above is object-keyed: an identity-hash key recurs once
    // a collection reuses the address, and a brand-new loader would inherit a
    // dead one's namespace id. Prune the dead, remap the moved.
    //
    // Three phases so the survivor predicate runs with NO lock held. That is
    // not tidiness: `is_marked` is the GC's own
    // `pointer_map.contains_key(addr) || heap.is_addr_live(addr)`, and
    // `is_addr_live` takes heap-interior locks (`old_gen`, `young_from`).
    // Calling it inside the critical section would mean holding this
    // process-global side table across a heap lock — the loader_meta -> heap
    // edge this crate is least able to afford, since it re-enters the VM
    // everywhere. Those heap locks are raw today, so the ordering checker
    // cannot see the edge and would not have complained; hoisting the call is
    // what makes this lock's `LockLevel::Scratch` (L0, "acquires nothing")
    // literally true rather than true-by-the-checker's-blind-spot.
    //
    // Concurrency-safe beyond the STW window it actually runs in: an entry
    // added between phases is absent from `dead` and is therefore KEPT. The
    // pass only ever removes addresses it positively judged dead, so the
    // failure mode is one collection's delay in pruning, never dropping a live
    // loader's bookkeeping.
    //
    // Same VM filter as the namespace pass above (gc-common w9-a).
    let addrs: Vec<usize> = {
        let table = loader_meta_store().lock();
        table
            .iter()
            .filter(|(vm, _, _)| *vm == vm_identity)
            .map(|(_, o, _)| o.as_ptr() as usize)
            .collect()
    };
    let dead: std::collections::HashSet<usize> =
        addrs.into_iter().filter(|addr| !is_marked(*addr)).collect();
    loader_meta_store().lock().retain_mut(|(vm, obj_ref, _)| {
        if *vm != vm_identity {
            return true;
        }
        let old_addr = obj_ref.as_ptr() as usize;
        if dead.contains(&old_addr) {
            return false;
        }
        // `pointer_map` is a plain `HashMap` owned by the caller — no lock.
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
        true
    });

    // gc-common w11-e: the closed-`URLClassLoader` rows, same three phases
    // and same VM filter. A dead loader's row goes (it used to stay until
    // VM teardown), a moved one's address follows. The rows are keyed by
    // identity hash, which a move does not change, so only the value moves.
    // gc-common w29-e: a bucket per hash, one address per closed loader.
    let closed_addrs: Vec<usize> = closed_url_classloader_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|((vm, _), _)| *vm == vm_identity)
        .flat_map(|(_, rows)| rows.iter().copied())
        .collect();
    let closed_dead: std::collections::HashSet<usize> = closed_addrs
        .into_iter()
        .filter(|addr| !is_marked(*addr))
        .collect();
    closed_url_classloader_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|&(vm, _), rows| {
            if vm != vm_identity {
                return true;
            }
            rows.retain_mut(|addr| {
                let old_addr = *addr;
                if closed_dead.contains(&old_addr) {
                    return false;
                }
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    *addr = new_addr;
                }
                true
            });
            !rows.is_empty()
        });

    // (gc-common w29-e: the reflective `Constructor` side rows that rode this
    // reconcile since w21-d are keyed by weak lock keys now, and dropped by
    // the lock-key sweep through `lang_class::forget_lang_class_keys`.)

    dead_class_ids.sort_unstable();
    dead_class_ids.dedup();
    dead_class_ids
}

/// Drop temporary fail-closed orphan markers after the VM has tombstoned the
/// corresponding class metadata. Keeping them after a completed unload would
/// turn the safety set itself into an unbounded per-loader metadata leak.
///
/// `class_ids` must name only classes that are no longer in the store: a
/// class still loaded keeps its marker (gc-common w11-e;
/// `vm::memory::gc::unload_dead_class_metadata` passes exactly the hints
/// whose class is gone, on every exit).
pub fn forget_unloaded_classes(vm_identity: usize, class_ids: &[u32]) {
    if class_ids.is_empty() {
        return;
    }
    let ids: std::collections::HashSet<u32> = class_ids.iter().copied().collect();
    orphaned_defining_loader_classes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(vm, id)| *vm != vm_identity || !ids.contains(id));
    // gc-common w18-d: an unloaded non-strong hidden class's row (and its
    // mirror's `mirror_pin` exclusion) goes with it. Its `loader_pin` row is
    // removed just below with every other unloaded class's.
    forget_non_strong_hidden(vm_identity, &ids);
    for id in class_ids {
        cratonvm_types::loader_pin::remove_loader_pin(vm_identity, *id);
    }
}

/// JEP 371: store a class's `classData` in its mirror's
/// `java.lang.Class.classData` field, which is where the JDK reads it
/// (`MethodHandles.classData` -> `JavaLangAccess.classData` ->
/// `Class.getClassData()`, a plain field read), and where HotSpot writes it
/// (`java_lang_Class::set_class_data` in `jvm_lookup_define_class`).
///
/// gc-common w10-f: this replaces two write-only side tables, the
/// mirror-keyed one that stood here and a `ClassId`-keyed one in
/// `lookup_define.rs`. Neither had a reader, so `classData` never reached
/// the JDK: `Class.getClassData()` read `null` for every hidden class.
/// Both were process-wide with no VM key; neither rooted nor remapped its
/// values; the mirror-keyed one went stale on the first moving collection,
/// and the `ClassId`-keyed one was never evicted. A heap field has none of
/// those problems: the collector traces and moves it with the mirror.
///
/// On a mirror whose class has no `classData` field (the synthetic JDK), the
/// write is dropped, as `set_field_by_name` drops any absent field: that
/// build has no `MethodHandles.classData` reader either.
///
/// `mirror` and `data` must be current addresses: callers read `data` back
/// through a native pin after the define and the mirror allocation.
pub(crate) fn attach_class_data(ctx: &mut dyn NativeContext, mirror: ObjectRef, data: ObjectRef) {
    ctx.set_field_by_name(mirror, "classData", Value::Object(Some(data)));
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
///
/// Keyed by `(vm_identity, class_id)`. `ClassId`s are minted per VM from zero,
/// so an unqualified key made VM B's class 42 report VM A's `ClassLoader`
/// object — a live reference into a foreign heap, handed to
/// `annotation_container_loader` and to the interpreter's loader-initiated
/// resolution. It surfaced as an intermittent
/// `ClassCastException: ? cannot be cast to …` (that `?` is
/// `class_name_of_id` failing on a class id this VM has never heard of) across
/// the proxy/annotation corpus tests, in a parallel run only.
fn defining_loader_store() -> &'static Mutex<DefiningLoaderTable> {
    static INSTANCE: OnceLock<Mutex<DefiningLoaderTable>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(DefiningLoaderTable::default()))
}

/// This VM's current defining-loader row addresses — exactly the set
/// `gc_reconcile_defining_loaders` will judge on the next collection. A
/// caller arms `VmHeap::zgc_set_reconcile_watch` with this BEFORE
/// `collect_garbage` so ZGC can capture each address's exact pre-slide
/// verdict (see HIB-CV-24 note on `gc_reconcile_defining_loaders`).
pub fn watched_defining_loader_addrs(vm_identity: usize) -> Vec<usize> {
    let map = defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut addrs: Vec<usize> = map
        .rows
        .iter()
        .filter(|((vm, _), _)| *vm == vm_identity)
        .map(|(_, obj_ref)| obj_ref.as_ptr() as usize)
        .collect();
    addrs.sort_unstable();
    addrs.dedup();
    addrs
}

/// The defining-loader rows, plus a reverse index from each loader to the
/// classes it defined, kept under the ONE mutex so the two cannot disagree.
///
/// gc-common w10-f
/// (`docs/internal/gc-common-round-20260923/common-w9a-find-loaded-class-scans-every-vms-defining-loader-rows-FIXED-20260923.md`):
/// [`class_defined_by_this_loader_object`] used to answer "which classes did
/// THIS loader object define" by walking every row of every VM under this
/// mutex, and then reading the name of each of that loader's classes. It runs
/// on every user-loader `findLoadedClass` miss, and every define is preceded
/// by at least one, so a loader defining `n` classes cost `O(n x rows)` row
/// visits plus `O(n^2)` class-name reads, with the process-wide mutex held
/// for each walk. The index removes both terms: `by_loader` names the loader's
/// classes directly, and each entry memoizes the names it has already read.
#[derive(Default)]
struct DefiningLoaderTable {
    /// `(vm_identity, class_id) -> defining loader`. The authoritative map.
    rows: std::collections::HashMap<(usize, u32), ObjectRef>,
    /// `(vm_identity, loader address) -> the class ids whose row names that
    /// loader`. Keyed by the address as a `usize`, so it is not an
    /// `ObjectRef`-keyed table: [`gc_reconcile_defining_loaders`] re-keys it
    /// through the pointer map and drops a dead loader's entry in the same
    /// critical section that prunes and remaps `rows`.
    by_loader: std::collections::HashMap<(usize, usize), LoaderDefinedClasses>,
}

/// One loader's classes, in registration order, and the names read so far.
#[derive(Default)]
struct LoaderDefinedClasses {
    ids: Vec<u32>,
    /// Name -> class id for `ids[..named]`. The first id registered under a
    /// name wins, and a name is never re-read: a class id names one class for
    /// the life of its VM (`ClassStore::add` appends, and ids are not reused).
    by_name: std::collections::HashMap<Arc<str>, u32>,
    /// How many of `ids`, from the front, have their names in `by_name`.
    named: usize,
}

impl LoaderDefinedClasses {
    /// Keep the ids `keep` accepts. Removing an id whose name was already
    /// read forgets every memoized name, since `by_name` holds only the first
    /// id per name and cannot tell which later id it was hiding. The names are
    /// read again lazily. Removal is rare: it happens only when a class is
    /// re-registered to another loader, or when a remap leaves the row
    /// naming a different address.
    fn retain_ids(&mut self, keep: impl Fn(u32) -> bool) {
        let named = self.named;
        let mut index = 0usize;
        let mut dropped_a_named_id = false;
        self.ids.retain(|&cid| {
            let kept = keep(cid);
            if !kept && index < named {
                dropped_a_named_id = true;
            }
            index += 1;
            kept
        });
        if dropped_a_named_id {
            self.by_name.clear();
            self.named = 0;
        }
    }

    /// Record names read for the ids that followed `ids[..self.named]` when
    /// they were snapshotted. Publishes in order and stops at the first id
    /// that is no longer at the next position (the list changed between the
    /// read and this call); the rest are read again by a later probe.
    fn publish_names(&mut self, read: &[(u32, Option<Arc<str>>)]) {
        for (cid, name) in read {
            if self.ids.get(self.named) != Some(cid) {
                return;
            }
            if let Some(name) = name {
                self.by_name.entry(Arc::clone(name)).or_insert(*cid);
            }
            self.named += 1;
        }
    }
}

impl DefiningLoaderTable {
    /// Insert or replace `(vm, class_id)`'s row, keeping the index in step.
    fn insert(&mut self, vm: usize, class_id: u32, loader: ObjectRef) {
        let new_addr = loader.as_ptr() as usize;
        match self.rows.insert((vm, class_id), loader) {
            Some(old) if old.as_ptr() as usize == new_addr => return,
            Some(old) => {
                let old_key = (vm, old.as_ptr() as usize);
                if let Some(entry) = self.by_loader.get_mut(&old_key) {
                    entry.retain_ids(|cid| cid != class_id);
                    if entry.ids.is_empty() {
                        self.by_loader.remove(&old_key);
                    }
                }
            }
            None => {}
        }
        self.by_loader
            .entry((vm, new_addr))
            .or_default()
            .ids
            .push(class_id);
    }

    /// Drop every row and index entry of `vm`, returning the dropped ids.
    fn forget_vm(&mut self, vm_identity: usize) -> Vec<u32> {
        let mut dropped_ids: Vec<u32> = Vec::new();
        self.rows.retain(|&(vm, class_id), _| {
            if vm == vm_identity {
                dropped_ids.push(class_id);
                false
            } else {
                true
            }
        });
        self.by_loader.retain(|&(vm, _), _| vm != vm_identity);
        dropped_ids
    }

    /// Bring `vm_identity`'s index entries in line with its rows after the
    /// reconcile pruned and remapped them. Each entry is re-keyed through
    /// `pointer_map`, and keeps only the ids whose row still names the
    /// (possibly moved) loader, so a dead loader's entry empties and goes.
    /// Memoized names survive the move: they belong to class ids, not to
    /// addresses. Costs one row look-up per indexed id, the same order as the
    /// loader-pin re-sync that follows it.
    fn reindex_vm(&mut self, vm_identity: usize, pointer_map: &cratonvm_types::PointerMap) {
        let keys: Vec<(usize, usize)> = self
            .by_loader
            .keys()
            .filter(|&&(vm, _)| vm == vm_identity)
            .copied()
            .collect();
        let mut rekeyed: Vec<(usize, LoaderDefinedClasses)> = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(mut entry) = self.by_loader.remove(&key) else {
                continue;
            };
            let new_addr = pointer_map.get(&key.1).copied().unwrap_or(key.1);
            let rows = &self.rows;
            entry.retain_ids(|cid| {
                rows.get(&(vm_identity, cid))
                    .is_some_and(|loader| loader.as_ptr() as usize == new_addr)
            });
            if !entry.ids.is_empty() {
                rekeyed.push((new_addr, entry));
            }
        }
        for (addr, entry) in rekeyed {
            match self.by_loader.entry((vm_identity, addr)) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(entry);
                }
                // Two old addresses that now name one loader (not expected
                // from a well-formed pointer map). Merge, and let the names be
                // read again rather than reason about which memo is right.
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let merged = slot.get_mut();
                    for cid in entry.ids {
                        if !merged.ids.contains(&cid) {
                            merged.ids.push(cid);
                        }
                    }
                    merged.by_name.clear();
                    merged.named = 0;
                }
            }
        }
    }
}

/// Class-ids whose registered defining loader was confirmed DEAD by a prior
/// `gc_reconcile_defining_loaders` pass. Once a class lands here it must
/// never again be treated as globally visible: `defining_loader_for`
/// returning `None` is ALSO the answer for "never had a registered loader in
/// the first place" (the overwhelmingly common built-in-loader case), so
/// `cid_visible_mirror` cannot distinguish "no restriction" from "the
/// restriction's target died" without this separate permanent record.
/// An entry lives as long as the class does (matches real unloading: once a
/// defining loader is gone, the class is gone from every OTHER loader's
/// perspective forever; a new loader wanting the same simple name must define
/// its own copy). It is removed only when the class itself is: by
/// [`forget_unloaded_classes`] once the VM has tombstoned the class metadata,
/// and by [`forget_vm_loader_singletons`] at VM teardown. The one other way
/// out is a new defining-loader row for the same class
/// ([`register_defining_loader`], gc-common w11-e): the live row then carries
/// the restriction itself, and a marker left beside it would hide the class
/// from the very loader that adopted it.
///
/// Which classes keep a marker past the unload transaction: those still in
/// the store under a BUILT-IN namespace (the transaction unloads only user
/// namespaces). Before gc-common w11-e the transaction's two early exits
/// dropped their markers too, so such a class became visible to every loader
/// exactly when nothing else died in the same collection.
fn orphaned_defining_loader_classes() -> &'static Mutex<std::collections::HashSet<(usize, u32)>> {
    static INSTANCE: OnceLock<Mutex<std::collections::HashSet<(usize, u32)>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Whether `class_id`'s defining loader has been confirmed collected. See
/// [`orphaned_defining_loader_classes`]. `pub` so the VM's unload tests can
/// observe the marker (gc-common w11-e).
pub fn is_defining_loader_orphaned(vm: usize, class_id: u32) -> bool {
    orphaned_defining_loader_classes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&(vm, class_id))
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
/// that case. Correctness: it only ever transitions false→true (in
/// `register_defining_loader`) and is never reset, so a `false` read here is
/// always accurate at the instant it's read for a map that has never had an
/// insert — no ABA/staleness risk. (It used to be reset alongside the map by
/// `reset_loader_singletons`; the map is per VM now and that reset is gone.)
static ANY_DEFINING_LOADER_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Highest `class_id` the [`defining_loader_bits`] mirror can answer for. Ids
/// at or above it fall back to the map (correct, just not lock-free); 1M is
/// far past what any real workload reaches, so the fallback is dead code in
/// practice rather than a second hot path.
const DEFINING_LOADER_BITS_CAP: u32 = 1 << 20;

/// Lock-free per-`ClassId` mirror of [`defining_loader_store`]'s KEY SET.
///
/// [`ANY_DEFINING_LOADER_REGISTERED`] answers the same question for the whole
/// PROCESS, and that is precisely why it stops paying. It latches true the
/// instant any user-defined loader defines any class — which in a servlet
/// container, an OSGi runtime, or anything using ByteBuddy/cglib/Groovy
/// happens once, early, and then never goes back — after which every caller
/// takes the store's `Mutex` on every lookup, forever, for the bootstrap and
/// app-loader classes that are the overwhelming majority of all lookups.
///
/// Measured (2026-08-03, `probes/LoaderStepCostProbe.java`): BCEL-parsing 156
/// class files costs 448 ms on a fresh VM and **830 ms after a single class is
/// defined through a `URLClassLoader`** — a permanent 1.8x on work that has
/// nothing to do with that loader. `new` is the amplifier: it is resolved from
/// scratch on every execution (`opcodes.rs`'s `Instruction::New` →
/// `resolve_class_loader_aware`), and a constant-pool parse constructs one
/// object per entry. That is the Tomcat webapp-deploy wall's degradation term:
/// successive deploys in one JVM ran 104 s, 162 s, 260 s, 298 s.
///
/// Keyed by `class_id`, so a class the map has no entry for is answered
/// without touching the `Mutex` no matter how many other loaders exist.
///
/// **Maintenance — three writers, matching the map's own three:**
/// [`register_defining_loader`] sets a bit, [`gc_reconcile_defining_loaders`]
/// clears the bits of entries it retires, and [`forget_vm_loader_singletons`]
/// clears the bits of a torn-down VM's ids that no other VM still holds
/// (gc-common w9-a; the whole-set clear `reset_loader_singletons` once did
/// went when the map became per VM, and its helper was dead code since).
/// Each writes the bit while the map lock is held (or, for
/// registration, after the insert), so a reader that observes a bit SET and
/// then takes the lock either finds the entry or finds it concurrently
/// removed — both already-legal outcomes. A reader that observes a bit CLEAR
/// returns `None`, which is the same race the process-wide latch already had
/// against a concurrent first registration.
///
/// Allocated lazily (128 KiB) on the first registration, so a run that never
/// uses a custom loader never pays for it.
fn defining_loader_bits() -> &'static [AtomicU64] {
    static INSTANCE: OnceLock<Box<[AtomicU64]>> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        (0..(DEFINING_LOADER_BITS_CAP as usize / 64))
            .map(|_| AtomicU64::new(0))
            .collect()
    })
}

/// Could `class_id` have a registered defining loader? `false` is exact — the
/// caller may skip the map entirely. `true` means "consult the map", which may
/// still answer `None`. Lock-free; public for the interpreter's cast-site fill
/// screen, which must not take the map's mutex per execution.
#[inline]
pub fn class_may_have_defining_loader(class_id: u32) -> bool {
    if !ANY_DEFINING_LOADER_REGISTERED.load(Ordering::Acquire) {
        return false;
    }
    if class_id >= DEFINING_LOADER_BITS_CAP {
        return true;
    }
    let word = &defining_loader_bits()[(class_id / 64) as usize];
    word.load(Ordering::Acquire) & (1u64 << (class_id % 64)) != 0
}

fn set_defining_loader_bit(class_id: u32) {
    if class_id >= DEFINING_LOADER_BITS_CAP {
        return;
    }
    defining_loader_bits()[(class_id / 64) as usize]
        .fetch_or(1u64 << (class_id % 64), Ordering::Release);
}

fn clear_defining_loader_bit(class_id: u32) {
    if class_id >= DEFINING_LOADER_BITS_CAP {
        return;
    }
    defining_loader_bits()[(class_id / 64) as usize]
        .fetch_and(!(1u64 << (class_id % 64)), Ordering::Release);
}

/// Record the user-defined `ClassLoader` object that defined `class_id`, so
/// `Class.getClassLoader()` returns the exact instance instead of the app-loader
/// fallback.
pub fn register_defining_loader(vm: usize, class_id: u32, loader: ObjectRef) {
    defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(vm, class_id, loader);
    // Bit AFTER the insert, latch after the bit: a reader that sees either
    // signal set then finds the entry already in the map.
    set_defining_loader_bit(class_id);
    ANY_DEFINING_LOADER_REGISTERED.store(true, Ordering::Release);
    // gc-common w11-e: a class whose previous defining loader died, and which
    // a live loader now adopts (a built-in-namespace class stays in the store
    // after its loader dies), is restricted by this new row from here on. Its
    // orphan marker would otherwise go on hiding it from every loader,
    // including this one (`cid_visible_mirror` checks the marker first). Not
    // nested in the store lock above: the reconcile takes store -> orphans,
    // and this takes orphans alone.
    orphaned_defining_loader_classes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(vm, class_id));
    // HIB-CV-24: mirror into the loader-pin registry the GC marker consults so a
    // live instance of this class keeps its defining loader alive (the
    // instance→loader edge HotSpot gets for free via `Class.getClassLoader`).
    //
    // gc-common w18-d: except for a non-strong hidden class, whose instances
    // must keep its MIRROR alive (the mirror keeps the loader through its
    // `classLoader` field). A re-registration of such a class must not turn
    // its edge back into instance -> loader, or its mirror (no longer rooted
    // and no longer pinned to the loader) could die under a live instance.
    let pin_target = non_strong_hidden_mirror(vm, class_id).unwrap_or(loader.as_ptr() as usize);
    cratonvm_types::loader_pin::set_loader_pin(vm, class_id, pin_target);
}

// ---------------------------------------------------------------------------
// Non-strong hidden classes (JEP 371) -- gc-common w18-d.
//
// A hidden class defined WITHOUT `ClassOption.STRONG` (`defineClass0`'s
// `STRONG_LOADER_LINK` bit clear) is unloaded by HotSpot as soon as the class
// itself is unreachable -- no mirror reference, no instance -- even while its
// defining loader lives. CratonVM unloaded a class only with its defining
// loader, so such a class of an application or platform loader never unloaded
// (`docs/internal/gc-common-round-20260923/common-w8e-non-strong-hidden-classes-unload-only-with-their-loader-FIXED-20260923.md`).
//
// The existing unload machinery already does everything needed once the
// class's liveness is its MIRROR's, not its loader's:
//
// * `loader_pin` is the collectors' instance -> "owner" edge, and it is keyed
//   by class id and followed by address, whatever the owner is. Pointing a
//   non-strong hidden class's row at its mirror makes every live instance mark
//   the mirror, and the mirror's `classLoader` field marks the loader. No
//   collector change.
// * Every loader-conditional root deferral (statics, class locks, condy
//   values, `ClassValue` results, reflection slots ...) asks
//   `vm_loader_pin_addr` for the owner to pin the value to, so those values now
//   hang off the mirror, as HotSpot keeps a class's statics in its mirror.
// * `vm::memory::roots` step 6 leaves such a mirror out of the root set, and
//   `mirror_pin` never pins it to its loader (`mirror_pin::exclude_mirror`).
// * `gc_reconcile_defining_loaders` judges the mirror with the collection's
//   own survivor predicate and reports a dead one as an unload hint.
// ---------------------------------------------------------------------------

/// `(vm_identity, class_id) -> the class's mirror address`, for every live
/// non-strong hidden class. Keyed by `(usize, u32)` and holding a plain
/// address, so it is not an `ObjectRef`-keyed table: the post-GC reconcile
/// prunes a row whose mirror died and remaps a moved one, in the same pass
/// that prunes and remaps the defining-loader rows.
///
/// LEVEL: `Scratch` (L0). Every critical section is an insert, a lookup, a
/// copy-out or a `retain` over plain values; none calls `ctx`, runs the GC's
/// survivor predicate, or takes another lock (the `loader_pin` / `mirror_pin`
/// writes are made after the guard drops).
fn non_strong_hidden_store() -> &'static OrderedPlMutex<std::collections::HashMap<(usize, u32), usize>>
{
    static INSTANCE: OnceLock<OrderedPlMutex<std::collections::HashMap<(usize, u32), usize>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| {
        OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch)
    })
}

/// Latch: has any VM ever registered a non-strong hidden class? Lets every
/// reader skip the lock in the common run that never does. Only ever goes
/// false -> true.
static ANY_NON_STRONG_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Register `class_id` (just defined as hidden, without `STRONG`) so it unloads
/// with its mirror rather than with its loader. Returns whether it did.
///
/// Call it when the define native is done with the class and about to return:
/// until then the class is an ordinary one (its mirror rooted, or pinned to its
/// loader), which is the conservative direction, and nothing the native still
/// allocates can collect the mirror under it.
///
/// Declines (the class stays loader-lifetime) when
/// * `CRATONVM_LOADER_UNLOAD=0`: nothing unloads in that mode, and the
///   defining-loader rows stay GC roots;
/// * the class has a defining loader and its mirror cannot carry the heap
///   edge to it (a layout without `Class.classLoader`). The instance -> loader
///   edge would otherwise be lost: instances would keep only the mirror, and
///   nothing on the mirror would keep the loader;
/// * the context has no VM (`vm_identity() == 0`, the trait default; live VMs
///   are numbered from 1). Every such context would share one set of per-VM
///   rows, which no collection ever reconciles.
pub fn register_non_strong_hidden_class(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    let vm = ctx.vm_identity();
    if vm == 0 || !loader_unload_enabled() {
        return false;
    }
    // Cached by the define path already: no allocation, so `mirror` is
    // current until this function returns.
    let mirror = ctx.get_class_mirror(class_id);
    let loader = defining_loader_for(vm, class_id.as_u32());
    if let Some(loader) = loader {
        if !mirror_names_loader(ctx, mirror, loader) {
            // `define_class_via_full` creates the mirror BEFORE it records
            // the defining loader, so `get_or_create_class_mirror` found no
            // loader to write. Write it now, as the `lang_system`
            // `defineClass0` backend does for every class it defines.
            ctx.set_field_by_name(mirror, "classLoader", Value::Object(Some(loader)));
            if !mirror_names_loader(ctx, mirror, loader) {
                return false;
            }
        }
    }
    register_non_strong_hidden_mirror(
        vm,
        class_id.as_u32(),
        mirror.as_ptr() as usize,
        loader.map(|l| l.as_ptr() as usize),
    );
    true
}

/// Does `mirror`'s `Class.classLoader` field name `loader`? The heap edge a
/// non-strong hidden class's mirror must carry (see
/// [`register_non_strong_hidden_class`]).
fn mirror_names_loader(ctx: &mut dyn NativeContext, mirror: ObjectRef, loader: ObjectRef) -> bool {
    matches!(
        ctx.get_field_by_name(mirror, "classLoader"),
        Value::Object(Some(l)) if l.as_ptr() == loader.as_ptr()
    )
}

/// The bookkeeping half of [`register_non_strong_hidden_class`], on
/// addresses: record the row, point the class's `loader_pin` row at its mirror
/// (a live instance now keeps the mirror), and exclude the mirror from
/// `mirror_pin` (a live loader no longer keeps it). `loader_addr` is the
/// class's defining loader, when it has one. `pub` for the VM's root-scan
/// tests.
pub fn register_non_strong_hidden_mirror(
    vm: usize,
    class_id: u32,
    mirror_addr: usize,
    loader_addr: Option<usize>,
) {
    non_strong_hidden_store()
        .lock()
        .insert((vm, class_id), mirror_addr);
    ANY_NON_STRONG_HIDDEN.store(true, Ordering::Release);
    cratonvm_types::loader_pin::set_loader_pin(vm, class_id, mirror_addr);
    cratonvm_types::mirror_pin::exclude_mirror(vm, mirror_addr, loader_addr);
}

/// Whether any VM in this process ever registered a non-strong hidden class
/// (one atomic load; only ever goes false -> true). gce e1/j: lets a per-frame
/// caller (`vm::memory::roots::frame_class_owners`) skip its walk entirely.
pub fn any_non_strong_hidden() -> bool {
    ANY_NON_STRONG_HIDDEN.load(Ordering::Acquire)
}

/// The mirror address of `vm`'s non-strong hidden class `class_id`, or `None`
/// for every other class (one relaxed load when no VM ever defined one).
pub fn non_strong_hidden_mirror(vm: usize, class_id: u32) -> Option<usize> {
    if !ANY_NON_STRONG_HIDDEN.load(Ordering::Acquire) {
        return None;
    }
    non_strong_hidden_store()
        .lock()
        .get(&(vm, class_id))
        .copied()
}

/// Every `(class_id, mirror address)` of `vm`'s live non-strong hidden
/// classes, for a root scan that asks once per collection rather than once
/// per mirror or frame. Empty (no lock) when no VM ever defined one.
pub fn non_strong_hidden_mirrors(vm: usize) -> std::collections::HashMap<u32, usize> {
    if !ANY_NON_STRONG_HIDDEN.load(Ordering::Acquire) {
        return std::collections::HashMap::new();
    }
    non_strong_hidden_store()
        .lock()
        .iter()
        .filter(|((owner, _), _)| *owner == vm)
        .map(|(&(_, cid), &mirror)| (cid, mirror))
        .collect()
}

/// Post-GC half for the non-strong hidden classes, run first by
/// [`gc_reconcile_defining_loaders`]: judge each of `vm`'s mirrors with the
/// cycle's survivor predicate (no lock held), drop the rows whose mirror died
/// and remap the moved ones. Returns the live rows at their current addresses
/// and the ids of the dead ones, which are unload hints whatever their
/// loader's state. Re-publishes the `mirror_pin` exclusions at the current
/// addresses, ahead of `vm::memory::gc::rebuild_mirror_pins` in the same
/// epilogue.
fn reconcile_non_strong_hidden(
    vm: usize,
    is_marked: &dyn Fn(usize) -> bool,
    pointer_map: &cratonvm_types::PointerMap,
) -> (Vec<(u32, usize)>, std::collections::HashSet<u32>) {
    let mut live: Vec<(u32, usize)> = Vec::new();
    let mut dead_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    if !ANY_NON_STRONG_HIDDEN.load(Ordering::Acquire) {
        return (live, dead_ids);
    }
    // Phase 1: this VM's rows.
    let ours: Vec<(u32, usize)> = non_strong_hidden_store()
        .lock()
        .iter()
        .filter(|((owner, _), _)| *owner == vm)
        .map(|(&(_, cid), &mirror)| (cid, mirror))
        .collect();
    if ours.is_empty() {
        return (live, dead_ids);
    }
    // Phase 2: verdicts, no lock held (`is_marked` takes heap locks).
    let dead: std::collections::HashSet<(u32, usize)> = ours
        .into_iter()
        .filter(|&(_, mirror)| !is_marked(mirror))
        .collect();
    // Phase 3: apply. A row registered between the phases is not in `dead`,
    // so it is kept (the same rule as the defining-loader pass).
    non_strong_hidden_store().lock().retain(|&(owner, cid), mirror| {
        if owner != vm {
            return true;
        }
        if dead.contains(&(cid, *mirror)) {
            dead_ids.insert(cid);
            return false;
        }
        if let Some(&new_addr) = pointer_map.get(mirror) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *mirror = new_addr;
        }
        live.push((cid, *mirror));
        true
    });
    let addrs: Vec<usize> = live.iter().map(|&(_, mirror)| mirror).collect();
    cratonvm_types::mirror_pin::replace_excluded_mirrors(vm, &addrs);
    (live, dead_ids)
}

/// Drop `vm`'s non-strong hidden rows for `class_ids` (classes no longer in
/// the store) and re-publish the remaining exclusions.
fn forget_non_strong_hidden(vm: usize, class_ids: &std::collections::HashSet<u32>) {
    if !ANY_NON_STRONG_HIDDEN.load(Ordering::Acquire) {
        return;
    }
    let remaining: Vec<usize> = {
        let mut store = non_strong_hidden_store().lock();
        store.retain(|&(owner, cid), _| owner != vm || !class_ids.contains(&cid));
        store
            .iter()
            .filter(|((owner, _), _)| *owner == vm)
            .map(|(_, &mirror)| mirror)
            .collect()
    };
    cratonvm_types::mirror_pin::replace_excluded_mirrors(vm, &remaining);
}

/// Look up the user-defined `ClassLoader` object that defined `class_id`.
pub fn defining_loader_for(vm: usize, class_id: u32) -> Option<ObjectRef> {
    if !class_may_have_defining_loader(class_id) {
        return None;
    }
    defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rows
        .get(&(vm, class_id))
        .copied()
}

/// Whether ANY class in this process has ever been defined by a
/// user-defined `ClassLoader` (i.e. `register_defining_loader` has been
/// called at least once). A single lock-free atomic load — safe for
/// interpreter hot paths that need to bail out before taking any
/// classloader-related lock at all. Since `register_defining_loader` is
/// called unconditionally whenever a class is assigned a
/// `ClassLoaderId::UserDefined(_)` identity (see the invariant documented
/// on `ANY_DEFINING_LOADER_REGISTERED` above), `false` here guarantees no
/// `ClassId` anywhere in `class_manager` currently has a `UserDefined`
/// loader id — so callers that only care about "is loader-initiated
/// resolution even possibly relevant" can skip a `class_manager` read lock
/// entirely in that (overwhelmingly common) case.
pub fn any_defining_loader_registered() -> bool {
    ANY_DEFINING_LOADER_REGISTERED.load(Ordering::Acquire)
}

/// Get or create the singleton platform class loader.
pub(crate) fn get_or_create_platform_loader(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    if let Some(obj) = platform_loader_of(vm) {
        return Ok(obj);
    }
    let mut obj = alloc_classloader(ctx, LOADER_PLATFORM)?;
    let obj_pin = ctx.pin_native_root(obj);
    let name = ctx.create_string("platform");
    let name_pin = ctx.pin_native_root(name);
    obj = ctx.read_native_pin(obj_pin, obj);
    let name = ctx.read_native_pin(name_pin, name);
    // L1: only on OUR layout. Slot 2 is `unnamedModule` on the real
    // `java.lang.ClassLoader`, NOT a second copy of `name` — a reference for a
    // reference, which is why the overlay detector never flagged it. Writing
    // the "platform" String there made `getUnnamedModule()` answer a String,
    // and (via `classloader_parent`'s slot-1 fallback) made the platform
    // loader's PARENT a String as well.
    if cl_has_synthetic_layout(ctx, obj) {
        ctx.set_field(obj, CL_NAME_REF, Value::Object(Some(name)));
    }
    // Also populate the REAL `name` field by name: the active getName native
    // (classloader_real) reads the real field slot, not CL_NAME_REF.
    obj = ctx.read_native_pin(obj_pin, obj);
    let name = ctx.read_native_pin(name_pin, name);
    ctx.set_field_by_name(obj, "name", Value::Object(Some(name)));
    // Platform's parent is bootstrap (null); already set by alloc_classloader
    obj = ctx.read_native_pin(obj_pin, obj);
    set_platform_loader(vm, Some(obj));
    ctx.unpin_native_roots(obj_pin);
    Ok(obj)
}

/// Mirror of real HotSpot's `JVM_LatestUserDefinedLoader` / `jdk.internal
/// .misc.VM.latestUserDefinedLoader()`: walk the Java call stack innermost
/// frame first and return the `ClassId` of the first frame whose class was
/// NOT loaded by the bootstrap or platform/extension loader (i.e. loader id
/// `>= 2`; see `NativeContext::loader_id_of_class`). Returns `None` if every
/// frame on the stack is bootstrap/platform (e.g. `main` itself, or a stack
/// walk with no user code visible).
///
/// Uses `NativeContext::frame_class_ids` (each frame's own already-resolved
/// `ClassId`), NOT a name-based re-resolution of `capture_stack_trace`'s
/// display `StackTraceEntry`s — re-resolving by name collapses to whichever
/// definition the global class table associates with that name (typically
/// the first one ever registered in the process), which silently picks the
/// WRONG class whenever the same name has been loaded more than once by
/// different loaders (exactly what happens running more than one
/// `@BytecodeEnhanced` Hibernate test class in a single process: each test
/// class execution gets its own fresh `EnhancingClassLoader`; once a second
/// test has run, `class_id_by_name("...TheFirstTestClass")` would still
/// resolve, but for a DIFFERENT class than the one actually executing on
/// that frame right now).
///
/// Shared by the real `VM.latestUserDefinedLoader0()` native (`lib.rs`) and
/// `serialization.rs`'s synthetic deserialization read-path
/// (`ois_read_object`), which never goes through
/// `ObjectInputStream.resolveClass()` and so has no other way to learn which
/// classloader a deserializing caller actually expects — without this,
/// `ois_read_object` resolved every stream class name via the
/// loader-oblivious `ensure_class_initialized`, silently materializing the
/// WRONG (e.g. non-bytecode-enhanced) class whenever a custom classloader
/// (like Hibernate's `EnhancingClassLoader`) defined the class actually
/// referenced by the code doing the deserializing.
///
/// LIVES HERE, NOT IN `serialization.rs`, ON PURPOSE. That module is
/// `#[cfg(any(feature = "experimental-serialization", feature =
/// "synthetic-jdk"))]` and BOTH features are default-off for `cratonvm-vm`
/// and `cratonvm-cli`. The `jdk/internal/misc/VM.latestUserDefinedLoader0()`
/// registration that calls this is reached by ordinary real-JDK bytecode on
/// EVERY `ObjectInputStream.readObject()` of a non-proxy class, so it has to
/// compile into the default build. While the helper lived in the gated
/// module the registration had to be gated with it, which silently reverted
/// the 2026-07-06 fix to an `UnsatisfiedLinkError` for every plain
/// `cargo build --release -p cratonvm-cli` (H2 `TestPreparedStatement`,
/// `TestObjectDataType`, `TestSampleApps`). Do not move it back.
pub(crate) fn latest_user_defined_loader_class(
    ctx: &mut dyn NativeContext,
) -> Option<cratonvm_types::ClassId> {
    ctx.frame_class_ids()
        .into_iter()
        .find(|&class_id| ctx.loader_id_of_class(class_id) >= 2)
}

/// Get or create the singleton application (system) class loader.
pub fn get_or_create_app_loader(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    if let Some(obj) = app_loader_of(vm) {
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
            return Ok(obj);
        }
        set_app_loader(vm, None);
    }
    let platform = get_or_create_platform_loader(ctx)?;
    let platform_pin = ctx.pin_native_root(platform);
    let mut obj = alloc_classloader(ctx, LOADER_APP)?;
    let obj_pin = ctx.pin_native_root(obj);
    let name = ctx.create_string("app");
    let name_pin = ctx.pin_native_root(name);
    let mut platform = ctx.read_native_pin(platform_pin, platform);
    obj = ctx.read_native_pin(obj_pin, obj);
    let mut name = ctx.read_native_pin(name_pin, name);
    // L1: only on OUR layout — see the same guard in
    // `get_or_create_platform_loader`. On the real layout slots 2 and 1 are
    // `unnamedModule` and `name`, so these two writes put the "app" String in
    // `unnamedModule` and the platform LOADER in `name`. The by-name writes
    // below are the correct path there and already run unconditionally.
    let synthetic_layout = cl_has_synthetic_layout(ctx, obj);
    if synthetic_layout {
        ctx.set_field(obj, CL_NAME_REF, Value::Object(Some(name)));
    }
    obj = ctx.read_native_pin(obj_pin, obj);
    platform = ctx.read_native_pin(platform_pin, platform);
    if synthetic_layout {
        ctx.set_field(obj, CL_PARENT_REF, Value::Object(Some(platform)));
    }
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
    set_both_parent_fields(ctx, obj, platform);
    // Populate the REAL static `java.lang.ClassLoader.scl` so the real-JDK
    // `ClassLoader.getSystemClassLoader()` bytecode (reached when a call site
    // does not resolve to our native; observed in
    // WebappClassLoaderBase.<init> at pc=174) returns this loader instead of
    // null. A null there made the subsequent `j.getParent()` NPE and aborted
    // every embedded-server webapp deploy ("Error starting the loader").
    obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_static_field_by_name("java/lang/ClassLoader", "scl", Value::Object(Some(obj)));
    obj = ctx.read_native_pin(obj_pin, obj);
    set_app_loader(vm, Some(obj));
    ctx.unpin_native_roots(platform_pin);
    Ok(obj)
}

/// Write the parent link into BOTH `parent` fields a built-in loader carries.
///
/// # There are two of them, and they are different fields
///
/// ```text
///   java/lang/ClassLoader              private final ClassLoader        parent
///   jdk/internal/loader/BuiltinClassLoader
///                                      private final BuiltinClassLoader parent
/// ```
///
/// A field is identified by its NAME AND DESCRIPTOR, and these two differ in
/// both descriptor and declaring class. `set_field_by_name` resolves from the
/// object's own class upwards, so on an `AppClassLoader` it finds
/// `BuiltinClassLoader`'s and stops — leaving `java.lang.ClassLoader.parent`
/// null forever.
///
/// That was invisible for as long as every reader was one of ours: the
/// `getParent()` native resolves by name too, so it read the field that HAD
/// been written and answered the platform loader. Real JDK bytecode does not:
/// `ClassLoader.getPackage` is `getfield #108 // Field parent:Ljava/lang/ClassLoader;`,
/// read the null, and took the `parent == null` branch to
/// `BootLoader.getDefinedPackage` — so `Package.getPackage("java.sql")` walked
/// past the platform loader that defines it and answered null, while
/// `platform.getPackage("java.sql")` called directly answered correctly. The
/// same field is read directly by `ClassLoader.loadClass`'s delegation and by
/// `checkClassLoaderPermission`; this is not a `getPackage` quirk.
///
/// MEASURED with `Field.set(app, platform)` from Java: one write, and
/// `app.getPackage("java.sql")` goes from `null` to `package java.sql` in the
/// same run.
///
/// Only the built-in loaders need this. `URLClassLoader` and every ordinary
/// user subclass inherit `ClassLoader.parent` with nothing shadowing it, so
/// the by-name write there already lands on the field the JDK reads.
fn set_both_parent_fields(ctx: &mut dyn NativeContext, loader: ObjectRef, parent: ObjectRef) {
    // The derived one first, by name: this is the write that was already here,
    // and the `getParent()`/`getName()` natives resolve the same way.
    ctx.set_field_by_name(loader, "parent", Value::Object(Some(parent)));
    // Then `java.lang.ClassLoader`'s own, addressed through the DECLARING
    // class so the shadow cannot capture it. `resolve_field_index` starts its
    // walk at the class it is given, and superclass fields keep their indices
    // in a subclass instance.
    if let Some(index) = ctx.resolve_field_index("java/lang/ClassLoader", "parent") {
        ctx.set_field(loader, index, Value::Object(Some(parent)));
    }
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
/// 1 = this loader is parallel-capable (the CratonVM default — see
/// `cl_is_registered_as_parallel_capable`), 0 = not. Must stay in agreement
/// with `registerAsParallelCapable()`, which always reports success.
const CL_IS_PARALLEL_CAPABLE: usize = 4;
const CL_DEFAULT_DOMAIN: usize = 5;
/// Unique loader ID for class namespace isolation (0 = not yet assigned)
const CL_LOADER_ID: usize = 6;
const CL_FIELD_COUNT: usize = 7;

// ---------------------------------------------------------------------------
// L1 — VM-internal loader bookkeeping lives beside the object, not in it
// ---------------------------------------------------------------------------
//
// Four of the seven synthetic slots above hold values the real JDK has no
// field for at all: `CL_LOADER_TYPE`, `CL_CLASSES_LOADED`,
// `CL_IS_PARALLEL_CAPABLE` and `CL_LOADER_ID` are CratonVM bookkeeping. On a
// real JDK image the loader object has the REAL layout — `parent`(0)
// `name`(1) `unnamedModule`(2) `nameAndId`(3) `parallelLockMap`(4)
// `package2certs`(5) `classes`(6) — so all four of our `Value::Int` writes
// land on reference fields. `Heap::set_field_as` coerces the `Int` to
// `Object(None)`, and the JDK's own field is destroyed. Measured 2026-08-04
// with `CRATONVM_DBG=overlay,overlay-all`: eight rows, four slots on each of
// `ClassLoaders$AppClassLoader` and `$PlatformClassLoader`, the writer named
// by `overlay-bt` as `alloc_classloader`.
//
// This is the third kind of layout defect and neither earlier fix applies:
// `VarHandle` (kind 1) had synthetic slots to keep on the synthetic layout,
// `Properties` (kind 2) had a real field we were indexing against the wrong
// class. Here `resolve_field_index_by_class_id` returns `None` because there
// IS no real field — so the value has to leave the object.
//
// Shape copied from `lang_invoke::vh_meta_put`/`vh_meta_get`, with two
// deliberate departures, both forced by facts about loaders specifically:
//
//  * **Keyed by the loader OBJECT, not by `identity_hash_code`.** Identity
//    hashes are address-derived and recur once a collection reuses the
//    region, so a fresh loader can inherit a dead one's entry — including its
//    namespace id, which is exactly the bug that made
//    `loader_namespace_id_store` move off identity hashes (see its doc
//    comment). Object keys are pruned and remapped by
//    [`gc_reconcile_defining_loaders`], alongside that store.
//  * **NOT a GC root.** `vh_meta_put` calls `register_var_handle_root`
//    because a VarHandle held only by a `static final` field has no other
//    root. A loader does not have that problem: the app/platform singletons
//    are already rooted by [`gc_scan_loader_singleton_roots`], and rooting
//    user loaders here would pin every one of them forever and defeat
//    loader unloading (`CRATONVM_LOADER_UNLOAD`, HIB-CV-24 Manifestation B).
//
//  * **Every member is an `Option`.** The design sketch had bare
//    `i32`/`bool`/`u32`, but each of the nine converted read sites has its
//    OWN default for a missing value (`LOADER_APP` in `cl_load_class`,
//    `LOADER_CUSTOM` in `cl_get_name`, `1` in
//    `cl_is_registered_as_parallel_capable`, "unassigned" for the id). A
//    single struct-wide default would silently change all of them; `Option`
//    keeps each caller's own fallback where it already is.

/// CratonVM's per-loader bookkeeping, held beside the loader object.
///
/// `None` means "this VM has never recorded that value for this loader",
/// which is NOT the same as any particular default — see the module comment
/// above.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LoaderMeta {
    /// `LOADER_BOOTSTRAP` / `LOADER_PLATFORM` / `LOADER_APP` / `LOADER_CUSTOM`.
    loader_type: Option<i32>,
    /// Count of classes defined through this loader's `defineClass` natives.
    classes_loaded: Option<i32>,
    /// `registerAsParallelCapable()` bookkeeping.
    parallel_capable: Option<bool>,
    /// CratonVM class-namespace id (`0` = the shared application namespace).
    loader_id: Option<u32>,
}

impl LoaderMeta {
    /// What the three `ClassLoader` constructor natives record: a custom
    /// loader that has defined nothing yet and is parallel-capable, with a
    /// namespace id only for the `(String, ClassLoader)` form (the other two
    /// leave the id unassigned until `loader_namespace_id` needs one).
    fn custom_initialiser(loader_id: Option<u32>) -> Self {
        LoaderMeta {
            loader_type: Some(LOADER_CUSTOM),
            classes_loaded: Some(0),
            parallel_capable: Some(true),
            loader_id,
        }
    }
}

/// Loader-object → [`LoaderMeta`], one row per `(vm_identity, loader)`. A
/// `Vec` rather than a map for the same reason `loader_namespace_id_store` is
/// one: entries are keyed by a raw heap address that the GC rewrites, and
/// `retain_mut` over a `Vec` remaps in place where a hash map would have to be
/// rebuilt. Loader counts are in the tens even for a servlet container.
///
/// The first field is the owning VM (gc-common w9-a). A loader address means
/// something only in the heap that allocated it: every lookup matches the
/// caller's `ctx.vm_identity()` as well as the address, the GC pass judges and
/// remaps only the collecting VM's rows, and `forget_vm_loader_singletons`
/// drops a VM's rows at teardown.
fn loader_meta_store() -> &'static OrderedPlMutex<Vec<(usize, ObjectRef, LoaderMeta)>> {
    // LEVEL (lock-discipline ratchet): `Scratch` is L0, the bottom of the
    // hierarchy — a thread holding it may acquire NOTHING else. That is a
    // claim about every critical section, and each was checked against it:
    // `retain` (VM teardown), `find`/`push` (`loader_meta_put`), `find`/copy
    // (`loader_meta_get`), one caller-supplied closure whose only caller
    // assigns a field (`loader_meta_upsert`), the test helper's `retain`, and
    // the GC pass's `retain_mut` — which reads `dead` and `pointer_map`, both
    // plain collections. None re-enters the VM; none takes another lock.
    //
    // The GC pass had to be restructured for that last clause to be true: it
    // used to call the collector's `is_marked` from inside the critical
    // section, and that reaches `heap.is_addr_live`, which takes the
    // `old_gen` / `young_from` locks. See `gc_reconcile_defining_loaders`.
    //
    // Why the bottom and not some other free level: this crate re-enters the
    // VM constantly (a native callback calls back into Java, taking the heap
    // and the L10 class-manager lock), so anything held across that re-entry
    // is a cycle. L0 says this one never is, and makes a future violation a
    // checker failure instead of a hang.
    static INSTANCE: OrderedPlMutex<Vec<(usize, ObjectRef, LoaderMeta)>> =
        OrderedPlMutex::new(Vec::new(), LockLevel::Scratch);
    &INSTANCE
}

/// Record (or replace) `loader`'s bookkeeping in VM `vm`.
pub(crate) fn loader_meta_put(vm: usize, loader: ObjectRef, meta: LoaderMeta) {
    let mut t = loader_meta_store().lock();
    match t
        .iter_mut()
        .find(|(v, l, _)| *v == vm && l.as_ptr() == loader.as_ptr())
    {
        Some((_, _, slot)) => *slot = meta,
        None => t.push((vm, loader, meta)),
    }
}

/// Read `loader`'s recorded bookkeeping, if VM `vm` has any.
pub(crate) fn loader_meta_get(vm: usize, loader: ObjectRef) -> Option<LoaderMeta> {
    loader_meta_store()
        .lock()
        .iter()
        .find(|(v, l, _)| *v == vm && l.as_ptr() == loader.as_ptr())
        .map(|&(_, _, m)| m)
}

/// Read-modify-write, creating an all-`None` entry if the loader has none.
fn loader_meta_upsert(vm: usize, loader: ObjectRef, f: impl FnOnce(&mut LoaderMeta)) {
    let mut t = loader_meta_store().lock();
    match t
        .iter_mut()
        .find(|(v, l, _)| *v == vm && l.as_ptr() == loader.as_ptr())
    {
        Some((_, _, slot)) => f(slot),
        None => {
            let mut meta = LoaderMeta::default();
            f(&mut meta);
            t.push((vm, loader, meta));
        }
    }
}

/// An instance field the REAL `java.lang.ClassLoader` declares and no
/// CratonVM stand-in can: `ensure_synthetic_class` fabricates a class with an
/// EMPTY field list (`fabricate_class`, `fields: vec![]`), so a fabricated
/// loader stub declares no named fields at all.
///
/// Three names, not one, so a rename in a future JDK degrades one witness at a
/// time instead of flipping the whole predicate. All three are private JDK
/// internals of `java.lang.ClassLoader` and have been since JDK 9.
const REAL_CLASSLOADER_WITNESS_FIELDS: [&str; 3] =
    ["parallelLockMap", "package2certs", "nameAndId"];

/// JDK-ONLY-LAYOUT: does this loader object actually have OUR seven-slot
/// synthetic layout, or the real JDK `ClassLoader` layout?
///
/// **Ask by NAME, never by field count.** `object_num_fields(x) >= N` returns
/// the requested count on both layouts, so it cannot tell them apart; that
/// version of this predicate shipped completely inert on 2026-08-04 and an A/B
/// against the pre-fix binary counted the identical overlay writes with and
/// without it (see the wave-2 README's failure modes).
///
/// `resolve_field_index_by_class_id` walks the class hierarchy, which is what
/// makes this work for `ClassLoaders$AppClassLoader` — the witness fields are
/// declared three superclasses up on `java.lang.ClassLoader`, so a
/// `declared_fields` test on the receiver's own class would answer "synthetic"
/// for every built-in loader and be inert exactly where the eight measured
/// rows are.
fn cl_has_synthetic_layout(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(loader);
    !REAL_CLASSLOADER_WITNESS_FIELDS
        .iter()
        .any(|name| ctx.resolve_field_index_by_class_id(cid, name).is_some())
}

// ---------------------------------------------------------------------------
// The four accessors every converted read site goes through.
//
// Order is the one `vh_field_desc` uses: side table first, raw slot second.
// The raw-slot fallback is gated on [`cl_has_synthetic_layout`] so that a
// loader allocated outside our path still works in synthetic-JDK mode, while
// on a real JDK layout no `CL_*` value is ever read out of an object slot —
// reading `parallelLockMap` back as an `Int` is the same confusion the write
// side just stopped committing.
// ---------------------------------------------------------------------------

/// `CL_LOADER_TYPE`. `None` = unknown; each caller keeps its own default.
fn loader_type_of(ctx: &dyn NativeContext, loader: ObjectRef) -> Option<i32> {
    if let Some(t) = loader_meta_get(ctx.vm_identity(), loader).and_then(|m| m.loader_type) {
        return Some(t);
    }
    if cl_has_synthetic_layout(ctx, loader) {
        if let Value::Int(v) = ctx.get_field(loader, CL_LOADER_TYPE) {
            return Some(v);
        }
    }
    None
}

/// `CL_CLASSES_LOADED`.
fn loader_classes_loaded_of(ctx: &dyn NativeContext, loader: ObjectRef) -> Option<i32> {
    if let Some(n) = loader_meta_get(ctx.vm_identity(), loader).and_then(|m| m.classes_loaded) {
        return Some(n);
    }
    if cl_has_synthetic_layout(ctx, loader) {
        if let Value::Int(v) = ctx.get_field(loader, CL_CLASSES_LOADED) {
            return Some(v);
        }
    }
    None
}

/// `CL_IS_PARALLEL_CAPABLE`.
pub(crate) fn loader_parallel_capable_of(
    ctx: &dyn NativeContext,
    loader: ObjectRef,
) -> Option<i32> {
    if let Some(p) = loader_meta_get(ctx.vm_identity(), loader).and_then(|m| m.parallel_capable) {
        return Some(i32::from(p));
    }
    if cl_has_synthetic_layout(ctx, loader) {
        if let Value::Int(v) = ctx.get_field(loader, CL_IS_PARALLEL_CAPABLE) {
            return Some(v);
        }
    }
    None
}

/// `CL_LOADER_ID`. `None` for "no namespace id assigned"; `0` is never a
/// user-loader id (it is the shared application namespace), so the callers'
/// pre-existing `v > 0` tests are preserved by returning `None` for it.
pub(crate) fn loader_id_of(ctx: &dyn NativeContext, loader: ObjectRef) -> Option<u32> {
    if let Some(id) = loader_meta_get(ctx.vm_identity(), loader).and_then(|m| m.loader_id) {
        if id > 0 {
            return Some(id);
        }
        return None;
    }
    if cl_has_synthetic_layout(ctx, loader) {
        if let Value::Int(v) = ctx.get_field(loader, CL_LOADER_ID) {
            if v > 0 {
                return Some(v as u32);
            }
        }
    }
    None
}

/// Write side: record in the table always, and mirror into the raw slot only
/// on our own layout.
fn loader_set_classes_loaded(ctx: &mut dyn NativeContext, loader: ObjectRef, n: i32) {
    loader_meta_upsert(ctx.vm_identity(), loader, |m| m.classes_loaded = Some(n));
    if cl_has_synthetic_layout(ctx, loader) {
        ctx.set_field(loader, CL_CLASSES_LOADED, Value::Int(n));
    }
}

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
pub(crate) const LK_PACKAGE: i32 = 0x08;
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
fn alloc_default_protection_domain(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut cs = try_alloc_concurrent_synthetic(ctx, CS_CLASS, CS_FIELD_COUNT)?;
    let cs_pin = ctx.pin_native_root(cs);
    ctx.set_field(cs, CS_LOCATION_REF, Value::Object(None));
    ctx.set_field(cs, CS_CERTIFICATES_REF, Value::Object(None));
    // Belt-and-suspenders: also set by name in case the real JDK CodeSource
    // layout reads through a different field index than our synthetic.
    cs = ctx.read_native_pin(cs_pin, cs);
    ctx.set_field_by_name(cs, "location", Value::Object(None));
    cs = ctx.read_native_pin(cs_pin, cs);
    ctx.set_field_by_name(cs, "certs", Value::Object(None));

    let mut pd = try_alloc_concurrent_synthetic(ctx, PD_CLASS, PD_FIELD_COUNT)?;
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
    Ok(pd)
}

pub(crate) fn alloc_classloader(
    ctx: &mut dyn NativeContext,
    loader_type: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    // WP1.5: built-in loaders must report the real JDK type name via
    // reflection. `jdk.internal.loader.ClassLoaders$PlatformClassLoader` for
    // the platform loader, `...$AppClassLoader` for the system loader, and
    // the abstract `java.lang.ClassLoader` only for unknown/custom callers.
    let class_name = match loader_type {
        LOADER_PLATFORM => "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
        LOADER_APP => "jdk/internal/loader/ClassLoaders$AppClassLoader",
        _ => CL_CLASS,
    };
    let mut obj = try_alloc_concurrent_synthetic(ctx, class_name, CL_FIELD_COUNT)?;
    let obj_pin = ctx.pin_native_root(obj);
    // L1: the synthetic slots go into the object ONLY on our own layout. On a
    // real JDK image `class_name` resolves to the real
    // `ClassLoaders$AppClassLoader` / `$PlatformClassLoader`, and NONE of the
    // seven indices means there what it means here:
    //
    //   ours                      | real `java.lang.ClassLoader`
    //   0 CL_LOADER_TYPE   Int    | parent            ClassLoader
    //   1 CL_PARENT_REF    ref    | name              String
    //   2 CL_NAME_REF      ref    | unnamedModule     Module
    //   3 CL_CLASSES_LOADED Int   | nameAndId         String
    //   4 CL_IS_PARALLEL…  Int    | parallelLockMap   ConcurrentHashMap
    //   5 CL_DEFAULT_DOMAIN ref   | package2certs     ConcurrentHashMap
    //   6 CL_LOADER_ID     Int    | classes           ArrayList
    //
    // The four `Int` rows are the eight measured overlay rows (four slots x
    // two built-in loaders): `Heap::set_field_as` coerces each to
    // `Object(None)` and the JDK's field is gone. The three reference rows
    // are the SAME defect one kind quieter — a reference for a reference, so
    // the detector cannot see it, but slot 1 still receives a ClassLoader
    // where `name:String` belongs and slot 5 a ProtectionDomain where
    // `package2certs:ConcurrentHashMap` belongs.
    //
    // The wave-2 brief's step 4 says to leave those three alone because "they
    // are references with real counterparts and are already written by name
    // too". The first half of that is a factual error: the by-name write goes
    // to a DIFFERENT slot than the index write (`name` is 1, and we index-write
    // 1 with the parent), so the index write is not a duplicate of it — it is
    // the corruption of an unrelated field. `classloader_parent`'s slot-1
    // fallback then read that field back and returned the platform loader's
    // `name` String AS ITS PARENT. Gated here, and at the matching reads.
    //
    // Nothing is lost by skipping any of them: `loader_meta_put` below records
    // the four VM-internal values, the by-name writes cover `name` / `parent`
    // / `defaultDomain`, and every reader consults the table (or the name)
    // first.
    let synthetic_layout = cl_has_synthetic_layout(ctx, obj);
    if synthetic_layout {
        ctx.set_field(obj, CL_LOADER_TYPE, Value::Int(loader_type));
        ctx.set_field(obj, CL_PARENT_REF, Value::Object(None));
        ctx.set_field(obj, CL_NAME_REF, Value::Object(None));
        ctx.set_field(obj, CL_CLASSES_LOADED, Value::Int(0));
        ctx.set_field(obj, CL_IS_PARALLEL_CAPABLE, Value::Int(1));
    }
    let pd = alloc_default_protection_domain(ctx)?;
    let pd_pin = ctx.pin_native_root(pd);
    obj = ctx.read_native_pin(obj_pin, obj);
    let pd = ctx.read_native_pin(pd_pin, pd);
    if synthetic_layout {
        ctx.set_field(obj, CL_DEFAULT_DOMAIN, Value::Object(Some(pd)));
    }
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
    if synthetic_layout {
        ctx.set_field(obj, CL_LOADER_ID, Value::Int(lid));
    }
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
            try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
        let name_to_module_pin = ctx.pin_native_root(name_to_module);
        obj = ctx.read_native_pin(obj_pin, obj);
        let name_to_module = ctx.read_native_pin(name_to_module_pin, name_to_module);
        ctx.set_field_by_name(obj, "nameToModule", Value::Object(Some(name_to_module)));
        let module_to_reader =
            try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
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
        try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
    let packages_map_pin = ctx.pin_native_root(packages_map);
    obj = ctx.read_native_pin(obj_pin, obj);
    let packages_map = ctx.read_native_pin(packages_map_pin, packages_map);
    ctx.set_field_by_name(obj, "packages", Value::Object(Some(packages_map)));
    // `ClassLoader.setDefaultAssertionStatus` uses `synchronized (assertionLock)`.
    // Real JDK ctors assign `this.assertionLock = new Object()`; synthetic
    // allocation skips that, so Surefire's forked booter NPEs on monitorenter.
    let lock = try_alloc_concurrent_synthetic(ctx, "java/lang/Object", 0)?;
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
    // `parallelLockMap`: both built-in loader classes register as
    // parallel-capable (`BuiltinClassLoader.<clinit>` and its subclasses), so
    // JDK 25's `ClassLoader` constructor gives them a map and
    // `getClassLoadingLock(name)` a per-name lock. Allocated here without that
    // constructor, the field stayed null and the lock was the loader itself,
    // serializing `BuiltinClassLoader.loadClassOrNull` on it (probe
    // `L5W28BuiltinLoaderLock`). `--jdk-only`, real layout only
    // (`--compatible` unchanged). Interpreter round i1 wave 28, lane L5.
    if !synthetic_layout
        && ctx.is_jdk_only()
        && (loader_type == LOADER_PLATFORM || loader_type == LOADER_APP)
    {
        let lock_map =
            try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
        obj = ctx.read_native_pin(obj_pin, obj);
        ctx.set_field_by_name(obj, "parallelLockMap", Value::Object(Some(lock_map)));
    }
    let obj = ctx.read_native_pin(obj_pin, obj);
    // L1: record the four VM-internal values beside the object. Done LAST, on
    // the post-allocation address: every step above can collect (the
    // ProtectionDomain, three ConcurrentHashMaps and the assertion lock are
    // all allocations), and the table is keyed by address.
    loader_meta_put(
        ctx.vm_identity(),
        obj,
        LoaderMeta {
            loader_type: Some(loader_type),
            classes_loaded: Some(0),
            parallel_capable: Some(true),
            loader_id: Some(lid as u32),
        },
    );
    ctx.unpin_native_roots(obj_pin);
    Ok(obj)
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
/// already keys real-JDK-mode ids in an object-keyed side table instead of
/// touching the field, so delegating to it fixes the corruption for free
/// while preserving identical id-assignment semantics (same
/// `allocate_loader_id()` counter, same "0 = application/built-in loader"
/// convention `loader_id_for`'s null-loader callers already rely on).
///
/// L1 finished the job on the WRITE side too: the `ClassLoader` constructor
/// natives now record the id in [`LoaderMeta`] and only mirror it into slot 6
/// when [`cl_has_synthetic_layout`] says the object has our layout.
pub(crate) fn get_or_assign_loader_id(ctx: &mut dyn NativeContext, cl: ObjectRef) -> u32 {
    loader_namespace_id(ctx, cl)
}

fn alloc_url_classloader(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let mut obj = try_alloc_concurrent_synthetic(ctx, UCL_CLASS, UCL_FIELD_COUNT)?;
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
    Ok(obj)
}

fn alloc_lookup(ctx: &mut dyn NativeContext, modes: i32) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, LK_CLASS, LK_FIELD_COUNT)?;
    ctx.set_field(obj, LK_LOOKUP_CLASS_REF, Value::Object(None));
    // W6-3: both index writes are the SYNTHETIC layout. On real JDK 25
    // (`javap -p java.lang.invoke.MethodHandles$Lookup`) slot 2 is
    // `allowedModes` (int) and slot 3 is `cachedProtectionDomain`, a
    // `private volatile ProtectionDomain` REFERENCE. `lk_set_modes` below
    // repairs slot 2 by name; slot 3 was never repaired, so `Int(modes)` sat
    // in a reference slot the GC scans as an oop. Gate both on the synthetic
    // layout — a real Lookup declares `prevLookupClass`, a fabricated stub
    // names its fields `_f0..`. `cachedProtectionDomain` must stay null: it is
    // a lazy cache `lookupClassProtectionDomain()` fills on first use.
    let synthetic_layout = lk_real_prev_lookup_class_slot(ctx, obj).is_none();
    if synthetic_layout {
        ctx.set_field(obj, LK_PREVIOUS_LOOKUP_CLASS, Value::Object(None));
        ctx.set_field(obj, LK_LOOKUP_MODE, Value::Int(modes));
    }
    // Write `allowedModes` so it lands on the real JDK field (3-field layout
    // puts it at slot 2, not the synthetic slot 1 = prevLookupClass). See
    // `lk_modes_of` and `lang_invoke::lk_write_allowed_modes`.
    lk_set_modes(ctx, obj, modes);
    Ok(obj)
}

/// The slot of the DECLARED `allowedModes` field, or `None` when the receiver
/// does not have the real `java.lang.invoke.MethodHandles$Lookup` layout.
///
/// The witness is CLASS-side on purpose, and the reason is stronger than the
/// one this note used to give. It claimed a by-name read of an ABSENT field
/// answers `Int(0)`. **It does not.** `vm/src/vm/vm_exec.rs`'s
/// `get_field_by_name` resolves the name in the hierarchy and, on a miss,
/// returns `Value::Object(None)` — the trait even documents that
/// (`native-api/src/registry.rs`: "Returns `Value::Object(None)` if the field
/// is not found"). `Int(0)` is what `test_utils::MockNativeContext` answers,
/// and what a PRESENT but never-written `int` slot decodes as. So the value
/// alone cannot separate any of three states: absent, present-and-null, and
/// present-and-zero.
///
/// That makes the class-side witness the only thing that answers the question
/// at all, not a hardening of a working test. A fabricated Lookup stub names
/// its fields `_f0.._f3` (`ensure_synthetic_class`), so `allowedModes` IS
/// absent there, and the old "by name first, synthetic slot second" reader
/// returned 0 for every synthetic Lookup and never reached the slot that
/// actually holds the modes: `lookupModes()` answered 0 and
/// the `find*` access gate saw a powerless Lookup for the whole of
/// synthetic-JDK mode. (That gate is
/// `lang_invoke::lk_enforce_find_access`. This module's own copy, named
/// `enforce_lookup_access`, was deleted 2026-08-12 as never-registered dead
/// code — see the tombstone above `lk_unreflect`.)
///
/// Asking the CLASS is also descriptor-safe: `resolve_field_index_by_class_id`
/// resolves the declared `int allowedModes` on `MethodHandles$Lookup`, not
/// some same-named field of another type further down a hierarchy.
fn lk_real_allowed_modes_slot(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<usize> {
    ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(obj), "allowedModes")
}

/// The slot of the DECLARED `prevLookupClass` field, or `None` when the
/// receiver does not have the real `MethodHandles$Lookup` layout.
///
/// The `prevLookupClass` half of [`lk_real_allowed_modes_slot`] — see that
/// function for why the witness must be class-side rather than a value-shape
/// test. `Some` from this one and `Some` from that one are the SAME layout
/// verdict, which is what lets `alloc_lookup`, `lk_set_modes`, `lk_modes_of`
/// and `lk_previous_lookup_class` agree about which object they are holding.
fn lk_real_prev_lookup_class_slot(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<usize> {
    ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(obj), "prevLookupClass")
}

/// Write `allowedModes`: the declared slot on a real Lookup, the synthetic
/// slot on a fabricated one.
///
/// The two arms are mutually exclusive by construction. That is load-bearing:
/// on the real layout `LK_ALLOWED_MODES` (slot 1) is `prevLookupClass`, a
/// REFERENCE the GC scans as an oop, so an `Int` written there is heap
/// corruption rather than merely a wrong answer — the same shape as the W6-3
/// `cachedProtectionDomain` finding repaired in `alloc_lookup` above.
fn lk_set_modes(ctx: &mut dyn NativeContext, obj: ObjectRef, modes: i32) {
    if let Some(slot) = lk_real_allowed_modes_slot(ctx, obj) {
        ctx.set_field_by_name(obj, "allowedModes", Value::Int(modes));
        if !matches!(ctx.get_field(obj, slot), Value::Int(m) if m == modes) {
            // Named write did not land; go through the resolved index. Never
            // through `LK_ALLOWED_MODES` — see the note above.
            ctx.set_field(obj, slot, Value::Int(modes));
        }
        return;
    }
    ctx.set_field(obj, LK_ALLOWED_MODES, Value::Int(modes));
}

/// Read a Lookup's `allowedModes` from whichever layout the receiver has.
pub(crate) fn lk_modes_of(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    if let Some(slot) = lk_real_allowed_modes_slot(ctx, this) {
        if let Value::Int(m) = ctx.get_field(this, slot) {
            return m;
        }
        if let Value::Int(m) = ctx.get_field_by_name(this, "allowedModes") {
            return m;
        }
        return 0;
    }
    if let Value::Int(m) = ctx.get_field(this, LK_ALLOWED_MODES) {
        return m;
    }
    0
}

/// The JDK's `FULL_POWER_MODES` == `PUBLIC|PRIVATE|PROTECTED|PACKAGE|MODULE`
/// == 0x1F.
///
/// NOT the same thing as [`LK_FULL_POWER`] (0x5F), which is this module's name
/// for the modes of `MethodHandles.lookup()` — that value additionally carries
/// `ORIGINAL`. `Lookup.in` and `Lookup.dropLookupMode` both mask against the
/// JDK's 0x1F, so the two must not be confused.
const LK_FULL_POWER_MODES: i32 = LK_PUBLIC | LK_PRIVATE | LK_PROTECTED | LK_PACKAGE | LK_MODULE;

/// `Lookup.in(requestedLookupClass)` mode arithmetic, JDK 25.
///
/// Measured (`java p.LkProbe` / `p.LkProbe2`, OpenJDK 25.0.3; receiver is
/// `MethodHandles.lookup()` in `p.LkProbe2`, whose `lookupModes()` is 95):
///
/// | target                                        | modes |
/// |-----------------------------------------------|-------|
/// | `in(LkProbe2.class)` — the lookup class itself | 95    |
/// | `in(LkProbe2.Nested.class)` — a NESTMATE       | 31    |
/// | `in(p.Mate.class)` — same package, other file  | **25**|
/// | `in(String.class)` — other module              | 1     |
/// | receiver 32 (`publicLookup()`), any target     | 32    |
/// | receiver 25, same package / nestmate           | 25    |
/// | receiver 25, other module                      | 1     |
/// | receiver 1 or 0, any target                    | 1 / 0 |
///
/// **The same-package row is 25, not 31.** `Lookup.in` applies FOUR
/// reductions, not two, and the third is the one a package-name comparison
/// alone cannot see (`VerifyAccess.isSamePackageMember` — same outermost
/// enclosing class, i.e. a nestmate):
///
/// ```text
///   if allowedModes == UNCONDITIONAL      -> unchanged (publicLookup stays 32)
///   if target == lookupClass              -> `this` (ORIGINAL kept)
///   newModes = prev & FULL_POWER_MODES                     // drops ORIGINAL
///   if !sameModule    newModes &= ~(MODULE|PACKAGE|PRIVATE|PROTECTED)
///   if !samePackage   newModes &= ~(PACKAGE|PRIVATE|PROTECTED)
///   if !sameNest      newModes &= ~(PRIVATE|PROTECTED)
/// ```
///
/// 95 -> 31 (mask) -> same package so PACKAGE survives -> not a nestmate, so
/// PRIVATE|PROTECTED go -> 31 & !6 == 25. Returning 31 there handed a
/// package-mate lookup PRIVATE access the real JDK does not grant, which is
/// the direction that turns a `find*` that SHOULD raise
/// `IllegalAccessException` into a silent success.
///
/// CratonVM does not model modules, so `!sameModule` and `!samePackage`
/// collapse into one test; both strip down to `PUBLIC` from 95, which is the
/// measured cross-module answer.
/// `pub(crate)` so `lang_invoke`'s competing `Lookup.in` registration (which
/// WINS in real-JDK mode — see the note in `register_classloader_natives`) can
/// adopt this arithmetic instead of keeping a second, differently-wrong copy.
pub(crate) fn lk_in_modes(
    prev: i32,
    same_class: bool,
    same_package: bool,
    same_nest: bool,
    target_is_public: bool,
) -> i32 {
    // `publicLookup()` is UNCONDITIONAL-only (32), and `in()` KEEPS it only for
    // a target the whole world can already see. The JDK routes this arm through
    // `Lookup.publicLookup(requestedLookupClass)`, which yields 0 for a class
    // that is not public or whose package is not exported.
    //
    // Re-measured on OpenJDK 25.0.3 (`PubIn`, receiver `publicLookup()` == 32):
    //
    // | target                                        | modes |
    // |-----------------------------------------------|-------|
    // | a PUBLIC class in the unnamed module          | 32    |
    // | a PUBLIC nested class                         | 32    |
    // | a package-private nested class                | **0** |
    // | a package-private top-level class             | **0** |
    // | `java.lang.String` (public, exported)         | 32    |
    // | `jdk.internal.misc.Unsafe` (public, NOT exported) | **0** |
    // | `java.lang.AbstractStringBuilder` (pkg-private)   | **0** |
    //
    // The old unconditional `return prev` came from a table measured only
    // against PUBLIC targets, and it handed `publicLookup().in(<package-private
    // class>)` the value 32 — a lookup that can resolve public members of a
    // class the JDK refuses to give any lookup at all.
    //
    // MODULES ARE NOT MODELLED, so the export half of the test is not applied:
    // a public class in a non-exported package (`jdk.internal.misc.Unsafe`)
    // answers 32 here and 0 on HotSpot. That is the one remaining divergence on
    // this arm, it is recorded rather than approximated by package prefix, and
    // it is strictly narrower than what this arm granted before.
    if prev == LK_UNCONDITIONAL {
        return if target_is_public { prev } else { 0 };
    }
    if same_class {
        // `in(lookupClass())` returns `this` in the JDK, ORIGINAL included.
        return prev;
    }
    let mut modes = prev & LK_FULL_POWER_MODES;
    if !same_package {
        modes &= !(LK_PACKAGE | LK_PRIVATE | LK_PROTECTED | LK_MODULE);
    }
    if !same_nest {
        // `isSamePackageMember`: a same-package class that is not a member of
        // the same top-level class is still "a cousin", and loses PRIVATE
        // (and PROTECTED with it).
        modes &= !(LK_PRIVATE | LK_PROTECTED);
    }
    modes
}

/// `Lookup.dropLookupMode(int)` mode arithmetic, JDK 25. `None` means the
/// argument is not a droppable mode and the JDK throws
/// `IllegalArgumentException`.
///
/// Measured (`java LkProbe`, OpenJDK 25.0.3, `old == 95`), against the naive
/// `old & !drop` the previous implementation used:
///
/// | drop           | real | `old & !drop` |
/// |----------------|------|---------------|
/// | PUBLIC         | 0    | 94            |
/// | PRIVATE        | 25   | 93            |
/// | PROTECTED      | 27   | 91            |
/// | PACKAGE        | 17   | 87            |
/// | MODULE         | 1    | 79            |
/// | UNCONDITIONAL  | 27   | 95            |
/// | ORIGINAL       | 27   | 31            |
///
/// The naive form is wrong for all SEVEN, not just for the ones the old
/// wrong-slot read reached: `dropLookupMode` also drops `PROTECTED` and
/// `ORIGINAL` unconditionally, then cascades per the dropped mode. Also
/// measured: `dropLookupMode(0)` and `dropLookupMode(PRIVATE|PROTECTED)` both
/// throw `IllegalArgumentException: <n> is not a valid mode to drop`.
fn lk_drop_modes(old: i32, drop: i32) -> Option<i32> {
    let mut modes = old & !(drop | LK_PROTECTED | LK_ORIGINAL);
    match drop {
        LK_PUBLIC => modes = 0,
        LK_MODULE => modes &= !(LK_PACKAGE | LK_PRIVATE | LK_PROTECTED),
        LK_PACKAGE => modes &= !(LK_PRIVATE | LK_PROTECTED),
        LK_PROTECTED | LK_PRIVATE | LK_ORIGINAL | LK_UNCONDITIONAL => {}
        _ => return None,
    }
    Some(modes)
}

// ---------------------------------------------------------------------------
// java.lang.ClassLoader natives
// ---------------------------------------------------------------------------

fn cl_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // L1: record before allocating anything — `this` is the live receiver at
    // this point, and the table is keyed by address.
    loader_meta_put(
        ctx.vm_identity(),
        this,
        LoaderMeta::custom_initialiser(None),
    );
    let synthetic_layout = cl_has_synthetic_layout(ctx, this);
    if synthetic_layout {
        ctx.set_field(this, CL_LOADER_TYPE, Value::Int(LOADER_CUSTOM));
    }
    // gen r4w3/rooting: `alloc_classloader`, `alloc_default_protection_domain`
    // and `try_alloc_concurrent_synthetic` can initialise classes / allocate
    // (GC points) on first use; keep `this` rooted and re-read after each.
    let this_pin = ctx.pin_native_root(this);
    // parent defaults to system class loader
    let sys = alloc_classloader(ctx, LOADER_APP);
    let this = ctx.read_native_pin(this_pin, this);
    if synthetic_layout {
        ctx.set_field(this, CL_PARENT_REF, Value::Object(Some(sys?)));
        ctx.set_field(this, CL_NAME_REF, Value::Object(None));
        ctx.set_field(this, CL_CLASSES_LOADED, Value::Int(0));
        ctx.set_field(this, CL_IS_PARALLEL_CAPABLE, Value::Int(1));
    } else {
        ctx.set_field_by_name(this, "parent", Value::Object(Some(sys?)));
    }
    // WP2.3: build a non-null defaultDomain so JDK preDefineClass's
    // `pd.getCodeSource()` chain doesn't NPE on the no-PD defineClass path.
    let pd = alloc_default_protection_domain(ctx)?;
    let this = ctx.read_native_pin(this_pin, this);
    if synthetic_layout {
        ctx.set_field(this, CL_DEFAULT_DOMAIN, Value::Object(Some(pd)));
    }
    ctx.set_field_by_name(this, "defaultDomain", Value::Object(Some(pd)));
    // S111r17: see alloc_classloader — initialize `packages` CHM so
    // ClassLoader.packages() doesn't NPE on `getfield + values()`.
    let packages_map =
        try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field_by_name(this, "packages", Value::Object(Some(packages_map)));
    Ok(None)
}

fn cl_init_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let parent = args.get(1).copied().unwrap_or(Value::Object(None));
    // L1: see `cl_init_default`.
    loader_meta_put(
        ctx.vm_identity(),
        this,
        LoaderMeta::custom_initialiser(None),
    );
    let synthetic_layout = cl_has_synthetic_layout(ctx, this);
    if synthetic_layout {
        ctx.set_field(this, CL_LOADER_TYPE, Value::Int(LOADER_CUSTOM));
    }
    if synthetic_layout {
        ctx.set_field(this, CL_PARENT_REF, parent);
        ctx.set_field(this, CL_NAME_REF, Value::Object(None));
        ctx.set_field(this, CL_CLASSES_LOADED, Value::Int(0));
        ctx.set_field(this, CL_IS_PARALLEL_CAPABLE, Value::Int(1));
    } else {
        ctx.set_field_by_name(this, "parent", parent);
    }
    // gen r4w3/rooting: the PD / CHM allocations below are GC-capable on
    // first use; keep `this` rooted and re-read after each.
    let this_pin = ctx.pin_native_root(this);
    let pd = alloc_default_protection_domain(ctx)?;
    let this = ctx.read_native_pin(this_pin, this);
    if synthetic_layout {
        ctx.set_field(this, CL_DEFAULT_DOMAIN, Value::Object(Some(pd)));
    }
    ctx.set_field_by_name(this, "defaultDomain", Value::Object(Some(pd)));
    // S111r17: see alloc_classloader.
    let packages_map =
        try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field_by_name(this, "packages", Value::Object(Some(packages_map)));
    Ok(None)
}

fn cl_init_name_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    let parent = args.get(2).copied().unwrap_or(Value::Object(None));
    // Assign the unique namespace id up front: `this` is the live receiver
    // here, before the ProtectionDomain / CHM allocations below can collect
    // and move it, and the L1 table is keyed by address. The id comes from a
    // counter, so pulling it forward changes nothing about its value's
    // meaning.
    let lid = ctx.allocate_loader_id();
    loader_meta_put(
        ctx.vm_identity(),
        this,
        LoaderMeta::custom_initialiser(Some(lid)),
    );
    let synthetic_layout = cl_has_synthetic_layout(ctx, this);
    if synthetic_layout {
        ctx.set_field(this, CL_LOADER_TYPE, Value::Int(LOADER_CUSTOM));
    }
    if synthetic_layout {
        ctx.set_field(this, CL_PARENT_REF, parent);
        ctx.set_field(this, CL_NAME_REF, name);
        ctx.set_field(this, CL_CLASSES_LOADED, Value::Int(0));
        ctx.set_field(this, CL_IS_PARALLEL_CAPABLE, Value::Int(1));
    } else {
        ctx.set_field_by_name(this, "parent", parent);
        ctx.set_field_by_name(this, "name", name);
    }
    // gen r4w3/rooting: the PD / CHM allocations below are GC-capable on
    // first use; keep `this` rooted and re-read after each.
    let this_pin = ctx.pin_native_root(this);
    let pd = alloc_default_protection_domain(ctx)?;
    let this = ctx.read_native_pin(this_pin, this);
    if synthetic_layout {
        ctx.set_field(this, CL_DEFAULT_DOMAIN, Value::Object(Some(pd)));
    }
    ctx.set_field_by_name(this, "defaultDomain", Value::Object(Some(pd)));
    // S111r17: see alloc_classloader.
    let packages_map =
        try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field_by_name(this, "packages", Value::Object(Some(packages_map)));
    if synthetic_layout {
        ctx.set_field(this, CL_LOADER_ID, Value::Int(lid as i32));
    }
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

/// The loader class `ModuleLayer.defineModulesWithOneLoader` /
/// `…WithManyLoaders` create for the modules of a new layer.
pub(crate) const JDK_LAYER_LOADER_CLASS: &str = "jdk/internal/loader/Loader";

/// [`is_builtin_loader_class`] as the VM `ctx` sees it. Under `--jdk-only`
/// the JDK's layer loader (`jdk.internal.loader.Loader`) is NOT a built-in
/// loader: HotSpot treats it as any other `ClassLoader` subclass, and its own
/// `loadClass(String, boolean)` / `findClass` / resource overrides are what
/// map a package to the module of the layer. The `jdk/internal/loader/`
/// prefix matched it, so `ClassLoader.loadClass` never ran its override (base
/// parent-first delegation answered `ClassNotFoundException` for every class
/// of the layer's modules) and its defines went to the application namespace
/// (interpreter round i1 wave 43, lane L5,
/// `i42-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module`,
/// probe `tools/probes/interp/L5/L5W43UserLayerModule.java`). `--compatible`
/// keeps the prefix rule.
pub(crate) fn is_builtin_loader_class_in(ctx: &dyn NativeContext, class_name: &str) -> bool {
    if class_name == JDK_LAYER_LOADER_CLASS && ctx.is_jdk_only() {
        return false;
    }
    is_builtin_loader_class(class_name)
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

/// Whether the bootstrap or platform loader can define `internal`: under
/// `--jdk-only` with a runtime image, exactly the classes the image defines
/// (plus `-Xbootclasspath/a`, which callers check separately); otherwise
/// [`is_bootstrap_class_name`]'s prefix list.
///
/// The prefix list is a guess, and it guesses wrong for every library that
/// lives under a JDK-looking prefix: `com/sun/el` (GlassFish EL, the
/// `ExpressionFactory` Hibernate Validator creates), `com/sun/mail`,
/// `com/sun/xml/bind`, `javax/inject`, ... For such a name a loader whose
/// parent is the platform loader was answered from the flat class store --
/// the APPLICATION loader's copy -- where HotSpot's platform loader throws
/// `ClassNotFoundException` and the loader's own `findClass` defines its own.
/// Spring's `CompileWithForkedClassLoaderClassLoader` (parent: platform) got
/// the application's `com.sun.el.ExpressionFactoryImpl` under its own
/// `jakarta.el.ExpressionFactory`, and `ExpressionFactory.newInstance()`'s
/// cast failed -- hidden until 2026-09-23 by the bytecode name rule that
/// accepted any same-named class
/// (`docs/internal/fixed-suite-bugs/spring/`
/// `spring-jdkonly-bytecode-cast-name-rule-jit-class-resolution-FIXED-20260923.md`).
pub(crate) fn is_runtime_image_class_name(ctx: &dyn NativeContext, internal: &str) -> bool {
    // In the image, and still not the bootstrap loader's: `MethodUtil` defines
    // it through its own loader (see `is_bootstrap_class_name`).
    if internal == "sun/reflect/misc/Trampoline" {
        return false;
    }
    if ctx.is_jdk_only() && !internal.starts_with('[') {
        if let Some(defined) = ctx.runtime_image_defines_class(internal) {
            return defined;
        }
    }
    is_bootstrap_class_name(internal)
}

/// Whether `loader_obj` is eligible for loader-initiated resolution of a
/// class it defined -- either the global `CRATONVM_LOADER_AWARE_RESOLUTION`
/// gate is on (default-on since the Hibernate custom-loader soak; see
/// [`loader_aware_resolution`]'s doc comment for the validation history), or
/// `loader_obj` is a `groovy.lang.GroovyClassLoader` / Spring's own AOT-test
/// isolating loaders (`org.springframework.core.test.tools.{DynamicClassLoader,
/// CompileWithForkedClassLoaderClassLoader}`) -- narrow carve-outs for when
/// the global gate is explicitly disabled (`CRATONVM_LOADER_AWARE_
/// RESOLUTION=0`), mirroring `is_groovy_class_loader` in interpreter.rs.
pub(crate) fn is_loader_aware_resolution_eligible(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
) -> bool {
    // Loader-identity consolidation: this used to inline its own
    // `cratonvm_types::flags::runtime_var("CRATONVM_LOADER_AWARE_RESOLUTION")` parse -- a FOURTH
    // copy of the same gate living right next to the crate's own
    // `loader_aware_resolution()` below. Route through that single
    // in-crate copy (which itself now delegates to
    // `cratonvm_classloading::loader_aware_resolution`, the workspace
    // source of truth) instead. See `loader-identity.md`.
    if loader_aware_resolution() {
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
    crate::nbflags().cl_bootstrap_scoped
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
    crate::nbflags().loader_unload
}

/// `CRATONVM_LOADER_AWARE_RESOLUTION` gate (default ON). Gates the
/// native-builtins half of loader-faithful class resolution
/// (per-user-loader namespace assignment in `defineClass`, exact
/// `findLoadedClass`, `descriptor_to_class_mirror_via_loader` for
/// reflective Field/Method/Constructor types, annotation Class-value
/// resolution) so it stays in lock-step with the interpreter
/// (`vm::runtime::env_cache::loader_aware_resolution`) and class-manager
/// (`cratonvm_classloading::loader_aware_resolution`) halves.
///
/// **Loader-identity consolidation:** this crate depends directly on
/// `cratonvm-classloading` (see `Cargo.toml`), so this is no longer an
/// independent `OnceLock`-cached env-var parse -- it forwards to
/// `cratonvm_classloading::loader_aware_resolution`, the single workspace
/// source of truth, which is what `vm::runtime::env_cache::
/// loader_aware_resolution` now also forwards to. This copy previously
/// drifted out of lock-step (stayed default OFF after `env_cache::
/// loader_aware_resolution` flipped to default ON for the `context.groovy`
/// bug-cluster fix), silently disabling this crate's share of the
/// loader-faithful fixes by default -- see
/// `hib-bytecode-enhancement-loader-faithful-linking-FIXED.md`
/// and `loader-identity.md`. Kept as a thin wrapper (rather
/// than switching call sites over to the classloading path directly) so
/// this crate's `#[inline]`/`pub(crate)` call sites and doc cross-references
/// do not need to change.
///
/// When off, every loader-identity path keeps its exact pre-gate behavior.
/// Empty / `"0"` ⇒ off; any other value ⇒ on.
#[inline]
pub(crate) fn loader_aware_resolution() -> bool {
    cratonvm_classloading::loader_aware_resolution()
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
        if is_builtin_loader_class_in(&*ctx, &name) {
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
        if is_builtin_loader_class_in(&*ctx, &name) {
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
        if is_builtin_loader_class_in(&*ctx, &name) {
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
//
// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
// rank 20): each entry is the receiver's native PIN HANDLE and the receiver as
// pushed (the `read_native_pin` fallback), matched by the pin's CURRENT
// address -- the w26-a pattern (`native-collections`' `reentry_guard_enter`).
// The entries used to be identity hashes. A hash follows the loader across a
// move, but two LIVE loaders can share one (every heap mints it from one
// 32-bit counter that wraps), so a colliding loader's `loadClass` reached from
// inside the first one's override read as the nested `super.loadClass`
// delegation and skipped its own override (`ModifiedClassPathClassLoader`'s
// package exclusions, say). A pin names exactly one object, and the outer
// frame that pushed an entry -- and so its pin -- is alive whenever an inner
// frame looks.
thread_local! {
    static SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT: RefCell<Vec<(usize, ObjectRef)>> =
        const { RefCell::new(Vec::new()) };
}

/// Push `this` (CURRENT) onto [`SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT`], or
/// answer `None` when an entry already names it (the nested delegation). On
/// `Some` the caller owns the returned pin and MUST pass it to
/// [`single_load_class_guard_leave`] on every path.
fn single_load_class_guard_enter(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let pin = ctx.pin_native_root(this);
    let reentered = SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT.with(|active| {
        let mut active = active.borrow_mut();
        let ctx: &dyn NativeContext = &*ctx;
        let held = active
            .iter()
            .any(|&(h, pushed)| ctx.read_native_pin(h, pushed) == this);
        if !held {
            active.push((pin, this));
        }
        held
    });
    if reentered {
        // The newest pin: releasing it releases nothing older.
        ctx.unpin_native_roots(pin);
        return None;
    }
    Some(pin)
}

/// Pop the entry [`single_load_class_guard_enter`] pushed under `pin` and
/// release the pin (the newest this frame holds: the guarded call's own pins
/// are balanced inside it).
fn single_load_class_guard_leave(ctx: &mut dyn NativeContext, pin: usize) {
    SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT.with(|active| {
        let mut active = active.borrow_mut();
        let found = active.iter().rposition(|&(h, _)| h == pin);
        debug_assert!(found.is_some(), "single loadClass guard entry missing");
        if let Some(i) = found {
            active.remove(i);
        }
    });
    ctx.unpin_native_roots(pin);
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
    let pin = single_load_class_guard_enter(ctx, this)?;
    if ctx.is_executing_instance_method(this, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;")
    {
        single_load_class_guard_leave(ctx, pin);
        return None;
    }
    let result = ctx.invoke_virtual(
        this,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(name_obj))],
    );
    single_load_class_guard_leave(ctx, pin);
    Some(result)
}

/// True if `this` is a USER-DEFINED `ClassLoader` (a non-builtin subclass), as
/// opposed to a built-in bootstrap/extension/application/URL loader. Used to
/// decide whether `findLoadedClass`/`findLoadedClass0` must be loader-scoped
/// (a user loader only "knows" classes in its own namespace) versus the global
/// lookup that is correct for the built-in loaders.
/// True iff `this` is a bare `java.net.URLClassLoader` instance (not a user
/// subclass — a subclass already gets its own namespace via
/// `is_user_defined_loader`'s `!is_builtin_loader_class` check).
///
/// `loader_namespace_id`/`peek_loader_namespace_id` are the ONLY callers —
/// `is_builtin_loader_class` intentionally still lists
/// `"java/net/URLClassLoader"` for its other ~20 call sites (isolation
/// checks, resource/service-loader resolution, …), where the bare class is
/// correctly treated as "not a distinguished user loader implementation".
/// But `new URLClassLoader(urls)` is a completely ordinary, unlimited-arity
/// application pattern for building an ISOLATED loader (e.g. Spring Boot's
/// `ApplicationHomeTests` constructs one per test method, each defining its
/// own unrelated `com.example.Source`); routing every such instance through
/// `loader_namespace_id`'s built-in shortcut (id `0`, the shared Application
/// namespace) made a SECOND bare `URLClassLoader` instance's class-define
/// collide with the first's as `IncompatibleClassChangeError: already
/// defined by application loader` — the two loaders are unrelated objects
/// with disjoint URLs, not aliases of the single real Application loader.
pub(crate) fn is_bare_url_class_loader(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        == Some("java/net/URLClassLoader")
}

pub(crate) fn is_user_defined_loader(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(this);
    // This predicate is reached from real ClassLoader bytecode before the
    // receiver's dynamic type has otherwise been constrained. Treating every
    // non-builtin class as a user loader made ordinary objects (notably String)
    // reach the synthetic CL_LOADER_ID slot probe, producing an OOB field read
    // during DoHead's reflective class loading. Require actual ClassLoader
    // inheritance before considering the built-in-name exclusion.
    let Some(class_loader) = ctx.class_id_by_name("java/lang/ClassLoader") else {
        return false;
    };
    if cid != class_loader && !ctx.is_subclass(cid, class_loader) {
        return false;
    }
    match ctx.class_name_of_id(cid) {
        Some(name) => !is_builtin_loader_class_in(&*ctx, &name),
        None => false,
    }
}

/// Loader-object → CratonVM loader-namespace-id side table for real-JDK mode.
///
/// In synthetic-JDK mode a user loader's namespace id lives in the synthetic
/// `CL_LOADER_ID` field slot (populated by `ClassLoader.<init>`/`defineClass`,
/// and mirrored into [`LoaderMeta::loader_id`] since L1). In real-JDK mode
/// that slot is a genuine `java.lang.ClassLoader` field and cannot be
/// repurposed, so the id is keyed on the loader OBJECT instead.
///
/// Distinct from the L1 [`loader_meta_store`], which it sits directly beside:
/// this table holds ids `loader_namespace_id_at` allocated *lazily*, on first
/// request, for loaders whose constructor never ran through one of our
/// natives. `LoaderMeta` holds what the constructor natives themselves
/// recorded. Both are object-keyed and both are pruned/remapped by the same
/// [`gc_reconcile_defining_loaders`] pass, for the same reason.
///
/// # GC contract — both halves are load-bearing
///
/// The entries are `(ObjectRef, u32)`. The `ObjectRef` is a raw heap address, so
/// this table has a two-part GC contract and **neither half may be dropped**:
///
/// * it is deliberately **NOT** a GC root (see `gc_scan_loader_singleton_roots`,
///   which roots only the app/platform singletons) — rooting it would pin every
///   user loader forever and defeat loader unloading;
/// * therefore [`gc_reconcile_defining_loaders`] MUST prune entries whose loader
///   died this cycle and remap the survivors that moved. That is the *only*
///   thing standing between a dead loader's address being reused and a brand-new
///   loader silently inheriting the dead one's namespace id — and, through
///   `register_user_loader_parent`, its parent link too.
///
/// The doc comment this replaced still described the table's *previous*,
/// identity-hash-keyed form ("holds only `i32 → u32` … no GC rooting"), which
/// had stopped being true; a reader who believed it would conclude there was
/// nothing for the collector to do here. Pinned by
/// `loader_namespace_store_is_pruned_and_remapped_by_gc_reconcile`.
///
/// # Rows are per VM
///
/// Each row is `(vm_identity, loader, namespace id)` (gc-common w9-a). The
/// address means something only in the heap that allocated it, so every
/// lookup matches the caller's VM as well as the address; the GC pass judges
/// and remaps only the collecting VM's rows (another VM's heap answers "dead"
/// for every one of them), VM creation no longer wipes the table, and
/// `forget_vm_loader_singletons` drops a VM's rows at teardown. Pinned by
/// `w9a_loader_namespace_and_meta_rows_belong_to_their_vm`.
fn loader_namespace_id_store() -> &'static Mutex<Vec<(usize, ObjectRef, u32)>> {
    // Keyed by the loader OBJECT, not its identity hash: identity hashes can
    // collide across distinct loader instances (address-derived hashes recur
    // after a collection reuses the region), and the old hash-keyed map never
    // pruned dead loaders — a fresh per-compile loader (Spring TestCompiler's
    // DynamicClassLoader) could inherit a dead sibling's namespace id and
    // resolve THAT loader's same-named generated classes. Entries are
    // remapped/pruned post-GC by [`gc_reconcile_defining_loaders`].
    static INSTANCE: OnceLock<Mutex<Vec<(usize, ObjectRef, u32)>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Reverse of `loader_namespace_id`: given a namespace id already allocated
/// via the object-keyed side table (real-JDK mode's path — a `UserDefined`
/// id from `class_manager`'s per-class `loader_id`, e.g. as returned by
/// `NativeContext::loader_id_of_class`), find the live `ClassLoader` object
/// that owns it. `None` for built-in namespaces (0/1/2) or a namespace this
/// process never allocated via the object-keyed store.
///
/// Since L1 an id assigned by a `ClassLoader` constructor native (rather than
/// by `loader_namespace_id_at`'s own allocation) IS tracked here too:
/// `remember_namespace_object` mirrors it in on first lookup. The doc used to
/// name that gap — "one only ever set through the synthetic-JDK
/// `CL_LOADER_ID` field slot, which this store doesn't track" — and it is now
/// closed in both modes.
///
/// Exists because several call sites need to *actively drive* a specific
/// loader's own `loadClass()` (JVMS §5.4.3 initiating-loader semantics) once
/// they already know a class's numeric namespace id but not the loader
/// object itself — `defining_loader_for` (a separate, narrowly-populated
/// side table keyed by `class_id`, written only by explicit
/// `register_defining_loader` calls) is NOT a reliable source for this: a
/// class defined via `ucl_try_define_local_class`'s isolated-loader native
/// path IS correctly assigned a real `UserDefined` namespace id (this store
/// IS populated for it, since `loader_namespace_id` is exactly what
/// assigned that id), independent of whether `register_defining_loader`
/// also happened to run for it.
///
/// VM-less form, test-only since the five callers outside this file
/// (`generics.rs`, `lang_class.rs` x3, `spring_startup_bootstrap.rs`) moved to
/// [`loader_object_for_namespace_id_in`] (gc-common w9 orchestrator,
/// `docs/internal/gc-common-round-20260923/applied/handoff-w9a-namespace-reverse-lookup-callers-pass-their-vm.md`).
/// It cannot promise that the object it returns lives in the caller's heap.
#[cfg(test)]
pub(crate) fn loader_object_for_namespace_id(ns_id: u32) -> Option<ObjectRef> {
    if ns_id < 3 {
        return None;
    }
    loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(_, _, id)| *id == ns_id)
        .map(|&(_, loader, _)| loader)
}

/// The namespace reverse lookup restricted to VM `vm`'s rows: the
/// loader object it returns is always one of `vm`'s own heap objects.
pub(crate) fn loader_object_for_namespace_id_in(vm: usize, ns_id: u32) -> Option<ObjectRef> {
    if ns_id < 3 {
        return None;
    }
    loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(v, _, id)| *v == vm && *id == ns_id)
        .map(|&(_, loader, _)| loader)
}

/// Stable CratonVM loader-namespace id for a `ClassLoader` instance, allocating
/// one on first request. Built-in loaders map to `0` (the Application / global
/// namespace — they ARE the global store). A user-defined loader uses the id
/// its constructor native recorded ([`LoaderMeta::loader_id`], mirrored into
/// the synthetic `CL_LOADER_ID` slot in synthetic-JDK mode) when it has one,
/// and otherwise gets one allocated here and keyed on the loader object. Used
/// by `defineClass` to give a user loader its own namespace so an
/// override-first redefinition of an already-loaded class does not collide
/// with the original definer.
pub fn loader_namespace_id(ctx: &mut dyn NativeContext, loader: ObjectRef) -> u32 {
    loader_namespace_id_at(ctx, loader, 0)
}

/// `loader_namespace_id` with the parent-walk recursion depth threaded through.
/// The depth cap only guards against a malformed `parent` cycle — real chains
/// are 2-3 deep.
fn loader_namespace_id_at(ctx: &mut dyn NativeContext, loader: ObjectRef, depth: usize) -> u32 {
    if !is_user_defined_loader(ctx, loader) && !is_bare_url_class_loader(ctx, loader) {
        // Which built-in loader `loader` actually is matters to a caller
        // reached through `parent_namespace_id`: a blanket `0` here does not
        // just mean "no id", it means "this loader's PARENT delegates to the
        // WHOLE built-in chain (Bootstrap, Extension, Application)" — see
        // `loaded_class_for_requesting_loader`'s built-in-chain fallback.
        // Collapsing the platform loader into that generic `0` let a
        // `ModifiedClassPathClassLoader` (parent = platform, specifically to
        // EXCLUDE Application from delegation) fall back to probing
        // Application anyway, silently resolving a same-named class through
        // the wrong loader — observed as the `PropertySource`/
        // `EnumerablePropertySource` cross-loader `ClassCastException`
        // family under `@ClassPathExclusions`. Identity check mirrors
        // `parent_is_platform` above: the singleton reference is the fast
        // path, the class name is the real-JDK fallback (the JDK can
        // manufacture another `PlatformClassLoader` object before our
        // singleton is observed).
        let vm = ctx.vm_identity();
        let is_platform = platform_loader_of(vm).is_some_and(|p| p.as_ptr() == loader.as_ptr())
            || ctx
                .class_name_of_id(ctx.class_id_of_object(loader))
                .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader");
        if is_platform {
            return cratonvm_types::ClassLoaderId::NATIVE_EXTENSION;
        }
        if app_loader_of(vm).is_some_and(|p| p.as_ptr() == loader.as_ptr()) {
            return cratonvm_types::ClassLoaderId::NATIVE_APPLICATION;
        }
        return 0;
    }
    if let Some(v) = loader_id_of(ctx, loader) {
        // A loader whose id came from a `ClassLoader` constructor native has
        // it in the L1 side table (and, in synthetic-JDK mode, in the field
        // slot as well), so this path never reaches the allocation below —
        // the parent link has to be recorded here too or the chain is
        // invisible for those loaders.
        //
        // Mirror it into the object-keyed store as well, or
        // `loader_object_for_namespace_id` cannot answer for this loader and
        // the "drive that loader's own loadClass" call sites lose their
        // receiver. Before L1 real-JDK mode got this for free: the
        // constructor's slot-6 write was coerced away, so the code below ran
        // and pushed the entry. Now that the id survives, push it here —
        // which also closes the same hole synthetic-JDK mode has always had
        // (`loader_object_for_namespace_id`'s doc used to name it).
        // Insert BEFORE `record_parent_link`, which recurses into this
        // function against a non-reentrant `Mutex`.
        remember_namespace_object(ctx.vm_identity(), loader, v);
        record_parent_link(ctx, loader, v, depth);
        return v;
    }
    // Resolve the parent's namespace BEFORE taking the store lock: doing it
    // afterwards re-enters this function (the parent may not have an id yet)
    // against a non-reentrant `std::sync::Mutex`.
    let parent_ns = parent_namespace_id(ctx, loader, depth);
    let vm = ctx.vm_identity();
    let mut map = loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, _, id)) = map
        .iter()
        .find(|(v, l, _)| *v == vm && l.as_ptr() == loader.as_ptr())
    {
        drop(map);
        cratonvm_classloading::register_user_loader_parent(id, parent_ns);
        return id;
    }
    let id = ctx.allocate_loader_id();
    map.push((vm, loader, id));
    drop(map);
    cratonvm_classloading::register_user_loader_parent(id, parent_ns);
    id
}

/// Record `loader → id` in the object-keyed namespace store if it is not
/// already there, so [`loader_object_for_namespace_id`] can invert an id that
/// was assigned by a `ClassLoader` constructor native rather than by
/// [`loader_namespace_id_at`]'s own allocation. Takes the lock briefly and
/// recurses into nothing. `vm` is the loader's own VM (`ctx.vm_identity()`).
fn remember_namespace_object(vm: usize, loader: ObjectRef, id: u32) {
    if id < 3 {
        return;
    }
    let mut map = loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if map
        .iter()
        .any(|(v, l, _)| *v == vm && l.as_ptr() == loader.as_ptr())
    {
        return;
    }
    map.push((vm, loader, id));
}

/// Namespace id of `loader`'s delegation parent, allocating one for the parent
/// if it does not have one yet. `0` for a built-in (or absent) parent.
///
/// Allocating eagerly matters: the child is normally the first of the pair to
/// define a class, so a lazy "record it when the parent gets an id" scheme
/// would leave the link permanently unset for exactly the case that needs it —
/// Spring's `TestCompiler` `DynamicClassLoader` over a
/// `@CompileWithForkedClassLoader` fork loader. Allocating an id for a loader
/// that never defines a class costs nothing: ids come from a counter, and the
/// class manager's `user_loaders` set is still only populated at define time.
fn parent_namespace_id(ctx: &mut dyn NativeContext, loader: ObjectRef, depth: usize) -> u32 {
    if depth >= cratonvm_classloading::MAX_USER_LOADER_DEPTH {
        return 0;
    }
    match classloader_parent(ctx, loader) {
        Some(parent) if parent.as_ptr() != loader.as_ptr() => {
            loader_namespace_id_at(ctx, parent, depth + 1)
        }
        _ => 0,
    }
}

/// Record `loader`'s parent link when its namespace id was already known.
/// A link once recorded — including the `0` that means "delegates to the
/// built-in chain" — is never recomputed, so this stays a single map probe on
/// the hot path.
fn record_parent_link(ctx: &mut dyn NativeContext, loader: ObjectRef, id: u32, depth: usize) {
    if id < 3 || cratonvm_classloading::user_loader_parent_known(id) {
        return;
    }
    let parent_ns = parent_namespace_id(ctx, loader, depth);
    cratonvm_classloading::register_user_loader_parent(id, parent_ns);
}

/// Read-only probe of a user loader's namespace id (no allocation). `None` when
/// the loader is built-in, or has not yet been assigned one (it has defined no
/// class under a distinct namespace).
pub(crate) fn peek_loader_namespace_id(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
) -> Option<u32> {
    if !is_user_defined_loader(ctx, loader) && !is_bare_url_class_loader(ctx, loader) {
        return None;
    }
    if let Some(v) = loader_id_of(ctx, loader) {
        return Some(v);
    }
    let vm = ctx.vm_identity();
    loader_namespace_id_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(v, l, _)| *v == vm && l.as_ptr() == loader.as_ptr())
        .map(|&(_, _, id)| id)
}

/// True for a JDK dynamic-proxy class's internal name (`jdk/proxyN/$ProxyM` on
/// JDK 9+, `com/sun/proxy/$ProxyM` on the legacy layout). Generated proxies are
/// never real classpath classes, so a built-in loader can only ever "find" one
/// because it leaked into CratonVM's flat global store — see the use in
/// [`find_loaded_class_for_loader`].
///
/// By generated-proxy SHAPE (`$Proxy<digits>` simple name) in any package: a
/// proxy for a non-public interface is defined in THAT interface's package
/// (`p/$Proxy3`, as `ProxyBuilder` does), which the older
/// `jdk/proxy*` / `com/sun/proxy*` prefix test missed, so the sibling-loader
/// leak guard did not cover it (round 13 wave 7, lane predicates).
/// `CRATONVM_PROXY_USER_SUBCLASS_ORDINARY=0` restores the prefix test.
pub(crate) fn is_generated_proxy_name(name: &str) -> bool {
    if !crate::reflect_annotations::proxy_user_subclass_is_ordinary() {
        return (name.starts_with("jdk/proxy") || name.starts_with("com/sun/proxy"))
            && name.contains("$Proxy");
    }
    crate::reflect_annotations::proxy_name_has_generated_shape(name)
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
    match defining_loader_for(ctx.vm_identity(), cid.as_u32()) {
        Some(def) => !loader_can_see_defining(ctx, this, def),
        None => false,
    }
}

/// Shared `findLoadedClass` logic (JVMS §5.3): returns the Class mirror for
/// `internal_name` only if `this` loader is recorded as having loaded it —
/// NEVER a class some OTHER loader happens to have loaded. Does NOT trigger
/// loading.
///
/// For a built-in loader the global loaded-class set is the right answer —
/// EXCEPT for a bare `java.net.URLClassLoader`, which is a JDK class but a
/// user-defined loader and takes the user-defined path below for every
/// non-proxy name (W7-82 / W7-87; see the long comment in
/// `find_loaded_class_for_loader_inner`). For a
/// user-defined loader, a class counts as "loaded by this loader" if either:
///   1. it lives in this loader's own namespace (a distinct copy this loader
///      defined — the override-first redefinition case), or
///   2. the globally-known class of that name records THIS loader as its
///      defining loader (the common case: e.g. ByteBuddy's `ByteArrayClassLoader`
///      defines under the Application namespace but registers itself as definer).
/// Otherwise it is not visible to this loader as "already loaded" → `None`,
/// which lets the loader's `loadClass` override proceed to `findClass`/define.
/// The `ClassId` of a class named `internal_name` that **this exact loader
/// object** is recorded as the defining loader of.
///
/// This is the strongest identity statement the VM can make about "who defined
/// it", and it is deliberately narrower than a namespace-id match. CratonVM
/// keys its class store on `(loader_id, name)`, and a `loader_id` is a
/// synthetic NAMESPACE number, not a loader: two distinct `ClassLoader` objects
/// can end up sharing one. That is not hypothetical — it is the measured shape
/// behind the Tomcat webapp stop/start family, where the ~14th
/// `WebappClassLoader` in a process started colliding with an earlier, already
/// finished one over `org/apache/catalina/loader/JdbcLeakPrevention`. On
/// HotSpot each of those loaders defines its own copy and none of them
/// conflicts.
///
/// So the two questions must not be confused:
///
/// * *"does this NAMESPACE hold the name"* — `class_id_defined_by_loader_exact`,
///   which is what the store can answer cheaply and what the duplicate-define
///   probe in the class manager uses; and
/// * *"did THIS OBJECT define it"* — this function, which is the one JVMS
///   §5.3.5 turns on and the only one that may raise a `LinkageError`.
///
/// **A `None` here is never proof of the negative.** The record is an
/// `ObjectRef` and a moving collection can leave a stale pointer, so a genuine
/// same-loader define can read back as "not recorded". Every caller must treat
/// `None` as "cannot tell" and take the permissive branch: a missed
/// `LinkageError` is the pre-existing behaviour, a spurious one is a new way to
/// break a workload.
pub(crate) fn class_defined_by_this_loader_object(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
) -> Option<cratonvm_types::ClassId> {
    // gc-common w10-f: one index look-up instead of a walk over every row of
    // every VM. Three phases, so no class-manager read (`class_name_arc_of_id`)
    // runs under the process-wide store mutex:
    //   1. under the lock, answer from the memoized names, or snapshot the ids
    //      whose names were never read;
    //   2. read those names with no lock held;
    //   3. under the lock again, publish them for the next probe.
    // A loader defining `n` classes therefore reads each name once in total,
    // not once per later miss.
    let key = (ctx.vm_identity(), this.as_ptr() as usize);
    let unread: Vec<u32> = {
        let table = defining_loader_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = table.by_loader.get(&key)?;
        if let Some(&cid) = entry.by_name.get(internal_name) {
            return Some(cratonvm_types::ClassId::new(cid));
        }
        entry.ids[entry.named..].to_vec()
    };
    if unread.is_empty() {
        return None;
    }
    let read: Vec<(u32, Option<Arc<str>>)> = unread
        .into_iter()
        .map(|cid| (cid, ctx.class_name_arc_of_id(cratonvm_types::ClassId::new(cid))))
        .collect();
    let hit = read
        .iter()
        .find(|(_, name)| name.as_deref() == Some(internal_name))
        .map(|&(cid, _)| cratonvm_types::ClassId::new(cid));
    if let Some(entry) = defining_loader_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_loader
        .get_mut(&key)
    {
        entry.publish_names(&read);
    }
    hit
}

pub(crate) fn find_loaded_class_for_loader(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
) -> Option<ObjectRef> {
    let __obsreg_dbg = crate::vmflags().loader.dbg_obsreg
        && (internal_name.contains("ObservationRegistry")
            || internal_name.contains("SecurityFilterAutoConfigurationEarlyInitializationTests")
            || internal_name.contains("PathRequestTests")
            || internal_name.contains("ManagementWebSecurityAutoConfigurationTests"));
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
    // W7-82 / W7-87 — the bare-`URLClassLoader` carve-out, both halves.
    //
    // `is_user_defined_loader` answers "is the loader's CLASS a JDK loader
    // class", and `is_builtin_loader_class` lists `java/net/URLClassLoader`
    // among them. But `URLClassLoader` is the one entry on that list with a
    // PUBLIC constructor — `java/lang/ClassLoader` is abstract and
    // `java/security/SecureClassLoader`'s constructors are protected, so an
    // instance of either is necessarily a user subclass carrying a user class
    // name. A bare `new URLClassLoader(urls, parent)` is therefore a JDK class
    // but a genuinely USER-DEFINED loader, and the namespace allocator already
    // says so: `loader_namespace_id_at` and `peek_loader_namespace_id` both
    // spell their guard `!is_user_defined_loader(..) &&
    // !is_bare_url_class_loader(..)`, so a bare `URLClassLoader` DEFINES into
    // its own namespace id (>= 3).
    //
    // This function was the one site left out of that carve-out, and the two
    // halves then disagreed in BOTH directions.
    //
    // W7-82 closed the first. The built-in branch's "a built-in loader never
    // counts as having loaded a class a user-defined loader defined" clause
    // (`loader_id_of_class(cid) > 2 -> None`) hid namespace-3 classes from the
    // very loader that had defined them, so the cache probe went blind, every
    // later lookup re-drove `define_class_full`, and a second
    // `Class.forName(name, true, loader)` surfaced as `ClassFormatError: ...
    // already defined by user-defined(3) loader` where HotSpot returns the
    // cached class.
    //
    // W7-87 closes the second — this branch's OTHER half. The GLOBAL FALLBACK
    // below also answered a bare `URLClassLoader` with any APPLICATION-namespace
    // class of that name: one it never defined and was never asked to load.
    // Measured against HotSpot 25: `new URLClassLoader(urls, null)
    // .loadClass("SomeAppClass")` returned the application loader's class where
    // HotSpot raises `ClassNotFoundException`, and `findLoadedClass` reported it
    // where HotSpot reports null. `new URLClassLoader(urls, null)` is THE
    // isolating-loader idiom, so the fallback silently defeated the isolation
    // the loader was constructed for — and `ucl_try_define_local_class`'s own
    // doc comment already named the rule it was breaking ("would let a
    // `URLClassLoader(urls, null)` resolve application classes its own (failed)
    // URL search should have hidden from it"). A `URLClassLoader` SUBCLASS was
    // correct throughout, on both arms: one line of `extends` decided it.
    //
    // So a bare `URLClassLoader` now takes the USER-DEFINED branch below
    // outright — exactly the predicate the namespace allocator uses. That
    // branch's steps 1 and 2 ARE W7-82's two additive probes, verbatim, so
    // nothing that half fixed is given up; what changes is that a miss now ends
    // in `None` instead of the global fallback. Unlike W7-82 this direction is
    // a NARROWING, with real blast radius — see
    // W7-87-urlclassloader-namespace-asymmetry.md.
    //
    // ONE case stays on the built-in branch: a GENERATED PROXY name. That arm is
    // already loader-identity- and delegation-aware (`proxy_hidden_from` ->
    // `loader_can_see_defining`), so it is not a leak, and
    // `classloader_real::load_class_visible_to` short-circuits proxy resolution
    // to this function BEFORE parent delegation runs — dropping it here would
    // turn a proxy a bare `URLClassLoader` can legitimately see through its
    // parent into a `ClassNotFoundException`. One axis at a time; proxy
    // visibility is not this lane's.
    let takes_builtin_branch = !is_user_defined
        && (is_generated_proxy_name(internal_name) || !is_bare_url_class_loader(ctx, this));
    if takes_builtin_branch {
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
            //
            // The three built-in loaders are NOT mutually visible either: they
            // form a strict ancestor chain (Bootstrap -> Extension/Platform ->
            // Application), and `findLoadedClass` must only report a class
            // defined by `this` or one of `this`'s OWN ancestors — never a
            // descendant's. A blanket `> 2` here treated Bootstrap, Extension,
            // and Application as one undifferentiated group: asking the
            // PLATFORM loader whether it has an application class "loaded"
            // (as happens on every `super.loadClass` delegation from a loader
            // parented to platform — e.g. a `ModifiedClassPathClassLoader`,
            // Spring's `@ClassPathExclusions` isolation) found the app
            // loader's pre-existing copy and returned it, so the isolated
            // loader's `loadClass` never reached its own `findClass` to define
            // a fresh one — a same-named class split across two loaders,
            // observed as the `PropertySource`/`EnumerablePropertySource`
            // family's `ClassCastException` under `@ClassPathExclusions`.
            // `builtin_loader_ordinal(this) == None` (bootstrap has no `this`
            // object in practice, or the singleton could not be identified)
            // keeps the old permissive bound as a safe fallback.
            let candidate_ordinal = ctx.loader_id_of_class(cid);
            if candidate_ordinal > 2 {
                return None;
            }
            if let Some(this_ordinal) = builtin_loader_ordinal(ctx, this) {
                if candidate_ordinal > this_ordinal as i32 {
                    return None;
                }
            }
            if let Some(def) = defining_loader_for(ctx.vm_identity(), cid.as_u32()) {
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
    if let Some(cid) = class_defined_by_this_loader_object(ctx, this, internal_name) {
        return Some(ctx.get_class_mirror(cid));
    }
    // 2. A globally-known class THIS loader is the defining loader of.
    if let Some(cid) = ctx.class_id_by_name(internal_name) {
        if let Some(def) = defining_loader_for(ctx.vm_identity(), cid.as_u32()) {
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
                exc?,
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

pub(crate) fn classloader_parent(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
) -> Option<ObjectRef> {
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
    // L1: the slot fallback is for OUR layout only. On the real layout slot 1
    // is `java.lang.ClassLoader.name` — a String — so this read handed the
    // platform loader's own name back as its PARENT, and every caller that
    // walks the chain (`builtin_loader_reachable`, `parent_namespace_id`,
    // Tomcat's `while (j.getParent() != null)`) then treated a String as a
    // ClassLoader. On a real layout a null by-name `parent` is the truth:
    // nothing but real bytecode and the by-name writes above ever sets it.
    if !cl_has_synthetic_layout(ctx, loader) {
        return None;
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
    if is_defining_loader_orphaned(ctx.vm_identity(), cid.as_u32()) {
        return None;
    }
    if let Some(def) = defining_loader_for(ctx.vm_identity(), cid.as_u32()) {
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
    // gen r4w3/rooting: `ensure_class_initialized` runs `<clinit>` (GC
    // point); re-read `this` before the visibility check uses it.
    let this_pin = ctx.pin_native_root(this);
    let initialized = ctx.ensure_class_initialized(internal);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    match initialized {
        Ok(cid) => Ok(cid_visible_mirror(ctx, this, cid)),
        Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ClassFile(
                cratonvm_types::error::ClassFileError::ClassNotFound { class_name },
            ),
        )) if class_name != internal
            && cratonvm_classloading::array_descriptor_element_class(internal)
                != Some(class_name.as_str()) =>
        {
            tracing::debug!(
                requested = internal,
                missing_dependency = %class_name,
                "resolve_global_if_visible: requested class exists but a \
                 dependency failed to resolve -- NoClassDefFoundError"
            );
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                crate::classloader_real::no_class_def_found_error(ctx, &class_name)?,
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
        crate::vmflags().loader.dbg_obsreg && internal.contains("ObservationRegistry");
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

/// GC-SAFETY wrapper. The delegation body below dispatches arbitrary Java
/// several times — the parent's `loadClass`, the receiver's `findClass`
/// override, `ensure_class_initialized`'s `<clinit>`, and exception
/// construction — and keeps using `this` and `name_obj` afterwards. Both are
/// bare Rust locals: `safe_native_call` pins the native's ARGS and the
/// collector remaps those pins, but nothing rewrites these copies. Hibernate
/// reaches exactly this path through `ClassLoaderServiceImpl.classForName` ->
/// `AggregatedClassLoader` (which is `super(null)` and overrides `findClass`
/// to iterate scoped child loaders), which is where `CRATONVM_DBG_STALE_OBJREF`
/// caught a stale deref. Root both for the whole body and re-read them after
/// every dispatch. See
/// map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md.
fn cl_load_class_base_delegation_inner(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name_obj: ObjectRef,
    internal: &str,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let name_pin = ctx.pin_native_root(name_obj);
    let result =
        cl_load_class_base_delegation_rooted(ctx, this, this_pin, name_obj, name_pin, internal);
    ctx.unpin_native_roots(this_pin);
    result
}

fn cl_load_class_base_delegation_rooted(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    this_pin: usize,
    name_obj: ObjectRef,
    name_pin: usize,
    internal: &str,
) -> MethodCallResult {
    // gen r4w3/rooting: function-scoped `mut` bindings so every post-GC-point
    // re-read below reaches all later uses instead of dying at a block brace.
    let mut this = this;
    let mut name_obj = name_obj;
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
            || platform_loader_of(ctx.vm_identity())
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
        && !ctx.is_bootstrap_appended_class(&internal)
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
        && !ctx.is_bootstrap_appended_class(&internal)
        && !builtin_loader_reachable(ctx, this);
    // JVM spec §5.3.2 — parent-first delegation:
    // 1. Check if this loader already loaded the class (findLoadedClass)
    let loader_type = loader_type_of(ctx, this).unwrap_or(LOADER_APP);

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
        let loader_id = loader_id_of(ctx, this);
        if let Some(lid) = loader_id {
            if let Some(cid) = ctx.class_id_by_name_and_loader(&internal, lid) {
                if let Some(mirror) = cid_visible_mirror(ctx, this, cid) {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
            }
        }
    }

    // A URLClassLoader parented only by bootstrap/platform is intentionally
    // isolated from application entries. Spring Boot's
    // ModifiedClassPathClassLoader uses this topology to remove selected JARs
    // from a test's classpath. Do this before the flat global-store fallback:
    // otherwise `super.loadClass` from its override returns the application
    // loader's same-named class, and Spring's ASM metadata resolves annotation
    // types through a different loader than the condition class itself.
    //
    // Keep bootstrap-appended classes on their normal parent-delegation path;
    // Mockito's injected dispatcher must remain bootstrap-defined.
    // In real-JDK mode an inherited URLClassLoader relationship is not always
    // visible through `object_extends` while dispatching a subclass override.
    // Its constructor URLs are retained independently and are the authoritative
    // signal that this loader has a private URL search path.
    let has_private_url_path = !loader_constructor_url_paths(ctx, this).is_empty();
    if (url_classloader_isolated_from_app(ctx, this)
        || ((parent_is_null || parent_is_platform) && has_private_url_path))
        && !is_bootstrap_class_name(&internal)
        && !ctx.is_bootstrap_appended_class(&internal)
    {
        if let Some(result) = ucl_try_define_local_class(ctx, this, &internal) {
            return result;
        }
        let exception = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &internal,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exception?,
        ));
    }

    // 2. Delegate to parent loader first (recursive parent-first delegation)
    if let Some(parent) = parent {
        // Recursively delegate to parent by calling its loadClass
        let parent_type = loader_type_of(ctx, parent).unwrap_or(LOADER_APP);
        let parent_lid = loader_id_of(ctx, parent);
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
            let delegated = ctx.invoke_virtual(
                parent,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[Value::Object(Some(name_obj))],
            );
            // The dispatch above ran arbitrary Java: everything captured
            // before it is a pre-move address on the fall-through path.
            // gen r4w3/rooting: ASSIGN (not shadow) so the refresh reaches
            // steps 2-6 below, outside this block.
            this = ctx.read_native_pin(this_pin, this);
            name_obj = ctx.read_native_pin(name_pin, name_obj);
            match delegated {
                Ok(Some(Value::Object(Some(mirror)))) => {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
                // W7-26 R1 -- the synthetic-mode twin of the narrowing applied to
                // `classloader_real.rs`'s step 0. JDK 25 `ClassLoader.loadClass`
                // wraps its parent delegation in exactly one `catch
                // (ClassNotFoundException)`; the bare `_ =>` also caught a
                // `LinkageError` and every `RuntimeException` the parent raised
                // and reported the class as merely absent, turning a diagnosable
                // failure into a wrong answer. `absorb_class_absent` keeps the
                // fall-through for the two class-absent shapes only, tested by
                // `ClassId` hierarchy rather than by name.
                //
                // The two `read_native_pin` refreshes above this `match` are
                // GC-correctness, not style: the `invoke_virtual` ran arbitrary
                // Java and every address captured before it is a pre-move one on
                // the fall-through path.
                Ok(_) => {}
                Err(failed) => {
                    crate::classloader_real::absorb_class_absent(&*ctx, failed)?;
                }
            }
        }
        // For built-in parent loaders (bootstrap/platform/app), use standard delegation
        if parent_type != LOADER_CUSTOM && !defer_to_find_class && !scoped_user_chain {
            // Standard delegation handles bootstrap → extension → app
            if let Some(mirror) = resolve_global_if_visible(ctx, this, &internal)? {
                return Ok(Some(Value::Object(Some(mirror))));
            }
            // gen r4w3/rooting: `<clinit>` may have run; refresh.
            this = ctx.read_native_pin(this_pin, this);
        }
    } else if !defer_to_find_class && !scoped_user_chain {
        // No parent (or null parent) → delegate directly to bootstrap loader
        // Bootstrap delegation: use the standard class loading chain
        if let Some(mirror) = resolve_global_if_visible(ctx, this, &internal)? {
            return Ok(Some(Value::Object(Some(mirror))));
        }
        // gen r4w3/rooting: `<clinit>` may have run; refresh.
        this = ctx.read_native_pin(this_pin, this);
    }

    // 3. Parent couldn't find it — fall back to standard loading
    //    (this covers bootstrap → extension → application delegation).
    //    Skipped when deferring to a custom `findClass` override (HIB-CV-24) so
    //    the supplied loader runs before CratonVM's global store answers.
    if !defer_to_find_class && !scoped_user_chain {
        if let Some(mirror) = resolve_global_if_visible(ctx, this, &internal)? {
            return Ok(Some(Value::Object(Some(mirror))));
        }
        // gen r4w3/rooting: `<clinit>` may have run; refresh.
        this = ctx.read_native_pin(this_pin, this);
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
    // gen r4w3/rooting: the local URL search may fetch / define (GC-capable)
    // even on a miss; refresh both locals before `findClass`.
    this = ctx.read_native_pin(this_pin, this);
    name_obj = ctx.read_native_pin(name_pin, name_obj);

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
        // `findClass` is the loader's own Java override (Hibernate's
        // AggregatedClassLoader iterates its scoped child loaders here) —
        // refresh before the fall-through arms reuse either local.
        // gen r4w3/rooting: ASSIGN so the refreshed `this` reaches steps 5/6
        // (`impl_jars_load_class`) below, outside this block.
        this = ctx.read_native_pin(this_pin, this);
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
    if let Ok(Some(mirror)) = impl_jars_load_class(ctx, Some(this), &internal) {
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
                exc?,
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
    // gen r4w3/rooting: the `findClass` upcall, the IMPL-JARS define and
    // `ensure_class_initialized` (`<clinit>`) are all GC points and `this` is
    // used after each; keep it rooted and re-read.
    let this_pin = ctx.pin_native_root(this);
    if receiver_overrides_find_class(ctx, this) {
        let result = ctx.invoke_virtual_bytecode_only(
            this,
            "findClass",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;",
            &[module_name, Value::Object(Some(name_obj))],
        );
        if matches!(result, Ok(Some(Value::Object(Some(_))))) {
            ctx.unpin_native_roots(this_pin);
            return result;
        }
    }
    let this = ctx.read_native_pin(this_pin, this);

    // A module-aware lookup can be the first request for an embedded
    // implementation dependency, so share loadClass/findClass(String)'s
    // IMPL-JARS fallback here as well.
    if let Ok(Some(mirror)) = impl_jars_load_class(ctx, Some(this), &internal) {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(Some(mirror))));
    }

    let initialized = ctx.ensure_class_initialized(&internal);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    match initialized {
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
        Some(Value::Int(_)) => return Err(RuntimeError::aioobe_index_only(-1).into()),
        _ => 0,
    };
    let length = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(_)) => return Err(RuntimeError::aioobe_index_only(-1).into()),
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
        return Err(RuntimeError::aioobe_index_only(-1).into());
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
            return Err(cratonvm_types::error::LinkageError::ClassFormatError {
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
        return Err(cratonvm_types::error::LinkageError::ClassFormatError {
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

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    // gen r4w3/rooting: `define_class_full` below resolves supertypes (GC
    // point) and `this` is used afterwards to record the defining loader;
    // root it here and re-read after each step that can collect.
    let this_pin = ctx.pin_native_root(this);
    let pd_url = match args.get(5) {
        Some(Value::Object(Some(pd))) => extract_pd_code_source_url(ctx, *pd),
        _ => None,
    };
    let this = ctx.read_native_pin(this_pin, this);

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
            return Err(cratonvm_types::error::LinkageError::ClassFormatError {
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
            // gen r4w3/rooting: re-read after `define_class_full`.
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            crate::classloader::register_defining_loader(ctx.vm_identity(), cid.as_u32(), this);
            let count = loader_classes_loaded_of(ctx, this).unwrap_or(0);
            loader_set_classes_loaded(ctx, this, count + 1);
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            ctx.unpin_native_roots(this_pin);
            if msg.starts_with("Runtime(OutOfMemoryError") {
                // gc-common w6-f: `-XX:MaxMetaspaceSize` refused the define
                // (`vm_exec.rs::define_class_full`) -- throw, as HotSpot does,
                // instead of returning null.
                return Err(RuntimeError::OutOfMemoryError {
                    message: "Metaspace".into(),
                }
                .into());
            }
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

/// `java.net.URL.toString()` for a URL we must not (or cannot) call bytecode
/// on — the CodeSource-location readers below and in `lookup_define` run on
/// the class-definition path, where `invoke_virtual("toExternalForm")` is not
/// available.
///
/// Measured (`java UrlProbe`, OpenJDK 25.0.3). `URLStreamHandler.toExternalForm`
/// is `protocol + ":" + ["//" + authority] + file + ["#" + ref]`, where `file`
/// is already `path + "?" + query`:
///
/// | spec                                   | toString                               | `protocol:file` alone |
/// |----------------------------------------|----------------------------------------|-----------------------|
/// | `file:/C:/repo/lib/foo.jar`            | `file:/C:/repo/lib/foo.jar`            | same                  |
/// | `file:///C:/repo/lib/foo.jar`          | `file:/C:/repo/lib/foo.jar`            | same                  |
/// | `jar:file:/C:/repo/lib/foo.jar!/`      | `jar:file:/C:/repo/lib/foo.jar!/`      | same                  |
/// | `jar:file:/o/lib/bar.jar!/com/x/Y.class`| `jar:file:/o/lib/bar.jar!/com/x/Y.class`| same                 |
/// | `file://server/share/x.jar`            | `file://server/share/x.jar`            | `file:/share/x.jar`   |
/// | `http://example.com:8080/a/b?q=1#frag` | `http://example.com:8080/a/b?q=1#frag` | `http:/a/b?q=1`       |
/// | `https://user@host/p`                  | `https://user@host/p`                  | `https:/p`            |
///
/// So `protocol + ":" + file` — what this module used to inline — is right for
/// exactly the classpath shapes and wrong for everything with an authority or
/// a fragment. The `authority` field, not `host`, is the one that round-trips:
/// `https://user@host/p` has `host == "host"` but `authority == "user@host"`.
///
/// Layout: real `java.net.URL` declares `protocol`(0) `host`(1) `port`(2)
/// `file`(3) `query`(4) `authority`(5) `path`(6) `userInfo`(7) `ref`(8)
/// (`javap -p java.net.URL`, JDK 25), and this crate's 13-slot synthetic URL
/// mirrors those indices — hence the by-name-then-slot reads. The LEGACY
/// 6-slot synthetic instead cached the whole spec in slots 0 and 5; a
/// protocol that reads back containing `':'` is that shape, and is returned
/// verbatim rather than being re-prefixed.
pub(crate) fn url_to_external_form(ctx: &dyn NativeContext, url: ObjectRef) -> Option<String> {
    // The location may already be a String rather than a URL.
    if let Some(s) = ctx.read_string(url) {
        return Some(s);
    }
    let read = |name: &str, slot: usize| -> Option<String> {
        match ctx.get_field_by_name(url, name) {
            Value::Object(Some(s)) => ctx.read_string(s),
            // A failed by-name read is never proof the field is null, so the
            // numeric fallback has to be tried regardless. This comment used
            // to say the reason was that an ABSENT field answers `Int(0)`;
            // that is the `MockNativeContext` behaviour, not production's.
            // Production (`vm_exec.rs::get_field_by_name`, and the trait
            // contract in `native-api/src/registry.rs`) answers
            // `Value::Object(None)` for a field it cannot resolve — which is
            // BYTE-FOR-BYTE the same answer as a present reference field that
            // happens to be null. The correct rationale is therefore the
            // stronger one: "by-name miss" and "field is genuinely null" are
            // indistinguishable from the value, so no `Object(None)` result
            // may be read as a layout verdict.
            _ => match ctx.get_field(url, slot) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            },
        }
    };
    // NOT `?`: a legacy 6-slot URL leaves slot 0 null and caches the whole
    // spec in slot 5 only, so "no protocol" is a shape to handle, not a
    // failure. (`jboss_module_loader`'s `classpath:/…` resource URLs are
    // exactly that shape.)
    let protocol = read("protocol", 0).unwrap_or_default();
    if protocol.contains(':') {
        // Legacy 6-slot synthetic URL with the full spec cached in slot 0.
        return Some(protocol);
    }
    if protocol.is_empty() {
        // Legacy 6-slot synthetic URL with the full spec cached in slot 5.
        return match ctx.get_field(url, 5) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        };
    }
    let mut out = String::with_capacity(protocol.len() + 32);
    out.push_str(&protocol);
    out.push(':');
    // `authority` is only at slot 5 on the real/13-slot layouts; the legacy
    // shape was already returned above, so the numeric read is safe here.
    match read("authority", 5) {
        Some(auth) if !auth.is_empty() => {
            out.push_str("//");
            out.push_str(&auth);
        }
        _ => {
            // No `authority` field (older synthetic): rebuild it from
            // host[:port], the way `net_uri_inet::url_external_form` does.
            let host = read("host", 1).unwrap_or_default();
            if !host.is_empty() {
                out.push_str("//");
                out.push_str(&host);
                let port = match ctx.get_field_by_name(url, "port") {
                    Value::Int(p) => p,
                    _ => match ctx.get_field(url, 2) {
                        Value::Int(p) => p,
                        _ => -1,
                    },
                };
                if port >= 0 {
                    out.push(':');
                    out.push_str(&port.to_string());
                }
            }
        }
    }
    if let Some(file) = read("file", 3) {
        out.push_str(&file);
    }
    if let Some(r) = read("ref", 8) {
        out.push('#');
        out.push_str(&r);
    }
    Some(out)
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
            // W7-7 had to gate a raw slot-5 read here on a class-side
            // `authority` witness, because slot 5 is the full-spec cache on the
            // legacy synthetic URL and the `authority` FIELD on a real one —
            // and `read_string` succeeds on both. `url_to_external_form` makes
            // that distinction once, for every caller, and additionally returns
            // the real URL's full external form instead of nothing.
            if let Some(s) = url_to_external_form(ctx, loc) {
                return Some(s);
            }
        }
    }
    // Real-JDK PD path: field 0 may not match. Try by-name.
    if let Value::Object(Some(cs)) = ctx.get_field_by_name(pd, "codesource") {
        if let Value::Object(Some(loc)) = ctx.get_field_by_name(cs, "location") {
            // Real `CodeSource.location` is typed `java.net.URL`, not
            // `String` — `read_string` correctly fails on it (it's a
            // different concrete class), which silently dropped every
            // real-JDK-constructed CodeSource's URL here (e.g.
            // `URLClassLoader.defineClass(name, Resource)`'s
            // `new CodeSource(url, signers)`, the path
            // `ModifiedClassPathClassLoader`/`@ClassPathOverrides` uses to
            // load an overridden jar's classes — see
            // `NoSuchMethodFailureAnalyzerTests`). Reconstruct the URL string
            // from its own real fields. This used to inline
            // `protocol + ":" + file`, which is `toString()` only while the
            // authority and ref are both absent — see `url_to_external_form`
            // for the measured table and the two shapes it got wrong.
            if let Some(s) = url_to_external_form(ctx, loc) {
                return Some(s);
            }
        }
    }
    None
}

// W7-13: `read_byte_buffer_slice` lived here — a SECOND `defineClass2`
// ByteBuffer decoder, and the one that actually ran, because this module's
// `defineClass2` registration shadows `lang_system`'s in synthetic-JDK mode.
// It hardcoded slot 0 as the backing `byte[]` (on a real `HeapByteBuffer`
// slot 0 is `Buffer.mark`, an int, so every real heap buffer fell through to
// the direct arm and died on "no native address") and it CLAMPED an
// out-of-range `(off, len)` with `min`/`saturating_add` instead of rejecting
// it. Both defects were already fixed in
// `lang_system::read_byte_buffer_define_class_slice`; the fix was inert
// wherever the shadow won. `cl_define_class2` now calls that decoder, so
// there is exactly one.

/// Bind the loader-id used to register the new class. We look up the
/// loader's recorded namespace id if it has one (lazily allocating a fresh
/// id), otherwise fall through to id 0 (= application loader). A null
/// loader is treated as the bootstrap class loader, which the backend
/// also models as id 0 in this VM.
/// Whether a HIDDEN class defined through a lookup on `lookup_mirror` (or,
/// with no lookup class, for `loader`) belongs in the BOOTSTRAP namespace
/// ([`cratonvm_native_api::DefineClassFull::bootstrap_namespace`]): the lookup
/// class is one the VM tags bootstrap (a `java.base` class, or another JDK
/// class the VM defined under the bootstrap key), or there is no lookup class
/// and the loader is the null one. HotSpot's `JVM_LookupDefineClass` defines a
/// hidden class in its lookup class's loader, module and runtime package; the
/// `0` loader id every built-in loader collapses to here is the APPLICATION
/// namespace, which put `java.lang.String$$StringConcat` (`StringConcatFactory`,
/// a full-power lookup on `String`) in another runtime package than the
/// package-private `StringConcatHelper$StringConcatBase` it extends, and
/// its spin failed with `IllegalAccessError`. Both modes. Interpreter round i1
/// wave 44, lane L5 (probe `tools/probes/interp/L4/L4W43CopyWithViews.java`,
/// `concat-*`).
pub(crate) fn hidden_define_in_bootstrap_namespace(
    ctx: &mut dyn NativeContext,
    lookup_mirror: Option<ObjectRef>,
    loader: Value,
) -> bool {
    let bootstrap = match lookup_mirror.and_then(|m| crate::lang_class::mirror_class_id(ctx, m)) {
        Some(cid) => {
            ctx.loader_id_of_class(cid) == cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP as i32
        }
        None => matches!(loader, Value::Object(None)),
    };
    if bootstrap && cratonvm_types::flags().loader.dbg_define {
        eprintln!("[DEFINE-DBG] hidden define in the bootstrap namespace (bootstrap lookup class)");
    }
    bootstrap
}

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
/// `CRATONVM-SPRING-GENUINE-BUGLIST`'s Groovy cluster
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
    // GC: a reference held in a Rust local across an allocating or Java-re-entering
    // call goes stale under a moving collector, and under the Generational
    // non-moving young sweep an unrooted object is ZEROED in place. Pin and
    // re-read. `safe_native_call_impl` truncates `native_pin_roots` when the native
    // returns, so an unmatched pin costs nothing on an error path. See
    // `internal/audits/wide-tranche-triage-20260907.md`.
    // `define_class_full` defines a class and `get_class_mirror` below
    // allocates the mirror, so BOTH incoming references — the `classData`
    // object and the loader — are stale by the time they are stored and
    // registered. The audit reported `loader`; reading found `class_data` too.
    let class_data_pin = match class_data {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let loader_pin = match loader {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    // gc-common w18-d: read before `opts` moves into the backend.
    let non_strong_hidden = opts.unloads_with_its_mirror();
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(name, &bytes, loader_id, opts)
    }));
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!("[define_class] panic inside define_class_full for {name}; aborting");
            return Err(cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: "defineClass: panic inside backend (likely malformed bytecode)".into(),
            }
            .into());
        }
    };
    match define_result {
        Ok(cid) => {
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
            //
            // MUST run before `get_class_mirror` below: `class_mirror_impl`
            // consults `defining_loader_for(cid)` at mirror-creation time —
            // the ONLY time it is ever consulted for this mirror, since the
            // mirror is cached from then on — to decide whether to record a
            // `mirror_pin` row. `get_class_mirror` before this registration
            // means `defining_loader_for` still answers `None` when the
            // mirror is minted, so no row is EVER recorded (there is no
            // retroactive backfill) and `roots.rs` step 6 falls back to
            // unconditionally rooting the mirror forever — permanently
            // defeating unloading for every class defined this way
            // (`ClassLoader.defineClass`: Jasper JSP compilation, Groovy
            // script classes, CGLIB proxies, ...). Confirmed via
            // `CRATONVM_DBG_MIRRORPIN`: `TestDefaultInstanceManager`'s
            // evicted JSP class mirror traced `pinned=false => ROOTED`
            // every cycle, so its annotation-cache entry could never be
            // reclaimed.
            if loader_aware_resolution() {
                if let Value::Object(Some(loader_obj)) = loader {
                    let loader_obj = match loader_pin {
                        Some((p, o)) => ctx.read_native_pin(p, o),
                        None => loader_obj,
                    };
                    if is_user_defined_loader(ctx, loader_obj) {
                        register_defining_loader(ctx.vm_identity(), cid.as_u32(), loader_obj);
                    }
                }
            }
            let mirror = ctx.get_class_mirror(cid);
            // classData (defineClass0 path) goes into the mirror's own
            // `Class.classData` field, read back through its pin.
            if let Some((p, o)) = class_data_pin {
                let data = ctx.read_native_pin(p, o);
                attach_class_data(ctx, mirror, data);
            }
            // Eager-init request: run <clinit> now (defineClass0 path
            // when `initialize == true`). `ctx.initialize_class` can
            // allocate and trigger a moving GC, so `mirror` (a raw
            // `ObjectRef` captured above and returned again below) must be
            // rooted across the call — same Family-1 stale-ObjectRef
            // pattern as the sibling `lk_ensure_initialized` fix. See
            // wildfly-parallel-boot-stale-objectref-residual.md.
            let mirror_pin = ctx.pin_native_root(mirror);
            if initialize {
                // `initialize = true` is a CONTRACT, not a hint. HotSpot's
                // `JVM_LookupDefineClass` links AND initializes before handing the
                // class back, and an initialization failure reaches the caller:
                // the original `Error`, or `ExceptionInInitializerError` wrapping
                // anything else (JVMS 5.5).
                //
                // This used to `tracing::warn!` and fall through, returning the
                // mirror as though `<clinit>` had succeeded. The caller then held
                // an UNINITIALIZED class and the real failure surfaced later,
                // somewhere with no visible connection to the define — the
                // silent-wrong-result shape this file's own error contract
                // (`cl_unsafe_define_class`'s note) argues against.
                //
                // Propagating `MethodCallFailed` unchanged is deliberate: it
                // carries the exception the initializer actually raised, so the
                // caller sees what HotSpot would have thrown rather than a
                // `ClassFormatError` about the define step. The sibling body in
                // `lang_system.rs` wraps instead, and that wrapping is a separate
                // deviation with three consumers keyed on its message text — see
                // `docs/internal/jdk-only/lane-2-system1-hidden-class-self-reference-FIXED-20260912.md`.
                //
                // `safe_native_call_impl` truncates `native_pin_roots` when the
                // native returns, so the unmatched `mirror_pin` above costs
                // nothing on this path — the same reasoning the pin's own comment
                // records for the other error arms.
                if let Err(e) = ctx.initialize_class(cid) {
                    tracing::warn!(
                        "defineClass0 initialize: <clinit> for {name} failed; propagating"
                    );
                    return Err(e);
                }
            }
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            ctx.unpin_native_roots(mirror_pin);
            // gc-common w18-d: last, after `<clinit>` and with nothing left
            // to allocate: from here the class lives as long as its mirror
            // and its instances, and the mirror is this native's return value.
            if non_strong_hidden {
                register_non_strong_hidden_class(ctx, cid);
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
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    let len = match read_nonneg_int(args, 4) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    let bytes = read_byte_array_slice(ctx, byte_array, off, len).map_err(|_msg| {
        cratonvm_types::error::MethodCallFailed::from(RuntimeError::aioobe_index_only(
            (off as i32).max(0),
        ))
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
    use cratonvm_types::error::RuntimeError;

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
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    let len = match read_nonneg_int(args, 4) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    // Decoded by `lang_system::read_byte_buffer_define_class_slice` — the SAME
    // decoder this crate's other `defineClass2` registration uses. See the
    // comment on that function: this entry point shadows that registration in
    // synthetic-JDK mode, so a private copy here meant the bounds hardening was
    // silently inert wherever the shadow won.
    let bytes = crate::lang_system::read_byte_buffer_define_class_slice(ctx, bb, off, len, &name)?;

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
/// `STRONG_LOADER_LINK` ties a hidden class's lifetime to its defining
/// loader — see `java.lang.invoke.MethodHandles.Lookup.ClassOption.STRONG`.
/// A hidden class defined WITHOUT it unloads with its mirror
/// (`DefineClassFull::non_strong_hidden`, [`register_non_strong_hidden_class`],
/// gc-common w18-d); see
/// `docs/internal/gc-common-round-20260923/common-w8e-non-strong-hidden-classes-unload-only-with-their-loader-FIXED-20260923.md`.
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
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    let len = match read_nonneg_int(args, 5) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    let bytes = read_byte_array_slice(ctx, byte_array, off, len).map_err(|_msg| {
        cratonvm_types::error::MethodCallFailed::from(RuntimeError::aioobe_index_only(
            (off as i32).max(0),
        ))
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
    // gc-common w18-d: a hidden class without STRONG_LOADER_LINK unloads with
    // its mirror (`define_class_via_full` registers it after the define).
    opts.non_strong_hidden = (flags & DEFINE_CLASS0_FLAG_STRONG) == 0;
    let _vm_anns = (flags & DEFINE_CLASS0_FLAG_VM_ANNOTATIONS) != 0; // advisory

    // Slot 9: Object classData (may be null)
    let class_data = match args.get(9) {
        Some(Value::Object(Some(_))) => Some(args[9]),
        _ => None,
    };

    // A hidden class of a bootstrap-tagged lookup class is defined in the
    // bootstrap namespace, as HotSpot defines it in its lookup class's loader
    // (`hidden_define_in_bootstrap_namespace`; interpreter round i1 wave 44).
    if opts.hidden && hidden_define_in_bootstrap_namespace(ctx, lookup_mirror, loader) {
        opts.bootstrap_namespace = true;
        opts.privileged_define = true;
    }
    let loader_id = loader_id_for(ctx, loader);
    define_class_via_full(
        ctx, &name, bytes, loader_id, opts, initialize, class_data, loader,
    )
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

    // `defineClass` is a JavaLangAccess method, and that interface's carrier
    // class is not spelled the same on every JDK -- see
    // `shared_secrets_bridge::JLA_CARRIER_CANDIDATES`. Register on each
    // candidate; the ones that are not the carrier on this image have no class
    // for anything to dispatch through.
    //
    // **Spelled inline rather than looped over that const, deliberately.**
    // `native-builtins/tests/registrar_drift.rs` scans this file as TEXT: its
    // `parse_loops` accepts `for x in [ .. ]` over literal elements and skips
    // any `for` whose `in` is not followed by `[`. Written as
    // `for carrier in JLA_CARRIER_CANDIDATES` this registration becomes
    // invisible to it -- measured, on 2026-09-09: total drift pairs 1356 ->
    // 1355 and `the_drift_baseline_has_no_stale_rows` reporting this very call
    // as "no longer drift[ing] ... registered by: <not registered anywhere
    // this scan can see>". The registration had not stopped happening; the
    // gate had stopped seeing it, and taking the regenerated baseline would
    // have retired a live row on a parser's blind spot.
    //
    // `jla_define_class_is_registered_on_every_carrier_candidate` below keeps
    // this list and that const from drifting apart, since the compiler cannot.
    for carrier in ["java/lang/System$1", "java/lang/System$2"] {
        r.register(
            carrier,
            "defineClass",
            "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BLjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
            jla_system_define_class,
        );
    }
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
        return Err(cratonvm_types::error::LinkageError::ClassFormatError {
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
        return Err(RuntimeError::aioobe_index_only(
            offset.saturating_add(length).min(i32::MAX as usize) as i32,
        )
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
            return Err(cratonvm_types::error::LinkageError::ClassFormatError {
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
        return Err(cratonvm_types::error::LinkageError::ClassFormatError {
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

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(6) {
        Some(Value::Object(Some(pd))) => extract_pd_code_source_url(ctx, *pd),
        _ => None,
    };

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
            return Err(cratonvm_types::error::LinkageError::ClassFormatError {
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
    // L1: `CL_NAME_REF` is slot 2, which on the real layout is
    // `unnamedModule` — reading it back handed a `Module` to a caller
    // expecting a `String`. The real `name` is slot 1 and is written BY NAME,
    // so ask for it that way on that layout and keep the synthetic slot for
    // ours. (The by-name read is deliberately NOT tried first on our own
    // layout: `resolve_field_index_in_hierarchy` answers the MOST-DERIVED
    // declaration, so a user loader that happens to declare its own `name`
    // field would shadow the loader name — the trap `native_enum_name` fell
    // into.)
    let name_val = if cl_has_synthetic_layout(ctx, this) {
        ctx.get_field(this, CL_NAME_REF)
    } else {
        ctx.get_field_by_name(this, "name")
    };
    if let Value::Object(Some(_)) = name_val {
        Ok(Some(name_val))
    } else {
        // Return loader type as name if no explicit name set
        let lt = loader_type_of(ctx, this).unwrap_or(LOADER_CUSTOM);
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
    Ok(Some(Value::Object(Some(app?))))
}

fn cl_get_platform_class_loader(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let platform = get_or_create_platform_loader(ctx);
    Ok(Some(Value::Object(Some(platform?))))
}

/// Public re-export of the `getResource` (singular) native so
/// `register_essential_natives` can install it in real-JDK mode. Without
/// this the JDK's own `ClassLoader.getResource` runs — and in real-JDK
/// mode the URLClassPath `<clinit>` swallow leaves the loader's resource
/// tables empty, so it returns null even for resources the bulk
/// `getResources` enumerator finds. Wave-1 Task B consistency fix.
pub fn cl_get_resource_essential(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if jdk_only_user_loader_receiver(ctx, args) {
        return ctx.invoke_special_bytecode_only(
            "java/lang/ClassLoader",
            "getResource",
            "(Ljava/lang/String;)Ljava/net/URL;",
            args,
        );
    }
    cl_get_resource(ctx, args)
}

/// `--jdk-only`: a USER-DEFINED loader's `getResource`/`getResources`/
/// `getResourceAsStream` run the JDK's own `ClassLoader` body. That body asks
/// the parent and then the loader's own `findResource(s)`, which for a
/// `URLClassLoader` is the real `URLClassPath` and so the URLs' own handlers,
/// as on HotSpot. These natives answered from the VM's flat class path and
/// minted handler-less URLs instead, so Spring Boot's `LaunchedClassLoader`
/// handed out `jar:file:/app.jar!/BOOT-INF/classes/X.class` where HotSpot
/// hands out `jar:nested:/app.jar/!BOOT-INF/classes/!/X.class` carrying Spring's
/// handler. See
/// `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
///
/// Only the INSTANCE forms with a user-loader receiver, counting a BARE
/// `URLClassLoader`, which is a loader of its own there: the static
/// `getSystemResource(s)` share these callbacks with the resource name at
/// `args[0]`, and the built-in loaders' lookups stay native, because the
/// application class path is the VM's own. `--compatible` is unchanged.
fn jdk_only_user_loader_receiver(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    if !ctx.is_jdk_only() || args.len() < 2 {
        return false;
    }
    match args.first() {
        Some(Value::Object(Some(receiver))) => {
            is_user_defined_loader(ctx, *receiver) || is_bare_url_class_loader(ctx, *receiver)
        }
        _ => false,
    }
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
pub(crate) fn is_classloader_instance(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
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

/// Kill switch for the first-hit `ClassLoader.getResource` walk below.
/// `CRATONVM_GETRESOURCE_FIRST_HIT=0` restores the whole-list walk that builds
/// every matching URL and returns element 0. Default ON.
///
/// A same-binary lever, not a safety valve: the two walks are required to
/// answer identically (`class_path.rs`'s
/// `the_incremental_walk_returns_what_the_whole_list_walk_returns_first`), so
/// the only thing this flag can change is how much of the classpath was
/// touched to get there. That makes it the A/B for the cost, which is the one
/// claim a page about a throughput gap has to be able to check.
fn get_resource_first_hit_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var_os("CRATONVM_GETRESOURCE_FIRST_HIT")
                .as_deref()
                .and_then(|s| s.to_str()),
            Some("0")
        )
    })
}

/// The first classpath URL for `resource_name`, stopping at the entry that
/// answers.
///
/// The one implementation behind BOTH singular resource doors —
/// `ClassLoader.getResource` here and `Class.getResource` in `lang_class` —
/// because they had the same whole-list-then-take-element-0 shape and a fix to
/// one of them is a fix a bisect can miss on the other.
///
/// Falls back to the whole-list walk for a GLOB name, which can match several
/// names inside a single classpath entry: "the first URL this entry serves"
/// would silently drop the rest, and `resource_name_supports_incremental_scan`
/// is the predicate that knows the difference.
pub(crate) fn first_resource_url(
    ctx: &mut dyn NativeContext,
    resource_name: &str,
) -> Option<String> {
    if get_resource_first_hit_enabled()
        && ctx.resource_name_supports_incremental_scan(resource_name)
    {
        return ctx
            .next_resource_url(resource_name, 0, 0)
            .map(|(url, _, _)| url);
    }
    ctx.find_all_resource_urls(resource_name).into_iter().next()
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
    // The STATIC forms (`getSystemResource`, `getSystemResourceAsStream`) take
    // the name at index 0 with NO receiver, so the `args.len() >= 2` guard just
    // below never saw them and a null name answered null instead of throwing.
    // MEASURED in BOTH modes (`probes/ClassLoaderShadowSweep.java`): HotSpot
    // NullPointerException, this VM no-throw.
    if args.len() == 1 && !matches!(args.first(), Some(Value::Object(Some(_)))) {
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/NullPointerException",
            1,
            "ClassLoader.getSystemResource name is null",
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
        ));
    }
    if args.len() >= 2 && !matches!(args.get(1), Some(Value::Object(Some(_)))) {
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/NullPointerException",
            1,
            "ClassLoader.getResource name is null",
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
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
    // A LEADING SLASH makes the lookup FAIL; it is not stripped.
    //
    // This is the asymmetry that catches everyone: `Class.getResource` takes a
    // name that may be absolute (leading `/`) OR relative to the class's
    // package, while `ClassLoader.getResource` takes an always-absolute name
    // that must NOT begin with `/`. The JDK uses the name verbatim, so `/X`
    // simply matches nothing.
    //
    // MEASURED in BOTH modes (`probes/ClassLoaderShadowSweep.java`):
    //
    //   APP.getResource("/ClassLoaderShadowSweep.class")
    //     HotSpot  null      CratonVM  file:/.../ClassLoaderShadowSweep.class
    //
    // `trim_start_matches('/')` made the two spellings equivalent -- more
    // permissive than the JDK in the direction that HIDES a bug: code passing a
    // `Class.getResource`-shaped name to a ClassLoader works here and returns
    // null on every other VM.
    //
    // Scoped to THIS native. The four sibling `trim_start_matches` calls in
    // this file serve `Class.getResource`-shaped doors, where stripping is
    // CORRECT, and were not in the measured set.
    if name.starts_with('/') {
        return Ok(Some(Value::Object(None)));
    }
    let resource_name = name.as_str();

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
                return Ok(Some(Value::Object(Some(url?))));
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
            //
            // `url_classloader_isolated_from_app` answers "no null parent
            // to consult" with `true` — deliberately correct for that
            // predicate's own job (should an APP-level parent be walked),
            // but a `null` parent is not "nothing to consult": real
            // `ClassLoader.getResource` still asks the BOOTSTRAP loader
            // (`parent != null ? parent.getResource(name) :
            // BootLoader.findResource(name)`). So the null-parent bootstrap
            // fallback below runs unconditionally, ahead of the
            // isolated-from-app gate, which only governs the non-null-parent
            // walk.
            if matches!(
                ctx.get_field_by_name(this_live, "parent"),
                Value::Object(None)
            ) {
                // A bare `new URLClassLoader(urls, null)` — e.g. byte-buddy's
                // `ClassFileLocator.ForClassLoader.ofBootLoader()` proxy, used
                // by `ClassInjector.UsingUnsafe`'s self-bootstrap of an
                // `AccessibleObject` mirror — relies on this to read a
                // `java.base` class file. Search boot-visible (`jrt:`)
                // resources before falling to this loader's own local URLs.
                if let Some(first) = ctx
                    .find_all_resource_urls(resource_name)
                    .iter()
                    .find(|url| url.starts_with("jrt:"))
                {
                    let url = crate::jboss_module_loader::build_synthetic_url(ctx, first);
                    ctx.unpin_native_roots(this_pin);
                    return Ok(Some(Value::Object(Some(url?))));
                }
            } else if !url_classloader_isolated_from_app(ctx, this_live) {
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
                    || !is_builtin_loader_class_in(&*ctx, &class_name)
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
                    // gen r4w3/rooting: keep `pin` live across the parent's
                    // `getResource` upcall (arbitrary Java) and the bootstrap
                    // URL build; `this_ref` is re-read before `findResource`.
                    let pin = ctx.pin_native_root(this_ref);
                    let name_for_parent = Value::Object(Some(ctx.create_string(&name)));
                    let this_ref = ctx.read_native_pin(pin, this_ref);
                    let parent = ctx.get_field_by_name(this_ref, "parent");
                    match parent {
                        Value::Object(Some(parent_ref)) => {
                            if let Ok(Some(Value::Object(Some(url)))) = ctx.invoke_virtual(
                                parent_ref,
                                "getResource",
                                "(Ljava/lang/String;)Ljava/net/URL;",
                                &[name_for_parent],
                            ) {
                                ctx.unpin_native_roots(pin);
                                return Ok(Some(Value::Object(Some(url))));
                            }
                        }
                        Value::Object(None) => {
                            // Null parent = delegate to the bootstrap class
                            // loader, not "no parent to consult" — see the
                            // matching fix in the URLClassLoader branch above.
                            if let Some(first) = ctx
                                .find_all_resource_urls(resource_name)
                                .iter()
                                .find(|url| url.starts_with("jrt:"))
                            {
                                let url =
                                    crate::jboss_module_loader::build_synthetic_url(ctx, first);
                                ctx.unpin_native_roots(pin);
                                return Ok(Some(Value::Object(Some(url?))));
                            }
                        }
                        _ => {}
                    }
                    // Parent had nothing (or is null/bootstrap): this loader's
                    // own findResource override gets the final say.
                    // gen r4w3/rooting: re-read through the still-held `pin`
                    // (the parent upcall above can move the receiver).
                    let this_ref = ctx.read_native_pin(pin, this_ref);
                    let name_for_find = Value::Object(Some(ctx.create_string(&name)));
                    let this_ref = ctx.read_native_pin(pin, this_ref);
                    ctx.unpin_native_roots(pin);
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
    //
    // STOP AT THE FIRST HIT. `next_resource_url` walks the same segments in
    // the same order and yields the same elements as `find_all_resource_urls`
    // (its own doc states the enumeration-to-exhaustion equivalence), so
    // element 0 is identical either way — but the whole-list call kept
    // scanning after it had the answer. For an archive entry that costs a hash
    // probe; for a DIRECTORY entry it costs an `exists()` and a canonicalize,
    // i.e. filesystem syscalls, on every remaining entry of the classpath.
    //
    // `getResource` is one call per class discovered by a ShrinkWrap package
    // scan (`ClassLoaderAsset.<init>` is `classLoader.getResource(name)`),
    // which is what made a quarkus `TestResourceManager.start()` several times
    // slower than HotSpot — HotSpot's `getResource` returns at the first hit.
    // The incremental walk already existed for the lazy `getResources`
    // enumeration; the singular door had simply never been wired to it.
    //
    // The gate is required: a GLOB name can match several entries WITHIN one
    // classpath entry, and "the first URL this entry serves" would drop the
    // rest — `resource_name_supports_incremental_scan` is what excludes those,
    // and they fall through to the whole-list walk below.
    let url_str = if let Some(first) = first_resource_url(ctx, resource_name) {
        first
    } else if ctx.find_resource(resource_name).is_some() {
        format!("classpath:{name}")
    } else {
        let dbg_all = crate::vmflags().loader.dbg_getresources;
        if dbg_all {
            eprintln!(
                "[GRES-DBG] getResource({}) -> NULL (no urls, no bytes)",
                resource_name
            );
        }
        return Ok(Some(Value::Object(None)));
    };

    let dbg_all = crate::vmflags().loader.dbg_getresources;
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
    Ok(Some(Value::Object(Some(url?))))
}

/// Public re-export of the `getResources` native for `register_essential_natives`
/// so the override is available in real-JDK mode regardless of feature flag.
pub fn cl_get_resources_essential(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if jdk_only_user_loader_receiver(ctx, args) {
        return ctx.invoke_special_bytecode_only(
            "java/lang/ClassLoader",
            "getResources",
            "(Ljava/lang/String;)Ljava/util/Enumeration;",
            args,
        );
    }
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

/// A `getResources` enumeration that builds each `java.net.URL` only when the
/// caller asks for it.
///
/// The array holds the spec strings the classpath walk produced; slot 2 of the
/// enumeration marks them as such, so `nextElement` runs
/// `build_synthetic_url` per element handed out rather than per element found.
/// See [`ENUM_ELEMENTS_URL_SPECS`] for the measurement that motivated it.
///
/// Falls back to the eager form when the fabricated `Enumeration$Impl` is
/// refused (`--jdk-only`), since the real `java.util.Enumeration` that lands
/// there has no slot to carry the marker and no native to act on it.
/// A `getResources` enumeration that does not scan the classpath until the
/// caller asks for an element, and then only far enough to find it.
///
/// Returns `None` when this call cannot use the form — the name can match more
/// than once inside a single entry (a glob), the VM has no incremental scan
/// (a mock `NativeContext`), or the fabricated `Enumeration$Impl` is refused
/// (`--jdk-only`) — and the caller falls back to scanning eagerly.
///
/// The empty case is left to the caller too. `cl_get_resources_impl` has a
/// documented fallback for "no entry served this name" (the `classpath:<name>`
/// pseudo-URL) and a cap for `META-INF/MANIFEST.MF`, both of which need the
/// whole result to decide; probing for the FIRST element here is what tells
/// the caller whether either can apply.
fn lazy_scan_enumeration(
    ctx: &mut dyn NativeContext,
    resource_name: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if !ctx.resource_name_supports_incremental_scan(resource_name) {
        return Ok(None);
    }
    // If the scan yields nothing at all, the caller's empty-result fallback
    // owns the answer — hand it back rather than returning an empty lazy
    // enumeration that would skip it.
    if ctx.next_resource_url(resource_name, 0, 0).is_none() {
        return Ok(None);
    }
    let name_obj = ctx.create_string(resource_name);
    let name_pin = ctx.pin_native_root(name_obj);
    let out = match try_alloc_concurrent_synthetic(ctx, ENUMERATION_IMPL_CLASS, 5) {
        Ok(enm) => {
            let name_obj = ctx.read_native_pin(name_pin, name_obj);
            ctx.set_field(enm, 0, Value::Object(Some(name_obj)));
            ctx.set_field(enm, 1, Value::Int(0));
            ctx.set_field(enm, 3, Value::Int(ENUM_ELEMENTS_LAZY_SCAN));
            ctx.set_field(enm, 4, Value::Int(0));
            Some(enm)
        }
        Err(_) => None,
    };
    ctx.unpin_native_roots(name_pin);
    Ok(out)
}

fn lazy_enumeration_from_url_strings(
    ctx: &mut dyn NativeContext,
    urls: &[String],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
    // GC-safety: `create_string` allocates, and `arr` is written across every
    // iteration, so it has to be read back through the pin each time.
    let arr_pin = ctx.pin_native_root(arr);
    for (i, u) in urls.iter().enumerate() {
        let spec = ctx.create_string(u);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(spec)));
    }
    let out = match try_alloc_concurrent_synthetic(ctx, ENUMERATION_IMPL_CLASS, 5) {
        Ok(enm) => {
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(enm, 0, Value::Object(Some(arr)));
            ctx.set_field(enm, 1, Value::Int(0));
            ctx.set_field(enm, 3, Value::Int(ENUM_ELEMENTS_URL_SPECS));
            Ok(enm)
        }
        Err(_) => enumeration_from_url_strings(ctx, urls),
    };
    ctx.unpin_native_roots(arr_pin);
    out
}

fn enumeration_from_url_strings(
    ctx: &mut dyn NativeContext,
    urls: &[String],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
    // GC-safety: `build_synthetic_url` per iteration allocates (transitively
    // GC-triggering); `arr` is written into again via `set_array_element`
    // afterward, both within the same iteration and across iterations, and
    // once more building the enclosing Enumeration below.
    let arr_pin = ctx.pin_native_root(arr);
    for (i, u) in urls.iter().enumerate() {
        let url_obj = crate::jboss_module_loader::build_synthetic_url(ctx, u);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(url_obj?)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    Ok(enm)
}

/// Build a merged enumeration without converting its URLs through external
/// forms.  Custom URLStreamHandler instances are object state, so rebuilding a
/// URL from its String (as the flat-classpath path does) makes in-memory
/// archives such as ShrinkWrap's `archive:` resources unreadable.
fn enumeration_from_rooted_urls(
    ctx: &mut dyn NativeContext,
    urls: &[RootedUrl],
) -> Result<ObjectRef, MethodCallFailed> {
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
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    Ok(enm)
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
        match ctx.class_name_arc_of_id(c).as_deref() {
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
            exc?,
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

    // The platform loader exposes the JDK's own module resources (`jrt:`) and
    // NOTHING from the application classpath. Answering it from the flat scan —
    // which is what the fall-through at the end of this function does, since
    // `jdk/internal/loader/*` is a builtin loader class — hands the whole
    // application classpath to every child whose parent is platform.
    //
    // That topology is exactly what Spring Boot's `ModifiedClassPathClassLoader`
    // is built on: it parents itself to the platform loader precisely so its own
    // (exclusion-filtered) URL array is the complete application view. Its
    // `getResources` is parent-first, so an unrestricted platform parent put the
    // excluded jar's `META-INF/services` entry straight back — the same leak the
    // receiver-local half of this fix addresses one level down.
    //
    // `cl_get_resource` has drawn this line for the singular lookup since the
    // ModifiedClassPath work; this is the plural half of the same rule, and it
    // keeps the two spec-consistent (`getResource` must return the first URL
    // `getResources` would).
    if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
        if is_platform_class_loader(ctx, this_ref) {
            let urls: Vec<String> = ctx
                .find_all_resource_urls(resource_name)
                .into_iter()
                .filter(|url| url.starts_with("jrt:"))
                .collect();
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
            let p_arr = ctx.pin_native_root(arr);
            for (i, url) in urls.iter().enumerate() {
                let url_obj = crate::jboss_module_loader::build_synthetic_url(ctx, url)?;
                let arr = ctx.read_native_pin(p_arr, arr);
                ctx.set_array_element(arr, i, Value::Object(Some(url_obj)));
            }
            let arr = ctx.read_native_pin(p_arr, arr);
            let enm = make_snapshot_enumeration(ctx, arr)?;
            ctx.unpin_native_roots(p_arr);
            return Ok(Some(Value::Object(Some(enm))));
        }
    }

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
                let enm = enumeration_from_rooted_urls(ctx, &urls)?;
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
                    if !is_builtin_loader_class_in(&*ctx, &class_name) || is_url_loader {
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
                        let enm = enumeration_from_rooted_urls(ctx, &delegated_urls)?;
                        return Ok(Some(Value::Object(Some(enm))));
                    }
                }
            }
        }
    }

    // The lazy form first: it is the same enumeration, in the same order, but
    // it stops scanning where the caller stops reading. It declines the cases
    // the two fallbacks below need a whole result for.
    if resource_name != "META-INF/MANIFEST.MF" {
        if let Some(enm) = lazy_scan_enumeration(ctx, resource_name)? {
            return Ok(Some(Value::Object(Some(enm))));
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
    let dbg_all = crate::vmflags().loader.dbg_getresources;
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
    let enm = lazy_enumeration_from_url_strings(ctx, &urls)?;
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
    // `URL[]`, the declared type (i7-L2): `URLClassLoader.getURLs()` hands
    // this out and callers `checkcast` clones of it. The elements are URLs, so
    // the class is loaded and the no-load lookup finds it.
    let url_component = crate::lang_class::reflection_component_id(ctx, "java/net/URL");
    let result = ctx.new_ref_array(url_component, size);
    for index in 0..size {
        let elements = ctx.read_native_pin(elements_pin, elements);
        ctx.set_array_element(result, index, ctx.get_array_element(elements, index));
    }
    ctx.unpin_native_roots(elements_pin);
    ctx.unpin_native_roots(path_pin);
    Some(result)
}

/// Construct a file URL from an absolute class-path entry. The manifest
/// resolver returns native paths rather than URL text, while Java callers of
/// `URLClassLoader.getURLs()` need real URL objects.
fn file_url_spec(path: &str) -> String {
    let path = path.replace('\\', "/");
    let encoded = path
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F");
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

fn new_url_from_spec(ctx: &mut dyn NativeContext, spec: &str) -> Option<ObjectRef> {
    let text = ctx.create_string(spec);
    let text_pin = ctx.pin_native_root(text);
    let url = match ctx.new_object("java/net/URL") {
        Ok(Some(Value::Object(Some(url)))) => url,
        _ => {
            ctx.unpin_native_roots(text_pin);
            return None;
        }
    };
    let url_pin = ctx.pin_native_root(url);
    let text = ctx.read_native_pin(text_pin, text);
    let result = ctx.invoke_special(
        "java/net/URL",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(url)), Value::Object(Some(text))],
    );
    let url = ctx.read_native_pin(url_pin, url);
    ctx.unpin_native_roots(text_pin);
    ctx.unpin_native_roots(url_pin);
    result.ok().map(|_| url)
}

/// The real JDK application loader is not a `URLClassLoader`, so Spring
/// Boot's `ModifiedClassPathClassLoader` obtains its individual class-path
/// entries from `java.class.path`. CratonVM exposes an AppClassLoader-shaped
/// URL loader instead; handing Spring the suite runner's single manifest
/// pathing JAR causes its exclusion filter to run *before* that manifest is
/// expanded, and the later local lookup silently restores excluded JARs.
///
/// Expose manifest dependencies as effective URLClassLoader entries. CratonVM
/// resolves a manifest `Class-Path` for every receiver-local lookup, so
/// returning its original pathing JAR here lets callers filter a different
/// class path than the loader will subsequently search.
fn expanded_manifest_urls(ctx: &mut dyn NativeContext, urls: ObjectRef) -> Option<ObjectRef> {
    // `new_url_from_spec` below can collect; the source array is revisited
    // while discovering the manifest entries, so keep it visible to a moving
    // collector throughout that phase as well.
    let urls_pin = ctx.pin_native_root(urls);
    let count = ctx.array_length(urls);
    let mut paths = Vec::new();
    let mut expanded_any = false;
    for index in 0..count {
        let urls = ctx.read_native_pin(urls_pin, urls);
        let url = match ctx.get_array_element(urls, index) {
            Value::Object(Some(url)) => url,
            _ => {
                ctx.unpin_native_roots(urls_pin);
                return None;
            }
        };
        let Some(path) = extract_url_path(ctx, url) else {
            ctx.unpin_native_roots(urls_pin);
            return None;
        };
        let manifest_paths = if std::path::Path::new(&path).is_file() {
            cratonvm_classloading::ClassPath::read_jar_manifest(std::path::Path::new(&path))
                .map(|manifest| manifest.resolve_class_path(std::path::Path::new(&path)))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if crate::nbflags().dbg_uclres {
            eprintln!(
                "[UCLURLS-DBG] path={path:?} manifest_entries={}",
                manifest_paths.len()
            );
        }
        if manifest_paths.is_empty() {
            paths.push(path);
        } else {
            expanded_any = true;
            paths.extend(manifest_paths);
        }
    }
    if !expanded_any {
        ctx.unpin_native_roots(urls_pin);
        return None;
    }
    ctx.unpin_native_roots(urls_pin);

    // `URL[]` (i7-L2), as `ucp_path_urls`: this replaces that array.
    let url_component = crate::lang_class::reflection_component_id(ctx, "java/net/URL");
    let result = ctx.new_ref_array(url_component, paths.len());
    let result_pin = ctx.pin_native_root(result);
    for (index, path) in paths.iter().enumerate() {
        let Some(url) = new_url_from_spec(ctx, &file_url_spec(path)) else {
            ctx.unpin_native_roots(result_pin);
            return None;
        };
        let result = ctx.read_native_pin(result_pin, result);
        ctx.set_array_element(result, index, Value::Object(Some(url)));
    }
    let result = ctx.read_native_pin(result_pin, result);
    ctx.unpin_native_roots(result_pin);
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
        if let Some(urls) = ucp_path_urls(ctx, *ucp) {
            // gen r4w3/rooting: `expanded_manifest_urls` can collect and then
            // answer `None` (a URL build failed); the `unwrap_or` fallback
            // must be the re-read array, not the pre-call address.
            let urls_pin = ctx.pin_native_root(urls);
            let expanded = expanded_manifest_urls(ctx, urls);
            let urls = ctx.read_native_pin(urls_pin, urls);
            ctx.unpin_native_roots(urls_pin);
            let result = expanded.unwrap_or(urls);
            return Ok(Some(Value::Object(Some(result))));
        }
    }
    tracing::debug!(
        target: "cratonvm_vm::runtime::classloader",
        "[URLClassPath shim] getURLs() returning empty URL[]"
    );
    let url_component = crate::lang_class::reflection_component_id(ctx, "java/net/URL");
    let arr = ctx.new_ref_array(url_component, 0);
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
    // gen r4w3/rooting: the `<init>` invoke runs Java (GC point); return the
    // re-read list, not the pre-invoke address.
    let list_pin = ctx.pin_native_root(list);
    let _ = ctx.invoke(
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(list))],
    );
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
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
        r.register(cls, "getURLs", "(Z)[Ljava/net/URL;", ucp_get_urls_empty);
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
/// The class CratonVM fabricates for a snapshot `java.util.Enumeration`.
///
/// Not a real JDK name: `java.util.Enumeration` is an INTERFACE and has no
/// `$Impl` nested class in any JDK. So under `--jdk-only` the fabrication is a
/// §1.1 violation the policy refuses, which is why it needs the landing below.
pub(crate) const ENUMERATION_IMPL_CLASS: &str = "java/util/Enumeration$Impl";

/// Slot 3 of [`ENUMERATION_IMPL_CLASS`]: element slot 0 holds the values the
/// enumeration yields, verbatim. Every producer except `getResources` uses
/// this, and it is what a zero-initialized instance already means, so an
/// `Enumeration$Impl` built by `new_object` (which writes no slots) behaves
/// exactly as it did before slot 3 existed.
///
/// Slot 3, not slot 2: `Hashtable.keys()`/`elements()` already stamp a
/// keys-vs-values discriminator into slot 2, and 1 is its `keys` value — so a
/// marker there turned every `Hashtable` key into a `java.net.URL`
/// (`ClassCastException: java.net.URL cannot be cast to java.lang.String`,
/// xerces reading SAX features, PomProfileReposEffectivePomTest).
pub(crate) const ENUM_ELEMENTS_AS_IS: i32 = 0;

/// Slot 3 of [`ENUMERATION_IMPL_CLASS`]: element slot 0 holds URL *spec
/// strings*, and each is turned into a `java.net.URL` by `nextElement`/`next`
/// at the moment it is handed out.
///
/// `ClassLoader.getResources` is the one producer whose consumers routinely
/// stop early. The JDK's own enumeration is lazy per element, so
/// `classLoader.resources(name).anyMatch(..)` — SmallRye Config's
/// `isInClassloader`, and the shape behind every `findFirst`/`anyMatch` over
/// `resources()` — materializes only as far as the first match. CratonVM's
/// native answered eagerly, building a `java.net.URL` (a 13-slot synthetic
/// plus three `String`s) for EVERY match before the caller looked at one.
///
/// On the Quarkus full-reactor harness that meant 1843 URLs built 1388 times
/// — 2.5M URL objects, ~12M allocations — where HotSpot built about eight per
/// call. Deferring construction to `nextElement` restores the short-circuit
/// without changing what the enumeration yields: the strings were already
/// computed by the classpath walk, and `build_synthetic_url` is the same
/// function the eager path called.
pub(crate) const ENUM_ELEMENTS_URL_SPECS: i32 = 1;

/// Slot 3 of [`ENUMERATION_IMPL_CLASS`]: there is no element array at all.
/// Slot 0 holds the RESOURCE NAME, and each element is found by resuming the
/// classpath scan from the cursor in slots 1 (entry index) and 4 (class-path
/// segment).
///
/// [`ENUM_ELEMENTS_URL_SPECS`] stopped `getResources` building 1843
/// `java.net.URL`s for a caller that wanted one, but it still SCANNED all 4230
/// classpath entries to collect the spec strings first. Measured on the Quarkus
/// full-reactor classpath, `resources("").anyMatch(..)` matching on element 1
/// cost 1.20 ms against HotSpot's 0.03 ms — 40x, all of it work the caller
/// never asked for. The JDK's enumeration touches one entry; this makes ours
/// do the same.
///
/// There is deliberately no "peeked element" slot: `hasMoreElements` re-runs
/// the same query `nextElement` does rather than caching. The query is a pure
/// function of (name, cursor) that stops at the first hit, so re-running it
/// costs only the entries between here and the next match — and a peek slot
/// would be one more piece of state to keep consistent with the cursor.
pub(crate) const ENUM_ELEMENTS_LAZY_SCAN: i32 = 2;

/// A real `java.util.Enumeration` over `array`, or `None` when this image
/// cannot build one.
///
/// Same rule as the snapshot-iterator and `System.Logger` landings, in its
/// strongest form: prefer a real class the JDK BUILDS ITSELF over one whose
/// fields we fill. The snapshot is already an `Object[]`,
/// `java.util.Arrays$ArrayList` is the JDK's own fixed-size list over exactly
/// that shape, and `Collections.enumeration(Collection)` turns one into a real
/// `Enumeration` — real bytecode the whole way, so nothing here has to know
/// what the resulting anonymous class is CALLED. That matters: it is
/// `java.util.Collections$3` on JDK 25, and an anonymous class's number is
/// precisely the kind of name that must not be written down.
///
/// `native-io`'s `zip_real_jar` already drives `Collections.enumeration` this
/// way for `ZipFile.entries()`, so the invoke is known to reach real bytecode
/// rather than a native of ours.
pub(crate) fn real_snapshot_enumeration(
    ctx: &mut dyn NativeContext,
    array: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let list = match ctx.new_object_initialized(
        "java/util/Arrays$ArrayList",
        "([Ljava/lang/Object;)V",
        &[Value::Object(Some(array))],
    )? {
        Some(Value::Object(Some(list))) => list,
        _ => return Ok(None),
    };
    match ctx.invoke(
        "java/util/Collections",
        "enumeration",
        "(Ljava/util/Collection;)Ljava/util/Enumeration;",
        &[Value::Object(Some(list))],
    )? {
        Some(Value::Object(Some(enm))) => Ok(Some(enm)),
        _ => Ok(None),
    }
}

/// The snapshot enumeration over `array`: the fabricated
/// [`ENUMERATION_IMPL_CLASS`] in `Compatible` mode, and a real
/// `java.util.Enumeration` when `--jdk-only` refuses that fabrication.
///
/// This is the last of the shapes item 4 of
/// `ensure-synthetic-class-cannot-enforce-only-record.md` listed as "refused
/// with nowhere to land". `Compatible` mode is byte-for-byte what it was: the
/// fallback is reached only from the refusal arm, which only strict mode takes.
pub(crate) fn make_snapshot_enumeration(
    ctx: &mut dyn NativeContext,
    array: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-SAFETY: both arms allocate — the fabricated shell, or a real
    // `Arrays$ArrayList` plus whatever `Collections.enumeration` builds — and
    // `array` is a bare Rust local the collector cannot see. Root it across
    // both and read it back through the pin, the same contract
    // `make_iterator_from_array` documents.
    let pin = ctx.pin_native_root(array);
    let out = match try_alloc_concurrent_synthetic(ctx, ENUMERATION_IMPL_CLASS, 5) {
        Ok(enm) => {
            let array = ctx.read_native_pin(pin, array);
            ctx.set_field(enm, 0, Value::Object(Some(array)));
            ctx.set_field(enm, 1, Value::Int(0));
            ctx.set_field(enm, 3, Value::Int(ENUM_ELEMENTS_AS_IS));
            Ok(enm)
        }
        Err(refusal) => {
            let array = ctx.read_native_pin(pin, array);
            match real_snapshot_enumeration(ctx, array) {
                Ok(Some(enm)) => Ok(enm),
                // Nothing real to stand in — an image with no
                // `Arrays$ArrayList` — so the refusal stands rather than
                // silently becoming a fabrication again.
                Ok(None) => Err(refusal),
                Err(err) => Err(err),
            }
        }
    };
    ctx.unpin_native_roots(pin);
    out
}

/// `Enumeration$Impl.nextElement()` / `.next()` — one body, because the two
/// have always been the same code and only one of them may now materialize.
///
/// Slot 3 says how to read slot 0's element: verbatim
/// ([`ENUM_ELEMENTS_AS_IS`], what every producer but `getResources` stores and
/// what a zero-initialized instance already means) or as a URL spec string to
/// be turned into a `java.net.URL` right here ([`ENUM_ELEMENTS_URL_SPECS`]).
/// The next `(spec, segment, index)` a lazy-scan enumeration would yield, or
/// `None` when the classpath is exhausted. Pure in the receiver: it reads the
/// cursor but never advances it, so `hasMoreElements` and `nextElement` both
/// call it and agree.
fn enum_impl_peek_lazy(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<(String, u32, u32)> {
    let name = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    let index = ctx.get_field(this, 1).as_int().unwrap_or(0).max(0) as u32;
    let segment = ctx.get_field(this, 4).as_int().unwrap_or(0).max(0) as u32;
    ctx.next_resource_url(&name, segment, index)
}

fn enum_impl_is_lazy(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    ctx.get_field(this, 3)
        .as_int()
        .unwrap_or(ENUM_ELEMENTS_AS_IS)
        == ENUM_ELEMENTS_LAZY_SCAN
}

fn enum_impl_next_element(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if enum_impl_is_lazy(ctx, this) {
        let Some((spec, segment, index)) = enum_impl_peek_lazy(ctx, this) else {
            return Ok(Some(Value::Object(None)));
        };
        ctx.set_field(this, 4, Value::Int(segment as i32));
        ctx.set_field(this, 1, Value::Int(index as i32));
        let url = crate::jboss_module_loader::build_synthetic_url(ctx, &spec)?;
        return Ok(Some(Value::Object(Some(url))));
    }
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
    if ctx
        .get_field(this, 3)
        .as_int()
        .unwrap_or(ENUM_ELEMENTS_AS_IS)
        != ENUM_ELEMENTS_URL_SPECS
    {
        return Ok(Some(elem));
    }
    let Value::Object(Some(spec_obj)) = elem else {
        return Ok(Some(elem));
    };
    let Some(spec) = ctx.read_string(spec_obj) else {
        return Ok(Some(elem));
    };
    // GC-safety: `build_synthetic_url` allocates (a 13-slot URL plus three
    // `String`s), so nothing raw may be held across it. `this` is not read
    // again after this point and the cursor was already advanced, so there is
    // nothing left to re-read through a pin.
    let url = crate::jboss_module_loader::build_synthetic_url(ctx, &spec)?;
    Ok(Some(Value::Object(Some(url))))
}

pub fn register_enumeration_impl_natives(r: &mut NativeMethodRegistry) {
    // Install the URLClassPath safe stubs alongside the enumeration helpers
    // so both real-JDK (`register_essential_natives`) and synthetic-JDK
    // (`register_classloader_natives`) callers pick them up.
    register_url_class_path_safe_stubs(r);

    let enm = "java/util/Enumeration$Impl";
    r.register(enm, "hasMoreElements", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if enum_impl_is_lazy(ctx, this) {
            let more = enum_impl_peek_lazy(ctx, this).is_some();
            return Ok(Some(Value::Int(i32::from(more))));
        }
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
    });
    r.register(
        enm,
        "nextElement",
        "()Ljava/lang/Object;",
        enum_impl_next_element,
    );
    r.register(enm, "hasNext", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if enum_impl_is_lazy(ctx, this) {
            let more = enum_impl_peek_lazy(ctx, this).is_some();
            return Ok(Some(Value::Int(i32::from(more))));
        }
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
    });
    r.register(enm, "next", "()Ljava/lang/Object;", enum_impl_next_element);
    let anon_enm = "cratonvm/synthetic/AnonymousObject$2";
    // `SyntheticStub`, stated for this block. The receiver is the VM's
    // anonymous-object fallback class — minted here, on no image — so §1.5's
    // "what an `ACC_NATIVE` method binds to" has nothing to point at, and the
    // `Bridge` tag was only ever keeping a fabricated class reachable under
    // `--jdk-only`, which §5 forbids outright.
    let __anon_enm_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
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
    r.set_category(__anon_enm_cat);
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
    if jdk_only_user_loader_receiver(ctx, args) {
        return ctx.invoke_special_bytecode_only(
            "java/lang/ClassLoader",
            "getResourceAsStream",
            "(Ljava/lang/String;)Ljava/io/InputStream;",
            args,
        );
    }
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
            exc?,
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
    // Mirror JDK `ClassLoader.getResourceAsStream` = `URL u =
    // getResource(name); return u != null ? u.openStream() : null;` for
    // EVERY classloader shape, not only `URLClassLoader`/user-defined ones.
    //
    // This used to gate on `object_extends(.., "java/net/URLClassLoader") ||
    // !is_builtin_loader_class(&class_name)`, which EXCLUDED the three
    // built-in loaders (`is_builtin_loader_class`'s `jdk/internal/loader/`
    // arm: `BuiltinClassLoader`, `ClassLoaders$AppClassLoader`,
    // `ClassLoaders$PlatformClassLoader`) and fell through to the raw,
    // loader-identity-BLIND `ctx.find_resource` below — which searches every
    // classpath regardless of which loader asked. Real
    // `getResourceAsStream` is never overridden by any of the three
    // (`javap -p jdk.internal.loader.ClassLoaders$PlatformClassLoader` shows
    // no override; only base `java.lang.ClassLoader` declares it), so
    // HotSpot always answers through `getResource`, which IS
    // loader-identity-aware for the platform loader because it is real
    // bytecode too (native only shadows `getResource` on `cl` and
    // `URLClassLoader` — see the registrar in this file). That asymmetry
    // between the shadowed `getResourceAsStream` and the un-shadowed
    // `getResource` was exactly the defect measured by
    // `apps/probes/L7LoaderInternalsSweep.java` rows 07/08: the platform
    // loader's `getResourceAsStream` found an application-classpath
    // resource its own `getResource` correctly refused. Delegating
    // universally routes every loader shape through the same
    // loader-identity-aware `getResource` HotSpot itself uses, closing the
    // asymmetry instead of special-casing the one class the census happened
    // to name.
    //
    // Routing through `getResource` (which now performs parent delegation)
    // also ensures a custom loader that doesn't override `findResource`
    // still finds resources its parent serves
    // (SerializationHelperTest/ProxyClassReuseTest), and a plain
    // user-instantiated `new URLClassLoader(urls, parent)` (e.g. Spring
    // Boot's `ServletComponentScanIntegrationTests.indexedComponentsAreRegistered`)
    // sees resources reachable only through the dynamically-registered
    // global URL walk (`ctx.find_all_resource_urls`, used by both
    // `getResource` and `ucl_find_resource`'s own fallback) that the raw
    // `ctx.find_resource` store below does not.
    if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
        if is_classloader_instance(ctx, this_ref) {
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
                return ctx.invoke_virtual(url, "openStream", "()Ljava/io/InputStream;", &[]);
            }
            return Ok(Some(Value::Object(None)));
        }
    }
    if let Some(bytes) = defined_class_resource_bytes(ctx, resource_name) {
        let len = bytes.len();
        let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes);
        tracing::debug!(
            target: "cratonvm_vm::runtime::resources",
            resource = %resource_name,
            bytes = len,
            "ClassLoader.getResourceAsStream served DEFINED-CLASS resource"
        );
        return Ok(Some(Value::Object(Some(stream?))));
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
            Ok(Some(Value::Object(Some(stream?))))
        }
    }
}

/// Serve a `"<internal/name>.class"` resource request from a class that was
/// dynamically DEFINED (via `Unsafe.defineClass`/`Lookup.defineClass`/
/// `ClassLoader.defineClass`/the various native `define_class_full` callers
/// such as `ConfigurationClassEnhancer.enhance`'s CGLIB-proxy emitter),
/// rather than found on a classpath jar/directory — `ctx.find_resource`
/// only ever searches the bootstrap/extension/application CLASSPATHS, so a
/// runtime-generated class's bytecode was previously never retrievable
/// through `getResourceAsStream`/`Class.getResourceAsStream` at all.
///
/// This matters for real bytecode-introspection tooling: ASM's
/// `ClassReader` (used by real CGLIB, e.g. when Spring AOP's
/// `proxyTargetClass=true` auto-proxy creator tries to CGLIB-subclass an
/// ALREADY-native-CGLIB-generated `ConfigurationClassEnhancer` proxy class
/// a SECOND time) resolves a class's own bytecode this way — without it,
/// generation fails with cglib's generic `Could not generate CGLIB
/// subclass... Common causes... final class or non-visible class`, even
/// though the class is neither.
///
/// Looked up via the existing `class_id_by_name` + `class_bytes` trait
/// methods — a loader-blind global name lookup, same as `resolve_or_load_
/// class_id` elsewhere in this codebase uses for similar "resolve this
/// well-known name" cases. That is unsound in general once 2+ loaders
/// define a same-named class (see the documented duplicate-`ClassId`
/// family elsewhere in this codebase's history) — but every class this
/// function can actually reach was named by one of THIS crate's own
/// generators, all of which mint a process-globally-unique, monotonically-
/// countered suffix (`$$SpringCGLIB$$<n>`, `...$$FB<n>`, etc.), so no two
/// loaders ever define one under the identical name in practice. A future
/// caller relying on this for a DIFFERENT (non-uniquely-named) class would
/// need a proper (name, defining-loader) lookup instead.
fn defined_class_resource_bytes(ctx: &dyn NativeContext, resource_name: &str) -> Option<Vec<u8>> {
    let internal_name = resource_name.strip_suffix(".class")?;
    let class_id = ctx.class_id_by_name(internal_name)?;
    ctx.class_bytes(class_id)
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
            Ok(Some(Value::Object(Some(stream?))))
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

/// `--jdk-only`: `jdk.internal.loader.BootLoader.findResource(String)`, the
/// bootstrap step of the JDK's `ClassLoader.getResource` for a loader with a
/// null parent.
///
/// Its JDK body asks `ClassLoaders.bootLoader()`, a `BuiltinClassLoader` this
/// VM never builds, so it answered null for every name. That did not matter
/// while the `ClassLoader.getResource*` natives answered user loaders
/// themselves. Once those run the JDK's body for user loaders, a null-parent
/// `URLClassLoader` could not see `java/lang/reflect/AccessibleObject.class`.
/// ByteBuddy reads exactly that to build its `Unsafe` injector, and without it
/// EasyMock could not mock `ClassLoader` (Tomcat's `TestWebappServiceLoader`).
///
/// The answer is the image's `jrt:` URL, only for a module the JDK's own
/// `ModuleLoaderMap` gives to the BOOT loader. `java.sql` and the other
/// platform modules are not visible from here, as on HotSpot. See
/// `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
pub fn bootloader_find_resource(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match boot_module_resource_urls(ctx, args).first() {
        Some(url) => {
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, url)?;
            Ok(Some(Value::Object(Some(url))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `--jdk-only`: `jdk.internal.loader.BootLoader.findResources(String)`, the
/// plural of [`bootloader_find_resource`].
pub fn bootloader_find_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let urls = boot_module_resource_urls(ctx, args);
    Ok(Some(Value::Object(Some(enumeration_from_url_strings(
        ctx, &urls,
    )?))))
}

/// The `jrt:` URLs of `args[0]` in modules the JDK defines to the boot loader.
fn boot_module_resource_urls(ctx: &mut dyn NativeContext, args: &[Value]) -> Vec<String> {
    let name = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Vec::new(),
    };
    let Some(sets) = jdk_builtin_module_sets(ctx) else {
        return Vec::new();
    };
    // Segment 0 is the boot class path: the run-time image, which serves every
    // module, so the module filter below is what makes the answer the BOOT
    // loader's. A `.jmod`-form boot path yields no `jrt:` URL and so no answer,
    // which is the pre-existing null.
    ctx.find_resource_urls_in_segment(&name, 0)
        .into_iter()
        .filter(|url| {
            url.strip_prefix("jrt:/")
                .and_then(|rest| rest.split('/').next())
                .is_some_and(|module| sets.boot.contains(module))
        })
        .collect()
}

/// `jdk.internal.loader.BuiltinClassLoader.findResourceAsStream(String,String)`.
pub fn builtin_classloader_find_resource_as_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    resource_stream_for_last_string_arg(ctx, args)
}

/// Synthetic-JDK `ClassLoader.getDefinedPackage(String)`.
///
/// Delegates to the real-JDK implementation rather than answering an
/// unconditional `null`: that stub is what made every package-name
/// `SpringApplication` source fail as `IllegalArgumentException: Invalid
/// source '<pkg>'` (`BeanDefinitionLoader.findPackage` returns exactly this
/// probe's result), and there is no reason for the two boot modes to disagree.
/// `getDefinedPackages`/`getPackages` below stay empty — see the note on
/// `lang_class::i2_classloader_get_defined_packages`.
fn cl_get_defined_package(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_class::i2_classloader_get_defined_package(ctx, args)
}

fn cl_get_defined_packages(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // `Package[0]`, the declared type (i8-L2), not `Object[0]`.
    let component = crate::lang_class::loading_component_id(ctx, "java/lang/Package");
    let empty = ctx.new_ref_array(component, 0);
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

/// `ClassLoader.isRegisteredAsParallelCapable()Z`.
///
/// WAVE-3 FIX (found by cross-package audit): this registration WINS over the
/// `phases_late::register_p61_classloader` copy (both are reachable only from
/// `register_synthetic_overrides`; lib.rs calls the classloader one LAST), and
/// it used to read a field that was written `0` at every initialiser and never
/// written `1` anywhere. Meanwhile `registerAsParallelCapable()` (above, and
/// `deprecated_internal.rs`, and `classloader_real.rs`) answers an
/// unconditional `true`. A loader that had just successfully registered was
/// then told it had not — the register/query pair contradicted itself.
///
/// The four `ClassLoader` initialisers now seed `parallel_capable = true`,
/// which is the truthful answer for this VM: every synthetic loader IS
/// parallel-capable, because class definition serialises on the VM's own
/// global class-registry lock rather than on the loader object, so no loader
/// can deadlock another by loading concurrently. The state stays per-loader
/// (not a constant) so a future per-loader model can flip it back to 0.
///
/// L1 moved that state out of slot 4 and into [`LoaderMeta`]: on a real JDK
/// image slot 4 is `java.lang.ClassLoader.parallelLockMap`, so the seed write
/// was destroying a JDK field and the read was answering its fallback by
/// accident. `phases_late`'s shadowed twin reads through the same accessor.
fn cl_is_registered_as_parallel_capable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Loader this VM never recorded, or one on the real JDK layout (where
    // slot 4 is `parallelLockMap`, not our flag): fall back to the same `true`
    // the registration call reports, rather than contradicting it.
    let val = loader_parallel_capable_of(ctx, this).unwrap_or(1);
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
    // `File.toURI().toURL()` percent-encodes reserved/space characters in the
    // path (e.g. a directory named `app location` becomes `app%20location`).
    // Real `URLClassPath` decodes this back (`new File(url.toURI())`) before
    // touching the filesystem; do the same here, mirroring the sibling
    // `jar:file:` decode in `net_phase_e::uri_percent_decode`. Decoding after
    // the `/!`-marker normalisation above keeps the jar-boundary detection on
    // the original encoded text.
    let p = crate::net_phase_e::uri_percent_decode(&p);
    // A root archive URL ends in `!/` (for example
    // `jar:file:src/test/resources/jars/app.jar!/`).  There is no virtual
    // subdirectory to retain in that form: it denotes the archive's root.
    // Leaving the marker in the filesystem token makes `ClassPath::new`
    // probe the non-existent path `app.jar!/`, so a receiver-local
    // URLClassLoader sees neither resources nor classes from its own JAR.
    // Keep non-root `!/prefix/` specifications intact; those become a
    // `NestedDirectory` classpath entry below.
    let p = if p.ends_with("!/") && p.find("!/") == Some(p.len() - 2) {
        p[..p.len() - 2].to_string()
    } else {
        p
    };
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
    ucl_mark_open(ctx, this);

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
pub(crate) const UCP_STASHED_URLS: usize = 0;

/// True when `ucp` carries CratonVM's SYNTHETIC `URLClassPath` layout — i.e.
/// [`UCP_STASHED_URLS`] (slot 0) really is our stash array.
///
/// Real `jdk.internal.loader.URLClassPath` declares `path`(0) — an
/// `ArrayList<URL>`, NOT an array. `array_length` of a non-array answers 0
/// (`vm_exec.rs` guards the kind), so the unguarded slot-0 read did not crash;
/// it silently reported "this loader owns no URLs", which is a wrong answer of
/// exactly the kind a URL-visibility fix is trying to avoid. Reachable
/// whenever `record_url_on_path` has created the list and `ucp_path_urls` has
/// still declined it — an empty `path`.
///
/// Class-side for the same reason [`cl_has_synthetic_layout`] is: a value-shape
/// test cannot do this job, because `get_field_by_name` answers
/// `Value::Object(None)` both for a name it cannot resolve and for a real
/// reference field that is null (`vm/src/vm/vm_exec.rs`, and the trait contract
/// in `native-api/src/registry.rs`). `URLClassPath` is not a `ClassLoader`, so
/// it needs its own witness rather than `cl_has_synthetic_layout`'s three.
fn ucp_synthetic_layout(ctx: &dyn NativeContext, ucp: ObjectRef) -> bool {
    ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(ucp), "path")
        .is_none()
}

/// Is the value stashed at [`UCP_STASHED_URLS`] genuinely a reference ARRAY?
///
/// This is the question `ucl_get_urls`'s stash fallback actually needs, and it
/// is NOT the same as "does this `ucp` have a synthetic layout". Gating the
/// stash on [`ucp_synthetic_layout`] looked right but broke a load-bearing
/// real-JDK path: [`record_ucl_urls`] deliberately stashes a real-mode
/// `URLClassLoader`'s constructor `URL[]` on its `ucp` PLACEHOLDER precisely
/// because the loader itself has the real field layout — see that function's
/// doc comment, and the Tomcat `StandardJarScanner` TLD regression it names.
/// A real-layout `ucp` with a real stash is therefore an expected shape, not a
/// contradiction, and the synthetic-layout gate refused it.
///
/// Asking about the stashed value answers the actual safety concern directly:
/// on a REAL `jdk.internal.loader.URLClassPath`, slot 0 is `path`, an
/// `ArrayList`, and `ObjectHeader::array_length` answers **0** for any
/// non-array — so a real `path` contributes nothing and is never indexed.
///
/// A length test rather than a class-name test, deliberately. Array objects do
/// not carry a nameable class in every context (`MockNativeContext` assigns
/// them `ClassId::new(0)`, whose name is `None`), so a `starts_with('[')`
/// witness silently answers "not an array" for genuine arrays. The length is
/// the one property both the real heap and the mock agree on.
///
/// An EMPTY stash answers `false` here, and that is correct rather than merely
/// tolerable: reading a zero-length array would contribute no URLs anyway, so
/// both arms produce the same result.
fn ucp_stash_is_reference_array(ctx: &dyn NativeContext, stashed: ObjectRef) -> bool {
    ctx.array_length(stashed) > 0
}

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
    crate::classloader_real::init_urlclassloader_constructor_with_default_parent(ctx, this, urls)?;
    Ok(None)
}

fn ucl_init_urls_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let urls = args.get(1).copied().unwrap_or(Value::Object(None));
    let parent = args.get(2).copied().unwrap_or(Value::Object(None));
    crate::classloader_real::init_urlclassloader_constructor_with_parent(ctx, this, urls, parent)?;
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
    if ucl_is_closed(ctx, this) {
        let name = args
            .get(1)
            .and_then(|value| match value {
                Value::Object(Some(name)) => ctx.read_string(*name),
                _ => None,
            })
            .unwrap_or_else(|| "<unknown>".to_owned());
        let exception = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &name,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exception?,
        ));
    }
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_name = ctx.read_string(name_obj).unwrap_or_default();
    let internal = class_name.replace('.', "/");
    // gc-common w20-f: the local URL search may wait for a peer definer, fetch
    // or define (GC-capable) even on a miss, and the delegation below used the
    // loader and the name at their entry addresses. Pinned across it and
    // re-read, as `cl_load_class_base_delegation_rooted` does around its own
    // call.
    let this_pin = ctx.pin_native_root(this);
    let name_pin = ctx.pin_native_root(name_obj);
    let local = ucl_try_define_local_class(ctx, this, &internal);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    let name_obj: ObjectRef = ctx.read_native_pin(name_pin, name_obj);
    ctx.unpin_native_roots(this_pin);
    if let Some(result) = local {
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
            //
            // KEPT SWALLOW, at JDK parity. `URLClassPath$Loader.getResource`
            // wraps its whole `url.openConnection()` / `getInputStream()`
            // region in `catch (Exception e) { return null; }`, so a failure
            // anywhere in the probe is "no such resource", not a thrown
            // exception out of `getResources`. This predicate returns `bool`
            // and its caller holds two native pins across the call, so it
            // cannot propagate without a signature change; the residual is
            // that an `Error` is absorbed here where the JDK's `catch
            // (Exception)` would let it out. Recorded, not silently kept.
            // W7-57-close-flush-swallow-sweep.md
            let _ = ctx.invoke_virtual(stream, "close", "()V", &[]);
            true
        }
        _ => false, // null stream (directory node) or FileNotFoundException
    }
}

pub(crate) fn object_extends(ctx: &dyn NativeContext, obj: ObjectRef, target: &str) -> bool {
    let mut class_id = ctx.class_id_of_object(obj);
    for _ in 0..64 {
        match ctx.class_name_arc_of_id(class_id).as_deref() {
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
        || platform_loader_of(ctx.vm_identity())
            .is_some_and(|platform| platform.as_ptr() == loader.as_ptr())
}

/// Mirror of [`is_platform_class_loader`] for the process-wide application
/// (system) loader singleton: `loader` is either literally
/// `jdk.internal.loader.ClassLoaders$AppClassLoader` by declared class, or
/// pointer-identical to the recorded singleton. Unlike
/// [`is_user_defined_loader`], which answers `false` for a bare
/// `java.net.URLClassLoader`/`ClassLoader` instance because its OWN declared
/// class is on `is_builtin_loader_class`'s name list (a check aimed at a
/// different question -- see that predicate's doc comment), this asks
/// specifically "is this object THE application loader", so a bare
/// `URLClassLoader` whose parent is some OTHER user loader (not the app
/// singleton, not platform) correctly answers `false` here too and is left
/// to the caller's own "not built-in" handling.
pub(crate) fn is_application_class_loader(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(loader))
        .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$AppClassLoader")
        || app_loader_of(ctx.vm_identity()).is_some_and(|app| app.as_ptr() == loader.as_ptr())
}

pub fn url_classloader_isolated_from_app(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    // Some real-JDK subclasses lose their inherited URLClassLoader identity
    // at native dispatch sites. Their constructor URL list is retained by our
    // URL loader shims, so accept that authoritative signal as well.
    if !object_extends(ctx, loader, "java/net/URLClassLoader")
        && loader_constructor_url_paths(ctx, loader).is_empty()
    {
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

fn empty_enumeration_impl(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    Ok(enm)
}

/// Cached `cratonvm_classloading::ClassPath::new(paths)` construction, keyed
/// by the exact `paths` vector.
///
/// PERF (2026-07-23): both call sites below (`loader_local_resource_urls`
/// and `ucl_try_define_local_class`) used to call `ClassPath::new(&paths)`
/// FRESH on every single invocation — i.e. every `URLClassLoader.findClass`/
/// `findResource` call re-read and re-parsed every jar on the loader's
/// classpath from scratch, with no caching at all (unlike
/// `jar_contents_cached`, which this superficially resembles but doesn't
/// share any code with). `ClassPath::new` opens and parses every classpath
/// entry eagerly, so on a large classpath (`module/spring-boot-data-redis`'s
/// test classpath has ~121 jars, several of them large — testcontainers.jar
/// alone is 12.5k entries) this made ordinary Spring context bootstrap,
/// which does hundreds of `ClassUtils.isPresent()`-style lookups per
/// `ApplicationContextRunner.run()`, pay a full classpath re-scan on EVERY
/// lookup — measured ~10-12s for just 100 lookups, when the underlying work
/// should be milliseconds after the first scan. `DataRedisAutoConfigurationTests`,
/// `DataRedisAutoConfigurationJedisTests`,
/// `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests`, and
/// `DataRedisHealthContributorAutoConfigurationTests` all HANG (300s suite
/// timeout) as a direct result. Cache by the exact paths vector: a
/// `URLClassLoader.addURL` call naturally produces a longer paths vector, so
/// it transparently gets its own fresh (correct) cache entry rather than
/// serving a stale one — no explicit invalidation needed.
///
/// BOUNDED (gc-common w9-a): at most [`LOCAL_URL_CLASS_PATH_CACHE_LIMIT`]
/// entries, evicting the least recently used. "Small in practice" did not
/// hold. Every URL loader of every VM in the process adds entries, per-test
/// loaders over fresh temp directories never repeat a key, and a loader that
/// `addURL`s k jars one at a time with a lookup in between leaves k entries
/// behind, parsing 1, 2, ..., k jars each. Nothing ever removed one. LRU
/// rather than the clear-all its unread twin used keeps the hot classpaths
/// of the loaders actually in use (the DataRedis case above is one classpath
/// looked up hundreds of times).
fn cached_class_path_for_paths(
    paths: &[String],
) -> std::sync::Arc<cratonvm_classloading::ClassPath> {
    let cache = url_class_path_cache();
    {
        let mut guard = cache.lock();
        let (map, tick) = &mut *guard;
        *tick += 1;
        let now = *tick;
        if let Some((cp, last_used)) = map.get_mut(paths) {
            *last_used = now;
            return Arc::clone(cp);
        }
    }
    // Construct outside the cache lock: opening a manifest pathing JAR can
    // touch hundreds of dependency archives and must not serialize unrelated
    // URLClassLoader resolution work.
    let cp = Arc::new(cratonvm_classloading::ClassPath::new(paths));
    let mut guard = cache.lock();
    let (map, tick) = &mut *guard;
    *tick += 1;
    let now = *tick;
    if let Some((existing, last_used)) = map.get_mut(paths) {
        *last_used = now;
        return Arc::clone(existing);
    }
    if map.len() >= LOCAL_URL_CLASS_PATH_CACHE_LIMIT {
        let oldest = map
            .iter()
            .min_by_key(|(_, (_, used))| *used)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            map.remove(&oldest);
        }
    }
    map.insert(paths.to_vec(), (Arc::clone(&cp), now));
    cp
}

/// Paths vector → (`ClassPath`, last-use tick), plus the tick counter.
/// See [`cached_class_path_for_paths`].
type UrlClassPathCache = (
    std::collections::HashMap<Vec<String>, (Arc<cratonvm_classloading::ClassPath>, u64)>,
    u64,
);

fn url_class_path_cache() -> &'static OrderedPlMutex<UrlClassPathCache> {
    // `Scratch` (L0): every critical section is a map probe, an insert or a
    // 64-entry eviction scan. `ClassPath::new` runs outside it.
    static CACHE: OnceLock<OrderedPlMutex<UrlClassPathCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        OrderedPlMutex::new((std::collections::HashMap::new(), 0), LockLevel::Scratch)
    })
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
    // placeholder (see `record_ucl_urls`). The slot reads are gated because on
    // the real layout slot 4 is `parallelLockMap` and slot 2 is
    // `unnamedModule` — see `ucl_get_urls` for the full layout.
    if cl_has_synthetic_layout(ctx, loader) {
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
        // Only when slot 0 really holds a URL ARRAY. On a real `URLClassPath`
        // slot 0 is `path`, an ArrayList, and `array_length` of that is not a
        // URL count. Ask about the STASHED VALUE rather than the `ucp`'s
        // layout: `record_ucl_urls` stashes here precisely for REAL-layout
        // loaders (see its doc comment and the Tomcat StandardJarScanner
        // regression it names), so a real `ucp` carrying a real stash is an
        // expected shape and a synthetic-layout gate wrongly refuses it.
        if let Value::Object(Some(urls)) = ctx.get_field(ucp, UCP_STASHED_URLS) {
            if ucp_stash_is_reference_array(ctx, urls) {
                for i in 0..ctx.array_length(urls) {
                    if let Value::Object(Some(url)) = ctx.get_array_element(urls, i) {
                        append_url(url);
                    }
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

    // Both raw-slot families are gated on the layout that gives them their
    // meaning — see `cl_has_synthetic_layout` / `ucp_synthetic_layout`.
    if cl_has_synthetic_layout(ctx, loader) {
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
    }

    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(loader, "ucp") {
        if ucp_synthetic_layout(ctx, ucp) {
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

/// True when `loader` is a `URLClassLoader`-shaped loader whose constructor URL
/// set we positively recorded (see [`record_ucl_urls`]) — i.e. its own URL view
/// is authoritative, and an EMPTY view genuinely means "this loader owns
/// nothing", not "we failed to observe its URLs".
///
/// The recorded set is either the synthetic-JDK `UCL_URLS_ARRAY` slot or the
/// real-JDK `ucp` placeholder our constructor shim installs; the presence of
/// EITHER is the proof. Same signal [`loader_local_resource_urls`] already
/// trusts for receiver-local resource lookup, so a loader this returns `false`
/// for is one whose local lookups are already going through the global path.
///
/// Requires an actual `java.net.URLClassLoader` (or subclass) receiver: those
/// raw slot indices only mean "URLs" on that shape. Probing them on, say, a
/// bare synthetic `java/lang/ClassLoader` reads an unrelated field — the same
/// out-of-bounds-slot hazard `is_user_defined_loader` guards against.
fn loader_has_recorded_url_set(ctx: &mut dyn NativeContext, loader: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(loader);
    let is_url_loader = ctx
        .class_id_by_name("java/net/URLClassLoader")
        .is_some_and(|ucl| cid == ucl || ctx.is_subclass(cid, ucl));
    if !is_url_loader {
        return false;
    }
    // The slot-4 disjunct is the SYNTHETIC signal and only means "URL array"
    // on the synthetic layout; on a real loader slot 4 is `parallelLockMap`, a
    // `ConcurrentHashMap` that is never null, so unguarded it answers `true`
    // for every real receiver without consulting anything about URLs. The
    // `ucp` disjunct is the real-layout signal and stands on its own.
    (cl_has_synthetic_layout(ctx, loader)
        && matches!(
            ctx.get_field(loader, UCL_URLS_ARRAY),
            Value::Object(Some(_))
        ))
        || matches!(ctx.get_field_by_name(loader, "ucp"), Value::Object(Some(_)))
}

/// Does `loader` answer resource lookups entirely out of a URL list CratonVM
/// can enumerate in full?
///
/// When this is true, `loader.getResources(name)` is exhaustive for that
/// loader — including when it comes back EMPTY — so a caller must not widen an
/// empty answer with a process-wide classpath scan. `ucl_find_resources` is
/// what makes that guarantee hold; this predicate is how a caller outside
/// `classloader.rs` (`service_loader::discover_providers`) asks whether it may
/// rely on it.
///
/// Deliberately narrower than [`loader_has_recorded_url_set`]: that one accepts
/// a `URLClassLoader` carrying a `ucp` whose URL list may not have been recorded
/// yet, which is precisely the "the local scan could not run" case a caller must
/// still fall back for.
pub(crate) fn loader_owns_complete_resource_view(
    ctx: &dyn NativeContext,
    loader: ObjectRef,
) -> bool {
    object_extends(ctx, loader, "java/net/URLClassLoader")
        && !loader_constructor_url_paths(ctx, loader).is_empty()
}

/// Is a package with class files under `class_glob` (e.g.
/// `com/example/pkg/*.class`) visible **to this specific loader**?
///
/// `ClassLoader.getDefinedPackage` is non-delegating in the JDK, so answering it
/// from the VM-global classpath makes every loader — including a deliberately
/// isolated one — claim every package on the process classpath. That is the
/// loader-identity gap behind
/// `fixed-suite-bugs/springboot/*-beandefinitionloader-package-scan-empty-FIXED.md`:
/// `new URLClassLoader("empty", new URL[0], null).getDefinedPackage(p)` answered
/// with a `Package` where HotSpot answers `null`.
///
/// Resolution order, deliberately failing back to the historical global answer
/// whenever the receiver's own view is not knowable:
/// 1. built-in loaders — see [`builtin_loader_segment`]. The APPLICATION and
///    PLATFORM loaders are probed against their OWN class-path segment, not the
///    concatenation; every other built-in (the boot loader) keeps the global
///    probe, because the boot loader genuinely does define the boot packages;
/// 2. a loader with its own recorded URLs — probe exactly those (already
///    pathing-jar aware via [`loader_local_resource_urls`]);
/// 3. a loader with a positively-recorded but EMPTY URL set — nothing is
///    visible, matching HotSpot;
/// 4. anything else (a custom loader we have no URL view of) — global probe,
///    i.e. unchanged from before this function existed.
/// Is this one of the loaders the VM itself creates (bootstrap / platform /
/// application), as opposed to a user-defined one?
///
/// **This is an identity test, not a claim about visibility.** Being built-in
/// does NOT mean the loader can see the whole process classpath — that reading
/// is exactly the bug `aa09d8bd8` fixed below, where the application loader
/// fabricated a `Package` for `java.lang`. Callers that want visibility must go
/// through [`package_class_files_visible_to_loader`], which segments it.
///
/// The one caller outside this module is `getDefinedPackage`'s DEFAULT-package
/// arm: a built-in loader that loaded a class from the classpath root has
/// defined the default package, and a user-defined loader has not.
pub(crate) fn loader_is_builtin(ctx: &mut dyn NativeContext, loader: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(loader))
        .is_some_and(|n| {
            // `--jdk-only`: a layer's `jdk.internal.loader.Loader` is a user
            // loader (`is_builtin_loader_class_in`).
            (n.starts_with("jdk/internal/loader/")
                && !(n == JDK_LAYER_LOADER_CLASS && ctx.is_jdk_only()))
                || n.starts_with("sun/misc/Launcher$")
        })
}

///
/// # 2026-08-22: step 1 used to be "built-in loaders ARE the global classpath"
///
/// It is not true of the application loader, and `RLangPackages` failed its
/// FIRST check on it, in BOTH modes:
///
/// ```text
///   appLoader.getDefinedPackage("java.lang")   HotSpot null   CratonVM java.lang
///   appLoader.getDefinedPackage("java.util")   HotSpot null   CratonVM java.util
///   platform .getDefinedPackage("java.lang")   HotSpot null   CratonVM java.lang
///   appLoader.getDefinedPackage("no.such")     HotSpot null   CratonVM null
/// ```
///
/// `java.lang` is defined by the BOOT loader. The global probe concatenates
/// bootstrap + extension + application, so the application loader found the
/// boot image's `java/lang/*.class` and fabricated a `Package` — the same
/// loader-identity error this function was written to fix for URLClassLoader,
/// left in place for the built-ins by the very branch that skipped them.
///
/// The `loader == None` arm above is untouched on purpose: it IS the boot
/// loader, and answering `true` for `java.lang` there is correct.
pub(crate) fn package_class_files_visible_to_loader(
    ctx: &mut dyn NativeContext,
    loader: Option<ObjectRef>,
    package_name: &str,
    class_glob: &str,
) -> bool {
    let Some(loader) = loader else {
        // No receiver object at all: this IS the boot loader, so ask the boot
        // loader's own definition question before falling back.
        if let Some(answer) =
            builtin_loader_defines_package(ctx, BuiltinLoaderKind::Boot, package_name)
        {
            return answer;
        }
        return !ctx.find_all_resource_urls(class_glob).is_empty();
    };
    let loader_class = ctx.class_name_of_id(ctx.class_id_of_object(loader));
    if loader_is_builtin(ctx, loader) {
        // The MODULE route first: for the boot and platform loaders it is the
        // only route there is. A class-path segment probe cannot answer for
        // them at all -- the boot image is a jimage, and a `java/sql/*.class`
        // glob over it returns nothing, which is why `java.sql` read `null` on
        // the platform loader and `java.lang` read `null` on the boot loader
        // (taking `Package.getPackage` down with it) until this arm existed.
        if let Some(kind) = builtin_loader_kind(loader_class.as_deref()) {
            if let Some(answer) = builtin_loader_defines_package(ctx, kind, package_name) {
                return answer;
            }
        }
        return match builtin_loader_segment(loader_class.as_deref()) {
            // The application loader's `-cp` segment, AND a loaded class. The
            // segment probe alone answers the visibility question this whole
            // function exists to stop answering: HotSpot's
            // `app.getDefinedPackage("com.example.app")` is `null` until a
            // class in it is defined and non-null after, and a class-file glob
            // cannot tell those two instants apart. Spring's
            // `BeanDefinitionLoader.findPackage` — the caller the app arm was
            // written for — loads a class from the package before asking
            // again, so it keeps working on the answer HotSpot gives it.
            Some(segment) => {
                !ctx.find_resource_urls_in_segment(class_glob, segment)
                    .is_empty()
                    && ctx.any_loaded_class_in_package(&package_name.replace('.', "/"))
            }
            // The boot loader, or a built-in shape this VM does not recognise:
            // the historical global probe, unchanged.
            None => !ctx.find_all_resource_urls(class_glob).is_empty(),
        };
    }
    if !loader_local_resource_urls(ctx, loader, class_glob).is_empty() {
        return true;
    }
    if loader_has_recorded_url_set(ctx, loader) {
        return false;
    }
    // Step 4 -- a custom loader this VM has no URL view of -- was the LAST arm
    // still answering the visibility question this function exists to stop
    // answering. `new ClassLoader(null) {}`, which defines nothing at all,
    // claimed every application package on the process class path:
    //
    //   custom.getDefinedPackage("com.example.app")   HotSpot null   was: a Package
    //
    // Ask the loader's OWN definitions instead. A user-defined loader has a
    // namespace id of its own, and `any_loaded_class_in_package_for_loader`
    // answers exactly "did THIS loader define a class in this package" -- which
    // is `getDefinedPackage`'s contract, and is what the loaders this arm was
    // written for actually need:
    //
    // * ByteBuddy's `JavaDispatcher$DynamicClassLoader` asks about the package
    //   it has just defined `Invoker` into, so it still answers non-null;
    // * `GroovyClassLoader.definePackageInternal` reads
    //   `getDefinedPackage(p) == null` before `definePackage(p, ...)`. The
    //   FIRST class in a package answers null (as on HotSpot, and as the
    //   caller wants), the second answers non-null and the duplicate
    //   `definePackage` -- `IllegalArgumentException: <pkg>` -- is skipped.
    //
    // A loader with no namespace id of its own (id < 3: it delegates to the
    // built-in chain) keeps the historical global probe, unchanged.
    let namespace = loader_namespace_id(ctx, loader);
    if namespace >= cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED {
        return ctx
            .any_loaded_class_in_package_for_loader(&package_name.replace('.', "/"), namespace);
    }
    !ctx.find_all_resource_urls(class_glob).is_empty()
}

/// Which of the three built-in loaders this is, by class name.
///
/// Separate from [`builtin_loader_segment`] on purpose: that one answers
/// "which slice of the class path does this loader own", which is a question
/// only the application and platform loaders have an answer to, and it is the
/// WRONG question for a loader whose classes come out of a jimage.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BuiltinLoaderKind {
    Boot,
    Platform,
    Application,
}

pub(crate) fn builtin_loader_kind(loader_class: Option<&str>) -> Option<BuiltinLoaderKind> {
    match loader_class? {
        "jdk/internal/loader/ClassLoaders$AppClassLoader" | "sun/misc/Launcher$AppClassLoader" => {
            Some(BuiltinLoaderKind::Application)
        }
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader"
        | "sun/misc/Launcher$ExtClassLoader" => Some(BuiltinLoaderKind::Platform),
        "jdk/internal/loader/ClassLoaders$BootClassLoader" => Some(BuiltinLoaderKind::Boot),
        _ => None,
    }
}

/// The JDK's own boot / platform module tables, read out of the running image.
///
/// `jdk.internal.module.ModuleLoaderMap$Modules.{bootModules,platformModules}`
/// are the two `Set<String>` statics the JDK's own module system consults to
/// decide which built-in loader defines a module's packages. Reading them beats
/// keeping a hand-copied list in Rust: the answer then comes from the image the
/// run actually loaded, and a JDK that moves a module between the two tables
/// moves this VM with it.
///
/// Memoised on SUCCESS ONLY. A failure -- the class not yet initialisable this
/// early in boot, a stripped or non-JDK image -- answers `None` and is retried
/// on the next call, which selects the historical class-path probe. That is the
/// pre-existing behaviour, never a silent "this loader defines nothing".
struct BuiltinModuleSets {
    boot: std::collections::HashSet<String>,
    platform: std::collections::HashSet<String>,
}

const MODULE_LOADER_MAP_MODULES: &str = "jdk/internal/module/ModuleLoaderMap$Modules";

thread_local! {
    /// Re-entrancy guard for [`jdk_builtin_module_sets`].
    ///
    /// Reading the tables runs Java: a class initialisation and three
    /// `invoke_virtual`s. If anything on that path reached
    /// `getDefinedPackage` again the cache would still be cold and the read
    /// would recurse without bound. The guard answers `None` on re-entry,
    /// which selects the historical class-path probe for that one inner call —
    /// the same fail-soft every other failure arm here takes.
    static READING_MODULE_SETS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn jdk_builtin_module_sets(ctx: &mut dyn NativeContext) -> Option<Arc<BuiltinModuleSets>> {
    // `OrderedPlMutex` at the leaf level, not a raw `Mutex`: this crate
    // re-enters the VM, so a global lock with no `LockLevel` is a deadlock the
    // order checker cannot see, and `lock_discipline_ratchet` refuses one
    // ("Do NOT raise the baseline").
    //
    // `Scratch` is honest here rather than convenient. The guard is never held
    // across anything: the read below runs Java -- a class initialisation and
    // three `invoke_virtual`s -- and it runs with NOTHING locked, because both
    // acquisitions are single statements that publish or fetch an `Arc` and
    // end. Nothing is taken while holding this, which is exactly what L0 means.
    //
    // Keyed by VM (gc-common w10-f). The tables come out of the image THIS VM
    // loaded, and two VMs in one process need not load the same one; the
    // process-wide slot this replaced handed the first VM's tables to every
    // later VM.
    let vm = ctx.vm_identity();
    let cell = builtin_module_sets_cache();
    // Bound to a local FIRST. An `if let` scrutinee temporary lives to the end
    // of the whole `if let`, so a guard taken there would still be held in an
    // `else` arm the next edit adds.
    let hit = cell.lock().get(&vm).cloned();
    if let Some(hit) = hit {
        return Some(hit);
    }
    if READING_MODULE_SETS.with(|f| f.replace(true)) {
        return None;
    }
    let sets = jdk_builtin_module_sets_uncached(ctx);
    READING_MODULE_SETS.with(|f| f.set(false));
    let sets = sets?;
    cell.lock().insert(vm, Arc::clone(&sets));
    Some(sets)
}

/// `vm_identity -> that VM's module tables`.
type ModuleSetsByVm = std::collections::HashMap<usize, Arc<BuiltinModuleSets>>;

/// `vm_identity -> package -> is it platform-defined`.
type PlatformPackagesByVm =
    std::collections::HashMap<usize, std::collections::HashMap<String, bool>>;

/// [`jdk_builtin_module_sets`]' per-VM memo. Dropped per VM by
/// [`forget_vm_loader_singletons`].
fn builtin_module_sets_cache() -> &'static OrderedPlMutex<ModuleSetsByVm> {
    static CACHE: OnceLock<OrderedPlMutex<ModuleSetsByVm>> = OnceLock::new();
    CACHE.get_or_init(|| OrderedPlMutex::new(ModuleSetsByVm::new(), LockLevel::Scratch))
}

/// [`package_is_platform_defined`]'s memo. Per VM for the reason
/// [`jdk_builtin_module_sets`] gives. Dropped per VM by
/// [`forget_vm_loader_singletons`].
fn platform_package_memo() -> &'static OrderedPlMutex<PlatformPackagesByVm> {
    static MEMO: OnceLock<OrderedPlMutex<PlatformPackagesByVm>> = OnceLock::new();
    MEMO.get_or_init(|| OrderedPlMutex::new(PlatformPackagesByVm::new(), LockLevel::Scratch))
}

fn jdk_builtin_module_sets_uncached(ctx: &mut dyn NativeContext) -> Option<Arc<BuiltinModuleSets>> {
    let cid = ctx
        .ensure_class_initialized(MODULE_LOADER_MAP_MODULES)
        .ok()
        .or_else(|| ctx.class_id_by_name(MODULE_LOADER_MAP_MODULES))?;
    let boot = read_static_string_set(ctx, cid, "bootModules")?;
    let platform = read_static_string_set(ctx, cid, "platformModules")?;
    Some(Arc::new(BuiltinModuleSets { boot, platform }))
}

/// One `static final Set<String>` field, walked into a Rust set.
///
/// The set and its iterator are held as GLOBAL ROOTS across the `invoke_virtual`
/// calls rather than as raw `ObjectRef`s: `iterator()`/`next()` allocate, so a
/// collection can move both between one call and the next, and a moved receiver
/// is the shape that has cost this codebase whole sessions. Runs once per VM.
fn read_static_string_set(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    field: &str,
) -> Option<std::collections::HashSet<String>> {
    let index = ctx.static_field_index_by_name(class_id, field)?;
    let set = match ctx.get_static_field(class_id, index) {
        Value::Object(Some(set)) => set,
        _ => return None,
    };
    let set_root = ctx.add_global_root(set);
    let out = read_string_set_rooted(ctx, set_root);
    ctx.remove_global_root(set_root);
    out
}

fn read_string_set_rooted(
    ctx: &mut dyn NativeContext,
    set_root: usize,
) -> Option<std::collections::HashSet<String>> {
    let set = ctx.resolve_global_root(set_root)?;
    let iterator = match ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(it)))) => it,
        _ => return None,
    };
    let it_root = ctx.add_global_root(iterator);
    let mut out = std::collections::HashSet::new();
    // Bounded for the same reason `real_defined_package_names` is: a runaway
    // iterator must not hang a boot-time lookup. The JDK's two tables hold
    // ~50 names between them.
    for _ in 0..4096 {
        let Some(it) = ctx.resolve_global_root(it_root) else {
            break;
        };
        match ctx.invoke_virtual(it, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(1))) => {}
            _ => break,
        }
        let Some(it) = ctx.resolve_global_root(it_root) else {
            break;
        };
        let Ok(Some(Value::Object(Some(name)))) =
            ctx.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])
        else {
            break;
        };
        if let Some(name) = ctx.read_string(name) {
            out.insert(name);
        }
    }
    ctx.remove_global_root(it_root);
    // An EMPTY table is a failed read, not an answer: the JDK never ships one.
    // Reporting `None` keeps the caller on its historical probe instead of
    // freezing "nothing is defined" into the memo.
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Is `package_slash` in a module the JDK's own table assigns to the PLATFORM
/// loader?
///
/// Memoised per package. A package's module cannot change once the image is
/// loaded, and `Class.getClassLoader()` is asked far more often than there are
/// packages -- so the lookup is a short-string hash, not a module-registry walk
/// plus an `Arc` clone, on every call.
///
/// A package whose module is not yet known is NOT memoised: answering `false`
/// because the module registry had not been populated yet, and then freezing
/// it, is how a memo turns a boot-order accident into a permanent wrong answer.
fn package_is_platform_defined(ctx: &mut dyn NativeContext, package_slash: &str) -> bool {
    // `OrderedPlMutex` at the leaf level, not a raw `Mutex`, for the same
    // reason as `jdk_builtin_module_sets` below: this crate re-enters the VM,
    // so a global lock with no `LockLevel` is a deadlock the order checker
    // cannot see, and `lock_discipline_ratchet` refuses one outright ("Do NOT
    // raise the baseline").
    //
    // `Scratch` is the honest level and not merely a convenient one: the guard
    // is never held across anything. The lookup below runs Java — module-set
    // construction plus `module_for_package` — and it runs with NOTHING
    // locked, because both acquisitions are single statements that read or
    // publish and end.
    //
    // Per VM since gc-common w10-f (`platform_package_memo`).
    let vm = ctx.vm_identity();
    let memo = platform_package_memo();
    // Bound to a local FIRST: an `if let` scrutinee temporary lives to the end
    // of the whole `if let`, so a guard taken there is still held in an `else`
    // arm the next edit adds.
    let hit = memo
        .lock()
        .get(&vm)
        .and_then(|packages| packages.get(package_slash))
        .copied();
    if let Some(hit) = hit {
        return hit;
    }
    let Some(sets) = jdk_builtin_module_sets(ctx) else {
        return false;
    };
    let Some(module) = ctx.module_for_package(package_slash) else {
        return false;
    };
    let answer = sets.platform.contains(&module);
    memo.lock()
        .entry(vm)
        .or_default()
        .insert(package_slash.to_string(), answer);
    answer
}

/// The PLATFORM loader, when `class_name` (slash form) is an image class whose
/// module the JDK assigns to it -- and `None` for a boot-module class, an
/// application class, or an image this VM cannot read the tables out of.
///
/// # Not every image class is boot-loaded
///
/// This VM reads the whole jimage through one class path and tags every class
/// in it `ClassLoaderId::Bootstrap`, so `Class.getClassLoader()` answered
/// `null` for all of them. The JDK does not: `ModuleLoaderMap` splits the
/// image's modules between the boot and platform loaders, and the ~24 platform
/// modules (`java.sql`, `java.net.http`, `java.scripting`, `jdk.httpserver`, ...)
/// are DEFINED by `ClassLoaders$PlatformClassLoader`. MEASURED:
///
/// ```text
///   java.sql.Connection .getClassLoader()   HotSpot PlatformClassLoader   was null
///   javax.script.ScriptEngine...            HotSpot PlatformClassLoader   was null
///   java.lang.String    .getClassLoader()   HotSpot null                  null
///   java.awt.Color      .getClassLoader()   HotSpot null (java.desktop is BOOT)
/// ```
///
/// The direction matters the way it does for an application class reported as
/// bootstrap-loaded: `null` means "the boot loader owns this" to every caller
/// that keys a cache, picks a proxy loader, or decides a delegation parent.
///
/// This changes the REPORTED loader only. Class definition, resource
/// resolution and loader namespaces are untouched -- the class store stays
/// flat, and `is_builtin_loader_class` already keeps `Class.getResource*` off
/// the loader-delegation path for every built-in loader, the platform one
/// included.
pub(crate) fn platform_loader_for_image_class(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Option<ObjectRef> {
    let (package_slash, _) = class_name.rsplit_once('/')?;
    if !package_is_platform_defined(ctx, package_slash) {
        return None;
    }
    get_or_create_platform_loader(ctx).ok()
}

/// The PLATFORM loader when `module_name` is one the JDK's own table assigns to
/// it; `None` for a boot module, an application module, or an unreadable image.
///
/// The module-name form of [`platform_loader_for_image_class`], for
/// `Module.getClassLoader()`. The JDK keeps the two answers in step -- every
/// class in `java.sql` reports the same loader its module does -- so they have
/// to come from the same table or a caller can catch this VM contradicting
/// itself with two calls.
pub(crate) fn platform_loader_for_module(
    ctx: &mut dyn NativeContext,
    module_name: &str,
) -> Option<ObjectRef> {
    let sets = jdk_builtin_module_sets(ctx)?;
    if !sets.platform.contains(module_name) {
        return None;
    }
    get_or_create_platform_loader(ctx).ok()
}

/// Is `loader` the VM's platform-loader singleton?
///
/// Identity first, class name as the real-JDK fallback -- the same two-step
/// [`loader_namespace_id_at`] uses, and for the same reason: the JDK can
/// manufacture another `PlatformClassLoader` object before our singleton is
/// observed.
pub(crate) fn is_platform_loader_object(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    platform_loader_of(ctx.vm_identity()).is_some_and(|p| p.as_ptr() == loader.as_ptr())
        || ctx
            .class_name_of_id(ctx.class_id_of_object(loader))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader")
}

/// Does this built-in loader DEFINE `package_name` (dot form)?
///
/// `None` means "not answerable here" -- the module tables could not be read, or
/// the application loader, whose packages come off the `-cp` segment and whose
/// existing probe is right. The caller turns `None` back into that probe.
///
/// Two conjuncts, and both are load-bearing:
///
/// * the package's module is in the JDK's own table for THIS loader. Module
///   membership alone is a capability, not a definition;
/// * a class in that package is actually LOADED. HotSpot defines a package when
///   a loader defines a class in it, so `plat.getDefinedPackage("java.sql")` is
///   `null` until something loads a `java.sql` class and non-null after --
///   which is what [`NativeContext::any_loaded_class_in_package`] answers.
///
/// A package in NO named module (the class path's unnamed module) is defined by
/// neither the boot nor the platform loader, so those two answer a definite
/// `false` rather than falling through to a global probe that would hand the
/// boot loader every application package on the class path.
fn builtin_loader_defines_package(
    ctx: &mut dyn NativeContext,
    kind: BuiltinLoaderKind,
    package_name: &str,
) -> Option<bool> {
    if kind == BuiltinLoaderKind::Application {
        return None;
    }
    let sets = jdk_builtin_module_sets(ctx)?;
    let slash = package_name.replace('.', "/");
    let table = match kind {
        BuiltinLoaderKind::Boot => &sets.boot,
        BuiltinLoaderKind::Platform => &sets.platform,
        BuiltinLoaderKind::Application => unreachable!("returned above"),
    };
    let in_table = ctx
        .module_for_package(&slash)
        .is_some_and(|module| table.contains(&module));
    if !in_table {
        return Some(false);
    }
    Some(ctx.any_loaded_class_in_package(&slash))
}

/// Which class-path segment a built-in loader OWNS, or `None` for the boot
/// loader (and any built-in shape not listed here).
///
/// Segment numbering is `ClassManager::find_resource_urls_in_segment`'s, which
/// is in turn `next_resource_url_from`'s: 0 bootstrap, 1 extension, 2
/// application.
///
/// `None` is the SAFE answer, not a gap: it selects the historical global
/// probe, so an unrecognised built-in behaves exactly as it did before this
/// function existed. Only the two loaders we can name positively are narrowed.
fn builtin_loader_segment(loader_class: Option<&str>) -> Option<u8> {
    match loader_class? {
        // The application loader owns `-cp` and nothing else.
        "jdk/internal/loader/ClassLoaders$AppClassLoader" | "sun/misc/Launcher$AppClassLoader" => {
            Some(2)
        }
        // The platform loader's extension segment, which is empty on a normal
        // run. It is reached only when the module tables above could NOT be
        // read: `builtin_loader_defines_package` answers for both the platform
        // and the boot loader before this match, and a segment probe cannot
        // answer for either of them anyway — the boot image is a jimage, and a
        // `java/sql/*.class` glob over it returns nothing.
        //
        // That was the residual the 2026-08-22 narrowing knowingly shipped
        // (`java.sql` read `null` on the platform loader), and it was wider
        // than the one package it named. Closed 2026-09-01 by the module route;
        // this arm is the fail-soft under it, not the answer.
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader"
        | "sun/misc/Launcher$ExtClassLoader" => Some(1),
        _ => None,
    }
}

fn loader_local_resource_urls(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    resource_name: &str,
) -> Vec<String> {
    let paths = loader_constructor_url_paths(ctx, loader);
    if paths.is_empty() {
        if crate::nbflags().dbg_uclres {
            eprintln!("[UCLRES-DBG] loader={loader:?} resource={resource_name} paths=[]");
        }
        return Vec::new();
    }
    // `cached_class_path_for_paths` (keyed by the exact paths vector) already
    // covers the repeated-rebuild cost that a "pathing JAR" -- a jar with no
    // class entries, only a manifest `Class-Path:` naming the real dependency
    // jars, used by e.g. the spring-boot-suite-runner to dodge Windows'
    // command-line length limit -- would otherwise pay on every single
    // lookup once `ClassPath::new` honours that manifest attribute (see
    // `classloading/src/class_path.rs`) and expands to the module's full,
    // possibly 100s-of-jars dependency list.
    let urls = cached_class_path_for_paths(&paths).find_all_resource_urls(resource_name);
    if crate::nbflags().dbg_uclres {
        eprintln!(
            "[UCLRES-DBG] loader={loader:?} resource={resource_name} paths={paths:?} urls={urls:?}"
        );
    }
    urls
}

/// Per-(loader-namespace-id, class-name) locks serializing concurrent
/// `ucl_try_define_local_class` attempts for the SAME class through the SAME
/// `URLClassLoader` instance.
///
/// Without this, two threads racing to load the same not-yet-defined class
/// through the same loader (e.g. Spring Boot's
/// `OnClassCondition$ThreadedOutcomesResolver`, which evaluates
/// autoconfiguration conditions on a background thread pool while the main
/// thread needs the same framework classes through the same
/// `ModifiedClassPathClassLoader`) can both pass the "not yet defined" check
/// below before either calls `define_class_full`. The second call then hits
/// a genuine but spurious `IncompatibleClassChangeError`
/// ("already defined by <this> loader") wrapped as a `NoClassDefFoundError`
/// -- exactly the check-then-act race a real JVM's per-class
/// `getClassLoadingLock` exists to prevent. Keyed by loader namespace id
/// (not the loader `ObjectRef`, which the entry-point pin/GC dance below
/// makes awkward to hash) plus class name, so unrelated loaders defining a
/// same-named class concurrently are never serialized against each other.
fn url_classloader_define_locks() -> &'static Mutex<
    std::collections::HashMap<(u32, String), std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>>,
> {
    static INSTANCE: OnceLock<
        Mutex<
            std::collections::HashMap<
                (u32, String),
                std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>,
            >,
        >,
    > = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Removes a [`url_classloader_define_locks`] entry once the last thread
/// using it is done (gc-common w9-a).
///
/// The map used to keep one entry — a `String` plus an `Arc`'d mutex and
/// condvar — for every class ever defined through a `URLClassLoader`, in
/// every loader, in every VM of the process, forever: a per-define leak that
/// a webapp redeploy or a per-test loader grows by thousands of entries each
/// time. An entry is only needed while some thread is defining or waiting on
/// its `(namespace, name)` pair.
///
/// Must be declared AFTER the thread's own `Arc` clone (so it drops first,
/// while that clone still counts) and BEFORE the in-progress guard (so the
/// flag is cleared and waiters are woken before the map lock is taken).
/// Two strong references then mean "the map's and mine": nobody else holds
/// the entry, and nobody can take a new clone meanwhile, because clones are
/// only taken under the map lock this drop holds. A waiter that still holds
/// a clone keeps the entry, and removes it itself when it leaves. An entry
/// already replaced by a newer one for the same key is left alone.
struct DefineLockEntryRelease {
    key: (u32, String),
    lock_addr: usize,
}

impl Drop for DefineLockEntryRelease {
    fn drop(&mut self) {
        let mut locks = url_classloader_define_locks()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let unused = locks.get(&self.key).is_some_and(|entry| {
            std::sync::Arc::as_ptr(entry) as usize == self.lock_addr
                && std::sync::Arc::strong_count(entry) == 2
        });
        if unused {
            locks.remove(&self.key);
        }
        // Two threads leaving the same entry at once can each still see the
        // other's clone and both keep it; it is then held by the map alone.
        // New clones are only taken under this lock and keep the count at 2
        // or more, so a count of 1 means "abandoned". Sweep those too; with
        // the removal above the map holds only in-flight pairs, so this pass
        // is over a handful of entries.
        locks.retain(|_, entry| std::sync::Arc::strong_count(entry) > 1);
    }
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
    // `--jdk-only`: the loader's own `findClass` defines its classes. The
    // `URLClassLoader` family is retired there, so that is the JDK's
    // `URLClassLoader.findClass` over its real `URLClassPath`, which reads the
    // class through the URL's handler and passes its `ProtectionDomain` to
    // `defineClass`. This shortcut defined the class from the loader's recorded
    // URLs itself, with neither. Callers treat `None` as "not handled here" and
    // go on to `findClass`.
    if ctx.is_jdk_only() {
        return None;
    }
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_UCLTRACE").is_ok() {
        let loader_cid = ctx.class_id_of_object(loader);
        let loader_class = ctx.class_name_of_id(loader_cid).unwrap_or_default();
        let paths = loader_constructor_url_paths(ctx, loader);
        eprintln!(
            "[UCLTRACE] name={internal_name} loader_ptr={:?} loader_class={loader_class} n_paths={} paths={:?}",
            loader.as_ptr(),
            paths.len(),
            paths,
        );
    }
    if let Some(mirror) = find_loaded_class_for_loader(ctx, loader, internal_name) {
        return Some(Ok(Some(Value::Object(Some(mirror)))));
    }

    // Serialize concurrent definers of this exact (loader, name) pair -- see
    // `url_classloader_define_locks`. `loader` is freshly passed in by the
    // caller (not yet pinned across any GC-unsafe window), so reading its
    // namespace id here is exactly as safe as the `find_loaded_class_for_loader`
    // probe just above.
    let define_lock_id = loader_namespace_id(ctx, loader);
    let define_lock_key = (define_lock_id, internal_name.to_string());
    let release_key = define_lock_key.clone();
    let define_lock = {
        let mut locks = url_classloader_define_locks()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        locks
            .entry(define_lock_key)
            .or_insert_with(|| std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new())))
            .clone()
    };
    let _define_lock_release = DefineLockEntryRelease {
        key: release_key,
        lock_addr: std::sync::Arc::as_ptr(&define_lock) as usize,
    };
    let (define_lock_mutex, define_lock_cvar) = &*define_lock;
    let mut in_progress = define_lock_mutex.lock().unwrap_or_else(|e| e.into_inner());
    // GC-safety: this loop WAITS on a condvar, which is the widest window there
    // is -- a peer thread's collection is exactly what runs while this one is
    // parked -- and then re-probes with `loader`, a bare Rust local carried in
    // from outside. `find_loaded_class_for_loader` can itself allocate, so even
    // without the wait the second turn would be reading a pre-GC address. Pin
    // it and re-read at the top of each turn.
    //
    // gc-common w9-a: EVERY return from here on releases this pin. Eight of
    // the early returns below used to leave it behind, so the loader stayed
    // rooted (and the pin slot occupied) until the enclosing native returned.
    let loader_pin = ctx.pin_native_root(loader);
    while *in_progress {
        let (guard, timeout) = define_lock_cvar
            .wait_timeout(in_progress, std::time::Duration::from_secs(30))
            .unwrap_or_else(|e| e.into_inner());
        in_progress = guard;
        let loader = ctx.read_native_pin(loader_pin, loader);
        // The other thread may have finished defining it (success -- return
        // its result) or failed (we should try ourselves rather than loop
        // forever on a definition that will never arrive).
        if let Some(mirror) = find_loaded_class_for_loader(ctx, loader, internal_name) {
            ctx.unpin_native_roots(loader_pin);
            return Some(Ok(Some(Value::Object(Some(mirror)))));
        }
        if timeout.timed_out() {
            break;
        }
    }
    // Double-checked probe under the define lock. The pre-lock
    // `find_loaded_class_for_loader` above can miss and this thread still lose
    // the race: another thread may define `(loader, internal_name)` and CLEAR
    // `in_progress` in the window between that probe and our acquiring the
    // mutex, so the `while *in_progress` wait loop (which does re-probe) never
    // runs even once. Defining again then fails with
    // `IncompatibleClassChangeError: already defined by user-defined(N)
    // loader`, which this function reports as `ClassFormatError` -- and for an
    // isolated loader `resolve_class_loader_aware` turns any `findClass`
    // failure into a hard `NoClassDefFoundError` with no global fallback. Seen
    // as a nondeterministic `NoClassDefFoundError:
    // org.springframework.boot.autoconfigure.condition.ConditionOutcome` under
    // Spring Boot's `ModifiedClassPathClassLoader`, where
    // `OnClassCondition$ThreadedOutcomesResolver` evaluates half the
    // auto-configuration conditions on a second thread and races the main one
    // for exactly these classes. HotSpot has no such window: `loadClass`
    // re-checks `findLoadedClass` after taking `getClassLoadingLock(name)`.
    let loader = ctx.read_native_pin(loader_pin, loader);
    if let Some(mirror) = find_loaded_class_for_loader(ctx, loader, internal_name) {
        ctx.unpin_native_roots(loader_pin);
        return Some(Ok(Some(Value::Object(Some(mirror)))));
    }
    *in_progress = true;
    drop(in_progress);

    /// Clears the in-progress flag and wakes any waiters, including on an
    /// unwind, so a panic mid-define doesn't strand other threads on the
    /// 30s wait forever.
    struct DefineInProgressGuard<'a> {
        mutex: &'a Mutex<bool>,
        cvar: &'a std::sync::Condvar,
    }
    impl<'a> Drop for DefineInProgressGuard<'a> {
        fn drop(&mut self) {
            let mut in_progress = self.mutex.lock().unwrap_or_else(|e| e.into_inner());
            *in_progress = false;
            self.cvar.notify_all();
        }
    }
    let _define_in_progress_guard = DefineInProgressGuard {
        mutex: define_lock_mutex,
        cvar: define_lock_cvar,
    };

    let resource_name = format!("{internal_name}.class");
    let paths = loader_constructor_url_paths(ctx, loader);
    // Keep the source metadata coupled to the exact classpath that supplied
    // the bytes. Falling back to ClassManager's process-wide lookup after a
    // successful local definition can attach a same-named application JAR as
    // this class's CodeSource (for example, a URLClassLoader override JAR).
    let local_class_path = (!paths.is_empty()).then(|| cached_class_path_for_paths(&paths));
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
    // gen r4w3/rooting: `fetch_http_resource` enters a blocking region, where
    // a peer thread's moving collection can run; refresh `loader` from the pin
    // taken above before any further use.
    let loader = ctx.read_native_pin(loader_pin, loader);

    let bytes = match bytes {
        Some(b) => b,
        None if http_bases.is_empty() => {
            // A class dynamically appended to the BOOTSTRAP search
            // (`Instrumentation.appendToBootstrapClassLoaderSearch` — e.g.
            // Mockito's inline mock maker injecting `MockMethodDispatcher`/
            // `MockMethodAdvice`) is visible to every loader via real
            // parent-delegation semantics (an isolated `URLClassLoader`'s
            // parent is null, i.e. the bootstrap loader itself — NOT "no
            // parent at all"). Defer to the caller's own fallback (which
            // consults the global class store, itself searching the
            // bootstrap path) instead of making the miss authoritative here.
            if url_classloader_isolated_from_app(ctx, loader)
                && !ctx.is_bootstrap_appended_class(internal_name)
            {
                let exception = crate::jboss_module_loader::alloc_single_message_exception(
                    ctx,
                    "java/lang/ClassNotFoundException",
                    1,
                    internal_name,
                );
                ctx.unpin_native_roots(loader_pin);
                return Some(exception.and_then(|exception| {
                    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                        exception,
                    ))
                }));
            }
            ctx.unpin_native_roots(loader_pin);
            return None;
        }
        None => {
            let exc = crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/ClassNotFoundException",
                1,
                internal_name,
            );
            ctx.unpin_native_roots(loader_pin);
            return Some(exc.and_then(|exc| {
                Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exc,
                ))
            }));
        }
    };

    if url_classloader_isolated_from_app(ctx, loader) {
        if let Err(error) =
            crate::lang_system::preload_isolated_loader_supertypes(ctx, loader, &bytes)
        {
            ctx.unpin_native_roots(loader_pin);
            return Some(Err(error));
        }
    }

    // gen r4w3/rooting: do NOT re-pin `loader` here -- after
    // `preload_isolated_loader_supertypes` (class loading, GC-capable) the
    // local is already stale, and a fresh pin would root (and then hand back)
    // the vacated address. Read through the pin taken before the wait loop.
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
            register_defining_loader(ctx.vm_identity(), cid.as_u32(), loader_live);
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        // Matches BOTH renderings. `define_class_full` hands this boundary a
        // `String` built with `format!("{e:?}")`, so the test is on the VmError's
        // DEBUG text. The duplicate raise site now produces
        // `DuplicateClassDefinition { .. }` (HotSpot throws
        // `java.lang.LinkageError` itself) where it used to produce an
        // `IncompatibleClassChangeError` whose message read "already defined
        // by" -- and this arm keyed on that wording, so correcting the type
        // without touching it here would have silently turned a RECOVERED
        // concurrent-definition race into a hard failure. The older spelling is
        // kept because this same arm serves other producers of that text.
        Ok(Err(msg))
            if msg.contains("DuplicateClassDefinition") || msg.contains("already defined by") =>
        {
            // Benign concurrent-definition race: ANOTHER thread (e.g. a
            // background thread pool eagerly resolving classes, or a
            // recursive supertype/interface resolution nested inside a
            // DIFFERENT class's `define_class_full` -- see the sibling fix
            // in `classloading::ClassManager`'s `resolve_supertype`)
            // independently defined this exact (loader, name) pair between
            // our `find_loaded_class_for_loader` pre-check above and this
            // `define_class_full` call actually completing. The
            // `url_classloader_define_locks` mutex above only serializes
            // OTHER callers of THIS function for the same name; it cannot
            // see a concurrent definer that reached `define_class_full`
            // through a different, unlocked path. Recover by returning the
            // winner's already-registered mirror instead of surfacing a
            // spurious `NoClassDefFoundError`.
            let loader_live = ctx.read_native_pin(loader_pin, loader);
            match find_loaded_class_for_loader(ctx, loader_live, internal_name) {
                Some(mirror) => Ok(Some(Value::Object(Some(mirror)))),
                None => {
                    tracing::warn!(
                        "URLClassLoader.findClass({internal_name}) define failed: {msg}"
                    );
                    Err(cratonvm_types::error::LinkageError::ClassFormatError {
                        class_name: internal_name.to_string(),
                        message: format!("URLClassLoader.findClass: {msg}"),
                    }
                    .into())
                }
            }
        }
        Ok(Err(msg)) => {
            // Backstop for the same race as the double-checked probe above,
            // covering definers that do NOT go through this function's lock
            // (`Lookup`/`Unsafe.defineClass`, `preload_isolated_loader_super`
            // `types`, a parent loader's own defining path). If the class is
            // now defined under this loader, the "already defined" error means
            // we merely LOST the race -- JVMS SS5.3.5 says the loser observes
            // the winner's class, not a linkage error. Returning it is what
            // HotSpot's `defineClass`-under-`getClassLoadingLock` does.
            // gen r4w3/rooting: `define_class_full` is GC-capable; re-read.
            let loader_live = ctx.read_native_pin(loader_pin, loader);
            if let Some(mirror) = find_loaded_class_for_loader(ctx, loader_live, internal_name) {
                tracing::debug!(
                    "URLClassLoader.findClass({internal_name}) lost a define race ({msg}); \
                     returning the winning definition"
                );
                ctx.unpin_native_roots(loader_pin);
                return Some(Ok(Some(Value::Object(Some(mirror)))));
            }
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
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        // A CLOSED loader finds nothing new — `URLClassLoader.close()` shuts its
        // `URLClassPath`, and this native is what stands in for that search in
        // real-JDK mode (the `ucp` CratonVM hands the loader is never
        // populated, so closing it has no effect on its own).
        if ucl_is_closed(ctx, this) {
            return Ok(Some(Value::Object(None)));
        }
    }
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
            return Ok(Some(Value::Object(Some(url?))));
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
        return Ok(Some(Value::Object(Some(url?))));
    }
    match ctx.find_resource(resource_name) {
        Some(_) => {
            let spec = format!("classpath:{name}");
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, &spec);
            Ok(Some(Value::Object(Some(url?))))
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
) -> Result<ObjectRef, MethodCallFailed> {
    if !is_array_list_object(ctx, custom_list) {
        return match std_enum {
            Some(e) => Ok(e),
            // `unwrap_or_else` cannot carry the refusal out of its closure,
            // and building the empty enumeration is now fallible.
            None => empty_enumeration_impl(ctx),
        };
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
    let arr = ctx.read_native_pin(p_arr, arr);
    let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    ctx.unpin_native_roots(p_custom); // releases p_custom, p_std, p_arr
    Ok(enm)
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
    // `close()` makes this loader incapable of discovering any further
    // resources. Do this before the shared flat-classpath scan: consulting
    // that scan after close leaks resources from unrelated live loaders.
    if ucl_is_closed(ctx, this) {
        return Ok(Some(Value::Object(Some(empty_enumeration_impl(ctx)?))));
    }
    let name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return cl_get_resources_impl(ctx, args, false),
    };
    let resource_name = name.trim_start_matches('/').to_string();

    let p_this = ctx.pin_native_root(this);

    // Does this receiver's OWN URL list answer the question? That decides what
    // an EMPTY local scan MEANS, and the two meanings need opposite handling:
    //
    //   * URL list knowable, scan empty  -> the resource genuinely is not on
    //     this loader's classpath. Returning it anyway is a leak.
    //   * URL list not knowable at all   -> the local scan could not run; the
    //     historical process-wide scan is all there is.
    //
    // Only the first case is new. It is what Spring Boot's
    // `ModifiedClassPathClassLoader` builds on purpose: it filters
    // `hibernate-validator-*.jar` / `logback-*.jar` out of its own `URL[]` so a
    // `@ClassPathExclusions` test sees a classpath without them. Falling back to
    // the flat scan handed that jar's `META-INF/services` entry straight back,
    // while `loadClass` still (correctly) refused the class it names --
    // `ServiceLoader` then read a registration for a provider it could not load
    // and raised `ServiceConfigurationError: ... Provider ... not found` where
    // HotSpot finds no providers at all. See
    // `classpath-exclusions-flat-scan-and-module-provides-leak-FIXED-20260810.md`.
    //
    // The SINGULAR `findResource` has drawn this line since the ModifiedClassPath
    // work (see its `object_extends(.., "java/net/URLClassLoader")` early return);
    // this is the plural half of the same rule, kept narrower so a loader whose
    // URLs CratonVM cannot see behaves exactly as before.
    let this_probe = ctx.read_native_pin(p_this, this);
    let has_own_urls = !loader_constructor_url_paths(ctx, this_probe).is_empty();

    // Run the standard flat-classpath scan first while the arguments are
    // still fresh; custom-handler/local probing below can allocate. Skipped
    // outright when the receiver answers for itself -- besides being the leak
    // above, it is a full process-wide walk per `findResources` call.
    let std_enum = if has_own_urls {
        None
    } else {
        cl_get_resources_impl(ctx, args, false)?
    };
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
        // gen r4w3/rooting: `build_synthetic_url` is GC-capable; keep `arr`
        // rooted and re-read it before every store and before the wrap.
        let p_arr = ctx.pin_native_root(arr);
        for (i, url) in local_urls.iter().enumerate() {
            let url_obj = crate::jboss_module_loader::build_synthetic_url(ctx, url)?;
            let arr = ctx.read_native_pin(p_arr, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(url_obj)));
        }
        let arr = ctx.read_native_pin(p_arr, arr);
        ctx.unpin_native_roots(p_arr);
        let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
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
        )?))),
        None => match local_ref.or(std_ref) {
            Some(e) => Some(Value::Object(Some(e))),
            // `std_enum` is `None` in exactly the `has_own_urls` case, so the
            // empty enumeration below is this loader's own authoritative "no
            // matches" -- not a dropped result.
            None => match std_enum {
                Some(e) => Some(e),
                None => Some(Value::Object(Some(empty_enumeration_impl(ctx)?))),
            },
        },
    };
    ctx.unpin_native_roots(p_this);
    Ok(result)
}

/// `URLClassLoader.getURLs()` — the URLs this loader was constructed with,
/// plus anything `addURL` appended. Nothing else.
///
/// It must NOT expand a jar's manifest `Class-Path`. The real JDK resolves
/// `Class-Path` lazily inside `URLClassPath`, never through this public
/// accessor, and the expansion is observable: it drops the referring jar
/// itself, invents entries for `Class-Path` names that do not exist on disk,
/// and re-percent-encodes an already-encoded entry (`project%20space` came
/// back as `project%2520space`). Spring Boot's `ChangeableUrls.fromClassLoader`
/// walks `getURLs()` and then expands each jar's manifest itself, so a
/// pre-expanded list made it report six directories where five were expected
/// — `ChangeableUrlsTests.urlsFromJarClassPathAreConsidered`.
///
/// The expansion was added for Spring Boot's `ModifiedClassPathClassLoader`
/// (`@ClassPathExclusions`), which is handed the suite runner's manifest-only
/// pathing JAR and would otherwise filter a class path different from the one
/// the loader searches. That never applied here: the application loader is
/// `jdk.internal.loader.ClassLoaders$AppClassLoader`, which is not a
/// `URLClassLoader`, so `ModifiedClassPathClassLoader.doExtractUrls` reads
/// `ManagementFactory.getRuntimeMXBean().getClassPath()` and never reaches
/// this native. `probes/DevtoolsChangeableUrlsProbe.java` covers the contract;
/// `probes/RuntimeClassPathProbe.java` shows the app-loader shape.
pub(crate) fn ucl_get_urls(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Object(Some(ucp)) = ctx.get_field_by_name(this, "ucp") {
        if let Some(result) = ucp_path_urls(ctx, ucp) {
            return Ok(Some(Value::Object(Some(result))));
        }
    }
    // Everything below reads the SYNTHETIC slot indices. On a real
    // `java.net.URLClassLoader` (`javap -p`, superclass fields first:
    // `java.lang.ClassLoader` declares `parent`(0) `name`(1) `unnamedModule`(2)
    // `nameAndId`(3) `parallelLockMap`(4) …, then `SecureClassLoader.pdcache`,
    // then `ucp` and `closeables`) slot 2 is a `Module` and slot 4 a
    // `ConcurrentHashMap`, where this module means URL-COUNT and URL-ARRAY.
    //
    // W7-7 left these raw reads unguarded as benign, and the arithmetic did
    // hold: slot 2 reads back as a reference, the `Value::Int` arm misses, the
    // count lands 0, and the slot-4 CHM is never indexed. But "right because
    // the tag happened not to match" is one layout change away from
    // `array_length(ConcurrentHashMap)`, and it is not the reason the code is
    // correct — the layout is. Say so. A real-layout loader's URLs are the
    // `ucp.path` list read above and nothing else, so the answer here is the
    // same empty array the count-0 arithmetic already produced.
    // `URL[]`, the declared type (i6-L2), on every return below.
    let url_component = crate::lang_class::reflection_component_id(ctx, "java/net/URL");
    if !cl_has_synthetic_layout(ctx, this) {
        let empty = ctx.new_ref_array(url_component, 0);
        return Ok(Some(Value::Object(Some(empty))));
    }
    let count = match ctx.get_field(this, UCL_URL_COUNT) {
        Value::Int(n) => n.max(0) as usize,
        _ => 0,
    };
    // Synthetic-JDK path: URLs live in the per-instance slots (`ucl_setup`/
    // `ucl_add_url`). Keep the legacy raw-slot fallback for old placeholders —
    // but only when the `ucp` is itself a fabricated stub, because on a real
    // `URLClassPath` slot 0 is the `path` ArrayList and `array_length` of an
    // ArrayList is not a URL count.
    if count == 0 {
        if let Value::Object(Some(ucp)) = ctx.get_field_by_name(this, "ucp") {
            if let Value::Object(Some(stashed)) = ctx.get_field(ucp, UCP_STASHED_URLS) {
                if ucp_stash_is_reference_array(ctx, stashed) {
                    let n = ctx.array_length(stashed);
                    // GC-safety (Family-1 stale ObjectRef): `stashed` is read
                    // out of the heap, so unlike `this`/`args` it is not rooted
                    // by the interpreter frame. `new_array` below can trigger a
                    // moving collection, after which the pre-allocation
                    // `stashed` would name the old address and the loop would
                    // copy from a stale object. Pin across the allocation and
                    // re-read through the pin, exactly as `ucl_add_url` does
                    // for `urls_arr`/`url_obj`. `n` is a plain length, so it
                    // survives the move; `result` is freshly allocated and the
                    // loop below allocates nothing, so neither needs a pin.
                    let stashed_pin = ctx.pin_native_root(stashed);
                    let result = ctx.new_ref_array(url_component, n);
                    let stashed = ctx.read_native_pin(stashed_pin, stashed);
                    ctx.unpin_native_roots(stashed_pin);
                    for i in 0..n {
                        let url = ctx.get_array_element(stashed, i);
                        ctx.set_array_element(result, i, url);
                    }
                    return Ok(Some(Value::Object(Some(result))));
                }
            }
        }
    }
    // Copy stored URLs into a new array of the exact size.
    //
    // GC-safety: `new_array` is the only allocation, and `urls_arr` is read out
    // of the heap *after* it, so there is no pre-allocation array reference
    // left to go stale. The loop allocates nothing. `this` DOES need the pin:
    // the interpreter frame roots the argument, but a moving collection updates
    // the frame's slot, not this Rust copy of it (gc-common w20-f; the comment
    // here used to say the frame made `this` safe on its own).
    let this_pin = ctx.pin_native_root(this);
    let result = ctx.new_ref_array(url_component, count);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Value::Object(Some(urls_arr)) = ctx.get_field(this, UCL_URLS_ARRAY) {
        for i in 0..count {
            let url = ctx.get_array_element(urls_arr, i);
            ctx.set_array_element(result, i, url);
        }
    }
    Ok(Some(Value::Object(Some(result))))
}

fn ucl_add_url(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;

    // Check if closed
    if ucl_is_closed(ctx, this) {
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
                //
                // gc-common w20-f: `this` too -- the new array and the count
                // were stored through the receiver's pre-allocation address.
                let this_pin = ctx.pin_native_root(this);
                let urls_arr_pin = ctx.pin_native_root(urls_arr);
                let url_obj_pin = ctx.pin_native_root(url_obj);
                let new_cap = arr_len * 2;
                let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                this = ctx.read_native_pin(this_pin, this);
                let urls_arr = ctx.read_native_pin(urls_arr_pin, urls_arr);
                url_obj = ctx.read_native_pin(url_obj_pin, url_obj);
                ctx.unpin_native_roots(this_pin);
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

pub(crate) fn ucl_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // The identity-keyed state is authoritative for both synthetic and real
    // URLClassLoader layouts. Writing synthetic slot 3 on a real JDK object
    // would instead corrupt an implementation-private field.
    ucl_mark_closed(ctx, this);
    Ok(None)
}

fn ucl_new_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let urls = args.first().copied().unwrap_or(Value::Object(None));
    let obj = alloc_url_classloader(ctx)?;
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
    let obj = alloc_url_classloader(ctx)?;
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
    Ok(Some(Value::Object(Some(obj?))))
}

/// `privateLookupIn(targetClass, caller)` — registered here on
/// `MethodHandles$Lookup`, which is **not the class that declares it**.
///
/// Reachability, re-derived rather than inherited. `javap -p
/// java.lang.invoke.MethodHandles` declares
/// `public static Lookup privateLookupIn(Class<?>, Lookup)`; `javap -p
/// java.lang.invoke.MethodHandles$Lookup` does not declare it at all. The
/// registry keys exactly on `(class, name, descriptor)` and only ever relaxes
/// the DESCRIPTOR (`find_with_descriptor_quirks`), never the class, and
/// `MethodHandles$Lookup` is a nested class — not a supertype of
/// `MethodHandles` — so it never appears on the static-resolution chain for
/// `MethodHandles.privateLookupIn`. (`synthetic_stub_superclass` puts
/// `MethodHandles` under `java/lang/Object` in synthetic mode too.) No
/// bytecode can name this triple and no in-tree caller does, so this
/// registration is **unreachable** — a stronger verdict than "not force
/// listed", and one the corrected registration-is-the-gate rule does not
/// disturb: that rule decides which implementation WINS for a triple, it does
/// not conjure a triple the language cannot spell.
///
/// The live copy is `lang_invoke.rs`'s registration on
/// `java/lang/invoke/MethodHandles`, whose `pli_enforce` carries the measured
/// OpenJDK 25.0.3 contract. This body is kept (never removed — standing
/// constraint) and brought into line with it so that a future registration on
/// the correct class key, or a force-list entry, cannot silently turn a
/// full-power grant back on. Two things it was getting wrong independently of
/// the access question:
///
/// * **It granted `LK_FULL_POWER` (0x5F, 95).** Re-measured for this lane on
///   OpenJDK 25.0.3: `privateLookupIn` answers **31** — `PUBLIC|PRIVATE|
///   PROTECTED|PACKAGE|MODULE`, i.e. [`LK_FULL_POWER_MODES`]. ORIGINAL is
///   dropped. Granting 95 hands out a mode bit the JDK never grants here.
/// * **The `target_class` `ObjectRef` was used after an allocation.**
///   `alloc_lookup` can run a moving GC, so the ref taken from `args` above it
///   may be stale by the time it is written into `lookupClass` — the Family-1
///   defect `lk_ensure_initialized` below already guards against.
///
/// The mode gate reproduces `pli_enforce`'s, valve included: refuse only on a
/// POSITIVELY read, nonzero, weak mode word. `lk_modes_of` answers 0 both for
/// a genuinely modeless Lookup and for one whose modes it could not read, and
/// refusing our "cannot tell" 0 would turn every Lookup this VM does not model
/// into an `IllegalAccessException`. The module `canRead`/`isOpen` gate is
/// deliberately NOT reproduced, for the reason `pli_enforce` records: CratonVM
/// has no module graph, so a faithful check would refuse calls the VM's own
/// machinery makes. Both omissions are one-directional — they can only admit
/// what HotSpot refuses, never refuse what HotSpot admits.
///
/// The two implementations must be collapsed onto `pli_enforce` (one
/// `pub(crate)` on it) rather than forked again; `lang_invoke.rs` is owned by
/// another lane. Same standing note as [`lk_in_modes`].
fn lk_private_lookup_in(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut target_class = args.first().copied().unwrap_or(Value::Object(None));
    let caller = args.get(1).copied().unwrap_or(Value::Object(None));
    // The caller's lookup class when the target is in another module (the
    // JDK's `newPreviousClass`; `private_lookup_in_module_refusal`).
    let mut prev_class: Option<ObjectRef> = None;

    // (1) `caller.allowedModes` is the JDK's FIRST dereference — measured: a
    //     null caller with a primitive target still raises the caller NPE, not
    //     the primitive `IllegalArgumentException`.
    let caller_ref = match caller {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot read field \"allowedModes\" because \"caller\" is null".to_string(),
                ),
            }
            .into());
        }
    };
    let modes = lk_modes_of(ctx, caller_ref);
    // (2) TRUSTED (-1) returns `new Lookup(targetClass)` from the top of the
    //     JDK method, before any check below.
    if modes != -1 {
        // (3) `targetClass.isPrimitive()`.
        let target_ref = match target_class {
            Value::Object(Some(o)) => o,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some(
                        "Cannot invoke \"java.lang.Class.isPrimitive()\" because \"targetClass\" is null"
                            .to_string(),
                    ),
                }
                .into());
            }
        };
        let target_name = crate::lang_class::mirror_class_name(ctx, target_ref);
        if matches!(
            crate::lang_class::native_class_is_primitive(ctx, &[Value::Object(Some(target_ref))]),
            Ok(Some(Value::Int(1)))
        ) {
            // `Class.toString()` of a primitive is bare — "int", "void" — so the
            // JDK's message has no "class " prefix.
            let name = target_name.unwrap_or_else(|| "?".to_string());
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: format!("{name} is a primitive class"),
                }
                .into(),
            );
        }
        // (4) `targetClass.isArray()`. Array mirrors are the ones whose name
        //     starts with '['; `Class.toString()` of an array IS prefixed and
        //     prints the binary name ("class [I", "class [Lp.Mate;").
        if let Some(name) = target_name {
            if name.starts_with('[') {
                return Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: format!("class {} is an array class", name.replace('/', ".")),
                    }
                    .into(),
                );
            }
        }
        // (5) the mode gate, with the `modes == 0` valve documented above.
        if modes != 0 && (modes & (LK_PRIVATE | LK_MODULE)) != (LK_PRIVATE | LK_MODULE) {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalAccessException {
                    message: "caller does not have PRIVATE and MODULE lookup mode".to_string(),
                }
                .into(),
            );
        }
        // (6) the module half (`--jdk-only`). It runs Java, so the target is
        //     read back from it.
        let (target_now, prev) = private_lookup_in_module_refusal(ctx, target_ref, caller_ref)?;
        target_class = Value::Object(Some(target_now));
        prev_class = prev;
    }

    // Root `target_class` across the allocation — `alloc_lookup` can move it.
    let pinned = match target_class {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    // Pinned after the target, so read and released first (a release frees
    // every pin from its handle onward).
    let prev_pinned = prev_class.map(|o| (ctx.pin_native_root(o), o));
    // Across modules the JDK drops `MODULE` (15, not 31).
    let modes = if prev_class.is_some() {
        LK_FULL_POWER_MODES & !LK_MODULE
    } else {
        LK_FULL_POWER_MODES
    };
    let obj = alloc_lookup(ctx, modes);
    let prev_class = match prev_pinned {
        Some((handle, o)) => {
            let current = ctx.read_native_pin(handle, o);
            ctx.unpin_native_roots(handle);
            Some(current)
        }
        None => None,
    };
    let target_class = match pinned {
        Some((handle, o)) => {
            let current = ctx.read_native_pin(handle, o);
            ctx.unpin_native_roots(handle);
            Value::Object(Some(current))
        }
        None => target_class,
    };
    let obj = obj?;
    ctx.set_field(obj, LK_LOOKUP_CLASS_REF, target_class);
    if prev_class.is_some() {
        ctx.set_field_by_name(obj, "prevLookupClass", Value::Object(prev_class));
    }
    Ok(Some(Value::Object(Some(obj))))
}

/// `--jdk-only`: the module half of `MethodHandles.privateLookupIn(targetClass,
/// caller)`, asked after its mode gate admitted `caller`. JDK 25:
///
/// ```text
/// if (targetModule != callerModule) {
///     if (!callerModule.canRead(targetModule))
///         throw new IllegalAccessException(callerModule + " does not read " + targetModule);
///     if (targetModule.isNamed() && !targetModule.isOpen(pn, callerModule))
///         throw new IllegalAccessException(targetModule + " does not open " + pn + " to " + callerModule);
/// }
/// ```
///
/// The natives never asked it, so `privateLookupIn(Object.class, lookup())`
/// from the class path answered a full-power lookup on `java.lang`
/// (`tools/probes/interp/L5/L5W44LoaderReview.java`, `lookup-define-java-lang`;
/// `L5W45PrivateLookupInOpens`; interpreter round i1 wave 45, lane L5). Asked
/// through the `Module` methods themselves (`getModule`, `canRead`, `isOpen`,
/// `toString`), which answer a layer module from the JDK's fields and a boot
/// module from the registry (`--add-opens` included), so this adds no second
/// copy of the module rules. `--compatible` asks nothing (its answer is
/// unchanged). A refusal is traced under `CRATONVM_DBG=access`.
///
/// Answers the target (read back: this runs Java) and, for an admitted
/// target of ANOTHER module, the caller's lookup class: the JDK then drops
/// `MODULE` and records that class as the new lookup's `prevLookupClass`
/// (`newModes &= ~Lookup.MODULE`, modes 15; interpreter round i1 wave 46,
/// lane L4). `None` for a same-module target, a TRUSTED caller,
/// `--compatible`, and every "cannot tell" exit, which keep the same-module
/// shape (modes 31, no previous lookup class).
pub(crate) fn private_lookup_in_module_refusal(
    ctx: &mut dyn NativeContext,
    target_class: ObjectRef,
    caller_lookup: ObjectRef,
) -> Result<(ObjectRef, Option<ObjectRef>), MethodCallFailed> {
    use cratonvm_native_api::{NativeHandle, NativeHandleScope};
    fn truthy(r: Option<Value>) -> bool {
        matches!(r, Some(Value::Int(v)) if v != 0)
    }
    /// `Module.toString()` of the module `h` roots.
    fn describe(
        scope: &mut NativeHandleScope<'_>,
        h: &NativeHandle,
    ) -> Result<String, MethodCallFailed> {
        let m = scope.get(h);
        Ok(match scope.invoke_virtual(m, "toString", "()Ljava/lang/String;", &[])? {
            Some(Value::Object(Some(s))) => scope.read_string(s).unwrap_or_default(),
            _ => String::new(),
        })
    }
    fn refuse(message: String) -> Result<(ObjectRef, Option<ObjectRef>), MethodCallFailed> {
        if crate::jboss_jdkspecific::dbg_layer_modules() {
            eprintln!("[ACCESS-DBG] privateLookupIn refused: {message}");
        }
        Err(cratonvm_types::error::RuntimeError::IllegalAccessException { message }.into())
    }
    // A TRUSTED caller returns `new Lookup(targetClass)` before any check.
    if !ctx.is_jdk_only() || lk_modes_of(ctx, caller_lookup) == -1 {
        return Ok((target_class, None));
    }
    let caller_class = match ctx.get_field_by_name(caller_lookup, "lookupClass") {
        Value::Object(Some(c)) => c,
        _ => match ctx.get_field(caller_lookup, LK_LOOKUP_CLASS_REF) {
            Value::Object(Some(c)) => c,
            _ => return Ok((target_class, None)),
        },
    };
    let mut scope = NativeHandleScope::new(ctx);
    let target_h = scope.root(target_class);
    let caller_class_h = scope.root(caller_class);
    let Some(Value::Object(Some(caller_module))) =
        scope.invoke_virtual(caller_class, "getModule", "()Ljava/lang/Module;", &[])?
    else {
        return Ok((scope.get(&target_h), None));
    };
    let caller_module_h = scope.root(caller_module);
    let target_now = scope.get(&target_h);
    let Some(Value::Object(Some(target_module))) =
        scope.invoke_virtual(target_now, "getModule", "()Ljava/lang/Module;", &[])?
    else {
        return Ok((scope.get(&target_h), None));
    };
    let target_module_h = scope.root(target_module);
    if target_module == scope.get(&caller_module_h) {
        return Ok((scope.get(&target_h), None));
    }
    let caller_now = scope.get(&caller_module_h);
    let target_now = scope.get(&target_module_h);
    let reads = scope.invoke_virtual(
        caller_now,
        "canRead",
        "(Ljava/lang/Module;)Z",
        &[Value::Object(Some(target_now))],
    )?;
    if !truthy(reads) {
        let caller_text = describe(&mut scope, &caller_module_h)?;
        let target_text = describe(&mut scope, &target_module_h)?;
        return refuse(format!("{caller_text} does not read {target_text}"));
    }
    let target_now = scope.get(&target_module_h);
    if !matches!(scope.get_field_by_name(target_now, "name"), Value::Object(Some(_))) {
        // An unnamed target module opens every package: admitted, across
        // modules.
        return Ok((scope.get(&target_h), Some(scope.get(&caller_class_h))));
    }
    let target_class_now = scope.get(&target_h);
    let Some(Value::Object(Some(pn))) =
        scope.invoke_virtual(target_class_now, "getPackageName", "()Ljava/lang/String;", &[])?
    else {
        return Ok((scope.get(&target_h), None));
    };
    let pn_text = scope.read_string(pn).unwrap_or_default();
    let target_now = scope.get(&target_module_h);
    let caller_now = scope.get(&caller_module_h);
    let open = scope.invoke_virtual(
        target_now,
        "isOpen",
        "(Ljava/lang/String;Ljava/lang/Module;)Z",
        &[Value::Object(Some(pn)), Value::Object(Some(caller_now))],
    )?;
    if truthy(open) {
        return Ok((scope.get(&target_h), Some(scope.get(&caller_class_h))));
    }
    let target_text = describe(&mut scope, &target_module_h)?;
    let caller_text = describe(&mut scope, &caller_module_h)?;
    refuse(format!("{target_text} does not open {pn_text} to {caller_text}"))
}

/// `publicLookup()`'s modes are **exactly** `UNCONDITIONAL` (0x20).
///
/// Verified against JDK 25 `java.base/java/lang/invoke/MethodHandles.java`:
/// `publicLookup()` returns `Lookup.PUBLIC_LOOKUP`, which is
/// `new Lookup(Object.class, null, UNCONDITIONAL)`, and `lookupModes()`
/// returns `allowedModes & ALL_MODES` (ALL_MODES includes UNCONDITIONAL).
/// This used to answer `PUBLIC|UNCONDITIONAL` (0x21) — a combination the JDK
/// itself treats as impossible: `Lookup.toString()` switches on the exact
/// mode word, has a `case UNCONDITIONAL` arm and no `PUBLIC|UNCONDITIONAL`
/// arm, and its `default:` branch asserts false.
///
/// Dropping the PUBLIC bit does not narrow access here:
/// `lang_invoke::lk_enforce_find_access` — the gate that actually runs — needs
/// no mode bit for a `public` member of a `public` class, and requires
/// `PRIVATE` for a non-public one, which this lookup never had. What it DOES
/// key on is `modes == UNCONDITIONAL` exactly, so leaving the PUBLIC bit set
/// would have skipped that arm and let `publicLookup()` reach public members
/// of package-private classes.
///
/// (This registration sits on `MethodHandles$Lookup`; the live
/// `MethodHandles.publicLookup()` static is `lang_invoke.rs`'s. Both now agree.)
fn lk_public_lookup(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = alloc_lookup(ctx, LK_UNCONDITIONAL);
    Ok(Some(Value::Object(Some(obj?))))
}

fn lk_lookup_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, LK_LOOKUP_CLASS_REF)))
}

/// `Lookup.previousLookupClass()` — the lookup class of the Lookup this one
/// was derived from by a MODULE-CROSSING `in()`, or null.
///
/// Measured on OpenJDK 25.0.3: it is null for `MethodHandles.lookup()`,
/// `publicLookup()`, `dropLookupMode(PRIVATE)`, `in(<the lookup class
/// itself>)` and `in(<a nestmate>)` — every lookup that never left its
/// module — and becomes non-null only once `in()` crosses a module boundary
/// (`lookup().in(String.class)` reports the original lookup class, and its
/// `toString()` renders as `java.lang.String/PrevLk/public`). CratonVM does
/// not model modules, and neither `alloc_lookup` nor `lk_in_method` ever
/// populates the field, so **null is the correct answer for every Lookup this
/// VM hands out**. What matters here is only that the answer is a REFERENCE.
///
/// The slot has to be chosen by layout, not assumed. `LK_PREVIOUS_LOOKUP_CLASS`
/// is 2, and on the real JDK 25 layout slot 2 is `allowedModes`, an `int`
/// (`javap -p java.lang.invoke.MethodHandles$Lookup`, instance fields in
/// declaration order: `lookupClass`(0), `prevLookupClass`(1),
/// `allowedModes`(2), `cachedProtectionDomain`(3)). Reading it raw therefore
/// returned an `Int` — the mode word, 95 for a full-power lookup — out of a
/// native whose descriptor is `()Ljava/lang/Class;`. A caller storing that
/// into a `Class` local holds a type-confused value, and the reference slot it
/// lands in is one the GC scans as an oop.
///
/// The witness is the same CLASS-side one `lk_real_allowed_modes_slot` uses,
/// so the two never disagree about which layout the receiver has.
fn lk_previous_lookup_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = match lk_real_prev_lookup_class_slot(ctx, this) {
        Some(slot) => ctx.get_field(this, slot),
        None => ctx.get_field(this, LK_PREVIOUS_LOOKUP_CLASS),
    };
    // Whatever the layout turned out to be, a `()Ljava/lang/Class;` native must
    // not return a primitive. A non-reference here means a layout this VM does
    // not model, and null is this method's own legal answer for "no previous
    // lookup class" — the answer the real JDK gives for every non-module-
    // crossing Lookup, which is all of them here.
    Ok(Some(match value {
        Value::Object(_) => value,
        _ => Value::Object(None),
    }))
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

/// JDK 25: `hasPrivateAccess() { return hasFullPrivilegeAccess(); }` --
/// `PRIVATE` and `MODULE`, not `PRIVATE` alone. The two differ only for a
/// lookup with `PRIVATE` and no `MODULE`, which a cross-module
/// `privateLookupIn` makes (modes 15; measured `false`,
/// `tools/probes/interp/L4/L4W46PrivateLookupInAcrossModules.java` row
/// `full-priv`; interpreter round i1 wave 46, lane L4).
fn lk_has_private_access(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_has_full_privilege_access(ctx, args)
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
    // wildfly-parallel-boot-stale-objectref-residual.md.
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
    // BAD BYTES ARE A `ClassFormatError`, not an `IllegalArgumentException`.
    // The `ClassLoader.defineClass` door three thousand lines above already
    // raises `LinkageError::ClassFormatError` here; these `Lookup` doors were
    // the two that did not, and a caller cannot catch what it is not thrown.
    // `ClassFormatError` is an `Error`; `IllegalArgumentException` is a
    // RuntimeException -- a bytecode generator that guards its emit with
    // `catch (ClassFormatError)` (which is what you write, because that is what
    // the JVM throws) sees nothing and lets a genuinely malformed class escape
    // as an unrelated runtime failure somewhere else.
    if class_bytes.len() < 8 || class_bytes[0..4] != CLASS_FILE_MAGIC {
        return Err(cratonvm_types::error::LinkageError::ClassFormatError {
            class_name: "<hidden>".to_string(),
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
            // A NULL array is an NPE, as `Objects.requireNonNull(bytes)` in the
            // JDK's own body gives. Measured on jdk-25.0.3.9-hotspot.
            return Err(RuntimeError::NullPointerException {
                message: Some("defineHiddenClass: bytes must not be null".to_string()),
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
    // Same rule as `Lookup.defineClass` above: malformed bytes are a
    // `ClassFormatError`, and an EMPTY array takes this path too (HotSpot
    // measures as `ClassFormatError`, not as a null-argument complaint).
    if class_bytes.len() < 8 || class_bytes[0..4] != CLASS_FILE_MAGIC {
        return Err(cratonvm_types::error::LinkageError::ClassFormatError {
            class_name: "<hidden>".to_string(),
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
    // gc-common w18-d: STRONG (ordinal 1) is no longer advisory. Without it
    // the class unloads with its mirror.
    let mut strong = false;
    if let Some(Value::Object(Some(options_arr))) = args.get(3) {
        let opt_count = ctx.array_length(*options_arr);
        for i in 0..opt_count {
            if let Value::Object(Some(opt)) = ctx.get_array_element(*options_arr, i) {
                if let Value::Int(ord) = ctx.get_field(opt, 0) {
                    if ord == 0 {
                        nestmate = true;
                    }
                    if ord == 1 {
                        strong = true;
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
        non_strong_hidden: !strong,
        ..Default::default()
    };
    let non_strong_hidden = opts.unloads_with_its_mirror();

    // cglib SEGV guard — JEP 371 hidden-class path is used by some
    // ASM frameworks (incl. byte-buddy when configured to use hidden
    // classes). If cglib ever routes here, short-circuit before define.
    if let Some(v) = cglib_guard_value(ctx, &original_name, &class_bytes) {
        // Wrap the placeholder mirror back into a Lookup so the caller
        // gets the contractual return type. If the placeholder is null,
        // fall through to the normal path (which will fail cleanly).
        if let Value::Object(Some(mirror)) = v {
            // gc-common w20-f: the placeholder mirror was stored at its
            // address from before the Lookup's allocation (the w18-d fix
            // below covered only the main path). Pinned across it.
            let guard_pin = ctx.pin_native_root(mirror);
            let lookup_alloc = alloc_lookup(ctx, LK_FULL_POWER);
            let mirror = ctx.read_native_pin(guard_pin, mirror);
            ctx.unpin_native_roots(guard_pin);
            let lookup: ObjectRef = lookup_alloc?;
            ctx.set_field(lookup, LK_LOOKUP_CLASS_REF, Value::Object(Some(mirror)));
            return Ok(Some(Value::Object(Some(lookup))));
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
            // initialize=true failures: see
            // `lang_system::exception_in_initializer_error`'s doc comment
            // (JVMS 5.5 — HotSpot raises `ExceptionInInitializerError`
            // wrapping the real `<clinit>` exception). Other errors are
            // IllegalArgumentException per JLS §5.3.5.
            if msg.contains("initialize after define failed") {
                return Err(crate::lang_system::exception_in_initializer_error(
                    ctx,
                    &hidden_name,
                    &msg,
                ));
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
    //
    // gc-common w18-d: the mirror is pinned across the Lookup's allocation.
    // It was held raw, so a moving collection there left the Lookup naming
    // the mirror's old address.
    //
    // gc-common w20-f: the pin was never released (it stayed until the
    // enclosing native returned) and leaked on the allocation's `?` exit.
    let mirror = ctx.get_class_mirror(cid);
    let mirror_pin = ctx.pin_native_root(mirror);
    let lookup_alloc = alloc_lookup(ctx, LK_FULL_POWER);
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    ctx.unpin_native_roots(mirror_pin);
    let obj: ObjectRef = lookup_alloc?;
    ctx.set_field(obj, LK_LOOKUP_CLASS_REF, Value::Object(Some(mirror)));
    // Last: nothing allocates after this, and the returned Lookup holds the
    // mirror.
    if non_strong_hidden {
        register_non_strong_hidden_class(ctx, cid);
    }
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
) -> Result<ObjectRef, MethodCallFailed> {
    // gen r4w3/rooting: `try_alloc_concurrent_synthetic` (class init) and
    // `mirror_class_id` are GC points and the three caller-supplied refs are
    // stored after them; root them first (lowest pins) and re-read.
    let class_mirror_pin = class_mirror.map(|r| (ctx.pin_native_root(r), r));
    let name_pin = name.map(|r| (ctx.pin_native_root(r), r));
    let method_type_pin = method_type.map(|r| (ctx.pin_native_root(r), r));
    let inputs_base = [class_mirror_pin, name_pin, method_type_pin]
        .iter()
        .flatten()
        .map(|(p, _)| *p)
        .min();
    let mh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", MH_FIELD_COUNT)?;
    let class_mirror = class_mirror_pin.map(|(p, r)| ctx.read_native_pin(p, r));
    let name = name_pin.map(|(p, r)| ctx.read_native_pin(p, r));
    let method_type = method_type_pin.map(|(p, r)| ctx.read_native_pin(p, r));
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
    // gen r4w3/rooting: `mirror_class_id` above may have collected; the
    // caller-supplied MethodType is read back through its pin.
    let method_type = method_type_pin.map(|(p, r)| ctx.read_native_pin(p, r));
    let mt_to_store = match method_type {
        Some(mt) => Some(mt),
        None => crate::lang_invoke::build_method_type_from_descriptor(ctx, "()V")?,
    };
    let mh = ctx.read_native_pin(mh_pin, mh);
    ctx.unpin_native_roots(mh_pin);
    if let Some(base) = inputs_base {
        ctx.unpin_native_roots(base);
    }
    if let Some(mt) = mt_to_store {
        ctx.set_field_by_name(mh, "type", Value::Object(Some(mt)));
    }
    Ok(mh)
}

// -----------------------------------------------------------------------
// DELETED 2026-08-12: `lk_member_access_flags`, `enforce_lookup_access` and
// the eleven `lk_find_*` natives that called it.
//
// SUPERSEDED, not merely unused. Every one of those triples —
// `findVirtual`, `findStatic`, `findConstructor`, `findGetter`, `findSetter`,
// `findStaticGetter`, `findStaticSetter`, `findSpecial`, `findVarHandle`,
// `findStaticVarHandle` — is registered by
// `lang_invoke::register_p63_method_handles_lookup`, and the registration
// site a few hundred lines below says in its own comment that this module
// must NOT re-register them ("that would overwrite the real implementations
// with incompatible stubs"). So these bodies had no registration in any
// mode: not `--real-jdk`, not `--jdk-only`, not `synthetic-jdk`. Nothing
// dispatched into them and nothing ever had.
//
// `dead_code` is allowed crate-wide (`lib.rs`), so nothing warned. What kept
// them looking alive was five unit tests aimed straight at them, green on
// every run since W3-1 and guarding an access check the VM does not invoke —
// which is precisely the defect W4-1 was filed for: `publicLookup()` reached
// a private method while a fully-written check for that exact case sat here
// passing its own tests. Deleting the tests alone would have left the next
// reader concluding the check exists. Both went, together, and the five
// assertions were re-pointed at `lang_invoke::lk_enforce_find_access` — the
// gate that actually runs, called as the first statement of all ten
// `lookup_find_*`. See that module's `#[cfg(test)]` block.
//
// NOT deleted, against what W4-1's own patch block prescribes:
// `lk_public_lookup`. It IS registered, on `MethodHandles$Lookup.publicLookup`
// a few hundred lines below, and its doc comment states the JDK 25 mode word
// it answers. W4-1's deletion list is wrong on that one entry.
// W7-62-ratchets-and-dead-code.md · W4-1-publiclookup-allowedmodes-never-checked.md
// -----------------------------------------------------------------------

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
    Ok(Some(Value::Object(Some(mh?))))
}

fn lk_unreflect_special(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lk_unreflect(ctx, args)
}

/// Two class mirrors' relationship, for [`lk_in_modes`]: `(same_class,
/// same_package, same_nest)`.
///
/// `same_nest` is the JDK's `VerifyAccess.isSamePackageMember`: same package
/// AND same outermost enclosing class. The JDK walks `getEnclosingClass()`;
/// we take the internal name up to the first `$`, which agrees with it for
/// every nested/inner/anonymous form the compiler emits (`p/Outer$Inner`,
/// `p/Outer$1`) — the one shape it over-approximates is a top-level class
/// whose SOURCE name literally contains `$`, which is legal but not a name
/// anything in this codebase produces.
///
/// An unresolvable mirror answers `(false, true, true)` — "a different class,
/// but do not additionally strip package or private access" — matching
/// `lang_invoke::lk_same_package`'s permissive default. Guessing "different
/// package" for a mirror we simply could not name would silently demote a
/// legitimate lookup to PUBLIC and turn every subsequent non-public
/// `find*` into a spurious `IllegalAccessException`.
pub(crate) fn lk_class_relation(ctx: &dyn NativeContext, a: Value, b: Value) -> (bool, bool, bool) {
    let name_of = |v: Value| match v {
        Value::Object(Some(m)) => crate::lang_class::mirror_class_name(ctx, m),
        _ => None,
    };
    let (Some(an), Some(bn)) = (name_of(a), name_of(b)) else {
        return (false, true, true);
    };
    if an == bn {
        return (true, true, true);
    }
    let package_of = |n: &str| match n.rfind('/') {
        Some(i) => n[..i].to_string(),
        None => String::new(),
    };
    let outermost_of = |n: &str| match n.find('$') {
        Some(i) => n[..i].to_string(),
        None => n.to_string(),
    };
    let same_package = package_of(&an) == package_of(&bn);
    let same_nest = same_package && outermost_of(&an) == outermost_of(&bn);
    (false, same_package, same_nest)
}

/// `MethodHandles.Lookup.in`'s TARGET-CLASS validity test, shared by both
/// registrations of the method.
///
/// The JDK opens `in` with three rejections, before any mode arithmetic:
///
/// ```java
/// Objects.requireNonNull(requestedLookupClass);
/// if (requestedLookupClass.isPrimitive())
///     throw new IllegalArgumentException(requestedLookupClass + " is a primitive class");
/// if (requestedLookupClass.isArray())
///     throw new IllegalArgumentException(requestedLookupClass + " is an array class");
/// ```
///
/// A native that only computes modes drops all three, and the drop is silent:
/// `lookup().in(int.class)` returns a Lookup over a primitive instead of
/// raising, and every later `find*` on it fails with a message naming the wrong
/// thing. Both `in` registrations — this file's (synthetic-JDK mode) and
/// `lang_invoke.rs::register_p63_method_handles_lookup`'s (both real-JDK arms)
/// — call this, for the same reason they share [`lk_in_modes`]: two copies of a
/// rule drift, and the drift is only visible from outside the VM.
///
/// Measured on OpenJDK 25.0.3: `lookup().in(int.class)` and
/// `lookup().in(String[].class)` both raise `IllegalArgumentException`;
/// `lookup().in(null)` raises `NullPointerException`. `regression-suite/src/
/// RJdkLookupIn.java` is the vector.
pub(crate) fn lk_check_in_target(
    ctx: &dyn NativeContext,
    target: Value,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let mirror = match target {
        Value::Object(Some(m)) => m,
        // `in(null)` is an NPE in the JDK, not an IAE and not a silent
        // full-power Lookup over nothing. `Objects.requireNonNull`'s: no
        // message (measured, HotSpot 25: `getMessage()` is null; interpreter
        // round i1 wave 43, lane L4, `L4W43LookupTeleport` row `in-null`).
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    let describe = |kind: &str| {
        let name = crate::lang_class::mirror_class_name(ctx, mirror)
            .map(|n| n.replace('/', "."))
            .unwrap_or_else(|| "?".to_string());
        cratonvm_types::error::RuntimeError::IllegalArgumentException {
            message: format!("{name} is {kind}"),
        }
    };
    if crate::lang_class::mirror_is_primitive(ctx, mirror) {
        return Err(describe("a primitive class").into());
    }
    if crate::lang_class::mirror_is_array(ctx, mirror) {
        return Err(describe("an array class").into());
    }
    Ok(())
}

fn lk_in_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    lk_check_in_target(ctx, target)?;
    // Read via `lk_modes_of`, which is correct against BOTH layouts. The old
    // `get_field(this, LK_ALLOWED_MODES)` here was the synthetic slot only; on
    // a real Lookup slot 1 is `prevLookupClass`, a reference, so the `Int` arm
    // missed and the modes silently defaulted.
    let modes = lk_modes_of(ctx, this);
    let lookup_class = ctx.get_field(this, LK_LOOKUP_CLASS_REF);
    let (same_class, same_package, same_nest) = lk_class_relation(ctx, lookup_class, target);
    // A Lookup reporting 0 has no modes to narrow. Keep it at 0 rather than
    // inventing PUBLIC: `in()` never GRANTS access the receiver did not have.
    // Measured: `lookup().dropLookupMode(PUBLIC)` is 0, and `.in(String.class)`
    // / `.in(<package-mate>)` / `.in(<its own lookup class>)` are all 0.
    let target_is_public = match target {
        Value::Object(Some(m)) => crate::lang_class::mirror_is_public(ctx, m),
        _ => false,
    };
    let new_modes = if modes == 0 {
        0
    } else {
        lk_in_modes(modes, same_class, same_package, same_nest, target_is_public)
    };
    // gen r4w3/rooting: `alloc_lookup` can initialise the Lookup class (GC
    // point); the target mirror is stored after it, so root and re-read it.
    let target_pin = match target {
        Value::Object(Some(m)) => Some((ctx.pin_native_root(m), m)),
        _ => None,
    };
    let new_lk = alloc_lookup(ctx, new_modes)?;
    let target = match target_pin {
        Some((pin, m)) => {
            let m = ctx.read_native_pin(pin, m);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(m))
        }
        None => target,
    };
    ctx.set_field(new_lk, LK_LOOKUP_CLASS_REF, target);
    Ok(Some(Value::Object(Some(new_lk))))
}

fn lk_drop_lookup_mode(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let drop_mode = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    // See `lk_in_method` for why this must not read `LK_ALLOWED_MODES` raw.
    // A 0 stays 0: dropping a mode never GRANTS one, and the previous code
    // reached the same answer for a fresh synthetic Lookup (whose slot 1 reads
    // back `Int(0)`, so its `LK_FULL_POWER` default arm never fired either).
    let modes = lk_modes_of(ctx, this);
    let new_modes = match lk_drop_modes(modes, drop_mode) {
        Some(m) => m,
        // Measured on JDK 25: the message is exactly this.
        None => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: format!("{drop_mode} is not a valid mode to drop"),
                }
                .into(),
            );
        }
    };
    // gen r4w3/rooting: `alloc_lookup` can initialise the Lookup class (GC
    // point); `this` is read after it, so root and re-read it.
    let this_pin = ctx.pin_native_root(this);
    let new_lk = alloc_lookup(ctx, new_modes)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
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
    // BOTH of these lose their slot. `lookup_define::register_lookup_define_class`
    // re-registers the identical triples on the WP2.3-B implementations, and
    // runs after this function from both registrars (`lib.rs` and
    // `reflect_annotations.rs`), so `lk_define_class` / `lk_define_hidden_class`
    // below are never dispatched — `--dump-native-registry` reports
    // `owns_slot: true` on `lookup_define.rs:901` and `:909`.
    //
    // Kept, and kept in step with the winner, because that is cheaper than
    // re-deriving them if the ordering is ever reversed. Do NOT fix a measured
    // `Lookup.define*` defect here: a fix applied to this pair changes nothing
    // observable. (It cost a full 34-minute rebuild to learn that once.)
    r.register(lk, "defineClass", "([B)Ljava/lang/Class;", lk_define_class);
    r.register(lk, "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;", lk_define_hidden_class);
    // findVirtual/findStatic/findConstructor/findGetter/findSetter/findSpecial/
    // findVarHandle/findStaticVarHandle are all registered in
    // lang_invoke::register_p63_method_handles_lookup — do NOT re-register here
    // as that would overwrite the real implementations with incompatible stubs.
    //
    // `in` and `dropLookupMode` are the EXCEPTION this comment used to omit,
    // and the omission mattered: `lang_invoke::register_p63_method_handles_lookup`
    // registers `in` too (lang_invoke.rs, "Lookup.in(targetClass)"), so there are
    // two implementations and which one runs depends on the mode.
    //   * synthetic-JDK mode: `register_synthetic_overrides` calls
    //     `register_phase63_natives` (lib.rs) BEFORE
    //     `classloader::register_classloader_natives`, so THIS registration wins.
    //   * real-JDK mode: `register_synthetic_overrides` is a no-op and vm_init's
    //     real arm calls `register_p63_method_handles_lookup` directly, so
    //     lang_invoke's wins.
    // Keep this one: measured against OpenJDK 25.0.3 (see `lk_in_modes`),
    // lang_invoke's copy answers 31 for `in(<the lookup class itself>)` where
    // the JDK answers 95, and it treats a receiver reporting 0 modes as
    // FULL_POWER — i.e. `in()` GRANTS access. lang_invoke.rs is owned by another
    // lane; the two must be collapsed onto `lk_in_modes` there, not forked again
    // here.
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
    // DELETED wave 4 (2026-07-28): `ByteArrayInputStream.close()V` was
    // registered here as a no-op. It was a DEAD registration —
    // `native-io::register_io_natives` registers the same triple (bound to
    // `native_bais_close`, itself `Ok(None)`) and runs strictly after this
    // registrar in every VM init path (`vm/src/vm/vm_init.rs`). The surviving
    // native-io copy carries the justification: the real JDK body is empty and
    // the class documents that closing has no effect.
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
    // KEEP (real JDK body is `return true;`) — re-verified wave 4, 2026-07-28.
    // `ByteArrayInputStream` is backed by an in-memory byte[], so mark/reset
    // is always supported; the `mark`/`reset` natives registered just above
    // genuinely implement it against slot 2, and this triple has no rival
    // registration anywhere in the tree (native-io registers BAIS
    // read/available/skip/reset/close but NOT markSupported), so this really
    // is the answer callers get.
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
        let this_pin = ctx.pin_native_root(this);
        let rb = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
        // gc-common w20-f: the receiver is pinned across the buffer's
        // allocation (it was written through its pre-allocation address).
        let this: ObjectRef = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
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
        // `DataInputStream` inherits `FilterInputStream.close()`, which is a
        // bare `in.close()` under `throws IOException` with no `catch`, so the
        // delegated failure PROPAGATES. Dropping it hid exactly the class of
        // fault this registration was added to stop leaking.
        // W7-57-close-flush-swallow-sweep.md
        if let Some(u) = underlying {
            ctx.invoke_virtual(u, "close", "()V", &[])?;
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
        // gc-common w20-f: receiver pinned across the buffer's allocation.
        let this_pin = ctx.pin_native_root(this);
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8192);
        let this: ObjectRef = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
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
        // gc-common w20-f: receiver pinned across the buffer's allocation.
        let this_pin = ctx.pin_native_root(this);
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, size as usize);
        let this: ObjectRef = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
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
    // KEEP (real JDK body is `return true;`) — re-verified wave 4, 2026-07-28.
    // `BufferedInputStream` overrides `markSupported` with an unconditional
    // `true`, and the `mark`/`reset` natives registered just above genuinely
    // implement it against `markpos`/`marklimit`. Shadowing note: `lib.rs`
    // registers this same triple (also `1`) from the essential-natives path;
    // the two agree, so last-registration-wins is harmless — but change both
    // together if the answer ever stops being constant.
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
    //
    // STUB-REMOVAL (wave 3): the no-op body is still wrong wherever this DOES
    // win (synthetic-JDK mode, where there is no real BIS bytecode to yield
    // to): real `BufferedInputStream.close()` closes the wrapped stream, so a
    // no-op leaked the underlying stream/fd on every `try (var in = new
    // BufferedInputStream(...))`. Delegate to the wrapped stream by NAME (`in`,
    // inherited from `FilterInputStream`) so no field-index coupling is added,
    // per the note above. `buf` is deliberately left alone — nulling it is what
    // real JDK does, but the native-io H2 fix relies on the lazy `buf`
    // allocation and this native must not disturb it.
    r.register(bis, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(inner)) = ctx.get_field_by_name(this, "in") {
            ctx.invoke_virtual(inner, "close", "()V", &[])?;
        }
        Ok(None)
    });

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

    /// The two names in `register_classloader_define_class` are written out
    /// instead of looped over `JLA_CARRIER_CANDIDATES` so the drift scanner can
    /// read them (see the comment there). Nothing in the language ties the two
    /// lists together, so tie them here: a candidate added to the const and not
    /// to that call site is a carrier on which `defineClass` silently is not
    /// registered, which on the image where it IS the carrier is a
    /// `NoSuchMethodError` from `BootLoader` during boot.
    #[test]
    fn jla_define_class_is_registered_on_every_carrier_candidate() {
        const DESC: &str = "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BLjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;";
        let mut r = NativeMethodRegistry::new();
        register_classloader_define_class(&mut r);
        let rows = r.dump_registrations();
        for cand in crate::shared_secrets_bridge::JLA_CARRIER_CANDIDATES {
            assert!(
                rows.iter().any(|(cls, name, desc, _)| *cls == cand
                    && *name == "defineClass"
                    && *desc == DESC),
                "no defineClass registration for carrier candidate {cand}; \
                 the inline list in register_classloader_define_class has \
                 drifted from JLA_CARRIER_CANDIDATES"
            );
        }
    }
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
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

        let hits = loader_local_resource_urls(&mut ctx, loader, "virtual/tomcat0807_webapp.txt");
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

    /// A loader whose OWN URL list is knowable answers `findResources` out of
    /// that list alone. An empty answer is the answer — widening it with the
    /// process-wide scan is what handed a `@ClassPathExclusions` test back the
    /// `META-INF/services` entry of the very jar it excluded.
    #[test]
    fn test_urlclassloader_find_resources_does_not_fall_back_to_flat_scan() {
        const SPI: &str = "META-INF/services/org.slf4j.spi.SLF4JServiceProvider";

        // The loader's own URL: a directory that does NOT hold the descriptor,
        // standing in for a classpath the excluded jar was filtered out of.
        let dir = tempfile::tempdir().expect("tempdir");

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

        // ... while the PROCESS-WIDE classpath does hold it. This is the leak
        // source: without it the assertion below would pass vacuously.
        ctx.set_resource(
            SPI,
            b"ch.qos.logback.classic.spi.LogbackServiceProvider\n".to_vec(),
        );
        let flat_name = ctx.create_string(SPI);
        let flat = cl_get_resources_impl(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(flat_name))],
            false,
        )
        .expect("flat scan")
        .expect("flat scan return value");
        assert_eq!(
            enumeration_len(&mut ctx, flat),
            1,
            "the process-wide scan must see this descriptor, else the assertion \
             below would pass without the leak ever being possible"
        );

        assert!(
            loader_local_resource_urls(&mut ctx, loader, SPI).is_empty(),
            "receiver-local scan must not find the excluded descriptor"
        );

        let name = ctx.create_string(SPI);
        let found = ucl_find_resources(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .expect("findResources native")
        .expect("return value");
        assert_eq!(
            enumeration_len(&mut ctx, found),
            0,
            "an empty receiver-local result must be returned as-is, not replaced \
             by the process-wide classpath scan"
        );
    }

    /// A loader CratonVM has NO URL view of keeps the historical flat-scan
    /// fallback — "the local scan found nothing" and "the local scan could not
    /// run" are different answers and only the first one is authoritative.
    #[test]
    fn test_urlclassloader_find_resources_keeps_flat_scan_without_recorded_urls() {
        const SPI: &str = "META-INF/services/com.acme.Service";

        let mut ctx = MockNativeContext::new();
        let loader = new_object_ref(&mut ctx, "java/net/URLClassLoader");
        ctx.set_resource(SPI, b"com.acme.Provider\n".to_vec());

        assert!(
            loader_constructor_url_paths(&ctx, loader).is_empty(),
            "fixture must leave this loader's URL list unknowable"
        );

        let name = ctx.create_string(SPI);
        let found = ucl_find_resources(
            &mut ctx,
            &[Value::Object(Some(loader)), Value::Object(Some(name))],
        )
        .expect("findResources native")
        .expect("return value");
        assert_eq!(
            enumeration_len(&mut ctx, found),
            1,
            "with no recorded URLs the process-wide scan is all there is"
        );
    }

    /// The platform loader owns the JDK module surface and nothing else. Serving
    /// it the flat application classpath makes every child parented to it — the
    /// shape `ModifiedClassPathClassLoader` is built on — see the very jars its
    /// exclusions removed, through parent-first delegation.
    #[test]
    fn test_platform_loader_get_resources_excludes_application_classpath() {
        const SPI: &str = "META-INF/services/org.slf4j.spi.SLF4JServiceProvider";

        let mut ctx = MockNativeContext::new();
        ctx.set_resource(
            SPI,
            b"ch.qos.logback.classic.spi.LogbackServiceProvider\n".to_vec(),
        );

        // Control: an ordinary receiver still gets the application classpath, so
        // an empty answer below is the platform rule and not an empty fixture.
        let app = new_object_ref(&mut ctx, "jdk/internal/loader/ClassLoaders$AppClassLoader");
        let app_name = ctx.create_string(SPI);
        let app_enum = cl_get_resources_impl(
            &mut ctx,
            &[Value::Object(Some(app)), Value::Object(Some(app_name))],
            true,
        )
        .expect("app getResources")
        .expect("app return value");
        assert_eq!(
            enumeration_len(&mut ctx, app_enum),
            1,
            "the application loader must still see the process classpath"
        );

        let platform = new_object_ref(
            &mut ctx,
            "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
        );
        let name = ctx.create_string(SPI);
        let found = cl_get_resources_impl(
            &mut ctx,
            &[Value::Object(Some(platform)), Value::Object(Some(name))],
            true,
        )
        .expect("platform getResources")
        .expect("return value");
        assert_eq!(
            enumeration_len(&mut ctx, found),
            0,
            "the platform loader must not enumerate application-classpath resources"
        );
    }

    /// Length of a snapshot `Enumeration$Impl` (field 0 is its backing array).
    fn enumeration_len(ctx: &mut MockNativeContext, value: Value) -> usize {
        let enm = match value {
            Value::Object(Some(o)) => o,
            other => panic!("expected an Enumeration object, got {other:?}"),
        };
        match ctx.get_field(enm, 0) {
            Value::Object(Some(arr)) => ctx.array_length(arr),
            _ => 0,
        }
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
                signature: None,
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
                signature: None,
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
                signature: None,
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

    /// gc-common w9-a: a VM's real-JDK loader namespace ids go with the VM —
    /// at ITS teardown (`forget_vm_loader_singletons`), not at the creation of
    /// some other VM (`reset_loader_singletons`, which used to wipe the whole
    /// table and so every concurrently live VM's rows with it).
    #[test]
    fn test_vm_teardown_forgets_real_jdk_loader_namespace_ids() {
        const VM: usize = 0x9A_0001;
        const NS: u32 = 90_901;
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let class_loader = ctx
            .ensure_class_initialized("java/lang/ClassLoader")
            .expect("ClassLoader class");
        let isolated_loader = ctx
            .ensure_class_initialized("example/IsolatedLoader")
            .expect("isolated loader class");
        ctx.set_superclass(isolated_loader, class_loader);
        let loader = new_object_ref(&mut ctx, "example/IsolatedLoader");
        loader_namespace_id_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((VM, loader, NS));

        assert_eq!(peek_loader_namespace_id(&mut ctx, loader), Some(NS));

        reset_loader_singletons();
        assert_eq!(
            peek_loader_namespace_id(&mut ctx, loader),
            Some(NS),
            "creating another VM must not wipe a live VM's namespace ids"
        );

        forget_vm_loader_singletons(VM);
        assert!(
            peek_loader_namespace_id(&mut ctx, loader).is_none(),
            "VM teardown must not leave stale real-JDK loader namespace ids"
        );
    }

    /// SHIM-AUDIT handover item 2 — pins the GC contract documented on
    /// [`loader_namespace_id_store`].
    ///
    /// This is a PIN, not a fail-before-the-fix regression test: both halves
    /// (prune the dead, remap the moved) are already wired, and were verified
    /// by reading `gc_reconcile_defining_loaders`. Until now the only thing
    /// holding the invariant was a prose comment, and the comment had already
    /// drifted (it still described the table's old identity-hash-keyed form and
    /// claimed it held no `ObjectRef`s at all). Deleting either half of the
    /// `retain_mut` now fails here instead of silently reintroducing the
    /// original defect: a dead loader's entry outliving it, its address being
    /// reused, and the new loader at that address inheriting the dead one's
    /// namespace id *and* — via `register_user_loader_parent` — its parent link.
    #[test]
    fn loader_namespace_store_is_pruned_and_remapped_by_gc_reconcile() {
        let mut ctx = MockNativeContext::new();
        let class_loader = ctx
            .ensure_class_initialized("java/lang/ClassLoader")
            .expect("ClassLoader class");
        let isolated = ctx
            .ensure_class_initialized("example/GcReconcileLoader")
            .expect("isolated loader class");
        ctx.set_superclass(isolated, class_loader);

        let dead = new_object_ref(&mut ctx, "example/GcReconcileLoader");
        let moved_from = new_object_ref(&mut ctx, "example/GcReconcileLoader");
        let moved_to = new_object_ref(&mut ctx, "example/GcReconcileLoader");

        // Ids well above `ClassLoaderId::NATIVE_FIRST_USER_DEFINED` and far from
        // anything another test in this process allocates.
        const DEAD_NS: u32 = 90_001;
        const MOVED_NS: u32 = 90_002;

        {
            let mut store = loader_namespace_id_store()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let vm = ctx.vm_identity();
            store.push((vm, dead, DEAD_NS));
            store.push((vm, moved_from, MOVED_NS));
        }
        assert_eq!(
            peek_loader_namespace_id(&mut ctx, dead),
            Some(DEAD_NS),
            "precondition: the dead loader's entry is in the store"
        );

        let dead_addr = dead.as_ptr() as usize;
        let from_addr = moved_from.as_ptr() as usize;
        let to_addr = moved_to.as_ptr() as usize;

        // These side-tables are process-global and the test binary is
        // multi-threaded, so this predicate reports every address it does not
        // own as ALIVE. Only `dead` is collected; nothing another test put in
        // either store can be pruned by this call.
        //
        // `moved_from` is reported alive on purpose: a relocated object IS a
        // survivor. That mirrors the real predicate
        // (`pointer_map.contains_key(addr) || heap.is_addr_live(addr)`) named in
        // `gc_reconcile_defining_loaders`' doc comment.
        let is_marked = move |addr: usize| addr != dead_addr;
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(from_addr, to_addr);

        gc_reconcile_defining_loaders(ctx.vm_identity(), &is_marked, &pointer_map, None);

        // Half 1 — PRUNE. The collected loader's entry is gone, so its address
        // can be recycled without the next loader inheriting its namespace.
        assert!(
            peek_loader_namespace_id(&mut ctx, dead).is_none(),
            "a loader collected this cycle must not keep its namespace-id entry"
        );
        assert!(
            loader_object_for_namespace_id(DEAD_NS).is_none(),
            "the dead namespace id must no longer resolve to any loader object"
        );

        // Half 2 — REMAP. The survivor kept its id and now answers at its NEW
        // address; the old address answers for nobody.
        assert_eq!(
            peek_loader_namespace_id(&mut ctx, moved_to),
            Some(MOVED_NS),
            "a relocated loader must keep its namespace id at its new address"
        );
        assert_eq!(
            loader_object_for_namespace_id(MOVED_NS).map(|o| o.as_ptr()),
            Some(moved_to.as_ptr()),
            "the reverse lookup must hand back the post-collection address"
        );
        assert!(
            peek_loader_namespace_id(&mut ctx, moved_from).is_none(),
            "the pre-collection address must no longer resolve"
        );

        // Leave the process-global store as we found it.
        loader_namespace_id_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, _, id)| *id != DEAD_NS && *id != MOVED_NS);
    }

    /// gc-common w9-a: the loader-namespace and loader-meta rows belong to the
    /// VM that recorded them.
    ///
    /// * VM A's reconcile, with a predicate that calls VM B's loader DEAD
    ///   (the production predicate answers "dead" for every address outside
    ///   A's heap), keeps B's namespace and meta rows, and does not remap
    ///   them through A's pointer map either.
    /// * A lookup from VM A does not find VM B's row for the same address.
    /// * `reset_loader_singletons` (another VM's creation) leaves both rows.
    /// * `forget_vm_loader_singletons(B)` drops exactly B's rows.
    #[test]
    fn w9a_loader_namespace_and_meta_rows_belong_to_their_vm() {
        const VM_A: usize = 0x9A_0A00;
        const VM_B: usize = 0x9A_0B00;
        const NS_A: u32 = 90_911;
        const NS_B: u32 = 90_912;
        let mut ctx_a = MockNativeContext::new();
        ctx_a.set_vm_identity(VM_A);
        let mut ctx_b = MockNativeContext::new();
        ctx_b.set_vm_identity(VM_B);
        for ctx in [&mut ctx_a, &mut ctx_b] {
            let class_loader = ctx
                .ensure_class_initialized("java/lang/ClassLoader")
                .expect("ClassLoader class");
            let isolated = ctx
                .ensure_class_initialized("example/W9aPerVmLoader")
                .expect("isolated loader class");
            ctx.set_superclass(isolated, class_loader);
        }
        let loader_a = new_object_ref(&mut ctx_a, "example/W9aPerVmLoader");
        let loader_b = new_object_ref(&mut ctx_b, "example/W9aPerVmLoader");
        let a_addr = loader_a.as_ptr() as usize;
        let b_addr = loader_b.as_ptr() as usize;

        {
            let mut store = loader_namespace_id_store()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            store.push((VM_A, loader_a, NS_A));
            store.push((VM_B, loader_b, NS_B));
        }
        loader_meta_put(
            VM_B,
            loader_b,
            LoaderMeta {
                loader_type: Some(LOADER_CUSTOM),
                parallel_capable: Some(false),
                ..LoaderMeta::default()
            },
        );

        // The rows answer only for their own VM, even at the same address.
        assert_eq!(peek_loader_namespace_id(&mut ctx_b, loader_b), Some(NS_B));
        assert!(loader_meta_get(VM_A, loader_b).is_none());
        assert_eq!(
            loader_object_for_namespace_id_in(VM_B, NS_B).map(|o| o.as_ptr() as usize),
            Some(b_addr)
        );
        assert!(
            loader_object_for_namespace_id_in(VM_A, NS_B).is_none(),
            "a namespace id minted in VM B must not resolve in VM A"
        );

        // VM A collects. Its heap calls B's loader (and A's own) dead, and its
        // pointer map would relocate B's address if B's rows were A's to remap.
        let is_marked = move |addr: usize| addr != b_addr && addr != a_addr;
        let mut pointer_map = cratonvm_types::PointerMap::default();
        let bogus_to = new_object_ref(&mut ctx_a, "example/W9aPerVmLoader");
        pointer_map.insert(b_addr, bogus_to.as_ptr() as usize);
        gc_reconcile_defining_loaders(VM_A, &is_marked, &pointer_map, None);

        assert!(
            loader_object_for_namespace_id_in(VM_A, NS_A).is_none(),
            "A's own dead loader is pruned"
        );
        assert_eq!(
            peek_loader_namespace_id(&mut ctx_b, loader_b),
            Some(NS_B),
            "VM A's collection must neither prune nor remap VM B's namespace row"
        );
        assert_eq!(
            loader_meta_get(VM_B, loader_b).and_then(|m| m.loader_type),
            Some(LOADER_CUSTOM),
            "VM A's collection must neither prune nor remap VM B's meta row"
        );
        assert_eq!(loader_parallel_capable_of(&ctx_b, loader_b), Some(0));

        // Another VM being created does not touch B's rows.
        reset_loader_singletons();
        assert_eq!(peek_loader_namespace_id(&mut ctx_b, loader_b), Some(NS_B));
        assert!(loader_meta_get(VM_B, loader_b).is_some());

        // B's teardown drops them.
        forget_vm_loader_singletons(VM_B);
        assert!(peek_loader_namespace_id(&mut ctx_b, loader_b).is_none());
        assert!(loader_meta_get(VM_B, loader_b).is_none());
        assert!(loader_object_for_namespace_id(NS_B).is_none());
        forget_vm_loader_singletons(VM_A);
    }

    /// gc-common w9-a: `URLClassLoader.close()` state is per VM. Identity
    /// hashes collide across VMs by construction, so VM A closing its loader
    /// must not close VM B's loader that has the same hash, and another VM's
    /// creation must not reopen it.
    #[test]
    fn w9a_closed_url_loader_state_is_per_vm() {
        const VM_A: usize = 0x9A_1A00;
        const VM_B: usize = 0x9A_1B00;
        let mut ctx_a = MockNativeContext::new();
        ctx_a.set_vm_identity(VM_A);
        let mut ctx_b = MockNativeContext::new();
        ctx_b.set_vm_identity(VM_B);
        let loader_a = new_object_ref(&mut ctx_a, "java/lang/Object");
        let loader_b = new_object_ref(&mut ctx_b, "java/lang/Object");
        // Two fresh mocks number their objects the same way, so the two
        // loaders normally share an identity hash here — the collision every
        // pair of real heaps produces. The assertions hold either way.
        ucl_mark_closed(&ctx_a, loader_a);
        assert!(ucl_is_closed(&ctx_a, loader_a));
        assert!(
            !ucl_is_closed(&ctx_b, loader_b),
            "A's close must not close B's loader, even on a shared identity hash"
        );

        ucl_mark_closed(&ctx_b, loader_b);
        assert!(ucl_is_closed(&ctx_b, loader_b));
        reset_loader_singletons();
        assert!(
            ucl_is_closed(&ctx_b, loader_b),
            "another VM's creation must not reopen a live VM's closed loader"
        );
        assert!(ucl_is_closed(&ctx_a, loader_a));

        forget_vm_loader_singletons(VM_A);
        assert!(!ucl_is_closed(&ctx_a, loader_a));
        assert!(
            ucl_is_closed(&ctx_b, loader_b),
            "A's teardown keeps B's state"
        );
        forget_vm_loader_singletons(VM_B);
        assert!(!ucl_is_closed(&ctx_b, loader_b));
    }

    /// gc-common w11-e: a closed `URLClassLoader`'s row goes when the loader
    /// dies (it used to stay until VM teardown), follows the loader when it
    /// moves, and another VM's row is not judged.
    #[test]
    fn w11e_closed_url_loader_rows_follow_their_loader_through_the_reconcile() {
        const VM: usize = 0x11E_1A00;
        const OTHER: usize = 0x11E_1B00;
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let mut other = MockNativeContext::new();
        other.set_vm_identity(OTHER);
        let dead = new_object_ref(&mut ctx, "java/lang/Object");
        let moved = new_object_ref(&mut ctx, "java/lang/Object");
        let foreign = new_object_ref(&mut other, "java/lang/Object");
        ucl_mark_closed(&ctx, dead);
        ucl_mark_closed(&ctx, moved);
        ucl_mark_closed(&other, foreign);
        let dead_addr = dead.as_ptr() as usize;
        let from_addr = moved.as_ptr() as usize;
        // gc-common w29-e: 4 GiB up keeps the mock identity hash (the address
        // truncated to i32), as a real move keeps the object's hash; lookups
        // compare the address, so the moved loader is asked for at `to_addr`.
        let to_addr: usize = from_addr + 0x1_0000_0000;
        // SAFETY: never dereferenced; the lookup hashes the address.
        let moved_now = unsafe { ObjectRef::from_raw(to_addr as *mut u8) };
        // The foreign loader is "dead" as VM's heap sees it, like every
        // address outside that heap. It must not be judged.
        let foreign_addr = foreign.as_ptr() as usize;
        let is_marked = move |addr: usize| addr != dead_addr && addr != foreign_addr;
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(from_addr, to_addr);

        gc_reconcile_defining_loaders(VM, &is_marked, &pointer_map, None);

        assert!(!ucl_is_closed(&ctx, dead), "a dead loader's row goes");
        assert!(ucl_is_closed(&ctx, moved_now), "a moved loader stays closed");
        assert!(
            !ucl_is_closed(&ctx, moved),
            "and its old address is no longer a closed loader"
        );
        let ours: Vec<usize> = closed_url_classloader_ids()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|((vm, _), _)| *vm == VM)
            .flat_map(|(_, rows)| rows.iter().copied())
            .collect();
        assert_eq!(
            ours,
            vec![to_addr],
            "one row left for this VM, at the loader's new address"
        );
        assert!(
            ucl_is_closed(&other, foreign),
            "another VM's row is not judged by this VM's predicate"
        );

        forget_vm_loader_singletons(VM);
        forget_vm_loader_singletons(OTHER);
        assert!(!ucl_is_closed(&other, foreign));
    }

    /// gc-common w9-a: `url_classloader_define_locks` keeps an entry only
    /// while some thread uses it. The first of two users leaving keeps it,
    /// the last one removes it, and an entry left behind with no user at all
    /// is swept by the next release.
    #[test]
    fn w9a_define_lock_entry_is_released_by_its_last_user() {
        type Entry = std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>;
        let key = (0x9A_3000u32, "w9a/ReleasedByLastUser".to_string());
        let other = (0x9A_3001u32, "w9a/ReleasedByLastUserOther".to_string());
        let take = |k: &(u32, String)| -> Entry {
            url_classloader_define_locks()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(k.clone())
                .or_insert_with(|| {
                    std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()))
                })
                .clone()
        };
        let present = |k: &(u32, String)| {
            url_classloader_define_locks()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(k)
        };
        let leave = |k: &(u32, String), mine: Entry| {
            drop(DefineLockEntryRelease {
                key: k.clone(),
                lock_addr: std::sync::Arc::as_ptr(&mine) as usize,
            });
            drop(mine);
        };

        let first = take(&key);
        let second = take(&key);
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        leave(&key, first);
        assert!(present(&key), "a waiter still holds the entry");
        leave(&key, second);
        assert!(!present(&key), "the last user removes the entry");

        // Held by the map alone: abandoned. The next release sweeps it. (No
        // "it is present" check in between: a release by a concurrently
        // running test may legitimately sweep it first.)
        drop(take(&key));
        let mine = take(&other);
        leave(&other, mine);
        assert!(!present(&other));
        assert!(!present(&key), "an abandoned entry is swept");
    }

    /// gc-common w9-a: the URL-classpath cache is bounded, and a hit returns
    /// the cached `ClassPath` rather than building another one. The paths
    /// name nothing on disk, so `ClassPath::new` builds an empty path.
    #[test]
    fn w9a_url_class_path_cache_is_bounded() {
        let key = |i: usize| vec![format!("/w9a-nonexistent/cache-bound-{i}.jar")];
        let first = cached_class_path_for_paths(&key(0));
        let again = cached_class_path_for_paths(&key(0));
        assert!(
            Arc::ptr_eq(&first, &again),
            "a hit must reuse the cached ClassPath"
        );
        for i in 1..=(LOCAL_URL_CLASS_PATH_CACHE_LIMIT + 8) {
            let _ = cached_class_path_for_paths(&key(i));
        }
        assert!(
            url_class_path_cache().lock().0.len() <= LOCAL_URL_CLASS_PATH_CACHE_LIMIT,
            "the cache must never exceed its bound"
        );
    }

    /// gc-common w9-a: a VM's teardown clears the defining-loader key-set bit
    /// of an id no other VM still holds, and keeps the bit of an id another
    /// live VM holds.
    #[test]
    fn w9a_vm_teardown_clears_only_unshared_defining_loader_bits() {
        const VM_A: usize = 0x9A_2A00;
        const VM_B: usize = 0x9A_2B00;
        const SHARED: u32 = 900_921;
        const A_ONLY: u32 = 900_922;
        let fake = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
        register_defining_loader(VM_A, SHARED, fake(0x9A_1000));
        register_defining_loader(VM_B, SHARED, fake(0x9A_2000));
        register_defining_loader(VM_A, A_ONLY, fake(0x9A_3000));
        assert!(class_may_have_defining_loader(A_ONLY));

        forget_vm_loader_singletons(VM_A);
        assert!(
            !class_may_have_defining_loader(A_ONLY),
            "nobody holds the id any more"
        );
        assert!(
            class_may_have_defining_loader(SHARED),
            "VM B still holds the id"
        );
        assert_eq!(
            defining_loader_for(VM_B, SHARED).map(|l| l.as_ptr() as usize),
            Some(0x9A_2000)
        );

        forget_vm_loader_singletons(VM_B);
        assert!(!class_may_have_defining_loader(SHARED));
    }

    /// gc-common w8-e: the defining-loader phase of the reconcile, now judged
    /// outside the store's mutex.
    ///
    /// * VM A's dead rows go, with an orphan marker and their loader pin.
    /// * A's moved row follows the pointer map, and so does its pin.
    /// * VM B's row on the SAME class id is not judged by A's predicate, even
    ///   though A's predicate calls B's loader dead (the production predicate
    ///   answers "dead" for every address outside A's heap).
    /// * The shared id keeps its key-set bit, because B still holds it. The id
    ///   only A held loses its bit.
    #[test]
    fn w8e_reconcile_judges_only_its_own_vms_rows_and_keeps_a_shared_ids_bit() {
        const VM_A: usize = 0xE8_0A00;
        const VM_B: usize = 0xE8_0B00;
        const SHARED: u32 = 900_801;
        const MOVED: u32 = 900_802;
        const A_ONLY: u32 = 900_803;
        let fake = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
        let a_dead: usize = 0xE8_1000;
        let a_from: usize = 0xE8_2000;
        let a_to: usize = 0xE8_3000;
        let a_only_dead: usize = 0xE8_4000;
        let b_loader: usize = 0xE8_5000;
        register_defining_loader(VM_A, SHARED, fake(a_dead));
        register_defining_loader(VM_B, SHARED, fake(b_loader));
        register_defining_loader(VM_A, MOVED, fake(a_from));
        register_defining_loader(VM_A, A_ONLY, fake(a_only_dead));

        // Dead: A's two loaders, and B's loader as A's heap sees it. Every
        // other address is reported alive, so the process-global namespace
        // and meta tables (which other tests use concurrently) lose nothing.
        let is_marked =
            move |addr: usize| addr != a_dead && addr != a_only_dead && addr != b_loader;
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(a_from, a_to);

        let dead = gc_reconcile_defining_loaders(VM_A, &is_marked, &pointer_map, None);
        assert_eq!(dead, vec![SHARED, A_ONLY]);

        assert!(defining_loader_for(VM_A, SHARED).is_none());
        assert!(is_defining_loader_orphaned(VM_A, SHARED));
        assert_eq!(
            defining_loader_for(VM_B, SHARED).map(|l| l.as_ptr() as usize),
            Some(b_loader),
            "another VM's row is not judged by this VM's predicate"
        );
        assert!(!is_defining_loader_orphaned(VM_B, SHARED));
        assert_eq!(
            defining_loader_for(VM_A, MOVED).map(|l| l.as_ptr() as usize),
            Some(a_to),
            "a survivor follows the pointer map"
        );

        assert!(class_may_have_defining_loader(SHARED), "VM B still holds the id");
        assert!(!class_may_have_defining_loader(A_ONLY), "nobody holds the id any more");

        use cratonvm_types::loader_pin::loader_pin_addr_for_vm;
        assert_eq!(loader_pin_addr_for_vm(VM_A, SHARED), None);
        assert_eq!(loader_pin_addr_for_vm(VM_A, A_ONLY), None);
        assert_eq!(loader_pin_addr_for_vm(VM_A, MOVED), Some(a_to));
        assert_eq!(loader_pin_addr_for_vm(VM_B, SHARED), Some(b_loader));

        forget_vm_loader_singletons(VM_A);
        forget_vm_loader_singletons(VM_B);
        clear_defining_loader_bit(SHARED);
        clear_defining_loader_bit(MOVED);
    }

    /// gc-common w11-e: the orphan marker of a class that stays loaded
    /// survives until a live loader adopts the class with a new
    /// defining-loader row, and then goes for that VM only. Selective
    /// forgetting (`forget_unloaded_classes`) drops only the ids it names.
    #[test]
    fn w11e_orphan_markers_go_only_with_their_class_or_a_new_defining_loader() {
        const VM: usize = 0x11E_0A00;
        const OTHER: usize = 0x11E_0B00;
        // Above `DEFINING_LOADER_BITS_CAP`, so no key-set bit is touched.
        const ADOPTED: u32 = 2_011_001;
        const STAYS: u32 = 2_011_002;
        const UNLOADED: u32 = 2_011_003;
        let fake = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
        let dead: usize = 0x11E_1000;
        let adopter: usize = 0x11E_2000;
        for id in [ADOPTED, STAYS, UNLOADED] {
            register_defining_loader(VM, id, fake(dead));
        }
        register_defining_loader(OTHER, ADOPTED, fake(dead));
        let no_moves = cratonvm_types::PointerMap::default();
        let is_marked = move |addr: usize| addr != dead;
        assert_eq!(
            gc_reconcile_defining_loaders(VM, &is_marked, &no_moves, None),
            vec![ADOPTED, STAYS, UNLOADED]
        );
        assert_eq!(
            gc_reconcile_defining_loaders(OTHER, &is_marked, &no_moves, None),
            vec![ADOPTED]
        );
        for id in [ADOPTED, STAYS, UNLOADED] {
            assert!(is_defining_loader_orphaned(VM, id));
        }
        assert!(is_defining_loader_orphaned(OTHER, ADOPTED));

        // The unload transaction names only the class it removed.
        forget_unloaded_classes(VM, &[UNLOADED]);
        assert!(!is_defining_loader_orphaned(VM, UNLOADED));
        assert!(
            is_defining_loader_orphaned(VM, STAYS),
            "a class still loaded keeps its marker"
        );

        // A live loader adopts the class: its row carries the restriction now.
        register_defining_loader(VM, ADOPTED, fake(adopter));
        assert!(!is_defining_loader_orphaned(VM, ADOPTED));
        assert_eq!(
            defining_loader_for(VM, ADOPTED).map(|l| l.as_ptr() as usize),
            Some(adopter)
        );
        assert!(
            is_defining_loader_orphaned(OTHER, ADOPTED),
            "another VM's marker on the same id is not this VM's to clear"
        );
        assert!(is_defining_loader_orphaned(VM, STAYS));

        forget_vm_loader_singletons(VM);
        forget_vm_loader_singletons(OTHER);
        assert!(!is_defining_loader_orphaned(VM, STAYS));
        assert!(!is_defining_loader_orphaned(OTHER, ADOPTED));
    }

    /// gc-common w10-f: `class_defined_by_this_loader_object` answers from the
    /// loader -> classes index, and gives the same answer as a full walk of
    /// the rows (the probe it replaced) before and after the reconcile prunes
    /// a dead loader and remaps a moved one, across re-registration to another
    /// loader, and with another VM holding rows on the same ids and the same
    /// loader address.
    #[test]
    fn w10f_this_loader_probe_uses_the_index_and_follows_the_reconcile() {
        use cratonvm_types::ClassId;
        const VM: usize = 0x10F_0A00;
        const OTHER: usize = 0x10F_0B00;
        let l1_from: usize = 0x10F_1000;
        let l1_to: usize = 0x10F_1800;
        let l2: usize = 0x10F_2000;
        fn fake(addr: usize) -> ObjectRef {
            unsafe { ObjectRef::from_raw(addr as *mut u8) }
        }
        fn probe(ctx: &mut MockNativeContext, this: usize, name: &str) -> Option<ClassId> {
            class_defined_by_this_loader_object(ctx, fake(this), name)
        }
        // The walk the index replaced, as the oracle.
        fn walk(ctx: &mut MockNativeContext, this: usize, name: &str) -> Option<ClassId> {
            let vm = ctx.vm_identity();
            let ids: Vec<u32> = defining_loader_store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rows
                .iter()
                .filter_map(|(&(row_vm, cid), loader)| {
                    (row_vm == vm && loader.as_ptr() as usize == this).then_some(cid)
                })
                .collect();
            ids.into_iter()
                .map(ClassId::new)
                .find(|&cid| ctx.class_name_arc_of_id(cid).as_deref() == Some(name))
        }
        let names = ["w10f/A", "w10f/B", "w10f/C", "w10f/D", "w10f/Missing"];
        let agree = |ctx: &mut MockNativeContext, loaders: &[usize], when: &str| {
            for &loader in loaders {
                for name in names {
                    assert_eq!(
                        probe(ctx, loader, name),
                        walk(ctx, loader, name),
                        "{when}: the index and the walk disagree on ({loader:#x}, {name})"
                    );
                }
            }
        };

        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let a = ctx.ensure_class_initialized("w10f/A").unwrap();
        let b = ctx.ensure_class_initialized("w10f/B").unwrap();
        let c = ctx.ensure_class_initialized("w10f/C").unwrap();
        let d = ctx.ensure_class_initialized("w10f/D").unwrap();
        register_defining_loader(VM, a.as_u32(), fake(l1_from));
        register_defining_loader(VM, b.as_u32(), fake(l1_from));
        register_defining_loader(VM, c.as_u32(), fake(l2));
        // Another VM, same id, same loader address: never an answer for VM.
        register_defining_loader(OTHER, a.as_u32(), fake(l2));

        assert_eq!(probe(&mut ctx, l1_from, "w10f/A"), Some(a));
        assert_eq!(probe(&mut ctx, l1_from, "w10f/A"), Some(a), "memoized hit");
        assert_eq!(probe(&mut ctx, l1_from, "w10f/C"), None);
        assert_eq!(probe(&mut ctx, l2, "w10f/A"), None, "another VM's row");
        agree(&mut ctx, &[l1_from, l2], "after registration");

        // A class registered after the names were read is still found.
        register_defining_loader(VM, d.as_u32(), fake(l1_from));
        assert_eq!(probe(&mut ctx, l1_from, "w10f/D"), Some(d));
        // Re-registration moves the id between the loaders' entries, and the
        // memoized name goes with it.
        register_defining_loader(VM, b.as_u32(), fake(l2));
        assert_eq!(probe(&mut ctx, l1_from, "w10f/B"), None);
        assert_eq!(probe(&mut ctx, l2, "w10f/B"), Some(b));
        assert_eq!(probe(&mut ctx, l1_from, "w10f/A"), Some(a));
        agree(&mut ctx, &[l1_from, l2], "after re-registration");

        // l2 dies, l1 moves. OTHER's row names l2 too and is not judged.
        let is_marked = move |addr: usize| addr != l2;
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(l1_from, l1_to);
        let dead = gc_reconcile_defining_loaders(VM, &is_marked, &pointer_map, None);
        let mut expected_dead = vec![b.as_u32(), c.as_u32()];
        expected_dead.sort_unstable();
        assert_eq!(dead, expected_dead);

        assert_eq!(probe(&mut ctx, l1_to, "w10f/A"), Some(a), "the index follows the move");
        assert_eq!(probe(&mut ctx, l1_to, "w10f/D"), Some(d));
        assert_eq!(probe(&mut ctx, l1_from, "w10f/A"), None, "the old address answers nothing");
        assert_eq!(probe(&mut ctx, l2, "w10f/C"), None, "a dead loader's entry is gone");
        agree(&mut ctx, &[l1_from, l1_to, l2], "after the reconcile");
        {
            let table = defining_loader_store()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert!(!table.by_loader.contains_key(&(VM, l2)));
            assert!(!table.by_loader.contains_key(&(VM, l1_from)));
            assert!(table.by_loader.contains_key(&(VM, l1_to)));
            assert!(
                table.by_loader.contains_key(&(OTHER, l2)),
                "another VM's entry is not judged by this VM's reconcile"
            );
        }
        assert_eq!(
            defining_loader_for(OTHER, a.as_u32()).map(|l| l.as_ptr() as usize),
            Some(l2)
        );

        forget_vm_loader_singletons(VM);
        forget_vm_loader_singletons(OTHER);
        let table = defining_loader_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            !table
                .by_loader
                .keys()
                .any(|&(vm, _)| vm == VM || vm == OTHER),
            "teardown drops the VM's index entries"
        );
    }

    /// gc-common w10-f: the JDK module tables and the platform-package memo
    /// are per VM. A VM that has cached them does not answer for another VM,
    /// and its teardown drops its rows. The mock has no `ModuleLoaderMap`
    /// statics, so an uncached VM reads nothing and answers `false`.
    #[test]
    fn w10f_module_tables_and_platform_package_memo_are_per_vm() {
        const VM_A: usize = 0x10F_2A00;
        const VM_B: usize = 0x10F_2B00;
        let mut ctx_a = MockNativeContext::new();
        ctx_a.set_vm_identity(VM_A);
        let mut ctx_b = MockNativeContext::new();
        ctx_b.set_vm_identity(VM_B);

        platform_package_memo()
            .lock()
            .entry(VM_A)
            .or_default()
            .insert("w10f/pkg".to_string(), true);
        assert!(package_is_platform_defined(&mut ctx_a, "w10f/pkg"));
        assert!(
            !package_is_platform_defined(&mut ctx_b, "w10f/pkg"),
            "VM A's memo must not answer for VM B"
        );

        builtin_module_sets_cache().lock().insert(
            VM_A,
            Arc::new(BuiltinModuleSets {
                boot: std::collections::HashSet::new(),
                platform: std::iter::once("java.sql".to_string()).collect(),
            }),
        );
        assert!(jdk_builtin_module_sets(&mut ctx_a).is_some());
        assert!(
            jdk_builtin_module_sets(&mut ctx_b).is_none(),
            "VM A's module tables must not answer for VM B"
        );

        forget_vm_loader_singletons(VM_A);
        forget_vm_loader_singletons(VM_B);
        assert!(builtin_module_sets_cache().lock().get(&VM_A).is_none());
        assert!(platform_package_memo().lock().get(&VM_A).is_none());
        assert!(
            !package_is_platform_defined(&mut ctx_a, "w10f/pkg"),
            "teardown drops the VM's memo"
        );
    }

    // -----------------------------------------------------------------
    // L1 — the four VM-internal loader fields live beside the object
    // -----------------------------------------------------------------

    /// Drop THIS test's entries from the process-global L1 table, and only
    /// those.
    ///
    /// `loader_meta_store()` is process-global and the test binary runs its
    /// tests on many threads at once, so a `.clear()` here deletes whatever a
    /// concurrently-running test just recorded. That is not hypothetical: it
    /// made two of these tests fail alternately, a different one each run,
    /// which reads like flakiness in the code under test rather than in the
    /// harness. Remove by address, the way
    /// `loader_namespace_store_is_pruned_and_remapped_by_gc_reconcile`
    /// already removes by id.
    fn forget_loader_meta(loaders: &[ObjectRef]) {
        let mine: Vec<usize> = loaders.iter().map(|l| l.as_ptr() as usize).collect();
        loader_meta_store()
            .lock()
            .retain(|(_, l, _)| !mine.contains(&(l.as_ptr() as usize)));
    }

    /// Declare `names` as instance fields of `cid`, so
    /// `resolve_field_index_by_class_id` finds them — the mock's resolver
    /// consults `declared_fields`.
    fn declare_instance_fields(
        ctx: &MockNativeContext,
        cid: cratonvm_types::ClassId,
        names: &[&str],
    ) {
        let fields = names
            .iter()
            .enumerate()
            .map(|(i, n)| cratonvm_native_api::FieldMetadata {
                name: (*n).to_string(),
                descriptor: "Ljava/lang/Object;".to_string(),
                access_flags: 0,
                slot_index: i,
                declaring_class_id: cid,
                is_static: false,
            })
            .collect();
        ctx.set_declared_fields(cid, fields);
    }

    /// Build a `ClassLoader` subclass and an instance of it. `real_layout`
    /// declares the JDK's own private fields, which is what tells the two
    /// layouts apart.
    fn make_loader(ctx: &mut MockNativeContext, class_name: &str, real_layout: bool) -> ObjectRef {
        let class_loader = ctx
            .ensure_class_initialized("java/lang/ClassLoader")
            .expect("ClassLoader class");
        let cid = ctx
            .ensure_class_initialized(class_name)
            .expect("loader class");
        ctx.set_superclass(cid, class_loader);
        if real_layout {
            declare_instance_fields(
                ctx,
                cid,
                &[
                    "parent",
                    "name",
                    "unnamedModule",
                    "nameAndId",
                    "parallelLockMap",
                    "package2certs",
                    "classes",
                ],
            );
        }
        new_object_ref(ctx, class_name)
    }

    /// The predicate must be FALSIFIABLE. `object_num_fields(x) >= N` was the
    /// first version of it and shipped completely inert — it answers the same
    /// on both layouts. Inject the real JDK's own field names and watch the
    /// answer flip; that is the whole difference the fix rests on.
    #[test]
    fn cl_layout_predicate_is_decided_by_a_real_jdk_field_name() {
        let mut ctx = MockNativeContext::new();
        let ours = make_loader(&mut ctx, "example/L1SyntheticLoader", false);
        let theirs = make_loader(&mut ctx, "example/L1RealLayoutLoader", true);

        assert!(
            cl_has_synthetic_layout(&ctx, ours),
            "a fabricated stub declares no fields at all — that IS our layout"
        );
        assert!(
            !cl_has_synthetic_layout(&ctx, theirs),
            "a class declaring java.lang.ClassLoader's own private fields is \
             the REAL layout; if this passes, the predicate cannot fail and \
             the whole fix is inert"
        );
    }

    /// Every witness name on its own has to be enough — the point of carrying
    /// three is that a rename in a future JDK degrades one at a time instead
    /// of flipping the predicate wholesale.
    #[test]
    fn cl_layout_predicate_fires_on_any_single_witness_field() {
        for (i, witness) in REAL_CLASSLOADER_WITNESS_FIELDS.iter().enumerate() {
            let mut ctx = MockNativeContext::new();
            let class_loader = ctx
                .ensure_class_initialized("java/lang/ClassLoader")
                .expect("ClassLoader class");
            let name = format!("example/L1Witness{i}");
            let cid = ctx.ensure_class_initialized(&name).expect("loader class");
            ctx.set_superclass(cid, class_loader);
            declare_instance_fields(&ctx, cid, &[witness]);
            let loader = new_object_ref(&mut ctx, &name);
            assert!(
                !cl_has_synthetic_layout(&ctx, loader),
                "`{witness}` alone must identify the real JDK layout"
            );
        }
    }

    /// On the real layout the four `CL_*` slots are `parent` / `nameAndId` /
    /// `parallelLockMap` / `classes` — references, all four. Writing an `Int`
    /// there is coerced to `Object(None)` and destroys the JDK's field. The
    /// eight measured census rows were exactly these writes on
    /// `ClassLoaders$AppClassLoader` and `$PlatformClassLoader`.
    #[test]
    fn cl_init_leaves_the_four_vm_internal_slots_untouched_on_a_real_layout() {
        let mut ctx = MockNativeContext::new();
        let loader = make_loader(&mut ctx, "example/L1RealCtorLoader", true);

        // A recognisable reference in every one of the four slots. If the
        // native writes an `Int` over it, the slot stops being this object.
        let sentinel = new_object_ref(&mut ctx, "java/lang/Object");
        for slot in [
            CL_LOADER_TYPE,
            CL_CLASSES_LOADED,
            CL_IS_PARALLEL_CAPABLE,
            CL_LOADER_ID,
        ] {
            ctx.set_field(loader, slot, Value::Object(Some(sentinel)));
        }

        let name = ctx.create_string("l1");
        let parent = make_loader(&mut ctx, "example/L1RealCtorParent", true);
        cl_init_name_parent(
            &mut ctx,
            &[
                Value::Object(Some(loader)),
                Value::Object(Some(name)),
                Value::Object(Some(parent)),
            ],
        )
        .expect("ClassLoader(String, ClassLoader) native");

        // Slot 0 IS the real `parent`, so it legitimately receives the parent
        // LOADER — by name. What it must never receive is `Int(LOADER_CUSTOM)`,
        // which `set_field_as` coerces to `Object(None)`.
        assert_eq!(
            ctx.get_field(loader, CL_LOADER_TYPE),
            Value::Object(Some(parent)),
            "slot 0 is `parent` on the real layout: the parent loader, never \
             the VM's loader-type tag"
        );
        // 3 / 4 / 6 are `nameAndId` / `parallelLockMap` / `classes` and have no
        // CratonVM counterpart at all — nothing may be written there.
        for slot in [CL_CLASSES_LOADED, CL_IS_PARALLEL_CAPABLE, CL_LOADER_ID] {
            assert_eq!(
                ctx.get_field(loader, slot),
                Value::Object(Some(sentinel)),
                "slot {slot} is a JDK reference field with no CratonVM \
                 counterpart and must not be overwritten with VM bookkeeping"
            );
        }

        // ... and the values are not lost: they moved next to the object.
        let meta =
            loader_meta_get(ctx.vm_identity(), loader).expect("constructor must record the meta");
        assert_eq!(meta.loader_type, Some(LOADER_CUSTOM));
        assert_eq!(meta.classes_loaded, Some(0));
        assert_eq!(meta.parallel_capable, Some(true));
        assert!(
            meta.loader_id.is_some_and(|id| id > 0),
            "the (String, ClassLoader) form assigns a namespace id"
        );
        assert_eq!(loader_type_of(&ctx, loader), Some(LOADER_CUSTOM));
        assert_eq!(loader_id_of(&ctx, loader), meta.loader_id);
        assert_eq!(loader_parallel_capable_of(&ctx, loader), Some(1));

        forget_loader_meta(&[loader, parent]);
    }

    /// Synthetic-JDK mode is unchanged: the slots are still written, so a
    /// reader that has not been converted (or a loader whose meta entry was
    /// dropped) still finds the values where they have always been.
    #[test]
    fn cl_init_still_writes_the_slots_on_our_own_layout() {
        let mut ctx = MockNativeContext::new();
        let loader = make_loader(&mut ctx, "example/L1SyntheticCtorLoader", false);

        let name = ctx.create_string("l1");
        cl_init_name_parent(
            &mut ctx,
            &[
                Value::Object(Some(loader)),
                Value::Object(Some(name)),
                Value::Object(None),
            ],
        )
        .expect("ClassLoader(String, ClassLoader) native");

        assert_eq!(
            ctx.get_field(loader, CL_LOADER_TYPE),
            Value::Int(LOADER_CUSTOM)
        );
        assert_eq!(ctx.get_field(loader, CL_CLASSES_LOADED), Value::Int(0));
        assert_eq!(ctx.get_field(loader, CL_IS_PARALLEL_CAPABLE), Value::Int(1));
        let slot_id = match ctx.get_field(loader, CL_LOADER_ID) {
            Value::Int(v) => v,
            other => panic!("expected a namespace id in slot 6, got {other:?}"),
        };
        assert!(slot_id > 0);
        assert_eq!(loader_id_of(&ctx, loader), Some(slot_id as u32));

        // The raw-slot fallback is what keeps a loader allocated outside our
        // path working. Drop the table entry and the slots must still answer.
        forget_loader_meta(&[loader]);
        assert_eq!(loader_type_of(&ctx, loader), Some(LOADER_CUSTOM));
        assert_eq!(loader_classes_loaded_of(&ctx, loader), Some(0));
        assert_eq!(loader_parallel_capable_of(&ctx, loader), Some(1));
        assert_eq!(loader_id_of(&ctx, loader), Some(slot_id as u32));
    }

    /// The fallback must NOT fire on the real layout: slot 4 there is
    /// `parallelLockMap`, and reading it back as this VM's parallel-capable
    /// flag is the same type confusion the write side just stopped
    /// committing. "No `CL_*` value is read from an object slot on a
    /// real-JDK run" is half of L1's done-when.
    #[test]
    fn readers_do_not_fall_back_to_raw_slots_on_a_real_layout() {
        let mut ctx = MockNativeContext::new();
        let loader = make_loader(&mut ctx, "example/L1NoFallbackLoader", true);

        // Plant values that WOULD be read if the fallback were ungated.
        ctx.set_field(loader, CL_LOADER_TYPE, Value::Int(LOADER_PLATFORM));
        ctx.set_field(loader, CL_CLASSES_LOADED, Value::Int(77));
        ctx.set_field(loader, CL_IS_PARALLEL_CAPABLE, Value::Int(0));
        ctx.set_field(loader, CL_LOADER_ID, Value::Int(4242));

        assert_eq!(loader_type_of(&ctx, loader), None);
        assert_eq!(loader_classes_loaded_of(&ctx, loader), None);
        assert_eq!(loader_parallel_capable_of(&ctx, loader), None);
        assert_eq!(loader_id_of(&ctx, loader), None);

        // `isRegisteredAsParallelCapable` therefore answers the same `true`
        // `registerAsParallelCapable` reports, rather than the `0` that
        // happened to be sitting in `parallelLockMap`'s slot.
        let answer = cl_is_registered_as_parallel_capable(&mut ctx, &[Value::Object(Some(loader))])
            .expect("native")
            .expect("return value");
        assert_eq!(answer, Value::Int(1));
    }

    /// The three REFERENCE slots are the same defect one kind quieter: slot 1
    /// is `name:String` and receives a ClassLoader, slot 2 is
    /// `unnamedModule:Module` and receives a String, slot 5 is
    /// `package2certs:ConcurrentHashMap` and receives a ProtectionDomain. A
    /// reference for a reference, so the overlay detector cannot see them —
    /// which is why the wave-2 brief's step 4 left them in place on the
    /// (mistaken) grounds that the by-name writes duplicate them.
    #[test]
    fn cl_init_leaves_the_three_reference_slots_untouched_on_a_real_layout() {
        let mut ctx = MockNativeContext::new();
        let loader = make_loader(&mut ctx, "example/L1RealRefSlotLoader", true);
        let sentinel = new_object_ref(&mut ctx, "java/lang/Object");
        for slot in [CL_PARENT_REF, CL_NAME_REF, CL_DEFAULT_DOMAIN] {
            ctx.set_field(loader, slot, Value::Object(Some(sentinel)));
        }

        let name = ctx.create_string("l1-refslots");
        let parent = make_loader(&mut ctx, "example/L1RefSlotParent", true);
        cl_init_name_parent(
            &mut ctx,
            &[
                Value::Object(Some(loader)),
                Value::Object(Some(name)),
                Value::Object(Some(parent)),
            ],
        )
        .expect("ClassLoader(String, ClassLoader) native");

        // Slot 1 IS `name` and legitimately receives the name STRING — by
        // name. The defect was the index write putting the parent LOADER
        // there.
        assert_eq!(
            ctx.get_field(loader, CL_PARENT_REF),
            Value::Object(Some(name)),
            "slot 1 is `name` on the real layout: the name String, never the \
             parent loader"
        );
        // 2 and 5 are `unnamedModule` and `package2certs`. Nothing CratonVM
        // holds belongs in either.
        for slot in [CL_NAME_REF, CL_DEFAULT_DOMAIN] {
            assert_eq!(
                ctx.get_field(loader, slot),
                Value::Object(Some(sentinel)),
                "slot {slot} belongs to the JDK on a real layout"
            );
        }
        // `parent` and `name` still arrive — by NAME, which is where the real
        // fields actually are.
        assert_eq!(
            ctx.get_field_by_name(loader, "parent"),
            Value::Object(Some(parent))
        );
        assert_eq!(
            ctx.get_field_by_name(loader, "name"),
            Value::Object(Some(name))
        );
        assert_eq!(classloader_parent(&mut ctx, loader), Some(parent));

        forget_loader_meta(&[loader, parent]);
    }

    /// `classloader_parent`'s slot fallback read slot 1 unconditionally. On a
    /// real layout that is `java.lang.ClassLoader.name` — a String — so a
    /// parentless loader whose name had been written reported its own NAME as
    /// its PARENT, and every caller that walks the chain then treated a String
    /// as a ClassLoader.
    #[test]
    fn classloader_parent_does_not_return_the_name_string_as_a_parent() {
        let mut ctx = MockNativeContext::new();
        let real = make_loader(&mut ctx, "example/L1ParentFallbackReal", true);
        let name = ctx.create_string("platform");
        // Exactly what `get_or_create_platform_loader` produces: the real
        // `name` field set, the real `parent` genuinely null.
        ctx.set_field_by_name(real, "name", Value::Object(Some(name)));
        assert_eq!(
            ctx.get_field(real, CL_PARENT_REF),
            Value::Object(Some(name)),
            "precondition: slot 1 IS the name on the real layout"
        );
        assert_eq!(
            classloader_parent(&mut ctx, real),
            None,
            "a parentless real-layout loader has no parent — not its own name"
        );

        // Synthetic layout keeps the fallback: it is what makes an ordinary
        // `URLClassLoader` constructed with a non-null parent resolve at all.
        let ours = make_loader(&mut ctx, "example/L1ParentFallbackSynthetic", false);
        let p = make_loader(&mut ctx, "example/L1ParentFallbackSyntheticParent", false);
        ctx.set_field(ours, CL_PARENT_REF, Value::Object(Some(p)));
        assert_eq!(classloader_parent(&mut ctx, ours), Some(p));
    }

    /// The side table is authoritative, ahead of the slot — the ordering
    /// `vh_field_desc` uses.
    #[test]
    fn the_side_table_outranks_the_raw_slot() {
        let mut ctx = MockNativeContext::new();
        let loader = make_loader(&mut ctx, "example/L1PrecedenceLoader", false);
        ctx.set_field(loader, CL_LOADER_TYPE, Value::Int(LOADER_APP));
        loader_meta_put(
            ctx.vm_identity(),
            loader,
            LoaderMeta {
                loader_type: Some(LOADER_CUSTOM),
                ..LoaderMeta::default()
            },
        );

        assert_eq!(loader_type_of(&ctx, loader), Some(LOADER_CUSTOM));
        // A member the table does NOT carry still falls through to the slot.
        ctx.set_field(loader, CL_CLASSES_LOADED, Value::Int(5));
        assert_eq!(loader_classes_loaded_of(&ctx, loader), Some(5));

        forget_loader_meta(&[loader]);
    }

    /// Same GC contract as `loader_namespace_id_store`, and for the same
    /// reason: the table is keyed by a raw heap address. Prune the dead so a
    /// recycled address cannot make a brand-new loader inherit a dead one's
    /// namespace id; remap the moved so a relocated loader keeps its own.
    /// Deleting either half of the `retain_mut` fails here.
    #[test]
    fn loader_meta_store_is_pruned_and_remapped_by_gc_reconcile() {
        let mut ctx = MockNativeContext::new();
        let dead = make_loader(&mut ctx, "example/L1GcDeadLoader", false);
        let moved_from = make_loader(&mut ctx, "example/L1GcMovedLoader", false);
        let moved_to = new_object_ref(&mut ctx, "example/L1GcMovedLoader");

        const DEAD_NS: u32 = 90_101;
        const MOVED_NS: u32 = 90_102;
        loader_meta_put(
            ctx.vm_identity(),
            dead,
            LoaderMeta {
                loader_id: Some(DEAD_NS),
                ..LoaderMeta::default()
            },
        );
        loader_meta_put(
            ctx.vm_identity(),
            moved_from,
            LoaderMeta {
                loader_id: Some(MOVED_NS),
                ..LoaderMeta::default()
            },
        );
        assert_eq!(loader_id_of(&ctx, dead), Some(DEAD_NS));

        let dead_addr = dead.as_ptr() as usize;
        let from_addr = moved_from.as_ptr() as usize;
        let to_addr = moved_to.as_ptr() as usize;
        // Process-global store in a multi-threaded test binary: report every
        // address this test does not own as ALIVE, so nothing another test
        // registered can be pruned by this call.
        let is_marked = move |addr: usize| addr != dead_addr;
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(from_addr, to_addr);

        gc_reconcile_defining_loaders(ctx.vm_identity(), &is_marked, &pointer_map, None);

        assert!(
            loader_meta_get(ctx.vm_identity(), dead).is_none(),
            "a loader collected this cycle must not keep its bookkeeping entry"
        );
        assert_eq!(
            loader_meta_get(ctx.vm_identity(), moved_to).and_then(|m| m.loader_id),
            Some(MOVED_NS),
            "a relocated loader must keep its bookkeeping at its new address"
        );
        assert!(
            loader_meta_get(ctx.vm_identity(), moved_from).is_none(),
            "the pre-collection address must no longer resolve"
        );

        forget_loader_meta(&[dead, moved_from, moved_to]);
    }

    /// `loader_object_for_namespace_id` has to be able to invert an id that a
    /// constructor native assigned. Before L1 real-JDK mode got that for free
    /// (the slot write was coerced away, so `loader_namespace_id_at` allocated
    /// a fresh id and stored the object); now the constructor's id survives,
    /// so it has to be mirrored in explicitly.
    #[test]
    fn a_constructor_assigned_namespace_id_is_invertible() {
        let mut ctx = MockNativeContext::new();
        let loader = make_loader(&mut ctx, "example/L1InvertibleLoader", true);
        let name = ctx.create_string("l1");
        cl_init_name_parent(
            &mut ctx,
            &[
                Value::Object(Some(loader)),
                Value::Object(Some(name)),
                Value::Object(None),
            ],
        )
        .expect("ClassLoader(String, ClassLoader) native");

        let ns = loader_namespace_id(&mut ctx, loader);
        assert!(ns > 0, "a user-defined loader gets its own namespace");
        assert_eq!(
            loader_object_for_namespace_id(ns).map(|o| o.as_ptr()),
            Some(loader.as_ptr()),
            "the id must resolve back to the loader that owns it"
        );

        loader_namespace_id_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, _, id)| *id != ns);
        forget_loader_meta(&[loader]);
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
        register_defining_loader(ctx.vm_identity(), cid.as_u32(), child_loader);

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

    /// W7-82. A bare `java.net.URLClassLoader` is a JDK CLASS but a
    /// user-defined LOADER — it is the only entry on `is_builtin_loader_class`'s
    /// list with a public constructor. `loader_namespace_id_at` already carves
    /// it out and gives it its own namespace to define into; this function must
    /// carve it out too, or the loader defines into a namespace it can never
    /// see and every later lookup re-drives the define, which the class
    /// manager's duplicate-define check then correctly rejects.
    #[test]
    fn test_bare_url_class_loader_sees_the_class_it_defined_itself() {
        let mut ctx = MockNativeContext::new();
        let loader = match ctx.new_object("java/net/URLClassLoader").unwrap() {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected classloader object, got {other:?}"),
        };
        let cid = ctx.ensure_class_initialized("aux/Target").unwrap();
        // A user namespace: exactly what `loader_namespace_id_at` hands a bare
        // `URLClassLoader`, and what the built-in branch's `> 2` clause hides.
        ctx.set_loader_id_override(cid, 7);
        register_defining_loader(ctx.vm_identity(), cid.as_u32(), loader);

        assert!(
            find_loaded_class_for_loader(&mut ctx, loader, "aux/Target").is_some(),
            "a bare URLClassLoader must see the class it is itself recorded as \
             having defined; hiding it re-drives the define and the duplicate \
             check rejects the second Class.forName"
        );
    }

    /// The other half, so the carve-out above cannot be widened into "a bare
    /// `URLClassLoader` sees any user-namespace class of that name". The
    /// two-independent-loaders isolation in `ForNameCacheProbe` group3 is the
    /// end-to-end form of this.
    #[test]
    fn test_bare_url_class_loader_does_not_see_another_loaders_class() {
        let mut ctx = MockNativeContext::new();
        let loader = match ctx.new_object("java/net/URLClassLoader").unwrap() {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected classloader object, got {other:?}"),
        };
        let other = match ctx.new_object("bsh/classpath/BshClassLoader").unwrap() {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected child loader object, got {other:?}"),
        };
        let cid = ctx.ensure_class_initialized("aux/Foreign").unwrap();
        ctx.set_loader_id_override(cid, 7);
        register_defining_loader(ctx.vm_identity(), cid.as_u32(), other);

        assert!(
            find_loaded_class_for_loader(&mut ctx, loader, "aux/Foreign").is_none(),
            "a bare URLClassLoader must NOT see a user-namespace class another \
             loader defined"
        );
    }

    /// W7-87 — the NARROWING half. `loader_id_of_class(cid) > 2 -> hide` only
    /// ever hid USER-namespace classes; the built-in branch's global fallback
    /// still handed a bare `URLClassLoader` any APPLICATION-namespace class of
    /// that name, one it never defined and was never asked to load. HotSpot 25
    /// answers `null` (`findLoadedClass`) / `ClassNotFoundException`
    /// (`loadClass`) — measured, not assumed. A bare `new URLClassLoader(urls,
    /// null)` is THE isolating-loader idiom, so this fallback defeated the
    /// isolation it was constructed for.
    ///
    /// The receiver here is a BARE `java/net/URLClassLoader` on purpose: the
    /// 2026-07-01 commit that produced W7-82 shipped two tests that instantiate
    /// `java/lang/ClassLoader`, and that is exactly why the case went unseen for
    /// six weeks. A `URLClassLoader` SUBCLASS was always correct — it is
    /// `is_user_defined_loader` and has no global fallback — so a test written
    /// against a subclass cannot fail here.
    #[test]
    fn test_bare_url_class_loader_does_not_see_an_app_namespace_class_it_never_loaded() {
        let mut ctx = MockNativeContext::new();
        let loader = match ctx.new_object("java/net/URLClassLoader").unwrap() {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected classloader object, got {other:?}"),
        };
        let cid = ctx.ensure_class_initialized("app/Ordinary").unwrap();
        // Application namespace, no registered defining loader: an ordinary
        // classpath class the application loader owns. `> 2` never fired for
        // this, so the global fallback used to return it.
        ctx.set_loader_id_override(cid, 2);

        assert!(
            find_loaded_class_for_loader(&mut ctx, loader, "app/Ordinary").is_none(),
            "a bare URLClassLoader must NOT see an application-namespace class \
             it neither defined nor was asked to load; HotSpot's findLoadedClass \
             reports null and its loadClass raises ClassNotFoundException"
        );
        // The CONTROL, in the same shape: a genuine built-in loader still sees
        // it. `test_builtin_find_loaded_class_keeps_application_namespace_hit`
        // asserts this independently and is deliberately left untouched; if the
        // narrowing had escaped its carve-out, that test would go red too.
        let builtin = match ctx.new_object("java/lang/ClassLoader").unwrap() {
            Some(Value::Object(Some(obj))) => obj,
            other => panic!("expected classloader object, got {other:?}"),
        };
        assert!(
            find_loaded_class_for_loader(&mut ctx, builtin, "app/Ordinary").is_some(),
            "the narrowing is scoped to java/net/URLClassLoader; a genuine \
             built-in loader must keep its global fallback"
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

    /// gc-common w10-f: `defineClass0`'s `classData` lands in the new class's
    /// mirror, in the `java.lang.Class.classData` field the JDK's
    /// `MethodHandles.classData` reads, not in a write-only side table. The
    /// mock mirror is class 0 with two slots; declaring `classData` at slot 1
    /// stands in for the real `java.lang.Class` layout.
    #[test]
    fn w10f_define_class0_puts_class_data_in_the_mirror_field() {
        use cratonvm_native_api::FieldMetadata;
        use cratonvm_types::{ArrayElementType, ClassId};
        let mut ctx = MockNativeContext::new();
        ctx.set_declared_fields(
            ClassId::new(0),
            vec![FieldMetadata {
                name: "classData".to_string(),
                descriptor: "Ljava/lang/Object;".to_string(),
                access_flags: 0,
                slot_index: 1,
                declaring_class_id: ClassId::new(0),
                is_static: false,
            }],
        );
        let payload = ctx.fresh_object_ref();
        let class_bytes: Vec<u8> = vec![
            0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x41, 0x00, 0x01, 0x00, 0x21, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let bytes = ctx.new_array(ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(bytes, i, Value::Int(*b as i32));
        }
        let name = ctx.create_string("w10f/WithClassData");
        let result = cl_define_class0(
            &mut ctx,
            &[
                Value::Object(None),          // loader
                Value::Object(None),          // lookup class
                Value::Object(Some(name)),    // name
                Value::Object(Some(bytes)),   // b
                Value::Int(0),                // off
                Value::Int(class_bytes.len() as i32), // len
                Value::Object(None),          // pd
                Value::Int(0),                // initialize
                Value::Int(DEFINE_CLASS0_FLAG_HIDDEN),
                Value::Object(Some(payload)), // classData
            ],
        );
        let mirror = match result {
            Ok(Some(Value::Object(Some(m)))) => m,
            other => panic!("defineClass0 must return the new class's mirror, got {other:?}"),
        };
        assert_eq!(
            ctx.get_field(mirror, 1),
            Value::Object(Some(payload)),
            "classData must be stored in the mirror's Class.classData field"
        );
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
    fn test_url_class_path_boolean_get_urls_registered() {
        let r = make_registry();
        assert!(r
            .find(
                "jdk/internal/loader/URLClassPath",
                "getURLs",
                "(Z)[Ljava/net/URL;",
            )
            .is_some());
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

    /// The class every LIVE `MethodHandles` static is registered on.
    ///
    /// `lookup()`, `publicLookup()` and `privateLookupIn(..)` are `static`
    /// members of `java.lang.invoke.MethodHandles`. `MethodHandles$Lookup`
    /// declares none of the three in any real JDK, so this module's
    /// registrations of those names on [`LK_CLASS`] address triples that
    /// `--real-jdk` and `--jdk-only` can never dispatch to — and
    /// `register_classloader_natives` is itself reachable only through
    /// `register_synthetic_overrides`, so they exist at all only in a
    /// `--features synthetic-jdk` build.
    const MH_STATICS_CLASS: &str = "java/lang/invoke/MethodHandles";

    /// W4-1's live residual, and it is the same species as the six tests W7-62
    /// moved out of this module: an assertion aimed only at the `LK_CLASS`
    /// triple reads as coverage of `MethodHandles.lookup()` while guarding a
    /// registration no live path reaches. W7-62 kept `lk_lookup` /
    /// `lk_public_lookup` on the grounds that they ARE registered; registered
    /// is not reachable, and the tests are the half that had to move.
    ///
    /// Both halves are asserted, LIVE FIRST: the first assertion is the one
    /// that goes red if `lang_invoke::register_p63_method_handles_lookup` ever
    /// stops registering the static, which is the failure that would actually
    /// break a running VM. The second is kept and labelled so that dropping the
    /// synthetic-mode twin still surfaces here rather than silently.
    #[test]
    fn test_lk_lookup_registered() {
        let r = make_registry();
        assert!(
            r.find(
                MH_STATICS_CLASS,
                "lookup",
                "()Ljava/lang/invoke/MethodHandles$Lookup;"
            )
            .is_some(),
            "the LIVE MethodHandles.lookup() static must stay registered"
        );
        assert!(
            r.find(
                LK_CLASS,
                "lookup",
                "()Ljava/lang/invoke/MethodHandles$Lookup;"
            )
            .is_some(),
            "this module's MethodHandles$Lookup twin (synthetic-jdk only)"
        );
    }

    /// See [`test_lk_lookup_registered`] — same rule, and this is the exact
    /// entry point W4-1 was filed for. `publicLookup()` reaching a private
    /// method was the defect; a green test on the unreachable `LK_CLASS` twin
    /// was part of what made it look covered.
    #[test]
    fn test_lk_public_lookup_registered() {
        let r = make_registry();
        assert!(
            r.find(
                MH_STATICS_CLASS,
                "publicLookup",
                "()Ljava/lang/invoke/MethodHandles$Lookup;"
            )
            .is_some(),
            "the LIVE MethodHandles.publicLookup() static must stay registered"
        );
        assert!(
            r.find(
                LK_CLASS,
                "publicLookup",
                "()Ljava/lang/invoke/MethodHandles$Lookup;"
            )
            .is_some(),
            "this module's MethodHandles$Lookup twin (synthetic-jdk only)"
        );
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
        let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();
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
        let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();
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
        let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();

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
        let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();

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
        let option = try_alloc_concurrent_synthetic(
            &mut ctx,
            "java/lang/invoke/MethodHandles$Lookup$ClassOption",
            1,
        )
        .unwrap();
        ctx.set_field(option, 0, Value::Int(0)); // NESTMATE ordinal
        let options_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(options_arr, 0, Value::Object(Some(option)));

        let lookup = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();

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

    /// The two "full power" values are DIFFERENT numbers and both are load
    /// bearing: 95 is what `MethodHandles.lookup().lookupModes()` answers,
    /// 0x1F is the JDK's own `FULL_POWER_MODES` mask that `in`/`dropLookupMode`
    /// apply. Measured on OpenJDK 25.0.3.
    #[test]
    fn test_full_power_modes_mask_excludes_original() {
        assert_eq!(LK_FULL_POWER, 95);
        assert_eq!(LK_FULL_POWER_MODES, 0x1F);
        assert_eq!(LK_FULL_POWER_MODES & LK_ORIGINAL, 0);
        assert_eq!(LK_FULL_POWER_MODES & LK_UNCONDITIONAL, 0);
    }

    /// `Lookup.dropLookupMode` against the real JDK 25 answers.
    ///
    /// Every row was read off `java LkProbe` on OpenJDK 25.0.3 with the
    /// receiver `MethodHandles.lookup()` (modes 95) — not derived from the JDK
    /// source, and deliberately NOT from `old & !drop`, which this asserts is
    /// wrong for all seven droppable modes.
    #[test]
    fn test_drop_lookup_mode_matches_jdk25() {
        for (drop, expected) in [
            (LK_PUBLIC, 0),
            (LK_PRIVATE, 25),
            (LK_PROTECTED, 27),
            (LK_PACKAGE, 17),
            (LK_MODULE, 1),
            (LK_UNCONDITIONAL, 27),
            (LK_ORIGINAL, 27),
        ] {
            assert_eq!(
                lk_drop_modes(LK_FULL_POWER, drop),
                Some(expected),
                "dropLookupMode(0x{drop:x}) on modes 95"
            );
            assert_ne!(
                LK_FULL_POWER & !drop,
                expected,
                "the naive `old & !drop` must NOT coincide with the JDK answer \
                 for 0x{drop:x} — if it does, this test has stopped proving anything"
            );
        }
    }

    /// Anything that is not exactly one of the seven mode constants is refused.
    /// Measured: `dropLookupMode(0)` and `dropLookupMode(PRIVATE|PROTECTED)`
    /// both raise `IllegalArgumentException` on JDK 25.
    #[test]
    fn test_drop_lookup_mode_rejects_non_modes() {
        assert_eq!(lk_drop_modes(LK_FULL_POWER, 0), None);
        assert_eq!(
            lk_drop_modes(LK_FULL_POWER, LK_PRIVATE | LK_PROTECTED),
            None
        );
        assert_eq!(lk_drop_modes(LK_FULL_POWER, 0x80), None);
    }

    /// `Lookup.in`, measured on OpenJDK 25.0.3 from `MethodHandles.lookup()`
    /// (modes 95). The lookup class itself keeps 95; a NESTMATE gets 31; a
    /// same-package class in another file gets **25**, not 31, because
    /// `isSamePackageMember` strips `PRIVATE|PROTECTED` from a "cousin"; and
    /// `String.class` gets 1.
    #[test]
    fn test_in_modes_matches_jdk25() {
        // (same_class, same_package, same_nest, target_is_public)
        assert_eq!(lk_in_modes(LK_FULL_POWER, true, true, true, true), 95);
        assert_eq!(lk_in_modes(LK_FULL_POWER, false, true, true, true), 31);
        assert_eq!(lk_in_modes(LK_FULL_POWER, false, true, false, true), 25);
        assert_eq!(lk_in_modes(LK_FULL_POWER, false, false, false, true), 1);
        // A full-power lookup's reduction does not depend on the target being
        // public — a package-private nestmate is still 31.
        assert_eq!(lk_in_modes(LK_FULL_POWER, false, true, true, false), 31);
        assert_eq!(lk_in_modes(LK_FULL_POWER, false, true, false, false), 25);
        // An already-reduced lookup never REGAINS a mode.
        assert_eq!(lk_in_modes(25, false, true, true, true), 25);
        assert_eq!(lk_in_modes(25, false, false, false, true), 1);
        assert_eq!(lk_in_modes(1, false, true, true, true), 1);
    }

    /// `publicLookup()` (UNCONDITIONAL, 32) through `in()`: KEPT for a PUBLIC
    /// target, and **0** for one that is not.
    ///
    /// The regression this pins is a whole-value one, not an edge: the arm used
    /// to `return prev` for every target, so `publicLookup().in(<a
    /// package-private class>)` reported 32 — a lookup able to resolve public
    /// members of a class the JDK hands no lookup at all. Measured on OpenJDK
    /// 25.0.3 (`PubIn`): a public nested class 32, a package-private nested
    /// class 0, a package-private top-level class 0, `java.lang.String` 32.
    #[test]
    fn test_public_lookup_in_drops_to_zero_for_a_non_public_target() {
        assert_eq!(lk_in_modes(LK_UNCONDITIONAL, false, false, false, true), 32);
        assert_eq!(lk_in_modes(LK_UNCONDITIONAL, false, true, true, true), 32);
        assert_eq!(lk_in_modes(LK_UNCONDITIONAL, false, false, false, false), 0);
        assert_eq!(lk_in_modes(LK_UNCONDITIONAL, false, true, true, false), 0);
        // …including a same-package cousin, which the `same_package` arm below
        // would otherwise have kept at 32.
        assert_eq!(lk_in_modes(LK_UNCONDITIONAL, false, true, false, false), 0);
    }

    /// The nestmate approximation: `p/Outer` and `p/Outer$Inner` share an
    /// outermost class; `p/Outer` and `p/Mate` do not.
    #[test]
    fn test_in_modes_nestmate_beats_bare_package_match() {
        // Package-mates that are NOT nestmates lose PRIVATE|PROTECTED …
        assert_eq!(
            lk_in_modes(LK_FULL_POWER, false, true, false, true) & LK_PRIVATE,
            0
        );
        // … while nestmates keep them.
        assert_ne!(
            lk_in_modes(LK_FULL_POWER, false, true, true, true) & LK_PRIVATE,
            0
        );
    }

    /// `lk_modes_of` must read the SYNTHETIC slot when the receiver's class
    /// does not declare `allowedModes`.
    ///
    /// The trap this pins: a by-name-first reader returned 0 for every
    /// fabricated Lookup and never consulted the slot that holds the value, so
    /// `lookupModes()` reported a powerless Lookup for the whole of
    /// synthetic-JDK mode.
    ///
    /// Note that this test exercises ONE of the two absent-field answers.
    /// `MockNativeContext` answers `Int(0)` for an unresolvable name;
    /// production (`vm_exec.rs::get_field_by_name`) answers
    /// `Value::Object(None)`. `lk_modes_of` survives both only because it asks
    /// the CLASS first — under production's answer a by-name-first reader
    /// would fall through the `Value::Int` arm instead of latching a false 0,
    /// but it still would not know which slot to read. Do not read a green
    /// result here as evidence about the `Int(0)` convention; there isn't one.
    #[test]
    fn test_lk_modes_of_reads_synthetic_slot_when_field_absent() {
        // The mock declares no fields for a fresh class, so `allowedModes` is
        // absent and the MOCK's `get_field_by_name` answers `Int(0)` for it.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let lk = alloc_lookup(&mut ctx, LK_FULL_POWER).unwrap();
        assert_eq!(lk_modes_of(&ctx, lk), LK_FULL_POWER);
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
    // MOVED 2026-08-12 to `lang_invoke.rs`'s test module.
    //
    // Six tests lived here — five aimed at `lk_find_virtual` / `lk_find_getter`
    // and one at this module's `lk_member_access_flags`. All six were green on
    // every run and none of them touched code the VM can reach: those bodies
    // were never registered (see the DELETED block above `lk_unreflect` for
    // why). They now sit beside `lk_enforce_find_access` in `lang_invoke.rs`,
    // aimed at the gate all ten `lookup_find_*` really call, and two arms the
    // old tests could not express were added there — a genuine `publicLookup()`
    // mode word (UNCONDITIONAL, not PUBLIC) and a zero-mode Lookup.
    // W7-62-ratchets-and-dead-code.md
    // -----------------------------------------------------------------------

    /// `MockNativeContext::resolve_field_index_by_class_id` now falls back to
    /// `ClassManager::synthetic_stub_fields`, the same table the VM resolves a
    /// name against for a class with no real bytes.
    ///
    /// Before this, the mock answered `None` for every field of every modelled
    /// class that had no hand-written `mock_*_field_slot` helper — i.e. "no such
    /// field" about fields the VM does resolve. Any predicate of the form
    /// *"does this class declare <a field only the REAL JDK class has>"* was
    /// therefore **unfalsifiable** under the mock: it could only ever answer
    /// one way, and a test of it passed vacuously.
    /// [`cl_has_synthetic_layout`] is exactly that shape.
    ///
    /// Both directions are asserted, because a fallback that answered `Some`
    /// for *everything* would be just as useless as one that answered `None`.
    #[test]
    fn resolve_by_class_id_sees_the_fabricated_model() {
        let mut ctx = MockNativeContext::new();
        let cid = ctx
            .ensure_class_initialized("java/security/ProtectionDomain")
            .expect("mock ensure_class_initialized must succeed");

        // The model's real JDK declaration order, pinned in
        // `classloading/src/shadow_layout.rs`. A modelled class with no
        // hand-written mock table is the case this fallback exists for.
        for (name, want) in [
            ("codesource", 0usize),
            ("classloader", 1),
            ("principals", 2),
            ("permissions", 3),
        ] {
            assert_eq!(
                ctx.resolve_field_index_by_class_id(cid, name),
                Some(want),
                "ProtectionDomain.{name} must resolve to its modelled slot"
            );
        }

        // A name the class does not declare still answers `None`, so the
        // predicate can still be falsified in the negative direction.
        assert_eq!(
            ctx.resolve_field_index_by_class_id(cid, "parallelLockMap"),
            None
        );
        assert_eq!(
            ctx.resolve_field_index_by_class_id(cid, "nosuchfield"),
            None
        );
    }

    /// The fallback is the TAIL of the chain, not the head.
    ///
    /// `mock_jdk_field_slot` is a deliberately arbitrary shared name->slot
    /// namespace for the `Field`/`Method`/`Constructor`/`MemberName` mirrors,
    /// and it disagrees with the fabricated model on every one of those names.
    /// It has to keep winning here because it wins in `get_field_by_name` /
    /// `set_field_by_name`: a reader and a writer that resolve the same name to
    /// different slots is worse than either mapping being "wrong". Putting the
    /// model first makes `create_method_object` write `modifiers` to one slot
    /// and `method_modifiers_value` read it from another — measured, three
    /// tests red.
    /// Harvest every class name the fabricated model could be asked about,
    /// straight out of the model's own source. A hand-written list would go
    /// stale silently, which is the failure mode this whole record is about.
    fn harvested_model_class_names() -> Vec<String> {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../classloading/src/class_manager.rs"
        ))
        .expect("classloading/src/class_manager.rs must be readable from the test");
        let bytes = src.as_bytes();
        let mut names: Vec<String> = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] != b'"' {
                i += 1;
                continue;
            }
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'"' {
                if bytes[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
            let lit = &src[start..j];
            if lit.len() > 3
                && lit.contains('/')
                && lit
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '$')
            {
                names.push(lit.to_string());
            }
            i = j + 1;
        }
        names.sort();
        names.dedup();
        names
    }

    /// THE GATE for this record's second half: **the mock's hand-written slot
    /// tables must not shadow the table the VM itself resolves names against.**
    ///
    /// §3 fixed the direction where the mock answered "no such field" about a
    /// field the VM resolves. This is the other direction, and it was live:
    /// `mock_jdk_field_slot` — the deliberately arbitrary flat namespace for
    /// the `Field`/`Method`/`Constructor`/`MemberName` mirrors — was consulted
    /// **class-blind and ahead of the model**, so it answered for any modelled
    /// class declaring one of its fifteen names. Measured across the 333
    /// classes `synthetic_stub_field_model` models, it shadowed `name` on
    /// twenty-seven of them (`java.lang.Enum`, `java.security.Permission`,
    /// `java.util.logging.Logger`, `org.xnio.Xnio`, `org.jboss.modules.Module`,
    /// …), each of which models `name` at slot 0 against the namespace's 1;
    /// and `io.undertow.server.HttpServerExchange.responseHeaders` at the
    /// mock's 3 against the model's 5.
    ///
    /// The reflect mirrors are the one exemption, and it is enumerated in
    /// `mock_reflect_mirror_field_slot` rather than implied — production keeps
    /// the same flat layout as `METHOD_LEGACY_SLOT_*` and allocates the mirror
    /// at eight fields, so the model's slots for the later names are past the
    /// end of the object.
    #[test]
    fn the_mock_slot_tables_do_not_shadow_the_fabricated_model() {
        const MIRRORS: &[&str] = &[
            "java/lang/reflect/Field",
            "java/lang/reflect/Method",
            "java/lang/reflect/Constructor",
            "java/lang/reflect/Executable",
            "java/lang/reflect/AccessibleObject",
            "java/lang/invoke/MemberName",
        ];

        let names = harvested_model_class_names();
        assert!(
            names.len() > 300,
            "the literal walker found only {} candidate class names in \
             class_manager.rs — the walker is broken, not the model",
            names.len()
        );

        let mut modelled = 0usize;
        let mut shadowed: Vec<String> = Vec::new();
        for cls in &names {
            let model = cratonvm_classloading::synthetic_stub_field_model(cls);
            let instance: Vec<_> = model.iter().filter(|f| !f.is_static()).collect();
            if instance.is_empty() {
                continue;
            }
            modelled += 1;
            if MIRRORS.contains(&cls.as_str()) {
                continue;
            }
            let mut ctx = MockNativeContext::new();
            let Ok(cid) = ctx.ensure_class_initialized(cls) else {
                continue;
            };
            for (want, f) in instance.iter().enumerate() {
                // `_fN` asserts nothing about a name and `_vmN` is a slot this
                // VM parks its own value in; neither is a name anybody resolves.
                if f.name.starts_with("_f") || f.name.starts_with("_vm") {
                    continue;
                }
                let got = ctx.resolve_field_index_by_class_id(cid, &f.name);
                if got != Some(want) {
                    shadowed.push(format!("{cls}.{} model={want} mock={got:?}", f.name));
                }
            }
        }

        assert!(
            modelled > 250,
            "only {modelled} of {} harvested names are modelled — the model or \
             the harvest changed shape, and this gate is measuring nothing",
            names.len()
        );
        assert!(
            shadowed.is_empty(),
            "{} modelled field(s) resolve to a slot the VM does not use, because \
             a hand-written mock table answered first. Every one of these is a \
             native tested against the wrong field:\n  {}",
            shadowed.len(),
            shadowed.join("\n  ")
        );
    }

    /// `resolve_field_index` is the THIRD by-name entry point, and until this
    /// change it consulted exactly one hand-written table
    /// (`mock_undertow_exchange_field_slot`) — so it answered `None` for every
    /// other class in the tree.
    ///
    /// Production reaches for it constantly: `java/lang/Enum.name`,
    /// `java/lang/Throwable.detailMessage`,
    /// `java/lang/StackTraceElement.declaringClass`, the whole
    /// `jdk.internal.foreign` segment family. Every branch behind those calls
    /// was unreachable under the mock — the same unfalsifiable-predicate shape
    /// §3 fixed one entry point over, in the entry point §3 did not touch.
    ///
    /// It answers for every class the model models, which is not every class
    /// production asks about: `java.lang.Throwable` has real bytes and no
    /// fabricated model, so `detailMessage` is still `None` here and
    /// `lang_misc`'s three `detailMessage` resolutions are still unfalsifiable
    /// under the mock. That is a gap in the model's coverage, not in the
    /// chain — a test that asserted otherwise was written first and went red.
    #[test]
    fn resolve_field_index_by_name_sees_the_fabricated_model() {
        let ctx = MockNativeContext::new();
        // `java.lang.Enum` models `name` first. Before this it was `None`, so
        // `native_enum_name`'s resolved-slot branch could not be reached.
        assert_eq!(ctx.resolve_field_index("java/lang/Enum", "name"), Some(0));
        // And it is the model, not a hand-written table, that answers. The
        // mock's `mock_undertow_exchange_field_slot` models the REAL Undertow
        // class (~30 fields) and puts `responseHeaders` at 3; the fabricated
        // model is a seven-field stand-in that puts it at 5, and 5 is what the
        // VM resolves. This asserted 3 before the tables were reordered.
        assert_eq!(
            ctx.resolve_field_index("io/undertow/server/HttpServerExchange", "responseHeaders"),
            Some(5)
        );
        // A name the model does NOT declare still falls through to the
        // hand-written table, which is what that table is for.
        assert_eq!(
            ctx.resolve_field_index("io/undertow/server/HttpServerExchange", "requestURI"),
            Some(21)
        );
        // Still falsifiable in the negative direction: a fallback that answered
        // `Some` for everything would be as useless as one answering `None`.
        assert_eq!(
            ctx.resolve_field_index("java/lang/Enum", "nosuchfield"),
            None
        );
    }

    /// All the by-name entry points must answer ONE slot for one name.
    ///
    /// They did not. `java/lang/reflect/Parameter` was special-cased in
    /// `get_field_by_name`/`set_field_by_name` and not in
    /// `resolve_field_index_by_class_id`, so a writer put `name` in slot 0 and
    /// a reader coming the other way looked in slot 1 — the exact
    /// reader/writer split the chain's ordering exists to prevent, sitting in
    /// the mock the whole time.
    #[test]
    fn every_by_name_entry_point_resolves_one_name_to_one_slot() {
        for (class, field) in [
            ("java/lang/reflect/Parameter", "name"),
            ("java/lang/reflect/Field", "clazz"),
            ("java/lang/Enum", "name"),
            ("java/security/ProtectionDomain", "codesource"),
        ] {
            let mut ctx = MockNativeContext::new();
            let cid = ctx
                .ensure_class_initialized(class)
                .expect("mock ensure_class_initialized must succeed");
            let by_id = ctx.resolve_field_index_by_class_id(cid, field);
            let by_name = ctx.resolve_field_index(class, field);
            assert_eq!(
                by_id, by_name,
                "{class}.{field}: resolve_field_index_by_class_id says {by_id:?} \
                 and resolve_field_index says {by_name:?}"
            );
            let slot = by_id.expect("all four pairs above are resolvable");

            // And the write half lands where the read half looks.
            let obj = ctx.alloc_object(cid, 16);
            ctx.set_field_by_name(obj, field, Value::Int(0x5EED));
            assert_eq!(
                ctx.get_field(obj, slot),
                Value::Int(0x5EED),
                "{class}.{field}: set_field_by_name did not write slot {slot}"
            );
            assert_eq!(ctx.get_field_by_name(obj, field), Value::Int(0x5EED));
        }
    }

    #[test]
    fn the_hand_written_namespace_still_wins_for_the_reflect_mirrors() {
        let mut ctx = MockNativeContext::new();
        let cid = ctx
            .ensure_class_initialized("java/lang/reflect/Field")
            .expect("mock ensure_class_initialized must succeed");
        // `mock_jdk_field_slot`'s answer, not the model's (which puts `clazz`
        // at 2, behind `override` and `accessCheckCache`).
        assert_eq!(ctx.resolve_field_index_by_class_id(cid, "clazz"), Some(0));
        assert_eq!(ctx.resolve_field_index_by_class_id(cid, "name"), Some(1));
    }

    /// gc-common w18-d
    /// (`common-w8e-non-strong-hidden-classes-unload-only-with-their-loader`):
    /// the post-GC reconcile reports a non-strong hidden class whose MIRROR
    /// died, while its defining loader is alive -- and a built-in-loader one
    /// with no defining loader at all. A strong class of the same live loader
    /// is not reported. A live non-strong class's instance edge (`loader_pin`)
    /// follows its moved mirror, and a re-registration of its defining loader
    /// cannot turn that edge back into instance -> loader. Its mirror is never
    /// pinned to the loader (`mirror_pin`).
    #[test]
    fn w18d_a_non_strong_hidden_class_unloads_with_its_mirror_not_its_loader() {
        use cratonvm_types::loader_pin::loader_pin_addr_for_vm;
        use cratonvm_types::mirror_pin::{add_mirror_pin, mirrors_for_loader};
        const VM: usize = 0x18D_0B00;
        // Above `DEFINING_LOADER_BITS_CAP`: no key-set bit is touched.
        const STRONG: u32 = 2_018_001;
        const USER_NS: u32 = 2_018_002;
        const BUILTIN_NS: u32 = 2_018_003;
        const LIVE: u32 = 2_018_004;
        let fake = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
        let loader: usize = 0x18D_1000;
        let strong_mirror: usize = 0x18D_2000;
        let user_mirror: usize = 0x18D_2100;
        let builtin_mirror: usize = 0x18D_2200;
        let live_from: usize = 0x18D_2300;
        let live_to: usize = 0x18D_2400;

        register_defining_loader(VM, STRONG, fake(loader));
        register_defining_loader(VM, USER_NS, fake(loader));
        // What `get_or_create_class_mirror` records when each mirror is made,
        // BEFORE the define native registers the non-strong class.
        add_mirror_pin(VM, loader, strong_mirror);
        add_mirror_pin(VM, loader, user_mirror);
        register_non_strong_hidden_mirror(VM, USER_NS, user_mirror, Some(loader));
        register_non_strong_hidden_mirror(VM, BUILTIN_NS, builtin_mirror, None);
        register_non_strong_hidden_mirror(VM, LIVE, live_from, None);

        assert_eq!(loader_pin_addr_for_vm(VM, STRONG), Some(loader));
        assert_eq!(
            loader_pin_addr_for_vm(VM, USER_NS),
            Some(user_mirror),
            "a non-strong class's instances keep its mirror, not its loader"
        );
        assert_eq!(
            mirrors_for_loader(loader),
            Some(vec![strong_mirror]),
            "a live loader must not keep a non-strong class's mirror"
        );

        // The loader and the strong class's mirror survive; both non-strong
        // mirrors die; the live one moves.
        let is_marked = move |addr: usize| addr != user_mirror && addr != builtin_mirror;
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(live_from, live_to);
        let dead = gc_reconcile_defining_loaders(VM, &is_marked, &pointer_map, None);
        assert_eq!(
            dead,
            vec![USER_NS, BUILTIN_NS],
            "both dead mirrors are unload hints; the strong class of the live loader is not"
        );
        assert_eq!(non_strong_hidden_mirror(VM, USER_NS), None);
        assert_eq!(non_strong_hidden_mirror(VM, BUILTIN_NS), None);
        assert!(defining_loader_for(VM, USER_NS).is_none(), "its defining row goes too");
        assert!(
            is_defining_loader_orphaned(VM, USER_NS),
            "fail-closed until the class leaves the store"
        );
        assert_eq!(defining_loader_for(VM, STRONG), Some(fake(loader)));
        assert_eq!(loader_pin_addr_for_vm(VM, STRONG), Some(loader));
        assert_eq!(non_strong_hidden_mirror(VM, LIVE), Some(live_to));
        assert_eq!(
            loader_pin_addr_for_vm(VM, LIVE),
            Some(live_to),
            "the instance edge follows the moved mirror"
        );

        // A later defining-loader registration keeps the mirror edge.
        register_defining_loader(VM, LIVE, fake(loader));
        assert_eq!(loader_pin_addr_for_vm(VM, LIVE), Some(live_to));

        // The unload transaction removed the two dead classes.
        forget_unloaded_classes(VM, &[USER_NS, BUILTIN_NS]);
        assert!(!is_defining_loader_orphaned(VM, USER_NS));
        assert_eq!(loader_pin_addr_for_vm(VM, USER_NS), None);
        assert_eq!(loader_pin_addr_for_vm(VM, BUILTIN_NS), None);
        assert_eq!(non_strong_hidden_mirror(VM, LIVE), Some(live_to), "untouched");

        forget_vm_loader_singletons(VM);
        assert_eq!(non_strong_hidden_mirror(VM, LIVE), None, "teardown drops the rows");
    }

    /// gc-common w18-d: registration writes the mirror's `classLoader` edge
    /// when the define path created the mirror before it recorded the loader
    /// (`define_class_via_full`), so a live instance -- which now keeps only
    /// the mirror -- still keeps the loader. And a context with no VM
    /// (`vm_identity() == 0`, every unit-test mock by default) registers
    /// nothing, so tests sharing that identity cannot see each other's rows.
    #[test]
    fn w18d_registration_gives_the_mirror_its_loader_edge_and_needs_a_vm() {
        use cratonvm_native_api::FieldMetadata;
        use cratonvm_types::ClassId;
        const VM: usize = 0x18D_0C00;
        if !loader_unload_enabled() {
            eprintln!("skipped: CRATONVM_LOADER_UNLOAD=0 unloads nothing");
            return;
        }
        let mut ctx = MockNativeContext::new();
        // The mock mirror is class 0 with two slots; `classLoader` at slot 1
        // stands in for the real `java.lang.Class` layout.
        ctx.set_declared_fields(
            ClassId::new(0),
            vec![FieldMetadata {
                name: "classLoader".to_string(),
                descriptor: "Ljava/lang/ClassLoader;".to_string(),
                access_flags: 0,
                slot_index: 1,
                declaring_class_id: ClassId::new(0),
                is_static: false,
            }],
        );
        let cid = ctx
            .ensure_class_initialized("w18d/Hidden")
            .expect("mock class id");

        assert!(
            !register_non_strong_hidden_class(&mut ctx, cid),
            "vm_identity 0 is no VM"
        );
        assert_eq!(non_strong_hidden_mirror(0, cid.as_u32()), None);

        ctx.set_vm_identity(VM);
        let loader = ctx.fresh_object_ref();
        register_defining_loader(VM, cid.as_u32(), loader);
        assert!(register_non_strong_hidden_class(&mut ctx, cid));
        let mirror = non_strong_hidden_mirror(VM, cid.as_u32()).expect("registered");
        assert_eq!(
            ctx.get_field_by_name(fake_ref(mirror), "classLoader"),
            Value::Object(Some(loader)),
            "the mirror must carry the edge to its defining loader"
        );
        assert_eq!(
            cratonvm_types::loader_pin::loader_pin_addr_for_vm(VM, cid.as_u32()),
            Some(mirror)
        );
        forget_vm_loader_singletons(VM);

        fn fake_ref(addr: usize) -> ObjectRef {
            unsafe { ObjectRef::from_raw(addr as *mut u8) }
        }
    }
}

/// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 20): the closed-`URLClassLoader` set and the single-argument
/// `loadClass` re-entry guard tell two live loaders with one identity hash
/// apart. Mock addresses 4 GiB apart share the mock identity hash (the address
/// truncated to `i32`); never dereferenced.
#[cfg(test)]
mod w29e_loader_collider_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the mock hashes the address.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    struct Teardown(usize);
    impl Drop for Teardown {
        fn drop(&mut self) {
            forget_vm_loader_singletons(self.0);
        }
    }

    /// Closing one of two same-hash loaders closes only it; reopening the
    /// other (never closed) leaves the first closed.
    #[test]
    fn a_same_hash_open_loader_is_not_closed() {
        const VM: usize = 0x29E0_2001;
        let _t = Teardown(VM);
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let (closed, open) = (at(0x1_29E0_2000), at(0x2_29E0_2000));
        assert_eq!(ctx.identity_hash_code(closed), ctx.identity_hash_code(open));

        ucl_mark_closed(&ctx, closed);
        assert!(ucl_is_closed(&ctx, closed));
        assert!(!ucl_is_closed(&ctx, open), "the open collider is not closed");
        ucl_mark_open(&ctx, open);
        assert!(ucl_is_closed(&ctx, closed), "reopening B does not reopen A");
        ucl_mark_closed(&ctx, open);
        ucl_mark_open(&ctx, closed);
        assert!(!ucl_is_closed(&ctx, closed));
        assert!(ucl_is_closed(&ctx, open));
    }

    /// The re-entry guard matches its receiver by pin, not by identity hash:
    /// a same-hash loader is not the nested delegation of the first.
    #[test]
    fn the_single_load_class_guard_tells_same_hash_loaders_apart() {
        let mut ctx = MockNativeContext::new();
        let (outer, collider) = (at(0x1_29E0_2008), at(0x2_29E0_2008));
        assert_eq!(ctx.identity_hash_code(outer), ctx.identity_hash_code(collider));
        let base = ctx.native_pin_count_for_test();

        let outer_pin = single_load_class_guard_enter(&mut ctx, outer).expect("first entry");
        assert_eq!(
            single_load_class_guard_enter(&mut ctx, outer),
            None,
            "the same loader again is the nested super.loadClass"
        );
        let collider_pin = single_load_class_guard_enter(&mut ctx, collider)
            .expect("a same-hash loader is not a re-entry");
        single_load_class_guard_leave(&mut ctx, collider_pin);
        single_load_class_guard_leave(&mut ctx, outer_pin);
        assert_eq!(ctx.native_pin_count_for_test(), base, "every pin released");
        SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT.with(|a| assert!(a.borrow().is_empty()));
    }

    /// gc-common w33-a (`identity-hash-keyed-side-tables-collide-sooner-with-the-8-byte-header`):
    /// with the 8-byte header's 20-bit hash, one bucket routinely holds
    /// several loaders. Three live loaders share one hash: A (closed) dies, B
    /// (closed) moves, C stays open. The reconcile drops only A's row and
    /// re-addresses only B's; C is still open, and a newcomer allocated at A's
    /// vacated address -- same address, so same mock hash -- is not closed.
    #[test]
    fn w33a_a_multi_row_bucket_survives_one_collider_dying_and_another_moving() {
        const VM: usize = 0x33A0_1001;
        let _t = Teardown(VM);
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let (a, b, c) = (at(0x1_33A0_1000), at(0x2_33A0_1000), at(0x3_33A0_1000));
        let b_to: usize = 0x4_33A0_1000;
        let b_now = at(b_to);
        let hash = ctx.identity_hash_code(a);
        for o in [b, c, b_now] {
            assert_eq!(ctx.identity_hash_code(o), hash, "premise: one bucket");
        }

        ucl_mark_closed(&ctx, a);
        ucl_mark_closed(&ctx, b);
        assert!(ucl_is_closed(&ctx, a) && ucl_is_closed(&ctx, b));
        assert!(!ucl_is_closed(&ctx, c), "the open collider is not closed");

        let (a_addr, b_from) = (a.as_ptr() as usize, b.as_ptr() as usize);
        let is_marked = move |addr: usize| addr != a_addr;
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(b_from, b_to);
        gc_reconcile_defining_loaders(VM, &is_marked, &pointer_map, None);

        assert!(ucl_is_closed(&ctx, b_now), "the moved collider stays closed");
        assert!(!ucl_is_closed(&ctx, b), "its vacated address is not closed");
        assert!(!ucl_is_closed(&ctx, c), "the open collider is still open");
        let newcomer = at(a_addr);
        assert!(
            !ucl_is_closed(&ctx, newcomer),
            "a loader at the dead collider's address is not closed"
        );
        let rows: Vec<usize> = closed_url_classloader_ids()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(VM, hash))
            .cloned()
            .unwrap_or_default();
        assert_eq!(rows, vec![b_to], "one row left in the bucket, at B's new address");
    }
}

#[cfg(test)]
mod r13w7_predicates_proxy_name_tests {
    use super::is_generated_proxy_name;

    /// A package-private interface's proxy (`p/$Proxy3`) is a generated proxy
    /// name too; a user nested class whose name merely contains `$Proxy` is not.
    #[test]
    fn package_private_proxy_is_recognised() {
        if !crate::reflect_annotations::proxy_user_subclass_is_ordinary() {
            return; // CRATONVM_PROXY_USER_SUBCLASS_ORDINARY=0 in this environment
        }
        for name in ["jdk/proxy1/$Proxy12", "com/sun/proxy/$Proxy0", "p/$Proxy3", "$Proxy4"] {
            assert!(is_generated_proxy_name(name), "{name}");
        }
        for name in [
            "jdk/proxy1/Outer$ProxyConfig",
            "p/Outer$Proxy3",
            "java/lang/reflect/Proxy$Instance",
            "jdk.proxy1.$Proxy12",
        ] {
            assert!(!is_generated_proxy_name(name), "{name}");
        }
    }
}
