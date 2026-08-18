# The ThreadContainer was dropped on start, and the JDK never takes it back off on exit

> **INTERLOCK FLIPPED 2026-08-12, UNRUN.**
> `native-builtins/src/shared_secrets_bridge.rs`'s
> `VM_REMOVES_THREADS_FROM_CONTAINERS` is now **`true`**. The blocker is gone:
> the de-registration half landed as `run_thread_exit_shared` in
> `vm/src/vm/vm_exec.rs`, invoked on both worker-death paths
> (docs/known-issues/jdk-only/W7-27-thread-exit-java-cleanup.md). So the
> registration half is live in every mode, and §4's "the add lands, the remove
> does not" measurement is now the thing to re-take rather than the reason to
> stay off.
>
> **This is a flip-and-measure, and the failure mode is a HANG.** Read
> §10 *"Reading a wrong flip out of a suite transcript"* before the run — it is
> written for whoever is looking at the output, not for whoever writes the fix.
> The one-line summary: a vector that stops mid-transcript with no `FAIL` and no
> `PASS` is this, and `CRATONVM_THREAD_CONTAINERS=0` restores the old behaviour
> in the same binary without a rebuild.
>
> Nothing in the flip was built or run. What is claimed is that the blocker no
> longer exists and that the constant now states the VM's actual behaviour;
> what is NOT claimed is that the pair works.

**Status (before the flip): the registration half is written in
`native-builtins/src/shared_secrets_bridge.rs` and is interlocked OFF, because
the de-registration half does not exist in this VM and landing the two out of
order is measurably a hang.** Nothing was rebuilt in this lane. Every number
below was taken with HotSpot Adoptium 25.0.3.9 and with the **pre-change**
binary `C:/craton/CratonVM/target/release/cratonvm.exe` (built 2026-08-11
19:41), and every one is reproducible from the probe sources quoted in §7.

This continues docs/known-issues/jdk-only/W7-18-structured-task-scope-jep505.md,
which bisected `StructuredTaskScope.join()` down to one dropped argument and
wrote a patch under *Out-of-file patch (not applied)*. That patch was checked
against the source first: **it had not been applied** — `jla_start_in_container`
still read `args[2]` only in its doc comment's parameter list. It was then
measured, and its preferred form is **wrong**: applied alone it hangs. §4 is
that measurement. This record exists because the correction is worth more than
the fix.

## 1. What the JDK contract actually requires

Four files out of `jdk-25.0.3.9-hotspot/lib/src.zip`. Note the package: JDK 25's
flock is **`jdk.internal.misc.ThreadFlock`**, not `java.util.concurrent`.

`jdk/internal/misc/ThreadFlock.java` — the wait condition, and the only thing
that moves it:

```java
    public Thread start(Thread thread) {
        ensureOwnerOrContainsThread();
        JLA.start(thread, container);
        return thread;
    }

    public boolean awaitAll() throws InterruptedException {
        ...
            return (threadCount == 0);
        while (threadCount > 0 && !permit) { ... LockSupport.park(); }
```

`threadCount` is `private volatile int` and is written in exactly two places,
both reached only through the container:

```java
    private void onStart(Thread thread) {
        incrementThreadCount();
        ...
    }

    private void onExit(Thread thread) {
        boolean removed = threads.remove(thread);
        assert removed;
        decrementThreadCount();
    }

    private void decrementThreadCount() {
        int count = (int) THREAD_COUNT.getAndAdd(this, -1) - 1;
        // signal owner when the count goes to zero
        if (count == 0) {
            LockSupport.unpark(owner());
        }
    }
```

`jdk/internal/vm/ThreadContainer.java` states when each hook runs, and the
comments are the contract:

```java
    /**
     * Invoked by {@code add} to add a thread to this container before it starts.
     */
    protected void onStart(Thread thread) { }

    /**
     * Invoked by {@code remove} to remove a thread from this container when it
     * terminates (or failed to start).
     */
    protected void onExit(Thread thread) { }
```

`java/lang/Thread.java` is where both calls come from. `start(ThreadContainer)`
is package-private, and it is a **different method from `start()`** — `start()`
does not touch containers at all:

```java
    void start(ThreadContainer container) {
        synchronized (this) {
            if (holder.threadStatus != 0) throw new IllegalThreadStateException();
            if (this.container != null)    throw new IllegalThreadStateException();
            setThreadContainer(container);
            boolean started = false;
            container.add(this);  // may throw
            try {
                inheritScopedValueBindings(container);
                start0();
                started = true;
            } finally {
                if (!started) {
                    container.remove(this);   // <- the ABNORMAL failed-to-start path
                }
            }
        }
    }
```

and the pair, in the method the VM calls on the terminating thread:

```java
    /**
     * This method is called by the VM to give a Thread
     * a chance to clean up before it actually exits.
     */
    private void exit() {
        try {
            if (headStackableScopes != null) StackableScope.popAll();
        } finally {
            // notify container that thread is exiting
            ThreadContainer container = threadContainer();
            if (container != null) {
                container.remove(this);
            }
        }
        ...
    }
```

So the contract is three obligations, not one: **bind** the thread to the
container (`setThreadContainer`), **add** it before it runs, and **remove** it
when it terminates *or fails to start*. `Thread.start(ThreadContainer)` owns the
first two and the failed-to-start case; `Thread.exit()` owns termination. A VM
that does not run `Thread.exit()` cannot honour the third.

`VirtualThread` overrides `start(ThreadContainer)` and ends in
`externalSubmitRunContinuationOrThrow()` rather than `start0()` — W7-18 flagged
that as the reason its patch might not be safe. **It does not apply on this VM.**
`jdk/internal/vm/ContinuationSupport.isSupported0()Z` is registered to return
`0` (`native-builtins/src/phases_late/concurrent.rs`), so `Thread.ofVirtual()`
yields `ThreadBuilders$BoundVirtualThread`, which overrides `run`, `park` and
`parkNanos` and **does not override `start(ThreadContainer)`**. It inherits
`Thread`'s, the one quoted above, which ends in `start0()` — and `start0()V` is
registered on the real-JDK path. Measured, not read: §4's experiment starts a
thread through that exact body on the current binary and it runs.

## 2. Baseline, measured (N=3 per arm)

`ContainerRegistrationProbe` (§7) reads the observables directly instead of
inferring them from `join()`'s behaviour.

| line | HotSpot 25 | `--real-jdk` | `--jdk-only` |
|---|---|---|---|
| `flock.class` | `jdk.internal.misc.ThreadFlock` | same | same |
| `flock.threadCount.beforeFork` | 0 | 0 | 0 |
| **`flock.threadCount.afterFork`** | **1** | **0** | **0** |
| `flock.join.blockedAtLeast300ms` | true | false | false |
| **`flock.containsThread.inTask`** | **true** | `NOT-RUN` | `NOT-RUN` / `false` |
| `flock.subtask.state` (after join) | SUCCESS | UNAVAILABLE | UNAVAILABLE |
| `flock.threadCount.afterJoin` | 0 | 0 | 0 |
| `abnormal.threadCount.afterFork` | 1 | 0 | 0 |
| `abnormal.join` | `FailedException:ISE: boom` | `no-throw` | `no-throw` |
| `abnormal.threadCount.afterJoin` | 0 | 0 | 0 |

`flock.containsThread.inTask=NOT-RUN` is itself the defect rather than a probe
gap: the task sets that cell after it starts, the owner reads it after `join()`
returns, and on CratonVM the read wins. HotSpot cannot produce `NOT-RUN` because
`join()` cannot return before the task has finished.

**Stability.** HotSpot: 3/3 byte-identical. `--real-jdk`: 3/3 byte-identical.
`--jdk-only`: 2/3 — `flock.containsThread.inTask` was `NOT-RUN` twice and
`false` once, which is the same race read at a different moment.

`probes/StructuredTaskScopeProbe` re-run today reproduces W7-18's asymmetry, and
sharpens it in one direction and softens it in another:

* CratonVM `--real-jdk`, 3 runs: **8 distinct content lines** varied —
  `awaitAll.ok.state`, `awaitAll.bad.state`, `allSuccessful.join.elements`,
  `allUntil.predicate.invocations`, `allUntil.join.elementStates`,
  `forkRunnable.state`, `configuration.subtask.get`, `subtask.b.get`.
* HotSpot, **8 runs**: 145 of 147 lines are fixed. W7-18 called the HotSpot
  transcript byte-identical across three runs; over eight it is not.
  `allUntil.join.elementStates` reorders in 7 of 8 runs
  (`UNAVAILABLE,SUCCESS` vs `SUCCESS,UNAVAILABLE`) and once
  `allUntil.predicate.invocations` read 2 rather than 1. That is a genuine race
  **in the probe's `allUntil` section**, not in the VM — two subtasks with no
  ordering between them — and it means the "HotSpot is stable, CratonVM is not"
  claim should be stated as *145/147 stable* and confined to the other sections.
  The `joinWaits.*` block, which is the one that matters, was stable 8/8.

`joinWaits`, unchanged from W7-18: `taskFinishedWhenJoinReturned` true/false,
`joinBlockedAtLeast200ms` true/false, `subtask.get` `done` vs
`ISE:Result is unavailable…`, `forkRunnable.ran` 1 vs 0,
`timeout.join` `TimeoutException` vs `no-throw`.

## 3. Where the container goes

`jla_start_in_container` in `native-builtins/src/shared_secrets_bridge.rs`,
registered twice in `register_java_lang_access` — on `java/lang/System$1` and on
the `jdk/internal/access/JavaLangAccess` interface — was:

```rust
    ctx.invoke_virtual(thread_obj, "start", "()V", &[])?;
```

`args[2]` was never read. `Thread.start()V` is itself shadowed by
`native_thread_start0`, so the call re-enters CratonVM's spawn and the JDK's
`Thread.start(ThreadContainer)` body — the only writer of `thread.container` and
the only caller of `container.add` — never executes.

`java/lang/Thread.start(Ljdk/internal/vm/ThreadContainer;)V` is registered
nowhere; `VirtualThread.start` is registered nowhere; no native is registered on
`ThreadContainer`, `ThreadContainers`, `SharedThreadContainer` or `ThreadFlock`
in any mode. Those four classes run as real JDK bytecode, and the bridge is the
one place the chain is cut.

## 4. Why W7-18's preferred patch is wrong: the add lands, the remove does not

The patch is testable **without a rebuild**, because
`Thread.start(ThreadContainer)` is ordinary package-private bytecode that no
native shadows: calling it reflectively executes exactly the body a fixed
bridge would reach. `ExitPairingProbe` (§7) does that against a live
`StructuredTaskScope`'s own flock and reads the count on both sides of the
worker's death.

```
                                          HotSpot 25   cratonvm --real-jdk
pair.container.class            jdk.internal.misc.ThreadFlock$ThreadContainerImpl (both)
pair.count.beforeStart                        0                0
pair.count.afterStart                         1                1
pair.worker.threadContainer.whileLive        set              set
pair.worker.ran                              true             true
pair.count.afterTermination                   0                1     <- not removed
pair.leaked                                 false             true
hang.join.returnedAfterMs                   0, 0, 1      SECTION-HUNG (3/3)
```

3 runs per arm, byte-identical within each arm.

Two things are established at once. **The registration half works on this VM
today** — `0 -> 1`, through the real `Thread.start(ThreadContainer)`, on a
`BoundVirtualThread`-era build with `start0()` doing the spawn. And **the
de-registration half does not exist**: the count stays at 1 forever after the
worker dies, and a `join()` issued at that point never returned in any of three
runs, where HotSpot returned in 0–1 ms.

The cause is not in this file. CratonVM never invokes `Thread.exit()V`. Its
platform-thread termination path (the spawn closure in `vm/src/vm/vm_exec.rs`)
runs `run()`, dispatches the uncaught-exception handler on failure, emits the
JFR thread-end event, retires the TLAB, marks the thread dead in the registry
and notifies the VM-level termination monitor. The virtual path is the mirror
image. **No Java-level cleanup runs on either, on either the normal or the
abnormal path** — so `container.remove(this)`,
`TerminatingThreadLocal.threadTerminated()` and `StackableScope.popAll()` are
all unreachable.

So W7-18's own falsifier — *"If instead `join()` now hangs, the `add` half
landed and the `remove` half did not"* — is not a risk to watch for. It is the
outcome, and it is now measured rather than predicted. This is the second time
in this campaign a record's prescribed fix has been wrong in a way only running
it could show.

**Blast radius of the missing `Thread.exit()` is wider than containers**, and it
is the real bug this lane found. It is filed here rather than in a lane of its
own because the container fix is blocked on it.

## 5. Blast radius of the missing registration, measured not asserted

The registry has several consumers and they fail differently. What was measured:

| consumer | today | why |
|---|---|---|
| `StructuredTaskScope.join()` / `ThreadFlock.awaitAll()` | **wrong, racy** — 15 divergent lines, 8 of them run-unstable | `threadCount` stuck at 0 |
| `ThreadFlock.containsThread` | **false inside the task** (true on HotSpot) | never added to `threads` |
| `ThreadFlock.threads()` | empty | same |
| `ThreadContainers.root()` count | **accidentally right** — `root.delta=1` for a live platform thread on both arms | `TrackingRootContainer.threadCount()` is `platformThreads().count() + VTHREADS.size()`, and `platformThreads()` enumerates every thread whose container is null. CratonVM's "virtual" threads are `BoundVirtualThread`, i.e. real platform threads, so they are counted by the first term instead of the second |
| `Thread.threadContainer()` on a terminated virtual thread | **`null`** (HotSpot: `…RootContainer$TrackingRootContainer`) | `VirtualThread.start()` is `start(ThreadContainers.root())`; CratonVM never reaches a container-taking start |
| `Thread.getAllStackTraces` | diverges, but **not through this defect** — `allstacks.containsTaskThread` is `true` here and `false` on HotSpot | CratonVM's is backed by the VM thread registry, not a `ThreadContainers` walk; the divergence is that its virtual threads are platform threads |
| Jetty's `SharedThreadContainer` | works, and would **not** hang under the add-only patch | it never waits on a count; `close()` just deregisters and `threads()` filters on `isAlive`. It would instead accumulate dead entries in `virtualThreads` — a leak, not a stall |

Two asymmetries are worth keeping. First, `root.threadCount` is right **by
accident**, and the accident reverses the day continuations are supported:
`platformThreads()` filters on `JLA.threadContainer(t) == null`, so a thread that
is bound to a container but never removed disappears from root enumeration as
well. Registration without de-registration corrupts this consumer in the
opposite direction from the one it fixes. Second, `SharedThreadContainer` — the
caller this bridge was written for — is the one consumer that tolerates either
bug, which is exactly why the passthrough survived: the workload that motivated
it cannot see the difference.

## 6. What landed, and in which modes

`native-builtins/src/shared_secrets_bridge.rs` only.

* `jla_start_in_container` now reads `args[2]` and, when registration is
  enabled, invokes `Thread.start(Ljdk/internal/vm/ThreadContainer;)V` — the JDK
  body, i.e. `setThreadContainer` + `container.add(this)` + `start0()`, with the
  JDK's own `finally { container.remove(this); }` covering failed-to-start.
* Registration was **off** by default, via one named constant
  `VM_REMOVES_THREADS_FROM_CONTAINERS`, whose doc comment carries §4's table and
  says what flipping it requires. `CRATONVM_THREAD_CONTAINERS=1`/`=0` overrides
  it at runtime so the two arms can be compared in **one** binary once the pair
  lands, rather than by rebuilding.
* When a non-null container is dropped, one `tracing::warn!` per process names
  what will not see the thread.

**2026-08-12: the constant is now `true`.** The clause above — "because the
enabled branch is not taken" — no longer holds, and the paragraph below is kept
as the record of what the *interlocked* state was. `CRATONVM_THREAD_CONTAINERS=0`
selects it in the same binary.

**Observable change while the interlock was off, precisely.** In every mode —
`Compatible`, `--real-jdk`, `--jdk-only`, synthetic — the threads started are the
same threads started the same way, byte-for-byte, because the enabled branch is
not taken. The only new output is a single WARN line per process, and only in a
run that actually starts a thread through a non-null container (Jetty's pool, any
`StructuredTaskScope` fork). A run that starts no such thread is unchanged
including its stderr.

**Observable change now that it is `true`.** In every mode, a thread started
through `JavaLangAccess.start(Thread, ThreadContainer)` with a **non-null**
container now runs the JDK's own `Thread.start(ThreadContainer)` body:
`setThreadContainer(container)` + `container.add(this)` + `start0()`. `start0()V`
is still CratonVM's own spawn, so the thread is created the same way; what is new
is the bookkeeping and its consumers waking up. A thread started through
`Thread.start()` — every ordinary thread, every `Executors` pool thread — is
untouched, because that is a different triple and this bridge is not on it. The
WARN line disappears from runs that used to emit it, which is itself the cheapest
positive signal that the flip took effect.

The constant is deliberately a lever and not `#[cfg]`: a default-off inner
`#[cfg]` gate is the shape this campaign has already found eleven times as
"class-library gaps" that were really switched-off code
(docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md censuses the
same failure). The warning is what keeps this one from reading as absence.

## Out-of-file patch — A APPLIED (as W7-27); B is a note

### A. `vm/src/vm/vm_exec.rs` — call `Thread.exit()` on the terminating thread

> **APPLIED. Do not re-apply.** This landed as
> `run_thread_exit_shared` in `vm/src/vm/vm_exec.rs`, called from both
> worker-death paths, unconditional, in every mode — see
> docs/known-issues/jdk-only/W7-27-thread-exit-java-cleanup.md §4 for what the
> landed form does differently from the sketch below (`get_loaded_class_id` +
> `invoke_special_shared_on_class` rather than the `find_class_id` /
> `invoke_on_class_shared` shape written here, and a presence check on `exit()V`
> so a stub `java/lang/Thread` is a no-op rather than a warning per thread
> death). All three properties this section required hold. **Its one gap is the
> primordial thread** — W7-27 §10C — which is not a blocker for this record: a
> container the main thread never leaves has no observer left to see it, because
> the process is ending.
>
> The sketch is kept below unedited, because the *reasoning* about the insertion
> point is what the landed form was checked against.

This is the blocker, and it is worth more than the container fix on its own:
`Thread.exit()` also drives `TerminatingThreadLocal.threadTerminated()` and
`StackableScope.popAll()`, neither of which has ever run in this VM.

In the platform-thread spawn closure in `NativeContextImpl::thread_start`, after
`run()` has returned **and after the uncaught-exception handler has been
dispatched** — the abnormal path must reach this too, which is the whole point —
and before the TLAB retire / `mark_dead` / termination-monitor notify:

```rust
    // `Thread.exit()` is what HotSpot's VM calls to let a thread clean up:
    // `StackableScope.popAll()`, `container.remove(this)`, then
    // `TerminatingThreadLocal.threadTerminated()` and `clearReferences()`.
    // CratonVM has never called it, so every one of those is dead code on this
    // VM. `ThreadContainer.remove` is the load-bearing one: it is the only
    // decrement of `ThreadFlock.threadCount`, and it is what unparks an owner
    // blocked in `awaitAll()`. Measured without it: a flock keeps a dead
    // thread forever and `join()` never returns (3/3) — see
    // docs/known-issues/jdk-only/W7-23-thread-container-registration.md §4.
    //
    // Runs on BOTH paths on purpose. A task that ends by throwing is exactly
    // when a structured-concurrency owner is waiting for the count to fall.
    // `exit()` is private on `java/lang/Thread`, so it must be resolved on
    // `java/lang/Thread` itself and not on the receiver's class: a Thread
    // subclass does not inherit it into its own vtable.
    if let Some(thread_cid) = shared_arc.classes.find_class_id("java/lang/Thread") {
        let exit_result = invoke_on_class_shared(
            &shared_arc,
            &mut jvm_thread,
            thread_cid,
            "exit",
            "()V",
            &[Value::Object(Some(run_thread_obj))],
        );
        if let Err(e) = exit_result {
            // Never propagate: this runs after the thread's own result has
            // been settled, and a failure here must not turn a clean thread
            // into a crashed one or skip the monitor notify below — that
            // notify is what wakes `Thread.join`.
            tracing::warn!("Thread.exit() failed for tid {}: {:?}", tid.0, e);
        }
    }
```

The same block belongs on the virtual-thread termination path, which invokes
`run()` and dispatches the uncaught handler in the same shape.

`find_class_id` is written as the shape of the lookup, not as a verified API
name — resolve it against whatever the closure already uses to name a class.
The three properties that must hold are: (1) it runs on both the normal and the
uncaught-exception path, (2) it cannot skip the monitor notify that follows, and
(3) it resolves `exit()V` on `java/lang/Thread`, not on the receiver's class.

**Falsifier.** With this landed and `CRATONVM_THREAD_CONTAINERS=1`, on
`--real-jdk`:

* `ExitPairingProbe`: `pair.count.afterTermination` must become `0` and
  `hang.join.returnedAfterMs` must print a number instead of `SECTION-HUNG`.
  Check that one **first** — it is the hang, and it is cheap.
* `ContainerRegistrationProbe`: `flock.threadCount.afterFork=1`,
  `flock.containsThread.inTask=true`, `flock.join.blockedAtLeast300ms=true`,
  `abnormal.threadCount.afterJoin=0`.
* `probes/StructuredTaskScopeProbe`: `joinWaits.taskFinishedWhenJoinReturned`
  and `joinBlockedAtLeast200ms` become `true`, `joinWaits.subtask.get` becomes
  `done`, `timeout.join` becomes `StructuredTaskScope$TimeoutException`. Run it
  three times: the eight unstable lines in §2 must stop moving, except
  `allUntil.*`, which moves on HotSpot too.
* Run a Jetty-backed suite. The bridge exists because `NoSuchMethodError` here
  aborted Jetty's pool at startup; `SharedThreadContainer` never blocks on a
  count, so it should be unaffected, but "should" is not a measurement.
* Watch for the opposite regression: a thread that is removed twice, or removed
  from a container it was never added to, trips `assert removed` in
  `ThreadFlock.onExit`. Assertions are off by default; run one arm with `-ea`.

### B. `Thread.exit()` is not the only Java-level cleanup that never runs

Recorded so it is not rediscovered. With `Thread.exit()` wired up, three further
JDK behaviours become live for the first time and none has ever been exercised
here: `TerminatingThreadLocal.threadTerminated()` (JDK-internal, but
`jdk.internal.misc` users depend on it), `StackableScope.popAll()`, and
`clearReferences()` nulling `threadLocals` / `inheritableThreadLocals` /
`inheritedAccessControlContext`. The last one is a thread-local retention
question, which is a different lane and a different oracle
(`CRATONVM_DBG_ROOT_SOURCE=1`).

## 7. Probe sources

Not checked in under `probes/`: this lane owns two files and neither is there.
Both are reflection-only for the reason W7-18 §4 gives — naming
`StructuredTaskScope` in source mints a `69.65535` class file that only HotSpot
will load, and then only behind `--enable-preview`, which CratonVM has no flag
for. Both bound every section on a daemon thread so a wrong VM prints
`SECTION-HUNG.x` instead of stopping the transcript.

Both need the same flags on both arms:

```sh
OPENS="--add-opens java.base/java.util.concurrent=ALL-UNNAMED \
       --add-opens java.base/jdk.internal.misc=ALL-UNNAMED \
       --add-opens java.base/jdk.internal.vm=ALL-UNNAMED \
       --add-opens java.base/java.lang=ALL-UNNAMED"
```

`ContainerRegistrationProbe` reads the observables on an ordinary `fork`:
resolve `StructuredTaskScope.open()`, pull the `ThreadFlock`-typed field off the
returned `StructuredTaskScopeImpl`, and print `flock.threads().count()` before
the fork, immediately after it, after `join()` and after `close()`, plus
`containsThread(currentThread)` recorded from inside the task; then the same
around a task that throws; then `ThreadContainers.root().threadCount()` across a
live platform thread; then `Thread.threadContainer()` on a terminated virtual
thread.

`ExitPairingProbe` is the one that decides the fix's shape, and it is short
enough to quote whole in its load-bearing part:

```java
    Object scope = STS.getMethod("open").invoke(null);
    Object flock = fieldOfType(scope, "ThreadFlock").get(scope);          // setAccessible
    Object container = fieldOfType(flock, "ThreadContainer").get(flock);  // setAccessible
    Method threads = flock.getClass().getMethod("threads");

    // exactly what a fixed jla_start_in_container reaches
    Method startInContainer = Thread.class.getDeclaredMethod(
            "start", Class.forName("jdk.internal.vm.ThreadContainer"));
    startInContainer.setAccessible(true);

    Thread t = new Thread(() -> sleep(400), "pair-worker");
    startInContainer.invoke(t, container);
    print("pair.count.afterStart", count(threads, flock));   // 1 on both VMs
    t.join(8000); sleep(300);
    print("pair.count.afterTermination", count(threads, flock)); // 0 vs 1
```

and then, in its own bounded section, the consequence: start a worker in the
container, wait for it to die, and call `scope.join()`. HotSpot returns in 0–1
ms; the current binary does not return. The `pair` section deliberately never
calls `join()` or `close()` — both spin on `threadCount > 0`, so on a leaking VM
they are the hang this probe exists to *predict* rather than to demonstrate.

## 10. Reading a wrong flip out of a suite transcript

Written for whoever is looking at the run, not for whoever wrote the fix. The
flip's failure mode is a **hang**, so it does not produce a `FAIL` line, and the
thing that makes it hard to recognise is that the transcript simply stops being
about the vector that is stuck.

**Signature 1 — the one to expect.** A vector that opens a
`StructuredTaskScope`, or reaches `jdk.internal.misc.ThreadFlock` any other way,
stops mid-transcript: its `CK` lines up to some point, then no `PASS`, no `FAIL`,
and the harness eventually times out or the whole run is killed. The owner thread
is parked in `LockSupport.park()` inside `awaitAll()` on a `threadCount` that
never fell. **Distinguish it from the other shape first**, because in this tree a
vector that produces no result line is more often a crash than a wait: this one
leaves the process alive and parked, with no exit code and no stack. One look at
whether the process is still running settles it, and it costs nothing.

**What to do about it, in order, without a rebuild.**

1. Re-run the same vector with `CRATONVM_THREAD_CONTAINERS=0`. If it passes, the
   flip is the cause and nothing else in the wave is implicated. That single
   comparison is the whole diagnosis; it is why the lever exists.
2. Re-run with `-ea` (assertions on). If `ThreadFlock.onExit`'s
   `assert removed` fires, the failure is the **opposite** regression — a thread
   removed twice, or removed from a container it was never added to — which is a
   defect in the pairing rather than in the count.
3. Look for `WARN … Thread.exit() failed on terminating thread …`. One line per
   thread death means `exit()` is being reached and throwing inside real JDK
   bytecode; the error text names which call. The container is then still held,
   so signature 1 follows from it and this is the earlier, more informative
   symptom.

**Signature 2 — the silent one.** A `ThreadContainers.root()` count that used to
be right becomes wrong. §5 records why: `platformThreads()` filters on
`JLA.threadContainer(t) == null`, so a thread that IS bound to a container drops
out of root enumeration. Registration therefore moves this consumer in the
opposite direction from the one it fixes, and it does so with no error. Anything
reading a thread dump or `getAllStackTraces` is where this shows.

**What is NOT evidence either way.** Jetty and Tomcat pools use
`SharedThreadContainer`, which never waits on a count — `close()` just
deregisters and `threads()` filters on `isAlive`. A green Jetty or Tomcat arm
says nothing about this flip in either direction (§5's last row). Likewise, a run
that starts no thread through a non-null container is byte-identical to the
pre-flip run, so a green suite is only informative for the vectors that
actually reach a container.

**Coverage, stated plainly: there is none, and it is not for want of trying.**
No scheduled `regression-suite` vector exercises a `ThreadContainer`, and one
cannot be written in the ordinary way: naming `StructuredTaskScope` in Java
source mints a `69.65535` class file HotSpot refuses without `--enable-preview`
and CratonVM has no flag for (W7-18 §2), so the vector has to be reflection-only
*and* needs `--add-opens java.base/jdk.internal.misc=ALL-UNNAMED` plus
`java.base/jdk.internal.vm`, which `regression-suite/run.sh`'s `class_args` does
not supply for any class. **Scheduling need, for whoever owns `run.sh`:** a
`class_args` entry granting those two opens to one new reflection-only vector is
the minimum that would let `ExitPairingProbe`'s two load-bearing lines
(`pair.count.afterTermination`, `hang.join.returnedAfterMs`) become a scheduled
assertion instead of a probe nobody runs. Until then this record's falsifier is a
hand-run, and `probes/` is never run by `run.sh`.

The one thing that IS scheduled and does move on the pair is the *other* half:
`regression-suite/src/RJdkExecutors.java`'s `threadExitCleanup()` asserts that a
terminated thread no longer reports the uncaught handler installed on it, which
is `Thread.exit()`'s `clearReferences()` observed through public API with no
opens at all. It covers W7-27, not this record — but a red there means the
decrement's *driver* is not running, which is the first thing to rule out before
reading any hang here as a container defect.

## 11. Verification pass 2026-08-12 (lane A16) — the flip is live and still unmeasured

**Nothing was built or run in this pass.** Source read against today's
worktree. The headline is unchanged: **this record is a flip awaiting a
measurement, and the measurement has not happened.**

### 11.1 The flip and its pair are where this record says they are

| claim | today | verdict |
|---|---|---|
| `VM_REMOVES_THREADS_FROM_CONTAINERS` is `true` | `shared_secrets_bridge.rs:765` | present |
| the runtime lever, read through `flags::runtime_var` (not `std::env::var`) | `:777`–`:786` | present — and the reason is written in place: a raw `getenv` is invisible to `flags::with_thread_overrides`, so an A/B through the supported hook would have measured the ambient environment |
| the bridge invokes the container-taking start | `:842`–`:847`, `"start"`, `"(Ljdk/internal/vm/ThreadContainer;)V"` | present |
| the WARN-once on the dropped path | `:860`–`:870` | present |
| `run_thread_exit_shared` | `vm/src/vm/vm_exec.rs:4998` | present |
| both worker-death call sites | `vm_exec.rs:4630` and `:13356` | present — the pairing this record was blocked on |

### 11.2 One hazard this record did NOT name, already answered by a later lane

The flip routes thread starts around `Thread.start()V` — the triple
`native_thread_start0` shadows — and onto a different triple. So the obvious
question is what happens to the **second-start** refusal, which was broken and
recently fixed (`Thread.start` must raise `IllegalThreadStateException`; a
second `start()` had been re-running the body and spawning a second OS thread
for a retired `Runnable`).

It is handled, and by argument rather than by accident.
`native-builtins/src/lang_system.rs:1048`–`:1058` carries the guard, and its
doc comment at `:1039`–`:1047` says explicitly that this native *"is the
convergence point of all four registrations **and of the container route**,
including the `--jdk-only` case where the real `start()` bytecode runs; a
refusal here does not leak a container registration, because that bytecode's
`finally` calls `container.onExit(this)`."* — i.e. `Thread.start(ThreadContainer)`
reaches `start0()`, which is still CratonVM's, so the guard still fires, and the
JDK's own `finally { container.remove(this); }` covers the failed-to-start case
§1 quotes. Note also that the JDK body's *own* two checks are not the guard
here: `holder.threadStatus` is inert on this VM (measured NEW=0 / terminated=0),
so only `this.container != null` fires from the bytecode side.

**Source argument, unrun.** It is the right argument and it is not a
measurement.

### 11.3 The flag surface is complete — recorded so nobody re-files it

This record introduced `CRATONVM_THREAD_CONTAINERS` and did not say what a new
`CRATONVM_*` name obliges. It is now declared in all four surface files:
`types/src/flag_groups.rs:1217` (`THREADS`/`thread-containers`),
`types/tests/flag-surface.txt:741`, `docs/flag-tokens.md:822`,
`docs/config/flag-inventory.md:1354` (`default-on`). W7-27 §10A landed it
(`78a0428ef`). `flag_declaration_guard.rs` will not fire.

### 11.4 §10's scheduling need is still unmet

`regression-suite/run.sh` contains **no** `--add-opens` anywhere — no
`jdk.internal.misc`, no `jdk.internal.vm`, for any class. So the reflection-only
vector §10 asks for still cannot be written, `ExitPairingProbe`'s two
load-bearing lines are still a hand-run, and `probes/` is still never run by
`run.sh` at any `SUITE=` value. **There is no scheduled assertion that can go
red or green on this flip.** The one adjacent scheduled thing is unchanged:
`regression-suite/src/RJdkExecutors.java::threadExitCleanup()` (`:373`, invoked
at `:480`), which covers W7-27's `clearReferences()` and is the first thing to
rule out before reading any hang here as a container defect.

### 11.5 A residual this record does not carry: the synthetic-mode arm

The enabled branch is **not** gated on JDK mode. `jla_start_in_container`
invokes `Thread.start(Ljdk/internal/vm/ThreadContainer;)V` with `?` on any
non-null container, in every mode, and `register_wp1_4_shared_secrets` (its
registrar, `native-builtins/src/lib.rs:10050`) is unconditional. The in-file
comment at `shared_secrets_bridge.rs:838`–`:841` argues synthetic mode never
presents a non-null container — fabricated `java/lang/Thread`, and the synthetic
`StructuredTaskScope` forks on the forking thread (W7-18 §6). **That is an
argument, not a guard.** If a non-null container ever does arrive there, the
`?` on an unresolvable method turns a working thread start into a failure, where
the pre-flip path fell through to `start()V`. Unmeasured, and the fix is a
`MethodCallFailed::InternalError`-only fallback — the same
`ExceptionThrown`-vs-`InternalError` discrimination
docs/known-issues/jdk-only/W7-26-getannotation-swallowed-exception.md's
`ladder_rung` makes — not a mode `#[cfg]`. Named here so the next reader of a
synthetic-mode thread failure does not start from scratch.

### 11.6 Today's probe screen says nothing about this record, and §10 predicts that

A 33-probe reachability screen was run on the current binary (`--jdk-only`
28/33). `ThreadMXBean` and `MXBean` **pass**. Per §5's last row and §10's *"What
is NOT evidence either way"*, that is expected and is not evidence: no probe in
the screen opens a `ThreadFlock` or reaches a `ThreadContainer`, and
`getAllStackTraces`-shaped consumers are backed by the VM thread registry, not
by a `ThreadContainers` walk. **No claim in this record is contradicted by the
screen; none is corroborated by it either.**

## What is not claimed

Nothing was rebuilt. The JDK contract, the two baselines, the eight-run HotSpot
stability figure, and §4's add-lands/remove-does-not measurement are all
measurements of HotSpot 25 and of the **pre-change** binary. They establish what
the JDK requires, that the container is dropped, that the registration half
works on this VM when it is reached, and that landing it alone hangs. They do
**not** establish that the edited source compiles, that the enabled branch
behaves, or that the `vm_exec.rs` patch above is correct as written.
