---
name: zgc-nonblockingapi-fragmentation-oom-double-fault-hang-FIXED-20260813
description: FIXED 2026-08-13. TestNonBlockingAPI hung under ZGC after an OutOfMemoryError raised with 1.99 GB of a 2 GB heap free. Six defects. The dominant one is not fragmentation at all - a TLAB chunk is RESERVED space no collection can reclaim, its size was flat at 512 KiB however many threads a workload ran, and 4000 Tomcat threads x 512 KiB is the entire heap (proved by an A/B - CRATONVM_ZGC_TLAB=0 passes 44/44 in 244.7s). Under it - a fixed-size chunk request its own 96-bytes-short remnants could never serve, large objects laid out among those chunks, a GC trigger blind to reservations, no floor under the large-object region, and (nothing to do with GC) Thread.getUncaughtExceptionHandler returning null where HotSpot returns the ThreadGroup, which made EVERY uncaught exception in the VM an NPE inside the handler.
metadata:
  type: fixed-bug
  area: gc, zgc, tomcat, threading
---

# `TestNonBlockingAPI` hung under ZGC — and the fragmentation was a symptom

**FIXED 2026-08-13.** Retires
`docs/known-issues/tomcat/zgc-nonblockingapi-fragmentation-oom-double-fault-hang-20260813.md`.

The reported symptom: a process that ran to a 3000 s timeout with no output
after test 42 of 44, having raised `OutOfMemoryError` for a 2.7 MB array with
**1.99 GB of a 2.15 GB heap free**. The reporting page named three things and
said root-causing two of them was "not chased further here". All three are
resolved, and chasing them turned up three more.

## The instrument, first — because the existing one was one question short

The guard line already separated *fragmented* from *exhausted*:

```
request=2789392 used=2147351584 capacity=2147483648
free_list_bytes=1992876920 largest_free_block=524192 free_spans=3892
```

"Fragmented" can mean a live/dead mosaic that only relocation can serve, or a
handful of survivors holding gigabytes hostage. Those want opposite fixes.
`Arena::frag_profile` decides between them: it lays the free list out across
the arena, measures the occupied runs *between* the holes, and computes — one
two-pointer pass over prefix sums — **the cheapest contiguous window that could
serve the failing request**, i.e. the fewest live bytes that would have to move.
`ZgcRealHeap` then walks that window's walls and names the occupants' classes,
through a new `collector::set_class_namer` hook (the gc crate cannot name a
`Class`; the VM installs the resolver the same way it installs the
displaced-hash one, and it uses `try_read` so a diagnostic can never become a
hang).

First run with it:

```
zgc frag: spans=6148 largest_span=524192 walls=6147 wall_bytes=153977032
  span_hist=8:964 16:763 512:145 1K:20 2K:362 4K:38 8K:19 16K:7 64K:1 256K:3826
zgc frag: the CHEAPEST window that could serve this request — 544 live bytes in
  4 run(s) are all that stand between 2620720 free bytes spread over 2621264
  bytes of contiguous arena.
zgc frag: wall occupant class=java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionNode count=4 bytes=384
zgc frag: wall occupant class=java/util/concurrent/locks/AbstractQueuedSynchronizer$ExclusiveNode count=2 bytes=160
```

**544 bytes of AQS nodes holding 2.6 MB hostage**, and 3,826 free spans of about
512 KiB — one per `ZGC_TLAB_MAX_CHUNK`. Not a mosaic: a layout property.

## Defect 1 (dominant) — a TLAB chunk is a RESERVATION, and its size ignored the thread count

`largest_free_block` was `524192` in **every** run, byte for byte: `524288 - 96`.
A number that stable across runs is a cap, not a distribution — and the cap is
the chunk.

A chunk is not allocated memory, it is *claimed* memory: the part not yet handed
to an object belongs to no object and to no free list, and **no collection can
reclaim it while its owning thread is alive**. Its size therefore has to be a
function of how many threads are claiming one, and it was not — it was
`capacity / 1024` clamped to `ZGC_TLAB_MAX_CHUNK`, i.e. a flat 512 KiB on any
heap above 512 MiB. `TestNonBlockingAPI` runs ~4,000 threads, and
**4,000 × 512 KiB is 2 GB — the whole heap.**

The A/B is unambiguous, and it uses a switch that already existed:

| arm | result |
|---|---|
| ZGC, `-Xmx 2g`, TLAB on (before) | `OutOfMemoryError`, then no further output to the timeout |
| ZGC, `-Xmx 2g`, **`CRATONVM_ZGC_TLAB=0`** | **`OK (44 tests)`, rc=0, 244.7 s, zero OOM warnings** |

The chunk's own doc already contained the escape hatch — it notes that the
Hibernate class it was last raised for "passes on the same heap with
`CRATONVM_ZGC_TLAB=0`" — and read that as evidence about *hole granularity*.
It is also evidence about *reservation*, and reservation is the bigger half.

**Fix:** `ZGC_TLAB_RESERVATION_SHARE` bounds what all chunks may hold at once
(`capacity / 16`), and `ZArenaTlabRegistry::chunk_bytes_now` divides it by the
live buffer count, clamped into `[min_tlab_size(), ZGC_TLAB_MAX_CHUNK]`. Below
~256 threads at `-Xmx 2g` the clamp binds and the chunk is the same 512 KiB it
was, so ordinary workloads are untouched. The live count is an `AtomicUsize`
rather than `slots.len()` because a refill runs with its cell held and the
documented lock order is `slots -> cell -> arena`: reading the map there would
be an inversion.

## Defect 2 — a fixed-size chunk request its own remnants can never serve

A refill asked for exactly `ZGC_TLAB_MAX_CHUNK`. A retired chunk hands back only
the span below its survivors — and a thread that parks leaves one 96-byte
`AQS$ConditionNode` in its own chunk. So **every chunk that ever held a live
object comes back too small to ever be a chunk again**, and the fixed-size
request can only be answered by a bump. The free list filled with 3,826 spans
that were each *one small object* short of reusable.

**Fix:** `tlab_refill` accepts a shorter *recycled* chunk down to
`chunk / 8` (which is `max_tlab_alloc`, so a chunk at the floor still holds
eight of the largest object a TLAB will ever be asked for). The decision is
`recycled_chunk_size`, split out as a pure function: reproducing the shape
through the heap's API needs 4,000 threads and a 2 GB arena, and the decision is
one comparison, so the test asserts on the comparison — including on the literal
96.

## Defect 3 — large objects laid out among thread-private chunks

On a heap that does not compact, the largest servable request is the largest gap
between two survivors, so the allocator's layout policy *is* the OOM policy.
Everything shared one bump region, so a 2.7 MB array sat among 512 KiB chunks
each holding one long-lived AQS node — and one survivor per chunk caps every
hole in the heap at one chunk.

This mechanism has now produced three OOMs. It was answered once by raising the
chunk from 64 KiB to 512 KiB (Hibernate `sql.exec.SmokeTests`, 2026-08-11),
which moved the ceiling instead of removing it: a 65,552-byte `DFAState[8192]`
failed at 64 KiB and a 2,101,264-byte `char[]` failed at 512 KiB.

**Fix:** the arena has two ends. Small objects and TLAB chunks bump **up** from
the bottom; anything too big for any TLAB to serve
(`ZGC_LARGE_OBJECT_MIN = ZGC_TLAB_MAX_CHUNK / 8`) bumps **down** from the top,
with its own free list, coalescer and cursor retraction. A large object's only
possible neighbours are then other large objects, which are rarer by orders of
magnitude. The two ends share the middle; the boundary is wherever the cursors
meet, which is what makes the region test on a free block
(`offset >= high_cursor`) permanently exact. `ZGC_TLAB_MAX_CHUNK`'s doc now says
in as many words not to reach for that knob again.

## Defect 4 — a GC trigger blind to reservations

`allocated` — what `needs_gc`'s live-bytes term reads — never counts the part of
a chunk not yet handed to an object. The blind spot is documented as bounded by
`live_threads * chunk` and dismissed as "a few MiB on the thread counts this VM
runs". Here it was the entire heap: `allocated` sat around 150 MB while the
arena filled to 2.15 GB, so the live-bytes trigger never fired at all. The only
trigger that ever fired was `headroom_low`, and at a bare
`zgc_headroom_margin` it fires when the un-bumped middle is already down to
16 MB — long after the small-object end has bumped through everything else.

**Fix:** `headroom_low` is armed while the middle is still
`margin + unclaimed large-object reserve` wide, so the collection happens early
enough for retired chunk tails to reach the free list and refills to start
recycling. It cannot storm: `needs_gc` puts both terms behind the same
`gc_rearm` floor.

Same defect class as the `ZipContentTests` fix of 2026-08-10, whose defect 1 was
"the trigger asked about LIVE BYTES when the binding constraint is ALLOCATABLE
SPACE". This one asked about the whole arena when the binding constraint is one
end's share of it.

## Defect 5 — no floor under the large-object region

`Arena::set_high_reserve` (ZGC sets `capacity / 8`) keeps the small-object end's
*bump* out of the top until the large-object end has claimed that much. Two
details are load-bearing and both were wrong in the first attempt, each caught
by re-measuring rather than by review:

* **The reserve is the last preference given up, not the first.** Its override
  originally sat above the free-list retry, so the small-object end reached it
  on ordinary TLAB churn and drained the whole reserve through it (measured:
  the region got 92 MB of a 268 MB floor). It now runs after the free list, the
  un-reserved bump tail and the merge have all said no — and it takes from the
  large-object end's own *free list* first, since spending bytes that end has
  already claimed costs the reserve nothing.
* **The large-object cursor's retraction is gated at the floor.** Retracting
  moves bytes out of the high free list into the shared middle, where the
  small-object end takes them; ungated, every sweep handed the freed head back
  and the region arrived at the failing allocation holding **3,224 free bytes**.
  It now returns only the excess above the floor, partially.

## Defect 6 — every uncaught exception in the VM was an NPE, and it was not a GC bug at all

The reported page's "proximate, easy one":

```
Thread Thread-4888 terminated with error: ExceptionThrown(ObjectRef { ptr: 0x... })
(dispatchUncaughtException also failed: ExceptionThrown(ObjectRef { ptr: 0x... }))
```

Two raw pointers, naming neither throwable. Making that line print the class and
`detailMessage` of both — via `describe_throwable`, which reads them straight
out of the heap with **no Java invoke and no allocation**, because the
commonest reason handler dispatch fails is that the heap cannot serve one —
named the second exception on the next run:

```
java/lang/NullPointerException: Cannot invoke
"java.lang.Thread$UncaughtExceptionHandler.uncaughtException(java.lang.Thread, java.lang.Throwable)"
because the return value of "java.lang.Thread.getUncaughtExceptionHandler()" is null
```

`probes/UncaughtProbe.java` (start a thread that throws; print the handler)
isolates it with no GC involvement whatsoever:

| | `getThreadGroup()` | `getUncaughtExceptionHandler()` | result |
|---|---|---|---|
| HotSpot 25 | `ThreadGroup[name=main,maxpri=10]` | `ThreadGroup[name=main,maxpri=10]` | stack trace printed |
| CratonVM, before | `ThreadGroup[name=main,maxpri=10]` | **`null`** | NPE inside the handler |
| CratonVM, after | `ThreadGroup[name=main,maxpri=10]` | `ThreadGroup[name=main,maxpri=10]` | stack trace printed |

The registered `Thread.getUncaughtExceptionHandler` native walked per-instance
side table -> real field -> process default -> **null**, and its own comment
called that "an approximation". It was not an approximation, it was the default
case: `ThreadGroup` *implements* `UncaughtExceptionHandler`, the JDK returns it
whenever no per-thread handler is set, and its `uncaughtException` is what
prints `Exception in thread "..."`. Worse,
`Thread.dispatchUncaughtException` — the fallback this VM invokes when it finds
no handler of its own — is literally
`getUncaughtExceptionHandler().uncaughtException(this, e)`, so `null` raised an
NPE *inside the uncaught-exception handler*, for **every uncaught exception on
every thread with no explicit handler, in every workload, on every collector**.

**Fix:** `real_thread_group` adds the missing rung (`holder.group`, with the
pre-JDK-19 `Thread.group` layout as a fallback).

## Also fixed: a handled OOM that reported itself as a VM crash

The reporting page filed "root cause of the native method panic" as an open
question. There was no native panic:

```
ERROR ... Native method panic caught in <cb@0x...>: unknown native method panic
```

`safe_native_call`'s payload ladder downcasts to `String` then `&str`, and
`NativeAllocOom` is neither — so the working, catchable heap-exhaustion path
fell through to "unknown native method panic" and was logged at ERROR with a
callback address and a Java frame. It now reports what happened, at the severity
it has. (Not as an early return: the funnel's shared tail — pin-watermark
truncation, the `native_pending_return` clear, the unpin ring — still has to
run, which the original code's comment is explicit about.)

## Verification

`org.apache.catalina.nonblocking.TestNonBlockingAPI`, ZGC (the default),
`-Xmx 2g`, one class per process, same Windows fixture:

| arm | JUnit | rc | wall | OOM warnings |
|---|---|---|---|---:|
| baseline `origin/dev` | no summary — died at sub-test 42 | **124 (timeout)** | 900 s | 1, then silence |
| **this branch** | **`OK (44 tests)`** | **0** | **279.3 s** | **0** |
| control: `CRATONVM_ZGC_TLAB=0`, this branch | `OK (44 tests)` | 0 | 244.7 s | 0 |

The control arm is what identified defect 1 and is kept as the ceiling: with
TLABs off entirely the class takes 244.7 s, so the bounded-reservation TLAB
costs ~14% against no TLAB at all on the most thread-heavy class in the suite,
and buys back a hang.

`cratonvm-gc` unit tests: **1504 pass**, 13 of them new — 8 for the two-ended
arena and its reserve, 3 for the recycled-chunk decision, 2 for the reservation
budget.

**The throughput cost, measured rather than assumed.** `TestTomcat` (an
ordinary, non-thread-heavy class), interleaved base/branch, two rounds each on
the same loaded host:

| | round 1 | round 2 | mean |
|---|---:|---:|---:|
| baseline | 32.7 s | 36.8 s | 34.8 s |
| this branch | 34.3 s | 38.4 s | 36.4 s |

**~4.6% slower**, and the branch is slower in both rounds, so it is probably
real rather than host noise — the earlier-firing `headroom_low` is the likely
cause. That is the price of the trade and it is worth stating plainly: this
class does not have enough threads for the chunk to shrink at all (the clamp
binds below ~256 threads), so what it is paying for is the trigger, not the
TLAB change. A wider re-measurement belongs with the suite re-baseline in
feature-designs/zgc-maturity-assessment-and-plan-20260813.md, Phase 1.

Every new test was watched to **fail before it passed**: stubbing `alloc_high`
to delegate to `alloc` fails all five region tests; stubbing the high retraction
to `0` fails two; the recycled-chunk tests were written against the literal 96.

The pre-existing flake `g1::tests::parallel_matches_serial_no_loss_or_dup`
(`ObjectRef pointer not 8-byte aligned: 0x19f`) reproduces on a pristine
`origin/dev` checkout at ~1 failure in 12 full-suite runs and passes 6/6 in
isolation; G1 does not use `Arena` at all, so it is not this change. It is
filed separately.

## What generalises

* **A reservation is invisible to a live-bytes trigger.** Any allocator that
  hands out space in units larger than the objects it holds needs its unit count
  bounded and its trigger able to see the units. "Bounded by
  `live_threads * chunk`" is only a bound if something bounds `live_threads`.
* **A fixed-size request that its own recycled remnants cannot satisfy is a
  one-way consumer.** The free list can be enormous and the allocator still be
  bump-only. `largest_free_block` identical across runs is the tell.
* **On a non-compacting heap the allocator's layout policy is the OOM policy.**
  Two populations with different lifetimes in one bump region means the
  shorter-lived one's survivors set the ceiling for the longer-lived one.
* **The kill switch is an experiment, not just an escape hatch.**
  `CRATONVM_ZGC_TLAB=0` answered in one run what four builds of allocator
  changes had not. It was already documented on the constant, as a remark
  rather than as a measurement to take.
* **Name the second exception.** The double fault was diagnosed the instant the
  log said which exception it was — and the reporter had to be written so it
  could not itself fail on the heap that had just failed.
