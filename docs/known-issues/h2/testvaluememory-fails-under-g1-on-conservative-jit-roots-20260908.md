# H2 `TestValueMemory` fails under `-XX:+UseG1GC`, and only under G1

*Found 2026-09-08 while retiring
`docs/internal/fixed-bugs/h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908.md`.
It is not a regression from that work — the numbers below are byte-identical on
the binary before it — and it is not what that page was open for. It is written
down here because nobody had run this test on the G1 arm.*

## Status

**OPEN.** `org.h2.test.unit.TestValueMemory`, `-Xmx2g`, on the same checkout,
same host, same commit:

| arm | result | worst row |
|---|---|---|
| CratonVM default (ZGC) | **PASS**, 40/40 types | 2.67x |
| CratonVM `-XX:+UseGenerationalGC` | **PASS**, 40/40 types | 2.30x |
| CratonVM `-XX:+UseG1GC` | **FAIL at Type 0** | **3.30x** |
| HotSpot JDK 25 | PASS | — |

```
AssertionError: Type: 0 Used memory: 3224 calculated: 976 length: 125000 size: 1
```

The assertion is `used > memory * 3`, so the threshold is 2928 KB and G1 reads
3224. The other two collectors read 2228 on the same row.

## It is conservative JIT-frame roots, and G1 pays more for them than the others

Turning the JIT off answers it completely, on every arm:

| Type 0, `Used memory` | ZGC | generational | G1 |
|---|---:|---:|---:|
| JIT on | 2228 | 2228 | **3224** |
| `--nojit` | 977 | 977 | **976** |

With precise roots all three collectors agree to within a kilobyte. The spread
is entirely in what a *conservative* root costs each of them, and G1's unit is
the coarsest: its reclamation unit is a region, and a conservative JIT root
takes its whole region out of the collection set — that is what
`jit-pinned-regions-excluded` in the G1 degraded-flag vocabulary names. Where
ZGC and the generational sweep retain the object, G1 retains everything sharing
its region.

That is the mechanism the numbers are consistent with; it is not yet the
measurement. Nothing here has counted pinned regions on this workload, and the
first thing to do is exactly that (`[GC] g1 ...` degraded flags with
`CRATONVM_GC_STATS=1`, plus `CRATONVM_G1_DBG_GRAY_PROV=1` for provenance)
rather than to act on the paragraph above.

`probes/RealDrop.java` does show the granularity difference from the other side:
G1 is the one arm whose `--nojit` row still *counts* the structure at `peak`
while still freeing every byte of it by `after`, which is what a region-granular
liveness answer looks like when the object really is dead.

| arm | before | peak | after | retained |
|---|---:|---:|---:|---:|
| G1, `--nojit` | 1420 | 3373 | 1420 | 0 |
| G1, JIT on | 1420 | 4445 | 4445 | +3025 |
| ZGC, JIT on | 1511 | 4441 | 4441 | +2930 |
| generational, JIT on | 1341 | 5133 | 4270 | +2929 |

## What this is NOT

* **Not the `GC_FLAG_HEADER` work.** Measured on both binaries: `Used memory:
  3224` before and after, character for character. G1 does not use the young
  non-moving sweep this page's predecessor was about.
* **Not the `freeMemory()` metric.** `VmHeap::live_bytes_estimate` falls through
  to `allocated_bytes` on the G1 arm, so that change is a no-op here, and G1's
  `allocated_bytes` already tracks reclamation (`peak` falls to `after` in the
  `--nojit` row above).
* **Not a new class of defect.** Conservative compiled-frame roots over-retain
  by design on every arm; this is the same mechanism with a coarser unit.

## Reproducer

```bash
H2=<h2 checkout>
CP="$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)"
cratonvm --java-home "$JDK25" -XX:+UseG1GC -Xmx2g -cp "$CP" \
    org.h2.test.unit.TestValueMemory        # rc=1 at Type 0, ~1 s
cratonvm --java-home "$JDK25" --nojit -XX:+UseG1GC -Xmx2g -cp "$CP" \
    org.h2.test.unit.TestValueMemory        # Type 0 reads 976
```

and the collector-free version:

```bash
cratonvm --java-home "$JDK25" -XX:+UseG1GC -Xmx2g -cp probes RealDrop 125000
cratonvm --java-home "$JDK25" --nojit -XX:+UseG1GC -Xmx2g -cp probes RealDrop 125000
```

## Where to start

Three, in order of what they cost:

0. **Measure the mechanism before changing anything.** How many regions does
   this run pin, how full are they, and how much of `3224 - 2228` they account
   for. `CRATONVM_GC_STATS=1` prints the degraded flags per cycle and
   `CRATONVM_G1_DBG_GRAY_PROV=1` names the pusher; `--nojit` is the control that
   makes the whole difference vanish, so the delta is attributable in one run.
   Everything below is a hypothesis until this is done.
1. **Narrow what a pinned region costs.** Excluding a whole region from the CSet
   is a correctness-safe answer to "a conservative word may point in here", and
   it is much coarser than the question needs to be. G1 already has
   `classify_candidate_header_view`, which decides whether an address is an
   object BASE; whether pinning can be narrowed using it — and what that costs
   on the conservative-scan path, which was moved off the regions lock in
   2026-09-05 precisely because it is hot — is the second measurement.
2. **Precise oop maps for compiled frames.** This would close the JIT-on rows on
   all three collectors and take `TestValueMemory` from 2.3-3.3x to HotSpot's
   0.5x, and it is a project rather than a fix.

Do not start by raising the threshold or excluding the class: the test is
measuring something real, and two of the three arms pass it.
