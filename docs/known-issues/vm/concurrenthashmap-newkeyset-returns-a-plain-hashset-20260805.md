# `ConcurrentHashMap.newKeySet()` returns a plain `HashSet`, so `ExecutorService.close()` hangs forever

| | |
|---|---|
| **Status** | OPEN — mechanism pinned to one function, fix not attempted here |
| **Severity** | high — an unbounded hang, reachable from `try (var es = Executors.new…())`, which is the idiomatic modern spelling |
| **Modes** | BOTH. `--real-jdk` and `--jdk-only` behave identically; this is not a strict-mode defect |
| **Opened** | 2026-08-05, by `probes/JdkOnlyPlatformProbe`'s `vthreads` section (L8, criterion 6) |
| **Owner file** | `native-collections/src/lib.rs` — L10's owned file, hence filed rather than fixed |

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
(`--add-opens=java.base/java.util.concurrent=ALL-UNNAMED`) says exactly that,
and says why:

| field | HotSpot 25 | CratonVM |
|---|---|---|
| `state` | 2 (`TERMINATED`) | 1 (`SHUTDOWN`) |
| `terminationSignal` | count 0 | count **1** |
| `threads` | `size=0` | `size=-1` |

`ThreadPerTaskExecutor.tryTerminate()` only advances `SHUTDOWN` → `TERMINATED`
when `threads.isEmpty()`. `threads` is a `ConcurrentHashMap.newKeySet()`, and
it is reporting **-1**, a value real `ConcurrentHashMap.size()` cannot produce
— it clamps `sumCount() < 0` to `0`.

## The mechanism

`native_chm_new_key_set` (`native-collections/src/lib.rs`) does not return a
`ConcurrentHashMap$KeySetView`. It returns a **`java/util/HashSet`**:

```rust
fn native_chm_new_key_set(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let mut set = alloc_synthetic(ctx, "java/util/HashSet", 1);
    let mut backing = rooted_across(…, |ctx| alloc_backing_map(ctx));
    …
}
```

Its own sibling states the reason out loud:

> `KeySetView`'s Java-side `add()` calls `CHM.putVal` which uses the native
> CHM's segment fields that we don't populate, so we must back this with our
> `HashSet` synthetic instead of letting a real `KeySetView` form.

So the object handed to callers is a `HashSet`, and `add`/`remove`/`size` on it
dispatch to the **`HashMap`** natives — which take no lock, because a `HashMap`
is not supposed to need one. `ConcurrentHashMap.newKeySet()` is the JDK's
canonical "give me a concurrent set", and CratonVM answers it with a
data structure that is unsafe under concurrent mutation.

## Reproducer

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
| CratonVM `--real-jdk` | `size=18 isEmpty=false iter=0` (also seen: 30, and 0) |

`iter=0` with `size=18` is the whole finding in one line: the **table is
correct and the count is not**. The set really is empty; it just does not
know it. `isEmpty()` is `size() == 0`, so it answers `false` forever, and
`tryTerminate` never fires.

Intermittent by nature — it needs a genuine interleaving. Eight threads
reproduce it roughly one run in two on a loaded host; one and two threads never
did.

## What it is not

Three hypotheses were measured and are ruled out. Do not re-check them.

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
  lives in the native side table, so `baseCount` is not the number that is
  wrong.

Note that the *raw* `ConcurrentHashMap` (`put`/`remove` directly) held up under
the same churn: those go through `native_chm_put`/`native_chm_remove`, which
take the per-segment monitor. It is only the `newKeySet()` product that is
unprotected, because it is not a `ConcurrentHashMap` at all.

## Blast radius

Anything whose liveness depends on a `ConcurrentHashMap.newKeySet()` reaching
empty. The JDK's own `ThreadPerTaskExecutor` is the one this was found through,
which means:

* `Executors.newVirtualThreadPerTaskExecutor()` and
  `Executors.newThreadPerTaskExecutor(factory)` in a try-with-resources — the
  idiomatic JDK 21+ spelling — can hang on the closing brace.
* Any explicit `close()`, or `shutdown()` + an unbounded `awaitTermination`.
* A *bounded* `awaitTermination(n, unit)` returns `false` instead of `true`,
  so callers that check the result report a shutdown timeout that did not
  happen.

`ThreadPoolExecutor` (`newFixedThreadPool`, `newCachedThreadPool`) is unaffected
— it does not use a `newKeySet()` — so the hang looks capricious: the same code
shape works for one factory method and wedges for another.

## Two candidate fixes, neither attempted

1. **Make the returned set concurrent.** Back `newKeySet()` with a real
   native-backed `ConcurrentHashMap` and return a view over it, so `add` and
   `remove` inherit the segment monitors that already make `native_chm_put` /
   `native_chm_remove` safe. `native_chm_key_set` already builds a live
   write-through view (`make_view_set_of(…, VIEW_KIND_KEYSET, …)`); the gap is
   that a `keySet()` view has no `add`, whereas a `newKeySet()` view must. Note
   that the existing view rebuilds its backing on every read, which is not
   obviously safe to do while other threads mutate the source — check that
   before reusing it.
2. **Give `ConcurrentHashMap$KeySetView` its own natives** and let
   `newKeySet()` return a real one. This also fixes the class identity —
   today the returned object is a `HashSet`, so any
   `(ConcurrentHashMap.KeySetView<K,?>) set` cast throws
   `ClassCastException` (cf.
   [[synthetic-standin-checkcast-use-jdk-interfaces]]). The hazard is the
   partial-surface trap: `KeySetView` has `addAll`/`removeAll`/`retainAll`/
   `forEach`/`stream`/`spliterator`/`toArray`/`iterator`/`hashCode`/`equals`,
   and any method left to real bytecode walks the unpopulated `table` and
   answers empty.

Whichever is taken, the regression test is the churn loop above — not a
single-threaded size check, which passes today.
