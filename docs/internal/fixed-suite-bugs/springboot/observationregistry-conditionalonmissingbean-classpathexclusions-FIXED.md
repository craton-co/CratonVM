# `@ConditionalOnMissingBean` misses a user bean under `@ClassPathExclusions`'s forked classloader

**Status: FIXED — confirmed 2026-07-21 (fix landed 2026-07-20 as a side effect of an unrelated commit)**

## Resolution

Re-verified against current `dev` (`055c6634d` and later): both
`RestClientObservationAutoConfigurationWithoutMetricsTests` and
`RestTemplateObservationAutoConfigurationWithoutMetricsTests` now **PASS**
consistently — 8/8 clean runs across `-Jit on`, `-Jit off`, and 3 repeated
`-Jit on` runs, no `BeanDefinitionOverrideException`.

The fix was **not** authored for this bug. It landed on `dev` at commit
`65d738bb5` (`fix(springboot): thread loader identity through DLBF
construction and lambda static/field method-handle dispatch`), merged via
`91f514f64` while investigating a *different* residual
([`webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md`](../../known-issues/springboot/webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md)),
and was already present in the exact `dev` tip (`055c6634d`) that this doc's
own investigation session had merged its "root cause NOT found" writeup on
top of — the two sessions' branches simply hadn't cross-verified against each
other yet.

The relevant half of that commit is exactly the "genuinely interesting,
likely-tangential finding" flagged (but not confirmed causal) in this doc's
original investigation: `vm/src/runtime/interpreter.rs`'s
`InvokeSpecial`/`NewInvokeSpecial`/`GetStatic`/`PutStatic` lambda-dispatch
arms — i.e. method references with **no receiver** to anchor loader identity
on, such as `TestObservationRegistry::create` (a static factory reference,
used by this doc's `.withBean(ObservationRegistry.class,
TestObservationRegistry::create)`) — previously fell back to a **global**
`class_manager.load_class(name)` on a cache miss instead of driving the host
loader's own `loadClass`. This is precisely the non-loader-aware SAM dispatch
gap this doc's original session found but couldn't confirm as causal (the
explicit `ObservationRegistry.class` type literal used for the
`@ConditionalOnMissingBean` condition check was already shown loader-correct;
the miss was in the supplier lambda's *own* identity/dispatch instead — which
turns out to matter after all, likely through the bean's actual
instantiation/registration path rather than the condition-evaluation path
this doc's traces covered).

The commit's other half (`native-builtins/src/spring_startup_bootstrap.rs`'s
`get_or_create_bean_factory` no longer collapsing a context's
`DefaultListableBeanFactory` fallback construction to a loader-blind global
lookup) plausibly also contributed, since `getBeanNamesForType`'s type-match
scan runs against that same `DefaultListableBeanFactory` instance.

**No further action needed on this doc.** See the commit message on
`65d738bb5` for the full two-part fix description.

## Verification (2026-07-21)

- 2/2 PASS x4 (baseline + 2 reruns `-Jit on` + 1 `-Jit off`) for both affected
  classes, worktree `C:\craton\CratonVM-obsregistry-classpathexclusions-20260721`,
  binary `cratonvm-obsregistry-classpathexclusions.exe`.
- Full regression sweep across `module/spring-boot-restclient`,
  `module/spring-boot-webclient`, and
  `module/spring-boot-security-oauth2-resource-server` (50 classes total, the
  full set touched by the parent `spring-boot-restclient-residuals-FIXED.md`
  investigation) run to confirm no regressions — see that doc / the merge
  commit for the final tally.

## Original report (superseded by Resolution above)

`RestClientObservationAutoConfigurationWithoutMetricsTests` and
`RestTemplateObservationAutoConfigurationWithoutMetricsTests`
(`module/spring-boot-restclient`) both failed their single test with:

```
JUnit Jupiter:RestClientObservationAutoConfigurationWithoutMetricsTests:restClientCreatedWithBuilderIsInstrumented()
    => java.lang.IllegalStateException: Unstarted application context ...[startupFailure=org.springframework.beans.factory.support.BeanDefinitionOverrideException] failed to start
     Caused by: org.springframework.beans.factory.support.BeanDefinitionOverrideException: Invalid bean definition with name 'observationRegistry' defined in org.springframework.boot.micrometer.observation.autoconfigure.ObservationAutoConfiguration: @Bean definition illegally overridden by existing bean definition: Generic bean: class=io.micrometer.observation.ObservationRegistry; scope=singleton; ...
```

This was a **new** residual, discovered while verifying that the
`InterceptingExecutableInvoker` livelock fix
([`spring-boot-restclient-residuals-FIXED.md`](spring-boot-restclient-residuals-FIXED.md))
held — both classes previously HANG-timed-out before ever reaching this
code path, so this bug was invisible until the hang was fixed 2026-07-18.
Reproduced identically with `-Jit off` (`--nojit`), ruling out a JIT
speculation issue at the time.

The test context was:

```java
private final ApplicationContextRunner contextRunner = new ApplicationContextRunner()
    .withBean(ObservationRegistry.class, TestObservationRegistry::create)
    .withConfiguration(AutoConfigurations.of(ObservationAutoConfiguration.class, RestClientAutoConfiguration.class,
            RestClientObservationAutoConfiguration.class));
```

Isolated the trigger to `@ClassPathExclusions("micrometer-core-*.jar")`
(routes execution through `ModifiedClassPathExtension`'s
`ModifiedClassPathClassLoader` + nested `Launcher` re-execution) — the
sibling `RestClientObservationAutoConfigurationTests` (same bean pattern, no
`@ClassPathExclusions`) always passed cleanly. Confirmed via `-Vm hotspot`
that this was a genuine CratonVM-specific bug (real HotSpot passed 1/1 for
both classes), not an environmental/pre-existing upstream test issue.

Two full investigation sessions (2026-07-20) ruled out, with live
`CRATONVM_DBG_OBSREG=1`/`CRATONVM_FORNAME_TRACE=1` tracing evidence: a
synthetic-JDK-mode parent-delegation bug, a naive
`url_classloader_isolated_from_app` misclassification, a stale/duplicate
`ObservationRegistry` `ClassId`, and the `CRATONVM_LOADER_AWARE_RESOLUTION`
gate being off — everything CratonVM-side traced as internally consistent at
the condition-evaluation layer. The diagnostic tooling added
(`CRATONVM_DBG_OBSREG=1`, gated `eprintln!`s in `classloading/src/class_manager.rs`,
`native-builtins/src/classloader.rs`, `native-builtins/src/classloader_real.rs`,
`native-builtins/src/lang_class.rs`) is kept as a zero-cost-when-unset
diagnostic for any future loader-identity investigation in this area.

## Affected classes (now passing)

| Module | Class |
|---|---|
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests` |
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests` |
