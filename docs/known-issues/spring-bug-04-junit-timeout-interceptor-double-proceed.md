# spring-bug-04: JUnit `@Timeout` interceptor chain invoked twice (`proceed()` called multiple times)

| | |
|---|---|
| **Category** | VM-CORRECTNESS (threads / invocation) |
| **Module** | spring-core (any test using JUnit `@Timeout` / `assertTimeoutPreemptively`) |
| **CratonVM** | FAIL — `JUnitException: Chain of InvocationInterceptors called invocation multiple times` |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | c5644da4 (dev) |
| **Status** | OPEN |
| **Suggested owner** | handoff candidate (test-infra symptom; root cause likely threading/MethodHandle) |

## Symptom
```
org.junit.platform.commons.JUnitException: Chain of InvocationInterceptors called invocation
  multiple times instead of just once: org.junit.jupiter.engine.extension.TimeoutExtension
  at …InvocationInterceptorChain.proceed
```
JUnit's `TimeoutExtension` runs the test body on a **separate thread** with a timeout. Under
CratonVM the interceptor's `proceed()` ends up invoked more than once, which JUnit detects and
rejects. Strongly suggests a CratonVM bug in cross-thread invocation or `MethodHandle`/lambda
invocation re-entry used by the timeout machinery.

## Affected test classes (confirmed CV-unique, HotSpot OK)
```
aot.generate.ValueCodeGeneratorTests
core.ReactiveAdapterRegistryTests
core.PropagationContextElementTests
```
(Will recur in every module — many Spring tests use `@Timeout`. Fixing this clears a broad band.)

## Reproduce
```bash
CP="$H;$(tr -d '\r' < .../spring-core/build/cratonvm-testcp.txt)"
KRUN_STACK=1 "$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.aot.generate.ValueCodeGeneratorTests
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.aot.generate.ValueCodeGeneratorTests   # passes
```

## Suspected root cause
`TimeoutExtension.interceptTestableMethod` submits the invocation to a single-use executor and
joins with a timeout. Candidates:
- the worker thread and the calling thread both run the invocation (thread start semantics — cf.
  memory note "boot-jdk-mismatch makes `new Thread(runnable)` no-op"; here we DO boot JDK25 so
  threads run, but the executor future may also run inline),
- a `MethodHandle.invoke` / lambda is dispatched twice.

## Notes
Verify whether the body executes on both the test thread and the timeout worker. Related to the
threading notes in memory. Likely **one** root cause across all listed classes.
