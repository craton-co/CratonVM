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

## Diagnosis update (2026-06-13) — NOT the Condition; it's a GC-quiescence / non-moving-sweep deadlock

`CRATONVM_REAL_AQS=1` does **NOT** clear the hang — so the synthetic
`ReentrantLock`/`Condition` await/signal is **not** the root cause (the doc's
original hypothesis is wrong). HotSpot runs the class 13/13 OK in ~6.5 s
(it has real timing tests). On CratonVM the class hangs past the 120 s watchdog,
and the watchdog's dump is **not** clean thread frames — it reports:

```
[quiesce] FIRST corruption: quiescence depth=5 enter_count=43210 leave_count=43205
<then a raw heap hex dump — the heap-corruption detector tripped>
```

So the failure is in the **GC-quiescence machinery** (`vm/src/jit/
conservative_roots.rs` + `gc::gc_quiescence`), not the lock: 5 threads have
entered JIT-frame quiescence (`enter()`) without a matching `leave()` — they are
parked in native `monitor_wait` (Condition.await) *inside a live JIT frame*, so
quiescence is wedged at depth 5, AND the heap-corruption check fires (a moving
collection appears to have run / scanned the parked threads' roots wrong despite
the quiescence gate that is supposed to force the address-stable non-moving
sweep). This is the same hard area as the precise-JIT-stack-maps / non-moving
young-sweep work (see memory: OSR main() corruptor, bug-D CIDR JIT/GC, selective
promotion) — a multi-threaded blocking workload that holds JIT-frame roots across
a native park, which the young sweep / quiescence gate doesn't handle.

NEXT: (a) confirm with `--nojit` — if the hang persists with NO JIT frames
(quiescence stays 0) the bug is purely in monitor/thread coordination; if it
clears, it's the JIT-frame-root-across-park interaction. (b) The env is easily
contaminated by leftover/cross-session `cratonvm.exe` processes (CPU starvation +
`taskkill /IM` cross-kills give false rc=1 hangs) — run in isolation with no
concurrent VMs and verify the process count is clean before trusting a hang.
Needs a dedicated GC/threading session, not a quick Condition fix.

## Affected classes (partial — append more later)
- producer.internals.BufferPoolTest (TIMEOUT)
- (other blocking-queue/lock tests likely: common.* network/selector, append from full run)
