# Proposal: discover `Reference`s in the marker, retire the pre-null / restore protocol

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 9
> of 54).** Not built. Still the precondition for retiring the `java/lang/ref`
> bridges under `--jdk-only`. Absorbs the remainders of
> `../../internal/gc/common-d-proposal-soft-reference-clock-without-the-global-lock-REJECTED-20260928.md` and
> `../../internal/gc/common-w4d-proposal-full-reference-delivery-by-default-REJECTED-20260928.md` (sections at the
> end). **Gate:** the page's entry ticket:
> `CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/ref/` plus `new
> WeakReference<>(new Object(), q); System.gc();` prints `cleared=true
> enqueued=true` on the staged collector; then the reference probes
> (cd_weakfin, mark2_remarkrefs, `GenR4DroppedReferenceLeakProbe`) unchanged
> per collector. **Size:** L (class-shape bit, three markers, processor
> rewrite; staged per collector).

> **STATUS (gc-common wave 6, lane D, 2026-09-24): OPEN (proposal). Design
> refreshed. No first step was taken in the marker: none is safe from a
> common lane, and the reason is now concrete. This page absorbs the retired
> cold-list proposal.**
>
> **The w5-d finding, restated as the design constraint.** The registry
> learns of a `Reference` ONLY from the `Bridge` constructor natives. With
> those bridges retired (the `CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/ref/`
> dial), nothing is ever cleared. No common-layer hook can replace them:
>
> * The real JDK 25 `Reference` has no constructor native.
> * `get()` is plain field access, intrinsified, with no native.
> * `refersTo0` and `clear0` see a reference only when the program calls
>   them.
>
> So discovery has to come from the one place every `Reference` passes
> through, the marker's object scan. Step 1 below (a `ReferenceKind` bit in
> the class shape) is therefore not an optimization. It is the precondition
> for retiring the bridges at all, and it lives in class linking
> (`classloading`) plus all three markers, all outside this round.
>
> **Five things the common layer did this round that the marker design
> inherits, rather than replaces.**
>
> 1. The phase logic in `gc/src/reference.rs` (soft, weak, final, phantom)
>    is marker-agnostic and survives, as step 3 says.
> 2. The w6-d depth-1 rule, `ReferenceProcessor::note_resurrected_finalizables`,
>    is HotSpot's ordering (weak cleared before final) expressed on a
>    registry. A marker that discovers references gets it for free, and at
>    every depth.
> 3. The w6-d soft-touch stamp already stores a per-reference clock in
>    `SoftReference.timestamp`, the field step 3 names. When the processor
>    can read that field at discovery, the stamp becomes HotSpot's
>    `timestamp` and the `soft_ref_lru_index` / `soft_ref_addr_index` tables
>    go. `../../internal/gc/common-d-proposal-soft-reference-clock-without-the-global-lock-REJECTED-20260928.md`
>    has what remains.
> 4. The cold-list proposal is retired into this one
>    (`docs/internal/gc-common-round-20260923/common-d-proposal-settled-reference-cold-list-RETIRED-20260923.md`).
>    Its named workload (a million LIVE `WeakHashMap` entries) is all
>    ACTIVE rows, so only discovery-by-marker makes the pass cost
>    proportional to unmarked referents.
> 5. The identity-stamp and referent-class-stamp tables are read in place
>    now, not cloned per pause. They disappear with the registry.
>
> **Staging, unchanged but with an entry condition.** ZGC first: its marker
> already has the `ref_skip_objs` seam. The entry condition is that
> `CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/ref/` plus
> `new WeakReference<>(new Object(), q); System.gc();` prints
> `cleared=true enqueued=true` on the staged collector. That is the w5-d
> scratch probe, which prints `false` today.

> **STATUS (gc-common wave 5, lane D, 2026-09-24): OPEN (proposal). One more
> reason for it, measured.** The registry learns of a `Reference` ONLY from
> the `Bridge` constructor natives in `native-builtins/src/reference.rs`
> (`WeakReference` / `SoftReference` / `PhantomReference` / `Reference`
> `<init>` → `discover_reference`).
>
> Under `--jdk-only` with `CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/ref/`,
> the dial that prices retiring those bridges, the real constructor
> bytecode runs instead. The registry then hears of nothing, and no weak
> reference is ever cleared or enqueued. A scratch probe on the w4 binary
> (`new WeakReference<>(new Object(), q); System.gc();`) printed:
>
> * dial armed: `cleared=false enqueued=false polled=false`;
> * unarmed, or armed for `java/lang/ref/ReferenceQueue` only:
>   `cleared=true enqueued=true polled=true`.
>
> So `java.lang.ref`'s constructor bridges cannot be retired under the
> strict mode's "real bytes are authoritative" rule while discovery is
> table-driven. A marker that discovers references from the object's class
> (step 1 below) has no such dependency. Not a new page: it is this
> proposal's precondition, stated as a failure.

> **STATUS (gc-common wave 2, lane F, 2026-09-23): OPEN (proposal), refreshed.**
> Not implementable in a common-infrastructure lane: step 2 is a change to all
> three markers. Two facts changed since filing: `clear()` is no longer
> inferred — both clear natives now settle the registry row directly
> (`retire_cleared_reference` → `ReferenceProcessor::retire_by_application`),
> so "phantom `clear()` still is not" no longer holds; and wave 2 found a
> second reason to want this design —
> `common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot.md` cannot
> be fixed in `reference.rs` because every backend resurrects finalizables
> inside the collection, BEFORE the VM processes weak references. A marker that
> discovers references gets HotSpot's order (soft, weak, final, phantom)
> between the strong closure and the finalizer closure for free.

*Status: **OPEN (proposal)**. Filed 2026-09-23, gc-common round wave 1, lane D.*

## Why

Every java.lang.ref defect lane D fixed or filed this round traces to one design
choice: the collector's markers trace a `Reference`'s referent slot as an
ordinary strong edge, so the VM compensates around each collection — it NULLS
every active referent slot before the mark (`weakref_null_referents_pre_gc`),
lets the registry decide, and WRITES BACK the survivors afterwards. That
protocol needs:

* a global registry of every `Reference` ever constructed, told about each one
  by the constructor native and about nothing else (hence `clear()` had to be
  inferred this round, and phantom `clear()` still is not);
* raw-address rows that must be relocated, pruned and identity-stamped because
  an address is not an identity across a compacting collection (identity
  stamps, referent-class stamps, relocation-target screens, shape guards — six
  independent guards on one write);
* synthetic GC roots for `Reference` objects (the "rooted forever" leak fixed
  this round was a consequence);
* a separate, different protocol for G1's concurrent cycle (slot HIDING in the
  marker, remark-time processing, resurrection of policy-kept soft referents).

HotSpot's design avoids all of it: the marker, on reaching a `Reference` whose
referent is not yet marked, does not trace slot 0 and instead pushes the
`Reference` on a per-type *discovered list* (linked through the `discovered`
field, which the real JDK layout already has, slot 3). After the closure, the
processor walks only the discovered lists; nothing is nulled that was not
decided, nothing is restored, no registry exists, and an unreachable
`Reference` is simply never discovered.

## Shape

1. A `ReferenceKind` bit in the class shape (set at class link for subclasses
   of `java/lang/ref/{Soft,Weak,Phantom,Final}Reference` and
   `jdk/internal/ref/Cleaner`), readable by all three markers from the header.
2. Each marker's object-scan: for a `Reference`-kind object, if slot 0 is
   non-null and unmarked (and, for Soft, the LRU policy says clearable this
   cycle), link it onto a discovered list instead of pushing slot 0. G1's
   existing skip-set machinery is the prototype.
3. `ReferenceProcessor` becomes a processor of discovered lists (the phase
   logic in `gc/src/reference.rs` survives almost unchanged), plus the soft
   LRU clock (HotSpot keeps it as `SoftReference.timestamp` / static `clock`
   in the Java object itself — no side table).
4. Finalizable objects: HotSpot's `Finalizer` objects are ordinary
   `FinalReference`s on the JDK's `unfinalized` list; with (1)-(3) the
   `register_finalizable` registry and the resurrection channel become the
   standard path.

Stage it per collector behind a flag: ZGC first (its marker already has the
skip-set seam and it is the default), then Generational's two markers, then
G1 (whose concurrent mark already hides slots).

## What it retires

`weakref_null_referents_pre_gc`, the post-GC restore loop and its guards,
`identity_stamps`, `referent_class_stamps`, `soft_pre_nulled`,
`pending_reference_object_addresses` rooting, `retain_shaped_weak_phantom`, and
most of `process_references_after_gc`.

## Merged from `common-d-proposal-soft-reference-clock-without-the-global-lock-REJECTED-20260928` (d8/y, 2026-09-28)

That page is retired as a duplicate of this one. What it carried over:

- **Built, default on (gc-common w6-d):** every locked soft-reference touch
  stamps the object's own `SoftReference.timestamp` with the millisecond and
  the processor's soft-index epoch (`gc/src/reference.rs::soft_touch_stamp`,
  `ReferenceProcessor::soft_touch_epoch`); a later `get()` whose stamp
  matches returns without the reference-processor lock
  (`vm/src/vm/vm_exec.rs::touch_soft_reference`). Model test
  `w6d_stamped_soft_touch_is_equivalent_to_the_locked_touch`.
- **Left, and it belongs to step 3 here:** HotSpot's clock model (a per-VM
  clock advanced once per collection, read back from `timestamp` at
  discovery). It coarsens the LRU to "since the last collection", so it
  ships behind this design's per-collector flag, and only after an 8-thread
  `get()` contention measurement shows the remaining one-lock-per-millisecond
  is worth removing.

## Merged from `common-w4d-proposal-full-reference-delivery-by-default-REJECTED-20260928` (d8/y, 2026-09-28)

That page is retired: its flip is a `--compatible` behaviour change. The one
`--jdk-only` item it still listed depends on this design: once the
`java/lang/ref` bridges are retired, `Reference.waitForReferenceProcessing()`
needs an `Intrinsic` that calls the w5-d delivery-thread hook (today a plain
registration in `native-builtins/src/lib.rs`); otherwise the real bytecode
asks `hasReferencePendingList()` and reports nothing in flight while the
delivery thread still holds a batch.
