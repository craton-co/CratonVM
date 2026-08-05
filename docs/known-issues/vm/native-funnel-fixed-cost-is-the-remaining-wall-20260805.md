# `safe_native_call`'s fixed cost is now the wall — and nobody has profiled it

| | |
|---|---|
| **Status** | OPEN — measured, bounded, and never profiled |
| **Severity** | high — it is the last large term on `java.util.concurrent`, and it is per-call on every native the VM has |
| **Opened** | 2026-08-05, closing [`uncontended-reentrantlock-pair-mostly-unattributed`](../../internal/uncontended-reentrantlock-pair-mostly-unattributed-RETIRED-20260805.md) |
| **Inherits** | `TestAsyncMessagesPerformance.testAsyncTiming`, and the `SmokeTests` concurrency ceiling as a suspected relative |

## What is left, and why it is the funnel

An uncontended `ReentrantLock.lock()`+`unlock()` pair is **~1,200 ns** against
HotSpot's 12.3 ns. It makes exactly three native calls, and after 2026-08-05
they are the whole of it:

| | ns | share |
|---|---:|---:|
| `setExclusiveOwnerThread` x2 | ~530-680 | ~45% |
| `Unsafe.compareAndSetInt` (measured via `AtomicInteger` CAS+set) | ~400-490 | ~37% |
| `Thread.currentThread()` x2 — served without a native at all | ~26 | 2% |
| unattributed | ~110-220 | 9-17% |

~250-300 ns per native call, and the **bodies are no longer the cost**:
`setExclusiveOwnerThread` is now a memoized field-index write plus an O(1)
index update, and the CAS reaches a real CAS. Neither re-resolves anything.
What is left around them is `safe_native_call_impl`: the argument copy and
GC-forwarding barrier, per-argument pinning into `native_pin_roots`, the STW
probe, two GC-pressure probes, two `thread_state::record_transition` calls,
`catch_unwind`, the JNI pending-exception drain, and the pin-watermark unwind.

The bound is tight, from `probes/AqsAttributionProbe.java` on one quiet run:

| | ns/op |
|---|---:|
| ordinary Java call | 7.9 |
| `Thread.onSpinWait` — answered inline, never enters anything | ~6.5 |
| `AtomicInteger.get` — **leaf**: site-cached, `safe_native_call_leaf` | 233 |
| `AtomicInteger.compareAndSet` — non-leaf: site-cached, full funnel | ~400-490 |

So the funnel proper is worth roughly **170-260 ns** (non-leaf minus leaf), and
the *leaf* path still costs **~225 ns over an ordinary Java call** — which is
its own question, since a leaf call is a thread-local map probe, an argument
decode, `catch_unwind`, and the callback.

## Why this is not just "item 2, again"

The retired `native-call-funnel-is-the-per-call-floor` closed its item 2 as
"answered, not done", on the grounds that ARCH-A3 had already collapsed the
funnel's thirteen diagnostic gates into one word and what remained needed a
profile. That was right then and it is still right — but the ranking has
changed. When that was written the funnel was one term among several; the
cascade, the by-name field resolve and the registry walk have all since been
removed, and it is now the **largest** one.

**Nobody has profiled it. This document asserts a bound, not a line.** Anyone
taking it on should produce a per-line or per-step attribution first, and
should expect the answer to be unevenly distributed — the two `record_transition`
calls, `catch_unwind`'s landing pads, and the pin push/truncate are each
plausible and none has been weighed.

## Two concrete directions, neither validated

* **Let more natives be leaves.** `setExclusiveOwnerThread` cannot be one today
  because it takes locks (the `synchronizer_owner` index, the registry read
  lock, a per-thread mutex), and leaf contract item 2 forbids blocking. Giving
  `JvmThread` a direct `Arc<Mutex<Vec<ObjectRef>>>` handle to its own entry's
  synchronizer list — the pattern `set_gc_block_state` already uses — would
  remove the registry lookup and leave one short uncontended mutex. Whether
  that is enough to satisfy the contract, or whether the contract should
  distinguish "blocks on something a safepoint can hold" from "takes a short
  internal mutex", is the design question and it should be settled explicitly
  rather than by relaxing the wording.
* **Shrink the leaf path itself.** 233 ns for a field read is not the funnel; it
  is the site-cache probe (`FxHashMap` on a 2-word key), `decode_dispatch_values`,
  and `coerce_native_return`. A monomorphic site could cache the decoded shape.

## Do not re-run these

Ruled out and measured — see
`docs/internal/aqs-thread-handoff-latency-RETIRED-20260805.md`:

* the AQS pre-park spin (the uncontended path never spins)
* "16 nested Java calls at a per-call floor" (a Java call is 7.9 ns; 16 is ~130 ns)
* OSR starving callees (`tier-osr-backedge=2000000000` is inert)
* `CRATONVM_JIT=direct-callee-calls`, `ir-direct-call`, `guarded-virtual-inline`
* JIT admission — the lock methods do compile
* **narrowing the `java/util/` virtual tier-up exclusion** — a ~30% *regression*
  for exactly these bodies (0.77x, 0.76x, `probes/JavaUtilTierUpExclusionProbe.java`)
* the Θ(threads) scan in `setExclusiveOwnerThread`'s JMX index (fixed 2026-08-05,
  `probes/AqsOwnerScaleProbe.java` is the differential)

## Reproduction

```powershell
$jdk = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& "$jdk\bin\javac.exe" -d out probes\AqsAttributionProbe.java
& "$jdk\bin\java.exe" -cp out AqsAttributionProbe            # HotSpot control
$env:CRATONVM_DBG = 'intrinsic-stats'
& <cratonvm.exe> --java-home $jdk -cp out AqsAttributionProbe
```

**Read `empty instance call (the scale)` first.** It was 8.4 ns on the machine
the original 10,502 ns figure came from and 23-27 ns on the machine these
numbers came from; comparing absolutes across hosts is how the "two thirds
unattributed" arithmetic went wrong in the first place. The probe prints
"VOID, rerun it" when the parts out-total the pair, which is what host drift
inside a single run looks like.
