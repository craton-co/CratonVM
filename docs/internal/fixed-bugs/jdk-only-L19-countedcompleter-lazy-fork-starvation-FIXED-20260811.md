> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkForkJoin` passes in the 53/1 run, and the decision this record was gated on **has been taken**. It landed its cure behind `CRATONVM_FJP_EAGER_FORK`, OFF by default, and listed three conditions under "What would justify making the new behaviour the default", prescribing "invert `fjt_fork_mode`'s `None` arm to `CountedCompleterEager` and keep `0`/`-fjp-eager-fork` as the escape hatch". That is exactly what `native-builtins/src/phases_late/concurrent.rs:7107` now says: *"since 2026-08-07 this is the OPT-OUT (`CRATONVM_FJP_EAGER_FORK=0`), not the default"*. The knob was kept, as asked.
>
> Previous location: `docs/known-issues/jdk-only/L19-countedcompleter-lazy-fork-starvation.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `CountedCompleter` has no consumer to intercept, so a lazy `fork()` starves it silently

**Status:** cure implemented in source 2026-08-06 (lane L19, JDK-only wave 2)
**behind an environment gate that is OFF by default**. Nothing about the
shipped default changes. The gate exists because the fix is an *ordering*
change and this lane could not build or run anything — see *The A/B* below,
which is the whole point of the lane.

Sits directly behind L12
(`L12-forkjoinpool-no-workers-awaitdone-hang.md`), which predicted this defect
and recommended it get its own lane.

## The model this sits inside

CratonVM does not run ForkJoin tasks on worker threads: `NativeContext` is not
`Send`, so there is no thread to hand a task to. Instead the pool is
**eager-inline** and the interception happens at the *consumer* of a forked
task:

* `ForkJoinTask.fork()` is a deliberately LAZY Bridge — it marks the task
  queued in the `fjp_state` side table and returns
  (`native-builtins/src/phases_early.rs`,
  `register_forkjoin_natives` + `register_real_jdk_forkjoin_essentials`).
* `join()` / `get()` / `invoke()`, `ForkJoinPool.invoke` / `submit` /
  `execute` / `lazySubmit` / `invokeAll` / `invokeAny`, and (since L12) the
  three static `ForkJoinTask.invokeAll` overloads all run the body inline and
  memoise the outcome in the same side table.

The model is correct whenever the consumer is one of those. It starves
whenever real bytecode waits on a forked task by any other route. L12 closed
the `awaitDone` route. This is the other one.

## The failure

`java.util.concurrent.CountedCompleter` never joins anything. Its protocol is
the *reverse* direction — completion is pushed UP from the child:

```java
// regression-suite/src/RJdkForkJoin.java:150-161
public void compute() {
    if (depth == 0) { leaves.incrementAndGet(); tryComplete(); return; }
    setPendingCount(2);
    new Counter(this, leaves, completions, depth - 1).fork();
    new Counter(this, leaves, completions, depth - 1).fork();
    tryComplete();
}
```

`fork()` is `final` in `ForkJoinTask`, so the lazy Bridge intercepts it for a
`CountedCompleter` receiver too. The parent then **never touches the child
again**. What is supposed to complete the parent is the child's own
`tryComplete()`, running in the child's body — which never runs.

JDK 25's `tryComplete()` is real bytecode over a plain counter:

```java
for (int c;;) {
    if ((c = a.pending) == 0) { a.onCompletion(s); a = (s = a).completer; ... }
    else if (U.compareAndSetInt(a, PENDING, c, c - 1)) return;
}
```

so with lazily-forked children the root's `pending` goes 2 → 2 (nothing
decrements it), `onCompletion` never fires, and — this is the part that makes
it worse than L12's defect — **nothing blocks**. There is no `awaitDone`, no
park, no timeout. The tree quietly does one branch of the work and returns
normally.

`regression-suite/src/RJdkForkJoin.java` pins it at **line 175**:

```java
pool.invoke(new Counter(null, leaves, completions, 6));   // 174
check(leaves.get() == 64, "CountedCompleter leaves: " + leaves.get());   // 175
check(completions.get() == 127, "onCompletion count: " + completions.get());  // 176
```

`pool.invoke` IS intercepted, so the ROOT's `compute()` runs; both of its
children are lazily forked and never run. Expect
`AssertionError: CountedCompleter leaves: 0`.

> Line 175 is a *prediction*, not a measurement. Before L12 landed, the run
> hung earlier (line 118), so this assertion has never been reached. It was
> verified here by reading the file, not by running it.

## Why "Bridge the protocol" is the wrong cure here

The narrower fix is normally right in this codebase, and the obvious narrow
fix is to intercept `tryComplete` / `propagateCompletion` / `complete` /
`helpComplete` as Bridges, the way `join()` / `get()` / `invoke()` already
are. It does not work, for a structural reason:

**Intercepting the consumer requires there to BE a consumer.** Every one of
those methods runs on the *parent* (or on an ancestor), and every one of them
only reads and decrements a counter. None of them names, reaches, or can
reach the children. A Bridge on `tryComplete()` that wanted to drive the
children would have to enumerate the process-global `fjp_state` map for
queued-not-done entries and read a `completer` field off each raw address to
work out which ones are its own — nondeterministic in order, and a stale key
in that map would be dereferenced as a live object. That is strictly worse
than the disease.

There is also a registration-side reason. A native on
`java/util/concurrent/CountedCompleter` would need **two** out-of-file
allow-list edits to be anything other than inert:

* `native-api/src/registry.rs` lists `CountedCompleter` in the real-FJP
  **drop** class-match (`registry.rs:5610`) but NOT in
  `keep_real_forkjointask_bridge`'s own class match (`registry.rs:5564-5570`),
  so under `CRATONVM_REAL_FORKJOINPOOL` every `CountedCompleter` native is
  dropped at registration;
* `is_forkjoin_native_override`
  (`vm/src/runtime/interpreter/native_override.rs:1682-1687`) does not name
  the class either, so nothing would force the native over the real bytecode
  in `--real-jdk` mode.

The `awaitQuiescence` precedent recorded in `registry.rs:5509-5520` is that a
name present in only one of those lists is *silently* inert.

## The cure: run the body at `fork()`, gated

The work has to happen on the producer side. `ForkJoinPool.execute(
Ljava/util/concurrent/ForkJoinTask;)V` already does exactly this
(`native-builtins/src/lib.rs:9758-9781`) via
`phases_early::fjp_compute_for_submit`, which **records** the completion in
the side table so a later `join()` returns the memoised result instead of
running `compute()` a second time. The eager path is therefore not new, and
not untrusted — it is the same path an existing, shipped entry point uses.

With eager fork the `Counter` tree in `RJdkForkJoin` produces exactly the
numbers the test asserts:

* `root.compute()` sets pending=2, then `child1.fork()` runs child1's WHOLE
  subtree inline. Its `tryComplete()` sees `pending==0`, calls
  `onCompletion`, walks to the root, CASes root's pending 2→1, returns.
* `child2.fork()` likewise; root's pending 1→0.
* `root.tryComplete()` sees 0, calls `onCompletion(root)`, `completer` is
  null, done.

depth 6 → `2^6 = 64` leaves, and `onCompletion` fires once per node,
`2^7 - 1 = 127`. That is lines 175 and 176.

Running the child inside the parent's `fork()` is a legal fork-join schedule:
it is exactly what a worker that steals the child and finishes it before the
parent finishes produces. The completion protocol sees an interleaving the
real JDK can also produce.

**No out-of-file allow-list edits are needed.**
`("fork", "()Ljava/util/concurrent/ForkJoinTask;")` is already on both
`keep_real_forkjointask_bridge` and `is_forkjoin_native_override`, for all
three of `ForkJoinTask` / `RecursiveTask` / `RecursiveAction`. That asymmetry
— the eager-fork cure needs zero list edits, the Bridge cure needs two — is
the second reason to prefer it.

### The gate

`native-builtins/src/phases_late/concurrent.rs`,
`register_forkjointask_eager_fork_gate`, called from **both**
`register_new15_loom` (synthetic path) and
`register_t19_k3_forkjoinpool_common` (real-JDK essentials path), mirroring
L12. Re-registering a triple **updates the existing slot in place**
(`NativeMethodRegistry::register`'s `match prior_slot` arm), and both call
sites run after the `phases_early` `fork()` registrations on their respective
boot paths, so the re-registration wins either way round.

| `CRATONVM_FJP_EAGER_FORK` | behaviour |
| --- | --- |
| unset, `0`, or anything unrecognised | lazy fork — **today's behaviour, the default** |
| `1` / `on` / `true` / `yes` / `cc` / `counted` | eager **only** when the receiver's class transitively extends `CountedCompleter`; byte-identical lazy path otherwise |
| `all` / `always` / `2` | eager for every `ForkJoinTask` — the broad variant L12 proposed, for measuring the ordering blast radius |

In `Lazy` mode the registrar returns before touching the registry at all: the
default path is not merely equivalent to today's, it is *untouched*, slot and
`NativeKind` included. An unrecognised value also means lazy, so a typo cannot
silently change behaviour.

**How the flag is read, and whether it latches.** It is read once per registry
build — i.e. once per `Vm`, at boot — not per `fork()`, via
`cratonvm_types::flags::runtime_var_os`. The name is a **declared** flag
(added to `flag_groups::INVENTORY` as `CRATONVM_THREADS=fjp-eager-fork`),
because `tools/flag-census/check-surface.sh` fails CI on any `"CRATONVM_*"`
literal that is not declared. Declared names are served from the immutable
process-wide `VmFlags` snapshot, **which latches on its first read anywhere in
the process**. Consequences:

* Setting it in the environment before launching `cratonvm` works — this is
  the A/B below, and every arm is its own process.
* A `std::env::set_var` from inside a Rust test, executed after any VM code
  has read a flag, is **invisible**. An in-process test must use
  `flags::with_process_overrides(&[("CRATONVM_FJP_EAGER_FORK", Some("1"))], ..)`
  and construct its `Vm` inside that guard.

## Blast radius: parallel streams

`java.util.stream.AbstractTask extends CountedCompleter`, and every
parallel-stream leaf task (`ReduceOps$ReduceTask`,
`AbstractShortCircuitTask`, …) extends `AbstractTask`. So `parallelStreams()`
(`RJdkForkJoin.java:184-216`) sits on this same fault line — **if** a parallel
pipeline in CratonVM actually reaches the real JDK's `AbstractTask` at all.
That is not certain: `native-builtins/src/phases_late/streams.rs:69-81,
309-314, 378-383, 435-440` registers `parallel()` on the `Stream` /
`IntStream` / `LongStream` / `DoubleStream` **interfaces** as a no-op
returning the receiver, and if that native wins over the real
`AbstractPipeline.parallel()` bytecode then `.parallel()` never sets the
source stage's parallel bit, no `AbstractTask` is ever constructed, and the
blast radius here is zero.

Whichever way that resolves, the shape to expect if `AbstractTask` IS reached
is a **wrong answer, not a hang** — the same silent shape as the `Counter`
tree, because `AbstractTask.compute()` also ends in `task.tryComplete()`.
`ReduceOps` then does `new ReduceTask(...).invoke().get()`, and our `invoke()`
Bridge answers the `compute()` return value (null, for the void
`CountedCompleter.compute()`) rather than `getRawResult()`, so a null or an
NPE at that `.get()` is as likely as a wrong number. The A/B settles it;
`RJdkForkJoin.java:187` (`parallel sum: ...`) is the assertion that reports.

## The A/B

`javac` and `cratonvm` on the Linux build host. Four arms: {default, gate on}
× {`--real-jdk`, `--jdk-only`}. **Interleave A-B-B-A** — a straight
A,A,B,B on this shared host measures the host, not the change.

```sh
cd regression-suite
javac -d out src/RJdkForkJoin.java

# --real-jdk, A-B-B-A
timeout 300 cratonvm --real-jdk -cp out RJdkForkJoin;                            echo "A1 rc=$?"
CRATONVM_FJP_EAGER_FORK=1 timeout 300 cratonvm --real-jdk -cp out RJdkForkJoin;  echo "B1 rc=$?"
CRATONVM_FJP_EAGER_FORK=1 timeout 300 cratonvm --real-jdk -cp out RJdkForkJoin;  echo "B2 rc=$?"
timeout 300 cratonvm --real-jdk -cp out RJdkForkJoin;                            echo "A2 rc=$?"

# --jdk-only, A-B-B-A
timeout 300 cratonvm --jdk-only -cp out RJdkForkJoin;                            echo "A3 rc=$?"
CRATONVM_FJP_EAGER_FORK=1 timeout 300 cratonvm --jdk-only -cp out RJdkForkJoin;  echo "B3 rc=$?"
CRATONVM_FJP_EAGER_FORK=1 timeout 300 cratonvm --jdk-only -cp out RJdkForkJoin;  echo "B4 rc=$?"
timeout 300 cratonvm --jdk-only -cp out RJdkForkJoin;                            echo "A4 rc=$?"

# Broad variant, both modes — the ordering blast radius, not the cure.
CRATONVM_FJP_EAGER_FORK=all timeout 300 cratonvm --real-jdk -cp out RJdkForkJoin; echo "C1 rc=$?"
CRATONVM_FJP_EAGER_FORK=all timeout 300 cratonvm --jdk-only -cp out RJdkForkJoin; echo "C2 rc=$?"
```

Reference: HotSpot 25 prints `PASS RJdkForkJoin (26 checks)`.

Expected:

* **A arms** — `AssertionError: CountedCompleter leaves: 0` at line 175,
  after `CK RJdkForkJoin sum=199990000 fill=24995000` (L12's checkpoint).
* **B arms** — past line 175/176, printing
  `CK RJdkForkJoin leaves=64 completions=127`, then on to
  `parallelStreams()`, which is the next unknown.

If a B arm exits 124 (timeout) rather than failing an assertion, look at
`tryComplete`'s CAS loop first: with the children now running, the *only*
remaining real-bytecode dependency is
`U.compareAndSetInt(a, PENDING, c, c - 1)` against a real JDK object field. A
CAS that can never succeed turns that `for(;;)` into a spin. Diagnose with
`CRATONVM_DEFAULT_WATCHDOG_SEC=45` and look for a `CountedCompleter.tryComplete`
frame.

### Regression guard for the B arms

The gate changes `fork()` ordering, so the ForkJoin-touching suites must be
re-run in the eager arm before anything is claimed:
`vm/tests/fjp_recursive.rs`, `vm/tests/rfjp1_recursive.rs`,
`probes/FjpMatrixProbe.java`, `regression-suite/src/RJdkExecutors.java`, and
whatever Spring/Hibernate slice the orchestrator is already running. In the
**default (A) arm** these cannot move at all — the registrar does not run.

## What would justify making the new behaviour the default

All three, together:

1. `CRATONVM_FJP_EAGER_FORK=1` reaches `PASS RJdkForkJoin (26 checks)` in
   **both** `--real-jdk` and `--jdk-only` — i.e. it fixes lines 175/176 *and*
   does not break `parallelStreams()` or `workerException()` behind them.
2. The regression guard above is unchanged between the A and B arms — in
   particular `rfjp1_recursive` (the depth-10 `FjpProbe`) still passes, since
   "eager fork was overflowing the host stack on deeply-recursive
   RecursiveTask probes" is the recorded reason `fork()` was made lazy. Under
   `=1` that probe takes the lazy path anyway (a `RecursiveTask` is not a
   `CountedCompleter`), so a failure there would mean the receiver test is
   wrong, not that the depth story is real.
3. `=all` buys nothing that `=1` does not — no additional suite flips green.
   If `=all` is *needed* for something, that is a separate finding and the
   default should still be `=1`'s behaviour, with `all` kept as the knob.

On all three: flip the default by inverting `fjt_fork_mode`'s `None` arm to
`CountedCompleterEager` and keeping `0`/`-fjp-eager-fork` as the escape hatch.
Do not delete the knob; the escape hatch is what makes the ordering change
bisectable later.

## Confidence, and the single observation that falsifies this

Reasoned from source; nothing here was executed. The mechanism (lazy `fork()`
+ a completion protocol with no consumer ⇒ `leaves == 0`) is a direct reading
of `RJdkForkJoin.java:150-176` against the `fork()` registrations, and the
64/127 arithmetic matches the test's own constants, which is a decent
independent check.

One structural argument makes it fairly hard to be wrong about *which*
registration matters. `fork()` is `final` in `ForkJoinTask`, so an
`invokevirtual` on a `Counter` receiver resolves to `ForkJoinTask.fork` — the
same declaring class as the `join()` that L12 proved is intercepted for a
`SumTask` receiver. And the predicted A-arm symptom (`leaves: 0`) can only be
produced by the lazy native *being reached*: if the real `fork()` bytecode ran
instead, the task would go into the real `WorkQueue` of a worker-less pool and
fail some other way. So confirming the A arm also confirms the slot the B arm
replaces.

**The falsifier:** run the B arm and still see `CountedCompleter leaves: 0`.
That would mean `fork()` on a `CountedCompleter` receiver is not reaching the
`ForkJoinTask.fork` native at all — most likely because real-JDK dispatch
resolves it against a class the three re-registrations do not cover, or
because `is_forkjoin_native_override` is not consulted on the route that
`Counter.compute()` takes. In that case the defect is a dispatch-routing
problem and every word above about *which* cure to apply is beside the point.

## Bookkeeping

`scripts/baselines/jdk-only-kind-map-25-linux.tsv` is unaffected: the gate
re-registers three triples that already exist, under the same
`NativeKind::Bridge`, and only when the env var is set.
