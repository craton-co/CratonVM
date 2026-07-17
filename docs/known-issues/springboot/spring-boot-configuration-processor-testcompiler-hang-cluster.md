# `spring-boot-configuration-processor` test classes HANG at the suite timeout, no output at all

**Status: OPEN — found 2026-07-17**

## Symptom

All 7 classes in `configuration-metadata/spring-boot-configuration-processor`
assigned in this rerun batch HANG to the per-class timeout (observed run
window ~280s, consistent with a ~300s timeout). None produce **any**
stdout (`.out.log` is 0 bytes for every one of them — not even the JUnit
"Test run finished" banner, so the process never reaches the point where
JUnit prints its summary). The only `.err.log` content is `WARN`-level
noise, repeating for the whole run at a slow, GC-like cadence
(~20-48s between bursts):

```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matching receiver type) obj=0x1e4373682b8 index=0 num_slots=0 class_id=ClassId(707) class_name=org/junit/jupiter/engine/execution/InterceptingExecutableInvoker real_field_count=Some(0)
```
repeated ~17 times (2 per burst, alternating between exactly the same 2
object addresses) between 19:52:02 and 19:56:43 in the representative log
below, then the process is killed with no further output.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/configuration-metadata_spring-boot-configuration-processor.org.springframework.boot.configurat-c1cf079ff131.err.log`
(`ConfigurationMetadataAnnotationProcessorTests`; the other 6 classes —
`ConstructorParameterPropertyDescriptorTests`, `EndpointMetadataGenerationTests`,
`JavaBeanPropertyDescriptorTests`, `LombokPropertyDescriptorTests`,
`MergeMetadataGenerationTests`, `PropertyDescriptorResolverTests` — show the
identical shape, just fewer/more repetitions and different object
addresses/`ClassId`s).

## Root cause

**Not pinned.** The recurring `gc::guard` warning is almost certainly a red
herring, not the hang's cause — it is explicitly documented as benign in
its own source
(`gc/src/gen_heap.rs:2067-2082`, the `is_true_undersized == false` /
`tracing::warn!` branch: "class layout is correct... returns a benign null
read instead of a SIGSEGV") and the same warning, at the same target/class
(`InterceptingExecutableInvoker`/`InvocationInterceptorChain`, `real_field_count=Some(0)`,
i.e. legitimately 0-field JUnit-internal helper classes), appears in
**every other class in this batch that completes normally** — including
fast, passing-mostly runs like `RecordableServerHttpRequestTests` (finishes
in 1.6s, 4/5 tests pass) and 20s/121s runs elsewhere in `spring-boot-webflux`
— so its mere presence does not explain why these particular 7 classes never
progress. The gap here is that in the config-processor classes it recurs
**for the full run with no other output ever appearing**, where in the
passing/failing classes it appears a handful of times near startup (JUnit
discovery) and then execution proceeds normally to a JUnit summary.

This module's tests use Spring's `org.springframework.core.test.tools.TestCompiler`,
which invokes the real `javax.tools.JavaCompiler` in-memory to compile
fixture sources against the real `spring-boot-configuration-processor`
annotation processor — a mechanism already documented as **substantially
slower under CratonVM than HotSpot** in
[`../../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md`](../../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md)
(fixed 2026-07-13 for a *different* symptom — a `CompilationException`
reached after ~300-1500s from an incomplete JRT classpath exposed to
`javac`). Four of that doc's originally-affected classes overlap with this
batch (`JavaBeanPropertyDescriptorTests`, `MergeMetadataGenerationTests`,
`LombokPropertyDescriptorTests`, `PropertyDescriptorResolverTests`); the
other 3 here (`ConfigurationMetadataAnnotationProcessorTests`,
`ConstructorParameterPropertyDescriptorTests`,
`EndpointMetadataGenerationTests`) are new to this cluster's history but
use the same `TestCompiler` machinery. Given the prior finding that this
compile path is dramatically slower under CratonVM (and that a `HANG-rerun
follow-up` note in this module's README recorded 90/150 originally-HANG
classes still not completing even at **5x** timeout), the leading — but
**unconfirmed this session** — hypothesis is that this is a continuation
or partial regression of that general TestCompiler-path slowness: these 7
classes never reach even the point of printing their first JUnit line
within the timeout window, rather than hitting a new, distinct hang
mechanism. This has **not** been verified by rerunning at a longer timeout
or by attaching a debugger to a hung process to capture actual thread
stacks — both would be the fastest way to confirm or refute it.

## Affected classes

| Module | Class |
|---|---|
| `configuration-metadata/spring-boot-configuration-processor` | `org.springframework.boot.configurationprocessor.ConfigurationMetadataAnnotationProcessorTests` |
| `configuration-metadata/spring-boot-configuration-processor` | `org.springframework.boot.configurationprocessor.ConstructorParameterPropertyDescriptorTests` |
| `configuration-metadata/spring-boot-configuration-processor` | `org.springframework.boot.configurationprocessor.EndpointMetadataGenerationTests` |
| `configuration-metadata/spring-boot-configuration-processor` | `org.springframework.boot.configurationprocessor.JavaBeanPropertyDescriptorTests` |
| `configuration-metadata/spring-boot-configuration-processor` | `org.springframework.boot.configurationprocessor.LombokPropertyDescriptorTests` |
| `configuration-metadata/spring-boot-configuration-processor` | `org.springframework.boot.configurationprocessor.MergeMetadataGenerationTests` |
| `configuration-metadata/spring-boot-configuration-processor` | `org.springframework.boot.configurationprocessor.PropertyDescriptorResolverTests` |
