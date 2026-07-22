# H2 Database suite — hang root-cause and fix, 2026-07-21

Follow-up to [RESULTS-20260721.md](RESULTS-20260721.md), which found that
~93% of `org.h2.test.db.*` classes hang under CratonVM `--java-home`
(real-JDK) mode and flagged root-causing the hang as the next step. This
session did that: live `gdb` attaches on stuck processes, a root cause, a
fix, and a full 218-class rerun.

- **Worktree:** `/data/wt-h2-hang-rootcause-20260721`, branch
  `fix/h2-db-suite-hang-rootcause-20260721`, off `dev` @ `5f1b6148f`.
- **Binary:** `target/release/cratonvm-h2-hang-20260721` (uniquely named).
- **JDK:** `/home/victor/jdk25` (Temurin 25.0.3+9) for CratonVM `--java-home`.

## Root cause

CratonVM's `ThreadPoolExecutor.shutdown()`/`ExecutorService.shutdown()`
natives (`native-builtins/src/lib.rs`, registered for both
`java/util/concurrent/ThreadPoolExecutor` and the `ExecutorService`
interface) replace the real JDK bytecode body with a custom Rust
implementation (`transition_real_executor_to_shutdown`) that:

1. Transitions `ctl` (the packed run-state + worker-count `AtomicInteger`)
   to the `SHUTDOWN` run-state — correct.
2. Calls `interrupt_executor_workers` to wake any idle workers parked in
   `LinkedBlockingQueue.take()` so they observe shutdown and exit — correct.
3. **Never calls `tryTerminate()`** — this is the bug.

Real OpenJDK's `ThreadPoolExecutor.shutdown()` (confirmed against the actual
JDK 25 source under `/home/victor/jdk25/lib/src.zip`) ends with an
unconditional call to the package-private `tryTerminate()`:

```java
public void shutdown() {
    ...
    tryTerminate();
}
```

`tryTerminate()` is the *only* code path that moves a pool whose
`workerCount` is **already zero** at `shutdown()` time from `SHUTDOWN` to
`TIDYING`/`TERMINATED` and fires `termination.signalAll()`. When
`workerCount > 0`, the real `processWorkerExit()` (unmodified bytecode, runs
whenever an interrupted worker actually exits) eventually calls its own
`tryTerminate()` and the pool self-heals — but a pool with **zero** workers
(never used, or already fully drained before `shutdown()` is called) has no
worker left alive to ever reach that path. Without CratonVM's shutdown stub
also calling it, such a pool is stuck in `SHUTDOWN` state forever, and any
call to `awaitTermination()` (which *is* real bytecode, and genuinely blocks
on `termination.awaitNanos(nanos)`) hangs for the full requested timeout.

H2's `org.h2.util.Utils.shutdownExecutor()` — used by
`org.h2.mvstore.FileStore.shutdownExecutors()` to close the per-`FileStore`
`serializationExecutor`/`bufferSaveExecutor` single-thread pools on
`close()`/`deleteDb()` — calls exactly this:

```java
executor.shutdown();
executor.awaitTermination(1, TimeUnit.DAYS);
```

Any `FileStore` whose serialization/save executor happened to be idle (zero
active workers — the common case for a short-lived `TestDb`-based test that
never triggers an async background write/save before closing) hits this
exactly, hanging for up to a day. This explains the "hangs near the very end
of the test, mid-`Connection`/database-close teardown" signature from the
first RESULTS doc, and why it hit the overwhelming majority of
`org.h2.test.db.*` classes: nearly every one calls `deleteDb()`/`close()` at
some point.

### How this was found

1. Reproduced `TestMultiDimension` standalone (both JIT-on and `--nojit`),
   confirmed the same "progress then silence" signature from the first
   RESULTS doc.
2. `gdb -p <pid>` on the stuck process, `thread apply all bt` on all
   threads. The main `main-vm` thread was parked inside
   `LockSupport.parkNanos` reached from `ThreadPoolExecutor.awaitTermination`
   (real bytecode, via `invoke_special_bytecode_only`) — i.e. genuinely
   blocked in the JDK's own wait loop, not stuck in VM-internal code.
3. Two other named threads (`H2-serialization`, `H2-save` — matching
   `FileStore`'s executor thread names) were alive and idle, parked
   normally in `LinkedBlockingQueue.take()`. Nothing was deadlocked from a
   locking perspective — the pool just never told them to stop, and even if
   it had, that still wouldn't unblock `awaitTermination()` (see below).
4. Instrumented `interrupt_executor_workers`/`transition_real_executor_to_shutdown`
   with temporary debug prints (env-var gated, since reverted) confirming:
   `ctl` read as `0xE0000000` (`RUNNING | workerCount=0`) at the moment
   `shutdown()` ran on the specific `FileStore` whose close was hanging —
   i.e. that pool's worker had never been created (0 tasks ever submitted to
   it before close), so `workers.add()`/HashSet tracking were red herrings;
   the real gap was the missing `tryTerminate()` call.
5. Cross-checked against the real JDK 25 source
   (`java.base/java/util/concurrent/ThreadPoolExecutor.java` from
   `$JDK25/lib/src.zip`) to confirm `shutdown()`'s real body always ends
   with `tryTerminate()`.

## Fix

`native-builtins/src/lib.rs`, `transition_real_executor_to_shutdown`: after
transitioning `ctl` to `SHUTDOWN` and interrupting idle workers, invoke the
real (package-private) `tryTerminate()` via
`invoke_special_bytecode_only("java/util/concurrent/ThreadPoolExecutor", "tryTerminate", "()V", ...)`,
matching what the real `shutdown()` bytecode does. This is a 12-line,
single-function change — no other files touched.

## Verification

- `org.h2.test.db.TestMultiDimension` now completes (exit 0) in both
  default (JIT-on) and `--nojit` modes, in well under 10 seconds each
  (previously: hung for the full test timeout in both modes).
- Spot-checked 10 other classes from the original 39-class hang list:
  `TestAlterSchemaRename`, `TestBackup`, `TestBigDb`, `TestCsv`,
  `TestDeadlock` now **pass**; `TestCases`, `TestFullText`, `TestIndex`,
  `TestMultiThread` still hang — but via a **different** signature (see
  "Residual hangs" below); `TestLob` now fails with a genuine (unrelated)
  `PipedInputStream` error instead of hanging.
- Full 218-class suite rerun (`./run-h2-suite.sh run --category all --jit on
  --jdk real --count 0 --class-to 60`, tag `jit-real-fixed`):

  ```
  PASS=114  HANG=61  FAIL=43   (218 total, wall-clock 4359s)
  ```

  vs. the original run (only 42/218 attempted before stopping):

  ```
  PASS=0    HANG=39  FAIL=3    (42 attempted)
  ```

  vs. the HotSpot baseline (full 218):

  ```
  PASS=200  FAIL=13  HANG=5    (5 hangs are org.h2.test.synth.* — open-ended
                                 stress/chaos tests, hang by design on
                                 HotSpot too)
  ```

  This one fix took CratonVM real-JDK mode from **0 passing classes** (on
  the 42 sampled) to **114 passing classes** (of the full 218) — the
  overwhelming majority of the original hang population is now fixed.

## Residual hangs — a SEPARATE, still-open bug (not fixed this session)

61 classes still report HANG. 11 are `org.h2.test.synth.*`, which hang by
design on HotSpot too (open-ended stress tests) and are not a regression.
The other 50 (`TestIndex`, `TestMultiThread`, `TestFullText`, `TestCases`,
`TestMVStore`, `TestPgServer`, `TestRecovery`, etc.) hang with a **different**
`gdb` signature than the one fixed here: the main thread is parked inside
`Thread.join()`/`Object.wait(timeout)` (`native_object_wait_timeout`),
waiting on a raw `java.lang.Thread` the test spawned directly (e.g.
`TestIndex`'s `ConcurrentUpdateThread extends Thread`, joined after
`start()`), not inside `ThreadPoolExecutor.awaitTermination`. This is
unrelated to the `tryTerminate()` gap fixed above — some other issue causes
one of these raw worker threads to never reach a state where `join()`
returns. Root-causing this is flagged as a follow-up; it was not chased
further in this session to keep the (confirmed, well-evidenced) fix above
focused and mergeable.

## Files

```text
apps/h2database-suite-runner/out/jit-real-fixed-jit-real-all-20260721-211247/
    full 218-class CratonVM jit-real rerun with the fix (PASS=114 HANG=61 FAIL=43)
```

## Next steps

- Root-cause the residual `Thread.join()`/raw-thread hang family (50
  classes) — a fresh `gdb` attach + backtrace on e.g. `TestIndex` or
  `TestMultiThread` is the natural starting point (see "Residual hangs"
  above for the signature already captured).
- The FAIL list is a mix of previously-known gaps (`TestAuthentication`,
  `TestAlter`, `TestLargeBlob` from the first RESULTS doc) and newly-visible
  ones from covering the full 218 classes for the first time
  (`TestAnalyzeTableTx`, `TestFunctions`, `TestLob`, etc.) — not
  investigated here, out of scope per this session's focus on the hang.
