// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Post-collection fixup for **address-keyed side-tables**.
//!
//! # The hazard
//!
//! `ObjectRef`'s `Hash`/`Eq` are address-based, so a
//! `HashMap<ObjectRef, _>` silently breaks across a moving collection.
//! Two distinct failures follow, and they need opposite remedies:
//!
//! * **A survivor that moved** leaves its entry stranded under the old
//!   address, and every later lookup misses. Harmless in isolation — the
//!   table merely stops working — so this half is a *performance* bug.
//! * **An entry whose object died** is the dangerous half. The collector
//!   hands the reclaimed address back to the allocator, so a later object
//!   can land on it and collide with the stale entry. The table then
//!   answers a lookup for the *new* object with the *old* object's value:
//!   a silent wrong answer, not a miss.
//!
//! Clearing the whole table on every collection closes both and is
//! sometimes the right trade, but it discards every live entry too.
//! [`remap_and_sweep`] keeps the live ones.
//!
//! # Why this is not an `external_roots` / `native_roots` provider
//!
//! Both registries pair a `scan` half (keep the referent alive) with a
//! `remap` half (keep the address valid), and both are driven from
//! `gc::update_all_roots` **after** its `pointer_map.is_empty()` early
//! return. That is the wrong shape for a *cache*:
//!
//! * A cache must not root its keys. An entry whose object is otherwise
//!   unreachable can never be looked up again — nothing is left to name
//!   it — so rooting it would convert the table into an immortality set
//!   and leak every object it ever saw.
//! * The sweep half has to run on a **non-moving** collection too, where
//!   `pointer_map` is empty and objects still die. A remap callback
//!   registered in either registry never runs on that path.
//!
//! So this runs before the early return, alongside
//! [`crate::memory::smuggled_longs::remap_and_sweep`], which sweeps its
//! own registry for exactly the same address-reuse reason.
//!
//! # Ordering within a cycle
//!
//! Callers pass the collector's old→new `pointer_map` and a liveness
//! predicate over **pre-remap** addresses. Both are evaluated against the
//! state the collector publishes at the end of the cycle, while the world
//! is still stopped, so no mutator can allocate onto a just-reclaimed
//! address before the sweep observes it as dead.

use crate::types::ObjectRef;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;

/// What one [`remap_and_sweep`] pass did, for tests and diagnostics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepStats {
    /// Entries whose key appeared in the pointer map and were re-keyed.
    pub moved: usize,
    /// Entries whose object survived without moving; key left alone.
    pub retained: usize,
    /// Entries whose object did not survive; dropped.
    pub dropped: usize,
}

/// Re-key `table` through the collector's relocation map and drop entries
/// whose object did not survive.
///
/// `is_live` answers "is this **pre-remap** address still a live object?"
/// — `VmHeap::is_object_address(addr).is_some()` is the usual
/// implementation. It is only consulted for keys absent from
/// `pointer_map`, since a key present there has already been proven to be
/// a relocated survivor.
///
/// Values are moved, never cloned, so `V` needs no bounds.
///
/// # Destination collisions
///
/// A key that moved is authoritative: it is inserted first, and an
/// unmoved entry is only kept if nothing already claimed its address.
/// This matters when object `A` is evacuated onto the address that dead
/// object `B` used to occupy. `is_live(B_addr)` is then true — the
/// address *is* live, but it belongs to `A` now — so a single-pass
/// implementation would keep `B`'s stale entry and let insertion order
/// decide which value survives.
///
/// gcd d10/r (2026-09-28): the collision IS reachable, and it does not need
/// `A` to have a row of its own. This doc used to say "today's collectors
/// evacuate into regions disjoint from the ones they free"; ZGC's default
/// relocation is an in-place arena SLIDE, and the Generational old-gen
/// compaction slides too, so the first dead object after a run of live ones
/// routinely has its base re-issued to the next survivor within the same
/// pause. An unmoved entry is therefore also dropped when its address is the
/// relocation DESTINATION of another key of `pointer_map` (a `(from, to)` pair
/// with `from != to`): whatever the table held there belonged to the object
/// that died. Found by one pass over the map's pairs, and only when an
/// unmoved entry exists (the candidate trick `process_references_after_gc`'s
/// referent screen uses): nothing is allocated for a table whose rows all
/// moved or died.
pub fn remap_and_sweep<V>(
    table: &mut FxHashMap<ObjectRef, V>,
    pointer_map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
) -> SweepStats {
    if table.is_empty() {
        return SweepStats::default();
    }

    let mut stats = SweepStats::default();

    // Non-moving cycle: nothing can be re-keyed and no destination can
    // collide, so the two-pass drain/re-insert below reduces to a filter.
    // `retain` does it in place — no drain, no two side vectors, no rehash of
    // every surviving entry — on exactly the cycles (every non-relocating
    // sweep) where the old path paid that for zero moves.
    if pointer_map.is_empty() {
        let before = table.len();
        table.retain(|obj, _| is_live(obj.as_ptr() as usize));
        stats.retained = table.len();
        stats.dropped = before - stats.retained;
        return stats;
    }

    let mut moved: Vec<(usize, V)> = Vec::new();
    let mut stayed: Vec<(usize, V)> = Vec::new();

    for (obj, value) in table.drain() {
        let addr = obj.as_ptr() as usize;
        match pointer_map.get(&addr) {
            Some(&new_addr) => moved.push((new_addr, value)),
            None if is_live(addr) => stayed.push((addr, value)),
            // Neither relocated nor still live: the object is gone. Drop
            // the entry so a future object allocated onto the reclaimed
            // address cannot collide with it.
            None => stats.dropped += 1,
        }
    }

    for (addr, value) in moved {
        table.insert(object_ref_at(addr), value);
        stats.moved += 1;
    }
    // gcd d10/r: the unmoved addresses some OTHER survivor was relocated
    // onto, whether or not that survivor had a row here. See "Destination
    // collisions" above.
    let claimed = relocated_onto(pointer_map, stayed.iter().map(|&(addr, _)| addr));
    for (addr, value) in stayed {
        let key = object_ref_at(addr);
        if table.contains_key(&key) || claimed.contains(&addr) {
            // A relocated survivor already claimed this address, so this
            // entry's object is dead after all — see "Destination
            // collisions" above.
            stats.dropped += 1;
            continue;
        }
        table.insert(key, value);
        stats.retained += 1;
    }

    stats
}

/// Rebuild an `ObjectRef` from an address the collector just published.
///
/// # Panics
///
/// Debug builds assert the address is non-null and 8-byte aligned, the
/// same precondition `ObjectRef::from_raw` documents.
#[inline]
fn object_ref_at(addr: usize) -> ObjectRef {
    debug_assert!(addr != 0 && addr % 8 == 0, "bad object address {addr:#x}");
    // SAFETY: `addr` comes from the collector's own pointer map or from a
    // key it just confirmed live, so it denotes a real object. The ref is
    // stored as a key and is not dereferenced here.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// The addresses among `candidates` that `pointer_map` relocated a DIFFERENT
/// object onto this pause: the `to` of a pair `(from, to)` with `from != to`
/// (an identity pair is a stationary survivor, never a collision). One pass
/// over the map, and none at all when there is no candidate or nothing moved.
/// gcd d10/r.
fn relocated_onto(
    pointer_map: &cratonvm_types::PointerMap,
    candidates: impl Iterator<Item = usize>, // addr-keyed: this pause's relocation targets
) -> FxHashSet<usize> {
    if pointer_map.is_empty() {
        return FxHashSet::default();
    }
    // addr-keyed: pause-local, dropped before this function returns.
    let candidates: FxHashSet<usize> = candidates.collect();
    if candidates.is_empty() {
        return FxHashSet::default();
    }
    pointer_map
        .iter()
        .filter(|&(&from, &to)| from != to && candidates.contains(&to))
        .map(|(_, &to)| to)
        .collect()
}

/// Can a relocation in the pause that just ran have moved a survivor onto the
/// base of an object that DIED in the same pause? Only then can an unmoved,
/// "survived in place" address be a dead object's -- see [`RelocationTargets`].
///
/// * G1: no. Evacuation copies into free regions (a survivor or old
///   allocation region above its `top`), which held no object before the
///   pause; a collection-set region is freed only AFTER its survivors left
///   it, and no survivor is copied into it in the same pause. Evacuation
///   failure self-forwards (identity pairs).
/// * Generational: only on a cycle that reclaimed OLD storage
///   (`gc_quiescence::old_gen_reclaimed_last_cycle`, read on the collecting
///   thread inside the pause, as `VmHeap::watched_pre_gc_addr_survived`
///   does): the mark-compact slide, or a promotion into a block the same
///   cycle's in-place old sweep freed. A young cycle copies into the
///   inactive semispace (empty before the pause) or into old blocks that were
///   free before it.
/// * ZGC: yes. Its default relocation is an in-place arena slide.
fn relocation_can_land_on_a_freed_base(heap: &crate::memory::vm_heap::VmHeap) -> bool {
    if heap.is_g1() {
        false
    } else if heap.is_generational() {
        cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle()
    } else {
        true
    }
}

/// The addresses a relocating pause moved a survivor ONTO, for the in-place
/// verdicts of the stop-the-world epilogue. gcd d10/r (2026-09-28),
/// `docs/internal/gc/gcd-d10r-in-place-verdicts-keep-rows-of-objects-a-slide-overwrote-FIXED-20260929.md`.
///
/// # The hole this closes
///
/// Every address-keyed side table is swept after a collection with the same
/// rule: a key in the pointer map moved (re-key it), and an unmoved key is
/// kept iff the object at it "survived in place" -- an object header parses
/// there and the collector does not call the address reclaimed. On a SLIDE
/// that rule is wrong for exactly the addresses the slide re-used: dead object
/// `D` at `a` is not moved (it is dead), survivor `S` slides from `s` onto `a`,
/// and after the pause `a` parses as a live object -- `S`. So `D`'s row
/// survives and answers for `S`: a JNI weak global ref to `D` hands out `S`, a
/// `Throwable` trace or a lock-key slot moves to a stranger, a dead
/// `ClassLoader`'s defining-loader row and a dead mirror stay cached, a dead
/// socket's fd is never queued for closing. The reference processor has had
/// the same screen for referents since gc-common w1-d
/// (`relocation_targets` in `process_references_after_gc`).
///
/// An address that is the destination of a pair `(s, a)` with `s != a` cannot
/// hold an object that stayed at `a`: two live objects do not overlap. So the
/// answer "some other survivor now lives at `a`" is exact.
///
/// # Cost
///
/// Nothing unless [`relocation_can_land_on_a_freed_base`] holds and the map is
/// non-empty. Then the destination set is built LAZILY, at the first address
/// that asks and would otherwise be kept: one `FxHashSet` of the moved pairs'
/// destinations, once per value of this type. A pause whose tables are empty,
/// or whose rows all moved or died, builds nothing.
///
/// # One per pause (gci i22/dr, P2/dr)
///
/// `gcd-d10r-proposal-one-relocation-target-set-per-pause-20260928`: the
/// stop-the-world epilogue used to hold up to THREE values of this type (the
/// verdicts of `process_references_after_gc`, of `run_collection_pause`'s JNI
/// weak-global sweep and of `memory::gc::update_all_roots`), and each built
/// its own copy of the same set -- O(relocated) three times on every
/// Generational cycle that reclaimed old storage and every ZGC cycle.
/// `run_collection_pause` now builds ONE with [`Self::for_pause`] right after
/// the collection and lends it to all three ([`InPlaceVerdict::capture_with`]).
/// The answers are the same by construction: the set is a function of the
/// pointer map alone (the same `&result.pointer_map` all three were handed),
/// and the arming bit is fixed for the pause (`old_gen_reclaimed_last_cycle`
/// is this thread's and only a collection writes it; the backend never
/// changes). Only the number of builds changes: at most one per pause.
///
/// `pub` (its constructors stay crate-private) because the public
/// `memory::gc::update_all_roots` takes it.
pub struct RelocationTargets<'m> {
    map: Option<&'m cratonvm_types::PointerMap>,
    // addr-keyed: borrowed from this pause's epilogue (see the struct doc); never outlives it.
    set: std::cell::OnceCell<FxHashSet<usize>>,
    /// gci i28/wt (`gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`):
    /// the PRE-collection addresses G1's per-object verdict
    /// (`G1Collector::pinned_young_referents_unheld`) found held by nothing in
    /// a young region this pause kept in place (a JIT / JNI pin, an evacuation
    /// failure). G1's in-place answer is region-granular there, so every
    /// in-pause verdict that borrows this value refuses them
    /// ([`InPlaceVerdict::survived`]). Empty off G1, on a pause that kept no
    /// young region, with `CRATONVM_G1_PINNED_REFERENT_VERDICT=0` and whenever
    /// G1 declined. Set once by the pause's driver ([`Self::with_g1_unheld`]).
    // addr-keyed: this pause's PRE-collection addresses, read only by its own epilogue and dropped with it.
    g1_unheld: FxHashSet<usize>,
}

impl<'m> RelocationTargets<'m> {
    /// No relocation to consult: every [`Self::claims`] answers `false`. The
    /// verdict before gcd d10/r, and the right one for a pause that moved
    /// nothing or cannot land a survivor on a freed base.
    pub(crate) fn none() -> Self {
        Self::consulting(None)
    }

    /// The relocations of the pause whose `pointer_map` this is, consulted
    /// only when the heap's collector can land a survivor on a freed base
    /// ([`relocation_can_land_on_a_freed_base`]).
    pub(crate) fn for_pause(
        heap: &crate::memory::vm_heap::VmHeap,
        pointer_map: &'m cratonvm_types::PointerMap,
    ) -> Self {
        let armed = !pointer_map.is_empty() && relocation_can_land_on_a_freed_base(heap);
        Self::consulting(armed.then_some(pointer_map))
    }

    fn consulting(map: Option<&'m cratonvm_types::PointerMap>) -> Self {
        Self {
            map,
            set: std::cell::OnceCell::new(),
            g1_unheld: FxHashSet::default(),
        }
    }

    /// gci i28/wt: this pause's G1 kept-young-region verdict (see the field
    /// `g1_unheld`), handed in by `run_collection_pause` before the value is
    /// lent to any verdict. An empty set changes nothing.
    pub(crate) fn with_g1_unheld(
        mut self,
        // addr-keyed: this pause's PRE-collection addresses (see the field).
        unheld: FxHashSet<usize>,
    ) -> Self {
        self.g1_unheld = unheld;
        self
    }

    /// gci i28/wt: did G1's per-object verdict find the object at the
    /// PRE-collection address `addr` held by nothing in a young region this
    /// pause kept in place? `false` whenever no verdict was taken.
    #[inline]
    pub(crate) fn g1_unheld(&self, addr: usize) -> bool {
        !self.g1_unheld.is_empty() && self.g1_unheld.contains(&addr)
    }

    /// gci i28/wt: the whole set, for the reference round
    /// (`process_references_after_gc`'s `is_marked`), which used to compute
    /// it itself for its own rows only.
    // addr-keyed: borrowed from this pause's epilogue (see the field).
    pub(crate) fn g1_unheld_set(&self) -> &FxHashSet<usize> {
        &self.g1_unheld
    }

    /// Did this pause relocate some OTHER object onto `addr`? Ask it only for
    /// an address that is not itself a key of the map (a key moved away, and
    /// every sweep answers it through the map first).
    pub(crate) fn claims(&self, addr: usize) -> bool {
        let Some(map) = self.map else {
            return false;
        };
        self.set
            .get_or_init(|| {
                // addr-keyed: this pause's relocation targets (see `set`).
                let mut set: FxHashSet<usize> =
                    FxHashSet::with_capacity_and_hasher(map.len(), Default::default());
                for (&from, &to) in map.iter() {
                    if from != to && to != 0 {
                        set.insert(to);
                    }
                }
                set
            })
            .contains(&addr)
    }

    /// Was this set built over `pointer_map` (or is it unarmed, which answers
    /// the same for every map)? The debug check of the borrowers of the
    /// pause's one set (gci i22/dr): a set lent with ANOTHER pause's map would
    /// screen the wrong destinations.
    pub(crate) fn lent_for(&self, pointer_map: &cratonvm_types::PointerMap) -> bool {
        self.map.is_none_or(|m| std::ptr::eq(m, pointer_map))
    }

    /// Has the destination set been built? `false` for a pause in which no
    /// address needed it.
    #[cfg(test)]
    pub(crate) fn built(&self) -> bool {
        self.set.get().is_some()
    }
}

/// The destination set an [`InPlaceVerdict`] consults: its own (a verdict
/// from [`InPlaceVerdict::capture`] / [`InPlaceVerdict::capture_after`]) or
/// the pause's one shared set (gci i22/dr, [`InPlaceVerdict::capture_with`]).
pub(crate) enum VerdictTargets<'h> {
    /// Built (lazily) by this verdict alone.
    Own(RelocationTargets<'h>),
    /// The pause's set, built at most once for every verdict that borrows it.
    Shared(&'h RelocationTargets<'h>),
}

impl<'h> VerdictTargets<'h> {
    #[inline]
    fn get(&self) -> &RelocationTargets<'h> {
        match self {
            Self::Own(t) => t,
            Self::Shared(t) => t,
        }
    }
}

/// Did the object at pre-remap address `addr` survive this collection IN
/// PLACE? The liveness verdict every address-keyed sweep driven from the VM
/// should use for a key that is NOT in the pointer map.
///
/// # Why not `is_object_address` alone
///
/// "An object header parses at this address" is not "the object this row was
/// made for survived", and the difference is not hypothetical
/// (gc-common w2-g, 2026-09-23):
///
/// * `OldGen::free` does not zero (round-5 #14: "the redundant per-free zero
///   pass has been dropped"), so a block the Generational non-moving old-gen
///   sweep or the concurrent old-gen sweep just freed still carries its dead
///   object's header. `GenerationalHeap::is_object_address` accepts it through
///   the extent-deduction path once the object-start bit is gone.
/// * The Generational non-moving YOUNG sweep zeroes what it reclaims, and an
///   all-zero header is a well-formed `java.lang.Object` with no fields.
///
/// Either way a DEAD row was kept, and it then answered for whatever object
/// the allocator placed on that address next: a Throwable reporting another
/// exception's stack trace, a primitive `long` that collides with the reused
/// address treated as a minted handle.
///
/// # The verdict
///
/// Kept iff the header parses AND the collector does not say the address is
/// reclaimed. `is_addr_live` is the cheap positive answer (G1: a live region;
/// ZGC: the exact registry; Generational: an allocated old-gen span or a
/// non-zero young survivor word). When it says no, `reclaimed_hole_at`
/// decides: it is `Some` only for free-list holes, the unallocated tail of a
/// semispace and the inactive semispace, and a live object is never in any of
/// those -- so this can only DROP rows the collector itself says are dead,
/// never a survivor. (`is_addr_live` alone would drop a live all-zero-header
/// `new Object()` kept in place by the non-moving young sweep; the
/// free-list test does not.)
///
/// Only meaningful between the end of a collection and the first allocation
/// that could reuse a reclaimed address: inside the stop-the-world epilogue,
/// or -- for a reclamation outside a pause -- immediately after it, before the
/// freed space can be handed out again.
pub(crate) fn survived_in_place(heap: &crate::memory::vm_heap::VmHeap, addr: usize) -> bool {
    if heap.is_object_address(addr).is_none() {
        return false;
    }
    heap.is_addr_live(addr) || heap.reclaimed_hole_at(addr).is_none()
}

/// [`survived_in_place`] for many addresses judged in ONE stop-the-world
/// window: the same verdict, but the Generational young free list is copied and
/// sorted once at [`InPlaceVerdict::capture`] rather than once per address.
///
/// gc-common w16-x
/// (`docs/internal/gc-common-round-20260923/applied/handoff-w16x-generational-survived-in-place-is-quadratic.md`):
/// per address, a dead young row cost a full `free_blocks_sorted()`, and the
/// sweep that makes D rows dead also makes about D free blocks. So the
/// epilogue's sweeps were quadratic in the garbage. After w15-b put every
/// `Properties` key into the lock-key registry, one `System.gc()` took
/// seconds.
///
/// Only for the stop-the-world epilogue. The out-of-pause sweeps run beside
/// allocating mutators and must keep asking [`survived_in_place`].
///
/// The window opens at [`Self::capture`] and closes when the value is
/// dropped; nothing may allocate on `heap` in between. The probe itself is
/// taken lazily, at the first address that needs the reclaimed-hole arm
/// (gc-common w18-a): a pause whose rows are all live, or whose tables are
/// empty, never copies the free list at all, and one that needs it copies it
/// exactly once. Taking it later inside the same allocation-free window is
/// the same answer as taking it at `capture`.
///
/// gcd d10/r: a verdict taken with [`Self::capture_after`] also refuses an
/// address the pause relocated another survivor onto ([`RelocationTargets`]);
/// [`survived_in_place`] cannot, having no pointer map, which is right for
/// the out-of-pause sweeps it serves (nothing moves there).
///
/// Users (gc-common w18-a extended the w16-x set): the weak side-table sweeps
/// of `process_references_after_gc`, and `gc::update_all_roots`'s own sweeps
/// (its inherited-`ThreadLocal` buckets directly; its smuggled-`long` and
/// Throwable back-trace sweeps and `run_collection_pause`'s JNI weak-global
/// sweep through
/// `docs/internal/gc-common-round-20260923/applied/handoff-w18a-in-pause-sweeps-take-the-in-place-verdict.md`).
pub(crate) struct InPlaceVerdict<'h> {
    heap: &'h crate::memory::vm_heap::VmHeap,
    probe: std::cell::OnceCell<crate::memory::vm_heap::ReclaimedHoleProbe>,
    /// gcd d10/r: the pause's relocations, so an address a survivor was
    /// moved ONTO is not mistaken for the dead object that used to be there.
    /// [`RelocationTargets::none`] for a verdict captured without a map;
    /// the pause's one shared set for [`Self::capture_with`] (gci i22/dr).
    targets: VerdictTargets<'h>,
}

impl<'h> InPlaceVerdict<'h> {
    /// A verdict with no knowledge of the pause's relocations: exactly
    /// [`survived_in_place`]. For a sweep that has no pointer map in hand; a
    /// stop-the-world epilogue that has one should use
    /// [`Self::capture_after`].
    pub(crate) fn capture(heap: &'h crate::memory::vm_heap::VmHeap) -> Self {
        Self {
            heap,
            probe: std::cell::OnceCell::new(),
            targets: VerdictTargets::Own(RelocationTargets::none()),
        }
    }

    /// The verdict for the epilogue of the pause whose relocation map is
    /// `pointer_map`: [`survived_in_place`], and additionally `false` for an
    /// address this pause relocated another object onto
    /// ([`RelocationTargets`], gcd d10/r). Identical to [`Self::capture`] on
    /// G1, on a non-moving pause and on a Generational cycle that reclaimed no
    /// old storage.
    pub(crate) fn capture_after(
        heap: &'h crate::memory::vm_heap::VmHeap,
        pointer_map: &'h cratonvm_types::PointerMap,
    ) -> Self {
        Self {
            heap,
            probe: std::cell::OnceCell::new(),
            targets: VerdictTargets::Own(RelocationTargets::for_pause(heap, pointer_map)),
        }
    }

    /// [`Self::capture_after`] over the pause's ONE destination set (gci
    /// i22/dr, P2/dr): `targets` is the [`RelocationTargets::for_pause`] the
    /// epilogue's driver (`run_collection_pause`) built once for this pause's
    /// pointer map, lent to every in-pause verdict so the set is built at
    /// most once per pause instead of once per verdict. Same answers as
    /// `capture_after(heap, <that map>)`: the set depends on the map and the
    /// pause's arming bit only (see "One per pause" on [`RelocationTargets`]).
    /// The reclaimed-hole probe stays per verdict, as before.
    pub(crate) fn capture_with(
        heap: &'h crate::memory::vm_heap::VmHeap,
        targets: &'h RelocationTargets<'h>,
    ) -> Self {
        Self {
            heap,
            probe: std::cell::OnceCell::new(),
            targets: VerdictTargets::Shared(targets),
        }
    }

    /// [`survived_in_place`]`(heap, addr)`, and not an address the pause
    /// relocated another object onto (only for a verdict from
    /// [`Self::capture_after`] or [`Self::capture_with`]).
    ///
    /// gci i28/wt: and, on G1, not an address the pause's per-object verdict
    /// found held by nothing in a young region the pause kept in place
    /// ([`RelocationTargets::with_g1_unheld`]; only for a verdict from
    /// [`Self::capture_with`] over the pause's set). The region-granular G1 arm
    /// of `is_addr_live` answers "live" for every object below such a
    /// region's cursor, so a JNI weak global or a side-table row whose owner
    /// died there outlived it until the region was evacuated.
    pub(crate) fn survived(&self, addr: usize) -> bool {
        if self.targets.get().g1_unheld(addr) {
            return false;
        }
        if self.heap.is_object_address(addr).is_none() {
            return false;
        }
        let in_place = self.heap.is_addr_live(addr) || {
            let probe = self.probe.get_or_init(|| self.heap.reclaimed_hole_probe());
            !self.heap.is_reclaimed_hole(probe, addr)
        };
        in_place && !self.targets.get().claims(addr)
    }

    /// Did the pause relocate some other object onto `addr`? The screen
    /// [`Self::survived`] applies, for a caller whose own liveness test is not
    /// `survived_in_place` (the class-metadata reconcile of
    /// `process_references_after_gc`). Shares this verdict's destination set,
    /// so one epilogue builds it at most once. Ask it only for an address that
    /// is not a key of the pointer map.
    pub(crate) fn claimed_by_relocation(&self, addr: usize) -> bool {
        self.targets.get().claims(addr)
    }

    /// Has the reclaimed-hole probe been taken? At most once per value, by
    /// construction; `false` for a window in which no address needed it.
    #[cfg(test)]
    pub(crate) fn probe_taken(&self) -> bool {
        self.probe.get().is_some()
    }
}

/// Sweep (never remap) every VM-side address-keyed side table that is safe to
/// sweep OUTSIDE a stop-the-world pause: the minted smuggled-`long` registry,
/// the Throwable backtrace table, the pending inherited-`ThreadLocal`
/// buckets of never-started threads, and the JNI weak global refs (gc-common
/// w8-c: cleared, never removed; the handle outlives its referent). Returns
/// nothing; each table keeps its own statistics.
///
/// `update_all_roots` sweeps these at the end of every STW collection. This is
/// the entry point for a reclamation that is NOT followed by
/// `update_all_roots` -- the Generational concurrent old-gen sweep
/// (`ConcurrentMarker::concurrent_sweep`) and G1's remark cleanup -- which
/// otherwise leave a dead row in place until the next STW collection, by
/// which time the freed address may already hold a new object that then
/// inherits the row. See
/// `docs/internal/gc-common-round-20260923/common-b-address-keyed-sweeps-miss-concurrent-reclamation-FIXED-20260923.md`
/// and its handoff for the two call sites.
///
/// Deliberately excludes the GPU input-residency cache
/// (`offload::input_cache::remap_and_sweep`), whose drain-then-rekey
/// protocol documents that mutators are stopped.
///
/// Callers (gc-common w3-g, applying
/// `docs/internal/gc-common-round-20260923/applied/handoff-w2g-sweep-address-keyed-tables-after-concurrent-reclamation.md`):
/// `interpreter::gc_and_alloc::maybe_concurrent_gc_at` after its old-gen sweep
/// (and between sweep slices when a pause is about to be joined), and
/// `g1_final_remark_cleanup` after a completed remark + cleanup.
///
/// Must not be called while holding the old-gen lock: `survived_in_place`
/// consults the old-gen free list under that same, non-reentrant lock.
pub(crate) fn sweep_address_keyed_tables(shared: &crate::vm::SharedVm) {
    let nothing_moved = cratonvm_types::PointerMap::default();
    crate::memory::smuggled_longs::remap_and_sweep(&nothing_moved, &shared.mem.heap);
    shared.sweep_throwable_stack_traces();
    // Pending inherited-ThreadLocal buckets of never-started Threads (w5-b).
    // Sweep-only with an empty map; takes the pending-table lock, then the
    // JNI global-ref table lock, never both at once.
    // Outside a pause: the per-address verdict, never an `InPlaceVerdict`.
    crate::memory::gc::sweep_inherited_thread_local_buckets(shared, &nothing_moved, &|addr| {
        survived_in_place(&shared.mem.heap, addr)
    });
    // JNI weak global refs (gc-common w8-c): a weak global whose referent this
    // reclamation freed reads as NULL from here on, before the freed space can
    // be handed out again. Same `survived_in_place` verdict; the table lock is
    // not held while the heap is asked (see `jni::sweep_weak_global_refs`).
    crate::native::jni::sweep_weak_global_refs_in_place(shared);
    // gci i22/g1rk: the native-builtins / native-io tables, in the same order,
    // with the same `survived_in_place` verdict (one body shared with
    // `sweep_address_keyed_tables_with`).
    sweep_native_address_keyed_tables(shared, &nothing_moved, &|addr| {
        survived_in_place(&shared.mem.heap, addr)
    });
}

/// gci i22/g1rk (`gch-h4g1-proposal-g1-remark-sweeps-every-address-keyed-table-mark-exact-20261009`):
/// [`sweep_address_keyed_tables`] with the caller's verdict `survived` for
/// every table that takes one -- the smuggled-`long` registry, the Throwable
/// back-trace table, the pending inherited-`ThreadLocal` buckets and every
/// native-builtins / native-io table -- in the same order. Nothing moved.
///
/// The JNI weak globals are NOT swept here: their mark-exact verdict needs the
/// remark's keep list (`jni::clear_weak_global_refs_unmarked(shared, is_live,
/// &keep)`, inside the remark callback), and the region-granular sweep after
/// it stays with [`sweep_address_keyed_tables`].
///
/// Caller: G1's remark doors (`g1_final_remark_cleanup`,
/// `g1_stw_full_mark_cycle`) through `g1_remark_sweep_address_keyed_mark_exact`,
/// right after a completed `cleanup` and before [`sweep_address_keyed_tables`],
/// with `G1Collector::trusted_mark_liveness`'s verdict (a dead key in a region
/// the cleanup kept is dropped now, not at the mixed pause that evacuates the
/// region). Opt-in `CRATONVM_G1_REMARK_ADDR_KEYED_MARK_EXACT`.
///
/// Locks: as [`sweep_address_keyed_tables`]; each table evaluates `survived`
/// with its own lock released (or as a leaf under it), so `survived` may take
/// the heap's region lock.
pub(crate) fn sweep_address_keyed_tables_with(
    shared: &crate::vm::SharedVm,
    survived: &dyn Fn(usize) -> bool,
) {
    let nothing_moved = cratonvm_types::PointerMap::default();
    crate::memory::smuggled_longs::remap_and_sweep_with(
        &nothing_moved,
        &shared.mem.heap,
        survived,
    );
    shared.sweep_throwable_stack_traces_with(survived);
    crate::memory::gc::sweep_inherited_thread_local_buckets(shared, &nothing_moved, survived);
    sweep_native_address_keyed_tables(shared, &nothing_moved, survived);
}

/// The native-builtins / native-io half of [`sweep_address_keyed_tables`]
/// and [`sweep_address_keyed_tables_with`] (gci i22/g1rk: split out
/// unchanged, `survived` in place of each call's `survived_in_place`
/// closure). `nothing_moved` is the empty map.
fn sweep_native_address_keyed_tables(
    shared: &crate::vm::SharedVm,
    nothing_moved: &cratonvm_types::PointerMap,
    survived: &dyn Fn(usize) -> bool,
) {
    // Weak Locale subtag rows (gc-common w11-d): the same verdict. The
    // tables' locks are leaves; the predicate takes the old-gen lock inside
    // them, the nesting this function already documents for its own tables.
    cratonvm_native_builtins::gc_sweep_locale_rows(shared.vm_identity, survived);
    // TLS owner rows (gc-common w12-a): the same verdict. The predicate is
    // evaluated with none of the TLS tables' locks held.
    cratonvm_native_builtins::gc_sweep_tls_rows(shared.vm_identity, survived);
    // JUL / JULI logging rows keyed on a Logger, Handler or context class
    // loader (gc-common w14-d): the same verdict, evaluated with none of the
    // logging tables' locks held.
    cratonvm_native_builtins::logmanager::gc_sweep_logging_rows(shared.vm_identity, survived);
    // Lock-key registry slots (gc-common w14-c): the same verdict, after the
    // TLS rows. The predicate is evaluated with the registry lock released.
    if cratonvm_native_builtins::gc_sweep_lock_keys(shared.vm_identity, survived) > 0 {
        // A dead user loader's defined-`Package` rows go with its freed weak
        // key (interpreter round i5 wave 105: the memo is this VM's, out of
        // the VM-less sweep's reach).
        cratonvm_native_builtins::lang_class::forget_dead_loader_defined_packages(
            &shared.loader_identity,
        );
    }
    // native-io side tables (gc-common w14-e): the same verdict; nothing
    // moved. The table lock is not held while the heap is asked.
    cratonvm_native_io::gc_sweep_io_side_tables(shared.vm_identity, nothing_moved, survived);
    // `com.sun.net.httpserver` link rows (gc-common w15-f): the same
    // verdict; nothing moved. The table lock is not held while the heap is
    // asked.
    cratonvm_native_builtins::phases_late::net_channels::gc_sweep_http_link_rows(
        shared.vm_identity,
        nothing_moved,
        survived,
    );
    // `java.net` socket rows, `SSLSessionContext` carriers, `SSLServerSocket`
    // option delegates and `HttpClient` shutdown rows (gc-common w16-a): the
    // same verdict; nothing moved. The table locks are not held while the heap
    // is asked.
    cratonvm_native_builtins::net_phase_e::gc_sweep_net_socket_rows(
        shared.vm_identity,
        nothing_moved,
        survived,
    );
    // `java.util.zip` side tables and the `ZipFile`/`JarFile` tables
    // (gc-common w17-c): the same verdict; nothing moved. The table locks are
    // not held while the heap is asked.
    cratonvm_native_builtins::phases_late::zip_streams::gc_sweep_zip_rows(
        shared.vm_identity,
        nothing_moved,
        survived,
    );
    // Synthetic `SubmissionPublisher` closed-exception rows (gc-common
    // w18-g): the same verdict; nothing moved. The table lock is not held
    // while the heap is asked.
    cratonvm_native_builtins::phases_late::concurrent::gc_sweep_submission_publisher_rows(
        shared.vm_identity,
        nothing_moved,
        survived,
    );
    // Undertow `HeaderMap` / `Undertow$Builder` rows (gc-common w20-g): the
    // same verdict; nothing moved. The table locks are not held while the heap
    // is asked.
    cratonvm_native_builtins::wildfly_undertow::gc_sweep_undertow_rows(
        shared.vm_identity,
        nothing_moved,
        survived,
    );
}

/// `addr` lies in one of `spans` (`(start, len)`, sorted by `start`,
/// non-overlapping).
pub(crate) fn in_spans(addr: usize, spans: &[(usize, usize)]) -> bool {
    let i = spans.partition_point(|&(start, _)| start <= addr);
    i > 0 && addr - spans[i - 1].0 < spans[i - 1].1
}

/// Drop every address-keyed row keyed inside a span a reclamation just freed
/// (`spans` sorted by start, non-overlapping; see [`in_spans`]).
///
/// Unlike [`sweep_address_keyed_tables`] this never asks the heap -- it is a
/// pure range test -- so it cannot be fooled by an object allocated onto a
/// freed span afterwards, and it may run UNDER the old-gen guard: the table
/// locks it takes (the smuggled-`long` registry mutex, the Throwable shard
/// locks, and since gc-common w8-c the JNI global-ref table mutex, for the
/// weak globals) are leaves below it. The reverse nesting (table lock, then the
/// old-gen lock inside `survived_in_place`) exists only in the table SWEEPS,
/// which run inside a stop-the-world pause or on the concurrent driver's own
/// thread outside its guard, never beside a sweep slice.
///
/// gc-common w5-g, closing
/// `common-w3g-concurrent-sweep-direct-old-alloc-window`: the Generational
/// concurrent sweep calls it with each slice's freed spans before the slice
/// drops the old-gen guard, i.e. before a direct old-gen allocation (a
/// humongous array) can land on one and make its dead row look live.
pub(crate) fn drop_address_keyed_rows_in(shared: &crate::vm::SharedVm, spans: &[(usize, usize)]) {
    if spans.is_empty() {
        return;
    }
    crate::memory::smuggled_longs::drop_in_spans(&shared.mem.heap, spans);
    for shard in shared.threads.throwable_stacks.iter() {
        let mut traces = shard.write();
        if !traces.is_empty() {
            traces.retain_addresses(|addr| !in_spans(addr, spans));
        }
    }
    // JNI weak global refs (gc-common w8-c): the same pure range test, under
    // the same guard. The JNI global-ref table lock is a leaf here too: no
    // path holds it while it asks the heap.
    crate::native::jni::clear_weak_global_refs_in_spans(shared, spans);
    // gch h1/ak: the pending inherited-`ThreadLocal` buckets of never-started
    // Threads (gc-common w5-b), keyed by the child `Thread`'s address. They
    // were swept only by `sweep_address_keyed_tables` at the END of the
    // concurrent sweep, with the heap-asking verdict; a direct old-gen
    // allocation landing on a freed span in between made a dead child's
    // bucket look live, and a `Thread` later placed at that address would
    // start with the dead one's inherited values. A pure range test here, as
    // for every row above. Locks: the native pending table, then (released
    // first) the JNI global-ref table to drop the dead buckets' handles --
    // both leaves under the old-gen guard; the reverse nesting (pending table,
    // then the old-gen lock inside `survived_in_place`) exists only in the
    // table SWEEPS, as the doc above says for the other tables.
    crate::memory::gc::sweep_inherited_thread_local_buckets(
        shared,
        &cratonvm_types::PointerMap::default(),
        &|addr| !in_spans(addr, spans),
    );
    // Weak Locale subtag rows (gc-common w11-d): a pure range test, so the
    // table locks stay leaves under the old-gen guard.
    cratonvm_native_builtins::gc_sweep_locale_rows(shared.vm_identity, &|addr| {
        !in_spans(addr, spans)
    });
    // TLS owner rows (gc-common w12-a): a pure range test; the TLS table
    // locks are `Scratch` leaves below the old-gen guard. gcg g2/e
    // (`common-w26c-proposal-sub-linear-freed-span-drops` item 2): the span
    // entry point judges the ranges under the owner lock in one pass, instead
    // of copying every owner address of the VM out per slice. Opt-in
    // (`CRATONVM_GC_TLS_SPAN_DROP`) until its perf row is judged; the rows
    // dropped are the same either way. gci i27/kd: read once per VM
    // (`SharedVm::gc_tls_span_drop`), not once per slice.
    if shared.gc_tls_span_drop {
        cratonvm_native_builtins::t27_tls::gc_drop_tls_rows_in_spans(shared.vm_identity, spans);
    } else {
        cratonvm_native_builtins::gc_sweep_tls_rows(shared.vm_identity, &|addr| {
            !in_spans(addr, spans)
        });
    }
    // JUL / JULI logging rows (gc-common w14-d): a pure range test; the
    // logging table locks are leaves below the old-gen guard.
    cratonvm_native_builtins::logmanager::gc_sweep_logging_rows(shared.vm_identity, &|addr| {
        !in_spans(addr, spans)
    });
    // Lock-key registry slots (gc-common w14-c): a pure range test; the
    // registry lock is a leaf below the old-gen guard. Since gc-common w26-c
    // the span entry point tests the ranges under the registry lock in one
    // pass, instead of copying every slot of the VM out per slice
    // (`common-w25a-lock-key-sweep-walks-the-whole-registry-per-concurrent-sweep-slice`).
    if cratonvm_native_builtins::gc_drop_lock_keys_in_spans(shared.vm_identity, spans) > 0 {
        // As after `gc_sweep_lock_keys` above (interpreter round i5 wave 105).
        cratonvm_native_builtins::lang_class::forget_dead_loader_defined_packages(
            &shared.loader_identity,
        );
    }
    // native-io side tables (gc-common w14-e): a pure range test, so the
    // table lock (a leaf) is safe under the old-gen guard.
    cratonvm_native_io::gc_sweep_io_side_tables(
        shared.vm_identity,
        &cratonvm_types::PointerMap::default(),
        &|addr| !in_spans(addr, spans),
    );
    // `com.sun.net.httpserver` link rows (gc-common w15-f): a pure range
    // test, so the table lock (a leaf) is safe under the old-gen guard.
    cratonvm_native_builtins::phases_late::net_channels::gc_sweep_http_link_rows(
        shared.vm_identity,
        &cratonvm_types::PointerMap::default(),
        &|addr| !in_spans(addr, spans),
    );
    // `java.net` socket rows and friends (gc-common w16-a): a pure range
    // test, so the table locks (leaves) are safe under the old-gen guard.
    cratonvm_native_builtins::net_phase_e::gc_sweep_net_socket_rows(
        shared.vm_identity,
        &cratonvm_types::PointerMap::default(),
        &|addr| !in_spans(addr, spans),
    );
    // `java.util.zip` side tables and the `ZipFile`/`JarFile` tables
    // (gc-common w17-c): a pure range test, so the table locks (leaves, and
    // one `Scratch` ordered lock) are safe under the old-gen guard.
    cratonvm_native_builtins::phases_late::zip_streams::gc_sweep_zip_rows(
        shared.vm_identity,
        &cratonvm_types::PointerMap::default(),
        &|addr| !in_spans(addr, spans),
    );
    // Synthetic `SubmissionPublisher` closed-exception rows (gc-common
    // w18-g): a pure range test, so the one leaf table lock is safe under the
    // old-gen guard.
    cratonvm_native_builtins::phases_late::concurrent::gc_sweep_submission_publisher_rows(
        shared.vm_identity,
        &cratonvm_types::PointerMap::default(),
        &|addr| !in_spans(addr, spans),
    );
    // Undertow `HeaderMap` / `Undertow$Builder` rows (gc-common w20-g): a pure
    // range test, so the leaf table locks are safe under the old-gen guard.
    cratonvm_native_builtins::wildfly_undertow::gc_sweep_undertow_rows(
        shared.vm_identity,
        &cratonvm_types::PointerMap::default(),
        &|addr| !in_spans(addr, spans),
    );
}

/// The workspace census of address-keyed `ObjectRef` tables.
///
/// `docs/threading/objectref-concurrency-contract.md` §7.3 records the gap this
/// closes. `ObjectRef`'s `Hash`/`Eq` are addresses, so every
/// `HashMap<ObjectRef, _>` in the tree needs a GC disposition — and "**Nothing
/// enumerates the tables that need it.**" A table added without one breaks
/// nothing at review time; it just starts answering a lookup for a new object
/// with a dead object's value once the allocator reuses the address.
///
/// This is that enumeration, and
/// `the_address_keyed_table_census_is_complete` enforces it.
#[cfg(test)]
mod census {
    /// One audited source file: its workspace-relative path, how many
    /// address-keyed declarations it contains, and the GC disposition each of
    /// its tables states in its own source.
    pub(super) struct AuditedFile {
        pub path: &'static str,
        /// Lines declaring `HashMap<ObjectRef` / `HashSet<ObjectRef`, comments
        /// excluded. A table is usually two (the accessor signature and the
        /// `static` behind it), so this is a count of declarations, not of
        /// tables. It is here to make ADDING a table to an
        /// already-audited file fail too, not just adding a new file.
        pub declarations: usize,
        pub disposition: &'static str,
    }

    pub(super) const AUDITED: &[AuditedFile] = &[
        AuditedFile {
            path: "gc/src/heap.rs",
            declarations: 1,
            disposition: "gpu_pinned_refs: PINNED — membership is exactly what \
                          forbids relocation, so no key in it can go stale.",
        },
        AuditedFile {
            path: "vm/src/vm/realms/class_realm.rs",
            declarations: 1,
            disposition: "class_mirrors_reverse: REMAPPED + SWEPT — the moved \
                          rows are re-keyed by update_all_roots step 14 \
                          (gc::rekey_reverse_mirrors, full rebuild on drift) \
                          and dead mirrors' rows are pruned with the forward \
                          map by gc::reconcile_class_mirrors, which runs on \
                          non-moving cycles too (gc::prune_reverse_mirrors).",
        },
        AuditedFile {
            path: "vm/src/memory/gc.rs",
            declarations: 3,
            disposition: "NO TABLE OF ITS OWN (gc-common w1-b, 2026-09-23). \
                          One declaration is rekey_reverse_mirrors' `&mut` \
                          parameter: it borrows class_realm.rs's \
                          class_mirrors_reverse, whose REMAPPED + SWEPT \
                          disposition above covers it. The other two are \
                          local FxHashMaps inside that helper's unit tests, \
                          built and dropped within one test body.",
        },
        AuditedFile {
            path: "vm/src/runtime/offload.rs",
            declarations: 4,
            disposition: "input_cache: REMAPPED + SWEPT — \
                          offload::input_cache::remap_and_sweep, which routes \
                          through this module and is driven from \
                          vm/src/memory/gc.rs. Still ONE table: the fourth \
                          declaration (2026-09-05) is drain_locked's `&mut` \
                          parameter: the drain takes the caller's guard, so \
                          that the DIRTY read-clear and the eviction it \
                          authorises happen under a single hold of the cache \
                          mutex. It borrows \
                          the same map rather than introducing another, so \
                          the remap+sweep disposition above covers it \
                          unchanged.",
        },
        AuditedFile {
            path: "native-builtins/src/net_phase_e.rs",
            declarations: 7,
            disposition: "inet_addr_side_table, ds_side_table and \
                          ds_peer_table: WEAK — REMAPPED + SWEPT per VM \
                          (gc-common w17-a). Keys are not roots \
                          (gc_scan_inet_addr_roots / gc_scan_ds_roots push \
                          nothing); gc_sweep_net_socket_rows \
                          (sweep_object_keyed_rows) drops a dead key's rows at \
                          all three weak-row sites, a dropped-open socket's UDP \
                          fd is queued for closing, and gc_update_inet_addr_refs \
                          / gc_update_ds_refs re-key the survivors. Until w17-a \
                          all three were SCANNED + REMAPPED. (The previous \
                          entry named ds_side_table as covered by the \
                          inet_addr pair; it was not — that pair walks only \
                          inet_addr_side_table, and ds_side_table had no GC \
                          disposition at all until ds_peer_table's arrival \
                          made the count wrong and surfaced it.) The re10 \
                          handler roots live in this file too but are counted \
                          under their own pair. Seven declarations, six tables: the 
                          seventh is gc_update_ds_refs own rekey helper, which 
                          takes a HashMap<ObjectRef, V> parameter and so is 
                          counted by a matcher that reads declarations, not 
                          tables.",
        },
        AuditedFile {
            path: "native-builtins/src/locale_bootstrap.rs",
            declarations: 3,
            disposition: "synthetic_locale_data: WEAK — SWEPT + REMAPPED per VM \
                          (gc-common w11-d). Rows carry their VM (w10-a); keys \
                          are not roots (the cached defaults are rooted through \
                          CACHED_LOCALE), gc_sweep_locale_rows(vm) drops dead \
                          Locales' rows after every STW collection \
                          (process_references_after_gc) and gc_update_locale_refs \
                          re-keys the survivors. The third declaration is \
                          rekey_vm_rows, the per-VM rekey helper both locale \
                          tables use: a parameter, not a table.",
        },
        AuditedFile {
            path: "native-builtins/src/lib.rs",
            declarations: 2,
            disposition: "locale_data: WEAK — SWEPT + REMAPPED per VM, with the \
                          bootstrap table (gc-common w11-d: \
                          gc_sweep_locale_rows / gc_update_locale_refs; the keys \
                          are no longer roots). Rows carry their VM (w10-a).",
        },
        // gc-common w10-d: `wildfly_core.rs` left the census. `EQE_PENDING` is
        // now a per-VM FIFO of Runnables (no address key), scanned and
        // remapped by the `wildfly-side-tables` root row.
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk every workspace source tree and require each address-keyed
    /// `ObjectRef` declaration to be accounted for in [`census::AUDITED`].
    ///
    /// Scans DIRECTORIES rather than a list of file names on purpose: a module
    /// split renames files, and a gate keyed on file names goes quietly
    /// fail-open at exactly the moment the code it guards was reorganised. For
    /// the same reason it fails loudly — rather than passing vacuously — when
    /// it cannot find the workspace root, when the walk turns up implausibly
    /// few files, or when the pattern matches nothing at all.
    #[test]
    fn the_address_keyed_table_census_is_complete() {
        fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    collect_rs(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the vm crate directory must have a parent")
            .to_path_buf();
        assert!(
            workspace.join("Cargo.toml").is_file(),
            "workspace root not found at {} — failing rather than scanning nothing",
            workspace.display()
        );

        let mut files = Vec::new();
        for entry in std::fs::read_dir(&workspace).expect("workspace root is readable") {
            let src = entry.expect("readable directory entry").path().join("src");
            if src.is_dir() {
                collect_rs(&src, &mut files);
            }
        }
        assert!(
            files.len() > 100,
            "only {} source files found under {} — the directory walk is broken",
            files.len(),
            workspace.display()
        );

        // Declarations, not mentions: the pattern inside a doc comment or a
        // prose note is not a table. Everything from the first `//` is prose.
        let declarations_in = |text: &str| {
            text.lines()
                .filter(|line| {
                    let code = line.split("//").next().unwrap_or("");
                    code.contains("HashMap<ObjectRef") || code.contains("HashSet<ObjectRef")
                })
                .count()
        };

        let mut findings: Vec<String> = Vec::new();
        let mut total_declarations = 0usize;
        let mut seen: Vec<&str> = Vec::new();
        for file in &files {
            let rel = file
                .strip_prefix(&workspace)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/");
            // This module is the fixup machinery and `value.rs` is where the
            // contract itself is argued; neither owns a table.
            if rel.ends_with("vm/src/memory/addr_keyed.rs") || rel.ends_with("types/src/value.rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            let count = declarations_in(&text);
            if count == 0 {
                continue;
            }
            total_declarations += count;
            match super::census::AUDITED
                .iter()
                .find(|a| rel.ends_with(a.path))
            {
                None => findings.push(format!(
                    "{rel}: {count} address-keyed declaration(s), NOT in the census"
                )),
                Some(audited) => {
                    seen.push(audited.path);
                    if audited.declarations != count {
                        findings.push(format!(
                            "{rel}: census says {} declaration(s), found {count} — a \
                             table was added or removed here, so re-audit it and \
                             update the entry",
                            audited.declarations
                        ));
                    }
                }
            }
        }
        for audited in super::census::AUDITED {
            if !seen.contains(&audited.path) {
                findings.push(format!(
                    "{}: in the census but no longer declares an address-keyed \
                     table — drop the entry (or fix the path if the file moved)",
                    audited.path
                ));
            }
        }

        assert!(
            total_declarations > 0,
            "the census matched no declarations at all — the pattern stopped \
             matching, so this test was about to pass vacuously"
        );
        assert!(
            findings.is_empty(),
            "address-keyed `ObjectRef` table census is out of date.\n\
             `ObjectRef`'s Hash/Eq are addresses: a moving collection strands \
             live entries, and a DEAD entry collides with whatever object the \
             allocator next places on that address — a silent wrong answer, not \
             a miss (docs/threading/objectref-concurrency-contract.md §7.3).\n\
             Give the table a scan+remap pair (see \
             `net_phase_e::gc_scan_inet_addr_roots`), route it through \
             `addr_keyed::remap_and_sweep`, or state why it is safe — then \
             record it in `census::AUDITED`.\n  {}",
            findings.join("\n  ")
        );
    }

    /// gcg g2/k (`common-w33b-proposal-census-raw-address-keyed-tables-DONE-20261015`): the
    /// census above sees tables keyed by `ObjectRef`, not tables keyed by an
    /// address held as a plain `usize` -- the same hazard (a dead object's row
    /// answers for whatever the allocator puts at its address next), invisible
    /// to the pattern. This sibling is a POPULATION ratchet over
    /// `HashMap<usize` / `HashMap<(usize` / `HashSet<usize` / `HashSet<(usize`
    /// declarations (FxHashMap / FxHashSet included) in the trees where object
    /// addresses are routinely keys. Most such keys are not addresses at all
    /// (class ids, VM identities, pcs), which is why it counts instead of
    /// failing on each one.
    ///
    /// A declaration is CLASSIFIED when its line, or the line right above it,
    /// carries `addr-keyed: <disposition>` in a comment (for example
    /// `// addr-keyed: REMAPPED by ..., PRUNED by ...`, or `addr-keyed: not an
    /// address`). Unclassified declarations may not grow past each tree's
    /// baseline; `vm/src/threading` starts fully classified, so any new one
    /// there must carry the marker. Lower a baseline when you classify or
    /// remove a declaration.
    ///
    /// The baselines were counted on the gcg g2 tree (2026-10-01) with the
    /// rule above (code is the text before the first `//`).
    #[test]
    fn gcg_k2_raw_address_keyed_declarations_are_classified_or_counted() {
        const PATTERNS: [&str; 4] = [
            "HashMap<usize",  // addr-keyed: a pattern literal, not a table
            "HashMap<(usize", // addr-keyed: a pattern literal, not a table
            "HashSet<usize",  // addr-keyed: a pattern literal, not a table
            "HashSet<(usize", // addr-keyed: a pattern literal, not a table
        ];
        const MARKER: &str = "addr-keyed:";
        const BASELINES: [(&str, usize); 8] = [
            // 325 -> 336 at the gcg round's 2026-10-02 merge of `dev`: dev brought 11
            // unclassified declarations (classloader.rs 3, classloader_value_sidetable.rs 2,
            // jboss_jdkspecific.rs 2, lang_class.rs 2, reflect_annotations.rs 1 -- plus 3 in
            // test_utils.rs). They need a disposition; see the gcg handoff, item "dev's
            // address-keyed declarations". 336 -> 338 at the second merge (dev `67f4beb552`):
            // lang_class.rs `ANNOTATION_JDK_HANDLERS` (keyed by the annotation proxy's address)
            // and the `dropped` proxy-address set that prunes it.
            // 338 -> 224 (gch h1/ak): every dev declaration classified, plus the classloader /
            // lang_class / logging / lang_math / panama families
            // (`docs/internal/gc/gch-h1ak-raw-address-keyed-audit-FIXED-20261012.md`).
            // 224 -> 220 (interpreter round i4 wave 96, lane L5): `cargo fmt` (dev
            // `dfedfc98ca`, i84 `b49c8bc6e6`) moved seven declarations off the line under
            // their marker (227 unclassified); the markers now sit on the declaration
            // lines (classloader.rs 2, jboss_jdkspecific.rs, lang_class.rs, lang_math.rs,
            // panama.rs 2). The gci round's 2026-10-15 merge of `dev` keeps 220.
            ("native-builtins/src", 220),
            ("native-io/src", 23),
            // 47 -> 49 at the same merge: dev's `deopt_resume.rs` `by_point` and
            // `invoke.rs` `SEEN` (see the gcg handoff). dev then brought `lambda.rs`
            // `LAMBDA_DOOR_SITES` (50, red on dev 2eee67be76). gch h1/ak classified `by_point`,
            // `SEEN`, `LAMBDA_DOOR_SITES` and `jit_bridge.rs`'s `osr_backend` parameter: 46.
            // 46 -> 45 (gci round, 2026-10-13): the gci waves added two, both now classified
            // (i5/hd `hprof.rs` `attributed`, i11/vt `interpreter.rs` unmounted-frames
            // `current`); the census counts 45, so the one row of slack is taken back.
            ("vm/src/runtime", 45),
            ("vm/src/threading", 0),
            // gcj j1/st (2026-10-19): two more trees where object addresses are keys --
            // the GC's own side tables and the VM core. Counted on the gcj base with the
            // rule above after classifying this lane's files (`addr_keyed.rs`,
            // `string_table.rs`): `vm/src/memory` 13 (`gc.rs` 10, `roots.rs` 3: pause-local
            // root / destination sets), `vm/src/vm` 9 (`vm_exec.rs` 7, `vm_init.rs` 1,
            // `vm_util.rs` 1 -- join-handle pointers, VM identities, pause-local sets; none
            // keyed by a heap address that outlives a pause).
            ("vm/src/memory", 13),
            ("vm/src/vm", 9),
            // gcj j2/jn2 (j1-st X1): `vm/src/native` starts fully classified -- the seven
            // `jni.rs` declarations are C-heap `Box` / buffer addresses (global-ref boxes,
            // element / string / critical copies), the G1 critical pins (a heap address held
            // in place by its region pin and strong global until its release removes the row)
            // and one pause-local remark set.
            ("vm/src/native", 0),
            // gcj j2/jt2 (j1-st X2): every declaration in vm/src/jit is classified.
            ("vm/src/jit", 0),
        ];

        fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect_rs(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the vm crate directory must have a parent")
            .to_path_buf();
        let mut findings: Vec<String> = Vec::new();
        let mut classified_total = 0usize;
        for (tree, baseline) in BASELINES {
            let root = workspace.join(tree);
            assert!(
                root.is_dir(),
                "{} is not a directory — the census would scan nothing",
                root.display()
            );
            let mut files = Vec::new();
            collect_rs(&root, &mut files);
            assert!(!files.is_empty(), "no source files under {tree}");
            let mut unclassified: Vec<String> = Vec::new();
            for file in &files {
                let Ok(text) = std::fs::read_to_string(file) else {
                    continue;
                };
                let mut previous = "";
                for (n, line) in text.lines().enumerate() {
                    let code = line.split("//").next().unwrap_or("");
                    if PATTERNS.iter().any(|p| code.contains(p)) {
                        if line.contains(MARKER) || previous.contains(MARKER) {
                            classified_total += 1;
                        } else {
                            let rel = file.strip_prefix(&workspace).unwrap_or(file);
                            unclassified.push(format!(
                                "{}:{}",
                                rel.to_string_lossy().replace('\\', "/"),
                                n + 1
                            ));
                        }
                    }
                    previous = line;
                }
            }
            if unclassified.len() > baseline {
                findings.push(format!(
                    "{tree}: {} unclassified raw-address-keyed declaration(s), \
                     baseline {baseline}. \
                     Classify the new one with an `{MARKER} <disposition>` comment on or \
                     above its line (or raise nothing: give it a GC disposition first). \
                     Unclassified lines:\n    {}",
                    unclassified.len(),
                    unclassified.join("\n    ")
                ));
            }
        }
        assert!(
            classified_total > 0,
            "no classified declaration found — the marker or the pattern stopped matching, \
             so this ratchet was about to pass vacuously"
        );
        assert!(findings.is_empty(), "{}", findings.join("\n"));
    }

    const A: usize = 0x1_0000;
    const B: usize = 0x2_0000;
    const C: usize = 0x3_0000;

    fn key(addr: usize) -> ObjectRef {
        object_ref_at(addr)
    }

    fn table(entries: &[(usize, u32)]) -> FxHashMap<ObjectRef, u32> {
        entries.iter().map(|&(a, v)| (key(a), v)).collect()
    }

    fn nothing_moved() -> cratonvm_types::PointerMap {
        cratonvm_types::PointerMap::default()
    }

    #[test]
    fn moved_entry_is_rekeyed_and_keeps_its_value() {
        let mut t = table(&[(A, 7)]);
        let map = cratonvm_types::PointerMap::from_iter([(A, B)]);

        let stats = remap_and_sweep(&mut t, &map, &|_| true);

        assert_eq!(stats.moved, 1);
        assert_eq!(t.get(&key(B)), Some(&7), "value must follow the object");
        assert!(!t.contains_key(&key(A)), "old address must not linger");
    }

    #[test]
    fn live_unmoved_entry_survives_a_nonmoving_sweep() {
        let mut t = table(&[(A, 7)]);

        let stats = remap_and_sweep(&mut t, &nothing_moved(), &|addr| addr == A);

        assert_eq!(
            stats,
            SweepStats {
                moved: 0,
                retained: 1,
                dropped: 0
            }
        );
        assert_eq!(t.get(&key(A)), Some(&7));
    }

    /// The correctness-critical case: a non-moving collection publishes an
    /// empty pointer map, so a remap-only fixup would be a no-op and the
    /// dead entry would stay to collide with whatever is allocated onto
    /// its reclaimed address next.
    #[test]
    fn dead_entry_is_dropped_even_when_nothing_moved() {
        let mut t = table(&[(A, 7)]);

        let stats = remap_and_sweep(&mut t, &nothing_moved(), &|_| false);

        assert_eq!(stats.dropped, 1);
        assert!(t.is_empty(), "reclaimed address must not stay keyed");
    }

    #[test]
    fn mixed_cycle_sorts_each_entry_into_the_right_bucket() {
        let mut t = table(&[(A, 1), (B, 2), (C, 3)]);
        // A relocated to 0x40000; B survived in place; C died.
        let map = cratonvm_types::PointerMap::from_iter([(A, 0x4_0000)]);

        let stats = remap_and_sweep(&mut t, &map, &|addr| addr == B);

        assert_eq!(
            stats,
            SweepStats {
                moved: 1,
                retained: 1,
                dropped: 1
            }
        );
        assert_eq!(t.get(&key(0x4_0000)), Some(&1));
        assert_eq!(t.get(&key(B)), Some(&2));
        assert_eq!(t.len(), 2);
    }

    /// A relocated survivor evacuated onto a dead entry's old address must
    /// win, whatever order the two entries come out of the table in.
    #[test]
    fn relocated_survivor_wins_a_destination_collision() {
        let mut t = table(&[(A, 1), (B, 2)]);
        // A moves onto B's address; B is dead, but `is_live(B)` now reports
        // true because A occupies that address.
        let map = cratonvm_types::PointerMap::from_iter([(A, B)]);

        let stats = remap_and_sweep(&mut t, &map, &|_| true);

        assert_eq!(t.len(), 1);
        assert_eq!(t.get(&key(B)), Some(&1), "A's value, not B's stale one");
        assert_eq!(
            stats,
            SweepStats {
                moved: 1,
                retained: 0,
                dropped: 1
            }
        );
    }

    /// gc-common w16-x: the batched verdict is the per-address one.
    #[test]
    fn in_place_verdict_agrees_with_survived_in_place() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        use cratonvm_types::ClassId;

        for backend in [GcBackend::Generational, GcBackend::G1] {
            let heap = VmHeap::new(backend, 16 * 1024 * 1024);
            let live = heap.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
            let mut addrs = vec![live, live + 8, 0x1_0000, 8];
            if let Some((lo, hi)) = heap.young_inactive_semispace_range() {
                addrs.extend([lo, hi - 16]);
            }
            let verdict = InPlaceVerdict::capture(&heap);
            for a in addrs {
                assert_eq!(verdict.survived(a), survived_in_place(&heap, a), "{a:#x}");
            }
        }
    }

    /// gc-common w18-a: the batched verdict is the per-address one for every
    /// row a REAL Generational collection leaves behind -- three dead rows in
    /// four, the shape that made the per-address form quadratic -- while the
    /// reclaimed-hole probe behind it is taken at most once for the whole
    /// sweep (`OnceCell`, by construction).
    #[test]
    fn in_place_verdict_agrees_across_a_collection_with_many_dead_rows() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        use cratonvm_types::ClassId;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let monitors = crate::threading::monitor::MonitorTable::new();
        let objs: Vec<_> = (0..256)
            .map(|i| heap.alloc_object(ClassId::new(0), (i % 4) + 1))
            .collect();
        let mut roots: Vec<_> = objs.iter().step_by(4).copied().collect();
        // SAFETY: single-threaded test; this heap has no other mutator.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        let result = heap.collect_garbage(&stw, &mut roots, &monitors);

        let verdict = InPlaceVerdict::capture(&heap);
        let mut dead = 0usize;
        for obj in &objs {
            let a = obj.as_ptr() as usize;
            // A moved row is answered through the pointer map by every sweep,
            // before either verdict is asked.
            if result.pointer_map.contains_key(&a) {
                continue;
            }
            let want = survived_in_place(&heap, a);
            dead += usize::from(!want);
            assert_eq!(verdict.survived(a), want, "{a:#x}");
        }
        assert!(dead > 0, "the fixture must exercise dead rows");
    }

    /// gc-common w18-a: the probe is lazy. A window whose rows never reach the
    /// reclaimed-hole arm (here: addresses that are not objects at all) never
    /// copies the Generational young free list.
    #[test]
    fn in_place_verdict_takes_no_probe_until_an_address_needs_it() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let verdict = InPlaceVerdict::capture(&heap);
        assert!(!verdict.survived(0x1_0000), "not a heap address");
        assert!(!verdict.survived(8), "the null page");
        assert!(!verdict.probe_taken(), "no address needed the free list");
    }

    /// `survived_in_place` never drops a row for an address the collector
    /// says is reclaimed memory is NOT (a live, parseable object), and always
    /// drops one it says IS -- here the inactive young semispace, which no live
    /// object ever occupies.
    #[test]
    fn survived_in_place_keeps_a_live_object_and_drops_reclaimed_space() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        use cratonvm_types::ClassId;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let live = heap.alloc_object(ClassId::new(7), 1);
        let live_addr = live.as_ptr() as usize;
        if heap.is_object_address(live_addr).is_some() {
            assert!(
                survived_in_place(&heap, live_addr),
                "a live object must never be judged dead by the reclaimed-space test"
            );
        }
        let (inactive_lo, _) = heap
            .young_inactive_semispace_range()
            .expect("the Generational heap has a semispace pair");
        assert!(
            !survived_in_place(&heap, inactive_lo),
            "the inactive semispace holds no live object; a row keyed there is dead"
        );
        assert!(
            !survived_in_place(&heap, 0x1_0000),
            "not a heap address at all"
        );
    }

    /// The sweep-only entry point for reclamations outside a pause runs on a
    /// fresh VM without touching anything it should not (both tables empty).
    #[test]
    fn sweep_address_keyed_tables_is_callable_on_an_idle_vm() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        sweep_address_keyed_tables(&shared);
    }

    /// gc-common w3-g: the out-of-pause sweep (the entry point the Generational
    /// concurrent old-gen sweep and G1's remark cleanup now call) drops a
    /// Throwable-trace row keyed at space the collector reports RECLAIMED --
    /// here the inactive young semispace, which no live object occupies -- and
    /// keeps the row of a live object. This is "free at A, sweep before A is
    /// reused": without the sweep the dead row would stay until the next STW
    /// collection, by which time an object placed at A would inherit it.
    #[test]
    fn the_out_of_pause_sweep_drops_a_row_keyed_at_reclaimed_space() {
        use crate::config::{GcAlgorithm, VmConfig};
        use cratonvm_types::ClassId;

        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let heap = &shared.mem.heap;
        let live = heap.alloc_object(ClassId::new(7), 1);
        let (dead_addr, _) = heap
            .young_inactive_semispace_range()
            .expect("the Generational heap has a semispace pair");
        let dead = object_ref_at(dead_addr);
        let no_frames: std::sync::Arc<[crate::native::registry::StackTraceEntry]> =
            std::sync::Arc::from(Vec::new());
        shared.store_throwable_stack_trace(live, no_frames.clone());
        shared.store_throwable_stack_trace(dead, no_frames);
        assert!(shared.throwable_stack_trace_for(dead).is_some());

        sweep_address_keyed_tables(&shared);

        assert!(
            shared.throwable_stack_trace_for(dead).is_none(),
            "a row keyed at reclaimed space must be dropped by the out-of-pause sweep"
        );
        if heap.is_object_address(live.as_ptr() as usize).is_some() {
            assert!(
                shared.throwable_stack_trace_for(live).is_some(),
                "a live object's row must survive the sweep"
            );
        }
    }

    /// gc-common w5-g: the span test the concurrent sweep's freed-span drop
    /// uses. `start` and `start + len - 1` are in, `start + len` is out, a gap
    /// between spans is out, and no spans means nothing is in.
    #[test]
    fn in_spans_boundaries() {
        let spans = [(0x1000usize, 0x40usize), (0x2000, 0x10)];
        assert!(in_spans(0x1000, &spans));
        assert!(in_spans(0x103F, &spans));
        assert!(!in_spans(0x1040, &spans));
        assert!(!in_spans(0x0FFF, &spans));
        assert!(!in_spans(0x1800, &spans), "the gap between spans");
        assert!(in_spans(0x2000, &spans));
        assert!(in_spans(0x200F, &spans));
        assert!(!in_spans(0x2010, &spans));
        assert!(!in_spans(0x1000, &[]));
    }

    /// gc-common w5-g (`common-w3g-concurrent-sweep-direct-old-alloc-window`):
    /// the freed-span drop removes a Throwable-trace row keyed inside a freed
    /// span WITHOUT asking the heap -- so a humongous array that has since
    /// landed on the span cannot make it look live -- and keeps a row keyed
    /// outside every span.
    #[test]
    fn the_freed_span_drop_is_a_pure_range_test() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let dead = object_ref_at(A + 0x40);
        let outside = object_ref_at(B);
        let no_frames: std::sync::Arc<[crate::native::registry::StackTraceEntry]> =
            std::sync::Arc::from(Vec::new());
        shared.store_throwable_stack_trace(dead, no_frames.clone());
        shared.store_throwable_stack_trace(outside, no_frames);

        drop_address_keyed_rows_in(&shared, &[]);
        assert!(
            shared.throwable_stack_trace_for(dead).is_some(),
            "no spans: no-op"
        );

        drop_address_keyed_rows_in(&shared, &[(A, 0x100)]);
        assert!(
            shared.throwable_stack_trace_for(dead).is_none(),
            "a row keyed inside a freed span is dropped"
        );
        assert!(
            shared.throwable_stack_trace_for(outside).is_some(),
            "a row keyed outside every span is kept"
        );
    }

    /// gcd d10/r: an unmoved row keyed at the address ANOTHER object was
    /// relocated onto is dead, even when that object has no row of its own --
    /// the slide collision `relocated_survivor_wins_a_destination_collision`
    /// covers only when both have rows.
    #[test]
    fn gcd_d10r_an_unmoved_row_at_a_relocation_destination_is_dropped() {
        // B died; A (no row here) slid onto B's base. `is_live(B)` is true
        // after the pause -- A lives there now.
        let mut t = table(&[(B, 2), (C, 3)]);
        let map = cratonvm_types::PointerMap::from_iter([(A, B)]);

        let stats = remap_and_sweep(&mut t, &map, &|_| true);

        assert_eq!(
            stats,
            SweepStats {
                moved: 0,
                retained: 1,
                dropped: 1
            }
        );
        assert!(!t.contains_key(&key(B)), "B's row must not pass to A");
        assert_eq!(t.get(&key(C)), Some(&3), "an uninvolved survivor stays");
    }

    /// gcd d10/r: the destination set answers "another survivor now lives
    /// here" for a moved pair's target, never for its source, never for an
    /// identity pair (a stationary survivor), and is built lazily.
    #[test]
    fn gcd_d10r_relocation_targets_claim_only_other_objects_destinations() {
        let map = cratonvm_types::PointerMap::from_iter([(A, B), (C, C)]);
        let targets = RelocationTargets::consulting(Some(&map));
        assert!(!targets.built(), "nothing asked yet");
        assert!(targets.claims(B), "A was relocated onto B");
        assert!(targets.built());
        assert!(!targets.claims(A), "a source is not a destination");
        assert!(
            !targets.claims(C),
            "an identity pair is a stationary survivor"
        );

        let none = RelocationTargets::none();
        assert!(!none.claims(B));
        assert!(!none.built(), "an unarmed screen never builds a set");
    }

    /// gcd d10/r: the screen is armed only where a relocation can land a
    /// survivor on the base of an object that died in the same pause: never on
    /// G1, on Generational only when the cycle reclaimed old storage, and never
    /// for an empty map.
    #[test]
    fn gcd_d10r_the_relocation_screen_is_armed_only_where_a_base_can_be_reused() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        /// `old_gen_reclaimed_last_cycle` is this thread's; put it back.
        struct RestoreReclaimed(bool);
        impl Drop for RestoreReclaimed {
            fn drop(&mut self) {
                cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(self.0);
            }
        }
        let _restore = RestoreReclaimed(cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle());

        let map = cratonvm_types::PointerMap::from_iter([(A, B)]);
        let empty = cratonvm_types::PointerMap::default();

        let g1 = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
        assert!(
            !RelocationTargets::for_pause(&g1, &map).claims(B),
            "G1 evacuates into free regions"
        );

        let gen = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(false);
        assert!(
            !RelocationTargets::for_pause(&gen, &map).claims(B),
            "a young-only cycle copies into space that held no object"
        );
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
        assert!(
            RelocationTargets::for_pause(&gen, &map).claims(B),
            "an old-gen compaction slides survivors onto dead bases"
        );
        assert!(!RelocationTargets::for_pause(&gen, &empty).claims(B));
    }

    /// gcd d10/r: a verdict taken with the pause's relocations refuses a live,
    /// parseable address that a DIFFERENT survivor was moved onto, and keeps
    /// it for an identity pair and for a verdict without relocations.
    #[test]
    fn gcd_d10r_an_address_a_survivor_was_moved_onto_did_not_survive_in_place() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        use cratonvm_types::ClassId;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let live = heap.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
        if heap.is_object_address(live).is_none() {
            // As `survived_in_place_keeps_a_live_object_and_drops_reclaimed_space`:
            // nothing to judge if the heap does not parse its own allocation.
            return;
        }
        assert!(InPlaceVerdict::capture(&heap).survived(live));

        let slid_onto = cratonvm_types::PointerMap::from_iter([(live + 0x100, live)]);
        let screened = InPlaceVerdict {
            heap: &heap,
            probe: std::cell::OnceCell::new(),
            targets: VerdictTargets::Own(RelocationTargets::consulting(Some(&slid_onto))),
        };
        assert!(screened.claimed_by_relocation(live));
        assert!(
            !screened.survived(live),
            "the object that was here died; the one here now arrived by relocation"
        );

        let stationary = cratonvm_types::PointerMap::from_iter([(live, live)]);
        let kept = InPlaceVerdict {
            heap: &heap,
            probe: std::cell::OnceCell::new(),
            targets: VerdictTargets::Own(RelocationTargets::consulting(Some(&stationary))),
        };
        assert!(
            kept.survived(live),
            "an identity pair is a stationary survivor"
        );
    }

    /// `old_gen_reclaimed_last_cycle` is this thread's; the gci i22/dr tests
    /// set it and put it back.
    struct I22drRestoreReclaimed(bool);
    impl Drop for I22drRestoreReclaimed {
        fn drop(&mut self) {
            cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(self.0);
        }
    }

    /// gci i22/dr (P2/dr): the pause's ONE destination set, lent to three
    /// verdicts with `capture_with`, answers every address exactly as three
    /// private sets (`capture_after`, the pre-i22 shape) answer it, and is
    /// built once: the first verdict that asks builds it, the others reuse it.
    #[test]
    fn gci_i22dr_one_shared_relocation_set_answers_as_three_private_ones() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let _restore =
            I22drRestoreReclaimed(cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle());
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        // Moved pairs (one onto another's source), an identity pair, a pair
        // to 0 (never a destination).
        let map = cratonvm_types::PointerMap::from_iter([
            (A, B),
            (B, C),
            (C + 0x100, C + 0x200),
            (C + 0x300, C + 0x300),
            (C + 0x400, 0),
        ]);
        let shared = RelocationTargets::for_pause(&heap, &map);
        assert!(!shared.built(), "lazy: nothing asked yet");
        let lent = [
            InPlaceVerdict::capture_with(&heap, &shared),
            InPlaceVerdict::capture_with(&heap, &shared),
            InPlaceVerdict::capture_with(&heap, &shared),
        ];
        let private = [
            InPlaceVerdict::capture_after(&heap, &map),
            InPlaceVerdict::capture_after(&heap, &map),
            InPlaceVerdict::capture_after(&heap, &map),
        ];
        let probes = [0usize, A, B, C, C + 0x100, C + 0x200, C + 0x300, C + 0x400, C + 0x500];
        for (i, (l, p)) in lent.iter().zip(private.iter()).enumerate() {
            for &addr in &probes {
                assert_eq!(
                    l.claimed_by_relocation(addr),
                    p.claimed_by_relocation(addr),
                    "verdict {i}, address {addr:#x}"
                );
            }
            assert!(shared.built(), "built by the first verdict that asked");
        }
        assert!(lent[0].claimed_by_relocation(B), "A was moved onto B");
        assert!(lent[1].claimed_by_relocation(C + 0x200));
        assert!(!lent[2].claimed_by_relocation(C + 0x300), "an identity pair");
        assert!(!lent[2].claimed_by_relocation(0), "a pair to 0 is no destination");
        assert!(!lent[2].claimed_by_relocation(A), "a source is no destination");
    }

    /// gci i22/dr: the shared set keeps the d10/r arming rule -- lent from a
    /// G1 heap, a young-only Generational cycle or an empty map, it claims
    /// nothing and is never built.
    #[test]
    fn gci_i22dr_a_shared_unarmed_set_claims_nothing_and_builds_nothing() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let _restore =
            I22drRestoreReclaimed(cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle());
        let map = cratonvm_types::PointerMap::from_iter([(A, B)]);
        let empty = cratonvm_types::PointerMap::default();

        let g1 = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
        let t = RelocationTargets::for_pause(&g1, &map);
        assert!(!InPlaceVerdict::capture_with(&g1, &t).claimed_by_relocation(B));
        assert!(!t.built(), "G1 never builds the set");

        let gen = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(false);
        let t = RelocationTargets::for_pause(&gen, &map);
        assert!(!InPlaceVerdict::capture_with(&gen, &t).claimed_by_relocation(B));
        assert!(!t.built(), "a young-only cycle never builds the set");

        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
        let t = RelocationTargets::for_pause(&gen, &empty);
        assert!(!InPlaceVerdict::capture_with(&gen, &t).claimed_by_relocation(B));
        assert!(!t.built(), "an empty map never builds the set");
    }

    /// gci i22/dr: `survived` through a lent set is `survived` through a
    /// private one -- a live, parseable address another survivor slid onto
    /// is refused by both, an uninvolved live address kept by both.
    #[test]
    fn gci_i22dr_survived_through_the_shared_set_matches_capture_after() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        use cratonvm_types::ClassId;

        let _restore =
            I22drRestoreReclaimed(cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle());
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let live = heap.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
        let other = heap.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
        if heap.is_object_address(live).is_none() || heap.is_object_address(other).is_none() {
            // As `gcd_d10r_an_address_a_survivor_was_moved_onto_did_not_survive_in_place`.
            return;
        }
        let map = cratonvm_types::PointerMap::from_iter([(live + 0x1000, live)]);
        let shared = RelocationTargets::for_pause(&heap, &map);
        let lent = InPlaceVerdict::capture_with(&heap, &shared);
        let private = InPlaceVerdict::capture_after(&heap, &map);
        for addr in [live, other] {
            assert_eq!(lent.survived(addr), private.survived(addr), "{addr:#x}");
        }
        assert!(!lent.survived(live), "slid onto: the object that was here died");
        assert!(lent.survived(other), "an uninvolved survivor stays");
    }

    /// gci i28/wt (`gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`):
    /// the pause's G1 unheld set rides on its one `RelocationTargets` and every
    /// verdict lent that value refuses exactly those addresses; an address
    /// outside the set keeps the old answer, a set-less value changes nothing,
    /// and the set never arms the relocation screen.
    #[test]
    fn gci_i28_wt_the_pause_s_g1_unheld_set_is_refused_by_every_lent_verdict() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        use cratonvm_types::ClassId;

        let g1 = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
        let dead = g1.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
        let live = g1.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
        let map = cratonvm_types::PointerMap::default();

        let plain = RelocationTargets::for_pause(&g1, &map);
        assert!(!plain.g1_unheld(dead), "no verdict handed in");
        assert!(plain.g1_unheld_set().is_empty());
        let before = InPlaceVerdict::capture_with(&g1, &plain);
        let (dead_before, live_before) = (before.survived(dead), before.survived(live));

        // addr-keyed: test-local PRE-collection addresses.
        let unheld: FxHashSet<usize> = [dead].into_iter().collect();
        let targets = RelocationTargets::for_pause(&g1, &map).with_g1_unheld(unheld);
        assert!(targets.g1_unheld(dead));
        assert!(!targets.g1_unheld(live));
        assert_eq!(targets.g1_unheld_set().len(), 1);
        let lent = [
            InPlaceVerdict::capture_with(&g1, &targets),
            InPlaceVerdict::capture_with(&g1, &targets),
        ];
        for v in &lent {
            assert!(!v.survived(dead), "an unheld address never survives in place");
            assert_eq!(v.survived(live), live_before, "outside the set: the old answer");
            assert!(!v.claimed_by_relocation(dead), "the set is not a relocation claim");
        }
        assert!(!targets.built(), "G1 still never builds the destination set");

        // A verdict not over the pause's value never sees the set.
        assert_eq!(InPlaceVerdict::capture(&g1).survived(dead), dead_before);
    }

    #[test]
    fn empty_table_is_a_no_op() {
        let mut t: FxHashMap<ObjectRef, u32> = FxHashMap::default();

        let stats = remap_and_sweep(
            &mut t,
            &cratonvm_types::PointerMap::from_iter([(A, B)]),
            &|_| true,
        );

        assert_eq!(stats, SweepStats::default());
        assert!(t.is_empty());
    }
}
