# `VarHandle` writes and CAS have no fast path — 35-303x, and it is most of `java.util.concurrent`

## Status
**OPEN (2026-08-24). Measured, localised, not fixed.** The defect is a gap in
an existing optimisation rather than a bug: `VARHANDLE_READ_DIRECT_FNS` binds
`VarHandle` READS of PRIMITIVE fields to a direct helper, and nothing else.
Every write, every CAS, and every reference-typed access falls through to the
generic native dispatch funnel.

## Severity
**HIGH, and broad.** `VarHandle` is the primitive under `CompletableFuture`,
`AbstractQueuedSynchronizer`, `ConcurrentHashMap`, `ForkJoinPool`,
`ConcurrentLinkedQueue` and `StampedLock`. Anything built on those pays it.

## The measurement

`HibfixVarHandleProbe`, single-threaded, each operation timed on its own after
a shared warmup:

| operation | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `VarHandle.compareAndSet`, reference field | 9.2 ns | **488.9 ns** | **53x** |
| `VarHandle.set`, reference field | 1.0 ns | **303.4 ns** | **303x** |
| `VarHandle.compareAndSet`, `int` field | 8.6 ns | **300.2 ns** | **35x** |
| `AtomicReference.compareAndSet` | 10.9 ns | **911.2 ns** | **84x** |
| `AtomicInteger.incrementAndGet` | 5.0 ns | 6.2 ns | **1.2x** |
| plain field store (baseline) | 1.1 ns | 10.3 ns | 9x |

**`AtomicInteger` is the control that makes this conclusive.** It has its own
intrinsic and is at parity, so this is not "atomics are slow" or "the box is
slow" — it is `VarHandle` specifically. A `VarHandle.set` of a reference costing
303 ns against a 10 ns plain store is a volatile store paying 30x an ordinary
one.

## Why: the fast path is reads-only, by construction

`jit/src/lib.rs` binds `VARHANDLE_READ_DIRECT_FNS` for

* modes `VARHANDLE_READ_MODES = ["get", "getVolatile", "getOpaque", "getAcquire"]`
* returns `VARHANDLE_READ_RETURNS = [Z B C S I J F D]`

and its own doc says of the reference kinds: "`L` and `[` are absent on
purpose". So the table has 32 slots, all of them reads of primitives. It was
built for netty's `RefCnt.isLiveNonVolatile`, which is `(int) VH.get(instance)`
— a read of an `int` — and it does that job.

Writes and CAS were never in scope. They still go through
`jit_invoke_dispatch`, paying the SATB flush, the reference-argument
forwarding, the site-key revalidation and two thread-local map probes that the
read path's own doc describes as the per-call floor it was created to remove.

## What it costs downstream

`HibfixComposeProbe2` composes 4 800 000 `CompletableFuture` chains with no
scheduler, no locks and no cross-thread handoff — each thread owns its futures
end to end:

| | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| 4.8 M compose chains | **412 ms** | **359 197 ms** | **~872x** |

`wrong=0` throughout: this is purely cost, not a correctness defect. A stack
profile of that run puts **34.0% in `CompletableFuture.tryPushStack`**, which is

```java
Completion h = stack;
NEXT.set(c, h);                        // VarHandle set, reference field
return STACK.compareAndSet(this, h, c);   // VarHandle CAS, reference field
```

with `UniCompose.tryFire` at 21.4%, `completeRelay` at 12.4% and
`uniComposeStage` at 8.0% behind it. Each thread owns its futures, so that CAS
is **uncontended and succeeds on the first attempt** — a third of the time in
an uncontended CAS is the primitive, not the algorithm. `ForkJoinTask.casStatus`
and `compareAndSetForkJoinTaskTag` appear lower down for the same reason.

## The fix, in the shape the existing code already has

Extend the direct-bind table the way it was extended for reads:

1. add the write and read-modify-write modes — `set`, `setVolatile`,
   `setRelease`, `setOpaque`, `compareAndSet`, `compareAndExchange`,
   `weakCompareAndSet`, `getAndSet`, `getAndAdd`;
2. add the reference kinds `L` and `[`, which need the store barrier the
   primitive path does not — that is why they were left out, and it is the
   real work here;
3. keep the same site-keyed bind so the funnel is skipped, not merely
   shortened.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
java -cp . HibfixVarHandleProbe                      # HotSpot control
cratonvm --java-home <jdk> -Dprobe.iters=2000000 -cp . HibfixVarHandleProbe
```

`HibfixComposeProbe2` is the downstream reproducer — deterministic, no
database, no flake, 412 ms against 359 s.

## Related

- [`../hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`](../hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md)
  §5.9 — where this was found. The reactive composition cost there is this
  defect, and it is the likely enabling condition for that page's correctness
  failure: a sequence-allocation race HotSpot settles in microseconds is run
  through machinery two orders of magnitude slower.
- [`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
  — the same shape of finding: a hot JDK primitive left on the generic funnel.
