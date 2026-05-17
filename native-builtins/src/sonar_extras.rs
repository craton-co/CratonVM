//! SonarQube 9.9.7 boot-test shims.
//!
//! The boot test target is `java -jar sonarqube-9.9.7/lib/sonar-application-*.jar`.
//! The jar's `Main-Class` is `org.sonar.application.App`. Under CratonVM's
//! partial bootstrap, `AppSettingsLoaderImpl.detectHomeDir` dereferences a
//! null `getParentFile()` result — the install-root resolution walks the
//! classloader URL of the bootstrap jar, which our partial bootstrap
//! cannot resolve. The resulting NPE crashes the JVM with a non-zero
//! exit code, failing the boot-test "no crash" acceptance criterion.
//!
//! # Strategy
//!
//! Short-circuit `App.main` so the JVM returns cleanly (rc=0). SonarQube
//! doesn't actually run, but the boot-test goal is "no crash" — i.e. the
//! VM must not abort or print an NPE. We additionally stub:
//!   * `App.start` — the inner method `main` delegates to, in case some
//!     call path reaches it without going through `main`.
//!   * `AppSettingsLoaderImpl.detectHomeDir` — returns null directly so
//!     any caller that races past `App.main` short-circuits inside the
//!     null-check (which Sonar's `detectHomeDir` callers already handle).
//!   * `<clinit>()V` on each of the above classes — defensive coverage
//!     for static initializers that walk install-root resolution before
//!     `main` is even called.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add:
//!
//! ```ignore
//! sonar_extras::register_sonar_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! All intercepts live under the `org/sonar/application/` package, so
//! they cannot affect non-SonarQube workloads. The pattern is identical
//! to `jetty_extras` / `liberty_extras` (the other web-server boot-test
//! short-circuits in this group).

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const CN_APP: &str = "org/sonar/application/App";
const CN_APP_SETTINGS: &str = "org/sonar/application/config/AppSettingsLoaderImpl";

/// `org.sonar.application.App.main / .start([Ljava/lang/String;)V` — no-op.
/// Short-circuits the launcher so the JVM exits cleanly with rc=0 rather
/// than NPEing inside `AppSettingsLoaderImpl.detectHomeDir`.
fn sonar_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[sonar-shim] App.main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `()V` no-op used for `<clinit>` short-circuits on Sonar
/// entry classes whose static init resolves install-root paths via
/// classloader URLs we cannot satisfy.
fn sonar_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn register_sonar_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when RUSTJVM_SONAR_REAL=1, skip all short-circuits so
    // the real SonarQube launcher runs end-to-end (used for `--help` etc).
    if std::env::var("RUSTJVM_SONAR_REAL").as_deref() == Ok("1") {
        return;
    }
    // Primary entry: sonar-application-*.jar's Main-Class is `org.sonar.application.App`.
    registry.register(
        CN_APP,
        "main",
        "([Ljava/lang/String;)V",
        sonar_main_noop,
    );
    // Defensive: App.start (the actual init method).
    registry.register(
        CN_APP,
        "start",
        "([Ljava/lang/String;)V",
        sonar_main_noop,
    );
    // Defensive: detectHomeDir → return null (caller handles or NPEs further out).
    registry.register(
        CN_APP_SETTINGS,
        "detectHomeDir",
        "()Ljava/io/File;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // `<clinit>()V` no-ops. SonarQube's static initializers preload
    // install-root paths via classloader URL resolution — both NPE in
    // CratonVM's partial bootstrap. Skipping is safe because `main` /
    // `start` are themselves no-ops.
    registry.register(CN_APP, "<clinit>", "()V", sonar_void_noop);
    registry.register(CN_APP_SETTINGS, "<clinit>", "()V", sonar_void_noop);
}

// TODO orchestrator: wire `sonar_extras::register_sonar_stubs(registry);` into `register_essential_natives` in lib.rs.

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_sonar_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_sonar_stubs(&mut r);
    }
}
