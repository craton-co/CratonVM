# Lane 5 — `java.util.concurrent`, `Thread`, `Unsafe` — RETIRED 2026-09-10

| | |
|---|---|
| **Status** | Retired. Every bucket-A/B row in the lane's prefix set is retired, classified, or blocked with the blocker named and measured. |
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

Three binaries, all from this branch, so the arithmetic is about the change and
not about a revision:

```text
  a   the Unsafe atomicity fix, no table
  b   + the executor fix, + 81 table rows
  c   + 19 more table rows (the delegating Unsafe atomics/fences, three VM rows)
```

**The refusal census first, because a wave can be a no-op that looks clean.**
A table entry retires a triple only when nothing already owns it —
`JdkOnlyViolation::SyntheticNativeRegistered` carries a `survivor`, and a
non-null one means an earlier registration is still serving and every probe
reads exactly as it did before. On binary c:

```text
  105 refusals, 0 with a survivor
```

105 rather than 100 because five triples are registered more than once
(`CopyOnWriteArrayList` 19 refusals for 16 rows, `ThreadPoolExecutor` 16 for
15, `jdk/internal/misc/Unsafe` 17 for 16).

**Whole probe tree, a against c, arms concurrent, 123 probes:**

```text
  worse   0
  better  5   L5ExecutorSweep -4, L5TpeCount -4, NullArgMsgProbe -2
              L4FilesSweep -307   <- an instrument artefact, see below
              VtHandoffProbe -4   <- the known-flaky row, not claimed
```

**Corpus, binary c:**

```text
  CRATONVM_ARGS=--jdk-only    132 passed, 0 failed
  SUITE=all                   132 passed, 0 failed
  SUITE=core                   91 passed, 1 failed  (RSocketChannelInterrupt)
```

**That one failure is the host, and it is attributed rather than assumed.**
`SUITE=core` run three more times on the CONTROL binary and three more on the
trial:

```text
  control  91/92 (failed: RBlockingQueue)   92/92   92/92
  trial    92/92                            92/92   92/92
```

The trial is 92/92 three times out of three, and the run that failed is the
CONTROL's, on a different vector. `RSocketChannelInterrupt` is a
socket-plus-interrupt vector and `RBlockingQueue` a concurrency one; which arm
fails moves with the host's load, not with the binary. **Host load flips
pass/fail, not only timings** — and the cheap tell, before any of these six
runs, was that the control failed at all.

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

## 7. The two probes the lane page named, and their noise floor

The lane page said `JdkOnlyPlatformProbe` and `VtHandoffProbe` are unusable and
that a delta from either is a coin flip. This session produced the cleanest
evidence of that yet, without meaning to.

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

## 9. What the next wave should do, in order

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
