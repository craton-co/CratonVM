# `Type.getTypeName()` dispatches on `java/lang/Integer` during a SessionFactory rebuild cascade

**Status:** OPEN — **not reproduced**, cause not located. The signature and the
witness's exact shape are established; the eliminations below are all
measurements, so the next attempt does not repeat them.

**Witness:** `org.hibernate.orm.test.hql.ASTParserLoadingTest`, JIT, 2 runs of a
round of 6 (2026-07-31, `cratonvm-hqlordinal-fix2-20260731.exe`). Both were
killed at the harness's 1200s cap (`rc=124`) without producing a result. Those
two logs are the only captured occurrence; a compact extract is preserved next
to this file as
[`evidence/gettypename-20260731-run-1-0-excerpt.txt`](evidence/gettypename-20260731-run-1-0-excerpt.txt)
(`.txt`, not `.log` — the repo's `.gitignore` drops `*.log`),
because the originals live under `apps/hib-suite-runner/runs/`, which is
gitignored.

> **This file was rewritten on 2026-08-01.** Its first version described the
> cascade as the consequence of a JUnit timeout and counted 21 SessionFactory
> bootstraps in an affected run. Both claims came from reading the summary rows
> rather than the logs. Re-reading the logs line by line gives a different and
> much sharper picture — see "What the field logs actually show". The
> `getTypeName` signature itself, and the reasoning about which call it is, were
> correct and are unchanged.

## Symptom

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/lang/Integer.getTypeName()Ljava/lang/String;"
  caller="org/hibernate/type/descriptor/java/spi/JavaTypeRegistry.addBaselineDescriptor(Lorg/hibernate/type/descriptor/java/JavaType;)V @pc=27"
```

The caller's bytecode is

```
 1: invokeinterface JavaType.getJavaType:()Ljava/lang/reflect/Type;   -> local 2
24: invokevirtual   addBaselineDescriptor:(Type;JavaType;)V
      (inlined; its body does registry.put(describedJavaType.getTypeName(), d))
27: return
```

so the reported `@pc=27` is the pc *past* the `invokevirtual` at 24, and the
failing call is the inlined callee's `invokeinterface Type.getTypeName()` on
local 2. Local 2 is whatever `descriptor.getJavaType()` returned, and nothing
between the `astore_2` at 6 and the `aload_2` at 22 allocates — so the wrong
value is produced by the accessor, not lost after it.

Only one baseline descriptor can put an `Integer` there:
`IntegerJavaType`, whose `getJavaType()` should return the `Integer.class`
mirror and which also declares `public static final Integer ZERO = 0`. A
`Class` mirror is a `java.lang.Class` — `get_or_create_class_mirror` allocates
it with `alloc_object(class_class_id, ..)` — so resolving the call against
`java/lang/Integer`, the class the mirror *describes*, is the defect. That
single-candidate reading also explains the rate: 2 warnings per bootstrap
attempt, one per `TypeConfiguration` the build constructs.

## What the field logs actually show

Both affected logs are structurally identical to each other, line for line,
with timestamps within 0.1s — they were parallel shards of the same round.

| | affected (1-0, 1-2) | healthy (1-1, 1-3, 1-4, 1-5) |
| --- | --- | --- |
| log lines | 4 673 | 37 015 |
| last SQL statement | line 4 208 | line 36 995 |
| `Database info` (completed bootstraps) | **5** | 4 |
| `getTypeName` warnings | **32** | 0 |
| outcome | killed at 1200s, no result | 106/106 |

The sequence in an affected run is:

1. the test body runs normally to **~11%** and then stops *inside*
   `ASTParserLoadingTest.testNumericExpressionReturnTypes`, between its
   multiplication assertions — the healthy runs emit one more
   `select (1*1) from Animal a1_0` at that point and continue. Nothing is
   logged about why: `CratonRunner` does not print test failures.
2. the schema is dropped and the factory rebuilt — `SessionFactoryExtension`
   releases it from `handleTestExecutionException`, so every test that throws
   makes the next one rebuild.
3. from the **sixth** bootstrap on, every rebuild raises the warning and dies
   *before* it reaches the connection pool: after the first warning the log has
   16 further build attempts (`HHH10005002`), 32 warnings, and **zero**
   `Database info`. No test ever runs again. It repeats until the wall cap.

Two things follow, and they are what makes this worth chasing:

* **The cascade is not a symptom of a timeout — it is the defect feeding
  itself.** A failed bootstrap releases the factory, which makes the next test
  rebuild, which fails identically.
* **It is permanent, not intermittent.** Whatever breaks priming at bootstrap 6
  is never repaired. That rules out a transient race at the call site and points
  at state — a compiled body, a cached resolution, or a corrupted object — that
  nothing invalidates.

### The one VM-level signal that separates the two groups

```
run-1-0  32 warnings  1:unregistered-jit-frame  2:active-safepoint-map-incomplete  3:unregistered  4:unregistered  5:compiled-frame-oop-not-published
run-1-2  32 warnings  1:unregistered-jit-frame  2:unregistered  3:unregistered  4:unregistered  5:compiled-frame-oop-not-published
run-1-1   0 warnings  1:unregistered  2:unregistered  3:unregistered  4:unregistered
run-1-3   0 warnings  1:unregistered  2:active-safepoint-map-incomplete  3:active-safepoint-map-incomplete  4:unregistered
run-1-4   0 warnings  1:unregistered  2:unregistered  3:unregistered  4:unregistered  5:unregistered
run-1-5   0 warnings  1:unregistered  2:unregistered  3:active-safepoint-map-incomplete  4:unregistered  5:unregistered  6:unregistered
```

`[moving-young] fallback reason=compiled-frame-oop-not-published` occurs in
**exactly** the two affected runs and in neither healthy one. Neither the
fallback count nor `active-safepoint-map-incomplete` discriminates.

**Do not read that as the cause.** It is logged at line 4 618 / 4 570 — nine
minutes *after* the first warning at 4 321 — so on the evidence it is downstream
of whatever went wrong, not its trigger. It is recorded because it is the only
VM-level line that separates the groups at all, and because the path it names
(the non-moving in-place old-gen sweep run during a young collection) is the
same path `c3dbb011a` later found to be corrupting live payload. That
coincidence was tested directly and did not hold up — see below.

## Ruled out

Everything here was measured.

### The call site itself

* **`getJavaType()` returning the wrong field.** `IntegerJavaType` declares
  `public static final Integer ZERO` alongside the inherited `Class<T> type`,
  and the failure names Integer *every* time — never String, never Long — so a
  getstatic/getfield mix-up on this one class was the best structural fit.
  `JavaTypeAccessorProbe` drives the real `AbstractClassJavaType.getJavaType()`
  through an interface-typed site over nine descriptor singletons: **810 000
  checks clean** across default, `CRATONVM_JIT_THRESHOLD=5`,
  `CRATONVM_GC_STRESS=1M`, and both.
* **`Type.getTypeName()` dispatch over many `Class` mirrors.**
  `TypeNameDispatchProbe`: one shared hot `Type`-typed site over 30 mirrors,
  **1 800 000 checks clean** under forced JIT and GC stress.
* **The number of times the failing method runs.** `BaselinePrimeProbe` reaches
  the real `JavaTypeRegistry` priming through `new TypeConfiguration()` — the
  exact per-bootstrap work, at microseconds a sample instead of ~26s.
  **15 000 primings** (default, `CRATONVM_GC_STRESS=1M`,
  `CRATONVM_JIT_THRESHOLD=5`, and both): 0 warnings, 0 failures. The field
  witness broke on its sixth bootstrap, i.e. after roughly 265 calls.
* **Suite-warmed VM state plus priming.** `PostSuitePrimeProbe` runs the real
  test class and then primes in the same JVM: 106/106 followed by **20 000
  primings**, 0 warnings.
* **`VIRTUAL_TARGET_CACHE` serving a stale entry after `JitInvokeInfo` address
  recycling.** Entries are only inserted when `cacheable_receiver`, and in that
  branch the cached `class_name` is the receiver class's own name — a pure
  function of the `ClassId` half of the key, so a recycled `info` pointer cannot
  change the answer. (Recycled *ClassIds* could, but `java/lang/Class` and
  `java/lang/Integer` are bootstrap classes that never unload.)

### Reproduction

Nothing below produced a single warning. Every run was 106/106.

| binary | condition | runs |
| --- | --- | --- |
| dev `b56da0bba` | the harness's own config — `--par 8`, `-Xmx 1500m`, JUnit timeout 120s | 8 |
| dev | JUnit timeout 30s, `--par 6` | 6 |
| dev | JUnit timeout 3s — a pure rebuild loop, 12 real bootstraps | 1 |
| dev | the class four times in ONE JVM: 13 bootstraps, 26 000 SQL statements, fully warm JIT | 3 |
| dev | `CRATONVM_NO_MOVING_YOUNG=1` — *every* young collection takes the non-moving in-place old-gen sweep | 3 |
| `7a8600b5e` (adds the lambda-proxy SAM fix) | the original hunt, `--par 6` × 4 rounds | 24 |
| `4ad586e79` (the witness's own era, instrumented) | the original hunt, `--par 6` × 3 rounds | 18 |
| `4ad586e79` | `CRATONVM_NO_MOVING_YOUNG=1` | 4 |

That last pair is the important one. The suspect collector path was forced on
for whole runs, on **both** the current tree and the witness's own era, and
neither produced the warning — so "dev is clean because the old-gen mark was
fixed" is not supported by this evidence, however well the mechanism fits.

**The witness era no longer reproduces its own baseline.** That round's healthy
runs scored 103/106; `4ad586e79` now scores 106/106 on this host, 22 runs out of
22. Whatever the field session's machine state was — it was heavily loaded and
shared — it is not being recreated here, and that is the honest reason for the
null result, not a fix.

## How to hunt it

The blocker is reproduction, and it is not bootstrap count, priming count, JIT
warmth, GC pressure, heap size, parallelism, or the collector path. All of those
were varied.

* `CRATONVM_DBG_CCE_BT=1` is the tracer. Since 2026-07-31 it dumps for **every**
  dispatch `NoSuchMethodError`, not only receivers that resolved to bare
  `java/lang/Object`, and since 2026-08-01 it prints an `NSME-RECV SHAPE` line:
  the receiver's kind, field count, first field values, and whether the address
  is registered in `class_mirrors_reverse`. That line answers the question the
  rest of the dump does not — *is this the intended object carrying a wrong
  class, or a different object entirely* — and the two need opposite fixes. **No
  captured occurrence has it yet;** the field runs predate both changes.
* **The first thing to explain is not the warning — it is why
  `testNumericExpressionReturnTypes` stopped.** That is the earliest observable
  divergence, 113 lines before the first warning, and nothing recorded why.
  `CratonRunner` did collect the failure, but only reported it through
  `printFailuresTo` *after the whole class finished* — and these runs were killed
  mid-class, so every failure they had collected died with the process. It now
  also prints `@@TESTFAIL <class> <test>` plus the stack trace to stderr as each
  failure happens, which makes the next occurrence self-explaining. That edit is
  in `apps/hib-suite-runner/CratonRunner.java`, deliberately **not** staged —
  the file's own header forbids it — so re-apply it if that fixture is ever
  restored from a clean checkout.
* If a run does start cascading, it stays cascading — so there is no need to
  catch the moment. Any bootstrap after the sixth reproduces on demand within
  ~26s, with the tracer on.

## Tools

Tracked next to the suite runner. All probes are HotSpot-clean.

| Tool | What it does |
| --- | --- |
| `run-typename-cascade.sh` | drives the rebuild cascade on purpose (shrink the JUnit timeout so every test rebuilds the factory), with knobs for repeat passes, heap and parallelism |
| `BaselinePrimeProbe.java` | `JavaTypeRegistry` priming at bootstrap rate in a fresh VM — real arm plus a structural clone that can see a wrong receiver *before* `getTypeName()` turns it into a `NoSuchMethodError` |
| `PostSuitePrimeProbe.java` | the same, after a full real suite run in the same JVM; also prints every test failure with its stack trace |
| `JavaTypeAccessorProbe.java` | `JavaType.getJavaType()` through an interface-typed site over nine descriptor singletons |
| `TypeNameDispatchProbe.java` | one shared `Type`-typed `getTypeName()` site over 30 distinct `Class` mirrors |
| `SessionFactoryChurnProbe.java` | repeated real SessionFactory bootstrap |
