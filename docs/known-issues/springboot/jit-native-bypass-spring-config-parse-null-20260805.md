# A JIT-only regression makes Spring's `MergedAnnotations.from(..)` return null, failing configuration-class parsing across five Spring Boot classes

**Status: OPEN — bisected to a commit range 2026-08-05, mechanism NOT yet
found.** One hypothesis has been tested and falsified; see below so nobody
spends the afternoon on it twice.

## Symptom

`AnnotatedElementUtils.getAnnotations(AnnotatedElement)` returns **null**. It
cannot: its body is `return MergedAnnotations.from(element, ..)`, a static
factory with no null return. The NPE lands one frame up:

```
Caused by: java.lang.NullPointerException: Cannot invoke
  "org.springframework.core.annotation.MergedAnnotations.isPresent(String)"
  because the return value of
  "org.springframework.core.annotation.AnnotatedElementUtils.getAnnotations(java.lang.reflect.AnnotatedElement)"
  is null
    at AnnotatedElementUtils.isAnnotated(AnnotatedElementUtils.java:232)
    at StandardAnnotationMetadata.isAnnotatedMethod(StandardAnnotationMetadata.java:168)
    at StandardAnnotationMetadata.getAnnotatedMethods(StandardAnnotationMetadata.java:148)
    at ConfigurationClassParser.retrieveBeanMethodMetadata(ConfigurationClassParser.java:459)
```

which surfaces as
`BeanDefinitionStoreException: Failed to parse configuration class
[org.springframework.boot.autoconfigure.ssl.SslAutoConfiguration]` and takes
out the whole `@SpringBootTest` context. There is **no** SIGSEGV, panic or
`internal error:` in the log — "crash" is the runner's verdict, not a VM abort.

## What is established

Measured on Azure Linux with the host quiet (load 2–8, memory ample), one
process per class:

| Binary | `KafkaAutoConfigurationTests` |
|---|---|
| `9405271bd^` — dev immediately BEFORE the native-funnel bypass | **6/6 PASS** |
| `ded183df8` — dev tip at the time of triage | **3/6 FAIL** (1–4 tests each) |
| `ded183df8`, `--nojit` | **3/3 PASS** |

So: JIT-only, intermittent, and introduced inside
`9405271bd..ded183df8`. The prime suspects in that range are the native-call
bypass family — `9405271bd` (*give compiled code the native funnel bypass*),
`0a4473c11`, `84e2cc79e` (*the leaf bypass needs BOTH entry points*),
`97f659ed0` (*the currentThread bypass needed the THIRD compile door*) — with
`002ec5eb8` (*per-ENTRY epochs*) and `0fe8e4767` / `4ac429f2f` (descriptor /
statics typing) also inside it. **The range has not been narrowed below the
family**, and the newest of these, `836631dcc` (*site-cache EVERY native from
compiled code*), is NOT in `ded183df8` — it landed later, so it is not the
cause of what was bisected here, though it widens the same path.

It is not Kafka-specific. Same two binaries, one run each on a quiet box:

| Class | `9405271bd^` | `ded183df8` |
|---|---|---|
| `KafkaAutoConfigurationIntegrationTests` | PASS | **FAIL** (`containersFailed=1`) |
| `PulsarAutoConfigurationTests` | PASS | **FAIL** (4 of 74) |
| `ConcurrentKafkaListenerContainerFactoryConfigurerTests` | PASS | PASS (intermittent) |
| `PulsarPropertiesMapperTests` | PASS | PASS (intermittent) |
| `HazelcastJpaDependencyAutoConfigurationTests` | PASS | PASS (intermittent) |

## Hypothesis tested and FALSIFIED: the object return was unrooted

`try_jit_leaf_native_dispatch` (`vm/src/jit/helpers.rs`) returns the native's
object result to compiled code as a bare address. Every other object-returning
JIT native fast path also parks it in `thread.native_pending_return` — the slot
the collector scans (`roots.rs`) and remaps (`gc.rs`) — and this function's own
`Thread.currentThread()` arm does too, with a comment calling it "the same
contract as every other JIT native fast path". The general callback arm does
not.

That reads like the bug. It is not, or at least not on its own:

* `probes/JitNativeReturnProbe.java` opens that window as wide as Java can —
  call an object-returning native, allocate, then read the result back, over
  six shapes (fresh object, identity-stable object, derived strings, `Map.get`,
  a never-null factory through a call layer, and `Thread.currentThread()` as a
  positive control), two passes, 200k iterations. **PROBE PASS on HotSpot and
  on CratonVM, plain and under `CRATONVM_DBG=gc-stress=65536`.**
* Adding the `native_pending_return` store did not fix the suite either. It was
  committed, measured, and **reverted** (`01fa68e87`, reverted by `f1c868438`).

Reading the code afterwards says why the theory is weak: between the native
returning and the compiled caller storing the value there is no Java
allocation and no safepoint, so the window the root would protect may simply
not exist on this path.

**Do not re-apply that change without a reproducer.** If it is ever wanted for
symmetry, it needs its own justification.

## What is NOT established, and a warning about how to measure it

The mechanism. Also unmeasured: whether the fix candidate is harmful. Two
attempts to A/B it produced garbage, both for reasons worth naming:

1. The first compared `ded183df8` against `latest dev + fix` — **two different
   dev bases**, ~100 commits apart. Any difference was uninterpretable.
2. The second and third used the right control but ran while the shared host
   was out of memory (31 GB total, **0–8 GB available**, load 32–36, sixteen
   `rustc` from other sessions). Both arms then failed **44–52 of 53** tests
   with crashes and `NOSUMMARY` — two orders of magnitude worse than the 1–4
   failures the real defect produces. That signature is the box, not the VM.

**Sanity rule for this class: the real defect fails 1–4 of 53 tests. If a run
fails 40+, or the host has under ~10 GB free, throw the result away.** Check
`free -g` and `/proc/loadavg` alongside every verdict — the reproducer script
`/data/tmp/fixab.sh` records both on each line for exactly this reason.

## Next steps

1. On a quiet host, bisect inside `9405271bd..ded183df8` — the family is six
   commits, so three builds settle it. Use `KafkaAutoConfigurationTests` with
   **6 runs per candidate** (the rate is roughly 50%).
2. With the commit known, instrument that path rather than guessing: the
   symptom is a *reference-typed return arriving as null in compiled code*, so
   compare what the bypass returns against what `invoke_or_native` would have
   returned for the same site.
3. `--nojit` is a clean workaround for anyone blocked on these five classes.

## Affected classes

- `module/spring-boot-kafka` — `KafkaAutoConfigurationTests`,
  `KafkaAutoConfigurationIntegrationTests`,
  `ConcurrentKafkaListenerContainerFactoryConfigurerTests`
- `module/spring-boot-pulsar` — `PulsarAutoConfigurationTests`,
  `PulsarPropertiesMapperTests`
- `module/spring-boot-hazelcast` — `HazelcastJpaDependencyAutoConfigurationTests`
