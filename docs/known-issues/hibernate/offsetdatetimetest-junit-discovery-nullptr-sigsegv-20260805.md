# `OffsetDateTimeTest` — SIGSEGV during JUnit discovery, JIT-compiled reflection

**Status:** OPEN (2026-08-05). Single occurrence, not yet confirmed
deterministic. Explicitly **not** the `ClassId(0)`/stale-pointer family — see
"Why this is a different bug" below; do not fold this into
[`bug-h2-classid0-stale-address-family.md`](../h2/bug-h2-classid0-stale-address-family.md)
without re-checking the fault address and GC-cycle count first.

## Symptom

`org.hibernate.orm.test.type.temporal.OffsetDateTimeTest`, real-JDK, JIT on,
dev tip (`24eae9ac5`, `CratonVM-hib-local-0712-v3`), 4-shard residual rerun.
The process dies with `rc=139` (SIGSEGV) before printing any `@@RESULT` line —
crash happens during JUnit5's test-discovery phase, before the class's own
test bodies ever run:

```
#
# A fatal error has been detected by the CratonVM Runtime Environment:
#
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x000002596DBB0161
#  Faulting access: read at address 0x000000000000000E
#  pid=33208 tid=24996
#  thread: "main-vm"
#
#  jdk mode: real-jdk (java.home=C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot)
#  gc collector: generational
#  gc young-gen policy: moving (Cheney young copy)
#  gc young-gen actual: 0 moving cycle(s), 0 cycle(s) diverted to the NON-MOVING sweep
#  gc young-gen last incomplete-coverage reason: unregistered-jit-frame-on-stack
#  jit: guarded compiled frames live process-wide: YES (quiescence depth=1)
#  jit: 43 compiled code range(s), cache generation 69
#  jit: faulting pc not attributed to a compiled method (JIT method names are off)
#  Java frames (primordial thread, 42 frame(s), published at the last blocking/safepoint deposit):
#    at CratonRunner.main(CratonRunner.java:-1)
#    at org/junit/platform/launcher/core/SessionPerRequestLauncher.execute(...)
#    ...
#    at org/junit/platform/launcher/core/DefaultLauncher.discover(DefaultLauncher.java:-1)
#    at org/junit/platform/launcher/core/EngineDiscoveryOrchestrator.discover(...)
#    at org/junit/jupiter/engine/JupiterTestEngine.discover(JupiterTestEngine.java:-1)
#    at org/junit/jupiter/engine/discovery/DiscoverySelectorResolver.resolveSelectors(...)
#    ...
#    at org/junit/jupiter/engine/discovery/ClassSelectorResolver.resolveStandaloneTestClass(...)
#    at org/junit/jupiter/engine/discovery/ClassSelectorResolver.newStandaloneClassTestDescriptor(...)
#    at org/junit/jupiter/engine/discovery/ClassSelectorResolver.newClassTemplateTestDescriptor(...)
#    at org/junit/jupiter/engine/descriptor/ClassTemplateTestDescriptor.<init>(...)
#    at org/junit/jupiter/engine/descriptor/ClassBasedTestDescriptor.<init>(...)
#    at org/junit/jupiter/engine/descriptor/ClassBasedTestDescriptor$LifecycleMethods.<init>(...)
#    at org/junit/jupiter/engine/descriptor/LifecycleMethodUtils.findAfterAllMethods(...)
#    at org/junit/jupiter/engine/descriptor/LifecycleMethodUtils.findMethodsAndCheckStatic(...)
#    at org/junit/jupiter/engine/descriptor/LifecycleMethodUtils.findMethodsAndCheckVoidReturnType(...)
#    at org/junit/platform/commons/support/AnnotationSupport.findAnnotatedMethods(...)
#    at org/junit/platform/commons/util/AnnotationUtils.findAnnotatedMethods(...)
#    at org/junit/platform/commons/util/ReflectionUtils.findMethods(...)
#    at org/junit/platform/commons/util/ReflectionUtils.streamMethods(...)
Native frames (most recent call first) [raw]:
   0: 0x00007FF6C5C2B142  (exe+0x153B142)
   1: 0x00007FFBA2106896  (external/jit)
   2: 0x00007FFBA2105C66  (external/jit)
   3: 0x00007FFBA22240DE  (external/jit)
   4: 0x000002596DBB0161  (external/jit)
```

Full log:
`apps/hib-suite-runner/runs/run-20260805-092346-custom/on-real/shard-1/raw.log`
(search for the fatal-error banner, ~line 113705).

## Why this is a different bug from the `ClassId(0)` stale-pointer family

Yesterday's `SmokeTests` crash was misattributed to the DoHead Layer-1
register-invisible-root mechanism and has since been rigorously refuted and
retired (`../../internal/fixed-suite-bugs/hibernate/smoketests-stale-pointer-nosuchmethod-crash-20260804-RETIRED.md`)
— the real mechanism was a blocked-thread root-scan screening gap, now fixed
(`bcf5dd99c`, included in this binary), plus its residual is tracked as a
face of `bug-h2-classid0-stale-address-family.md`. Before writing this crash
up as another witness of that family, checked the two things that family's
own doc says to check first:

1. **The fault address.** Every face of the `ClassId(0)` family reads a
   *real*, plausible heap address whose header has gone stale (e.g.
   `0x16cc325d598`, `0x2001d790498`) — a freed block or an evacuated
   from-copy. This crash's faulting address is **`0x000000000000000E`** —
   14 decimal, i.e. `null_base + 0x0E`. That is the signature of a plain null
   receiver plus a small field-offset load, not a stale/relocated object.
2. **GC cycle count at crash time.** `gc young-gen actual: 0 moving cycle(s),
   0 cycle(s) diverted to the NON-MOVING sweep` — **no young collection has
   run at all** in this process. The `ClassId(0)` family's entire mechanism
   is a collector reclaiming or relocating something still referenced; with
   zero collections having occurred, there is nothing for a collector to have
   gotten wrong yet. The `last incomplete-coverage reason` field is state
   carried from VM init, not evidence a collection caused this fault.

Both checks say this is not that family. It looks instead like a **JIT
codegen bug**: a null check the interpreter would have taken (or the JDK
bytecode itself guards against) elided or miscompiled in one of the
JIT-compiled frames on the native stack (`external/jit` frames 1-4), reached
via JUnit5's reflection-heavy discovery path
(`ReflectionUtils.findMethods` → `AnnotationSupport.findAnnotatedMethods` →
`LifecycleMethodUtils.findAfterAllMethods` → `ClassBasedTestDescriptor`
construction) while resolving `@AfterAll`/lifecycle methods for this specific
class. `CRATONVM_DBG_JIT_NAMES=1` was not set for this run, so the compiled
method at the faulting pc is unidentified — that's the first thing to add for
a repro.

## Not yet established

- Whether this reproduces at all — single occurrence, one run.
- Which compiled method is at fault (needs `CRATONVM_DBG_JIT_NAMES=1` and/or
  `CRATONVM_SYMBOLIZE=<RVAs>` against this exact binary).
- Whether `OffsetDateTimeTest` is relevant at all, or whether any class whose
  discovery reaches this same reflection path at the right JIT-tiering moment
  would trigger it (the crash is in JUnit/reflection code, not in anything
  `OffsetDateTimeTest`-specific — its own test bodies never ran).
- Whether `--nojit` avoids it (would confirm the JIT-codegen hypothesis
  the way it did for the sibling `batchtest-jit-duplicate-batch-insert-unique-violation-20260804.md`
  and `HIB-CV-39`/inline-alloc-header-corruption bugs, which used the same
  diagnostic move).

## Repro

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_JIT_NAMES=1 <cv> \
  --java-home "<jdk25>" --Xmx 1500m @common.args -Dcraton.batch=1 \
  CratonRunner org.hibernate.orm.test.type.temporal.OffsetDateTimeTest
```

Before spending investigation time, re-run 3-5x to establish a hit rate — a
single occurrence during discovery, immediately after two other shards
finished (`BasicCdiTest`'s Weld container shutdown log appears immediately
before the fatal-error banner in the same shard), leaves open the
possibility of transient host-load timing rather than a deterministic bug.
