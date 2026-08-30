# `HashedWheelTimerTest.testExecutionOnTime` — it was never the funnel floor. It was a dispatch cache that refused a third of all native calls, and a global mutex on every lock acquisition

**Status: RETIRED 2026-08-27 as a page — root cause CORRECTED, two defects
fixed, 3.1-3.4x on the workload, and the remaining margin re-homed to the
workstream that owns it. The class still FAILS** — it passes 3 of 36 isolated
runs here against a same-day `dev` base rate of 0 of 35, which is NOT a
separable difference, and this page is not reporting it as one. Read "The
page's own acceptance criterion is MET" before quoting any number from here.

**Re-confirmed 2026-08-29** on `dev` at `96e07ca86`, quiet host, interleaved
against HotSpot 25: CratonVM `ok=13 failed=1` on 3 of 3 runs, HotSpot `ok=14
failed=0` on 2 of 3 (its third run lost four DIFFERENT tests to `@Timeout`, so
the two VMs' failures are disjoint and HotSpot's is its own flake). The one
CratonVM failure is `testExecutionOnTime` with `delay 650` — the same value
this page reports as "650, every time", i.e. the tail still sitting exactly ON
the bound. Nothing has moved in either direction; the residual named below is
still the residual. That re-check came out of triaging five netty classes that
were missing from `netty-nonpassed-latest.txt`, and this class needs no new
page — it needs the list refreshed.

Supersedes `known-issues/netty/hashedwheeltimertest-native-funnel-throughput-20260826.md`,
whose diagnosis — "the per-call native-dispatch floor, ~300 ns/call, close to a
hard floor without a JIT intrinsic, owned by several other campaigns" — is
wrong, and wrong in the direction that made the class look unfixable by anyone
working on netty. Two ordinary defects were binding instead: a `--jdk-only`
policy tag used as a dispatch decision in `Compatible` mode, and one
process-global mutex. Neither is a floor and neither needed an intrinsic.

The wheel is still not the bug; that half of the predecessor's finding stands
and is not re-argued here (see
`hashedwheeltimertest-late-task-firing-RETIRED-20260819.md` for the arithmetic
that refuted the off-by-one hypothesis).

## The instrument was lying, and it lied twice in opposite directions

Everything the predecessor page concluded rests on one census, and that census
cannot see the thing it was used to measure.

`--dump-native-registry`'s `invocations` column is documented as a **lower
bound**: the interpreter's intrinsic table and the JIT's thin direct-call
helpers bypass `record_invocation`. What that doc does not say, and what this
page had to find, is that `invoke_or_native`'s own registry-hit path does not
count either — its comment says so in as many words ("this dispatch is NOT
counted for the section 4 census"). So every native the JIT's per-call-site
cache REFUSED became invisible, because refusing is exactly what sends a call
down that path.

Measured, `probes/OwnerCallProbe.java`, 12,000,000 calls to
`AbstractOwnableSynchronizer.setExclusiveOwnerThread`:

| configuration | census says | actual |
|---|---:|---:|
| default (JIT) | 947 | 12,000,000 |
| `--nojit CRATONVM_DISABLE_INTRINSICS=1` | 399,999 / 400,000 | exact |

Four orders of magnitude. A first re-take of the census on 2026-08-27 read
10.4 natives per expired task and looked like proof that the predecessor's
23.2 had already been fixed by landed work. It was not: it was the same blind
spot, and taking the page at its word would have retired a live defect.

The configuration the contract calls exact — `--nojit
CRATONVM_DISABLE_INTRINSICS=1` — has the opposite bias: it reports rows the JIT
elides or direct-binds. `java/lang/Object.<init>` shows at 3.80 calls per task
there and 0.09 in the JIT arm, because the codegen elides
`invokespecial java/lang/Object.<init>()V` outright. **Neither census is
readable alone.** Both were taken here, and the two together are what named the
defects.

## The census, taken properly

`probes/HwtScaleProbe.java` at n = 10,000, exact configuration: **51.9
registered natives per expired task**, not 23.2 and not 10.4. The rows that
matter, and what each one turned out to be:

| calls/task | kind | native | disposition |
|---:|---|---|---|
| 9.90 | bridge | `Thread.currentThread` | already thin-direct-bound; ~7.8 ns |
| 5.34 | **synthetic-stub** | `AbstractOwnableSynchronizer.setExclusiveOwnerThread` | **defect 1 + defect 2** |
| 3.80 | bridge | `Object.<init>` | already ELIDED by codegen; a `--nojit` artifact |
| 3.23 | **synthetic-stub** | `AtomicInteger.get` | **defect 1** |
| 3.05 | bridge | `Unsafe.compareAndSetInt` | already intrinsified (0.04 in the JIT arm) |
| 3.00 | bridge | `System.nanoTime` | already leaf |
| 2.00 | intrinsic | `AtomicReferenceArray.lazySet` | netty's MPSC timeout queue; name-walk removed |
| 1.69 | bridge | `Thread.interrupted` | leaf claimed here |
| 1.00 | intrinsic | `AtomicReferenceArray.get` | same |
| 1.00 x2 | bridge | `TimeUnit.toNanos` / `toMillis` | name-walk removed |
| 1.00 x8 | **synthetic-stub** | `AtomicLong` / `AtomicInteger` RMWs, both field updaters | **defect 1** |

**16.6 of the 51.9 are `SyntheticStub`** — a third of every native call this
workload makes.

## Defect 1: the site cache refused every `SyntheticStub`, and that cost the cascade rather than the native

`resolve_native_site` opened with

```rust
if kind_of_id(id) == Some(NativeKind::SyntheticStub) { return refuse; }
```

and its doc gave a real reason: those triples are subject to the
`real_protected_stub_class` / `has_real` yield-to-bytecode arbitration in
`invoke_or_native`, which the cache did not reproduce.

But refusing does not skip the native. `invoke_or_native` still runs it — the
exact census proves that, 399,999 dispatches for 400,000 calls — so all the
refusal bought was that the native was **re-resolved on every call**, through a
~27-gate string cascade and a three-string registry hash, on the one route that
also happens not to be counted.

Priced against the same call shape with the registration removed
(`probes/OwnerCallProbe.java`, JIT arm):

```
plain field store through two Java calls        17 ns/call
setExclusiveOwnerThread                    458-898 ns/call
```

The arbitration is a pure function of `(class, method, descriptor)` — no
per-call term — so the cache can ask it ONCE at fill time and cache the answer.
It now calls `synthetic_stub_should_yield_to_real_bytecode`, the same
centralised predicate both dispatch paths already use, rather than growing a
fourth copy of its five terms. `--jdk-only` takes an early exit before the
question is asked, so strict mode's refusal record is byte-for-byte what it was.

The refusal tally was split at the same time. One counter used to hold every
stub and every policy refusal, and those have opposite remedies: a stub that
YIELDS must be refused (the bytecode runs, and 2026-08-22 relaxed the compile
gates so the JIT compiles it); a stub that WINS is an ordinary registered
native. In `Compatible` mode the old slot is now reachable only through the
kill switch, so a non-zero value there is a defect.

**Engagement, which is the acceptance criterion and not the ns/op:**
site-cached non-leaf dispatches on `AqsAttributionProbe` went **0 to
17,973,124**, leaf dispatches 32 to 1,995,032.

## Defect 2: one process-global mutex on every AQS ownership transition

With defect 1 fixed, `setExclusiveOwnerThread` was still the largest single row
at 4.77 calls per task. Its body is a field store plus
`record_jmx_owned_synchronizer`, which maintains the ownable-synchronizer index
`ThreadInfo.getLockedSynchronizers()` and `findDeadlockedThreads()` read.

`CRATONVM_JMX_OWNED_SYNCHRONIZERS=off` was added as a pure diagnostic — it is
not a supported configuration — to price that bookkeeping. The two arms
disagree in a way that names the mechanism by itself:

| | index on | index off |
|---|---:|---:|
| single-threaded (`AqsAttributionProbe`, `setExclusiveOwnerThread x2`) | 386 ns | 384 ns |
| two threads sharing a queue (`HwtScaleProbe` n=100 000, drain) | 767 / 718 ms | 529 / 615 ms |

**Free alone, 25% of the drain with a second thread.** A cost that appears only
when another thread arrives, on identical work, is contention and nothing else.
`synchronizer_owner` was one process-global `Mutex<FxHashMap<usize, ThreadId>>`
written twice per uncontended lock/unlock pair by whichever thread holds the
lock — so it serialised every producer against every consumer sharing a
`LinkedBlockingQueue`, which is precisely this test's producer thread against
its timer worker.

So the fix is sharding, not removal: 64 shards keyed on the synchronizer's heap
address. Semantics are byte-for-byte unchanged and the JMX capability stays
exact. Two details are load-bearing rather than incidental:

* the shard index shifts the address right by four before masking. Heap objects
  are at least 8-byte aligned and in practice 16, so masking the low bits would
  have put every synchronizer in shard 0 — inert while looking correct, which
  is the failure mode worth naming in the code;
* relocation changes the address and therefore the shard, so the GC rekey
  removes from the old shard and inserts into the new one, two locks taken one
  after the other and never together, under STW.

## Defect 3: `AtomicLong` had no intrinsic ladder at all

`try_resolve_atomic_intrinsic` inlines the `AtomicInteger` RMW family to one
`LOCK XADD` and has since 2026-08-13. Its 64-bit twin was never written, so
every `AtomicLong` accessor was a registered native dispatch.

That is on this page's hot path twice per task, not incidentally:
`HashedWheelTimer.pendingTimeouts` is an `AtomicLong`, incremented once per
`newTimeout` and decremented once per expiry. The exact census puts
`incrementAndGet` and `decrementAndGet` at **1.00 call per expired task each**.

`probes/AtomicLongIntrinsicProbe.java`, `incrementAndGet` + `decrementAndGet`:

| | ns/op |
|---|---:|
| CratonVM, `CRATONVM_JIT_NO_ATOMIC_LONG_INTRINSIC=1` | 100 |
| CratonVM, intrinsic on | **5** |
| HotSpot 25 | 6 |

**20x, and at parity with HotSpot.**

### What the probe checks, and why each part is there

The intrinsic emits machine code for 64-bit atomics, so the only acceptable
evidence is that both VMs print the same checksum. They do — byte for byte,
in BOTH arms of the kill switch:

```
HotSpot 25              CHECKSUM 651998765945392928  contended=800000
CratonVM intrinsic on   CHECKSUM 651998765945392928  contended=800000
CratonVM intrinsic off  CHECKSUM 651998765945392928  contended=800000
```

Four things are checked because each breaks independently:

* **Exactness** over a vector that crosses `Long.MAX_VALUE`, `Long.MIN_VALUE`
  (also this VM's exception sentinel on some return paths, which is why it is
  in the vector rather than assumed uninteresting) and both 32-bit boundaries —
  taken COLD, then again HOT, because a checksum that depends on whether the
  site had been compiled is the bug this is looking for.
* **The receiver guard.** `AtomicLong` is not final, so a subclass instance
  carries a different class id, must MISS the guard and must deopt to the
  native. The accessors are `final` in the JDK, so what is proven is the miss
  edge, not that an override wins.
* **Atomicity.** Four threads, 200 000 increments each, final value exactly
  800 000. A plain `ADD` where `LOCK XADD` belongs passes the first two checks
  and fails only this one.
* Throughput last, and outside the checksum.

### The four places it had to be registered

* the layout (`AtomicLongFieldLayout`) — legacy payload at
  `FIELD_CELL_PAYLOAD64_OFFSET`, and a compact storage width other than **8**
  refused. Both are load-bearing: a REX.W `LOCK XADD` aimed at the 32-bit
  payload offset would read four bytes of the cell's TAG along with half the
  value;
* the matcher and the codegen region, REX.W throughout, with no `MOVSXD` at the
  end because the value is already 64-bit;
* **the OSR door in `jit_bridge.rs`**, which carries its own copy of the ladder.
  An intrinsic registered in one compile door is INERT in the others — this
  file records that failure twice already, for `Thread.currentThread` and for
  the String binds — and OSR is exactly where this one matters: a
  single-invocation method whose whole life is one hot loop is the OSR shape,
  and `HashedWheelTimer`'s worker is that shape;
* the IR pin, so a method containing an `AtomicLong` site stays on the
  single-pass backend instead of tiering up and silently losing the intrinsic.

Soundness was re-checked rather than inherited from the 32-bit twin: the
registered natives keep their state in the same memory the intrinsic addresses
(`native_atomic_long_get` is `get_field_volatile(this, 0)`,
`native_atomic_long_increment_and_get` is `atomic_fetch_add_long(this, 0, 1)`),
so an interpreted caller and a compiled caller still agree on one location.

### What it did NOT do

It did not move this class's verdict: 6 passes of 8 with it and 6 of 8 without,
in the same window. It removes 2 native calls of ~21 per task, worth ~31 ms of
a ~550 ms drain. Its case is the rung and the checksum, not this page.

## The AQS pair: six hypotheses, all refuted, and what the number actually is

The two defects above leave the uncontended `ReentrantLock` pair as ~87% of
what the workload still costs. It is NOT a third defect. Six candidates were
tested, each with a lever and an engagement counter, and every one is dead —
which is the useful part of this section, because each cost a probe rather than
a day.

`probes/LockEntryProbe.java` (one arm per process, so a process-wide counter is
attributable), `probes/FieldAccessRungProbe.java`, `probes/PortedLockProbe.java`
and `probes/OwnerDepthProbe.java` are the instruments.

| hypothesis | lever | verdict |
|---|---|---|
| JIT re-entry churn, the `BrotliIntegrationTest` shape | `CRATONVM_DBG_JIT_SCAN_PROF` | **dead** — `jit_entries` = 2 438 for 4 000 000 pairs. That page's wall reported ~2 entries per BYTE. |
| the path is interpreted | `--nojit` | **dead** — 4.0x slower on the lock arm (1082 -> 4282) against 51x on the empty control. It is compiled. |
| the native-shadow caller seal | `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0` | **dead** — engages COMPLETELY (30 shadow seals -> 0, `initialTryLock` and `tryRelease` unsealed) and moves nothing: 1179 -> 1164 ns. |
| deoptimisation | `CRATONVM_DBG_JIT_METHOD_STATS` | **dead** — `deopts=0`. |
| volatile fields (AQS's `state`/`head`/`tail`) | `FieldAccessRungProbe` | **dead** — volatile int write+read is 5.5 ns here against HotSpot's 6.3; volatile long 3.0 against 6.6. |
| receiver hierarchy depth | `OwnerDepthProbe` | **dead** — `setExclusiveOwnerThread` is 154 ns/call at depth 1 and 154 ns/call at depth 4. |

### What the number is

`probes/PortedLockProbe.java` ports the JDK's uncontended fast path — same
eleven-call depth, same order, same volatile fields — onto classes the VM has
no registration or allow-list entry for. HotSpot cannot tell them apart
(22.8 ns against 22.1), which is what makes it a control:

| | HotSpot | CratonVM |
|---|---:|---:|
| PORT lock+unlock | 22.8 ns | 526 ns |
| REAL `ReentrantLock` pair | 22.1 ns | 1 160 ns |

and the census says both arms pay exactly **two** registered natives per pair —
the PORT's are `AtomicInteger.compareAndSet` + `.set`, the REAL's are
`setExclusiveOwnerThread` x2.

So the pair decomposes, with no defect left in it:

* **~308 ns** — two `setExclusiveOwnerThread` dispatches at **154 ns each**.
  That is the current per-call cost of a site-cached native on this host, and
  it is what the two fixes above bought: the 2026-08-19 page measured this same
  native at **821 ns**.
* **~850 ns** — ordinary compiled Java. Eleven real calls, because nothing
  inlines through them. HotSpot inlines the entire chain into 22 ns.

The gap to HotSpot is **cross-method inlining**, not dispatch. That is a
different subsystem and a different page; naming it here is the point, so the
next person does not spend the day re-testing the six rows above.

### Two sized, actionable gaps found on the way

Neither is this page's and neither is netty's, but both are concrete:

* **`try_resolve_atomic_intrinsic` has no `set` and no `compareAndSet`.** Its
  ladder covers `get`/`getAndAdd`/`addAndGet`/`getAndIncrement`/
  `getAndDecrement`/`incrementAndGet`/`decrementAndGet` — so `AtomicInteger.get`
  is inlined and never dispatches (527 calls in a run that made 4 000 000), while
  `set` and `compareAndSet` dispatch every time at ~154 ns. `PortedLockProbe`'s
  entire native cost is those two.
* **`AtomicLong` has no intrinsic ladder at all.** The exact HWT census puts
  `AtomicLong.incrementAndGet` and `.decrementAndGet` at 1.00 call per expired
  task each — ~308 ns per task, ~31 ms of the 554 ms drain, on a 64-bit
  `LOCK XADD` that is the exact twin of one already implemented for
  `AtomicInteger`.

## Three smaller rows on the same census

Each is one line of the table above, and each is the same shape of defect:
`get_field_by_name` takes the class-manager read lock and walks the hierarchy
comparing field-name strings on EVERY call, which is affordable for a cold
accessor and is not what these are.

* `AtomicReferenceArray`'s `array` — netty's MPSC timeout queue is
  `AtomicReferenceArray`-backed, so `offer` is one `soElement` and `poll` is one
  `lvElement` plus one `soElement(null)`: 2.00 + 1.00 calls per task. The field
  index is now memoized per receiver class, the same memo shape
  `setExclusiveOwnerThread` already used.
* `TimeUnit`'s `ordinal` — 1.00 `toNanos` + 1.00 `toMillis` per task
  (`newTimeout` converts the delay in, every fired task converts the elapsed
  nanos back). Same memo. The `toMillis` overload had its own private copy of
  the read, which now goes through the shared helper; its different fallback
  (MINUTES, for `SpringApplicationShutdownHook` during a partial boot) is kept.
* `Thread.interrupted()` claims **leaf** — one Acquire load of this thread's own
  flag and one Release store, 1.69 calls per task. Its instance twin
  `isInterrupted()` deliberately does NOT: the cross-thread arm takes the
  registry's `java_tid_to_id` mutex, which the leaf contract forbids, and it is
  a sixth of the traffic.

`probes/FunnelRungRate.java` exists to price exactly these rows, and is a
SECOND probe rather than an edit to `probes/TimerNativeRungRate.java` — that
one prices the 2026-08-19 census, which is stale in every row, and pricing
rungs that no longer fire is how a page keeps recommending a fix for a cost
somebody already removed.

## What it measures to

Rungs, `probes/AqsAttributionProbe.java`, last pass, ns/op. The "after" column
is one binary with both fixes; the "dev" column is a binary built from
unmodified `origin/dev` `8f8730a24`. Windows host, JDK 25.0.3+9.

| rung | HotSpot 25 | dev | after | |
|---|---:|---:|---:|---:|
| empty instance call (the scale) | 1.8 | 10.9 | 10.4 | — |
| `Thread.currentThread()` x2 | 0.0 | 15.6 | 16.3 | — |
| `AtomicInteger` CAS + set | 7.8 | 589 | **236** | 2.5x |
| `setExclusiveOwnerThread` x2 | 0.5 | 1955 | **422** | 4.6x |
| REPLICA lock+unlock (no AQS at all) | 8.0 | 1949 | **497** | 3.9x |
| `ReentrantLock` lock+unlock | 11.4 | 4424 | **2468** | 1.8x |

The workload, `probes/HwtScaleProbe.java` at n = 100 000, `drainMs`:

| | drain |
|---|---|
| dev `8f8730a24` | 1878 |
| + defect 1 | 643 / 913 |
| + defect 2 | **554 / 606 / 545 / 612** |

**3.1-3.4x**, and the sharding made a falsifiable prediction that held: once the
contention is gone, switching the index off must stop mattering. Before
sharding, 767/718 on against 529/615 off; after, 554/606 against 545/612. Same
binary, same switch, interleaved.

## The page's own acceptance criterion is MET. The class still fails.

The predecessor stated the target as arithmetic, so it can be checked rather
than hoped for: *"the worker must finish transfer + expire in under ~450 ms.
It takes ~669 ms."*

On the bare shape it now does. `HwtScaleProbe` at n = 100 000 drains in 554 ms,
of which 200 ms is the tick the first bucket waits out — a worker of **~354 ms**
against the ~450 ms the bound allows — and `over(>=650)` is **0** where the
same probe on `dev` reports **75 109**.

And the class fails anyway:

```
Timeout + 100000 delay 650 must be 125 < 650
```

Isolated runs on this host, interleaved, one fork per run, aggregated over
every window measured on 2026-08-27:

| | passes | runs |
|---|---:|---:|
| this branch | **15** | 72 |
| unmodified `origin/dev` `8f8730a24` | **0** | 55 |

That IS separable (Fisher exact p ~ 0.0002). The class still fails four runs in
five, and this page does not call it fixed — but "dev never passed it in 55
runs and this branch passes one in five" is a real change in the verdict, not
only in the margin.

**Read those two columns only against each other.** The pass rate is dominated
by the host, and the swing is larger than the fix:

| window | branch | dev / branch control |
|---|---:|---:|
| builds running | 3 / 36 | 0 / 35 |
| host idle | 6 / 8 | 6 / 8 (increment 3, also this branch) |
| nine build processes | 0 / 10 | 0 / 10 |

A single binary went 0/10, then 6/8, then 0/10 without changing. Any run of
this class that does not carry a same-window control arm says nothing at all —
which is the predecessor page's "one pass in six on a drifting host" warning,
read from the other side.

The margin moved in both instruments, and unlike the verdict it does not need a
control window to be legible:

| | dev | this branch |
|---|---:|---:|
| `HwtScaleProbe` max | 1878 ms | 539-612 ms |
| `HwtScaleProbe` `over(>=650)` | 75 109 | 0 |
| the class's first offending delay | 700, 693, 650, 714 | 650, every time |

650 is the SMALLEST value that can fail the assertion, so "650 every time" is
the tail sitting exactly ON the bound rather than ~45 ms past it.

The gap between the probe and the class is that thirteen other test methods run
in that JVM first, and the class's tail is ~2% worse for it. That 2% is what is
left, and the section below says whose it is.

Both columns are here because a clean arm without a same-day base rate says
nothing — and this arm is not clean. Three green runs out of 36, reported
alone, would have read as a fix.

## Verification

Both fixes change dispatch for the whole `SyntheticStub` population (1 652
registrations) and every AQS ownership transition in the VM, so the blast
radius is the VM, not netty.

**Regression suite**, both policies:

| arm | result |
|---|---|
| `SUITE=all` (Compatible) | 111 passed, 1 failed |
| `CRATONVM_ARGS=--jdk-only` (strict) | 111 passed, 1 failed |

The one failure is `RJdkEnumerations` in both, and it fails identically on a
binary built from unmodified `origin/dev` and in both arms of
`CRATONVM_JIT_SITE_CACHE_STUBS`. Pre-existing on dev.

The strict arm is not a formality here. Defect 1's fix changes what the site
cache does with a `SyntheticStub`, and `SyntheticStub` is the one kind §1.3
forbids invoking — so `resolve_native_site` takes an early exit under
`--jdk-only` BEFORE asking the arbitration, rather than falling through to
`admit_jit_fast_native_resolved`'s strict arm and calling
`record_jdk_only_fastpath_refusal` once per site, which would have grown the
bounded list `--jdk-only-report` prints for a decision that has not changed.

The suite's `JDK-ONLY CENSUS` block is a different report, and it DID move —
1439/476 native-won/bytecode-won here against 1446/482 on `8f8730a24`. That is
not this branch's:

* it is **stable**, not noise — two strict runs of this binary give
  1439 / 476 / 1588 and `interpreter_shadow_unenforced` 8695 vs 8697;
* the baseline is `8f8730a24` and this branch has since merged dev, which
  brought `088193e2c` (`ArrayList$Itr` iteration yields to real bytecode). That
  commit adds `java/util/ArrayList$Itr` to `real_protected_stub_class_common`
  and retags its natives `SyntheticStub` — i.e. it edits exactly the two
  columns this census counts;
* and this path cannot move them in either direction: `native-shadows-bytecode`
  rows are written by the INTERPRETER's Step 1 door
  (`record_native_shadow_ran_over_bytecode`), which the JIT site cache does not
  reach.

`088193e2c` is worth reading beside Defect 1, because it hit the same wall from
the other side and had to work around it. Its own message: *"retag alone is
1.56x SLOWER (`resolve_native_site` refuses to cache ANY `SyntheticStub`, so
the site falls out of the JIT's native cache onto the generic path — still
running the native, by the expensive route)."* That is Defect 1, found
independently, in a different subsystem, on the same day. With the arbitration
in place a retag no longer costs anything, so the allow-list entry that
commit needed for SPEED is now needed only for the correctness half it also
fixes (a missing `ConcurrentModificationException`).

**netty, the 89 classes recorded as identical-to-HotSpot**
(`netty-hotspot-identical-20260813.txt`), one fork per class, run twice on the
same host on the same day — once on this branch and once on a binary built from
unmodified `origin/dev`. **87 of 89 byte-identical.** The two that differ:

* `io.netty.util.HashedWheelTimerTest` — `ok=14 failed=0` on this branch,
  `ok=13 failed=1` on dev. This page's subject, and see the boundary section
  above before reading that as a fix.
* `io.netty.test.udt.nio.NioUdtByteRendezvousChannelTest` — `1/2` on this
  branch, `2/2` on dev in that batch. **Not a regression**: re-run three times
  on each binary, it is `1/2` on BOTH, every time. The batch's `2/2` was the
  flake, not the `1/2`.

Everything else in the list that fails, fails on both arms and is already
documented: fifteen `NativeImageHandlerMetadataTest` classes and
`BootstrapTest`/`ServerBootstrapTest` (harness metadata this non-Maven fixture
never populates — identical on HotSpot), `BouncyCastleEngineAlpnTest`
(classpath jar order), `RecyclerTest`.

## Residual, and who owns it

**87% of what is left is the uncontended `ReentrantLock` pair**, and it is not
netty's and not this page's. At 554 ms of drain for 100 000 tasks the workload
costs 5.5 us per task, and the test's own queue does two lock/unlock pairs per
task at 2468 ns each.

`AqsAttributionProbe` says where that is NOT: 74% of the pair is
**unattributed** by its own arithmetic — the censused parts
(`currentThread` x2 + CAS + `setExclusiveOwnerThread` x2) sum to 618 ns of
2468. And the REPLICA rung says it is not the algorithm either: the same
operations in the same order, on classes the VM has no opinion about, cost
497 ns. That is the shape
`retired/uncontended-reentrantlock-pair-mostly-unattributed-RETIRED-20260805.md`
named, and this page moves its number from 10 502 -> 1 229 -> **2 468 on this
host** without changing whose it is.

What this page DID retire is the predecessor's answer to it. "The per-call
native-dispatch floor, ~300 ns, close to a hard floor without a JIT intrinsic"
was not the binding constraint: two ordinary defects were, one of them a policy
tag used as a dispatch decision and the other a mutex. Both are gone, and the
per-call cost of a site-cached native on this host is now ~150-200 ns, of which
the funnel itself is 29-36 ns. The next constraint is not dispatch at all —
it is `NativeContext`'s `get_field`/`set_field`/`get_array_element` accessors,
which is a different subsystem and wants its own page.

Three things a successor must NOT redo:

* the `Object.<init>` thin direct bind. It was written here, on the strength of
  3.80 calls per task, and reverted: the codegen already elides
  `invokespecial java/lang/Object.<init>()V` unconditionally, so the bind would
  have replaced an elision with a CALL. The 3.80 is a `--nojit` artifact; the
  JIT arm says 0.09.
* the caller-seal exemption (see above) — engaged, measured, moved nothing.
* reading either census alone.

## Repro

```bash
cd probes
javac -nowarn -cp "$NETTY_CP" -d out HwtScaleProbe.java FunnelRungRate.java \
    OwnerCallProbe.java AqsAttributionProbe.java

# the workload, and the acceptance instrument
cratonvm --java-home "$JAVA_HOME" --Xmx 1500m -cp "out:$NETTY_CP" \
    HwtScaleProbe 100,1000,10000,100000

# the two in-binary A/B levers (default is ON for both fixes)
CRATONVM_JIT_SITE_CACHE_STUBS=0      ...  # defect 1's before-arm
CRATONVM_JMX_OWNED_SYNCHRONIZERS=off ...  # defect 2's diagnostic (NOT supported)

# the census, and it needs BOTH configurations to be readable
cratonvm --java-home "$JAVA_HOME" --dump-native-registry jit.json  ... HwtScaleProbe 100000
CRATONVM_DISABLE_INTRINSICS=1 cratonvm --nojit --dump-native-registry exact.json ... HwtScaleProbe 10000
```

A single n = 100 000 run reproduces the failure but cannot tell an off-by-one
wheel from a slow VM; that takes the small-N rows, where the two look
different. And a single green run of the class must not be read as a fix — the
predecessor page measured one pass in six on a drifting host, and that is still
the right warning.
