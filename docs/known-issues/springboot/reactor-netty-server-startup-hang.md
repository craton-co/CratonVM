# spring-boot-reactor-netty: `NettyReactiveWebServerFactoryTests` HANG, tight constant-rate spin (UNCONFIRMED)

**Status: OPEN — found 2026-07-17. Hypothesis only, not root-caused.**

## Symptom

`NettyReactiveWebServerFactoryTests` times out (HANG) with an empty
`.out.log`. `.err.log` shows normal boot warm-up (`Unsafe`
`ARRAY_*_BASE_OFFSET` post-clinit fixup, `BigInteger` constant population,
`sun.misc.Unsafe MEMORY_ACCESS_OPTION`, `File fs/separator` fixup, a
`Missing native method in real-JDK mode` WARN for
`jdk/jfr/internal/JVM.subscribeLogLevel` — all routine boot noise seen
across many other classes, not specific to this failure), then settles into
the same well-known, otherwise-benign
`gen_heap::get_field: out-of-bounds field read dropped` guard warning
against `org/junit/jupiter/engine/execution/InterceptingExecutableInvoker`
(documented as harmless noise elsewhere —
`docs/internal/app-jvm-bugs/bug-wildfly-get-field-factory-noise.md`) —
repeating at a **constant, fast (~80-100ms) cadence**, with zero other
output, for the remainder of the observed run.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-reactor-netty.org.springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests.err.log`

## Analysis (honest: not root-caused)

As with the `spring-boot-zipkin` hangs (see
[`zipkin-realsocket-retry-spin-hang.md`](zipkin-realsocket-retry-spin-hang.md)),
the repeating warning itself is tracked, benign noise — not the cause. What
differs from the zipkin case is the **cadence shape**: here the interval
stays flat (~80-100ms) rather than decaying/growing, which reads more like
a **fixed-interval polling loop** (e.g. an event-loop or readiness-check
spin with a constant sleep) than an exponential-backoff HTTP retry. This is
consistent with — but not proof of — Reactor Netty's embedded server
startup path (binding a real socket under `CRATONVM_REAL_NET_SOCKETS=1`,
per the module's runner methodology note) spinning on a bind/readiness
check that never signals completion under CratonVM.

This session did not identify the specific blocked/spinning thread or
frame; filed as OPEN with an honest "not root-caused" status. A
`--stack-dump-on-timeout` capture mid-hang is the concrete next step needed
to turn this into a confirmed root cause — not available in this
investigation (log-analysis-only scope, no live binary access).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-reactor-netty` | `org.springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests` |
