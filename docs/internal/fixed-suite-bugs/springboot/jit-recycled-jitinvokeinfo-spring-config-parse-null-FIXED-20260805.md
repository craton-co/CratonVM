# A JIT-only regression nulled a Spring annotation lookup across six Spring Boot classes — FIXED by `383e7f5cf`

**Status: FIXED 2026-08-05 by `383e7f5cf`** (*fix(jit): a recycled
JitInvokeInfo address let one call site serve another's dispatch*), which
landed on dev from `fix/hib-offsetdatetime-discovery-sigsegv-20260805` while
this page was still open. Confirmed here by building its parent and itself and
interleaving them on the local reproducer.

This page was opened the same day with the mechanism unknown, and its bisect
had converged on a DIFFERENT commit. Both results were right; the relationship
between them is the interesting part, and is the reason this file is worth
keeping.

## Symptom

`AnnotatedElementUtils.getAnnotations(AnnotatedElement)` returned **null**. It
cannot: its body is `return MergedAnnotations.from(element, ..)`, a static
factory with no null return. The NPE landed one frame up:

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

surfacing as `BeanDefinitionStoreException: Failed to parse configuration class
[SslAutoConfiguration]`, taking out the whole `@SpringBootTest` context. The
local runs also produced `Proxy$Dispatch.invokeProxy: null Method arg`, Mockito
"Could not modify all classes", and occasional SIGSEGV — one family, a
reference arriving wrong in compiled code.

## Cause

`383e7f5cf`'s own message has the full account. In brief: every per-thread
dispatch memo in `jit/helpers.rs` is keyed on
`JitSiteKey = (vm_identity, JitInvokeInfo pointer)`. The `JitInvokeInfo` boxes
are owned by `CompiledMethod::_jit_invoke_infos` and are freed when the method
drops, after which the allocator can hand the same address to the next
compile's info. The key then names a **different call site** while the memo
still holds the previous site's answer.

`VIRTUAL_TARGET_CACHE` holds a resolved dispatch CLASS NAME, so a reused site
resolves its own method name against the previous site's class;
`NATIVE_SITE_CACHE` holds a resolved leaf-native callback, so the reused site
calls the previous site's native and returns whatever it returns. Only 2 of the
8 site-keyed memos were being revalidated, because the hazard had been framed
narrowly as "a raw entry pointer can go stale" — but a memo does not have to
hold a code pointer to be wrong once its key stops identifying its site. That
is exactly how a static factory with no null return hands back null.

The fix keys revalidation on the JIT cache generation, which is bumped
unconditionally whenever a `CompiledMethod` is published, and clears all eight
memos in one place.

## The other bisect result, and why it was not wrong

Before `383e7f5cf` existed, `git bisect run` over `9405271bd..ded183df8` named
**`b68c3b319`** (*fix(jit): the callee compile gate read an inherited method's
bytecode against the SUBCLASS constant pool*) as first-bad, and its parent
`41349f661` as 53/53 PASS. That measurement was sound and it was NOT the
defect.

`b68c3b319` is itself a correct fix: the generic-metadata scan was passing the
RECEIVER's class as the constant-pool owner while scanning an INHERITED
method's bytecode, so the indices resolved against an unrelated pool, the
conservative `_ => return true` arm fired, and the method was permanently
bail-listed. Removing that lets a large class of inherited framework accessors
compile for the first time — measured on `KafkaAutoConfigurationTests`,
**321 callees across 111 classes** (`CRATONVM_DBG=callee-probe` now prints them
as `NEWLY-ADMITTED class.method desc`).

More compiled methods means more `CompiledMethod`s created and dropped, which
means more `JitInvokeInfo` addresses reused, which means more collisions in the
memos above. So `b68c3b319` raised the RATE of a latent defect from
undetectable to constant. That is what the intensity gradient was:

| Binary | failures of 53 |
|---|---|
| `41349f661` (`b68c3b319^`) | 0 |
| `b68c3b319` | 1 |
| dev tip 2026-08-05 morning | 40–51, plus SIGSEGV |

**A bisect on an exposure-rate defect lands on whatever raised the rate above
the detection threshold, not on the defect.** The tell was already visible and
was recorded at the time: the first-bad commit's own diff was a plainly correct
fix in an unrelated direction, and the failure count kept CLIMBING over the
commits after it rather than staying flat. A frequency ramp is not what a
single miscompiled method looks like.

## Measurement

Local fixture `C:/craton/kafka-fixture` (139 jars, 170 MB, copied off the suite
host), run from `module/spring-boot-kafka`. Windows reproduces this far more
strongly than Linux did — 40–51 of 53 versus 1–4 — so the whole bisect ran off
the suite host.

Boundary, interleaved in both orders on the same box, one process per run:

| Binary | `KafkaAutoConfigurationTests`, failures of 53 |
|---|---|
| `383e7f5cf^` (`a918ec6e7`) | **42, NOSUMMARY, NOSUMMARY, 47** |
| `383e7f5cf` | **0, 0, 0, 0** |
| dev + this branch merged | **0, 0, 0, 0, 0, 0, 0** |
| HotSpot 25.0.3+9, same fixture | 53/53 PASS |

Per class on the merged binary, with the pre-fix binary as control:

| Class | `383e7f5cf^` | merged dev |
|---|---|---|
| `KafkaAutoConfigurationTests` | 42, 47, ×2 no-summary | **0/53 ×7** |
| `ConcurrentKafkaListenerContainerFactoryConfigurerTests` | 0/5, **5/5**, 0/5 | **0/5 ×3** |

The middle row is worth keeping: that class is intermittent, and a single green
run on the pre-fix binary would have "exonerated" it. It took three.

## NOT verified, and why

`KafkaAutoConfigurationIntegrationTests` **cannot be verified in this fixture**:
it reports `tests=0 skipped=3 containersFailed=0` and exits 0 on BOTH binaries —
every test skipped for want of a broker. That is vacuous, not a pass and not a
failure. (The first parser here labelled it `CONTAINERFAIL`, i.e. invented a
regression out of a skip. `tests=0` is not one verdict: separate
`skipped=N` from a container that actually blew up.)

`PulsarAutoConfigurationTests`, `PulsarPropertiesMapperTests` and
`HazelcastJpaDependencyAutoConfigurationTests` are **not measured**. Their
modules are not in the local fixture, and the Azure suite host was at load 58
with 5 GB of 31 GB free — the exact condition this page's own trap list says
makes every red meaningless. They are expected to be fixed, because the defect
is a general JIT dispatch-memo keying bug with no module-specific component and
they were reported as intermittent instances of this same signature, but that
is an inference. Re-run them on a quiet host to close it.

## Measurement traps this class set

Each of these produced a wrong answer during triage:

1. **`grep -oE 'failed=[0-9]+'` also matches inside `containersFailed=N`.** A
   container abort (`tests=0`, 49 ms, nothing ran) scored as "0 failures" — as
   a PASS — and sent a whole narrowing pass down a blind alley. Parse named
   fields.
2. **`tests=0` is at least three different outcomes**: all-skipped (vacuous),
   container blew up, nothing ran. See above.
3. **One run decides nothing.** A deny filter that read 0 once gave 45 and 46
   next; `ConcurrentKafkaListener` needed 3 runs to go red at all. Use N≥3.
4. **A `CRATONVM_JIT_DENY` narrowing "found" `org/springframework/util`.**
   Repeated, denying it turns every run into a container abort — a different
   failure mode, not a fix. That claim was withdrawn. Note also that
   `resolve_inline_site_from` consults NEITHER the bail list nor
   `jit_force_interpret`, so denying a method does not stop it being inlined:
   the lever can exonerate a guilty method.
5. **A path-limited bisect can finger a merge.** This one first converged on
   `eebd3c606`, a merge whose GOOD parent was the branch side; the change came
   from the dev side and `gc/`, `classloading/`, `native-io/` were outside the
   filter. Open the merge and test what it brought in.
6. **A hypothesis that reads like the bug.** `try_jit_leaf_native_dispatch`
   returns an object result to compiled code without parking it in
   `thread.native_pending_return`, unlike every sibling path. Probed six ways
   under `gc-stress` — PASS on both VMs — and the change was committed,
   measured, and reverted (`01fa68e87` / `f1c868438`). There is no Java
   allocation or safepoint in that window, so the root it would add protects
   nothing. Do not re-apply it without a reproducer.

## Diagnostic added here

`38c57f885` — under `CRATONVM_DBG=callee-probe`, `try_jit_compile_callee_slow`
now evaluates the generic-metadata scan with BOTH constant-pool owners and
reports every callee the old argument would have refused and the new one
admits:

```
[callee-probe] NEWLY-ADMITTED <class>.<method> <descriptor>
```

`class.method` leads so the output feeds `CRATONVM_JIT_DENY` unchanged. This
replaces the plan of diffing the `callee-probe` tally across two builds, which
cannot work: that dump is truncated to its 30 hottest rows, and two builds load
different code. Inert unless the flag is set.
