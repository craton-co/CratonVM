# `HashedWheelTimerTest.testExecutionOnTime` — the wheel is right; the VM cannot drain 100 000 tasks inside the 325 ms of slack the test leaves

**Status: RETIRED 2026-08-19 as a page — root-caused, and the residual re-homed
to the workstream that owns it.** The class still fails. What changed is that
the two hypotheses this page opened with are now one refuted and one confirmed,
with the arithmetic that decides between them.

Superseded page: `known-issues/netty/hashedwheeltimertest-late-task-firing-20260819.md`
(the OPEN version, which said "not yet root-caused").

## The two hypotheses, decided

The open page offered:

1. **a genuine `HashedWheelTimer` bucket-firing bug** — an off-by-one tick;
2. **a throughput symptom** under this test's specific load shape.

(1) is **refuted**. (2) is **confirmed**, and the mechanism is an overload
cascade rather than a uniform slowdown.

## The instrument

`probes/HwtScaleProbe.java` runs `testExecutionOnTime`'s exact shape — 200 ms
tick, 125 ms requested delay, one `TimerTask` per iteration appending
`nanoTime - start` to a `LinkedBlockingQueue<Long>` — at four scales, and prints
the whole delay distribution instead of the first value that breaks the bound.
The test itself only ever runs N = 100 000 and only reports the first failure,
which is why it read as a knife-edge miss at exactly 650.

Azure host 2 (Linux x86_64, 8 cores), JDK 25, dev `c6c81ec5b`, real-JDK mode,
engine-default collector, host load ~4 on both arms, interleaved.

**HotSpot 25 (the control):**

| n | schedMs | drainMs | min | p50 | p99 | max | over(>=650) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 0 | 200 | 200 | 200 | 200 | 200 | 0 |
| 1 000 | 1 | 202 | 201 | 201 | 202 | 202 | 0 |
| 10 000 | 2 | 212 | 202 | 207 | 210 | 210 | 0 |
| 100 000 | 9 | 234 | 212 | 224 | 234 | 234 | 0 |

**CratonVM:**

| n | schedMs | drainMs | min | p50 | p99 | max | over(>=650) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 2 | 203 | 202 | 202 | 203 | 203 | 0 |
| 1 000 | 8 | 223 | 209 | 217 | 220 | 220 | 0 |
| 10 000 | 43 | 257 | 215 | 242 | 257 | 257 | 0 |
| 100 000 | 198 | 869 | **316** | 597 | 864 | **869** | **40 867** |

## Why that refutes the wheel bug

`HashedWheelTimer`'s own arithmetic bounds a correct run. `newTimeout` stores
`deadline = nanoTime + delay - startTime`; `transferTimeoutsToBuckets` computes
`calculated = deadline / tickDuration` and files the task in bucket
`max(calculated, tick)`; the worker processes bucket *k* at wall time
`(k+1) * tickDuration`. So a task scheduled at relative time `r` fires at
`(floor((r + 125) / 200) + 1) * 200`, and the largest delay a **correct** wheel
can produce is

> `tickDuration + timeout` = 200 + 125 = **325 ms**

reached at `r` just past 75 ms (mod 200). `maxTimeout` is `2 ×` that, which is
where the test's 650 comes from — it is not a tight bound, it is a **100 %
margin over the algorithm's own worst case.**

Three readings say the wheel is honouring that arithmetic:

* **Nothing ever fires early.** `under(<125) = 0` at every N on both VMs. An
  off-by-one in either direction would show here at N = 100 first.
* **At N ≤ 10 000 CratonVM's max is 257 ms — inside 325.** A bucket-selection
  error is a property of the arithmetic, not of the load: it would move the
  whole distribution at every N, including N = 100, where CratonVM's max is
  203 ms, one tick, exactly right.
* **The N = 100 000 `min` names the real mechanism.** 316 ms, for the FIRST
  task expired — the one scheduled at `r ≈ 0`, which a correct wheel fires at
  the tick-0 boundary, 200 ms. The extra 116 ms is
  `transferTimeoutsToBuckets` moving all 100 000 queued timeouts into buckets
  *before* the first one can run. HotSpot's same figure is 212 ms: same tick,
  12 ms of transfer.

## What it actually is: an overload cascade, not a uniform slowdown

The whole failure lives in one thread. The worker must, inside one tick:
transfer 100 000 pending timeouts into buckets, then expire a bucket's worth of
tasks. Measured from the table above:

| phase | CratonVM | HotSpot |
|---|---:|---:|
| transfer 100 000 timeouts | ~116 ms | ~12 ms |
| expire + run 100 000 tasks | ~553 ms | ~22 ms |
| **worker total** | **~669 ms** | **~34 ms** |

Once a bucket takes longer than 200 ms to drain, `waitForNextTick` returns
immediately on the next iteration and the lag carries forward — which is why
the distribution has a long tail (p50 597, max 869) rather than a fixed offset.
`schedMs` matters too, and in the direction that makes CratonVM's job *harder*:
HotSpot schedules all 100 000 in 9 ms so every task lands in bucket 0 and fires
in one batch; CratonVM takes 198 ms, spreading the tasks across two buckets that
then drain back-to-back behind the transfer.

**Not the clock.** `probes/SleepAccuracyProbe.java`, same binaries, same host:
`Thread.sleep` overshoot is 59–129 µs on CratonVM against 58–155 µs on HotSpot
across 1/2/5/10/25/50/100/200 ms — indistinguishable. `waitForNextTick`'s single
`Thread.sleep` per tick is not oversleeping, and `System.nanoTime` (74 ns
against HotSpot's 21) is 3.6× but only 300 000 calls, ~22 ms of the 669.

## Where the 669 ms goes

`--dump-native-registry` on the N = 100 000 arm: **2 320 908 native calls, 23.2
per expired task.**

| invocations | per task | native |
|---:|---:|---|
| 424 196 | 4.24 | `AbstractOwnableSynchronizer.setExclusiveOwnerThread` |
| 304 967 | 3.05 | `AtomicInteger.get` |
| 300 017 | 3.00 | `System.nanoTime` |
| 299 283 | 2.99 | `VarHandle.setRelease` |
| 126 489 | 1.26 | `Thread.interrupted` |
| 100 002 | 1.00 | `TimeUnit.toNanos` |
| 100 000 | 1.00 | `Long.longValue` |
| 100 000 | 1.00 | `TimeUnit.toMillis` |
| 100 000 | 1.00 | `AtomicLong.decrementAndGet` |
| 100 000 | 1.00 | `AtomicLong.incrementAndGet` |
| 99 803 | 1.00 | `VarHandle.getVolatile` |
| 99 497 | 1.00 | `VarHandle.compareAndSet` |
| 99 496 | 1.00 | `Long.valueOf` |

`perf record -F 999`, same workload, aggregated over all threads — the head is
the native funnel and the collector's receiver validation inside it, which is
the same shape `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md` recorded for
snappy:

```
 8.13%  try_jit_site_cached_native_dispatch
 4.98%  jit_invoke_virtual_mic
 4.94%  ZObjectStarts::contains
 4.83%  safe_native_call_impl
 4.37%  ZgcRealHeap::is_object_address
 3.18%  native_stack_has_jit_frame
 2.26%  forward_jit_reference_args
 2.03%  NativeMethodRegistry::find_with_kind
 1.92%  NativeContextImpl::record_jmx_owned_synchronizer
```

Funnel-and-boundary machinery is ~25 % of CPU; the collector's per-call receiver
validation another ~9 %.

Priced as rungs (`probes/TimerNativeRungRate.java`, best-of-40, HotSpot
interleaved as the control; the host was loaded, so read the ratios):

| rung | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `LinkedBlockingQueue` add+poll | 75 ns | 8 437 ns | **112×** |
| `ReentrantLock` lock+unlock | 11 ns | 1 549 ns | **141×** |
| `Long.valueOf` + `longValue` | 0.26 ns | 503 ns | 1 900× |
| `TimeUnit.toMillis` + `toNanos` | 1.7 ns | 390 ns | 235× |
| `AtomicInteger.get` | 0.5 ns | 132 ns | 265× |
| `Thread.interrupted` | 0.6 ns | 148 ns | 264× |
| `System.nanoTime` | 32 ns | 86 ns | 2.7× |

The model closes: one expired task is `nanoTime` + `TimeUnit.toMillis` +
`Long.valueOf` + `queue.add`, and `queue.add` alone is two uncontended
`ReentrantLock` pairs. 86 + 195 + 215 + ~4 200 ≈ 4.7 µs against the 5.5 µs the
scale probe measures per task. **`LinkedBlockingQueue.add` is 76 % of it, and
the AQS lock pairs inside it are 56 % of the whole.**

`probes/LockNativeCensusProbe.java` says what an uncontended pair costs and why:
1 000 000 lock/unlock pairs execute **exactly two** registered natives, both
`setExclusiveOwnerThread`, at 1 566 ns/pair. Everything else on the AQS path —
`Thread.currentThread`, the state CAS — is already served without a registry
call. That number, and its history from 10 502 ns, is
`retired/uncontended-reentrantlock-pair-mostly-unattributed-RETIRED-20260805.md`'s,
and its residual is `fixed-bugs/native-funnel-fixed-cost-is-the-remaining-wall-RETIRED-20260806.md`'s.

## The class is load-sensitive, which the predecessor page denied

The open page's strongest argument for "not flakiness" was that two isolated
reruns failed identically. Six more isolated runs, one class per process, taken
2026-08-19 on a host whose load was drifting between 9 and 21:

```
run 1   found=14 started=14 ok=14 failed=0     <- PASSES
run 2   found=14 started=14 ok=13 failed=1
run 3   found=14 started=14 ok=13 failed=1
run 4   found=14 started=14 ok=13 failed=1
run 5   found=14 started=14 ok=13 failed=1
run 6   found=14 started=14 ok=13 failed=1
```

One pass in six. That is not a contradiction of the page's claim — the class is
still overwhelmingly a failure and HotSpot never fails it — but it is a third
independent argument for hypothesis 2 and against hypothesis 1: **a wheel that
selected the wrong bucket could not pass on a quiet host.** A margin that the
host's load decides is exactly what "the worker needs 669 ms and the test
allows ~450" predicts.

It also means a single green run of this class must not be read as a fix.
Anything claiming to close it needs the scale probe's numbers, not a rerun.

## What would make the class pass

Arithmetic, so it can be checked rather than hoped for. The failing quantity is
`max delay ≈ 200 + worker_total`, and the bound is 650, so the worker must
finish transfer + expire in **under ~450 ms**. It takes ~669 ms on a quiet host.
That is **1.5×**, on a path that is ~60 % native-dispatch floor by the census
above.

One rung of it landed on this branch: `Long.valueOf`/`longValue` had no thin
`*_DIRECT_FN` bind while the `Integer` twins have had one since 2026-07 (see the
commit `perf(jit): Long.valueOf/longValue had no thin direct bind`). That is 2 of
the 23 natives per task and it does **not** close the gap — it is recorded here
because the census is what found it, not because it retires this class.

The rest is not this page's and not netty's: it is the per-call dispatch floor,
and the three campaigns that already own it took the AQS pair from 10 502 ns to
~1 229 ns before concluding that the remaining ~300 ns per native call is the
funnel itself. This class is now a **member of that list with a measured
arithmetic target**, which is more than it had as an untriaged "confirmed
CratonVM-specific" row.

## Confirmed CratonVM-specific (unchanged)

```bash
java @common.args -Dcraton.batch=1 CratonRunner io.netty.util.HashedWheelTimerTest
```

HotSpot 25, same host, isolated: `found=14 started=14 ok=14 failed=0`, twice.
CratonVM fails the same one method consistently, on both the default and ZGC
collectors.

## Repro

```bash
cd /data/cvm-<wt>/probes
javac -nowarn -cp "$NETTY_CP" -d out HwtScaleProbe.java SleepAccuracyProbe.java
java             -cp "out:$NETTY_CP" HwtScaleProbe 100,1000,10000,100000   # oracle
cratonvm --java-home "$JAVA_HOME" --Xmx 1500m -cp "out:$NETTY_CP" HwtScaleProbe 100,1000,10000,100000
cratonvm --java-home "$JAVA_HOME" --dump-native-registry=/tmp/hwt-reg.json -cp "out:$NETTY_CP" HwtScaleProbe 100000
```

The scale sweep is the load-bearing part. A single N = 100 000 run reproduces the
failure but cannot tell the two hypotheses apart — that takes the small-N rows,
where a correct wheel and a slow VM look different.

## Related

* `known-issues/netty/fail-hang-crash-rerun-20260817.md` — where this was first flagged.
* `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md` — the same funnel profile from a different netty class.
* `retired/uncontended-reentrantlock-pair-mostly-unattributed-RETIRED-20260805.md` — what an uncontended AQS pair costs and why.
* `fixed-bugs/native-funnel-fixed-cost-is-the-remaining-wall-RETIRED-20260806.md` — the residual this class's 1.5× belongs to.
