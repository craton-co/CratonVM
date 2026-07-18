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

use std::sync::Mutex;

use cratonvm_native_api::registry::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ObjectRef, Value};

use crate::alloc_concurrent_synthetic;

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
    // Make `getURLs()` return the loader's URLs (stashed on the `ucp`
    // placeholder). Without this, real-mode `getURLs()` falls through to the
    // empty `URLClassPath` shim and a classloader's URLs are invisible to
    // callers like Tomcat's `StandardJarScanner`. Runs before classpath
    // registration so it happens even when no path is extractable.
    crate::classloader::record_ucl_urls(ctx, this, urls);
    let dbg = std::env::var_os("CRATONVM_DBG_UCLREG").is_some();
    let arr = match urls {
        Value::Object(Some(a)) => a,
        _ => {
            if dbg {
                eprintln!("[UCLREG-DBG] <init> urls arg not an array: {urls:?}");
            }
            return;
        }
    };
    let count = ctx.array_length(arr);
    let mut paths = Vec::with_capacity(count);
    for i in 0..count {
        if let Value::Object(Some(url_obj)) = ctx.get_array_element(arr, i) {
            if let Some(p) = extract_url_path(ctx, url_obj) {
                if !p.is_empty() {
                    paths.push(p);
                }
            } else if dbg {
                eprintln!("[UCLREG-DBG]   url[{i}] extract_url_path -> None");
            }
        } else if dbg {
            eprintln!("[UCLREG-DBG]   url[{i}] not an object");
        }
    }
    if dbg {
        eprintln!("[UCLREG-DBG] <init> count={count} extracted={:?}", paths);
    }
    // A URLClassLoader owns an isolated search path. Publishing constructor
    // URLs into CratonVM's process-wide application path made later temporary
    // loaders observe resources from earlier ones. The URLClassLoader natives
    // resolve from the recorded per-loader URLs, so do not flatten them here.
}

/// Cached system/platform classloader objects (created lazily).
///
/// GC note (gc-followups-20260706): BOTH statics are WRITE-ONLY — every read
/// path goes through `classloader::get_or_create_{app,platform}_loader`, whose
/// own stores are GC-scanned + remapped (`gc_scan_loader_singleton_roots` /
/// `gc_update_loader_singleton_refs`). The raw refs cached here are neither
/// rooted nor remapped, so they go stale after a moving GC — GC-safe ONLY
/// because nothing ever reads them back. Do not add readers; read via the
/// classloader.rs singletons instead.
static SYSTEM_CL: Mutex<Option<ObjectRef>> = Mutex::new(None);
static PLATFORM_CL: Mutex<Option<ObjectRef>> = Mutex::new(None);

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
fn init_classloader_common_fields(ctx: &mut dyn NativeContext, this: ObjectRef) {
    // GC-safety: every `alloc_concurrent_synthetic`/`new_object`/`invoke` call
    // below can trigger a moving GC; `this` (and, briefly, `cs`) are each
    // reused repeatedly across multiple such hazards, unpinned otherwise.
    // Same "Family 1" stale-ObjectRef pattern as the WildFly boot-crash
    // fixes (see docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md)
    // -- pin both now and re-read the forwarded reference right before use.
    let this_pin = ctx.pin_native_root(this);
    // defaultDomain → ProtectionDomain(CodeSource(null URL, null certs),
    // null perms, this loader, null principals). Build CodeSource first.
    let cs = alloc_concurrent_synthetic(ctx, "java/security/CodeSource", 2);
    let cs_pin = ctx.pin_native_root(cs);
    ctx.set_field_by_name(cs, "location", Value::Object(None));
    ctx.set_field_by_name(cs, "certs", Value::Object(None));

    let pd = alloc_concurrent_synthetic(ctx, "java/security/ProtectionDomain", 4);
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
    let classes = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 4);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "classes", Value::Object(Some(classes)));

    // `packages` — ConcurrentHashMap; `ClassLoader.packages()` does
    // `getfield packages → values()` and would NPE on null.
    let packages = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "packages", Value::Object(Some(packages)));

    // `package2certs` — ConcurrentHashMap consulted by `checkCerts`.
    let pkg2certs = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "package2certs", Value::Object(Some(pkg2certs)));

    // `parallelLockMap` — used by `getClassLoadingLock`.
    let lock_map = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 16);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "parallelLockMap", Value::Object(Some(lock_map)));

    // `assertionLock` — `setDefaultAssertionStatus` synchronizes on it.
    let lock = alloc_concurrent_synthetic(ctx, "java/lang/Object", 0);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "assertionLock", Value::Object(Some(lock)));

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
    ctx.unpin_native_roots(this_pin);
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
    let diag = std::env::var_os("CRATONVM_DBG_URLCL").is_some();
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
fn init_urlclassloader_constructor(ctx: &mut dyn NativeContext, this: ObjectRef, urls: Value) {
    let this_pin = ctx.pin_native_root(this);
    let urls_pin = match urls {
        Value::Object(Some(urls)) => Some((ctx.pin_native_root(urls), urls)),
        _ => None,
    };

    let this = ctx.read_native_pin(this_pin, this);
    init_classloader_common_fields(ctx, this);
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
        let sys = get_or_create_system_cl(ctx);
        ctx.set_field_by_name(this, "parent", Value::Object(sys));
        // Initialise defaultDomain / classes / packages etc. — see
        // `init_classloader_common_fields`. Without this a subclass that
        // calls `defineClass` NPEs in `preDefineClass` on a null
        // `defaultDomain`.
        init_classloader_common_fields(ctx, this);
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
        init_classloader_common_fields(ctx, this);
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
            init_classloader_common_fields(ctx, this);
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

    // ClassLoader.getName() — read name field
    r.register(cl, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let name = ctx.get_field_by_name(this, "name");
        match name {
            Value::Object(Some(_)) => Ok(Some(name)),
            _ => {
                let s = ctx.create_string("");
                Ok(Some(Value::Object(Some(s))))
            }
        }
    });

    // ClassLoader.getSystemClassLoader() — return the system (app) class loader
    r.register(
        cl,
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
        |ctx, _args| {
            let sys = get_or_create_system_cl(ctx);
            Ok(Some(Value::Object(sys)))
        },
    );

    // ClassLoader.getPlatformClassLoader() — return platform class loader
    r.register(
        cl,
        "getPlatformClassLoader",
        "()Ljava/lang/ClassLoader;",
        |ctx, _args| {
            let platform = get_or_create_platform_cl(ctx);
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
        |ctx, _args| {
            let m = alloc_concurrent_synthetic(ctx, "java/lang/Module", 2);
            ctx.set_field(m, 0, Value::Object(None)); // null name => unnamed => isNamed() == false
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
            let internal = class_name.replace('.', "/");
            // JVMS §5.3 / no-load: report a class only if THIS loader loaded it.
            // For a user-defined loader this is its own namespace or the classes
            // it is the recorded defining loader of — so a fresh custom loader
            // gets null for an app-loaded class and its override-first
            // redefinition fires. Built-in loaders keep the global (no-load)
            // lookup. (Shared with the synthetic-mode `cl_find_loaded_class`.)
            match crate::classloader::find_loaded_class_for_loader(ctx, this, &internal) {
                Some(mirror) => Ok(Some(Value::Object(Some(mirror)))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );

    // ClassLoader.registerAsParallelCapable() — always return true
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
        let parent = get_or_create_system_cl(ctx);
        ctx.set_field_by_name(this, "parent", Value::Object(parent));
        let urls = args.get(1).copied().unwrap_or(Value::Object(None));
        // URLClassLoader extends SecureClassLoader extends ClassLoader;
        // the inherited `defaultDomain` / `classes` / `packages` fields
        // still need initialising (see `init_classloader_common_fields`).
        // `closeables` / `ucp` — see `init_urlclassloader_fields`.
        // Register the URLs with the application classpath so classes inside
        // the jars/dirs are actually loadable — without this a custom
        // URLClassLoader (Tomcat's CommonClassLoader, ActiveMQ's launcher)
        // can never find its classes and throws ClassNotFoundException.
        init_urlclassloader_constructor(ctx, this, urls);
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
            init_urlclassloader_constructor(ctx, this, urls);
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
            init_urlclassloader_constructor(ctx, this, urls);
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
            init_urlclassloader_constructor(ctx, this, urls);
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
            init_urlclassloader_constructor(ctx, this, urls);
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
            let urls = args.get(1).copied().unwrap_or(Value::Object(None));
            let acc = args.get(2).copied().unwrap_or(Value::Object(None));
            let parent = get_or_create_system_cl(ctx);
            ctx.set_field_by_name(this, "parent", Value::Object(parent));
            ctx.set_field_by_name(this, "acc", acc);
            init_urlclassloader_constructor(ctx, this, urls);
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
            init_urlclassloader_constructor(ctx, this, urls);
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
        return ctx.invoke_virtual(
            this,
            "loadClass",
            "(Ljava/lang/String;Z)Ljava/lang/Class;",
            &[Value::Object(Some(class_name_obj)), Value::Int(0)],
        );
    }

    cl_real_load_class_base(ctx, this, class_name_obj)
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
    let mirror_val = match ctx.load_class(internal) {
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
        )) if class_name != internal => {
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
) -> ObjectRef {
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
        let cause = ctx.read_native_pin(cause_pin, cause);
        let result = ctx.new_object_initialized(
            "java/lang/NoClassDefFoundError",
            "(Ljava/lang/String;Ljava/lang/Throwable;)V",
            &[Value::Object(Some(ncdfe_msg)), Value::Object(Some(cause))],
        );
        ctx.unpin_native_roots(cause_pin);
        if let Ok(Some(Value::Object(Some(exc)))) = result {
            return exc;
        }
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
fn cl_real_load_class_base(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    class_name_obj: ObjectRef,
) -> cratonvm_types::error::MethodCallResult {
    let class_name = ctx.read_string(class_name_obj).unwrap_or_default();
    let internal = class_name.replace('.', "/");

    // The platform loader owns JDK modules, never application entries. The
    // flat class store is shared by every loader in CratonVM, so allowing its
    // normal fallback here turns platform-parent delegation into accidental
    // application-parent delegation. That breaks URLClassLoader children that
    // intentionally replace or exclude application JARs.
    if crate::classloader::is_platform_class_loader(ctx, this)
        && !crate::classloader::is_bootstrap_class_name(&internal)
        && !cratonvm_classloading::is_bootstrap_appended_class(&internal)
    {
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &class_name,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc,
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
            exc,
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
    if crate::classloader::is_user_defined_loader(ctx, this) {
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
    let parent = match ctx.get_field_by_name(this, "parent") {
        Value::Object(Some(parent)) => Some(parent),
        Value::Object(None) | Value::Int(0) | Value::Long(0) => None,
        _ => None,
    };
    let parent_is_null = parent.is_none();
    // The platform loader can load JDK modules but not application/test
    // classes. Do not let the flat global class store impersonate an app parent.
    let parent_is_platform = parent.is_some_and(|parent| {
        ctx.class_name_of_id(ctx.class_id_of_object(parent))
            .is_some_and(|name| name == "jdk/internal/loader/ClassLoaders$PlatformClassLoader")
    });
    let overrides_find_class = crate::classloader::receiver_overrides_find_class(ctx, this);
    let defer_to_find_class = overrides_find_class
        && (parent_is_null || parent_is_platform)
        && crate::classloader::cl_bootstrap_scoped()
        && !crate::classloader::is_bootstrap_class_name(&internal);
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
        && !crate::classloader::is_bootstrap_class_name(&internal)
        && !cratonvm_classloading::is_bootstrap_appended_class(&internal)
        && !crate::classloader::builtin_loader_reachable(ctx, this);

    // A URLClassLoader parented only by bootstrap/platform is intentionally
    // isolated from application entries. Spring Boot's
    // ModifiedClassPathClassLoader uses exactly this topology after removing
    // selected JARs: a process-wide fallback would resurrect the excluded
    // class and make ClassUtils.isPresent report a false positive. Search its
    // recorded URLs, then make the miss authoritative.
    if crate::classloader::url_classloader_isolated_from_app(ctx, this)
        && !crate::classloader::is_bootstrap_class_name(&internal)
    {
        if let Some(result) = crate::classloader::ucl_try_define_local_class(ctx, this, &internal) {
            return result;
        }
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            &class_name,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc,
        ));
    }

    // 1. Standard VM class loading (skipped when deferring to a custom findClass,
    //    or when the loader's chain cannot reach a built-in loader).
    if !defer_to_find_class && !scoped_user_chain {
        match load_class_visible_to(ctx, this, &internal) {
            ClassLookup::Found(mirror) => return Ok(Some(mirror)),
            ClassLookup::DependencyMissing(missing) => {
                let exc = no_class_def_found_error(ctx, &missing);
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exc,
                ));
            }
            ClassLookup::NotFound => {}
        }
        if let Some(mirror) =
            crate::jboss_module_loader::load_property_bridge_class(ctx, &class_name)
        {
            return Ok(Some(mirror));
        }
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
        );
        match result {
            // findClass produced the class — that is the answer.
            Ok(Some(Value::Object(Some(_)))) => return result,
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
                return result;
            }
            // In the deferred case, findClass missing/throwing falls through to
            // the global store as the last resort. Otherwise propagate.
            _ if !defer_to_find_class => return result,
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
                    exc,
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
        exc,
    ))
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
    let internal = class_name.replace('.', "/");
    if let Some(result) = crate::classloader::ucl_try_define_local_class(ctx, this, &internal) {
        return result;
    }
    if let Ok(Some(mirror)) = ctx.load_class(&internal) {
        return Ok(Some(mirror));
    }
    let exc = crate::jboss_module_loader::alloc_single_message_exception(
        ctx,
        "java/lang/ClassNotFoundException",
        1,
        &class_name,
    );
    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
        exc,
    ))
}

/// Get or create the system (app) class loader singleton.
pub fn get_or_create_system_cl(
    ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
) -> Option<ObjectRef> {
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
    // one identity. (The old SYSTEM_CL static stays only as a write-only
    // alias to whatever we hand out — it is NOT a GC root and is never
    // remapped, which is safe solely because it is never read back; the
    // canonical GC-tracked copy lives in classloader.rs's app-loader store.)
    let obj = crate::classloader::get_or_create_app_loader(ctx);
    *SYSTEM_CL.lock().unwrap_or_else(|e| e.into_inner()) = Some(obj);
    Some(obj)
}

/// Get or create the platform class loader singleton.
fn get_or_create_platform_cl(
    ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
) -> Option<ObjectRef> {
    // Identity-unified with classloader.rs::get_or_create_platform_loader —
    // same rationale as `get_or_create_system_cl` above.
    let obj = crate::classloader::get_or_create_platform_loader(ctx);
    *PLATFORM_CL.lock().unwrap_or_else(|e| e.into_inner()) = Some(obj);
    Some(obj)
}
