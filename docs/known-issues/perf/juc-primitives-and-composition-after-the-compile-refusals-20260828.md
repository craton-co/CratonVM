# `java.util.concurrent` primitives are still 12-37x, and composition 18x, with every compile refusal gone

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

## Closed since this page was filed

**`VarHandle.compareAndSet` on a reference field — was the top item below, now
bound (2026-08-28).** 156.8 ns -> **52.1 ns**, a **3.01x**, landing on the
bound-`set` floor of 51.1. Against HotSpot's 5.3 that row goes **29.6x ->
9.8x**, and this page's own composite row for it goes 35x -> **16.9x**.

It was served from inside the funnel rather than bound, and the reason it was
not bound — "five arguments, and Windows' ARG_REGS is four, so it needs the
stack-argument setup this bind does not" — had stopped being true before anyone
read it: `emit_stack_arg_setup` has marshalled exactly that since Round-8
wave-3. See
`performance/varhandle-compareandset-thin-direct-bind-FIXED-20260828.md`.

> **The first revision of this page was measured on a busier box and one of its
> conclusions did not survive.** It put the range at 9-114x and named
> `AtomicReference.compareAndSet`'s synthetic stub as a 710 ns tax with an
> obvious fix. Re-measured on an idle box, that range was 16-40x and the stub
> was not a tax at all — see "What was tried and refuted". Both tables were six
> interleaved runs; the difference was the box, which is exactly the failure
> mode the `VarHandle` page warned about and this page then walked into. The
> table below is the third measurement, on the merged binary with the CAS bind
> in, and every row's spread is under 3%.

**`AtomicReference.compareAndSet`'s synthetic stub — was the worst row in the
table below at 37x, now de-registered (2026-08-29).** The refutation this page
recorded in "What was tried and refuted" was correct AT THE TIME (stub 257 ns,
real bytecode 289 ns) but stopped being true the moment the CAS bind above
landed: real-JDK `AtomicReference.compareAndSet` is one line delegating to
`VarHandle.compareAndSet`, and that line is now thin-direct-bound instead of
funnel-served. Six interleaved runs, kill switch as the only difference,
`--dump-native-registry` confirmed no native registered for the tuple either
way (so this genuinely reaches real bytecode, not a second stub — see the
note in "What was tried and refuted" about the first attempt gating the wrong
registrar):

| | stub kept | stub dropped |
|---|---:|---:|
| `AtomicReference.compareAndSet` | 519.5-543.8 (**531.7**) | 331.4-344.0 (**332.0**) |
| `VarHandle.compareAndSet` reference (control, composite) | 256.5-261.1 | 257.7-272.6 |
| `AtomicInteger.increment` (control) | 5.8-6.0 | 5.5-6.0 |

A **1.6x** win, with both controls unmoved across arms — this is the change,
not the box. Measured on a moderately busy box (load 10-16, not the idle box
the rest of this page insists on), so the table above still needs a clean
idle-box re-measurement pass with this row included; the ratio (not the
absolute ns) is what this measurement can actually support. The stub
(`native_atomic_ref_cas` in `native-builtins/src/util_concurrent_ext.rs`) and
its registration are removed, not just gated off — see the comment left in
its place for the arithmetic and the evidence.

## Severity
**HIGH and broad**, for the same reason the `VarHandle` page was: these are the
primitives under `CompletableFuture`, `AbstractQueuedSynchronizer`,
`ConcurrentHashMap`, `ForkJoinPool`, `ConcurrentLinkedQueue` and `StampedLock`.

## The measurement

> **The CAS rows of this probe are COMPOSITES.** Each iteration does a
> `VarHandle.get` and *then* the CAS, so "`VarHandle.compareAndSet` reference
> 231 ns" is a 77 ns read plus a ~154 ns CAS. HotSpot's rows compose the same
> way, so the RATIO is sound; the attribution is not, and a profile taken
> against these rows measures two operations. `VhCasProbe` (with the rest under
> `apps/probes/`) carries the expected value in a Java local and times the CAS
> alone — use that one to attribute, and this one for continuity with the
> numbers above.

`HibfixVarHandleProbe`, `-Dprobe.iters=2000000`, single-threaded, idle host
(load 0.06), six runs of each VM **interleaved**. Medians, with min and max so
the spread is visible — every row's spread is under 6%, which is what says the
box was quiet:

| operation | CratonVM min-max (median, ns) | HotSpot | ratio |
|---|---:|---:|---:|
| `AtomicReference.compareAndSet` | 263.4-269.8 (**266.8**) | 7.3 | **37x** |
| `VarHandle.get` reference | 76.3-77.7 (**76.8**) | 3.2 | **24x** |
| `VarHandle.compareAndSet` reference † | 125.0-133.1 (**128.1**) | 7.6 | 17x |
| `VarHandle.get` int | 46.2-48.4 (**46.3**) | 3.2 | 15x |
| `VarHandle.set` reference | 50.8-53.5 (**51.3**) | 3.6 | 14x |
| `VarHandle.compareAndSet` int † | 87.6-89.3 (**88.2**) | 7.4 | 12x |
| plain field store (baseline) | 5.8-6.2 (**5.9**) | 3.1 | 1.9x |
| `AtomicInteger.incrementAndGet` | 4.7-4.9 (**4.7**) | 6.3 | **0.7x** |

† a composite — see the note above. Subtracting the matching `get` row gives
128.1 - 76.8 = **51.3** for the reference CAS alone, against `VhCasProbe`'s
independent 52.1 for the same operation in the same window. Two probes with
different shapes agreeing to 1.5% is the reason to trust either.

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

> **This conclusion was correct when written and stopped being true on
> 2026-08-28.** The arithmetic above is "a `VarHandle` CAS at ~233 ns plus a
> frame" — once the CAS bind took that 233 ns down to ~53.6, the same
> arithmetic points the other way, and it now measures out that way too. See
> "Closed since this page was filed": the stub is removed. Keep this section
> as the record of why the FIRST measurement was wrong (the gate on the wrong
> registrar) and the SECOND was right for its binary, not as today's answer.

## Where to look first

1. **`VarHandle.get` on a reference field, 77.0 ns against 2.4 — and against
   41.3 for the same read of an `int`.** Now the top row that is not a wrapper
   of another row. `VARHANDLE_READ_DIRECT_FNS` binds reads of PRIMITIVE fields
   only, and its comment says `L` and `[` "are absent on purpose": a reference
   RETURN must be published as a handoff root before the caller can store it,
   and the direct arm takes no thread borrow to publish one with. That
   justification is sound and the 1.9x over the `int` read is its measured
   price. It is also the LAST of the three access directions still unbound —
   `set` was bound on 2026-08-24 and `compareAndSet` on 2026-08-28, both on the
   argument that a reference travelling INWARD needs no root, and a CAS proved
   a `Z`-returning bind carries no reference out either. A read is the one that
   genuinely returns one, so this is the hard case rather than an oversight.
2. **A NATIVE profile of the composition probe with everything compiled.** The
   last one was taken when composition was 92.35% interpreted, so it measured
   the interpreter and nothing else. The CAS bind moved composition 1.10x,
   almost exactly the 9.6% its 10.47 CAS per chain predicted — which is
   reassuring about the accounting and leaves ~90% of the chain unattributed.
   The `AtomicReference` stub removal closed above should move it again by
   roughly however much `AtomicLong.incrementAndGet` (1.00 per chain, still
   funnel-served — a different class, not touched by either fix this page has
   landed) does NOT explain of the remaining ~90%; still nobody's profiled the
   current shape end to end.

## What is already excluded, with the evidence

Do not re-derive these.

* **Compile refusals.** `hot_but_stuck_in_interpreter=0` on both probes.
  Restoring either of the two fixed refusals with its kill switch costs 2.19x
  and 2.88x, so the instrument works and reads zero.
* **The `VarHandle` CAS funnel entry.** Bound 2026-08-28; the funnel arm still
  serves the interpreter and the declined sites, and the two share one
  implementation.
* **The native-shadow caller seal.** `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0`
  changes the compile count by one method and the runtime by nothing. Its blast
  radius is real (a median 1 010 methods sealed per class on the
  hibernate-reactive suite) and its ceiling on this workload is ~0.
* **The `vh_meta_table` global mutex.** Removed; 10.9x on thread scaling.
* **The volatile stripe pool sharing one cache line.** Fixed; 1.64x at 24
  threads on `HibfixVarHandleScale`, and exactly 1.00x at one thread.
* **The CAS funnel.** `try_varhandle_instance_field_cas` serves the instance
  case: 3 839 190 served, 0 declined on the composition workload. Worth 0.5%.
* **The funnel's own descriptor re-parse.** `varhandle_cas_operand_kinds` was
  re-walking the call site's descriptor string on every declined/unbound CAS
  (`CharSearcher::next_match` at 3.84% of a native profile taken before the
  bind existed to take the hot sites away from this route). Memoized per call
  site (2026-08-29, same one-slot thread-local idiom as
  `native-builtins::lang_invoke::VH_PLAN_MEMO`); re-profiled after, the same
  function drops to 0.50%. Worth little on THIS page's own probes now that the
  bind serves the hot sites, but free and correct, so it stays for whatever
  declined sites keep reaching the funnel.
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
