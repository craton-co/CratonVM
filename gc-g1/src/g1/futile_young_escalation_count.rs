#[allow(unused_imports)]
use super::*;

impl G1Collector {
    /// Escalations granted so far (see [`Self::take_futile_young_escalation`]).
    pub fn futile_young_escalation_count(&self) -> u64 {
        self.futile_young_escalations.load(Ordering::Relaxed)
    }

    /// The single-threaded young evacuation. [`Self::young_collection`] is the
    /// dispatcher; this is the body it falls back to.
    ///
    /// Split out for F-02: the serial and parallel drivers now install
    /// forwarding pointers by the same protocol but retire them in different
    /// places (the parallel one inside `parallel_evacuate`, this one between
    /// Phases 4 and 5), and a test that goes through the dispatcher exercises
    /// whichever the ambient flags select. A retirement test that silently ran
    /// the OTHER path passed with `retire_forwards` deleted, which is exactly
    /// the vacuous check this split exists to prevent.
    pub fn young_collection_serial(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let start = std::time::Instant::now();
        let mut regions = self.regions.write();
        // SECURITY FIX (V7a): this collection will reset/retype CSet
        // regions (Phase 5). Bump the RSet TLS-cache epoch *before* any
        // reclassification so every mutator's fast-path cache entry is
        // invalidated and falls to the Free-gated slow path. `Release`
        // pairs with the `Acquire` load on the fast path.
        self.bump_rset_cache_epoch();
        // Item 1 -- a pause's span marks belong to that pause.
        self.clear_humongous_references();
        // G1AUD-9: a destination cursor from the previous pause names a region
        // this one may free, retype, or put in the CSet. Drop both before any
        // reclassification so the first placement of each type does not start
        // from a hint that is guaranteed to miss.
        self.reset_evac_dest_hints();
        // JIT round 23 wave 8, lane L9 (`CRATONVM_G1_SERIAL_FORWARD_PRESIZE`).
        let mut pointer_map: cratonvm_types::PointerMap = self.serial_pointer_map();
        let mut objects_copied = 0usize;
        let mut bytes_copied = 0usize;

        // Regions holding a conservatively-discovered JIT root must NOT be
        // evacuated: the collector cannot rewrite the (register/spill) slot that
        // holds the only reference, so the object must stay put (the
        // generational collector achieves this by not moving anything while in
        // JIT). Map each published root address to its region and exclude those
        // from the CSet, exactly like JNI-pinned regions. Empty unless a thread
        // is in JIT (the common case for a JIT-triggered young GC).
        let jit_pinned_regions = self.pause_pinned_region_set(&regions, roots);
        if self.gc_log_enabled.load(Ordering::Relaxed) && !jit_pinned_regions.is_empty() {
            eprintln!(
                "[g1][PINS] young pause: {}",
                self.describe_pin_set(&regions, &jit_pinned_regions),
            );
        }
        if gc_flags().g1_dbg_pins {
            eprintln!(
                "[g1][PINS] young pause: jit_active={} pin_addrs={} pin_regions={:?} {}",
                cratonvm_gc_core::gc_quiescence::is_active(),
                cratonvm_gc_core::gc_quiescence::pinned_jit_root_count(),
                jit_pinned_regions,
                self.describe_pin_set(&regions, &jit_pinned_regions),
            );
        }

        // Build collection set: all Eden + Survivor regions (skip pinned + any
        // region holding a conservative JIT root)
        let cset: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                !r.pinned
                    && !jit_pinned_regions.contains(i)
                    && (r.region_type() == RegionType::Eden
                        || r.region_type() == RegionType::Survivor)
            })
            .map(|(i, _)| i)
            .collect();
        // JIT round 23 wave 3, lane L4: the young-source prerequisite --
        // every young region this pause leaves out of its set is walked for
        // its outgoing edges before anything below reads a remembered set.
        {
            let in_cset: RegionSet = cset.iter().copied().collect();
            self.record_excluded_young_sources(&mut regions, &|i| in_cset.contains(&i));
        }

        // See `low_space_rset_source_liveness`: under allocation pressure a
        // holder the last trustworthy completed mark proves dead is not a
        // remembered-set source. Other pauses retain the region-level RSet
        // contract. Before Phase 1 moves roots or CSet objects.
        let precise_rset_liveness = self
            .native_alloc_pressure
            .load(Ordering::Relaxed)
            .then(|| self.low_space_rset_source_liveness(&regions))
            .flatten();

        // How many young regions each pin vocabulary kept out of this CSet.
        // These are the regions the pause CANNOT reclaim, so they belong in
        // the cycle record: an operator seeing G1 reclaim nothing needs to be
        // able to tell "nothing was garbage" from "everything was pinned".
        let (jni_pinned_out, jit_pinned_out) =
            count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);
        // Carried to `phases` below, which is not in scope yet.
        let cset_regions_selected = cset.len() as u32;

        if cset.is_empty() {
            let mut degraded = cratonvm_gc_core::gc_metrics::g1_degraded::EMPTY_COLLECTION_SET;
            if jni_pinned_out > 0 {
                degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::JNI_PINNED_REGIONS_EXCLUDED;
            }
            if jit_pinned_out > 0 {
                degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::JIT_PINNED_REGIONS_EXCLUDED;
            }
            cratonvm_gc_core::gc_metrics::record_g1_cycle(
                cratonvm_gc_core::gc_metrics::g1_cycle_kind::YOUNG,
                0,
                0,
                (jni_pinned_out + jit_pinned_out) as u32,
                0,
                degraded,
            );
            // gci i32/ec (`i30-rp-report.md` hand-back 1): every young region
            // pinned out is still an in-place pause; arm the kept-region
            // verdict (`note_refproc_empty_cset_pause`).
            self.record_kept_young_regions_empty_cset(&regions);
            self.note_refproc_empty_cset_pause(jni_pinned_out + jit_pinned_out);
            return GcResult {
                stats: GcStats {
                    objects_copied: 0,
                    bytes_copied: 0,
                    bytes_freed: 0,
                },
                pointer_map,
            };
        }

        // A pinned region must never enter a collection set: Phase 5 resets
        // (zero-fills and re-types) every CSet region that holds no
        // self-forwarded object, so a pinned region in the CSet is a
        // relocated-or-freed JNI-critical array — the precise thing
        // `pin_region_for_addr` promises cannot happen. The filter above is
        // the enforcement; this states it so a future edit to the predicate
        // cannot quietly drop a term. Cheap: one pass over a short Vec.
        debug_assert!(
            cset.iter()
                .all(|&i| !regions[i].pinned && !jit_pinned_regions.contains(&i)),
            "G1 young CSet contains a pinned region"
        );

        // G1AUD-11 — pre-evacuation `(region_type, cursor)` snapshot. A region
        // whose either field differs afterwards is one this pause WROTE INTO,
        // and those, plus the CSet's remembered-set sources, are the only
        // regions the Phase-4 fix-up can have anything to do. Derived by
        // observation rather than from the evacuator's bookkeeping, so a
        // future allocation path cannot forget to register itself — see
        // `phase4_regions_to_walk`. One pass over two words per region.
        let pre_evac: Vec<(RegionType, usize)> = regions
            .iter()
            .map(|r| (r.region_type(), r.cursor()))
            .collect();

        // Phase 1: Scan roots and evacuate reachable objects from CSet
        let cset_set: RegionSet = cset.iter().copied().collect();
        let mut work_list: Vec<*mut u8> = Vec::new();
        // G1AUD-10 — per-phase breakdown. Five `Instant`s on a path that
        // already takes one; see `G1PausePhases` for why this is not
        // debug-gated.
        let mut phases = G1PausePhases::default();
        // What this pause was ALLOWED to collect, and what the two pin
        // vocabularies kept out of it -- see the field docs.
        phases.cset_regions = cset_regions_selected;
        phases.jni_pinned_out = jni_pinned_out as u32;
        phases.jit_pinned_out = jit_pinned_out as u32;
        let mut phase_mark = std::time::Instant::now();

        // Process root references
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if let Some(region_idx) = self.region_for_ptr(&regions, old_ptr) {
                if cset_set.contains(&region_idx) {
                    let plausible = self.note_root_object_plausibility(&regions, old_ptr as usize);
                    // A ROOT THAT IS NOT AN OBJECT START IS NOT EVACUATED.
                    //
                    // Measured on `TestKillProcessWhileWriting` (2026-09-02): a
                    // CSet root landed 0x28 bytes INSIDE a live reference array
                    // (`grid=INTERIOR of=0x108 delta=0x28 size=0x110 cid=185
                    // kind=Array`, the extent independently confirmed by the
                    // region's own carve trail). `evacuate_object` then read
                    // that ELEMENT as a header: a heap pointer's low half became
                    // the class id and its high half became `num_slots` --
                    // 0x200, which is why every holder in that investigation
                    // reported `num_slots=512` -- so the object was sized at
                    // 0x2010, eight kilobytes were copied into a Survivor
                    // region, and `*root` was rewritten to name the result.
                    // Scanning that fake object as 512 legacy slots is the
                    // source of the rejected candidates, the dangling references
                    // and the segfault this class is filed for.
                    //
                    // Copying it was never right. `note_root_object_plausibility`
                    // has reported the condition since it was written and
                    // `NON_OBJECT_ROOT_COPIED` counted the copies -- the guard
                    // was measurement-only, and what it measured was the
                    // collector manufacturing an object out of an array element.
                    //
                    // Leaving the root alone cannot be worse. A conservative JIT
                    // root is a stack word that merely LOOKS like a pointer, so
                    // an interior or garbage one names nothing the mutator reads
                    // through; and pinning its region keeps whatever it is
                    // interior to from moving out from under it, which is the
                    // treatment `pin_region_for_addr` already exists to give a
                    // conservatively-discovered root.
                    if !plausible {
                        let n = NON_OBJECT_ROOT_SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
                        if n <= 8 || n.is_power_of_two() {
                            tracing::warn!(
                                "[g1] a NON-OBJECT root was SKIPPED (#{n}): 0x{:x} — it is not the start of a live object, so evacuating it would have sized an object from bytes that are not a header. The root is left unchanged and its region pinned.",
                                old_ptr as usize,
                            );
                        }
                        // NO PIN HERE. `G1Region::pinned` is the JNI-critical
                        // pin, cleared only by a matching `unpin_region` or by
                        // `reset` -- and a pinned region is never collected, so
                        // it is never reset. Setting it from this loop leaked
                        // the region for the life of the process, and bought
                        // nothing even for this pause: the CSet is already
                        // chosen by the time a root is processed.
                        //
                        // `pinned_region_set_including_non_object_roots` is
                        // where a non-object root's region is kept out of the
                        // CSet, which is BEFORE selection and per-pause. This
                        // arm is the backstop for anything that reaches here
                        // anyway.
                        continue;
                    }
                    // Step 9: `fresh` is ignored here — the root loop keeps its
                    // existing unconditional push (a duplicate root re-scans
                    // idempotently). Gating it on `fresh` is deferred to the
                    // parallel evacuator (where duplicate worklist entries
                    // matter for worker load).
                    if let Some((new_ptr, _fresh)) = self.evacuate_object(
                        &mut regions,
                        old_ptr,
                        &mut pointer_map,
                        &mut objects_copied,
                        &mut bytes_copied,
                        &cset_set,
                    ) {
                        *root = unsafe { ObjectRef::from_raw(new_ptr) };
                        work_list.push(new_ptr);
                    }
                }
            }
        }

        // Phase 1b: marking keep-alive (see `marking_keepalive_roots`).
        // While a concurrent mark cycle is active, CSet-resident gray/SATB
        // objects are snapshot-live: evacuate them like roots so the marker
        // can finish tracing them. The worklist remap at the end of this
        // pause then rewrites every gray through `pointer_map` instead of
        // dropping it (a drop = the object's unscanned subtree is silently
        // unmarked = cleanup frees live Old/humongous objects).
        // JIT round 23 wave 13, lane L9 (`CRATONVM_G1_KEEPALIVE_REGRAY_ONCE`,
        // opt-in): only the SATB-sourced keep-alive roots are re-grayed; a
        // root that came from the gray set is re-grayed by the end-of-pause
        // remap, which rewrites that very entry to the copy. Pushing it here
        // as well left TWO gray entries per object per pause, and the
        // next pause of the cycle found both in its CSet again: the gray set
        // DOUBLED per pause for every young object the starved marker had not
        // reached (`roots_us` 4.2 s + `other_us` 3.6 s of 26.3 s of young
        // pauses on `KeepLong 30`, n23-w12a).
        let (mut keepalive, satb_sourced) = self.marking_keepalive_roots_split(&regions, &cset_set);
        // g1pin: the peer-stack pinned objects are roots too, after the
        // SATB-sourced prefix (`append_peer_object_pin_roots`).
        self.append_peer_object_pin_roots(&mut keepalive, &cset_set);
        let regray_once = g1_keepalive_regray_once_enabled();
        // The re-grays, pushed under one lock after the loop
        // (`push_gray_or_mark_all`): every one names a to-space copy, whose
        // region nothing below retypes before the push.
        let mut regray: Vec<usize> = Vec::new();
        for (k, addr) in keepalive.into_iter().enumerate() {
            if !self.evacuation_supply_is_an_object(&regions, "marking-keepalive", addr) {
                continue;
            }
            if let Some((new_ptr, fresh)) = self.evacuate_object(
                &mut regions,
                addr as *mut u8,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &cset_set,
            ) {
                if fresh {
                    work_list.push(new_ptr);
                }
                if keepalive_needs_regray(regray_once, k, satb_sourced) {
                    regray.push(new_ptr as usize);
                }
            }
        }
        self.push_gray_or_mark_all(&regions, regray);

        phases.roots_us = phase_mark.elapsed().as_micros() as u64;
        phase_mark = std::time::Instant::now();

        // Phase 2: Scan remembered sets for references into CSet
        // (Collect rset sources before mutating regions)
        //
        // G1AUD-5 (defect G1-8): `live_rset_sources` drops entries whose source
        // region has been RECYCLED since the edge was recorded. The previous
        // "is the source Free right now?" filter (still applied inside
        // `scan_source_region_for_cset_refs`) misses precisely the case that
        // costs: a source that was freed and then re-typed into a live region
        // is not Free, so it was re-walked *wholesale* every pause on behalf of
        // an object that no longer exists — resurrecting that object's
        // referents, cycle after cycle.
        let mut rset_sources: RegionSet = Self::live_rset_sources(&regions, &cset);

        // CRIT fix (UAF): actually process the collected rset sources.
        // Dedup source indices so we walk each source region at most once.
        // JIT-pinned regions are scanned as sources too: their objects stay in
        // place (excluded from the CSet) but may reference objects that ARE in
        // the CSet, and JIT-compiled code may have installed those references
        // through stores the collector cannot assume went through
        // `post_write_barrier_rset`, so the pinned region is walked wholesale
        // as defense-in-depth. Without this, a list/tree straddling pinned and
        // CSet regions loses the CSet-side nodes (SIGSEGV / wrong checksum).
        //
        // NOTE (audited): JNI-pinned regions (`r.pinned`, GetPrimitiveArray-
        // Critical) do NOT need this treatment. Interpreter/native ref stores
        // all funnel through `post_write_barrier_rset`, which records EVERY
        // cross-region edge (young→young included — an earlier revision of
        // this comment claimed otherwise), and Phase 4's
        // `collect_outgoing_cross_region_edges` re-records edges the
        // evacuation itself rewrites. A CSet object whose only referent sits
        // in a JNI-pinned region is therefore reached via that region's RSet
        // membership — pinned regions are ordinary RSet sources. Regression:
        // `jni_pinned_young_region_holder_keeps_cset_referent_alive{,_parallel}`.
        // Deliberately NOT added wholesale here: an unconditional walk of
        // pinned regions would resurrect their dead objects' referents every
        // pause (the documented "undead" compounding) for no soundness gain.
        //
        // G1AUD-4 (2026-07-31, CORRECTED 2026-09-20 — lane B) — the "all ref
        // stores funnel through `post_write_barrier_rset`" premise above is
        // TRUE, without exception, for every store path under G1: the
        // interpreter's, every native/JIT-helper store, JIT-compiled stores
        // into OLD receivers, AND the JIT-compiled null->non-null `putfield`
        // into a YOUNG receiver that this paragraph used to except.
        //
        // The chain, re-verified against the code on 2026-09-20 (this is the
        // form the citation has to take, because the claim is about four
        // emitters in another crate): all four inline reference-store emitters
        // — `jit/src/x64/objects.rs:1512` and `:1640`,
        // `jit/src/x64/op_field.rs:1168` and `:1302` — gate the inline,
        // barrier-free store on `licm::region_bounds_are_live`, which reads the
        // CONTENT of the six-word `gen_heap::JIT_REGION_BOUNDS` table. The only
        // writer of that table's words is the GENERATIONAL heap's publication
        // (`gc/src/gen_heap.rs:3524`). Under G1 nothing publishes it, so it is
        // all-zero, `region_bounds_are_live` is false, and every one of those
        // emitters falls through to `emit_ref_putfield_helper_call` ->
        // `jit_putfield_object` -> `post_write_barrier_rset`. G1 additionally
        // refuses the inline barrier outright at `objects.rs:1512`/`:1640`
        // (`!g1 && ...`). `gen_heap.rs` itself records that the table's
        // emptiness under G1 is LOAD-BEARING for exactly this question.
        //
        // WHY THIS PARAGRAPH KEPT ITS HISTORY INSTEAD OF BEING DELETED. Defect
        // G1-2 was closed in `jit/` on 2026-08-13 and the audit row still said
        // "Not fixed" until 2026-08-18. For those five days `docs/GC.md` cited
        // G1-2 as the open blocker on narrowing the Phase-4 whole-heap walk, so
        // a live optimisation was being declined on a closed defect — and this
        // comment, which outlived the audit row by over a month, is what the
        // next reader of the narrowing would have found. The narrowing is now
        // landed (`phase4_regions_to_walk`); a reader who re-derives "the
        // remembered set is knowingly incomplete for a whole class of stores"
        // from a stale comment would revert it. Hence: corrected in place, with
        // the dates, rather than silently dropped.
        //
        // TWO CONSEQUENCES, deliberately NOT acted on here (they are behaviour,
        // and this correction is not):
        //
        //  * the JIT-pinned regions added to the source set below still call
        //    themselves "defense-in-depth against compiled stores that might
        //    have skipped the barrier". On the corrected premise that
        //    justification is empty — and `card_screen_for_source` has already
        //    stopped believing it, since `g1_card_screen_jit_pinned` defaults
        //    ON. Whoever removes the wholesale walk owns the measurement.
        //  * the `NOTE (audited)` paragraph just above declines to add
        //    JNI-pinned regions to the SOURCE set. That conclusion survives the
        //    correction untouched, because it never rested on the barrier claim
        //    — it rests on the "undead" cost of a wholesale source walk. It is
        //    also not in tension with `phase4_regions_to_walk` adding JNI-pinned
        //    regions to the FIX-UP walk set: Phase 4 rewrites slots and
        //    re-derives edges, it does not RETAIN anything, so the undead
        //    objection does not reach it. Read the two decisions together.
        //
        // `verify_no_dangling_into_cset` below is still the tripwire, but it is
        // now guarding the narrowing rather than a known-open barrier hole; see
        // `CsetSlotVerdict::StaleForward`.

        let dbg_phases = gc_flags().g1_dbg_reach;
        let p1_forwards = pointer_map.len();
        rset_sources.extend(jit_pinned_regions.iter());
        let unique_sources = rset_sources;
        let rset_sources_scanned = unique_sources.len();
        // G1AUD-11: the narrow Phase-4 set needs these after the walk below
        // consumes them.
        let narrow_sources = unique_sources.clone();
        if dbg_phases {
            eprintln!(
                "[g1][PHASES] cycle={} roots={} p1_forwards={p1_forwards} sources={:?}",
                self.collection_count.load(Ordering::Relaxed),
                roots.len(),
                unique_sources
            );
        }
        // F-05 — the card screen is applied to the remembered set's own
        // sources and NOT to the JIT-pinned regions added above; see the
        // `card_screen` parameter's doc for why the defence-in-depth walk must
        // stay wholesale.
        let card_screen_snapshot = self.card_scan_snapshot();
        for src_idx in unique_sources {
            self.scan_source_region_for_cset_refs(
                &mut regions,
                src_idx,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
                self.card_screen_for_source(&jit_pinned_regions, src_idx),
                precise_rset_liveness
                    .as_ref()
                    .filter(|_| !jit_pinned_regions.contains(&src_idx)),
            );
        }
        self.fill_card_scan_phases(&mut phases, card_screen_snapshot);
        if dbg_phases {
            eprintln!(
                "[g1][PHASES] after-sources forwards={} (delta={})",
                pointer_map.len(),
                pointer_map.len() - p1_forwards
            );
        }

        phases.rset_us = phase_mark.elapsed().as_micros() as u64;
        phase_mark = std::time::Instant::now();

        // Phase 3: Cheney-style scan of evacuated objects
        // JIT round 23 wave 8, lane L9: the per-pause serial evacuation
        // knobs (`CRATONVM_G1_SERIAL_EVAC_SIZE_MEMO`, `CRATONVM_G1_EVAC_SHAPE_KEY`).
        let mut evac_knobs = SerialEvacKnobs::new_before_retire();
        let mut scan_idx = 0;
        while scan_idx < work_list.len() {
            let obj_ptr = work_list[scan_idx];
            scan_idx += 1;

            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            self.scan_and_evacuate_refs_tuned(
                &mut regions,
                obj_ptr,
                header,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
                &mut evac_knobs,
            );
        }

        // Phase 3c: the collection-overlay edges (`g1_overlay.rs`) — rows of
        // owners outside the CSet are roots, forwarded CSet owners are followed
        // to a fixpoint. After the closure, so "forwarded" means "live".
        let mut overlay = self.overlay_pause_begin(
            &mut regions,
            &cset_set,
            &mut pointer_map,
            &mut objects_copied,
            &mut bytes_copied,
            self.overlay_prepare(&cset_set, true),
        );

        // Phase 3.5: finalizer resurrection (see
        // `collect_garbage_with_finalizers`). Must run after the closure is
        // complete (so "not forwarded" == dead) and before Phase 5 resets
        // the CSet regions (after that, the bytes are gone).
        self.resurrect_dead_finalizers(
            &mut regions,
            &cset_set,
            &mut pointer_map,
            &mut objects_copied,
            &mut bytes_copied,
            &mut work_list,
        );
        // A resurrected object can own overlay rows (or reach an owner).
        self.overlay_pause_follow(
            &mut overlay,
            &mut regions,
            &cset_set,
            &mut pointer_map,
            &mut objects_copied,
            &mut bytes_copied,
        );
        self.overlay_pause_finish(overlay, &regions, &pointer_map);

        phases.closure_us = phase_mark.elapsed().as_micros() as u64;
        phase_mark = std::time::Instant::now();

        // Phase 4: Update forwarding pointers in non-CSet regions
        let narrow = self.phase4_regions_to_walk(&regions, Some(&pre_evac), &narrow_sources);
        // gci i15/rs: opt-in -- the unchanged members are walked by young card.
        let young_screen =
            self.phase4_young_card_screen_set(&regions, &pre_evac, &cset_set, narrow.as_ref());
        let census = self.update_references_in_regions_screened(
            &mut regions,
            &cset_set,
            &pointer_map,
            narrow.as_ref(),
            young_screen.as_ref(),
        );
        phases.fixup_us = phase_mark.elapsed().as_micros() as u64;
        phases.fixup_regions = census.walked_regions;
        phases.fixup_bytes = census.walked_bytes;
        phase_mark = std::time::Instant::now();

        // Phase 5: Free evacuated regions
        // F-02: retire the from-space forwards this pause installed, before
        // Phase 5 decides which regions to free and which to keep. After the
        // fix-up (which resolves them), before the reclaim (which is what makes
        // a kept region's stale forward outlive the pause). See
        // `retire_forwards`.
        self.retire_forwards(&pointer_map, &cset_set);
        self.report_pending_corrupt_holders(&regions);
        let _ = self.count_unretired_forwards(&regions, &cset_set, &pointer_map);
        let bytes_freed = self.free_or_keep_cset(&mut regions, &cset, &pointer_map);
        self.record_kept_young_regions_from_pre(&regions, &pre_evac);

        // Humongous spans are never evacuated, so this is the only point in a
        // young/mixed pause that can free one. It must come after Phase 5: the
        // census it consumes is only a death certificate once the CSet is gone
        // and the pause's to-space has been walked. See
        // `eager_reclaim_humongous_locked` for the full ordering argument.
        let bytes_freed = bytes_freed
            + self.eager_reclaim_humongous_locked(
                &mut regions,
                roots,
                &pointer_map,
                &census,
                &jit_pinned_regions,
            );

        // SECURITY FIX (V7b): after the CSet is freed, scan survivors for
        // any slot still pointing into a freed CSet region with no
        // forwarding entry (incomplete remembered set => UAF). No-op on
        // the release/quiet path; aborts in debug.
        phases.free_us = phase_mark.elapsed().as_micros() as u64;
        // The census, while the guard is still held -- see the field docs.
        phases.record_region_census(&regions);
        phase_mark = std::time::Instant::now();
        self.verify_no_dangling_into_cset(&regions, &cset_set, &pointer_map, roots);
        self.dbg_verify_rset_completeness(&regions, "young-serial");
        // Env-gated diagnostics (no-ops unless CRATONVM_G1_DBG_HEADERS /
        // CRATONVM_G1_DBG_ZERO are set) — same coverage the parallel path has,
        // so serial-path corruption is also caught at the collection that
        // introduces it.
        self.dbg_verify_no_unrewritten_forward(&regions, &cset_set, &pointer_map, roots);
        self.dbg_scan_for_zeroed_refs(&regions, &cset_set, roots);
        self.dbg_verify_reachable_integrity(&regions, roots, "young-serial");
        // F-07: the budgeted V7b sweep runs in RELEASE and used to be charged
        // to no phase at all. See `G1PausePhases::verify_us`.
        phases.verify_us = phase_mark.elapsed().as_micros() as u64;

        // Young and mixed evacuation leave humongous spans IN PLACE — they are
        // never evacuated — but they are no longer left ALIVE unconditionally:
        // `eager_reclaim_humongous_locked` above frees any span this pause can
        // prove nothing references. Anything it declines to judge falls through
        // to the concurrent-mark cleanup phase as before.

        // Reset current eden if it was in the CSet
        self.retire_eden_slots_in_cset(&cset_set);

        // Update old gen bytes tracking.
        // Relaxed ordering: this is a statistics counter read only by IHOP heuristics;
        // exact inter-thread visibility ordering is not required.
        self.recompute_old_gen_bytes(&regions);
        self.publish_occupancy_metrics(&regions);

        // Remap monitors
        monitors.remap_after_gc(&pointer_map);
        // INT-8: carry the referent-slot skip set across this pause
        // (re-key survivors, prune CSet casualties) — same protocol
        // point as the monitor-table remap. No-op outside a mark cycle.
        self.remap_reference_skip_set(&cset_set, &pointer_map);
        // The overlay mark snapshot, by the same protocol (`g1_overlay.rs`).
        self.remap_overlay_snapshot(&regions, &cset_set, &pointer_map, true);
        // G1CORE-6: same protocol point, same reason — the dedup table's
        // canonical addresses go stale on exactly the objects this pause moved.
        self.remap_string_dedup_table(&cset_set, &pointer_map);

        // Round-7 fix (HIGH, audit §12): the concurrent mark worklist holds
        // raw object addresses that may have just been evacuated by this
        // young STW. The marker, after we unpark mutators, would otherwise
        // dereference stale pointers into freed regions → UAF. Walk the
        // worklist and apply the same `pointer_map` we applied to roots
        // and remembered-set updates.
        //
        // With the Phase-1b marking keep-alive, every CSet-resident gray was
        // evacuated above and therefore has a forwarding entry — the drop arm
        // below is defensive only. (It used to fire for grays the evacuation
        // closure did not reach, on the reasoning "unreached ⇒ dead ⇒ safe to
        // drop" — unsound under SATB: a gray unreachable at PAUSE time was
        // still live at MARK START, and dropping it unmarked its whole
        // unscanned subtree, which can include Old/humongous objects that are
        // very much live at cleanup. That was the marking-soundness defect
        // behind SteadyChurn's freed-live-Old-region failure.)
        //
        // Done under STW (still holding `regions.lock()`), so no marker
        // thread can be reading/writing `mark_worklist` concurrently — the
        // concurrent marker takes the same lock for each step.
        self.remap_gray_set_after_pause(&regions, &cset_set, &pointer_map);

        let pause_us = start.elapsed().as_micros() as u64;
        phases.close(pause_us);
        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };
        self.record_collection_with_phases(G1CollectionType::YoungOnly, pause_us, &stats, phases);
        cratonvm_gc_core::gc_metrics::record_g1_cycle(
            cratonvm_gc_core::gc_metrics::g1_cycle_kind::YOUNG,
            cset.len() as u32,
            0,
            (jni_pinned_out + jit_pinned_out) as u32,
            rset_sources_scanned as u32,
            self.g1_pause_degraded_flags_for(&pointer_map, jni_pinned_out, jit_pinned_out, false),
        );

        // JIT round 23 wave 8, lane L9: the next serial pause's pre-size.
        self.note_serial_forwards(pointer_map.len());
        GcResult { stats, pointer_map }
    }

    /// Phase H (RH.8) test hook — invoke `f` with mutable access to
    /// the region table.  **Intended for integration tests only.**
    /// External callers must assume this method will be removed if a
    /// safer API replaces it; it exists to let Phase H tests set up
    /// synthetic `gc_efficiency` values on old regions without having
    /// to run a full marking cycle.
    ///
    /// Holding the internal region lock across the closure guarantees
    /// no concurrent GC work observes a partially-mutated table.
    #[doc(hidden)]
    pub fn with_regions_mut<F: FnOnce(&mut [G1Region])>(&self, f: F) {
        let mut guard = self.regions.write();
        f(&mut guard);
        // The closure is allowed to retype regions, and the Free-region count
        // (G1AUD-9: and the young-region count) are the inputs `needs_gc`'s
        // caches depend on. Republish them here so this — the sanctioned
        // "mutate the region table" API — leaves the caches EXACT instead of
        // leaning on the backstop re-scan. See
        // `G1Collector::free_region_count`.
        self.publish_region_census(&guard);
    }

    /// Phase H (RH.8) — select the old regions that will be evacuated
    /// in the next mixed collection, ordered by expected benefit.
    ///
    /// Selection policy:
    /// 1. **Eligible:** region must be of [`RegionType::Old`] and not
    ///    [`G1Region::pinned`] (JNI critical sections may pin regions).
    /// 2. **Rank:** ascending [`G1Region::gc_efficiency`] (= lowest
    ///    live-bytes ratio wins) — this is HotSpot's "worst-first"
    ///    heuristic: the region with the most reclaimable garbage is
    ///    selected first, maximising memory freed per byte copied.
    /// 3. **Cap:** at most `old_cset_region_threshold_percent` of all
    ///    regions (defaults to 10%), with a minimum of one region so
    ///    a tiny heap still makes progress.
    /// 4. **Pause-target cap (Step 7):** on top of the percentage cap, stop
    ///    adding old regions once their estimated copy time (rolling
    ///    `evac_ns_per_byte` × `live_bytes`) would exceed `max_gc_pause_ms`,
    ///    always keeping ≥1 region. Deferred regions are reclaimed in a later
    ///    mixed cycle.
    /// 5. **Deterministic:** ties broken by ascending region index.
    ///
    /// This helper is exposed publicly so tests and external policy
    /// hooks can inspect the selection without running a full mixed
    /// GC cycle.  The real cycle duplicates the logic inline to avoid
    /// releasing its region-table lock guard.
    pub fn select_old_regions_for_mixed_gc(&self) -> Vec<usize> {
        self.select_old_regions_for_mixed_gc_within(g1_pause_cost_model())
    }

    /// [`Self::select_old_regions_for_mixed_gc`] with the cost model as a
    /// PARAMETER, so both arms are reachable from a test regardless of what the
    /// ambient environment latched `CRATONVM_G1_PAUSE_COST_MODEL` to.
    pub fn select_old_regions_for_mixed_gc_within(&self, marginal_fixup: bool) -> Vec<usize> {
        // gci i11/gm finding 3 -- a thin read-only view of the ONE selector
        // the production pauses call ([`Self::choose_old_cset`]). No exclusion
        // set (a test has no conservative JIT roots) and no census.
        let regions = self.regions.read();
        self.choose_old_cset(&regions, &RegionSet::new(), OldCsetSource::Mixed, marginal_fixup)
            .picked
    }

    /// gci i11/gm finding 3 -- THE old-collection-set selector.
    ///
    /// There used to be three: this function's test-only predecessor (the one
    /// that priced the marginal fix-up walk under `CRATONVM_G1_PAUSE_COST_MODEL`)
    /// and two hand copies inline in [`Self::mixed_collection`] and
    /// [`Self::mixed_collection_parallel`], which never charged the fix-up and
    /// disagreed with each other on the census. Every pause and every test now
    /// selects here; each switch keeps its arm:
    ///
    /// * `CRATONVM_G1_MIXED_HOTSPOT_CHOICE` (gci i10/rh): live estimate, rank,
    ///   `max_old` rounding and the per-pause minimum `mixed_min_old`;
    /// * `CRATONVM_G1_PAUSE_COST_MODEL` (`marginal_fixup`): the budget charges
    ///   the young half and each candidate pays the marginal fix-up walk it
    ///   causes (one to-space region plus its not-yet-charged rset sources);
    /// * a full compaction (gcd d10/o): its own candidates, uncapped, no budget,
    ///   no minimum;
    /// * an allocation-failure compaction plan: the plan's sources ARE the old
    ///   half, so nothing is chosen here.
    ///
    /// Selection policy (the Phase H / Step 7 rules, unchanged on the default
    /// arm): eligible = Old, not JNI-pinned, not in `exclude`, carries mark
    /// data (`live_bytes > 0`: a region promoted into after cleanup carries the
    /// `reset()` defaults and would sort first at a zero price), below the live
    /// threshold; ranked lowest-live first, index tiebreak; truncated at
    /// `max_old`; then at the copy budget once the minimum is taken (at least
    /// one region for forward progress). The take is a prefix of the ranking,
    /// so pricing the first `max_old` candidates up front is the same
    /// arithmetic the old incremental loops did.
    ///
    /// Takes the region table by slice: every caller already holds a guard.
    pub fn choose_old_cset(
        &self,
        regions: &[G1Region],
        exclude: &RegionSet,
        source: OldCsetSource,
        marginal_fixup: bool,
    ) -> OldCsetChoice {
        let region_size = self.config.region_size;
        let hotspot = self.mixed_hotspot_choice;
        let mut excluded = 0usize;
        let (candidates, full) = match source {
            OldCsetSource::Plan => (Vec::new(), false),
            OldCsetSource::FullCompaction { young_bytes } => (
                self.full_compaction_candidates(regions, exclude, young_bytes),
                true,
            ),
            OldCsetSource::Mixed => {
                let threshold = self.config.mixed_gc_live_threshold_percent as usize;
                let mut c: Vec<(usize, f64)> = regions
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| {
                        r.region_type() == RegionType::Old
                            && !r.pinned
                            && r.live_bytes > 0
                            // Item 3 — the live threshold; see `mixed_phase_has_work`.
                            && r.mixed_live(hotspot) * 100 < region_size * threshold
                    })
                    .filter(|(i, _)| {
                        let out = exclude.contains(i);
                        excluded += out as usize;
                        !out
                    })
                    .map(|(i, r)| (i, r.mixed_rank(hotspot, region_size)))
                    .collect();
                // Stable sort so that among regions with identical efficiency
                // the lower index wins — keeps selection deterministic.
                c.sort_by(|a, b| {
                    a.1.partial_cmp(&b.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.0.cmp(&b.0))
                });
                (c, false)
            }
        };
        let (max_old, budget_ns) = if full {
            (usize::MAX, u64::MAX)
        } else {
            let cap_percent = self.config.old_cset_region_threshold_percent;
            let cap = if hotspot {
                mixed_choice::max_old_regions(regions.len(), cap_percent)
            } else {
                ((regions.len() * cap_percent as usize) / 100).max(1)
            };
            // Ten-findings item 10 / LANE E wave 2: the pause goal minus the
            // fix-up EMA, minus the young half under the cost model.
            (cap, self.old_cset_copy_budget_ns_within(marginal_fixup))
        };
        // gci i10/rh finding 2 -- HotSpot takes `min_old` whatever the budget says.
        let min_floor = if hotspot && !full {
            self.mixed_min_old.load(Ordering::Relaxed).max(1)
        } else {
            1
        };
        // LANE W5-A — one read site for the old-copy price, which applies the
        // young-derived bootstrap when no mixed pause has ever measured one.
        // See [`Self::old_evac_ns_per_byte`].
        let ns_per_byte = self.old_evac_ns_per_byte();
        // An uncapped, unbudgeted full compaction never reads a fix-up price.
        let fixup_per_region = if marginal_fixup && !full {
            self.fixup_ns_per_region()
        } else {
            0
        };
        // LANE E wave 2 — the MARGINAL fix-up price, and the walk set that
        // makes it marginal.
        //
        // The pre-wave-2 loop solved `Σ copy(cset) < goal - fixup(PREVIOUS
        // cset)`, with the right-hand side constant while the left-hand side
        // grew. But the walk's cost is not constant with respect to this
        // choice: `phase4_regions_to_walk` narrows the walk to the CHANGED
        // regions plus the remembered-set SOURCES, so every old region added
        // to the collection set adds its to-space destination to the changed
        // set AND pulls that region's rset sources into the walk. The model
        // therefore under-estimated in the direction that overruns, which is
        // the direction a pause goal cannot tolerate.
        //
        // `charged` is what makes this a marginal price rather than a second
        // fixed one: a source region named by two candidates is walked once, so
        // it is charged once. Seeded with the candidate indices themselves as
        // they are taken, because a CSet region's to-space destination is a
        // changed region the walk covers either way.
        //
        // gci i11/gm: priced up front for the first `max_old` candidates. The
        // take below is always a PREFIX of the ranking, so candidate k's price
        // with candidates 0..k charged is exactly what the old incremental
        // loop computed when it reached k.
        let available = candidates.len();
        let limit = available.min(max_old);
        let mut charged: FxHashSet<usize> = FxHashSet::default();
        let mut costs: Vec<u64> = Vec::with_capacity(limit);
        for &(i, _) in candidates.iter().take(limit) {
            let mut cost = regions[i].mixed_evac_cost_ns(ns_per_byte, hotspot);
            if fixup_per_region > 0 {
                // One region for the to-space destination this candidate's
                // live data lands in, plus every rset source of this candidate
                // that no already-selected candidate has paid for.
                let mut new_walk_regions = 1u64;
                for src in regions[i].rset.sources() {
                    if src != i && charged.insert(src) {
                        new_walk_regions += 1;
                    }
                }
                cost = cost.saturating_add(fixup_per_region.saturating_mul(new_walk_regions));
            }
            charged.insert(i);
            costs.push(cost);
        }
        let (taken, budget_capped) =
            mixed_choice::old_cset_take(&costs, min_floor, max_old, budget_ns);
        OldCsetChoice {
            picked: candidates[..taken].iter().map(|&(i, _)| i).collect(),
            available,
            max_old,
            budget_capped,
            fixup_charged: fixup_per_region > 0 && taken > 0,
            excluded,
        }
    }

    /// gci i11/gm finding 3 -- the `[GC] g1 mixed-census:` counters for one
    /// production old-CSet choice, counted in ONE place for both mixed drivers
    /// (the parallel driver used to count none of them). `plan_sources` is the
    /// allocation-failure compaction plan's old half, which the serial driver
    /// always counted as selected.
    pub fn note_old_cset_census(&self, choice: &OldCsetChoice, plan_sources: usize) {
        if choice.budget_capped {
            self.mixed_cset_capped_budget
                .fetch_add(1, Ordering::Relaxed);
        } else if choice.available > choice.max_old {
            self.mixed_cset_capped_percent
                .fetch_add(1, Ordering::Relaxed);
        }
        self.mixed_old_regions_selected
            .fetch_add((choice.picked.len() + plan_sources) as u64, Ordering::Relaxed);
        if choice.fixup_charged {
            self.mixed_fixup_charged_pauses
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The tail of both mixed drivers: spend one of the mixed phase's
    /// `mixed_gc_remaining` pauses and close the phase on the last one.
    ///
    /// gci i11/gm finding 8 -- `compaction` is a `System.gc()` full compaction
    /// or an allocation-failure compaction plan. Both are routed through the
    /// mixed drivers but neither is a mixed pause (their old half is not the
    /// mixed choice), so under `CRATONVM_G1_MIXED_COUNT_SKIPS_COMPACTION` they
    /// leave the count alone; HotSpot's full GC does not shorten a mixed phase
    /// by one either. Default arm: spends one, as before. Either way a
    /// compaction inside a mixed phase is counted for the census.
    ///
    /// Relaxed ordering: `mixed_gc_remaining` and `marking_complete` are
    /// GC-internal scheduling counters only accessed during STW pauses.
    pub fn spend_mixed_count(&self, compaction: bool) {
        let remaining = self.mixed_gc_remaining.load(Ordering::Relaxed);
        if remaining == 0 {
            return;
        }
        if compaction {
            self.mixed_count_compaction_pauses
                .fetch_add(1, Ordering::Relaxed);
            if self.mixed_count_skips_compaction {
                return;
            }
        }
        self.mixed_gc_remaining
            .store(remaining - 1, Ordering::Relaxed);
        if remaining - 1 == 0 {
            self.marking_complete.store(false, Ordering::Relaxed);
            // gci i11/go (finding 9): the phase is over; no `Prepare Mixed`
            // may outlive it.
            self.prepare_mixed_pending.store(false, Ordering::Relaxed);
            // gci i18/mx: and the next phase's first breaker poll is a
            // fresh trial (`mixed_futile_breaker_allows_within`).
            self.mixed_futile_phase_polled.store(false, Ordering::Relaxed);
        }
    }

    /// Move one humongous span for the allocation-failure compaction
    /// ([`HumongousMove`], planned by [`Self::plan_humongous_compaction`]):
    /// claim the Free destination run, copy the object, carry the start
    /// region's liveness, mark bit and remembered set over, and forward the
    /// source header to the copy. The caller adds the source span to the CSet
    /// and the forward to the pause's `PointerMap`, so every reference is
    /// resolved by the same machinery as an evacuated object's and Phase 5
    /// frees the source regions.
    ///
    /// Returns `(old_base, new_base, size)`, or `None` (nothing changed) when
    /// the plan no longer matches the table: the source is not a well-formed
    /// span of `len` regions, the destination is not all Free, the two
    /// overlap, the object is already forwarded, or the destination cannot be
    /// committed.
    pub fn relocate_humongous_span(
        &self,
        regions: &mut [G1Region],
        mv: HumongousMove,
    ) -> Option<(usize, usize, usize)> {
        let HumongousMove {
            src_start,
            len,
            dst_start,
        } = mv;
        let region_size = self.config.region_size;
        if len == 0
            || region_size == 0
            || src_start + len > regions.len()
            || dst_start + len > regions.len()
            || (dst_start < src_start + len && src_start < dst_start + len)
        {
            return None;
        }
        if regions[src_start].region_type() != RegionType::HumongousStart
            || !regions[src_start + 1..src_start + len]
                .iter()
                .all(|r| r.region_type() == RegionType::HumongousContinuation)
            || !regions[dst_start..dst_start + len]
                .iter()
                .all(|r| r.region_type() == RegionType::Free)
        {
            return None;
        }
        let size = regions[src_start].cursor();
        if size < HEADER_SIZE || size.div_ceil(region_size).max(1) != len {
            return None;
        }
        let old = regions[src_start].data.as_ptr() as usize;
        let new = regions[dst_start].data.as_ptr() as usize;
        // SAFETY: `old` is the start of a well-formed humongous span (checked
        // above) and the pause holds the regions guard as a writer.
        let observed = unsafe {
            (*(old as *const ObjectHeader))
                .mark_word
                .load(Ordering::Acquire)
        };
        if ObjectHeader::is_forwarded_mark(observed) {
            return None;
        }
        if !self.commit_through_region(dst_start + len - 1) {
            return None;
        }
        regions[dst_start].set_region_type(RegionType::HumongousStart);
        regions[dst_start].set_cursor(0);
        for r in &mut regions[dst_start + 1..dst_start + len] {
            r.set_region_type(RegionType::HumongousContinuation);
            r.set_cursor(0);
        }
        // SAFETY: the destination run is `len` committed, adjacent regions of
        // the contiguous arena (`claim_humongous_span_locked`'s layout), so it
        // holds `size` bytes; it does not overlap the source (checked above).
        unsafe {
            std::ptr::copy_nonoverlapping(old as *const u8, new as *mut u8, size);
            (*(new as *const ObjectHeader))
                .mark_word
                .store(observed, Ordering::Relaxed);
        }
        regions[dst_start].set_cursor(size);
        regions[dst_start].live_bytes = regions[src_start].live_bytes;
        regions[dst_start].gc_efficiency = regions[src_start].gc_efficiency;
        if regions[src_start].mark_bitmap.is_marked(old) {
            regions[dst_start].mark_bitmap.try_mark(new);
        }
        let coarsened = regions[src_start].rset.is_coarsened();
        let sources = regions[src_start].rset.sources_with_generations();
        if coarsened {
            regions[dst_start].rset.coarsen();
        }
        for (source, generation) in sources {
            regions[dst_start]
                .rset
                .add_reference_in_generation(source, generation);
        }
        self.note_free_regions_claimed(len, 0);
        // SAFETY: the copy above is complete; the forward makes it findable.
        unsafe { (*(old as *const ObjectHeader)).set_forwarding_address(new as *mut u8) };
        HUMONGOUS_RELOCATED.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            src_start,
            dst_start,
            regions = len,
            size,
            "g1 allocation-failure compaction: relocated a humongous span out of the target window"
        );
        Some((old, new, size))
    }

    /// Pick the least disruptive window that can become the failed humongous
    /// span.  Every non-Free region in the window becomes a CSet source and
    /// destinations are restricted to the complement.  The conservative
    /// region-count capacity check is intentionally stronger than a live-byte
    /// estimate: even a stale mark estimate cannot make the plan overwrite a
    /// live source.
    pub fn plan_humongous_compaction(
        &self,
        regions: &[G1Region],
        roots: &[ObjectRef],
        target_regions: usize,
    ) -> Option<HumongousCompactionPlan> {
        if target_regions <= 1 || target_regions > regions.len() {
            return None;
        }
        let pinned = self.pinned_region_set_including_non_object_roots(regions, roots);
        let total_free = regions
            .iter()
            .filter(|r| r.region_type() == RegionType::Free)
            .count();
        let mut best: Option<((usize, usize, usize, usize), HumongousCompactionPlan)> = None;
        let move_humongous = true;
        let region_size = self.config.region_size.max(1);
        let mut blocked_by_pin = 0usize;
        let mut blocked_by_unmovable = 0usize;
        let mut blocked_by_unknown_mark = 0usize;
        let mut blocked_by_capacity = 0usize;
        let staged_old = self.compaction_staged_old.lock().clone();
        // Windows whose only obstacle was a humongous destination run:
        // `(window start, spans, Old sources, Free regions inside)`.
        let mut staging: Vec<(usize, Vec<(usize, usize)>, usize, usize)> = Vec::new();
        for start in 0..=regions.len() - target_regions {
            let end = start + target_regions;
            let mut sources = Vec::new();
            let mut young_sources = 0usize;
            let mut copied_bytes = 0usize;
            let mut free_inside = 0usize;
            let mut possible = true;
            // Humongous spans the window touches, `(span_start, span_len)`.
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (idx, region) in regions[start..end].iter().enumerate() {
                let idx = start + idx;
                match region.region_type() {
                    RegionType::Free => free_inside += 1,
                    RegionType::HumongousStart | RegionType::HumongousContinuation
                        if move_humongous =>
                    {
                        let Some(span_start) = humongous_span_start(regions, idx) else {
                            blocked_by_unmovable += 1;
                            possible = false;
                            break;
                        };
                        if spans.iter().any(|&(s, _)| s == span_start) {
                            continue;
                        }
                        let len = regions[span_start].cursor().div_ceil(region_size).max(1);
                        let span_end = span_start + len;
                        let well_formed = span_end <= regions.len()
                            && regions[span_start].cursor() >= HEADER_SIZE
                            && regions[span_start + 1..span_end]
                                .iter()
                                .all(|r| r.region_type() == RegionType::HumongousContinuation);
                        if !well_formed {
                            blocked_by_unmovable += 1;
                            possible = false;
                            break;
                        }
                        if (span_start..span_end).any(|i| regions[i].pinned || pinned.contains(&i))
                        {
                            blocked_by_pin += 1;
                            possible = false;
                            break;
                        }
                        spans.push((span_start, len));
                        copied_bytes = copied_bytes.saturating_add(regions[span_start].cursor());
                    }
                    RegionType::Eden | RegionType::Survivor => {
                        if region.pinned || pinned.contains(&idx) {
                            blocked_by_pin += 1;
                            possible = false;
                            break;
                        }
                        // `mixed_collection` always puts the ordinary young
                        // CSet through its normal young evacuation protocol.
                        // This target planner names only OLD blockers; adding
                        // a young member here would replace that protocol with
                        // the old-only special set and loses its survivor-age
                        // accounting.
                        young_sources += 1;
                        copied_bytes = copied_bytes.saturating_add(region.cursor());
                    }
                    RegionType::Old => {
                        // A completed mark cycle is required before this plan
                        // is armed.  `live_bytes == 0` here means either that
                        // cleanup should already have freed it, or that it has
                        // no trustworthy mark data; neither is safe to move.
                        if region.pinned || pinned.contains(&idx) {
                            blocked_by_pin += 1;
                            possible = false;
                            break;
                        }
                        if region.live_bytes == 0 && !staged_old.contains(&idx) {
                            blocked_by_unknown_mark += 1;
                            possible = false;
                            break;
                        }
                        sources.push(idx);
                        copied_bytes = copied_bytes.saturating_add(if region.live_bytes == 0 {
                            region.cursor()
                        } else {
                            region.live_bytes
                        });
                    }
                    // A humongous span cannot move through the regional
                    // evacuator, and every remaining type is deliberately
                    // non-movable for this one-pause plan.
                    _ => {
                        blocked_by_unmovable += 1;
                        possible = false;
                        break;
                    }
                }
            }
            if !possible || (sources.is_empty() && young_sources == 0 && spans.is_empty()) {
                continue;
            }
            let free_outside = total_free.saturating_sub(free_inside);
            // Each span needs one Free run of its own length outside the
            // window, disjoint from the other spans' destinations; the Old
            // sources then need one Free region each among what is left.
            let mut humongous_moves = Vec::new();
            let mut moved_regions = 0usize;
            if !spans.is_empty() {
                let mut avail: Vec<bool> = regions
                    .iter()
                    .enumerate()
                    .map(|(i, r)| r.region_type() == RegionType::Free && !(start..end).contains(&i))
                    .collect();
                let mut by_len = spans.clone();
                by_len.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                for (src_start, len) in by_len {
                    let (found, _) = best_fit_run(avail.iter().copied(), len);
                    let Some(dst_start) = found else {
                        humongous_moves.clear();
                        break;
                    };
                    for slot in &mut avail[dst_start..dst_start + len] {
                        *slot = false;
                    }
                    moved_regions += len;
                    humongous_moves.push(HumongousMove {
                        src_start,
                        len,
                        dst_start,
                    });
                }
                if humongous_moves.len() != spans.len() {
                    blocked_by_capacity += 1;
                    staging.push((start, spans, sources.len(), free_inside));
                    continue;
                }
            }
            if free_outside < sources.len() + moved_regions {
                blocked_by_capacity += 1;
                continue;
            }
            let plan = HumongousCompactionPlan {
                requested_regions: target_regions,
                target_start: start,
                target_end: end,
                sources,
                young_sources,
                humongous_moves,
                staged_for: None,
            };
            // Fewest humongous regions moved first, then fewest Old sources,
            // then fewest bytes copied, then the lowest window.
            let key = (moved_regions, plan.sources.len(), copied_bytes, start);
            if best.as_ref().is_none_or(|current| key < current.0) {
                best = Some((key, plan));
            }
        }
        if best.is_none() && !staging.is_empty() {
            if let Some(plan) = self.plan_humongous_staging(
                regions,
                &pinned,
                &staged_old,
                total_free,
                target_regions,
                staging,
            ) {
                tracing::warn!(
                    target_regions,
                    blocked_by_capacity,
                    stage_start = plan.target_start,
                    stage_end = plan.target_end,
                    sources = plan.sources.len(),
                    young_sources = plan.young_sources,
                    "g1 allocation-failure compaction: no window has a destination run for its \
                     humongous span; this pause clears one first"
                );
                return Some(plan);
            }
        }
        match best {
            Some((_, plan)) => Some(plan),
            None => {
                tracing::warn!(
                    target_regions,
                    blocked_by_pin,
                    blocked_by_unmovable,
                    blocked_by_unknown_mark,
                    blocked_by_capacity,
                    map = %region_type_map(regions, &pinned),
                    "g1 allocation-failure compaction: no target window (blocker census)"
                );
                None
            }
        }
    }

    /// r16 G1 page: the staging half of the allocation-failure compaction.
    /// Every window of `target_regions` holds a live humongous span whose
    /// destination run does not exist: the Free regions outside the window
    /// are there, but fragmented by Old and young regions. Pick, for one of
    /// the cheapest such windows, a run of the span's length outside it whose
    /// non-Free members are all movable, and plan a pause that evacuates them
    /// (the run is the plan's target range, so to-space avoids it). The next
    /// recovery pause then plans the real window and moves the span into the
    /// run. Capacity: the run's blockers, plus the window's own Old sources
    /// for that next pause, fit in the Free regions outside both.
    pub fn plan_humongous_staging(
        &self,
        regions: &[G1Region],
        pinned: &RegionSet,
        staged_old: &[usize],
        total_free: usize,
        target_regions: usize,
        mut candidates: Vec<(usize, Vec<(usize, usize)>, usize, usize)>,
    ) -> Option<HumongousCompactionPlan> {
        if self.compaction_stages.load(Ordering::Acquire) >= HUMONGOUS_COMPACTION_MAX_STAGES {
            return None;
        }
        // Cheapest windows first: the least humongous data to move, then the
        // fewest Old sources. Only a few are tried; each try is O(regions * len).
        candidates.sort_by_key(|(start, spans, sources, _)| {
            (
                spans.iter().map(|&(_, len)| len).sum::<usize>(),
                *sources,
                *start,
            )
        });
        let mut best: Option<((usize, usize, usize), HumongousCompactionPlan)> = None;
        for (w_start, spans, w_sources, w_free) in candidates.into_iter().take(4) {
            let w_end = w_start + target_regions;
            // The longest span is the one whose run is missing; a shorter one
            // finds its run in the next pause's ordinary planning, or stages
            // again within the bound.
            let Some(&(_, len)) = spans
                .iter()
                .max_by_key(|&&(start, len)| (len, usize::MAX - start))
            else {
                continue;
            };
            if len > regions.len() {
                continue;
            }
            for r_start in 0..=regions.len() - len {
                let r_end = r_start + len;
                if r_start < w_end && r_end > w_start {
                    continue;
                }
                let mut old = Vec::new();
                let mut young = 0usize;
                let mut copied = 0usize;
                let mut free_in_run = 0usize;
                let mut movable = true;
                for idx in r_start..r_end {
                    let region = &regions[idx];
                    if region.pinned || pinned.contains(&idx) {
                        movable = false;
                        break;
                    }
                    match region.region_type() {
                        RegionType::Free => free_in_run += 1,
                        RegionType::Eden | RegionType::Survivor => {
                            young += 1;
                            copied = copied.saturating_add(region.cursor());
                        }
                        RegionType::Old if region.live_bytes > 0 => {
                            old.push(idx);
                            copied = copied.saturating_add(region.live_bytes);
                        }
                        RegionType::Old if staged_old.contains(&idx) => {
                            old.push(idx);
                            copied = copied.saturating_add(region.cursor());
                        }
                        _ => {
                            movable = false;
                            break;
                        }
                    }
                }
                if !movable || free_in_run == len {
                    continue;
                }
                let free_outside = total_free.saturating_sub(w_free + free_in_run);
                if free_outside < old.len() + young + w_sources {
                    continue;
                }
                let key = (old.len() + young, copied, r_start);
                if best.as_ref().is_none_or(|current| key < current.0) {
                    best = Some((
                        key,
                        HumongousCompactionPlan {
                            requested_regions: target_regions,
                            target_start: r_start,
                            target_end: r_end,
                            sources: old,
                            young_sources: young,
                            humongous_moves: Vec::new(),
                            staged_for: Some(target_regions),
                        },
                    ));
                }
            }
        }
        best.map(|(_, plan)| plan)
    }

    /// Whether an allocation-failure compaction request is armed and not yet
    /// consumed by a pause (a staging pause re-arms it).
    pub fn humongous_compaction_pending(&self) -> bool {
        self.pending_humongous_compaction.lock().is_some()
    }

    /// Perform a mixed collection. Evacuates young + selected old regions.
    pub fn mixed_collection(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        // `arm_fragmented_humongous_compaction` installs only the requested
        // span. Build the actual window under this pause's region state below.
        let requested_compaction = self
            .pending_humongous_compaction
            .lock()
            .take()
            .map(|plan| (plan.requested_regions, plan.target_end));
        // Mixed GC always uses the SERIAL evacuator, even under
        // `CRATONVM_G1_PARALLEL_EVAC`. The parallel evacuator still has an OPEN
        // correctness bug, so mixed (which reclaims old gen) stays on the
        // differentially-verified serial path.
        //
        // Two distinct parallel-evac defects exist:
        //  1. self-forward (evacuation-failure) UAF — FIXED on dev `7e97111e`
        //     (`parallel_evacuate` clears each self-forward's `forwarding_ptr`
        //     after merging the shards; the `key == value` loop).
        //  2. a RARE, NON-DETERMINISTIC race in the *young* parallel evacuator
        //     (`run_worker`/`evacuate`/`process_object`) that intermittently
        //     corrupts a live object under frequent young GCs at small heaps —
        //     reproduced on `SteadyChurn @16m --nojit` (~1 in 8 runs, smallest
        //     heap only; serial is always correct). This is NOT defect 1 (no
        //     to-space exhaustion) and is the tracked parallel-evac follow-up.
        //     Because it lives in the shared `parallel_evacuate` closure that
        //     `mixed_collection_parallel` also drives, mixed must not be routed
        //     to it until the race is fixed — even though `PromoteMixed` happened
        //     to pass at the larger heaps it was tested on.
        //
        // mixed GCs are infrequent, so serializing them costs little. The
        // parallel young path is unchanged (`young_collection` still dispatches
        // to it under the flag). `mixed_collection_parallel` is retained (and
        // unit-tested directly) for when defect 2 is resolved.
        //
        // 2026-09-20 — THE ASYMMETRY ABOVE IS NO LONGER SELF-CONSISTENT, and
        // this gate is the smallest change that makes it measurable rather
        // than just arguable.
        //
        // Defect 2 is described as living "in the shared `parallel_evacuate`
        // closure that `mixed_collection_parallel` also drives", and the
        // conclusion drawn from that was to keep mixed serial. But since
        // `CRATONVM_G1_PARALLEL_EVAC` was flipped to default-ON (and since F-01
        // dropped the `in_jit` veto), `young_collection` dispatches into that
        // same closure on EVERY production pause — thousands of times for each
        // mixed pause this refusal protects. So the refusal cannot be doing
        // what it says: if the race is live, it is live on the frequent path
        // and mixed is the wrong thing to be guarding; if it is not, mixed is
        // serial for no reason and old-generation reclamation — the phase that
        // copies the MOST bytes per pause — is the one phase that never uses
        // the worker pool.
        //
        // `CRATONVM_G1_PARALLEL_MIXED=1` is opt-IN, so production is
        // byte-for-byte what it is today unless someone asks. What it buys is
        // that `mixed_collection_parallel` stops being live code with no route
        // to it outside unit tests: until then no gauntlet arm and no soak
        // could exercise it at all, which is why "is defect 2 still real" had
        // stayed an open question rather than a measurement.
        //
        // 2026-09-21 — IT IS A MEASUREMENT NOW, AND THE ANSWER CHANGES WHICH
        // SENTENCE ABOVE IS LOAD-BEARING.
        //
        // Defect 2 did not reproduce in three independent passes, and this
        // gate still stays off — but not for the race any more. The lever
        // parallelises PHASE 3, and on the shape a mixed pause actually has
        // (an old live set reached through the remembered set) 100.0% of the
        // bytes the pause copies are copied in PHASE 2, by the driver thread
        // alone. It is inert by construction, so flipping it is not a
        // throughput decision that is merely unmeasured; it is one that has
        // been measured and comes back at zero. What to fix instead is named
        // in the W6-P page. See `parallel_mixed_enabled`,
        // `docs/feature-designs/concurrent-gc-maturation.md`
        // and
        // `docs/feature-designs/concurrent-gc-maturation.md`.
        // LANE W6-P — the ENGAGEMENT COUNT for this lever, recorded at the one
        // place that knows the answer.
        //
        // Every A/B this lever has been given so far compared two whole runs
        // and could not say whether either of them took a single mixed pause.
        // They did not: eight `G1OldBurstProbe` configurations plus the
        // standing battery produced ZERO mixed pauses in up to 8,945 young
        // ones. A difference measured over that is a difference between two
        // identical runs. This counter makes the denominator printable, so the
        // next person's A/B either shows a non-zero `parallel` arm or is
        // discarded before it is believed. See `g1_mixed_route_counts`.
        //
        // 2026-10-27 (g1ep5) -- default-ON: a mixed pause's YOUNG half is
        // copied here too, and on `G1epKeepRounds` it is nearly all of the
        // pause (see `parallel_mixed_enabled`).
        let parallel = requested_compaction.is_none()
            && parallel_mixed_enabled()
            && use_parallel_evacuator(
                parallel_evac_enabled(),
                cratonvm_gc_core::gc_quiescence::is_active(),
                true,
            );
        note_mixed_route(parallel);
        if parallel {
            // See the note at `young_collection`'s dispatch: the width is
            // recorded by `parallel_evacuate`, not by whoever routed to it.
            return self.mixed_collection_parallel(roots, monitors);
        }
        let start = std::time::Instant::now();
        let mut regions = self.regions.write();
        let compaction_plan = requested_compaction.and_then(|(requested, target)| {
            self.plan_humongous_compaction(&regions, roots, target)
                .map(|mut plan| {
                    plan.requested_regions = requested;
                    plan
                })
        });
        if let Some(plan) = &compaction_plan {
            tracing::warn!(
                requested = plan.requested_regions,
                target_regions = plan.target_end - plan.target_start,
                target_start = plan.target_start,
                target_end = plan.target_end,
                sources = plan.sources.len(),
                young_sources = plan.young_sources,
                humongous_moves = plan.humongous_moves.len(),
                "g1 allocation-failure compaction: evacuating target-window blockers"
            );
            *self.humongous_compaction_avoid.lock() = Some(HumongousCompactionReservation {
                start: plan.target_start,
                end: plan.target_end,
                requested_regions: plan.requested_regions,
            });
        } else if let Some((requested, target)) = requested_compaction {
            tracing::warn!(
                requested,
                target_regions = target,
                "g1 allocation-failure compaction: no movable target window after full mark"
            );
        }
        // SECURITY FIX (V7a): mixed GC resets/retypes CSet regions
        // (Phase 5). Invalidate every mutator's RSet fast-path cache
        // before any reclassification — see `rset_cache_epoch`.
        self.bump_rset_cache_epoch();
        // Item 1 -- a pause's span marks belong to that pause.
        self.clear_humongous_references();
        // G1AUD-9: a destination cursor from the previous pause names a region
        // this one may free, retype, or put in the CSet. Drop both before any
        // reclassification so the first placement of each type does not start
        // from a hint that is guaranteed to miss.
        self.reset_evac_dest_hints();
        // JIT round 23 wave 8, lane L9 (`CRATONVM_G1_SERIAL_FORWARD_PRESIZE`).
        let mut pointer_map: cratonvm_types::PointerMap = self.serial_pointer_map();
        let mut objects_copied = 0usize;
        let mut bytes_copied = 0usize;

        // Exclude regions holding a conservative JIT root from the CSet (pin in
        // place) — see `young_collection` / `jit_pinned_region_set`. A mixed GC
        // can also select the (now promoted) region of a long-lived JIT-rooted
        // object, so this guard matters for both young and old CSet members.
        let jit_pinned_regions = self.pause_pinned_region_set(&regions, roots);

        // Build CSet: all young regions + worst old regions (skip pinned + any
        // region holding a conservative JIT root)
        let mut cset: Vec<usize> = compaction_plan
            .as_ref()
            .map(|plan| plan.sources.clone())
            .unwrap_or_else(|| {
                regions
                    .iter()
                    .enumerate()
                    .filter(|(i, r)| {
                        !r.pinned
                            && !jit_pinned_regions.contains(i)
                            && (r.region_type() == RegionType::Eden
                                || r.region_type() == RegionType::Survivor)
                    })
                    .map(|(i, _)| i)
                    .collect()
            });
        // A target-window compaction changes only which OLD blockers join the
        // mixed CSet.  Young regions must still take the normal mixed/young
        // evacuation route: it preserves their age and survivor placement and
        // frees young blockers that happen to lie inside the target window.
        if compaction_plan.is_some() {
            cset.extend(
                regions
                    .iter()
                    .enumerate()
                    .filter(|(i, r)| {
                        !r.pinned
                            && !jit_pinned_regions.contains(i)
                            && (r.region_type() == RegionType::Eden
                                || r.region_type() == RegionType::Survivor)
                    })
                    .map(|(i, _)| i),
            );
        }
        // JIT round 23 wave 3, lane L4: the young-source prerequisite --
        // every young region this pause leaves out of its set is walked for
        // its outgoing edges before anything below reads a remembered set.
        {
            let in_cset: RegionSet = cset.iter().copied().collect();
            self.record_excluded_young_sources(&mut regions, &|i| in_cset.contains(&i));
        }
        let cset_young = cset
            .iter()
            .filter(|&&idx| {
                matches!(
                    regions[idx].region_type(),
                    RegionType::Eden | RegionType::Survivor
                )
            })
            .count();
        let (jni_pinned_out, jit_pinned_out) =
            count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);

        // Old half of the CSet: ONE selector for every pause and test (gci
        // i11/gm finding 3; see [`Self::choose_old_cset`] for the Step 7
        // pause-target sizing, the percentage cap, the full-compaction and
        // compaction-plan arms and each switch's arm). The census
        // (`capped_budget` / `capped_percent` / `old_regions`) is counted by
        // `note_old_cset_census`, the same call the parallel driver makes.
        // gcd d10/o — a full-compaction pause takes its own candidates, uncapped.
        let full_compaction = self.full_compaction_this_pause.load(Ordering::Relaxed);
        let plan_sources = compaction_plan
            .as_ref()
            .map(|plan| plan.sources.len())
            .unwrap_or(0);
        let old_source = if compaction_plan.is_some() {
            OldCsetSource::Plan
        } else if full_compaction {
            OldCsetSource::FullCompaction {
                young_bytes: cset.iter().map(|&i| regions[i].cursor()).sum(),
            }
        } else {
            OldCsetSource::Mixed
        };
        let old_choice = self.choose_old_cset(
            &regions,
            &jit_pinned_regions,
            old_source,
            g1_pause_cost_model(),
        );
        self.note_old_cset_census(&old_choice, plan_sources);
        self.end_mixed_phase_if_only_pinned_left(&old_choice);
        let old_selected = plan_sources + old_choice.picked.len();
        cset.extend(old_choice.picked.iter().copied());

        // Ten-findings item 2 — the pre-evacuation `(region_type, cursor)`
        // snapshot the narrow fix-up diffs against; see the young path. Taken
        // BEFORE the humongous relocation below, so the destination spans read
        // as changed regions and the narrow fix-up walks them.
        let pre_evac: Vec<(RegionType, usize)> = regions
            .iter()
            .map(|r| (r.region_type(), r.cursor()))
            .collect();
        // r16 G1 page: the compaction plan's humongous spans move out of the
        // target window before anything is evacuated. Each moved span joins the
        // CSet: its one object is forwarded exactly as an evacuated object is
        // (roots, remembered-set sources and the Cheney scan all resolve the
        // forward), and Phase 5 frees its regions. A move whose destination is
        // no longer Free is dropped and its span stays where it is.
        let mut relocated: Vec<(usize, usize)> = Vec::new();
        if let Some(plan) = &compaction_plan {
            for &mv in &plan.humongous_moves {
                if let Some((old, new, size)) = self.relocate_humongous_span(&mut regions, mv) {
                    cset.extend(mv.src_start..mv.src_start + mv.len);
                    pointer_map.insert(old, new);
                    objects_copied += 1;
                    bytes_copied += size;
                    relocated.push((old, new));
                }
            }
        }
        let cset_set: RegionSet = cset.iter().copied().collect();
        // Same invariant as the young path: a pinned region must never enter a
        // collection set, because Phase 5 resets every CSet region that holds
        // no self-forwarded object. A mixed CSet is the harder case — it also
        // takes Old regions, where a long-lived JIT-rooted or JNI-pinned object
        // is most likely to have ended up.
        debug_assert!(
            cset.iter()
                .all(|&i| !regions[i].pinned && !jit_pinned_regions.contains(&i)),
            "G1 mixed CSet contains a pinned region"
        );
        let mut work_list: Vec<*mut u8> =
            relocated.iter().map(|&(_, new)| new as *mut u8).collect();
        // G1AUD-10 / F-07 — the mixed driver reported no phase breakdown at
        // all, which is backwards: a mixed pause is the long one, and it is the
        // only kind whose fix-up and free phases touch the old generation.
        let mut phases = G1PausePhases::default();
        // PARITY (2026-09-06): the mixed drivers left these three at zero, so a
        // mixed pause's `[GC-STAT]` line said `cset_regions=0` for a pause that
        // by definition collected some. See `young_collection_parallel`. The
        // pin counts are the YOUNG-region ones, which is exactly what this
        // path's own `record_g1_cycle` already reports -- an old region kept
        // out of a mixed CSet is not counted by either.
        {
            let (jni_pinned_out, jit_pinned_out) =
                count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);
            phases.cset_regions = cset.len() as u32;
            phases.jni_pinned_out = jni_pinned_out as u32;
            phases.jit_pinned_out = jit_pinned_out as u32;
        }
        let mut phase_mark = std::time::Instant::now();

        // Evacuate roots
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if let Some(region_idx) = self.region_for_ptr(&regions, old_ptr) {
                if cset_set.contains(&region_idx) {
                    let plausible = self.note_root_object_plausibility(&regions, old_ptr as usize);
                    // A ROOT THAT IS NOT AN OBJECT START IS NOT EVACUATED.
                    //
                    // Measured on `TestKillProcessWhileWriting` (2026-09-02): a
                    // CSet root landed 0x28 bytes INSIDE a live reference array
                    // (`grid=INTERIOR of=0x108 delta=0x28 size=0x110 cid=185
                    // kind=Array`, the extent independently confirmed by the
                    // region's own carve trail). `evacuate_object` then read
                    // that ELEMENT as a header: a heap pointer's low half became
                    // the class id and its high half became `num_slots` --
                    // 0x200, which is why every holder in that investigation
                    // reported `num_slots=512` -- so the object was sized at
                    // 0x2010, eight kilobytes were copied into a Survivor
                    // region, and `*root` was rewritten to name the result.
                    // Scanning that fake object as 512 legacy slots is the
                    // source of the rejected candidates, the dangling references
                    // and the segfault this class is filed for.
                    //
                    // Copying it was never right. `note_root_object_plausibility`
                    // has reported the condition since it was written and
                    // `NON_OBJECT_ROOT_COPIED` counted the copies -- the guard
                    // was measurement-only, and what it measured was the
                    // collector manufacturing an object out of an array element.
                    //
                    // Leaving the root alone cannot be worse. A conservative JIT
                    // root is a stack word that merely LOOKS like a pointer, so
                    // an interior or garbage one names nothing the mutator reads
                    // through; and pinning its region keeps whatever it is
                    // interior to from moving out from under it, which is the
                    // treatment `pin_region_for_addr` already exists to give a
                    // conservatively-discovered root.
                    if !plausible {
                        let n = NON_OBJECT_ROOT_SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
                        if n <= 8 || n.is_power_of_two() {
                            tracing::warn!(
                                "[g1] a NON-OBJECT root was SKIPPED (#{n}): 0x{:x} — it is not the start of a live object, so evacuating it would have sized an object from bytes that are not a header. The root is left unchanged and its region pinned.",
                                old_ptr as usize,
                            );
                        }
                        // NO PIN HERE. `G1Region::pinned` is the JNI-critical
                        // pin, cleared only by a matching `unpin_region` or by
                        // `reset` -- and a pinned region is never collected, so
                        // it is never reset. Setting it from this loop leaked
                        // the region for the life of the process, and bought
                        // nothing even for this pause: the CSet is already
                        // chosen by the time a root is processed.
                        //
                        // `pinned_region_set_including_non_object_roots` is
                        // where a non-object root's region is kept out of the
                        // CSet, which is BEFORE selection and per-pause. This
                        // arm is the backstop for anything that reaches here
                        // anyway.
                        continue;
                    }
                    // Step 9: `fresh` is ignored here — the root loop keeps its
                    // existing unconditional push (a duplicate root re-scans
                    // idempotently). Gating it on `fresh` is deferred to the
                    // parallel evacuator (where duplicate worklist entries
                    // matter for worker load).
                    if let Some((new_ptr, _fresh)) = self.evacuate_object(
                        &mut regions,
                        old_ptr,
                        &mut pointer_map,
                        &mut objects_copied,
                        &mut bytes_copied,
                        &cset_set,
                    ) {
                        *root = unsafe { ObjectRef::from_raw(new_ptr) };
                        work_list.push(new_ptr);
                    }
                }
            }
        }

        // Marking keep-alive (see `marking_keepalive_roots`): a mixed pause
        // can run while a NEW concurrent mark cycle is active (IHOP re-fires
        // during the post-cleanup mixed sequence — `start_concurrent_mark`
        // does not clear `marking_complete`), and its CSet includes Old
        // regions, exactly where the gray set concentrates. Same protocol as
        // the young paths.
        // JIT round 23 wave 13, lane L9 (`CRATONVM_G1_KEEPALIVE_REGRAY_ONCE`,
        // opt-in): only the SATB-sourced keep-alive roots are re-grayed; a
        // root that came from the gray set is re-grayed by the end-of-pause
        // remap, which rewrites that very entry to the copy. Pushing it here
        // as well left TWO gray entries per object per pause, and the
        // next pause of the cycle found both in its CSet again: the gray set
        // DOUBLED per pause for every young object the starved marker had not
        // reached (`roots_us` 4.2 s + `other_us` 3.6 s of 26.3 s of young
        // pauses on `KeepLong 30`, n23-w12a).
        let (mut keepalive, satb_sourced) = self.marking_keepalive_roots_split(&regions, &cset_set);
        // g1pin: the peer-stack pinned objects are roots too, after the
        // SATB-sourced prefix (`append_peer_object_pin_roots`).
        self.append_peer_object_pin_roots(&mut keepalive, &cset_set);
        let regray_once = g1_keepalive_regray_once_enabled();
        // The re-grays, pushed under one lock after the loop
        // (`push_gray_or_mark_all`): every one names a to-space copy, whose
        // region nothing below retypes before the push.
        let mut regray: Vec<usize> = Vec::new();
        for (k, addr) in keepalive.into_iter().enumerate() {
            if !self.evacuation_supply_is_an_object(&regions, "marking-keepalive", addr) {
                continue;
            }
            if let Some((new_ptr, fresh)) = self.evacuate_object(
                &mut regions,
                addr as *mut u8,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &cset_set,
            ) {
                if fresh {
                    work_list.push(new_ptr);
                }
                if keepalive_needs_regray(regray_once, k, satb_sourced) {
                    regray.push(new_ptr as usize);
                }
            }
        }
        self.push_gray_or_mark_all(&regions, regray);

        // CRIT fix (UAF): process remembered-set sources for every CSet
        // region. Collect sources up front (before mutating regions during
        // evacuation), dedup, then walk each source region rewriting
        // CSet-bound slots and seeding the work_list with newly evacuated
        // targets. Without this, cross-region refs (e.g. old → young)
        // were silently dropped, leaving stale pointers in non-CSet
        // regions after CSet reset.
        phases.roots_us = phase_mark.elapsed().as_micros() as u64;
        phase_mark = std::time::Instant::now();

        let mixed_rset_sources: RegionSet = {
            // Round-9 gc CRIT-8: the snapshot accessors return owned data
            // (the underlying map lives behind a per-RSet mutex), so the RSet
            // lock is not held across the body.
            //
            // G1AUD-5 (defect G1-8): entries whose source was recycled since
            // the edge was recorded are dropped rather than re-walked — see
            // `live_rset_sources` and the note in `young_collection`.
            let mut set = Self::live_rset_sources(&regions, &cset);
            // JIT-pinned regions are scanned as sources too (see
            // young_collection): their objects stay in place but their CSet
            // referents must still be evacuated and fixed up.
            set.extend(jit_pinned_regions.iter());
            set
        };
        let rset_sources_scanned = mixed_rset_sources.len();
        let narrow_sources = mixed_rset_sources.clone();

        // F-05 — see the twin in `young_collection`: the JIT-pinned additions
        // are walked wholesale, the remembered set's own sources are screened.
        let card_screen_snapshot = self.card_scan_snapshot();
        for src_idx in mixed_rset_sources {
            self.scan_source_region_for_cset_refs(
                &mut regions,
                src_idx,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
                self.card_screen_for_source(&jit_pinned_regions, src_idx),
                None,
            );
        }
        self.fill_card_scan_phases(&mut phases, card_screen_snapshot);

        phases.rset_us = phase_mark.elapsed().as_micros() as u64;
        phase_mark = std::time::Instant::now();

        // Cheney scan
        // JIT round 23 wave 8, lane L9: the per-pause serial evacuation
        // knobs (`CRATONVM_G1_SERIAL_EVAC_SIZE_MEMO`, `CRATONVM_G1_EVAC_SHAPE_KEY`).
        let mut evac_knobs = SerialEvacKnobs::new_before_retire();
        let mut scan_idx = 0;
        while scan_idx < work_list.len() {
            let obj_ptr = work_list[scan_idx];
            scan_idx += 1;

            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            self.scan_and_evacuate_refs_tuned(
                &mut regions,
                obj_ptr,
                header,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut work_list,
                &mut evac_knobs,
            );
        }

        // Phase 3c: collection-overlay edges — see young_collection_serial.
        let mut overlay = self.overlay_pause_begin(
            &mut regions,
            &cset_set,
            &mut pointer_map,
            &mut objects_copied,
            &mut bytes_copied,
            self.overlay_prepare(&cset_set, false),
        );

        // Phase 3.5: finalizer resurrection — see young_collection.
        self.resurrect_dead_finalizers(
            &mut regions,
            &cset_set,
            &mut pointer_map,
            &mut objects_copied,
            &mut bytes_copied,
            &mut work_list,
        );
        self.overlay_pause_follow(
            &mut overlay,
            &mut regions,
            &cset_set,
            &mut pointer_map,
            &mut objects_copied,
            &mut bytes_copied,
        );
        self.overlay_pause_finish(overlay, &regions, &pointer_map);

        phases.closure_us = phase_mark.elapsed().as_micros() as u64;
        phase_mark = std::time::Instant::now();

        // Update references and free evacuated regions.
        //
        // Ten-findings item 2: the mixed fix-up is narrowed by the same rule as
        // the young one. F-04 left it wide on the argument that the OLD
        // members' rsets are "maintained by this very walk's rebuild half", so
        // narrowing would feed the rebuild its own output. That argument proves
        // too much: the young CSet's rsets are maintained by exactly the same
        // producers (the mutator barrier, the rebuild over walked regions, the
        // evacuation-failure fix-up), and the young walk has been narrow since
        // G1AUD-11. What makes either sound is that every slot needing a
        // rewrite lives in a region that is a recorded SOURCE of some CSet
        // member or a region this pause WROTE INTO — and the rebuild only ever
        // ADDS edges for the regions it walks, which is what a narrow walk
        // does too; a region it does not walk keeps the entries it had. The
        // rebuild has never been what makes THIS pause sound (a missing
        // barrier entry is a UAF in this pause and repaired for the next —
        // `g1-audit.md` §2.1); it repairs for later pauses, and it still
        // does. `CRATONVM_G1_NARROW_FIXUP=0` restores the wide walk for both.
        let narrow = self.phase4_regions_to_walk(&regions, Some(&pre_evac), &narrow_sources);
        let census = self.update_references_in_regions(
            &mut regions,
            &cset_set,
            &pointer_map,
            narrow.as_ref(),
        );
        phases.fixup_us = phase_mark.elapsed().as_micros() as u64;
        phases.fixup_regions = census.walked_regions;
        phases.fixup_bytes = census.walked_bytes;
        phase_mark = std::time::Instant::now();
        // A relocated span is a new SOURCE region: name it in the remembered
        // set (and card) of every region its object references, as the kept
        // objects of an evacuation failure are named.
        for &(_, new) in &relocated {
            self.record_outgoing_rset_edges(&mut regions, new);
        }

        // F-02: retire the from-space forwards this pause installed, before
        // Phase 5 decides which regions to free and which to keep. After the
        // fix-up (which resolves them), before the reclaim (which is what makes
        // a kept region's stale forward outlive the pause). See
        // `retire_forwards`.
        self.retire_forwards(&pointer_map, &cset_set);
        self.report_pending_corrupt_holders(&regions);
        let _ = self.count_unretired_forwards(&regions, &cset_set, &pointer_map);
        let bytes_freed = self.free_or_keep_cset(&mut regions, &cset, &pointer_map);
        self.record_kept_young_regions_from_pre(&regions, &pre_evac);

        // The allocation-failure compactor promises a contiguous destination
        // span, not merely that it *attempted* to evacuate the blockers.  Keep
        // the post-pause geometry visible on the rare failure path: a retained
        // CSet region (to-space exhaustion) is materially different from a
        // later mutator allocation that consumed the completed span.
        if let Some(plan) = &compaction_plan {
            let free_in_target = regions[plan.target_start..plan.target_end]
                .iter()
                .filter(|region| region.region_type() == RegionType::Free)
                .count();
            tracing::warn!(
                requested = plan.target_end - plan.target_start,
                target_start = plan.target_start,
                target_end = plan.target_end,
                free_in_target,
                moved_sources = plan.sources.len(),
                relocated_humongous = relocated.len(),
                "g1 allocation-failure compaction: post-evacuation target geometry"
            );
            if let Some(target) = plan.staged_for {
                // The run is Free (or as Free as to-space exhaustion left it);
                // the request goes back for the next recovery pause, which
                // plans the window with this pause's region state.
                *self.compaction_staged_old.lock() = (0..regions.len())
                    .filter(|&i| {
                        pre_evac[i].0 == RegionType::Free
                            && regions[i].region_type() == RegionType::Old
                    })
                    .collect();
                self.compaction_stages.fetch_add(1, Ordering::AcqRel);
                self.pending_humongous_compaction
                    .lock()
                    .replace(HumongousCompactionPlan {
                        requested_regions: plan.requested_regions,
                        target_start: 0,
                        target_end: target,
                        sources: Vec::new(),
                        young_sources: 0,
                        humongous_moves: Vec::new(),
                        staged_for: None,
                    });
            } else {
                self.compaction_staged_old.lock().clear();
            }
        }

        // Humongous spans are never evacuated, so this is the only point in a
        // young/mixed pause that can free one. It must come after Phase 5: the
        // census it consumes is only a death certificate once the CSet is gone
        // and the pause's to-space has been walked. See
        // `eager_reclaim_humongous_locked` for the full ordering argument.
        // Not in a pause that relocated a humongous span: the moved span's
        // remembered set and this pause's scan marks name its SOURCE address,
        // so the census could read the live copy as unreferenced.
        let bytes_freed = bytes_freed
            + if relocated.is_empty() {
                self.eager_reclaim_humongous_locked(
                    &mut regions,
                    roots,
                    &pointer_map,
                    &census,
                    &jit_pinned_regions,
                )
            } else {
                0
            };

        // SECURITY FIX (V7b): mixed GC frees old regions as well as young
        // ones, where a stale/incomplete rset is most likely. Verify no
        // survivor slot dangles into a freed CSet region. No-op on the
        // release/quiet path; aborts in debug.
        phases.free_us = phase_mark.elapsed().as_micros() as u64;
        // The census, while the guard is still held -- see the field docs
        // and the PARITY note in `young_collection_parallel`.
        phases.record_region_census(&regions);
        phase_mark = std::time::Instant::now();
        self.verify_no_dangling_into_cset(&regions, &cset_set, &pointer_map, roots);
        self.dbg_verify_rset_completeness(&regions, "mixed-serial");
        // Env-gated diagnostics (no-ops unless CRATONVM_G1_DBG_HEADERS /
        // CRATONVM_G1_DBG_ZERO are set) — mixed-path coverage matching the
        // parallel young path.
        self.dbg_verify_no_unrewritten_forward(&regions, &cset_set, &pointer_map, roots);
        self.dbg_scan_for_zeroed_refs(&regions, &cset_set, roots);
        self.dbg_verify_reachable_integrity(&regions, roots, "mixed-serial");
        phases.verify_us = phase_mark.elapsed().as_micros() as u64;

        self.retire_eden_slots_in_cset(&cset_set);

        // Relaxed ordering: statistics counter for IHOP heuristics only.
        self.recompute_old_gen_bytes(&regions);
        self.publish_occupancy_metrics(&regions);

        monitors.remap_after_gc(&pointer_map);
        // INT-8: carry the referent-slot skip set across this pause
        // (re-key survivors, prune CSet casualties) — same protocol
        // point as the monitor-table remap. No-op outside a mark cycle.
        self.remap_reference_skip_set(&cset_set, &pointer_map);
        // The overlay mark snapshot, by the same protocol (`g1_overlay.rs`).
        self.remap_overlay_snapshot(&regions, &cset_set, &pointer_map, false);
        // G1CORE-6: same protocol point, same reason — the dedup table's
        // canonical addresses go stale on exactly the objects this pause moved.
        self.remap_string_dedup_table(&cset_set, &pointer_map);

        // Remap (or drop) stale concurrent-mark worklist entries — same
        // protocol as the young paths. A mixed pause moves OLD objects, where
        // the gray set concentrates; before this block the mixed path left
        // every gray pointing into freed CSet regions (marker UAF whenever a
        // new mark cycle overlaps the mixed sequence). With the keep-alive
        // above, every retained CSet gray has a forwarding entry.
        self.remap_gray_set_after_pause(&regions, &cset_set, &pointer_map);

        // Decrement mixed GC counter (gci i11/gm finding 8: a compaction pause
        // is not a mixed pause; see `spend_mixed_count`).
        self.spend_mixed_count(full_compaction || compaction_plan.is_some());

        let elapsed = start.elapsed();
        let pause_us = elapsed.as_micros() as u64;
        // Step 7 — recalibrate the rolling copy-cost from this mixed cycle's
        // actual pause / bytes copied, so the next mixed CSet is sized against
        // real wall-clock throughput.
        // LANE W5-A — routed through `update_evac_cost_for_mixed` so the
        // definition of the observation (whole pause, or the closure that did
        // the copying) lives with the model rather than in each driver.
        self.update_evac_cost_for_mixed(
            elapsed.as_nanos() as u64,
            phases.fused_closure_us(),
            bytes_copied,
        );
        phases.close(pause_us);
        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };
        // gci i18/mx -- the degraded flags BEFORE the record (one pointer-map
        // scan, shared with `record_g1_cycle`), so the futile breaker can
        // leave an evacuation-failure or compaction pause out of its streak.
        let degraded = self.g1_pause_degraded_flags_for(&pointer_map, jni_pinned_out, jit_pinned_out, false);
        phases.evacuation_failed =
            degraded & cratonvm_gc_core::gc_metrics::g1_degraded::EVACUATION_FAILURE != 0;
        phases.compaction = full_compaction || compaction_plan.is_some();
        self.record_collection_with_phases(G1CollectionType::Mixed, pause_us, &stats, phases);
        cratonvm_gc_core::gc_metrics::record_g1_cycle(
            cratonvm_gc_core::gc_metrics::g1_cycle_kind::MIXED,
            cset_young as u32,
            old_selected as u32,
            (jni_pinned_out + jit_pinned_out) as u32,
            rset_sources_scanned as u32,
            degraded,
        );
        // JIT round 23 wave 8, lane L9: the next serial pause's pre-size.
        self.note_serial_forwards(pointer_map.len());
        GcResult { stats, pointer_map }
    }

    // -----------------------------------------------------------------------
    // Step 9 — parallel evacuation drivers (gated; see the module note above)
    // -----------------------------------------------------------------------

    /// Number of evacuation workers (always >= 1). With 1 worker the parallel
    /// code path drains serially — useful for determinism testing.
    ///
    /// Precedence: the `CRATONVM_G1_WORKERS` diagnostic override, then an
    /// explicit `gc_worker_threads` (including requests above the CPU count), then the
    /// machine-derived ergonomic. See [`ergonomic_gc_worker_threads`].
    pub fn parallel_worker_count(&self) -> usize {
        gc_worker_threads_for(&self.config)
    }

    /// F-12 — how many CONCURRENT-MARK workers this collector runs.
    ///
    /// Fixed at construction, because it sizes `mark_deques` and a worker's id
    /// indexes into that array.
    #[inline]
    pub fn mark_worker_count(&self) -> usize {
        self.mark_deques.len()
    }

    /// F-12 diagnostics — successful work-steals so far this process.
    #[inline]
    pub fn dbg_mark_steals(&self) -> usize {
        self.mark_steals.load(Ordering::Relaxed)
    }

    /// F-12 diagnostics — objects scanned, per worker id.
    ///
    /// The DISTRIBUTION is what this is for. A total tells you marking
    /// happened; only the split tells you whether it happened in parallel, and
    /// "all of it on worker 0" is a serial marker wearing a parallel marker's
    /// clothes.
    pub fn dbg_mark_worker_scans(&self) -> Vec<usize> {
        self.mark_worker_scans
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect()
    }

    /// F-12 diagnostics for the out-of-crate throughput probe
    /// (`gc/tests/g1_mark_parallelism.rs`): the per-worker scan split.
    ///
    /// Public because an integration test is a separate crate, and the split is
    /// exactly what keeps that probe from quoting a wall-clock number for a run
    /// in which the parallelism never engaged.
    pub fn dbg_mark_worker_scans_public(&self) -> Vec<usize> {
        self.dbg_mark_worker_scans()
    }

    /// F-12 diagnostics for the out-of-crate throughput probe: steal count.
    pub fn dbg_mark_steals_public(&self) -> usize {
        self.dbg_mark_steals()
    }

    /// F-12 diagnostics — is `addr` marked in its region's bitmap?
    pub fn dbg_is_marked(&self, addr: usize) -> bool {
        let regions = self.regions.read();
        match self.lookup_region_for_addr(addr) {
            Some(idx) => regions[idx].mark_bitmap.is_marked(addr),
            None => false,
        }
    }

    /// F-12 — per-worker gray-deque cap.
    ///
    /// `MARK_WORKLIST_CAP` exists to turn a pathological fan-out into a
    /// deterministic overflow protocol instead of an allocator OOM, and that
    /// argument is about TOTAL gray memory. Splitting the cap across the
    /// deques keeps the total bounded at roughly twice the original (the seed
    /// queue still carries the full cap, because seeds are the class of entry
    /// that must not be dropped — see the G1MARK-7 note in `remark`), rather
    /// than at `workers + 1` times it. The floor keeps a many-worker
    /// configuration from giving each deque a cap so small that ordinary
    /// marking trips the overflow rescan.
    #[inline]
    pub fn mark_deque_cap(&self) -> usize {
        (MARK_WORKLIST_CAP / self.mark_deques.len().max(1)).max(4096)
    }

    /// zt pl4 item 3: `(parallel workers, concurrent mark)` CPU in
    /// nanoseconds, HotSpot G1's `sun.threads.cpu_time.gc_parallel_workers`
    /// and `gc_conc_mark` (both monotone).
    pub fn gc_thread_cpu_ns(&self) -> (u64, u64) {
        let relaxed = std::sync::atomic::Ordering::Relaxed;
        (
            self.evac_pool
                .get()
                .map_or(0, cratonvm_gc_core::evac_pool::EvacPool::worker_cpu_ns)
                .saturating_add(self.driver_cpu_ns.load(relaxed)),
            self.conc_mark_cpu_ns.load(relaxed),
        )
    }

    /// zt pl4 item 3: book the collecting thread's CPU inside a collection
    /// (see `driver_cpu_ns`).
    pub fn note_driver_cpu_ns(&self, ns: u64) {
        self.driver_cpu_ns
            .fetch_add(ns, std::sync::atomic::Ordering::Relaxed);
    }

    /// zt pl4 item 3: the concurrent-mark workers' CPU total (see the field).
    pub fn conc_mark_cpu_counter(&self) -> &std::sync::atomic::AtomicU64 {
        &self.conc_mark_cpu_ns
    }

    /// g1gen/c6: this collector's own evacuation-pool census; `None` while no
    /// parallel evacuation has built the pool.
    pub fn evac_pool_census(&self) -> Option<cratonvm_gc_core::evac_pool::EvacPoolCensus> {
        self.evac_pool.get().map(cratonvm_gc_core::evac_pool::EvacPool::census)
    }

    /// The persistent evacuation worker pool, created on first use.
    ///
    /// See the `evac_pool` field for why this is lazy rather than built in
    /// [`G1Collector::new`].
    pub fn evac_pool(&self) -> &cratonvm_gc_core::evac_pool::EvacPool {
        self.evac_pool.get_or_init(|| {
            cratonvm_gc_core::evac_pool::EvacPool::new(self.parallel_worker_count().saturating_sub(1))
        })
    }

    /// Shared seed + parallel transitive-closure core used by both
    /// `young_collection_parallel` and `mixed_collection_parallel`.
    ///
    /// Seeds the work queue from `roots` and from `sources` on the calling
    /// (driver) thread, then runs the work-stealing closure across
    /// `parallel_worker_count()` workers (the driver participating as one). Roots
    /// are rewritten in place. Returns the merged forwarding map plus the
    /// objects/bytes copied.
    ///
    /// `sources` is the set of region indices to walk wholesale looking for
    /// CSet-bound references. G1AUD-6 (defect G1-9): it is computed by the
    /// CALLER, while the `regions` guard is still dereferenceable, and must be
    /// the same set the serial path builds — the live remembered-set sources of
    /// every CSet region PLUS every JIT-pinned region. This function used to
    /// derive it itself from the CSet remembered sets alone, which silently
    /// dropped the JIT-pinned half; see `young_collection_parallel` for why that
    /// is a live-object loss and not merely a missing optimisation.
    ///
    /// `wholesale_sources` (F-05) is the subset of `sources` that must be
    /// walked with NO card screen — the JIT-pinned regions. The screen is
    /// derived from the premise that every reference store into a region
    /// reached `post_write_barrier_rset`, and walking the pinned regions
    /// wholesale is precisely the collector declining to rely on that premise
    /// (see the note at the `young_collection` call site). Passing the same set
    /// on both arms is what keeps `CRATONVM_G1_PARALLEL_EVAC` a speed switch
    /// rather than a semantics switch — the divergence class of defect G1-9.
    ///
    /// SAFETY: `regions_base` must be the live regions Vec base and the caller
    /// must hold `regions.lock()` for the whole call WITHOUT dereferencing the
    /// guard (see the module SAFETY MODEL note). `pool` lists currently-Free
    /// region indices reserved for to-space allocation.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::g1) unsafe fn parallel_evacuate(
        &self,
        regions_base: RegionsBase,
        regions_len: usize,
        cset_set: &RegionSet,
        pool: Vec<usize>,
        pool_committed: usize,
        roots: &mut [ObjectRef],
        keepalive: &[usize],
        sources: &RegionSet,
        wholesale_sources: &RegionSet,
        precise_rset_liveness: Option<&MarkProvenDead>,
        overlay: Option<&overlay::OverlayPrepass>,
        // gci i14/yt: out — `(roots_us, rset_us)`, the driver's wall time for
        // Phases 1-1c and for Phase 2; see `G1PausePhases::parallel_seed_us`.
        seed_split: &mut (u64, u64),
    ) -> (
        cratonvm_types::PointerMap,
        usize,
        usize,
        Option<Vec<(usize, usize)>>,
    ) {
        // One-shot confirmation that the parallel evacuator is genuinely active
        // (the gauntlet lesson: never assume a gated path was taken — verify).
        {
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    "g1: parallel evacuation ACTIVE ({} workers, CRATONVM_G1_PARALLEL_EVAC)",
                    self.parallel_worker_count()
                );
            }
        }
        // `CRATONVM_G1_EVAC_COPY_WATCH=1` only; see [`CopyWatch`]. Declared here
        // so it outlives `shared` and can be verified at two checkpoints.
        // A PROCESS static, not a per-pause local. The in-pause checkpoints
        // measured 0 of 374k and 0 of 401k copies rewritten, so the interval
        // that matters is the one BETWEEN pauses: if a copy this pause made is
        // corrupt when the next pause starts, the writer is not the collector.
        let copy_watch: Option<&CopyWatch> = gc_flags()
            .g1_evac_copy_watch
            .then(|| COPY_WATCH.get_or_init(CopyWatch::default));
        let arena = self.arena_base..self.arena_end;
        // CHECKPOINT 0 — the previous pause's copies, re-read before this pause
        // touches anything. Entries whose region has been recycled since are
        // dropped rather than reported.
        if let Some(w) = copy_watch {
            w.verify(
                "at the START of the next pause",
                arena.clone(),
                Some(RegionView::Raw(regions_base, regions_len)),
            );
            w.clear();
        }
        // Membership test for the reserved pool; see `SharedEvac::pool_set`.
        let pool_set: RegionSet = pool.iter().copied().collect();
        // Three cursors packed into one word — front for Old, back for
        // Survivor, grow for the uncommitted tail; see
        // `SharedEvac::claim_pool_slot`. The packing caps the pool at 2^21
        // slots, which is 2 TiB at the 1 MiB minimum region size.
        debug_assert!(pool.len() < 1 << POOL_CURSOR_BITS);
        debug_assert!(pool_committed <= pool.len());
        let pool_next = AtomicU64::new(0);
        // Old regions a previous pause's promotion TLABs left partly filled --
        // see `SharedEvac::resume` for what taking them up is worth. Built
        // here rather than in each driver so the young and mixed parallel
        // paths cannot diverge on it (the shape of defect G1-9), and from the
        // raw base under the module's SAFETY MODEL, exactly as
        // `shared_dest_alloc` reads regions.
        //
        // The room floor is one legacy cell: below it the region cannot hold
        // even the smallest object, and claiming it would burn a resume slot
        // and then fall through to the pool anyway.
        let resume: Vec<usize> = {
            (0..regions_len)
                .filter(|idx| !pool_set.contains(idx) && !cset_set.contains(idx))
                .filter(|&idx| {
                    // SAFETY: `idx < regions_len`; a shared read of one region.
                    let r = &*regions_base.0.add(idx);
                    r.region_type() == RegionType::Old
                        && r.data.len().saturating_sub(r.cursor()) >= HEADER_SIZE + SLOT_SIZE
                })
                .collect()
        };
        let resume_set: RegionSet = resume.iter().copied().collect();
        let resume_next = AtomicUsize::new(0);
        // Per-PAUSE, deliberately: the proof `shared_dest_alloc` records is
        // "no eligible region of this type holds `size` bytes RIGHT NOW", and
        // Phase 5 frees the whole collection set the moment the pause ends.
        // A process-lifetime latch would carry a true statement about one
        // pause into every pause after it. See `SharedEvac::dest_empty_slot`.
        let dest_empty_from: [AtomicUsize; 2] =
            [AtomicUsize::new(usize::MAX), AtomicUsize::new(usize::MAX)];
        // gci i14/yt: the shared current Survivor region for
        // `CRATONVM_G1_SURVIVOR_PLAB`; per pause, like `dest_empty_from`.
        let survivor_plab_region = AtomicUsize::new(usize::MAX);
        // JIT round 23 wave 10: the shared current Old region for
        // `CRATONVM_G1_OLD_PLAB`; per pause, like the Survivor one.
        let old_plab_region = AtomicUsize::new(usize::MAX);
        let old_plab = g1_old_plab_enabled();
        let queue: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        let outstanding = AtomicUsize::new(0);
        let idle_peers = AtomicUsize::new(0);
        // ONE clone of the frozen-peer skip spans for the whole pause, instead
        // of one per source region inside `seed_source_region`; see
        // `SharedEvac::jit_skips`. Every mutator is parked, so the list cannot
        // change under the walk.
        let jit_skips = self.jit_tlab_skip_spans();
        // Snapshot the flags the inner loops read; see `SharedEvac::screens`.
        let flags = gc_flags();
        let nworkers = self.parallel_worker_count();
        // At most one worker per `EVAC_POOL_REGIONS_PER_WORKER` regions of
        // to-space pool (`CRATONVM_G1_EVAC_WORKERS_BY_POOL`, default ON).
        let nworkers =
            {
                // JIT round 23 wave 10, lane L9: with BOTH destinations shared
                // (`CRATONVM_G1_SURVIVOR_PLAB` and `CRATONVM_G1_OLD_PLAB`) a
                // worker no longer holds a whole Free region of its own, so
                // the per-worker quantum is the shared one.
                let per_worker = evac_pool_regions_per_worker(old_plab);
                nworkers.min((pool.len() / per_worker).max(1))
            };
        // The to-space pool's region count, for `old_plab_worker_split` below
        // (`pool` is rebound to the evacuation thread pool further down).
        let to_space_regions = pool.len();
        // THE WIDTH THIS PAUSE ACTUALLY EVACUATED AT.
        //
        // Recorded here rather than at either dispatcher, because this is the
        // first point at which an evacuation is certain to happen at all --
        // `lane-d-small-findings-not-fixed.md` item 5. Both drivers can return
        // before reaching this line (an empty collection set), and a census
        // field whose doc says "actually asked for" must not move on a pause
        // that asked for nothing.
        G1_EVAC_WORKERS_LAST.store(nworkers, Ordering::Relaxed);
        // g1sz: every worker's `TlabSet::tospace`, moved here at retire.
        let tospace = Mutex::new(ClosureTospaceEdges::default());
        let object_pins = self.peer_object_pins_snapshot();
        let mut object_pin_regions = RegionSet::new();
        for &pin_addr in &object_pins {
            if let Some(ridx) = self.lookup_region_for_addr(pin_addr) {
                object_pin_regions.insert(ridx);
            }
        }
        let shared = SharedEvac {
            collector: self,
            regions_base,
            regions_len,
            cset: cset_set,
            pool: &pool,
            pool_committed,
            pool_set: &pool_set,
            pool_next: &pool_next,
            resume: &resume,
            resume_set: &resume_set,
            resume_next: &resume_next,
            dest_empty_from: &dest_empty_from,
            queue: &queue,
            outstanding: &outstanding,
            idle_peers: &idle_peers,
            // F-18: snapshot the ADAPTIVE threshold once per pause, not the
            // configured one. Once per pause rather than per object so every
            // worker in one pause makes the same decision — a threshold that
            // moved mid-pause would tenure two objects of the same age
            // differently for no reason the heap could explain.
            // gci i47w/sg47: 0 on a tenure-all `System.gc()` pause.
            promotion_age: self.evac_tenuring_threshold(),
            copy_watch,
            jit_skips: &jit_skips,
            screens: true,
            supply_screen: true,
            shared_dest: true,
            survivor_plab: true,
            survivor_plab_region: &survivor_plab_region,
            old_plab,
            old_plab_region: &old_plab_region,
            old_plab_bytes: old_plab_bytes_for(self.config.region_size, nworkers),
            plab_copy_end_elide: true,
            one_store_mark: g1_evac_one_store_mark_enabled(),
            copy_end_owner_store: g1_evac_copy_end_owner_store_enabled(),
            dest_latch: true,
            card_rset: true,
            card_clean: flags.g1_card_clean,
            precise_rset_liveness,
            wholesale_sources,
            // LANE W7-W (2026-09-21) — see `EvacShareCensus`.
            share_census: flags.g1_evac_share_census,
            idle_spill: true,
            idle_sleep: true,
            leaf_no_push: evac_leaf_no_push_enabled(),
            prefetch: g1_evac_prefetch_enabled(),
            object_pins,
            object_pin_regions,
            idle_try_lock: evac_idle_try_lock_enabled(),
            workers: nworkers,
            overlay,
            tospace_edges: true,
            tospace: &tospace,
        };

        let mut objs = 0usize;
        let mut bytes = 0usize;
        let mut main_tlab = TlabSet::default();
        let mut main_forwards: Vec<(usize, usize)> = self.take_forward_buffer();
        let mut main_deferred_self_forwarded: Vec<usize> = Vec::new();
        // Lane D (2026-09-20) — the driver's own engagement row. Worker 0 of
        // the census table IS the driver, and keeping its row in the same
        // shape as a helper's is what makes "all the work landed on the
        // driver" a readable distribution rather than a comparison between
        // two different kinds of number. See [`EvacWorkerCensus`].
        let mut main_census = EvacWorkerCensus {
            pauses: 1,
            ..Default::default()
        };

        // gci i14/yt: the seed/closure timing split (`seed_split`).
        let seed_roots_start = std::time::Instant::now();
        // Phase 1 (driver): seed roots.
        for root in roots.iter_mut() {
            let old_ptr = root.as_ptr();
            if let Some(ridx) = self.lookup_region_for_addr(old_ptr as usize) {
                if cset_set.contains(&ridx) {
                    // A ROOT THAT IS NOT AN OBJECT START IS NOT EVACUATED —
                    // the 2026-09-02 rule the SERIAL drivers apply at
                    // `young_collection` and `mixed_collection`, arriving on
                    // the arm that is actually the default. Read the long note
                    // at the serial site: a conservative JIT root landing 0x28
                    // bytes inside a live reference array made `evacuate` size
                    // an object out of an array ELEMENT, copy eight kilobytes
                    // of it into a Survivor region, and rewrite the root to
                    // name the result. Every downstream symptom — rejected
                    // candidates, dangling references, a SIGSEGV in whatever
                    // touched the fabricated object next — follows from that
                    // one copy, and this arm had no guard against it at all.
                    if !shared.note_root_object_plausibility_or_skip(old_ptr) {
                        continue;
                    }
                    if let Some((new_ptr, fresh)) = shared.evacuate(
                        &mut main_tlab,
                        old_ptr,
                        &mut main_forwards,
                        &mut objs,
                        &mut bytes,
                    ) {
                        *root = ObjectRef::from_raw(new_ptr);
                        if fresh {
                            if new_ptr == old_ptr {
                                main_deferred_self_forwarded.push(new_ptr as usize);
                            } else {
                                outstanding.fetch_add(1, Ordering::AcqRel);
                                queue.lock().push(new_ptr as usize);
                            }
                            shared.note_overlay_owner_copied(old_ptr, new_ptr);
                        }
                    }
                }
            }
        }

        // Per-helper state, shared by every dispatch of this pause (the
        // keep-alive seed, the Phase 2 seed and the closure) so a helper's
        // TLABs survive from the seeds into the closure. A `Mutex` per slot
        // rather than raw cells for the same reason the shards below used one:
        // it is what lets the dispatched body be a plain `Fn`, it is taken
        // once per helper per phase, and it is uncontended by construction
        // (slot `i` is touched only by worker `i`).
        let evac_pool = self.evac_pool();
        let helper_slots = nworkers.saturating_sub(1).min(evac_pool.helpers());
        // JIT round 23 wave 10, lane L9 (`CRATONVM_G1_OLD_PLAB`): the driver and
        // the first helpers keep whole Old regions of their own, up to what
        // the pool affords at `EVAC_POOL_REGIONS_PER_WORKER`; only the width
        // the shared-destination cap ADDED promotes through shared Old PLABs.
        let whole_old_workers = old_plab_worker_split(to_space_regions);
        let worker_state: Vec<Mutex<(TlabSet, EvacShard)>> = (0..helper_slots)
            .map(|i| {
                let mut set = TlabSet::default();
                // Worker 0 is the driver (`main_tlab`); helper `i` is worker
                // `i + 1`.
                set.old.plab_ok = old_plab && i + 1 >= whole_old_workers;
                let shard = EvacShard { forwards: self.take_forward_buffer(), ..EvacShard::default() };
                Mutex::new((set, shard))
            })
            .collect();

        // Phase 1b: seed marking keep-alive addresses (CSet-resident
        // gray/SATB objects — snapshot-live; see `marking_keepalive_roots`).
        // Evacuated exactly like roots; the fresh copies are re-grayed by the
        // caller and the worklist remap after Phase 5 rewrites the old gray
        // entries through the forwarding map instead of dropping them.
        //
        // g1ep (`CRATONVM_G1_PARALLEL_KEEPALIVE_SEED`, default ON, `=0` kills):
        // a long list is evacuated by the pool, chunks claimed with one
        // `fetch_add`, each worker into its own TLABs and forwards (the
        // helpers' slots carry on into Phase 2 and the closure, as the
        // parallel Phase 2 seed's do). The list is the SATB log's CSet
        // residents plus the CSet-resident grays, so on a heap whose mutator
        // overwrites a young reference per allocation (`G1epKeepRounds`) it
        // holds about one object per allocation since the last pause, and the
        // driver copied every one of them alone while the pool waited: 50-80
        // ms of `roots_us` on the later young pauses, 10% of all young pause
        // time. Each copy is claimed by the forwarding CAS, so two workers
        // reaching one object (a repeated entry) agree on its copy exactly as
        // the closure's workers do.
        let keepalive_helpers = if keepalive.len() >= PARALLEL_KEEPALIVE_SEED_MIN
        {
            helper_slots
        } else {
            0
        };
        if keepalive_helpers == 0 {
            for &addr in keepalive {
                let old_ptr = addr as *mut u8;
                if let Some(ridx) = self.lookup_region_for_addr(addr) {
                    if cset_set.contains(&ridx) {
                        // THE SUPPLY SCREEN THE SERIAL ARM APPLIES HERE, on the arm
                        // that is the default. `young_collection_serial` and
                        // `mixed_collection` both run every keep-alive address
                        // through `evacuation_supply_is_an_object` before handing it
                        // to the evacuator; this loop tested CSet RESIDENCY and
                        // nothing else. A keep-alive entry is a gray-set / SATB
                        // address, i.e. a DERIVED one, and `SharedEvac::evacuate`'s
                        // last-ditch guard checks only alignment -- which an
                        // interior pointer passes. Gated by the same
                        // `CRATONVM_G1_EVAC_SUPPLY_SCREEN` as the serial arm, so
                        // the lever stays one lever rather than two.
                        if !self.evacuation_supply_is_an_object_view(
                            shared.view(),
                            "marking-keepalive",
                            addr,
                        ) {
                            continue;
                        }
                        if let Some((new_ptr, fresh)) = shared.evacuate(
                            &mut main_tlab,
                            old_ptr,
                            &mut main_forwards,
                            &mut objs,
                            &mut bytes,
                        ) {
                            if fresh {
                                if new_ptr == old_ptr {
                                    main_deferred_self_forwarded.push(new_ptr as usize);
                                } else {
                                    outstanding.fetch_add(1, Ordering::AcqRel);
                                    queue.lock().push(new_ptr as usize);
                                }
                                shared.note_overlay_owner_copied(old_ptr, new_ptr);
                            }
                        }
                    }
                }
            }
        } else {
            // One keep-alive address into one worker's state; a fresh moved
            // copy goes to `fresh` (queued per chunk), a fresh self-forward to
            // `deferred`. The screens are the serial loop's.
            let seed_keepalive = |addr: usize,
                                  tlab: &mut TlabSet,
                                  forwards: &mut Vec<(usize, usize)>,
                                  objs: &mut usize,
                                  bytes: &mut usize,
                                  deferred: &mut Vec<usize>,
                                  fresh: &mut Vec<usize>| {
                let old_ptr = addr as *mut u8;
                let Some(ridx) = self.lookup_region_for_addr(addr) else {
                    return;
                };
                if !cset_set.contains(&ridx)
                    || !self.evacuation_supply_is_an_object_view(
                        shared.view(),
                        "marking-keepalive",
                        addr,
                    )
                {
                    return;
                }
                // SAFETY: the driver holds the regions guard for the whole
                // dispatch, and every copy is claimed by the forwarding CAS
                // (the closure's contract, which this shares).
                if let Some((new_ptr, is_fresh)) =
                    unsafe { shared.evacuate(tlab, old_ptr, forwards, objs, bytes) }
                {
                    if is_fresh {
                        if new_ptr == old_ptr {
                            deferred.push(new_ptr as usize);
                        } else {
                            fresh.push(new_ptr as usize);
                        }
                        // SAFETY: as above.
                        unsafe { shared.note_overlay_owner_copied(old_ptr, new_ptr) };
                    }
                }
            };
            const CHUNK: usize = 512;
            let chunks = keepalive.len().div_ceil(CHUNK);
            let cursor = AtomicUsize::new(0);
            let claim = || -> Option<(usize, usize)> {
                let c = cursor.fetch_add(1, Ordering::Relaxed);
                (c < chunks).then(|| (c * CHUNK, ((c + 1) * CHUNK).min(keepalive.len())))
            };
            let publish = |fresh: &mut Vec<usize>| {
                if !fresh.is_empty() {
                    outstanding.fetch_add(fresh.len(), Ordering::AcqRel);
                    queue.lock().append(fresh);
                }
            };
            let state_ref = &worker_state;
            let body = |i: usize| {
                let mut slot = state_ref[i].lock();
                let (tlab, shard) = &mut *slot;
                shard.census.pauses = 1;
                let mut fresh = Vec::new();
                while let Some((lo, hi)) = claim() {
                    for &addr in &keepalive[lo..hi] {
                        seed_keepalive(
                            addr,
                            tlab,
                            &mut shard.forwards,
                            &mut shard.objs,
                            &mut shard.bytes,
                            &mut shard.deferred,
                            &mut fresh,
                        );
                    }
                    publish(&mut fresh);
                }
            };
            evac_pool.scope(keepalive_helpers, &body, || {
                let mut fresh = Vec::new();
                while let Some((lo, hi)) = claim() {
                    for &addr in &keepalive[lo..hi] {
                        seed_keepalive(
                            addr,
                            &mut main_tlab,
                            &mut main_forwards,
                            &mut objs,
                            &mut bytes,
                            &mut main_deferred_self_forwarded,
                            &mut fresh,
                        );
                    }
                    publish(&mut fresh);
                }
            });
        }

        // Phase 1c (driver): the collection-overlay roots read before the
        // closure (`g1_overlay.rs`) — rows of owners outside the CSet and the
        // mark snapshot's values — and the rows of any CSet owner they copy.
        // Not re-grayed: these are roots of this pause, not marking work.
        if let Some(ov) = overlay {
            for &addr in &ov.roots {
                let Some(ridx) = self.lookup_region_for_addr(addr) else {
                    continue;
                };
                if !cset_set.contains(&ridx)
                    || !shared.candidate_ok("parallel-overlay-root", std::ptr::null_mut(), 0, addr)
                {
                    continue;
                }
                let old_ptr = addr as *mut u8;
                if let Some((new_ptr, fresh)) = shared.evacuate(
                    &mut main_tlab,
                    old_ptr,
                    &mut main_forwards,
                    &mut objs,
                    &mut bytes,
                ) {
                    if fresh {
                        let mut kids: Vec<usize> = Vec::new();
                        SharedEvac::record_fresh_child(
                            old_ptr,
                            new_ptr,
                            true,
                            &mut kids,
                            &mut main_deferred_self_forwarded,
                        );
                        shared.note_overlay_owner_copied(old_ptr, new_ptr);
                        if !kids.is_empty() {
                            outstanding.fetch_add(kids.len(), Ordering::AcqRel);
                            queue.lock().extend(kids);
                        }
                    }
                }
            }
        }

        let seed_rset_start = std::time::Instant::now();
        seed_split.0 = seed_rset_start
            .saturating_duration_since(seed_roots_start)
            .as_micros() as u64;
        // Phase 2: seed the source regions the caller selected — the CSet
        // remembered-set sources plus the JIT-pinned regions (G1AUD-6).
        //
        // THIS IS THE PHASE WHOSE COST IS O(HEAP). `g1-maturation.md` §1.3
        // says it directly: Phase 2 "walks every rset source region wholesale",
        // and the remembered set is at 1 MB region granularity with no
        // coarsening, so "region R may point into me" means "walk all of R".
        // Every byte of that walk ran on the DRIVER THREAD ALONE while the
        // whole persistent pool sat parked on a condvar — which is the single
        // largest piece of unexploited parallelism left in the pause, and it is
        // the piece that grows with the live heap rather than with the
        // collection set.
        //
        // The partition is by SOURCE REGION, claimed with one `fetch_add`, and
        // that is what makes it sound without a new discipline:
        //
        //   * a region index is handed to exactly one worker, and each worker
        //     rewrites slots only inside the region it claimed, so no two
        //     walkers touch the same bytes;
        //   * the card table is `AtomicU8` and a region covers a whole,
        //     disjoint run of cards, so `snapshot`/`clean_and_redirty` on two
        //     regions cannot alias;
        //   * destination allocation is the same `evacuate`/`tlab_alloc`
        //     machinery Phase 3 already runs from several workers at once;
        //   * a source that is ALSO a destination this pause (see
        //     `SharedEvac::resume`) is clamped to `pre_cursors` below, so a
        //     walker never sweeps over an object another worker is at that
        //     instant still copying in.
        //
        // The workers carry their TLABs straight into Phase 3 rather than
        // retiring them here. Retiring between the phases would abandon one
        // partly-filled region per worker per pause — the exact defect
        // `SharedEvac::resume`'s measurement table is about, re-created one
        // phase earlier.
        //
        // Default OFF behind `CRATONVM_G1_PARALLEL_SEED=1`: the seed walk is
        // the phase that reads the remembered set, and this pass has no way to
        // run the gauntlet that would license flipping it. See
        // `docs/feature-designs/concurrent-gc-maturation.md` for what
        // to measure and what to check before it becomes the default.
        let pool = self.evac_pool();
        let sources_vec: Vec<usize> = sources.iter().collect();
        // F-05: the JIT-pinned half of `sources` is walked wholesale, exactly
        // as on the serial arm — unless `CRATONVM_G1_CARD_SCREEN_JIT_PINNED`
        // says the barrier now covers it. See `card_screen_for_source`.
        // Precomputed so the seed body needs no `&self` beyond `shared`.
        let source_screens: Vec<bool> = sources_vec
            .iter()
            .map(|&i| self.card_screen_for_source(&wholesale_sources, i))
            .collect();
        let pre_cursors: Vec<usize> = sources_vec
            .iter()
            .map(|&i| unsafe { (*regions_base.0.add(i)).cursor() })
            .collect();
        const HUMONGOUS_SEED_CHUNK_SLOTS: usize = 16_384;
        enum SeedWork {
            WholeRegion { k: usize },
            HumongousChunk { meta_idx: usize, start: usize, end: usize, first: bool },
        }
        let mut seed_work: Vec<SeedWork> = Vec::with_capacity(sources_vec.len());
        let mut humongous_metas: Vec<HumongousSeedMeta> = Vec::new();
        let allow_chunked = parallel_seed_enabled() && nworkers > 1 && pool.helpers() > 0;
        for (k, &src_idx) in sources_vec.iter().enumerate() {
            if allow_chunked {
                match unsafe {
                    try_prepare_humongous_array_seed(
                        self,
                        &shared,
                        src_idx,
                        source_screens[k],
                        Some(pre_cursors[k]),
                        shared.precise_rset_liveness,
                        shared.wholesale_sources.contains(&src_idx),
                    )
                } {
                    HumongousSeedPlan::AlreadySkipped => continue,
                    HumongousSeedPlan::Split(meta) => {
                        let meta_idx = humongous_metas.len();
                        let len = meta.len;
                        humongous_metas.push(meta);
                        let mut start = 0usize;
                        while start < len {
                            let end = (start + HUMONGOUS_SEED_CHUNK_SLOTS).min(len);
                            seed_work.push(SeedWork::HumongousChunk {
                                meta_idx,
                                start,
                                end,
                                first: start == 0,
                            });
                            start = end;
                        }
                        continue;
                    }
                    HumongousSeedPlan::NotApplicable => {}
                }
            }
            seed_work.push(SeedWork::WholeRegion { k });
        }

        let seed_helpers = if parallel_seed_enabled() && seed_work.len() > 1 {
            nworkers.saturating_sub(1).min(pool.helpers())
        } else {
            0
        };
        // `helper_slots` / `worker_state`: made before Phase 1b, above.
        if seed_helpers == 0 {
            for item in &seed_work {
                match *item {
                    SeedWork::WholeRegion { k } => {
                        main_census.seed_regions += 1;
                        shared.seed_source_region(
                            sources_vec[k],
                            &mut main_tlab,
                            &mut main_forwards,
                            &mut objs,
                            &mut bytes,
                            &mut main_deferred_self_forwarded,
                            source_screens[k],
                            None,
                        );
                    }
                    SeedWork::HumongousChunk { meta_idx, start, end, first } => {
                        if first {
                            main_census.seed_regions += 1;
                        }
                        unsafe {
                            seed_humongous_array_chunk(
                                &shared,
                                &mut main_tlab,
                                &mut main_forwards,
                                &mut objs,
                                &mut bytes,
                                &mut main_deferred_self_forwarded,
                                &humongous_metas[meta_idx],
                                start,
                                end,
                            );
                        }
                    }
                }
            }
        } else {
            let seed_cursor = AtomicUsize::new(0);
            let shared_ref = &shared;
            let state_ref = &worker_state;
            let sources_ref = &sources_vec;
            let screens_ref = &source_screens;
            let cursors_ref = &pre_cursors;
            let work_ref = &seed_work;
            let metas_ref = &humongous_metas;
            let claim = || -> Option<usize> {
                let k = seed_cursor.fetch_add(1, Ordering::Relaxed);
                (k < work_ref.len()).then_some(k)
            };
            let seed_body = |i: usize| {
                let mut slot = state_ref[i].lock();
                let (tlab, shard) = &mut *slot;
                shard.census.pauses = 1;
                while let Some(w_idx) = claim() {
                    match work_ref[w_idx] {
                        SeedWork::WholeRegion { k } => {
                            shard.census.seed_regions += 1;
                            unsafe {
                                shared_ref.seed_source_region(
                                    sources_ref[k],
                                    tlab,
                                    &mut shard.forwards,
                                    &mut shard.objs,
                                    &mut shard.bytes,
                                    &mut shard.deferred,
                                    screens_ref[k],
                                    Some(cursors_ref[k]),
                                );
                            }
                        }
                        SeedWork::HumongousChunk { meta_idx, start, end, first } => {
                            if first {
                                shard.census.seed_regions += 1;
                            }
                            unsafe {
                                seed_humongous_array_chunk(
                                    shared_ref,
                                    tlab,
                                    &mut shard.forwards,
                                    &mut shard.objs,
                                    &mut shard.bytes,
                                    &mut shard.deferred,
                                    &metas_ref[meta_idx],
                                    start,
                                    end,
                                );
                            }
                        }
                    }
                }
            };
            pool.scope(seed_helpers, &seed_body, || {
                while let Some(w_idx) = claim() {
                    match seed_work[w_idx] {
                        SeedWork::WholeRegion { k } => {
                            main_census.seed_regions += 1;
                            shared.seed_source_region(
                                sources_vec[k],
                                &mut main_tlab,
                                &mut main_forwards,
                                &mut objs,
                                &mut bytes,
                                &mut main_deferred_self_forwarded,
                                source_screens[k],
                                Some(pre_cursors[k]),
                            );
                        }
                        SeedWork::HumongousChunk { meta_idx, start, end, first } => {
                            if first {
                                main_census.seed_regions += 1;
                            }
                            unsafe {
                                seed_humongous_array_chunk(
                                    &shared,
                                    &mut main_tlab,
                                    &mut main_forwards,
                                    &mut objs,
                                    &mut bytes,
                                    &mut main_deferred_self_forwarded,
                                    &humongous_metas[meta_idx],
                                    start,
                                    end,
                                );
                            }
                        }
                    }
                }
            });
        }
        for meta in &humongous_metas {
            unsafe { finish_humongous_array_seed(self, meta) };
        }
        // THE SEED/CLOSURE SPLIT, LATCHED HERE AND NOWHERE ELSE.
        //
        // `objs`/`bytes` are running totals across Phases 1, 1b and 2 for the
        // driver and across Phase 2 for a helper. Phase 3 keeps adding to the
        // same accumulators, so the only place the two halves can be told
        // apart is this line — before the closure dispatch and after the seed.
        // Without the split every pause reads as driver-heavy, because the
        // driver is the only thread that can seed roots and keep-alives at
        // all, and that would make the census answer a question nobody asked.
        main_census.seed_objs = objs as u64;
        main_census.seed_bytes = bytes as u64;
        for slot in &worker_state {
            let mut slot = slot.lock();
            let (_, shard) = &mut *slot;
            shard.census.seed_objs = shard.objs as u64;
            shard.census.seed_bytes = shard.bytes as u64;
        }

        seed_split.1 = seed_rset_start.elapsed().as_micros() as u64;
        // Phase 3: parallel transitive closure.
        //
        // The helper threads are PERSISTENT (see [`cratonvm_gc_core::evac_pool`]). This
        // used to open a `std::thread::scope` and spawn them here, which put
        // thread creation inside every pause; `EvacPool::scope` keeps the same
        // shape — dispatch, driver participates, barrier — with the spawn paid
        // once for the collector's life instead. The barrier is what replaces
        // scope's join as the synchronisation point after which the driver may
        // dereference the regions guard again, so everything the module's
        // SAFETY MODEL note says about that boundary still holds verbatim.
        // DO NOT WAKE THE POOL FOR A CLOSURE THAT HAS NOTHING TO CLOSE OVER.
        //
        // `outstanding` at this point is exactly what Phases 1/1b/2 seeded.
        // Zero is not a rare case: it is every pause whose collection set held
        // no object any root, keep-alive or remembered-set source reaches — an
        // all-garbage young generation, which is the shape a healthy
        // allocation-heavy workload produces most of the time. Dispatching
        // anyway costs a `notify_all`, `helpers` thread wake-ups, `helpers`
        // condvar re-acquisitions and the barrier, for `helpers` workers that
        // each load `outstanding`, see 0, retire two empty TLABs and park
        // again. That is a scheduler round trip per worker added to the
        // shortest pauses the collector has, and it scales with the worker
        // count — so it lands hardest on exactly the machines the ergonomic
        // sizes widest.
        let helpers = if outstanding.load(Ordering::Acquire) == 0 {
            0
        } else {
            nworkers.saturating_sub(1).min(pool.helpers())
        };
        // The per-helper slots are the ones Phase 2 may already have filled —
        // its TLABs carry through, so a helper resumes the destination region
        // it was packing rather than abandoning it partly filled.
        {
            let shared_ref = &shared;
            let state_ref = &worker_state;
            let body = move |i: usize| {
                let mut slot = state_ref[i].lock();
                let (tlab, shard) = &mut *slot;
                // SAFETY: identical contract to the driver's own `run_worker`
                // call below — the regions guard is held by the driver for the
                // whole dispatch and the disjointness discipline in the module
                // SAFETY MODEL note governs every region access inside.
                shard.census.pauses = 1;
                unsafe {
                    shared_ref.run_worker(
                        tlab,
                        &mut shard.objs,
                        &mut shard.bytes,
                        &mut shard.forwards,
                        &mut shard.deferred,
                        &mut shard.census,
                    );
                }
            };
            pool.scope(helpers, &body, || {
                // The driver participates as a worker, reusing its seeded TLAB
                // and accumulators.
                unsafe {
                    shared.run_worker(
                        &mut main_tlab,
                        &mut objs,
                        &mut bytes,
                        &mut main_forwards,
                        &mut main_deferred_self_forwarded,
                        &mut main_census,
                    );
                }
            });
        }
        // RETIRE EVERY HELPER TLAB, INCLUDING THE ONES NO WORKER RAN.
        //
        // `run_worker` retires its own on the way out, so for a helper that ran
        // both phases this is a no-op (`retire_tlab` takes `region_idx`, which
        // is already `None`). It is here for the helper that ran the SEED and
        // then was NOT dispatched into the closure — which happens whenever the
        // seed found no CSet-bound reference at all, because the Phase-3 width
        // above is zero when `outstanding` is zero. Such a helper is holding a
        // destination region whose live cursor has never been written back, and
        // every object it placed sits ABOVE that cursor: invisible to the
        // Phase-4 fix-up walk, invisible to the next pause's linear walk, and
        // about to be overwritten by the next TLAB that resumes the region.
        // Unconditional because this is the kind of invariant that must not
        // depend on which of two widths a pause happened to pick.
        for slot in &worker_state {
            let mut slot = slot.lock();
            unsafe {
                shared.retire_all(&mut slot.0);
            }
        }
        // CHECKPOINT 1 — the parallel closure has ended and every worker has
        // joined, so anything the watch reports here was written by a WORKER
        // (or the driver acting as one) while the closure ran.
        if let Some(w) = copy_watch {
            w.verify("after the parallel closure", arena.clone(), None);
        }
        // PUBLISH THE ENGAGEMENT CENSUS — here, and not one line later.
        //
        // This is the last point at which the per-worker split still exists:
        // the loop below folds every shard into the driver's accumulators and
        // the serial self-forward drain after it adds driver-only work on top.
        // A census taken after either would report the shape of the MERGE
        // rather than the shape of the pause. See [`EvacWorkerCensus`].
        main_census.objs_copied = (objs as u64).saturating_sub(main_census.seed_objs);
        main_census.bytes_copied = (bytes as u64).saturating_sub(main_census.seed_bytes);
        let mut rows: Vec<EvacWorkerCensus> = Vec::with_capacity(worker_state.len() + 1);
        rows.push(main_census);
        // g1ep3: on the sorted map arm the workers' forward lists are not
        // appended to the driver's; the self-forward scan and the map build
        // read them where they lie (`shard_lists`), in the merged list's order.
        let sorted_map = true;
        let mut shard_lists: Vec<Vec<(usize, usize)>> = Vec::new();
        if sorted_map {
            shard_lists.reserve(worker_state.len());
        } else {
            let shard_pairs: usize = worker_state.iter().map(|s| s.lock().1.forwards.len()).sum();
            main_forwards.reserve(shard_pairs);
        }
        for slot in worker_state {
            let (_, mut shard) = slot.into_inner();
            shard.census.objs_copied = (shard.objs as u64).saturating_sub(shard.census.seed_objs);
            shard.census.bytes_copied =
                (shard.bytes as u64).saturating_sub(shard.census.seed_bytes);
            rows.push(shard.census);
            objs += shard.objs;
            bytes += shard.bytes;
            if sorted_map {
                shard_lists.push(shard.forwards);
            } else {
                main_forwards.extend_from_slice(&shard.forwards);
                self.give_forward_buffer(shard.forwards);
            }
            main_deferred_self_forwarded.extend(shard.deferred);
        }
        publish_evac_worker_census(&rows);
        // The driver's own pairs; the drain below appends after them.
        let driver_len = main_forwards.len();

        // The identity forward itself is the authoritative signal that an
        // evacuation-failed object stayed in its CSet region. Derive the serial
        // drain set from the merged forward shards as a backstop for every
        // caller path, rather than relying only on the side-channel populated
        // when a caller observes `fresh && old == new`.
        //
        // FIXPOINT: the serial drain can itself self-forward children (pool
        // exhaustion mid-drain), so re-derive the identity-forward set and
        // re-drain until no new work appears — every LIVE self-forwarded
        // holder gets its slots rewritten before Phase 4/5.
        //
        // DEAD objects in kept regions are deliberately NOT scanned (an
        // earlier revision walked every object of every kept region here;
        // under sustained evacuation failure that resurrected the kept
        // regions' garbage wholesale each pause — the dead holders' targets
        // were evacuated as if live — and OOMed the SteadyChurn recreation).
        // Their slots may retain stale cross-cycle references, which is safe:
        // a stale reference can only point into a region that is Free, a
        // this-pause destination, or a mutator-reused region — never a
        // CSet-resident live object — so nothing consults it as a live edge.
        let mut serial_tlab = TlabSet::default();
        let mut drained = 0usize;
        // Cursors and membership set carried ACROSS the fixpoint rounds; see
        // `append_self_forwarded_from_forwards_incremental` for why the
        // one-shot form's per-round rebuild was the wrong cost curve here.
        let mut sf_forwards_from = 0usize;
        let mut sf_deferred_from = 0usize;
        let mut sf_seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        // g1ep3: the workers' lists, kept apart on the sorted arm, are scanned
        // once, after the driver's pairs and before any the drain adds -- the
        // order the merged list had. They do not grow.
        if !self.no_self_forward_this_pause() {
            if !shard_lists.is_empty() {
                SharedEvac::append_self_forwarded_from_forwards_incremental(
                    &main_forwards,
                    &mut sf_forwards_from,
                    &mut sf_deferred_from,
                    &mut sf_seen,
                    &mut main_deferred_self_forwarded,
                );
                for list in &shard_lists {
                    let mut from = 0usize;
                    SharedEvac::append_self_forwarded_from_forwards_incremental(
                        list,
                        &mut from,
                        &mut sf_deferred_from,
                        &mut sf_seen,
                        &mut main_deferred_self_forwarded,
                    );
                }
            }
            loop {
                SharedEvac::append_self_forwarded_from_forwards_incremental(
                    &main_forwards,
                    &mut sf_forwards_from,
                    &mut sf_deferred_from,
                    &mut sf_seen,
                    &mut main_deferred_self_forwarded,
                );
                if drained >= main_deferred_self_forwarded.len() {
                    break;
                }
                unsafe {
                    shared.drain_deferred_self_forwarded(
                        &mut serial_tlab,
                        &mut main_forwards,
                        &mut objs,
                        &mut bytes,
                        &mut main_deferred_self_forwarded,
                        &mut drained,
                    );
                }
            }
        }
        // g1pin: the drain's copies are to-space holders too; their edges
        // join the closure's, and a real failure in the drain marks them
        // incomplete, as in a worker.
        if shared.tospace_edges {
            serial_tlab.tospace.drain_into(&mut tospace.lock());
        }
        unsafe {
            shared.retire_all(&mut serial_tlab);
        }

        // CHECKPOINT 2 — the serial self-forward drain has run. A mismatch that
        // is new since checkpoint 1 was written by the DRAIN, not by a worker;
        // one that was already there is the closure's. Two checkpoints is the
        // smallest split that decides which half to read.
        if let Some(w) = copy_watch {
            w.verify("after the serial self-forward drain", arena.clone(), None);
        }

        // g1sz: the closure-derived to-space edges, only when the closure and
        // the self-forward drain settled every slot of every to-space holder.
        // g1pin: a real evacuation failure marks them incomplete where it
        // happens; a pin's drain only adds its copies' edges. See
        // `ClosureTospaceEdges`.
        let tospace_edges = if shared.tospace_edges {
            let t = std::mem::take(&mut *tospace.lock());
            (!t.incomplete).then_some(t.edges)
        } else {
            None
        };

        // Merge the per-worker forward shards into the pointer map consumed by
        // the VM root remap and Phases 4/5.
        //
        // SIZED BY OBJECTS, NOT BY EDGES (lane D wave 2, 2026-09-20).
        //
        // `main_forwards` has one entry per RECORDED FORWARD, and three of the
        // four arms that record one are deliberate duplicates: the
        // already-forwarded fast path pushes on every incoming reference to a
        // shared object, and both CAS-loser arms push a pair the winner also
        // pushed. That is correct and load-bearing — DEFECT-2 and the
        // 2026-08-26 "SECOND SITE" fix are both about an arm that did NOT
        // push, and `pointer_map` is a map, so duplicates cost nothing but
        // space. What it means is that `main_forwards.len()` is |E|, while the
        // map it sizes holds |V| — for a typical Java graph a 1-2x
        // over-allocation of a `HashMap`, on the largest single allocation the
        // pause makes.
        //
        // The evacuator already counts |V| exactly, in two pieces: `objs` is
        // every object COPIED (incremented only on the CAS-winner copy arm),
        // and the self-forward set is every object the evacuation-FAILURE arm
        // installed an identity forward for — the one class of key that has no
        // copy behind it and so is missing from `objs`. Their sum is an upper
        // bound on the distinct keys, and `min` with the edge count keeps it
        // from ever being the larger of the two guesses.
        //
        // This is `lane-d-small-findings-not-fixed.md` item 1's CONSUMER half.
        // The per-worker `Vec` is still sized by |E| and still should be: the
        // dedup that would fix it needs a per-worker set, which is the
        // allocation this design exists to avoid, and the Vec is
        // `(usize, usize)` pairs rather than hash table buckets.
        // g1sz: the build's own clock, for `pointer_map_us` on the
        // `[GC] g1 pause-phases:` line (it is inside `closure_us`).
        let map_build_start = std::time::Instant::now();
        let map_cap = objs
            .saturating_add(main_deferred_self_forwarded.len())
            .min(main_forwards.len());
        // g1sz: the hashed arms' pre-sized map, made only when one runs (the
        // sorted arm would have allocated sixteen shards for nothing).
        let hashed_map = || {
            cratonvm_types::PointerMap::with_capacity_and_hasher(map_cap, Default::default())
        };
        let mut pointer_map: cratonvm_types::PointerMap;
        // JIT round 23 wave 6, lane L4 (`CRATONVM_G1_PAR_POINTER_MAP_MERGE`,
        // default ON, `=0` kills): the fold runs on the evacuation pool, one
        // shard range per worker (`PointerMap::par_extend_pairs_with`, the
        // generational heap's `map_merge` since gen-gc-five), instead of one
        // hash insert per forward on the driver -- a million-plus inserts on a
        // pause that copies a large survivor set (`R23W3Keep obj -Xmx1g`: 3.5 M
        // objects in the first pause). Same answer: later pairs win, exactly
        // as the sequential loop.
        let merge_threads = (pool.helpers() + 1).min(cratonvm_types::pointer_map::SHARDS);
        if sorted_map {
            let threads = pool.helpers() + 1;
            let mut sources: Vec<&[(usize, usize)]> = Vec::with_capacity(shard_lists.len() + 2);
            sources.push(&main_forwards[..driver_len]);
            sources.extend(shard_lists.iter().map(Vec::as_slice));
            sources.push(&main_forwards[driver_len..]);
            pointer_map = sorted_pointer_map_by_region(
                &sources,
                self.arena_base,
                self.region_shift,
                regions_len,
                threads,
                &|jobs, job| {
                    let next = AtomicUsize::new(0);
                    let body = |_helper: usize| loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= jobs {
                            break;
                        }
                        job(i);
                    };
                    pool.scope(jobs.saturating_sub(1), &body, || body(0));
                },
                Some(&self.sorted_buffers),
            );
            drop(sources);
            for list in shard_lists {
                self.give_forward_buffer(list);
            }
            self.give_forward_buffer(main_forwards);
        } else if merge_threads > 1
            && main_forwards.len() >= 4096
        {
            let sources = [main_forwards];
            pointer_map = hashed_map();
            pointer_map.par_extend_pairs_with(&sources, merge_threads, |n, job| {
                if n > 1 {
                    let body = |helper: usize| job(helper + 1);
                    pool.scope(n - 1, &body, || job(0));
                } else {
                    (0..n).for_each(job);
                }
            });
        } else {
            pointer_map = hashed_map();
            for (o, n) in main_forwards {
                pointer_map.insert(o, n);
            }
        }

        // DEFECT-2 FIX (part 2 of 2): restore the evacuator's
        // "`forwarding_ptr == 0` at collection start" invariant for EVERY
        // from-space object forwarded this cycle, not just the self-forwarded
        // ones. The loop, and the whole argument for preserving the shape
        // quartet while clearing the tag, now live in ONE place --
        // `retire_forwards` -- rather than in a private copy per arm.
        //
        // WHERE it runs is the 2026-09-06 correction. This retirement used to
        // happen HERE, at the end of the evacuator, while the three serial
        // drivers run theirs between Phase 4 and Phase 5. Two things sit
        // between those two points.
        //
        // Phase 3.5, `resurrect_dead_finalizers`, re-enters the SERIAL
        // `evacuate_object` for a dead finalizable's whole transitive closure,
        // and since F-02 that function's only "already evacuated?" test is the
        // forwarding tag in the from-space mark word. Retiring the tags first
        // told Phase 3.5 that every object this pause had already copied was
        // untouched: it copied each one AGAIN and overwrote the correct
        // `pointer_map` entry with the duplicate, splitting the object's
        // identity. Nothing crashed, because `retire_forwards` preserves the
        // quartet and the duplicate is a well-formed object.
        //
        // And Phase 3.5 installs forwards of its OWN. Nothing after this point
        // retired anything, so those outlived the pause in a kept region, where
        // the next cycle's fast path reads them as a this-cycle answer.
        //
        // `CRATONVM_G1_RETIRE_FORWARDS_LATE=0` puts it back here for a bisect.

        self.phase_totals
            .note_pointer_map_build(map_build_start.elapsed().as_micros() as u64);
        (pointer_map, objs, bytes, tospace_edges)
    }

    /// Parallel young-only collection (Step 9). Behaviour-equivalent to
    /// [`Self::young_collection`]'s serial body — Phases 1–3 run through the
    /// multi-threaded evacuator, Phases 4/5 + stats mirror the serial path.
    pub fn young_collection_parallel(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let start = std::time::Instant::now();
        let mut regions = self.regions.write();
        // SECURITY FIX (V7a): invalidate every mutator's RSet fast-path cache
        // before any reclassification (see `rset_cache_epoch`).
        self.bump_rset_cache_epoch();
        // Item 1 -- a pause's span marks belong to that pause.
        self.clear_humongous_references();
        // G1AUD-9: a destination cursor from the previous pause names a region
        // this one may free, retype, or put in the CSet. Drop both before any
        // reclassification so the first placement of each type does not start
        // from a hint that is guaranteed to miss.
        self.reset_evac_dest_hints();

        // Same conservative-JIT-root region exclusion as the serial path:
        // objects held by un-rewritable JIT register/spill slots must not
        // move. Empty (free) unless a thread is in JIT, and the young path
        // only dispatches here when none is — this keeps the invariant even
        // if that gate is ever loosened or the fn is called directly.
        let jit_pinned_regions = self.pause_pinned_region_set(&regions, roots);
        // The same `[g1][PINS]` line the serial path prints. It was missing
        // here, and the parallel evacuator is the DEFAULT path — so an
        // investigation into why the card screen engages so little under a warm
        // JIT (a JIT-pinned source is walked wholesale, screen bypassed) could
        // not see the pin set on the arm that actually runs. A diagnostic that
        // only prints on the path nobody takes is a diagnostic that does not
        // exist.
        if self.gc_log_enabled.load(Ordering::Relaxed) && !jit_pinned_regions.is_empty() {
            eprintln!(
                "[g1][PINS] young pause (parallel): {}",
                self.describe_pin_set(&regions, &jit_pinned_regions),
            );
        }
        if gc_flags().g1_dbg_pins {
            eprintln!(
                "[g1][PINS] young pause (parallel): jit_active={} pin_addrs={} \
                 pin_regions={:?} {}",
                cratonvm_gc_core::gc_quiescence::is_active(),
                cratonvm_gc_core::gc_quiescence::pinned_jit_root_count(),
                jit_pinned_regions,
                self.describe_pin_set(&regions, &jit_pinned_regions),
            );
            // gci i9/fa: the same pin set, by publisher, summed for the
            // `[GC] g1 pin-attribution:` line.
            self.note_pin_attribution(&regions, &jit_pinned_regions);
        }
        let cset: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                !r.pinned
                    && !jit_pinned_regions.contains(i)
                    && (r.region_type() == RegionType::Eden
                        || r.region_type() == RegionType::Survivor)
            })
            .map(|(i, _)| i)
            .collect();
        // JIT round 23 wave 3, lane L4: the young-source prerequisite --
        // every young region this pause leaves out of its set is walked for
        // its outgoing edges before anything below reads a remembered set.
        {
            let in_cset: RegionSet = cset.iter().copied().collect();
            self.record_excluded_young_sources(&mut regions, &|i| in_cset.contains(&i));
        }
        if cset.is_empty() {
            // OBSERVABILITY PARITY with the serial driver (G1-7). A pause that
            // declines to collect because every young region was pinned out is
            // still a pause, and the serial path records it with
            // `EMPTY_COLLECTION_SET` plus whichever pin excluded the regions.
            // The parallel path returned here silently, so under parallel
            // evacuation an operator watching G1 fail to reclaim saw NO cycle
            // at all rather than one saying why — the exact blindness
            // `record_g1_cycle` exists to remove. Surfaced by
            // `a_pinned_region_is_never_evacuated` the moment parallel
            // evacuation became the default.
            let (jni_pinned_out, jit_pinned_out) =
                count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);
            let mut degraded = cratonvm_gc_core::gc_metrics::g1_degraded::EMPTY_COLLECTION_SET
                | cratonvm_gc_core::gc_metrics::g1_degraded::PARALLEL_EVACUATOR;
            if jni_pinned_out > 0 {
                degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::JNI_PINNED_REGIONS_EXCLUDED;
            }
            if jit_pinned_out > 0 {
                degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::JIT_PINNED_REGIONS_EXCLUDED;
            }
            cratonvm_gc_core::gc_metrics::record_g1_cycle(
                cratonvm_gc_core::gc_metrics::g1_cycle_kind::YOUNG,
                0,
                0,
                (jni_pinned_out + jit_pinned_out) as u32,
                0,
                degraded,
            );
            // gci i32/ec (`i30-rp-report.md` hand-back 1): every young region
            // pinned out is still an in-place pause; arm the kept-region
            // verdict (`note_refproc_empty_cset_pause`).
            self.record_kept_young_regions_empty_cset(&regions);
            self.note_refproc_empty_cset_pause(jni_pinned_out + jit_pinned_out);
            return GcResult {
                stats: GcStats {
                    objects_copied: 0,
                    bytes_copied: 0,
                    bytes_freed: 0,
                },
                pointer_map: cratonvm_types::PointerMap::default(),
            };
        }
        let cset_set: RegionSet = cset.iter().copied().collect();
        // Compute before the parallel driver starts placing to-space objects:
        // its worker TLAB cursors are intentionally unpublished until retire.
        let precise_rset_liveness = self
            .native_alloc_pressure
            .load(Ordering::Relaxed)
            .then(|| self.low_space_rset_source_liveness(&regions))
            .flatten();
        let (pool, pool_committed) = self.parallel_to_space_pool(&regions);

        // F-04 — G1AUD-11's pre-evacuation `(region_type, cursor)` snapshot,
        // which until now only the SERIAL young path took.
        //
        // The narrowing argument in `phase4_regions_to_walk` is a property of
        // the remembered set and of what the pause wrote into; nothing in it is
        // specific to a single-threaded evacuator. Passing `None` here meant
        // the DEFAULT path always took the whole-heap fix-up while the fallback
        // path took the narrow one — so the two dispatch arms differed in the
        // one phase that scales with the live heap rather than the collection
        // set, and any comparison between them was measuring that instead of
        // the evacuator.
        //
        // One pass over two words per region, on a path that already makes
        // several such passes.
        let pre_evac: Vec<(RegionType, usize)> = regions
            .iter()
            .map(|r| (r.region_type(), r.cursor()))
            .collect();

        // Marking keep-alive (see `marking_keepalive_roots`): computed while
        // the guard is still dereferenceable, passed to the evacuator as
        // extra roots, and re-grayed after Phase 5 below.
        let (mut keepalive, keepalive_satb) = self.marking_keepalive_roots_split(&regions, &cset_set);
        // g1pin: the peer-stack pinned objects are roots too, after the
        // SATB-sourced prefix (`append_peer_object_pin_roots`).
        self.append_peer_object_pin_roots(&mut keepalive, &cset_set);

        // G1AUD-6 (defect G1-9) — the source set, built EXACTLY as the serial
        // `young_collection` builds it (`live_rset_sources` + every JIT-pinned
        // region), while the guard is still dereferenceable.
        //
        // The parallel evacuator used to derive this set itself from the CSet
        // remembered sets alone, omitting the `unique_sources.extend(
        // jit_pinned_regions)` term the serial path has carried since the
        // original CSet-straddle UAF fix. That is not a lost optimisation: a
        // JIT-pinned region is EXCLUDED from the CSet (so it is never traced as
        // a from-space object) and is reached only as a remembered-set source,
        // so any CSet object referenced from it that the barrier failed to
        // record was neither evacuated nor rewritten — its holder's slot then
        // pointed into a region Phase 5 zero-filled.
        //
        // `jit_pinned_region_set()` is NOT gated on `gc_quiescence::is_active()`
        // (see its INT-3 note): it also contains every region holding a
        // published un-retired TLAB tail, so this set is routinely non-empty on
        // a multi-threaded pause with NO thread in JIT — which is precisely the
        // configuration G1-9's live-object corruption was reproduced under
        // (`SteadyChurn @16m --nojit`, smallest heap, ~1 run in 8). Whether or
        // not it is the whole of that defect, the serial/parallel divergence is
        // real and the fail-safe direction is to walk MORE sources, never
        // fewer.
        let parallel_sources: RegionSet = {
            let mut set = Self::live_rset_sources(&regions, &cset);
            set.extend(jit_pinned_regions.iter());
            set
        };

        // G1AUD-10 / F-07 — per-phase breakdown on the parallel driver too.
        // The parallel evacuator FUSES phases 1, 1b, 2 and 3 into one
        // work-stealing closure (roots and rset sources are seeds of the same
        // queue), so there is no instant at which "the root scan is done" is a
        // fact about the pause. Reporting the whole seed+closure as
        // `closure_us` and leaving `roots_us`/`rset_us` at zero says exactly
        // that, and keeps the six phases a partition of `pause_us`; splitting
        // one measured interval into three plausible-looking numbers would not.
        //
        // gci i14/yt: superseded in part. The DRIVER's seeding is sequential
        // before the closure dispatch -- Phases 1-1c (roots, keep-alive,
        // overlay roots) then Phase 2 (rset sources, parallel only under
        // `CRATONVM_G1_PARALLEL_SEED=1`, still before Phase 3) -- so those two
        // instants are facts, and `parallel_evacuate` now measures them
        // (`seed_split`). `roots_us` / `rset_us` carry the seed and
        // `closure_us` the rest (`G1PausePhases::set_parallel_evacuation_us`).
        // What the note above says stays true of Phase 3: a seed's copies
        // spill work into the closure, so `rset_us` is the walk plus the
        // direct copies, as on the serial arm. The controllers read
        // `fused_closure_us`, the old figure.
        let mut phases = G1PausePhases::default();
        // PARITY (2026-09-06): these four were filled on the SERIAL arm only,
        // so on the DEFAULT (parallel) arm every `[GC-STAT]` line reported
        // `free_regions=0 eden_regions=0 surv_regions=0 old_regions=0
        // hum_regions=0 cset_regions=0 jni_pinned_out=0 jit_pinned_out=0` --
        // an all-zero census that reads as a fact about the heap and is a fact
        // about the instrument. The field docs already describe the previous
        // incarnation of exactly this bug ("An instrument armed where it cannot
        // fire"); the fix for it landed on one of the two arms.
        {
            let (jni_pinned_out, jit_pinned_out) =
                count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);
            phases.cset_regions = cset.len() as u32;
            phases.jni_pinned_out = jni_pinned_out as u32;
            phases.jit_pinned_out = jit_pinned_out as u32;
        }
        let mut phase_mark = std::time::Instant::now();

        // Take the raw regions base; do NOT deref `regions` again until after
        // `parallel_evacuate` returns (see the module SAFETY MODEL note).
        let regions_len = regions.len();
        let regions_base = RegionsBase(regions.as_mut_ptr());
        let active_evacuation_cset = self.activate_evacuation_cset(&cset_set);
        // F-05 — the parallel arm reports seed+closure as one `closure_us`, so
        // the card-screen counters are diffed around the whole call rather than
        // around a separate Phase 2.
        // Overlay rows read before the closure, so the workers seed and
        // expand them (`g1_overlay.rs`).
        let overlay_prepass = self.overlay_prepare(&cset_set, true);
        let card_screen_snapshot = self.card_scan_snapshot();
        // gci i14/yt: `(roots_us, rset_us)` of the seed, split out of
        // `closure_us` below.
        let mut seed_split = (0u64, 0u64);
        let (pointer_map, objects_copied, bytes_copied, mut tospace_edges) = unsafe {
            self.parallel_evacuate(
                regions_base,
                regions_len,
                &cset_set,
                pool,
                pool_committed,
                roots,
                &keepalive,
                &parallel_sources,
                &jit_pinned_regions,
                precise_rset_liveness.as_ref(),
                Some(&overlay_prepass),
                &mut seed_split,
            )
        };
        drop(active_evacuation_cset);
        self.fill_card_scan_phases(&mut phases, card_screen_snapshot);
        // Phase 3.5 — finalizer resurrection (F-01). The serial driver runs
        // this between the closure and the fix-up, and it was the last real
        // reason the dispatch in `young_collection` forced serial. It needs
        // nothing the parallel path cannot give it: the regions guard is held
        // again here, the shards are merged into `pointer_map`, and every
        // worker TLAB has been retired (its cursor written back), so the
        // serial `evacuate_object` it uses finds correct region state. A
        // candidate that no worker forwarded has an untouched mark word, which
        // is exactly the from-space object this expects.
        let mut pointer_map = pointer_map;
        // g1sz: the closure's own copies; anything Phase 3c / 3.5 adds was not
        // scanned by the closure (`drop_closure_settled_tospace_from_fixup`).
        let mut closure_objects_copied = objects_copied;
        let mut objects_copied = objects_copied;
        let mut bytes_copied = bytes_copied;
        {
            // Phase 3c: collection-overlay edges — see young_collection_serial.
            // Serial, after the parallel closure, in the state Phase 3.5 needs.
            let mut overlay = self.overlay_pause_begin(
                &mut regions,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                overlay_prepass,
            );
            // g1ep7: on the evacuation pool, a closure of its own
            // (`resurrect_dead_finalizers_parallel`); the serial body otherwise.
            match self.resurrect_dead_finalizers_parallel(
                &mut regions,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut tospace_edges,
            ) {
                Some(copied) => closure_objects_copied += copied,
                None => {
                    let mut resurrect_list: Vec<*mut u8> = Vec::new();
                    self.resurrect_dead_finalizers(
                        &mut regions,
                        &cset_set,
                        &mut pointer_map,
                        &mut objects_copied,
                        &mut bytes_copied,
                        &mut resurrect_list,
                    );
                }
            }
            self.overlay_pause_follow(
                &mut overlay,
                &mut regions,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
            );
            self.overlay_pause_finish(overlay, &regions, &pointer_map);
        }
        phases.set_parallel_evacuation_us(phase_mark.elapsed().as_micros() as u64, seed_split);
        phase_mark = std::time::Instant::now();

        // Phase 4: update interior refs in non-CSet regions. F-04 — narrowed
        // by the same rule the serial path uses; `parallel_sources` is
        // precisely the `narrow_sources` term (`live_rset_sources` plus every
        // JIT-pinned region) that `phase4_regions_to_walk` wants.
        let mut narrow = self.phase4_regions_to_walk(&regions, Some(&pre_evac), &parallel_sources);
        // g1sz: the regions the closure filled from empty and settled.
        self.drop_closure_settled_tospace_for_driver(
            &regions,
            &pre_evac,
            &cset_set,
            &parallel_sources,
            narrow.as_mut(),
            tospace_edges,
            objects_copied == closure_objects_copied,
        );
        // gci i9/fa: opt-in -- a pinned region that is wholly clean and that
        // this pause did not write is not walked (`drop_clean_pinned_from_fixup`).
        if fixup_skip_clean_pinned_armed(
            false,
            true,
            gc_flags().g1_card_screen_jit_pinned,
            gc_flags().g1_card_clean,
        ) {
            if let Some(set) = narrow.as_mut() {
                let (n, bytes) = self.drop_clean_pinned_from_fixup(
                    &regions,
                    &pre_evac,
                    &jit_pinned_regions,
                    set,
                );
                self.phase_totals.note_fixup_skip(n, bytes);
            }
        }
        // gci i15/rs: opt-in -- the unchanged members are walked by young card.
        let young_screen =
            self.phase4_young_card_screen_set(&regions, &pre_evac, &cset_set, narrow.as_ref());
        let census = self.update_references_in_regions_screened(
            &mut regions,
            &cset_set,
            &pointer_map,
            narrow.as_ref(),
            young_screen.as_ref(),
        );
        phases.fixup_us = phase_mark.elapsed().as_micros() as u64;
        phases.fixup_regions = census.walked_regions;
        phases.fixup_bytes = census.walked_bytes;
        phase_mark = std::time::Instant::now();

        // F-02, at the protocol point the three serial drivers use: after the
        // fix-up (which resolves the forwards) and before the reclaim (which is
        // what makes a kept region's stale forward outlive the pause). The
        // parallel drivers used to retire inside `parallel_evacuate` instead --
        // see the note there for what that cost. `pointer_map` here also
        // carries Phase 3.5's forwards, which the old placement could not.
        {
            self.retire_forwards(&pointer_map, &cset_set);
        }

        // WIRED INTO THE PARALLEL DRIVERS TOO (2026-09-07). The drain and the
        // forward verifier were on the three SERIAL bodies only, so on a
        // workload that takes the parallel arm -- which is the DEFAULT -- they
        // never ran. Measured cost of that gap: the Tomcat class this family is
        // named for produced 14 corrupt-cell reports across two runs and ZERO
        // grid verdicts, while H2 emitted 8 a run, and the difference was read
        // as a property of the workload until the wiring was checked.
        //
        // Unconditional rather than inside the `retire_forwards_late` arm: with
        // that flag OFF the retire happens early inside `parallel_evacuate`,
        // and the holders still need draining here.
        self.report_pending_corrupt_holders(&regions);
        let _ = self.count_unretired_forwards(&regions, &cset_set, &pointer_map);
        // Phase 5: free evacuated regions.
        let bytes_freed = self.free_or_keep_cset(&mut regions, &cset, &pointer_map);
        self.record_kept_young_regions_from_pre(&regions, &pre_evac);

        // Humongous spans are never evacuated, so this is the only point in a
        // young/mixed pause that can free one. It must come after Phase 5: the
        // census it consumes is only a death certificate once the CSet is gone
        // and the pause's to-space has been walked. See
        // `eager_reclaim_humongous_locked` for the full ordering argument.
        let bytes_freed = bytes_freed
            + self.eager_reclaim_humongous_locked(
                &mut regions,
                roots,
                &pointer_map,
                &census,
                &jit_pinned_regions,
            );
        phases.free_us = phase_mark.elapsed().as_micros() as u64;
        // The census, while the guard is still held -- see the field docs
        // and the PARITY note in `young_collection_parallel`.
        phases.record_region_census(&regions);
        phase_mark = std::time::Instant::now();
        self.verify_no_dangling_into_cset(&regions, &cset_set, &pointer_map, roots);
        self.dbg_verify_rset_completeness(&regions, "young-parallel");
        self.dbg_verify_no_unrewritten_forward(&regions, &cset_set, &pointer_map, roots);
        self.dbg_scan_for_zeroed_refs(&regions, &cset_set, roots);
        self.dbg_verify_reachable_integrity(&regions, roots, "young-parallel");
        phases.verify_us = phase_mark.elapsed().as_micros() as u64;

        // Re-gray the keep-alive copies (SATB-origin entries never sat in the
        // worklist, so the remap below cannot rewrite them — push their
        // relocated copies explicitly; duplicates are idempotent).
        // `CRATONVM_G1_KEEPALIVE_REGRAY_ONCE` (opt-in) applies here as on the
        // serial drivers: a gray-sourced root's entry is rewritten to its copy
        // by the remap below, so only the SATB-sourced prefix is pushed.
        let regray_once = g1_keepalive_regray_once_enabled();
        self.push_gray_or_mark_all(
            &regions,
            keepalive
                .iter()
                .enumerate()
                .filter(|&(k, _)| keepalive_needs_regray(regray_once, k, keepalive_satb))
                .filter_map(|(_, addr)| pointer_map.get(addr).copied()),
        );

        self.retire_eden_slots_in_cset(&cset_set);

        self.recompute_old_gen_bytes(&regions);
        self.publish_occupancy_metrics(&regions);

        monitors.remap_after_gc(&pointer_map);
        // INT-8: carry the referent-slot skip set across this pause
        // (re-key survivors, prune CSet casualties) — same protocol
        // point as the monitor-table remap. No-op outside a mark cycle.
        self.remap_reference_skip_set(&cset_set, &pointer_map);
        // The overlay mark snapshot, by the same protocol (`g1_overlay.rs`).
        self.remap_overlay_snapshot(&regions, &cset_set, &pointer_map, true);
        // G1CORE-6: same protocol point, same reason — the dedup table's
        // canonical addresses go stale on exactly the objects this pause moved.
        self.remap_string_dedup_table(&cset_set, &pointer_map);

        // Remap (or drop) stale concurrent-mark worklist entries — identical to
        // the serial young path. Done under STW (guard held) so no marker step
        // races us.
        self.remap_gray_set_after_pause(&regions, &cset_set, &pointer_map);

        let pause_us = start.elapsed().as_micros() as u64;
        phases.close(pause_us);
        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };
        self.record_collection_with_phases(G1CollectionType::YoungOnly, pause_us, &stats, phases);
        let (jni_pinned_out, jit_pinned_out) =
            count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);
        cratonvm_gc_core::gc_metrics::record_g1_cycle(
            cratonvm_gc_core::gc_metrics::g1_cycle_kind::YOUNG,
            cset.len() as u32,
            0,
            (jni_pinned_out + jit_pinned_out) as u32,
            // G1AUD-6: the parallel path now reports the sources it walked,
            // like the serial one. A hard 0 here made the two paths' cycle
            // records incomparable, which is exactly how the missing
            // JIT-pinned source term stayed invisible.
            parallel_sources.len() as u32,
            self.g1_pause_degraded_flags_for(&pointer_map, jni_pinned_out, jit_pinned_out, true),
        );
        GcResult { stats, pointer_map }
    }

    /// LANE W6-P test hook — run the PARALLEL mixed driver on this collector
    /// regardless of `CRATONVM_G1_PARALLEL_MIXED`.
    ///
    /// # Why a hook and not the flag
    ///
    /// [`parallel_mixed_enabled`] latches its answer in a `OnceLock` on first
    /// read, so ONE process can only ever take one of the two arms. Every
    /// measurement this lever has ever been given was therefore a comparison
    /// between two DIFFERENT runs — which is the shape that has been retracted
    /// three times this round, because on a workload that takes no mixed pause
    /// the two runs are identical and the "difference" is host noise.
    ///
    /// An equivalence test needs both drivers over ONE fixture inside ONE
    /// process: the same heap, built by the same allocation sequence, driven
    /// once serially and once in parallel, with the surviving graph and the
    /// set of evacuated objects compared afterwards. That is what this exists
    /// for, and it is the same `#[doc(hidden)] pub` shape as
    /// [`Self::with_regions_mut`], for the same reason — a test in a separate
    /// crate cannot reach a `pub` body, and routing the test through
    /// the env lever would make the test a test OF the env lever.
    ///
    /// **Not for production callers.** The dispatcher in
    /// [`Self::mixed_collection`] also checks `use_parallel_evacuator`
    /// (quiescence, the JIT veto); this hook checks nothing. See
    /// `docs/feature-designs/concurrent-gc-maturation.md`.
    #[doc(hidden)]
    pub fn mixed_collection_parallel_forced(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        self.mixed_collection_parallel(roots, monitors)
    }

    /// Parallel mixed collection (Step 9). Behaviour-equivalent to
    /// [`Self::mixed_collection`]'s serial body (including the Step-7
    /// pause-target old-region CSet sizing and the mixed-cycle bookkeeping).
    pub fn mixed_collection_parallel(
        &self,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let start = std::time::Instant::now();
        let mut regions = self.regions.write();
        self.bump_rset_cache_epoch();
        // Item 1 -- a pause's span marks belong to that pause.
        self.clear_humongous_references();
        // G1AUD-9: a destination cursor from the previous pause names a region
        // this one may free, retype, or put in the CSet. Drop both before any
        // reclassification so the first placement of each type does not start
        // from a hint that is guaranteed to miss.
        self.reset_evac_dest_hints();

        // Build CSet: all young regions + worst (most-garbage) old regions,
        // bounded by both the percentage cap and the Step-7 pause budget —
        // identical selection to the serial `mixed_collection`.
        // Conservative-JIT-root region exclusion — see young_collection_parallel.
        let jit_pinned_regions = self.pause_pinned_region_set(&regions, roots);
        let mut cset: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                !r.pinned
                    && !jit_pinned_regions.contains(i)
                    && (r.region_type() == RegionType::Eden
                        || r.region_type() == RegionType::Survivor)
            })
            .map(|(i, _)| i)
            .collect();
        // Young half of the CSet, captured before the old members are pushed —
        // the `record_g1_cycle` young/old split needs both counts.
        // JIT round 23 wave 3, lane L4: the young-source prerequisite --
        // every young region this pause leaves out of its set is walked for
        // its outgoing edges before anything below reads a remembered set.
        {
            let in_cset: RegionSet = cset.iter().copied().collect();
            self.record_excluded_young_sources(&mut regions, &|i| in_cset.contains(&i));
        }
        let cset_young = cset.len();

        // Old half: the same selector, census and arms as the serial driver
        // (gci i11/gm finding 3). This driver is never handed a compaction plan
        // (`mixed_collection` routes here only without one).
        // gcd d10/o — a full-compaction pause takes its own candidates, uncapped.
        let full_compaction = self.full_compaction_this_pause.load(Ordering::Relaxed);
        let old_source = if full_compaction {
            OldCsetSource::FullCompaction {
                young_bytes: cset.iter().map(|&i| regions[i].cursor()).sum(),
            }
        } else {
            OldCsetSource::Mixed
        };
        let old_choice = self.choose_old_cset(
            &regions,
            &jit_pinned_regions,
            old_source,
            g1_pause_cost_model(),
        );
        self.note_old_cset_census(&old_choice, 0);
        self.end_mixed_phase_if_only_pinned_left(&old_choice);
        let old_selected = old_choice.picked.len();
        cset.extend(old_choice.picked.iter().copied());

        if cset.is_empty() {
            // OBSERVABILITY PARITY with the serial driver (G1-7). A pause that
            // declines to collect because every young region was pinned out is
            // still a pause, and the serial path records it with
            // `EMPTY_COLLECTION_SET` plus whichever pin excluded the regions.
            // The parallel path returned here silently, so under parallel
            // evacuation an operator watching G1 fail to reclaim saw NO cycle
            // at all rather than one saying why — the exact blindness
            // `record_g1_cycle` exists to remove. Surfaced by
            // `a_pinned_region_is_never_evacuated` the moment parallel
            // evacuation became the default.
            let (jni_pinned_out, jit_pinned_out) =
                count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);
            let mut degraded = cratonvm_gc_core::gc_metrics::g1_degraded::EMPTY_COLLECTION_SET
                | cratonvm_gc_core::gc_metrics::g1_degraded::PARALLEL_EVACUATOR;
            if jni_pinned_out > 0 {
                degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::JNI_PINNED_REGIONS_EXCLUDED;
            }
            if jit_pinned_out > 0 {
                degraded |= cratonvm_gc_core::gc_metrics::g1_degraded::JIT_PINNED_REGIONS_EXCLUDED;
            }
            cratonvm_gc_core::gc_metrics::record_g1_cycle(
                cratonvm_gc_core::gc_metrics::g1_cycle_kind::MIXED,
                0,
                0,
                (jni_pinned_out + jit_pinned_out) as u32,
                0,
                degraded,
            );
            // gci i32/ec (`i30-rp-report.md` hand-back 1): every young region
            // pinned out is still an in-place pause; arm the kept-region
            // verdict (`note_refproc_empty_cset_pause`).
            self.record_kept_young_regions_empty_cset(&regions);
            self.note_refproc_empty_cset_pause(jni_pinned_out + jit_pinned_out);
            // g1ep5: and it is still one of the mixed phase's pauses. The
            // serial body has no early return, so it spends the count at its
            // tail on an empty CSet too; this one returned before its tail,
            // and a phase whose pauses all came back empty never closed.
            self.spend_mixed_count(full_compaction);
            return GcResult {
                stats: GcStats {
                    objects_copied: 0,
                    bytes_copied: 0,
                    bytes_freed: 0,
                },
                pointer_map: cratonvm_types::PointerMap::default(),
            };
        }
        let cset_set: RegionSet = cset.iter().copied().collect();
        let (pool, pool_committed) = self.parallel_to_space_pool(&regions);

        // Marking keep-alive (see `marking_keepalive_roots` and the note in
        // the serial `mixed_collection`).
        let (mut keepalive, keepalive_satb) = self.marking_keepalive_roots_split(&regions, &cset_set);
        // g1pin: the peer-stack pinned objects are roots too, after the
        // SATB-sourced prefix (`append_peer_object_pin_roots`).
        self.append_peer_object_pin_roots(&mut keepalive, &cset_set);

        // Item 2 — pre-evacuation snapshot for the narrow fix-up.
        let pre_evac: Vec<(RegionType, usize)> = regions
            .iter()
            .map(|r| (r.region_type(), r.cursor()))
            .collect();

        // G1AUD-6 — same source set the serial `mixed_collection` builds; see
        // the long note in `young_collection_parallel`.
        let parallel_sources: RegionSet = {
            let mut set = Self::live_rset_sources(&regions, &cset);
            set.extend(jit_pinned_regions.iter());
            set
        };

        // G1AUD-10 / F-07 — phase breakdown, on the same terms as
        // `young_collection_parallel`: the evacuator fuses phases 1-3, so the
        // whole seed+closure is reported as `closure_us`.
        let mut phases = G1PausePhases::default();
        // PARITY (2026-09-06): the mixed drivers left these three at zero, so a
        // mixed pause's `[GC-STAT]` line said `cset_regions=0` for a pause that
        // by definition collected some. See `young_collection_parallel`. The
        // pin counts are the YOUNG-region ones, which is exactly what this
        // path's own `record_g1_cycle` already reports -- an old region kept
        // out of a mixed CSet is not counted by either.
        {
            let (jni_pinned_out, jit_pinned_out) =
                count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);
            phases.cset_regions = cset.len() as u32;
            phases.jni_pinned_out = jni_pinned_out as u32;
            phases.jit_pinned_out = jit_pinned_out as u32;
        }
        let mut phase_mark = std::time::Instant::now();

        let regions_len = regions.len();
        let regions_base = RegionsBase(regions.as_mut_ptr());
        let active_evacuation_cset = self.activate_evacuation_cset(&cset_set);
        // F-05 — the parallel arm reports seed+closure as one `closure_us`, so
        // the card-screen counters are diffed around the whole call rather than
        // around a separate Phase 2.
        // Overlay rows read before the closure, so the workers seed and
        // expand them (`g1_overlay.rs`); a mixed pause reads every row.
        let overlay_prepass = self.overlay_prepare(&cset_set, false);
        let card_screen_snapshot = self.card_scan_snapshot();
        // gci i14/yt: `(roots_us, rset_us)` of the seed, split out of
        // `closure_us` below.
        let mut seed_split = (0u64, 0u64);
        let (pointer_map, objects_copied, bytes_copied, mut tospace_edges) = unsafe {
            self.parallel_evacuate(
                regions_base,
                regions_len,
                &cset_set,
                pool,
                pool_committed,
                roots,
                &keepalive,
                &parallel_sources,
                &jit_pinned_regions,
                None,
                Some(&overlay_prepass),
                &mut seed_split,
            )
        };
        drop(active_evacuation_cset);
        self.fill_card_scan_phases(&mut phases, card_screen_snapshot);
        // Phase 3.5 — finalizer resurrection; see the twin in
        // `young_collection_parallel`.
        let mut pointer_map = pointer_map;
        // g1sz: the closure's own copies; anything Phase 3c / 3.5 adds was not
        // scanned by the closure (`drop_closure_settled_tospace_from_fixup`).
        let mut closure_objects_copied = objects_copied;
        let mut objects_copied = objects_copied;
        let mut bytes_copied = bytes_copied;
        {
            // Phase 3c: collection-overlay edges — see young_collection_serial.
            // Serial, after the parallel closure, in the state Phase 3.5 needs.
            let mut overlay = self.overlay_pause_begin(
                &mut regions,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                overlay_prepass,
            );
            // g1ep7: on the evacuation pool, a closure of its own
            // (`resurrect_dead_finalizers_parallel`); the serial body otherwise.
            match self.resurrect_dead_finalizers_parallel(
                &mut regions,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
                &mut tospace_edges,
            ) {
                Some(copied) => closure_objects_copied += copied,
                None => {
                    let mut resurrect_list: Vec<*mut u8> = Vec::new();
                    self.resurrect_dead_finalizers(
                        &mut regions,
                        &cset_set,
                        &mut pointer_map,
                        &mut objects_copied,
                        &mut bytes_copied,
                        &mut resurrect_list,
                    );
                }
            }
            self.overlay_pause_follow(
                &mut overlay,
                &mut regions,
                &cset_set,
                &mut pointer_map,
                &mut objects_copied,
                &mut bytes_copied,
            );
            self.overlay_pause_finish(overlay, &regions, &pointer_map);
        }
        phases.set_parallel_evacuation_us(phase_mark.elapsed().as_micros() as u64, seed_split);
        phase_mark = std::time::Instant::now();

        // Narrow, like the serial mixed path — see the item 2 note there.
        let mut narrow =
            self.phase4_regions_to_walk(&regions, Some(&pre_evac), &parallel_sources);
        self.drop_closure_settled_tospace_for_driver(
            &regions,
            &pre_evac,
            &cset_set,
            &parallel_sources,
            narrow.as_mut(),
            tospace_edges,
            objects_copied == closure_objects_copied,
        );
        let census = self.update_references_in_regions(
            &mut regions,
            &cset_set,
            &pointer_map,
            narrow.as_ref(),
        );
        phases.fixup_us = phase_mark.elapsed().as_micros() as u64;
        phases.fixup_regions = census.walked_regions;
        phases.fixup_bytes = census.walked_bytes;
        phase_mark = std::time::Instant::now();

        // F-02 -- same protocol point as the serial mixed driver; see the note
        // on the parallel young path.
        {
            self.retire_forwards(&pointer_map, &cset_set);
        }

        // WIRED INTO THE PARALLEL DRIVERS TOO (2026-09-07). The drain and the
        // forward verifier were on the three SERIAL bodies only, so on a
        // workload that takes the parallel arm -- which is the DEFAULT -- they
        // never ran. Measured cost of that gap: the Tomcat class this family is
        // named for produced 14 corrupt-cell reports across two runs and ZERO
        // grid verdicts, while H2 emitted 8 a run, and the difference was read
        // as a property of the workload until the wiring was checked.
        //
        // Unconditional rather than inside the `retire_forwards_late` arm: with
        // that flag OFF the retire happens early inside `parallel_evacuate`,
        // and the holders still need draining here.
        self.report_pending_corrupt_holders(&regions);
        let _ = self.count_unretired_forwards(&regions, &cset_set, &pointer_map);
        let bytes_freed = self.free_or_keep_cset(&mut regions, &cset, &pointer_map);
        self.record_kept_young_regions_from_pre(&regions, &pre_evac);

        // Humongous spans are never evacuated, so this is the only point in a
        // young/mixed pause that can free one. It must come after Phase 5: the
        // census it consumes is only a death certificate once the CSet is gone
        // and the pause's to-space has been walked. See
        // `eager_reclaim_humongous_locked` for the full ordering argument.
        let bytes_freed = bytes_freed
            + self.eager_reclaim_humongous_locked(
                &mut regions,
                roots,
                &pointer_map,
                &census,
                &jit_pinned_regions,
            );
        phases.free_us = phase_mark.elapsed().as_micros() as u64;
        // The census, while the guard is still held -- see the field docs
        // and the PARITY note in `young_collection_parallel`.
        phases.record_region_census(&regions);
        phase_mark = std::time::Instant::now();
        self.verify_no_dangling_into_cset(&regions, &cset_set, &pointer_map, roots);
        self.dbg_verify_rset_completeness(&regions, "mixed-parallel");
        // PARITY (2026-09-20). These two were on the OTHER FOUR drivers —
        // young-serial, young-parallel, mixed-serial and the kept-region drain
        // — and missing here alone. `mixed-parallel` is the driver that
        // evacuates OLD regions with several workers, i.e. the one whose
        // unrewritten-forward and zeroed-reference populations are hardest to
        // reason about, so the one arm with no tripwire was the arm that most
        // needed one. It also made `CRATONVM_G1_DBG_HEADERS` / `_DBG_ZERO` a
        // lie by omission: an operator who set them and ran a mixed pause under
        // `CRATONVM_G1_PARALLEL_MIXED=1` got silence, which reads as a clean
        // pause and was an instrument that was never armed.
        self.dbg_verify_no_unrewritten_forward(&regions, &cset_set, &pointer_map, roots);
        self.dbg_scan_for_zeroed_refs(&regions, &cset_set, roots);
        self.dbg_verify_reachable_integrity(&regions, roots, "mixed-parallel");
        phases.verify_us = phase_mark.elapsed().as_micros() as u64;

        // Re-gray the keep-alive copies (see young_collection_parallel).
        let regray_once = g1_keepalive_regray_once_enabled();
        self.push_gray_or_mark_all(
            &regions,
            keepalive
                .iter()
                .enumerate()
                .filter(|&(k, _)| keepalive_needs_regray(regray_once, k, keepalive_satb))
                .filter_map(|(_, addr)| pointer_map.get(addr).copied()),
        );

        self.retire_eden_slots_in_cset(&cset_set);

        self.recompute_old_gen_bytes(&regions);
        self.publish_occupancy_metrics(&regions);

        monitors.remap_after_gc(&pointer_map);
        // INT-8: carry the referent-slot skip set across this pause
        // (re-key survivors, prune CSet casualties) — same protocol
        // point as the monitor-table remap. No-op outside a mark cycle.
        self.remap_reference_skip_set(&cset_set, &pointer_map);
        // The overlay mark snapshot, by the same protocol (`g1_overlay.rs`).
        self.remap_overlay_snapshot(&regions, &cset_set, &pointer_map, false);
        // G1CORE-6: same protocol point, same reason — the dedup table's
        // canonical addresses go stale on exactly the objects this pause moved.
        self.remap_string_dedup_table(&cset_set, &pointer_map);

        // Remap (or drop) stale concurrent-mark worklist entries — same
        // protocol as the young paths (the mixed CSet includes Old regions,
        // where the gray set concentrates).
        self.remap_gray_set_after_pause(&regions, &cset_set, &pointer_map);

        // gci i11/gm finding 8 -- see `spend_mixed_count`.
        self.spend_mixed_count(full_compaction);

        let elapsed = start.elapsed();
        let pause_us = elapsed.as_micros() as u64;
        // LANE W5-A — routed through `update_evac_cost_for_mixed` so the
        // definition of the observation (whole pause, or the closure that did
        // the copying) lives with the model rather than in each driver.
        self.update_evac_cost_for_mixed(
            elapsed.as_nanos() as u64,
            phases.fused_closure_us(),
            bytes_copied,
        );
        phases.close(pause_us);
        let stats = GcStats {
            objects_copied,
            bytes_copied,
            bytes_freed,
        };
        // G1-7 parity: the serial mixed driver records a cycle and this one did
        // not, so under parallel mixed evacuation the `[GC] g1 cycle` stream
        // simply skipped every mixed pause — the same blindness the empty-CSet
        // arm above was fixed for, on the arm that actually reclaims old gen.
        // gci i18/mx -- the census and the degraded flags are taken BEFORE the
        // record, as in the serial driver, so the futile breaker can leave an
        // evacuation-failure or full-compaction pause out of its streak.
        let (jni_pinned_out, jit_pinned_out) =
            count_young_regions_pinned_out(regions.as_slice(), &jit_pinned_regions);
        let degraded = self.g1_pause_degraded_flags_for(&pointer_map, jni_pinned_out, jit_pinned_out, true);
        phases.evacuation_failed =
            degraded & cratonvm_gc_core::gc_metrics::g1_degraded::EVACUATION_FAILURE != 0;
        phases.compaction = full_compaction;
        self.record_collection_with_phases(G1CollectionType::Mixed, pause_us, &stats, phases);
        cratonvm_gc_core::gc_metrics::record_g1_cycle(
            cratonvm_gc_core::gc_metrics::g1_cycle_kind::MIXED,
            cset_young as u32,
            old_selected as u32,
            (jni_pinned_out + jit_pinned_out) as u32,
            parallel_sources.len() as u32,
            degraded,
        );
        GcResult { stats, pointer_map }
    }

    // -----------------------------------------------------------------------
    // Evacuation helpers
    // -----------------------------------------------------------------------

    /// Find which region a raw pointer belongs to.
    ///
    /// O(log R) via binary search on the cached `region_lookup` table.
    /// The `regions` parameter is retained for signature compatibility but
    /// is no longer scanned linearly — the per-slot evacuation hot path
    /// (CRIT-P4) used to incur an O(R) probe per reference slot, dragging
    /// scan cost to O(objects × refs × regions). Lookup is now independent
    /// of the live-region count.
    ///
    /// Note: this no longer skips `Free` regions (matching
    /// [`Self::region_for_ptr_with_regions`]). All callers either filter
    /// via the CSet (which excludes Free regions by construction) or are
    /// inherently safe against the case.
    pub fn region_for_ptr(&self, _regions: &[G1Region], ptr: *mut u8) -> Option<usize> {
        self.lookup_region_for_addr(ptr as usize)
    }

    /// Run a collection with finalizer-aware resurrection: any address in
    /// `finalizer_addrs` (registered, not-yet-enqueued finalizable objects,
    /// from `ReferenceProcessor::finalizer_referent_addresses`) that DIES in
    /// this collection's CSet is evacuated anyway — with its transitive
    /// closure — so `finalize()` can later run against valid memory, exactly
    /// like the generational backend's Phase 2.5. Returns the collection
    /// result plus the POST-copy addresses of the resurrected objects; the
    /// caller must enqueue them for finalization AND mark their processor
    /// entries enqueued (once-only finalization).
    ///
    /// Dead finalizables OUTSIDE the CSet (e.g. promoted to an Old region a
    /// young pause does not collect) are left in place, still registered —
    /// they are picked up by whichever later collection selects their
    /// region. gci i31/gq: except in a YOUNG region the pause kept in place (a
    /// JIT / JNI pin): the VM's kept-region verdict judges those after the
    /// collection and queues the dead ones at this pause
    /// (`g1_pinned_young_referents_unheld`, `CRATONVM_G1_PINNED_REFERENT_FINALIZABLES`).
    pub fn collect_garbage_with_finalizers(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
    ) -> (GcResult, Vec<usize>) {
        *self.pending_finalizer_roots.lock() = finalizer_addrs.to_vec();
        // The copy Phase 3.5 does not consume — see
        // `finalizer_addrs_this_pause`.
        *self.finalizer_addrs_this_pause.lock() = finalizer_addrs.to_vec();
        self.resurrected_finalizers.lock().clear();
        self.resurrection_closure.lock().clear();
        self.finalizer_pause
            .store(!finalizer_addrs.is_empty(), Ordering::Relaxed);
        let result = <Self as cratonvm_gc_core::collector::GarbageCollector>::collect_garbage(
            self, stw, roots, monitors,
        );
        // Belt-and-braces: clear any candidates a path did not consume
        // (e.g. an empty-CSet early return) so a later plain collection
        // never sees stale candidates.
        self.pending_finalizer_roots.lock().clear();
        self.finalizer_addrs_this_pause.lock().clear();
        self.finalizer_pause.store(false, Ordering::Relaxed);
        // gci i30/rp: the resurrection closure at its FINAL addresses, kept
        // (not drained) for the kept-young-region referent verdict
        // (`pinned_young_referents_unheld`). `collect_garbage` cleared it at
        // entry; the closure is still here (the VM drains it after this
        // returns). Composed through the final map, as `dead` below is.
        let holders: Vec<usize> = self
            .resurrection_closure
            .lock()
            .iter()
            .map(|a| result.pointer_map.get(a).copied().unwrap_or(*a))
            .collect();
        *self.resurrection_holders.lock() = holders;
        let mut dead = std::mem::take(&mut *self.resurrected_finalizers.lock());
        // `retry_after_evacuation_failure` (run inside collect_garbage,
        // after Phase 3.5) can relocate objects AGAIN via the kept-region
        // drain and composes those forwards into the final pointer map —
        // resolve each resurrected address through it so the caller never
        // enqueues an intermediate (already-vacated) address.
        for addr in dead.iter_mut() {
            if let Some(&fixed) = result.pointer_map.get(&*addr) {
                *addr = fixed;
            }
        }
        (result, dead)
    }

    /// `VmHeap::take_resurrection_closure`'s G1 arm: PRE-collection addresses
    /// of every object the last collection's Phase 3.5 evacuated that its
    /// strong closure had not (the resurrected finalizables included). The
    /// reference processor treats them as unmarked for soft and weak
    /// processing, as HotSpot does: a weak reference to an object reachable
    /// only through a finalizable is cleared before `finalize()` runs.
    /// Drained: a second call answers empty.
    pub fn take_resurrection_closure(&self) -> Vec<usize> {
        std::mem::take(&mut *self.resurrection_closure.lock())
    }

    /// Phase 3.5: evacuate dead-but-finalizable CSet objects (and their
    /// transitive closure, via the same Phase-3 scan) so `finalize()` can run
    /// against valid memory. See [`Self::collect_garbage_with_finalizers`].
    /// No-op when no candidates are pending (every plain collection).
    ///
    /// Runs on ALL FOUR evacuation drivers since F-01. It used to be serial-only,
    /// which is why the dispatch in `young_collection` forced the serial
    /// evacuator whenever candidates were pending. The parallel drivers call it
    /// at the same protocol point — after the closure, before the fix-up —
    /// with the regions guard reacquired and the worker shards already merged
    /// into `pointer_map`, so every input it reads is in the state the serial
    /// path leaves them in.
    pub fn resurrect_dead_finalizers(
        &self,
        regions: &mut Vec<G1Region>,
        cset_set: &RegionSet,
        pointer_map: &mut cratonvm_types::PointerMap,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        work_list: &mut Vec<*mut u8>,
    ) {
        let candidates = std::mem::take(&mut *self.pending_finalizer_roots.lock());
        if candidates.is_empty() {
            return;
        }
        // This phase runs AFTER the closure and evacuates through the serial
        // `evacuate_object`, whose "already evacuated?" test is the from-space
        // forwarding tag. Whether it runs at all on a given workload therefore
        // decides whether a retirement-ordering defect between the two can
        // show — and nothing counted it. See `RESURRECT_PHASE_RUNS`.
        let runs = RESURRECT_PHASE_RUNS.fetch_add(1, Ordering::Relaxed) + 1;
        let copied_before = *objects_copied;
        let mut resurrected = Vec::new();
        let scan_resume = work_list.len();
        for old_addr in candidates {
            if pointer_map.contains_key(&old_addr) {
                continue; // survived normally (or already self-forwarded)
            }
            let Some(ridx) = self.lookup_region_for_addr(old_addr) else {
                continue; // not a current heap address
            };
            if !cset_set.contains(&ridx) {
                continue; // not dying this pause — stays registered
            }
            // Dead in the CSet: evacuate it like a root and let the scan
            // below pull its subtree out too.
            if !self.evacuation_supply_is_an_object(regions, "finalizer-resurrect", old_addr) {
                continue;
            }
            if let Some((new_ptr, fresh)) = self.evacuate_object(
                regions,
                old_addr as *mut u8,
                pointer_map,
                objects_copied,
                bytes_copied,
                cset_set,
            ) {
                if fresh {
                    work_list.push(new_ptr);
                }
                resurrected.push(new_ptr as usize);
            }
        }
        // Re-run the Phase-3 closure over the resurrected subtree.
        let mut scan_idx = scan_resume;
        while scan_idx < work_list.len() {
            let obj_ptr = work_list[scan_idx];
            scan_idx += 1;
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            self.scan_and_evacuate_refs(
                regions,
                obj_ptr,
                header,
                cset_set,
                pointer_map,
                objects_copied,
                bytes_copied,
                work_list,
            );
        }
        // The depth-2 set (`take_resurrection_closure`): every object this
        // phase freshly copied is on `work_list` above `scan_resume` (a fresh
        // evacuation is always pushed, a copy the strong closure already made
        // never is), and its `pointer_map` KEY is the pre-collection address
        // the reference processor asks about. An object in an Old region
        // outside the CSet is not evacuated, and a young pause's
        // `is_addr_live` answers for it region-wide, so it is not recorded.
        // Paid only on a pause with a dead finalizable in the CSet.
        if work_list.len() > scan_resume {
            let copied: FxHashSet<usize> = work_list[scan_resume..]
                .iter()
                .map(|&p| p as usize)
                .collect();
            let closure: Vec<usize> = pointer_map
                .iter()
                .filter(|&(_, to)| copied.contains(to))
                .map(|(&from, _)| from)
                .collect();
            self.resurrection_closure.lock().extend(closure);
        }
        let copied_here = objects_copied.saturating_sub(copied_before) as u64;
        RESURRECT_PHASE_COPIED.fetch_add(copied_here, Ordering::Relaxed);
        if runs <= 8 || runs.is_power_of_two() {
            tracing::warn!(
                "[g1] Phase 3.5 resurrection RAN (#{runs}): resurrected={} copied={copied_here} \
                 — every object it copies is evacuated by the SERIAL path, which decides \
                 \"already evacuated?\" from the from-space forwarding tag alone.",
                resurrected.len(),
            );
        }
        if !resurrected.is_empty() {
            self.resurrected_finalizers.lock().extend(resurrected);
        }
    }

    /// g1ep7: [`Self::resurrect_dead_finalizers`] on the evacuation pool, for
    /// the parallel drivers (`CRATONVM_G1_PARALLEL_RESURRECT`, default ON;
    /// `None` with it off, and the caller runs the serial body).
    ///
    /// The dead finalizable collection-set objects are the roots of a second
    /// [`Self::parallel_evacuate`] closure, with no remembered-set source and
    /// a fresh to-space pool. The from-space forwards of the first closure are
    /// still installed (they are retired after Phase 4), so an object it
    /// already copied is found forwarded, as the serial body finds it. The
    /// pass's map is merged into `pointer_map` (a key both maps hold carries
    /// the same forward) and its tospace edges into `tospace_edges`; its keys
    /// the first map lacks are the resurrection closure. Returns the objects it
    /// copied, all scanned by its own closure.
    ///
    /// `ZipFsFinalizerChurn 300 60 -Xmx512m` (each dropped `ZipFileSystem`
    /// keeps its entry index for `finalize()`, about 1800 objects): 33 M
    /// objects over 546 young pauses went through the serial evacuator on the
    /// driver alone, about 125 ms of every pause's 170, and HotSpot keeps those
    /// referents alive in its parallel reference processing.
    pub fn resurrect_dead_finalizers_parallel(
        &self,
        regions: &mut Vec<G1Region>,
        cset_set: &RegionSet,
        pointer_map: &mut cratonvm_types::PointerMap,
        objects_copied: &mut usize,
        bytes_copied: &mut usize,
        tospace_edges: &mut Option<Vec<(usize, usize)>>,
    ) -> Option<usize> {
        if !g1_parallel_resurrect_enabled() {
            return None;
        }
        let candidates = std::mem::take(&mut *self.pending_finalizer_roots.lock());
        if candidates.is_empty() {
            return Some(0);
        }
        let runs = RESURRECT_PHASE_RUNS.fetch_add(1, Ordering::Relaxed) + 1;
        let mut seeds: Vec<ObjectRef> = Vec::new();
        for old_addr in candidates {
            if pointer_map.contains_key(&old_addr) {
                continue; // survived normally (or already self-forwarded)
            }
            match self.lookup_region_for_addr(old_addr) {
                Some(ridx) if cset_set.contains(&ridx) => {}
                _ => continue, // not a heap address, or not dying this pause
            }
            if !self.evacuation_supply_is_an_object(regions, "finalizer-resurrect", old_addr) {
                continue;
            }
            // SAFETY: a proved object start in a collection-set region.
            seeds.push(unsafe { ObjectRef::from_raw(old_addr as *mut u8) });
        }
        if seeds.is_empty() {
            return Some(0);
        }
        let (pool, pool_committed) = self.parallel_to_space_pool(regions);
        let regions_len = regions.len();
        let regions_base = RegionsBase(regions.as_mut_ptr());
        let active = self.activate_evacuation_cset(cset_set);
        let mut seed_split = (0u64, 0u64);
        let none = RegionSet::new();
        // SAFETY: as the first closure's call: the caller holds the regions
        // write guard and does not dereference it until this returns.
        let (map, objs, bytes, edges) = unsafe {
            self.parallel_evacuate(
                regions_base,
                regions_len,
                cset_set,
                pool,
                pool_committed,
                &mut seeds,
                &[],
                &none,
                &none,
                None,
                None,
                &mut seed_split,
            )
        };
        drop(active);
        let resurrected: Vec<usize> = seeds.iter().map(|r| r.as_ptr() as usize).collect();
        // The closure is what this pass copied: its map also names objects the
        // first closure had forwarded (it meets their forwards), and those are
        // strongly reachable.
        self.resurrection_closure.lock().extend(
            map.iter()
                .filter(|&(k, v)| k != v && !pointer_map.contains_key(k))
                .map(|(&k, _)| k),
        );
        *objects_copied += objs;
        *bytes_copied += bytes;
        let pool = self.evac_pool();
        pointer_map.compose_after_par(&map, pool.helpers() + 1, &|jobs, job| {
            let at = AtomicUsize::new(0);
            let body = |_helper: usize| loop {
                let i = at.fetch_add(1, Ordering::Relaxed);
                if i >= jobs {
                    break;
                }
                job(i);
            };
            pool.scope(jobs.saturating_sub(1), &body, || body(0));
        });
        match (tospace_edges.as_mut(), edges) {
            (Some(all), Some(more)) => all.extend(more),
            _ => *tospace_edges = None,
        }
        RESURRECT_PHASE_COPIED.fetch_add(objs as u64, Ordering::Relaxed);
        if runs <= 8 || runs.is_power_of_two() {
            tracing::warn!(
                "[g1] Phase 3.5 resurrection RAN (#{runs}): resurrected={} copied={objs} \
                 -- on the evacuation pool (CRATONVM_G1_PARALLEL_RESURRECT).",
                resurrected.len(),
            );
        }
        self.resurrected_finalizers.lock().extend(resurrected);
        Some(objs)
    }

}
