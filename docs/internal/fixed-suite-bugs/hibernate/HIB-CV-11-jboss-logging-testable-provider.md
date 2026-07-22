# HIB-CV-11 — JBoss Logging does not pick up Hibernate-testing's `TestableLoggerProvider` → log-inspection tests fail

**Severity:** Medium — fails ≥5 classes that inspect emitted log messages.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-builtins/src/jboss_msc.rs`). `LoggerProviders.findProvider` now honors the ServiceLoader-registered provider (Hibernate testing's `TestableLoggerProvider`), AND loads the `org.jboss.logging.LoggerProvider` interface on demand — the earlier partial fix used `class_id_by_name`, which returns None when `findProvider` runs during a message-logger `<clinit>` (before that interface class is loaded), silently falling back to JDK for the whole run. Verified: `Logger.getLogger(name)` → `DelegatingLogger` in the Hibernate context; BootLoggingTests, SessionFactoryNamingTests (5/5), AnyTypeFlushToLoggableStringTest, ImmutableEntityUpdateQueryHandlingModeWarningTest all pass (1 residual: UniqueConstraintBatchingTest fails on a separate assertion).
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected.

## Symptom

```
org.hibernate.AssertionFailure: Unexpected log type: JBoss Logger didn't register the custom
  TestableLoggerProvider as logger provider
```
and the variant
```
java.lang.AssertionError: Unexpected logger type: org.jboss.logging.JDKLogger.
  Logger must be: org.hibernate.testing.logger.DelegatingLogger
```

Affected (sample): `BootLoggingTests`, `SessionFactoryNamingTests`,
`UniqueConstraintBatchingTest`, `AnyTypeFlushToLoggableStringTest`,
`ImmutableEntityUpdateQueryHandlingModeWarningTest`.

## Root cause (mechanism)

Hibernate's `hibernate-testing` installs a custom `org.jboss.logging.LoggerProvider`
(`TestableLoggerProvider`) so tests can assert on emitted log records; `Logger.getLogger(...)` must
then return a `org.hibernate.testing.logger.DelegatingLogger`. On CratonVM, JBoss Logging instead
yields `org.jboss.logging.JDKLogger` (the JUL-backed default provider), so the test's
`LoggerInspectionExtension` assertion that the active logger is the testable/delegating type fails.

CratonVM's handling of `org.jboss.logging.Logger` / `LoggerProviders` provider selection does not
honour the test-installed `TestableLoggerProvider` (likely a native/synthetic JBoss-Logging shim
that hard-routes to `JDKLogger`, bypassing `LoggerProviders.find()` / the
`org.jboss.logging.provider` discovery path). The real provider-selection must run so the
test-registered provider wins.

## Suspected area / next step

Find where CratonVM intercepts `org.jboss.logging.Logger.getLogger` / `LoggerProviders` and ensure
the real provider-detection (system property `org.jboss.logging.provider`, ServiceLoader, classpath
probe) executes — so `TestableLoggerProvider`/`DelegatingLogger` is selected when the testing
harness installs it. Cross-check with a probe that calls
`org.jboss.logging.Logger.getLogger("x").getClass()` after the test provider is registered.

## Residual (2026-06-15) — provider locked to JDK in the Hibernate context

`findProvider` honoring ServiceLoader is verified in a **minimal** context (`SlProbe`: `Logger.getLogger("x")` → `DelegatingLogger`). But in the **full Hibernate** context (`ConnLogProbe`: reference `ConnectionInfoLogger.CONNECTION_INFO_LOGGER` first, then `Logger.getLogger(LOGGER_NAME)`), CratonVM returns **`JDKLogger`** even for a brand-new logger name — HotSpot returns `DelegatingLogger`. So `org.jboss.logging.LoggerProviders` is selecting/caching `JDKLoggerProvider` early (first triggered by `getMessageLogger` during a message-logger interface `<clinit>`), via a path that does **not** go through the fixed `findProvider` ServiceLoader selection. Passing the `LoggerProvider` interface's own classloader to `ServiceLoader.load` (instead of the TCCL) did **not** change it. Next step: find where the JDK provider is force-selected/cached during early Hibernate logging bootstrap (candidate: a second `Logger.getLogger`/`getMessageLogger` interception, or a pre-seeded `LoggerProviders` static) and route it through the real provider selection. Remaining: 5 `@MessageKeyInspection` classes.
