#[allow(unused_imports)]
use super::*;

/// [`g1_force_full_cycle`] with an optional `between` hook (gci i14/ox,
/// `gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018` item
/// 7's cost half): when a cycle was open on entry and a fresh one is due,
/// the hook is called once, after the open cycle's remark + cleanup ran (or
/// another thread was seen to close it) and before the fresh cycle starts;
/// `true` (it allocated what the failed request needed) returns at once with
/// `true` and the fresh cycle does not run. Only the interpreter ladder passes
/// one (`last_ditch_then_second_major` under
/// `CRATONVM_G1_OOME_RETRY_BEFORE_FRESH_CYCLE`); `System.gc()` and every other
/// caller pass `None`, the loop below byte for byte. `false` otherwise.
///
/// gci i28/dg: `door_forces_stw` -- the caller's door is not a user-requested
/// concurrent cause ([`g1_door_forces_stw_full_cycle`]), so the fresh cycle is
/// the stop-the-world one even under `-XX:+ExplicitGCInvokesConcurrent`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_force_full_cycle_with(
    shared: &SharedVm,
    thread: &mut JvmThread,
    mut between: Option<&mut dyn FnMut(&SharedVm, &mut JvmThread) -> bool>,
    door_forces_stw: bool,
) -> bool {
    if !shared.mem.heap.is_g1() {
        return false;
    }
    let started_at = std::time::Instant::now();
    let mut deadline = started_at + FORCE_FULL_CYCLE_BUDGET;
    // JIT round 22 wave 9 (lane memory, `CRATONVM_G1_FULL_CYCLE_BUDGET_FOLLOWS_PROGRESS`):
    // the budget is renewed while other initiators keep completing pauses.
    let budget_follows = g1_full_cycle_budget_follows_progress_enabled();
    let hard_deadline = started_at + FORCE_FULL_CYCLE_HARD_CAP;
    let mut seen_progress = full_cycle_progress_now(shared);
    let quiesce_by = started_at + FORCE_FULL_QUIESCE_WAIT;
    let mut backoff = LastDitchBackoff::default();
    // A cycle already mid-flight is finished first; its snapshot predates
    // this request, so a fresh one follows (see the doc above).
    let mut saw_active = shared.mem.heap.g1_is_marking_active();
    let mut need_fresh = saw_active && g1_force_full_fresh_cycle();
    // gci i15/ca (`gci-i13oa-...-review-findings-20261013` item 7's cost half,
    // opt-in `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE`): when the caller is a
    // full collection whose report half marked the open cycle aborted, the
    // cycle is ABORTED (`g1_abort_open_cycle_pause`) instead of finished, and
    // only the fresh cycle -- which marks from its own roots -- runs: one
    // marking cycle per full collection, HotSpot's shape. Only with a fresh
    // cycle due (`CRATONVM_G1_FORCE_FULL_FRESH_CYCLE=0` keeps the finish).
    // Switch off, or no mark: `false`, the loop below as before.
    let mut abort_open =
        need_fresh && super::gc_events::g1_open_cycle_marked_aborted(shared).is_some();
    // LANE W7-D — this door is counted, but NOT through `g1_drive_mark_cycle`,
    // and it counts ONE `waiting` per CALL rather than one per loop turn, so
    // `visits` means the same thing in every row of the door census.
    let mut waited = false;
    let mut lost_races = 0u32;
    loop {
        // gci i15/ca: the abort is for the cycle open on entry. Once no cycle
        // marks, a cycle seen later is this call's own fresh one (or one a
        // later trigger opened), which is finished, never aborted.
        if abort_open && !shared.mem.heap.g1_is_marking_active() {
            abort_open = false;
        }
        if abort_open && shared.mem.heap.g1_is_marking_active() {
            saw_active = true;
            match g1_abort_open_cycle_pause(shared, thread) {
                Some(aborted) => {
                    // Aborted: nothing was freed, so the i14/ox hook (an
                    // attempt after the open cycle's cleanup) has nothing to
                    // try; the fresh cycle starts on the next turn. Not open
                    // inside the pause (another thread's remark closed it
                    // first): the next turn's `saw_active` arm takes over.
                    abort_open = false;
                    if aborted {
                        need_fresh = false;
                        saw_active = false;
                        waited = false;
                    }
                    continue;
                }
                None => {
                    // Lost the STW race: take part in the winner's pause.
                    lost_races += 1;
                    safepoint_check(shared, thread);
                    backoff.snooze();
                }
            }
        } else if shared.mem.heap.g1_is_marking_active() {
            saw_active = true;
            let quiesced = shared.mem.heap.g1_concurrent_mark_finished();
            if quiesced || std::time::Instant::now() >= quiesce_by {
                // Runs remark (+ an in-pause drain to a fixed point when the
                // marker had not converged) and cleanup under a brief STW; on
                // a lost STW race the cycle stays open and the loop retries.
                let ran = g1_final_remark_cleanup(shared, thread);
                note_mark_door(
                    MarkDoor::ForceFull,
                    if ran {
                        MarkDoorOutcome::Finished
                    } else {
                        MarkDoorOutcome::LostStw
                    },
                );
                if ran {
                    oome_ladder_step(shared, thread, "full-cycle finished-open");
                    if !quiesced {
                        let waited_ms = elapsed_ms(started_at);
                        tracing::debug!(
                            waited_ms,
                            "[G1] last-ditch full cycle: the concurrent marker had not \
                             converged; its closure was finished inside the remark pause"
                        );
                    }
                    if need_fresh {
                        // gci i14/ox: the caller's attempt first.
                        if fresh_cycle_hook_allocated(&mut between, shared, thread) {
                            return true;
                        }
                        need_fresh = false;
                        saw_active = false;
                        waited = false;
                        continue;
                    }
                    return false;
                }
                // Lost the STW race: another thread's pause is in flight and
                // is waiting for THIS thread to arrive.
                lost_races += 1;
                safepoint_check(shared, thread);
                backoff.snooze();
            } else {
                if !waited {
                    waited = true;
                    note_mark_door(MarkDoor::ForceFull, MarkDoorOutcome::Waiting);
                }
                // TAKE PART IN ANY PAUSE THAT IS WAITING FOR US (2026-09-23).
                //
                // A thread waiting in VM Rust code reaches no safepoint poll:
                // the interpreter's polls are at allocation sites and backward
                // branches, and the cross-thread takeover only freezes threads
                // in COMPILED code. So any other thread that requested a pause
                // meanwhile — a young collection, or the remark this loop is
                // itself waiting to run — would wait for this thread until the
                // wait ended. `safepoint_check` is a single flag load when
                // nothing is pending, and this thread is GC-safe here (every
                // caller has already been through a collection that roots its
                // frames).
                safepoint_check(shared, thread);
                backoff.snooze();
            }
        } else if saw_active {
            // Another thread closed the cycle — cleanup has run.
            oome_ladder_step(shared, thread, "full-cycle closed-by-other");
            if need_fresh {
                // gci i14/ox: the caller's attempt first.
                if fresh_cycle_hook_allocated(&mut between, shared, thread) {
                    return true;
                }
                need_fresh = false;
                saw_active = false;
                waited = false;
                continue;
            }
            return false;
        } else {
            // Not started yet (or the initial-mark STW lost its race —
            // g1_concurrent_mark_cycle returns without activating in that
            // case). Start/retry it.
            //
            // JIT round 20 wave 21 (lane g1ref): HotSpot's `System.gc()` is a
            // stop-the-world full collection, so the fresh cycle runs whole
            // inside one pause (`g1_stw_full_mark_cycle`) unless
            // `-XX:+ExplicitGCInvokesConcurrent` asked for the concurrent one.
            // Unconditional: a concurrent window let any `Reference.get()`
            // keep a weakly-reachable referent alive through every cycle.
            if g1_force_full_cycle_is_stw(shared.config.explicit_gc) || door_forces_stw {
                if g1_stw_full_mark_cycle(shared, thread) {
                    oome_ladder_step(shared, thread, "full-cycle stw-fresh");
                    note_mark_door(MarkDoor::ForceFull, MarkDoorOutcome::Finished);
                    // This WAS the fresh cycle: any open one was finished above
                    // before `need_fresh` led here.
                    return false;
                }
                // The STW lost its race: take part in the winner's pause.
                lost_races += 1;
                safepoint_check(shared, thread);
                backoff.snooze();
                if force_full_cycle_out_of_time(
                    std::time::Instant::now(),
                    &mut deadline,
                    hard_deadline,
                    &mut seen_progress,
                    || full_cycle_progress_now(shared),
                    budget_follows,
                ) {
                    oome_ladder_step(shared, thread, "full-cycle gave up: budget exhausted");
                    report_forced_cycle_incomplete(shared, started_at, saw_active, lost_races);
                    return false;
                }
                continue;
            }
            g1_concurrent_mark_cycle(shared, thread);
            if shared.mem.heap.g1_is_marking_active() {
                note_mark_door(MarkDoor::ForceFull, MarkDoorOutcome::Started);
            } else {
                // The initial-mark STW lost its race: same stall as above.
                lost_races += 1;
                safepoint_check(shared, thread);
                backoff.snooze();
            }
        }
        if force_full_cycle_out_of_time(
            std::time::Instant::now(),
            &mut deadline,
            hard_deadline,
            &mut seen_progress,
            || full_cycle_progress_now(shared),
            budget_follows,
        ) {
            oome_ladder_step(shared, thread, "full-cycle gave up: budget exhausted");
            report_forced_cycle_incomplete(shared, started_at, saw_active, lost_races);
            return false;
        }
    }
}

/// gci i14/ox: [`g1_force_full_cycle_with`]'s hook call, at most once per
/// cycle call (the hook is taken): `true` = it allocated. `None` (every caller
/// but the interpreter ladder under its opt-in): `false`, nothing called.
pub(in crate::runtime::interpreter::gc_and_alloc) fn fresh_cycle_hook_allocated(
    between: &mut Option<&mut dyn FnMut(&SharedVm, &mut JvmThread) -> bool>,
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> bool {
    match between.take() {
        Some(hook) => hook(shared, thread),
        None => false,
    }
}

/// gci i15/ca (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`
/// item 7's cost half, opt-in `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE`): ONE
/// attempt at a full collection's abort of the open G1 cycle, HotSpot's
/// `G1ConcurrentMark::concurrent_cycle_abort`. [`g1_force_full_cycle_with`]
/// calls it instead of [`g1_final_remark_cleanup`] for a cycle the report half
/// marked aborted, and then drives the fresh cycle, which marks from its own
/// roots: the open cycle's remark (a fixed-point drain, reference processing,
/// class unloading, cleanup) is not paid.
///
/// The pause is the remark's protocol without its work: request, take-over
/// (every mutator parked or frozen, so no store is mid-SATB-log while the
/// queue closes, and the frozen peers' TLAB tails are published as for any
/// pause), then `VmHeap::g1_abort_concurrent_mark` (join the marker, drop the
/// gray set / overlay snapshot / skip set / TAMS rows, close and discard the
/// SATB queue), resume, release. No root is gathered (nothing is marked), no
/// reference is processed (this processor discovers nothing while marking: its
/// rows are the registry, untouched), nothing is freed, no address-keyed table
/// is swept. What may read the aborted cycle's state afterwards, and why none
/// of it acts on the half-built bitmap, is on `G1Collector::abort_concurrent_mark`.
///
/// Reported as HotSpot reports the aborted cycle: the cycle is (re-)marked
/// aborted inside the pause, before the remark-kind timer takes the cycle edge,
/// so the edge books no `G1 Concurrent GC` collection and no JFR pause row, and
/// with its cut stamped prints `Concurrent Mark Abort` / `Concurrent Mark Cycle
/// <ms>` and closes the cycle (`gc_events::g1_cycle_edge_finish`). The
/// `--verbose:gc` pause line says `door=g1-remark` (the timer's kind). Under a
/// full collection's one-report scope the pause folds into it like the remark
/// it replaces. Engagement line under `CRATONVM_DBG_GC_OVERHEAD=1`:
/// `[GC_OVERHEAD] g1-abort-open-cycle: thread=N cycle=C`.
///
/// `Some(true)` = aborted; `Some(false)` = the pause ran but no cycle was
/// marking (another thread's remark closed it first; nothing done, no timer);
/// `None` = lost the STW race, nothing done.
/// r23 w6 (`CRATONVM_G1_STARVED_FULL`, default ON, `=0` kills; G1 only): the
/// mark driver's answer to a futile-young streak (`G1Collector::take_starved_full`)
/// -- HotSpot G1's evacuation failure followed by a `Pause Full (G1 Compaction
/// Pause)`. The open cycle is aborted ([`g1_abort_open_cycle_pause`]; its
/// half-built bitmap is never read), then one pause tenures every live young
/// object and slides every movable Old region in place
/// (`G1Collector::arm_starved_full`; `g1_compact_in_place.rs` marks from the
/// roots itself, so it needs neither the aborted cycle nor a Free to-space).
///
/// Why: `R23W6KeepLong -XX:+UseG1GC -Xmx1g` -- each round's 634 MB of chains
/// is dead in Old when the next round fills the heap. HotSpot meets that heap
/// with one 33 ms `Pause Full` per round (81 rounds in 20 s); here the Free
/// pool sat at 1-3% while about a hundred 2 MB young pauses ran behind a
/// marker they starved (8-9 rounds). With it: 93-109 pauses per 10 rounds of
/// `G1epKeepRounds` against 153-177 (HotSpot 76-79), one starved full per
/// round as HotSpot's one `Pause Full`. A lost STW race (the abort or the
/// pause) returns `false`; the streak then builds again. Nothing here throws
/// or refuses: the allocation ladder is untouched.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_starved_full(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    if g1_abort_open_cycle_pause(shared, thread).is_none() {
        return false;
    }
    if shared.mem.heap.g1_is_marking_active() || !shared.mem.heap.g1_arm_starved_full() {
        return false;
    }
    cratonvm_gc::gc_quiescence::request_system_gc_tenure();
    maybe_gc_forced_at(shared, thread, G1_LAST_DITCH_COMPACTION_SITE);
    let _ = cratonvm_gc::gc_quiescence::take_system_gc_tenure_request();
    true
}

pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_abort_open_cycle_pause(shared: &SharedVm, thread: &mut JvmThread) -> Option<bool> {
    let counted_os_tids = request_non_collection_pause(shared, thread.thread_id)?;
    let mut xt_roots: Vec<ObjectRef> = Vec::new();
    // Armed BEFORE the take-over, as every open-coded pause: an unwind resumes
    // the frozen peers and reopens the world.
    let mut pause_guard = GenConcPauseGuard::arm(shared);
    pause_guard.hold(stw_take_over_and_wait(
        shared,
        &mut xt_roots,
        &counted_os_tids,
    ));
    if !shared.mem.heap.g1_is_marking_active() {
        let taken = pause_guard.release();
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        return Some(false);
    }
    let cycle_id = super::gc_events::g1_mark_open_cycle_aborted(shared);
    let mut pause_timer = non_collection_pause_start(shared, NonCollectionPause::G1Remark);
    // SAFETY (I-17): `stw_take_over_and_wait` above parked every other mutator
    // (or froze it) and `taken` is still held by the guard, so this thread is
    // the only mutator until the release below; the token witnesses the STW
    // phase `g1_abort_concurrent_mark` requires.
    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
    let aborted = shared.mem.heap.g1_abort_concurrent_mark(&stw);
    if aborted {
        // The cycle is closed: its edge prints the abort lines and does not
        // re-arm the mark for a retry.
        super::gc_events::non_collection_pause_cleanup_point(
            &mut pause_timer,
            Some(std::time::Instant::now()),
        );
    }
    non_collection_pause_seal(&mut pause_timer);
    let taken = pause_guard.release();
    retire_skip_spans_and_resume(shared, taken);
    shared
        .mem
        .gc_barrier
        .complete_gc(cratonvm_types::PointerMap::default());
    non_collection_pause_finish(pause_timer);
    if aborted && gc_overhead_dbg_on() {
        let cycle = match cycle_id {
            Some(id) => id.to_string(),
            None => "-".to_string(),
        };
        eprintln!(
            "[GC_OVERHEAD] g1-abort-open-cycle: thread={} cycle={}",
            thread.thread_id.0, cycle
        );
    }
    Some(aborted)
}


/// `CRATONVM_G1_FORCE_FULL_FRESH_CYCLE=0` -- let [`g1_force_full_cycle`] return
/// after finishing a cycle that was already open on entry, without driving one
/// from a fresh snapshot (the behaviour before 2026-09-30). Default on.
///
/// gci i12/sx: read per call (once per forced full cycle, i.e. per
/// `System.gc()` on G1) through `runtime_flag_default_on`, the same value rule
/// as the hand-rolled match it replaces (unset, empty or any other value is
/// on; `0` / `false` / `off` / `no`, trimmed and case-insensitive, is off),
/// with no process `OnceLock` (`vm/tests/per_vm_state_statics_ratchet.rs` -1).
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_force_full_fresh_cycle() -> bool {
    true
}



/// How long [`g1_force_full_cycle`] lets the background marker converge before
/// it runs the final remark anyway (which then finishes the closure inside the
/// pause). One marker poll interval (`g1_concurrent::WORKER_POLL_MS`): a
/// marker that has not converged by then is being fed faster than it drains.
pub(in crate::runtime::interpreter::gc_and_alloc) const FORCE_FULL_QUIESCE_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// Upper bound on one [`g1_force_full_cycle`] call. Since w3-a it is reached
/// only when the call's STW requests keep losing to other initiators.
pub(in crate::runtime::interpreter::gc_and_alloc) const FORCE_FULL_CYCLE_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// JIT round 22 wave 9 (lane memory): the outer bound of one
/// [`g1_force_full_cycle_with`] call when its budget follows progress
/// ([`g1_full_cycle_budget_follows_progress_enabled`]): a livelock guard.
///
/// JIT round 22 wave 10 (lane memory, review of the wave-9 rule): 6 s, not
/// 30. A GENUINE `OutOfMemoryError` on a busy multi-threaded heap is the case
/// that reaches the cap: the other threads keep completing (futile) young
/// pauses, which renew the budget, while this thread's stop-the-world
/// requests keep losing. Each allocation's ladder runs up to two full cycles,
/// so a 30 s cap could delay each thread's throw by a minute. Three renewals
/// cover the loaded-host stall the rule exists for (a few seconds of lost
/// elections) and bound a real OOME to seconds (`R22W10G1Ladder`
/// `oome-threads`).
pub(in crate::runtime::interpreter::gc_and_alloc) const FORCE_FULL_CYCLE_HARD_CAP: std::time::Duration = std::time::Duration::from_secs(6);

/// JIT round 22 wave 9, lane memory (page
/// `docs/feature-designs/jit-ir-optimizations.md`, the w9
/// measurement; `CRATONVM_G1_FULL_CYCLE_BUDGET_FOLLOWS_PROGRESS`, HotSpot
/// parity, default ON, `=0` restores the fixed budget): the
/// [`FORCE_FULL_CYCLE_BUDGET`] of [`g1_force_full_cycle_with`] (the
/// last-ditch / overhead-limit / `System.gc()` full cycle) is renewed each time
/// it runs out while another initiator's collection has COMPLETED and RECLAIMED
/// something (more Free regions or a smaller Old extent; wave 10) since the
/// last renewal, up to [`FORCE_FULL_CYCLE_HARD_CAP`].
///
/// The fixed two seconds is wall time. On a loaded host the cycle's STW
/// requests lose to the other threads' pauses for longer than that while the
/// VM is making progress (every lost race is someone else's completed pause).
/// The call then gave up without a completed mark, so the staged compaction
/// had no trusted liveness to compact on, and the ladder threw
/// `OutOfMemoryError` with the heap full of reclaimable garbage:
/// `R22FlipGcRows names-mark`, about 29 MiB live in 64 MiB, threw under a
/// `cargo test` load and passed on a quiet machine. HotSpot's
/// `VM_G1CollectFull` waits for its safepoint as long as it takes. A call
/// whose races are lost to pauses that complete nothing -- no collection
/// counted -- still stops at the fixed budget, as before. Read once per call.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_cycle_budget_follows_progress_enabled() -> bool {
    true
}

/// What [`force_full_cycle_out_of_time`] compares across a renewal: the
/// collection count, the Free region count and the Old regions' used extent
/// (`G1Collector::region_census().old`), and (JIT round 22 wave 11) the
/// TENURED footprint, Old plus Humongous, in regions and in used bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::gc_and_alloc) struct FullCycleProgress {
    pub(in crate::runtime::interpreter::gc_and_alloc) collections: u64,
    pub(in crate::runtime::interpreter::gc_and_alloc) free_regions: usize,
    pub(in crate::runtime::interpreter::gc_and_alloc) old_bytes: usize,
    pub(in crate::runtime::interpreter::gc_and_alloc) tenured_regions: usize,
    pub(in crate::runtime::interpreter::gc_and_alloc) tenured_bytes: usize,
}

/// `CRATONVM_G1_FULL_CYCLE_PROGRESS_TENURED` (JIT round 22 wave 11, lane
/// review; correctness, default ON, `=0` restores the wave-10 measure): what
/// counts as "a collection reclaimed something" for
/// [`force_full_cycle_out_of_time`].
///
/// The wave-10 measure ("more Free regions, or a smaller Old extent") still
/// renews on the futile young pauses it was written to exclude. A young pause
/// on a genuinely full heap empties Eden INTO the Free pool (its garbage dies,
/// its survivors are promoted), so a snapshot taken just after it reads more
/// Free regions than one taken while Eden was full -- whichever side of the
/// pause the two snapshots fell on decided the renewal, not the heap. Free
/// regions are refilled by Eden at once, and they say nothing about whether the
/// live set shrank. The tenured footprint does: only a collection that dropped
/// Old or Humongous garbage (a mixed, full or remark-driven cycle, a humongous
/// reclaim) shrinks it, and promotion grows it. Renewing only when it shrank,
/// in regions or in bytes, keeps the loaded-host case (another initiator's
/// cycle compacting Old) and drops the young churn. The cost of the rule:
/// a reclaim that promotion outpaces in the same window does not renew, which
/// is the fixed-budget answer, the wave-9 behaviour. Read once per expired
/// budget, so not cached.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_cycle_progress_tenured_enabled() -> bool {
    true
}

/// Did the heap reclaim something between `seen` and `marks`? See
/// [`g1_full_cycle_progress_tenured_enabled`] for the two measures.
pub(in crate::runtime::interpreter::gc_and_alloc) fn full_cycle_reclaimed(seen: &FullCycleProgress, marks: &FullCycleProgress, tenured: bool) -> bool {
    if tenured {
        marks.tenured_regions < seen.tenured_regions || marks.tenured_bytes < seen.tenured_bytes
    } else {
        marks.free_regions > seen.free_regions || marks.old_bytes < seen.old_bytes
    }
}

/// [`FullCycleProgress`] now; zeros off G1 (never reached there).
pub(in crate::runtime::interpreter::gc_and_alloc) fn full_cycle_progress_now(shared: &SharedVm) -> FullCycleProgress {
    let collections = shared.mem.heap.collection_count();
    match &shared.mem.heap {
        _ if shared.mem.heap.is_g1() => {
            let h = &shared.mem.heap;
            // One census walk, so the tenured pair is one snapshot.
            let census = h.region_census().unwrap();
            FullCycleProgress {
                collections,
                free_regions: h.g1_free_region_count().unwrap_or(0),
                old_bytes: census.old.0,
                tenured_regions: census.old.1 + census.humongous.1,
                tenured_bytes: census.old.0 + census.humongous.0,
            }
        }
        _ => FullCycleProgress {
            collections,
            free_regions: 0,
            old_bytes: 0,
            tenured_regions: 0,
            tenured_bytes: 0,
        },
    }
}

/// The budget decision of [`g1_force_full_cycle_with`]: `true` = give up.
/// Before `deadline`: go on (`now_marks` is not even read). Past it, with
/// `follows` and below `hard_deadline`, the budget is renewed from `now` only
/// on PROGRESS since `seen`: a collection completed AND it reclaimed something
/// -- more Free regions, or a smaller Old extent (JIT round 22 wave 10, page
/// `r22-w10-full-cycle-budget-renews-on-any-completed-pause-FIXED-20261020.md`: a
/// moving count alone renewed on the futile pauses of other threads' failing
/// allocations, so a genuine OOME waited out the cap). Otherwise give up.
/// Pure apart from its two out-parameters.
pub(in crate::runtime::interpreter::gc_and_alloc) fn force_full_cycle_out_of_time(
    now: std::time::Instant,
    deadline: &mut std::time::Instant,
    hard_deadline: std::time::Instant,
    seen: &mut FullCycleProgress,
    now_marks: impl FnOnce() -> FullCycleProgress,
    follows: bool,
) -> bool {
    if now < *deadline {
        return false;
    }
    if !follows || now >= hard_deadline {
        return true;
    }
    let marks = now_marks();
    let reclaimed = full_cycle_reclaimed(seen, &marks, g1_full_cycle_progress_tenured_enabled());
    if marks.collections != seen.collections && reclaimed {
        *seen = marks;
        *deadline = (now + FORCE_FULL_CYCLE_BUDGET).min(hard_deadline);
        return false;
    }
    true
}

/// The wait between two turns of [`g1_force_full_cycle`]: a few yields (a
/// quiescence or a competing pause usually resolves within microseconds), then
/// sleeps doubling from 50 µs to 1 ms. Replaces an unbounded `yield_now` spin,
/// which kept a core busy for the whole wait.
#[derive(Default)]
pub(in crate::runtime::interpreter::gc_and_alloc) struct LastDitchBackoff {
    pub(in crate::runtime::interpreter::gc_and_alloc) turns: u32,
}

impl LastDitchBackoff {
    pub(in crate::runtime::interpreter::gc_and_alloc) const YIELD_TURNS: u32 = 8;

    /// The sleep for the given turn, `None` for a yield. Split out for the test.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn pause_for(turns: u32) -> Option<std::time::Duration> {
        if turns <= Self::YIELD_TURNS {
            return None;
        }
        // 50 µs << k, capped at 1 ms.
        let k = (turns - Self::YIELD_TURNS - 1).min(5);
        Some(std::time::Duration::from_micros((50u64 << k).min(1_000)))
    }

    pub(in crate::runtime::interpreter::gc_and_alloc) fn snooze(&mut self) {
        self.turns = self.turns.saturating_add(1);
        match Self::pause_for(self.turns) {
            None => std::thread::yield_now(),
            Some(d) => std::thread::sleep(d),
        }
    }
}

/// A forced G1 cycle that could not be completed within its budget: say so,
/// with the reason, rather than returning as if it had run. Rate-limited — the
/// OOM path can reach this once per failed allocation.
#[cold]
pub(in crate::runtime::interpreter::gc_and_alloc) fn report_forced_cycle_incomplete(
    shared: &SharedVm,
    started_at: std::time::Instant,
    saw_active: bool,
    lost_races: u32,
) {
    static REPORTED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if REPORTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
        let waited_ms = elapsed_ms(started_at);
        let still_marking = shared.mem.heap.g1_is_marking_active();
        let marker_converged = shared.mem.heap.g1_concurrent_mark_finished();
        tracing::warn!(
            target: "cratonvm::gc::guard",
            waited_ms,
            cycle_seen_open = saw_active,
            still_marking,
            marker_converged,
            lost_stw_races = lost_races,
            "G1 last-ditch full cycle could not be completed: its stop-the-world \
             requests kept losing to other initiators; proceeding without it \
             (dead Old/humongous regions stay unreclaimed until a later cycle)"
        );
    }
}

/// Whole milliseconds since `t0`, saturating.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn elapsed_ms(t0: std::time::Instant) -> u64 {
    u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)
}


/// DBG (CRATONVM_DBG_DM): trace every stage of the direct-memory
/// reclamation chain -- Cleaner discovery, action emission, the drain, and
/// `Bits.reserveMemory`'s reclaim-and-retry. Default-off; when unset this is
/// one cached `OnceLock` read, NOT a per-call `runtime_var_os` (that takes
/// the process-env lock, and this sits on the post-GC path).
pub(in crate::runtime::interpreter) fn dm_dbg_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| false)}

/// `run_cleaner_actions` entries that got past the exclusion switches.
pub(in crate::runtime::interpreter::gc_and_alloc) static DM_CLEANER_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// ...of those, the ones that bailed because a JIT borrow was live.
pub(in crate::runtime::interpreter::gc_and_alloc) static DM_CLEANER_JIT_BLOCKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// ...the ones that reached the drain and found nothing queued.
pub(in crate::runtime::interpreter::gc_and_alloc) static DM_CLEANER_EMPTY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Cleaner actions actually run.
pub(in crate::runtime::interpreter::gc_and_alloc) static DM_CLEANER_DRAINED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// gcg g3/rw (`common-w16c-proposal-post-gc-native-cleanup-hook`, the
/// two-hunk minimum; opt-in `CRATONVM_GC_POST_GC_CLOSE_ORPHAN_FDS`): close the
/// file descriptors the collection epilogue's weak-row sweep
/// (`cratonvm_native_io::gc_sweep_io_side_tables`: a dead `DatagramChannel`'s
/// socket, a dead synthetic `RandomAccessFile`'s file) parked for this VM.
/// Without it they wait for the next native of the same family
/// (`native_dc_open` / `native_dc_close`, the synthetic-RAF constructors /
/// `close`), so a VM that stops opening channels keeps them until teardown;
/// HotSpot closes them from its `Cleaner` thread shortly after the collection.
///
/// Runs where the post-collection Cleaner drain runs ([`run_cleaner_actions`],
/// [`run_cleaner_actions_forced`]): a mutator past its poll, never inside the
/// pause, with no collector lock held. No Java runs and nothing is allocated,
/// so neither a live JIT borrow nor the reference-delivery thread matters
/// here. Closing needs only the VM's fd table: a second close is a no-op and a
/// reusable id is closed only under its parked generation, so a race with the
/// family natives' own drain is harmless. The switch is read per call; the callers run once per collection
/// or per raised reference-work hint, not per allocation.
///
/// gcg g5/rs (vg3 verdict): default ON, `=0` kills it. `rw3_orphanfd_on_gen`
/// and `_on_compat` read `bound=0` during the hold (HotSpot `hs_drop`: 0), the
/// `keep` control `bound=1` (HotSpot 1), the old default `bound=1`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn close_orphan_fds_after_gc(shared: &SharedVm) {
    // JIT r23 wave 3 review: a reusable (OS-keyed) id is closed only while it
    // still has the generation it was parked with (`close_orphan_fd`).
    for orphan in cratonvm_native_io::take_orphan_fds(shared.vm_identity) {
        cratonvm_native_io::close_orphan_fd(&shared.natives.fd_table, orphan);
    }
}



/// Drain pending Cleaner actions, deferring if a JIT borrow is live.
///
/// This is the ordinary-GC entry point (`maybe_gc`). It keeps the original
/// conservative behaviour: `maybe_gc` is reachable through
/// `jit_invoke_dispatch` -> `bail_to_interpreter` -> interpreter, and there the
/// interpreter's `&mut JvmThread` is itself fabricated from the JIT TLS, so a
/// cleaner's `run()` re-entering JIT would be a genuine *sibling* aliasing
/// borrow. That is the shape behind the avrora `FileCleanable` crash the guard
/// was added for. Actions left queued here are GC-relocated by
/// `update_after_gc` and run at the next non-JIT safepoint.
pub(in crate::runtime::interpreter) fn run_cleaner_actions(shared: &SharedVm, thread: &mut JvmThread) {
    // gcg g3/rw: first, the fds the weak-row sweep parked (opt-in; no Java).
    close_orphan_fds_after_gc(shared);
    // gc-common w3-d: with the reference-delivery thread on, the allocating
    // mutator runs nothing (JLS §12.6 / the `Cleaner` contract) — see
    // `finalizer_thread_takes_delivery`.
    if finalizer_thread_takes_delivery(shared, thread) {
        return;
    }
    run_cleaner_actions_impl(shared, thread, false);
}

/// Drain pending Cleaner actions even when a JIT borrow is live.
///
/// Called ONLY from `force_gc_from_native`, i.e. from inside a native method
/// invocation, where `thread` is the native dispatcher's own legitimate
/// `&mut JvmThread` rather than one derived from `jit_thread_mut`.
///
/// Deferring on that path is useless: its callers -- `System.gc()` and
/// `Bits.reserveMemory`'s reclaim-and-retry -- have no later safepoint to wait
/// for. `Bits.reserveMemory` throws `OutOfMemoryError: Direct buffer memory` as
/// soon as this returns having freed nothing. Measured on H2's
/// `org.h2.test.store.TestMVStore`: 156 cleaner actions queued and every drain
/// attempt refused, because the allocating thread is a `FileStore` writer-pool
/// worker running compiled code, so the guard held every time it mattered.
///
/// Running them is made safe the same way the interpreter re-enters JIT from a
/// bail: `set_jit_thread` opens a nested scope, so the cleaner's `run()` gets a
/// *child reborrow* of `thread` rather than an aliasing sibling, and
/// `restore_jit_thread` re-installs the outer level on the way out.
pub(in crate::runtime::interpreter) fn run_cleaner_actions_forced(shared: &SharedVm, thread: &mut JvmThread) {
    // gcg g3/rw: first, the fds the weak-row sweep parked (opt-in; no Java).
    close_orphan_fds_after_gc(shared);
    // gc-common w3-d: hand the batch to the reference-delivery thread and wait
    // for it (bounded), so `Bits.reserveMemory` still sees the native memory
    // freed when `System.gc()` returns. A timeout falls through to the inline
    // drain below. See `finalizer_thread_drained_for`.
    if finalizer_thread_drained_for(shared, thread) {
        return;
    }
    // RAII so an unwinding cleaner action cannot leave the outer JIT level
    // un-restored.
    struct NestedJitScope(Option<crate::jit::helpers::JitThreadScope>);
    impl Drop for NestedJitScope {
        fn drop(&mut self) {
            if let Some(scope) = self.0.take() {
                crate::jit::helpers::restore_jit_thread(scope);
            }
        }
    }
    let _scope = NestedJitScope(if crate::jit::helpers::is_jit_thread_set() {
        Some(crate::jit::helpers::set_jit_thread(thread))
    } else {
        None
    });
    run_cleaner_actions_impl(shared, thread, true);
}


/// gci i9/ng (i8/pe finding B): the reason code `stw_take_over_and_wait` puts
/// in the first-wins scalar when it refuses coverage for un-rewritable peer
/// state without a labelled door. A taken-over peer outside the precise-native
/// set is the take-over; otherwise the refusal came from helper windows,
/// pinned native claims among them (`TakenOver::pinned_native_claim_count`
/// is counted "like a pinned blocked helper window").
pub(in crate::runtime::interpreter::gc_and_alloc) fn takeover_refusal_label(unrewritable_taken: usize) -> usize {
    use cratonvm_gc::gc_quiescence::incomplete_reason as r;
    if unrewritable_taken > 0 {
        r::XT_TAKEOVER
    } else {
        r::XT_HELPER_WINDOW
    }
}

