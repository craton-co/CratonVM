# `java.util.concurrent` primitives are 16-40x, and composition is 19x, with every compile refusal gone

## Status
**OPEN, opened 2026-08-28.** This is the residual of two pages that closed the
same week, and it is what is left after the thing that dominated both was fixed:

* `performance/completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`
  — `UniCompose.tryFire` / `UniRelay.tryFire` were force-interpreted by a stale
  `ForkJoinTask`-subclass blocklist, and the probe's own `chain` by a `dup_x2`
  the backend could not prove. Composition went **3.95x**.
* `performance/varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`
  — `VarHandle.set` bound, the per-call global mutex removed, the CAS served
  in-funnel, and the volatile stripe pool unpacked from a single cache line.

Nothing on this page is a compile refusal any more.
`CRATONVM_DBG=jit-method-stats` on the composition probe reports
`hot_but_stuck_in_interpreter=0`, and on the per-op probe `osr=1 c2=1` with the
same zero — so what follows is the cost of COMPILED code, not of the
interpreter.

> **The first revision of this page was measured on a busier box and one of its
> conclusions did not survive.** It put the range at 9-114x and named
> `AtomicReference.compareAndSet`'s synthetic stub as a 710 ns tax with an
> obvious fix. Re-measured on an idle box against the current dev tip, the range
> is 16-40x and the stub is not a tax at all — see
> "What was tried and refuted". Both tables were six interleaved runs; the
> difference is the box, which is exactly the failure mode the `VarHandle` page
> warned about and this page then walked into.

## Severity
**HIGH and broad**, for the same reason the `VarHandle` page was: these are the
primitives under `CompletableFuture`, `AbstractQueuedSynchronizer`,
`ConcurrentHashMap`, `ForkJoinPool`, `ConcurrentLinkedQueue` and `StampedLock`.

## The measurement

`HibfixVarHandleProbe`, `-Dprobe.iters=2000000`, single-threaded, idle host
(load 0.06), six runs of each VM **interleaved**. Medians, with min and max so
the spread is visible — every row's spread is under 6%, which is what says the
box was quiet:

| operation | CratonVM min/med/max (ns) | HotSpot min/med/max (ns) | ratio |
|---|---:|---:|---:|
| `AtomicReference.compareAndSet` | 263.6 / **265.8** / 277.6 | 6.7 / 6.7 / 7.5 | **40x** |
| `VarHandle.compareAndSet` reference | 229.6 / **231.0** / 242.4 | 6.6 / 6.7 / 7.0 | **35x** |
| `VarHandle.get` reference | 76.9 / **77.0** / 77.8 | 2.4 / 2.4 / 2.6 | 32x |
| `VarHandle.compareAndSet` int | 147.3 / **148.7** / 149.3 | 6.7 / 7.1 / 7.2 | 21x |
| `VarHandle.set` reference | 50.8 / **50.8** / 50.8 | 2.7 / 2.8 / 3.0 | 18x |
| `VarHandle.get` int | 41.1 / **41.3** / 41.4 | 2.3 / 2.5 / 2.5 | 17x |
| plain field store (baseline) | 6.1 / **6.1** / 6.3 | 2.1 / 2.4 / 2.5 | 2.5x |
| `AtomicInteger.incrementAndGet` | 4.7 / **4.7** / 4.7 | 5.7 / 5.9 / 6.0 | **0.8x** |

**`AtomicInteger` is still the control that makes this conclusive**, and it says
something stronger than it did on 2026-08-24: CratonVM is **faster** than
HotSpot on it. So this is not "atomics are slow", not "the box is slow", and not
"the JDK's j.u.c. classes are slow" — it is these specific operations. The plain
store at 2.5x is the second control: the ordinary field path is close, and the
gap opens on the `VarHandle` operations specifically.

## Downstream: composition

`HibfixComposeProbe2` — no scheduler, no locks, no cross-thread handoff, each
thread owning its futures end to end. Idle host, interleaved, `wrong=0` in every
run of every arm:

| configuration | CratonVM | HotSpot | ratio |
|---|---:|---:|---:|
| 2 threads, 80 000 chains | 874-908, median **882 ms** | 43-49, median 46 ms | **19x** |
| 24 threads, 4.8 M chains | 27 003 / 27 098 ms | 216 / 421 ms | **64-125x** |

The 24-thread row is reported as a RANGE on purpose: CratonVM's two runs agree
to 0.4% and HotSpot's differ by 1.9x, so the ratio is only as good as HotSpot's
own spread and a single number would be fiction. What the two rows do agree on
is a second question this page carries: per chain, CratonVM goes 11.0 us ->
5.6 us as the workload grows (1.96x) and HotSpot 0.58 us -> 0.045-0.088 us
(6.6-13x). Both amortise; HotSpot amortises several times harder. Whether that
is warmup, thread scaling or both is NOT established here, and the arms that
would separate them are cheap: hold the chain count fixed and vary the threads.

### The cost structure, on the current binary

`--dump-native-registry` on the 2-thread / 80 000-chain run — **18 native
crossings per chain**:

| native | calls | per chain | complete? |
|---|---:|---:|:--:|
| `java/lang/invoke/VarHandle.compareAndSet` | 837 835 | **10.47** | yes |
| `java/lang/Integer.intValue` | 319 174 | 3.99 | **no — a floor** |
| `java/util/concurrent/CompletableFuture.complete` | 200 000 | 2.50 | yes |
| `java/util/concurrent/atomic/AtomicLong.incrementAndGet` | 80 000 | 1.00 | yes |
| `java/lang/Object.<init>` | 2 477 | **0.03** | yes |

Two of those rows are results rather than costs. `Object.<init>` was **9 per
chain** while composition was interpreted and is now 0.03 — that is what
compiling the completion machinery did to the crossing count. `VarHandle.set`
was 3.99 per chain on 2026-08-27 and is absent now. And read the
`invocations_complete` column: a `false` means the number is a FLOOR, not a
total, so `Integer.intValue` is at least 3.99 per chain.

**10.47 CAS per chain at 231 ns is ~2.4 us of an ~11.0 us chain — about 22%**,
the largest single identified component and not the majority of it. That
estimate assumes the marginal cost of a CAS inside composition matches the
isolated loop's, which a native profile has not yet confirmed.

Note that a CAS served by `try_varhandle_instance_field_cas` still calls
`count_jit_native_dispatch`, so this row counts fast-path CASes too: it is a
count of CAS OPERATIONS, not of funnel misses.

## What was tried and refuted

**De-registering `AtomicReference.compareAndSet` over a real JDK. Built,
measured, reverted.**

The hypothesis was the §6 pattern: on JDK 9+ that method is one line —
`return VALUE.compareAndSet(this, expectedValue, newValue);` — so a synthetic
stub over it should be a pure tax, and dropping it should let the call reach the
`VarHandle` CAS fast path. The first revision of this page measured
`AtomicReference.compareAndSet` at 1198.9 ns against `VarHandle.compareAndSet`
at 488.7 and called the 710 ns difference "the shadow".

Two things went wrong with that, and both are worth having written down.

1. **The first gate was inert, and the registry said why.** It was placed on the
   `AtomicReference` block in `native-builtins/src/phases_early.rs`.
   `--dump-native-registry` then reported **400 000 invocations in BOTH arms** —
   and its `registered_by` column named the owner:
   `native-builtins/src/util_concurrent_ext.rs:505`. The registry is FIRST-WINS
   and `register_atomic_reference_natives` runs first, so the block that was
   gated never owned the slot. A method here has several independent registrars;
   gate the one the dump names, and check the dump rather than the source.
2. **With the gate in the right place it engaged perfectly — and made the
   operation SLOWER.** ABBA on one binary, six repetitions, with the kill switch
   as the only difference and the stub confirmed absent/present in the registry
   dump either way:

   | | stub dropped | stub kept |
   |---|---:|---:|
   | `AtomicReference.compareAndSet` | **289.0 ns** | **256.9 ns** |
   | `VarHandle.compareAndSet` reference (control) | 232.8 | 233.1 |
   | `AtomicInteger.increment` (control) | 4.7 | 4.7 |

   The JDK's own bytecode path costs MORE than the stub: a `VarHandle` CAS at
   ~233 ns plus the wrapper frame is ~289, against ~257 for the stub. The
   controls are unmoved, so this is the change and not the box.

The "710 ns shadow" was an artefact of the busier first run. On a quiet box
`AtomicReference.compareAndSet` is 265.8 against `VarHandle.compareAndSet`'s
231.0 — a 1.15x wrapper, which is about what a wrapper should cost. **The stub
is not the problem; the CAS underneath it is**, and that is where the 35x lives.

## Where to look first

1. **`VarHandle.compareAndSet` on a reference field, 231 ns against 6.7.** It is
   the top row that is not a wrapper of another row, it is 10.47 calls per
   composition chain, and it is already served by
   `try_varhandle_instance_field_cas` inside `jit_invoke_dispatch` — so the
   remaining cost is inside that fast path, not in reaching it. The int CAS at
   148.7 is the same path with a cheaper payload, and the 82 ns between them
   bounds what the reference-specific work (SATB pre-barrier on `expected`, the
   post `write_barrier` on success) costs.
2. **`VarHandle.get` on a reference field, 77.0 against 2.4 — 32x, and against
   41.3 for the same read of an `int`.** `VARHANDLE_READ_DIRECT_FNS` binds reads
   of PRIMITIVE fields only, and its comment says `L` and `[` "are absent on
   purpose": a reference RETURN must be published as a handoff root before the
   caller can store it, and the direct arm takes no thread borrow to publish one
   with. That justification is sound, and the 1.9x is its measured price — worth
   knowing before anyone tries to bind it, and worth re-examining now that the
   write side found a way (a reference travels INWARD in a register the caller's
   own frame already describes, so a write has no window to root across).
3. **A NATIVE profile of the composition probe with everything compiled.** The
   last one was taken when composition was 92.35% interpreted, so it measured
   the interpreter and nothing else. Nobody has profiled the current shape, and
   the 22% the CAS accounts for leaves most of the chain unexplained.

## What is already excluded, with the evidence

Do not re-derive these.

* **Compile refusals.** `hot_but_stuck_in_interpreter=0` on both probes.
  Restoring either of the two fixed refusals with its kill switch costs 2.19x
  and 2.88x, so the instrument works and reads zero.
* **The `AtomicReference` CAS shadow.** Measured and refuted above.
* **The native-shadow caller seal.** `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0`
  changes the compile count by one method and the runtime by nothing. Its blast
  radius is real (a median 1 010 methods sealed per class on the
  hibernate-reactive suite) and its ceiling on this workload is ~0.
* **The `vh_meta_table` global mutex.** Removed; 10.9x on thread scaling.
* **The volatile stripe pool sharing one cache line.** Fixed; 1.64x at 24
  threads on `HibfixVarHandleScale`, and exactly 1.00x at one thread.
* **The CAS funnel.** `try_varhandle_instance_field_cas` serves the instance
  case: 3 839 190 served, 0 declined on the composition workload. Worth 0.5%.
* **Java-frame profiling.** A Java-frame sampler attributes the whole of a
  native call to the Java frame that made it. It produced "34% in
  `tryPushStack`" and sent three days into `VarHandle` primitives that were not
  the constraint. Use `perf` on the binary.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
javac -d <out> HibfixVarHandleProbe.java HibfixComposeProbe2.java
cratonvm --java-home <jdk> -Dprobe.iters=2000000 -cp <out> HibfixVarHandleProbe
java -Dprobe.iters=2000000 -cp <out> HibfixVarHandleProbe
```

Interleave the two VMs and repeat at least six times. A single pair on a busy
box is not a result — that is how this page's first revision got a 114x and a
wrong diagnosis out of the same probe that answers 40x when the box is idle.

## Related

- `performance/completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`
- `performance/varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`
- [`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
  — the same shape: a hot JDK primitive left on the generic funnel.
- [`interpreted-invoke-cost-350ns-20260825.md`](interpreted-invoke-cost-350ns-20260825.md)
