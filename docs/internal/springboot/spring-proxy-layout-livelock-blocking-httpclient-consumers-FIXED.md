# Spring proxy layout-probe livelock blocking broad HttpClient consumer classes — FIXED

**Status: FIXED 2026-07-18**

## Root cause

The reported `$Proxy28` slot-one diagnostics were a symptom, not an undersized proxy layout. Real-super generated proxies correctly have only the inherited `java.lang.reflect.Proxy.h` field at slot zero.

The liveness fault was the unrelated foreign-function downcall-adapter fast path in `try_stackless_invoke`. It selected any method named `invoke`, read a receiver field before proving the receiver was a `MethodHandle`, and could therefore repeatedly probe JUnit/Spring startup objects with an incompatible layout. The guard returned a benign null, leaving the invocation path without forward progress.

The integrated JUnit adapter fix (`b7309a005`) first verifies the runtime receiver hierarchy is `java/lang/invoke/MethodHandle` or a subclass. Ordinary same-named methods return to normal dispatch without a speculative field read. This also prevents the proxy-layout warning flood observed by these consumer classes.

## Validation

Using JDK 25.0.3 and a fresh release build named `cratonvm-spring-proxy-layout-livelock-019f7681.exe`, with `CRATONVM_DBG_OOBFIELD=org/springframework/core/$Proxy`:

| Mode | Class | Result |
|---|---|---|
| JIT | `ReactiveHttpClientAutoConfigurationTests` | PASS, 13/13, 56.0 s |
| JIT | `TestRestTemplateTests` | PASS, 48/48, 60.8 s |
| `--nojit` | `ReactiveHttpClientAutoConfigurationTests` | PASS, 13/13, 36.3 s |
| `--nojit` | `TestRestTemplateTests` | PASS, 48/48, 46.2 s |

None of the four runs emitted an `$Proxy` OOB trace or reached the prior 300-second per-class timeout. The selected redirect assertions remain covered by the full-class executions; no remaining HttpClient-builder transport issue is attributed to this closure.
