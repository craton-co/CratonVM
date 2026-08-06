# `ThreadPoolExecutor.execute` drops every task past the core pool size, and `shutdown()` leaves idle workers parked

| | |
|---|---|
| **Status** | OPEN — mechanism pinned to two counters, fix not attempted |
| **Severity** | high — silent task loss with no exception, plus an unbounded hang from `close()` / `awaitTermination` |
| **Modes** | `--real-jdk` (measured). `--jdk-only` / `--synthetic-jdk` not yet measured |
| **Opened** | 2026-08-06, found while closing the `ConcurrentHashMap.newKeySet()` hang |
| **Not that bug** | measured identically on the binaries either side of that fix, and `ThreadPoolExecutor` holds no `newKeySet()` |
| **Already red** | `regression-suite/src/RExecutorShutdown.java` fails on `dev` today — see below |

## Two measurements

### 1. Tasks past the core pool size are never enqueued

Submit 8 trivial `Callable`s to `Executors.newFixedThreadPool(4)` and take each
future with a bound:

```java
ThreadPoolExecutor p = (ThreadPoolExecutor) Executors.newFixedThreadPool(4);
List<Future<Integer>> fs = new ArrayList<>();
for (int i = 0; i < 8; i++) { final int k = i; fs.add(p.submit(() -> k)); }
System.out.println("queue=" + p.getQueue().size() + " pool=" + p.getPoolSize()
    + " taskCount=" + p.getTaskCount());
for (int i = 0; i < 8; i++) fs.get(i).get(3, TimeUnit.SECONDS);
```

| | after 8 submits | futures resolved |
|---|---|---|
| HotSpot 25 | `taskCount=8` | 8 of 8 |
| CratonVM `--real-jdk` | `queue=0 pool=4 active=3 completed=0 **taskCount=3**` | **4 of 8**, then `TimeoutException` on future 4 |

`taskCount` is 3, then 4 — never 8 — and the queue is empty throughout. So the
four extra tasks are not "queued and never taken"; **`execute()` never accepted
them at all**. `getTaskCount()` is `completedTaskCount + active + queue.size()`,
so a task the executor never counted is a task it never enqueued.

`Executors.newCachedThreadPool()` given the same 8 tasks resolves **8 of 8**
(`pool=7 completed=8`). A cached pool creates a worker per task and hands the
task straight to it, never using the queue as a holding area. That is the split:
the worker-creation path works, the **queue-handoff path** — `execute`'s second
branch, `workQueue.offer(command)` once `workerCountOf(c) >= corePoolSize` —
does not.

### 2. `shutdown()` does not wake parked idle workers

`regression-suite/src/RExecutorShutdown.java` is red on `dev` today, on both the
pre-fix and post-fix binaries:

```
AssertionError: shutdown() left idle workers parked (awaitTermination timed out)
    at RExecutorShutdown.idleWorkersWake(RExecutorShutdown.java:62)
```

`idleWorkersWake` submits three tasks to a pool of three, sleeps until all three
have finished and parked in `getTask()`, then `shutdown()`s and waits 60 s. The
pool never reaches `TERMINATED`. An *empty* pool does:

```
fixed(4), 0 tasks : awaitTermination=true  isTerminated=true
fixed(4), 8 tasks : awaitTermination=false isTerminated=false
```

so the shutdown state transition itself works. It is `interruptIdleWorkers` —
`w.tryLock()` per worker, then `t.interrupt()` — that is not getting the parked
workers out of `getTask()`.

Consequence: `try (var es = Executors.newFixedThreadPool(4))` hangs on the
closing brace. `ExecutorService.close()` is `shutdown()` plus an UNBOUNDED
`awaitTermination(1L, DAYS)`, so a pool that cannot terminate is a thread that
never returns.

## Where to look

Both faces live in `ThreadPoolExecutor`'s `ctl` accounting, which CratonVM's
`execute` bridge partly replaces (see the already-fixed family in
`fixed-suite-bugs/threadpoolexecutor-*` — dispatch degrading to
synchronous, `mainLock` NPEs, `shutdown` self-recursion, prestart). None of
those touched enqueue-versus-reject or idle-worker wakeup.

* **Face 1.** `execute(Runnable)` is: if `workerCountOf(c) < corePoolSize` try
  `addWorker(command, true)`; else if running, `workQueue.offer(command)`; else
  `reject(command)`. Since nothing is rejected (no
  `RejectedExecutionException`), nothing is queued, and `taskCount` never grows
  past the number of workers, the suspicion is that the bridge takes only the
  first branch and silently returns when `addWorker` fails.
* **Face 2.** `interruptIdleWorkers` distinguishes idle from running with
  `w.tryLock()`. If the `Worker` AQS state is not being maintained, `tryLock()`
  fails for an idle worker and it is never interrupted — which is exactly the
  observed "left idle workers parked".

## Blast radius

Any code that submits more tasks than the pool has threads and then waits on
the results — the ordinary use of a bounded pool. The dropped tasks raise
nothing: the caller simply blocks on a future that will not resolve.
`try`-with-resources over any `ThreadPoolExecutor` factory hangs on the closing
brace once the pool has been given work.

`Executors.newVirtualThreadPerTaskExecutor()` and
`newThreadPerTaskExecutor(factory)` are NOT affected — they are
`ThreadPerTaskExecutor`, not `ThreadPoolExecutor`, and their own hang was a
different defect (the retired
`concurrenthashmap-newkeyset-returns-a-plain-hashset` write-up).

## Reproducers

`TpeStateProbe` (the counter dump above; the load-bearing line is
`taskCount=3`), and `TpeMatrixProbe`, which sweeps pool factory × drain
strategy and shows `fixed(4), 0 tasks` terminating while every non-empty shape
does not.
