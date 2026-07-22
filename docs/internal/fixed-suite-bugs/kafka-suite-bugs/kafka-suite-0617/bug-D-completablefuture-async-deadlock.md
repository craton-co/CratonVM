# Bug D — `CompletableFuture.*Async` / `ForkJoinPool.execute` deadlock ✅ FIXED

| | |
|---|---|
| **Kind** | Hang / deadlock |
| **CratonVM** | was TIMEOUT (largest cluster) · **HotSpot** OK |
| **Status** | ✅ FIXED on dev — `51197975` (bounded real-thread pool) |

## Root cause

CratonVM ran `CompletableFuture.*Async` / `ForkJoinPool.execute(Runnable)` **eager-inline on
the calling thread** (its Rust ForkJoinPool workers can't run Java bytecode). Any task that
blocks waiting for a signal the *submitter* sends later — the canonical case is a start-gate
`CountDownLatch.await()` (submitter `countDown()`s only after submitting), and any `*Async`
stage whose completer is the submitter — deadlocks the submitter. Confirmed by a 1-line repro
(`MinGate`) that hangs even at N=1; `Executors.newFixedThreadPool` (real Java threads) works.
This was the largest `consumer.internals` TIMEOUT cluster (e.g. `RecordHeadersTest`).

## Fix

Route `ForkJoinPool.execute(Runnable)` to a **process-wide singleton bounded pool** —
`new ThreadPoolExecutor(0, 256, 10s, SynchronousQueue)` — via `pool.submit()` (a real TPE
`submit` runs real bytecode on a real worker; `execute` would hit the inline native override).
Cached-style: reuses idle workers (no thread-per-task explosion), retires them after 10 s idle
so the VM still exits cleanly without daemon threads. Singleton held in a
`Mutex<Option<ObjectRef>>` kept alive via `register_var_handle_root` (NOT `pin_native_root` —
that is a transient per-thread pin stack). Recursive `ForkJoinTask.fork/invoke/submit` compute
path stays eager-inline.

Rejected alternatives: inline (deadlock); platform-thread-per-task (477-thread explosion on
RecordHeadersTest); virtual-thread-per-task (carrier starvation at N=100).

Validated: `RecordHeadersTest` TIMEOUT(600s) → **OK 212/212**; MinGate 1/8/100 DONE; recursive
fork/join sum correct; MinAsync/MinGate2 OK. Also: the 2 sweep SIGSEGV crashers
(KafkaShareConsumerMetricsTest, RecordAccumulatorTest) no longer crash on latest dev (now hit
this same async hang path, since fixed/contained).
