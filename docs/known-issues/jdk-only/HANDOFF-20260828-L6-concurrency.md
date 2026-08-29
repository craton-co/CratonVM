# L6 — concurrency and threads: 147 rows

**Read `HANDOFF-20260828-SCOPE.md` first.**

**Owner: unclaimed.** L5 (`claude/jdk-only-mode-handoff-09b48c`, worktree
`h2-known-issues-206dee`) is the only lane currently running.

## Your families

```text
java/util/concurrent/ConcurrentHashMap   48 bridge-with-code rows
java/lang/Thread                         37
java/util/concurrent/ForkJoinTask        36
java/util/concurrent/ForkJoinPool        26
                                        ---
                                        147   (7%)
```

Registrars: `native-collections/src/lib.rs` (shared with L3/L4) for CHM, and
`native-builtins/src/jdk25_concurrency.rs` plus `lang_system.rs` for the rest.

## READ THIS BEFORE YOU TOUCH `ConcurrentHashMap`

**`ConcurrentHashMap.elements()` never terminates.** Bisected in two builds to
**dev's `a0168ed03`** ("ConcurrentHashMap's values view gets its own cursor"),
merged as `a52eaefc8`. Reverting that commit alone clears it 3/3. Record:
`rjdkenumerations-is-red-on-dev-from-the-chm-values-cursor-20260827.md`.

* It is **not yours to fix without talking to that lane** — it carries a
  measured perf win.
* `RJdkEnumerations` will be RED in your strict and `SUITE=all` arms because of
  it. That is expected; do not bisect it again.
* The record's §2 was CORRECTED: the defect does **not** need
  `--jdk-only-report`. Three `put`s and an `elements()` drain reproduce it in
  both modes with no flags. `probes/Phase1Sweep.java`'s P1-F row is the smallest
  repro.

Any CHM probe you write must **hard-cap its drains** or it will hang instead of
reporting.

## Also yours

**`AsynchronousFileChannel.write` returns `CompletableFuture` where HotSpot
returns `sun.nio.ch.PendingFuture`** — every VALUE agrees, the type does not.
Recorded as OPEN in
`the-roadmaps-phase-1-and-3-re-adjudicated-and-six-fixes-20260827.md` §6. It
implies the async channel completes synchronously, which is worth confirming
before deciding whether the type matters.

## The hazard specific to this lane

Everything here is timing-sensitive, and this host **flips PASS/FAIL under
load** — that is recorded, not theoretical. So:

* **Never conclude from one run.** A failure that passes standalone is leakage
  or load until proven otherwise; a repeat run and a standalone run are both
  cheap and both required.
* Join all work before reading it, make every task a pure function of its input,
  and print no thread name, pool size or timing. `probes/LoaderModuleSweep.java`
  does this for ForkJoin and is a working template.
* `RBlockingQueue` is a **known flake** on this host (`HANDOFF-20260812.md`, "do
  not chase it") — one failure under suite load, passes standalone and on
  repeat. Search the known-issues tree for a vector's name before bisecting it.

## Edges that pay

* **`Thread`**: `setName(null)` NPE (already fixed — check before re-probing),
  `start()` twice (`IllegalThreadStateException`), `interrupt` and the
  interrupted-status clearing rules of `interrupted()` vs `isInterrupted()`,
  `join(0)` meaning "forever", `setPriority` out of range, `getState` through a
  lifecycle.
* **`ForkJoinTask`**: `isCompletedAbnormally` vs `isCancelled`, `getException`
  after a throwing task, `cancel(true)` before and after run, `join` on a
  cancelled task, `inForkJoinPool` from outside a pool.
* **`ForkJoinPool`**: `getParallelism` bounds, `commonPool` never null,
  `shutdown` vs `shutdownNow`, `awaitTermination` with zero and negative
  timeouts, `invoke` of a task that throws.
* **`ConcurrentHashMap`**: refuses null keys AND null values — unlike `HashMap`,
  which is right next door in the same file. That is the shared-surface trap
  this campaign has hit three times.
