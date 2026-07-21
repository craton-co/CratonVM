# `JacksonAutoConfigurationTests` — severe (600s+) slowdown, not a hang/deadlock

**Status: OPEN — root cause narrowed 2026-07-21 (see "Root cause narrowed"
below), not fixed.** Originally found 2026-07-20, split out of
`docs/known-issues/springboot/otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang.md`
(that doc's root cause is FIXED; this class was miscategorized into it).
This update corrects the original doc's test-count claim (**74** total test
methods, not 6 — see below) and adds a much sharper root-cause finding:
**JUnit5's own reflective test-execution machinery, not Spring/Jackson bean
creation, is the dominant cost.**

## Symptom

`module/spring-boot-jackson`'s `org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests`
does not finish within 600s (10 minutes) — the original 2026-07-17 report only tested
up to shard timeout (unknown, but far under 600s) and concluded "HANG" from an empty
`.out.log` and a `.err.log` ending at a Mockito self-attach log line, coincidentally the
same last-line shape as the genuine `OtlpMetricsPropertiesConfigAdapterTests`
infinite-recursion bug (see the FIXED doc above) — but this class's cause is unrelated.

**Test count correction**: the class actually has **74** test methods — 30
plain `@Test` and 44 `@ParameterizedTest` (43 of those parameterized by the
3-value `MapperType` enum: CBOR/JSON/XML — so ~159 real test invocations once
parameterization is expanded), not "6 test methods" as originally recorded.
That original number was either a miscount or referred to a different subset;
current `dev` (confirmed at `65c6021f9`+) has 74.

## Root cause narrowed (2026-07-21): JUnit5 execution-machinery overhead, not Spring/Jackson bean creation

Confirmed via two independent techniques (see
`[[reference_cpu_sample_deadlock_vs_slow_technique]]` and
`[[reference_stack_dump_on_timeout_watchdog_diagnosis]]` in project memory)
this is **not** a hang/deadlock/infinite-recursion — the process makes real,
continuous forward progress. The new finding is *where* the time actually
goes, isolated with a decisive A/B:

1. **A hand-rolled repro that calls the exact same Spring/Jackson code
   directly** (`ApplicationContextRunner` + `AutoConfigurations.of(JacksonAutoConfiguration.class)`,
   then `assertThat(context).hasSingleBean(...)` — the literal body of
   `definesFactory`/`definesMapper`/`definesMapperBuilder`, for all 3
   `MapperType` values, 9 assertions total, **but invoked from a plain
   `main()`, bypassing JUnit5's `Launcher`/Jupiter engine entirely**)
   completes on CratonVM in **~22s total (~2.5s/assertion)** — consistent
   with the same systemic ~40-75x-vs-HotSpot per-call dispatch overhead
   documented in
   [`oauth2resourceserverautoconfigurationtests-severe-slowdown.md`](oauth2resourceserverautoconfigurationtests-severe-slowdown.md)
   (HotSpot baseline for the same 9 assertions: ~1.5s total, ~40-55ms each
   after warmup). Not fast, but nowhere near a 600s-class problem — at this
   rate 159 real invocations would total roughly 400s, under the timeout.
2. **The same class run through the real JUnit5 launcher**
   (`apps/spring-boot-suite-runner`'s `SbRunner`, i.e. exactly what the
   suite runner does) does not even finish the **first** parameterized
   invocation of `definesMapper` within a 60-67s `--stack-dump-on-timeout`
   window. ~8,069 stack-dump snapshots were captured in that window (the
   watchdog re-samples on every interpreter safepoint while the target
   keeps making progress, not a fixed single dump — see
   [[reference_stack_dump_on_timeout_watchdog_diagnosis]]); roughly the
   first half of those snapshots (by dump count, a rough proxy for time
   share since dumps fire at a roughly steady safepoint-polling rate) show
   the thread still deep inside creating a single bean
   (`JacksonAutoConfiguration$JacksonJsonCustomizerConfiguration.standardJsonMapperBuilderCustomizer`,
   reached via ordinary `AbstractAutowireCapableBeanFactory` factory-method
   instantiation — nothing unusual about the bean itself, see its trivial
   source below), before the stack finally unwinds (155→~65 frames) into
   JUnit5's own `NodeTestTask`/`ConditionEvaluator`/`TemplateExecutor`
   machinery for the *next* dynamic test invocation. Not frozen at one `pc`
   — genuinely still executing — just an order of magnitude slower than
   the equivalent hand-rolled call for the *same* bean-creation work.

**Conclusion**: the Spring/Jackson-level work itself (context refresh, bean
creation, `hasSingleBean` type matching) pays the same systemic ~40-75x
per-call dispatch tax documented elsewhere in this repo (see
[[reference_hashmap_native_call_dispatch_overhead_20260711]],
[[reference_jit_invoke_cache_thrash_dispatch_heavy]], the `project_wire_tiered_manager`
initiative) and is *not*, by itself, catastrophic — my 9-assertion hand-rolled
repro proves that path alone stays well under the suite timeout even at 159
invocations. The **additional**, much larger multiplier comes from routing
those same invocations through JUnit5's own reflective execution machinery
(`InterceptingExecutableInvoker`/`InvocationInterceptorChain`, extension
resolution, `@ParameterizedTest`/`@EnumSource` argument-provider machinery,
per-test `ConditionEvaluator` calls) — itself another reflection/dispatch-call-dense
subsystem paying the exact same per-call tax, just with a much higher call
count per test than plain Spring bean creation. This matches (and sharpens)
a detail already visible in the sibling
[`oauth2resourceserverautoconfigurationtests-severe-slowdown.md`](oauth2resourceserverautoconfigurationtests-severe-slowdown.md)
investigation, whose captured stacks also spent real depth inside
`InterceptingExecutableInvoker`/`InvocationInterceptorChain` alongside the
Spring-level cost — suggesting JUnit5's own invocation machinery, not any
one library's bean-creation code, may be the dominant shared contributor
across *most* of this repo's "severe slowdown, not a hang" Spring Boot test
classes.

The `standardJsonMapperBuilderCustomizer` bean itself is unremarkable —
its `@Bean` factory method is a two-line constructor call
(`JacksonAutoConfiguration.java` — `JacksonJsonCustomizerConfiguration.standardJsonMapperBuilderCustomizer`):
```java
@Bean
StandardJsonMapperBuilderCustomizer standardJsonMapperBuilderCustomizer(ObjectProvider<JacksonModule> modules,
        AutowireCapableBeanFactory beanFactory) {
    return new StandardJsonMapperBuilderCustomizer(this.jacksonProperties, modules.stream().toList(), beanFactory);
}
```
so there is nothing Jackson-specific or pathological in that bean's own
code — it just happened to be the frame on the stack when most watchdog
samples landed, because it's one of the more expensive individual
instantiations (constructor-injecting an `ObjectProvider<JacksonModule>`,
which itself does bean-type matching) in a class whose real cost driver is
elsewhere (JUnit5 machinery), not because this bean is itself broken or
looping.

### Superseded hypotheses from the original 2026-07-20 filing

The original doc listed three unconfirmed candidates. Status now:
- "JIT hot-path denial pattern" ([[reference_hot_op_helperization_trap]]):
  not the primary driver — `--nojit` A/B testing (done for the sibling
  OAuth2ResourceServer investigation, same systemic mechanism) showed near-identical
  timing with JIT on/off for the underlying per-call overhead, so this isn't
  a missed-inlining regression so much as calls paying full dispatch cost
  regardless of tier.
- "`DefaultBindConstructorProvider$Constructors.isAutowiredPresent`/`getConstructors`
  quadratic reflection": not confirmed or refuted directly this session —
  worth re-checking specifically within the JUnit5-machinery hypothesis
  above (i.e., is this reflection call itself invoked once per test by
  JUnit5's parameter-resolution/constructor-detection extensions, scaling
  with the 159-invocation count, rather than being Jackson-specific).
- "Whether JIT is even warming up for this hot path": confirmed **not
  warming up usefully** — `CRATONVM_DBG_JIT_METHOD_STATS` (via an explicit
  `System.exit(0)`, since it's a pre-exit hook not a normal-return hook) on
  the equivalent Spring-free microbenchmark showed only the enclosing loop
  method ever gets an OSR compile; individual call targets never tier up.

## Repro

```powershell
cd C:\craton\CratonVM\apps\spring-boot-suite-runner
# module\tclass TSV: module/spring-boot-jackson\torg.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests
.\run-spring-boot-suite.ps1 -Vm craton -Jit on -TimeoutSec 600 -Parallel 1 -RunName repro-jackson-slow -ClassList <that tsv> -Exe <cratonvm.exe> -JdkHome <jdk-25>
# Times out (HANG per the runner's classification) at 600s.
```

For a live thread dump / CPU sample, add `-CratonArgs '--stack-dump-on-timeout=60'`
(single `key=value` array element — two separate elements makes PowerShell
mis-bind the second one to the script's own `-Category` parameter).

For the isolated hand-rolled A/B (Spring/Jackson cost only, no JUnit5): a
~55-line standalone `main()` (`JacksonRepro2.java` in this investigation's
scratch dir, not checked into the repo) that builds the same
`ApplicationContextRunner` + `AutoConfigurations.of(JacksonAutoConfiguration.class)`
and runs the same 9 `hasSingleBean` assertions the real test methods do,
compiled against the module's own `build/cratonvm-test-cp.txt`. Useful as a
fast (~22s) regression canary for the Spring/Jackson-level cost in isolation
from JUnit5 overhead.

## Impact

Blocks `JacksonAutoConfigurationTests` (74 tests) from ever completing in the
suite runner's normal per-class timeout window. Not correctness-affecting (no
wrong results observed, purely a throughput/latency problem). The real fix is
the same [[project_wire_tiered_manager]] systemic dispatch-overhead
initiative referenced by the sibling OAuth2ResourceServer doc — not attempted
here for the same reason (hot, shared, correctness-critical dispatch/tier-up
machinery needs that initiative's own established validation rigor, not a
speculative single-session patch). A future session with bandwidth to
profile JUnit5's own `InterceptingExecutableInvoker`/`InvocationInterceptorChain`/
extension-resolution call paths specifically (rather than Spring/Jackson bean
creation) is the most promising next step, since this doc's evidence points
there as the dominant, currently-uninvestigated cost center.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-jackson` | `org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests` |

Likely affects any Spring Boot test class with a large number of
`@ParameterizedTest`/`@Test` methods relative to its timeout budget — see
[`oauth2resourceserverautoconfigurationtests-severe-slowdown.md`](oauth2resourceserverautoconfigurationtests-severe-slowdown.md)
for the sibling case (47 tests, Spring Security instead of Jackson, same
"severe slowdown, not a hang" shape, same suspected JUnit5-machinery
contribution).
