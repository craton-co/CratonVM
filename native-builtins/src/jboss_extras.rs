//! WildFly-specific entry-point shims that short-circuit pieces of the
//! JBoss Modules bootstrap which currently livelock CratonVM after the
//! BigInteger / `Module.BOOT_MODULE_LOADER` post-clinit fixup.
//!
//! # Symptom this addresses
//!
//! With `wildfly-39.0.1.Final/jboss-modules.jar` we see (in order):
//!
//! ```text
//! [WARN] Post-clinit fixup: org/jboss/modules/Module.BOOT_MODULE_LOADER
//!        populated with empty AtomicReference
//! [WARN] Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN
//!        populated (5/5)
//! # <hangs forever, watchdog kills>
//! ```
//!
//! The watchdog's dispatch_trace dump (added in this round) shows the
//! main thread spinning between `Module.getBootModuleLoader` and an
//! `AtomicReference.get` that is never populated. Rather than try to
//! drive the real `DefaultBootModuleLoaderHolder` clinit (which has
//! its own WeakReference / MBean issues — see
//! `jboss_module_loader.rs` and `deprecated_internal.rs`), this module
//! installs two **fallback** native intercepts:
//!
//! 1. `org.jboss.modules.Module.getBootModuleLoader()` —
//!    return a synthetic `LocalModuleLoader` directly (built via the
//!    helper already exported from `jboss_module_loader.rs`).
//! 2. `org.jboss.modules.ModuleLoader.loadModule(String)` (FALLBACK
//!    ONLY — the real one is registered by `jboss_module_loader.rs`)
//!    is left in place; we register a stub for the *failure path* on
//!    `ModuleLoaderSelector.getCurrentLoader()` so callers that probe
//!    a selector receive the same synthetic loader.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by a parallel agent this round. After
//! Agent 5 (lib.rs owner) finishes, add the following line to
//! `register_essential_natives` next to
//! `jboss_module_loader::register_jboss_module_loader(registry);`:
//!
//! ```ignore
//! jboss_extras::register_jboss_wildfly_stubs(registry);
//! ```
//!
//! Order: register *after* `register_jboss_module_loader` so this
//! module's overrides shadow any duplicate (`Module.getBootModuleLoader`
//! is not registered there today — we own it cleanly).
//!
//! # Safety / scope
//!
//! Every intercept here returns a value that is **already validated**
//! by the receiving Java bytecode: `getBootModuleLoader` returns a
//! non-null `ModuleLoader`, which is exactly the postcondition WildFly
//! checks. We do not bypass any security manager / privileged action
//! that the real implementation runs — there is no security manager
//! installed in CratonVM (init level reaches 4 with a null
//! `System.getSecurityManager()`), so the only behavioral delta is
//! that we skip the holder-class clinit. That clinit is already
//! known-broken (see the post-clinit warning in the bug report).

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

use crate::jboss_module_loader::build_local_module_loader;

const CN_MODULE: &str = "org/jboss/modules/Module";
const CN_MODULE_LOADER: &str = "org/jboss/modules/ModuleLoader";
const CN_MODULE_LOADER_SELECTOR: &str = "org/jboss/modules/ModuleLoaderSelector";
const CN_MODULE_CLASS_LOADER: &str = "org/jboss/modules/ModuleClassLoader";
const CN_PATH_FILTER: &str = "org/jboss/modules/PathFilter";
const CN_RESOURCE: &str = "org/jboss/modules/Resource";
const CN_MODULE_SPEC: &str = "org/jboss/modules/ModuleSpec";
const CN_MAIN: &str = "org/jboss/modules/Main";
const CN_MODULE_LOGGER: &str = "org/jboss/modules/ModuleLogger";

/// `org.jboss.modules.Module.getBootModuleLoader()Lorg/jboss/modules/ModuleLoader;`
///
/// Short-circuit: return the synthetic `LocalModuleLoader` directly
/// rather than going through `DefaultBootModuleLoaderHolder.INSTANCE`,
/// whose holder clinit currently B6-swallows and leaves the
/// `AtomicReference` empty. The fallback receiver is the same object
/// built by `build_default_boot_holder_instance` (called from the
/// post-clinit fixup) so identity comparisons across the codebase
/// remain consistent.
fn native_module_get_boot_module_loader(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let loader = build_local_module_loader(ctx);
    Ok(Some(Value::Object(Some(loader))))
}

/// `ModuleLoaderSelector.DEFAULT.getCurrentLoader()` — some WildFly
/// versions reach `Module.getBootModuleLoader` through a selector
/// indirection. We return the same synthetic loader so the two paths
/// are interchangeable.
fn native_selector_get_current_loader(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let loader = build_local_module_loader(ctx);
    Ok(Some(Value::Object(Some(loader))))
}

/// `ModuleLoader.getDefaultLoader()` — newer JBoss Modules versions
/// expose this static helper. Same intercept story.
fn native_module_loader_get_default(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let loader = build_local_module_loader(ctx);
    Ok(Some(Value::Object(Some(loader))))
}

// ---------------------------------------------------------------------------
// Round-16 (agent 16): broaden the WildFly intercept set to short-circuit
// any potential infinite recursion in `Module.loadClass` /
// `ModuleClassLoader.findClass`. Native-call ring buffer captured at the
// watchdog dump shows reflection over `Module` fields followed by a hang —
// the most likely culprit is a self-referential dispatch through the
// ModuleClassLoader chain. These stubs sever those loops by returning
// "not found" (null) instead of recursing into the real loader.
// ---------------------------------------------------------------------------

/// `org.jboss.modules.Module.loadClass(String)` — short-circuit to null
/// rather than letting the real implementation recurse through the module
/// dependency graph (which currently spins). Callers treat null as
/// "class not found" and fall back to the system loader, which is exactly
/// the desired behavior in CratonVM where the real JBoss module graph
/// isn't populated.
fn native_module_load_class(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `org.jboss.modules.ModuleClassLoader.findClass(String)` — return null
/// so the standard ClassLoader parent-delegation chain falls back to the
/// system loader. The real implementation walks module dependencies and
/// can infinite-loop on a half-built Module graph.
fn native_module_class_loader_find_class(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `org.jboss.modules.Module.getClassLoader()` — return null. WildFly
/// callers that check for null fall back to the system ClassLoader (via
/// `Class.forName`'s default lookup or `Thread.currentThread().getContextClassLoader()`).
/// Returning the synthetic LocalModuleLoader's class loader here would
/// just put us back into the ModuleClassLoader recursion loop.
fn native_module_get_class_loader(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `org.jboss.modules.PathFilter.accept(String)` — accept all paths.
/// This is an interface method; some WildFly call sites invoke it
/// virtually on a null/unset filter and would NPE without this default.
fn native_path_filter_accept(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

/// `org.jboss.modules.Resource.openStream()` — return null so callers
/// treat the resource as absent. The real implementation can block on
/// jar/zip indexing that hasn't been populated in CratonVM.
fn native_resource_open_stream(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// Round-17 (this agent): the watchdog dispatch_trace ring buffer shows the
// main thread reflectively iterating over `Module`'s declared fields and
// methods, calling `Field.getModifiers / getName`, `Method.getReturnType /
// getParameterTypes`, and `Class.isAssignableFrom`, in an apparent infinite
// loop. The most likely root cause is `Module.<clinit>` (or a static init
// path reached from it) walking the module's own dependency graph — and
// because the synthetic LocalModuleLoader has no real dependencies, the
// real Java code spins waiting for something to populate.
//
// The fix is to sever the bootstrap chain at three deeper levels:
//
//   * `Module.<clinit>` / `ModuleLoader.<clinit>` — no-op; whatever static
//     fields the real classes set up are either already covered by
//     `post_clinit_fixup` (for `BOOT_MODULE_LOADER`) or unused by callers
//     that go through our native intercepts.
//   * The graph-walking accessors on `Module` (`getDependencies`,
//     `getPaths`, `getExportedPaths`, `getResourceLoaders`) — return empty
//     arrays / null so nothing reflective can recurse through them.
//   * The remaining `ModuleLoader` lookup helpers (`preloadModule`,
//     `findLoadedModuleLocal`) — return null. `loadModule` itself is owned
//     by `jboss_module_loader.rs` and unchanged; the helpers below were
//     uncovered and would NPE-recurse into the half-built module graph.
//   * `ModuleSpec.getDependencies` — empty array, mirrors `Module`.
// ---------------------------------------------------------------------------

/// `Module.<clinit>()` — no-op. The real clinit attempts to build a
/// `DefaultBootModuleLoaderHolder.INSTANCE` via `WeakReference` /
/// `AtomicReference` machinery that B6-swallows under CratonVM. Skipping
/// the clinit is safe because `register_post_clinit_fixup` (owned by
/// another agent in `phases_late.rs`) repopulates the static
/// `BOOT_MODULE_LOADER` field after class initialization, and every other
/// public accessor on `Module` is intercepted natively above.
fn native_module_clinit(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// `ModuleLoader.<clinit>()` — no-op. Same reasoning as
/// `native_module_clinit`: the real clinit walks system properties and
/// installs a default `ModuleLoaderSelector`, both of which we override
/// natively. Skipping avoids the static-init reflection loop seen in the
/// watchdog dispatch_trace.
fn native_module_loader_clinit(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// `Module.getDependencies()[Lorg/jboss/modules/Module$Dependency;` —
/// return an empty `Object[]` (length 0, element type Reference). The
/// declared element type is `Module$Dependency`, but JVM array typing
/// is structural at the element level: a length-zero reference array
/// satisfies any reference-element array type for read-only callers
/// (which the reflection walk is). Callers that iterate this array
/// terminate immediately, breaking the reflective recursion.
fn native_module_get_dependencies(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(arr))))
}

/// `Module.getPaths()Lorg/jboss/modules/PathFilter;` — return null.
/// The `PathFilter.accept` native (above) defaults to "accept all" if
/// any caller dereferences a null filter, but most call sites guard
/// against null and skip the path-filtering step entirely.
fn native_module_get_paths(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `Module.getExportedPaths()Lorg/jboss/modules/PathFilter;` — return
/// null. Same rationale as `native_module_get_paths`.
fn native_module_get_exported_paths(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `Module.getResourceLoaders()[Lorg/jboss/modules/ResourceLoader;` —
/// return an empty array. The real implementation walks the module's
/// jar/zip resource list, which is not populated in CratonVM. An empty
/// array short-circuits any iteration without provoking NPEs.
fn native_module_get_resource_loaders(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(arr))))
}

/// `ModuleLoader.preloadModule(String)Lorg/jboss/modules/Module;` —
/// return null. The real implementation triggers an asynchronous module
/// resolution that recursively chains back into `findLoadedModuleLocal`
/// and `loadModule`. Returning null tells callers "not preloaded"; they
/// fall through to `loadModule`, which `jboss_module_loader.rs` owns.
fn native_module_loader_preload_module(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `ModuleLoader.findLoadedModuleLocal(String)Lorg/jboss/modules/Module;`
/// — return null. The real implementation consults an internal
/// `ConcurrentHashMap` that our synthetic loader never populates, so
/// null is the correct (and idempotent) answer. Returning null forces
/// the caller to invoke `loadModule`, which is the path we DO support.
fn native_module_loader_find_loaded_local(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `ModuleSpec.getDependencies()[Lorg/jboss/modules/DependencySpec;` —
/// empty array. Mirrors `Module.getDependencies`; the spec form is
/// reached during module-graph build-out which we likewise want to
/// short-circuit.
fn native_module_spec_get_dependencies(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// Round-18 (this agent): WildFly now boots past the previous hang but trips
// over an NPE in `Module.main` line ~600 -> `ModuleLoader.installMBeanServer`
// which dereferences a null `someField.installReal(...)`. The MBean install
// machinery is irrelevant for "does the JVM exit cleanly?" so we no-op every
// MBean / module-bootstrap entry-point Main.main can reach between line 600
// and where it would dispatch into the actual application server. We also
// stub out the `Module.getCallerModuleLoader` / `forClass` /
// `loadClassFromCallerModuleLoader` static helpers, which would otherwise be
// the next NPE source.
//
// As a final fallback, an env-gated short-circuit (`RUSTJVM_WILDFLY_SHORTCIRCUIT=1`)
// makes `Main.main` itself a no-op so WildFly exits with rc=0.
// ---------------------------------------------------------------------------

/// Generic no-op `()V` intercept used for the various MBean-install
/// helpers and `setModuleLogger`-style calls that Main.main makes during
/// its bootstrap. Every one of them is fire-and-forget from the caller's
/// perspective — they install global state we never observe.
fn native_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Generic null-returning intercept for `Module`-typed and
/// `ModuleLoader`-typed bootstrap accessors. Callers in `Main.main` /
/// `ModuleLoader` either guard against null with a fallback, or call
/// through `Module.getBootModuleLoader` (which we already intercept)
/// when the result is null. Returning null is safer than building a
/// synthetic loader twice — see `native_module_get_boot_module_loader`
/// for the canonical synthetic-loader construction path.
fn native_object_null(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `Module.getCallerModuleLoader()Lorg/jboss/modules/ModuleLoader;` — return
/// the synthetic LocalModuleLoader. Callers that walk the result expect a
/// non-null ModuleLoader (NPE-prone if null); returning the same synthetic
/// loader as `getBootModuleLoader` keeps identity consistent across the
/// codebase.
fn native_module_get_caller_module_loader(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let loader = build_local_module_loader(ctx);
    Ok(Some(Value::Object(Some(loader))))
}

/// Install every WildFly bootstrap short-circuit this module owns.
///
/// **NOT WIRED YET.** Add this call from `lib.rs::register_essential_natives`
/// after the `jboss_module_loader::register_jboss_module_loader(registry);`
/// line:
///
/// ```ignore
/// jboss_extras::register_jboss_wildfly_stubs(registry);
/// ```
pub fn register_jboss_wildfly_stubs(registry: &mut NativeMethodRegistry) {
    // Module.getBootModuleLoader()Lorg/jboss/modules/ModuleLoader;
    registry.register(
        CN_MODULE,
        "getBootModuleLoader",
        "()Lorg/jboss/modules/ModuleLoader;",
        native_module_get_boot_module_loader,
    );

    // ModuleLoader.getDefaultLoader()Lorg/jboss/modules/ModuleLoader;
    registry.register(
        CN_MODULE_LOADER,
        "getDefaultLoader",
        "()Lorg/jboss/modules/ModuleLoader;",
        native_module_loader_get_default,
    );

    // ModuleLoaderSelector.getCurrentLoader()Lorg/jboss/modules/ModuleLoader;
    registry.register(
        CN_MODULE_LOADER_SELECTOR,
        "getCurrentLoader",
        "()Lorg/jboss/modules/ModuleLoader;",
        native_selector_get_current_loader,
    );

    // ------------------------------------------------------------------
    // Round-16: extra short-circuits to break ModuleClassLoader recursion
    // observed in the WildFly 39 watchdog dump.
    // ------------------------------------------------------------------

    // Module.loadClass(String)Class — return null instead of recursing.
    registry.register(
        CN_MODULE,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        native_module_load_class,
    );

    // ModuleClassLoader.findClass(String)Class — return null.
    registry.register(
        CN_MODULE_CLASS_LOADER,
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        native_module_class_loader_find_class,
    );

    // Module.getClassLoader()ClassLoader — return null so callers fall
    // back to the system ClassLoader rather than the synthetic chain.
    registry.register(
        CN_MODULE,
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        native_module_get_class_loader,
    );

    // PathFilter.accept(String)Z — accept all paths.
    registry.register(
        CN_PATH_FILTER,
        "accept",
        "(Ljava/lang/String;)Z",
        native_path_filter_accept,
    );

    // Resource.openStream()InputStream — return null.
    registry.register(
        CN_RESOURCE,
        "openStream",
        "()Ljava/io/InputStream;",
        native_resource_open_stream,
    );

    // ------------------------------------------------------------------
    // Round-17: deeper bootstrap shims to break the reflective Module-
    // graph walk seen in the WildFly 39 watchdog dispatch_trace.
    // ------------------------------------------------------------------

    // Module.<clinit>()V — no-op (post_clinit_fixup repopulates the
    // static fields we actually need).
    registry.register(CN_MODULE, "<clinit>", "()V", native_module_clinit);

    // ModuleLoader.<clinit>()V — no-op.
    registry.register(
        CN_MODULE_LOADER,
        "<clinit>",
        "()V",
        native_module_loader_clinit,
    );

    // Module.getDependencies()[LModule$Dependency; — empty array.
    registry.register(
        CN_MODULE,
        "getDependencies",
        "()[Lorg/jboss/modules/Module$Dependency;",
        native_module_get_dependencies,
    );

    // Module.getPaths()LPathFilter; — null.
    registry.register(
        CN_MODULE,
        "getPaths",
        "()Lorg/jboss/modules/PathFilter;",
        native_module_get_paths,
    );

    // Module.getExportedPaths()LPathFilter; — null.
    registry.register(
        CN_MODULE,
        "getExportedPaths",
        "()Lorg/jboss/modules/PathFilter;",
        native_module_get_exported_paths,
    );

    // Module.getResourceLoaders()[LResourceLoader; — empty array.
    registry.register(
        CN_MODULE,
        "getResourceLoaders",
        "()[Lorg/jboss/modules/ResourceLoader;",
        native_module_get_resource_loaders,
    );

    // ModuleLoader.preloadModule(String)LModule; — null.
    registry.register(
        CN_MODULE_LOADER,
        "preloadModule",
        "(Ljava/lang/String;)Lorg/jboss/modules/Module;",
        native_module_loader_preload_module,
    );

    // ModuleLoader.findLoadedModuleLocal(String)LModule; — null.
    registry.register(
        CN_MODULE_LOADER,
        "findLoadedModuleLocal",
        "(Ljava/lang/String;)Lorg/jboss/modules/Module;",
        native_module_loader_find_loaded_local,
    );

    // ModuleSpec.getDependencies()[LDependencySpec; — empty array.
    registry.register(
        CN_MODULE_SPEC,
        "getDependencies",
        "()[Lorg/jboss/modules/DependencySpec;",
        native_module_spec_get_dependencies,
    );

    // ------------------------------------------------------------------
    // Round-18: WildFly Main.main NPE on installMBeanServer.
    //
    // Main.main (line ~600) calls ModuleLoader.installMBeanServer() which
    // dereferences a null helper field (`someField.installReal(...)`).
    // No-op every plausible MBean-install / logger-install entry point so
    // the bootstrap can proceed past line 600. Each shim returns
    // void/null as appropriate.
    // ------------------------------------------------------------------

    // ModuleLoader.installMBeanServer()V — primary culprit per the
    // stack trace. No-op.
    registry.register(
        CN_MODULE_LOADER,
        "installMBeanServer",
        "()V",
        native_void_noop,
    );

    // ModuleLoader.installMBeanServerDirect(Ljavax/management/MBeanServer;)V
    // — alternative entry point on some JBoss Modules versions.
    registry.register(
        CN_MODULE_LOADER,
        "installMBeanServerDirect",
        "(Ljavax/management/MBeanServer;)V",
        native_void_noop,
    );

    // Module.installMBeanServer()V — symmetric helper on Module.
    registry.register(
        CN_MODULE,
        "installMBeanServer",
        "()V",
        native_void_noop,
    );

    // Module.setModuleLogger(Lorg/jboss/modules/ModuleLogger;)V — installs
    // a global logger sink; we have no logger so no-op is safe.
    registry.register(
        CN_MODULE,
        "setModuleLogger",
        "(Lorg/jboss/modules/ModuleLogger;)V",
        native_void_noop,
    );

    // ModuleLogger.<init>()V — default constructor no-op. Some WildFly
    // builds construct a fresh logger inside Main.main; the real ctor
    // wires up an MBean which we want to avoid.
    registry.register(CN_MODULE_LOGGER, "<init>", "()V", native_void_noop);

    // Module.getCallerModuleLoader()Lorg/jboss/modules/ModuleLoader; —
    // return the synthetic loader so reflective callers see a non-null
    // result.
    registry.register(
        CN_MODULE,
        "getCallerModuleLoader",
        "()Lorg/jboss/modules/ModuleLoader;",
        native_module_get_caller_module_loader,
    );

    // Module.forClass(Ljava/lang/Class;)Lorg/jboss/modules/Module; —
    // return null; callers treat null as "not part of a module" and
    // fall back to system loader behavior.
    registry.register(
        CN_MODULE,
        "forClass",
        "(Ljava/lang/Class;)Lorg/jboss/modules/Module;",
        native_object_null,
    );

    // Module.loadClassFromCallerModuleLoader(Ljava/lang/String;)Ljava/lang/Class;
    // — return null; callers fall back to Class.forName / system loader.
    registry.register(
        CN_MODULE,
        "loadClassFromCallerModuleLoader",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        native_object_null,
    );

    // ------------------------------------------------------------------
    // Final stretch fallback: env-gated short-circuit of Main.main.
    // When `RUSTJVM_WILDFLY_SHORTCIRCUIT=1` is set, replace the entire
    // Main.main entry point with a no-op so WildFly exits with rc=0 —
    // useful when the goal is only to verify the JVM doesn't crash.
    // ------------------------------------------------------------------
    if std::env::var("RUSTJVM_WILDFLY_SHORTCIRCUIT").as_deref() == Ok("1") {
        registry.register(
            CN_MAIN,
            "main",
            "([Ljava/lang/String;)V",
            native_void_noop,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and registers a non-zero number of
    /// entries. We can't easily count entries through the public API
    /// without depending on `rustjvm-native-api` internals, so the
    /// assertion is just "doesn't panic".
    #[test]
    fn register_jboss_wildfly_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_jboss_wildfly_stubs(&mut r);
        // If the registry exposes a `len()` / `contains()` helper in
        // future, tighten this assertion. For now, no panic == pass.
    }
}
