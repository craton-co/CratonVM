//! ClassLoader natives for real-JDK mode.
//!
//! The real JDK `ClassLoader.<init>` is extremely complex — it creates
//! ArrayList, ProtectionDomain, NativeLibraries, Module, ConcurrentHashMap,
//! and more. Many of these depend on JDK internals we don't fully support.
//!
//! These simplified natives use `set_field_by_name`/`get_field_by_name` so
//! they work with the real JDK field layout (not hardcoded synthetic indices).

use std::sync::Mutex;

use rustjvm_native_api::registry::{NativeContext, NativeMethodRegistry};
use rustjvm_types::{ObjectRef, Value};

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
    if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
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
fn register_url_array(ctx: &mut dyn NativeContext, urls: Value) {
    let arr = match urls {
        Value::Object(Some(a)) => a,
        _ => return,
    };
    let count = ctx.array_length(arr);
    let mut paths = Vec::with_capacity(count);
    for i in 0..count {
        if let Value::Object(Some(url_obj)) = ctx.get_array_element(arr, i) {
            if let Some(p) = extract_url_path(ctx, url_obj) {
                if !p.is_empty() {
                    paths.push(p);
                }
            }
        }
    }
    if !paths.is_empty() {
        tracing::debug!(
            "URLClassLoader.<init> (real-JDK): registering {} URL(s) to classpath",
            paths.len()
        );
        ctx.register_dynamic_classpath(&paths);
    }
}

/// Cached system/platform classloader objects (created lazily).
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
    // defaultDomain → ProtectionDomain(CodeSource(null URL, null certs),
    // null perms, this loader, null principals). Build CodeSource first.
    let cs = alloc_concurrent_synthetic(ctx, "java/security/CodeSource", 2);
    ctx.set_field_by_name(cs, "location", Value::Object(None));
    ctx.set_field_by_name(cs, "certs", Value::Object(None));

    let pd = alloc_concurrent_synthetic(ctx, "java/security/ProtectionDomain", 4);
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
    ctx.set_field_by_name(this, "classes", Value::Object(Some(classes)));

    // `packages` — ConcurrentHashMap; `ClassLoader.packages()` does
    // `getfield packages → values()` and would NPE on null.
    let packages = alloc_concurrent_synthetic(
        ctx,
        "java/util/concurrent/ConcurrentHashMap",
        16,
    );
    ctx.set_field_by_name(this, "packages", Value::Object(Some(packages)));

    // `package2certs` — ConcurrentHashMap consulted by `checkCerts`.
    let pkg2certs = alloc_concurrent_synthetic(
        ctx,
        "java/util/concurrent/ConcurrentHashMap",
        16,
    );
    ctx.set_field_by_name(this, "package2certs", Value::Object(Some(pkg2certs)));

    // `parallelLockMap` — used by `getClassLoadingLock`.
    let lock_map = alloc_concurrent_synthetic(
        ctx,
        "java/util/concurrent/ConcurrentHashMap",
        16,
    );
    ctx.set_field_by_name(this, "parallelLockMap", Value::Object(Some(lock_map)));

    // `assertionLock` — `setDefaultAssertionStatus` synchronizes on it.
    let lock = alloc_concurrent_synthetic(ctx, "java/lang/Object", 0);
    ctx.set_field_by_name(this, "assertionLock", Value::Object(Some(lock)));
}

/// Register ClassLoader natives needed in real-JDK mode.
///
/// These override the complex real-JDK constructors with minimal versions
/// that just store the parent reference, and provide getParent() that reads
/// it back.
pub fn register_classloader_real_natives(r: &mut NativeMethodRegistry) {
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
    r.register(
        cl,
        "getParent",
        "()Ljava/lang/ClassLoader;",
        |ctx, args| {
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
        },
    );

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

    // ClassLoader.findLoadedClass(String) — check if class is already loaded
    r.register(
        cl,
        "findLoadedClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| {
            let class_name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let class_name = ctx.read_string(class_name_obj).unwrap_or_default();
            let internal = class_name.replace('.', "/");
            match ctx.load_class(&internal) {
                Ok(Some(mirror)) => Ok(Some(mirror)),
                _ => Ok(Some(Value::Object(None))),
            }
        },
    );

    // ClassLoader.registerAsParallelCapable() — always return true
    r.register(
        cl,
        "registerAsParallelCapable",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );

    // URLClassLoader ctors — Spring Boot launcher allocates
    // `LaunchedURLClassLoader` via these signatures early in boot.
    r.register(ucl, "<init>", "([Ljava/net/URL;)V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let parent = get_or_create_system_cl(ctx);
        ctx.set_field_by_name(this, "parent", Value::Object(parent));
        // Best-effort: stash URL array into known field name when present.
        let urls = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field_by_name(this, "ucp", urls);
        // URLClassLoader extends SecureClassLoader extends ClassLoader;
        // the inherited `defaultDomain` / `classes` / `packages` fields
        // still need initialising (see `init_classloader_common_fields`).
        init_classloader_common_fields(ctx, this);
        // Register the URLs with the application classpath so classes inside
        // the jars/dirs are actually loadable — without this a custom
        // URLClassLoader (Tomcat's CommonClassLoader, ActiveMQ's launcher)
        // can never find its classes and throws ClassNotFoundException.
        register_url_array(ctx, urls);
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
            ctx.set_field_by_name(this, "ucp", urls);
            init_classloader_common_fields(ctx, this);
            register_url_array(ctx, urls);
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
            // The boolean `resolve` arg (slot 2) is ignored — we always resolve.
            // Drop it so `cl_real_load_class` sees the `(receiver, name)` shape.
            let trimmed: Vec<Value> = args.iter().take(2).copied().collect();
            cl_real_load_class(ctx, &trimmed)
        },
    );
}

/// `ClassLoader.loadClass(String)` for real-JDK mode.
///
/// Delegation order (JVMS §5.3 / `ClassLoader.loadClass` contract):
///   1. standard VM class loading (bootstrap → platform → app),
///   2. if that fails and the receiver is a non-builtin `ClassLoader`
///      subclass overriding `findClass`, dispatch the override virtually,
///   3. otherwise throw `ClassNotFoundException`.
fn cl_real_load_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> rustjvm_types::error::MethodCallResult {
    let class_name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_name = ctx.read_string(class_name_obj).unwrap_or_default();
    let internal = class_name.replace('.', "/");

    // 1. Standard VM class loading.
    if let Ok(Some(mirror)) = ctx.load_class(&internal) {
        return Ok(Some(mirror));
    }

    // 2. Custom-classloader extension point: if the receiver overrides
    //    `findClass`, the JVM `loadClass` contract requires us to call it.
    //    `invoke_virtual` resolves on the receiver's actual class, so this
    //    dispatches to the subclass's overriding `findClass` bytecode.
    if let Some(Value::Object(Some(this))) = args.first() {
        if crate::classloader::receiver_overrides_find_class(ctx, *this) {
            return ctx.invoke_virtual(
                *this,
                "findClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[Value::Object(Some(class_name_obj))],
            );
        }
    }

    // 3. Genuinely not found and no user override — throw CNFE per spec.
    let exc = alloc_concurrent_synthetic(ctx, "java/lang/ClassNotFoundException", 1);
    let msg = ctx.create_string(&class_name);
    ctx.set_field(exc, 0, Value::Object(Some(msg)));
    Err(rustjvm_types::error::MethodCallFailed::ExceptionThrown(exc))
}

/// Get or create the system (app) class loader singleton.
pub fn get_or_create_system_cl(
    ctx: &mut dyn rustjvm_native_api::registry::NativeContext,
) -> Option<ObjectRef> {
    let existing = *SYSTEM_CL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(obj) = existing {
        return Some(obj);
    }
    // Create platform loader first (parent of system loader)
    let platform = get_or_create_platform_cl(ctx);
    // Allocate a ClassLoader object
    let obj_val = match ctx.new_object("java/lang/ClassLoader") {
        Ok(Some(Value::Object(Some(obj)))) => obj,
        _ => return None,
    };
    ctx.set_field_by_name(obj_val, "parent", Value::Object(platform));
    let name = ctx.create_string("app");
    ctx.set_field_by_name(obj_val, "name", Value::Object(Some(name)));
    *SYSTEM_CL.lock().unwrap_or_else(|e| e.into_inner()) = Some(obj_val);
    Some(obj_val)
}

/// Get or create the platform class loader singleton.
fn get_or_create_platform_cl(
    ctx: &mut dyn rustjvm_native_api::registry::NativeContext,
) -> Option<ObjectRef> {
    let existing = *PLATFORM_CL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(obj) = existing {
        return Some(obj);
    }
    let obj_val = match ctx.new_object("java/lang/ClassLoader") {
        Ok(Some(Value::Object(Some(obj)))) => obj,
        _ => return None,
    };
    // Platform loader's parent is null (bootstrap)
    ctx.set_field_by_name(obj_val, "parent", Value::Object(None));
    let name = ctx.create_string("platform");
    ctx.set_field_by_name(obj_val, "name", Value::Object(Some(name)));
    *PLATFORM_CL.lock().unwrap_or_else(|e| e.into_inner()) = Some(obj_val);
    Some(obj_val)
}
