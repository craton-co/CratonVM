# `CapturedOutput`/`OutputCaptureExtension` sees empty console output (cross-module FAIL cluster)

**Status: OPEN (majority FIXED 2026-07-18) — found 2026-07-17**

## Update 2026-07-18 — root cause fixed, most of this doc's classes now pass

The root cause (not the "ConsoleAppender caches a stale `System.out`"
hypothesis below, which was directly disproven by a standalone repro) was
that `ch/qos/logback/classic/Logger`/`LoggerContext.getLogger` and
`org/apache/commons/logging/LogFactory`/`Log` were natively overridden to
hand back throwaway synthetic objects whose `addAppender`/`info`/`warn`/
`error` never reached `System.out`/`System.err` at all — see
`docs/internal/springboot/conditionevaluationreport-capturedoutput-empty-cluster-FIXED.md`
for the full resolution writeup (filed against the sibling doc that first
found this cluster). Fixed in `native-builtins/src/lib.rs` by removing
those overrides so real Logback/commons-logging bytecode runs.

Re-verified 2026-07-18 with binary `cratonvm-captured-output-fix-20260717.exe`
(worktree `C:\craton\CratonVM-captured-output-cluster-20260717`, branch
`fix/captured-output-empty-cluster-20260717`) against every class this
doc lists:

**Now PASS** (15 classes) — `RemoteClientConfigurationTests`,
`RestartApplicationListenerTests`,
`ServletManagementContextAutoConfigurationIntegrationTests`,
`WebMvcObservationAutoConfigurationTests`, `WelcomePageHandlerMappingTests`,
`ErrorMvcAutoConfigurationTests`, `GraphQlAutoConfigurationTests`,
`EndpointIdTests`, `FreeMarkerAutoConfigurationTests`, `HealthEndpointTests`,
`ReactiveHealthIndicatorImplementationTests`, `AbstractHealthIndicatorTests`,
`AbstractReactiveHealthIndicatorTests`,
`LoggingApplicationListenerIntegrationTests`.

**Still FAIL** (7 classes, all in `core/spring-boot` specifically) — with
several genuinely different shapes, not re-investigated at the source
level this session (out of scope for the fix above, which targeted a
different doc):
- `SimpleMainTests`, `ConfigurationWarningsApplicationContextInitializerTests` —
  still empty/wrong-content (`ConfigurationWarningsApplicationContextInitializerTests`:
  actual `""`; `SimpleMainTests`: banner present but not the
  `"Started SpringApplication in"` line) — same general family as this
  doc's original symptom, but not yet root-caused to a specific remaining
  native override.
- `ConfigurationPropertiesTests`, `FailureAnalyzersIntegrationTests` — FAIL,
  not yet triaged this session (only the class-level FAIL/PASS status was
  captured, not the individual assertion diffs).
- `DefaultSslBundleRegistryTests` — actual is only the Mockito self-attach
  banner, still missing `"SSL bundle 'test1' has been updated..."` — same
  "capture live, this logger's line specifically missing" shape as the
  original doc, unclear if a residual of the same family or a distinct
  logger-specific gap.
- `ErrorPageFilterTests` — a **new, different** failure: actual now
  contains a real `ArrayIndexOutOfBoundsException` thrown *by* Log4j2's
  own `Appender STDOUT` machinery (`java.lang.String.checkOffset` /
  `AbstractStringBuilder.insert`) instead of the expected error-page log
  line — this is Log4j2-specific (this class apparently exercises the
  Log4j2 backend, not Logback) and looks like a distinct, newly-exposed
  Log4j2 appender bug, not the commons-logging/Logback issue this doc (or
  its sibling) was about. Worth its own doc if reproduced again.
- `GraylogExtendedLogFormatStructuredLogFormatterTests` (log4j2 and
  logback variants) — still FAIL, not triaged this session.

Given a substantial, multi-shaped residual remains concentrated in
`core/spring-boot`, this doc stays OPEN rather than archiving — the 15
now-passing classes are removed from the "Affected classes" table below;
the 7 `core/spring-boot` classes remain listed pending further triage.

---

**Status (original, 2026-07-17): OPEN**

## Symptom

3 classes across 2 modules fail identically: a test injects JUnit5's
`org.springframework.boot.test.system.CapturedOutput` parameter (backed by
`OutputCaptureExtension`, which is supposed to capture everything written
to `System.out`/`System.err` — including log output — during the test) and
then asserts that a specific log/console message was printed. In every
failing case, the captured output is the **empty string**, even though the
code under test unquestionably logs the expected message on a working JVM
(these classes pass on the same-scope real-HotSpot baseline).

| Module | Class | Method | Expected substring |
|---|---|---|---|
| `module/spring-boot-devtools` | `RemoteClientConfigurationTests` | `warnIfRestartDisabled(CapturedOutput)` | `"Remote restart is disabled"` |
| `module/spring-boot-devtools` | `RemoteClientConfigurationTests` | `warnIfNotHttps(CapturedOutput)` | `"is insecure"` |
| `module/spring-boot-devtools` | `RestartApplicationListenerTests` | `enableWithSystemPropertyWhenImplicitlyDisabled(CapturedOutput)` | `"Restart enabled irrespective of application packaging..."` |
| `module/spring-boot-devtools` | `RestartApplicationListenerTests` | `enableWithSystemProperty(CapturedOutput)` | `"Restart enabled irrespective of application packaging..."` |
| `module/spring-boot-devtools` | `RestartApplicationListenerTests` | `disableWithSystemProperty(CapturedOutput)` | `"Restart disabled due to System property"` |
| `module/spring-boot-devtools` | `RestartApplicationListenerTests` | `implicitlyDisabledInTests(CapturedOutput)` | `"Restart disabled due to context in which it is running"` |
| `module/spring-boot-servlet` | `ServletManagementContextAutoConfigurationIntegrationTests` | `childManagementContextShouldStartForEmbeddedServer(CapturedOutput)` | `"Tomcat started on port"` (x2) |
| `module/spring-boot-servlet` | `ServletManagementContextAutoConfigurationIntegrationTests` | `childManagementContextShouldRestartWhenParentIsStoppedThenStarted(CapturedOutput)` | `"Tomcat started on port"` (x2, then x4) |

Representative failure (`RemoteClientConfigurationTests`):

```
JUnit Jupiter:RemoteClientConfigurationTests:warnIfRestartDisabled(CapturedOutput)
  => java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "Remote restart is disabled"
     org.springframework.boot.devtools.remote.client.RemoteClientConfigurationTests.warnIfRestartDisabled(RemoteClientConfigurationTests.java:85)
```

`ServletManagementContextAutoConfigurationIntegrationTests` fails the same
way but through an AssertJ soft-assertion group
(`org.assertj.core.error.AssertJMultipleFailuresError`) wrapping
`assertThat(output).satisfies(numberOfOccurrences("Tomcat started on port", 2))`
at `ServletManagementContextAutoConfigurationIntegrationTests.java:69`/`96` —
same missing-capture shape, just asserted via a custom `Consumer` instead of
a direct `contains(...)`.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-devtools.org.springframework.boot.devtools.remote.client.RemoteClientConfigurationTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-devtools.org.springframework.boot.devtools.restart.RestartApplicationListenerTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-servlet.org.springframework.boot.servlet.autoconfigure.actuate.web.ServletM-74666079edd6.out.log`

Not all `CapturedOutput`-using tests in these classes fail — e.g. the other
5 tests in `RemoteClientConfigurationTests`/`RestartApplicationListenerTests`
pass, and 2/4 in `ServletManagementContextAutoConfigurationIntegrationTests`
pass — so the capture mechanism isn't unconditionally broken; it fails on a
subset whose common thread is asserting on output produced by *real
logging* (SLF4J/Logback `INFO`/`WARN` records reaching the console via
`ConsoleAppender`) rather than direct `System.out.println` calls.

## Root cause

**Not confirmed — hypothesis.** `OutputCaptureExtension` (Spring Boot test
support, real bytecode) installs a capturing wrapper by calling
`System.setOut`/`System.setErr` with a substitute `PrintStream` before each
test, and restores the original afterward. That only captures writes that
go through a **freshly-read** `System.out`/`System.err` reference at the
time of the call. Logback's `ConsoleAppender`/`OutputStreamAppender`
resolves its target stream (`System.out`) once when the appender
`start()`s, and caches it in the appender's own field rather than reading
`System.out` fresh on every `append()` call — so if the appender was
already started (and cached the pre-swap `System.out`) before
`OutputCaptureExtension` substitutes the stream for this particular test,
every subsequent log record through that appender bypasses the captured
stream entirely, silently landing on the real console instead. This
produces exactly the observed shape: `CapturedOutput` sees `""` regardless
of how many matching log lines were actually emitted.

This project has an already-fixed cluster in the same area —
`docs/internal/springboot/logback-loggercontext-listenerlist-final-field-corruption-FIXED.md`
— about the shared `LoggerContext`/appender singleton not being correctly
reconstructed across the many `SpringApplication.run()` calls each test
class makes in one process (CratonVM previously registered
`LoggerContext.<init>()` as a no-op in one native-registration path). That
fix targeted a `final` field going null; it did not address (and the fix
doc does not claim to address) whether the `ConsoleAppender`'s *cached
`System.out` reference* is correctly refreshed to the current
(test-substituted) stream on each of those resets. Given both symptoms
live in the same "Logback singleton survives across many
`SpringApplication.run()` calls in one CratonVM process" area, this is a
plausible residual of the same family, but it has **not** been verified
against the appender re-initialization code path — would need a live repro
instrumenting `ConsoleAppender.start()`/`setOutputStream` to confirm which
`System.out` instance it ends up holding relative to
`OutputCaptureExtension`'s swap.

## Update 2026-07-17 (bin8 rerun triage) — 4 more classes across 2 more modules (`spring-boot-webmvc`, `spring-boot-graphql`)

Same exact shape (`CapturedOutput` asserted non-empty, actual `""`, capture
mechanism otherwise live) hits 4 more classes:

```
JUnit Jupiter:WebMvcObservationAutoConfigurationTests:afterMaxUrisReachedFurtherUrisAreDeniedWhenUsingCustomObservationName(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "Reached the maximum number of 'uri' tags for 'my.http.server.requests'"

JUnit Jupiter:WebMvcObservationAutoConfigurationTests:afterMaxUrisReachedFurtherUrisAreDenied(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual: ""
to contain: "Reached the maximum number of 'uri' tags for 'http.server.requests'"

JUnit Jupiter:WelcomePageHandlerMappingTests:logsInvalidAcceptHeader(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual: ""
to contain: "Received invalid Accept header. Assuming all media types are accepted"

JUnit Jupiter:ErrorMvcAutoConfigurationTests:renderWhenAlreadyCommittedLogsMessage(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual: ""
to contain: "Cannot render error page for request [/path] and exception [Exception message] as the response has already been committed. As a result, the response may have the wrong status code."

JUnit Jupiter:GraphQlAutoConfigurationTests:schemaInspectionShouldBeEnabledByDefault(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual: ""
to contain: "GraphQL schema inspection"
```

`WebMvcObservationAutoConfigurationTests` is the exact Servlet-side mirror of
the already-noted `WebFluxObservationAutoConfigurationTests` case in
[`conditionevaluationreport-capturedoutput-empty-cluster.md`](conditionevaluationreport-capturedoutput-empty-cluster.md)'s
bin14 update (same Observation "maximum number of 'uri' tags" warning,
same-shaped assertion, WebMvc instead of WebFlux) — reinforces that doc's
"general `OutputCaptureExtension` capture gap, not subsystem-specific"
conclusion rather than adding a new hypothesis. `WelcomePageHandlerMappingTests`,
`ErrorMvcAutoConfigurationTests`, and `GraphQlAutoConfigurationTests` each add
a 3rd/4th/5th distinct logger/subsystem (`WelcomePageHandlerMapping`,
`BasicErrorController`, GraphQL schema inspection) to the growing list of
loggers this affects, further weakening any per-subsystem explanation.

None of the 4 classes' failing tests are the only tests in their class — each
class has other, passing tests (including other `CapturedOutput` tests in
`WebMvcObservationAutoConfigurationTests` itself, e.g. its non-"afterMaxUris"
tests) — consistent with this cluster's existing observation that the
capture mechanism is not unconditionally broken, only for output produced via
real SLF4J/Logback logging reaching the console appender.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.WebMvcObservationAutoC-2d1454761abd.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.WelcomePageHandlerMappingTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.error.ErrorMvcAutoConf-a395938dae7e.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-graphql.org.springframework.boot.graphql.autoconfigure.GraphQlAutoConfigurationTests.out.log`

No new evidence on the appender-caching hypothesis above (not independently
verified against the `ConsoleAppender`/`OutputStreamAppender` source this
session either) — this update only broadens the affected-class list.

## Update 2026-07-17 (bin11 rerun triage) — 2 more classes across 2 more modules (`spring-boot-actuator`, `spring-boot-freemarker`)

Same exact shape (`CapturedOutput` asserted non-empty, actual `""`,
underlying log call guarded by `Log.isWarnEnabled()` and issued via Apache
Commons Logging `LogFactory.getLog(...)`, same as this cluster's other
members) hits 2 more classes:

```
JUnit Jupiter:EndpointIdTests:ofWhenContainsDeprecatedCharsLogsWarning(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual: ""
to contain: "Endpoint ID 'foo-bar' contains invalid characters, please migrate to a valid format"

JUnit Jupiter:FreeMarkerAutoConfigurationTests:nonExistentTemplateLocation(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual: ""
to contain: "Cannot find template location"
```

Both confirmed via source to follow the identical
`Log logger = LogFactory.getLog(...)` / `if (logger.isWarnEnabled()) {
logger.warn(...) }` pattern already implicated in this doc's root-cause
hypothesis:
`org.springframework.boot.actuate.endpoint.EndpointId` (`EndpointId.java:40,42,149-155`,
dedup-tracked via a `loggedWarnings` `Set` that the test resets first via
`EndpointId.resetLoggedWarnings()`, ruling out "already logged, deduped"
as the explanation) and
`org.springframework.boot.freemarker.autoconfigure.FreeMarkerAutoConfiguration`
(`FreeMarkerAutoConfiguration.java:49,62-68`). Neither class uses
`@ClassPathExclusions`/classpath modification, both are plain
`ApplicationContextRunner`-style autoconfiguration tests — consistent with
this cluster's existing "real SLF4J/Logback logging reaching the console
appender" common thread, not a `System.out.println`-direct case.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-actuator.org.springframework.boot.actuate.endpoint.EndpointIdTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-freemarker.org.springframework.boot.freemarker.autoconfigure.FreeMarkerAuto-f93cf57cfed1.out.log`

## Update 2026-07-17 (large `core/spring-boot` batch triage, 73-class rerun doc pass)

9 more classes, all in `core/spring-boot` itself (the module this doc's
existing entries were not from), hit the identical
"`CapturedOutput` sees less than HotSpot does" shape:

| Class | Method(s) | Expected substring | Actual |
|---|---|---|---|
| `SimpleMainTests` | `mixedContext`, `configClassContext`, `xmlContext` (all take `CapturedOutput`) | `"Started SpringApplication in"` | banner only, nothing after it |
| `ConfigurationWarningsApplicationContextInitializerTests` | 4 methods, e.g. `logWarningInOrgSpringPackage` | `"Your ApplicationContext is unlikely to start due to a @ComponentScan of ..."` | `""` (fully empty) |
| `LoggingApplicationListenerIntegrationTests` | `loggingPerformedDuringChildApplicationStartIsNotLost` | `"Child application starting"` | banner only |
| `ConfigurationPropertiesTests` | `loadWhenHasMultiplePropertySourcesPlaceholderConfigurerShouldLogWarning` | `"Multiple PropertySourcesPlaceholderConfigurer beans registered"` | `""` |
| `FailureAnalyzersIntegrationTests` | `analysisIsPerformed` | `"APPLICATION FAILED TO START"` | banner only |
| `DefaultSslBundleRegistryTests` | `shouldLogIfUpdatingBundleWithoutListeners` | `"SSL bundle 'test1' has been updated but may be in use..."` | only the Mockito self-attach warning |
| `ErrorPageFilterTests` | `errorMessageForRequestWithoutPathInfo`, `errorMessageForRequestWithPathInfo` | `"request [/test]"` / `"request [/test/alpha]"` | `""` (fully empty) |
| `log4j2.GraylogExtendedLogFormatStructuredLogFormatterTests` | `shouldNotAllowInvalidFieldNames` | `"'/' is not a valid field name according to GELF standard"` | only the Mockito self-attach warning |
| `logback.GraylogExtendedLogFormatStructuredLogFormatterTests` | `shouldNotAllowInvalidFieldNames`, `shouldNotAllowIllegalFieldNames` | GELF field-name validation messages | only the Mockito self-attach warning |

Same exact fingerprint as this doc's existing entries: capture is
demonstrably *live* (it does see the banner, or the unrelated Mockito
self-attach warning, in several of these) but never sees the specific
`Log`/SLF4J-routed message the test asserts on. Full logs (all
`craton-rerun-20260717/shard1/logs/core_spring-boot.<class>.out.log`):
`org.springframework.boot.SimpleMainTests`,
`org.springframework.boot.context.ConfigurationWarningsApplicationContextInitializerTests`,
`org.springframework.boot.context.logging.LoggingApplicationListenerIntegrationTests`,
`org.springframework.boot.context.properties.ConfigurationPropertiesTests`,
`org.springframework.boot.diagnostics.FailureAnalyzersIntegrationTests`,
`org.springframework.boot.ssl.DefaultSslBundleRegistryTests`,
`org.springframework.boot.web.servlet.support.ErrorPageFilterTests`,
`org.springframework.boot.logging.log4j2.GraylogExtendedLogFormatStructuredLog-443b0c01bd64.out.log`,
`org.springframework.boot.logging.logback.GraylogExtendedLogFormatStructuredLo-c6ae8f917c43.out.log`.

### A third, more concrete candidate mechanism

Reading `native-builtins/src/lib.rs`'s `println`/`print` native fast paths
this session turned up a **third hypothesis**, more concrete than the two
above, worth checking before further speculation:

`stream_fd` (`native-builtins/src/lib.rs:43976`) is used by every
`native_println_*`/`native_print_*` native (e.g. `native_println_string`,
which backs `PrintStream`/`PrintWriter`'s `println(String)`) to decide
whether the stream object being written to is (or wraps) the *current*
`System.out`/`System.err`:

```rust
fn stream_fd(ctx: &dyn NativeContext, args: &[Value]) -> Option<u32> {
    let Some(Value::Object(Some(stream))) = args.first() else { return None; };
    let out = ctx.get_system_stream("out");
    let err = ctx.get_system_stream("err");
    let matches_fd = |obj: &ObjectRef| -> Option<u32> {
        if let Some(o) = &out { if std::ptr::eq(obj.as_ptr(), o.as_ptr()) { return Some(1); } }
        if let Some(e) = &err { if std::ptr::eq(obj.as_ptr(), e.as_ptr()) { return Some(2); } }
        None
    };
    ...
}
```

and `stream_writeln`/`stream_write` (`lib.rs:44123-44149`) use a hit from
`stream_fd` to write **straight to the raw OS file descriptor**
(`ctx.fd_table().write_string(fd, text)`), bypassing the Java-level
`PrintStream`/`OutputStream` object's own `write()` method entirely — this
is a legitimate fast path for the pristine, un-redirected `System.out`, but
`ctx.get_system_stream("out")` returns *whatever `System.out` currently is*
— including a test-installed capturing stream after
`OutputCaptureExtension`/Spring's `OutputCapture.SystemCapture` calls
`System.setOut(this.out)`. If any code later prints **directly onto the
current `System.out` object** (rather than onto some other wrapper that
chains down to it via field 0, which `stream_fd` also walks up to 4 levels
deep), this fast path will identity-match it, take the raw-fd shortcut, and
skip the substitute stream's actual `write(byte[],int,int)` override (in
Spring's case, `OutputCapture$OutputStreamCapture.write`, which is what
appends into `capturedStrings`) — text still reaches the real console (a
human watching the run would see it fine, matching this doc's own note that
`thread.printed_lines`-style consumers keep working), but `CapturedOutput`
never receives it. This does not by itself explain why the **banner**
(printed via `System.out.println` too, at a point where `System.out` is
already the substitute stream) shows up in `actual` while later
Logback/Log4j2-routed lines don't — that asymmetry still needs a live
repro to nail down (it may point back to hypothesis 1 above: Logback/Log4j2
writing multi-line `byte[]` blocks through a *different*, non-`println`
call path than the banner's, or holding a stale target reference so it
never reaches this fast path's identity check at all). Filed as a third,
unconfirmed candidate — not verified against a live repro this session.

## Update 2026-07-17 (bin10 rerun triage) — 4 more classes in `module/spring-boot-health`

Same exact shape hits 4 more classes, all in `module/spring-boot-health`:
`HealthEndpointTests` (1/27, `healthWhenIndicatorIsSlow`, expects
`"Health contributor"`), `ReactiveHealthIndicatorImplementationTests` (2/3,
expects `"Health check failed with RuntimeException"`/`"Health check failed
for custom"`), `AbstractHealthIndicatorTests` (5/6, expects
`"Test message"`/`"Health check failed"`), and
`AbstractReactiveHealthIndicatorTests` (5/6, same shape as the previous,
reactive variant). All log via `org.apache.commons.logging.Log`
(`LogFactory.getLog`), which this module's classpath resolves to real
Logback 1.5.32 (confirmed present via `cratonvm-test-cp.txt`; no
`logback-test.xml` override in this module, so Logback's default
auto-configured `ConsoleAppender` is in play) — consistent with this
cluster's "real SLF4J/Logback logging reaching the console appender, not
direct `System.out.println`" common thread. In `AbstractHealthIndicatorTests`,
the one test that asserts `output.doesNotContain(...)` (rather than
`.contains(...)`) passes trivially, same tell as the rest of this cluster.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-health.org.springframework.boot.health.actuate.endpoint.HealthEndpointTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-health.org.springframework.boot.health.actuate.endpoint.ReactiveHealthIndic-0cffd2310466.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-health.org.springframework.boot.health.contributor.AbstractHealthIndicatorTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-health.org.springframework.boot.health.contributor.AbstractReactiveHealthIndicatorTests.out.log`

No new evidence on any of the 3 candidate mechanisms above — this update
only broadens the affected-class list.

## Affected classes

**FIXED 2026-07-18** (removed from this table — see the update at the top):
`module/spring-boot-health`'s `HealthEndpointTests`,
`ReactiveHealthIndicatorImplementationTests`, `AbstractHealthIndicatorTests`,
`AbstractReactiveHealthIndicatorTests`; `module/spring-boot-devtools`'s
`RemoteClientConfigurationTests`, `RestartApplicationListenerTests`;
`module/spring-boot-servlet`'s
`ServletManagementContextAutoConfigurationIntegrationTests`;
`module/spring-boot-webmvc`'s `WebMvcObservationAutoConfigurationTests`,
`WelcomePageHandlerMappingTests`, `ErrorMvcAutoConfigurationTests`;
`module/spring-boot-graphql`'s `GraphQlAutoConfigurationTests`;
`module/spring-boot-actuator`'s `EndpointIdTests`;
`module/spring-boot-freemarker`'s `FreeMarkerAutoConfigurationTests`;
`core/spring-boot`'s `LoggingApplicationListenerIntegrationTests`.

Still OPEN (residual — see the 2026-07-18 update above for per-class
detail):

| Module | Class |
|---|---|
| `core/spring-boot` | `org.springframework.boot.SimpleMainTests` (3 of 4 failing tests — the 4th, `basePackageScan`, is a different bug, see `core-spring-boot-configdata-resource-resolution-empty-cluster.md`) |
| `core/spring-boot` | `org.springframework.boot.context.ConfigurationWarningsApplicationContextInitializerTests` (4 tests) |
| `core/spring-boot` | `org.springframework.boot.context.properties.ConfigurationPropertiesTests` (1 of 114 tests) |
| `core/spring-boot` | `org.springframework.boot.diagnostics.FailureAnalyzersIntegrationTests` |
| `core/spring-boot` | `org.springframework.boot.ssl.DefaultSslBundleRegistryTests` |
| `core/spring-boot` | `org.springframework.boot.web.servlet.support.ErrorPageFilterTests` (2 of 26 tests — now a Log4j2 `ArrayIndexOutOfBoundsException` inside `Appender STDOUT`, a different/new shape, see 2026-07-18 update) |
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.GraylogExtendedLogFormatStructuredLogFormatterTests` (1 of its failing tests) |
| `core/spring-boot` | `org.springframework.boot.logging.logback.GraylogExtendedLogFormatStructuredLogFormatterTests` (2 of its failing tests) |
