# A JIT-only regression nulls a Spring annotation lookup, failing configuration-class parsing across six Spring Boot classes

**Status: OPEN — first bad commit identified 2026-08-05 (`b68c3b319`),
mechanism NOT yet found.** The commit is a TRIGGER, not the defect: it removes
an over-conservative bail-list and so lets a large class of methods compile
that never compiled before. Something in that newly-admitted set miscompiles.
One hypothesis has been tested and falsified; see below so nobody spends the
afternoon on it twice.

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

## The bisect, finished 2026-08-05 — and it runs on a LOCAL box

The earlier note said this needed the Azure suite host. It does not. The whole
failing slice is 139 jars and 170 MB; copied to `C:/craton/kafka-fixture` it
runs the real class off the suite host entirely, and Windows gives a far
**stronger** signal than Linux:

| | `KafkaAutoConfigurationTests` |
|---|---|
| HotSpot 25.0.3+9, same local fixture | **53/53 PASS** |
| CratonVM latest dev, JIT | **40–49 of 53 FAIL** |
| CratonVM latest dev, `--nojit` | **53/53 PASS** |

Same defect, not a lookalike: the local log carries
`NullPointerException: Cannot invoke "MergedAnnotation.isPresent()" because
"annotation" is null` and the same
`Failed to parse configuration class [SslAutoConfiguration]`. (The Windows run
ALSO surfaces `Proxy$Dispatch.invokeProxy: null Method arg` and Mockito
"Could not modify all classes" — same family, a reference arriving null in
compiled code.)

**First bad commit: `b68c3b319`** — *fix(jit): the callee compile gate read an
inherited method's bytecode against the SUBCLASS constant pool*. Its parent
`41349f661` is **53/53 PASS**; `b68c3b319` itself fails. Found with
`git bisect run`, path-limited to `vm jit native-builtins native-collections
types`, over `9405271bd..ded183df8`, then confirmed against the parent.

**It is a trigger, not the defect.** The commit is itself correct: the gate was
passing the RECEIVER's class as the constant-pool owner while scanning an
INHERITED method's bytecode, so pool indices resolved against the wrong class,
the conservative `_ => return true` arm fired, and the method was permanently
bail-listed. Fixing that lets a large class of inherited framework accessors
compile for the first time. One of those newly-admitted methods miscompiles.
That also explains the intensity gradient: **1** failing test at `b68c3b319`
itself, **40–49** at dev tip, as later commits admit still more.

So reverting `b68c3b319` would re-hide the bug and give back a real
compile-coverage regression. The fix is to find the miscompile it exposed.

## Measurement traps this class sets

Anyone continuing needs all four, because each one produced a wrong answer here:

1. **`grep -oE 'failed=[0-9]+'` also matches inside `containersFailed=N`.** A
   container-level abort (`tests=0`, 49 ms, nothing ran) scored as "0
   failures" — i.e. as a PASS — and sent a whole narrowing pass down a blind
   alley. Parse the named fields, and treat `tests=0` or `containersFailed>0`
   as INDETERMINATE, never as good.
2. **One run does not decide anything.** A deny filter that read "0 failures"
   once gave 45 and 46 on the next two runs. Baseline is 40–49 plus occasional
   container aborts and occasional segfaults. Use N≥3.
3. **The `CRATONVM_JIT_DENY` narrowing did not reproduce.** Denying
   `org/springframework/util` looked like a clean fix on one sample; repeated,
   it turns every run into a container abort — a different failure mode, not a
   fix. No culprit class has been identified, and the earlier claim that one
   had been is withdrawn.
4. **A path-limited bisect can finger a merge.** This one first converged on
   `eebd3c606`, a merge whose GOOD parent was the branch side; the change came
   in from the dev side, and `gc/`, `classloading/` and `native-io/` were
   outside the path filter entirely. Always open the merge and test the real
   commits it brought in.

## Next steps

1. Find the miscompiled method among those `b68c3b319` newly admits. The
   commit adds `CRATONVM_DBG=callee-probe`, which tallies compile refusals by
   reason — diff that tally across `b68c3b319^1` and `b68c3b319` to get the
   exact set that changed from refused to compiled. That set, not the whole
   program, is the search space.
2. Then bisect WITHIN that set with `CRATONVM_JIT_DENY` (substring match on
   `class.method`) or `CRATONVM_JIT_BISECT_ONLY` (class-name prefix allowlist),
   at N≥3 runs per filter, scoring with the field-exact parser above.
3. `--nojit` is a clean workaround for anyone blocked on these six classes.

## Affected classes

- `module/spring-boot-kafka` — `KafkaAutoConfigurationTests`,
  `KafkaAutoConfigurationIntegrationTests`,
  `ConcurrentKafkaListenerContainerFactoryConfigurerTests`
- `module/spring-boot-pulsar` — `PulsarAutoConfigurationTests`,
  `PulsarPropertiesMapperTests`
- `module/spring-boot-hazelcast` — `HazelcastJpaDependencyAutoConfigurationTests`
