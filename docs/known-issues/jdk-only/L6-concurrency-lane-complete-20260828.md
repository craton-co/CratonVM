# L6 (concurrency and threads) is CLEAN — 34 defects, and all three arms green

**Status: COMPLETE 2026-08-28.** Lane L6 of the seven in
`HANDOFF-20260828-SCOPE.md`. Worktree `/data/cvm-l6cc-20260828` on the Linux
build host, branch `claude/l6-concurrency-20260828`.

## 1. The result

| probe | rows | compat | `--jdk-only` |
| --- | ---: | --- | --- |
| `ThreadShadowSweep` | 122 | 0 diffs | 0 diffs |
| `ForkJoinShadowSweep` | 178 | 0 diffs | 0 diffs |
| `ChmShadowSweep` | 208 | 0 diffs | 0 diffs |
| `AsyncChannelSweep` | 38 | 0 diffs | 0 diffs |
| **total** | **546** | **clean** | **clean** |

Covering all **109 `native-won` triples** the report names in
`ConcurrentHashMap` (48 static rows), `Thread` (37), `ForkJoinTask` (36) and
`ForkJoinPool` (26), plus the lane's fourth assignment,
`AsynchronousFileChannel`. Every probe is stable across two runs in every mode.

**33 defects fixed.** Every one on a contract edge — argument validation, null
contracts, refusal TYPES, callback re-entrancy, lifecycle guards, identity.
Not one is a wrong answer to an ordinary `put`, `get`, `join` or `start`. That
is now six families out of six in this campaign.

The oracle is **HotSpot 25.0.4+7**, not the 25.0.3+9 the Windows lanes use:
25.0.3+9 is not on this host and 25.0.4+7 is. No row depends on the difference.

`compatibility_classes: 0` and `synthetic_stub_invocations: 0` on all four
probes — the definition-of-done predicate holds on every path this lane walks.

## 2. Two of the 33 were not wrong answers. They were a HANG and an infinite loop

Both had been invisible because the instrument could not survive them.

```text
chm.computeIfAbsent("q", k -> chm.computeIfAbsent("q", k2 -> 1))
  HotSpot  IllegalStateException: Recursive update       CratonVM  never returned
chm.elements() on a three-entry map
  HotSpot  1 2 3 then false                              CratonVM  null, forever
```

### 2.1 The reservation protocol could not recognise its own thread

`computeIfAbsent` deliberately runs its mapper with NO monitor held and marks
the key with a reservation instead — the 2026-07-21 WildFly AB-BA fix. Phase 1
of a re-entrant call finds that reservation, **cannot tell that the thread
waiting for it to clear is ITSELF**, and waits in a 50 ms loop that nothing can
ever end.

`compute` is the same gap's other polarity: it holds a REENTRANT segment
monitor, so the inner call simply succeeded and the map came back holding a
mapping the JDK refuses to make. One bug, two opposite symptoms, and the quiet
one is the more dangerous.

`chm_reject_recursive_update` keys on `(map, key hash)` per thread. The JDK's
rule is per-BIN — `hash & (n-1)` — which is COARSER, so this is strictly
narrower than the JDK: it never throws where the JDK would not. It needs no
`equals` call, which on this path would be more user code re-entering the same
map. The frames are a thread-local of `(pin handle, hash)` — a pin handle
rather than an address, because the mapper is arbitrary Java and can complete a
moving collection — popped by a `Drop` guard so `?` and panic both unwind it.

**The probe row carries a 20-second watchdog on a daemon thread, and that is
how it was measured at all.** The first CHM run died at row 105 of 208 and the
103 rows behind it were invisible; `diff` reported the missing tail as ordinary
`<` lines, which is exactly the failure mode `HANDOFF-20260828-SCOPE` §3 warns
about.

### 2.2 There may only be one producer of a carrier class

`elements()` is `rjdkenumerations-is-red-on-dev-from-the-chm-values-cursor`,
bisected there to `a0168ed03` and left as that lane's call. **The mechanism is
now identified and the fix keeps that lane's parity win rather than reverting
it.**

`a0168ed03` added `ConcurrentHashMap$ValuesView →
ConcurrentHashMap$ValueIterator` to `VALUES_ITR_CARRIERS`, which registers this
file's SNAPSHOT `hasNext`/`next` on that class. `native_chm_elements` was
independently building a REAL `ConcurrentHashMap$ValueIterator` over the
published mirror table. **A registration keys on the CLASS and cannot tell two
producers apart**, so the real cursor ran the snapshot bodies over slots nothing
had written. The doc comment three paragraphs above the new entry had already
recorded this exact failure as a measured dead end.

The fix removes the OTHER producer: `elements()` hands back the same snapshot
carrier `values().iterator()` already produces, whose class implements
`Enumeration` as well as `Iterator`. `keys()` moved with it once §4.5 made
`keySet()`'s iterator the real `ConcurrentHashMap$KeyIterator` — two producers
of THAT class would have reproduced the whole failure on the key side.
`chm_real_dual_iterator` is deleted; it had no other caller.

**And then it drained EMPTY, which is a quieter wrong answer than the one it
replaced.** `BaseIterator.hasMoreElements()` is not `return hasNext();` — it is
`return next != null;`, a DIRECT field read that never reaches the native.
`hasMoreElements`/`nextElement` are now registered on both carrier classes.

## 3. `java.lang.Thread` — 5 defects, all in both modes

| row | HotSpot | CratonVM (before) |
| --- | --- | --- |
| `new Thread((String) null)` | NPE `'name' is null` | no-throw |
| `new Thread(r, (String) null)` | NPE `'name' is null` | no-throw |
| `t.start(); t.setDaemon(true)` | `IllegalThreadStateException` | no-throw |
| `new Thread(r).interrupt()`, then `isInterrupted()` | `true` | `false` |
| `ofVirtual().unstarted(r).setDaemon(false)` | IAE `'false' not legal for virtual threads` | no-throw |

**The two constructors were two defects each.** The throw never happened, and
the null was then replaced by the VM's own `Thread-N` default — so a caller that
passed a computed null got a thread whose name is not the one it asked for and
no way to notice. `setName(null)` already had this refusal; the six name-taking
constructors are the other half of the same contract and had none. All six now
share one helper, and four of them are beyond what these probes reach.

**`setDaemon` was three missing guards and the ORDER is load-bearing.** The JDK
is `isVirtual() && !on` → IAE, then `isAlive()` → `IllegalThreadStateException`,
then the write. The is-alive guard matters most: the daemon flag is read once,
when the thread starts, so writing it afterwards was a silent no-op that told
the caller its non-daemon thread was now a daemon — and a JVM that will not exit
is exactly what that caller was avoiding.

**The interrupt row is scoped deliberately.** `native_thread_interrupt` already
mirrors the interrupt onto the Java `interrupted` field; the registered
`isInterrupted()` read only the VM thread registry, which has no entry for a
thread that has never started — so the mirror was written and never read. It
now consults the field ONLY when `thread_run_state` says NEW. OR-ing it in
unconditionally would resurrect a status `Thread.interrupted()` had just
cleared, which the row `isInterrupted after interrupted() cleared` measures and
which passes today.

## 4. `ForkJoinTask` / `ForkJoinPool` — 7 defects, 17 rows, all in both modes

### 4.1 A cancelled task raised the wrong CLASS (3 rows)

```text
task.cancel(false); task.join() / .get() / .invoke()
  HotSpot  CancellationException      CratonVM  IllegalStateException
```

`fjp_state_get_checked` raised `IllegalStateException` carrying the string
`"java.util.concurrent.CancellationException: task was cancelled"`, and its own
comment said why: `RuntimeError` had no variant for the real class. **A proxy is
exactly as good as its message and no better.** `catch (CancellationException)`
is the ONE way a caller tells a cancelled task from a failed one, and it did not
fire. `RuntimeError::CancellationException` and
`RuntimeError::RejectedExecutionException` now exist, both with the
empty-message-means-no-message convention (measured: HotSpot reaches the no-arg
constructor on all six of these paths).

### 4.2 `reinitialize()` discarded the raw result

```text
t.invoke(); t.reinitialize(); t.getRawResult()      HotSpot 10   CratonVM null
```

The real `reinitialize()` clears `aux` and all of `status` except `1<<24`; it
does NOT touch `RecursiveTask.result`, an ordinary field of the subclass. This
model keeps the raw result in a side table and was removing the whole entry. It
is now RESET in place with `result` kept. The cost is that
`fjp_queued_task_count()` — an estimate by its own javadoc — counts a
reinitialised task as pending until it runs again; a task that has been reset
genuinely is not done, and a wrong `getRawResult()` is not defensible either
way.

### 4.3 A task could not tell it was running in a pool (2 rows)

```text
pool.invoke(task calling ForkJoinTask.inForkJoinPool())   HotSpot true      CratonVM false
pool.invoke(task calling ForkJoinTask.getPool())          HotSpot non-null  CratonVM null
```

This pool runs every task INLINE on the submitting thread, so the JDK's own
answer — `Thread.currentThread() instanceof ForkJoinWorkerThread` — is false even
in the middle of `pool.invoke`. Not cosmetic: `inForkJoinPool()` is how library
code decides whether it may `fork()` or must run inline, so a permanent `false`
sends every such caller down the "I am on an ordinary thread" branch while it is
executing inside a pool.

`FJP_POOL_STACK` is a thread-local of `(pin handle, pool)` pushed by the pool's
own submission natives and read by two new registrations.

**This is where a registration is silently inert, and it cost a build.**
`native-api/src/registry.rs` DROPS any `Bridge` on the three task classes whose
triple `keep_real_forkjointask_bridge` does not name, and
`is_forkjoin_native_override` decides whether a surviving one WINS. The first
build registered both natives, added neither list entry, and measured exactly
the same `false` as before. Both lists now carry the pair, each with a comment
pointing at the other.

### 4.4 Six null arguments and three post-shutdown submissions (9 rows)

```text
pool.submit/execute/invoke(null)   HotSpot NullPointerException        CratonVM no-throw
pool.shutdown(); pool.submit(..)   HotSpot RejectedExecutionException  CratonVM no-throw
```

The second group is the one that matters. These natives run the task inline, so
"the pool is shut down" had no path to the code deciding whether to run it: a
caller that had shut its pool down and was draining it still had new work
executed, silently, on its own thread.

`fjp_reject_submission` reads the shutdown state by INVOKING the real
`isShutdown()` rather than consulting the side table — `ForkJoinPool.shutdown()`
is not registered on the real-JDK path (confirmed against
`--dump-native-registry`), so real bytecode owns `runState`. That is also why it
cannot recurse. Anything other than a definite `true` is treated as
not-shut-down, so a pool this VM cannot interrogate keeps the old behaviour
rather than refusing work it would have run.

### 4.5 `commonPool()` was not a singleton

```text
ForkJoinPool.commonPool() == ForkJoinPool.commonPool()   HotSpot true   CratonVM false
```

The registration's own comment had said so since it was written ("TODO: this
allocates a FRESH pool object on every call"). Any code keying a map, a registry
or a shutdown-once flag on the pool identity saw a different pool every time it
looked. Now cached in an `AtomicUsize`, rooted in `gc_scan_forkjoin_roots` and
remapped in `gc_update_forkjoin_refs` beside the task side table.

**Making it a singleton is what MADE the common pool's shutdown contract
askable.** The JDK specifies `commonPool().shutdown()` and `.shutdownNow()` as
no-ops, and with a fresh object per call the probe could not have told the
difference. On the singleton all six rows are HotSpot's: `isShutdown()` stays
`false` after both calls, `isTerminated()` is `false`, `shutdownNow()` returns
empty, and the pool still runs a task afterwards. `fjp_reject_submission`
exempts the common pool for the same reason.

**A caveat the next reader needs.** `isShutdown()` on the common pool is real
`ForkJoinPool` bytecode reading a `runState` field on a carrier this VM
allocated but never initialised, and it answers `false` because nothing writes
that field — not because anything decided it should. If the common-pool carrier
ever gains real pool bootstrap state, these six rows are the ones that move.

### 4.6 `pool.invoke` did not copy the task's exception — and its sibling must not

```text
pool.invoke(task throwing IllegalStateException("boom"))
  HotSpot   IllegalStateException "java.lang.IllegalStateException: boom", cause = the original
  CratonVM  the original itself, no cause
```

`ForkJoinTask` copies a failed task's throwable into a same-class instance with
the original as its cause, so a caller on another thread gets a stack trace that
reaches its own frame. This pool runs tasks inline, so there is no boundary to
cross — but the oracle is the oracle, and the question is which shapes it
answers STABLY. Measured over **eight** HotSpot runs on an idle host:

```text
pool.invoke(throwing task)         cause present   8/8   <- deterministic
task.invoke()                      no cause        8/8   <- deterministic
ForkJoinTask.invokeAll(t1, t2)     cause present   3/8   <- NOT deterministic
```

So the copy is applied to `ForkJoinPool.invoke` and nowhere else.
`ForkJoinTask.invoke()` must NOT copy and already matched. `invokeAll` is a coin
HotSpot flips — whether the failing task ran on a worker or was helped inline
decides it — so its probe row asserts the exception TYPE and nothing else.
**Pinning either face of that coin would be a probe reporting the host's load**,
and asserting it was how the first ForkJoin run came back `hs UNSTABLE`.

The copy falls back to the original exception when the class has no
`(Throwable)` constructor: reporting the failure with a poorer trace beats
reporting a different failure.

## 5. `ConcurrentHashMap` — 18 defects, 33 rows, all in both modes

### 5.1 The constructor had no argument validation at all (8 rows)

```text
new ConcurrentHashMap<>(-1) / (MIN_VALUE) / (16, 0f) / (16, -1f) / (16, NaN)
                            / (16, 0.75f, 0) / (16, 0.75f, -1) / (-1, 0.75f, 1)
  HotSpot IllegalArgumentException (NO message)   CratonVM no-throw
```

The JDK is one line and all three things that are easy to get wrong about it
were:

* it is `new IllegalArgumentException()` with **no message at all**, unlike
  `HashMap`'s, which names the offending value. Inventing one here would have
  been the eleventh fabricated string in this family (`G1-1`);
* `!(loadFactor > 0.0f)` catches **NaN** for free. Written as `loadFactor <=
  0.0f` — the shape a from-memory implementation reaches for — NaN passes, the
  threshold becomes NaN and the table never resizes;
* `concurrencyLevel <= 0` is a refusal, not a clamp. The old body did
  `(*v).max(1)`, and **clamping an argument is not validating it** — the same
  finding `Arrays.copyOfRange` produced in the first four families.

`newKeySet(int)` inherits the refusal; its `find_map(|v| Int(n) if *n > 0)` was
the clamp that hid it.

### 5.2 The null axis — the trap this campaign has now hit four times (8 rows)

`ConcurrentHashMap` refuses null keys AND null values; `HashMap`, in the same
registrar file, accepts both.

```text
containsValue(null) / contains(null) / values().contains(null)
putAll(null) / new ConcurrentHashMap<>((Map) null)
new ConcurrentHashMap<>(a map holding one null value) / (one null key)
replaceAll((k, v) -> null)  /  searchValues(1, null)
  HotSpot NullPointerException   CratonVM no-throw
```

Three are worth naming.

* **`values().contains(null)`** is `map.containsValue(o)`, which refuses. Every
  other carrier in `MAP_VIEW_CARRIERS` is backed by a map that accepts null
  values, so the shared body is right for them and wrong for exactly one. The
  override is registered INSIDE the carrier loop rather than after it, because a
  second `r.register` elsewhere only wins if it runs later and nothing in a
  registrar's source position says when it runs.
* **`replaceAll`** was calling the HashMap body per segment, where a function
  returning null is a legal way to map a key to null. The map kept its old
  values and the caller's mistake was invisible.
* **the two bulk-copy paths** (`putAll` and `<init>(Map)`) reach `putVal` once
  per entry in the JDK, so a null anywhere in the SOURCE is an NPE. They put
  through the HashMap body, so copying a map with one null value produced a CHM
  that had quietly dropped an entry.

### 5.3 Recursive update

See §2.1.

### 5.4 `elements()`

See §2.2. Four rows.

### 5.5 The two iterator carriers, and a fabrication with them (3 rows)

```text
chm.keySet().iterator().getClass()
  HotSpot     java.util.concurrent.ConcurrentHashMap$KeyIterator
  compatible  java.util.HashMap$KeyItr        <- FABRICATED
  --jdk-only  java.util.Arrays$ArrayItr       <- the refusal landing
chm.entrySet().iterator().getClass()
  HotSpot     java.util.concurrent.ConcurrentHashMap$EntryIterator
  both        java.util.HashMap$EntryIterator
```

One receiver answered two different wrong class names depending on the mode, and
the compatible-mode one was a **fabricated class in the middle of the collection
surface** — a Phase-1 row nobody had counted. Both views now mint their own
family's real class, which is safe here for the reason the `TreeMap$KeyIterator`
row states and the `Hashtable$Enumerator` attempt failed: single producer.

**This is where the second inert-edit trap of the lane was.**
`native_ksv_iterator` wrote its five snapshot fields at ABSOLUTE slot indices,
which is right for exactly one carrier: the fabricated `HashMap$KeyItr` declares
no fields of its own, so its `key_itr_base` is 0 and the distinction was
invisible. The real `ConcurrentHashMap$KeyIterator` declares ten, and every
READER in the file offsets by `key_itr_base` — so the first `next()` on a
three-element key set threw `NoSuchElementException` and `remove()` before
`next()` answered `UnsupportedOperationException` where HotSpot says
`IllegalStateException`. **Reading the mint site is not enough; the readers have
to be read with it.**

### 5.6 The key-set spliterator reported the wrong characteristics

Every synthetic spliterator in the VM shared one constant `SIZED | SUBSIZED |
ORDERED`, and for this receiver all three of those bits are wrong while the two
that matter are missing. `CONCURRENT` is how a stream learns its source can
change under it; a pipeline told `SIZED | SUBSIZED` instead is entitled to
pre-size its result array to a count that may already be stale. The carrier now
has an optional fourth slot for its own characteristics, read only when present
so the three-field shape is untouched.

### 5.7 The bulk-operation family — three registered, six answers wrong (6 rows)

```text
reduceKeys(1, (a,b) -> a+b)                      HotSpot "abc"  CratonVM null
reduceKeys(1, String::toUpperCase, min)          HotSpot "A"    CratonVM null
reduceValuesToInt(1, Integer::intValue, 0, sum)  HotSpot 6      CratonVM 0
reduceValuesToLong(..)                           HotSpot 6      CratonVM 0
reduceEntriesToInt(1, e -> 1, 0, sum)            HotSpot 3      CratonVM 0
searchValues(1, v -> v == 2 ? "found" : null)    HotSpot found  CratonVM null
```

Every method in this family walks `table` through a `Traverser`, and a
natively-backed CHM keeps its entries in a SEGMENTED layout that never populates
`table` (`W7-96-chm-table-never-populated`). Unregistered, they run real
bytecode over an empty tree and **answer the identity element** — the worst
shape a wrong answer can take here, because every one of them has a perfectly
ordinary-looking result for an empty map.

`reduceValues`, `searchKeys` and `search` were registered — the three the round-2
differential happened to ask — and answered correctly, which is why the family
read as working. **The registered half was evidence for the unregistered half.**
The registrar's own comment named the gap ("The rest of the family is still
unregistered; the list is in W7-36") and it stayed open until something asked.

The remaining ~28 methods are now registered as ONE block over four shared
walkers. One implementation note worth keeping: the accumulator lives in a
**pinned one-slot reference array**, not in a pin taken inside the walk.
`chm_bulk_walk` releases its own pin base on the way out and every pin the loop
body took sits above that base, so an accumulator pinned per iteration would be
dangling by the time the caller read it.

## 6. `AsynchronousFileChannel` — the lane's fourth assignment, 3 defects + the type

`the-roadmaps-phase-1-and-3-re-adjudicated-and-six-fixes-20260827` §6 recorded
the future's type as OPEN and asked whether it matters:

> `AsynchronousFileChannel.write` returns a `CompletableFuture` where HotSpot
> returns `sun.nio.ch.PendingFuture`. Every value agrees; the type does not.

`AsyncChannelSweep` asks the CONSEQUENCES — 38 rows over the `Future` contract,
the `CompletionHandler` form, the argument refusals and the bytes on disk. Every
value does agree, and the channel's own class was already right
(`sun.nio.ch.SimpleAsynchronousFileChannelImpl`). Three unrelated gaps sat
beside it:

```text
ch.read(readOnlyBuffer, 0)          HotSpot IllegalArgumentException  CratonVM no-throw
ch.close(); ch.read(buf, 0)         HotSpot no-throw (the FUTURE fails) CratonVM IOException
AsynchronousFileChannel.open(absent, READ)
                                    HotSpot NoSuchFileException        CratonVM IOException
```

The second is the interesting one: the JDK defers the closed-channel check into
the task it submits, so `read()` hands back a future and the
`ClosedChannelException` arrives at `get()` wrapped in an `ExecutionException`.
Raising at the call is both the wrong moment and the wrong class. The third
matters because `NoSuchFileException` extends `FileSystemException` extends
`IOException`: the bare parent satisfies every `catch (IOException)` and NONE of
the `catch (NoSuchFileException)` that tell "the file is not there" from "the
read failed".

**The type is closed too, and the result is VERIFIED rather than assumed.**
`PendingFuture` keeps its answer in `result` and its completion in a separate
`haveResult` flag, so a minted one whose fields did not resolve is a future that
blocks in `get()` FOREVER — strictly worse than a wrong class name. So the mint
is followed by an `isDone()` call on the object itself, and anything other than
a definite `true` falls back to the `CompletableFuture` this used to build. A
runtime assertion, not a static layout claim.

Worth closing rather than recording, because the gap was an OVER-capability:
`CompletableFuture` is a `CompletionStage`, so a caller could `instanceof
CompletableFuture` and hang `thenApply` off a future HotSpot never lets it reach
— code that then fails only on HotSpot, which is the wrong way round for a
compatibility VM.

## 7. What PASSED, because that is where the work is NOT

The bulk of all four families was already right, and the pattern is the one the
campaign keeps finding: **the middles are correct and the perimeters were not.**

* **`Thread`**: the whole lifecycle state machine, the interrupt-status clearing
  rules (`isInterrupted()` does not clear, `interrupted()` does, `sleep`/`join`
  throw AND clear), every `join`/`sleep` argument guard including
  `join(0, 1000000)` and `sleep(0, -1)`, `setPriority` out of range both ways,
  `holdsLock(null)`, `enumerate(null)`, the removed `stop()`, the
  uncaught-exception handler chain including the default handler, both
  `Thread.Builder` families, virtual threads, `InheritableThreadLocal` across a
  thread boundary, and `getStackTrace` shape.
* **`ForkJoinTask`**: the five-predicate completion algebra over all four states,
  `complete`/`completeExceptionally` including the
  `CancellationException`-makes-it-cancelled rule, the task tag CAS at both
  `Short` extremes, every `adapt` overload including checked-exception wrapping,
  `fork` from outside a pool, `tryUnfork`, `helpQuiesce`, all four `invokeAll`
  shapes.
* **`ForkJoinPool`**: construction validation, `getAsyncMode`, the whole shutdown
  state machine with zero, negative and null-unit arguments, `shutdownNow` with
  a blocked task, `invokeAll`/`invokeAny`, `ManagedBlocker` including the
  already-releasable case.
* **`ConcurrentHashMap`**: every ordinary read and write, `putIfAbsent`, the
  two-argument `remove`/`replace`, all of `compute`/`computeIfPresent`/`merge`'s
  null-result-removes and absent-key rules, `keySet(v)`'s addability against
  plain `keySet()`'s refusal, `Map.Entry.setValue` write-through and its null
  refusal, live-view semantics through later writes, weakly-consistent iteration
  (a write during iteration must NOT throw `ConcurrentModificationException` —
  the opposite of the HashMap family in the same file), and 800 concurrent
  writes read back one by one from four threads.
* **`AsynchronousFileChannel`**: the whole `Future` contract, both
  `CompletionHandler` forms with their attachments, `force`, `truncate`,
  `lock`/`tryLock`, EOF, and every other argument refusal.

## 8. The three arms — all green

Final run, on the fully merged tree, release binary:

| arm | result |
| --- | --- |
| `CRATONVM_ARGS=--jdk-only` | **112 passed, 0 failed** |
| `SUITE=all` | **112 passed, 0 failed** |
| `SUITE=core` | **72 passed, 0 failed** |

The baseline this lane started from was 111/1, 110/2, 72/0
(`the-roadmaps-phase-1-and-3-re-adjudicated-and-six-fixes-20260827` §6.5).
Every red it lost is recorded below or in §9.

### The two reds that were not this lane's, and how that was established

An earlier run of these arms was 109/3, 110/2 and 71/1, with `RExceptions` and
`RJdkFailure` failing in every arm. **The attribution was measured rather than
argued**: pristine `origin/dev` (`d17feaad2`) was checked out in its own
worktree, built into its own target dir, and the same three vectors run against
it.

| vector | pristine `origin/dev` at `d17feaad2` | this branch, then |
| --- | --- | --- |
| `RExceptions` | FAIL both modes | FAIL both modes — unchanged |
| `RJdkFailure` | FAIL both modes | FAIL both modes — unchanged |
| `RJdkEnumerations` | FAIL both modes | **PASS compatible**, FAIL `--jdk-only` |

`RExceptions` and `RJdkFailure` were ONE defect and not this lane's:

```text
Class.forName("[Lcom.cratonvm.absent.NoSuchClass20260812;")
  the ClassNotFoundException must name the ELEMENT, not the descriptor
  got: [Lcom.cratonvm.absent.NoSuchClass20260812;
```

**Lane L1 fixed it in `39e2ded07` while this lane was verifying**, and both
vectors are green above. The 20-minute pristine-dev build is what made that a
handoff rather than a hunt: without it the honest options were "chase a
`java.lang.Class` defect in someone else's family" or "push and hope", and the
`--jdk-only` arm would have looked two vectors worse than it was.

**One more thing that build settled.** `origin/dev` at `8f9ae7a9c` did not
COMPILE on a default (no `gpu-offload`) build — two `craton_gpu.rs` functions
had lost their `#[cfg]` and named symbols that only exist under the feature, 16
hard errors. Measured on the pristine worktree before assuming a merge artefact.
L1 fixed it independently in `43088b840`; this branch's identical fix was
resolved in its favour at the merge, because theirs also restores the doc
comment that went missing with the attribute.

**`RJdkEnumerations` improved twice.** On dev it fails in compatible mode with
`ConcurrentHashMap.elements(): hasMoreElements() never terminated` — §2.2, fixed
here. Under `--jdk-only` it then got FURTHER than it ever had and died on a
SECOND, pre-existing Phase-1 fabrication, which §9 closes:

```text
java/lang/NoClassDefFoundError: cratonvm/internal/ArrayListViewItr
  at java/util/Collections$SynchronizedCollection.iterator
  at RJdkEnumerations.sortedImages   <- iterating Properties.values()
```

`alloc_arraylist_iterator` (`native-collections/src/lib.rs`) mints
`AL_VIEW_ITR_CLASS` — a fabricated class — for any backing with no real
`modCount`, and its `try_alloc_synthetic(..)?` has **no refusal arm**: under
`--jdk-only` the refusal propagates and kills the caller, which is the Phase-1
shape the roadmap is about. `Properties.values()` is a
`Collections.synchronizedCollection` over a `Hashtable$ValueCollection`, so it
takes that path.

**This was first recorded as lane L3's, and that was wrong — see §9.**

Everything else is green: `cargo test -p cratonvm-types`; the seven
native-builtins gate tests with and without `--features management`; `--lib` for
`cratonvm-native-collections`, `cratonvm-native-builtins` and
`cratonvm-native-api`; and all four probes 0-diff twice each in both modes on
the same tree. `tools/check_markdown_links.py` reports the same six pre-existing
issues it reports on pristine dev.

## 9. The pre-existing probes, and the handoff that turned out not to be one

The scope doc gained a warning after this lane started:

> **RE-RUN YOUR FAMILY'S EXISTING PROBES ON THE FINAL BINARY, not only the ones
> you wrote.** A new probe asks the questions its author thought of.

L6 had not. Eleven probes in the tree touch these families; run against the final
binary in both modes they found **no new defect** — and one crash that the four
new sweeps could not have seen, because none of them iterates a `Properties`
values view.

```text
                                    compat   --jdk-only
MapViewBehaviourProbe   194 rows    0 diffs  DIED at row 0
ItrClassProbe            66 rows   18 rows   DIED at row 31
```

Both died on the fabrication `RJdkEnumerations` dies on:
`NoClassDefFoundError: cratonvm/internal/ArrayListViewItr`, at
`Collections$SynchronizedCollection.iterator()` — which is what
`Hashtable.values()` returns, so `Properties` reaches it too. **So the item §8
handed to lane L3 was not one vector under one flag; it was every `--jdk-only`
caller that iterates a map values view, and it took out two probes before their
first row.**

### The reason it was deferred did not survive being looked at

§8 first said the refusal needed a `SnapshotItrRoute` for a Hashtable-backed
values view and that inventing one was the `Properties` cluster's work. Reading
the code instead of the summary: the route arms in `native_snapshot_itr_remove`
are one line each, and the one this needs already exists as a function —
`native_al_remove_obj` is the view's own registered `remove(Object)` and already
propagates a removal into the SOURCE MAP for a live values view. So
`SnapshotItrRoute::ViewCollection` is that single call, and
`alloc_arraylist_iterator` gets the refusal arm its two sibling mint sites have
had since 2026-08-11.

**Before recording something as too expensive to fix, check the API you are
assuming you lack** — which is the lesson the scope doc's own §0 already carried
from L5, and which this lane then had to learn again.

One detail that is not incidental: the snapshot handed to
`real_snapshot_iterator` is an EXACT-LENGTH COPY, not the view's own backing
array. `real_snapshot_iterator` uses the array directly when the lengths match,
and this route's `remove()` shifts that same array underneath the iterator — the
cursor would skip the element after every removal. The fabricated carrier never
had that problem because its `next()` re-reads the live list each time.

### What the other nine probes said

Nothing to fix, and the three that differ are worth naming so the next reader
does not chase them:

* **`ChmVsHashMapProbe`** prints ns/op. It is a PERFORMANCE probe and its diff
  is CratonVM being slower than HotSpot, which is not a contract.
* **`CowSnapshotProbe`**'s one differing row labels itself
  `(either answer is legal)` — a weakly-consistent CHM iterator may or may not
  observe a concurrent removal. HotSpot saw 3, CratonVM 4.
* **`ChmOrderCensus`** differs on 818 of 2282 rows, all of them hash-container
  ITERATION ORDER (`keys=[0,1,]` against `keys=[1,0,]`). It exists to record that
  divergence; it is unspecified in the JDK and the campaign's own probe rules say
  never to assert it.
* `ChmTableSizeProbe` exits 1 with zero rows on **HotSpot too** — the probe needs
  arguments this harness does not pass. Not a VM result either way.

`ItrClassProbe` is worth one more line, because it is the census `a0168ed03`
verified its carrier change against. Compatible mode differs on 18 of 66
receivers where that record measured 20: the two rows that moved are this lane's
`ConcurrentHashMap` key-set and entry-set iterators. Under `--jdk-only` it is 10
— **strict mode names more of HotSpot's iterator classes than compatible mode
does**, which is the sixth place in this campaign where `--jdk-only` is the more
correct of the two.

## 10. Reproduce

> **The `probes/` tree is no longer in the working tree.** `3b2901531`
> (*"major doc consistency update before the realeas"*, 2026-08-29) removed 915
> files and 126 525 lines, the whole probe corpus among them — every probe this
> record names, and every probe the other six lane records name. The four
> sweeps are in history and restore in one command each:
>
> ```bash
> git show c8f47f9a5:probes/ThreadShadowSweep.java   > probes/ThreadShadowSweep.java
> git show c8f47f9a5:probes/ForkJoinShadowSweep.java > probes/ForkJoinShadowSweep.java
> git show c8f47f9a5:probes/ChmShadowSweep.java      > probes/ChmShadowSweep.java
> git show 2790005f4:probes/AsyncChannelSweep.java   > probes/AsyncChannelSweep.java
> git show 2790005f4:probes/ChmElemDbg.java probes/ThreadIntrDbg.java probes/L6MsgProbe.java
> ```
>
> The final verification in §8 ran the compiled classes in `probes/out` — which
> is untracked build output and survived the deletion — against the newly built
> binary. That is a measurement of the BINARY with unchanged probe bytecode, not
> a recompile, and it is stated that way rather than implied.

```bash
javac -d probes/out probes/{Thread,ForkJoin,Chm}ShadowSweep.java probes/AsyncChannelSweep.java
for P in ThreadShadowSweep ForkJoinShadowSweep ChmShadowSweep AsyncChannelSweep; do
  "$JDK/bin/java" -cp probes/out $P > hs-$P.out 2>/dev/null
  cratonvm --java-home "$JDK"            -cp probes/out $P > cc-$P.out 2>/dev/null
  cratonvm --java-home "$JDK" --jdk-only -cp probes/out $P > jo-$P.out 2>/dev/null
  diff hs-$P.out cc-$P.out; diff hs-$P.out jo-$P.out
done
```

**Check the ROW COUNT before reading any diff.** Each probe's last line is
`rows N DONE <name>`; a run that died partway produces a short file whose
missing tail `diff` reports as ordinary `<` lines. That is how the CHM hang first
read as 30 clean rows and 103 differences.

Two one-line diagnostics, both no-flags and both modes:

```bash
cratonvm --java-home "$JDK" -cp probes/out ChmElemDbg      # elements()  — §2.2
cratonvm --java-home "$JDK" -cp probes/out ThreadIntrDbg   # interrupt   — §3
```

and `probes/L6MsgProbe.java`, which is the MESSAGE of every exception this lane
started throwing, measured on the oracle before any of them was written.
