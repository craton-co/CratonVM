# `OutputCaptureExtension`'s nested per-method `CapturedOutput` window leaks content across test methods in the same class

**Status: OPEN — found 2026-07-18**

## Context

Residual of [`conditionevaluationreport-capturedoutput-empty-cluster.md`](../../internal/springboot/conditionevaluationreport-capturedoutput-empty-cluster-FIXED.md)
(now FIXED/archived — that doc's primary defect, a synthetic
`ch.qos.logback.classic.Logger`/`org.apache.commons.logging.Log` pair that
silently swallowed all Logback/commons-logging output before it ever
reached `System.out`/`System.err`, is fixed). With real output now
reaching `CapturedOutput`, one class from that doc's affected list —
`core/spring-boot-test-autoconfigure`'s
`OnFailureConditionReportContextCustomizerFactoryTests` — still fails, but
with a **different, more specific** shape than "empty": the captured text
is now rich and real (banner, real log lines, a full CONDITIONS EVALUATION
REPORT block), but it's the **wrong test method's** content bleeding into
the current method's capture window.

## Symptom

2 of 3 tests fail:

```
JUnit Jupiter:OnFailureConditionReportContextCustomizerFactoryTests:loadFailureShouldNotPrintReportWhenApplicationPropertiesIsBroken(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual:
  "Mockito is currently self-attaching ...
  <Spring Boot banner>
  <real log lines for THIS test's own FailingTests context>
  ...
  CONDITIONS EVALUATION REPORT
  ...
   OnFailureConditionReportContextCustomizerFactoryTests.TestAutoConfiguration matched:
  ..."
not to contain:
  "TestAutoConfiguration matched"
```

The test explicitly asserts the conditions-evaluation-report block should
**not** appear for this scenario (`application.properties` broken → context
fails before the report-relevant beans are even evaluated), yet the
captured text contains a full report — including `TestAutoConfiguration
matched`, which per the doc's original 2026-07-17 analysis appears to be
content that belongs to a *different* test method's run in the same class,
not this one's.

Full log:
`apps/spring-boot-suite-runner/.suite/results/captured-output-fix-verify-20260717c/all-jit/logs/core_spring-boot-test-autoconfigure.org.springframework.boot.test.autoconfigure.OnFailureConditionReportC-98fa9a272e77.out.log`

## Root cause (hypothesis — not confirmed against native source this session)

Spring's `OutputCapture` (`org.springframework.boot.test.system.OutputCapture`)
maintains a `Deque<SystemCapture>` — one pushed in `beforeAll` (class-level,
outermost `PrintStreamCapture` wrapping the pristine `System.out`) and one
pushed/popped per test method in `beforeEach`/`afterEach` (nested, wrapping
whatever `System.out` currently is). `CapturedOutput.getAll()` concatenates
**every** `SystemCapture` currently on the deque, not just the innermost
one — by design, this lets a class-level capture also see output. Given
this class's tests each start a **new** `AnnotationConfigApplicationContext`/
`SpringApplication`-driven scenario per method (via nested
`@Nested`/`FailingTests` static classes, judging by the logger name
`OnFailureConditionReportContextCustomizerFactoryTests$FailingTests` in the
captured text), the observed behavior is consistent with: Logback's
`ConsoleAppender` (bound once to the *live-at-that-moment* `System.out` —
see the FIXED doc's `ConsoleTarget$1` analysis, which reads `System.out`
fresh on every write, so this isn't a caching problem) correctly writes
into *some* currently-active `SystemCapture` layer, but the specific
JUnit5 test method ordering/nesting on CratonVM does not isolate each
method's own window the same way HotSpot does — a later test's capture
window observes an earlier method's still-buffered content.

This needs a live repro instrumenting `OutputCapture.SystemCapture.append`
(or an equivalent trace of `this.systemCaptures` deque contents at
assertion time per test method) to pin down whether this is a JUnit5
extension-context `Store` scoping bug (nested `@Nested` classes may get
their own `ExtensionContext` chain, and `OutputCaptureExtension`'s
`context.getStore(...).computeIfAbsent(...)` must correctly find the
*class-hierarchy-appropriate* `OutputCapture` instance) or something
specific to how many `SystemCapture` layers are alive at once in this
particular multi-context-per-class test shape. Not root-caused at the
source level this session — this doc was filed straight from the
observed symptom after the primary (empty-capture) defect was fixed.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-test-autoconfigure` | `org.springframework.boot.test.autoconfigure.OnFailureConditionReportContextCustomizerFactoryTests` (2 of 3 tests) |
