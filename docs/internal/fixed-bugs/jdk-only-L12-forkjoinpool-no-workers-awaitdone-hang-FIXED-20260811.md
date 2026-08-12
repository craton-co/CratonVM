> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkForkJoin` passes in the 53/1 run. Both "Out-of-file patches (must be applied for this fix to do anything)" are applied: the three static `invokeAll` overloads are in `keep_real_forkjointask_bridge` (`native-api/src/registry.rs:6072`) and in `is_forkjoin_native_override` (`vm/src/runtime/interpreter/native_override.rs:1717`), each carrying this record's `// L12:` comment. "What this does NOT fix" — the `CountedCompleter leaves: 0` failure it predicted — was taken by L19 and W2-8 and is closed; the `fork()` ordering change it declined to make is now the shipped default (`CRATONVM_FJP_EAGER_FORK`, flipped 2026-08-07).
>
> Previous location: `docs/known-issues/jdk-only/L12-forkjoinpool-no-workers-awaitdone-hang.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `ForkJoinTask.invokeAll` was the one real-bytecode route from a lazy `fork()` to an `awaitDone()` nothing can satisfy

**Status:** FIXED in source 2026-08-06 (lane L12, JDK-only wave 2). Not yet
verified against a binary — see *How to verify* below. **The fix is incomplete
for the whole test**: it removes the hang, and `RJdkForkJoin` is then expected
to fail *loudly and fast* on the `CountedCompleter` section. See *What this
does NOT fix*.

## The failure

`regression-suite/src/RJdkForkJoin.java` hangs in **both** `--real-jdk` and
`--jdk-only`: exit code 124 on a 300-second budget with **zero** checkpoint
output. HotSpot 25 runs the same class in a few seconds to
`PASS RJdkForkJoin (26 checks)`. Identical behaviour in both arms is the tell
that this is an ordinary Compatible-mode defect, not a strict-mode policy drop.

Run under the VM's own watchdog (`CRATONVM_DEFAULT_WATCHDOG_SEC=45`) the dump
is decisive:

```
1 thread(s) dumped
tid=0 name="main" frames=34
  depth 0   RJdkForkJoin.main
  depth 1   RJdkForkJoin.recursiveTasks pc=186
  depth 2   RJdkForkJoin$FillAction.compute pc=90
  depth 3   ForkJoinTask.invokeAll(FJT,FJT)V pc=25
  depth 4   ForkJoinTask.doExec pc=13
  depth 5   RecursiveAction.exec pc=4
  depth 6   RJdkForkJoin$FillAction.compute pc=90
  ...  the 4-frame cycle repeats ~7 more times ...
  depth 30  RJdkForkJoin$FillAction.compute pc=90
  depth 31  ForkJoinTask.invokeAll(FJT,FJT)V pc=83     <-- pc=83, not pc=25
  depth 32  ForkJoinTask.awaitDone(ZJ)I pc=141
  depth 33  ForkJoinTask.awaitDone(FJP,IZJ)I pc=218    <-- BLOCKED HERE
```

`recursiveTasks` reaching bci 186 pins the hang to line 118,
`pool.invoke(new FillAction(filled, 0, 5000))` — the third operation in the
method. That is why there is no output: the first `System.out.println` in
`recursiveTasks` is at line 129, after the fill. Lines 109-115 (`pool.invoke`
of a `SumTask`, then `pool.submit` + `get`) had already *succeeded*, which is
itself load-bearing evidence (see *Ruling out the alternatives*).

`FillAction.compute` (RJdkForkJoin.java:87-96) is the classic shape:

```java
protected void compute() {
    if (hi - lo <= 32) { for (int i = lo; i < hi; i++) { a[i] = i * 2; } return; }
    int mid = (lo + hi) >>> 1;
    invokeAll(new FillAction(a, lo, mid), new FillAction(a, mid, hi));
}
```

## Root cause

JDK 25's `ForkJoinTask.invokeAll(t1, t2)` is, in essence:

```java
t2.fork();                    // hand t2 to a worker
t1.doExec();                  // run t1 INLINE on the calling thread
t2.awaitDone(...);            // block until somebody else finished t2
```

CratonVM's `ForkJoinTask.fork()` is a **deliberately lazy** Bridge native
(`native-builtins/src/phases_early.rs`, `register_real_jdk_forkjoin_essentials`
and `register_forkjoin_natives`): it only marks the task *queued* in the
`fjp_state` side table and returns. What actually drives `compute()` is
`join()` / `get()` / `invoke()`, each of which is a side-table-backed native
that runs the body inline and memoises the result.

`invokeAll` calls **none of those**. It goes straight from `fork()` to
`awaitDone()`, which is real JDK bytecode that parks until another thread
completes the task. This pool runs every task on the calling thread, so no
other thread will ever exist to complete it. The wait cannot end.

The stack is the mechanism written out: every `invokeAll` frame at `pc=25` is
the inline `t1.doExec()` arm (the recursion the main thread walked all the way
down), and the single frame at `pc=83` is the `t2.awaitDone(...)` arm at the
bottom of that recursion.

Every *other* entry point into the inline model was already covered —
`ForkJoinPool.invoke` / `submit` / `execute` / `invokeAll(Collection)` /
`invokeAny` / `lazySubmit`, and `ForkJoinTask.join` / `get` / `invoke` /
`isDone` / `cancel` / `complete` / `getException`. The three **static**
`ForkJoinTask.invokeAll` overloads were simply never registered, and they were
the last real-bytecode route from a lazy `fork()` to an `awaitDone()`.

## Ruling out the alternatives

The brief offered four candidates. This is **(c)**: `fork()` is a stub that
never enqueues, so `awaitDone` waits on a task that was never scheduled.

* **(a) `ForkJoinPool` never starts worker threads.** True as a *statement of
  fact*, but it is the design, not the defect. `ForkJoinPool.<init>` is not
  intercepted (`native-api/src/registry.rs` drops all `ForkJoinPool` natives
  except a small Bridge allow-list, and `<init>` is not on it), so the real
  constructor runs; but `pool.invoke(task)` is an intercepted Bridge that runs
  the body on the caller and never submits anything, so the real pool is never
  asked to create a worker. Making it create workers is *not* the fix: it would
  contradict the whole eager-inline model, which exists because CratonVM's
  `NativeContext` is not `Send` and worker threads cannot run Java bytecode.
* **(b) `Thread.start` for `ForkJoinWorkerThread` does not create a live VM
  thread.** Never exercised. Nothing ever calls `registerWorker` /
  `createWorker`, because nothing ever submits to the real pool (see (a)).
* **(d) Workers exist but are invisible to the watchdog and blocked
  themselves.** Ruled out by `1 thread(s) dumped` *plus* the fact that the
  single thread's own stack fully explains the hang without reference to any
  other thread: it is parked in `awaitDone` for a sibling that provably was
  never scheduled anywhere.

The positive evidence for (c) rather than "the whole task family is broken" is
that **`SumTask` worked**. `SumTask.compute` (line 65-69) does
`left.fork(); right.compute(); left.join()` — and the stack proves execution
got past it to line 118. Same lazy `fork()`, but the consumer is `join()`,
which is intercepted and drives `compute()`. The difference between the arm
that works and the arm that hangs is exactly "does the consumer go through an
intercepted native, or through `awaitDone`".

The brief also suggested the `Executors`-style cure (L10: *drop* the native so
real bytecode runs). That is the wrong direction here. Dropping
`ForkJoinTask.fork()` would hand the task to the real `WorkQueue`/`ctl`
machinery of a pool with no workers — the hang would move, not go away — and
the drop-registration comment in `registry.rs` records that letting `fork()`
fall through "reaches the real WorkQueue/CAS path and reopens the residual
timeout/corruption face". The cure is the opposite: register the *missing*
inline Bridge.

## What changed

### 1. `native-builtins/src/phases_late/concurrent.rs` (this lane's file)

New `register_forkjointask_invoke_all_bridge`, registering all three static
overloads as `NativeKind::Bridge`:

| class | method | descriptor |
| --- | --- | --- |
| `java/util/concurrent/ForkJoinTask` | `invokeAll` | `(Ljava/util/concurrent/ForkJoinTask;Ljava/util/concurrent/ForkJoinTask;)V` |
| `java/util/concurrent/ForkJoinTask` | `invokeAll` | `([Ljava/util/concurrent/ForkJoinTask;)V` |
| `java/util/concurrent/ForkJoinTask` | `invokeAll` | `(Ljava/util/Collection;)Ljava/util/Collection;` |

Each one pins every task (a running task body allocates freely, so a sibling
held as a bare address would be stale by the next iteration) and then drives
each task through `join()`.

`join()` — not a direct `compute()` invoke — is deliberate. It *is* the
coherent model: it computes a task that has not run, returns the memoised
result for one that has (so a task already driven by an enclosing `join()` is
never run twice), and rethrows the task's own throwable **unwrapped**, which is
precisely what the real `invokeAll` does via `reportExecutionException`. A
first failure aborts the batch, matching the JDK: `invokeAll(t1, t2)` never
reaches `t2`'s wait if `t1` completed abnormally.

The registrar is called from **both** `register_new15_loom` (synthetic path)
and `register_t19_k3_forkjoinpool_common` (which `lib::register_essential_natives`
calls on the real-JDK boot path, `native-builtins/src/lib.rs:9815`). No change
to `lib.rs` was needed.

### 2. Two out-of-file list edits (see *Out-of-file patches*)

A registration on `ForkJoinTask` is **inert** unless three lists agree
entry-for-entry. `registry.rs` records the precedent in its own comment:
`awaitQuiescence` was in the interpreter's force-list but not the registry's
keep-list, so the registration was dropped and the "force the native" had no
native to force.

1. `native-api/src/registry.rs`, `keep_real_forkjointask_bridge` — in real-FJP
   mode every `ForkJoinTask`/`RecursiveTask`/`RecursiveAction` native **not**
   on this list is dropped at registration time.
2. `vm/src/runtime/interpreter/native_override.rs`,
   `is_forkjoin_native_override` — what forces the native to win over the real
   JDK bytecode (consulted from `vm/src/vm/vm_exec.rs:20747` and
   `native_override.rs:3366`).

## Out-of-file patches (must be applied for this fix to do anything)

**Patch A — `native-api/src/registry.rs`**, in `keep_real_forkjointask_bridge`.
The anchor line below is the file's **only** occurrence of `getException`, so
no line number is needed (and registry.rs is being edited concurrently by other
lanes — do not trust a line number here). Replace:

```rust
                    | ("getException", "()Ljava/lang/Throwable;")
```

with:

```rust
                    | ("getException", "()Ljava/lang/Throwable;")
                    // L12: the STATIC `invokeAll` overloads. JDK 25's
                    // `invokeAll(t1, t2)` runs one task inline and then blocks
                    // in `awaitDone` for the FORKED sibling — which the lazy
                    // `fork()` above never schedules and no worker thread
                    // exists to run. RJdkForkJoin hung there forever.
                    | (
                        "invokeAll",
                        "(Ljava/util/concurrent/ForkJoinTask;Ljava/util/concurrent/ForkJoinTask;)V",
                    )
                    | ("invokeAll", "([Ljava/util/concurrent/ForkJoinTask;)V")
                    | ("invokeAll", "(Ljava/util/Collection;)Ljava/util/Collection;")
```

**Patch B — `vm/src/runtime/interpreter/native_override.rs`**, in
`is_forkjoin_native_override`. Again the anchor is the file's only
`getException` line. Replace:

```rust
            | ("getException", "()Ljava/lang/Throwable;")
```

with:

```rust
            | ("getException", "()Ljava/lang/Throwable;")
            // L12: the STATIC `invokeAll` overloads — the last real-bytecode
            // route from the lazy `fork()` above to an `awaitDone()` that no
            // worker thread can satisfy. Must stay in step with
            // `keep_real_forkjointask_bridge` in native-api/src/registry.rs.
            | (
                "invokeAll",
                "(Ljava/util/concurrent/ForkJoinTask;Ljava/util/concurrent/ForkJoinTask;)V"
            )
            | ("invokeAll", "([Ljava/util/concurrent/ForkJoinTask;)V")
            | ("invokeAll", "(Ljava/util/Collection;)Ljava/util/Collection;")
```

## What this does NOT fix

`RJdkForkJoin` is expected to progress past `recursiveTasks()` and then fail
**fast and loudly** in `countedCompleter()` (line 174-176), with
`AssertionError: CountedCompleter leaves: 0`. That is a *different* defect with
the same origin: `Counter.compute` (line 150-161) does

```java
setPendingCount(2);
new Counter(this, ...).fork();
new Counter(this, ...).fork();
tryComplete();
```

`fork()` is `final` in `ForkJoinTask`, so the lazy Bridge intercepts it for a
`CountedCompleter` receiver too — but a `CountedCompleter` has **no** `join()`;
its consumer is the `tryComplete()` / `onCompletion()` propagation protocol,
which never runs because the children never run. `leaves` stays 0 and
`completions` stays 0.

That is not fixable inside this lane's files: `native-api/src/registry.rs`
drops *every* `java/util/concurrent/CountedCompleter` native in real-FJP mode
(the class appears in the drop list right after `keep_real_forkjointask_bridge`
but **not** in the keep-list's own class match), so nothing can be registered
on it.

The principled cure is to make `fork()` **eager** — run the body via
`fjp_compute_for_submit`, which records completion in the side table so a
later `join()` returns the memoised result instead of re-running. That is
already exactly what `ForkJoinPool.execute(ForkJoinTask)V` does
(`native-builtins/src/lib.rs:9758-9781`), and its comment records the bug that
forced it: a bare `exec()` invoke that never RECORDED the task as done made the
matching `join()` run `compute()` a second time. The historical reason `fork()`
is lazy — "eager fork was overflowing the host stack on deeply-recursive
RecursiveTask probes" — is very likely that same non-recording double-execution
turning linear work into `2^depth`, not a genuine depth problem: with eager
fork, `left.fork(); right.compute(); left.join()` is no deeper than the lazy
form, which runs the identical subtree from `left.join()` at the same frame.

That change was **not** made here: it is outside this lane's files, cannot be
measured in this lane (no build, no VM), and would alter `fork()` ordering for
every Spring / Hibernate / JUnit / stream workload in the corpus. It wants its
own lane with an A/B.

`java.util.stream`'s parallel machinery (`AbstractTask extends
CountedCompleter`) is on the same fault line, so `parallelStreams()`
(line 184-216) may also produce wrong answers rather than hanging. Unknown
until a binary exists.

## How to verify (once a binary exists)

```sh
cd regression-suite
javac -d out src/RJdkForkJoin.java

# Both arms must get past the fill and print the first checkpoint.
timeout 300 cratonvm --real-jdk -cp out RJdkForkJoin
timeout 300 cratonvm --jdk-only -cp out RJdkForkJoin
```

The **hang** is fixed iff both arms terminate well inside the budget (exit code
is no longer 124) and both print

```
CK RJdkForkJoin sum=199990000 fill=24995000
```

which is the line immediately after the previously-hanging `pool.invoke(new
FillAction(...))`. Expect the run to then stop at
`AssertionError: CountedCompleter leaves: 0` — that is the separate, documented
follow-up above, not a regression from this change.

Under the watchdog, the signature to confirm gone is any frame named
`ForkJoinTask.awaitDone`:

```sh
CRATONVM_DEFAULT_WATCHDOG_SEC=45 cratonvm --real-jdk -cp out RJdkForkJoin
```

Regression guard for the rest of the corpus: `RJdkExecutors` (ordinary
`ThreadPoolExecutor`) must still pass, and the ForkJoin-touching suites
(`vm/tests/fjp_recursive.rs`, `vm/tests/rfjp1_recursive.rs`,
`probes/FjpMatrixProbe.java`) must be unchanged — the three new natives only
add coverage for methods that previously fell through to real bytecode, so a
change in any of those points at the list edits, not at the registrar.

## Bookkeeping

`scripts/baselines/jdk-only-kind-map-25-linux.tsv` gains three `bridge` rows
under `java/util/concurrent/ForkJoinTask invokeAll ...` once a binary can
re-freeze it. The baseline is a reference dump, not a compile-time gate, so it
does not block this change.
