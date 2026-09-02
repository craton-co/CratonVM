# `VarHandle` writes and CAS have no fast path — 35-303x, and it is most of `java.util.concurrent`

## Status
**FIXED and CLOSED (2026-08-27).** Four steps, the first three landed on
2026-08-24 and the last on 2026-08-27: `set` is bound (698 000 native
dispatches -> 0, 246.5 -> 64.8 ns), the per-call global mutex is gone (**10.9x
at 24 threads**, and the anti-scaling with it), the CAS is served inside the
funnel (**4 396 000 served, 0 declined**, 1.27-1.39x), and the flat-throughput
residual this page named as its last open item is root-caused and fixed —
the volatile stripe pool was **64 one-byte mutexes in a single cache line**
(**1.64x at 24 threads**, and the curve scales again).

The per-op VarHandle costs that remain, and the composition gap under them, are
a separate page:
[`juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`](juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md).

What this page got WRONG: it predicted the CAS was "the only thing between this
and the 872x" composition gap. With the CAS fast path serving 3.84 M calls on
that workload, composition does not move — 4 CAS per chain at ~65 ns saved is
0.5% of a 54 us chain. `CompletableFuture` composition is not VarHandle-bound.
Where its 872x lived was an open question when that was written and is now
answered: two compile refusals, neither of them here. The defect this page IS
about was a gap in an existing optimisation rather than a bug —
`VARHANDLE_READ_DIRECT_FNS` bound READS of PRIMITIVE fields and nothing else.

## Severity
**HIGH, and broad.** `VarHandle` is the primitive under `CompletableFuture`,
`AbstractQueuedSynchronizer`, `ConcurrentHashMap`, `ForkJoinPool`,
`ConcurrentLinkedQueue` and `StampedLock`. Anything built on those pays it.

## The measurement

`HibfixVarHandleProbe`, single-threaded, each operation timed on its own after
a shared warmup:

| operation | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `VarHandle.compareAndSet`, reference field | 9.2 ns | **488.9 ns** | **53x** |
| `VarHandle.set`, reference field | 1.0 ns | **303.4 ns** | **303x** |
| `VarHandle.compareAndSet`, `int` field | 8.6 ns | **300.2 ns** | **35x** |
| `AtomicReference.compareAndSet` | 10.9 ns | **911.2 ns** | **84x** |
| `AtomicInteger.incrementAndGet` | 5.0 ns | 6.2 ns | **1.2x** |
| plain field store (baseline) | 1.1 ns | 10.3 ns | 9x |

**`AtomicInteger` is the control that makes this conclusive.** It has its own
intrinsic and is at parity, so this is not "atomics are slow" or "the box is
slow" — it is `VarHandle` specifically. A `VarHandle.set` of a reference costing
303 ns against a 10 ns plain store is a volatile store paying 30x an ordinary
one.

## Why: the fast path is reads-only, by construction

`jit/src/lib.rs` binds `VARHANDLE_READ_DIRECT_FNS` for

* modes `VARHANDLE_READ_MODES = ["get", "getVolatile", "getOpaque", "getAcquire"]`
* returns `VARHANDLE_READ_RETURNS = [Z B C S I J F D]`

and its own doc says of the reference kinds: "`L` and `[` are absent on
purpose". So the table has 32 slots, all of them reads of primitives. It was
built for netty's `RefCnt.isLiveNonVolatile`, which is `(int) VH.get(instance)`
— a read of an `int` — and it does that job.

Writes and CAS were never in scope. They still go through
`jit_invoke_dispatch`, paying the SATB flush, the reference-argument
forwarding, the site-key revalidation and two thread-local map probes that the
read path's own doc describes as the per-call floor it was created to remove.

## What it costs downstream

`HibfixComposeProbe2` composes 4 800 000 `CompletableFuture` chains with no
scheduler, no locks and no cross-thread handoff — each thread owns its futures
end to end:

| | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| 4.8 M compose chains | **412 ms** | **359 197 ms** | **~872x** |

`wrong=0` throughout: this is purely cost, not a correctness defect. A stack
profile of that run puts **34.0% in `CompletableFuture.tryPushStack`**, which is

```java
Completion h = stack;
NEXT.set(c, h);                        // VarHandle set, reference field
return STACK.compareAndSet(this, h, c);   // VarHandle CAS, reference field
```

with `UniCompose.tryFire` at 21.4%, `completeRelay` at 12.4% and
`uniComposeStage` at 8.0% behind it. Each thread owns its futures, so that CAS
is **uncontended and succeeds on the first attempt** — a third of the time in
an uncontended CAS is the primitive, not the algorithm. `ForkJoinTask.casStatus`
and `compareAndSetForkJoinTaskTag` appear lower down for the same reason.

## Step 1 landed: `set` is bound (2026-08-24)

The write modes now have the bind the read modes have had:
`VARHANDLE_WRITE_DIRECT_FNS`, 36 slots over
`["set", "setVolatile", "setRelease", "setOpaque"]` x
`[Z B C S I J F D L]`, recognised at BOTH the single-pass and the OSR door.

**References are in scope here, and that is not an oversight in the read
table's direction.** The read bind excludes `L`/`[` because a reference RETURN
must be published as a handoff root before the caller can store it, and the
direct arm takes no thread borrow to publish one with. A write has no return:
the reference travels INWARD, in a register the compiled caller's own frame
already describes, so there is no window to root across. The store itself goes
through `set_field_volatile_as`, which routes to the collector's own inherent
write barrier rather than an open-coded one — so this arm and the interpreter's
`putfield` tell the GC the same story.

Measured on ONE binary with `CRATONVM_JIT_VARHANDLE_WRITE_DIRECT_HELPERS`:

| | bind off | bind on |
|---|---:|---:|
| `VarHandle.set` NATIVE dispatches (census) | **698 000** | **0** |
| `VarHandle.set` reference | 246.5 ns | **64.8 ns** |
| `VarHandle.CAS int` (control, unbound) | 233.8 ns | 239.5 ns |
| `VarHandle.get` natives (control) | 500 000 | 500 000 |
| `compareAndSet` natives (control) | 1 396 000 | 1 396 000 |

The controls are what make it a bind and not a coincidence: every unbound
operation is unchanged, and the probe's self-check values are identical in both
arms. 2097 jit tests, 2610 vm tests and 71/71 regression vectors pass.

### The census is what caught the first version being inert

Bound in the single-pass ladder ALONE, the numbers were: 698 000 native
dispatches with the bind on, 698 000 with it off. Identical. The site counter
said "bound"; the census said "moved nothing". The probe's stores are in a
`main` loop, and **a loop body is an OSR compilation** — which the read bind's
own comment says in as many words about its OSR twin. A timing A/B alone would
have read as noise and been believed.

(A second mistake the same day, for the same file: the OSR block first spliced
INSIDE the read bind's `if let`, immediately after its `continue;` —
unreachable, and it compiled cleanly because the two closing braces landed on
the far side of it. `cargo check` passing proved nothing; reading the nesting
did.)

### What step 1 does NOT buy

`HibfixComposeProbe2` moved **2.5%** (139.8 s -> 136.4 s), and that is the
honest headline for the downstream workload. `tryPushStack` is `NEXT.set(c, h)`
**and** `STACK.compareAndSet(this, h, c)`; only the first is bound, and the CAS
is the more expensive half (489 ns against 303 ns), with more CAS behind it in
`UniCompose.tryFire` and `completeRelay`. Composition is CAS-dominated, so it
stays slow until step 2.

The remaining 64.8 ns is also still 65x HotSpot's 1.0 ns. Two costs are left in
the helper itself, and the second is the bigger prize:

## The per-call global mutex is gone (2026-08-24)

`vh_meta_table` is ONE process-global `parking_lot::Mutex` around one map, and
every `VarHandle` operation took it. That is not a per-op cost, it is a
scalability wall: measured with `HibfixVarHandleScale`, whose threads each own
their own object and their own field so there is **no contention on the data**,
throughput went DOWN with threads.

Both doors into the table are now memoised per thread, guarded by a
`VH_META_GENERATION` counter that every mutation bumps
(`vh_meta_put`, `vh_meta_update_field_index`), each bumping AFTER dropping the
lock so no reader can memoise a pre-change answer against a post-change
generation:

* `varhandle_instance_field_plan` — the JIT read and write fast paths;
* `vh_meta_get` — the generic native funnel.

| threads | before | after | gain |
|---:|---:|---:|---:|
| 1 | 8 313 819 ops/s | 9 679 449 | 1.16x |
| 4 | 6 287 026 | 7 760 197 | 1.23x |
| 8 | 1 521 043 | 7 466 459 | **4.9x** |
| 16 | 848 673 | 7 446 187 | **8.8x** |
| 24 | 603 694 | 6 573 526 | **10.9x** |

The collapse is gone: throughput holds ~7M ops/s from 4 threads to 24 instead
of falling to 0.07x of single-threaded. It was FLAT rather than scaling after
this, so a shared bottleneck remained; this page named the volatile store's
stripe lock and `java_identity_hash` as the candidates. **It was the stripe
lock, and not for the reason the name suggests** — see "The stripe pool was one
cache line" below, which closes that residual.

71/71 regression vectors and 4149 native-builtins tests pass.

### Memoising only the fast paths did nothing for composition, and that is how the second door was found

The plan memo alone took the scaling probe from 0.07x to 0.68x at 24 threads
and moved `CompletableFuture` composition by **zero**. Composition is
CAS-dominated, CAS has no direct bind, and the generic native reaches the table
through `vh_meta_get` — a different function, the same mutex. One lock, two
callers, and only one of them covered. The gap between the two measurements is
what exposed it; either number alone would have been read as a result.

### What the lock was NOT

With both doors memoised, composition improved ~9.5% (90.3 s -> 81.8 s on the
24-thread probe) and no more. **The mutex was the SCALING problem, not the
composition bottleneck.** What is left in composition is the CAS's own funnel
overhead — the SATB flush, the reference-argument forwarding, the site-key
revalidation and the two thread-local map probes that a direct bind exists to
skip. That is step 2, and it is now the only thing between this and the 872x.

### A note on reading the per-op numbers

One run after the change showed `VarHandle.set` at 99.4 ns against the 64.8 ns
measured before it — apparently a regression. The unrelated controls had moved
too (plain field store 15.7 -> 19.8 ns, `AtomicInteger` 6.4 -> 10.7 ns), i.e.
the box was ~1.5x busier. Only back-to-back comparisons on the same binary pair
are usable here; the scaling table above is one.

## The stripe pool was one cache line (2026-08-27), and that was the flat curve

With the `vh_meta_table` convoy gone, `HibfixVarHandleScale` still did not
scale: each thread owns its own object and its own field, so there is no
contention on the data and none left on the metadata, and throughput still
stopped rising past 4 threads. This page nominated two candidates. It was the
first one, by a mechanism its own name hides.

`parking_lot::Mutex<()>` is **one byte** — its `RawMutex` is an `AtomicU8` and
the `()` payload is zero-sized. So `[parking_lot::Mutex<()>; 64]`, which is what
the pool was, occupied exactly **64 bytes: one cache line, for every stripe of
every object of every field**. Striping by `(obj_ref, index)` removed the
LOGICAL contention — two threads rarely pick the same stripe — and left the
HARDWARE contention completely untouched: a `lock()`/`unlock()` pair is two
atomic read-modify-writes, each of which takes that single line exclusive, so N
threads storing to N different volatile fields of N different objects serialised
on the coherence protocol exactly as if the pool had one stripe.

The comment that sat above it reasoned about "false-sharing" meaning stripe
COLLISION, and put the pool's whole size at "64 x ~5 bytes is negligible" —
which is precisely the property that made every stripe share one line. Each
stripe is now `#[repr(align(128))]` (128 rather than 64 because x86-64's
adjacent-cache-line prefetcher pulls lines in pairs); the whole pool is 8 KiB.

Measured on an idle host, two frozen binaries differing ONLY in the padding,
ABBA-interleaved, three repetitions each, medians:

| threads | unpadded | padded | gain |
|---:|---:|---:|---:|
| 1 | 17 630 000 ops/s | 17 620 000 | **1.00x** |
| 2 | 14 920 000 | 18 230 000 | 1.22x |
| 4 | 22 190 000 | 25 820 000 | 1.16x |
| 8 | 27 870 000 | 40 550 000 | 1.46x |
| 16 | 29 480 000 | 43 170 000 | 1.46x |
| 24 | 31 120 000 | 50 970 000 | **1.64x** |

**The single-thread row is the control that makes this a cache-line result.** A
cache line cannot be contended by one thread, and one thread measures 1.00x —
17.63 M against 17.62 M, closer than the run-to-run spread of either arm. Every
gain appears exactly where the mechanism predicts it: nowhere at 1 thread, and
growing with the number of threads. Scaling `vs 1 thread` goes from 1.76x to
2.88x at 24 threads, and the 2-thread arm stops being SLOWER than 1 thread
(0.85x -> 1.04x).

`gc/src/collector.rs` carries a gate for it that asserts both halves — that a
bare `Mutex<()>` is tiny (the reason padding is needed, and the thing that would
make a future "simplification" back to `[Mutex<()>; N]` look harmless) and that
each stripe both starts on and OCCUPIES its own 128 bytes.

### What HotSpot is not a baseline for here

`HibfixVarHandleScale` on HotSpot reports 257 M ops/s at one thread and
**42 000 M ops/s at 8-24 threads** — 164x from one thread on an eight-core box,
which is not a speedup that exists. The probe stores into an object nothing
subsequently reads, so C2 deletes the loop. The cross-VM ratio on this probe is
meaningless; the CratonVM-against-CratonVM comparison above, on one probe and
two binaries that differ in one attribute, is the measurement.

## The CAS is served inside the funnel now (2026-08-24), and it does NOT fix composition

### The two directions were not symmetric, and that was the whole cost

Decomposing the funnel by arity with the two kill switches, on one binary:

| | bound | funnelled | funnel cost |
|---|---:|---:|---:|
| `get int` (1 coordinate) | 46.2 ns | 94.8 ns | **~49 ns** |
| `set` (2 arguments) | 64.8 ns | 246.5 ns | **~182 ns** |

A funnelled READ is cheap because it never reaches the native:
`try_varhandle_instance_field_read` catches it inside `jit_invoke_dispatch`.
There was no write or CAS equivalent, so a funnelled CAS ran the whole
`varhandle_compare_and_set` — its segment-handle probe, its FFM-layout probe,
its byte-view probe and its array probe — before reaching the instance-field
case.

`try_varhandle_instance_field_cas` is that missing twin. It needs no codegen,
no stack-argument setup and no ABI work, and unlike a compile-time bind it also
covers the sites the JIT declines and every interpreter dispatch.

`VmExec::compare_and_swap_field` was extracted into
`compare_and_swap_field_shared` so both routes call one implementation — the
same argument the read path makes for `varhandle_instance_field_read_bits`, and
a CAS has more to get wrong: the SATB pre-barrier fires on `expected` BEFORE the
store and the post `write_barrier` only on success.

| | fast path off | on |
|---|---:|---:|
| `field CAS in-funnel` | `served=0 declined=0` | **`served=4 396 000 declined=0`** |
| `VarHandle.CAS int` | 232.4 ns | **167.6 ns** (1.39x) |
| `VarHandle.CAS reference` | 363.1 ns | **285.3 ns** (1.27x) |

Every CAS served, zero declines, references included; correctly inert when
switched off; baselines stable at 15.9 / 15.7 ns. 2099 jit tests, 71/71
regression vectors.

### The census could NOT verify this one, unlike the `set` bind

`--dump-native-registry` reports the SAME `VarHandle.compareAndSet` count with
the fast path on and off — 4 396 000 either way — because a served CAS still
calls `count_jit_native_dispatch`, exactly as a served read does. The `set`
bind was verifiable that way (698 000 -> 0) precisely because a thin direct call
never enters the funnel at all. Here the hit/decline pair is the only
instrument, and it had to be wired into the shutdown report before any of the
above could be claimed.

### It does not speed up composition, and that refutes what this page predicted

An earlier revision said the CAS was "the only thing between this and the 872x".
It is not. Interleaved on a quiet box:

| arm | composition | CAS served |
|---|---:|---:|
| off | 47 149 ms | 0 |
| on | 51 346 ms | 3 839 190 |
| off | 57 630 ms | 0 |
| on | 51 700 ms | 3 839 196 |

The fast path engages FULLY on composition — 3.84 M CAS served, zero declined —
and composition does not move. The arithmetic says why: 3 839 190 CAS over
960 000 chains is **4 CAS per chain**, saving ~65 ns each, or ~260 ns against a
chain that costs ~54 us. **0.5%.**

So `CompletableFuture` composition is not VarHandle-bound, and the 872x is
somewhere else entirely. The 34%-in-`tryPushStack` profile reading that
motivated this did not survive the kill switch: a Java-frame sampler attributes
the whole of a native call to the Java frame that made it, so "34% in
tryPushStack" was never evidence about which part of that call was expensive.

(The off arm alone spans 47.1 s to 57.6 s — a 22% spread — so a 2% effect is
not measurable on this host regardless. Any future composition claim needs many
runs, not two.)

### Where the 872x actually is: FOUND, and it is not VarHandle at all

A native profile settled it: composition runs INTERPRETED (92.35% in the VM
binary, **0.32% in JIT code**), because its two hottest methods —
`UniCompose.tryFire` and `UniRelay.tryFire` — are ASKED and REFUSED by the
compiler with `reason=unrecorded`. (The native-shadow seal was the first
explanation and is retracted: turning it off changes nothing.) On
the binary current at the time `--nojit` was marginally FASTER than JIT, which
is what "never compiled" looks like.

**That was ANSWERED and FIXED on 2026-08-27**, and it was not the native-shadow
seal either: `UniCompose.tryFire` and `UniRelay.tryFire` were refused by a
`ForkJoinTask`-subclass blocklist whose own comment claimed it left
`CompletableFuture` alone, plus a `dup_x2` shape the single-pass backend could
not prove. Composition went 3.95x, and 446x -> 113x against HotSpot. See
`performance/completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`.

That is why every fix on this page moved the primitive and not the workload.
The old text below is kept as written:

What is now excluded: the global mutex (removed, 10.9x on scaling), the CAS
funnel (served in full, 0.5%), the `set` funnel (bound, 698 000 dispatches
eliminated), `thenCompose` relay correctness and the Vert.x bridge (both clean
at volume). `HibfixComposeProbe2` is still a deterministic 412 ms vs 359 s
reproducer with no database and no flake, and profiling it by Java frame has now
been shown to mislead — the next attempt needs a NATIVE profile.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
java -cp . HibfixVarHandleProbe                      # HotSpot control
cratonvm --java-home <jdk> -Dprobe.iters=2000000 -cp . HibfixVarHandleProbe
```

`HibfixComposeProbe2` is the downstream reproducer — deterministic, no
database, no flake, 412 ms against 359 s.

## Related

- `performance/completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`
  — where the 872x actually was. Every fix on this page moved the primitive and
  not the workload, and that page is the reason.
- [`juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`](juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md)
  — what is left: `VarHandle` operations at 9-50x and `AtomicReference.CAS` at
  114x, measured against the same `AtomicInteger`-at-parity control this page
  used.
- [`../../known-issues/hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`](../../known-issues/hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md)
  §5.9 — where this was found. The reactive composition cost there is this
  defect, and it is the likely enabling condition for that page's correctness
  failure: a sequence-allocation race HotSpot settles in microseconds is run
  through machinery two orders of magnitude slower.
- [`../../known-issues/perf/bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](../../known-issues/perf/bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
  — the same shape of finding: a hot JDK primitive left on the generic funnel.
