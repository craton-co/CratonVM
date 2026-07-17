# `ConditionEvaluationReport*` logging tests: `CapturedOutput` is empty — condition-evaluation-report text never reaches the captured log stream

**Status: OPEN — found 2026-07-17**

**Update 2026-07-17 (bin14 rerun triage):** the identical symptom shape
(`CapturedOutput` asserted non-empty, actual `""`) also hits two
`module/spring-boot-webflux` classes unrelated to condition-evaluation
reporting — `WebFluxObservationAutoConfigurationTests` (asserting captured
log output contains an Observation "maximum number of 'uri' tags" warning)
and `DefaultErrorWebExceptionHandlerIntegrationTests` (asserting captured
log output contains a "500 Server Error" log line). Same mechanism
(`OutputCaptureExtension`/`CapturedOutput` sees nothing where HotSpot sees
the expected log line), different logger/subsystem being asserted on —
this generalizes hypothesis 1 below (a general `OutputCaptureExtension`
capture gap, not something specific to `ConditionEvaluationReportLogger`)
over hypothesis 2 (report-specific empty data), since these two new classes
have nothing to do with `ConditionEvaluationReport`. See the new rows in
"Affected classes" below; not independently re-investigated at the source
level this session either.

## Symptom

| Class | tests failed/total |
|---|---:|
| `ConditionEvaluationReportLoggerTests` | 5/6 |
| `ConditionEvaluationReportLoggingListenerTests` | 3/5 |
| `ConditionEvaluationReportLoggingProcessorTests` | 1/1 |

All 9 failures share the identical shape: the test injects
`CapturedOutput` (Spring Boot's `@ExtendWith(OutputCaptureExtension.class)`
stdout/stderr capture) and asserts it contains some substring the condition
evaluation report logger is supposed to have written — and the captured
output is the **empty string** instead:

```
=> java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "CONDITIONS EVALUATION REPORT"
       org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggerTests.loggerWithDebugLevelShouldLogAtDebug(ConditionEvaluationReportLoggerTests.java:86)
```

```
=> java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "Unable to provide the condition evaluation report"
       org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggerTests.noErrorIfNotInitialized(ConditionEvaluationReportLoggerTests.java:54)
```

```
=> java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "did not find any beans of type java.time.Duration (OnBeanCondition)"
       org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggerTests.logsOutput(ConditionEvaluationReportLoggerTests.java:112)
```

Every one of the 9 individual failures across the 3 classes is this same
shape — `actual: ""`, expected substring varies per test (different report
text, or different guidance message), which rules out "the report text
itself is wrong" (a wrong-but-nonempty string would show a real, differing
`actual`) in favor of "nothing at all reached the captured stream." One
`ConditionEvaluationReportLoggingProcessorTests` case shows the capture
picking up an *unrelated* line instead of nothing (Mockito's self-attach
warning), confirming the capture mechanism itself is live and does receive
some output — just never the condition-evaluation-report text specifically.

Full logs (relative to repo root):
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.logging.ConditionEvaluat-0f88d899ae58.out.log` (`ConditionEvaluationReportLoggerTests`)
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.logging.ConditionEvaluat-8dc52a8b2d5e.out.log` (`ConditionEvaluationReportLoggingListenerTests`)
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.logging.ConditionEvaluat-6dbc08deaf1f.out.log` (`ConditionEvaluationReportLoggingProcessorTests`)

`module/spring-boot-webflux` (added 2026-07-17, bin14 rerun):
```
JUnit Jupiter:WebFluxObservationAutoConfigurationTests:afterMaxUrisReachedFurtherUrisAreDeniedWhenUsingCustomObservationName(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "Reached the maximum number of 'uri' tags for 'my.http.server.requests'"
       org.springframework.boot.webflux.autoconfigure.WebFluxObservationAutoConfigurationTests.lambda$afterMaxUrisReachedFurtherUrisAreDeniedWhenUsingCustomObservationName$0(WebFluxObservationAutoConfigurationTests.java:81)

JUnit Jupiter:DefaultErrorWebExceptionHandlerIntegrationTests:jsonError(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "500 Server Error for HTTP GET "/""
       org.springframework.boot.webflux.autoconfigure.error.DefaultErrorWebExceptionHandlerIntegrationTests.lambda$jsonError$0(DefaultErrorWebExceptionHandlerIntegrationTests.java:123)
```
Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webflux.org.springframework.boot.webflux.autoconfigure.WebFluxObservationAu-582cdaac8bd8.out.log` (`WebFluxObservationAutoConfigurationTests`, 2 of 5 tests fail this way)
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webflux.org.springframework.boot.webflux.autoconfigure.error.DefaultErrorWe-46f162b6dcb4.out.log` (`DefaultErrorWebExceptionHandlerIntegrationTests`, 1 of 22 tests fails this way)

**Update 2026-07-17 (bin9 rerun triage):** the identical shape also hits
`module/spring-boot-h2console`'s `H2ConsoleAutoConfigurationTests`, 2 of its
4 failing test methods (the other 2 are the unrelated
`disposablebeanadapter`/`shutdown`-ambiguity bug, see
`jooq-destroy-method-ambiguity-and-hang.md`):

```
JUnit Jupiter:H2ConsoleAutoConfigurationTests:allDataSourceUrlsAreLoggedWhenMultipleAvailable(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual:
  "Mockito is currently self-attaching to enable the inline-mock-maker. ..."
to contain:
  "H2 console available at '/h2-console'. Databases available at 'someJdbcUrl', 'anotherJdbcUrl'"
       org.springframework.boot.h2console.autoconfigure.H2ConsoleAutoConfigurationTests.lambda$allDataSourceUrlsAreLoggedWhenMultipleAvailable$0(H2ConsoleAutoConfigurationTests.java:161)

JUnit Jupiter:H2ConsoleAutoConfigurationTests:allDataSourceUrlsAreLoggedWhenNonCandidate(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "H2 console available at '/h2-console'. Databases available at 'someJdbcUrl', 'anotherJdbcUrl'"
       org.springframework.boot.h2console.autoconfigure.H2ConsoleAutoConfigurationTests.lambda$allDataSourceUrlsAreLoggedWhenNonCandidate$0(H2ConsoleAutoConfigurationTests.java:172)
```

Both assert on `H2ConsoleAutoConfiguration$H2ConsoleLogger`'s log line
(different logger/subsystem again, same "nothing captured" mechanism). One
instance again shows the capture picking up the unrelated Mockito banner
rather than nothing — the same "capture mechanism is live, just never sees
this logger's output" pattern as the `ConditionEvaluationReportLoggingProcessorTests`
case above, reinforcing hypothesis 1 (general capture/log-routing gap) over
hypothesis 2 (empty report data — there's no analogous "report object" here,
just a plain `log.info(...)` call, yet the same symptom occurs).

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-h2console.org.springframework.boot.h2console.autoconfigure.H2ConsoleAutoCon-3c0cb3295e8f.out.log`

**Update 2026-07-17 (bin3 rerun triage):** a fourth module hits the identical
shape — `module/spring-boot-http-client`
`HttpClientMetricsAutoConfigurationTests.afterMaxUrisReachedFurtherUrisAreDenied(CapturedOutput)`:

```
=> java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "Reached the maximum number of 'uri' tags for 'http.client.requests'."
       org.springframework.boot.http.client.autoconfigure.metrics.HttpClientMetricsAutoConfigurationTests.lambda$afterMaxUrisReachedFurtherUrisAreDenied$0(HttpClientMetricsAutoConfigurationTests.java:55)
```

This is the exact same "max URI tags reached" warning-message shape as the
`WebFluxObservationAutoConfigurationTests` entry above (`http.client.requests`
vs. `my.http.server.requests` — same underlying Micrometer `Observation`
max-URI-tags guard, different meter name), further reinforcing hypothesis 1
(a general `OutputCaptureExtension`/logging-pipeline capture gap) over
hypothesis 2. Not independently re-investigated at the source level this
session either. This is the class's only test, so 1/1 fails this way. Full
log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-http-client.org.springframework.boot.http.client.autoconfigure.metrics.Http-b356cdd18e36.out.log`

**Update 2026-07-17 (bin5 rerun triage):** three more classes across three
more modules hit the same `OutputCaptureExtension`/`CapturedOutput`
mechanism, widening this from "empty" to "empty OR stale/wrong-window"
content — the capture isn't just failing to receive new output, it
sometimes returns content that belongs to a *different* test method
entirely:

- `module/spring-boot-security` | `UserDetailsServiceAutoConfigurationTests#testDefaultUsernamePassword` —
  actual `""`, expected to contain `"Using generated security password:"`.
  Same "nothing captured" shape as the original 3 classes.
- `module/spring-boot-jersey` | `JerseyAutoConfigurationServletContainerTests#existingJerseyServletIsAmended` —
  actual is the **Spring Boot startup banner** (ASCII art + version line),
  expected to contain `"Configuring existing registration for Jersey
  servlet"`. Not empty this time — the capture returns *something*, just
  not the specific log line asserted on, same as the H2Console/Mockito-banner
  cases above.
- `core/spring-boot-test-autoconfigure` | `OnFailureConditionReportContextCustomizerFactoryTests`
  (3/3 tests failed) — the clearest evidence yet that this is a
  **window-isolation** bug, not just a routing gap. All 3 tests assert on
  `CapturedOutput` around a *different* context-startup failure each time;
  the actual captured content in 2 of the 3 (`loadFailureShouldNotPrintReportWhenApplicationPropertiesIsBroken`,
  `loadFailureShouldNotPrintReportWhenDisabled`) is a **full banner +
  "CONDITIONS EVALUATION REPORT" / "TestAutoConfiguration matched" block**
  that the test explicitly asserts should NOT be present for that scenario
  (`not to contain: "TestAutoConfiguration matched"` — but it's there
  anyway), while the 3rd (`loadFailureShouldPrintReport`, which per file
  order likely runs first) sees only the banner and is missing the
  `"Error creating bean with name 'faultyBean'"` line it should contain.
  This is consistent with capture windows bleeding content across test
  methods within the same class (a later method's capture shows an earlier
  method's conditions-report output) rather than a uniform "this logger's
  output never reaches capture" gap — though see caveat below, this is not
  a strictly-growing single buffer either (the leading Mockito self-attach
  line that appears in test 1's capture does NOT also appear in test 3's,
  ruling out "one big buffer that only ever grows").
  Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-test-autoconfigure.org.springframework.boot.test.autoconfigure.OnFailureCondi-98fa9a272e77.out.log`

This third data point (wrong-but-nonempty, specifically *another test
method's* content) argues for a variant of hypothesis 1 below: not simply
"this logger's output never reaches the captured stream" (which wouldn't
explain a different method's banner/report showing up), but that
`OutputCaptureExtension`'s per-test-method capture window (a push/pop
stack around a swapped `System.out`/`System.err`) is not being scoped
correctly — a later method sometimes observes an earlier method's window
content instead of a correctly-isolated, currently-live one. One relevant,
confirmed-correct fact from source: CratonVM's `System.setOut`/`setErr`
override storage (`native-builtins/src/lib.rs`, `system_overridden_streams`,
~line 43617) is a single **process-wide** `OnceLock<Mutex<HashMap<&str,
ObjectRef>>>` keyed by `"out"`/`"err"`/`"in"` — this correctly matches real
JDK semantics (`System.out` is one global static field, not thread-local),
so it is not itself a bug, but it is exactly the kind of shared global
state that a missing/incorrect per-capture-instance push/pop discipline
(on the Java side, in Spring's own `OutputCapture`/`SystemCapture`, or in
however CratonVM's underlying `PrintStream`/`OutputStream` write path
buffers and flushes) could interact badly with. Not pinned to a specific
native function this session.

## Root cause (hypothesis, not confirmed against CratonVM source)

Not pinned to a specific file/line in this session. Two candidate mechanisms,
neither directly investigated here:

1. **Logging-framework wiring gap.** `ConditionEvaluationReportLogger`/
   `ConditionEvaluationReportLoggingListener` write via SLF4J/Logback at
   `DEBUG`/`INFO` level depending on the test. If CratonVM's real-JDK-mode
   Logback/SLF4J bridge silently drops or misroutes log records at these
   levels for this specific logger category (rather than a wholesale outage
   — other Spring Boot tests elsewhere in the suite do successfully assert
   on captured log output), the report text would never reach the
   `OutputCaptureExtension`'s captured `System.out`/`System.err`, matching
   the symptom exactly. This module's README already documents one
   previously-fixed, unrelated Logback field-corruption bug
   (`LoggerContext.loggerContextListenerList null cluster`, FIXED
   2026-07-15) in the same general area (SLF4J/Logback bridge on CratonVM),
   which raises the prior that this bridge is a recurring source of
   subtle logging-pipeline gaps, but that specific fix does not obviously
   cover this symptom and was not re-verified against these 3 classes here.
2. **`ConditionEvaluationReport` itself is empty at report-generation time**
   (e.g. `ConditionEvaluationReport.get(beanFactory)` returns a report with
   zero recorded conditions, because CratonVM's classpath/condition
   evaluation machinery for `@Conditional` never populates the report
   object in the first place) — which would make the *logger* correctly
   log nothing because there is nothing to log, rather than the log pipeline
   dropping real content. This is not distinguishable from hypothesis 1
   using only the JUnit-summary text available in these logs; it would
   require either a direct breakpoint/print inside
   `ConditionEvaluationReportLogger.logMessage` or a minimal standalone
   repro that constructs a `ConditionEvaluationReport` and logs it without
   going through `OutputCaptureExtension` at all.

Given the un-clustered "long tail" framing already used in this doc's
sibling entries, and that no source-level investigation was done this
session, this is filed as OPEN with the symptom precisely captured rather
than a confirmed root cause.

**Update 2026-07-17 (bin6 rerun triage):** a **variant** of this symptom
also hits `core/spring-boot-properties-migrator`'s
`PropertiesMigrationListenerTests.sampleReport(CapturedOutput)`:

```
=> java.lang.AssertionError:
Expecting actual:
  "
  .   ____          _            __ _ _
 /\\ / ___'_ __ _ _(_)_ __  __ _ \ \ \ \
( ( )\___ | '_ | '_| | '_ \/ _` | \ \ \ \
 \\/  ___)| |_)| | | | | || (_| |  ) ) ) )
  '  |____| .__|_| |_|_| |_\__, | / / / /
 =========|_|==============|___/=/_/_/_/

 :: Spring Boot ::       (v4.1.0-SNAPSHOT)

"
to contain:
  "commandLineArgs"
       org.springframework.boot.context.properties.migrator.PropertiesMigrationListenerTests.sampleReport(PropertiesMigrationListenerTests.java:52)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-properties-migrator.org.springframework.boot.context.properties.migrator.Prop-86f2d209b880.out.log`.

This is a **variant, not an exact match**, of the shape above: `actual` here
is **not the empty string** — the captured output has the Spring Boot
startup banner (proving `CapturedOutput` is live and receiving *some*
output, same as the `ConditionEvaluationReportLoggingProcessorTests`
Mockito-warning case already noted above) — but is still missing the
`PropertiesMigrationListener`'s own report body (which should contain a
`"commandLineArgs"` legacy-property-usage warning). Same general mechanism
class as hypothesis 1 above (a specific logger/report-writer's output
failing to reach the captured stream while other output around it does
reach it) applied to a third, unrelated subsystem
(`PropertiesMigrationListener`, an `ApplicationListener` that logs a
deprecation report via a plain `Log`, not `ConditionEvaluationReportLogger`
or an Observation warning) — further generalizing away from
report-specific/subsystem-specific explanations and toward a broader
logger-category or timing-related capture gap in
`OutputCaptureExtension`/CratonVM's stdout redirection. Not independently
root-caused at the source level this session either.

**Update 2026-07-17 (bin2 rerun triage):** a fifth module hits an *adjacent*
shape — `module/spring-boot-opentelemetry`'s `OpenTelemetryEnvironmentVariableEnvironmentPostProcessorTests`
(15 of 80 tests) and `OpenTelemetryEnvironmentVariablesTests` (6 of 56
tests):

```
JUnit Jupiter:OpenTelemetryEnvironmentVariableEnvironmentPostProcessorTests:shouldMapOtelTracesSampler(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "Invalid value for environment variable 'OTEL_TRACES_SAMPLER': 'invalid'"
       org.springframework.boot.opentelemetry.autoconfigure.OpenTelemetryEnvironmentVariableEnvironmentPostProcessorTests.shouldMapOtelTracesSampler(OpenTelemetryEnvironmentVariableEnvironmentPostProcessorTests.java:133)

JUnit Jupiter:OpenTelemetryEnvironmentVariablesTests:getIntShouldReturnNoneAndWarnWhenNotANumber(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "Invalid value for integer environment variable 'VAR': 'abc'"
       org.springframework.boot.opentelemetry.autoconfigure.OpenTelemetryEnvironmentVariablesTests.assertThatLogContains(OpenTelemetryEnvironmentVariablesTests.java:523)
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-opentelemetry.org.springframework.boot.opentelemetry.autoconfigure.OpenTele-a6762ceeaae9.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-opentelemetry.org.springframework.boot.opentelemetry.autoconfigure.OpenTele-2adb81d14abb.out.log`

Same "actual: empty string" shape, but a mechanically **different logging
path** than every prior entry in this doc: `OpenTelemetryEnvironmentVariables`
(`apps/spring-boot/module/spring-boot-opentelemetry/src/main/java/org/springframework/boot/opentelemetry/autoconfigure/OpenTelemetryEnvironmentVariables.java:40,46`)
logs via a plain `org.apache.commons.logging.Log` obtained from a Spring
Boot `DeferredLogFactory` (`this.logger = deferredLogFactory.getLog(...)`),
the mechanism `EnvironmentPostProcessor`s use to log *before* the real
logging system is initialized — messages queue in a `DeferredLog` and are
replayed onto the real logger once available. This predates any Logback
`ConsoleAppender` even existing, so it cannot share hypothesis 1 above
(`ConsoleAppender`'s cached pre-swap `System.out`) verbatim, though the
*eventual* replay presumably still funnels through the same SLF4J/Logback
bridge this doc's other entries implicate — filed here as a **likely, not
confirmed**, same-cluster instance rather than a fully independent bug, per
this doc's existing pattern of generalizing across subsystems that all
exhibit "capture mechanism live, this one logger's output never reaches
it." Not independently root-caused at the source level this session.

Not all tests in either class fail this way — 65/80 and 50/56 pass
respectively — consistent with this doc's established "capture mechanism
isn't unconditionally broken" observation.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggerTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggingListenerTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.logging.ConditionEvaluationReportLoggingProcessorTests` |
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.autoconfigure.WebFluxObservationAutoConfigurationTests` |
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.autoconfigure.error.DefaultErrorWebExceptionHandlerIntegrationTests` |
| `core/spring-boot-properties-migrator` | `org.springframework.boot.context.properties.migrator.PropertiesMigrationListenerTests` (1 of 1 failing test, variant shape — see update above) |
| `module/spring-boot-h2console` | `org.springframework.boot.h2console.autoconfigure.H2ConsoleAutoConfigurationTests` (2 of its 4 failing test methods only) |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.autoconfigure.metrics.HttpClientMetricsAutoConfigurationTests` |
| `module/spring-boot-opentelemetry` | `org.springframework.boot.opentelemetry.autoconfigure.OpenTelemetryEnvironmentVariableEnvironmentPostProcessorTests` (15 of 80 tests, likely-same-cluster, different logging path — see update above) |
| `module/spring-boot-opentelemetry` | `org.springframework.boot.opentelemetry.autoconfigure.OpenTelemetryEnvironmentVariablesTests` (6 of 56 tests, likely-same-cluster, different logging path — see update above) |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.UserDetailsServiceAutoConfigurationTests` (1 of 22 tests, added bin5) |
| `module/spring-boot-jersey` | `org.springframework.boot.jersey.autoconfigure.JerseyAutoConfigurationServletContainerTests` (1 of 1 test, added bin5) |
| `core/spring-boot-test-autoconfigure` | `org.springframework.boot.test.autoconfigure.OnFailureConditionReportContextCustomizerFactoryTests` (3 of 3 tests, added bin5 — the stale-window evidence) |
