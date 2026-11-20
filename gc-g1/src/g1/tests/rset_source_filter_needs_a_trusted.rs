#[allow(unused_imports)]
use super::*;

    /// Without a completed mark there is no proof a holder is dead, so there
    /// is no filter, and the verifier treats a stale slot's holder as live.
    #[test]
    pub fn rset_source_filter_needs_a_trusted_completed_mark() {
        let gc = G1Collector::new(many_region_config());
        let holder = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1);
        let h_idx = gc
            .lookup_region_for_addr(holder.as_ptr() as usize)
            .expect("holder is in a region");
        gc.with_regions_mut(|regions| regions[h_idx].set_region_type(RegionType::Old));
        {
            let regions = gc.regions.read();
            assert!(
                gc.low_space_rset_source_liveness(&regions).is_none(),
                "no completed mark: a root trace is not a liveness proof, so no filter"
            );
        }

        arm_completed_mark_with_dead_old_region(&gc, h_idx);
        let addr = holder.as_ptr() as usize;
        {
            let regions = gc.regions.read();
            let dead = gc
                .low_space_rset_source_liveness(&regions)
                .expect("a trusted completed mark yields the filter");
            assert!(
                dead.proves_dead(&regions[h_idx], h_idx, addr),
                "an unmarked pre-TAMS Old holder is proven dead"
            );
        }

        // Marked: live at the snapshot, never filtered.
        gc.regions.read()[h_idx].mark_bitmap.try_mark(addr);
        {
            let regions = gc.regions.read();
            let dead = gc.low_space_rset_source_liveness(&regions).unwrap();
            assert!(!dead.proves_dead(&regions[h_idx], h_idx, addr));
        }
        gc.regions.write()[h_idx].mark_bitmap.clear_exclusive();

        // A cleanup that distrusted its closure proves nothing.
        gc.last_mark_trusted.store(false, Ordering::Release);
        {
            let regions = gc.regions.read();
            assert!(gc.low_space_rset_source_liveness(&regions).is_none());
        }
        gc.last_mark_trusted.store(true, Ordering::Release);

        // A registered finalizable holder is resurrected by finalization: it
        // and what it reaches are kept.
        *gc.pending_finalizer_roots.lock() = vec![addr];
        {
            let regions = gc.regions.read();
            let dead = gc.low_space_rset_source_liveness(&regions).unwrap();
            assert!(
                !dead.proves_dead(&regions[h_idx], h_idx, addr),
                "a pending finalizable object is not proven dead"
            );
        }
        gc.pending_finalizer_roots.lock().clear();

        // Allocated after the snapshot: at or above TAMS, never proven dead.
        let late = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1);
        let late_idx = gc.lookup_region_for_addr(late.as_ptr() as usize).unwrap();
        if late_idx == h_idx {
            let regions = gc.regions.read();
            let dead = gc.low_space_rset_source_liveness(&regions).unwrap();
            assert!(!dead.proves_dead(&regions[h_idx], h_idx, late.as_ptr() as usize));
        }
    }

    /// gci i14/hd item 7: the live-only visitor skips exactly what a trusted
    /// completed mark proves dead, visits everything else, and visits nothing
    /// (answering `None`) without a proof.
    #[test]
    pub fn gci_i14hd_visitor_skips_proven_dead_old_objects() {
        let gc = G1Collector::new(many_region_config());
        let dead = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1);
        let live = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1);
        let h_idx = gc
            .lookup_region_for_addr(dead.as_ptr() as usize)
            .expect("the object is in a region");
        let mut seen = 0usize;
        assert!(gc
            .for_each_object_skipping_proven_dead(&mut |_: *mut u8, _: usize| seen += 1)
            .is_none());
        assert_eq!(seen, 0, "no proof: nothing visited");

        arm_completed_mark_with_dead_old_region(&gc, h_idx);
        gc.regions.read()[h_idx].mark_bitmap.try_mark(live.as_ptr() as usize);
        let mut visited: Vec<usize> = Vec::new();
        let skipped = gc
            .for_each_object_skipping_proven_dead(&mut |p: *mut u8, _: usize| visited.push(p as usize))
            .expect("a trusted completed mark is a proof");
        assert!(skipped >= 1);
        assert!(!visited.contains(&(dead.as_ptr() as usize)), "the unmarked pre-TAMS object is skipped");
        if gc.lookup_region_for_addr(live.as_ptr() as usize) == Some(h_idx) {
            assert!(visited.contains(&(live.as_ptr() as usize)), "the marked one is visited");
        }
        assert_eq!(visited.len() as u64 + skipped, gc.walk_objects().len() as u64);
        gc.regions.write()[h_idx].mark_bitmap.clear_exclusive();
    }

    /// A completed mark is stronger evidence than a conservative root word.
    /// The V7b walk used to re-admit this unmarked pre-TAMS holder because the
    /// root slice happened to contain its address, then reported its dead slot
    /// as a live post-evacuation UAF.  That is the exact shape a dropped old
    /// `byte[][]` takes in the native factory regression.
    #[test]
    pub fn lane_b_verifier_uses_completed_mark_to_reject_a_conservative_dead_root() {
        let gc = G1Collector::new(many_region_config());
        let (holder, target, h_idx, t_idx) = lane_b_cross_region_holder(&gc);
        let cset: RegionSet = [t_idx].into_iter().collect();
        let map = cratonvm_types::PointerMap::default();
        arm_completed_mark_with_dead_old_region(&gc, h_idx);

        {
            let mut regions = gc.regions.write();
            regions[t_idx].set_region_type(RegionType::Free);
            assert!(
                !regions[h_idx]
                    .mark_bitmap
                    .is_marked(holder.as_ptr() as usize),
                "fixture holder must be pre-TAMS garbage, not snapshot-live"
            );
        }

        {
            let regions = gc.regions.read();
            gc.verify_no_dangling_into_cset_within(
                &regions,
                &cset,
                &map,
                &[holder],
                usize::MAX,
                usize::MAX,
            );
        }
        assert!(
            gc.v7b_reported.lock().is_empty(),
            "a completed mark must reject a conservative root word naming dead holder {holder:?}; target={target:?}"
        );
    }

    /// …and the two populations it must NOT report, or the counter is noise.
    ///
    /// A self-forward (`k -> k`) in a region `free_or_keep_cset` KEPT is a live
    /// object sitting exactly where the slot points. Both terms are tested at
    /// once because getting either wrong fires on every evacuation failure,
    /// which is the one condition under which an operator most needs the
    /// counter to mean something.
    #[test]
    pub fn lane_b_verifier_stays_quiet_for_a_self_forward_in_a_kept_region() {
        let gc = G1Collector::new(many_region_config());
        let (_holder, target, _h_idx, t_idx) = lane_b_cross_region_holder(&gc);

        let cset: RegionSet = [t_idx].into_iter().collect();
        let mut map = cratonvm_types::PointerMap::default();
        // Self-forwarded: the object did not move.
        map.insert(target.as_ptr() as usize, target.as_ptr() as usize);

        {
            // The region is KEPT — `free_or_keep_cset` leaves its type alone —
            // so nothing here is stale.
            let regions = gc.regions.read();
            gc.verify_no_dangling_into_cset_within(
                &regions,
                &cset,
                &map,
                &[],
                usize::MAX,
                usize::MAX,
            );
        }
        // The per-COLLECTOR report ledger, not the process-global counter:
        // `PHASE4_STALE_FORWARD_SLOTS` is a `static` and the test harness runs
        // this beside the positive test in the same process, so a
        // "the count did not move" assertion on it is a race, not a check.
        assert!(
            gc.v7b_reported.lock().is_empty(),
            "an identity forward in a kept region is a live object at the \
             address the slot names; reporting it would make the counter fire \
             on every evacuation failure"
        );
    }

    /// The budgeted sampler must resume INSIDE the region it ran out in.
    ///
    /// It used to store `region + 1`, so the tail of any region holding more
    /// than `verify_budget()` objects was not "picked up by a later pause" (as
    /// the comment claimed) — it was skipped, permanently, because the next
    /// pass to reach that region started at offset 0 and truncated at the same
    /// object. A 1 MiB region of 32-byte objects holds 32 768 of them against a
    /// default budget of 4 096, so that is the ordinary case.
    #[test]
    pub fn lane_b_budgeted_verify_resumes_inside_the_region_it_truncated() {
        let gc = make_collector();
        for i in 0..64 {
            let o = gc.alloc_object(ClassId::new(1), 2);
            gc.set_field(o, 0, Value::Int(i));
        }

        let regions = gc.regions.read();
        let empty_cset: RegionSet = RegionSet::new();
        let empty_map = cratonvm_types::PointerMap::default();

        gc.cset_verify_cursor.store(0, Ordering::Relaxed);
        gc.verify_no_dangling_into_cset_within(
            &regions,
            &empty_cset,
            &empty_map,
            &[],
            4,
            usize::MAX,
        );
        let first = gc.cset_verify_cursor.load(Ordering::Relaxed);
        assert!(
            first >> 32 > 0,
            "truncating part-way through a region must record WHERE, got {first:#x}"
        );

        gc.verify_no_dangling_into_cset_within(
            &regions,
            &empty_cset,
            &empty_map,
            &[],
            4,
            usize::MAX,
        );
        let second = gc.cset_verify_cursor.load(Ordering::Relaxed);
        assert!(
            second >> 32 > first >> 32,
            "the second budgeted pass must start where the first stopped and \
             get FURTHER, or coverage never accumulates: first={first:#x} \
             second={second:#x}"
        );
    }

    /// gci i29/cv2 (gci-i26cv) — the resume offset is valid only in the
    /// incarnation of the region it was recorded in.
    ///
    /// Cleanup frees all-dead Old regions after remark and the next Mixed
    /// pause refills them as promotion destinations; a verifier that resumed
    /// at the old offset landed inside an object of the new grid and strode
    /// its body as 16-byte `Value` cells (the `corrupt Value cell` bursts of
    /// vi26-cv C1 r2, `caller=` this walk, holder `grid=INTERIOR`). Here the
    /// grid is left unchanged and only the incarnation moves, so "walked from
    /// the top" is observable exactly: the pass truncates where the first did.
    #[test]
    pub fn gci_i29_cv2_budgeted_verify_does_not_resume_into_a_recycled_region() {
        let gc = make_collector();
        for i in 0..64 {
            let o = gc.alloc_object(ClassId::new(1), 2);
            gc.set_field(o, 0, Value::Int(i));
        }
        let empty_cset: RegionSet = RegionSet::new();
        let empty_map = cratonvm_types::PointerMap::default();

        gc.cset_verify_cursor.store(0, Ordering::Relaxed);
        {
            let regions = gc.regions.read();
            gc.verify_no_dangling_into_cset_within(
                &regions,
                &empty_cset,
                &empty_map,
                &[],
                4,
                usize::MAX,
            );
        }
        let first = gc.cset_verify_cursor.load(Ordering::Relaxed);
        assert!(
            first >> 32 > 0,
            "the budgeted pass must stop inside the region, got {first:#x}"
        );
        let ridx = first & 0xffff_ffff;

        // A new incarnation of the same region: what cleanup's free plus the
        // next pause's refill leave behind (`reset` bumps exactly this).
        gc.with_regions_mut(|rs| rs[ridx].reuse_epoch = rs[ridx].reuse_epoch.wrapping_add(1));
        {
            let regions = gc.regions.read();
            gc.verify_no_dangling_into_cset_within(
                &regions,
                &empty_cset,
                &empty_map,
                &[],
                4,
                usize::MAX,
            );
        }
        let second = gc.cset_verify_cursor.load(Ordering::Relaxed);
        assert_eq!(
            second, first,
            "a recycled region must be walked from the top, not resumed at an \
             offset recorded in its previous incarnation: first={first:#x} \
             second={second:#x}"
        );

        // Control: the incarnation is now stamped again, so the next pass
        // resumes and advances -- the guard does not turn the resume off.
        {
            let regions = gc.regions.read();
            gc.verify_no_dangling_into_cset_within(
                &regions,
                &empty_cset,
                &empty_map,
                &[],
                4,
                usize::MAX,
            );
        }
        let third = gc.cset_verify_cursor.load(Ordering::Relaxed);
        assert!(
            third >> 32 > second >> 32,
            "the same incarnation must still resume: second={second:#x} third={third:#x}"
        );
    }

    /// gci i29/cv2 — the stamp moves on every `reset` (every free) and on a
    /// retype, and on nothing else the verifier does.
    #[test]
    pub fn gci_i29_cv2_resume_stamp_moves_on_reset_and_retype() {
        let gc = make_collector();
        let o = gc.alloc_object(ClassId::new(1), 2);
        let ridx = gc
            .lookup_region_for_addr(o.as_ptr() as usize)
            .expect("the object's region");
        let before = {
            let regions = gc.regions.read();
            G1Collector::cset_verify_resume_stamp_of(&regions[ridx])
        };
        let again = {
            let regions = gc.regions.read();
            G1Collector::cset_verify_resume_stamp_of(&regions[ridx])
        };
        assert_eq!(before, again, "reading the stamp must not move it");

        let mut after_reset = 0;
        gc.with_regions_mut(|rs| {
            rs[ridx].reset(1);
            after_reset = G1Collector::cset_verify_resume_stamp_of(&rs[ridx]);
        });
        assert_ne!(before, after_reset, "a reset is a new incarnation");

        let mut after_retype = 0;
        gc.with_regions_mut(|rs| {
            rs[ridx].set_region_type(RegionType::Old);
            after_retype = G1Collector::cset_verify_resume_stamp_of(&rs[ridx]);
        });
        assert_ne!(after_reset, after_retype, "a retype restarts the walk too");
    }

    /// gci i29/cv2 — the flagless sweep finds a live object whose first
    /// header word was zeroed (the vi26-cv C1 r19 shape: `class_id=0`,
    /// `gc_flags=0`, second word intact) and nothing else: sound headers carry
    /// `GC_FLAG_HEADER`, and all-zero pairs (unused TLAB tails) are zeroed
    /// space, counted but never reported.
    #[test]
    pub fn gci_i29_cv2_flagless_sweep_finds_a_zeroed_first_header_word_only() {
        let gc = make_collector();
        let mut objs = Vec::new();
        for i in 0..16 {
            let o = gc.alloc_object(ClassId::new(7), 2);
            gc.set_field(o, 0, Value::Int(i));
            objs.push(o);
        }
        {
            let regions = gc.regions.read();
            let (found, tally) = gc.flagless_headers_in(&regions);
            assert!(found.is_empty(), "sound headers are never flagless: {found:?}");
            assert!(tally.objects >= 16, "the walk must step over the objects: {tally:?}");
            assert_eq!(tally.desynced_regions, 0, "{tally:?}");
        }
        let victim = objs[5].as_ptr() as usize;
        // SAFETY: test-only, single-threaded; the first header word of a live
        // legacy object (class id + mark). The second word (its slot count)
        // is left alone, so the grid still closes -- exactly r19's object.
        unsafe { std::ptr::write(victim as *mut u64, 0) };
        let regions = gc.regions.read();
        let (found, tally) = gc.flagless_headers_in(&regions);
        assert_eq!(
            found.iter().copied().collect::<Vec<_>>(),
            vec![victim],
            "exactly the zeroed header, at its object start: {tally:?}"
        );
        assert_eq!(tally.desynced_regions, 0, "the grid still closes: {tally:?}");
    }

    /// gci i29/cv2 — the sweep's NEW / CARRIED split: an address that was
    /// flagless before a pause and moved is carried, not produced by it.
    #[test]
    pub fn gci_i29_cv2_flagless_new_and_carried_split() {
        let prev: FxHashSet<usize> = [0x1000usize, 0x2000].into_iter().collect();
        let current: FxHashSet<usize> = [0x2000usize, 0x9000, 0xa000].into_iter().collect();
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x9000);
        map.insert(0x3000, 0xa000);
        let (new, carried) = flagless_new_and_carried(&prev, &current, Some(&map));
        assert_eq!(new, vec![0xa000], "0xa000's source was not flagless before");
        assert_eq!(carried, vec![(0x9000, 0x1000)]);
        let (new_no_map, carried_no_map) = flagless_new_and_carried(&prev, &current, None);
        assert_eq!(new_no_map, vec![0x9000, 0xa000]);
        assert!(carried_no_map.is_empty());
    }

    /// gci i30/hz — a legacy object of `fields` `Int(0)` slots, asserted
    /// legacy (an unregistered class id has no compact layout), and its
    /// region's `(base, cursor)`.
    pub fn gci_i30_hz_legacy_object(gc: &G1Collector, cid: u32, fields: usize) -> ObjectRef {
        let o = gc.alloc_object(ClassId::new(cid), fields);
        for i in 0..fields {
            gc.set_field(o, i, Value::Int(0));
        }
        // SAFETY: a freshly allocated object's header.
        let h = unsafe { &*(o.as_ptr() as *const ObjectHeader) };
        assert!(
            !cratonvm_types::is_compact_object(h),
            "fixture class {cid} must be legacy (16-byte cells)"
        );
        o
    }

    pub fn gci_i30_hz_prefix_of(gc: &G1Collector, addr: usize) -> (usize, usize) {
        let regions = gc.regions.read();
        let idx = gc
            .region_for_ptr(&regions, addr as *mut u8)
            .expect("fixture address is in a live region");
        (regions[idx].data.as_ptr() as usize, regions[idx].cursor())
    }

    /// gci i30/hz — an address inside an object's extent (r19's
    /// `HashMap + 0x20` shape: a cell whose words read as a class-0, flagless
    /// header) is COVERED by that object's header.
    #[test]
    pub fn gci_i30_hz_cover_finds_the_object_an_interior_address_lies_in() {
        let gc = make_collector();
        let o = gci_i30_hz_legacy_object(&gc, 30_013, 3);
        let base_o = o.as_ptr() as usize;
        let (base, cursor) = gci_i30_hz_prefix_of(&gc, base_o);
        let interior = base_o + 32;
        // SAFETY: `interior` is 8-aligned inside the region's allocated prefix.
        let cover = unsafe { flagless_candidate_cover(base, cursor, interior) };
        assert_eq!(
            cover,
            FlaglessCover::Covered {
                start: base_o,
                class_id: 30_013,
                size: HEADER_SIZE + 3 * SLOT_SIZE,
            },
            "{}",
            cover.describe(interior)
        );
    }

    /// gci i30/hz — an object start whose own first word was zeroed (the shape
    /// gci-i29cv2 first assumed) is NOT covered: the object before it ends
    /// exactly there, so the refusal never applies to it.
    #[test]
    pub fn gci_i30_hz_cover_does_not_claim_a_zeroed_object_start() {
        let gc = make_collector();
        let a = gci_i30_hz_legacy_object(&gc, 30_014, 1);
        let b = gci_i30_hz_legacy_object(&gc, 30_014, 1);
        let (a_addr, b_addr) = (a.as_ptr() as usize, b.as_ptr() as usize);
        assert_eq!(
            b_addr,
            a_addr + HEADER_SIZE + SLOT_SIZE,
            "fixture: two consecutive allocations on one thread are adjacent"
        );
        // SAFETY: test-only, single-threaded; `b`'s first header word.
        unsafe { std::ptr::write(b_addr as *mut u64, 0) };
        let (base, cursor) = gci_i30_hz_prefix_of(&gc, b_addr);
        // SAFETY: `b_addr` is 8-aligned inside the allocated prefix.
        let cover = unsafe { flagless_candidate_cover(base, cursor, b_addr) };
        assert_eq!(
            cover,
            FlaglessCover::NotCovered {
                start: a_addr,
                class_id: 30_014,
                size: HEADER_SIZE + SLOT_SIZE,
            },
            "{}",
            cover.describe(b_addr)
        );
        // Outside the prefix: no verdict.
        // SAFETY: rejected before any read.
        let out = unsafe { flagless_candidate_cover(base, cursor, base + cursor) };
        assert_eq!(out, FlaglessCover::Undecided("outside-prefix"));
    }

    /// gci i30/hz — the reference-slot screen REFUSES a covered flagless
    /// candidate by default (no forwarding state is installed inside the
    /// covering object) and counts it; the covering object itself still passes.
    #[test]
    pub fn gci_i30_hz_screen_refuses_a_covered_flagless_candidate() {
        let gc = make_collector();
        let holder = gci_i30_hz_legacy_object(&gc, 30_015, 1);
        let o = gci_i30_hz_legacy_object(&gc, 30_013, 3);
        let base_o = o.as_ptr() as usize;
        let interior = base_o + 32;
        // SAFETY: the two words of `o`'s cell 1, read before and after.
        let before = unsafe { std::ptr::read((interior + 4) as *const [u32; 3]) };
        {
            let regions = gc.regions.read();
            assert!(
                gc.evacuation_candidate_is_an_object(
                    &regions,
                    "gci-i30-hz-test",
                    holder.as_ptr(),
                    HEADER_SIZE,
                    base_o,
                ),
                "the covering object is a sound candidate"
            );
            assert!(
                !gc.evacuation_candidate_is_an_object(
                    &regions,
                    "gci-i30-hz-test",
                    holder.as_ptr(),
                    HEADER_SIZE,
                    interior,
                ),
                "an interior address must be refused"
            );
        }
        assert_eq!(
            gc.cleanup_scrub_counters()
                .evac_candidate_flagless_covered_refused
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            gc.cleanup_scrub_counters()
                .evac_candidate_flagless
                .load(Ordering::Relaxed),
            1
        );
        // SAFETY: as above.
        let after = unsafe { std::ptr::read((interior + 4) as *const [u32; 3]) };
        assert_eq!(before, after, "the screen writes nothing into the covering object");
    }

    /// gci i39/cv -- fixture: a two-slot reference array `holder` whose slot 0
    /// names a humongous `target` (always in a region of its own; slot 1 stays
    /// null). The target's rset names the holder's region iff `named`, and the
    /// holder's card is dirty iff `dirty`. Answers `(gc, holder, target, t_idx,
    /// h_idx)`.
    pub fn gci_i39_cv_fixture(
        named: bool,
        dirty: bool,
    ) -> (G1Collector, ObjectRef, ObjectRef, usize, usize) {
        let gc = make_collector();
        let holder = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
        let target = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        assert!(gc.is_humongous(target));
        gc.set_array_element(holder, 0, Value::Object(Some(target)))
            .unwrap();
        let h_idx = gc
            .lookup_region_for_addr(holder.as_ptr() as usize)
            .expect("holder in a region");
        let t_idx = gc
            .lookup_region_for_addr(target.as_ptr() as usize)
            .expect("span in a region");
        assert_ne!(h_idx, t_idx, "fixture: holder and target in different regions");
        let generation = gc.rset_generation();
        {
            let regions = gc.regions.read();
            regions[t_idx].rset.clear();
            if named {
                regions[t_idx]
                    .rset
                    .add_reference_in_generation(h_idx, generation);
            }
            let h = &regions[h_idx];
            gc.cards()
                .clear_range(h.data.as_ptr() as usize, h.data.len());
        }
        if dirty {
            gc.cards().dirty_addr(holder.as_ptr() as usize);
        }
        (gc, holder, target, t_idx, h_idx)
    }

    /// gci i39/cv -- snapshot, reset region `t_idx` (the free), audit.
    pub fn gci_i39_cv_free_and_audit(
        gc: &G1Collector,
        t_idx: usize,
        pointer_map: Option<&cratonvm_types::PointerMap>,
    ) -> FreedSlotAuditOut {
        let pre = {
            let regions = gc.regions.read();
            FreedSlotAuditPre::take(&regions[..], [t_idx], &[])
        };
        let generation = gc.rset_generation();
        gc.with_regions_mut(|rs| rs[t_idx].reset(generation));
        let regions = gc.regions.read();
        freed_slot_audit(gc, &regions[..], &pre, "evac", pointer_map)
    }

    /// gci i39/cv -- a slot that names a region the free just reset is
    /// counted once (the null slot beside it is not), and a holder the rset
    /// names with a dirty card reads `walk-skip`. The audit writes nothing.
    #[test]
    pub fn gci_i39_cv_freed_slot_audit_counts_a_slot_that_outlives_its_target_region() {
        let (gc, holder, target, t_idx, _) = gci_i39_cv_fixture(true, true);
        let out = gci_i39_cv_free_and_audit(&gc, t_idx, None);
        assert_eq!(out.freed_regions, 1);
        assert_eq!(out.survivors, 1, "exactly the one non-null slot into the freed region");
        assert_eq!(out.by_why[FreedSlotWhy::WalkSkip as usize], 1);
        assert_eq!(
            gc.cleanup_scrub_counters()
                .freed_slot_survivors
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            gc.get_array_element(holder, 0).unwrap(),
            Value::Object(Some(target)),
            "the audit reads only"
        );
    }

    /// gci i39/cv -- the rset and card halves of `why=` are told apart.
    #[test]
    pub fn gci_i39_cv_freed_slot_audit_splits_rset_hole_card_hole_and_stale_entry() {
        let (gc, _, _, t_idx, _) = gci_i39_cv_fixture(false, true);
        let out = gci_i39_cv_free_and_audit(&gc, t_idx, None);
        assert_eq!(out.survivors, 1);
        assert_eq!(
            out.by_why[FreedSlotWhy::RsetHole as usize],
            1,
            "rset does not name the holder"
        );

        let (gc, _, _, t_idx, _) = gci_i39_cv_fixture(true, false);
        let out = gci_i39_cv_free_and_audit(&gc, t_idx, None);
        assert_eq!(
            out.by_why[FreedSlotWhy::CardHole as usize],
            1,
            "named, card clean"
        );

        let (gc, _, _, t_idx, h_idx) = gci_i39_cv_fixture(true, true);
        let later = gc.rset_generation() + 5;
        gc.with_regions_mut(|rs| rs[h_idx].set_recycled_in_generation(later));
        let out = gci_i39_cv_free_and_audit(&gc, t_idx, None);
        assert_eq!(
            out.by_why[FreedSlotWhy::RsetStale as usize],
            1,
            "an entry older than the holder region's recycle is dropped by the scans"
        );
    }

    /// gci i39/cv -- a target the pass COPIED elsewhere reads `fixup-miss`.
    #[test]
    pub fn gci_i39_cv_freed_slot_audit_reads_a_forwarded_target_as_a_fixup_miss() {
        let (gc, _, target, t_idx, _) = gci_i39_cv_fixture(true, true);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(target.as_ptr() as usize, target.as_ptr() as usize + 64);
        let out = gci_i39_cv_free_and_audit(&gc, t_idx, Some(&map));
        assert_eq!(out.by_why[FreedSlotWhy::FixupMiss as usize], 1);
    }

    /// gci i39/cv -- no free, no survivor (and no walk).
    #[test]
    pub fn gci_i39_cv_freed_slot_audit_is_silent_without_a_free() {
        let (gc, _, _, t_idx, _) = gci_i39_cv_fixture(true, true);
        let regions = gc.regions.read();
        let pre = FreedSlotAuditPre::take(&regions[..], [t_idx], &[]);
        let out = freed_slot_audit(&gc, &regions[..], &pre, "evac", None);
        assert_eq!(out.freed_regions, 0);
        assert_eq!(out.survivors, 0);
        assert_eq!(out.regions_walked, 0);
    }

    /// gci i39/cv -- the pre-registered precedence of `why=`.
    #[test]
    pub fn gci_i39_cv_freed_slot_why_precedence() {
        use super::FreedSlotRset as R;
        use super::FreedSlotWhy as W;
        assert_eq!(W::classify(true, true, R::Missing, false), W::MarkedHolder);
        assert_eq!(W::classify(false, true, R::Missing, false), W::FixupMiss);
        assert_eq!(W::classify(false, false, R::Missing, true), W::RsetHole);
        assert_eq!(W::classify(false, false, R::Stale, true), W::RsetStale);
        assert_eq!(W::classify(false, false, R::Named, false), W::CardHole);
        assert_eq!(W::classify(false, false, R::Coarsened, true), W::WalkSkip);
        assert_eq!(W::classify(false, false, R::NoSnapshot, true), W::WalkSkip);
    }
