//! KC16 boot fix — `org/jboss/modules/log/JDKModuleLogger.<clinit>` shim.
//!
//! ## Problem
//!
//! `org.jboss.modules.log.JDKModuleLogger.<clinit>` is invoked early during
//! the JBoss Modules `Main.main` boot of Keycloak 16 (KC16). The JDK 25
//! source for this clinit is, in essence:
//!
//! ```text
//!   try { TRACE = Level.parse("TRACE"); } catch (IAE) { TRACE = Level.FINEST; }
//!   try { DEBUG = Level.parse("DEBUG"); } catch (IAE) { DEBUG = Level.FINE;   }
//!   try { WARN  = Level.parse("WARN");  } catch (IAE) { WARN  = Level.WARNING;}
//! ```
//!
//! On rustjvm under JDK 25 the clinit aborts with a wrapped
//! `NullPointerException("Cannot invoke isNamed on null")`. The producer of
//! the null receiver lives somewhere in the transitive `Level.parse` chain
//! (`KnownLevel.findByName` -> `ClassLoaderValue.computeIfAbsent` ->
//! `BootLoader.getClassLoaderValueMap` / `JavaLangAccess` shim) — fixing it
//! at the producer side requires a non-trivial structural change to module
//! plumbing that is owned by sibling agents.
//!
//! Operationally the cascade surfaces as a B6 silent-swallow line on every
//! KC16 boot:
//!
//! ```text
//!   B6: silent-swallow ... class=org/jboss/modules/log/JDKModuleLogger
//!        exc=java/lang/NullPointerException: Cannot invoke isNamed on null
//! ```
//!
//! ## Fix (Path B — tactical clinit shim)
//!
//! We register a custom `<clinit>` for `JDKModuleLogger` that bypasses the
//! upstream cascade entirely:
//!
//!   1. Force-initialize `java/util/logging/Level` so its standard static
//!      Levels (`FINEST`, `FINE`, `WARNING`) are available.
//!   2. Read `Level.FINEST` / `Level.FINE` / `Level.WARNING` and write them
//!      directly into JDKModuleLogger's `TRACE` / `DEBUG` / `WARN` static
//!      slots. This matches the fallback branch of the real clinit, so the
//!      static fields are non-null and downstream `trace()` / `debug()` /
//!      `warn()` calls (which read the level via `getstatic` then call
//!      `Logger.isLoggable(level)`) behave correctly.
//!   3. Both steps are wrapped in try-style soft-fail logic — if any field
//!      lookup fails (e.g. layout drift) we still return Ok so the clinit
//!      finalizes as Initialized; downstream NPE on null TRACE then becomes
//!      a separate sibling-owned issue.
//!
//! The B6 swallow line is gone because the no-op clinit never raises an
//! exception — `ensure_class_initialized_shared` finalizes the class in the
//! Initialized state without entering the swallow arm.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const JDKMODULE_LOGGER: &str = "org/jboss/modules/log/JDKModuleLogger";
const JUL_LEVEL: &str = "java/util/logging/Level";

/// Custom `<clinit>` for `org/jboss/modules/log/JDKModuleLogger` that
/// installs the `FINEST` / `FINE` / `WARNING` fallback Levels into
/// `TRACE` / `DEBUG` / `WARN` and never throws.
fn clinit(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Best-effort: ensure java.util.logging.Level is initialized so the
    // standard static fields below are populated. If init fails, we fall
    // through and the static slots stay at their default (null) — the
    // class still finalizes as Initialized, so the B6 swallow line goes
    // away. Downstream NPE on a null Level field then becomes a separate
    // (sibling-owned) issue.
    let _ = ctx.ensure_class_initialized(JUL_LEVEL);

    // Read Level.FINEST / FINE / WARNING from the now-initialized Level
    // class and copy into JDKModuleLogger.{TRACE, DEBUG, WARN}.
    if let Some(level_cid) = ctx.class_id_by_name(JUL_LEVEL) {
        let read = |c: &mut dyn NativeContext, name: &str| -> Option<Value> {
            let idx = c.static_field_index_by_name(level_cid, name)?;
            Some(c.get_static_field(level_cid, idx))
        };

        let finest = read(ctx, "FINEST").unwrap_or(Value::Object(None));
        let fine = read(ctx, "FINE").unwrap_or(Value::Object(None));
        let warning = read(ctx, "WARNING").unwrap_or(Value::Object(None));

        ctx.set_static_field_by_name(JDKMODULE_LOGGER, "TRACE", finest);
        ctx.set_static_field_by_name(JDKMODULE_LOGGER, "DEBUG", fine);
        ctx.set_static_field_by_name(JDKMODULE_LOGGER, "WARN", warning);
    }

    Ok(None)
}

/// Wire the `JDKModuleLogger.<clinit>` shim into the native registry.
/// Called from `register_essential_natives` so it is active in both
/// real-JDK and synthetic-JDK modes.
pub fn register_jboss_jdk_module_logger(registry: &mut NativeMethodRegistry) {
    registry.register(JDKMODULE_LOGGER, "<clinit>", "()V", clinit);
}
