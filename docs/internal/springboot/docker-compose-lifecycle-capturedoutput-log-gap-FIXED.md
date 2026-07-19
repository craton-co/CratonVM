# `DockerComposeLifecycleManagerTests` — `CapturedOutput` never sees this class's `commons-logging` output

**Status: FIXED 2026-07-18**

## Resolution

Same root cause and fix as
`conditionevaluationreport-capturedoutput-empty-cluster-FIXED.md`:
`org/apache/commons/logging/LogFactory.getLog(...)` was natively
overridden to return a fake `Log` whose `info`/`warn` never reached
`System.out`/`System.err`. `DockerComposeLifecycleManager`'s `private
static final Log logger = LogFactory.getLog(...)` (this doc's hypothesis
(a), "a narrower residual of the `LoggerContext` family") was exactly
right — removing the fake `Log`/`LogFactory` override (and the sibling
`ch/qos/logback/classic/Logger`/`LoggerContext.getLogger` overrides that
made the underlying Logback appender unreachable even when a real `Log`
did delegate to SLF4J) fixes it.

Verified with binary `cratonvm-captured-output-fix-20260717.exe`
(worktree `C:\craton\CratonVM-captured-output-cluster-20260717`, branch
`fix/captured-output-empty-cluster-20260717`):
`DockerComposeLifecycleManagerTests` — **PASS**, 11.8s (previously 4/31
failing with the empty-`CapturedOutput` shape).

## Symptom (pre-fix)

| Class | Failures |
|---|---:|
| `org.springframework.boot.docker.compose.lifecycle.DockerComposeLifecycleManagerTests` | 4/31 |

All 4 failing tests used `@ExtendWith`'d `CapturedOutput` and asserted a
specific `logger.info(...)`/`logger.warn(...)` line reached the captured
`System.out`/`System.err` text; every failure showed the captured output
as the empty string. See git history for this file for the original full
symptom detail (representative failures, root-cause hypotheses).

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-docker-compose` | `org.springframework.boot.docker.compose.lifecycle.DockerComposeLifecycleManagerTests` |
