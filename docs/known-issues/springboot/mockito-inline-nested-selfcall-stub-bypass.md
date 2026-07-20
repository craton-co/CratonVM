# Mockito inline mock maker: a stub on a method reached through a nested self-call chain (interface default → utility helper → target method) is silently bypassed

**Status: OPEN — found 2026-07-20, while fixing
`docs/internal/springboot/otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang-FIXED.md`.**

## Symptom

`OtlpMetricsPropertiesConfigAdapterTests.whenPropertiesUrlIsNotSetThenUseOtlpConfigUrlAsFallback`:

```java
OtlpMetricsPropertiesConfigAdapter adapter = spy(createAdapter());
given(adapter.get("management.otlp.metrics.export.url")).willReturn("https://my-endpoint/v1/metrics");
assertThat(adapter.url()).isEqualTo("https://my-endpoint/v1/metrics");
// actual: "http://localhost:4318/v1/metrics" (the hardcoded default — the stub never fired)
```

This is the one residual failure out of 30 after fixing the `isOverridden`
infinite-recursion bug — no hang, no crash, just a wrong value.

## Root cause (partially isolated, not yet fixed)

`adapter.url()`'s call chain to reach `get(String)` is (each arrow a separate,
individually advice-woven method call, since Mockito's inline maker redefines
every class/method in the hierarchy):

```
OtlpMetricsPropertiesConfigAdapter.url()                    [redefined, overrides]
  -> PropertiesConfigAdapter.obtain(getter, fallback)        [redefined, `protected final`]
    -> fallback.get()  (a synthetic lambda backing `OtlpConfig.super::url`)
      -> OtlpConfig.url()  (interface default, invokespecial) [redefined]
        -> PropertyValidator.getUrlString(config, "url")     [NOT redefined — 3rd-party util]
          -> config.get(fullKey)                              [redefined, target of the stub]
```

Confirmed via a minimal standalone repro
(`docs/known-issues/springboot/repros/mockito-nested-selfcall-stub-bypass-RealProbe.java`,
uses the real Spring Boot / Micrometer classes directly, no JUnit needed — must be
placed at
`org/springframework/boot/micrometer/metrics/autoconfigure/export/otlp/RealProbe.java`
on the classpath since `OtlpMetricsPropertiesConfigAdapter` is package-private):

- A **direct** call `spyAdapter.get(key)` (from ordinary test code, any declared type
  — concrete or interface-typed reference) correctly returns the stubbed value.
- The **same call**, reached through the 4-layer nested chain above (starting from
  `spyAdapter.url()`), never even reaches `MockMethodAdvice.handle()` at all — confirmed
  with `Mockito.verify(spyAdapter, atLeastOnce()).get(key)` throwing
  `WantedButNotInvoked` immediately after `spyAdapter.url()` runs. So this is not a
  stub-matching/argument-equality bug (verified separately that Mockito's invocation
  recording and argument matching both work correctly for directly-observed calls) — the
  advice's own interception is being skipped entirely for the nested `get()` call.
- `native_mockito_mock_method_advice_is_overridden` was checked live at this exact call
  (`isOverridden(spyAdapter, PushRegistryPropertiesConfigAdapter.get) = false`, i.e.
  "not overridden, should intercept") — correct. So `isOverridden` is NOT the culprit
  here; the bug this doc tracks is different from (and downstream of) the one fixed in
  `otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang-FIXED.md`.

Leading hypothesis (not yet confirmed against CratonVM source): Mockito's
`MockMethodAdvice.SelfCallInfo` — a single `ThreadLocal<Object>` that
`RealMethodCall`/`tryInvoke` sets to the receiver instance right before reflectively
re-invoking a method "for real" (`callRealMethod()`), and whose prologue check
(`checkSelfCall`) is meant to consume it **once** (match → clear → skip interception for
that one call) so that any *further* nested call goes back through normal interception.
With 4-5 real-method layers in this chain (`url()`, `obtain()` — itself `final`, needing
Mockito's own final-stripping — a private/synthetic lambda method backing
`OtlpConfig.super::url`, `OtlpConfig.url()`, and finally `get()`), each layer does its
own intercept → `CallsRealMethods` → set-`SelfCallInfo` → reflect-invoke-real →
consume-`SelfCallInfo` → run-real-body cycle. Something in that chain, on CratonVM,
either consumes/sets the flag at the wrong point or otherwise causes `get()`'s own
prologue to treat itself as still "inside a suspended self-call" and skip its own
`isMocked`/`isOverridden` checks (and hence `dispatcher.handle()`) entirely — which
would explain zero recording. This has NOT been confirmed against a live trace of
`SelfCallInfo`/`checkSelfCall` on CratonVM (would need `CRATONVM_DBG_*`-gated tracing of
the interpreter's `ifne`/`ifeq`/`invokevirtual` handling for
`MockMethodAdvice$SelfCallInfo.checkSelfCall` across all 4-5 layers of this specific
chain, or an equivalent live probe) — left for a follow-up session.

## Repro

See `docs/known-issues/springboot/repros/mockito-nested-selfcall-stub-bypass-RealProbe.java`.
Compile it into
`org/springframework/boot/micrometer/metrics/autoconfigure/export/otlp/RealProbe.class`
and run against the `spring-boot-micrometer-metrics` module's test classpath (e.g.
`module/spring-boot-micrometer-metrics/build/cratonvm-test-cp.txt`) plus that class
directory — no Spring context, no JUnit required. Minimal form:

```java
OtlpMetricsPropertiesConfigAdapter adapter = new OtlpMetricsPropertiesConfigAdapter(...);
OtlpMetricsPropertiesConfigAdapter spyAdapter = spy(adapter);
given(spyAdapter.get("management.otlp.metrics.export.url")).willReturn("https://my-endpoint/v1/metrics");
System.out.println(spyAdapter.url()); // wrong: prints the hardcoded default, not the stub
```

Reproduces with `--nojit` too (not JIT-specific, same as the sibling `isOverridden` bug).

## Impact

Narrow so far — only 1/30 tests in `OtlpMetricsPropertiesConfigAdapterTests`. Likely
affects any Mockito spy/mock test where a stubbed method is reached only through a
multi-layer self-call chain involving a `final` method and/or an interface-default
`super` call, which is a fairly specific (if not rare) shape. Sweep other Spring Boot
suite failures for the same signature (`given(...)` stub silently not honored, no
exception, real-method value returned instead) before assuming this is isolated.
