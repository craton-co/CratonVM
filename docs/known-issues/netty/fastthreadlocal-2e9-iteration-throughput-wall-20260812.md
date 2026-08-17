# FastThreadLocalTest — a 2.1-billion-iteration loop against a per-allocation helper call

**Status: OPEN — throughput, not correctness.** Narrowed **2.1x** on
2026-08-17 and re-sized against current measurements. The class still does not
finish inside any reasonable per-class wall, so this page stays open; what
changed is that both of its open *questions* are now answered, and the one
remaining cost has a named blocker instead of a hypothesis.

The forensics — what was fixed, and why the allocation lever this page tested
twice was never going to move — are in
`fixed-suite-bugs/netty/osr-door-binds-ctor-and-the-inline-new-lever-is-inert-FIXED-20260817.md`.
Everything below is the *current* state. The long 08-12 / 08-13 measurement
history lives in that record rather than here, because every number in it has
since moved.

## What the test does

`io.netty.util.concurrent.FastThreadLocalTest.testConstructionWithIndex`:

```java
int ARRAY_LIST_CAPACITY_MAX_SIZE = Integer.MAX_VALUE - 8;
...
while (nextIndex.get() < ARRAY_LIST_CAPACITY_MAX_SIZE) {
    new FastThreadLocal<Boolean>();
}
```

`FastThreadLocal()` is `index = InternalThreadLocalMap.nextVariableIndex()`,
i.e. `nextIndex.getAndIncrement()` plus a bounds check. **2 147 483 639
iterations of (allocate one object + one atomic increment)** by construction —
there is no shortcut and nothing the VM can do to shorten the loop itself.

HotSpot JDK 25 runs the whole class in ~50 s.

The correctness half — the JIT eliding the constructor's write to the static
`nextIndex`, which made the loop non-terminating — was fixed 2026-08-12 and is
not at issue here. `lastVariableIndex()` advances by exactly the iteration
count on every arm measured below.

## Where it stands

Interleaved, same binary, `CRATONVM_NO_OSR_CTOR_BIND=1` as the off-arm:

| loop | before 08-17 | after | HotSpot |
|---|---|---|---|
| `new FastThreadLocal<Boolean>()` | 420-495 ns/op | **199-266 ns/op** | 7.3-7.8 ns/op |
| `new Object()` — allocation alone | ~110 ns/op | ~110 ns/op | ~1.8 ns/op |
| `nextIndex.getAndIncrement()` alone | ~5.7 ns/op | ~5.7 ns/op | ~5.0 ns/op |

The constructor dispatch is no longer the gap. **Allocation is**, and at
~110 ns/op it is ~236 s for this iteration count on its own — already past the
netty suite's 180 s per-class wall before anything else is counted.

## Why allocation costs what it does

Not because of the gating flags this page tested twice. `emit_inline_tlab_new`
**does not allocate inline**: its own comment records that the raw compiled
TLAB-cursor bump was routed back through the checked runtime helper after it
left a malformed young-space span under concurrent Elasticsearch merge churn.
Only the header writes are inline. So every `new` pays a helper call by
design, and `skip_helper` — which the 08-17 work made reachable from the OSR
door for the first time — only removes a *second* call
(`jit_post_tlab_init`), which measures neutral (111.4 vs 118.0 ns/op, three
interleaved rounds). It is available behind
`CRATONVM_JIT_REAL_NEW_SITE_FLAGS` and deliberately off.

Inside the helper, `ZgcRealHeap::alloc_raw_tlab` is 22% of a whole-process
profile of this loop, and its fast path is not a pointer bump: a thread-local
lookup, a linear scan of a `Vec<(u64, Arc<Mutex<ZArenaTlab>>)>`, an
`Arc::clone`, a `parking_lot` lock, the bump, an unlock, an `Arc` drop, and an
atomic `fetch_add`.

## What would close it, in order

1. **Give the JIT-emitted TLAB bump the publication contract
   `Tlab::alloc_initialized` has.** The one change that makes the ~110 ns
   collapse, and the one that makes `CRATONVM_JIT_REAL_NEW_SITE_FLAGS` worth
   defaulting on. It is also the change that corrupted the heap the last time
   it was attempted, so it needs the contract — not a revert of the revert.
2. **Cheapen `alloc_raw_tlab`'s fast path** — the `Arc` clone/drop pair and the
   TLS `Vec` scan, ~10-20 ns of the ~110, no protocol change. Carries a
   re-entrancy hazard (`tlab_refill` runs under the same borrow), so it wants
   care rather than a quick edit.
3. **Cache the per-allocation class-initialisation re-check** — 12.2% of the
   profile, for a predicate whose state is monotonic.
4. **Constructor inlining**, which is what lets IR-level escape analysis
   scalar-replace the allocation while keeping the atomic side effect. This is
   what C2 does here, and why HotSpot's constructor loop outruns its own bare
   atomic loop.

## Ground truth

Re-run on the 08-17 binary with a **2400-second cap**: still `rc=124`. No
`@@TESTFAIL` — nothing has failed, the loop is simply still going.

That run was on a host under load 20-35 from unrelated work, which inflates
it. The microbenchmark extrapolation on a quiet host is ~430-500 s, and the
real run is consistently worse than the steady-state probe predicts, because
2.1 billion immediately-dead allocations against a 1500 m heap pay a GC cost
the probe does not.

## Recommendation

Unchanged in shape, better in size: leave this as a recorded throughput gap.

A `class-overrides.tsv` floor would convert the HANG into a real result, and
would also let the class's **other 12 tests** report for the first time — they
are not known to fail, they simply never reach `@@RESULT`, because JUnit runs
`testConstructionWithIndex` in the same fork and the harness kills it at the
wall. That trade is worth revisiting once item 1 lands and the required cap is
minutes rather than tens of minutes. At the current cost the class would still
be the slowest in a 657-class suite by a wide margin, which is why no row is
added yet.

## Repro

```bash
cd apps/netty-suite-runner
echo io.netty.util.concurrent.FastThreadLocalTest > /tmp/one.txt
CV_BIN=<binary> bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 2400 --out /tmp/repro
```

`probes/FtlRate.java` (the real loop) and `probes/CtorShapeRateProbe.java`
(the same loop split into allocation / atomic / constructor shapes) are the
quick way to re-check progress. **Interleave the arms** — a loaded host moves
every number, including `atomicOnly`, which no change in this area can touch.
