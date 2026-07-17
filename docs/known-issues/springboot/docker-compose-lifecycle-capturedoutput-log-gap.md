# `DockerComposeLifecycleManagerTests` — `CapturedOutput` never sees this class's `commons-logging` output

**Status: OPEN — found 2026-07-17 (hypothesis, not confirmed against native logging source)**

## Symptom

| Class | Failures |
|---|---:|
| `org.springframework.boot.docker.compose.lifecycle.DockerComposeLifecycleManagerTests` | 4/31 |

All 4 failing tests use `@ExtendWith`'d `CapturedOutput` (Spring Boot's
`OutputCaptureExtension`) and assert that a specific log line the
production code emits via `logger.info(...)`/`logger.warn(...)` shows up in
the captured `System.out`/`System.err` text. In every failure, the captured
output is simply empty:

```
JUnit Jupiter:DockerComposeLifecycleManagerTests:shouldLogIfServicesAreAlreadyRunning(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual:
  ""
to contain:
  "There are already Docker Compose services running, skipping startup"
       org.springframework.boot.docker.compose.lifecycle.DockerComposeLifecycleManagerTests.shouldLogIfServicesAreAlreadyRunning(DockerComposeLifecycleManagerTests.java:396)

JUnit Jupiter:DockerComposeLifecycleManagerTests:whenStartFailsLogsAreRetrieved(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual: "" to contain: "docker compose up failed with the following logs:"

JUnit Jupiter:DockerComposeLifecycleManagerTests:whenLogsAreUnavailableFailureIsHandled(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual: "" to contain: "docker compose up failed and its logs were unavailable"

JUnit Jupiter:DockerComposeLifecycleManagerTests:whenStartUsesStartAndItFailsLogsAreRetrieved(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual: "" to contain: "docker compose start failed with the following logs:"
```

The other 27 tests in the same class (including several that also use
`CapturedOutput` for different assertions — not exhaustively checked) pass.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-docker-compose.org.springframework.boot.docker.compose.lifecycle.DockerCompos-c6c2f40c8403.out.log`

## Root cause (hypothesis — not confirmed against native logging source this session)

All 4 failing messages are emitted from `DockerComposeLifecycleManager`
(`apps/spring-boot/core/spring-boot-docker-compose/src/main/java/.../DockerComposeLifecycleManager.java`)
via `private static final org.apache.commons.logging.Log logger = LogFactory.getLog(...)`
(`logger.info(skip.getLogMessage())` at line 129;
`logDockerComposeLogs(...)` used by the other 3 failing tests logs through
the same field). Commons Logging routes through the SLF4J bridge to Logback
in this test environment, and Logback's `ConsoleAppender` is expected to
write to `System.out`, which `CapturedOutput`/`OutputCaptureExtension`
substitutes for the duration of the test.

This repo has an already-fixed sibling bug in the same general area —
[`../../internal/springboot/logback-loggercontext-listenerlist-final-field-corruption-FIXED.md`](../../internal/springboot/logback-loggercontext-listenerlist-final-field-corruption-FIXED.md)
— where an SLF4J-bridge `LoggerContext` mirror bypassed Logback's real
constructor, breaking listener wiring; that fix was verified via
`BannerTests` (`System.out`-level Spring Boot banner output). This
class's specific 4-test failure could be either: (a) a narrower residual
of that same family not covered by the `BannerTests` verification (e.g. a
`Log`-vs-`Logger` commons-logging bridging path the banner test doesn't
exercise), or (b) something specific to how/when `DockerComposeLifecycleManager`'s
static `logger` field is initialized relative to `CapturedOutput`'s
`System.out` substitution (e.g. the `Log` instance is resolved and cached
before the extension swaps streams, if commons-logging or the SLF4J bridge
caches a `PrintStream` reference rather than re-reading `System.out` on
every write).

**Not confirmed:** this session did not trace the native `System.out`
substitution or Logback appender code to pin this to a specific
file/line — the two candidate mechanisms above are plausible but
unverified. Confirming would mean: build a minimal standalone repro (a
commons-logging `Log.info(...)` call inside a `CapturedOutput`-extended
JUnit test, no Spring context) and check whether the captured text is
empty on CratonVM but populated on HotSpot; if reproducible standalone,
trace `System.setOut`/`OutputCaptureExtension`'s stream-swap native path
next.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-docker-compose` | `org.springframework.boot.docker.compose.lifecycle.DockerComposeLifecycleManagerTests` |
