// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ClassLoader natives for real-JDK mode.
//!
//! The real JDK `ClassLoader.<init>` is extremely complex — it creates
//! ArrayList, ProtectionDomain, NativeLibraries, Module, ConcurrentHashMap,
//! and more. Many of these depend on JDK internals we don't fully support.
//!
//! These simplified natives use `set_field_by_name`/`get_field_by_name` so
//! they work with the real JDK field layout (not hardcoded synthetic indices).

use cratonvm_native_api::registry::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ObjectRef, Value};

use crate::try_alloc_concurrent_synthetic;
use cratonvm_types::error::MethodCallFailed;

/// Normalise a raw URL/path string to a filesystem path the classpath
/// loader can resolve.
///
/// Handles `file:` URLs (`file:/C:/dir/x.jar`), bare paths, and the
/// Windows leading-slash-before-drive quirk (`/C:/dir` → `C:/dir`),
/// which otherwise makes `PathBuf::from(...)` fail every `is_dir()` /
/// `exists()` probe on Windows.
fn normalise_url_path(raw: &str) -> String {
    let p = raw.strip_prefix("file:").unwrap_or(raw);
    let p = p.strip_prefix("//").unwrap_or(p);
    let bytes = p.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        p[1..].to_string()
    } else {
        p.to_string()
    }
}

/// Extract a filesystem path from a `java.net.URL` object.
///
/// Reads the URL's `path`/`file` fields by NAME (works for both real-JDK
/// URL objects and CratonVM's synthetic URLs), falling back to the URL's
/// `toString()`-style full string.
fn extract_url_path(ctx: &dyn NativeContext, url_obj: ObjectRef) -> Option<String> {
    for field in ["path", "file"] {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(url_obj, field) {
            if let Some(raw) = ctx.read_string(s) {
                if !raw.is_empty() {
                    return Some(normalise_url_path(&raw));
                }
            }
        }
    }
    ctx.read_string(url_obj).map(|raw| normalise_url_path(&raw))
}

/// Register every URL in a `URL[]` array with the dynamic application
/// classpath so classes inside those jars/dirs become loadable.
fn register_url_array(ctx: &mut dyn NativeContext, this: ObjectRef, urls: Value) {
    // gc-common w14 (orchestrator): the debug listing below reads the array
    // and allocates nothing, so it runs FIRST. `record_ucl_urls` can allocate
    // (and move `this` and the array), and nothing here uses either after it.
    // The listing used to run after it and read the pre-call array
    // (`scripts/stale-receiver-audit.py`).
    if crate::nbflags().dbg_uclreg {
        match urls {
            Value::Object(Some(arr)) => {
                let count = ctx.array_length(arr);
                let mut paths = Vec::with_capacity(count);
                for i in 0..count {
                    if let Value::Object(Some(url_obj)) = ctx.get_array_element(arr, i) {
                        match extract_url_path(ctx, url_obj) {
                            Some(p) if !p.is_empty() => paths.push(p),
                            Some(_) => {}
                            None => eprintln!("[UCLREG-DBG]   url[{i}] extract_url_path -> None"),
                        }
                    } else {
                        eprintln!("[UCLREG-DBG]   url[{i}] not an object");
                    }
                }
                eprintln!("[UCLREG-DBG] <init> count={count} extracted={:?}", paths);
            }
            _ => eprintln!("[UCLREG-DBG] <init> urls arg not an array: {urls:?}"),
        }
    }
    // Make `getURLs()` return the loader's URLs (stashed on the `ucp`
    // placeholder). Without this, real-mode `getURLs()` falls through to the
    // empty `URLClassPath` shim and a classloader's URLs are invisible to
    // callers like Tomcat's `StandardJarScanner`.
    //
    // A URLClassLoader owns an isolated search path. Publishing constructor
    // URLs into CratonVM's process-wide application path made later temporary
    // loaders observe resources from earlier ones. The URLClassLoader natives
    // resolve from the recorded per-loader URLs, so do not flatten them here.
    crate::classloader::record_ucl_urls(ctx, this, urls);
}

/// Populate the instance fields that the real JDK `ClassLoader.<init>`
/// field-initialisers / constructor body would set, but which our
/// simplified real-JDK `<init>` natives previously skipped.
///
/// The real JDK `ClassLoader(Void, String, ClassLoader)` private
/// constructor assigns several `final` fields inline — most importantly
/// `defaultDomain = new ProtectionDomain(new CodeSource(null, null),
/// null, this, null)`. When a custom loader (e.g. a `ClassLoader`
/// subclass that calls `super(parent)`) is constructed through one of
/// our `<init>` natives, that field initialiser never runs, so
/// `defaultDomain` stays null.
///
/// `ClassLoader.defineClass(...)` → `preDefineClass` then does
/// `pd = defaultDomain; checkCerts(name, pd.getCodeSource())` and NPEs
/// with "Cannot invoke getCodeSource on null" — the exact failure that
/// blocked the cglib probe (a `ClassLoader` subclass calling
/// `defineClass`). Build the same non-null `defaultDomain` shape here
/// so the real JDK `defineClass` bytecode path runs cleanly.
fn init_classloader_common_fields(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), MethodCallFailed> {
    // GC-safety: every `alloc_concurrent_synthetic`/`new_object`/`invoke` call
    // below can trigger a moving GC; `this` (and, briefly, `cs`) are each
    // reused repeatedly across multiple such hazards, unpinned otherwise.
    // Same "Family 1" stale-ObjectRef pattern as the WildFly boot-crash
    // fixes (see wildfly-parallel-boot-stale-objectref-residual.md)
    // -- pin both now and re-read the forwarded reference right before use.
    let this_pin = ctx.pin_native_root(this);
    // defaultDomain → ProtectionDomain(CodeSource(null URL, null certs),
    // null perms, this loader, null principals). Build CodeSource first.
    let cs = try_alloc_concurrent_synthetic(ctx, "java/security/CodeSource", 2)?;
    let cs_pin = ctx.pin_native_root(cs);
    ctx.set_field_by_name(cs, "location", Value::Object(None));
    ctx.set_field_by_name(cs, "certs", Value::Object(None));

    let pd = try_alloc_concurrent_synthetic(ctx, "java/security/ProtectionDomain", 4)?;
    let cs = ctx.read_native_pin(cs_pin, cs);
    ctx.unpin_native_roots(cs_pin);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(pd, "codesource", Value::Object(Some(cs)));
    ctx.set_field_by_name(pd, "permissions", Value::Object(None));
    ctx.set_field_by_name(pd, "classloader", Value::Object(Some(this)));
    ctx.set_field_by_name(pd, "principals", Value::Object(None));
    // `hasAllPerm`/`staticPermissions` are booleans; default 0 is fine.
    ctx.set_field_by_name(this, "defaultDomain", Value::Object(Some(pd)));

    // `classes` — ArrayList the JDK uses to pin loaded classes. JDK code
    // (`addClass`) does `synchronized (classes) { classes.add(c); }`; a
    // null here would NPE on monitorenter.
    // 2 = the table's own count. Nothing here writes a slot; the list is
    // handed to `ClassLoader.classes` and filled through `add`. See the T9C
    // gate: a literal wider than the table reads as an indexed shape.
    let classes = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "classes", Value::Object(Some(classes)));

    // `packages` — ConcurrentHashMap; `ClassLoader.packages()` does
    // `getfield packages → values()` and would NPE on null.
    let packages =
        try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "packages", Value::Object(Some(packages)));

    // `package2certs` — ConcurrentHashMap consulted by `checkCerts`.
    let pkg2certs =
        try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "package2certs", Value::Object(Some(pkg2certs)));

    // `parallelLockMap` — used by `getClassLoadingLock` — and `assertionLock`
    // (`setDefaultAssertionStatus` synchronizes on it). JDK 25's private
    // `ClassLoader(Void, String, ClassLoader)` sets both from
    // `ParallelLoaders.isRegistered(getClass())`: a new map and a new lock
    // object for a registered loader class, else `null` and `this`, so a loader
    // that never called `registerAsParallelCapable()` locks ITSELF in
    // `loadClass` (and in the VM, see `loader_locks_itself`). Under
    // `--jdk-only` the set is the JDK's real one (the constant-`true`
    // `registerAsParallelCapable` shadow is retired there), so it is asked.
    // `--compatible` keeps the map for every loader, unchanged: its
    // `registerAsParallelCapable` native answers `true` without registering
    // and its `SecureClassLoader.<clinit>` is a no-op, so the set there holds
    // no loader class but `ClassLoader` itself and asking it would serialize
    // the loaders HotSpot treats as parallel-capable (`URLClassLoader`'s
    // subclasses that do register). Interpreter round i1 wave 28, lane L5;
    // probe `L5W27LoaderLockShape`.
    let this = ctx.read_native_pin(this_pin, this);
    if ctx.is_jdk_only() && !loader_class_registered_parallel_capable(ctx, this_pin, this) {
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field_by_name(this, "parallelLockMap", Value::Object(None));
        ctx.set_field_by_name(this, "assertionLock", Value::Object(Some(this)));
    } else {
        let lock_map =
            try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)?;
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field_by_name(this, "parallelLockMap", Value::Object(Some(lock_map)));

        let lock = try_alloc_concurrent_synthetic(ctx, "java/lang/Object", 0)?;
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field_by_name(this, "assertionLock", Value::Object(Some(lock)));
    }

    // `pdcache` — `SecureClassLoader`'s `Map<CodeSource, ProtectionDomain>`,
    // declared `private final pdcache = new ConcurrentHashMap<>(11)` and set
    // by `SecureClassLoader.<init>`'s inline field initialiser. Because the
    // simplified URLClassLoader/ClassLoader `<init>` natives bypass the real
    // `SecureClassLoader` constructor, this field stays null. The real-JDK
    // bytecode for `SecureClassLoader.getProtectionDomain(CodeSource)` (reached
    // from `SecureClassLoader.defineClass(name, byte[], …, CodeSource)`) does
    // `pdcache.computeIfAbsent(cs, …)`, so a null `pdcache` throws
    // `NullPointerException: Cannot invoke computeIfAbsent on null`. Concrete
    // breakage: Groovy's `GroovyClassLoader$ClassCollector.createClass` →
    // `defineClass` → `getProtectionDomain` NPE during the Groovy compiler's
    // `class generation` phase (Spring Boot buildSrc `SpringRepositoriesExtension`
    // Groovy script compile). Build a real, segment-initialised CHM via its
    // native `<init>()V` (a bare `alloc_concurrent_synthetic` leaves the segments
    // array null, so `computeIfAbsent` would silently no-op) and only when null
    // so a real ctor that already ran is not clobbered.
    let this = ctx.read_native_pin(this_pin, this);
    if !matches!(
        ctx.get_field_by_name(this, "pdcache"),
        Value::Object(Some(_))
    ) {
        if let Ok(Some(Value::Object(Some(chm)))) =
            ctx.new_object("java/util/concurrent/ConcurrentHashMap")
        {
            let chm_pin = ctx.pin_native_root(chm);
            let _ = ctx.invoke(
                "java/util/concurrent/ConcurrentHashMap",
                "<init>",
                "()V",
                &[Value::Object(Some(chm))],
            );
            let chm = ctx.read_native_pin(chm_pin, chm);
            ctx.unpin_native_roots(chm_pin);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field_by_name(this, "pdcache", Value::Object(Some(chm)));
        }
    }
    if ctx.is_jdk_only() {
        let this = ctx.read_native_pin(this_pin, this);
        set_loader_name_and_id(ctx, this_pin, this);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(())
}

/// `ClassLoader.nameAndId`, as JDK 25's constructor computes it
/// (`ClassLoader.nameAndId(ClassLoader)`): the quoted `name`, or the loader's
/// class name when it has none, then ` @` and the identity hash in hex. It is
/// what HotSpot's `loader_name_and_id` prints for the loader, in a
/// `ClassCastException` among others (interpreter round i1 wave 32). The
/// constructor bridges left it null. Only user loaders reach these bridges:
/// the built-in loaders are allocated without a constructor, and their
/// messages are written without the hash.
///
/// `this` must be pinned at `this_pin`.
fn set_loader_name_and_id(ctx: &mut dyn NativeContext, this_pin: usize, this: ObjectRef) {
    let label = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(n)) => ctx.read_string(n).map(|s| format!("'{s}'")),
        _ => None,
    }
    .or_else(|| {
        ctx.class_name_of_id(ctx.class_id_of_object(this))
            .map(|n| n.replace('/', "."))
    });
    let Some(label) = label else {
        return;
    };
    let text = format!("{label} @{:x}", ctx.identity_hash_code(this) as u32);
    // Computed text: JDK 25's `nameAndId` is a fresh String, never interned
    // (the native string pool is a never-pruned root; see
    // `r13w10-misc10`'s `create_string` ratchet).
    let s = ctx.create_string_uninterned(&text);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "nameAndId", Value::Object(Some(s)));
}

/// `ClassLoader$ParallelLoaders.isRegistered(this.getClass())`, the question
/// JDK 25's `ClassLoader` constructor asks before choosing its locks.
///
/// The loader's class has finished `<clinit>` (the `new` that allocated `this`
/// initialized it), which is where `registerAsParallelCapable()` registers it,
/// so the answer is final. A failed call (no answer) keeps the map: that is
/// what this constructor did before it asked, and a loader wrongly treated as
/// parallel-capable only loses a lock HotSpot would take, where one wrongly
/// treated as NOT parallel-capable gains a lock HotSpot would not.
fn loader_class_registered_parallel_capable(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
) -> bool {
    let this = ctx.read_native_pin(this_pin, this);
    let class_id = ctx.class_id_of_object(this);
    let mirror = ctx.get_class_mirror(class_id);
    !matches!(
        ctx.invoke_static(
            "java/lang/ClassLoader$ParallelLoaders",
            "isRegistered",
            "(Ljava/lang/Class;)Z",
            &[Value::Object(Some(mirror))],
        ),
        Ok(Some(Value::Int(0)))
    )
}

/// Whether a class load through `loader` holds the loader's own monitor, as
/// HotSpot's does for a loader that is not parallel-capable: a user-defined
/// loader under `--jdk-only` whose real `ClassLoader.parallelLockMap` field is
/// null.
///
/// That field is the one signal both halves of the JDK read.
/// `ClassLoader.getClassLoadingLock(name)` returns `this` when it is null, so
/// the JDK's `loadClass(String,boolean)` synchronizes on the loader; HotSpot's
/// `java_lang_ClassLoader::parallelCapable` reads the same field to decide
/// whether `SystemDictionary` holds the loader's monitor (`ObjectLocker`)
/// around a VM-initiated load. Holding the SAME monitor in both places is what
/// keeps the two lock orders from inverting.
///
/// `false` for every other loader, so nothing changes for them:
/// * outside `--jdk-only` (`--compatible` stays byte-for-byte; its
///   constructors give every loader a map anyway);
/// * a loader without the real layout (the synthetic JDK's `ClassLoader`
///   declares no `parallelLockMap`);
/// * the built-in loaders: CratonVM allocates the application and platform
///   loaders without running a constructor, so their field is null although
///   the JDK registers both classes parallel-capable. Locking them would
///   serialize every application class load, which HotSpot never does.
///
/// Interpreter round i1 wave 28, lane L5 (probes `L5W27LoaderLockShape`,
/// `L5W28LoaderMonitorAtLoad`).
pub fn loader_locks_itself(ctx: &mut dyn NativeContext, loader: ObjectRef) -> bool {
    if !ctx.is_jdk_only() {
        return false;
    }
    let class_id = ctx.class_id_of_object(loader);
    let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, "parallelLockMap") else {
        return false;
    };
    if !matches!(
        ctx.get_field(loader, slot),
        Value::Object(None) | Value::Int(0) | Value::Long(0)
    ) {
        return false;
    }
    crate::classloader::is_user_defined_loader(ctx, loader)
}

/// `loader.loadClass(name)` called by a native on the VM's behalf, as HotSpot's
/// `SystemDictionary::resolve_instance_class_or_null` makes the call: with
/// the loader's monitor held when the loader is not parallel-capable
/// ([`loader_locks_itself`], `--jdk-only` user loaders), so a loader whose
/// `loadClass` override does not synchronize is never entered by two
/// VM-initiated loads at once (the duplicate-definition race of
/// `docs/internal/fixed-bugs/interpreter-L5-a-racing-resolution-of-one-class-entry-can-leave-threads-disagreeing-FIXED-20261005.md`).
/// The interpreter's resolution door (`constants.rs`
/// `drive_defining_loader_load_named`) and the base `loadClass` native
/// (`cl_real_load_class_base`) take the same monitor, which is re-entrant.
/// Every other loader, and `--compatible`, is called exactly as before. The
/// loader and the name are pinned across the call (the monitor wait and the
/// Java call are GC points). Interpreter round i1 wave 29, lane L5.
pub fn invoke_load_class_as_the_vm(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    name: ObjectRef,
) -> cratonvm_types::error::MethodCallResult {
    let loader_pin = ctx.pin_native_root(loader);
    let name_pin = ctx.pin_native_root(name);
    let locked = loader_locks_itself(ctx, loader);
    let loader = if locked {
        ctx.monitor_enter_gc_safe(loader)
    } else {
        loader
    };
    let name = ctx.read_native_pin(name_pin, name);
    let result = ctx.invoke_virtual(
        loader,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(name))],
    );
    if locked {
        let loader = ctx.read_native_pin(loader_pin, loader);
        ctx.monitor_exit(loader);
    }
    ctx.unpin_native_roots(loader_pin);
    result
}

/// [`invoke_load_class_as_the_vm`] for a dotted class name not yet a Java
/// `String`: the loader is pinned across the `String` allocation, which three
/// `lang_class.rs` callers used to make with the loader reference unpinned
/// (a collection inside `create_string` left them calling `loadClass` on a
/// stale address). Wave 29, lane L5.
pub fn load_class_named_as_the_vm(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    dotted_name: &str,
) -> cratonvm_types::error::MethodCallResult {
    let loader_pin = ctx.pin_native_root(loader);
    let name = ctx.create_string(dotted_name);
    let loader = ctx.read_native_pin(loader_pin, loader);
    let result = invoke_load_class_as_the_vm(ctx, loader, name);
    ctx.unpin_native_roots(loader_pin);
    result
}

/// Whether `loader`'s `loadClass` is the loader's own bytecode: some class of
/// its chain below the JDK's base (a `URLClassLoader` subclass's override
/// counts, a bare `URLClassLoader` does not) overrides `loadClass(String)` or
/// `loadClass(String,boolean)`. Such a loader's throwable is its own decision
/// (a child-first or filtering loader refusing a name), not a miss of
/// CratonVM's base-delegation native, so a VM-initiated resolution that meets
/// it fails with it, as on HotSpot (`--jdk-only`; interpreter round i1 wave 38,
/// lane L5, `vm/src/runtime/resolve/loader_throw.rs`). Pure: reads declared
/// methods, runs no Java.
pub fn loader_load_class_is_bytecode(ctx: &mut dyn NativeContext, loader: ObjectRef) -> bool {
    crate::classloader::receiver_overrides_load_class_single(ctx, loader)
        || crate::classloader::receiver_overrides_load_class_resolve(ctx, loader)
}

/// Whether a `ClassNotFoundException` that CratonVM's base-delegation
/// `loadClass` native ([`cl_real_load_class_base`]) throws for `internal` on
/// `loader`'s behalf is the loader's own `findClass` decision, not a miss of
/// the native's flat-store emulation: its parent is null or the platform
/// loader, the name is neither a runtime-image nor a bootstrap-appended name,
/// and the loader either overrides `findClass` (the native's
/// `defer_to_find_class` arm, which consults no flat store before `findClass`
/// and passes a `findClass` throw through as it is) or has no built-in
/// loader on its chain (the native's `scoped_user_chain`: no flat store at
/// all, so its CNFE is the JDK `findClass`'s). On HotSpot such a loader sees
/// only the bootstrap (or platform) classes before its `findClass` too.
/// The VM-initiated resolution door then fails with that exception, as on
/// HotSpot (`--jdk-only`; interpreter round i1 wave 40, lane L5,
/// `vm/src/runtime/interpreter/constants.rs`
/// `drive_defining_loader_load_named`; probe
/// `tools/probes/interp/L5/L5W40BaseLoaderThrow.java`). Pure: reads fields and
/// declared methods, runs no Java, allocates nothing on the heap.
pub fn base_load_class_miss_is_the_loaders(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    internal: &str,
) -> bool {
    let parent_scoped = match crate::classloader::classloader_parent(ctx, loader) {
        None => true,
        Some(parent) => ctx
            .class_name_of_id(ctx.class_id_of_object(parent))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader"),
    };
    parent_scoped
        && crate::classloader::cl_bootstrap_scoped()
        && !crate::classloader::is_runtime_image_class_name(ctx, internal)
        && !ctx.is_bootstrap_appended_class(internal)
        && (crate::classloader::receiver_overrides_find_class(ctx, loader)
            || !crate::classloader::builtin_loader_reachable(ctx, loader))
}

/// `--jdk-only`: the mirror of the class `loader` INITIATED under
/// `internal_name` (JVMS §5.3: the VM asked it — the class-resolution door or
/// `Class.forName` with it — and it answered another loader's class), for its
/// `findLoadedClass` after its own definitions missed: HotSpot's
/// `findLoadedClass0` reads the loader's dictionary, which holds both
/// (`interpreter-L5-an-initiating-loader-record-is-invisible-to-findloadedclass-and-define-FIXED-20261003`,
/// interpreter round i1 wave 38, lane L5; probe `L5W37InitiatingLoaderRecord`).
/// `None` in the other mode, for a built-in loader, and without a record.
pub(crate) fn initiated_class_mirror(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    internal_name: &str,
) -> Option<ObjectRef> {
    if !ctx.is_jdk_only() {
        return None;
    }
    let ns = crate::classloader::peek_loader_namespace_id(ctx, loader)?;
    let class_id = ctx.initiated_class_for_loader(ns, internal_name)?;
    Some(ctx.get_class_mirror(class_id))
}

/// HotSpot's duplicate-definition text ([`duplicate_definition_message`]) for
/// the define backend, which knows the defining loader by namespace id only:
/// `loader_ns` defining `internal_name` after it initiated it as `initiated`
/// (JVMS §5.3.5; `vm_exec.rs` `define_class_full`,
/// `runtime::resolve::initiating_records::define_refusal`). The kind and
/// module clause are read off `initiated` (the new class's bytes are not
/// parsed yet on that path). `None` without the loader object or its
/// `nameAndId`. Wave 38, lane L5.
pub fn initiated_duplicate_definition_message(
    ctx: &mut dyn NativeContext,
    loader_ns: u32,
    internal_name: &str,
    initiated: cratonvm_types::ClassId,
) -> Option<String> {
    let loader =
        crate::classloader::loader_object_for_namespace_id_in(ctx.vm_identity(), loader_ns)?;
    duplicate_definition_message_for(ctx, loader, internal_name, Some(initiated))
}

/// `loader`'s `ClassLoader.nameAndId`, as HotSpot prints a loader (`'name'
/// @hash`, or `ClassName @hash` for an unnamed one): the text the `--jdk-only`
/// constructor bridge stores ([`set_loader_name_and_id`]). The built-in
/// loaders are allocated without a constructor, so their field is null; they
/// are named by class (`'app'`, `'platform'`), as their `nameAndId` would
/// name them. `None` for any other loader without the text.
fn loader_name_and_id_text(ctx: &mut dyn NativeContext, loader: ObjectRef) -> Option<String> {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(loader, "nameAndId") {
        if let Some(text) = ctx.read_string(s).filter(|t| !t.is_empty()) {
            return Some(text);
        }
    }
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(loader))?;
    match &*class_name {
        "jdk/internal/loader/ClassLoaders$AppClassLoader" => Some("'app'".to_string()),
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader" => Some("'platform'".to_string()),
        _ => None,
    }
}

/// `ClassLoader.checkName` (JDK 25): a binary name `findLoadedClass` /
/// `loadClass` may look up — no '/' and no leading '['; null and empty pass
/// (the native then answers null). HotSpot's `findLoadedClass0` native refuses
/// the same names (`VerifyFixClassname` / `verifyClassname`), so both doors
/// share it.
pub(crate) fn find_loaded_class_name_is_legal(name: &str) -> bool {
    !(name.contains('/') || name.starts_with('['))
}

/// HotSpot's `SecurityException` text for `loader` defining the class `bytes`
/// name under `java/` through `ClassLoader.defineClass` with a NULL name (a
/// named one is refused by `preDefineClass` in Java first), measured on JDK 25
/// (`tools/probes/interp/L5/L5W42DefineClassErrors.java`):
///
/// `Class loader (instance of): 'o' @1b6d3586 tried to load prohibited package
/// name: java.l5w42` (an unnamed loader prints its `nameAndId`, the class name
/// and `@hash`, unquoted).
///
/// `None` when the bytes do not parse, the class has no package, or the loader
/// has no `nameAndId` (the caller keeps its own wording). `--jdk-only`
/// callers; interpreter round i1 wave 42, lane L5.
pub(crate) fn prohibited_package_message(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    bytes: &[u8],
) -> Option<String> {
    let class_file = cratonvm_reader::read_class(bytes).ok()?;
    let this_class: &str = &class_file.this_class;
    let (package, _) = this_class.rsplit_once('/')?;
    let me = loader_name_and_id_text(ctx, loader)?;
    Some(format!(
        "Class loader (instance of): {me} tried to load prohibited package name: {}",
        package.replace('/', ".")
    ))
}

/// HotSpot's `LinkageError` text for `loader` defining `internal_name` a
/// second time (`SystemDictionary::check_constraints`, the new class's
/// `class_in_module_of_loader`), measured on JDK 25
/// (`tools/probes/interp/L5/L5W38DuplicateDefineMessage.java`):
///
/// `loader 'l1' @1b6d3586 attempted duplicate class definition for p.Dep. (p.Dep
/// is in unnamed module of loader 'l1' @1b6d3586, parent loader 'app')`
///
/// An interface reads `duplicate interface definition`; a null parent is
/// `'bootstrap'`. The kind and module are the defined class's (the new one's
/// bytes are not parsed on this path; a redefinition under one name keeps
/// both). `None` when either loader has no `nameAndId`
/// (`loader_name_and_id_text`): the caller keeps its own wording. Reads
/// fields only (no allocation). `--jdk-only` callers; interpreter round i1
/// wave 38, lane L5
/// (`docs/internal/fixed-bugs/interpreter-L5-duplicate-class-definition-message-lacks-hotspots-loader-text-FIXED-20261002.md`).
pub fn duplicate_definition_message(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    internal_name: &str,
) -> Option<String> {
    let existing =
        crate::classloader::class_defined_by_this_loader_object(ctx, loader, internal_name);
    duplicate_definition_message_for(ctx, loader, internal_name, existing)
}

/// [`duplicate_definition_message`] with the class whose kind and module the
/// message reads given (`None`: "class", unnamed module).
fn duplicate_definition_message_for(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    internal_name: &str,
    existing: Option<cratonvm_types::ClassId>,
) -> Option<String> {
    let me = loader_name_and_id_text(ctx, loader)?;
    // HotSpot's `class_in_module_of_loader` names the parent of a
    // user-defined loader only: a built-in loader's clause ends at its name
    // (measured on JDK 25: `Lookup.defineClass` of a class the application
    // loader defined already reads `(p.X is in unnamed module of loader
    // 'app')`; probe `tools/probes/interp/L5/L5W44LoaderReview.java`,
    // `lookup-define-duplicate`; interpreter round i1 wave 44, lane L5).
    let builtin = ctx
        .class_name_of_id(ctx.class_id_of_object(loader))
        .is_some_and(|n| {
            n == "jdk/internal/loader/ClassLoaders$AppClassLoader"
                || n == "jdk/internal/loader/ClassLoaders$PlatformClassLoader"
        });
    let parent = if builtin {
        String::new()
    } else {
        match ctx.get_field_by_name(loader, "parent") {
            Value::Object(Some(parent)) => {
                format!(", parent loader {}", loader_name_and_id_text(ctx, parent)?)
            }
            _ => ", parent loader 'bootstrap'".to_string(),
        }
    };
    let kind = if existing.is_some_and(|id| ctx.is_interface_class(id)) {
        "interface"
    } else {
        "class"
    };
    // By identity: a class of a non-boot layer's module names that module
    // (interpreter round i1 wave 45, lane L5).
    let module = match existing {
        Some(id) => ctx.module_identity_of_class(id).describe(),
        None => "unnamed module".to_string(),
    };
    let dotted = internal_name.replace('/', ".");
    Some(format!(
        "loader {me} attempted duplicate {kind} definition for {dotted}. ({dotted} is in \
         {module} of loader {me}{parent})"
    ))
}

/// Whether `loader`'s `loadClass` answers a JDK class name exactly as
/// CratonVM's global route does, so the VM need not ask it: no class of its
/// delegation chain overrides either `loadClass` overload below the JDK's
/// base (so the base native's parent-first delegation runs, which answers a
/// runtime-image name from the built-in chain after `findLoadedClass`, and
/// the caller has already looked at the loader's own definitions), and every
/// parent is a built-in loader, the bootstrap loader, or such a loader too.
///
/// `false` = the loader may answer its own class for such a name (a
/// child-first `loadClass`, or one with a filter) and must be asked, as
/// HotSpot always asks it. A chain deeper than 16 answers `false` (asked).
/// Pure: reads the loader classes' declared methods and the `parent` fields,
/// runs no Java. Interpreter round i1 wave 28, lane L5.
pub fn loader_answers_jdk_names_as_the_jdk(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
) -> bool {
    let mut current = loader;
    for _ in 0..16 {
        // The application / platform loader: the built-in chain the global
        // route stands for. (A bare `URLClassLoader` / `ClassLoader` is not
        // stopped at: its parent may be a child-first user loader.)
        let class_id = ctx.class_id_of_object(current);
        if ctx
            .class_name_of_id(class_id)
            .is_some_and(|name| name.starts_with("jdk/internal/loader/"))
        {
            return true;
        }
        if crate::classloader::receiver_overrides_load_class_single(ctx, current)
            || crate::classloader::receiver_overrides_load_class_resolve(ctx, current)
        {
            return false;
        }
        match crate::classloader::classloader_parent(ctx, current) {
            Some(parent) => current = parent,
            None => return true,
        }
    }
    false
}

/// For a loader [`loader_answers_jdk_names_as_the_jdk`] calls transparent:
/// whether it answers `internal` exactly as CratonVM's global route does.
/// The global route also answers from the APPLICATION class path, which only
/// a delegation chain that reaches the application loader sees; a chain that
/// ends at the bootstrap loader (a null parent) or at the platform loader
/// sees the runtime image and `-Xbootclasspath/a` only, and for any other
/// name HotSpot's `loadClass` reaches the loader's own `findClass`. So a
/// `javax/…` library class on the class path (`javax/inject`, `com/sun/el`)
/// must be asked of such a loader, not answered with the application's copy
/// (`--jdk-only`; interpreter round i1 wave 40, lane L5,
/// `constants.rs` `drive_loader_for_global_name`; probe
/// `tools/probes/interp/L5/L5W40BootParentGlobalName.java`). Pure: reads
/// `parent` fields and class names, runs no Java.
pub fn transparent_loader_answers_as_the_global_route(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    internal: &str,
) -> bool {
    let mut current = loader;
    for _ in 0..16 {
        let class_id = ctx.class_id_of_object(current);
        let name = ctx.class_name_of_id(class_id);
        match name.as_deref() {
            Some("jdk/internal/loader/ClassLoaders$AppClassLoader") => return true,
            Some(n) if n.starts_with("jdk/internal/loader/") => break,
            _ => {}
        }
        match crate::classloader::classloader_parent(ctx, current) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    crate::classloader::is_runtime_image_class_name(ctx, internal)
        || ctx.is_bootstrap_appended_class(internal)
}

/// Populate the `URLClassLoader`-specific instance fields that the real
/// JDK `URLClassLoader` constructors set via inline field initialisers.
///
/// `URLClassLoader` declares two `final` fields beyond what `ClassLoader`
/// provides:
///   - `ucp`        — a `jdk.internal.loader.URLClassPath`
///   - `closeables` — a `java.util.WeakHashMap<Closeable,Void>`
///
/// Every real `URLClassLoader(...)` constructor assigns both inline
/// (`closeables = new WeakHashMap<>()`, `ucp = new URLClassPath(...)`).
/// CratonVM's simplified `URLClassLoader.<init>` natives bypass the real
/// constructor, so these fields stay `null`.
///
/// The concrete failure this fixes: the real-JDK bytecode for
/// `URLClassLoader.getResourceAsStream(String)` does
/// `synchronized (closeables) { ... }` (a `monitorenter` on the
/// `closeables` field, pc≈55). A `null` `closeables` makes that
/// `monitorenter` throw `NullPointerException` — exactly the Spring Boot 4
/// boot failure (`Could not initialize Java logging` → NPE at
/// `URLClassLoader.getResourceAsStream pc=56`).
///
/// `closeables` is built via `new_object` + the `()V` `<init>` so it routes
/// through CratonVM's native `WeakHashMap.<init>` (which installs the
/// `buckets`/`size`/`capacity` slots the `WeakHashMap.containsKey`/`put`
/// natives expect). A plain `alloc_concurrent_synthetic` would leave the
/// buckets array null and the subsequent `containsKey` would NPE instead.
fn init_urlclassloader_fields(ctx: &mut dyn NativeContext, this: ObjectRef) {
    // Both `new_object` and `invoke` below may trigger a moving collection.
    // Keep the receiver rooted throughout and reload it before every write;
    // otherwise a URLClassLoader can be observed with a null `ucp` after its
    // constructor appears to have initialized it. Tomcat's
    // WebappLoader.buildClassPath then immediately dereferences that field.
    let this_pin = ctx.pin_native_root(this);
    let diag = crate::nbflags().dbg_urlcl;
    // `closeables` — WeakHashMap. `getResourceAsStream` synchronizes on it.
    // Only populate when currently null so a real `<init>` that already ran
    // (e.g. the name-carrying constructor whose bytecode we don't override)
    // is not clobbered.
    let this = ctx.read_native_pin(this_pin, this);
    let existing = ctx.get_field_by_name(this, "closeables");
    if diag {
        eprintln!(
            "[DBG_URLCL] init_urlclassloader_fields: closeables(before)={:?} ucp={:?}",
            existing,
            ctx.get_field_by_name(this, "ucp"),
        );
    }
    if !matches!(existing, Value::Object(Some(_))) {
        if let Ok(Some(Value::Object(Some(whm)))) = ctx.new_object("java/util/WeakHashMap") {
            let whm_pin = ctx.pin_native_root(whm);
            // Route through the native `WeakHashMap.<init>()V` so the
            // backing buckets array is installed; ignore failure (the
            // object is still a valid non-null monitor either way).
            let _ = ctx.invoke(
                "java/util/WeakHashMap",
                "<init>",
                "()V",
                &[Value::Object(Some(whm))],
            );
            let whm = ctx.read_native_pin(whm_pin, whm);
            ctx.unpin_native_roots(whm_pin);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field_by_name(this, "closeables", Value::Object(Some(whm)));
            if diag {
                eprintln!(
                    "[DBG_URLCL] init_urlclassloader_fields: closeables(after)={:?}",
                    ctx.get_field_by_name(this, "closeables"),
                );
            }
        } else if diag {
            eprintln!(
                "[DBG_URLCL] init_urlclassloader_fields: failed to allocate WeakHashMap for closeables",
            );
        }
    }

    // `ucp` — jdk.internal.loader.URLClassPath. The real-JDK
    // `URLClassLoader.getURLs()` / `findResource()` bytecode dereferences this
    // field (`return ucp.getURLs();`), so a null value throws
    // `NullPointerException: Cannot invoke getURLs on null`. Concrete breakage:
    // Tomcat's `WebappLoader.buildClassPath()` -> `URLClassLoader.getURLs()` ->
    // NPE -> "Error starting the loader" -> StandardContext start fails -> the
    // whole embedded server fails to start (every TomcatBaseTest server test
    // hangs/errs). CratonVM serves class/resource loading from its global
    // dynamic classpath (see `register_url_array`), NOT this `ucp` field, and
    // installs shim `URLClassPath` natives (`getURLs` -> empty, `findResource`
    // -> null) via `register_url_class_path_safe_stubs`. So a bare non-null
    // `URLClassPath` instance is sufficient to keep the real bytecode from
    // NPEing while leaving CratonVM's class loading unchanged.
    let this = ctx.read_native_pin(this_pin, this);
    let ucp_existing = ctx.get_field_by_name(this, "ucp");
    if !matches!(ucp_existing, Value::Object(Some(_))) {
        if let Ok(Some(Value::Object(Some(ucp)))) =
            ctx.new_object("jdk/internal/loader/URLClassPath")
        {
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field_by_name(this, "ucp", Value::Object(Some(ucp)));
            if diag {
                eprintln!(
                    "[DBG_URLCL] init_urlclassloader_fields: ucp(after)={:?}",
                    ctx.get_field_by_name(this, "ucp"),
                );
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
}

/// Finish a simplified `URLClassLoader` constructor without allowing a moving
/// collection to stale either its receiver or the caller-supplied URL array.
///
/// The three helpers below all allocate on realistic paths. In particular,
/// keeping `this` only in a native local across `init_classloader_common_fields`
/// could make the following `ucp` initialization write through a forwarded
/// reference, leaving the live loader with its default null field.
pub(crate) fn init_urlclassloader_constructor(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    urls: Value,
) -> Result<(), MethodCallFailed> {
    let this_pin = ctx.pin_native_root(this);
    let urls_pin = match urls {
        Value::Object(Some(urls)) => Some((ctx.pin_native_root(urls), urls)),
        _ => None,
    };

    let this = ctx.read_native_pin(this_pin, this);
    init_classloader_common_fields(ctx, this)?;
    let this = ctx.read_native_pin(this_pin, this);
    init_urlclassloader_fields(ctx, this);
    let this = ctx.read_native_pin(this_pin, this);
    let urls = urls_pin
        .map(|(pin, urls)| ctx.read_native_pin(pin, urls))
        .map_or(urls, |urls| Value::Object(Some(urls)));
    register_url_array(ctx, this, urls);

    if let Some((pin, _)) = urls_pin {
        ctx.unpin_native_roots(pin);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(())
}

pub(crate) fn init_urlclassloader_constructor_with_parent(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    urls: Value,
    parent: Value,
) -> Result<(), MethodCallFailed> {
    ctx.set_field_by_name(this, "parent", parent);
    init_urlclassloader_constructor(ctx, this, urls)?;
    Ok(())
}

pub(crate) fn init_urlclassloader_constructor_with_default_parent(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    urls: Value,
) -> Result<(), MethodCallFailed> {
    let this_pin = ctx.pin_native_root(this);
    // gen r4w3/rooting: `urls` crosses the same GC point as `this`.
    let urls_pin = match urls {
        Value::Object(Some(u)) => Some((ctx.pin_native_root(u), u)),
        _ => None,
    };
    let parent = get_or_create_system_cl(ctx)?;
    let this = ctx.read_native_pin(this_pin, this);
    let urls = match urls_pin {
        Some((pin, u)) => Value::Object(Some(ctx.read_native_pin(pin, u))),
        None => urls,
    };
    init_urlclassloader_constructor_with_parent(ctx, this, urls, Value::Object(parent))?;
    ctx.unpin_native_roots(this_pin);
    Ok(())
}

/// Register ClassLoader natives needed in real-JDK mode.
///
/// These override the complex real-JDK constructors with minimal versions
/// that just store the parent reference, and provide getParent() that reads
/// it back.
pub fn register_classloader_real_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cl = "java/lang/ClassLoader";
    let ucl = "java/net/URLClassLoader";

    // ClassLoader.<init>()V — default constructor (parent = system class loader)
    r.register(cl, "<init>", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // gen r4w3/rooting: `get_or_create_system_cl` can allocate / run
        // Java on first use; keep `this` rooted across it.
        let this_pin = ctx.pin_native_root(this);
        let sys = get_or_create_system_cl(ctx)?;
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        ctx.set_field_by_name(this, "parent", Value::Object(sys));
        // Initialise defaultDomain / classes / packages etc. — see
        // `init_classloader_common_fields`. Without this a subclass that
        // calls `defineClass` NPEs in `preDefineClass` on a null
        // `defaultDomain`.
        init_classloader_common_fields(ctx, this)?;
        Ok(None)
    });

    // ClassLoader.<init>(ClassLoader)V — constructor with explicit parent
    r.register(cl, "<init>", "(Ljava/lang/ClassLoader;)V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let parent = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field_by_name(this, "parent", parent);
        init_classloader_common_fields(ctx, this)?;
        Ok(None)
    });

    // ClassLoader.<init>(String, ClassLoader)V — named constructor
    r.register(
        cl,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let parent = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "name", name);
            ctx.set_field_by_name(this, "parent", parent);
            init_classloader_common_fields(ctx, this)?;
            Ok(None)
        },
    );

    // ClassLoader.getParent() — read parent field
    r.register(cl, "getParent", "()Ljava/lang/ClassLoader;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let parent = ctx.get_field_by_name(this, "parent");
        // Coerce Int(0) from zero-initialized fields to Object(None)
        match parent {
            Value::Object(_) => Ok(Some(parent)),
            Value::Int(0) | Value::Long(0) => Ok(Some(Value::Object(None))),
            _ => Ok(Some(Value::Object(None))),
        }
    });

    // ClassLoader.getName() — read the real `name` field.
    //
    // `null` for an unnamed loader, NOT `""`. `ClassLoader.getName()` is
    // specified as "the name of this class loader **or null if this class
    // loader is not named**", and the three unnamed constructors
    // (`ClassLoader()`, `ClassLoader(ClassLoader)`, and
    // `ClassLoader(String,ClassLoader)` with a null name) all leave it null.
    // This used to answer `""`, which is a different value from the one every
    // caller's null check is written against — measured against HotSpot 25 by
    // `probes/L1LoaderIdentityProbe` (`parented-getName` / `default-getName` /
    // `nullparent-getName`), which is also the regression test for it.
    // `getName()` on the app / platform loaders is unaffected: their real
    // `name` field is populated by name in `get_or_create_app_loader` /
    // `get_or_create_platform_loader`.
    r.register(cl, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        match ctx.get_field_by_name(this, "name") {
            name @ Value::Object(Some(_)) => Ok(Some(name)),
            _ => Ok(Some(Value::Object(None))),
        }
    });

    // ClassLoader.getSystemClassLoader() — return the system (app) class loader
    r.register(
        cl,
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
        |ctx, _args| {
            let sys = get_or_create_system_cl(ctx)?;
            Ok(Some(Value::Object(sys)))
        },
    );

    // ClassLoader.getPlatformClassLoader() — return platform class loader
    r.register(
        cl,
        "getPlatformClassLoader",
        "()Ljava/lang/ClassLoader;",
        |ctx, _args| {
            let platform = get_or_create_platform_cl(ctx)?;
            Ok(Some(Value::Object(platform)))
        },
    );

    // ClassLoader.getUnnamedModule() — return THIS loader's unnamed module.
    // Stock bytecode returns `this.unnamedModule`, but our synthetic loaders
    // never populate that field, so the real-bytecode read hands back a
    // null/garbage value — observed as a String reaching
    // `ResourceBundle.getCallerModule(...).isNamed()` (when `getCallerClass()`
    // is null, `getCallerModule` falls back to
    // `getSystemClassLoader().getUnnamedModule()`), surfacing as
    // `NoSuchMethodError: java/lang/String.isNamed()Z` mid WildFly boot.
    // Return a synthetic *unnamed* Module: field 0 (name) = null, so the
    // `Module.isNamed()` native (phases_late `register_p59_module`) reports
    // false and `checkNamedModule` passes. Single allocation (no
    // `create_string`), so there is no GC window that could relocate/reclaim
    // the unpinned object before return.
    r.register(
        cl,
        "getUnnamedModule",
        "()Ljava/lang/Module;",
        |ctx, args| {
            // Return the ONE canonical unnamed-module mirror rather than a fresh
            // allocation per call: the JDK compares Modules by identity, so
            // `Foo.class.getModule() == loader.getUnnamedModule()` (and Mockito's
            // `assureCanReadMockito` / ResourceBundle's caller-module checks) must
            // see the same object. It also carries a non-null `loader`, matching
            // HotSpot. See `lang_class::canonical_unnamed_module`.
            // Per-loader for user-defined loaders (HotSpot: every loader has
            // its own unnamed module and `Module.getClassLoader()` answers
            // that loader); built-in loaders keep the single canonical one.
            let this = match args.first() {
                Some(Value::Object(o)) => *o,
                _ => None,
            };
            let m = crate::lang_class::unnamed_module_for_loader(ctx, this)?;
            Ok(Some(Value::Object(Some(m))))
        },
    );

    // ClassLoader.findLoadedClass(String) — check if a class is ALREADY
    // loaded; per the JVM spec this MUST NOT trigger loading. The previous
    // impl called `ctx.load_class(&internal)`, which loaded any
    // classpath-resolvable class as a side effect and then reported it as
    // loaded — so `findLoadedClass("not.yet.Loaded")` returned non-null
    // where HotSpot returns null. That broke Spring's `assertClassNotLoaded`
    // (type-filter / classpath-scanning tests: `AnnotationTypeFilterTests`,
    // `AssignableTypeFilterTests`, … — bug-06 family 3), which scans class
    // *bytes* via ASM without loading and then asserts the class was not
    // loaded. Use a no-load lookup of the loaded-class set instead (matches
    // the synthetic-mode `cl_find_loaded_class` handler and HotSpot).
    r.register(
        cl,
        "findLoadedClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let class_name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let class_name = ctx.read_string(class_name_obj).unwrap_or_default();
            // `--jdk-only`: the JDK's `findLoadedClass` answers null for a name
            // `checkName` refuses (a '/' in it, or a leading '[') before its
            // native runs; this registration stands in front of that bytecode,
            // so it applies the same test. `findLoadedClass("p/X")` answered
            // the class (interpreter round i1 wave 42 follow-up, lane L5;
            // `L5W42LoaderSurface`, `find-loaded-own`).
            if ctx.is_jdk_only() && !find_loaded_class_name_is_legal(&class_name) {
                return Ok(Some(Value::Object(None)));
            }
            let internal = class_name.replace('.', "/");
            // JVMS §5.3 / no-load: report a class only if THIS loader loaded it.
            // For a user-defined loader this is its own namespace or the classes
            // it is the recorded defining loader of — so a fresh custom loader
            // gets null for an app-loaded class and its override-first
            // redefinition fires. Built-in loaders keep the global (no-load)
            // lookup. (Shared with the synthetic-mode `cl_find_loaded_class`.)
            // Then, `--jdk-only`, a class the loader INITIATED (JVMS §5.3;
            // wave 38, lane L5): see `initiated_class_mirror`.
            match crate::classloader::find_loaded_class_for_loader(ctx, this, &internal) {
                Some(mirror) => Ok(Some(Value::Object(Some(mirror)))),
                None => Ok(Some(Value::Object(initiated_class_mirror(ctx, this, &internal)))),
            }
        },
    );

    // ClassLoader.registerAsParallelCapable() — always return true.
    // KEEP (constant, justified): the boolean means "this loader class is now
    // registered as parallel-capable". CratonVM does not serialize loading on a
    // per-class-name lock in the first place, so every loader behaves as
    // parallel-capable and `true` is the accurate answer, not an optimistic
    // one. (Real JDK returns false only for the bookkeeping case where a
    // superclass was not itself registered — a distinction we do not model.)
    //
    // Wave-3 shadowing note: this class+method+descriptor is registered THREE
    // times — here, `classloader.rs::cl_register_as_parallel_capable`, and
    // `deprecated_internal.rs`. All three return 1, so last-write-wins is
    // currently harmless; if you ever change the answer, change all three or
    // the edit will be silently shadowed.
    //
    // RESOLVED — the "known inconsistency" this comment used to describe is
    // gone; re-verified wave 4 (2026-07-28). The claim was that
    // `classloader.rs::cl_is_registered_as_parallel_capable` read a
    // `CL_IS_PARALLEL_CAPABLE` field nothing ever SET, so the query answered
    // false while this answers true. All four ClassLoader initialisers in
    // `classloader.rs` (`alloc_classloader`, `cl_init`, `cl_init_parent`,
    // `cl_init_name_parent`) now seed that slot to `Int(1)`, and the query
    // native falls back to `1` for loaders whose layout is too short to carry
    // the slot. Register and query therefore agree. If you ever make this
    // return something other than 1, update `CL_IS_PARALLEL_CAPABLE`'s four
    // writers and `cl_is_registered_as_parallel_capable`'s fallback too.
    //
    // Not implementable as a caller-class lookup from here in any case: the
    // method is STATIC, so there is no receiver, and `NativeContext` exposes no
    // caller-class / stack-walk accessor to stand in for the JDK's
    // `Reflection.getCallerClass()` + static `ParallelLoaders` set.
    r.register(cl, "registerAsParallelCapable", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });

    // URLClassLoader ctors — Spring Boot launcher allocates
    // `LaunchedURLClassLoader` / `LaunchedClassLoader` via these signatures
    // early in boot. Spring Boot 3.2+/4's `LaunchedClassLoader` calls
    // `super(urls, parent)` (the 2-arg form below).
    //
    // NOTE: the `ucp` field of `URLClassLoader` is declared as a
    // `jdk.internal.loader.URLClassPath`, NOT a `URL[]`. Storing the raw
    // `URL[]` there is wrong-typed; later real-JDK bytecode that does
    // `ucp.findResource(...)` would fail. We therefore do NOT write `ucp`
    // here — `register_url_array` already wires the URLs into CratonVM's
    // dynamic classpath, and `getResource`/`findClass` are served by
    // natives that consult that classpath, not the `ucp` field. The
    // `closeables` field IS initialised (via `init_urlclassloader_fields`)
    // because the real-JDK `getResourceAsStream` bytecode synchronizes on
    // it and a null value throws NPE on `monitorenter`.
    r.register(ucl, "<init>", "([Ljava/net/URL;)V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // gen r4w3/rooting: `get_or_create_system_cl` can allocate / run
        // Java; root `this` and `urls` (read BEFORE the GC point — the args
        // slice is never refreshed) and re-read both afterwards.
        let mut urls = args.get(1).copied().unwrap_or(Value::Object(None));
        let this_pin = ctx.pin_native_root(this);
        let urls_pin = match urls {
            Value::Object(Some(u)) => Some((ctx.pin_native_root(u), u)),
            _ => None,
        };
        let parent = get_or_create_system_cl(ctx)?;
        let this = ctx.read_native_pin(this_pin, this);
        if let Some((pin, u)) = urls_pin {
            urls = Value::Object(Some(ctx.read_native_pin(pin, u)));
        }
        ctx.unpin_native_roots(this_pin);
        ctx.set_field_by_name(this, "parent", Value::Object(parent));
        // URLClassLoader extends SecureClassLoader extends ClassLoader;
        // the inherited `defaultDomain` / `classes` / `packages` fields
        // still need initialising (see `init_classloader_common_fields`).
        // `closeables` / `ucp` — see `init_urlclassloader_fields`.
        // Register the URLs with the application classpath so classes inside
        // the jars/dirs are actually loadable — without this a custom
        // URLClassLoader (Tomcat's CommonClassLoader, ActiveMQ's launcher)
        // can never find its classes and throws ClassNotFoundException.
        init_urlclassloader_constructor(ctx, this, urls)?;
        Ok(None)
    });
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let parent = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "parent", parent);
            let urls = args.get(1).copied().unwrap_or(Value::Object(None));
            init_urlclassloader_constructor(ctx, this, urls)?;
            Ok(None)
        },
    );
    // Name-carrying URLClassLoader constructors (JDK 9+). Spring Boot's
    // launcher and several frameworks use these. Registering natives for
    // every signature guarantees `closeables` is non-null no matter which
    // `super(...)` form a `URLClassLoader` subclass invokes — otherwise the
    // unhandled signature falls through to real-JDK bytecode whose own
    // inline field initialisers may not run cleanly in CratonVM.
    r.register(
        ucl,
        "<init>",
        "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let urls = args.get(2).copied().unwrap_or(Value::Object(None));
            let parent = args.get(3).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "name", name);
            ctx.set_field_by_name(this, "parent", parent);
            init_urlclassloader_constructor(ctx, this, urls)?;
            Ok(None)
        },
    );
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let urls = args.get(1).copied().unwrap_or(Value::Object(None));
            let parent = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "parent", parent);
            init_urlclassloader_constructor(ctx, this, urls)?;
            Ok(None)
        },
    );
    r.register(
        ucl,
        "<init>",
        "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let urls = args.get(2).copied().unwrap_or(Value::Object(None));
            let parent = args.get(3).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "name", name);
            ctx.set_field_by_name(this, "parent", parent);
            init_urlclassloader_constructor(ctx, this, urls)?;
            Ok(None)
        },
    );

    // ES.2 — package-private `AccessControlContext`-carrying constructors.
    //
    // `URLClassLoader.newInstance(URL[])` (and the 2-arg variant) is a static
    // factory that the JDK and apps use to build a child classloader whose
    // loaded classes inherit the *caller's* protection domain. It allocates a
    // `java.net.FactoryURLClassLoader` (a package-private subclass), whose
    // constructor chains to `super(urls, acc)` / `super(name, urls, parent,
    // acc)`. Those two `URLClassLoader` constructors are PACKAGE-PRIVATE and
    // were not registered here, so they fell through to real-JDK bytecode that
    // builds a `jdk.internal.loader.URLClassPath` `ucp` field — but CratonVM's
    // `findClass`/`getResource` consult the GLOBAL dynamic classpath
    // (`register_dynamic_classpath`), NOT the `ucp` field, so the URLs were
    // never wired in and classes inside them were invisible.
    //
    // Concrete breakage: Elasticsearch's `CliToolProvider.load` builds the
    // server-cli classloader via `URLClassLoader.newInstance(URL[])`; the
    // `server` `CliToolProvider` lived in that jar and so was never found
    // (`ServiceLoader` saw only the parent-classpath `node`/`shard`
    // providers → `AssertionError: CliToolProvider [server] not found`).
    // Mirror the public ctors: wire the URLs into the dynamic classpath.
    //
    // `URLClassLoader(URL[], AccessControlContext)` — real JDK calls the no-arg
    // `super()`, so the delegation parent is the system class loader.
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/security/AccessControlContext;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let mut urls = args.get(1).copied().unwrap_or(Value::Object(None));
            let mut acc = args.get(2).copied().unwrap_or(Value::Object(None));
            // gen r4w3/rooting: `get_or_create_system_cl` can allocate / run
            // Java; root `this`, `urls` and `acc` across it and re-read.
            let this_pin = ctx.pin_native_root(this);
            let urls_pin = match urls {
                Value::Object(Some(u)) => Some((ctx.pin_native_root(u), u)),
                _ => None,
            };
            let acc_pin = match acc {
                Value::Object(Some(a)) => Some((ctx.pin_native_root(a), a)),
                _ => None,
            };
            let parent = get_or_create_system_cl(ctx)?;
            let this = ctx.read_native_pin(this_pin, this);
            if let Some((pin, u)) = urls_pin {
                urls = Value::Object(Some(ctx.read_native_pin(pin, u)));
            }
            if let Some((pin, a)) = acc_pin {
                acc = Value::Object(Some(ctx.read_native_pin(pin, a)));
            }
            ctx.unpin_native_roots(this_pin);
            ctx.set_field_by_name(this, "parent", Value::Object(parent));
            ctx.set_field_by_name(this, "acc", acc);
            init_urlclassloader_constructor(ctx, this, urls)?;
            Ok(None)
        },
    );
    // `URLClassLoader(String, URL[], ClassLoader, AccessControlContext)` —
    // real JDK calls `super(name, parent)`, so the parent is the explicit arg.
    r.register(
        ucl,
        "<init>",
        "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/security/AccessControlContext;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let urls = args.get(2).copied().unwrap_or(Value::Object(None));
            let parent = args.get(3).copied().unwrap_or(Value::Object(None));
            let acc = args.get(4).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "name", name);
            ctx.set_field_by_name(this, "parent", parent);
            ctx.set_field_by_name(this, "acc", acc);
            init_urlclassloader_constructor(ctx, this, urls)?;
            Ok(None)
        },
    );

    // S111r9 — SecureClassLoader.<clinit> override.
    //
    // The real-JDK bytecode for `java/security/SecureClassLoader.<clinit>` is
    // a single `invokestatic ClassLoader.registerAsParallelCapable(); pop;
    // return` and should always succeed (our `registerAsParallelCapable`
    // native returns 1). However, when this `<clinit>` runs as part of the
    // singleton AppClassLoader allocation chain (`alloc_concurrent_synthetic`
    // for `ClassLoaders$AppClassLoader` triggers the `ClassLoader →
    // SecureClassLoader → BuiltinClassLoader → AppClassLoader` superclass
    // init walk), an `IllegalStateException` gets attributed to it whose
    // cause is `IOException("Unable to find ZIP central directory records
    // after reading 65557 bytes")` — Spring Boot Loader's
    // `CentralDirectoryEndRecord` error wrapped by
    // `ExecutableArchiveLauncher.<init>`. The exception is constructed by
    // Java code that runs LATER in the boot but the ObjectRef leaks back
    // through the dispatch path during the first SCL <clinit>, defeating
    // the swallow's Initialized state and breaking the rest of the
    // ClassLoader chain (manifests as `Comparable.loadClass` NSME at the
    // app's MainMethodRunner.run dispatch).
    //
    // Bypass the bytecode entirely with a no-op native — the only side
    // effect of the original bytecode is `registerAsParallelCapable()` which
    // we already make a no-op via the native above. Writing this override
    // means the SCL <clinit> never enters the interpreter, never goes
    // through the corrupt dispatch path, and always succeeds. The class
    // state transitions cleanly to Initialized and the `URLClassLoader →
    // BuiltinClassLoader → AppClassLoader` chain initialises end-to-end,
    // unblocking real-app classloader dispatch (Spring Boot 2 fat-jar
    // launcher's `LaunchedURLClassLoader.loadClass`, Spring Cloud Gateway
    // demo, sister jars).
    r.register(
        "java/security/SecureClassLoader",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );

    // ClassLoader.loadClass(String) — delegate to VM class loading.
    //
    // `findClass` is the documented `ClassLoader` extension point: application
    // code subclasses `ClassLoader` and overrides `findClass` to load classes
    // from custom sources (Eclipse Equinox OSGi, custom classloaders). The JVM
    // `loadClass` contract is "after parent delegation fails, call findClass".
    // Because this native stands in for `ClassLoader.loadClass` (CratonVM keeps
    // no JDK bytecode for it), it MUST perform the virtual `findClass` dispatch
    // itself when the receiver is a non-builtin subclass that overrides it —
    // otherwise the override is silently shadowed and custom classloaders break.
    r.register(
        cl,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        cl_real_load_class,
    );
    r.register(
        cl,
        "loadClass",
        "(Ljava/lang/String;Z)Ljava/lang/Class;",
        |ctx, args| {
            // Base `ClassLoader.loadClass(String,boolean)` — resolve arg ignored.
            // This native stands in for the BASE method only; a subclass override
            // of loadClass(String,boolean) runs its own bytecode. Reaching here
            // means base parent-first delegation (the receiver inherits it, or a
            // subclass override called `super.loadClass(name, resolve)`). Go
            // STRAIGHT to base delegation — NOT `cl_real_load_class` — so the
            // override-dispatch is not re-triggered (which would recurse via the
            // override's `super` call).
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            cl_real_load_class_base(ctx, this, name_obj)
        },
    );
    r.set_category(__prev_cat);
}

/// `ClassLoader.loadClass(String)` for real-JDK mode.
///
/// `ClassLoader.loadClass(String)` is spec'd as `return loadClass(name, false)`.
/// If the receiver's actual class overrides the protected
/// `loadClass(String,boolean)` (Spring's `OverridingClassLoader`, OSGi-like and
/// test-isolation loaders that redefine eligible classes under themselves or
/// reject filtered names BEFORE parent delegation), dispatch that override
/// virtually so its custom ordering and defining-loader identity are honored.
/// Reimplementing base delegation here would resolve the class through the
/// global/app class store and ignore the user loader entirely.
///
/// Otherwise fall back to base parent-first delegation (`cl_real_load_class_base`).
fn cl_real_load_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };

    // BINARY NAME VALIDATION, at THIS entry point only.
    //
    // `ClassLoader.loadClass` takes a BINARY name -- `java.lang.String`. Two
    // spellings that look close enough are refused by the JDK, and both were
    // accepted here. MEASURED against HotSpot 25.0.3+9 in BOTH modes
    // (`probes/ClassLoaderShadowSweep.java`):
    //
    //   loadClass("java/lang/String")  HotSpot ClassNotFoundException  CratonVM ok
    //   loadClass("[I")                HotSpot ClassNotFoundException  CratonVM ok
    //
    // The slash form got in because `cl_real_load_class_base_rooted` does
    // `replace('.', "/")`, a NO-OP on a name that already uses slashes. The
    // array form got in because the class store holds array classes under their
    // JVMS descriptor.
    //
    // SCOPED TO THIS FUNCTION, and that scoping is the whole point: the first
    // cut put this check in the shared `cl_real_load_class_base`, which
    // `Class.forName` also reaches -- and `Class.forName("[I")` MUST resolve.
    // The broad guard turned three passing `forName` rows in
    // `ClassShadowSweep` red and crashed `ClassLoaderShadowSweep` at row 20 of
    // 90. Arrays are findable through `forName` and not through `loadClass`;
    // both probes now assert both halves so the asymmetry cannot be "fixed" in
    // the wrong direction later.
    if let Some(name) = ctx.read_string(class_name_obj) {
        if name.contains('/') || name.starts_with('[') {
            let exc = crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/ClassNotFoundException",
                1,
                &name,
            );
            return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                exc?,
            ));
        }
    }

    // A CLOSED `URLClassLoader` is not refused here (gc-common w12-d). Its
    // `loadClass` still runs the override below, `findLoadedClass` and parent
    // delegation, as HotSpot's does; only the search of its OWN URLs stops
    // (`cl_real_load_class_base_rooted` skips the local define, and
    // `ucl_real_find_class` refuses). This used to throw
    // `ClassNotFoundException` for every name, `java.lang.String` included,
    // but it read the synthetic `close()`'s set, which real-JDK mode never
    // wrote. Since the real-JDK `close()` bridge writes that same per-VM set,
    // the blanket refusal would have started firing.
    //
    // `ClassLoader.loadClass(String)` is virtual too. Honor an override of
    // that exact public overload before looking for the protected
    // `(String,boolean)` form: Spring Boot's ModifiedClassPathClassLoader
    // overrides only this method to reject @ClassPathExclusions packages.
    // Calling it virtually is safe because a real override is present; base
    // ClassLoader receivers continue to the native delegation below.
    if let Some(result) =
        crate::classloader::invoke_single_load_class_override(ctx, this, class_name_obj)
    {
        return result;
    }

    // Honor a `loadClass(String,boolean)` override (override-first loaders).
    // `super.loadClass(name, resolve)` from such an override lands on the base
    // `loadClass(String,boolean)` native (→ `cl_real_load_class_base`), so there
    // is no recursion back here.
    if crate::classloader::receiver_overrides_load_class_resolve(ctx, this) {
        return Ok(ctx.invoke_virtual(
            this,
            "loadClass",
            "(Ljava/lang/String;Z)Ljava/lang/Class;",
            &[Value::Object(Some(class_name_obj)), Value::Int(0)],
        )?);
    }

    Ok(cl_real_load_class_base(ctx, this, class_name_obj)?)
}

/// Outcome of [`load_class_visible_to`].
///
/// Distinguishes "nothing found for the requested name" (caller should keep
/// trying other resolution strategies, ultimately throwing
/// `ClassNotFoundException`) from "the requested class file exists but a
/// supertype/interface failed to resolve" (JVMS §5.3/§5.4: HotSpot throws
/// `NoClassDefFoundError` naming the missing *dependency*, not a bare CNFE on
/// the originally requested class). See `no_class_def_found_error` for the
/// exception shape.
enum ClassLookup {
    Found(Value),
    /// The requested class exists but `.0` (its supertype/interface, in
    /// internal/slash form) could not be resolved.
    DependencyMissing(String),
    NotFound,
}

/// `ctx.load_class` resolves through CratonVM's flat global class store with no
/// awareness of the requesting loader. Wrap it with the same JVMS §5.3
/// defining-loader visibility check the synthetic-JDK path applies
/// (`cid_visible_mirror`) so a user-defined loader cannot resolve a SIBLING
/// loader's dynamically-defined class here -- two unrelated sibling loaders
/// each defining their own same-named class from different bytecode must get
/// distinct `Class` objects, not the first loader's (real-JDK-mode analogue of
/// the synthetic path's `resolve_global_if_visible`).
fn load_class_visible_to(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    internal: &str,
) -> ClassLookup {
    // gen r4w3/rooting: `ctx.load_class` can allocate / upcall; keep `this`
    // rooted and re-read it before `cid_visible_mirror` below.
    let this_pin = ctx.pin_native_root(this);
    let loaded = ctx.load_class(internal);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let mirror_val = match loaded {
        Ok(Some(v)) => v,
        Ok(None) => return ClassLookup::NotFound,
        // `ClassManager::load_class` propagates a recursive supertype/
        // interface load failure UNCHANGED (see `resolve_supertype` in
        // classloading/src/class_manager.rs) -- so `class_name` here names
        // whichever class in the hierarchy actually failed to resolve, not
        // necessarily `internal`. When they differ, the requested class file
        // itself was found; only a dependency is missing. Discovered
        // 2026-07-17 debugging a Hibernate `LockTest` CNFE that was actually
        // a missing-jar `EntityManagerFactoryBasedFunctionalTest` superclass
        // -- this case used to be swallowed by a catch-all `_ => return
        // None`, surfacing a message-less CNFE on the wrong class and
        // costing significant time to root-cause.
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
                "load_class_visible_to: requested class exists but a \
                 dependency failed to resolve -- NoClassDefFoundError"
            );
            return ClassLookup::DependencyMissing(class_name);
        }
        Err(e) => {
            // Genuinely-missing requested class, or some other internal
            // error -- unchanged CNFE-eventually behavior, but keep the real
            // cause visible in debug logs instead of fully discarding it.
            tracing::debug!(
                requested = internal,
                error = %e,
                "load_class_visible_to: ctx.load_class failed"
            );
            return ClassLookup::NotFound;
        }
    };
    match mirror_val {
        Value::Object(Some(mirror_obj)) => match ctx.class_id_from_mirror(mirror_obj) {
            Some(cid) => crate::classloader::cid_visible_mirror(ctx, this, cid)
                .map(|m| ClassLookup::Found(Value::Object(Some(m))))
                .unwrap_or(ClassLookup::NotFound),
            None => ClassLookup::Found(mirror_val),
        },
        other => ClassLookup::Found(other),
    }
}

/// Build a `NoClassDefFoundError` naming `missing_internal` (JVMS
/// §5.3/§5.4 supertype/interface resolution failure), with a
/// `ClassNotFoundException(missing_internal)` cause -- matching real JDK25's
/// shape for this exact scenario (verified 2026-07-17 against `~/jdk25`: a
/// class whose superclass' `.class` file is hidden throws
/// `NoClassDefFoundError: pkg/Sup` -- internal/slash form message -- with
/// `cause = java.lang.ClassNotFoundException: pkg.Sup` -- dotted form).
///
/// Follows the same alloc-and-pin idiom as sibling exception construction in
/// this file (see the `ClassNotFoundException` throw in
/// `cl_real_load_class_base` step 3, and `alloc_single_message_exception`) --
/// `cause` is a fresh heap object that must stay rooted across the further
/// allocations (`create_string`, the outer `new_object_initialized`) needed
/// to build the final exception.
pub(crate) fn no_class_def_found_error(
    ctx: &mut dyn NativeContext,
    missing_internal: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let dotted = missing_internal.replace('/', ".");
    let cnfe_msg = ctx.create_string(&dotted);
    let cause = match ctx.new_object_initialized(
        "java/lang/ClassNotFoundException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(cnfe_msg))],
    ) {
        Ok(Some(Value::Object(Some(c)))) => Some(c),
        _ => None,
    };
    if let Some(cause) = cause {
        let cause_pin = ctx.pin_native_root(cause);
        let ncdfe_msg = ctx.create_string(missing_internal);
        // `NoClassDefFoundError` declares only `()V` and `(String)V` — there is
        // NO `(String,Throwable)` constructor on it in JDK 25 (measured). This
        // used to call that descriptor and it only ever worked because CratonVM
        // fabricated it for every throwable-family class; the JDK's own
        // `ClassLoader` builds the same object the same way this does now,
        // message-only plus `initCause`.
        let result = ctx.new_object_initialized(
            "java/lang/NoClassDefFoundError",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(ncdfe_msg))],
        );
        if let Ok(Some(Value::Object(Some(exc)))) = result {
            let exc_pin = ctx.pin_native_root(exc);
            let cause = ctx.read_native_pin(cause_pin, cause);
            let _ = ctx.invoke_virtual(
                exc,
                "initCause",
                "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
                &[Value::Object(Some(cause))],
            );
            let exc = ctx.read_native_pin(exc_pin, exc);
            ctx.unpin_native_roots(exc_pin);
            ctx.unpin_native_roots(cause_pin);
            return Ok(exc);
        }
        ctx.unpin_native_roots(cause_pin);
    }
    // Fallback (constructor dispatch unavailable for some reason): reuse the
    // same message-only idiom the sibling `ClassNotFoundException` throw
    // uses elsewhere in this file.
    crate::jboss_module_loader::alloc_single_message_exception(
        ctx,
        "java/lang/NoClassDefFoundError",
        1,
        missing_internal,
    )
}

/// Base `ClassLoader.loadClass` parent-first delegation for real-JDK mode
/// (CratonVM keeps no JDK bytecode for `ClassLoader.loadClass`).
///
/// Delegation order (JVMS §5.3 / `ClassLoader.loadClass` contract):
///   1. standard VM class loading (bootstrap → platform → app),
///   2. if that fails and the receiver is a non-builtin `ClassLoader`
///      subclass overriding `findClass`, dispatch the override virtually,
///   3. otherwise throw `ClassNotFoundException`.
///
/// Reached when the receiver does NOT override `loadClass(String,boolean)`, and
/// via `super.loadClass(name, resolve)` (the base native) from a subclass that
/// wants standard parent-first delegation as its fallback.
/// Legacy escape hatch for the "no fabricated stubs from `loadClass`" rule in
/// [`cl_real_load_class_base`]: `CRATONVM_CL_STUB_DELEGATION=1` lets parent
/// delegation answer with a synthetic stub again, as it did before 2026-07-27.
fn stub_may_answer_load_class() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_CL_STUB_DELEGATION")
            .ok()
            .as_deref()
            == Some("1")
    })
}

/// W7-26 R1 — narrow a loader-ladder swallow to the shape the JDK's own
/// `catch` clause names.
///
/// JDK 25 `java.lang.ClassLoader.loadClass(String,boolean)` wraps its parent
/// delegation in exactly one `catch`:
///
/// ```java
/// try {
///     if (parent != null) { c = parent.loadClass(name, false); }
///     else { c = findBootstrapClassOrNull(name); }
/// } catch (ClassNotFoundException e) {
///     // ClassNotFoundException thrown if class not found
///     // from the non-null parent class loader
/// }
/// ```
///
/// So the JDK absorbs a `ClassNotFoundException` and **nothing else**. A
/// `LinkageError` out of a parent's `loadClass`, an
/// `ExceptionInInitializerError` from a loader's `<clinit>`, an
/// `OutOfMemoryError`, or any `RuntimeException` the loader raised all leave
/// `loadClass` on HotSpot. Every loader ladder in this tree was spelled
/// `_ => {}` or `if let Ok(...)`, which absorbs all of them and reports the
/// class as merely absent — a diagnosable failure turned into a wrong answer,
/// which is the species `W7-26-getannotation-swallowed-exception.md` is about.
/// The type test is by `ClassId` hierarchy — the question `instanceof` asks —
/// never by class name, matching `native-api/src/delegated_close.rs`.
///
/// `NoClassDefFoundError` is absorbed alongside `ClassNotFoundException`
/// because CratonVM's own resolution reports the same "this name is not
/// reachable from here" condition with it: `no_class_def_found_error` and the
/// `ClassLookup::DependencyMissing` arm in this file both mint one, so
/// re-raising it would refuse the fall-through the ladders exist to reach.
/// This is exactly the pair `W7-26` residual R1 prescribes.
///
/// `Ok(())` means **absorbed** — treat it as "the class is absent" and keep
/// going down the ladder. `Err(failed)` hands the original failure straight
/// back so the caller can `?` it.
///
/// **Residual, stated rather than hidden:**
/// `MethodCallFailed::InternalError` is absorbed too, which the rule in
/// `delegated_close.rs` argues against (it is not a Java throwable, so no JDK
/// `catch` can name it). It is kept absorbed because it is also the shape
/// `resolve_class_loader_aware` returns for an ordinary "not on any classpath
/// entry" miss — its terminal arm is `Err(MethodCallFailed::from(e))` over a
/// `VmError` — which is the single most common way a rung legitimately falls
/// through. Separating the two needs the resolver to stop reporting plain
/// absence as an internal error; narrowing it here first would refuse every
/// ordinary miss.
pub(crate) fn absorb_class_absent(
    ctx: &dyn NativeContext,
    failed: MethodCallFailed,
) -> Result<(), MethodCallFailed> {
    let thrown = match failed {
        // See the residual above.
        MethodCallFailed::InternalError(_) => return Ok(()),
        MethodCallFailed::ExceptionThrown(obj) => obj,
    };
    let thrown_class = ctx.class_id_of_object(thrown);
    for absent in [
        "java/lang/ClassNotFoundException",
        "java/lang/NoClassDefFoundError",
    ] {
        // A name that was never loaded cannot be this throwable's supertype,
        // so a missing root simply does not absorb.
        if let Some(root) = ctx.class_id_by_name(absent) {
            if ctx.is_subclass(thrown_class, root) {
                return Ok(());
            }
        }
    }
    Err(MethodCallFailed::ExceptionThrown(thrown))
}

/// GC-SAFETY wrapper — the real-JDK sibling of
/// `classloader::cl_load_class_base_delegation_inner`, and the one this
/// workload actually takes (the repro runs with `--java-home`). The body
/// dispatches arbitrary Java twice, at the parent's `loadClass` and at the
/// receiver's `findClass` override, and keeps using `this` and
/// `class_name_obj` on every fall-through arm. Both are bare Rust locals:
/// `safe_native_call` pins the native's ARGS and the collector remaps those
/// pins, but nothing rewrites these copies. Hibernate's
/// `AggregatedClassLoader` is `super(null)` and overrides `findClass` to
/// iterate its scoped child loaders, so the `findClass` arm is the hot one
/// here and its body allocates heavily —
/// `CRATONVM_DBG_STALE_OBJREF` caught a stale deref in a native invoked from
/// `ClassLoaderServiceImpl.classForName`. See
/// map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md.
fn cl_real_load_class_base(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    class_name_obj: ObjectRef,
) -> cratonvm_types::error::MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let name_pin = ctx.pin_native_root(class_name_obj);
    // JDK 25 `loadClass(String,boolean)` runs under
    // `synchronized (getClassLoadingLock(name))`, which is the loader itself
    // for a loader not registered parallel-capable. This native stands in for
    // that method, so it takes the same monitor there (`--jdk-only`, user
    // loaders; see `loader_locks_itself`). A per-name lock for a
    // parallel-capable loader is not taken, as before. Re-entrant with the
    // VM-initiated load's hold of the same monitor
    // (`constants.rs` `drive_defining_loader_load_named`). Wave 28, lane L5.
    let locked = loader_locks_itself(ctx, this);
    let (this, class_name_obj) = if locked {
        let this = ctx.monitor_enter_gc_safe(this);
        (this, ctx.read_native_pin(name_pin, class_name_obj))
    } else {
        (this, class_name_obj)
    };
    let result = cl_real_load_class_base_rooted(ctx, this, this_pin, class_name_obj, name_pin);
    if locked {
        let this = ctx.read_native_pin(this_pin, this);
        ctx.monitor_exit(this);
    }
    ctx.unpin_native_roots(this_pin);
    result
}

fn cl_real_load_class_base_rooted(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    this_pin: usize,
    class_name_obj: ObjectRef,
    name_pin: usize,
) -> cratonvm_types::error::MethodCallResult {
    // gen r4w3/rooting: function-scoped `mut` bindings so the post-GC-point
    // re-reads below (parent `loadClass`, step-1 lookup, local URL define,
    // `findClass`) reach every later use instead of dying at a block brace.
    let mut this = this;
    let mut class_name_obj = class_name_obj;
    let class_name = ctx.read_string(class_name_obj).unwrap_or_default();
    let internal = class_name.replace('.', "/");
    let __obsreg_dbg =
        crate::vmflags().loader.dbg_obsreg && internal.contains("ObservationRegistry");
    if __obsreg_dbg {
        let this_cls = ctx.class_name_of_id(ctx.class_id_of_object(this));
        let parent_field = ctx.get_field_by_name(this, "parent");
        let parent_cls = match parent_field {
            cratonvm_types::Value::Object(Some(p)) => {
                ctx.class_name_of_id(ctx.class_id_of_object(p))
            }
            _ => None,
        };
        let isolated = crate::classloader::url_classloader_isolated_from_app(ctx, this);
        let platform_singleton = crate::classloader::platform_loader_dbg(ctx.vm_identity());
        eprintln!(
            "[OBSREG-DBG] cl_real_load_class_base ENTER this={:?} this_class={:?} name={} parent_field={:?} parent_class={:?} isolated_from_app={} platform_singleton={:?}",
            this, this_cls, internal, parent_field, parent_cls, isolated, platform_singleton
        );
    }

    // The platform loader owns JDK modules, never application entries. The
    // flat class store is shared by every loader in CratonVM, so allowing its
    // normal fallback here turns platform-parent delegation into accidental
    // application-parent delegation. That breaks URLClassLoader children that
    // intentionally replace or exclude application JARs.
    if crate::classloader::is_platform_class_loader(ctx, this)
        && !crate::classloader::is_runtime_image_class_name(ctx, &internal)
        && !ctx.is_bootstrap_appended_class(&internal)
    {
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &class_name,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
        ));
    }

    // Loader isolation for generated dynamic proxies (JVMS §5.3). A
    // `jdk.proxyN.$ProxyM` lives in its defining loader's per-loader dynamic
    // module, so it must resolve ONLY through a loader that can see it (its
    // defining loader or a delegation descendant). Use the loader-aware
    // `find_loaded_class_for_loader` (step 1 = this loader's own namespace,
    // step 2 = a globally-known proxy whose recorded defining loader IS this
    // loader). The loader-blind `ctx.load_class` global fallback below would
    // otherwise hand ANY loader — a sibling, or the app loader reached via
    // parent-first delegation — another loader's proxy, so
    // `ClassUtils.isCacheSafe(composite, siblingLoader)` wrongly returned true
    // via its `isLoadable` (`siblingLoader.loadClass(name)`) fallback
    // (ClassUtilsTests.isCacheSafe). Generated proxies are never on the
    // classpath, so a loader that cannot see this one has no other way to load
    // it → ClassNotFoundException. (The defining loader registers its proxies
    // under its CANONICAL namespace id — see `proxy_loader_namespace` — so the
    // step-1 scoped lookup here finds them.)
    if crate::classloader::is_generated_proxy_name(&internal) {
        if let Some(mirror) = crate::classloader::find_loaded_class_for_loader(ctx, this, &internal)
        {
            return Ok(Some(Value::Object(Some(mirror))));
        }
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &class_name,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
        ));
    }

    // JVMS §5.3.2 step 1 — `findLoadedClass` FIRST (user-defined loaders only).
    //
    // `ClassLoader.loadClass` begins with `c = findLoadedClass(name); if (c ==
    // null) { ...delegate... }`. A user loader that has itself defined this name
    // — the override-first redefinition case: `TestClassLoader.overrideClass(
    // Entity.class)` then `loadClass("jakarta.persistence.Entity")` — MUST get
    // its OWN copy back, before the flat-global `ctx.load_class` (step 1 below)
    // hands back the application loader's original. HotSpot returns the
    // overridden class here (HHH-7084 `testSystemClassLoaderNotOverriding`);
    // without this, `loadClass` returned the app-loader copy while
    // `findLoadedClass` (correctly) returned the overridden one.
    //
    // `find_loaded_class_for_loader` is the exact, real-mode-safe logic the
    // `findLoadedClass` native uses (identity-hash namespace store +
    // defining-loader registry, NO global fallback), so it returns ONLY a class
    // THIS loader actually defined / is the defining loader of — `None`
    // otherwise, correctly deferring to parent delegation / `findClass`. Scoped
    // to user-defined loaders so built-in (app/platform/bootstrap) resolution is
    // completely unchanged; the null-parent `findClass`-overriding deferral path
    // (Hibernate's `AggregatedClassLoader`) is preserved because that loader has
    // not defined the class itself (→ `None`).
    //
    // `--jdk-only` adds the BARE `URLClassLoader`: it defines its classes in its
    // own namespace there (`defines_in_own_namespace`), through its real
    // `findClass`, so a repeated `loadClass` must find them here rather than
    // define them a second time.
    if crate::classloader::is_user_defined_loader(ctx, this)
        || (ctx.is_jdk_only() && crate::classloader::is_bare_url_class_loader(ctx, this))
    {
        if let Some(mirror) = crate::classloader::find_loaded_class_for_loader(ctx, this, &internal)
        {
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }

    // HIB-CV-24 / SBR-14 — honor a supplied child/isolated `ClassLoader`.
    //
    // Step 1 below (`ctx.load_class`) resolves through CratonVM's flat global
    // store. For a custom loader whose parent is the bootstrap loader (a
    // `super(null)` loader) and which overrides `findClass` to load from its own
    // source (Hibernate's `AggregatedClassLoader`, which iterates scoped child
    // loaders; plugin / test-isolation loaders), that global pre-resolution acts
    // like the application loader and answers the request before the loader's own
    // `findClass` ever runs — so the supplied loader is bypassed and a class it
    // would have defined (or counted) is served from the wrong loader (JVMS §5.3:
    // a bootstrap parent cannot load an application class, so `findClass` MUST
    // run). When such a loader overrides `findClass` and the requested class is
    // not a bootstrap/platform class, defer global resolution to AFTER `findClass`.
    //
    // CRUCIAL: only for a NULL parent. A loader with a non-null (application/
    // platform) parent must keep JVMS parent-first — the parent legitimately
    // loads the class and `findClass` is NOT called (else we would wrongly define
    // app classes under the child loader, diverging from HotSpot). Built-in
    // loaders and bootstrap class names also keep the fast global path. Opt-out:
    // `CRATONVM_CL_BOOTSTRAP_SCOPED=0`.
    let parent = crate::classloader::classloader_parent(ctx, this);
    let parent_is_null = parent.is_none();
    // The platform loader can load JDK modules but not application/test
    // classes. Do not let the flat global class store impersonate an app parent.
    let parent_is_platform = parent.is_some_and(|parent| {
        ctx.class_name_of_id(ctx.class_id_of_object(parent))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader")
    });
    let overrides_find_class = crate::classloader::receiver_overrides_find_class(ctx, this);
    let bootstrap_appended = ctx.is_bootstrap_appended_class(&internal);
    let defer_to_find_class = overrides_find_class
        && (parent_is_null || parent_is_platform)
        && crate::classloader::cl_bootstrap_scoped()
        && !crate::classloader::is_runtime_image_class_name(ctx, &internal)
        && !bootstrap_appended;
    // JVMS 5.3-faithful scoping of the flat-store fallback (real-JDK-mode
    // counterpart of the same fix in classloader.rs's synthetic-mode base
    // delegation — this file has its OWN parallel loadClass implementation,
    // gated on real-vs-synthetic JDK mode, and was missed the first time).
    // CratonVM's global store stands in for "the app classpath, reachable
    // through the parent chain". A loader whose REAL parent chain never
    // passes a built-in loader (e.g. `new ClassLoader(null) {}`, or a loader
    // parented to such) can only see bootstrap classes on HotSpot; answering
    // an application class from the flat store bypasses the loader's own
    // fallback logic (ThrowawayClassLoaderTests.loadingClassFromResourceClosesInputStream:
    // the resource-stream fallback never ran because loadClass resolved the
    // probe class globally first, failing its stream-closing contract test).
    // A loader with a findClass override keeps its step-2b rescue below, so
    // only override-less chains change.
    let scoped_user_chain = crate::classloader::cl_bootstrap_scoped()
        && !crate::classloader::is_runtime_image_class_name(ctx, &internal)
        && !bootstrap_appended
        && !crate::classloader::builtin_loader_reachable(ctx, this);

    // `URLClassLoader.close()` shut this loader's `URLClassPath` (gc-common
    // w12-d): the two searches of its own recorded URLs below are skipped, as
    // on HotSpot, where a closed loader finds nothing new on its URLs but
    // still delegates and still answers the classes it already defined
    // (`findLoadedClass` above). Read once, here: the set is keyed by
    // identity hash, which no later move changes.
    this = ctx.read_native_pin(this_pin, this);
    let closed_url_loader = crate::classloader::ucl_is_closed(ctx, this);

    // A URLClassLoader parented only by bootstrap/platform is intentionally
    // isolated from application entries. Spring Boot's
    // ModifiedClassPathClassLoader uses exactly this topology after removing
    // selected JARs: a process-wide fallback would resurrect the excluded
    // class and make ClassUtils.isPresent report a false positive. Search its
    // recorded URLs, then make the miss authoritative.
    if crate::classloader::url_classloader_isolated_from_app(ctx, this)
        && !crate::classloader::is_runtime_image_class_name(ctx, &internal)
        && !bootstrap_appended
    {
        // `--jdk-only`: the rest of `ClassLoader.loadClass` after an isolated
        // parent's miss is "already loaded here?", then the loader's own
        // `findClass`. For a `URLClassLoader` that is the JDK's own
        // `URLClassLoader.findClass` over its real `URLClassPath`: the
        // family is retired there, and `ucl_try_define_local_class` refuses
        // to stand in for it.
        if ctx.is_jdk_only() {
            if let Some(mirror) =
                crate::classloader::find_loaded_class_for_loader(ctx, this, &internal)
            {
                return Ok(Some(Value::Object(Some(mirror))));
            }
            let this = ctx.read_native_pin(this_pin, this);
            let class_name_obj = ctx.read_native_pin(name_pin, class_name_obj);
            return ctx.invoke_virtual(
                this,
                "findClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[Value::Object(Some(class_name_obj))],
            );
        }
        if !closed_url_loader {
            if let Some(result) =
                crate::classloader::ucl_try_define_local_class(ctx, this, &internal)
            {
                return result;
            }
        }
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &class_name,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
        ));
    }

    // 0. Real parent-first delegation to a USER-DEFINED parent (JVMS §5.3.2
    //    step 2). "Standard VM class loading" below answers through
    //    CratonVM's flat global class store, which stands in for bootstrap
    //    → platform → app delegation — but a CUSTOM parent's own recorded
    //    URLs are deliberately kept OUT of that global store (see
    //    `ucl_try_define_local_class`'s doc comment: "that would make one
    //    temporary loader's classes and resources visible to another"), so
    //    the global store can never answer on a custom parent's behalf.
    //    Without this step a `URLClassLoader` built with a custom parent
    //    (e.g. Spring Boot's `PropertiesLauncher.wrapWithCustomClassLoader`
    //    wrapping a `LaunchedClassLoader`) could never see anything the
    //    parent itself would have resolved — every lookup fell straight to
    //    the (parent-blind) global store and then a bare `ClassNotFoundException`.
    //    Scoped to a genuinely user-defined parent so builtin (app/platform/
    //    bootstrap) parents are unaffected and keep using the faster global
    //    path below. A miss, or a `ClassNotFoundException`/
    //    `NoClassDefFoundError` from the parent, still lets the remaining
    //    fallbacks run (`findClass` override, deferred resolution) — but it is
    //    recorded as AUTHORITATIVE rather than swallowed, which is what
    //    `parent_user_defined_authoritative_miss` below is for. Any OTHER
    //    failure from the parent's `loadClass` propagates (W7-26 R1); this
    //    comment previously read "a miss or exception here is swallowed
    //    (`_ => {}`)", which had already stopped being true of the miss and is
    //    now untrue of the exception as well.
    // Set when the user-defined parent's own `loadClass` was actually
    // invoked (real bytecode) and did NOT hand back a class. That refusal
    // is authoritative — the parent already ran its own full delegation/
    // exclusion logic (which, for a loader like Spring Boot's
    // `ModifiedClassPathClassLoader`, deliberately narrows its own URL list
    // via `@ClassPathExclusions`), and CratonVM's flat global store mixes
    // together every loader's classpath, so it can still find a class the
    // parent specifically hid. Falling through to "standard VM class
    // loading" below after such a refusal defeats the exclusion: a
    // `ResourcesClassLoader` (installed by `@WithPackageResources`) wrapping
    // a correctly-exclusion-aware `ModifiedClassPathClassLoader` parent got
    // `ClassNotFoundException` from the parent's own `loadClass`, but that
    // `Err` doesn't match the `Ok(Some(Object(Some(_))))` pattern above, so
    // it was silently swallowed and step 1 re-resolved the excluded class
    // globally anyway — `@ConditionalOnClass` checks made through such a
    // loader then saw a class the exclusion was written to hide. See
    // data-redis-jedis-sslbundle-withpackageresources-classloader-leak-FIXED.md.
    let mut parent_user_defined_authoritative_miss = false;
    if let Some(parent) = parent {
        if crate::classloader::is_user_defined_loader(ctx, parent) {
            let delegated = ctx.invoke_virtual(
                parent,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[Value::Object(Some(class_name_obj))],
            );
            // Arbitrary Java ran: refresh both locals before the
            // fall-through arms below reuse them.
            // gen r4w3/rooting: ASSIGN (not shadow) so the refresh outlives
            // this block and reaches steps 1/2/2b below.
            this = ctx.read_native_pin(this_pin, this);
            class_name_obj = ctx.read_native_pin(name_pin, class_name_obj);
            match delegated {
                Ok(Some(Value::Object(Some(mirror)))) => {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
                // The parent ran and answered null (or void). That is the
                // authoritative miss this flag was added for — unchanged.
                Ok(_) => {
                    parent_user_defined_authoritative_miss = true;
                }
                // W7-26 R1 — the arm above used to be a bare `_ =>` that
                // covered this one too, so ANY failure from the parent's own
                // `loadClass` read as "the parent does not have it". JDK 25
                // `ClassLoader.loadClass` catches `ClassNotFoundException`
                // here and nothing else, so a `LinkageError`, an
                // `ExceptionInInitializerError` from the parent loader's own
                // `<clinit>`, or an `OutOfMemoryError` was being reported to
                // the caller as a plain `ClassNotFoundException` naming this
                // class (step 3 below) — the exception's identity gone and
                // the real cause unnameable. `absorb_class_absent` keeps the
                // `ClassNotFoundException`/`NoClassDefFoundError` half, which
                // is every case the exclusion-aware loaders this branch
                // exists for actually produce.
                Err(failed) => {
                    absorb_class_absent(&*ctx, failed)?;
                    parent_user_defined_authoritative_miss = true;
                }
            }
        }
    }

    // 1. Standard VM class loading (skipped when deferring to a custom findClass,
    //    or when the loader's chain cannot reach a built-in loader, or when a
    //    user-defined parent already authoritatively refused above).
    // `ClassLoader.loadClass` must never answer with a FABRICATED synthetic
    // stub. The stub mechanism exists so enterprise *bytecode* can link
    // against classes that are genuinely absent (`is_enterprise_stub_prefix`:
    // `io/quarkus/`, `org/jboss/`, `io/smallrye/`, ...); HotSpot's
    // `loadClass` has no such notion and throws `ClassNotFoundException`.
    // Returning one here breaks every loader that *depends* on the parent
    // refusing:
    //
    //   Quarkus's fast-jar `RunnerClassLoader` lists `io.quarkus.value.registry`
    //   (among 40 packages) as PARENT-FIRST. It calls
    //   `getParent().loadClass(name)` inside `try { } catch
    //   (ClassNotFoundException)` and, on the expected refusal, falls through
    //   to its own index -- which has the class, in
    //   `lib/quarkus/generated-bytecode.jar`. CratonVM's app loader instead
    //   answered with a stub, so Arc's generated `ValueRegistry_..._Synthetic_Bean`
    //   became a method-less, interface-less class registered globally under
    //   `Application`, and `ArcContainerImpl` died casting it to
    //   `InjectableBean`.
    //
    // The stub is still minted by CratonVM's own constant-pool / `Class.forName`
    // fallbacks when nothing else can supply the name -- this only stops the
    // explicit `loadClass` API from manufacturing one mid-delegation.
    // `CRATONVM_CL_STUB_DELEGATION=1` restores the legacy behaviour.
    let stub_would_answer_delegation =
        !stub_may_answer_load_class() && ctx.would_fabricate_synthetic_stub(&internal);
    if !parent_user_defined_authoritative_miss
        && !defer_to_find_class
        && !scoped_user_chain
        && !stub_would_answer_delegation
    {
        match load_class_visible_to(ctx, this, &internal) {
            ClassLookup::Found(mirror) => return Ok(Some(mirror)),
            ClassLookup::DependencyMissing(missing) => {
                let exc = no_class_def_found_error(ctx, &missing);
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exc?,
                ));
            }
            ClassLookup::NotFound => {}
        }
        if let Some(mirror) =
            crate::jboss_module_loader::load_property_bridge_class(ctx, &class_name)
        {
            return Ok(Some(mirror));
        }
        // gen r4w3/rooting: step-1 class loading / property-bridge loading
        // can allocate or upcall; refresh before the uses below.
        this = ctx.read_native_pin(this_pin, this);
    }

    // URLClassLoader searches its recorded URLs after parent delegation. The
    // helper is a no-op for loaders without recorded URLs, and subclasses do
    // not always expose their inherited URLClassLoader identity here. A closed
    // loader's URLs are shut (gc-common w12-d, see `closed_url_loader`).
    if !bootstrap_appended && !closed_url_loader {
        if let Some(result) = crate::classloader::ucl_try_define_local_class(ctx, this, &internal) {
            return result;
        }
        // gen r4w3/rooting: the local URL search may read/fetch/define
        // (GC-capable) even on a miss; refresh both locals.
        this = ctx.read_native_pin(this_pin, this);
        class_name_obj = ctx.read_native_pin(name_pin, class_name_obj);
    }

    // 2. Custom-classloader extension point: if the receiver overrides
    //    `findClass`, the JVM `loadClass` contract requires us to call it.
    //    `invoke_virtual` resolves on the receiver's actual class, so this
    //    dispatches to the subclass's overriding `findClass` bytecode.
    if overrides_find_class {
        let result = ctx.invoke_virtual(
            this,
            "findClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(class_name_obj))],
        )?;
        // The override is the loader's own Java (Hibernate's
        // AggregatedClassLoader iterates its scoped child loaders here);
        // refresh before the fall-through arms reuse either local.
        // gen r4w3/rooting: ASSIGN so the refreshed `this` reaches step 2b's
        // `load_class_visible_to` below, outside this block.
        this = ctx.read_native_pin(this_pin, this);
        match result {
            // findClass produced the class — that is the answer.
            Some(Value::Object(Some(_))) => return Ok(result),
            // A miss from URLClassLoader's own native URL/HTTP search is
            // authoritative -- propagate it (e.g. ClassNotFoundException)
            // rather than falling through to step 2b's global-store
            // fallback, which would let a null-parent URLClassLoader resolve
            // application classes its own (failed) URL search should have
            // hidden from it. See docs/known-issues/keycloak/
            // test-classserver-invalidpackage-classnotfound-not-thrown.md.
            _ if defer_to_find_class
                && crate::classloader::find_class_is_urlclassloader_native(ctx, this) =>
            {
                return Ok(result);
            }
            // In the deferred case, findClass missing/throwing falls through to
            // the global store as the last resort. Otherwise propagate.
            _ if !defer_to_find_class => return Ok(result),
            _ => {}
        }
    }

    // 2b. Deferred-resolution last resort (HIB-CV-24): CratonVM's flat store is
    //     the only source of application classes, so a findClass-overriding loader
    //     whose override legitimately misses still resolves here.
    if defer_to_find_class && !scoped_user_chain {
        match load_class_visible_to(ctx, this, &internal) {
            ClassLookup::Found(mirror) => return Ok(Some(mirror)),
            ClassLookup::DependencyMissing(missing) => {
                let exc = no_class_def_found_error(ctx, &missing);
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exc?,
                ));
            }
            ClassLookup::NotFound => {}
        }
    }

    // 3. Genuinely not found and no user override — throw CNFE per spec.
    let exc = crate::jboss_module_loader::alloc_single_message_exception(
        ctx,
        "java/lang/ClassNotFoundException",
        1,
        &class_name,
    );
    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
        exc?,
    ))
}

/// Invoke the real-mode base `ClassLoader.loadClass(String, boolean)` bridge
/// from a registered subclass native without re-entering subclass dispatch.
pub(crate) fn cl_real_load_class_base_from_args(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(cl_real_load_class_base(ctx, this, name_obj)?)
}

/// Real-JDK-mode `URLClassLoader.findClass(String)`.
///
/// The real JDK bytecode resolves the class through the `ucp`
/// (`jdk.internal.loader.URLClassPath`) field, which CratonVM shims to a bare
/// synthetic instance (`register_url_class_path_safe_stubs`) whose `getResource`
/// returns null — so the real `findClass` ALWAYS throws `ClassNotFoundException`.
/// CratonVM instead first serves `URLClassLoader` resolution from the receiver's
/// own recorded URLs, defining the class under that receiver's loader namespace,
/// then falls back to the historical global dynamic classpath path where each
/// loader's `<init>` registered its URLs via [`register_url_array`]. Throw
/// `ClassNotFoundException` on a genuine miss, matching the JDK `findClass`
/// contract.
///
/// `URLClassLoader.findClass` is normally reached only from `loadClass` (which
/// CratonVM serves natively via [`cl_real_load_class`], never touching the real
/// `findClass`). It becomes reachable when a `URLClassLoader` subclass overrides
/// `loadClass` and calls `findClass(name)` directly — most notably Jasper's
/// `JasperLoader`, which loads the runtime-compiled `org.apache.jsp.*_jsp`
/// servlet from its scratch-dir URL. Without this native every compiled JSP/tag
/// 500s with `ClassNotFoundException: org.apache.jsp.*_jsp`. The dispatch is
/// forced onto this native by `intercept_urlclassloader_subclass_find_class`
/// (the subclass methodref escapes the static-class force-native gate).
pub fn ucl_real_find_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_name = ctx.read_string(name_obj).unwrap_or_default();
    // A CLOSED loader must stop finding classes it has not already loaded —
    // that is the whole observable half of `URLClassLoader.close()` (already
    // defined classes stay defined; `close()` unloads nothing). Checked HERE
    // because in real-JDK mode this native IS `findClass`: the real bytecode
    // would consult `ucp`, which CratonVM leaves unpopulated, so closing `ucp`
    // has no effect and every post-close load used to succeed.
    //
    // One per-VM set for both `close()` natives (gc-common w12-d). This
    // checked the synthetic set and then a second, real-mode one, which was
    // process-wide and keyed by the bare identity hash.
    if crate::classloader::ucl_is_closed(ctx, this) {
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &class_name,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
        ));
    }
    let internal = class_name.replace('.', "/");
    // gc-common w20-f handoff: the local URL search may wait for a peer
    // definer, fetch or define (GC-capable) even on a miss; re-read the loader
    // after it.
    let this_pin = ctx.pin_native_root(this);
    let local = crate::classloader::ucl_try_define_local_class(ctx, this, &internal);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(result) = local {
        return result;
    }
    if crate::classloader::url_classloader_isolated_from_app(ctx, this)
        && !ctx.is_bootstrap_appended_class(&internal)
    {
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &class_name,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
        ));
    }
    // W7-26 R2 — this was `if let Ok(Some(mirror)) = ctx.load_class(...)`, so
    // EVERY failure fell through to the `ClassNotFoundException` below. That
    // is a re-raise with the wrong type, which is worse than a propagate:
    // `URLClassLoader.findClass` raises `ClassNotFoundException` only when the
    // class file cannot be FOUND. When it is found and cannot be used, HotSpot
    // lets `defineClass`'s failure out — a `ClassFormatError`, a
    // `VerifyError`, an `UnsupportedClassVersionError`, an
    // `IncompatibleClassChangeError` — and a caller's
    // `catch (ClassNotFoundException)` deliberately does not match any of
    // them. Reporting "not found" for a class that is present but malformed
    // sends every loader in the corpus down its "try the next source" path,
    // and the real cause is unnameable from the Java side.
    match ctx.load_class(&internal) {
        Ok(Some(mirror)) => return Ok(Some(mirror)),
        // No mirror and no failure — genuinely absent; fall through to the
        // `ClassNotFoundException` below, exactly as before.
        Ok(None) => {}
        Err(failed) => absorb_class_absent(&*ctx, failed)?,
    }
    let exc = crate::jboss_module_loader::alloc_single_message_exception(
        ctx,
        "java/lang/ClassNotFoundException",
        1,
        &class_name,
    );
    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
        exc?,
    ))
}

/// Get or create the system (app) class loader singleton.
pub fn get_or_create_system_cl(
    ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // IDENTITY-UNIFIED with the loader object `Class.getClassLoader()`
    // returns (classloader.rs::get_or_create_app_loader). These used to be
    // two distinct singletons — a plain "java/lang/ClassLoader" here vs the
    // synthetic ClassLoaders$AppClassLoader there — so
    // `someClass.getClassLoader() == ClassLoader.getSystemClassLoader()`
    // was FALSE. Gradle's ClassLoaderVisitor uses exactly that identity
    // check to decide "this is the system loader, read java.class.path";
    // on the mismatch it extracted an EMPTY classpath, the module registry
    // found no Gradle installation, and every ProjectBuilder bootstrap died
    // ("Cannot find resource 'gradle-plugins.properties'"). One object,
    // one identity, kept (per VM, GC-scanned and remapped) by classloader.rs's
    // app-loader store. gc-common w12-d deleted the write-only process-wide
    // `SYSTEM_CL` / `PLATFORM_CL` statics this also wrote: unrooted,
    // never remapped, shared by every VM, and never read.
    let obj = crate::classloader::get_or_create_app_loader(ctx)?;
    Ok(Some(obj))
}

/// Get or create the platform class loader singleton.
fn get_or_create_platform_cl(
    ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // Identity-unified with classloader.rs::get_or_create_platform_loader —
    // same rationale as `get_or_create_system_cl` above.
    let obj = crate::classloader::get_or_create_platform_loader(ctx)?;
    Ok(Some(obj))
}

/// gc-common w12-d (`common-w11e-real-mode-closed-url-loader-set-is-one-per-process`):
/// the real-JDK `URLClassLoader.close()` bridge writes the per-VM closed set
/// the w9-a / w11-e fixes made, so its state is keyed by VM, dropped at VM
/// teardown and pruned when the loader dies. The twin of
/// `classloader::w9a_closed_url_loader_state_is_per_vm` for the real-mode pair
/// (`servlet::register_url_classloader_close_bridge` writing,
/// `ucl_is_closed` / `ucl_real_find_class` reading).
#[cfg(test)]
mod w12d_real_mode_closed_loader_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    const VM_A: usize = 0x12D_1A00;
    const VM_B: usize = 0x12D_1B00;

    /// Drops both VMs' loader rows even when an assertion fails.
    struct Teardown;
    impl Drop for Teardown {
        fn drop(&mut self) {
            for vm in [VM_A, VM_B] {
                crate::classloader::forget_vm_loader_singletons(vm);
            }
        }
    }

    fn vm_ctx(vm: usize) -> MockNativeContext {
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(vm);
        ctx
    }

    /// `URLClassLoader.close()` through the real-JDK bridge. The mock answers
    /// `getURLs` with null, so no classpath root is retracted.
    fn real_close(ctx: &mut MockNativeContext, loader: ObjectRef) {
        let mut r = NativeMethodRegistry::new();
        crate::servlet::register_url_classloader_close_bridge(&mut r);
        let close = r
            .find("java/net/URLClassLoader", "close", "()V")
            .expect("the real-JDK close() bridge is registered");
        assert!(close(ctx, &[Value::Object(Some(loader))]).is_ok());
    }

    #[test]
    fn real_mode_close_is_per_vm_dropped_at_teardown_and_pruned() {
        let _t = Teardown;
        let mut ctx_a = vm_ctx(VM_A);
        let mut ctx_b = vm_ctx(VM_B);
        // Two fresh mocks normally number their objects alike, so the two
        // loaders share an identity hash: the collision two real heaps
        // produce, and the case the old bare-hash set got wrong. The
        // assertions hold either way.
        let loader_a = ctx_a.fresh_object_ref();
        let loader_b = ctx_b.fresh_object_ref();

        real_close(&mut ctx_a, loader_a);
        assert!(crate::classloader::ucl_is_closed(&ctx_a, loader_a));
        assert!(
            crate::classloader::ucl_is_closed(&ctx_a, loader_a),
            "one closed set serves the synthetic and the real-mode readers"
        );
        assert!(
            !crate::classloader::ucl_is_closed(&ctx_b, loader_b),
            "A's close() must not close B's loader, even on a shared identity hash"
        );

        // B closes its own loader; A's teardown drops only A's row.
        real_close(&mut ctx_b, loader_b);
        crate::classloader::forget_vm_loader_singletons(VM_A);
        assert!(
            !crate::classloader::ucl_is_closed(&ctx_a, loader_a),
            "A's teardown drops A's row"
        );
        assert!(crate::classloader::ucl_is_closed(&ctx_b, loader_b), "and keeps B's");

        // B's loader dies: B's reconcile prunes the row with it.
        let dead = loader_b.as_ptr() as usize;
        let is_marked = move |addr: usize| addr != dead;
        let _ = crate::classloader::gc_reconcile_defining_loaders(
            VM_B,
            &is_marked,
            &cratonvm_types::PointerMap::default(),
            None,
        );
        assert!(
            !crate::classloader::ucl_is_closed(&ctx_b, loader_b),
            "a dead loader's closed row goes with it"
        );
    }
}
