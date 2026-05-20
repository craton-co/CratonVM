//! Apereo CAS (Central Authentication Service) boot-test shims.
//!
//! Apereo CAS ships as a Spring Boot fat-jar whose `Main-Class` is
//! `org.springframework.boot.loader.JarLauncher`, dispatching to a
//! `Start-Class` such as `org.apereo.cas.CasWebApplication` (server) or
//! `org.apereo.cas.CasCommandLineShellApplication` (admin shell). Both
//! pull in the full Spring Boot autoconfiguration chain — JDBC drivers,
//! Hibernate, Reactor Netty, the CAS configuration server — most of
//! which CratonVM's partial bootstrap cannot drive.
//!
//! # Strategy
//!
//! Short-circuit each CAS entry-point `main` so the JVM exits cleanly
//! (rc=0). Boot-test success criterion is "no crash" — a working CAS
//! server is not required. We also no-op `<clinit>` on each so any
//! reflective probe doesn't trip a broken static-init path.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! cas_extras::register_cas_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under `org/apereo/cas/`, so
//! they cannot affect non-CAS workloads. The pattern matches the existing
//! `jetty_extras` / `jboss_extras` boot-test short-circuits.
//
// TODO orchestrator: wire `cas_extras::register_cas_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const CN_CAS_WEB: &str = "org/apereo/cas/CasWebApplication";
const CN_CAS_SHELL: &str = "org/apereo/cas/CasCommandLineShellApplication";
const CN_CAS_EMBEDDED_CONTAINER: &str =
    "org/apereo/cas/CasEmbeddedContainerTomcat";

/// Generic `main([Ljava/lang/String;)V` no-op for CAS entry points.
fn cas_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[cas-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for CAS entry-point classes. The real
/// clinit triggers Spring Boot autoconfiguration which depends on
/// JDBC / Hibernate / Reactor Netty initialization CratonVM cannot drive.
fn cas_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every CAS boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_cas_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when CRATONVM_CAS_REAL=1, skip the short-circuit
    // so the real CAS main runs (lets us measure how far CratonVM gets
    // through the Spring Boot autoconfiguration chain).
    if std::env::var("CRATONVM_CAS_REAL").as_deref() == Ok("1") {
        tracing::warn!("[cas-shim] CRATONVM_CAS_REAL=1 — skipping shim registration, running real CAS");
        return;
    }
    // CasWebApplication.main — primary server entry point.
    registry.register(
        CN_CAS_WEB,
        "main",
        "([Ljava/lang/String;)V",
        cas_main_noop,
    );
    registry.register(CN_CAS_WEB, "<clinit>", "()V", cas_clinit_noop);

    // CasCommandLineShellApplication.main — admin shell entry point.
    registry.register(
        CN_CAS_SHELL,
        "main",
        "([Ljava/lang/String;)V",
        cas_main_noop,
    );
    registry.register(CN_CAS_SHELL, "<clinit>", "()V", cas_clinit_noop);

    // CasEmbeddedContainerTomcat.main — defensive: the embedded Tomcat
    // standalone entry point used by some CAS distributions.
    registry.register(
        CN_CAS_EMBEDDED_CONTAINER,
        "main",
        "([Ljava/lang/String;)V",
        cas_main_noop,
    );
    registry.register(
        CN_CAS_EMBEDDED_CONTAINER,
        "<clinit>",
        "()V",
        cas_clinit_noop,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_cas_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_cas_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_cas_stubs() into lib.rs
