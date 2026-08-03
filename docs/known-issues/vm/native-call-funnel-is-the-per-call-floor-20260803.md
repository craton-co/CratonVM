# The per-call floor is the NATIVE funnel, not Java calls

| | |
|---|---|
| **Status** | OPEN — characterised, one fix landed, the larger half named |
| **Severity** | high — it is the wall under `java.util.concurrent` and every native-dense path |
| **Discovered** | 2026-08-03, taking on [`aqs-thread-handoff-latency`](aqs-thread-handoff-latency-20260803.md) |

## Java calls are fine. Native calls are 40-100x them.

Converged multi-pass measurements (`probes/CallShapeProbe.java`,
`probes/NativeShapeProbe.java`, `probes/CallFloorConvergenceProbe.java`), quiet
host, last pass of four:

| | HotSpot | CratonVM |
|---|---|---|
| no call (control) | ~0 ns | 1.0 ns |
| **interpreter-answered (`Thread.onSpinWait`)** | 38.3 ns | **4.4 ns** |
| **ordinary Java call** (final/open class, private/public, void/long — all identical) | ~0 ns | **8.4 ns** |
| INTRINSIC static, 1 primitive arg (`Math.abs`) | 0.4 ns | **184 ns** |
| INTRINSIC receiver (`String.length`) | 0.1 ns | 411 ns |
| NATIVE static, no args (`System.nanoTime`) | 25.4 ns | 326 ns |
| NATIVE static, no args, returns object (`Thread.currentThread`) | 0.0 ns | 409 ns |
| NATIVE static + object arg (`identityHashCode`) | 1.0 ns | 361 ns |
| NATIVE receiver, no args (`AtomicInteger.get`) | 0.4 ns | 489 ns |
| NATIVE receiver + 2 primitive args (`AtomicInteger.CAS`) | 5.4 ns | 808 ns |

Read the shape, not the rows:

* An **ordinary Java call is 8.4 ns** and does not care whether the class is
  final, the method private, or the return void. Java call dispatch is not the
  problem, and `probes/CallFloorProbe.java` agrees (1.85 ns invokestatic,
  7.2 ns invokevirtual).
* Anything entering `safe_native_call` costs **~180-330 ns fixed, plus
  ~100-150 ns per argument** — the per-arg slope is visible across the last
  five rows.
* **Being an "intrinsic" does not help.** `Math.abs` is in the interpreter
  intrinsic table and still costs 184 ns, because `invoke_cached_intrinsic`
  routes through the same funnel. The only rung near zero is
  `Thread.onSpinWait`, which the interpreter answers *inline* and which
  therefore never enters the funnel at all — the 4.4 ns is the proof that the
  funnel, not the call, is the cost.

The funnel is `safe_native_call_impl`: argument copy + GC-forwarding barrier,
per-argument pinning into `native_pin_roots`, an STW probe, two GC-pressure
probes, two `thread_state::record_transition` calls, the native ring buffer,
`catch_unwind`, and the `memwatch`/`ec_watch` polls.

## Why it matters: the concurrency stack is native-dense

`probes/LockNativeCensusProbe.java` run under `--dump-native-registry` gives the
exact per-operation native count for an uncontended `ReentrantLock`
`lock()`+`unlock()` pair — no guessing which accessors are shadowed:

| native | calls per lock/unlock pair |
|---|---|
| `Thread.currentThread()` | 2.00 |
| `AbstractOwnableSynchronizer.setExclusiveOwnerThread(Thread)` | 2.00 |
| `jdk/internal/misc/Unsafe.compareAndSetInt` | 1.00 |

Five natives, and nothing else on the path is native (`getState`/`setState`/
`getExclusiveOwnerThread` all run as bytecode). Five funnel entries at
330-810 ns is 2.5-4 us of the measured 7.6-14.8 us pair.

`setExclusiveOwnerThread` is native **deliberately** — it is the JDK's single
ownership transition, and intercepting it is how `ThreadMXBean` builds its
owned-synchronizer index without a racy heap walk. It is not a mistake to
remove; it is a feature whose cost is the funnel.

## Fixed here: `Thread.currentThread()` answered inline

Same treatment as `Thread.onSpinWait`: once a thread's `java_thread_obj` mirror
exists, `currentThread()` is one field read of an existing GC root — nothing to
pin, nothing that allocates, collects or throws. The interpreter now answers it
from the inline cache without entering the funnel, falling through to the
ordinary path when the mirror has not been built yet.

Proven with `CRATONVM_INTRINSIC_STATS=1` (which counts fast-path hits — the
counter exists because two earlier attempts at this shape were **inert**, and
timings alone cannot tell "never installed" from "installed but no faster"):

| `--nojit` | before | after |
|---|---|---|
| interpreter intrinsic dispatches | 24.0M | **32.0M** (+8M = the measured loop) |
| `Thread.currentThread()` | 306 ns | **136 ns** |

## The larger half, not yet done: the JIT's own native path

**With the JIT on, the numbers do not move** — the counter goes 16,007,000 →
16,013,999, i.e. essentially none of the compiled loop's calls reach the
interpreter's inline cache. Compiled code dispatches natives through
`vm/src/jit/helpers.rs`, which has its own path into
`safe_native_call_prevalidated_objects`. Since hot code is compiled, that is
where the remaining ~400 ns per native lives.

The codebase already has the precedent, applied case by case: `Integer.valueOf`
and `Integer.intValue` have hand-written JIT fast paths that skip the funnel,
with the reasoning spelled out in `helpers.rs` — *"this operation cannot
allocate or safepoint, so entering `safe_native_call` adds only rooting/panic/
dispatch overhead on every unbox in a compiled loop."* That is exactly the
argument for `Thread.currentThread`, and the generalisation is the real fix:

1. **Extend the funnel bypass to the JIT side** for `Thread.currentThread`
   first (2 of the 5 natives on the lock path), then as a *class* of
   "leaf natives" rather than another hand-written case: no object arguments to
   pin, cannot allocate, cannot safepoint, cannot throw. That predicate wants to
   live on the registration (a `NativeKind`-adjacent flag), not in a growing
   `match` in `helpers.rs`.
2. **Reduce the fixed funnel cost itself.** ~180-330 ns for zero arguments is
   ~1000 cycles of bookkeeping, most of it diagnostics that are off by default.
   Nobody has profiled it; this document asserts *where* the time is, not
   *which line*.
3. `Unsafe.compareAndSetInt` (1 per lock pair) genuinely must reach a real CAS,
   but 808 ns for it is funnel, not atomics.

## Corrections to earlier revisions of the AQS doc

Two of my own conclusions were wrong and are retracted here:

* **"An empty instance call costs 431 ns."** It converges to 8.4 ns by pass 2.
  The 431 ns came from a single warm-up-then-measure pass — the probe looked
  before the loop was compiled. Every rung is now multi-pass and prints all
  passes; a rung that has not gone flat is not a measurement.
* **"The AQS gap is 16 calls at the per-call floor."** Built on that bad number.
  16 Java calls is 138 ns, not 15 us. The gap is the five *native* calls plus
  what `setExclusiveOwnerThread` does inside them.

## Reproduction

```powershell
$jdk = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& "$jdk\bin\javac.exe" -d out probes\NativeShapeProbe.java probes\CallShapeProbe.java `
                              probes\CallFloorConvergenceProbe.java probes\LockNativeCensusProbe.java
& "$jdk\bin\java.exe" -cp out NativeShapeProbe                      # HotSpot control
& <cratonvm.exe> --java-home $jdk -cp out NativeShapeProbe
& <cratonvm.exe> --dump-native-registry n.json --java-home $jdk -cp out LockNativeCensusProbe 1000000
```

`CallShapeProbe`'s HotSpot column reads 0.00 throughout — HotSpot eliminates the
loops outright. That arm is a sanity check only; use `NativeShapeProbe` for
cross-VM comparison.
