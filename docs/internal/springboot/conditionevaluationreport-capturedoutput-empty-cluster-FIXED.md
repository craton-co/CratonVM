# `ConditionEvaluationReport*` logging tests: `CapturedOutput` is empty — condition-evaluation-report text never reaches the captured log stream

**Status: FIXED 2026-07-18**

## Resolution

Root cause: `ch/qos/logback/classic/Logger`/`LoggerContext.getLogger` and
`org/apache/commons/logging/LogFactory`/`Log` were natively overridden to
hand back throwaway **synthetic** objects instead of running real Logback/
commons-logging bytecode — a residual of the same "overlay bound to a
real-JDK class layout" family already fixed once for
`LoggerContext.<init>` itself (see
`logback-loggercontext-listenerlist-final-field-corruption-FIXED.md`).
That earlier fix made `LoggerContext` construction real (`ContextBase`
state, `StatusManager`, `TurboFilterList` all real), but never revisited
the sibling `getLogger()` method or `ch/qos/logback/classic/Logger`
itself — so every `LoggerContext.getLogger(...)` call still fabricated a
2-field synthetic `Logger` whose `addAppender`/`info`/`warn`/`error`/
`filterAndLog_*` were unconditional native no-ops. Concretely:
`BasicConfigurator.configure()` would call `context.getLogger("ROOT")`,
get a disposable synthetic `Logger` back (NOT the real `LoggerContext.root`
field the real constructor had already created), call `addAppender` on it
(a no-op), and the *real* root logger — the one every other `Logger`
inherits appenders from — never received the `ConsoleAppender`. Separately,
`org/apache/commons/logging/LogFactory.getLog(...)` (used by the
conventional Spring Boot `private final Log logger =
LogFactory.getLog(getClass());` pattern) was independently stubbed to a
fake `Log` whose `info`/`warn`/`error` routed through
`ctx.record_printed_line` with a fake `"[ACL] "` prefix (bypassing
`System.out`/`System.err` entirely) and whose `debug`/`trace` were pure
no-ops. Both bypassed Spring Boot's `OutputCaptureExtension`/`CapturedOutput`
(which hooks `System.out`/`System.err`) regardless of the extension's own
push/pop bookkeeping — the doc's original "System.out redirection" and
"ConsoleAppender caches a stale System.out" hypotheses were both **ruled
out** by a standalone repro replicating Spring's exact `PrintStreamCapture`/
`OutputStreamCapture` nesting: Logback's `ConsoleAppender` does **not**
cache `System.out` at all — `ch.qos.logback.core.joran.spi.ConsoleTarget$1`
does a fresh `getstatic System.out` + `invokevirtual write(...)` on
**every single write call**, so it always reaches whatever `System.out`
currently is. The actual defect was upstream of that: the `Logger`/`Log`
objects themselves never delivered writes anywhere real.

Fix (`native-builtins/src/lib.rs`): removed both native overrides —
`LoggerContext.getLogger(String)`/`getLogger(Class)`, every
`ch/qos/logback/classic/Logger` instance method, and both duplicate
registrations of `LogFactory.getLog`/`Log.*` — so real bytecode runs for
all of them, exactly as `LoggerContext.<init>` already did. Real
commons-logging's own SLF4J-bridge discovery now correctly hands back a
real, Logback-backed `Log`.

**Regression check:** the original rationale for stubbing commons-logging
was `SpringApplicationShutdownHook`'s static `Log logger =
LogFactory.getLog(...)`, said to NPE inside real discovery — re-verified
via `BannerTests` (6/6 pass, exercises `SpringApplication.run()` — and
hence `SpringApplicationShutdownHook` class-init — six times in one
process) with no NPE.

**Residuals filed separately** (both newly exposed by this fix, not
pre-existing symptoms of the empty-capture defect):
- `oncondition-report-window-isolation-residual.md` —
  `OnFailureConditionReportContextCustomizerFactoryTests` now gets real,
  rich `CapturedOutput` content, but content from one test method bleeds
  into another's capture window (2 of 3 tests).
- `propertiesmigration-logfactory-oom-residual.md` —
  `PropertiesMigrationListenerTests.sampleReport` now reaches real
  `commons-logging`'s `LogFactoryImpl`, which OOMs inside
  `Hashtable.rehash` after tens of millions of `getLog` calls — ruled out
  both a generic `Hashtable` rehash bug and a `Class.getName()`/`String`
  identity bug via standalone repros; likely an unrelated runaway/proxy-
  generation loop that the old fake `Log` always short-circuited before
  reaching.

Final verification used binary
`cratonvm-captured-output-fix-20260717.exe` (worktree
`C:\craton\CratonVM-captured-output-cluster-20260717`, branch
`fix/captured-output-empty-cluster-20260717`) against 13 of this doc's
affected classes:

| Class | Result |
|---|---:|
| `ConditionEvaluationReportLoggerTests` | 6/6 PASS (was 1/6) |
| `ConditionEvaluationReportLoggingListenerTests` | PASS (was 2/5) |
| `ConditionEvaluationReportLoggingProcessorTests` | PASS (was 0/1) |
| `WebFluxObservationAutoConfigurationTests` | PASS |
| `DefaultErrorWebExceptionHandlerIntegrationTests` | PASS (was HANG at 180s, now completes ~148s) |
| `PropertiesMigrationListenerTests` | FAIL — new residual, see above |
| `H2ConsoleAutoConfigurationTests` | PASS (4/4) — the 2 CapturedOutput-specific tests were fixed by this change; the other 2, previously failing from the separately-tracked `jooq-destroy-method-ambiguity-and-hang-FIXED.md` bug, started passing after merging in `origin/dev`'s concurrent `Class.getMethods` override-shadowing fix (unrelated to this doc) |
| `HttpClientMetricsAutoConfigurationTests` | PASS |
| `OpenTelemetryEnvironmentVariableEnvironmentPostProcessorTests` | PASS |
| `OpenTelemetryEnvironmentVariablesTests` | PASS |
| `UserDetailsServiceAutoConfigurationTests` | PASS (was HANG at 180s, now completes ~130s) |
| `JerseyAutoConfigurationServletContainerTests` | PASS |
| `OnFailureConditionReportContextCustomizerFactoryTests` | FAIL — new residual, see above |

12 of 13 classes fully pass; the 2 remaining failures (`PropertiesMigrationListenerTests`,
`OnFailureConditionReportContextCustomizerFactoryTests`) are newly-exposed,
narrower, separately-tracked residuals rather than the original
empty-capture symptom.

This same root cause also explained the sibling docs' symptoms:
`docker-compose-lifecycle-capturedoutput-log-gap.md` (fully fixed, see
`docker-compose-lifecycle-capturedoutput-log-gap-FIXED.md`) and
`capturedoutput-empty-console-cluster.md` (15 of 22 classes fixed; a
`core/spring-boot`-concentrated residual remains, stays OPEN — see that
doc for a 2026-07-18 update reconciling this fix against a concurrent
session's different approach to the same symptom).

## Symptom

| Class | tests failed/total |
|---|---:|
| `ConditionEvaluationReportLoggerTests` | 5/6 |
| `ConditionEvaluationReportLoggingListenerTests` | 3/5 |
| `ConditionEvaluationReportLoggingProcessorTests` | 1/1 |

All 9 original failures shared the identical shape: the test injects
`CapturedOutput` (Spring Boot's `@ExtendWith(OutputCaptureExtension.class)`
stdout/stderr capture) and asserts it contains some substring the
condition evaluation report logger is supposed to have written — and the
captured output was the **empty string** instead:

```
=> java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "CONDITIONS EVALUATION REPORT"
       org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggerTests.loggerWithDebugLevelShouldLogAtDebug(ConditionEvaluationReportLoggerTests.java:86)
```

The doc grew to cover 20+ classes across 10+ modules over several rerun
sessions on 2026-07-17, all sharing this exact "capture mechanism is live
(sees the banner, an unrelated Mockito warning, etc.) but never the
specific Logback/commons-logging-routed line" shape. Full original
symptom detail, per-update triage history, and the full affected-class
list are preserved in git history for this file
(`docs/known-issues/springboot/conditionevaluationreport-capturedoutput-empty-cluster.md`
prior to this fix).

## Affected classes (original list, pre-fix)

| Module | Class |
|---|---|
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggerTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggingListenerTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggingProcessorTests` |
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.autoconfigure.WebFluxObservationAutoConfigurationTests` |
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.autoconfigure.error.DefaultErrorWebExceptionHandlerIntegrationTests` |
| `core/spring-boot-properties-migrator` | `org.springframework.boot.context.properties.migrator.PropertiesMigrationListenerTests` |
| `module/spring-boot-h2console` | `org.springframework.boot.h2console.autoconfigure.H2ConsoleAutoConfigurationTests` |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.autoconfigure.metrics.HttpClientMetricsAutoConfigurationTests` |
| `module/spring-boot-opentelemetry` | `org.springframework.boot.opentelemetry.autoconfigure.OpenTelemetryEnvironmentVariableEnvironmentPostProcessorTests` |
| `module/spring-boot-opentelemetry` | `org.springframework.boot.opentelemetry.autoconfigure.OpenTelemetryEnvironmentVariablesTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.UserDetailsServiceAutoConfigurationTests` |
| `module/spring-boot-jersey` | `org.springframework.boot.jersey.autoconfigure.JerseyAutoConfigurationServletContainerTests` |
| `core/spring-boot-test-autoconfigure` | `org.springframework.boot.test.autoconfigure.OnFailureConditionReportContextCustomizerFactoryTests` |
