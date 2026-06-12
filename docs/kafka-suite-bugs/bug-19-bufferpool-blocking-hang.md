# Bug 19 — `BufferPoolTest` hangs (blocking buffer-pool `Condition.await` never wakes)

**Severity:** High (true hang) — `producer.internals.BufferPoolTest` TIMEOUT (rc=124,
no output). Reproduces under `--nojit`. HotSpot runs it in well under a second.

## Symptom
The class produces no `RESULT` line and is killed by the external timeout. No
exception, no panic — a genuine **block**: a thread waits on a
`java.util.concurrent.locks.Condition` (the buffer pool's "more memory available"
condition) that is never signalled.

## Root cause (to pin down)
`BufferPool.allocate(...)` blocks on `Condition.await()` when the pool is exhausted
and is woken by `deallocate(...)` → `moreMemory.signal()`. The test
(`testBlockTimeout`, `testBufferExhaustion`, …) drives this across threads. On
CratonVM the waiter is never woken — points at a defect in
`ReentrantLock`/`Condition` `await`/`signal` (lost wakeup), or in cross-thread
scheduling of the producer/consumer threads. Related family: the documented
blocked-thread / AQS work (`CRATONVM_REAL_AQS`) and `ArrayBlockingQueue` deadlocks.

**To check:** run with `CRATONVM_REAL_AQS=1` (real `java.util.concurrent` AQS) and
without; if the hang clears under real AQS, the synthetic `ReentrantLock`/`Condition`
is the culprit. Capture a stack dump (let the default 120s watchdog fire, i.e. do
NOT set `CRATONVM_DISABLE_DEFAULT_WATCHDOG`) to confirm the waiting frame.

## Affected classes (partial — append more later)
- producer.internals.BufferPoolTest (TIMEOUT)
- (other blocking-queue/lock tests likely: common.* network/selector, append from full run)
