# `module/spring-boot-restclient`: two unrelated residuals

**Status: Issue A FIXED 2026-07-18 (`b7309a005`), Issue B FIXED 2026-07-20, `ObservationRegistry` residual FIXED (confirmed 2026-07-21)**

## Update 2026-07-21: `ObservationRegistry` residual closed

The `BeanDefinitionOverrideException` residual noted below (tracked in
[`observationregistry-conditionalonmissingbean-classpathexclusions.md`](observationregistry-conditionalonmissingbean-classpathexclusions-FIXED.md))
is now also fixed — see that doc for details. It turned out to already be
fixed on `dev` by `65d738bb5`, an unrelated commit from a concurrent
investigation session, discovered and confirmed via re-verification rather
than a new fix authored in this pass. All residuals of this doc are now
closed.

## Resolution

**Issue A** (the `InterceptingExecutableInvoker` speculative-layout-probe
livelock) was fixed as part of a wider cluster —
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md)
in this directory — merged to `dev` 2026-07-18. `RestClientObservationAutoConfigurationWithoutMetricsTests`
and `RestTemplateObservationAutoConfigurationWithoutMetricsTests` no longer
hang (verified both JIT-on and `--nojit`); each now completes in well under
a minute instead of hitting the suite timeout.

**Issue B** (the `arg$1` lambda-captured-field reflection gap) was fixed
2026-07-20 in `native-builtins/src/lang_class.rs`. Root cause: lambda-proxy
classes live in `shared.lambda_proxies`, never `class_manager`, so
`NativeContext::declared_fields` (backing `Class.getDeclaredField(s)`)
returned an empty list for them — `getDeclaredField("arg$1")` always threw
`NoSuchFieldException`, and AssertJ's `extracting("...arg$1...")` reflection
fallback failed identically across every affected module.

Fix: `declared_fields_with_aliases` now synthesizes the captured-variable
fields a real `LambdaMetafactory`-spun proxy class carries, from
`NativeContext::lambda_proxy_serial_metadata(class_id)`'s `capture_types`
(one type char per capture, in factory-argument/heap-field order — see
`allocate_lambda_proxy` in `vm/src/runtime/invokedynamic.rs`, which writes
captures at object fields `0..n` in that same order). Two non-obvious
details, both found empirically by running the actual affected tests (not
guessable from reading `LambdaMetafactory`'s public contract alone):

1. **Field naming is 1-based**: HotSpot's `InnerClassLambdaMetafactory`
   names captured fields `arg$1`, `arg$2`, ... — there is no `arg$0`. The
   *field name* suffix is `capture_index + 1`; the *heap slot index* stays
   0-based (`capture_index`) to match how `allocate_lambda_proxy` actually
   lays out the object. Getting this off-by-one wrong doesn't throw
   `NoSuchFieldException` (the field IS found) — it silently returns the
   *wrong captured value* to the caller, which then fails one step further
   downstream with a confusing, unrelated-looking error (e.g. "Cannot
   locate field `restClient` on class `ParameterizedTypeReference$1`" —
   the accessor resolved a different capture than the one the test's field
   chain expected).
2. **Not synthetic**: real HotSpot's generated captured-variable fields are
   plain `private final`, **without** `ACC_SYNTHETIC`. Marking them
   synthetic (a natural first guess, since they're VM-generated) breaks
   AssertJ specifically: `org.assertj.core.util.introspection.FieldUtils
   .readField` hard-rejects synthetic fields (`IllegalArgumentException:
   Reading synthetic field is not supported`), and `extracting("arg$1")` is
   exactly AssertJ's own pattern for reaching a lambda's capture.

Reference-typed captures lose their exact declared type upstream (
`parse_descriptor_args` in `vm/src/runtime/invokedynamic.rs` collapses every
object/array capture to a single `'L'` marker with no class name retained),
so the synthesized field's descriptor falls back to `Ljava/lang/Object;`.
This is fine in practice — every consumer found (`Field.get`, AssertJ's
extractors) reads reflectively without re-validating the declared type.

Verified via `apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`
against a fresh release build (JIT on):

| Class | Before | After |
|---|---|---|
| `HttpServiceClientAutoConfigurationTests` | FAIL 2/7 | **PASS 7/7** |
| `RestClientObservationAutoConfigurationWithoutMetricsTests` | HANG | FAIL 1/1 (new residual, see below) |
| `RestTemplateObservationAutoConfigurationWithoutMetricsTests` | HANG | FAIL 1/1 (new residual, see below) |
| `ReactiveHttpServiceClientAutoConfigurationTests` (`module/spring-boot-webclient`) | FAIL 5/7 | **PASS 7/7** |
| `ReactiveOAuth2ResourceServerAutoConfigurationTests` (`module/spring-boot-security-oauth2-resource-server`) | FAIL 3/45 (this doc under-counted at "3 of its tests"; the class has 45) | **PASS 50/50** |

(`ReactiveOAuth2ResourceServerAutoConfigurationTests`'s test count reads 50,
not 45 — JUnit's dynamic/parameterized expansion; both figures refer to the
same class.)

## New residual discovered while verifying Issue A

Once Issue A stopped masking these two classes behind an infinite hang,
they now run to completion and both fail with a real, unrelated error:
`BeanDefinitionOverrideException` for bean `observationRegistry`. This is
**not** an arg$1/lambda-reflection issue and not the livelock — it reproduces
identically with `--nojit`, and the exact same `.withBean(ObservationRegistry
.class, TestObservationRegistry::create)` + `@ConditionalOnMissingBean`
pattern passes cleanly in the sibling test that lacks
`@ClassPathExclusions("micrometer-core-*.jar")`
(`RestClientObservationAutoConfigurationTests`, 5/5 PASS). Tracked as a new,
separate, open issue:
[`observationregistry-conditionalonmissingbean-classpathexclusions.md`](../../known-issues/springboot/observationregistry-conditionalonmissingbean-classpathexclusions.md).

## Original symptom (superseded by Resolution above)

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
first log line. Root-caused and fixed as part of
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md)
— see that doc for the full mechanism (a foreign-function downcall adapter
fast path selected solely by method name, without proving the receiver was
a `MethodHandle`).

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
       org.assertj.core.util.introspection.IntrospectionError.<init>(IntrospectionError.java:62)
       org.assertj.core.util.introspection.IntrospectionError.<init>(IntrospectionError.java:52)
     Caused by: java.lang.IllegalArgumentException: Cannot locate field arg$1 on class org.springframework.web.service.invoker.HttpServiceMethod$ExchangeResponseFunction$$Lambda/0x8000031a
       org.assertj.core.util.introspection.FieldUtils.readField(FieldUtils.java:171)
```

Also independently reproduced (byte-identical `arg$1` shape) in
`module/spring-boot-webclient`'s `ReactiveHttpServiceClientAutoConfigurationTests`
and `module/spring-boot-security-oauth2-resource-server`'s
`ReactiveOAuth2ResourceServerAutoConfigurationTests` — confirming this was a
general lambda-captured-field reflection gap, not specific to
`HttpServiceMethod`'s lambdas. See Resolution above for the fix and full
verification across all 3 modules.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests` |
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests` |
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.service.HttpServiceClientAutoConfigurationTests` |
| `module/spring-boot-webclient` | `org.springframework.boot.webclient.autoconfigure.service.ReactiveHttpServiceClientAutoConfigurationTests` |
| `module/spring-boot-security-oauth2-resource-server` | `org.springframework.boot.security.oauth2.server.resource.autoconfigure.reactive.ReactiveOAuth2ResourceServerAutoConfigurationTests` |
