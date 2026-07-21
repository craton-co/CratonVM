# `JacksonAutoConfigurationTests` — severe (600s+) slowdown, not a hang/deadlock

**Status: OPEN — root cause narrowed 2026-07-21, refined further same day
after isolating JUnit5's own overhead directly (see "Root cause narrowed"
and "Refinement" below), not fixed.** Originally found 2026-07-20, split out
of
`docs/known-issues/springboot/otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang.md`
(that doc's root cause is FIXED; this class was miscategorized into it).
This update corrects the original doc's test-count claim (**74** total test
methods, not 6 — see below). The headline finding: neither Spring/Jackson
bean creation nor JUnit5's own execution machinery is individually
catastrophic in isolation — the two appear to **compound multiplicatively**
when nested together, which is what actually produces the 600s+ wall time.

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

**Conclusion (as of the initial 2026-07-21 pass)**: the Spring/Jackson-level
work itself (context refresh, bean creation, `hasSingleBean` type matching)
pays the same systemic ~40-75x per-call dispatch tax documented elsewhere in
this repo (see [[reference_hashmap_native_call_dispatch_overhead_20260711]],
[[reference_jit_invoke_cache_thrash_dispatch_heavy]], the
`project_wire_tiered_manager` initiative) and is *not*, by itself,
catastrophic — the 9-assertion hand-rolled repro proves that path alone
stays well under the suite timeout even at 159 invocations. The gap was
provisionally attributed to JUnit5's own reflective execution machinery —
**see the refinement below, which tests that attribution directly and finds
it's only part of the story.**

## Refinement (2026-07-21, same day): isolating JUnit5 alone shows it's ~8x, not the whole gap — the two costs compound multiplicatively

Built a third, even more isolated probe: a trivial Spring-free/Jackson-free
test class (`JUnit5MachineryProbe.java`) with the *same shape* as the real
class — 30 plain `@Test` methods + 44 `@ParameterizedTest` methods each
parameterized by 3 values (`@ValueSource(ints = {1,2,3})`, mirroring
`MapperType`'s 3 values) — 162 total invocations, each doing nothing but
trivial integer arithmetic. Ran this through the **real** JUnit5 `Launcher`
(`LauncherDiscoveryRequestBuilder` + `selectClass` + `launcher.execute()`,
the exact same API `SbRunner`/the suite runner uses), with precise
`System.currentTimeMillis()` timing around `launcher.execute()`:

| | HotSpot | CratonVM | Ratio |
|---|---|---|---|
| 162 no-op test invocations, `launcher.execute()` | 667ms (~4.1ms/test) | 5,555ms (~34.3ms/test) | **~8.3x** |

**This refutes "JUnit5 machinery alone is the dominant cost"** as originally
concluded above — 8.3x is much smaller than the ~40-75x raw per-call
dispatch-overhead baseline, and nowhere near enough on its own to explain a
process that doesn't finish even one parameterized invocation of a real test
in 60-67s. JUnit5's own machinery, running genuinely empty test bodies, is
*not* catastrophically slow on CratonVM.

**But the numbers reconcile cleanly if the two costs compound
multiplicatively rather than adding**: the hand-rolled Spring/Jackson-only
repro cost ~2.47s/assertion (22,218ms / 9). The real class's `--stack-dump-on-timeout=60`
run got through roughly one third of its 60-67s window per parameterized
invocation of `definesMapper` (3 `MapperType` values sharing that window) —
call it **~20s/real invocation**. `2.47s × 8.3 ≈ 20.5s` — matching the
observed real-invocation estimate almost exactly. That is: JUnit5's own
per-invocation overhead, when it *wraps* a test body that itself makes many
Spring/Jackson native/reflective calls, doesn't just add its own ~8x-vs-HotSpot
cost on top — it appears to **multiply** the wrapped code's own per-call tax,
plausibly because JUnit5's `InterceptingExecutableInvoker`/`InvocationInterceptorChain`
adds several dozen extra frames to the call stack for the *entire duration*
of the wrapped test body, and CratonVM's per-native-call machinery
(conservative JIT-frame root scanning, in particular — see
[[reference_hashmap_native_call_dispatch_overhead_20260711]]) plausibly
scales with stack depth/frame count, so every one of the many Spring/Jackson
native calls *inside* the wrapped body pays a larger tax than the same call
would sitting at a shallower stack depth outside JUnit5's wrapping.

**Not confirmed directly** — this multiplicative-compounding explanation is
inferred from the arithmetic lining up (2.47s × 8.3 ≈ 20.5s ≈ the observed
~20s), not from direct profiling proof that conservative root-scanning cost
(or any other specific mechanism) actually scales with stack depth in this
codebase. That would be the natural next step: instrument or profile a
single Spring/Jackson native call's cost at two different call-stack depths
(e.g. called directly from `main()` vs. called from inside 50 extra
pass-through Java frames) to confirm whether frame-count-dependent cost is
the actual mechanism, independent of JUnit5 specifically.

At **~20s/real invocation × 159 real invocations ≈ 3,180s** — this now
overshoots the observed 600s+ timeout by 5x rather than sitting right at the
edge, which is itself informative: it suggests either (a) not every
invocation is as expensive as `definesMapper`'s (plausible — some of the 74
methods do much less work per invocation), or (b) the "~20s per invocation"
estimate, derived from a single 60-67s sample window, is itself a rough
upper-bound proxy rather than a tight per-invocation average. Either way,
the qualitative conclusion — cumulative real invocations, each paying a
compounded (not merely additive) JUnit5×Spring/Jackson tax, comfortably
exceed 600s — holds without needing any non-termination.

This finding **generalizes**: it predicts that ANY Spring Boot test class
combining (a) many test invocations and (b) reflection/native-call-heavy
work per invocation (which describes most Spring context-refresh-per-test
patterns) will show a similar compounding effect, not just Jackson or
OAuth2ResourceServer specifically — worth checking the "test-method-count ×
measured-per-context-cost" arithmetic assumes ADDITIVE JUnit5 overhead is
being systematically UNDER-estimated across this repo's other
"severe-slowdown" docs if they used a JUnit5-bypassing hand-rolled repro
(as both this doc and
[`oauth2resourceserverautoconfigurationtests-severe-slowdown.md`](oauth2resourceserverautoconfigurationtests-severe-slowdown.md)
did) rather than measuring real JUnit5-launched invocations directly.

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

For the isolated JUnit5-machinery-only A/B (no Spring/Jackson at all):
`JUnit5MachineryProbe.java` (162 trivial `@Test`/`@ParameterizedTest`
invocations, no assertions beyond integer arithmetic) +
`JUnit5ProbeRunner.java` (a ~30-line `main()` driving the real
`org.junit.platform.launcher.Launcher` API directly), compiled against just
the JUnit Platform/Jupiter jars — no Spring Boot module classpath needed at
all, so this compiles and runs in seconds even outside a Gradle-built
module. Both kept in this investigation's scratch dir, not checked into the
repo; regenerate from this doc's description if needed (a `gen.sh` heredoc
approach produced the 74-method probe class quickly).

## Impact

Blocks `JacksonAutoConfigurationTests` (74 tests) from ever completing in the
suite runner's normal per-class timeout window. Not correctness-affecting (no
wrong results observed, purely a throughput/latency problem). The real fix is
the same [[project_wire_tiered_manager]] systemic dispatch-overhead
initiative referenced by the sibling OAuth2ResourceServer doc — not attempted
here for the same reason (hot, shared, correctness-critical dispatch/tier-up
machinery needs that initiative's own established validation rigor, not a
speculative single-session patch). The most promising concrete next step,
per the "Refinement" section above, is confirming (or refuting) whether
per-native-call cost in this codebase actually scales with Java call-stack
depth/frame count — if so, that's a single, well-scoped mechanism that would
explain the JUnit5×Spring/Jackson compounding effect (and plausibly other
"severe slowdown" classes) without needing to profile JUnit5's own machinery
in more detail (which, per the isolated 8.3x measurement here, is not
pathological on its own).

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
