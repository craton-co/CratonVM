# Two thirds of an uncontended `ReentrantLock` pair — ATTRIBUTED, RETIRED 2026-08-05

| | |
|---|---|
| **Status** | RETIRED — the pair is accounted for, and the term that dominated it is fixed |
| **Opened** | 2026-08-05, retiring `aqs-thread-handoff-latency` and `native-call-funnel-is-the-per-call-floor` |
| **Closed by** | `perf/aqs-unattributed-20260805` |
| **Successor** | [`native-funnel-fixed-cost-is-the-remaining-wall`](../known-issues/vm/native-funnel-fixed-cost-is-the-remaining-wall-20260805.md) |

The brief said: *"do not open the third attempt the way the first two were
opened — account for the whole 10,502 ns before proposing a fix."* That is what
happened, and it changed the answer.

## The instrument

`probes/AqsAttributionProbe.java`. It does not ask what is expensive; it
**builds the pair up from its parts**, one monomorphic loop method per rung, and
prints the arithmetic at the end so a reader cannot skip the residual:

```
control -> empty call -> currentThread x2 -> CAS -> setExclusiveOwnerThread x2
        -> a REPLICA lock/unlock with no AQS classes at all
        -> tryLock/unlock -> lock/unlock
```

The replica is the load-bearing rung. It performs the same operations in the
same order — read state, CAS 0→1, store owner, read state, read owner, clear
owner, store state — against classes the VM has no special opinion about. On
HotSpot it lands within 1.28x of the real pair, which is what says the probe is
measuring the algorithm and not itself.

## What the parts actually were

Quiet host, last of four passes, real-JDK mode. The `empty instance call` scale
is printed on every run and is how these are compared — this host ran at
23-27 ns against the 8.4 ns of the machine the 10,502 ns figure came from, so
read the shares, not the absolutes.

| | before | after | |
|---|---:|---:|---|
| empty instance call (the scale) | 23.3 | 27.3 | |
| `Thread.currentThread()` x2 | 16.6 | 25.8 | already fixed 2026-08-04 |
| CAS + set (stands in for `Unsafe.compareAndSetInt`) | 676.6 | **490.7** | |
| `setExclusiveOwnerThread` x2 | 1642.8 | **594.5** | |
| REPLICA lock+unlock (no AQS) | 1330.0 | **697.5** | |
| `ReentrantLock` tryLock+unlock | 2640.7 | **1383.3** | |
| **`ReentrantLock` lock+unlock** | **2832.5** | **1331.4** | |
| sum of the censused parts | 2336.0 | 1111.0 | |
| **unattributed** | **496.5 (18%)** | **220.3 (17%)** | |

So the "two thirds unattributed" was itself wrong, and wrong for an
instructive reason: **the pair was never 10,502 ns on current dev.** Between
2026-08-03 and 2026-08-05 it had already fallen to ~2,832 ns — the leaf-native
work plus other sessions' landings — while the arithmetic in the successor
brief was still subtracting 2026-08-03's terms from 2026-08-03's total. Measure
the total and the terms *in the same run*, or the residual is fiction.

With both measured together, 82% of the pair was already attributed before this
branch changed anything, and `setExclusiveOwnerThread` x2 was **58% of it**.

## The fix: the site cache was serving the wrong half

The leaf-native work (2026-08-04) gave compiled code a per-call-site native
resolution cache, but only installed an entry when the native claimed the leaf
contract. Everything else — which is nearly everything — still ran
`vm_exec::invoke_or_native` **per call**: a ~27-gate string cascade, then a
three-string registry hash, then `real_protected_stub_class`, then
`resolve_native_dispatch_wave1`, then the capability gate, and only then the
funnel.

The measurement that names it, from the same probe:

| | ns/op |
|---|---:|
| ordinary Java call | 7.9 |
| `AtomicInteger.get` — leaf, site-cached | 233 |
| `AtomicInteger.compareAndSet` — non-leaf | 806 |
| `setExclusiveOwnerThread` — non-leaf, 2 object args | 821 |

The ~570 ns between the leaf and non-leaf rungs is not the funnel. **Skipping
`invoke_or_native` is worth more than skipping the funnel**, and the leaf flag
was gating the wrong thing. So the cache now admits every registered native and
the leaf claim decides only which wrapper runs — `safe_native_call_leaf` or the
full `safe_native_call_prevalidated_objects`.

Two things had to be got right for that to be sound:

* **The superclass walk.** `AbstractOwnableSynchronizer.setExclusiveOwnerThread`
  is reached with a `ReentrantLock$NonfairSync` receiver, two levels down.
  Resolving on the receiver class alone found nothing and refused the site — the
  single reason the biggest term on the path had no fast path.
  `resolve_native_owner_for_receiver` reproduces `invoke_or_native`'s rule
  exactly, including its exception: a parent that declares the method in
  bytecode ends the walk **unless that same parent also has a registered
  native**, which is precisely the AOS case.
* **The refusal tally.** Reason 5 ("registered but does not claim leaf") is
  retired rather than renumbered, so a count from an older run cannot be read
  against the new list.

Two smaller fixes on the same native, both memoization of work that was a
per-class constant:

* `set_field_by_name("exclusiveOwnerThread", …)` took the class-manager read
  lock and walked the hierarchy comparing field-name strings on every call. The
  index is memoized per receiver class id (8 slots — a one-entry cache thrashes
  between a lock's sync and a `ThreadPoolExecutor$Worker`).
* `record_jmx_owned_synchronizer` resolved the owner's `ThreadId` from its
  mirror — a field read plus the registry's `java_tid_to_id` mutex — when AQS
  always passes the *current* thread. A pointer compare against
  `java_thread_obj` short-circuits it.

Result: `setExclusiveOwnerThread` 821 → ~300 ns per call.

## Verification

`CRATONVM_DBG=intrinsic-stats` reports the two populations separately, because
a single number could not tell "the leaf half still works" from "everything
became non-leaf":

```
compiled leaf-native dispatches:                      1,995,028
compiled site-cached native dispatches (non-leaf):   17,972,722
leaf sites refused, capability-classified triple:             1
leaf sites refused, no native ... (bytecode wins):           20
```

A-B-B-A interleaved, both orders, scale rung flat across all four runs:

| | fixed | base | base | fixed |
|---|---:|---:|---:|---:|
| empty instance call (scale) | 34.3 | 22.3 | 34.3 | 22.8 |
| `setExclusiveOwnerThread` x2 | **683** | 2061 | 1751 | **530** |
| **`ReentrantLock` lock+unlock** | **1229** | 5190 | 2687 | **1191** |

Every fixed run beats every base run on both rungs. Taking the *closest* pair
rather than the flattering one: 2,687 → 1,229 ns, **2.2x**.

The base arm is much noisier than the fixed arm (2,687 vs 5,190 on two runs of
the same binary), which is itself a result: a path that re-resolves per call is
far more sensitive to what else the machine is doing. One base run produced a
*negative* residual — the parts, measured earlier, out-totalled the pair
measured later. The probe now prints "VOID, rerun it" when that happens.

## What is left

~1,200 ns against HotSpot's 12.3 ns, and it is no longer a mystery:

| | ns | share |
|---|---:|---:|
| `setExclusiveOwnerThread` x2 | ~530-680 | ~45% |
| CAS + set | ~400-490 | ~37% |
| `Thread.currentThread()` x2 | ~26 | 2% |
| unattributed | ~110-220 | **9-17%** |

Three native calls per pair at ~250-300 ns each, and their bodies are now a
memoized field write and a CAS. **That is the funnel**, which is item 2 of the
retired `native-call-funnel-is-the-per-call-floor` doc — closed there as
"answered, not done: it needs a profile, not an assertion". It is now the
largest single thing between this VM and HotSpot on the concurrency stack, so
it gets its own document rather than a bullet:
[`native-funnel-fixed-cost-is-the-remaining-wall`](../known-issues/vm/native-funnel-fixed-cost-is-the-remaining-wall-20260805.md).

## The lesson worth keeping

All three attempts at this number failed the same way, including the brief that
warned about it. The rule that would have caught every one of them:

> **Measure the total and the terms in the same run, and print the residual.**

Not "find the expensive thing" — a residual that stays large *is* the finding,
and a residual that goes negative means the run is void.
