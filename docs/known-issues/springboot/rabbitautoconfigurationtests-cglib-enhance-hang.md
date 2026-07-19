# `RabbitAutoConfigurationTests` HANG immediately after CGLIB `@Configuration` enhancement

**Status: OPEN — found 2026-07-17**

## Symptom

| Module | Class | Note |
|---|---|---|
| `module/spring-boot-amqp` | `RabbitAutoConfigurationTests` | HANG, killed by suite timeout, no `SBRUNNER_RESULT` |

The `.err.log` (523 lines) starts with the same
`gen_heap::get_field: out-of-bounds field read dropped`
(`InterceptingExecutableInvoker`) warnings seen in the
`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`
cluster, but — unlike those 5 classes — this run **progresses past that
point**: it goes on to real TLS/keystore and CGLIB activity, then stops:

```
[cratonvm-tls] rejecting setKeyEntry TLS identity that does not build a valid ServerConfig (key parse/cert-mismatch); keeping the previously installed identity (see http-server-sslengine-identity-singleton-clobber)
[cratonvm-tls] rejecting setKeyEntry TLS identity that does not build a valid ServerConfig (key parse/cert-mismatch); keeping the previously installed identity (see http-server-sslengine-identity-singleton-clobber)
[cratonvm-tls] rejecting setKeyEntry TLS identity that does not build a valid ServerConfig (key parse/cert-mismatch); keeping the previously installed identity (see http-server-sslengine-identity-singleton-clobber)
[CCE] enhance: defined org/springframework/boot/amqp/autoconfigure/RabbitAutoConfigurationTests$CustomMessageConverterConfiguration$$EnhancerByCGLIB$$0 (super=org/springframework/boot/amqp/autoconfigure/RabbitAutoConfigurationTests$CustomMessageConverterConfiguration, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=1)
```
— this is the **last line in the log**. No further output of any kind
(no exception, no further CGLIB/TLS activity, no test result) until the
suite timeout kills the process.

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-amqp.org.springframework.boot.amqp.autoconfigure.RabbitAutoConfigurationTests.err.log`

No socket/connect/network-related log lines appear anywhere in the log (a
targeted grep for `connect`/`5672`/`socket`/`refused`/`timeout`, case
insensitive, found nothing), which weighs against a "blocked on a real
AMQP broker connection" theory — `RabbitAutoConfigurationTests` does not
eagerly open a broker connection at context-refresh time for its
`@Bean`-method-based configuration classes; the connection would normally
be created lazily.

## Root cause

**Not confirmed — hypothesis.** The hang lands immediately after CGLIB
finishes *defining* the enhanced subclass for a
`@Configuration(proxyBeanMethods = true)` inner class
(`CustomMessageConverterConfiguration$$EnhancerByCGLIB$$0`) — i.e. right
where the Spring container would next either instantiate that enhanced
class or invoke one of its intercepted `@Bean` methods through CGLIB's
`MethodInterceptor` callback. This project has prior, related — but
**already-fixed** — hangs in the same "dynamic class generation /
enhancement, then stuck" shape:
`docs/internal/fixed-suite-bugs/http-server-zerocopy-bytebuddy-vtable-classmanager-deadlock-FIXED.md`
(ByteBuddy, not CGLIB, and a different trigger — AssertJ's lazy proxy
generation) and
`docs/internal/fixed-suite-bugs/class-manager-rwlock-recursive-read-deadlock-FIXED.md`.
Given both of those are marked FIXED and neither is a byte-for-byte match
(different generator — CGLIB vs ByteBuddy — and this hang happens *after*
class definition completes, not during it), this is **not** presented as a
recurrence of either fixed bug, only as a plausible same-family candidate
(a lock-ordering issue between class definition/registration and
first-use dispatch for a freshly-defined synthetic class) worth checking
against `vtable`/`class_manager` locking if this is picked up — not
confirmed against current source this round.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-amqp` | `org.springframework.boot.amqp.autoconfigure.RabbitAutoConfigurationTests` |
