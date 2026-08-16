# ZGC threw `OutOfMemoryError` on allocation churn that G1 and HotSpot survive

**Status: FIXED 2026-08-16.** The slide threw the whole low free list away on
every compaction, including the holes it had not written into. On a churn
workload that is the entire heap: the sweep free-listed 400 MB, the slide
cleared it, and from then on the bump cursor — which never rewinds — advanced
by exactly the bytes allocated until it reached capacity and the VM threw
`OutOfMemoryError` with 99 % of the heap dead.

Found while measuring the fragmentation cost the retired
`zgc-resourceleakdetector-corpse-read` write-up left as its open question.

## The failure

`repros/frag-churn/FragProbe.java` — 4 000 000 allocations of 64 B .. 8 KiB
through one method, with a 512-entry rolling window of live references, in a
512 MiB heap:

```
Exception in thread "main" java/lang/OutOfMemoryError: Java heap space (alloc_array length 5661)
```

A 5 661-byte array in a 512 MiB heap whose live set is 2.1 MB.

| collector | `--nojit` before | `--nojit` after |
|---|---|---|
| ZGC | **OOME**, 3 collections | completes, 90 collections |
| generational (`-XX:+UseGenerationalGC`) | completes | completes |
| G1 | completes | completes |
| HotSpot 25 | completes | — |

**One correction to this page as first written.** It listed "generational
(default)" as also failing. That row was ZGC under another name — `Zgc` *is*
the default `GcAlgorithm`, so the unflagged arm was never the generational
heap. Run with `-XX:+UseGenerationalGC` it completes, before and after. Only
ZGC was ever affected, which the fix's location makes obvious in hindsight.

## What the numbers said

Two lines added to the sweep — what it freed, and what the arena had to show
for it, measured under the same lock — settle it in one read
(`--verbose:gc`, `[GC] zgc-reclaim`), here with the TLAB off so every byte goes
through one path:

| cycle | `bytes_freed` | `free_list_bytes` | `cursor` | allocated since |
|---|---|---|---|---|
| 1 | 400 489 568 | 400 489 568 | 402 655 472 | 402 655 472 |
| 2 | 133 666 840 | 133 666 840 | 536 335 864 | 133 680 392 |
| 3 | 519 464 | 519 464 | 536 870 488 | 534 624 |
| 4 | 0 | 0 | 536 870 848 | 360 |

Read the last two columns together: **the cursor advances by exactly the bytes
allocated, every cycle.** Not one byte ever comes out of the free list — and
the free list starts each cycle at exactly what that cycle's sweep put in it,
so whatever the previous cycle left was gone before the mutators ran.

## Root cause

`Arena::compact_low_to` ended with `clear_low_free_list()`, on a stated
premise:

> The low free list is dropped wholesale rather than filtered: after a slide
> every low hole is inside the reclaimed span by construction, so a surviving
> entry would name bytes that are now un-bumped tail and would hand them out
> twice.

That is true of a slide which compacts the **whole** low region. ZGC's
compacts the pages its relocation-set selector picked, into
`[slide_floor, dest)`, and leaves every other byte exactly where it was. A
hole outside that window is a real free block below the cursor — and the sweep
never re-discovers it, because the sweep only free-lists objects that **die**.
A hole that was already free when the slide ran is lost permanently.

It is also why the arm **with the JIT on survives**: `relocate_stw` declines to
relocate while a compiled frame is live (64 of 68 cycles on this workload), so
`compact_low_to` is barely called and the free list is left alone. The
JIT-quiescence fix was masking this one, exactly as it masked the rewrite-pass
defect.

## The fix

`compact_low_to` now takes the destination window the slide actually wrote
into and keeps the holes outside it. A block is dropped if it overlaps that
window (a survivor may be sitting on it — the slide places survivors without
consulting this list) or lies at or above the new cursor (those bytes are
un-bumped tail, and serving them from both the list and the cursor is the
double-hand-out the wholesale clear was guarding against). A block straddling
the cursor is truncated rather than dropped.

`compaction_keeps_the_free_holes_it_did_not_write_into` pins all three cases.

## What is NOT fixed

**ZGC serves fewer large contiguous blocks than G1 on this workload**: after
the fix the probe's closing assay gets 27 blocks of 4 MiB under `--nojit`
against G1's 101 and the generational heap's 94. That is fragmentation, not
failure — the run completes — and it is the same collector-shape question the
retired corpse-read page's cost table raised. Not chased here.

## Repro

```bash
javac docs/known-issues/repros/frag-churn/FragProbe.java
CRATONVM_GC_STATS=1 <cratonvm> --java-home <jdk-25> --Xmx 512m \
  -XX:+UseZGC --nojit --verbose:gc -cp . FragProbe 200 20000
```

`[GC] zgc-reclaim` is the line to read: `bytes_freed` large beside a small
`free_list_bytes` is memory that was swept and then lost.
