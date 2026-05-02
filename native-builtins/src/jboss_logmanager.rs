//! Block 2C / Path A — synthetic `org.jboss.logmanager.LogManager` shim.
//!
//! ## What this fixes
//!
//! KC16 boot under real-JDK mode prints
//!
//! ```text
//! WARNING: Failed to load the specified log manager class
//!          org.jboss.logmanager.LogManager
//! ```
//!
//! (or in JDK 25, `Could not load Logmanager "org.jboss.logmanager.LogManager"`)
//! and `standalone/log/server.log` is never written.
//!
//! ## Root cause
//!
//! `java.util.logging.LogManager.initLogManager()` runs during JDK
//! `<clinit>` and does (paraphrased):
//!
//! ```java
//! Class<?> c = ClassLoader.getSystemClassLoader().loadClass(prop);
//! Object o = c.newInstance();             // <-- fails here
//! manager = (LogManager) o;
//! ```
//!
//! `loadClass(...)` already succeeds in our VM — `class_manager.rs::is_jdk_class`
//! treats `org/jboss/...` as auto-stub-eligible and `synthetic_stub_fields`
//! gives the LogManager class four pre-allocated slots
//! (`properties`, `loggerRegistry`, `rootLogger`, `ready`). The break is at
//! `Class.newInstance()`: the JDK bytecode of `Class.newInstance` walks
//! through `getReflectionFactory().getConstructor0(...)`, then
//! `ReflectionFactory.copyConstructor`, then
//! `ReflectionFactory.newInstance`. A synthetic stub has zero `methods`,
//! so `getConstructor0` raises `NoSuchMethodException` →
//! `InstantiationException`, which is caught by the `Exception` handler
//! in `initLogManager` and produces the visible WARNING.
//!
//! `native-builtins::lang_class::native_class_new_instance` already
//! sidesteps reflection by calling `ctx.invoke(name, "<init>", "()V", …)`
//! directly. That function is registered in `register_synthetic_overrides`
//! today but NOT in `register_essential_natives` — so real-JDK mode
//! (which is what KC16 boot uses) never gets it. Re-registering it in
//! `register_essential_natives` from this module fixes the warning AND
//! makes `getLogManager()` return the singleton our `logmanager.rs`
//! natives produced.
//!
//! ## Bootstrap log file
//!
//! `org.jboss.boot.log.file` is a system property WildFly's bootstrap
//! sets to a stable path so early errors are recoverable from disk even
//! when the configured handler chain isn't online yet. We expose
//! [`emit_boot_log`] for any native logger surface (the `org.jboss.logmanager.Logger.*`
//! natives in `wildfly_core.rs`) to route a `LEVEL [name] message` line:
//!
//! * If `org.jboss.boot.log.file` is set to a non-empty path, append
//!   the line to that file (best-effort — IO errors are silently
//!   dropped so the boot path is never derailed by a non-writeable
//!   log location).
//! * Otherwise write to stderr.
//!
//! No emojis, no leading whitespace munging — the format is meant to
//! match `LEVEL [logger.name] message\n` so existing log-tail tooling
//! continues to work.
//!
//! ## Why a synthetic shim and not a real JAR
//!
//! Path B (auto-discover the real `jboss-logmanager-*.jar` and put it
//! on the bootstrap classpath) is structurally cleaner but blocked by a
//! deeper classloader-ordering issue: `Class.forName` from inside
//! `java.util.logging.LogManager.<clinit>` runs before any
//! later-discovered JAR is visible to the system loader. The synthetic
//! stub is reachable via the bootstrap loader path, so it's the
//! immediately-shippable fix.
//!
//! ## Constraints honoured
//!
//! * No `eprintln!` markers that would trip `scripts/check-no-diag-prints.sh`
//!   (which only flags `[DIAG-*]`/`[TRACE-*]`/`[SB-TRACE]` prefixes).
//! * Does NOT shadow a real `org.jboss.logmanager.LogManager` if the
//!   user has wired a JAR onto the classpath: this module only
//!   registers the *native shim* for `Class.newInstance` — when the
//!   real bytecode is present, `class_manager.rs::upgrade_synthetic_class`
//!   has already replaced our stub with the real class and the JDK
//!   bytecode path runs unmodified.
//! * Default case (`-Djava.util.logging.manager` unset) is unaffected:
//!   the existing JDK code path (allocate
//!   `java.util.logging.LogManager` directly) still executes.

#![allow(clippy::needless_pass_by_value)]

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

use crate::lang_class::native_class_new_instance;

/// Boot-log file path resolved on first use (cached so we don't pay the
/// `get_system_property` cost on every log line). `None` here means "no
/// path configured at first call → fall back to stderr forever". This is
/// deliberate: the brief asks the path to be honoured if set; thrashing
/// open/close on every emission would make per-line costs unacceptable.
fn boot_log_path() -> &'static Mutex<Option<Option<String>>> {
    static INSTANCE: OnceLock<Mutex<Option<Option<String>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Public-for-tests helper: clear the cached boot-log path resolution
/// so a fresh `get_system_property("org.jboss.boot.log.file")` call
/// runs on the next `emit_boot_log` invocation.
#[cfg(test)]
pub(crate) fn reset_boot_log_path_cache_for_tests() {
    if let Ok(mut g) = boot_log_path().lock() {
        *g = None;
    }
}

/// Resolve the boot-log file path lazily. Returns `Some(path)` if the
/// system property is set to a non-empty value, `None` otherwise.
fn resolve_boot_log_path(ctx: &mut dyn NativeContext) -> Option<String> {
    let mut guard = boot_log_path().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = guard.as_ref() {
        return cached.clone();
    }
    let resolved = ctx
        .get_system_property("org.jboss.boot.log.file")
        .filter(|s| !s.is_empty());
    *guard = Some(resolved.clone());
    resolved
}

/// Append one bootstrap log line to either the configured boot-log
/// file or stderr. Format: `LEVEL [logger-name] message\n`.
///
/// Failures (file open / write errors) are intentionally swallowed:
/// boot-time logging must never derail the boot itself if the operator
/// configured an unwriteable path. Tracing-level diagnostics about the
/// drop go to `tracing::warn!` so a `RUST_LOG=warn` invocation still
/// surfaces them.
pub fn emit_boot_log(ctx: &mut dyn NativeContext, level: &str, name: &str, msg: &str) {
    let path = resolve_boot_log_path(ctx);
    emit_boot_log_with_path(path.as_deref(), level, name, msg);
}

/// Context-free variant of [`emit_boot_log`] used by code paths that
/// don't have a `&mut dyn NativeContext` handy (e.g. the
/// `wildfly_core::LoggerMirror::log` Send-safe path that runs on
/// background threads). Reads the cached path resolved by an earlier
/// `emit_boot_log` (or by `prime_boot_log_path`) without touching
/// `NativeContext`. If the cache hasn't been primed yet, falls back
/// to stderr — which is the same behaviour `emit_boot_log` would have
/// when the system property isn't set.
pub fn emit_boot_log_no_ctx(level: &str, name: &str, msg: &str) {
    let cached = boot_log_path()
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .flatten();
    emit_boot_log_with_path(cached.as_deref(), level, name, msg);
}

/// Pre-resolve and cache the boot-log path so that subsequent
/// `emit_boot_log_no_ctx` calls from background threads can read it
/// without a `NativeContext`. Idempotent — second call is a no-op if
/// the cache already records a value. Also emits a single header line
/// to the configured boot-log file (or stderr) so the file is
/// guaranteed to be non-empty when the operator goes to inspect it
/// — even if the WildFly bootstrap crashes before any logger emits.
pub fn prime_boot_log_path(ctx: &mut dyn NativeContext) {
    // Snapshot the cache state before resolving so we can detect a
    // first-time prime and emit the header exactly once. Subsequent
    // primes (e.g. from re-entrant `getLogManager` calls during
    // initLogManager) are silent.
    let was_primed = boot_log_path()
        .lock()
        .ok()
        .map(|g| g.is_some())
        .unwrap_or(true);
    let path = resolve_boot_log_path(ctx);
    if !was_primed {
        emit_boot_log_with_path(
            path.as_deref(),
            "INFO",
            "rustjvm.bootstrap",
            "Block 2C: synthetic JBoss LogManager shim active; \
             routing INFO/WARN/SEVERE Logger calls through this file.",
        );
    }
}

fn emit_boot_log_with_path(path: Option<&str>, level: &str, name: &str, msg: &str) {
    let line = format!("{} [{}] {}\n", level, name, msg);
    if let Some(path) = path {
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(line.as_bytes()) {
                    tracing::warn!(error = %e, path = %path, "boot-log write failed");
                }
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path, "boot-log open failed");
            }
        }
    }
    // Stderr fallback. Use eprint! (NOT eprintln!) because `line` already
    // ends with \n. Plain eprint is not flagged by check-no-diag-prints.sh —
    // that script only rejects the `[DIAG-*]` / `[TRACE-*]` / `[SB-TRACE]`
    // bring-up markers we removed long ago.
    eprint!("{}", line);
}

/// Install every native this module owns. Called from
/// `register_essential_natives` so it's active in BOTH synthetic-jdk
/// and real-JDK feature configurations — the latter is what KC16 boot
/// uses, and is exactly the configuration the WARNING is reproducible in.
///
/// We only register `Class.newInstance` here; the LogManager / Logger
/// surface itself is owned by `logmanager.rs::register_logmanager_natives`
/// (also wired into `register_essential_natives` via
/// `register_annotation_overrides`). Registering the same method twice
/// is harmless — `NativeMethodRegistry::register` is last-writer-wins —
/// but we deliberately avoid duplicating that work here.
pub fn register_jboss_logmanager_natives(registry: &mut NativeMethodRegistry) {
    // `Class.newInstance` re-registration. The synthetic-jdk feature
    // config registers this same callback in `register_synthetic_overrides`
    // at line 5311; without the duplicate registration here, real-JDK
    // mode (`use_synthetic_jdk == false`) leaves the JDK reflection-based
    // bytecode in place, which fails on synthetic-stub classes whose
    // `methods` vec is empty (they have no `<init>` Method object even
    // though we DO register a `<init>()V` native callback). Our native
    // bypass calls `ctx.invoke(name, "<init>", "()V", &[obj])` directly,
    // which dispatches through the registry and finds the no-op
    // `<init>` from `logmanager.rs::native_jboss_init`.
    //
    // Risk surface: this changes the behaviour of `Class.newInstance()`
    // on EVERY class in real-JDK mode, not just LogManager. Reviewed —
    // for a class that has its own bytecode `<init>`, our native still
    // finds the right entry because the `NativeMethodRegistry::find`
    // call inside `ctx.invoke` walks through the bytecode dispatch
    // when no native is registered. The only observable behaviour
    // change is that synthetic-stub classes now get a runnable
    // newInstance instead of an InstantiationException.
    registry.register(
        "java/lang/Class",
        "newInstance",
        "()Ljava/lang/Object;",
        native_class_new_instance,
    );

    // `org.jboss.logmanager.LogManager.<init>(Ljava/lang/String;)V` —
    // some bootstrap paths reflectively call the legacy single-arg
    // ctor (the real JBoss class doesn't have it but synthetic-stub
    // discovery sometimes attempts it). No-op so the call succeeds.
    registry.register(
        "org/jboss/logmanager/LogManager",
        "<init>",
        "(Ljava/lang/String;)V",
        |_ctx, _args| Ok(None),
    );

    // Defensive: register `addLogger(Logger)Z` returning true. The
    // primary registration is in `logmanager.rs`; this duplicate makes
    // sure it's reachable even if someone reorders `register_essential_natives`
    // and the logmanager pass runs after this module.
    registry.register(
        "org/jboss/logmanager/LogManager",
        "addLogger",
        "(Ljava/util/logging/Logger;)Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );

}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;

    // Tests that touch the boot_log_path cache share process-wide state;
    // serialise them with a mutex so parallel tests don't race.
    fn test_lock() -> &'static std::sync::Mutex<()> {
        static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        L.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn register_adds_class_new_instance() {
        let mut r = NativeMethodRegistry::new();
        let before = r.len();
        register_jboss_logmanager_natives(&mut r);
        let after = r.len();
        assert!(
            after > before,
            "register_jboss_logmanager_natives must add at least one entry"
        );
        assert!(
            r.find("java/lang/Class", "newInstance", "()Ljava/lang/Object;")
                .is_some(),
            "Class.newInstance MUST be registered so JDK initLogManager's \
             reflection path is bypassed for synthetic-stub LogManager"
        );
    }

    #[test]
    fn register_adds_jboss_log_manager_string_init() {
        let mut r = NativeMethodRegistry::new();
        register_jboss_logmanager_natives(&mut r);
        assert!(
            r.find("org/jboss/logmanager/LogManager", "<init>", "(Ljava/lang/String;)V")
                .is_some(),
            "Single-arg LogManager ctor stub must be registered"
        );
    }

    #[test]
    fn register_adds_jboss_log_manager_add_logger() {
        let mut r = NativeMethodRegistry::new();
        register_jboss_logmanager_natives(&mut r);
        let cb = r
            .find("org/jboss/logmanager/LogManager", "addLogger", "(Ljava/util/logging/Logger;)Z")
            .expect("addLogger must be registered");
        let mut ctx = mock_ctx();
        let result = cb(&mut ctx, &[Value::Object(None), Value::Object(None)])
            .expect("addLogger should not error")
            .expect("addLogger should return a value");
        assert_eq!(
            result,
            Value::Int(1),
            "addLogger should return true (matches JBoss LM behaviour)"
        );
    }

    #[test]
    fn emit_boot_log_writes_to_file_when_property_set() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_boot_log_path_cache_for_tests();

        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "rustjvm_test_boot_log_{}.log",
            std::process::id()
        ));
        // Clean stale file from a prior test run.
        let _ = std::fs::remove_file(&path);

        let mut ctx = mock_ctx();
        ctx.set_system_property(
            "org.jboss.boot.log.file",
            path.to_str().expect("test path must be valid UTF-8"),
        );

        emit_boot_log(&mut ctx, "INFO", "test.logger", "hello world");

        let content = std::fs::read_to_string(&path).expect("boot log file must exist after emit");
        assert!(
            content.contains("INFO [test.logger] hello world"),
            "expected INFO+name+msg in {:?}, got {:?}",
            path,
            content
        );
        let _ = std::fs::remove_file(&path);
        reset_boot_log_path_cache_for_tests();
    }

    #[test]
    fn emit_boot_log_falls_back_to_stderr_when_property_unset() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_boot_log_path_cache_for_tests();

        // No system-property set -> resolve_boot_log_path returns None ->
        // emit goes to stderr. We can't easily capture stderr from a
        // unit test, so we just assert the call doesn't panic and the
        // cache records the unset state.
        let mut ctx = mock_ctx();
        emit_boot_log(&mut ctx, "WARNING", "no.handler", "fallback");

        let cached = boot_log_path().lock().unwrap().clone();
        assert_eq!(
            cached,
            Some(None),
            "after a stderr-fallback emit the cache must record the unset state \
             (Some(None) — initialised, but no path)"
        );
        reset_boot_log_path_cache_for_tests();
    }
}
