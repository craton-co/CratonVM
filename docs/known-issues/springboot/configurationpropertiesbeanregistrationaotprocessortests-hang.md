# `ConfigurationPropertiesBeanRegistrationAotProcessorTests` — genuine HANG (OPEN)

Class: `org.springframework.boot.context.properties.ConfigurationPropertiesBeanRegistrationAotProcessorTests`
(`core/spring-boot`). Found investigating Spring Boot core39 residual Cluster
A (`spring-boot-core39-residual-clusters-20260723.md`),
2026-07-24. Worktree `springboot-core39-clusterA-20260723`.

## Status: confirmed real hang, not a throughput/timeout issue

Unlike its cluster-mate `ConfigurationPropertySourcesTests` (also originally
reported as a HANG, now confirmed to be CPU-bound-but-completing — see the
residual-clusters doc), this class does **not** complete even with a 7200s
(2-hour) budget, and **zero** of its 9 `@Test` methods report a result in
that window (the class's own stdout shows only three rounds of Hibernate
Validator initialization banners, no JUnit test-result lines at all).

## Evidence gathered

- **CPU-sampled, not deadlocked**: `Get-Process`'s `TotalProcessorTime` grew
  ~7.9s per 8s of wall-clock while hung — the process is genuinely computing
  the entire time, not parked on a lock/IO.
- **Stack-dump-on-timeout samples at 60s, 400s, 1100s, and (implicitly) up to
  7200s all land at the identical Java-level location**:
  ```
  top=org/hibernate/validator/internal/metadata/aggregated/BeanMetaDataImpl.getClassLevelConstraintsAsDescriptors@19
    <- BeanMetaDataImpl.createBeanDescriptor@36
    <- BeanMetaDataImpl.getBeanDescriptor@46
  ```
  The exact same bytecode offset recurring across samples taken **hours**
  apart, with zero test methods completing in between, is much stronger
  evidence of a true stuck/looping condition than
  `ConfigurationPropertySourcesTests`' samples were (that class's identical
  stack locations turned out to just reflect a genuinely dominant, but
  linearly-scaling, hot call — it completed once given enough time).
- **Native-call ring buffer** repeatedly shows the last active native call as
  `java/util/Arrays.stream([Ljava/lang/Object;)Ljava/util/stream/Stream;`
  marked `STILL-IN-NATIVE`, growing from `5695ms ago` (at the 60s dump) to
  `371088ms ago` (at the 400s dump on the same run) — consistent with ONE
  invocation of `Arrays.stream` (called via
  `LenientObjectToEnumConverterFactory`-adjacent code, or from Hibernate
  Validator's own constraint iteration — not conclusively pinned) never
  returning, though a later instrumented run showed many OTHER
  `Arrays.stream`/`Arrays$ArrayList.stream()` calls completing successfully
  around the same code region — so this is not a universal
  `Arrays.stream()` breakage (ruled out, see below), just this call site /
  this specific input shape.
- **Not fixed by the `String.chars()` array-kind bug fix** (see
  `docs/internal/fixed-suite-bugs/springboot/string-chars-wrong-array-kind-and-properties-formfeed-escape-FIXED.md`)
  — rebuilt and reran with that fix in place; identical hang, identical
  stack location.
- **Not a generic Hibernate Validator bug**: a minimal standalone repro
  (`Validation.buildDefaultValidatorFactory()` +
  `validator.getConstraintsForClass(PlainBean.class)` for a plain POJO with
  no constraint annotations) completes in ~320ms total. The hang is specific
  to whatever this test class's actual scenario does differently — most
  likely the interaction between Hibernate Validator's per-class metadata
  caching and the `@CompileWithForkedClassLoader`-driven fresh classloader +
  in-process `javac` compilation this class performs for 4 of its 9 test
  methods (each spins up a full `GenericApplicationContext`, runs AOT
  processing, compiles the generated source with the real JDK compiler API,
  and loads the result through a freshly-forked classloader) — but this has
  not been conclusively pinned to a specific line.
- **Not an empty-array edge case**: `Arrays.stream(new Object[0])` and
  `Arrays.stream(SomeClass.class.getAnnotations())` (0-length case, since the
  test's sample beans carry no annotations) both complete correctly and fast
  in isolation.

## What's needed to close this

A real debugger attached to the hung process (`cdb`/`gdb`) would very likely
resolve this quickly by showing the actual native (Rust) call stack instead
of just the last-known Java frame — this environment does not currently have
one available (see `reference_cpu_sample_deadlock_vs_slow_technique` in
project memory). Failing that: bisect by instrumenting (temporarily, behind
an env-gated `eprintln!`) every native call reachable from
`BeanMetaDataImpl.getClassLevelConstraintsAsDescriptors`'s real call graph —
annotation reflection (`Class.getAnnotations`/`getDeclaredAnnotations`),
`EnumSet`/`Arrays.stream` chains, and the CGLIB/ByteBuddy-adjacent classes
involved in Hibernate Validator's bean-introspection — one native at a time,
narrowing until the exact stuck call is found. This is a multi-hour
investigation on its own; do not attempt it under time pressure.

## Reproduction

```powershell
$env:JAVA_HOME = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -SpringBootRoot 'C:\craton\CratonVM\apps\spring-boot' `
  -Vm craton -Exe <built cratonvm.exe> -JdkHome $env:JAVA_HOME `
  -ClassList <tsv with just this class> `
  -RunName <name> -Jit off -Parallel 1 -TimeoutSec 300 -CratonArgs @('--stack-dump-on-timeout=60')
```

Reproduces every time, deterministically, within the first ~60s.
