//! ClassLoader natives for real-JDK mode.
//!
//! The real JDK `ClassLoader.<init>` is extremely complex — it creates
//! ArrayList, ProtectionDomain, NativeLibraries, Module, ConcurrentHashMap,
//! and more. Many of these depend on JDK internals we don't fully support.
//!
//! These simplified natives use `set_field_by_name`/`get_field_by_name` so
//! they work with the real JDK field layout (not hardcoded synthetic indices).

use std::sync::Mutex;

use cratonvm_native_api::registry::NativeMethodRegistry;
use cratonvm_types::{ObjectRef, Value};

use crate::alloc_concurrent_synthetic;

/// Cached system/platform classloader objects (created lazily).
static SYSTEM_CL: Mutex<Option<ObjectRef>> = Mutex::new(None);
static PLATFORM_CL: Mutex<Option<ObjectRef>> = Mutex::new(None);

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

    // ClassLoader.loadClass(String) — delegate to VM class loading
    r.register(
        cl,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| {
            let class_name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let class_name = ctx.read_string(class_name_obj).unwrap_or_default();
            let internal = class_name.replace('.', "/");
            match ctx.load_class(&internal) {
                Ok(Some(mirror)) => {
                    Ok(Some(mirror))
                }
                Ok(None) | Err(_) => {
                    // JDK spec: ClassLoader.loadClass throws ClassNotFoundException
                    let exc = alloc_concurrent_synthetic(ctx, "java/lang/ClassNotFoundException", 1);
                    let msg = ctx.create_string(&class_name);
                    ctx.set_field(exc, 0, Value::Object(Some(msg)));
                    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc))
                }
            }
        },
    );
}

/// Get or create the system (app) class loader singleton.
pub fn get_or_create_system_cl(
    ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
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
    ctx: &mut dyn cratonvm_native_api::registry::NativeContext,
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
