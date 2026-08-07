# FIXED — the synthetic blocking-queue natives were applied to real JDK queues, so `ThreadPoolExecutor` dropped every task past the core pool size

| | |
|---|---|
| **Status** | ✅ FIXED 2026-08-06 — the whole synthetic queue family is now dropped in real-JDK mode, not just `LinkedBlockingDeque` |
| **Severity** | was high **for measurement**, not for shipped behaviour — see "What this was NOT" |
| **Modes** | a binary built `--features synthetic-jdk` and RUN in `--real-jdk` / `--jdk-only`. The default `cratonvm-cli` build was never affected |
| **Opened** | 2026-08-06, while closing the `ConcurrentHashMap.newKeySet()` hang |
| **Fixed in** | `native-api/src/registry.rs`, the `drop_real_layout_synthetic` rule |
| **Regression test** | `regression-suite/src/RBlockingQueue.java`, in the suite's default `CORE_CLASSES` |

## What this was NOT

**The original filing was wrong about reachability, and that is the most
important correction here.** It recorded `Modes: --real-jdk (measured)` and
`Severity: high`, which reads as "the shipping VM drops tasks". It does not.

The natives responsible are registered inside
`#[cfg(feature = "synthetic-jdk")] register_blocking_queue_natives(...)`. The
default `cratonvm-cli` build does not enable that feature, so it never compiles
them. Measured on the same source tree, same JDK, same host:

| build | `LinkedBlockingQueue` | `ThreadPoolExecutor` probe | `regression-suite` |
|---|---|---|---|
| default (shipping) | matches HotSpot | **PASS**, 8 of 8 futures | **29 passed, 0 failed** |
| `--features synthetic-jdk`, run `--real-jdk` | `offer` true, `size()` 0, `poll` NPEs | FAIL, 4 of 8 futures | 22 passed, 7 failed |

This is the `synthetic MODE is not the synthetic FEATURE` trap: the Cargo
feature decides what is *compiled and registered*, the launcher flag decides
which *class library* is loaded. A feature-enabled binary run against the real
JDK registers synthetic natives on top of real classes, and that is the only
configuration this defect ever existed in.

It is still worth fixing, and not a little: that configuration is exactly what
the vm test gate and the regression suite are usually run with. Seven suite
classes were red for this reason alone, so the defect corrupted *measurement* —
including my own A/B baselines in the two sessions that preceded this one, which
reported "7 pre-existing dev failures" that dev does not actually have.

## The mechanism

`register_blocking_queue_natives` models every queue with the
native-collections four-slot layout — `array/head/size/capacity`. No real JDK
class in the family has that shape:

* `LinkedBlockingQueue` is `head/last/count/putLock/takeLock/notEmpty/notFull/capacity`
* `ArrayBlockingQueue` is `items/takeIndex/putIndex/count/lock/notEmpty/notFull`
* `ConcurrentLinkedQueue` / `Deque` are a CAS-linked node chain with no locks at all

The synthetic `<init>` never assigns the real fields, so the object is
half-native: the methods with natives use the side layout, and the first method
without one runs real bytecode against uninitialised state.

```
LinkedBlockingQueue q = new LinkedBlockingQueue<>();
q.offer("a"); q.offer("b"); q.offer("c");   // -> true, true, true
q.size()                                     // -> 0
q.peek()                                     // -> null
q.poll(1, TimeUnit.SECONDS)                  // -> NullPointerException:
                                             //    "takeLock" is null
```

`LinkedBlockingDeque` had already been exempted by an earlier fix; the other
four had not, and the exemption was written as a single `class_name ==` test
rather than a family.

### Why that reached `ThreadPoolExecutor`

`ThreadPoolExecutor`'s work queue **is** a `LinkedBlockingQueue`. Real
`execute()` is: try `addWorker(command, true)` while under `corePoolSize`, else
`workQueue.offer(command)`. The offer returned `true` into the side layout, so
nothing was rejected and no exception was raised — but no worker could ever take
the task, because `getTask()` → `workQueue.take()` is real bytecode reading the
real, never-initialised fields.

So exactly `corePoolSize` tasks ran and the rest vanished silently:

| | tasks submitted | futures resolved | `getTaskCount()` |
|---|---|---|---|
| `newFixedThreadPool(4)` | 8 | **4** | 4 |
| `newSingleThreadExecutor()` | 6 | **1** | — |
| `newCachedThreadPool()` | 8 | 8 | — |

The cached pool escaping is the tell: it has `corePoolSize == 0` and a
`SynchronousQueue`, so it creates a worker per task and hands the task over
directly, never using the queue as a holding area.

The queue also explains the second face in the original filing. A pool whose
tasks are stuck cannot reach `TERMINATED`, so `shutdown()` +
`awaitTermination` returned `false` and `regression-suite`'s
`RExecutorShutdown.idleWorkersWake` failed. That test is green again now, with
no change to the executor code at all.

## The fix

One rule in `native-api/src/registry.rs`: the existing
`drop_real_layout_synthetic` exemption for `LinkedBlockingDeque` becomes a
family test covering `LinkedBlockingQueue`, `ArrayBlockingQueue`,
`ConcurrentLinkedQueue`, `ConcurrentLinkedDeque` and the `BlockingQueue`
interface registrations. The real bodies are self-contained — `ReentrantLock` +
`Condition` for the blocking pair, CAS for the `ConcurrentLinked` pair — so
dropping the synthetic surface is the whole fix.

Synthetic-jdk **mode** keeps the natives: `drop_real_layout_synthetic` is only
ever set in a real-JDK arm.

## Verification

| | before | after |
|---|---|---|
| `QueueProbe` (6 queue types + cross-thread `take`), `--real-jdk` | 6 failures | PASS |
| `TpeProbe` (fixed/cached/single, `taskCount`, `awaitTermination`, try-with-resources), `--real-jdk` | 8 failures | PASS |
| same, `--jdk-only` | — | PASS |
| `SynQueueProbe`, `--synthetic-jdk` | works | **byte-identical output** — mode untouched |
| `regression-suite`, synthetic-jdk build | 22 passed, 7 failed | 23 passed, 6 failed (`RExecutorShutdown` green) |
| `regression-suite`, default build | 29 passed, 0 failed | unchanged |
| `RBlockingQueue` (new) | FAIL | PASS, A/B/B/A |
| 9 executor/concurrency vm test binaries | — | all green |
| `cratonvm-vm --lib` | 3948/10 | 3948/10, the identical 10 — all dev's, proven by reverting the one file |

## The residual this opened

Six regression-suite classes are still red in the synthetic-jdk build under
`--real-jdk` — `RStrings`, `RSerial`, `RCrypto`, `RChannelInterrupt`,
`RFileTimes`, `RNioNoFollow` — and all six pass in the default build. They are
the same shape as this bug (a synthetic native surface applied to a real JDK
object) in other subsystems, and each needs the same treatment: find the
registration, confirm the real bytecode is self-contained, add it to the
`drop_real_layout_synthetic` family. Filed as
`synthetic-jdk-feature-binary-diverges-under-real-jdk-FIXED-20260807.md`,
now retired to `docs/internal/` — all seven are closed.

Until those are closed, **an A/B run with a `--features synthetic-jdk` binary
must not treat those six as "pre-existing dev failures"** — they are artifacts
of the measuring instrument, not of `dev`.
