---
name: zgc-oom-with-84-percent-of-the-heap-free-FIXED-20260810
description: FIXED 2026-08-10. ZipContentTests died with OutOfMemoryError at the Spring Boot suite's default -Xmx 2g from the day ZGC became the default collector. Two independent defects, both specific to a collector that does not compact - the GC trigger asked about LIVE BYTES when the binding constraint is ALLOCATABLE SPACE (fired at 66.8% live while an 8 KB allocation was failing), and the arena's bump cursor was a ONE-WAY RATCHET, so a 16 MB array was unservable forever with 1.81 GB free and a 1.05 MB largest hole. 29/29 PASS on both collectors after.
metadata:
  type: fixed-bug
  area: gc, zgc, springboot
---

# ZGC raised `OutOfMemoryError` with 84% of the heap free

**FIXED 2026-08-10.** `org.springframework.boot.loader.zip.ZipContentTests` now
passes **29/29 at `-Xmx 2g` under ZGC**, and 29/29 under Generational in the same
build. All 1463 `cratonvm-gc` unit tests pass.

## The report

The day ZGC became the default collector (`f9fc4d776`), the class began failing
at the suite's *default* heap:

```
SBRUNNER_LOAD_FAIL org.springframework.boot.loader.zip.ZipContentTests ::
java.lang.OutOfMemoryError: Java heap space (native primitive array of length 8192)
  at java.io.InputStreamReader.read / BufferedReader.fill / BufferedReader.readLine
SBRUNNER_RESULT tests=0 failed=0 aborted=0 skipped=0 containersFailed=1
```

The same class at the same heap passed 29/29 repeatedly before the flip, and
`-XX:+UseGenerationalGC` restored it. The ZGC-default merge had predicted the
shape — it says ZGC "costs ~1.5x heap because it does not compact" — and noted
"The Spring Boot suite has not been re-run under ZGC". This was the first
Spring Boot data point on it.

"Needs more heap" turned out to be wrong twice over.

## Defect 1 — the trigger asked about live bytes

The reproduction's decisive line:

```
[GC] zgc-real: collections=10 occupancy=1434932032/2147483648 bytes
```

Ten collections had already run, and live was 1.435 GB of 2.147 GB — **66.8%**,
under the 75% `gc_threshold`. So at the moment the failing allocation raised
`OutOfMemoryError`, `needs_gc()` was answering **"no collection needed"**.

It was answering the wrong question. `needs_gc` tests
`allocated >= gc_threshold`, and the sweep stores *retained* bytes back into
`allocated` (`occupancy` is documented as "the post-sweep live figure … unlike a
bump cursor"). That is a LIVE-BYTES predicate, and it is correct on a compacting
heap, where live bytes and allocatable space move together.

This heap does not compact. The bump cursor never rewinds and reclaimed space
returns only as free-list holes, so allocatable space is
`max(capacity - cursor, largest_free_block)` — and it falls as the **garbage**
grows, which is exactly the quantity the sweep subtracts back out of
`allocated`. The two diverge by however much garbage exists.

**Fix:** a second, independent trigger term, `headroom_low`, armed under the
arena lock in `alloc_raw` once the arena can no longer serve a
`zgc_headroom_margin` request (`capacity/128`, floor 8 MiB → 16 MiB at
`-Xmx 2g`). The margin exists because a native cannot collect where it stands:
it must keep serving until `vm_exec`'s native-boundary hook can run a cycle.

Both terms sit behind the **same `gc_rearm` floor**, so this cannot recreate the
GC-storm livelock that field exists to prevent — right after a sweep `gc_rearm`
exceeds `allocated`, so a still-low headroom waits for genuinely new allocation
instead of firing a cycle per allocation. Arming is off the hot path: while the
un-bumped tail is above the margin (the whole of a normal run) it is two field
reads and a compare.

Result: live at failure fell from 66.8% to **15.1%**, and the 8 KB failure went
away — but the class still died.

## Defect 2 — the bump cursor was a one-way ratchet

The failure moved to a **16 MB** request, and the failure report added for this
investigation named the residual outright:

```
request=16777232  used=2130722576  capacity=2147483648
free_list_bytes=1805614192  largest_free_block=1072552
```

**1.81 GB free; largest contiguous hole 1.05 MB.** `FileCopyUtils.copy`
(`ZipContentTests.loadWhenHasFrontMatterOpensZip`) reads a file into a single
16 MB `byte[]`, and there was nowhere to put it while 84% of the heap was
garbage.

Read `used` against `capacity`: 2,130,722,576 of 2,147,483,648. The cursor never
rewinds, so it is a **one-way ratchet** — once a process has cumulatively
allocated its capacity, the un-bumped tail is gone for the rest of the run and
every later request must fit a free-list hole, however little is live. The tail
had shrunk to 16,761,072 bytes against a 16,777,232-byte request: short by
**16 KB**, and able only to shrink.

Coalescing was working — it had built 1 MB spans — but it cannot merge across a
live object, and ~325 MB of survivors scattered through 2.13 GB chop it into
~1 MB pieces. No collection can serve 16 MB there, because serving it means
*moving* survivors, and this collector does not relocate.

**Fix:** objects die young, so the top of the arena is usually all garbage.
After the sweep coalesces, if the topmost span ends exactly at the cursor, the
cursor retreats into it (`Arena::retract_cursor_into_free_tail`) — restoring a
large CONTIGUOUS region, the one thing a free list of scattered holes cannot
offer, without relocating anything. One comparison per sweep.

## Verification

| arm | result | CPU | collections | occupancy |
|---|---|---:|---:|---|
| ZGC, `-Xmx 2g` | **29/29 PASS** | 266.03 s | 12 | 466,863,456 (21.7%) |
| Generational, `-Xmx 2g` (control) | 29/29 PASS | 256.86 s | — | — |

The generational arm is run every time on purpose: a ZGC fix must not land by
breaking the collector that already worked. CPU is comparable, so the extra
collections are not costing throughput.

Two unit tests, both watched to **fail before they passed** (the retraction was
temporarily stubbed to `0`; the first then failed on `left: 0, right: 2816`):

* `a_wholly_free_tail_is_handed_back_to_the_bump_cursor` — built so it cannot
  pass by accident. The free tail is deliberately not the largest block and a
  live object sits above the interior hole, so a retraction returning *any* free
  block rather than the one ending AT the cursor would hand out live bytes, and
  one that merely coalesced would not move the cursor. It also asserts the
  retracted span leaves the free list, or those bytes would be served twice.
* `a_free_hole_below_a_live_object_does_not_move_the_cursor` — the other half of
  the contract. Getting this wrong is heap corruption, not a missed
  optimisation.

## What made this diagnosable

The first commit of the fix was not a fix. `alloc_raw` returned `None` and the
Java-level error named only the request size, so "genuinely full of live data"
and "has the bytes, no hole this big" — which want completely different fixes —
were indistinguishable from outside. The failure path now reports
`request / used / capacity / free_list_bytes / largest_free_block` once.

That report is what turned defect 2 from a guess into a measurement, and it is
the piece most worth keeping: on a non-compacting collector, exhaustion by
fragmentation is the characteristic failure, and it was the one the error could
not name.

## Residual

Still true, and not addressed here: this collector cannot serve a large
allocation whose size exceeds the largest hole when survivors are scattered
above the cursor. The retraction fixes the common case (a garbage tail) but not
the general one, which needs either relocation or a separate humongous-object
region. Nothing in the Spring Boot suite is known to hit it after this fix; a
workload that keeps a live object pinned near the top of the arena while
demanding multi-megabyte arrays would.

Also open: the Spring Boot suite as a whole has still not been re-run under the
ZGC default. This class was the first data point; others may sit on the same
1.5x-heap cliff for unrelated reasons.
