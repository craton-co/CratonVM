# `module/spring-boot-restclient`: two unrelated residuals

**Status: OPEN — found 2026-07-17**

This module contributed 3 non-passing classes to the 2026-07-17 rerun,
splitting into 2 unrelated root causes.

## Issue A — `*ObservationAutoConfigurationWithoutMetricsTests` HANG: tight sub-second retry loop, no output ever produced

| Class | Note |
|---|---|
| `RestClientObservationAutoConfigurationWithoutMetricsTests` | HANG, 0-byte stdout |
| `RestTemplateObservationAutoConfigurationWithoutMetricsTests` | HANG, 0-byte stdout |

Both hang for the entire run with **no** stdout output at all (JUnit never
prints even its startup banner) and an unbroken, tight (~100-300ms period)
stream of the same benign `gc::guard` out-of-bounds-field-read warning
(see
[`spring-boot-configuration-processor-testcompiler-hang-cluster.md`](spring-boot-configuration-processor-testcompiler-hang-cluster.md)
for why this specific warning is independently known to be benign/red-herring
noise — `real_field_count=Some(0)` on JUnit's own 0-field
`InterceptingExecutableInvoker`/`InvocationInterceptorChain` helper
objects), alternating between exactly 2 live object addresses from the very
first log line:

```
2026-07-17T20:00:50.340404Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (...) obj=0x23814019158 index=0 num_slots=0 class_id=ClassId(754) class_name=org/junit/jupiter/engine/execution/InterceptingExecutableInvoker real_field_count=Some(0)
2026-07-17T20:00:50.526804Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (...) obj=0x2381402db50 index=0 num_slots=0 class_id=ClassId(754) class_name=org/junit/jupiter/engine/execution/InterceptingExecutableInvoker real_field_count=Some(0)
2026-07-17T20:00:50.865266Z  WARN ... obj=0x23814019158 ...
2026-07-17T20:00:50.990349Z  WARN ... obj=0x2381402db50 ...
```
(cadence continues at ~100-300ms intervals for the whole run, unlike the
config-processor cluster's ~20-48s cadence — a materially different, much
tighter loop)

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-restclient.org.springframework.boot.restclient.autoconfigure.RestClientObse-4833dac321f3.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-restclient.org.springframework.boot.restclient.autoconfigure.RestTemplateOb-232da76edbe3.err.log`

### Root cause

**Update:** this matches, exactly, the signature independently
root-caused (as a hypothesis, still unconfirmed by live repro) in
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md) —
a fixed-period (~150-400ms), unbroken-for-the-whole-run rhythm of the same
`gc::guard` out-of-bounds-field-read warning against the *same handful of
live object addresses* (here: exactly 2, alternating), with **zero** other
log output including no JUnit banner/summary. That doc distinguishes this
shape (steady rhythm, fixed objects, zero forward progress = livelock)
from the same warning's ordinary, rare, early-only appearance in the
majority of this round's classes (including many that pass) or the much
slower ~20-48s GC-cadence appearance in
[`spring-boot-configuration-processor-testcompiler-hang-cluster.md`](spring-boot-configuration-processor-testcompiler-hang-cluster.md).
Its working theory: some hot-path (likely JIT-compiled or interpreter
Collection/Map fast-path) speculatively probes
`InterceptingExecutableInvoker`/`InvocationInterceptorChain` — real,
0-field JUnit5 internal classes — as if they had a Collection-like layout;
the guard safely drops each individual out-of-bounds read (so the process
doesn't crash), but whatever loop depends on that probe returning a real
value never observes one, so it retries forever instead of ever reaching
`InterceptingExecutableInvoker.invoke`/`invokeVoid` — the exact method
JUnit5 uses to dispatch `@Test`/`@BeforeEach` calls through the interceptor
chain — which is why no JUnit output (not even the startup banner) is ever
produced. Not yet pinned to the specific caller issuing the speculative
probe; see the referenced doc's own "Root cause" section for the full
reasoning and suggested next steps (`CRATONVM_DBG_JIT_DISASM` / `--nojit`
bisection). This supersedes the network-retry hypothesis originally
considered here — no HTTP-specific evidence was ever found, and the
signature match to a cluster spanning 3 unrelated modules
(devtools/servlet/jsonb, none doing HTTP client calls) is much stronger
evidence than the coincidence of these 2 classes happening to be
`RestClient`/`RestTemplate` tests.

## Issue B — `HttpServiceClientAutoConfigurationTests` FAIL: AssertJ can't read a lambda's captured-variable field via reflection

| Class | tests failed/total |
|---|---:|
| `HttpServiceClientAutoConfigurationTests` | 2/7 |

```
JUnit Jupiter:HttpServiceClientAutoConfigurationTests:whenHasUserDefinedRequestFactoryBuilder()
    => org.assertj.core.util.introspection.IntrospectionError:
Can't find any field or property with name 'arg$1'.
Error when introspecting properties was :
- No getter for property 'arg$1' in org.springframework.web.service.invoker.HttpServiceMethod$ExchangeResponseFunction$$Lambda/0x8000031a
Error when introspecting fields was :
- Unable to obtain the value of the field <'arg$1'> from <@5d123>
       org.assertj.core.extractor.ByNameSingleExtractor.apply(ByNameSingleExtractor.java:32)
       org.springframework.boot.restclient.autoconfigure.service.HttpServiceClientAutoConfigurationTests.getRestClient(HttpServiceClientAutoConfigurationTests.java:232)
     Caused by: java.lang.IllegalArgumentException: Cannot locate field arg$1 on class org.springframework.web.service.invoker.HttpServiceMethod$ExchangeResponseFunction$$Lambda/0x8000031a
       org.assertj.core.util.introspection.FieldUtils.readField(FieldUtils.java:171)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-restclient.org.springframework.boot.restclient.autoconfigure.service.HttpSe-7ff1b204baf2.out.log`

### Root cause (unconfirmed hypothesis)

The test's own helper (`getRestClient`,
`HttpServiceClientAutoConfigurationTests.java:232`) uses AssertJ's
`extracting("arg$1")` to reach into a lambda instance
(`HttpServiceMethod$ExchangeResponseFunction`'s method-reference lambda)
and read its captured variable — `arg$1` is the standard javac-generated
name for a lambda/inner-class's first synthetic captured-field. AssertJ's
fallback field-reflection path
(`org.assertj.core.util.introspection.FieldUtils.readField`) calls
`Class.getDeclaredField("arg$1")` (or iterates `getDeclaredFields()`) on
the lambda proxy class and gets `IllegalArgumentException: Cannot locate
field arg$1` — i.e. CratonVM's reflective field metadata for this
`$$Lambda` proxy class does not expose (or does not name) its
captured-variable field the way real HotSpot's `LambdaMetafactory`-spun
classes do. This is a distinct mechanism from the already-documented,
already-fixed lambda-`Comparable`/class-registration gaps in
`comparable-classcast-lambda-proxy-unknown-class.md` (that doc is about
`class_manager` never registering lambda-proxy classes for
`instanceof`/`Comparable` checks, not about a lambda class's own
declared-field enumeration) — filed as a distinct, narrower issue. Not
investigated further at the source level (e.g.
`native-builtins/src/lang_class.rs`'s `getDeclaredField(s)` path for
synthesized lambda classes) this session.

**Cross-reference (2026-07-17, separate triage batch, same rerun):**
`module/spring-boot-webclient`'s `ReactiveHttpServiceClientAutoConfigurationTests`
independently fails 5/6 tests with the byte-identical `arg$1` shape against
the reactive sibling lambda class
(`HttpServiceMethod$ReactorExchangeResponseFunction$$Lambda` instead of
`HttpServiceMethod$ExchangeResponseFunction$$Lambda`), via its own
`getWebClient`/`getJdkHttpClient` helper (`ReactiveHttpServiceClientAutoConfigurationTests.java:204`,
`:188`) — e.g.
```
JUnit Jupiter:ReactiveHttpServiceClientAutoConfigurationTests:whenHasUserDefinedHttpConnectorBuilder()
    => org.assertj.core.util.introspection.IntrospectionError:
Can't find any field or property with name 'arg$1'.
...
     Caused by: java.lang.IllegalArgumentException: Cannot locate field arg$1 on class org.springframework.web.service.invoker.HttpServiceMethod$ReactorExchangeResponseFunction$$Lambda/0x800002e2
```
Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webclient.org.springframework.boot.webclient.autoconfigure.service.Reactive-10289f527353.err.log`
(and matching `.out.log`). The 6th failure in that class
(`configuresClientFromPropertiesWhenHasHttpConnectorAutoConfiguration`, an
`AssertionError: Expecting Optional to contain: 5S but was empty` from
`assertConnectTimeout`) has a different visible symptom shape and is **not
confirmed** to share this exact cause — plausibly the same underlying
`getJdkHttpClient` chain hitting a different synthetic-object gap rather than
the `arg$1` reflection failure specifically; not traced further this session.

**Cross-reference (2026-07-17, bin7 rerun triage):**
`module/spring-boot-security-oauth2-resource-server`'s
`reactive.ReactiveOAuth2ResourceServerAutoConfigurationTests` independently
fails 3 of its tests with the byte-identical `arg$1` shape, against a
different lambda entirely (`NimbusReactiveJwtDecoder$JwkSetUriReactiveJwtDecoderBuilder$$Lambda`/
`NimbusReactiveJwtDecoder$PublicKeyReactiveJwtDecoderBuilder$$Lambda`, not
`HttpServiceMethod`) — confirming this is a general lambda-captured-field
reflection gap, not specific to `HttpServiceMethod`'s lambdas:
```
JUnit Jupiter:ReactiveOAuth2ResourceServerAutoConfigurationTests:autoConfigurationUsingJwkSetUriShouldConfigureResourceServerUsingSingleJwsAlgorithm()
    => org.assertj.core.util.introspection.IntrospectionError:
Can't find any field or property with name 'arg$1'.
Error when introspecting properties was :
- No getter for property 'arg$1' in org.springframework.security.oauth2.jwt.NimbusReactiveJwtDecoder$JwkSetUriReactiveJwtDecoderBuilder$$Lambda/0x8000039d
```
Confirmed from source
(`ReactiveOAuth2ResourceServerAutoConfigurationTests.java:145,170,200`) —
the test itself does `.extracting("jwtProcessor.arg$1.signatureAlgorithms")`/
`.extracting("jwtProcessor.arg$1.jwsKeySelector.expectedJWSAlg")`, the exact
same `arg$1` captured-variable-field navigation pattern as
`HttpServiceClientAutoConfigurationTests.getRestClient`. Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-security-oauth2-resource-server.org.springframework.boot.security.oauth2.se-6031fd510b0d.out.log`.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests` |
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests` |
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.service.HttpServiceClientAutoConfigurationTests` |
| `module/spring-boot-webclient` | `org.springframework.boot.webclient.autoconfigure.service.ReactiveHttpServiceClientAutoConfigurationTests` (5/6 failures; 6th not confirmed same cause) |
| `module/spring-boot-security-oauth2-resource-server` | `org.springframework.boot.security.oauth2.server.resource.autoconfigure.reactive.ReactiveOAuth2ResourceServerAutoConfigurationTests` (3 failures, added bin7) |
