# `Arrays.sort(long[])` throws a bogus `ArrayIndexOutOfBoundsException` under OSR

| | |
|---|---|
| **Status** | OPEN — reproducible in two lines, narrowed to ONE method's OSR artifact |
| **Severity** | high — `Arrays.sort` is not a corner of the JDK, and the failure is a *wrong index*, i.e. silent data corruption is one branch away |
| **HotSpot** | PASS |
| **CratonVM** | FAIL on the FIRST sort, every run, JIT on |
| **Discovered** | 2026-08-03, trying to run `probes/HandoffLayersProbe.java` (it sorts a `long[]` of latency samples and died before printing anything) |

## Symptom

```
java.lang.ArrayIndexOutOfBoundsException: Index -621629441 out of bounds for length 20000
    at java.util.DualPivotQuicksort.mixedInsertionSort(DualPivotQuicksort.java:1474)
    at java.util.DualPivotQuicksort.sort(DualPivotQuicksort.java:157)
    ...
    at java.util.Arrays.sort(Arrays.java:137)
```

The index is garbage and differs every run (`-621629441`, `1561423871`,
`181506047`, …); the array length in the message is always correct. So the
array reference is fine and an *index* has been restored or computed wrong.

## Reproduction

`probes/SortProbe.java` — sort a `long[]` of random values, check the result:

```bash
javac -d /tmp/out probes/SortProbe.java
<cratonvm> --java-home <jdk25> --Xmx 2g -cp /tmp/out SortProbe 20000 20
```

* HotSpot: `PROBE-OK 200 sorts of 20000 longs`
* CratonVM: `PROBE-FAIL rep=0 …ArrayIndexOutOfBoundsException…`

Size threshold — it needs the quicksort path, not the small-array insertion
sort:

| n | verdict |
|---|---|
| 100 | PASS |
| 1000 | FAIL (rep 2) |
| 5000 | FAIL (rep 0) |
| 20000 | FAIL (rep 0) |

## What it is NOT — every one of these measured, not reasoned

| lever | result |
|---|---|
| `--nojit` | **PASS.** So it is the compiler. |
| `--Xmx 12g` (too big to collect) | still FAILS — not the collector, and not a GC use-after-free ([[reference_exclude_jit_wrong_object_before_blaming_gc]] applies in the other direction here) |
| `CRATONVM_JIT_OSR=0` | **PASS.** So it is the OSR path specifically, not whole-method compilation. |
| `CRATONVM_JIT_DENY=java/util/DualPivotQuicksort`, `java/util`, `java/`, `java/,jdk/,sun/` | still FAILS |
| `CRATONVM_JIT_BISECT_ONLY=<no match at all>` | still FAILS — **the deny/bisect filters do not reach the OSR compile path.** That is a gap in the bisection tooling worth closing on its own; it is why this could not be narrowed to a method the usual way. |
| `CRATONVM_JIT_OSR_DEAD_LOCALS=0` | still FAILS — not the dead-local entry relaxation |
| `CRATONVM_JIT_OSR_SINGLE_PC=1` (added by this investigation) | still FAILS — not an artifact being entered at a *second* loop header |

`CRATONVM_JIT_OSR_SINGLE_PC` is new (`CompiledMethod::osr_compiled_entry_pc`,
default OFF). The trace made multi-pc entry the obvious suspect:

```
OSR-compile java/util/DualPivotQuicksort.partitionDualPivot([JIIII)[I entry_pc=117 entry=0x… len=8979
OSR-reuse   java/util/DualPivotQuicksort.partitionDualPivot([JIIII)[I entry_pc=93  entry=0x… len=8979
OSR-compile java/util/DualPivotQuicksort.mixedInsertionSort([JII)V   entry_pc=282 entry=0x… len=15582
OSR-reuse   java/util/DualPivotQuicksort.mixedInsertionSort([JII)V   entry_pc=368 entry=0x… len=15582
```

An artifact compiled for one pc being entered at another is exactly the shape
of [[reference_osr_entry_only_valid_at_a_loop_header]]. Restricting entry to
the compiled pc does **not** fix it, so the OSR-compiled **body** is wrong, not
the choice of entry point. Keep the lever: it took one build to answer and it
will take one env var next time.

## Narrowed to one method (2026-08-04)

The bisect levers reach the OSR compile path as of `dev` `12b8cbdea` — another
session closed exactly the gap this doc's step 1 asked for
(`compile_osr_artifact` now consults `cratonvm_jit::jit_force_interpret`; see
`annotation-scan-arrayread-sigsegv.md`, which hit the same wall). With working
filters the answer is unambiguous:

| `CRATONVM_JIT_DENY=` | verdict |
|---|---|
| `zzzNoSuchPrefix` via `BISECT_ONLY` (nothing compiles) | PASS — the filter is now effective |
| `DualPivotQuicksort` | PASS |
| `DualPivotQuicksort.mixedInsertionSort` | **PASS** |
| `DualPivotQuicksort.partitionDualPivot` | FAIL |
| `DualPivotQuicksort.insertionSort` | FAIL |
| `DualPivotQuicksort.heapSort` | FAIL |
| `DualPivotQuicksort.sort` | FAIL |
| `DualPivotQuicksort.tryMergeRuns` | FAIL |
| `DualPivotQuicksort.pushDown` | FAIL |
| `SortProbe` (the probe's own code) | FAIL |

**The defect is the OSR artifact for
`java.util.DualPivotQuicksort.mixedInsertionSort([JII)V`, compiled at
`osr_bci=282`.** Denying that one method — and only that one — makes every sort
correct.

Beware when writing further deny lists: the matcher is a case-sensitive
substring over `Class.method`, and `DualPivotQuicksort.mixedInsertionSort`
contains the substring `sort`. A deny list containing `sort` silently disables
the culprit and reads as a pass for the wrong reason.

### Not the operand-stack widths

`CRATONVM_DBG=stack-kinds` over the failing compile:

```
[stack-kinds] …mixedInsertionSort:([JII)V answered 276 of 446 pcs (calls=0 fields=0 statics=0)
[stack-kinds] …mixedInsertionSort:([JII)V bci=282 emitter_depth=0 analysis=Some([]) accepted=true
```

Every OSR entry candidate reports `emitter_depth=0` with an empty stack, so the
typed-operand-stack work is not implicated — the entry states are trivial. The
body is wrong, not the state restored into it. (`answered 276 of 446` is a
separate observation worth a look on its own, but the accepted entries are all
empty-stack.)

## What is left to bisect

Both OSR-compiled methods are `long[]` + `int`-index kernels:
`partitionDualPivot([JIIII)[I` and `mixedInsertionSort([JII)V`. They combine a
`long` local with `int` operand-stack entries written mid-expression
(`a[i = low]` compiles to `dup`/`istore`) and a pre-decrement inside a loop
condition (`a[--i]`) — the width-source distinction that the OSR typed-
operand-stack work is about (`jit/src/x64/stack_kinds.rs`,
`CRATONVM_DBG=stack-kinds`).

`probes/OsrLongIndexProbe.java` extracts exactly that inner loop and **passes**
on CratonVM, so the shape alone is not sufficient — the surrounding method (the
three-way split, the `pin`, or `partitionDualPivot`'s five int parameters) is
part of it. That probe is kept as the negative result: do not re-extract this
loop expecting it to fail.

## Suggested next step

1. ~~Teach the deny/bisect filters to gate the OSR compile path.~~ Done on
   `dev` by another session; the narrowing above is the result.
2. ~~Find which method.~~ `mixedInsertionSort([JII)V`.
3. ~~`CRATONVM_DBG=stack-kinds`.~~ Clean — empty stack at every accepted entry.
4. **Next:** diff that method's OSR artifact against its whole-method artifact
   with `jit/tests/x64_artifact_corpus.rs` (bytes AND metadata). The
   whole-method compile appears sound — `CRATONVM_JIT_OSR=0` passes while the
   method still compiles — so the two artifacts for the same bytecode disagree,
   which is exactly what that differ is for.

## Blast radius

Anything that sorts a `long[]` (or, untested, any primitive array through the
same `DualPivotQuicksort` kernels) with ≥1000 elements. That includes
`probes/HandoffLayersProbe.java` and `probes/ExecDispatchProbe.java`, which is
how it was found — both compute percentiles by sorting their sample array, so
the AQS/handoff latency work is blocked on this until it is fixed or the probes
are changed to avoid `Arrays.sort`.
