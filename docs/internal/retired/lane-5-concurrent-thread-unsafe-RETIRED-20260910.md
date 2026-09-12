# Lane 5 — `java.util.concurrent`, `Thread`, `Unsafe` — RETIRED 2026-09-10

| | |
|---|---|
| **Status** | Retired 2026-09-10. Two RESIDUAL waves on 2026-09-11: §9a took four of §10's five items, §9b took 33 more rows, answered §10.1 by measurement and corrected two of §9a's own claims. Table count is now 98 + 67 + 33. What is left is §11. |
| **Was** | `docs/known-issues/jdk-only-lanes/lane-5-concurrent-thread-unsafe.md` |
| **Table** | [`RETIRED_SHADOW_L5_TRIPLES`](../../../native-api/src/retired_shadow.rs) |
| **Ownership authority** | [`lane-0-integration-and-gates.md`](../../known-issues/jdk-only-lanes/lane-0-integration-and-gates.md) |
| **Method** | [`jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md) |

The lane page said its own §7: *every bucket-A/B row in the prefix set is
retired, classified as C/D/E/F, a reviewed `Intrinsic` with its probe, or
blocked with the blocker named — with the sub-word atomics covered at all four
byte offsets, `ScopedMemoryAccess` settled jointly with L4, and every cited
delta backed by a noise floor from the same probe.* That is what this page
records, so the page it replaces can go.

**The headline is not the retirement.** 98 rows moved; four defects were
found, and three of them were live in shipped behaviour rather than in the
retirement:

- **`Unsafe.getAndSet*` / `getAndAdd*` on an ARRAY ELEMENT was a read-then-write.**
  Not atomic, and the reason `RJdkForkJoin` reported `CountedCompleter leaves:
  128`.
- **`ExecutorService.submit(Runnable, T)` and `invokeAny` ran the task on the
  CALLING thread** and returned an already-completed future, so a real
  `ThreadPoolExecutor` never saw it.
- **`Thread.State.valueOf("NOPE")` returned an enum constant** instead of
  throwing — fixed by the retirement rather than by an edit.
- **A retired shadow disarms a real-JDK keep arm**, because the re-tag runs
  before the keep predicate reads the kind. Two rows came back out of the table
  because of it, and the mechanism is general — §4a.

---

## 1. The scope is 405 rows, and getting there is two subtractions

A prefix filter over `--dump-native-registry --explain-jdk-only` reports **516**
bucket-A/B eligible rows under `java/util/concurrent/`, `jdk/internal/misc/`,
`sun/misc/`, `java/lang/Thread*`, `jdk/internal/vm/`. Two subtractions, both
lane-0 rules rather than this lane's choice, take that to the 405 the lane page
claimed:

```text
  516   bucket A/B, owns_slot, effective kind Bridge, in the L5 prefix set
 -111   rows from a registrar whose classes span more than one lane  (lane T's)
  405   this lane's scope, over 23 classes
```

Both numbers are from a dump taken with **this lane's own binary**. A dump from
`dev`'s tip on 2026-09-10 reports 659 instead of 516, because that binary
predates the Phase 3 `ConcurrentHashMap` wave and still registers its 99 rows.
*Take the dump from the binary you are about to change.*

The 111 cross-lane rows matter more than their count suggests. Lane 0 §3:
**the unit of work for a cross-cutting registrar is the registrar, and lane T
owns it whole.** `CopyOnWriteArraySet.equals` is registered by the same
`native-collections/src/lib.rs` line as `HashSet.equals` and
`LinkedHashSet.equals` — so 21 of `CopyOnWriteArraySet`'s 22 rows are lane T's,
and this lane owns exactly one of them (`retainAll`). That single row is not
retired either; see §4.

```text
  91  jdk/internal/misc/Unsafe            11  CompletableFuture
  82  sun/misc/Unsafe                      9  PriorityBlockingQueue
  33  java/lang/Thread                     9  TimeUnit
  30  jdk/internal/misc/ScopedMemoryAccess 9  jdk/internal/misc/VM
  25  ForkJoinTask                         8  jdk/internal/vm/Continuation
  19  ForkJoinPool                         5  sun/misc/Signal
  16  CopyOnWriteArrayList                 4  AbstractExecutorService
  15  ThreadPoolExecutor                   4  jdk/internal/misc/Signal
  14  RecursiveTask                        3  jdk/internal/vm/ContinuationScope
  12  RecursiveAction                      2  Thread$State
                                           2  ScheduledThreadPoolExecutor
                                           1  Thread$FieldHolder
                                           1  CopyOnWriteArraySet
```

## 2. `getAndSetReference` on an array was not atomic, and that is the ForkJoin gap

The lane page called `RJdkForkJoin`'s `AssertionError: CountedCompleter leaves:
128` "a genuine behavioural difference" and this lane's first correctness
target. It is, and it is not in `CountedCompleter`'s pending-count protocol.

`unsafe_natives_ext.rs` splits every `Unsafe.getAndSet*` / `getAndAdd*` on the
receiver's **shape**. The field arm has always been a `compare_and_swap_field`
retry loop. The ARRAY arm was a bare `get_array_element` + `set_array_element`
pair. So the atomicity of one method depended on what you called it on, and no
field-shaped probe could see it.

`apps/probes/L5CasRace.java` asks the array arm directly — 2000 slots, 4
threads, and the only correct answer is one claim per slot:

```text
                             HotSpot   CratonVM before   after
  getAndSetReference claims    2000          2035         2000
  compareAndSetReference       2000          2000         2000   <- the control
  getAndSetInt claims          2000          2000         2000
  getAndAddInt total          80000         71698        80000
  getAndAddLong total         80000         68399        80000
  compareAndSetInt total      80000         80000        80000   <- the control
```

**Read the controls, not the reds.** `compareAndSetReference` runs on the same
array through the same offset decoder and is exact, so this is not offset
decoding and not the heap; it is the missing loop. And the two `getAndAdd`
rows lose ~10% *every* run while `getAndSetReference` lost claims only
sometimes — a probe that asked only the reference row would have called this a
flake.

**Why the corpus saw it in exactly one vector.** `ForkJoinPool$WorkQueue`
claims a queued task with `U.getAndSetReference(a, slotOffset(k), null)`, so a
doubled claim is a task EXECUTED TWICE. A divide-and-conquer SUM is idempotent
— a pool running everything twice still sums correctly — so every ordinary
ForkJoin assertion passes over it. `RJdkForkJoin.countedCompleter` is the one
assertion in the corpus that counts SIDE EFFECTS instead of reducing values.

`apps/probes/L5CountedCompleter.java` is that question asked three ways, and
the fix is scored on it with the real `ForkJoinPool` bytecode running
(`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ForkJoinPool,java/util/concurrent/ForkJoinTask`):

```text
  ccTree depth=6, 64 leaves expected
    before, 8 runs   64, 64, 76, 64, 100, 91, 128, 96
    after,  6 runs   64, 121, 64, 64, 64, 64
```

The same arm on the WHOLE `java/util/concurrent/` prefix, before the fix, read
70, 128, 81, 109, 114, 127, 128, 128 — so "sometimes correct" was already the
shape, and eight runs is the minimum that shows it. **Three passes would have
called this fixed.**

**Not fixed, improved** — and §3 says what the residue is.

## 3. The residue: a real `ForkJoinPool` claims an external submission twice

With the atomicity fix in, one shape still doubles, and it is deterministic
rather than racy. `apps/probes/L5CountedCompleter.java`'s `fanOut` reads
`rootComputes=2/1` on every run, and a reduced form isolates it to the
submission path:

```text
  arming ForkJoinPool only          main runs the task, then a worker runs it too
    pool.invoke(task)               computes=2   (HotSpot 1)
    pool.execute(task); task.join() computes=2   (HotSpot 1)
    pool.submit(task).get()         computes=2   (HotSpot 1)
    task.invoke()                   computes=1   (HotSpot 1)   <- the control
```

Both executions enter with `isDone()==false` and overlap, so they are
concurrent rather than sequential re-entry. The control identifies the
boundary: a task that never enters the pool is fine, so this is the external
submission being claimed by both the submitter's help path and a worker.

The primitives underneath are not the cause, and that is measured rather than
assumed. `apps/probes/L5CasRace.java` is exact on every row after the fix, and
`apps/probes/L5UnsafeAccess.java` asks the exact surface a `WorkQueue` runs on
— five adjacent `int` fields through `putIntOpaque` / `getIntOpaque` /
`putIntRelease` / `getIntAcquire`, `getAndBitwiseOrInt` (which is what
`ForkJoinTask.setDone` uses), `putReferenceRelease` / `getReferenceAcquire` at
four array indices and on a reference field, the slot claim itself, and every
one of them mixed with plain bytecode reads and writes so that two address
spaces would show up as a disagreement. **11 rows, byte-identical to
HotSpot.**

**So `ForkJoinPool` is held, and it holds `ForkJoinTask`, `RecursiveTask` and
`RecursiveAction` with it.** That is 51 more rows, and it is one unit rather
than four decisions: a retired task class waits on the real pool. Armed on the
task classes alone, `apps/probes/L5ExecutorSweep.java` hangs at
`ForkJoinPool.submit(...).get()` and `ForkJoinShadowSweep` at
`invokeAll(t1, t2)` — 178 rows before the arm, 92 after, and the missing tail
is what `diff` reports as 86 differing rows.

## 3a. CORRECTION, 2026-09-11: the double is a RACE, and it is not in shipped mode

§3 above says the doubling is "deterministic rather than racy" and reads it as
the JDK's own `WorkQueue` being claimed by both the submitter's help path and a
worker. **Both halves of that are wrong**, and the correction matters because
§9 put 70 rows behind it.

`apps/probes/L5FjDouble.java` scores twenty submission shapes — three task
classes × five routes, plus the commonPool and two `execute(Runnable)` rows —
and records, at every body entry, the THREAD, the nesting DEPTH and the order.
That separates the three defects `computes=2` can mean: two threads, a nested
re-entry, or two sequential routes. Five runs of each configuration on one
binary at `dev`:

```text
  run   dial on ForkJoinPool   dial on pool + 3 task classes   unarmed
   1            3                          8                      0
   2            2                          4                      0
   3            3                          5                      0
   4            2                          6                      0
   5            2                          2                      0
```

Three things follow, and none of them is in §3:

  * **Unarmed `--jdk-only` never doubles.** 0 of 20 rows, five runs out of five.
    Whatever this is, it is not in shipped behaviour — §3's four rows were taken
    with the dial ARMED on `ForkJoinPool`, which its own code block says and the
    prose then drops.
  * **It is a race.** The count varies run to run and so does the identity of
    the rows: one `pool-only` run doubled `{RecursiveTask execute+join,
    RecursiveAction execute+join, CountedCompleter execute+join, RecursiveTask
    submit+get, RecursiveAction submit+get, RecursiveAction execute+get,
    commonPool invoke}` and another doubled two of those and nothing else. A
    row that doubles is not a property of the shape.
  * **Arming the task classes with the pool makes it WORSE, not better.** §3's
    remedy — "it is one unit rather than four decisions" — predicts that arming
    the pool and the three task classes together fixes it. Measured, that arm
    doubles 2 to 8 rows where the pool alone doubles 2 to 3. The "one unit"
    reading is not supported by the only experiment that tests it.

Every doubling row reads `threads=2 maxDepth=1 CONCURRENT-two-threads`, so the
mechanism is two threads rather than re-entrancy — and the reason is one this
lane can name. `fjp_state` (the side table these natives complete tasks in) and
the JDK's own write-once `status` word are TWO sources of truth for "is this
task done", and **nothing claims a task before its body runs**: `FjpEntry` has
`done`, `cancelled`, `result` and `thrown`, and `done` is set only after
`compute()` returns. Refuse the pool's natives and the caller's inline path and
a real worker both read "not done" and both run the body.

So the 70 rows stay held, with a better-stated blocker: **not "the external
submission is claimed twice" but "the two halves of this pool disagree about
completion, and the side table has no claim state."** Adding one — or moving
the pool and task surface onto a single model — is the work, and it is a design
change rather than a missing registration.

## 4. What was retired: 98 rows over 10 classes

| class | rows | what the whole-tree arm measured |
|---|---|---|
| `CopyOnWriteArrayList` | 16 | 0 worse, 2 better, 0 truncated, 0 vacuous |
| `ThreadPoolExecutor` | 15 | 0 worse in the same arm |
| `jdk/internal/misc/ScopedMemoryAccess` | 16 | 0 worse, 2 better; 518 dial yields on L4's own buffer sweeps |
| `CompletableFuture` | 11 | 0 worse |
| `PriorityBlockingQueue` | 9 | 0 worse |
| `TimeUnit` | 9 | 0 worse |
| `Thread$State` | 2 | fixes a defect — see below |
| `Thread$FieldHolder` | 1 | 0 worse |
| `jdk/internal/misc/Unsafe` | 16 | see below — a subset, not the class |
| `jdk/internal/misc/VM` | 3 | whole-tree arm moved one probe, which timed out in BOTH arms |

Each row also carries a dispatch observed per triple, from a probe run's
`--dump-native-registry` or from one of the 132 `--jdk-only` corpus reports.
That precondition is the one that decided the size of this wave, and §5 is
about what it cost.

**`Thread$State.valueOf` is a retirement that FIXES something.**
`Thread.State.valueOf("NOPE")` returned the enum constant `NOPE` — a value that
does not exist — instead of throwing `IllegalArgumentException`. Arming
`java/lang/Thread$` alone takes `L5ExecutorSweep` from 4 diffs to 2, and the
row that closes is that one. This is §1.4 working as designed: the remedy is to
delete the shim, not to write the check into it.

**The `*Internal` twin rule, which is why 16 `ScopedMemoryAccess` rows and not
8.** Eight were dispatched (`L4ByteBufferSweep`, `L4TypedBufferSweep`). Each is
a public wrapper whose only body calls its own `@ForceInline` `…Internal` twin,
and the twin is registered too — so retiring the wrapper alone produces a
configuration nobody measured: real outer, native inner. The class-wide arm
that measured clean yielded both halves, so each retired wrapper brings its
twin. The 14 rows with no dispatch on either half stay out.

**`ScopedMemoryAccess`, settled with L4.** The lane page required this jointly,
because a liveness check split across two lanes is a liveness check nobody
owns. It is settled by measurement on **L4's own instruments**: the whole-tree
arm on `jdk/internal/misc/ScopedMemoryAccess` alone is 0 worse and 2 better,
and the dial was asked 323 times in `L4ByteBufferSweep` and 199 in
`L4TypedBufferSweep` — the two probes that drive `ByteBuffer` bulk operations
through it. The 14 aligned accessors nobody calls are held, so L4 is not handed
a half-retired session check. Note for L4: the `get*Unaligned` names here are
NOT the `Unsafe` rows of the same name — these take a `MemorySegment` base and
a real byte offset, which is a different number from an `Unsafe` slot index,
and that is why one family retires and the other cannot.

## 4a. The two rows that came back out: a retirement is mode-blind, a keep arm is not

This wave was 100 rows for most of a day. The two that came back out are the
most transferable thing on this page, because nothing in the four retirement
preconditions asks the question that removed them.

`NativeMethodRegistry::register` re-tags a retired triple `Bridge` ->
`SyntheticStub` and *then* calls `register_inner`:

```rust
if self.effective_category() == NativeKind::Bridge
    && (receiver_declared_by_no_supported_image(class_name)
        || triple_is_retired_shadow(class_name, method_name, descriptor))
{
    self.current_category = Some(NativeKind::SyntheticStub);
    self.register_inner(..);          // <- keep arms run HERE
    return;
}
```

Inside `register_inner`, real-JDK mode (`drop_real_layout_synthetic`) keeps a
small set of natives it cannot safely execute as bytecode, and every keep is
written as a predicate over the kind:

```text
  keep_real_scheduled_executor_bridge = effective_category() == Bridge && ...
  keep_real_forkjoinpool_bridge       = effective_category() == Bridge && ...
  keep_real_forkjointask_bridge       = effective_category() == Bridge && ...
```

By the time those run the answer is already `SyntheticStub`, so **a triple in a
retirement table loses its native in REAL-JDK mode as well.** That is not what
a §1.4 shadow retirement is for, and real-JDK is a mode no lane page asks you to
measure.

`ScheduledThreadPoolExecutor.<init>(I, ThreadFactory, RejectedExecutionHandler)`
and `getCorePoolSize()I` are exactly the two triples
`keep_real_scheduled_executor_bridge` names — kept, with a comment, for Spring's
`ThreadPoolTaskScheduler` anonymous subclass, because the constructor delegates
to `ThreadPoolExecutor`'s real constructor and the getter resolves the inherited
field by name. Both are bucket-A rows with a clean whole-tree arm and an
observed dispatch, so all four preconditions passed. They are still not
retirable.

**There is no flag to branch the re-tag on.** `drop_real_layout_synthetic` is
set in real-JDK mode *and* in `--jdk-only` (`vm_init.rs` sets it whenever
`!config.use_synthetic_jdk`), so "skip the re-tag in real-JDK mode" is not
expressible at the registration site. The remedy is to take the row out of the
table, which is what happened.

**What caught it was a unit test, and by luck.**
`registry::tests::real_layout_mode_drops_enumset_native_surface` has exactly one
`Bridge`-survives control, and that control happened to be the 3-arg
constructor. Had it been any other triple this would have landed. So the fix
includes the gate that asks on purpose:

```text
  registry::tests::real_layout_bridge_keeps_are_not_retired_shadows
```

It walks `RETIRED_SHADOW_TABLES` — every wave, not this one — and for each
triple registers it through `register_inner` under `Bridge`, which is the state
`register` would have been in without the re-tag, then requires real-layout mode
to drop it anyway. Anything that survives is keep-listed and must leave its
table. It reads the real code path rather than restating the predicates, so a
new keep arm is covered the day it is written.

The sweep for others was cheap and is worth recording as the method: of the ten
remaining classes in this table, `registry.rs` names only two at all —
`jdk/internal/misc/Unsafe` in a hash-test fixture and
`java/util/concurrent/ThreadPoolExecutor` in a comment. No other collision
exists in this wave.

## 5. What was held, with the blocker

307 rows. Every one has a measurement, not a judgement.

### 127 rows: the class's whole arm moves the VM AWAY from HotSpot

| class | rows | measured |
|---|---|---|
| `jdk/internal/misc/Unsafe` | 75 | 8 probes worse, 5 truncated. `L4BridgeSweep` 499 rows → 0, `L4ByteBufferSweep` 406 → 111, `L4TailSweep2` 189 → 26, `SecuritySurfaceSweep` +380 |
| `java/lang/Thread` | 33 | 9 worse, 5 truncated. `ThreadShadowSweep` 122 rows → 54, `ConcurrentStressSweep` +54 |
| `ForkJoinPool` | 19 | §3 |

`Unsafe`'s blast radius is `java.nio`, and that is the shape to expect: the
class is a memory-access API, so its users are every buffer in the image.

**Sixteen of its 91 rows are retired anyway, and the line is not a judgement
call.** An atomic or a fence that DELEGATES to an `ACC_NATIVE` primitive at the
SAME offset is retirable, because the JDK's Java body is then a loop over calls
this VM already serves:

```java
    public final int getAndAddInt(Object o, long offset, int delta) {
        int v;
        do { v = getIntVolatile(o, offset); }
        while (!weakCompareAndSetInt(o, offset, v, v + delta));
        return v;
    }
```

Every term in that is one of ours, and the offset is passed through untouched.
Anything that does ARITHMETIC on the offset is not retirable, for the reason in
§5's structural blocker. So `getAndSetReference` retires and `getAndSetByte`
cannot, though they are neighbours in the same file with the same shape — and
`ScopedMemoryAccess.getIntUnaligned` retires while `Unsafe.getIntUnaligned`
cannot, though they share a name, because one is handed a real byte offset into
a `MemorySegment` and the other a slot index.

The sixteen: `getAndAddInt`, `getAndAddLong`, `getAndSetInt`, `getAndSetLong`,
`getAndSetReference`, `getReferenceAcquire`, `putReferenceRelease`,
`putReferenceOpaque`, `putIntOpaque`, `loadFence`, `storeFence`,
`storeStoreFence`, `weakCompareAndSetInt`, `weakCompareAndSetIntPlain`,
`weakCompareAndSetLong`, `weakCompareAndSetReference`.

### 133 rows: one unit with a held class

| class | rows | why it is one unit |
|---|---|---|
| `sun/misc/Unsafe` | 82 | the legacy façade over `jdk/internal/misc/Unsafe`; retire them together or the two disagree about the same memory |
| `ForkJoinTask` | 25 | §3 — a retired task class waits on the held pool |
| `RecursiveTask` | 14 | §3 |
| `RecursiveAction` | 12 | §3 |

`sun/misc/Unsafe` is the one to read twice, because it has a second and
stronger reason. **The probe tree cannot ask about it at all**: 121 of 121
probes report the dial VACUOUS on that scope, and the 132 corpus reports reach
18 of the 82. Precondition 1 fails by measurement rather than by omission, and
a future wave has to bring a workload that uses the legacy façade before it can
say anything at all about these rows.

### 40 rows: no instrument in this tree dispatches them

| class | rows | measured |
|---|---|---|
| `jdk/internal/misc/ScopedMemoryAccess` | 14 | the aligned accessors and their `…Internal` twins; 0 dispatches in probes or corpus |
| `jdk/internal/vm/Continuation` | 8 | 0 dispatches |
| `jdk/internal/misc/VM` | 6 | 0 dispatches; the other 3 are retired |
| `sun/misc/Signal` | 5 | 0 dispatches |
| `jdk/internal/misc/Signal` | 4 | 0 dispatches |
| `jdk/internal/vm/ContinuationScope` | 3 | 0 dispatches |

### The structural blocker inside `Unsafe`'s 75

Counted in the 127 above, and recorded separately because the blocker is
different in kind: 22 of those rows cannot be retired by any future wave that
does not change what `objectFieldOffset` returns.

**`objectFieldOffset` answers a SLOT INDEX, and the JDK's sub-word atomics are
byte arithmetic.** `compareAndSetByte` and its relatives are implemented in
Java over a 4-byte CAS:

```java
    long wordOffset = offset & ~3;
    int  shift      = (int)(offset & 3) << 3;   // 24 - shift when BE
    ...  getIntVolatile(o, wordOffset) ... weakCompareAndSetInt(...)
```

Over a slot index, `offset & ~3` names a **different field**. The same applies
to the sixteen `get*Unaligned` / `put*Unaligned` rows (each of the eight names
is registered at two descriptors), which decompose a byte
range a slot index does not have.

**The lane page asked for this at all four byte positions, and here it is.**
`apps/probes/L5SubwordAtomics.java` asks `compareAndSet`, `compareAndExchange`,
`getAndAdd`, `getAndSet`, `getAndBitwiseOr`, `weakCompareAndSet` and the
volatile accessors at four ADJACENT fields and four ADJACENT array indices, for
`byte`, `boolean`, `short` and `char`, printing all four values on every row so
that wrong shift arithmetic shows up as a clobbered NEIGHBOUR rather than only
as a wrong target:

```text
  132 rows, 0 diffs against HotSpot 25.0.4+7, unarmed
```

The natives are right. It is the retirement that would be wrong — which is the
opposite of the usual finding and the reason the lane page put it first.

### 4 rows: a dead registration, a door that never opens

`AbstractExecutorService.submit` ×3 and `invokeAny` are registered on a class
that is abstract. A dispatch door asks the registry about the DECLARING class
of the resolved method, and every concrete executor in the image —
`ThreadPoolExecutor`, `ForkJoinPool` — carries its own registration of the same
names, so the abstract one is never the answer.
`apps/probes/L5ExecutorSweep.java` builds the one receiver shape that could
reach it (a direct subclass declaring only `execute`) and the rows still read
`invocations: 0`. Lane 0 §1: **deleting these is worth doing and is not a
retirement**, so they are not in the table and must not be counted as one.

### 2 rows: a real-JDK keep arm this table would have disarmed

`ScheduledThreadPoolExecutor.<init>(I, ThreadFactory, RejectedExecutionHandler)`
and `getCorePoolSize()I`. All four preconditions pass and they are still not
retirable, for a reason that belongs to the retirement MECHANISM rather than to
these rows — §4a has it, and the gate that now asks the question of every wave.

### 1 row: a partial that has evidence against it

`CopyOnWriteArraySet.retainAll` is this lane's only row on that class; the other
21 are lane T's. Retiring one row of a class whose state model the other 21
maintain is the configuration the Phase 3 note already has evidence against
("arming `ConcurrentHashMap$` alone killed `ChmShadowSweep` at row 13 of 208
where arming the whole class left it byte-identical"), and the measurement
agrees: arming `java/util/concurrent/CopyOnWrite` empties the set out —
`IoSystemSweep` reads `cow set sorted |[]|` — while arming
`CopyOnWriteArrayList` alone is 0 worse over the whole tree.

## 6. The acceptance measurement

**Two binaries from ONE revision, differing in ONE file.** Both are
`--release --features management` at `33e6d6759` on the merged tree; the control
has `origin/dev`'s `retired_shadow.rs` substituted in and nothing else changed,
so the difference between them is this lane's table and the two source fixes are
in BOTH. That is the shape a retirement's A/B has to have — binary vs binary,
not dial vs dial, and not one binary from each of two revisions.

```text
  cratonvm-l5ctl   cd1592380cbc   dev's table
  cratonvm-l5d     75524176ae18   this branch's 98
```

**First, that the retirement is not INERT**, because a wave that refuses nothing
produces a clean A/B for the wrong reason. `--jdk-only-report` carries a
`refusals` object, and one probe (`L5ExecutorSweep`) run once on each binary
reads:

```text
                                   l5ctl   l5d
  interpreter_bytecode_preferred     117     138   (+21)
  interpreter_shadow_unenforced      141     105   (-36)
```

Twenty-one dispatches move from a native to the JDK's own bytecode and
thirty-six shadow observations stop being raised, in a single probe. The table
is live.

**Whole probe tree, control against trial, arms concurrent, 123 probes:**

```text
  measured           123
  worse                1   VtHandoffProbe +10  <- the flaky row; discharged below
  better               2   L5ExecutorSweep -2, NullArgMsgProbe -2
  line-count moved     0
```

`line-count moved 0` is the load-bearing one: no probe produced a different
NUMBER of rows on the two binaries, so nothing crashed earlier, truncated, or
silently stopped. Every `L4FilesSweep`-shaped instrument artefact from the
earlier waves shows up in that column first, and this run has none.

**The one worse row is the control's flakiness, and it is measured rather than
waved away.** `VtHandoffProbe`, twelve runs interleaved between the two binaries
against the same HotSpot capture:

```text
  l5ctl   10, 0, 0, 10, 10, 14
  l5d     10, 10, 14, 10, 10, --
```

Both arms span 0 to 14 on an unchanged binary, and 16 rows every time. The A/B
caught the control on one of its zero runs. §7 already priced this probe from
the phase-2 batteries; this is the same number from a different instrument, and
it is bigger than the effect anybody could claim from it.

**Corpus, the trial binary, all three arms:**

```text
  CRATONVM_ARGS=--jdk-only    132 passed, 0 failed
  SUITE=all                   132 passed, 0 failed
  SUITE=core                   92 passed, 0 failed
```

`SUITE=core` is 92/92 here. It read 91/92 earlier in the session and that was
attributed to the host at the time — six runs, three per arm, with the CONTROL
supplying the only failure. This run agrees with the attribution rather than
resting on it.

**The gate set, and which reds are `dev`'s:**

```text
  cratonvm-types                        607/0, and doc_numeric_claims 3/4
  native-builtins --lib   (x3, alone)   4237 / 0 each time
  native-builtins registrar_drift       6/1
  native-api --lib                      367 / 0
```

Two of those are red and neither is this branch's, each checked against a
pristine `origin/dev` worktree rather than assumed:

  * `doc_numeric_claims::flag_inventory_surface_counts_are_current` — the
    hand-written flag count in `docs/config/flag-inventory.md` says 1,382 and the
    tree has 1,387. Identical failure, identical numbers, on pristine dev.
  * `registrar_drift::the_drift_baseline_has_no_stale_rows` — `register_p59_module`
    and `java/lang/Class.getModule()`. Identical on pristine dev, under both
    `--lib` and `--tests`.

**And one red that was neither dev's nor this branch's: 43 failures that the
host invented.** The first `--tests` pass reported 4194 passed / 43 failed in the
default arm, all in `jboss_module_loader::t19_h15_*`, while the SAME `--lib`
passed under `--features management` and `--features synthetic-jdk` in the same
sweep. A deterministic break does not pick one feature arm. That run was
concurrent with a 50-minute release build; run alone the same target is **4237
passed / 0 failed, three times out of three**, which is pristine dev's number
exactly. A failure that does not reproduce alone is a claim about the host, and
the cheap tell was that two of the three arms disagreed with the first.


### Re-verified after the 2026-09-11 dev merge

Everything above is the A/B that ISOLATES this lane: two binaries from one
revision, one file apart. The merge that followed brought 21 dev commits
including lane 1's wave 2 (325 more retired triples), so the landing protocol's
"release build of the MERGED tree" is a second, separate measurement, and it is
this:

```text
  cratonvm-types                        6 targets, all ok (doc_numeric_claims
                                        now 4/4 — dev fixed the flag count)
  native-api --lib                      375 / 0
  native-builtins --lib     x3 arms     4238 / 4270 / 4415, 0 failed
  the other nine --test targets x3      all ok, stub_ratchet 12/0
  regression-suite, l5e     --jdk-only  132 / 132
                            SUITE=all   132 / 132
                            SUITE=core   92 /  92
```

One red remains and it is dev's, checked against a pristine `origin/dev`
worktree under both `--lib` and `--tests`:
`registrar_drift::the_drift_baseline_has_no_stale_rows`.

**`--tests` is fail-fast across targets**, and `registrar_drift` sorts before
`registry_contracts` and `stub_ratchet`, so a red there means the two most
relevant gates in the set never ran at all. The nine were re-run by name to get
the table above. A gate set that stops at somebody else's red has not told you
about yours.

### The instrument lesson: two arms of a filesystem probe collide

`L4FilesSweep` moved by 294 in the first A/B and by −307 in the second. It is
neither. Run alone, both binaries produce 397 rows and **0 diffs** against
HotSpot, four pairs out of four. Run as concurrent arms, six pairs read:

```text
  pair   control d(hs)   trial d(hs)
   1          6              141
   2        126              397
   3        104              397
   4         84              119
   5        129              184
   6        119               17     <- the trial is BETTER here
```

Both arms fail, in both directions. The probe writes into the JDK's own temp
directory, so two of it in flight collide with each other — and setting
`TMPDIR` per arm does not fix it, because the path comes from
`java.io.tmpdir`. **A/B arms must be concurrent on this host** (ABBA read 1.9×
for a flag that costs nothing), so the remedy is to score a filesystem probe
sequentially and say so, not to sequentialise the battery.

The tell was there without the re-runs: `rc=1` on BOTH arms. A row where the
control also failed is not a row about the trial.

### The instrument lesson: a corpus arm that runs nothing exits clean

The first attempt at the three arms above produced this, in four seconds:

```text
  ### jdk-only
  ### SUITE=all
  ### SUITE=core
  CORPUSDONE
```

Three headers, no suite lines, exit 0. `regression-suite/run.sh` needs `JDK=`
as well as `CV=`, and without it stops at `ERROR: javac not found` — which the
`grep -E 'REGRESSION SUITE|COUNTS'` filter discarded, leaving a transcript that
looks like three arms with nothing to say rather than three arms that never ran.

The general form is already in this tree twice (`difftest` skipping every seed
with no `javac`; `REQUIRE_E2E` guarding the binary and not the run), and the
remedy is the same each time: **put the error pattern in the filter**, and read
the CLOCK. Three corpus arms cannot finish in four seconds.

## 7. The two probes the lane page named, and their noise floor

The lane page said `JdkOnlyPlatformProbe` and `VtHandoffProbe` are unusable and
that a delta from either is a coin flip. This session produced the cleanest
evidence of that yet, without meaning to — and then produced it a second time
from a different instrument, in §6's twelve interleaved `VtHandoffProbe` runs,
where the CONTROL binary spans 0 to 14 on six runs of unchanged code.

A whole-tree battery prints the dial's `yielded/reached` beside every row. In
three of the arms above, both probes moved **while the dial was never asked** —
`y/r = 0/0`, meaning the armed run and the base run were the same run:

```text
  arm                          probe                  delta   y/r
  ForkJoinTask,RecursiveTask   JdkOnlyPlatformProbe     +2     0/0
  ForkJoinTask,RecursiveTask   VtHandoffProbe           +4     0/0
  sun/misc/Unsafe              JdkOnlyPlatformProbe     -2     0/0
  sun/misc/Unsafe              VtHandoffProbe          +14     0/0
```

That is a noise floor of at least 14 lines on `VtHandoffProbe`, measured on one
binary with nothing armed. **No delta from either probe is cited anywhere in
this record**, and the two of them account for four of the "worse" rows in the
summaries above.

## 8. Instruments added, and what each was for

| probe | rows | what it asks that nothing else did |
|---|---|---|
| `L5CasRace` | 8 | is an `Unsafe` read-modify-write on an ARRAY ELEMENT atomic (needs `--add-exports`) |
| `L5SubwordAtomics` | 132 | the sub-word family at all four byte positions of a word (needs `--add-exports`) |
| `L5CountedCompleter` | 8 | "a forked task runs exactly once", asked three ways |
| `L5CowSweep` | 67 | `CopyOnWriteArrayList`/`Set` snapshot isolation, refusals, `addIfAbsent` |
| `L5ExecutorSweep` | 104 | the executor and task-status surface, written to DISPATCH rows nothing calls |
| `L5TpeCount` | 6 | does the POOL see the task, per submission shape |
| `L5UnsafeAccess` | 11 | the exact `Unsafe` surface a `WorkQueue` runs on (needs `--add-exports`) |
| `l5run.sh` | — | the runner for the two `--add-exports` probes the battery correctly excludes |

`L5ExecutorSweep` is the one worth copying. **84 rows in this lane's *clean*
classes had `invocations: 0` in every probe run and in all 132 corpus
reports** — not wrong, just never called. A row nothing calls cannot be
retired, and, as `L5TpeCount` then showed, cannot be known to be right either.

Writing the caller is what moved them:

```text
  class                         dispatched before -> after
  TimeUnit                          3 -> 9
  ThreadPoolExecutor                6 -> 15
  CompletableFuture                 4 -> 11
  PriorityBlockingQueue             3 -> 9
  ScheduledThreadPoolExecutor       0 -> 2
  Thread$State                      0 -> 2
```

Thirty-two rows, thirty of them in the table, and one live defect found on the
way. (The `ScheduledThreadPoolExecutor` pair is dispatched and measured like
the rest; what keeps it out of the table is §4a, not this probe.) Twenty-six more of the 84 are the `ForkJoinTask`/`RecursiveTask`/
`RecursiveAction` rows — the probe reaches those too, and §3 holds them for a
different reason. The last twenty-six are the four dead
`AbstractExecutorService` rows and twenty-two `ScopedMemoryAccess` aligned
accessors; eight of those twenty-two are in the table under the twin rule, and
the other fourteen would need a receiver shape the image does not otherwise
produce.

## 9a. What the residual wave actually did, 2026-09-11

§9's list was written the day before and four of its five items were taken the
next day. What each turned into, because three of them turned into something
other than what the item predicted:

| §9 item | outcome |
|---|---|
| 1. `ForkJoinPool`'s external submission | **re-characterised, not fixed.** §3a: a race, absent from shipped mode, and the "one unit" remedy measured worse. One real inconsistency behind it WAS fixed — see below. The 70 rows stay held. |
| 2. a `sun.misc.Unsafe` workload | **done, and it unblocked 48 rows.** See below. |
| 3. delete the four `AbstractExecutorService` registrations | **done.** |
| 4. the rest of `jdk/internal/misc/Unsafe` | untouched; still needs a dispatch per triple. |
| 5. the two `ScheduledThreadPoolExecutor` rows | untouched; still needs a real-JDK Spring measurement. |

**`ForkJoinPool.execute` had an override entry and no native.**
`is_forkjoin_native_override` has listed BOTH `execute` descriptors for a long
time; nothing registered either, and `keep_real_forkjoinpool_bridge` listed
neither. That is precisely the failure the paragraph above that keep-list warns
about — *"the two lists must agree entry-for-entry; a name present in only one
of them is silently inert"* — sitting in the file that says it. So
`pool.execute(task)` ran the concrete JDK bytecode into a real `WorkQueue`
while `join()` read the side table. `execute(ForkJoinTask)V` is now registered
on its sibling `submit`'s policy and on the keep-list.
`execute(Runnable)V` stays off both ON PURPOSE: a `Runnable` gives the caller
no task to join, so nothing can double, and real bytecode puts it on a real
worker — closer to HotSpot than this pool's borrow-the-caller model.

**`sun.misc.Unsafe`: 67 rows retired, and the blocker was a missing
instrument.** §5 held all 82 with "precondition 1 fails by measurement" — 121
probes reported the dial vacuous on that scope. §9.2 said what that was worth:
*until a workload exists, the count is not evidence of anything.*
`apps/probes/L5SunMiscUnsafe.java` is the workload, and it changed the verdict:

```text
  1. the dial was asked              y/r = 76/76 on the workload
  2. whole probe tree no worse       134 measured: 0 toward, 1 away and that
                                     row is y/r=0/0 VACUOUS with a line count
                                     that moved -- not the dial
  3. the image target carries Code   outcome=bytecode-won on all 67, and
                                     `javap -p sun.misc.Unsafe` reports ZERO
                                     native methods on the class -- all 99
                                     carry Code and delegate to
                                     `theInternalUnsafe`
  4. a per-triple dispatch observed  67 distinct triples, one row each
```

It took two sittings, and the second one is the page taking its own advice.
The first workload reached 48 triples; §9a said the way to the rest is *a probe
edit, not a build*, and adding the volatile twins, the long atomics and the
bulk-memory trio reached 19 more. Armed, the widened probe reads
`d(base,armed) = 0` on 50 rows -- arming the prefix changes nothing about its
output at all.

The probe is **byte-identical armed and unarmed**, 36 rows. A wave that refuses
48 natives and changes no answer is the definition of a shadow.

It also settles a fact the page assumed the other way: **`sun.misc.Unsafe` is
fully functional on JDK 25.** Every accessor and volatile twin, all three
`compareAndSwap*`, `getAndAdd`/`getAndSet`, the static-field pair,
`allocateMemory`/`setMemory`/`freeMemory`, `allocateInstance`,
`getLoadAverage`, `park`/`unpark` and `throwException` answer on both VMs.
Terminal deprecation says nothing about whether a method works today, and the
one row where the VMs disagree is `getUnsafe` — which JDK 25 does not declare
at all.

Two of the 82 were recorded here as **deletions rather than retirements**:
`ensureClassInitialized` and `shouldBeInitialized`, "which `javap -p
sun.misc.Unsafe` declares on none of the 17, 21 or 25 images". **That sentence
is wrong — see §9b.** Thirty-two more were simply not reached by this workload
and stay out, because precondition 4 is per-triple however obvious a sibling
looks. Widening the probe takes them, and that is a probe edit rather than a
build.

**A third row was recorded as a deletion and it was wrong.** `getUnsafe` also
answered `NoSuchMethodException`, and this page first read that as absence. It
is declared — `public static`, on all three images — and the exception was the
JDK's core-reflection METHOD FILTER hiding it, which is the door that stops a
library acquiring `Unsafe` reflectively. CratonVM's `getUnsafe` native is
correct (`SecurityException` off the boot path, measured); what diverges is
that this VM implements no member filter at all, so nine of ten filtered JDK
members are reflectively visible here and `java.lang.ClassLoader` reports 18
fields where HotSpot reports 0. That is cross-cutting rather than this lane's
and has its own page,
`docs/known-issues/jdk-only/core-reflection-has-no-member-filter-20260911.md`,
with `apps/probes/ReflectMemberFilter.java` as its instrument.

The lesson outlives the row: **the image is not the authority on what
reflection answers.** A census built from class files cannot see that defect,
and `javap` is what caught the wrong claim.

**The instrument lesson.** §5's blocker for this class was true and useless at
the same time: "no probe reaches it" is a statement about the probe tree, not
about the rows. The battery agrees — of 134 probes, `L5SunMiscUnsafe` is the
ONLY one that asks this dial, and the other 133 still read `VACUOUS`. A
vacuous scope is a request for a workload, not a verdict, and this page said so
itself before anybody acted on it.

### The residual wave's acceptance

Two binaries from one revision, differing in the residual commits: `r0` at
`dev` (`8f1666414`) and `r2` at the branch tip.

```text
  not inert       r2 raises 87 sun/misc/Unsafe refusals unarmed, r0 raises 0.
                  87 is the kind-map's row count for 67 triples -- the third
                  independent route to the same number, after the table and
                  the stub ratchet.
  the workload    d(r0,r2) = 0 on 50 rows. Identical output.
  probe tree      135 measured, arms concurrent: 1 worse, 1 better,
                  1 line-count moved
  corpus on r2    --jdk-only 132/132, SUITE=all 132/132, SUITE=core 92/92
  gates at tip    native-api --lib 377/0; stub_ratchet and registry_contracts
                  green in all three feature arms; cratonvm-types green
```

**Both moved rows are instruments, and both were re-measured rather than
argued.**

`L4FileSweep` read `ctl=488 trial=421, d(hs,trial)=83` with the line count
moved — which is the collision this page already documents for the filesystem
family, down to the tell. Run ALONE it is **488 rows and 0 diffs on both
binaries, three times out of three.** The earlier `r0`-vs-`r1` pass of the same
tree had it clean, so the collision is intermittent, which is why the remedy is
to score this family sequentially rather than to trust either reading.

`VtHandoffProbe` read `-10`, and §7 prices that probe's noise floor at 14 lines
on an unchanged binary. It is not claimed as an improvement.

The `--jdk-only` corpus arm also read 131/132 once, with a `HARNESS ERROR [G3]`
on `RArrayStoreLibrary` — a vector that published no check count, which is a
harness-level complaint rather than a behaviour diff. Re-run it is **132/132 on
r2 AND on r0**, so it reproduces on neither binary.

## 9b. The second residual wave, 2026-09-11 — and two things §9a got wrong

§9a closed with a five-item list and the claim that four of its five were
taken. This section is the second sitting: **33 more rows retired, §10.1
answered by measurement rather than by hypothesis, and two factual corrections
to §9a itself.** Both corrections are the same species of error, and it is the
species this lane has now made three times.

### 9b.1 `ensureClassInitialized` and `shouldBeInitialized` are NOT deletions

§9a says, of those two `sun.misc.Unsafe` triples:

> which `javap -p sun.misc.Unsafe` declares on none of the 17, 21 or 25 images

That sentence is false, and the measurement is one command per image:

```text
  17   public boolean shouldBeInitialized(java.lang.Class<?>);
       public void ensureClassInitialized(java.lang.Class<?>);
  21   both, identically
  25   neither — removed
```

They are declared, `public`, on two of the three supported images. So they are
not deletions: **deleting them would take the registration away from 17 and
21.** What they are is a VERSION BOUNDARY — live on 17 and 21, gone on 25 — and
this lane's whole workload runs on 25, where nothing can dispatch them.
Precondition 4 wants a dispatch observed by the citing instrument, and on this
image there can be none. So the rows stay out of the table for a third reason
rather than the first one, and the test that guards them says so.

**Three times now, the same mistake**: `getUnsafe` was misfiled from a
`NoSuchMethodException` (§9a), and these two from a `javap` run on ONE image.
The rule that covers both:

> "Declared by no supported image" is a claim about THREE images, and
> reflection is not one of them.

The L1 `Unsafe` page got this right and is worth copying. Its §4.6 is titled
*"nine registrations for methods **this JDK image** does not declare"* and
prints `On 25.0.4+7:` above the list. It scoped the claim to what it measured;
§9a read that list and dropped the scope.

### 9b.2 A three-image census, and what it found instead

The correction was cheap, so the question got asked properly — every registered
`Unsafe` triple against all three images:

```text
  image                    sun.misc.Unsafe          jdk.internal.misc.Unsafe
  17  (17.0.20.1+1)        74 declared, 0 native    367 declared, 70 native
  21  (21.0.12+8)          74 declared, 0 native    367 declared, 68 native
  25  (25.0.4+7)           83 declared, 0 native    349 declared, 68 native
```

`sun.misc.Unsafe` GAINING nine methods between 21 and 25 is not a transcription
error: 25 removes `ensureClassInitialized` and `shouldBeInitialized` and adds
eleven private ones for the terminal-deprecation warning itself
(`beforeMemoryAccess`, `isMemoryAccessWarned`, `singleLineWarning`, three
lambdas, and the rest). The class is being wrapped in a warning, not emptied.

Two more facts fall straight out and neither was on this page.

**`sun.misc.Unsafe` has NO native methods on any of the three images.** Every
one of the 83 carries `Code` and delegates to `theInternalUnsafe`, so
precondition 3 is satisfied by construction for that whole class — which is why
67 + 13 of its rows retire cleanly and why the class is nearly finished.

**`jdk.internal.misc.Unsafe` is 68 `ACC_NATIVE` methods and ~280 Java ones.**
Contract §1.5 keeps every one of the 68 a `Bridge` permanently, so a fifth of
that class is not a retirement backlog at all — it is the floor. §10.4 treated
the class as one undifferentiated remainder; it is two populations, and `javap`
separates them in one command.

### 9b.3 What the wave retired: 33 triples, two instruments

`RETIRED_SHADOW_L5S_TRIPLES`.

**13 `sun/misc/Unsafe` address-form accessors.** §9a said the way to the rows
its workload missed was *"a probe edit rather than a build"*, and it was:
`apps/probes/L5SunMiscUnsafe.java` grew an `addrRow` helper and seven cases that
allocate, round-trip one type through the `(long)` accessor pair, and free.

```text
  1. the dial was asked          reached 13, yielded 13, declined_no_bytecode 0
  2. whole probe tree no worse   see the acceptance section
  3. the image target has Code   declared=true, acc_native=false, has_code=true
                                 on all 13, from --explain-jdk-only
  4. a dispatch per triple       outcome=bytecode-won on all 13, one row each
```

13 dispatches for 14 calls is not a miscount: `getByte(J)B` was retired in the
first wave, so it is no longer registered and the dial cannot reach it.

**20 `jdk/internal/misc/Unsafe` delegating accessors**, dispatched by
`probes/UnsafeShadowSweep.java` — L1's 457-row harness, which this lane did not
have to write. These are §10.4's list exactly.

### 9b.4 `invocations` cannot answer precondition 4 on this surface

Precondition 4 is written as *`invocations > 0` in that instrument's own run*,
and that field is **not readable here**. The registry dump taken from the
probe's own run reports, for every one of the 13 address rows:

```text
  invocations: 0    invocations_complete: false
  slots_with_incomplete_invocations: 45
```

A zero from a counter that says it did not finish counting is a fact about the
counter. The evidence used instead is the `--jdk-only-report`'s per-triple
`outcome`, which comes from the dispatch itself — and it needs
**`--explain-jdk-only`**, without which every `outcome` is `null` and the whole
set reads as "nothing dispatched anywhere".

### 9b.5 The class-wide dial is not a verdict on 20 triples

Arming `jdk/internal/misc/Unsafe` whole takes `UnsafeShadowSweep` from **472
rows and rc=0 to 292 rows and rc=1**, and `d(hs,armed)` from 12 to 192. §10.4
predicted exactly this. It is a statement about the CLASS, not about these 20
triples: the same armed run yields **46** distinct triples to bytecode, and the
26 this wave does not take are what breaks it. Three families, each excluded for
a reason that does not expire with more probe rows:

  * **every `*Unaligned` row** — they compute a byte offset from the offset they
    are handed, and this VM hands them a slot index;
  * **the sub-word atomics** (`compareAndExchange{Byte,Short}`,
    `compareAndSet{Byte,Short}`, `getAndAdd{Byte,Short}`) — same cause, the JDK
    masks within an enclosing word at a computed byte offset;
  * **the four that ARE the numbering** — `objectFieldOffset`,
    `staticFieldOffset`, `staticFieldBase`, `arrayIndexScale` — plus `getUnsafe`
    and `ensureClassInitialized`.

`weakCompareAndSetIntPlain` is out and its seven siblings are in. The sweep
never dispatched it, and a row whose only evidence is that its siblings passed
is what precondition 4 exists to refuse.

### 9b.6 §10.1 answered: 98 of the 99 ForkJoin keeps cannot be retired at all

§10.1 called `ForkJoinPool`'s external submission "the last thing standing
between this lane and its largest family" and put 70 rows behind it. §3a
re-characterised the defect. This closes the item, and the answer turns out not
to be about the defect at all.

`real_forkjoinpool` is **ON by default**
(`!present("CRATONVM_SYNTHETIC_FORKJOINPOOL")`), in every mode, so the only
ForkJoin natives that exist are the ones `keep_real_forkjoinpool_bridge` and
`keep_real_forkjointask_bridge` name. Both predicates open with
`self.effective_category() == NativeKind::Bridge` — and the retirement re-tag in
`register` has already turned a retired triple into a `SyntheticStub` before
they are consulted. §4a found that mechanism on two `ScheduledThreadPoolExecutor`
rows. Measured over the keep lists themselves — one registration per triple into
a fresh registry under each kind, which is what
`real_layout_bridge_keeps_are_not_retired_shadows` does to the tables:

```text
  keep-listed ForkJoin triples          99
  kept as Bridge, DROPPED as stub       98     <- unretirable BY THE TABLE
  kept as both                           1     ForkJoinPool.execute(FJT)V
  dropped as both                        0
```

So 98 of them are not "held pending a fix" — they are **outside what a
retirement table can express**. Putting one in a table does not make it yield to
bytecode in `--jdk-only`; it deletes the native in every mode, compatible
included, which is a behaviour change no retirement is allowed to make.

The one exception is
`ForkJoinPool.execute(Ljava/util/concurrent/ForkJoinTask;)V`, because the drop
arm reads `method_name != "execute"`. It is retirable by the table and **should
not be retired**, for the unrelated reason §9a records: it was registered the
previous day precisely because, unregistered, it queued into a real `WorkQueue`
while `join()` read the side table.

**So §10.1 is not a retirement item and never was.** The work is to move the
pool and task surface off the `fjp_state` side table and delete the keep arms
with it — one subsystem change, after which there is nothing left to retire
because there is nothing left registered. Sizing it as "70 rows this lane could
take" was wrong by construction, and no amount of probing the double would have
found that out: it is a property of `register`, not of `ForkJoinPool`.

### 9b.7 The second residual wave's acceptance

Two binaries from one revision, `cv-base` at `dev` (`e240573a8`) and `cv-l5s`
with the table and the probe edit.

```text
  not inert       cv-base registers all 33 of the triples; cv-l5s registers 0
                  of them, 9240 -> 9207 strict registrations. The sweep's
                  bridge_invocations fall 9853 -> 9711: 142 dispatches that
                  were a native are now the JDK's own bytecode.
  the workloads   d(base,l5s) = 0 on L5SunMiscUnsafe (57 rows) and 0 on
                  UnsafeShadowSweep (472 rows). d(hs,*) unchanged at 2 and 12 —
                  the 2 is the `getUnsafe` member-filter row, the 12 are L1's
                  adjudicated residuals.
  probe tree      154 measured, 0 worse, 0 better, 10 moved
  stub ratchet    stubs +33 in both measured arms; TOTAL FLAT
```

**The probe tree was run with a CONTROL arm, and that is the only reason the
result reads as it does.** Three CratonVM runs per probe — base, base again, and
trial — so "A and B differ" can be read against "A and A differ":

```text
  probe                     d(hs,A)  d(hs,B)  MOVED(A,B)  NOISE(A,A)
  FilePathSweep                  60       60          60          60
  HibfixVarHandleProbe           16       16          16          16
  SbLayoutBench                  12       12          12          12
  VhCasProbe                      8        8           8           8
  RandomBench / ScannerBench      6        6           6           6
  L4AbsPath                      10       10           8          10
  FilesSweep                      2        2           2           2
  SystemRuntimeObjectSweep       12       12           2           2
```

Every moved row moves by the same amount when the base binary is diffed against
ITSELF. Without that column these are ten regressions to explain, and a
plausible story is available for most of them. The first pass of this battery
had no control and reported `L5FjDouble` as BETTER by 2; the controlled run does
not list it at all.

**The one WORSE row was `VtHandoffProbe`, 10 -> 14, and it was re-measured
rather than argued.** Five runs of each binary against the same oracle:

```text
  run       1     2     3     4     5
  base     10    10     0    14    14
  l5s      10    10     0    10    10
```

**The base binary produces 14 twice on its own, and 0 once.** The probe's range
is {0, 10, 14} — the trial binary never left 10 except for the same zero — so
the battery's single WORSE row is this probe's own spread, which §7 already
prices at 14 lines. Nothing is claimed in either direction.

**The stub ratchet was moved with the PAIRED ratchet, not arithmetic.**
`CRATONVM_UNRETIRE_NATIVE_SHADOW` set to the 33 triples gives the before-number
on the very same tree:

```text
  arm                     stubs before -> after     total before -> after
  no-management               2728 -> 2761             13622 -> 13622
  management                  2755 -> 2788             13990 -> 13990
  synthetic-jdk               2728 -> 2761             13657 -> 13657
```

Total FLAT and stubs up by exactly the wave — the ratchet's own first case,
*"existing fakes were relabelled (welcome; re-freeze with the list)"*. The list
is 33 `@@STUB` lines, added, with none removed.

**Then `dev` moved and it was measured again.** Lane 4's wave 2 landed +137 on
the same three constants while this one was in flight, so the pair was retaken
on the merge: `2865 -> 2898`, `2892 -> 2925`, `2865 -> 2898`, totals unchanged.
+33 and flat both times. A delta is a property of the branch only if it survives
the tree moving under it, which is why lane 4 re-measured theirs too.

**The paired run also found the TOTALS 13 low — a re-occurrence, not a
discovery.** `MEASURED_TOTAL_REGISTRATIONS_*` is **ungated**: nothing asserts
it, and its only reader is the stub test's failure message, which uses it to
classify WHY the stub count moved. Each read 13 below the tree it was measured
on (13609 vs 13622, 13977 vs 13990, 13644 vs 13657). The constants' own doc
comment already records the same drift twice — *"REFRESHED 2026-08-24 ... both
were stale by 13 in BOTH arms ... the same equal-in-both-arms drift this doc
comment already records twice"* — so this is the fourth time, with the same
consequence each time: a wave that trusts the constant reads "total UP by 13,
stubs up by 33" and classifies a clean relabel as *"new fakes were registered —
the regression this gate exists for"*. Re-frozen to the measured values. It is
the `STRICT_MIN_TOTAL_REGISTRATIONS` lesson from the other side, and lane 4
replaced that constant with `STRICT_UNEXPLAINED_DROP_MAX` the same day for the
same reason: an absolute number nothing enforces is not evidence.

## 10. What the next wave should do, in order (as written 2026-09-10; see §9a for what happened)

1. **`ForkJoinPool`'s external submission.** §3 localises it to the submitter's
   help path racing a worker, with the `Unsafe` primitives underneath proven
   exact. It unblocks 70 rows (`ForkJoinPool` + the three task classes) and it
   is the last thing standing between this lane and its largest family.
2. **A `sun.misc.Unsafe` workload.** 82 rows that no instrument in the tree
   reaches. Until one exists, that count is not evidence of anything.
3. **Delete the four `AbstractExecutorService` registrations** — dead weight,
   not a shadow, and not a retirement.
4. **The rest of `jdk/internal/misc/Unsafe`, per triple rather than per class.**
   Sixteen delegating atomics and fences are already retired (§4); the rule
   that admitted them — delegates to an `ACC_NATIVE` primitive at the SAME
   offset — admits more rows that no instrument here dispatches:
   `compareAndExchange{Int,Long,Reference}{Acquire,Release}`,
   `weakCompareAndSet*{Acquire,Release,Plain}`, `getIntAcquire`,
   `getIntOpaque`, `getLongAcquire`, `getReferenceOpaque`, `putIntRelease`,
   `putLongRelease`. Each needs a dispatch first, which means a probe, not a
   build. What DOES need a build per iteration is anything in the
   memory-address family, because the dial arms a class and the table is per
   triple, and the class-wide arm is where `L4BridgeSweep` goes from 499 rows
   to zero.
5. **The two `ScheduledThreadPoolExecutor` rows, if and only if real-JDK mode
   is measured.** §4a took them out because the re-tag disarms
   `keep_real_scheduled_executor_bridge` and this lane measured `--jdk-only`
   only. They are not permanently unretirable — they are unretirable *by the
   table*. Retiring them means establishing, in REAL-JDK mode, that Spring's
   `ThreadPoolTaskScheduler` anonymous subclass still constructs without the
   bridge, and then deleting the keep arm and the table entry together. That is
   a Spring-corpus measurement, not a probe-tree one, so it belongs to whoever
   owns that corpus rather than to this lane.
   `registry::tests::real_layout_bridge_keeps_are_not_retired_shadows` will go
   red the moment someone adds the rows without the other half, which is the
   point of it.

## 11. What is left, after the second residual wave (2026-09-11)

§10 is kept above as written, because three of its five items turned out to be
mis-sized rather than merely undone and the record of how is worth more than a
tidy list. This is what a next wave would actually find.

**1. `sun/misc/Unsafe` is finished except for six rows, and none of the six is a
retirement.** 67 + 13 = 80 of its registrations are retired. What is left:

```text
  <clinit>()V                     a class initialiser, not a shadow
  getUnsafe()Lsun/misc/Unsafe;    correct native; the divergence is the
                                  core-reflection member filter, another lane's
  ensureClassInitialized, shouldBeInitialized
                                  declared on 17 and 21, removed on 25 — §9b.1.
                                  Retirable only from a 17 or 21 run
  defineClass, acquireFence, releaseFence, compareAndExchangeObject,
  weakCompareAndExchangeObject    declared by NO supported image — deletions
```

**2. The deletion worklist is 25 triples and it needs a census, not a table.**
Registrations whose method is declared by none of 17, 21 and 25 — five on
`sun/misc/Unsafe`, twenty on `jdk/internal/misc/Unsafe`, mostly the
`weakCompareAndExchange*` family that the JDK renamed to `weakCompareAndSet*`.
They shadow nothing and no caller on any supported image can name them, which is
lane 0's bucket F. **Check the source before counting rows here**: several
neighbours in the same position (`getReferencePlain`, `putReferencePlain`,
`monitorEnter`, `monitorExit`, `defineAnonymousClass`) were deleted on
2026-08-29 and `scripts/baselines/jdk-only-kind-map-25-linux.tsv` still carries
rows for them, because that baseline is amended by hand and was never
re-measured after the deletion. A worklist derived from the baseline alone will
be padded with work already done.

**3. `jdk/internal/misc/Unsafe`, per triple, minus the floor.** 68 of its
methods are `ACC_NATIVE` on the image and stay `Bridge` forever (§1.5). Of the
rest, 20 are now retired and 26 more were dispatched-and-yielded in the armed
sweep but belong to the three families §9b.5 names as permanently blocked. The
remainder needs a dispatch each, and `probes/UnsafeShadowSweep.java` is the
instrument — widening it is a probe edit, not a build.

**4. `ForkJoinPool` is a subsystem change, not a wave.** §9b.6: 98 of the 99
keep-listed triples cannot be expressed in a retirement table at all, because
the keep predicate reads the kind and the re-tag has already changed it. The
work is to move the pool and task surface off the `fjp_state` side table and
delete the keep arms with it; there is no intermediate step that retires rows
one at a time. Whoever takes it should read §3a first — the double is a race
between two completion models, it is absent from shipped `--jdk-only`, and
arming the task classes with the pool measured worse.

**5. The two `ScheduledThreadPoolExecutor` rows** — unchanged from §10.5, and
now visibly one instance of the §9b.6 mechanism rather than a curiosity. They
need a real-JDK Spring measurement and then the keep arm and the table entry
deleted together, which is that corpus owner's call rather than this lane's.

**Not this lane's, and open:** the core-reflection member filter,
`docs/known-issues/jdk-only/core-reflection-has-no-member-filter-20260911.md`.
