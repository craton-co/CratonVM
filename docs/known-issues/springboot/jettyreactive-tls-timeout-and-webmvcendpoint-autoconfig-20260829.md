# `JettyReactiveWebServerFactoryTests` (mTLS reject) and `WebMvcEndpointIntegrationTests` (actuator autoconfig) — two confirmed CratonVM bugs, unrelated to each other

## Status
**OPEN, both confirmed CratonVM-specific, neither fully root-caused.** Found
2026-08-29 triaging a full Spring Boot suite run's 6 residual FAILs (4 of
the 6 already tracked elsewhere as not-CratonVM-bugs: `ZipContentTests` and
the micrometer trio). These two are new. Differential-verified against stock
HotSpot 25 on the same classpath: both pass cleanly there.

Filed together as one doc since both surfaced in the same sweep, but they
are **not the same bug** — different modules, different mechanisms, nothing
shared identified.

## 1. `JettyReactiveWebServerFactoryTests.sslNeedsClientAuthenticationFailsWithoutClientCertificate()`

**HotSpot**: 37/37 pass (1 unrelated skip: `noCompressionForUserAgent`,
"Jetty 12 does not support User-Agent-based compression"). **CratonVM**: this
one test fails, the other 36 pass.

```
java.lang.AssertionError: VerifySubscriber timed out on reactor.core.publisher.MonoFlatMap$FlatMapMain@7234
	at reactor.test.DefaultStepVerifierBuilder$DefaultVerifySubscriber.verify(...)
	at org.springframework.boot.web.server.reactive.AbstractReactiveWebServerFactoryTests.testClientAuthFailure(...)
	at ...sslNeedsClientAuthenticationFailsWithoutClientCertificate(...)
```

The test starts a Jetty reactive server requiring client-certificate
authentication (mTLS), connects without presenting one, and expects the
reactive pipeline to surface a handshake failure through a `StepVerifier`.
The log immediately before the failure shows the handshake DID fail as
expected:
```
io.netty.channel.StacklessClosedChannelException
	Suppressed: io.netty.handler.ssl.StacklessSSLHandshakeException: Connection closed while SSL/TLS handshake was in progress
```
So the server-side rejection appears to happen — but the `StepVerifier`
never observes a terminal signal (error or completion) and times out instead
of the client-side call this test asserts on. This points at the client-side
`WebClient`/Reactor Netty call not propagating the connection-reset/handshake
failure as an error onto the reactive chain the way HotSpot's stack does,
rather than at the server-side TLS rejection itself (which appears correct).
Not root-caused further — the exact point where the signal is lost
(Netty's `SslHandler` → Reactor Netty's connection lifecycle → the `Mono`
this test subscribes to) was not traced.

## 2. `WebMvcEndpointIntegrationTests` — two failures, likely one shared cause within this class

**HotSpot**: 4/4 pass. **CratonVM**: 2 of 4 fail.

```
webMvcEndpointHandlerMappingIsConfiguredWithPathPatternParser():
  NoSuchBeanDefinitionException: No qualifying bean of type
  'org.springframework.boot.webmvc.actuate.endpoint.web.WebMvcEndpointHandlerMapping' available

endpointJsonMapperCanBeApplied():
  AssertionFailedError: [HTTP status code] expected: 200 but was: 404
```

Both failures are consistent with the same underlying cause: the
`WebMvcEndpointHandlerMapping` bean this test's `DefaultConfiguration` inner
class expects `ManagementContextAutoConfiguration` /
`ServletManagementContextAutoConfiguration` / `WebEndpointAutoConfiguration`
to register (via `@ImportAutoConfiguration`) never gets created — the first
test observes this directly (`getBean(...)` throws
`NoSuchBeanDefinitionException`), and the second is very plausibly downstream
of it: with no handler mapping registered, the actuator endpoint under test
simply isn't routed, producing a 404 instead of 200. Not confirmed the two
share a cause (not traced with a debugger/breakpoint), but the shape strongly
suggests it — one missing bean explaining both.

Not root-caused further — which specific `@Conditional*` in the
autoconfiguration chain evaluates differently under CratonVM (a classpath
condition, a bean-presence condition, a property condition) was not
isolated in this pass.

## Next steps
* For (1): add logging/a breakpoint in Reactor Netty's connection-error
  path to see whether the `StacklessSSLHandshakeException` ever reaches the
  `Mono` the test subscribes to, or is swallowed/misrouted earlier under
  CratonVM specifically. Compare against a minimal non-Spring Reactor Netty
  mTLS-rejection repro if the Spring context proves hard to isolate in.
* For (2): enable Spring's autoconfiguration report
  (`--debug` / `ConditionEvaluationReport`) for
  `webMvcEndpointHandlerMappingIsConfiguredWithPathPatternParser()` on both
  VMs and diff which conditions matched — this should point directly at the
  divergent condition without needing to read the whole autoconfiguration
  chain by hand.

## Repro
```bash
cd apps/spring-boot/module/spring-boot-jetty   # or spring-boot-webmvc
source <toolchain env>
CP="apps/spring-boot/sb-runner:$(cat build/cratonvm-test-cp.txt)"
<cratonvm-bin> --java-home <jdk25-home> --Xmx 2g -cp "$CP" SbRunner \
  org.springframework.boot.jetty.reactive.JettyReactiveWebServerFactoryTests
<cratonvm-bin> --java-home <jdk25-home> --Xmx 2g -cp "$CP" SbRunner \
  org.springframework.boot.webmvc.autoconfigure.actuate.web.WebMvcEndpointIntegrationTests
# compare against: <jdk25-home>/bin/java -cp "$CP" SbRunner <same class>
```
