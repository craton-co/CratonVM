#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;

impl VmHeap {
    /// gci i32/ec: G1's private closure of the dead finalizables the last
    /// kept-region verdict found (`dead`, unchanged addresses): the young
    /// objects (POST-collection addresses) only they reach. See
    /// `G1Collector::kept_finalizables_private_closure`. `Err(NotArmed)` off
    /// G1.
    pub fn g1_kept_finalizables_private_closure(
        &self,
        dead: &[usize],
        roots: &[usize],
    ) -> Result<cratonvm_gc_g1::g1::G1KeptFinalizableClosure, cratonvm_gc_g1::g1::G1PinnedReferentDecline> {
        match &self.inner {
            VmHeapInner::G1(h) => h.kept_finalizables_private_closure(dead, roots),
            _ => {
                let _ = (dead, roots);
                Err(cratonvm_gc_g1::g1::G1PinnedReferentDecline::NotArmed)
            }
        }
    }

    /// Exact post-collection survival verdict for a PRE-collection address
    /// that was published as WATCHED before the collection ran (every address
    /// the reference processor holds — see
    /// `ReferenceProcessor::all_tracked_addrs`).
    ///
    /// This is the strict counterpart of [`Self::is_addr_live`], and exists
    /// because that predicate's old-generation arm is deliberately permissive:
    /// it answers `true` for *any* address inside the old-gen arena, which is
    /// correct for a minor cycle (old gen is not touched, so nothing moved)
    /// but wrong the moment an old-gen reclamation runs in the same cycle.
    /// After a mark-compact `major_gc` a dead old-gen object has been slid
    /// over by a live neighbour or left in the zeroed freed tail; after the
    /// in-place `sweep_old_gen_non_moving` its block is back on the free list.
    /// Reference processing then judged the dead entry "survived", never
    /// pruned it, and every subsequent collection wrote a referent pointer
    /// through the stale address into whatever now occupied that memory —
    /// dropped by the `gen_heap::set_field` out-of-bounds guard when the
    /// victim was the zeroed tail, and a silent dangling-pointer store into a
    /// live object otherwise (`SIGSEGV` /
    /// `gen_heap::read_slot: corrupt Value cell`, the HIB-CV-32 family; see
    /// `bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md`).
    ///
    /// Both old-gen paths now emit an identity `pointer_map` entry for every
    /// watched address that survived without moving, so once
    /// `gc_quiescence::old_gen_reclaimed_last_cycle()` is set, map membership
    /// is a complete and exact proof.
    pub fn watched_pre_gc_addr_survived(
        &self,
        addr: usize,
        pointer_map: &cratonvm_types::PointerMap,
    ) -> bool {
        // gch h4/wr (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD`, default ON since gci i1/og and jr20 wave 9):
        // a watched young survivor the last true-root major left out of its
        // trace is dead, though it is still in place. Asked FIRST, before the
        // map (its identity entry was removed, but this stays the one
        // authority) and before the permissive escape hatch below: once its
        // old referents may be freed, "live" would hand back a use-after-free.
        if self.true_root_watched_left_out(addr) {
            return false;
        }
        // gcj j1yg-pause: an OLD address on a cycle that reclaimed no old
        // storage is a survivor whatever the map holds for it (the arm below
        // answers the same) -- asked before the map, so the reference passes'
        // thousands of old rows each skip a hash probe into a map of every
        // young survivor.
        if let VmHeapInner::Generational(h) = &self.inner {
            if h.is_old_gen_addr(addr) && !cratonvm_gc_core::gc_quiescence::old_gen_reclaimed_last_cycle() {
                return true;
            }
        }
        if pointer_map.contains_key(&addr) {
            return true;
        }
        // Bisection escape hatch — restores the pre-fix permissive predicate.
        if crate::gc_flags().no_exact_refproc_survival {
            return self.is_addr_live(addr);
        }
        match &self.inner {
            VmHeapInner::Generational(h) => {
                if h.is_old_gen_addr(addr) {
                    return !cratonvm_gc_core::gc_quiescence::old_gen_reclaimed_last_cycle();
                }
                h.is_live_young_survivor(addr)
            }
            // G1/ZGC: `is_addr_live` (live-region membership / registry
            // lookup); neither has a generation whose addresses are trusted
            // wholesale.
            //
            // gci i27/gr: G1's arm is exact only for a region the pause
            // EVACUATED (its survivors are map keys, so a non-key there is
            // dead). A young region the pause kept in place (a JIT or JNI pin,
            // an evacuation failure) keeps every object below its cursor, dead
            // or alive, and this answers "survived" for all of them. Reference
            // processing asks G1's per-object verdict for those first
            // (`g1_pinned_young_referents_unheld`, `process_references_after_gc`).
            // gci i28/wt: the JNI weak-global sweep refuses the same unheld
            // referents through the pause's `InPlaceVerdict::survived`, which
            // it ANDs with this; an address-keyed side-table row is refused
            // only when its owner is also one of the offered candidates -- see
            // `gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`.
            VmHeapInner::G1(_) => self.is_addr_live(addr),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => self.is_addr_live(addr),
        }
    }

    /// ZGC's relocation ledger: what the slide moved AWAY from `addr`, as
    /// `(from, moved_to, class_id, size, still_live_at_target)`.
    ///
    /// `None` on every other backend, and `None` on ZGC unless
    /// `CRATONVM_DBG_ZGC_CORPSE` armed the run -- the ledger costs a map insert
    /// per relocated object and a cycle relocates hundreds of thousands, so it
    /// cannot be flag-free. It is asked anyway because it is the one thing that
    /// separates "the holder was never rewritten when its referent moved" from
    /// "the object died later": `still_live_at_target` answers exactly that.
    pub fn zgc_corpse_lookup(&self, addr: usize) -> Option<(usize, usize, u32, usize, bool)> {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.corpse_lookup(addr),
            _ => {
                let _ = addr;
                None
            }
        }
    }

    /// H2-CID0 — see [`GenerationalHeap::live_holders_of`]. Empty for every
    /// backend that does not publish one (G1).
    pub fn live_holders_of(&self, addr: usize, cap: usize) -> Vec<(usize, u32, usize)> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.live_holders_of(addr, cap),
            // See `ZgcRealHeap::live_holders_of`: the slot ordinal it reports
            // is the object's own reference-slot ordinal, not a field index.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.live_holders_of(addr, cap),
            _ => Vec::new(),
        }
    }

    /// Is `addr` inside memory the collector has RECLAIMED?
    ///
    /// H2-CID0 — see [`GenerationalHeap::reclaimed_hole_at`] for why this
    /// exists: it is the flag-free discriminator between an ordinary
    /// `new Object()` and a reference into a span the collector freed and
    /// zeroed, which are otherwise indistinguishable at the point a
    /// `checkcast` fails with `java.lang.Object` as the actual class.
    ///
    /// G1 returns `None`: its liveness is region-based and `is_addr_live`
    /// already answers exactly, so there is no free-list view to consult. The
    /// ZGC arm's `None` rested on the same sentence and that half of it was
    /// wrong — see the arm's own comment below; it answers now.
    ///
    /// Cost: a young from-space address on Generational, and any arena address
    /// on ZGC, copies and sorts that arena's whole free list PER CALL. A caller
    /// that judges many addresses in one allocation-free window takes
    /// [`Self::reclaimed_hole_probe`] once and asks [`Self::is_reclaimed_hole`]
    /// (gc-common w16-x / w18-a; the stop-the-world epilogue's
    /// `addr_keyed::InPlaceVerdict`).
    //
    // Doc placement, 2026-09-20: this block, and `live_holders_of`'s, had both
    // slid onto `zgc_corpse_lookup` above, which is why the "G1/ZGC return
    // `None`" sentence outlived the 2026-08-17 ZGC arm that contradicts it —
    // nothing was reading it next to the code it described.
    pub fn reclaimed_hole_at(&self, addr: usize) -> Option<(&'static str, usize, usize)> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.reclaimed_hole_at(addr),
            VmHeapInner::G1(_) => None,
            // ZGC answers this now (2026-08-17). The arm said `None` on the
            // grounds that "their liveness is region/registry based and
            // `is_addr_live` already answers exactly, so there is no free-list
            // view to consult" -- true of G1, but ZGC's sweep zeroes each dead
            // object and returns its span to an arena free list, which is
            // precisely the view this predicate wants. While it answered
            // `None`, the DEFAULT collector reported nothing at all for a
            // reclaimed receiver.
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.reclaimed_hole_at(addr),
        }
    }

    /// Capture a [`ReclaimedHoleProbe`]. Stale once anything allocates, so
    /// only a stop-the-world epilogue may hold one.
    ///
    /// Per backend (gc-common w18-a: spelled out rather than `_ =>`, the rule
    /// this type's doc states):
    ///
    /// * Generational: the young free list and semispace geometry, once.
    /// * G1: nothing to capture; `reclaimed_hole_at` is `None` for free.
    /// * ZGC: nothing worth capturing for the `survived_in_place` verdict. Its
    ///   `reclaimed_hole_at` does rebuild the arena free list per call, but the
    ///   verdict never reaches it: ZGC's `is_object_address` and `is_addr_live`
    ///   are the same exact-registry test, so an address either fails the
    ///   first (dead, answered without the free list) or passes the second
    ///   (live). Should that ever change, capture the arena list here.
    pub fn reclaimed_hole_probe(&self) -> ReclaimedHoleProbe {
        match &self.inner {
            VmHeapInner::Generational(h) => ReclaimedHoleProbe::Generational(h.reclaimed_hole_probe()),
            VmHeapInner::G1(_) => ReclaimedHoleProbe::PerAddress,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => ReclaimedHoleProbe::PerAddress,
        }
    }

    /// `self.reclaimed_hole_at(addr).is_some()`, answered through `probe`.
    pub fn is_reclaimed_hole(&self, probe: &ReclaimedHoleProbe, addr: usize) -> bool {
        match (&self.inner, probe) {
            (VmHeapInner::Generational(h), ReclaimedHoleProbe::Generational(p)) => {
                h.is_reclaimed_hole_probed(p, addr)
            }
            _ => self.reclaimed_hole_at(addr).is_some(),
        }
    }

    /// Generational: `[base, base + capacity)` of the INACTIVE young
    /// semispace — the arena a moving cycle has just evacuated and zeroed.
    ///
    /// This is [`reclaimed_hole_at`](Self::reclaimed_hole_at)'s
    /// inactive-semispace arm hoisted into a plain range, so a caller that
    /// must test many addresses at once (the blocked-thread root audit in
    /// `ThreadRegistry::fold_pointer_map_into_blocked`) pays one arena lock
    /// instead of three per address. No live object is ever in here: a live
    /// young object is in the active semispace or in old gen.
    ///
    /// G1/ZGC have no semispace pair, hence `None`.
    pub fn young_inactive_semispace_range(&self) -> Option<(usize, usize)> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.young_inactive_semispace_range()),
            VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        }
    }

    /// Generational: the young addresses the last collection vacated -- the
    /// inactive semispace, plus a pinned in-place cycle's took-back spans (gen
    /// r5w5/pin9). G1/ZGC have neither, hence `None`.
    pub fn young_vacated_probe(&self) -> Option<cratonvm_gc_gen::gen_heap::YoungVacatedProbe> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.young_vacated_probe()),
            VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        }
    }

    /// Publish the young semispace geometry for
    /// `gen_heap::dead_young_ref_reason_global`. No-op on the backends that
    /// have no semispace pair.
    pub fn publish_young_geometry(&self) {
        if let VmHeapInner::Generational(h) = &self.inner {
            h.publish_young_geometry();
        }
    }

    /// Generational: is `addr` a young reference naming no live object, and
    /// why? See `GenerationalHeap::dead_young_ref_reason`.
    ///
    /// `None` on every other backend — the predicate is defined in terms of a
    /// semispace pair, and G1/ZGC have none.
    pub fn dead_young_ref_reason(&self, addr: usize) -> Option<&'static str> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.dead_young_ref_reason(addr),
            VmHeapInner::G1(_) => None,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => None,
        }
    }

    /// Generational: is `addr` inside EITHER young semispace? Used by
    /// reference processing to detect stale PRE-GC Reference addresses
    /// (young + absent from the pointer map ⇒ did not survive the GC).
    /// G1: `false` (no semispace; staleness is handled by `is_addr_live`).
    pub fn is_in_young_addr(&self, addr: usize) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.is_in_young_either(addr as *const u8),
            VmHeapInner::G1(_) => false,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => false,
        }
    }

    /// True when `addr` is safe to defer to the loader-scoped
    /// `metadata_pin` side-channel instead of pushing it as an unconditional
    /// GC root (see `vm::memory::roots`'s static-field, class-lock and
    /// CONSTANT_Dynamic root sections, all gated on `conditional_metadata`).
    ///
    /// Generational: every old-gen address, and — gc-common w36-d — a YOUNG
    /// address on exactly the cycles [`Self::mirror_pin_deferrable`] defers a
    /// young mirror on (`young_marker_follows_side_tables`: the cycle is
    /// certain to take the non-moving young precise marker), unless
    /// `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER` opts out of both.
    ///
    /// Why a young value was refused until w36-d: the MOVING young copy
    /// closure seeds strictly from the direct root set and never consults
    /// `metadata_pin`, so a young address deferred on a moving cycle has no
    /// path to be marked; if it has no other reachability (a static field's
    /// value right after `<clinit>` assigns it, a class-lock / condy object)
    /// it is reclaimed and its memory reused — a live object that reads back
    /// as a DIFFERENT, unrelated type
    /// (`spb1-springframework-util-investigation-FIXED.md`, repro 3). The
    /// non-moving young precise marker is different: `scan_young_object`
    /// follows `metadata_pin` from every young owner it marks, and since w36-d
    /// `seed_metadata_pins_of_old_owners` seeds the values of every OLD owner
    /// (which no young cycle scans) — the precondition the w3-b handoff named.
    /// So on a cycle certain to take that marker a young value is reached
    /// through its loader, and deferring it is what lets the loader unload
    /// (`common-w2b-generational-young-classvalue-values-root-their-class`,
    /// `common-w16b-clv-values-are-permanent-roots-that-pin-their-loader`).
    ///
    /// G1 / ZGC: always `true` — both backends' `metadata_pin` consumers
    /// (`g1.rs`, `zgc.rs`) walk every live region uniformly during the same
    /// full-mark pass that activates `conditional_metadata`, so deferring a
    /// young-resident object is sound; this matches their existing,
    /// unconditional behavior and is unchanged here.
    pub fn metadata_pin_deferrable(&self, addr: usize) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                if h.is_old_gen_addr(addr) {
                    return true;
                }
                mirror_pin_young_defer_enabled()
                    && cratonvm_gc_core::gc_quiescence::young_marker_follows_side_tables()
            }
            VmHeapInner::G1(_) => true,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => true,
        }
    }

    /// Same question as [`Self::metadata_pin_deferrable`], but for the
    /// `mirror_pin` side channel specifically (`vm::memory::roots` step 6 —
    /// `java.lang.Class` mirrors of user-loader-defined classes).
    ///
    /// `metadata_pin_deferrable` is old-gen-only under Generational because
    /// `metadata_pin`'s guaranteed consumer is `old_gen_gc`'s BFS. `mirror_pin`
    /// has a SECOND consumer the metadata case cannot rely on:
    /// `mark_young_precise_object` follows it as an ordinary marking edge — so
    /// whenever this cycle is certain to take the non-moving young marker, a
    /// still-YOUNG mirror is reachable through its loader and does not need an
    /// unconditional root.
    ///
    /// This is what makes class unloading work at all for a short-lived loader:
    /// a webapp/JSP class whose mirror has never been promoted (the common case
    /// when an explicit `System.gc()` is the first collection of the run) would
    /// otherwise be rooted directly forever, and — since a mirror's
    /// `classLoader` field is a real heap edge — would drag its entire defining
    /// loader and everything that loader references along with it. Symptom:
    /// `TestDefaultInstanceManager.testClassUnloading` counting 9 cached
    /// annotation entries where HotSpot has 8, because the evicted JSP's
    /// `Class` was the sole root-held anchor of the whole compiler graph. See
    /// `docs/feature-designs/concurrent-gc-maturation.md`.
    ///
    /// Opt out with `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER=1`, which restores the
    /// old old-gen-only behaviour for bisection.
    pub fn mirror_pin_deferrable(&self, addr: usize) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                if h.is_old_gen_addr(addr) {
                    return true;
                }
                mirror_pin_young_defer_enabled()
                    && cratonvm_gc_core::gc_quiescence::young_marker_follows_side_tables()
            }
            VmHeapInner::G1(_) => true,
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => true,
        }
    }

    /// Backend-generic post-GC staleness verdict for a PRE-collection
    /// address: `true` iff the object at `addr` did NOT survive the
    /// collection whose `pointer_map` is supplied — i.e. writing through
    /// `addr` (or `pointer_map`-relocating it) would touch freed/reused
    /// memory. Used by the post-GC reference-processing writers
    /// (clear/enqueue/finalize/cleaner) as their anti-corruption guard.
    ///
    /// Per backend:
    /// - Generational: a young-space address absent from the pointer map did
    ///   not survive (a live young object is always in the map after a
    ///   moving young GC, and non-moving sweeps emit identity entries for
    ///   watched survivors). Old-gen addresses are conservatively treated
    ///   as surviving (they do not move in a minor GC; major relocations
    ///   are merged into the map).
    /// - G1: every live CSet object is in the pointer map (identity entries
    ///   for self-forwarded ones) and every live non-CSet address sits in a
    ///   live region — so "absent from the map AND not in a live region" is
    ///   exact. This closes the G1/ZGC hole where the old young-only guard
    ///   was hardwired inert and stale finalize/cleaner addresses flowed to
    ///   `run_finalizers` (UAF on recycled CSet memory).
    /// - ZGC: dead means gone from the registry (`is_addr_live` false). The
    ///   moved case never reaches that arm: the `pointer_map` test at the top
    ///   of this function answers first. That is what keeps the arm correct
    ///   now that ZGC compacts by default (`CRATONVM_ZGC_RELOCATE`, on since
    ///   2026-08-13); the bullet gave "non-moving" as its reason until
    ///   2026-09-01, and the reason had expired even though the answer had
    ///   not. The R6 audit note further down this file reaches the same
    ///   finding by reading the two arms rather than the collector.
    pub fn pre_gc_addr_did_not_survive(
        &self,
        addr: usize,
        pointer_map: &cratonvm_types::PointerMap,
    ) -> bool {
        // gch h4/wr: the strict sibling's first question, answered the same
        // way here (the two must not disagree about one address).
        if self.true_root_watched_left_out(addr) {
            return true;
        }
        if pointer_map.contains_key(&addr) {
            return false;
        }
        match &self.inner {
            // "Young and unmapped" is NOT a death certificate. It proves death
            // only for a MOVING young collection, where every survivor gets a
            // `pointer_map` entry. The non-moving sweep keeps survivors in
            // place and produces NO map entries at all, so this arm condemned
            // every live young object the moment the moving collector fell
            // back — and it falls back on every collection in any workload
            // with a live JIT frame it cannot map
            // (`reason=innermost-rbp-belongs-to-unguarded-callee`).
            //
            // What that cost: `process_references_after_gc` skips the enqueue
            // when either the `Reference` or its `ReferenceQueue` "did not
            // survive", so NO reference was ever enqueued — a `WeakReference`
            // was cleared but never delivered, and no `Cleaner` action ever
            // ran. `EnqProbe` reports `gc enqueued it = false` where HotSpot
            // enqueues; H2 then grows without bound, and the UPDATE workload
            // that used to run in `--Xmx 1g` dies with `Out of memory` at 4g.
            //
            // `is_live_young_survivor` is the discriminator built for exactly
            // this (see its soundness argument: STW-window-only, zeroed-span
            // discriminator, moving-collection compatible), and it is already
            // what the strict sibling `watched_pre_gc_addr_survived` uses for
            // its young arm — these two must not disagree about the same
            // address. The conjunction keeps the original verdict everywhere
            // it was right: a genuinely dead young address, and the abandoned
            // old address of an object a moving collection relocated, both
            // still answer "did not survive", which is the `bc math-ec 0x4`
            // protection this predicate exists for.
            VmHeapInner::Generational(h) => {
                h.is_in_young_either(addr as *const u8) && !h.is_live_young_survivor(addr)
            }
            VmHeapInner::G1(_) => !self.is_addr_live(addr),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => !self.is_addr_live(addr),
        }
    }

    /// Walk all live objects in the heap (both generations / all regions).
    /// Must be called during a GC safepoint (all mutator threads paused).
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        dispatch!(&self.inner, walk_objects())
    }

    /// gci i14/hd: [`Self::walk_objects`] as a visitor, the same objects in
    /// the same order under the same safepoint contract. G1 walks its regions
    /// without materialising the list (`G1Collector::for_each_object`: a heap
    /// dump or histogram of a large heap, at an `OutOfMemoryError` too, no
    /// longer allocates 16 bytes per object first); Generational lists only
    /// its young half and visits the old one in slices
    /// (`GenerationalHeap::for_each_object`, gci i14/hd second pass); ZGC
    /// still walks into a list and visits it.
    pub fn for_each_object(&self, f: &mut dyn FnMut(*mut u8, usize)) {
        match &self.inner {
            VmHeapInner::G1(h) => h.for_each_object(f),
            VmHeapInner::Generational(h) => h.for_each_object(f),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => {
                for (ptr, size) in h.walk_objects() {
                    f(ptr, size);
                }
            }
        }
    }

    /// gci i14/hd second pass (item 7): [`Self::for_each_object`] minus the
    /// objects the last trusted completed mark proves dead, on G1
    /// (`G1Collector::for_each_object_skipping_proven_dead`); the number
    /// skipped, or `None` (nothing visited) where no such proof exists -- off
    /// G1 always. The live-only heap walks' opt-in.
    pub fn for_each_object_skipping_proven_dead(
        &self,
        f: &mut dyn FnMut(*mut u8, usize),
    ) -> Option<u64> {
        match &self.inner {
            VmHeapInner::G1(h) => h.for_each_object_skipping_proven_dead(f),
            _ => None,
        }
    }

    /// TEMPORARY diagnostic (CRATONVM_DBG_MIRRORPIN / TestDefaultInstanceManager
    /// investigation): scan every live object in the heap for a reference to
    /// `target_addr`, returning `(holder_addr, holder_class_id)` for each
    /// match. Must be called during a GC safepoint (same contract as
    /// `walk_objects`, which this is built on). O(live objects × avg field
    /// count) — debug-only, never on a hot path. Remove once the
    /// investigation concludes.
    pub fn find_referrers(&self, target_addr: usize) -> Vec<(usize, u32)> {
        let mut out = Vec::new();
        for (obj_ptr, _size) in self.walk_objects() {
            // SAFETY: `walk_objects` yields the start of each live object, so
            // `obj_ptr` targets a valid, fully-initialized `ObjectHeader`.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let mut found = false;
            unsafe {
                cratonvm_gc_gen::gen_heap::for_each_ref_slot(obj_ptr, header, |ref_ptr, _slot| {
                    if ref_ptr as usize == target_addr {
                        found = true;
                    }
                });
            }
            if found {
                out.push((obj_ptr as usize, header.class_id.as_u32()));
            }
        }
        out
    }

    /// JIT round 23 wave 1 L7 (`CRATONVM_DBG=root-kind-census`, the VM's
    /// `memory::root_kind_census`): the object that starts at `addr` -- its
    /// total size and class id -- and every heap reference it holds (its
    /// reference slots, then its external-overlay edges, the two edge families
    /// a collector's mark follows), each passed to `visit`. `None`, and nothing
    /// visited, when `addr` is not an object start ([`Self::is_object_address`])
    /// or its header implies an implausible size. Read-only; must run with the
    /// world stopped (the same contract as [`Self::walk_objects`]).
    pub fn dbg_object_size_and_refs(
        &self,
        addr: usize,
        visit: &mut dyn FnMut(usize),
    ) -> Option<(usize, u32)> {
        let obj = self.is_object_address(addr)?;
        let base = obj.as_ptr();
        // SAFETY: `is_object_address` accepted `addr` as the start of a live
        // object of this heap, so its header bytes are in bounds; the world is
        // stopped (the caller's contract), so nothing rewrites them.
        let header = unsafe { &*(base as *const ObjectHeader) };
        let size = cratonvm_gc_core::gc::object_total_size(header);
        if size < HEADER_SIZE {
            return None;
        }
        // SAFETY: as above; the slot ranges `header` implies are inside the
        // object's extent (`for_each_ref_slot` also caps an array's length).
        unsafe {
            cratonvm_gc_gen::gen_heap::for_each_ref_slot(base, header, |r, _| visit(r as usize));
        }
        let class_id = header.class_id.as_u32();
        cratonvm_gc_core::external_roots::with_external_roots_for_owner(addr, Some(class_id), |refs| {
            for r in refs {
                visit(r.as_ptr() as usize);
            }
        });
        Some((size, class_id))
    }

    /// TEMPORARY diagnostic (doc-26 class-unloading investigation): BFS the
    /// full *referrer* closure of `target_addr` in a SINGLE heap walk, so a
    /// "why is this still alive" question can be answered without one
    /// O(live objects) pass per hop.
    ///
    /// Returns `(depth, addr, class_id, referrer_count)` for every object that
    /// transitively references `target_addr`, breadth-first from the target
    /// (`depth == 0` is the target itself), capped at `max_nodes`. An entry
    /// with `referrer_count == 0` is held by NO heap field — i.e. it is
    /// retained by a GC ROOT, which is exactly the interesting case: the
    /// closure's zero-referrer members enumerate every root that could be
    /// keeping the target alive.
    ///
    /// Same safepoint contract as `walk_objects`. Debug-only.
    pub fn referrer_closure(
        &self,
        target_addr: usize,
        max_nodes: usize,
    ) -> Vec<(usize, usize, u32, usize)> {
        // One walk -> reverse edge map (referent -> [(holder, holder_cid)]).
        let mut reverse: std::collections::HashMap<usize, Vec<(usize, u32)>> =
            std::collections::HashMap::new();
        for (obj_ptr, _size) in self.walk_objects() {
            // SAFETY: `walk_objects` yields the start of each live object.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let holder = (obj_ptr as usize, header.class_id.as_u32());
            unsafe {
                cratonvm_gc_gen::gen_heap::for_each_ref_slot(obj_ptr, header, |ref_ptr, _slot| {
                    if !ref_ptr.is_null() {
                        reverse.entry(ref_ptr as usize).or_default().push(holder);
                    }
                });
            }
        }

        let mut out = Vec::new();
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<(usize, usize, u32)> =
            std::collections::VecDeque::new();
        queue.push_back((0, target_addr, u32::MAX));
        seen.insert(target_addr);
        while let Some((depth, addr, cid)) = queue.pop_front() {
            let referrers = reverse.get(&addr).map_or(0, Vec::len);
            out.push((depth, addr, cid, referrers));
            if out.len() >= max_nodes {
                break;
            }
            if let Some(holders) = reverse.get(&addr) {
                for &(holder, holder_cid) in holders {
                    if seen.insert(holder) {
                        queue.push_back((depth + 1, holder, holder_cid));
                    }
                }
            }
        }
        out
    }

    /// TEMPORARY diagnostic (doc-26 class-unloading investigation): traverse
    /// the FULL referrer closure of `target_addr` and report only its
    /// *root-held* members — objects that no live heap field points at, yet
    /// which are themselves live. Those are precisely the entry points through
    /// which a GC root (or a side-table propagation such as `mirror_pin` /
    /// `loader_pin` / an overlay owner edge) is keeping `target_addr` alive.
    ///
    /// Each returned entry is a path `[root_held, .., target]` of
    /// `(addr, class_id)` pairs, so the retaining chain is readable end to end
    /// instead of having to be reassembled from a flat node dump. At most
    /// `max_paths` paths are returned; the traversal itself is bounded by
    /// `max_nodes` so a pathological heap cannot wedge the diagnostic.
    ///
    /// Same safepoint contract as `walk_objects`. Debug-only.
    pub fn root_held_paths(
        &self,
        target_addr: usize,
        max_nodes: usize,
        max_paths: usize,
    ) -> Vec<Vec<(usize, u32)>> {
        self.retention_paths(target_addr, &|_| false, max_nodes, max_paths)
    }

    /// As [`Self::root_held_paths`], but additionally terminates a branch at
    /// any address the caller's `is_root` predicate accepts — i.e. at a member
    /// of the ACTUAL root vector the collector was handed this cycle.
    ///
    /// This is the query that distinguishes the two ways an object can survive:
    /// a path ending at a root-set member says "a real GC root reaches it, and
    /// this chain is how"; a path ending at a zero-referrer node inside a
    /// strongly-connected component says "nothing outside this component
    /// references it — it was marked through a side-table propagation
    /// (`loader_pin` / `mirror_pin` / `metadata_pin` / an overlay owner edge)".
    /// Chasing the wrong one of those wastes an entire investigation cycle.
    pub fn retention_paths(
        &self,
        target_addr: usize,
        is_root: &dyn Fn(usize) -> bool,
        max_nodes: usize,
        max_paths: usize,
    ) -> Vec<Vec<(usize, u32)>> {
        let mut reverse: std::collections::HashMap<usize, Vec<(usize, u32)>> =
            std::collections::HashMap::new();
        for (obj_ptr, _size) in self.walk_objects() {
            // SAFETY: `walk_objects` yields the start of each live object.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let holder = (obj_ptr as usize, header.class_id.as_u32());
            unsafe {
                cratonvm_gc_gen::gen_heap::for_each_ref_slot(obj_ptr, header, |ref_ptr, _slot| {
                    if !ref_ptr.is_null() {
                        reverse.entry(ref_ptr as usize).or_default().push(holder);
                    }
                });
            }
        }

        // BFS outwards along reverse edges, remembering each node's parent (the
        // object it references) so a discovered root-held node can be walked
        // back down to the target.
        let mut parent: std::collections::HashMap<usize, (usize, u32)> =
            std::collections::HashMap::new();
        let mut cid_of: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        let mut root_held: Vec<usize> = Vec::new();
        queue.push_back(target_addr);
        seen.insert(target_addr);
        let mut visited = 0usize;
        while let Some(addr) = queue.pop_front() {
            visited += 1;
            if visited >= max_nodes {
                break;
            }
            // A root-set member terminates the branch: we have the answer for
            // this chain and walking past it would only find its own referrers.
            if addr != target_addr && is_root(addr) {
                root_held.push(addr);
                continue;
            }
            match reverse.get(&addr) {
                None => root_held.push(addr),
                Some(holders) if holders.is_empty() => root_held.push(addr),
                Some(holders) => {
                    for &(holder, holder_cid) in holders {
                        if seen.insert(holder) {
                            cid_of.insert(holder, holder_cid);
                            parent.insert(holder, (addr, holder_cid));
                            queue.push_back(holder);
                        }
                    }
                }
            }
        }

        let mut paths = Vec::new();
        for node in root_held.into_iter().take(max_paths) {
            let mut path = vec![(node, cid_of.get(&node).copied().unwrap_or(u32::MAX))];
            let mut cur = node;
            // Walk parent links down to the target (bounded by `seen`'s size).
            while let Some(&(next, _)) = parent.get(&cur) {
                path.push((next, cid_of.get(&next).copied().unwrap_or(u32::MAX)));
                cur = next;
                if cur == target_addr || path.len() > 64 {
                    break;
                }
            }
            paths.push(path);
        }
        paths
    }

    /// TEMPORARY diagnostic (is_live_young_survivor false-positive
    /// investigation): scan every live object in the heap for one whose
    /// `class_id` matches `target_class_id`, returning its address. Used to
    /// tell apart "a live instance of the evicted class genuinely still
    /// exists somewhere (loader_pin correctly keeps its loader alive)" from
    /// "nothing legitimate justifies the loader being marked alive." Must be
    /// called during a GC safepoint (same contract as `walk_objects`).
    pub fn find_instances_of_class(&self, target_class_id: u32) -> Vec<usize> {
        let mut out = Vec::new();
        for (obj_ptr, _size) in self.walk_objects() {
            // SAFETY: `walk_objects` yields the start of each live object, so
            // `obj_ptr` targets a valid, fully-initialized `ObjectHeader`.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if header.class_id.as_u32() == target_class_id {
                out.push(obj_ptr as usize);
            }
        }
        out
    }
}
impl VmHeap {
    /// Constructs a `VmHeap` wrapping a `GenerationalHeap` without exposing the
    /// internal `VmHeapInner` enum outside `cratonvm-gc`.
    pub fn from_generational(heap: GenerationalHeap) -> Self {
        Self {
            inner: VmHeapInner::Generational(heap),
        }
    }

    /// Constructs a `VmHeap` wrapping a shared `ZgcRealHeap` without exposing
    /// `VmHeapInner` outside `cratonvm-gc`.
    #[cfg(feature = "zgc")]
    pub fn from_zgc(collector: Arc<ZgcRealHeap>) -> Self {
        Self::from_zgc_arc(collector)
    }

    /// Constructs a test/standalone `VmHeap` backed by ZGC with `capacity` bytes.
    #[cfg(feature = "zgc")]
    pub fn from_zgc_capacity(capacity: usize) -> Self {
        Self::from_zgc_arc(Arc::new(ZgcRealHeap::with_capacity(capacity)))
    }

    #[cfg(feature = "zgc")]
    pub(in crate::vm_heap) fn from_zgc_arc(collector: Arc<ZgcRealHeap>) -> Self {
        Self {
            inner: VmHeapInner::Zgc(collector),
        }
    }

    pub fn g1_published_jit_pin_census(&self) -> (usize, usize) {
        match &self.inner {
            VmHeapInner::G1(state) => state.collector.published_jit_pin_census(),
            _ => (0, 0),
        }
    }

    pub fn g1_region_shape_map(&self) -> String {
        match &self.inner {
            VmHeapInner::G1(state) => state.collector.region_shape_map(),
            _ => String::new(),
        }
    }

    pub fn g1_set_survivor_ratio(&self, survivor_ratio: Option<u64>) {
        if let VmHeapInner::G1(state) = &self.inner {
            state.collector.set_survivor_ratio(survivor_ratio);
        }
    }

    pub fn exit_summary_arenas(&self) -> [(usize, usize, usize, usize); 3] {
        match &self.inner {
            VmHeapInner::Generational(h) => h.exit_summary_arenas(),
            _ => [(0, 0, 0, 0); 3],
        }
    }

    pub fn reserved_span(&self) -> (usize, usize) {
        match &self.inner {
            VmHeapInner::G1(state) => state.collector.reserved_span(),
            _ => (0, 0),
        }
    }

    pub fn reserved_bytes(&self) -> usize {
        match &self.inner {
            VmHeapInner::G1(state) => state.collector.reserved_bytes(),
            _ => self.heap_capacity(),
        }
    }

    pub fn copy_reference_slots_colored(
        &self,
        src_base: usize,
        src_off: usize,
        dst_base: usize,
        dst_off: usize,
        len: usize,
        backward: bool,
    ) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.copy_reference_slots_colored(
                src_base, src_off, dst_base, dst_off, len, backward,
            );
        }
        let _ = (src_base, src_off, dst_base, dst_off, len, backward);
        false
    }

    pub fn layout_domain(&self) -> u32 {
        match &self.inner {
            VmHeapInner::Generational(h) => h.layout_domain(),
            VmHeapInner::G1(state) => state.collector.layout_domain(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(z) => z.layout_domain(),
        }
    }

    pub fn vm_tlab_enabled(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.vm_tlab_enabled();
        }
        false
    }

    pub fn vm_tlab_engagement(&self) -> (usize, usize, usize, usize) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.vm_tlab_engagement();
        }
        (0, 0, 0, 0)
    }

    pub fn g1_gc_beans(&self) -> Option<&Arc<cratonvm_gc_core::gc_metrics::BackendGcBeans>> {
        match &self.inner {
            VmHeapInner::G1(state) => Some(state.gc_beans()),
            _ => None,
        }
    }

    #[cfg(feature = "zgc")]
    pub fn mark_end_service_begin(&self) -> Option<cratonvm_gc_zgc::zgc::ZMarkEndTicket> {
        match &self.inner {
            VmHeapInner::Zgc(state) => state.mark_end_service_begin(),
            _ => None,
        }
    }

    #[cfg(feature = "zgc")]
    pub fn mark_end_service_still_due(&self, ticket: cratonvm_gc_zgc::zgc::ZMarkEndTicket) -> bool {
        match &self.inner {
            VmHeapInner::Zgc(state) => state.mark_end_service_still_due(ticket),
            _ => false,
        }
    }

    #[cfg(feature = "zgc")]
    pub fn mark_end_service_end(
        &self,
        ticket: cratonvm_gc_zgc::zgc::ZMarkEndTicket,
        paused: Option<bool>,
        continued_here: bool,
    ) {
        if let VmHeapInner::Zgc(state) = &self.inner {
            state
                .mark_end_service_end(ticket, paused, continued_here);
        }
    }

    #[cfg(feature = "zgc")]
    pub fn try_mark_end(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &[ObjectRef],
    ) -> cratonvm_gc_zgc::zgc::ZMarkEndAttempt {
        match &self.inner {
            VmHeapInner::Zgc(state) => state.try_mark_end(stw, roots),
            _ => cratonvm_gc_zgc::zgc::ZMarkEndAttempt::Complete,
        }
    }

    #[cfg(feature = "zgc")]
    pub fn request_tick_cycle(&self, cause: cratonvm_gc_zgc::zgc::timer::ZTickCause) {
        if let VmHeapInner::Zgc(state) = &self.inner {
            state.request_tick_cycle(cause);
        }
    }

    #[cfg(feature = "zgc")]
    pub fn withdraw_tick_cycle_if_answered(
        &self,
        cause: cratonvm_gc_zgc::zgc::timer::ZTickCause,
        now: std::time::Instant,
    ) -> bool {
        match &self.inner {
            VmHeapInner::Zgc(state) => state.withdraw_tick_cycle_if_answered(cause, now),
            _ => false,
        }
    }

    #[cfg(feature = "zgc")]
    pub fn withdraw_tick_cycle_on_stop(&self, cause: cratonvm_gc_zgc::zgc::timer::ZTickCause) -> bool {
        match &self.inner {
            VmHeapInner::Zgc(state) => state.withdraw_tick_cycle_on_stop(cause),
            _ => false,
        }
    }

    #[cfg(feature = "zgc")]
    pub fn tick_pause_guard(&self) -> Option<cratonvm_gc_zgc::zgc::timer::TickPauseInFlight<'_>> {
        match &self.inner {
            VmHeapInner::Zgc(state) => Some(state.tick_pause_guard()),
            _ => None,
        }
    }

    /// Returns whether this heap is using the ZGC collector.
    /// Returns the collector-agnostic JIT barrier description for this heap.
    pub fn barrier_set(&self) -> cratonvm_types::BarrierSet {
        match &self.inner {
            VmHeapInner::Generational(_) => cratonvm_types::BarrierSet::generational(),
            VmHeapInner::G1(_) => cratonvm_types::BarrierSet::g1(),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => cratonvm_types::BarrierSet::zgc(),
        }
    }

    pub fn is_zgc(&self) -> bool {
        #[cfg(feature = "zgc")]
        {
            matches!(&self.inner, VmHeapInner::Zgc(_))
        }
        #[cfg(not(feature = "zgc"))]
        {
            false
        }
    }

    /// HotSpot unified-logging collector name (`"Serial"`, `"G1"`, or `"ZGC"`).
    pub fn collector_log_name(&self) -> &'static str {
        match &self.inner {
            VmHeapInner::Generational(_) => "Serial",
            VmHeapInner::G1(_) => "G1",
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => "ZGC",
        }
    }

    /// Young and old GarbageCollectorMXBean names for this collector.
    pub fn gc_bean_names(&self) -> (&'static str, &'static str) {
        match &self.inner {
            VmHeapInner::Generational(_) => ("DefNew", "SerialOld"),
            VmHeapInner::G1(_) => ("G1New", "G1Old"),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(_) => ("ZGC Minor", "ZGC Major"),
        }
    }

    pub fn mark_active(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.mark_active();
        }
        false
    }

    pub fn load_barrier_armed(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.load_barrier_armed();
        }
        false
    }

    pub fn contains_object_address(&self, addr: usize) -> bool {
        match &self.inner {
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(z) => z.contains_object_address(addr),
            _ => self.is_object_address(addr).is_some(),
        }
    }

    pub fn parallel_worker_count(&self) -> Option<usize> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.parallel_worker_count()),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(z) => z.gc_parallel_threads(),
            _ => None,
        }
    }

    pub fn region_census(&self) -> Option<cratonvm_gc_g1::g1::G1RegionCensus> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.region_census()),
            _ => None,
        }
    }

    pub fn g1_parked_mirror_keyed_view(&self, with_ranges: bool) -> (bool, Vec<(usize, usize)>) {
        match &self.inner {
            VmHeapInner::G1(h) => h.parked_mirror_keyed_view(with_ranges),
            _ => (false, Vec::new()),
        }
    }

    pub fn g1_free_region_count(&self) -> Option<usize> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.free_region_count()),
            _ => None,
        }
    }

    pub fn g1_band_reject_pin(&self, addr: usize) -> Option<cratonvm_gc_g1::g1::BandRejectPin> {
        match &self.inner {
            VmHeapInner::G1(h) => h.band_reject_pin(addr),
            _ => None,
        }
    }

    pub fn g1_jit_edge_filter_addr(&self) -> usize {
        match &self.inner {
            VmHeapInner::G1(h) => h.jit_edge_filter_addr(),
            _ => 0,
        }
    }

    pub fn g1_unpin_region(&self, region_idx: usize) {
        if let VmHeapInner::G1(h) = &self.inner {
            h.unpin_region(region_idx);
        }
    }

    pub fn g1_is_humongous_request(&self, bytes: usize) -> bool {
        match &self.inner {
            VmHeapInner::G1(h) => h.is_humongous_request(bytes),
            _ => false,
        }
    }

    pub fn g1_last_pause_end(&self) -> Option<std::time::Instant> {
        match &self.inner {
            VmHeapInner::G1(h) => h.last_pause_end(),
            _ => None,
        }
    }

    pub fn g1_full_compaction_pause_count(&self) -> Option<u64> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.full_compaction_pause_count()),
            _ => None,
        }
    }

    pub fn g1_humongous_alloc_wants_concurrent_start(&self, bytes: usize) -> bool {
        match &self.inner {
            VmHeapInner::G1(h) => h.humongous_alloc_wants_concurrent_start(bytes),
            _ => false,
        }
    }

    pub fn g1_layout_census_counts(&self) -> (u64, u64) {
        match &self.inner {
            VmHeapInner::G1(h) => h.layout_census_counts(),
            _ => (0, 0),
        }
    }

    pub fn g1_layout_census_due(&self, retained_total: u64) -> bool {
        match &self.inner {
            VmHeapInner::G1(h) => h.layout_census_due(retained_total),
            _ => false,
        }
    }

    pub fn g1_object_class_ids(&self) -> Option<(rustc_hash::FxHashSet<u32>, bool)> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.object_class_ids()),
            _ => None,
        }
    }

    pub fn g1_resurrect_after_remark(&self, addrs: &[usize]) {
        if let VmHeapInner::G1(h) = &self.inner {
            h.resurrect_after_remark(addrs);
        }
    }

    pub fn g1_trusted_mark_liveness(&self) -> Option<cratonvm_gc_g1::g1::RemarkLiveness<'_>> {
        match &self.inner {
            VmHeapInner::G1(h) => h.trusted_mark_liveness(),
            _ => None,
        }
    }

    pub fn g1_mixed_collection_count(&self) -> Option<u64> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.mixed_collection_count()),
            _ => None,
        }
    }

    pub fn g1_prepare_mixed_pause_count(&self) -> Option<u64> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.prepare_mixed_pause_count()),
            _ => None,
        }
    }

    pub fn g1_any_young_or_humongous_addr(&self, addrs: &[usize]) -> bool {
        match &self.inner {
            VmHeapInner::G1(h) => h.any_young_or_humongous_addr(addrs),
            _ => false,
        }
    }

    pub fn g1_survivor_ratio(&self) -> Option<usize> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.survivor_ratio()),
            _ => None,
        }
    }

    pub fn g1_young_target_regions(&self) -> Option<usize> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.young_target_regions()),
            _ => None,
        }
    }

    pub fn g1_would_start_marking(&self) -> bool {
        match &self.inner {
            VmHeapInner::G1(h) => h.would_start_marking(),
            _ => false,
        }
    }

    pub fn g1_set_young_gen_sizing(&self, req: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing) -> Vec<String> {
        match &self.inner {
            VmHeapInner::G1(h) => h.set_young_gen_sizing(req),
            _ => Vec::new(),
        }
    }

    pub fn g1_raw_ref_array_copy_admitted(&self, src_total: usize, dst_total: usize) -> bool {
        match &self.inner {
            VmHeapInner::G1(h) => h.collector.raw_ref_array_copy_admitted(src_total, dst_total),
            _ => false,
        }
    }

    pub fn g1_post_write_barrier_rset(&self, src_obj: ObjectRef, stored_ref: ObjectRef) {
        if let VmHeapInner::G1(h) = &self.inner {
            h.collector.post_write_barrier_rset(src_obj, stored_ref);
        }
    }

    pub fn g1_young_collection(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) {
        if let VmHeapInner::G1(h) = &self.inner {
            h.young_collection(roots, monitors);
        }
    }

    pub fn zgc_set_always_pre_touch(&self, v: bool) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.set_always_pre_touch(v);
        }
        false
    }

    pub fn zgc_set_soft_max_heap_bytes(&self, bytes: usize) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_soft_max_heap_bytes(bytes);
        }
    }

    pub fn zgc_set_ergonomic_soft_max_heap_bytes(&self, bytes: usize) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_ergonomic_soft_max_heap_bytes(bytes);
        }
    }

    pub fn zgc_set_director_proactive(&self, v: bool) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_director_proactive(v);
        }
    }

    pub fn zgc_set_director_allocation_spike_tolerance(&self, v: f64) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_director_allocation_spike_tolerance(v);
        }
    }

    #[cfg(feature = "zgc")]
    pub fn zgc_set_collection_interval(&self, v: Option<std::time::Duration>) {
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_collection_interval(v);
        }
    }

    #[cfg(feature = "zgc")]
    pub fn zgc_set_collection_interval_minor(&self, v: Option<std::time::Duration>) {
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_collection_interval_minor(v);
        }
    }

    pub fn zgc_set_collection_interval_only(&self, v: bool) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_collection_interval_only(v);
        }
    }

    pub fn zgc_set_gc_worker_threads(
        &self,
        parallel: Option<usize>,
        conc: Option<usize>,
        dynamic: Option<bool>,
    ) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_gc_worker_threads(parallel, conc, dynamic);
        }
    }

    pub fn zgc_set_exhaustion_unwind(&self, f: fn(usize, &'static str)) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_exhaustion_unwind(f);
        }
    }

    pub fn g1_take_tenuring_xlog(&self) -> Option<(u64, u8, u8, Vec<u64>)> {
        match &self.inner {
            VmHeapInner::G1(h) => h.take_tenuring_xlog().map(|(a, b, c, d)| (a, b, c, d.to_vec())),
            _ => None,
        }
    }

    pub fn g1_young_holder_skips_post_barrier(&self, obj: ObjectRef) -> bool {
        match &self.inner {
            VmHeapInner::G1(h) => h.young_holder_skips_post_barrier(obj),
            _ => false,
        }
    }

    pub fn g1_pin_array_payload_for_critical(
        &self,
        obj_addr: usize,
        offset: usize,
        bytes: usize,
    ) -> Option<usize> {
        match &self.inner {
            VmHeapInner::G1(h) => h.pin_array_payload_for_critical(obj_addr, offset, bytes),
            _ => None,
        }
    }

    pub fn gen_jit_ref_store_plan(&self) -> Option<(usize, usize, usize, u8)> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.jit_ref_store_plan()),
            _ => None,
        }
    }

    pub fn zgc_arm_load_barrier_plain_stores(&self, flag: bool) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.arm_load_barrier_plain_stores(flag);
        }
        false
    }

    /// Arm colored stores only while the heap's mutators are stopped.
    #[cfg(feature = "zgc")]
    pub fn zgc_arm_load_barrier_colored_stores(&self) -> bool {
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.arm_load_barrier_colored_stores();
        }
        false
    }

    #[cfg(feature = "zgc")]
    pub fn zgc_set_barrier_color(&self, color: Option<cratonvm_gc_zgc::zgc::vaddr::ZColor>) {
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_barrier_color(color);
        }
    }

    pub fn zgc_committed_span_containing(&self, addr: usize) -> Option<(usize, usize)> {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.committed_span_containing(addr);
        }
        None
    }

    pub fn zgc_committed_arena_spans(&self) -> Vec<(usize, usize)> {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.committed_arena_spans();
        }
        Vec::new()
    }

    pub fn zgc_os_committed_bytes(&self) -> usize {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.os_committed_bytes();
        }
        0
    }

    pub fn zgc_set_generational_enabled(&self, enabled: bool) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_generational_enabled(enabled);
        }
    }

    pub fn gen_old_gen_sorted_object_bases(&self) -> Vec<usize> {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                let (objects, _gaps) = h.old_gen_lock().walk_objects_with_gaps();
                let mut bases: Vec<usize> = objects.iter().map(|&(p, _)| p as usize).collect();
                bases.sort_unstable();
                bases
            }
            _ => Vec::new(),
        }
    }

    pub fn gen_forward_refusal_reason(&self, addr: usize) -> Option<&'static str> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.forward_refusal_reason(addr),
            _ => None,
        }
    }

    pub fn gen_jmx_collectors(&self) -> Option<[cratonvm_gc_core::gc_metrics::JmxCollector; 2]> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.jmx_collectors()),
            _ => None,
        }
    }

    pub fn gen_jmx_memory_pools(&self) -> Option<[cratonvm_gc_core::gc_metrics::JmxPoolUsage; 3]> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.jmx_memory_pools()),
            _ => None,
        }
    }

    pub fn gen_jmx_collection_committed(&self) -> Option<[Option<u64>; 3]> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.jmx_collection_committed()),
            _ => None,
        }
    }

    pub fn gen_gc_notifications(&self) -> Option<&cratonvm_gc_core::gc_metrics::GcNotificationQueue> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.gc_notifications()),
            _ => None,
        }
    }

    pub fn gen_write_barrier_ref_array_element(&self, array: ObjectRef, index: usize, stored_value: Value) {
        if let VmHeapInner::Generational(h) = &self.inner {
            h.write_barrier_ref_array_element(array, index, stored_value);
        }
    }

    pub fn gen_set_resolved_compact_ref_field(&self, obj_ref: ObjectRef, index: usize, off: usize, value: Value) {
        if let VmHeapInner::Generational(h) = &self.inner {
            h.set_resolved_compact_ref_field(obj_ref, index, off, value);
        }
    }

    pub fn gen_mark_next_young_cycle_forced(&self, forced: bool) {
        if let VmHeapInner::Generational(h) = &self.inner {
            h.mark_next_young_cycle_forced(forced);
        }
    }

    pub fn gen_note_oome_ladder(&self, slot: usize, n: u64) {
        if let VmHeapInner::Generational(h) = &self.inner {
            h.note_oome_ladder(slot, n);
        }
    }

    pub fn gen_young_exhausted_refill_wants_collection(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.young_exhausted_refill_wants_collection(),
            _ => false,
        }
    }

    pub fn gen_tlab_tail_sink_enabled(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.tlab_tail_sink_enabled(),
            _ => false,
        }
    }

    pub fn gen_collect_young_to_old_seeds(
        &self,
        req: &cratonvm_gc_gen::gen_heap::YoungToOldSeedRequest<'_>,
    ) -> Option<cratonvm_gc_gen::gen_heap::YoungToOldSeeds> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.collect_young_to_old_seeds(req)),
            _ => None,
        }
    }

    pub fn gen_walk_young_objects(&self) -> Option<Vec<(*mut u8, usize)>> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.walk_young_objects()),
            _ => None,
        }
    }

    pub fn gen_young_reached_only_through(
        &self,
        roots: &[usize],
        extra_live: &[usize],
        strong_holder: &dyn Fn(usize) -> bool,
        through: &[usize],
    ) -> Option<Result<Vec<usize>, cratonvm_gc_gen::gen_heap::Y2oFallback>> {
        match &self.inner {
            VmHeapInner::Generational(h) => {
                Some(h.young_reached_only_through(roots, extra_live, strong_holder, through))
            }
            _ => None,
        }
    }

    pub fn gen_young_object_class_ids(&self) -> Option<(rustc_hash::FxHashSet<u32>, bool)> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.young_object_class_ids()),
            _ => None,
        }
    }

    pub fn gen_old_object_class_ids(&self) -> Option<(rustc_hash::FxHashSet<u32>, bool)> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.old_object_class_ids()),
            _ => None,
        }
    }

    pub fn gen_set_tenuring_xlog(&self, enabled: bool) {
        if let VmHeapInner::Generational(h) = &self.inner {
            h.set_tenuring_xlog(enabled);
        }
    }

    pub fn gen_take_tenuring_xlog(&self) -> Option<(u64, u8, u8, Vec<u64>)> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.take_tenuring_xlog(),
            _ => None,
        }
    }

    pub fn gen_take_escalation_split(&self) -> Option<cratonvm_gc_gen::gen_heap::EscalationSplit> {
        match &self.inner {
            VmHeapInner::Generational(h) => h.take_escalation_split(),
            _ => None,
        }
    }

    pub fn g1_satb_queue(&self) -> Option<&cratonvm_gc_core::satb::SatbQueue> {
        match &self.inner {
            VmHeapInner::G1(h) => Some(h.satb_queue()),
            _ => None,
        }
    }

    pub fn zgc_set_mark_active(&self, active: bool) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_mark_active(active);
        }
    }

    pub fn zgc_drain_mark_ingress(&self, out: &mut Vec<u64>) -> usize {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.drain_mark_ingress(out);
        }
        0
    }

    pub fn zgc_set_vm_tlab_enabled(&self, enabled: bool) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_vm_tlab_enabled(enabled);
        }
    }

    pub fn gen_oome_ladder_census(&self) -> Option<[u64; cratonvm_gc_gen::gen_heap::oome_ladder::LEN]> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.oome_ladder_census()),
            _ => None,
        }
    }

    pub fn gen_verbose_gc_enabled(&self) -> Option<bool> {
        match &self.inner {
            VmHeapInner::Generational(h) => Some(h.verbose_gc_enabled()),
            _ => None,
        }
    }

    pub fn gen_satb_active(&self) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.satb_queue_ref().is_some_and(|q| q.is_active()),
            _ => false,
        }
    }

    pub fn gen_set_survivor_ratio(
        &mut self,
        survivor_ratio: Option<u64>,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
    ) {
        if let VmHeapInner::Generational(h) = &mut self.inner {
            h.set_survivor_ratio(survivor_ratio, young);
        }
    }

    pub fn gen_set_min_tlab_floor(&mut self, typed_min_tlab_size: usize) {
        if let VmHeapInner::Generational(h) = &mut self.inner {
            h.set_min_tlab_floor(typed_min_tlab_size);
        }
    }

    pub fn zgc_jit_ref_store_plan(&self) -> Option<(usize, usize, usize, u8)> {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return Some(z.jit_ref_store_plan());
        }
        None
    }

    pub fn zgc_note_ref_store_element(&self, obj_addr: usize, slot_addr: usize, len: usize) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.note_ref_store_element(obj_addr, slot_addr, len);
        }
    }

    pub fn is_conc_cycle_active(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.is_conc_cycle_active();
        }
        false
    }

    pub fn conc_sweep_pending(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.conc_sweep_pending();
        }
        false
    }

    pub fn try_arm_tick_concurrent_mark(
        &self,
        #[allow(unused_variables)] cause: cratonvm_gc_zgc::zgc::timer::ZTickCause,
    ) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.try_arm_tick_concurrent_mark(cause);
        }
        false
    }

    pub fn zgc_set_resolved_compact_ref_field(
        &self,
        obj: ObjectRef,
        index: usize,
        offset: usize,
        storage: cratonvm_types::FieldStorageKind,
        value: Value,
    ) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_resolved_compact_ref_field(obj, index, offset, storage, value);
            return;
        }
        self.set_field(obj, index, value);
    }

    pub fn zgc_mark_active(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.mark_active();
        }
        false
    }

    pub fn zgc_satb_pre_barrier(&self, old_addr: usize) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.satb_pre_barrier(old_addr);
        }
    }

    pub fn zgc_note_ref_store(&self, obj_addr: usize) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.note_ref_store(obj_addr);
        }
    }

    pub fn zgc_vm_pause_cpu_start(&self) -> Option<(u64, u64)> {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.vm_pause_cpu_start();
        }
        None
    }

    pub fn zgc_book_vm_pause_cpu(&self, start: (u64, u64)) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.book_vm_pause_cpu(start);
        }
    }

    pub fn zgc_director_tick_wanted(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.director_tick_wanted();
        }
        false
    }

    pub fn zgc_tick_pause_in_flight(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.tick_pause_in_flight();
        }
        false
    }

    pub fn zgc_stop_director_tick(&self) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.stop_director_tick();
        }
    }

    #[cfg(feature = "zgc")]
    pub fn zgc_director_tick(&self, now: std::time::Instant) -> Option<cratonvm_gc_zgc::zgc::timer::ZTick> {
        if let VmHeapInner::Zgc(z) = &self.inner {
            return Some(z.director_tick(now));
        }
        None
    }

    pub fn zgc_director_tick_stopped(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.director_tick_stopped();
        }
        false
    }

    pub fn zgc_mark_continue_wanted(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.mark_continue_wanted();
        }
        false
    }

    pub fn zgc_mark_continue_released(&self, pause_ns: u64) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.mark_continue_released(pause_ns);
        }
    }

    #[cfg(feature = "zgc")]
    pub fn zgc_take_hugetlb_refusal_report(&self) -> Option<cratonvm_gc_zgc::zgc::ZgcHugetlbRefusalReport> {
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.take_hugetlb_refusal_report();
        }
        None
    }

    pub fn zgc_take_mark_continue_log(&self) -> Vec<(u64, u64)> {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.take_mark_continue_log();
        }
        Vec::new()
    }

    pub fn zgc_take_last_concurrent_mark_nanos(&self) -> u64 {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.take_last_concurrent_mark_nanos();
        }
        0
    }

    pub fn zgc_explicit_uncommit_unsupported(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.explicit_uncommit_unsupported();
        }
        false
    }

    pub fn zgc_place_jit_ref_store_gates(&self, cell: &'static cratonvm_gc_gen::gen_heap::JitRefStoreGates) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.place_jit_ref_store_gates(cell);
        }
        false
    }

    pub fn zgc_set_uncommit_options(&self, enabled: bool, delay_secs: u64) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_uncommit_options(enabled, delay_secs);
        }
    }

    pub fn zgc_set_uncommit_floor(&self, bytes: usize) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.set_uncommit_floor(bytes);
        }
    }

    pub fn zgc_advise_transparent_huge_pages(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.advise_transparent_huge_pages();
        }
        false
    }

    pub fn zgc_explicit_huge_pages(&self) -> bool {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.explicit_huge_pages();
        }
        false
    }

    pub fn zgc_explicit_initial_commit(&self) -> Option<(usize, bool)> {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            return z.explicit_initial_commit();
        }
        None
    }

    #[cfg(feature = "zgc")]
    pub fn zgc_uncommitter_starter(
        &self,
        log: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
        jfr: Option<cratonvm_gc_zgc::zgc::ZUncommitJfrSink>,
    ) -> Option<Box<dyn FnOnce() + Send>> {
        if let VmHeapInner::Zgc(z) = &self.inner {
            let zgc = std::sync::Arc::clone(z);
            return Some(Box::new(move || {
                let _ = zgc.start_uncommitter(log, jfr);
            }));
        }
        None
    }

    #[cfg(feature = "zgc")]
    pub fn zgc_prefaulter_starter(&self) -> Option<Box<dyn FnOnce() + Send>> {
        if let VmHeapInner::Zgc(z) = &self.inner {
            let zgc = std::sync::Arc::clone(z);
            return Some(Box::new(move || {
                let _ = zgc.start_prefaulter();
            }));
        }
        None
    }
}


/// Render the `[GC] tlab-guard:` line for the shutdown summary.
///
/// `line` is [`cratonvm_gc_core::tlab::tlab_census_lines`]'s second element; `urgent` is
/// [`cratonvm_gc_core::tlab::tlab_guard_line_is_urgent`], i.e. "at least one tripwire
/// fired".
///
/// # Why the two cases are rendered differently
///
/// `gengc-alloc-tlab-instruments-are-write-only-FIXED-20260924.md` filed these two
/// counters as write-only, and the reason a write-only detector is worse than
/// no detector is that its silence reads as a green light. Printing the line
/// at all fixes half of that; the other half is that a fired tripwire must not
/// look like one more gauge in a block of thirty gauges. So:
///
/// * not urgent — the bare `key=value` line, which is the "the tripwire says
///   no" reading that a missing line cannot give you;
/// * urgent — the SAME `key=value` prefix (so a grep for
///   `filler_over_object=` keeps working either way) plus a suffix naming the
///   hazard.
///
/// Split out of `print_gc_summary` so the shape is testable without a heap and
/// without depending on the process-wide counters, which `cargo test` races.
pub(in crate::vm_heap) fn render_tlab_guard_line(line: &str, urgent: bool) -> String {
    if !urgent {
        return line.to_string();
    }
    format!(
        "{line} — TRIPWIRE FIRED: a TLAB tail was filled over a word that \
         still held an object (`filler_over_object`), or a refill handed a \
         thread a chunk that already held one (`refill_over_object`). Either \
         way a live object is buried and will be read back as an all-zero \
         header. This is a use-after-free in waiting, not tuning — the \
         per-event detail is on `tracing` target `cratonvm::gc::guard` (the \
         first eight occurrences and then powers of two only, so this count is \
         the authoritative total); see \
         docs/feature-designs/generative-gc-architecture.md"
    )
}

pub use cratonvm_gc_core::gc_metrics::pause_sum_as_collection_time_ms;



impl cratonvm_gc_core::tlab::TlabTailSink for VmHeap {
    fn reclaim_tlab_tail(&self, start: usize, end: usize) -> bool {
        match &self.inner {
            VmHeapInner::Generational(h) => h.reclaim_tlab_tail(start, end),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(z) => z.reclaim_tlab_tail(start, end),
            _ => false,
        }
    }

    fn note_retired_span(&self, start: usize, cursor: usize, end: usize) {
        #[cfg(feature = "zgc")]
        if let VmHeapInner::Zgc(z) = &self.inner {
            z.note_retired_span(start, cursor, end);
        }
    }
}

