// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
/// `docs/feature-designs/real-jdk-mode.md`.
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
    /// The G1 near-full proxy-define page: the `[base, end)` ranges of the
    /// regions this pause's G1 in-place full compaction SLID
    /// (`VmHeap::g1_last_pause_slid_ranges`). Every live object there is a key
    /// of the pause's pointer map (identity forwards included), so an address
    /// in these ranges that is not a key held no live object -- whatever
    /// parses there now is a survivor slid onto or over it. Empty off G1 and
    /// on every pause that did not slide.
    g1_slid: Vec<(usize, usize)>,
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
        let mut t = Self::consulting(armed.then_some(pointer_map));
        if heap.is_g1() {
            t.g1_slid = heap.g1_last_pause_slid_ranges();
        }
        t
    }

    fn consulting(map: Option<&'m cratonvm_types::PointerMap>) -> Self {
        Self {
            map,
            set: std::cell::OnceCell::new(),
            g1_unheld: FxHashSet::default(),
            g1_slid: Vec::new(),
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
        // The G1 in-place slide (see `g1_slid`): the callers ask only for an
        // address that is not a key, and a non-key in a slid region is dead.
        if self.g1_slid.iter().any(|&(lo, hi)| addr >= lo && addr < hi) {
            return true;
        }
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

    /// Was this pause's relocation set armed?
    ///
    /// Armed on ZGC (every moving cycle), Generational (when the cycle reclaimed
    /// old storage), and G1 (when the pause slid). Unarmed on G1 evacuations
    /// and young-only Generational cycles.
    #[inline]
    pub(crate) fn is_armed(&self) -> bool {
        self.map.is_some() || !self.g1_slid.is_empty()
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
/// (`docs/feature-designs/real-jdk-mode.md`):
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
/// `docs/feature-designs/real-jdk-mode.md`).
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
/// `docs/feature-designs/real-jdk-mode.md`
/// and its handoff for the two call sites.
///
/// Deliberately excludes the GPU input-residency cache
/// (`offload::input_cache::remap_and_sweep`), whose drain-then-rekey
/// protocol documents that mutators are stopped.
///
/// Callers (gc-common w3-g, applying
/// `docs/feature-designs/real-jdk-mode.md`):
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
    // locks are `Scratch` leaves below the old-gen guard. The span entry
    // point judges the ranges under the owner lock in one pass, instead of
    // copying every owner address of the VM out per slice (common-w26c
    // item 2); it drops the rows `gc_sweep_tls_rows` would.
    cratonvm_native_builtins::t27_tls::gc_drop_tls_rows_in_spans(shared.vm_identity, spans);
    // JUL / JULI logging rows (gc-common w14-d): a pure range test; the
    // logging table locks are leaves below the old-gen guard.
    cratonvm_native_builtins::logmanager::gc_sweep_logging_rows(shared.vm_identity, &|addr| {
        !in_spans(addr, spans)
    });
    // Lock-key registry slots (gc-common w14-c): a pure range test; the
    // registry lock is a leaf below the old-gen guard. Since gc-common w26-c
    // the span entry point tests the ranges under the registry lock in one
    // pass, instead of copying every slot of the VM out per slice
    // (`common-w25a-lock-key-sweep-walks-the-whole-registry-per-concurrent-sweep-slice`);
    // since common-w26c item 1 it reads only the slots inside the spans, off
    // the registry's address index.
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
mod census;

#[cfg(test)]
mod tests;
