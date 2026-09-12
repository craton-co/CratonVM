# AQS is 13-26x HotSpot on handoffs — RETIRED 2026-08-05

| | |
|---|---|
| **Status** | RETIRED — all three of its items are closed, two by code and one by measurement |
| **Opened** | 2026-08-03, root-causing `TestAsyncMessagesPerformance` SEQ2 |
| **Closed by** | `perf/aqs-native-funnel-20260804` |
| **Owned** | the residue of the retired `websocket-async-send-interframe-latency` doc |
| **Residual, still OPEN** | [`uncontended-reentrantlock-pair-mostly-unattributed` — ATTRIBUTED and RETIRED 2026-08-05](uncontended-reentrantlock-pair-mostly-unattributed-RETIRED-20260805.md), residual now [`native-funnel-fixed-cost-is-the-remaining-wall`](../known-issues/vm/native-funnel-fixed-cost-is-the-remaining-wall-20260805.md) |

This document was corrected twice while it was open, and it is worth saying
plainly that **the second correction was also incomplete**. Revision 1 blamed
the pre-park spin; revision 2 blamed the per-call floor and handed the fix to
[`native-call-funnel-is-the-per-call-floor`](native-call-funnel-is-the-per-call-floor-RETIRED-20260805.md).
Revision 2's arithmetic does not close its own gap, and that is recorded below
rather than buried — it is the reason this retires with a named successor
instead of a clean bill.

## Item 1 — inlining / the per-call floor. DONE, on the half that was open.

The floor doc's item 1 ("extend the funnel bypass to the JIT side … as a class
of leaf natives, not another hand-written case") is implemented. See that
doc's closeout for the mechanism, the A-B-B-A measurement and the counter that
proves the path fires. Headline: five natives that this document's own probes
measure went **3.9x-10.5x** faster from compiled code, and
`Thread.currentThread()` — which the JDK calls **twice per uncontended
`lock()`/`unlock()` pair** — went 564 ns → 53 ns.

The *bimorphic splicing / deopt-capable guard* half of item 1 (profile-guided
inlining §8) is untouched and stays where it lives, in that design doc. It was
never this document's to carry.

## Item 2 — the trivial atomic accessors. DONE.

> "`AtomicInteger.get()` costs **969 ns**, *more than an empty bytecode call*,
> for a body that is `return value;` on a volatile int … for the plain
> `get`/`set` accessors the native is now strictly worse than the bytecode it
> replaces."

Correct, and now fixed — but not by removing the natives. `AtomicInteger` /
`AtomicLong` `get` / `set` / `lazySet` / `intValue` / `longValue` are registered
as **leaf** natives, so they skip both `invoke_or_native` and the funnel.
`AtomicInteger.get()` measures **1026 → 233 ns** (4.4x), and 183 ns on the
quieter of two interleaved runs.

The document's own caveat — "it also needs the real `value` field to be live on
these synthetic-layout objects, which is unverified" — did not need resolving,
because nothing here changes *which* body runs. The registered native still
reads field 0 through `get_field_volatile`, exactly as before; only the wrapper
around it is gone. That is a strictly smaller change than the one the item
proposed, and it keeps the CAS/arithmetic members the item wanted to preserve
untouched — they are deliberately excluded from the leaf set, because they take
the monitor table's per-object CAS lock.

## Item 3 — narrow the `java/util/` tier-up exclusion. MEASURED. Do not.

> "The `java/util/` tier-up exclusion is miscalibrated even though it is not the
> bottleneck here … Worth narrowing on its own merits, with its own
> measurement."

It got its own measurement, and the measurement says the narrowing would be a
**regression**. `probes/JavaUtilTierUpExclusionProbe.java` compares an
uncontended lock/unlock loop on a `ReentrantLock` (receiver inside
`java/util/`, so virtual tier-up is suppressed) against a user subclass running
byte-for-byte the same inherited bodies (receiver outside `java/util/`, so
tier-up is admitted), each with its own monomorphic loop method:

| | run 1 | run 2 |
|---|---:|---:|
| `ReentrantLock` — tier-up SUPPRESSED | 30,653 ns/op | 28,754 ns/op |
| `MyLock extends ReentrantLock` — tier-up ADMITTED | 39,954 ns/op | 37,904 ns/op |
| subclass speed | **0.77x** | **0.76x** |

Admitting these bodies to the cached-virtual tier-up costs ~30%, reproducibly.
So the prefix is indeed miscalibrated *as a description* — its comment ties it
to a Spring collections graph and it catches all of `java.util.concurrent` as
collateral — but the collateral is currently doing the concurrency stack a
favour. The exclusion is left exactly as it is, and this row is closed as
"answered", not "deferred". Anyone re-opening it should reproduce these two
runs first; the probe already handles the polymorphic-call-site trap that made
an earlier cut of it read backwards.

> ### 2026-09-11: the two runs were re-attempted, and they do not reproduce.
>
> That instruction was followed by
> `performance/composition-native-callback-and-the-promotion-question-CLOSED-20260911.md`.
> The probe was rebuilt at `apps/probes/JavaUtilTierUpExclusionProbe.java`
> (`probes/` was deleted on 2026-08-29) with the arms INTERLEAVED and a median
> over reps, because a single A-then-B pair does not resolve this at the
> spread the rebuild shows.
>
> | | 2026-08-05 | 2026-09-11, 6 reps | 10 reps |
> |---|---:|---:|---:|
> | `ReentrantLock` | 30 653 / 28 754 ns/op | 1 346 | 1 408 |
> | `MyLock extends it` | 39 954 / 37 904 ns/op | 1 395 | 1 327 |
> | subclass speed | **0.77x / 0.76x** | **0.99x** [0.93-1.06] | **1.05x** [0.97-1.14] |
>
> The absolute numbers moved 21x in five weeks, so the 0.77x was measured
> against a VM that no longer exists. Priced properly — as a one-binary A/B on
> the BASE arm alone, which removes the "different class, different call site,
> different inlining" this two-receiver comparison also carries —
> `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL=1` reads **1.02x favourable with
> overlapping ranges**. There is no 30 % regression left to defend.
>
> The exclusion nevertheless stays, and this row stays "do not" — for the
> reason its own origin commit (cb563d707) gives and this page never quoted:
> the cached virtual route "can publish a stale receiver-specific entry and
> **then spin**". A spin is not a slowdown, and it is not what the 30 % number
> was ever evidence about. See that page's item 2 for what would have to be
> shown to flip it.

## The AQS-specific defect nobody had looked for

Not in the original document, found by reading rather than measuring, and then
measured:

**Every AQS ownership transition walked every registered thread and took every
registered thread's mutex.** `AbstractOwnableSynchronizer
.setExclusiveOwnerThread` is intercepted so `ThreadMXBean` can keep an
owned-synchronizer index, and the interception updated that index by scanning
`ThreadRegistry` and `retain`-ing one entry out of each thread's list. AQS
calls the setter on acquire (owner) and on release (null), so that is **twice
per uncontended `lock()`/`unlock()` pair** — and the cost of one uncontended
lock therefore grew with the number of threads in the VM.

This is invisible to `probes/AqsBreakdownProbe.java`, which is single-threaded —
which is exactly why a document built on that probe could conclude "there is no
AQS-specific defect". `probes/AqsOwnerScaleProbe.java` is the differential that
sees it, and it FAILS on the pre-fix binary:

| live threads | lock/unlock | `synchronized` (control) |
|---:|---:|---:|
| 1 | 51.6 us | 1.11 us |
| 16 | 60.4 us | 1.17 us |
| 64 | 64.8 us | 1.13 us |
| 256 | **107.0 us** | 1.52 us |

HotSpot is flat at 14-16 ns across the same range. The `synchronized` control —
a monitor is one bytecode and never reaches AQS — is flat here too, which is
what rules out host load as the explanation.

A reverse index (`ThreadRegistry::synchronizer_owner`) replaces the scan, so a
transition touches at most two threads' lists. It is keyed by a heap address,
so `update_thread_objs_after_gc` rekeys it in the same pass that remaps the
lists; without that, the next transition for a relocated lock would miss its
previous owner and record it against two threads at once. Both properties are
pinned by unit tests in `vm/src/threading/thread_registry.rs`.

**Honesty about the after-measurement:** the host was carrying 2x drift by the
time the fixed binary existed, and the post-fix scale probe was not
reproducible to a useful precision — the baseline's own slope varied between
1.33x and 2.07x across re-runs. The fix is an algorithmic one (Θ(threads)
mutex acquisitions → at most 2) with the behaviour pinned by tests; it is not
claimed here as a measured speedup, and the residual slope the fixed binary
still shows at 256 threads is more likely the O(threads) safepoint census than
anything on this path. Re-run the probe on a quiet host before quoting a number.

## What is still open, and why this retires with a successor

Revision 2 replaced "16 Java calls at 431 ns" with "five native calls at
330-810 ns". Take that seriously and it does not add up either:

| | |
|---|---:|
| measured uncontended `lock()`+`unlock()` pair (this doc's table) | **10,502 ns** |
| five native calls at the floor doc's own 330-810 ns | ~2,500-4,000 ns |
| the 16 nested Java calls, at the corrected 8.4 ns each | ~138 ns |
| **unattributed** | **~6,400-7,900 ns, i.e. two thirds** |

Both corrections were arguments about the *biggest identified* term, and
neither checked whether the identified terms summed to the total. They do not.
Removing ~90% of the funnel cost from two of the five natives should therefore
move the pair by single-digit percent, and nothing in this session's runs
contradicts that.

So the pair is still tens of microseconds against HotSpot's 14.8 ns, and the
reason is **not** known. That is now its own document — with the arithmetic
above as its starting point, and a warning not to repeat the mistake of
attributing the total to the first expensive thing found on the path:
[`uncontended-reentrantlock-pair-mostly-unattributed` — ATTRIBUTED and RETIRED 2026-08-05](uncontended-reentrantlock-pair-mostly-unattributed-RETIRED-20260805.md), residual now [`native-funnel-fixed-cost-is-the-remaining-wall`](../known-issues/vm/native-funnel-fixed-cost-is-the-remaining-wall-20260805.md).

`TestAsyncMessagesPerformance.testAsyncTiming` is therefore **not** unblocked by
this work, and neither is the `SmokeTests` concurrency ceiling. They move to the
successor.

## The two traps, preserved

Both are still true and both are still in the probes' own comments:

1. **The harness cost more than the thing measured.** Driving each operation
   through a `(int) -> void` lambda put a ~2.2 us `invokeinterface` inside every
   rung. Every benchmark is an inline loop in its own method.
2. **A shared call site goes polymorphic.** One `lockUnlock(ReentrantLock, int)`
   called with two receivers made its `lock.lock()` site bimorphic, and a poly
   site costs ~6x a monomorphic one — the entire reason the subclass first
   appeared 2.5x slower. Each receiver class has its own loop method.

A third belongs beside them now, and it is not about Java:

3. **A fast path that is never installed looks exactly like one that is no
   faster.** The first cut of the JIT leaf path measured 0 hits with unchanged
   ns/op, because a gate refused every site in every configuration. The second
   measured 24M hits and one *unmoved* rung, with no refusal recorded for it —
   which is what revealed that compiled `invokevirtual` arrives at a different
   entry point (`jit_invoke_virtual_mic`) than compiled `invokestatic`. Count
   the hits AND count every refusal, by reason.

## Reproduction

```powershell
$jdk = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& "$jdk\bin\javac.exe" -d out probes\NativeShapeProbe.java probes\AqsBreakdownProbe.java `
                              probes\AqsOwnerScaleProbe.java probes\JavaUtilTierUpExclusionProbe.java
& "$jdk\bin\java.exe" -cp out NativeShapeProbe                 # HotSpot control
$env:CRATONVM_DBG = 'intrinsic-stats'
& <cratonvm.exe> --java-home $jdk -cp out NativeShapeProbe     # read the hit counter, then the ns/op
```

Interleave the arms and run both orders. Under background load every number
roughly triples and the ratios shift; the `control: no call` and `empty
instance call` rungs are the load scale, and a run whose scale moved between
its first and last rung is not a measurement.

---

## Original document, 2026-08-03

<details>
<summary>Reproduced from git history.</summary>

The full text is at `docs/known-issues/vm/aqs-thread-handoff-latency-20260803.md`
in git history (removed by the branch that produced this closeout). Its
measurement table — uncontended `ReentrantLock` lock+unlock at 10,502 ns against
HotSpot's 14.8, `synchronized` at only 47x rather than 710x, `AtomicInteger.get`
at 969 ns — is what every claim above is measured against, and the "levers ruled
out" table (OSR starvation, `direct-callee-calls`, `ir-direct-call`,
`guarded-virtual-inline`, all inert) still stands.

</details>
