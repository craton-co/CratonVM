# `TestDiskFull` — `class_id=ClassId(0)`/`num_slots=0` guard burst, then `SIGSEGV`, `ClassCastException`, or a hang (fast reproducer for the `TestMVStoreCachePerformance` family)

## Status
**OPEN** — newly characterised 2026-07-31 while closing out
`bug-h2-testgetgeneratedkeys-corrupt-value-cell-hib-cv-32-family.md`. Not
root-caused. Filed separately because it is *not* the defect that report was
about (see "What this is not").

## Severity
**HIGH** — hard `SIGSEGV` in some runs; wrong-type `ClassCastException` in
others; a >300 s hang in others.

## Affected test class
`org.h2.test.synth.TestDiskFull`

## Why this doc is worth having
The already-open
`bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md` describes the
same signature — a burst of
`gen_heap::set_field: out-of-bounds field write dropped ... class_id=ClassId(0)
class_name=java/lang/Object num_slots=0` warnings followed by a `SIGSEGV` — but
its reproducer takes **~972 s** per attempt and was seen once. `TestDiskFull`
run in a fresh scratch CWD takes **~2 s** per attempt and fails at a
measurable rate, so it is a far cheaper handle on the same signature.

## Measured rates (2026-07-31, Azure host, JDK 25, `--Xmx 1g`, JIT on)
Each run: fresh empty scratch CWD, 300 s timeout, 60 runs per arm.

| arm | pass | `Chunk N not found` (rc=1) | timeout (rc=124) | `SIGSEGV` (rc=139) |
|---|---|---|---|---|
| CratonVM `dev` + wrapper-equals fix | 30 | 14 | 15 | 1 |
| CratonVM `dev` (pre-fix) | 28 | 7 | 16 | 9 |
| **stock HotSpot JDK 25** | **27** | **3** | **0** | **0** |

Caveats on the numbers, not on the conclusion: the host was heavily
oversubscribed (load average 12–113 across the window, a system-wide OOM kill
of another tenant's process at 13:46) and the pre-fix arm's nine `SIGSEGV`s are
**consecutive runs 50–60**, i.e. a clustered wedge rather than an
independent-per-run rate — do not read the 9-vs-1 difference as an effect of
the wrapper-equals fix. What the HotSpot column settles is the part that
matters: `Chunk N not found` **also happens on stock HotSpot** and is upstream
H2 fault-injection flakiness, while `SIGSEGV`, the 300 s hangs, and the
`ClassCastException` below are **CratonVM-only**.

## Symptom 1 — guard burst then `SIGSEGV`
Hundreds of these, back to back, in the ~1 ms before the crash:
```
[WARN] gen_heap::set_field: out-of-bounds field write dropped (caller used slot
  index past receiver's layout — class layout is correct; the bug is in the
  caller's slot computation) obj=0x20028aa97c0 index=0 num_slots=0
  class_id=ClassId(0) class_name=java/lang/Object real_field_count=Some(0)
  value=Object(Some(ObjectRef { ptr: 0x20028aa96e0 }))
```
Every one: `index=0`, `num_slots=0`, `class_id=ClassId(0)`, and the stored
value is a live-looking `ObjectRef` **0xe0 below the receiver** — a consistent
offset, which suggests a systematic address/layout computation error rather
than random corruption. Then:
```
# A fatal error has been detected by the CratonVM Runtime Environment:
#  SIGSEGV at pc=0x5f2723b2cc60, addr=0x5f2722698000, pid=2610343
#  fault pc is in NO recently freed code buffer
#  fault pc is in NO live registered code buffer
#  maps: fault pc IS MAPPED — r-xp, inside the cratonvm text segment
```

## Symptom 2 — a synthetic object where a `String[]` should be
```
Caused by: java/lang/ClassCastException:
  cratonvm.synthetic.AnonymousObject$3 cannot be cast to [Ljava.lang.String;
	at org/h2/util/StringUtils.toUpperEnglish(StringUtils.java:91)
	at org/h2/command/Tokenizer.convertCase(Tokenizer.java:1071)
```
preceded by the read-side counterpart of the same guard:
```
[WARN] gen_heap::get_field: out-of-bounds field read dropped ... obj=0x2004a767860
  index=2 num_slots=0 class_id=ClassId(0) class_name=java/lang/Object
  real_field_count=Some(0)
```
`StringUtils.toUpperEnglish` reads the `private static final String[]
TO_UPPER_CACHE` static, so a `cratonvm.synthetic.AnonymousObject$3` coming back
from that read is a **static-field slot** holding the wrong object — the same
"receiver/slot reads back as a bare `java/lang/Object` with zero fields" shape
as symptom 1.

## Symptom 3 — hang
15–16 runs of 60 exceeded a 300 s timeout in both CratonVM arms and **zero** of
28 on HotSpot. The host was loaded, so some of this is contention; the
zero-on-HotSpot column says it is not all contention.

## What this is not
The originating report
(`bug-h2-testgetgeneratedkeys-corrupt-value-cell-hib-cv-32-family.md`, now
`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testgetgeneratedkeys-wrapper-equals-missing-type-check-FIXED.md`)
noted `TestDiskFull` failing with
`AbstractMethodError: org/h2/value/Value.getValueType()I has no Code attribute`
inside `ScriptCommand`'s row serialization and wondered whether it was the same
family. **That specific `AbstractMethodError` did not reproduce once** in 120
short-form runs (60 pre-fix + 60 fixed) plus 3 long-form runs (~19 min each,
the `write op count` loop actually engaging with 461 and 331 iterations — all
three passed). It is a single unreproduced observation; do not carry it forward
as an established symptom.

## Repro
```bash
H2=<repo>/apps/h2database/h2
CP="$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)"
d=$(mktemp -d); cd "$d"          # a FRESH empty CWD matters, see below
<cratonvm-bin> --java-home /home/victor/jdk25 --Xmx 1g -c "$CP" \
  org.h2.test.synth.TestDiskFull
```
Loop it — roughly 1 run in 2 fails, in one of the three ways above.

**CWD matters.** In a fresh CWD the test's own
`Math.min(1000, Integer.MAX_VALUE - fs.getDiskFullCount() + 10)` overflows
negative, so the 1000-iteration loop is skipped and only the single
`test(Integer.MAX_VALUE)` pass plus the closing `script to 'memFS:test.sql'`
runs — ~2 s. Re-running in the H2 checkout root instead gives a real
`write op count` (461 / 331 observed) and takes ~19 min. The suite runner gives
every class a fresh scratch CWD, so the short form is what the suite actually
measures.

## Suggested next steps
1. `CRATONVM_DBG_CELLCORRUPT=1` on a failing short run to get the holder dump +
   backtrace for the `class_id=ClassId(0)` receivers — that single step is what
   turned the `TestGetGeneratedKeys` report from "heap corruption, root cause
   open" into a one-line fix, and no one has done it for this signature yet.
2. Symptom 2 gives a named, specific target: find who writes
   `cratonvm.synthetic.AnonymousObject$3` into `StringUtils.TO_UPPER_CACHE`'s
   static slot (or who reads that static with the wrong slot index).
3. The constant `0xe0` delta between receiver and stored value in symptom 1 is
   worth chasing directly — it points at a fixed miscomputed offset.
