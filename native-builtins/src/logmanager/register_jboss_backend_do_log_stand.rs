#[allow(unused_imports)]
use super::*;

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// gci i40/jb: the twelve `doLog` / `doLogf` console stand-ins of the six
/// jboss-logging backends, registered only with
/// `CRATONVM_JBOSS_NO_CONSOLE_SINK=0` (see the call site in
/// [`register_logmanager_natives`]). The caller holds the explicit `Bridge`
/// scope (AGENTS.md); every triple is a retired shadow, so `register`
/// re-tags it `SyntheticStub`. The loop is the pre-i40 one, verbatim (the
/// registrar-drift parser expands literal loops).
pub fn register_jboss_backend_do_log_stand_ins(registry: &mut NativeMethodRegistry) {
    for cls in &[
        "org/jboss/logging/JBossLogManagerLogger",
        "org/jboss/logging/JDKLogger",
        "org/jboss/logging/Slf4jLogger",
        "org/jboss/logging/Slf4jLocationAwareLogger",
        "org/jboss/logging/Log4j2Logger",
        "org/jboss/logging/Log4jLogger",
    ] {
        registry.register(
            cls,
            "doLog",
            "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/Object;[Ljava/lang/Object;Ljava/lang/Throwable;)V",
            native_jboss_logging_logger_do_log, NativeKind::SyntheticStub,
        );
        registry.register(
            cls,
            "doLogf",
            "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;Ljava/lang/Throwable;)V",
            native_jboss_logging_logger_do_logf, NativeKind::SyntheticStub,
        );
    }
}

/// Register every `LogManager` / `Logger` native this module owns. Call
/// from `register_essential_natives` in `lib.rs` BEFORE the fallback
/// stubs so these win in the `NativeMethodRegistry` lookup.
pub fn register_logmanager_natives(registry: &mut NativeMethodRegistry) {
    // ---------------- java.util.logging.Level ----------------
    registry.register(
        CLS_JUL_LEVEL,
        "parse",
        "(Ljava/lang/String;)Ljava/util/logging/Level;",
        native_level_parse, NativeKind::SyntheticStub,
    );
    // `Level.findLevel` is `parse`'s non-throwing, package-private sibling —
    // and it is the one the JDK's OWN logging bytecode uses. `LogManager
    // .getLevelProperty(name, default)` is literally
    // `getProperty(name)` -> `Level.findLevel(val.trim())` -> `?: default`,
    // and `Handler`'s `configure()` (run from `new ConsoleHandler()`) applies
    // its configured level through exactly that call.
    //
    // With `parse` overridden but `findLevel` left to real bytecode, the two
    // disagreed: the real `findLevel` resolves names through `Level`'s
    // internal `KnownLevel` registry, which our natively-created `Level`
    // instances are never entered into, so it returned null and every
    // `getLevelProperty` silently fell back to its default. Observable as
    // `java.util.logging.ConsoleHandler.level = ALL` in `conf/logging.properties`
    // producing a handler at `INFO`, which then dropped every FINE/FINER
    // record — while `getProperty` and `Level.parse` both returned `ALL`
    // when called directly, which is what made it look like a level-config
    // bug rather than a handler-config one.
    registry.register(
        CLS_JUL_LEVEL,
        "findLevel",
        "(Ljava/lang/String;)Ljava/util/logging/Level;",
        native_level_find_level, NativeKind::SyntheticStub,
    );
    // ---------------- java.util.logging.LogManager ----------------
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "getLogManager",
        "()Ljava/util/logging/LogManager;",
        native_get_log_manager, NativeKind::SyntheticStub,
    );
    registry.register(CLS_JUL_LOG_MANAGER, "<init>", "()V", native_jboss_init, NativeKind::SyntheticStub);
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        native_get_logger, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "addLogger",
        "(Ljava/util/logging/Logger;)Z",
        native_add_logger, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "readConfiguration",
        "()V",
        native_read_configuration_no_arg, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "readConfiguration",
        "(Ljava/io/InputStream;)V",
        native_read_configuration_with_stream, NativeKind::SyntheticStub,
    );
    registry.register(CLS_JUL_LOG_MANAGER, "reset", "()V", native_reset, NativeKind::SyntheticStub);
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "getLoggerNames",
        "()Ljava/util/Enumeration;",
        native_get_logger_names, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_get_property, NativeKind::SyntheticStub,
    );
    // KEEP (real JDK body has nothing left to do) — re-derived wave 4,
    // 2026-07-28. `LogManager.checkAccess()`'s entire body was
    // `SecurityManager sm = System.getSecurityManager(); if (sm == null)
    // return; sm.checkPermission(new LoggingPermission("control", null));`.
    // JEP 486 (JDK 24) permanently disabled the Security Manager:
    // `System.getSecurityManager()` now always returns null, so on a real JDK
    // 25 the method returns immediately on its first branch for every caller.
    // An empty body is therefore behaviourally identical, not a suppressed
    // check — and it must stay empty: `Logger.setLevel`/`addHandler`/`reset`
    // all call it, so throwing here would break configuration, not secure it.
    registry.register(CLS_JUL_LOG_MANAGER, "checkAccess", "()V", |_ctx, _args| {
        Ok(None)
    }, NativeKind::SyntheticStub);
    // IMPLEMENTED wave 4 (2026-07-28). Both halves used to be constants: `add`
    // returned `this` and dropped the listener on the floor, `remove` was a
    // no-op justified as "consistent with the add". The justification was
    // wrong — `readConfiguration()`/`readConfiguration(InputStream)`/
    // `updateConfiguration(...)` are all really implemented in this module, and
    // the JDK fires the listener chain after each of them, so the drop WAS
    // observable. They are now backed by `config_listeners(vm)` and fired from
    // `fire_configuration_listeners`.
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "addConfigurationListener",
        "(Ljava/lang/Runnable;)Ljava/util/logging/LogManager;",
        native_add_configuration_listener, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "removeConfigurationListener",
        "(Ljava/lang/Runnable;)V",
        native_remove_configuration_listener, NativeKind::SyntheticStub,
    );
    // `updateConfiguration(Function)` re-reads the DEFAULT (filesystem /
    // `java.util.logging.config.file`) configuration — exactly the input
    // `native_read_configuration_no_arg` refuses to parse, and for exactly the
    // same reason. Sharing that handler keeps the two refusals from drifting
    // apart, and keeps the rationale in ONE place.
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "updateConfiguration",
        "(Ljava/util/function/Function;)V",
        native_read_configuration_no_arg, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "updateConfiguration",
        "(Ljava/io/InputStream;Ljava/util/function/Function;)V",
        native_update_configuration_with_stream, NativeKind::SyntheticStub,
    );

    // ---------------- org.jboss.logmanager.LogManager ----------------
    // When Quarkus sets `java.util.logging.manager=org.jboss.logmanager.LogManager`,
    // the JDK's `LogManager.getLogManager()` reflectively instantiates
    // the JBoss subclass. We intercept that path so the result is our
    // same singleton and so the `<init>` no-op doesn't trip on the
    // JBoss-specific bytecode which wires up a handler chain we don't
    // implement.
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "getLogManager",
        "()Ljava/util/logging/LogManager;",
        native_get_jboss_log_manager, NativeKind::SyntheticStub,
    );
    registry.register(CLS_JBOSS_LOG_MANAGER, "<init>", "()V", native_jboss_init, NativeKind::SyntheticStub);
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        native_get_jboss_manager_logger, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "addLogger",
        "(Ljava/util/logging/Logger;)Z",
        native_add_logger, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "readConfiguration",
        "()V",
        native_read_configuration_no_arg, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "readConfiguration",
        "(Ljava/io/InputStream;)V",
        native_read_configuration_with_stream, NativeKind::SyntheticStub,
    );
    registry.register(CLS_JBOSS_LOG_MANAGER, "reset", "()V", native_reset, NativeKind::SyntheticStub);
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "getLoggerNames",
        "()Ljava/util/Enumeration;",
        native_get_logger_names, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_get_property, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "checkAccess",
        "()V",
        // KEEP — same reasoning as the `java.util.logging.LogManager.checkAccess`
        // registration above (JEP 486 leaves the inherited body with nothing to
        // do). `org.jboss.logmanager.LogManager` does not override it; this
        // registration exists only so the JBoss class name also resolves.
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge,
    );

    // ---------------- KC16: org.jboss.logmanager.Logger attachment overrides ----------------
    // Override real-JDK `org/jboss/logmanager/Logger.getAttachment` /
    // `attach` / `attachIfAbsent` / `detach` with side-table-backed
    // natives. The bytecode bodies in jboss-logmanager-2.1.18.Final.jar
    // dereference `this.loggerNode` which is null when the JDK warns
    // "Failed to load the specified log manager class
    // org.jboss.logmanager.LogManager" and falls back to a plain
    // `Logger`. See `vm_exec.rs` `check_override` for the dispatch
    // override that selects these natives over the real bytecode.
    // If you still see the JDK line "Failed to load the specified log
    // manager class org.jboss.logmanager.LogManager", ensure
    // `jboss-logmanager` is visible on the same classpath / layer as
    // `-Djava.util.logging.manager=org.jboss.logmanager.LogManager`
    // (WildFly ships it under `modules/`).
    registry.register(
        "org/jboss/logmanager/Logger",
        "getAttachment",
        "(Lorg/jboss/logmanager/Logger$AttachmentKey;)Ljava/lang/Object;",
        native_jboss_logger_get_attachment, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "attach",
        "(Lorg/jboss/logmanager/Logger$AttachmentKey;Ljava/lang/Object;)Ljava/lang/Object;",
        native_jboss_logger_attach, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "attachIfAbsent",
        "(Lorg/jboss/logmanager/Logger$AttachmentKey;Ljava/lang/Object;)Ljava/lang/Object;",
        native_jboss_logger_attach_if_absent, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "detach",
        "(Lorg/jboss/logmanager/Logger$AttachmentKey;)Ljava/lang/Object;",
        native_jboss_logger_detach, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getLevel",
        "()Ljava/util/logging/Level;",
        native_jboss_logger_get_level, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getParent",
        "()Lorg/jboss/logmanager/Logger;",
        native_jboss_logger_get_parent, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "setLevel",
        "(Ljava/util/logging/Level;)V",
        native_jboss_logger_set_level, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "isLoggable",
        "(Ljava/util/logging/Level;)Z",
        native_jboss_logger_is_loggable, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getLogContext",
        "()Lorg/jboss/logmanager/LogContext;",
        native_jboss_logger_get_log_context, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getEffectiveLevel",
        "()I",
        native_jboss_logger_get_effective_level, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getName",
        "()Ljava/lang/String;",
        native_jboss_logger_get_name, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getUseParentHandlers",
        "()Z",
        native_jboss_logger_get_use_parent_handlers, NativeKind::SyntheticStub,
    );
    // setUseParentHandlers(Z)V — the REAL bytecode does
    // `this.loggerNode.setUseParentHandlers(flag)`, which NPEs on our synthetic
    // Logger's null `loggerNode`. Keycloak's RUNTIME_INIT logging configuration
    // calls this (per-category `setUseParentHandlers(false)`) and the NPE aborts
    // startup: "Cannot invoke org.jboss.logmanager.LoggerNode.setUseParentHandlers
    // because this.loggerNode is null". Null-safe no-op, mirroring the
    // `java/util/logging/Logger` override and `getUseParentHandlers`'s constant
    // (we don't model a real LoggerNode; the value isn't tracked).
    registry.register(
        "org/jboss/logmanager/Logger",
        "setUseParentHandlers",
        "(Z)V",
        native_jboss_logger_handler_noop, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getHandlers",
        "()[Ljava/util/logging/Handler;",
        native_jboss_logger_get_handlers, NativeKind::SyntheticStub,
    );
    // The three handler MUTATORS were no-ops (and `getHandlers` a constant
    // empty array) for as long as nothing but WildFly/Keycloak boot logging
    // reached this class — those callers only needed the methods not to NPE on
    // the synthetic Logger's null `loggerNode`. Repointing
    // `LogManager.getLogger` at the JBoss shape (the fix in
    // `julogger-cast-to-jbosslogmanager-logger-20260817-FIXED.md`) made this the
    // shape ORDINARY application code gets back, and the first thing
    // `io.quarkus.test.AbstractQuarkusExtensionTest.beforeAll` does with it is
    // install an `InMemoryLogHandler` on the root logger and later assert on
    // what it collected. Against the no-ops it collected nothing, forever,
    // with no error anywhere — the JUL face has had a working, GC-safe,
    // ancestor-walking handler chain the whole time, so wire this one to it
    // rather than growing a second.
    registry.register(
        "org/jboss/logmanager/Logger",
        "addHandler",
        "(Ljava/util/logging/Handler;)V",
        native_jul_logger_add_handler, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "removeHandler",
        "(Ljava/util/logging/Handler;)V",
        native_jul_logger_remove_handler, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "setHandlers",
        "([Ljava/util/logging/Handler;)V",
        native_jboss_logger_set_handlers, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "setUseParentFilters",
        "(Z)V",
        native_jboss_logger_handler_noop, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getUseParentFilters",
        "()Z",
        native_jboss_logger_get_use_parent_filters, NativeKind::SyntheticStub,
    );
    // Keycloak boot NPE — `Logger.logRaw` real-JDK bytecode dereferences
    // `this.loggerNode` and NPEs at `LoggerNode.isLoggable` (pc=48) and
    // `LoggerNode.publish` (pc=70). Our synthetic Logger has no
    // LoggerNode wired up, so any caller path (e.g. `Log4jLogger.doLogf`
    // -> `JBossLogManagerFacade` -> `Logger.logRaw`) crashes during
    // `SystemExiter.logBeforeExit` on WildFly bootstrap. Force the
    // null-safe native override that emits a best-effort line to stderr
    // and returns without touching the missing field.
    //
    // gci i38/jb (AGENTS.md: state the kind at the site): from here to the
    // end of the `doLog` / `doLogf` loop below the JBoss console stand-ins
    // are registered under an explicit `Bridge` scope, restored after the
    // loop. It was the ambient `Bridge` of `register_essential_natives_with_
    // shims` (via `register_annotation_overrides`), so nothing changes at run
    // time: every one of these sixteen triples is a retired shadow
    // (`retired_shadow.rs` `RETIRED_SHADOW_QUARKUS_LOG_TRIPLES`,
    // `RETIRED_SHADOW_SBR2_SPRINGAPP_APP_TRIPLES`), so `register` re-tags each
    // `Bridge` to `SyntheticStub` -- dropped under `--real-jdk`, and under
    // `--compatible` beaten by the library's `Code` at every door that knows
    // it (`resolve_dispatch` step 3). The registry census does not move.
    // Removal record: `gci-i32jh-compatible-jboss-and-juli-console-sinks-are-
    // not-hotspots-20261015` (needs the real jboss-logmanager pipeline on a
    // VM-built `org.jboss.logmanager.Logger`, i.e. a non-null `loggerNode`).
    //
    // gci i40/jb: the twelve `doLog` / `doLogf` rows are retired on the
    // default path (below; -12 on the registry census); the four `org/jboss/
    // logmanager/Logger` rows stay (they guard a VM-minted `Logger` carrier,
    // `get_or_create_jboss_logger`, whose `LoggerNode` is a minimal shell)
    // but print no sink line by default (`jboss_console_sink_off`).
            registry.register(
        "org/jboss/logmanager/Logger",
        "logRaw",
        "(Lorg/jboss/logmanager/ExtLogRecord;)V",
        native_jboss_logger_log_raw, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "logRaw",
        "(Ljava/util/logging/LogRecord;)V",
        native_jboss_logger_log_raw, cratonvm_native_api::NativeKind::Bridge,
    );
    // log(Level, Supplier<String>) — see native_jboss_logger_log_level_supplier
    // doc comment: this overload's real bytecode NPEs on `this.loggerNode`
    // before it ever reaches logRaw.
    registry.register(
        "org/jboss/logmanager/Logger",
        "log",
        "(Ljava/util/logging/Level;Ljava/util/function/Supplier;)V",
        native_jboss_logger_log_level_supplier, cratonvm_native_api::NativeKind::Bridge,
    );
    // log(LogRecord) — same loggerNode NPE, one level up from logRaw:
    // JUnit Platform's `LoggerFactory$DelegatingLogger.log` builds a real
    // `LogRecord` itself (via `createLogRecord`) and calls this overload
    // directly rather than `logRaw`. `native_jboss_logger_log_raw` already
    // extracts loggerName/level/message/thrown from a record BY NAME, which
    // works identically for a plain LogRecord, so reuse it as-is.
    registry.register(
        "org/jboss/logmanager/Logger",
        "log",
        "(Ljava/util/logging/LogRecord;)V",
        native_jboss_logger_log_raw, cratonvm_native_api::NativeKind::Bridge,
    );

    // ---------------- WFLY visibility: JBossLogManagerLogger.doLog / doLogf ----------------
    // WildFly's boot logging goes:
    //   org.jboss.as.server.ServerLogger.info("WFLYSRV0025: ...")
    //     → org.jboss.logging.Logger.info(...)
    //     → org.jboss.logging.JBossLogManagerLogger.doLog(Level,fqcn,msg,params,t)
    //     → org.jboss.logmanager.Logger.logRaw(ExtLogRecord)  [null-safe stub]
    // The logRaw stub never sees the original message string (LogRecord
    // field offsets unknown). Intercepting `doLog`/`doLogf` directly
    // gives us the message object as args[3], the level enum as args[1],
    // and the logger name from `this.name`. Emit a formatted line to
    // stdout (`emit_framework_log`) so every WFLY* / JBAS* / Hibernate /
    // Undertow log surface.
    //
    // gci i40/jb (`gci-i32jh-...-20261015`, Batch 9b B-VERDICT (i)): RETIRED on
    // the default path (`jboss_console_sink_off`, `CRATONVM_JBOSS_NO_CONSOLE_SINK`
    // default ON; `=0` registers them again). Each of the six backends' `doLog`
    // / `doLogf` is concrete library bytecode, so `resolve_dispatch` step 3 /
    // `resolve_native_dispatch_wave1` (`bytecode_available && kind !=
    // Intrinsic`) already ran the library at every door that resolves the
    // method: `vi38-jb.list` B measured `JDKLogger` and `JBossLogManagerLogger`
    // (JIT and `--nojit`): HotSpot's records, no `org/jboss/` native invoked,
    // no sink line. `vi40-jb.list` B adds `Slf4j*Logger` and `Log4j2Logger`.
    // The `loggerNode` hazard these were once a detour around sits one level
    // down, in `org/jboss/logmanager/Logger.logRaw`, whose null-safe stand-in
    // above stays. -12 registrations, all retired-shadow `SyntheticStub`s
    // (`the native-registry census`, every arm).
    if !jboss_console_sink_off() {
        register_jboss_backend_do_log_stand_ins(registry);
    }
    // gci i38/jb: end of the explicit `Bridge` scope opened before `logRaw`.
    
    // Round 92 (OPT-IN ONLY, default OFF): native overrides for the abstract
    // `org/jboss/logging/Logger.info/warn/error/...` overloads.
    //
    // Registering natives on the base `Logger` short-circuits virtual dispatch
    // to the concrete subtype's `doLog`/`doLogf`. That silently DEFEATS any
    // user `Logger` subclass that overrides `doLog` to observe events — most
    // importantly Hibernate's testing `DelegatingLogger`, whose `doLog` drives
    // the `LogListener` interception behind `@LoggingInspections` /
    // `MessageKeyWatcher` / `Triggerable`. With these base-class natives in
    // place every log-assertion test observed ZERO events, producing CV-only
    // wrong-result FAILs across the Hibernate suite (e.g.
    // UniqueConstraintBatchingTest expected:<1> but was:<0>, the
    // DetachedBag delayed-operation watchers, …). So by DEFAULT we now let the
    // real jboss-logging bytecode run and dispatch through to the real
    // `doLog`/`doLogf`.
    //
    // WildFly boot visibility does NOT depend on this block: the Round-90
    // `doLog`/`doLogf` intercepts registered above already fire on every
    // concrete backend subclass (JBossLogManagerLogger, JDKLogger, Slf4jLogger,
    // Log4j2Logger, Log4jLogger), so `WFLY*` boot messages still reach stderr.
    // Set CRATONVM_JBOSS_LOGGER_BASE_EMIT=1 to restore the old blanket
    // base-class emit if a logger outside that set ever needs it.
    if crate::nbflags().jboss_logger_base_emit {
        let jlog = "org/jboss/logging/Logger";
        // info family
        // The `(String loggerFqcn, Object message, Throwable)` forms get the
        // fqcn-aware handler — message is args[2], NOT the first String arg.
        registry.register(
            jlog,
            "info",
            "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Throwable;)V",
            native_jboss_logger_info_fqcn, NativeKind::SyntheticStub,
        );
        registry.register(
            jlog,
            "warn",
            "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Throwable;)V",
            native_jboss_logger_warn_fqcn, NativeKind::SyntheticStub,
        );
        registry.register(
            jlog,
            "error",
            "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Throwable;)V",
            native_jboss_logger_error_fqcn, NativeKind::SyntheticStub,
        );
        for (m, sig) in &[
            ("info", "(Ljava/lang/Object;)V"),
            ("info", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("infof", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "infof",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            (
                "infof",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("infof", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            (
                "infof",
                "(Ljava/lang/Throwable;Ljava/lang/String;[Ljava/lang/Object;)V",
            ),
            ("infov", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "infov",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            (
                "infov",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("infov", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            (
                "infov",
                "(Ljava/lang/Throwable;Ljava/lang/String;[Ljava/lang/Object;)V",
            ),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_info, NativeKind::SyntheticStub);
        }
        for (m, sig) in &[
            ("warn", "(Ljava/lang/Object;)V"),
            ("warn", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("warnf", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "warnf",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            (
                "warnf",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("warnf", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            (
                "warnf",
                "(Ljava/lang/Throwable;Ljava/lang/String;[Ljava/lang/Object;)V",
            ),
            ("warnv", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "warnv",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            (
                "warnv",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("warnv", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            (
                "warnv",
                "(Ljava/lang/Throwable;Ljava/lang/String;[Ljava/lang/Object;)V",
            ),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_warn, NativeKind::SyntheticStub);
        }
        for (m, sig) in &[
            ("error", "(Ljava/lang/Object;)V"),
            ("error", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("errorf", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "errorf",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            (
                "errorf",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("errorf", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            (
                "errorf",
                "(Ljava/lang/Throwable;Ljava/lang/String;[Ljava/lang/Object;)V",
            ),
            ("errorv", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "errorv",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("errorv", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_error, NativeKind::SyntheticStub);
        }
        for (m, sig) in &[
            ("fatal", "(Ljava/lang/Object;)V"),
            ("fatal", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("fatalf", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            ("fatalv", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_fatal, NativeKind::SyntheticStub);
        }
        for (m, sig) in &[
            ("debug", "(Ljava/lang/Object;)V"),
            ("debug", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("debugf", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            ("debugv", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_debug, NativeKind::SyntheticStub);
        }
        for (m, sig) in &[
            ("trace", "(Ljava/lang/Object;)V"),
            ("trace", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("tracef", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            ("tracev", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_trace, NativeKind::SyntheticStub);
        }
    }

    // Static JUL factory methods. When WildFly installs
    // org.jboss.logmanager.LogManager, JBoss's own Logger.getLogger delegates
    // here and immediately checkcasts the result to org.jboss.logmanager.Logger.
    registry.register(
        CLS_JUL_LOGGER,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        native_jul_static_get_logger, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "getLogger",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/util/logging/Logger;",
        native_jul_static_get_logger_with_bundle, NativeKind::SyntheticStub,
    );

    registry.register(
        "java/util/logging/LogRecord",
        "getMessage",
        "()Ljava/lang/String;",
        native_jul_log_record_get_message, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "addHandler",
        "(Ljava/util/logging/Handler;)V",
        native_jul_logger_add_handler, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "removeHandler",
        "(Ljava/util/logging/Handler;)V",
        native_jul_logger_remove_handler, NativeKind::SyntheticStub,
    );

    // JUL convenience methods for callers that bypass jboss-logging.
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;)V",
        native_jul_logger_log_level_msg, NativeKind::SyntheticStub,
    );
    // `log(Level, String, Object)` — single-parameter overload; the JDK
    // wraps `param1` in a one-element array before building the LogRecord.
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/Object;)V",
        native_jul_logger_log_param, NativeKind::SyntheticStub,
    );
    // `log(Level, String, Object[])` — MessageFormat-style parameterized
    // overload. See `native_jul_logger_log_params` doc comment: this is
    // the exact call Jython 2.7.4's `PrePy.maybeWrite` makes on every
    // startup warning, and its absence was a real (non-clinit-ordering)
    // NPE in `Logger.getEffectiveLoggerBundle()` reading the never-populated
    // `loggerBundle` field on our synthetic Logger shape.
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;[Ljava/lang/Object;)V",
        native_jul_logger_log_params, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "info",
        "(Ljava/lang/String;)V",
        native_jul_logger_info, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "warning",
        "(Ljava/lang/String;)V",
        native_jul_logger_warning, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "severe",
        "(Ljava/lang/String;)V",
        native_jul_logger_severe, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "fine",
        "(Ljava/lang/String;)V",
        native_jul_logger_fine, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "finer",
        "(Ljava/lang/String;)V",
        native_jul_logger_finer, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "finest",
        "(Ljava/lang/String;)V",
        native_jul_logger_finest, NativeKind::SyntheticStub,
    );
    // `logp(Level, sourceClass, sourceMethod, msg)` and the 5-arg
    // variant with a trailing Throwable. JULI's DirectJDKLog routes
    // every Tomcat/JULI log call through these instead of the simple
    // `warning(String)` / `log(Level,String)` helpers, so a missing
    // native here swallows every Tomcat log line silently (rc=0, no
    // output) — that was the entire "Bootstrap version prints
    // nothing" symptom.
    registry.register(
        CLS_JUL_LOGGER,
        "logp",
        "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        native_jul_logger_logp, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "logp",
        "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/Throwable;)V",
        native_jul_logger_logp, NativeKind::SyntheticStub,
    );
    // `entering`/`exiting`/`throwing` — the method-trace family. The real-JDK
    // bodies build their FINER record and push it through the private
    // `doLog`, which dereferences `Logger.loggerBundle`; that field is null on
    // the Logger instances `getLogger` mints here (no constructor ever ran),
    // which is what made `throwing` raise
    // `NullPointerException: ... "lb" is null`. `allocate_logger` now seeds
    // the field, and these natives additionally make the whole family behave
    // identically no matter which JUL class body is loaded — including
    // stamping `LogRecord.thrown`, which the console-only path dropped.
    // NOTE: `phases_late::register_p71_logging_extras` registers the same
    // three triples, but only along the synthetic-JDK path; this registrar is
    // the one that also runs in default real-JDK mode (via
    // `reflect_annotations::register_annotation_overrides`) and, being
    // registered later, wins in both.
    registry.register(
        CLS_JUL_LOGGER,
        "entering",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| jul_trace_marker(ctx, args, "ENTRY"), NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "exiting",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| jul_trace_marker(ctx, args, "RETURN"), NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "throwing",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/Throwable;)V",
        |ctx, args| jul_trace_marker(ctx, args, "THROW"), NativeKind::SyntheticStub,
    );
    // Throwable/Supplier-carrying `log` overloads. The real JDK routes these
    // through a LogRecord + handler chain the synthetic JUL doesn't wire, so
    // the records (and their throwables) were silently dropped — hiding errors
    // that callers log-and-swallow (e.g. JUnit's `ListenerRegistry.notifyEach`).
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/Throwable;)V",
        native_jul_logger_log_throwable, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/util/function/Supplier;Ljava/lang/Throwable;)V",
        native_jul_logger_log_throwable, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/Throwable;Ljava/util/function/Supplier;)V",
        native_jul_logger_log_throwable, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/util/function/Supplier;)V",
        native_jul_logger_log_supplier, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/LogRecord;)V",
        native_jul_logger_log_record, NativeKind::SyntheticStub,
    );
    // `isLoggable(Level)` — JULI's DirectJDKLog gates every log call on
    // this; the real bytecode returns false for our parent-less
    // synthetic Logger, swallowing all output. See native doc comment.
    registry.register(
        CLS_JUL_LOGGER,
        "isLoggable",
        "(Ljava/util/logging/Level;)Z",
        native_jul_logger_is_loggable, NativeKind::SyntheticStub,
    );

    // ---------------- KC16: org.jboss.logmanager.LogContext overrides ----------------
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getLogContext",
        "()Lorg/jboss/logmanager/LogContext;",
        native_jboss_log_context_get_log_context, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getSystemLogContext",
        "()Lorg/jboss/logmanager/LogContext;",
        native_jboss_log_context_get_log_context, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getLogger",
        "(Ljava/lang/String;)Lorg/jboss/logmanager/Logger;",
        native_jboss_log_context_get_logger, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getLoggerIfExists",
        "(Ljava/lang/String;)Lorg/jboss/logmanager/Logger;",
        native_jboss_log_context_get_logger_if_exists, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getLevelForName",
        "(Ljava/lang/String;)Ljava/util/logging/Level;",
        native_jboss_log_context_get_level_for_name, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "checkAccess",
        "(Lorg/jboss/logmanager/LogContext;)V",
        native_jboss_log_context_check_access, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "checkSecurityAccess",
        "()V",
        native_jboss_log_context_check_access, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "addCloseHandler",
        "(Ljava/lang/AutoCloseable;)V",
        native_jboss_log_context_add_close_handler, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getCloseHandlers",
        "()Ljava/util/Set;",
        native_jboss_log_context_get_close_handlers, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "setCloseHandlers",
        "(Ljava/util/Collection;)V",
        native_jboss_log_context_add_close_handler, NativeKind::SyntheticStub,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "close",
        "()V",
        native_jboss_log_context_add_close_handler, NativeKind::SyntheticStub,
    );

    // ---------------- KC16-JUL: java/util/logging/Logger null-safe accessors ----------------
    // Real-JDK `Logger.getResourceBundleName()` reads
    // `this.loggerBundle.resourceBundleName`, NPE-ing when our Logger init
    // path leaves `loggerBundle` null. WildFly's
    // `org/jboss/as/server/SystemExiter.logBeforeExit` calls this on the
    // exit-reason logger path, killing the boot.
    //
    // Both accessors used to answer a hardcoded null, which fixed that NPE by
    // making the resource-bundle feature invisible: `Logger.getLogger(name,
    // bundleName)` accepted a bundle name and no caller could ever read it
    // back, so localized JUL logging silently degraded to the raw message key
    // on every Logger, real or synthetic. `allocate_logger` now seeds
    // `loggerBundle` and `native_jul_static_get_logger_with_bundle` stamps a
    // named `LoggerBundle` when one was requested, so these read the real
    // field — still null-safe at every step, so the SystemExiter path keeps
    // its null answer and its no-NPE guarantee.
    registry.register(
        CLS_JUL_LOGGER,
        "getResourceBundleName",
        "()Ljava/lang/String;",
        native_jul_logger_get_resource_bundle_name, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "getResourceBundle",
        "()Ljava/util/ResourceBundle;",
        native_jul_logger_get_resource_bundle, NativeKind::SyntheticStub,
    );

    // ---------------- Enumeration<String> wrapper ----------------
    registry.register(
        CLS_LOGGER_ENUMERATION,
        "hasMoreElements",
        "()Z",
        native_enumeration_has_more, NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_LOGGER_ENUMERATION,
        "nextElement",
        "()Ljava/lang/Object;",
        native_enumeration_next, NativeKind::SyntheticStub,
    );
}

