# Spring Framework AOT runner classpath and batch isolation

> **STATUS: RESOLVED (2026-07-29).** The four reported rows were not four
> remaining VM semantic failures. One was a Linux fixture-classpath mismatch;
> the other three were cascading discovery OOMs from sharing one CratonVM heap
> across multiple heavy test classes. The runner now matches Gradle's owning-
> module classpath and uses one VM per class by default. Final reconciliation
> also exposed and fixed a newly merged StackWalker retain-access regression.

## Reported result

The 2026-07-29 residual rerun at
`apps/spring-suite-runner/out/rerun2sh-20260727/sh2b-jit-real-failed-20260729-140754`
reported:

- `FileNativeConfigurationWriterTests`: 7 found, 2 passed, 5 failed with
  `Unexpected: comment`.
- `BeanDefinitionMethodGeneratorTests`: load error,
  `OutOfMemoryError: Java heap space (new_object class_id 353 fields 19)`.
- `BeanRegistrationsAotContributionTests`: the same immediate load error.
- `InstanceSupplierCodeGeneratorTests`: the same immediate load error.

The three load errors occurred consecutively in one four-class batch after
previous classes had consumed the shared 2 GiB heap. The second and third AOT
classes failed in 3 ms, before discovery could run, which identified them as
cascades rather than independent linkage failures.

## Root causes

### Owning-module main output missing from the classpath

`spring-core/build/cratonvm-testcp.txt` begins with test outputs and then the
packaged `spring-core-7.1.0-SNAPSHOT.jar`; it does not contain
`spring-core/build/classes/java/main`. As a result `SpringVersion` came from a
versioned manifest JAR, `SpringVersion.getVersion()` was non-null, and Spring's
native configuration writer emitted a top-level `comment` that the
`NON_EXTENSIBLE` assertions correctly rejected.

This was fixture behavior, not a CratonVM divergence. On the original
classpath, HotSpot and CratonVM both reported 2/7 with the same five
`Unexpected: comment` failures. Prepending the owning module's Java/Kotlin main
outputs and resources made both VMs report 7/7. The runner now reproduces
Gradle's test-runtime ordering in `one.sh`, `hs.sh`, and `run-suite.sh`.

### Cross-class heap state in batched reruns

The default rerun placed four classes in one CratonVM process. Spring's AOT
compiler, instrumentation, and class-loader state is deliberately global and
allocation-heavy; later classes inherited the earlier classes' depleted heap.
All three reported AOT classes pass when launched in clean VMs. Correctness
runs therefore default to `BATCH=1` in both `run-suite.sh` and `runlist.sh`.
Explicit batching remains available as an opt-in throughput experiment.

### Focused slice counter advanced before the requested start

Validation also found that `--start N --count C` incremented `C` on rows before
`N`, so `--start 51 --count 1` selected zero classes. The slice now increments
only after a row is selected. The focused runner regression changed from
`0 classes` to exactly one class and produced the expected 7/7 result.

### StackWalker retain access was scoped to the callback, not the frame

During final reconciliation, an incoming StackWalker contract change made all
34 `BeanDefinitionMethodGeneratorTests` fail in both modes while initializing
Log4j. Log4j legally returns an `Optional<StackFrame>` from `StackWalker.walk()`
and calls `getDeclaringClass()` after the walk callback returns. CratonVM stored
the `RETAIN_CLASS_REFERENCE` authorization in a thread-local flag and cleared
it when the callback returned, so the retained frame then threw
`UnsupportedOperationException: No access to RETAIN_CLASS_REFERENCE`.

The synthetic frame now carries a persistent retain bit in slot 7. Both
`getDeclaringClass()` and `getMethodType()` read that per-frame bit, so frames
remain valid after `walk()` returns while frames from a default walker still
throw as required. The real JDK 25 `ClassFrameInfo` carrier also uses the
correct `RETAIN_CLASS_REF_BIT` mask, `0x08000000`; the prior `0x01` mask did not
match JDK 25's constructor or `retainClassRef()` bytecode.

A focused HotSpot/CratonVM probe verified retained one-option and set-option
walkers, a rejected default walker, and successful Log4j `LogManager`
initialization in JIT and `--nojit` modes.

## Validation

Remote host: `victor@20.83.144.174`, real JDK 25, isolated worktree
`/data/data/wt-spring-aot-writer-beans-20260729-019fae8e`.

Validated binary:
`localbin/cratonvm-spring-aot-writer-beans-final-48cdde50-019fae8e`, built from
`48cdde50b876bbcfd5c9179cf547af3bd8569e05`, SHA-256
`809022a46fa9f6123395609615c031b7cb8411265b8eafd0baefed67def2572a`.

| class | JIT | `--nojit` |
|---|---:|---:|
| `FileNativeConfigurationWriterTests` | 7/7 | 7/7 |
| `BeanDefinitionMethodGeneratorTests` | 34/34 | 34/34 |
| `BeanRegistrationsAotContributionTests` | 14/14 | 14/14 |
| `InstanceSupplierCodeGeneratorTests` | 26 found, 24 passed, 2 skipped | 26 found, 24 passed, 2 skipped |

Each mode totals 4 classes, 81 tests found, 79 passed, 0 failed, 2 skipped,
0 aborted. All eight process exits were zero and all eight logs had zero
`FAILCAUSE`, `LOADERR`, abort, panic, or segmentation-fault markers.

The long `BeanRegistrationsAotContributionTests` full-class runs completed in
7,159,294 ms with JIT and 7,767,466 ms with `--nojit`; they were not replaced by
method probes or inferred from process liveness.
