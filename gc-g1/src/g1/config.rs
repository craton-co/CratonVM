#[allow(unused_imports)]
use super::*;

impl<'a> SharedEvac<'a> {
    /// The region table as the shared screens want it. See [`RegionView`].
    #[inline]
    pub fn view(&self) -> RegionView<'_> {
        RegionView::Raw(self.regions_base, self.regions_len)
    }

    /// Is the serial arm's header-screen parity armed on this arm?
    /// `CRATONVM_G1_PARALLEL_EVAC_SCREEN=0` stands it down, which is the
    /// same-binary A/B for the crash clusters it was added for.
    ///
    /// Reads the per-pause snapshot rather than `gc_flags()`; see the
    /// [`Self::screens`] field for why.
    #[inline]
    pub fn screens_armed(&self) -> bool {
        self.screens
    }

    /// `declared`, clamped to the holder's region when the screens are armed.
    #[inline]
    pub fn walkable(&self, obj_ptr: *mut u8, declared: usize, stride: usize) -> usize {
        if !self.screens_armed() {
            return declared;
        }
        self.collector.holder_walkable_slots_view(
            self.view(),
            obj_ptr,
            declared,
            stride,
            HolderBound::RegionEnd,
        )
    }

    /// May this CSet-resident candidate be dereferenced and evacuated?
    #[inline]
    pub fn candidate_ok(&self, site: &'static str, holder: *mut u8, slot: usize, raw: usize) -> bool {
        self.candidate_screen(site, holder, slot, raw).0
    }

    /// [`Self::candidate_ok`], and whether it proved what [`Self::evacuate`]'s
    /// supply screen asks (see
    /// `G1Collector::evacuation_candidate_screen_view`); `false` with the
    /// screens off, which prove nothing.
    #[inline]
    pub fn candidate_screen(
        &self,
        site: &'static str,
        holder: *mut u8,
        slot: usize,
        raw: usize,
    ) -> (bool, bool) {
        if !self.screens_armed() {
            return (true, false);
        }
        self.collector
            .evacuation_candidate_screen_view(self.view(), site, holder, slot, raw)
    }

    /// The parallel driver's copy of the serial arms' root guard: is this
    /// CSet-resident root the start of a live object, and if not, count the
    /// skip. Returns `true` when the root may be followed.
    ///
    /// The serial sites also PIN nothing (see the long comment there: the JNI
    /// `pinned` flag is a different vocabulary with a different clearing rule),
    /// so leaving the root untouched is the whole action here too.
    pub fn note_root_object_plausibility_or_skip(&self, old_ptr: *mut u8) -> bool {
        if !self.screens_armed() {
            return true;
        }
        if self
            .collector
            .note_root_object_plausibility_view(self.view(), old_ptr as usize)
        {
            return true;
        }
        let n = NON_OBJECT_ROOT_SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::warn!(
                "[g1] a NON-OBJECT root was SKIPPED by the PARALLEL evacuator (#{n}): 0x{:x} — it is not the start of a live object, so evacuating it would have sized an object from bytes that are not a header. The root is left unchanged.",
                old_ptr as usize,
            );
        }
        false
    }

    pub fn append_self_forwarded_from_forwards(
        forwards: &[(usize, usize)],
        deferred_self_forwarded: &mut Vec<usize>,
    ) {
        let mut seen = std::collections::HashSet::new();
        let mut forwards_from = 0usize;
        let mut deferred_from = 0usize;
        Self::append_self_forwarded_from_forwards_incremental(
            forwards,
            &mut forwards_from,
            &mut deferred_from,
            &mut seen,
            deferred_self_forwarded,
        );
    }

    /// The re-entrant form of [`Self::append_self_forwarded_from_forwards`],
    /// for the drain FIXPOINT in `parallel_evacuate`.
    ///
    /// The one-shot form above rebuilds its `seen` set from the whole deferred
    /// list and rescans the whole forward list on every call, and the fixpoint
    /// calls it once per drain round while both lists only ever GROW — so a
    /// pause that needed `k` rounds paid `O(k * n)` hashing on top of the drain
    /// it was already doing. That is the wrong cost curve for the one path a
    /// pause takes when it is already in trouble: rounds and `n` both rise with
    /// evacuation-failure pressure, so the bookkeeping grew quadratically in
    /// exactly the pauses that could least afford it.
    ///
    /// The two cursors carry the scan position across rounds; `seen` carries
    /// the membership test. Absorbing the deferred list's new tail FIRST is
    /// what keeps this exactly equivalent to the one-shot form: the drain
    /// pushes its own freshly-scanned children straight into `deferred`
    /// without going through this function, and the one-shot form saw them
    /// because it rebuilt `seen` from the whole vector.
    pub fn append_self_forwarded_from_forwards_incremental(
        forwards: &[(usize, usize)],
        forwards_from: &mut usize,
        deferred_from: &mut usize,
        seen: &mut std::collections::HashSet<usize>,
        deferred_self_forwarded: &mut Vec<usize>,
    ) {
        while *deferred_from < deferred_self_forwarded.len() {
            seen.insert(deferred_self_forwarded[*deferred_from]);
            *deferred_from += 1;
        }
        while *forwards_from < forwards.len() {
            let (old, new) = forwards[*forwards_from];
            *forwards_from += 1;
            if old == new && seen.insert(old) {
                deferred_self_forwarded.push(old);
                // Already in `seen`, so the absorb loop above must not be
                // asked to re-insert it on the next round.
                *deferred_from = deferred_self_forwarded.len();
            }
        }
    }

    /// g1sz (`CRATONVM_G1_EVAC_PREFETCH`, opt-in): start the cache miss on a
    /// collection-set referent's header before the holder's evacuate pass
    /// reaches it.
    ///
    /// The closure copies a referent when it finds it, and the first touch of
    /// each from-space header is a miss the claim CAS then waits on. A chain
    /// node holding `next` and a payload took those two misses one after the
    /// other; prefetching every collection-set referent of the holder first
    /// lets them overlap (HotSpot's `G1ScanEvacuatedObjClosure` prefetches a
    /// referent when it pushes the slot, for the same reason). A hint: an
    /// address that is not a collection-set object is never prefetched, and a
    /// prefetch cannot fault.
    /// g1pin: is `addr` one of this pause's peer-stack object pins
    /// ([`Self::object_pins`])?
    #[inline(always)]
    pub fn object_pinned(&self, addr: usize) -> bool {
        if self.object_pins.is_empty() {
            return false;
        }
        let Some(ridx) = self.collector.lookup_region_for_addr(addr) else {
            return false;
        };
        if !self.object_pin_regions.contains(&ridx) {
            return false;
        }
        self.object_pins.binary_search(&addr).is_ok()
    }

    #[inline(always)]
    pub fn prefetch_cset_referent(&self, raw: usize) {
        if let Some(ridx) = self.collector.lookup_region_for_addr(raw) {
            if self.cset.contains(&ridx) {
                #[cfg(target_arch = "x86_64")]
                // SAFETY: `_mm_prefetch` is a hint that cannot fault on any
                // address, and SSE is part of the x86-64 baseline.
                unsafe {
                    core::arch::x86_64::_mm_prefetch(
                        raw as *const i8,
                        core::arch::x86_64::_MM_HINT_T0,
                    );
                }
            }
        }
    }

    pub fn record_fresh_child(
        old_ptr: *mut u8,
        new_ptr: *mut u8,
        defer_self_forwarded: bool,
        children: &mut Vec<usize>,
        deferred_self_forwarded: &mut Vec<usize>,
    ) {
        if defer_self_forwarded && old_ptr == new_ptr {
            deferred_self_forwarded.push(new_ptr as usize);
        } else {
            children.push(new_ptr as usize);
        }
    }

    /// JIT round 23 wave 8, lane L9 (`CRATONVM_G1_EVAC_LEAF_NO_PUSH`, OPT-IN
    /// `=1` since its n23-w8b A/B; see `evac_leaf_no_push_enabled`):
    /// [`Self::record_fresh_child`] for the worker closure,
    /// except that a fresh COPY of a primitive array is not pushed at all.
    ///
    /// A primitive array has no reference to scan, so its gray-queue entry
    /// bought one push, one pop and one `process_object` call that walks
    /// nothing (HotSpot's `do_copy_to_survivor_space` does not push a
    /// `typeArray` either). On a chain-shaped live set (`KeepLong`: each node
    /// holds `next` and a `byte[224]`) that entry also kept every worker's
    /// stack two deep, which is exactly the depth at which the idle-peer spill
    /// gives the OLDEST entry away -- often the chain's continuation -- so a
    /// chain migrated through the shared queue mutex once per node while a
    /// peer was idle. Without the leaf a worker that walks one chain holds a
    /// one-deep stack and keeps it.
    ///
    /// Not skipped: a self-forward (`old == new`, the evacuation-failure arm,
    /// whose deferred bookkeeping is unchanged) and a possible collection-
    /// overlay owner (`may_own` hashes class ids, so class 0 can collide).
    ///
    /// SAFETY: `new_ptr` is a copy this pause just wrote whole (its header
    /// included), as for [`Self::note_overlay_owner_copied`].
    #[inline]
    pub unsafe fn record_fresh_child_screened(
        &self,
        old_ptr: *mut u8,
        new_ptr: *mut u8,
        defer_self_forwarded: bool,
        children: &mut Vec<usize>,
        deferred_self_forwarded: &mut Vec<usize>,
    ) {
        if self.leaf_no_push && old_ptr != new_ptr && self.fresh_copy_is_scan_free_leaf(new_ptr) {
            return;
        }
        Self::record_fresh_child(
            old_ptr,
            new_ptr,
            defer_self_forwarded,
            children,
            deferred_self_forwarded,
        );
    }

    /// Is the fresh copy at `new_ptr` an object whose scan would visit no
    /// reference: a primitive array that cannot be a collection-overlay owner?
    ///
    /// SAFETY: `new_ptr` is a whole object header this pause wrote.
    #[inline]
    pub unsafe fn fresh_copy_is_scan_free_leaf(&self, new_ptr: *mut u8) -> bool {
        let header = &*(new_ptr as *const ObjectHeader);
        evac_header_is_primitive_array(header)
            && match self.overlay {
                Some(ov) => !ov.may_own(header.class_id.as_u32()),
                None => true,
            }
    }

    /// The collection-overlay edge, inside the parallel closure, part 1: this
    /// worker has just copied (`fresh`) the object at `old_ptr` to `new_ptr`.
    /// If it is a CSet owner with rows (`g1_overlay.rs`), remember the copy, so
    /// whichever worker later SCANS it expands its rows as extra fields
    /// ([`Self::expand_overlay_rows`]). Expanding here instead put a whole
    /// row graph on the discovering worker — every owner held by one big
    /// array landed on one thread. Exactly one worker sees an object fresh,
    /// so each owner is noted once. Owners reached any other way are
    /// followed by the serial fixpoint after the closure.
    ///
    /// SAFETY: `new_ptr` is a copy (or self-forward) this pause just wrote.
    #[inline]
    pub unsafe fn note_overlay_owner_copied(&self, old_ptr: *mut u8, new_ptr: *mut u8) {
        let Some(overlay) = self.overlay else {
            return;
        };
        let class_id = (*(new_ptr as *const ObjectHeader)).class_id.as_u32();
        if overlay.may_own(class_id) {
            overlay.note_copied(old_ptr as usize, new_ptr as usize);
        }
    }

    /// Part 2, at the start of [`Self::process_object`]: if `obj_ptr` is a
    /// noted owner copy, evacuate its rows' references as its children (and
    /// note any owner among them in turn).
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn expand_overlay_rows(
        &self,
        tlab: &mut TlabSet,
        obj_ptr: *mut u8,
        class_id: u32,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
        defer_self_forwarded: bool,
        children: &mut Vec<usize>,
        deferred_self_forwarded: &mut Vec<usize>,
    ) {
        let Some(overlay) = self.overlay else {
            return;
        };
        if !overlay.may_own(class_id) {
            return;
        }
        let Some(old_owner) = overlay.take_copied(obj_ptr as usize) else {
            return;
        };
        let mut refs: Vec<usize> = Vec::new();
        overlay.expand_owner_refs(old_owner, |r| refs.push(r));
        overlay::OVERLAY_PARALLEL_EXPANDED.fetch_add(1, Ordering::Relaxed);
        for r in refs {
            let Some(ridx) = self.collector.lookup_region_for_addr(r) else {
                continue;
            };
            if !self.cset.contains(&ridx)
                || !self.candidate_ok("parallel-overlay-row", std::ptr::null_mut(), 0, r)
            {
                continue;
            }
            if let Some((copied, fresh)) = self.evacuate(tlab, r as *mut u8, forwards, objs, bytes)
            {
                if fresh {
                    self.record_fresh_child_screened(
                        r as *mut u8,
                        copied,
                        defer_self_forwarded,
                        children,
                        deferred_self_forwarded,
                    );
                    self.note_overlay_owner_copied(r as *mut u8, copied);
                }
            }
        }
    }

    /// Claim/bump-allocate `size` bytes into `tlab`. Returns the destination
    /// address, or `None` on free-region-pool exhaustion (the same loss
    /// semantics the serial allocator has when it cannot find space).
    ///
    /// SAFETY: `regions_base` must be the live regions Vec base; the claimed
    /// region index is unique to this worker (lock-free `fetch_add`), so the
    /// `&mut G1Region` formed here never aliases another thread.
    #[inline(always)]
    pub unsafe fn tlab_alloc(&self, tlab: &mut Tlab, size: usize) -> Option<usize> {
        if tlab.region_idx.is_some() {
            let aligned = (tlab.base + tlab.offset + 7) & !7;
            let new_off = (aligned - tlab.base) + size;
            if new_off <= tlab.len {
                tlab.offset = new_off;
                #[cfg(target_arch = "x86_64")]
                if ((aligned ^ (tlab.base + new_off)) & !63) != 0 && new_off + 256 <= tlab.len {
                    // Keep upcoming destination PLAB cache lines in L1d M-state ahead of copy+CAS.
                    core::arch::asm!(
                        "prefetchw [{p}]",
                        p = in(reg) (tlab.base + new_off + 256),
                        options(nostack, readonly, preserves_flags),
                    );
                }
                return Some(aligned);
            }
        }
        self.tlab_alloc_slow(tlab, size)
    }

    #[cold]
    #[inline(never)]
    unsafe fn tlab_alloc_slow(&self, tlab: &mut Tlab, size: usize) -> Option<usize> {
        // gci i19/yc: a copy no region can hold fails HERE, before the loop.
        if !evac_copy_fits_a_region(size, self.collector.config.region_size) {
            return None;
        }
        let mut first_miss = true;
        loop {
            if tlab.region_idx.is_some() {
                if !first_miss {
                    let aligned = (tlab.base + tlab.offset + 7) & !7;
                    let new_off = (aligned - tlab.base) + size;
                    if new_off <= tlab.len {
                        tlab.offset = new_off;
                        return Some(aligned);
                    }
                }
                first_miss = false;
                // gci i16/pl (finding 4): HotSpot's
                // `G1PLABAllocator::allocate_direct_or_new_plab` -- an object
                // of at least `ParallelGCBufferWastePct` (10 %) of a PLAB that
                // does not fit the current chunk is copied straight into the
                // shared Survivor region and the chunk is KEPT; only a small
                // one retires the chunk for a fresh one. Shared chunks exist
                // only under `CRATONVM_G1_SURVIVOR_PLAB`.
                // JIT round 23 wave 10: Survivor chunks only -- the direct copy
                // goes into the shared SURVIVOR region, so an Old chunk
                // (`CRATONVM_G1_OLD_PLAB`) must not take it.
                if tlab.shared
                    && tlab.dest_type == RegionType::Survivor
                    && survivor_plab_wants_direct(size)
                {
                    if let Some(p) = self.survivor_plab_direct(size) {
                        return Some(p);
                    }
                }
                // Current region is full — retire it (write back the cursor) and
                // fall through to claim a fresh one.
                self.retire_tlab(tlab);
            }
            // gci i14/yt: a PLAB chunk of the shared Survivor region before a
            // whole region of its own (`CRATONVM_G1_SURVIVOR_PLAB`). `false`
            // only when the pool is spent; the arms below then take the
            // object down the existing exhaustion path.
            if self.survivor_plab
                && tlab.dest_type == RegionType::Survivor
                && self.claim_survivor_plab(tlab, size)
            {
                continue;
            }
            // Take up an OLD region a previous pause left partly filled before
            // consuming a fresh Free one. Without this the old generation grew
            // by one region per worker per pause regardless of how much was
            // promoted -- see `SharedEvac::resume` for the measurement.
            if self.claim_resume_region(tlab, size) {
                continue;
            }
            // JIT round 23 wave 10, lane L9 (`CRATONVM_G1_OLD_PLAB`): a PLAB
            // chunk of the shared Old region before a whole Free region of
            // this worker's own. `false` when the pool is spent, so the
            // existing exhaustion path (shared destinations, then the
            // evacuation-failure self-forward) runs exactly as before.
            if self.old_plab
                && tlab.dest_type == RegionType::Old
                && tlab.plab_ok
                && self.claim_old_plab(tlab, size)
            {
                continue;
            }
            let claimed = self.claim_pool_slot(tlab.dest_type);
            // gci i23/tn: the pause's Survivor region cap refused the region
            // (`CRATONVM_G1_SURVIVOR_REGION_CAP`). Not to-space exhaustion:
            // answer at once, without the shared-destination scan or the
            // exhaustion census below; `evacuate` copies the object into Old.
            // gci i48/rv48 (`gci-i24rv-...` item 14a): only on the shared-PLAB
            // arm, the one whose `claim_pool_slot` counts claims against the
            // cap. On the per-worker arm the refusal was the serial arm's, so
            // a `None` here is a genuine pool exhaustion: it still tries the
            // partly filled Survivor regions before `evacuate` goes to Old.
            if claimed.is_none()
                && tlab.dest_type == RegionType::Survivor
                && self.survivor_plab
                && self.collector.survivor_cap_reached()
            {
                return None;
            }
            if claimed.is_none() {
                // TO-SPACE EXHAUSTION — but exhaustion of WHAT?
                //
                // This arm can only ever allocate out of `pool`, the Free
                // regions reserved before the dispatch. Its serial twin
                // `alloc_in_type_locked_scan` first scans EVERY existing
                // non-CSet region of the destination type and bump-allocates
                // into a partially-filled one, and only then claims a Free
                // region. So the two arms do not run out at the same time, and
                // the difference is not a detail: returning `None` here sends
                // the object down the evacuation-FAILURE path (self-forward,
                // kept region, `retry_after_evacuation_failure`), which is a
                // different and much more fragile machine than a copy.
                //
                // The number that decides whether that gap is real is how much
                // room the serial arm WOULD have found at this instant, so
                // count it here rather than reasoning about it. Rate-limited
                // and only on the failure path, so a healthy pause pays
                // nothing.
                // FALL BACK THE WAY THE SERIAL ARM ALREADY DOES.
                //
                // `alloc_in_type_locked_scan` scans every existing non-CSet
                // region of the destination type and bump-allocates into a
                // partially-filled one BEFORE it claims a Free region. This arm
                // only ever had the Free pool, so it declared to-space
                // exhaustion — and sent the object down the evacuation-FAILURE
                // path — while to-space was still there. MEASURED 2026-09-06 on
                // `TestHostConfigAutomaticDeploymentXmlExternalWarXml`:
                //
                //   EXHAUSTED ITS POOL: dest_type=Old size=48 pool_len=1
                //     — but 2017 of 2025 non-CSet Old regions still have room
                //
                // for a FORTY-EIGHT byte promotion, and zero such events on the
                // serial arm in the same census.
                //
                // `bump_alloc` takes `&self` and advances the cursor with a
                // compare-exchange (F-11 made it atomic for exactly this), so a
                // shared destination is inside the module's discipline — it is
                // the "atomically-CAS shared regions" half, extended from CSet
                // regions to destination ones. What it is NOT is a TLAB: a TLAB
                // owns its region's cursor and `retire_tlab` STORES it, so this
                // path deliberately leaves `tlab` untouched and hands back one
                // object's worth of space.
                if self.shared_dest {
                    if let Some(p) = self.shared_dest_alloc(tlab.dest_type, size) {
                        return Some(p);
                    }
                }
                let n = PARALLEL_TLAB_POOL_EXHAUSTED.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 8 || n.is_power_of_two() {
                    let mut with_room = 0usize;
                    let mut of_type = 0usize;
                    // THE 2026-09-20 CORRECTION TO THIS CENSUS.
                    //
                    // `with_room` used to be the whole diagnosis, and on a
                    // young pause it was WRONG in the direction that invents a
                    // defect. It counted every non-CSet region of the
                    // destination type, which on a young pause is exactly the
                    // set of regions this pause claimed out of `pool` as TLABs
                    // — regions `shared_dest_alloc` must never bump into,
                    // because `retire_tlab` STORES their owner's offset as the
                    // cursor and would discard anything written past it. Worse,
                    // it read `r.cursor()`, which for a live TLAB is the value
                    // from BEFORE the TLAB started bumping: room that has
                    // already been consumed and not yet written back.
                    //
                    // Measured on `HumongousChurn 48 6000 512 -Xmx160m`, the
                    // resulting `with_room` tracked the WORKER COUNT (1, 1, 3,
                    // 7 at `CRATONVM_G1_WORKERS` 1, 2, 4, 8) rather than any
                    // property of the heap — it was counting the other
                    // workers' TLABs — and the line's closing sentence, "the
                    // two arms disagree about whether this pause is out of
                    // space", was read as a measurement and turned into a
                    // defect report. The serial arm would have found nothing
                    // either.
                    //
                    // `eligible` applies the exact filter `shared_dest_alloc`
                    // applies. That one IS the disagreement between the arms,
                    // and it is the only number here worth acting on.
                    let mut eligible = 0usize;
                    for idx in 0..self.regions_len {
                        // SAFETY: `idx < regions_len`; a shared read of one
                        // region, exactly as `seed_source_region` takes.
                        let r = &*self.regions_base.0.add(idx);
                        if r.region_type() != tlab.dest_type || self.cset.contains(&idx) {
                            continue;
                        }
                        of_type += 1;
                        if r.data.len().saturating_sub(r.cursor()) >= size {
                            with_room += 1;
                            if !self.pool_set.contains(&idx) && !self.resume_set.contains(&idx) {
                                eligible += 1;
                            }
                        }
                    }
                    let latched = self.shared_dest_proved_empty(tlab.dest_type, size);
                    tracing::warn!(
                        "[g1] parallel evacuation EXHAUSTED ITS POOL (#{n}): dest_type={:?} size={size} pool_len={} of_type={of_type} with_room={with_room} ELIGIBLE={eligible} proved_empty={latched} — `eligible` is the only number here that names a gap: it applies `shared_dest_alloc`'s own filter (not in the pool, not a resume region, not in the CSet), so `eligible > 0` means the parallel arm fell over to the evacuation-FAILURE path while to-space it was allowed to use was still there. `with_room` does NOT mean that and must not be read as it: on a young pause it counts this pause's own TLAB regions, at their pre-bump cursors, and tracks the worker count rather than the heap. `proved_empty=true` means a full pass already showed there is nothing, and the scan was skipped rather than repeated.",
                        tlab.dest_type,
                        self.pool.len(),
                    );
                }
                return None;
            }
            let idx = self.pool[claimed.expect("checked is_none above")];
            // F-16: the pool is FREE regions, which under a reserved heap may
            // be address space rather than memory. Commit before the region is
            // retyped and written into. A refusal is to-space exhaustion, which
            // this loop's caller already handles by self-forwarding — the same
            // outcome as an empty pool, reached one region earlier.
            if !self.collector.commit_through_region(idx) {
                return None;
            }
            // W3-A: `&*`, not `&mut *`. Every field this arm writes is now
            // `&self`-writable (the atomic type/age below, and the cursor via
            // `retire_tlab`), so the `&mut G1Region` that used to be formed
            // here — while peer workers held `&G1Region` to the same region
            // through `SharedEvac::view()` — is gone, and with it the aliasing
            // half of the undefined behaviour. What is left is a shared read of
            // one region, exactly as `seed_source_region` takes.
            let region = &*self.regions_base.0.add(idx);
            region.claim_as_evacuation_destination(tlab.dest_type);
            // G1AUD-10: a freshly-Free region is NO LONGER zero-filled — `reset`
            // stopped scrubbing (see its note; it was 42% of a young pause and
            // redundant with the allocator). This path does not need it and
            // never did: every byte below the cursor this TLAB finally writes
            // back is a whole object placed by `evacuate`'s
            // `copy_nonoverlapping`, and the align-to-8 below is always a no-op
            // because every object size is a multiple of 8 — so the TLAB leaves
            // no gap for a walker to interpret. Objects are copied in, not
            // default-initialised, which is what separates this from
            // `bump_alloc` (whose zeroing IS load-bearing, for Java's
            // default-zero fields).
            tlab.region_idx = Some(idx);
            tlab.base = region.data.addr();
            tlab.len = region.data.len();
            tlab.offset = 0;
        }
    }

    /// Claim one slot of [`Self::pool`] for a TLAB of `dest_type`, returning
    /// the INDEX INTO `pool` (not the region index), or `None` when the pool
    /// is spent.
    ///
    /// # Which end, and why it is not an aesthetic choice
    ///
    /// The serial allocator splits its Free-region claims by occupant:
    /// `claim_free_region_for` sends Eden and Survivor to
    /// `claim_free_region_young`, which searches DOWNWARD from the top of the
    /// committed prefix, and everything else to `claim_free_region`, which
    /// searches upward from zero. That split is ten-findings item 8, and its
    /// whole purpose is contiguity: `find_contiguous_free` looks for a
    /// humongous run from index 0, so a young claim taken from the bottom
    /// punches a hole in exactly the runs the next humongous allocation needs.
    /// HotSpot separates the two populations the same way.
    ///
    /// This arm took every claim from the front of `pool` — which is ascending
    /// region order — for BOTH destination types. So on the default (parallel)
    /// path, every Survivor region a young pause claimed came from the bottom
    /// of the heap, against the policy the serial path implements, and the
    /// difference grew with the worker count because each worker claims its
    /// own. A workload that mixes large buffers with young churn therefore
    /// fragmented itself faster under the evacuator that is actually running.
    /// Survivor now takes from the back and Old from the front, so the two
    /// populations grow toward each other exactly as item 8 intends.
    ///
    /// # Why one word rather than two atomics
    ///
    /// Two independent counters cannot answer "is the pool spent?" without a
    /// window in which `front + back` transiently exceeds the length and two
    /// workers are handed the SAME region — which is a second worker
    /// bump-allocating into a TLAB whose owner will `set_cursor` over it. One
    /// packed word with a compare-exchange makes the test and the claim a
    /// single atomic step, and the loop is uncontended in practice: it runs
    /// once per REGION consumed, not once per object.
    ///
    /// # Why the ends are the ends of the COMMITTED part
    ///
    /// Until 2026-09-30 both ends were the ends of the whole region TABLE, so
    /// the first Survivor claim of the first parallel young pause took the
    /// highest region of the reservation, and `commit_through_region` — which
    /// must keep the committed set a prefix — committed all of `-Xmx` below it
    /// (`XmsProbe -Xms64m -Xmx512m` read `total=512m` after one pause; HotSpot
    /// reads 64m). The serial `claim_free_region_young` never did this: it
    /// searches down from the top of the committed prefix and grows the prefix
    /// by one region only when the prefix holds no Free region. The pool is
    /// now split the same way — committed Free regions first, the uncommitted
    /// tail after them — and the two ends work inside the committed part; once
    /// it is spent, either destination type takes the next tail region in
    /// ascending order, so the prefix grows by what the pause needs. The pool
    /// still holds every Free region, so to-space exhaustion is reached at
    /// exactly the same point as before: only the claim ORDER moved.
    pub fn claim_pool_slot(&self, dest_type: RegionType) -> Option<usize> {
        const MASK: u64 = (1u64 << POOL_CURSOR_BITS) - 1;
        let len = self.pool.len();
        if len == 0 {
            return None;
        }
        // gci i23/tn (`CRATONVM_G1_SURVIVOR_REGION_CAP`): HotSpot's
        // `G1CollectedHeap::new_gc_alloc_region` hands out no Survivor region
        // once `G1Policy::is_survivor_full`. On the shared-PLAB arm only: the
        // per-worker arm claims one Survivor region PER WORKER, so a cap of
        // `ceil(young / 8)` regions would be spent by the worker count, not
        // by the survivor volume HotSpot's cap measures.
        if dest_type == RegionType::Survivor
            && self.survivor_plab
            && !self.collector.survivor_cap_admits_region()
        {
            return None;
        }
        let k = self.pool_committed.min(len);
        // Survivor from the back, everything else (Old) from the front.
        let from_back = dest_type == RegionType::Survivor;
        let mut cur = self.pool_next.load(Ordering::Relaxed);
        loop {
            let front = (cur & MASK) as usize;
            let back = ((cur >> POOL_CURSOR_BITS) & MASK) as usize;
            let grow = (cur >> (2 * POOL_CURSOR_BITS)) as usize;
            let (next, slot) = if front + back < k {
                if from_back {
                    (cur + (1u64 << POOL_CURSOR_BITS), k - 1 - back)
                } else {
                    (cur + 1, front)
                }
            } else if k + grow < len {
                (cur + (1u64 << (2 * POOL_CURSOR_BITS)), k + grow)
            } else {
                return None;
            };
            match self.pool_next.compare_exchange_weak(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(slot),
                Err(observed) => cur = observed,
            }
        }
    }

    /// Hand `tlab` a partly-filled non-CSet region of its destination type,
    /// resuming at that region's existing cursor. Returns whether one was
    /// claimed; the caller then re-enters the TLAB fast path.
    ///
    /// The claim is a `fetch_add` into [`Self::resume`], so an index is handed
    /// to exactly one worker and the `&mut G1Region` formed here never aliases
    /// another thread -- the same discipline the Free-pool claim below uses.
    /// A candidate that no longer qualifies (retyped, or filled by a worker
    /// that got there first) is skipped rather than retried, because the
    /// cursor only ever moves forward.
    ///
    /// SURVIVOR IS DELIBERATELY EXCLUDED, and not as caution: on a young pause
    /// the CSet takes every Eden and Survivor region, so a Survivor candidate
    /// would be filtered out by `resume`'s own CSet term and the branch would
    /// be dead. Old is the destination whose regions outlive the pause.
    pub unsafe fn claim_resume_region(&self, tlab: &mut Tlab, size: usize) -> bool {
        if tlab.dest_type != RegionType::Old || self.resume.is_empty() {
            return false;
        }
        loop {
            let i = self.resume_next.fetch_add(1, Ordering::Relaxed);
            if i >= self.resume.len() {
                return false;
            }
            let idx = self.resume[i];
            // F-16: an in-use region of the destination type is already
            // committed, but ask rather than assume (as `shared_dest_alloc`
            // does).
            if !self.collector.commit_through_region(idx) {
                continue;
            }
            // SAFETY: `idx < regions_len` by construction of `resume`, and the
            // `fetch_add` above makes this index this worker's alone.
            //
            // W3-A: `&*`, not `&mut *`. This arm never wrote anything through
            // the reference — it reads the cursor, the type and the extent and
            // hands the region to a TLAB — so the `&mut` was pure aliasing
            // hazard: it was formed while peers held `&G1Region` to the very
            // same region through `SharedEvac::view()`.
            let region = &*self.regions_base.0.add(idx);
            let cursor = region.cursor();
            if region.region_type() != RegionType::Old
                || region.data.len().saturating_sub(cursor) < size
            {
                continue;
            }
            PARALLEL_EVAC_RESUMED_DEST_REGIONS.fetch_add(1, Ordering::Relaxed);
            tlab.region_idx = Some(idx);
            tlab.base = region.data.addr();
            tlab.len = region.data.len();
            // The one line that makes this a RESUME rather than a re-claim:
            // start where the previous pause's TLAB stopped. `retire_tlab`
            // stores `offset` as the cursor, so this round-trips exactly.
            tlab.offset = cursor;
            return true;
        }
    }

    /// One object's worth of space in an EXISTING non-CSet region of
    /// `dest_type`, for when the Free pool is gone.
    ///
    /// Skips the pool entirely: every region in it is, or is about to become, a
    /// worker's TLAB, and `retire_tlab` stores that TLAB's offset as the
    /// region's cursor — a store that would silently discard anything this
    /// path bumped into the same region. Skips CSet regions for the reason
    /// `alloc_in_type_locked_scan` spells out: Phase 5 frees them, so a copy
    /// placed there is lost and every reference to it dangles.
    ///
    /// # Why this starts from a hint rather than from index 0
    ///
    /// This is the path a pause takes once its Free pool is gone, and until
    /// 2026-09-20 it restarted an O(num_regions) scan FOR EVERY OBJECT. Those
    /// are exactly the pauses that have the most objects left to place: a
    /// 2048-region heap with the pool exhausted paid two million region probes
    /// per thousand promotions, per worker, with every probe touching another
    /// worker's `region_type` — so the arm that exists to keep a pause off the
    /// evacuation-FAILURE machinery made the pause quadratic in the number of
    /// objects it saved.
    ///
    /// The cursor reused here is [`G1Collector::evac_dest_hint`], the same cell
    /// `alloc_in_type_locked` keeps for the serial arm, and for the same reason:
    /// the region the last object of this type landed in is overwhelmingly the
    /// region the next one fits in too. Sharing it with the serial arm is safe
    /// because BOTH validate a candidate before using it (right type, not in
    /// the CSet/pool/resume, `bump_alloc` succeeds), so a stale or foreign hint
    /// costs one region-type read and nothing else; `reset_evac_dest_hints`
    /// drops both cells at the top of every pause.
    ///
    /// Several workers sharing one cursor is a feature, not contention: they
    /// are all trying to pack the same destination region, and a `Relaxed`
    /// store that loses a race merely sends the next probe one region further
    /// back than it had to start. `bump_alloc` is the CAS that actually
    /// arbitrates the space.
    pub unsafe fn shared_dest_alloc(&self, dest_type: RegionType, size: usize) -> Option<usize> {
        if self.regions_len == 0 {
            return None;
        }
        // THE FUTILE-PASS LATCH. See [`Self::shared_dest_proved_empty`].
        //
        // MEASURED 2026-09-20 on `HumongousChurn 48 6000 512 -Xmx160m
        // --nojit`: `tlab_pool_exhausted=742926`, `shared_dest_allocs=0`. The
        // fallback ran 742,926 times, scanned all 160 regions every time, and
        // succeeded ZERO times — 119 million region probes that could not
        // have found anything, because on a YOUNG pause a Survivor candidate
        // cannot exist by construction: every pre-existing Survivor region is
        // in the CSet, and every Survivor region this pause created came out
        // of `pool` and is therefore in `pool_set`. The scan is O(regions) per
        // OBJECT on exactly the pauses that have the most objects left to
        // place, which is the same cost curve the `evac_dest_hint` change was
        // written to remove and which the hint cannot remove here: a hint only
        // helps a scan that can succeed.
        let slot = self.dest_empty_slot(dest_type);
        if let Some(s) = slot {
            if size >= self.dest_empty_from[s].load(Ordering::Relaxed) {
                return None;
            }
        }
        let hint_cell = self.collector.evac_dest_hint(dest_type);
        let start = hint_cell
            .map(|c| c.load(Ordering::Relaxed))
            .filter(|&h| h < self.regions_len)
            .unwrap_or(0);
        // One wrapping pass: `start..len` then `0..start`. A full pass is still
        // what proves "no region of this type has room", which is the answer
        // the caller needs before it self-forwards.
        for step in 0..self.regions_len {
            let idx = {
                let i = start + step;
                if i >= self.regions_len {
                    i - self.regions_len
                } else {
                    i
                }
            };
            if self.pool_set.contains(&idx)
                || self.resume_set.contains(&idx)
                || self.cset.contains(&idx)
            {
                continue;
            }
            // SAFETY: `idx < regions_len`; a shared read, and `bump_alloc`
            // takes `&self` and is a CAS.
            let r = &*self.regions_base.0.add(idx);
            if r.region_type() != dest_type {
                continue;
            }
            // F-16: under a reserved heap a region may be address space rather
            // than memory. An in-use region of the destination type is already
            // committed, but ask rather than assume.
            if !self.collector.commit_through_region(idx) {
                continue;
            }
            if let Some((ptr, _)) = r.bump_alloc(size, 8, "parallel-evac:shared-dest") {
                // gci i10/rh finding 1: `live_bytes` has no writer during a pause.
                self.collector.note_promoted_into_marked(r, size);
                if let Some(c) = hint_cell {
                    c.store(idx, Ordering::Relaxed);
                }
                PARALLEL_SHARED_DEST_ALLOCS.fetch_add(1, Ordering::Relaxed);
                return Some(ptr as usize);
            }
        }
        // A FULL PASS FOUND NOTHING — record it, so the next object of this
        // type and this size or larger does not repeat it.
        if let Some(s) = slot {
            self.dest_empty_from[s].fetch_min(size, Ordering::Relaxed);
            PARALLEL_SHARED_DEST_PROVED_EMPTY.fetch_add(1, Ordering::Relaxed);
        }
        None
    }

    /// Which [`Self::dest_empty_from`] cell a destination type uses, or `None`
    /// for a type this fallback never serves.
    ///
    /// # Why a failing pass may be remembered for the rest of the pause
    ///
    /// The latch is only sound if the candidate set can never GROW during the
    /// closure, and if a non-candidate can never gain room. Both hold, and
    /// neither by luck:
    ///
    /// * A region joins the `region_type == dest_type` population only in
    ///   `tlab_alloc`, which retypes a region it took out of `pool` — and
    ///   every pool region is in `pool_set`, which this scan skips
    ///   unconditionally and forever. `claim_resume_region` does not retype at
    ///   all. Phases 4 and 5, which is where regions are freed and reclassified,
    ///   run after every worker has stopped.
    /// * Room only ever shrinks. `bump_alloc` advances a cursor with a CAS;
    ///   `retire_tlab` stores a TLAB's offset, which started at the region's
    ///   cursor (resume) or at zero on a fresh claim, and only grew — and both
    ///   kinds of region are excluded from this scan anyway.
    ///
    /// The latch is keyed by the SMALLEST size that failed rather than being a
    /// plain bool, because "no region has room for a 512 KiB array" says
    /// nothing whatever about a 32-byte object. `fetch_min` makes the record
    /// monotone under concurrent workers, and the test is `size >= recorded`,
    /// so a smaller request always still gets its own full pass.
    ///
    /// `None` also when `CRATONVM_G1_EVAC_DEST_LATCH=0` disarms the latch,
    /// which restores the repeated full pass exactly — the bisection arm for
    /// anyone who suspects the monotonicity argument above.
    #[inline]
    pub fn dest_empty_slot(&self, dest_type: RegionType) -> Option<usize> {
        if !self.dest_latch {
            return None;
        }
        match dest_type {
            RegionType::Survivor => Some(0),
            RegionType::Old => Some(1),
            _ => None,
        }
    }

    /// Diagnostic mirror of the latch state, for the pool-exhaustion warning.
    #[inline]
    pub fn shared_dest_proved_empty(&self, dest_type: RegionType, size: usize) -> bool {
        self.dest_empty_slot(dest_type)
            .is_some_and(|s| size >= self.dest_empty_from[s].load(Ordering::Relaxed))
    }

    /// Write a TLAB's final bump cursor back to its region and clear the TLAB.
    pub unsafe fn retire_tlab(&self, tlab: &mut Tlab) {
        if let Some(idx) = tlab.region_idx.take() {
            // W3-A: `&*`, not `&mut *` — `set_cursor` has taken `&self` since
            // F-11 made the cursor atomic, so the `&mut` here was only ever the
            // aliasing hazard without the write that would have justified it.
            let region = &*self.regions_base.0.add(idx);
            if tlab.shared {
                // gci i14/yt: a PLAB chunk of a shared region. The region's
                // cursor already stands past the chunk (the claim was the
                // bump); stamp the unused tail so a linear walk of
                // `[0, cursor)` steps over it, as HotSpot retires a PLAB.
                let tail = tlab.len.saturating_sub(tlab.offset);
                if tail > 0 {
                    cratonvm_gc_core::heap::write_filler(tlab.base + tlab.offset, tail);
                    // JIT round 23 wave 10: an Old chunk (`CRATONVM_G1_OLD_PLAB`)
                    // is stamped the same way but not counted in the Survivor
                    // census.
                    if tlab.dest_type == RegionType::Survivor {
                        self.collector
                            .survivor_plab_tail_bytes
                            .fetch_add(tail as u64, Ordering::Relaxed);
                    }
                }
            } else {
                region.set_cursor(tlab.offset);
            }
        }
        tlab.base = 0;
        tlab.len = 0;
        tlab.offset = 0;
        tlab.shared = false;
    }

    /// gci i14/yt — `CRATONVM_G1_SURVIVOR_PLAB`: give `tlab` (a Survivor
    /// TLAB) a PLAB chunk big enough for `size` out of the pause's shared
    /// current Survivor region, claiming and publishing a fresh pool region
    /// when the current one cannot hold it. HotSpot's shape: every worker
    /// carves `YoungPLABSize`-like chunks from one shared survivor allocation
    /// region, so a pause that copies 2 MB of survivors leaves ~2 regions,
    /// not one per copying worker (`gci-i14yt-g1-survivor-destinations-...`).
    ///
    /// Sound under the module's discipline: the chunk is claimed with the
    /// region's own compare-exchange (`G1Region::claim_unzeroed` since gci
    /// i16/pl; `bump_alloc`, which also zeroed it, before), so
    /// no two workers own overlapping bytes; the region is a POOL region, so
    /// `shared_dest_alloc` (which skips `pool_set`) never bumps into it and
    /// the destination latch's argument is unchanged; a fresh region is ours
    /// alone until the publishing store, and its cursor is reset to 0 before
    /// the first carve. `retire_tlab` stamps the unused tail of each chunk.
    /// A region whose tail is too short for the next chunk hands out its
    /// remainder whole when that still fits the object, so a region is left
    /// with at most one unclaimed tail of less than one object.
    ///
    /// Returns `false` only when the pool is spent (or a commit refused):
    /// the caller then takes its existing exhaustion path.
    ///
    /// gci i15/ra (finding 1 of
    /// `gci-i15ra-g1-evacuation-additions-review-findings-FIXED-20261016`): ONE
    /// worker installs each fresh region. It used to be every worker that
    /// found the current region full: each claimed a pool region of its own,
    /// carved one chunk, and all but the first publish lost their CAS, so
    /// every region switch could leave up to `workers - 1` Survivor regions
    /// holding a single 32 KiB chunk -- the sparse-survivor shape this switch
    /// exists to remove, re-created at every switch (and at the first claim
    /// of a pause whose driver seeded no Survivor copy). Now the switching
    /// worker first moves the shared word to [`SURVIVOR_PLAB_SWITCHING`];
    /// peers that meet it wait for the publish (or for the restore, when the
    /// pool is spent) and carve from the new region. Every path that takes
    /// the word to the sentinel stores a region index (or the old value) back
    /// before it returns, and nothing between can block on a peer.
    pub unsafe fn claim_survivor_plab(&self, tlab: &mut Tlab, size: usize) -> bool {
        let need = (size + 7) & !7;
        let mut waits = 0u32;
        loop {
            let cur = self.survivor_plab_region.load(Ordering::Acquire);
            if cur == SURVIVOR_PLAB_SWITCHING {
                // A peer is installing the next region: a pool claim, a
                // commit, a retype and one unzeroed chunk carve (no zeroing
                // since gci i16/pl). Spin briefly, then yield.
                if waits == 0 {
                    // gci i25/pl: once per waiting claim, for the census.
                    self.collector
                        .survivor_plab_switch_waits
                        .fetch_add(1, Ordering::Relaxed);
                }
                waits = waits.wrapping_add(1);
                if waits % 64 == 0 {
                    std::thread::yield_now();
                } else {
                    std::hint::spin_loop();
                }
                continue;
            }
            if cur != usize::MAX && cur < self.regions_len {
                // SAFETY: `cur < regions_len`; a shared read, and every write
                // below is the region's CAS bump.
                let region = &*self.regions_base.0.add(cur);
                let chunk = survivor_plab_chunk(need, region.data.len());
                if chunk >= need {
                    // gci i16/pl (finding 3): not zeroed -- every byte is
                    // copied over or stamped as filler at retirement.
                    if let Some(off) =
                        region.claim_unzeroed(chunk, "parallel-evac:survivor-plab")
                    {
                        self.install_survivor_plab(tlab, cur, region, off, chunk);
                        return true;
                    }
                }
                // No room for a whole chunk: the remainder, if it holds this
                // object. A lost race re-reads and retries.
                let rem = region.data.len().saturating_sub(region.cursor()) & !7;
                if rem >= need {
                    if let Some(off) =
                        region.claim_unzeroed(rem, "parallel-evac:survivor-plab-rest")
                    {
                        self.install_survivor_plab(tlab, cur, region, off, rem);
                        return true;
                    }
                    continue;
                }
            }
            // An object no region can hold is not a young copy; never hand
            // out a chunk smaller than the request (the caller would loop).
            // Asked BEFORE a pool region is claimed (gci i15/ra finding 2:
            // it used to be asked after, leaving the claimed region retyped
            // Survivor, empty and unpublished).
            if survivor_plab_chunk(need, self.collector.config.region_size) < need {
                return false;
            }
            // Become the one worker that installs the next region. A lost CAS
            // means a peer published a region or is installing one: re-read.
            if self
                .survivor_plab_region
                .compare_exchange(
                    cur,
                    SURVIVOR_PLAB_SWITCHING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }
            // From here every return puts a value other than the sentinel
            // back: the new region, or `cur` when there is none to install.
            let installed = self.install_fresh_survivor_plab_region(tlab, need);
            self.survivor_plab_region.store(installed.unwrap_or(cur), Ordering::Release);
            return installed.is_some();
        }
    }

    /// gci i15/ra: the elected half of [`Self::claim_survivor_plab`] -- claim
    /// a pool region, retype it Survivor, carve `need` bytes' chunk into
    /// `tlab` and answer the region's index for the caller to publish; `None`
    /// (nothing carved) when the pool is spent, a commit is refused or the
    /// carve fails. Runs while the shared word holds
    /// [`SURVIVOR_PLAB_SWITCHING`], so it must not wait on a peer.
    pub unsafe fn install_fresh_survivor_plab_region(
        &self,
        tlab: &mut Tlab,
        need: usize,
    ) -> Option<usize> {
        // A fresh region from the pool, as the per-worker arm claims one.
        let slot = self.claim_pool_slot(RegionType::Survivor)?;
        let idx = self.pool[slot];
        if !self.collector.commit_through_region(idx) {
            return None;
        }
        // SAFETY: `idx` came out of the pool through the one-word claim, so
        // no other worker holds it until the caller publishes it.
        let region = &*self.regions_base.0.add(idx);
        region.claim_as_evacuation_destination(RegionType::Survivor);
        region.set_cursor(0);
        let chunk = survivor_plab_chunk(need, region.data.len());
        if chunk < need {
            return None;
        }
        // gci i16/pl (finding 3): unzeroed, as the chunks above.
        let off = region.claim_unzeroed(chunk, "parallel-evac:survivor-plab")?;
        self.install_survivor_plab(tlab, idx, region, off, chunk);
        self.collector
            .survivor_plab_regions
            .fetch_add(1, Ordering::Relaxed);
        Some(idx)
    }

    /// Point `tlab` at the chunk `[off, off + len)` of the shared Survivor
    /// region `idx`.
    #[inline]
    pub fn install_survivor_plab(
        &self,
        tlab: &mut Tlab,
        idx: usize,
        region: &G1Region,
        off: usize,
        len: usize,
    ) {
        tlab.region_idx = Some(idx);
        tlab.base = region.data.addr();
        tlab.offset = off;
        tlab.len = off + len;
        tlab.shared = true;
        self.collector
            .survivor_plab_chunks
            .fetch_add(1, Ordering::Relaxed);
    }

    /// gci i16/pl (finding 4): copy destination for a large Survivor object
    /// straight out of the pause's shared Survivor region, leaving the
    /// worker's chunk in place (HotSpot's direct allocation). `None` when
    /// there is no published shared region (none yet, or a peer is
    /// switching) or it has no room: the caller then retires its chunk and
    /// takes the ordinary PLAB path, which switches regions.
    ///
    /// Sound for the reasons the chunks are: the span is claimed by the
    /// region's own compare-exchange, the region is a pool region
    /// (`shared_dest_alloc` never bumps into it), and `evacuate` writes the
    /// whole object into it before anything reads below the cursor; a
    /// CAS-loser's abandoned copy is whole too.
    ///
    /// SAFETY: as [`Self::claim_survivor_plab`] (`regions_base` is the live
    /// table; the shared index is bounds-checked before the deref).
    pub unsafe fn survivor_plab_direct(&self, size: usize) -> Option<usize> {
        let cur = self.survivor_plab_region.load(Ordering::Acquire);
        // `usize::MAX` (no region yet) and `SURVIVOR_PLAB_SWITCHING` are
        // both far above any table length.
        if cur >= self.regions_len {
            return None;
        }
        let region = &*self.regions_base.0.add(cur);
        let need = (size + 7) & !7;
        let off = region.claim_unzeroed(need, "parallel-evac:survivor-direct")?;
        self.collector
            .survivor_plab_direct_objects
            .fetch_add(1, Ordering::Relaxed);
        self.collector
            .survivor_plab_direct_bytes
            .fetch_add(need as u64, Ordering::Relaxed);
        Some(region.data.addr() + off)
    }

    /// JIT round 23 wave 10, lane L9 (`CRATONVM_G1_OLD_PLAB`): give `tlab` (an
    /// Old TLAB) a PLAB chunk big enough for `size` out of the pause's shared
    /// current Old region, installing a fresh pool region when the current one
    /// cannot hold it -- [`Self::claim_survivor_plab`]'s protocol for the Old
    /// destination, word for word: the chunk is claimed with the region's own
    /// compare-exchange (`claim_unzeroed`), so no two workers own overlapping
    /// bytes; the region is a POOL region, so `shared_dest_alloc` (which skips
    /// `pool_set`) never bumps into it; ONE worker installs each fresh region
    /// (the word goes to [`SURVIVOR_PLAB_SWITCHING`] meanwhile, and every path
    /// stores a region index or the old value back); `retire_tlab` stamps the
    /// unused tail of each chunk as a filler.
    ///
    /// # Why
    ///
    /// A worker's Old TLAB used to be a whole Free region, so every promoting
    /// worker could end a pause holding a part-filled Old region, and
    /// [`EVAC_POOL_REGIONS_PER_WORKER`] capped the pause at `pool / 8` workers to
    /// keep those tails to a quarter of the pool. On `KeepLong -Xmx1g` the pool
    /// sits at 16-19 regions, so the pause ran at width 2 or went serial. With
    /// both destinations shared the pool feeds two regions at a time whatever
    /// the width (HotSpot's PLAB model), and the cap is relaxed to
    /// [`EVAC_POOL_REGIONS_PER_WORKER_SHARED`].
    ///
    /// Returns `false` only when the pool is spent (or a commit refused): the
    /// caller then takes its existing exhaustion path, which ends in the
    /// evacuation-failure self-forward -- nothing is lost, the object stays.
    pub unsafe fn claim_old_plab(&self, tlab: &mut Tlab, size: usize) -> bool {
        let need = (size + 7) & !7;
        let mut waits = 0u32;
        loop {
            let cur = self.old_plab_region.load(Ordering::Acquire);
            if cur == SURVIVOR_PLAB_SWITCHING {
                waits = waits.wrapping_add(1);
                if waits % 64 == 0 {
                    std::thread::yield_now();
                } else {
                    std::hint::spin_loop();
                }
                continue;
            }
            if cur != usize::MAX && cur < self.regions_len {
                // SAFETY: `cur < regions_len`; a shared read, and every write
                // below is the region's CAS bump.
                let region = &*self.regions_base.0.add(cur);
                let chunk = old_plab_chunk(need, region.data.len(), self.old_plab_bytes);
                if chunk >= need {
                    if let Some(off) = region.claim_unzeroed(chunk, "parallel-evac:old-plab") {
                        self.install_old_plab(tlab, cur, region, off, chunk);
                        return true;
                    }
                }
                let rem = region.data.len().saturating_sub(region.cursor()) & !7;
                if rem >= need {
                    if let Some(off) = region.claim_unzeroed(rem, "parallel-evac:old-plab-rest") {
                        self.install_old_plab(tlab, cur, region, off, rem);
                        return true;
                    }
                    continue;
                }
            }
            if old_plab_chunk(need, self.collector.config.region_size, self.old_plab_bytes) < need {
                return false;
            }
            if self
                .old_plab_region
                .compare_exchange(cur, SURVIVOR_PLAB_SWITCHING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            let installed = self.install_fresh_old_plab_region(tlab, need);
            self.old_plab_region.store(installed.unwrap_or(cur), Ordering::Release);
            return installed.is_some();
        }
    }

    /// The elected half of [`Self::claim_old_plab`]: claim a pool region, retype
    /// it Old, carve `need` bytes' chunk into `tlab` and answer the region's
    /// index for the caller to publish; `None` (nothing carved) when the pool
    /// is spent, a commit is refused or the carve fails. Runs while the shared
    /// word holds [`SURVIVOR_PLAB_SWITCHING`], so it must not wait on a peer.
    pub unsafe fn install_fresh_old_plab_region(&self, tlab: &mut Tlab, need: usize) -> Option<usize> {
        let slot = self.claim_pool_slot(RegionType::Old)?;
        let idx = self.pool[slot];
        if !self.collector.commit_through_region(idx) {
            return None;
        }
        // SAFETY: `idx` came out of the pool through the one-word claim, so no
        // other worker holds it until the caller publishes it.
        let region = &*self.regions_base.0.add(idx);
        region.claim_as_evacuation_destination(RegionType::Old);
        region.set_cursor(0);
        let chunk = old_plab_chunk(need, region.data.len(), self.old_plab_bytes);
        if chunk < need {
            return None;
        }
        let off = region.claim_unzeroed(chunk, "parallel-evac:old-plab")?;
        self.install_old_plab(tlab, idx, region, off, chunk);
        Some(idx)
    }

    /// Point `tlab` at the chunk `[off, off + len)` of the shared Old region
    /// `idx` (`CRATONVM_G1_OLD_PLAB`). [`Self::install_survivor_plab`] without
    /// its Survivor census, which keeps counting Survivor chunks only.
    #[inline]
    pub fn install_old_plab(
        &self,
        tlab: &mut Tlab,
        idx: usize,
        region: &G1Region,
        off: usize,
        len: usize,
    ) {
        tlab.region_idx = Some(idx);
        tlab.base = region.data.addr();
        tlab.offset = off;
        tlab.len = off + len;
        tlab.shared = true;
    }

    /// Retire both TLABs of a worker (called once the worker is done), and
    /// flush its per-worker copy counters (gci i26/pc,
    /// [`TlabSet::flush_copy_counters`]). Every `TlabSet` `parallel_evacuate`
    /// creates reaches this before the pause's readers run: a helper's at the
    /// end of `run_worker` and again in the driver's slot sweep, the driver's
    /// own at the end of its `run_worker`, the serial drain's after its
    /// fixpoint loop.
    pub unsafe fn retire_all(&self, tlab: &mut TlabSet) {
        self.retire_tlab(&mut tlab.survivor);
        self.retire_tlab(&mut tlab.old);
        tlab.flush_copy_counters(self.collector);
        tlab.provenance.flush();
        if !tlab.tospace.edges.is_empty() || tlab.tospace.incomplete {
            tlab.tospace.drain_into(&mut self.tospace.lock());
        }
    }

    /// g1sz: the edge of one settled slot of a to-space `holder` (region
    /// `holder_idx`), now naming `target` -- the per-slot body of
    /// `collect_outgoing_cross_region_edges_noting_young`'s `note`, for
    /// [`ClosureTospaceEdges`]. Returns whether it dirtied the holder's card,
    /// so the caller stores the card at most once per holder.
    #[inline]
    pub fn note_tospace_edge(
        &self,
        tospace: &mut ClosureTospaceEdges,
        holder_idx: usize,
        target: usize,
    ) -> bool {
        let Some(t) = self.collector.lookup_region_for_addr(target) else {
            return false;
        };
        if t == holder_idx {
            return false;
        }
        let Some(region) = self.view().get(t) else {
            tospace.incomplete = true;
            return false;
        };
        match region.region_type() {
            RegionType::HumongousStart | RegionType::Eden | RegionType::Survivor | RegionType::Old => {
                tospace.push(t, holder_idx);
                true
            }
            // A reference names an object start, so a continuation or a Free
            // region is a slot Phase 4 should judge, not this shortcut.
            RegionType::HumongousContinuation | RegionType::Free => {
                tospace.incomplete = true;
                false
            }
        }
    }

    /// Evacuate one CSet object: atomic header CAS-forwarding + TLAB copy.
    /// Returns `Some((new_ptr, fresh))` where `fresh` is true iff THIS call
    /// performed the copy (CAS winner), `None` on alloc failure / corrupt
    /// header. The from-space object's `forwarding_ptr` is the single install
    /// slot; on a CAS loss the speculatively-copied destination is abandoned
    /// (becomes unreferenced to-space garbage reclaimed next cycle) and the
    /// winner's pointer is returned so all references converge.
    #[track_caller]
    pub unsafe fn evacuate(
        &self,
        tlab: &mut TlabSet,
        old_ptr: *mut u8,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
    ) -> Option<(*mut u8, bool)> {
        self.evacuate_screened(tlab, old_ptr, forwards, objs, bytes, false)
    }

    /// [`Self::evacuate`] for a candidate whose caller's screen already
    /// proved the supply screen's two questions (`supply_proved`, from
    /// [`Self::candidate_screen`] on the same address just before): that
    /// screen is then not run a second time. g1sz: `process_object` screened
    /// every collection-set referent twice; the classification alone was 7%
    /// of a one-worker closure profile.
    #[track_caller]
    #[inline]
    pub unsafe fn evacuate_screened(
        &self,
        tlab: &mut TlabSet,
        old_ptr: *mut u8,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
        supply_proved: bool,
    ) -> Option<(*mut u8, bool)> {
        self.evacuate_screened_impl(tlab, None, old_ptr, forwards, objs, bytes, supply_proved)
    }

    #[track_caller]
    #[inline]
    pub unsafe fn evacuate_screened_cached(
        &self,
        tlab: &mut TlabSet,
        layout_cache: &mut CompactLayoutCache,
        old_ptr: *mut u8,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
        supply_proved: bool,
    ) -> Option<(*mut u8, bool)> {
        self.evacuate_screened_impl(
            tlab,
            Some(layout_cache),
            old_ptr,
            forwards,
            objs,
            bytes,
            supply_proved,
        )
    }

    #[track_caller]
    unsafe fn evacuate_screened_impl(
        &self,
        tlab: &mut TlabSet,
        layout_cache: Option<&mut CompactLayoutCache>,
        old_ptr: *mut u8,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
        supply_proved: bool,
    ) -> Option<(*mut u8, bool)> {
        // LAST-DITCH ALIGNMENT GUARD. Everything below dereferences `old_ptr`
        // as an `ObjectHeader`, and the evacuation-failure arm feeds `old_ptr`
        // itself to `ObjectHeader::make_forwarded`, whose FIRST assert is
        // "forwarding target must have its low 2 bits clear". An unaligned
        // candidate therefore does not degrade — it PANICS in a `g1-evac-N`
        // worker, and `RetireOnExit` converts that into a `main-vm` abort of
        // the whole process. That is the shape of all five Cluster-D crashes in
        // `g1-evac-forwarding-assert-and-three-sigsegv-clusters`.
        //
        // The two worker walks now screen their candidates with
        // `evacuation_candidate_is_an_object_view`, so nothing SHOULD arrive
        // here misaligned. This guard is for the three driver-side callers that
        // do not (roots, keep-alive, the self-forward drain) and for whatever
        // the next supply route turns out to be: two instructions to turn an
        // unrecoverable abort into a counted refusal and a continued pause.
        // `None` is the caller-visible "evacuation failed", which every one of
        // the six call sites already handles by leaving the slot alone.
        //
        // Gated with the rest of the parity work on
        // `CRATONVM_G1_PARALLEL_EVAC_SCREEN`, so `=0` really does restore the
        // pre-2026-09-05 behaviour — abort included. A guard that stayed armed
        // under the off word would make the switch a HALF A/B: the crash it is
        // meant to reproduce would not come back, and the arm would look like
        // evidence that the screens were not what fixed it.
        // IS THE CANDIDATE THE START OF AN OBJECT? Asked HERE, one frame above
        // every supply route, because that is the only place the answer covers
        // all of them -- and `#[track_caller]` on this function makes the
        // report name the route that offered it.
        //
        // `evacuate` sizes an object from the header at this address, copies
        // that many bytes, and installs a forwarding mark word at `addr + 8`.
        // For an INTERIOR address the third of those writes eight bytes inside
        // a LIVE object's body and leaves the word beside it untouched, which
        // is `g1-eight-byte-write-at-a-live-objects-base-20260906`'s
        // corrupt-cell family exactly. The alignment guard below is the only
        // thing this entry point used to ask, and an interior address is
        // 8-aligned.
        if self.supply_screen
            && !supply_proved
            && !self.collector.addr_is_followable_object_view(
                self.view(),
                old_ptr as usize,
                "parallel-evacuate",
                GridProof::No,
            )
        {
            let n = EVAC_SUPPLY_NON_OBJECT.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                tracing::warn!(
                    "[g1] parallel evacuation REFUSED a candidate that is not an OBJECT \
                     START (#{n}): addr=0x{:x} caller={} -- sizing an object from these \
                     bytes and installing a forwarding mark word at addr+8 is how an \
                     eight-byte `target | MARK_FORWARDED` lands inside a live object's \
                     body. The reference is left unchanged and the pause continues.",
                    old_ptr as usize,
                    std::panic::Location::caller(),
                );
            }
            return None;
        }
        if !supply_proved && self.screens_armed() && (old_ptr.is_null() || (old_ptr as usize) & 0x7 != 0) {
            let n = PARALLEL_EVAC_UNALIGNED_CANDIDATE.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                tracing::warn!(
                    "[g1] parallel evacuation REFUSED an unaligned candidate (#{n}): addr=0x{:x} — an object start is 8-aligned, so this word is not one. Evacuating it would have asserted inside `make_forwarded` and aborted the VM. The reference is left unchanged and the pause continues.",
                    old_ptr as usize,
                );
            }
            return None;
        }

        // Since the 32 -> 24 header shrink the forwarding slot IS the mark
        // word, so this is a tagged CAS rather than a zero-vs-nonzero one: the
        // unforwarded value is not 0 but "any word whose 2-bit state is not
        // `MARK_FORWARDED`" (NEUTRAL, THIN_LOCKED or INFLATED). Comparing
        // against 0 here would fail forever on any locked object and, worse,
        // would install a forward over an INFLATED word's monitor pointer.
        let mark_atomic = &(*(old_ptr as *const ObjectHeader)).mark_word;

        // Fast path: already forwarded this cycle.
        let observed = mark_atomic.load(Ordering::Acquire);

        if ObjectHeader::is_forwarded_mark(observed) {
            // VALIDATE THE DECODE. See `G1Collector::decode_forwarding_target`:
            // a word whose low two bits are `0b11` by coincidence passes
            // `is_forwarded_mark`, and its decoded "target" was being returned
            // here, stored into the caller's reference slot and pushed onto the
            // gray worklist.
            let existing = match self.peer_forwarding_target(observed, old_ptr) {
                Some(t) => t as usize,
                None => return None,
            };
            // DEFECT-2 FIX (part 1 of 2): record the forward in THIS cycle's
            // forward set even though we didn't perform the copy, so it reaches
            // `pointer_map`. The serial path dedups via the per-cycle
            // `pointer_map` (so every forward is recorded); the parallel path
            // dedups via the persistent `forwarding_ptr` header field, and a
            // fast-path hit previously returned `existing` WITHOUT recording it.
            // That left the address `old_ptr -> existing` absent from
            // `pointer_map`, so the VM's `update_all_roots` could not remap a root
            // still pointing at `old_ptr` — it stayed stuck on the from-space
            // object and dangled when the region was reused (the rare
            // `java/lang/Object`). Recording it lets the root be remapped to
            // `existing`. Safe because part 2 (clearing `forwarding_ptr` for every
            // evacuated object at cycle end) guarantees `existing` is a THIS-cycle
            // forward (a fully-scanned live copy), never a stale prior-cycle one.
            forwards.push((old_ptr as usize, existing));
            return Some((existing as *mut u8, false));
        }

        let obj_size = {
            let header = &*(old_ptr as *const ObjectHeader);
            match layout_cache {
                Some(cache) => object_total_size_cached(header, cache),
                None => object_total_size(header),
            }
        };
        if obj_size < HEADER_SIZE {
            tracing::warn!(
                "g1::evacuate(parallel): refusing to evacuate object at {:p} — \
                 implausible size {} (corrupt header)",
                old_ptr,
                obj_size
            );
            return None;
        }

        // gci i23/tn: `survivor_full` -- this worker's Survivor copy already
        // failed under the pause's Survivor region cap (HotSpot's per-worker
        // `_tenuring_threshold = 0`).
        let by_age = {
            let header = &*(old_ptr as *const ObjectHeader);
            header.gc_age() >= self.promotion_age
        };
        let mut promote = by_age || tlab.survivor_full;
        let dest_tlab = if promote {
            &mut tlab.old
        } else {
            &mut tlab.survivor
        };
        // g1pin: a peer-stack pinned object takes the evacuation-failure arm
        // below, and no destination is tried for it (`g1_pins_peer_stack_objects`).
        let pinned = self.object_pinned(old_ptr as usize);
        let mut placed = if pinned {
            None
        } else {
            self.tlab_alloc(dest_tlab, obj_size)
        };
        // A promotion the cap caused (counted on the winning arm below).
        let mut capped_promotion = promote && !by_age;
        // gci i23/tn (`CRATONVM_G1_SURVIVOR_REGION_CAP`, default ON): HotSpot's
        // `G1ParScanThreadState::allocate_in_next_plab` -- a young-destined copy
        // that found no Survivor room once the cap refused a region is copied
        // into Old (a premature promotion in every respect below: the
        // old-generation stamp, the promoted bytes), not self-forwarded.
        if !pinned && placed.is_none() && !promote && self.collector.survivor_cap_reached() {
            tlab.survivor_full = true;
            placed = self.tlab_alloc(&mut tlab.old, obj_size);
            if placed.is_some() {
                promote = true;
                capped_promotion = true;
            }
        }
        // gci i27/gs (`gci-i23tn-...` item 2; `CRATONVM_G1_SURVIVOR_OVERFLOW_TO_OLD`,
        // default OFF): the serial arm's i13/ef retry, on this arm too --
        // HotSpot's `allocate_in_next_plab` copies a young-destined object
        // into Old whenever the Survivor allocation failed (an exhausted pool
        // here, not the cap), and only then fails the evacuation. Counted on
        // the winning arm below.
        let mut overflow_promotion = false;
        if !pinned && placed.is_none() && !promote && self.collector.survivor_overflow_to_old {
            placed = self.tlab_alloc(&mut tlab.old, obj_size);
            if placed.is_some() {
                promote = true;
                overflow_promotion = true;
            }
        }
        // gci i25/pl: does this copy need the per-copy `parallel_copy_end`
        // publish below? Not when it sits in a SHARED Survivor PLAB chunk (or
        // is a direct copy into the shared region): `tlab_alloc` answers from
        // those only while the TLAB is still `shared` afterwards, and both
        // were claimed with the region's own cursor CAS, so the cursor the
        // forward-target validator reads already covers the copy. On that arm
        // every worker's copy used to `fetch_max` ONE cache line of the one
        // shared region -- see `plab_copy_end_needs_publish`.
        let (copy_end_publish, copy_end_owner, cached_target) = if let Some(new_addr) = placed {
            let dt = if promote { &tlab.old } else { &tlab.survivor };
            let in_tlab = dt.region_idx.is_some()
                && new_addr >= dt.base
                && new_addr < dt.base.saturating_add(dt.len);
            if in_tlab {
                if dt.shared && self.plab_copy_end_elide {
                    (false, false, None)
                } else {
                    let end = new_addr - dt.base + obj_size;
                    (
                        true,
                        !dt.shared && self.copy_end_owner_store,
                        dt.region_idx.map(|i| (i, end)),
                    )
                }
            } else {
                (
                    plab_copy_end_needs_publish(self.plab_copy_end_elide, dt.shared),
                    false,
                    None,
                )
            }
        } else {
            (false, false, None)
        };
        let new_addr = match placed {
            Some(a) => a,
            None => {
                // EVACUATION FAILURE (to-space pool exhausted): self-forward in
                // place rather than dropping the object (mirrors the serial path).
                // CAS the from-space header's forwarding slot to the object's OWN
                // address; the winner records an identity forward (old→old) so
                // Phase 5 keeps its region, a loser adopts whatever address won
                // (a real new location, or another self-forward). `fresh` from the
                // CAS keeps the object scanned exactly once.
                let old = old_ptr as usize;
                return match (*(old_ptr as *const ObjectHeader)).try_self_forward(observed) {
                    Ok(_) => {
                        forwards.push((old, old));
                        self.collector.note_self_forward(old);
                        // A real evacuation failure leaves slots for Phase 4
                        // to judge (`ClosureTospaceEdges`); a pin does not.
                        if !pinned {
                            tlab.tospace.incomplete = true;
                        }
                        if !ObjectHeader::is_forwarded_mark(observed) {
                            self.collector
                                .self_forward_marks
                                .lock()
                                .push((old, observed));
                        }
                        // The evacuation-FAILURE arm. The first version of this
                        // watch recorded only the normal-copy winner and so
                        // could not see the objects an OOM-ing run actually
                        // produces — this arm is the one to-space exhaustion
                        // takes, and its objects are queued and walked in
                        // place, in from-space, exactly like a copy.
                        if let Some(w) = self.copy_watch {
                            let h = &*(old_ptr as *const ObjectHeader);
                            let ridx = self
                                .collector
                                .lookup_region_for_addr(old)
                                .unwrap_or(usize::MAX);
                            let epoch = self.view().get(ridx).map(|r| r.reuse_epoch).unwrap_or(0);
                            w.note_copy(old, old, h.class_id.as_u32(), h.num_slots(), ridx, epoch);
                        }
                        Some((old_ptr, true))
                    }
                    // A racing worker got there first. Its word is FORWARDED by
                    // construction (nothing else writes this word during the
                    // pause), but decode defensively: a non-forwarded loser
                    // value would mean an unmodelled writer, and adopting its
                    // payload as an address is exactly the INFLATED/FORWARDED
                    // aliasing this encoding was audited against.
                    Err(actual)
                        if ObjectHeader::is_forwarded_mark(actual)
                            && self.peer_forwarding_target(actual, old_ptr).is_some() =>
                    {
                        let target = self
                            .peer_forwarding_target(actual, old_ptr)
                            .unwrap_or(old_ptr);
                        // THE THIRD ARM OF THE SAME SHAPE. `evacuate`'s
                        // already-forwarded fast path records the adopted
                        // forward (DEFECT-2 part 1) and its copy-path CAS-loser
                        // arm records it too (the 2026-08-26 "SECOND SITE" fix).
                        // This one — the CAS loser on the EVACUATION-FAILURE
                        // path — still returned the winner's target without
                        // recording it, so `old` reached neither `pointer_map`
                        // (nothing outside the collector could be remapped off
                        // it) nor `retire_forwards` (its tag outlived the
                        // pause, in a KEPT region by construction, where the
                        // next cycle's fast path reads it as a this-cycle
                        // answer).
                        //
                        // Recorded, not deduplicated: the winner records the
                        // same pair, and `pointer_map` is a map. Writing it
                        // from every arm is what makes "every forward this
                        // pause installed is a key of `pointer_map`" a property
                        // of the code rather than of which arm happened to run.
                        // Validated by the guard on this arm; see
                        // `decode_forwarding_target`.
                        EVACUATE_CAS_LOSER_FORWARDS.fetch_add(1, Ordering::Relaxed);
                        forwards.push((old, target as usize));
                        Some((target, false))
                    }
                    Err(_) => None,
                };
            }
        };
        let new_ptr = new_addr as *mut u8;

        #[cfg(target_arch = "x86_64")]
        // Request exclusive (E/M) ownership of old_ptr before copying so try_claim_forwarding
        // does not pay an S->M upgrade RFO right after the memcpy store buffer writes.
        core::arch::asm!(
            "prefetchw [{p}]",
            p = in(reg) old_ptr,
            options(nostack, readonly, preserves_flags),
        );

        copy_object_bytes(old_ptr, new_ptr, obj_size);

        // Atomic mark-word transfer (matches the serial T2-4 fix: the bulk
        // memcpy is UB for the AtomicU64 mark word).
        // JIT round 23 G1 evacuation page (`CRATONVM_G1_EVAC_ONE_STORE_MARK`,
        // default ON, `=0` kills): the destination's mark word is ONE plain
        // store of the snapshot with the old-generation stamp or the
        // incremented age already folded in (`ObjectHeader::mark_with_gc_flags`
        // / `mark_with_gc_age`, gen r5w6/pin10's helpers), instead of the
        // snapshot store plus a locked `fetch_or` / compare-exchange loop on
        // the line the copy just wrote. No other worker can see the copy before
        // the claim below publishes it. On `R23W6KeepLong -Xmx1g` that locked
        // `or` was 29% of this function's self time.
        let dest_mark = destination_mark(observed, promote, self.one_store_mark);
        {
            let new_h = new_ptr as *mut ObjectHeader;
            // `observed`, NOT a fresh load: the mark word is the forwarding slot
            // now, so re-reading it here could pick up a racing worker's
            // FORWARDED value and stamp the destination as forwarded. The
            // snapshot was taken before the copy and is known non-forwarded.
            (*new_h)
                .mark_word
                .store(dest_mark.unwrap_or(observed), Ordering::Relaxed);
        }

        let new_header = &mut *(new_ptr as *mut ObjectHeader);
        if dest_mark.is_none() {
            if promote {
                // G1AUD-1 — same old-generation stamp as the serial
                // `evacuate_object`; see the long note there for why the JIT's
                // inline reference-store fast paths depend on this bit.
                new_header.add_gc_flags(GC_FLAG_OLD_GEN);
            } else {
                new_header.set_gc_age(new_header.gc_age().saturating_add(1));
            }
        }
        // (No destination forwarding clear: the mark-word store above wrote
        // the known non-forwarded snapshot over whatever the memcpy carried.)

        // Install the forward on the OLD (from-space) header. Winner copies; a
        // loser abandons its `new_ptr` and adopts the winner's address.
        // Two steps since the 8-byte header: CLAIM (one CAS on the mark word,
        // to FORWARDED|BUSY), then PUBLISH (write the target into the source's
        // second word and clear BUSY). The copy above was taken before the
        // claim, so the source body it read was intact; a reader that meets
        // BUSY waits for the publish.
        let old_header = &*(old_ptr as *const ObjectHeader);
        match old_header.try_claim_forwarding(observed) {
            Ok(_) => {
                if copy_end_publish {
                    if let Some((target_idx, end)) = cached_target {
                        if let Some(region) = self.view().get(target_idx) {
                            if copy_end_owner {
                                region.publish_owned_parallel_copy_end(end);
                            } else {
                                region.publish_parallel_copy_end(end);
                            }
                        }
                    } else {
                        let target_idx =
                            (new_addr - self.collector.arena_base) >> self.collector.region_shift;
                        if let Some(region) = self.view().get(target_idx) {
                            let target_offset =
                                new_addr.saturating_sub(region.data.as_ptr() as usize);
                            let end = target_offset.saturating_add(obj_size);
                            if copy_end_owner {
                                region.publish_owned_parallel_copy_end(end);
                            } else {
                                region.publish_parallel_copy_end(end);
                            }
                        }
                    }
                }
                // g1sz: the target's provenance into this worker's batch,
                // flushed at `retire_all` (see `ProvenanceBatch`).
                old_header.publish_claimed_forwarding_batched(new_ptr, &mut tlab.provenance);
                if G1Collector::gray_prov_enabled()
                    && new_header.kind() == ObjectKind::Array
                    && new_header.element_type() == ArrayElementType::Reference
                    && new_header.array_length() >= 30_000
                {
                    eprintln!(
                        "[REF-EVAC-COPY] from={:#x} to={new_addr:#x} len={} from_ctx=[{}]",
                        old_ptr as usize,
                        new_header.array_length(),
                        self.collector
                            .describe_rejected_address_view(self.view(), old_ptr as usize)
                    );
                }
                forwards.push((old_ptr as usize, new_addr));
                *objs += 1;
                *bytes += obj_size;
                // The published PROMOTION, counted on the same arm and for the
                // same reason as the survivor histogram below: a CAS loser's
                // copy into an old TLAB is abandoned to-space garbage, and
                // counting it would inflate the figure in proportion to the
                // worker count. See `G1Collector::bytes_promoted_total`.
                // gci i26/pc: into this worker's `TlabSet`, flushed to the
                // collector's word once at `retire_all` (one contended
                // `fetch_add` per worker per pause instead of per copy).
                if promote {
                    tlab.note_promoted_bytes(obj_size);
                }
                // gci i23/tn: the cap's census, on the winning arm for the
                // same reason (a CAS loser's copy is abandoned). g1ep: into
                // this worker's `TlabSet`, flushed with the copy counters.
                // The collector-wide `fetch_add` it replaces ran on EVERY
                // copy a full Survivor cap promoted -- nearly every copy of a
                // pause on a heap that keeps its live set -- from every worker
                // on one cache line: 28% of `evacuate_screened`'s samples on
                // `G1epKeepRounds 4 -Xmx1g` (the `fetch_add`
                // `gci-i25pl-g1-parallel-copy-updates-two-global-counters-per-object`
                // retired for the age table and promoted bytes).
                if capped_promotion {
                    tlab.cap_promotions += 1;
                }
                // gci i27/gs: the overflow opt-in's census, same arm.
                if overflow_promotion {
                    tlab.overflow_promotions += 1;
                }
                // F-18 survivor-age accounting, MOVED HERE from beside the
                // `set_gc_age` above (2026-09-20).
                //
                // The histogram feeds `tenuring_threshold`, which decides at
                // what age the next pause promotes — so it has to describe the
                // copies the pause PUBLISHED. Called before the CAS, it also
                // counted every CAS LOSER: a loser's copy is abandoned
                // to-space garbage that no reference names and Phase 5 never
                // looks at, and `objs`/`bytes` two lines up have always been
                // incremented only on the winning arm. So the histogram was
                // the one survivor statistic that double-counted a two-worker
                // race on a shared object, and it double-counted it in
                // proportion to the worker count — inflating the apparent
                // survivor volume at exactly the ages a diamond-shaped graph
                // produces, which is the input a lowered tenuring threshold
                // comes out of.
                //
                // The age was already stamped into the destination header
                // above, so the value read here is the published one.
                //
                // gci i26/pc: into this worker's private table, flushed at
                // `retire_all` (`TlabSet::flush_copy_counters`); the sums the
                // readers see after the pause are unchanged.
                if !promote {
                    tlab.note_survivor_age(new_header.gc_age(), obj_size);
                }
                // Record the copy AFTER winning the CAS, so the watch holds
                // exactly the copies this pause published — a CAS loser's
                // speculative copy is abandoned to-space garbage and its later
                // contents mean nothing.
                if let Some(w) = self.copy_watch {
                    let ridx = self
                        .collector
                        .lookup_region_for_addr(new_addr)
                        .unwrap_or(usize::MAX);
                    let epoch = self.view().get(ridx).map(|r| r.reuse_epoch).unwrap_or(0);
                    w.note_copy(
                        new_addr,
                        old_ptr as usize,
                        new_header.class_id.as_u32(),
                        new_header.num_slots(),
                        ridx,
                        epoch,
                    );
                }
                Some((new_ptr, true))
            }
            // The loser must DECODE the winner's word. `compare_exchange`
            // hands back the current *mark word*, and a forwarded mark word is
            // `target | MARK_FORWARDED` (`make_forwarded`) — the tag lives in
            // the low bits. This arm used to cast that word straight to a
            // pointer, so the loser adopted an address three bytes past the
            // real one and wrote it into the parent's reference slot and onto
            // the gray queue. Symptom:
            //
            //     ObjectRef pointer not 8-byte aligned: 0x7299462265db
            //
            // (every reported address is an 8-aligned one +3). In a build
            // without that debug assertion it is a corrupt reference stored
            // into a live object.
            //
            // Only a two-worker race on the SAME object reaches this arm, which
            // is why it needed a diamond — a shared child with two parents — to
            // show up at all, and why the tree-shaped evacuation tests never
            // saw it. Decoded exactly like the evacuation-failure loser twenty
            // lines above, including its defensive check: a non-forwarded loser
            // value means an unmodelled writer, and adopting its payload as an
            // address is the INFLATED/FORWARDED aliasing this encoding was
            // audited against.
            Err(winner)
                if ObjectHeader::is_forwarded_mark(winner)
                    && self.peer_forwarding_target(winner, old_ptr).is_some() =>
            {
                let target = self
                    .peer_forwarding_target(winner, old_ptr)
                    .unwrap_or(old_ptr);
                // DEFECT-2 FIX, SECOND SITE (2026-08-26). This arm returned the
                // winner's target WITHOUT recording `old_ptr -> target` in
                // `forwards`, so the forward never reached this cycle's
                // `pointer_map` and no root naming `old_ptr` could be remapped
                // — it stayed on the from-space copy and dangled once the
                // region was reused. That is verbatim the defect part 1 fixed
                // for the fast path forty lines up; the same omission survived
                // here, on the arm only a two-worker race on ONE object can
                // reach.
                //
                // The rarity and the LOAD DEPENDENCE follow from that race:
                // more parallel evacuation means more CAS losses, which is why
                // the netty `ParameterizedSslHandlerTest` stall this was found
                // from is 1 run in 5 at load 20-27 and 0 in 122 at load 3-7.
                //
                // Part 2's invariant is unaffected: it clears `forwarding_ptr`
                // for every key in `pointer_map` at cycle end, so this key is
                // cleared too — which is what part 2 wants, not a hazard to it.
                // Validated by the guard on this arm; see
                // `decode_forwarding_target`.
                EVACUATE_CAS_LOSER_FORWARDS.fetch_add(1, Ordering::Relaxed);
                forwards.push((old_ptr as usize, target as usize));
                // Our speculative copy is abandoned, and since the 8-byte header
                // it is not guaranteed faithful: the winner writes its target
                // into the source's second word (a long header's LENGTH) right
                // after claiming, and our copy may have read it. Linear region
                // walks (card scans, `find_unretired_forwards`) size a body from
                // that word, so stamp a reference-free filler of the same
                // footprint, as `gen_evac`'s loser arm does.
                //
                // SAFETY: `new_ptr` is this worker's own freshly allocated
                // `obj_size`-byte block, referenced by nothing.
                cratonvm_gc_core::heap::write_filler(new_addr, obj_size);
                Some((target, false))
            }
            Err(_) => {
                // Not a forward this pause installed (see the guard above).
                // The speculative copy is still ours and still unreferenced;
                // keep it walkable for the same reason the loser arm does.
                cratonvm_gc_core::heap::write_filler(new_addr, obj_size);
                None
            }
        }
    }

    /// Decode the FORWARDED mark `observed` of the source `old_ptr`, waiting
    /// out a PEER's claim that has not published yet — for as long as it takes
    /// (up to the [`PEER_CLAIM_WAIT_LIMIT`] safety valve) rather than for the
    /// sub-millisecond bound [`ObjectHeader::forwarding_address`] puts on it.
    ///
    /// # Why the bound is wrong here
    ///
    /// `forwarding_address` gives up on a `FORWARDED | BUSY` word after
    /// `FORWARDING_BUSY_WAIT_LIMIT` CPU-relax rounds (a fraction of a
    /// millisecond) and answers "not forwarded", because header SCREENS call it
    /// on words that only look like a claim. The evacuator is not such a
    /// caller: with the supply screen armed its source is a proved object
    /// start, and its BUSY mark is a genuine claim a worker of this same pause
    /// publishes two stores later, with nothing between them that can fail.
    /// On an oversubscribed host the winner can be descheduled inside that
    /// window for far longer than the bound. The three arms that read a peer's
    /// forward then answered `None`, which every caller of `evacuate` treats
    /// as "leave the slot alone", and the copy-path loser left its speculative
    /// copy in to-space UNSTAMPED. That copy was taken before the claim, so it
    /// can carry the winner's target in its second word (a long header's
    /// shape word), and a mis-sized body desynchronises every linear walk of
    /// its region — Phase 4's fix-up among them, which then leaves the live
    /// objects after it holding from-space addresses. The generational
    /// evacuator found and closed the same window on 2026-09-26
    /// (`gen_evac::wait_for_claimed_forwarding`, gen r5w1/young5); this is
    /// G1's copy of that fix.
    ///
    /// Found while retiring
    /// `g1-parallel-evac-pool-exhaustion-loses-a-held-monitor` (2026-09-30),
    /// whose hang it fits (parallel arm only — the serial arm never writes
    /// BUSY — load-dependent, a stale reference into reused space) but could
    /// not be confirmed against, because that hang no longer reproduces.
    /// `PARALLEL_EVAC_CLAIM_WAITS` counts how often the window is reached.
    ///
    /// With the supply screen OFF the source is not proved, so the bounded
    /// decode is kept there.
    pub unsafe fn peer_forwarding_target(&self, observed: u32, old_ptr: *mut u8) -> Option<*mut u8> {
        if self.supply_screen && observed & cratonvm_types::MARK_FWD_BUSY != 0 {
            let mark = wait_out_peer_claim(&*(old_ptr as *const ObjectHeader));
            return self
                .collector
                .decode_forwarding_target(mark, old_ptr as usize);
        }
        self.collector
            .decode_forwarding_target(observed, old_ptr as usize)
    }

    /// Scan one already-evacuated (to-space) object's reference fields; evacuate
    /// each CSet target, rewrite the slot, and collect freshly-evacuated targets
    /// into `children`. Mirrors the serial `scan_and_evacuate_refs` slot
    /// dispatch.
    pub unsafe fn process_object(
        &self,
        tlab: &mut TlabSet,
        layout_cache: &mut CompactLayoutCache,
        obj_ptr: *mut u8,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
        children: &mut Vec<usize>,
        deferred_self_forwarded: &mut Vec<usize>,
        defer_self_forwarded: bool,
    ) {
        let header = &*(obj_ptr as *const ObjectHeader);
        // g1sz: a to-space holder's region, when the closure records its edges
        // (`ClosureTospaceEdges`). A self-forwarded holder stays in the
        // collection set and is never recorded (its pause walks anyway).
        let tospace_holder = if self.tospace_edges {
            self.collector
                .lookup_region_for_addr(obj_ptr as usize)
                .filter(|h| !self.cset.contains(h))
        } else {
            None
        };
        // Whether `note_tospace_edge` has dirtied this holder's start card
        // (once per holder is enough).
        let mut tospace_dirtied = false;
        // The collection-overlay rows of an owner copy, as extra fields
        // (`Self::expand_overlay_rows`). One relaxed gate for every other object.
        self.expand_overlay_rows(
            tlab,
            obj_ptr,
            header.class_id.as_u32(),
            forwards,
            objs,
            bytes,
            defer_self_forwarded,
            children,
            deferred_self_forwarded,
        );
        // THE HOLDER'S OWN FIRST WORD IS A POINTER INTO THIS ARENA.
        //
        // `class_id`(4) + mark word(4) is the object's first word since the
        // 8-byte header (r11w18 g1store: this said `class_id` + `shape`, the
        // pre-merge layout, and the test paired those; see
        // `header_first_word`), so this test asks whether something wrote an
        // eight-byte pointer at the holder's base. The measurement below was
        // taken on the old layout. It is not the CANDIDATE screen: candidates are
        // referents, and by the time a holder reaches this queue it was a sound
        // object when `evacuate` copied it. A corrupt holder here therefore
        // means the corruption happened AFTER the copy, i.e. inside this pause.
        //
        // Measured 2026-09-06, `TestHostConfigAutomaticDeploymentXmlExternalWarXml`
        // under G1: 19 corrupt-cell holders in one run, `word0_plausible_ptr`
        // TRUE on 19 of 19, `#[track_caller]` naming THIS walk for 12 of them.
        // Walking such a holder strides `num_slots` = the pointer's HIGH dword
        // (752 in that run) of 16-byte cells over its neighbours and rewrites
        // the ones that decode as references — so refusing here is what stops
        // one corrupted header from manufacturing the next.
        //
        // A cursor-based holder screen (`candidate_header_is_plausible`) cannot
        // be used at this site: the parallel arm carves TLABs and publishes a
        // destination region's cursor only at `retire_tlab`, so every fresh
        // to-space holder is legitimately ABOVE its region's cursor for the
        // whole dispatch. This test needs no cursor.
        if self.screens {
            if let Some(paired) = self.collector.holder_word0_arena_pointer(header) {
                let n = PARALLEL_SCAN_HOLDER_WORD0_IS_POINTER.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 8 || n.is_power_of_two() {
                    tracing::warn!(
                        "[g1] parallel ref-scan REFUSED a HOLDER whose first word is an ARENA POINTER (#{n}): holder=0x{:x} class_id={} num_slots={} paired=0x{paired:016x} mark=0x{:016x} gc_age={} — `class_id`+mark word IS the header's first word, so something wrote an 8-byte pointer at this object's base. Walking it would stride {} cells over its neighbours and rewrite them. Skipped; the pause continues.",
                        obj_ptr as usize,
                        header.class_id.as_u32(),
                        header.num_slots(),
                        header.mark_word.load(Ordering::Relaxed),
                        header.gc_age(),
                        header.num_slots(),
                    );
                }
                // QUEUE IT FOR THE END-OF-PAUSE GRID VERDICT.
                //
                // This refusal population is the one
                // `g1-eight-byte-write-at-a-live-objects-base-20260906` calls "the
                // genuine shape", and it is the one population that never got the
                // question asked of it: `grid_closes_on_cursor` and
                // `locate_in_object_grid` are only reachable with the region table
                // held, and a refusal returns immediately. So "is this address an
                // object start at all" -- the question that decides whether there
                // is a WRITER to find or a walk that arrived somewhere it should
                // not have -- was answered for the corrupt-CELL family and never
                // for this one.
                //
                // The holder's own first sixteen bytes stand in for the cell: they
                // ARE what the refusal is about.
                {
                    // SAFETY: the screen above already read this header.
                    let (w0, w1) = unsafe {
                        (
                            std::ptr::read(obj_ptr as *const u64),
                            std::ptr::read((obj_ptr as *const u64).add(1)),
                        )
                    };
                    queue_pending_corrupt_holder(
                        obj_ptr as *mut u8,
                        header,
                        obj_ptr as usize,
                        0,
                        w0,
                        w1,
                        "parallel-ref-scan-holder-word0-arena-pointer",
                    );
                }
                if tospace_holder.is_some() {
                    tlab.tospace.incomplete = true;
                }
                return;
            }
            // gci i19/yc -- THE SERIAL ARM'S SECOND HOLDER SCREEN, which this
            // arm never got. `scan_and_evacuate_refs` (serial) and
            // `fixup_walk_region` (Phase 4) both refuse a non-humongous holder
            // whose OWN header sizes it larger than a region; this arm only
            // clamped the walk to the region end (`walkable`) and then walked
            // -- and rewrote, with forwarding addresses -- every 16-byte cell
            // from the holder to that end. That is the shape of the vi17-nr
            // `nr17_jt_lt1_r4` burst (one legacy walk striding 0x480+ bytes over
            // small compact objects and a byte array). Sound to refuse: no
            // allocator places a non-humongous object larger than a region,
            // and a to-space copy is never humongous.
            if let Some(declared) = self.collector.parallel_holder_body_refused(
                self.view(),
                obj_ptr as *const u8,
                header,
            ) {
                let n = SERIAL_SCAN_HOLDER_BODY_TOO_BIG.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 8 || n.is_power_of_two() {
                    tracing::warn!(
                        "[g1] parallel ref-scan REFUSED a HOLDER whose declared body cannot FIT \
                         IN A REGION (#{n}): holder=0x{:x} class_id={} num_slots={} \
                         declared_body={declared} region_size={} -- walking it would rewrite \
                         the rest of its region. Skipped; the pause continues.",
                        obj_ptr as usize,
                        header.class_id.as_u32(),
                        header.num_slots(),
                        self.collector.config.region_size,
                    );
                }
                if tospace_holder.is_some() {
                    tlab.tospace.incomplete = true;
                }
                return;
            }
        }
        let (kind, etype, alen) = (header.kind(), header.element_type(), header.array_length());
        // PARITY WITH THE SERIAL ARM (2026-09-05). `scan_and_evacuate_refs`
        // clamps its element walk with `holder_walkable_slots` and screens
        // every candidate with `evacuation_candidate_is_an_object` before
        // `evacuate_object` dereferences it; this walk did neither, and it is
        // the DEFAULT arm. `array_length` is a u32 bounded only by `i32::MAX`,
        // so `HEADER_SIZE + len * 8` is not implied to be inside the region by
        // anything — one wrong header walked into the next region, read its
        // bytes as references, and (below) REWROTE the ones that looked like
        // CSet pointers with forwarding addresses. See
        // `g1-evac-forwarding-assert-and-three-sigsegv-clusters`.
        if kind == ObjectKind::Array {
            if etype == ArrayElementType::Reference {
                let walkable = self.walkable(obj_ptr, alen as usize, 8);
                if tospace_holder.is_some() && walkable < alen as usize {
                    tlab.tospace.incomplete = true;
                }
                // Once per holder: the header does not change during its scan.
                let extent = holder_extent(obj_ptr, header);
                // g1sz: the referents `EVAC_PREFETCH_AHEAD` elements ahead.
                if self.prefetch {
                    for i in 0..walkable.min(EVAC_PREFETCH_AHEAD) {
                        let raw = std::ptr::read(obj_ptr.add(HEADER_SIZE + i * 8) as *const u64);
                        self.prefetch_cset_referent(raw as usize);
                    }
                }
                for i in 0..walkable {
                    let slot_ptr = obj_ptr.add(HEADER_SIZE + i * 8);
                    if self.prefetch && i + EVAC_PREFETCH_AHEAD < walkable {
                        let ahead = std::ptr::read(
                            obj_ptr.add(HEADER_SIZE + (i + EVAC_PREFETCH_AHEAD) * 8) as *const u64,
                        );
                        self.prefetch_cset_referent(ahead as usize);
                    }
                    let raw: u64 = std::ptr::read(slot_ptr as *const u64);
                    if raw == 0 {
                        continue;
                    }
                    let ref_ptr = raw as usize as *mut u8;
                    // g1sz: what the slot names once this iteration is done
                    // (`None`: a collection-set referent left in place).
                    let mut settled = Some(raw as usize);
                    if let Some(ridx) = self.collector.lookup_region_for_addr(ref_ptr as usize) {
                        if self.cset.contains(&ridx) {
                            settled = None;
                            // Item 4 ordering, as on the serial arm: region
                            // membership first (cheap, and a non-CSet referent
                            // is never followed), the header screen only for a
                            // CSet resident.
                            let (ok, proved) =
                                self.candidate_screen("parallel-scan[array]", obj_ptr, i, raw as usize);
                            if !ok {
                                if tospace_holder.is_some() {
                                    tlab.tospace.incomplete = true;
                                }
                                continue;
                            }
                            if let Some((new_ptr, fresh)) = self.evacuate_screened_cached(
                                tlab, layout_cache, ref_ptr, forwards, objs, bytes, proved,
                            ) {
                                note_ref_write(
                                    slot_ptr as usize,
                                    new_ptr as u64,
                                    obj_ptr as usize,
                                    extent,
                                    8,
                                );
                                std::ptr::write(slot_ptr as *mut u64, new_ptr as u64);
                                // g1pin: a pinned referent stays put, and the
                                // edge to it is as settled as a copy's.
                                if new_ptr != ref_ptr || self.object_pinned(ref_ptr as usize) {
                                    settled = Some(new_ptr as usize);
                                }
                                if fresh {
                                    self.record_fresh_child_screened(
                                        ref_ptr,
                                        new_ptr,
                                        defer_self_forwarded,
                                        children,
                                        deferred_self_forwarded,
                                    );
                                    self.note_overlay_owner_copied(ref_ptr, new_ptr);
                                }
                            }
                        }
                    }
                    if let Some(h) = tospace_holder {
                        match settled {
                            Some(target) => {
                                if self.note_tospace_edge(&mut tlab.tospace, h, target)
                                    && !tospace_dirtied
                                {
                                    tospace_dirtied = true;
                                    self.collector.cards.dirty_addr_at_object_start(
                                        crate::g1_cards::ObjectStart::of_header(header),
                                    );
                                }
                            }
                            None => tlab.tospace.incomplete = true,
                        }
                    } else if let Some(target) = settled {
                        if let Some(tr) = self.collector.lookup_region_for_addr(target) {
                            crate::g1::lane_a_note_humongous_reference_view(
                                self.collector,
                                self.view(),
                                tr,
                            );
                        }
                    }
                }
            }
        } else {
            // COMPACT-LAYOUT PARITY (G1-9). This used to stride
            // `HEADER_SIZE + slot_idx * SLOT_SIZE` over `num_slots()` and decode
            // each 16 bytes as a `Value`, i.e. it assumed the LEGACY uniform
            // cell layout for every object. Every other reference walk in this
            // file — the serial evacuator, the Phase-4 remap, the mark scan,
            // the V7b verifier — goes through `for_each_flat_object_reference_trusting_header`,
            // which dispatches on `is_compact_object` and walks the registered
            // `CompactLayout::field_offsets`. The parallel evacuator did not,
            // and that is a real divergence, not a stylistic one:
            //
            //   * a compact object's reference fields live at PACKED offsets,
            //     so the legacy stride visits neither of them. Their referents
            //     are never evacuated and their slots are never rewritten — the
            //     holder survives into to-space still pointing into a region
            //     Phase 5 then frees. That is exactly the
            //     `[g1][SECURITY V7b] post-evacuation dangling reference` this
            //     path was reported to produce;
            //   * `num_slots()` is the hierarchy-wide FIELD COUNT, so the
            //     legacy stride also runs off the end of a compact body — a
            //     19-field compact object occupies 152 bytes but this loop
            //     addressed `16 + 19*16 = 320` of them, reading and, on a
            //     decode that happened to look like `Value::Object`, WRITING
            //     over whatever followed it in the region.
            //
            // Neither symptom is a race: it reproduces identically with
            // `CRATONVM_G1_WORKERS=1`. It stayed hidden because a compact
            // object requires a registered `CompactLayout`, which only a real
            // class does — the gc unit tests allocate through
            // `alloc_object(ClassId, n)` with no layout registered, so their
            // objects are legacy-layout and this loop was accidentally correct
            // for every one of them.
            // Same two additions as the array arm above, for the same reason
            // and with the same stride argument the serial arm makes: a legacy
            // 16-byte `Value` slot leaves the region TWICE as fast as an 8-byte
            // array element, and this walk WRITES.
            let declared_slots = header.num_slots() as usize;
            let walkable = if cratonvm_types::is_compact_object(header) {
                declared_slots
            } else {
                self.walkable(obj_ptr, declared_slots, SLOT_SIZE)
            };
            // g1sz: Phase 4's clamp test (`clamped` in
            // `collect_outgoing_cross_region_edges_noting_young`); a compact
            // holder's walk ignores the cap.
            if tospace_holder.is_some() && walkable < declared_slots {
                tlab.tospace.incomplete = true;
            }
            let extent = holder_extent(obj_ptr, header);
            let inline_refs = if cratonvm_types::is_compact_object(header) {
                layout_cache.get_inline(header.class_id.as_u32(), declared_slots as u32)
            } else {
                None
            };
            let mut visit_slot = |slot_ptr: *mut u8,
                                  raw: usize,
                                  compact: bool,
                                  layout_cache: &mut CompactLayoutCache| {
                    let ref_ptr = raw as *mut u8;
                    // g1sz: what the slot names once this call is done
                    // (`None`: a collection-set referent left in place).
                    let mut settled = Some(raw);
                    if let Some(ridx) = self.collector.lookup_region_for_addr(raw) {
                        if self.cset.contains(&ridx) {
                            settled = None;
                            let (ok, proved) = self.candidate_screen(
                                "parallel-scan[object]",
                                obj_ptr,
                                (slot_ptr as usize).wrapping_sub(obj_ptr as usize),
                                raw,
                            );
                            if !ok {
                                if tospace_holder.is_some() {
                                    tlab.tospace.incomplete = true;
                                }
                                return;                            }
                            if let Some((new_ptr, fresh)) = self.evacuate_screened_cached(
                                tlab, layout_cache, ref_ptr, forwards, objs, bytes, proved,
                            ) {
                                write_flat_object_reference_watched(
                                    slot_ptr,
                                    new_ptr as usize,
                                    compact,
                                    obj_ptr,
                                    extent,
                                );
                                // g1pin: a pinned referent stays put, and the
                                // edge to it is as settled as a copy's.
                                if new_ptr != ref_ptr || self.object_pinned(ref_ptr as usize) {
                                    settled = Some(new_ptr as usize);
                                }
                                if fresh {
                                    self.record_fresh_child_screened(
                                        ref_ptr,
                                        new_ptr,
                                        defer_self_forwarded,
                                        children,
                                        deferred_self_forwarded,
                                    );
                                    self.note_overlay_owner_copied(ref_ptr, new_ptr);
                                }
                            }
                        }
                    }
                    if let Some(h) = tospace_holder {
                        match settled {
                            Some(target) => {
                                if self.note_tospace_edge(&mut tlab.tospace, h, target)
                                    && !tospace_dirtied
                                {
                                    tospace_dirtied = true;
                                    self.collector.cards.dirty_addr_at_object_start(
                                        crate::g1_cards::ObjectStart::of_header(header),
                                    );
                                }
                            }
                            None => tlab.tospace.incomplete = true,
                        }
                    } else if let Some(target) = settled {
                        if let Some(tr) = self.collector.lookup_region_for_addr(target) {
                            crate::g1::lane_a_note_humongous_reference_view(
                                self.collector,
                                self.view(),
                                tr,
                            );
                        }
                    }
            };
            if let Some(refs) = inline_refs {
                // Single pass over the inline descriptor: load each non-null slot once,
                // overlap referent header prefetches across fields, then evacuate from stack.
                let mut cached = [(std::ptr::null_mut::<u8>(), 0usize); 8];
                let mut n = 0usize;
                for &disp in refs.disps.iter().take(refs.ref_len as usize) {
                    let slot = obj_ptr.add(disp as usize);
                    let raw = std::ptr::read(slot as *const u64) as usize;
                    if raw != 0 {
                        if self.prefetch {
                            self.prefetch_cset_referent(raw);
                        }
                        cached[n] = (slot, raw);
                        n += 1;
                    }
                }
                for &(slot_ptr, raw) in &cached[..n] {
                    visit_slot(slot_ptr, raw, true, layout_cache);
                }
            } else {
                if self.prefetch {
                    for_each_flat_object_reference_cached(
                        obj_ptr,
                        header,
                        0,
                        walkable,
                        layout_cache,
                        |_, raw, _, _| self.prefetch_cset_referent(raw),
                    );
                }
                for_each_flat_object_reference_cached(
                    obj_ptr,
                    header,
                    0,
                    walkable,
                    layout_cache,
                    visit_slot,
                );
            }
        }
    }

    /// Seed phase (PARALLEL arm) -- walk a non-CSet remembered-set source
    /// region, evacuate every CSet-bound reference and rewrite the slot in
    /// place, publishing freshly-evacuated targets onto the shared work queue.
    ///
    /// # This is a thin wrapper, and that is the point
    ///
    /// The walk itself is [`walk_source_region_for_cset_refs`], which the
    /// SERIAL arm (`G1Collector::scan_source_region_for_cset_refs`) also calls.
    /// This function and that one used to be separate bodies, each carrying a
    /// comment saying the two had to agree -- because
    /// `CRATONVM_G1_PARALLEL_EVAC` is supposed to change how FAST a pause runs,
    /// not what it FINDS. Nothing enforced it. Defect G1-9 was a divergence
    /// between them, the 2026-09-20 audit found five more, and three of those
    /// changed what the pause found on THIS arm -- the one every young pause
    /// takes. See [`SourceWalkArm`] for the list and for the five things that
    /// genuinely still differ.
    ///
    /// F-05 -- `card_screen` is the serial walker's parameter with the same
    /// meaning; see `G1Collector::card_screen_for_source` for why a JIT-pinned
    /// source passes `false`.
    ///
    /// `cursor_limit` is the PRE-DISPATCH cursor when seed helpers are running
    /// beside this walk, and `None` for the single-threaded driver seed; see
    /// [`walk_source_region_for_cset_refs`] for why a walk that read the live
    /// cursor could sweep over to-space objects another worker is still
    /// copying in.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn seed_source_region(
        &self,
        source_idx: usize,
        tlab: &mut TlabSet,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
        deferred_self_forwarded: &mut Vec<usize>,
        card_screen: bool,
        cursor_limit: Option<usize>,
    ) {
        let mut arm = ParallelSourceWalk {
            shared: self,
            tlab,
            forwards,
            objs,
            bytes,
            deferred_self_forwarded,
            newly: Vec::new(),
            layout_cache: crate::g1::value_from_unaligned_ptr::CompactLayoutCache::default(),
        };
        // SAFETY: the driver holds the regions guard for the whole dispatch, so
        // the region TABLE is quiescent; `cursor_limit` is what bounds the walk
        // to bytes no concurrent worker is writing.
        walk_source_region_for_cset_refs(
            self.collector,
            &mut arm,
            self.cset,
            self.jit_skips,
            source_idx,
            card_screen,
            cursor_limit,
            self.precise_rset_liveness
                .filter(|_| !self.wholesale_sources.contains(&source_idx)),
        );
        arm.publish();
    }

    /// Worker drain loop: pop gray objects, scan + evacuate their refs, push
    /// fresh children, until `outstanding` reaches 0. Retires its TLABs on exit.
    ///
    /// # The shared queue is the scaling wall, and this is where it is paid
    ///
    /// Every gray object used to cost TWO acquisitions of one process-wide
    /// `Mutex<Vec<usize>>`: one to pop the parent, one to push its children.
    /// On the 32-CPU box this collector's ergonomic sizes at 23 workers, that
    /// is 46 acquisitions of a single lock per object copied, on a phase whose
    /// per-object work is a few hundred nanoseconds of pointer chasing. The
    /// lock is not merely contended at that width, it is the critical path —
    /// which is the concrete reason the measured benefit of adding evacuation
    /// workers has been so much smaller than the worker count.
    ///
    /// So each worker now keeps a LOCAL stack and touches the shared queue
    /// only at the two boundaries that matter:
    ///
    ///   * when its local stack runs dry it lifts a BATCH — roughly one
    ///     worker's fair share, capped — instead of a single item;
    ///   * when its local stack grows past [`EVAC_LOCAL_PUBLISH_HIGH`] it
    ///     spills the OLDEST half back to the shared queue, which is what keeps
    ///     a worker that happens to find a wide subtree from hoarding it while
    ///     its neighbours spin;
    ///   * when a peer is idle and the shared queue is empty, it gives away
    ///     its oldest entries below that valve too, one per idle peer and at
    ///     most half its stack (`CRATONVM_G1_EVAC_IDLE_SPILL`, default ON).
    ///
    /// The third rule is for a live set that hangs off ONE root and is walked
    /// depth-first. The stack then holds the un-popped siblings along the
    /// current path, about the tree's depth, and never reaches the valve: on
    /// binary trees at depth 20 (one young pause, 2.1M objects, 51 MB) the
    /// driver scanned 99.1% of all objects while seven helpers idled. On such
    /// a walk the oldest entries are the siblings nearest the root, i.e. the
    /// largest unexplored subtrees, which is what an idle peer needs (the w7w
    /// page's "a spill carries no bytes" is about a spine whose oldest entries
    /// are leaves; one idle peer lifting a leaf costs it one scan).
    ///
    /// Popping locally from the BACK and spilling from the FRONT is deliberate:
    /// the back is the deepest, most recently touched part of the frontier
    /// (warm in cache, and the shape that keeps the gray set small), while the
    /// front is the oldest and the most likely to be a genuinely independent
    /// subtree for another worker.
    ///
    /// THE TERMINATION PROTOCOL IS UNCHANGED, and this is the property to
    /// check before touching this loop: every address sitting in a local stack
    /// contributed `+1` to `outstanding` when it was discovered, and that `+1`
    /// is retired only by `RetireOnExit` once the address has been scanned. So
    /// `outstanding == 0` still implies "no item is queued anywhere, locally or
    /// globally, and none is in flight" — the local stacks do not create a
    /// state in which a worker can exit while holding work.
    ///
    /// `CRATONVM_G1_EVAC_LOCAL_QUEUE=0` restores the take-one/push-all
    /// behaviour. It changes only the ORDER in which the closure visits the
    /// frontier — never which objects it reaches — so it is a throughput lever,
    /// not a semantics one; it exists because a change to a worker loop is
    /// exactly the kind of thing a later bisect wants to be able to switch off
    /// in one binary.
    pub unsafe fn run_worker(
        &self,
        tlab: &mut TlabSet,
        objs: &mut usize,
        bytes: &mut usize,
        forwards: &mut Vec<(usize, usize)>,
        deferred_self_forwarded: &mut Vec<usize>,
        census: &mut EvacWorkerCensus,
    ) {
        let mut children: Vec<usize> = Vec::new();
        let mut local: Vec<usize> = Vec::new();
        let batching = true;
        // The idle-peer spill (see the fn note): this worker is counted in
        // `idle_peers` from its first empty probe until it finds work again.
        let idle_spill = batching && self.idle_spill;
        let mut counted_idle = false;
        let idle_sleep = self.idle_sleep;
        // Lane D (2026-09-20) — the engagement census; see [`EvacWorkerCensus`].
        //
        // Off restores the pre-census loop exactly: every counter below is
        // behind this bool, including the `Instant` pair, so `=0` is a real
        // bisection arm and not merely a quieter report. See
        // [`evac_census_enabled`] for the measurement that put the default on.
        let counting = evac_census_enabled();
        // LANE W7-W (2026-09-21) — the valve's two distributions. Worker-local
        // for the whole drain loop and merged into the process table once, on
        // the way out; nothing here touches a shared line per object. See
        // [`EvacShareCensus`].
        let sharing = self.share_census;
        let mut share_census = EvacShareCensus::default();
        // Start of the CURRENT idle episode, if this worker is in one. One
        // clock read when the worker runs dry and one when it finds work
        // again — NOT one per `spin_loop` probe, which at
        // `EVAC_IDLE_SPINS_BEFORE_YIELD` probes per yield would put a
        // `QueryPerformanceCounter` in the tightest loop in the pause.
        let mut idle_since: Option<std::time::Instant> = None;
        // A worker's fair share of a queue of `n`, so a small frontier is not
        // swallowed whole by the first worker to reach it and a large one is
        // not handed out an item at a time.
        let share = |n: usize| -> usize {
            (n.div_ceil(self.workers.max(1))).clamp(1, EVAC_LOCAL_BATCH_MAX)
        };
        // How many consecutive empty probes this worker has made. Spinning on
        // `yield_now` alone hands the CPU back to the scheduler on every probe,
        // which on Windows is a syscall; a short `spin_loop` first covers the
        // overwhelmingly common case (a neighbour is microseconds from pushing).
        let mut idle_spins = 0u32;
        // JIT round 23 wave 6, lane L4 (`CRATONVM_G1_EVAC_BATCHED_OUTSTANDING`,
        // default ON, `=0` kills; only with the local stack): the termination
        // counter is no longer touched per object. `outstanding` was one
        // `fetch_sub` per scanned object and one `fetch_add` per child list,
        // on ONE cache line every worker hits -- the evacuator's per-object
        // cost grew with the worker count (`R23W3Keep obj -Xmx1g`: closure time
        // flat from 2 to 23 workers). Batched, a worker HOLDS `held` units of
        // `outstanding` for the items on its local stack and the one it is
        // scanning, and settles them only when it must:
        // * while its stack is non-empty it keeps `held >= 1`, so
        //   `outstanding > 0` and no peer can see the closure as finished;
        // * before giving items to the shared queue it tops `held` up to its
        //   stack depth (`settle`), and the given items take their units with
        //   them (a lifter adopts them, `held += take`), so every queued
        //   address has contributed its `+1`, as the protocol requires;
        // * when its stack runs dry it releases everything it holds, BEFORE
        //   it reads `outstanding == 0` -- so the counter reaches 0 exactly
        //   when no worker holds work and nothing is queued, as before;
        // * an unwind releases what it holds (`ReleaseHeld`), so a panicking
        //   worker cannot wedge its peers on a count nobody will retire.
        let batched = batching;
        let held = std::cell::Cell::new(0usize);
        struct ReleaseHeld<'r> {
            outstanding: &'r AtomicUsize,
            held: &'r std::cell::Cell<usize>,
        }
        impl Drop for ReleaseHeld<'_> {
            fn drop(&mut self) {
                let h = self.held.replace(0);
                if h > 0 {
                    self.outstanding.fetch_sub(h, Ordering::AcqRel);
                }
            }
        }
        let _release_held = ReleaseHeld {
            outstanding: self.outstanding,
            held: &held,
        };
        // Make `held` equal to the stack depth `depth` (top up first, so the
        // counter never under-reads work this worker is about to give away).
        let settle = |depth: usize| {
            let h = held.get();
            if depth > h {
                self.outstanding.fetch_add(depth - h, Ordering::AcqRel);
            } else if depth < h {
                self.outstanding.fetch_sub(h - depth, Ordering::AcqRel);
            }
            held.set(depth);
        };
        let mut layout_cache = CompactLayoutCache::default();
        loop {
            // Local work first. A non-empty local stack implies
            // `outstanding > 0` by the protocol above, so the termination test
            // does not need to be re-asked for it.
            let item = match local.pop() {
                Some(addr) => Some(addr),
                None => {
                    // Batched: the stack is dry, so release every unit held
                    // BEFORE asking whether the closure is over.
                    if batched {
                        settle(0);
                    }
                    if self.outstanding.load(Ordering::Acquire) == 0 {
                        break;
                    }
                    // JIT round 23 wave 8, lane L9
                    // (`CRATONVM_G1_EVAC_IDLE_TRY_LOCK`, OPT-IN `=1` since
                    // its n23-w8b A/B): once a worker is in an idle episode
                    // (`idle_spins > 0`), a probe that finds the queue held
                    // counts as an empty probe instead of queueing on the
                    // mutex. Up to `workers - 1` idle peers used to take the
                    // lock on every probe, convoying in front of the busy
                    // worker that wanted it to spill or give work away. The
                    // first probe of each episode still blocks, and a held
                    // lock means its holder is making progress, so no item is
                    // stranded: `outstanding > 0` keeps this worker probing.
                    let mut probe = if self.idle_try_lock && idle_spins > 0 {
                        self.queue.try_lock()
                    } else {
                        Some(self.queue.lock())
                    };
                    // `n > 0` only when the probe holds the queue.
                    let n = probe.as_ref().map_or(0, |q| q.len());
                    if n == 0 {
                        None
                    } else if !batching {
                        let q = probe.as_mut().expect("n > 0: the probe holds the queue");
                        if counting {
                            census.lifts += 1;
                            census.lifted += 1;
                        }
                        q.pop()
                    } else {
                        let q = probe.as_mut().expect("n > 0: the probe holds the queue");
                        let take = share(n);
                        if counting {
                            census.lifts += 1;
                            census.lifted += take as u64;
                        }
                        local.extend(q.drain(n - take..));
                        // Batched: the lifted items' units were counted when
                        // they were queued; this worker now holds them.
                        if batched {
                            held.set(held.get() + take);
                        }
                        // A lift is the other place `local` grows, and
                        // `residue_max` claims to be the deepest the stack
                        // ever got. It cannot reach the valve from here — a
                        // lift happens only when the stack is EMPTY and takes
                        // at most `EVAC_LOCAL_BATCH_MAX` (64) — but a maximum
                        // that skipped a growth site would be a maximum with
                        // an asterisk. Not bucketed: the spill test is not
                        // applied here, so a lift is not a sample of the
                        // distribution the histogram is about.
                        if sharing {
                            share_census.residue_max =
                                share_census.residue_max.max(local.len() as u64);
                        }
                        local.pop()
                    }
                }
            };
            match item {
                Some(addr) => {
                    // Retire this item's outstanding count on EVERY exit from
                    // the block, including an unwind out of `process_object`.
                    //
                    // A bare `fetch_sub` at the end of the arm is skipped by a
                    // panic, and the leak is unrecoverable: the count never
                    // reaches 0, so every other worker spins in the `None` arm
                    // below forever, the driver never returns from
                    // `thread::scope`, and the panic that started it is never
                    // propagated. A crash silently became an unkillable hang
                    // that also hid its own cause — measured at 3h08m of CPU
                    // for a suite that finishes in 2.5s. This is
                    // defence-in-depth for the decode bug fixed in `evacuate`
                    // above: with the guard, a future panic here FAILS the
                    // collection loudly instead of wedging the process.
                    struct RetireOnExit<'r>(&'r AtomicUsize);
                    impl Drop for RetireOnExit<'_> {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    // Batched: the item's unit stays in `held` (see above).
                    let _retire = (!batched).then(|| RetireOnExit(self.outstanding));

                    if counted_idle {
                        self.idle_peers.fetch_sub(1, Ordering::Relaxed);
                        counted_idle = false;
                    }
                    idle_spins = 0;
                    // Close the idle episode, if there was one. Charged here
                    // rather than at the probe so the census measures the
                    // WAIT, not the sampling of it.
                    if let Some(since) = idle_since.take() {
                        census.idle_ns += since.elapsed().as_nanos() as u64;
                    }
                    if counting {
                        census.scanned += 1;
                    }
                    children.clear();
                    self.process_object(
                        tlab,
                        &mut layout_cache,
                        addr as *mut u8,
                        forwards,
                        objs,
                        bytes,
                        &mut children,
                        deferred_self_forwarded,
                        true,
                    );
                    // LANE W7-W — the child-list width half of the valve's
                    // input, sampled on EVERY scan including the zero-child
                    // ones. A histogram that counted only the scans that
                    // pushed something would report the distribution of
                    // branching among branching objects, which is not the
                    // distribution the valve sees.
                    if sharing {
                        let c = children.len();
                        share_census.scans += 1;
                        share_census.children_total += c as u64;
                        share_census.children_max = share_census.children_max.max(c as u64);
                        share_census.child_hist[evac_share_bucket(c)] += 1;
                        if !batching {
                            share_census.scans_without_local_stack += 1;
                        }
                    }
                    if !children.is_empty() {
                        // Add children to the outstanding count BEFORE retiring
                        // the parent so the counter never transiently hits 0
                        // while work remains. `_retire` drops at the end of this
                        // block, i.e. after this add — the ordering the comment
                        // asks for is preserved.
                        // Batched: the children stay on this worker's stack,
                        // covered by `held >= 1`; no shared write.
                        if !batched {
                            self.outstanding.fetch_add(children.len(), Ordering::AcqRel);
                        }
                        if !batching {
                            let mut q = self.queue.lock();
                            q.extend(children.iter().copied());
                        } else {
                            local.extend(children.iter().copied());
                            // LANE W7-W — the OTHER half, and the one the
                            // constant is actually compared against. Sampled
                            // here, immediately after the extend and before
                            // any drain, because that is the instant the
                            // stack is at its deepest and the instant the
                            // valve's test is applied to it.
                            if sharing {
                                let depth = local.len();
                                share_census.residue_max =
                                    share_census.residue_max.max(depth as u64);
                                share_census.residue_hist[evac_share_bucket(depth)] += 1;
                                if depth > EVAC_LOCAL_PUBLISH_HIGH {
                                    share_census.spills += 1;
                                    // THE DISCRIMINATOR. If the child list
                                    // that triggered this spill was itself
                                    // over the threshold, W6-P's account
                                    // explains it. If it was not, the stack
                                    // crossed the line by accumulating
                                    // un-popped siblings, and child-list
                                    // width is not what engaged the valve.
                                    if children.len() > EVAC_LOCAL_PUBLISH_HIGH {
                                        share_census.spills_from_one_wide_object += 1;
                                    } else {
                                        share_census.spills_from_accumulation += 1;
                                    }
                                }
                            }
                            if local.len() > EVAC_LOCAL_PUBLISH_HIGH {
                                // Give away the OLDEST half; see the fn note.
                                let spill = local.len() / 2;
                                if counting {
                                    census.spills += 1;
                                }
                                // Batched: the given items must carry their
                                // units into the queue.
                                if batched {
                                    settle(local.len());
                                }
                                let mut q = self.queue.lock();
                                q.extend(local.drain(..spill));
                                if batched {
                                    held.set(held.get() - spill);
                                }
                            } else if idle_spill
                                && local.len() >= 2
                                && self.idle_peers.load(Ordering::Relaxed) > 0
                            {
                                // A peer is starving below the valve: give it
                                // the OLDEST entries, one per idle peer and at
                                // most half the stack, if the shared queue is
                                // empty (a non-empty queue already feeds it).
                                // `try_lock`: a busy queue means somebody is
                                // lifting or spilling right now.
                                if let Some(mut q) = self.queue.try_lock() {
                                    if q.is_empty() {
                                        let give = (local.len() / 2)
                                            .min(self.idle_peers.load(Ordering::Relaxed))
                                            .max(1);
                                        if batched {
                                            settle(local.len());
                                        }
                                        q.extend(local.drain(..give));
                                        if batched {
                                            held.set(held.get() - give);
                                        }
                                        if counting {
                                            census.spills += 1;
                                            census.idle_spills += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                None => {
                    if idle_spill && !counted_idle {
                        self.idle_peers.fetch_add(1, Ordering::Relaxed);
                        counted_idle = true;
                    }
                    // Queue momentarily empty but work still outstanding
                    // (another worker is mid-scan and about to push). Spin
                    // briefly before conceding the core: the window is usually
                    // a few hundred nanoseconds wide and `yield_now` is a
                    // syscall.
                    idle_spins = idle_spins.saturating_add(1);
                    if counting {
                        census.idle_probes += 1;
                        if idle_since.is_none() {
                            idle_since = Some(std::time::Instant::now());
                        }
                    }
                    if idle_spins < EVAC_IDLE_SPINS_BEFORE_YIELD {
                        std::hint::spin_loop();
                    } else if idle_sleep
                        && idle_spins
                            >= EVAC_IDLE_SPINS_BEFORE_YIELD + EVAC_IDLE_YIELDS_BEFORE_SLEEP
                    {
                        // Long idle: stop competing for the CPU the workers
                        // that hold the work need (see `EVAC_IDLE_SLEEP`).
                        std::thread::sleep(EVAC_IDLE_SLEEP);
                    } else {
                        std::thread::yield_now();
                    }
                }
            }
        }
        // A worker that breaks out of the loop from inside an idle episode
        // (the `outstanding == 0` arm is reached through it) has to charge the
        // tail, or the LAST wait of every pause — which is the one the
        // termination protocol is judged on — would be the only one never
        // counted.
        if let Some(since) = idle_since.take() {
            census.idle_ns += since.elapsed().as_nanos() as u64;
        }
        if counted_idle {
            self.idle_peers.fetch_sub(1, Ordering::Relaxed);
        }
        // LANE W7-W — one lock acquisition per worker per pause, after the
        // drain loop has finished with it. `EvacWorkerCensus` is published by
        // the DRIVER after the barrier and this one is published by the worker
        // itself, because unlike that table this one is not partitioned by
        // worker slot: the question it answers is about the GRAPH, and summing
        // the workers is the reading.
        if sharing {
            publish_evac_share_census(&share_census);
        }
        self.retire_all(tlab);
    }

    /// Drain evacuation-failed objects after the parallel closure is complete.
    ///
    /// A self-forwarded object stays in its original CSet region, so scanning it
    /// rewrites that from-space object's slots in place. Doing that rewrite in a
    /// worker can race another worker that is still copying the same object as a
    /// source. The driver therefore scans these in-place objects only after all
    /// parallel workers have stopped.
    /// `scan` is the persistent cursor into `work`: entries before it were
    /// already scanned by an earlier drain round of the same collection (the
    /// caller loops this to a fixpoint with `append_kept_cset_region_objects`,
    /// which can only grow `work`). The caller retires `tlab` once the
    /// fixpoint is reached — NOT here — so successive rounds keep filling the
    /// same partially-used to-space region instead of burning one per round.
    pub unsafe fn drain_deferred_self_forwarded(
        &self,
        tlab: &mut TlabSet,
        forwards: &mut Vec<(usize, usize)>,
        objs: &mut usize,
        bytes: &mut usize,
        work: &mut Vec<usize>,
        scan: &mut usize,
    ) {
        let mut ignored_deferred = Vec::new();
        let mut layout_cache = CompactLayoutCache::default();
        while *scan < work.len() {
            let addr = work[*scan];
            *scan += 1;
            self.process_object(
                tlab,
                &mut layout_cache,
                addr as *mut u8,
                forwards,
                objs,
                bytes,
                work,
                &mut ignored_deferred,
                false,
            );
        }
    }
}

/// Configuration for the G1 garbage collector.
#[derive(Debug, Clone)]
pub struct G1CollectorConfig {
    /// Total heap size in bytes (default 256 MB). This is `-Xmx`: since F-16
    /// it is the size of the RESERVATION — address space — not of the memory
    /// charged to the process at startup.
    pub heap_size: usize,
    /// F-16 — bytes to commit up front (`-Xms`). `0` means "use the
    /// ergonomic", which is a sixteenth of the heap with a four-region floor.
    ///
    /// Before F-16 the whole of `heap_size` was allocated and zeroed in the
    /// constructor and `-Xms` was parsed and discarded, so `-Xmx16g` charged
    /// 16 GiB against the process whether or not a byte was used.
    pub initial_heap_size: usize,
    /// Region size in bytes (default 1 MB).
    pub region_size: usize,
    /// Target maximum GC pause in milliseconds (default 200).
    pub max_gc_pause_ms: u64,
    /// Initiating heap occupancy percent (**default 70**; raised from
    /// HotSpot's 45 because the default heap is 256 MiB — see the initializer
    /// in `impl Default`, which carries the reasoning).
    ///
    /// LANE E wave 2, small finding 1: this said "default 45" while the
    /// initializer three dozen lines below set 70 and explained why. The flag
    /// documentation under `docs/config/` is GENERATED from these doc comments,
    /// so the stale number was being published, not merely written down.
    pub ihop_percent: u8,
    /// Tenuring threshold: survive this many young GCs before promotion (default 15).
    pub promotion_age: u8,
    /// Number of parallel GC worker threads, or **0 for the machine-derived
    /// ergonomic** (the default since F-13; see
    /// [`ergonomic_gc_worker_threads`]).
    ///
    /// This used to default to a literal `4`, and `parallel_worker_count` took
    /// `min(4, available_parallelism())` — so on a 32-core host G1 evacuated
    /// with four threads and there was no way, short of the
    /// `CRATONVM_G1_WORKERS` diagnostic override, to say otherwise. A fixed
    /// small number is a floor for a laptop and a ceiling for a server; 0 means
    /// "ask the machine", and a non-zero value is an explicit request
    /// (`-XX:ParallelGCThreads`) that is honoured even above the hardware count.
    pub gc_worker_threads: usize,
    /// Enable string deduplication (default false).
    pub string_dedup_enabled: bool,
    /// Target number of mixed GC cycles after marking (default 8).
    pub mixed_gc_count_target: u8,
    /// Maximum percentage of old regions to include per mixed GC (default 10).
    pub old_cset_region_threshold_percent: u8,
    /// Ten-findings item 3 — an Old region at or above this percent live is
    /// never a mixed-collection candidate (default 85; HotSpot's
    /// `G1MixedGCLiveThresholdPercent`). Evacuating it would be nearly a full
    /// copy for almost no reclaim.
    pub mixed_gc_live_threshold_percent: u8,
    /// Item 3 — the mixed phase ends early once the garbage held by the
    /// remaining candidates is below this percent of the heap (default 5;
    /// HotSpot's `G1HeapWastePercent`).
    pub heap_waste_percent: u8,
}

impl Default for G1CollectorConfig {
    fn default() -> Self {
        Self {
            heap_size: 256 * 1024 * 1024,
            // F-16: 0 = the ergonomic. See the field doc.
            initial_heap_size: 0,
            region_size: 1024 * 1024,
            max_gc_pause_ms: 200,
            // T19.3.G1: raised from 45 → 70 so static-init bursts
            // on small heaps don't fire a concurrent marking cycle
            // before the heap has actually retained anything worth
            // collecting. HotSpot defaults 45% but assumes a 32 GiB
            // heap where 45% = ~14 GiB; at 256 MiB (our default)
            // 45% is only 115 MiB, which a Quarkus static-init
            // replay can chew through in under two seconds.
            // Adaptive IHOP (see `update_ihop`) still adjusts
            // downward under real memory pressure.
            ihop_percent: 70,
            promotion_age: 15,
            // F-13: 0 = derive from the machine. See the field doc.
            gc_worker_threads: 0,
            string_dedup_enabled: false,
            mixed_gc_count_target: 8,
            old_cset_region_threshold_percent: 10,
            mixed_gc_live_threshold_percent: 85,
            heap_waste_percent: 5,
        }
    }
}

// ---------------------------------------------------------------------------
// G1 Region
// ---------------------------------------------------------------------------

/// Non-owning view of one region's backing slice inside
/// [`G1Collector::arena`].
///
/// `base` is the slice's start address in the arena and `len` is
/// `region_size`. `base` is stored as a `usize` (not a raw pointer) so that
/// `G1Region` stays `Send + Sync` exactly as it did with the old
/// `data: Vec<u8>` field. `Deref`/`DerefMut` expose the slice, so every
/// existing `region.data.as_ptr()` / `.len()` / `.fill(0)` / indexing call
/// site keeps working unchanged.
///
/// IMPORTANT: cross-region access (a humongous object whose payload extends
/// past this region's `len` into the physically-adjacent next region) must go
/// through [`RegionBuf::addr`] integer arithmetic, NOT through the `Deref`
/// slice — `.as_ptr().add(off)` past `len` would be out of the slice's
/// provenance. The arena guarantees those bytes are contiguous and live.
pub struct RegionBuf {
    pub base: usize,
    pub len: usize,
}

impl RegionBuf {
    #[inline]
    pub fn new(base: usize, len: usize) -> Self {
        Self { base, len }
    }
    /// Start address of this region's slice, as a `usize`. Use this (plus an
    /// offset cast to `*mut u8`) for any access that may cross into the
    /// adjacent region (humongous payloads); the arena keeps the bytes
    /// contiguous and live.
    #[inline]
    pub fn addr(&self) -> usize {
        self.base
    }
}

impl Deref for RegionBuf {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        // SAFETY: `[base, base+len)` is a live, per-region-exclusive slice of
        // the collector's `arena` (allocated once, never freed/reallocated for
        // the collector's lifetime); standalone test regions point at a leaked
        // boxed slice, equally stable. Region ranges never overlap.
        unsafe { std::slice::from_raw_parts(self.base as *const u8, self.len) }
    }
}

impl DerefMut for RegionBuf {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: see `deref`; `&mut self` gives unique access to this region's
        // disjoint arena range.
        unsafe { std::slice::from_raw_parts_mut(self.base as *mut u8, self.len) }
    }
}

/// Enhanced region descriptor for the G1 collector.
pub struct G1Region {
    /// Current region classification.
    ///
    /// # W3-A — why this is atomic, and why the field is private
    ///
    /// It was a plain `RegionType`, and it was a DATA RACE on the default young
    /// path. `SharedEvac::tlab_alloc` retypes a region it has just claimed out
    /// of the Free pool, on one evacuation worker, through a `&mut G1Region`
    /// formed from the raw regions base — while other workers are reading the
    /// type of that same region with no synchronisation at all:
    /// `shared_dest_alloc` scans EVERY region index looking for a destination,
    /// `claim_resume_region` tests `!= Old`, the pool-exhaustion census sweeps
    /// the whole table, and every screen reached through `SharedEvac::view()`
    /// (`classify_candidate_header_view`, `holder_walkable_slots_view`,
    /// `addr_is_followable_object_view`, …) resolves an address to a region and
    /// asks what type it is. Those reads are reads of OTHER workers' regions by
    /// construction — that is what they are for — so the module's disjointness
    /// discipline (see the SAFETY MODEL note) never covered them.
    ///
    /// That is undefined behaviour in the Rust/LLVM model, not a stale read
    /// with a bounded cost, and its consequence is not theoretical either:
    /// `classify_candidate_header_view` returns `HeaderVerdict::RegionFree` for
    /// a region it believes is Free, which makes the evacuator DROP the
    /// reference. A worker that still sees `Free` for a region a peer retyped
    /// one instruction ago therefore refuses a live referent, which Phase 5
    /// then frees under a holder that still points at it. That is the
    /// "intermittently corrupts a live object" symptom class the mixed-path
    /// note names, it is load-dependent, and it gets worse with more workers.
    ///
    /// As an `AtomicU8` the write and the reads are defined, and — equally
    /// important — `tlab_alloc` and `claim_resume_region` no longer need
    /// `&mut G1Region` at all, so the `&mut`-while-`&`-aliased half of the UB
    /// goes with it.
    ///
    /// The field is PRIVATE so that a future site cannot go around the
    /// accessors and reintroduce the plain access; read it with
    /// [`Self::region_type`] and write it with [`Self::set_region_type`]. The
    /// same reasoning as `cursor` (F-11), which this follows exactly.
    pub region_type: AtomicU8,
    /// Backing storage for this region (a slice of [`G1Collector::arena`]).
    pub data: RegionBuf,
    /// Bump pointer: next free byte offset.
    ///
    /// # F-11 — why this is atomic
    ///
    /// It was a plain `usize`, which made every bump — a Java object
    /// allocation, and every 256 KiB TLAB refill — require `&mut G1Region`,
    /// which required the exclusive regions lock. With a 256 KiB default TLAB
    /// against 1 MiB regions, four refills exhaust an Eden region, so a
    /// multi-threaded allocator hit that exclusive lock constantly AND hit the
    /// region-claim slow path behind it every fourth refill.
    ///
    /// As an `AtomicUsize` the bump is a compare-exchange loop under a SHARED
    /// (read) regions guard, so N allocating threads no longer serialise on the
    /// region table — and neither does the concurrent marker, which reads
    /// cursors under the same shared guard (F-10).
    ///
    /// Read it with [`Self::cursor`] and write it with [`Self::set_cursor`];
    /// the field is private so that no site can go around
    /// [`Self::bump_alloc`]'s claim protocol by assigning it directly.
    ///
    /// **Publication window, and why it is bounded.** `bump_alloc` publishes
    /// the new cursor with the compare-exchange and only THEN zeroes the range
    /// it claimed, so between those two points a concurrent reader that walks
    /// `[0, cursor)` can see bytes that are neither zeroed nor a written
    /// object. That window is not new in kind — the object HEADER has always
    /// been written by the caller after the allocator released the lock, and a
    /// 256 KiB TLAB carve has always exposed a large span below the cursor that
    /// its owner fills incrementally — but F-11 changes what is in it from
    /// zeroes to the stale bytes of a recycled region (`G1Region::reset` no
    /// longer scrubs; see its comment). The rule that keeps it bounded is that
    /// **no concurrent reader walks `[0, cursor)`**: every heap walk is either
    /// stop-the-world or takes the regions WRITE guard, which excludes the
    /// allocator. The one concurrent walk that existed —
    /// `concurrent_mark_step`'s overflow rescan — was moved onto the write
    /// guard for exactly this reason. Adding a walker that runs under `read()`
    /// is what would falsify this.
    pub cursor: AtomicUsize,
    /// Highest complete parallel-evacuation TLAB copy published this pause.
    /// The public region cursor is intentionally delayed until TLAB retirement,
    /// so a source forwarding mark can point above it while still naming a
    /// fully initialized copy. This bound is narrower than the region end and
    /// exists only for validating those collector-published forwarding targets.
    /// gci i25/pl: copies into a shared Survivor PLAB chunk do not publish it
    /// (their chunk was claimed through `cursor`, which already covers them;
    /// `plab_copy_end_needs_publish`), so on that arm it stays 0.
    pub parallel_copy_end: AtomicUsize,
    /// Bytes of live data (computed during marking).
    pub live_bytes: usize,
    /// GC efficiency: `live_bytes / region_size`. Lower means more garbage.
    pub gc_efficiency: f64,
    /// gci i10/rh finding 1 -- the cursor when the last `cleanup` stored
    /// `live_bytes`; 0 = no mark data. HotSpot's `garbage_bytes` is
    /// `cursor_at_cleanup - live_bytes` and does not move when the region takes
    /// promotions afterwards. Read only on `CRATONVM_G1_MIXED_HOTSPOT_CHOICE`.
    pub cursor_at_cleanup: usize,
    /// Per-region remembered set.
    pub rset: RememberedSet,
    /// Whether this region is pinned (JEP 423: JNI critical region pinning).
    /// Mirror of `pin_count > 0` — the CSet-selection filters read this flag.
    pub pinned: bool,
    /// Number of live JNI-critical pins on this region (refcount). Overlapping
    /// critical sections on arrays in the same region — or nested checkouts of
    /// one array — must refcount: a single bool would let an inner `Release`
    /// clear the pin while an outer section is still live, re-admitting the
    /// region to the collection set and relocating an array a native pointer's
    /// copy-back still depends on. Maintained by [`G1Collector::pin_region`] /
    /// [`G1Collector::unpin_region`].
    pub pin_count: u32,
    /// gci i16/pl (finding 8 of
    /// `gci-i15ra-g1-evacuation-additions-review-findings-FIXED-20261016`): pins
    /// taken or released under the SHARED regions guard. A region's total pin
    /// count is `pin_count + critical_extra_pins` ([`Self::pin_total`]). The
    /// shared-guard arms only ever move this counter while `pinned` is already
    /// true and never take the total to zero, so `pinned` (written only under
    /// the WRITE guard) stays exactly `pin_total() > 0` for every reader that
    /// can see it; the first pin and the last unpin still take the write
    /// guard. See [`G1Collector::pin_region`].
    pub critical_extra_pins: std::sync::atomic::AtomicU32,
    /// Survivor age (number of young GCs survived).
    ///
    /// # W3-A — atomic, and what actually reads it
    ///
    /// Written on the same line of `SharedEvac::tlab_alloc` as `region_type`,
    /// from an evacuation worker, while peers may be reading it through
    /// `RegionView::Raw` — the same data race, on the same store.
    ///
    /// Worth being exact about the STAKES, because they are smaller than the
    /// field's name suggests and this file has been bitten by consequences
    /// asserted rather than checked. **The promotion decision does not read
    /// this field.** `should_promote` reads `ObjectHeader::gc_age` against
    /// `tenuring_threshold`, per object; this is a per-REGION counter whose
    /// only readers today are two diagnostic lines
    /// (`describe_rejected_address_view` and the "became Free without reset"
    /// report), one of which runs on an evacuation worker. So the harm from the
    /// race is a diagnostic that misreports the region it is describing —
    /// which is not nothing, since both of those lines exist to start an
    /// investigation into a corrupted walk, and a wrong number in them sends it
    /// the wrong way — plus the plain fact that the write itself was UB.
    ///
    /// It is fixed here rather than deferred because it is the same store as
    /// `region_type`, and because a field that is written correctly is the only
    /// kind a future reader can start depending on.
    ///
    /// Read with [`Self::age`], write with [`Self::set_age`]. Unlike
    /// `region_type` this carries no publication obligation of its own — see
    /// the accessors for why `Relaxed` is the whole of what it needs.
    pub age: AtomicU8,
    /// High-water byte offset ever dirtied across previous incarnations of this
    /// region since the last full scrub (`0` on a freshly mapped region). Under
    /// `gc_flags().gen_zero_once`, `bump_alloc_initialized` only needs to zero
    /// bytes below `dirty_high_water`; bytes at or above it are still pristine
    /// kernel-zeroed anonymous pages.
    pub dirty_high_water: u32,
    /// ngr: the region hand-out clock's value when this region last left the
    /// Free pool ([`Self::set_region_type`];
    /// `gc_quiescence::region_handout_tick`). Every object now in the region
    /// was placed there at or after this stamp. 0 until the first hand-out.
    pub handout_stamp: AtomicU64,
    /// Incarnation counter, bumped on every [`Self::reset`]. Lets the
    /// concurrent-mark cleanup detect that a region was recycled after the
    /// mark-start snapshot (its content then has no mark information and
    /// must be treated as live) — the TAMS-equivalent guard.
    pub reuse_epoch: u64,
    /// G1AUD-5 (defect G1-8) — value of [`G1Collector::rset_cache_epoch`] at
    /// this region's most recent [`Self::reset`].
    ///
    /// Remembered-set entries carry the generation they were recorded in
    /// (`RememberedSet::add_reference_in_generation`); an entry naming THIS
    /// region as a source is dead once `recycled_in_generation` exceeds its
    /// stamp, because `reset` zero-filled everything that could have held the
    /// edge. Starts at 0 (== the initial `rset_cache_epoch`), so an entry
    /// recorded before any collection is never mistaken for stale, and a reset
    /// site that ever forgets to advance it only over-retains.
    ///
    /// # W3-A — why this is atomic, and what it unblocks
    ///
    /// It was a plain `u64` written under the regions WRITE guard and read
    /// under a READ guard, which is sound as far as it goes. What it blocks is
    /// option 4 of `lane-a-rset-tls-fast-path-epoch-window.md` — the
    /// *recommended* fix for the write barrier's lock-free TLS fast path, which
    /// wants to key that path's staleness test on THIS region's recycle stamp
    /// rather than on the heap-wide `rset_cache_epoch`. W3-B landed the interim
    /// fix (re-read the global epoch after the insert) and recorded that option
    /// 4 "requires the field to become an `AtomicU64` first, with the writer in
    /// `G1Region::reset` and the reader on the barrier's fast path", because
    /// reading a plain field from a path that holds no lock at all is a data
    /// race — the same defect class W3-A closed on `region_type`, on the
    /// hottest path in the collector.
    ///
    /// This is that field change. The barrier protocol itself is NOT changed
    /// here; see `docs/feature-designs/concurrent-gc-maturation.md` §9 for
    /// the one thing about option 4 that the recommendation does not account
    /// for, which is W3-B's call and not this lane's.
    ///
    /// Read with [`Self::recycled_in_generation`], write with
    /// [`Self::set_recycled_in_generation`]. Private for the reason
    /// `region_type` is: an atomic `pub` field is reintroducible by anyone who
    /// stores to it with the wrong ordering.
    pub recycled_in_generation: AtomicU64,
    /// Per-region mark bitmap for concurrent marking.
    ///
    /// Round-2 fix (HIGH — GC #5): the bitmap is keyed off the region's
    /// own heap-allocated `data.as_ptr()` base, so `try_mark`/`is_marked`
    /// accept real object addresses living inside this region. The
    /// previous design used a single global bitmap rooted at address 0,
    /// which silently rejected every real region address and produced
    /// `live_bytes = 0` for every region (breaking mixed-GC region
    /// selection). The Vec backing the region is never reallocated
    /// (only zero-filled by `reset`), so the bitmap base remains stable
    /// for the entire collector lifetime.
    pub mark_bitmap: MarkBitmap,
    /// Diagnostic-only provenance ring for this region's cursor advances
    /// (`CRATONVM_G1_DBG_REACH=1`). See [`BumpTrail`] for why a walk break needs
    /// it: the trail of walked objects names the victim, this names the call
    /// that committed the bytes.
    pub bump_trail: Mutex<BumpTrail>,
    /// F-06 — this region's `(incarnation, cursor, type)` at the last
    /// `start_concurrent_mark`, or `None` outside a cycle.
    ///
    /// A per-region mirror of one row of `G1Collector::mark_start_snapshot`,
    /// written in the same loop. The snapshot stays because
    /// `is_live_after_mark` reads it under a different rule; this exists
    /// because the MARKER needs to know a region's TAMS at the moment it marks
    /// an object, and it holds `&[G1Region]` and nothing else.
    pub mark_start: Option<(u64, usize, RegionType)>,
    /// F-06 — bytes of objects marked BELOW this region's TAMS in the current
    /// cycle.
    ///
    /// Maintained by [`Self::try_mark_and_account`], which every mark site goes
    /// through, so that `cleanup` can read per-region liveness instead of
    /// walking every object in the heap to recompute it. Atomic because the
    /// mark sites hold `&[G1Region]`, not `&mut`.
    ///
    /// Cleared by [`Self::reset`] (the bitmap is cleared there too, so the two
    /// stay in step) and at every mark start.
    pub marked_bytes_below_tams: AtomicUsize,
    /// F-05 — the arena-wide card table, shared by every region and by the
    /// collector.
    ///
    /// Held here, rather than only on [`G1Collector`], for one reason:
    /// [`Self::reset`] is the point at which a region's remembered set is
    /// cleared, and the cards are the same claim at a finer granularity, so
    /// they have to be cleared in the same place or the two drift. `reset`
    /// takes `&mut self` on a region and has no path to the collector, so the
    /// region has to be able to reach the table itself. Every region in a
    /// collector shares one `Arc`; the table is allocated once in
    /// `G1Collector::new` and never resized.
    pub cards: std::sync::Arc<crate::g1_cards::G1CardTable>,
    /// The same, restricted to `tlab:*` carves.
    ///
    /// A shared ring cannot answer the question it exists for. TLAB carves are
    /// rare and huge (one `0x1fa78`-byte carve observed) while per-object
    /// advances are constant and small, so by the time a walk breaks inside a
    /// carve, hundreds of `obj:*` entries have evicted it — which is exactly
    /// what happened on all four breaks of the first run. Keeping carves in
    /// their own ring makes the owner of a large committed hole recoverable no
    /// matter how much per-object traffic followed it.
    pub tlab_trail: Mutex<BumpTrail>,
}

impl G1Region {
    /// Create a free region backed by `region_size` bytes starting at arena
    /// address `base`. The caller ([`G1Collector::new`]) guarantees the range
    /// is a live, region-exclusive slice of the collector's `arena`.
    pub fn from_arena(
        base: usize,
        region_size: usize,
        cards: std::sync::Arc<crate::g1_cards::G1CardTable>,
    ) -> Self {
        // Round-2 fix (HIGH — GC #5): bitmap covers exactly this region's
        // backing slice. Base = arena address, span = region_size.
        let mark_bitmap = MarkBitmap::new(base, region_size);
        Self {
            cards,
            region_type: AtomicU8::new(RegionType::Free.to_u8()),
            data: RegionBuf::new(base, region_size),
            cursor: AtomicUsize::new(0),
            parallel_copy_end: AtomicUsize::new(0),
            live_bytes: 0,
            gc_efficiency: 0.0,
            cursor_at_cleanup: 0,
            rset: RememberedSet::default(),
            pinned: false,
            pin_count: 0,
            critical_extra_pins: std::sync::atomic::AtomicU32::new(0),
            age: AtomicU8::new(0),
            dirty_high_water: 0,
            handout_stamp: AtomicU64::new(0),
            reuse_epoch: 0,
            recycled_in_generation: AtomicU64::new(0),
            mark_bitmap,
            mark_start: None,
            marked_bytes_below_tams: AtomicUsize::new(0),
            bump_trail: Mutex::new(BumpTrail::default()),
            tlab_trail: Mutex::new(BumpTrail::default()),
        }
    }

    /// Standalone test constructor: allocates a dedicated, leaked backing
    /// buffer so a region can own stable memory without a shared arena.
    /// Production regions come from [`G1Region::from_arena`] over
    /// [`G1Collector::arena`]; this exists only for the unit tests that
    /// exercise a single region in isolation.
    #[cfg(test)]
    pub fn new(region_size: usize) -> Self {
        let buf = vec![0u8; region_size].into_boxed_slice();
        let base = Box::leak(buf).as_mut_ptr() as usize;
        // F-05: a standalone region gets a card table covering exactly itself,
        // so `reset` and the card screen behave as they do in a real arena.
        let cards = std::sync::Arc::new(crate::g1_cards::G1CardTable::new(base, region_size));
        Self::from_arena(base, region_size, cards)
    }

    /// F-06 — this region's top-at-mark-start, in bytes from its base.
    ///
    /// Objects BELOW it were in the mark snapshot, so the bitmap is
    /// authoritative for them. Everything at or above postdates the snapshot,
    /// carries no mark information, and is implicitly live.
    ///
    /// The three arms are the rule `cleanup` has always applied, moved here so
    /// the marker and the cleanup verdict cannot drift apart:
    ///
    /// * no cycle data — `cleanup` driven outside a real mark cycle, as the
    ///   unit tests do — put TAMS at the top so nothing is implicitly live and
    ///   the verdict is pure-bitmap;
    /// * a snapshot that still describes this region — its recorded fill level,
    ///   clamped to the current cursor;
    /// * recycled (incarnation bumped) or re-typed since the snapshot — the
    ///   region's ENTIRE content postdates it.
    pub fn tams(&self) -> usize {
        match self.mark_start {
            None => self.cursor(),
            Some((epoch, snap_cursor, snap_type))
                if epoch == self.reuse_epoch && snap_type == self.region_type() =>
            {
                snap_cursor.min(self.cursor())
            }
            Some(_) => 0,
        }
    }

    /// W3-A — this region's current classification.
    ///
    /// # Why `Acquire`, at every site, rather than `Relaxed` at most of them
    ///
    /// The load carries a real publication obligation at the site that matters.
    /// `SharedEvac::tlab_alloc` claims a Free region, commits it through
    /// `commit_through_region` (F-16 — which may map memory), stamps the
    /// survivor `age`, and only THEN publishes the new type. A reader that
    /// observes the new type is entitled to assume the region is committed and
    /// that the age it reads next is the one the claimer stamped, and that is
    /// exactly an `Acquire` load paired with the `Release` store in
    /// [`Self::set_region_type`]. `Relaxed` would leave the age read racing
    /// forward of the type read on a weak model, which is the survivor-age
    /// logic reading a 0 for a region that is already Survivor.
    ///
    /// The other ~200 read sites are serial-path reads under the exclusive
    /// regions guard, where the guard already supplies the ordering and
    /// `Relaxed` would be sufficient. They get `Acquire` anyway, for one
    /// reason: a SECOND accessor with a weaker ordering is a fork in the
    /// encoding, and the next person to add a read site would have to decide
    /// which one they are allowed to use — from a call site that usually cannot
    /// tell whether it runs on the parallel arm, because most of these bodies
    /// serve both arms through [`RegionView`]. One accessor with the strong
    /// ordering cannot be got wrong. It is also free where it is hot: on
    /// x86-64 an `Acquire` byte load and a `Relaxed` one emit BYTE-FOR-BYTE
    /// identical code (`movzbl`), so the ordering is free; what the atomic does
    /// cost is one `movzbl` that a plain field read folded into the caller's
    /// `cmp`. Measured, with a null arm, in
    /// `docs/feature-designs/concurrent-gc-maturation.md` §6.
    #[inline]
    pub fn region_type(&self) -> RegionType {
        RegionType::from_u8(self.region_type.load(Ordering::Acquire))
    }

    /// W3-A — reclassify this region.
    ///
    /// `Release`, pairing with [`Self::region_type`]'s `Acquire`: everything
    /// this thread did to make the region fit its new classification — the
    /// F-16 commit, the survivor `age` stamp, a `set_cursor` — must be visible
    /// to any worker that observes the new type, because observing the new type
    /// is precisely how those workers decide the region is usable.
    ///
    /// Takes `&self`, not `&mut self`. That is the point of the change and not
    /// an incidental convenience: `SharedEvac::tlab_alloc` used to form a
    /// `&mut G1Region` from the raw regions base while peers held `&G1Region`
    /// to the same region, which is aliasing UB independent of any field. With
    /// every field it writes now `&self`-writable, the `&mut` is gone.
    ///
    /// A region leaving the Free pool takes a hand-out stamp first
    /// ([`Self::handout_stamp`]), before the type store publishes it as usable.
    #[inline]
    pub fn set_region_type(&self, region_type: RegionType) {
        if region_type != RegionType::Free && self.region_type() == RegionType::Free {
            self.handout_stamp.store(
                cratonvm_gc_core::gc_quiescence::region_handout_tick(),
                Ordering::Relaxed,
            );
        }
        self.region_type
            .store(region_type.to_u8(), Ordering::Release);
    }

    /// ngr: the hand-out clock's value when this region last left the Free
    /// pool; see the field.
    #[inline]
    pub fn handout_stamp(&self) -> u64 {
        self.handout_stamp.load(Ordering::Relaxed)
    }

    /// W3-A — this region's survivor age (young collections survived).
    ///
    /// `Relaxed`. The field carries no ordering obligation of its own: it is a
    /// plain counter, it guards no other memory (see the field's own note on
    /// what does and does not read it), and the one cross-thread publication it
    /// participates in — a worker stamping `age = 1` on a region it is turning
    /// into a Survivor — is carried by the `Release` on
    /// [`Self::set_region_type`] that follows it. A reader reaches a region's
    /// age only after deciding from its TYPE that the age means something, so
    /// the `Acquire` on that read is what orders this one.
    #[inline]
    pub fn age(&self) -> u8 {
        self.age.load(Ordering::Relaxed)
    }

    /// W3-A — set this region's survivor age. `Relaxed`; see [`Self::age`].
    ///
    /// Callers that are publishing a newly-claimed Survivor region must set the
    /// age BEFORE `set_region_type`, so the `Release` there publishes it.
    #[inline]
    pub fn set_age(&self, age: u8) {
        self.age.store(age, Ordering::Relaxed);
    }

    /// W3-A — the [`G1Collector::rset_cache_epoch`] generation in which this
    /// region was most recently [`Self::reset`].
    ///
    /// `Acquire`, pairing with the `Release` in `reset`. The obligation is
    /// real and is the whole point of the field being atomic: `reset` clears
    /// this region's remembered set and THEN stores the stamp, so a reader that
    /// observes a new stamp is entitled to conclude that the clear which
    /// accompanied it has also happened. A `Relaxed` load would leave a
    /// lock-free reader free to see the new stamp and the old rset contents,
    /// which is precisely the conclusion the barrier's fast path would be
    /// drawing.
    ///
    /// Every reader today holds the regions guard and would be correct with
    /// `Relaxed`; the accessor is `Acquire` for the same reason
    /// [`Self::region_type`] is — one accessor cannot be got wrong, and the
    /// load is a plain `mov` on x86-64 either way.
    #[inline]
    pub fn recycled_in_generation(&self) -> u64 {
        self.recycled_in_generation.load(Ordering::Acquire)
    }

    /// W3-A — record the generation this region was reset in.
    ///
    /// `Release`; see [`Self::recycled_in_generation`]. Callers must have
    /// already cleared everything the new stamp declares dead — in practice the
    /// only caller is [`Self::reset`], and the statement order there is
    /// load-bearing.
    #[inline]
    pub fn set_recycled_in_generation(&self, generation: u64) {
        self.recycled_in_generation
            .store(generation, Ordering::Release);
    }

    /// W3-A — publish this Free region as an evacuation destination of
    /// `dest_type`, the way `SharedEvac::tlab_alloc` does when a worker takes
    /// a region out of the parallel evacuator's Free pool.
    ///
    /// # Why the two stores are a named operation rather than two lines
    ///
    /// Their ORDER is the whole of the publication protocol, and a protocol
    /// spread over two statements at a call site is a protocol the next edit
    /// silently breaks. The age is stamped FIRST, with a `Relaxed` store; the
    /// type store that follows is `Release` and is what publishes it. A peer
    /// worker loads the type with `Acquire` — in `shared_dest_alloc`'s
    /// destination scan, in `claim_resume_region`'s `!= Old` test, or in any of
    /// the `RegionView` screens — and if it sees `Survivor` it is then
    /// guaranteed to see `age == 1` and the F-16 `commit_through_region` that
    /// preceded this call, rather than the zero the region carried while it was
    /// Free. The `age` half of that is today only worth a correct diagnostic
    /// line (see the field's note); the COMMIT half is worth a worker not
    /// bump-allocating into address space that is not yet memory.
    ///
    /// Swapping the two stores reinstates exactly the window W3-A closed, and
    /// that is a real window on x86 and not only a model one: the pre-W3-A code
    /// stored the type first, so a peer could observe a region already typed
    /// `Survivor` whose age was still 0. `gc/tests/g1_w3a_region_metadata.rs`
    /// calls THIS function — not a copy of it — precisely so that the swap is
    /// caught rather than merely disapproved of in a comment.
    ///
    /// Takes `&self`: the caller is an evacuation worker reaching the region
    /// through the raw regions base while its peers hold `&G1Region` to the
    /// same region, so forming a `&mut` here would be aliasing UB no matter
    /// what the fields were.
    #[inline]
    pub fn claim_as_evacuation_destination(&self, dest_type: RegionType) {
        if dest_type == RegionType::Survivor {
            self.set_age(1);
        }
        self.set_region_type(dest_type);
    }

    /// This region's bump cursor: the offset of the next free byte.
    ///
    /// F-11 made the field atomic (see its doc); this is the read side. The
    /// load is `Acquire` so a reader that observes a cursor also observes the
    /// writes the claiming thread made before publishing it.
    #[inline]
    pub fn cursor(&self) -> usize {
        self.cursor.load(Ordering::Acquire)
    }

    /// End of the latest completely initialized parallel TLAB copy.
    #[inline]
    pub fn parallel_copy_end(&self) -> usize {
        self.parallel_copy_end.load(Ordering::Acquire)
    }

    /// Publish a fully written copy before its source forwarding mark.
    #[inline]
    pub fn publish_parallel_copy_end(&self, end: usize) {
        self.parallel_copy_end.fetch_max(end, Ordering::Release);
    }

    /// [`Self::publish_parallel_copy_end`] for the region's single writer: the
    /// worker whose TLAB owns the whole region (`copy_end_owned_by`). Its
    /// copies only move up, and a previous owner in the same pause retired
    /// (its last publish happens-before this worker's claim), so a Release
    /// store leaves the value `fetch_max` would.
    #[inline]
    pub fn publish_owned_parallel_copy_end(&self, end: usize) {
        self.parallel_copy_end.store(end, Ordering::Release);
    }

    /// Set this region's bump cursor outright.
    ///
    /// For the stop-the-world phases that move a cursor wholesale — retiring a
    /// TLAB's unused tail, sizing a humongous start, `reset`, and the tests
    /// that stage a fill level. Ordinary allocation must go through
    /// [`Self::bump_alloc`], which CLAIMS a range instead of overwriting the
    /// cursor: a `set_cursor` racing a concurrent claim would silently
    /// un-allocate whatever the claimer had just been handed. Every caller is
    /// therefore either stop-the-world or holds the regions write guard.
    #[inline]
    pub fn set_cursor(&self, value: usize) {
        self.cursor.store(value, Ordering::Release);
    }

    /// F-06 — mark `addr` black and, if it predates TAMS, add its size to
    /// [`Self::marked_bytes_below_tams`]. Returns what `try_mark` returned.
    ///
    /// # Why every mark site goes through this
    ///
    /// `cleanup` used to derive per-region liveness by walking every object of
    /// every non-Free region and consulting the bitmap — an O(heap)
    /// stop-the-world pass at the end of each concurrent cycle, growing with
    /// the old generation, which is the one part of the heap the collector
    /// otherwise never touches synchronously. Real G1 has no such pause because
    /// it accumulates the same number DURING marking. This is that
    /// accumulation.
    ///
    /// It only works if it is the ONLY way a bit gets set. An unaccounted mark
    /// makes `live_bytes` too small, and a too-small `live_bytes` is what lets
    /// `cleanup` free a live Old region in place — so the four production mark
    /// sites (`concurrent_mark_step`, `push_gray_or_mark`'s overflow arm,
    /// `remark`'s seed-at-cap arm, and the SATB keep-alive's seed-at-cap arm)
    /// all call this and none call `mark_bitmap.try_mark` directly.
    ///
    /// The size is bounded by the region's own cursor before it is added, which
    /// is the same bound the walk applied, so a corrupt header inflates nothing.
    /// An object that straddles TAMS is counted in full — under bump
    /// allocation TAMS is an object boundary so it cannot legitimately happen,
    /// and over-counting only retains a region for one more cycle.
    pub fn try_mark_and_account(&self, addr: usize) -> bool {
        match self.try_mark_and_measure(addr) {
            None => false,
            Some(bytes) => {
                if bytes > 0 {
                    self.marked_bytes_below_tams
                        .fetch_add(bytes, Ordering::Relaxed);
                }
                true
            }
        }
    }

    /// F-12 — [`Self::try_mark_and_account`] split in two: mark the bit and
    /// RETURN the bytes the caller must add to
    /// [`Self::marked_bytes_below_tams`], instead of adding them here.
    ///
    /// `None` means the object was already black. `Some(0)` means it was newly
    /// marked but is at or above TAMS (implicitly live; `cleanup` adds that
    /// extent wholesale, so counting it here is the G1MAT-1 double-count).
    ///
    /// # Why this exists, and the obligation it creates
    ///
    /// This is still the ONLY caller of `mark_bitmap.try_mark`, so F-06's
    /// "every mark goes through one funnel" property is intact. What moves is
    /// WHERE the byte count lands. One `fetch_add` per marked object is free
    /// for a single marker and is not free for several: a mark cycle walks a
    /// handful of regions, so N workers were doing a read-modify-write on the
    /// SAME `AtomicUsize` cache line once per object. Measured on a
    /// 160,000-object graph at six workers, that one atomic was about 30 ns per
    /// object — enough on its own to make parallel marking slower than serial
    /// marking.
    ///
    /// The obligation is that the returned bytes MUST reach the accumulator.
    /// An under-count makes a live region look dead and `cleanup` frees it in
    /// place, which is the failure F-06's doc describes. The marker discharges
    /// it through [`MarkLiveBytes`], whose `Drop` flushes — so an early
    /// `break`, a `return`, or an unwind in the middle of a batch cannot lose
    /// the count. Any NEW caller must do the same, or call
    /// `try_mark_and_account` and pay the atomic.
    ///
    /// A debug build re-derives the accumulator by walking the heap on every
    /// cleanup and asserts the two agree, so a dropped batch is a failing test
    /// rather than a silently freed live region.
    pub fn try_mark_and_measure(&self, addr: usize) -> Option<usize> {
        if !self.mark_bitmap.try_mark(addr) {
            return None;
        }
        let Some(off) = addr.checked_sub(self.data.addr()) else {
            return Some(0);
        };
        // At or above TAMS: implicitly live, and `cleanup` adds that extent
        // wholesale. Counting it here as well is the G1MAT-1 double-count.
        if off >= self.tams() {
            return Some(0);
        }
        // SAFETY: `off < tams <= cursor`, so `addr` is inside this region's
        // live extent; every caller reached it through a region lookup, and the
        // regions guard is held for the duration of a mark step.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        let size = object_total_size(header);
        if size >= HEADER_SIZE && off.saturating_add(size) <= self.cursor() {
            Some(size)
        } else {
            Some(0)
        }
    }

    /// JIT round 23 wave 8, lane L9 (`CRATONVM_G1_MARK_SIZE_MEMO`):
    /// [`Self::try_mark_and_measure`] with the object's size asked through
    /// the marker's per-step [`WalkSizeMemo`] instead of `object_total_size`.
    /// The memo answers exactly what `object_total_size` answers (see the
    /// type), so the mark bit, the TAMS rule and the credited bytes are those
    /// of [`Self::try_mark_and_measure`]; with the memo disabled it IS that
    /// function's arithmetic.
    pub fn try_mark_and_measure_sized(&self, addr: usize, sizes: &mut WalkSizeMemo) -> Option<usize> {
        if !self.mark_bitmap.try_mark(addr) {
            return None;
        }
        let Some(off) = addr.checked_sub(self.data.addr()) else {
            return Some(0);
        };
        if off >= self.tams() {
            return Some(0);
        }
        // SAFETY: as in `try_mark_and_measure`: `off < tams <= cursor`, and the
        // regions guard is held for the duration of a mark step.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        let size = sizes.size(header);
        if size >= HEADER_SIZE && off.saturating_add(size) <= self.cursor() {
            Some(size)
        } else {
            Some(0)
        }
    }

    /// F-12 — add `bytes` to this region's live accumulator in one step.
    /// The flush half of [`Self::try_mark_and_measure`].
    #[inline]
    pub fn add_marked_bytes(&self, bytes: usize) {
        if bytes > 0 {
            self.marked_bytes_below_tams
                .fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Remaining free bytes in this region.
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor())
    }

    /// Step 7 — estimated time (ns) to evacuate this region's live data, used
    /// by pause-target collection-set sizing. Evacuation cost is dominated by
    /// copying the region's live bytes (plus per-slot reference rewriting);
    /// `ns_per_byte` is the collector's rolling `evac_ns_per_byte` calibration.
    pub fn estimated_evac_cost_ns(&self, ns_per_byte: u64) -> u64 {
        (self.live_bytes as u64).saturating_mul(ns_per_byte)
    }

    /// gci i10/rh finding 1 -- the live bytes the mixed choice prices: HotSpot's
    /// `used() - garbage_bytes()` on the `CRATONVM_G1_MIXED_HOTSPOT_CHOICE` arm
    /// (bytes promoted in after cleanup are live), the marked bytes otherwise.
    pub fn mixed_live(&self, hotspot: bool) -> usize {
        if hotspot && self.cursor_at_cleanup > 0 {
            mixed_choice::live_estimate(self.cursor(), self.live_bytes, self.cursor_at_cleanup)
        } else {
            self.live_bytes
        }
    }

    /// gci i10/rh finding 1 -- the reclaimable bytes the waste floor and the full
    /// compaction's screen sum: the garbage fixed at cleanup on the switch arm.
    pub fn mixed_reclaimable(&self, hotspot: bool) -> usize {
        if hotspot && self.cursor_at_cleanup > 0 {
            mixed_choice::reclaimable_bytes(self.live_bytes, self.cursor_at_cleanup)
        } else {
            self.cursor().saturating_sub(self.live_bytes)
        }
    }

    /// gci i10/rh finding 1 -- the ranking key (lower = more garbage first).
    pub fn mixed_rank(&self, hotspot: bool, region_size: usize) -> f64 {
        if hotspot && self.cursor_at_cleanup > 0 && region_size > 0 {
            self.mixed_live(true) as f64 / region_size as f64
        } else {
            self.gc_efficiency
        }
    }

    /// gci i10/rh finding 1 -- the copy price of what the mixed choice thinks is live.
    pub fn mixed_evac_cost_ns(&self, ns_per_byte: u64, hotspot: bool) -> u64 {
        (self.mixed_live(hotspot) as u64).saturating_mul(ns_per_byte)
    }

    /// Reset this region to Free state.
    ///
    /// `rset_generation` is the collector's current
    /// [`G1Collector::rset_cache_epoch`], which every recycle/retype phase
    /// bumps before it touches a region. Recording it here is what lets the
    /// remembered set drop entries that name this region as a source from an
    /// earlier incarnation (G1AUD-5 / defect G1-8): the parameter is mandatory
    /// so a new reset site cannot silently skip the stamp.
    pub fn reset(&mut self, rset_generation: u64) {
        // Captured BEFORE the fields are cleared — the zero-fill below is
        // bounded by them (see the `dirty` computation at the end).
        let prev_cursor = self.cursor();
        let was_humongous_continuation = self.region_type() == RegionType::HumongousContinuation;
        self.set_region_type(RegionType::Free);
        self.set_cursor(0);
        self.parallel_copy_end.store(0, Ordering::Release);
        self.live_bytes = 0;
        self.gc_efficiency = 0.0;
        self.cursor_at_cleanup = 0;
        self.rset.clear();
        // F-05 — the cards are the same claim as the rset at a finer
        // granularity, so they are cleared by the same statement that clears
        // it. A card says "a cross-region reference store landed in these 512
        // bytes"; this reset zero-fills the region and hands it back to the
        // allocator, so nothing that could have held such a reference survives
        // and every card of the region is a lie from here on. Leaving them
        // dirty would not be unsound (Phase 2 would over-scan, exactly as a
        // stale rset entry makes it over-scan) but it would make the screen
        // useless within a few cycles: a region that is recycled every young
        // pause would accumulate dirty cards forever.
        //
        // Cleared over `data.len()` rather than `prev_cursor`: a
        // `HumongousContinuation` carries `cursor = 0` while its bytes hold the
        // start object's payload, so a cursor-bounded clear would leave that
        // span dirty for the region's next incarnation. The whole slice is the
        // only bound that is right for every region type, and every region
        // boundary is a card boundary (see `G1CardTable`), so this never
        // touches a neighbour's cards.
        self.cards
            .clear_range(self.data.as_ptr() as usize, self.data.len());
        self.pinned = false;
        self.pin_count = 0;
        *self.critical_extra_pins.get_mut() = 0;
        self.set_age(0);
        // New incarnation: content allocated from here on postdates any
        // in-flight mark cycle's snapshot (see `cleanup`).
        self.reuse_epoch = self.reuse_epoch.wrapping_add(1);
        // G1AUD-5: every rset entry naming this region as a source with a
        // stamp strictly below this value describes an object this reset just
        // zero-filled. Monotone by construction (`rset_cache_epoch` only ever
        // increases), so a later reset can never lower the bar.
        //
        // W3-A: this store is `Release` and it is deliberately AFTER
        // `self.rset.clear()` above. A lock-free reader that observes the new
        // stamp therefore also observes the clear that accompanied it — which
        // is the ordering option 4 of the TLS-fast-path page needs, and the
        // reason the two statements must not be reordered for tidiness.
        self.set_recycled_in_generation(rset_generation);
        // Round-2 fix (HIGH — GC #5): clear stale mark bits so they don't
        // pollute the next concurrent-mark cycle. The Vec is never
        // reallocated (only `fill(0)`'d) so the bitmap's base address
        // remains valid.
        //
        // gc-common w4-e: the memset form (`clear_exclusive`). `&mut self` is
        // the witness, and it holds for the bitmap WORDS: every caller reaches
        // this through the regions write guard (or a pause-local `&mut [G1Region]`
        // taken from it), the marker reads bits only under the read guard, the
        // parallel evacuator's `RegionsBase` access ends at the pool barrier
        // before Phase 5 resets anything, and the one lock-free raw
        // `*const G1Region` (the RSet TLS cache in `post_write_barrier_rset`)
        // touches `rset`, never `mark_bitmap`.
        self.mark_bitmap.clear_exclusive();
        // F-06: the byte accumulator is derived from those bits and must be
        // cleared with them. `mark_start` is deliberately NOT cleared — the
        // `reuse_epoch` bump above makes `tams()` answer 0 for this region,
        // i.e. "its entire content postdates the mark snapshot", which is
        // exactly the verdict the old cleanup walk reached for a region
        // recycled mid-cycle.
        self.marked_bytes_below_tams.store(0, Ordering::Relaxed);
        // G1AUD-10 — DO NOT scrub the freed bytes. The allocator already does
        // it, and this was the single most expensive phase of a young pause.
        //
        // MEASURED, and that is how it was found. The per-phase `[GC-STAT]`
        // breakdown (`G1PausePhases`) on `probes/G1ChurnPauseProbe 96 900` at
        // `-Xmx2048m` put a 330 ms young pause at: roots 0.7%, remembered-set
        // walks 0.03%, Cheney closure 38%, whole-heap fix-up 10-18%, and
        // **freeing the collection set 42%** — the largest phase in the pause,
        // larger than the closure that does all the copying. `bytes_freed` was
        // 1.61 GB against a 135 ms free phase: 11.9 GB/s, which is memset
        // bandwidth and nothing else.
        //
        // WHY IT IS REDUNDANT. Every byte a region ever hands out is zeroed by
        // the ALLOCATOR at the moment it is handed out:
        //
        //   * `G1Region::bump_alloc` zeroes exactly the range it returns — the
        //     single establishment of the TLAB zeroing contract, which is what
        //     gives a fresh Java object its default-zero fields and a TLAB
        //     carve its zeroed body (the inline JIT `new` depends on this);
        //   * the humongous arm of `alloc_in_region_locked` zeroes the whole
        //     span `claim_humongous_span_locked` reserved for it,
        //     precisely because a continuation slice may still hold stale
        //     collected data.
        //
        // and no byte is left between objects for a walker to trip over,
        // because every object size is a multiple of 8: a compact body is
        // rounded (`classloading/src/class.rs`, "Rounding the tail keeps the
        // next header aligned"), a legacy body is `num_slots * SLOT_SIZE` with
        // `SLOT_SIZE` 8 or 16, and array data is rounded by
        // `array_data_size_checked`. So `bump_alloc`'s align-to-8 is always a
        // no-op, there is no inter-object padding, and every byte below a live
        // region's `cursor` belongs to exactly one object that was fully
        // written. Nothing reads a `Free` region at all — every walker skips
        // `RegionType::Free`, and `is_addr_in_live_region` /
        // `classify_candidate_header` reject it outright.
        //
        // WHAT IS LOST. Only a debugging property: a use-after-free read of a
        // reclaimed region now sees stale object bytes instead of zeros. That
        // is what `CRATONVM_G1_SCRUB_FREE=1` restores — and restoring it is the
        // FIRST thing to try if a G1 heap-corruption investigation wants the
        // old world back, because it also makes this the single-binary A/B for
        // the change. (HotSpot does not eagerly zero reclaimed regions either.)
        let dirty = if was_humongous_continuation {
            self.data.len()
        } else {
            prev_cursor.min(self.data.len())
        };
        if gc_flags().g1_scrub_free {
            // `HumongousContinuation` carries `cursor = 0` while its bytes hold
            // the START object's payload, so bounding by its own cursor would
            // scrub nothing; `HumongousStart` carries the whole object size,
            // which runs past its own slice, so it is clamped.
            self.data[..dirty].fill(0);
            self.dirty_high_water = 0;
        } else {
            self.dirty_high_water = self
                .dirty_high_water
                .max(u32::try_from(dirty).unwrap_or(u32::MAX));
        }
    }

    /// Bump-allocate `size` bytes (with alignment) in this region.
    /// Returns `(pointer, offset_within_region)` or `None` if region is full.
    ///
    /// `site` is a stable `&'static str` naming the caller, recorded in
    /// [`Self::bump_trail`] under `CRATONVM_G1_DBG_REACH=1`. A per-object carve
    /// and a TLAB carve leave indistinguishable bytes behind but carry entirely
    /// different obligations afterwards (write a header now vs. retire/publish
    /// the tail later), so the tag has to come from the call site — nothing left
    /// in the region recovers it.
    pub fn bump_alloc(
        &self,
        size: usize,
        align: usize,
        site: &'static str,
    ) -> Option<(*mut u8, usize)> {
        self.bump_alloc_initialized(size, align, site, |_| {})
    }

    /// [`Self::bump_alloc`], with `init` run over the fresh span before the
    /// pointer is returned to the caller.
    ///
    /// # What this does and does NOT close
    ///
    /// It closes the half of the `cursor` field's documented publication window
    /// that belongs to the OBJECT HEADER. That doc says the window "is not new
    /// in kind -- the object HEADER has always been written by the caller after
    /// the allocator released the lock"; with an initializer the header is
    /// written inside the allocator instead, so the four call sites
    /// (`try_alloc_object`, `try_alloc_array`, and the `GarbageCollector`
    /// `alloc_object` / `alloc_array`) cannot individually forget to, and the
    /// span carries a decodable header the moment the allocator returns.
    ///
    /// It does NOT close the F-11 window itself, and must not be read as
    /// claiming to. Under F-11 the compare-exchange that claims the range IS
    /// the publication, so nothing can run before it; `init` runs after the
    /// claim and after the zeroing, exactly where the caller's header store
    /// used to run, only sooner and unconditionally. What bounds that window is
    /// the rule the `cursor` field states -- no concurrent reader walks
    /// `[0, cursor)` -- and this changes nothing about that rule.
    ///
    /// An earlier version of this function moved zeroing and `init` ABOVE a
    /// plain-`usize` cursor commit. That ordering does not survive F-11 and the
    /// claim is not made here any more.
    pub fn bump_alloc_initialized(
        &self,
        size: usize,
        align: usize,
        site: &'static str,
        // `Fn`, not `FnOnce`: `alloc_in_region_initialized` has arms that may
        // each attempt a bump, and only the arm that SUCCEEDS runs the
        // initializer, so callers build their header inside the closure body
        // and the closure stays re-callable.
        init: impl Fn(*mut u8),
    ) -> Option<(*mut u8, usize)> {
        let base = self.data.addr();
        // F-11 — claim the range with a compare-exchange rather than a
        // read-modify-write through `&mut`. The loop body is the identical
        // arithmetic the exclusive version did; the only new thing is that a
        // losing racer re-reads the cursor and tries again against the winner's
        // result. A plain `fetch_add` would not do: the claim is `align`, then
        // `+ size`, then a bounds check, so a thread has to be able to WITHDRAW
        // when the region turns out to be full, and `fetch_add` cannot.
        //
        // Termination: every successful exchange strictly increases the cursor
        // and the cursor is bounded by `data.len()`, so a losing racer makes
        // progress toward either a fit or the `end > len` refusal.
        let (ptr, offset_in_region) = loop {
            let cursor = self.cursor.load(Ordering::Acquire);
            let current = base + cursor;
            let aligned = (current + align - 1) & !(align - 1);
            let offset_in_region = aligned - base;
            let end = offset_in_region + size;

            if end > self.data.len() {
                return None;
            }

            // `AcqRel` on success publishes this claim to the next claimer;
            // `Acquire` on failure is what makes the re-read see the winner's.
            if self
                .cursor
                .compare_exchange_weak(cursor, end, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break (aligned as *mut u8, offset_in_region);
            }
        };

        if gc_flags().g1_dbg_reach {
            self.bump_trail
                .lock()
                .record(self.reuse_epoch, offset_in_region, size, site);
            if site.starts_with("tlab:") {
                self.tlab_trail
                    .lock()
                    .record(self.reuse_epoch, offset_in_region, size, site);
            }
        }
        // Zero-init the allocated area. This is the SINGLE establishment of
        // the TLAB zeroing contract — `refill_tlab`'s carves used to repeat it
        // over the identical range. `refill_tlab_zeroes_dirty_eden_bytes` is
        // the oracle: it dirties Eden above the cursor and fails if this goes.
        //
        // The claim above guarantees this range is exclusively ours — no other
        // thread's exchange can overlap it — so two allocators zeroing at once
        // never touch the same byte. See the `cursor` field doc for the window
        // between publishing the cursor and finishing this zeroing, and for the
        // rule (no concurrent walker below a cursor) that bounds it.
        unsafe {
            if gc_flags().gen_zero_once {
                let dirty_hi = self.dirty_high_water as usize;
                if offset_in_region < dirty_hi {
                    let zero_len = (dirty_hi - offset_in_region).min(size);
                    cratonvm_gc_core::arena::ZeroPlan::fast_zero_tlab_bytes(ptr, zero_len);
                }
            } else {
                cratonvm_gc_core::arena::ZeroPlan::fast_zero_tlab_bytes(ptr, size);
            }
        }
        // The header, before the pointer is handed out. See this function's
        // doc for what this does and does not close: the claim above is the
        // publication under F-11, so this is the caller's header store moved
        // inside the allocator, not a reordering of it against the cursor.
        init(ptr);
        Some((ptr, offset_in_region))
    }

    /// gci i16/pl (finding 3 of
    /// `gci-i15ra-g1-evacuation-additions-review-findings-FIXED-20261016`): claim
    /// `size` bytes, 8-aligned, with the same compare-exchange as
    /// [`Self::bump_alloc`] but WITHOUT zeroing them; returns the offset in
    /// the region.
    ///
    /// For evacuation destinations only (the Survivor PLAB of
    /// `SharedEvac::claim_survivor_plab` and its direct allocations): every
    /// byte of the span is a whole copied object, an abandoned CAS-loser copy
    /// (also whole) or the filler `SharedEvac::retire_tlab` stamps over a
    /// chunk's unused tail, before any walker reads `[0, cursor)` -- the
    /// per-worker destination arm has never zeroed either (G1AUD-10), and
    /// HotSpot does not zero PLABs. A mutator TLAB must keep `bump_alloc`.
    /// `size` must be a multiple of 8 (every caller rounds it).
    pub fn claim_unzeroed(&self, size: usize, site: &'static str) -> Option<usize> {
        let base = self.data.addr();
        let offset_in_region = loop {
            let cursor = self.cursor.load(Ordering::Acquire);
            let aligned = (base + cursor + 7) & !7;
            let offset_in_region = aligned - base;
            let end = offset_in_region.checked_add(size)?;
            if end > self.data.len() {
                return None;
            }
            if self
                .cursor
                .compare_exchange_weak(cursor, end, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break offset_in_region;
            }
        };
        if gc_flags().g1_dbg_reach {
            self.bump_trail
                .lock()
                .record(self.reuse_epoch, offset_in_region, size, site);
        }
        Some(offset_in_region)
    }

    /// gci i16/pl: this region's JNI-critical pins, both halves (the
    /// write-guard `pin_count` and the shared-guard
    /// `critical_extra_pins`).
    pub fn pin_total(&self) -> u32 {
        self.pin_count
            .saturating_add(self.critical_extra_pins.load(Ordering::Acquire))
    }

    /// gci i16/pl: one more pin under the SHARED regions guard, only on a
    /// region that is already pinned (so `pinned` needs no write). `false`
    /// when it is not pinned or the counter is saturated: the caller takes
    /// the write-guard arm.
    pub fn try_pin_shared(&self) -> bool {
        if !self.pinned {
            return false;
        }
        self.critical_extra_pins
            .try_update(Ordering::AcqRel, Ordering::Acquire, |e| e.checked_add(1))
            .is_ok()
    }

    /// gci i16/pl: one pin fewer under the SHARED regions guard, only when it
    /// is not the region's last (`pin_count + extra >= 2`, with `pin_count`
    /// frozen while the shared guard is held), so `pinned` stays true and
    /// needs no write. `false`: the caller takes the write-guard arm.
    pub fn try_unpin_shared(&self) -> bool {
        let base = self.pin_count;
        self.critical_extra_pins
            .try_update(Ordering::AcqRel, Ordering::Acquire, |e| {
                if e > 0 && base.saturating_add(e) >= 2 {
                    Some(e - 1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    /// gci i16/pl: the write-guard pin (the old body of
    /// [`G1Collector::pin_region`]).
    pub fn pin_exclusive(&mut self) {
        self.pin_count = self.pin_count.saturating_add(1);
        self.pinned = true;
    }

    /// gci i16/pl: the write-guard unpin. Takes a shared-guard pin first
    /// (they are indistinguishable), then a `pin_count` one (saturating: a
    /// call with no pin is a no-op), and re-derives `pinned` from the total.
    pub fn unpin_exclusive(&mut self) {
        let extra = self.critical_extra_pins.get_mut();
        if *extra > 0 {
            *extra -= 1;
        } else {
            self.pin_count = self.pin_count.saturating_sub(1);
        }
        let total = self.pin_count.saturating_add(*extra);
        self.pinned = total > 0;
    }

    /// Get a raw pointer to the start of this region's data.
    pub fn _base_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Get a mutable raw pointer to the start of this region's data.
    pub fn base_ptr_mut(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }
}

/// Round-5 HIGH #6 — defensive cap on the gray-set worklist.
///
/// The mark worklist was previously an unbounded `Vec<usize>`. A
/// pathological object graph (e.g. a malicious or buggy classloader that
/// produces an exceptionally wide reference fan-out during a concurrent
/// mark cycle, or a marker thread that has been starved so the worklist
/// grows faster than it drains) could OOM the JVM by ballooning this
/// Vec. The cap below converts that silent allocator-OOM into a
/// deterministic panic, which is strictly better than crashing the
/// entire VM with an unrecoverable allocation failure deep inside
/// `Vec::push`.
///
/// 1 million entries * 8 bytes = 8 MiB — chosen large enough that any
/// realistic mark cycle stays well below it, small enough that the
/// panic is reproducible in tests.
///
/// TODO(round-5+): the real fix is an overflow-handling strategy —
/// either spill the worklist to a backing region, or drop the explicit
/// worklist entirely and fall back to a "mark-everything-dirty" sweep
/// pass guided by the card table. Both are too invasive for this
/// hotfix; the cap below is the defensive interim.
pub const MARK_WORKLIST_CAP: usize = 1 << 20;

/// F-10 — how many gray objects the concurrent marker scans per acquisition of
/// the regions lock.
///
/// `concurrent_mark_step` used to take the lock once per STEP; the background
/// worker's step is 256 objects (`g1_concurrent::WORKER_STEP_BUDGET`) and the
/// drain-to-fixed-point callers pass `usize::MAX`, so a mutator wanting to
/// allocate — or a stop-the-world pause wanting to start — waited for the
/// marker's whole step. This is the granularity the lock is now held at
/// instead.
///
/// It is a latency/overhead trade with no correctness content. Each scan is
/// tens to a few hundred nanoseconds; an uncontended `parking_lot` read
/// acquire/release is a couple of atomics. At 32 the marker pays 8 acquisitions
/// per 256-object step (well under 1% of the step) and bounds any waiting
/// writer's queueing delay to about a microsecond. Smaller buys little and
/// starts to matter against the scan itself; much larger reproduces the
/// convoying this constant exists to remove.
///
/// `CRATONVM_G1_MARK_LOCK_YIELD=0` substitutes `usize::MAX` here, restoring the
/// single-acquisition step exactly.
pub const MARK_LOCK_BATCH: usize = 32;

/// F-12 — how many gray addresses a marking worker pulls from the shared seed
/// queue in one refill.
///
/// The seed queue (`G1Collector::mark_worklist`) is where roots, SATB
/// overwrites and the overflow rescan deposit work, and every worker refills
/// from it under one lock. Taking one entry at a time would make that lock the
/// new bottleneck; taking too many would leave one worker holding the whole
/// initial root set while its peers steal it back an entry at a time. 64 is two
/// batches' worth of scanning, so a worker that refills has enough to work on
/// while its peers refill too.
pub const MARK_REFILL_CHUNK: usize = 64;

/// Ten-findings item 1 — a humongous span whose remembered set names more
/// live source regions than this is retained until the next mark cycle's
/// cleanup rather than having every source walked at every pause. Eight is
/// HotSpot's `G1EagerReclaimRemSetThreshold` default and the same trade: a
/// widely-referenced buffer is not the short-lived one eager reclaim is for.
pub const EAGER_RECLAIM_MAX_SOURCES: usize = 8;

/// Ten-findings item 9 — a reference array longer than this is marked in
/// chunks of this many elements. The concurrent marker scans a gray object
/// while holding the region table's shared guard, so one `Object[10_000_000]`
/// used to be an 80 MB scan during which every pause (an exclusive-guard
/// taker) waited. A gray-set entry now carries a chunk index in its high 16
/// bits (`gray_entry`), the first chunk marks the array and pushes a
/// continuation for the second, and so on; a 4096-element chunk is ~32 KB of
/// slots, on the order of one ordinary object scan.
pub const MARK_ARRAY_CHUNK: usize = 4096;
/// Item 9 — where the chunk index lives in a gray-set entry. User-space
/// addresses on x86-64 and AArch64 fit in 48 bits (Windows: 47); the top 16
/// carry the chunk. An array longer than `GRAY_CHUNK_MAX * MARK_ARRAY_CHUNK`
/// elements (2^28, a 2 GB reference array) has its tail scanned whole by the
/// last chunk.
pub const GRAY_CHUNK_SHIFT: u32 = 48;
pub const GRAY_ADDR_MASK: usize = (1usize << GRAY_CHUNK_SHIFT) - 1;
pub const GRAY_CHUNK_MAX: usize = (1usize << (usize::BITS - GRAY_CHUNK_SHIFT)) - 1;
const _: () = assert!(
    usize::BITS == 64,
    "the gray-set chunk encoding packs a chunk index above a 48-bit address"
);

#[inline]
pub fn gray_entry(addr: usize, chunk: usize) -> usize {
    debug_assert_eq!(
        addr & !GRAY_ADDR_MASK,
        0,
        "a gray address must fit in 48 bits"
    );
    addr | (chunk.min(GRAY_CHUNK_MAX) << GRAY_CHUNK_SHIFT)
}

#[inline]
pub fn gray_entry_addr(entry: usize) -> usize {
    entry & GRAY_ADDR_MASK
}

#[inline]
pub fn gray_entry_chunk(entry: usize) -> usize {
    entry >> GRAY_CHUNK_SHIFT
}

/// F-12 — RAII "this thread has mark work in flight" counter.
///
/// Termination for a parallel marker cannot be "every queue is empty": a worker
/// that has popped an object and is scanning it holds work that is in no queue,
/// and its children are not pushed until the scan finishes. A peer that
/// observed empty queues at that moment would declare marking converged with a
/// live subtree unscanned. This counter is the missing term, and it is the same
/// shape as `SharedEvac::run_worker`'s `outstanding`.
///
/// The decrement is in `Drop` for the reason recorded there: a worker that
/// unwound past its decrement would leave the count permanently above zero, and
/// a leaked count of exactly that kind once cost 3h08m of CPU in a suite that
/// finishes in seconds. Here it would not hang — it would make the cycle never
/// report convergence, so the coordinator would spin the marker until the next
/// pause forced the issue — which is quieter and therefore worse.
pub struct ActiveMarker<'a>(&'a AtomicUsize);

/// F-12 — one batch's worth of live-byte credits, flushed once instead of once
/// per marked object.
///
/// `G1Region::try_mark_and_measure` hands back the bytes a newly-marked object
/// contributes to its region's `marked_bytes_below_tams`, and this holds them
/// until the end of the batch. The point is the shape of the contention: a mark
/// cycle touches a handful of regions, so a per-object `fetch_add` puts N
/// markers on one cache line once per object. Batching turns that into one
/// read-modify-write per region per 32 objects.
///
/// A batch touches very few distinct regions, so a linear scan over a small
/// vector beats any map — and the vector is reused across batches, so the
/// steady state allocates nothing.
///
/// **The flush is in `Drop`, and that is the whole safety argument.** An
/// under-counted region looks deader than it is, and `cleanup` frees a
/// too-dead-looking Old region IN PLACE. Every exit from the batch — the normal
/// end, the `break` when the deque runs dry, and an unwind out of a scan — must
/// therefore flush, which is exactly what `Drop` guarantees and what an
/// explicit call at the bottom of the loop would not.
pub struct MarkLiveBytes<'a> {
    pub regions: &'a [G1Region],
    /// `(region index, bytes)`, in first-touched order.
    pub pending: Vec<(usize, usize)>,
}

impl<'a> MarkLiveBytes<'a> {
    pub fn new(regions: &'a [G1Region]) -> Self {
        Self {
            regions,
            pending: Vec::new(),
        }
    }

    #[inline]
    pub fn add(&mut self, region_idx: usize, bytes: usize) {
        if bytes == 0 {
            return;
        }
        if let Some(entry) = self.pending.iter_mut().find(|(i, _)| *i == region_idx) {
            entry.1 += bytes;
            return;
        }
        self.pending.push((region_idx, bytes));
    }
}

impl Drop for MarkLiveBytes<'_> {
    fn drop(&mut self) {
        for &(idx, bytes) in self.pending.iter() {
            self.regions[idx].add_marked_bytes(bytes);
        }
    }
}

impl<'a> ActiveMarker<'a> {
    #[inline]
    pub fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}

impl Drop for ActiveMarker<'_> {
    #[inline]
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Bound on the per-collection pause-record ring (§7 item 6). 64K records is
/// ~1.5 MB and covers a very long soak's most-recent window for p50/p99; older
/// records are evicted (and counted) so memory stays bounded.
pub const PAUSE_HISTORY_CAP: usize = 1 << 16;

/// How many [`GarbageCollector::needs_gc`] queries one mutator may answer from
/// [`G1Collector::free_region_count`] before it re-scans, as a backstop for a
/// region-type transition that forgot to publish a fresh count.
///
/// Every path that MOVES a region out of the Free pool already publishes
/// (`note_region_consumed_locked`, the TLAB refill's own reserve count) and a
/// collection publishes on its way out, so in practice the cache is exact.
/// This bound only decides how long a hypothetical missed transition could go
/// unnoticed for one mutator: 1024 allocations is at most ~40 KB of objects,
/// against a trigger that fires on 25% of the whole heap. The cadence is
/// deliberately thread-local: `needs_gc` is polled after every interpreted
/// allocation, so a collector-wide atomic counter would make every allocator
/// perform a cache-line-bouncing RMW merely to schedule this diagnostic
/// backstop.
pub const NEEDS_GC_RECOUNT_INTERVAL: usize = 1024;

/// One allocation-failure-only compaction operation.  The `target` range is
/// intentionally absent from `sources`: its Free members are left alone and
/// every movable blocker is evacuated into a Free region outside the range.
/// Consequently, a successful evacuation turns the entire range into one
/// contiguous humongous span without changing normal G1 placement policy.
#[derive(Clone, Debug)]
pub struct HumongousCompactionPlan {
    /// The allocation that failed.  The target is deliberately wider: a
    /// fallible native allocation preserves G1's TLAB reserve, so coalescing
    /// exactly this many regions would still leave a correctly refused retry.
    pub requested_regions: usize,
    pub target_start: usize,
    pub target_end: usize,
    pub sources: Vec<usize>,
    pub young_sources: usize,
    /// Humongous spans inside the target window that the pause relocates to a
    /// Free run outside it (`G1Collector::relocate_humongous_span`), as HotSpot's
    /// last-ditch full collection moves humongous objects (JDK-8191565).
    pub humongous_moves: Vec<HumongousMove>,
    /// `Some(target)`: a STAGING pause. No window of `target` regions could
    /// take its humongous span's destination, so this pause only evacuates
    /// the blockers of that destination run (the plan's target range), and
    /// `mixed_collection` re-arms the request at `target` regions for the
    /// next recovery pause, which then finds the run Free.
    pub staged_for: Option<usize>,
}

/// Staging pauses one allocation-failure ladder may run before the real
/// compaction (see `HumongousCompactionPlan::staged_for`). One clears the
/// destination run of the longest span; a second covers a window that holds
/// two spans neither of which has a run.
pub const HUMONGOUS_COMPACTION_MAX_STAGES: usize = 2;

/// One planned humongous relocation: the span `[src_start, src_start + len)`
/// moves to `[dst_start, dst_start + len)`, which is Free and outside the
/// compaction's target window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HumongousMove {
    pub src_start: usize,
    pub len: usize,
    pub dst_start: usize,
}

/// One character per region, for the allocation-failure compaction's
/// failure line: `.` Free, `E` Eden, `S` Survivor, `O` Old (`o` with no mark
/// data), `H` humongous start, `h` continuation; upper/lower case swapped to
/// `*` when the region is pinned.
pub fn region_type_map(regions: &[G1Region], pinned: &RegionSet) -> String {
    regions
        .iter()
        .enumerate()
        .map(|(i, r)| {
            if r.pinned || pinned.contains(&i) {
                return '*';
            }
            match r.region_type() {
                RegionType::Free => '.',
                RegionType::Eden => 'E',
                RegionType::Survivor => 'S',
                RegionType::Old if r.live_bytes == 0 => 'o',
                RegionType::Old => 'O',
                RegionType::HumongousStart => 'H',
                RegionType::HumongousContinuation => 'h',
            }
        })
        .collect()
}

/// Humongous spans the allocation-failure compaction relocated
/// ([`G1Collector::relocate_humongous_span`]), process lifetime.
pub static HUMONGOUS_RELOCATED: AtomicU64 = AtomicU64::new(0);



/// The objects the last trustworthy completed mark proves dead.
///
/// An object is proven dead when its region is Old in the same incarnation
/// (`reuse_epoch`) and type the mark-start snapshot recorded, it lies below
/// that region's TAMS, its mark bit is clear, and it is not in `keep` (the
/// closure of registered finalizable objects, which finalization resurrects).
/// Such an object was unreachable at the snapshot, so nothing can reach it
/// now: this is the evidence `cleanup` frees whole Old regions on. Objects
/// allocated or copied into a region after the snapshot sit at or above its
/// TAMS, and a recycled region carries a new epoch, so neither is ever
/// proven dead.
pub struct MarkProvenDead {
    pub snapshot: Vec<(u64, usize, RegionType)>,
    pub keep: FxHashSet<usize>,
}

/// gci i18/nr (i17/nr N1): whether a completed mark covers every JIT-pinned
/// object, i.e. whether [`MarkProvenDead`] may be built at all. True only
/// when the remark screens its roots AND seeds every published JIT pin
/// (`CRATONVM_G1_MARK_ROOT_SCREEN`, `CRATONVM_G1_REMARK_SEEDS_JIT_PINS`,
/// both default ON) -- the same condition `cleanup`'s bitmap scrub requires
/// before it scrubs a JIT-pinned region.
pub fn mark_proof_covers_jit_pins(mark_root_screen: bool, remark_seeds_jit_pins: bool) -> bool {
    mark_root_screen && remark_seeds_jit_pins
}

/// gci i22/g1rk: [`G1Collector::is_live_after_mark`] over a copy of the
/// mark-start snapshot taken once ([`G1Collector::remark_liveness`],
/// [`G1Collector::trusted_mark_liveness`]). Borrows the collector; holds no
/// lock between calls.
pub struct RemarkLiveness<'a> {
    pub collector: &'a G1Collector,
    pub snapshot: Vec<(u64, usize, RegionType)>,
}

impl RemarkLiveness<'_> {
    /// `is_live_after_mark(addr)`, the snapshot read from the copy.
    pub fn is_live(&self, addr: usize) -> bool {
        let regions = self.collector.regions.read();
        let Some(idx) = self.collector.region_for_ptr(&regions, addr as *mut u8) else {
            return true;
        };
        let region = &regions[idx];
        if region.region_type() == RegionType::Free {
            return false;
        }
        G1Collector::live_against_mark_snapshot(region, idx, &self.snapshot, addr)
    }
}

impl MarkProvenDead {
    pub fn proves_dead(&self, region: &G1Region, idx: usize, addr: usize) -> bool {
        let Some(&(epoch, tams, region_type)) = self.snapshot.get(idx) else {
            return false;
        };
        if region_type != RegionType::Old
            || region.region_type() != RegionType::Old
            || epoch != region.reuse_epoch
        {
            return false;
        }
        let base = region.data.as_ptr() as usize;
        let Some(offset) = addr.checked_sub(base) else {
            return false;
        };
        offset < tams && !region.mark_bitmap.is_marked(addr) && !self.keep.contains(&addr)    }}

/// gci i11/gm finding 3 -- which old regions [`G1Collector::choose_old_cset`]
/// may consider for this pause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OldCsetSource {
    /// A mixed pause: marked Old regions below the live threshold, capped at
    /// `old_cset_region_threshold_percent` and the copy budget.
    Mixed,
    /// A `System.gc()` full compaction (gcd d10/o): `full_compaction_candidates`,
    /// uncapped and unbudgeted. `young_bytes` is the young half's occupancy.
    FullCompaction { young_bytes: usize },
    /// An allocation-failure compaction plan: the plan's sources are the old
    /// half and nothing is chosen.
    Plan,
}

/// gci i11/gm finding 3 -- what [`G1Collector::choose_old_cset`] chose.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OldCsetChoice {
    /// The old regions taken, in rank order.
    pub picked: Vec<usize>,
    /// Candidates before the percentage cap truncated them.
    pub available: usize,
    /// The percentage cap in force (`usize::MAX` on a full compaction).
    pub max_old: usize,
    /// The copy budget stopped the take before the cap or the candidates did.
    pub budget_capped: bool,
    /// The marginal fix-up price was charged on at least one taken region.
    pub fixup_charged: bool,
    /// g1ep7: mixed candidates the exclusion set (this pause's pinned
    /// regions) kept out; 0 on the plan and full-compaction arms.
    pub excluded: usize,
}

/// A just-compacted run reserved for the allocation that proved it needed
/// that exact span.  Keeping this separate from ordinary Free placement is
/// what makes the allocation-failure compaction a transaction rather than a
/// diagnostic: mutators resume after the pause, so they must not consume the
/// newly contiguous run before the failed allocation gets its retry.
#[derive(Clone, Copy, Debug)]
pub struct HumongousCompactionReservation {
    pub start: usize,
    pub end: usize,
    pub requested_regions: usize,
}

thread_local! {
    /// Per-mutator cadence for G1's free-region-cache drift guard. This is not
    /// GC policy or VM state: each query still reads the same collector-local
    /// cache and runs the same trigger arithmetic. It only decides which
    /// caller pays the rare O(regions) validation scan.
    pub static NEEDS_GC_RECOUNT_TICK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

