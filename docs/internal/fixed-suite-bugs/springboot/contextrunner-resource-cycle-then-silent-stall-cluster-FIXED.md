# `ApplicationContextRunner`-style tests HANG after many successful resource create/destroy cycles, then go silent with no error

**Status: FIXED/REFUTED — 2026-07-17.** The original hypothesis (a leaked
native resource from repeated `ApplicationContext` create/destroy cycles
eventually deadlocking a later cycle) is **refuted**. None of the 6 originally
listed classes are actually deadlocked. Live per-OS-thread CPU sampling
during a hang (`Get-Process ... .Threads | Select TotalProcessorTime`,
sampled twice a few seconds apart) showed one thread continuously consuming
~100% CPU the whole time for every one of the 5 "genuinely parked" classes —
never a thread parked with flat, near-zero CPU growth. Running each of those
5 classes standalone with a generous timeout (600–1800s instead of the
suite's 300s shard default) showed every one of them reaches a real JUnit
result (`FAIL`, never `HANG`) well within that window. The 6th class
(`OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests`)
was always the weakest-confidence member of the original cluster (flagged as
such at the time) and turned out to be a **genuinely different bug** — real,
unbounded interpreter recursion (a captured `--stack-dump-on-timeout` dump
showed the main thread's interpreter frame count at 3132 and still growing,
cycling through `InterceptingExecutableInvoker.invoke`/`invokeVoid` — not a
parked thread at all), now tracked separately.

## What was actually going on, per class

| Class | Original characterization | What re-investigation found |
|---|---|---|
| `CacheAutoConfigurationTests` | Hazelcast full start/stop cycles observed, then silence | Not deadlocked — CPU-pegged the whole time. Completed standalone in 657.8s pre-fix / 392.7s post-`Class.getMethods()`-fix (14→5 failures). Still needs >300s. Residual: [`cacheautoconfigurationtests-infinispan-null-cachemanager-residual.md`](../../known-issues/springboot/cacheautoconfigurationtests-infinispan-null-cachemanager-residual.md) |
| `JerseyEndpointRequestIntegrationTests` | Tomcat repeated cycles observed | Not deadlocked. Completed in 460.7s post-fix (5/9 failed). Residual: [`webclient-loopback-self-connect-timeout-os10060-cluster.md`](../../known-issues/springboot/webclient-loopback-self-connect-timeout-os10060-cluster.md) |
| `MvcEndpointRequestIntegrationTests` | Tomcat repeated cycles observed | Not deadlocked. Completed in 548.7s post-fix (5/9 failed). Same residual doc as above. |
| `EndpointRequestIntegrationTests` | Tomcat repeated cycles observed | Not deadlocked. Completed in 772.5s post-fix (2/4 failed). Same residual doc as above. |
| `OpenTelemetryTracingAutoConfigurationTests` | `SdkTracerProvider` repeated shutdown lines, then silence | Not deadlocked. Completed in 1161.6s post-fix (1/36 failed, a `StackOverflowError` — see [`opentelemetry-contextstorage-early-init-recursion-cluster.md`](opentelemetry-contextstorage-early-init-recursion-cluster-FIXED.md)). This one needed the longest of the 5 — **still exceeds even a 900s carve-out**, needed 1400s in the runner's per-class override table. |
| `OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests` | Weakest-confidence member; hangs almost immediately, not after many cycles | **Confirmed different bug** — genuine unbounded recursion, not resource-cycle-related at all. See [`opentelemetry-contextstorage-early-init-recursion-cluster.md`](opentelemetry-contextstorage-early-init-recursion-cluster-FIXED.md). |

## Root cause of the *original* misdiagnosis

Two compounding factors, both real:

1. **A genuine, already-fixed correctness bug inflated both the failure
   count and the wall-clock time.** `Class.getMethods()` didn't shadow
   overridden methods across a class/interface hierarchy walk
   (`native-builtins/src/lang_class.rs::collect_public_methods`), so Spring's
   `DisposableBeanAdapter` destroy-method resolution saw N duplicate
   `close()`/`shutdown()`/`dispose()` candidates for any bean whose destroy
   method was declared at multiple hierarchy levels (Hazelcast's
   `HazelcastInstance.shutdown()`, in this cluster's case) and refused to
   pick one, throwing `BeanCreationException: ... Invalid destruction
   signature` for every context that instantiated such a bean. Fixed
   2026-07-17 in commit `b6f38f669` (merged `42e0717ac`) — see
   [`class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md`](class-getmethods-override-shadowing-duplicate-close-cluster-FIXED.md).
   This fix alone cut `CacheAutoConfigurationTests`'s failure count from 14
   to 5 and its wall time from 657.8s to 392.7s.
2. **CratonVM is simply much slower than the 300s shard default assumes for
   these particular test classes** — heavy, repeated reflection/annotation
   metadata scanning (`AnnotationTypeMappings`/`ConcurrentReferenceHashMap`/
   `ClassReader.accept` were prominent in every captured stack sample) across
   many `ApplicationContextRunner` cycles per class, plus multiple real
   heavyweight resource startups (Tomcat, Hazelcast, Infinispan, OTel SDK).
   Not further root-caused to a single fixable hot path this session — flagged
   as a standing, generic CratonVM performance gap (reflection/annotation
   scanning under heavy repeated use), not specific to this cluster. The
   *practical* fix applied here is the same one already used for
   `JdbcSessionAutoConfigurationTests` (a ~14-minute class): a documented,
   per-class timeout override in the suite runner
   (`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`'s
   `Get-EffectiveClassTimeoutSec`), not a VM code change.

## Fix applied

`Get-EffectiveClassTimeoutSec` in
`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1` now carries a
`$slowClasses` table with a validated-safe timeout (measured wall time +
comfortable margin) for each of the 5 genuinely-just-slow classes:
`CacheAutoConfigurationTests` (600s), `JerseyEndpointRequestIntegrationTests`
(600s), `MvcEndpointRequestIntegrationTests` (700s),
`EndpointRequestIntegrationTests` (1000s),
`OpenTelemetryTracingAutoConfigurationTests` (1400s). Verified: rerunning
`CacheAutoConfigurationTests` with the standard `-TimeoutSec 300` argument now
logs `timeout override=600s ...` and the class correctly reports `FAIL` (not
`HANG`).

## Original symptom writeup (for context, historical)

6 classes across 3 modules originally showed `.err.log`s with substantial
genuine application-level activity (Tomcat/Hazelcast/OTel resource
create/destroy cycles) before going completely silent and hitting the
300-second shard timeout with no `SBRUNNER_RESULT` line. The original
hypothesis was a leaked native resource (thread, port, lock) from an earlier
successful `ApplicationContextRunner` cycle eventually deadlocking a later
one — motivated by a Tomcat webapp-classloader thread-leak WARN seen in a
sibling class. That hypothesis is refuted by the evidence above; the
resource-cycling logs were real, ordinary application activity, and the
class was always going to finish given enough wall-clock time.

## Originally affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.EndpointRequestIntegrationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.MvcEndpointRequestIntegrationTests` |
| `module/spring-boot-cache` | `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests` |
| `module/spring-boot-micrometer-tracing-opentelemetry` | `org.springframework.boot.micrometer.tracing.opentelemetry.autoconfigure.OpenTelemetryTracingAutoConfigurationTests` |
| `module/spring-boot-micrometer-tracing-opentelemetry` | `org.springframework.boot.micrometer.tracing.opentelemetry.autoconfigure.OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests` (reclassified — different bug, see above) |
