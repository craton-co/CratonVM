# `TestMultiThread` — MVStore background writer sees an object of the wrong class

## Status

**STILL OPEN 2026-08-17, but no longer unexplained.** **Six** real defects
behind it were found and fixed on
`fix/h2-mvstore-writer-object-identity-20260816` — three in the reference
machinery (below) and three in the stale-reference family the second pass went
after — one of which this page had filed as a separate curiosity. The failure
itself still reproduces, and the mechanism now has a measured name instead of
four candidate explanations.

**What the failure IS, measured:** a live object relocated by the ZGC slide,
with one holder never rewritten. The verdict comes out of the collector itself
now (see *Instruments added*):

```
receiver names an address the ZGC slide VACATED.  site="invoke dispatch"
  obj=0x20043d013f8  vacated_from=0x20043d013f8  moved_to=0x2004382e808
  original_class=org/h2/engine/SessionLocal  original_size=1056
  target_still_live=true
```

`target_still_live=true` is the whole finding: the object is alive at its new
address, so this is not a lifetime bug and not a reclamation bug. Something kept
naming the old address after the collector moved it, and the address has since
been re-occupied — which is why the receiver reads as `java.lang.Object`
(zeroed), as a `java.math.BigDecimal`, or as a `java.lang.String`, depending on
what landed there.

### What has been RULED OUT, each by measurement

| Hypothesis | Evidence against |
|---|---|
| the reference processor writing through a stale address | fixed (below), and the failure survives the fix |
| a heap reference slot the slide's rewrite pass missed | `CRATONVM_DBG_ZGC_VERIFY_SLIDE=1`: `missed_rewrites=0`; the 35 unregistered targets are W7-84 primitives, `0 aliasing` |
| a parked thread's frame locals/fields going stale over `Thread.sleep` | `ParkedLocalProbe`: 8 holders x 400 rounds across 22304 relocated objects over 3 compactions — `bad=0`, same as HotSpot |
| a `long`/`double`-kinded local the remap refuses to rewrite | `CRATONVM_DBG_BUG03=1`: 5534 in-map local decisions, **every one** `skip_kind=false skip_nonobj=false` |
| a thread that no heal path reached | `thread_last_heal == heap_collection` at every reported stale slot |
| a waiter applying another pause's pointer map | `CRATONVM_DBG_MAPGEN=1`: **0** mismatches — every waiter gets the map of the pause it arrived for |
| a thread running Java while censused as blocked | `CRATONVM_DBG_BLOCKED_ACCESS=warn`: 0 violations |
| a live frame slot left naming a vacated address | 0, once the ledger stopped counting re-issued addresses — the 8-per-run the first version of that instrument reported were fresh allocations in the vacated span |
| a stale reference being STORED into a frame local | `set_local` detector: 0 hits across every reproduced failure |
| a stale reference being PUSHED on the operand stack | push detector (both `Value` and compact paths): 0 hits |
| a field read through a stale RECEIVER | `get_field` receiver detector: 0 hits |

### The stale-reference hunt (2026-08-17, second pass)

The address is not surviving the heal — it is **re-introduced after it**, by
code holding an `ObjectRef` in a Rust local where no root scan can see it. Three
defects of that family were found by pointing the (now exact) vacated-address
ledger at the forwarding barrier, and all three are fixed:

* **The forwarding barrier had nothing to read on this collector.**
  `VmHeap::load_and_forward` — the repair every one of its 46 call sites relies
  on, precisely because its caller holds a reference the collector cannot see —
  works by reading a FORWARDING WORD at the old address. ZGC's slide leaves
  none: `Arena::compact_low_to` zeroes the span above the new cursor and the
  memmove overwrites the rest. **The barrier was a silent no-op on the default
  collector.** The slide now publishes its `from -> to` pairs into
  `ZgcRealHeap::relocations` and the barrier consults them when the address is
  not a live object base (which is also why a re-issued address can never reach
  the table, so no pruning is needed for correctness).
* **`apps_h2::h2_comparison_compare` / `h2_comparison_get_value`** kept
  `session`, `left`, `right` and the two operand expressions in Rust locals
  across several `ctx` callbacks. Caught red-handed: with the ledger armed, a
  failing run logged the barrier being handed a moved address from exactly these
  two functions, and the failure it produced is the `SessionLocal` receiver at
  the top of this page. Now pinned and re-read.
* **`NativeContext`'s write entry points forwarded the RECEIVER but not the
  VALUE.** A native that read an object before a callback and stored it
  afterwards wrote a stale pointer straight into the heap
  (`properties_sidetable::mirror_loaded_entries_to_properties_backend` was
  caught doing it), where the next reader `checkcast`s it. `set_field`,
  `set_field_by_name`, `set_array_element` and `set_static_field` now forward
  the value too.

**The failure still reproduces**, so at least one producer of the same family is
still open: the surviving witnesses are `ClassCastException` at a `checkcast`,
i.e. a stale pointer read back out of a heap slot or read THROUGH a stale
receiver, with no `load_and_forward` on the path to catch it. The A/B over the
fix set is inconclusive at the rates measured (pre-fix 4 of 12 corrupt,
post-fix 3 of 8), which is exactly what one would expect if each fix removes one
producer out of several.

**What is no longer in doubt:** the residual is entirely a compaction problem.
`CRATONVM_ZGC_RELOCATE=0` is now **29 runs, 0 failures** (14 on the pre-fix
binary, 15 on the fixed one) against roughly a third of runs corrupt with
relocation on, under identical GC stress.

### Third pass: where the remaining producer is NOT

The two consumption points the barrier does not sit on were instrumented — the
operand-stack push (both the `Value` and the compact path) and the field-read
RECEIVER — and both report **zero** across every reproduced failure, alongside
`set_local`'s zero. So at the moment of the failure there is no reference to a
vacated-and-not-yet-reissued address anywhere in play.

That is not the absence of a defect; it is the instrument going quiet exactly
when the damage becomes visible. The exact ledger drops an address the instant
the allocator re-issues it (which is what makes it exact), and the failure only
becomes *observable* after re-issue: until then the stale holder reads the
zeroed corpse, and afterwards it reads a valid object of the wrong class. The
`checkcast` reporter, which consults the collector's own relocation history
rather than the ledger, still says what it always said:

```
receiver names an address the ZGC slide VACATED ... target_still_live=true
…and this is where that address stood in the OWNING thread's own GC
   bookkeeping. in_published_snapshot=false — the snapshot the collector marks
   this thread from did not contain a slot the thread's frames hold: a root
   COLLECTION gap, not a mark or sweep one.
```

**The one lever that moves it:** `CRATONVM_NO_LOCAL_LIVENESS=1` — the kill
switch for the per-bci local-liveness root filter. Interleaved ABBA with the
detectors armed on both arms:

| arm | runs | corrupt |
|---|---|---|
| A — per-bci liveness ON (default) | 12 | **5** |
| B — `CRATONVM_NO_LOCAL_LIVENESS=1` | 8 | **0** |

**Read that as masking, not as the culprit.** The filter's contract is
per-instruction bytecode liveness, and the slot it was caught dropping —
`MVPrimaryIndex.lockRow pc=22 local[2]`, traced by recording every address the
filter withholds and looking the failing receiver up in it — is the `Row`
parameter at the method's `areturn`, which is genuinely dead: nothing reads it
again. What the filter changes is how quickly a dead object's address becomes
**re-issuable**, and re-issue is what turns a latent stale holder into a
`ClassCastException`. Retaining every dead local hides the defect by keeping the
address occupied by the right object.

### Fourth pass: the root inventory is clean, and one near-miss

Two more instruments, and a fix that was nearly landed on an artifact.

* **The two in-pause frame verifiers were reading a forwarding word ZGC never
  writes.** `ARRIVE-STALE` and `WAKE-STALE` asked `debug_forwarded_target`, so
  both reported zero on the default collector whatever the truth was. Each now
  uses the record it already holds — this collection's `pointer_map` at the
  arrival site, the accumulated `fixup` chain at the wake site. These are the
  only EXACT places to ask: the remap has just run and no mutator on the thread
  has resumed, so a slot holding a map key is unambiguously one the remap did
  not reach. **Result: 0 across 23 runs**, once slide DESTINATIONS are excluded
  (before that exclusion it "found" 80-168 a run, every one a slot legitimately
  holding the survivor that slid INTO a vacated address).
* **`CRATONVM_DBG_ROOT_REMAP_AUDIT=1`** re-runs the root scan at the end of
  `update_all_roots` and looks for an address this collection moved. The scan
  inventory (`roots.rs`) and the remap inventory (`native_roots.rs`) are two
  different lists, and a source in the first but not the second is exactly this
  defect's shape. `collect_roots` is now labelled by section, so a hit names
  which of its forty sections produced the root. **Result: 0.**

**The near-miss, recorded because it nearly shipped.** Placed BEFORE the
blocked-thread fold, the audit reported 387-407 un-remapped roots a run, every
one from section 11, "Root snapshot (for cross-thread GC scanning)" — which
reads exactly like "the fold skips non-blocked threads, so their published
snapshots are never rewritten". A fix for that was written, and then the control
(`CRATONVM_NO_UNBLOCKED_SNAPSHOT_REMAP=1`, one binary, audit at the END) reported
**0 with the fix and 0 without it**: every stale snapshot entry belonged to a
BLOCKED thread and the existing fold already handled it. The 387 were an artifact
of where the audit ran, not a finding. The change was reverted and the reasoning
left in `fold_pointer_map_into_blocked_audited` for whoever measures the
excluded-thread race for real.

### Where the reference must therefore be

Heap slots (slide verifier), frame locals and stacks (both heal sites, exact
predicates), and the entire scanned root inventory are each verified complete at
the end of the pause. So the holder is none of them: it is a raw `ObjectRef` in
VM-side state that is **neither scanned nor remapped** — a native's Rust local
across a callback, or a side table in neither inventory. That is the same class
the barrier backtraces caught twice already (`apps_h2`,
`properties_sidetable`), and the `load_and_forward` instrument from the second
pass is the one that names them, one at a time, as each is fixed.

**Next step:** the ledger has to survive re-issue to catch the USE. Keep the
full vacated history and disambiguate with the identity hash minted into the
object at slide time (`VmHeap::identity_hash_code` already provides one, and the
reference processor's stamp shows the pattern): a holder whose address now
carries a different identity than the one recorded for it at the move is stale,
whether or not the space has been handed out again. That is the one instrument
that can name the holder after re-issue, which is where every current one goes
blind.

## The three reference-machinery defects fixed on the way

### 1. `Collections.synchronizedSet` / `synchronizedMap` / `synchronizedList` never took their `mutex`

The natives forwarded straight to the backing collection; the `mutex` field was
written by the constructor and read by nobody. A registered native shadows the
class's own bytecode at every dispatch site, so the real JDK implementation
could not compensate — and `SynchronizedList`, which has no natives of its own,
inherits `add`/`remove`/`size` from `SynchronizedCollection` and lost elements
too. 8 threads x 4000 distinct elements added then removed (`SyncSetProbe`):

```
                HOTSPOT   BEFORE   AFTER
  set.size()       0       3716      0
  map.size()       0       1888      0
  list.size()    32000     26540   32000
```

`3716` is this page's own `size() == -26`, which it filed as "probably a
separate defect worth its own look". It was not separate: H2 keeps
`CloseWatcher.refs` (a set of `PhantomReference`s, one per connection) in one of
these wrappers, and losing entries from it kills a `Reference` the VM's
processor is still tracking. Isolated with `PhantomIdentityProbe -Dsyncset=true`
(3200 phantom references through one queue on 8 threads; `delivered` = polled
back out):

```
  HOTSPOT   3200 3200 3200
  BEFORE    2616 3106 3189 3184
  AFTER     3200 3200 3200 3200
```

With a `ConcurrentHashMap`-backed registry instead, both builds deliver cleanly
— which is what identifies the wrapper rather than the reference processor.

Contended entry is `monitor_enter_gc_safe`, not `monitor_enter`: these wrappers
are contended by construction and the owner is inside `HashMap.put`, which
allocates and can be parked at a safepoint holding the mutex — a plain
`monitor_enter` leaves the waiter counted in the STW barrier's `expected` set,
which is the three-way wedge `Monitor::block_enter` documents. The GC-safe wait
can span a moving collection, so `this`, the mutex and every reference argument
are pinned across it and re-read afterwards.

### 2. The pre-GC referent-null pass was the one unscreened reference-processor write

`process_references_after_gc`'s cleared / enqueue / restore loops were given a
class-shape guard on 2026-08-16. `weakref_null_referents_pre_gc`, which performs
the same kind of write through the same kind of address, kept only
`num_fields >= 2` — which `org.h2.engine.SessionLocal` passes, and every
`org.h2.value.Value` passes. That is why this page could record the failure as
surviving "the shape guard that now screens every reference-processor write": it
did, because this write was not one of the screened ones.

### 3. The same-class hole is closed by an identity stamp

A shape guard cannot tell a reclaimed `Reference` from ANOTHER `Reference`
re-issued at the same address, and H2 allocates a `CloseWatcher` per connection,
so same-class re-issue is the common case rather than the exotic one. Every
entry now carries the identity hash the object had at `discover_reference` time
(`VmHeap::identity_hash_code` mints from a monotonic counter into the object's
own mark word, and the mark word travels with the object across a relocation —
the "monotonic registration id written into the object" this page's *Next steps*
asked for, already present). Checked at the pre-GC null pass and all three
post-GC write sites. Both "cannot tell" answers — an unstamped entry, and a
thin-locked object whose hash is displaced out of the mark word — fall back to
the shape guard rather than declining, so the stamp can refuse a write but can
never lose a legitimate one. Verified: `PhantomIdentityProbe` delivers exactly as
before the stamp (3110-3200 of 3200, unchanged distribution) with zero
`SKIP reidentified`.

## Instruments added (each of them answered something this page could not)

* **`VmHeap::reclaimed_hole_at` / `live_holders_of` now have ZGC arms.** They
  answered `None`/empty on the DEFAULT collector since 2026-08-10, so the whole
  flag-free reclaimed-receiver verdict in `vm/src/memory/reclaim_guard.rs` was
  inert exactly where this defect lives: a reproduced failure logged 13
  `gc::guard` lines, all of them unrelated startup warnings.
* **`report_reclaimed_receiver` consults ZGC's relocation ledger** when
  `CRATONVM_DBG_ZGC_CORPSE` armed the run. That is what produced the verdict at
  the top of this page.
* **The last un-migrated copy of that verdict — `checkcast` — now calls the
  shared reporter.** The re-served face of this defect has a non-zero class id,
  which is exactly what the local copy's `ClassId(0)` gate suppressed.
* **`CRATONVM_DBG_VACATED_FRAMES=1`** records one collection's pointer-map keys
  and reports any LIVE frame slot still naming one at the next safepoint, with
  thread, method, pc, slot, the class now at the address, and where the original
  went. An address that is also a slide DESTINATION is excluded — survivors
  slide onto vacated addresses, and reporting those manufactures findings (the
  first version of this instrument did, on `ThreadPoolExecutor.runWorker`).
* **`CRATONVM_DBG_MAPGEN=1`** checks that a safepoint waiter gets the pointer map
  of the pause it arrived for — the release condition and the map read are two
  different facts. It reports 0, which retires that hypothesis.

## Repro

```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

Roughly 1 run in 4 on a loaded host. The extracted `testConcurrentUpdate`
(`MvidRepro`: 25 connections x 1000 committed `UPDATE`s over a 10000-row table)
plus `CRATONVM_DBG_GC_STRESS=4194304 -Dupdates=120` is the same failure in ~70 s
with 80 collections and 72 compactions per run instead of 2 — measured about 1
in 15 that way, against >900 s for the whole class to reach the method at all.
**`rc=0` is not a pass**: the MVStore background writer's panic does not fail the
main thread, so grep the stderr:

```bash
grep -cE "Exception in thread|NoSuchMethodError|ClassCastException|Cannot invoke" err.log
```

Two greps discriminate it from the retired reference-queue bug, and both must be
empty:

```bash
grep -c "cannot be cast to class org.h2.util.CloseWatcher" err.log   # retired bug
grep -c "Cannot read the array length"                     err.log   # retired bug
```

## The compaction correlation

Interleaved ABBA on the pre-fix binary: ZGC relocation ON, **2 of 6** runs FAIL;
relocation OFF (`CRATONVM_ZGC_RELOCATE=0`), **0 of 14**. Compaction is what
re-issues a vacated address quickly enough for the stale holder to reach a live
object of another class, so it is the amplifier — and, on the evidence above,
also the necessary condition. `CRATONVM_ZGC_RELOCATE=0` is therefore a usable
mitigation for a workload that hits this, at the cost of the only
defragmentation this collector has.

## Original witnesses (filed 2026-08-16, unchanged)

Both are the MVStore **background writer** thread committing
`.../data/test/lockMode.mv.db`, inside `TestMultiThread.testConcurrentUpdate`,
`--nojit`, `--Xmx 1g`:

```
[cratonvm] WARN vm_exec: NoSuchMethodError
    method="java/lang/String.toByteArray()[B"
    caller="org/h2/mvstore/db/ValueDataType.write(Lorg/h2/mvstore/WriteBuffer;Lorg/h2/value/Value;)V @pc=529"
```

```
Exception in thread "MVStore background writer .../lockMode.mv.db"
  ... java.lang.NullPointerException: Cannot invoke
  "org.h2.value.Value.getValueType()" because "v" is null [2.4.249/3]
```

Four more faces were measured during this work, all the same mechanism:

```
NoSuchMethodError: 'int java.lang.Object.compareWithNull(
    org.h2.value.Value, org.h2.value.Value, boolean)'   <- receiver was a SessionLocal
ClassCastException: [Lorg.h2.value.Value;      cannot be cast to org.h2.mvstore.Page
ClassCastException: java.math.BigDecimal       cannot be cast to org.h2.mvstore.Page
ClassCastException: java.lang.ref.WeakReference cannot be cast to ...
```

It is present on pristine `origin/dev` as well as on every build since; on the
pristine arm it was usually pre-empted by the louder reference-queue bug
(retired 2026-08-16), which is why it had not been seen alone before. HotSpot
JDK 25 passes the same class on the same classpath in the same sessions.
