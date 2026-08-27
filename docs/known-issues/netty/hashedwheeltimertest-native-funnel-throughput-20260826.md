# `HashedWheelTimerTest.testExecutionOnTime` — confirmed CratonVM-specific, root-caused as native-dispatch funnel cost, not a wheel bug

## Status
**OPEN, root-caused, not fixed.** Pulled back from
`fixed-suite-bugs/netty/hashedwheeltimertest-late-task-firing-RETIRED-20260819.md`
(full investigation, kept as the detailed record) — this page is the public-facing
summary. Reproduces again in the 2026-08-26 complete-suite ZGC×4-shard run
(`testExecutionOnTime`: "Timeout + 100000 delay 739 must be 125 < 650").

## The bound and why it's real margin, not a tight test

`HashedWheelTimer` fires a task scheduled at relative time `r` with a 125ms delay
and a 200ms tick at `(floor((r+125)/200)+1) * 200`. The worst case a *correct*
wheel can produce is `tickDuration + timeout = 325ms`; the test's `maxTimeout`
bound of 650ms is double that — a 100% margin over the algorithm's own worst
case, not a knife-edge assertion.

## Confirmed: not a wheel bug, not host noise

- Isolated HotSpot 25, same host: `found=14 ok=14 failed=0`, repeatably.
- Isolated CratonVM, same host, both the default and ZGC collectors: fails the
  same one method consistently.
- Nothing ever fires early on either VM at any scale (rules out an off-by-one
  bucket selection).
- At N ≤ 10,000 tasks CratonVM's max delay is inside the 325ms correctness bound
  — only N = 100,000 (the test's actual scale) breaks it.

## Root cause: an overload cascade in one worker thread, not a uniform slowdown

At N = 100,000, the single timer worker thread must, inside one tick: transfer
100,000 pending timeouts into buckets, then expire a bucket's worth of tasks.
Measured:

| phase | CratonVM | HotSpot |
|---|---:|---:|
| transfer 100,000 timeouts | ~116 ms | ~12 ms |
| expire + run 100,000 tasks | ~553 ms | ~22 ms |
| **worker total** | **~669 ms** | **~34 ms** |

The test's bound needs the worker to finish in under ~450ms; it takes ~669ms —
**1.5x** over. Once a bucket takes longer than the 200ms tick to drain, the wait
loop returns immediately next iteration and the lag compounds, producing the
long tail (p50 597ms, max 869ms at N=100,000) rather than a fixed offset.

Priced per expired task: `--dump-native-registry` shows 23.2 native calls per
task (`AbstractOwnableSynchronizer.setExclusiveOwnerThread`, `AtomicInteger.get`,
`System.nanoTime`, `VarHandle.setRelease`/`getVolatile`/`compareAndSet`,
`Thread.interrupted`, `TimeUnit.toNanos`/`toMillis`, `Long.valueOf`/`longValue`,
`AtomicLong.increment/decrementAndGet`). `LinkedBlockingQueue.add` alone is two
uncontended `ReentrantLock` pairs and is 76% of the per-task cost; the AQS lock
pairs inside it are 56% of the whole. This is the same native-dispatch funnel
floor (`try_jit_site_cached_native_dispatch`, `safe_native_call_impl`,
`ZObjectStarts::contains`, receiver-validation machinery) that other CratonVM
throughput pages attribute ~300ns/call to — see the internal page's "Related"
section for the full chain of native-funnel-cost investigations this shares
with.

## Not a fix, just a step

`Long.valueOf`/`longValue` got a thin direct bind (already landed on a prior
branch) — 2 of the 23 natives per task, doesn't close the 1.5x gap. Closing it
needs the per-call native-dispatch floor itself down, which several other
campaigns already own and have priced at ~300ns/call as close to a hard floor
without a JIT intrinsic for the specific hot paths involved.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.util.HashedWheelTimerTest
```

## Related
- Full investigation: `fixed-suite-bugs/netty/hashedwheeltimertest-late-task-firing-RETIRED-20260819.md`
