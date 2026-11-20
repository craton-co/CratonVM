#[allow(unused_imports)]
use super::*;

/// zr z5/cr: the `[GC] zgc-refusal:` shutdown line (ZGC only). `collections=`
/// refusal collections (positive control: non-zero on any run that met a
/// failed allocation, e.g. every OOME probe); the `window_*` buckets and
/// `window_bytes` say how much the mutators allocated between the previous
/// collection and each refusal (a near-OOM crawl is many small windows with a
/// long `streak_max`); `skipped_futile=` refusals whose forced collection the
/// futile rule saved; zs s6/no: `exhausted_armed` / `exhausted_refused` the
/// opt-in exhausted verdict (`CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`); zs s9/no2:
/// `near_full_soft_clears` the
/// opt-in near-full soft clear (`CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE`),
/// `exhausted_spent` the fills an exhausted verdict ended; zt t4/ch:
/// `gate_refused` the compiled refills the occupancy gate refused (objects
/// spilled per object while a collection was due), `gate_collects` /
/// `gate_collects_dropped` `CRATONVM_ZGC_REFILL_GATE_COLLECTS` (default ON
/// since zt t12/st);
/// zt t6/rv3: `gate_collects_joined` its requests that ran no pause of their
/// own (counted in `gate_collects` before). zt t12/rt: `gate_passed` /
/// `gate_carved`, `floor_fitted` and `stall_soft_clears` left the line with
/// their retired opt-ins (`CRATONVM_ZGC_REFILL_CAPACITY_GATE`,
/// `CRATONVM_ZGC_NEAR_FULL_FLOOR_FITS`, `CRATONVM_ZGC_STALL_CLEARS_SOFT`).
pub fn zgc_refusal_line(shared: &SharedVm) -> Option<String> {
    use std::sync::atomic::Ordering::Relaxed;
    shared.mem.heap.zgc_bytes_allocated_since_cycle()?;
    let c = &shared.mem.tlab_wedge.zgc_refusal;
    // zs s6/no: the opt-in exhausted verdict's engagement.
    let exhausted = shared
        .mem
        .heap
        .zgc_exhausted_verdict_stats()
        .unwrap_or_default();
    Some(format!(
        "[GC] zgc-refusal: collections={} skipped_futile={} owed_paid={} relief_skipped={} \
         soft_skipped_room={} soft_skip_repaid={} soft_skip_unpaid={} \
         native_replays={} native_replay_served={} \
         window_lt4k={} window_lt64k={} window_lt1m={} window_ge1m={} window_bytes={} \
         after_refusal={} streak_max={} exhausted_armed={} exhausted_refused={} \
         near_full_soft_clears={} exhausted_spent={} gate_refused={} gate_collects={} \
         gate_collects_dropped={} gate_collects_joined={} oom_fresh_marks={}",
        c.collections.load(Relaxed),
        c.skipped_futile.load(Relaxed),
        c.owed_paid.load(Relaxed),
        c.relief_skipped.load(Relaxed),
        c.soft_skipped_room.load(Relaxed),
        c.soft_skip_repaid.load(Relaxed),
        c.soft_skip_unpaid.load(Relaxed),
        c.native_replays.load(Relaxed),
        c.native_replay_served.load(Relaxed),
        c.window_lt4k.load(Relaxed),
        c.window_lt64k.load(Relaxed),
        c.window_lt1m.load(Relaxed),
        c.window_ge1m.load(Relaxed),
        c.window_bytes.load(Relaxed),
        c.after_refusal.load(Relaxed),
        c.streak_max.load(Relaxed),
        exhausted.0,
        exhausted.1,
        c.near_full_soft_clears.load(Relaxed),
        shared
            .mem
            .heap
            .zgc_exhausted_spent_count()
            .unwrap_or_default(),
        c.gate_refused.load(Relaxed),
        c.gate_collects.load(Relaxed),
        c.gate_collects_dropped.load(Relaxed),
        c.gate_collects_joined.load(Relaxed),
        c.oom_fresh_marks.load(Relaxed),
    ))
}

/// zt t8/rv4 (`zgc-zs10rv7-wave-s9-review-residuals-CLOSED-20261019.md` item 5): the
/// `[GC] zgc-metaspace-backfill:` shutdown line, ZGC only (`None` on the other
/// collectors). `retry_passes=` counts the drain points that ran the
/// busy-lock back-fill retries (`gc_events::zgc_metaspace_backfill_for_drain`,
/// `BackendGcBeans::note_metaspace_backfill_retry_pass`): with the hold memo
/// (`CRATONVM_ZGC_METASPACE_BACKFILL_HOLD_MEMO`, default ON) a held record
/// pays them once per class-definition epoch or collection, with `=0` at
/// every drain point. The memo's deciding row reads this figure on both arms.
pub fn zgc_metaspace_backfill_line(shared: &SharedVm) -> Option<String> {
    if !shared.mem.heap.is_zgc() {
        return None;
    }
    Some(format!(
        "[GC] zgc-metaspace-backfill: retry_passes={}",
        shared.mem.zgc_gc_beans.metaspace_backfill_retry_passes()
    ))
}

/// zt t20/x16 (D10 of `t16-row-verdicts.md`: "no `[GC] gc_notifications:`
/// line in any census row"): the GC-notification census of a ZGC heap, for
/// the shutdown summary. `VmHeap::print_gc_summary` prints it for
/// Generational and G1 only (ZGC's queue lives in this VM's realm, which the
/// summary cannot reach), so a ZGC run's `notif_dropped` / `notif_deferred`
/// were unreadable. Same rule as the other collectors' line (printed once a
/// bean has had a listener: anything queued or deferred) and the same
/// `GcNotificationCensus::summary_line` text, so its keys stay the
/// `notif_`-prefixed ones the summary uniqueness test already covers. `None`
/// off ZGC or when nothing was queued.
pub fn zgc_gc_notifications_line(shared: &SharedVm) -> Option<String> {
    if !shared.mem.heap.is_zgc() {
        return None;
    }
    let census = shared.mem.zgc_gc_beans.notifications().census();
    (census.queued != 0 || census.deferred != 0).then(|| census.summary_line())
}






/// zt t4/ch: count a compiled refill the occupancy gate refused on ZGC
/// (`gate_refused=` on `[GC] zgc-refusal:`). A no-op on Generational and G1.
/// Reached only on the refusal arm, i.e. per object while a ZGC collection is
/// due and not yet taken -- the window `zgc_refill_gate_collect` closes.
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_zgc_gate_refused(shared: &SharedVm) {
    if shared.mem.heap.is_zgc() {
        shared
            .mem
            .tlab_wedge
            .zgc_refusal
            .gate_refused
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// zt t4/ch: `CRATONVM_ZGC_REFILL_GATE_COLLECTS`, read once per VM
/// (`ZgcRefusalCensus::gate_collects_seed`). zt t12/st: **default ON**, `=0`
/// kills (the t4/ch opt-in arm, where the refused refill spills per object
/// until the consult, the wedge breaker, another thread's native-boundary
/// pressure hook or an allocation FAILURE collects). See
/// [`zgc_refill_gate_collect`], "Why it is default ON".



/// zt t4/ch (`CRATONVM_ZGC_REFILL_GATE_COLLECTS`, default ON since zt t12/st,
/// `=0` kills): on ZGC, a compiled refill the occupancy gate refused takes
/// the due collection NOW.
///
/// # The window it closes
///
/// On ZGC the gate in `tlab_alloc_shaped_inner` is `!needs_gc()`, so it
/// refuses exactly when a collection is due. The collection itself is taken
/// only by the refill trigger's consult, which runs every 4 MiB of SUCCESSFUL
/// refills or every 65,536 slow-path entries -- and a refused gate makes no
/// refill, so the byte clock stops -- or by the wedge breaker after 16,384
/// consecutive refusals. Until then every allocation of every compiled thread
/// takes the per-object locked path (`try_alloc_*_full` -> `alloc_raw`: the
/// arena lock, a registry `fetch_or`, a shared `fetch_add` per object). On a
/// workload where other threads allocate large arrays (which never ask
/// `needs_gc` on the default path: `jit_newarray`'s ZGC headroom arm is the
/// opt-in `CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM`), the large threads keep the
/// heap past the trigger while the small threads crawl per object: the shape
/// of `Zr2A2SmallUnderLarge 4 4 10 1024 prim` (8-10x fewer small allocations
/// than HotSpot locally).
///
/// # Why it is default ON (zt t12/st)
///
/// The same window is where a single compiled allocating thread STALLS.
/// `Zr1TrCauseProbe 200` at `-Xmx48m` (one `main` thread, a compiled
/// `byte[4096]` loop): the threshold (75%, 36 MiB) is crossed between two
/// consults, the gate refuses, and `main` spills per object towards the
/// arena's end. What collects then is either another thread's native call
/// (`vm_exec`'s pressure hook acts on the latch `alloc_raw` raises: 344 of
/// 395 forced collections were `vm-exec-pressure` on a local run) or, when no
/// other thread crosses a native boundary before the arena runs out
/// (~8 MiB of spill on that heap), `main`'s own allocation FAILURE -- an
/// `Allocation Stall` collection, 36-45 per run (148-150 on a loaded box)
/// against HotSpot's 4-5 stall collections. Taking the collection at the
/// refusal leaves none (`stall_gc=0` on every local run, `gate_collects`
/// engaged); HotSpot's remaining stalls wait on a concurrent cycle in
/// flight, which this stop-the-world collector does not have. The rows are
/// in `docs/feature-designs/zgc-architecture.md`.
///
/// The collection is the one the consult would take (same verdict, same site
/// `tlab-alloc-shaped`, so the same door label and census), stamped so a
/// request that raced another thread's collection is dropped, and rate-limited
/// like the newarray arm (`newarray_headroom_rate_skips`: at most one per
/// 1/32 of the heap allocated since the last collection), so a live set held
/// above the trigger cannot storm. `true` = a collection ran or was dropped
/// as stale (the caller re-probes the gate); `false` = not engaged (off, not
/// ZGC, or rate-limited), and the wedge breaker decides as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_refill_gate_collect(thread: &mut JvmThread, shared: &SharedVm) -> bool {
    use std::sync::atomic::Ordering;
    if !shared.mem.heap.is_zgc() {
        return false;
    }
    let Some(window) = shared.mem.heap.zgc_bytes_allocated_since_cycle() else {
        return false;
    };
    if newarray_headroom_rate_skips(window, shared.mem.heap.heap_capacity()) {
        return false;
    }
    let stamp = zgc_gc_request_stamp(shared);
    let wedge = &shared.mem.tlab_wedge;
    // As the consult arm does when it collects: the consult clocks restart.
    wedge.slowpath_entries_since_gc.store(0, Ordering::Relaxed);
    wedge.rearm_refill_bytes();
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    let census = &wedge.zgc_refusal;
    match maybe_gc_forced_collected_stamped(shared, thread, "tlab-alloc-shaped", stamp) {
        Some(true) => census.gate_collects.fetch_add(1, Ordering::Relaxed),
        // zt t6/rv3: no pause of its own (joined another thread's, or the
        // futile rule skipped it); was counted as a collection.
        Some(false) => census.gate_collects_joined.fetch_add(1, Ordering::Relaxed),
        None => census.gate_collects_dropped.fetch_add(1, Ordering::Relaxed),
    };
    true
}

/// zr z3/ja: [`maybe_gc_forced_pub_at`] for a decision site that took a
/// [`zgc_gc_request_stamp`]. Returns `false` when the request was DROPPED
/// because a collection completed after the decision (the caller re-tries its
/// allocation, as HotSpot's allocating thread does after a skipped VM
/// operation, and must not count the drop as a collection of its own), else
/// `true` (this thread collected, or took part in another thread's pause, as
/// the unstamped door does). `stamp: None` is the unstamped door.
pub fn maybe_gc_forced_stamped_pub_at(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    stamp: Option<u64>,
) -> bool {
    maybe_gc_forced_collected_stamped(shared, thread, site, stamp).is_some()
}


/// [`maybe_gc_forced_collected`] with a decision stamp: `None` = dropped as
/// stale (checked at entry and again just before the request), else
/// `Some(collected)`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn maybe_gc_forced_collected_stamped(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    stamp: Option<u64>,
) -> Option<bool> {
    maybe_gc_forced_collected_stamped_on(shared, thread, site, stamp, None)
}

/// The body of the forced door: [`maybe_gc_forced_collected_stamped`] (zr
/// z3/ja's decision stamp) with gci i10/dl's full-collection `requester`
/// ([`maybe_gc_forced_collected_on`]). Merge of origin/dev (2026-10-15).
pub(in crate::runtime::interpreter::gc_and_alloc) fn maybe_gc_forced_collected_stamped_on(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    stamp: Option<u64>,
    requester: Option<GcDoor>,
) -> Option<bool> {
    if zgc_gc_request_is_stale(shared, stamp) {
        return None;
    }
    // zr z5/cr: a ZGC allocation-refusal door. `Some(window)` = this is one
    // (bytes allocated since the previous collection); the futile rule may
    // skip its forced collection (the ladder's clear-soft rung follows).
    let refusal_window = zgc_refusal_window(shared, site, stamp);
    let sealed_at_refusal = shared
        .mem
        .tlab_wedge
        .collections_sealed
        .load(std::sync::atomic::Ordering::Acquire);
    if let Some(window) = refusal_window {
        if zgc_refusal_skip_applies(site, window) {
            let census = &shared.mem.tlab_wedge.zgc_refusal;
            census
                .skipped_futile
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            census
                .soft_rung_owed
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Some(false);
        }
    }
    // zr z11/rv9 (zr4rv4 stale-stamp page, item 4): the forced-entry census
    // is booked AFTER the second stale check below, so a request dropped
    // there is not counted as a forced entry. Diagnostic only.
    // CRIT (TLAB UAF) — retire this thread's TLAB before initiating GC, exactly
    // as `maybe_gc` and `force_gc_from_native` do. This forced path (allocation
    // failure / `create_exception_object`) was the one GC initiator that did NOT
    // retire: its TLAB `[cursor,end)` stays in young-from across the collection,
    // so its unfilled tail is un-walkable to the sweep and, after the young
    // swap+reset, the stale TLAB hands out memory the collector considers free —
    // the same use-after-free / heap-desync class as the parked/blocked-thread
    // TLAB bugs. Surfaced as a SIGSEGV in the moving collector's post-copy
    // `pointer_map` walk under multi-threaded churn (TestFileStoreConcurrency).
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    // GC-overhead accounting: live set BEFORE the collection (post-TLAB-retire),
    // so `note_gc_productivity` can compute how much this forced GC actually
    // freed (`before - after`). See `note_gc_productivity` / `gc_overhead_limit_exceeded`.
    //
    // Use the free-list-aware live estimate, NOT `allocated_bytes`: the default
    // (non-moving) young sweep reclaims dead objects into the from-space free
    // list without retreating the bump cursor, so `allocated_bytes` reads a
    // perfectly-productive sweep as "freed 0" and latches the overhead limit
    // after `GC_OVERHEAD_LIMIT_CYCLES` young fills — a spurious
    // `OutOfMemoryError` on a heap that is almost entirely garbage. Restores
    // part 3 of a9c580aff, which d8092acba ("fix-tests-real-jdk-contracts")
    // reverted in this file while leaving both accessors in place and
    // caller-less; see 24-stringcache-oom-under-load-FIXED.md.
    let before_live = shared.mem.heap.live_bytes_estimate();
    let before_promoted = shared.mem.heap.bytes_promoted_total();
    // Round-5 fix (CRIT — UAF): see comment in `maybe_gc`. The forced
    // path is also an initiator path; drain its per-thread SATB buffer
    // before scanning roots.
    shared.mem.heap.flush_thread_satb();

    // LANE W7-D — did this call actually take a collection? Only a pause that
    // RAN may advance the mark-cycle lifecycle, for the same reason
    // `maybe_gc`'s epilogue sits inside its collection block: the finish half
    // opens a brief STW, and doing that from a thread that just lost the STW
    // race and went to `safepoint_check` would be a second pause bolted onto
    // someone else's.
    //
    // gc-common w2-a (2026-09-23): the pause is the shared door
    // (`run_collection_pause`). What this door used to do differently — open
    // the coverage cycle UNGUARDED before its request (the loser-reset race of
    // `handoff-e-forced-gc-coverage-reset-and-jfr`), drop the collector's
    // dead-finalizer list, collect without the barrier on one thread — is gone
    // with its two private copies of the pause.
    // gcd d9/b census (`[GC] oome_ladder:`). The compiled refill trigger
    // (`tlab_alloc_shaped_inner`, site `tlab-alloc-shaped`) takes this door on
    // an OCCUPANCY verdict (`needs_gc_for_jit_allocation`), not on a failed
    // allocation: it is counted with `maybe_gc`'s occupancy collections when
    // the old generation was wedged at entry (`ladder_threshold_wedged*`),
    // and every other caller as a forced collection (`ladder_forced*`).
    //
    // gci i8/gs (`gci-i7orch-g1-young-sizing-and-door-label-review-findings`
    // item 1): the two G1 JIT array headroom sites are occupancy verdicts
    // too (`needs_gc_for_jit_allocation`), and every occupancy verdict runs
    // its pause on the `AllocationThreshold` door, so `--verbose:gc` prints
    // `door=alloc-threshold` instead of reading as a failed allocation. The
    // door changes the label (and the debug-only mtroots reason) and nothing
    // else: `run_collection_pause` samples the sealed productivity figures on
    // both allocation doors (`door_samples_productivity`), so the overhead
    // verdict below is judged exactly as before, and the forced-cycle mark
    // and the census keep keying off `occupancy_door` as they did.
    // zr z3/ja: the opt-in ZGC `jit_newarray` headroom arm decides on the same
    // occupancy verdict as the refill trigger (`needs_gc()`), so it is not an
    // allocation stall either (`occupancy_door_site` lists it).
    let occupancy_door = occupancy_door_site(site);
    let wedged_at_entry = occupancy_door && old_gen_wedged_now(shared);
    // zr z3/ja: the second stale check, as close to the request as this door
    // gets without entering the barrier (a pause that completes after it is
    // not detected: the request then wins and runs, the status quo). Before
    // the forced-cycle mark below, so a dropped request leaves none behind.
    if zgc_gc_request_is_stale(shared, stamp) {
        return None;
    }
    cratonvm_types::gc_entry_census::note_forced_at(site);
    // zr z3/t2 (host follow-up to rv3 finding 1): ZGC's stall / cause
    // classification is by SITE, exactly: the doors an occupancy verdict
    // initiates (`ZGC_OCCUPANCY_FORCED_SITES`) are neither stalls nor
    // `Allocation Stall`; every other forced door follows a failed allocation
    // and is both, whether or not a trigger rule happens to be pending (the
    // pending-latch reading this replaces excluded nearly every genuine stall
    // on a tight heap). `occupancy_door` above stays the ladder census's own,
    // narrower test.
    let zgc_occupancy = zgc_forced_site_is_occupancy(site);
    // gci i3/yg: mark the cycle this door requests as FORCED for the young
    // pause-goal loop (`CRATONVM_GC_YOUNG_PAUSE_SKIP_FORCED`; ignored with the
    // switch off). Not the compiled refill trigger: its door is an occupancy
    // verdict, i.e. the trigger fired. Cleared after the door so an unconsumed
    // mark cannot reach a later cycle.
    let forced_mark = !occupancy_door;
    if forced_mark {
        shared.mem.heap.gen_mark_next_young_cycle_forced(true);
    }
    let t0 = std::time::Instant::now();
    // zki: a commit the hugetlbfs pool refused is reported before the
    // collection it asks for (explicit large pages only).
    super::gc_events::zgc_report_hugetlb_refusals(shared);
    // zs s8/jf2: the refused request as it stands before this collection
    // (later collections of the ladder would age the record out).
    let stall_page = super::gc_events::zgc_stall_page_at_entry(shared);
    // gci i10/dl: a full-collection requester's door wins (`forced_pause_door`).
    // gci i11/go: G1's recovery compaction is `G1CompactionPause`.
    let door = forced_site_door(site, occupancy_door, requester);
    // gci i20/mr finding 1: ZGC keeps dev's door (`forced_door_on_heap`).
    // (Neither Generational nor G1 is ZGC; no `cfg(feature = "zgc")` needed.)
    let door = forced_door_on_heap(
        !shared.mem.heap.is_generational() && !shared.mem.heap.is_g1(),
        door,
    );
    // zt t11/tg4 (`t11-x8-report.md` T3): the collecting thread carries the
    // site's stall classification into the collector, which leaves the
    // `Timer` clock and the proactive rule's major clock alone for a stall's
    // cycle (zt t20/tg9 retired the opt-in warm-up use of the mark).
    // `false` off ZGC (no reader there).
    let stall_site = shared
        .mem
        .heap
        .zgc_forced_collection_is_stall(zgc_occupancy, site == "tlab-alloc-shaped");
    // zt t31/zc11 (program page 1f residual R7, `CRATONVM_ZGC_MARK_CONTINUE`):
    // an occupancy site's collection may be a Pause Mark End attempt that
    // releases the world, as `maybe_gc`'s occupancy poll's may. A compiled
    // allocation loop closes its cycles here (the refill trigger,
    // `tlab-alloc-shaped`), never through the poll, so MTCHURN made no
    // attempt in 18 closes. A released attempt answers "not collected", as a
    // losing initiator does: the caller's retry allocates (an occupancy
    // verdict, not a failure). A failed allocation and a full-collection
    // requester always collect.
    let mark_continue = ZgcMarkContinueDoor::open(zgc_occupancy && requester.is_none());
    // zt t31/x10 (item 5): and the heap holds the stall until a cycle serves
    // it, so a pause this thread only takes part in is a stall's too.
    if stall_site {
        shared.mem.heap.zgc_note_stall_cycle_pending();
    }
    let pause = super::gc_events::with_zgc_forced_door_occupancy(zgc_occupancy, || {
        cratonvm_gc::gc_quiescence::with_allocation_stall_door(stall_site, || {
            run_collection_pause(shared, thread, door)
        })
    });
    drop(mark_continue);
    if forced_mark {
        shared.mem.heap.gen_mark_next_young_cycle_forced(false);
    }
    let collected = pause.is_some();
    if let Some(window) = refusal_window {
        zgc_refusal_note_collection(shared, window, sealed_at_refusal);
    }
    // zr z2/t2: on ZGC a forced collection that no occupancy verdict asked for
    // is an ALLOCATION STALL -- this mutator could not allocate and waited for
    // (or ran) the collection that serves it, whether it initiated the pause
    // or took part in another thread's. The stall is recorded on the heap's
    // `ZgcMetrics` (no-op off ZGC); see `zgc_allocation_stall_finish` for the
    // opt-in `-Xlog:gc` line. `=0` (`CRATONVM_ZGC_STALL_CAUSE_FROM_TRIGGER`):
    // every door but the refill trigger's counts (the z2 reading).
    if shared
        .mem
        .heap
        .zgc_forced_collection_is_stall(zgc_occupancy, site == "tlab-alloc-shaped")
    {
        super::gc_events::zgc_allocation_stall_finish(shared, thread, t0, stall_page);
    }
    if collected {
        use cratonvm_gc::gen_heap::oome_ladder as l;
        // gci i10/dl: a pause serving a full-collection requester is not an
        // allocation-ladder rung.
        let ladder_rung = requester.is_none();
        if ladder_rung && !occupancy_door {
            ladder_census(shared, l::FORCED, 1);
            ladder_census(shared, l::FORCED_US, elapsed_us(t0));
        } else if ladder_rung && wedged_at_entry {
            ladder_census(shared, l::THRESHOLD_WEDGED, 1);
            ladder_census(shared, l::THRESHOLD_WEDGED_US, elapsed_us(t0));
        }
    }
    // gcf f7/o: the d10/o opt-in that left the compiled refill trigger's
    // occupancy cycles unjudged (`CRATONVM_GC_REFILL_TRIGGER_UNJUDGED`) is
    // deleted -- its positive control never fired in vf1, vf3 or vf5, not even
    // on the purpose-built wedge probe. Every cycle this door runs is judged.
    //
    // gci i10/dl: every ALLOCATION cycle (`door_samples_productivity`); a
    // pause serving a full-collection requester is not judged, as
    // `force_full_collection`'s own pause is not.
    if let Some(pause) = pause.filter(|_| door_samples_productivity(door)) {
        // gcf f1/o: the refill trigger's engagement line (the d9b page's
        // evidence: `streak=raised` is a trigger cycle that grew the streak).
        let streak_before = occupancy_door.then(|| {
            shared
                .mem
                .gc_unproductive_streak
                .load(std::sync::atomic::Ordering::Relaxed)
        });
        note_gc_productivity_for_pause(
            shared,
            before_live,
            before_promoted,
            pause.productivity_after,
        );
        if let Some(streak_before) = streak_before {
            if gc_overhead_dbg_on() {
                let streak_after = shared
                    .mem
                    .gc_unproductive_streak
                    .load(std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "{}",
                    refill_trigger_engagement_line(wedged_at_entry, streak_before, streak_after)
                );
            }
        }
    }
    // LANE W7-D — THE DOOR THAT WAS NOT THERE.
    //
    // `marking_complete` is set in exactly one place (`G1Collector::cleanup`),
    // reached from exactly one VM function (`g1_final_remark_cleanup`), whose
    // callers were `maybe_gc`'s epilogue, the OPT-IN JIT mark driver, and the
    // pre-OOM ladder. This function — the allocation-failure collection that
    // `alloc_object_shared`, `gc_alloc_array`, `create_string_or_oom`, the
    // TLAB refill wedge and all 24 `maybe_gc_forced_pub` call sites funnel
    // into, and which is where G1 takes most of its pauses — ran a full
    // collection and returned without touching the cycle machinery at all.
    //
    // The consequence is not a missed optimisation, it is a latch. A cycle
    // that opens is never closed, so `g1_is_marking_active()` stays true
    // forever; and `g1_should_start_marking()`'s own
    // `&& !g1_is_marking_active()` guard then refuses every later start. One
    // open cycle, `marking_complete == false`, `needs_mixed_gc()` false, and
    // no mixed pause for the life of the process — measured as
    // `remark_pauses=1, cleanup_pauses=0` on a default build in
    // `w6m-a-workload-that-mixes.md` §4, ON `--nojit` AS WELL AS JIT, which is
    // the reading the "compiled loops never reach `maybe_gc`" account could
    // not explain and this one does.
    //
    // Same two calls, same order, same state: the collection above has
    // finished and `complete_gc` has reopened the world, exactly as in
    // `maybe_gc`'s epilogue. `CRATONVM_G1_ALLOC_MARK_DRIVE=0`
    // restores the old behaviour byte for byte and is the bisection lever for
    // any change in mark-cycle frequency dated after 2026-09-21.
    //
    // gci i11/go (`gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (c)):
    // the compaction pause that ends a G1 full collection (`requester` is
    // `Some`) reached this drive too, so the `[GC] g1 mark-doors:` census
    // counted it under `alloc_fail` and, with IHOP still over its threshold
    // right after the full collection, a NEW concurrent cycle could open
    // here. HotSpot starts no marking cycle from a full collection. Under the
    // opt-in `CRATONVM_G1_FULL_COMPACTION_SKIPS_MARK_DRIVE` that pause skips
    // the drive (the full collection's own `g1_force_full_cycle` already
    // closed the cycle it ran); unset, unchanged. Read only on a requester's
    // pause, i.e. once per G1 full collection.
    if collected
        && shared.mem.heap.is_g1()
        && !full_compaction_skips_mark_drive(requester, g1_full_compaction_skips_mark_drive)
    {
        g1_drive_mark_cycle(shared, thread, MarkDoor::AllocFail);
    }
    // gci i27/ob: a G1 young report parked for this thread's pause and not
    // resolved by a drive (the compaction pause that skips it) is `Normal`.
    // No-op off G1 and after a drive (one lock, an empty `Vec`).
    if collected && shared.mem.heap.is_g1() {
        super::gc_events::g1_emit_deferred_young_report(shared, thread.thread_id, false);
    }
    // gen r5w1/refs5 — THE SAME MISSING DOOR, ON GENERATIONAL.
    //
    // The generational concurrent start trigger was asked only by `maybe_gc`'s
    // epilogue (interpreted allocation) and the `System.gc()` door. Compiled
    // code never reaches `maybe_gc`: every young collection a JIT-compiled
    // loop causes comes through HERE, so under the default concurrent-first
    // policy a JIT-heavy program opened no concurrent cycle at all and every
    // old-gen collection was a STW fallback (orchestrator, dev `9e252c8b2`,
    // `GenR4W4SteadyPromotionProbe -Xmx512m`: `concdrv_cycles_started=0`,
    // `minor=134 major=7`). See
    // `docs/feature-designs/generative-gc-architecture.md`.
    //
    // * With the service thread attached (`CRATONVM_GEN_CONC_SERVICE_THREAD`)
    //   the driver's trigger HANDS a due cycle to it and returns at once — a
    //   lock pair, no pause, safe for every caller of this door.
    // * Otherwise under `CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR` — DEFAULT-ON since
    //   gen r5w2/conc6 — which runs a due cycle INLINE as `maybe_gc`'s epilogue
    //   does: the failing allocation waits for the cycle, and the cycle's
    //   pauses and Phase-2/sweep safepoint polls happen inside this door,
    //   where the same thread already takes part in pauses (it initiates one
    //   above, or joins the winner's when it loses the race).
    //
    // Why default-on (gen r5w2/conc6). The concurrent-first old-gen policy is
    // the default, and without this ask it was inert for every program whose
    // hot loop is compiled. Every collection that RUNS now asks the trigger
    // once, whatever door initiated it (`maybe_gc`, `System.gc()`, and this
    // one), which is the "start from the young pause" proposal's property
    // (`gengc-r5w1-refs5-proposal-concurrent-start-from-the-young-pause`)
    // without a latch: a thread that LOSES the pause race never asks, but the
    // winner, on whichever door, does. The inline run is the same function
    // `maybe_gc` has always run for interpreted code, with the same root set
    // (`collect_roots`, which covers this thread's pinned native roots and its
    // compiled frames exactly as the collection above did), and the cycle's
    // pauses are mark-only. The service thread stays the opt-in that moves the
    // cycle off the allocating thread. `=0` restores the old door byte for
    // byte: no trigger ask, no census.
    if collected
        && shared.mem.heap.is_generational()
        && (shared.mem.concurrent_gc_state.service_attached()
            || cratonvm_types::flags().gc.gen_conc_alloc_fail_door)
    {
        // gcg g4/oo (opt-in `CRATONVM_GEN_CONC_OWED_POLL`,
        // `gengc-r5w2-conc6-proposal-the-failing-allocation-should-not-wait-for-the-cycle`):
        // with no service thread, the failing allocation is served first and
        // the cycle is OWED -- latched for the next non-allocating point that
        // may run it (`gen_owed_concurrent_cycle_at_poll`, the compiled poll's
        // slow path; `maybe_gc` for interpreted code).
        if alloc_fail_cycle_deferred(
            shared.mem.concurrent_gc_state.service_attached(),
            gen_conc_owed_poll_enabled(),
        ) {
            if shared.mem.concurrent_gc_state.phase() == cratonvm_gc::ConcurrentGcPhase::Idle {
                let _ = shared.mem.concurrent_gc_state.latch_inline_start_request();
            }
        } else {
            maybe_concurrent_gc_at(shared, thread, MarkDoor::AllocFail);
        }
    }
    if collected {
        let _ = forced_door_hands_off_gc_notifications(shared, thread);
    }
    // zs s6/no: last, so nothing this door allocates after its pause (the
    // notifications above) meets the verdict.
    if refusal_window.is_some() {
        zgc_refusal_judge_exhausted(shared, site, sealed_at_refusal);
    }
    Some(collected)
}

/// zs s9/no2: the default of `CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE=on`:
/// 60 (6 % of the heap free after the last collection). On 25.0.3,
/// `Zs6NoSoftNearFull -Xmx256m` keeps the cache at every run up to 91 % fill
/// and clears it at every run from 94 %; at 92-93 % (~6.5-5.5 % free) it varies
/// run to run (whether a major stalls). 6 % sits inside that band.
pub(crate) const ZGC_NEAR_FULL_SOFT_CLEAR_DEFAULT_PERMILLE: usize = 60;

/// zs s9/no2: parse `CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE`: an off word
/// (`0`, `off`, ...) -> `0` (off); a number -> that many permille of the
/// capacity (clamped to 1000); unset, empty or another on word -> the default
/// 60. zt (2026-10-19): DEFAULT ON -- on the local HotSpot 25.0.3 box
/// `Zs6NoSoftNearFull 95` is SAME with it (3/3, both modes) and DIFF without,
/// 90 / 80 stay SAME, the regression set (SoftRefBeforeOome, GcOverheadProbe,
/// Zs6NoExhaustedTail soft 2 at 256m / 64m, Zr1RfSoftLruProbe,
/// ProxyBoxingOomeProbe) stays SAME, and an interleaved 5-round A/B costs
/// 0.88-1.01x CPU on the unchanged rows. `=0` kills.
pub(in crate::runtime::interpreter::gc_and_alloc) fn near_full_soft_clear_permille_from(value: Option<&str>) -> usize {
    let Some(v) = value.map(str::trim) else {
        return ZGC_NEAR_FULL_SOFT_CLEAR_DEFAULT_PERMILLE;
    };
    if v.is_empty() {
        return ZGC_NEAR_FULL_SOFT_CLEAR_DEFAULT_PERMILLE;
    }
    if cratonvm_types::flags::value_is_off(v) {
        return 0;
    }
    match v.parse::<usize>() {
        Ok(n) => n.min(1000),
        Err(_) => ZGC_NEAR_FULL_SOFT_CLEAR_DEFAULT_PERMILLE,
    }
}

/// zs s9/no2: the pure decision of [`zgc_near_full_soft_clear`]: under
/// `permille` / 1000 of the capacity free after the last collection.
pub(in crate::runtime::interpreter::gc_and_alloc) fn near_full_soft_clear_due(capacity: usize, free_after: usize, permille: usize) -> bool {
    permille != 0 && (free_after as u128) * 1000 < (capacity as u128) * permille as u128
}

/// zs s9/no2 (`zgc-zs6no-soft-references-survive-the-collections-hotspot-stalls-on-CLOSED-20261019.md`,
/// opt-in `CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE`): HotSpot's ZGC clears
/// every soft reference in a major collection caused by an allocation stall
/// (`should_clear_all_soft_references`: the `_z_allocation_stall` cause, or
/// a mutator stalled waiting for an old collection). On a nearly full heap its
/// concurrent major cannot keep up with the mutator, so its majors there ARE
/// stalls: on 25.0.3, `Zs6NoSoftNearFull 94|95` clears the whole cache on
/// every run and `90|91` keeps it (no stall; 92-93 vary); the LRU rule (1000 ms per free MiB at the last
/// collection, ~12 s at 95 %) would have kept it after the probe's 3 s idle.
/// This VM collects stop-the-world and paces near full by occupancy, so its
/// near-full collections are not refusals and a stall rule (zs s6/no's
/// `CRATONVM_ZGC_STALL_CLEARS_SOFT`, retired in zt t12/rt) never fired there
/// (s6a host: `all` on both arms). The stand-in: an allocation-driven ZGC collection (not
/// `System.gc()`) whose previous cycle left under `permille` of the heap free
/// asks the per-VM clear-all-soft request the last-ditch rung uses. Off
/// (`0`): one flag read per collection. `near_full_soft_clears=` on
/// `[GC] zgc-refusal:`. Read per collection (seeded on first use would need a
/// per-VM slot; a collection is rare next to the read).
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_near_full_soft_clear(shared: &SharedVm) {
    let Some((capacity, free_after)) = shared.mem.heap.zgc_free_after_last_cycle() else {
        return;
    };
    let v = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok();
    let permille = near_full_soft_clear_permille_from(v.as_deref());
    if !near_full_soft_clear_due(capacity, free_after, permille) || !weakref_clear_enabled() {
        return;
    }
    let mut refs = shared.mem.ref_processor.lock();
    if !refs.has_active_soft_refs() {
        return;
    }
    refs.request_clear_all_soft_refs();
    drop(refs);
    shared
        .mem
        .tlab_wedge
        .zgc_refusal
        .near_full_soft_clears
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// zs s6/no: the exhausted verdict (opt-in `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`;
/// `director::exhausted_verdict` states the rule and its price). At the end of
/// a ZGC refusal door's collection, on the ladder doors
/// ([`ZGC_REFUSAL_SKIP_SITES`], whose ladder continues with an attempt, the
/// clear-soft rung and an attempt) and the clear-soft rung itself: when a
/// collection ended since the refusal and left under the floor free, this
/// thread's next fallible attempts are refused until the next collection, so
/// the ladder runs its clear-soft rung and throws instead of crawling one
/// whole-heap collection per sliver of free space. Not on the other refusal
/// sites (stress hooks, `ldc` and exception doors): their callers have no
/// rung after the attempt. No-op off ZGC and with the switch off.
///
/// zs s8/rv5: one skip site has no clear-soft rung either:
/// [`ZGC_REFUSAL_RUNGLESS_SKIP_SITES`] (`jit-helpers`, the compiled `new` /
/// `newarray` / `anewarray` / box doors, which throw through
/// `jit_alloc_oom` right after their one retry). Armed there, the verdict
/// refused that retry and the compiled allocation threw `OutOfMemoryError`
/// with soft references still live, which the `SoftReference` contract (and
/// HotSpot) never does. There it is armed only when no soft reference is
/// active, i.e. when the rung those doors lack would have nothing to clear.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_refusal_judge_exhausted(shared: &SharedVm, site: &str, sealed_at_refusal: u64) {
    if !(ZGC_REFUSAL_SKIP_SITES.contains(&site) || site == ZGC_REFUSAL_SOFT_RUNG_SITE) {
        return;
    }
    let sealed_now = shared
        .mem
        .tlab_wedge
        .collections_sealed
        .load(std::sync::atomic::Ordering::Acquire);
    if sealed_now == sealed_at_refusal {
        return;
    }
    if ZGC_REFUSAL_RUNGLESS_SKIP_SITES.contains(&site)
        && shared.mem.heap.zgc_exhausted_verdict_enabled()
        && shared.mem.ref_processor.lock().has_active_soft_refs()
    {
        shared.mem.heap.zgc_lift_exhausted_verdict();
        return;
    }
    let _ = shared.mem.heap.zgc_arm_exhausted_verdict();
}

/// zs s8/rv5: the [`ZGC_REFUSAL_SKIP_SITES`] whose callers run no clear-soft
/// rung between their retry and their `OutOfMemoryError` (see
/// [`zgc_refusal_judge_exhausted`]).
pub(in crate::runtime::interpreter::gc_and_alloc) const ZGC_REFUSAL_RUNGLESS_SKIP_SITES: &[&str] = &["jit-helpers"];

/// gcg g4/oo: `CRATONVM_GEN_CONC_OWED_POLL` (default on; `runtime_flag_default_on`).
/// Read once per allocation-failure collection on Generational, and by the
/// poll consumer only when a request is latched. `pub(crate)` for the JIT's
/// helper-entry pickup (`jit_take_gen_inline_concurrent_start`), which leaves
/// the latch to the poll under the switch (cross-lane hunk, `g4-oo-report.md`).
pub(crate) fn gen_conc_owed_poll_enabled() -> bool {
    true
}

/// The pure gate of the owed-cycle deferral: only without a service thread
/// (which already takes the cycle off the allocating thread) and only under
/// the switch.
pub(in crate::runtime::interpreter::gc_and_alloc) fn alloc_fail_cycle_deferred(service_attached: bool, owed_poll_on: bool) -> bool {
    !service_attached && owed_poll_on
}

/// gcg g4/oo, `gengc-r5w2-conc6-proposal-the-failing-allocation-should-not-wait-for-the-cycle`
/// step 1: run an owed Generational concurrent cycle at a point that is NOT
/// inside an allocation. For the compiled safepoint poll's slow path
/// (`jit_safepoint_slow_path_body`, after its `safepoint_check`; cross-lane
/// hunk in `g4-oo-report.md`), where the poll site has spilled every live
/// reference of the frame, so the cycle's pauses scan it as they scan a
/// parked thread. Never from `safepoint_check` itself: the driver polls it
/// between slices.
///
/// Takes the latched request ([`gen_inline_concurrent_start_pub`]'s latch,
/// which the allocation-failure door sets under `CRATONVM_GEN_CONC_OWED_POLL`)
/// and runs the driver, which re-asks the trigger, so a stale request opens
/// nothing. Off (the switch), and with nothing latched, one relaxed load.
pub fn gen_owed_concurrent_cycle_at_poll(shared: &SharedVm, thread: &mut JvmThread) {
    if !shared.mem.concurrent_gc_state.inline_start_requested() || !gen_conc_owed_poll_enabled() {
        return;
    }
    if shared.mem.concurrent_gc_state.take_inline_start_request() {
        flush_tlab_allocation_batch(thread, shared);
        thread.tlab.retire();
        maybe_concurrent_gc_at(shared, thread, MarkDoor::JitDriver);
    }
}


/// gcf f1/o: the `[GC_OVERHEAD] refill-trigger:` line `maybe_gc_forced_collected`
/// prints (under `CRATONVM_DBG_GC_OVERHEAD=1`) for EVERY collection the compiled
/// refill trigger wins.
///
/// `raised` (`streak_after > streak_before`) is the d9b page's defect in one
/// word: a trigger cycle that grew the overhead streak. The `arm=judged` word
/// stays for the probe rows that grep it; since gcf f7/o deleted the unjudged
/// opt-in (`CRATONVM_GC_REFILL_TRIGGER_UNJUDGED`) every trigger cycle is
/// judged. Pure, for the unit test.
pub(in crate::runtime::interpreter::gc_and_alloc) fn refill_trigger_engagement_line(
    wedged_at_entry: bool,
    streak_before: u32,
    streak_after: u32,
) -> String {
    let effect = if streak_after > streak_before {
        "raised"
    } else if streak_after < streak_before {
        "reset"
    } else {
        "kept"
    };
    format!(
        "[GC_OVERHEAD] refill-trigger: arm=judged wedged_at_entry={wedged_at_entry} \
         streak_before={streak_before} streak_after={streak_after} streak={effect}"
    )
}

/// The allocation-failure door's GC-notification hand-off (gc-common w36-e,
/// `common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications`).
///
/// This door records the collection's notifications but can never deliver
/// them: its callers (the JIT allocation helpers, `alloc_object_shared`,
/// `gc_alloc_array`, the TLAB refill wedge) hold unrooted references, so no
/// Java may run here. Delivery was left to the next door that may run Java
/// (`maybe_gc`, `drain_ref_work_queued_elsewhere`), and a loop running in
/// compiled code reaches none: since JIT round 11 compiles the OSR body of a
/// method with an exception table or a `synchronized` block, the allocating
/// loop of `GcNotificationThreadProbe` (a `synchronized` block) delivered
/// NOTHING under `--compatible` on any backend (`notifications=0`), where
/// HotSpot delivers every one on its `Notification Thread`.
///
/// The rule is the VM's own ([`gc_notifications_go_to_delivery_thread`]),
/// applied at one more door, and every step of it is heap-free (a registry
/// read, a counter bump and a condvar notify, or a first-use spawn that only
/// registers a STARTING thread): a `--real-jdk` VM hands off as at every door;
/// a `--compatible` one only when this thread holds a user-visible lock (its
/// lock-free collections keep the inline delivery at the next drain point, as
/// before); the `CRATONVM_FINALIZER_THREAD=0` arm does nothing. `true` when the
/// delivery thread took them.
pub(in crate::runtime::interpreter::gc_and_alloc) fn forced_door_hands_off_gc_notifications(shared: &SharedVm, thread: &JvmThread) -> bool {
    gc_notifications_go_to_delivery_thread(shared, thread)
        || zgc_undeliverable_door_hands_off_gc_notifications(shared, thread)
}

/// `CRATONVM_ZGC_UNDELIVERABLE_NOTIFICATIONS_OFF_THREAD` (zt t12/gn4,
/// `zgc-zt11gn3-*` defect 1 of `t11-row-verdicts.md`; default ON, `=0`
/// kill). Under the `--compatible` rule ([`GcNotificationDelivery::WhenLockHeld`])
/// a lock-free door keeps its notifications for an inline delivery "at the
/// next drain point", and a door that cannot deliver now -- the
/// allocation-failure door (never), an inline door inside a JIT borrow (the
/// non-forced drain defers, `run_gc_notifications`), and the ref-work drain
/// under a borrow (returns before the drain) -- may never reach one: a
/// compiled allocation loop followed by a `Thread.sleep` wait is the whole
/// program. `Zt11Gn3GenParity` under `--compatible` with
/// `CRATONVM_ZGC_GENERATIONAL=1`: the minor ran (`-Xlog:gc` `GC(2) Minor
/// Collection (Allocation Rate)`), and its notification never reached the
/// `ZGC Minor Cycles` listener in the probe's 5 s wait (`causes=[]`), where
/// `--real-jdk` (`OffMutator`) and HotSpot 25 (its `Notification Thread`)
/// deliver `[Allocation Rate]`. With the switch such a door hands them to the
/// delivery thread as the metadata door does
/// ([`hand_gc_notifications_to_delivery_thread`], lock or no lock). ZGC only:
/// G1 and Generational keep the w7-d rule. A lock-free door that CAN deliver
/// (outside a JIT borrow) still delivers inline, and `System.gc()`'s forced
/// drain is untouched. `=0`: the next drain point, as before.
///
/// t1 wave 2 lane Z (2026-10-26): UNREACHABLE under every shipped policy.
/// Since 2026-10-19 (`8b54ad9864`) [`gc_notification_delivery_for`] maps both
/// the default and the full finalizer policy to
/// [`GcNotificationDelivery::OffMutator`] in BOTH modes, so
/// [`gc_notifications_go_to_delivery_thread`] -- which every door asks first
/// -- hands queued notifications off on every backend (G1 and Generational
/// included), and the `WhenLockHeld` rule this gate tests is never produced.
/// That is why `T1ZgcNotifyLockFree` delivered on G1 and Serial: the
/// "ZGC-only" shape is not a G1/Gen defect. Kept (switches are never deleted)
/// for the day a lock-held notification rule returns; widen `zgc` here then.
/// Page: `docs/feature-designs/zgc-architecture.md`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_undeliverable_notifications_off_thread_enabled() -> bool {
    true
}

/// zt t12/gn4: the rule of [`zgc_undeliverable_door_hands_off_gc_notifications`],
/// pure: only a ZGC heap, with the switch on, under the `--compatible`
/// rule. `OffMutator` already hands off at every door and `Inline` (the
/// `CRATONVM_FINALIZER_THREAD=0` arm) never does.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_undeliverable_door_hands_off_for(
    zgc: bool,
    enabled: bool,
    rule: GcNotificationDelivery,
) -> bool {
    zgc && enabled && rule == GcNotificationDelivery::WhenLockHeld
}

/// zt t12/gn4: a door that cannot deliver its queued GC notifications now
/// hands them to the delivery thread (see
/// [`zgc_undeliverable_notifications_off_thread_enabled`]). `true` when the
/// delivery thread took them. Heap-free and wait-free, as
/// [`metadata_door_hands_off_gc_notifications`]; one load when nothing is
/// queued, which is checked first.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_undeliverable_door_hands_off_gc_notifications(
    shared: &SharedVm,
    thread: &JvmThread,
) -> bool {
    if !super::gc_events::gc_notifications_pending(shared) {
        return false;
    }
    let zgc = shared.mem.heap.is_zgc();
    if !zgc_undeliverable_door_hands_off_for(
        zgc,
        zgc && zgc_undeliverable_notifications_off_thread_enabled(),
        gc_notification_delivery(shared),
    ) {
        return false;
    }
    hand_gc_notifications_to_delivery_thread(shared, thread)
}

/// zt t15/x12 (`CRATONVM_ZGC_FORCED_NOTIFICATION_DRAIN_BOUNDED`,
/// `gc_events::zgc_forced_notification_drain_bounded`): hand the records a
/// bounded forced drain left to the delivery thread, marked for its
/// `--compatible` batch as a lock-holding door's are. `false` -- nothing
/// handed -- under the inline policy (`CRATONVM_FINALIZER_THREAD=0`, no
/// delivery thread by design) or when no thread could be started; the drain
/// point then keeps its re-drain.
#[cfg(feature = "management")]
pub(in crate::runtime::interpreter) fn forced_drain_hands_rest_to_delivery_thread(
    shared: &SharedVm,
    thread: &JvmThread,
) -> bool {
    if gc_notification_delivery(shared) == GcNotificationDelivery::Inline {
        return false;
    }
    hand_gc_notifications_to_delivery_thread(shared, thread)
}

/// zt t12/gn4: [`zgc_undeliverable_door_hands_off_gc_notifications`] for an
/// inline door (`maybe_gc`) that the lock rule left to deliver itself: only
/// inside a JIT borrow, where its non-forced drain would defer.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_jit_borrowed_door_hands_off_gc_notifications(
    shared: &SharedVm,
    thread: &JvmThread,
) -> bool {
    crate::jit::helpers::is_jit_thread_set()
        && zgc_undeliverable_door_hands_off_gc_notifications(shared, thread)
}

// ---------------------------------------------------------------------------
// gcd d2/j (2026-09-27): the futile-young backoff of the object doors
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`,
// the allocation half. The object doors -- `jit_new_object`'s and
// `jit_anewarray_object`'s young probe, the interpreter's
// `alloc_object_shared` -- COLLECT FIRST when young cannot serve the object,
// then spill it to the old generation if young still cannot. With young full
// of LIVE objects and every young cycle non-moving (a peer's compiled-helper
// window keeps the cycle from copying, so nothing is promoted), that is one
// futile young cycle per object: the cycle frees nothing, the object spills,
// the next object misses again. The GC-overhead limit cannot end it -- a cycle
// counts only while the old generation is wedged, and it is far from full --
// so the program neither progresses at speed nor throws (`oome-thread-exit`
// of `GenR4W6JitOomRootProbe` ran into the 300 s timeout). HotSpot's young
// collection promotes instead; the array doors here already spill first
// (`jit_newarray`, `gc_alloc_array`: SB-LOADER-ZIPCONTENT).
//
// The escalation: a forced young cycle after which young STILL cannot serve
// the object that missed is recorded as futile, and while the verdict stands
// the object doors spill first and force the next young cycle only after a
// quantum of allocation (256 KiB doubling per consecutive futile verdict, at
// most 2 % of the heap -- the overhead limit's yardstick), or a matching
// number of skipped entries. The object lands where it would have landed
// after the futile cycle anyway (the old generation), so only the number of
// futile collections changes; a spill that fails (the old generation full
// too) still collects at once, so the overhead streak and the
// `OutOfMemoryError` are reached exactly as before. A cycle after which young
// can serve the object clears the verdict. Generational only; G1 and ZGC are
// untouched. `CRATONVM_GC_FUTILE_YOUNG_BACKOFF=0` restores one collection per
// door entry.

/// The first re-arm quantum of the futile-young backoff, in bytes.
pub(in crate::runtime::interpreter::gc_and_alloc) const FUTILE_YOUNG_QUANTUM_BASE: u64 = 256 * 1024;

/// The smallest object a door allocates: the entry-counted re-arm is the byte
/// quantum over this, so it never closes the window before the byte count
/// would for a real object, and still closes it for a door whose allocation
/// never reaches `bytes_allocated_total`.
pub(in crate::runtime::interpreter::gc_and_alloc) const FUTILE_YOUNG_MIN_OBJECT_BYTES: u64 = 16;

/// Is the futile-young backoff armed for this VM's heap at all?
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn futile_young_backoff_applies(shared: &SharedVm) -> bool {
    shared.mem.heap.is_generational() && cratonvm_types::flags().gc.futile_young_backoff
}

/// Can young serve a `size`-byte object right now? The object doors' own
/// entry probe (the lock-free bump tail, then the free list).
pub fn young_can_serve(shared: &SharedVm, size: usize) -> bool {
    let heap = &shared.mem.heap;
    heap.young_bump_headroom(size) || heap.try_alloc_young_probe(size).is_some()
}

/// Asked by an object door that found young unable to serve its object,
/// BEFORE it forces a young cycle: `true` = skip the cycle and spill to the
/// old generation first (the backoff's window is open), `false` = collect as
/// before. Idle (always `false`) until a forced cycle has been judged futile
/// by [`note_forced_young_cycle_outcome`]. See the section comment above.
pub fn futile_young_backoff_skips(shared: &SharedVm) -> bool {
    use std::sync::atomic::Ordering;
    let st = &shared.mem.alloc_ladder;
    let streak = st.futile_young_streak.load(Ordering::Relaxed);
    if streak == 0 || !futile_young_backoff_applies(shared) {
        return false;
    }
    let quantum = futile_young_quantum(streak, shared.mem.heap.heap_capacity());
    let allocated = shared
        .mem
        .bytes_allocated_total
        .load(Ordering::Relaxed)
        .saturating_sub(st.futile_young_alloc_mark.load(Ordering::Relaxed));
    let skipped = st
        .futile_young_skipped_since
        .fetch_add(1, Ordering::Relaxed)
        + 1;
    if !futile_young_window_open(quantum, allocated, skipped) {
        return false;
    }
    st.futile_young_skips.fetch_add(1, Ordering::Relaxed);
    // gcd d9/b: the per-VM census field above is printed nowhere; the
    // shutdown summary reads this one (`ladder_futile_young_skips`).
    ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::FUTILE_SKIPS, 1);
    true
}

/// Recorded by an object door right AFTER its forced young cycle (whether this
/// thread ran it or joined a peer's): can young now serve the `size`-byte
/// object that missed? Yes clears the backoff's verdict; no (a futile cycle)
/// grows the streak and opens a fresh window from now. A no-op off
/// Generational and under `CRATONVM_GC_FUTILE_YOUNG_BACKOFF=0`.
pub fn note_forced_young_cycle_outcome(shared: &SharedVm, size: usize) {
    use std::sync::atomic::Ordering;
    if !futile_young_backoff_applies(shared) {
        return;
    }
    let st = &shared.mem.alloc_ladder;
    // gcd d9/c (cross-lane request 2b, applied by d9/b): a cycle that could
    // not promote and left young under one quantum of room is futile even
    // though this object fits: the next sliver ends in the same cycle
    // (`gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles`).
    // `CRATONVM_GC_YOUNG_DRAIN_BLOCKED=0` takes the gated walk out of the
    // term. gcj j1yg-thrash: a wedged old generation counts as no drain too
    // (young full of arrays old gen cannot take, each cycle promoting the
    // last iteration's sliver into an old hole: one cycle per object).
    // Narrowing: the 256 KiB quantum fits every supported `usize`.
    let starved = shared
        .mem
        .heap
        .young_starved_behind_blocked_drain(FUTILE_YOUNG_QUANTUM_BASE as usize);
    if young_can_serve(shared, size) && !starved {
        if st.futile_young_streak.load(Ordering::Relaxed) != 0 {
            st.futile_young_streak.store(0, Ordering::Relaxed);
        }
        return;
    }
    st.futile_young_alloc_mark.store(
        shared.mem.bytes_allocated_total.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    st.futile_young_skipped_since.store(0, Ordering::Relaxed);
    let streak = st
        .futile_young_streak
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    let verdicts = st.futile_young_verdicts.fetch_add(1, Ordering::Relaxed) + 1;
    // gcd d9/b: `ladder_futile_young_verdicts` (see `futile_young_backoff_skips`).
    ladder_census(
        shared,
        cratonvm_gc::gen_heap::oome_ladder::FUTILE_VERDICTS,
        1,
    );
    if gc_overhead_dbg_on() {
        eprintln!(
            "[GC_OVERHEAD] futile-young verdict: size={size} streak={streak} \
             quantum={} verdicts={verdicts} skips={}",
            futile_young_quantum(streak, shared.mem.heap.heap_capacity()),
            st.futile_young_skips.load(Ordering::Relaxed),
        );
    }
}

/// The re-arm quantum after `streak` consecutive futile verdicts (`streak >=
/// 1`): [`FUTILE_YOUNG_QUANTUM_BASE`] doubling per verdict, capped at 2 % of
/// `cap` (never below the base).
pub(in crate::runtime::interpreter::gc_and_alloc) fn futile_young_quantum(streak: u32, cap: usize) -> u64 {
    // Widening: usize -> u64 is loss-free on every supported target.
    let cap_bound = (cap as u64 / 50).max(FUTILE_YOUNG_QUANTUM_BASE);
    let shift = streak.saturating_sub(1).min(16);
    (FUTILE_YOUNG_QUANTUM_BASE << shift).min(cap_bound)
}

/// The backoff's window: open (skip the cycle) while less than `quantum`
/// bytes were allocated and fewer than `quantum / 16` door entries skipped
/// since the verdict.
pub(in crate::runtime::interpreter::gc_and_alloc) fn futile_young_window_open(quantum: u64, allocated: u64, skipped: u64) -> bool {
    allocated < quantum && skipped < quantum / FUTILE_YOUNG_MIN_OBJECT_BYTES
}

/// `CRATONVM_DBG_GC_OVERHEAD`, read once: the `[GC_OVERHEAD]` lines of
/// [`note_gc_productivity`] and of the futile-young verdicts.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_overhead_dbg_on() -> bool {
    static GC_OVERHEAD_DBG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GC_OVERHEAD_DBG
        .get_or_init(|| None::<std::ffi::OsString>.is_some())}

/// gcd d9/b: add `n` to slot `slot` (`cratonvm_gc::gen_heap::oome_ladder`) of
/// this VM's allocation-failure ladder census, printed at shutdown as
/// `[GC] oome_ladder:` (`--verbose:gc` / `CRATONVM_GC_STATS`). Generational
/// only; per heap, so per VM. A relaxed add, only on the ladder's rungs.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn ladder_census(shared: &SharedVm, slot: usize, n: u64) {
    shared.mem.heap.gen_note_oome_ladder(slot, n);
}

/// gcd d9/b: is this VM's old generation wedged right now
/// (`gen_heap::old_gen_is_wedged`, the overhead limit's free-space yardstick)?
/// `false` off Generational. One old-generation read; asked once per
/// collection, never per allocation.
pub(in crate::runtime::interpreter::gc_and_alloc) fn old_gen_wedged_now(shared: &SharedVm) -> bool {
    shared.mem.heap.is_generational()
        && cratonvm_gc::gen_heap::old_gen_is_wedged(
            shared.mem.heap.old_gen_headroom(),
            shared.mem.heap.heap_capacity(),
        )
}

/// Microseconds since `t0`, saturating (gcd d9/b census).
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn elapsed_us(t0: std::time::Instant) -> u64 {
    u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX)
}


/// Default number of consecutive unproductive allocation-failure GCs (each
/// freeing < 2% of capacity *while the old generation cannot absorb 2% of
/// capacity* — see [`note_gc_productivity`]) after which the allocation paths
/// declare OOM instead of continuing to GC-thrash. Overridable via
/// `CRATONVM_GC_OVERHEAD_LIMIT` (set to `0` to disable the limit entirely).
pub(in crate::runtime::interpreter::gc_and_alloc) const GC_OVERHEAD_LIMIT_CYCLES: u32 = 8;

/// After a forced (allocation-failure) GC completes, record whether it was
/// *productive* — i.e. whether it actually relieved heap pressure. Measured as
/// the bytes it freed: `before - after` live bytes, where `before` is the live
/// set at `maybe_gc_forced` entry (post-TLAB-retire) and `after` is the live set
/// once the collection finishes. A forced GC that freed < 2% of total heap
/// capacity counts toward the GC-overhead streak — provided the old generation
/// is also too full to absorb 2% of capacity, see the HIB-GCOVERHEAD-HALFFULL.1
/// note below; one that freed more resets the streak either way.
///
/// The *freed-amount* signal (not post-GC fullness) is the right one for a
/// generational heap: in a retained-allocation death-spiral the young semi-space
/// is emptied every cycle (so total *fullness* sits near young/total ≈ 50% and
/// never looks exhausted), yet the GC frees ~nothing net because every survivor
/// is promoted into an already-full old generation. Promoted bytes DO count
/// toward productivity (they re-enable young allocation, which is the point of
/// the collection) — but the 2%-of-capacity threshold still catches the
/// death-spiral: a wedged, ~full old generation cannot absorb 2% of total heap
/// capacity per cycle, so its sliver-promotions stay "unproductive" and the
/// streak still trips the overhead limit. A healthy young→old drain moves far
/// more than 2% and resets it.
/// Forced GCs happen on allocation failure (young full *and* promotion
/// blocked) -- and, gcd d9/b correcting this note, on the compiled refill
/// trigger (`tlab_alloc_shaped_inner`, site `tlab-alloc-shaped`), which takes
/// the forced door on an OCCUPANCY verdict. Those are judged here too, so on a
/// wedged old generation a compiled program's ordinary young cycles feed the
/// streak where an interpreted program's (`maybe_gc`, never judged) do not.
///
/// HIB-GCOVERHEAD-HALFFULL.1 (2026-07-31) — the freed-bytes test is only HALF
/// of HotSpot's `UseGCOverheadLimit`, which additionally requires a free-space
/// condition before it will convert GC pressure into an `OutOfMemoryError`.
/// Without that half, any defect that stops young draining reads identically to
/// the death spiral: `DefaultCatalogAndSchemaTest` died with `OutOfMemoryError`
/// after thirty forced GCs on a heap that was **49 % full with 570 MB free**,
/// because a 5 KB array allocation ran into a latched streak rather than a full
/// heap. The condition added here is the death spiral's own defining fact, taken
/// straight from the paragraph above: *"a wedged, ~full old generation cannot
/// absorb 2 % of total heap capacity per cycle"*. So a cycle counts toward the
/// streak only when the old generation genuinely cannot absorb that much. It is
/// deliberately NOT a total-fullness gate — those were rejected for the reason
/// stated above, and rightly.
///
/// ALL THREE BACKENDS NOW FEED THIS (2026-09-21). Two of its three inputs used
/// to be hardwired on G1 and ZGC: `bytes_promoted_total` answered a constant
/// `0` on both, so the one credit this metric grants for draining young — the
/// credit a G1 pause is most likely to earn, since a G1 pause IS a young drain
/// — was inexpressible on two of three collectors, and `live_bytes_estimate`
/// and `old_gen_headroom` reached them through a catch-all arm rather than a
/// stated one. G1 now counts bytes copied into Old regions and ZGC counts bytes
/// aged across the promotion threshold; the dispatcher's three accessors are
/// exhaustive, so the next backend to grow a counter gets a compile error
/// instead of a silent zero. See
/// `docs/feature-designs/zgc-architecture.md`.
///
/// This is the safety net, not the fix. The `promoted=0`-forever condition that
/// exposed it was a real collector defect (selective promotion switched off by a
/// flag that had changed meaning — see `gc_quiescence::unrewritable_peer_state`)
/// and is fixed at its source. What this guarantees is that the next such defect
/// surfaces as slowness, which is diagnosable, rather than as a spurious OOM on
/// a half-empty heap, which is not.
///
/// MUTATOR PROGRESS (gen r4w5/thrash5, 2026-09-24). Freed bytes are a proxy
/// for "the program can go on", and TLAB filler breaks the proxy: a collection
/// that frees 3 MB of retired-TLAB filler frees 3 MB nobody had allocated into.
/// On a wedged old generation the verdict now also counts a cycle whose window
/// since the previous forced collection shows no progress — under 2 % of
/// capacity allocated and a live set that did not fall — see
/// [`gc_cycle_is_unproductive`], which states the rule and why a program that
/// dropped its data is not caught by it.
///
/// `sealed_after` (gc-common w5-a): `(live, promoted_total)` sampled inside the
/// pause by [`run_collection_pause`] ([`CollectedPause::productivity_after`]).
/// `None` reads both now, as before -- after the release, where the released
/// mutators' allocation is counted against this collection.
pub(in crate::runtime::interpreter) fn note_gc_productivity(
    shared: &SharedVm,
    before_live: usize,
    before_promoted: u64,
    sealed_after: Option<(usize, u64)>,
) {
    note_gc_productivity_with(shared, before_live, before_promoted, sealed_after, None);
}

/// `CRATONVM_GC_OVERHEAD_SEALED_WINDOW` (gci i2/oo, default OFF): the
/// allocation-failure door's verdict reads its progress window from counters
/// sealed inside the pause, and the window plus the streak are one critical
/// section keyed on the pause ([`note_gc_productivity_for_pause`]). Opt-in
/// because it moves when the crawl backstop latches
/// (`gci-i1pp-overhead-verdict-bookkeeping-runs-after-the-release-CLOSED-20261013`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_overhead_sealed_window_enabled() -> bool {
    false
}

/// The allocation-failure door's verdict for the pause it initiated
/// ([`maybe_gc_forced_collected`]).
///
/// Default (switch off): [`note_gc_productivity`] with the sealed live and
/// promoted figures, byte for byte as before -- the progress window's counters
/// are read after the release.
///
/// `CRATONVM_GC_OVERHEAD_SEALED_WINDOW` on (gci i2/oo, page
/// `gci-i1pp-overhead-verdict-bookkeeping-runs-after-the-release-CLOSED-20261013`):
/// 1. the window closes on `bytes_allocated_total` / `thread_allocated_total`
///    as sealed inside the pause, so the released mutators' first refills are
///    credited to the window that opens, not the one that closed;
/// 2. the four window marks and the streak are written under
///    `GcProgressMarks::verdict`, so two threads' verdicts never interleave;
/// 3. a verdict for an OLDER pause than the newest one judged is dropped: it
///    can neither reset a streak a newer unproductive pause raised nor judge a
///    window twice.
pub(in crate::runtime::interpreter) fn note_gc_productivity_for_pause(
    shared: &SharedVm,
    before_live: usize,
    before_promoted: u64,
    sealed: Option<SealedProductivity>,
) {
    let after = sealed.map(|s| (s.live, s.promoted));
    let Some(s) = sealed.filter(|_| gc_overhead_sealed_window_enabled()) else {
        note_gc_productivity(shared, before_live, before_promoted, after);
        return;
    };
    let mut judged = shared.mem.gc_progress.verdict.lock();
    if !sealed_verdict_is_newer(*judged, s.pause_key) {
        // The engagement evidence of item 3 (no `unproductive=` field, so it
        // never counts as a verdict line).
        if gc_overhead_dbg_on() {
            eprintln!(
                "[GC_OVERHEAD] sealed-window dropped an older verdict: pause_key={} judged={}",
                s.pause_key, *judged
            );
        }
        return;
    }
    *judged = s.pause_key;
    note_gc_productivity_with(
        shared,
        before_live,
        before_promoted,
        after,
        Some((s.bytes_allocated, s.thread_allocated)),
    );
}

/// The rule of [`note_gc_productivity_for_pause`]'s step 3: only a pause newer
/// than the newest one judged is judged. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn sealed_verdict_is_newer(last_judged: u64, pause_key: u64) -> bool {
    pause_key > last_judged
}

/// [`note_gc_productivity`]'s body. `sealed_counters`:
/// `(bytes_allocated_total, thread_allocated_total)` sampled inside the pause
/// (the sealed window); `None` reads both now, as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_gc_productivity_with(
    shared: &SharedVm,
    before_live: usize,
    before_promoted: u64,
    sealed_after: Option<(usize, u64)>,
    sealed_counters: Option<(u64, u64)>,
) {
    let cap = shared.mem.heap.heap_capacity();
    if cap == 0 {
        return;
    }
    // Free-list-aware live metric (see the capture site in `maybe_gc_forced`):
    // the non-moving sweep reclaims into the young free list without moving
    // the bump cursor, so `allocated_bytes` would read a fully-productive
    // sweep as "freed 0" and falsely latch the overhead limit.
    let (after_live, promoted_total_after) = sealed_after.unwrap_or_else(|| {
        (
            shared.mem.heap.live_bytes_estimate(),
            shared.mem.heap.bytes_promoted_total(),
        )
    });
    // A promotion-only cycle conserves live bytes but still did useful
    // allocation-enabling work (it drained young), so credit promoted bytes.
    // The 2%-of-capacity threshold below still catches the genuine
    // everything-survives-into-a-full-old-gen death spiral.
    let promoted = promoted_total_after.saturating_sub(before_promoted) as usize;
    let freed = before_live
        .saturating_sub(after_live)
        .saturating_add(promoted);
    // The free-space half (see the doc comment): the old generation must be
    // unable to absorb 2% of total capacity — the death spiral's own definition
    // of "wedged" — before a sliver-freeing cycle counts toward the streak.
    // Same 2%-of-`cap` yardstick as the freed-bytes test, so the two halves
    // cannot drift apart.
    let old_headroom = shared.mem.heap.old_gen_headroom();
    let backstop_only = gc_overhead_backstop_only(shared);
    // GCJ old-generation retirement: a nearly full CURRENT capacity is not
    // exhaustion while a refused request can still grow it. javac at 192 MiB
    // reached this backstop before using its reservation; disabling only the
    // backstop completed all 1,795 exact HotSpot class files. Credit that
    // headroom to the default crawl decision. An explicit overhead limit
    // keeps its existing current-capacity pressure policy.
    let pressure_headroom = if backstop_only {
        shared.mem.heap.old_gen_headroom_with_growth()
    } else {
        old_headroom
    };
    // Use the same two-percent threshold as the Generational young-trigger
    // wedge policy (`gen_heap::young_trigger_floor_after_collection`). The
    // default crawl decision also credits the growth space described above.
    let old_gen_wedged = cratonvm_gc::gen_heap::old_gen_is_wedged(pressure_headroom, cap);
    // unproductive: the old gen is wedged AND (freed < 2% of capacity OR no
    // mutator progress) -- `gc_cycle_is_unproductive`.
    // Widening: usize -> u64 is loss-free on every supported target.
    let freed_sliver = two_percent_sliver(freed as u64, cap);
    // gen r4w5/thrash5: the mutator-progress half (see `MutatorProgress`).
    let progress = sample_mutator_progress_at(shared, after_live, sealed_counters);
    // gcg g4/oo: under the HotSpot default's crawl backstop only a cycle that
    // starved the mutators counts (`overhead_limit_word_for`).
    let unproductive = if backstop_only {
        gc_cycle_is_progress_starved(cap, old_gen_wedged, progress)
            // zr z2/cp (rv3's `zgc-zr3rv3-overhead-backstop-progress-half-is-
            // implied-on-zgc`): on ZGC "allocated under 2 % of the heap" is
            // implied by "under 2 % free", so the window must ALSO show the
            // live set growing (`zgc_window_is_crawl`).
            && (!zgc_overhead_judged(shared) || zgc_window_is_crawl(progress))
    } else {
        gc_cycle_is_unproductive(freed, cap, old_gen_wedged, progress)
    };
    if unproductive {
        // gcd d9/b: `[GC] oome_ladder: ladder_forced_unproductive`.
        ladder_census(
            shared,
            cratonvm_gc::gen_heap::oome_ladder::FORCED_UNPRODUCTIVE,
            1,
        );
    }
    let streak = if unproductive {
        shared
            .mem
            .gc_unproductive_streak
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    } else {
        shared
            .mem
            .gc_unproductive_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
        // gce e2/o: the latched episode is over.
        reset_latched_grace(shared);
        0
    };
    // Cached: this runs after every forced collection, and an unarmed gate
    // must not cost a flag-table probe each time (`gc_overhead_dbg_on`).
    if gc_overhead_dbg_on() {
        // `young_*` (SB-LOADER-ZIPCONTENT, 2026-08-04): `before`/`after` are
        // `live_bytes_estimate`, i.e. `young.used - young.free_list + old.used`.
        // A run of non-moving young sweeps leaves the young bump cursor pinned
        // at the top with the reclaimed space in the free list, so a heap that
        // is 70% free reads as "148 MB live" and every diagnosis stops there.
        // `young_largest_free` vs `young_free_list` is the fragmentation face.
        let (y_used, y_free, y_largest, y_cap) = shared.mem.heap.young_occupancy();
        // Selective-promotion census: `promoted=0` for hundreds of cycles has
        // at least three very different causes (pass never ran / nothing
        // tenurable / everything pinned) and the aggregate cannot tell them
        // apart. See `gen_heap::SP_CENSUS`.
        let (sw, sel, defrag, cand, pin, unaged, evac, ofull) =
            cratonvm_gc::gen_heap::selective_promotion_census();
        // Widening: usize -> i128 is loss-free; -1 prints "no window yet".
        let (progress_allocated, progress_prev_live) = progress.map_or((-1i128, -1i128), |p| {
            (i128::from(p.allocated), p.prev_live_after as i128)
        });
        eprintln!(
            "[GC_OVERHEAD] before={before_live} after={after_live} promoted={promoted} \
             freed={freed} cap={cap} old_headroom={old_headroom} \
             pressure_headroom={pressure_headroom} freed_sliver={freed_sliver} \
             progress_allocated={progress_allocated} progress_prev_live={progress_prev_live} \
             old_gen_wedged={old_gen_wedged} unproductive={unproductive} streak={streak} \
             young_used={y_used} young_free_list={y_free} young_largest_free={y_largest} \
             young_cap={y_cap} sp_sweeps={sw} sp_selective={sel} sp_defrag={defrag} \
             sp_candidates={cand} sp_pinned={pin} sp_unaged={unaged} sp_evacuated={evac} \
             sp_old_full={ofull} backstop={backstop}",
            // gcg h1/ob (handoff item 23): which verdict judged this cycle --
            // `true` = the HotSpot default's crawl backstop
            // (`gc_cycle_is_progress_starved`), so `unproductive=true
            // backstop=true` up to `streak=8` is the backstop engaging.
            backstop = backstop_only,
        );
    }
}

/// Is `bytes` under 2 % of `cap`? The one yardstick of the overhead limit (the
/// freed-bytes half, the progress half, and — through
/// `gen_heap::old_gen_is_wedged` — the free-space half).
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn two_percent_sliver(bytes: u64, cap: usize) -> bool {
    // Widening: u64/usize -> u128 cannot overflow the products.
    u128::from(bytes) * 100 < (cap as u128) * 2
}

/// zr z2/cp: the part of a forced collection's window the live set kept, as a
/// fraction `1 / ZGC_CRAWL_RETAINED_DIVISOR` of what the mutators allocated:
/// at or above it the window is the crawl ([`zgc_window_is_crawl`]).
pub(in crate::runtime::interpreter) const ZGC_CRAWL_RETAINED_DIVISOR: u64 = 16;

/// zr z2/cp: is this forced collection's window the near-OOM CRAWL -- the
/// heap filling with LIVE data a few kilobytes at a time -- rather than a
/// program working in the free space it has? Applied on top of
/// [`gc_cycle_is_progress_starved`] on ZGC, whose own "allocated under 2 % of
/// the heap" a whole-heap backend passes by construction once it is wedged
/// (the mutators can allocate at most the free space the collection left;
/// rv3's `zgc-zr3rv3-overhead-backstop-progress-half-is-implied-on-zgc`).
///
/// The crawl RETAINS: `Zr2CpCapacity grow` / `ProxyBoxingOomeProbe` keep
/// ~40 % of every window, so the live set after each collection exceeds the
/// previous one's and the free space shrinks toward zero. A program whose
/// steady live set sits just under `-Xmx` keeps nothing of its window:
/// `Zr3Rv3NearFullSteady 975 400 1024 -Xmx64m` (debug build) showed
/// `after=66439000` on every one of its forced collections, windows of
/// 250-515 KB, and `oome phase=steady` under the window-size rule this
/// replaces; the host's 982-permille row at 256m freed exactly what it
/// allocated (z2c). So a cycle counts only when the live set grew by at
/// least `1 / ZGC_CRAWL_RETAINED_DIVISOR` of the window (and the window is
/// not empty: back-to-back collections of several refused threads, or a
/// native-boundary relief, allocate nothing in between and grow nothing).
///
/// Not a HotSpot rule: HotSpot's `UseGCOverheadLimit` exists only on Parallel
/// GC, and its ZGC throws only when a stalled allocation still fails after its
/// collections. This is the narrowest test found that still ends the crawl
/// (429 collections / 33.7 s CPU on `grow` without the backstop, against
/// HotSpot's ~144 / 1.3 s on the same host). No window: never.
pub(in crate::runtime::interpreter) fn zgc_window_is_crawl(progress: Option<MutatorProgress>) -> bool {
    progress.is_some_and(|p| {
        // Widening: usize -> u64 is loss-free on every supported target.
        let grew = (p.live_after as u64).saturating_sub(p.prev_live_after as u64);
        p.allocated > 0
            && grew > 0
            && grew.saturating_mul(ZGC_CRAWL_RETAINED_DIVISOR) >= p.allocated
    })
}

/// The mutator-progress window of one allocation-failure collection: what the
/// program did between the previous forced collection and this one.
/// gen r4w5/thrash5 (2026-09-24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter) struct MutatorProgress {
    /// Bytes the mutators allocated since the previous forced collection
    /// ended: the larger of the two per-VM allocation counters' deltas
    /// (`bytes_allocated_total` counts TLAB and slow-path allocation,
    /// `thread_allocated_total` adds the native funnels), so an allocation
    /// either one sees counts as progress.
    pub allocated: u64,
    /// The live-bytes estimate the previous forced collection left.
    pub prev_live_after: usize,
    /// The live-bytes estimate this collection left.
    pub live_after: usize,
}

/// The verdict of [`note_gc_productivity`], pure.
///
/// A collection is unproductive only while the old generation is wedged
/// (HIB-GCOVERHEAD-HALFFULL.1, unchanged), and then when EITHER
///
/// * it freed under 2 % of capacity (the historical freed-bytes half), OR
/// * **(gen r4w5/thrash5)** the mutators made no progress since the previous
///   forced collection: they allocated under 2 % of capacity in the whole
///   window, and the live set did not drop by 2 % of capacity either. The
///   collection then freed only what nobody allocated into — the filler
///   buried under retired TLABs — and the next allocation fails again on the
///   same live set.
///
/// The freed-bytes half alone cannot see that loop: every cycle of
/// `GenR4W4HeapFullThrashProbe` step `threads` freed 3.4 MB (3.5 % of a 96 MiB
/// heap) of TLAB filler while the program linked a few hundred bytes. The
/// live-set condition is what keeps a program that DROPPED its data after an
/// `OutOfMemoryError` out of this arm: its next collection's live set falls,
/// even though it has allocated almost nothing since the error.
///
/// `progress` is `None` before the first forced collection (no window yet),
/// under `CRATONVM_GC_OVERHEAD_PROGRESS=0`, and (gen r4w6/review6) on every
/// backend but Generational; the verdict is then the historical one byte for
/// byte.
pub(in crate::runtime::interpreter) fn gc_cycle_is_unproductive(
    freed: usize,
    cap: usize,
    old_gen_wedged: bool,
    progress: Option<MutatorProgress>,
) -> bool {
    if !old_gen_wedged {
        return false;
    }
    // Widening: usize -> u64 is loss-free on every supported target.
    if two_percent_sliver(freed as u64, cap) {
        return true;
    }
    progress.is_some_and(|p| {
        let live_flat = p.live_after.saturating_add(cap / 50) >= p.prev_live_after;
        two_percent_sliver(p.allocated, cap) && live_flat
    })
}

/// `CRATONVM_GC_OVERHEAD_PROGRESS=0` switches off the mutator-progress half of
/// the GC-overhead limit ([`gc_cycle_is_unproductive`]) and the Generational
/// "full collection before the overhead-limit error" exit in
/// [`collect_and_retry_with_thread`], restoring both in the same binary.
/// Default ON. gen r4w5/thrash5 (2026-09-24).
///
/// gen r4w6/review6: read from the typed snapshot
/// (`GcFlags::gc_overhead_progress`), not memoised in a process-wide
/// `OnceLock` — that latched the first reader's value, a test's
/// `with_thread_overrides` included, for every later VM and test.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_overhead_progress_enabled() -> bool {
    cratonvm_types::flags().gc.gc_overhead_progress
}

/// [`sample_mutator_progress`] closing the window on `counters`
/// (`(bytes_allocated_total, thread_allocated_total)` sealed inside the pause,
/// gci i2/oo) instead of the counters as they read now.
pub(in crate::runtime::interpreter::gc_and_alloc) fn sample_mutator_progress_at(
    shared: &SharedVm,
    live_after: usize,
    counters: Option<(u64, u64)>,
) -> Option<MutatorProgress> {
    use std::sync::atomic::Ordering;
    let marks = &shared.mem.gc_progress;
    let (bytes_now, thread_now) = counters.unwrap_or_else(|| {
        (
            shared.mem.bytes_allocated_total.load(Ordering::Relaxed),
            shared.mem.thread_allocated_total.load(Ordering::Relaxed),
        )
    });
    let window = marks.armed.load(Ordering::Relaxed).then(|| {
        let bytes_prev = marks.bytes_allocated_mark.load(Ordering::Relaxed);
        let thread_prev = marks.thread_allocated_mark.load(Ordering::Relaxed);
        let prev_live = marks.live_after_mark.load(Ordering::Relaxed);
        MutatorProgress {
            allocated: bytes_now
                .saturating_sub(bytes_prev)
                .max(thread_now.saturating_sub(thread_prev)),
            // Truncation: a live-bytes figure recorded from a `usize` below.
            prev_live_after: usize::try_from(prev_live).unwrap_or(usize::MAX),
            live_after,
        }
    });
    marks
        .bytes_allocated_mark
        .store(bytes_now, Ordering::Relaxed);
    marks
        .thread_allocated_mark
        .store(thread_now, Ordering::Relaxed);
    // Widening: usize -> u64 is loss-free on every supported target.
    marks
        .live_after_mark
        .store(live_after as u64, Ordering::Relaxed);
    marks.armed.store(true, Ordering::Relaxed);
    // gen r4w6/review6: Generational only. The evidence for this half is the
    // generational thrash (`GenR4W4HeapFullThrashProbe`, `GenR4W5ThreadsOomProbe`);
    // on G1 and ZGC — ZGC is the default collector — it changed the default
    // overhead verdict unmeasured (and the JIT's inline TLAB bumps reach the
    // counters it compares only at a TLAB retire). Measure it there first
    // (the same probes under `-XX:+UseG1GC` / the default, `--verbose:gc`
    // with `CRATONVM_DBG_GC_OVERHEAD=1`), then widen this.
    window.filter(|_| {
        gc_overhead_progress_enabled()
            && (shared.mem.heap.is_generational() || zgc_overhead_judged(shared))
    })
}

/// zr z2/cp: `CRATONVM_ZGC_SOFT_CLEAR_CONSERVATIVE` (default ON, `=0` kills;
/// ZGC only). The referents of the soft references the last-ditch collection
/// condemned (`clear_all`: this thread's pre-collection pass applied
/// `condemn_all_soft_refs`) that this pause's conservative compiled-frame
/// scan published (`gc_quiescence::pinned_jit_roots_snapshot`). The caller
/// treats them as unmarked for reference processing only, so those soft
/// references are cleared and enqueued instead of restored. Memory is
/// untouched: the referent stays where the conservative word kept it until a
/// later cycle.
///
/// Why. `tools/probes/interp/L5/SoftRefBeforeOome` printed `cleared false`
/// on ZGC with the JIT on (`cleared true` with `--nojit`, with
/// `CRATONVM_DBG_NO_JIT_ROOT_SCAN=1`, and on HotSpot): `CRATONVM_DBG=weakref`
/// showed the referent condemned and nulled, then surviving the collection
/// and restored; `CRATONVM_DBG_JIT_ROOTSCAN=1` showed the OSR-compiled loop
/// frame scanned conservatively (`precise_only=false`,
/// `reason=unregistered-jit-frame-on-stack`, 192 words), one of them the dead
/// result of the loop's `soft.get()`. The specification wants every softly
/// reachable object released before `OutOfMemoryError`; a stale
/// compiled-frame word is not a Java reference.
///
/// The trade: a live compiled LOCAL holding the referent is a conservative
/// word too, so a program that keeps `soft.get()`'s result in a compiled
/// frame across the refused allocation sees its soft reference cleared while
/// it still holds the object (HotSpot would keep it). The referent itself is
/// never freed early. Only on the last-ditch collection before an
/// `OutOfMemoryError`. zr z7/sp narrowed it: a live local that a precise root
/// also records (the shadow stack, an interpreter slot) is kept
/// (`CRATONVM_ZGC_SOFT_CLEAR_PRECISE_ROOT_VALUES`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_soft_clear_all_conservative_referents(
    shared: &SharedVm,
    ref_proc: &cratonvm_gc::reference::ReferenceProcessor,
    clear_all: bool,
    // addr-keyed: local to this last-ditch collection (one pause); dropped before the world resumes.
) -> std::collections::HashSet<usize> {
    let mut out = std::collections::HashSet::new();
    if !clear_all
        || !shared.mem.heap.is_zgc()
    {
        return out;
    }
    let condemned = ref_proc.soft_pre_nulled_active_pairs();
    if condemned.is_empty() {
        return out;
    }
    // addr-keyed: local to this last-ditch collection (one pause); dropped before the world resumes.
    let pinned: std::collections::HashSet<usize> =
        cratonvm_gc::gc_quiescence::pinned_jit_roots_snapshot()
            .into_iter()
            .collect();
    out.extend(soft_referents_named_by(&condemned, &pinned));
    // zr z5/rv5 (`CRATONVM_ZGC_SOFT_CLEAR_PRECISE_FIRST`): keep every
    // referent the collection reached from the PRECISE roots alone -- a heap
    // path, a static, a registered frame's slot -- as HotSpot keeps a soft
    // reference to a strongly reachable object. Only what nothing but a
    // conservative word kept is cleared. `None` (the probe did not run: not
    // armed, or a concurrently marked cycle) leaves the cp rule as it was.
    let named = out.len();
    // zr z7/sp: taken unconditionally so an armed pause's list never reaches
    // a later one.
    let root_named = SOFT_PRECISE_ROOT_NAMED.with(|c| std::mem::take(&mut *c.borrow_mut()));
    if let Some(kept) = shared.mem.heap.zgc_take_soft_precise_kept() {
        // addr-keyed: local to this last-ditch collection (one pause); dropped before the world resumes.
        let kept: std::collections::HashSet<usize> = kept.into_iter().collect();
        out.retain(|a| !kept.contains(a));
        let after_mark = out.len();
        // zr z7/sp (`CRATONVM_ZGC_SOFT_CLEAR_PRECISE_ROOT_VALUES`): the
        // collector's first drain defers every ROOT VALUE a conservative word
        // also names, so a referent that a precise root holds DIRECTLY (a
        // static field that is the referent, an interpreter local) read as
        // "conservative only" and was cleared. Those were recorded by
        // provenance at arm time ([`soft_precise_root_named`]) and are kept.
        out.retain(|a| !root_named.contains(a));
        // zr z7/sp: the engagement witness the host probes read
        // (`Zr7SpStaticHolderStaleWord`, `Zr7SpLiveLocalWeakCleaner`): how
        // many condemned referents a conservative word named, how many of
        // those the precise closure reached, and how many a precise root
        // named directly.
        if crate::runtime::interpreter::dbg_weakref_on() {
            eprintln!(
                "[weakref] clear-all: precise-first condemned={} named={} precise_kept={} \
                 root_kept={} cleared={}",
                condemned.len(),
                named,
                named - after_mark,
                after_mark - out.len(),
                out.len()
            );
        }
    } else if crate::runtime::interpreter::dbg_weakref_on() {
        eprintln!(
            "[weakref] clear-all: precise-first not run condemned={} named={}",
            condemned.len(),
            named
        );
    }
    if !out.is_empty() && crate::runtime::interpreter::dbg_weakref_on() {
        eprintln!(
            "[weakref] clear-all: {} condemned soft referent(s) held by conservative \
             compiled-frame words are cleared, not restored",
            out.len()
        );
    }
    out
}

/// zt t11/gn3 (`CRATONVM_ZGC_GEN_YOUNG_JNI_WEAK_STRONG`, default ON since zt
/// t14/gn5, `=0` kill; ZGC's generational mode only): the roots a young cycle armed to keep its
/// referents (`CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES`, default ON) adds for
/// the JNI weak globals -- every uncleared weak global's referent, so the
/// weak-global sweep after the pause (`jni::sweep_weak_global_refs_with`)
/// finds it marked and keeps the handle, as the young cycle keeps a
/// `java.lang.ref` referent. Without it a minor clears a weak global whose
/// young referent nothing else holds. HotSpot 25's minor keeps it
/// (`Zt10Gn2YoungJniWeak minor`: `cleared=false`; t12 rows `gn4_JW_1s` SAME,
/// `gn4_JW_1` the kill arm `cleared=true`, `minors=1 majors=0`). A root to an old object costs nothing (it is
/// pre-marked). On a MISPREDICTED arming (the collection runs whole-heap,
/// `young_refs_mispredicted=`) the referents survive that major, one major
/// longer than HotSpot, as the `java.lang.ref` referents of the same arming
/// do. Empty (one relaxed load) on G1, GenGC, non-generational ZGC and
/// every unarmed collection; the flag is read only on an armed one.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_young_cycle_jni_weak_roots(shared: &SharedVm) -> Vec<ObjectRef> {
    if !shared.mem.heap.zgc_young_refs_strong_armed()
    {
        return Vec::new();
    }
    crate::native::jni::weak_global_referent_addrs(shared)
        .into_iter()
        // SAFETY: each address is an uncleared weak global's referent, read
        // under the table's lock at this safepoint: a live object base the
        // collection about to run has not moved yet.
        .map(|addr| unsafe { ObjectRef::from_raw(addr as *mut u8) })
        .collect()
}

/// zr z5/rv5: `CRATONVM_ZGC_SOFT_CLEAR_PRECISE_FIRST` (default ON, `=0`
/// kills; ZGC only, under `CRATONVM_ZGC_SOFT_CLEAR_CONSERVATIVE`). Called in
/// the pause between the pre-collection reference pass and the collection:
/// when that pass applied the last-ditch clear-all rule, arm the collector to
/// report which condemned soft referents the precise roots reach before the
/// conservative compiled-frame words are traced
/// (`ZgcRealHeap::arm_soft_precise_probe`). That collection marks serially
/// in two drains; nothing else changes.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_arm_soft_precise_probe_for_pause(
    shared: &SharedVm,
    roots: &[ObjectRef],
    initiator_end: usize,
) {
    SOFT_PRECISE_ROOT_NAMED.with(|c| c.borrow_mut().clear());
    if !shared.mem.heap.is_zgc()
        || !crate::runtime::interpreter::soft_clear_all_applied_peek()
    {
        return;
    }
    let candidates: Vec<usize> = shared
        .mem
        .ref_processor
        .lock()
        .soft_pre_nulled_active_pairs()
        .into_iter()
        .map(|(_, referent)| referent)
        .collect();
    {
        let ranges = crate::memory::roots::initiator_conservative_root_ranges(shared.vm_identity);
        let named = soft_precise_root_named(&candidates, roots, initiator_end, ranges);
        if !named.is_empty() && crate::runtime::interpreter::dbg_weakref_on() {
            // Which `collect_roots` step holds each claimed referent (the
            // step names resolve under `CRATONVM_DBG_ROOT_REMAP_AUDIT=1`).
            let r = ranges.unwrap_or_default();
            for (i, root) in roots.iter().enumerate().take(initiator_end) {
                let a = root.as_ptr() as usize;
                if !in_any_range(i, &r) && named.contains(&a) {
                    eprintln!(
                        "[weakref] clear-all: precise root[{i}] names condemned referent {a:#x} \
                         (excluded {r:?}; step {})",
                        crate::memory::roots::scan_section_of(i)
                    );
                }
            }
        }
        SOFT_PRECISE_ROOT_NAMED.with(|c| *c.borrow_mut() = named);
    }
    shared.mem.heap.zgc_arm_soft_precise_probe(candidates);
}

thread_local! {
    // zr z7/sp: the condemned soft referents a PRECISE root of this pause's
    // initiator names directly ([`soft_precise_root_named`]). Written by
    // [`zgc_arm_soft_precise_probe_for_pause`], taken by
    // [`zgc_soft_clear_all_conservative_referents`] in the same pause.
    // addr-keyed: local to this last-ditch collection (one pause); taken before the world resumes.
    pub(in crate::runtime::interpreter::gc_and_alloc) static SOFT_PRECISE_ROOT_NAMED: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(in crate::runtime::interpreter::gc_and_alloc) fn in_any_range(i: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|&(s, e)| i >= s && i < e)
}

/// zr z7/sp: the `candidates` that a precise root of the initiator's own
/// `collect_roots` result names: `roots[..initiator_end]` outside
/// `conservative` (the ranges that may hold conservative compiled-frame
/// words: step 14's frame scan and step 11's own earlier deposit; `None` =
/// this thread's last scan was another VM's, so nothing is claimed). The
/// collector cannot tell a static whose value IS the referent from a
/// conservative word holding the same address (it sees only values, and
/// defers both to the second drain), so the provenance is read here. Peers'
/// snapshot roots (past `initiator_end`) are not claimed: a snapshot mixes a
/// peer's precise slots with its conservative words.
pub(in crate::runtime::interpreter::gc_and_alloc) fn soft_precise_root_named(
    candidates: &[usize],
    roots: &[ObjectRef],
    initiator_end: usize,
    conservative: Option<[(usize, usize); 2]>,
) -> Vec<usize> {
    let Some(ranges) = conservative else {
        return Vec::new();
    };
    if candidates.is_empty() {
        return Vec::new();
    }
    // addr-keyed: local to this last-ditch collection (one pause); dropped before the world resumes.
    let wanted: std::collections::HashSet<usize> =
        candidates.iter().copied().filter(|&c| c != 0).collect();
    let mut out: Vec<usize> = roots
        .iter()
        .take(initiator_end)
        .enumerate()
        .filter(|(i, _)| !in_any_range(*i, &ranges))
        .map(|(_, r)| r.as_ptr() as usize)
        .filter(|a| wanted.contains(a))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The pure half of [`zgc_soft_clear_all_conservative_referents`]: the
/// condemned `(reference, referent)` pairs whose referent a conservative word
/// names.
pub(in crate::runtime::interpreter::gc_and_alloc) fn soft_referents_named_by(
    condemned: &[(usize, usize)],
    // addr-keyed: local to this last-ditch collection (one pause); dropped before the world resumes.
    conservative_words: &std::collections::HashSet<usize>,
) -> Vec<usize> {
    condemned
        .iter()
        .filter(|(_, referent)| *referent != 0 && conservative_words.contains(referent))
        .map(|(_, referent)| *referent)
        .collect()
}

/// zr z2/cp: `CRATONVM_ZGC_OVERHEAD_BACKSTOP` (**opt-in** `=1` since
/// 2026-10-13; it landed default ON in `79bd6beb75`) -- the GC-overhead crawl
/// backstop on ZGC.
///
/// Why opt-in. HotSpot's `UseGCOverheadLimit` exists only on Parallel GC;
/// HotSpot ZGC throws only when an allocation still cannot be satisfied after
/// its last-resort collection. Any overhead verdict on ZGC throws while the
/// heap still has room, and the host showed the Java-visible cost: the census
/// row `GcOverheadProbe -XX:+UseZGC -Xmx256m` went from SAME (z2b) to DIFF on
/// z2c / z3b in both modes (the `catch` block's `println` succeeds in the ~2 %
/// the verdict left free, where HotSpot's throws a second `OutOfMemoryError`),
/// and rv3's `Zr3Rv3NearFullSteady 982 400 1024 -Xmx256m` threw at round 55
/// under the window-size rule. Off, the near-OOM crawl the backstop ended is
/// back (`Zr2CpCapacity grow -Xmx256m`: ~3 300 collections, 126 s CPU on z2c's
/// kill arm), filed as
/// `docs/feature-designs/zgc-architecture.md`
/// with the JIT refill path as the lead (`--nojit` on a debug build ran 94
/// collections for the same shape at `-Xmx64m`, 25 of them forced).
///
/// Why. Once the z2/cp failure tier
/// (`CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT`) let a ZGC heap fill to its last
/// megabytes, every refused allocation on a nearly-full heap ran a forced
/// whole-heap collection that freed the few kilobytes the mutator had churned
/// since the previous one, and the program limped on: `Zr2CpCapacity grow
/// -Xmx256m` ran 429 collections and 33.7 s CPU (`--compatible`: 5 169, 356 s)
/// where HotSpot ZGC runs ~52 and 1.3 s. The backstop's verdict was
/// Generational-only (no progress window on ZGC, so no streak, so no limit).
///
/// With it on, ZGC gets the progress window ([`sample_mutator_progress`]) and
/// the latched exit throws after the soft-reference rung, like Generational's
/// judged exit (the forced ZGC cycle IS a whole-heap collection, so it is the
/// full collection the verdict needs): eight consecutive forced collections on
/// a heap with under 2 % headroom, each of which let the mutators allocate under
/// 2 % of the heap with the live set flat, and the next refusal is the
/// `OutOfMemoryError` (after the bounded latched grace). A heap with room, or a
/// program that dropped its data (the live set falls), never counts.
pub(crate) fn zgc_overhead_judged(shared: &SharedVm) -> bool {
    shared.mem.heap.is_zgc()
        && false
}


/// Returns `true` when the heap has GC-thrashed past the overhead limit — i.e.
/// `GC_OVERHEAD_LIMIT_CYCLES` consecutive forced GCs each freed < 2% of the
/// heap while the old generation was too full to absorb that much (both halves
/// required — see `note_gc_productivity`, and
/// `fixed-suite-bugs/hibernate/` for the spurious-OOM-at-49%-full
/// report that added the second half).
/// The allocation-failure paths call this right after `maybe_gc_forced`
/// and, when it is `true`, surface a catchable `OutOfMemoryError` (the
/// pre-allocated `singleton_oom`) instead of retrying into an O(n²) death-spiral
/// on a heap full of live (retained) objects. Mirrors HotSpot's
/// `UseGCOverheadLimit`. Disabled (always `false`) when this VM's limit
/// ([`gc_overhead_limit_cycles`]) is `0`: `CRATONVM_GC_OVERHEAD_LIMIT=0`,
/// `-XX:-UseGCOverheadLimit`, or the HotSpot-25 default under
/// `CRATONVM_GC_OVERHEAD_LIMIT_HOTSPOT=1`.
pub fn gc_overhead_limit_exceeded(shared: &SharedVm) -> bool {
    let limit = gc_overhead_limit_cycles(shared);
    if limit == 0 {
        return false; // disabled for this VM
    }
    shared
        .mem
        .gc_unproductive_streak
        .load(std::sync::atomic::Ordering::Relaxed)
        >= limit
}

/// gcf f2/o: this VM's GC-overhead limit, in consecutive unproductive forced
/// cycles (`0` = no limit). Resolved once PER VM (it used to be one process
/// `static OnceLock`, so two VMs -- or two unit tests -- could not differ):
///
/// PERF: [`gc_overhead_limit_exceeded`] runs on the per-allocation slow path
/// (`jit_new_object` and the interpreter allocation sites). An uncached
/// `runtime_var` there was ~6% of binarytrees-18 wall time (getenv does a
/// linear environ scan), hence the per-VM `OnceLock`
/// (`AllocLadderState::overhead_limit_cycles`): one acquire load per call.
pub(crate) fn gc_overhead_limit_cycles(shared: &SharedVm) -> u32 {
    overhead_limit_word(shared) & !OVERHEAD_CRAWL_BACKSTOP_ONLY
}

/// gcg g4/oo: is this VM's limit the crawl backstop only
/// ([`overhead_limit_word_for`])? Then a forced collection counts toward the
/// streak only when it starved the mutators ([`gc_cycle_is_progress_starved`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_overhead_backstop_only(shared: &SharedVm) -> bool {
    overhead_limit_word(shared) & OVERHEAD_CRAWL_BACKSTOP_ONLY != 0
}

/// The per-VM resolved limit word: the cycles, plus
/// [`OVERHEAD_CRAWL_BACKSTOP_ONLY`] when it is the backstop. One acquire load
/// after the first call (`AllocLadderState::overhead_limit_cycles`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn overhead_limit_word(shared: &SharedVm) -> u32 {
    *shared
        .mem
        .alloc_ladder
        .overhead_limit_cycles
        .get_or_init(|| {
            let env = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok();
            let hotspot = gc_overhead_limit_hotspot_default();
            overhead_limit_word_for(
                env.as_deref(),
                shared.config.use_gc_overhead_limit,
                hotspot,
                hotspot && env.is_none(),
            )
        })
}

/// gcg g4/oo: the bit of the limit word that marks the crawl backstop.
pub(in crate::runtime::interpreter::gc_and_alloc) const OVERHEAD_CRAWL_BACKSTOP_ONLY: u32 = 1 << 31;

/// gcg g4/oo (`gcg-g4jm-gen-jitoomroot-runs-out-the-clock-in-oome-thread-exit`):
/// the limit word. [`gc_overhead_limit_cycles_for`] decides the limit as
/// before; when that is `0` because of the HotSpot default
/// (`CRATONVM_GC_OVERHEAD_LIMIT_HOTSPOT`, default ON since gcg g1b) and
/// `backstop` holds, the word is [`GC_OVERHEAD_LIMIT_CYCLES`] marked
/// [`OVERHEAD_CRAWL_BACKSTOP_ONLY`].
///
/// Why. HotSpot Serial's full collection COMPACTS: after it, the free space is
/// exactly capacity minus live, the next allocations fill it, and the
/// collection after that throws `Java heap space`. CratonVM's collections do
/// not compact the old generation, and each forced young cycle frees the
/// filler that retired TLABs left behind, so a program linking small objects
/// on a full heap (two threads in `GenR4W6JitOomRootProbe`'s
/// `oome-thread-exit`) gets a few kilobytes per cycle, forever: the
/// gen r4w5/thrash5 crawl. The historical overhead limit was what ended it;
/// the HotSpot default turned the limit off and with it the crawl's end (g2a:
/// rc 124 at 300 s in both `s1b_gen_*_jor` arms). The backstop keeps only the
/// half HotSpot's compaction stands for: eight consecutive forced collections,
/// on a wedged old generation, each of which let the mutators allocate under
/// 2 % of the heap with the live set flat. A cycle that merely freed a sliver
/// while the program made progress no longer counts (that is the
/// `GC overhead limit` HotSpot's Serial does not have). Its error is the
/// ladder's ordinary `Java heap space`, after a full collection
/// (`OverheadLimitExit::Throw`). Generational only: no other backend hands
/// the verdict a progress window, so G1 and ZGC keep "no limit".
/// `CRATONVM_GC_OVERHEAD_CRAWL_BACKSTOP=0` restores the g2a answer (no limit).
pub(in crate::runtime::interpreter::gc_and_alloc) fn overhead_limit_word_for(
    env: Option<&str>,
    flag: Option<bool>,
    hotspot_default: bool,
    backstop: bool,
) -> u32 {
    let cycles = gc_overhead_limit_cycles_for(env, flag, hotspot_default);
    if cycles == 0 && hotspot_default && backstop {
        GC_OVERHEAD_LIMIT_CYCLES | OVERHEAD_CRAWL_BACKSTOP_ONLY
    } else {
        cycles
    }
}



/// gcg g4/oo: the crawl backstop's per-cycle verdict, pure: the old generation
/// is wedged and the window since the previous forced collection shows no
/// mutator progress (the progress half of [`gc_cycle_is_unproductive`]
/// alone). No window (first forced collection, `CRATONVM_GC_OVERHEAD_PROGRESS=0`,
/// any backend but Generational): never.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_cycle_is_progress_starved(
    cap: usize,
    old_gen_wedged: bool,
    progress: Option<MutatorProgress>,
) -> bool {
    old_gen_wedged
        && progress.is_some_and(|p| {
            let live_flat = p.live_after.saturating_add(cap / 50) >= p.prev_live_after;
            two_percent_sliver(p.allocated, cap) && live_flat
        })
}


/// gcf f2/o (2026-10-01), `gcf-f1o-overhead-limit-has-no-hotspot-switch-and-serial-has-no-limit`:
/// `CRATONVM_GC_OVERHEAD_LIMIT_HOTSPOT` (default ON since gcg g1, `=0` restores
/// the historical limit; `runtime_flag_default_on`)
/// makes the DEFAULT limit HotSpot 25's for the collectors this VM has: none.
/// In HotSpot 25 `UseGCOverheadLimit` (default true) is honoured only by the
/// Parallel collector (`GCOverheadChecker`, `psScavenge` / `psParallelCompact`);
/// Serial, G1 and ZGC accept the flag and never throw `GC overhead limit
/// exceeded` -- they throw `Java heap space` only when an allocation fails
/// after a full collection. Off, the historical limit
/// ([`GC_OVERHEAD_LIMIT_CYCLES`]) applies unless the flag turns it off: the
/// limit is what ends the thrash livelocks of `gengc-r4w5-thrash5` /
/// `r5w2 chain-hot`, so the default flips only on the OOME battery's A/B.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_overhead_limit_hotspot_default() -> bool {
    true
}

/// The pure resolution behind [`gc_overhead_limit_cycles`].
///
/// * `env` -- `CRATONVM_GC_OVERHEAD_LIMIT`, the developer override; when set
///   it wins (`0` = off; unparseable = the default cycles, as before).
/// * `flag` -- `-XX:±UseGCOverheadLimit` (`VmConfig::use_gc_overhead_limit`).
///   `-` turns the limit off (what HotSpot's Serial / G1 / ZGC do anyway, so
///   honouring it is parity). `+` keeps the historical limit, except under
///   the HotSpot default where it is, as in HotSpot 25 off Parallel, a no-op.
/// * `hotspot_default` -- [`gc_overhead_limit_hotspot_default`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_overhead_limit_cycles_for(
    env: Option<&str>,
    flag: Option<bool>,
    hotspot_default: bool,
) -> u32 {
    if let Some(v) = env {
        return v.trim().parse::<u32>().unwrap_or(GC_OVERHEAD_LIMIT_CYCLES);
    }
    if flag == Some(false) || hotspot_default {
        return 0;
    }
    GC_OVERHEAD_LIMIT_CYCLES
}


/// Force a GC cycle from a native method (e.g. System.gc()).
/// Runs GC with finalizer-aware resurrection, processes references,
/// and invokes pending finalizers.
///
/// Every EXPLICIT GC request reaches the collector here (`System.gc()`,
/// `Runtime.gc()`, `MemoryMXBean.gc()`, the `System.gc()` in
/// `Bits.reserveMemory`, all through `NativeContext::force_gc`), so this is
/// where this VM's [`ExplicitGcPolicy`](crate::config::ExplicitGcPolicy)
/// applies (gc-common w8-g, `-XX:±DisableExplicitGC` /
/// `-XX:±ExplicitGCInvokesConcurrent`): see `explicit_gc_action`. The
/// non-explicit collections -- `jcmd GC.run` ([`run_diagnostic_command_gc`],
/// requested through `AllocLadderState::dcmd_gc_requested`), the metaspace refusal
/// ([`force_gc_for_class_metadata`]) and every allocation door -- never read it.
pub fn force_gc_from_native(shared: &SharedVm, thread: &mut JvmThread) {
    force_gc_from_native_with_jfr_stack(shared, thread, 0);
}

/// [`force_gc_from_native`] with the requesting Java stack's `jdk.jfr`
/// backend stack-trace id (`0` = none) for its `jdk.SystemGC` row: gci i23/pk
/// (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 3). The id is taken
/// where a `NativeContext` exists (`NativeContextImpl::force_gc`, through
/// `jfr_vm_more::system_gc_stack_trace`), as HotSpot's `JVM_GC` commits the
/// event with the caller's stack.
pub fn force_gc_from_native_with_jfr_stack(
    shared: &SharedVm,
    thread: &mut JvmThread,
    jfr_stack: i64,
) {
    full_gc_door(shared, thread, shared.config.explicit_gc, true, jfr_stack);
}

/// [`force_gc_from_native`] whatever `VmConfig::explicit_gc` says: the full
/// collection and its drains, no `jdk.SystemGC` event. For VM-internal callers
/// that need a collection to shed native-side table entries
/// (`NativeContext::force_gc_for_vm`); NOT an explicit GC, so
/// `-XX:+DisableExplicitGC` cannot turn it off. gc-common w8-g
/// (`handoff-w8g-vm-internal-collection-uses-the-explicit-door`).
pub fn force_gc_for_vm(shared: &SharedVm, thread: &mut JvmThread) {
    full_gc_door(shared, thread, crate::config::ExplicitGcPolicy::Full, false, 0);
}

/// JVMTI `ForceGarbageCollection` (gci i27/ob,
/// `gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017` finding 4's
/// residual): [`force_gc_for_vm`] on its own door, [`GcDoor::JvmtiForceGc`],
/// so the collection reports HotSpot's cause, `JvmtiEnv
/// ForceGarbageCollection` (`-Xlog:gc` `Pause Full (..)`, JMX `getGcCause()`,
/// JFR), where it reported `System.gc()`. Like `force_gc_for_vm`: not an
/// explicit GC (`-XX:+DisableExplicitGC` does not gate it, no `jdk.SystemGC`).
///
/// Finalizers, cleaners, and notifications are left for their normal delivery
/// paths; this door does not run them inline on the agent thread.
pub fn force_gc_for_jvmti(shared: &SharedVm, thread: &mut JvmThread) {
    force_full_collection(shared, thread, GcDoor::JvmtiForceGc);
}

/// The `System.gc()` door's body, under `policy`; `explicit` says whether it
/// is a Java-visible explicit GC (emits `jdk.SystemGC`). `jfr_stack`: the
/// backend stack-trace id of the request's `jdk.SystemGC` row (`0` = none;
/// gci i23/pk).
pub(in crate::runtime::interpreter::gc_and_alloc) fn full_gc_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    policy: crate::config::ExplicitGcPolicy,
    explicit: bool,
    jfr_stack: i64,
) {
    let action = explicit_gc_action(policy);
    let invoked_concurrent = policy == crate::config::ExplicitGcPolicy::Concurrent;
    // gci i12/ge (`gci-i12ge-...-review-findings-20261013` item 1): the
    // `jdk.jfr` backend's `jdk.SystemGC`, ONE per request spanning its
    // collection (HotSpot's `JVM_GC` commits `EventSystemGC` around
    // `collect`); it wrote one per `System.gc()`-caused PAUSE, with
    // `invokedConcurrent` always false, and for `force_gc_for_vm` too.
    let mut backend_system_gc = None;
    if explicit && action != ExplicitGcAction::Ignore {
        emit_system_gc_jfr_event(shared, thread, invoked_concurrent);
        backend_system_gc = Some((system_gc_wall_ns(), std::time::Instant::now()));
    }
    match action {
        // `-XX:+DisableExplicitGC`: HotSpot's `JVM_GC` returns before it does
        // anything, so no collection, no drain, no `jdk.SystemGC` event and no
        // count anywhere (`gc_cycle_count`, `system_gc_collections`, the JMX
        // beans).
        ExplicitGcAction::Ignore => return,
        ExplicitGcAction::Collect => force_full_collection(shared, thread, GcDoor::SystemGc),
    }
    if let Some((start_ns, t0)) = backend_system_gc {
        // zs s7/jf (merge of origin/dev, 2026-10-18): on ZGC the `Runtime.gc()`
        // native writes this request's `jdk.SystemGC` itself, on the caller
        // with its stack (`jfr_vm_more::force_gc_as_system_gc`, asked through
        // `zgc_jfr_system_gc_on_caller`, the same test); writing it here too
        // would make two rows. Elsewhere, and under
        // `CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER=0`, this row is the one.
        if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording()
            && super::gc_events::zgc_jfr_system_gc_on_caller(shared).is_none()
        {
            // gci i19/ob (`gci-i12ge-...` residual r1): HotSpot's `JVM_GC`
            // commits `jdk.SystemGC` on the calling Java thread.
            // gci i20/ob: the resolution (Java `Thread.name`, tid off the
            // `Thread` object for the primordial `main`) is
            // `gc_events::jfr_requester_of`, shared with the collection rows.
            let who = super::gc_events::jfr_requester_of(shared, thread.thread_id);
            let caller = who.as_ref().map(|w| w.as_caller());
            cratonvm_native_builtins_jfr::jfr_vm_more::system_gc_by(
                shared.vm_identity,
                caller,
                invoked_concurrent,
                start_ns,
                u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX),
                jfr_stack,
            );
        }
    }
    // Run pending finalizers.
    //
    // FORCED, for the same reason the cleaner drain below is: deferring here is
    // not a deferral. An application that asks for collection from inside a
    // compiled loop never reaches a call where `is_jit_thread_set()` is false,
    // so the guarded variant defers forever and every finalizable object in the
    // process becomes immortal. See `run_finalizers_forced`.
    run_finalizers_forced(shared, thread);
    // Run pending Cleaner actions (NEW-17). These were submitted to
    // shared.mem.cleaner_thread by process_references_after_gc.
    //
    // FORCED variant: this is the last-ditch reclaim behind
    // `Bits.reserveMemory` (and `System.gc()`). Deferring here is not a
    // deferral at all -- the caller throws `OutOfMemoryError: Direct buffer
    // memory` the moment we return without freeing anything.
    run_cleaner_actions_forced(shared, thread);
    // gen r4w3/obs2: queued GC notifications, forced for the same reason as the
    // two drains above (see `run_gc_notifications`). gc-common w4-a: on the
    // reference-delivery thread when it serves (see
    // `gc_notifications_go_to_delivery_thread`). zt t20/x16: on ZGC a
    // `--compatible` door hands them over too, lock or no lock
    // ([`zgc_collection_door_hands_off_gc_notifications`]).
    if !gc_notifications_go_to_delivery_thread(shared, thread)
        && !zgc_collection_door_hands_off_gc_notifications(shared, thread)
    {
        run_gc_notifications(shared, thread, true);
    }
}

/// `CRATONVM_ZGC_SYSTEM_GC_NOTIFICATIONS_OFF_THREAD` (zt t20/x16, defect D10 of
/// `docs/feature-designs/zgc-architecture.md`, page
/// `zgc-zt13x10-review-residuals-CLOSED-20261019.md` item 24; default ON, `=0` kill).
///
/// HotSpot 25's `System.gc()` (`JVM_GC`) delivers no GC notification itself:
/// `GCNotifier::pushNotification` queues it and the `Notification Thread`
/// sends it, after the call, for every cause. A `--real-jdk` VM does the
/// same here (every door hands off, [`GcNotificationDelivery::OffMutator`]),
/// and `Zt13X10MissedDrain` was 20/20 clean there. A `--compatible` VM kept
/// one door of its own: a lock-free `System.gc()` caller DELIVERED its
/// notifications inline, through the forced, bounded drain
/// (`run_gc_notifications(.., true)`), and that inline drainer is the only
/// path the two modes do not share. Rows `x12_N1c_06/13/17` and
/// `x12_N1c_census` (3 of 20 plus the census) show its cost: the caller
/// became the queue's drainer while an allocating peer collected, ran the
/// cold delivery path under that peer's pauses (one `System.gc()` took
/// 0.8-4 s: `sysgc_max_ms`, and the census row's `-Xlog` shows 70 peer
/// collections inside the first call), and a record still queued when its
/// drain point stopped (a busy-class-lock hold, a refused build, a re-drain
/// chain at its bound) had no later drain point: the peer stops when
/// `System.gc()` returns and the program sleeps (`late=1`, the notification
/// past the probe's 2-s wait). With the switch a ZGC collection door on a
/// `--compatible` VM under the default policy hands its notifications to
/// the delivery thread whether or not a lock is held, as the jdk-only arm
/// and the metadata door do ([`hand_gc_notifications_to_delivery_thread`],
/// marked for the `WhenLockHeld` batch): the listener runs on the
/// `Notification Thread`, under HotSpot's name, and `System.gc()` returns
/// without running it. `false` -- the door delivers inline, as before --
/// off ZGC, under the inline policy (`CRATONVM_FINALIZER_THREAD=0`), with
/// nothing queued, or when no delivery thread could be started. The
/// allocation doors keep the w7-d lock rule (only the explicit doors move).
/// `=0`: the inline bounded drain, as before.
///
/// t1 wave 2 lane Z: unreachable today on every backend, for the reason given
/// at [`zgc_undeliverable_notifications_off_thread_enabled`] (the rule is
/// `OffMutator` in both modes, so `gc_notifications_go_to_delivery_thread`
/// has already handed the records off before this is asked).
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_collection_door_hands_off_gc_notifications(shared: &SharedVm, thread: &JvmThread) -> bool {
    if !super::gc_events::gc_notifications_pending(shared) {
        return false;
    }
    let zgc = shared.mem.heap.is_zgc();
    if !zgc_undeliverable_door_hands_off_for(
        zgc,
        zgc && zgc_system_gc_notifications_off_thread_enabled(),
        gc_notification_delivery(shared),
    ) {
        return false;
    }
    hand_gc_notifications_to_delivery_thread(shared, thread)
}

/// zt t20/x16: the switch of [`zgc_collection_door_hands_off_gc_notifications`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_system_gc_notifications_off_thread_enabled() -> bool {
    true
}

/// The full collection a `-XX:MaxMetaspaceSize` refusal needs before it throws
/// `OutOfMemoryError: Metaspace` -- [`force_gc_from_native`] WITHOUT its three
/// drains (finalizers, cleaner actions, GC notifications). gc-common w7-g.
///
/// `ClassLoader.defineClass` reaches the metaspace check holding the
/// class-loading lock of the loader it defines into (`loadClass` is
/// `synchronized (getClassLoadingLock(name))`). The `System.gc()` drains there
/// would run application code in the middle of that define, on that thread,
/// holding that lock: a `NotificationListener` inline under `--compatible`,
/// every queued cleaner `Runnable` inline, and, under the default finalizer
/// policy, a bounded wait (the finalizer timeout, 2 s by default) for the
/// delivery thread -- twice, since the refusal collects twice. HotSpot's
/// metadata-threshold collection runs none of that on the defining thread.
/// What the refusal needs is only what the collection itself does: class
/// unloading gives a dead loader's charge back. Queued finalizers, cleaner
/// actions and notifications are not lost: the pause raises the ref-work hint
/// (`run_collection_pause`) and the next door that may run Java drains them.
///
/// Coalesces with a sibling `System.gc()` exactly as `System.gc()` does.
///
/// Not an explicit GC (HotSpot's `GCCause::_metadata_GC_threshold`): it
/// ignores `-XX:+DisableExplicitGC` / `-XX:+ExplicitGCInvokesConcurrent`
/// (`VmConfig::explicit_gc`) and always collects fully. gc-common w8-g.
///
/// gci i13/gp (`gci-i12ge-g1-event-model-review-findings-RETIRED-UNFIXED-20261018` item 5,
/// opt-in `CRATONVM_G1_METADATA_CONCURRENT_START`): on G1 it first tries
/// HotSpot's `start_concurrent_gc_for_metadata_allocation`
/// ([`g1_metadata_concurrent_start`]); the full collection runs only when
/// that did not start a cycle (one was already running: HotSpot's
/// `force_concurrent_start_if_outside_cycle` declines and the full GC
/// follows). The define re-checks after each call (`vm_exec.rs`), so a define
/// that still does not fit reaches the second call, which finds the cycle the
/// first one opened running and collects fully -- HotSpot's `G1 full GC for
/// Metaspace`.
///
/// gci i47w/nt47: whichever collection ran, the notifications it queued are
/// handed to the delivery thread before returning
/// ([`metadata_door_hands_off_gc_notifications`]) -- a hand-off, not a
/// drain: nothing runs or waits on this thread.
pub fn force_gc_for_class_metadata(shared: &SharedVm, thread: &mut JvmThread) {
    if !g1_metadata_concurrent_start(shared, thread) {
        force_full_collection(shared, thread, GcDoor::MetadataThreshold);
    }
    let _ = metadata_door_hands_off_gc_notifications(shared, thread);
}

/// gci i47w/nt47
/// (`docs/feature-designs/concurrent-gc-maturation.md`):
/// the metadata door's GC-notification hand-off. `true` when the delivery
/// thread took the queued notifications.
///
/// [`force_gc_for_class_metadata`] runs no drain (gc-common w7-g) and left its
/// notifications to "the next door that may run Java". Under `--compatible`
/// that door delivers inline only when it is reached outside a JIT borrow
/// (`drain_ref_work_queued_elsewhere_cold`), and a define loop running in
/// compiled code -- `GciGp13G1PauseShapeProbe meta`, 60000 `defineClass`
/// calls from an OSR-compiled loop, then `Thread.sleep` -- reaches none:
/// `notif_queued=168 notif_delivered=0`, `notif_first_wait_us=-` (no drain ever
/// began). `--real-jdk` delivered only because its delivery thread drains the
/// queue on every batch, whatever woke it. HotSpot delivers every one on its
/// `Notification Thread`.
///
/// This door can never deliver inline, so under the `--compatible` rule
/// ([`GcNotificationDelivery::WhenLockHeld`]) it hands off whether or not the
/// defining thread holds a lock ([`drainless_door_hands_off_gc_notifications`]);
/// a `--real-jdk` VM ([`GcNotificationDelivery::OffMutator`]) hands off as at
/// every door; the `CRATONVM_FINALIZER_THREAD=0` arm hands nothing off (the
/// notifications wait for the next inline drain point, as before). Heap-free
/// (a mark, a counter bump and a condvar notify, or a first-use spawn of a
/// STARTING thread) and wait-free, as `forced_door_hands_off_gc_notifications`.
/// When no delivery thread can be started the notifications stay queued, the
/// ref-work hint raised, exactly as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn metadata_door_hands_off_gc_notifications(shared: &SharedVm, thread: &JvmThread) -> bool {
    let rule = gc_notification_delivery(shared);
    if !drainless_door_hands_off_gc_notifications(rule)
        || !super::gc_events::gc_notifications_pending(shared)
    {
        return false;
    }
    match rule {
        GcNotificationDelivery::WhenLockHeld => {
            hand_gc_notifications_to_delivery_thread(shared, thread)
        }
        _ => gc_notifications_go_to_delivery_thread(shared, thread),
    }
}

/// gci i47w/nt47: does a door that may never run Java (the metadata door)
/// hand its queued GC notifications to the delivery thread under `rule`?
/// Every rule but the inline bisection arm. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn drainless_door_hands_off_gc_notifications(rule: GcNotificationDelivery) -> bool {
    match rule {
        GcNotificationDelivery::Inline => false,
        GcNotificationDelivery::WhenLockHeld | GcNotificationDelivery::OffMutator => true,
    }
}

/// How many times [`g1_metadata_concurrent_start`] tries to initiate its
/// young pause before it leaves the request to the full collection (the
/// bound `force_full_collection` uses).
pub(in crate::runtime::interpreter::gc_and_alloc) const METADATA_CONCURRENT_START_ATTEMPTS: u32 = 4;

/// gci i13/gp: G1's answer to a `-XX:MaxMetaspaceSize` refusal under the
/// opt-in `CRATONVM_G1_METADATA_CONCURRENT_START` -- one young pause on the
/// `Metadata GC Threshold` door (labelled `Pause Young (Concurrent Start)`,
/// `gc_events::g1_metadata_pause_is_concurrent_start`) and a marking cycle
/// opened right after it ([`g1_concurrent_mark_cycle`], this VM's initial
/// mark), NOT waited for: the cycle's remark unloads the dead loaders' classes
/// (`g1_remark_process_references`) whenever a later pause closes it. `true`
/// when a cycle is marking on return (this call's, or one another thread
/// opened meanwhile); `false` -- the caller collects fully -- off G1, with the
/// switch off, when a cycle was already marking at entry, or when every
/// attempt lost its STW race or the initial mark lost its race.
///
/// The young pause counts as a `System.gc()`-door collection for coalescing
/// (`run_collection_pause` bumps the count for this door), so a sibling
/// `System.gc()` that took part in it skips its own young pause; it still
/// runs its full cycle and compaction (`force_full_collection`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_metadata_concurrent_start(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    if !shared.mem.heap.is_g1()
        || shared.mem.heap.g1_is_marking_active()
        || !super::gc_events::g1_metadata_concurrent_start_enabled()
    {
        return false;
    }
    let mut collected = false;
    for _attempt in 0..METADATA_CONCURRENT_START_ATTEMPTS {
        // As `force_full_collection`'s loop: retire the TLAB and drain this
        // thread's SATB buffer before initiating.
        flush_tlab_allocation_batch(thread, shared);
        thread.tlab.retire();
        shared.mem.heap.flush_thread_satb();
        if run_collection_pause(shared, thread, GcDoor::MetadataThreshold).is_some() {
            collected = true;
            break;
        }
    }
    if !collected {
        return false;
    }
    if !shared.mem.heap.g1_is_marking_active() {
        g1_concurrent_mark_cycle(shared, thread);
    }
    let marking = shared.mem.heap.g1_is_marking_active();
    if marking {
        cratonvm_types::gc_entry_census::note_from_native();
    }
    marking
}

/// How long `jcmd GC.run` waits for its collection
/// ([`request_diagnostic_command_gc`]). HotSpot's attach listener runs the VM
/// operation itself and returns when it completed; here the collection runs on
/// the next mutator that allocates (an idle VM runs none), so the wait is
/// bounded and the command says when it gave up. gci i9/dc: only the fallback
/// now -- the attach commands run their collection on a thread attached for it
/// first ([`run_dcmd_full_gc_on_attach_thread`]). A unit VM has no mutator to
/// take the request, so its tests wait a quarter of a second.
pub(crate) const DCMD_GC_RUN_WAIT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(250)
} else {
    std::time::Duration::from_secs(10)
};

/// `jcmd GC.run`, the requesting half, for a thread that is not a mutator (the
/// attach listener). gci i2/oo, page
/// `gci-i1pp-jcmd-gc-run-is-a-deferred-young-collection-reported-as-completed-FIXED-20261012`.
///
/// It used to latch `gc_requested` and report `GC triggered and completed` at
/// once: the latch ran an ordinary YOUNG collection labelled `Allocation
/// Failure` at the next interpreted allocation, never on a compiled loop.
/// Now it raises `AllocLadderState::dcmd_gc_requested` (with `gc_requested`, the
/// latch `maybe_gc` already polls with one load), which the next `maybe_gc` or
/// JIT-guarded TLAB refill turns into [`run_diagnostic_command_gc`], and waits
/// up to `wait` for a full collection to complete. `true` when one completed
/// after the request (the requested one, or a `System.gc()` that completed
/// meanwhile: HotSpot's `VM_GC_Operation::skip_operation` coalesces those
/// too); `false` when the wait ran out, the request then stays pending.
pub(crate) fn request_diagnostic_command_gc(shared: &SharedVm, wait: std::time::Duration) -> bool {
    use std::sync::atomic::Ordering;
    let baseline = system_gc_collections(shared).load(Ordering::Acquire);
    shared
        .mem
        .alloc_ladder
        .dcmd_gc_requested
        .store(true, Ordering::Release);
    shared.mem.gc_requested.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + wait;
    loop {
        if system_gc_satisfied(baseline, system_gc_collections(shared).load(Ordering::Acquire)) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Take a pending `jcmd GC.run` request: `true` exactly once per request.
/// One relaxed load when none is pending.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn take_diagnostic_command_gc_request(shared: &SharedVm) -> bool {
    use std::sync::atomic::Ordering;
    let latch = &shared.mem.alloc_ladder.dcmd_gc_requested;
    latch.load(Ordering::Relaxed) && latch.swap(false, Ordering::AcqRel)
}

/// `jcmd GC.run`, the collecting half, on the mutator that took the request:
/// HotSpot's `_dcmd_gc_run` -- a full collection (`System.gc()`'s door, with
/// its STW retry and coalescing, the G1 full cycle), cause `Diagnostic
/// Command`, not gated by `-XX:+DisableExplicitGC`, no `jdk.SystemGC` event,
/// and none of `System.gc()`'s forced drains (the caller's own epilogue runs
/// the ordinary ones where it may run Java).
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_diagnostic_command_gc(shared: &SharedVm, thread: &mut JvmThread) {
    force_full_collection(shared, thread, GcDoor::DiagnosticCommand);
}

/// Which full collection an attach command runs before it answers (gci i9/dc):
/// `jcmd GC.run` or the collection a live-only heap walk runs first. Each is
/// HotSpot's VM operation of the same name, with its own `GCCause`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DcmdFullGc {
    /// `jcmd GC.run`: `_dcmd_gc_run`, `Diagnostic Command`.
    GcRun,
    /// `GC.class_histogram` without `-all`, `jmap -histo:live`:
    /// `VM_GC_HeapInspection`, `Heap Inspection Initiated GC`.
    HeapInspection,
    /// `GC.heap_dump` without `-all`, `jmap -dump:live`: `VM_HeapDumper`,
    /// `Heap Dump Initiated GC`.
    HeapDump,
}

impl DcmdFullGc {
    pub(in crate::runtime::interpreter::gc_and_alloc) fn door(self) -> GcDoor {
        match self {
            DcmdFullGc::GcRun => GcDoor::DiagnosticCommand,
            DcmdFullGc::HeapInspection => GcDoor::HeapInspection,
            DcmdFullGc::HeapDump => GcDoor::HeapDump,
        }
    }
}



/// An attach command's full collection, run NOW from the requesting
/// (non-mutator) thread, as HotSpot's attach listener has its VM thread run
/// the VM operation: gci i9/dc, page
/// `gci-i8rv-dcmd-live-walk-on-an-idle-vm-FIXED-20261017`.
///
/// The latch path ([`request_diagnostic_command_gc`]) collects only when a
/// mutator next reaches `maybe_gc` or a guarded TLAB refill: on a VM whose
/// Java threads are all blocked (an idle server, exactly when an operator runs
/// `jmap -histo:live`) it waited its 10 s out, the live-only walk then counted
/// the dead objects, and the request stayed armed for a full collection
/// HotSpot never runs. Here a new OS thread attaches as the daemon "Attach
/// Listener" ([`crate::native::jni::run_attached_service`], the attach `load`
/// / `properties` operations' carrier), takes the `System.gc()` door's full
/// collection ([`force_full_collection`]: STW retry, coalescing, the G1 full
/// cycle) under `kind`'s cause, delivers the GC notifications it queued (the
/// door GC.run's latch epilogue uses; finalizers and cleaner actions stay with
/// the ref-work hint for the next door, as HotSpot leaves them to the
/// Finalizer / Cleaner threads), detaches, and the caller waits for it to end.
///
/// `None` when this path does not apply and the caller takes the latch path:
/// the switch is off; `shared` is not a registered VM (a unit VM: no
/// `Arc` to attach to); or the CALLING thread is itself a VM thread (a
/// foreign attachment, or an OS thread a registry entry claims) -- the pause
/// would wait for the caller, which waits for the pause; or the platform
/// cannot say (no OS-tid backend); or no thread could be started.
/// `Some(true)` when a full collection completed after the call began (this
/// one or a coalesced sibling's), `Some(false)` when the attached job did not
/// run.
pub(crate) fn run_dcmd_full_gc_on_attach_thread(
    shared: &SharedVm,
    kind: DcmdFullGc,
) -> Option<bool> {
    if crate::native::jni::is_foreign_attached()
        || shared.threads.thread_registry.current_os_tid_claimed() != Some(false)
    {
        return None;
    }
    let vm = crate::native::jni::vm_for_identity(shared.vm_identity)?;
    let door = kind.door();
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel::<u64>(1);
    let (job_tx, job_rx) = std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
    let (out_tx, out_rx) = std::sync::mpsc::channel::<bool>();
    let job: crate::native::jni::AttachedServiceJob = Box::new(move |shared, thread| {
        let baseline = system_gc_collections(shared).load(std::sync::atomic::Ordering::Acquire);
        force_full_collection(shared, thread, door);
        if !gc_notifications_go_to_delivery_thread(shared, thread) {
            run_gc_notifications(shared, thread, false);
        }
        let now = system_gc_collections(shared).load(std::sync::atomic::Ordering::Acquire);
        let _ = out_tx.send(system_gc_satisfied(baseline, now));
    });
    if job_tx.send(job).is_err() {
        return None;
    }
    drop(job_tx);
    let handle = std::thread::Builder::new()
        .name("Attach Listener".to_string())
        .spawn(move || {
            crate::native::jni::run_attached_service(vm, "Attach Listener", started_tx, job_rx)
        })
        .ok()?;
    let _ = handle.join();
    drop(started_rx);
    Some(out_rx.recv().unwrap_or(false))
}

/// zt t7/tg (`zgc-zt1tp-director-parity-residuals-FIXED-20261019.md` item 3): start
/// this VM's ZGC director tick -- HotSpot's `ZDirector` thread for the rules
/// that need a clock rather than an allocation to fire: the `Timer` rule of
/// `-XX:ZCollectionInterval=<seconds>` (and, under the deciding director
/// `CRATONVM_ZGC_RATE_TRIGGER=1`, the idle proactive tick,
/// `CRATONVM_ZGC_DIRECTOR_IDLE_TICK`, default ON since zt t20).
///
/// Nothing starts on a default run (`ZgcRealHeap::director_tick_wanted` is
/// false without the flag), off ZGC, or when the thread cannot be spawned.
/// The thread holds only a `Weak` to the VM between ticks, so it never keeps
/// the VM alive and ends at the first tick after the VM is gone. It is not a
/// Java thread: it sleeps on the heap's clock and, when a rule says yes, runs
/// the collection on a thread attached for that one collection
/// ([`run_zgc_tick_collection`]). `true` when the thread started.
pub(crate) fn start_zgc_director_tick(shared: &std::sync::Arc<SharedVm>) -> bool {
    if !shared.mem.heap.is_zgc() {
        return false;
    }
    let heap = &shared.mem.heap;
    if !heap.zgc_director_tick_wanted() {
        return false;
    }
    let weak = std::sync::Arc::downgrade(shared);
    std::thread::Builder::new()
        .name("ZDirector".to_string())
        .spawn(move || zgc_director_tick_loop(weak))
        .is_ok()
}

/// zt t31/x10 (`CRATONVM_GC_FULL_GC_HEAP_DUMPS`, default ON, `=0` kill;
/// page `zgc-zt13x10-review-residuals-*` item 30h): start this VM's G1
/// periodic-collection service, HotSpot 25's `G1PeriodicGCTask` on its
/// `G1 Service` thread. Each turn, when `G1PeriodicGCInterval` (a
/// `setVMOption` write first, then the command line) is not 0, it checks
/// HotSpot's three conditions -- no marking cycle in progress, at least the
/// interval since the last collection pause, the one-minute load average
/// at or below a non-zero `G1PeriodicGCSystemLoadThreshold` -- and then runs
/// the periodic collection ([`run_g1_periodic_collection`]); it sleeps the
/// interval (one second while it is 0) and looks again.
///
/// G1 only. HotSpot's thread always exists; here it starts with the first
/// non-zero interval (VM creation, or [`note_g1_periodic_interval_written`]),
/// so a default run has no extra thread. It is not a Java thread and holds
/// only a `Weak` between turns; [`stop_g1_periodic_gc_service`] ends it.
/// `true` when the thread started.
pub(crate) fn start_g1_periodic_gc_service(shared: &std::sync::Arc<SharedVm>) -> bool {
    if !shared.mem.heap.is_g1()
        || !crate::config::hotspot_flags::full_gc_heap_dumps_enabled()
        || !shared.mem.g1_periodic.claim_start()
    {
        return false;
    }
    let weak = std::sync::Arc::downgrade(shared);
    std::thread::Builder::new()
        .name("G1 Service".to_string())
        .spawn(move || g1_periodic_gc_loop(weak))
        .is_ok()
}

/// zt t31/x10: VM creation's half of [`start_g1_periodic_gc_service`]: only
/// when the command line sets a non-zero `-XX:G1PeriodicGCInterval`.
pub(crate) fn start_g1_periodic_gc_service_at_boot(shared: &std::sync::Arc<SharedVm>) -> bool {
    crate::config::hotspot_flags::g1_periodic_gc_interval_arg(&shared.config) != 0
        && start_g1_periodic_gc_service(shared)
}

/// zt t31/x10: a `setVMOption("G1PeriodicGCInterval", ..)` wrote
/// `interval_ms`; a non-zero one starts the service if it is not running
/// (G1PeriodicGCInterval is manageable: HotSpot's task reads it each turn).
pub(crate) fn note_g1_periodic_interval_written(shared: &SharedVm, interval_ms: u64) {
    if interval_ms == 0 {
        return;
    }
    if let Some(owned) = shared.try_get_arc() {
        let _ = start_g1_periodic_gc_service(&owned);
    }
}

/// zt t31/x10: stop this VM's G1 periodic service for good, as HotSpot's
/// `before_exit` stops the `G1 Service` thread: no periodic collection
/// starts after it. For the launcher's shutdown arms, beside
/// [`stop_zgc_director_tick`]. A no-op without the service.
pub fn stop_g1_periodic_gc_service(shared: &SharedVm) {
    shared.mem.g1_periodic.stop();
}

/// zt t31/x10: `G1PeriodicGCInterval` in force (ms): a `setVMOption` write,
/// else the command line's.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_periodic_interval_ms(shared: &SharedVm) -> u64 {
    shared
        .mem
        .managed_flags
        .number_text("G1PeriodicGCInterval")
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or_else(|| crate::config::hotspot_flags::g1_periodic_gc_interval_arg(&shared.config))
}

/// zt t31/x10: `G1PeriodicGCSystemLoadThreshold` in force: a `setVMOption`
/// write, else the command line's.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_periodic_load_threshold(shared: &SharedVm) -> f64 {
    shared
        .mem
        .managed_flags
        .number_text("G1PeriodicGCSystemLoadThreshold")
        .and_then(|text| text.parse::<f64>().ok())
        .unwrap_or_else(|| {
            crate::config::hotspot_flags::g1_periodic_gc_load_threshold_arg(&shared.config)
        })
}

/// zt t31/x10: HotSpot's `log_debug(gc, periodic)` lines.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_periodic_debug(message: &str) {
    use crate::runtime::unified_logging::{is_unified_logging_enabled, log_unified, LogLevel, LogTag};
    if is_unified_logging_enabled(&[LogTag::GcPeriodic], LogLevel::Debug) {
        log_unified(&[LogTag::GcPeriodic], LogLevel::Debug, message);
    }
}

/// zt t31/x10: the one-, five- and fifteen-minute load averages
/// (`os::loadavg(.., 3)`), `None` where the platform has none (HotSpot's
/// `-1`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn load_averages_os() -> Option<[f64; 3]> {
    #[cfg(unix)]
    {
        let mut load = [0.0f64; 3];
        // SAFETY: `getloadavg` writes at most `nelem` (3) doubles into the
        // buffer it is given, which holds three.
        let got = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
        (got == 3).then_some(load)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// zt t31/x10: the one-minute load average, `None` where the platform has
/// none (the periodic check then skips, as HotSpot's on `-1`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn one_minute_load_average() -> Option<f64> {
    load_averages_os().map(|l| l[0])
}

/// zt t31/x10 (x10 page item 2): ZGC's `Load:` figures, zeros where the
/// platform has none (`ZStatLoad::print` prints its zero-initialised array
/// then).
pub(crate) fn load_averages() -> [f64; 3] {
    load_averages_os().unwrap_or([0.0; 3])
}

/// zt t31/x10: `ZCPU::count()` (`os::processor_count`), the CPUs ZGC's
/// `Load:` percentages divide by -- the `gc,init` block's `total`.
pub(crate) fn zgc_stat_cpu_count(_shared: &SharedVm) -> usize {
    let online = std::thread::available_parallelism().map_or(1, |n| n.get());
    crate::vm::vm_init::configured_cpu_count()
        .unwrap_or(online)
        .max(online)
}

/// zt t31/x10: `G1PeriodicGCTask::should_start_periodic_gc`, with its debug
/// lines. `since_start` measures the time since the last collection before
/// the first (HotSpot's clock starts at heap initialisation).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_periodic_should_start(shared: &SharedVm, interval_ms: u64, service_start: std::time::Instant) -> bool {
    if shared.mem.heap.g1_is_marking_active() {
        g1_periodic_debug("Concurrent cycle in progress. Skipping.");
        return false;
    }
    let last = match &shared.mem.heap {
        _ => match shared.mem.heap.g1_last_pause_end() {
            Some(t) => Some(t),
            None => return false,
        },
    };
    // Truncation: saturates past 584 million years.
    let since_ms =
        u64::try_from(last.unwrap_or(service_start).elapsed().as_millis()).unwrap_or(u64::MAX);
    if since_ms < interval_ms {
        g1_periodic_debug(&format!(
            "Last GC occurred {since_ms}ms before which is below threshold {interval_ms}ms. Skipping."
        ));
        return false;
    }
    let threshold = g1_periodic_load_threshold(shared);
    if threshold > 0.0 {
        let load = one_minute_load_average();
        if load.is_none_or(|l| l > threshold) {
            g1_periodic_debug(&format!(
                "Load {:.2} is higher than threshold {threshold:.2}. Skipping.",
                load.unwrap_or(-1.0)
            ));
            return false;
        }
    }
    true
}

/// zt t31/x10: the service's loop ([`start_g1_periodic_gc_service`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_periodic_gc_loop(weak: std::sync::Weak<SharedVm>) {
    let service_start = std::time::Instant::now();
    loop {
        let Some(shared) = weak.upgrade() else {
            return;
        };
        if shared.mem.g1_periodic.is_stopped() {
            return;
        }
        let interval = g1_periodic_interval_ms(&shared);
        if interval != 0 {
            g1_periodic_debug("Checking for periodic GC.");
            if g1_periodic_should_start(&shared, interval, service_start)
                && !run_g1_periodic_collection(&shared)
            {
                g1_periodic_debug("GC request denied. Skipping.");
            }
        }
        drop(shared);
        // HotSpot's `schedule(G1PeriodicGCInterval == 0 ? 1000 :
        // G1PeriodicGCInterval)`.
        std::thread::sleep(std::time::Duration::from_millis(if interval == 0 {
            1000
        } else {
            interval
        }));
    }
}

/// zt t31/x10: run one periodic collection on a thread attached for it, as
/// the ZGC director's tick runs its own ([`run_zgc_tick_collection`]): the
/// service thread is no mutator. The attachment is kept out of the JMM's
/// thread counters (HotSpot's `G1 Service` is a `NonJavaThread`). `true`
/// when a collection ran.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_g1_periodic_collection(shared: &std::sync::Arc<SharedVm>) -> bool {
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel::<u64>(1);
    let (job_tx, job_rx) = std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
    let (out_tx, out_rx) = std::sync::mpsc::channel::<bool>();
    let job: crate::native::jni::AttachedServiceJob = Box::new(move |shared, thread| {
        let _ = out_tx.send(g1_periodic_collection(shared, thread));
    });
    if job_tx.send(job).is_err() {
        return false;
    }
    drop(job_tx);
    let owned = std::sync::Arc::clone(shared);
    let Ok(handle) = std::thread::Builder::new()
        .name("G1 Service".to_string())
        .spawn(move || {
            crate::native::jni::run_attached_service_as(owned, "G1 Service", false, started_tx, job_rx)
        })
    else {
        return false;
    };
    let _ = handle.join();
    drop(started_rx);
    out_rx.recv().unwrap_or(false)
}

/// zt t31/x10: HotSpot 25's `try_collect(GCCause::_g1_periodic_collection)`,
/// on the attached thread. Under the default `-XX:+G1PeriodicGCInvokesConcurrent`:
/// one young pause on [`GcDoor::G1PeriodicCollection`] -- `Concurrent Start`
/// when no cycle marks and no mixed phase holds the start back (its label
/// then reads `Prepare Mixed` / `Mixed`, as HotSpot's) -- and the marking
/// cycle that start opens, driven here to its remark and cleanup as
/// HotSpot's concurrent mark thread drives it (nobody else would on an idle
/// VM). Under `-XX:-G1PeriodicGCInvokesConcurrent`: the full collection,
/// `Pause Full (G1 Periodic Collection)` ([`GcDoor::G1PeriodicFull`]). GC
/// notifications are delivered as the attach commands deliver them;
/// finalizers stay with the reference-delivery hint. `true` when a pause ran.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_periodic_collection(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    const PERIODIC_GC_ATTEMPTS: u32 = 3;
    if shared.mem.g1_periodic.is_stopped() {
        return false;
    }
    if !shared.mem.heap.is_g1() {
        return false;
    }
    let concurrent = crate::config::hotspot_flags::jvm_arg_bool(
        &shared.config,
        "G1PeriodicGCInvokesConcurrent",
        true,
    );
    let collected = if concurrent {
        let mut collected = false;
        for _attempt in 0..PERIODIC_GC_ATTEMPTS {
            // As `maybe_gc` before its pause (a fresh attachment has neither
            // in use, but the door's contract is the initiator's).
            flush_tlab_allocation_batch(thread, shared);
            thread.tlab.retire();
            shared.mem.heap.flush_thread_satb();
            let idle = !shared.mem.heap.g1_is_marking_active();
            let mixed = shared.mem.heap.g1_mixed_collection_count().unwrap_or(0);
            let prepare = shared.mem.heap.g1_prepare_mixed_pause_count().unwrap_or(0);
            if run_collection_pause(shared, thread, GcDoor::G1PeriodicCollection).is_none() {
                continue;
            }
            collected = true;
            // The pause was the concurrent start (no mixed phase held it
            // back): open its cycle, as `force_concurrent_start_if_outside_cycle`.
            if idle
                && mixed == shared.mem.heap.g1_mixed_collection_count().unwrap_or(0)
                && prepare == shared.mem.heap.g1_prepare_mixed_pause_count().unwrap_or(0)
                && !shared.mem.heap.g1_is_marking_active()
            {
                g1_concurrent_mark_cycle(shared, thread);
                g1_periodic_finish_cycle(shared, thread);
            }
            break;
        }
        collected
    } else {
        let before = shared
            .mem
            .gc_cycle_count
            .load(std::sync::atomic::Ordering::Relaxed);
        force_full_collection(shared, thread, GcDoor::G1PeriodicFull);
        shared
            .mem
            .gc_cycle_count
            .load(std::sync::atomic::Ordering::Relaxed)
            != before
    };
    if collected && !gc_notifications_go_to_delivery_thread(shared, thread) {
        run_gc_notifications(shared, thread, false);
    }
    collected
}

/// zt t31/x10: how long the periodic service drives its cycle before it
/// leaves the rest to the mutators' polls (a wedged marker must not hold the
/// service).
pub(in crate::runtime::interpreter::gc_and_alloc) const G1_PERIODIC_CYCLE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// zt t31/x10: drive the cycle a periodic collection opened to its remark
/// and cleanup ([`g1_final_remark_cleanup`] once the marker has quiesced),
/// taking part in every pause another thread asks for meanwhile; bounded by
/// [`G1_PERIODIC_CYCLE_BUDGET`] and the service's stop.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_periodic_finish_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    let deadline = std::time::Instant::now() + G1_PERIODIC_CYCLE_BUDGET;
    let mut backoff = LastDitchBackoff::default();
    while shared.mem.heap.g1_is_marking_active()
        && !shared.mem.g1_periodic.is_stopped()
        && std::time::Instant::now() < deadline
    {
        if shared.mem.heap.g1_concurrent_mark_finished() && g1_final_remark_cleanup(shared, thread) {
            return;
        }
        safepoint_check(shared, thread);
        backoff.snooze();
    }
}

/// `CRATONVM_ZGC_TICK_STOP_WAITS` (zt t11/tg4, `t11-x8-report.md` C12 / T1;
/// default ON, `=0` kill). The director stop ([`stop_zgc_director_tick`])
/// turns back every tick collection that has not reached its last stop
/// check, but one past it ran its pause after the stop, so its `GC(n)` lines
/// could print after the `gc,exit` summary; HotSpot's director stop waits
/// for its running cycle. With the switch the launcher's main-thread exit
/// arms (main returned, the `-version` boot) wait for that pause
/// ([`wait_for_zgc_tick_pause`]) inside a blocked region, before the
/// summary. The `System.exit` arm does not wait (its thread's state is not
/// known there; the page records it). `=0`: no wait.
pub fn zgc_tick_stop_waits_enabled() -> bool {
    true
}

/// zt t11/tg4: whether a ZGC director tick collection is past its last stop
/// check with its pause not ended (`false` off ZGC). Read after
/// [`stop_zgc_director_tick`]; the launcher enters a blocked region and
/// calls [`wait_for_zgc_tick_pause`] only when this says so.
pub fn zgc_tick_pause_in_flight(shared: &SharedVm) -> bool {
    match &shared.mem.heap {
        _ => shared.mem.heap.zgc_tick_pause_in_flight(),
    }
}

/// zt t11/tg4: wait, at most [`ZGC_TICK_STOP_WAIT_LIMIT`], for the tick
/// pause of [`zgc_tick_pause_in_flight`] to end. The caller must not be a
/// counted mutator outside a blocked region (the pause waits for every such
/// thread): the launcher calls it inside `begin_main_thread_blocking_region`.
/// `true` when no tick pause is in flight on return.
pub fn wait_for_zgc_tick_pause(shared: &SharedVm) -> bool {
    let deadline = std::time::Instant::now() + ZGC_TICK_STOP_WAIT_LIMIT;
    while zgc_tick_pause_in_flight(shared) {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    true
}

/// zt t11/tg4: the bound of [`wait_for_zgc_tick_pause`]: a stuck pause must
/// not hold the exit forever.
pub(in crate::runtime::interpreter::gc_and_alloc) const ZGC_TICK_STOP_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// zt t9/tg2 (`zgc-zt8x5-wave-t7-review-residuals-CLOSED-20261019.md` item 9): stop
/// this VM's ZGC director tick for good, as HotSpot's `before_exit` stops its
/// director thread after the shutdown hooks: no `Timer` (or idle `Proactive`)
/// cycle starts after it, so none can print its `-Xlog:gc` lines after the
/// `gc,exit` summary or ask for a pause while the exiting thread runs the
/// launcher's exit work. A collection already running finishes; the tick
/// thread ends at its next look at the clock. A no-op off ZGC and without a
/// tick. For the launcher's shutdown arms, before the `gc,exit` summary.
pub fn stop_zgc_director_tick(shared: &SharedVm) {
    shared.mem.heap.zgc_stop_director_tick();
}

/// zt t7/tg: the director tick's loop. Each turn upgrades the VM, asks the
/// heap's clock (`ZgcRealHeap::director_tick`), and then sleeps (holding no
/// strong reference), collects, or ends.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_director_tick_loop(weak: std::sync::Weak<SharedVm>) {
    use cratonvm_gc::zgc::timer::{ZTick, TICK_MAX_WAIT, TICK_MIN_WAIT};
    loop {
        let Some(shared) = weak.upgrade() else {
            return;
        };
        let tick = match &shared.mem.heap {
            _ => shared.mem.heap.zgc_director_tick(std::time::Instant::now()).unwrap_or(ZTick::Off),
        };
        match tick {
            ZTick::Off => return,
            ZTick::Wait(wait) => {
                drop(shared);
                std::thread::sleep(wait);
            }
            ZTick::Collect(cause) => {
                let ran = run_zgc_tick_collection(&shared, cause);
                drop(shared);
                if !ran {
                    // The attach or the pause could not run (a VM shutting
                    // down, a refused attach): look again later rather than
                    // retry at once.
                    std::thread::sleep(TICK_MAX_WAIT);
                }
            }
            // zt t23/zc6 (`CRATONVM_ZGC_MARK_END_SERVICE`, opt-in): the open
            // every-trigger cycle stayed at its fixed point across two looks;
            // close it now ([`run_zgc_mark_end_collection`]). A refused or
            // stale attempt looks again after the shortest tick.
            ZTick::CloseMark => {
                let ran = run_zgc_mark_end_collection(&shared);
                drop(shared);
                if !ran {
                    std::thread::sleep(TICK_MIN_WAIT);
                }
            }
        }
    }
}

/// zt t23/zc6 (`CRATONVM_ZGC_MARK_END_SERVICE`, opt-in; stage 1f of the
/// concurrent collector program): run the closing collection of an open
/// every-trigger cycle that has reached its fixed point, on a thread attached
/// for it, as [`run_zgc_tick_collection`] runs a tick's. HotSpot's driver runs
/// Pause Mark End as soon as marking completes; here the closing collection
/// otherwise waits for the next allocation poll, and a program that stops
/// allocating keeps the cycle -- and the SATB barrier on every reference store
/// -- open. `true` when the job ran.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_zgc_mark_end_collection(shared: &std::sync::Arc<SharedVm>) -> bool {
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel::<u64>(1);
    let (job_tx, job_rx) = std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
    let (out_tx, out_rx) = std::sync::mpsc::channel::<bool>();
    let job: crate::native::jni::AttachedServiceJob = Box::new(move |shared, thread| {
        let _ = out_tx.send(zgc_mark_end_collection(shared, thread));
    });
    if job_tx.send(job).is_err() {
        return false;
    }
    drop(job_tx);
    let owned = std::sync::Arc::clone(shared);
    // As the tick's attachment: HotSpot's director is a `NonJavaThread`.
    let jmm_counted = !zgc_tick_thread_uncounted_enabled();
    let Ok(handle) = std::thread::Builder::new()
        .name("ZDirector".to_string())
        .spawn(move || {
            crate::native::jni::run_attached_service_as(
                owned,
                "ZDirector",
                jmm_counted,
                started_tx,
                job_rx,
            )
        })
    else {
        return false;
    };
    let _ = handle.join();
    drop(started_rx);
    out_rx.recv().unwrap_or(false)
}

/// zt t23/zc6: the mark-end service's collection, on the attached thread: the
/// allocation-threshold door -- the one the closing poll would take, so the
/// cycle keeps the cause its trigger latched and no `System.gc()` semantics
/// apply -- run only while the cycle the tick found is still open, at its
/// fixed point and not a `System.gc()` caller's (that caller closes its own).
/// The heap counts each attempt (`mec_svc_*` on `[GC] zgc-markend-census:`).
/// Finalizers and cleaner actions stay with the ref-work hint, as the tick's.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_mark_end_collection(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    if !shared.mem.heap.is_zgc() {
        return false;
    }
    let heap = &shared.mem.heap;
    if heap.zgc_director_tick_stopped() {
        return false;
    }
    let Some(ticket) = heap.mark_end_service_begin() else {
        return false;
    };
    // As `maybe_gc` before its pause (a fresh attachment has neither in use).
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    shared.mem.heap.flush_thread_satb();
    // In flight before the last checks, as the tick's collection: the
    // launcher's exit arms wait for a pause past the director stop.
    let in_flight = heap.tick_pause_guard();
    if heap.zgc_director_tick_stopped() || !heap.mark_end_service_still_due(ticket) {
        drop(in_flight);
        heap.mark_end_service_end(ticket, None, false);
        return false;
    }
    // zt t24/zc8 (`CRATONVM_ZGC_MARK_CONTINUE`): the close may be a Pause
    // Mark End attempt that releases the world (`mcc_svc_continued`). zt t31
    // (program page 1f R4): told apart by this thread's own release count, not
    // the heap's global one, which another thread's release also moves.
    let released_before = zgc_mark_end_releases_here();
    let mark_continue = ZgcMarkContinueDoor::open(true);
    let paused = run_collection_pause(shared, thread, GcDoor::AllocationThreshold).is_some();
    drop(mark_continue);
    drop(in_flight);
    let continued_here = zgc_mark_end_releases_here() != released_before;
    heap.mark_end_service_end(ticket, Some(paused), continued_here);
    if paused && !zgc_tick_hands_off_gc_notifications(shared, thread) {
        run_gc_notifications(shared, thread, false);
    }
    true
}


// ---------------------------------------------------------------------------
// zt t24/zc8: Concurrent Mark Continue (stage 1f of
// `docs/feature-designs/zgc-architecture.md`)
// ---------------------------------------------------------------------------

thread_local! {
    /// zt t24/zc8: set while this thread's allocation-door collection request
    /// may end as a released ZGC Pause Mark End attempt ([`ZgcMarkContinueDoor`]).
    pub(in crate::runtime::interpreter::gc_and_alloc) static ZGC_MARK_CONTINUE_DOOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// zt t31/zc11 (program page 1f R4): the Pause Mark End attempts this thread
    /// released ([`zgc_release_mark_end_attempt`]).
    pub(in crate::runtime::interpreter::gc_and_alloc) static ZGC_MARK_END_RELEASES_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// zt t31/zc11: how many Pause Mark End attempts this thread has released.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_mark_end_releases_here() -> u64 {
    ZGC_MARK_END_RELEASES_HERE.with(std::cell::Cell::get)
}

/// zt t24/zc8 (`CRATONVM_ZGC_MARK_CONTINUE`, opt-in; on under
/// `CRATONVM_ZGC_CONCURRENT=1`, `=0` kills it there): the scope of a
/// collection request whose pause may be a Pause Mark End attempt that
/// releases the world instead of collecting -- the occupancy poll's
/// (`maybe_gc`) and the mark-end service's (`zgc_mark_end_collection`). The
/// other doors (the timer tick, `System.gc()` and its siblings, an allocation
/// failure, a latched request, the metadata threshold) never open it, so their
/// pause always collects, as HotSpot's do. Per thread and per call: dropping
/// it restores the previous value, so nothing nested in what runs after the
/// pause inherits it.
pub(in crate::runtime::interpreter::gc_and_alloc) struct ZgcMarkContinueDoor(pub(in crate::runtime::interpreter::gc_and_alloc) bool);

impl ZgcMarkContinueDoor {
    pub(in crate::runtime::interpreter::gc_and_alloc) fn open(open: bool) -> Self {
        Self(ZGC_MARK_CONTINUE_DOOR.with(|c| c.replace(open)))
    }

    pub(in crate::runtime::interpreter::gc_and_alloc) fn is_open() -> bool {
        ZGC_MARK_CONTINUE_DOOR.with(std::cell::Cell::get)
    }
}

impl Drop for ZgcMarkContinueDoor {
    fn drop(&mut self) {
        let previous = self.0;
        ZGC_MARK_CONTINUE_DOOR.with(|c| c.set(previous));
    }
}

/// zt t24/zc8: a Pause Mark End attempt that found mark work left -- the
/// pause's timer (`door=zgc-mark-end`) and where its work began.
pub(in crate::runtime::interpreter::gc_and_alloc) struct ZgcMarkEndRelease {
    pub(in crate::runtime::interpreter::gc_and_alloc) timer: Option<super::gc_events::NonCollectionPauseTimer>,
    pub(in crate::runtime::interpreter::gc_and_alloc) t0: std::time::Instant,
}

/// zt t24/zc8: the ZGC Pause Mark End attempt inside `run_collection_pause`'s
/// stopped world, before the collection's events, root gather and reference
/// pass (`ZgcRealHeap::try_mark_end`; the design is the "Concurrent Mark
/// Continue" section of `gc/src/zgc/mark_end.rs`). `Some` when mark work is
/// left: the caller releases the world without a collection
/// ([`zgc_release_mark_end_attempt`]). `None` -- the collection goes on --
/// when the door is not [`ZgcMarkContinueDoor`]-open or not the allocation
/// door, the heap is not ZGC, no attempt is due (`mark_continue_wanted`: the
/// switch off, no every-trigger cycle open, the cycle's attempts used up, the
/// urgent reserve), or the mark is complete.
///
/// The roots are the collection's sources -- this thread's scan, every
/// thread's snapshot, the frozen peers' conservative roots -- gathered with
/// `collect_roots` (the collection's own `collect_roots_registry_appended`
/// has one caller by rule; this scan pushes the thread mirrors a second time,
/// which the claim drops). A complete attempt costs the collection that
/// follows one extra scan; nothing it marks is unreachable.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_mark_end_attempt(
    shared: &SharedVm,
    thread: &JvmThread,
    door: GcDoor,
    xt_roots: &[ObjectRef],
) -> Option<ZgcMarkEndRelease> {
    // zt t31/zc11 (1f R7): ZGC's forced occupancy doors run on
    // `AllocationFailure` (`forced_door_on_heap`); the open door is what
    // decides, the door kind only keeps every other door out.
    if !matches!(
        door,
        GcDoor::AllocationThreshold | GcDoor::AllocationFailure
    ) || !ZgcMarkContinueDoor::is_open()
    {
        return None;
    }
    if !shared.mem.heap.is_zgc() {
        return None;
    }
    let heap = &shared.mem.heap;
    if !heap.zgc_mark_continue_wanted() {
        return None;
    }
    let t0 = std::time::Instant::now();
    let mut roots = collect_roots(shared, thread);
    let collect_end = roots.len();
    roots.extend(shared.threads.thread_registry.collect_all_root_snapshots());
    let snapshot_end = roots.len();
    roots.extend_from_slice(xt_roots);
    // JIT r23 w1b L7 (R23W1L7-4; `CRATONVM_DBG=root-kind-census`, off by default).
    let census_ends = (collect_end, snapshot_end, roots.len());
    root_kind_census_for_g1_mark(shared, "zgc-mark-end", &roots, census_ends);
    // SAFETY: called by `run_collection_pause` once `stw_take_over_and_wait`
    // returned, with the take-over still held: every other mutator is parked
    // at a safepoint or was forcibly stopped and conservatively scanned.
    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
    match heap.try_mark_end(&stw, &roots) {
        cratonvm_gc::zgc::ZMarkEndAttempt::Complete => None,
        cratonvm_gc::zgc::ZMarkEndAttempt::Continue => {
            let mut timer =
                non_collection_pause_start(shared, NonCollectionPause::ZgcMarkEndContinue);
            super::gc_events::non_collection_pause_backdate(&mut timer, t0);
            Some(ZgcMarkEndRelease { timer, t0 })
        }
    }
}

/// zt t24/zc8: release the world after a failed Pause Mark End attempt, as a
/// non-collection pause ends (`zgc_concurrent_mark_cycle`'s tail): seal the
/// pause inside the stopped world and start the heap's continue phase there
/// (`mark_continue_released`, the attempt's pause for the closing collection's
/// `-Xlog:gc,phases` lines; inside, so no later attempt can begin first),
/// resume the frozen peers, reopen the world with an empty pointer map
/// (nothing moved), disarm the unwind guard; then the pause's `[GC] pause:`
/// line, bean and `-Xlog:safepoint` line, and the VM pause span.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_release_mark_end_attempt(
    shared: &SharedVm,
    taken: crate::jit::xt_root_scan::TakenOver,
    abort: MovingPauseUnwindAbort,
    release: ZgcMarkEndRelease,
    vm_pause_cpu: Option<(u64, u64)>,
) {
    let ZgcMarkEndRelease { mut timer, t0 } = release;
    ZGC_MARK_END_RELEASES_HERE.with(|c| c.set(c.get().wrapping_add(1)));
    non_collection_pause_seal(&mut timer);
    let pause_ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
    shared.mem.heap.zgc_mark_continue_released(pause_ns);
    retire_skip_spans_and_resume(shared, taken);
    shared
        .mem
        .gc_barrier
        .complete_gc(cratonvm_types::PointerMap::default());
    abort.disarm();
    non_collection_pause_finish(timer);
    if let Some(start) = vm_pause_cpu {
        shared.mem.heap.zgc_book_vm_pause_cpu(start);
    }
}


/// `CRATONVM_ZGC_TICK_THREAD_UNCOUNTED` (zt t11/tg4, `zgc-zt10tg3-*` item 1;
/// default ON, `=0` kill). HotSpot 25's ZGC director is a `NonJavaThread`:
/// the `Timer` cycles it starts on an idle heap move no `ThreadMXBean`
/// counter (local 25.0.3, `Zt10Tg3TickThreads`, `-XX:ZCollectionInterval=1`,
/// three cycles: `started-delta=0`). Each tick collection here runs on a
/// thread attached for it, which the JMM counted as a started (and, while it
/// ran, live and peak) Java thread. With the switch the attachment is kept
/// out of those counters (`jni::run_attached_service_as`). Reachable only
/// with `-XX:ZCollectionInterval` or the opt-in idle tick, ZGC only. `=0`:
/// counted, as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_tick_thread_uncounted_enabled() -> bool {
    true
}

/// `CRATONVM_ZGC_TICK_NOTIFICATIONS_OFF_THREAD` (zt t11/tg4,
/// `zgc-zt11tg4-*` item 1; default ON, `=0` kill). HotSpot 25 runs a GC
/// listener for a `Timer` cycle on its `Notification Thread`, never on the
/// director (local 25.0.3, `Zt11Tg4TickNotificationThread`, five idle
/// `Timer` cycles: every listener call on `Notification Thread`). A
/// `--compatible` VM's tick job delivered them inline, on its attached
/// `ZDirector` thread (the `WhenLockHeld` rule hands off only a lock-holding
/// door's), which also built a `java.lang.Thread` for that attachment that
/// `Thread.getAllStackTraces()` could list. With the switch the tick hands
/// them off as the metadata door does
/// ([`metadata_door_hands_off_gc_notifications`]: under `WhenLockHeld`
/// whether or not a lock is held). `--real-jdk` (`OffMutator`) already
/// handed them off; the `CRATONVM_FINALIZER_THREAD=0` arm still delivers
/// inline. `=0`: the rule of every other door.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_tick_notifications_off_thread_enabled() -> bool {
    true
}

/// zt t11/tg4: the tick collection's GC-notification hand-off; `true` when
/// the delivery thread took them (the job must not run the listeners). See
/// [`zgc_tick_notifications_off_thread_enabled`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_tick_hands_off_gc_notifications(shared: &SharedVm, thread: &JvmThread) -> bool {
    if zgc_tick_notifications_off_thread_enabled() {
        metadata_door_hands_off_gc_notifications(shared, thread)
    } else {
        gc_notifications_go_to_delivery_thread(shared, thread)
    }
}

/// zt t7/tg: run one tick collection for `cause` on a thread attached for it,
/// as the attach commands run theirs ([`run_dcmd_full_gc_on_attach_thread`]):
/// the tick thread itself is no mutator and cannot take part in a pause. The
/// attached thread idles in the GC-blocked region until its job runs, is a
/// counted mutator while the job runs, initiates the pause through the
/// shared door, and detaches. `true` when the job ran and its collection (or
/// a sibling's that answered it) completed.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_zgc_tick_collection(
    shared: &std::sync::Arc<SharedVm>,
    cause: cratonvm_gc::zgc::timer::ZTickCause,
) -> bool {
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel::<u64>(1);
    let (job_tx, job_rx) = std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
    let (out_tx, out_rx) = std::sync::mpsc::channel::<bool>();
    let job: crate::native::jni::AttachedServiceJob = Box::new(move |shared, thread| {
        let _ = out_tx.send(zgc_tick_collection(shared, thread, cause));
    });
    if job_tx.send(job).is_err() {
        return false;
    }
    drop(job_tx);
    let owned = std::sync::Arc::clone(shared);
    // zt t11/tg4 (`zgc-zt10tg3-*` item 1): HotSpot's director is a
    // `NonJavaThread`; the attachment that runs its collection here is kept
    // out of the JMM's thread counters.
    let jmm_counted = !zgc_tick_thread_uncounted_enabled();
    let Ok(handle) = std::thread::Builder::new()
        .name("ZDirector".to_string())
        .spawn(move || {
            crate::native::jni::run_attached_service_as(
                owned,
                "ZDirector",
                jmm_counted,
                started_tx,
                job_rx,
            )
        })
    else {
        return false;
    };
    let _ = handle.join();
    drop(started_rx);
    out_rx.recv().unwrap_or(false)
}

/// zt t7/tg: the tick's collection, on the attached thread: a RULE-driven
/// cycle, the one `maybe_gc` would run had an allocation polled -- the
/// allocation-threshold door with the rule's cause charged on the heap's
/// trigger latch (`ZgcRealHeap::request_tick_cycle`), so it reports `Timer`
/// (or `Proactive`), clears no soft reference beyond what the rule-driven
/// door does, and runs none of `System.gc()`'s forced drains. Finalizers and
/// cleaner actions stay with the ref-work hint for the next door (HotSpot
/// runs them on the Finalizer / Cleaner threads, never on its director); GC
/// notifications are delivered as the attach commands deliver them.
///
/// A lost pause took part in another thread's: when that cycle answered the
/// rule (the clock restarted) the request is withdrawn and the job is done;
/// otherwise (a young-only cycle on a generational heap, which does not
/// restart HotSpot's major timer) it tries again, a bounded number of times.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_tick_collection(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cause: cratonvm_gc::zgc::timer::ZTickCause,
) -> bool {
    const TICK_GC_ATTEMPTS: u32 = 3;
    if !shared.mem.heap.is_zgc() {
        return false;
    }
    let heap = &shared.mem.heap;
    for _attempt in 0..TICK_GC_ATTEMPTS {
        // zt t10/tg3 (`t10-x7-report.md` R1, `CRATONVM_ZGC_TICK_STOP_ABORTS`):
        // the VM stopped the director tick (`stop_zgc_director_tick`, before
        // the `gc,exit` summary): no new attempt starts after it. A bit the
        // previous attempt charged and a lost pause left on the latch is
        // taken back too, while it is still the tick's own.
        if heap.zgc_director_tick_stopped() {
            heap.withdraw_tick_cycle_on_stop(cause);
            return false;
        }
        heap.request_tick_cycle(cause);
        // As `maybe_gc` before its pause: retire this thread's TLAB and
        // drain its SATB buffer (a fresh attachment has neither in use, but
        // the door's contract is the initiator's, whoever it is).
        flush_tlab_allocation_batch(thread, shared);
        thread.tlab.retire();
        shared.mem.heap.flush_thread_satb();
        // zt t10/tg3: the stop may land between the request and the pause;
        // the request's own cause bit is then taken back so it labels no
        // later collection. A stop after this point lets the pause run.
        // zt t11/tg4 (`t11-x8-report.md` C12 / T1): the pause is marked in
        // flight BEFORE this last check (both `SeqCst`), so a stop that
        // comes after the check sees it, and the launcher's main-thread exit
        // arms wait for it to end before the `gc,exit` summary
        // (`wait_for_zgc_tick_pause`, `CRATONVM_ZGC_TICK_STOP_WAITS`), as
        // HotSpot's director stop waits for its running cycle.
        // zt t12/x9: a guard, so a pause that unwinds still leaves the
        // in-flight count (the exit wait would otherwise sit out its bound).
        let in_flight = heap.tick_pause_guard();
        if heap.zgc_director_tick_stopped() {
            drop(in_flight);
            heap.withdraw_tick_cycle_on_stop(cause);
            return false;
        }
        if heap.try_arm_tick_concurrent_mark(cause) {
            zgc_concurrent_mark_cycle(shared, thread);
            if heap.is_conc_cycle_active() {
                drop(in_flight);
                return true;
            }
        }
        if matches!(
            cause,
            cratonvm_gc::zgc::timer::ZTickCause::Proactive
                | cratonvm_gc::zgc::timer::ZTickCause::Timer
        ) && (heap.is_conc_cycle_active() || heap.conc_sweep_pending())
        {
            heap.withdraw_tick_cycle_if_answered(cause, std::time::Instant::now());
            drop(in_flight);
            return true;
        }
        let paused = run_collection_pause(shared, thread, GcDoor::AllocationThreshold).is_some();
        drop(in_flight);
        if paused {
            if !zgc_tick_hands_off_gc_notifications(shared, thread) {
                run_gc_notifications(shared, thread, false);
            }
            return true;
        }
        if heap.withdraw_tick_cycle_if_answered(cause, std::time::Instant::now()) {
            return true;
        }
    }
    false
}

/// JFR `jdk.SystemGC` for one explicit GC that is not ignored, as HotSpot's
/// `JVM_GC` commits it (`invokedConcurrent` is the
/// `-XX:+ExplicitGCInvokesConcurrent` setting, whatever the collector).
/// `emit_system_gc_event` had no production caller before gc-common w8-g.
/// One relaxed load when no recording runs.
pub(in crate::runtime::interpreter::gc_and_alloc) fn emit_system_gc_jfr_event(shared: &SharedVm, thread: &JvmThread, invoked_concurrent: bool) {
    if !cratonvm_jfr::is_enabled() {
        return;
    }
    let now_ns = system_gc_wall_ns();
    let mut jfr = shared.debug.flight_recorder.lock();
    cratonvm_jfr::builtin::emit_system_gc_event(
        &mut jfr,
        invoked_concurrent,
        now_ns,
        thread.thread_id.0,
    );
}

/// zs s7/jf: `NativeContext::jfr_system_gc_on_caller` -- `Some(invoked
/// concurrent)` when the `Runtime.gc()` native writes `jdk.SystemGC` on the
/// calling thread (ZGC; see `gc_events::zgc_jfr_system_gc_on_caller`).
pub fn zgc_jfr_system_gc_on_caller(shared: &SharedVm) -> Option<bool> {
    super::gc_events::zgc_jfr_system_gc_on_caller(shared)
}

/// Wall-clock nanoseconds since the epoch (JFR's time base) for the
/// `jdk.SystemGC` rows. gci i12/ge: shared by both recorders' rows.
pub(in crate::runtime::interpreter::gc_and_alloc) fn system_gc_wall_ns() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
}

/// What one explicit GC request does in this VM. gc-common w8-g.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::gc_and_alloc) enum ExplicitGcAction {
    /// `-XX:+DisableExplicitGC`: nothing at all.
    Ignore,
    /// The `System.gc()` door ([`force_full_collection`]) and its drains.
    Collect,
}

/// The rule of [`force_gc_from_native`] for this VM's `VmConfig::explicit_gc`.
///
/// # Why `Concurrent` collects exactly as `Full` does
///
/// `-XX:+ExplicitGCInvokesConcurrent` exists to replace HotSpot's
/// stop-the-world FULL (whole-heap, compacting) collection with what G1 does
/// under it (`try_collect_concurrently`, waited out since JDK 15): a
/// concurrent-start YOUNG pause with cause `System.gc()`, then a marking cycle
/// that the caller waits to see finish (remark, cleanup). CratonVM's G1 has no
/// full collection to replace. Its `System.gc()`-door pause is an ordinary
/// young/mixed evacuation of the collection set
/// (`G1Collector::collect_garbage_with_finalizers`; G1 never reads the major-GC
/// request), and
/// [`force_full_collection`] then runs [`g1_force_full_cycle`], which joins or
/// starts a cycle and returns once its remark + cleanup ran. That is the
/// flag's shape already. HotSpot ignores the flag for collectors without a
/// concurrent explicit GC (Serial, Parallel: the Generational backend's
/// model), and ZGC's `System.gc()` is its whole-heap cycle either way. So here
/// the flag changes only the `invokedConcurrent` field of `jdk.SystemGC`,
/// which HotSpot also sets from it -- and, on G1, whether
/// [`g1_force_full_cycle`]'s fresh cycle is concurrent (this flag) or runs
/// whole inside one pause (`g1_force_full_cycle_is_stw`, JIT round 20 wave 21).
pub(in crate::runtime::interpreter::gc_and_alloc) fn explicit_gc_action(policy: crate::config::ExplicitGcPolicy) -> ExplicitGcAction {
    use crate::config::ExplicitGcPolicy;
    match policy {
        ExplicitGcPolicy::Disabled => ExplicitGcAction::Ignore,
        ExplicitGcPolicy::Concurrent | ExplicitGcPolicy::Full => ExplicitGcAction::Collect,
    }
}

/// The collection half of [`force_gc_from_native`]: one `System.gc()`-door
/// full collection (or a coalesced sibling's), the G1 concurrent-cycle tick
/// and, on G1, the synchronous full marking cycle.
pub(in crate::runtime::interpreter::gc_and_alloc) fn force_full_collection(shared: &SharedVm, thread: &mut JvmThread, door: GcDoor) {
    cratonvm_types::gc_entry_census::note_from_native();
    // zt t31c: whatever this door's young pause holds back, released at its end.
    let mut held = G1DoorHeldFinalizables {
        shared,
        armed: false,
    };
    let coalesce_baseline = system_gc_coalescing_baseline(shared, thread);
    // One HotSpot full collection has one previous soft-policy clock. Our G1
    // evacuation/remark/compaction pauses must not age a reference twice
    // within that same operation. Concurrent user requests remain distinct
    // young/concurrent collections and keep their normal per-round clocks.
    let _soft_policy_cycle = if shared.mem.heap.is_g1()
        && (g1_door_forces_stw_full_cycle(door)
            || g1_force_full_cycle_is_stw(shared.config.explicit_gc))
    {
        let mut backoff = LastDitchBackoff::default();
        loop {
            let acquired = shared.mem.ref_processor.lock().try_begin_soft_policy_cycle();
            if acquired {
                break;
            }
            safepoint_check(shared, thread);
            backoff.snooze();
        }
        let scope = G1FullSoftPolicyCycle(shared);
        // A full sibling completed while this caller waited. Its entire
        // operation, including reference processing, satisfies this request;
        // the native caller still performs its usual finalizer drains.
        if system_gc_completed_since(shared, coalesce_baseline) {
            return;
        }
        Some(scope)
    } else {
        None
    };
    // zt t31/x10 (`CRATONVM_GC_FULL_GC_HEAP_DUMPS`): HotSpot's
    // `-XX:+HeapDumpBeforeFullGC` dump, when this door is a full collection
    // on this heap (Serial, G1's `Pause Full`); `-XX:+HeapDumpAfterFullGC`'s
    // runs at the end, after a collection this call ran. No-op otherwise.
    let hotspot_full = super::gc_events::door_runs_hotspot_full_collection(shared, door);
    if hotspot_full {
        full_gc_heap_dump(shared, thread, true, full_gc_dump_gc_id(shared, true));
        owe_full_gc_heap_dump_after(shared, thread);
    }
    // gci i12/ge (`gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (b)):
    // under the opt-in `CRATONVM_G1_FULL_GC_ONE_REPORT`, G1's pauses below
    // (young, the marking cycle, compaction) are reported as ONE `Pause Full
    // (<cause>)` / `G1 Old Generation` collection (`gc_events.rs`); `None`
    // otherwise (and on every other backend).
    let full_report = super::gc_events::g1_full_report_begin(shared, door);
    // gci i13/gp (opt-in `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE`): a G1 cycle
    // marking now is the one HotSpot's full collection aborts; its remark
    // still runs below (`maybe_concurrent_gc_at` / `g1_force_full_cycle`) but
    // reports the abort. No-op otherwise.
    let _ = super::gc_events::g1_full_gc_abort_open_cycle(shared, door);
    // zt t3/mk (X1-9): on ZGC an open concurrent cycle runs to its end as its
    // own collection before the requested one, as HotSpot's driver port does.
    // No-op on every other backend and with no cycle open.
    zgc_full_request_closes_open_cycle(shared, thread);
    // zt t18/zc1 (opt-in `CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`, on under
    // `CRATONVM_ZGC_CONCURRENT=1`): the requested collection is the mark end
    // of a concurrent cycle opened here, after the call, rather than a
    // stop-the-world mark. No-op off ZGC and with the switch off.
    zgc_system_gc_runs_concurrent_cycle(shared, thread, door);
    // How many times `System.gc()` tries to INITIATE its own collection before
    // settling for having taken part in someone else's. See the loop below.
    const FORCE_GC_ATTEMPTS: u32 = 4;
    // `System.gc()` must COLLECT, not merely stand at a safepoint (2026-09-23).
    //
    // Until this date a `System.gc()` that lost the STW race — another thread
    // already initiating, which under a multi-threaded allocation load is the
    // common case — parked in `safepoint_check` for THAT collection and
    // returned. The other collection may be a young one; `request_major_gc`
    // below is a THREAD-LOCAL flag that only the initiating thread's collector
    // reads (`gc_quiescence::take_major_gc_request`), so the full collection
    // the caller asked for simply did not happen — and the unconsumed flag
    // then leaked into this thread's next, unrelated collection, turning an
    // allocation-triggered young GC into a major, non-moving one.
    //
    // So: try to initiate; on losing, take part in the pause in flight and try
    // again, a bounded number of times. Every iteration makes progress for
    // SOMEONE (either this thread collects, or it lets another thread's pause
    // finish), so this cannot livelock, and the bound keeps a pathological
    // contender from turning `System.gc()` into a spin. If every attempt lost,
    // the request is withdrawn rather than left to hijack a later cycle.
    //
    // COALESCING (gc-common w7-g, `common-e-small-findings` item 7). The retry
    // made N threads calling `System.gc()` together run up to N back-to-back
    // full collections. HotSpot runs one: `VM_GC_Operation::skip_operation`
    // drops a request when a full collection completed after the caller read
    // the count. Same rule here, per VM: a lost attempt that sees another
    // `System.gc()`-door collection completed since this call began returns
    // without collecting again -- that collection was as full as ours
    // (its initiator made the same major request) and took its roots after
    // this call began (see `system_gc_coalescing_baseline`). A lost attempt
    // that took part in an ALLOCATION pause still retries, as before.
    let mut collected = false;
    let mut coalesced = false;
    for _attempt in 0..FORCE_GC_ATTEMPTS {
        // Retire TLAB before GC
        flush_tlab_allocation_batch(thread, shared);
        thread.tlab.retire();
        // Round-5 fix (CRIT — UAF): drain this thread's per-thread SATB
        // buffer before initiating GC; see `maybe_gc` for the full rationale.
        shared.mem.heap.flush_thread_satb();
        // Real HotSpot's `System.gc()` triggers a FULL (old-gen-inclusive)
        // collection by default — request one explicitly, since the collector's
        // own Phase 5 otherwise only runs a major cycle when old gen crosses an
        // occupancy threshold. See `gc_quiescence`'s doc comment for the full
        // rationale (an already-promoted, genuinely-dead object is never swept by
        // a `System.gc()` that only triggers a minor collection).
        cratonvm_gc::gc_quiescence::request_major_gc();
        // gci i47w/sg47 (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default ON since the 2026-10-17 flip; a no-op
        // otherwise): this door's pause tenures every live young object, as
        // HotSpot's full collection does (Serial's mark-compact, G1's Full
        // GC). Withdrawn after the loop, whatever the loop's outcome.
        cratonvm_gc::gc_quiescence::request_system_gc_tenure();
        // gc-common w2-a (2026-09-23): the pause is the shared door
        // (`run_collection_pause`). The coverage cycle now opens only with a
        // WON request; the finalizable-root snapshot is taken INSIDE the pause
        // (it was taken before the request, so a finalizable object another
        // mutator registered and dropped meanwhile was freed without
        // `finalize()` — `handoff-d-force-gc-finalizable-roots-after-stw`); the
        // dead-finalizer list goes through `enqueue_resurrected_finalizers`
        // (not the address-keyed `enqueue`, which refused a new object at a
        // finalized one's old address); and one thread alone still holds the
        // barrier. A `None` has already taken part in the winner's pause,
        // exactly as this loop's lost arm always did.
        if run_collection_pause(shared, thread, door).is_some() {
            collected = true;
            break;
        }
        if system_gc_completed_since(shared, coalesce_baseline) {
            coalesced = true;
            break;
        }
    }
    held.armed = collected;
    // `gc_cycle_count` is advanced by `gc_event_finish` inside each collecting
    // arm above (dev's gen r4/plumbing), so it is not bumped again here.
    //
    // gci i47w/sg47: the tenure-all request belongs to the pause above only
    // (won, lost or coalesced): never to G1's marking-cycle and compaction
    // pauses below, nor to a later collection on this thread.
    let _ = cratonvm_gc::gc_quiescence::take_system_gc_tenure_request();
    if !collected {
        // Every attempt took part in someone else's pause instead (or a
        // sibling `System.gc()` satisfied this one). Withdraw the full-GC
        // request so it cannot hijack this thread's next, unrelated
        // collection (see the loop's doc above).
        let _ = cratonvm_gc::gc_quiescence::take_major_gc_request();
    }
    // zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`, opt-in): HotSpot's
    // `System.gc()` returns after the whole cycle. A call that took part in
    // another thread's pause (or coalesced with it) may find that pause's
    // sweep still running on its initiator: wait for it, or run it. The door
    // that collected already ran its own. One load when nothing is parked.
    let _ = shared.mem.heap.zgc_finish_pending_sweep_for_waiter();
    if coalesced {
        tracing::debug!(
            "System.gc() coalesced with a sibling System.gc() collection that completed \
             after this call began"
        );
    }
    // zt t26/zc5b (`CRATONVM_DBG_SYSTEM_GC_DOOR=1`, diagnostic, reads only):
    // one line per call naming how the door ended, so a probe round whose
    // weak references survived its `System.gc()` can be matched to a call
    // that never ran its own collection (`lost`: every attempt took part in
    // another thread's allocation pause) or to one that did (`collected`:
    // then a root kept the referents).
    system_gc_door_census(shared, collected, coalesced);
    // What follows still runs on a coalesced call: the G1 full cycle joins the
    // sibling's cycle if it is still open (`g1_force_full_cycle` helps finish
    // an open cycle), and `force_gc_from_native`'s forced drains keep
    // "finalizers queued by the collection have run when System.gc() returns"
    // for THIS caller too.
    //
    // INT-8: a forced GC advances the G1 concurrent-cycle machinery exactly
    // like an allocation-triggered young GC (`maybe_gc`'s epilogue calls
    // this at 819/903). Without it, a `System.gc()`-driven application —
    // whose forced young collections keep Eden below the allocation-GC
    // threshold — could NEVER start or complete a marking cycle: no cleanup
    // ever reclaimed dead Old regions and no remark-time reference
    // processing ever ran. HotSpot's default `System.gc()` under G1 is a
    // full collection that processes every generation's references; this
    // IHOP/completion check is the closest cycle-machinery equivalent.
    //
    // LANE W7-D — counted as its own door. This call and the
    // `last_ditch_reclaim` below are the two halves of the ONE flag-free route
    // that was measured completing cycles on a default build
    // (`w6m-a-workload-that-mixes.md` §4: 6 cycles, 9 mixed pauses), so a
    // census that folded it into `maybe_gc`'s row could not be used to check
    // that account.
    //
    // gci i15/ca (opt-in `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE`): when this
    // full collection marked the open G1 cycle aborted and the cycle is still
    // open, the tick is skipped: on G1 with a cycle open it can only FINISH
    // that cycle (a remark + cleanup the abort below exists to avoid), never
    // start one. `g1_force_full_cycle` then aborts it and marks afresh.
    // Switch off, or no marked open cycle: the tick runs, as before.
    let abort_pending = shared.mem.heap.g1_is_marking_active()
        && super::gc_events::g1_open_cycle_marked_aborted(shared).is_some();
    // gci cs48 (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`,
    // vi28-dg E4): a user-requested concurrent cycle (`System.gc()` / `jcmd
    // GC.run` under `-XX:+ExplicitGCInvokesConcurrent`) is HotSpot 25's
    // forced concurrent start (`decide_on_concurrent_start_pause`,
    // `is_user_requested_concurrent_full_gc`): with no cycle open, the door's
    // pause above is the `Concurrent Start` whatever IHOP says, and
    // `g1_force_full_cycle` below opens its cycle (the parked report prints
    // just before it). The IHOP tick is skipped then; a cycle already open is
    // waited out as before (the tick may finish it).
    let user_concurrent = g1_door_is_user_requested_concurrent(shared, door);
    let user_start = user_concurrent && !shared.mem.heap.g1_is_marking_active();
    if !abort_pending && !user_start {
        maybe_concurrent_gc_at(shared, thread, MarkDoor::SystemGc);
    }
    if shared.mem.heap.is_g1() {
        // An explicit System.gc() is a full-collection request, not merely an
        // Eden evacuation. Finish the G1 mark/remark/cleanup synchronously so
        // dead old regions, weak loaders, and their metadata are observable
        // before System.gc() returns.
        //
        // gc-common w2 (orchestrator, from handoff-d-system-gc-must-not-run-
        // the-last-ditch-soft-clear): the full cycle ONLY, not
        // `last_ditch_reclaim`. That is the allocation-failure rung, whose
        // soft-clear step condemns every SoftReference -- the rule that applies
        // only before throwing `OutOfMemoryError`. HotSpot's `System.gc()` on G1
        // keeps soft referents its LRU policy keeps; measured by
        // `RefCheckOld`'s `softKept` column (8/8 on HotSpot, 0/8 before this).
        //
        // gci i28/dg: only `System.gc()` and `jcmd GC.run` may turn
        // concurrent under `-XX:+ExplicitGCInvokesConcurrent`; every other
        // door's fresh cycle is the stop-the-world one, as HotSpot's.
        //
        // gci i47w/xl47 (`gci-i47w-j45-g1-system-gc-prints-two-safepoint-lines-FIXED-20261016`):
        // every collection pause this thread initiates from here to the door's
        // end (the compaction pause) is part of the door's ONE HotSpot
        // operation, whose `Safepoint` line and count the door's own pause
        // above already took: none of its own. Dropped at the block's end.
        let _one_operation = super::gc_events::g1_door_safepoint_fold(shared);
        if g1_door_forces_stw_full_cycle(door) {
            g1_force_full_cycle_stw(shared, thread);
        } else {
            g1_force_full_cycle(shared, thread);
        }
        // gci cs48: a report the door's pause parked for a cycle another
        // thread opened first is a `Normal` pause (a no-op once the cycle
        // above printed it as the `Concurrent Start`).
        super::gc_events::g1_emit_deferred_young_report(shared, thread.thread_id, false);
        // gci i12/ge: a cycle the compaction pause's mark drive opens is an
        // ordinary one, not part of the one report.
        super::gc_events::g1_full_report_marking_done(full_report.as_ref());
        // gci cs48 (vi28-dg E4): HotSpot's user-requested concurrent cycle
        // ends with its cleanup (`try_collect_concurrently` waits for it);
        // no compaction pause follows (it printed an extra `Pause Young
        // (Mixed) (System.gc())`).
        if !user_concurrent {
            g1_full_compaction_pause(shared, thread, door);
        } else {
            g1_full_gc_forget_undiscovered(shared);
        }
        // gci i30/ob hand-back 2 (vi34-ob S1): HotSpot's door is ONE full
        // collection, which leaves the Survivor pool's collection usage empty;
        // the compaction pause above (or the young pause before the mark) was
        // the last stamp here. Only for a stop-the-world door's cycle and
        // without the one-report scope (whose finish stamps itself).
        //
        // gci i41/rs (`gci-i39rv-waves-i35-i38-review-residuals-FIXED-20261016` item
        // 1): by default the stamp is taken INSIDE the compaction pause
        // (`run_collection_pause`, `g1_door_compaction_pause_stamps_survivor`),
        // not here with the world released; when no compaction pause ran (an
        // untrusted mark, `CRATONVM_G1_FULL_COMPACTION=0`) the stop-the-world
        // remark's every-pool stamp (`stw_full_mark_stamps_every_pool`) is the
        // door's last and already left the Survivor pool empty.
        // `CRATONVM_G1_FULL_DOOR_SURVIVOR_STAMP_IN_PAUSE=0` restores this
        // door-end call (and turns the in-pause stamp off).
        let stw_cycle = g1_door_forces_stw_full_cycle(door)
            || g1_force_full_cycle_is_stw(shared.config.explicit_gc);
        if stw_cycle && full_report.is_none() && !g1_full_door_survivor_stamp_in_pause_enabled() {
            super::gc_events::g1_full_door_leaves_the_survivor_empty(shared);
        }
    }
    if let Some(report) = full_report {
        report.finish();
    }
    // zt t31/x10: `-XX:+HeapDumpAfterFullGC`, for a full collection this
    // call ran (a coalesced or lost request ran none, as HotSpot's skipped
    // operation dumps nothing).
    if hotspot_full {
        settle_full_gc_heap_dump_after(shared, thread, collected, full_gc_dump_gc_id(shared, false));
    }
}

/// zt t3/mk (X1-9 of `docs/feature-designs/zgc-architecture.md`,
/// `CRATONVM_ZGC_FULL_REQUEST_CLOSES_OPEN_CYCLE`, default ON, `=0` kill): a
/// full request (`System.gc()`, JVMTI, the VM-internal full doors) that finds
/// a ZGC concurrent cycle open first lets that cycle finish as its OWN
/// collection -- one pause on the allocation door, which closes the cycle
/// with its mark set exactly as an allocation-triggered collection would --
/// and only then runs the requested collection, which then has no cycle to
/// close and marks from scratch.
///
/// HotSpot 25's `ZDriverPort::send_sync` queues the request behind the running
/// major cycle: that cycle completes (its `GcInfo`, its `-Xlog:gc` line, its
/// JFR rows) and a second cycle starts after the call. The t1 cut
/// (`CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK`) abandoned the open cycle instead,
/// so `GarbageCollectorMXBean.getCollectionCount()`, the `-Xlog:gc` lines and
/// `jdk.GarbageCollection` moved by 1 where HotSpot's move by 2. It stays as
/// the fallback for a cycle that opens between this pause and the requested
/// one.
///
/// It also closes the lost-election hole of the t1 cut: the request flag is
/// the requester's thread-local, so a peer's allocation pause that won the
/// election closed the cycle with its SATB set. Here the cycle is closed
/// BEFORE the requested pause, so a peer's pause that wins the requested
/// election marks from scratch (no cycle is open) and the door retries anyway
/// (an allocation pause does not coalesce a full request).
///
/// A lost election here is fine: the winner's collection closed the cycle.
/// Race, recorded: a cycle that closes between the read below and this pause
/// makes this pause an extra ordinary collection.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_full_request_closes_open_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    if !shared.mem.heap.zgc_concurrent_mark_active() {
        return;
    }
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    shared.mem.heap.flush_thread_satb();
    let _ = run_collection_pause(shared, thread, GcDoor::AllocationThreshold);
}

/// zt t18/zc1 (`CRATONVM_ZGC_SYSTEM_GC_CONCURRENT`, opt-in; on under the
/// concurrent profile `CRATONVM_ZGC_CONCURRENT=1`, `=0` kills it there): a ZGC
/// `System.gc()` / `jcmd GC.run` runs HotSpot 25's shape -- a concurrent cycle
/// that STARTS after the call (Pause Mark Start), a concurrent mark with every
/// other mutator running, and the requested collection as its mark end -- in
/// place of one stop-the-world collection that marks the whole heap inside the
/// pause. HotSpot's ZGC has no other kind of collection: its `System.gc()` is
/// `ZDriverPort::send_sync` of a whole concurrent cycle, which the caller waits
/// for.
///
/// 1. The heap records the request and makes it the OWNER of the cycle about
///    to open (`VmHeap::zgc_request_system_gc_concurrent_cycle`), before the
///    mark start, so a collection that runs in between consumes the ownership
///    and the request falls back to the stop-the-world collection.
/// 2. The mark start is the every-trigger mode's own
///    ([`zgc_concurrent_mark_cycle`]): the take-over cover, the reference skip
///    sets and the JFR rows are exactly those of a trigger-opened cycle.
/// 3. This thread waits, taking part in every pause it is asked to, until the
///    marker reaches its fixed point (bounded by
///    `cratonvm_gc::zgc::ZGC_SYSTEM_GC_CONC_WAIT_MS`). Meanwhile the heap's
///    `needs_gc` holds the fixed point for this request, so the cycle is not
///    closed by the next allocating poll under the allocation door.
/// 4. The caller's loop then runs its collection as before; because the
///    request owns the open cycle, that collection closes it with its mark set
///    instead of abandoning it (`CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK`), so the
///    pause is the mark end, not a whole mark. Every later step of
///    `force_full_collection` (coalescing, finalizer drains, the JFR
///    `jdk.SystemGC` row) is unchanged.
///
/// The doors HotSpot's ZGC answers with a whole concurrent cycle:
/// `System.gc()`, `jcmd GC.run` and, since zt t31/zc11 (program page 1b),
/// JVMTI `ForceGarbageCollection` (`GCCause::_jvmti_force_gc`, a synchronous
/// major cycle) and the class-metadata door (`_metadata_GC_threshold`; HotSpot
/// starts that cycle asynchronously and the define retries, here the define's
/// own call waits for it -- the same cycle, one wait earlier). The heap
/// inspection and dump doors keep the stop-the-world collection: HotSpot's
/// ZGC cannot run a cycle from inside their VM operation. A peer's collection that closes the
/// cycle first (an allocation failure, the urgent reserve) is booked `lost`
/// and the caller's collection then marks from scratch, as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_system_gc_runs_concurrent_cycle(shared: &SharedVm, thread: &mut JvmThread, door: GcDoor) {
    if !matches!(
        door,
        GcDoor::SystemGc
            | GcDoor::DiagnosticCommand
            | GcDoor::JvmtiForceGc
            | GcDoor::MetadataThreshold
    ) {
        return;
    }
    if !shared.mem.heap.zgc_request_system_gc_concurrent_cycle() {
        return;
    }
    // zt t31/zc11 (1b): the mark-start pause carries the requester's cause;
    // zt t32: on any thread that serves the request (`requested_cause`).
    let cycle = super::gc_events::zgc_concurrent_cycle_jfr(shared);
    if let Some(c) = &cycle {
        c.set_requested_cause(Some(door.cause()));
    }
    super::gc_events::with_zgc_mark_start_door(door, || {
        zgc_concurrent_mark_cycle(shared, thread);
        zgc_system_gc_adopt_served_start(shared, thread);
    });
    if let Some(c) = &cycle {
        c.set_requested_cause(None);
    }
    let opened = shared.mem.heap.zgc_concurrent_mark_active();
    shared.mem.heap.zgc_note_system_gc_concurrent_cycle(opened);
    if !opened {
        return;
    }
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(cratonvm_gc::zgc::ZGC_SYSTEM_GC_CONC_WAIT_MS);
    let mut backoff = LastDitchBackoff::default();
    // A thread waiting in VM Rust code reaches no safepoint poll of its own
    // (see `g1_force_full_cycle_with`): `safepoint_check` lets every pause
    // that is waiting for this thread -- a peer's collection, a mark-start of
    // another cycle -- proceed. The cycle can close under us (the urgent
    // reserve, an allocation failure); the loop then ends at once.
    while shared.mem.heap.zgc_concurrent_mark_active()
        && !shared.mem.heap.zgc_concurrent_mark_caught_up()
        && std::time::Instant::now() < deadline
    {
        safepoint_check(shared, thread);
        backoff.snooze();
    }
}

/// `CRATONVM_ZGC_MARK_START_REFRESHES_INITIATOR_SNAPSHOT` (zt t32, default
/// ON, `=0` kills): the ZGC mark start publishes its initiator's roots before
/// it gathers them (see the call in [`zgc_concurrent_mark_cycle`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_mark_start_refreshes_initiator_snapshot_enabled() -> bool {
    !Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .is_ok_and(|v| cratonvm_types::flags::value_is_off(&v))
}

/// `CRATONVM_ZGC_SYSTEM_GC_ADOPT_SERVED_START` (zt t32, default ON, `=0`
/// kills): see [`zgc_system_gc_adopt_served_start`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_system_gc_adopt_served_start_enabled() -> bool {
    !Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .is_ok_and(|v| cratonvm_types::flags::value_is_off(&v))
}

/// How many times a requested cycle's door re-tries its mark start after
/// losing it to a peer's pause ([`zgc_system_gc_adopt_served_start`]).
pub(in crate::runtime::interpreter::gc_and_alloc) const ZGC_SYSTEM_GC_START_RETRIES: usize = 3;

/// zt t32 (successor page section 10): the requesting door's mark start found
/// another stop-the-world pause in progress -- typically a PEER's allocating
/// poll serving this very request, since a requested cycle is a recorded
/// every-trigger request -- and returned without a cycle. The door then
/// dropped its ownership and ran a stop-the-world collection that marked from
/// scratch, over a cycle the peer opened for it: `Zs4Ob3PauseShape` booked
/// four pauses for one `System.gc()` under the profile on a loaded host (30
/// of 96 runs). HotSpot's synchronous request owns the one cycle it starts.
///
/// So the door takes part in the pause in progress (`safepoint_check`), and
/// if a cycle is open after it, ADOPTS it: it opened after the request was
/// recorded (the request is refused while a cycle is open), so its roots
/// postdate the call, which is all the door's own mark start would have
/// given. Still no cycle and the start still due: the door tries its own mark
/// start again, at most [`ZGC_SYSTEM_GC_START_RETRIES`] times. A start that
/// is no longer due (the request lapsed, the take-over cover latched) falls
/// back as before. `CRATONVM_ZGC_SYSTEM_GC_ADOPT_SERVED_START=0` restores the
/// single attempt.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_system_gc_adopt_served_start(shared: &SharedVm, thread: &mut JvmThread) {
    if !zgc_system_gc_adopt_served_start_enabled() {
        return;
    }
    for _ in 0..ZGC_SYSTEM_GC_START_RETRIES {
        if shared.mem.heap.zgc_concurrent_mark_active() {
            return;
        }
        safepoint_check(shared, thread);
        if shared.mem.heap.zgc_concurrent_mark_active()
            || !shared.mem.heap.zgc_should_start_concurrent_mark()
        {
            return;
        }
        zgc_concurrent_mark_cycle(shared, thread);
    }
}


pub(in crate::runtime::interpreter::gc_and_alloc) struct G1FullSoftPolicyCycle<'a>(&'a SharedVm);

impl Drop for G1FullSoftPolicyCycle<'_> {
    fn drop(&mut self) {
        self.0.mem.ref_processor.lock().finish_soft_policy_cycle();
    }
}

/// gcd d10/o: the compacting half of G1's full collection, after the marking
/// cycle [`g1_force_full_cycle`] has just completed. Cleanup frees only Old
/// regions with no live byte; a region keeping a few live objects beside a
/// megabyte of garbage stays until a mixed pause takes it, and neither
/// `System.gc()` nor the last-ditch rungs run one. This arms one uncapped
/// mixed pause (`G1Collector::arm_full_compaction`) and runs it -- HotSpot's
/// full collection compacts on both paths. A pause another thread initiates
/// meanwhile consumes the request instead, which is as good. No-op off G1, when
/// the cycle's cleanup did not trust its closure, or under
/// `CRATONVM_G1_FULL_COMPACTION=0`.
///
/// gci i10/dl: the pause runs on `door`, the full collection's requester
/// (`System.gc()`, `Metadata GC Threshold`, `jcmd GC.run`, the live-only
/// attach walks), not on the allocation-failure door: it reported
/// `Allocation Failure` for every one of them
/// (`gci-i9dc-g1-full-collection-compaction-pause-reports-allocation-failure-FIXED-20261017`).
///
/// gci i48/rf48 (`gci-i15rf-...-20261014` residual): the compaction pause is
/// the last pause of the one full collection, so what its cycle's remark left
/// undiscovered stays undiscovered there (armed before it); and nothing of
/// the full collection's undiscovered bookkeeping outlives this call, whether
/// or not a pause ran (an untrusted mark arms no compaction).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_compaction_pause(shared: &SharedVm, thread: &mut JvmThread, door: GcDoor) {
    g1_full_gc_arm_undiscovered(shared);
    if shared.mem.heap.g1_arm_full_compaction() {
        let _ = maybe_gc_forced_collected_on(shared, thread, "g1-full-compaction", Some(door));
    }
    g1_full_gc_forget_undiscovered(shared);
}

/// gci i41/rs: `CRATONVM_G1_FULL_DOOR_SURVIVOR_STAMP_IN_PAUSE` (default ON,
/// `=0` kills): the G1 full-collection door's Survivor re-stamp (gci i30/ob
/// hand-back 2) is taken inside the door's own full-compaction pause
/// ([`g1_door_compaction_pause_stamps_survivor`]) instead of at the door's end
/// with the world released (`gci-i39rv-waves-i35-i38-review-residuals-FIXED-20261016`
/// item 1). `=0` restores the door-end call in [`force_full_collection`].
/// Read at the door's end and, inside a pause, only when that pause ran a
/// full compaction on a full-collection door (no static).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_door_survivor_stamp_in_pause_enabled() -> bool {
    true
}

/// G1's count of full-compaction pauses run so far
/// (`G1Collector::full_compaction_pause_count`); `None` off G1.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_compaction_pause_count(shared: &SharedVm) -> Option<u64> {
    match &shared.mem.heap {
        _ => shared.mem.heap.g1_full_compaction_pause_count(),
    }
}

/// Did a full-compaction pause run since `before` ([`g1_full_compaction_pause_count`]
/// read earlier in the same pause)? `false` off G1.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_compactions_since(shared: &SharedVm, before: Option<u64>) -> bool {
    full_compaction_count_moved(before, g1_full_compaction_pause_count(shared))
}

/// The rule of [`g1_full_compactions_since`], apart. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn full_compaction_count_moved(before: Option<u64>, now: Option<u64>) -> bool {
    matches!((before, now), (Some(b), Some(n)) if n != b)
}

/// gci i41/rs: does this pause take the G1 full-collection door's Survivor
/// stamp (`gc_events::g1_full_door_compaction_stamps_the_survivor`)? Exactly
/// when it ran a full compaction (`compaction_ran`), its door is a
/// full-collection door ([`g1_full_collection_door`]: a compaction request an
/// allocation pause consumed -- the last-ditch rungs' own, or a door's request
/// another thread's pause took -- stamps nothing), the door's cycle is the
/// stop-the-world one (the door-end call's own condition), and the switch
/// ([`g1_full_door_survivor_stamp_in_pause_enabled`], read last, so an
/// ordinary pause reads no flag) is on. The one-report scope's folded pauses
/// are refused by the stamp itself (no bean snapshot). The stamp seam.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_door_compaction_pause_stamps_survivor(
    compaction_ran: bool,
    door: GcDoor,
    policy: crate::config::ExplicitGcPolicy,
    switch_on: impl FnOnce() -> bool,
) -> bool {
    compaction_ran
        && g1_full_collection_door(door)
        && (g1_door_forces_stw_full_cycle(door) || g1_force_full_cycle_is_stw(policy))
        && switch_on()
}

/// This VM's count of COMPLETED `System.gc()`-door collections
/// (`HeapRealm::system_gc_collections`), bumped by [`run_collection_pause`]
/// inside the pause, before the release. Per VM: a sibling VM's `System.gc()`
/// satisfies nothing here. gc-common w7-g (`common-e-small-findings` item 7).
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn system_gc_collections(shared: &SharedVm) -> &std::sync::atomic::AtomicU64 {
    &shared.mem.system_gc_collections
}

/// zt t26/zc5b: `CRATONVM_DBG_SYSTEM_GC_DOOR=1` (diagnostic, default OFF,
/// reads only; page `zgc-zt26zc5b-compatible-loader-unload-and-weak-round-residuals-FIXED-20261024`
/// item 1). One stderr line per `System.gc()` door, after its attempts:
/// `[sysgc-door] outcome=collected|coalesced|lost sysgc_collections=N
/// gc_cycles=M conc_mark=<bool>`. `lost` = every attempt took part in
/// another thread's (allocation) pause and no sibling `System.gc()`
/// completed, so this call ran no collection of its own. Read once per call.
pub(in crate::runtime::interpreter::gc_and_alloc) fn system_gc_door_census(shared: &SharedVm, collected: bool, coalesced: bool) {
    if !false {
        return;
    }
    use std::sync::atomic::Ordering;
    let sysgc = system_gc_collections(shared).load(Ordering::Acquire);
    let cycles = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    eprintln!(
        "[sysgc-door] outcome={} sysgc_collections={sysgc} gc_cycles={cycles} conc_mark={}",
        system_gc_door_outcome(collected, coalesced),
        shared.mem.heap.zgc_concurrent_mark_active(),
    );
}

/// The `outcome=` word of [`system_gc_door_census`]. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn system_gc_door_outcome(collected: bool, coalesced: bool) -> &'static str {
    if collected {
        "collected"
    } else if coalesced {
        "coalesced"
    } else {
        "lost"
    }
}

/// The count a `System.gc()` call may coalesce against, read when the call
/// begins; `None` when this call must not coalesce at all.
///
/// # Why "one completed since" is enough (HotSpot's `skip_operation` rule)
///
/// A `System.gc()`-door collection that COMPLETES after this read took its
/// roots after it too. The caller is running VM code here, so it is counted by
/// any pause requested meanwhile, and that pause cannot collect before the
/// caller arrives (a lost attempt's `safepoint_check`) -- its roots then include
/// the caller's current state, and the garbage the caller made before asking is
/// garbage to it. A caller the take-over freezes is frozen from before this
/// read (it is not running) until that pause completes, so such a pause bumps
/// before the read.
///
/// The one caller that argument does not cover is a thread whose own
/// `in_blocked_region` is raised (a JNI host thread marked blocked that reaches
/// this door, or a leaked raise): a census excludes it by identity and a pause
/// can collect from its deposited snapshot. It gets `None` and keeps the
/// pre-w7 "retry until I collect" behaviour.
pub(in crate::runtime::interpreter::gc_and_alloc) fn system_gc_coalescing_baseline(shared: &SharedVm, thread: &JvmThread) -> Option<u64> {
    if thread
        .gc_block_state
        .in_blocked_region
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return None;
    }
    Some(system_gc_collections(shared).load(std::sync::atomic::Ordering::Acquire))
}

/// Has a `System.gc()`-door collection completed since `baseline` was read?
/// See [`system_gc_coalescing_baseline`]. The bump happens before the
/// winner's `complete_gc`, whose generation release is what woke the losing
/// caller, so it is visible here.
pub(in crate::runtime::interpreter::gc_and_alloc) fn system_gc_completed_since(shared: &SharedVm, baseline: Option<u64>) -> bool {
    baseline.is_some_and(|before| {
        system_gc_satisfied(
            before,
            system_gc_collections(shared).load(std::sync::atomic::Ordering::Acquire),
        )
    })
}

/// The coalescing rule itself: at least one completed since the baseline.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn system_gc_satisfied(baseline: u64, now: u64) -> bool {
    now > baseline
}

/// Drain pending Cleaner actions and invoke their Runnable.run() method.
///
/// Each entry is the address of a `java/lang/ref/Cleaner$Cleanable`
/// synthetic. Field 0 holds the Runnable action; field 1 is the cleaned
/// flag (idempotency guard, also set by user-triggered Cleanable.clean()).
///
/// A real-JDK `jdk.internal.ref.Cleaner` also arrives here — the reference
/// processor emits it as an action rather than enqueuing it onto its
/// reader-less `dummyQueue` (see `ReferenceEntry::runs_cleaner`). It has a
/// completely different layout and is handled by invoking its own `clean()`.
///
/// Per the `Cleaner` contract, exceptions thrown by an action are caught
/// and logged — they must not propagate into the GC pipeline.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_cleaner_actions_impl(shared: &SharedVm, thread: &mut JvmThread, force: bool) {
    // bc math-ec 0x4 exclusion switches — see `process_references_after_gc`.
    if no_refproc() || no_cleaners() {
        return;
    }
    if dm_dbg_enabled() {
        DM_CLEANER_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    // Re-entrancy safety: if a JIT helper currently holds the `&mut JvmThread`
    // (we were reached via `jit_invoke_dispatch` → `bail_to_interpreter` →
    // interpreter → `maybe_gc`), running a cleaner action's `run()` could
    // execute JIT-compiled code that calls `jit_thread_mut`, aliasing the live
    // borrow (debug: aliasing assert; release: UB/SEGV — the avrora crash
    // exposed by real-bytecode RAF's FileCleanable cleanups). Leave the actions
    // queued; they are GC-relocated (`update_after_gc`) and run at the next
    // top-level (non-JIT) safepoint.
    if !force && crate::jit::helpers::is_jit_thread_set() {
        if dm_dbg_enabled() {
            let n = DM_CLEANER_JIT_BLOCKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            // ALWAYS print when something is actually queued: a silent bail with
            // a non-empty backlog is precisely the case worth seeing, and the
            // %200 rate limit hid it at the one moment that mattered.
            if n % 200 == 1 || shared.mem.cleaner_thread.pending_count() > 0 {
                eprintln!(
                    "[dm] run_cleaner_actions BLOCKED jit_thread_set: blocked={} pending={} calls={} thread={:?}",
                    n,
                    shared.mem.cleaner_thread.pending_count(),
                    DM_CLEANER_CALLS.load(std::sync::atomic::Ordering::Relaxed),
                    std::thread::current().id()
                );
            }
        }
        return;
    }
    // S-bytebuddy r3 — independent recursion guard for cleaner-action
    // dispatch. A Runnable.run() invoked from here can itself enqueue
    // (or trigger GC of) another Cleanable, which lands back in
    // `run_cleaner_actions` on the same OS thread. The aggregate
    // EXEC_DEPTH guard in `execute` catches this too, but only after
    // we have already burned ~10 Rust frames per turn of the loop.
    // A small dedicated counter trips earlier and avoids the loop
    // accumulating frames before the bigger guard fires.
    thread_local! {
        static CLEANER_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }
    struct CleanerDepthGuard;
    impl Drop for CleanerDepthGuard {
        fn drop(&mut self) {
            CLEANER_DEPTH.with(|d| {
                let v = d.get();
                d.set(v.saturating_sub(1));
            });
        }
    }
    let cdepth = CLEANER_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    // Limit calibrated to roughly 1/10 of EXEC_DEPTH (so a runaway
    // cleaner cascade trips here long before the main guard). The
    // per-iteration step factor is implicit: each cleaner action burns
    // ~10 native frames between dispatch and return.
    if cdepth > 1_000 {
        CLEANER_DEPTH.with(|d| {
            let v = d.get();
            d.set(v.saturating_sub(1));
        });
        // Don't propagate as a Java exception — the cleaner contract
        // forbids exceptions escaping. Just drop the remaining actions
        // on the floor; they will be retried on the next GC tick.
        return;
    }
    let _cleaner_depth_guard = CleanerDepthGuard;

    let addrs = shared.mem.cleaner_thread.drain_actions();
    if dm_dbg_enabled() {
        use std::sync::atomic::Ordering::Relaxed;
        if addrs.is_empty() {
            let e = DM_CLEANER_EMPTY.fetch_add(1, Relaxed) + 1;
            if e % 500 == 1 {
                eprintln!(
                    "[dm] run_cleaner_actions DRAIN empty x{} (calls={} jit_blocked={} cum_drained={})",
                    e,
                    DM_CLEANER_CALLS.load(Relaxed),
                    DM_CLEANER_JIT_BLOCKED.load(Relaxed),
                    DM_CLEANER_DRAINED.load(Relaxed)
                );
            }
        } else {
            let d = DM_CLEANER_DRAINED.fetch_add(addrs.len() as u64, Relaxed) + addrs.len() as u64;
            eprintln!(
                "[dm] run_cleaner_actions DRAIN n={} cum_drained={} calls={} jit_blocked={} thread={:?}",
                addrs.len(),
                d,
                DM_CLEANER_CALLS.load(Relaxed),
                DM_CLEANER_JIT_BLOCKED.load(Relaxed),
                std::thread::current().id()
            );
        }
    }
    // PIN THE WHOLE BATCH. `drain_actions` removed these addresses from the
    // cleaner queue, which is what `update_after_gc` remaps and what the GC
    // roots — so from here on this local list is their only record. Every
    // iteration below runs Java (`clean()`, a lambda body, `run()`), which can
    // allocate and collect, and a moving collection during action `i` used to
    // leave actions `i+1..` pointing at vacated addresses: the loop then set
    // "cleaned" and nulled slot 0 of whatever the allocator had put there.
    // Pinned, they are rooted and rewritten in place; each is re-read from its
    // pin slot when its turn comes, the discipline `unwind_to_handler`
    // documents.
    // An address outside the heap cannot be a live action; it is dropped here
    // rather than handed to the collector as a root (the per-item shape screen
    // below still runs for everything that is pinned).
    let pin_base = thread.native_pin_roots.len();
    for &addr in &addrs {
        if shared.mem.heap.is_heap_addr(addr).is_none() {
            continue;
        }
        // SAFETY: addr was produced by the cleaner thread's drain_actions and
        // points at a valid object header within the heap arena (the queue is
        // kept current by `update_after_gc`, and nothing has run since).
        thread
            .native_pin_roots
            .push(unsafe { ObjectRef::from_raw(addr as *mut u8) });
    }
    let pin_end = thread.native_pin_roots.len();
    for pin in pin_base..pin_end {
        let cleanable = thread.native_pin_roots[pin];
        // A real `jdk.internal.ref.Cleaner` is NOT the synthetic `Cleanable`
        // shape the rest of this loop assumes (field 0 = action, field 1 =
        // cleaned flag) — its slots are `PhantomReference`'s. It carries its own
        // `clean()`, which unlinks it from the class's static list and runs its
        // thunk exactly once, and that is precisely what the JDK's
        // `ReferenceHandler` calls when it sees one. Dispatch to it and skip the
        // `Cleanable` decoding entirely: reading field 1 of a `Cleaner` as a
        // "cleaned" flag would be reading its `queue`.
        //
        // See `native_phantom_ref_init` for why these arrive here at all.
        {
            let class_id = shared.mem.heap.class_id_of(cleanable);
            let (is_jdk_cleaner, shaped) = {
                let cm = shared.classes.class_manager.read();
                (
                    cm.get_class(class_id)
                        .is_some_and(|c| c.name.as_ref() == "jdk/internal/ref/Cleaner"),
                    is_cleanable_shaped(&cm, shared, cleanable),
                )
            };
            // See `is_cleanable_shaped`. The submit side screens too, but the
            // queue survives collections between the two, so the write site
            // checks for itself rather than trusting an older verdict.
            if !shaped {
                continue;
            }
            if is_jdk_cleaner {
                // Errors are swallowed per the Cleaner contract, exactly as for
                // the `Cleanable` arm below. `class_id` IS
                // `jdk/internal/ref/Cleaner` (that is what `is_jdk_cleaner`
                // tested), so dispatching on it skips a name lookup per action.
                let _ = crate::vm::invoke_by_class_id_shared(
                    shared,
                    thread,
                    class_id,
                    "clean",
                    "()V",
                    &[Value::Object(Some(cleanable))],
                );
                continue;
            }
        }
        // Idempotency: skip if user code already invoked clean().
        let already = matches!(shared.mem.heap.get_field(cleanable, 1), Value::Int(1),);
        if already {
            continue;
        }
        shared.mem.heap.set_field(cleanable, 1, Value::Int(1));
        let action = match shared.mem.heap.get_field(cleanable, 0) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        // Clear the action slot so it can be GC'd on the next cycle.
        shared.mem.heap.set_field(cleanable, 0, Value::Object(None));
        let class_id = shared.mem.heap.class_id_of(action);

        // Fast path: if the action is a lambda proxy (the common case —
        // `cleaner.register(buf, () -> {...})`), `invoke_shared` would
        // fall through to the abstract `Runnable.run()` declaration which
        // has no Code attribute. Route through `try_lambda_dispatch` so
        // the proxy's SAM impl_handle actually fires.
        let is_lambda = shared.classes.lambda_proxies.read().contains_key(&class_id);
        if is_lambda {
            // Errors are silently swallowed per the Cleaner contract.
            let _ = try_lambda_dispatch(shared, thread, action, class_id, "run", "()V", &[]);
            continue;
        }

        let class_known = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .is_some();
        if class_known {
            // Errors are silently swallowed per the Cleaner contract
            // (JDK catches Throwable inside CleanerImpl.run()).
            //
            // gc-common w1-d: on the action's own `ClassId`, not its name —
            // see the same change in `run_finalizers_impl`. A cleanup action is
            // very often an application class (a webapp's resource closer), and
            // a loader-blind name lookup is exactly what a redeployed webapp's
            // second copy of that class defeats.
            let _ = crate::vm::invoke_by_class_id_shared(
                shared,
                thread,
                class_id,
                "run",
                "()V",
                &[Value::Object(Some(action))],
            );
        }
    }
    thread.native_pin_roots.truncate(pin_base);
}

/// `CRATONVM_FORCED_FINALIZERS=0` — go back to deferring `System.gc()`'s
/// finalizer drain whenever a JIT helper holds the thread borrow.
///
/// That deferral is the defect this flag A/Bs, so `0` is the BROKEN arm and
/// exists only to measure it on one binary. See [`run_finalizers_forced`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn forced_finalizers_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Dequeue pending finalizable objects and invoke their finalize() method.
///
/// Defers when a JIT helper holds the thread borrow. Correct for the
/// allocation-triggered callers, which will be back within an allocation or
/// two — and WRONG for `System.gc()`, which is why that path calls
/// [`run_finalizers_forced`] instead.
pub(in crate::runtime::interpreter) fn run_finalizers(shared: &SharedVm, thread: &mut JvmThread) {
    // gc-common w3-d / w4-d: `finalize()` never runs on an allocating mutator
    // that holds a user-visible lock (JLS §12.6) — by default only then, with
    // `CRATONVM_FINALIZER_THREAD=1` never. See `FinalizerDelivery`.
    if finalizer_thread_takes_finalizers(shared, thread) {
        return;
    }
    run_finalizers_impl(shared, thread, false);
}

