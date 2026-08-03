# AQS is 13-26x HotSpot on handoffs — because it is call-dense, not because of the spin

| | |
|---|---|
| **Status** | OPEN — root cause identified; there is no AQS-specific defect |
| **Severity** | high — AQS is a VM-wide primitive, but the fix is not in AQS |
| **Discovered** | 2026-08-03, root-causing `TestAsyncMessagesPerformance` SEQ2 |
| **Owns** | the residue of the retired [`websocket-async-send-interframe-latency`](../../internal/fixed-suite-bugs/tomcat/websocket-async-send-interframe-latency-CLOSED-20260803.md) doc |
| **Real owner of the fix** | the per-call dispatch floor — see *Where this actually belongs* |

> **This document's first revision was wrong**, and is corrected below. It
> blamed `AbstractQueuedSynchronizer.acquire`'s pre-park spin (up to 255
> `Thread.onSpinWait()` rounds). That was inference from the handoff numbers,
> never measured. Measuring the *uncontended* path — no contention, no spin, no
> parking whatsoever — accounts for essentially the whole gap on its own.

## The measurement that settles it

`probes/AqsBreakdownProbe.java`. Single-threaded, uncontended, quiet host.
**Nothing here spins or parks.**

| | HotSpot | CratonVM | ratio |
|---|---|---|---|
| empty instance call (the scale) | 0.6 ns | 431 ns | 719x |
| **`ReentrantLock` lock+unlock, uncontended** | **14.8 ns** | **10,502 ns** | **710x** |
| `ReentrantLock` tryLock+unlock | 13.9 ns | 7,958 ns | 573x |
| `ReentrantLock` FAIR lock+unlock | 14.0 ns | 9,480 ns | 677x |
| `Semaphore` acquire+release (permits free) | 16.4 ns | 6,612 ns | 403x |
| `Condition.signal` (no waiter) | 13.7 ns | 8,922 ns | 651x |
| `CountDownLatch.await` (already zero) | 0.5 ns | 276 ns | 553x |
| `synchronized` block, uncontended | 16.0 ns | **754 ns** | **47x** |
| `AtomicInteger.compareAndSet` | 4.5 ns | 616 ns | 137x |
| `AtomicInteger.get` | 0.3 ns | 969 ns | 3230x |
| `Thread.onSpinWait` | 36.9 ns | 130 ns | 3.5x |

An uncontended `ReentrantLock.lock()` + `unlock()` costs **10.5 us**. The
handoff figures this doc opened with (`Condition.signal -> await` 96.9 us,
`ThreadPoolExecutor.execute -> task` 167 us — `probes/HandoffLayersProbe.java`,
`probes/ExecDispatchProbe.java`) are roughly ten and sixteen of those. The spin
was never the story.

## Why: AQS is call-dense, and calls cost ~500-850 ns

The uncontended `lock()`/`unlock()` pair is **16 nested calls** (JDK 25 source):

```
lock()   -> Sync.lock() -> NonfairSync.initialTryLock()
                             -> Thread.currentThread()
                             -> compareAndSetState(0,1) -> U.compareAndSetInt()
                             -> setExclusiveOwnerThread(current)
unlock() -> Sync.release(1) -> Sync.tryRelease(1)
                                 -> getState()
                                 -> getExclusiveOwnerThread()
                                 -> Thread.currentThread()
                                 -> setExclusiveOwnerThread(null)
                                 -> setState(c)
                             -> signalNext(head)
```

Sixteen calls at the ~490-850 ns this VM charges per call is 8-13.5 us — the
entire measurement, with nothing left over to explain. HotSpot inlines all
sixteen into ~15 ns. `synchronized` is only 47x rather than 710x for exactly
this reason: a monitor is one bytecode the VM implements directly, not sixteen
Java calls.

**The methods are not the problem, and they do compile.**
`CRATONVM_DBG=jit-compiled` shows `ReentrantLock.lock`, `ReentrantLock.unlock`,
`ReentrantLock$Sync.lock`, `NonfairSync.initialTryLock` and `AQS.release` all
compiled — and the number is still 10 us. Compilation is not the lever; the
cost is *between* the compiled methods.

## Levers ruled out — all measured, none moved it

| lever | result |
|---|---|
| OSR starving callees of the hotness signal | **No.** `CRATONVM_JIT=tier-osr-backedge=2000000000` (OSR off) leaves the numbers unchanged, and the tracked-invocation count stays at ~500 either way. |
| the `java/util/` virtual tier-up exclusion | **No.** `execute_invokevirtual_cached` does suppress tier-up when the receiver class `starts_with("java/util/")`, which does swallow all of `java.util.concurrent` — but a user subclass of `ReentrantLock` (receiver outside `java/util`, identical inherited bodies, `probes/JavaUtilTierUpExclusionProbe.java`) is **2.6x slower**, not faster. |
| JIT admission / "hot method never compiles" | **No.** The lock methods compile — see above. |
| `CRATONVM_JIT=direct-callee-calls` | inert (3 interleaved rounds, within noise) |
| `CRATONVM_JIT=ir-direct-call` | inert |
| `CRATONVM_JIT=guarded-virtual-inline` | inert |

## Where this actually belongs

The per-call dispatch floor. An empty *instance* call is 431 ns here against
HotSpot's 0.6 ns inlined, and `invokevirtual` from compiled code takes the
generic dispatch helper (992 ns monomorphic / 6027 ns polymorphic, measured
elsewhere). Until that closes, no amount of AQS-specific work can help: the
JDK's concurrency classes are written as many small methods precisely because
every other JVM inlines them away.

In descending order of expected value:

1. **Inlining.** Nothing in the ruled-out list actually splices a callee body
   into its caller. `docs/feature-designs/profile-guided-inlining.md` §8 still
   has bimorphic splicing and a deopt-capable guard open. Sixteen calls going
   to zero is the whole gap.
2. **`AtomicInteger` is fully natively overridden.** The schema-2 census lists
   `get`, `set`, `compareAndSet`, `incrementAndGet` and 13 more as `bridge`
   natives, so `AtomicInteger.get()` costs **969 ns** — *more than an empty
   bytecode call* — where the real body is `return value;` on a volatile int,
   and being a native it can never be compiled or inlined. Dropping those in
   real-JDK mode is a contained experiment with a clear hypothesis; it needs
   `Unsafe.compareAndSetInt` to be sound first.
3. The `java/util/` tier-up exclusion is miscalibrated even though it is not
   the bottleneck here: its comment ties it to a Spring *collections* graph,
   and `java.util.concurrent.*` is caught by the prefix as collateral. Worth
   narrowing on its own merits, with its own measurement.

## What this blocks

- `TestAsyncMessagesPerformance.testAsyncTiming` — SEQ2's 500 us budget spans
  two AQS handoffs; 476-491 breaches of 500 against HotSpot's 0-2.
- Any latency budget denominated in thread handoffs. The `SmokeTests`
  concurrency ceiling and H2's single-threaded `INSERT`+`commit` gap are
  plausible relatives, not yet confirmed against this measurement.

## Reproduction

```powershell
$jdk = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& "$jdk\bin\javac.exe" -d out probes\AqsBreakdownProbe.java probes\HandoffLayersProbe.java
& "$jdk\bin\java.exe" -cp out AqsBreakdownProbe            # HotSpot control
& <cratonvm.exe> --java-home $jdk -cp out AqsBreakdownProbe
```

Interleave the arms and run both orders on a quiet host. Under background load
every number roughly triples and the ratios shift.

## Two traps this investigation walked into

Both are recorded in the probes' own comments, because both produced confident
wrong numbers first.

1. **The harness cost more than the thing measured.** The first cut drove each
   operation through a `(int) -> void` lambda so the bench loop could be a
   one-liner. A lambda's `invokeinterface` costs ~2.2 us here, so every row came
   back at 2-3 us and `AtomicInteger.get` "measured" 3021 ns. Every benchmark is
   now an inline loop in its own method.
2. **A shared call site goes polymorphic.** Testing base-vs-subclass through one
   `lockUnlock(ReentrantLock, int)` made its `lock.lock()` site bimorphic, and a
   poly site costs ~6x a monomorphic one — the entire reason the subclass first
   appeared 2.5x slower. Each receiver class now has its own loop method.

Same lesson twice: on a VM whose call floor is ~500 ns, any abstraction inside a
microbenchmark is part of the measurement.
