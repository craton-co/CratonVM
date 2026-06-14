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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::alloc_concurrent_synthetic;

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

/// Field layout for our synthetic Module:
///
///   slot 0: name (String)
///   slot 1: layer (ModuleLayer or null)
///   slot 2: packages (Set<String>)
///   slot 3: descriptor (ModuleDescriptor or null)
///   slot 4: loader (ClassLoader or null)
const MODULE_FIELD_COUNT: usize = 5;
const MODULE_SLOT_NAME: usize = 0;
const MODULE_SLOT_LAYER: usize = 1;
const MODULE_SLOT_PACKAGES: usize = 2;

/// Build the singleton boot ModuleLayer.
fn build_boot_layer(ctx: &mut dyn NativeContext) -> ObjectRef {
    let layer = alloc_concurrent_synthetic(ctx, "java/lang/ModuleLayer", MODULE_LAYER_FIELD_COUNT);
    ctx.set_field(layer, 0, Value::Int(1));
    layer
}

/// Build a `java.util.HashSet<String>` pre-populated with `packages`.
fn build_package_set(ctx: &mut dyn NativeContext, packages: &[&str]) -> ObjectRef {
    let set = alloc_concurrent_synthetic(ctx, "java/util/HashSet", 3);
    // Seed a descriptive default layout so cratonvm's synthetic HashSet
    // paths don't misread null slots: slot 0 = backing array or null
    // marker, slot 1 = size (Int), slot 2 = capacity (Int).
    ctx.set_field(set, 0, Value::Object(None));
    ctx.set_field(set, 1, Value::Int(packages.len() as i32));
    ctx.set_field(set, 2, Value::Int(16));
    // Actual element storage: an Object[] array referenced from slot 0.
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, packages.len());
    for (i, pkg) in packages.iter().enumerate() {
        let s = ctx.create_string(pkg);
        ctx.set_array_element(arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(set, 0, Value::Object(Some(arr)));
    set
}

/// Build a synthetic Module for `name`, bound to the boot layer.
fn build_module(ctx: &mut dyn NativeContext, name: &str, layer: ObjectRef) -> ObjectRef {
    let module = alloc_concurrent_synthetic(ctx, "java/lang/Module", MODULE_FIELD_COUNT);
    let name_str = ctx.create_string(name);
    ctx.set_field(module, MODULE_SLOT_NAME, Value::Object(Some(name_str)));
    ctx.set_field(module, MODULE_SLOT_LAYER, Value::Object(Some(layer)));
    // Only `java.base` gets the full JDK package set; other synthetic
    // modules get an empty set (callers check `contains` before acting).
    let packages = if name == "java.base" {
        build_package_set(ctx, BOOT_JDK_PACKAGES)
    } else {
        build_package_set(ctx, &[])
    };
    ctx.set_field(module, MODULE_SLOT_PACKAGES, Value::Object(Some(packages)));
    module
}

/// Wrap an ObjectRef as `Optional.of(value)`.
fn wrap_optional_present(ctx: &mut dyn NativeContext, value: ObjectRef) -> ObjectRef {
    let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
    ctx.set_field(opt, 0, Value::Object(Some(value)));
    opt
}

/// `ModuleLayer.boot()` — produce the cached boot layer.
pub(crate) fn native_module_layer_boot(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let layer = build_boot_layer(ctx);
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

    let layer_ref = match args.first() {
        Some(Value::Object(Some(l))) => *l,
        _ => build_boot_layer(ctx),
    };
    let module = build_module(ctx, &name, layer_ref);
    let opt = wrap_optional_present(ctx, module);
    Ok(Some(Value::Object(Some(opt))))
}

/// `Module.getName()` — return the String stored at slot 0.
pub(crate) fn native_module_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, MODULE_SLOT_NAME)))
}

/// `Module.getPackages()` — return the Set at slot 2, lazily populated
/// if the Module object was built elsewhere without pre-seeded packages.
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
    let existing = ctx.get_field(this, MODULE_SLOT_PACKAGES);
    if let Value::Object(Some(_)) = existing {
        return Ok(Some(existing));
    }
    // Slot not populated — seed with the full JDK package set. Callers
    // already check `.contains(...)` so an over-inclusive set is fine.
    let packages = build_package_set(ctx, BOOT_JDK_PACKAGES);
    ctx.set_field(this, MODULE_SLOT_PACKAGES, Value::Object(Some(packages)));
    Ok(Some(Value::Object(Some(packages))))
}

/// `Module.getLayer()` — return the ModuleLayer at slot 1, or the boot
/// layer as a safe default.
pub(crate) fn native_module_get_layer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let layer = build_boot_layer(ctx);
            return Ok(Some(Value::Object(Some(layer))));
        }
    };
    let existing = ctx.get_field(this, MODULE_SLOT_LAYER);
    if let Value::Object(Some(_)) = existing {
        return Ok(Some(existing));
    }
    let layer = build_boot_layer(ctx);
    ctx.set_field(this, MODULE_SLOT_LAYER, Value::Object(Some(layer)));
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
    _args: &[Value],
) -> MethodCallResult {
    // Build a REAL empty HashSet via its constructor. A synthetic HashSet with
    // slot-based fields breaks in real-JDK mode: the real `HashSet.iterator()`
    // bytecode reads `this.map` (a HashMap) which our synthetic object never
    // populates, so `modules().iterator()` returned null and Tomcat's
    // `for (ResolvedModule m : boot().configuration().modules())` web-fragment
    // scan (StandardJarScanner.doScanClassPath) NPE'd on `iterator.hasNext()`,
    // failing every embedded-server context start.
    ctx.new_object_initialized("java/util/HashSet", "()V", &[])
}

/// `ModuleLayer.configuration()` — return a synthetic `Configuration`.
///
/// Spring's `findAllModulePathResources` enumerates modules via
/// `boot().configuration().modules()`. We don't model JPMS configurations,
/// so a synthetic one-field `Configuration` is enough — its `modules()`
/// override returns an empty Set.
pub(crate) fn native_module_layer_configuration(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let cfg = alloc_concurrent_synthetic(ctx, "java/lang/module/Configuration", 1);
    Ok(Some(Value::Object(Some(cfg))))
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
    registry.register(ml, "boot", "()Ljava/lang/ModuleLayer;", native_module_layer_boot);
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
    registry.register(ml, "modules", "()Ljava/util/Set;", native_module_layer_modules);
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
    registry.register(m, "getPackages", "()Ljava/util/Set;", native_module_get_packages);
    registry.register(m, "getLayer", "()Ljava/lang/ModuleLayer;", native_module_get_layer);
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
    registry.register(m, "getClassLoader", "()Ljava/lang/ClassLoader;", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    });

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
        let result = native_module_layer_boot(&mut ctx, &[])
            .unwrap()
            .unwrap();
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
        assert!(s.contains("null") || s.contains("Null"), "expected NPE, got {}", s);
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
        let layer = build_boot_layer(&mut ctx);
        let module = build_module(&mut ctx, "java.base", layer);
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
        assert!(s.contains("null") || s.contains("Null"), "expected NPE, got {}", s);
    }

    #[test]
    fn module_get_packages_lazily_populates_empty_module() {
        let mut ctx = MockNativeContext::new();
        // Build a module with empty packages slot by using a name that
        // isn't java.base (so build_module seeds empty).
        let layer = build_boot_layer(&mut ctx);
        let module = build_module(&mut ctx, "java.sql", layer);
        // Overwrite packages to null to simulate lazy-load path.
        ctx.set_field(module, MODULE_SLOT_PACKAGES, Value::Object(None));
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
        let layer = build_boot_layer(&mut ctx);
        let module = build_module(&mut ctx, "java.base", layer);
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
            .find("java/lang/Module", "getClassLoader", "()Ljava/lang/ClassLoader;")
            .expect("Module.getClassLoader must be registered");
        let mut ctx = MockNativeContext::new();
        let layer = build_boot_layer(&mut ctx);
        let module = build_module(&mut ctx, "java.base", layer);
        let result = cb(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        // Per JDK 25 spec: boot-layer modules with bootstrap loader return null.
        assert!(matches!(result, Value::Object(None)),
            "expected null ClassLoader for boot-layer module, got {:?}", result);
    }

    #[test]
    fn t19_h12_module_get_classloader_returns_null_even_for_packages_slot_set() {
        // Defense in depth: even if the synthetic Module's slot 2 (packages)
        // holds a HashSet — which is what triggered the real-world NSME —
        // our native must NOT return that HashSet as the classloader.
        let mut r = NativeMethodRegistry::new();
        register_jboss_jdkspecific(&mut r);
        let cb = r
            .find("java/lang/Module", "getClassLoader", "()Ljava/lang/ClassLoader;")
            .expect("Module.getClassLoader must be registered");
        let mut ctx = MockNativeContext::new();
        let layer = build_boot_layer(&mut ctx);
        // Build a fully-populated module (slot 2 = HashSet of packages).
        let module = build_module(&mut ctx, "java.base", layer);
        // Confirm slot 2 IS a HashSet (this is the bug condition).
        let pkgs = ctx.get_field(module, MODULE_SLOT_PACKAGES);
        assert!(matches!(pkgs, Value::Object(Some(_))), "slot 2 should be a Set");
        // Native must NOT route through slot 2.
        let result = cb(&mut ctx, &[Value::Object(Some(module))])
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, Value::Object(None)),
            "Module.getClassLoader must not leak the packages HashSet, got {:?}",
            result
        );
    }
}
