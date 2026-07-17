# spring-boot-zipkin: both classes HANG with a retry-shaped spin, zero other output (UNCONFIRMED)

**Status: OPEN — found 2026-07-17. Hypothesis only, not root-caused.**

## Symptom

Both classes in `module/spring-boot-zipkin` time out (HANG) with **empty**
`.out.log` files. The only signal in `.err.log` for the entire run duration
is the well-known, otherwise-benign
`gen_heap::get_field: out-of-bounds field read dropped` guard warning
against `org/junit/jupiter/engine/execution/InterceptingExecutableInvoker`
(documented elsewhere as harmless noise —
`docs/internal/app-jvm-bugs/bug-wildfly-get-field-factory-noise.md`) —
repeating at a **regular, increasing interval** for the whole run, with no
other output before or after.

`ZipkinHttpClientSenderTests`: fires at ~50-100ms intervals initially,
decaying to roughly one pair per second later in the run (an
exponential-backoff-shaped cadence). Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-zipkin.org.springframework.boot.zipkin.autoconfigure.ZipkinHttpClientSenderTests.err.log`

`ZipkinContainerConnectionDetailsFactoryWithoutActuatorTests`: same shape,
same cadence pattern. Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-zipkin.org.springframework.boot.zipkin.testcontainers.ZipkinContainerConnec-6e2e95826526.err.log`

## Analysis (honest: not root-caused)

The repeating `get_field` OOB warning is **not itself the cause** — it is
pre-existing tracked noise emitted on every JUnit interceptor invocation
(harmless, already guarded/dropped). What is diagnostic here is that it is
the **only** signal for the entire hang window, at a cadence that looks like
an exponential-backoff retry loop (starts fast, slows down) — consistent
with a network client (`ZipkinHttpClientSenderTests` uses a real HTTP
client to send spans; `ZipkinContainerConnectionDetailsFactoryWithoutActuatorTests`
is a Testcontainers-based connection-details test that likely needs to
reach — or time out reaching — a Zipkin container/HTTP endpoint) retrying a
connection attempt that never completes and never throws, so it retries
forever instead of failing fast, up to (and past) the suite runner's
timeout.

This session did **not** identify which specific blocking call spins, nor
whether the retry itself is CratonVM-specific behavior or the underlying
connection attempt (which per the module's methodology note, both classes
run under `CRATONVM_REAL_NET_SOCKETS=1`) hangs indefinitely instead of
failing/timing out the way it does on HotSpot. Both classes share the exact
same signature, so this is filed as one shared cluster rather than two
per-class docs, but the mechanism is genuinely unconfirmed.

**What would confirm/refute this:** a `--stack-dump-on-timeout` capture (or
`CRATONVM_DBG_*` thread-state trace) taken mid-hang would show which
thread/frame is actually blocked or spinning — that is the single next step
that would turn this from a hypothesis into a root cause. This session did
not have live-binary access to capture one (investigation was log-only per
task scope).

## Possibly related

The `module/spring-boot-hazelcast` `HazelcastAutoConfigurationServerTests`
HANG (see
[`hazelcast-socketchannel-bind-and-server-hang.md`](hazelcast-socketchannel-bind-and-server-hang.md))
and the `module/spring-boot-reactor-netty`
`NettyReactiveWebServerFactoryTests` HANG (see
[`reactor-netty-server-startup-hang.md`](reactor-netty-server-startup-hang.md))
show a similar "silent hang, only benign JUnit-interceptor noise visible"
shape under real-socket mode — worth cross-checking once one of the three
is root-caused, but each is filed separately here since none of the three
has a confirmed mechanism yet, and the interval *shape* differs between
this doc (decaying/exponential) and the reactor-netty one (constant fast
interval) — they may not share the same underlying cause.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-zipkin` | `org.springframework.boot.zipkin.autoconfigure.ZipkinHttpClientSenderTests` |
| `module/spring-boot-zipkin` | `org.springframework.boot.zipkin.testcontainers.ZipkinContainerConnectionDetailsFactoryWithoutActuatorTests` |
