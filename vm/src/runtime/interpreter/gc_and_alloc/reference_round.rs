#[allow(unused_imports)]
use super::*;

/// zt t31/zc11: the body of the VM reference round (`process_references_after_gc`
/// after its loader / class-unload reconciliation), under `prep`'s guards.
pub(in crate::runtime::interpreter) fn reference_round(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    pause_relocations: &crate::memory::addr_keyed::RelocationTargets<'_>,
    prep: ReferenceRoundPrep<'_>,
    // zt t31/zc11: the round runs after the release (2c): no slot is
    // restored (see the restore loop).
    after_release: bool,
) {
    let ReferenceRoundPrep {
        free_mb,
        refproc_prepass,
        reference_next_slot,
        enqueued_sentinel,
        class_manager,
        reference_cid,
        queue_cid,
        mut ref_proc,
    } = prep;
    // The verdicts `process_references_after_gc` captured for its head, again
    // for this body (two cheap captures over the same relocation set).
    let in_place = crate::memory::addr_keyed::InPlaceVerdict::capture_with(
        &shared.mem.heap,
        pause_relocations,
    );
    let claimed = |addr: usize| -> bool {
        !pointer_map.contains_key(&addr) && in_place.claimed_by_relocation(addr)
    };
    // gci i27/gr (`jr20-g1-young-pause-keeps-every-referent-in-a-jit-pinned-region-FIXED-20261016`,
    // default ON, `CRATONVM_G1_PINNED_REFERENT_VERDICT=0` kills): the weak /
    // phantom `Reference` objects and referents a G1 pause kept in place in a
    // young region (a JIT / JNI pin, an evacuation failure) that NOTHING holds
    // after the pause. `is_marked` below answers them dead; empty (no work
    // past one load) off G1 and on every pause that kept no young region.
    // gci i28/wt: taken once per pause by `run_collection_pause`, before this
    // round (it takes the processor lock briefly itself, then walks the heap
    // with no VM lock held), for these rows AND the JNI weak globals, and
    // carried on the pause's relocation set; the JNI sweep and the side-table
    // sweeps above read it through `in_place.survived`.
    // addr-keyed: this pause's PRE-collection addresses, consumed by its own round below.
    let g1_pinned_unheld = pause_relocations.g1_unheld_set();

    // An object is "marked" (survived GC) if:
    // 1. It appears in the pointer map (evacuated/copied during collection), OR
    // 2. It resides in a live (non-collected) heap region (G1: Old/Humongous regions
    //    that weren't in the collection set are still live).
    // Every address reaching this predicate belongs to the reference
    // processor and was therefore published as watched by
    // `weakref_null_referents_pre_gc` — so the exact predicate applies. It
    // only differs from the permissive `is_addr_live` when this cycle
    // reclaimed old-gen storage, in which case an old-gen address absent from
    // the pointer map genuinely did not survive (see
    // `VmHeap::watched_pre_gc_addr_survived`). Fall back to the permissive
    // form when the pre-GC publication pass is switched off, since then no
    // watch set was published and no identity entries were emitted.
    //
    // gcd d10/r: and never an address this pause relocated ANOTHER object
    // onto (`claimed`). `watched_pre_gc_addr_survived` answers ZGC's unmoved
    // addresses with its exact registry test, which after a slide proves that
    // SOME object starts there, not that it is the one this row names: a
    // referent or `Reference` that died at a base the slide re-issued read as
    // live, the weak reference was not cleared, and only the restore screen's
    // `relocation_targets` below caught it a round late.
    //
    // gci i27/gr: and never an address G1's per-object verdict found unheld in
    // a young region the pause kept in place (`g1_pinned_unheld`, above): the
    // region-granular G1 arm of `watched_pre_gc_addr_survived` answers "live"
    // for every object below such a region's cursor.
    let is_marked = |addr: usize| -> bool {
        if after_release {
            if shared.mem.heap.is_zgc() {
                return shared.mem.heap.zgc_is_marked_addr(addr);
            }
        }
        if g1_pinned_unheld.contains(&addr) {
            return false;
        }
        if weakref_clear_enabled() {
            shared
                .mem
                .heap
                .watched_pre_gc_addr_survived(addr, pointer_map)
                && !claimed(addr)
        } else {
            pointer_map.contains_key(&addr)
                || (shared.mem.heap.is_addr_live(addr) && !claimed(addr))
        }
    };

    // gengc-round3 (lane `refdriver`, 2026-09-21): retire the rows whose
    // `Reference` OBJECT did not survive BEFORE the phases run, not only after
    // them.
    //
    // WHAT IT FIXES.
    // `process_references_with_finalizer_trace` seeds `soft_live` — the
    // closure that decides whether a `WeakReference` may be cleared — from
    // every soft entry Phase 1 left alone. A `SoftReference` INSTANCE that
    // died in this same cycle still has a row here (the tail
    // `remove_collected` below is what drops it, and that is too late), so its
    // referent entered `soft_live` and a `WeakReference` aimed at the same
    // object was left uncleared for the cycle: `get()` handing back storage
    // the collector is entitled to reclaim. Weak references are the one
    // reference type whose contract is exactly that this cannot happen.
    // `gengc-mark-dead-softref-object-still-roots-weak-closure-FIXED-20260923.md`.
    //
    // WHY A REORDERING RATHER THAN A PREDICATE. Two rounds tried to screen the
    // soft-survivor roots inside `process_references` and both broke the same
    // three tests, because `is_marked` is "the collector marked this address
    // in THIS cycle" and a caller may pass a predicate for which that is not
    // "this object still exists". A row that is gone needs no predicate.
    //
    // WHY THE NARROW ENTRY POINT AND NOT `remove_collected` ITSELF. That one
    // prunes all five lists, and `finalizer_refs` hold rows the VM registered
    // as `reference_obj == referent == the finalizable object`
    // (`SharedVm::register_finalizable`, which `java/lang/ref/Finalizer.register`
    // reaches through `register_if_finalizable` since gc-common w5-d), while a
    // `cleaner_refs` row's object is the synthetic `Cleaner$Cleanable`. For those rows "the reference object
    // did not survive" IS "the object is dead" — the condition under which the
    // row must FIRE — so pruning them here would switch finalization off
    // instead of running it. `remove_collected_reference_objects` touches only
    // soft/weak/phantom, whose `reference_obj` is always a real
    // `java.lang.ref.Reference`; this is the same list split, for the same
    // reason, that `retain_shaped_weak_phantom` already makes below.
    //
    // WHAT CHANGES OBSERVABLY. A soft/weak/phantom/cleaner whose own instance
    // died this cycle no longer clears, enqueues or emits an action. Every one
    // of those emissions was already declined further down this function by
    // `is_stale_young` (which is `!is_marked` plus the young rule), so the
    // heap writes are unchanged; what changes is that the dead soft row stops
    // rooting `soft_live`. It also matches HotSpot, which never DISCOVERS an
    // unreachable `Reference` and so never processes one.
    //
    // WAS GENERATIONAL ONLY; EVERY BACKEND SINCE gc-common w1-d (2026-09-23).
    // The 2026-09-21 gate kept G1 and ZGC on the old sequence because the
    // reordering belonged to that wave's generational lane. On those two
    // backends the pre-pass was close to inert anyway: every UNCLEARED
    // soft/weak/phantom row was a synthetic GC root
    // (`pending_reference_object_addresses`), so its `Reference` could not be
    // dead when the phases ran. That rooting is now narrowed to queued,
    // unsettled rows, so a queue-less `SoftReference` the program dropped is
    // dead on every backend — and on G1/ZGC its row would otherwise seed
    // `soft_live` and keep a `WeakReference` to a dead referent uncleared, the
    // exact hazard this pre-pass exists for. `remove_collected` is unchanged;
    // `gc/src/zgc.rs`'s own tail call is on its private, empty processor and
    // does not see this one. `CRATONVM_GC_NO_REFPROC_PREPASS=1` restores the
    // old order everywhere.
    #[cfg(feature = "zgc")]
    let is_zgc_after_release_sync =
        after_release && shared.mem.heap.is_zgc();
    #[cfg(not(feature = "zgc"))]
    let is_zgc_after_release_sync = false;
    #[cfg(feature = "zgc")]
    let zgc_sync_is_marked = |addr: usize| -> bool {
        shared.mem.heap.is_zgc() && shared.mem.heap.zgc_is_marked_addr(addr)
    };
    let mut deferred_weak_prune = false;
    if refproc_prepass {
        if is_zgc_after_release_sync && ref_proc.weak_refs_len() >= 32_768 {
            ref_proc.remove_collected_non_weak_reference_objects(&is_marked);
            deferred_weak_prune = true;
        } else if is_zgc_after_release_sync {
            #[cfg(feature = "zgc")]
            ref_proc.remove_collected_reference_objects_sync(&zgc_sync_is_marked);
        } else {
            ref_proc.remove_collected_reference_objects(&is_marked);
        }
    }

    // gc-common w1-d (2026-09-23): the rows the pre-collection pass examined
    // and found ALREADY null — the application called `Reference.clear()`, a
    // bare slot store the registry never hears about — or refused to touch
    // (a recycled address). Retire them before the phases can clear-and-
    // enqueue a reference the program cancelled. See
    // `ReferenceProcessor::retire_entries_the_pre_gc_pass_found_cleared`.
    // Gated like the pass that feeds it: with `CRATONVM_WEAKREF_CLEAR=0` no
    // pass ran and the examined set is empty.
    if weakref_clear_enabled() {
        let retired = ref_proc.retire_entries_the_pre_gc_pass_found_cleared();
        if retired != 0 && dbg_weakref() {
            eprintln!(
                "[weakref] retired {retired} weak/soft reference(s) the application \
                 had already cleared (or whose address was recycled)"
            );
        }
    }

    // The `0` third argument is deliberate, not a second hardcode: when the
    // caller passes 0, `gc::reference` substitutes the mutator clock it
    // observes through `touch_soft_reference` (`last_observed_clock_ms`).
    // gcg g3/x: `-Xlog:gc+ref=debug` / `gc+phases+ref=debug`, rendered here
    // and printed after the pause by the collection's own lines.
    let xlog_ref_t0 = crate::runtime::unified_logging::xlog_reference_lines_wanted()
        .then(std::time::Instant::now);
    // zr z2/cp: under the last-ditch clear-all rule on ZGC, a condemned soft
    // referent kept alive only by a conservatively scanned compiled-frame word
    // is not "strongly reachable" for this decision
    // (`zgc_soft_clear_all_conservative_referents`).
    //
    // zr z5/rv5: for the SOFT decision only. This set used to be folded into
    // `is_marked` for every phase, so the same live object also had its weak
    // references cleared and its `Cleaner` / phantoms fired in the same round
    // -- a native free under a `DirectByteBuffer` the compiled frame still
    // uses -- while a FINALIZABLE referent became a finalizer root whose
    // closure kept the soft reference after all.
    // `CRATONVM_ZGC_SOFT_CLEAR_SOFT_PHASE_ONLY=0` restores the folded
    // predicate.
    let soft_forced_dead = zgc_soft_clear_all_conservative_referents(
        shared,
        &ref_proc,
        crate::runtime::interpreter::soft_clear_all_applied_take(),
    );
    // zt t10/gn2 (`CRATONVM_ZGC_GEN_ARMED_MAJOR_SOFT_CLOCK`, default ON, `=0`
    // kill; ZGC generational mode only, a no-op on every other collector): a
    // collection the pre-collection pass armed as young that ran whole-heap
    // (`young_refs_mispredicted`) recorded no "now" for the soft clock, so
    // this round would leave `SoftReference.clock` where the last major put
    // it; HotSpot's major advances it. Taken first, so the kill still
    // consumes the note.
    //
    // zt t12/x9: the same misprediction also ends the (default ON since t20)
    // `CRATONVM_ZGC_GEN_YOUNG_HOLDS_SOFT_FREE` hold the pre-collection pass
    // set from its (young) prediction: this round ends a MAJOR, which stores
    // its own free-at-last-gc figure, as HotSpot's major does.
    if shared.mem.heap.zgc_take_armed_major_soft_clock_due() {
        ref_proc.set_young_round_holds_soft_free(false);
        {
            ref_proc.note_round_clock_now();
        }
    }
    let result = if soft_forced_dead.is_empty() {
        if is_zgc_after_release_sync {
            #[cfg(feature = "zgc")]
            {
                ref_proc.process_references_sync(
                    &zgc_sync_is_marked,
                    deferred_weak_prune,
                    free_mb,
                    0,
                )
            }
            #[cfg(not(feature = "zgc"))]
            {
                ref_proc.process_references(&is_marked, free_mb, 0)
            }
        } else {
            ref_proc.process_references(&is_marked, free_mb, 0)
        }
    } else {
        if deferred_weak_prune {
            #[cfg(feature = "zgc")]
            ref_proc.remove_collected_weak_refs_sync(&zgc_sync_is_marked);
        }
        ref_proc.note_soft_phase_forced_dead(soft_forced_dead.iter().copied());
        ref_proc.process_references(&is_marked, free_mb, 0)
    };
    // zs s7/jf: the ZGC collection's `jdk.GCReferenceStatistics` (soft, weak,
    // final, phantom), the same per-type figures as the `gc+ref` census
    // below: the references this pass found with an unreachable referent. A
    // no-op off ZGC and under `CRATONVM_ZGC_JFR_REF_AND_YOUNG_EVENTS=0`.
    //
    // g1gen/c1b: on G1 and Generational the same counts go to the VM's slot
    // for this collection's JFR write (`CRATONVM_GC_JFR_REFERENCE_STATISTICS`,
    // default ON): HotSpot 25 writes the four rows per collection on every
    // collector (`ReferenceProcessorStats`, `GCTracer::report_gc_reference_stats`).
    {
        let s = &result.stats;
        let w = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        let counts = [
            w(s.soft_refs_cleared),
            w(s.weak_refs_cleared),
            // zt t2/jf (X1): resurrected finalizables are flagged after this
            // round (`ReferenceProcessingStats` doc): count them here.
            w(s.finalizer_refs_enqueued
                .saturating_add(s.finalizer_refs_resurrected)),
            w(s.phantom_refs_enqueued
                .saturating_add(s.cleaner_refs_processed)),
        ];
        if shared.mem.heap.is_zgc() {
            if super::gc_events::zgc_jfr_ref_and_young_enabled() {
                shared.mem.heap.zgc_note_jfr_reference_counts(counts);
            }
        } else if super::gc_events::gc_jfr_reference_statistics_enabled() {
            // Summed until the collection's JFR write takes them: one
            // collection may run more than one round (a generational
            // `System.gc()`'s young and old passes), and HotSpot reports the
            // collection's references once.
            let mut slot = shared.debug.jfr_gc_reference_counts.lock();
            let sum = slot.unwrap_or([0; 4]);
            *slot = Some(std::array::from_fn(|i| sum[i].saturating_add(counts[i])));
        }
    }
    // gci i27/gr (`gci-i15rf-...-20261014` residual, opt-in
    // `CRATONVM_G1_FULL_GC_CARRIES_UNDISCOVERED_REFS`): on G1, carry the rows
    // this round left alone as undiscovered (the stop-the-world probe's
    // verdict) to the final remark of the same full collection, valid while
    // the heap's collection count stays where this pause left it. See
    // `ReferenceProcessor::carry_probe_undiscovered` and
    // `g1_remark_carried_undiscovered_references`. Switch off: no carry.
    if shared.mem.heap.is_g1() && g1_full_gc_carries_undiscovered_refs_enabled() {
        ref_proc.carry_probe_undiscovered(pointer_map, shared.mem.heap.collection_count());
    } else {
        ref_proc.drop_carried_undiscovered();
    }
    // gcj j4/rf (`gcj-j1rf-reference-processor-review-residuals` item 2): the
    // census is built here but handed to the log after the restore pass below,
    // which can still clear a weak / soft row (`clear_after_refused_restore_of`)
    // after `result.stats` was copied; those clears join it there.
    let mut xlog_ref_census: Option<crate::runtime::unified_logging::RefProcCensus> = None;
    if let Some(t0) = xlog_ref_t0 {
        let s = &result.stats;
        let w = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        let this_round = [
            w(s.soft_refs_cleared),
            w(s.weak_refs_cleared),
            // gci i11/rp (R1): the finalizables this collection resurrected
            // are HotSpot's discovered-and-processed FinalReferences; their
            // rows are flagged by the channel after this round, so
            // `finalizer_refs_enqueued` alone read 0 on every backend.
            w(s.finalizer_refs_enqueued
                .saturating_add(s.finalizer_refs_resurrected)),
            w(s.phantom_refs_enqueued
                .saturating_add(s.cleaner_refs_processed)),
        ];
        let total_ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let census = crate::runtime::unified_logging::RefProcCensus {
            discovered: this_round,
            // gci i11/rp (R9): HotSpot's `Dropped` counts references the
            // MARKER discovered (referent unmarked when it visited the
            // `Reference`) whose referent a later edge of the same
            // collection marked -- traversal-order dependent. This VM has
            // no marker-time discovery (every active referent is hidden
            // before the mark), so it cannot tell "visited first" from
            // "referent reached first"; counting every live hidden
            // referent would print the boot's live `WeakHashMap` /
            // `ThreadLocal` entries that HotSpot's full collection does
            // not report (the g3 `x3_ref_phases` row: `WeakReference
            // Discovered: 1, Dropped: 0`, SAME). `0` is HotSpot's answer
            // whenever the marker reaches a live referent before (or
            // without) its `Reference`, its usual full-GC shape; a young
            // collection's `Dropped: N` is not reproduced.
            dropped: [0; 4],
            processed: this_round,
            total_ns,
            phase_ns: [total_ns, 0, 0],
            phase1_kind_ns: [0, 0, 0],
        };
        xlog_ref_census = Some(census);
    }
    // The census is emitted after the restore pass (see `xlog_ref_census`).
    let emit_ref_census = |ref_proc: &mut cratonvm_gc::reference::ReferenceProcessor| {
        // Taken on every pause, so a pause without the log does not leave its
        // late clears to the next one that has it.
        let late = ref_proc.take_late_cleared();
        let Some(mut census) = xlog_ref_census else {
            return;
        };
        if census_counts_late_clears_enabled() {
            for (kind, n) in late.into_iter().enumerate() {
                census.discovered[kind] = census.discovered[kind].saturating_add(n);
                census.processed[kind] = census.processed[kind].saturating_add(n);
            }
        }
        // gci i25/sg (`gci-i24rv-...-review-findings-20261015` item 16): a
        // pause folded into a G1 one-report full collection hands its census
        // to the report, which prints ONE block under its own id; elsewhere
        // (the default) the block is deferred as before.
        if !super::gc_events::g1_full_scope_absorb_ref_census(shared, false, &census) {
            // gci i15/rf (G-7): a G1 evacuation pause's block is HotSpot G1's
            // `Post Evacuate Collection Set` one, two `Indents` steps deeper.
            crate::runtime::unified_logging::defer_xlog_reference_processing_at(
                shared
                    .mem
                    .gc_cycle_count
                    .load(std::sync::atomic::Ordering::Relaxed),
                &census,
                reference_block_indent_levels(shared),
            );
        }
    };

    // bc math-ec 0x4 ROOT-CAUSE FIX (2026-06-09, hexdump-proven): the
    // cleared/enqueue lists hold PRE-GC addresses; `pointer_map.get(..)
    // .unwrap_or(addr)` keeps the STALE address for a Reference that was
    // RECLAIMED this cycle (a live young object is ALWAYS in the pointer map
    // after a moving young GC). Writing through that stale address corrupts
    // whatever now occupies the memory: the measured corruption was THIS
    // loop's `Object(None)` referent-clear landing mis-gridded — victim
    // payload = 0x4 (the Object discriminant), next word nulled (hexdump in
    // h2-testscript-segv-findings.md). The earlier
    // `num_fields < 2` guard was too weak (a phantom header at the stale
    // address can read num_slots >= 2). PRECISE criterion: a pre-GC address
    // in EITHER young semispace that is NOT a pointer-map key did not
    // survive — skip it entirely. Old-gen addresses don't move in a minor GC
    // (major relocations ARE merged into the map) and stay processed.
    // Backend-generic since 2026-07-10 (`pre_gc_addr_did_not_survive`): the
    // original closure checked only the Generational young semispaces, so it
    // was hardwired inert for G1/ZGC — dead finalize/cleaner/enqueue
    // addresses flowed through unguarded and `run_finalizers` later
    // dereferenced freed CSet memory (finalize-on-recycled-object UAF).
    // OLD-GEN RECLAMATION FIX (HIB-CV-32 family,
    // `TestMVStoreCachePerformance`): `pre_gc_addr_did_not_survive` has the
    // same old-generation blind spot `is_addr_live` had — for the Generational
    // heap it only rejects YOUNG addresses absent from the pointer map, and
    // answers "survived" for every old-gen address on the grounds that a minor
    // GC does not move old gen. That stops being true the moment the SAME
    // cycle reclaims old-gen storage (mark-compact `major_gc`, or the in-place
    // `sweep_old_gen_non_moving`): the pre-GC address of a reclaimed old-gen
    // Reference / queue / cleaner action now names the zeroed compaction tail,
    // a live object slid onto it, or a free block. The cleared/enqueue writes
    // below then landed on an innocent occupant, and — worse — the finalize
    // and cleaner loops handed that address to `run_finalizers` /
    // `run_cleaner_actions`, which INVOKE Java methods on it.
    //
    // Every address that reaches this predicate belongs to the reference
    // processor and was published as watched by
    // `weakref_null_referents_pre_gc`, so `watched_pre_gc_addr_survived` is
    // exact for it (both old-gen paths emit an identity `pointer_map` entry
    // for watched survivors). OR the two verdicts: a "did not survive" from
    // either predicate declines the write, which is always the safe
    // direction, and keeps the young rule exactly as strict as before (the
    // `bc math-ec 0x4` fix). Falls back to the young-only rule when the
    // pre-GC publication pass is off, since then nothing was watched.
    //
    // gcd d10/r: an address this pause relocated another object onto
    // (`claimed`: never a map key, so a moved object's own pre-collection
    // address is never condemned by it) is stale on either arm.
    let is_stale_young = |addr: usize| -> bool {
        let young_stale = shared
            .mem
            .heap
            .pre_gc_addr_did_not_survive(addr, pointer_map)
            || claimed(addr);
        if !weakref_clear_enabled() {
            return young_stale;
        }
        young_stale
            || !shared
                .mem
                .heap
                .watched_pre_gc_addr_survived(addr, pointer_map)
    };

    // SHAPE GUARD (H2 `TestMultiThread`, 2026-08-16). Everything on the
    // processor's cleared / to-enqueue lists is a `java.lang.ref.Reference` and
    // a `java.lang.ref.ReferenceQueue` by construction — so if the object now
    // sitting at the recorded address is neither, the address no longer names
    // what the processor recorded and every write below would land on an
    // innocent occupant.
    //
    // `num_fields >= 2` was the only shape test the two loops had, and it is a
    // coincidence rather than a check: a `java.lang.String` has four
    // (`value`, `coder`, `hash`, `hashIsZero`) and passes it, as does almost
    // every other class. Both failure modes were measured, one run apart, from
    // concurrent `DriverManager.getConnection` on this suite:
    //
    // * the ENQUEUE loop published the reusing object as the queue head and
    //   `ReferenceQueue.poll()` handed it straight back —
    //   `ClassCastException: class java.lang.String cannot be cast to class
    //   org.h2.util.CloseWatcher` out of `CloseWatcher.pollUnclosed`, and the
    //   same cast against `sun.nio.ch.FileLockTable$FileLockReference` out of
    //   `FileLockTable.removeStaleEntries` — two unrelated JDK/app call sites,
    //   one bug;
    // * the CLEARED loop nulled field 0, which on a `String` is `value`, a
    //   `byte[]` — surfacing far away as `NullPointerException: Cannot read the
    //   array length because "<local4>" is null`.
    //
    // `None` (the class not loaded) means no Reference object can exist yet, so
    // the guard has nothing to judge and admits — it must never be the thing
    // that silently stops reference processing on a stripped image.
    //
    // Both guards screen the KIND first, and the `num_fields >= 2` test above
    // them is why: an array MIRRORS ITS LENGTH into `num_slots`, so a
    // `Reference[2]` reports two "fields" and — because a reference array
    // carries its COMPONENT's class id — also answers `is_subclass_of(
    // java/lang/ref/Reference)`. It would pass both tests and reach the
    // positional `get_field(obj, 0)` / `get_field(obj, 1)` reads below, which
    // stride packed 8-byte elements as 16-byte `Value` cells. Same species as
    // `corrupt-value-cell-producer-was-a-string-array-FIXED-20260822`; the heap
    // accessors refuse it now, but a door that can answer "not a Reference"
    // for free should not make the heap say it.
    let is_reference_shaped = |obj: ObjectRef| -> bool {
        if shared.mem.heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array {
            return false;
        }
        match reference_cid {
            Some(cid) => class_manager.is_subclass_of(shared.mem.heap.class_id_of(obj), cid),
            None => true,
        }
    };
    let is_queue_shaped = |obj: ObjectRef| -> bool {
        if shared.mem.heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array {
            return false;
        }
        match queue_cid {
            Some(cid) => class_manager.is_subclass_of(shared.mem.heap.class_id_of(obj), cid),
            None => true,
        }
    };
    // The sentinel is a `ReferenceQueue$Null`; anything else at the address it
    // resolved to means the resolution was wrong, and the loop falls back to
    // the synthetic `Int(1)` it always wrote. Checked once, not per enqueue.
    let enqueued_sentinel =
        enqueued_sentinel.filter(|&s| queue_cid.is_some() && is_queue_shaped(s));

    // THE IDENTITY STAMP -- the exact test the two shape guards above
    // approximate. See `ReferenceProcessor::identity_stamps`: a shape guard
    // cannot tell a reclaimed `Reference` whose address was re-issued to
    // ANOTHER `Reference` from the entry it recorded, and H2 allocates a
    // `CloseWatcher` (a `PhantomReference`) per connection, so that case is the
    // common one rather than the exotic one. The stamp is the identity hash the
    // object carried at `discover_reference` time; it lives in the object's own
    // mark word and travels with it across a relocation.
    //
    // `pre_gc_addr` is the key the processor's table is still on at this point
    // (`update_after_gc` runs at the very end of this function), `obj` is the
    // post-relocation object the write would land on.
    //
    // Both `0` cases mean "cannot tell" and fall through to the shape guards
    // rather than declining: an unstamped entry (every in-tree test constructs
    // those) and a thin-locked object, whose hash is displaced out of the mark
    // word, must not lose their reference processing.
    //
    // gc-common w6-d (2026-09-24): the stamp is handed in by the caller, read
    // through `ref_proc.identity_stamp(pre_gc_addr)` at the check. This used to
    // CLONE the processor's whole identity table (and its referent-class
    // table, for the restore screen) on every collection, because a closure
    // holding a borrow of `ref_proc` cannot live across the loops' mutating
    // calls (`take_newly_cleared`, `clear_after_refused_restore`). Taking the
    // stamp as an argument needs no borrow at all, and the read is the same
    // value the clone held: nothing between the old clone point and the last
    // check writes either table (`remove_collected`, the only pruner, runs
    // after the restore loop). Two O(registered references) copies per pause,
    // gone. `common-d-proposal-settled-reference-cold-list`.
    // (The relocation-target set the restore screen consults is built inside
    // the restore block, and only for the referents that can need it — see
    // there. It used to be a SipHash `HashSet` of EVERY `pointer_map` value,
    // built here on every collection whether or not a single reference was
    // active: on a moving young cycle that is one insert per survivor.)
    let identity_matches = |stamp: Option<i32>, obj: ObjectRef| -> bool {
        match stamp {
            Some(stamp) if stamp != 0 => {
                let now = shared.mem.heap.identity_hash_code(obj);
                now == 0 || now == stamp
            }
            _ => true,
        }
    };

    // Null referent field (field 0) on cleared weak/soft references.
    // ROOT-CAUSE FIX (2026-06-10): once-only emission — the legacy
    // `cleared_ref_objects()` re-emitted every ever-cleared Reference on
    // EVERY GC; after the Reference died, the per-cycle null-write through
    // its recycled (and then legitimately-remapped!) address corrupted the
    // innocent object reusing the memory. A referent is nulled exactly once.
    let cleared = ref_proc.take_newly_cleared();
    for ref_addr in cleared {
        // ROOT-CAUSE guard (see `is_stale_young` above): a pre-GC young
        // address absent from the pointer map did NOT survive this GC —
        // writing the `Object(None)` clear through it would corrupt the
        // memory's new occupant (the PROVEN bc-math-ec 0x4 writer).
        // Test the RELOCATED address, not the pre-GC one.
        //
        // The write below already goes through `pointer_map`-relocated
        // `actual_addr`; this guard used to test `ref_addr`, so for a survivor
        // that MOVED the two disagreed about which object they meant. Under a
        // non-moving collector they are the same address and it never
        // mattered; ZGC began compacting on 2026-08-13, and from then every
        // Reference the slide moved was judged dead here and silently never
        // cleared or enqueued — no `WeakReference` delivery and no `Cleaner`
        // action for it, which on netty is how a direct `ByteBuf`'s native
        // memory stops being freed.
        //
        // A no-op wherever the map is empty, i.e. every non-moving cycle on
        // every backend.
        //
        // gc-common w1-d (2026-09-23): a `pointer_map` KEY is itself the proof
        // of survival — the map holds survivors (and identity entries for
        // watched ones) and nothing else — so a moved Reference is not asked
        // again. Asking the backend about its NEW address was wrong on exactly
        // one arm: Generational, a Reference PROMOTED by a cycle that also
        // reclaimed old-gen storage. `watched_pre_gc_addr_survived` answers an
        // unwatched old-gen address with `!old_gen_reclaimed_last_cycle()`,
        // i.e. "dead", so the clear and the enqueue of every Reference promoted
        // by such a cycle (a `System.gc()` with tenured survivors) were
        // silently dropped. For an unmoved Reference `relocated == ref_addr`
        // and nothing changes.
        let relocated = pointer_map.get(&ref_addr).copied().unwrap_or(ref_addr);
        if !pointer_map.contains_key(&ref_addr) && is_stale_young(relocated) {
            if straystack_enabled() {
                eprintln!("[refproc] SKIP dead CLEARED ref @0x{ref_addr:x} (young, not in map)");
            }
            continue;
        }
        // The ref object itself may have been relocated
        let actual_addr = pointer_map.get(&ref_addr).copied().unwrap_or(ref_addr);
        // SAFETY: actual_addr was produced by process_references and points at a valid object header within the heap arena.
        let obj_ref = unsafe { ObjectRef::from_raw(actual_addr as *mut u8) };
        // Belt-and-suspenders: a live `java.lang.ref.Reference` always has
        // >= 2 instance fields (referent, queue); a reused/zeroed slot is a
        // bare 0-field `Object`. (Kept in addition to the precise
        // `is_stale_young` guard — also covers old-gen reuse after a major GC.)
        if shared.mem.heap.num_fields(obj_ref) < 2 {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP stale CLEARED ref @0x{:x} (num_fields={})",
                    actual_addr,
                    shared.mem.heap.num_fields(obj_ref),
                );
            }
            continue;
        }
        // See `is_reference_shaped`: the field-count test above cannot tell a
        // reclaimed-and-reused slot from the Reference that used to be there.
        if !is_reference_shaped(obj_ref) {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP reshaped CLEARED ref @0x{actual_addr:x} (not a Reference)"
                );
            }
            continue;
        }
        // Shape-clean but a DIFFERENT `Reference` -- see `identity_matches`.
        if !identity_matches(ref_proc.identity_stamp(ref_addr), obj_ref) {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP reidentified CLEARED ref @0x{actual_addr:x} (identity stamp mismatch)"
                );
            }
            continue;
        }
        if refaudit_enabled() {
            let cid = shared.mem.heap.class_id_of(obj_ref);
            // gcg g6/rs (`gcg-g5yg-xlog-ref-census-...`): also the class the
            // pre-collection pass recorded for the referent that died, so the
            // census's cleared rows can be named against HotSpot's.
            let referent_class = match ref_proc.referent_class_stamp(ref_addr) {
                Some(stamp) if stamp != 0 => {
                    // The class id is the stamp's low 32 bits (the kind bits
                    // sit above them).
                    let raw = (stamp
                        & !(cratonvm_gc::reference::REFERENT_STAMP_KIND_KNOWN
                            | cratonvm_gc::reference::REFERENT_STAMP_ARRAY))
                        as u32;
                    let array = stamp & cratonvm_gc::reference::REFERENT_STAMP_KIND_KNOWN != 0
                        && stamp & cratonvm_gc::reference::REFERENT_STAMP_ARRAY != 0;
                    let name = class_manager
                        .get_class(ClassId::new(raw))
                        .map(|c| c.name.as_ref().to_string())
                        .unwrap_or_else(|| format!("<unresolved cid={raw}>"));
                    if array {
                        format!("{name}[]")
                    } else {
                        name
                    }
                }
                _ => "<unstamped>".to_string(),
            };
            eprintln!(
                "[refaudit] cleared-null addr=0x{actual_addr:x} class={} fields={} referent_class={referent_class}",
                class_manager
                    .get_class(cid)
                    .map(|c| c.name.as_ref().to_string())
                    .unwrap_or_else(|| format!("<unresolved cid={}>", cid.as_u32())),
                shared.mem.heap.num_fields(obj_ref),
            );
        }
        shared.mem.heap.set_field(obj_ref, 0, Value::Object(None));
    }

    // gc-common w3-d: the POST-collection addresses of the queues the loop below
    // links into, for the delivery thread's wake-up (flag-gated; see
    // `gc_enqueued_queues_notify`). Handed over after `update_after_gc` below.
    // gc-common w5-d: also under `--real-jdk` without the flag, per VM — see
    // `queue_wake_ups_recorded_for`.
    let record_enqueued_queues = queue_wake_ups_recorded(shared);
    let mut gc_enqueued_queues: Vec<usize> = Vec::new();
    let java_queue_lock_slot = class_manager
        .find_bootstrap_class_by_name("java/lang/ref/ReferenceQueue")
        .and_then(|cid| crate::vm::vm_exec::resolve_field_index_in_hierarchy(
            cid, "lock", class_manager.class_store(),
        ));
    let mut deferred_reference_enqueues = Vec::new();
    // zt t34: the pairs the round may link itself, after the processor and
    // class-manager guards are released (`splice_unlocked_queue_enqueues`).
    let queue_splice = QueueSpliceSlots::resolve(&class_manager, enqueued_sentinel);
    let final_reference_cid =
        class_manager.find_bootstrap_class_by_name("java/lang/ref/FinalReference");
    let mut splice_candidates: Vec<(usize, usize)> = Vec::new();

    // Enqueue cleared/phantom references into their ReferenceQueues on the heap.
    // The linked-list protocol: push ref onto queue's head, use referent field as "next" ptr,
    // clear the ref's queue field (one-shot enqueue), increment queue size.
    for (ref_addr, queue_addr) in &result.to_enqueue {
        // ROOT-CAUSE guard (see `is_stale_young` above): skip the whole
        // enqueue when either the Reference or its queue did not survive —
        // the head/size/next writes below through a stale address are the
        // same proven corruption class as the cleared-referent write.
        // Relocated addresses, for the reason on the cleared loop above: the
        // enqueue writes below resolve through the map, so the guard has to as
        // well or a moved Reference (or a moved ReferenceQueue) is declined as
        // dead.
        // A map key is a survival proof on its own — see the cleared loop
        // above for the Generational promote-and-reclaim case this closes.
        let ref_reloc = pointer_map.get(ref_addr).copied().unwrap_or(*ref_addr);
        let q_reloc = pointer_map.get(queue_addr).copied().unwrap_or(*queue_addr);
        let ref_dead = !pointer_map.contains_key(ref_addr) && is_stale_young(ref_reloc);
        let q_dead = !pointer_map.contains_key(queue_addr) && is_stale_young(q_reloc);
        if ref_dead || q_dead {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP dead ENQUEUE ref@0x{ref_addr:x}/q@0x{queue_addr:x} (young, not in map)"
                );
            }
            continue;
        }
        let actual_ref = pointer_map.get(ref_addr).copied().unwrap_or(*ref_addr);
        let actual_q = pointer_map.get(queue_addr).copied().unwrap_or(*queue_addr);
        // SAFETY: actual_ref and actual_q were produced by process_references (with pointer_map relocation) and point at valid object headers within the heap arena.
        let ref_obj = unsafe { ObjectRef::from_raw(actual_ref as *mut u8) };
        let q_obj = unsafe { ObjectRef::from_raw(actual_q as *mut u8) }; // Cast: GC object pointer conversion
                                                                         // avrora `get_field` OOB fix (residual): `pending_queues` is keyed on
                                                                         // addresses and is re-emitted every GC. A `ReferenceQueue` reachable
                                                                         // only through this pending-enqueue record (no live Java reference) is
                                                                         // reclaimed by the sweep and its slot reused for a bare
                                                                         // `java.lang.Object`; the synthetic head/size writes below would then
                                                                         // trip the `gen_heap` out-of-bounds guard. Checking the POST-relocation
                                                                         // (`actual_q`) layout distinguishes a genuinely-dead queue (reused as a
                                                                         // 0-field `Object`) from a live one (still `>= 2` fields) — a live
                                                                         // queue, even one relocated this cycle, is remapped through
                                                                         // `pointer_map` and keeps its real layout, so legitimate enqueues are
                                                                         // unaffected. A dead queue has no consumer to `poll()` the reference
                                                                         // back out, so dropping the enqueue is correct.
        if shared.mem.heap.num_fields(q_obj) < 2 {
            continue;
        }
        // bc math-ec 0x4 STALE-REF FIX: the same reclaimed-and-reused hazard
        // applies to `ref_obj` (writes to its fields 0 and 1 below), which —
        // unlike `q_obj` — was NOT liveness-checked. A `ref_addr` not present in
        // `pointer_map` keeps its stale PRE-GC address via `unwrap_or`; if that
        // Reference was reclaimed and its slot reused, `set_field(ref_obj, ..)`
        // strays into the reusing object (and `set_field(q_obj,0,ref_obj)` would
        // publish a dangling head). A live Reference has >= 2 fields; skip the
        // whole enqueue otherwise (a dead ref has no consumer to poll it back).
        if shared.mem.heap.num_fields(ref_obj) < 2 {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP stale ENQUEUE ref @0x{:x} (num_fields={}) into q@0x{:x}",
                    actual_ref,
                    shared.mem.heap.num_fields(ref_obj),
                    actual_q,
                );
            }
            continue;
        }
        // See `is_reference_shaped`: this is the loop that published a
        // `java.lang.String` as a `ReferenceQueue` head. Both participants are
        // checked — a reused QUEUE slot would take the head/size writes just as
        // wrongly, and a live Reference linked into it would be stranded there.
        if !is_reference_shaped(ref_obj) || !is_queue_shaped(q_obj) {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP reshaped ENQUEUE ref @0x{actual_ref:x} into q@0x{actual_q:x} (not Reference/ReferenceQueue)"
                );
            }
            continue;
        }
        // The loop that published a re-issued object as a queue head: a
        // same-class re-issue is shape-clean here, and linking one into a queue
        // hands it to `ReferenceQueue.poll()` as if it were the reference that
        // died. Only the Reference is stamped -- a `ReferenceQueue` is not
        // discovered through this registry, so it has no stamp to check.
        if !identity_matches(ref_proc.identity_stamp(*ref_addr), ref_obj) {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP reidentified ENQUEUE ref @0x{actual_ref:x} into q@0x{actual_q:x} (identity stamp mismatch)"
                );
            }
            continue;
        }
        if java_queue_lock_slot.is_some_and(|slot| slot < shared.mem.heap.num_fields(q_obj)) {
            // poll0 may be stopped while holding lock with an old head in a
            // Java local. Its resumed store would discard an in-pause splice.
            // zt t34: unless no thread holds that lock at this pause; see
            // `splice_unlocked_queue_enqueues`. A `FinalReference` stays with
            // the Reference Handler (`enqueue0` also counts it in `VM`).
            let final_ref = final_reference_cid.is_some_and(|f| {
                class_manager.is_subclass_of(shared.mem.heap.class_id_of(ref_obj), f)
            });
            if final_ref {
                deferred_reference_enqueues.push((actual_ref, actual_q));
            } else {
                splice_candidates.push((actual_ref, actual_q));
            }
            continue;
        }
        // Push onto the legacy synthetic queue (no Java monitor/poll0 body).
        //
        // Queue linkage uses the Reference's `next` field (slot 2, matching
        // the real-JDK `java.lang.ref.Reference` layout: referent, queue,
        // next, discovered) — NOT the referent slot. The old protocol reused
        // slot 0 (the referent) as the next pointer, so every
        // enqueued-but-not-yet-polled WeakReference answered `get()` with
        // the NEXT Reference in the queue instead of null (only the
        // first-enqueued, whose next was null, read as cleared — the
        // RefCheck `deadCleared=1/256 enqueued=256` signature). References
        // with fewer than 3 fields (legacy synthetic shape) fall back to the
        // old slot-0 linkage, which is at least consistent with the poll
        // side's identical fallback.
        if refaudit_enabled() {
            let rc = shared.mem.heap.class_id_of(ref_obj);
            let qc = shared.mem.heap.class_id_of(q_obj);
            let nm = |cid: cratonvm_types::ClassId| {
                class_manager
                    .get_class(cid)
                    .map(|c| c.name.as_ref().to_string())
                    .unwrap_or_else(|| format!("<unresolved cid={}>", cid.as_u32()))
            };
            eprintln!(
                "[refaudit] enqueue ref=0x{actual_ref:x}/{} q=0x{actual_q:x}/{}",
                nm(rc),
                nm(qc)
            );
        }
        let old_head = shared.mem.heap.get_field(q_obj, 0); // RQ_FIELD_HEAD
        shared
            .mem
            .heap
            .set_field(q_obj, 0, Value::Object(Some(ref_obj))); // new head
        let next_slot = if shared.mem.heap.num_fields(ref_obj) <= 2 {
            0 // legacy synthetic 2-field shape: referent, queue only
        } else {
            reference_next_slot
        };
        shared.mem.heap.set_field(ref_obj, next_slot, old_head); // REF_FIELD_NEXT
                                                                 // RQ_FIELD_SIZE. Slot 1 is `size` in the synthetic two-slot shape but
                                                                 // `queueLength` — a `long` — on a real JDK ReferenceQueue, whose own
                                                                 // `enqueue0`/`poll0` bytecode reads it back. Preserve the stored width.
        let new_size = match shared.mem.heap.get_field(q_obj, 1) {
            Value::Long(v) => Value::Long(v + 1),
            Value::Int(v) => Value::Int(v + 1),
            _ => Value::Int(1),
        };
        shared.mem.heap.set_field(q_obj, 1, new_size);
        // Mark as enqueued. See `gc_enqueued_sentinel`: `ReferenceQueue.ENQUEUED`
        // on the real layout, the synthetic `Int(1)` on the two-slot shape.
        // Slot 1 = REF_FIELD_QUEUE.
        shared
            .mem
            .heap
            .set_field(ref_obj, 1, gc_enqueued_mark(enqueued_sentinel, next_slot));
        // gc-common w3-d: `enqueue0` would `lock.notifyAll()` here; the pause
        // cannot, so the delivery thread does it after (`gc_enqueued_queues_notify`).
        if record_enqueued_queues {
            gc_enqueued_queues.push(actual_q);
        }
    }

    // Enqueue objects for finalization (M8 fix: relocate via pointer_map
    // because the ref-processor holds pre-GC addresses).
    for obj_addr in &result.to_finalize {
        // Finalizable objects are rooted via `finalizer_addrs`, so a LIVE one
        // is always in the pointer map after a moving young GC; a stale young
        // address here would be dereferenced later by `run_finalizers`.
        if is_stale_young(*obj_addr) {
            if straystack_enabled() {
                eprintln!("[refproc] SKIP dead FINALIZE obj @0x{obj_addr:x} (young, not in map)");
            }
            continue;
        }
        let actual = pointer_map.get(obj_addr).copied().unwrap_or(*obj_addr);
        // `enqueue_unfinalized`, not `enqueue`: this address comes from a
        // processor row that was never enqueued before, and the legacy
        // `enqueue`'s address-keyed `already_finalized` gate (removed, gcj
        // j4/rf) refused a NEW object that had been allocated where a
        // finalized one used to live. See that method.
        shared.mem.finalizer_thread.enqueue_unfinalized(actual);
    }

    // Relocate any cleaner actions DEFERRED from earlier GC cycles (queued but
    // not yet run because a JIT borrow was live — see `run_cleaner_actions`).
    // Their cleanable objects may have been evacuated by this collection, so
    // remap their raw addresses before any later drain dereferences them.
    shared.mem.cleaner_thread.update_after_gc(pointer_map);
    // gcj j4/rf: the address-keyed `already_finalized` set this pruned every
    // pause (always empty since gci i11/rp) is gone with the legacy
    // `FinalizerThread::enqueue` door.
    // Same for any finalizers deferred from earlier GC cycles.
    shared.mem.finalizer_thread.update_after_gc(pointer_map);

    // Submit cleaner actions — same pre-GC→post-GC relocation as above.
    // Without this, run_cleaner_actions later derefs a stale address
    // pointing at evacuated memory → SEGV at class_id_of (cleaner_probe).
    for action_addr in &result.cleaner_actions {
        // Same staleness guard as the finalize loop above.
        if is_stale_young(*action_addr) {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP dead CLEANER action @0x{action_addr:x} (young, not in map)"
                );
            }
            continue;
        }
        let actual = pointer_map
            .get(action_addr)
            .copied()
            .unwrap_or(*action_addr);
        // SAFETY: `actual` is the post-relocation address of an object the
        // processor is holding live for this submission.
        let action_obj = unsafe { ObjectRef::from_raw(actual as *mut u8) };
        // See `is_cleanable_shaped`: `run_cleaner_actions` NULLS slot 0 of
        // whatever it dequeues, so a reused address costs an innocent object its
        // first field. Screening at submit keeps the dead address out of a queue
        // that outlives this collection.
        if !is_cleanable_shaped(&class_manager, shared, action_obj) {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP reshaped CLEANER action @0x{actual:x} (not a Reference/Cleanable)"
                );
            }
            continue;
        }
        shared.mem.cleaner_thread.submit_action(actual);
    }

    // Drain ref_processor's finalization_queue → FinalizerThread (inline
    // to avoid re-locking ref_processor which we already hold).
    while let Some(obj_addr) = ref_proc.dequeue_for_finalization() {
        shared.mem.finalizer_thread.enqueue_unfinalized(obj_addr);
    }

    // HIB-CV-24 (Manifestation B): restore the referent slots of Weak/Phantom
    // references whose referent SURVIVED, and prune Reference objects collected
    // this cycle. The pre-collection pass (`weakref_null_referents_pre_gc`)
    // nulled every active Weak/Phantom referent slot so the marker could not keep
    // the referent alive through the live Reference. `process_references` above
    // then flagged the dead-referent entries `cleared`/`enqueued` (weak cleared /
    // phantom queued) — so the entries STILL active here are exactly those whose
    // referent survived via a strong path. Write their (relocated) referent
    // address back so `get()` keeps returning the live object; dead ones keep the
    // null slot. Runs before `update_after_gc` so processor addresses are still
    // the pre-collection (pointer-map key) view.
    if weakref_clear_enabled() && (ref_proc.has_pre_gc_nulled() || refaudit_enabled()) {
        // REFPROC AUDIT (see `refaudit_enabled`). Every `continue` below leaves
        // the slot the PRE-GC pass nulled still null, in a Reference that is
        // live — the comment on the referent screen says so in as many words
        // ("a refusal leaves the slot NULL"). Two of the five decline paths
        // print nothing at all, so the straystack census cannot distinguish
        // "restored everything" from "abandoned half of them". Count each one.
        let audit = refaudit_enabled();
        // zwr (`CRATONVM_DBG_REF_RESTORED`): `(Reference, referent, class id)`
        // of the first ARRAY referents this round kept, so a survivor can be matched
        // against the root that held it (`CRATONVM_DBG_FRAME_ROOT_UNNAMED`).
        let name_restored = false;
        let mut restored_named: Vec<(usize, usize, u32)> = Vec::new();
        let (mut r_ok, mut r_no_ref, mut r_no_referent, mut r_shape, mut r_stamp, mut r_screen) =
            (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
        // gc-common w1-d: entries whose slot this cycle's pre-GC pass did NOT
        // null — nothing to put back. See the gate at the top of the loop.
        let mut r_not_nulled = 0u64;
        // gc-common w1-d: weak/soft entries a refusal or a dead referent left
        // null, now converted into a collector clear (+ deferred enqueue).
        let mut r_converted = 0u64;
        let mut active = ref_proc.weak_phantom_active_pairs();
        // SOFT-CLEAR GAP (2026-08-15): the pre-collection pass also nulled the
        // referent slot of every soft entry its LRU policy condemned. The ones
        // still active here are the condemned entries whose referent turned
        // out to be strongly reachable anyway, so `process_soft_refs` kept
        // them -- and their slot 0 must be written back for exactly the reason
        // a surviving weak referent's must be. The loop below is already
        // generic over `(reference_obj, referent)` pairs, so they ride it.
        active.extend(ref_proc.soft_pre_nulled_active_pairs());
        if dbg_weakref() && !active.is_empty() {
            eprintln!(
                "[weakref] post-gc restore pass: {} surviving referent(s) \
                 (weak/phantom + policy-kept soft)",
                active.len()
            );
        }
        // Read once per collection rather than once per restored entry.
        let screen_on = !cratonvm_types::flags().gc.no_referent_identity_screen;
        // JIT round 21 lane refs: `CRATONVM_DBG_REFERENT_HOLDER` (diagnostic;
        // `None` unless set). Re-landed 2026-10-26 into dev's `reference_round`,
        // which has no root vector in hand: `direct_root=` reads false there.
        let holder_filter = referent_holder_filter();
        let mut holder_reported = 0usize;
        // Every address a survivor RELOCATED INTO this cycle, restricted to the
        // addresses the screen below can actually ask about: the referents of
        // entries this pass nulled whose referent did NOT move (a moved
        // referent is found through the map and never consults the set). A
        // pre-collection address that appears here is not the address it used
        // to be: the object that lived there is gone and a slid survivor now
        // owns the base.
        //
        // gc-common w1-d (perf): this used to be every `pointer_map` value,
        // collected into a SipHash set before the function even knew whether
        // any reference was active — one insert per survivor of every moving
        // cycle. Now: an `FxHashSet` of the (few) candidate referents, one
        // allocation-free pass over the map's values only when there is a
        // candidate, and nothing at all on a non-moving cycle.
        //
        // gci i23/pk (i22/dr finding F1) & gcd-d10r completion:
        // The referent restore screen refuses an unmoved referent if another survivor
        // was relocated onto its address this cycle. Instead of building a private,
        // candidate-filtered hash set of relocations, reuse the pause's shared
        // `pause_relocations` set via `pause_relocations.claims(referent_old)`.
        // Armed only when `pause_relocations.is_armed()` (on ZGC every moving cycle,
        // and on Generational when the cycle reclaimed old storage; unarmed on G1 and
        // young-only Generational cycles).
        let screen_can_hit = pause_relocations.is_armed();
        for (ref_obj_old, referent_old) in active {
            // gc-common w1-d (2026-09-23): put back ONLY what this cycle took.
            //
            // Every entry still active here used to get its registry referent
            // written into slot 0. That is right for a slot the pre-collection
            // pass nulled, and wrong for one it found already null: the
            // application cleared it (`Reference.clear()` is a bare field store
            // the registry never hears about), and writing the referent back
            // RESURRECTED a cleared reference — `r.clear(); System.gc();
            // r.get()` answered the old object whenever it was still strongly
            // reachable, on every collector. It also retried a restore a
            // previous cycle had REFUSED, from an address that cycle had
            // declined to trust. The pass records exactly the slots it nulled
            // (`stamp_referent_class`); weak/soft entries it found null were
            // retired before the phases ran, and a phantom's slot is invisible
            // to `get()`, so skipping loses nothing.
            if !ref_proc.was_nulled_by_pre_gc_pass(ref_obj_old) {
                r_not_nulled += 1;
                continue;
            }
            // Locate the (possibly relocated) Reference object; skip if it did
            // not itself survive — never write through freed/reused memory.
            let ref_obj_new = match pointer_map.get(&ref_obj_old) {
                Some(&a) => a,
                None if shared
                    .mem
                    .heap
                    .watched_pre_gc_addr_survived(ref_obj_old, pointer_map) =>
                {
                    ref_obj_old
                }
                None => {
                    r_no_ref += 1;
                    continue;
                }
            };
            // The referent survived (this entry was not cleared/enqueued): find
            // its post-collection address (relocated → pointer map; old-gen
            // in place → live).
            let mut referent_moved = false;
            let referent_new = match pointer_map.get(&referent_old) {
                Some(&a) => {
                    referent_moved = true;
                    a
                }
                None if shared
                    .mem
                    .heap
                    .watched_pre_gc_addr_survived(referent_old, pointer_map) =>
                {
                    referent_old
                }
                // Defensive: should not happen for an active entry, but never
                // write a stale referent — leave the slot null.
                //
                // gc-common w1-d: and say so in the registry. The slot is null
                // and the referent is gone, which IS a cleared weak/soft
                // reference; leaving the row active kept a dead address as its
                // `referent` and never delivered it to its queue. Phantoms are
                // untouched (see `clear_after_refused_restore`).
                None => {
                    r_no_referent += 1;
                    // gci i11/rp R4: this pair's row, not the first at the address.
                    if ref_proc.clear_after_refused_restore_of(ref_obj_old, referent_old) {
                        r_converted += 1;                    }
                    continue;
                }
            };
            // SAFETY: both addresses are live post-collection object headers.
            let ro = unsafe { ObjectRef::from_raw(ref_obj_new as *mut u8) };
            let rt = unsafe { ObjectRef::from_raw(referent_new as *mut u8) };
            // JIT round 21 lane refs: `CRATONVM_DBG_REFERENT_HOLDER` (diagnostic).
            if let Some(filter) = holder_filter.as_deref() {
                report_referent_holder(
                    shared,
                    filter,
                    &[],
                    (ref_obj_old, referent_old, referent_new),
                    rt,
                    &mut holder_reported,
                );
            }
            // HIB-WEAKREF-RECYCLE.1 (2026-07-31): same belt-and-suspenders
            // shape check the `cleared` loop above already applies, and for the
            // same documented reason — neither the pointer map nor
            // `is_addr_live` can tell a live old-gen Reference from freed
            // old-gen memory that has been recycled, because `is_old_gen_addr`
            // is a pure address-range test. A live `java.lang.ref.Reference`
            // always has >= 2 instance fields; anything else at this address is
            // the memory's new occupant, and writing slot 0 of it corrupts an
            // unrelated object (or trips the `gen_heap` OOB guard, which is how
            // this was found).
            if shared.mem.heap.num_fields(ro) < 2 || !is_reference_shaped(ro) {
                if straystack_enabled() {
                    eprintln!(
                        "[refproc] SKIP stale weak/phantom RESTORE ref @0x{:x} (num_fields={})",
                        ref_obj_new,
                        shared.mem.heap.num_fields(ro),
                    );
                }
                r_shape += 1;
                continue;
            }
            // This pass writes an OBJECT into slot 0, not a null, so a
            // shape-clean re-issue here installs an unrelated reference in a
            // live object's first field -- the `java.lang.String` receiver
            // shape the H2 `TestMultiThread` MVStore-writer report opens with.
            if !identity_matches(ref_proc.identity_stamp(ref_obj_old), ro) {
                if straystack_enabled() {
                    eprintln!(
                        "[refproc] SKIP reidentified weak/phantom RESTORE ref @0x{ref_obj_new:x} (identity stamp mismatch)"
                    );
                }
                r_stamp += 1;
                continue;
            }
            // AND THE SAME QUESTION ABOUT THE REFERENT, which nothing asked.
            //
            // Every guard above proves `ro` is the Reference that was
            // discovered. `rt` had no guard at all, and this line writes it
            // into somebody's `referent` slot. The address it came from is a
            // PRE-collection one, and an address is not an identity once a
            // compacting collector has re-issued it: survivors slide DOWN into
            // the space dead objects vacated, so a dead referent's base is very
            // often a live object's new base. `watched_pre_gc_addr_survived`
            // then answers `true` through ZGC's `is_object_address`, which
            // proves an object lives there and NOT that it is this one.
            //
            // Measured: `SoftReference.get()` returning a
            // `java.io.ClassCache$CacheRef` where
            // `java.lang.invoke.MethodTypeForm.cachedLambdaForm` casts to
            // `LambdaForm`, and the `ObjectStreamClass` twin of it, both on the
            // default collector only — G1 and Generational never took this
            // arm (`is_addr_live` is exact for the first, and the second emits
            // identity pointer-map entries for every watched address).
            //
            // Two screens, in increasing cost:
            //
            //  * a pre-collection address that is a relocation TARGET this
            //    cycle is definitively somebody else's now, unless the map
            //    itself is what sent us there;
            //  * otherwise the referent's CLASS, recorded from slot 0 by the
            //    pre-GC null pass, must still be the class of whatever lives
            //    at the address.
            //
            // A refusal leaves the slot NULL, which reads as a cleared
            // reference. That is a legal answer for a soft reference and an
            // early one for a weak reference; installing a stranger is neither.
            //
            // gc-common w1-d: a refused weak/soft entry is now also CLEARED in
            // the registry (and its enqueue deferred to the next round), so the
            // null the application can already see is a cleared reference the
            // collector stands behind rather than an active row with an
            // untrusted referent. See `clear_after_refused_restore`.
            if screen_on && screen_can_hit && !referent_moved && pause_relocations.claims(referent_old) {
                // gci i11/rp R7 / zr z4/ab: the per-VM count, not a process global.
                note_referent_restore_refused(shared);
                if straystack_enabled() || dbg_weakref() {
                    eprintln!(
                        "[refproc] REFUSE weak/phantom RESTORE ref @0x{ref_obj_new:x}: \
                         referent @0x{referent_old:x} is a relocation TARGET this cycle \
                         (total refused={})",
                        referent_restore_refusals(shared)
                    );
                }
                r_screen += 1;
                if ref_proc.clear_after_refused_restore_of(ref_obj_old, referent_old) {
                    r_converted += 1;
                }
                continue;
            }
            if let Some(want_class) = ref_proc
                .referent_class_stamp(ref_obj_old)
                .filter(|_| screen_on)
            {
                let have_class = shared.mem.heap.class_id_of(rt).as_u32();
                // Kind-aware when the stamp says so: an array's header carries
                // its COMPONENT's class id, so a bare id cannot tell an
                // `Integer[]` from an `Integer` reusing its address. A plain
                // class-id stamp keeps the old test. See
                // `cratonvm_gc::reference::referent_stamp_admits`.
                let have_is_array =
                    shared.mem.heap.kind_of(rt) == crate::memory::heap::ObjectKind::Array;
                if want_class != 0
                    && !cratonvm_gc::reference::referent_stamp_admits(
                        want_class,
                        have_class,
                        have_is_array,
                    )
                {
                    note_referent_restore_refused(shared);
                    if straystack_enabled() || dbg_weakref() {
                        eprintln!(
                            "[refproc] REFUSE weak/phantom RESTORE ref @0x{ref_obj_new:x}: \
                             referent @0x{referent_new:x} is class {have_class}, recorded \
                             {want_class} (total refused={})",
                            referent_restore_refusals(shared)
                        );
                    }
                    r_screen += 1;
                    if ref_proc.clear_after_refused_restore_of(ref_obj_old, referent_old) {
                        r_converted += 1;
                    }
                    continue;
                }
            }
            // Slot 0 = REF_FIELD_REFERENT. `set_field` fires the write barrier,
            // so a young referent restored into a promoted (old-gen) Reference
            // re-marks the old→young card.
            r_ok += 1;
            if name_restored
                && restored_named.len() < 16
                && shared.mem.heap.kind_of(rt) == crate::memory::heap::ObjectKind::Array
            {
                restored_named.push((
                    ref_obj_new,
                    referent_new,
                    shared.mem.heap.class_id_of(rt).as_u32(),
                ));
            }
            // zt t31/zc11 (2c): after the release the slot is the program's
            // again -- a `Reference.clear()` may already have run -- and a
            // hiding cycle never nulled it (nor moved anything), so there is
            // nothing to restore; writing would undo that clear.
            if !after_release {
                shared.mem.heap.set_field(ro, 0, Value::Object(Some(rt)));
            }
        }
        for (r, t, cid) in restored_named.drain(..) {
            eprintln!("[ref-restored] ref=0x{r:x} referent=0x{t:x} cid={cid} r_ok={r_ok}");
        }
        if audit {
            // (The previous format string lost its line-continuation
            // backslashes and printed runs of 18 spaces between fields.)
            eprintln!(
                "[refaudit] post-gc-restore restored={r_ok} not_nulled_this_cycle={r_not_nulled} \
                 abandoned_no_ref_survivor={r_no_ref} abandoned_no_referent_survivor={r_no_referent} \
                 abandoned_shape={r_shape} abandoned_stamp={r_stamp} abandoned_screen={r_screen} \
                 converted_to_clear={r_converted}"
            );
        }
        // Drop entries whose Reference object was collected this cycle so the
        // side-lists stay bounded and the pre-GC null pass never dereferences a
        // freed Reference (same survivor predicate as everything above).
        if refproc_prepass {
            ref_proc.remove_collected_non_reference_rows(&is_marked);
        } else {
            ref_proc.remove_collected(&is_marked);
        }
        // HIB-WEAKREF-RECYCLE.1 (2026-07-31): `is_marked` could not see old-gen
        // reuse — `is_old_gen_addr` was a pure range check, so a weak/phantom
        // entry whose Reference object was reclaimed by an old-gen sweep
        // survived `remove_collected` forever and both referent passes kept
        // targeting recycled memory every cycle. (Since the watched-set
        // predicate, `is_marked` is exact on Generational -- see the skip
        // below -- and stays region-granular elsewhere.) Follow up with the shape test:
        // resolve each entry to its post-collection address exactly as the
        // restore loop above does, and keep it only if a `Reference` (>= 2
        // instance fields) is still what lives there. Runs BEFORE
        // `update_after_gc`, so the stored addresses are still the pre-GC view
        // the pointer map is keyed on.
        let still_a_reference = |addr: usize| -> bool {
            let cur = match pointer_map.get(&addr) {
                Some(&a) => a,
                // gcd d10/r: not an address the pause relocated another object
                // onto -- that `Reference` is whatever slid there, not this row's.
                None if shared.mem.heap.is_addr_live(addr) && !claimed(addr) => addr,
                None => return false,
            };
            // SAFETY: `cur` is a heap address the collector just reported as
            // live (relocated target, or unmoved and in a live region), so its
            // object header is mapped and readable.
            let o = unsafe { ObjectRef::from_raw(cur as *mut u8) };
            shared.mem.heap.num_fields(o) >= 2 && is_reference_shaped(o)
        };
        // gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals`
        // item 3, `CRATONVM_GEN_SKIP_SHAPE_ROW_PRUNE`, default ON, `=0` kill):
        // on Generational with the watched set published, `is_marked` is
        // `watched_pre_gc_addr_survived`, which is exact after an old
        // reclamation too, so `remove_collected` above already dropped every
        // row whose `Reference` died and this pass found nothing but paid a
        // header read and an `is_subclass_of` per weak / phantom row per
        // pause. Under `CRATONVM_DBG_REFPROC_AUDIT` it still runs and reports
        // what it would have dropped (must be 0).
        let is_zgc_after_release = {
            #[cfg(feature = "zgc")]
            {
                shared.mem.heap.is_zgc()
                    && after_release
                    && pointer_map.is_empty()
            }
            #[cfg(not(feature = "zgc"))]
            {
                false
            }
        };
        let skip_shape_prune = (shared.mem.heap.is_generational() || is_zgc_after_release)
            && weakref_clear_enabled()
            && gen_skip_shape_row_prune_enabled();
        if !skip_shape_prune {
            ref_proc.retain_shaped_weak_phantom(&still_a_reference);
        } else if audit {
            let dropped = ref_proc.retain_shaped_weak_phantom(&still_a_reference);
            if dropped != 0 {
                eprintln!(
                    "[refaudit] gen shape prune (skipped by default) dropped {dropped} \
                     weak/phantom row(s) remove_collected kept"
                );
            }
        }
    }
    // Every shape guard above is done; release the L10 read guard.
    drop(class_manager);
    // gcj j4/rf: the restore pass's late clears are in; hand the census over.
    emit_ref_census(&mut ref_proc);

    // gc-common w1-d: this collection's pre/post protocol is complete. Forget
    // which slots the pre-collection pass nulled, which rows it examined and
    // which soft rows it condemned, so nothing that runs WITHOUT a pass of its
    // own — G1's final remark — reads them as current. Before the relocation
    // below so it does not rebuild a set that is about to be emptied.
    ref_proc.finish_pre_gc_cycle();

    // Relocate all addresses in the ref processor to match the new heap layout
    ref_proc.update_after_gc(pointer_map);
    // gcg g6/rs: the soft clock this round committed, for `SoftReference.clock`.
    let soft_clock_ms = ref_proc.soft_gc_clock();
    drop(ref_proc);
    // gcg g6/rs: HotSpot advances `SoftReference.clock` after the round; this
    // VM does under `CRATONVM_REF_DISCOVER_AT_ALLOCATION` only (a no-op
    // otherwise). Outside every lock this function took.
    crate::runtime::interpreter::publish_soft_reference_clock(shared, soft_clock_ms);

    // gc-common w3-d: hand this collection's queue wake-ups to the delivery
    // thread and wake it (flag-gated; a no-op otherwise). AFTER
    // `cleaner_thread.update_after_gc` above: these are already post-collection
    // addresses and must not be relocated a second time. Outside the processor
    // lock, which nothing below needs.
    let deferred_reference_enqueues = splice_unlocked_queue_enqueues(
        shared,
        queue_splice,
        splice_candidates,
        final_reference_cid,
        deferred_reference_enqueues,
        &mut gc_enqueued_queues,
        !after_release,
    );
    gc_enqueued_queues_hand_off(shared, &gc_enqueued_queues);
    gc_reference_enqueues_hand_off(shared, &deferred_reference_enqueues);
}



/// The JDK field slots [`splice_unlocked_queue_enqueues`] writes, resolved
/// by name on their declaring classes (`ReferenceQueue.head`, `.queueLength`,
/// `.lock`; `Reference.queue`, `.next`), and `ReferenceQueue.ENQUEUED`. `None`
/// when any is missing (no JDK `java.lang.ref`, the synthetic two-slot shape).
#[derive(Clone, Copy)]
pub(in crate::runtime::interpreter::gc_and_alloc) struct QueueSpliceSlots {
    pub(in crate::runtime::interpreter::gc_and_alloc) q_head: usize,
    pub(in crate::runtime::interpreter::gc_and_alloc) q_len: usize,
    pub(in crate::runtime::interpreter::gc_and_alloc) q_lock: usize,
    pub(in crate::runtime::interpreter::gc_and_alloc) r_queue: usize,
    pub(in crate::runtime::interpreter::gc_and_alloc) r_next: usize,
    pub(in crate::runtime::interpreter::gc_and_alloc) enqueued: ObjectRef,
}

impl QueueSpliceSlots {
    pub(in crate::runtime::interpreter::gc_and_alloc) fn resolve(
        class_manager: &crate::classloading::ClassManager,
        enqueued: Option<ObjectRef>,
    ) -> Option<Self> {
        let store = class_manager.class_store();
        let field = |class: &str, name: &str| {
            let cid = class_manager.find_bootstrap_class_by_name(class)?;
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(cid, name, store)
        };
        Some(Self {
            q_head: field("java/lang/ref/ReferenceQueue", "head")?,
            q_len: field("java/lang/ref/ReferenceQueue", "queueLength")?,
            q_lock: field("java/lang/ref/ReferenceQueue", "lock")?,
            r_queue: field("java/lang/ref/Reference", "queue")?,
            r_next: field("java/lang/ref/Reference", "next")?,
            enqueued: enqueued?,
        })
    }
}

/// zt t34 (`zgc-round-stress-leaves-weak-references-unenqueued-under-load`):
/// link each `(Reference, ReferenceQueue)` of `candidates` into its queue as
/// `ReferenceQueue.enqueue0` does when no thread holds the queue's `lock`,
/// and hand the rest to the Reference Handler as before. Answers the pairs
/// left for it (`deferred` plus the candidates not linked); the queues
/// linked into join `queues`, whose waiters the delivery thread wakes with
/// `synchronized (lock) { lock.notifyAll(); }` after the pause -- the last
/// statement of `enqueue0`.
///
/// Why the lock decides it (`reference-queue-gc-splice-overwrites-paused-poll`):
/// every Java read-modify-write of `head` (`enqueue0`, `poll0`, `forEach`)
/// runs under `lock`, and a thread stopped inside one holds it, so its
/// resumed store of an old `head` would drop a splice -- that is why the
/// round deferred EVERY pair to the Reference Handler. With the lock held by
/// no thread at this pause, no stopped frame is inside such a section, and
/// the next one to run reads the spliced `head` under the lock. A `wait`er
/// in `remove0` released it and re-reads `head` through `poll0`. Deferring
/// made delivery as fast as the Reference Handler runs: on a loaded host,
/// between pauses of a hundred milliseconds, it enqueued about eight
/// thousand references a second, and `Zt31Zc11RoundStress` missed its 20 s
/// drain with every referent long cleared.
///
/// Only inside the pause (`world_stopped`), only where this VM records queue
/// wake-ups and a delivery thread takes them (started here on first use, as
/// [`gc_enqueued_queues_hand_off`] starts it: an unwoken untimed `remove()`
/// would sleep on), and only for a `Reference` whose `queue` still names this
/// queue (`enqueue0` returns `false` for `NULL_QUEUE` / `ENQUEUED`; the
/// Reference Handler keeps that verdict).
///
/// The pairs still waiting for the Reference Handler are retried here too:
/// those of a queue that was locked at an earlier pause, and those of a round
/// that ran after the release (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`), where no
/// splice is sound. `CleanerThread::update_after_gc` relocated them earlier in
/// this round. A `FinalReference` (`final_reference_cid`) is never linked:
/// `enqueue0` also counts it in `VM`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn splice_unlocked_queue_enqueues(
    shared: &SharedVm,
    slots: Option<QueueSpliceSlots>,
    mut candidates: Vec<(usize, usize)>,
    final_reference_cid: Option<cratonvm_types::ClassId>,
    mut deferred: Vec<(usize, usize)>,
    queues: &mut Vec<usize>,
    world_stopped: bool,
) -> Vec<(usize, usize)> {
    let slots = match slots {
        Some(s)
            if world_stopped
                && (!candidates.is_empty() || shared.mem.cleaner_thread.has_reference_enqueues())
                && queue_wake_ups_recorded(shared)
                && request_finalizer_thread(shared, true) =>
        {
            s
        }
        _ => {
            deferred.extend(candidates);
            return deferred;
        }
    };
    let heap = &shared.mem.heap;
    let waiting = shared.mem.cleaner_thread.drain_reference_enqueues();
    if !waiting.is_empty() {
        let cm = shared.classes.class_manager.read();
        for (r, q) in waiting {
            // SAFETY: rooted and relocated by `CleanerThread`; the world is
            // stopped.
            let r_obj = unsafe { ObjectRef::from_raw(r as *mut u8) };
            if final_reference_cid.is_some_and(|f| cm.is_subclass_of(heap.class_id_of(r_obj), f)) {
                deferred.push((r, q));
            } else {
                candidates.push((r, q));
            }
        }
    }
    let monitors = &shared.threads.monitors;
    for (r, q) in candidates {
        // SAFETY: post-collection addresses of a live `Reference` and a live
        // `ReferenceQueue`, shape-screened by the round's loop; the world is
        // stopped.
        let r_obj = unsafe { ObjectRef::from_raw(r as *mut u8) };
        let q_obj = unsafe { ObjectRef::from_raw(q as *mut u8) };
        let fields = heap.num_fields(q_obj);
        let linkable = slots.q_lock < fields
            && slots.q_head < fields
            && slots.q_len < fields
            && slots.r_next < heap.num_fields(r_obj)
            && slots.r_queue < heap.num_fields(r_obj)
            && matches!(heap.get_field(q_obj, slots.q_lock),
                Value::Object(Some(lock)) if monitors.is_unheld_at_pause(lock))
            && matches!(heap.get_field(r_obj, slots.r_queue),
                Value::Object(Some(owner)) if owner.as_ptr() == q_obj.as_ptr());
        let Value::Long(len) = heap.get_field(q_obj, slots.q_len) else {
            deferred.push((r, q));
            continue;
        };
        if !linkable {
            deferred.push((r, q));
            continue;
        }
        // `r.next = (head == null) ? r : head;` -- self-looped at the end.
        let next = match heap.get_field(q_obj, slots.q_head) {
            Value::Object(Some(head)) => head,
            _ => r_obj,
        };
        heap.set_field(r_obj, slots.r_next, Value::Object(Some(next)));
        heap.set_field(q_obj, slots.q_head, Value::Object(Some(r_obj)));
        heap.set_field(q_obj, slots.q_len, Value::Long(len + 1));
        heap.set_field(r_obj, slots.r_queue, Value::Object(Some(slots.enqueued)));
        if queues.last() != Some(&q) {
            queues.push(q);
        }
    }
    deferred
}

pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_reference_enqueues_hand_off(shared: &SharedVm, pairs: &[(usize, usize)]) {
    // The genuine JDK Reference Handler takes these through its native pending
    // list bridge and invokes ReferenceQueue.enqueue under the queue's lock.
    shared.mem.cleaner_thread.submit_reference_enqueues(pairs);
}

/// Hand a reference pass's queue wake-ups to the delivery thread and wake it.
/// With no delivery thread to take them (flag off, or it could not be
/// started) the list is dropped at once rather than kept rooted forever —
/// nothing would ever drain it. gc-common w3-d.
///
/// gc-common w5-d: under the default policy a `--real-jdk` VM records
/// wake-ups too (`queue_wake_ups_recorded_for`), and a non-empty list then
/// requests a batch — starting the delivery thread on first use, from inside
/// the pause, exactly as the full hand-off's `wake_finalizer_thread` does. That
/// batch notifies the queues (`drain_finalizer_thread_batch`) and otherwise
/// runs only what the default policy already gives the thread; cleaner actions
/// stay on their inline doors. An empty list requests nothing, so a strict VM
/// whose collections enqueue nothing never starts a thread for it.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_enqueued_queues_hand_off(shared: &SharedVm, gc_enqueued_queues: &[usize]) {
    shared
        .mem
        .cleaner_thread
        .note_queues_to_notify(gc_enqueued_queues);
    let taken = if finalizer_thread_enabled() {
        wake_finalizer_thread(shared)
    } else {
        !gc_enqueued_queues.is_empty() && request_finalizer_thread(shared, true)
    };
    if !taken && !gc_enqueued_queues.is_empty() {
        let _ = shared.mem.cleaner_thread.drain_queues_to_notify();
    }
}

/// Restores the post-GC referent pass refused because it could not prove the
/// object at the recorded address is still the referent. Non-zero means this
/// VM would have installed a stranger in a `Reference`'s slot 0.
///
/// zr z4/ab: per VM (`AllocLadderState::referent_restore_refusals`). It was a process
/// static, so a second VM in the process printed the first VM's refusals as
/// its own (`common-watched-referent-set-is-per-thread-not-per-heap-20261012.md` §3). gci i11/rp (R7)
/// fixed the same static on `cratonvm_gc::ReferenceProcessor`
/// (`note_restore_refused` / `restore_refusals`, still there with their own
/// test); the merge of origin/dev (2026-10-15) kept this counter for the
/// restore loop.
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_referent_restore_refused(shared: &SharedVm) {
    shared
        .mem
        .alloc_ladder
        .referent_restore_refusals
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many restores this VM has refused so far. Printed beside each refusal
/// and available to a test.
pub(crate) fn referent_restore_refusals(shared: &SharedVm) -> u64 {
    shared
        .mem
        .alloc_ladder
        .referent_restore_refusals
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// JIT round 21 lane refs
/// (`docs/feature-designs/generative-gc-architecture.md`):
/// `CRATONVM_DBG_REFERENT_HOLDER=<class substring>` (`1`: every class) -- the
/// class filter of [`report_referent_holder`], or `None` when unset.
pub(in crate::runtime::interpreter::gc_and_alloc) fn referent_holder_filter() -> Option<String> {
    if !crate::runtime::env_cache::dbg_referent_holder() {
        return None;
    }
    let v = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).unwrap_or_default();
    let v = v.trim();
    Some(if v == "1" {
        String::new()
    } else {
        v.to_string()
    })
}

/// `CRATONVM_DBG_REFERENT_HOLDER`: say WHAT kept a weak / phantom (or
/// policy-kept soft) referent alive through this collection, for a referent
/// whose class name contains `filter`. One `[referent-holder]` line per
/// surviving referent (at most 64 per collection, retention paths for 4):
///
/// * `direct_root` -- the referent is in this cycle's root vector itself;
/// * `jit_pinned` -- a thread's conservative JIT-root publication names it
///   (`gc_quiescence::pinned_jit_roots_snapshot`: a peer's blocking deposit
///   included);
/// * `jit_prov` -- the compiled-frame word / band / whole-band sweep that
///   rooted it during the collector's own root scan
///   (`conservative_roots::record_provenance_for`, armed on every backend by
///   this switch): `method=.. off=.. region=..` for a frame band word,
///   `scan_one_frame addr=..` for a layout-free sweep (the whole-band
///   fallback, a foreign innermost frame, the A5 sweep);
/// * `root_source` -- the named native root source, when
///   `CRATONVM_DBG_ROOT_SOURCE=1` is also set;
/// * for a referent that is not a direct root, up to four heap retention
///   paths (`VmHeap::retention_paths`, class names per hop).
///
/// Diagnostic only: reads, never writes; the class lookup never blocks.
pub(in crate::runtime::interpreter::gc_and_alloc) fn report_referent_holder(
    shared: &SharedVm,
    filter: &str,
    cycle_roots: &[ObjectRef],
    (ref_obj_old, referent_old, referent_new): (usize, usize, usize),
    rt: ObjectRef,
    reported: &mut usize,
) {
    // Lines per collection, and of those the ones that also walk the heap
    // for retention paths (a whole-heap walk each).
    const CAP: usize = 64;
    const PATH_REPORTS: usize = 4;
    if *reported >= CAP {
        return;
    }
    let cid = shared.mem.heap.class_id_of(rt);
    let name_of = |cid: ClassId| -> String {
        shared
            .classes
            .class_manager
            .try_read_untracked()
            .and_then(|cm| cm.get_class(cid).map(|c| c.name.to_string()))
            .unwrap_or_else(|| format!("class_id={}", cid.as_u32()))
    };
    let name = name_of(cid);
    if !filter.is_empty() && !name.contains(filter) {
        return;
    }
    *reported += 1;
    // addr-keyed: diagnostic local (CRATONVM_DBG_REFERENT_HOLDER) built and dropped inside one pause.
    let root_set: std::collections::HashSet<usize> =
        cycle_roots.iter().map(|r| r.as_ptr() as usize).collect();
    let direct_root = root_set.contains(&referent_old) || root_set.contains(&referent_new);
    let pinned = cratonvm_gc::gc_quiescence::pinned_jit_roots_snapshot();
    let jit_pinned = pinned.contains(&referent_old) || pinned.contains(&referent_new);
    let jit_prov = cratonvm_gc::gc_quiescence::lookup_jit_root_provenance(referent_old)
        .or_else(|| cratonvm_gc::gc_quiescence::lookup_jit_root_provenance(referent_new));
    let source = crate::memory::native_roots::root_source_of(referent_old)
        .or_else(|| crate::memory::native_roots::root_source_of(referent_new));
    // JIT round 21 lane refs, second pass: the scans that run OUTSIDE the
    // collection's armed root pass (a blocking deposit, the cross-thread
    // helper-window pass over a blocked peer's whole stack) record here.
    let unarmed_prov = cratonvm_gc::gc_quiescence::lookup_jit_holder_provenance(referent_old)
        .or_else(|| cratonvm_gc::gc_quiescence::lookup_jit_holder_provenance(referent_new));
    eprintln!(
        "[referent-holder] ref=0x{ref_obj_old:x} referent=0x{referent_old:x}->0x{referent_new:x} \
         class={name} direct_root={direct_root} jit_pinned={jit_pinned} jit_prov={} \
         unarmed_prov={} root_source={}",
        jit_prov.as_deref().unwrap_or("-"),
        unarmed_prov.as_deref().unwrap_or("-"),
        source.unwrap_or("-"),
    );
    if direct_root || *reported > PATH_REPORTS {
        return;
    }
    let is_root = |a: usize| root_set.contains(&a);
    let paths = shared
        .mem
        .heap
        .retention_paths(referent_new, &is_root, 200_000, 4);
    // `retention_paths` ends a branch at a node no heap object references; a
    // path that is the referent alone therefore says NO heap object holds it
    // (its own `Reference`'s slot was nulled before the collection): it
    // survived through a root/pin above, a side-table edge, or -- on G1 -- the
    // membership of a region kept out of the collection set.
    if paths.len() == 1 && paths[0].len() == 1 && paths[0][0].0 == referent_new {
        eprintln!(
            "[referent-holder]   no heap referrer (root/pin, side-table edge or kept region)"
        );
        return;
    }
    eprintln!("[referent-holder]   retention_paths={}", paths.len());
    for path in &paths {
        let rendered: Vec<String> = path
            .iter()
            .map(|&(a, c)| {
                if a == referent_new {
                    format!("<referent>@{a:#x}")
                } else {
                    format!("{}@{a:#x}", name_of(ClassId::new(c)))
                }
            })
            .collect();
        eprintln!("[referent-holder]   {}", rendered.join(" -> "));
    }
}

/// Try to allocate an object, running GC and retrying on failure.
/// Returns the ObjectRef or a RuntimeError::OutOfMemoryError.
///
/// Fast path: TLAB bump-pointer (no lock).
/// Medium path: refill TLAB from shared arena (one lock acquisition).
/// Slow path: GC + retry.
pub(in crate::runtime::interpreter) fn gc_alloc_object(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = gc_alloc_object_unsampled(shared, thread, class_id, num_fields)?;
    // Interpreter round i2 wave 63, lane L1: a C JVMTI env's
    // `SampledObjectAlloc`; one relaxed load when none listens.
    #[cfg(feature = "experimental-debug")]
    if shared
        .debug
        .debugger_gates
        .native_event_armed(crate::debug::NativeEvent::SampledAlloc)
    {
        return Ok(crate::jvmti::native_env::sample_allocation(
            shared, thread, obj,
        ));
    }
    Ok(obj)
}

/// [`gc_alloc_object`] without the JVMTI allocation sample.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_alloc_object_unsampled(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};

    // ONE shape decision for this allocation, used both to reserve the region
    // and to stamp the header. See `plan_tlab_object_shape`.
    let (total_size, _body_size, _gc_flags) =
        plan_tlab_object_shape_for(shared, class_id, num_fields, tlab_site::INTERPRETER);
    let _ = (HEADER_SIZE, SLOT_SIZE);

    // The per-class allocation recipe (`jit::alloc_class_cache`), shared with
    // the JIT's slow-path allocators: every default to write and the finalizer
    // flag, published lock-free after the first allocation of the class. A
    // warm `new` therefore takes no class-manager lock and walks no hierarchy.
    // Read BEFORE the allocation (wave 16), so the first `new` of a class
    // takes its class-manager read with no unrooted new object in hand. The
    // borrow outlives an `invalidate` (entries are retired, not freed).
    let recipe = class_alloc_recipe(shared, class_id);

    // Whether the body this allocation got already spells every JVM default:
    // a COMPACT body carved from a TLAB chunk (see `compact_body_holds_defaults`).
    let mut zero_body_is_default = false;
    // TLAB fast path: try thread-local bump allocation (no lock)
    let obj = if total_size <= cratonvm_gc::tlab::tlab_max_alloc() {
        if let Some(ptr) = tlab_alloc_object(thread, shared, class_id, num_fields, total_size) {
            zero_body_is_default = compact_body_holds_defaults(num_fields, total_size);
            ptr
        } else {
            // TLAB miss: fall through to shared heap
            alloc_object_shared(shared, thread, class_id, num_fields)?
        }
    } else {
        // Large object: skip TLAB, allocate directly from shared heap
        alloc_object_shared(shared, thread, class_id, num_fields)?
    };

    // Same writes as the locked walk below (every instance slot, reference
    // slots included); off with `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE`.
    if let Some(info) = recipe {
        use crate::jit::alloc_class_cache::PrimKind;
        if zero_body_is_default {
            // Every store below would write zero bytes into zero bytes: a
            // compact slot is tagless, and `write_compact_field` spells
            // `Long(0)`, `Float(0.0)`, `Double(0.0)` and `Object(None)` as
            // all-zero at every storage kind (narrow oops encode null as 0).
            // Round i1 wave 16 (lock-free `new` stage 3, compact half).
            if info.has_finalizer {
                shared.register_finalizable(obj.as_ptr() as usize); // Cast: GC object pointer to address
            }
            crate::memory::ref_discovery::note_allocation(shared, obj, class_id);
            return Ok(obj);
        }
        let heap = &shared.mem.heap;
        for &(inst_idx, kind) in info.prim_inits.iter() {
            // An int-family default needs no store: the body is zeroed when
            // the TLAB chunk is carved (and by the shared-heap allocator), and
            // an all-zero legacy slot decodes as `Int(0)` (tag word 0) — a
            // compact one as zero of its storage kind. `long`/`float`/`double`
            // carry a tag that zero bytes do not spell, and a reference needs
            // the `Object(None)` tag (see `init_primitive_fields`), so those
            // are still written. Round i1 wave 4 (lock-free `new` stage 3).
            if kind == PrimKind::Int {
                continue;
            }
            // Widening: u32 field index -> usize
            heap.set_field(obj, inst_idx as usize, kind.default_value());
        }
        for &inst_idx in info.ref_inits.iter() {
            // Widening: u32 field index -> usize
            heap.set_field(obj, inst_idx as usize, Value::Object(None));
        }
        if info.has_finalizer {
            shared.register_finalizable(obj.as_ptr() as usize); // Cast: GC object pointer to address
        }
        crate::memory::ref_discovery::note_allocation(shared, obj, class_id);
        return Ok(obj);
    }

    // Uncached: the locked walk, through the same helper every other
    // constructor-bound allocation uses (`init_new_instance`: ONE class-manager
    // read for the defaults AND `has_finalizer`, then the JLS §12.6
    // registration outside the guard). This was an inline copy of that helper
    // until round i1 wave 5; the merge with `dev` (gc-common w2-d, which
    // introduced `init_new_instance`) kept the recipe arm above and left the
    // copy here, two spellings of one rule.
    init_new_instance(shared, obj, class_id);
    Ok(obj)
}

/// The published allocation recipe for `class_id`, building and publishing it
/// on the first allocation of the class. `None` when the cache is switched
/// off, the id is outside its dense range, or the class (or a superclass) is
/// not in the store -- the caller then takes its locked walk.
///
/// The recipe is built under a class-manager read; that read is the cost the
/// FIRST allocation of each class pays, and no later one does.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn class_alloc_recipe(
    shared: &SharedVm,
    class_id: ClassId,
) -> Option<&crate::jit::alloc_class_cache::ClassAllocInfo> {
    use crate::jit::alloc_class_cache::ClassAllocInfo;
    // gcg g6/jm: the VM's own reading (`JitAllocClassCache::enabled`), not a process static.
    if !shared.jit.jit_alloc_class_cache.enabled() {
        return None;
    }
    let cache = &shared.jit.jit_alloc_class_cache;
    if let Some(info) = cache.get(class_id.as_u32()) {
        return Some(info);
    }
    let recipe = {
        let cm = shared.classes.class_manager.read();
        ClassAllocInfo::build(&cm.class_store, class_id)
    }?;
    cache.insert(class_id.as_u32(), recipe)
}

/// JVMS §6.5 `new`, linking exceptions: `InstantiationError` when the
/// resolved class is an interface or `abstract`. Answers `(exception class,
/// message)`, or `None` when the class may be instantiated.
///
/// HotSpot raises it from `InterpreterRuntime::_new`
/// (`InstanceKlass::check_valid_for_instantiation`) after the class resolved —
/// so after the JVMS §5.4.4 access check — and BEFORE it is initialized, with
/// the class's external name as the message (`InstantiationError: p.Abs`).
/// Until round i1 wave 16 no `new` here checked it: bytecode `new` of an
/// abstract class (separate compilation, or generated bytecode) allocated an
/// instance of it and ran its constructor.
///
/// A `ClassOrigin::CompatibilityStub` is exempt: its access flags are guessed
/// (`ClassManager` marks a missing `Foo$Bar` or `*able` name as an interface),
/// so they say nothing about the real class. No `--real-jdk` class is a stub.
///
/// Asked where a `new` site is resolved: the interpreter's `op_new` slow path,
/// the JIT's CP-indexed `new` helper, and the compile-time `new` row (which
/// defers such a site to that helper). A site-cache hit never asks: the site
/// is only filled after a successful `new`, and a class's `ACC_ABSTRACT` /
/// `ACC_INTERFACE` bits cannot change afterwards (redefinition may not change
/// class modifiers).
pub(crate) fn new_instantiation_refusal(
    class: &crate::classloading::Class,
) -> Option<(&'static str, String)> {
    if class.origin.is_compatibility_stub() || !(class.is_interface() || class.is_abstract()) {
        return None;
    }
    Some((
        "java/lang/InstantiationError",
        crate::runtime::resolve::selection::external_class_name(class),
    ))
}

/// The JVM default value (JVMS §2.3 table, §4.12.5) for a field whose
/// descriptor starts with `desc_first`.
///
/// Total, by construction: an unrecognised or malformed byte answers `null`,
/// which is the same fall-open
/// `cratonvm_gc::heap::default_value_for_descriptor` +
/// `alloc_object_with_descriptors` take together
/// (`.unwrap_or(Value::Object(None))`), so the two allocation entry points
/// cannot disagree about a broken descriptor.
///
/// Split out of [`init_primitive_fields`]' hot loop so the mapping is
/// testable without a `SharedVm` and a populated class store, and so the
/// JIT's byte-for-byte duplicate of that loop
/// (`vm/src/jit/helpers.rs::jit_init_primitive_fields`) can be collapsed onto
/// one table — see `G56-1` NOMINATION 1. `#[inline]`, so the split costs the
/// allocation path nothing.
#[inline]
pub fn jvm_default_for_descriptor(desc_first: u8) -> Value {
    match desc_first {
        b'I' | b'B' | b'C' | b'S' | b'Z' => Value::Int(0),
        b'J' => Value::Long(0),
        b'F' => Value::Float(0.0),
        b'D' => Value::Double(0.0),
        // Reference (`L`), array (`[`), and every malformed or unrecognised
        // descriptor byte: null, WRITTEN. See `init_primitive_fields` for why
        // this cannot be left to the allocator's zero fill.
        _ => Value::Object(None),
    }
}

/// Write the JVM default value (JVMS §2.3, §4.12.5) into every *instance*
/// field of a freshly allocated object: `Int(0)` for `I B C S Z`, `Long(0)`
/// for `J`, `Float(0.0)` for `F`, `Double(0.0)` for `D`, and
/// **`Object(None)` for `L`/`[` and for any descriptor byte this match does
/// not recognise**.
///
/// The name is historical — it predates the reference arm and is spelled at
/// eleven call sites outside this file, so renaming it is a wider edit than
/// the fix deserves. Read it as `init_default_fields`.
///
/// # Why the reference arm is a WRITE and not a skip (G56-1)
///
/// Until 2026-08-17 the `L`/`[` arm was `_ => None` carrying the comment
/// *"Reference types: already `Object(None)` from zero memory"*. That premise
/// was true when it was written and has been false since `Value::Object`
/// gained its `NonNull` niche. `Value` is `#[repr(u32)]` with `Int = 0` and
/// `Object = 4` (`types/src/value.rs`), so:
///
/// | slot bytes | decodes as |
/// |---|---|
/// | all zero | `Value::Int(0)` — tag word 0 |
/// | tag word 4, payload64 0 | `Value::Object(None)` |
///
/// `gen_heap::read_slot` says the same thing in its own doc: *"there is no
/// 'zeroed slot reads as null' shortcut"*. So a `null` default cannot be
/// obtained from the allocator's zero fill; the tag word has to be stored.
/// This is not an optimisation that was skipped, it is the one write that
/// makes the slot mean what the class declares.
///
/// MEASURED, `target-rel4` (`cb2ade4fd`), `--real-jdk`,
/// `CRATONVM_DBG_COERCION=1`, 16 vectors: **1,105 of 1,120** descriptor-
/// coercion events were `primitive-into-reference`/`read`/`L`|`[`/`Int(0)`,
/// i.e. the first descriptor-aware read of a reference field this loop had
/// left as raw zero. Two clusters that were opened as suspected defects —
/// `ReferenceQueue.head` (736) and `Properties.defaults` (243) — are that
/// shape and nothing else (`G49-1`).
///
/// # Every reader already agrees on the answer, which is why this is safe
///
/// SOURCE-VERIFIED, all four readers of a never-written reference slot:
///
/// * the interpreter's own `getfield` (`opcodes.rs`) carries a local fixup,
///   `Value::Int(0) | Value::Long(0) => value = Value::Object(None)`, for
///   exactly this slot shape; an `Object(None)` falls through its `_ => {}`;
/// * the JIT's inline `getfield` (`jit/src/x64/bytecode_walk.rs`) loads the
///   8-byte payload at `FIELD_CELL_PAYLOAD64_OFFSET` and never looks at the
///   tag — zero either way;
/// * the descriptor-aware pair (`heap::coerce_field_value_for_slot`) turns
///   `Int(0)` at an `L` slot into `Object(None)` *and reports the loss*;
///   handed an `Object(None)` it passes it through silently;
/// * `values_equal_for_cas` (`vm_exec.rs`) equates `Object(None)` and
///   `Int(0)` in **both** directions, so no CAS loop changes outcome.
///
/// The collector agrees too: `gen_heap::for_each_ref_slot`'s legacy arm
/// matches `Value::Object(Some(_))`, which neither shape satisfies.
///
/// So the write changes no answer anywhere — it removes a mis-tagged
/// intermediate state that four separate readers were each repairing
/// locally, and with it 98.7% of the G30 instrument's population.
///
/// # Cost
///
/// One extra `VmHeap::set_field` per reference instance field per object.
/// This lane may not build and so cannot measure the after-cost; the bound is
/// stated instead. The added store is the *cheapest* store this function
/// makes: `write_barrier` returns on its first tag test for anything that is
/// not `Object(Some(_))` (`gen_heap.rs`), there is no SATB pre-barrier on
/// this path, the class-manager read lock is held once for the whole walk,
/// and the object body was bump-allocated microseconds earlier so it is
/// L1-resident. MEASURED (static, `javap -p -s` over the 494 classes
/// `RJdkHello --real-jdk` loads): instance fields split 361 primitive /
/// 613 reference, so the store count rises by ~1.7x on that mix. The
/// *cheaper* option — filling the object body with the `Object(None)`
/// pattern in the allocator instead of zeroing it — is a `gc/` change and is
/// nominated in `G56-1`, not taken here.
///
/// # Callers
///
/// Every one of the eleven call sites (`gc_and_alloc.rs`, `vm_exec.rs` ×8,
/// `vm_init.rs` ×2) invokes this immediately after `alloc_object` /
/// `try_alloc_object_full` on an object nothing has written yet. That is now
/// load-bearing: called on a *populated* object this would null every
/// reference field. It was harmless before only because the reference arm
/// did nothing.
///
/// # Return value
///
/// `class_id`'s `has_finalizer` bit, read under the same class-manager read
/// guard as the walk, so a caller that must also register the object for
/// finalization does not take the lock a second time. Callers that do not
/// care ignore it (it is a plain `bool`, not `#[must_use]`). Registering is
/// NOT done here, because not every caller should: see
/// [`init_new_instance`].
pub fn init_primitive_fields(shared: &SharedVm, obj: ObjectRef, class_id: ClassId) -> bool {
    let cm = shared.classes.class_manager.read();
    let has_finalizer = cm
        .class_store
        .get(class_id)
        .map_or(false, |c| c.has_finalizer);
    write_default_instance_fields(shared, &cm.class_store, obj, class_id);
    has_finalizer
}

/// The body of [`init_primitive_fields`], for a caller that already holds the
/// class-manager read guard (`init_primitive_fields`, which reads `has_finalizer`
/// under the same guard).
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn write_default_instance_fields(
    shared: &SharedVm,
    store: &crate::classloading::ClassStore,
    obj: ObjectRef,
    class_id: ClassId,
) {
    let mut cid = Some(class_id);
    while let Some(current_id) = cid {
        if let Some(class) = store.get(current_id) {
            let mut inst_idx = class.first_field_index;
            for f in &class.fields {
                if f.is_static() {
                    continue;
                }
                let desc_first = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                shared
                    .mem
                    .heap
                    .set_field(obj, inst_idx, jvm_default_for_descriptor(desc_first));
                inst_idx += 1;
            }
            cid = class.superclass;
        } else {
            break;
        }
    }
}

/// [`init_primitive_fields`] plus JLS §12.6 finalizer registration, for an
/// instance whose constructor chain is about to run: the interpreter's `new`
/// (`gc_alloc_object`) and the `NativeContext` constructors that allocate AND
/// invoke `<init>` (`new_object_initialized`,
/// `new_object_initialized_with_class_id` -- reflective
/// `Constructor.newInstance` among them). The plain `new_object` allocates
/// without a constructor and does not register.
///
/// # Which allocations register, and why not every one
///
/// HotSpot registers a finalizable instance when its `Object.<init>` returns
/// (`RegisterFinalizersAtInit`, default true), plus once for `JVM_Clone`. So
/// an instance whose constructor runs is registered, and one that is merely
/// allocated -- `Unsafe.allocateInstance`, JNI `AllocObject` -- is NOT. This
/// VM registers at allocation instead of at `Object.<init>`, so the
/// allocation sites that are always followed by a constructor call are the
/// ones that register, and `allocate_instance` deliberately does not
/// (`common-d-finalizable-objects-from-native-allocation-paths-are-never-registered`,
/// gc-common w2-d). Before this, only the interpreter's and the JIT's `new`
/// registered at all, so an instance built by reflection was collected
/// without its `finalize()` ever running.
///
/// Exactly one registration per object: none of these callers registers on
/// its own, and the JIT's `new` has its own registration
/// (`jit_post_alloc_init`) and never comes through here.
///
/// Registering before the constructor runs also registers an object whose
/// constructor throws before `super()` / `this(..)` completes, which HotSpot
/// never finalizes. Since JIT round 21 wave 10 (lane finalize,
/// `CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`, default ON) such a
/// registration is withdrawn when the interpreter's unwinder proves it
/// (`exception_dispatch::unconstructed_mark_of`); a constructor that ran
/// compiled keeps it.
#[inline]
pub(crate) fn init_new_instance(shared: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    if init_primitive_fields(shared, obj, class_id) {
        shared.register_finalizable(obj.as_ptr() as usize); // Cast: GC object pointer to address    
}
    // gcg g4/rs: opt-in discovery at allocation (one relaxed load when off).
    crate::memory::ref_discovery::note_allocation(shared, obj, class_id);}

/// [`init_primitive_fields`] for a CONSTRUCTOR-LESS native allocation
/// (`Unsafe.allocateInstance`, `NativeContext::allocate_instance`,
/// `instantiate(.., NoConstructor)`, `try_instantiate_no_constructor`): the
/// defaults, no finalizer registration (HotSpot registers when `Object.<init>`
/// returns and none runs here; a constructor `MethodHandle` registers at its
/// `linkToSpecial` of `<init>`, native-builtins
/// `link_to_special_constructor_registers`), and -- JIT round 21 wave 8, lane
/// reflect (`VmConfig::native_alloc_notes_references`,
/// `CRATONVM_NATIVE_ALLOC_NOTES_REFERENCES`, default ON) -- the discovery note
/// of a `java.lang.ref` subclass, as [`init_new_instance`] makes. HotSpot
/// discovers a `Reference` by its class whoever allocated it, and every
/// constructor `MethodHandle` (so every JDK 25 `Constructor.newInstance`)
/// allocates through `Unsafe.allocateInstance`: a reflectively built
/// `WeakReference` subclass was never cleared or enqueued. One that no
/// constructor ever binds is requeued by the pre-collection binder until it
/// dies (null queue slot), and one bound with a null referent is dropped, as
/// HotSpot never discovers it.
#[inline]
pub(crate) fn init_no_constructor_instance(shared: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    init_primitive_fields(shared, obj, class_id);
    if shared.config.native_alloc_notes_references {
        crate::memory::ref_discovery::note_allocation(shared, obj, class_id);
    }
}

/// Register `obj` for finalization when its class has a finalizer: the body of
/// `NativeContext::register_if_finalizable` (`Object.clone()`,
/// `Finalizer.register`, a constructor `MethodHandle`'s `linkToSpecial` of
/// `<init>`) and of the compiled `linkToSpecial` link
/// (`jit::helpers::mh_compiled_link_target`). JIT round 21 wave 8, lane
/// reflect: every constructor `MethodHandle` call asks it now, so the
/// published allocation recipe answers first, lock-free, as the interpreter's
/// `new` reads it; a class with no recipe takes the class-manager read, as
/// before. At most one row per object (`ReferenceProcessor::discover_reference`).
pub(crate) fn register_if_finalizable_shared(shared: &SharedVm, obj: ObjectRef) {
    if class_has_finalizer_shared(shared, obj) {
        shared.register_finalizable(obj.as_ptr() as usize); // Cast: GC object pointer to address
    }
}

/// Whether `obj`'s class has a finalizer: the published allocation recipe
/// first, lock-free, else the class-manager read. The question
/// [`register_if_finalizable_shared`] asks, shared with the unconstructed-object
/// withdrawal (JIT round 21 wave 10, lane finalize:
/// `exception_dispatch::unconstructed_mark_of`).
pub(crate) fn class_has_finalizer_shared(shared: &SharedVm, obj: ObjectRef) -> bool {
    class_has_finalizer_of(shared, shared.mem.heap.class_id_of(obj))
}

/// A native constructor door abandoned a freshly allocated object before
/// invoking its `<init>` (for example, argument adaptation failed). Unlike the
/// constructor-throw path below, the caller itself proves no constructor frame
/// ran, so no unwinder note is needed.
pub(crate) fn withdraw_finalizable_before_constructor(
    shared: &SharedVm,
    obj: ObjectRef,
    epoch: u64,
) -> bool {
    if shared.mem.address_epoch() != epoch
        || !class_has_finalizer_shared(shared, obj)
        || !super::finalizer_register_at_object_init_enabled(&shared.config)
    {
        return false;
    }
    shared.withdraw_unconstructed_finalizable_now(
        obj.as_ptr() as usize,
        epoch,
    )
}

/// A native construction door's withdrawal (JIT round 21 wave 10, lane
/// finalize, `CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`, default ON): `obj`
/// is the instance the door allocated and registered, its constructor call
/// returned an exception, and `epoch_before_call` is the address epoch read
/// before that call. When the interpreter's unwinder proved the constructor
/// threw before `Object.<init>` returned
/// (`exception_dispatch::unconstructed_mark_of`, noted through
/// `SharedVm::note_unconstructed_finalizable`) and no collection has run
/// since, the registration goes: HotSpot never registered it. The doors: the
/// `NativeContext` constructions (`new_object_initialized*`), JNI `NewObject*`
/// and a JNI `<init>` call that registered its receiver, and a constructor
/// `MethodHandle`'s `linkToSpecial` (native-builtins, through
/// `NativeContext::withdraw_unconstructed_finalizable`). Returns whether a
/// registration was withdrawn.
pub(crate) fn withdraw_unconstructed_finalizable_at_door(
    shared: &SharedVm,
    obj: ObjectRef,
    epoch_before_call: u64,
) -> bool {
    if shared.mem.address_epoch() != epoch_before_call
        || !class_has_finalizer_shared(shared, obj)
        || !super::finalizer_register_at_object_init_enabled(&shared.config)
    {
        return false;
    }
    shared.withdraw_unconstructed_finalizable_noted(
        obj.as_ptr() as usize, // Cast: GC object pointer to address
        epoch_before_call,
    )
}

/// [`class_has_finalizer_shared`] by class id.
pub(in crate::runtime::interpreter::gc_and_alloc) fn class_has_finalizer_of(shared: &SharedVm, class_id: ClassId) -> bool {
    let cached = if shared.jit.jit_alloc_class_cache.enabled() {
        shared
            .jit
            .jit_alloc_class_cache
            .get(class_id.as_u32())
            .map(|info| info.has_finalizer)
    } else {
        None
    };
    cached.unwrap_or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .class_store
            .get(class_id)
            .map_or(false, |c| c.has_finalizer)
    })
}

/// TLAB fast path for object allocation. Returns None on TLAB miss.
///
/// T19.3.G1 (GC allocation-storm): the refill size consulted here is
/// adaptive — after the first refill on a given thread the tracker
/// inside the thread's [`cratonvm_gc::Tlab`] recommends the next size
/// based on fill time and alloc count, so a thread that just burned
/// through 64 KB in under a millisecond gets a 128 KB chunk next
/// time and so on up to the documented cap. Each refill bumps
/// `shared.mem.tlab_refill_count` so operators can spot-check refill behavior
/// without adding a per-allocation shared metric.
#[inline(always)]
pub(crate) fn tlab_alloc_object(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    num_fields: usize,
    total_size: usize,
) -> Option<ObjectRef> {
    let (body_size, gc_flags) = shape_of_reserved(num_fields, total_size);
    tlab_alloc_object_inner(
        thread, shared, class_id, num_fields, body_size, gc_flags, total_size, false,
    )
}

/// TLAB hit-only path for the tiny byte arrays backing compact dynamic Strings.
/// The caller falls back to the heap allocator on a miss, so this never refills
/// or collects after another newly-created object is live in its native helper.
pub(crate) fn tlab_alloc_byte_array(
    thread: &mut JvmThread,
    shared: &SharedVm,
    length: usize,
) -> Option<ObjectRef> {
    use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET};
    let length_u32 = u32::try_from(length).ok()?;
    // The object's real footprint, computed exactly as `tlab_alloc_array`
    // computes it: the payload is rounded up to the 8-byte grid. This used to
    // be `HEADER_SIZE + length`, unrounded. The TLAB cursor rounded it anyway,
    // but `note_tlab_object` (whose ZGC registry audit reads it as "what the
    // cursor advanced by"), the a2 breadcrumb and the allocation batch all saw
    // up to 7 bytes less than the object occupies (gc-common w1-f).
    let data_size = cratonvm_gc::heap::array_data_size_checked(length, ArrayElementType::Byte)?;
    let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
    if total_size > cratonvm_gc::tlab::tlab_max_alloc() {
        return None;
    }
    let ptr = thread.tlab.alloc_initialized(total_size, 8, |ptr| {
        let header = ObjectHeader::new(
            ClassId::new(0),
            ObjectKind::Array,
            ArrayElementType::Byte,
            length_u32,
            length_u32,
        );
        // SAFETY: `ptr` is the base of a TLAB chunk the allocator just
        // reserved for this object and has not published, so nothing can race
        // the store. It is header-aligned, at least `size_of::<ObjectHeader>()`
        // bytes, and uninitialised — hence `ptr::write`, not an assignment.
        unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
        // ZGC registers every TLAB object the moment its header is complete
        // (`VmHeap::note_tlab_object`); a no-op on the linear-sweep backends.
        shared.mem.heap.note_tlab_object(ptr, total_size);
    })?;
    note_tlab_allocation(thread, shared, total_size);
    cratonvm_gc::a2dbg::record(
        ptr as usize,
        0,
        ObjectKind::Array as u8,
        ArrayElementType::Byte as u8,
        length_u32,
        length_u32,
        total_size,
    );
    // SAFETY: `ptr` is the just-initialised object base from the TLAB bump
    // above — non-null, header-aligned, and its header was written before this
    // point, so it is a well-formed object reference.
    Some(unsafe { ObjectRef::from_raw(ptr) })
}

/// [`tlab_alloc_object`] for the JIT allocation slow path (`jit_new_object`):
/// identical bump allocation, but the REFILL arm first asks — in O(1) —
/// whether young can supply the chunk WITHOUT another GC (bump-tail headroom
/// OR a coalesced reclaimed span via the cached-bound early-exit probe), and
/// bails to the caller's old-gen spill when it cannot.
///
/// History (why the gate looks like this): the JIT slow path historically
/// never refilled TLABs — once the inline bump's TLAB filled, EVERY
/// allocation took the global slow chain. Round 1 (2026-07-06) tried the
/// interpreter's unconditional refill: bt16 got 10x faster but bt18 (large
/// LIVE young set under the non-moving sweep) collapsed 10.6s→463s; gating
/// on `try_alloc_young_probe(requested)` still 39s (its
/// `largest_free_block` fallback FULL-SCANNED the fragmented free list per
/// allocation — 55% of wall); a bump-tail-only gate still ~119s. Round 2
/// uses the machinery that did not exist then: `young_has_free_block`
/// early-exits at the first satisfying span and fail-fasts through the
/// cached `Arena::max_free_upper` bound, so a post-sweep+coalesce young (a
/// handful of big spans) serves refills at bump speed, while a
/// genuinely-full young answers `false` in O(1) → old-gen spill exactly
/// like the historical no-refill behaviour.
#[inline(always)]
pub(crate) fn tlab_alloc_object_guarded_refill(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    num_fields: usize,
    total_size: usize,
) -> Option<ObjectRef> {
    let (body_size, gc_flags) = shape_of_reserved(num_fields, total_size);
    tlab_alloc_object_inner(
        thread, shared, class_id, num_fields, body_size, gc_flags, total_size, true,
    )
}

/// The ARRAY twin of [`tlab_alloc_object_guarded_refill`], for the JIT's
/// `newarray`/`anewarray` helpers.
///
/// Until this existed the JIT had no TLAB path for arrays *at all*. Its object
/// sites bump inline (`emit_inline_tlab_new`) and miss into the guarded refill
/// above; its array sites had neither, so every single JIT-compiled array
/// allocation went to `try_alloc_young_probe` + `try_alloc_array`, which take
/// the global `young_from` mutex twice — the second time holding it across the
/// bump, the zeroing AND the header init, so hold time scales with the array's
/// size. The interpreter's `gc_alloc_array` grew its TLAB arm on 2026-07-25
/// (`7888b80b6`, +36% aggregate at 4 threads) and the JIT's was left behind;
/// `AllocScaleProbe` reports the difference as a `long[16]` aggregate scaling
/// factor stuck near 1.00x while `new Object()` scales.
///
/// `refill_needs_young_room = true` for the same reason the object twin passes
/// it: on this path a refill that forces a collection is worse than spilling to
/// old gen, because the caller has a cheaper fallback and, unlike the
/// interpreter, may be holding JIT frames the collector cannot map precisely.
#[inline(always)]
pub(crate) fn tlab_alloc_array_guarded_refill(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    element_type: ArrayElementType,
    length: usize,
) -> Option<ObjectRef> {
    use cratonvm_gc::heap::{ObjectKind, ARRAY_DATA_OFFSET, HEADER_SIZE};
    let length_u32 = u32::try_from(length).ok()?;
    let data_size = cratonvm_gc::heap::array_data_size_checked(length, element_type)?;
    let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
    // Anything at or above the TLAB's per-allocation cap goes down the ordinary
    // path, which owns the young-vs-old-gen (humongous) routing decision.
    if total_size > cratonvm_gc::tlab::tlab_max_alloc() {
        return None;
    }
    let obj = tlab_alloc_shaped_inner(
        thread,
        shared,
        class_id,
        TlabShape::Array {
            element_type,
            length_u32,
        },
        total_size,
        true,
    )?;
    cratonvm_gc::a2dbg::record(
        obj.as_ptr() as usize,
        class_id.as_u32(),
        ObjectKind::Array as u8,
        element_type as u8,
        length_u32,
        length_u32,
        total_size,
    );
    Some(obj)
}

/// DBG (CRATONVM_DBG_INVOKESTATS): counts invoke-dispatch path outcomes to
/// diagnose whether the monomorphic inline cache (`InvokeCache`) is actually
/// staying warm for a workload, or whether calls are falling through to the
/// vtable-fast / full-slow-path resolution on every call. index: 0=inline
/// cache HIT, 1=inline cache MISS, 2=vtable_fast reached, 3=execute_invoke_kind
/// (full slow path) reached. Prints a running tally every 100000 events on
/// each counter to bound output volume for long-running processes.
///
/// The off test inline, the count out of line (interpreter round i2 wave 57,
/// lane L7), as `site_stats::bump` since wave 50: this is called on every
/// invoke-door and inline-cache hit (fourteen sites) and on every
/// signature-polymorphic route-cache hit, and a body this size (a static
/// table, a formatted `eprintln!`) is one fat LTO may keep out of line --
/// a call per hit to return at once in every run without
/// `CRATONVM_DBG_INVOKESTATS`.
///
/// `on` is `VmConfig::invoke_stats` (the same variable, read once per VM;
/// interpreter round i2 wave 60, lane L7): the caller's `shared.config` load,
/// where this was a process `OnceLock` read per call.
#[inline(always)]
pub(in crate::runtime::interpreter) fn dbg_invoke_stats_record(on: bool, index: usize) {
    if on {
        dbg_invoke_stats_counted(index);
    }
}

#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::gc_and_alloc) fn dbg_invoke_stats_counted(index: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Index 4: a signature-polymorphic site served by its cached route
    // (`sig_poly_site`, wave 48, lane L7).
    static COUNTS: [AtomicU64; 5] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    let Some(counter) = COUNTS.get(index) else {
        return;
    };
    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if n % 100000 == 1 {
        eprintln!(
            "[invokestats] cache_hit={} cache_miss={} vtable_fast={} slow_path={} sigpoly_hit={}",
            COUNTS[0].load(Ordering::Relaxed),
            COUNTS[1].load(Ordering::Relaxed),
            COUNTS[2].load(Ordering::Relaxed),
            COUNTS[3].load(Ordering::Relaxed),
            COUNTS[4].load(Ordering::Relaxed),
        );
    }
}

/// DBG (CRATONVM_DBG_TLABMISS): gate-failure state dump — the live young
/// arena facts at the moment the guarded-refill young-room gate said no.
/// Sampled every 2^20 failures (plus the first).
pub(in crate::runtime::interpreter) fn dbg_refill_fail_state(shared: &SharedVm, requested: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| None::<std::ffi::OsString>.is_some())
    {
        return;
    }
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    if n & 0xFFFFF == 1 {
        let (used, cap, fl, largest) = shared.mem.heap.young_arena_diag();
        eprintln!(
            "[gatefail] n={n} requested={requested} used={used}/{cap} free_list={fl} largest_free={largest} headroom={} has_free={}",
            shared.mem.heap.young_bump_headroom(requested),
            shared.mem.heap.young_has_free_block(requested),
        );
    }
}

/// DBG (CRATONVM_DBG_TLABMISS): which step of the guarded TLAB refill fails
/// and with what request size. stage: 0=young-room gate, 1=refill_tlab
/// returned None. Prints every 2^20 events per stage.
#[inline]
pub(in crate::runtime::interpreter) fn dbg_refill_fail(stage: usize, requested: usize) {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| None::<std::ffi::OsString>.is_some())
    {
        return;
    }
    static COUNTS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
    static LAST_REQ: AtomicUsize = AtomicUsize::new(0);
    LAST_REQ.store(requested, Ordering::Relaxed);
    let n = COUNTS[stage].fetch_add(1, Ordering::Relaxed) + 1;
    if n & 0xFFFFF == 1 {
        eprintln!(
            "[refillfail] gate={} refill_none={} last_requested={}",
            COUNTS[0].load(Ordering::Relaxed),
            COUNTS[1].load(Ordering::Relaxed),
            requested,
        );
    }
}

/// Refilled bytes between two consults of the young trigger on the healthy
/// path: 4 MiB, i.e. every 4–16 full-size TLABs. Small against any semi-space
/// the trigger is worth having on, large enough that a mini-TLAB storm still
/// consults `needs_gc` (one `Mutex` acquisition) a few hundred times per
/// gigabyte rather than per refill.
///
/// ## The refill-bytes re-arm metric (`shared.mem.tlab_wedge.refill_bytes_since_gc`)
///
/// Bytes handed out by SUCCESSFUL TLAB refills since the last refill-time
/// `needs_gc()` fire — the second re-arm metric, for the HEALTHY path. The
/// counter, and the three wedge-breaker counters beside it, live in the VM's
/// heap realm (`vm/src/vm/realms/heap_realm.rs` `TlabWedgeState`) since
/// gc-common w6-c; before, they were process statics, so one VM's refills
/// re-armed another's consult (`common-f-process-global-allocation-state`).
///
/// `gen-gc-minor-pause-20260902` swept `CRATONVM_GC_YOUNG_TRIGGER_PERCENT`
/// over 50/75/90 and got identical collection counts at every setting, with
/// `young_bytes_before` equal to the from-space CAPACITY on every cycle: the
/// collections were driven by allocation failure, never by the trigger. The
/// slow-path entry counter (`tlab_wedge.slowpath_entries_since_gc`) was
/// why. It was sized for the degraded modes it guards
/// (a per-object slow path enters tens of thousands of times per second), but
/// a healthy JIT workload refills a 256 KiB–1 MiB TLAB per slow-path entry
/// and exhausts a 256 MiB semi-space in a few hundred entries — never the
/// 65,536 the gate demanded. So on exactly the workloads that allocate the
/// most, the trigger was consulted zero times per cycle, the from-space ran
/// to capacity, and the pause-goal feedback that moves the threshold
/// (`adapt_young_trigger_to_pause`) moved a number nothing read.
///
/// Two metrics, OR-ed: the entry count still fires in the crumb wedge (where a
/// bytes stamp freezes: the per-object path does not bump
/// `bytes_allocated_total`), and the bytes count fires on healthy TLAB flow
/// after every `NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES` of refills.
/// `needs_gc` carries its own anti-livelock floor, so consulting it more often
/// cannot storm a young gen whose live set sits above the threshold.
pub(in crate::runtime::interpreter::gc_and_alloc) const NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES: u64 = 4 * 1024 * 1024;

// zt t2/rv (`zgc-zs2rv-wave-s1-review-residuals-CLOSED-20261019.md` item 6): the ZGC
// director's poll latency (`director::poll_latency_bytes`) assumes this step;
// the gc crate cannot name it, so the two are pinned here at compile time.
#[cfg(feature = "zgc")]
const _: () = assert!(
    NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES == cratonvm_gc::zgc::director::POLL_REFILL_CONSULT_BYTES
);

/// The entry-count re-arm of the refill-time young trigger (the crumb wedge's
/// metric; see the trigger in [`tlab_alloc_shaped_inner`]).
pub(in crate::runtime::interpreter::gc_and_alloc) const NEEDSGC_MIN_ENTRIES_BETWEEN_FIRES: u64 = 65_536;

/// Publish the current thread's bounded TLAB-accounting batch. This is called
/// at a refill, GC, or wedge decision so the shared byte total is exact when a
/// policy consumes it, while ordinary bump allocations only mutate fields
/// owned by their JvmThread.
#[inline(always)]
pub(in crate::runtime::interpreter::gc_and_alloc) fn flush_tlab_allocation_batch(thread: &mut JvmThread, shared: &SharedVm) {
    let _ = shared;
    thread.tlab.flush_vm_allocation_batch();
}

/// Charge a successful TLAB allocation without touching a shared cache line
/// until the per-thread accumulator reaches its bounded batch size.
#[inline(always)]
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_tlab_allocation(thread: &mut JvmThread, shared: &SharedVm, bytes: usize) {
    let _ = shared;
    let _ = thread.tlab.note_vm_tlab_allocation(bytes);
}

/// Charge an allocation that bypassed the TLAB to the thread's own total
/// (`getThreadAllocatedBytes`) and, in batches, to this VM's
/// `thread_allocated_total` (`getTotalThreadAllocatedBytes`). Attaches the
/// thread's buffer to that total first, so a thread whose allocations all
/// miss the TLAB is still counted by its own VM (gc-common w6-c).
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_thread_external_allocation(thread: &mut JvmThread, shared: &SharedVm, bytes: usize) {
    thread
        .tlab
        .ensure_vm_thread_allocation_total(&shared.mem.thread_allocated_total);
    thread.tlab.note_external_allocation(bytes);
    // A shared-heap allocation moved the occupancy: `maybe_gc` asks next.
    note_occupancy_may_have_moved(thread);
}

/// Gate for the two refill-time GC triggers (the wedge-breaker and the
/// `needs_gc()` consult) — the crumb-treadmill cure (10.5M consecutive
/// refill failures, one GC per 23 s run without them). **Default ON**
/// (opt out with `CRATONVM_TLAB_GC_TRIGGER=0`).
///
/// History: the triggers shipped default-OFF (perf/halfgap-20260717)
/// because the extra mid-drain collections exposed a latent walk-grid
/// corruption (bt18 5/5 wrong checksums, "cursor overshot into free
/// block"). Root cause fixed 2026-07-18: an unaligned young-arena capacity
/// (1 GiB - 4) made `refill_tlab`'s `requested.min(available)` mint
/// unaligned TLAB sizes whose free-list split remnants sat off the 8-byte
/// object grid (plus an untracked `Tlab::new` round-down sliver), derailing
/// the non-moving walk and truncating the mark oracle. See
/// tlab-trigger-gc-young-walk-corruption-FIXED.md; the arena
/// now enforces grid alignment end-to-end and the mark oracle fails safe
/// above a truncated walk's frontier.
pub(in crate::runtime::interpreter::gc_and_alloc) fn tlab_gc_trigger_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map(|v| {
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            })
            .unwrap_or(true)
    })
}

/// The size the JIT TLAB-refill gate in [`tlab_alloc_shaped_inner`] probes the
/// young BUMP TAIL for. OFF (`CRATONVM_TLAB_GATE_BUMP_FLOOR` unset, the
/// default) it is `requested`, exactly as before. ON it is what
/// `GenerationalHeap::refill_tlab`'s fragmentation fallback can serve from a
/// tail — the fragmentation floor — raised to the object in hand (a mini-TLAB
/// that cannot hold it is a refill wasted), and never above `requested`.
/// gen r4w3/alloc3, 2026-09-23.
pub(in crate::runtime::interpreter::gc_and_alloc) fn tlab_gate_bump_probe(
    gate_bump_floor: bool,
    free_block_floor: usize,
    total_size: usize,
    requested: usize,
) -> usize {
    if gate_bump_floor {
        free_block_floor.max(total_size).min(requested)
    } else {
        requested
    }
}

/// Wedge-breaker for the guarded TLAB refill (perf/halfgap-20260717, the
/// second TLAB-remnant wedge — see `cratonvm_gc::tlab::FRAG_TLAB_FLOOR` for
/// the live capture).
///
/// The guarded-refill young-room gate can fail on EVERY allocation for the
/// rest of a run: tiny per-object allocations keep succeeding off free-list
/// slivers, so no allocation failure ever forces the young collection whose
/// sweep+coalesce would heal the fragmentation (observed live: 10.5 million
/// consecutive gate failures, one young GC in a 23-second run). This
/// counts CONSECUTIVE gate failures and, past a threshold, forces one
/// orchestrated collection so TLAB flow can resume.
///
/// Storm guard: a forced break re-arms only after `WEDGE_REARM_BYTES` of
/// further allocation (read from `shared.mem.bytes_allocated_total`). If the
/// collection did not heal the free list (nothing coalescable — genuinely
/// full young of live data), the gate keeps failing but no further forced
/// collections fire until real allocation progress has been made, so the
/// worst case adds one young GC per `WEDGE_REARM_BYTES` allocated — never
/// a per-allocation GC storm (the failure mode that forced the round-1
/// unconditional-refill revert documented at the JIT call site).
///
/// The breaker's state is per VM, `shared.mem.tlab_wedge` (gc-common w6-c):
/// the successful-refill reset in `tlab_alloc_shaped_inner` shares the
/// consecutive-failure counter with this function, and another VM in the
/// same process can no longer count toward this VM's forced collection.
pub(in crate::runtime::interpreter) fn tlab_refill_wedge_break(
    thread: &mut JvmThread,
    shared: &SharedVm,
    site: &'static str,
) -> bool {
    use std::sync::atomic::Ordering;
    /// Consecutive gate failures before a forced collection. At the
    /// observed wedge rate this is a few milliseconds of per-object
    /// slow-path work — long enough that transient pressure never trips
    /// it, short enough that a real wedge is broken almost immediately.
    const WEDGE_BREAK_THRESHOLD: u64 = 16_384;
    /// Allocation progress required before a second forced collection.
    const WEDGE_REARM_BYTES: u64 = 64 * 1024 * 1024;

    let wedge = &shared.mem.tlab_wedge;
    flush_tlab_allocation_batch(thread, shared);
    let fails = wedge.gate_consecutive_fails.fetch_add(1, Ordering::Relaxed) + 1;
    if fails < WEDGE_BREAK_THRESHOLD {
        return false;
    }
    let alloc_total = shared.mem.bytes_allocated_total.load(Ordering::Relaxed);
    cratonvm_types::gc_entry_census::note_alloc_total(alloc_total);
    let last = wedge.last_break_alloc_total.load(Ordering::Relaxed);
    if last != 0 && alloc_total.saturating_sub(last) < WEDGE_REARM_BYTES {
        return false;
    }
    if wedge
        .last_break_alloc_total
        .compare_exchange(
            last,
            alloc_total.max(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        )
        .is_err()
    {
        // Another thread is breaking the same wedge; let it.
        return false;
    }
    wedge.gate_consecutive_fails.store(0, Ordering::Relaxed);
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    // zr z3/t2: `site` names which wedge: the refill GATE's
    // (`tlab-refill-wedge-gate`; on ZGC the gate is `!needs_gc()`, so an
    // occupancy verdict) or `refill_tlab`'s own refusal (`tlab-refill-wedge`,
    // a failed allocation).
    maybe_gc_forced_at(shared, thread, site);
    true
}

/// gci i4/yg (`CRATONVM_GEN_YOUNG_EXHAUSTED_REFILL_COLLECTS`, opt-in, read per
/// heap): a JIT-guarded refill found young unable to hold the object at all.
/// When the Generational heap grants it
/// (`GenerationalHeap::young_exhausted_refill_wants_collection`: once per young
/// cycle, after a quarter of the semi was carved since the cycle's first
/// refill), collect young here -- HotSpot Serial's answer to a failed eden
/// allocation -- and return `true` so the caller retries the refill once,
/// exactly as after [`tlab_refill_wedge_break`]; otherwise `false` and the
/// caller's old-generation spill runs as before. Always `false` on G1 / ZGC
/// and with the switch off (one heap-field branch).
///
/// The door is the wedge breaker's (same call site, same retire-then-force
/// order), counted as a forced collection under its own site name
/// (`tlab-refill-exhausted` in the forced-site census and `ladder_forced`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn young_exhausted_refill_collect(thread: &mut JvmThread, shared: &SharedVm) -> bool {
    if !shared.mem.heap.is_generational() {
        return false;
    }
    if !shared.mem.heap.gen_young_exhausted_refill_wants_collection() {
        return false;
    }
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    maybe_gc_forced_at(shared, thread, "tlab-refill-exhausted");
    true
}

/// The interpreter's TLAB fast path allocates the COMPACT body shape for
/// classes that have one, instead of the uniform 16-byte-cell layout it wrote
/// for years — **default ON since 2026-09-03**, opt out with
/// `CRATONVM_COMPACT_TLAB_ALLOC=0`.
///
/// # Why this is the right shape
///
/// It is the shape everything else already uses. The JIT's inline `new`
/// (`emit_inline_tlab_new`) has emitted compact bodies for months, and
/// `gen_heap::alloc_object` — the TLAB-miss and large-object path — plans them
/// too. Only this path did not, so the same class got one shape or the other
/// depending on which allocator happened to serve it, and every compact fast
/// path in the JIT needed a second legacy-shaped arm to cope. Three of those
/// arms were added in the week before this flipped.
///
/// # What earned the default
///
/// It shipped OFF first, because the one previous attempt at this unification
/// miscompiled `probes/FjpProbe.java` and the comment it left demanded the
/// change be made at every allocation site at once with that probe in the gate.
/// The switch then reproduced that miscompile deterministically, which is how
/// its root cause was found: the `Integer`/`Long` boxing fast paths wrote a raw
/// 16-byte `Value` cell under a SAFETY comment asserting the object was
/// legacy-layout — an assumption about which allocator the site calls, not a
/// property of the object.
///
/// With that fixed, the evidence for turning it on:
///
/// * `regression-suite/run.sh` 89/89 with the shape enabled, on the default
///   collector, under `-XX:+UseGenerationalGC` and under `-XX:+UseG1GC` — a
///   HotSpot-differential oracle, not a self-comparison.
/// * A 228-program differential soak per collector: every workload run twice
///   with the shape off to establish it is reproducible at all, then once with
///   it on, comparing exit status and stdout byte for byte. 164 deterministic
///   programs agree on Generational; the two that differ are a clock probe and
///   `RandomLeak`, which prints `javaHeapUsed` and reports **35% less heap**
///   (14.7 MB against 9.6 MB) for identical program output.
/// * `org.h2.test.unit.TestCache`, a real application: `rc=0`, 1,872,185 of
///   2,508,687 objects compacted, **87.5 MB less allocated**.
/// * Throughput is a wash — eight alternated rounds, medians 16.6 s either way.
///   The prize here is memory, and `header-shrink.md` always said it would be.
///
/// `=0` restores the legacy shape exactly, and remains the first thing to set
/// if an object is ever suspected of being read at the wrong offset.
pub(crate) fn compact_tlab_alloc_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// TLAB objects given the compact body shape, and those left legacy.
///
/// A count needs its complement to be readable: "compact=0" means either that
/// the switch is off or that no allocated class has a registered layout, and
/// those are different facts.

/// Bytes the compact shape saved against what the legacy shape would have
/// taken for the same allocations. The point of the change, in the only unit
/// that matters.

/// Whether the three shape counters above are kept at all.
///
/// They are printed only by the exit report under
/// `CRATONVM_DBG_JIT_METHOD_STATS` (`vm-cli`'s `TLAB object shapes:` line), and
/// kept unconditionally they were two `lock xadd`s on process-global lines per
/// allocated object — most of `plan_tlab_object_shape_at`'s 18-22% self time on
/// bintrees/hashmap (round 9 wave 2 `perfdiag`,
/// `perf-compact-tlab-planner-reads-an-env-var-per-allocation-20260918.md`).
/// `flags()` is a cached struct: one relaxed load and a `OnceLock` read.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn tlab_shape_stats_enabled() -> bool {
    cratonvm_types::flags().jit.method_stats
}

/// `(compact, legacy, bytes_saved)` for TLAB object allocations.
///
/// All zero unless `CRATONVM_DBG_JIT_METHOD_STATS` is on
/// ([`tlab_shape_stats_enabled`]); the only reader prints under that switch.
pub fn tlab_object_shape_counts(shared: &SharedVm) -> (u64, u64, u64) {
    use std::sync::atomic::Ordering;
    (
        shared
            .mem
            .diagnostics
            .tlab_compact_objects
            .load(Ordering::Relaxed),
        shared
            .mem
            .diagnostics
            .tlab_legacy_objects
            .load(Ordering::Relaxed),
        shared
            .mem
            .diagnostics
            .tlab_compact_bytes_saved
            .load(Ordering::Relaxed),
    )
}

/// The shape a TLAB object allocation should take: `(total_size, body_size,
/// gc_flags)`, where a `body_size` of 0 and no flags mean the legacy uniform
/// 16-byte-cell layout.
///
/// ONE lookup per allocation, and the result is carried to the header stamp
/// rather than recomputed there. Two independent lookups could disagree if the
/// class's layout were replaced between them (the exact hazard the JIT's inline
/// emitter carries a layout-replace guard for), and a body sized by one lookup
/// with a header stamped by the other is heap corruption.
/// Which TLAB object sites may plan the compact shape, as a bitmask —
/// `CRATONVM_COMPACT_TLAB_SITES`, default all.
///
/// A bisection lever, not a tuning knob. `CRATONVM_COMPACT_TLAB_ALLOC=1`
/// reproduces the `FjpProbe` miscompile in one run but says nothing about
/// WHICH of the five sites is responsible, and each answer would otherwise cost
/// a fifteen-minute rebuild. See [`TlabSite`].
pub(crate) fn compact_tlab_site_mask() -> u32 {
    static G: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *G.get_or_init(
        || match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => v.trim().parse::<u32>().unwrap_or(u32::MAX),
            Err(_) => u32::MAX,
        },
    )
}

/// Whether `CRATONVM_DBG_COMPACT_TLAB` is set, read ONCE.
///
/// It used to be a `runtime_var_os` per compact allocation — 99.6% of every
/// flag read in the process on bintrees and ~8% of wall time in the read
/// machinery (hash probe, UTF-8 check, census). `dbg_jit_alloc_filter` in
/// `jit/helpers.rs` records the identical defect once before.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn dbg_compact_tlab_enabled() -> bool {
    tlab_planner_gates().dbg_compact_tlab
}

/// The planner's three latched switches as ONE latched word:
/// `CRATONVM_COMPACT_TLAB_ALLOC` folded into the site mask (0 when the compact
/// shape is off, so `mask & site` answers both questions), and
/// `CRATONVM_DBG_COMPACT_TLAB`.
///
/// gc-common w4-c, first step of `common-f-proposal-one-allocation-debug-gate-REJECTED-20260927`:
/// the compact arm of [`plan_tlab_object_shape_at`] read three distinct
/// `OnceLock`s per allocation (enable, site mask, debug); it now reads one.
/// Semantics-preserving: each input was already latched for the life of the
/// process, and [`compact_tlab_alloc_enabled`] / [`compact_tlab_site_mask`]
/// stay as the JIT's accessors and are what this is computed from.
#[derive(Clone, Copy)]
pub(in crate::runtime::interpreter::gc_and_alloc) struct TlabPlannerGates {
    /// Sites allowed to plan the compact shape; 0 when the shape is off.
    pub(in crate::runtime::interpreter::gc_and_alloc) site_mask: u32,
    /// `CRATONVM_DBG_COMPACT_TLAB`.
    pub(in crate::runtime::interpreter::gc_and_alloc) dbg_compact_tlab: bool,
}

#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn tlab_planner_gates() -> TlabPlannerGates {
    static G: std::sync::OnceLock<TlabPlannerGates> = std::sync::OnceLock::new();
    *G.get_or_init(|| TlabPlannerGates {
        site_mask: if compact_tlab_alloc_enabled() {
            compact_tlab_site_mask()
        } else {
            0
        },
        dbg_compact_tlab: None::<std::ffi::OsString>
            .is_some(),
    })
}

/// One line per distinct class that gets the compact shape —
/// `CRATONVM_DBG_COMPACT_TLAB=1`.
#[cold]
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_compact_tlab_class(class_id: ClassId, num_fields: usize, site: u32) {
    use std::sync::atomic::Ordering;
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<(u32, u32)>>> =
        std::sync::Mutex::new(None);
    static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let mut g = match SEEN.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let seen = g.get_or_insert_with(std::collections::HashSet::new);
    if !seen.insert((class_id.as_u32(), site)) {
        return;
    }
    // Bounded: a runaway class count would drown the run it is meant to
    // explain.
    if COUNT.fetch_add(1, Ordering::Relaxed) >= 200 {
        return;
    }
    let name = cratonvm_gc::gc::resolve_class_info(class_id.as_u32())
        .map(|(n, _)| n)
        .unwrap_or_else(|| "<unresolved>".to_string());
    eprintln!(
        "[compact-tlab] site={site} class={name} id={} num_fields={num_fields}",
        class_id.as_u32()
    );
}

/// Does [`plan_tlab_object_shape_at`] have OBSERVABLE side effects on this
/// run — i.e. may a caller that memoizes its answer be skipping an
/// instrument rather than just a lookup?
///
/// The planner is a pure function of `(class_id, num_fields, site)` except for
/// two diagnostic arms: `tlab_shape_stats_enabled` bumps
/// `TLAB_COMPACT_OBJECTS` / `TLAB_LEGACY_OBJECTS` (and the saved-bytes
/// counter), and `dbg_compact_tlab_enabled` reaches `note_compact_tlab_class`.
/// A caller that caches the plan per `(vm, class, slots)` — which
/// `jit_integer_value_of_direct_body` does for the escaping-box arm — would
/// silently stop both instruments counting its site, so it asks this first and
/// re-plans per call whenever either is armed. Both are latched `OnceLock`s, so
/// this is one relaxed load.
#[inline]
pub(crate) fn tlab_shape_planner_is_instrumented() -> bool {
    tlab_shape_stats_enabled() || dbg_compact_tlab_enabled()
}

#[inline]
/// VM-owned accounting around the pure planner.
pub(crate) fn plan_tlab_object_shape_for(
    shared: &SharedVm,
    class_id: ClassId,
    num_fields: usize,
    site: u32,
) -> (usize, u32, u8) {
    let shape = plan_tlab_object_shape_at(class_id, num_fields, site);
    if tlab_shape_stats_enabled() {
        use std::sync::atomic::Ordering::Relaxed;
        let d = &shared.mem.diagnostics;
        if shape.2 & cratonvm_types::GC_FLAG_COMPACT != 0 {
            d.tlab_compact_objects.fetch_add(1, Relaxed);
            let legacy = cratonvm_gc::heap::HEADER_SIZE + num_fields * cratonvm_gc::heap::SLOT_SIZE;
            d.tlab_compact_bytes_saved
                .fetch_add(legacy.saturating_sub(shape.0) as u64, Relaxed);
        } else {
            d.tlab_legacy_objects.fetch_add(1, Relaxed);
        }
    }
    shape
}

pub(crate) fn plan_tlab_object_shape_at(
    class_id: ClassId,
    num_fields: usize,
    site: u32,
) -> (usize, u32, u8) {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
    use std::sync::atomic::Ordering;
    let legacy_total = HEADER_SIZE + num_fields * SLOT_SIZE;
    let gates = tlab_planner_gates();
    if (gates.site_mask & site) != 0 {
        if let Some(total) = cratonvm_types::compact_tlab_total_size(class_id.as_u32(), num_fields)
        {
            if let Ok(body_u32) = u32::try_from(total) {
                if gates.dbg_compact_tlab {
                    note_compact_tlab_class(class_id, num_fields, site);
                }
                return (total, body_u32, cratonvm_types::GC_FLAG_COMPACT);
            }
        }
    }
    (legacy_total, 0, 0)
}

/// The header shape implied by the size a caller actually RESERVED.
///
/// Derived, never re-planned. The wrappers used to call the planner a second
/// time and `debug_assert` that the two agreed; they cannot be relied on to
/// agree once the planner is site-screened (`CRATONVM_COMPACT_TLAB_SITES`),
/// and a body sized by one answer with a header stamped from the other is heap
/// corruption. Reading the shape back out of the reservation makes the two
/// impossible to separate.
///
/// A compact body packs each field to its natural width (at most 8 bytes), so
/// it is strictly smaller than the legacy `num_fields * SLOT_SIZE` for any
/// non-empty object; equality means legacy. A zero-field object has no body at
/// all and the shapes coincide, which is why it reads as legacy and why that
/// costs nothing.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn shape_of_reserved(num_fields: usize, total_size: usize) -> (u32, u8) {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
    let legacy_total = HEADER_SIZE + num_fields * SLOT_SIZE;
    if total_size == legacy_total {
        return (0, 0);
    }
    // A compact reservation is the layout's TOTAL size (8-byte header
    // included); the second value carries it on for diagnostics.
    match u32::try_from(total_size) {
        Ok(total) => (total, cratonvm_types::GC_FLAG_COMPACT),
        Err(_) => (0, 0),
    }
}

/// Kill switch for [`compact_body_holds_defaults`]: `false` makes the
/// interpreter's `new` write the recipe's defaults into compact bodies again
/// (every store a zero into a zero). Not an env var; flip and rebuild.
pub(in crate::runtime::interpreter::gc_and_alloc) const COMPACT_BODY_DEFAULT_SKIP_ENABLED: bool = true;

/// Does a TLAB object [`tlab_alloc_object`] just reserved at `total_size` for
/// `num_fields` slots already hold every JVM default, so that `gc_alloc_object`
/// may skip the recipe's stores? Exactly when the reservation is COMPACT —
/// read back from the size by [`shape_of_reserved`], the same derivation that
/// stamped the header.
///
/// Why that is sufficient: the TLAB chunk is zeroed when carved
/// (`cratonvm_gc::Tlab::new`'s contract, which the int-family skip already
/// rests on), `init_object_header` zeroes a compact header's second word, and a
/// compact slot is tagless (`types/src/field_layout.rs`): `read_compact_field`
/// reads zero bytes as `Int(0)` / `Long(0)` / `Float(0.0)` / `Double(0.0)` /
/// `Object(None)` at the matching storage kind, and `write_compact_field`
/// stores each of those defaults as zero bytes (pinned by
/// `compact_zero_body_already_holds_every_recipe_default`). So the skipped
/// stores leave the heap byte-for-byte as they would have. A LEGACY slot is
/// different — zero decodes as `Int(0)`, so `long`/`float`/`double` and
/// reference defaults need their tag written — and keeps the stores.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn compact_body_holds_defaults(num_fields: usize, total_size: usize) -> bool {
    COMPACT_BODY_DEFAULT_SKIP_ENABLED
        && (shape_of_reserved(num_fields, total_size).1 & cratonvm_types::GC_FLAG_COMPACT) != 0
}

/// [`plan_tlab_object_shape_at`] for a caller that does not name a site.
///
/// Used by the two TLAB wrappers, which re-plan only to check that the size
/// they were handed is the one the shape asked for; the site screen is the
/// original caller's and must not be applied twice.
#[inline]
pub(crate) fn plan_tlab_object_shape(class_id: ClassId, num_fields: usize) -> (usize, u32, u8) {
    plan_tlab_object_shape_at(class_id, num_fields, u32::MAX)
}

/// Which header [`tlab_alloc_object_inner`] should stamp on the region it
/// reserves. Everything else about the allocation — the TLAB fast path, the
/// refill gate, the wedge breakers, the retire-before-replace protocol — is
/// identical for both shapes, so arrays reuse this function rather than
/// carrying a second, less-hardened copy of that machinery.
#[derive(Clone, Copy)]
pub(in crate::runtime::interpreter) enum TlabShape {
    /// `num_fields` object slots.
    /// `body_size`/`gc_flags` carry the shape [`plan_tlab_object_shape`]
    /// chose, so the header stamp and the size the caller reserved come from
    /// ONE lookup. A zero `body_size` with no flags is the legacy uniform
    /// 16-byte-cell layout.
    Object {
        num_fields: usize,
        body_size: u32,
        gc_flags: u8,
    },
    /// `length` elements of `element_type`.
    Array {
        element_type: ArrayElementType,
        length_u32: u32,
    },
}

impl TlabShape {
    /// Stamp the shape's header at `ptr`.
    ///
    /// SAFETY: the caller must have reserved at least `HEADER_SIZE` bytes at
    /// `ptr`, 8-byte aligned and exclusively owned until the TLAB cursor is
    /// committed.
    #[inline(always)]
    ///
    /// No identity hash is taken (gc-common w1-f). Both call sites used to mint
    /// one with `VmHeap::next_identity_hash` and pass it here, and neither arm
    /// ever read it: the object arm calls `init_object_header`, which has no hash
    /// parameter, and the array arm builds an `ObjectHeader` that has no hash
    /// field. The hash has lived lazily in the mark word since 2026-08-07. The
    /// mint was not free. On G1 and ZGC `next_hash` is a `fetch_add` on the
    /// heap's one `next_hash_code`, so every interpreter and JIT-helper TLAB
    /// allocation on every thread bounced that cache line for a value nothing
    /// read. Generational amortises it through a thread-local block but still
    /// paid the TLS access.
    pub(in crate::runtime::interpreter::gc_and_alloc) unsafe fn init_header(self, ptr: *mut u8, class_id: ClassId) {
        match self {
            // H1 USED TO mint a non-zero identity hash here so a fresh header
            // was never all-zero, which the stale-pointer detector relied on to
            // avoid mis-flagging legitimate `new Object()` instances.
            //
            // The identity hash left the header on 2026-08-07 and is installed
            // lazily in the mark word, so that discriminator is gone: a live,
            // never-hashed, never-locked bare Object now reads all-zero exactly
            // like reclaimed memory. The detector's predicates were rewritten
            // to test the mark word instead and are documented as WEAKER --
            // see `gc/src/g1.rs`'s zeroed-region closure. Minting eagerly is
            // not an option to get it back: a non-zero mark word loses the
            // thin-lock CAS, so every `synchronized` block would inflate.
            TlabShape::Object {
                num_fields,
                body_size,
                gc_flags,
            } => init_object_header(ptr, class_id, num_fields, body_size, gc_flags),
            TlabShape::Array {
                element_type,
                length_u32,
            } => {
                use cratonvm_gc::heap::{ObjectHeader, ObjectKind};
                let header = ObjectHeader::new(
                    class_id,
                    ObjectKind::Array,
                    element_type,
                    length_u32,
                    length_u32,
                );
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        }
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
/// Pure TLAB bump allocation for already-initialized classes (no refill, no GC point).
#[inline(always)]
pub(crate) fn tlab_bump_object_fast(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    num_fields: usize,
    total_size: usize,
    body_size: u32,
    gc_flags: u8,
) -> Option<ObjectRef> {
    let ptr = thread.tlab.alloc_initialized(total_size, 8, |ptr| {
        init_object_header(ptr, class_id, num_fields, body_size, gc_flags);
        shared.mem.heap.note_tlab_object(ptr, total_size);
    })?;
    note_tlab_allocation(thread, shared, total_size);
    Some(unsafe { ObjectRef::from_raw(ptr) })
}

pub(in crate::runtime::interpreter) fn tlab_alloc_object_inner(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    num_fields: usize,
    body_size: u32,
    gc_flags: u8,
    total_size: usize,
    refill_needs_young_room: bool,
) -> Option<ObjectRef> {
    // The shape is the caller's: `body_size`/`gc_flags` come from ONE
    // `plan_tlab_object_shape_at` (or are read back from the reserved size by
    // `shape_of_reserved`), compact by default since 2026-09-03 -- see
    // `compact_tlab_alloc_enabled` for the FjpProbe miscompile that kept it
    // legacy until then and how it was root-caused. (gc-common w2-d: this
    // comment still said "LEGACY layout, on every backend, deliberately".)
    tlab_alloc_shaped_inner(
        thread,
        shared,
        class_id,
        TlabShape::Object {
            num_fields,
            body_size,
            gc_flags,
        },
        total_size,
        refill_needs_young_room,
    )
}

#[inline(always)]
pub(in crate::runtime::interpreter) fn tlab_alloc_shaped_inner(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    shape: TlabShape,
    total_size: usize,
    refill_needs_young_room: bool,
) -> Option<ObjectRef> {
    use std::sync::atomic::Ordering;

    // Fast path: bump-allocate from the current TLAB without taking
    // any lock. This is the steady-state path for ~99% of allocations
    // once the adaptive sizer has settled.
    if let Some(ptr) = thread.tlab.alloc_initialized(total_size, 8, |ptr| {
        // SAFETY: `alloc_initialized` reserved `total_size` (>= HEADER_SIZE)
        // bytes at `ptr`, 8-byte aligned and privately owned until commit.
        unsafe { shape.init_header(ptr, class_id) };
        // ZGC registers every TLAB object the moment its header is complete
        // (`VmHeap::note_tlab_object`); a no-op on the linear-sweep backends.
        shared.mem.heap.note_tlab_object(ptr, total_size);
    }) {
        note_tlab_allocation(thread, shared, total_size);
        // SAFETY: ptr was produced by TLAB allocation and points at a valid object header within the heap arena.
        return Some(unsafe { ObjectRef::from_raw(ptr) });
    }

    // gen r4w4/alloc4: HotSpot's refill-waste limit (`Tlab::keep_on_miss`,
    // armed only by `CRATONVM_TLAB_SHARE_SIZER`). A miss whose tail is larger
    // than the thread's limit KEEPS the buffer and allocates this one object
    // outside it, instead of burying the tail under a filler the young
    // trigger then charges as occupancy. Only on the JIT's guarded paths
    // (`refill_needs_young_room`): their `None` falls back to a YOUNG-first
    // allocation (`try_alloc_object_full`, `try_alloc_young_probe` +
    // `try_alloc_array`), whereas for the native allocator a `None` from this
    // function means "young cannot supply a TLAB" and switches it to old-gen
    // batch spills (`NativeContextImpl::alloc_object`) — a keep there would
    // tenure young objects.
    //
    // gen r5w5/sizer9: and only while young can serve the object from its bump
    // tail (`young_bump_headroom`, the lock-free published triple). The JIT
    // fallbacks are young-FIRST, not young-only: on a young generation that the
    // kept buffer itself helps fill, they spill the object to the old generation
    // (`try_alloc_object_full` / `try_alloc_array_full`), which is exactly the
    // tenuring the paragraph above rules out. `keep_may_apply` is asked first
    // so the probe is paid only when a keep would otherwise happen, and a
    // refused keep neither raises the limit nor counts. See
    // `docs/feature-designs/generative-gc-architecture.md`.
    //
    // zr z3/ja: never on ZGC. There the `None` does not land in a lock-free
    // young bump tail: every JIT fallback (`try_alloc_object`,
    // `try_alloc_array[_full]`) is the heap's own locked TLAB cell, per object,
    // and ZGC returns retired tails to the free list, so the waste the limit
    // guards is recovered anyway. Only reachable with the share sizer
    // explicitly on (`CRATONVM_TLAB_SHARE_SIZER=1`; OFF by default on ZGC);
    // see `zgc-zr3ja-proposal-tlab-share-sizer-on-zgc-20261012.md`.
    if refill_needs_young_room
        && !shared.mem.heap.is_zgc()
        && thread.tlab.keep_may_apply()
        && shared.mem.heap.young_bump_headroom(total_size)
        // gcj j2/yg2: under the VM's TLAB flags (`CRATONVM_TLAB_HOTSPOT_WASTE_LIMIT`
        // raises by `-XX:TLABWasteIncrement`; off, `keep_on_miss` byte for byte).
        && thread.tlab.keep_on_miss_sized(total_size, &shared.mem.tlab_sizing)
    {
        return None;
    }

    // Crumb-treadmill fix (perf/halfgap-20260717): the JIT allocation path
    // never consulted `needs_gc()` — it only reacted to HARD allocation
    // failure (probe/alloc returning None). With the fragmentation-floor
    // refill serving ever-smaller free-list crumbs, allocation can succeed
    // indefinitely off a degrading free list (and spill to old gen) without
    // any failure ever occurring, so the young collection whose
    // sweep+coalesce would restore full-size TLAB flow never triggers —
    // observed live as ONE young GC in a 23-second BinTreesClassic d=18 run
    // at -Xmx8g. Consult the same live-occupancy trigger the interpreter
    // path uses, once per refill (never per object), rate-limited by
    // allocation progress so a genuinely-full-of-live-data young gen cannot
    // thrash back-to-back collections (the "150 no-progress sweeps" failure
    // mode documented on `needs_gc` itself).
    // Rate limit: needs_gc() is naturally self-limiting (the collection
    // tenures survivors via selective promotion and rebuilds the free list,
    // so `live = used - free_list` collapses immediately after), but keep a
    // slow-path-entries-since-last-fire backstop against a pathological
    // live-set-≥-threshold loop. NOTE: the re-arm metric deliberately is
    // NOT `bytes_allocated_total` — the per-object slow path this guard
    // exists for never bumps that counter, so a bytes-based re-arm freezes
    // in exactly the wedge it guards (measured: 11.5M refill failures, one
    // GC, counter parked).
    //
    // gen-gc-five (2026-09-02): the entry count alone left the trigger DEAD on
    // healthy TLAB flow — see `NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES`. A
    // refill-bytes stamp is OR-ed in so the trigger is consulted every few
    // full-size TLABs; the entry count keeps the crumb wedge covered. Both
    // counters are this VM's (`shared.mem.tlab_wedge`, gc-common w6-c).
    // gcg g4/jn (2026-10-02): the per-thread batches of these counters
    // (gcf f2/o, `CRATONVM_TLAB_REFILL_THREAD_COUNTERS`) were deleted after
    // triage tg1 found no gain; every refill writes them, as before f2/o.
    //
    // gci i2/oo: a pending `jcmd GC.run` is taken here too, on the same
    // JIT-guarded refills that may already collect just below, so a program
    // whose allocating loop is compiled (and never reaches `maybe_gc`) runs it
    // at its next refill. One relaxed load per refill when none is pending.
    if refill_needs_young_room && take_diagnostic_command_gc_request(shared) {
        flush_tlab_allocation_batch(thread, shared);
        thread.tlab.retire();
        run_diagnostic_command_gc(shared, thread);
    }
    if refill_needs_young_room && tlab_gc_trigger_enabled() {
        use std::sync::atomic::Ordering;
        let wedge = &shared.mem.tlab_wedge;
        let entries = wedge
            .slowpath_entries_since_gc
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        let refilled = wedge.refill_bytes_since_rearm();
        let consulted = entries >= NEEDSGC_MIN_ENTRIES_BETWEEN_FIRES
            || refilled >= NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES;
        if consulted && shared.mem.heap.needs_gc_for_jit_allocation() {
            // zr z3/ja: stamp the decision (every collector since gci
            // i48/zl2; `None` under the kill, the old door). A drop leaves this
            // refill to proceed as a declined consult would: a collection has
            // run since the verdict.
            let stamp = zgc_gc_request_stamp(shared);
            wedge.slowpath_entries_since_gc.store(0, Ordering::Relaxed);
            wedge.rearm_refill_bytes();
            flush_tlab_allocation_batch(thread, shared);
            thread.tlab.retire();
            let _ = maybe_gc_forced_collected_stamped(shared, thread, "tlab-alloc-shaped", stamp);
        } else {
            // zr z3/cs (`CRATONVM_ZGC_CONC_EVERY_TRIGGER`): a declined consult
            // may have become a concurrent-cycle request. A compiled
            // allocation loop's only `needs_gc` is this one, so its cycle is
            // opened here or not at all (the request would lapse into a
            // stop-the-world collection). One match and one relaxed load with
            // the switch off; a compile-time `false` off ZGC.
            if consulted && shared.mem.heap.zgc_conc_start_requested() {
                flush_tlab_allocation_batch(thread, shared);
                thread.tlab.retire();
                zgc_concurrent_mark_cycle(shared, thread);
            }
            if refilled >= NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES {
                // Consulted and declined: re-arm the bytes stamp so the next
                // consult is another 4 MiB away rather than on every refill.
                wedge.rearm_refill_bytes();
            }
        }
    }

    // Slow path: the current TLAB is exhausted. Ask the adaptive sizer
    // for the next refill size, request it from the shared arena, and
    // install a fresh TLAB. The sizer looks at the just-retired TLAB's
    // fill-time and alloc-count stats to grow/shrink/keep the request.
    //
    // gen r4w2/alloc2 (2026-09-23): `is_empty()` is ALSO true of every retired
    // buffer (`finish_retire` nulls `start`), so a refill that follows an early
    // retire (park, blocking native, forced GC — and the `tlab-alloc-shaped`
    // trigger branch just above) re-arms at the flat baseline and the ladder
    // only ever sees drains. Its opt-in fix (`CRATONVM_TLAB_SIZE_RETIRED`,
    // `Tlab::refill_request_size`) was removed by gce e2/o, unmeasured and
    // superseded: the share sizer below samples an early retire like a drain
    // (gcg g1/j: and caps the refill after an under-used one at twice its use,
    // without counting it toward the raise -- `Tlab::share_refill_request`).
    //
    // gen r4w4/alloc4: `CRATONVM_TLAB_SHARE_SIZER` (default in
    // `alloc_policy_defaults`) replaces both ladder arms with the HotSpot-shaped
    // sizer — the thread's exponentially averaged bytes per young collection,
    // one fiftieth per refill (`Tlab::share_refill_request`). It needs the
    // collection count to close its windows: one `HeapStats` snapshot per
    // refill, never per object. The ladder arms below stay as they were for
    // the A/B.
    //
    // gen r5w4/defaults8: the switch's default is per backend (an explicit
    // value wins; ON on the Generational heap since gcg g6/jn and on G1 since
    // gci i48/zl3, OFF on ZGC), so it is asked of THIS VM's heap, once per
    // refill. The same answer arms the new buffer's refill-waste limit below
    // (`Tlab::arm_share_sizer`).
    let share_sizer_on = shared.mem.heap.tlab_share_sizer_enabled();
    // gen r5w6/sizer10: never below the object in hand, on EVERY arm (sizer9
    // fixed the share-sizer arm; `gengc-r5w5-sizer9-refill-can-carve-less-than-the-object`
    // is the ladder's half). The ladder answers in `[min_tlab_size(),
    // max_tlab_size()]` = 8 KiB .. 1 MiB and this function serves objects up to
    // `tlab_max_alloc()` (32 KiB): a thread the pressure tracker shrank to
    // 8 KiB that missed on a 20 KiB array carved an 8 KiB buffer the object
    // could not use, installed it, and returned `None` — which the native
    // allocator reads as "young cannot supply a TLAB" and answers with
    // old-generation batch spills (premature tenuring). A request already at
    // least the object is unchanged, so every run whose objects fit the
    // ladder's answer carves byte for byte as before. On the 8-byte grid, like
    // every request. `refill_tlab`'s own clamp to what young has left is the
    // page's other half: gcd d1/d passes this floor to the refill below
    // (`VmHeap::refill_tlab_at_least`).
    let object_floor = total_size.saturating_add(7) & !7;
    // gci i45/sc: this VM's TLAB flags (`TlabSizing::DEFAULT` = the old
    // constants, byte for byte, on every arm below).
    let tlab_sizing = shared.mem.tlab_sizing;
    let requested = if !tlab_sizing.use_tlab() {
        // gci i45/sc: `-XX:-UseTLAB` -- no buffer beyond the object in hand, so
        // every allocation takes this shared (locked) path, as HotSpot's
        // shared-eden allocation does. Never `None` for it: the native
        // allocator reads a `None` as "young cannot supply a TLAB" and would
        // tenure the object.
        object_floor
    } else if share_sizer_on {
        // gen r5w2/alloc6: one epoch per YOUNG pause (HotSpot's sample
        // point); `collection_count` counts a young pause that also
        // collected old gen twice on Generational and halved the sample.
        // gen r5w4/defaults8: re-checked with the tail sink on, see
        // `VmHeap::young_pause_epoch`.
        let epoch = shared.mem.heap.young_pause_epoch();
        // gen r5w5/sizer9: never below the object in hand. The share sizer's
        // desired size falls to `min_tlab_size()` (8 KiB) for a thread that
        // was idle last window, and this function serves objects up to
        // `tlab_max_alloc()` (32 KiB): a smaller carve cannot hold the object,
        // so the allocation below fails on the fresh buffer and this returns
        // `None` — which the native allocator reads as "young cannot supply a
        // TLAB" and answers with old-generation batch spills. On the 8-byte
        // grid, like every request.
        thread
            .tlab
            .share_refill_request_sized(epoch, &tlab_sizing)
            .max(tlab_sizing.min_size())
            .max(object_floor)
    } else {
        // The ladder. First allocation on this thread (no history): the
        // "start big" baseline (a typed `-XX:TLABSize`), so a static-init
        // burst doesn't refill three times before the sizer weighs in;
        // `-XX:-ResizeTLAB`: that size, always. Otherwise the thread-local
        // pressure tracker of the just-retired TLAB, never below the floor
        // (`MIN_TLAB_SIZE`, or a typed `-XX:MinTLABSize`) even if it returns
        // zero (pathological input).
        let first = thread.tlab.is_empty();
        tlab_sizing
            .ladder_refill_request(first, || thread.tlab.next_refill_size())
            .max(object_floor)
    };

    // JIT slow path (`tlab_alloc_object_guarded_refill`): carve a fresh TLAB
    // only when young can supply the chunk WITHOUT a GC — the O(1) bump-tail
    // check, then the amortized-O(1) reclaimed-span probe. Otherwise bail to
    // the caller's non-TLAB fallback (old-gen spill). See the wrapper doc
    // for the failure modes this gate was shaped by.
    // Fragmentation-tolerant free-block probe: any reclaimed span that can
    // hold a minimum-sized TLAB is worth refilling from — `refill_tlab`'s
    // fragmentation fallback serves the largest available block capped at
    // `requested` (see the wedge note there). Probing for the FULL
    // `requested` size wedged this gate shut on a free list made entirely of
    // just-under-`requested` split remnants (the bimodal-bt18 4.5s mode:
    // ~2 GiB of 131056-byte blocks vs a 131072-byte request, every
    // allocation crawling through the per-object slow path while the young
    // collection that would re-coalesce them never triggered).
    // Second-wedge fix (perf/halfgap-20260717): probe at the FRAGMENTATION
    // floor, not `min_tlab_size()`. Steady-state splitting converges on
    // remnants just under whatever floor this gate probes for (observed
    // live: a free list of exactly-4080-byte blocks against the old 8192
    // floor — 10.5M consecutive gate failures, every allocation in the
    // per-object slow path). `refill_tlab`'s fragmentation fallback serves
    // the largest available block at the same floor, so gate and server
    // agree — the [[tlab-remnant-wedge]] "allocator gate and server must
    // agree on satisfiability" rule, applied one level further down.
    let free_block_floor = cratonvm_gc::tlab::frag_tlab_floor().min(requested);
    // gen r4w3/alloc3 (2026-09-23), opt-in `CRATONVM_TLAB_GATE_BUMP_FLOOR`
    // (default OFF: `bump_probe == requested`, the gate byte for byte): probe
    // the bump tail for what `refill_tlab` can actually serve from it — its
    // fragmentation fallback carves a tail as small as the floor — but never
    // less than the object in hand. OFF, a young from-space whose tail is
    // below `requested` and whose free list has no floor-sized block refuses
    // here, and the thread spills to the old gen per object while a usable
    // tail sits unused (wave-1 cross-lane request 2).
    let bump_probe = tlab_gate_bump_probe(
        cratonvm_types::flags().gc.tlab_gate_bump_floor,
        free_block_floor,
        total_size,
        requested,
    );
    // On ZGC the two probes below both answer `!needs_gc()` -- the OCCUPANCY
    // verdict, not whether the arena can carve a chunk -- so once the trigger
    // is armed every compiled refill is refused and each object takes the
    // locked per-object path. (zt t12/rt: zr z5/cr's opt-in capacity gate,
    // which asked the arena instead, was refuted and retired.)
    let occupancy_gate_refuses = refill_needs_young_room
        && !shared.mem.heap.young_bump_headroom(bump_probe)
        && !shared.mem.heap.young_has_free_block(free_block_floor);
    if occupancy_gate_refuses {
        dbg_refill_fail_state(shared, requested);
        // zt t4/ch: on ZGC this refusal IS a due collection (the gate is
        // `!needs_gc()`): count it, and (`CRATONVM_ZGC_REFILL_GATE_COLLECTS`,
        // default ON since zt t12/st, `=0` kills) take that collection now
        // instead of spilling per object until the wedge breaker's 16,384
        // refusals or an allocation stall (see `zgc_refill_gate_collect`).
        note_zgc_gate_refused(shared);
        // Sustained gate failure = the wedge: per-object allocations keep
        // succeeding so nothing else will ever trigger the collection that
        // coalesces the free list. Force one (rate-limited) and re-probe.
        // gci i4/yg: or, under `CRATONVM_GEN_YOUNG_EXHAUSTED_REFILL_COLLECTS`,
        // the young collection an exhausted young earned
        // (`young_exhausted_refill_collect`; `false` with the switch off, so
        // the chain is the historical one).
        if !tlab_gc_trigger_enabled()
            || !(zgc_refill_gate_collect(thread, shared)
                || tlab_refill_wedge_break(thread, shared, "tlab-refill-wedge-gate")
                || young_exhausted_refill_collect(thread, shared))
            || (!shared.mem.heap.young_bump_headroom(bump_probe)
                && !shared.mem.heap.young_has_free_block(free_block_floor))
        {
            return None;
        }
    }
    // NOTE: the wedge-breaker's consecutive-failure counter is reset ONLY on
    // a successful refill below — NOT on a gate pass. The gate can pass on
    // every attempt (a ≥floor block exists, or its cached bound is stale)
    // while `refill_tlab` itself fails every time; resetting here parked the
    // counter at 1 through an 11.5M-failure wedge (measured).

    // Bug-D fix (TLAB tail-filler on refill, 2026-06-12): retire the OUTGOING
    // TLAB *before* replacing it. The fast path above returned `None` because
    // `total_size` did not fit the TLAB's REMAINING tail — not because the TLAB
    // was fully consumed. That leftover tail (up to `total_size - 1` bytes, and
    // for a large object/array refill potentially many KiB) is zeroed arena
    // memory inside the young from-space's live `[base, used)` range. Replacing
    // `thread.tlab` without retiring drops that tail un-tracked: it is neither a
    // walkable object nor a free-list hole. The moving collector never notices
    // (it traces live roots, not a linear walk), but the **non-moving young
    // sweep** that runs while JIT frames are active walks young linearly — it
    // strides into the zeroed tail, decodes it as a run of 40-byte all-zero
    // "objects", and desyncs off the object grid when the tail length is not a
    // multiple of 40, mis-reading a later object's payload as a header. That is
    // the `RemoteCIDRFilter` / `bintrees` "implausible object size" corruption
    // (a subsequent moving GC then SIGSEGVs walking the wrecked heap).
    //
    // `retire()` installs a synthetic `int[]` filler over `[cursor, end)` so the
    // walker strides the tail in O(1); it is a no-op on an empty (first-alloc)
    // TLAB. `requested` (read from the outgoing TLAB's pressure tracker above)
    // is already computed, so retiring here does not disturb the sizer.
    // gci i28/tl: `retire_for_refill` is `retire` with the buried tail
    // counted as refill waste (the `TLAB totals` line's `slow:` figure).
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire_for_refill();

    // gcd d1/d: at least the object in hand, or no carve at all
    // (`gengc-r5w5-sizer9-refill-can-carve-less-than-the-object`, clamp 2).
    // `requested >= object_floor` already, so this only changes the refills
    // young could not have served the object from anyway: they used to carve
    // a smaller buffer, install it, fail the object in it and return `None`;
    // now they return `None` without the carve. Generational and G1 (gch
    // h4/g1: `G1Collector::refill_tlab_at_least` passes over a current Eden
    // region that cannot hold the floor); ZGC forwards to its plain
    // `refill_tlab` (gci i13/oa: this said "Generational only").
    let mut refill = shared
        .mem
        .heap
        .refill_tlab_at_least(requested, object_floor);
    cratonvm_types::gc_entry_census::note_refill(refill.is_some());
    if refill.is_none() {
        dbg_refill_fail(1, requested);
        // Second-wedge fix, stage-1 arm (perf/halfgap-20260717): the gate
        // above can keep PASSING on a stale cached free-block bound while
        // the real free list has degraded to sub-floor dust, so
        // `refill_tlab` itself is where a wedge can spin (observed live:
        // 9.4M consecutive stage-1 failures with ZERO stage-0 gate
        // failures). Count these toward the same breaker; on a sustained
        // run force one coalescing collection and retry the refill once.
        //
        // gci i4/yg: the same retry after the young collection an exhausted
        // young earned (`young_exhausted_refill_collect`, opt-in
        // `CRATONVM_GEN_YOUNG_EXHAUSTED_REFILL_COLLECTS`; `false` with the
        // switch off). i4/fj's O4: on a compiled `byte[4096]` churn 83 % of
        // these refills failed between two refill-time consults and every
        // object went to the old generation.
        if refill_needs_young_room
            && tlab_gc_trigger_enabled()
            && (tlab_refill_wedge_break(thread, shared, "tlab-refill-wedge")
                || young_exhausted_refill_collect(thread, shared))
        {
            refill = shared
                .mem
                .heap
                .refill_tlab_at_least(requested, object_floor);
            cratonvm_types::gc_entry_census::note_refill_retry(refill.is_some());
        }
    } else {
        // gcf f1/o: read before writing. Every successful refill of every
        // thread used to STORE here, so the per-VM line bounced between
        // allocating cores even though the counter is non-zero only inside a
        // wedge; the load keeps the line shared on the healthy path. Same
        // value afterwards either way.
        let fails = &shared.mem.tlab_wedge.gate_consecutive_fails;
        if fails.load(std::sync::atomic::Ordering::Relaxed) != 0 {
            fails.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if let Some((buf, size)) = refill {
        shared.mem.tlab_refill_count.fetch_add(1, Ordering::Relaxed);
        // The healthy-path re-arm metric for the refill-time young trigger
        // above, and (zt t16/pl7) the never-reset total the
        // `sun.gc.tlab.alloc` perf counter differences per collection
        // (HotSpot's `_allocated_size`, grown by every buffer `fill` hands
        // out). zt t31/x10 (`CRATONVM_TLAB_REFILL_ONE_ATOMIC`): one atomic,
        // the metric read as the total less its re-arm mark.
        // Widening: usize -> u64 (value preserved).
        shared.mem.tlab_wedge.note_refill_bytes(size as u64);
        // Read the outgoing TLAB's running per-thread allocation total before
        // the struct is replaced — `Tlab::new` starts a fresh one at zero, and
        // `getThreadAllocatedBytes` must not go backwards at a refill.
        let carried = thread.tlab.thread_allocated_bytes();
        // gen r4w4/alloc4: the share sizer's history rides the same carry.
        let share = thread.tlab.share_sizer();
        // gci i28/tl: and the `TLAB totals` counters.
        let cycle = thread.tlab.cycle_mark();
        // And how much of it this VM's `thread_allocated_total` already holds:
        // all of it after an attached buffer (the retire above published
        // everything), less after a first `Tlab::empty()` that was never
        // attached (e.g. a JIT helper's pre-refill humongous allocation). The
        // attach below credits the difference (gc-common w6-c).
        let vm_published = thread.tlab.vm_thread_published_bytes();
        // SAFETY: buf and size were just returned by the arena allocator and the memory is zeroed.
        thread.tlab = unsafe { cratonvm_gc::Tlab::new(buf, size) };
        // The carve moved the occupancy every backend's trigger reads: the
        // next `maybe_gc` on this thread asks it.
        note_occupancy_may_have_moved(thread);
        thread
            .tlab
            .attach_vm_allocation_counter(&shared.mem.bytes_allocated_total);
        thread.tlab.adopt_allocation_total(carried);
        thread.tlab.adopt_share_sizer(share);
        thread.tlab.adopt_cycle_mark_for_refill(cycle);
        // gen r5w4/defaults8: `Tlab::new` latched the heap-less reading of
        // the share sizer's switch; give the buffer this heap's answer, the
        // one that sized `requested`, so its refill-waste limit agrees.
        thread.tlab.arm_share_sizer(share_sizer_on);
        // Counted only now that the refill was granted (a no-op unless the
        // share sizer sized this request) — see `Tlab::note_share_refill`.
        // gcj j3/yg3: under the VM's flags (`CRATONVM_GEN_TLAB_RAISE_BY_GRANTED_BYTES`,
        // opt-in: the raise counts granted bytes); `note_share_refill` byte for
        // byte without it.
        thread.tlab.note_share_refill_sized(size, &tlab_sizing);
        // After `adopt_allocation_total`, which sets the carry the attach
        // measures against.
        thread
            .tlab
            .attach_vm_thread_allocation_total(&shared.mem.thread_allocated_total, vm_published);
        // gen r4w6/tlab6 (`CRATONVM_GEN_TLAB_TAIL_SINK`, default ON since gen
        // r5w2, latched per heap): the generational heap that carved this chunk takes its
        // unused tail back at the retire — every park, blocking native and
        // contended `monitorenter` included — instead of the retire burying it
        // under a filler. See `GenerationalHeap::return_tlab_tail` and
        // `docs/feature-designs/generative-gc-architecture.md`.
        if shared.mem.heap.is_generational() {
        let h = &shared.mem.heap;
        if h.gen_tlab_tail_sink_enabled() {
                // SAFETY: `h` lives inside `shared.mem`, i.e. inside this VM's
                // `SharedVm`, which outlives every JvmThread and so this TLAB
                // (the argument `attach_vm_allocation_counter` above rests on);
                // the heap is never moved out of it while threads run.
                unsafe { thread.tlab.attach_tail_sink(h) };
            }
        }
        // Start the new refill-window timer so `next_refill_size`
        // measures this TLAB's lifetime from the moment we installed it.
        thread.tlab.begin_refill(size);
        if let Some(ptr) = thread.tlab.alloc_initialized(total_size, 8, |ptr| {
            // SAFETY: same contract as the fast path — a freshly reserved,
            // 8-byte-aligned, privately-owned `total_size` region.
            unsafe { shape.init_header(ptr, class_id) };
            shared.mem.heap.note_tlab_object(ptr, total_size);
        }) {
            note_tlab_allocation(thread, shared, total_size);
            // SAFETY: ptr was produced by TLAB allocation and points at a valid object header within the heap arena.
            return Some(unsafe { ObjectRef::from_raw(ptr) });
        }
    }
    None
}

/// Per-class tally of TLAB allocations that produced a LEGACY header while a
/// matching compact layout was registered.
///
/// The blind spot this closes: `plan_object_alloc`'s `[compact-legacy]` census
/// reports every legacy allocation **that goes through the planner**, and this
/// path does not go through the planner. A class could therefore allocate
/// legacy on the hottest path in the program and be entirely absent from the
/// only report that names legacy allocations — which is exactly what happened
/// to `org/bouncycastle/crypto/digests/SHA256Digest`, 100% of `jit_getfield`'s
/// receivers on Generational and nowhere in the census. See
/// every-jit-getfield-takes-the-helper-FIXED-20260820.md.
///
/// Sixteen slots, linear scan, first-come, and only touched under
/// `CRATONVM_DBG_COMPACT_LEGACY`: the registry lookup it performs is far too
/// expensive for the allocation fast path in a measured configuration.
pub(in crate::runtime::interpreter::gc_and_alloc) static TLAB_LEGACY_CLASSES: [(
    std::sync::atomic::AtomicU32,
    std::sync::atomic::AtomicU64,
    std::sync::OnceLock<String>,
); 16] = [const {
    (
        std::sync::atomic::AtomicU32::new(u32::MAX),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::OnceLock::new(),
    )
}; 16];

/// `(description, class id, count)` for every class this path allocated legacy.
pub fn tlab_legacy_object_classes() -> Vec<(String, u32, u64)> {
    use std::sync::atomic::Ordering;
    TLAB_LEGACY_CLASSES
        .iter()
        .filter_map(|(cid, count, name)| {
            let cid = cid.load(Ordering::Relaxed);
            if cid == u32::MAX {
                return None;
            }
            Some((
                name.get()
                    .cloned()
                    .unwrap_or_else(|| "<unnamed>".to_string()),
                cid,
                count.load(Ordering::Relaxed),
            ))
        })
        .collect()
}

/// Record that this TLAB allocation of `class_id` produced a legacy header.
/// Names the class and whether a compact layout existed for its field count —
/// "a layout was registered and we ignored it" and "no layout exists" are
/// different problems and the census must not conflate them.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_tlab_legacy_object(class_id: ClassId, num_fields: usize) {
    use std::sync::atomic::Ordering;
    let cid = class_id.as_u32();
    for (slot_cid, count, name) in TLAB_LEGACY_CLASSES.iter() {
        let cur = slot_cid.load(Ordering::Relaxed);
        if cur == cid {
            count.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if cur == u32::MAX
            && slot_cid
                .compare_exchange(u32::MAX, cid, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let registered = cratonvm_types::class_layout(cid).map(|l| l.field_count());
            let matches = registered == Some(num_fields);
            let _ = name.set(format!(
                "{} num_fields={num_fields} registered_layout_fields={registered:?} compact_layout_was_available={matches}",
                cratonvm_gc::gc::resolve_class_info(cid)
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| "<unresolved>".to_string()),
            ));
            count.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
}

/// Initialize an object header at the given pointer.
///
/// **This doc claimed an eager identity hash until 2026-09-08 and had been
/// wrong for a month.** `ObjectHeader::new` has had no hash parameter since the
/// 2026-08-06/07 header shrink folded the hash into the mark word and made it
/// lazy, so nothing here has minted one since; `grep next_identity_hash` finds
/// no caller on this path. The text is kept, corrected, because the property it
/// was defending is real and is now defended by something else.
///
/// H1, as it actually stands: a fresh TLAB-allocated `new Object()`
/// (`cid=0`, `fields=0`) must not publish an all-zero first 16 bytes. It would
/// otherwise be indistinguishable from stale, zeroed memory — which cost the
/// stale-pointer detector in `execute_invoke` a 100% false-positive rate on
/// legitimate `Object` keys (CGLIB's HashMap operations), and cost the young
/// non-moving sweep the ability to parse its own arena
/// (`h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908`).
///
/// `ObjectHeader::new` now sets `GC_FLAG_HEADER`, so the property holds for
/// every allocator without anything having to be minted — and, unlike an eager
/// hash, without making every `synchronized` block lose its thin-lock CAS.
#[inline(always)]
pub(in crate::runtime::interpreter) fn init_object_header(
    ptr: *mut u8,
    class_id: ClassId,
    num_fields: usize,
    body_size: u32,
    gc_flags: u8,
) {
    use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind};
    let header = ObjectHeader::new(
        class_id,
        ObjectKind::Object,
        ArrayElementType::Reference,
        // Unused for an Object kind (the array-length slot).
        body_size,
        // A class-file field table is u16-sized, so this is unreachable for a
        // verified Java class. Keep the allocation path panic-free if a corrupt
        // synthetic caller nevertheless violates that invariant.
        u32::try_from(num_fields).unwrap_or(u32::MAX),
    );
    // The flags go on BEFORE the header reaches memory: marking a header
    // compact clears its second word, which on a compact instance is field
    // storage (its header is 8 bytes), so the store below writes zeroes there.
    if gc_flags != 0 {
        header.add_gc_flags(gc_flags);
    }
    // SAFETY: ptr points to freshly allocated, properly aligned memory for the
    // object; `write_to` writes the header word, plus the shape word for a
    // long (legacy) header only.
    unsafe { header.write_to(ptr) };
    // The census of LEGACY headers this path writes (`array_length = 0`, no
    // `GC_FLAG_COMPACT`). Since the compact TLAB shape went default-on
    // (2026-09-03, `compact_tlab_alloc_enabled`) this function writes BOTH
    // shapes, and the census used to count every object it stamped -- compact
    // ones included -- as legacy (gc-common w2-d). Only a legacy stamp is
    // reported now. See `note_tlab_legacy_object`.
    if (gc_flags & cratonvm_types::GC_FLAG_COMPACT) == 0
        && cratonvm_types::flags().gc.dbg_compact_legacy
    {
        note_tlab_legacy_object(class_id, num_fields);
    }
    // A2 breadcrumb (CRATONVM_DBG_A2): the interpreter TLAB fast path bypasses
    // gen_heap, so record the header it writes here, with the footprint of
    // the shape it actually stamped (it used to record the legacy footprint
    // for a compact object too, and the compact `array_length` mirror as 0).
    let compact = (gc_flags & cratonvm_types::GC_FLAG_COMPACT) != 0;
    cratonvm_gc::a2dbg::record(
        ptr as usize,
        class_id.as_u32(),
        ObjectKind::Object as u8,
        ArrayElementType::Reference as u8,
        if compact { body_size } else { 0 },
        u32::try_from(num_fields).unwrap_or(u32::MAX),
        if compact {
            // `body_size` is the compact layout's TOTAL (header included).
            body_size as usize
        } else {
            cratonvm_gc::heap::HEADER_SIZE + num_fields * cratonvm_gc::heap::SLOT_SIZE
        },
    );
}

/// Shared-heap allocation path (with lock). Used for TLAB misses and large objects.
pub(crate) fn alloc_object_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = match shared.mem.heap.try_alloc_object(class_id, num_fields) {
        Some(obj) => obj,
        None => {
            // gcd d2/j: while the futile-young backoff's window is open (the
            // last forced young cycle at an object door left young unable to
            // serve the object), spill first, as the JIT's `new` does -- the
            // object would land in the old generation after a futile cycle
            // anyway. A spill that fails takes the ladder below.
            let spilled = if futile_young_backoff_skips(shared) {
                shared.mem.heap.try_alloc_object_full(class_id, num_fields)
            } else {
                None
            };
            match spilled {
                Some(obj) => obj,
                // SB-LOADER-ZIPCONTENT (2026-08-04): the post-GC retries use the
                // old-gen-spilling `try_alloc_object_full`, for the same reason
                // `gc_alloc_array` gives on the array side — once a non-moving
                // JIT-safe young sweep has fragmented the young free list, a
                // young-only retry reports OOM while the old generation still
                // holds most of the heap. (The first attempt above stays
                // young-only: it is the fast path, and spilling before a GC has
                // even been attempted would promote ordinary short-lived
                // objects straight into old gen.)
                None => {
                    // gcd d2/j: the forced cycle's verdict for the backoff,
                    // judged once, on the heap the first retry sees (the legacy
                    // footprint bounds the compact one from above).
                    let probe_size = cratonvm_gc::heap::HEADER_SIZE
                        .saturating_add(num_fields.saturating_mul(cratonvm_gc::heap::SLOT_SIZE));
                    let mut judged = false;
                    collect_and_retry(shared, thread, "alloc-object-shared", |s| {
                        if !judged {
                            judged = true;
                            note_forced_young_cycle_outcome(s, probe_size);
                        }
                        s.mem.heap.try_alloc_object_full(class_id, num_fields)
                    })
                    .ok_or_else(|| {
                        // T1.7.7 — write an HPROF dump on OOM if `-XX:+HeapDumpOnOutOfMemoryError`.
                        maybe_dump_heap_on_oom(shared, thread);
                        MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::OutOfMemoryError {
                                message: format!(
                                    "Java heap space (alloc_object with {} fields)",
                                    num_fields
                                ),
                            },
                        ))
                    })?
                }
            }
        }
    };
    // Charge what the backend actually reserved. gc-common w2-d
    // (`common-f-shared-path-accounts-legacy-size`): this used to charge the
    // LEGACY footprint (`HEADER_SIZE + num_fields * SLOT_SIZE`) while
    // Generational plans the COMPACT body on this path, so a compact object
    // past a TLAB miss was over-charged up to ~2x, and the same program
    // reported different allocated bytes depending on how many allocations
    // missed. (G1's and, since gc-common w4-c, ZGC's fallible entries plan the
    // compact body too -- `common-w2d-g1-zgc-fallible-object-path-never-plans-compact`
    // -- and reading the header back is right whichever shape was laid down.)
    let footprint = shared_object_footprint(shared, obj, num_fields);
    // T19.3.G1 — slow-path bytes count toward the allocation rate just like
    // TLAB-served bytes, so `--verbose:gc` reflects true throughput even for
    // large objects that skipped the TLAB. Direct, not through the per-thread
    // batch: a thread's batch reaches the VM counter only once its TLAB has
    // been attached at a refill, so a thread that has not refilled yet would
    // silently drop these bytes. This is the slow path; it already took the
    // backend's allocation lock.
    // Widening: usize → u64 is loss-free on all supported targets.
    shared
        .mem
        .bytes_allocated_total
        .fetch_add(footprint as u64, std::sync::atomic::Ordering::Relaxed);
    // Same bytes, per thread — the counter behind
    // `com.sun.management.ThreadMXBean.getThreadAllocatedBytes`. It cannot
    // come from the TLAB cursor here, because this object never touched it.
    note_thread_external_allocation(thread, shared, footprint);
    Ok(obj)
}

/// The footprint of an object [`alloc_object_shared`] just received from the
/// backend, read back from the header it wrote: the compact body when the
/// backend planned one (`GC_FLAG_COMPACT`), else the legacy
/// `HEADER_SIZE + num_slots * SLOT_SIZE` -- `cratonvm_gc::gc::object_total_size`,
/// the sizing rule the collectors themselves walk with.
///
/// Clamped to the legacy size the request implies: a compact body is never
/// larger, and `object_body_size` answers an "impossibly large" sentinel for
/// a compact object whose layout it cannot find, which must not reach a
/// monitoring counter. Saturating, so a corrupt field count cannot panic.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn shared_object_footprint(shared: &SharedVm, obj: ObjectRef, num_fields: usize) -> usize {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
    let legacy = HEADER_SIZE.saturating_add(num_fields.saturating_mul(SLOT_SIZE));
    let real = cratonvm_gc::gc::object_total_size(shared.mem.heap.get_header(obj));
    if (HEADER_SIZE..=legacy).contains(&real) {
        real
    } else {
        legacy
    }
}

/// T1.7.7 — write an HPROF heap dump when allocation fails and the
/// `heap_dump_on_oom` flag is set. Best-effort: any I/O error is logged
/// but does not propagate, because we are *already* about to throw OOM
/// and replacing that with an I/O error would be worse for the user.
///
/// The dump runs at most once per VM lifetime (gated by an atomic
/// flag on the shared VM) so a tight allocation loop doesn't write
/// thousands of dumps.
///
/// `pub(crate)` since gen r4w4/oom (2026-09-24): the JIT allocation helpers'
/// `jit_alloc_oom` and the native funnel's allocation-unwind arm
/// (`safe_native_call_impl`) dump too, as HotSpot's
/// `report_java_out_of_memory` does for every `OutOfMemoryError` it raises.
///
/// Reports "Java heap space"; a site raising a different `OutOfMemoryError`
/// (the VM-limit refusal) calls [`maybe_dump_heap_on_oom_for`].
///
/// gci i10/al: `thread` is the caller's own exclusive handle (see
/// [`maybe_dump_heap_on_oom_for`]).
pub(crate) fn maybe_dump_heap_on_oom(shared: &SharedVm, thread: &mut JvmThread) {
    maybe_dump_heap_on_oom_for(shared, thread, "Java heap space");
}

/// [`maybe_dump_heap_on_oom`] naming the Java-visible `OutOfMemoryError`
/// message the dump is for.
///
/// gen r4w4/oom (2026-09-24): HotSpot's console lines. With
/// `-XX:+HeapDumpOnOutOfMemoryError`, `report_java_out_of_memory` prints
/// `java.lang.OutOfMemoryError: <message>`, then the dumper prints
/// `Dumping heap to <path> ...` and `Heap dump file created [<n> bytes in
/// <t> secs]`, on standard output. CratonVM wrote only a `tracing` line, so a
/// script (or a person) waiting for the dump saw nothing. The failure arm keeps
/// its `tracing` line and prints HotSpot's `Unable to create <path>: <why>`.
/// Also HotSpot's directory rule: a `-XX:HeapDumpPath` naming an existing
/// directory gets `java_pid<pid>.hprof` inside it, and the default is
/// `java_pid<pid>.hprof` in the working directory.
///
/// # gen r4w5/thrash5 (2026-09-24): the whole of `report_java_out_of_memory`
///
/// This is the chokepoint every VM-raised `OutOfMemoryError` passes, so it
/// now does all four things HotSpot's `report_java_out_of_memory` does, in
/// HotSpot's order, for the FIRST such error of the VM only:
///
/// 1. `-XX:+HeapDumpOnOutOfMemoryError`: the dump above;
/// 2. `-XX:OnOutOfMemoryError=<cmd>[;<cmd>...]`: each command, `%p` replaced
///    by the pid, run to completion through the shell
///    ([`run_on_out_of_memory_error_commands`]);
/// 3. `-XX:+CrashOnOutOfMemoryError`: `Aborting due to
///    java.lang.OutOfMemoryError: <message>`, then an abort (the crash
///    handler, where installed, writes the `hs_err_pid<pid>.log`);
/// 4. `-XX:+ExitOnOutOfMemoryError`: `Terminating due to
///    java.lang.OutOfMemoryError: <message>`, then exit status 3.
///
/// One latch covers all four, as HotSpot's `out_of_memory_reported` does:
/// `debug.oom_dump_written`, now the VM's "an OutOfMemoryError was reported"
/// flag (per VM, not a process global). It is taken only when at least one of
/// the four is configured, so with none of them this returns at once, exactly
/// as before (every embedding entry point and `--compatible` default to none).
///
/// # gci i10/al: the caller's `&mut JvmThread`, not a re-derived one
///
/// `thread` is `&mut` (it was `&`) because the dump's GC-safe retry
/// ([`write_heap_dump_on_oom`]) opens a blocked region on it. With a shared
/// view the region had to re-derive a `&mut JvmThread` from
/// `jni::current_jvm_thread_of` while the caller's borrow -- the JIT OOME
/// door's `jit_thread_mut` guard, an interpreter ladder's `&mut` -- was still
/// live: two live exclusive paths to one thread
/// (`gci-i10al-oome-dump-gc-safe-slice-re-derives-mut-jvmthread-FIXED-20261016`).
/// Every caller already holds `&mut JvmThread`, so the call sites are
/// unchanged (implicit reborrow).
pub(crate) fn maybe_dump_heap_on_oom_for(shared: &SharedVm, thread: &mut JvmThread, message: &str) {
    use std::io::Write as _;
    use std::sync::atomic::Ordering;
    // gcd d4/j: every heap-OOME door passes here; owe the native funnel one
    // collection (`native_call_owes_oome_major`). Before the early return:
    // it is not a reporting feature.
    note_heap_oome_raised(shared, thread, message);
    // JIT round 22 wave 12 (lane memory): `CRATONVM_DBG_OOME_LADDER`'s line,
    // for whichever door ran the ladder.
    oome_ladder_report_throw(shared, thread, message);
    let config = &shared.config;
    let on_oom_commands = config
        .on_out_of_memory_error
        .as_deref()
        .filter(|raw| !raw.trim().is_empty());
    // zt t27/lg1 (`CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS`): a
    // `setVMOption("HeapDumpOnOutOfMemoryError", ..)` written since start-up
    // wins over the command line, as HotSpot reads the flag here.
    let heap_dump_on_oom = shared
        .mem
        .managed_flags
        .get_bool("HeapDumpOnOutOfMemoryError")
        .unwrap_or(config.heap_dump_on_oom);
    if !(heap_dump_on_oom
        || on_oom_commands.is_some()
        || config.crash_on_out_of_memory_error
        || config.exit_on_out_of_memory_error)
    {
        return;
    }
    if shared
        .debug
        .oom_dump_written
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return; // already reported: HotSpot reports the first error only
    }
    if heap_dump_on_oom {
        write_heap_dump_on_oom(shared, thread, message);
    }
    if let Some(raw) = on_oom_commands {
        run_on_out_of_memory_error_commands(raw, message, std::process::id());
    }
    if config.crash_on_out_of_memory_error {
        {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "Aborting due to java.lang.OutOfMemoryError: {message}");
            let _ = out.flush();
        }
        let _ = std::io::stderr().flush();
        std::process::abort();
    }
    if config.exit_on_out_of_memory_error {
        {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(
                out,
                "Terminating due to java.lang.OutOfMemoryError: {message}"
            );
            let _ = out.flush();
        }
        let _ = std::io::stderr().flush();
        // HotSpot: `os::_exit(3)`, no shutdown hooks.
        exit_without_cleanup(3);
    }
}

/// HotSpot's `os::_exit`: end the process with `code` at once, running no
/// Java shutdown hooks and no C `atexit` handlers or static destructors.
/// gen r4w6/review6 (2026-09-24).
///
/// `std::process::exit` is `libc::exit` on Unix, which runs the `atexit` list
/// and the loaded native libraries' destructors while the VM's other threads
/// are still running; a destructor that faults there turns the promised
/// status 3 into a signal. The caller flushed stdout (Rust's global handle,
/// so everything buffered in it) before calling. On Windows
/// `std::process::exit` already ends in `ExitProcess`, which skips the CRT's
/// `atexit` list; it is called from a fresh thread through
/// `native-api` `process_exit::exit_process` (gc-common w11-f), because
/// `ExitProcess` on the Java thread can hang (`common-w10v`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn exit_without_cleanup(code: i32) -> ! {
    #[cfg(unix)]
    {
        // SAFETY: `_exit` takes no pointers and never returns; ending the
        // process without any cleanup is the whole intent here.
        unsafe { libc::_exit(code) }
    }
    #[cfg(not(unix))]
    {
        cratonvm_native_api::process_exit::exit_process(code)
    }
}

/// Step 1 of [`maybe_dump_heap_on_oom_for`]: the HPROF dump with HotSpot's
/// console lines. The caller holds the report latch.
///
/// gci i10/al: `thread` is the caller's own `&mut JvmThread`; the GC-safe
/// slice borrows it, no second exclusive path is derived.
pub(in crate::runtime::interpreter::gc_and_alloc) fn write_heap_dump_on_oom(shared: &SharedVm, thread: &mut JvmThread, message: &str) {
    use std::io::Write as _;
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "java.lang.OutOfMemoryError: {message}");
        let _ = out.flush();
    }
    write_heap_dump_now(shared, thread, "OutOfMemoryError", &mut stdout_line);
}

/// The file the next heap dump of this VM writes and whether it is
/// gzip-framed, by `HeapDumper::dump_heap`'s rule: the first dump's path is
/// [`heap_dump_path_for`] over `HeapDumpPath`, every later one that path
/// with `.<n>` appended (zt t31/x10: the sequence the full-collection dumps
/// share, `HeapRealm::heap_dumps`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn next_heap_dump_target(shared: &SharedVm) -> (String, bool) {
    // zt t27/lg1: a `setVMOption("HeapDumpPath", ..)` wins over the command
    // line; an empty path is HotSpot's "not set" (`java_pid<pid>.hprof`).
    let configured = shared
        .mem
        .managed_flags
        .heap_dump_path()
        .or_else(|| shared.config.heap_dump_path.clone())
        .filter(|path| !path.is_empty());
    // zt t28/mf (`CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS`, row `lg1_mf_diag`):
    // HotSpot's `HeapDumper::dump_heap` gzips the dump above
    // `HeapDumpGzipLevel` 0 (a written value first, as above), and its
    // default name is then `java_pid<pid>.hprof.gz`.
    let gzip = crate::config::hotspot_flags::manageable_effect_flags_enabled()
        && shared
            .mem
            .managed_flags
            .number_text("HeapDumpGzipLevel")
            .and_then(|text| text.parse::<u8>().ok())
            .unwrap_or_else(|| crate::config::hotspot_flags::heap_dump_gzip_level_arg(&shared.config))
            > 0;
    let path = shared
        .mem
        .heap_dumps
        .next_path(|| heap_dump_path_for(configured.as_deref(), std::process::id(), gzip));
    (path, gzip)
}

/// One `HeapDumper::dump_heap` of this VM ([`next_heap_dump_target`]) with
/// its two console lines, from a mutator outside every pause: the
/// out-of-memory dump and (zt t31/x10) the full-collection dumps. `why`
/// names the dump in the debug line only; `sink` takes each console line
/// ([`stdout_line`] prints it at once; a full-collection dump whose trace
/// scope the `-Xlog` writer prints later collects them).
pub(in crate::runtime::interpreter::gc_and_alloc) fn write_heap_dump_now(
    shared: &SharedVm,
    thread: &mut JvmThread,
    why: &str,
    sink: &mut dyn FnMut(String),
) {
    let (path, gzip) = next_heap_dump_target(shared);
    let arc = shared.get_arc();
    // The first line before the walk, as HotSpot prints it, and flushed: a
    // dump of a large heap takes seconds, and the first line is how a
    // watcher learns the process has not hung.
    sink(format!("Dumping heap to {path} ..."));
    let started = std::time::Instant::now();
    // obsaudit D11: pass this thread's id so dump_heap can request a real
    // stop-the-world pause for the walk instead of racing live mutators.
    //
    // gci i9/hj (`gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-FIXED-20261017`
    // item 3): a pause refused because another stop-the-world holds the world
    // used to fall back at once to an unpaused walk -- while that other pause,
    // which counts this thread, could be moving objects as soon as it let go.
    // When this OS thread's own `JvmThread` is in reach, each refusal now waits
    // one GC-safe slice ([`oom_dump_gc_safe_slice`]) and asks again, bounded
    // (`hprof::dump_heap_on_oom`); otherwise the single request as before.
    //
    // gci i10/al (`gci-i10al-oome-dump-gc-safe-slice-re-derives-mut-jvmthread-FIXED-20261016`):
    // the slice borrows the caller's own `&mut` (`thread`) instead of a
    // `&mut *` re-derived from `jni::current_jvm_thread_of` while the caller's
    // borrow (the JIT door's `jit_thread_mut` guard) was live. "Own" is now
    // read by address against the registry, without dereferencing any other
    // pointer to this thread (`oom_dump_thread_is_registered`).
    let tid = thread.thread_id;
    let result = if oom_dump_thread_is_registered(shared, thread) {
        let mut slice = || oom_dump_gc_safe_slice(shared, &mut *thread);
        crate::runtime::hprof::dump_heap_on_oom(&arc, &path, tid, gzip, &mut slice)
    } else {
        crate::runtime::hprof::dump_heap_framed(&arc, &path, tid, gzip)
    };
    heap_dump_result_line(result, &path, started, why, sink);
}

/// zt t31/x10: [`write_heap_dump_now`] for a caller that has only its
/// thread's id (the `-Xlog` writer running an owed after-dump,
/// `gc_events::xlog_collection_lines`): the single pause request.
pub(in crate::runtime::interpreter::gc_and_alloc) fn write_heap_dump_by_tid(
    shared: &SharedVm,
    tid: crate::threading::jvm_thread::ThreadId,
    why: &str,
    sink: &mut dyn FnMut(String),
) {
    let (path, gzip) = next_heap_dump_target(shared);
    let arc = shared.get_arc();
    sink(format!("Dumping heap to {path} ..."));
    let started = std::time::Instant::now();
    let result = crate::runtime::hprof::dump_heap_framed(&arc, &path, tid, gzip);
    heap_dump_result_line(result, &path, started, why, sink);
}

/// The second console line of a heap dump (`HeapDumper::dump`'s `Heap dump
/// file created [..]`, or `Unable to create <path>: <why>`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn heap_dump_result_line(
    result: Result<u64, String>,
    path: &str,
    started: std::time::Instant,
    why: &str,
    sink: &mut dyn FnMut(String),
) {
    match result {
        Ok(bytes) => {
            sink(format!(
                "Heap dump file created [{bytes} bytes in {:.3} secs]",
                started.elapsed().as_secs_f64()
            ));
            // zt t27 (row `lg1_mf_oom`): HotSpot prints only the two stdout
            // lines; an `error!` here put an extra line on stderr.
            tracing::debug!("wrote {} byte HPROF heap dump to {} ({})", bytes, path, why)
        }
        Err(e) => {
            sink(format!("Unable to create {path}: {e}"));
            tracing::debug!("failed to write HPROF heap dump ({}): {}", why, e)
        }
    }
}

/// A [`write_heap_dump_now`] sink: the line on standard output at once,
/// flushed (a dump of a large heap takes seconds).
pub(in crate::runtime::interpreter::gc_and_alloc) fn stdout_line(line: String) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// zt t31/x10 (`CRATONVM_GC_FULL_GC_HEAP_DUMPS`, default ON, `=0` kill;
/// page `zgc-zt13x10-review-residuals-*` item 30h): HotSpot 25's
/// `CollectedHeap::full_gc_dump` on G1 and Serial. With
/// `-XX:+HeapDumpBeforeFullGC` (`before`) or `-XX:+HeapDumpAfterFullGC` (a
/// `setVMOption` write first), and while fewer than `FullGCHeapDumpLimit`
/// (`0` = no limit) such dumps were written, one heap dump
/// ([`write_heap_dump_now`]: `Dumping heap to <path> ...`, `Heap dump file
/// created [...]`) inside the `GC(<n>) Heap Dump (before full gc)` trace
/// scope; `gc_id` is the full collection's. HotSpot dumps inside the
/// collection's safepoint; here the collecting thread dumps right before its
/// full collection's pause is requested or right after it returns, in the
/// dump's own pause. ZGC runs no full collection (nor does HotSpot's), so no
/// ZGC door calls this. Two loads when both flags are off.
///
/// The `-Xlog` writer prints a collection's lines after its pause, so where
/// it will print the collection's `gc,start` line (`-Xlog:gc,gc+start`) the
/// before-dump's lines are kept (`gc_events::defer_full_gc_dump_lines`) and
/// printed right after that line, inside the scope as on HotSpot; the dump
/// itself is still written before the pause. The after-dump is then owed to
/// the writer ([`owe_full_gc_heap_dump_after`]), which runs it at HotSpot's
/// point among the full collection's lines; [`settle_full_gc_heap_dump_after`]
/// runs one the writer did not.
pub(in crate::runtime::interpreter) fn full_gc_heap_dump(shared: &SharedVm, thread: &mut JvmThread, before: bool, gc_id: u64) {
    if !full_gc_heap_dump_flag_on(shared, before) || !take_full_gc_dump_slot(shared) {
        return;
    }
    let name = full_gc_heap_dump_flag_name(before);
    let deferred = before && super::gc_events::full_gc_dump_before_deferred();
    let mut lines: Vec<String> = Vec::new();
    if !deferred {
        super::gc_events::full_gc_heap_dump_xlog(gc_id, before, None);
    }
    let started = std::time::Instant::now();
    {
        let mut sink = |line: String| {
            if deferred {
                lines.push(line);
            } else {
                stdout_line(line);
            }
        };
        write_heap_dump_now(shared, thread, name, &mut sink);
    }
    // Truncation: saturates past 584 years.
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    if deferred {
        super::gc_events::defer_full_gc_dump_lines(gc_id, lines, elapsed);
    } else {
        super::gc_events::full_gc_heap_dump_xlog(gc_id, before, Some(elapsed));
    }
}

/// zt t31/x10: `full_gc_dump`'s `FullGCHeapDumpLimit` check and count (a
/// `setVMOption` write first, then the command line; `0` = no limit).
pub(in crate::runtime::interpreter::gc_and_alloc) fn take_full_gc_dump_slot(shared: &SharedVm) -> bool {
    let limit = shared
        .mem
        .managed_flags
        .number_text("FullGCHeapDumpLimit")
        .and_then(|text| text.parse::<u32>().ok())
        .or_else(|| {
            crate::config::hotspot_flags::jvm_arg_value(&shared.config, "FullGCHeapDumpLimit")?
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);
    shared.mem.heap_dumps.take_full_gc_dump(limit)
}

/// zt t31/x10: the after-dump of the full collection about to run, owed to
/// the `-Xlog` writer when it prints that collection's lines
/// (`gc_events::run_owed_full_gc_dump_after`, at HotSpot's point among them);
/// otherwise left to [`settle_full_gc_heap_dump_after`]. No-op with
/// `HeapDumpAfterFullGC` off.
pub(in crate::runtime::interpreter) fn owe_full_gc_heap_dump_after(shared: &SharedVm, thread: &JvmThread) {
    if full_gc_heap_dump_flag_on(shared, false) && super::gc_events::full_gc_dump_after_owed_to_writer() {
        super::gc_events::owe_full_gc_dump_after(thread.thread_id);
    }
}

/// zt t31/x10: after the full collection's door (or pause): the before-dump
/// lines the writer did not print (no collection line came), then the
/// after-dump when `ran_full` and the writer did not run it.
pub(in crate::runtime::interpreter) fn settle_full_gc_heap_dump_after(
    shared: &SharedVm,
    thread: &mut JvmThread,
    ran_full: bool,
    gc_id: u64,
) {
    super::gc_events::flush_deferred_full_gc_dump_lines();
    let _ = super::gc_events::take_owed_full_gc_dump_after();
    let already_ran = super::gc_events::take_full_gc_dump_after_ran();
    if ran_full && !already_ran {
        full_gc_heap_dump(shared, thread, false, gc_id);
    }
}

/// zt t31/x10: the owed after-dump, run by the `-Xlog` writer from the
/// collecting thread (id only) at HotSpot's point among the full
/// collection's lines; its lines print at once, inside the scope.
pub(in crate::runtime::interpreter) fn run_full_gc_heap_dump_after_by_tid(
    shared: &SharedVm,
    tid: crate::threading::jvm_thread::ThreadId,
    gc_id: u64,
) {
    if !full_gc_heap_dump_flag_on(shared, false) || !take_full_gc_dump_slot(shared) {
        return;
    }
    super::gc_events::full_gc_heap_dump_xlog(gc_id, false, None);
    let started = std::time::Instant::now();
    write_heap_dump_by_tid(shared, tid, "HeapDumpAfterFullGC", &mut stdout_line);
    // Truncation: saturates past 584 years.
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    super::gc_events::full_gc_heap_dump_xlog(gc_id, false, Some(elapsed));
}

/// zt t31/x10: the manageable flag [`full_gc_heap_dump`] reads.
pub(in crate::runtime::interpreter::gc_and_alloc) fn full_gc_heap_dump_flag_name(before: bool) -> &'static str {
    if before {
        "HeapDumpBeforeFullGC"
    } else {
        "HeapDumpAfterFullGC"
    }
}

/// zt t31/x10: is [`full_gc_heap_dump_flag_name`] on for this VM (a
/// `setVMOption` write first, then the command line) with the switch on?
pub(in crate::runtime::interpreter::gc_and_alloc) fn full_gc_heap_dump_flag_on(shared: &SharedVm, before: bool) -> bool {
    let name = full_gc_heap_dump_flag_name(before);
    shared
        .mem
        .managed_flags
        .get_bool(name)
        .unwrap_or_else(|| crate::config::hotspot_flags::jvm_arg_bool(&shared.config, name, false))
        && crate::config::hotspot_flags::full_gc_heap_dumps_enabled()
}

/// zt t31/x10: the `GC(<n>)` id [`full_gc_heap_dump`] prints: before a
/// collection, the id its first pause takes (the next one); after it, the
/// id its last pause took.
pub(in crate::runtime::interpreter::gc_and_alloc) fn full_gc_dump_gc_id(shared: &SharedVm, before: bool) -> u64 {
    let next = shared
        .mem
        .gc_cycle_count
        .load(std::sync::atomic::Ordering::Relaxed);
    if before {
        next
    } else {
        next.saturating_sub(1)
    }
}

/// gci i9/hj: one wait of the OOME dump between two requests of its pause,
/// GC-safe, so the pause that refused it -- which may be waiting for this very
/// thread -- counts it and finishes. `thread` is the dump caller's own
/// `JvmThread`, the registered one for its id ([`oom_dump_thread_is_registered`]),
/// reborrowed from the caller's `&mut` (gci i10/al: no `unsafe`, no second
/// exclusive path). A thread already in a blocked region owes no pause anything and
/// sleeps; any other waits inside one, as `vm_exec::wait_out_declined_pause_slice`
/// and `jvmti::native_env::gc_safe` do (RUNNABLE under
/// `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE`, as HotSpot waits for a VM operation).
pub(in crate::runtime::interpreter::gc_and_alloc) fn oom_dump_gc_safe_slice(shared: &SharedVm, thread: &mut JvmThread) {
    use cratonvm_native_api::NativeThreadAccess as _;
    const SLICE: std::time::Duration = std::time::Duration::from_millis(1);
    if thread
        .gc_block_state
        .in_blocked_region
        .load(std::sync::atomic::Ordering::Acquire)
    {
        std::thread::sleep(SLICE);
        return;
    }
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    if shared.threads.thread_registry.vm_internal_waits_runnable() {
        ctx.begin_vm_wait_region();
    } else {
        ctx.begin_blocking_region();
    }
    std::thread::sleep(SLICE);
    ctx.end_blocking_region();
}

/// gci i10/al: may the OOME dump open GC-safe slices on `thread`? Only when it
/// is the `JvmThread` the registry publishes for its own id (a blocked region
/// needs a registered thread; a unit test's free-standing `JvmThread::new` is
/// not one), compared by address as `gen_conc_service::stop_gen_concurrent_service_thread`
/// does. The caller holds `thread` as `&mut`, which only the thread that
/// runs it does, so no other pointer to it is dereferenced here (i9/hj read
/// `jni::current_jvm_thread_of`, whose answer it then borrowed mutably while
/// the caller's borrow was live).
pub(in crate::runtime::interpreter::gc_and_alloc) fn oom_dump_thread_is_registered(shared: &SharedVm, thread: &JvmThread) -> bool {
    let own = thread as *const JvmThread as usize;
    shared.threads.thread_registry.own_jvm_thread_addr(thread.thread_id) == Some(own)
}


/// Where `-XX:+HeapDumpOnOutOfMemoryError` writes, by HotSpot's rule: the
/// configured `-XX:HeapDumpPath` if it names a file, `java_pid<pid>.hprof`
/// inside it if it names an existing directory, and `java_pid<pid>.hprof` in
/// the working directory when unset. Split out so the rule is testable.
/// zt t28/mf: `java_pid<pid>.hprof.gz` when `gzip` (`HeapDumpGzipLevel` above
/// 0), as `HeapDumper::dump_heap` names it.
pub(in crate::runtime::interpreter::gc_and_alloc) fn heap_dump_path_for(configured: Option<&str>, pid: u32, gzip: bool) -> String {
    let file = format!("java_pid{pid}.hprof{}", if gzip { ".gz" } else { "" });
    match configured {
        None => file,
        Some(p) if std::path::Path::new(p).is_dir() => std::path::Path::new(p)
            .join(file)
            .to_string_lossy()
            .into_owned(),
        Some(p) => p.to_string(),
    }
}

/// HotSpot's `Arguments::copy_expand_pid`: `%p` becomes the pid, `%%` a
/// single `%`, and any other `%` is kept as it is (with the character after
/// it). gen r4w5/thrash5 (2026-09-24).
pub(in crate::runtime::interpreter::gc_and_alloc) fn expand_on_error_pid(cmd: &str, pid: u32) -> String {
    let mut out = String::with_capacity(cmd.len() + 8);
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                out.push('%');
                chars.next();
            }
            Some('p') => {
                out.push_str(&pid.to_string());
                chars.next();
            }
            _ => out.push('%'),
        }
    }
    out
}

/// HotSpot's `next_OnError_command` over a whole `-XX:OnOutOfMemoryError`
/// value: commands are separated by `;`, leading blanks and empty commands are
/// skipped, and each command is pid-expanded ([`expand_on_error_pid`]). Several
/// `-XX:OnOutOfMemoryError` options accumulate, joined by a newline (HotSpot's
/// `ccstrlist`); the newline stays inside a command, where the shell reads it
/// as a separator. gen r4w5/thrash5 (2026-09-24).
pub(in crate::runtime::interpreter::gc_and_alloc) fn on_out_of_memory_error_commands(raw: &str, pid: u32) -> Vec<String> {
    raw.split(';')
        .map(|cmd| cmd.trim_start_matches(' '))
        .filter(|cmd| !cmd.is_empty())
        .map(|cmd| expand_on_error_pid(cmd, pid))
        .collect()
}

