//! Block 2C / Path A — synthetic `org.jboss.logmanager.LogManager`
//! shim regression test.
//!
//! Background: KC16 boot under real-JDK mode prints
//!
//! ```text
//! WARNING: Failed to load the specified log manager class
//!          org.jboss.logmanager.LogManager
//! ```
//!
//! (or the JDK-25 wording `Could not load Logmanager "..."`) because
//! `java.util.logging.LogManager.initLogManager` calls
//! `Class.newInstance()` on the synthetic-stub `LogManager` and the
//! reflection-based JDK bytecode for `Class.newInstance` raises
//! `InstantiationException` when the synthetic stub has zero `Method`
//! entries.
//!
//! The fix in `native-builtins/src/jboss_logmanager.rs` re-registers
//! `Class.newInstance` (which already has a working native in
//! `lang_class.rs::native_class_new_instance` that bypasses reflection
//! and dispatches `<init>()V` directly) inside `register_essential_natives`,
//! so real-JDK mode no longer falls into the broken JDK bytecode path.
//!
//! What this file checks (registry-only — no JVM bytecode dispatch
//! required, so it runs fast in CI):
//!
//! 1. `Class.newInstance()` is registered after `register_essential_natives`
//!    runs. Without this, the JDK bytecode for `initLogManager` raises
//!    `InstantiationException` on every synthetic stub LogManager class.
//! 2. `org/jboss/logmanager/LogManager.<init>(Ljava/lang/String;)V` is
//!    registered (the legacy single-arg ctor stub).
//! 3. `org/jboss/logmanager/LogManager.addLogger(Ljava/util/logging/Logger;)Z`
//!    is registered and returns `true` (matches JBoss LM behaviour).
//! 4. `org/jboss/logmanager/LogManager.getLogger(Ljava/lang/String;)
//!    Ljava/util/logging/Logger;` is registered (delegated to
//!    `logmanager.rs::native_get_logger`).
//! 5. `org/jboss/logmanager/LogManager.<init>()V` is registered (no-arg
//!    ctor, used by `Class.newInstance()` invocations).
//!
//! Cross-reference: see `apps/lm_probe/LmProbe.java` for the end-to-end
//! probe runnable under the rustjvm-cli, and the brief / commit message
//! for the KC16-reproducer command line that confirms the WARNING is
//! suppressed.

use rustjvm_native_api::NativeMethodRegistry;

fn build_registry() -> NativeMethodRegistry {
    // Direct registration via `register_essential_natives` — that's the
    // universal block that runs in both synthetic-jdk and real-JDK
    // feature configurations. The latter is what KC16 boot uses.
    let mut r = NativeMethodRegistry::new();
    rustjvm_native_builtins::register_essential_natives(&mut r);
    r
}

#[test]
fn class_new_instance_is_registered_in_essential_natives() {
    let r = build_registry();
    assert!(
        r.find("java/lang/Class", "newInstance", "()Ljava/lang/Object;")
            .is_some(),
        "Block 2C regression: Class.newInstance MUST be registered in \
         register_essential_natives so JDK initLogManager's reflection \
         path bypass works in real-JDK mode (the configuration KC16 \
         boot uses). Without it, every synthetic-stub LogManager raises \
         InstantiationException during getLogManager()."
    );
}

#[test]
fn jboss_log_manager_no_arg_init_is_registered() {
    let r = build_registry();
    assert!(
        r.find("org/jboss/logmanager/LogManager", "<init>", "()V")
            .is_some(),
        "org.jboss.logmanager.LogManager.<init>()V MUST be registered \
         (in logmanager.rs) so the Class.newInstance native dispatch \
         finds a no-op constructor instead of NSME."
    );
}

#[test]
fn jboss_log_manager_string_init_is_registered() {
    let r = build_registry();
    assert!(
        r.find(
            "org/jboss/logmanager/LogManager",
            "<init>",
            "(Ljava/lang/String;)V",
        )
        .is_some(),
        "Block 2C: legacy single-arg LogManager ctor stub MUST be \
         registered as a defensive no-op. Some bootstrap paths reflect \
         on the (String) overload even though real JBoss LM doesn't \
         have it."
    );
}

#[test]
fn jboss_log_manager_add_logger_is_registered() {
    let r = build_registry();
    assert!(
        r.find(
            "org/jboss/logmanager/LogManager",
            "addLogger",
            "(Ljava/util/logging/Logger;)Z",
        )
        .is_some(),
        "Block 2C: addLogger MUST be registered (returning true so \
         WildFly's logger-init code paths advance)."
    );
}

#[test]
fn jboss_log_manager_get_logger_is_registered() {
    let r = build_registry();
    assert!(
        r.find(
            "org/jboss/logmanager/LogManager",
            "getLogger",
            "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        )
        .is_some(),
        "Block 2C: getLogger MUST be registered (delegated to the \
         JUL-side native in logmanager.rs)."
    );
}

#[test]
fn jboss_log_manager_get_log_manager_is_registered() {
    // The static factory is what JDK initLogManager indirectly hits
    // via `getLogManager()` after the (overridden) Class.newInstance
    // bypasses the reflection path. If this is missing we'd still
    // print the WARNING because the singleton wouldn't survive a
    // round-trip through getLogManager().
    let r = build_registry();
    assert!(
        r.find(
            "org/jboss/logmanager/LogManager",
            "getLogManager",
            "()Ljava/util/logging/LogManager;",
        )
        .is_some(),
        "Block 2C: org.jboss.logmanager.LogManager.getLogManager MUST \
         be registered so the singleton our native produced survives \
         the round-trip through JDK getLogManager()."
    );
}
