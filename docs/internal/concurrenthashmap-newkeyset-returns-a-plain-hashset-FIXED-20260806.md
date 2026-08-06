# FIXED — `ConcurrentHashMap.newKeySet()` returned a plain `HashSet`, so `ExecutorService.close()` hung forever

| | |
|---|---|
| **Status** | ✅ FIXED 2026-08-06 — `newKeySet()` returns a real `ConcurrentHashMap$KeySetView` over a real native-backed `ConcurrentHashMap` |
| **Severity** | was high — an unbounded hang, reachable from `try (var es = Executors.new…())` |
| **Modes** | was BOTH; fixed and verified in `--real-jdk`, `--jdk-only` and `--synthetic-jdk` |
| **Opened** | 2026-08-05, by `probes/JdkOnlyPlatformProbe`'s `vthreads` section (L8, criterion 6) |
| **Fixed in** | `native-collections/src/lib.rs` (the `KSV_CLASS` section), one dispatch-gate entry in `vm/src/runtime/interpreter/native_override.rs`, three synthetic-stub table entries in `classloading/src/class_manager.rs`, and the `newSetFromMap` registration in `native-builtins/src/reflect_annotations.rs` |
| **Regression test** | `regression-suite/src/RChmKeySetView.java`, in the suite's default `CORE_CLASSES` |

## The symptom, from the top

`JdkOnlyPlatformProbe`'s `vthreads` section ran to completion on HotSpot 25 and
was `SIGKILL`ed at the 300-second bound on both CratonVM policies. Bisected
step by step, the run gets through every virtual-thread construct — an
`ofVirtual().start()`, `Thread.startVirtualThread`, 16 tasks submitted to
`Executors.newVirtualThreadPerTaskExecutor()`, all 16 futures resolved with the
right values — and then hangs on the **`close()`** of that executor.

`ExecutorService.close()` is a default interface method: `shutdown()`, then
`while (!terminated) terminated = awaitTermination(1L, TimeUnit.DAYS)`. So a
`close()` that never returns is an executor that never reaches `TERMINATED`.

Reflecting into the executor after a deliberately short `awaitTermination`
(`--add-opens=java.base/java.util.concurrent=ALL-UNNAMED`) said exactly that,
and said why:

| field | HotSpot 25 | CratonVM |
|---|---|---|
| `state` | 2 (`TERMINATED`) | 1 (`SHUTDOWN`) |
| `terminationSignal` | count 0 | count **1** |
| `threads` | `size=0` | `size=-1` |

`ThreadPerTaskExecutor.tryTerminate()` only advances `SHUTDOWN` → `TERMINATED`
when `threads.isEmpty()`. `threads` is a `ConcurrentHashMap.newKeySet()`, and
it was reporting **-1**, a value real `ConcurrentHashMap.size()` cannot produce
— it clamps `sumCount() < 0` to `0`.

## The mechanism

`native_chm_new_key_set` did not return a `ConcurrentHashMap$KeySetView`. It
returned a **`java/util/HashSet`**, and its own sibling stated the reason out
loud:

> `KeySetView`'s Java-side `add()` calls `CHM.putVal` which uses the native
> CHM's segment fields that we don't populate, so we must back this with our
> `HashSet` synthetic instead of letting a real `KeySetView` form.

So the object handed to callers was a `HashSet`, and `add`/`remove`/`size` on
it dispatched to the **`HashMap`** natives — which take no lock, because a
`HashMap` is not supposed to need one. `ConcurrentHashMap.newKeySet()` is the
JDK's canonical "give me a concurrent set", and CratonVM answered it with a
data structure that is unsafe under concurrent mutation.

### The reproducer, and what it actually measured

Balanced churn — every `add` is followed by its own `remove`, so the correct
final size is `0` on any interleaving:

```java
Set<Object> s = ConcurrentHashMap.newKeySet();
Thread[] th = new Thread[8];
for (int t = 0; t < 8; t++) {
    th[t] = new Thread(() -> {
        for (int i = 0; i < 500; i++) { Object o = new Object(); s.add(o); s.remove(o); }
    });
    th[t].start();
}
for (Thread t : th) t.join();
int iter = 0; for (Object o : s) iter++;
System.out.println("size=" + s.size() + " isEmpty=" + s.isEmpty() + " iter=" + iter);
```

| | result |
|---|---|
| HotSpot 25 | `size=0 isEmpty=true iter=0` — every run |
| CratonVM `--real-jdk`, before | `size=-3 / 56 / 131`, `iter=9..11` — 3 of 3 rounds bad, twice over in an A/B/B/A |
| CratonVM `--jdk-only`, before | the run itself wedged past the 200 s bound, twice over |
| CratonVM `--synthetic-jdk`, before | `size=77 / 88`, `iter=5..6` — 3 of 3 rounds bad |
| CratonVM, all three modes, after | `size=0 isEmpty=true iter=0`, 3 of 3 rounds clean, twice over in each mode |

The original write-up recorded `iter=0` alongside `size=18` and concluded "the
table is correct and the count is not". On a loaded 16-core host the table is
not correct either: `iter` came back 9, 10 and 11. The unlocked bucket writes
lose and duplicate nodes as well as miscounting. Both faces are the one missing
lock.

## The fix

Option 2 of the two the original write-up proposed, taken in full:
`ConcurrentHashMap$KeySetView` has its own natives, and `newKeySet()` returns a
real one.

* `newKeySet()` / `newKeySet(int)` allocate a real `ConcurrentHashMap` (with the
  segmented native layout `chm_init_segments` builds) and wrap it in a real
  `ConcurrentHashMap$KeySetView` whose `map` field points at it and whose
  `value` field is `Boolean.TRUE`. Every mutation therefore inherits the
  per-segment monitor that already makes `native_chm_put` / `native_chm_remove`
  safe, and `size()` is the sum of per-segment counts a writer only ever updates
  under that segment's lock — it cannot go negative.
* `keySet(V mappedValue)` returns a live, add-able view over `this`, which is
  what the overload exists for. It used to delegate to `keySet()`, whose product
  is neither add-able nor a `KeySetView`.
* `Collections.newSetFromMap(new ConcurrentHashMap<>())` — the other canonical
  spelling, and the same wrong answer for the same reason — now returns that
  same live view. A `newSetFromMap` over an empty map IS `map.keySet(TRUE)`, so
  it is built by the same helper rather than duplicated. The registration's
  `IdentityHashMap` and `LinkedCaseInsensitiveMap` branches (which need the real
  `Collections$SetFromMap` for their non-value-hash key semantics) are
  untouched, as is the plain-`HashMap`/`WeakHashMap` fallback.
* The class identity is fixed as a consequence:
  `(ConcurrentHashMap.KeySetView<K,?>) set` no longer throws
  `ClassCastException`, and `getMappedValue()` / `getMap()` work.

### The partial-surface trap, and how it is avoided

The original write-up warned that giving `KeySetView` its own natives risks
leaving some method to real bytecode, which then walks the unpopulated `table`
and answers empty. Three things keep that from happening:

1. The registered surface is complete, not minimal: `size`, `isEmpty`, `clear`,
   `contains`, `add`, `remove`, `iterator`, `toArray` ×3, `toString`,
   `hashCode`, `equals`, `forEach`, `stream`, `spliterator`, `containsAll`,
   `addAll`, `removeAll`, `retainAll`, `removeIf`, `getMappedValue`, `getMap`.
2. In real-JDK mode a registered native only beats real bytecode when the
   `(class, method, descriptor)` triple is in
   `force_native_over_real_jdk_bytecode`. The entry added there lists exactly
   the methods `KeySetView` itself declares whose bodies read `table`. The
   methods `CollectionView` declares — `size`/`isEmpty`/`clear`/`toArray`/
   `toString`/`containsAll`/`removeAll`/`retainAll` — are deliberately NOT
   forced: their real bodies are written in terms of `map`, already native, or
   of `iterator()`, which now is. Forcing them would also capture `ValuesView`
   and `EntrySetView`, which share that declaring class but not these
   semantics.
3. `CollectionView.removeAll` / `retainAll` are written in terms of
   `it.remove()`, so the view's iterator wires the view itself as its removal
   target, and `native_map_key_itr_remove` routes a `KeySetView` backing to the
   view's own `remove` instead of `native_hs_remove`.

### The synthetic-jdk half, which is a different problem

In `--synthetic-jdk` mode there is no `CollectionView` bytecode at all, so those
same registrations are the only implementation — which is why the surface is
registered in full rather than only where real bytecode is wrong.

That alone was not enough, in two ways, and both are worth knowing about for the
next native that fabricates a class:

* **Interface registrations outrank exact-class ones.**
  `java/util/Set.size`, `java/util/Set.iterator`,
  `java/util/Collection.iterator`, `java/util/AbstractSet.hashCode` … are what a
  call site resolves to when the receiver's class declares no such method, which
  for a fabricated `KeySetView` is every method. So the first cut had `add`,
  `contains`, `remove`, `toArray` and `toString` working — those have no
  interface-level registration and fall through to the exact-class natives —
  while `size()` answered `0` and `iterator()` answered `null`, NPE-ing every
  `for (x : set)`. Thirteen generic entry points now consult a memoized
  `CF_KEY_SET_VIEW` classification bit and reroute (`ksv_route`); a
  `class_name_of_id` call there would have cost every ordinary `HashSet.size()`
  a lock and a `String`.
* **A fabricated class has no supertypes unless the stub tables give it any.**
  Without `java/util/Set` in `jdk_interfaces`, `AbstractSet.equals`'s "a Set
  equals only another Set" guard answered false for a set that was plainly
  equal. `class_manager.rs` now declares the view's superclass
  (`java/util/AbstractSet`), its interfaces, and its two named fields
  (`map`, `value`) so `resolve_field_index_by_class_id` finds the same slots in
  synthetic mode that it finds against the real class.

## Verification

| probe | `--real-jdk` | `--jdk-only` | `--synthetic-jdk` |
|---|---|---|---|
| balanced churn, A/B/B/A vs the pre-fix binary | pre 3/3 bad → post 0/3 | pre wedged → post 0/3 | pre 3/3 bad → post 0/3 |
| 4,000 distinct concurrent adds; disjoint add/remove; iterate-under-mutation | PASS | PASS | PASS |
| full `Set` surface + `KeySetView` cast + `getMappedValue`/`getMap` + `keySet(V)` write-through | PASS | PASS | see note |
| `newVirtualThreadPerTaskExecutor()` try-with-resources close | 25 ms | 24 ms | n/a |
| `newThreadPerTaskExecutor(factory)` bounded `awaitTermination` | `true` | `true` | n/a |
| `Collections.newSetFromMap(new ConcurrentHashMap<>())` surface + churn | PASS | PASS | see note |
| `regression-suite` `RChmKeySetView` | 5 of 5 runs PASS; FAILs on the pre-fix binary | | |
| `regression-suite` full `CORE_CLASSES` | 8 pre-existing failures, identical set on both binaries — no regression | | |

The `--synthetic-jdk` notes: the surface and `newSetFromMap` probes both stop
early in that mode, on both the pre-fix and post-fix binaries, for reasons that
have nothing to do with this bug —
`Arrays.asList("a","b","c")` returns a list of **size 0** there, and
`java/util/IdentityHashMap` is absent. Filed as residual 3. The view itself is
fine in synthetic mode: three `add`s give `size=3`, iteration yields 3,
`toArray().length` is 3 and `toString()` is `[a, b, c]`.

## Blast radius, closed

* `Executors.newVirtualThreadPerTaskExecutor()` and
  `Executors.newThreadPerTaskExecutor(factory)` in a try-with-resources close
  promptly.
* A bounded `awaitTermination(n, unit)` on those executors returns `true`.
* `Collections.newSetFromMap(new ConcurrentHashMap<>())` survives the same
  balanced churn.

## Residuals, handed off

Three things this change deliberately did not fix. All three were measured on
the same host in the same session, and all three reproduce identically on the
binaries either side of this work — none is a regression from it.

1. **`ThreadPoolExecutor.execute` drops every task past the core pool size, and
   `shutdown()` leaves idle workers parked.** The original write-up said
   `ThreadPoolExecutor` (`newFixedThreadPool`, `newCachedThreadPool`) "is
   unaffected — it does not use a `newKeySet()`", and that is true of *this*
   defect. It is not true that such a pool works: 8 tasks submitted to a
   `newFixedThreadPool(4)` leave `getTaskCount()` at 3 with an empty queue and
   only 4 futures ever resolving, and `shutdown()` never reaches `TERMINATED`.
   `regression-suite/src/RExecutorShutdown.java` is red on `dev` today for the
   second half of that. So `try (var es = Executors.newFixedThreadPool(4))`
   hangs on the closing brace too — for its own, unrelated reason. Filed as
   `threadpoolexecutor-drops-queued-tasks-and-never-terminates-20260806.md`.

2. **`ConcurrentHashMap.keySet()` (no argument) still returns a live-view
   `HashSet`, not a `KeySetView`.** It is not add-able in the JDK either, and
   its `remove` writes through to the source map's native (locked) `remove`, so
   it has neither half of this bug. The only remaining gap is class identity:
   `(ConcurrentHashMap.KeySetView<K,?>) chm.keySet()` still throws
   `ClassCastException`. Closing it means retargeting the `make_view_set_of`
   machinery that Felix, Spring and H2 paths depend on — a much larger blast
   radius than the identity gap justifies on its own.

3. **`Arrays.asList(...)` returns an empty list under `--synthetic-jdk`.**
   `Arrays.asList("a","b","c")` reports `size=0` and iterates nothing, so
   `new HashSet<>(asList)` and `HashSet.addAll(asList)` are both empty too —
   on the pre-fix binary as much as the post-fix one. It is the sole cause of
   every remaining "surface" failure in that mode. Unfiled here because it is
   squarely a synthetic-class-library gap, not a collections-concurrency one.

## What it is not

Three hypotheses were measured and ruled out while the bug was open. They are
kept here so nobody re-checks them.

* **Not identity-hash instability across GC.** 4,000 objects in a
  `ConcurrentHashMap.newKeySet()` and a `HashSet`, 40 MB of allocation and an
  explicit `System.gc()` in between: `identityHashChanged=0 chmMiss=0 hsMiss=0
  removeFailures=0` on both VMs.
* **Not a broken CAS.** Eight threads × 20,000 iterations of
  `AtomicLong.incrementAndGet`, `AtomicInteger.incrementAndGet`,
  `LongAdder.increment` and an `AtomicLongFieldUpdater` compare-and-set loop —
  the exact shape of `ConcurrentHashMap.addCount` — land on the exact expected
  160,000 on both VMs. No lost updates.
* **Not the real `ConcurrentHashMap` counter.** On CratonVM the real
  `baseCount`, `counterCells` and `table` fields are all zero/null after a
  workload the map serviced correctly, which is
  [[native-backed-state-is-invisible-to-real-jdk-bytecode]] again — the state
  lives in the native side table, so `baseCount` was not the number that was
  wrong.

The raw `ConcurrentHashMap` (`put`/`remove` directly) held up under the same
churn throughout: those go through `native_chm_put`/`native_chm_remove`, which
take the per-segment monitor. It was only the `newKeySet()` product that was
unprotected, because it was not a `ConcurrentHashMap` at all — which is exactly
what the fix changes.
