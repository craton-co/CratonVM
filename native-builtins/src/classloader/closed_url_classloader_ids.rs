#[allow(unused_imports)]
use super::*;


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
pub const LOCAL_URL_CLASS_PATH_CACHE_LIMIT: usize = 64;

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
//
// Interpreter round i5 wave 102, lane L5
// (`interpreter-i101-L5-remaining-loader-family-process-globals-FIXED-20261016.md`): the table is
// the VM's own now (`LoaderIdentityTables::closed_url_loaders`), keyed by the
// identity hash alone; it was a process map keyed by `(VM identity, hash)`.
// addr-keyed: not a heap address -- the identity hash; the loader ADDRESSES in the values are PRUNED + REMAPPED by gc_reconcile_defining_loaders.
pub fn closed_url_classloader_ids(
    tables: &LoaderIdentityTables,
) -> &Mutex<std::collections::HashMap<i32, Vec<usize>>> {
    &tables.closed_url_loaders
}

/// `loader` must be CURRENT (it is matched by address).
pub fn ucl_is_closed(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    let key = ctx.identity_hash_code(loader);
    let addr = loader.as_ptr() as usize;
    closed_url_classloader_ids(loader_tables(ctx))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .is_some_and(|rows| rows.contains(&addr))
}

pub fn ucl_mark_open(ctx: &dyn NativeContext, loader: ObjectRef) {
    let key = ctx.identity_hash_code(loader);
    let addr = loader.as_ptr() as usize;
    let mut set = closed_url_classloader_ids(loader_tables(ctx))
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
pub fn ucl_mark_closed(ctx: &dyn NativeContext, loader: ObjectRef) {
    let key = ctx.identity_hash_code(loader);
    let addr = loader.as_ptr() as usize;
    let mut set = closed_url_classloader_ids(loader_tables(ctx))
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
///
/// Interpreter round i5 wave 102, lane L5
/// (`interpreter-i101-L5-remaining-loader-family-process-globals-FIXED-20261016.md`): no longer
/// a row of the process map `LOADER_SINGLETONS`; each VM's
/// [`LoaderIdentityTables`] holds its own (`singletons`), dropped with the VM.
#[derive(Default)]
pub struct VmLoaderSingletons {
    pub platform: Option<ObjectRef>,
    pub app: Option<ObjectRef>,
    /// SBR2-LOADERS: `platform`/`app` are the JDK's `ClassLoaders` objects.
    pub adopted: bool,
    /// Slash-form packages of the modules the JDK assigns to the platform loader.
    pub platform_packages: std::collections::HashSet<String>,
    /// Slash-form packages of the run-time image's modules the JDK assigns to
    /// the APPLICATION loader (`jdk.compiler`, `jdk.jshell`, ...): neither boot
    /// nor platform in `ModuleLoaderMap`. Interpreter round i5 wave 100, lane L3.
    pub app_image_packages: std::collections::HashSet<String>,
    /// `--real-jdk`: the one `ProtectionDomain` per class-path code base of
    /// the application loader, as `SecureClassLoader`'s `pdcache` keeps one
    /// per `CodeSource`. Built for the load-time transformers before the class
    /// exists and answered again by its `getProtectionDomain()`
    /// (`lang_class::app_class_path_domain`). Rooted with the loader it names,
    /// which never unloads; bounded by the class path's entries.
    pub app_code_source_domains: std::collections::HashMap<String, ObjectRef>,
}

/// Run `f` with this VM's singleton row locked.
///
/// The lock is never held across a Java allocation: `get_or_create_*_loader`
/// allocates first and publishes afterwards, exactly as
/// `security_manager::with_security_state` requires, because
/// [`gc_scan_loader_singleton_roots`] takes this same lock at a safepoint.
pub fn with_loader_singletons<R>(
    tables: &LoaderIdentityTables,
    f: impl FnOnce(&mut VmLoaderSingletons) -> R,
) -> R {
    let mut guard = tables.singletons.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

pub fn platform_loader_of(tables: &LoaderIdentityTables) -> Option<ObjectRef> {
    with_loader_singletons(tables, |row| row.platform)
}

pub fn set_platform_loader(tables: &LoaderIdentityTables, value: Option<ObjectRef>) {
    with_loader_singletons(tables, |row| row.platform = value);
}

pub fn app_loader_of(tables: &LoaderIdentityTables) -> Option<ObjectRef> {
    with_loader_singletons(tables, |row| row.app)
}

pub fn set_app_loader(tables: &LoaderIdentityTables, value: Option<ObjectRef>) {
    with_loader_singletons(tables, |row| {
        row.app = value;
        // Each cached domain names the loader it was built for.
        row.app_code_source_domains.clear();
    });
}

/// The application loader's cached `ProtectionDomain` for `code_base`, if one
/// was built (see `VmLoaderSingletons::app_code_source_domains`).
pub fn app_code_source_domain(
    tables: &LoaderIdentityTables,
    code_base: &str,
) -> Option<ObjectRef> {
    with_loader_singletons(tables, |row| row.app_code_source_domains.get(code_base).copied())
}

/// Publish `domain` as the application loader's `ProtectionDomain` for
/// `code_base` and answer the published one: a domain another thread published
/// first wins, so every caller hands out the same object.
pub fn publish_app_code_source_domain(
    tables: &LoaderIdentityTables,
    code_base: &str,
    domain: ObjectRef,
) -> ObjectRef {
    with_loader_singletons(tables, |row| {
        *row.app_code_source_domains
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
pub fn builtin_loader_ordinal(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<u32> {
    if platform_loader_of(loader_tables(ctx)).is_some_and(|p| p.as_ptr() == this.as_ptr())
        || ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader")
    {
        return Some(cratonvm_types::ClassLoaderId::NATIVE_EXTENSION);
    }
    if app_loader_of(loader_tables(ctx)).is_some_and(|p| p.as_ptr() == this.as_ptr())
        || ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$AppClassLoader")
    {
        return Some(cratonvm_types::ClassLoaderId::NATIVE_APPLICATION);
    }
    None
}

/// The class the built-in loader `this` (application or platform), or its
/// built-in parent, itself defined under `internal_name`: the answer
/// `findLoadedClass` owes when the name has no unique global answer because a
/// URL or user loader defined a class of the same name too. `class_id_by_name`
/// answers `None` for such an ambiguous name, and the application loader then
/// missed its OWN class and defined the name a second time, a duplicate
/// definition (interpreter round i2 wave 61, lane L5; probe
/// `L5W61BootSearchSideDefine`, row `app-child`). Cold: asked only when the
/// global probe missed.
///
/// This is also the whole of `docs/feature-designs/native-dispatch-architecture.md`
/// (JIT round 18): a user loader defines `Outer$Nested` first, the application
/// loader then loads its own copy, and every later `findLoadedClass` of the
/// application loader must report that copy (`R18MhresidDupe`,
/// `R19FindLoadedBehindUser`). The answer still passes the built-in branch's
/// ancestor and defining-loader visibility checks in
/// [`find_loaded_class_for_loader_inner`]. Gated by
/// [`find_loaded_builtin_behind_user_enabled`] (`=0` restores the duplicate
/// definition, the positive control).
#[cold]
#[inline(never)]
pub fn builtin_own_definition(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
) -> Option<cratonvm_types::ClassId> {
    let ordinal = builtin_loader_ordinal(ctx, this)?;
    (cratonvm_types::ClassLoaderId::NATIVE_EXTENSION..=ordinal)
        .rev()
        .find_map(|o| ctx.class_id_defined_by_loader_exact(internal_name, o))
}

/// zt t21/df9 (define page §9): the application loader `this` asked
/// `findLoadedClass(internal_name)` when the global lookup answered `cid`, a
/// class the PLATFORM loader object defined from bytes (an `Extension`-tagged
/// class with a defining-loader row): the application loader's own
/// definition of the name, when it has one, is the answer. HotSpot reads the
/// asking loader's dictionary, which holds its own class, so
/// `BuiltinClassLoader.loadClassOrNull` (`findLoadedClass` first) returns the
/// application loader's copy, never its parent's. `None`: not the
/// application loader, `cid` not Java-defined, no own copy (the existing
/// ancestor rules then answer), or `CRATONVM_DEFINE_PLATFORM_LOADER_OWN_NAMESPACE=0`.
/// Cold: only for an `Extension`-tagged candidate (the image's platform
/// classes carry the boot tag or no defining-loader row).
#[cold]
#[inline(never)]
pub fn builtin_own_definition_first(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
    cid: cratonvm_types::ClassId,
) -> Option<cratonvm_types::ClassId> {
    defining_loader_for(loader_tables(ctx), cid.as_u32())?;
    if !crate::lang_system::define_platform_loader_own_namespace()
        || builtin_loader_ordinal(ctx, this)? != cratonvm_types::ClassLoaderId::NATIVE_APPLICATION
    {
        return None;
    }
    let own = ctx.class_id_defined_by_loader_exact(
        internal_name,
        cratonvm_types::ClassLoaderId::NATIVE_APPLICATION,
    )?;
    if own == cid {
        return None;
    }
    match defining_loader_for(loader_tables(ctx), own.as_u32()) {
        Some(def) if def.as_ptr() != this.as_ptr() => None,
        _ => Some(own),
    }
}

/// zt t22/df10 (define page §9c): the answer `found` of the Java-facing
/// `findLoadedClass` natives (`ClassLoader.findLoadedClass` and
/// `findLoadedClass0`, both modes) for the loader `this`, with a class an
/// ANCESTOR of a built-in loader defined withheld unless the loader
/// initiated it. HotSpot's `JVM_FindLoadedClass` reads the asking loader's
/// dictionary: the classes it defined, and those the VM resolved THROUGH it
/// (`SystemDictionary::load_instance_class` files the answer of a loader
/// that is not the defining one: a constant-pool resolution by one of its
/// classes, `Class.forName` with it, a supertype of a class it defined). A
/// Java `loadClass` delegation records nothing: after only
/// `Class.forName("java.sql.Timestamp", false, platformLoader)`, the
/// application loader's `findLoadedClass` is null on HotSpot, while
/// [`find_loaded_class_for_loader`] answers any class of the loader's
/// ancestor chain. That function stays as it is (the VM's own fast paths ask
/// it for "visible to the loader"); only the Java-visible answer narrows.
/// A user-defined loader, an own class, a generated proxy and a class whose
/// defining loader cannot be told yet (the module tables not read) keep the
/// answer; `CRATONVM_FIND_LOADED_BUILTIN_INITIATED_ONLY=0` keeps every one.
/// A withheld answer is not a miss of the class: `BuiltinClassLoader.
/// loadClassOrNull` delegates to the loader that defines it, as on HotSpot.
pub fn find_loaded_answer_for_java(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
    found: Option<ObjectRef>,
) -> Option<ObjectRef> {
    let mirror = found?;
    if !ctx.builtin_find_loaded_initiated_only() {
        return Some(mirror);
    }
    builtin_answer_initiated(ctx, this, internal_name, mirror).then_some(mirror)
}

/// The most direct subtypes [`builtin_answer_initiated`] reads for one
/// answer (a JDK root has thousands; the loader's own subtypes of such a
/// root are almost always recorded by resolution first).
pub const BUILTIN_SUPERTYPE_SCAN_LIMIT: usize = 4096;

/// [`find_loaded_answer_for_java`]'s rule: may the built-in loader `this`
/// answer `mirror` (a class `find_loaded_class_for_loader` found for
/// `internal_name`)? `true` unless it is an ancestor's class the loader
/// never initiated.
#[inline(never)]
pub fn builtin_answer_initiated(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal_name: &str,
    mirror: ObjectRef,
) -> bool {
    let Some(cid) = ctx.class_id_from_mirror(mirror) else {
        return true;
    };
    let tag = ctx.loader_id_of_class(cid);
    // A user loader's class (the built-in branch never answers one to a
    // built-in loader), and a generated proxy (`proxy_hidden_from`'s own
    // rule decided it).
    if !(0..=2).contains(&tag) || is_generated_proxy_name(internal_name) {
        return true;
    }
    let Some(this_ordinal) = builtin_loader_ordinal(ctx, this) else {
        return true;
    };
    // The class's DEFINING loader as Java sees it (`Class.getClassLoader`):
    // a Java define names its loader object; an image class of a platform
    // (or application) module carries the boot tag but the JDK's
    // `ModuleLoaderMap` gives it to that loader; any other class its tag.
    let definer = match defining_loader_for(loader_tables(ctx), cid.as_u32()) {
        Some(def) if def.as_ptr() == this.as_ptr() => return true,
        Some(def) if is_platform_class_loader(ctx, def) => {
            cratonvm_types::ClassLoaderId::NATIVE_EXTENSION
        }
        Some(def) if is_application_class_loader(ctx, def) => {
            cratonvm_types::ClassLoaderId::NATIVE_APPLICATION
        }
        // A user loader's definition filed in a built-in namespace: the
        // existing visibility rules decided it.
        Some(_) => return true,
        None if tag == 0 => {
            if loader_tables(ctx).module_sets.lock().is_none() {
                // Early boot: which loader the image gives the class is not
                // known yet (and asking must not initialise
                // `ModuleLoaderMap`), so the ancestor answer stands.
                return true;
            }
            image_class_builtin_loader_id(ctx, internal_name).map_or(0, |id| id as u32)
        }
        None => tag as u32,
    };
    if definer == this_ordinal {
        return true;
    }
    // An ancestor's class: answered from the loader's dictionary rows only.
    if ctx.builtin_loader_initiated(this_ordinal, cid) {
        ctx.note_builtin_find_loaded_initiated(1);
        return true;
    }
    // JVMS §5.3.5 step 3: the loader resolved `cid` as the direct supertype
    // of a class it defined. Read off the store's subtype indices (no row is
    // written at define); a hit is filed as the loader's row, so the next
    // answer is the read above.
    if builtin_defined_a_direct_subtype(ctx, this_ordinal, cid) {
        ctx.note_jdk_class_initiated(this_ordinal, cid);
        ctx.note_builtin_find_loaded_initiated(2);
        return true;
    }
    ctx.note_builtin_find_loaded_initiated(0);
    false
}

/// Did the built-in loader `this_ordinal` define a direct subtype of `cid`?
/// The application loader's classes carry its tag; the platform loader's
/// are its Java-defined classes (`Extension` tag) and the boot-tagged image
/// classes of its modules ([`image_class_builtin_loader_id`]), and since zt
/// df4 the application loader's the boot-tagged classes of the JDK's tool
/// modules too. zt df4 §9c: the image classes are read exactly, every direct
/// subtype (`NativeContext::builtin_image_class_has_direct_supertype`); the
/// bounded read below is the rule-off path.
#[cold]
#[inline(never)]
pub fn builtin_defined_a_direct_subtype(
    ctx: &mut dyn NativeContext,
    this_ordinal: u32,
    cid: cratonvm_types::ClassId,
) -> bool {
    if let Some(image) = ctx.builtin_image_class_has_direct_supertype(cid, this_ordinal) {
        // Any subtype carrying the loader's own tag answers; the first is
        // enough.
        return image || !ctx.direct_subtypes_of_loaders(cid, &[this_ordinal], 1).is_empty();
    }
    let platform = this_ordinal == cratonvm_types::ClassLoaderId::NATIVE_EXTENSION;
    let loaders: &[u32] = if platform {
        &[
            cratonvm_types::ClassLoaderId::NATIVE_EXTENSION,
            cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP,
        ]
    } else {
        &[this_ordinal]
    };
    let subtypes = ctx.direct_subtypes_of_loaders(cid, loaders, BUILTIN_SUPERTYPE_SCAN_LIMIT);
    subtypes.into_iter().any(|sub| {
        if ctx.loader_id_of_class(sub) == this_ordinal as i32 {
            return true;
        }
        platform
            && ctx.class_name_of_id(sub).is_some_and(|name| {
                !name.starts_with('[')
                    && image_class_builtin_loader_id(ctx, &name) == Some(this_ordinal as i32)
            })
    })
}

/// Temporary debug-only accessor (CRATONVM_DBG_OBSREG investigation).
/// zt df4 §9c: also the platform loader object a built-in duplicate-define
/// refusal names (`classloader_real::initiated_duplicate_definition_message`).
pub fn platform_loader_dbg(tables: &LoaderIdentityTables) -> Option<ObjectRef> {
    platform_loader_of(tables)
}

/// zt df4 §9c: the application loader object, for the same message.
pub fn app_loader_object_in(tables: &LoaderIdentityTables) -> Option<ObjectRef> {
    app_loader_of(tables)
}

/// Per-VM teardown: drop the built-in loader singletons held for
/// `vm_identity`.
///
/// Called from `release_vm_native_state` when the last `Arc<SharedVm>` goes
/// away. Without it the row — and the raw heap addresses in it — outlives the
/// heap that produced them, and a later VM that reused the identity would
/// inherit a dead `ClassLoader`.
pub fn forget_vm_loader_singletons(vm_identity: usize) {
    // Interpreter round i5 wave 101, lane L5: the defining-loader, orphan,
    // non-strong hidden, loader-namespace and loader-meta rows are the VM's
    // own `LoaderIdentityTables` now and die with its `SharedVm`. Wave 102
    // (lane L5) moved the rest there too: the built-in loader singletons, the
    // closed-URLClassLoader set (gc-common w9-a's reason to drop it here: its
    // raw heap addresses mean nothing once this heap is gone), and the JDK
    // boot/platform module tables, the platform-package memo and the
    // modules-defined-to-loaders bit (gc-common w10-f: read out of THIS VM's
    // image). Only a context without a VM (a unit test's mock) keeps a
    // detached instance per identity; empty that one.
    forget_detached_loader_tables(vm_identity);
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
    // (Interpreter round i5 wave 101, lane L5: the tables the next two notes
    // name are now fields of each VM's own `LoaderIdentityTables`, so there
    // is no process table left to wipe. The notes are kept for the history.)
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
    // (only `ClassPath`s keyed by path lists), it is bounded by
    // `LOCAL_URL_CLASS_PATH_CACHE_LIMIT`, and since wave 65 it is the VM's
    // own (`ArchiveBytesCaches::url_class_path`), dropped with its registry. The cache that
    // used to be cleared here had no reader left (gc-common w9-a).
}

/// GC root scan for the singleton built-in class loaders.
///
/// The app + platform `ClassLoader` synthetics live ONLY in the
/// per-VM singleton row (`LoaderIdentityTables::singletons`) (not a Java field or VM root table), so
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
pub fn gc_scan_loader_singleton_roots(tables: &LoaderIdentityTables, out: &mut Vec<ObjectRef>) {
    with_loader_singletons(tables, |row| {
        out.extend(row.app);
        out.extend(row.platform);
        out.extend(row.app_code_source_domains.values().copied());
    });
    // Defining-loader side-table values are live ClassLoader objects reachable
    // only from this map. Legacy behavior roots them all (which is why a
    // user/isolated loader could never be collected — HIB-CV-24 Manifestation B).
    // With `CRATONVM_LOADER_UNLOAD` ON (default) we DON'T root them, so a loader
    // the application no longer references becomes collectable; the now-stale
    // entry is pruned post-GC by `gc_reconcile_defining_loaders`. App/platform
    // singletons stay rooted above, so built-in loaders are unaffected.
    // zt t21/tg10: a VM made with `-XX:-ClassUnloading` roots them too, as
    // HotSpot makes every class loader's data a strong root then.
    if !loader_unload_enabled_for(tables) {
        for o in tables
            .defining
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rows
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
pub fn gc_update_loader_singleton_refs(
    tables: &LoaderIdentityTables,
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
    with_loader_singletons(tables, |row| {
        remap(&mut row.app);
        remap(&mut row.platform);
        for domain in row.app_code_source_domains.values_mut() {
            let mut slot = Some(*domain);
            remap(&mut slot);
            *domain = slot.unwrap_or(*domain);
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
    tables: &LoaderIdentityTables,
    is_marked: &dyn Fn(usize) -> bool,
    pointer_map: &cratonvm_types::PointerMap,
    // addr-keyed: a parameter -- the caller's pre-relocation verdicts for this pause only.
    exact_results: Option<&std::collections::HashMap<usize, bool>>,
) -> Vec<u32> {
    let vm_identity = tables.vm_identity;
    // i4 wave 84, lane L5: every address-keyed loader memo filed before this
    // pass is void from here (`CRATONVM_LOADER_NAMESPACE_INDEX`; see
    // `LoaderIdentityTables::reconcile_generation`). Bumped first: the pass
    // runs stop-the-world for this VM, so no filing of this VM straddles it.
    tables.reconcile_generation.fetch_add(1, Ordering::AcqRel);
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
        reconcile_non_strong_hidden(tables, is_marked, pointer_map, exact_results);

    // Phase 1: this VM's defining-loader rows.
    let ours: Vec<(u32, usize)> = tables
        .defining
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rows
        .iter()
        .map(|(&cid, obj_ref)| (cid, obj_ref.as_ptr() as usize))
        .collect();
    // Phase 2: verdicts, no lock held.
    let mut dead_rows: std::collections::HashSet<(u32, usize)> = std::collections::HashSet::new();
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
    let mut map = tables.defining.lock().unwrap_or_else(|e| e.into_inner());
    if !dead_rows.is_empty() || !hidden_dead.is_empty() {
        let mut orphans = tables.orphaned.lock().unwrap_or_else(|e| e.into_inner());
        map.rows.retain(|&class_id, obj_ref| {
            // Every row is this VM's own (the table is the VM's, interpreter
            // round i5 wave 101, lane L5).
            let addr = obj_ref.as_ptr() as usize;
            if !(dead_rows.contains(&(class_id, addr)) || hidden_dead.contains(&class_id)) {
                return true;
            }
            // Loader unreachable and collected this cycle — drop the stale
            // entry. (No deref of `obj_ref`; the memory may already be
            // freed/reused.) Record the class as permanently orphaned: dropping
            // the entry alone makes `defining_loader_for` indistinguishable
            // from "never restricted", which would make this class incorrectly
            // visible to every other loader from now on (see
            // `is_defining_loader_orphaned` doc comment).
            orphans.insert(class_id);
            dead_class_ids.push(class_id);
            false
        });
    }
    // Survivors: remap the ones that relocated (moving collection).
    if !pointer_map.is_empty() {
        for obj_ref in map.rows.values_mut() {
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
        map.reindex(pointer_map);
    }
    // The lock-free key-set mirror is this VM's own (interpreter round i5
    // wave 101, lane L5; it was keyed by `class_id` alone across every VM of
    // the process, so a bit stayed set while ANY VM held a row for the id).
    // Clear the bit of every id whose row is gone (the `contains_key` check
    // is only a guard: the `retain` above drops every dead id's row).
    for &id in &dead_class_ids {
        if !map.rows.contains_key(&id) {
            tables.clear_defining_loader_bit(id);
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
        .filter(|(cid, _)| !hidden_ids.contains(*cid))
        .map(|(&cid, obj_ref)| (cid, obj_ref.as_ptr() as usize))
        .collect();
    pins.extend(hidden_live.iter().copied());
    cratonvm_types::loader_pin::replace_loader_pins(vm_identity, &pins);
    drop(map);
    // JIT round 23 wave 6: a class whose loader died leaves the
    // `receiver_overrides_load_class` memo with its namespace (after the
    // defining table's lock is released, as the `loader_meta` pass below).
    tables.load_class_override_forget(&dead_class_ids);

    // Same treatment for the loader-namespace side-table (object-keyed): drop
    // entries whose loader was collected this cycle, remap survivors that
    // moved. A pruned dead loader's address can then be reused by a NEW
    // loader without inheriting the dead namespace. Same three phases.
    //
    // Only THIS VM's rows are collected, judged and remapped (gc-common w9-a;
    // since interpreter round i5 wave 101, lane L5, the table itself is this
    // VM's). This table and the loader-meta table below once carried no VM,
    // so this VM's `is_marked` judged every other live VM's rows too — and
    // the production predicate answers "dead" for every address outside this
    // VM's heap, so each collection deleted every other VM's rows.
    let ns_addrs: Vec<usize> = tables
        .loader_namespaces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(obj_ref, _)| obj_ref.as_ptr() as usize)
        .collect();
    // i4 wave 84, lane L5: the three object-keyed loader tables below judge
    // each row the way the defining-loader pass above does -- the pre-slide
    // exact verdict first (ZGC), `is_marked` otherwise. They used
    // `is_marked` alone, which on ZGC is the post-slide registry probe: a
    // survivor slid into a dead loader's span this cycle kept the dead row
    // (HIB-CV-24's ZGC manifestation, in the namespace, meta and closed-URL
    // tables). When the slid survivor is itself a moved loader, its own row
    // is remapped onto the same address and the two rows alias: the live
    // loader can answer the dead one's namespace id (first match wins).
    // `watched_defining_loader_addrs` arms these tables' addresses too.
    let alive = |addr: usize| -> bool {
        exact_results
            .and_then(|m| m.get(&addr).copied())
            .unwrap_or_else(|| is_marked(addr))
    };
    // addr-keyed: local to this reconcile (one pause).
    let ns_dead: std::collections::HashSet<usize> = ns_addrs
        .into_iter()
        .filter(|addr| !alive(*addr))
        .collect();
    tables
        .loader_namespaces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain_rows_mut(|(obj_ref, _)| {
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
    // This VM's table, like the namespace table above.
    let addrs: Vec<usize> = {
        let table = tables.loader_meta.lock();
        table.iter().map(|(o, _)| o.as_ptr() as usize).collect()
    };
    // addr-keyed: local to this reconcile (one pause).
    let dead: std::collections::HashSet<usize> =
        addrs.into_iter().filter(|addr| !alive(*addr)).collect();
    tables.loader_meta.lock().retain_rows_mut(|(obj_ref, _)| {
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
    // (Wave 102, lane L5: the set is this VM's own, so no VM filter.)
    let closed_addrs: Vec<usize> = closed_url_classloader_ids(tables)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .flat_map(|rows| rows.iter().copied())
        .collect();
    // addr-keyed: local to this reconcile (one pause).
    let closed_dead: std::collections::HashSet<usize> = closed_addrs
        .into_iter()
        .filter(|addr| !alive(*addr))
        .collect();
    closed_url_classloader_ids(tables)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, rows| {
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
pub fn forget_unloaded_classes(tables: &LoaderIdentityTables, class_ids: &[u32]) {
    if class_ids.is_empty() {
        return;
    }
    let vm_identity = tables.vm_identity;
    let ids: std::collections::HashSet<u32> = class_ids.iter().copied().collect();
    tables
        .orphaned
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|id| !ids.contains(id));
    // gc-common w18-d: an unloaded non-strong hidden class's row (and its
    // mirror's `mirror_pin` exclusion) goes with it. Its `loader_pin` row is
    // removed just below with every other unloaded class's.
    forget_non_strong_hidden(tables, &ids);
    // zt t11/df3: an orphan of a failed `addClass` upcall that unloads before
    // any retry adopted it (its loader died) leaves its row too. Nothing else
    // removed it: the row outlived the class for the life of the VM, and a
    // non-empty map sends every later duplicate arm and `Proxy` cache hit
    // past the empty-map fast path. Empty (no work beyond the lock) unless an
    // upcall failed.
    {
        let mut orphans = tables.add_class_orphans.lock();
        if !orphans.is_empty() {
            orphans.retain(|id, _| !ids.contains(id));
        }
    }
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
pub fn attach_class_data(ctx: &mut dyn NativeContext, mirror: ObjectRef, data: ObjectRef) {
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
///
/// Interpreter round i5 wave 101, lane L5: the table is now a field of the
/// VM's own [`LoaderIdentityTables`], so the key is the bare `class_id` and a
/// foreign VM's row cannot exist here at all.
///
/// One VM's loader-identity tables: which loader OBJECT defined each class,
/// which classes a loader object defined, which classes lost their loader,
/// which hidden classes live by their mirror, each loader object's
/// bookkeeping ([`LoaderMeta`]) and namespace id, plus the lock-free screens
/// over them.
///
/// Interpreter round i5 wave 101, lane L5
/// (`docs/feature-designs/native-dispatch-architecture.md`):
/// these were nine process globals (`defining_loader_store`,
/// `orphaned_defining_loader_classes`, `ANY_DEFINING_LOADER_REGISTERED`,
/// `defining_loader_bits`, `non_strong_hidden_store`, `ANY_NON_STRONG_HIDDEN`,
/// `loader_meta_store`, `loader_namespace_id_store`,
/// `LOADER_RECONCILE_GENERATION`), every row qualified by the VM identity.
/// Every VM of a process took the same mutexes on every define, the latches
/// and the per-class-id bitset never went back for ANY VM once one VM defined
/// through a user loader, and each table needed a teardown hook. AGENTS.md:
/// no process globals for compatibility state. One instance per VM now:
/// `SharedVm::loader_identity` owns it (dropped with the VM), the VM's
/// `NativeContext` hands it to the natives
/// (`NativeSystemAccess::loader_identity_tables`, reached through
/// [`loader_tables`]), and the VM's own callers pass `&shared.loader_identity`.
/// A context without a VM (a unit test's mock) gets a detached instance per
/// VM identity ([`detached_loader_tables`]).
pub struct LoaderIdentityTables {
    /// The owning VM, for the GC marker's per-VM pin registries
    /// (`loader_pin`, `mirror_pin`) and the loader singletons, which are still
    /// keyed by identity.
    pub vm_identity: usize,
    /// The defining-loader rows and their loader -> classes index. Every
    /// existing `lock()` is an exclusive (write) acquisition, as the `Mutex`
    /// this was; only the read-only lookups take the shared side, and only
    /// with `CRATONVM_DEFINING_LOADER_SHARED_READS` ([`DefiningLock`]).
    pub defining: DefiningLock,
    /// `CRATONVM_DEFINING_LOADER_SHARED_READS`, read once for this VM
    /// ([`defining_shared_reads`]; interpreter round i7 wave 143, lane L5).
    pub defining_shared_reads: OnceLock<bool>,
    /// Class ids whose defining loader was confirmed dead
    /// ([`is_defining_loader_orphaned`]).
    pub orphaned: Mutex<std::collections::HashSet<u32>>,
    /// Has this VM ever registered a defining loader? Only goes false -> true.
    pub any_defining: AtomicBool,
    /// Lock-free mirror of `defining`'s key set (see
    /// [`class_may_have_defining_loader`]); allocated on the first
    /// registration.
    pub defining_bits: OnceLock<Box<[AtomicU64]>>,
    /// `class_id -> mirror address` of each live non-strong hidden class.
    pub non_strong_hidden: OrderedPlMutex<std::collections::HashMap<u32, usize>>,
    /// Has this VM ever registered a non-strong hidden class?
    pub any_non_strong_hidden: AtomicBool,
    /// Loader object -> [`LoaderMeta`] (a `Vec` of rows, with the opt-in
    /// position memo of [`MemoRows`]).
    pub loader_meta: OrderedPlMutex<MemoRows<LoaderMeta>>,
    /// Loader object -> namespace id (`loader_namespace_id_at`); a `Vec` of
    /// rows with the opt-in position memo of [`MemoRows`].
    pub loader_namespaces: Mutex<MemoRows<u32>>,
    /// Bumped by every [`gc_reconcile_defining_loaders`] of this VM (see
    /// [`loader_address_epoch`]).
    pub reconcile_generation: AtomicU64,
    // Interpreter round i5 wave 102, lane L5
    // (`interpreter-i101-L5-remaining-loader-family-process-globals-FIXED-20261016.md`): the
    // loader family's remaining process tables, each keyed by VM identity
    // until then, live here too.
    /// The built-in loader singletons ([`VmLoaderSingletons`]; was the
    /// process map `LOADER_SINGLETONS`). Same raw lock as before; GC-rooted
    /// by [`gc_scan_loader_singleton_roots`].
    pub singletons: Mutex<VmLoaderSingletons>,
    /// `URLClassLoader.close()` state: identity hash -> the closed loaders'
    /// addresses (see [`closed_url_classloader_ids`]; was keyed by `(VM,
    /// hash)` in one process map). The addresses are PRUNED + REMAPPED by
    /// [`gc_reconcile_defining_loaders`].
    // addr-keyed: not a heap address -- the identity hash; the address VALUES are pruned + remapped by gc_reconcile_defining_loaders.
    pub closed_url_loaders: Mutex<std::collections::HashMap<i32, Vec<usize>>>,
    /// [`jdk_builtin_module_sets`]' memo (was `builtin_module_sets_cache`).
    pub module_sets: OrderedPlMutex<Option<Arc<BuiltinModuleSets>>>,
    /// [`package_builtin_kind`]'s memo (was `platform_package_memo`).
    pub builtin_packages: OrderedPlMutex<std::collections::HashMap<String, BuiltinLoaderKind>>,
    /// This VM's boot modules are defined to the built-in loaders
    /// ([`set_modules_defined_to_loaders`]; was `modules_defined_to_loaders`).
    pub modules_defined_to_loaders: AtomicBool,
    /// The VM's user-loader delegation parents: the SAME `Arc` its
    /// `ClassManager` reads during resolution
    /// ([`Self::with_user_loader_parents`]); written by
    /// [`record_parent_link`] and the namespace scans. The process global
    /// `cratonvm_classloading::loaders::USER_LOADER_PARENTS` until
    /// interpreter round i5 wave 103, lane L5.
    pub user_loader_parents: Arc<cratonvm_classloading::UserLoaderParents>,
    /// The defined-`Package` memo behind `Class.getPackage()` and
    /// `ClassLoader.getDefinedPackage(s)` (`lang_class::DefinedPackageMemo`).
    /// The process map `lang_class::defined_package_memo`, keyed by VM
    /// identity, until interpreter round i5 wave 105, lane L5.
    pub defined_packages: crate::lang_class::DefinedPackageMemo,
    /// zt t9/df: class id -> digest of the defining bytes, for each class a
    /// define door published and whose `ClassLoader.addClass` upcall then
    /// failed ([`note_add_class_orphan_on_failure`]); a retry of the same
    /// bytes by the same loader adopts it ([`adopt_add_class_orphan`]).
    /// Written only on ZGC under `CRATONVM_ZGC_DEFINE_ADD_CLASS_RETRY`. Class
    /// ids are never reused, so a row of a class that later unloads is never
    /// matched again (a few bytes per refused upcall).
    pub add_class_orphans: OrderedPlMutex<std::collections::HashMap<u32, u64>>,
    /// zt t18/df7: the VM's native-access state (`--enable-native-access`,
    /// `--illegal-native-access`, the restricted-method warn-once latch;
    /// `panama::native_access`), process globals before. Installed from
    /// `VmConfig::native_access` when the VM is made.
    pub native_access: crate::panama::NativeAccessState,
    /// zt t21/tg10: does this VM unload classes? `false` under HotSpot's
    /// `-XX:-ClassUnloading` / `-Xnoclassgc` (`VmConfig::class_unloading`),
    /// installed when the VM is made, before its first define. HotSpot then
    /// treats every class loader's data as a strong root: no loader, class or
    /// mirror is ever unloaded. Read through [`loader_unload_enabled_for`].
    pub class_unloading: AtomicBool,
    /// `CRATONVM_LOOKUP_IN_LOADS_OUTER`, read once for this VM
    /// ([`lk_in_loads_outer_enabled`]; JIT round 21 wave 16, lane lookup-in).
    pub lookup_in_loads_outer: OnceLock<bool>,
    /// `CRATONVM_DEFINE_ADD_CLASS_HINTED`, read once for this VM
    /// ([`call_loader_add_class_hinted`]; interpreter round i7 wave 135, lane
    /// L5).
    pub define_add_class_hinted: OnceLock<bool>,
    /// The `ClassLoader.addClass` declared-method hint of this VM:
    /// `(class id << 32) | (method index + 1)`, `0` for none. The VM checks
    /// the index on every call, so a stale word costs a re-resolution only.
    pub add_class_hint: AtomicU64,
    /// [`receiver_overrides_load_class`]'s memo (JIT round 23 wave 6, the
    /// proxy-define page; `CRATONVM_LOADER_LOAD_CLASS_OVERRIDE_MEMO`): the
    /// answer per (receiver class id, `loadClass` overload), stamped with
    /// `NativeContext::class_hierarchy_memo_stamp`. A moved stamp empties it
    /// (an edge edit, a stub upgrade, an unload, a redefinition), and the GC
    /// reconcile drops the rows of the classes whose loader died.
    // LEVEL: `Scratch` (L0): each acquisition reads or files one row and
    // ends; no critical section calls `ctx` or takes another lock.
    pub load_class_overrides: OrderedPlMutex<LoadClassOverrideMemo>,
}

/// See [`LoaderIdentityTables::load_class_overrides`].
#[derive(Default)]
pub struct LoadClassOverrideMemo {
    pub stamp: u64,
    // addr-keyed: not a heap address -- a class id and a `loadClass` overload index.
    pub rows: std::collections::HashMap<(u32, u8), bool>,
}

impl LoaderIdentityTables {
    /// The remembered [`receiver_overrides_load_class`] answer for
    /// `(class_id, overload)` under `stamp`, if any. A different stamp
    /// empties the memo first (stamps only grow).
    pub fn load_class_override_lookup(&self, stamp: u64, class_id: u32, overload: u8) -> Option<bool> {
        let mut memo = self
            .load_class_overrides
            .lock();
        if memo.stamp != stamp {
            memo.rows.clear();
            memo.stamp = stamp;
            return None;
        }
        memo.rows.get(&(class_id, overload)).copied()
    }

    /// Remember `answer` for `(class_id, overload)`, computed under `stamp`;
    /// dropped when the memo has moved to another stamp since.
    pub fn load_class_override_record(&self, stamp: u64, class_id: u32, overload: u8, answer: bool) {
        let mut memo = self
            .load_class_overrides
            .lock();
        if memo.stamp == stamp {
            memo.rows.insert((class_id, overload), answer);
        }
    }

    /// Forget the rows of `class_ids` (their defining loader died).
    pub fn load_class_override_forget(&self, class_ids: &[u32]) {
        if class_ids.is_empty() {
            return;
        }
        let mut memo = self
            .load_class_overrides
            .lock();
        if memo.rows.is_empty() {
            return;
        }
        let dead: std::collections::HashSet<u32> = class_ids.iter().copied().collect();
        memo.rows.retain(|&(id, _), _| !dead.contains(&id));
    }
}

/// zt t2/nd (`docs/feature-designs/zgc-architecture.md`):
/// a loader side table's rows -- still a `Vec` of `(loader, value)`, which
/// every reader and the GC reconcile's `retain_mut` see through `Deref` /
/// `DerefMut` exactly as before -- plus a position memo that answers "the
/// first row whose loader is at this address" (and, for the namespace table,
/// "the first row with this id") without a scan, under
/// `CRATONVM_LOADER_TABLES_HASHED` (default ON since JIT r24; `=0`: the scans).
///
/// The memo mirrors the rows, not the heap: it maps an address to the index
/// a scan of the CURRENT rows would find, so it is right whenever it is in
/// step with them. Within one reconcile generation
/// ([`LoaderIdentityTables::reconcile_generation`], bumped BEFORE the GC pass
/// rewrites or prunes any row) the production writers only append (`push`)
/// or change a row's value in place (`loader_meta_put` / `_upsert`), so the
/// memo catches up by indexing the appended tail. It is rebuilt when the
/// generation moves, when the rows got shorter (a test's `retain`), when the
/// last row it indexed no longer sits at its index, and after
/// [`Self::clear_rows`]. A hit is verified against the row before it is
/// answered; a failed check falls back to the scan. The proxy-define row
/// (`proxy-define-row-cpu-is-class-file-api-java-and-define-epoch-flushes-20261016-RETIRED-20261024.md`) made a fresh
/// loader per iteration, and every first lookup of a fresh loader scanned
/// every live loader's row 3-5 times.
pub struct MemoRows<T> {
    pub rows: Vec<(ObjectRef, T)>,
    pub memo: RowIndexMemo,
}

/// [`MemoRows`]' position memo.
#[derive(Default)]
pub struct RowIndexMemo {
    /// The reconcile generation the memo was built in.
    pub generation: u64,
    /// Rows indexed so far (a prefix of the rows).
    pub synced: usize,
    /// The address of row `synced - 1` when it was indexed.
    pub last_addr: usize,
    /// `false`: rebuild on the next use.
    pub valid: bool,
    // addr-keyed: mirrors the rows' current keys for ONE reconcile generation (the rows are remapped / pruned only by gc_reconcile_defining_loaders, which bumps the generation first); every hit is checked against the row.
    pub by_addr: std::collections::HashMap<usize, usize>,
    /// Value key (the namespace id) -> the first row holding it.
    pub by_key: std::collections::HashMap<u32, usize>,
}

impl<T> std::ops::Deref for MemoRows<T> {
    type Target = Vec<(ObjectRef, T)>;
    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl<T> std::ops::DerefMut for MemoRows<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rows
    }
}

impl<T> MemoRows<T> {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            memo: RowIndexMemo::default(),
        }
    }

    /// Drop every row and the memo.
    pub fn clear_rows(&mut self) {
        self.rows.clear();
        self.memo.valid = false;
    }

    /// `retain_mut` over the rows for the GC reconcile's prune-and-remap, which
    /// also drops the memo, under the same lock as the rewrite.
    ///
    /// zt t3/x2: the generation alone does not cover the rewrite. The reconcile
    /// bumps [`LoaderIdentityTables::reconcile_generation`] BEFORE it takes
    /// this table's lock, so a lookup that lands between the bump and the
    /// rewrite rebuilds the memo at the NEW generation from the OLD rows; a
    /// same-length remap whose last row did not move then passes every
    /// staleness check of [`Self::sync`], and a moved loader's new address
    /// misses (`None`, no scan) -- a second namespace id for one loader.
    pub fn retain_rows_mut(&mut self, f: impl FnMut(&mut (ObjectRef, T)) -> bool) {
        self.rows.retain_mut(f);
        self.memo.valid = false;
    }

    /// Bring the memo in step with the rows (see the type).
    pub fn sync(&mut self, generation: u64, key: impl Fn(&T) -> Option<u32>) {
        let rows = &self.rows;
        let m = &mut self.memo;
        let stale = !m.valid
            || m.generation != generation
            || rows.len() < m.synced
            || (m.synced > 0 && rows[m.synced - 1].0.as_ptr() as usize != m.last_addr);
        if stale {
            m.by_addr.clear();
            m.by_key.clear();
            m.synced = 0;
            m.generation = generation;
            m.valid = true;
        }
        for (i, (loader, value)) in rows.iter().enumerate().skip(m.synced) {
            m.by_addr.entry(loader.as_ptr() as usize).or_insert(i);
            if let Some(k) = key(value) {
                m.by_key.entry(k).or_insert(i);
            }
        }
        m.synced = rows.len();
        if let Some((loader, _)) = rows.last() {
            m.last_addr = loader.as_ptr() as usize;
        }
    }

    /// The index of the first row whose loader is at `addr`, as
    /// `iter().position` answers it.
    pub fn position_of(
        &mut self,
        generation: u64,
        addr: usize,
        key: impl Fn(&T) -> Option<u32>,
    ) -> Option<usize> {
        self.sync(generation, key);
        match self.memo.by_addr.get(&addr).copied() {
            Some(i) if row_has_loader(&self.rows, i, addr) => Some(i),
            Some(_) => {
                self.memo.valid = false;
                self.rows.iter().position(|(l, _)| l.as_ptr() as usize == addr)
            }
            None => None,
        }
    }
}

impl MemoRows<u32> {
    /// The namespace id of the first row whose loader is `loader` (a scan
    /// with the switch off).
    pub fn namespace_of(&mut self, generation: u64, loader: ObjectRef) -> Option<u32> {
        let addr = loader.as_ptr() as usize;
        self.position_of(generation, addr, |id| Some(*id)).map(|i| self.rows[i].1)
    }

    /// The loader of the first row holding namespace `ns_id` (a scan with
    /// the switch off).
    pub fn loader_of(&mut self, generation: u64, ns_id: u32) -> Option<ObjectRef> {
        self.sync(generation, |id| Some(*id));
        match self.memo.by_key.get(&ns_id).copied() {
            Some(i) if row_has_id(&self.rows, i, ns_id) => Some(self.rows[i].0),
            Some(_) => {
                self.memo.valid = false;
                self.rows
                    .iter()
                    .find(|(_, id)| *id == ns_id)
                    .map(|&(loader, _)| loader)
            }
            None => None,
        }
    }
}

impl MemoRows<LoaderMeta> {
    /// The index of `loader`'s row (a scan with the switch off).
    pub fn meta_position(&mut self, generation: u64, loader: ObjectRef) -> Option<usize> {
        let addr = loader.as_ptr() as usize;
        self.position_of(generation, addr, |_| None)
    }
}

/// Is row `i` of `rows` the loader at `addr`? (The memo's hit check.)
pub fn row_has_loader<T>(rows: &[(ObjectRef, T)], i: usize, addr: usize) -> bool {
    rows.get(i).is_some_and(|(l, _)| l.as_ptr() as usize == addr)
}

/// Does row `i` of `rows` hold namespace `ns_id`? (The memo's hit check.)
pub fn row_has_id(rows: &[(ObjectRef, u32)], i: usize, ns_id: u32) -> bool {
    rows.get(i).is_some_and(|&(_, id)| id == ns_id)
}



impl LoaderIdentityTables {
    /// The generation [`MemoRows`] are kept in step with (read under the
    /// table's lock).
    pub fn rows_generation(&self) -> u64 {
        self.reconcile_generation.load(Ordering::Acquire)
    }

    /// Empty tables for the VM numbered `vm_identity`, with a parent-link
    /// table of their own (a detached instance, or a test's).
    pub fn new(vm_identity: usize) -> Self {
        Self::with_user_loader_parents(
            vm_identity,
            Arc::new(cratonvm_classloading::UserLoaderParents::new()),
        )
    }

    /// Empty tables for the VM numbered `vm_identity` that share
    /// `user_loader_parents` with that VM's `ClassManager`
    /// (`ClassManager::user_loader_parents`; `SharedVm::new`).
    pub fn with_user_loader_parents(
        vm_identity: usize,
        user_loader_parents: Arc<cratonvm_classloading::UserLoaderParents>,
    ) -> Self {
        Self {
            vm_identity,
            user_loader_parents,
            defining: DefiningLock::new(DefiningLoaderTable::default()),
            defining_shared_reads: OnceLock::new(),
            orphaned: Mutex::new(std::collections::HashSet::new()),
            any_defining: AtomicBool::new(false),
            defining_bits: OnceLock::new(),
            // LEVEL: `Scratch` (L0); see `register_non_strong_hidden_mirror`'s
            // and `LoaderMeta`'s notes. No critical section calls `ctx`, runs
            // the GC's survivor predicate or takes another lock.
            non_strong_hidden: OrderedPlMutex::new(
                std::collections::HashMap::new(),
                LockLevel::Scratch,
            ),
            any_non_strong_hidden: AtomicBool::new(false),
            loader_meta: OrderedPlMutex::new(MemoRows::new(), LockLevel::Scratch),
            loader_namespaces: Mutex::new(MemoRows::new()),
            reconcile_generation: AtomicU64::new(0),
            singletons: Mutex::new(VmLoaderSingletons::default()),
            closed_url_loaders: Mutex::new(std::collections::HashMap::new()),
            // LEVEL: `Scratch` (L0), as the two process memos they replace:
            // each acquisition is one statement that reads or publishes and
            // ends; the reads that run Java run with nothing locked.
            module_sets: OrderedPlMutex::new(None, LockLevel::Scratch),
            builtin_packages: OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch),
            modules_defined_to_loaders: AtomicBool::new(false),
            defined_packages: crate::lang_class::DefinedPackageMemo::new(),
            // LEVEL: `Scratch` (L0): each acquisition is one insert, probe or
            // remove; no `ctx` call and no other lock under it.
            add_class_orphans: OrderedPlMutex::new(
                std::collections::HashMap::new(),
                LockLevel::Scratch,
            ),
            native_access: crate::panama::NativeAccessState::new(),
            class_unloading: AtomicBool::new(true),
            lookup_in_loads_outer: OnceLock::new(),
            define_add_class_hinted: OnceLock::new(),
            add_class_hint: AtomicU64::new(0),
            load_class_overrides: OrderedPlMutex::new(LoadClassOverrideMemo::default(), LockLevel::Scratch),
        }
    }

    /// zt t18/df7: the VM's native-access state (see the field).
    pub fn native_access(&self) -> &crate::panama::NativeAccessState {
        &self.native_access
    }

    /// The VM's defined-`Package` memo (see the field).
    pub fn defined_packages(&self) -> &crate::lang_class::DefinedPackageMemo {
        &self.defined_packages
    }

    /// The VM these tables belong to.
    pub fn vm_identity(&self) -> usize {
        self.vm_identity
    }

    /// zt t21/tg10: install this VM's `-XX:±ClassUnloading` (see the field).
    pub fn set_class_unloading(&self, on: bool) {
        self.class_unloading.store(on, Ordering::Release);
    }

    /// zt t21/tg10: does this VM unload classes (see the field)? `true`
    /// unless the VM was made with class unloading off.
    pub fn class_unloading(&self) -> bool {
        self.class_unloading.load(Ordering::Acquire)
    }

    /// The VM's user-loader delegation parents (see the field).
    pub fn user_loader_parents(&self) -> &cratonvm_classloading::UserLoaderParents {
        &self.user_loader_parents
    }

    /// Drop every row and reset the latches (a detached instance's identity
    /// teardown, [`forget_vm_loader_singletons`]). A VM's own instance is never
    /// cleared: it dies with the VM.
    pub fn clear(&self) {
        *self.defining.lock().unwrap_or_else(|e| e.into_inner()) = DefiningLoaderTable::default();
        self.orphaned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        if let Some(bits) = self.defining_bits.get() {
            for word in bits.iter() {
                word.store(0, Ordering::Release);
            }
        }
        self.non_strong_hidden.lock().clear();
        *self.load_class_overrides.lock() = LoadClassOverrideMemo::default();
        self.loader_meta.lock().clear_rows();
        self.loader_namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear_rows();
        *self.singletons.lock().unwrap_or_else(|e| e.into_inner()) = VmLoaderSingletons::default();
        self.closed_url_loaders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self.module_sets.lock() = None;
        self.builtin_packages.lock().clear();
        self.modules_defined_to_loaders.store(false, Ordering::Release);
        self.user_loader_parents.clear();
        self.defined_packages.clear();
        self.add_class_orphans.lock().clear();
    }

    /// The lock-free key-set mirror of `defining` (allocated, 128 KiB, on
    /// first use).
    pub fn bits(&self) -> &[AtomicU64] {
        self.defining_bits.get_or_init(|| {
            (0..(DEFINING_LOADER_BITS_CAP as usize / 64))
                .map(|_| AtomicU64::new(0))
                .collect()
        })
    }

    pub fn set_defining_loader_bit(&self, class_id: u32) {
        if class_id >= DEFINING_LOADER_BITS_CAP {
            return;
        }
        self.bits()[(class_id / 64) as usize].fetch_or(1u64 << (class_id % 64), Ordering::Release);
    }

    pub fn clear_defining_loader_bit(&self, class_id: u32) {
        if class_id >= DEFINING_LOADER_BITS_CAP {
            return;
        }
        // Never allocates the bitset just to clear a bit in it.
        if let Some(bits) = self.defining_bits.get() {
            bits[(class_id / 64) as usize].fetch_and(!(1u64 << (class_id % 64)), Ordering::Release);
        }
    }
}

/// THIS context's VM's [`LoaderIdentityTables`]: the VM's own
/// (`NativeSystemAccess::loader_identity_tables`), or, for a context with no
/// VM behind it (a unit test's mock context), the detached instance of its
/// VM identity ([`detached_loader_tables`]).
pub fn loader_tables(ctx: &dyn NativeContext) -> &LoaderIdentityTables {
    match ctx
        .loader_identity_tables()
        .and_then(|any| any.downcast_ref::<LoaderIdentityTables>())
    {
        Some(tables) => tables,
        None => detached_loader_tables(ctx.vm_identity()),
    }
}

/// The tables of a context that has no VM (the `NativeContext` trait's default
/// `loader_identity_tables`, i.e. the mock contexts of unit tests), one leaked
/// instance per VM identity, so two mock VMs stay apart exactly as two real
/// ones do. A real VM never reaches this: its context always answers its own
/// `SharedVm::loader_identity`. [`forget_vm_loader_singletons`] empties an
/// identity's instance (the rows of a test VM's teardown); the instance itself
/// is a few words and stays.
///
/// This is the one process-wide map left of the nine tables (interpreter round
/// i5 wave 101, lane L5): it is unreachable from a VM-backed context by
/// construction, so it holds no VM's compatibility state.
pub fn detached_loader_tables(vm_identity: usize) -> &'static LoaderIdentityTables {
    let mut map = detached_loader_table_map().lock();
    *map.entry(vm_identity)
        .or_insert_with(|| &*Box::leak(Box::new(LoaderIdentityTables::new(vm_identity))))
}

// addr-keyed: not a heap address -- a mock context's VM identity; rows are emptied by forget_vm_loader_singletons.
pub type DetachedLoaderTables = std::collections::HashMap<usize, &'static LoaderIdentityTables>;

/// [`detached_loader_tables`]' map. LEVEL: `Scratch` (L0): a probe and an
/// insert, nothing else held.
pub fn detached_loader_table_map() -> &'static OrderedPlMutex<DetachedLoaderTables> {
    static DETACHED: OnceLock<OrderedPlMutex<DetachedLoaderTables>> = OnceLock::new();
    DETACHED
        .get_or_init(|| OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch))
}

/// Empty the detached instance of `vm_identity`, if one was ever made.
pub fn forget_detached_loader_tables(vm_identity: usize) {
    let found = detached_loader_table_map().lock().get(&vm_identity).copied();
    if let Some(tables) = found {
        tables.clear();
    }
}

/// This VM's current defining-loader row addresses — exactly the set
/// `gc_reconcile_defining_loaders` will judge on the next collection. A
/// caller arms `VmHeap::zgc_set_reconcile_watch` with this BEFORE
/// `collect_garbage` so ZGC can capture each address's exact pre-slide
/// verdict (see HIB-CV-24 note on `gc_reconcile_defining_loaders`).
///
/// Since i4 wave 84 (lane L5) also every address of the loader-namespace,
/// loader-meta and closed-`URLClassLoader` tables and the non-strong hidden
/// classes' mirrors, which that pass judges beside the defining-loader rows
/// and which alias the same way on a slide. Each table's lock is taken
/// alone, in turn.
pub fn watched_defining_loader_addrs(tables: &LoaderIdentityTables) -> Vec<usize> {
    let mut addrs: Vec<usize> = tables
        .defining
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rows
        .values()
        .map(|obj_ref| obj_ref.as_ptr() as usize)
        .collect();
    addrs.extend(
        tables
            .loader_namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(obj_ref, _)| obj_ref.as_ptr() as usize),
    );
    addrs.extend(
        tables
            .loader_meta
            .lock()
            .iter()
            .map(|(obj_ref, _)| obj_ref.as_ptr() as usize),
    );
    addrs.extend(
        closed_url_classloader_ids(tables)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .flat_map(|rows| rows.iter().copied()),
    );
    if tables.any_non_strong_hidden.load(Ordering::Acquire) {
        addrs.extend(tables.non_strong_hidden.lock().values().copied());
    }
    addrs.sort_unstable();
    addrs.dedup();
    addrs
}

/// The defining-loader rows, plus a reverse index from each loader to the
/// classes it defined, kept under the ONE mutex so the two cannot disagree.
///
/// gc-common w10-f
/// (`docs/feature-designs/native-dispatch-architecture.md`):
/// [`class_defined_by_this_loader_object`] used to answer "which classes did
/// THIS loader object define" by walking every row of every VM under this
/// mutex, and then reading the name of each of that loader's classes. It runs
/// on every user-loader `findLoadedClass` miss, and every define is preceded
/// by at least one, so a loader defining `n` classes cost `O(n x rows)` row
/// visits plus `O(n^2)` class-name reads, with the process-wide mutex held
/// for each walk. The index removes both terms: `by_loader` names the loader's
/// classes directly, and each entry memoizes the names it has already read.
/// (The notes above are [`DefiningLoaderTable`]'s, below.)
///
/// The defining-loader table's lock (interpreter round i7 wave 143, lane L5;
/// `i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot`, wave
/// 115's "What remains": the table's `Mutex` serialised its readers). An
/// `RwLock` whose `lock()` is the WRITE side, so every caller that took the
/// `Mutex` keeps an exclusive acquisition with the same poison handling;
/// [`defining_table_read`] takes the shared side for a read-only lookup when
/// the VM's `CRATONVM_DEFINING_LOADER_SHARED_READS` is on (performance,
/// default OFF), else the write side as before.
pub struct DefiningLock {
    pub inner: std::sync::RwLock<DefiningLoaderTable>,
}

impl DefiningLock {
    pub fn new(table: DefiningLoaderTable) -> Self {
        Self {
            inner: std::sync::RwLock::new(table),
        }
    }

    /// The exclusive acquisition (what the `Mutex`'s `lock` was).
    pub fn lock(&self) -> std::sync::LockResult<std::sync::RwLockWriteGuard<'_, DefiningLoaderTable>> {
        self.inner.write()
    }
}

/// A read-only view of the defining-loader table: shared or exclusive.
pub enum DefiningRead<'a> {
    Shared(std::sync::RwLockReadGuard<'a, DefiningLoaderTable>),
    Exclusive(std::sync::RwLockWriteGuard<'a, DefiningLoaderTable>),
}

impl std::ops::Deref for DefiningRead<'_> {
    type Target = DefiningLoaderTable;
    fn deref(&self) -> &DefiningLoaderTable {
        match self {
            DefiningRead::Shared(g) => g,
            DefiningRead::Exclusive(g) => g,
        }
    }
}

/// `CRATONVM_DEFINING_LOADER_SHARED_READS` for `tables`' VM (`runtime_flag_default_on`,
/// read once per VM).
pub fn defining_shared_reads(tables: &LoaderIdentityTables) -> bool {
    *tables.defining_shared_reads.get_or_init(|| {
        true
    })
}

/// The defining-loader table for a read-only lookup: the shared side under
/// [`defining_shared_reads`], else the exclusive side, as before.
pub fn defining_table_read(tables: &LoaderIdentityTables) -> DefiningRead<'_> {
    if defining_shared_reads(tables) {
        DefiningRead::Shared(tables.defining.inner.read().unwrap_or_else(|e| e.into_inner()))
    } else {
        DefiningRead::Exclusive(tables.defining.lock().unwrap_or_else(|e| e.into_inner()))
    }
}


#[derive(Default)]
pub struct DefiningLoaderTable {
    /// `class_id -> defining loader`, of the owning VM. The authoritative map.
    /// (The loader VALUES are PRUNED + REMAPPED by gc_reconcile_defining_loaders.)
    pub rows: std::collections::HashMap<u32, ObjectRef>,
    /// `loader address -> the class ids whose row names that loader`. Keyed
    /// by the address as a `usize`, so it is not an `ObjectRef`-keyed table:
    /// [`gc_reconcile_defining_loaders`] re-keys it through the pointer map
    /// and drops a dead loader's entry in the same critical section that
    /// prunes and remaps `rows`.
    // addr-keyed: REMAPPED + PRUNED by DefiningLoaderTable::reindex (from gc_reconcile_defining_loaders); dies with the VM's LoaderIdentityTables.
    pub by_loader: std::collections::HashMap<usize, LoaderDefinedClasses>,
}

/// One loader's classes, in registration order, and the names read so far.
#[derive(Default)]
pub struct LoaderDefinedClasses {
    pub ids: Vec<u32>,
    /// Name -> class id for `ids[..named]`. The first id registered under a
    /// name wins, and a name is never re-read: a class id names one class for
    /// the life of its VM (`ClassStore::add` appends, and ids are not reused).
    pub by_name: std::collections::HashMap<Arc<str>, u32>,
    /// How many of `ids`, from the front, have their names in `by_name`.
    pub named: usize,
}

impl LoaderDefinedClasses {
    /// Keep the ids `keep` accepts. Removing an id whose name was already
    /// read forgets every memoized name, since `by_name` holds only the first
    /// id per name and cannot tell which later id it was hiding. The names are
    /// read again lazily. Removal is rare: it happens only when a class is
    /// re-registered to another loader, or when a remap leaves the row
    /// naming a different address.
    pub fn retain_ids(&mut self, keep: impl Fn(u32) -> bool) {
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
    pub fn publish_names(&mut self, read: &[(u32, Option<Arc<str>>)]) {
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
    /// Insert or replace `class_id`'s row, keeping the index in step.
    pub fn insert(&mut self, class_id: u32, loader: ObjectRef) {
        self.insert_with(class_id, loader, true);
    }

    /// [`Self::insert`]; with `index == false` the row is written but the
    /// class is left out of `by_loader` (zt t16/df6: a define whose name is
    /// not bound yet, [`register_defining_loader_unbound`]), so
    /// [`class_defined_by_this_loader_object`] cannot answer it until
    /// [`Self::index_id`] runs.
    pub fn insert_with(&mut self, class_id: u32, loader: ObjectRef, index: bool) {
        let new_addr = loader.as_ptr() as usize;
        match self.rows.insert(class_id, loader) {
            Some(old) if old.as_ptr() as usize == new_addr && index => return,
            Some(old) => {
                let old_key = old.as_ptr() as usize;
                if let Some(entry) = self.by_loader.get_mut(&old_key) {
                    entry.retain_ids(|cid| cid != class_id);
                    if entry.ids.is_empty() {
                        self.by_loader.remove(&old_key);
                    }
                }
            }
            None => {}
        }
        if index {
            self.by_loader
                .entry(new_addr)
                .or_default()
                .ids
                .push(class_id);
        }
    }

    /// zt t16/df6: put `class_id` (its row written by [`Self::insert_with`]
    /// with `index == false`) into its loader's `by_loader` entry. A no-op
    /// without a row. Not a scan of the entry (a loader may hold thousands of
    /// ids, and this runs once per define): an id pushed twice is harmless,
    /// since `publish_names` keeps the first id per name and `retain_ids`
    /// drops every copy.
    pub fn index_id(&mut self, class_id: u32) {
        let Some(loader) = self.rows.get(&class_id) else {
            return;
        };
        let entry = self.by_loader.entry(loader.as_ptr() as usize).or_default();
        if entry.ids.last() != Some(&class_id) {
            entry.ids.push(class_id);
        }
    }

    /// Bring the index entries in line with the rows after the reconcile
    /// pruned and remapped them. Each entry is re-keyed through
    /// `pointer_map`, and keeps only the ids whose row still names the
    /// (possibly moved) loader, so a dead loader's entry empties and goes.
    /// Memoized names survive the move: they belong to class ids, not to
    /// addresses. Costs one row look-up per indexed id, the same order as the
    /// loader-pin re-sync that follows it.
    pub fn reindex(&mut self, pointer_map: &cratonvm_types::PointerMap) {
        let keys: Vec<usize> = self.by_loader.keys().copied().collect();
        let mut rekeyed: Vec<(usize, LoaderDefinedClasses)> = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(mut entry) = self.by_loader.remove(&key) else {
                continue;
            };
            let new_addr = pointer_map.get(&key).copied().unwrap_or(key);
            let rows = &self.rows;
            entry.retain_ids(|cid| {
                rows.get(&cid)
                    .is_some_and(|loader| loader.as_ptr() as usize == new_addr)
            });
            if !entry.ids.is_empty() {
                rekeyed.push((new_addr, entry));
            }
        }
        for (addr, entry) in rekeyed {
            match self.by_loader.entry(addr) {
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
///
/// The set is [`LoaderIdentityTables::orphaned`], the VM's own since
/// interpreter round i5 wave 101, lane L5 (it was a process global keyed by
/// `(vm_identity, class_id)`, the "orphaned_defining_loader_classes" table).
///
/// Whether `class_id`'s defining loader has been confirmed collected. `pub`
/// so the VM's unload tests can observe the marker (gc-common w11-e).
pub fn is_defining_loader_orphaned(tables: &LoaderIdentityTables, class_id: u32) -> bool {
    tables
        .orphaned
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
/// that case. Correctness: it only ever transitions false→true (in
/// `register_defining_loader`) and is never reset, so a `false` read here is
/// always accurate at the instant it's read for a map that has never had an
/// insert — no ABA/staleness risk. (It used to be reset alongside the map by
/// `reset_loader_singletons`; the map is per VM now and that reset is gone.)
///
/// The latch is [`LoaderIdentityTables::any_defining`] since interpreter round
/// i5 wave 101, lane L5: it was one process-wide `AtomicBool`, so once ANY VM
/// of the process defined a class through a user loader, every other VM's
/// hot paths answered "maybe" and took the map's mutex.
///
/// Highest `class_id` the [`LoaderIdentityTables::bits`] mirror can answer for. Ids
/// at or above it fall back to the map (correct, just not lock-free); 1M is
/// far past what any real workload reaches, so the fallback is dead code in
/// practice rather than a second hot path.
pub const DEFINING_LOADER_BITS_CAP: u32 = 1 << 20;

/// Lock-free per-`ClassId` mirror of the defining-loader rows' KEY SET
/// ([`LoaderIdentityTables::defining_bits`]).
///
/// The latch answers the same question for the whole VM (until interpreter
/// round i5 wave 101 the whole PROCESS), and that is precisely why it stops
/// paying. It latches true the
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
/// **Maintenance — two writers, matching the map's own two:**
/// [`register_defining_loader`] sets a bit and [`gc_reconcile_defining_loaders`]
/// clears the bits of entries it retires (a VM's teardown drops the whole set
/// with its `LoaderIdentityTables`, interpreter round i5 wave 101, lane L5).
/// Each writes the bit while the map lock is held (or, for
/// registration, after the insert), so a reader that observes a bit SET and
/// then takes the lock either finds the entry or finds it concurrently
/// removed — both already-legal outcomes. A reader that observes a bit CLEAR
/// returns `None`, which is the same race the VM-wide latch already had
/// against a concurrent first registration.
///
/// Allocated lazily (128 KiB) on the first registration, so a run that never
/// uses a custom loader never pays for it.
///
/// Could `class_id` have a registered defining loader? `false` is exact — the
/// caller may skip the map entirely. `true` means "consult the map", which may
/// still answer `None`. Lock-free; public for the interpreter's cast-site fill
/// screen, which must not take the map's mutex per execution.
#[inline]
pub fn class_may_have_defining_loader(tables: &LoaderIdentityTables, class_id: u32) -> bool {
    if !tables.any_defining.load(Ordering::Acquire) {
        return false;
    }
    if class_id >= DEFINING_LOADER_BITS_CAP {
        return true;
    }
    let word = &tables.bits()[(class_id / 64) as usize];
    word.load(Ordering::Acquire) & (1u64 << (class_id % 64)) != 0
}

/// Record the user-defined `ClassLoader` object that defined `class_id`, so
/// `Class.getClassLoader()` returns the exact instance instead of the app-loader
/// fallback.
pub fn register_defining_loader(tables: &LoaderIdentityTables, class_id: u32, loader: ObjectRef) {
    register_defining_loader_with(tables, class_id, loader, true);
}

/// zt t16/df6 (define page §1, corner 2): [`register_defining_loader`] for a
/// define whose name the class manager has not bound yet
/// (`DefineClassFull::bind_name_after_add_class`): the row is written (so the
/// mirror minted next gets its `mirror_pin` row, and `getClassLoader`, the
/// loader pin and the unload hints work as for any class), but the class is
/// left out of the loader's by-name index, so
/// [`class_defined_by_this_loader_object`] -- and through it
/// `findLoadedClass` -- cannot answer it while its `addClass` upcall runs.
/// [`settle_unbound_define`] indexes it once the name is bound, and never
/// when the define is withdrawn.
pub fn register_defining_loader_unbound(
    tables: &LoaderIdentityTables,
    class_id: u32,
    loader: ObjectRef,
) {
    register_defining_loader_with(tables, class_id, loader, false);
}

/// zt t16/df6: put a class registered by [`register_defining_loader_unbound`]
/// into its loader's by-name index.
pub fn index_defining_loader(tables: &LoaderIdentityTables, class_id: u32) {
    tables
        .defining
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .index_id(class_id);
}

pub fn register_defining_loader_with(
    tables: &LoaderIdentityTables,
    class_id: u32,
    loader: ObjectRef,
    index: bool,
) {
    let vm = tables.vm_identity;
    tables
        .defining
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert_with(class_id, loader, index);
    // Bit AFTER the insert, latch after the bit: a reader that sees either
    // signal set then finds the entry already in the map.
    tables.set_defining_loader_bit(class_id);
    tables.any_defining.store(true, Ordering::Release);
    // gc-common w11-e: a class whose previous defining loader died, and which
    // a live loader now adopts (a built-in-namespace class stays in the store
    // after its loader dies), is restricted by this new row from here on. Its
    // orphan marker would otherwise go on hiding it from every loader,
    // including this one (`cid_visible_mirror` checks the marker first). Not
    // nested in the store lock above: the reconcile takes store -> orphans,
    // and this takes orphans alone.
    tables
        .orphaned
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&class_id);
    // HIB-CV-24: mirror into the loader-pin registry the GC marker consults so a
    // live instance of this class keeps its defining loader alive (the
    // instance→loader edge HotSpot gets for free via `Class.getClassLoader`).
    //
    // gc-common w18-d: except for a non-strong hidden class, whose instances
    // must keep its MIRROR alive (the mirror keeps the loader through its
    // `classLoader` field). A re-registration of such a class must not turn
    // its edge back into instance -> loader, or its mirror (no longer rooted
    // and no longer pinned to the loader) could die under a live instance.
    let pin_target =
        non_strong_hidden_mirror(tables, class_id).unwrap_or(loader.as_ptr() as usize);
    cratonvm_types::loader_pin::set_loader_pin(vm, class_id, pin_target);
}

/// gci i12/ul F2 (`gci-i12ul-class-unloading-and-side-tables-on-g1-review-findings-RETIRED-UNFIXED-20261018`):
/// give the array class `array_cid` of a user namespace the defining-loader row
/// of its innermost element class `element_cid` (JVMS 5.3.3: an array class is
/// defined by its element's loader), under `CRATONVM_ARRAY_CLASS_LOADER_ROWS`
/// (default ON since 2026-10-17, `=0` kills). Returns whether a row was added.
///
/// With the row, as HotSpot's `ObjArrayKlass -> element klass -> CLD`:
/// * a live `X[]` instance keeps the loader alive (`loader_pin`, the collectors'
///   instance -> owner edge);
/// * the array's mirror gets a `mirror_pin` row when it is minted
///   (`class_mirror_impl`), so the root scan defers it to the loader instead of
///   rooting it -- it used to be rooted on every scan, and its `componentType`
///   reached the element's mirror and the loader, so the loader never unloaded;
/// * the array class is an unload hint with the loader's other classes, so it
///   leaves the store and the namespace can empty.
///
/// Called by the VM when the array's mirror is minted and, for every array the
/// class manager queued (`ClassManager::take_pending_user_arrays`), at the start
/// of each root scan. A no-op when the array already has a row or the element
/// has none (a built-in element).
pub fn register_array_class_loader(
    tables: &LoaderIdentityTables,
    array_cid: u32,
    element_cid: u32,
) -> bool {
    if !array_class_loader_rows_enabled() {
        return false;
    }
    register_array_class_loader_row(tables, array_cid, element_cid)
}

/// gci i12/ul F2: `CRATONVM_ARRAY_CLASS_LOADER_ROWS` (default ON since
/// 2026-10-17, `runtime_flag_default_on`, `=0` kills), the switch of
/// [`register_array_class_loader`]; the VM's callers ask it first to skip
/// their class-manager read. No `static`.
pub fn array_class_loader_rows_enabled() -> bool {
    true
}

/// [`register_array_class_loader`] without its switch.
pub fn register_array_class_loader_row(
    tables: &LoaderIdentityTables,
    array_cid: u32,
    element_cid: u32,
) -> bool {
    if array_cid == element_cid || defining_loader_for(tables, array_cid).is_some() {
        return false;
    }
    // A non-strong hidden element's owner is its MIRROR, not its loader
    // (gc-common w18-d): a loader row for its array would let the element
    // unload under a live array instance. Left as before (no row).
    if non_strong_hidden_mirror(tables, element_cid).is_some() {
        return false;
    }
    match defining_loader_for(tables, element_cid) {
        Some(loader) => {
            register_defining_loader(tables, array_cid, loader);
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Non-strong hidden classes (JEP 371) -- gc-common w18-d.
//
// A hidden class defined WITHOUT `ClassOption.STRONG` (`defineClass0`'s
// `STRONG_LOADER_LINK` bit clear) is unloaded by HotSpot as soon as the class
// itself is unreachable -- no mirror reference, no instance -- even while its
// defining loader lives. CratonVM unloaded a class only with its defining
// loader, so such a class of an application or platform loader never unloaded
// (`docs/feature-designs/class-resolution-and-caching.md`).
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

// The rows are [`LoaderIdentityTables::non_strong_hidden`]: `class_id -> the
// class's mirror address`, for every live non-strong hidden class of the VM
// (a process global keyed by `(vm_identity, class_id)` until interpreter round
// i5 wave 101, lane L5). Holding a plain address, it is not an
// `ObjectRef`-keyed table: the post-GC reconcile prunes a row whose mirror
// died and remaps a moved one, in the same pass that prunes and remaps the
// defining-loader rows.
//
// LEVEL: `Scratch` (L0). Every critical section is an insert, a lookup, a
// copy-out or a `retain` over plain values; none calls `ctx`, runs the GC's
// survivor predicate, or takes another lock (the `loader_pin` / `mirror_pin`
// writes are made after the guard drops).
//
// The latch beside it ([`LoaderIdentityTables::any_non_strong_hidden`]) lets
// every reader skip the lock in the common run that never registers one. Only
// ever goes false -> true.

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
    // zt t21/tg10: nor in a VM made with `-XX:-ClassUnloading`, where HotSpot
    // keeps every class (a non-strong hidden one included) for the VM's life.
    if vm == 0 || !loader_unload_enabled_for(loader_tables(ctx)) {
        return false;
    }
    // Cached by the define path already: no allocation, so `mirror` is
    // current until this function returns.
    let mirror = ctx.get_class_mirror(class_id);
    let loader = defining_loader_for(loader_tables(ctx), class_id.as_u32());
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
        loader_tables(ctx),
        class_id.as_u32(),
        mirror.as_ptr() as usize,
        loader.map(|l| l.as_ptr() as usize),
    );
    true
}

/// Does `mirror`'s `Class.classLoader` field name `loader`? The heap edge a
/// non-strong hidden class's mirror must carry (see
/// [`register_non_strong_hidden_class`]).
pub fn mirror_names_loader(ctx: &mut dyn NativeContext, mirror: ObjectRef, loader: ObjectRef) -> bool {
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
    tables: &LoaderIdentityTables,
    class_id: u32,
    mirror_addr: usize,
    loader_addr: Option<usize>,
) {
    let vm = tables.vm_identity;
    tables
        .non_strong_hidden
        .lock()
        .insert(class_id, mirror_addr);
    tables.any_non_strong_hidden.store(true, Ordering::Release);
    cratonvm_types::loader_pin::set_loader_pin(vm, class_id, mirror_addr);
    cratonvm_types::mirror_pin::exclude_mirror(vm, mirror_addr, loader_addr);
}

/// Whether this VM ever registered a non-strong hidden class (one atomic
/// load; only ever goes false -> true). gce e1/j: lets a per-frame caller
/// (`vm::memory::roots::frame_class_owners`) skip its walk entirely.
pub fn any_non_strong_hidden(tables: &LoaderIdentityTables) -> bool {
    tables.any_non_strong_hidden.load(Ordering::Acquire)
}

/// The mirror address of the VM's non-strong hidden class `class_id`, or
/// `None` for every other class (one load when the VM never defined one).
pub fn non_strong_hidden_mirror(tables: &LoaderIdentityTables, class_id: u32) -> Option<usize> {
    if !tables.any_non_strong_hidden.load(Ordering::Acquire) {
        return None;
    }
    tables.non_strong_hidden.lock().get(&class_id).copied()
}

/// Every `(class_id, mirror address)` of the VM's live non-strong hidden
/// classes, for a root scan that asks once per collection rather than once
/// per mirror or frame. Empty (no lock) when the VM never defined one.
pub fn non_strong_hidden_mirrors(tables: &LoaderIdentityTables) -> std::collections::HashMap<u32, usize> {
    if !tables.any_non_strong_hidden.load(Ordering::Acquire) {
        return std::collections::HashMap::new();
    }
    let rows = tables.non_strong_hidden.lock();
    (*rows).clone()
}

/// Post-GC half for the non-strong hidden classes, run first by
/// [`gc_reconcile_defining_loaders`]: judge each of `vm`'s mirrors with the
/// cycle's survivor predicate (no lock held), drop the rows whose mirror died
/// and remap the moved ones. Returns the live rows at their current addresses
/// and the ids of the dead ones, which are unload hints whatever their
/// loader's state. Re-publishes the `mirror_pin` exclusions at the current
/// addresses, ahead of `vm::memory::gc::rebuild_mirror_pins` in the same
/// epilogue.
pub fn reconcile_non_strong_hidden(
    tables: &LoaderIdentityTables,
    is_marked: &dyn Fn(usize) -> bool,
    pointer_map: &cratonvm_types::PointerMap,
    // addr-keyed: a parameter -- the caller's pre-relocation verdicts for this pause only.
    exact_results: Option<&std::collections::HashMap<usize, bool>>,
) -> (Vec<(u32, usize)>, std::collections::HashSet<u32>) {
    let vm = tables.vm_identity;
    let mut live: Vec<(u32, usize)> = Vec::new();
    let mut dead_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    if !tables.any_non_strong_hidden.load(Ordering::Acquire) {
        return (live, dead_ids);
    }
    // Phase 1: this VM's rows.
    let ours: Vec<(u32, usize)> = tables
        .non_strong_hidden
        .lock()
        .iter()
        .map(|(&cid, &mirror)| (cid, mirror))
        .collect();
    if ours.is_empty() {
        return (live, dead_ids);
    }
    // Phase 2: verdicts, no lock held (`is_marked` takes heap locks). The
    // pre-slide exact verdict first, as the defining-loader pass does (i4
    // wave 84, lane L5): on ZGC `is_marked` is the post-slide registry probe,
    // which calls a dead mirror live once a survivor slid into its span --
    // and the row would then publish that survivor's address as an EXCLUDED
    // mirror (`replace_excluded_mirrors` below), unrooting whatever mirror
    // now lives there.
    let dead: std::collections::HashSet<(u32, usize)> = ours
        .into_iter()
        .filter(|&(_, mirror)| {
            !exact_results
                .and_then(|m| m.get(&mirror).copied())
                .unwrap_or_else(|| is_marked(mirror))
        })
        .collect();
    // Phase 3: apply. A row registered between the phases is not in `dead`,
    // so it is kept (the same rule as the defining-loader pass).
    tables
        .non_strong_hidden
        .lock()
        .retain(|&cid, mirror| {
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

/// Drop the VM's non-strong hidden rows for `class_ids` (classes no longer in
/// the store) and re-publish the remaining exclusions.
pub fn forget_non_strong_hidden(
    tables: &LoaderIdentityTables,
    class_ids: &std::collections::HashSet<u32>,
) {
    if !tables.any_non_strong_hidden.load(Ordering::Acquire) {
        return;
    }
    let remaining: Vec<usize> = {
        let mut store = tables.non_strong_hidden.lock();
        store.retain(|cid, _| !class_ids.contains(cid));
        store.values().copied().collect()
    };
    cratonvm_types::mirror_pin::replace_excluded_mirrors(tables.vm_identity, &remaining);
}

/// Look up the user-defined `ClassLoader` object that defined `class_id`.
pub fn defining_loader_for(tables: &LoaderIdentityTables, class_id: u32) -> Option<ObjectRef> {
    if !class_may_have_defining_loader(tables, class_id) {
        return None;
    }
    // Wave 143 (`CRATONVM_DEFINING_LOADER_SHARED_READS`): a shared read.
    defining_table_read(tables).rows.get(&class_id).copied()
}

/// Whether ANY class of this VM has ever been defined by a
/// user-defined `ClassLoader` (i.e. `register_defining_loader` has been
/// called at least once). A single lock-free atomic load — safe for
/// interpreter hot paths that need to bail out before taking any
/// classloader-related lock at all. Since `register_defining_loader` is
/// called unconditionally whenever a class is assigned a
/// `ClassLoaderId::UserDefined(_)` identity (see the invariant documented
/// on [`LoaderIdentityTables::any_defining`]), `false` here guarantees no
/// `ClassId` anywhere in this VM's `class_manager` currently has a
/// `UserDefined` loader id — so callers that only care about "is
/// loader-initiated resolution even possibly relevant" can skip a
/// `class_manager` read lock entirely in that (overwhelmingly common) case.
/// Per VM since interpreter round i5 wave 101, lane L5 (another VM's user
/// loaders no longer turn it on).
pub fn any_defining_loader_registered(tables: &LoaderIdentityTables) -> bool {
    tables.any_defining.load(Ordering::Acquire)
}

/// Whether this VM's application loader object exists yet (adopted or made).
/// The VM's `addClass` record of a class it defined itself
/// ([`record_vm_defined_class_in_builtin_loader`]) answers "not now" before
/// that; `vm_util::record_in_builtin_loader` keeps such classes pending and
/// records them once this is `true`. Interpreter round i7 wave 139, lane L5.
pub fn builtin_app_loader_known(tables: &LoaderIdentityTables) -> bool {
    app_loader_of(tables).is_some()
}

/// SBR2 wave 8 (lane LOADERS): `CRATONVM_REAL_BUILTIN_LOADERS=1` makes the
/// VM's platform and application loader the JDK's own `ClassLoaders` objects
/// (`ClassLoaders.platformClassLoader()` / `appClassLoader()`), built by the
/// real `BuiltinClassLoader` constructors, instead of minting look-alikes with
/// `alloc_classloader`. `--real-jdk` only; ON by default there (`=0` turns it off).
/// Read per VM (`NativeContext::real_builtin_loaders`, the registry's field);
/// a process `OnceLock` until interpreter round i4 wave 95, lane L5.
pub fn real_builtin_loaders_enabled(ctx: &dyn NativeContext) -> bool {
    ctx.is_jdk_only() && ctx.real_builtin_loaders()
}

/// Whether `vm''s built-in loader singletons are the JDK's own objects.
pub fn builtin_loaders_adopted(tables: &LoaderIdentityTables) -> bool {
    with_loader_singletons(tables, |r| r.adopted)
}

/// The adopted built-in loader for a class tagged `NATIVE_EXTENSION` (1) or
/// `NATIVE_APPLICATION` (2); `None` when nothing is adopted or for any other
/// tag. Lets the mirror allocator fill `Class.classLoader` without a context.
pub fn adopted_builtin_loader_for_tag(
    tables: &LoaderIdentityTables,
    tag: u32,
    class_name: &str,
) -> Option<ObjectRef> {
    with_loader_singletons(tables, |row| {
        // A platform-module class of the image carries the boot tag (0): only the JDK's own
        // platform-package table (adoption) can say which.
        if tag == 0 {
            if !row.adopted {
                return None;
            }
            let (pkg, _) = class_name.rsplit_once('/')?;
            if row.platform_packages.contains(pkg) {
                return row.platform;
            }
            // Interpreter round i5 wave 100, lane L3 (`L3W100ModuleInfoByModule`
            // `--real-jdk`: `com.sun.tools.javac.Main.class.getResource
            // ("/module-info.class")` read null, HotSpot
            // `jrt:/jdk.compiler/module-info.class`): an image class of an
            // APPLICATION-loader module carries the boot tag too. HotSpot's
            // `Class.classLoader` is the application loader; null sent the
            // JDK's `Class.getResource*` down the `BootLoader` arm, whose
            // `nameToModule` has no `jdk.compiler`.
            return row
                .app_image_packages
                .contains(pkg)
                .then_some(row.app)
                .flatten();
        }
        // `--compatible` keeps the VM's own loader objects (not adopted) but they ARE the answer
        // `Class.getClassLoader` gives for these tags; the JDK's `Class.getPackage` / `getProtectionDomain`
        // bodies read the field, and a null there made an application class look like a boot class
        // (`BootLoader.definePackage`: "InternalError: net.bytebuddy not loaded").
        match tag {
            1 => row.platform,
            2 => row.app,
            _ => None,
        }
    })
}

/// The adopted APPLICATION loader for a boot-tagged (tag 0) class of the image
/// module `module` when the JDK's `ModuleLoaderMap` gives that module to the
/// application loader (`jdk.compiler`, `jdk.jshell`, ...: every image module in
/// neither its boot nor its platform set); `None` otherwise, while this VM's
/// module tables are cold, or when nothing is adopted. Lets the mirror allocator
/// fill `Class.classLoader` from creation, as HotSpot's `create_mirror` does,
/// with the answer `Class.getClassLoader`'s native already gives
/// ([`builtin_loader_for_image_class`] under `app_module_loaders`).
///
/// gci i4/jd: the field used to be written only when that native ran, so a
/// `jdk.compiler` class whose `getClassLoader` was first answered by the JDK
/// body (`return classLoader;`) read `null`: `MemberName.ensureTypeVisible`
/// refused `JavaCompiler::readModuleName` ("bad method type alias ... not
/// visible") and the in-VM `MethodType` walk asked the bootstrap loader for
/// `com/sun/tools/javac/main/JavaCompiler` (the slashed
/// `ClassNotFoundException`) in the concurrent javac first round
/// (`docs/feature-designs/native-dispatch-architecture.md`).
///
/// `class_name` must be in a JDK package (the native's `is_jdk_pkg` rule), so a
/// class appended to the bootstrap search never names a loader here. Runs no
/// Java: the module tables are read from [`jdk_builtin_module_sets`]' memo only.
pub fn adopted_app_loader_for_image_module(
    tables: &LoaderIdentityTables,
    module: &str,
    class_name: &str,
    app_module_loaders: bool,
) -> Option<ObjectRef> {
    // A dynamic proxy's own `jdk.proxyN` module is defined to the proxy's loader,
    // not by the image's table: never named here.
    if !app_module_loaders
        || !is_image_jdk_package_name(class_name)
        || module.starts_with("jdk.proxy")
    {
        return None;
    }
    // Merge of dev into the gci round (2026-10-16): the memo and the
    // singleton row are this VM's own `LoaderIdentityTables` since
    // interpreter round i5 wave 102, lane L5 (process maps keyed by VM
    // identity before). Dev's wave 100 L3 `app_image_packages` arm of
    // `adopted_builtin_loader_for_tag` answers the same classes first.
    let sets = tables.module_sets.lock().clone()?;
    if sets.boot.contains(module) || sets.platform.contains(module) {
        return None;
    }
    with_loader_singletons(tables, |row| {
        if !row.adopted {
            return None;
        }
        row.app
    })
}

/// `lang_class::native_class_get_class_loader`'s `is_jdk_pkg` predicate: the
/// package prefixes of the runtime image. The `sun/reflect/misc/Trampoline`
/// exception is gone (interpreter round i7 wave 137, lane L5): only a
/// boot-tagged class reaches [`adopted_app_loader_for_image_module`], and the
/// boot copy's module, `java.base`, is in the boot set, which names no loader.
pub fn is_image_jdk_package_name(class_name: &str) -> bool {
    class_name.starts_with("java/")
        || class_name.starts_with("javax/")
        || class_name.starts_with("jdk/")
        || class_name.starts_with("sun/")
        || class_name.starts_with("com/sun/")
}

thread_local! {
    pub static ADOPTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this thread is inside [`adopt_real_builtin_loader`], i.e. running the
/// JDK's `ClassLoaders.<clinit>`. A boot-layer build started from here would
/// define the modules to a look-alike platform loader (the nested request meets
/// the adoption guard and mints one), so the `Class.getModule()` trigger waits.
pub fn builtin_loader_adoption_in_progress() -> bool {
    ADOPTING.with(|a| a.get())
}

/// Make the adopted application loader's own `unnamedModule` the VM's canonical unnamed module (see the
/// call site): the loader's object is published as the canonical one when none exists yet; when one was
/// minted before the adoption, it is written into the loader's field and takes the loader as its own.
pub fn unify_app_unnamed_module(ctx: &mut dyn NativeContext, loader: ObjectRef) {
    let pin = ctx.pin_native_root(loader);
    match ctx.get_cached_module_mirror(None) {
        None => {
            if let Value::Object(Some(own)) = ctx.get_field_by_name(loader, "unnamedModule") {
                let is_module = ctx
                    .class_name_arc_of_id(ctx.class_id_of_object(own))
                    .as_deref()
                    == Some("java/lang/Module");
                if is_module {
                    ctx.cache_module_mirror(None, own);
                    // The first unnamed-module publication applies
                    // `--enable-native-access=ALL-UNNAMED` to the real
                    // `Module.ALL_UNNAMED_MODULE`, as the
                    // `canonical_unnamed_module` mint does; without it the
                    // JDK's `Module.ensureNativeAccess` warned under the flag
                    // (`R20YCompatSegRows --real-jdk` on the dev merge).
                    crate::lang_class::grant_unnamed_native_access(ctx);
                }
            }
        }
        Some(canonical) => {
            let canonical_pin = ctx.pin_native_root(canonical);
            let loader = ctx.read_native_pin(pin, loader);
            ctx.set_field_by_name(loader, "unnamedModule", Value::Object(Some(canonical)));
            let canonical = ctx.read_native_pin(canonical_pin, canonical);
            let loader = ctx.read_native_pin(pin, loader);
            ctx.set_field_by_name(canonical, "loader", Value::Object(Some(loader)));
            ctx.unpin_native_roots(canonical_pin);
        }
    }
    ctx.unpin_native_roots(pin);
}

/// Adopt `ClassLoaders.<method>()` as the VM's built-in loader of `loader_type`.
/// `None` (and nothing cached) when the JDK's object is not available yet, e.g.
/// re-entry from inside `ClassLoaders.<clinit>`.
pub fn adopt_real_builtin_loader(
    ctx: &mut dyn NativeContext,
    method: &str,
    loader_type: i32,
) -> Option<ObjectRef> {
    if ADOPTING.with(|a| a.replace(true)) {
        return None;
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ADOPTING.with(|a| a.set(false));
        }
    }
    let _reset = Reset;
    let Ok(Some(Value::Object(Some(obj)))) = ctx.invoke(
        "jdk/internal/loader/ClassLoaders",
        method,
        "()Ljava/lang/ClassLoader;",
        &[],
    ) else {
        return None;
    };
    let obj_pin = ctx.pin_native_root(obj);
    let mut obj = ctx.read_native_pin(obj_pin, obj);
    // The JDK's constructor ran through the `ClassLoader.<init>` natives,
    // which record a custom loader with a fresh namespace id; the built-in
    // loaders live in the shared application namespace (id 0).
    loader_meta_put(
        loader_tables(ctx),
        obj,
        LoaderMeta {
            loader_type: Some(loader_type),
            classes_loaded: Some(0),
            parallel_capable: Some(true),
            loader_id: Some(0),
        },
    );
    // Fields the constructor natives leave null, as `alloc_classloader` fills.
    for (field, class) in [
        ("parallelLockMap", "java/util/concurrent/ConcurrentHashMap"),
        ("assertionLock", "java/lang/Object"),
    ] {
        obj = ctx.read_native_pin(obj_pin, obj);
        if !matches!(ctx.get_field_by_name(obj, field), Value::Object(None)) {
            continue;
        }
        let nfields = if class == "java/lang/Object" { 0 } else { 3 };
        let Ok(v) = try_alloc_concurrent_synthetic(ctx, class, nfields) else {
            continue;
        };
        let v_pin = ctx.pin_native_root(v);
        if class == "java/lang/Object" {
            let v = ctx.read_native_pin(v_pin, v);
            let _ = ctx.invoke_special(class, "<init>", "()V", &[Value::Object(Some(v))]);
        }
        obj = ctx.read_native_pin(obj_pin, obj);
        let v = ctx.read_native_pin(v_pin, v);
        ctx.set_field_by_name(obj, field, Value::Object(Some(v)));
        ctx.unpin_native_roots(v_pin);
    }
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Some(obj)
}

/// Record the packages of the platform-loader modules, for the mirror allocator.
pub fn publish_platform_packages(ctx: &mut dyn NativeContext) {
    let Some(sets) = jdk_builtin_module_sets(ctx) else {
        return;
    };
    let mut pkgs = std::collections::HashSet::new();
    for module in sets.platform.iter() {
        for p in ctx.module_packages(module) {
            pkgs.insert(p.replace('.', "/"));
        }
    }
    // The run-time image's application-loader modules (wave 100, lane L3; see
    // `adopted_builtin_loader_for_tag`): every listed JDK system module that
    // `ModuleLoaderMap` gives to neither the boot nor the platform loader
    // (the BOOTED image's, wave 102 lane L5: `is_image_module`, so a JDK 21
    // image module JDK 25 dropped counts too).
    let mut app_pkgs = std::collections::HashSet::new();
    for module in ctx.module_names() {
        if sets.boot.contains(&module)
            || sets.platform.contains(&module)
            || ctx.module_is_class_path_only(&module)
            || !crate::jboss_jdkspecific::is_image_module(&*ctx, &module)
        {
            continue;
        }
        for p in ctx.module_packages(&module) {
            app_pkgs.insert(p.replace('.', "/"));
        }
    }
    with_loader_singletons(loader_tables(ctx), |row| {
        row.platform_packages = pkgs;
        row.app_image_packages = app_pkgs;
    });
}

/// `ClassLoader.nameAndId` of a built-in loader this VM mints itself (the
/// `--compatible` application and platform loaders, allocated without the
/// JDK constructor, which computes it): `text`, the quoted name with no
/// ` @hash` (`ClassLoader.nameAndId` adds none for a `BuiltinClassLoader`).
/// The JDK's own `ProxyBuilder.ensureVisible` message
/// (`JLA.getLoaderNameID(loader)`, a read of this field) printed `null` for
/// them under `--compatible` where HotSpot prints `'app'` / `'platform'`
/// (`L3W137ProxyEnsureVisible`, orchestrator's w138 run). Only a null field
/// is written. `CRATONVM_BUILTIN_LOADER_NAME_AND_ID=0` leaves it null.
/// Interpreter round i7 wave 139, lane L3. `obj` must be pinned at `obj_pin`.
pub fn set_builtin_loader_name_and_id(
    ctx: &mut dyn NativeContext,
    obj_pin: usize,
    obj: ObjectRef,
    text: &str,
) {
    if ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(obj), "nameAndId").is_none() {
        return;
    }
    if !matches!(ctx.get_field_by_name(obj, "nameAndId"), Value::Object(None)) {
        return;
    }
    let s = ctx.create_string_uninterned(text);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field_by_name(obj, "nameAndId", Value::Object(Some(s)));
}

/// Get or create the singleton platform class loader.
pub fn get_or_create_platform_loader(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(obj) = platform_loader_of(loader_tables(ctx)) {
        return Ok(obj);
    }
    if real_builtin_loaders_enabled(ctx) {
        if let Some(obj) = adopt_real_builtin_loader(ctx, "platformClassLoader", LOADER_PLATFORM) {
            set_platform_loader(loader_tables(ctx), Some(obj));
            with_loader_singletons(loader_tables(ctx), |row| {
                row.adopted = true;
                // An app loader minted while the JDK's own was still being
                // built (re-entry) points at a look-alike parent: drop it.
                row.app = None;
            });
            publish_platform_packages(ctx);
            let pin = ctx.pin_native_root(obj);
            init_loader_native_libraries(ctx, pin, obj);
            let obj = ctx.read_native_pin(pin, obj);
            ctx.unpin_native_roots(pin);
            return Ok(obj);
        }
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
    set_builtin_loader_name_and_id(ctx, obj_pin, obj, "'platform'");
    // Platform's parent is bootstrap (null); already set by alloc_classloader
    obj = ctx.read_native_pin(obj_pin, obj);
    set_platform_loader(loader_tables(ctx), Some(obj));
    // After the cache write, for the re-entry reason `get_or_create_app_loader`
    // gives.
    init_loader_native_libraries(ctx, obj_pin, obj);
    let obj = ctx.read_native_pin(obj_pin, obj);
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
pub fn latest_user_defined_loader_class(
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
    if let Some(obj) = app_loader_of(loader_tables(ctx)) {
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
        set_app_loader(loader_tables(ctx), None);
    }
    if real_builtin_loaders_enabled(ctx) {
        // Platform first: the app loader's parent must be the adopted one.
        get_or_create_platform_loader(ctx)?;
        if let Some(obj) = adopt_real_builtin_loader(ctx, "appClassLoader", LOADER_APP) {
            set_app_loader(loader_tables(ctx), Some(obj));
            with_loader_singletons(loader_tables(ctx), |r| r.adopted = true);
            if with_loader_singletons(loader_tables(ctx), |r| r.platform_packages.is_empty()) {
                publish_platform_packages(ctx);
            }
            let pin = ctx.pin_native_root(obj);
            // ONE unnamed module for the application loader. `Class.getModule()` of every classpath class
            // answers the VM's canonical unnamed module, while the JDK's `ClassLoader.getUnnamedModule()` body
            // (retired from the native shadow: it now runs) answers the adopted loader's OWN `unnamedModule`
            // field; two objects meant `Foo.class.getModule() == loader.getUnnamedModule()` flipped between
            // true and false by call (the first call went through the native door, later ones through the
            // body), `Package.getPackageInfo`'s `c.getModule() == module` failed, and `Package.getAnnotation`
            // answered null: JAXB lost package-level `@XmlSchema` (jOOQ `Settings` read as defaults).
            let obj = ctx.read_native_pin(pin, obj);
            unify_app_unnamed_module(ctx, obj);
            let obj = ctx.read_native_pin(pin, obj);
            ctx.set_static_field_by_name("java/lang/ClassLoader", "scl", Value::Object(Some(obj)));
            let obj = ctx.read_native_pin(pin, obj);
            init_loader_native_libraries(ctx, pin, obj);
            let obj = ctx.read_native_pin(pin, obj);
            ctx.unpin_native_roots(pin);
            return Ok(obj);
        }
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
    set_builtin_loader_name_and_id(ctx, obj_pin, obj, "'app'");
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
    set_app_loader(loader_tables(ctx), Some(obj));
    // After `set_app_loader`: `NativeLibraries.newInstance` runs Java, and a
    // re-entry into this function must find the cached loader, not mint a
    // second one.
    init_loader_native_libraries(ctx, obj_pin, obj);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(platform_pin);
    Ok(obj)
}

/// `ClassLoader.libraries` -- the loader's own `NativeLibraries`, which the
/// real JDK sets in a FIELD INITIALISER (`private final NativeLibraries
/// libraries = NativeLibraries.newInstance(this);`) that runs inside every
/// `ClassLoader` constructor. The built-in app/platform loaders are minted here
/// without a constructor, and user loaders go through the `<init>` bridges in
/// `classloader_real.rs`, so the field stayed null and the real
/// `ClassLoader.loadLibrary` (`Runtime.load0`/`loadLibrary0` once their
/// shadows are retired) NPE'd on `libs.loadLibrary(..)` (SBR wave 4 FJP's
/// `RJdkJni` dial red; SBR wave 5 FJP2 fixes it).
///
/// `--real-jdk` only: `--compatible` keeps the `Runtime`/`System` load natives,
/// which never read the field, and stays byte-for-byte unchanged. Runs Java
/// (`NativeLibraries.<clinit>` on first use), so `loader` must be pinned at
/// `loader_pin`; a failure leaves the field as it was (null), which is the
/// pre-existing state.
pub fn init_loader_native_libraries(
    ctx: &mut dyn NativeContext,
    loader_pin: usize,
    loader: ObjectRef,
) {
    // `--real-jdk`, or `--compatible` with the `JdkRuntime` row (SBR round 5,
    // lane compat2): `Runtime.load0` / `loadLibrary0` are the JDK's bodies.
    if !ctx.aligned(cratonvm_types::HotSpotAlignment::JdkRuntime) {
        return;
    }
    let Some(idx) = ctx.resolve_field_index("java/lang/ClassLoader", "libraries") else {
        return;
    };
    let loader = ctx.read_native_pin(loader_pin, loader);
    if matches!(ctx.get_field(loader, idx), Value::Object(Some(_))) {
        return;
    }
    if let Ok(Some(Value::Object(Some(libs)))) = ctx.invoke(
        "jdk/internal/loader/NativeLibraries",
        "newInstance",
        "(Ljava/lang/ClassLoader;)Ljdk/internal/loader/NativeLibraries;",
        &[Value::Object(Some(loader))],
    ) {
        // No allocation between the return and the store.
        let loader = ctx.read_native_pin(loader_pin, loader);
        ctx.set_field(loader, idx, Value::Object(Some(libs)));
    }
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
pub fn set_both_parent_fields(ctx: &mut dyn NativeContext, loader: ObjectRef, parent: ObjectRef) {
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
pub const LOADER_BOOTSTRAP: i32 = 0;
pub const LOADER_PLATFORM: i32 = 1;
pub const LOADER_APP: i32 = 2;
pub const LOADER_CUSTOM: i32 = 3;

// ClassLoader synthetic field indices (7 fields)
pub const CL_LOADER_TYPE: usize = 0;
pub const CL_PARENT_REF: usize = 1;
pub const CL_NAME_REF: usize = 2;
pub const CL_CLASSES_LOADED: usize = 3;
/// 1 = this loader is parallel-capable (the CratonVM default — see
/// `cl_is_registered_as_parallel_capable`), 0 = not. Must stay in agreement
/// with `registerAsParallelCapable()`, which always reports success.
pub const CL_IS_PARALLEL_CAPABLE: usize = 4;
pub const CL_DEFAULT_DOMAIN: usize = 5;
/// Unique loader ID for class namespace isolation (0 = not yet assigned)
pub const CL_LOADER_ID: usize = 6;
pub const CL_FIELD_COUNT: usize = 7;

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
pub struct LoaderMeta {
    /// `LOADER_BOOTSTRAP` / `LOADER_PLATFORM` / `LOADER_APP` / `LOADER_CUSTOM`.
    pub loader_type: Option<i32>,
    /// Count of classes defined through this loader's `defineClass` natives.
    pub classes_loaded: Option<i32>,
    /// `registerAsParallelCapable()` bookkeeping.
    pub parallel_capable: Option<bool>,
    /// CratonVM class-namespace id (`0` = the shared application namespace).
    pub loader_id: Option<u32>,
}

impl LoaderMeta {
    /// What the three `ClassLoader` constructor natives record: a custom
    /// loader that has defined nothing yet and is parallel-capable, with a
    /// namespace id only for the `(String, ClassLoader)` form (the other two
    /// leave the id unassigned until `loader_namespace_id` needs one).
    pub fn custom_initialiser(loader_id: Option<u32>) -> Self {
        LoaderMeta {
            loader_type: Some(LOADER_CUSTOM),
            classes_loaded: Some(0),
            parallel_capable: Some(true),
            loader_id,
        }
    }
}

/// Loader-object → [`LoaderMeta`] ([`LoaderIdentityTables::loader_meta`]),
/// one row per loader. A `Vec` rather than a map for the same reason the
/// namespace table is one: entries are keyed by a raw heap address that the
/// GC rewrites, and `retain_mut` over a `Vec` remaps in place where a hash map
/// would have to be rebuilt. Loader counts are in the tens even for a servlet
/// container.
///
/// The table is the owning VM's (gc-common w9-a qualified each row by the VM;
/// interpreter round i5 wave 101, lane L5 made the table itself the VM's). A
/// loader address means something only in the heap that allocated it: every
/// lookup goes to the caller's own VM's table, the GC pass judges and remaps
/// only that table, and the table dies with its VM.
//
// LEVEL (lock-discipline ratchet): `Scratch` is L0, the bottom of the
// hierarchy — a thread holding it may acquire NOTHING else. That is a
// claim about every critical section, and each was checked against it:
// `clear` (a detached instance's teardown), `find`/`push` (`loader_meta_put`),
// `find`/copy (`loader_meta_get`), one caller-supplied closure whose only
// caller assigns a field (`loader_meta_upsert`), the test helper's `retain`,
// and the GC pass's `retain_mut` — which reads `dead` and `pointer_map`, both
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
//
// Record (or replace) `loader`'s bookkeeping in the VM's table.
pub fn loader_meta_put(tables: &LoaderIdentityTables, loader: ObjectRef, meta: LoaderMeta) {
    let mut t = tables.loader_meta.lock();
    match t.meta_position(tables.rows_generation(), loader) {
        Some(i) => t.rows[i].1 = meta,
        None => t.push((loader, meta)),
    }
}

/// Read `loader`'s recorded bookkeeping, if the VM has any.
pub fn loader_meta_get(tables: &LoaderIdentityTables, loader: ObjectRef) -> Option<LoaderMeta> {
    let mut t = tables.loader_meta.lock();
    let i = t.meta_position(tables.rows_generation(), loader)?;
    Some(t.rows[i].1)
}

/// Read-modify-write, creating an all-`None` entry if the loader has none.
pub fn loader_meta_upsert(
    tables: &LoaderIdentityTables,
    loader: ObjectRef,
    f: impl FnOnce(&mut LoaderMeta),
) {
    let mut t = tables.loader_meta.lock();
    match t.meta_position(tables.rows_generation(), loader) {
        Some(i) => f(&mut t.rows[i].1),
        None => {
            let mut meta = LoaderMeta::default();
            f(&mut meta);
            t.push((loader, meta));
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
pub const REAL_CLASSLOADER_WITNESS_FIELDS: [&str; 3] =
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
pub fn cl_has_synthetic_layout(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(loader);
    // `CRATONVM_LOADER_NAMESPACE_INDEX` (interpreter round i3 wave 83, lane
    // L5): a class already found on the real layout answers from the per-VM
    // memo instead of a by-name field walk up its hierarchy; only that verdict
    // is memoized (see `LoaderNamespaceIndex`).
    let memo = ctx.loader_namespace_index();
    if memo
        && archive_bytes_caches(ctx)
            .is_some_and(|caches| caches.loader_namespaces.read().is_real_layout(cid.as_u32()))
    {
        return false;
    }
    let synthetic = !REAL_CLASSLOADER_WITNESS_FIELDS
        .iter()
        .any(|name| ctx.resolve_field_index_by_class_id(cid, name).is_some());
    if memo && !synthetic {
        if let Some(caches) = archive_bytes_caches(ctx) {
            caches
                .loader_namespaces
                .write()
                .note_real_layout(cid.as_u32());
        }
    }
    synthetic
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
pub fn loader_type_of(ctx: &dyn NativeContext, loader: ObjectRef) -> Option<i32> {
    if let Some(t) = loader_meta_get(loader_tables(ctx), loader).and_then(|m| m.loader_type) {
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
pub fn loader_classes_loaded_of(ctx: &dyn NativeContext, loader: ObjectRef) -> Option<i32> {
    if let Some(n) = loader_meta_get(loader_tables(ctx), loader).and_then(|m| m.classes_loaded) {
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
pub fn loader_parallel_capable_of(
    ctx: &dyn NativeContext,
    loader: ObjectRef,
) -> Option<i32> {
    if let Some(p) = loader_meta_get(loader_tables(ctx), loader).and_then(|m| m.parallel_capable) {
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
pub fn loader_id_of(ctx: &dyn NativeContext, loader: ObjectRef) -> Option<u32> {
    if let Some(id) = loader_meta_get(loader_tables(ctx), loader).and_then(|m| m.loader_id) {
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
pub fn loader_set_classes_loaded(ctx: &mut dyn NativeContext, loader: ObjectRef, n: i32) {
    loader_meta_upsert(loader_tables(ctx), loader, |m| m.classes_loaded = Some(n));
    if cl_has_synthetic_layout(ctx, loader) {
        ctx.set_field(loader, CL_CLASSES_LOADED, Value::Int(n));
    }
}

// URLClassLoader synthetic field indices (6 fields)
pub const UCL_LOADER_TYPE: usize = 0;
pub const UCL_PARENT_REF: usize = 1;
pub const UCL_URL_COUNT: usize = 2;
pub const UCL_CLOSED: usize = 3;
/// Array of URL objects added via addURL / constructor
pub const UCL_URLS_ARRAY: usize = 4;
/// Unique loader ID for class namespace isolation
pub const UCL_LOADER_ID: usize = 5;
pub const UCL_FIELD_COUNT: usize = 6;

// MethodHandles$Lookup synthetic field indices (4 fields)
pub const LK_LOOKUP_CLASS_REF: usize = 0;
pub const LK_ALLOWED_MODES: usize = 1;
pub const LK_PREVIOUS_LOOKUP_CLASS: usize = 2;
pub const LK_LOOKUP_MODE: usize = 3;
pub const LK_FIELD_COUNT: usize = 4;

// Lookup mode bitmask constants
pub const LK_PUBLIC: i32 = 0x01;
pub const LK_PRIVATE: i32 = 0x02;
pub const LK_PROTECTED: i32 = 0x04;
pub const LK_PACKAGE: i32 = 0x08;
pub const LK_MODULE: i32 = 0x10;
pub const LK_UNCONDITIONAL: i32 = 0x20;
pub const LK_ORIGINAL: i32 = 0x40;
pub const LK_FULL_POWER: i32 =
    LK_PUBLIC | LK_PRIVATE | LK_PROTECTED | LK_PACKAGE | LK_MODULE | LK_ORIGINAL;

// HiddenClass synthetic field indices (2 fields)
pub const HC_NEST_HOST_REF: usize = 0;
pub const HC_CLASS_DATA_REF: usize = 1;
pub const HC_FIELD_COUNT: usize = 2;

// ProtectionDomain synthetic field indices (3 fields)
pub const PD_CODE_SOURCE_REF: usize = 0;
pub const PD_PERMISSIONS_REF: usize = 1;
pub const PD_CLASS_LOADER_REF: usize = 2;
pub const PD_FIELD_COUNT: usize = 3;

// CodeSource synthetic field indices (2 fields)
pub const CS_LOCATION_REF: usize = 0;
pub const CS_CERTIFICATES_REF: usize = 1;
pub const CS_FIELD_COUNT: usize = 2;

// ---------------------------------------------------------------------------
// ClassLoader delegation model helpers
// ---------------------------------------------------------------------------

/// Determines delegation order for class loading.
pub fn delegation_order(loader_type: i32) -> &'static str {
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

pub const CL_CLASS: &str = "java/lang/ClassLoader";
pub const UCL_CLASS: &str = "java/net/URLClassLoader";
pub const LK_CLASS: &str = "java/lang/invoke/MethodHandles$Lookup";
pub const HC_CLASS: &str = "java/lang/ClassLoader$HiddenClass";
pub const PD_CLASS: &str = "java/security/ProtectionDomain";
pub const CS_CLASS: &str = "java/security/CodeSource";

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
pub fn alloc_default_protection_domain(
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



pub fn alloc_classloader(
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
    // `classes`: the real constructor's `new ArrayList<>()`. `ClassLoader.addClass` is `synchronized
    // (classes) { classes.add(c); }` and the JDK's own `MethodHandles.Lookup.defineClass`
    // (`ClassDefiner.defineClass`, what ByteBuddy's `ClassInjector.UsingLookup` -- Mockito's inline mock
    // maker -- ends in) calls it on the target loader: left null, every Mockito mock of an application
    // class failed with `NullPointerException: Cannot enter synchronized block because "this.classes" is
    // null` under `--compatible`. Real layout only: on ours slot 6 is the loader id.
    if !synthetic_layout {
        let classes = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
        let classes_pin = ctx.pin_native_root(classes);
        let _ = ctx.invoke_special(
            "java/util/ArrayList",
            "<init>",
            "()V",
            &[Value::Object(Some(classes))],
        );
        obj = ctx.read_native_pin(obj_pin, obj);
        let classes = ctx.read_native_pin(classes_pin, classes);
        ctx.unpin_native_roots(classes_pin);
        ctx.set_field_by_name(obj, "classes", Value::Object(Some(classes)));
    }
    // zt t24/df11 (define page §9d): `package2certs`, the real constructor's `new
    // ConcurrentHashMap<>()`. `ClassLoader.preDefineClass` calls `checkCerts(name, cs)`, whose body
    // is `package2certs.putIfAbsent(pname, certs)`: left null, every Java `defineClass` through a
    // minted built-in loader (the `--compatible` application and platform loaders) threw
    // `NullPointerException: ... because "this.package2certs" is null` where HotSpot defines the
    // class. Real layout only (on ours slot 5 is the default domain);
    // `CRATONVM_MINTED_LOADER_PACKAGE2CERTS=0` leaves it null (the positive control).
    if !synthetic_layout {
        let certs =
            try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
        obj = ctx.read_native_pin(obj_pin, obj);
        ctx.set_field_by_name(obj, "package2certs", Value::Object(Some(certs)));
    }
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
    // `L5W28BuiltinLoaderLock`). `--real-jdk`, real layout only; since wave
    // 48 (lane L5) also `--compatible` running the JDK's parallel-capable
    // loader model (`NativeContext::faithful_loader_locks`). Interpreter round
    // i1 wave 28, lane L5.
    if !synthetic_layout
        && ctx.faithful_loader_locks()
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
        loader_tables(ctx),
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
pub fn get_or_assign_loader_id(ctx: &mut dyn NativeContext, cl: ObjectRef) -> u32 {
    loader_namespace_id(ctx, cl)
}

pub fn alloc_url_classloader(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
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

pub fn alloc_lookup(ctx: &mut dyn NativeContext, modes: i32) -> Result<ObjectRef, MethodCallFailed> {
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
pub fn lk_real_allowed_modes_slot(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<usize> {
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
pub fn lk_real_prev_lookup_class_slot(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<usize> {
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
pub fn lk_set_modes(ctx: &mut dyn NativeContext, obj: ObjectRef, modes: i32) {
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
pub fn lk_modes_of(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
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
pub const LK_FULL_POWER_MODES: i32 = LK_PUBLIC | LK_PRIVATE | LK_PROTECTED | LK_PACKAGE | LK_MODULE;

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
/// `pub` so `lang_invoke`'s competing `Lookup.in` registration (which
/// WINS in real-JDK mode — see the note in `register_classloader_natives`) can
/// adopt this arithmetic instead of keeping a second, differently-wrong copy.
pub fn lk_in_modes(
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
pub fn lk_drop_modes(old: i32, drop: i32) -> Option<i32> {
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

pub fn cl_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // L1: record before allocating anything — `this` is the live receiver at
    // this point, and the table is keyed by address.
    loader_meta_put(
        loader_tables(ctx),
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

pub fn cl_init_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let parent = args.get(1).copied().unwrap_or(Value::Object(None));
    // L1: see `cl_init_default`.
    loader_meta_put(
        loader_tables(ctx),
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

pub fn cl_init_name_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
        loader_tables(ctx),
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
pub fn is_builtin_loader_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/lang/ClassLoader" | "java/net/URLClassLoader" | "java/security/SecureClassLoader"
    ) || class_name.starts_with("jdk/internal/loader/")
        || class_name.starts_with("sun/misc/Launcher$")
}

/// The loader class `ModuleLayer.defineModulesWithOneLoader` /
/// `…WithManyLoaders` create for the modules of a new layer.
pub const JDK_LAYER_LOADER_CLASS: &str = "jdk/internal/loader/Loader";

/// [`is_builtin_loader_class`] as the VM `ctx` sees it. Under `--real-jdk`
/// the JDK's layer loader (`jdk.internal.loader.Loader`) is NOT a built-in
/// loader: HotSpot treats it as any other `ClassLoader` subclass, and its own
/// `loadClass(String, boolean)` / `findClass` / resource overrides are what
/// map a package to the module of the layer. The `jdk/internal/loader/`
/// prefix matched it, so `ClassLoader.loadClass` never ran its override (base
/// parent-first delegation answered `ClassNotFoundException` for every class
/// of the layer's modules) and its defines went to the application namespace
/// (interpreter round i1 wave 43, lane L5,
/// `i42-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module`,
/// probe `tools/probes/interp/L5/L5W43UserLayerModule.java`). Both modes
/// since wave 48 (lane L6: `--compatible` records a layer's modules by
/// identity too, and a layer's classes were `ClassNotFoundException` there).
pub fn is_builtin_loader_class_in(_ctx: &dyn NativeContext, class_name: &str) -> bool {
    if class_name == JDK_LAYER_LOADER_CLASS {
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
pub fn is_bootstrap_class_name(internal: &str) -> bool {
    internal.starts_with("java/")
        || internal.starts_with("javax/")
        || internal.starts_with("jdk/")
        // `sun/reflect/misc/Trampoline` included (interpreter round i7 wave
        // 138, lane L5): it is a `java.base` class the boot loader serves like
        // any other. `MethodUtil`'s own copy is a second class of the name in
        // MethodUtil's namespace (its `loadClass` override never delegates for
        // it), and the bootstrap-namespace doors answer the boot copy exactly
        // (`bootstrap_class_id_exact`). Its copy's references to its own
        // name resolve to itself (`constants.rs` `self_name_reference`, wave
        // 139), so no exception is left.
        || internal.starts_with("sun/")
        || internal.starts_with("com/sun/")
        || internal.starts_with("org/w3c/dom")
        || internal.starts_with("org/xml/sax")
        || internal.starts_with("org/ietf/jgss")
        || internal.starts_with("org/jcp/xml")
        || internal.starts_with("[")
}

/// Whether the bootstrap or platform loader can define `internal`: under
/// `--real-jdk` with a runtime image, exactly the classes the image defines
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
/// (`docs/feature-designs/native-dispatch-architecture.md`
/// `spring-jdkonly-bytecode-cast-name-rule-jit-class-resolution-FIXED-20260923.md`).
pub fn is_runtime_image_class_name(ctx: &dyn NativeContext, internal: &str) -> bool {
    // No `sun/reflect/misc/Trampoline` exception any more (interpreter round
    // i7 wave 138, lane L5; see `is_bootstrap_class_name`): a built-in or
    // null-parented loader's delegation for it reaches the boot copy, as
    // `ClassLoader.getSystemClassLoader().loadClass(..)` does on HotSpot.
    // The image's own index answers, not the package-prefix heuristic below:
    // `--real-jdk`, and `--compatible` with the `runtime-image-names` row
    // (interpreter round i1 wave 50, lane L5). The heuristic calls every
    // `javax/` name the image's, so a null- or platform-parented loader was
    // "transparent" for `javax.l5w40.Lazy` (on the class path, not in the
    // image) and never asked (probe `L5W40BootParentGlobalName`).
    if ctx.aligned(cratonvm_types::HotSpotAlignment::RuntimeImageNames)
        && !internal.starts_with('[')
    {
        if let Some(defined) = ctx.runtime_image_defines_class(internal) {
            if defined != is_bootstrap_class_name(internal) {
                // The prefix heuristic would have answered otherwise (wave
                // 51, lane L5: the alignment census).
                ctx.note_alignment_divergence(cratonvm_types::HotSpotAlignment::RuntimeImageNames);
            }
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
pub fn is_loader_aware_resolution_eligible(
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
pub fn cl_bootstrap_scoped() -> bool {
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
/// `pub` (not `pub`): also consulted by `vm::memory::roots` /
/// `vm::memory::gc` to gate rooting/reconciliation of the `SharedVm::class_mirrors`
/// cache the same way — a `java.lang.Class` mirror's `classLoader` field is a
/// real heap edge, so unconditionally rooting a user-defined class's mirror
/// keeps its loader alive forever too, defeating this gate for any loader that
/// ever had a class reflected on (`getClass()`, annotations, ...).
pub fn loader_unload_enabled() -> bool {
    crate::nbflags().loader_unload
}

/// zt t21/tg10: [`loader_unload_enabled`] for one VM: also `false` when that
/// VM was made with HotSpot's `-XX:-ClassUnloading` / `-Xnoclassgc`
/// ([`LoaderIdentityTables::class_unloading`]). Then the defining-loader rows
/// are GC roots ([`gc_scan_loader_singleton_roots`]), no hidden class is
/// registered to unload with its mirror ([`register_non_strong_hidden_class`])
/// and the VM's root scan roots every class mirror and static
/// (`vm::memory::roots::conditional_loader_metadata`): the
/// `CRATONVM_LOADER_UNLOAD=0` shape, per VM.
pub fn loader_unload_enabled_for(tables: &LoaderIdentityTables) -> bool {
    loader_unload_enabled() && tables.class_unloading()
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
/// this crate's `#[inline]`/`pub` call sites and doc cross-references
/// do not need to change.
///
/// When off, every loader-identity path keeps its exact pre-gate behavior.
/// Empty / `"0"` ⇒ off; any other value ⇒ on.
#[inline]
pub fn loader_aware_resolution() -> bool {
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
pub fn receiver_overrides_find_class(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
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
            // URLs at all -- see docs/feature-designs/native-dispatch-architecture.md.
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
pub fn find_class_is_urlclassloader_native(
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
pub fn receiver_overrides_load_class(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    descriptor: &str,
) -> bool {
    let receiver = ctx.class_id_of_object(this);
    // JIT round 23 wave 6 (`CRATONVM_LOADER_LOAD_CLASS_OVERRIDE_MEMO`, default
    // ON): the walk below reads only stored classes by id (superclass edges,
    // names, declared methods), so its answer is kept per receiver class and
    // overload under the context's hierarchy stamp. Mock contexts have no
    // stamp and walk every time.
    let memo = load_class_override_memo_key(ctx, descriptor);
    if let Some((stamp, overload)) = memo {
        if let Some(answer) =
            loader_tables(&*ctx).load_class_override_lookup(stamp, receiver.as_u32(), overload)
        {
            return answer;
        }
    }
    let answer = receiver_overrides_load_class_walk(ctx, receiver, descriptor);
    if let Some((stamp, overload)) = memo {
        loader_tables(&*ctx).load_class_override_record(stamp, receiver.as_u32(), overload, answer);
    }
    answer
}

