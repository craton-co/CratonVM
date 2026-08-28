# `java.util.concurrent` primitives are 9-114x, and composition is 113x, with every compile refusal gone

## Status
**OPEN, opened 2026-08-28.** This is the residual of two pages that closed the
same week, and it is what is left after the thing that dominated both was fixed:

* `performance/completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`
  — `UniCompose.tryFire` / `UniRelay.tryFire` were force-interpreted by a stale
  `ForkJoinTask`-subclass blocklist, and the probe's own `chain` by a `dup_x2`
  the backend could not prove. Composition went **3.95x**, 446x -> 113x.
* `performance/varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`
  — `VarHandle.set` bound, the per-call global mutex removed, the CAS served
  in-funnel, and the volatile stripe pool unpacked from a single cache line.

Nothing on this page is a compile refusal any more. `CRATONVM_DBG=jit-method-stats`
on the composition probe reports `hot_but_stuck_in_interpreter=0`. What is left
is the per-operation cost of the primitives themselves — and the single worst
row is already diagnosed rather than merely measured: see "Where to look first"
item 1, `AtomicReference.compareAndSet`, which is a synthetic stub shadowing a
one-line delegation that would otherwise hit an existing fast path.

## Severity
**HIGH and broad**, for the same reason the `VarHandle` page was: these are the
primitives under `CompletableFuture`, `AbstractQueuedSynchronizer`,
`ConcurrentHashMap`, `ForkJoinPool`, `ConcurrentLinkedQueue` and `StampedLock`.

## The measurement

`HibfixVarHandleProbe`, `-Dprobe.iters=2000000`, single-threaded, on an idle
host (load 0.06). Six runs of each VM, **interleaved** — that alternation is not
optional here: this probe times each operation once after a shared warmup, and
the page it came from records a run where every number moved 1.5x because the
box got busier. Medians of six, with the min and max so the spread is visible:

| operation | CratonVM min/med/max (ns) | HotSpot min/med/max (ns) | ratio |
|---|---:|---:|---:|
| `AtomicReference.compareAndSet` | 1146.7 / **1198.9** / 1231.0 | 9.6 / 10.5 / 30.2 | **114x** |
| `VarHandle.compareAndSet` reference | 409.0 / **488.7** / 499.8 | 9.2 / 9.8 / 12.3 | **50x** |
| `VarHandle.compareAndSet` int | 237.2 / **293.1** / 302.7 | 9.7 / 11.9 / 18.9 | 25x |
| `VarHandle.get` reference | 145.4 / **149.7** / 162.0 | 3.4 / 7.5 / 9.2 | 20x |
| `VarHandle.set` reference | 59.6 / **87.2** / 88.4 | 5.0 / 6.4 / 6.8 | 14x |
| `VarHandle.get` int | 47.9 / **66.9** / 84.7 | 4.2 / 7.4 / 8.5 | 9x |
| plain field store (baseline) | 13.0 / **13.3** / 13.5 | 3.9 / 4.7 / 7.3 | 2.8x |
| `AtomicInteger.incrementAndGet` | 5.5 / **5.6** / 5.7 | 7.7 / 9.1 / 11.6 | **0.6x** |

**`AtomicInteger` is still the control that makes this conclusive**, and it now
says something stronger than it did on 2026-08-24: CratonVM is **faster** than
HotSpot on it. So this is not "atomics are slow", not "the box is slow", and not
"the JDK's j.u.c. classes are slow" — it is these specific operations.

**These are compiled-path numbers, not interpreter numbers.** The probe's loops
live in `main`, which is the shape that measures the interpreter if nothing
OSR-compiles it; `CRATONVM_DBG=jit-method-stats` on the same run reports
`osr=1 c2=1` and `hot_but_stuck_in_interpreter=0`, so the loop was compiled.

## Downstream: composition

`HibfixComposeProbe2`, no scheduler, no locks, no cross-thread handoff — each
thread owns its futures end to end. Idle host, interleaved, `wrong=0` in every
run of every arm:

| configuration | CratonVM | HotSpot | ratio |
|---|---:|---:|---:|
| 2 threads, 80 000 chains | 852 ms | 44 ms | **19x** |
| 24 threads, 4.8 M chains | 27 590 ms | 244 ms | **113x** |

The two rows are the same code, so the difference between 19x and 113x is a
second question this page carries: per chain, HotSpot goes from 0.55 us to
0.051 us as the workload grows (a 10.8x amortisation), CratonVM from 10.6 us to
5.7 us (1.9x). Both improve with more work; HotSpot improves 5.7x more. Whether
that is warmup, thread scaling or both is not established here, and the arms to
separate them are cheap: hold chains fixed and vary threads.

### The cost structure, re-counted on the FIXED binary

`--dump-native-registry` on the 2-thread / 80 000-chain run — **21 native
crossings per chain**, and the shape is nothing like the one this workload had
while it was interpreted:

| native | calls | per chain | complete? |
|---|---:|---:|:--:|
| `java/lang/invoke/VarHandle.compareAndSet` | 837 518 | **10.47** | yes |
| `java/lang/Integer.intValue` | 319 056 | 3.99 | **no — a floor** |
| `java/lang/invoke/VarHandle.set` | 319 050 | 3.99 | yes |
| `java/util/concurrent/CompletableFuture.complete` | 200 000 | 2.50 | yes |
| `java/lang/Object.<init>` | 2 779 | **0.03** | yes |

`Object.<init>` was **9 per chain** when composition was interpreted and is now
0.03: that is what compiling the completion machinery did to the crossing count,
and it is an independent confirmation that the fix took. Read the
`invocations_complete` column: a `false` there means the number is a FLOOR, not
a total, so `Integer.intValue` is at least 3.99 per chain.

At the 488.7 ns measured above, **10.47 CAS per chain is ~5.1 us of a
~10.6 us chain — about half of it**, and by far the largest identified
component. That estimate carries one assumption worth stating: the per-op probe
measures an isolated, single-threaded, cache-hot loop, so the marginal cost of a
CAS inside composition could be higher or lower. Confirming it needs a native
profile, which is item 3 below.

Note also that a CAS served by `try_varhandle_instance_field_cas` still calls
`count_jit_native_dispatch`, so this row counts fast-path CASes too. It is a
count of CAS OPERATIONS, not of funnel misses.

## What is already excluded, with the evidence

Do not re-derive these.

* **Compile refusals.** `hot_but_stuck_in_interpreter=0` on the composition
  probe. Restoring either of the two fixed refusals with its kill switch costs
  2.19x and 2.88x respectively, so the instrument works and reads zero.
* **The native-shadow caller seal.** `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0`
  changes the compile count by one method and the runtime by nothing. Its blast
  radius is real (a median 1 010 methods sealed per class on the
  hibernate-reactive suite) and its ceiling on this workload is ~0.
* **The `vh_meta_table` global mutex.** Removed; it was 10.9x on thread scaling
  and ~9.5% on composition.
* **The volatile stripe pool sharing one cache line.** Fixed; 1.64x at 24
  threads on `HibfixVarHandleScale`, and exactly 1.00x at one thread.
* **The CAS funnel.** `try_varhandle_instance_field_cas` serves the instance
  case inside `jit_invoke_dispatch`: 3 839 190 served, 0 declined on the
  composition workload. It was worth 0.5% there.
* **Java-frame profiling.** A Java-frame sampler attributes the whole of a
  native call to the Java frame that made it. It is what produced "34% in
  `tryPushStack`" and sent three days into `VarHandle` primitives that turned
  out not to be the constraint. Use `perf` on the binary.

## Where to look first

1. **`AtomicReference.compareAndSet` at 114x, against `VarHandle.compareAndSet`
   reference at 50x — and the route is already identified.** On JDK 9+ that
   method is a one-line delegation:

   ```java
   public final boolean compareAndSet(V expectedValue, V newValue) {
       return VALUE.compareAndSet(this, expectedValue, newValue);
   }
   ```

   so it should cost a `VarHandle` CAS plus a call, i.e. ~490 ns. It costs
   1199 ns because **CratonVM registers a synthetic stub over it and the stub
   wins even in real-JDK mode.** `--dump-native-registry` on the probe:

   ```
   2000  java/util/concurrent/atomic/AtomicReference.compareAndSet(...)Z  kind=synthetic-stub
   2000  java/util/concurrent/atomic/AtomicReference.get()...             kind=synthetic-stub
   ```

   (`native-builtins/src/phases_early.rs`, the `let ar = ".../AtomicReference"`
   block). The stub calls `compare_and_swap_field` on slot 0 — correct, but it
   arrives through the FULL native funnel, whereas a `VarHandle` CAS is caught
   by `try_varhandle_instance_field_cas` inside `jit_invoke_dispatch` and never
   reaches a native at all. That is the 710 ns.

   The fix has a direct precedent: §6 of the hibernate-reactive page
   de-registered seven `CompletionStage` methods for being pure delegations over
   a real JDK, and `native-collections/src/lib.rs` has the shape to copy —
   `let skip_delegating_cf = r.real_jdk() && cf_delegating_yield_enabled();`,
   a gate plus a kill switch so one binary can be A/B'd against itself. What has
   to be checked before doing it: the stubs also cover `<init>`, `get`, `set`,
   `lazySet`, `getAndSet` and `compareAndExchange`, all of which assume `value`
   is instance slot 0; the real JDK class declares exactly one instance field,
   so the assumption holds and dropping the whole block is the coherent move,
   but `AtomicReference` is reached from everywhere and this wants its own gate
   cycle rather than a ride-along.

   The `AtomicInteger` twin being at parity (0.6x — FASTER than HotSpot) is what
   says the wrapper pattern itself is fine: that one has its own intrinsic.
2. **`VarHandle.get` reference at 20x, against `VarHandle.get` int at 9x.**
   `VARHANDLE_READ_DIRECT_FNS` binds reads of PRIMITIVE fields only, and its
   comment says `L` and `[` "are absent on purpose": a reference RETURN must be
   published as a handoff root before the caller can store it, and the direct
   arm takes no thread borrow to publish one with. That justification is sound
   and the 2.2x is its measured price — which is worth knowing before anyone
   tries to bind it.
3. **A NATIVE profile of the composition probe with everything compiled.** The
   last one was taken when composition was 92.35% interpreted, so it measured
   the interpreter and nothing else. Nobody has profiled the current shape.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
javac -d <out> HibfixVarHandleProbe.java HibfixComposeProbe2.java
# interleave the two VMs; a single pair on a busy box is not a result
cratonvm --java-home <jdk> -Dprobe.iters=2000000 -cp <out> HibfixVarHandleProbe
java -Dprobe.iters=2000000 -cp <out> HibfixVarHandleProbe
cratonvm --java-home <jdk> -Dprobe.threads=2 -Dprobe.chains=40000 -cp <out> HibfixComposeProbe2
```

## Related

- `performance/completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`
- `performance/varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`
- [`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
  — the same shape: a hot JDK primitive left on the generic funnel.
- [`interpreted-invoke-cost-350ns-20260825.md`](interpreted-invoke-cost-350ns-20260825.md)
