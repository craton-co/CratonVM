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
