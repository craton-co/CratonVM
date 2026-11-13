# Gen: on chain-fill OOME probes, full collections cost ~10x HotSpot's each and run 5-10x as often

> **STATUS (2026-10-29, filed while retiring
> :
> OPEN; phase/cadence diagnosis added 2026-11-02, no source fix yet.** Same on dev `7788214711` and on that page's fix; stdout is
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

Related retired correctness record: `pinned-copy-pause-javac-correctness-RETIRED-20261030`
(the default-mode javac corruption check; its historical pause-performance gap was out of scope for retirement).


## 2026-11-01 fresh Gen pause breakdown (OPEN)

A fresh exact-source `Gcd1PinnedCalleeOomeProbe -Xmx64m` process preserved all three OOME recovery facts and `PASS`. CratonVM Generational GC took 4.19 s wall (4.46 s user, 0.78 s system); shutdown stats reported 28 minor and 19 major collections, 19 mark-sweep-compact collections / 2,976 ms, and 24 copy collections / 459 ms. The OOME ladder recorded 20 forced collections (9 unproductive), 3 ladder majors, and one debt payment. Its 28 timed pauses summed to 3,434.7 ms, with a 254.8 ms maximum. The largest named phase totals were `major_check` 152.7 ms and `evac_drain` 121.6 ms; this does not yet attribute the 2,976 ms mark-sweep-compact total to its internal phases. A fresh HotSpot SerialGC control passed in 0.39 s wall. The collectors differ, so these are diagnostic, not a matched pricing claim. Logs: `/tmp/tier1-gen-probes-20261101/cvm-gcd1-gen-timed.{log,time}` and `hotspot-gcd1-serial-timed.{log,time}` on host `64.177.43.150`.

## 2026-11-02 release phase and trigger diagnosis (OPEN)

A fresh full-LTO release build from `dev` `28023eade6` and fresh processes on
host `64.177.43.150` narrowed the two costs. The probe fixtures are in
`/tmp/tier1-gen-probes-20261101`.

At `-Xmx64m`, CratonVM `--compatible -XX:+UseGenerationalGC` passed all three
recovery facts in 4.64 s wall. A HotSpot 25 G1 run passed the same stdout in
0.30 s; HotSpot reported 6 `Pause Full` collections including the explicit
`System.gc()`, and 16 young pauses. This is useful as a generational workload
control, not a same-collector pricing claim. CratonVM reported 17 mark-sweep-
compact collections (2,930 ms) and 25 copy collections (514 ms). Its release
`[gcphase]` trace put repeated major marking at roughly 55–110 ms and pin-aware
compaction at 52–116 ms per collection. Steady marks popped about 1.20 million
objects; one parallel drain handled only about 20 thousand. Disabling
`CRATONVM_GC_OLD_PAR_MARK` kept the same 17 collections and facts, at 4.16 s
wall with 3,011 ms mark-sweep-compact time, so that drain is not the fix. Logs:
`/tmp/tier1-gen-oome-release-{plain.log,plain.time,par-off.log,par-off.time}`
and `/tmp/tier1-gen-oome-hotspot-g1.{log,time,gc.log}`.

At `-Xmx256m`, the CratonVM Generational `GenR4W5JitOomRetentionProbe` passed in
2:03.54 (142.62 s user, 8.53 s system). It recorded 155 majors across 183 young
decisions, 145 back-to-back majors, 150 majors freeing under 5% of capacity, and
107 allocation-failure plus 6 concurrent-start-late causes. The OOME ladder
recorded 150 forced collections and 42 ladder majors over three OOMEs. Fresh
HotSpot 25 SerialGC passed in 4.04 s with 22 full and 6 young collections. The
collector mismatch limits the timing comparison, but the repeated-major count is
confirmed. HotSpot G1 was also tried and printed `FAIL` on the probe's third
recovery scenario; it is excluded as an acceptance control. Logs:
`/tmp/tier1-gen-oome-retention-release.{log,time}`, `/tmp/tier1-gen-oome-
retention-hotspot-{serial,g1}.{log,time,gc.log}`.

The existing opt-in `CRATONVM_GC_OLD_TRIGGER_SERIAL=1` did not fix the count:
the same probe passed in 2:19.69 with 155 majors, 152 low-yield majors, 108
allocation-failure causes, and 7 concurrent-start-late causes. Logs:
`/tmp/tier1-gen-oome-retention-serial-trigger.{log,time}`.

The measured root cause is repeated old-gen mark/compact work under allocation-
failure and late-concurrent-start pressure, with a linked live chain forcing a
large serial mark on each pass. The opt-in Serial trigger and the old-mark
parallel drain are ruled out as fixes. No source repair or acceptance
improvement was established; this page remains OPEN. The remaining work is to
reduce the repeated major work or its per-pause cost without losing OOME
recovery, then rerun all three OOME probes in Real-JDK and Compatible against fresh
HotSpot controls.

The companion `GenR4W6JitOomRootProbe` also passed all eight facts in Compatible
mode in 2:35.71. It recorded 224 majors across 270 young decisions, 213 back-to-
back and 219 low-yield majors (159 allocation-failure, 9 concurrent- start-late,
56 requested). HotSpot SerialGC passed the same eight facts in 6.01 s with 39
full and 14 young collections. Logs: `/tmp/tier1-gen-oome-root-
release.{log,time}` and `/tmp/tier1-gen-oome-root-hotspot-
serial.{log,time,gc.log}`.

Fresh Real-JDK release runs passed the same 64 MB, retention, and root probes. Real-JDK's Java-visible OOME facts
matched HotSpot SerialGC (`diff=0` after filtering CratonVM shutdown
diagnostics). At 64 MB it took 6.19 s and reported the same 17 majors; the
retention probe took 2:04.61 with 148 majors (146 low-yield, 137 back-to-back);
the root probe took 2:02.27 with 178 majors (173 low-yield, 166 back-to-back).
The root probe's HotSpot SerialGC control passed all eight facts in 6.01 s with
39 full and 14 young collections. Logs: `/tmp/tier1-gen-oome-
jdk-{64,retention,root}.{log,time}`, `/tmp/tier1-gen-oome-root-hotspot-
serial.{log,time,gc.log}`.


## 2026-11-01 exact-pushed-head recheck (OPEN)

The release binary built from pushed `dev` `485d70ff85ca19adbd11c1440aef05ca4e1217f2` passed the 64 MiB `Gcd1PinnedCalleeOomeProbe`, all six Java-visible retention facts, and all eight root facts in both Real-JDK and Compatible modes. Fresh HotSpot 25 SerialGC controls passed the same deterministic facts; direct fact extraction confirmed parity for all three probes.

At 64 MiB, Real-JDK / Compatible took 6.76 / 6.04 s and recorded 20 / 16 major collections. At 256 MiB, retention took 84.72 / 94.10 s with 147 / 155 majors and 72,170 / 80,623 ms in mark-sweep-compact; its allocation ladders recorded 36 / 42 majors across three OOMEs. The root probe took 94.38 / 106.31 s with 226 / 211 majors and 79,461 / 86,334 ms in mark-sweep-compact; its ladders recorded 46 / 76 majors across six OOMEs. HotSpot SerialGC controls took 0.40 / 2.74 / 5.67 s for the 64 MiB, retention, and root probes. The runtimes use different collectors, so these timings are diagnostic rather than matched pricing. The repeated-major and per-pause costs remain; no source repair or closure is established. Logs and `/usr/bin/time` captures are `/tmp/tier1-w13-acceptance-20261102/gen-{hotspot,jdk,compat}-{64,retention,root}.{log,time}`.


## 2026-11-02 exact merged-head recheck (OPEN)

The clean release binary from pushed `dev` `52865679eec8eaf5f32ccbfca739da901251ee0` passed the 64 MiB recovery probe, all six retention facts, and all eight root facts in both Real-JDK and Compatible mode. Major counts were 17 / 19 at 64 MiB, 154 / 152 for retention, and 225 / 219 for root (Real-JDK / Compatible). HotSpot SerialGC parity controls from the preceding exact-head validation passed the same deterministic facts. These repeatable facts do not repair the repeated-major cost; Gen OOME remains OPEN. Logs and time captures: `/tmp/tier1-w13-acceptance-20261102/postmerge-gen-{jdk,compat}-{64,retention,root}.{log,time}`.
