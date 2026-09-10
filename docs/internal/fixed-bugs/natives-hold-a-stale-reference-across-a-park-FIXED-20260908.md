# ✅ FIXED — natives held a stale reference across a PARK, and one released its pins before its last use

## Status

**FIXED 2026-09-08 — these nine sites.** The FAMILY is not declared closed;
see "The family is NOT declared closed" for the one residual this work found
and did not chase. Nine natives, one instrument, one new gate rule, two new
probes. The reproducer that found it is
`probes/OldToYoungBarrierSweep.java`; it went from **2 SIGSEGVs in 6 runs** to
**0 in 35** across `Generational`+JIT, `Generational --nojit` and ZGC.

Retires three pages:

* `generational-young-sweep-frees-an-interpreter-held-object-FIXED-20260908.md`
  — the defect, fixed.
* `jerseyendpointrequestintegrationtests-sigsegv-generational-FIXED-20260908.md`
  — the same defect, on the strength of an identical fault decode; see "What
  this does NOT claim".
* `gpuresidencygc-generational-jit-reclaims-a-live-object-FIXED-20260908.md`
  — NOT a defect: its remaining evidence was an instrument reporting
  re-allocated addresses, and the screen below takes it to zero.

## The shape, and why the existing audit could not see it

`unpinned-native-locals-audit-48-fixed-20260825.md` and the loop-carried sweep
that followed it closed the ALLOCATION window: a native that holds an
`ObjectRef` across a call that can allocate, and dereferences it afterwards.
Both worked from `scripts/stale-receiver-audit.py`, whose rule is a HELPER that
takes the receiver by value, can allocate, and returns unit — a shape that
cannot hand the refreshed reference back.

The window this page is about is not an allocation. It is a **park**:

```rust
let wr = ctx.monitor_wait(this, timeout);   // releases the monitor, PARKS
ctx.monitor_exit(this);                     // ...on the PRE-wait address
```

`monitor_wait` is the widest GC window a native can open — wider than any
allocation, because it does not merely *permit* a collection, it waits for one.
A peer's collection runs to completion inside it. The reference it was called
on is a bare Rust local that nothing rewrites, so the `monitor_exit` that pairs
with the wait, and every later turn of the caller's retry loop, ran on an
address the collection had moved or reclaimed.

Three things hid it:

1. **`monitor_wait` was not in the audit's `ALLOC0` list at all**, so a helper
   that only parks did not even count as GC-capable.
2. The rule's premise is a `fn` taking the receiver by value; these are inline
   `ctx.` method calls in the body itself.
3. The one helper that *is* the right shape, `monitor_wait_release`, returns
   `MethodCallResult` — so the "returns the ref, therefore safe" filter
   excluded it, even though it never returned the ref.

## The two faces, and the quiet one is worse

`MonitorTable::exit` opens with `header_of(obj_ref)`.

* **The loud face**: the young slot was RECLAIMED, so the read faults —
  `EXCEPTION_ACCESS_VIOLATION` inside `MonitorTable::exit`, reached from
  whatever native did the wait. This is the crash both retired pages report.
* **The quiet face**: the object merely MOVED. The exit then releases a monitor
  at an address nothing owns, the real monitor is never released, and every
  later thread that enters that object blocks forever. Nothing is logged. This
  is the shape a suite reports as a timeout rather than a crash.

## The nine sites

| file | site | what crossed the window |
|---|---|---|
| `util_concurrent_ext.rs` | `with_sync_mutex` | the wrapper's `mutex`, across `body` — which re-enters Java — and the pins were released BEFORE `monitor_exit` used it |
| `lib.rs` | `monitor_wait_release` | the receiver, between the wait and the exit; 10 call sites, all retry loops |
| `lib.rs` | `cb_await_inner` | the `state` int[] holder every `cb_get`/`cb_set` reads, plus the error-path exit |
| `phases_early.rs` | `exchanger_do_exchange` | the receiver, at two `monitor_exit`s |
| `phases_early.rs` | `Phaser.arriveAndAwaitAdvance` | the holder array — **no pin at all** across the wait loop |
| `native-collections/lib.rs` | `native_lbq_put_blocking` | the receiver AND the element: the refresh was scoped to the loop BODY, and `native_lbq_offer` was handed the raw pre-park `args` |
| `native-collections/lib.rs` | `native_lbq_take_blocking` | the receiver, same scoping mistake |
| `util_concurrent_ext.rs` | the `CopyOnWriteArrayList` bridge (11 registrations) | `this`, the old array and the element across `ctx.new_array` and across `equals` |
| `native-collections/lib.rs` | `native_cowal_bulk_remove_predicate`, `native_cowal_add_all` | the monitor, the arrays, and — in the first — every surviving element, held as raw `Value`s across the remaining `predicate.test` calls |

The COWAL rows are the ALLOCATION window rather than the park window, and they
are here because the probe that found the park sites walked into them on the
next run. `ctx.new_array` is a collection point; the bridge held its receiver,
its source array and its element across two of them and then released the
monitor on the pre-allocation address.

### `with_sync_mutex` is also an ORDER bug, and that one is new

```rust
let out = body(ctx, this, &fixed);
ctx.unpin_native_roots(base);     // releases the frame that owns `mutex_h`
ctx.monitor_exit(mutex);          // ...and THEN uses a pin-protected value
```

Even with a refresh added, that order is wrong: `unpin_native_roots(base)`
truncates the pin stack that `mutex_h` lives in, so the refresh has to happen
before it and the unpin has to be the last statement. The function's own doc
comment already said "nothing below may use a pre-wait `ObjectRef`" — for the
contended ENTRY. `body` re-enters Java and is the same window.

## The reproducer

`probes/OldToYoungBarrierSweep.java` — every door a live reference can enter a
tenured container through, six threads, exact single-writer invariants, driven
against a collector that discovers old-to-young edges from a card table alone.
Each payload carries its identity twice (an `int` and a `String`) so a
reclaimed-and-reused slot fails as a WRONG VALUE and a reclaimed-and-decommitted
one as a fault.

| arm | before | after |
|---|---|---|
| `Generational`, JIT, 20 rounds | 1 SIGSEGV / 3 | 0 / 6 |
| `Generational`, `--nojit`, 20 rounds | 1 SIGSEGV / 3 | 0 / 6 |
| `Generational`, JIT, 60 rounds | — | 0 / 10 |
| `Generational`, `--nojit`, 60 rounds | — | 0 / 10 |
| ZGC | 0 / 3 | 0 / 3 |

The crash, symbolized against the same binary:

```text
EXCEPTION_ACCESS_VIOLATION (SIGSEGV) at pc=…+0x1950211
Faulting address decodes as an indexed load: [rbx+r13*8]   r13=1
  cratonvm_vm::threading::monitor::MonitorTable::exit          monitor.rs:2219
  cratonvm_vm::vm::vm_exec::monitor_exit_and_retract_jmx       vm_exec.rs:4776
  cratonvm_native_builtins::util_concurrent_ext::native_sync_map_get:+0x397
  cratonvm_vm::vm::vm_exec::safe_native_call_impl
```

`[rbx+r13*8]` with `r13=1` is the second header word — `header_of(obj_ref)`.
That decode is character for character the one
`jerseyendpointrequestintegrationtests-sigsegv-generational-FIXED-20260908.md`
records, on a different workload and a different day.

`probes/ConcurrencyUnderGcSweep.java` is the second probe: the blocking half of
`java.util.concurrent` under allocation pressure, every row an exact count, a
per-phase deadline so a leaked monitor shows up as a NAMED phase rather than a
hung run. It is what found the `PriorityBlockingQueue` defect below.

## The rest of the evidence, including the negative result

| measurement | before | after |
|---|---|---|
| `probes/OldToYoungBarrierSweep`, all arms | 2 SIGSEGV / 6 | **0 / 35** |
| Spring Boot subset, 90 classes, plain `Generational` | — | **90 / 90 PASS** |
| the same subset, `GC_STRESS=4194304` + `GC_VERIFY_RSET=1` | 88 PASS, **2 HANG** | 88 PASS, 1 FAIL |
| `probes/NativeLoopReceiverSweep growth`, three-flag harness | 18 rows OK | **identical 18 rows** |
| `probes/ConcurrencyUnderGcSweep`, three collectors | — | **OK on all three** |
| `cargo test -p cratonvm-native-builtins` | — | **4259 passed, 0 failed** |

`NativeLoopReceiverSweep growth` is the regression check the COWAL and LBQ
rewrites needed, and it is the only reason that row is here: its own three-flag
SIGSEGV was root-caused and retired the same day, independently, as
`stale-value-at-set_field-methodhandles-lookup-RETIRED-20260908.md`
(`MethodHandles.lookup()` storing a pre-GC class mirror). It never reproduced on
this Windows box on either binary, which is why nothing here claims it.

The one remaining FAIL is `BindableTests.withAnnotationsShouldSetAnnotations`
(26 of its 27 tests pass), and it is not one of the sites fixed here: the same
class fails the same way on the 2026-09-07 `dev` tip under the same stress
interval, so it is pre-existing. The two classes that HUNG in this configuration
before no longer do — one of them is this class, whose failure mode moved from a
900 s timeout to a 520-830 s failure.

**The negative result is worth as much as the fix.** That stress run produced
**15,369 `[rset-verify]` reports and `missing=0` in every one** — the card table
delivered every old-to-young edge it was asked for. Old-to-young discovery went
card-table-only on 2026-09-02 (`CRATONVM_GC_FULL_RSET_SCAN`, default OFF), four
days before the first of these pages was filed, which made "a missed write
barrier" the obvious suspect and made it worth ruling out with a number rather
than an argument. It is ruled out.

## The gate now has a rule for this

`scripts/stale-receiver-audit.py` grew RULE 2, the park window: a
`ctx.monitor_wait(V, …)` followed, in the same function and before any refresh,
by a use of `V`. Run against the pre-fix tree it reports **7 sites in 6
functions** — every one of the park rows in the table above. Run against the
fixed tree it reports **zero**, and the committed baseline is empty, so a new
one is a red job.

Its selftest asserts all four judgements, not just the positive: the defect
fires, a `read_native_pin` between wait and exit does not, a refresh written
through a deref (`*obj = …`) does not, and a wait in TAIL position does not.
The deref case is there because the rule's own first version failed it —
`*obj = ctx.read_native_pin(pin, *obj);` was read as a block-comment
continuation, so the scan walked past the refresh and reported the CORRECT line
after it. A rule that reads `*` as prose cannot see a Rust deref; see
`is_comment_line`.

## Two things found on the way, both landed, neither this defect

### `CRATONVM_DBG_DEADRECV` reported mostly noise

The guard asked two reclamation rings whether an address had been freed. Neither
ring is pruned when the allocator hands the span back out — `record_young_span_freed`
appends one entry per coalesced span and nothing removes it — so **every object
later allocated inside a swept span answers `young_freed_lookup` for the rest of
the process**. The rings remember what was freed, not what is dead.

It now asks `is_object_address` first, which is the opposite question and the
one the guard wants: the arena's object-start bitmap records "a base this arena
handed out and has NOT freed". What that trades away is stated at the site: an
ABA — a stale reference to an address since re-served for a different object —
now reads as live. `CRATONVM_DBG_VACATED_FRAMES` is the instrument that tracks
re-issue exactly.

That **retires a third page**.
`gpuresidencygc-generational-jit-reclaims-a-live-object-FIXED-20260908.md` read
"8 hits, every run, with the JIT on; 0 with `--nojit`" as its remaining
evidence. Measured on its own command:

| arm | rc | guard hits | wall |
|---|---:|---:|---:|
| before, `Generational`+JIT, armed | 1 (CME) | 8, 8, 8 | 163 / 185 / 251 s |
| before, `Generational` `--nojit`, armed | 0 | 0, 0 | 428 / 475 s |
| before, `Generational`+JIT, **unarmed** | 0 | 0 | 4 / 4 / 4 s |
| **after the screen**, armed | **0** | **0, 0, 0** | **4 / 4 / 4 s** |

Every hit was a re-allocated address. The `--nojit` split was never about the
JIT either: only the non-moving sweep calls `record_young_span_freed`, and
whether a JIT frame is live is exactly what selects that sweep — so the split is
a statement about which collector ran, and would read the same on a VM with no
defect at all.

The 40-60x wall-time gap in that table is the second half of the finding. Both
lookups are LINEAR scans of their rings, and the old-gen ring is 2^20 entries,
on every `identity_hash_code` and `class_id_of_object`. The young ring's header
comment rejects the gated forensics it replaced for precisely this reason — "the
instrument changed the thing it measured" — and its WRITE side is O(1) and
lock-free as designed. Nothing had said the read side was not.

### `PriorityBlockingQueue.poll(long, TimeUnit)` never woke on a `put`

Found by `ConcurrencyUnderGcSweep`'s `priorityblockingqueue` phase, which
expired its 120 s deadline while every other queue passed in milliseconds.

The natives own `offer`/`put`/`poll()`/`take()` and coordinate through the
receiver's object monitor. `poll(long, TimeUnit)` was left to the real class's
own bytecode, which waits on `notEmpty` under `lock` — and no `Condition` waiter
is on an object monitor, so `monitor_notify_all` never reached them. Reduced to
four lines (an empty queue, a producer that puts after 300 ms): HotSpot returns
in under a second, this VM took more than one, and at 480 elements the probe's
120 s phase deadline expired.

**The obvious repair is wrong, and it was tried first.** Signalling `notEmpty`
from the native insert does wake the real body promptly — and that body then
sifts down through `PBQ_FIELD_DATA`/`PBQ_FIELD_SIZE` as a BINARY HEAP, while
every native here keeps them a SORTED ARRAY. The two representations agree only
for zero or one element, so the fast arm produced
`NullPointerException: Cannot invoke "java.lang.Comparable.compareTo(Object)"
because "key" is null` and consumed 1 of 480. Waking a mixed path is worse than
starving it.

The landed fix stops the mixing instead: `poll(long, TimeUnit)` is now a native
(`native_pbq_poll_timed`), the same shape as `take()` with a deadline, on the
native representation and the same object monitor as its siblings. The remaining
real-bytecode readers (`drainTo`, `remove(Object)`, `iterator`) still walk a
sorted array through a heap-shaped body; that is pre-existing, unchanged, and
the representation mismatch is the real defect there — it wants its own page.

## The family is NOT declared closed, and here is the residual

The stressed subset produced one reclaimed-receiver report that none of the nine
fixes touches:

```text
receiver is inside a YOUNG span the non-moving sweep zeroed and returned to the
free list.  site="invoke dispatch"  actual_class_id=0
  target_class=java/lang/Object.asGenericType()Lnet/bytebuddy/…/TypeDescription$Generic;
…and it was RECLAIMED BY THE YOUNG SWEEP while still reachable.
  original_class=net/bytebuddy/description/type/TypeDescription$Generic$OfNonGenericType$ForLoadedType
```

That is the ALWAYS-ON consumer, not the ring-based one this page corrects: it
fires on an interpreter INVOKE whose receiver reads an all-zero header AND whose
address `reclaimed_hole_at` finds on the free list right now, so neither half of
it can be a re-allocation false positive.

**It is PRE-EXISTING.** The `dev` tip of 2026-09-07 20:29 — built the day before
any of this work — reports the same victim class at the same sweep cycle
(`6353`) on the same reproducer. These fixes neither caused it nor cured it.

**And it is not what fails the test**, which is a claim this page made in its
first revision and which a later run refutes: on the merged tree the class
failed with the same `MockitoException` and the guard reported nothing at all.
Two co-occurrences were a coincidence of a deterministic workload. The reclaim
and the Mockito failure are two separate pre-existing things in one class.

Filed with the evidence, the ruled-out mechanisms, and the most specific lead
(the predicate guarding the conservative frame-slot probe is narrower than the
collector's own, so an A5 unregistered-JIT-frame cycle ran the non-moving sweep
with the root-widening pass off):
`docs/internal/springboot/bindabletests-bytebuddy-receiver-reclaimed-under-gc-stress-20260908.md`.

That lead was a real gap and is fixed (`collect_roots` step 14a5), and of the
THREE disagreeing predicates the page named, one turned out to be a disjunct
that could only ever read `false` — deleted. Neither was the reclaim: the page
now carries a two-minute Linux reproducer and stays OPEN.

## What this does NOT claim

* The Jersey page's own crash was **not reproduced locally** — that class does
  not currently load in this checkout (`JerseyAutoConfiguration` is missing from
  the module's generated test classpath, on HotSpot too, so it is a fixture gap
  rather than a VM one). It is retired on the strength of the identical fault
  decode and the identical faulting site, not on a re-run.
* The `ConcurrentSkipListMap` bridge in `util_concurrent_ext.rs` carries the
  same shape and is deliberately **not** touched: `registrar_reachability`
  records it as REAL-JDK BYTECODE, and `native-collections`'
  `concurrent_skip_list_map_not_intercepted` pins that the natives stay
  unregistered, so it is unreachable in every configuration these reports use.
* Nothing here is a collector change. The young sweep's own root-in-dead-span
  invariant (`gen_heap.rs`) was already unconditional and already retains a span
  a root points into; the references in this family were in no root set at all,
  which is why it stayed quiet.
