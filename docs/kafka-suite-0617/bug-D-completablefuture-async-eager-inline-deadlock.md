# Bug D — `CompletableFuture.*Async` / `ForkJoinPool.execute` run inline on the caller → deadlock ⏱️ TIMEOUT cluster

| | |
|---|---|
| **Severity** | High — root cause of the largest remaining CratonVM-only TIMEOUT cluster |
| **Kind** | Hang / deadlock (TIMEOUT @ 600s) |
| **Surfaced by** | The heavy `consumer.internals.*` / `producer.internals.*` tests (FetcherTest, SenderTest, TransactionManagerTest, CommitRequestManagerTest, …), `KafkaAdminClientTest`, and the trivial `common.header.internals.RecordHeadersTest` |
| **CratonVM** | TIMEOUT · **HotSpot** OK |
| **Status** | OPEN — root-caused with a 1-line-pattern minimal repro |
| **Recommendation** | Fix is infrastructure-level (≈Bug B tier), not a quick skip-list entry. See "Fix direction". |

## Symptom

A cluster of ~30 CratonVM-only TIMEOUTs (HotSpot passes them in seconds). Distinct from
Bug C (the WeakHashMap/log4j2 JIT hang): these **also hang under `--nojit`**, so it is not a
JIT defect.

## Root cause

`--stack-dump-on-timeout` of the trivial `RecordHeadersTest` shows the **main thread** parked
inside the async task itself:

```
RecordHeadersTest.assertRecordHeaderReadThreadSafe
  → CompletableFuture.asyncRunStage
    → CompletableFuture$AsyncRun.run
      → RecordHeadersTest.lambda$assertRecordHeaderReadThreadSafe$1
        → CountDownLatch.await()      ← tid=0 (MAIN) parked here
```

The test uses the classic **start-gate** stress pattern: spawn N readers via
`CompletableFuture.runAsync(...)`, each of which calls `latch.await()`, then the main thread
calls `latch.countDown()` to release them all at once, then joins.

CratonVM implements `CompletableFuture.*Async` and `ForkJoinPool.execute(Runnable)` with an
**eager-inline policy** — the submitted `Runnable` is run *synchronously on the calling
thread*, not on a worker thread:

- `native-builtins/src/lib.rs` ~1868: `ForkJoinPool.execute(Ljava/lang/Runnable;)V` →
  `ctx.invoke_virtual(runnable, "run", "()V", &[])` (runs inline).
- Documented at lib.rs ~1828: *"Our ForkJoinPool worker threads do not run Java bytecode
  (NativeContext is not Send) … Eager-inline policy: invoke `runnable.run()` on the calling
  thread. This loses true parallelism but preserves the user-observable contract that
  `.get()` returns the final stage's value."*
- The override is forced over the real JDK bytecode by the dispatch gate in
  `vm/src/vm/vm_exec.rs` (search `WP4.2`).

So when the submitted task **blocks** — e.g. waits on a latch / future / queue that the
*submitting thread itself* is responsible for signalling later — running it inline means the
submitter blocks before it can do the signalling. **Deadlock.**

## Minimal repro

`ksuite/repro/MinGate.java` (and `MinAsync.java`, `MinGate2.java`, `Procs.java`):

```java
CountDownLatch latch = new CountDownLatch(1);          // start gate
List<CompletableFuture<Void>> fs = IntStream.range(0, N)
    .mapToObj(i -> CompletableFuture.runAsync(() -> {   // task blocks on the gate
        try { latch.await(); } catch (InterruptedException e) { throw new RuntimeException(e); }
    }))
    .collect(Collectors.toList());
latch.countDown();                                      // never reached on CratonVM
CompletableFuture.allOf(fs.toArray(new CompletableFuture[0])).get(20, SECONDS);
```

Results (clean, JIT on or off — same):

| repro | CratonVM | HotSpot | why |
|---|---|---|---|
| `MinAsync` — single **non-blocking** `runAsync` | OK | OK | inline run completes fine |
| `MinGate` — N **blocking** `runAsync` (commonPool), incl. **N=1** | **HANG** (hangs before `countDown` even prints) | OK | task runs inline on main → main blocks in `await()` → never `countDown`s |
| `MinGate2` — N blocking tasks on `Executors.newFixedThreadPool(N)` | **OK** (DONE) | OK | a real fixed pool uses real Java threads, so tasks run off-thread |

This isolates it precisely: the bug is **specific to the ForkJoinPool/`commonPool`/
`CompletableFuture.*Async` execution path**; explicit `Executors.*` thread pools (real Java
threads) work, and `new Thread(...).start()` works.

### Secondary observation — common-pool parallelism

`ForkJoinPool.commonPool().getParallelism()` reports **1** on CratonVM vs **7** (= procs−1)
on HotSpot, even though `Runtime.availableProcessors()` correctly returns 8 on both. The
Rust-side `common_pool()` (`vm/src/threading/forkjoin.rs:243`) clamps to `[1,4]`, and the
Java-visible value is overridden to 1 by the native gate (`vm_exec.rs`, `T19_K3_FJP_NATIVE_
OVERRIDE`). This is a contributing factor but **not** the core defect — the deadlock
reproduces even at N=1, because the tasks never reach a worker at all (they run inline).

## Fix direction

The eager-inline policy is the defect. The fix is to run `CompletableFuture.*Async` /
`ForkJoinPool.execute` tasks on **real Java threads** (which CratonVM supports — see
`Executors.newFixedThreadPool`, `Thread.start0`, `vm/src/vm/vm_exec.rs:3002 thread_start`),
not on the caller. Concretely: back the async/common-pool executor with a real Java-thread
pool, so a blocking task parks a *worker* thread and the submitter stays free to signal it.

Blast radius / why this is not a quick fix:

1. The same eager-inline path also serves **recursive `ForkJoinTask.fork/invoke`** compute
   workloads (FjpProbe) that *rely* on inline execution; the change must target only the
   `execute(Runnable)` / `*Async` executor path, not recursive compute.
2. `CompletableFuture` completion must work **cross-thread**: `completeValue` (the native CAS
   at lib.rs ~1846) and the `Signaller` park/unpark in `.get()` must be thread-safe. Regular
   Java threads already park/unpark correctly (MinGate2 passes), so this should hold, but it
   needs validation.
3. Unbounded thread-per-task would spawn ~100 threads for tests like RecordHeadersTest
   (HotSpot does grow the common pool similarly under managed blocking); a bounded real
   Java-thread pool with blocking-compensation is the robust target.

Regression witnesses: `MinGate` (must DONE), `MinAsync`/`MinGate2` (must stay OK), `FjpProbe`
(recursive compute must stay OK), and the Kafka `consumer.internals` TIMEOUT cluster.
