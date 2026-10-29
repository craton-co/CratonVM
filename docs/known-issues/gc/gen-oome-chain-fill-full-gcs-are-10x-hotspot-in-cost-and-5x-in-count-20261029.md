# Gen: on chain-fill OOME probes, full collections cost ~10x HotSpot's each and run 5-10x as often

> **STATUS (2026-10-29, filed while retiring
> `docs/internal/gc/gen-gcd1-pinned-callee-oome-probe-crawls-unaged-young-sweeps-FIXED-20261028.md`):
> OPEN, measured, not investigated.** Same on dev `7788214711` and on that page's fix; stdout is
> SAME as HotSpot everywhere. This is a time gap, not a correctness gap.

## Symptom

The generational OOM probes that fill the heap with a linked chain until `OutOfMemoryError`
(`tools/bench/Gcd1PinnedCalleeOomeProbe`, `GenR4W5JitOomRetentionProbe`, `GenR4W6JitOomRootProbe`)
print HotSpot's output but take 10-80x HotSpot's time. Host 64.177.43.150 (16 cores, load 24-58
from other sessions), release frame-pointer builds, `-XX:+UseSerialGC`, `CRATONVM_GC_STATS=1`;
HotSpot 25.0.4 with `-Xlog:gc`:

| probe | heap | HotSpot wall | HotSpot young / full GCs | CratonVM wall | CratonVM full GCs |
|---|---|---|---|---|---|
| `Gcd1PinnedCalleeOomeProbe` | 64m | 1.3 s | 3 / 11 | 7-18 s | 13 |
| `Gcd1PinnedCalleeOomeProbe` | 256m | - | 2 / 11 | 105-119 s | - |
| `GenR4W5JitOomRetentionProbe` | 256m | 5.5 s | 6 / 22 | 160-454 s | 146 |
| `GenR4W6JitOomRootProbe` | 256m | - | 14 / 43 | 243-589 s (CPU 226-262 s) | 197-227 |

Two separate factors multiply:

1. **Per-pause cost.** At `-Xmx64m` both run the same shape (`Gcd1PinnedCalleeOomeProbe`: HotSpot 10
   full GCs of a ~59 MB chain before the error). HotSpot's full GCs take 50-120 ms each; ours
   ~600 ms-1 s (`marksweepcompact_collections=13 marksweepcompact_time_ms=7975`, `[gcpause]
   collection took 840-1590ms`). The pinned young copy cycles of the growth phase also take 120-750 ms
   where HotSpot's young pauses of the same survivors take ~45 ms.
2. **Count.** At `-Xmx256m` the allocation-failure ladder runs far more full collections per error
   than HotSpot: `ladder_forced_per_oome` ~46, `ladder_majors_per_oome` ~12.7 on
   `GenR4W5JitOomRetentionProbe` (3 errors, 146 full collections against HotSpot's 22).

## What is not the cause

- Not the crawl of the retired gcd1 page: that was a young-sweep livelock (fixed); both factors are
  present on runs that never crawled, on dev and on the fix alike, with the same collection counts.
- Not load: the per-pause ratio holds on CPU time (`/usr/bin/time` user+sys), and the count is
  load-independent.

## First steps

1. Per-pause: `CRATONVM_DBG=gcpause` on `Gcd1PinnedCalleeOomeProbe -Xmx64m` names the phases of the
   ~1 s collections only as `collection took`; split the full collection's phases (mark, plan,
   compact, fixup) and compare bytes/s against HotSpot's `-Xlog:gc+phases=debug`.
2. Count: diff the ladder's rungs (`[GC] oome_ladder:`, `ladder_deciding`, `ladder_second_majors`)
   against HotSpot's sequence (allocation failure -> young -> full -> full with soft refs cleared ->
   error) on `GenR4W5JitOomRetentionProbe -Xmx256m`; find which rungs HotSpot does not have.

Related open page: [`pinned-copy-pause-is-5x-hotspot-serial-on-jit-warm-javac-20261027`](pinned-copy-pause-is-5x-hotspot-serial-on-jit-warm-javac-20261027.md)
(the young pinned-copy pause, factor 1's young half).
