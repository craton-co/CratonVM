# `TestTransaction.testMergeUsing` — a MERGE USING updates half the rows it should

| | |
|---|---|
| **Status** | OPEN, found 2026-08-18. A **correctness** defect, not throughput. |
| **Symptom** | `java.lang.AssertionError: Expected: 100 actual: 50` at `TestTransaction.testMergeUsing(TestTransaction.java:446)` |
| **Collectors** | All three — Generational, G1 and ZGC, independently. |
| **HotSpot** | **PASSES**, same host, same heap, same cap, 9.9 s. |
| **Determinism** | 5 for 5: three collector arms plus two standalone repeats, every one failing at the same assertion. |

## Why this one is worth attention

Almost everything else in the H2 non-passing set is wall-clock — HotSpot finishes in seconds and CratonVM runs into the 300 s cap (see !nonpassed-40-census-20260818.md §2a). **This is not that.** CratonVM completes the class in essentially the same time as HotSpot and returns a wrong number:

| | HotSpot 25 | CratonVM |
|---|---:|---:|
| wall | 9.9 s | 9.4 – 10.2 s |
| result | PASS | `Expected: 100 actual: 50` |

Same speed, wrong answer. There is no timeout, no OOM, no exception from the engine — the statement reports having affected **50** rows where H2 asserts **100**. Exactly half.

```
Exception in thread "main" java/lang/AssertionError: Expected: 100 actual: 50
    at org/h2/test/TestBase.fail(TestBase.java:334)
    at org/h2/test/TestBase.assertEquals(TestBase.java:506)
    at org/h2/test/db/TestTransaction.testMergeUsing(TestTransaction.java:446)
    at org/h2/test/db/TestTransaction.test(TestTransaction.java:52)
    at org/h2/test/TestBase.testFromMain(TestBase.java:479)
```

**"Exactly half" is the lead.** A row count that is off by a clean factor of two, in a MERGE USING, points at the merge's source-row iteration or its matched/not-matched partitioning terminating early or visiting alternate rows — not at arithmetic. Whether the missing 50 are the matched half, the not-matched half, or every other row is the first thing to establish, and the test's own source at `TestTransaction.java:446` names which branch it is counting.

## Not the collector, and not the JIT (untested)

All three collectors fail identically, so it is not a GC defect. **The JIT has not been ruled in or out** — the obvious next step is `--nojit`, which for this tree is the standard first move on a suspected dispatch or codegen defect and costs one 10-second run. If `--nojit` passes, this is a codegen/inline-cache problem and belongs with the interface-dispatch miscompile recorded in the Tomcat census; if it still fails, it is engine-level.

## Reproduction

Deterministic, ~10 s, no special conditions. On the Azure host:

```bash
cd /data/h2gc-20260817
export CRATONVM_BIN=<cratonvm>  JDK25=/data/toolchain/jdk-25
export H2_ROOT=/data/cratonvm/apps/h2database/h2
export OUTROOT=$PWD/out-probe  H2_GC_FLAG='-XX:+UseZGC'
bash run-h2-suite.sh run --category all \
    --only '^org\.h2\.test\.db\.TestTransaction\$' --max-heap 1g --class-to 180 --tag probe

# control — passes
bash run-h2-suite.sh hotspot --category all \
    --only '^org\.h2\.test\.db\.TestTransaction\$' --max-heap 1g --class-to 180
```

Suggested first three steps, in order: `--nojit`; then print the actual row set the merge touched rather than its count; then narrow to whether the 50 are contiguous or alternating.
