# `TestMultiThread` — MVStore background writer sees an object of the wrong class

> Moved here from `known-issues/h2/` on 2026-08-17. The page as filed
> (2026-08-16) is preserved below the fix, because two of its measurements are
> load-bearing and one of its guesses was wrong in an instructive way.

## Status

**FIXED 2026-08-17** — branch `fix/h2-mvstore-writer-object-identity-20260816`.

**Root cause: `Collections.synchronizedSet` / `synchronizedMap` /
`synchronizedList` never took their `mutex`.** The wrapper's whole contract is
the lock; CratonVM's natives forwarded straight to the backing collection and
the `mutex` field, written by the constructor, was read by nobody. A registered
native shadows the class's own bytecode at every dispatch site, so the real JDK
implementation could not compensate — and `SynchronizedList`, which has no
natives of its own, inherits `add`/`remove`/`size` from `SynchronizedCollection`
and lost elements too.

H2 keeps `org.h2.util.CloseWatcher.refs` — a set of `PhantomReference`s, one per
connection — in exactly that wrapper (`CloseWatcher.java:32`), as it keeps
`Database.userSessions` (`Database.java:145`) and `TcpServer.running`. Under
`testConcurrentUpdate`'s 25 connections the racing `HashSet` loses entries, so a
`CloseWatcher` becomes unreachable **while the VM's `ReferenceProcessor` is
still tracking it**. Its address is reclaimed, re-issued to an unrelated
allocation, and the processor's write sites then write through it. Two of those
sites had no guard that could see this:

* the **pre-GC referent-null pass** (`weakref_null_referents_pre_gc`) had only
  `num_fields >= 2` — which `org.h2.engine.SessionLocal` passes, and every
  `org.h2.value.Value` passes. It nulled slot 0 of whatever now lived there:
  the `NullPointerException: Cannot invoke "org.h2.value.Value.getValueType()"
  because "v" is null` witness;
* the **post-GC restore pass** writes an object into slot 0, and its guard is a
  class-shape test, which cannot tell a reclaimed `Reference` from another
  `Reference` re-issued at the same address — and H2 allocates a `CloseWatcher`
  per connection, so same-class re-issue is the common case: the
  `NoSuchMethodError: 'byte[] java.lang.String.toByteArray()'` witness.

That is why the page below records the failure as surviving "the shape guard
that now screens every reference-processor write". It did: the pre-GC pass was
not one of the screened writes, and the screened ones are shape tests.

### The three fixes

| # | Fix | File |
|---|-----|------|
| 1 | `Collections$Synchronized{Collection,Set,Map}` natives take the wrapper's `mutex` | `native-builtins/src/util_concurrent_ext.rs` |
| 2 | the pre-GC referent-null pass gets the class-shape guard the post-GC loops already had | `vm/src/runtime/interpreter.rs` |
| 3 | an **identity stamp** — the identity hash the object carried at `discover_reference` — checked at the pre-GC null pass and all three post-GC write sites | `gc/src/reference.rs`, `vm/src/runtime/interpreter/gc_and_alloc.rs`, `vm/src/vm/vm_exec.rs` |

Fix 1 removes the producer, fixes 2 and 3 remove the writers. Fix 3 is the
"identity stamp rather than a shape test" this page's own *Next steps* asked
for, and it needed nothing new in the object: `VmHeap::identity_hash_code`
already mints from a monotonic counter into the object's own mark word, and the
mark word travels with the object when a collector relocates it. Both "cannot
tell" answers (an unstamped entry, or a thin-locked object whose hash is
displaced out of the mark word) fall back to the shape guard rather than
declining, so the stamp can refuse a write but can never lose a legitimate one.

Locking in fix 1 is deliberately `monitor_enter_gc_safe`, not `monitor_enter`:
these wrappers are contended by construction and the owner is inside
`HashMap.put`, which allocates and can be parked at a safepoint holding the
mutex — a plain `monitor_enter` leaves the waiter counted in the STW barrier's
`expected` set, which is the three-way wedge `Monitor::block_enter` documents.
The GC-safe wait can span a moving collection, so `this`, the mutex and every
reference argument are pinned across it and re-read afterwards.

### Measured

**The wrapper, 8 threads x 4000 distinct elements added and then removed
(`SyncSetProbe`):**

```
                HOTSPOT   BEFORE   AFTER
  set.size()       0       3716      0
  map.size()       0       1888      0
  list.size()    32000     26540   32000
```

`3716` is the same defect the page below reports as `size() == -26` and files
as "probably a separate defect worth its own look". It was not separate; it was
the producer.

**The reference residue this page describes, isolated
(`PhantomIdentityProbe -Dsyncset=true`: 8 threads, 3200 `PhantomReference`s
through one queue, registry = `Collections.synchronizedSet(new HashSet<>())`,
delivered = polled back out):**

```
  HOTSPOT   3200 3200 3200
  BEFORE    2616 3106 3189 3184        <- references LOST from the racing set
  AFTER     3200 3200 3200 3200
```

With the registry swapped for a `ConcurrentHashMap`-backed set, *both* builds
deliver cleanly — which is what identifies the wrapper, not the reference
processor, as the thing that was losing them.

**End to end.** `MvidRepro` is `testConcurrentUpdate` standalone (25
connections x 1000 committed `UPDATE`s over a 10000-row table): the whole
class needs >900 s to reach that method, one extraction is ~290 s, and under
`CRATONVM_DBG_GC_STRESS=4194304` with `-Dupdates=120` it is ~70 s with 80
collections and 72 compactions per run instead of 2.

```
  before, unstressed, ZGC default   2 of 6 runs FAIL
  before, GC-stressed               1 of 37 runs corrupt
  after,  GC-stressed               0 of 34 runs corrupt
  after,  unstressed, ZGC default   0 of 15 runs (incl. -XX:+UseGenerationalGC)
```

The stressed rate is low enough on its own that the A/B alone would not settle
it — the probes above are what settle it, and the end-to-end runs are the
confirmation that nothing else in the chain is still open.

**Gates.** `cratonvm-gc` 1583/0.

### Two negatives worth keeping

* **The compaction correlation is real but is not the defect.** Interleaved
  ABBA on the pre-fix binary: relocation ON, 2 of 6 runs FAIL; relocation OFF
  (`CRATONVM_ZGC_RELOCATE=0`), 0 of 14. Compaction is what re-issues a
  reclaimed address quickly enough for the stale write to land on something
  live, so it is the amplifier, not the cause. `CRATONVM_DBG_ZGC_VERIFY_SLIDE=1`
  reports `missed_rewrites=0` on the same workload: the slide's own rewrite
  pass is complete, and no heap slot is left naming a vacated address.
* **Parked frames were not it either.** `ParkedLocalProbe` (8 threads holding
  one object in a local and one in a field across `Thread.sleep`, 4 churn
  threads forcing collections, 22304 objects relocated over 3 compactions):
  `bad=0` on CratonVM and on HotSpot.

### Residual

`PhantomIdentityProbe` **without** the wrapper still under-delivers
occasionally on CratonVM — 3110-3200 of 3200, on both the pre-fix and the
fixed binary, where HotSpot delivers 3200 every time. That is a separate,
smaller reference-delivery gap; it is not this defect (it predates the fix and
is unchanged by it), and it does not corrupt anything — the references that do
arrive are the ones registered, with the right identity.

### Instrument added while chasing this

`VmHeap::reclaimed_hole_at` and `live_holders_of` answered `None`/empty on ZGC,
the default collector since 2026-08-10, so the whole flag-free
reclaimed-receiver verdict in `vm/src/memory/reclaim_guard.rs` was inert exactly
where this defect lives: the reproduced failure logged 13 `gc::guard` lines, all
of them unrelated startup warnings. ZGC now answers both (arena free list, then
the un-bumped middle between the two cursors; holders decoded through
`reference_slots`), and `report_reclaimed_receiver` additionally consults ZGC's
relocation ledger when `CRATONVM_DBG_ZGC_CORPSE` armed the run — which is what
separates "the holder was never rewritten when its referent moved" from "the
object died afterwards".

---

## The failure, as filed 2026-08-16

Both witnesses are the MVStore **background writer** thread committing
`.../data/test/lockMode.mv.db`, inside `TestMultiThread.testConcurrentUpdate`,
`--nojit`, `--Xmx 1g`:

```
[cratonvm] WARN vm_exec: NoSuchMethodError
    method="java/lang/String.toByteArray()[B"
    caller="org/h2/mvstore/db/ValueDataType.write(Lorg/h2/mvstore/WriteBuffer;Lorg/h2/value/Value;)V @pc=529"

Exception in thread "MVStore background writer .../lockMode.mv.db"
  org.h2.mvstore.MVStoreException: java.util.concurrent.ExecutionException:
  org.h2.mvstore.MVStoreException: java.lang.NoSuchMethodError:
  'byte[] java.lang.String.toByteArray()' [2.4.249/3]
	at org.h2.mvstore.MVStore.panic(MVStore.java:515)
	at org.h2.mvstore.MVStore.storeNow(MVStore.java:992)
	at org.h2.mvstore.FileStore$BackgroundWriterThread.run(FileStore.java:2266)
```

`ValueDataType.write` reaches `toByteArray()` on the `BigInteger` it takes from
a numeric `Value`. The receiver at that call site is a `java.lang.String`.
Nothing in H2 can put one there.

The second witness is the same thread, same file, same panic path, with the
wrong object arriving as a null instead:

```
Exception in thread "MVStore background writer .../lockMode.mv.db"
  ... java.lang.NullPointerException: Cannot invoke
  "org.h2.value.Value.getValueType()" because "v" is null [2.4.249/3]
	at org.h2.mvstore.MVStore.panic(MVStore.java:515)
```

Two more faces of the same defect were measured during the fix, on the
extracted repro and on the full class:

```
java.lang.NoSuchMethodError: 'int java.lang.Object.compareWithNull(
    org.h2.value.Value, org.h2.value.Value, boolean)'      <- receiver: SessionLocal
java.lang.ClassCastException: [Lorg.h2.value.Value;
    cannot be cast to org.h2.mvstore.Page                  <- MVStore background writer
```

`java.lang.Object` as the receiver's class is the all-zero header the collector
leaves over a span it reclaimed, not a real `Object`.

### Why it was a residual and not a regression

Measured in one ABBA-interleaved sequence (`A B B A A B B A`, 8 runs, same host,
same session, `A` = pristine `origin/dev` @ `ecc09d40d`, `B` = the same tree plus
the reference-queue fixes):

| arm | result |
|---|---|
| A (pristine dev) | 4/4 FAIL — 3x `ClassCastException` out of `ReferenceQueue.poll()`, **1x this defect** |
| B (fixed) | 3/4 PASS, **1x this defect** |

So it fired on the pristine build too; on `A` it was usually pre-empted by the
louder reference-queue bug, which is why it had not been seen on its own before.
Distinct from the retired `bug-h2-testmultithread-npe-array-length-null-local-concurrent-connect`
write-up: its two signatures (`ClassCastException: ... cannot be cast to class
org.h2.util.CloseWatcher` / `... FileLockTable$FileLockReference` out of
`ReferenceQueue.poll()`) no longer occur, and no `java.lang.ref` frame appears
anywhere in this one.

## Repro

```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

Roughly 1 run in 4 on a loaded host; run at least 6 before calling it absent.
The extracted `testConcurrentUpdate` (`MvidRepro`) plus
`CRATONVM_DBG_GC_STRESS=4194304 -Dupdates=120` is the same failure in ~70 s, and
is what the A/B above used. Two greps discriminate this from the retired
reference-queue bug, and both must be empty:

```bash
grep -c "cannot be cast to class org.h2.util.CloseWatcher" err.log   # retired bug
grep -c "Cannot read the array length"                     err.log   # retired bug
```
