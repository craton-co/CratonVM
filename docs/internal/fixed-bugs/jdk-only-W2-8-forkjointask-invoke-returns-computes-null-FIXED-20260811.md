> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkForkJoin` passes in the 53/1 run. This record specified a cure it could not build; the cure is in the tree — `native-builtins/src/phases_early.rs:8489-8509` implements `ForkJoinTask.invoke()` as `doInvoke(); return getRawResult();` through a virtual `getRawResult` dispatch with the `method_exists` guard and the memoisation this record specified. "The wall behind this one: `isCompletedAbnormally()`" was taken by W3-4, which is still open in `docs/known-issues/jdk-only/` with a fix in flight.
>
> Previous location: `docs/known-issues/jdk-only/W2-8-forkjointask-invoke-returns-computes-null.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `invoke()` answers `compute()`'s value, so every real parallel-stream terminal gets `null`

**Status:** cure specified 2026-08-07 (lane W2-8, JDK-only wave 2). The cure is
a **four-line arm change plus a ten-line helper** in
`native-builtins/src/phases_early.rs` — see *The cure* below. This lane could
not build or run; the *failure*, however, was measured (by the orchestrator,
fresh build, today) rather than predicted.

Predicted in advance by L19
(`L19-countedcompleter-lazy-fork-starvation.md`, "Blast radius: parallel
streams"), which wrote — before any of it was observed — that "our `invoke()`
Bridge answers the `compute()` return value (null, for the void
`CountedCompleter.compute()`) rather than `getRawResult()`". That prediction is
now confirmed by measurement. This lane is the ordering prerequisite for
migrating any stream source onto the real JDK pipeline.

## The model this sits inside

Unchanged from L12/L19. CratonVM does not run ForkJoin tasks on worker threads
(`NativeContext` is not `Send`), so `fork()` is a lazy Bridge and every
*consumer* — `join()` / `get()` / `invoke()`, the `ForkJoinPool.invoke` /
`submit` / `execute` / `lazySubmit` / `invokeAll` / `invokeAny` family, and the
three static `ForkJoinTask.invokeAll` overloads — runs the body inline and
memoises the outcome in the `fjp_state` side table.

Every one of those routes funnels through a single function,
`phases_early::fjp_compute_and_complete`. That is where the defect is, and it
is why one fix covers all of them.

## The failure (measured)

`CRATONVM_FJP_EAGER_FORK=1`, fresh build, `--jdk-only`:

```
CK RJdkForkJoin sum=199990000 fill=24995000
CK RJdkForkJoin leaves=64 completions=127        <- L19's cure works
Exception in thread "main" java/lang/NullPointerException:
    Cannot invoke "java.util.stream.Node.getChildCount()" because "node" is null
    at RJdkForkJoin.parallelStreams(RJdkForkJoin.java:192)
    at java/util/stream/ReferencePipeline.toArray(ReferencePipeline.java:658)
    ...
    at java/util/stream/Nodes.collect(Nodes.java:330)
    at java/util/stream/Nodes.flatten(Nodes.java:466)
```

Those are real JDK `java.util.stream` frames with real line numbers.
`Collection.parallelStream()` has zero native registrations anywhere in the
tree, so `src.parallelStream()` (`RJdkForkJoin.java:191`) builds a genuine
`ReferencePipeline` and runs the real `AbstractPipeline` / `Nodes` machinery,
which is backed by `java.util.stream.AbstractTask extends CountedCompleter`.

The null `Node` comes from `Nodes.collect`:

```java
Node<P_OUT> node = new CollectorTask.OfRef<>(helper, generator, spliterator).invoke();
```

### Why `invoke()` returns null

`fjp_compute_and_complete` picks the task's entry point from the receiver's
runtime class (`fjt_entry_point`), in this order: `compute()Ljava/lang/Object;`,
then `compute()V`, then `exec()Z`. Verified against JDK 25 with `javap`:

* `CountedCompleter.compute()` is `public abstract void compute();`
* `AbstractTask` overrides exactly that void `compute()`; neither
  `Nodes$CollectorTask` nor `ReduceOps$ReduceTask` overrides it or
  `getRawResult()`.

So a stream task lands on the **`ComputeVoid`** arm, and that arm was:

```rust
FjtEntry::ComputeVoid => ctx
    .invoke_virtual(live_task, "compute", "()V", &[])
    .map(|_| Value::Object(None)),
```

`compute()` is void, so its value is *always* null. For `RecursiveAction` that
is right — the class exists for tasks with no result. For a
`CountedCompleter` it is wrong: the real `ForkJoinTask.invoke()` is

```java
public final V invoke() { ...doInvoke()...; return getRawResult(); }
```

and a `CountedCompleter` leaves its value in the raw-result slot. `javap` on
`AbstractTask` shows `compute()` ending in

```
129: invokevirtual  // Method doLeaf:()Ljava/lang/Object;
132: invokevirtual  // Method setLocalResult:(Ljava/lang/Object;)V
137: invokevirtual  // Method tryComplete:()V
```

i.e. the value goes to `localResult`, which `AbstractTask.getRawResult()`
returns. We threw it away and handed back `compute()`'s null.

### Blast radius is the whole real stream stack, not one call

Disassembling `java.util.stream` shows every parallel terminal takes this exact
route — `ReduceOps$ReduceOp`, `ForEachOps$ForEachOp`, `MatchOps$MatchOp`,
`FindOps$FindOp` and `Nodes` between them contain **17** `invoke:()Ljava/lang/Object;`
call sites and **zero** calls to `quietlyInvoke` / `quietlyJoin`. So `invoke()`
is the single lever; there is no second route to fix.

## The cure: ask the receiver for its raw result, exactly as the JDK does

In `native-builtins/src/phases_early.rs`, the `ComputeVoid` arm of
`fjp_compute_and_complete` re-reads the task's pin (a `compute()` that
allocates can move it) and then calls `getRawResult()` on the receiver:

```rust
FjtEntry::ComputeVoid => match ctx.invoke_virtual(live_task, "compute", "()V", &[]) {
    Ok(_) => {
        live_task = ctx.read_native_pin(task_pin, live_task);
        Ok(fjt_raw_result(ctx, live_task))
    }
    Err(e) => Err(e),
},
```

`fjt_raw_result` **invokes the virtual method**; it does not read a field.
That is load-bearing. `getRawResult()` is declared abstract on `ForkJoinTask`
and is overridden per subclass with a different field each time —
`RecursiveTask` keeps `result`, `AbstractTask` keeps `localResult`, a user
subclass keeps whatever it likes. A by-name field read would have to guess,
and this repo has a recorded family of bugs where a by-name field read is not
descriptor-aware and silently answers `Int(0)` / null for a name it fails to
match (`memory/get-field-by-name-answers-int-zero-for-an-absent-field.md`).
Invoking the method delegates the choice to virtual dispatch, which already
resolves it correctly — see *Dispatch* below.

A missing or failing `getRawResult()` degrades to null, which is the value this
arm returned unconditionally before, so no shape can regress: synthetic-JDK
stubs that have no such method, and `RecursiveAction`, are byte-identical.

### Dispatch: which `getRawResult()` actually runs

The concern is the recorded trap that a native registered on an abstract
superclass intercepts user subclasses
(`memory/natives-on-java-util-abstract-classes-intercept-user-subclasses.md`).
It does not fire here, for a reason that is in the dispatcher:

`populate_virtual_invoke_cache` / the vtable fast path
(`vm/src/runtime/interpreter/dispatch_virtual.rs`) first check whether the
receiver class declares its own bytecode, then walk the superclass chain and
**stop at the first parent that has bytecode and no native**. For a
`Nodes$CollectorTask$OfRef` receiver the walk is
`OfRef` -> `CollectorTask` (neither declares `getRawResult`) -> `AbstractTask`,
which has bytecode and no native registration (natives exist only on
`ForkJoinTask` / `RecursiveTask` / `RecursiveAction`, and `CountedCompleter`'s
are dropped wholesale in real-JDK mode by `registry.rs:5604-5615`). So real
`AbstractTask.getRawResult()` runs and returns `localResult`.

For the other shapes:

* `RecursiveTask` subclass -> `ComputeObject` arm, untouched.
* `RecursiveAction` subclass -> the native on `RecursiveAction` returns a
  constant null, which is what the real JDK returns.
* a concrete direct `ForkJoinTask` subclass (JUnit's `ExclusiveTask`) **must**
  implement the abstract `getRawResult()`, so the receiver declares its own
  bytecode and wins outright. This is also the pre-existing `Exec` arm, which
  already called `getRawResult()` — that arm is left exactly as it was.

### Memoisation

`fjp_complete_from_outcome` stores the returned value with
`fjp_state_set_done`, so the corrected result — not a done-flag — is what a
second `join()` / `get()` / `invoke()` answers. `FjpEntry.result` is already
GC-rooted and remapped (`gc_scan_forkjoin_roots` / `gc_update_forkjoin_refs`),
so the newly-retained `Node` graph is safe under a moving collector.

## Verification

```sh
cd regression-suite
javac -d build src/RJdkForkJoin.java
CRATONVM_FJP_EAGER_FORK=1 cratonvm --java-home "$JDK" --jdk-only -cp build RJdkForkJoin
```

Expected progress: past `parallel filter size` (`:193`) and on through
`parallelStreams()`. HotSpot 25 prints `PASS RJdkForkJoin (26 checks)`.

Run the `--real-jdk` arm too, and interleave A-B-B-A per the host rule.
Regression guard, same as L19: `vm/tests/fjp_recursive.rs`,
`vm/tests/rfjp1_recursive.rs`, `probes/FjpMatrixProbe.java`,
`regression-suite/src/RJdkExecutors.java`.

**No out-of-file allow-list edits are needed.** `("invoke", "()Ljava/lang/Object;")`
is already on both `keep_real_forkjointask_bridge` (`native-api/src/registry.rs`)
and `is_forkjoin_native_override`
(`vm/src/runtime/interpreter/native_override.rs`), for all three task classes.
Nothing about registration changes; only what the shared compute helper
returns.

## The wall behind this one: `isCompletedAbnormally()`

Not this lane's defect, and it is in `workerException()` rather than
`parallelStreams()`, but it is the next assertion the same test reaches
(`RJdkForkJoin.java:258`). `ForkJoinTask.isCompletedAbnormally()` is
`(status & ABNORMAL) != 0` over the real `status` field. CratonVM records
abnormal completion in the `fjp_state` side table and **never writes the real
`status` word**, so the predicate reports `false` for a task whose body threw —
while its sibling `isCompletedNormally()` (which IS registered) correctly
reports `false` too. Both answers say "not finished".

`grep -rn isCompletedAbnormally --include=*.rs` returns nothing: the method is
registered nowhere and named in neither allow-list. Curing it is three
coordinated edits (a registration next to `isCompletedNormally` in
`phases_early.rs`, plus one entry in **each** of `registry.rs`'s
`keep_real_forkjointask_bridge` and `native_override.rs`'s
`is_forkjoin_native_override` — the `awaitQuiescence` precedent in
`registry.rs` is that a name in only one list is silently inert). It should be
its own lane, and it must land as one unit.

## Confidence, and the single observation that falsifies this

High for the diagnosis, which is not a guess: the failure was measured, the
stack is real JDK frames, the `AbstractTask` bytecode was disassembled from the
host JDK 25 rather than recalled, and L19 predicted this exact shape from the
same source months of evidence earlier. The arithmetic of the mechanism —
void `compute()` -> `Value::Object(None)` -> a null `Node` at
`Nodes.flatten` -> `node.getChildCount()` — has no free parameters.

**The falsifier:** apply the cure and still get a null / NPE at the same line.
That would mean `getRawResult()` on a stream-task receiver is *not* reaching
`AbstractTask`'s bytecode — most plausibly because the ancestor-native walk
finds the `ForkJoinTask` native first on the route these receivers take, in
which case `invoke()` would now be answering the side table's own empty result
slot instead of `compute()`'s null: the same null by a different road, and the
cure becomes "root the raw result through `setRawResult`" rather than "ask the
receiver".

A second, softer falsifier sits one line further on: `check(evens.size() == 2500)`
at `:193`. `src` is built by CratonVM's *synthetic* stream stack and then read
by the *real* `Collection.parallelStream()` -> `spliterator()`. If that list's
state is native-overlay-backed and invisible to real bytecode
(`memory/native-backed-state-is-invisible-to-real-jdk-bytecode.md`), the
pipeline will run correctly over an empty source and report `0`. That would be
a **different** defect — the stream-source migration the sibling lane owns —
and not evidence against this fix.
