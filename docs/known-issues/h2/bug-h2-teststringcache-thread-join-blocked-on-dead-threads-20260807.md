# `TestStringCache` HANGs in `Thread.join()` on worker threads the registry itself reports as not alive

## Status
**OPEN, single-sample** — found 2026-08-07 in a full 218-class suite sweep on
a clean host (`origin/dev` merge @ `f9315411a`, load average ~14).
`org.h2.test.unit.TestStringCache` is in the 41-class non-passing set,
HANGs at the per-class timeout. Split out from
[`bug-h2-mvstore-insert-loop-perf-hang.md`](bug-h2-mvstore-insert-loop-perf-hang.md)
because the thread-registry state captured here does not look like the
"genuinely working, just slow" signature confirmed for every other class in
that sweep — it looks like a **missed wakeup**.

## Repro
```bash
cd apps/h2database/h2
env CRATONVM_DEFAULT_WATCHDOG_SEC=25 <cratonvm-bin> --java-home /home/victor/jdk25 \
  --Xmx 1g --nojit -c "<full test classpath>" org.h2.test.unit.TestStringCache
```
(Direct binary invocation — the suite runner wrapper did not reliably
deliver `CRATONVM_DEFAULT_WATCHDOG_SEC` to the child; see the note in the
perf-hang doc.)

## The dump

```
--- T19.H1 thread summary: 4 registered thread(s) ---
  tid=0 os_tid=... name="main"    alive=true  daemon=false blocked=false roots=13 top=java/lang/Thread.join@129 <- java/lang/Thread.join@2 <- org/h2/test/unit/TestStringCache.testMultiThreads@99
  tid=1 os_tid=... name="Thread-0" alive=false daemon=false blocked=true  roots=1  top=<no-frame-trace>
  tid=2 os_tid=... name="Thread-1" alive=false daemon=false blocked=true  roots=1  top=<no-frame-trace>
  tid=3 os_tid=... name="Thread-2" alive=false daemon=false blocked=true  roots=1  top=<no-frame-trace>
--- T19.H1 full frame chains: 1 alive thread(s) with a deposited snapshot ---
  tid=0 "main" (6 frames, oldest first):
    [0] org/h2/test/unit/TestStringCache.main@6
    [1] org/h2/test/TestBase.testFromMain@8
    [2] org/h2/test/unit/TestStringCache.test@22
    [3] org/h2/test/unit/TestStringCache.testMultiThreads@99
    [4] java/lang/Thread.join@2
    [5] java/lang/Thread.join@129
```

`testMultiThreads` (per H2's own source) spawns a small number of worker
threads and joins each in turn to make sure concurrent access to the string
cache doesn't corrupt it. `main` is parked in `Thread.join()` — expected,
that is what the test does while workers run.

What is **not** expected: all three worker threads (`Thread-0`,
`Thread-1`, `Thread-2`) are reported `alive=false` — i.e. CratonVM's own
thread registry believes their Java-level `run()` has already returned —
**and** `blocked=true` with no frame trace, simultaneously. If a thread has
genuinely terminated, `Thread.join()` on it should return immediately (the
termination notification is exactly what unblocks a joiner); if `main` is
still parked in `join()` while the target is marked not-alive, the
notification either never fired or `join()`'s wait predicate re-checked
something other than the liveness flag that flipped.

Two readings:

1. **A missed wakeup / lost notify in `Thread.join()`'s implementation**:
   the worker threads did finish, `alive` was correctly flipped to `false`,
   but whatever wakes a parked joiner (condvar notify, park/unpark, a
   registry-scanned flag) didn't reach `main`, so it is parked forever on a
   target that will never change state again. This is the more concerning
   reading — a genuine correctness bug in thread-termination signaling that
   would affect any `Thread.join()` racing a fast-terminating thread, not
   just this test.
2. **Stale/incorrect registry bookkeeping**: `alive=false` is wrong and the
   threads are in fact still running (`blocked=true` would then be the
   accurate half), in which case the real bug is elsewhere (whatever they're
   each blocked on) and the registry's `alive` flag is simply not trustworthy
   in this state — which would itself undermine every `alive=`/`blocked=`
   read in every other watchdog dump in this sweep and is worth ruling out
   first for that reason alone.

The `no-frame-trace` for all three workers means neither reading can be
settled from this dump alone — there is no captured Java frame to show
either "returned cleanly" or "stuck here."

## Why this doesn't belong in the perf-hang doc
Every other class confirmed in that doc shows a thread actively executing —
varying bytecode offsets across repeated samples, working through a large
loop. This dump shows the *opposite*: a `main` thread parked waiting for a
signal from threads the runtime itself says are done. That is a
synchronization-correctness question, not a throughput one.

## Next steps
* Re-run with the workers' own thread state instrumented (or a debug print
  in `testMultiThreads` before/after each worker's `run()` body) to
  establish independently whether the workers actually completed.
* Grep the `Thread.join()` / thread-termination-notify implementation
  (`vm/src/runtime/interpreter.rs` or wherever `Thread.join`/`Thread.exit`
  bookkeeping lives) for how a terminating thread wakes its joiners, and
  whether that path can race with (or be skipped by) whatever sets
  `alive=false`.
* Determine whether `alive=false` in the `T19.H1` thread summary is sourced
  from the same flag `Thread.isAlive()` reads, or a separate registry field
  that can desync from it — reading 1 vs 2 above hinges on this.
* Check whether this reproduces on a **repeat run** (this is a single
  sample); if it reproduces reliably, `TestStringCache.testMultiThreads` is
  a clean, short, deterministic-looking regression vehicle for whichever
  mechanism is at fault.

## Related
* [`bug-h2-mvstore-insert-loop-perf-hang.md`](bug-h2-mvstore-insert-loop-perf-hang.md)
  — where this investigation started; ruled out as the same cause.
