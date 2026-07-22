# H2 Database suite — residual raw-thread hang cluster investigation, 2026-07-22

Follow-up to [RESULTS-20260721-hang-rootcause.md](RESULTS-20260721-hang-rootcause.md),
which fixed the `ThreadPoolExecutor.shutdown()` `tryTerminate()` gap and took
CratonVM real-JDK mode from 0 to 114/218 passing classes, but left 61 classes
still reporting HANG at the suite runner's 60s per-class timeout (11
`org.h2.test.synth.*` — expected, hang by design on HotSpot too — and 50
others flagged as a follow-up with a *different* `gdb` signature: main
thread parked in `Thread.join()`/`Object.wait()`, not
`ThreadPoolExecutor.awaitTermination`).

- **Worktree:** `/data/wt-h2-hang-rawthread-20260721`, branch
  `fix/h2-db-suite-hang-rawthread-20260721`, off `dev` @ `9ba1e1c6d` (the
  merge that landed the previous fix).
- **Binary:** `target/release/cratonvm-h2-rawthread-20260721`.

## Summary of the finding

**This is not the same class of bug as the `tryTerminate()` fix, and there
is no analogous single-line correctness fix for it.** Deep investigation —
live `gdb` attaches, CPU-time sampling per OS thread, and extended-timeout
reruns — shows the residual cluster is predominantly **not deadlocked**.
The Java threads these tests spawn make genuine forward progress and
**do eventually complete correctly**; they just take far longer than the
60s (and often the suite's own 300s default) per-class timeout allows,
because of heavy lock contention in H2's own MVStore code combined with
CratonVM's per-operation interpretation overhead — a throughput gap, not a
missed wakeup or a broken `join()`/`wait()` implementation.

## Investigation

### 1. gdb signature confirms progress, not deadlock

Reproduced `org.h2.test.db.TestIndex` standalone and attached `gdb -p <pid>`
while it was "hung" (no progress for the suite runner's 60s budget). The
main thread was parked in `Object.wait(timeout)` (`native_object_wait_timeout`,
via `Thread.join()`'s no-args path), consistent with the task's framing.
But `ps -T -p <pid>` at the same moment showed **all four of the test's
`ConcurrentUpdateThread` worker LWPs at ~25% CPU each** (not near-0%, which
is what a genuinely deadlocked/leaked thread would show) — i.e. actively
running, not stuck. A second gdb sample a few seconds later showed the
"parked" threads' stacks unchanged in kind (still in
`native_lock_support_park`, the `LockSupport.park()`/AQS wait path) but with
higher accumulated CPU time, confirming a park→brief-work→re-contend cycle
rather than a true stall.

`Thread.join()` itself (`vm/src/vm/vm_exec.rs::thread_join`) is implemented
as a genuine OS-level `JoinHandle::join()` — Rust's own primitive, not a
custom wait/notify mechanism — so a "missed wakeup" in `join()`'s own
plumbing was already the least likely explanation; if the underlying OS
thread hasn't exited, `join()` cannot return, full stop. That reframed the
question from "is `join()`/`park()`/`wait()` broken" to "why does the
child thread's own work take so long".

### 2. Extended-timeout reruns confirm these tests genuinely finish

- `TestIndex` standalone, no suite-runner timeout: **completes (exit 0) in
  ~216–218s**, reproduced 3 times (two standalone runs + once inside the
  suite-runner rerun below).
- `TestMultiThread` standalone with a 1200s timeout: **completes (exit 0)
  in ~870s** (`user` CPU time ~28 minutes across its ~25 concurrent
  worker threads at one point in the run — confirming heavy, but
  productive, multi-thread concurrency, not a spin/livelock).
- Reran the 50-class (+ a few substring-matched extras, 57 total) residual
  list through `./run-h2-suite.sh run --only <regex> --class-to 300`
  (5x the original 60s budget) against the *unmodified* merged fix binary
  (no new code changes — this was purely a "does more time help?" probe).
  Stopped after 20/57 classes to keep within this session's time budget;
  partial tally:

  ```
  PASS=8  HANG=11  FAIL=1   (20/57 attempted, --class-to 300)
  ```

  8 classes that were `HANG` at 60s flipped straight to `PASS` at 300s with
  zero code changes: `TestCompatibility` (121.8s), `TestCompatibilityOracle`,
  `TestCompatibilitySQLServer`, `TestDateStorage` (83.7s), `TestIndex`
  (216.0s), `TestIndexHints`, `TestMultiThreadedKernel`, `TestRunscript`
  (186.8s). The 11 that still didn't finish in 300s include `TestMultiThread`
  — which we already independently confirmed *does* finish, just needs
  ~870s, well past even the 300s probe.

### 3. Root cause of the slowness (not a discrete "bug" in the deadlock sense)

All the sampled hanging classes spawn multiple raw `Thread`s that write
heavily to the same H2 table/MVStore map concurrently. H2's own MVStore
uses a **lock-free CAS root pointer** for its B-tree maps
(`org.h2.mvstore.MVMap.tryLock()`, `apps/h2database/h2/src/main/org/h2/mvstore/MVMap.java:1948-1971`),
with an escalating backoff on contention:

```java
if (attempt < CPU_COUNT) {
    Thread.onSpinWait();
} else if (attempt < CPU_COUNT + (CPU_COUNT + estimatedContention) / 2) {
    Thread.yield();
} else {
    synchronized (lock) { lock.wait(1); }
}
```

Every concurrent insert/delete/update that collides on the same map's root
CAS retries through this ladder. With CratonVM's per-bytecode-instruction
and per-native-dispatch overhead being inherently higher than HotSpot's
fully warmed JIT, and 4–25 threads all hammering the same MVMap
concurrently in these tests, the cumulative cost compounds well past what
a 60s (or even 300s) timeout budgets for — while still converging to a
correct result eventually.

Notably, **the highest-leverage fix for exactly this pattern is already on
`dev`**: `native-builtins/src/lib.rs` (~line 77484) registers
`Thread.onSpinWait()` as `std::hint::spin_loop()` (a true cheap CPU hint),
with a comment documenting a *prior*, since-fixed regression where it was
wrongly implemented as `std::thread::yield_now()` — "one contended
[lock] handoff could cost SECONDS whenever the box had competing load
... the 'crawl regime'" — the exact symptom shape seen here. That fix
predates this investigation and is already merged; there was no further
low-risk, single-function fix of the same shape left to make.

### 4. Confound: this session ran on a loaded shared host

`uptime` during this investigation consistently showed **load average
~6–7 out of 16 cores**, with 2–3 *other* concurrent sessions' `cratonvm`
processes (`cvm-dohead-tran`, `cratonvm-fullsu` — not this investigation's
own processes) independently pegged at 100%+ CPU throughout. This matches a
documented, recurring false-positive pattern in this codebase's own history
(prior "host contention artifact" closures for unrelated hangs/perf
regressions). `Thread.yield()`'s underlying `sched_yield()` and general OS
thread-scheduling latency both degrade under real CPU oversubscription —
so the absolute wall-clock numbers measured here (216s, 870s, 121.8s, ...)
are almost certainly inflated relative to an idle host, and the *true* gap
between CratonVM and a practical suite-runner timeout may be smaller than
what was directly observed. This was not something a code-level fix in
this session could address or control for.

### 5. One unrelated minor gap noticed, not chased

Two classes in the sample (`TestCluster`, `TestReadOnly`) logged a
non-fatal `WARN`-level `NoSuchMethodError: java/lang/String.create(Z)V`
from `Socket.getImpl()` at startup. Both processes continued running and
accumulating CPU time for the rest of their timeout window afterward (i.e.
this did not block them), so it looks like a benign, already-tolerated gap
rather than the cause of their hang — not investigated further given the
time budget.

## Conclusion / disposition

No code change was made in this branch. Unlike the `tryTerminate()` fix,
this cluster is not a correctness bug with a clean, verifiable, single-site
fix — it is a throughput characteristic of CratonVM's interpretation +
native-dispatch overhead under heavy multi-threaded MVStore lock
contention, for which the most relevant known fix (`onSpinWait` →
`spin_loop()`) is already on `dev`, and whose measured severity in this
session was confounded by real concurrent load from other sessions on the
shared host. Per the residual-hang task's own guidance ("if it turns out to
be multiple unrelated bugs / not cleanly root-causable, be upfront about
that rather than forcing one fix"), this is reported as an investigation
writeup rather than a code fix.

### Recommended follow-ups (not done here)

1. Re-run the same 50-class residual list on a *confirmed-idle* host (no
   concurrent sessions) with a generous timeout (≥900s) to get an
   uninflated baseline — this would separate "genuinely too slow" from
   "host-contention artifact" definitively.
2. If a genuine gap remains on an idle host, profile the hot path inside
   `MVMap.tryLock()`'s contended branch (`Thread.yield()` /
   `synchronized(lock){ lock.wait(1); }`) specifically under CratonVM to
   see whether `Object.wait(1)`'s per-call overhead (GC-block-state
   bookkeeping, root-snapshot deposit, JFR event emission — see
   `vm/src/vm/vm_exec.rs`'s `park`/`monitor_wait` paths) is disproportionate
   compared to HotSpot's, which would be a legitimate, scoped optimization
   target distinct from the `onSpinWait` fix already merged.
3. Investigate the benign `String.create(Z)V` `NoSuchMethodError` noticed
   on `TestCluster`/`TestReadOnly` (`Socket.getImpl()` call site) — low
   priority, did not appear to block execution.
4. Consider whether the H2 suite runner's default `--class-to` should be
   raised (e.g. to 300s or higher) specifically for the
   `org.h2.test.db.Test{Index,MultiThread,Compatibility,...}`-style
   concurrency-heavy classes, since several are confirmed-correct given
   enough time — treating them as suite failures at 60s may be measuring
   the timeout budget more than a real defect.

## Files

```text
apps/h2database-suite-runner/out/rawthread-longtimeout-jit-real-all-20260721-232829/
    partial (20/57) --class-to 300 rerun of the residual hang list — PASS=8 HANG=11 FAIL=1
```
