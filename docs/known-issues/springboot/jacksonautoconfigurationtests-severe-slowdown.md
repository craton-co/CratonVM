# `JacksonAutoConfigurationTests` — severe (600s+) slowdown, not a hang/deadlock

**Status: OPEN — found 2026-07-20, split out of
`docs/known-issues/springboot/otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang.md`
(that doc's root cause is FIXED; this class was miscategorized into it).**

## Symptom

`module/spring-boot-jackson`'s `org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests`
(6 test methods, each does one `contextRunner.run(...)` full Spring context bootstrap)
does not finish within 600s (10 minutes) — the original 2026-07-17 report only tested
up to shard timeout (unknown, but far under 600s) and concluded "HANG" from an empty
`.out.log` and a `.err.log` ending at a Mockito self-attach log line, coincidentally the
same last-line shape as the genuine `OtlpMetricsPropertiesConfigAdapterTests`
infinite-recursion bug (see the FIXED doc above) — but this class's cause is unrelated.

## Root cause — NOT a hang/deadlock

Confirmed via two independent techniques (see
`[[reference_cpu_sample_deadlock_vs_slow_technique]]` in project memory):

1. `--stack-dump-on-timeout` watchdog dumps (armed at 90s) show a normal,
   non-repeating, slowly-progressing (124 → 125 → 125 frames across 3 dumps 90s apart)
   Spring context-refresh call stack — ordinary bean scanning / condition evaluation
   (`OnBeanCondition.getMatchingBeans` → `getBeanDefinitionsForType` →
   `DefaultBindConstructorProvider$Constructors.isAutowiredPresent` reflection work).
   **Zero** Mockito/ByteBuddy frames anywhere in any dump — this class does use
   `mock(ObjectMapper.class)` / `mock(JsonMapperBuilderCustomizer.class)` etc., but
   nothing in the observed stack ever touches Mockito's advice machinery, so the
   `isOverridden` bug fixed in the sibling doc cannot be the cause here.
2. CPU-sampled the live process's OS threads twice, 20s apart, via
   `(Get-Process -Id $pid).Threads | Select Id,TotalProcessorTime`: the worker thread's
   `TotalProcessorTime` grew by ~20s across the 20s wall-clock window — pegged at ~100%
   CPU. A parked/deadlocked thread would show near-zero CPU growth; this does not. The
   process is genuinely computing, just far too slowly.

So this is a severe (100-1000x vs HotSpot, which runs this class in low single-digit
seconds) performance pathology somewhere in this class's bean-condition-evaluation hot
path, not a correctness bug, deadlock, or infinite loop with a fixed cycle. Root cause
not yet isolated — candidates worth checking first (not yet attempted):
- Whether this matches the general "JIT hot-path denial" pattern documented in
  `[[reference_hot_op_helperization_trap]]` (a silent 5-50x interpreter/JIT regression
  class already seen elsewhere).
- Whether `DefaultBindConstructorProvider$Constructors.isAutowiredPresent` /
  `getConstructors` (the "top" frame in the thread summary) does something
  reflection-heavy per-bean that's quadratic or otherwise pathological under
  CratonVM's reflection implementation specifically (e.g. repeated
  `Class.getDeclaredConstructors()` without caching, combined with a slow
  reflection-metadata path).
- Whether JIT is even warming up for this hot path at all (`CRATONVM_DBG_JIT_DISASM`),
  since this class's 6 tests each do a FULL fresh context refresh — if JIT profiling
  state resets or never triggers per-run, the interpreter alone would need to carry
  the whole reflection-heavy bootstrap every time.

## Repro

```powershell
cd C:\craton\CratonVM\apps\spring-boot-suite-runner
# module\tclass TSV: module/spring-boot-jackson\torg.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests
.\run-spring-boot-suite.ps1 -Vm craton -Jit on -TimeoutSec 600 -Parallel 1 -RunName repro-jackson-slow -ClassList <that tsv> -Exe <cratonvm.exe> -JdkHome <jdk-25>
# Times out (HANG per the runner's classification) at 600s.
```

For a live thread dump / CPU sample, launch `cratonvm.exe` directly (not through the
runner, which disables the internal watchdog by default) with
`--stack-dump-on-timeout 90 --jar <pathing-jar-with-Main-Class:SbRunner>
org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests`, working
directory `module/spring-boot-jackson`.

## Impact

Blocks `JacksonAutoConfigurationTests` (6 tests) from ever completing in the suite
runner's normal per-class timeout window. Not correctness-affecting (no wrong results
observed, purely a throughput/latency problem) but severe enough that it needs its own
investigation — a future session should profile/instrument the hot reflection path
directly rather than assume "just raise the timeout" will work (600s wasn't enough).
