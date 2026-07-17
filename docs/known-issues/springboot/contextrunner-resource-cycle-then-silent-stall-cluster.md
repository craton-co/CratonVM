# `ApplicationContextRunner`-style tests HANG after many successful resource create/destroy cycles, then go silent with no error

**Status: OPEN — found 2026-07-17**

## Symptom

6 classes across 3 modules HANG (full-timeout, no `SBRUNNER_RESULT`, `.out.log`
empty in every case). Unlike the classpath-modification-driven hangs in
[`modifiedclasspath-aether-network-hang-cluster.md`](modifiedclasspath-aether-network-hang-cluster.md)
(none of these 6 use `@ClassPathExclusions`/`@ClassPathOverrides` — checked
against the actual test source in this worktree, `apps/spring-boot/module/.../src/test/java/...`)
and unlike the churning-warning shape in
[`mockwebenvironmentservletcomponentscanintegrationtests-hang.md`](mockwebenvironmentservletcomponentscanintegrationtests-hang.md),
these 6 classes' `.err.log`s show **substantial genuine application-level
activity** — many test methods evidently ran and completed successfully,
each one starting and then cleanly tearing down a real, heavyweight
resource (an embedded Tomcat instance, a Hazelcast node, an OpenTelemetry
`SdkTracerProvider`) — before the process goes completely silent partway
through the class and never produces another log line until the suite
kills it at the shard timeout.

| Module | Class | Resource observed cycling | Last content before silence |
|---|---|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.EndpointRequestIntegrationTests` | Tomcat (`Initializing ProtocolHandler` → `Starting service` → `Starting Servlet engine` cycles, several times) | `INFO [org.apache.catalina.core.StandardEngine] Starting Servlet engine: [Apache Tomcat/11.0.22]` (mid-startup, next cycle never completes) |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests` | Tomcat, same pattern, plus one full `Initializing`→`Starting`→`Stopping`→`Initializing` cycle visible | `INFO [org.apache.catalina.core.StandardEngine] Starting Servlet engine: [Apache Tomcat/11.0.22]` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.MvcEndpointRequestIntegrationTests` | Tomcat, several full `Initializing`→`Starting`→`Stopping` cycles | `INFO [org.apache.coyote.http11.Http11NioProtocol] Stopping ProtocolHandler ["http-nio-auto-5-57669"]` |
| `module/spring-boot-cache` | `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests` | Hazelcast (`[127.0.0.1]:5703 is STARTING` → `... is STARTED` → `... is SHUTTING_DOWN` → `Hazelcast Shutdown is completed in 148 ms` → `... is SHUTDOWN`, one **complete** lifecycle) | `INFO [com.hazelcast.core.LifecycleService] [127.0.0.1]:5703 [dev] [5.5.0] [127.0.0.1]:5703 is SHUTDOWN` (a clean, fully-finished cycle — the very next line is nothing) |
| `module/spring-boot-micrometer-tracing-opentelemetry` | `org.springframework.boot.micrometer.tracing.opentelemetry.autoconfigure.OpenTelemetryTracingAutoConfigurationTests` | OpenTelemetry `SdkTracerProvider` (`INFO [io.opentelemetry.sdk.trace.SdkTracerProvider] Calling shutdown() multiple times.` recurring many times, meaning many contexts were created+torn down successfully) | one more `Calling shutdown() multiple times.` line, then silence |
| `module/spring-boot-micrometer-tracing-opentelemetry` | `org.springframework.boot.micrometer.tracing.opentelemetry.autoconfigure.OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests` | None visible — hangs almost immediately (see caveat below) | `Mockito is currently self-attaching to enable the inline-mock-maker...` |

Full logs (relative to repo root, under
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/`):
- `shard5/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.web.reacti-c5af1ba604cb.err.log`
- `shard5/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.web.servle-a0d9d711811f.err.log`
- `shard5/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.web.servle-aca18e35438a.err.log`
- `shard2/logs/module_spring-boot-cache.org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests.err.log`
- `shard4/logs/module_spring-boot-micrometer-tracing-opentelemetry.org.springframework.boot.micrometer.tracin-662392d3f975.err.log`
- `shard4/logs/module_spring-boot-micrometer-tracing-opentelemetry.org.springframework.boot.micrometer.tracin-6f0b5fa976e2.err.log`

## Root cause (hypothesis, not confirmed — no debugger attach this session)

**Not pinned to a specific file/line.** The shared shape (repeated
successful create/destroy of a heavyweight resource, then a clean stop
that never leads to further progress) is consistent with a resource-count-
or thread-count-dependent condition: something that only manifests after N
successful cycles rather than on the first one (ruling out a
straightforward first-call deadlock), and that produces total silence
rather than a crash or a repeating diagnostic (ruling out a busy-loop —
this doc's classes are the closest thing in this session's batch to a
genuine, quiet, `park()`-style deadlock, as opposed to the churning-warning
shape in the sibling doc above). Two candidate directions, neither
investigated at the source level this session:

1. **A leaked/lingering resource from an earlier successful cycle
   eventually deadlocks a later one.** `EndpointRequestIntegrationTests`'s
   sibling class in this same session's batch
   (`ReactiveManagementWebSecurityAutoConfigurationTests`, filed in
   `modifiedclasspath-aether-network-hang-cluster.md`) logged: `WARN
   [org.apache.catalina.loader.WebappClassLoaderBase] The web application
   [ROOT] appears to have started a thread named [parallel-1] but has
   failed to stop it` — a real, CratonVM-observed thread-leak warning from
   Tomcat's own webapp classloader, naming a `ForkJoinPool.commonPool()`
   worker thread specifically. If each `ApplicationContextRunner.run()`
   cycle in these classes leaves one or more threads/handles alive past
   context close (a common shape for a native resource whose
   Rust-side teardown doesn't fully release something the next cycle's
   startup needs — a port, a lock, a thread-pool slot), enough
   accumulated cycles could eventually deadlock or starve a subsequent
   startup. This is consistent with the "many successful cycles, then a
   silent stop" shape, but no specific leaked resource was identified for
   Hazelcast or the OTel `SdkTracerProvider` case — only the one Tomcat
   thread-leak WARN was directly observed, and only in a sibling class,
   not these 6 directly.
2. **A different, only-sometimes-reached teardown/startup code path** (e.g.
   a specific bean combination, port-reuse check, or health-check wait)
   that most test methods in a class don't exercise, and the class happens
   to reach it only after N earlier methods ran — i.e. not a cumulative
   leak, just a genuinely blocking path in one specific test method,
   coincidentally reached late in execution order. This is
   indistinguishable from hypothesis 1 without knowing exactly which test
   method each class was on when it went silent (not captured — no
   `.out.log` content and no thread dump on timeout).

**`OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests`
does not fit the "many successful cycles" pattern** — it hangs almost
immediately, with only the "Mockito is currently self-attaching" line
printed before going silent (this class constructs a `Mockito.mock(ContextStorage.class)`
as an instance field initializer, so this line appears on the very first
test). This is grouped here rather than with the other 5 only because it
shares a module and a Mockito-adjacent moment with
`OpenTelemetryTracingAutoConfigurationTests`; it may be an unrelated,
earlier-triggering bug and is flagged as the weaker-confidence member of
this cluster. Cross-reference: `docs/internal/kafka-suite-bugs/bug-09-mockito-inline-mockmaker-selfattach.md`
reports the Mockito self-attach mechanism itself as **COMPLETE/fixed**
(verified end-to-end for `mock()`/`verify()`/stubbing against a different
suite) — so if that fix is present in this build, this hang is likely a
different, newer stall reached shortly *after* self-attach succeeds rather
than a recurrence of that original self-attach failure. Not confirmed
either way.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.EndpointRequestIntegrationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.MvcEndpointRequestIntegrationTests` |
| `module/spring-boot-cache` | `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests` |
| `module/spring-boot-micrometer-tracing-opentelemetry` | `org.springframework.boot.micrometer.tracing.opentelemetry.autoconfigure.OpenTelemetryTracingAutoConfigurationTests` |
| `module/spring-boot-micrometer-tracing-opentelemetry` | `org.springframework.boot.micrometer.tracing.opentelemetry.autoconfigure.OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests` (weaker-confidence member — see caveat above) |
