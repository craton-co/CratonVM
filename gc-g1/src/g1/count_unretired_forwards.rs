#[allow(unused_imports)]
use super::*;

impl G1Collector {
    /// Count objects still carrying a FORWARDED mark after [`Self::retire_forwards`].
    ///
    /// `CRATONVM_G1_VERIFY_FORWARDS_RETIRED=1` only, and a COUNTER rather than a
    /// repair on purpose. `retire_forwards` clears every key of the pointer map,
    /// so a forward can only survive it by never reaching that map — which is a
    /// shape this file has fixed three times (the parallel driver's own retire
    /// ordering, Phase 3.5's forwards, and the CAS-loser arm that did not record
    /// its adopted forward). A survivor in a KEPT region outlives the pause and
    /// is read as a header by the next one.
    ///
    /// Measure before repairing: a default-ON sweep that neutralises marks is a
    /// write to every CSet region, and this branch has already shipped one
    /// default-ON screen whose refusals were all healthy objects. If this
    /// counter is non-zero the repair is justified and the count says how much
    /// it is worth; if it is zero the failure path leaks nothing and the repair
    /// would be unmeasured risk for no benefit.
    pub fn count_unretired_forwards(
        &self,
        regions: &[G1Region],
        cset: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
    ) -> usize {
        if !gc_flags().g1_verify_forwards_retired {
            return 0;
        }
        // Under `CRATONVM_G1_RETIRE_KEPT_ONLY` only a KEPT region's forwards
        // are retired (the rest are freed in Phase 5), so only a kept region
        // can hold a survivor that matters.
        let kept = gc_flags()
            .g1_retire_kept_only
            .then(|| self.self_forwarded_regions(pointer_map));
        let mut found = 0usize;
        // The CSet is Eden / Survivor / Old, and an Eden region is exactly
        // where TLAB retirement leaves its sentinels. Neither a frozen peer's
        // reserved tail nor a sub-header gap filler is a walkable object, and
        // reading one as a header here does more than desync the stride: the
        // mark-word read below is at `+8`, which for an 8-byte gap at the very
        // top of a region is past the region's own span. Every other linear
        // walker in this file takes both lists; this one did not.
        //
        // Lane B reached the same fix independently in the 2026-09-20 round,
        // which is worth recording for the reason rather than the coincidence:
        // this counter exists to decide whether a repair is justified, and its
        // whole value is that a ZERO is trustworthy. A walk that manufactures
        // non-zero counts out of filler bytes would have argued for a
        // default-ON write to every CSet region.
        let jit_skips = self.jit_tlab_skip_spans();
        for i in cset.iter() {
            if kept.as_ref().is_some_and(|kept| !kept.contains(&i)) {
                continue;
            }
            let Some(r) = regions.get(i) else { continue };
            let base = r.data.as_ptr() as usize;
            let cursor = r.cursor();
            let mut off = 0usize;
            while off < cursor {
                let addr = base + off;
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, addr) {
                    off += skip;
                    continue;
                }
                if let Some(gap) = gap_filler_len(addr as *const u8) {
                    off += gap;
                    continue;
                }
                // SAFETY: below the region's own cursor, under the regions lock,
                // and Phase 5 has not run.
                let h = unsafe { &*(addr as *const ObjectHeader) };
                if is_humongous_filler(h) {
                    // The filler covers the rest of the region by construction.
                    break;
                }
                let m = h.mark_word.load(Ordering::Relaxed);
                // The quartet survives retirement, so the size is readable
                // whether or not the mark is still forwarded.
                let size = object_total_size(h);
                if ObjectHeader::is_forwarded_mark(m) {
                    found += 1;
                    if found <= 4 {
                        tracing::warn!(
                            "[g1] UNRETIRED FORWARD (#{found}): obj=0x{:x} region={i} \
                             off=0x{off:x} mark={m:#018x} target=0x{:x} class_id={} \
                             -- still FORWARDED after `retire_forwards`, so it never \
                             reached the pointer map. If this region is kept, the next \
                             pause reads this word as a header.",
                            base + off,
                            h.forwarding_address() as usize,
                            h.class_id.as_u32(),
                        );
                    }
                }
                // Lane B — `size < HEADER_SIZE`, not `size == 0`. Every other
                // object walk in this file uses the stronger bound, and the
                // weak one here was not equivalent: `object_total_size` can
                // return a non-zero value below one header (its own `0`
                // sentinel is only one of the implausible answers a garbage
                // header produces), and advancing by such a value steps the
                // walk to an address that is not 8-aligned to an object start
                // for the rest of the region. The stop is the same stop the
                // phase-4 walk and the V7b verifier take.
                if size < HEADER_SIZE || size > cursor - off {
                    break;
                }
                off += size;
            }
        }
        if found > 0 {
            UNRETIRED_FORWARDS.fetch_add(found, Ordering::Relaxed);
        }
        found
    }

    /// F-02 — clear the forwarding tag from every from-space object this pause
    /// forwarded, restoring the "no object carries a forward at collection
    /// start" invariant.
    ///
    /// The serial evacuator installs forwards in from-space mark words since
    /// F-02, which means it inherits the obligation the parallel evacuator
    /// already carries (its DEFECT-2 part 2). Phase 5 zero-fills a FREED
    /// region, so a forward left in one is harmless — but a region holding a
    /// self-forwarded object is KEPT, and every from-space body in it survives
    /// the pause. A forward left on one of those is read by the NEXT cycle's
    /// fast path as a this-cycle answer, which strands a root on an abandoned
    /// object.
    ///
    /// Clearing every key rather than only the self-forwards (`k == v`) is
    /// deliberate and is the correction that half-fix needed: the
    /// normally-evacuated bodies left behind in a kept region are stale in
    /// exactly the same way.
    ///
    /// NEUTRAL is the right resting lock state — the live copy carries the mark
    /// word this evacuation transferred to it — but the QUARTET must survive:
    /// `kind` and `element_type` are what every linear region walker sizes a
    /// from-space object from, and Phase 5 has not zeroed the region yet.
    /// Storing a bare `MARK_NEUTRAL` here would leave an abandoned copy
    /// claiming to be a zero-slot plain object.
    ///
    /// Must run AFTER Phase 4 (which resolves forwards) and BEFORE Phase 5.
    ///
    /// Only sources in a region the pause KEEPS are retired
    /// (`CRATONVM_G1_RETIRE_KEPT_ONLY`, default ON; `=0` retires every key).
    /// Phase 5 frees every other CSet region, a freed region is never read
    /// (every walker skips `RegionType::Free`), and the allocator zeroes every
    /// byte it hands out again (see `G1Region::reset`). So restoring a
    /// forwarded-away body there only paid a random header access per copied
    /// object: on binary trees, ~10% of a GC-bound run's samples, for
    /// nothing. The kept set is [`Self::self_forwarded_regions`], the same
    /// computation `free_or_keep_cset` keeps by. A key outside `cset` (not
    /// expected) is still retired.
    pub fn retire_forwards(&self, pointer_map: &cratonvm_types::PointerMap, cset: &RegionSet) {
        let kept_only = gc_flags().g1_retire_kept_only;
        // g1sz: with nothing self-forwarded the pause keeps no region, and
        // every key of its map names a collection-set object (`evacuate` is
        // reached only for those), so the loop below would skip every entry.
        // It walked the whole map to do so: 540 k forwards per young pause on
        // `BinTreesClassic 18`.
        if kept_only && self.no_self_forward_this_pause() {
            return;
        }
        // g1pin: under kept-only, every forward outside the CSet regions this
        // pause frees, by key range (`for_each_forward_outside_regions`).
        let freed: RegionSet = if kept_only {
            let kept = self.self_forwarded_regions(pointer_map);
            cset.iter().filter(|i| !kept.contains(i)).collect()
        } else {
            RegionSet::default()
        };
        self.for_each_forward_outside_regions(pointer_map, &freed, |k, v| {
            // SAFETY: `k` is a from-space object address forwarded this pause;
            // its header is intact and its region is held under the
            // collection's `regions` lock (Phase 5 has not run yet).
            // `retire_forwarding_from` also restores a forwarded-away body's
            // second word (a long header's shape and hash, a compact
            // instance's first field) from the copy, so the abandoned body
            // stays sized and walkable exactly as it was before the forward.
            //
            // The copy is found through the map's RECORDED target, not the
            // header: since the 8-byte header the target sits in the source's
            // second word, a body word, and a store into it during the pause
            // would be followed as a pointer. A disagreement is counted and
            // reported rather than trusted.
            unsafe {
                let h = &*(k as *const ObjectHeader);
                let mark = h.mark();
                let recorded = if v != k
                    && ObjectHeader::is_forwarded_mark(mark)
                    && mark & cratonvm_types::MARK_FWD_SELF == 0
                {
                    let in_header = h.forwarding_address() as usize;
                    if in_header != v {
                        let n = RETIRE_TARGET_MISMATCH.fetch_add(1, Ordering::Relaxed) + 1;
                        if n <= 8 || n.is_power_of_two() {
                            tracing::error!(
                                "[g1] retire: a forward's target word no longer names its \
                                 recorded copy (#{n}): holder=0x{k:x} class_id={} mark={mark:#010x} \
                                 header_target=0x{in_header:x} recorded=0x{v:x} -- something \
                                 stored into the abandoned source's second word during the pause",
                                h.class_id.as_u32(),
                            );
                        }
                    }
                    v as *mut u8
                } else {
                    std::ptr::null_mut()
                };
                h.retire_forwarding_from(recorded);
            }
        });
        // Self-forwards (evacuation failure): the object did not move, so its
        // pre-forward lock state and identity hash come back whole.
        //
        // Only for an address THIS pause's map records as a self-forward
        // (`key == value`), and only the latest saved mark per address. The
        // list lives on the collector, so an entry a path pushed without
        // reaching a retire would otherwise be replayed by a LATER pause onto
        // whatever that address holds by then -- another object, or the middle
        // of one.
        let saved_marks = std::mem::take(&mut *self.self_forward_marks.lock());
        let mut latest: rustc_hash::FxHashMap<usize, u32> = rustc_hash::FxHashMap::default();
        for (addr, saved) in saved_marks {
            latest.insert(addr, saved);
        }
        for (addr, saved) in latest {
            if pointer_map.get(&addr) != Some(&addr) {
                continue;
            }
            // SAFETY: `addr` is an object this pause self-forwarded (an
            // identity key of its map); its region is kept (a self-forward
            // pins it) and still held here.
            unsafe { (*(addr as *const ObjectHeader)).restore_self_forwarded_mark(saved) };
        }
    }

    /// Evacuate a single object from its current region to Survivor or Old.
    /// Returns `Some((new_ptr, fresh))` where `fresh` is `true` iff THIS call
    /// performed the copy (vs found an existing forwarding entry), or `None`
    /// on evacuation failure.
    ///
    /// Step 9 (parallel-evac foundation): `fresh` is the dedup signal callers
    /// use to gate the work_list push, replacing a separate
    /// `pointer_map.contains_key` pre-check — that pre-check is a TOCTOU the
    /// moment evacuation is sharded across worker threads (two workers both
    /// sample "absent", both copy, the loser's allocation leaks and is
    /// double-scanned). Reading the freshness from the evacuation outcome
    /// instead means the parallel evacuator can later derive it from the
    /// atomic CAS-forwarding install in the object header (`forwarding_ptr`)
    /// without changing the call sites.
    #[track_caller]
    pub fn evacuate_object(
        &self,
        regions: &mut Vec<G1Region>,
        old_ptr: *mut u8,
        pointer_map: &mut cratonvm_types::PointerMap,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        cset: &RegionSet,
    ) -> Option<(*mut u8, bool)> {
        self.evacuate_object_tuned(
            regions,
            old_ptr,
            pointer_map,
            objects_copied,
            bytes_copied,
            cset,
            &mut SerialEvacKnobs::off(),
            false,
        )
    }

    /// [`Self::evacuate_object`] with the serial Cheney loop's per-pause
    /// knobs (JIT round 23 wave 8, lane L9; see [`SerialEvacKnobs`]). The
    /// copy, the forward and every census are those of `evacuate_object`;
    /// only HOW the object's size and shape are read changes.
    #[track_caller]
    pub fn evacuate_object_tuned(
        &self,
        regions: &mut Vec<G1Region>,
        old_ptr: *mut u8,
        pointer_map: &mut cratonvm_types::PointerMap,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        cset: &RegionSet,
        knobs: &mut SerialEvacKnobs,
        candidate_proved: bool,
    ) -> Option<(*mut u8, bool)> {
        let old_addr = old_ptr as usize;

        // F-02 — the "already forwarded?" test is a LOAD AND A TAG COMPARE, not
        // a hash probe.
        //
        // Since the 32 -> 24 header shrink the forwarding slot IS the mark word,
        // and the parallel evacuator has always used it (`SharedEvac::evacuate`
        // CAS-installs there). The serial path kept its answer in the per-cycle
        // `pointer_map` instead and paid `FxHashMap::get` on every visit to
        // every reference slot pointing into the collection set — inside the
        // Cheney closure, which is the largest phase of a young pause.
        //
        // The two evacuators now share one protocol, which is worth as much as
        // the cycles: "where did this object go" had two answers maintained by
        // two mechanisms, and only one of them was visible to the other path.
        //
        // `pointer_map` is still built. It is the pause's OUTPUT — the VM's
        // root/monitor/JNI-handle/dedup-table/mark-worklist remaps all consume
        // it — it is what Phase 5 reads to tell a self-forward from a real
        // copy, and it is the list `retire_forwards` walks. What it stops being
        // is the collector's own lookup structure.
        //
        // SAFETY: every caller has already established that `old_ptr` is a
        // plausible object header inside a live CSet region (roots go through
        // `note_root_object_plausibility`, slots through
        // `evacuation_candidate_is_an_object`), and the regions guard is held.
        // The serial twin of the screen in `SharedEvac::evacuate`; see it for
        // what an interior address costs here.
        //
        // JIT round 23 wave 9, lane L9 (`CRATONVM_G1_SERIAL_EVAC_SCREEN_ONCE`,
        // default ON, `=0` kills): a slot candidate the scan just proved with
        // `evacuation_candidate_is_an_object` (`candidate_proved`) skips this
        // screen's `classify_candidate_header_view` half -- the SAME pure
        // function of the SAME address over the SAME region table, asked a
        // line earlier on this thread with nothing written in between (the
        // pause is stopped-the-world and serial), so it answers `Object` again.
        // The screen's other half, `note_implausible_legacy_header_view`, still
        // runs: the candidate screen refuses an implausible header only under
        // the opt-in `CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE`, and this screen
        // refuses it always, so keeping that call keeps the verdict -- and its
        // census -- byte for byte. `classify_candidate_header_view` was 7.9%
        // self of the serial pause on n23-w8g `KeepLong`.
        let followable = |regions: &Vec<G1Region>| {
            if candidate_proved && knobs.screen_once {
                // SAFETY: as for the candidate screen that proved this address.
                let header = unsafe { &*(old_ptr as *const ObjectHeader) };
                !self.note_implausible_legacy_header_view(
                    RegionView::Slice(regions),
                    old_ptr,
                    header,
                    "serial-evacuate",
                )
            } else {
                self.addr_is_followable_object_view(
                    RegionView::Slice(regions),
                    old_addr,
                    "serial-evacuate",
                    GridProof::No,
                )
            }
        };
        if !followable(regions) {
            let n = EVAC_SUPPLY_NON_OBJECT.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                tracing::warn!(
                    "[g1] serial evacuation REFUSED a candidate that is not an OBJECT \
                     START (#{n}): addr=0x{old_addr:x} caller={} {}",
                    std::panic::Location::caller(),
                    self.describe_rejected_address(regions, old_addr),
                );
            }
            return None;
        }
        let mark_atomic = unsafe { &(*(old_ptr as *const ObjectHeader)).mark_word };
        let observed = mark_atomic.load(Ordering::Acquire);

        if ObjectHeader::is_forwarded_mark(observed) {
            // Same validation as the parallel twin's fast path; see
            // `Self::decode_forwarding_target`. A word that is not a forward
            // this collector installed must not be answered with as if it were.
            return self
                .decode_forwarding_target(observed, old_addr)
                .map(|t| (t, false));
        }

        // PREDICT-THEN-VERIFY BACKSTOP. The tag compare above is the whole
        // "already evacuated?" test, and it is only correct while no phase of
        // the pause has retired the tags. `pointer_map` is the pause's other
        // record of the same fact and is never retired, so it can say whether
        // the tag compare just lied -- for one hash probe on the path that is
        // about to memcpy an object, which is not the per-slot cost F-02
        // removed.
        //
        // A hit means a second copy of a LIVE object was about to be made and
        // this address's `pointer_map` entry about to be overwritten with the
        // duplicate. That is not a degradation, it is an identity split, and it
        // is silent because the duplicate is well-formed. Count it always (the
        // count is the only evidence a retirement-order regression exists at
        // all) and, unless `CRATONVM_G1_REEVAC_GUARD=0`, answer from the map
        // exactly as the tag would have.
        //
        // JIT round 23 wave 8, lane L9 (`CRATONVM_G1_REEVAC_PROBE_BEFORE_RETIRE_SKIP`,
        // default ON, `=0` kills): the probe is skipped where it cannot hit,
        // i.e. in a Cheney loop that runs before its pause retires any forward
        // (`SerialEvacKnobs::before_retire`, set only by the young and mixed
        // drivers' Phase-3 loops, whose `retire_forwards` comes after). Until
        // a retirement, every `pointer_map` key has its tag: every insert in
        // those drivers before Phase 3 is `evacuate_object`'s (tag first, via
        // `set_forwarding_address`, on the copy and the self-forward arms) or
        // `relocate_humongous_span`'s (tag first too), nothing in a pause
        // clears a tag but `retire_forwards` and the self-forward restore
        // after it, and the serial arm never writes BUSY. So an untagged
        // address here is not a key, and the probe would miss: n23-w8g
        // sampled it at 9.8% of the serial pause. Debug builds still probe and
        // assert that.
        let probe = !knobs.before_retire || cfg!(debug_assertions);
        if let Some(&existing) = probe.then(|| pointer_map.get(&old_addr)).flatten() {
            debug_assert!(
                !knobs.before_retire,
                "r23 w8 L9: a pre-retirement Cheney loop found untagged 0x{old_addr:x} \
                 already forwarded to 0x{existing:x}; the probe skip is unsound"
            );
            let n = REEVACUATED_AFTER_RETIRE.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                tracing::warn!(
                    "[g1] evacuation asked to RE-EVACUATE 0x{old_addr:x} (#{n}): this pause \
                     already forwarded it to 0x{existing:x}, but its mark word no longer \
                     carries the tag, so the copy path was about to make a SECOND live copy \
                     and overwrite the forwarding entry with it. A driver retired its \
                     forwards before a phase that still evacuates."
                );
            }
            {
                return Some((existing as *mut u8, false));
            }
        }

        let header = unsafe { &*(old_ptr as *const ObjectHeader) };
        // `CRATONVM_G1_SERIAL_EVAC_SIZE_MEMO`: the same answer as
        // `object_total_size(header)` (see `WalkSizeMemo`).
        let obj_size = knobs.sizes.size(header);

        // gc-abort-cleanup (mirrors gc.rs::try_forward_object): a corrupt /
        // implausible header makes `object_total_size` return the `0` sentinel
        // (or otherwise an implausible size). Refuse to evacuate it rather than
        // memcpy `0`/garbage bytes and install a bad forwarding entry; bail out
        // as an evacuation failure (`None`) with a diagnostic instead of
        // aborting the VM.
        if obj_size < HEADER_SIZE {
            tracing::warn!(
                "g1::evacuate_object: refusing to evacuate object at {:p} — implausible \
                 size {} (kind=0x{:02x}, num_slots={}, array_len={}); corrupt header",
                old_ptr,
                obj_size,
                ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                header.num_slots(),
                header.array_length(),
            );
            return None;
        }

        self.note_implausible_legacy_header(regions, old_ptr, header, "evacuate-src");

        // Decide destination based on age. F-18: the threshold is re-derived
        // after every pause from the age histogram, so it may be below the
        // configured `promotion_age` when survivor space is under pressure.
        // gci i23/tn: `survivor_full_serial` -- a Survivor copy of this pause
        // already failed under the Survivor region cap, so this one goes to
        // Old without the scan (HotSpot's worker `_tenuring_threshold = 0`).
        // gci i47w/sg47: `evac_tenuring_threshold` is 0 on a tenure-all
        // `System.gc()` pause, so every copy goes to Old.
        let by_age = header.gc_age() >= self.evac_tenuring_threshold();
        let mut promote = by_age || self.survivor_full_serial.load(Ordering::Relaxed);
        let dest_type = if promote {
            RegionType::Old
        } else {
            RegionType::Survivor
        };

        // gci i13/ef finding 9 (`CRATONVM_G1_SURVIVOR_OVERFLOW_TO_OLD`, default
        // OFF): HotSpot's `G1ParScanThreadState::allocate_copy_slow` retries a
        // failed young-destination allocation in Old before it fails the
        // evacuation. A Survivor copy that found no room therefore tries a
        // non-CSet Old region (or a Free one) and, on success, is a promotion
        // in every respect below (`GC_FLAG_OLD_GEN` stamp, promoted bytes).
        //
        // gci i23/tn (`CRATONVM_G1_SURVIVOR_REGION_CAP`, default ON): the same
        // retry when the pause's Survivor region cap refused the region, with
        // its own census (`survivor_cap_promotions`).
        // g1pin: a peer-stack pinned object takes the evacuation-failure arm
        // below, and no destination is tried for it (`g1_pins_peer_stack_objects`).
        let pinned = self.peer_object_pinned(old_addr);
        let alloc = if pinned {
            None
        } else {
            self.alloc_in_type_locked(regions, dest_type, obj_size, cset)
        };
        let placed = match alloc {
            None if pinned => None,
            None if !promote && self.survivor_cap_reached() => {
                self.survivor_full_serial.store(true, Ordering::Relaxed);
                let overflow = self.alloc_in_type_locked(regions, RegionType::Old, obj_size, cset);
                if overflow.is_some() {
                    promote = true;
                    self.survivor_cap_promotions
                        .fetch_add(1, Ordering::Relaxed);
                }
                overflow
            }
            None if !promote && self.survivor_overflow_to_old => {
                let overflow = self.alloc_in_type_locked(regions, RegionType::Old, obj_size, cset);
                if overflow.is_some() {
                    promote = true;
                    self.survivor_overflow_promotions
                        .fetch_add(1, Ordering::Relaxed);
                }
                overflow
            }
            // gci i23/tn: a copy sent to Old by the survivor-full latch (not
            // by its age) is the cap's promotion too.
            Some(p) if promote && !by_age => {
                self.survivor_cap_promotions
                    .fetch_add(1, Ordering::Relaxed);
                Some(p)
            }
            other => other,
        };
        let new_ptr = match placed {
            Some(p) => p,
            None => {
                // EVACUATION FAILURE (to-space exhausted: no non-CSet region of the
                // destination type has room and no Free region is left). Real G1
                // "self-forwards" such an object — keeps it IN PLACE rather than
                // dropping it. The previous `?` returned None here, so the caller
                // skipped the object: its referrers' slots kept pointing into a
                // CSet region that Phase 5 then reset/freed → silent live-object
                // loss (a wrong result under memory pressure where the
                // generational collector correctly OOMs).
                //
                // Self-forward: install old→old in the pointer map (an identity
                // forward) and return the object at its current address with
                // `fresh = true` so the caller still scans its fields (refs to
                // objects that DID evacuate are rewritten; refs to other
                // self-forwarded objects stay put) and the referrer's slot is
                // rewritten to `old_ptr` (a no-op — the object did not move).
                // Phase 5 detects self-forwarded objects (key == value in the
                // pointer map) and KEEPS their regions instead of freeing them,
                // so nothing is lost. The heap is then simply not reclaimed → the
                // triggering mutator allocation fails → a clean, catchable
                // OutOfMemoryError, exactly as the generational collector does.
                // F-02: install the identity forward in the header too, so a
                // later visit in this same pause takes the tag-compare fast
                // path above instead of re-deciding. `retire_forwards` clears
                // it before Phase 5 — which matters most here, because a
                // self-forwarded object's region is KEPT, so a forward left on
                // it would outlive the pause.
                let saved = unsafe { (*(old_ptr as *const ObjectHeader)).mark() };
                if !ObjectHeader::is_forwarded_mark(saved) {
                    self.self_forward_marks.lock().push((old_addr, saved));
                }
                unsafe { (*(old_ptr as *const ObjectHeader)).set_forwarding_address(old_ptr) };
                pointer_map.insert(old_addr, old_addr);
                self.note_self_forward(old_addr);
                return Some((old_ptr, true));
            }
        };

        // The SOURCE's shape, before anything is copied. Compared against the
        // destination below: "the source was already not an object" and "the
        // copy produced something that is not an object" are different defects
        // with different producers, and every report this page has carried so
        // far read the destination only -- after the evacuator had already
        // written it -- so it could not tell them apart.
        // `CRATONVM_G1_EVAC_SHAPE_KEY`: see `evac_shape_key`.
        let src_shape = evac_shape_key(header, knobs.shape_key);

        // Copy object data
        unsafe {
            std::ptr::copy_nonoverlapping(old_ptr, new_ptr, obj_size);
        }

        // Round-2 fix (T2-4): explicit atomic load+store for the mark_word
        // field. The bulk memcpy above is technically UB for `AtomicU64`:
        // even under STW the memory model requires atomic ops on atomic
        // locations. Replicate the mark word atomically so subsequent CAS
        // operations (monitor inflation, etc.) on the new copy observe a
        // properly synchronized initial value.
        //
        // NOTE: a future concurrent G1 collector needs a different
        // forwarding protocol — CAS-install the forwarding pointer and
        // re-read the mark word if a mutator raced the evacuation.
        //
        // F-02: `observed`, NOT a fresh load — the same reasoning the parallel
        // evacuator states at its own mark-word transfer. The mark word is the
        // forwarding slot, so re-reading it after the install below would stamp
        // the DESTINATION as forwarded. The snapshot was taken before the copy
        // and is known non-forwarded.
        // JIT round 23 G1 evacuation page: one plain store of the finished
        // mark word (`destination_mark`, `CRATONVM_G1_EVAC_ONE_STORE_MARK`);
        // the copy is this pause's alone until the forward below names it.
        let dest_mark = destination_mark(observed, promote, knobs.one_store_mark);
        // SAFETY: both pointers reference a fully written ObjectHeader.
        unsafe {
            let new_header_ptr = new_ptr as *mut ObjectHeader;
            (*new_header_ptr)
                .mark_word
                .store(dest_mark.unwrap_or(observed), std::sync::atomic::Ordering::Relaxed);
        }

        // Increment GC age on the new copy
        let new_header = unsafe { &mut *(new_ptr as *mut ObjectHeader) };
        if dest_mark.is_some() {
            if !promote {
                // F-18: this object is in survivor space at the age it now carries.
                self.note_survivor_age(new_header.gc_age(), obj_size);
            }
        } else if promote {
            // G1AUD-1 — stamp the header's old-generation bit on promotion.
            //
            // G1 does not need this bit itself (region type is authoritative,
            // and `region_for_ptr` is O(log R)); the JIT does. Every inline
            // reference-store fast path in `jit/src/x64.rs` decides "no post
            // barrier needed" from `gc_flags & GC_FLAG_OLD_GEN == 0`
            // (`:8778`, `:8846`, `:16741`, `:16810`). The INT-6 mitigation
            // intended G1 receivers to be routed to the full-barrier helper by
            // the published-region containment guard instead — but two emitters
            // reach the same inline store WITHOUT that guard (the
            // `receiver_is_trusted_oop` arms at `:16707`/`:16795`, which emit a
            // bare null check, and `emit_inline_fresh_ctor_compact_ref_putfield`
            // at `:8825`, which emits none). On an unstamped G1 heap every
            // receiver reads as young there, so a null->non-null store into a
            // promoted object skipped `post_write_barrier_rset` and the
            // old->young edge never entered the remembered set — the next young
            // pause frees the still-live referent (UAF).
            //
            // Stamping makes the JIT's own old-generation test fire, which is
            // the fail-safe direction: the store takes `jit_putfield_object`,
            // which runs the complete SATB + RSet barrier pair. Nothing inside
            // this crate reads the bit on a G1 heap (`grep '\.gc_flags'
            // gc/src/g1.rs`), and `concurrent_mark`'s header validator already
            // lists it as a known flag, so the stamp is invisible to G1 itself.
            new_header.add_gc_flags(GC_FLAG_OLD_GEN);
        } else {
            new_header.set_gc_age(new_header.gc_age().saturating_add(1));
            // F-18: this object is in survivor space at the age it now carries.
            self.note_survivor_age(new_header.gc_age(), obj_size);
        }
        // The destination must carry the source's identity. `set_gc_age` and
        // `add_gc_flags` touch only quartet bits; nothing here may change the
        // class, the shape or the kind.
        let dst_shape = evac_shape_key(new_header, knobs.shape_key);
        if dst_shape != src_shape {
            // The report spells the decoded shapes whichever key compared.
            let src_shape = (header.class_id.as_u32(), header.num_slots(), header.kind());
            let dst_shape = (
                new_header.class_id.as_u32(),
                new_header.num_slots(),
                new_header.kind(),
            );
            let n = EVAC_COPY_SHAPE_DRIFT.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                tracing::warn!(
                    "[g1] evacuation COPY CHANGED an object's shape (#{n}): old={:#x} new={:#x} size={obj_size:#x} src=(class {}, slots {}, {:?}) dst=(class {}, slots {}, {:?}) -- the memcpy and the two quartet updates are the only writes here, so a difference means the destination span was not exclusively this object's.",
                    old_addr,
                    new_ptr as usize,
                    src_shape.0, src_shape.1, src_shape.2,
                    dst_shape.0, dst_shape.1, dst_shape.2,
                );
            }
        }
        self.note_implausible_legacy_header(regions, new_ptr, new_header, "evacuate-dest");

        // F-02: install the forward on the FROM-space header, after the
        // destination is fully written. Ordering matters for the same reason it
        // does in the parallel evacuator: the forward is what makes the copy
        // findable, so it must not become visible before the copy is complete.
        unsafe { (*(old_ptr as *const ObjectHeader)).set_forwarding_address(new_ptr) };
        pointer_map.insert(old_addr, new_ptr as usize);
        *objects_copied += 1;
        *bytes_copied += obj_size;
        // Counted HERE rather than in the `if promote` block above, so the two
        // evacuators agree on what a promotion is: bytes that reached an Old
        // region in a copy this pause published. The evacuation-FAILURE arm
        // returns before this point, which is correct — a self-forwarded object
        // did not move.
        if promote {
            self.note_promoted_bytes(obj_size);
        }

        Some((new_ptr, true))
    }

    // FIVE doc blocks used to sit here, stacked on the one function below:
    // `scan_and_evacuate_refs`'s, `note_implausible_legacy_header`'s,
    // `holder_word0_arena_pointer`'s, `holder_body_cannot_fit_a_region`'s and
    // this one's. Their functions moved and the docs did not, so rustdoc
    // rendered four unrelated contracts on `addr_is_in_humongous_region` while
    // the four functions they belong to had none at all. Each is now on its own
    // item.
    /// Is `addr` inside a HUMONGOUS span? Such an object legitimately spans
    /// more than one region, so "its size exceeds a region" says nothing about
    /// it and must not be read as a refusal.
    pub fn addr_is_in_humongous_region(&self, regions: &[G1Region], addr: usize) -> bool {
        self.lookup_region_for_addr(addr)
            .and_then(|i| regions.get(i))
            .is_some_and(|r| {
                matches!(
                    r.region_type(),
                    RegionType::HumongousStart | RegionType::HumongousContinuation
                )
            })
    }

    /// The holder's declared body, when it CANNOT FIT IN A REGION.
    ///
    /// If an object's own header sizes it larger than a whole region, no
    /// allocator placed it here and the count is not a count. Independent of
    /// [`Self::holder_word0_arena_pointer`]: this catches the shape MEASURED as
    /// that screen's residual on H2 (2026-09-06, `class_id=0 num_slots=65536`
    /// -> a 1 MiB body), whose two header dwords are not a pointer and so pass
    /// the word0 test.
    ///
    /// Not screened on `class_id == 0` alone: id 0 is a legitimate class id in
    /// this tree (`is_zeroed` needs four fields to agree, and the allocator
    /// tests use it), so it is the SIZE that is impossible, not the id.
    ///
    /// The size comes from `object_total_size`, not from the legacy
    /// `HEADER_SIZE + num_slots * SLOT_SIZE` this doc used to state — see the
    /// comment in the body for the measured consequence of the hand-rolled
    /// formula.
    pub fn holder_body_cannot_fit_a_region(&self, header: &ObjectHeader) -> Option<usize> {
        let region_size = self.config.region_size;
        if region_size == 0 {
            return None;
        }
        // `object_total_size`, NOT `num_slots * SLOT_SIZE`. The hand-rolled
        // legacy formula this used to carry is wrong for the two shapes that
        // actually appear here, and `object_total_size`'s own comment says so:
        // it "OVER-sizes" a COMPACT instance (whose true body size lives in
        // `array_length`), and for an ARRAY the body is
        // `array_length * element_size`, nothing to do with `num_slots`.
        //
        // MEASURED CONSEQUENCE, 2026-09-06 (this is a bug this screen HAD, not
        // a hypothetical): every refusal in two H2 runs — 15 and 14 of them —
        // was `kind=Array`, `elem=Byte`/`Int`, lengths 82180..1048576. Ordinary
        // large primitive arrays, refused as impossible because their length
        // was multiplied by the 16-byte legacy slot stride. Phase 4 then
        // skipped their rset-edge collection.
        //
        // A size of 0 is `object_total_size`'s own "implausible array header"
        // signal and is left to the caller's existing guards rather than
        // turned into a refusal here.
        let size = object_total_size(header);
        if size > region_size {
            Some(size)
        } else {
            None
        }
    }

    /// gci i19/yc -- [`Self::holder_body_cannot_fit_a_region`] for the PARALLEL
    /// ref-scan holder at `obj_ptr`, with the serial arm's humongous exemption
    /// (`addr_is_in_humongous_region`) asked through a [`RegionView`]: a
    /// humongous object legitimately spans more than one region, so its size is
    /// no evidence. `Some(declared)` is a refusal.
    pub fn parallel_holder_body_refused(
        &self,
        regions: RegionView<'_>,
        obj_ptr: *const u8,
        header: &ObjectHeader,
    ) -> Option<usize> {
        let declared = self.holder_body_cannot_fit_a_region(header)?;
        let humongous = self
            .lookup_region_for_addr(obj_ptr as usize)
            .and_then(|i| regions.get(i))
            .is_some_and(|r| {
                matches!(
                    r.region_type(),
                    RegionType::HumongousStart | RegionType::HumongousContinuation
                )
            });
        if humongous {
            None
        } else {
            Some(declared)
        }
    }

    /// The holder's FIRST EIGHT BYTES, when they are a pointer into this
    /// collector's own arena rather than a `class_id`/mark-word pair.
    ///
    /// r11w18 g1store: since the 8-byte-header merge `ObjectHeader` is
    /// `class_id`(4) + `mark_word`(4) + `shape`(4) + `aux`(4), so the first
    /// word is `class_id` + MARK ([`header_first_word`]). This used to pair
    /// `class_id` with `num_slots()` -- the first word of the OLD layout --
    /// which refused every real array whose length equals the arena base's
    /// high dword (+1) (on the old layout as well: that word really is an
    /// arena address), and since the merge also missed the
    /// pointer-over-the-base shape it exists for. A real header's first word carries `GC_FLAG_HEADER` at
    /// bit 59 and is never an arena address, so the test now has no false
    /// positive on any published header.
    ///
    /// Shared deliberately. This screen was written for the parallel arm on
    /// 2026-09-06 and the serial arm did not get it -- the twin-pair divergence
    /// this file already has defects from. A holder refused by one evacuator and
    /// walked by the other is not a screen, it is a coin flip on which arm the
    /// pause happened to take.
    pub fn holder_word0_arena_pointer(&self, header: &ObjectHeader) -> Option<u64> {
        let paired = header_first_word(header);
        let (base, end) = (self.arena_base, self.arena_end);
        if end > base && (paired as usize) >= base && (paired as usize) < end {
            Some(paired)
        } else {
            None
        }
    }

    /// Report -- once per power of two -- a header that claims to be a legacy
    /// object of class 0 with an implausible field count, and say WHERE the
    /// bytes came from. See [`EVAC_IMPLAUSIBLE_CLASS0_COPY`].
    ///
    /// Measurement only: the caller's behaviour is unchanged. The point is the
    /// SOURCE-side context, which no report on this page has ever carried --
    /// every rejection so far named the holder after it had already been
    /// copied into a Survivor region, so the carve that produced it was two
    /// moves behind.
    pub fn note_implausible_legacy_header(
        &self,
        regions: &[G1Region],
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        site: &'static str,
    ) -> bool {
        self.note_implausible_legacy_header_view(RegionView::Slice(regions), obj_ptr, header, site)
    }

    /// [`Self::note_implausible_legacy_header`] against a [`RegionView`], so the
    /// parallel evacuator reaches the same body. See [`RegionView`].
    ///
    /// g1sz: the predicate is inlined into every caller and the report is a
    /// cold call. As one function it was entered once per collection-set
    /// reference the evacuator screens, and its prologue (six saved registers
    /// and a 352-byte frame for the report it almost never writes) took 7% of
    /// a `G1epKeepRounds 10` profile.
    #[inline(always)]
    pub fn note_implausible_legacy_header_view(
        &self,
        regions: RegionView<'_>,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        site: &'static str,
    ) -> bool {
        if !self.legacy_header_is_implausible(header) {
            return false;
        }
        self.report_implausible_legacy_header(regions, obj_ptr, header, site);
        true
    }

    /// The shapes [`Self::note_implausible_legacy_header_view`] reports.
    #[inline(always)]
    pub fn legacy_header_is_implausible(&self, header: &ObjectHeader) -> bool {
        // Two shapes, both of which `classify_candidate_header` accepts because
        // it only validates the TAG bytes and an upper bound on `shape`:
        //
        //  * a class id in the BAND no loader mints, and
        //  * class 0 with a field count no class has. `ClassId(0)` is what
        //    every primitive array carries (`newarray` passes it verbatim) and
        //    the MIC/PIC empty-slot sentinel; its objects have zero to a
        //    handful of fields. A reference array whose kind bit is unset reads
        //    exactly this way.
        //
        // Both authorise a SIXTEEN-byte-stride walk over something that is not
        // a legacy object.
        //
        // # The band, and why "large class id" alone was WRONG
        //
        // The first version of this screen refused every `class_id >= 1 << 24`,
        // on the reasoning that a process with 16 million loaded classes has
        // other problems. That reasoning is right about LOADED classes and
        // wrong about this VM's class-id space, which has two synthetic
        // regions above it:
        //
        //  * [`cratonvm_types::AUTOBOX_CLASS_ID`] is `u32::MAX`, and
        //  * lambda proxies are numbered by `SharedVm::alloc_lambda_proxy_id`,
        //    a bare counter from `0x8000_0000` (see `resolution.rs`, which
        //    allocates `ClassId::new(0x8000_0000)`).
        //
        // Both are ordinary live objects. Measured: a 500 s
        // `TestKillProcessWhileWriting` run reported 18 "implausible" headers
        // and ALL EIGHTEEN were autobox wrappers (`class_id=4294967295`,
        // `num_slots=1`) or lambda proxies (`class_id=2147483648..64`) -- a
        // screen that fires on unchanged, correct behaviour, which is worse
        // than no screen because it invites a conclusion.
        //
        // The band between them is what no id occupies.
        const MAX_LOADED_CLASS_ID: u32 = 1 << 24;
        const LAMBDA_PROXY_CLASS_ID_BASE: u32 = 0x8000_0000;
        const IMPLAUSIBLE_CLASS0_SLOTS: u32 = 1024;
        let cid = header.class_id.as_u32();
        // THE THIRD SHAPE, and the one the band above is blind to by
        // construction: the `class_id` and `shape` dwords, recombined, are a
        // pointer INTO THIS COLLECTOR'S OWN ARENA.
        //
        // That is what a heap pointer written over the pair looks like, and it
        // is the measured population. 2026-09-06, `TestHostConfigAutomatic-
        // DeploymentXmlExternalWarXml` under G1:
        //
        //   holder=0x2f056b807b0 class_id=2461533248 num_slots=752
        //                        mark=0x1000000000000000 gc_age=1
        //
        // `num_slots` 752 = 0x2F0 over `class_id` 2461533248 = 0x92B80440 is
        // the single word 0x000002F092B80440, and this run's arena is at
        // 0x2f0……. The mark word beside it is well-formed — a plausible
        // quartet and an age the copy incremented — so the header was sound
        // when it was written and something later put a pointer in the pair.
        //
        // **The band cannot see it.** `0x92B80440 >= 0x8000_0000`, so it lands
        // in the range this screen deliberately exempts for lambda proxies
        // (`SharedVm::alloc_lambda_proxy_id` counts up from `0x8000_0000`).
        // A 64-bit heap pointer's LOW dword has bit 31 set about half the
        // time, so the band screen was blind to half of this population — and
        // it is not a coincidence which half: the exemption exists precisely
        // over the values a pointer's low half occupies.
        //
        // The arena test does not have that hole and is far more specific than
        // the band: it needs BOTH dwords to conspire — `shape` to equal the
        // arena's high dword AND `class_id` to fall inside its low range —
        // where the band needs only one. It is also the collector's own
        // geometry rather than an assumption about the class-id space, so it
        // cannot go stale the way "large class ids are implausible" did.
        //
        // r11w18 g1store: "the pair" is the header's REAL first word,
        // `class_id` + mark word ([`header_first_word`]); the `class_id` +
        // `shape` recombination above is the pre-8-byte-header layout and
        // no longer names eight contiguous bytes.
        if header.kind() != ObjectKind::Object {
            return false;
        }
        (cid >= MAX_LOADED_CLASS_ID && cid < LAMBDA_PROXY_CLASS_ID_BASE)
            || (cid == 0 && header.num_slots() >= IMPLAUSIBLE_CLASS0_SLOTS)
            || self.header_pair_is_arena_pointer(header)
    }

    #[inline(always)]
    pub fn header_pair_is_arena_pointer(&self, header: &ObjectHeader) -> bool {
        let paired = header_first_word(header);
        self.arena_end > self.arena_base
            && (paired as usize) >= self.arena_base
            && (paired as usize) < self.arena_end
    }

    /// The report half of [`Self::note_implausible_legacy_header_view`], for
    /// a header [`Self::legacy_header_is_implausible`] refused.
    #[cold]
    #[inline(never)]
    pub fn report_implausible_legacy_header(
        &self,
        regions: RegionView<'_>,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        site: &'static str,
    ) {
        let cid = header.class_id.as_u32();
        let paired = header_first_word(header);
        let pair_is_arena_pointer = self.header_pair_is_arena_pointer(header);
        let n = EVAC_IMPLAUSIBLE_CLASS0_COPY.fetch_add(1, Ordering::Relaxed) + 1;
        if n > 8 && !n.is_power_of_two() {
            return;
        }
        let addr = obj_ptr as usize;
        let where_from = self
            .lookup_region_for_addr(addr)
            .and_then(|i| regions.get(i).map(|r| (i, r)))
            .map(|(i, r)| {
                let base = r.data.as_ptr() as usize;
                let off = addr.wrapping_sub(base);
                format!(
                    "r{i}/{:?}/off={off:#x}/cursor={:#x}/reuse_epoch={}/recycled_in_generation={} {} {} {}",
                    r.region_type(),
                    r.cursor(),
                    r.reuse_epoch,
                    r.recycled_in_generation(),
                    // The carve that produced this span, when the trails are
                    // recording. This is the whole point of the report.
                    r.tlab_trail
                        .lock()
                        .describe_owner_or(&r.bump_trail.lock(), r.reuse_epoch, off),
                    // Is this a REAL object boundary in the region's own grid,
                    // or one a desynced walk invented? `grid=OBJECT-START` with
                    // a sane `prev=` says an allocator put an object here and
                    // its header is wrong; `grid=INTERIOR` says the address
                    // came from somewhere that had no business naming it.
                    self.locate_in_object_grid(r, addr),
                    // ...and the bytes, so "shape written, mark word never
                    // written" is readable rather than inferred.
                    hexdump_around(r.data.as_ptr() as *mut u8, r.cursor(), off),
                )
            })
            .unwrap_or_else(|| "r?".to_string());
        tracing::warn!(
            "[g1] IMPLAUSIBLE legacy header at {site} (#{n}): obj={addr:#x} class_id={} kind=Object num_slots={} paired=0x{paired:016x} pair_is_arena_pointer={pair_is_arena_pointer} mark={:#018x} claims={:#x} bytes source={where_from} -- no allocation in this VM produces a class-0 legacy object with that many fields; a reference array whose kind bit is unset reads exactly this way, and the walk it authorises is eight times the array's extent. `pair_is_arena_pointer` says the class_id/shape dwords recombine to an address in THIS arena, i.e. a pointer was written over the pair -- see the note above the screen.",
            cid,
            header.num_slots(),
            header.mark_word.load(Ordering::Relaxed),
            HEADER_SIZE + header.num_slots() as usize * SLOT_SIZE,
        );
    }

    /// Reject a candidate reference the evacuator is about to DEREFERENCE
    /// when it does not look like a live object header, and say so once.
    ///
    /// `region_for_ptr` answers "is this word inside the region base table's
    /// span?" — containment, nothing more. `evacuate_object` then reads the
    /// candidate's `ObjectHeader`. A word that is in-span but is not an object
    /// (a region base, an uncommitted page, a stale address whose region has
    /// been recycled) therefore faults INSIDE THE COLLECTOR, with no
    /// attribution and no chance for the pause to continue.
    ///
    /// `is_object_address` is the validator this collector already trusts for
    /// conservative JIT roots and for the auto-box read: alignment, live-region
    /// containment, and both header tag bytes, none of which requires trusting
    /// the candidate. Using it here is the fail-safe the 2026-08-06 Hibernate
    /// G1 comparison asked for by name — it observed that the default
    /// collector's equivalent stale-coverage paths degrade to a controlled Java
    /// error while G1's take a native SIGSEGV.
    ///
    /// Returns `true` when the reference may be evacuated.
    ///
    /// This does NOT explain where a rejected word came from. It makes the
    /// event survivable and attributable; the producer is a separate question.
    ///
    /// Takes the caller's ALREADY-BORROWED `regions` slice and must not call
    /// [`Self::is_object_address`]: that helper's `is_addr_in_live_region` half
    /// re-acquires `self.regions`, which the evacuator is holding — a deadlock,
    /// not a slow path.
    pub fn evacuation_candidate_is_an_object(
        &self,
        regions: &[G1Region],
        site: &'static str,
        holder: *mut u8,
        slot: usize,
        raw: usize,
    ) -> bool {
        self.evacuation_candidate_is_an_object_view(
            RegionView::Slice(regions),
            site,
            holder,
            slot,
            raw,
        )
    }

    /// [`Self::evacuation_candidate_is_an_object`] against a [`RegionView`].
    ///
    /// This is the screen the PARALLEL evacuator was missing. `evacuate`'s own
    /// doc comment on the serial side records the contract — "every caller has
    /// already established that `old_ptr` is a plausible object header inside a
    /// live CSet region (roots go through `note_root_object_plausibility`,
    /// slots through `evacuation_candidate_is_an_object`)" — and
    /// `SharedEvac::process_object` / `seed_source_region` established neither,
    /// which is how a word that is merely INSIDE a CSet region's span reached
    /// `ObjectHeader::make_forwarded` and tripped its low-2-bits assert from a
    /// `g1-evac-N` worker. See `g1-evac-forwarding-assert-and-three-sigsegv-clusters`.
    pub fn evacuation_candidate_is_an_object_view(
        &self,
        regions: RegionView<'_>,
        site: &'static str,
        holder: *mut u8,
        slot: usize,
        raw: usize,
    ) -> bool {
        self.evacuation_candidate_screen_view(regions, site, holder, slot, raw)
            .0
    }

    /// [`Self::evacuation_candidate_is_an_object_view`], answering also
    /// whether the screen PROVED what `SharedEvac::evacuate`'s supply screen
    /// asks (`addr_is_followable_object_view` without the grid proof): the
    /// verdict was `Object` and the legacy-header screen found nothing
    /// implausible. With the implausible refusal off (the default) the first
    /// answer can be `true` while the second is `false`, and the supply screen
    /// must then run as before. g1sz: a candidate that passed both is not
    /// classified a second time.
    pub fn evacuation_candidate_screen_view(
        &self,
        regions: RegionView<'_>,
        site: &'static str,
        holder: *mut u8,
        slot: usize,
        raw: usize,
    ) -> (bool, bool) {
        let (verdict, _) = self.classify_candidate_header_view(regions, raw);
        if verdict == HeaderVerdict::Object {
            // Same second look as `note_root_object_plausibility`'s, on the
            // other supply route. A slot holding an INTERIOR address passes
            // `classify_candidate_header` and is then evacuated as an object,
            // which is how a 0x2010-byte "object" whose header is a heap
            // pointer gets carved into a Survivor region.
            // SAFETY: the verdict above validated the tag bytes and the
            // address's containment below its region's cursor.
            let cand = unsafe { &*(raw as *const ObjectHeader) };

            // THE ARENA TEST REFUSES BY DEFAULT; THE CLASS-ID BAND STILL DOES
            // NOT. They are two different questions and one flag used to answer
            // both.
            //
            // `g1-eight-byte-write-at-a-live-objects-base-20260906` left
            // `CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE` opt-in on the reasoning
            // that "a refusal that is WRONG drops a live reference, which
            // manufactures exactly the premature-free corruption this page is
            // about". That reasoning is sound and it was taken against the
            // CLASS-ID BAND test -- an assumption about which ids a loader
            // mints, which had already gone stale once (autobox and
            // lambda-proxy ids). It was NOT taken against the arena test, which
            // was added to `note_implausible_legacy_header` afterwards and
            // folded into the same flag.
            //
            // The arena test is a different kind of statement. `class_id`(4) +
            // `shape`(4) IS the header's first eight bytes, so a pair that
            // recombines to an address inside THIS collector's own arena is not
            // a header -- it is the first body word of something else, read at
            // an address that is not an object start. No loader, no id space
            // and no future class count can make it one. The same test already
            // refuses by default on the SERIAL evacuator's holders and on the
            // Phase-4 fixup's (`CRATONVM_G1_SERIAL_EVAC_HOLDER_SCREEN`); the
            // reference-slot CANDIDATE route was the one place it only warned.
            //
            // What that cost, measured 2026-09-08 on
            // `TestHostConfigAutomaticDeploymentXmlExternalWarXml` (Linux,
            // -Xmx1g, `CRATONVM_G1_WORKERS=16`): a candidate at
            // `holder + 0x38` -- the PAYLOAD word of a legacy cell, i.e. an
            // interior address -- was followed into `evacuate`, which installed
            // a forwarding mark word at `candidate + 8`. That address is
            // `holder + 0x40`, the DISCRIMINANT word of the next cell, so a
            // live 16-slot object came out of the pause with
            // `raw0 = 0x727ff9d00003` (a target tagged `MARK_FORWARDED`) and
            // `raw1 = 0` at slot 3. That is this page's corrupt-cell family,
            // bit for bit: "an EIGHT-byte quantity sitting where a SIXTEEN-byte
            // cell's first word belongs, with the second word untouched".
            //
            // `CRATONVM_G1_EVAC_CANDIDATE_ARENA_SCREEN=0` stands it down.
            {
                if let Some(paired) = self.holder_word0_arena_pointer(cand) {
                    let n = EVAC_REF_REJECTED_ARENA.fetch_add(1, Ordering::Relaxed) + 1;
                    if n <= 8 || n.is_power_of_two() {
                        tracing::warn!(
                            "[g1] {site}: REFUSED a candidate whose first header word is an \
                             ARENA POINTER (#{n}): holder=0x{:x} slot={slot} \
                             candidate=0x{raw:x} paired=0x{paired:016x} \
                             mark=0x{:016x} -- `class_id`+mark word IS the first eight bytes \
                             of a header, so a pair that recombines to an address in this \
                             collector's own arena is a BODY word read at an address that \
                             is not an object start. Following it would have installed a \
                             forwarding mark word eight bytes inside a live object. The \
                             slot is left unchanged and the pause continues.",
                            holder as usize,
                            cand.mark_word.load(Ordering::Relaxed),
                        );
                    }
                    return (false, false);
                }
            }
            let implausible = self.note_implausible_legacy_header_view(
                regions,
                raw as *mut u8,
                cand,
                "ref-slot-candidate",
            );
            // ACT ON THE SECOND LOOK, do not merely note it.
            //
            // The comment above has always said this is "the same second look
            // as `note_root_object_plausibility`'s", and until 2026-09-06 it
            // was not: the ROOT route is
            // `candidate_header_is_plausible && !note_implausible_legacy_header`
            // (`addr_is_followable_object`), and this route discarded the
            // second half and returned `true`. `addr_is_followable_object`'s
            // own doc says why that is not a style difference — "Both screens,
            // because either alone admits the other's defect… Measured
            // 2026-09-02: gating on the first alone is what let a conservative
            // root pointing 0x28 bytes inside a live reference array reach
            // `evacuate_object`, which sized an object from the array ELEMENT
            // and copied eight kilobytes of it into a Survivor region." The
            // 2026-09-02 lesson reached the root supply route and stopped
            // there; a REFERENCE SLOT holding an interior address satisfies
            // the tag screen exactly as trivially, because the first eight
            // bytes of a slot are a heap pointer whose halves read as a class
            // id and a `num_slots`.
            //
            // Not hypothetical, and it is what
            // `g1-parallel-evacuator-had-none-of-the-serial-arms-header-screens`
            // left behind: with that page's screens armed, a G1 run still
            // reports `IMPLAUSIBLE legacy header at ref-slot-candidate` and
            // still faults, because this site saw the header and followed it
            // anyway. The band it screens on is one no loader mints (see
            // `note_implausible_legacy_header`: `1<<24 ..< 0x8000_0000`, or
            // class 0 carrying >= 1024 slots), narrowed on measurement after
            // its first version rejected autobox and lambda-proxy ids — so a
            // refusal here cannot drop a live reference.
            //
            // `CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE=0` restores the
            // note-only behaviour, which is the same-binary A/B for the claim.
            // WHAT THIS DOES AND DOES NOT FIX -- measured 2026-09-06 on
            // `org.h2.test.store.TestMVStoreTool` (-Xmx256m, G1,
            // `CRATONVM_G1_JIT_MARK_DRIVER=1`), one binary, ABBA, refusing
            // against the note-only arm:
            //
            //   refusing    ref-slot 9 / 16 refused   evacuate-src 0   dest 0
            //   note-only   ref-slot 4 accepted       evacuate-src 4   dest 2
            //
            // So it closes the chain it is on: candidates refused here stop
            // reaching `evacuate_object` and the downstream implausible-header
            // reports go to zero.
            //
            // It does NOT stop the `corrupt Value cell` bursts, and expecting it
            // to was a mistake worth recording. Those are 32 in BOTH arms (32 is
            // the report cap, so read ">= 32") at 17-62 mixed pauses. They are a
            // SEPARATE defect that merely co-occurs, and its shape is different
            // in every respect that matters here: the holders carry PLAUSIBLE
            // class ids -- 665 with 192 slots, 13079432 with 1024 -- which is
            // precisely why this screen never fires on them. What is wrong is
            // their MARK WORD, which holds a heap-address-shaped value with
            // `gc_flags=0` (0x200001ea4fb17cd0, 0x100001ea4fb14cf0,
            // 0x300001ea4fb16850: age nibble intact, the rest an address in the
            // live heap). A zero `gc_flags` makes `is_compact_object` answer
            // false, so the walk strides 16-byte legacy cells over what is
            // really a compact object and every slot decodes as garbage.
            //
            // Those marks are NOT forwarding pointers -- `make_forwarded` ORs in
            // MARK_FORWARDED and these have the low two bits clear -- so
            // `retire_forwards` is excluded as well. What writes a
            // heap-address-shaped value into a live object's mark word and
            // clears its `gc_flags` is the open question.
            if implausible && gc_flags().g1_evac_ref_implausible_refuse {
                let n = EVAC_REF_REJECTED_IMPLAUSIBLE.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 8 || n.is_power_of_two() {
                    tracing::warn!(
                        "[g1] {site}: REFUSED a candidate whose legacy header is \
                         IMPLAUSIBLE (#{n}): holder=0x{:x} slot={slot} candidate=0x{raw:x} \
                         candidate_low3={} {} \
                         — the tag screen passed it, the class-id band screen did not. \
                         The slot is left unchanged and the pause continues; the report \
                         one line above names the shape. This is the refusal the ROOT \
                         route has made since 2026-09-02.",
                        holder as usize,
                        raw & 0b111,
                        ref_write_provenance(holder as usize + slot),
                    );
                }
                return (false, false);
            }
            // gci i16/nc -- a candidate every screen above passed whose header
            // carries no `GC_FLAG_HEADER`. Every allocator stamps the flag and
            // every mark-word transition keeps it (forwarded included), so on
            // a sound heap this is a body word read at an address that is not
            // an object start: the vi7-g1y np rows' `int[]` read as a legacy
            // header (class id ~5800, ~5800 slots, flags 0) is this shape.
            // Counted on the default path (`CleanupScrubCounters::evac_candidate_flagless`, printed
            // on the cleanup freed-target-scrub line); refused only under
            // `CRATONVM_G1_EVAC_CANDIDATE_REQUIRES_HEADER_FLAG=1` (default OFF:
            // a WRONG refusal leaves a live slot naming from-space, so the
            // refusal waits for its rows). Since gci i30/hz a candidate a
            // preceding header's extent COVERS is refused by default
            // (`CRATONVM_G1_EVAC_REFUSE_COVERED_FLAGLESS=0` follows it): it is
            // provably not an object start, so no real object loses its slot.
            if cand.gc_flags() & cratonvm_types::GC_FLAG_HEADER == 0 {
                let n = self
                    .scrub_counters
                    .evac_candidate_flagless
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                // gci i30/hz (gci-i29cv2) -- is the candidate INSIDE the object
                // before it? r19's flagless "objects" are `HashMap + 0x20`;
                // following such an address installs `FORWARDED` and a target
                // inside the map's body and copies a slice of it as a class-0
                // object. Refused by default (`g1_evac_refuse_covered_flagless`).
                let cover = self
                    .lookup_region_for_addr(raw)
                    .and_then(|i| regions.get(i))
                    .map_or(FlaglessCover::Undecided("no-region"), |r| {
                        // SAFETY: the verdict above placed `raw` (8-aligned)
                        // below this region's cursor; the scan stays inside
                        // `[base, raw)` of the same allocated prefix.
                        unsafe {
                            flagless_candidate_cover(r.data.as_ptr() as usize, r.cursor(), raw)
                        }
                    });
                let refuse_covered = cover.is_covered();
                let covered_total = if refuse_covered {
                    self.scrub_counters
                        .evac_candidate_flagless_covered_refused
                        .fetch_add(1, Ordering::Relaxed)
                        + 1
                } else {
                    self.scrub_counters
                        .evac_candidate_flagless_covered_refused
                        .load(Ordering::Relaxed)
                };
                let refuse = refuse_covered || g1_evac_candidate_requires_header_flag();
                if n <= 8 || n.is_power_of_two() {
                    // Where the holder and the candidate live, and whether the
                    // holder is marked: a DEAD holder in a surviving Old region
                    // naming a freed-and-reused region is the producer this
                    // page is narrowed to (`i30-hz-report.md`).
                    let region_desc = |a: usize| -> String {
                        self.lookup_region_for_addr(a)
                            .and_then(|i| regions.get(i).map(|r| (i, r)))
                            .map_or_else(
                                || "r?".to_string(),
                                |(i, r)| {
                                    format!(
                                        "r{i}/{:?}/off=0x{:x}/epoch={}/recycled_gen={}/marked={}",
                                        r.region_type(),
                                        a.wrapping_sub(r.data.as_ptr() as usize),
                                        r.reuse_epoch,
                                        r.recycled_in_generation(),
                                        r.mark_bitmap.is_marked(a),
                                    )
                                },
                            )
                    };
                    let holder_cid = if holder.is_null() {
                        u32::MAX
                    } else {
                        // SAFETY: `holder` is the object whose slot is being
                        // screened; its header word is mapped.
                        unsafe { &*(holder as *const ObjectHeader) }.class_id.as_u32()
                    };
                    tracing::warn!(
                        "[g1] {site}: candidate WITHOUT GC_FLAG_HEADER passed the header \
                         screens (#{n}): holder=0x{:x} slot={slot} candidate=0x{raw:x} \
                         class_id={} num_slots={} mark=0x{:08x} refused={refuse} -- a body \
                         word read as a header (gci i16/nc). {} refused_covered={refuse_covered} \
                         covered_refusals_total={covered_total} holder_cid={holder_cid} \
                         holder_at={} candidate_at={} {}",
                        holder as usize,
                        cand.class_id.as_u32(),
                        cand.num_slots(),
                        cand.mark_word.load(Ordering::Relaxed),
                        cover.describe(raw),
                        region_desc(holder as usize),
                        region_desc(raw),
                        ref_write_provenance(holder as usize + slot),
                    );
                }
                if refuse {
                    return (false, false);
                }
            }
            return (true, !implausible);        }
        // WHICH refusal, not just THAT one. See [`EVAC_REF_REJECTED_TORN`]:
        // "at/above the cursor" and "inside the allocated prefix but the tags
        // do not decode" are different defects with different producers, and
        // one counter over both is what made `#134217728` unactionable.
        let torn = evac_refusal_is_torn(verdict);
        let n = EVAC_REF_REJECTED.fetch_add(1, Ordering::Relaxed) + 1;
        let torn_n = if torn {
            EVAC_REF_REJECTED_TORN.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            EVAC_REF_REJECTED_TORN.load(Ordering::Relaxed)
        };
        // Rate-limited like the other GC fail-safes: the first is always
        // visible, then powers of two, so a pathological cycle cannot flood a
        // suite log while a single occurrence still cannot hide. A TORN
        // candidate gets its OWN budget: it is the rare population, and
        // sharing a throttle with the routine one is how a handful of real
        // events hide behind millions of ordinary ones.
        if n <= 8 || n.is_power_of_two() || (torn && (torn_n <= 8 || torn_n.is_power_of_two())) {
            // SAFETY: `holder` is the object currently being scanned; the
            // evacuator owns it under the `regions` lock.
            // The holder's SHAPE, not just its class. A rejection says a word
            // inside a live region is not an object header; whether the HOLDER
            // is one is the question that names the producer, and `class_id=0
            // kind=Object` (zeroed memory being walked as an object) reads
            // identically to a real class 0 without `num_slots`/`array_length`
            // beside it.
            let (holder_class, holder_kind, holder_slots, holder_len) = unsafe {
                let h = &*(holder as *const ObjectHeader);
                (
                    h.class_id.as_u32(),
                    h.kind(),
                    h.num_slots(),
                    h.array_length(),
                )
            };
            let holder_region = self
                .lookup_region_for_addr(holder as usize)
                .and_then(|i| regions.get(i).map(|r| (i, r)))
                .map(|(i, r)| {
                    format!(
                        "r{i}/{:?}/off={}/cursor={}",
                        r.region_type(),
                        (holder as usize).wrapping_sub(r.data.as_ptr() as usize),
                        r.cursor()
                    )
                })
                .unwrap_or_else(|| "r?".to_string());
            // WHERE THE HOLDER SITS IN ITS OWN REGION'S OBJECT GRID. The
            // 2026-08-30 census established that every rejection this site
            // produces carries the same holder shape (`class_id=0
            // kind=Object num_slots=8192 array_len=0`) and asked what such a
            // thing is. That question has exactly two answers and this string
            // separates them: `grid=OBJECT-START` means the bytes at a real
            // object start decode to that shape, so the HEADER is lying and
            // the producer is an allocator or a mark-word writer, while
            // `grid=INTERIOR` / `grid=DESYNC-BEFORE-TARGET` / `grid=WALK-BROKE`
            // means the holder address is not an object start at all and the
            // producer is whatever put it on a worklist.
            //
            // The raw mark word is printed beside it because the shape's
            // `kind` and `element_type` live in its top 16 bits: an all-zero
            // quartet under a non-zero `shape` is what a PRIMITIVE ARRAY
            // (which carries `class_id` 0 -- see `try_alloc_array`'s callers)
            // looks like when its kind tags were never written or were
            // overwritten, and that reads back as exactly the censused shape.
            let holder_grid = self
                .lookup_region_for_addr(holder as usize)
                .and_then(|i| regions.get(i))
                .map(|r| {
                    let off = (holder as usize).wrapping_sub(r.data.as_ptr() as usize);
                    format!(
                        "{} {}",
                        self.locate_in_object_grid(r, holder as usize),
                        // The RAW bytes. A `class_id` of 0x65676170 with
                        // `num_slots` 115 is the ASCII `page` + `s` -- an H2
                        // MVStore chunk header, i.e. byte-ARRAY PAYLOAD being
                        // read as an object header. Decoding that from two
                        // decimal fields is a trick a reader should not have
                        // to repeat.
                        hexdump_around(r.data.as_ptr() as *mut u8, r.cursor(), off),
                    )
                })
                .unwrap_or_else(|| "grid=no-region".to_string());
            // SAFETY: as above -- the evacuator owns the holder under the lock.
            let holder_mark = unsafe {
                (*(holder as *const ObjectHeader))
                    .mark_word
                    .load(Ordering::Relaxed)
            };
            tracing::warn!(
                "[g1] {site}: REJECTED a non-object candidate (#{n}, torn={torn} torn_total={torn_n} verdict={verdict:?}): holder=0x{:x} class_id={holder_class} kind={holder_kind:?} num_slots={holder_slots} array_len={holder_len} holder_mark=0x{holder_mark:016x} holder_region={holder_region} {holder_grid} slot={slot} candidate=0x{raw:x} — the word is inside the region span but is not a live object header, so evacuating it would have dereferenced it. The slot is left unchanged and the pause continues.",
                holder as usize,
            );
        }
        (false, false)
    }

    /// How many 8-byte reference slots of `obj_ptr` may safely be walked:
    /// `declared`, clamped to what remains inside the holder's own region.
    ///
    /// Reports (rate-limited) when the clamp actually bites, because that means
    /// a header claimed more elements than its region can hold — which is the
    /// corrupt-header case, not a large-object case: a genuinely large array is
    /// humongous and its continuation slices are physically contiguous, so the
    /// span below covers them.
    /// `stride` is the byte size of one walked element: `8` for a reference
    /// array's compact pointers, [`SLOT_SIZE`] for a legacy object's 16-byte
    /// `Value` slots. It was hard-coded to `8`, which quietly made this clamp
    /// unusable for the flat-object walk that needs it just as much —
    /// `num_slots` is no more bounded by a plausible header than `array_length`
    /// is, and its stride is twice as long, so it leaves the region twice as
    /// fast.
    pub fn holder_walkable_slots(
        &self,
        regions: &[G1Region],
        obj_ptr: *mut u8,
        declared: usize,
        stride: usize,
    ) -> usize {
        self.holder_walkable_slots_view(
            RegionView::Slice(regions),
            obj_ptr,
            declared,
            stride,
            HolderBound::Cursor,
        )
    }

    /// [`Self::holder_walkable_slots`] against a [`RegionView`], with the
    /// holder's end selected by `bound`.
    ///
    /// # Why the parallel arm cannot use `HolderBound::Cursor`
    ///
    /// The serial evacuator places every to-space copy with
    /// `alloc_in_type_locked`, which publishes the destination region's cursor
    /// as it bumps — so a to-space holder is always BELOW its region's cursor
    /// and the cursor bound is exact. The parallel evacuator carves per-worker
    /// TLABs and only writes the cursor back in `retire_tlab`, so for the whole
    /// dispatch a freshly-copied holder sits ABOVE the region's published
    /// cursor. Clamping it to the cursor would return 0 for every object the
    /// parallel arm copies — it would not harden the walk, it would stop the
    /// closure from following any reference at all.
    ///
    /// `HolderBound::RegionEnd` is the bound that is still true there: the
    /// holder's own region is committed (it holds the copy), and the point of
    /// the clamp is that a walk must not leave the region it started in.
    pub fn holder_walkable_slots_view(
        &self,
        regions: RegionView<'_>,
        obj_ptr: *mut u8,
        declared: usize,
        stride: usize,
        bound: HolderBound,
    ) -> usize {
        let addr = obj_ptr as usize;
        let region_size = self.config.region_size;
        if region_size == 0 || addr < self.arena_base || addr >= self.arena_end {
            return declared;
        }
        // The holder may be humongous: walk forward across continuation slices
        // so a legitimately large array is not clamped. Shift, not `div` — see
        // the note in `classify_candidate_header_view`; this one is called once
        // per HOLDER rather than once per reference, but it is the same
        // argument and the two should not drift.
        let mut idx = (addr - self.arena_base) >> self.region_shift;
        let Some(start) = regions.get(idx) else {
            return declared;
        };
        let base = start.data.as_ptr() as usize;
        let mut end = base
            + match bound {
                HolderBound::Cursor => start.cursor(),
                HolderBound::RegionEnd => start.data.len(),
            };
        while let Some(next) = regions.get(idx + 1) {
            if next.region_type() != RegionType::HumongousContinuation {
                break;
            }
            idx += 1;
            end = next.data.as_ptr() as usize + region_size;
        }
        if end <= addr + HEADER_SIZE {
            return 0;
        }
        let room = (end - addr - HEADER_SIZE) / stride.max(1);
        if room >= declared {
            return declared;
        }
        // gci i9/ng (i8/nt review item 1): a COMPACT holder is not a clamp.
        // `for_each_flat_object_reference_capped` ignores `max_slots` on the
        // compact arm (it is bounded by its layout's own body), so this cap,
        // computed with the LEGACY 16-byte stride over a field COUNT, never
        // restricts that walk. Counting it made the "expected ZERO" counter
        // read 30 000-77 000 on the netty rows and hid a real clamp (a dropped
        // live reference) in the noise. The returned value is unchanged.
        //
        // SAFETY: `addr` is inside a live region's span (checked above) and
        // 8-aligned by the caller's own screen; reading the header is the same
        // access the walk is about to make. An array is never compact; a
        // compact-flagged ARRAY header is a corrupt one, and stays counted.
        let holder = unsafe { &*(obj_ptr as *const ObjectHeader) };
        if holder.kind() != ObjectKind::Array && cratonvm_types::is_compact_object(holder) {
            return room;
        }
        let n = EVAC_HOLDER_CLAMPED.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            // IS THE CLAMPED HOLDER AN OBJECT AT ALL?
            //
            // This counter cannot be read without that field. A clamp on a
            // CORRUPT holder is the guard doing its job; a clamp on a SOUND one
            // is the guard dropping live references — the walk stops early, the
            // referents in the slots it skipped are never evacuated, and they
            // dangle. Those are opposite verdicts and the report used to print
            // the same line for both.
            //
            // `paired` is the `class_id`/`shape` dword pair recombined; the two
            // dwords ARE the header's first word, so a pair that lands inside
            // this arena is a pointer written over it and the holder is not an
            // object. `off` is the holder's offset in its own region: a
            // legitimately-allocated object cannot start so near the end that
            // its own declared body does not fit, so a large `off` beside a
            // small `room` is the corrupt case too.
            let (paired, off) = {
                // SAFETY: `addr` is inside a live region's span (checked above)
                // and 8-aligned by the caller's own screen; reading the first
                // eight bytes is the same access the walk is about to make.
                let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
                (
                    // r11w18 g1store: class id + MARK is the first word now.
                    header_first_word(h),
                    addr.wrapping_sub(base),
                )
            };
            let pair_is_arena_pointer = self.arena_end > self.arena_base
                && (paired as usize) >= self.arena_base
                && (paired as usize) < self.arena_end;
            // A compact holder never reaches this point (gc-common w6-f, and
            // gci i9/ng moved the screen above the counter): the capped walk
            // ignores `max_slots` on the compact arm, so a clamp computed with
            // the LEGACY 16-byte stride never restricted it.
            {
                tracing::warn!(
                    "[g1] evacuation ref-scan CLAMPED a holder's element walk (#{n}): obj=0x{addr:x} off=0x{off:x} stride={stride} declared={declared} room={room} paired=0x{paired:016x} pair_is_arena_pointer={pair_is_arena_pointer} bound={bound:?} — the header claims more reference slots than its region holds, so the walk would have read past the region. Walking {room}. `pair_is_arena_pointer=true` means the holder is not an object and this clamp is a refusal; `false` on a plausible `off` would mean the clamp is dropping live references and IS the defect. Compact holders are not reported: `for_each_flat_object_reference_capped` ignores `max_slots` on the compact arm, so a cap computed with the LEGACY 16-byte stride never restricts their walk.",
                );
            }
        }
        room
    }

    /// [`Self::is_object_address`]'s checks, against a borrowed `regions` slice
    /// instead of re-locking: alignment, arena bounds, the owning region being
    /// live and the address being below its allocation cursor, and both header
    /// tag bytes decoding to defined enum values. It does not take
    /// `is_object_address`'s two narrower screens (reserved mark bits, and the
    /// humongous object-START proof), so it accepts a superset: the safe
    /// direction here, for the reason below.
    ///
    /// Deliberately does NOT consult `kept_unresolved_*` the way
    /// `is_addr_in_live_region` does. Those sets are empty outside the rare
    /// wedged-drain window, they are behind their own mutexes (the same
    /// deadlock hazard), and erring towards ACCEPTING there is the safe
    /// direction for this guard: a false accept is only the pre-guard
    /// behaviour, whereas a false reject would drop a live reference.
    pub fn candidate_header_is_plausible(&self, regions: &[G1Region], addr: usize) -> bool {
        self.candidate_header_is_plausible_view(RegionView::Slice(regions), addr)
    }

    /// [`Self::candidate_header_is_plausible`] against a [`RegionView`].
    pub fn candidate_header_is_plausible_view(&self, regions: RegionView<'_>, addr: usize) -> bool {
        self.classify_candidate_header_view(regions, addr).0 == HeaderVerdict::Object
    }

    /// [`Self::candidate_header_is_plausible`], but returning WHICH check said
    /// no, plus the owning region index when there is one.
    ///
    /// The bool wrapper is what the hot guards call. This variant exists so the
    /// two rejection warnings can name the failing step instead of reporting an
    /// undifferentiated "not an object": "in-span but above the cursor" and
    /// "in-span, below the cursor, but the tag bytes are garbage" are different
    /// defects with different upstream producers, and a counter that cannot
    /// tell them apart cannot start an investigation.
    pub fn classify_candidate_header(
        &self,
        regions: &[G1Region],
        addr: usize,
    ) -> (HeaderVerdict, Option<usize>) {
        self.classify_candidate_header_view(RegionView::Slice(regions), addr)
    }

    /// [`Self::classify_candidate_header`] against a [`RegionView`].
    pub fn classify_candidate_header_view(
        &self,
        regions: RegionView<'_>,
        addr: usize,
    ) -> (HeaderVerdict, Option<usize>) {
        if addr == 0 || addr & 0x7 != 0 {
            return (HeaderVerdict::NullOrUnaligned, None);
        }
        if addr < self.arena_base || addr >= self.arena_end {
            return (HeaderVerdict::OutsideArena, None);
        }
        let region_size = self.config.region_size;
        if region_size == 0 {
            return (HeaderVerdict::NoRegionGeometry, None);
        }
        // F-09's shift, not a `div`. This screen was a cold path when it was
        // written — a few conservative roots per pause — and became a HOT one
        // on 2026-09-05, when the parallel evacuator (the DEFAULT arm) started
        // calling it once per CSet-bound reference. `region_size` is a runtime
        // value, so `/` compiles to a real 64-bit `div`: tens of cycles,
        // unpipelined. `lookup_region_for_addr` already carries the note and
        // the `normalize_region_size` argument that licenses the shift.
        let idx = (addr - self.arena_base) >> self.region_shift;
        let Some(r) = regions.get(idx) else {
            return (HeaderVerdict::NoSuchRegion, None);
        };
        let region_type = r.region_type();
        if region_type == RegionType::Free {
            return (HeaderVerdict::RegionFree, Some(idx));
        }
        let base = r.data.as_ptr() as usize;
        if addr < base {
            return (HeaderVerdict::BelowRegionBase, Some(idx));
        }
        // A humongous continuation slice is live in its entirety; only the
        // start region carries the object's full `cursor`.
        let readable_len = if region_type == RegionType::HumongousContinuation {
            region_size
        } else {
            r.cursor()
        };
        let Some(readable_end) = base.checked_add(readable_len) else {
            return (HeaderVerdict::AboveCursor, Some(idx));
        };
        let Some(header_end) = addr.checked_add(std::mem::size_of::<ObjectHeader>()) else {
            return (HeaderVerdict::AboveCursor, Some(idx));
        };
        // A conservative word may start inside the committed prefix while
        // its shape word (or another later header field) straddles the cursor.
        // The tag bytes alone are not enough to borrow ObjectHeader: screen_shape
        // reads the shape after them. Reject the whole candidate before either
        // tag probing or a typed header access.
        if addr >= readable_end || header_end > readable_end {
            return (HeaderVerdict::AboveCursor, Some(idx));
        }
        let ptr = addr as *const u8;
        // SAFETY: the full header is inside the readable region prefix checked
        // above (`ObjectHeader` has no invalid bit patterns: `ClassId(u32)`,
        // `AtomicU32`, `AtomicU64`). Load `mark_word` once with `Acquire` and
        // validate its tag bytes before any method reads the shape word.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        let mark = header.mark_word.load(Ordering::Acquire);
        let kind_tag = ObjectHeader::kind_tag(mark);
        let Some(kind) = object_kind_from_tag(kind_tag) else {
            return (HeaderVerdict::BadKindTag, Some(idx));
        };
        if !cratonvm_types::element_tag_ok(kind_tag, ObjectHeader::element_type_tag(mark)) {
            return (HeaderVerdict::BadElementTag, Some(idx));
        }
        if kind == ObjectKind::HumongousFiller {
            return (HeaderVerdict::HumongousFiller, Some(idx));
        }
        if matches!(regions, RegionView::Raw(..))
            && ObjectHeader::gc_flags_of(mark) & cratonvm_types::GC_FLAG_HEADER != 0
            && ObjectHeader::is_forwarded_mark(mark)
            && mark & cratonvm_types::MARK_FWD_BUSY != 0
        {
            wait_out_peer_claim(header);
        }
        const MAX_PLAUSIBLE_SLOTS: u32 = 1 << 24;
        let is_short = (ObjectHeader::gc_flags_of(mark) & cratonvm_types::GC_FLAG_COMPACT) != 0;
        let sane_shape = if kind == ObjectKind::Array || !is_short {
            header
                .screen_shape(|target| self.forward_target_has_readable_header(regions, target))
                .is_some_and(|shape| {
                    if kind == ObjectKind::Array {
                        shape <= i32::MAX as u32
                    } else {
                        shape <= MAX_PLAUSIBLE_SLOTS
                    }
                })
        } else {
            // A compact instance has no shape word. Its field count comes
            // from the registered compact-layout table and never follows a
            // forwarding pointer.
            //
            // g1sz: so it is not asked. That count is a class's field count
            // (< 65 536, JVMS 4.1) or 0, never above the bound, and it says
            // nothing about THIS header -- the same argument, and the same
            // skip, as the read barrier's screen (round 11 wave 15, lane
            // `rt`). The probe was one of six to eight per evacuated object.
            true
        };
        if !sane_shape {
            return (HeaderVerdict::ImplausibleShape, Some(idx));
        }
        (HeaderVerdict::Object, Some(idx))
    }

    /// Whether `target` names a complete, published object header, against the
    /// region table already borrowed by a candidate screen. A slice uses its
    /// committed cursor; a raw parallel view also admits complete copies above
    /// that cursor using the per-region publication watermark. This must not
    /// call `live_span_at`: the parallel view is used while its region storage
    /// is already borrowed, and a fresh lock here could deadlock.
    #[inline]
    pub fn forward_target_has_readable_header(&self, regions: RegionView<'_>, target: usize) -> bool {
        if target & 0x7 != 0 || target < self.arena_base || target >= self.arena_end {
            return false;
        }
        let region_size = self.config.region_size;
        if region_size == 0 {
            return false;
        }
        let idx = (target - self.arena_base) >> self.region_shift;
        let Some(region) = regions.get(idx) else {
            return false;
        };
        let base = region.data.as_ptr() as usize;
        let Some(end) = target.checked_add(std::mem::size_of::<ObjectHeader>()) else {
            return false;
        };
        let region_end = base.saturating_add(region_size);
        match region.region_type() {
            RegionType::Free | RegionType::HumongousContinuation => false,
            RegionType::HumongousStart => target == base && end <= region_end,
            _ => {
                // A parallel evacuation publishes its destination cursor only
                // when the worker retires its TLAB. A source header already
                // forwarded to a copy in that live TLAB therefore names a
                // complete header above `cursor()`. `parallel_copy_end` is
                // published after each complete copy and before its forwarding
                // mark, so it bounds the unretired copies without treating the
                // rest of a committed region as initialized. Keep the stronger
                // published-cursor bound for the serial slice path.
                let bound = match regions {
                    RegionView::Slice(_) => base.saturating_add(region.cursor()),
                    RegionView::Raw(..) => {
                        base.saturating_add(region.cursor().max(region.parallel_copy_end()))
                    }
                };
                target >= base && end <= region_end && end <= bound
            }
        }
    }

    /// One line of context for a rejected address: the verdict, and what the
    /// owning region looks like right now.
    pub fn describe_rejected_address(&self, regions: &[G1Region], addr: usize) -> String {
        self.describe_rejected_address_view(RegionView::Slice(regions), addr)
    }

    /// [`Self::describe_rejected_address`] against a [`RegionView`].
    pub fn describe_rejected_address_view(&self, regions: RegionView<'_>, addr: usize) -> String {
        let (verdict, idx) = self.classify_candidate_header_view(regions, addr);
        match idx.and_then(|i| regions.get(i).map(|r| (i, r))) {
            Some((i, r)) => {
                let base = r.data.as_ptr() as usize;
                format!(
                    "verdict={verdict:?} region={i} type={:?} base=0x{base:x} cursor=0x{:x} \
                     off=0x{:x} age={} pinned={} reuse_epoch={} recycled_in_generation={} {}",
                    r.region_type(),
                    r.cursor(),
                    addr.wrapping_sub(base),
                    r.age(),
                    r.pinned,
                    r.reuse_epoch,
                    r.recycled_in_generation(),
                    self.locate_in_object_grid(r, addr),
                )
            }
            None => format!("verdict={verdict:?} region=none"),
        }
    }

    /// Does `addr` name a live object the collector may FOLLOW -- evacuate,
    /// scan, forward?
    ///
    /// Both screens, because either alone admits the other's defect:
    ///
    /// * [`Self::candidate_header_is_plausible`] validates the two header tag
    ///   bytes and containment below the owning region's cursor. An INTERIOR
    ///   address satisfies both trivially: the first eight bytes of a reference
    ///   slot are a heap pointer whose low half reads as a class id and whose
    ///   high half reads as `num_slots`.
    /// * [`Self::note_implausible_legacy_header`] rejects exactly that -- a
    ///   class id in the band no loader mints, or class 0 carrying thousands of
    ///   slots.
    ///
    /// Measured 2026-09-02: gating on the first alone is what let a conservative
    /// root pointing 0x28 bytes inside a live reference array reach
    /// `evacuate_object`, which sized an object from the array ELEMENT and
    /// copied eight kilobytes of it into a Survivor region.
    pub fn addr_is_followable_object(
        &self,
        regions: &[G1Region],
        addr: usize,
        site: &'static str,
    ) -> bool {
        self.addr_is_followable_object_view(RegionView::Slice(regions), addr, site, GridProof::Yes)
    }

    /// [`Self::addr_is_followable_object`] against a [`RegionView`].
    pub fn addr_is_followable_object_view(
        &self,
        regions: RegionView<'_>,
        addr: usize,
        site: &'static str,
        prove: GridProof,
    ) -> bool {
        if !self.candidate_header_is_plausible_view(regions, addr) {
            return false;
        }
        // SAFETY: the screen above validated the tag bytes and placed `addr`
        // inside a live region's committed span.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        // THE EMPTY-HEADER SHAPE, which no header screen can reject; see
        // `empty_header_is_a_real_object`. Tested before the band screen
        // because it is the cheaper predicate to FAIL: three field reads, and
        // only a match pays for the grid walk.
        if prove == GridProof::Yes
            && header_has_empty_shape(header)
            && !self.empty_header_is_a_real_object(regions, addr)
        {
            let n = EVAC_EMPTY_HEADER_REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                tracing::warn!(
                    "[g1] {site}: REFUSED an EMPTY-HEADER candidate the object grid places \
                     INSIDE another object (#{n}): addr=0x{addr:x} mark={:#018x} {} -- \
                     `class_id=0 num_slots=0 kind=Object` is what the PAYLOAD WORD of a \
                     null `Value` cell reads as, and every header screen accepts it. \
                     Following it would have installed a forwarding mark word at addr+8, \
                     which is the next cell's discriminant word inside a live object.",
                    header.mark_word.load(Ordering::Relaxed),
                    self.describe_rejected_address_view(regions, addr),
                );
            }
            return false;
        }
        !self.note_implausible_legacy_header_view(regions, addr as *mut u8, header, site)
    }

    /// Decode a `MARK_FORWARDED` mark word into the address it names, refusing
    /// a word this collector could not have written.
    ///
    /// # `forwarding_target` strips two bits; `make_forwarded` asserts three
    ///
    /// `ObjectHeader::make_forwarded` asserts
    /// `plausible_heap_pointer(target)` -- non-null, **8-byte aligned**, under
    /// 2^47 -- so every forwarding target this or any other collector installs
    /// has its low THREE bits clear. `ObjectHeader::forwarding_target` masks
    /// off `MARK_STATE_MASK` (the low **two**) and the quartet, so **bit 2
    /// survives the decode**. A word that is not a forwarding pointer at all
    /// but whose low two bits happen to be `0b11` therefore passes
    /// `is_forwarded_mark` and decodes to an address with bit 2 set -- which is
    /// impossible for a real one.
    ///
    /// MEASURED 2026-09-08 on `TestHostConfigAutomaticDeploymentXmlExternalWarXml`
    /// (Linux, `-Xmx1g`, `CRATONVM_G1_WORKERS=16`), by a tripwire on every
    /// reference-slot write:
    ///
    /// ```text
    /// slot=0x7a24de7ce638 value=0x00006e6547246e6c low3=4 width=8
    /// slot=0x7a24deafd098 value=0x0000636a61636a2c low3=4 width=8
    /// slot=0x7a24deafd0c8 value=0x0000636a61636a2c low3=4 width=8
    /// slot=0x7a24deafd758 value=0x00000000f5ecdec4 low3=4 width=8
    /// ```
    ///
    /// **`low3 = 4` on every one**, and the values are ASCII (`0x636a61636a2c`
    /// is `"(jacj"`, `0x6e6547246e6c` is `"ln$Ge"`) -- Java string payload read
    /// as a mark word. Bit 2 set on 4 of 4 is the signature of this decode and
    /// of nothing else.
    ///
    /// # Why this is the producer the page was looking for
    ///
    /// `g1-eight-byte-write-at-a-live-objects-base-20260906` searched the READ
    /// side for six months of pages and concluded "the origin is upstream of
    /// every walk on this page and is still unfound". It is here, one frame
    /// above every walk:
    ///
    ///  1. a candidate reference passes the header screens (they validate the
    ///     mark word's TAG BYTES at bits 48..53, which say nothing about bits
    ///     0..2);
    ///  2. `evacuate`'s already-forwarded fast path reads its mark word, sees
    ///     `0b11`, and returns a decoded target that is not an address;
    ///  3. the caller **stores that value into the live reference slot it was
    ///     scanning** -- eight bytes for an array element or a compact field,
    ///     sixteen for a legacy `Value` cell -- and **pushes it onto the gray
    ///     worklist as a holder**;
    ///  4. the walk that next picks that holder up reads a "header" out of
    ///     whatever those bytes are: an arena pointer where `class_id`/`shape`
    ///     belong, a `num_slots` of 30825, a body extent of twelve gigabytes.
    ///     Those are this page's two measured families, in order.
    ///
    /// The copy watch was clean at every checkpoint because nothing rewrote a
    /// COPY; the six flat walks all read the damage because the damage is in
    /// the slot they read; and screening one walk moved the reports to the next
    /// because the producer is none of them.
    ///
    /// # What the refusal costs, stated honestly
    ///
    /// `None` means "this word is not a forward this collector installed", and
    /// the callers already handle it: the reference slot is left holding the
    /// address it had. When the verdict is RIGHT that is strictly better than
    /// storing a value no allocator ever returned — the slot still names the
    /// object it named.
    ///
    /// Lane B, 2026-09-20: the previous wording of this section said the
    /// refusal "cannot drop a live reference", full stop, and that is only true
    /// of a correct verdict. A WRONG refusal — a real forward this screen
    /// declines — leaves the slot pointing at a from-space object that Phase 5
    /// then frees, which is the use-after-free this file exists to prevent. The
    /// screen is therefore held to the same standard as any other: it must
    /// refuse only on evidence, and where it has no evidence it must accept.
    /// See the `bounds_known` arm in the body for the one place that mattered.
    pub fn decode_forwarding_target(&self, mark: u32, holder: usize) -> Option<*mut u8> {
        // The target lives in the holder's second word (the mark word only
        // says FORWARDED); `forwarding_address` waits out a parallel claim
        // that has not published yet, and answers a self-forward itself.
        // SAFETY: `holder` is the object whose mark word was read as `mark`.
        let target = unsafe { (*(holder as *const ObjectHeader)).forwarding_address() } as usize;
        // The install-time assertion, restated as a test. Alignment alone
        // catches the measured population (bit 2, 4 of 4); the arena bound
        // catches the half of a random word that happens to be 8-aligned.
        let aligned = cratonvm_types::plausible_heap_pointer(target as u64);
        // Lane B — the arena bound is only EVIDENCE when the arena bounds are
        // known, and this expression folded "the bounds are degenerate" into
        // the same `false` as "the target is outside them".
        //
        // The two are opposite verdicts. The doc below says a refusal "cannot
        // drop a live reference" because the slot keeps the address it has —
        // true for a word that was never a forward, and FALSE for a real
        // forward wrongly refused: there the referent has moved, the from-space
        // copy is about to be freed by Phase 5, and the slot is left naming
        // zero-filled memory. That is the use-after-free this whole file is
        // organised around, produced by a screen.
        //
        // So an unknowable bound fails OPEN, on alignment alone, and says so.
        // `G1Collector::new` always publishes `[arena_base, arena_end)`, so
        // this arm is unreachable today and the counter is how anyone would
        // learn that stopped being true.
        let bounds_known = self.arena_end > self.arena_base;
        let in_arena = target >= self.arena_base && target < self.arena_end;
        if aligned && (in_arena || !bounds_known) {
            if !bounds_known {
                let n = FORWARD_TARGET_UNBOUNDED_ARENA.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 4 || n.is_power_of_two() {
                    tracing::warn!(
                        "[g1] a FORWARDED mark word was accepted on ALIGNMENT ALONE (#{n}): \
                         holder=0x{holder:x} mark={mark:#018x} target=0x{target:x} -- this \
                         collector's arena bounds are degenerate \
                         (base=0x{:x} end=0x{:x}), so the containment half of the screen \
                         has nothing to say. Refusing here would leave the referring slot \
                         pointing at a from-space object Phase 5 is about to free.",
                        self.arena_base,
                        self.arena_end,
                    );
                }
            }
            return Some(target as *mut u8);
        }
        let n = FORWARD_TARGET_IMPLAUSIBLE.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::warn!(
                "[g1] a FORWARDED mark word decoded to a target `make_forwarded` could \
                 not have installed (#{n}): holder=0x{holder:x} mark={mark:#018x} \
                 target=0x{target:x} low3={} aligned={aligned} in_arena={in_arena} \
                 arena=[{:#x},{:#x}) -- `make_forwarded` asserts \
                 `plausible_heap_pointer`, so a real target has its low THREE bits \
                 clear, while `forwarding_target` strips only the low TWO. This word's \
                 low two bits are `0b11` by coincidence, not by installation. Refused: \
                 the slot keeps the address it has.",
                target & 0b111,
                self.arena_base,
                self.arena_end,
            );
        }
        None
    }

    /// A candidate whose header reads `class_id = 0`, `num_slots = 0`,
    /// `kind = Object` -- an EMPTY HEADER -- proved against the region's own
    /// object grid.
    ///
    /// # The one shape no header screen can reject
    ///
    /// A legacy `Value` cell is sixteen bytes: a discriminant word then a
    /// payload word. A NULL reference cell is therefore
    /// `[4, 0]` -- `Value::Object(None)` -- and the address of its PAYLOAD word
    /// has, as its own next sixteen bytes, `[0, <the next cell's
    /// discriminant>]`. Read as a header that is:
    ///
    ///   class_id = 0, num_slots = 0, kind = Object (tag 0), element_type =
    ///   Reference (tag 0), gc_flags = 0, gc_age = 0, state = NEUTRAL
    ///
    /// -- which is a *valid, sixteen-byte, zero-field object*. It satisfies
    /// [`Self::classify_candidate_header_view`] (both tag bytes decode, the
    /// address is 8-aligned and below its region's cursor), it satisfies
    /// [`Self::note_implausible_legacy_header_view`] (class 0 with a handful of
    /// slots is legitimate in this tree, and `0` is not a pointer into the
    /// arena), and it satisfies `object_total_size` (sixteen bytes, fits
    /// anywhere). **Every screen this file has passes it.**
    ///
    /// So it reaches `evacuate`, which installs a forwarding mark word at
    /// `addr + 8` -- the DISCRIMINANT word of the next cell of a live object --
    /// and the result is this page's corrupt-cell family exactly:
    ///
    /// ```text
    /// slot=0x7a4356f031f8 slot_index=10 raw0=0x00007a4354900013 raw1=0x0
    /// raw0_tag=3  raw0_untagged_arena=true  slot_in=HOLDER
    /// slot_minus_8(class_id=0 slots=0 kind=Object mark=0x00007a4354900013)
    /// ```
    ///
    /// `slot - 8` is the null cell's payload word, `slot` is the next cell's
    /// discriminant word, and what sits in it is `target | MARK_FORWARDED`
    /// with the sixteen-byte cell's second word untouched. That is "an EIGHT
    /// byte quantity sitting where a SIXTEEN-byte cell's first word belongs",
    /// measured 58 times by
    /// `g1-eight-byte-write-at-a-live-objects-base-20260906` and never
    /// attributed.
    ///
    /// # Why the grid, and why only here
    ///
    /// The region's object grid is the ONLY oracle that can separate the two:
    /// it walks from the region base by each object's own declared size, so it
    /// knows where objects actually begin. It is O(region) and cannot be run
    /// per candidate -- but it does not have to be. The empty-header shape is
    /// what a null cell's payload looks like and is essentially never a real
    /// allocation: `ClassId(0)` with `kind == Object` is the MIC/PIC empty-slot
    /// sentinel and the primitive-array id, not an instance of anything. The
    /// counters say how often the assumption holds.
    ///
    /// Returns `true` (follow it) when the grid confirms an object start, or
    /// when the grid cannot answer -- an unanswerable grid must not become a
    /// refusal, because a refusal that is WRONG drops a live reference, which
    /// is the corruption this page is about.
    pub fn empty_header_is_a_real_object(&self, regions: RegionView<'_>, addr: usize) -> bool {
        let Some(idx) = self.lookup_region_for_addr(addr) else {
            return true;
        };
        let Some(r) = regions.get(idx) else {
            return true;
        };
        let base = r.data.as_ptr() as usize;
        let cursor = r.cursor();
        if addr < base || addr >= base + cursor {
            return true;
        }
        // THE BUDGET. See `EMPTY_HEADER_PROOF_BUDGET_PER_PAUSE`: the proof is
        // O(region) and this is the evacuation path, so a workload that
        // produces the shape in bulk must not turn a correctness screen into a
        // pause-time regression. Above it, answer as the collector did before
        // this screen existed and count the waiver.
        if EMPTY_HEADER_PROOFS_THIS_PAUSE.fetch_add(1, Ordering::Relaxed)
            >= EMPTY_HEADER_PROOF_BUDGET_PER_PAUSE
        {
            EVAC_EMPTY_HEADER_WAIVED.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        EVAC_EMPTY_HEADER_PROVED.fetch_add(1, Ordering::Relaxed);
        let jit_skips = self.jit_tlab_skip_spans();
        let mut offset = 0usize;
        while base + offset < addr {
            let obj_ptr = (base + offset) as *mut u8;
            if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                offset += skip;
                continue;
            }
            // TLAB-retire GAP sentinel -- see `gap_filler_len`. A sub-header
            // TLAB tail is NOT a walkable object, and this walk's verdict is
            // consumed by `addr_is_followable_object_view`, which REFUSES the
            // candidate on `false`. Reading the sentinel as a header therefore
            // does not merely desync a diagnostic: it either lands the walk on
            // a wrong boundary (so a real object start reports `false` and a
            // live reference is dropped) or trips the desync test below and
            // wastes the proof. Every other walker in this file skips the span
            // by its recorded length; this one did not.
            if let Some(gap) = gap_filler_len(obj_ptr) {
                offset += gap;
                continue;
            }
            // SAFETY: below the region's published cursor, inside its own span.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let obj_size = object_total_size(header);
            if obj_size == 0 || obj_size > cursor.saturating_sub(offset) {
                // The grid desynced before reaching the address, so it cannot
                // answer. Not a refusal -- see the doc.
                return true;
            }
            offset += obj_size;
        }
        base + offset == addr
    }

    /// Is `addr` the START of an object, at a supply route that has no screen
    /// of its own?
    ///
    /// # The routes, and what an unscreened one costs
    ///
    /// `evacuate_object` sizes an object from the header at the address it is
    /// given and `copy_nonoverlapping`s that many bytes, then installs a
    /// forwarding mark word at `addr + 8`. Handed an INTERIOR address it does
    /// all three against bytes that are not a header:
    ///
    ///  * the copy reproduces a live object's BODY words as the destination's
    ///    header, so the destination's first eight bytes are whatever the
    ///    victim had at that offset -- typically a reference payload, i.e. **a
    ///    pointer into this arena sitting where `class_id`/`shape` belong**,
    ///    with a plausible-looking mark word beside it. That is the
    ///    arena-pointer holder family of
    ///    `g1-eight-byte-write-at-a-live-objects-base-20260906`, and it is why
    ///    those holders keep turning up as the FIRST object of a fresh
    ///    to-space region;
    ///  * the forwarding install writes eight bytes at `addr + 8`, which for an
    ///    interior `addr` is **inside a live object's body**, leaving the word
    ///    beside it untouched. That is the corrupt-cell family from the same
    ///    page: `raw0 = target | MARK_FORWARDED`, `raw1 = 0`, at a cell the
    ///    grid places inside a sound holder. MEASURED 2026-09-08, one such cell
    ///    at `slot_off_in_obj=0x60` of a live 16-slot object, `raw0` tagged 3
    ///    and its untagged value an arena address, with the reference-write
    ///    watch showing NO collector write on that slot -- because the writer
    ///    was a mark-word store, not a reference write.
    ///
    /// Three routes reach `evacuate_object` without asking: the marking
    /// KEEP-ALIVE set (`marking_keepalive_roots` filters on CSet residency
    /// only), the evacuation-failure DRAIN's seeds
    /// (`drain_kept_self_forwards`, whose seeds are the identity keys of a
    /// FAILED pause's forwarding map -- the least trustworthy addresses the
    /// collector holds), and Phase 3.5's finalizer RESURRECTION. The root loops
    /// have screened since 2026-09-02 and the two worker walks since
    /// 2026-09-05; these three were left, and `SharedEvac::evacuate`'s
    /// last-ditch guard checks only ALIGNMENT, which an interior address passes
    /// trivially.
    ///
    /// Refusing cannot drop a live reference: an address that is not an object
    /// start has no object to evacuate. What the caller loses is a keep-alive /
    /// seed / resurrection it could not have serviced correctly anyway -- and
    /// on every one of these routes the address is a DERIVED one (a gray-set
    /// entry, a forwarding-map key, a finalizer registration), never the only
    /// path to a live object.
    ///
    /// `CRATONVM_G1_EVAC_SUPPLY_SCREEN=0` stands it down, which is the
    /// same-binary A/B.
    pub fn evacuation_supply_is_an_object(
        &self,
        regions: &[G1Region],
        site: &'static str,
        addr: usize,
    ) -> bool {
        self.evacuation_supply_is_an_object_view(RegionView::Slice(regions), site, addr)
    }

    /// [`Self::evacuation_supply_is_an_object`] against a [`RegionView`], so
    /// the PARALLEL driver's keep-alive seed applies the identical screen.
    ///
    /// It did not, and that is the G1-9 shape again: the supply screen landed
    /// on `young_collection_serial` / `mixed_collection` (which call it per
    /// keep-alive address) and on the evacuation-failure drain and the
    /// finalizer resurrection, while `parallel_evacuate`'s Phase 1b tested only
    /// CSet RESIDENCY before handing the address to `SharedEvac::evacuate` --
    /// on the arm that is the default (`CRATONVM_G1_PARALLEL_EVAC`, on unless
    /// `0`). The keep-alive set is a gray-set / SATB snapshot, i.e. exactly the
    /// derived-address population [`Self::evacuation_supply_is_an_object`]'s
    /// own doc names as the least trustworthy the collector holds, and
    /// `SharedEvac::evacuate`'s last-ditch guard checks only ALIGNMENT, which
    /// an interior address passes trivially.
    pub fn evacuation_supply_is_an_object_view(
        &self,
        regions: RegionView<'_>,
        site: &'static str,
        addr: usize,
    ) -> bool {
        if self.addr_is_followable_object_view(regions, addr, site, GridProof::Yes) {
            return true;
        }
        let n = EVAC_SUPPLY_NON_OBJECT.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::warn!(
                "[g1] {site}: an evacuation SUPPLY address is not an object start (#{n}): \
                 addr=0x{addr:x} {} -- `evacuate_object` would have sized an object from \
                 bytes that are not a header, copied them, and installed a forwarding mark \
                 word at addr+8, which for an interior address is inside a LIVE object's \
                 body. Skipped; the pause continues.",
                self.describe_rejected_address_view(regions, addr),
            );
        }
        false
    }

    /// Is this CSet-resident root the start of a live object? Counts and, for
    /// the first few, describes the ones that are not.
    ///
    /// LOAD-BEARING since 2026-09-02; this doc said "measurement only — the
    /// caller's behaviour is unchanged" for as long as the callers evacuated
    /// the root either way. They now skip and pin on a `false`, so this
    /// function decides whether a root is followed.
    ///
    /// TWO predicates, and only both together are the question. The tag screen
    /// ([`Self::candidate_header_is_plausible`]) asks whether the two header
    /// tag bytes decode and the address sits below its region's cursor — which
    /// an INTERIOR address satisfies trivially, because the first eight bytes
    /// of a reference slot are a heap pointer whose halves read as a class id
    /// and a `num_slots`. The implausible-header screen is what rejects those.
    /// Wiring the refusal to the tag screen alone measured `skipped=0` on a run
    /// that was still fabricating objects out of array elements.
    pub fn note_root_object_plausibility(&self, regions: &[G1Region], addr: usize) -> bool {
        self.note_root_object_plausibility_view(RegionView::Slice(regions), addr)
    }

    /// [`Self::note_root_object_plausibility`] against a [`RegionView`], so the
    /// PARALLEL driver's Phase-1 root seed applies the identical screen. It did
    /// not before 2026-09-05: the 2026-09-02 "a root that is not an object
    /// start is not evacuated" hardening landed on `young_collection` /
    /// `mixed_collection` only, and parallel evacuation is the DEFAULT arm
    /// (`CRATONVM_G1_PARALLEL_EVAC`, on unless `0`).
    pub fn note_root_object_plausibility_view(&self, regions: RegionView<'_>, addr: usize) -> bool {
        if self.addr_is_followable_object_view(regions, addr, "cset-root", GridProof::Yes) {
            return true;
        }
        let n = NON_OBJECT_ROOT_SEEN.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::warn!(
                "[g1] CSet ROOT is not an object (#{n}): addr=0x{addr:x} {}",
                self.describe_rejected_address_view(regions, addr),
            );
        }
        false
    }

    /// Does this region's object grid CLOSE -- does the linear walk land
    /// exactly on the cursor, having stepped only whole objects?
    ///
    /// This is the check [`Self::locate_in_object_grid`]'s own doc says is
    /// missing. That function strides each object by the size ITS OWN header
    /// declares, so `grid=OBJECT-START` reports where the walk ARRIVED, not
    /// that an allocator put an object there: one wrong size upstream misparses
    /// every boundary after it and still lands on an "object start" each time.
    ///
    /// A walk that ends exactly at the cursor stepped a consistent set of
    /// sizes, so the boundaries it printed are the allocator's. A walk that
    /// OVERSHOOTS crossed at least one wrong size, and every verdict it gave
    /// for that region is an artefact of the misparse rather than evidence
    /// about the address asked about. Without this, the two cases are
    /// indistinguishable in the log -- which is how `grid=OBJECT-START` came to
    /// be read as proof of a real object start.
    pub fn grid_closes_on_cursor(&self, region: &G1Region) -> (bool, usize, usize) {
        let base = region.data.as_ptr() as usize;
        let cursor = region.cursor();
        let jit_skips = self.jit_tlab_skip_spans();
        let (mut offset, mut objects) = (0usize, 0usize);
        while offset < cursor {
            let obj_ptr = (base + offset) as *mut u8;
            if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                offset += skip;
                continue;
            }
            // TLAB-retire GAP sentinel -- see `gap_filler_len`. Without this a
            // region that merely holds a retired sub-header TLAB tail reports
            // "the grid does not close", which is the verdict that tells the
            // reader every OTHER answer about that region is an artefact. A
            // false one suppresses real findings, which is the failure mode a
            // closure test exists to prevent.
            if let Some(gap) = gap_filler_len(obj_ptr) {
                offset += gap;
                continue;
            }
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let obj_size = object_total_size(header);
            if obj_size == 0 || obj_size > cursor.saturating_sub(offset) {
                return (false, objects, offset);
            }
            offset += obj_size;
            objects += 1;
        }
        (offset == cursor, objects, offset)
    }

    /// gci i29/cv2 — every object start in a non-Free region whose header
    /// carries no `GC_FLAG_HEADER`, walked exactly as
    /// [`Self::grid_closes_on_cursor`] walks (skip spans, gap fillers, stop at
    /// a humongous filler or a size that leaves the cursor). An all-zero
    /// sixteen bytes is zeroed space (an unused TLAB tail), not a header, and
    /// is only counted. Pure over `regions`; the switch is the caller's.
    pub fn flagless_headers_in(
        &self,
        regions: &[G1Region],
    ) -> (FxHashSet<usize>, FlaglessSweepTally) {
        let jit_skips = self.jit_tlab_skip_spans();
        let mut found: FxHashSet<usize> = FxHashSet::default();
        let mut tally = FlaglessSweepTally::default();
        for r in regions.iter() {
            if matches!(r.region_type(), RegionType::Free | RegionType::HumongousContinuation) {
                continue;
            }
            tally.regions += 1;
            let base = r.data.as_ptr() as usize;
            let cursor = r.cursor();
            let mut offset = 0usize;
            while offset < cursor {
                let obj_ptr = (base + offset) as *mut u8;
                if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                    offset += skip;
                    continue;
                }
                if let Some(gap) = gap_filler_len(obj_ptr) {
                    offset += gap;
                    continue;
                }
                // SAFETY: `offset < cursor` inside this region's allocated
                // prefix; the world is stopped (every caller is a pause or a
                // test), so no allocator is writing below the cursor.
                let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                if is_humongous_filler(header) {
                    break;
                }
                let obj_size = object_total_size(header);
                if obj_size == 0 || obj_size > cursor - offset {
                    tally.desynced_regions += 1;
                    break;
                }
                tally.objects += 1;
                if header.gc_flags() & cratonvm_types::GC_FLAG_HEADER == 0 {
                    let w0 = header_first_word(header);
                    let w1 = if offset + 16 <= cursor {
                        // SAFETY: the second word lies below the cursor.
                        unsafe { std::ptr::read((obj_ptr as *const u64).add(1)) }
                    } else {
                        0
                    };
                    if w0 == 0 && w1 == 0 {
                        tally.zero_pairs += 1;
                    } else {
                        found.insert(obj_ptr as usize);
                    }
                }
                offset += obj_size;
            }
        }
        (found, tally)
    }

    /// gci i29/cv2 (gci-i26cv, the vi26-cv C1 r19 shape) —
    /// `CRATONVM_G1_DBG_FLAGLESS_SWEEP` (default OFF, `runtime_flag_on`, read
    /// per sweep): name the phase in which a live object's header LOST (or
    /// never had) `GC_FLAG_HEADER`.
    ///
    /// r19's 32 corrupt cells were not a walk desync: four walkers (rset
    /// source, worklist, outgoing-edge collection, Phase 4) met the same five
    /// holders at real
    /// object starts of grids that close -- by its bytes a 0x18-byte compact
    /// object (fields `1, 1, 12, 0.75f`) whose first header word reads
    /// `class_id=0`, `gc_flags=0` and only the age nibble (plus `OLD_GEN`
    /// once promoted), so every legacy walk takes its
    /// first field pair `(1, 1)` as `num_slots=1` and its next word
    /// `0x3f400000_0000000c` (an `int 12` and a `float 0.75f`) as a `Value`
    /// cell. The copier sizes it the same way (0x20) and drags the next
    /// object's header along as `raw1`. Beside it the ref-slot screen counted
    /// `candidate WITHOUT GC_FLAG_HEADER` holders with `class_id=0
    /// num_slots=0` that age by one per pause. "Every allocator stamps the
    /// flag and nothing clears it" -- so something zeroed eight bytes at a live
    /// object's base, or allocated one without it, and nothing on the default
    /// path says WHEN.
    ///
    /// This sweep does, by elimination over the points it runs at:
    /// `pause-start` and `remark-start` (the mutator ran since the last
    /// sweep), `pause-end` (this evacuation pause ran; a to-space address
    /// whose from-space source was already flagless at `pause-start` is
    /// reported as CARRIED, not new) and `cleanup-end` (remark/cleanup ran).
    /// The first sweep that lists an
    /// address NEW names the producer's phase; with `CRATONVM_G1_DBG_REACH=1`
    /// the line also names the TLAB carve (thread) that owns the span.
    pub fn flagless_header_sweep(
        &self,
        regions: &[G1Region],
        phase: &'static str,
        forwards: Option<&cratonvm_types::PointerMap>,
    ) {
        if !g1_dbg_flagless_sweep() {
            return;
        }
        let (current, tally) = self.flagless_headers_in(regions);
        let mut prev = self.flagless_sweep_prev.lock();
        let (new, carried) = flagless_new_and_carried(&prev, &current, forwards);
        let gc = self.collection_count.load(Ordering::Relaxed);
        eprintln!(
            "[g1][FLAGLESS] {phase} gc={gc} sweep: regions={} objects={} flagless={} new={} \
             carried={} zero_pairs={} desynced_regions={}",
            tally.regions,
            tally.objects,
            current.len(),
            new.len(),
            carried.len(),
            tally.zero_pairs,
            tally.desynced_regions,
        );
        // Which phase a NEW address convicts: `pause-start` / `remark-start`
        // follow a sweep after which only the mutator ran; `pause-end` and
        // `cleanup-end` follow one taken at the start of the same pause.
        let verdict = match phase {
            "pause-start" | "remark-start" => "made BETWEEN pauses (mutator)",
            "pause-end" => "made BY THIS PAUSE (no flagless source at pause-start)",
            "cleanup-end" => "made BY remark/cleanup (none at remark-start)",
            _ => "phase unknown",
        };
        for (k, &addr) in new.iter().enumerate().take(8) {
            let Some(i) = self.lookup_region_for_addr(addr) else {
                continue;
            };
            let Some(r) = regions.get(i) else {
                continue;
            };
            let base = r.data.as_ptr() as usize;
            let off = addr - base;
            let cursor = r.cursor();
            // SAFETY: `addr` is an object start the walk above found below
            // this region's cursor; the four words read are clamped to it.
            let word = |o: usize| -> String {
                if off + o + 8 <= cursor {
                    format!("{:#018x}", unsafe { std::ptr::read((addr + o) as *const u64) })
                } else {
                    "past-cursor".to_string()
                }
            };
            let header = unsafe { &*(addr as *const ObjectHeader) };
            eprintln!(
                "[g1][FLAGLESS] {phase} gc={gc} NEW #{} obj={addr:#x} \
                 r{i}/{:?}/off={off:#x}/cursor={cursor:#x}/reuse_epoch={} \
                 class_id={} num_slots={} mark={:#010x} words=[{} {} {} {}] {} \
                 grid_closes={} {} -- {verdict}",
                k + 1,
                r.region_type(),
                r.reuse_epoch,
                header.class_id.as_u32(),
                header.num_slots(),
                header.mark(),
                word(0),
                word(8),
                word(16),
                word(24),
                // `grid=OBJECT-START ... prev=(..)`: the object whose size put
                // this boundary here -- in r19 always a 0x28 `cid=185` one.
                self.locate_in_object_grid(r, addr),
                self.grid_closes_on_cursor(r).0,
                r.tlab_trail
                    .lock()
                    .describe_owner_or(&r.bump_trail.lock(), r.reuse_epoch, off),
            );
        }
        for &(to, from) in carried.iter().take(4) {
            eprintln!("[g1][FLAGLESS] {phase} gc={gc} CARRIED obj={to:#x} from={from:#x}");
        }
        *prev = current;
    }

    /// Where `addr` falls in its region's OWN object grid, walked linearly from
    /// the region base exactly as `scan_source_region_for_cset_refs` walks it.
    ///
    /// This is the question that separates the candidate explanations for a
    /// rejected address. "It is 0x18 bytes inside a live `class_id=42` object"
    /// means someone produced an INTERIOR pointer; "it is inside a TLAB skip
    /// span" means the address is in memory no object grid covers; "the walk
    /// desynced before reaching it" means the region's own grid is broken and
    /// the address is a symptom rather than the cause.
    pub fn locate_in_object_grid(&self, region: &G1Region, addr: usize) -> String {
        let base = region.data.as_ptr() as usize;
        if addr < base {
            return "grid=below-base".to_string();
        }
        let target = addr - base;
        let jit_skips = self.jit_tlab_skip_spans();
        let mut offset = 0usize;
        let mut objects = 0usize;
        // The object the walk stepped over to arrive here, and the step it
        // took. `grid=OBJECT-START` alone cannot distinguish "the grid is
        // sound and this really is an object" from "the grid is walking at
        // wrong boundaries and lands here by construction" -- and the second
        // is what a header full of ASCII (`class_id=0x65676170`, the bytes
        // `page`, from an H2 MVStore chunk header) means. The PREDECESSOR is
        // the object whose size decided this boundary, so it is the one to
        // name.
        let mut prev = String::from("prev=none");
        while offset < region.cursor() {
            let obj_ptr = (base + offset) as *mut u8;
            if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                if target < offset + skip {
                    return format!("grid=IN-JIT-TLAB-SKIP span_start=0x{offset:x} len=0x{skip:x}");
                }
                offset += skip;
                continue;
            }
            if let Some(gap) = gap_filler_len(obj_ptr) {
                if target < offset + gap {
                    return format!("grid=IN-TLAB-GAP-FILLER gap_start=0x{offset:x} len=0x{gap:x}");
                }
                offset += gap;
                continue;
            }
            // The tag half of the real walk's validation (the region-bounds half
            // is implied by the loop). A desync here is itself the answer, so
            // report where it happened rather than guessing past it.
            let ptr = obj_ptr as *const u8;
            let kind_ok =
                unsafe { object_kind_from_tag(cratonvm_types::kind_tag_at(ptr)) }.is_some();
            let elem_ok = unsafe { cratonvm_types::element_type_tag_ok_at(ptr) };
            if !kind_ok || !elem_ok {
                return format!(
                    "grid=DESYNC-BEFORE-TARGET at=0x{offset:x} after={objects} objects \
                     (target=0x{target:x})"
                );
            }
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if is_humongous_filler(header) {
                return format!("grid=HUMONGOUS-FILLER at=0x{offset:x}");
            }
            let obj_size = object_total_size(header);
            if obj_size < HEADER_SIZE || offset + obj_size > region.cursor() {
                return format!(
                    "grid=WALK-BROKE at=0x{offset:x} obj_size=0x{obj_size:x} after={objects} \
                     objects (target=0x{target:x})"
                );
            }
            if target == offset {
                return format!("grid=OBJECT-START idx={objects} size=0x{obj_size:x} {prev}");
            }
            if target < offset + obj_size {
                return format!(
                    "grid=INTERIOR of=0x{offset:x} delta=0x{:x} size=0x{obj_size:x} cid={} \
                     kind={:?} idx={objects}",
                    target - offset,
                    header.class_id.as_u32(),
                    header.kind(),
                );
            }
            prev = format!(
                "prev=(off=0x{offset:x} cid={} kind={:?} elem={:?} alen={} slots={} size=0x{obj_size:x} mark={:#018x})",
                header.class_id.as_u32(),
                header.kind(),
                header.element_type(),
                header.array_length(),
                header.num_slots(),
                header.mark_word.load(Ordering::Relaxed),
            );
            offset += obj_size;
            objects += 1;
        }
        format!("grid=PAST-CURSOR walked={objects} objects to 0x{offset:x} {prev}")
    }

    /// Which object does `slot`'s region place it in, walked from the region
    /// base by each object's own declared size, and is that `holder`?
    ///
    /// This is the per-address companion to [`Self::grid_closes_on_cursor`].
    /// The closure test is a property of the whole region -- it says the sizes
    /// the walk strode summed to the cursor -- and a holder that over-declares
    /// while a later object under-declares by the same amount still closes. The
    /// question a corrupt-cell report needs answered is narrower and local: the
    /// walk read sixteen bytes at `slot` while claiming to be inside `holder`,
    /// so is `slot` inside `holder`'s allocated extent or inside the object
    /// after it?
    ///
    /// Returns a rendered verdict rather than a bool because the three outcomes
    /// need different follow-ups and a bool cannot carry which one happened.
    pub fn grid_object_containing(&self, regions: &[G1Region], slot: usize, holder: usize) -> String {
        let Some(r) = self
            .lookup_region_for_addr(slot)
            .and_then(|i| regions.get(i))
        else {
            return "slot_in=NO-REGION".to_string();
        };
        let base = r.data.as_ptr() as usize;
        let cursor = r.cursor();
        if slot < base || slot >= base + cursor {
            return format!("slot_in=ABOVE-CURSOR cursor={cursor:#x}");
        }
        let jit_skips = self.jit_tlab_skip_spans();
        let mut offset = 0usize;
        while offset < cursor {
            let obj_ptr = (base + offset) as *mut u8;
            if let Some(skip) = jit_tlab_skip_span_len(&jit_skips, obj_ptr as usize) {
                if slot < base + offset + skip {
                    return format!("slot_in=JIT-TLAB-SKIP span={offset:#x}+{skip:#x}");
                }
                offset += skip;
                continue;
            }
            // TLAB-retire GAP sentinel -- see `gap_filler_len`. This walk
            // decides HOLDER vs NEIGHBOUR for a corrupt-cell report, and the
            // two have different fixes; reading a gap sentinel as an object
            // shifts every boundary after it, so the verdict names the wrong
            // object or degenerates to `GRID-DESYNC`.
            if let Some(gap) = gap_filler_len(obj_ptr) {
                if slot < base + offset + gap {
                    return format!("slot_in=TLAB-GAP-FILLER gap={offset:#x}+{gap:#x}");
                }
                offset += gap;
                continue;
            }
            // SAFETY: `base + offset` is inside the region's own committed span,
            // below the published cursor.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let obj_size = object_total_size(header);
            if obj_size == 0 || obj_size > cursor.saturating_sub(offset) {
                return format!("slot_in=GRID-DESYNC at={offset:#x} size={obj_size:#x}");
            }
            if slot < base + offset + obj_size {
                let start = base + offset;
                let verdict = if start == holder {
                    "HOLDER"
                } else {
                    "NEIGHBOUR"
                };
                return format!(
                    "slot_in={verdict} obj={start:#x} obj_size={obj_size:#x} \
                     obj_class_id={} obj_slots={} obj_kind={:?} obj_is_compact={} \
                     slot_off_in_obj={:#x} slot_is_that_objs_mark_word={}",
                    header.class_id.as_u32(),
                    header.num_slots(),
                    header.kind(),
                    cratonvm_types::is_compact_object(header),
                    slot - start,
                    slot == start + cratonvm_types::MARK_WORD_OFFSET,
                );
            }
            offset += obj_size;
        }
        "slot_in=PAST-CURSOR".to_string()
    }

    /// Drain [`PENDING_CORRUPT_HOLDERS`] and answer the one question the
    /// walk-site report cannot: is the holder at a REAL object start?
    ///
    /// Called once per pause with the region table still held, so
    /// `locate_in_object_grid` and `hexdump_around` are available. The two
    /// verdicts have different fixes and nothing else separates them:
    ///
    ///  * `grid=OBJECT-START` -- an allocator really put an object there, so
    ///    its header was overwritten after the fact and there is a WRITER to
    ///    find. The `bytes[...]` dump then shows what landed on it. Measured
    ///    2026-09-06 over 118 distinct H2 holders: 53 have BOTH header words
    ///    holding arena pointers, 25 word0 only, 7 the mark only, 11 a
    ///    SELF-forward -- so this is not the single eight-byte write at the
    ///    base that the Tomcat page infers from a clean mark word.
    ///  * `grid=INTERIOR` (or any desync verdict) -- the address was never an
    ///    object start, the words read as a header are a neighbour's slots,
    ///    and no header writer exists. The bug is then whatever put that
    ///    address on a walk, which is the family
    ///    `g1-parallel-evacuator-had-none-of-the-serial-arms-header-screens`
    ///    already names.
    pub fn report_pending_corrupt_holders(&self, regions: &[G1Region]) {
        let drained: Vec<PendingCorruptHolder> =
            std::mem::take(&mut *PENDING_CORRUPT_HOLDERS.lock());
        for rec in drained {
            let PendingCorruptHolder {
                holder: addr,
                class_id: cid,
                num_slots: slots,
                mark,
                slot,
                slot_index,
                raw0,
                raw1,
                caller,
                reason,
            } = rec;
            // RE-TAKE OF THIS FAMILY'S `word0_plausible_ptr` STATISTIC.
            // The page cites that field TRUE on 19 of 19 holders as its evidence
            // that the header's first word is a pointer. It reports TRUE for
            // `(18<<32)|64` -- an ordinary class_id/num_slots pair -- so the
            // statistic cannot be read as taken. This computes the SAME question
            // the refusal screens ask, from the same arena bounds, and prints it
            // beside the holder so the two can be compared per address instead of
            // per run.
            // r11w18 g1store: `class_id` + MARK word is the header's first
            // word since the 8-byte header (`header_first_word`); the old
            // `class_id` + shape pairing reported `word0_arena_test=true` for
            // an ordinary TLAB filler `int[442]` in an arena at `0x1ba_....`.
            let paired = ((mark as u32 as u64) << 32) | cid as u64;
            let arena_says = self.arena_end > self.arena_base
                && (paired as usize) >= self.arena_base
                && (paired as usize) < self.arena_end;
            // THE SPLIT. See `CopyWatch::lookup`: in-ledger means the source was
            // already corrupt (the checkpoints prove the copy did not change it),
            // not-in-ledger means nothing copied this object this pause.
            let provenance = COPY_WATCH
                .get()
                .and_then(|w| w.lookup(addr))
                .map(|(src, was_cid, was_shape)| {
                    format!(
                        "copied_this_pause=YES src={src:#x} was(class_id={was_cid} \
                         shape={was_shape}) source_was_already_corrupt={}",
                        was_cid == cid && was_shape == slots,
                    )
                })
                .unwrap_or_else(|| "copied_this_pause=no".to_string());
            let where_from = self
                .lookup_region_for_addr(addr)
                .and_then(|i| regions.get(i).map(|r| (i, r)))
                .map(|(i, r)| {
                    let base = r.data.as_ptr() as usize;
                    let off = addr.wrapping_sub(base);
                    let (closes, walked, ended) = self.grid_closes_on_cursor(r);
                    format!(
                        "r{i}/{:?}/off={off:#x}/cursor={:#x}/reuse_epoch={} grid_closes_on_cursor={closes} grid_walked={walked} grid_ended={ended:#x} {} {}",
                        r.region_type(),
                        r.cursor(),
                        r.reuse_epoch,
                        self.locate_in_object_grid(r, addr),
                        hexdump_around(r.data.as_ptr() as *mut u8, r.cursor(), off),
                    )
                })
                .unwrap_or_else(|| "r?".to_string());
            // WHICH OBJECT DOES THE GRID PUT THE CORRUPT SLOT IN?
            //
            // `grid_closes_on_cursor` says the region's boundaries are the
            // allocator's; it does not say the HOLDER's declared body is the
            // bytes the allocator gave IT. This does, for the one address that
            // matters. The two answers have opposite fixes and nothing else on
            // this page separates them:
            //
            //  * `slot_in=HOLDER` -- the walk stayed inside the object it was
            //    walking, so a foreign write really did land in a live object's
            //    body and there is a WRITER upstream of every walk.
            //  * `slot_in=NEIGHBOUR` -- the holder over-declared, the walk
            //    strode past its end, and the "corrupt cell" is the next
            //    object's header and mark word read as a `Value`. The defect is
            //    then the holder's own count, not a writer, and every
            //    corrupt-cell verdict on that population is a misparse.
            let slot_in = self.grid_object_containing(regions, slot, addr);
            // WHAT DOES `raw0` NAME, AND WHAT IS EIGHT BYTES BEFORE THE SLOT?
            //
            // The measured corrupt cell is `raw0 = <arena address> | 3`,
            // `raw1 = 0`. Three producers fit that and they are told apart by
            // these two fields alone:
            //
            //  * `target_mark == raw0` -- the value IS the mark word of the
            //    object it names, so something COPIED a mark word into this
            //    cell rather than storing a reference. The search is then for a
            //    16-byte copy whose source was an object header.
            //  * `slot_minus_8_is_object_start` -- an object begins at
            //    `slot - 8`, so `slot` IS that object's mark word and the cell
            //    the walk read straddles an object boundary. The producer is
            //    then whatever made the walk believe `slot` was a cell.
            //  * neither -- an eight-byte store of a forwarding mark word at an
            //    address that is not a mark word, i.e. `make_forwarded`
            //    installed against an INTERIOR address (`slot - 8` being the
            //    interior one).
            let raw0_target = (raw0 & !0b11u64) as usize;
            let target_desc = if self.arena_end > self.arena_base
                && raw0_target >= self.arena_base
                && raw0_target < self.arena_end
            {
                // SAFETY: inside the arena, which is mapped for the collector's
                // lifetime; sixteen bytes at an 8-aligned address in it.
                let h = unsafe { &*(raw0_target as *const ObjectHeader) };
                let m = h.mark_word.load(Ordering::Relaxed);
                format!(
                    "target=0x{raw0_target:x} target_class_id={} target_slots={} \
                     target_mark={m:#018x} target_mark_is_raw0={} \
                     target_is_region_base={}",
                    h.class_id.as_u32(),
                    h.num_slots(),
                    m as u64 == raw0,
                    self.lookup_region_for_addr(raw0_target)
                        .and_then(|i| regions.get(i))
                        .is_some_and(|r| r.data.as_ptr() as usize == raw0_target),
                )
            } else {
                "target=not-in-arena".to_string()            };
            let before_desc = if slot >= self.arena_base + 8 && slot < self.arena_end {
                // SAFETY: as above, eight bytes below an in-arena address.
                let h = unsafe { &*((slot - 8) as *const ObjectHeader) };
                format!(
                    "slot_minus_8(class_id={} slots={} kind={:?} mark={:#018x})",
                    h.class_id.as_u32(),
                    h.num_slots(),
                    h.kind(),
                    h.mark_word.load(Ordering::Relaxed),
                )
            } else {
                "slot_minus_8=?".to_string()            };
            tracing::warn!(
                "[g1] CORRUPT-CELL HOLDER GRID VERDICT: holder={addr:#x} class_id={cid} \
                 num_slots={slots} mark={mark:#018x} gc_flags={:#x} paired={paired:#018x} \
                 word0_arena_test={arena_says} arena=[{:#x},{:#x}) reason={reason} \
                 caller={caller} \
                 slot={slot:#x} slot_index={slot_index} raw0={raw0:#018x} raw1={raw1:#018x} \
                 raw0_tag={} raw0_untagged_arena={} {target_desc} {before_desc} \
                 {slot_in} {} \
                 source={where_from} {provenance}",
                (mark >> 56) & 0xF,
                self.arena_base,
                self.arena_end,
                raw0 & 0b11,
                self.arena_end > self.arena_base
                    && ((raw0 & !0b11u64) as usize) >= self.arena_base
                    && ((raw0 & !0b11u64) as usize) < self.arena_end,
                ref_write_provenance(slot),
            );
        }
    }

    /// Scan an evacuated object's reference fields. For each reference pointing
    /// into the CSet, evacuate the target and update the field.
    ///
    /// **STW-only correctness contract** (Round-7 audit §2): the
    /// `pointer_map.contains_key(...)` dedup pre-check below is a
    /// plain-`HashMap` operation that is correct ONLY because young/mixed
    /// evacuation runs single-threaded under STW with the calling thread
    /// holding `self.regions.read()` for the entire collection.
    ///
    /// The TOCTOU this paragraph used to warn about — two workers sampling
    /// `contains_key == false` for the same address, both copying, one leaking
    /// its copy and both scanning — is real, and it is why the PARALLEL
    /// evacuator does not share this map at all. It took the "per-worker
    /// shards" half of the advice and dropped the `contains_key` half
    /// entirely: `SharedEvac::evacuate` decides the winner with a CAS on the
    /// from-space object's own mark word, each worker records its winning
    /// `(old, new)` pairs into a thread-local `Vec`, and those shards are
    /// merged into one `pointer_map` only after the worker scope joins. So no
    /// `DashMap` is owed here; this map stays plain because it stays
    /// single-threaded. See the module note above `RegionsBase`.
    ///
    /// The caller is documented to hold the regions lock; we cannot
    /// `debug_assert!` directly on lock ownership (parking_lot Mutex offers
    /// no such API), so the invariant is enforced by the type-level
    /// `&mut Vec<G1Region>` parameter (only the lock holder can produce
    /// it) plus this contract comment.
    pub fn scan_and_evacuate_refs(
        &self,
        regions: &mut Vec<G1Region>,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        cset: &RegionSet,
        pointer_map: &mut cratonvm_types::PointerMap,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        work_list: &mut Vec<*mut u8>,
    ) {
        self.scan_and_evacuate_refs_tuned(
            regions,
            obj_ptr,
            header,
            cset,
            pointer_map,
            objects_copied,
            bytes_copied,
            work_list,
            &mut SerialEvacKnobs::off(),
        )
    }

    /// [`Self::scan_and_evacuate_refs`] with the serial Cheney loop's per-pause
    /// [`SerialEvacKnobs`] (JIT round 23 wave 8, lane L9), handed to every
    /// `evacuate_object_tuned` it makes.
    pub fn scan_and_evacuate_refs_tuned(
        &self,
        regions: &mut Vec<G1Region>,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        cset: &RegionSet,
        pointer_map: &mut cratonvm_types::PointerMap,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        work_list: &mut Vec<*mut u8>,
        knobs: &mut SerialEvacKnobs,
    ) {
        // The HOLDER has to be an object too. It arrives from the worklist or
        // from an rset source walk, and a wrong header here is what walks the
        // loops below out of the region entirely — see
        // `holder_walkable_slots`.
        // The holder AT SCAN TIME. `evacuate_object` already screens both ends
        // of every copy and reports `copy_shape_drift`; a worklist holder is a
        // to-space copy that went through it. So if this fires while
        // `evacuate-src` / `evacuate-dest` stayed silent for the same address,
        // the header was sound when it was copied and is not sound now --
        // i.e. something overwrote the copy WITHIN the pause, which is a
        // different defect from anything the copy path can produce.
        //
        // Ten-findings item 4, second half: this is a DETECTOR, not a guard the
        // pause needs, so it costs a cold header read per worklist holder in
        // production for a defect that has never fired outside a debug run.
        // Both reads now sit behind `CRATONVM_G1_VERIFY_HOLDERS`, which is what
        // "skip the plausibility check for objects the pause just copied" asks
        // for while keeping the instrument available to the run that needs it.
        //
        // Not deleted, because the shape it catches is real and nothing else
        // looks for it: a to-space copy whose header was sound when
        // `evacuate_object` screened it and is not sound now means something
        // overwrote the copy WITHIN the pause.
        //
        // The safety argument for skipping it: the loops below never trust this
        // header for their BOUNDS. `holder_walkable_slots` clamps the element
        // count to what the holder's own region actually holds, so a wrong
        // header can no longer walk into the next region -- which was the
        // hazard that put an unconditional screen here.
        if gc_flags().g1_verify_holders
            && !{
                self.note_implausible_legacy_header(regions, obj_ptr, header, "worklist-holder");
                self.candidate_header_is_plausible(regions, obj_ptr as usize)
            }
        {
            let n = EVAC_HOLDER_REJECTED.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                let why = self.describe_rejected_address(regions, obj_ptr as usize);
                tracing::warn!(
                    "[g1] evacuation ref-scan REJECTED a non-object HOLDER (#{n}): obj=0x{:x} {why} — walking its slots would have read outside any live region. Skipped; the pause continues.",
                    obj_ptr as usize,
                );
            }
            return;
        }
        // THE SAME HOLDER SCREEN THE PARALLEL ARM GOT, on the arm that produced
        // every corrupt-cell report measured on H2 (2026-09-06,
        // `org.h2.test.store.TestMVStoreTool` -Xmx256m G1 with the mark driver:
        // 16/16, 14/22, 13/20, 20/20, 22/24, 19/27 reports named THIS walk).
        //
        // The screen above is not this one and cannot stand in for it: it is
        // cursor-based, it is behind `CRATONVM_G1_VERIFY_HOLDERS`, and with that
        // flag ON it rejected ZERO holders in 4 of 4 runs while the parallel
        // arm's word0 test refused 2-10 in the same runs. An ablation on a screen
        // that never fires is a vacuous arm, which is what that A/B produced.
        //
        // Why this walk is a WRITER and not just a reader: the loops below
        // rewrite the cells they visit with forwarding addresses. The bound they
        // carry, `holder_walkable_slots`, is the REGION -- so a holder claiming
        // 448 slots strides 448 16-byte cells (7 KB) over the objects that FOLLOW
        // it in its own region and rewrites every span that decodes as a CSet
        // reference. That is one corrupted header manufacturing the next, and it
        // is why the victims' first two words read as two arena pointers one
        // object-size apart while their bodies read as a neighbour's payload.
        //
        // The comment above argues it is safe to skip a holder screen because
        // `holder_walkable_slots` stops the walk leaving the REGION. That is one
        // region too weak: it protects the next region, not the next OBJECT in
        // this one, and this walk writes.
        {
            if let Some(paired) = self.holder_word0_arena_pointer(header) {
                let n = SERIAL_SCAN_HOLDER_WORD0_IS_POINTER.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 8 || n.is_power_of_two() {
                    tracing::warn!(
                        "[g1] serial ref-scan REFUSED a HOLDER whose first word is an \
                         ARENA POINTER (#{n}): holder=0x{:x} class_id={} num_slots={} \
                         paired=0x{paired:016x} mark=0x{:016x} gc_age={} -- `class_id`+mark \
                         word IS the header's first word, so this is not a header. Walking it \
                         would stride {} cells over its neighbours and REWRITE them. \
                         Skipped; the pause continues.",
                        obj_ptr as usize,
                        header.class_id.as_u32(),
                        header.num_slots(),
                        header.mark_word.load(Ordering::Relaxed),
                        header.gc_age(),
                        header.num_slots(),
                    );
                }
                return;
            }
            // The word0 test above RETURNS on a `Some`, so it is `None` on
            // every path that reaches here. This used to re-ask it and gate on
            // `.is_none()`, which reads as a second condition and is not one:
            // the guard was vacuous and the extra header read paid for nothing.
            if let Some(declared) = self
                .holder_body_cannot_fit_a_region(header)
                .filter(|_| !self.addr_is_in_humongous_region(regions, obj_ptr as usize))
            {
                let n = SERIAL_SCAN_HOLDER_BODY_TOO_BIG.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 8 || n.is_power_of_two() {
                    tracing::warn!(
                        "[g1] serial ref-scan REFUSED a HOLDER whose declared body cannot \
                         FIT IN A REGION (#{n}): holder=0x{:x} class_id={} num_slots={} \
                         declared_body={declared} region_size={} mark=0x{:016x} -- no \
                         allocator placed an object this size here, so the count is not a \
                         count. Walking it would rewrite the rest of the region. Skipped.",
                        obj_ptr as usize,
                        header.class_id.as_u32(),
                        header.num_slots(),
                        self.config.region_size,
                        header.mark_word.load(Ordering::Relaxed),
                    );
                }
                return;
            }
        }
        if header.kind() == ObjectKind::Array {
            if header.element_type() == ArrayElementType::Reference {
                // Clamp to what the holder's own region actually holds. A
                // reference array's element count is a u32 bounded only by
                // `i32::MAX`, and `HEADER_SIZE + len * 8` is never checked
                // against the region — so one wrong header walks into the next
                // region's base. The collector knows the bound; use it.
                let declared = header.array_length() as usize;
                let len = self.holder_walkable_slots(regions, obj_ptr, declared, 8);
                // Lane B — the holder's extent is a property of the HOLDER, and
                // it was being recomputed per SLOT inside both arms of this
                // scan. `holder_extent` calls `object_total_size`, which for an
                // array re-reads the shape dword and does the element-size
                // arithmetic; this loop is the Cheney closure, the largest
                // phase of a young pause, and the value is loop-invariant. It
                // is also passed EAGERLY to `note_ref_write`, whose own fast
                // path is a single `OnceLock::get` — so the argument cost was
                // paid on every evacuated slot of every run, armed or not.
                let extent = holder_extent(obj_ptr, header);
                for i in 0..len {
                    let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + i * 8) };
                    let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                    if raw == 0 {
                        continue;
                    }
                    // Ten-findings item 4: decide CSet membership from the
                    // ADDRESS before touching the referent. The plausibility
                    // screen reads the candidate's header, which for the
                    // old->old references that dominate a source walk is one
                    // cold cache line per slot spent on an object this pause
                    // will never move. Region index and bitset test first;
                    // only a CSet resident is worth validating.
                    let ref_ptr = raw as usize as *mut u8;
                    let Some(region_idx) = self.region_for_ptr(regions, ref_ptr) else {
                        continue;
                    };
                    // Item 1 -- the span-liveness mark, taken from the index the
                    // CSet test below needs anyway.
                    self.note_humongous_reference(regions, region_idx);
                    if !cset.contains(&region_idx) {
                        // Not in the CSet, so not forwarded: the pointer map
                        // only ever names CSet residents (`evacuate_object` is
                        // reached through this same membership test), so the
                        // "already forwarded" rewrite that used to sit here
                        // could never fire.
                        continue;
                    }
                    if !self.evacuation_candidate_is_an_object(
                        regions,
                        "worklist-scan[array]",
                        obj_ptr as *mut u8,
                        i,
                        raw as usize,
                    ) {
                        continue;
                    }
                    // Round-5 fix (CRIT, O(N²)): push `new_ptr` onto the
                    // worklist only when this evacuation was FRESH — the
                    // freshness comes from the evacuation outcome, not a
                    // separate `contains_key` pre-check (a TOCTOU under
                    // parallel evacuation).
                    if let Some((new_ptr, fresh)) = self.evacuate_object_tuned(
                        regions,
                        ref_ptr,
                        pointer_map,
                        objects_copied,
                        bytes_copied,
                        cset,
                        knobs,
                        true,
                    ) {
                        note_ref_write(
                            slot_ptr as usize,
                            new_ptr as u64,
                            obj_ptr as usize,
                            extent,
                            8,
                        );
                        unsafe {
                            std::ptr::write(slot_ptr as *mut u64, new_ptr as u64);
                        }
                        if fresh {
                            work_list.push(new_ptr);
                        }
                    }
                }
            }
        } else {
            // The flat walk gets the SAME region clamp the reference-array branch
            // beside it already has, with the 16-byte `SLOT_SIZE` stride
            // `holder_walkable_slots` grew for it. `candidate_header_is_plausible`
            // validated the holder's tag bytes and its containment; it did not
            // validate that `HEADER_SIZE + num_slots * SLOT_SIZE` lands inside the
            // region, and a legacy slot leaves a region twice as fast as an array
            // element does.
            //
            // `record_outgoing_rset_edges` already makes exactly this argument and
            // applies exactly this clamp; it was the only one of the three flat
            // walks that got it. The two that did not are the two that also WRITE:
            // an unclamped walk here does not merely read past the holder, it
            // rewrites `Value` cells past the holder with forwarded pointers, i.e.
            // it corrupts whatever objects follow it in the region.
            let walkable_slots = self.holder_walkable_slots(
                regions,
                obj_ptr,
                header.num_slots() as usize,
                SLOT_SIZE,
            );
            // Lane B — loop-invariant; see the array arm's copy of this comment.
            let extent = holder_extent(obj_ptr, header);
            for_each_flat_object_reference_capped(
                obj_ptr,
                header,
                0,
                walkable_slots,
                |slot_ptr, raw, compact| {
                    // Item 4 — membership before plausibility; see the array
                    // arm above for why.
                    let ref_ptr = raw as *mut u8;
                    let Some(region_idx) = self.region_for_ptr(regions, ref_ptr) else {
                        return;
                    };
                    // Item 1 -- the span-liveness mark, taken from the index the
                    // CSet test below needs anyway.
                    self.note_humongous_reference(regions, region_idx);
                    if !cset.contains(&region_idx) {
                        return;
                    }
                    if !self.evacuation_candidate_is_an_object(
                        regions,
                        "worklist-scan[object]",
                        obj_ptr as *mut u8,
                        // The SLOT, not the candidate. Both were `raw` here,
                        // so every rejection this site has ever reported
                        // printed a heap address where the offset belongs --
                        // and this site is the one that fires in the millions
                        // (`#134217728` on `TestKillProcessWhileWriting`), so
                        // the whole population was unattributable.
                        (slot_ptr as usize).wrapping_sub(obj_ptr as usize),
                        raw,
                    ) {
                        return;
                    }
                    // Round-5 fix (CRIT, O(N²)): only push the forwarded
                    // target onto the worklist when this call site actually
                    // evacuated it (`fresh`).
                    if let Some((new_ptr, fresh)) = self.evacuate_object_tuned(
                        regions,
                        ref_ptr,
                        pointer_map,
                        objects_copied,
                        bytes_copied,
                        cset,
                        knobs,
                        true,
                    ) {
                        write_flat_object_reference_watched(
                            slot_ptr,
                            new_ptr as usize,
                            compact,
                            obj_ptr,
                            extent,
                        );
                        if fresh {
                            work_list.push(new_ptr);
                        }
                    }
                },
            );
        }
    }

    /// CRIT fix (UAF): scan every object in a non-CSet source region and, for
    /// each reference slot pointing into the CSet, evacuate the target (if not
    /// already evacuated) and rewrite the slot to the forwarded pointer in
    /// place. Without this step the RSet sources collected in young/mixed
    /// Phase 2 were dropped, so cross-region references from non-CSet into the
    /// CSet survived as stale pointers after the CSet regions were reset,
    /// causing silent use-after-free.
    ///
    /// # This is a thin wrapper, and that is the point
    ///
    /// The walk itself is [`walk_source_region_for_cset_refs`], which the
    /// PARALLEL arm (`SharedEvac::seed_source_region`) also calls. The two used
    /// to be separate bodies with a comment on each saying they had to agree;
    /// defect G1-9 was a divergence between them and the 2026-09-20 audit found
    /// five more, three of which changed what the pause FINDS rather than how
    /// fast it finds it -- on the arm that is the default. See
    /// [`SourceWalkArm`] for the full list and for the five things that
    /// genuinely differ between the arms.
    ///
    /// F-05 -- `card_screen` asks for the card table to narrow this walk (the
    /// whole-region skip and the per-object skip; both are additionally gated
    /// on `CRATONVM_G1_CARD_RSET`).
    ///
    /// Callers pass `false` for the JIT-PINNED regions, which
    /// `young_collection`/`mixed_collection` add to the source set on top of
    /// the remembered set's own. Those are walked wholesale as
    /// defence-in-depth -- the comment at the young-collection call site says
    /// so explicitly -- precisely because the collector does not want to assume
    /// every store into them went through `post_write_barrier_rset`. A card
    /// screen is derived from that same assumption, so applying it there would
    /// quietly convert a deliberate belt-and-braces walk into one that trusts
    /// the belt. The cost of the carve-out is bounded by the number of pinned
    /// regions, which is zero unless a thread is in JIT or a TLAB tail is
    /// unretired.
    pub fn scan_source_region_for_cset_refs(
        &self,
        regions: &mut Vec<G1Region>,
        source_idx: usize,
        cset: &RegionSet,
        pointer_map: &mut cratonvm_types::PointerMap,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        work_list: &mut Vec<*mut u8>,
        card_screen: bool,
        precise_rset_liveness: Option<&MarkProvenDead>,
    ) {
        // Hoisted out of the walk: `jit_tlab_skip_spans` takes a mutex and
        // CLONES the span Vec, and the spans cannot change during a pause
        // (every mutator is parked). The parallel arm hoists it one level
        // further still, to once per pause.
        let jit_skips = self.jit_tlab_skip_spans();
        let mut arm = SerialSourceWalk {
            gc: self,
            regions,
            cset,
            pointer_map,
            objects_copied,
            bytes_copied,
            work_list,
        };
        // SAFETY: Phase 2 runs at a stop-the-world safepoint with the regions
        // guard held by the caller, so the region table is quiescent for the
        // walk. `cursor_limit` is `None` because no other evacuation worker is
        // live on this arm -- there is exactly one walker, and reading the live
        // cursor is what this arm has always done.
        unsafe {
            walk_source_region_for_cset_refs(
                self,
                &mut arm,
                cset,
                &jit_skips,
                source_idx,
                card_screen,
                None,
                precise_rset_liveness,
            );
        }
    }

    /// G1AUD-11 — the regions Phase 4 must walk, or `None` for "all of them".
    ///
    /// # Why the whole-heap walk can be narrowed at all
    ///
    /// A slot needing a forwarding rewrite points at an object that was
    /// EVACUATED, i.e. one that lived in the collection set. Only three kinds
    /// of place can hold such a slot:
    ///
    /// * a root — Phase 1 rewrites those directly;
    /// * an object in the CSet — evacuated, and Phase 3 scans every to-space
    ///   copy;
    /// * an object in a non-CSet region — reachable to the collector ONLY
    ///   through that region's membership in the CSet's remembered set, which
    ///   is exactly what Phase 2 walks.
    ///
    /// So the rewrite half of Phase 4 is redundant with Phases 2 and 3 as soon
    /// as the remembered set is complete. It is: every mutator reference store
    /// reaches `post_write_barrier_rset` — the interpreter's and every
    /// native's directly, and every JIT-compiled one through
    /// `jit_putfield_object` since defect G1-2 was closed (under G1 the
    /// `JIT_REGION_BOUNDS` table is never published, so `region_bounds_are_live`
    /// is false and all four inline emitters take the helper).
    ///
    /// # Why the set is derived by observation
    ///
    /// The other half of Phase 4 is the GC-INTERNAL rset rebuild: edges the
    /// collector itself creates by copying an object or by rewriting a slot.
    /// Those live in whichever regions this pause WROTE INTO, and enumerating
    /// them from the evacuator's bookkeeping would mean every present and
    /// future allocation path (serial, parallel, finalizer resurrection, the
    /// kept-region drain) remembering to register itself — a list that is one
    /// forgotten call site away from a silent lost edge.
    ///
    /// Instead the caller snapshots `(region_type, cursor)` for every region
    /// before evacuation, and a region is "changed" iff either differs
    /// afterwards. That catches any write by any path, including paths added
    /// later, and costs one O(regions) pass over two words.
    ///
    /// # What still forces the wide walk
    ///
    /// Two things, and the humongous census is no longer one of them (see the
    /// comment in the body): `CRATONVM_G1_NARROW_FIXUP=0`, and a snapshot that
    /// does not describe this region table. A coarsened remembered set needs no
    /// special case: `live_rset_sources` already answers "every live region"
    /// for one, so the narrow set degrades to the wide one on its own.
    ///
    /// # The two terms the CALLERS cannot supply, and why they are added here
    ///
    /// Lane-B hardening, 2026-09-20. Both are widenings — they can only make
    /// the fix-up walk MORE regions — and both cost a pass over region metadata
    /// with no object walk, so neither is worth a flag of its own.
    ///
    /// * **JNI-pinned regions** (`G1Region::pinned`). Every CSet filter in this
    ///   file excludes them (`young_collection`, `mixed_collection`, and both
    ///   parallel drivers all test `!r.pinned`), so a pinned young region
    ///   survives a pause in which every other young region is evacuated — and
    ///   it is reached by the collector ONLY through the CSet's remembered
    ///   sets. The callers hand this function `live_rset_sources` PLUS the
    ///   JIT-pinned set; the JNI-pinned set is in neither, because
    ///   `young_collection` deliberately declines to walk those wholesale as
    ///   rset sources. That decision is about Phase 2, where a wholesale walk
    ///   would resurrect the pinned region's dead objects' referents. Phase 4
    ///   has no such cost: it rewrites slots and re-derives edges, it does not
    ///   retain anything. So the region that the wide walk used to cover for
    ///   free, and that no other term covers, is added back.
    /// * **Humongous span starts.** A humongous object's slots are walked from
    ///   its `HumongousStart` region, whose cursor is the FULL object size; a
    ///   `HumongousContinuation` index in the walk set breaks out on the filler
    ///   header and rewrites nothing. Every producer of an rset entry is
    ///   believed to key on the OBJECT's address (which for a humongous object
    ///   is always in the start region) rather than the SLOT's, so a
    ///   continuation index should never reach this set — but "should never" is
    ///   the premise, and the cost of being wrong is a humongous object's
    ///   reference array left holding from-space addresses. Mapping each member
    ///   through `humongous_span_start` makes the set closed under the
    ///   containment the walk actually uses.
    pub fn phase4_regions_to_walk(
        &self,
        regions: &[G1Region],
        pre: Option<&[(RegionType, usize)]>,
        rset_sources: &RegionSet,
    ) -> Option<RegionSet> {
        // Ten-findings item 1: the humongous census used to force the wide walk
        // here ("a whole-heap question"), which made every young pause on a
        // heap holding ONE large buffer O(heap). The question is now answered
        // from the spans' remembered sets — see
        // `humongous_spans_referenced_by_rset` — so nothing about humongous
        // objects widens the fix-up any more.
        let pre = pre?;
        if pre.len() != regions.len() {
            // A snapshot that does not describe this region table cannot be
            // used to decide what changed. Fail wide.
            return None;
        }
        let mut set = rset_sources.clone();
        for (i, r) in regions.iter().enumerate() {
            let (was_type, was_cursor) = pre[i];
            if r.region_type() != was_type || r.cursor() != was_cursor {
                set.insert(i);
            }
            // Lane B — the JNI-pin term. See the doc above: a pinned region is
            // held out of every CSet and is in neither of the two sets the
            // callers build, so narrowing took away the only walk that ever
            // covered it. One predicate per region, no object touched, and
            // `pinned` is false for every region on a heap with no live
            // JNI-critical array — which is almost every heap, almost always.
            if r.pinned {
                set.insert(i);
            }
        }
        // Lane B — close the set under humongous span containment. The walk in
        // `update_references_in_regions` reaches a humongous object's slots
        // only from its `HumongousStart` index, so a continuation index in this
        // set walks nothing at all. `humongous_span_start` answers `None` for
        // every ordinary region, so this is a type compare per member of a set
        // that is small by construction.
        let mut span_starts: Vec<usize> = Vec::new();
        for i in set.iter() {
            if regions.get(i).map(|r| r.region_type()) == Some(RegionType::HumongousContinuation) {
                if let Some(start) = humongous_span_start(regions, i) {
                    span_starts.push(start);
                }
            }
        }
        for start in span_starts {
            set.insert(start);
        }
        Some(set)
    }

    /// g1sz: [`Self::drop_closure_settled_tospace_from_fixup`] for the two
    /// parallel drivers, with the census. `no_later_copies` is false when
    /// Phase 3c / 3.5 copied anything after the closure; a wide walk
    /// (`narrow` `None`) is left alone.
    pub fn drop_closure_settled_tospace_for_driver(
        &self,
        regions: &[G1Region],
        pre: &[(RegionType, usize)],
        cset: &RegionSet,
        sources: &RegionSet,
        narrow: Option<&mut RegionSet>,
        edges: Option<Vec<(usize, usize)>>,
        no_later_copies: bool,
    ) {
        let Some(narrow) = narrow else {
            return;
        };
        let edges = edges.filter(|_| no_later_copies);
        let refused = edges.is_none();
        let (n, bytes) =
            self.drop_closure_settled_tospace_from_fixup(regions, pre, cset, sources, narrow, edges);
        self.phase_totals.note_fixup_closure_skip(n, bytes, refused);
    }

    /// g1sz (`CRATONVM_G1_FIXUP_SKIP_CLOSURE_TOSPACE`, default ON): take out
    /// of the narrow Phase-4 set every region whose only change this pause is
    /// the parallel closure's copies, and record the closure's edges in their
    /// targets' remembered sets instead. Answers the regions and bytes it
    /// took out.
    ///
    /// `edges` is `Some` only when the closure vouched for its to-space (see
    /// [`ClosureTospaceEdges`]) and the caller saw no copy after the closure;
    /// `None` leaves the walk as it was.
    ///
    /// A region qualifies when it is now Survivor or Old, outside the
    /// collection set and not JNI-pinned, and either
    ///
    /// * was Free in the pre-evacuation snapshot: every object in it is a
    ///   copy this pause's closure made and scanned (or a filler, or a
    ///   primitive leaf with no slot), so Phase 4 would find no slot to
    ///   rewrite, and the edges it would derive are the ones in `edges`; or
    /// * was already Old and is not one of the pause's remembered-set or
    ///   pinned sources (`sources`): a resumed promotion destination. Its new
    ///   objects are the closure's, as above. Its earlier objects hold no slot
    ///   into the collection set (a holder of one is an rset source, and the
    ///   sources are walked), and their edges were derived by the pause that
    ///   placed them.
    ///
    /// Anything else (a pinned region, a source) is walked as before.
    pub fn drop_closure_settled_tospace_from_fixup(
        &self,
        regions: &[G1Region],
        pre: &[(RegionType, usize)],
        cset: &RegionSet,
        sources: &RegionSet,
        narrow: &mut RegionSet,
        edges: Option<Vec<(usize, usize)>>,
    ) -> (u64, u64) {
        let Some(mut edges) = edges else {
            return (0, 0);
        };
        if pre.len() != regions.len() {
            return (0, 0);
        }
        let mut skip = RegionSet::new();
        let (mut n, mut bytes) = (0u64, 0u64);
        for i in narrow.iter() {
            let r = &regions[i];
            let now = r.region_type();
            let fresh = pre[i].0 == RegionType::Free;
            let resumed =
                pre[i].0 == RegionType::Old && now == RegionType::Old && !sources.contains(&i);
            if (fresh || resumed)
                && matches!(now, RegionType::Survivor | RegionType::Old)
                && !cset.contains(&i)
                && !r.pinned
            {
                skip.insert(i);
                n += 1;
                bytes += r.cursor() as u64;
            }
        }
        // The edges go in whether or not a region was skipped: an edge whose
        // holder is still walked is one Phase 4 adds again, and an rset is a
        // set.
        edges.sort_unstable();
        edges.dedup();
        let generation = self.rset_generation();
        for (target, holder) in edges {
            if let Some(t) = regions.get(target) {
                t.rset.add_reference_in_generation(holder, generation);
            }
        }
        if n > 0 {
            *narrow = narrow.iter().filter(|i| !skip.contains(i)).collect();
        }
        (n, bytes)
    }

    /// gci i9/fa (`CRATONVM_G1_FIXUP_SKIP_CLEAN_PINNED=1`, default OFF; the G1
    /// half of `gci-i1orch-windows-jni-foreign-attach-hangs-RETIRED-UNFIXED-20261018`) -- take
    /// out of the narrow Phase-4 set every region of `pinned` that is pinned
    /// out of this pause's collection set, was not written by it, and carries
    /// no dirty card. Answers the regions and bytes it took out.
    ///
    /// # Why
    ///
    /// The narrow set walks every pinned region WHOLESALE: `phase4_regions_to_walk`
    /// is handed `live_rset_sources` plus the whole pin set. On the vi5g-g1r F
    /// rows that was a median of 11 regions (mean 7.8 MB) per young pause, and
    /// `fixup_ns_ema` (0.53-0.77 ms) was the largest share of a 1.2-1.7 ms
    /// pause against HotSpot G1's 0.23 ms. Phase 2 already screens the same
    /// regions by card (`CRATONVM_G1_CARD_SCREEN_JIT_PINNED`, default ON) and
    /// steps over a wholly clean one without reading an object.
    ///
    /// # Why it is sound, on the premise Phase 2 already runs on
    ///
    /// Phase 4 does two things in a walked region: it rewrites slots that name
    /// an evacuated object, and it re-derives the region's outgoing cross-region
    /// edges (rset entries and cards). With `CRATONVM_G1_CARD_CLEAN` off a card
    /// is cleared only when its region is recycled, so a wholly clean region is
    /// one into which no reference store was ever recorded since it was last
    /// recycled (the barrier inventory on `GcFlags::g1_card_screen_jit_pinned`),
    /// and every edge Phase 4 itself ever derived dirtied its holder's card
    /// (`collect_outgoing_cross_region_edges` stores the card unconditionally).
    /// So such a region holds no slot naming another collectable region: no
    /// slot to rewrite, no edge to derive. The region must also be unchanged by
    /// this pause (its type and cursor equal to the pre-evacuation snapshot: no
    /// copy, promotion or resumed TLAB landed in it), not JNI-pinned (that term
    /// is `phase4_regions_to_walk`'s own widening, kept), and an ordinary
    /// Eden / Survivor / Old region (a humongous span's cards and cursor are
    /// not one region's).
    ///
    /// What the switch gives up is the mask: a region the premise is wrong
    /// about (a store that dirtied no card) used to be repaired here when its
    /// referent was evacuated through another path; with the switch on it is
    /// not. `verify_no_dangling_into_cset` (budgeted on every pause) and
    /// `CRATONVM_G1_DBG_RSET=1` are the checkers; `CRATONVM_DBG_STALE_OBJREF`
    /// does not cover G1.
    pub fn drop_clean_pinned_from_fixup(
        &self,
        regions: &[G1Region],
        pre: &[(RegionType, usize)],
        pinned: &RegionSet,
        narrow: &mut RegionSet,
    ) -> (u64, u64) {
        let cards = self.cards();
        let mut skip = RegionSet::new();
        let (mut n, mut bytes) = (0u64, 0u64);
        for i in pinned.iter() {
            if !narrow.contains(&i) {
                continue;
            }
            let (Some(r), Some(&(was_type, was_cursor))) = (regions.get(i), pre.get(i)) else {
                continue;
            };
            let ty = r.region_type();
            let cursor = r.cursor();
            if !fixup_skip_region_eligible(ty, was_type, cursor, was_cursor, r.pinned) {
                continue;
            }
            if cards.any_dirty_in(r.data.as_ptr() as usize, cursor) {
                continue;
            }
            skip.insert(i);
            n += 1;
            bytes += cursor as u64;
        }
        if n > 0 {
            *narrow = narrow.iter().filter(|i| !skip.contains(i)).collect();
        }
        (n, bytes)
    }

    /// gci i15/rs (`CRATONVM_G1_YOUNG_CARD_SCREEN=1`, default OFF; page
    /// `gci-i2st-the-javac-stall-is-a-progressing-pause-regime-not-a-lost-wakeup-FIXED-20261017`,
    /// G1 half) -- the members of the narrow Phase-4 set that
    /// [`Self::fixup_walk_region`] may walk by YOUNG-dirty card and demote:
    /// every member that is not in the collection set, is an ordinary Eden /
    /// Survivor / Old region, is not JNI-pinned and was not written by this
    /// pause (type and cursor equal to the pre-evacuation snapshot; the
    /// `fixup_skip_region_eligible` rule of i9/fa). A region this pause wrote
    /// into (to-space, promotion target, relabelled) is walked whole as before,
    /// which is where its new objects' cards are derived.
    ///
    /// `None` -- the walk exactly as before -- unless the switch is on and its
    /// premise holds (`young_card_screen_armed`), the walk is narrow, the
    /// snapshot describes this table, and the collection set is all young
    /// (`cset_is_young_only`: a mixed pause keeps the sticky screen).
    pub fn phase4_young_card_screen_set(
        &self,
        regions: &[G1Region],
        pre: &[(RegionType, usize)],
        cset: &RegionSet,
        narrow: Option<&RegionSet>,
    ) -> Option<RegionSet> {
        if !young_card_screen_armed(
            young_card_screen_switch(),
            true,
            gc_flags().g1_card_clean,
        ) {
            return None;
        }
        let narrow = narrow?;
        if pre.len() != regions.len() {
            return None;
        }
        if !cset_is_young_only(cset, |i| regions.get(i).map(|r| r.region_type())) {
            return None;
        }
        let mut set = RegionSet::new();
        for i in narrow.iter() {
            if cset.contains(&i) {
                continue;
            }
            let (Some(r), Some(&(was_type, was_cursor))) = (regions.get(i), pre.get(i)) else {
                continue;
            };
            if fixup_skip_region_eligible(r.region_type(), was_type, r.cursor(), was_cursor, r.pinned) {
                set.insert(i);
            }
        }
        Some(set)
    }

    /// Phase 4 — rewrite interior references in the non-CSet regions this pause
    /// has to walk, using the pointer map.
    ///
    /// `narrow` is [`Self::phase4_regions_to_walk`]'s answer: `Some(set)` walks
    /// exactly that set, and `None` means "every non-CSet, non-Free region",
    /// which is what `CRATONVM_G1_NARROW_FIXUP=0` and an unusable pre-snapshot
    /// both fall back to.
    ///
    /// Returns the [`HumongousCensus`] this walk produced, which is the only
    /// evidence an evacuation pause has about humongous liveness (see
    /// [`Self::eager_reclaim_humongous_locked`]). Piggybacking it here rather
    /// than walking the heap a second time is the whole reason eager reclaim is
    /// affordable per pause: this pass already visits every reference slot in
    /// every non-CSet region.
    pub fn update_references_in_regions(
        &self,
        regions: &mut Vec<G1Region>,
        cset: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
        narrow: Option<&RegionSet>,
    ) -> HumongousCensus {
        self.update_references_in_regions_screened(regions, cset, pointer_map, narrow, None)
    }

    /// gci i15/rs -- [`Self::update_references_in_regions`] with the young
    /// card screen: every region of `young_screen` (built by
    /// [`Self::phase4_young_card_screen_set`], `None` everywhere but the two
    /// young drivers under `CRATONVM_G1_YOUNG_CARD_SCREEN=1`) is walked by
    /// YOUNG-dirty card and its cards demoted afterwards; see
    /// [`Self::fixup_walk_region`]. `None` is byte for byte the walk before.
    pub fn update_references_in_regions_screened(
        &self,
        regions: &mut Vec<G1Region>,
        cset: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
        narrow: Option<&RegionSet>,
        young_screen: Option<&RegionSet>,
    ) -> HumongousCensus {
        let mut census = HumongousCensus::default();
        let rewrite = !pointer_map.is_empty();
        // An empty forwarding map means nothing moved, so there is nothing to
        // rewrite — but the census still has to be taken, because "no object was
        // copied this pause" says nothing at all about whether a humongous span
        // is dead. So the walk runs either way when eager reclaim is on, with
        // only the per-object *rewrite* skipped.
        //
        // When it is OFF, nothing consumes the census and this reverts exactly
        // to the previous behaviour — return immediately, walk nothing. That
        // keeps `CRATONVM_G1_EAGER_HUMONGOUS=0` a COMPLETE off-switch for this
        // feature rather than one that leaves the extra walk running, which is
        // what a bisection lever has to be to be worth anything.
        // G1AUD-9: and there is nothing to take a census OF unless the heap
        // actually holds a humongous span. `g1_eager_humongous` is on by
        // default, so without this test a pause that moved nothing still walked
        // every object of every non-CSet region — the whole live old
        // generation — to build a `referenced` set that
        // `eager_reclaim_humongous_locked` would then find no span to apply.
        // One pass over the region TYPES (no object walk) settles it.
        let heap_has_humongous = regions
            .iter()
            .any(|r| r.region_type() == RegionType::HumongousStart);
        // Ten-findings item 1: a census is taken only when this walk is WIDE. A
        // narrow walk cannot answer a whole-heap question, so eager reclaim
        // reads the humongous spans' remembered sets instead
        // (`humongous_spans_referenced_by_rset`), and a humongous span no
        // longer forces every young pause to walk the whole old generation.
        let want_census = heap_has_humongous && narrow.is_none();
        census.taken = want_census;
        if !rewrite && !want_census {
            return census;
        }

        // RSet rebuild (CORRECTNESS — remembered-set completeness for GC-internal
        // pointer rewrites). An edge whose holder and referent are collected
        // together carries NO remembered-set entry while they share a generation:
        // a young->young edge needs none (young is always collected whole), and an
        // edge a mutator stored while BOTH ends were young recorded only the
        // (now-recycled) young source region. When the holder/referent later age
        // or are promoted to Old, that edge silently becomes Old->young OR
        // Old->old (or humongous->old, the array-of-nodes case) — but no mutator
        // write barrier ever fires for it (the rewrite below, and the promotion
        // copy, are GC-internal). So the referent's region never learns of the
        // source: the next YOUNG GC drops a still-live young referent, AND a MIXED
        // GC that selects the referent's Old region never scans the source so it
        // drops the still-live OLD referent (proven: `MixedChurn` loses
        // cross-referenced young nodes; `PromoteMixed`'s mixed GC drops every node
        // held only through a humongous `keep[]` array — the V7b verifier reports
        // the holder region dangling into a freed CSet region). This is the
        // general (aging/promotion) case of the JIT-pinned-straddle fix landed
        // earlier. Since this pass already walks every non-CSet (=> Old/Humongous)
        // region each collection, we rebuild the rset here at no extra walk: every
        // cross-region reference from this region into a *collectable* region
        // (Eden/Survivor/Old — the region types that can enter a young or mixed
        // CSet) is recorded so the next collection scans this region as a source.
        // (Edges are collected and applied after the walk to keep borrows simple;
        // `add_reference` dedups.)
        // F-02 — the CSet-screened forwarding lookup this walk resolves slots
        // through. See `ForwardLookup`.
        // `RegionView::Raw`, not `Slice`: the walk below takes `&mut regions[i]`
        // for the card table, so a shared slice borrow cannot live across it.
        // This is the identical access the parallel arm's screens already take
        // (see `RegionView`) -- base pointer plus length, one region at a time,
        // through shared references only, over a table that is not reallocated
        // during a pause.
        let regions_len = regions.len();
        let forwards = ForwardLookup {
            collector: self,
            cset,
            map: pointer_map,
            regions: RegionView::Raw(RegionsBase(regions.as_mut_ptr()), regions_len),
        };
        let mut new_rset_edges: Vec<(usize, usize)> = Vec::new();
        // G1AUD-9 — the `(target, holder)` pairs already emitted for the region
        // being walked. `holder` is constant for a whole region walk and an
        // rset is a SET, so the pair is fully determined by `target`.
        //
        // Without it this rebuild pushed one pair per cross-region REFERENCE
        // SLOT: an Old region holding a 100k-element node array that points into
        // Eden emitted 100k copies of the same edge, and the apply loop below
        // then took that destination's rset mutex and hashed the same source
        // 100k times. Cleared per region, so it costs one small `HashSet` and
        // bounds the pushes by the region COUNT rather than the slot count.
        let mut seen_targets: RegionSet = RegionSet::new();
        let jit_skips = self.jit_tlab_skip_spans();
        let dbg_walk = gc_flags().g1_dbg_reach;
        // Lane B — hoisted out of the per-OBJECT body below, where it was one
        // `OnceLock::get` plus a field load for every object of every walked
        // region. The flag is process-wide and immutable after first read, so
        // reading it per object bought nothing.
        let holder_screen = true;
        let mut walk_aborted = false;

        // Lane B — ITERATE THE NARROW SET, do not filter the region table by
        // it.
        //
        // The previous form ran `0..regions.len()` and skipped the regions the
        // narrow set did not name. That is the same *work* per walked region,
        // but it makes the loop's floor O(number of regions) rather than
        // O(|narrow|) — and the number of regions is precisely the quantity
        // this whole narrowing exists to stop the pause from being
        // proportional to. On a 32 GB heap with the default 1 MB region that
        // floor is 32 768 bounds-checked indexes and bitset probes on a pause
        // whose narrow set is typically a few dozen entries.
        //
        // `RegionSet::iter` yields ascending indices (it is a dense bitset, not
        // a hash set), so the walk order is UNCHANGED from the wide form. That
        // matters for more than tidiness: the `[g1][WALKBRK]` diagnostics below
        // and the edge list this builds are read by humans comparing two runs,
        // and a nondeterministic order would make a narrow run and a wide run
        // incomparable even when both are correct.
        let walk_order: Vec<usize> = match narrow {
            Some(n) => n.iter().collect(),
            None => (0..regions.len()).collect(),
        };

        // gci i10/pp (`CRATONVM_G1_PARALLEL_FIXUP=1`, default OFF): the same
        // per-region walk, spread over the evacuation pool, with the edges
        // merged in `walk_order`. See `update_references_in_regions_parallel`
        // for the soundness argument. Never taken for a walk that owes the
        // humongous census (a whole-heap union that arm does not build).
        // gci i15/rs: never for a walk that owes the census (a wide walk; the
        // callers build the set only for a narrow one, this is the backstop).
        let young_screen = young_screen.filter(|_| !want_census);
        if let Some(census_out) = self.try_update_references_in_regions_parallel_tasks(
            regions,
            cset,
            pointer_map,
            rewrite,
            &walk_order,
            &jit_skips,
            dbg_walk,
            holder_screen,
            want_census,
            young_screen,
        ) {
            return census_out;
        }

        for i in walk_order {
            let walk = self.fixup_walk_region(
                regions,
                i,
                cset,
                &forwards,
                rewrite,
                &jit_skips,
                dbg_walk,
                holder_screen,
                &mut new_rset_edges,
                &mut seen_targets,
                want_census.then_some(&mut census),
                young_screen.is_some_and(|s| s.contains(&i)),
            );
            if walk.walked {
                // G1AUD-10: the fix-up's own denominator.
                census.walked_regions = census.walked_regions.saturating_add(1);
                census.walked_bytes = census.walked_bytes.saturating_add(walk.bytes);
            }
            if walk.aborted {
                walk_aborted = true;
            }
        }

        // G1AUD-5: the Phase-4 rebuild is a GC-internal edge producer; stamp
        // its entries with this pause's generation so they age out with the
        // source region like the mutator barrier's do.
        let generation = self.rset_generation();
        for (target_region, source_region) in new_rset_edges {
            regions[target_region]
                .rset
                .add_reference_in_generation(source_region, generation);
        }

        census.complete = want_census && !walk_aborted;
        census
    }

    /// gci i10/pp -- one region of the Phase-4 walk: rewrite every slot of
    /// region `i`'s objects that names an evacuated object, and collect the
    /// region's outgoing cross-region edges into `new_rset_edges` (deduped for
    /// the region by `seen_targets`, which this clears). Returns whether the
    /// region was walked, the bytes it covered, and whether the walk broke off
    /// before the cursor.
    ///
    /// The body of [`Self::update_references_in_regions`]' loop, moved here
    /// unchanged so the serial loop and the pool's workers
    /// ([`Self::update_references_in_regions_parallel`]) run ONE body: a second
    /// copy is the twin-walk shape defect G1-9 was made of.
    ///
    /// Writes only the bytes of region `i`'s objects (the slot rewrites); every
    /// other effect is an atomic (cards, block offsets, counters), a
    /// mutex-guarded diagnostic, or `new_rset_edges` / `seen_targets` /
    /// `census`, which the caller owns. `regions` is only read. So two calls on
    /// DIFFERENT regions may run at once.
    ///
    /// # gci i15/rs -- `young_screen` (`CRATONVM_G1_YOUNG_CARD_SCREEN=1`)
    ///
    /// Set only for a region of [`Self::phase4_young_card_screen_set`]: an
    /// ordinary Eden / Survivor / Old region this YOUNG-ONLY pause did not
    /// write (type and cursor unchanged, not JNI-pinned). Then the walk visits
    /// only the objects overlapping a YOUNG-dirty card (`G1_CARD_DIRTY`), and
    /// afterwards demotes every young-dirty card of the region whose objects
    /// hold no reference into a young region to `G1_CARD_OLD_ONLY`
    /// (`G1CardTable::demote_unkept_young`). This is what makes the young
    /// pause's base cost proportional to the cards that point into young, not
    /// to the old space: the sticky table saturates (no card is cleaned until
    /// its region is recycled; vi7-g1y skipped ~5 % of 35-57 MB), and every
    /// pause re-walked every object of every rset source twice.
    ///
    /// THE INVARIANT (I): every object outside the collection set that holds a
    /// reference into a young region has a young-dirty START card.
    ///
    /// * Every producer stores `G1_CARD_DIRTY` unconditionally (the mutator
    ///   barrier `post_write_barrier_rset`; `record_outgoing_rset_edges`;
    ///   `collect_outgoing_cross_region_edges`, which also re-dirties every
    ///   object this walk visits that holds any cross-region reference; the
    ///   source walk's re-dirty of a holder it rewrote, `young_redirty`). The
    ///   JIT's inline G1 barrier stores no card (it filters, then calls the
    ///   helper). So a card leaves OLD_ONLY on the first such store.
    /// * `G1_CARD_OLD_ONLY` is written only here, over a region walked to its
    ///   cursor without a break, a refusal kept, or a frozen-peer TLAB span
    ///   (any of those leaves the region's cards untouched), after this pause
    ///   rewrote the slots: a card is demoted only when no object overlapping
    ///   a young-dirty card of it still references a young target
    ///   (`young_card_target`) or could not be fully examined.
    /// * Nothing re-targets an existing slot into young without a producer:
    ///   objects only move young -> old, a region becomes young only from Free
    ///   (`clear_range` on reset cleans its cards) or Eden -> Survivor, and a
    ///   region written by a pause is "changed" and walked whole that pause.
    ///
    /// With (I), Phase 2 and this walk may skip every object without a
    /// young-dirty card on a pause whose collection set is all young, and the
    /// rset edges they would have re-derived were derived by the walk that
    /// demoted the card (edges are pruned only when their source is
    /// recycled). Mixed pauses keep the sticky `!= CLEAN` screen, to which an
    /// OLD_ONLY card is dirty. `verify_no_dangling_into_cset` and
    /// `CRATONVM_G1_DBG_RSET=1` are the checkers.
    #[allow(clippy::too_many_arguments)]
    pub fn fixup_walk_region(
        &self,
        regions: &[G1Region],
        i: usize,
        cset: &RegionSet,
        forwards: &ForwardLookup<'_>,
        rewrite: bool,
        jit_skips: &[(usize, usize)],
        dbg_walk: bool,
        holder_screen: bool,
        new_rset_edges: &mut Vec<(usize, usize)>,
        seen_targets: &mut RegionSet,
        mut census: Option<&mut HumongousCensus>,
        young_screen: bool,
    ) -> FixupRegionWalk {
        // A narrow set is built before Phase 5, from indices that may since
        // have been reclassified, so the Free/CSet screens stay exactly
        // where they were rather than moving into the set construction.
        if i >= regions.len() || cset.contains(&i) || regions[i].region_type() == RegionType::Free {
            return FixupRegionWalk::default();
        }

        let cursor = regions[i].cursor();
        // `addr()`, the same base `data.as_mut_ptr()` named: the table is held
        // SHARED here (each pool worker holds its own view of it).
        let base = regions[i].data.addr() as *mut u8;
        // Lane B — region-constant, read once per region rather than once
        // per object. A HUMONGOUS object legitimately spans more than one
        // region, so "bigger than a region" is not evidence about it; only
        // the non-humongous case can use that test. See the `refuse` screen
        // below for the rest of that argument.
        let region_is_humongous = matches!(
            regions[i].region_type(),
            RegionType::HumongousStart | RegionType::HumongousContinuation
        );
        let mut offset = 0usize;
        let mut trail = WalkTrail::default();
        let mut walk = FixupRegionWalk {
            walked: true,
            bytes: cursor as u64,
            aborted: false,
        };
        // New holder — the memo above is keyed on target alone.
        seen_targets.clear();

        // gci i15/rs -- the young card screen (see this fn's doc). Never on a
        // humongous region or a walk that owes the census; the set builder
        // already excludes both, this is the backstop.
        let young_screen = young_screen && !region_is_humongous && census.is_none();
        let cards = self.cards();
        if young_screen && !cards.any_young_dirty_in(base as usize, cursor) {
            // Nothing in this region can reference a young region (I): no
            // slot to rewrite, no edge to derive, no card to demote.
            return walk;
        }
        // The start cards of the visited objects that keep a young reference
        // (or could not be fully examined), and whether the region may be
        // demoted at the end (cleared by anything the walk did not examine).
        let mut young_keep = young_screen.then(|| cards.empty_set(base as usize, cursor));
        let mut young_demote_ok = young_screen;

        while offset < cursor {
            let obj_ptr = unsafe { base.add(offset) };
            // INT-3 — frozen-peer TLAB tail: uninitialized, no walkable
            // filler; must be skipped before any byte is interpreted.
            if let Some(skip) = jit_tlab_skip_span_len(jit_skips, obj_ptr as usize) {
                if dbg_walk {
                    trail.record(offset, u32::MAX, b'S', skip);
                }
                // gci i15/rs: bytes nobody examined -- leave the cards alone.
                young_demote_ok = false;
                offset += skip;
                continue;
            }
            // TLAB-retire gap sentinel: skip its exact span — breaking
            // here would leave the rest of this region's references
            // un-fixed-up after evacuation (stale pointers).
            if let Some(gap) = gap_filler_len(obj_ptr) {
                if dbg_walk {
                    trail.record(offset, u32::MAX, b'G', gap);
                }
                offset += gap;
                continue;
            }
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            // Round-9 gc CRIT-1: humongous continuation filler covers
            // the entire region; skip the rest.
            if is_humongous_filler(header) {
                // gci i15/rs: the rest of the region is not examined.
                young_demote_ok = false;
                break;
            }
            let obj_size = object_total_size(header);

            if obj_size < HEADER_SIZE || offset + obj_size > cursor {
                if dbg_walk {
                    eprintln!(
                        "[g1][WALKBRK] phase4 region={i} off={offset:#x} \
                         cursor={cursor:#x} obj_size={obj_size:#x} \
                         type={:?} reuse_epoch={} cid={} kind={} alen={} slots={} \
                         trail=[{}]",
                        regions[i].region_type(),
                        regions[i].reuse_epoch,
                        header.class_id.as_u32(),
                        ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                        header.array_length(),
                        header.num_slots(),
                        trail.render(),
                    );
                    eprintln!(
                        "[g1][WALKBRK-BUMPS] phase4 region={i} {} carves=[{}] bumps=[{}]",
                        regions[i].tlab_trail.lock().describe_owner_or(
                            &regions[i].bump_trail.lock(),
                            regions[i].reuse_epoch,
                            offset,
                        ),
                        regions[i].tlab_trail.lock().render(),
                        regions[i].bump_trail.lock().render(),
                    );
                }
                // The rest of this region was never inspected, so any
                // reference it holds into a humongous span is unseen. Say
                // so: `eager_reclaim_humongous_locked` must not read the
                // resulting `referenced` set as a death certificate.
                walk.aborted = true;
                break;
            }
            if dbg_walk {
                trail.record(
                    offset,
                    header.class_id.as_u32(),
                    ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                    obj_size,
                );
            }
            // gci i15/rs -- the per-object young screen, over the object's
            // whole span (a store names its holder's START card; the span test
            // covers a straddling holder whichever card it was).
            if young_screen && !cards.any_young_dirty_in(obj_ptr as usize, obj_size) {
                offset += obj_size;
                continue;
            }

            // PHASE 4 WALKS THE SAME HEADER, TWICE, WITH NO BOUND AND NO
            // SCREEN. Both calls below reach
            // `for_each_flat_object_reference_trusting_header`, whose own doc
            // says it "bounds it by nothing" -- and `update_object_refs`
            // WRITES every slot it visits. Measured on H2 2026-09-06: with the
            // evacuation-time screens on, corrupt-cell reports moved to
            // exactly these two callers (`collect_outgoing_cross_region_edges`
            // and `update_object_refs`), carrying the same arena-pointer
            // holders the evacuator had just refused.
            //
            // Screening here rather than inside the two walks is what covers
            // both with one test: this is the only caller of either, and it is
            // the only place that holds the region geometry the test needs.
            // A HUMONGOUS object legitimately spans more than one region,
            // so "bigger than a region" is not evidence about it. Only the
            // non-humongous case can use that test.
            // Lane B — `region_is_humongous` is a property of the REGION,
            // and it was being recomputed (with a bounds-checked index into
            // the region table) once per object. Hoisted to the top of this
            // region's walk as `region_is_humongous`; the flag load beside
            // it is hoisted out of the whole fix-up as `holder_screen`.
            let refuse = holder_screen
                && (self.holder_word0_arena_pointer(header).is_some()
                    || (!region_is_humongous
                        && self.holder_body_cannot_fit_a_region(header).is_some()));
            if refuse {
                let n = PHASE4_HOLDER_REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 8 || n.is_power_of_two() {
                    tracing::warn!(
                        "[g1] Phase-4 fixup REFUSED a HOLDER (#{n}): holder=0x{:x} \
                         class_id={} num_slots={} mark=0x{:016x} region={i} -- its \
                         header is not a header, and BOTH walks below trust the \
                         count. `update_object_refs` would rewrite the rest of the \
                         region with forwarding addresses. Skipped; the walk \
                         advances by this object's own size.",
                        obj_ptr as usize,
                        header.class_id.as_u32(),
                        header.num_slots(),
                        header.mark_word.load(Ordering::Relaxed),
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
                        "phase4-fixup-holder",
                    );
                }
                // gci i15/rs: a refusal is not evidence of absence.
                if let Some(k) = young_keep.as_mut() {
                    k.insert_addr(obj_ptr as usize);
                }
            }
            if !refuse {
                if rewrite {
                    update_object_refs(obj_ptr, header, forwards);
                }
                let keeps_young = self.collect_outgoing_cross_region_edges_noting_young(
                    regions,
                    i,
                    obj_ptr as *mut u8,
                    header,
                    new_rset_edges,
                    seen_targets,
                    census.as_deref_mut(),
                    young_screen.then_some(cset),
                );
                if keeps_young {
                    if let Some(k) = young_keep.as_mut() {
                        k.insert_addr(obj_ptr as usize);
                    }
                }
            }
            offset += obj_size;
        }
        // gci i15/rs -- demote the young-dirty cards no visited object keeps,
        // only after a walk that examined the whole region (no break, no
        // abort, no frozen-peer span). See (I) in this fn's doc.
        if young_demote_ok && !walk.aborted {
            if let Some(k) = young_keep.as_ref() {
                cards.demote_unkept_young(base as usize, cursor, k);
            }
        }
        walk
    }

    /// gci i10/pp -- how many pool helpers Phase 4 may use this walk; `0`
    /// keeps the serial loop. `CRATONVM_G1_PARALLEL_FIXUP` (read per walk),
    /// never for a walk that owes the humongous census, and never for fewer
    /// than two regions (nothing to share).
    ///
    /// R23-W6-FLIP (JIT round 23 wave 6, lane L4): default ON, `=0` kills.
    /// n23-w5, `R23W3Keep obj -XX:+UseG1GC -Xmx1g`, 2 x 2 interleaved runs:
    /// `fixup_us` of the first (3.5 M-object) pause 189-231 ms -> 30 ms, and the
    /// young-sizing model, which charges the fix-up per pause, then stops
    /// shrinking eden to 51 regions: 1 young pause per run instead of 5, and
    /// `keep-obj` 431-483 -> 60-76 ns/op (HotSpot G1 54); `fact` lines equal.
    pub fn phase4_parallel_helpers(&self, want_census: bool, regions_to_walk: usize) -> usize {
        if want_census || regions_to_walk < 2 {
            return 0;
        }
        let pool = self.evac_pool();
        phase4_fixup_helpers(self.parallel_worker_count(), pool.helpers(), regions_to_walk)
    }

    /// gci i10/pp (`CRATONVM_G1_PARALLEL_FIXUP=1`, default OFF; the G1 half of
    /// `gci-i1orch-windows-jni-foreign-attach-hangs-RETIRED-UNFIXED-20261018`) -- Phase 4's
    /// region walk over the persistent evacuation pool. Returns
    /// `(walked_regions, walked_bytes, aborted)`.
    ///
    /// # Why
    ///
    /// The foreign-attach rows put a median of 11 pinned regions (mean 7.8 MB)
    /// into every young pause's Phase-4 set, walked object by object on the
    /// driver alone while the pool is parked: `fixup_ns_ema` 0.53-0.77 ms of a
    /// 1.2-1.7 ms pause against HotSpot G1's 0.23 ms pause.
    ///
    /// # Why it is sound
    ///
    /// Each index of `walk_order` is claimed by exactly one thread (one
    /// `fetch_add` cursor), and [`Self::fixup_walk_region`] writes only the
    /// slots of objects of the region it is handed -- the disjointness the
    /// parallel seed walk (`CRATONVM_G1_PARALLEL_SEED`) already rests on. Its
    /// other effects are shared-safe: card and block-offset stores are
    /// atomics, region type and cursor are atomics read only, the forwarding
    /// map and the CSet are read only, the refusal queue and counters are a
    /// static mutex and atomics. The remembered-set edges are NOT applied by
    /// the workers: each region's edges land in that region's own result slot
    /// and the driver applies them after the pool's barrier, in `walk_order`
    /// (ascending region index) -- the sequence the serial loop applies, so
    /// the rsets and their generation stamps come out the serial walk's.
    ///
    /// The table is reached through [`RegionsBase`], as in `parallel_evacuate`:
    /// the caller holds the regions guard for the whole call and nothing
    /// dereferences it during the dispatch; `EvacPool::scope`'s barrier is the
    /// point after which the driver touches `regions` again.
    ///
    /// What differs from the serial walk: the ORDER of the walk's own
    /// diagnostics (`[g1][WALKBRK]`, the refusal warnings) across regions.
    ///
    /// gci i15/rs: the production path calls
    /// [`Self::update_references_in_regions_parallel_screened`]; this form is
    /// what the i10/pp unit tests drive.
    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn update_references_in_regions_parallel(
        &self,
        regions: &mut Vec<G1Region>,
        cset: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
        rewrite: bool,
        walk_order: &[usize],
        jit_skips: &[(usize, usize)],
        dbg_walk: bool,
        holder_screen: bool,
        helpers: usize,
    ) -> (u32, u64, bool) {
        self.update_references_in_regions_parallel_screened(
            regions,
            cset,
            pointer_map,
            rewrite,
            walk_order,
            jit_skips,
            dbg_walk,
            holder_screen,
            helpers,
            None,
        )
    }

    /// gci i15/rs -- [`Self::update_references_in_regions_parallel`] with the
    /// young card screen's region set (see
    /// [`Self::update_references_in_regions_screened`]); `None` is the walk
    /// before. Each region's cards are read and demoted only by the worker
    /// that claimed the region (regions never share a card).
    #[allow(clippy::too_many_arguments)]
    pub fn update_references_in_regions_parallel_screened(
        &self,
        regions: &mut Vec<G1Region>,
        cset: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
        rewrite: bool,
        walk_order: &[usize],
        jit_skips: &[(usize, usize)],
        dbg_walk: bool,
        holder_screen: bool,
        helpers: usize,
        young_screen: Option<&RegionSet>,
    ) -> (u32, u64, bool) {
        let regions_len = regions.len();
        let base = RegionsBase(regions.as_mut_ptr());
        // One slot per region of the walk, written once by the thread that
        // claimed it; the merge below reads them in `walk_order`.
        let results: Vec<Mutex<Option<(FixupRegionWalk, Vec<(usize, usize)>)>>> =
            (0..walk_order.len()).map(|_| Mutex::new(None)).collect();
        {
            let next = AtomicUsize::new(0);
            let body = |_worker: usize| {
                // SAFETY: `base` / `regions_len` describe the live region table,
                // which the caller's guard keeps alive and unreallocated for the
                // whole dispatch, and every access through this view is a shared
                // read (see the doc above) -- the access `RegionView::Raw`
                // already hands the evacuation workers. (Copied whole into a
                // local first, so the closure captures the `Sync` wrapper and
                // never the bare pointer field.)
                let table: RegionsBase = base;
                let view: &[G1Region] = unsafe {
                    std::slice::from_raw_parts(table.0 as *const G1Region, regions_len)
                };
                let forwards = ForwardLookup {
                    collector: self,
                    cset,
                    map: pointer_map,
                    regions: RegionView::Raw(table, regions_len),
                };
                let mut seen_targets = RegionSet::new();
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= walk_order.len() {
                        break;
                    }
                    let i = walk_order[k];
                    let mut edges: Vec<(usize, usize)> = Vec::new();
                    let walk = self.fixup_walk_region(
                        view,
                        i,
                        cset,
                        &forwards,
                        rewrite,
                        jit_skips,
                        dbg_walk,
                        holder_screen,
                        &mut edges,
                        &mut seen_targets,
                        None,
                        // gci i15/rs: a region's cards are touched only by the
                        // worker that claimed it (regions never share a card).
                        young_screen.is_some_and(|s| s.contains(&i)),
                    );
                    *results[k].lock() = Some((walk, edges));
                }
            };
            self.evac_pool().scope(helpers, &body, || body(helpers));
        }
        self.phase_totals.note_fixup_parallel();
        // G1AUD-5, as on the serial arm: stamped with this pause's generation.
        let generation = self.rset_generation();
        let (mut walked_regions, mut walked_bytes, mut aborted) = (0u32, 0u64, false);
        for slot in results {
            let Some((walk, edges)) = slot.into_inner() else {
                continue;
            };
            if walk.walked {
                walked_regions = walked_regions.saturating_add(1);
                walked_bytes = walked_bytes.saturating_add(walk.bytes);
            }
            aborted |= walk.aborted;
            for (target_region, source_region) in edges {
                regions[target_region]
                    .rset
                    .add_reference_in_generation(source_region, generation);
            }
        }
        (walked_regions, walked_bytes, aborted)
    }

    /// Parallel Phase-4 walk supporting both `want_census` (humongous-span
    /// union) and slot-chunked `HumongousStart` reference arrays (fusing slot
    /// rewrite + outgoing rset/humongous collection in one parallel pass).
    #[allow(clippy::too_many_arguments)]
    fn try_update_references_in_regions_parallel_tasks(
        &self,
        regions: &mut Vec<G1Region>,
        cset: &RegionSet,
        pointer_map: &cratonvm_types::PointerMap,
        rewrite: bool,
        walk_order: &[usize],
        jit_skips: &[(usize, usize)],
        dbg_walk: bool,
        holder_screen: bool,
        want_census: bool,
        young_screen: Option<&RegionSet>,
    ) -> Option<HumongousCensus> {
        let workers = self.parallel_worker_count();
        let pool = self.evac_pool();
        if workers <= 1 || pool.helpers() == 0 {
            return None;
        }

        const CHUNK_SLOTS: usize = 131_072;
        struct HumongousPhase4Meta {
            region_idx: usize,
            obj_ptr: usize,
            walkable: usize,
            needs_card_dirty: std::sync::atomic::AtomicBool,
        }
        enum Phase4Task {
            WholeRegion { region_idx: usize },
            HumongousChunk { meta_idx: usize, start: usize, end: usize, first: bool },
        }

        let mut metas: Vec<HumongousPhase4Meta> = Vec::new();
        let mut tasks: Vec<Phase4Task> = Vec::with_capacity(walk_order.len());
        for &i in walk_order {
            let can_split = i < regions.len()
                && !cset.contains(&i)
                && regions[i].region_type() == RegionType::HumongousStart
                && !young_screen.is_some_and(|s| s.contains(&i));
            if can_split {
                let cursor = regions[i].cursor();
                let base = regions[i].data.addr();
                if cursor >= HEADER_SIZE
                    && jit_tlab_skip_span_len(jit_skips, base).is_none()
                    && gap_filler_len(base as *const u8).is_none()
                {
                    let header = unsafe { &*(base as *const ObjectHeader) };
                    let obj_size = object_total_size(header);
                    if !is_humongous_filler(header)
                        && obj_size >= HEADER_SIZE
                        && obj_size == cursor
                        && header.kind() == ObjectKind::Array
                        && header.element_type() == ArrayElementType::Reference
                        && !(holder_screen && self.holder_word0_arena_pointer(header).is_some())
                    {
                        let declared = header.array_length() as usize;
                        let walkable = self.holder_walkable_slots(regions, base as *mut u8, declared, 8);
                        if walkable >= CHUNK_SLOTS * 2 {
                            let meta_idx = metas.len();
                            metas.push(HumongousPhase4Meta {
                                region_idx: i,
                                obj_ptr: base,
                                walkable,
                                needs_card_dirty: std::sync::atomic::AtomicBool::new(
                                    walkable < declared,
                                ),
                            });
                            let mut start = 0usize;
                            while start < walkable {
                                let end = (start + CHUNK_SLOTS).min(walkable);
                                tasks.push(Phase4Task::HumongousChunk {
                                    meta_idx,
                                    start,
                                    end,
                                    first: start == 0,
                                });
                                start = end;
                            }
                            continue;
                        }
                    }
                }
            }
            tasks.push(Phase4Task::WholeRegion { region_idx: i });
        }

        let helpers = phase4_fixup_helpers(workers, pool.helpers(), tasks.len());
        if helpers == 0 {
            return None;
        }

        let regions_len = regions.len();
        let base = RegionsBase(regions.as_mut_ptr());
        let results: Vec<Mutex<Option<(FixupRegionWalk, Vec<(usize, usize)>, Option<RegionSet>)>>> =
            (0..tasks.len()).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let tasks_ref = &tasks;
        let metas_ref = &metas;
        let body = |_worker: usize| {
            let table: RegionsBase = base;
            let view: &[G1Region] =
                unsafe { std::slice::from_raw_parts(table.0 as *const G1Region, regions_len) };
            let forwards = ForwardLookup {
                collector: self,
                cset,
                map: pointer_map,
                regions: RegionView::Raw(table, regions_len),
            };
            let mut seen_targets = RegionSet::new();
            loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                if k >= tasks_ref.len() {
                    break;
                }
                let mut edges: Vec<(usize, usize)> = Vec::new();
                match tasks_ref[k] {
                    Phase4Task::WholeRegion { region_idx } => {
                        let mut task_census = want_census.then(HumongousCensus::default);
                        let walk = self.fixup_walk_region(
                            view,
                            region_idx,
                            cset,
                            &forwards,
                            rewrite,
                            jit_skips,
                            dbg_walk,
                            holder_screen,
                            &mut edges,
                            &mut seen_targets,
                            task_census.as_mut(),
                            young_screen.is_some_and(|s| s.contains(&region_idx)),
                        );
                        *results[k].lock() =
                            Some((walk, edges, task_census.map(|c| c.referenced)));
                    }
                    Phase4Task::HumongousChunk {
                        meta_idx,
                        start,
                        end,
                        first,
                    } => {
                        let meta = &metas_ref[meta_idx];
                        let holder = meta.region_idx;
                        let obj_ptr = meta.obj_ptr as *mut u8;
                        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                        let data_start = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET) };
                        let extent = holder_extent(obj_ptr, header);
                        let mut refs_set = want_census.then(RegionSet::new);
                        let mut dirtied_card = false;
                        seen_targets.clear();
                        for s_idx in start..end {
                            let slot_ptr = unsafe { data_start.add(s_idx * 8) };
                            let raw_val: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                            if raw_val == 0 {
                                continue;
                            }
                            let mut cur_addr = raw_val as usize;
                            let Some(mut target_ridx) = self.lookup_region_for_addr(cur_addr) else {
                                continue;
                            };
                            if rewrite && cset.contains(&target_ridx) {
                                if let Some(&new_addr) = pointer_map.get(&cur_addr) {
                                    note_ref_write(
                                        slot_ptr as usize,
                                        new_addr as u64,
                                        meta.obj_ptr,
                                        extent,
                                        8,
                                    );
                                    unsafe {
                                        std::ptr::write(slot_ptr as *mut u64, new_addr as u64);
                                    }
                                    cur_addr = new_addr;
                                    let Some(new_ridx) = self.lookup_region_for_addr(cur_addr)
                                    else {
                                        continue;
                                    };
                                    target_ridx = new_ridx;
                                }
                            }
                            if let Some(span) = humongous_span_start(view, target_ridx) {
                                if span != holder {
                                    if let Some(cs) = refs_set.as_mut() {
                                        cs.insert(span);
                                    }
                                    dirtied_card = true;
                                    if seen_targets.insert(span) {
                                        edges.push((span, holder));
                                    }
                                }
                            } else if target_ridx != holder
                                && is_collectable_region_type(view[target_ridx].region_type())
                            {
                                dirtied_card = true;
                                if seen_targets.insert(target_ridx) {
                                    edges.push((target_ridx, holder));
                                }
                            }
                        }
                        if dirtied_card {
                            meta.needs_card_dirty.store(true, Ordering::Relaxed);
                        }
                        let walk = FixupRegionWalk {
                            walked: first,
                            bytes: if first {
                                view[holder].cursor() as u64
                            } else {
                                0
                            },
                            aborted: false,
                        };
                        *results[k].lock() = Some((walk, edges, refs_set));
                    }
                }
            }
        };
        pool.scope(helpers, &body, || body(helpers));
        self.phase_totals.note_fixup_parallel();

        for meta in &metas {
            if meta.needs_card_dirty.load(Ordering::Relaxed) {
                let header = unsafe { &*(meta.obj_ptr as *const ObjectHeader) };
                self.cards
                    .dirty_addr_at_object_start(crate::g1_cards::ObjectStart::of_header(header));
            }
        }

        let mut census = HumongousCensus::default();
        census.taken = want_census;
        let generation = self.rset_generation();
        let mut aborted = false;
        for slot in results {
            let Some((walk, edges, task_refs)) = slot.into_inner() else {
                continue;
            };
            if walk.walked {
                census.walked_regions = census.walked_regions.saturating_add(1);
                census.walked_bytes = census.walked_bytes.saturating_add(walk.bytes);
            }
            aborted |= walk.aborted;
            for (target_region, source_region) in edges {
                regions[target_region]
                    .rset
                    .add_reference_in_generation(source_region, generation);
            }
            if let Some(refs) = task_refs {
                for idx in refs.iter() {
                    census.referenced.insert(idx);
                }
            }
        }
        census.complete = want_census && !aborted;
        Some(census)
    }

    /// Record (into `out`) every cross-region reference from `obj` (which lives
    /// in non-CSet `holder` region — Old or humongous, OR a JIT/JNI-pinned
    /// Eden / Survivor region held out of the CSet: gci i10/pp, the
    /// foreign-attach rows walk ~11 such young regions per pause) into a
    /// *collectable* region as a
    /// `(target_region, holder)` rset edge. See `update_references_in_regions`
    /// for why this is required for remembered-set completeness.
    ///
    /// A "collectable" target is one whose region type can enter a collection
    /// set: `Eden`/`Survivor` (young CSet) OR `Old` (mixed CSet). Recording the
    /// Old targets is what lets a mixed GC find the live old graph reachable only
    /// through a GC-internal Old->old or humongous->old edge (the `PromoteMixed`
    /// drop). Humongous targets are never collected by young/mixed evacuation, so
    /// edges into them carry no rset entry (consistent with the mutator barrier,
    /// which records cross-region edges regardless of target type but whose
    /// entries on humongous regions are simply never consulted).
    ///
    /// gci i15/rs: Phase 4 calls
    /// [`Self::collect_outgoing_cross_region_edges_noting_young`]; this form
    /// (the same edges and cards, no answer) is what the unit tests drive.
    #[cfg_attr(not(test), allow(dead_code))]
    #[allow(clippy::too_many_arguments)]
    pub fn collect_outgoing_cross_region_edges(
        &self,
        regions: &[G1Region],
        holder: usize,
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        out: &mut Vec<(usize, usize)>,
        seen: &mut RegionSet,
        census: Option<&mut HumongousCensus>,
    ) {
        let _ = self.collect_outgoing_cross_region_edges_noting_young(
            regions, holder, obj_ptr, header, out, seen, census, None,
        );
    }

}
