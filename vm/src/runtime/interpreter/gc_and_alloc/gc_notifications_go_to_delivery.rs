#[allow(unused_imports)]
use super::*;


/// Current rule (2026-10-19): both runtime modes deliver notifications off the
/// mutator under the default policy. Otherwise a lock-free interpreter drain
/// can steal a set-time pool sensor request after its setter has handed it to
/// the worker. GC records and pool sensors share the same one-drainer queue;
/// every mutator door must therefore leave that queue to its delivery thread.
/// The explicit inline diagnostic policy and no-owner fallback still apply.
/// Finalizer and cleaner policies are independent and are not changed here.
///
/// # Historical routing (before 2026-10-19)
///
/// Do GC notifications (JMX `NotificationListener`s on a GC bean) belong to the
/// reference-delivery thread rather than to the calling door? gc-common w4-a
/// (`handoff-w3d-gc-notifications-on-the-delivery-thread`,
/// `common-w3d-gc-notifications-run-on-the-allocating-mutator`, FIXED in w7-d).
///
/// `false` on the `=0` bisection arm ([`GcNotificationDelivery::Inline`]), so
/// the door keeps its inline delivery byte-for-byte; a `--compatible` VM under
/// the default policy takes the lock-held rule below (`# --compatible`).
/// Otherwise:
///
/// * nothing queued: `false` (the door's `run_gc_notifications` is one load);
/// * the caller IS the delivery thread (a finalizer or listener allocated and
///   collected): `true` — its own batch ends with `run_gc_notifications`, after
///   the finalizer that triggered this collection has returned;
/// * otherwise the delivery thread is woken, or STARTED for the notifications
///   alone (`wake_finalizer_thread` starts one only for finalizer / cleaner
///   work, so a notification-only collection used to fall back to the inline
///   path); `true` unless no thread could be started (a unit fixture with no
///   owning `Arc`), when the door keeps the inline delivery.
///
/// HotSpot delivers these on its `Notification Thread`, asynchronously for
/// every cause including `System.gc()`, so the forced door skipping them is the
/// HotSpot behaviour, not a weakened guarantee.
///
/// # `--real-jdk` (gc-common w6-d, 2026-09-24)
///
/// A `--real-jdk` VM takes this path under the default policy too
/// ([`gc_notification_delivery`]). That is a bug fix, not a policy
/// preference: a `NotificationListener` is application code, and run inline it
/// executes in the middle of whichever `new` tripped the collection — on that
/// thread, holding that thread's monitors (so a listener that synchronizes on
/// one of them RE-ENTERS it, mid-critical-section, which is the exact hazard
/// JLS §12.6 rules out for finalizers), with that thread's `ThreadLocal`s,
/// context class loader and interrupt status, and able to throw into nothing.
/// HotSpot never does that: its listeners run on the `Notification Thread`.
/// A strict VM already starts the delivery thread for its queue wake-ups
/// (w5-d), so this adds no thread a strict VM with GC listeners would not
/// have.
///
/// # `--compatible` (gc-common w7-d, 2026-09-24)
///
/// A `--compatible` VM under the default policy hands its notifications over
/// exactly when the collecting thread holds a user-visible lock (a monitor or
/// an `AbstractOwnableSynchronizer`, [`thread_holds_user_locks`]) — the line
/// w4-d drew for `finalize()` under the same policy, for the same reason. That
/// is the case in which the inline delivery is a defect rather than a
/// difference of thread identity:
///
/// * **re-entry.** Monitors are re-entrant, so a listener that synchronizes on
///   a lock the allocating thread holds ENTERS it, half-way through that
///   thread's critical section, and sees whatever invariant the section had
///   broken (measured: 16 of 16 deliveries in `GcNotificationThreadProbe`);
/// * **deadlocks HotSpot cannot have.** A listener that blocks on a lock held
///   by a thread which is itself waiting for a lock the allocating thread
///   holds deadlocks the allocator inside its own `new`. On HotSpot the
///   listener waits on the `Notification Thread` and the allocator proceeds.
///
/// A lock-free collecting thread keeps the inline delivery byte for byte:
/// what differs there (the thread name, its `ThreadLocal`s and context class
/// loader) is not a correctness hazard and moves only with the full hand-off
/// (`docs/feature-designs/real-jdk-mode.md`). The
/// hand-off is marked for the delivery thread
/// (`FinalizerThread::note_gc_notifications_handed_off`) so its batch
/// delivers ONLY what a lock-holding door handed it, never what a lock-free
/// door is about to deliver itself. Nothing waits for it, `System.gc()`
/// included: HotSpot's delivery is asynchronous for every cause, and a wait
/// would stall a lock-holding `System.gc()` whenever the listener needs a
/// `java.util.concurrent` lock the caller holds (undetectable, so the full
/// 2-s bound). The kill switch is the existing inline arm,
/// `CRATONVM_FINALIZER_THREAD=0` ([`GcNotificationDelivery::Inline`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_notifications_go_to_delivery_thread(shared: &SharedVm, thread: &JvmThread) -> bool {
    let rule = gc_notification_delivery(shared);
    if rule == GcNotificationDelivery::Inline {
        return false;
    }
    if !super::gc_events::gc_notifications_pending(shared) {
        return false;
    }
    if rule == GcNotificationDelivery::WhenLockHeld {
        return gc_notifications_handed_off_under_lock(shared, thread);
    }
    if shared.mem.finalizer_thread.delivery_thread() == Some(thread.thread_id.0) {
        return true;
    }
    if wake_finalizer_thread(shared) {
        return true;
    }
    // Not claimed and no finalizer / cleaner work: start it for the
    // notifications. `start_finalizer_thread` requests a batch before the
    // thread exists, so its first batch delivers them. (Under the default
    // policy `wake_finalizer_thread` always declines, and this is also the
    // request: an already-claimed thread gets a `request_delivery`.)
    start_finalizer_thread(shared)
}

/// On every collector, hand a pool-threshold sensor request that
/// `MemoryPoolMXBean.setUsageThreshold` just left pending to the delivery
/// thread, so it is delivered without waiting for this thread's next
/// allocation door -- as HotSpot's Service Thread delivers it (local 25.0.3:
/// within milliseconds, the setting thread asleep). The caller is a JMX
/// native on a thread inside the synchronized `setUsageThreshold`, i.e. one
/// holding a user-visible lock, so the `--compatible` arm hands off as
/// [`gc_notifications_handed_off_under_lock`] does (marked, then requested);
/// the `--real-jdk` arm always delivers off the mutator. `false` (nothing
/// done) under the inline policy (`CRATONVM_FINALIZER_THREAD=0`) or when no
/// delivery thread could be started; the caller's drain hint then stands.
pub(crate) fn hand_set_time_threshold_crossing_to_delivery_thread(shared: &SharedVm) -> bool {
    match gc_notification_delivery(shared) {
        GcNotificationDelivery::Inline => false,
        GcNotificationDelivery::OffMutator => request_finalizer_thread(shared, true),
        GcNotificationDelivery::WhenLockHeld => {
            let ft = &shared.mem.finalizer_thread;
            ft.note_gc_notifications_handed_off();
            if request_finalizer_thread(shared, true) {
                true
            } else {
                let _ = ft.take_gc_notifications_handed_off();
                false
            }
        }
    }
}

/// The [`GcNotificationDelivery::WhenLockHeld`] arm of
/// [`gc_notifications_go_to_delivery_thread`] (a `--compatible` VM under the
/// default policy), with notifications queued: `true` when `thread` holds a
/// user-visible lock and the delivery thread takes them (started on first
/// use), so the door must not run the listeners itself. The caller being the
/// delivery thread (a finalizer or listener that allocated while holding a
/// lock) also answers `true`: its own batch delivers them once that code has
/// returned. `false` keeps the door's inline delivery: no lock held, or no
/// thread could be started (a unit fixture with no owning `Arc`).
/// gc-common w7-d (2026-09-24).
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_notifications_handed_off_under_lock(shared: &SharedVm, thread: &JvmThread) -> bool {
    if !thread_holds_user_locks(shared, thread) {
        return false;
    }
    hand_gc_notifications_to_delivery_thread(shared, thread)
}

/// The hand-off of [`gc_notifications_handed_off_under_lock`] once the door
/// has decided to hand off (a lock-holding door, or since gci i47w/nt47 the
/// metadata door, which can never deliver inline): mark the notifications for
/// the delivery thread's `WhenLockHeld` batch and request it (started on
/// first use). `true` when a delivery thread takes them; `false` -- mark
/// withdrawn -- when none could be started.
pub(in crate::runtime::interpreter::gc_and_alloc) fn hand_gc_notifications_to_delivery_thread(shared: &SharedVm, thread: &JvmThread) -> bool {
    let ft = &shared.mem.finalizer_thread;
    // Marked BEFORE the request, so the batch opened for it sees the mark
    // (see `FinalizerThread::note_gc_notifications_handed_off`).
    ft.note_gc_notifications_handed_off();
    if ft.delivery_thread() == Some(thread.thread_id.0) {
        return true;
    }
    if request_finalizer_thread(shared, true) {
        return true;
    }
    // Nobody will take them: deliver inline, as before, and leave no mark for
    // a later batch to act on.
    let _ = ft.take_gc_notifications_handed_off();
    false
}

/// Who delivers one VM's GC notifications. gc-common w6-d / w7-d; see
/// [`gc_notification_delivery_for`] for the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::gc_and_alloc) enum GcNotificationDelivery {
    /// The door that collected, always (`CRATONVM_FINALIZER_THREAD=0`, the
    /// bisection arm and the kill switch of both fixes).
    Inline,
    /// The delivery thread when the collecting thread holds a user-visible
    /// lock, the door otherwise (a `--compatible` VM under the default
    /// policy, gc-common w7-d).
    WhenLockHeld,
    /// The delivery thread, always (the full hand-off, or a `--real-jdk` VM
    /// under the default policy, gc-common w6-d).
    OffMutator,
}

/// This VM's [`GcNotificationDelivery`]. Per VM, from `SharedVm::config`,
/// never a process flag.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_notification_delivery(shared: &SharedVm) -> GcNotificationDelivery {
    gc_notification_delivery_for(finalizer_delivery_policy(), shared.config.is_jdk_only())
}

/// Notifications use HotSpot's asynchronous delivery in both runtime modes.
/// The finalizer policy's lock-held default does not make notifications safe
/// to run inline: their shared queue can contain a set-time sensor handoff.
/// Only the explicit inline diagnostic policy disables asynchronous delivery.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_notification_delivery_for(
    policy: FinalizerDelivery,
    _jdk_only: bool,
) -> GcNotificationDelivery {
    match policy {
        FinalizerDelivery::Always | FinalizerDelivery::WhenLockHeld => {
            GcNotificationDelivery::OffMutator
        }
        FinalizerDelivery::Inline => GcNotificationDelivery::Inline,
    }
}

/// Run the finalizers and Cleaner actions a collection on ANOTHER door queued,
/// from `maybe_gc`'s no-collection path (gc-common w3-a, 2026-09-23).
///
/// Every collection enqueues its dead finalizable objects
/// (`run_collection_pause`), but only a door that may run Java afterwards
/// drains them, and the allocation-failure door (`maybe_gc_forced_at` — every
/// JIT allocation helper, `alloc_object_shared`, `gc_alloc_array`, the TLAB
/// refill wedge) may not: its callers hold unrooted references in Rust locals.
/// So a program whose collections were all allocation failures ran no
/// `finalize()` until an interpreted allocation happened to trip `needs_gc`
/// itself, and `finalizable_roots` meanwhile rooted the whole queue on every
/// cycle (`docs/feature-designs/real-jdk-mode.md`).
///
/// `maybe_gc` is called after every interpreted allocation with the new object
/// already rooted, and its collecting path already runs both drains at this
/// very point, so running them here is no new upcall site. The check is one
/// relaxed load of a flag on its own cache line; a raised flag is lowered
/// BEFORE the drain (a collection that queues more afterwards raises it
/// again). Not while a JIT helper holds the thread borrow: the guarded drains
/// would decline, and lowering the hint then would lose it.
///
/// This narrows the page, it does not close it: a program that never executes
/// an interpreted allocation (a fully compiled loop) still reaches no drain.
///
/// # With the reference-delivery thread (gc-common w4-a)
///
/// With `CRATONVM_FINALIZER_THREAD=1` this path must never run `finalize()` or
/// a cleaner action on the allocating mutator (JLS §12.6 — the hazard that
/// thread exists to remove). It hands the work to that thread instead: the
/// wake is a counter bump and a notify, no Java runs here, so a live JIT
/// borrow is no reason to wait and the hint is consumed at once. (Before w4-a
/// the inline drain was already declined inside `run_finalizers` /
/// `run_cleaner_actions`, but only when no JIT borrow was live; with one, the
/// hint stayed raised and nothing was woken from here.) Only when no delivery
/// thread could be started does it fall back to the inline drain below.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn drain_ref_work_queued_elsewhere(shared: &SharedVm, thread: &mut JvmThread) {
    if !shared.mem.gc_barrier.ref_work_queued() {
        return;
    }
    drain_ref_work_queued_elsewhere_cold(shared, thread);
}

/// The raised-hint half of [`drain_ref_work_queued_elsewhere`], out of line so
/// the per-allocation check stays one load.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::gc_and_alloc) fn drain_ref_work_queued_elsewhere_cold(shared: &SharedVm, thread: &mut JvmThread) {
    // Checks the flag first (latched), so flag-off behaviour is unchanged.
    // A woken batch ends with `run_gc_notifications`, so queued notifications
    // ride on it.
    if finalizer_thread_takes_delivery(shared, thread) {
        let _ = shared.mem.gc_barrier.take_ref_work_queued();
        return;
    }
    if crate::jit::helpers::is_jit_thread_set() {
        // zt t12/gn4: the hint stays raised for the finalizer and Cleaner
        // drains, as before; a `--compatible` ZGC VM's queued notifications
        // go to the delivery thread now rather than wait for a drain point
        // outside the borrow (one load when none is queued).
        let _ = zgc_undeliverable_door_hands_off_gc_notifications(shared, thread);
        return;
    }
    if shared.mem.gc_barrier.take_ref_work_queued() {
        run_finalizers(shared, thread);
        run_cleaner_actions(shared, thread);
        // gc-common w4-a: and the GC notifications an allocation-FAILURE
        // collection queued (`run_collection_pause` raises the hint for them
        // too) — that door records them but cannot deliver.
        if !gc_notifications_go_to_delivery_thread(shared, thread) {
            run_gc_notifications(shared, thread, false);
        }
    }
}

/// Run ONE collection pause from this thread, or take part in the one already
/// in flight.
///
/// Returns `Some` iff this thread initiated and completed a collection (the
/// world is running again). `None` means another thread's pause was already
/// requested: this thread has taken part in it (`safepoint_check`) and
/// collected nothing itself. The [`CollectedPause`] carries what the door
/// needs sampled INSIDE the pause (gc-common w5-a).
///
/// The caller owns what is specific to its door: before the call, TLAB retire,
/// SATB drain and any major-GC request or productivity baseline; after it, the
/// finalizer / cleaner drains, the concurrent-cycle tick and productivity
/// accounting.
///
/// # One door (gc-common w2-a, 2026-09-23)
///
/// `maybe_gc`, `maybe_gc_forced_at` and `force_gc_from_native` each open-coded
/// this pause twice — a single-threaded arm and a multi-threaded arm, six
/// copies — and every per-collection obligation added over two months landed
/// on a subset of them
/// (`docs/feature-designs/real-jdk-mode.md`
/// has the table). Folding them here fixes, by construction:
///
/// * **Dead finalizers on every door.** The collector's second tuple element,
///   the post-collection addresses of the dead finalizable objects it
///   resurrected, was consumed only by `System.gc()`. The four
///   allocation-driven arms dropped it, so every dead finalizable object was
///   resurrected — with its whole subtree — on every allocation-triggered
///   collection and `finalize()` never ran until something called
///   `System.gc()`. Now every collection hands it to
///   [`enqueue_resurrected_finalizers`].
/// * **The finalizable-root snapshot is taken inside the pause.** `System.gc()`
///   took it before its request, so a finalizable object registered and
///   dropped by another mutator in between was freed without `finalize()`.
/// * **Every collection holds the STW barrier.** The `alive_count <= 1` arms
///   collected with no request at all, a decision made from a racy snapshot:
///   a thread that registered during the collection (a JNI
///   `AttachCurrentThread`, a VM service thread) passed `mark_stw_ready` and
///   ran Java code against a heap being relocated. The census inside the
///   request is now the single decision; with one thread it computes
///   `expected = 0` and the wait returns at once, and a thread registering
///   meanwhile waits the pause out at `mark_stw_ready` or its first poll.
/// * **The coverage cycle opens with the WINNING request only**
///   ([`GcBarrier::request_stw_opening_cycle`]), and this thread publishes its
///   own roots after that — neither a losing initiator nor a sibling that
///   reaches the request second can erase another pause's verdicts.
/// * `ec_watch` remap, JFR / `-Xlog:gc` accounting and the cycle ordinal
///   (`gc_event_finish`) on every door.
///
/// The post-GC debug verifier set that only `maybe_gc`'s single-threaded arm
/// ran now runs on every pause (gc-common w7-g, `common-e-small-findings`
/// item 1): its heap walkers under [`debug_heap_walks_permitted`], the rest
/// unconditionally.
///
/// [`GcBarrier::request_stw_opening_cycle`]: crate::threading::gc_barrier::GcBarrier::request_stw_opening_cycle
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_collection_pause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    door: GcDoor,
) -> Option<CollectedPause> {
    // gcj j1pn ledger item 3: FIRST, the collector-entry capture of this
    // frame -- every frame of the pause is a callee, below its SP, so this
    // thread's young pin deposits skip them (`CollectorEntry`).
    let _collector_entry = crate::jit::conservative_roots::enter_collector_entry();
    // gcj pcp: this VM's compile workers start no compile until the pause
    // returns (`CRATONVM_JIT_COMPILE_YIELDS_TO_GC`).
    let _jit_yields = shared.jit.tiered_manager.gc_pause_scope();
    // gcj j1pn raw-write item 7: the pause's raw write-back audit lines carry
    // this VM's identity.
    let _audit_vm = cratonvm_gc::root_write_audit::VmTagScope::enter(shared.vm_identity as u64);
    // DBG (CRATONVM_DBG_MTROOTS) reason code: 1=System.gc, 2=alloc-young,
    // 3=forced-alloc — the numbering `mtroots_set_gc_ctx` documents.
    let mtroots_reason: u8 = match door {
        GcDoor::SystemGc | GcDoor::MetadataThreshold | GcDoor::DiagnosticCommand => 1,
        GcDoor::HeapInspection | GcDoor::HeapDump | GcDoor::JvmtiForceGc => 1,
        // zt t31/x10: G1's periodic collection, a VM-requested cycle.
        GcDoor::G1PeriodicCollection | GcDoor::G1PeriodicFull => 1,
        GcDoor::AllocationThreshold => 2,
        // gci i11/go: the G1 recovery compaction is a forced allocation pause.
        GcDoor::AllocationFailure | GcDoor::G1CompactionPause => 3,
        GcDoor::G1HumongousAllocation => 3,
    };
    // zt t31/x10 (`CRATONVM_GC_FULL_GC_HEAP_DUMPS`): on Serial an allocation
    // pause may run the old generation's collection too -- HotSpot's full
    // collection, which `-XX:+HeapDumpBeforeFullGC` / `AfterFullGC` bracket.
    // Before: when that collection is already due (a major request, or the
    // old generation's own trigger before this pause promotes anything);
    // after: when it ran. The explicit doors dump in `force_full_collection`.
    let gen_alloc_full_dump = shared.mem.heap.is_generational()
        && matches!(door, GcDoor::AllocationThreshold | GcDoor::AllocationFailure)
        && (full_gc_heap_dump_flag_on(shared, true) || full_gc_heap_dump_flag_on(shared, false));
    if gen_alloc_full_dump {
        if cratonvm_gc::gc_quiescence::major_gc_requested() || shared.mem.heap.old_gen_needs_gc() {
            full_gc_heap_dump(shared, thread, true, full_gc_dump_gc_id(shared, true));
        }
        owe_full_gc_heap_dump_after(shared, thread);
    }
    // zs s9/no2 (opt-in): a ZGC allocation-driven collection on a heap the
    // last cycle left nearly full clears every soft reference, as HotSpot's
    // ZGC stalled majors do there; see `zgc_near_full_soft_clear`.
    // (Merge of origin/dev, 2026-10-18: named as the two allocation doors, so
    // dev's explicit doors -- `jcmd GC.run`, the heap inspection and dump,
    // JVMTI `ForceGarbageCollection` -- stay out as `System.gc()` does; the
    // G1 doors never reach a ZGC heap.)
    if matches!(
        door,
        GcDoor::AllocationThreshold | GcDoor::AllocationFailure
    ) {
        zgc_near_full_soft_clear(shared);
    }
    // zt t6/ob4 (`zgc-zs3ac-collection-cpu-residual-shares` item 2): on ZGC,
    // open the VM's span of this pause before the safepoint request, so the
    // rendezvous and the common epilogue are booked too
    // (`CRATONVM_ZGC_VM_PAUSE_CPU`, `vm_pause_ms=`). `None` elsewhere.
    let vm_pause_cpu = shared.mem.heap.zgc_vm_pause_cpu_start();
    let initiator = thread.thread_id;
    // xt-hardening (2026-07-03): the counted alive set's OS tids are
    // snapshotted atomically with the expected computation (same closure,
    // same barrier lock) for identity-based takeover excusal.
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let mut census_alive: usize = 0;
    let mut census_initiator_counted: Option<bool> = None;
    // gcj j3/rt3 (`CRATONVM_GEN_STW_HOLD_SEES_SIBLING_PAUSE`, opt-in,
    // Generational only; residuals page item 7): an ALLOCATION request whose
    // fairness hold let a sibling's whole collection run loses, like HotSpot's
    // skipped `VM_CollectForAllocation`; explicit doors (`System.gc()`, jcmd,
    // JVMTI, heap dump / inspection) always collect.
    let sibling_rule = shared.mem.gc_barrier.hold_sees_sibling_pause()
        && shared.mem.heap.is_generational()
        && matches!(
            door,
            GcDoor::AllocationThreshold | GcDoor::AllocationFailure
        );
    let won = shared.mem.gc_barrier.request_stw_opening_cycle_for_allocation(
        initiator,
        sibling_rule,
        || {
            // gc-common w3-a: the census also says whether THIS thread owns a
            // counted slot (by its own id, or by OS thread for a virtual thread
            // on a counted carrier), so the barrier's `- 1` is taken only when
            // it is true — `common-a-initiator-assumed-in-census`.
            let (n, blocked, tids, blocked_tids, initiator_counted) = shared
                .threads
                .thread_registry
                .alive_count_blocked_and_os_tids_for(initiator);
            // DIAGNOSTIC (2026-07-13, STW takeover 5-class cluster
            // investigation): print the EXACT identity set counted as
            // "expected" (alive AND NOT in_blocked_region) at the instant this
            // pause is requested, to disambiguate whether a thread later seen
            // parked was already excluded at request time or genuinely raced
            // in. (Was on `maybe_gc`'s multi-threaded arm only.)
            // gcj j2/cm2: the per-VM latch, not an environment read under `inner`.
            if shared.threads.thread_registry.dbg_stw_expected_ids() {
                let expected_ids: Vec<u64> = shared
                    .threads
                    .thread_registry
                    .alive_thread_ids_excluding(&blocked_tids);
                eprintln!(
                    "[stw-expected] initiator={} n={} blocked={} expected_ids={:?}",
                    initiator.0, n, blocked, expected_ids
                );
            }
            counted_os_tids = tids;
            census_alive = n;
            census_initiator_counted = initiator_counted;
            crate::threading::gc_barrier::StwCensus {
                alive: u32::try_from(n).unwrap_or(u32::MAX),
                blocked: u32::try_from(blocked).unwrap_or(u32::MAX),
                blocked_tids,
                initiator_counted,
                // A collection never excuses a running native: it holds raw
                // object references a moving collector cannot rewrite.
                excused_running: Vec::new(),
            }
        },
        // Opened ONLY if this request wins, under the barrier lock, before any
        // peer can observe the pause — see `request_stw_opening_cycle`.
        cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle,
    );
    if !won {
        // Another thread is already collecting — just take part.
        safepoint_check(shared, thread);
        return None;
    }
    // Round 12 wave 1 (lock W19-1): see `request_non_collection_pause`.
    shared.threads.monitors.wake_lazy_parkers();
    // Interpreter round i3 wave 72, lane L6 (`CRATONVM_PARK_LAZY_STW_WAKE`).
    shared.threads.thread_registry.wake_lazy_park_phases();
    // Publish this thread's own roots AFTER the coverage cycle opened, so its
    // own verdicts (a conservative JIT scan bump, an incomplete-coverage mark)
    // are this pause's, not erased by it. Only its own collection reads them,
    // and the peers are not released until `complete_gc`.
    update_root_snapshot(shared, thread);
    mtroots_set_gc_ctx(shared, thread, mtroots_reason);
    mtroots_dump_initiator(shared, thread, mtroots_reason);

    // DBG (bc math-ec, CRATONVM_DBG_ECWATCH): detect watched-cell corruption
    // at GC ENTRY (addresses still valid, pre-relocation). Fires even if the
    // corruptor was NOT dispatched via `safe_native_call` (e.g. an inline
    // interpreter intrinsic), confirming the watch machinery works and that
    // a watched EC field really did flip to a small value before this GC.
    if crate::runtime::ec_watch::enabled() {
        let watched = crate::runtime::ec_watch::size(shared.vm_identity);
        let gc_hits = crate::runtime::ec_watch::detect(shared.vm_identity);
        if !gc_hits.is_empty() {
            eprintln!(
                "[ecwatch-GC] {} CORRUPTED-at-GC of {} watched cells:",
                gc_hits.len(),
                watched,
            );
            for (holder, idx, expected, now) in gc_hits {
                eprintln!(
                    "[ecwatch-GC]   holder@0x{holder:x} fld[{idx}]: 0x{expected:x} -> 0x{now:x}"
                );
            }
        } else if watched > 0 {
            eprintln!("[ecwatch-GC] {watched} watched cells, all clean at this GC");
        }
    }

    // BUG-03 — forcibly stop in-JIT peers and conservatively scan them before
    // waiting for the cooperative mutators. With no peer (the census counted
    // only this thread) the wait returns at once.
    let mut xt_roots: Vec<ObjectRef> = Vec::new();
    // JIT round 19 residual (`r19-an-unwind-out-of-a-pause-leaks-its-lazy-native-claims-FIXED-20261007`):
    // armed BEFORE the take-over, disarmed once the world is released.
    let unwind_abort = MovingPauseUnwindAbort::arm();
    let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
    // gcj j2/jn2 (`CRATONVM_JNI_IN_NATIVE_KEEP_TLAB`, opt-in, Generational
    // only): the TLABs JNI in-native threads kept, retired on their behalf
    // now that the barrier is satisfied. One latched load with the switch off.
    retire_in_native_kept_tlabs(shared);
    // zt t24/zc8 (`CRATONVM_ZGC_MARK_CONTINUE`, opt-in; stage 1f, HotSpot's
    // Concurrent Mark Continue): an allocation-door collection that closes an
    // every-trigger ZGC cycle first tries a Pause Mark End -- before this
    // pause's events, root gather or reference pass, so a failed attempt
    // leaves nothing of a collection behind -- and, when mark work is left,
    // releases the world without collecting (a pause of its own) and the pool
    // continues. One thread-local load when no door opened it.
    if let Some(release) = zgc_mark_end_attempt(shared, thread, door, &xt_roots) {
        zgc_release_mark_end_attempt(shared, taken, unwind_abort, release, vm_pause_cpu);
        return None;
    }
    // No counted PEER: the census counted nobody, or exactly one thread that
    // is this one. `census_alive <= 1` alone called a pause "solo" when the one
    // counted thread was somebody else (an initiator outside the census —
    // `initiator_counted == Some(false)`), and ran the solo-only debug heap
    // walkers with a parked or frozen peer.
    //
    // gc-common w4-a: `Some(true)` exactly, not "not `Some(false)`". `None`
    // means the census could not tell — the initiator's own id is not counted
    // and some counted entry has no published OS id — so the one counted
    // thread may well be a peer; the solo-only walkers must not run then. The
    // census answers `Some(true)` by id before it consults OS ids, so a truly
    // single-threaded pause still reads solo on every platform.
    let solo = census_alive == 0 || (census_alive == 1 && census_initiator_counted == Some(true));

    // DBG: the PRE-collection half of the heap-stale verifier. Its whole
    // point is to be paired with the post-GC call further down — see
    // `verify_heap_object_fields_pre_gc`: without it a stale field cannot
    // be dated, because the semispaces alternate and the post-GC pass
    // reports the same field on every second cycle forever. Inside the pause
    // now (it used to walk the heap before the request, with peers running).
    //
    // gc-common w7-g: under the same walkability rule as the POST half
    // (`debug_heap_walks_permitted`). It ran unconditionally, so on a
    // multi-threaded pause it linearly walked a frozen or blocked peer's
    // unretired TLAB tail -- the very walk the POST half was gated off for.
    if debug_heap_walks_permitted(shared, solo) {
        crate::memory::gc::verify_heap_object_fields_pre_gc(shared);
    }

    let mut gc_event = gc_event_start(shared);
    // gcj j1yg-pause (`CRATONVM_DBG_GCPAUSE`): the pause the `-Xlog:gc` line
    // reports, split at the points below and printed as one `[vmpause]` line
    // at the seal -- the collector's own `[gcpause]` line covers only the
    // `collect` row.
    let mut vm_pause_marks: Option<(std::time::Instant, Vec<(&'static str, u64)>)> =
        cratonvm_types::flags().gc.dbg_gcpause.then(|| (std::time::Instant::now(), Vec::new()));
    let mut vm_pause_mark = |name: &'static str| {
        if let Some((t, marks)) = vm_pause_marks.as_mut() {
            let now = std::time::Instant::now();
            marks.push((name, now.duration_since(*t).as_micros() as u64));
            *t = now;
        }
    };
    // zt t16/pl7: HotSpot's per-collection TLAB statistics for the
    // `sun.gc.tlab.*` perf counters, with the world stopped (the point
    // `gc_event_start` takes the `TLAB totals` line at). One relaxed load
    // unless this VM's image has those counters.
    crate::runtime::perf_data_file::publish_tlab_stats(shared);
    // gci i41/rs: G1's full-compaction pause count before this pause collects
    // (`None` off G1), read with the world stopped; see the Survivor stamp
    // after `gc_event_seal` below.
    let g1_full_compactions_before = g1_full_compaction_pause_count(shared);
    // gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`, default ON since 2026-10-17): ask the
    // registry gather below for its old-only section. Off, or a heap that
    // cannot prove this pause young-only for those roots: nothing armed. The
    // guard disarms on drop (an unwind must not leave the arm to another
    // door's gather, e.g. a concurrent cycle's initial mark).
    let _parked_split_arm = arm_parked_old_only_gather(shared);
    // gci i14/ve (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED`, default ON since 2026-10-17): on a licensed
    // Generational pause the same gather keys deposited JDK continuations by
    // their mirrors (no root; their frames are `metadata_pin` rows owned by
    // the mirror); `update_all_roots` retires the ones whose mirror died.
    // Disarm-on-drop, like the split's guard.
    let _parked_keyed_arm = arm_parked_mirror_keyed_gather(shared);
    // gci i21/pr (`CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`, opt-in): the same
    // gather leaves out this thread's own snapshot, which `collect_roots`
    // step 11 pushes. Disarm-on-drop, like the two guards above.
    let _initiator_skip_arm = arm_initiator_snapshot_skip(shared, thread);
    // Collect roots: current thread + all snapshots + shared state.
    //
    // gc-common w6-a: `collect_roots_registry_appended` because the registry's
    // snapshots are appended just below -- they push every alive thread's
    // mirror, which `collect_roots` step 10b would otherwise push again
    // (`common-b-proposal-dedupe-the-initiator-root-set.md`).
    let mut roots = crate::memory::roots::collect_roots_registry_appended(shared, thread);
    let initiator_root_count = roots.len(); // gen r4w6/oldpin6, see its publication below
    let mut snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
    // gch h4/bp: the compiled bands JNI in-native threads left to the pause
    // (off unless `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`).
    append_in_native_band_roots(shared, &mut snapshot_roots);
    // gci i13/vg: the gather's old-only section goes to the heap, which adds
    // it back to every phase that marks or moves old space.
    stage_parked_old_only_roots(shared);
    // gen r4w6/oomjit6: where each part of the vector starts, for the old-gen
    // root census's index sections (`oldmark_census_stage_sections`).
    let census_collect_end = roots.len();
    roots.extend(snapshot_roots);
    let census_snapshot_end = roots.len();
    // `CRATONVM_DBG_ROOT_REMAP_AUDIT`: the list the collector is about to
    // mark from, so a post-GC verifier can say whether a slot it found
    // naming a reclaimed object was ever in it. After the registry append
    // (w6-a): the thread mirrors now arrive only through it, and the parked
    // peers' snapshots are marked from too.
    crate::memory::gc::note_root_set(&roots);
    vm_pause_mark("roots");
    // INT-3 (G1) — everything a frozen peer can address must not
    // move; must follow collect_roots (which clears the pins).
    pin_frozen_peer_roots_for_g1(shared, &xt_roots, &taken);
    // BUG-03 — conservative roots from forcibly-stopped in-JIT peers.
    roots.extend(xt_roots);
    // `CRATONVM_DBG_ZGC_WILD`: which part of this vector a root index belongs to,
    // and the thread and frame slot of every parked thread's snapshot root that
    // is not an object start -- the halves the collector's `[ZGC-WILD] ...
    // edge=root[i]` line cannot name on its own.
    // One latch with the collector's `[ZGC-WILD]` reader, read by value
    // (zt t6/rv3: this copy read presence, so `=0` turned it on).
    if cratonvm_gc::zgc::dbg_zgc_wild() {
        eprintln!(
            "[ZGC-WILD-SECTIONS] initiator=0..{census_collect_end} snapshots=..{census_snapshot_end} \
             frozen_jit=..{} thread={:?}",
            roots.len(),
            std::thread::current().name()
        );
        shared
            .threads
            .thread_registry
            .dbg_wild_snapshot_roots(&|a| shared.mem.heap.is_object_address(a).is_some());
    }

    // STW invariant: the barrier's quota is met, so every other mutator has
    // parked at its safepoint poll OR (BUG-03) been forcibly stopped in JIT
    // and conservatively scanned.
    // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
    // zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`, opt-in): or hide them from
    // the collection's markers instead. Timed for the VM reference round's
    // census (`zgc_note_vm_reference_round` below; two clock reads).
    let refs_pre_t0 = std::time::Instant::now();
    weakref_null_referents_pre_gc(shared);
    let refs_pre_us = refs_pre_t0.elapsed().as_micros() as u64;
    // zr z5/rv5: on ZGC's last-ditch clear-all collection, ask the collector
    // which condemned soft referents the precise roots reach on their own
    // (`zgc_arm_soft_precise_probe_for_pause`).
    zgc_arm_soft_precise_probe_for_pause(shared, &roots, census_collect_end);
    // zt t11/gn3 (`CRATONVM_ZGC_GEN_YOUNG_JNI_WEAK_STRONG`, default ON since
    // zt t14/gn5, `=0` kill): on a ZGC generational young cycle that keeps its referents, the JNI weak
    // globals' referents are roots too. Appended last (after the frozen-JIT
    // roots): nothing reads this vector back by index. Empty on every other
    // collector and collection.
    roots.extend(zgc_young_cycle_jni_weak_roots(shared));
    // gc-common w8-c: watch the JNI weak globals' referents too, so the
    // post-collection verdict below is exact on a cycle that reclaims old-gen
    // storage. AFTER `weakref_null_referents_pre_gc`, which replaces the set.
    // zr z1/rf: not on a ZGC heap, which never reads the set
    // (`watched_set_unread`).
    if !crate::runtime::interpreter::watched_set_unread(shared) {
        crate::native::jni::watch_weak_global_referents(shared);
    }
    // SAFETY: the STW invariant stated just above holds — every other
    // mutator is parked at a safepoint (or was forcibly stopped and
    // conservatively scanned), so this thread is the only mutator and
    // may assert exclusive heap access.
    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
    // Snapshotted INSIDE the pause, immediately before the collection: a
    // finalizable object another mutator registered (and dropped) after an
    // earlier snapshot would otherwise be freed instead of resurrected, and
    // its `finalize()` would never run (`handoff-d-force-gc-finalizable-roots-after-stw`).
    //
    // gcj j3/x2 R1 (`CRATONVM_GC_QUEUED_FINALIZABLES_ARE_ROOTS`): the queued
    // ones are strong roots, pushed before the root layout is published; the
    // candidates are the registered set (`pause_finalizable_inputs`).
    let (fin_roots, queued_fin_roots) = pause_finalizable_inputs(shared);
    roots.extend(queued_fin_roots);
    // gci i16/fr (`CRATONVM_STW_UNDISCOVERED_REFS_STRONG`, opt-in): a
    // `Reference` only a dead finalizable object reaches keeps its referent,
    // as HotSpot's keep-alive phase keeps it. Inactive, `fin_candidates` is
    // `&fin_roots`. See `StwUndiscoveredProbe`.
    // gci i48/rf48: also engaged, by default, on a G1 full collection's pauses.
    let probe_engaged = || stw_probe_engaged(shared, door);
    let stw_probe = StwUndiscoveredProbe::before_collection_if(shared, &fin_roots, &probe_engaged);
    let fin_candidates = stw_probe.collector_candidates(&fin_roots);
    publish_old_pin_root_layout_for_pause(shared, thread, initiator_root_count, roots.len());
    // JIT r23 w1 L7 (`CRATONVM_DBG=root-kind-census`, diagnostic, off by
    // default): which root kind keeps how many bytes reachable, read before
    // the collection moves anything (`memory::root_kind_census`).
    root_kind_census_for_pause(
        shared,
        door,
        &roots,
        (census_collect_end, census_snapshot_end),
        taken.census_helper_from().map(|h| census_snapshot_end + h),
        fin_candidates,
        solo,
    );
    vm_pause_mark("pre_collect");
    let (result, dead_finalizers) = {
        // gen r4w6/oomjit6: `CRATONVM_DBG=oldmark-root-census` only (both are
        // `None` otherwise). The sections name every root by the part of this
        // door that pushed it; the door scope lets the census's VM labeller
        // read this thread and this VM for the length of the collection.
        // Scoped to the call so `thread` is not written while the labeller may
        // hold a pointer to it (`OldmarkCensusDoor::enter`'s contract).
        let _census_sections = oldmark_census_stage_sections(
            census_collect_end,
            census_snapshot_end,
            taken.census_helper_from().map(|h| census_snapshot_end + h),
        );
        let _census_door = crate::jit::helpers::OldmarkCensusDoor::enter(shared, thread);
        // HIB-CV-24 (ZGC manifestation): arm the class-unload reconciliation
        // watch with this VM's current defining-loader addresses BEFORE the
        // collection, so ZGC can capture their exact pre-slide mark-bitmap
        // verdict (see `zgc_capture_reconcile_watch`) instead of leaving
        // `process_references_after_gc` to judge them post-slide via the
        // compaction-aliasable `is_addr_live`. No-op on Generational/G1.
        shared.mem.heap.zgc_set_reconcile_watch(
            cratonvm_native_builtins::classloader::watched_defining_loader_addrs(
                &shared.loader_identity,
            ),
        );
        // zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`, opt-in): this door runs
        // a parked sweep after its release (below), so the collection may
        // park one. A no-op off ZGC and with the switch off.
        shared.mem.heap.zgc_arm_concurrent_sweep_runner();
        shared.mem.heap.collect_garbage_with_finalizers(
            &stw,
            &mut roots,
            fin_candidates,
            &shared.threads.monitors,
        )
    };
    // zt t22/zc3: the collection took the hidden rows; forget them anyway, so
    // no later mark start can read a stale address. A no-op off ZGC.
    vm_pause_mark("collect");
    shared.mem.heap.zgc_clear_stw_reference_skip();
    let reconcile_exact_results = shared.mem.heap.zgc_take_reconcile_results();
    cratonvm_gc::gen_heap::clear_old_pin_root_layout();
    // gc-common w6-d: soft/weak processing must not count the objects this
    // collection marked only to run their `finalize()` (HotSpot clears a weak
    // reference to a finalizable object before finalizing it).
    note_resurrected_finalizables_for_reference_processing(
        shared,
        fin_candidates,
        &result.pointer_map,
        &dead_finalizers,
    );
    // gci i16/fr: the probe referents are not finalizable -- out of the list
    // before anything enqueues it; the probe is noted for the round below.
    let dead_finalizers = stw_probe.after_collection(shared, &result.pointer_map, dead_finalizers);
    // gci i22/dr (P2/dr, `gcd-d10r-proposal-one-relocation-target-set-per-pause-20260928`):
    // the pause's ONE relocation-destination set, lent to every in-pause
    // in-place verdict below (`process_references_after_gc`, the JNI
    // weak-global sweep, `update_all_roots`), which used to build a private
    // copy each. Lazy as before: built at the first address that asks, never
    // on G1, on a young-only Generational cycle or for an empty map. Dropped
    // after `update_all_roots`, before `complete_gc` takes the map.
    //
    // gci i28/wt (`gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`):
    // it also carries the pause's G1 kept-young-region verdict, taken ONCE
    // here for every address a weak consumer of this epilogue will ask about
    // (the reference rows, the JNI weak globals), so the reference round, the
    // JNI weak-global sweep and every side-table sweep that takes
    // `InPlaceVerdict::survived` refuse the same unheld objects. Empty (one
    // load) off G1 and on every pause that kept no young region.
    //
    // gci i31/gq (`gci-i30rp-g1-dead-finalizable-in-a-kept-young-region-is-not-finalized-at-the-pause-FIXED-20261016`):
    // the same verdict names the dead registered finalizables of those
    // regions; they join this pause's `dead_finalizers` (unchanged addresses)
    // and are queued below with the collection's own. Empty off G1.
    let mut g1_kept_dead_finalizables: Vec<usize> = Vec::new();
    let relocation_targets = crate::memory::addr_keyed::RelocationTargets::for_pause(
        &shared.mem.heap,
        &result.pointer_map,
    )
    .with_g1_unheld(g1_pinned_young_referents_unheld(
        shared,
        &result.pointer_map,
        &roots,
        &mut g1_kept_dead_finalizables,
    ));
    let mut dead_finalizers = dead_finalizers;
    dead_finalizers.extend(g1_kept_dead_finalizables);
    vm_pause_mark("post_collect");
    let refs_post_t0 = std::time::Instant::now();
    // zt t31/zc11 (program page 1c step 2c, `CRATONVM_ZGC_CONCURRENT_REF_ROUND`):
    // on a ZGC cycle that parked its sweep, only the round's head runs here;
    // the round runs after the release, before the sweep's install (below).
    let round_after_release = zgc_reference_round_after_release(shared, &result.pointer_map);
    if round_after_release {
        if process_references_head(
            shared,
            &result.pointer_map,
            &roots,
            reconcile_exact_results.as_ref(),
            &relocation_targets,
        ) {
            absorb_zgc_discovered_weak_phantom(shared, &result.pointer_map);
        }
    } else {
        // zt t32 (`CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): the round runs in
        // this pause, so a referent clear the collection staged for after the
        // release runs here first (a no-op when nothing is staged).
        shared.mem.heap.zgc_run_staged_referent_clear();
        process_references_after_gc(
            shared,
            &result.pointer_map,
            &roots,
            reconcile_exact_results.as_ref(),
            &relocation_targets,
        );
    }
    let refs_post_us = refs_post_t0.elapsed().as_micros() as u64;
    let refs_jni_t0 = std::time::Instant::now();
    // gc-common w8-c: JNI weak global refs are not roots. Remap the ones whose
    // referent survived and clear the ones whose referent died, at the same
    // decision point as `java.lang.ref` clearing: after the collector's
    // resurrection of dead finalizable objects (a resurrected referent keeps
    // its weak global, the JNI phantom strength), with `result.pointer_map`
    // still in hand and the world still stopped.
    {
        // gc-common w18-a: one reclaimed-hole verdict for the whole sweep.
        // gcd d10/r (applied by d10/o): with the pause's relocations (a weak
        // global whose referent died at a base a slide re-issued must read
        // NULL). gci i22/dr: the pause's one shared set.
        let in_place = crate::memory::addr_keyed::InPlaceVerdict::capture_with(
            &shared.mem.heap,
            &relocation_targets,
        );
        let _ =
            crate::native::jni::sweep_weak_global_refs_with(shared, &result.pointer_map, &|addr| {
                in_place.survived(addr)
            });
    }
    // zt t22/zc3 (item 1, the census of stage 1c): this pause's VM reference
    // round -- the pre-collection pass, `process_references_after_gc` and the
    // JNI weak-global sweep -- which the heap's `refs_us=` never saw. Per
    // collection `[GC] zgc-vm-refs:` under `--verbose:gc`; `[GC]
    // zgc-concurrent-refs:` at exit. A no-op off ZGC.
    let refs_jni_us = refs_jni_t0.elapsed().as_micros() as u64;
    // gcj j1yg-pause round 3: the rows the pre-collection pass set aside
    // (`stash_settled_old_rows`) go back now, after every reference pass of
    // this pause and before anything outside it can read the lists.
    shared
        .mem
        .ref_processor
        .lock()
        .restore_settled_old_rows_after(
            shared.mem.heap.g1_old_collecting_pauses().unwrap_or(0),
            &result.pointer_map,
        );
    vm_pause_mark("refs");
    // zt t31/zc11: a round after the release is noted when it ends.
    if !round_after_release {
        shared
            .mem
            .heap
            .zgc_note_vm_reference_round(refs_pre_us, refs_post_us, refs_jni_us);
    }
    // gc-common w5-a: finalizers an EARLIER collection queued are still
    // waiting, and no door has taken the ref-work hint for whole pauses — the
    // fully-compiled-program case. Read BEFORE this collection adds its own
    // (every drain empties the queue, so a non-empty queue here survived a
    // whole inter-collection interval). Two loads unless stale; acted on after
    // the release. See `hand_undrained_finalizers_to_delivery_thread`.
    let finalizers_undrained = shared
        .mem
        .gc_barrier
        .ref_work_stale(UNDRAINED_REF_WORK_PAUSES)
        && shared.mem.finalizer_thread.pending_count() > 0;
    // AFTER `process_references_after_gc`: that pass ends with the processor's
    // `update_after_gc`, so its rows already hold the post-collection
    // addresses this list is in. See `enqueue_resurrected_finalizers`.
    enqueue_resurrected_finalizers(shared, &dead_finalizers);
    // gci i31/gq (G-2, default-ON `CRATONVM_G1_SYSTEM_GC_DEFERS_QUEUED_FINALIZABLES`):
    // a G1 full-collection door's young pause carries what it just queued to
    // the stop-the-world mark of the same full collection. No-op otherwise.
    g1_carry_door_queued_finalizables(shared, door, &dead_finalizers);
    // gci i48/rf48 (default path of the G-9 switch): and the `Reference`s its
    // round left undiscovered, to the remark of the same full collection.
    g1_carry_door_undiscovered_refs(shared, door, &result.pointer_map);
    // gc-common w3-a: tell the doors that can run Java but did not collect that
    // there is a queue to drain. Only some doors drain after their own pause
    // (`maybe_gc`, `System.gc()`); the allocation-FAILURE door cannot, and a
    // program whose collections are all allocation failures (a compiled
    // allocation loop) otherwise left every finalizer it queued waiting for a
    // collection on another door. Two mutex reads per COLLECTION, never per
    // allocation. See `drain_ref_work_queued_elsewhere`.
    if shared.mem.finalizer_thread.pending_count() > 0
        || shared.mem.cleaner_thread.pending_count() > 0
    {
        shared.mem.gc_barrier.note_ref_work_queued();
    }

    // Update shared VM state (statics, string pool, etc.)
    vm_pause_mark("finalizers");
    update_all_roots(shared, thread, &result.pointer_map, &relocation_targets);
    vm_pause_mark("update_roots");
    // gci i22/dr: the last borrower of the pause's destination set.
    drop(relocation_targets);
    // DBG (bc math-ec, CRATONVM_DBG_ECWATCH): the moving collector relocated
    // survivors — REMAP each watched holder through the pointer_map so
    // watches PERSIST across this GC (the corruption frequently hits an
    // object that survived the GC that wrote it). Every door, both arms.
    crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
    // gc-common w7-g (`common-e-small-findings` item 1): on EVERY pause, not
    // only a solo one. The instruments that walk no heap run always; the two
    // heap walkers run whenever the heap is linearly walkable, which the
    // census's thread count never measured -- see `debug_heap_walks_permitted`.
    post_gc_debug_verifiers(
        shared,
        thread,
        &result.pointer_map,
        debug_heap_walks_permitted(shared, solo),
    );

    if taken.native_claim_count() > 0
        && false
    {
        let relocated = result
            .pointer_map
            .iter()
            .filter(|(from, to)| from != to)
            .count();
        eprintln!(
            "[ffm-lazy] collection claims={} frozen={} relocated={relocated} verdict={:?}",
            taken.native_claim_count(),
            taken.unrewritable_count(),
            cratonvm_gc::gc_quiescence::takeover_verdict()
        );
    }

    tracing::debug!(
        "GC completed ({:?}, {} counted threads): {} objects copied, {} bytes freed",
        door,
        census_alive,
        result.stats.objects_copied,
        result.stats.bytes_freed,
    );

    // BUG-03 — drop the TLAB skip regions and resume the forcibly-stopped
    // in-JIT peers now that the heap is consistent again (non-moving sweep on
    // Generational / pinned-in-place regions + walker-skipped TLAB tails on
    // G1 (INT-3) → their pointers are unchanged). xt-hardening (2026-07-03):
    // BOTH must happen BEFORE `complete_gc` reopens the world — a released
    // mutator could otherwise win the NEXT STW, re-freeze the still-suspended
    // peers and publish fresh skip regions that THIS initiator's late clear
    // would wipe, letting the next sweep walk (and free-list) the frozen
    // peers' reserved tails. A resumed peer that immediately requests the
    // next GC blocks until `complete_gc` anyway (the barrier is still closed
    // here), so the reorder introduces no new window.
    //
    // gc-common w5-a: the collection's END is stamped here, with the world
    // still stopped (`gc_event_seal`): from the next line on, the resumed
    // in-JIT peers and then the released mutators allocate, and the
    // `-Xlog:gc` / JFR after-heap figure and every duration must not include
    // that (`common-w4f-gc-event-durations-sampled-after-release`).
    vm_pause_mark("post_update");
    if let Some((_, marks)) = vm_pause_marks.take() {
        let total: u64 = marks.iter().map(|&(_, us)| us).sum();
        let mut line = format!("[vmpause] total={:.3}ms ", total as f64 / 1000.0);
        for (name, us) in &marks {
            line.push_str(&format!(" {name}={:.3}ms", *us as f64 / 1000.0));
        }
        {
            let rp = shared.mem.ref_processor.lock();
            line.push_str(&format!(
                "  ref_rows aside={} weak={} soft={} phantom={} cleaner={}",
                rp.settled_old_last().map_or_else(|| "-".to_string(), |n| n.to_string()),
                rp.weak_ref_count(),
                rp.soft_ref_count(),
                rp.phantom_ref_count(),
                rp.cleaner_ref_count(),
            ));
        }
        eprintln!("{line}");
    }
    gc_event_seal(shared, &mut gc_event);
    // gci i41/rs (`gci-i39rv-waves-i35-i38-review-residuals-FIXED-20261016` item 1):
    // a G1 full-collection door's full-compaction pause stamps the Survivor
    // pool as HotSpot's full collection leaves it HERE, with the world still
    // stopped, instead of `force_full_collection` after the release.
    if g1_door_compaction_pause_stamps_survivor(
        g1_full_compactions_since(shared, g1_full_compactions_before),
        door,
        shared.config.explicit_gc,
        g1_full_door_survivor_stamp_in_pause_enabled,
    ) {
        super::gc_events::g1_full_door_compaction_stamps_the_survivor(shared, &gc_event);
    }
    // gc-common w7-g (`common-e-small-findings` item 7): count a completed
    // `System.gc()`-door collection BEFORE the release, so a `System.gc()`
    // caller that lost this pause's request and took part in it reads the
    // bump when `complete_gc`'s generation release wakes it, and coalesces
    // (`force_gc_from_native`, `system_gc_completed_since`). gci i2/oo: the
    // `jcmd GC.run` door is the same full collection, and its requester
    // (`request_diagnostic_command_gc`) waits for this count to move. gci
    // i9/dc: and the live-only attach walks' full collections
    // (`run_dcmd_full_gc_on_attach_thread`), which a `System.gc()` may
    // coalesce against as HotSpot's `skip_operation` does on any full count.
    if matches!(
        door,
        GcDoor::SystemGc | GcDoor::MetadataThreshold | GcDoor::DiagnosticCommand
            | GcDoor::HeapInspection
            | GcDoor::HeapDump
            | GcDoor::JvmtiForceGc
    ) {
        system_gc_collections(shared).fetch_add(1, std::sync::atomic::Ordering::Release);
    }
    // gc-common w5-a: the allocation-failure door's productivity figures,
    // sampled here for the same reason -- read after the release they
    // included whatever the released mutators allocated (TLAB refills first),
    // understating what this collection freed and feeding the GC-overhead
    // streak toward a spurious `OutOfMemoryError` on a busy multi-threaded
    // heap. See `note_gc_productivity`. gci i2/oo: the two allocation
    // counters and the pause key are sealed here too, for the opt-in
    // sealed verdict window (`note_gc_productivity_for_pause`).
    // gci i8/gs: and the occupancy verdicts `maybe_gc_forced_collected`
    // labels `AllocationThreshold` -- see `door_samples_productivity`.
    let mut productivity_after =
        door_samples_productivity(door).then(|| SealedProductivity::sample(shared));
    // zt t31/zc11 (2c): the round's locks, the last thing taken in the pause
    // (rank order, `reference_round_prep`) and held through the release, so no
    // row joins the processor and no class is defined before the round; and
    // the parked sweep's install held until the round ends. Nothing from here
    // to the release takes a ranked lock.
    let round_prep = round_after_release.then(|| {
        shared.mem.heap.zgc_hold_sweep_install();
        (
            ZgcSweepInstallHold(shared),
            reference_round_prep(shared, &result.pointer_map),
        )
    });
    retire_skip_spans_and_resume(shared, taken);
    // zr z4/rv4: the stale-request epoch, BEFORE the release (see
    // `TlabWedgeState::collections_sealed`). Every collector bumps it, and
    // (gci i48/zl2) every collector's stamps read it.
    shared
        .mem
        .tlab_wedge
        .collections_sealed
        .fetch_add(1, std::sync::atomic::Ordering::Release);
    // zt t33: every word the pause healed or stored since its remap flip
    // carries the mark color (so R6's plan needs no live-set walk for it);
    // the mutators store the good color again.
    shared.mem.heap.zgc_end_closing_heal_color();
    // Signal all threads with the pointer map
    shared.mem.gc_barrier.complete_gc(result.pointer_map);
    unwind_abort.disarm();
    // zt t31/zc11 (2c): the reference round, with the mutators running and
    // the mark bits still the pause's (the install is held), then the install
    // may run. No pointer moved: the round's map and relocation set are empty.
    if let Some((hold, prep)) = round_prep {
        let t0 = std::time::Instant::now();
        // zt t32 (steps 2b / 2d, `CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): the
        // referent clear the collection staged, with the mutators running
        // behind the read gate (`ref_discovery::gate_referent_read`); the
        // round then finds every slot as the pause's clear left it.
        shared.mem.heap.zgc_run_staged_referent_clear();
        let no_moves = cratonvm_types::PointerMap::default();
        let targets =
            crate::memory::addr_keyed::RelocationTargets::for_pause(&shared.mem.heap, &no_moves);
        reference_round(shared, &no_moves, &targets, prep, true);
        drop(targets);
        let round_us = t0.elapsed().as_micros() as u64;
        hold.release(round_us.max(1));
        shared.mem.heap.zgc_note_vm_reference_round_after_release(
            refs_pre_us,
            refs_post_us,
            refs_jni_us,
            round_us,
        );
        if shared.mem.finalizer_thread.pending_count() > 0
            || shared.mem.cleaner_thread.pending_count() > 0
        {
            shared.mem.gc_barrier.note_ref_work_queued();
        }
    }
    // zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`, opt-in): the sweep the
    // collection parked runs HERE, on this thread, with every other mutator
    // running -- the cycle's concurrent sweep. It runs no Java and waits for
    // no safepoint (a pause requested meanwhile waits for this thread as for
    // any thread in VM code). Before the events below, so the `-Xlog:gc` line,
    // the JFR heap summary, the JMX collection usage and the allocation
    // door's productivity verdict all describe the swept heap, as HotSpot
    // reports a cycle at its end. `false` (one load) when nothing was parked.
    // zt t32 (R6, `CRATONVM_ZGC_CONCURRENT_RELOCATE`): the planned objects no
    // barrier copied yet, with every other mutator running, before the parked
    // sweep (it frees nothing in the plan's regions, which are off the free
    // list). Runs no Java and waits for no safepoint, as the sweep below.
    // `false` (one lock) when nothing is planned.
    let conc_reloc_cpu0 = shared.mem.heap.zgc_thread_cpu_nanos();
    let _ = shared.mem.heap.zgc_run_concurrent_relocate_drain();
    let sweep_ran = shared.mem.heap.zgc_run_pending_sweep_after_release();
    let conc_sweep_cpu_ns = match (conc_reloc_cpu0, shared.mem.heap.zgc_thread_cpu_nanos()) {
        (Some(a), Some(b)) => b.saturating_sub(a),
        _ => 0,
    };
    if sweep_ran {
        super::gc_events::gc_event_reseal_after_concurrent_sweep(shared, &mut gc_event);
        if let (Some(p), Some(live)) = (
            productivity_after.as_mut(),
            shared.mem.heap.zgc_concurrent_sweep_last_live(),
        ) {
            p.live = live;
        }
    }
    // T19.3.G1 cycle counter, the JFR GC events and the `-Xlog:gc` line — one
    // call, shared by every door. See `gc_event_finish`.
    // gci i20/ob: the requesting thread is `jdk.ExecuteVMOperation.caller`
    // (resolved only while a jdk.jfr backend recording runs).
    gc_event_finish_by(shared, door, gc_event, Some(thread.thread_id));
    // gcj j1/st item 3: the String table's concurrent work a collection's
    // notification owed (HotSpot's service thread: its lines follow the
    // pause's `-Xlog:gc` line). One load when nothing is owed.
    let conc_str_cpu0 = shared.mem.heap.zgc_thread_cpu_nanos();
    crate::memory::string_table::hotspot_concurrent_work(shared);
    let conc_str_cpu_ns = match (conc_str_cpu0, shared.mem.heap.zgc_thread_cpu_nanos()) {
        (Some(a), Some(b)) => b.saturating_sub(a),
        _ => 0,
    };
    // zt t6/ob4: close the VM's span (request, rendezvous, `collect_garbage`,
    // epilogue, event emission), excluding concurrent post-release work.
    if let Some(start) = vm_pause_cpu {
        let excluded = conc_sweep_cpu_ns.saturating_add(conc_str_cpu_ns);
        shared.mem.heap.zgc_book_vm_pause_cpu((start.0.saturating_add(excluded), start.1));
    }
    // zt t11/pl3: on ZGC, the in-process perf counters show this pause before
    // the wake-up below (`CRATONVM_ZGC_PERF_PAUSE_REFRESH`), as HotSpot
    // updates them at the pause. A no-op on every other collector. zt t12/x9:
    // not before every delivery -- the notifications are queued above, after
    // `complete_gc`, and a delivery thread a hint already woke (this pause's
    // finalizer hint, an earlier one) can deliver them first; see
    // `zgc-zt12x9-wave-t11-review-residuals-SUPERSEDED-20261019.md`. zt t13/x10: when a
    // listener is waiting, `gc_events::record_gc_notification` refreshes them
    // before it queues (`CRATONVM_ZGC_PERF_REFRESH_BEFORE_ENQUEUE`); this
    // refresh stays for every pause.
    crate::runtime::perf_data_file::refresh_after_pause(shared);
    // gc-common w4-a: `gc_event_finish` queues this collection's JMX GC
    // notifications on every door, but only `maybe_gc` and `System.gc()`
    // deliver them; the allocation-FAILURE door cannot run Java. Raise the same
    // hint as for queued finalizers so `maybe_gc`'s no-collection path (or the
    // reference-delivery thread it wakes) delivers them, instead of leaving them
    // queued until a collection on another door. One load when nothing queued.
    if super::gc_events::gc_notifications_pending(shared) {
        shared.mem.gc_barrier.note_ref_work_queued();
    }
    if finalizers_undrained {
        hand_undrained_finalizers_to_delivery_thread(shared);
    }
    // gci i14/ve: the execution states of the parked continuations this
    // pause found unreachable (their entries were retired inside it); dropped
    // now the world is released. Empty unless `CRATONVM_GC_VT_PARKED_MIRROR_KEYED`.
    let collected = shared.threads.thread_registry.take_collected_parked();
    if !collected.is_empty() {
        crate::vm::jdk_continuation::retire_collected_continuations(shared, collected);
    }
    // zt t31/x10: Serial's allocation pause ran the old generation's
    // collection (`gc_event_shape`'s `Pause Full`): the after-dump.
    if gen_alloc_full_dump {
        settle_full_gc_heap_dump_after(
            shared,
            thread,
            cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle(),
            full_gc_dump_gc_id(shared, false),
        );
    }
    Some(CollectedPause { productivity_after })
}

/// A collection [`run_collection_pause`] initiated and completed (gc-common
/// w5-a).
#[derive(Clone, Copy, Debug)]
pub(in crate::runtime::interpreter::gc_and_alloc) struct CollectedPause {
    /// The productivity figures sampled INSIDE the pause, after the
    /// collection and before any mutator resumed. Only the allocation-failure
    /// door asks for them (its productivity metric, [`note_gc_productivity`]);
    /// `None` on the other doors.
    pub(in crate::runtime::interpreter::gc_and_alloc) productivity_after: Option<SealedProductivity>,
}

/// What [`run_collection_pause`] samples for the allocation-failure door's
/// verdict while the world is still stopped (gc-common w5-a; gci i2/oo added
/// the allocation counters and the pause key, read only under
/// `CRATONVM_GC_OVERHEAD_SEALED_WINDOW`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter) struct SealedProductivity {
    /// `live_bytes_estimate` after the collection.
    pub(in crate::runtime::interpreter::gc_and_alloc) live: usize,
    /// `bytes_promoted_total` after the collection.
    pub(in crate::runtime::interpreter::gc_and_alloc) promoted: u64,
    /// `HeapRealm::bytes_allocated_total` at the seal.
    pub(in crate::runtime::interpreter::gc_and_alloc) bytes_allocated: u64,
    /// `HeapRealm::thread_allocated_total` at the seal.
    pub(in crate::runtime::interpreter::gc_and_alloc) thread_allocated: u64,
    /// `gc_generation + 1` read inside the pause: unique per pause, and a
    /// later pause has a larger key (the barrier bumps the generation at
    /// every release).
    pub(in crate::runtime::interpreter::gc_and_alloc) pause_key: u64,
}

impl SealedProductivity {
    /// Sample every figure now; called inside the pause.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn sample(shared: &SharedVm) -> Self {
        use std::sync::atomic::Ordering;
        Self {
            live: shared.mem.heap.live_bytes_estimate(),
            promoted: shared.mem.heap.bytes_promoted_total(),
            bytes_allocated: shared.mem.bytes_allocated_total.load(Ordering::Relaxed),
            thread_allocated: shared.mem.thread_allocated_total.load(Ordering::Relaxed),
            pause_key: shared
                .mem
                .gc_barrier
                .gc_generation
                .load(Ordering::Acquire)
                .saturating_add(1),
        }
    }
}

/// How many completed pauses the ref-work hint must survive untaken, with
/// finalizers still queued from before the latest collection, before
/// [`hand_undrained_finalizers_to_delivery_thread`] acts. Two, not one: a
/// `finalize()` that allocates can collect in the middle of the inline drain
/// that is emptying the queue, and that nested collection sees the previous
/// pause's hint and a non-empty queue; needing a second whole pause keeps an
/// interpreted program's inline drain its own.
pub(in crate::runtime::interpreter::gc_and_alloc) const UNDRAINED_REF_WORK_PAUSES: u64 = 2;

/// gen r4w6/oomjit6 (2026-09-24): stage the index sections of the root vector
/// `run_collection_pause` built, for `CRATONVM_DBG=oldmark-root-census`
/// (`gc/src/gen_heap_oldmark_census.rs`). `None`, and nothing computed,
/// without the token.
///
/// The vector is `collect_roots` (`[0, collect_end)`), then every alive
/// thread's registry snapshot (`[collect_end, snapshot_end)`, this thread's
/// own deposit included), then the forcibly-stopped in-JIT peers'
/// conservative scan (`[snapshot_end, ..)`). The first part is split per
/// `collect_roots` step when `roots::scan_section_of` has marks for it; those
/// are recorded only under `CRATONVM_DBG_ROOT_REMAP_AUDIT` today, and the
/// census page asks for them to be recorded under the census token too.
///
/// gci i4/wr (`gci-i1fr-frames-roots-and-helper-windows-review-findings` item
/// 1): `helper_from` is the root-vector index where the helper-window pass's
/// roots begin (`TakenOver::census_helper_from`, offset by `snapshot_end`);
/// with it the frozen-peer tail is split into its take-over and helper-window
/// halves. Both labels keep the `xt frozen in-JIT peers` prefix.
pub(in crate::runtime::interpreter::gc_and_alloc) fn oldmark_census_stage_sections(
    collect_end: usize,
    snapshot_end: usize,
    helper_from: Option<usize>,
) -> Option<cratonvm_gc::gen_heap::OldmarkCensusStagedSections> {
    if !cratonvm_gc::gen_heap::oldmark_census_enabled() {
        return None;
    }
    Some(cratonvm_gc::gen_heap::stage_oldmark_census_root_sections(
        oldmark_census_sections(
            collect_end,
            snapshot_end,
            helper_from,
            crate::memory::roots::scan_section_of,
        ),
    ))
}

/// The section list [`oldmark_census_stage_sections`] stages, with the
/// per-index step lookup passed in so it is testable without a collection.
/// `step_of` answers `"<marks-off>"` when no marks were recorded, and
/// `"<before-first-section>"` for the activation loaders `collect_roots`
/// pushes before its first mark.
pub(in crate::runtime::interpreter::gc_and_alloc) fn oldmark_census_sections(
    collect_end: usize,
    snapshot_end: usize,
    helper_from: Option<usize>,
    step_of: fn(usize) -> &'static str,
) -> Vec<(usize, &'static str)> {
    let mut sections: Vec<(usize, &'static str)> = Vec::new();
    if collect_end > 0 && step_of(0) != "<marks-off>" {
        let mut last: Option<&'static str> = None;
        for i in 0..collect_end {
            let step = match step_of(i) {
                "<before-first-section>" => "0: activation defining loaders (collect_roots)",
                s => s,
            };
            if last != Some(step) {
                sections.push((i, step));
                last = Some(step);
            }
        }
    }
    if sections.is_empty() {
        sections.push((
            0,
            "initiator collect_roots (no step marks; set CRATONVM_DBG_ROOT_REMAP_AUDIT=1)",
        ));
    }
    sections.push((
        collect_end,
        "registry snapshots (every alive thread, this one included)",
    ));
    match helper_from {
        Some(h) if h >= snapshot_end => {
            sections.push((
                snapshot_end,
                "xt frozen in-JIT peers: take-over (conservative)",
            ));
            sections.push((
                h,
                "xt frozen in-JIT peers: helper windows (conservative)",
            ));
        }
        _ => sections.push((snapshot_end, "xt frozen in-JIT peers (conservative)")),
    }
    sections
}

/// JIT r23 w1 L7: does a collection pause through `door` take the root-kind
/// census in `mode`? `Selected`: the allocation-failure, humongous, G1
/// compaction and explicit doors (`System.gc()`, jcmd, heap inspection and
/// dump, JVMTI) -- the pauses an OOME ladder or a `jmap -histo:live`
/// comparison reads. `All`: every door. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn root_kind_census_takes_door(
    mode: crate::memory::root_kind_census::CensusMode,
    door: GcDoor,
) -> bool {
    use crate::memory::root_kind_census::CensusMode;
    match mode {
        CensusMode::Off => false,
        CensusMode::All => true,
        CensusMode::Selected => matches!(
            door,
            GcDoor::AllocationFailure
                | GcDoor::G1CompactionPause
                | GcDoor::G1HumongousAllocation
                | GcDoor::SystemGc
                | GcDoor::DiagnosticCommand
                | GcDoor::HeapInspection
                | GcDoor::HeapDump
                | GcDoor::JvmtiForceGc
        ),
    }
}

/// JIT r23 w1 L7 (`CRATONVM_DBG=root-kind-census`): the census of
/// `run_collection_pause`'s root vector, in the pause, before the collection.
/// `ends` is `(collect_end, snapshot_end)` (the old-gen census's sections);
/// the extra kinds are the pause's finalizable candidates (marked by the
/// collector beside the roots) and the published JIT pin words (a G1 region
/// a pin names stays out of the collection set). One flag read when off.
pub(in crate::runtime::interpreter::gc_and_alloc) fn root_kind_census_for_pause(
    shared: &SharedVm,
    door: GcDoor,
    roots: &[ObjectRef],
    ends: (usize, usize),
    helper_from: Option<usize>,
    fin_candidates: &[usize],
    solo: bool,
) {
    use crate::memory::root_kind_census as census;
    if !root_kind_census_takes_door(census::mode(), door) {
        return;
    }
    let (collect_end, snapshot_end) = ends;
    let sections = oldmark_census_sections(
        collect_end,
        snapshot_end,
        helper_from,
        crate::memory::roots::scan_section_of,
    );
    // gkr: name a big object a published pin word holds, and where the word
    // came from (this thread's band scan, or another publisher's).
    let (own, others) = cratonvm_gc::gc_quiescence::pinned_jit_roots_snapshot_split();
    for (src, words) in [("own", &own), ("other", &others)] {
        let every = false;
        for &addr in words.iter() {
            let sized = shared.mem.heap.dbg_object_size_and_refs(addr, &mut |_| {});
            if every && sized.is_none() {
                let prov = cratonvm_gc::gc_quiescence::lookup_jit_reject_provenance(addr)
                    .or_else(|| cratonvm_gc::gc_quiescence::lookup_jit_root_provenance(addr))
                    .unwrap_or_else(|| "-".into());
                eprintln!("[root-census] pin-word src={src} addr=0x{addr:x} unsized prov=[{prov}]");
            }
            if let Some((size, _)) = sized {
                if size >= 256 * 1024 || every {
                    let prov = cratonvm_gc::gc_quiescence::lookup_jit_root_provenance(addr)
                        .or_else(|| cratonvm_gc::gc_quiescence::lookup_jit_holder_provenance(addr))
                        .or_else(|| {
                            cratonvm_gc::gc_quiescence::lookup_jit_reject_provenance(addr)
                                .map(|p| format!("reject {p}"))
                        })
                        .unwrap_or_else(|| "-".into());
                    eprintln!(
                        "[root-census] big-pin-word src={src} bytes={size} addr=0x{addr:x} prov=[{prov}]"
                    );
                }
            }
        }
    }
    let extra = vec![
        (
            "finalizable candidates (collector-marked)".to_string(),
            fin_candidates.to_vec(),
        ),
        (
            "published JIT pin words (G1: region kept out of the CSet)".to_string(),
            cratonvm_gc::gc_quiescence::pinned_jit_roots_snapshot(),
        ),
    ];
    let walkable = solo
        || shared
            .threads
            .thread_registry
            .collect_reserved_tlab_tails()
            .is_empty();
    census::run(
        shared,
        &format!("collect:{door:?}"),
        roots,
        &sections,
        Some((collect_end, snapshot_end)),
        extra,
        walkable,
    );
}

/// JIT r23 w1 L7: the census of a G1 mark pause's root vector (the stop-the-
/// world full mark, the concurrent cycle's remark), before it marks. Since
/// JIT r23 w1b (R23W1L7-4) also the G1 initial mark, the ZGC mark start and
/// mark end, and (through [`root_kind_census_for_parts`]) the Generational
/// concurrent initial mark and remark; the name is the first caller's.
/// `ends` is `(collect_end, snapshot_end, xt_end)`; past `xt_end` the full
/// mark appends the frozen peers' referents. Labels of the `collect_roots`
/// part follow its scan marks; a remark that stripped keyed mirrors out of
/// that part (`strip_registry_mirrors`) shifts them by the stripped count.
/// Not walkable for the region summary (no TLAB-tail proof here).
pub(in crate::runtime::interpreter::gc_and_alloc) fn root_kind_census_for_g1_mark(
    shared: &SharedVm,
    site: &str,
    roots: &[ObjectRef],
    ends: (usize, usize, usize),
) {
    use crate::memory::root_kind_census as census;
    if census::mode() == census::CensusMode::Off {
        return;
    }
    let (collect_end, snapshot_end, xt_end) = ends;
    let mut sections = oldmark_census_sections(
        collect_end,
        snapshot_end,
        None,
        crate::memory::roots::scan_section_of,
    );
    if roots.len() > xt_end {
        sections.push((xt_end, "frozen peers' referents (mark only)"));
    }
    census::run(
        shared,
        site,
        roots,
        &sections,
        Some((collect_end, snapshot_end)),
        Vec::new(),
        false,
    );
}

/// JIT r23 w1b L7 (R23W1L7-4): [`root_kind_census_for_g1_mark`] for a pause
/// that holds its roots as the three parts `[collect_roots, registry
/// snapshots, frozen peers]` (the Generational concurrent initial mark and
/// remark); the joined vector is built only when the census is on. The
/// young-to-old seeds those pauses append afterwards are not in it.
pub(in crate::runtime::interpreter::gc_and_alloc) fn root_kind_census_for_parts(shared: &SharedVm, site: &str, parts: [&[ObjectRef]; 3]) {
    use crate::memory::root_kind_census as census;
    if census::mode() == census::CensusMode::Off {
        return;
    }
    let all: Vec<ObjectRef> = parts.iter().flat_map(|p| p.iter().copied()).collect();
    let ends = (parts[0].len(), parts[0].len() + parts[1].len(), all.len());
    root_kind_census_for_g1_mark(shared, site, &all, ends);
}

/// JIT r23 w1 L7: after a G1 mark pause's cleanup, the region census it left
/// (pairs with [`root_kind_census_for_g1_mark`]: reached bytes there against
/// Old used here tells retention from fragmentation). One flag read when off.
pub(in crate::runtime::interpreter::gc_and_alloc) fn root_kind_census_after_g1_cleanup(shared: &SharedVm, site: &str, completed: bool) {
    use crate::memory::root_kind_census as census;
    if census::mode() == census::CensusMode::Off {
        return;
    }
    if let Some(c) = shared.mem.heap.region_census() {
        eprintln!(
            "[root-census] site={site}-after-cleanup completed={completed} free={} old={}/{} \
             humongous={}/{} eden={} survivor={}",
            c.free, c.old.0, c.old.1, c.humongous.0, c.humongous.1, c.eden.0, c.survivor.0
        );
    }
}


/// Hand finalizers no door could run to the reference-delivery thread,
/// starting it on first use (gc-common w5-a,
/// `common-w2a-finalizers-queued-by-forced-collections-wait-for-another-door`,
/// the fully-compiled-program half).
///
/// A program running compiled code end to end collects only through the
/// allocation-FAILURE door (the JIT allocation helpers), which cannot run Java,
/// and never reaches `maybe_gc`'s no-collection drain, the one door that takes
/// the ref-work hint. Its finalizers were queued — and, through
/// `finalizable_roots`, re-rooted with everything they reach — collection after
/// collection, and never run. HotSpot runs them on its `Finalizer` thread
/// whatever the mutators are doing; this VM has that thread
/// (`finalizer_thread_main`), and under the default
/// [`FinalizerDelivery::WhenLockHeld`] policy its batch runs exactly the
/// finalizers (cleaner actions keep their inline doors).
///
/// Called by [`run_collection_pause`] after the release, only when the hint
/// stayed untaken for [`UNDRAINED_REF_WORK_PAUSES`] pauses AND finalizers from
/// an earlier collection were still queued — which an interpreted program's
/// drains (every one empties the queue, and the no-collection path runs after
/// every interpreted allocation) never leave. Policy:
///
/// * `WhenLockHeld` (default): request a batch, starting the thread on first
///   use. A later stale collection requests the next batch.
/// * `Always` (`CRATONVM_FINALIZER_THREAD=1`): nothing — every pause already
///   woke the thread.
/// * `Inline` (`=0`, the pre-w3 bisection arm): nothing, byte for byte.
///
/// Starting the thread runs no Java and touches no heap (see
/// [`start_finalizer_thread`]); a VM with no owning `Arc` (a unit fixture)
/// keeps the old behaviour.
pub(in crate::runtime::interpreter::gc_and_alloc) fn hand_undrained_finalizers_to_delivery_thread(shared: &SharedVm) {
    if finalizer_delivery_policy() != FinalizerDelivery::WhenLockHeld {
        return;
    }
    let _served = request_finalizer_thread(shared, true);
}

/// gen r4w6/oldpin6 (2026-09-24) — publish, for the collection this thread is
/// about to run, which roots a pinned old-gen compaction must pin
/// (`cratonvm_gc::gen_heap::OldPinRootLayout`; oldcompact5's root-gatherer
/// request, `docs/feature-designs/generative-gc-architecture.md`).
///
/// `roots[..initiator_root_count]` is this thread's `collect_roots` output;
/// the rest (`..total_roots`) is the peers' snapshots and the take-over's
/// words. When the interpreter's conservative frame probe ran on this pause
/// (`gc_quiescence::is_active()`, step 1's `conservative_locals`, or the A5
/// unregistered-JIT-frame flag, step 14a5), the collector used to pin EVERY
/// root, because the probe's words sit in the slice unmarked. This re-derives
/// the probe's words from this thread's frames — the same two scanners, the
/// same frames, the same heap, inside the same pause, so it finds every word
/// step 1 or 14a5 added — and names the peers' part as the only range still
/// pinned wholesale (a peer's snapshot may hold probe words from its own
/// deposit, which cannot be re-derived here).
///
/// Over-pinning is the only possible error: a word this finds that step 1 did
/// not add (`CRATONVM_NO_CONSERVATIVE_LOCALS`) pins an object that did not
/// need it. The collector ignores a layout whose `root_len` differs from its
/// slice (fails closed to pinning everything), and the door clears it after
/// the collection (`clear_old_pin_root_layout`).
///
/// Costs nothing unless `CRATONVM_GC_OLD_PINNED_COMPACT` is on, the heap is
/// generational, and the probe ran; then one frame walk of this thread.
/// Kept apart from the door's other root publication (lane `oomjit6` edits
/// that) so the two merge cleanly.
pub(in crate::runtime::interpreter::gc_and_alloc) fn publish_old_pin_root_layout_for_pause(
    shared: &SharedVm,
    thread: &JvmThread,
    initiator_root_count: usize,
    total_roots: usize,
) {
    if !cratonvm_types::flags().gc.old_pinned_compact || !shared.mem.heap.is_generational() {
        return;
    }
    // After `collect_roots`: the A5 flag is this cycle's answer by now (step
    // 14's JIT scan set it), exactly where step 14a5 reads it.
    let interpreter_probe = cratonvm_gc::gc_quiescence::is_active()
        || cratonvm_gc::gc_quiescence::unregistered_jit_frame_on_stack()
        || cratonvm_gc::gc_quiescence::conservative_peers_live();
    if !interpreter_probe {
        // The collector pins no root range on such a pause; nothing to say.
        return;
    }
    let mut probe: Vec<ObjectRef> = Vec::new();
    for frame in thread.frames.iter() {
        frame.scan_locals_conservative(&mut probe, &shared.mem.heap);
        frame
            .stack
            .scan_object_refs_conservative(&mut probe, &shared.mem.heap);
    }
    let interpreter_words: Vec<usize> = probe.iter().map(|o| o.as_ptr() as usize).collect();
    let lo = initiator_root_count.min(total_roots);
    cratonvm_gc::gen_heap::publish_old_pin_root_layout(cratonvm_gc::gen_heap::OldPinRootLayout {
        interpreter_words,
        pin_all: (lo, total_roots),
        root_len: total_roots,
    });
}

/// gci i21/pr: `CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`, read once per
/// collection pause. WARNING flip (gcj j2, 2026-10-19, `triage-j1.md`): default
/// ON on the Generational backend (`=0` kills); G1 and ZGC keep the opt-in.
pub(in crate::runtime::interpreter::gc_and_alloc) fn dedupe_initiator_snapshot_enabled(
    _shared: &SharedVm,
) -> bool {
    true
}

/// gci i21/pr (`CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`, opt-in; step 11 of
/// `docs/feature-designs/real-jdk-mode.md`):
/// arm `run_collection_pause`'s registry gather to leave out the snapshot of
/// every registry entry that shares this thread's `root_snapshot` `Arc`
/// (`ThreadRegistry::arm_initiator_snapshot_skip`).
///
/// # Why the root SET is unchanged
///
/// `collect_roots` step 11 pushes `thread.root_snapshot`'s words into the
/// door's vector unconditionally, and nothing truncates the vector below that
/// point except step 14's own scan range. The registry entry of this thread
/// holds the SAME `Arc` (`set_root_snapshot` shares it), so its words reached
/// the collector twice. Nothing can write the snapshot between step 11 and
/// the gather: only its owner publishes it (`update_root_snapshot`, the
/// blocked deposit), the owner is this thread, which is inside the door, and
/// the blocked fold and the post-pause remaps run after the collection. The
/// gather keeps the entry's mirror, JMX roots and conservative-guard words.
///
/// # What does change (why opt-in)
///
/// The duplicate sat in the PEERS' part of the vector, which a Generational
/// pinned old-gen compaction pins wholesale (`OldPinRootLayout::pin_all`,
/// from `initiator_root_count`). Without it the initiator's own snapshot is
/// judged as the initiator's part is (its conservative compiled-frame words
/// through the published JIT pins, its interpreter probe words through the
/// layout), which is the gen r4w6/oldpin6 contract; precise frame roots then
/// stop pinning, so that compaction can move more. ZGC's precise-root claim
/// reads only the initiator's part and is unaffected; G1 pins from published
/// words, not from vector sections.
///
/// The guard disarms on drop, so an arm no gather consumed never reaches
/// another door's gather (the concurrent cycles' marks have no step 11), and
/// reports the words left out under `CRATONVM_DBG_ROOTPROF=1`.
#[must_use]
pub(in crate::runtime::interpreter::gc_and_alloc) fn arm_initiator_snapshot_skip<'a>(
    shared: &'a SharedVm,
    thread: &JvmThread,
) -> InitiatorSnapshotSkipArm<'a> {
    if !dedupe_initiator_snapshot_enabled(shared) {
        return InitiatorSnapshotSkipArm { registry: None };
    }
    let registry = &shared.threads.thread_registry;
    registry.arm_initiator_snapshot_skip(&thread.root_snapshot);
    InitiatorSnapshotSkipArm {
        registry: Some(registry),
    }
}

/// gci i21/pr: [`arm_initiator_snapshot_skip`]'s disarm-on-drop guard.
pub(in crate::runtime::interpreter::gc_and_alloc) struct InitiatorSnapshotSkipArm<'a> {
    pub(in crate::runtime::interpreter::gc_and_alloc) registry: Option<&'a crate::threading::thread_registry::ThreadRegistry>,
}

impl Drop for InitiatorSnapshotSkipArm<'_> {
    fn drop(&mut self) {
        if let Some(registry) = self.registry {
            let skipped = registry.disarm_initiator_snapshot_skip();
            if crate::memory::native_roots::rootprof::on() {
                match skipped {
                    Some(words) => {
                        eprintln!("[rootprof] initiator-snapshot dedupe: skipped_words={words}")
                    }
                    None => eprintln!("[rootprof] initiator-snapshot dedupe: not consumed"),
                }
                // gcj j3/x2: gathers on another thread that left the arm
                // alone (lifetime, per registry; 0 on a healthy run).
                let foreign = registry.initiator_skip_foreign_gathers();
                if foreign > 0 {
                    eprintln!("[rootprof] initiator-snapshot dedupe: foreign_gathers={foreign}");
                }
            }
        }
    }
}

/// gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`, default ON since 2026-10-17; page
/// `gci-i12vp-parked-continuations-folded-by-every-young-pause-FIXED-20261016`, the
/// gather half): arm `run_collection_pause`'s registry gather to split the
/// unmounted continuations whose words are all outside the collector's young
/// set into an old-only section (`ThreadRegistry::arm_parked_split_gather`).
/// The heap answers the young set only when it can keep the contract
/// (`VmHeap::parked_old_only_young_ranges`: Generational outside an open
/// concurrent mark, G1 when this pause is young-only, never ZGC); otherwise
/// nothing is armed and the gather is the historical one.
///
/// Returns a guard that disarms on drop, so an unwind between the arm and
/// the gather can never leave an arm for another door's gather -- the
/// concurrent cycles' initial marks and remarks (`maybe_concurrent_gc_at`,
/// `g1_concurrent_mark_cycle`) run their own unarmed
/// `collect_all_root_snapshots` and must always see every root.
#[must_use]
pub(in crate::runtime::interpreter::gc_and_alloc) fn arm_parked_old_only_gather(shared: &SharedVm) -> ParkedSplitArm<'_> {
    let registry = &shared.threads.thread_registry;
    if !registry.vt_parked_gather_filter() {
        return ParkedSplitArm { registry: None };
    }
    match shared.mem.heap.parked_old_only_young_ranges() {
        Some(young) => {
            registry.arm_parked_split_gather(&young);
            ParkedSplitArm {
                registry: Some(registry),
            }
        }
        None => ParkedSplitArm { registry: None },
    }
}

/// gci i13/vg: [`arm_parked_old_only_gather`]'s disarm-on-drop guard. On the
/// normal path `stage_parked_old_only_roots` has already taken the section
/// and the drop's take finds nothing.
pub(in crate::runtime::interpreter::gc_and_alloc) struct ParkedSplitArm<'a> {
    pub(in crate::runtime::interpreter::gc_and_alloc) registry: Option<&'a crate::threading::thread_registry::ThreadRegistry>,
}

impl Drop for ParkedSplitArm<'_> {
    fn drop(&mut self) {
        if let Some(registry) = self.registry {
            let _ = registry.take_parked_old_only();
        }
    }
}

/// gci i13/vg: hand the armed gather's old-only section to the heap for this
/// pause (`VmHeap::stage_parked_old_only_roots`), which the Generational
/// collector adds back to the root set of a same-pause major. Always takes
/// (and so disarms) the registry's slot, empty or not, and always stages, so
/// no section outlives its pause.
pub(in crate::runtime::interpreter::gc_and_alloc) fn stage_parked_old_only_roots(shared: &SharedVm) {
    let registry = &shared.threads.thread_registry;
    if !registry.vt_parked_gather_filter() {
        return;
    }
    let old_only = registry.take_parked_old_only();
    shared.mem.heap.stage_parked_old_only_roots(old_only);
}

/// gci i14/ve (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED`, default ON since 2026-10-17; item 6 of
/// `gci-i11vt-virtual-thread-continuation-roots-review-findings-FIXED-20261016`):
/// arm the registry's next gather to key the deposited JDK continuations by
/// their `VirtualThread` mirrors (`ThreadRegistry::arm_mirror_keyed_gather`),
/// when this pause may defer a root to a `metadata_pin` row
/// ([`parked_mirror_keyed_licensed`]). Called inside the pause, before
/// `collect_roots`; the guard disarms a request no gather consumed.
pub(in crate::runtime::interpreter::gc_and_alloc) fn arm_parked_mirror_keyed_gather(shared: &SharedVm) -> ParkedKeyedArm<'_> {
    let registry = &shared.threads.thread_registry;
    if !registry.vt_parked_mirror_keyed() || !parked_mirror_keyed_licensed(shared) {
        return ParkedKeyedArm { registry: None };
    }
    // Never block on the manager's lock inside the pause: a stopped peer may
    // hold it. Contended: key nothing this pause (the ordinary rooted
    // gather), and count it for the census.
    let Some(candidates) = shared
        .threads
        .virtual_thread_manager
        .try_deposited_jdk_continuation_ids()
    else {
        registry.note_mirror_keyed_contended();
        return ParkedKeyedArm { registry: None };
    };
    if candidates.is_empty() {
        return ParkedKeyedArm { registry: None };
    }
    let (old_base, old_len) = shared.mem.heap.old_gen_info();
    registry.arm_mirror_keyed_gather(shared.vm_identity, old_base, old_len, candidates);
    ParkedKeyedArm {
        registry: Some(registry),
    }
}

/// gci i14/ve: may this pause key parked continuations by their mirrors?
///
/// * Generational only. G1's in-pause verdict is region-granular (a dead
///   mirror in a region no pause collects reads as live, while its rows'
///   values were left unmarked by the remark), and G1 / ZGC never take this
///   door's licence to their markers; the G1 half is narrowed on the page.
/// * JDK-driven continuations (`VirtualThreadManager::jdk_continuations`):
///   the bound path's virtual threads are scheduled by this VM.
/// * No concurrent old-generation cycle open: its `ConcurrentMarker` follows
///   no `metadata_pin` row (`roots::conditional_loader_metadata`'s note), and
///   its own initial-mark / remark gathers never arm.
/// * No debugger that reads locals (a JDWP / JVMTI agent may read an
///   unmounted thread's frames by its id, without its mirror).
/// * The cycle's loader-metadata licence (`roots::conditional_loader_metadata`,
///   recomputed here inside the pause from the same inputs `collect_roots`
///   step 1 reads): the cycles on which statics, class locks and pending
///   inherited `ThreadLocal`s are deferred to rows exactly this way.
pub(in crate::runtime::interpreter::gc_and_alloc) fn parked_mirror_keyed_licensed(shared: &SharedVm) -> bool {
    shared.mem.heap.is_generational()
        && shared.threads.virtual_thread_manager.jdk_continuations()
        && shared.mem.concurrent_gc_state.phase() == cratonvm_gc::ConcurrentGcPhase::Idle
        && !crate::runtime::jvmti::debugger_observes_locals(shared)
        && crate::memory::roots::conditional_loader_metadata(shared)
}

/// gci i14/ve: [`arm_parked_mirror_keyed_gather`]'s disarm-on-drop guard. On
/// the normal path the gather has consumed the request and the drop finds
/// nothing; the deferred list is the same pause's `update_all_roots` sweep's.
pub(in crate::runtime::interpreter::gc_and_alloc) struct ParkedKeyedArm<'a> {
    pub(in crate::runtime::interpreter::gc_and_alloc) registry: Option<&'a crate::threading::thread_registry::ThreadRegistry>,
}

impl Drop for ParkedKeyedArm<'_> {
    fn drop(&mut self) {
        if let Some(registry) = self.registry {
            registry.disarm_mirror_keyed_gather();
        }
    }
}

/// gci i15/vh (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED` on G1; item 6 of
/// `gci-i11vt-virtual-thread-continuation-roots-review-findings-FIXED-20261016`,
/// its G1 half): arm the registry's next gather to key the deposited JDK
/// continuations by their mirrors, on one of the two G1 MARKING root scans --
/// the initial mark (`g1_concurrent_mark_cycle`) and the remark
/// (`g1_final_remark_cleanup`). Never on an evacuation pause: those root every
/// parked thread as before, and inside a cycle also every address the rows
/// name (`roots::g1_cycle_keeps_metadata_rows`), so nothing a row names dies
/// in one.
///
/// Why the two marking scans may leave a keyed entry out (frames AND mirror):
/// the cycle's marker follows every `metadata_pin` row of an object it marks
/// (`G1Collector::scan_object_refs`'s side-table tail, re-captured when the
/// rows' generation moves), so the frames are marked exactly when the mirror
/// is; the frames hold `this` of `VirtualThread.park`, so only an outside
/// edge marks the mirror. A thread keyed at the initial mark had its frames
/// filed as rows of the snapshot (SATB: a later mount and re-park can only
/// add objects that were reachable at the snapshot or allocated above TAMS).
/// The remark first marks the rows of every live owner to a fixed point (the
/// concurrent marker may have scanned a mirror before the gather filed its
/// rows), then judges the keyed mirrors on the completed mark
/// (`judge_mirror_keyed_parked_after_mark`) and retires the unmarked ones
/// after cleanup, only if cleanup trusted the mark
/// (`settle_mirror_keyed_parked_after_mark`). Old-region words only: a young
/// or humongous word keeps the whole entry rooted.
///
/// Returns a guard: [`G1ParkedKeyedGuard::strip_registry_mirrors`] takes step
/// 10b's copy of each keyed mirror out of the door's `collect_roots` result
/// (a plain scan pushes every alive thread's mirror there), and its drop
/// disarms and forgets an unjudged request.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_key_parked_for_mark(shared: &SharedVm) -> G1ParkedKeyedGuard<'_> {
    let registry = &shared.threads.thread_registry;
    if !registry.vt_parked_mirror_keyed() || !g1_parked_mirror_keyed_licensed(shared) {
        return G1ParkedKeyedGuard { registry: None };
    }
    // Never block on the manager's lock inside the pause (as the
    // Generational arm): contended, key nothing this scan.
    let Some(candidates) = shared
        .threads
        .virtual_thread_manager
        .try_deposited_jdk_continuation_ids()
    else {
        registry.note_mirror_keyed_contended();
        return G1ParkedKeyedGuard { registry: None };
    };
    if candidates.is_empty() {
        return G1ParkedKeyedGuard { registry: None };
    }
    if !shared.mem.heap.is_g1() {
        return G1ParkedKeyedGuard { registry: None };
    }
    let old_ranges = shared.mem.heap.g1_parked_mirror_keyed_view(true).1;
    if old_ranges.is_empty() {
        return G1ParkedKeyedGuard { registry: None };
    }
    // No pair of an earlier scan may reach this one's strip or verdict.
    registry.forget_mirror_keyed_deferred();
    registry.arm_mirror_keyed_gather_ranges(shared.vm_identity, old_ranges, candidates);
    G1ParkedKeyedGuard {
        registry: Some(registry),
    }
}

/// gci i15/vh: may this G1 marking scan key parked continuations?
///
/// * G1 with a marking cycle open (its marker follows the rows; the scan is
///   the initial mark's or the remark's -- the caller is structural).
/// * JDK-driven continuations: the bound path's threads are this VM's.
/// * No debugger that reads locals (it may read an unmounted thread's frames
///   by its id, without its mirror).
///
/// Unlike the Generational licence, no `roots::conditional_loader_metadata`
/// term: on G1 that licence is the marking scans themselves
/// (`gc_quiescence::class_unload_marking`), and the G1 marker follows every
/// row whatever the loader-unload switch says.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_parked_mirror_keyed_licensed(shared: &SharedVm) -> bool {
    shared.mem.heap.is_g1()
        && shared.mem.heap.g1_is_marking_active()
        && shared.threads.virtual_thread_manager.jdk_continuations()
        && !crate::runtime::jvmti::debugger_observes_locals(shared)
}

/// gci i15/vh: whether the last G1 cleanup trusted its closure
/// (`G1Collector::parked_mirror_keyed_view`). `false` off G1.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_parked_keyed_mark_trusted(shared: &SharedVm) -> bool {
    shared.mem.heap.g1_parked_mirror_keyed_view(false).0
}

/// gci i15/vh: [`g1_key_parked_for_mark`]'s guard. On drop: disarm a request
/// no gather consumed and forget any pair no verdict settled (an unwinding
/// pause keeps its threads; the rows stay for the cycle's marker).
pub(in crate::runtime::interpreter::gc_and_alloc) struct G1ParkedKeyedGuard<'a> {
    pub(in crate::runtime::interpreter::gc_and_alloc) registry: Option<&'a crate::threading::thread_registry::ThreadRegistry>,
}

impl G1ParkedKeyedGuard<'_> {
    /// Take step 10b's copy of every mirror the gather keyed out of `roots`
    /// (`thread_registry::strip_keyed_mirror_registry_roots`: one occurrence
    /// per keyed thread; any other occurrence is a real root and stays).
    pub(in crate::runtime::interpreter::gc_and_alloc) fn strip_registry_mirrors(&self, roots: &mut Vec<ObjectRef>) -> usize {
        match self.registry {
            Some(registry) => crate::threading::thread_registry::strip_keyed_mirror_registry_roots(
                roots,
                &registry.mirror_keyed_deferred_mirrors(),
            ),
            None => 0,
        }
    }

    /// The initial mark keys but never judges: its mark is not complete.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn forget_without_verdict(&self) {
        if let Some(registry) = self.registry {
            registry.forget_mirror_keyed_deferred();
        }
    }
}

impl Drop for G1ParkedKeyedGuard<'_> {
    fn drop(&mut self) {
        if let Some(registry) = self.registry {
            registry.disarm_mirror_keyed_gather();
            registry.forget_mirror_keyed_deferred();
        }
    }
}

/// Whether the two debug HEAP WALKERS -- `validate_object_sizes`
/// (`CRATONVM_DBG_VALIDATE_NEW`) and the heap-stale verifier
/// (`CRATONVM_DBG_HEAP_STALE`, both halves) -- may run at this point of the
/// pause. gc-common w7-g, `common-e-small-findings` item 1.
///
/// Both enumerate objects with `VmHeap::walk_objects`. The walk the solo gate
/// was protecting is the Generational one: `walk_young_objects` parses
/// from-space linearly and skips free blocks and GAP fillers but NOT the
/// published JIT TLAB skip spans, so a peer's reserved-but-unallocated TLAB
/// tail -- a frozen in-JIT peer's, or a blocked thread's that never retired --
/// is parsed as objects (G1's walk skips the spans; ZGC's enumerates its
/// allocation registry). A peer that parked at a safepoint poll retired its
/// TLAB on the way in (`safepoint_check`) and leaves nothing to misparse.
///
/// So the question is not "how many threads did the census count" but "does
/// any alive thread hold an unretired tail", and the registry answers that
/// exactly (`collect_reserved_tlab_tails`, valid inside the pause). `solo` is
/// kept as a short cut: it is the old gate, and with no peer there is none.
///
/// `false` at once when neither walker is armed, so the registry walk is paid
/// only under a debug flag. A skipped walk says so (rate-limited) instead of
/// passing as a clean one.
pub(in crate::runtime::interpreter::gc_and_alloc) fn debug_heap_walks_permitted(shared: &SharedVm, solo: bool) -> bool {
    if !debug_heap_walkers_armed() {
        return false;
    }
    if solo {
        return true;
    }
    let tails = shared.threads.thread_registry.collect_reserved_tlab_tails();
    if tails.is_empty() {
        return true;
    }
    note_debug_heap_walk_skipped(tails.len());
    false
}

/// Either debug heap walker's own gate. Read per pause, exactly as the walkers
/// read them themselves.
pub(in crate::runtime::interpreter::gc_and_alloc) fn debug_heap_walkers_armed() -> bool {
    false
}

/// A pause whose debug heap walks were skipped: say so, at most eight times a
/// process -- a verifier that is silent because it did not run must not read
/// as a verifier that found nothing.
#[cold]
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_debug_heap_walk_skipped(unretired_tails: usize) {
    static REPORTED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if REPORTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
        eprintln!(
            "[gc-debug] heap walkers (CRATONVM_DBG_VALIDATE_NEW / CRATONVM_DBG_HEAP_STALE) \
             skipped for this pause: {unretired_tails} thread(s) hold an unretired TLAB tail \
             a linear heap walk would parse as objects"
        );
    }
}

/// The post-collection debug instruments `maybe_gc`'s single-threaded arm used
/// to run inline. Every one is gated on its own `CRATONVM_DBG_*` flag and is a
/// flag read when off. Runs inside the pause, after `update_all_roots`, on
/// every pause (gc-common w7-g): the watch polls and the frame dump read no
/// heap walk and always run; the two heap walkers run when `heap_walks`
/// ([`debug_heap_walks_permitted`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn post_gc_debug_verifiers(
    shared: &SharedVm,
    thread: &JvmThread,
    pointer_map: &cratonvm_types::PointerMap,
    heap_walks: bool,
) {
    // GC-EXIT detect: a watched cell that was clean at GC ENTRY but reads 0x4
    // here was corrupted *by collect_garbage itself* (between entry and exit)
    // — isolating GC-vs-mutator definitively.
    if crate::runtime::ec_watch::enabled() {
        for (holder, idx, expected, now) in crate::runtime::ec_watch::detect(shared.vm_identity) {
            eprintln!(
                "[ecwatch-GCEXIT] holder@0x{holder:x} fld[{idx}]: 0x{expected:x} -> 0x{now:x} (corrupted DURING collect_garbage)"
            );
        }
    }
    // bc math-ec 0x4 (CRATONVM_DBG_MEMWATCH): post-GC poll of the watched
    // absolute address — a HIT here (vs at a mutator safepoint) means the flip
    // happened inside collect_garbage / reference processing.
    crate::runtime::memwatch::poll("post-gc", || {
        thread
            .frames
            .iter()
            .rev()
            .take(28)
            .map(|f| {
                format!(
                    "  {}.{}{} pc={}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    if heap_walks {
        // DBG (env-gated): validate every young object's header size against
        // its class — pins a JIT `new` that wrote a wrong-size header.
        crate::memory::gc::validate_object_sizes(shared);
        // DBG: run the heap-stale verifier after EVERY GC (incl. the
        // non-moving JIT-active sweep, where update_all_roots early-returns on
        // the empty pointer_map). It flags any LIVE object whose field points
        // to a ZEROED/reclaimed object — i.e. a live object the sweep wrongly
        // reclaimed (missing root). The referrer names the bug.
        crate::memory::gc::verify_heap_object_fields(shared, pointer_map);
    }
    // DBG (CRATONVM_DBG_CORRUPT_FRAMES): on the FIRST GC that detects sweep
    // corruption, dump the mutator's Java stack. With a tiny young gen
    // (frequent GC) this fires close to the JIT corruptor — the interpreted
    // frame on top is the BC method that called the JIT-compiled corruptor.
    if None::<std::ffi::OsString>.is_some()
        && cratonvm_gc::gen_heap::SWEEP_CORRUPTION_HITS.load(std::sync::atomic::Ordering::Relaxed)
            > 0
    {
        use std::sync::atomic::{AtomicBool, Ordering as DbgO};
        static DUMPED: AtomicBool = AtomicBool::new(false);
        if !DUMPED.swap(true, DbgO::Relaxed) {
            eprintln!(
                "[corrupt-frames] FIRST sweep corruption — mutator Java stack ({} frames, top first):",
                thread.frames.len()
            );
            for (i, f) in thread.frames.iter().enumerate().rev().take(60) {
                eprintln!(
                    "  [{}] {}.{}{} pc={}",
                    i,
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc
                );
            }
        }
    }
}

/// Self-call identity proof for the raw direct self-recursive CALL routing
/// (see `cratonvm_jit::set_self_call_identity_stable`): true iff `class_id`
/// was defined by a BUILTIN loader (bootstrap/extension/application) AND the
/// loader-qualified exact-name lookup maps the class's name back to this exact
/// `ClassId`. Builtin loader registries hold one class per name and resolve a
/// self-reference to the already-defined class, so a same-named shadow can
/// never rebind the target; `UserDefined` loaders (enhancement/duplicating
/// loaders) return false and keep the dispatch route.
pub(in crate::runtime::interpreter) fn self_call_identity_stable(shared: &SharedVm, class_id: ClassId) -> bool {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    !matches!(
        class.loader_id,
        cratonvm_types::ClassLoaderId::UserDefined(_)
    ) && cm.class_defined_by_loader_exact(&class.name, class.loader_id) == Some(class_id)
}

/// Force a GC cycle regardless of threshold.
/// Used by allocation helpers when the fast-path allocation fails.
/// Public wrapper so sibling modules (exceptions, invokedynamic) can force a
/// GC cycle when a direct allocation fails. (This doc sat on
/// `self_call_identity_stable` until 2026-09-23.)
pub fn maybe_gc_forced_pub(shared: &SharedVm, thread: &mut JvmThread) {
    maybe_gc_forced_at(shared, thread, "unlabelled")
}

/// [`maybe_gc_forced_pub`] with the caller's identity, for the census.
pub fn maybe_gc_forced_pub_at(shared: &SharedVm, thread: &mut JvmThread, site: &'static str) {
    maybe_gc_forced_at(shared, thread, site)
}

/// `zgc_concurrent_mark_cycle` for the JIT allocation helpers.
///
/// Same reason `maybe_gc_forced_pub` exists: `vm/src/jit/helpers.rs` is a
/// sibling module and the cycle opener is `pub(in crate::runtime::interpreter)`. See
/// `jit_maybe_start_zgc_concurrent_mark` for why the JIT needs its own call
/// site at all -- a fully compiled allocation loop reaches `maybe_gc` never.
pub fn zgc_concurrent_mark_cycle_pub(shared: &SharedVm, thread: &mut JvmThread) {
    zgc_concurrent_mark_cycle(shared, thread);
}

/// gcf f1/c (`gengc-r4w4-concmark4-...-polled-only-after-a-young-collection`)
/// — is a Generational inline concurrent-start request latched
/// (`CRATONVM_GEN_CONC_INLINE_START`)? One relaxed load; `false` on every run
/// without the flag (nothing latches) and on G1 / ZGC (no generational heap
/// latches).
///
/// Why the JIT allocation helpers need it: the latch is set by a DIRECT
/// old-gen allocation (a humongous array, a young spill), and the only pickup
/// was [`maybe_gc`], which a compiled allocation loop never reaches — the same
/// gap `jit_maybe_start_zgc_concurrent_mark` closes for ZGC. On
/// `GenR4W5DirectOldGrowthProbe` (the loop OSR-compiles in its first
/// iteration) the request was latched once, then sat untaken until the final
/// interpreted allocation (`println`'s string), so the run's majors were all
/// allocation-failure STW collections (`majcad_fallback_start_late=16`) and
/// `concdrv_cycles_completed` stayed 0 on 2 of 3 runs (gce ve2 c-5). A latch
/// that is never taken also blocks the next latch
/// (`ConcurrentGcState::inline_growth_poll_armed`), so the counter read
/// `latched=1 taken=1` on every run.
#[inline]
pub fn gen_inline_concurrent_start_pending(shared: &SharedVm) -> bool {
    shared.mem.concurrent_gc_state.inline_start_requested()
}

/// gcf f1/c — take a latched Generational inline start request and run the
/// concurrent driver on this thread (see [`gen_inline_concurrent_start_pending`]).
///
/// For the JIT allocation helpers, called at the helper's ENTRY, before it
/// allocates: the object the helper is about to return would otherwise be an
/// old-gen block referenced only from a Rust local while the cycle marks and
/// sweeps. At the entry every live value of the compiled caller is in its
/// frame, which the initial mark and the remark scan exactly as they do for
/// ZGC's mark start at the same place. The driver re-asks the trigger
/// (`concurrent_cycle_due`), so a request gone stale opens nothing.
pub fn gen_inline_concurrent_start_pub(shared: &SharedVm, thread: &mut JvmThread) {
    if shared.mem.concurrent_gc_state.take_inline_start_request() {
        maybe_concurrent_gc_at(shared, thread, MarkDoor::JitDriver);
    }
}

/// Drive G1's concurrent-mark lifecycle from a path compiled code reaches.
///
/// BOTH HALVES, in `maybe_concurrent_gc`'s order and for its reasons: finish
/// first, so an active cycle whose background marker has drained gets its STW
/// remark + cleanup from the thread that already has the barrier context;
/// start second, so the two cannot race within one call.
///
/// Why it exists at all: `maybe_concurrent_gc` is the only caller of either
/// half, and it lives inside `maybe_gc`, which a JIT-compiled workload reaches
/// ZERO times (instrumented on `org.h2.test.store.TestMVStoreTool`, -Xmx256m,
/// 2026-09-05: zero calls across a run with 65 young pauses). `needs_gc()` is
/// false for G1 because the collector triggers its own young pauses from inside
/// the allocator, so `maybe_gc` returns at its gate and the body never runs.
///
/// The consequence measured on that workload: `marking_complete` is never set,
/// so `needs_mixed_gc()` is never true, so `mixed_phase_has_work()` is not
/// called ONCE and no mixed collection ever runs. Old grew to 245 of 256
/// regions, `free` reached 0, and the pause census reported 18-54 to-space
/// exhaustions and 17 evacuation failures per run. Only `g1_force_full_cycle`,
/// the last-ditch pre-OOM path, drove either half.
///
/// The FINISH half is the one with no substitute: a cycle that path starts is
/// otherwise never remarked or cleaned up, so even the marking that does happen
/// yields no `live_bytes` and no mixed candidates.
///
/// LANE W7-D — the account above is TRUE AND INCOMPLETE, and the correction
/// matters more than the original claim. `maybe_concurrent_gc` was never "the
/// only caller of either half": `g1_force_full_cycle` is a second, and the
/// missing one was not a caller at all. `maybe_gc_forced_at` — the
/// allocation-failure collection that `alloc_object_shared`, `gc_alloc_array`,
/// the TLAB refill wedge and every JIT allocation helper funnel into, and
/// which is where G1 takes most of its pauses — ran a full collection and
/// returned without touching the cycle machinery. So the defect is not
/// confined to compiled code: under `--nojit`, where every allocation reaches
/// `maybe_gc`, a run still finished ZERO cycles
/// (`w6m-a-workload-that-mixes.md` §4). That is now
/// `CRATONVM_G1_ALLOC_MARK_DRIVE`, default ON, and it makes this driver an
/// accelerator rather than a prerequisite.
pub fn g1_drive_concurrent_mark_pub(shared: &SharedVm, thread: &mut JvmThread) {
    g1_drive_mark_cycle(shared, thread, MarkDoor::JitDriver)
}

/// [`create_string_or_oom`] for UTF-16 code units.
///
/// Identical escalation ladder — the only difference is that the source is a
/// `&[u16]` rather than a `&str`, so an unpaired surrogate survives into the
/// allocated `String`. String concatenation builds its result this way; see
/// `string-concat-loses-unpaired-surrogates-FIXED-20260805.md`.
pub(crate) fn create_string_from_units_or_oom(
    shared: &SharedVm,
    thread: &mut JvmThread,
    units: &[u16],
) -> Result<ObjectRef, MethodCallFailed> {
    use crate::vm::try_create_java_string_from_units as try_new_string;
    if let Some(obj) = try_new_string(shared, units) {
        return Ok(obj);
    }
    if let Some(obj) = collect_and_retry(shared, thread, "create-string-units", |s| {
        try_new_string(s, units)
    }) {
        return Ok(obj);
    }
    maybe_dump_heap_on_oom(shared, thread);
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::OutOfMemoryError {
            message: format!("Java heap space (String of {} UTF-16 units)", units.len()),
        },
    )))
}

/// `create_string_or_oom` for a string LITERAL: pooled, so two `ldc`s of the
/// same `CONSTANT_String` push the identical reference (JVMS §5.1).
///
/// gc-common w8 verification (2026-09-24): `ldc` resolved a literal through
/// the infallible `create_java_string`, so the FIRST execution of an `ldc`
/// whose literal was not yet pooled, on a full heap, printed
/// `FATAL: heap exhausted allocating java/lang/String (9 units)` and aborted
/// the process. The probe caught `OutOfMemoryError` and then printed the
/// literal `"FILL-OOME"`; HotSpot throws `OutOfMemoryError` from the `ldc`
/// instead. The pool's write lock is released before the collection runs
/// (each attempt takes it afresh).
pub(crate) fn intern_string_literal_or_oom(
    shared: &SharedVm,
    thread: &mut JvmThread,
    text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    use crate::vm::try_create_java_string_reporting as try_pooled;
    if let Some((obj, _)) = try_pooled(shared, text) {
        return Ok(obj);
    }
    if let Some(obj) = collect_and_retry(shared, thread, "intern-string-literal", |s| {
        try_pooled(s, text).map(|(obj, _)| obj)
    }) {
        return Ok(obj);
    }
    maybe_dump_heap_on_oom(shared, thread);
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::OutOfMemoryError {
            message: format!("Java heap space (String of {} UTF-8 bytes)", text.len()),
        },
    )))
}

/// The one escalation every interpreter allocation ladder runs once its first,
/// collection-free attempt has failed: publish and retire the TLAB, force a
/// collection, stop if the GC-overhead limit has tripped, retry, run G1's
/// last-ditch complete mark cycle (dead Old/humongous spans are only reclaimed
/// by a finished cycle's cleanup; a no-op on the other backends), retry.
/// `None` means the caller must raise `OutOfMemoryError`.
///
/// gc-common w2-d: the object ladder (`alloc_object_shared`) and the array
/// ladder (`gc_alloc_array`) had this sequence inline, and the two String
/// ladders had a copy WITHOUT the overhead-limit check -- so a heap full of
/// live data failed fast on `new` after `GC_OVERHEAD_LIMIT_CYCLES`
/// unproductive collections but kept collecting (and ran a G1 last-ditch
/// cycle) on every `"a" + b` until it happened to succeed. One helper, one
/// policy (`common-f-string-ladder-skips-overhead-limit`).
///
/// `attempt` is called with the heap AFTER each collection; it must re-derive
/// anything address-shaped it needs, because a collection may have moved it.
///
/// `pub(in crate::runtime::interpreter)` so `interpreter.rs`'s `multianewarray` shares it
/// (`alloc_multi_array_collecting`, on this helper since gc-common w3-c).
pub(in crate::runtime::interpreter) fn collect_and_retry<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    mut attempt: impl FnMut(&SharedVm) -> Option<T>,
) -> Option<T> {
    note_ladder_door_entered(shared, LadderDoor::Interpreter);
    oome_ladder_begin(shared, thread, &format!("door=interp site={site}"));
    let r = collect_and_retry_with_thread(shared, thread, site, |s, _| attempt(s));
    if r.is_some() {
        note_ladder_door_allocated(shared, LadderDoor::Interpreter);
        oome_ladder_forget(shared, thread);
    }
    r
}

/// JIT round 22 wave 11, lane memory (the FlipGcRows page):
/// `CRATONVM_DBG_OOME_LADDER=1`, read once per VM into
/// `AllocLadderState::dbg_oome_ladder`. With it, every allocation door's
/// ladder records what it ran ([`oome_ladder_begin`] at the door,
/// [`oome_ladder_step`] at each rung) and the `OutOfMemoryError` chokepoint
/// ([`maybe_dump_heap_on_oom_for`]) prints ONE line for the throwing thread
/// ([`oome_ladder_report_throw`]); without it, one cached load per recorded
/// step and nothing else. Wave 12: every door (interpreter, JIT helpers,
/// native collecting / factory / JNI), not the interpreter's only.
/// Diagnostics only.
pub(crate) fn oome_ladder_dbg(shared: &SharedVm) -> bool {
    *shared
        .mem
        .alloc_ladder
        .dbg_oome_ladder
        .get_or_init(|| false)}

/// The heap state a ladder step is reported with: Free regions, Old and
/// humongous used bytes, and whether a concurrent mark is open (G1); the
/// collection count elsewhere.
pub(in crate::runtime::interpreter::gc_and_alloc) fn oome_ladder_marks(shared: &SharedVm) -> String {
    match &shared.mem.heap {
        _ if shared.mem.heap.is_g1() => {
            let h = &shared.mem.heap;
            let c = h.region_census().unwrap();
            let mirrors = shared.classes.class_mirrors.try_read().map(|m| m.len());
            // JIT r23 w1b L7 (R23W1L7-2): the regions the published JIT pin
            // words hold out of every collection set, and the words.
            let (jit_pinned_regions, jit_pin_words) = h.g1_published_jit_pin_census();
            format!(
                "free={} old={} humongous={} marking={} mirrors={mirrors:?} jit_pinned_regions={jit_pinned_regions} jit_pin_words={jit_pin_words} regions[old={} eden={}/{}B survivor={}/{}B] map={}",
                c.free,
                c.old.0,
                c.humongous.0,
                shared.mem.heap.g1_is_marking_active(),
                c.old.1,
                c.eden.1,
                c.eden.0,
                c.survivor.1,
                c.survivor.0,
                h.g1_region_shape_map()
            )
        }
        _ => format!("collections={}", shared.mem.heap.collection_count()),
    }
}

/// A door's ladder starts (`CRATONVM_DBG_OOME_LADDER` only): drop whatever
/// this thread recorded before (a ladder that allocated, or a rung reached
/// outside any door), then record the door.
pub(crate) fn oome_ladder_begin(shared: &SharedVm, thread: &JvmThread, door: &str) {
    if !oome_ladder_dbg(shared) {
        return;
    }
    let tid = thread.thread_id.0;
    let marks = oome_ladder_marks(shared);
    let mut trace = shared.mem.alloc_ladder.oome_ladder_trace.lock();
    trace.retain(|(t, _)| *t != tid);
    trace.push((tid, format!("enter {door} [{marks}]")));
}

/// Record one step of this thread's ladder, with the heap state after it
/// (`CRATONVM_DBG_OOME_LADDER` only). Bounded: past
/// [`OOME_LADDER_TRACE_CAP`] entries the oldest quarter is dropped (steps a
/// rung recorded outside any door are otherwise never cleared).
pub(crate) fn oome_ladder_step(shared: &SharedVm, thread: &JvmThread, step: &str) {
    if !oome_ladder_dbg(shared) {
        return;
    }
    let marks = oome_ladder_marks(shared);
    let mut trace = shared.mem.alloc_ladder.oome_ladder_trace.lock();
    if trace.len() >= OOME_LADDER_TRACE_CAP {
        trace.drain(..OOME_LADDER_TRACE_CAP / 4);
    }
    trace.push((thread.thread_id.0, format!("{step} [{marks}]")));
}

/// The bound of `AllocLadderState::oome_ladder_trace` (diagnostics only).
pub(in crate::runtime::interpreter::gc_and_alloc) const OOME_LADDER_TRACE_CAP: usize = 4096;

/// The G1 near-full proxy-define page: `CRATONVM_DBG_OOME_LADDER=all`
/// (diagnostics only, read once per VM).
pub(in crate::runtime::interpreter::gc_and_alloc) fn oome_ladder_dbg_all(shared: &SharedVm) -> bool {
    *shared.mem.alloc_ladder.dbg_oome_ladder_all.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .is_ok_and(|v| v.eq_ignore_ascii_case("all"))    })}

/// Drop this thread's recorded steps (its ladder allocated).
pub(in crate::runtime::interpreter::gc_and_alloc) fn oome_ladder_forget(shared: &SharedVm, thread: &JvmThread) {
    if !oome_ladder_dbg(shared) {
        return;
    }
    let tid = thread.thread_id.0;
    let mut trace = shared.mem.alloc_ladder.oome_ladder_trace.lock();
    // The G1 near-full proxy-define page: `=all` prints the ladders that
    // allocated too, when they ran more than their door.
    if oome_ladder_dbg_all(shared) {
        let mine: Vec<&str> = trace
            .iter()
            .filter(|(t, _)| *t == tid)
            .map(|(_, s)| s.as_str())
            .collect();
        if mine.len() > 1 {
            eprintln!(
                "[cratonvm] oome-ladder: thread={tid} ALLOCATED after: {}",
                mine.join(" -> ")
            );
        }
    }
    trace.retain(|(t, _)| *t != tid);
}

/// An `OutOfMemoryError` is about to be thrown on this thread (called from
/// the chokepoint every VM-raised heap OOME passes,
/// [`maybe_dump_heap_on_oom_for`]): print this thread's steps as ONE line,
/// then drop them (`CRATONVM_DBG_OOME_LADDER` only). Nothing recorded (a
/// VM-limit refusal, which runs no ladder) prints nothing.
pub(in crate::runtime::interpreter::gc_and_alloc) fn oome_ladder_report_throw(shared: &SharedVm, thread: &JvmThread, message: &str) {
    if !oome_ladder_dbg(shared) {
        return;
    }
    let tid = thread.thread_id.0;
    let steps: Vec<String> = {
        let mut trace = shared.mem.alloc_ladder.oome_ladder_trace.lock();
        let mine = trace
            .iter()
            .filter(|(t, _)| *t == tid)
            .map(|(_, s)| s.clone())
            .collect();
        trace.retain(|(t, _)| *t != tid);
        mine
    };
    if steps.is_empty() {
        return;
    }
    eprintln!(
        "[cratonvm] oome-ladder: thread={tid} oome=({message}) THROWS after: {} | now [{}]",
        steps.join(" -> "),
        oome_ladder_marks(shared)
    );
}

/// [`collect_and_retry`] for an attempt that needs the calling thread too (a
/// `NativeContextImpl` is built from it). The one escalation; the thread-free
/// form above delegates here.
pub(in crate::runtime::interpreter::gc_and_alloc) fn collect_and_retry_with_thread<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    mut attempt: impl FnMut(&SharedVm, &mut JvmThread) -> Option<T>,
) -> Option<T> {
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    maybe_gc_forced_at(shared, thread, site);
    oome_ladder_step(shared, thread, "forced-collection");
    // GC-overhead limit: if repeated forced GCs have freed almost nothing, the
    // heap is full of live objects -- declare OOM now rather than retrying into
    // a death-spiral (a sliver freed each cycle would otherwise let allocation
    // limp on, GC-thrashing).
    if gc_overhead_limit_exceeded(shared) {
        // G1's productivity streak describes young collections only.  It is
        // therefore not evidence that the whole heap is live: a mutator can
        // drop data after it has been promoted to Old (or a humongous span),
        // and every preceding young collection quite correctly reclaimed
        // nothing.  Before raising OOME, run G1's one bounded whole-heap
        // mark/cleanup attempt.  Besides reclaiming dead Old spans, this arms
        // the one mixed compaction pause when the failed request proved
        // fragmentation.  The retry below remains bounded to one attempt.
        //
        // gcg g3o residual (2026-10-04): the cycle is G1's latched exit
        // ([`g1_latched_overhead_exit`]), the one every door takes on G1 (the
        // native factory, the JIT helpers and the exception door too): one
        // attempt after a productive cycle or on the bounded grace, else the
        // error.
        if shared.mem.heap.is_g1() {
            let before_live = shared.mem.heap.live_bytes_estimate();
            let allowed = g1_latched_overhead_exit(shared, thread, before_live);
            oome_ladder_step(
                shared,
                thread,
                &format!("g1-overhead-latched-exit attempt-allowed={allowed}"),
            );
            if !allowed {
                return None;
            }
            return attempt(shared, thread);
        }
        // ...but not before every SoftReference has been cleared, which
        // `java.lang.ref.SoftReference` guarantees before any OutOfMemoryError
        // (HotSpot's overhead limit clears them too, as its "near" state). The
        // early exit used to skip the whole last-ditch rung, so a thrashing
        // heap full of soft-referenced cache entries threw with the cache
        // still populated (`tools/probes/interp/L5/SoftRefBeforeOome`). At
        // most one extra collection, and none when the application holds no
        // live soft references. (Merged from dev's interpreter round, which
        // added it to the object and array ladders; here it covers all four.)
        let collected = last_ditch_clear_soft_refs(shared, thread);
        // gen r4w4 (orchestrator, 2026-09-24): on Generational the streak
        // stays latched after the program DROPS its data, and the dropped
        // data is in the old generation, which the forced young cycle above
        // does not collect below the 75 % floor. Without a major here the
        // first allocation after `made = null` threw with most of the heap
        // garbage (`GenR4W4NativeStringOomProbe`: `println` after the drop).
        // Serial runs a full collection before every OOME; so do we, once per
        // attempt, and a productive major resets the streak
        // (`note_gc_productivity`). Same request/collect pair as
        // `last_ditch_reclaim`'s Generational step. (G1's last-ditch cycle
        // stayed off this fast-fail exit until gcd d10/o: see the unjudged
        // `Attempt` arm below and `g1_overhead_limit_full_cycle`.)
        //
        // gen r4w5/thrash5 (2026-09-24): and when that full collection leaves
        // the streak latched, this IS the error, as it is in the JIT helpers
        // (which throw on a latched streak without retrying at all). Retrying
        // anyway let an interpreted mutator limp on whatever the young cycle
        // had freed — on a heap thrashing on TLAB filler, enough for the next
        // object every time — and the limit never ended the loop. Only when
        // THIS thread ran that collection (so `note_gc_productivity` judged
        // it): a pause another thread ran may have relieved the heap without
        // being judged, and then the retry decides, as before. A soft-reference
        // collection that did not reset the streak is followed by the major
        // too (it used to stop there), so the error always comes after a full
        // collection. `CRATONVM_GC_OVERHEAD_PROGRESS=0` restores the retry.
        let generational = shared.mem.heap.is_generational();
        let judge_major = generational && gc_overhead_progress_enabled();
        let mut major_ran = false;
        if generational && (!collected || (judge_major && gc_overhead_limit_exceeded(shared))) {
            // gcd d2/j: one attempt, as before, but a request the lost race
            // left pending is withdrawn (`generational_major_on_this_thread`);
            // the `AttemptThenLastDitch` exit below then runs the last-ditch
            // major, which retries.
            //
            // gcd d4/j: and when judged, a SECOND major if the first freed
            // under 2 % of the heap (`majors_to_decide_oome`): one STW major
            // cannot free data a dead OLD object keeps through a young one
            // (young→old nepotism), so a single futile major is no verdict.
            major_ran = if judge_major {
                majors_to_decide_oome(shared, thread, "overhead-limit-major")
            } else {
                generational_major_on_this_thread(shared, thread, "overhead-limit-major", 1)
            };
        }
        // gen r5w5/sizer9: the verdict is honoured only after a FULL collection
        // this call ran — see `overhead_limit_exit`.
        //
        // zr z2/cp: on ZGC (`zgc_overhead_judged`) the forced cycle at the top
        // was whole-heap, so it is that full collection: judged, and the
        // streak re-read after the soft rung decides.
        let zgc_judged = zgc_overhead_judged(shared);
        match overhead_limit_exit(
            judge_major || zgc_judged,
            major_ran || zgc_judged,
            collected,
            ((judge_major && major_ran) || zgc_judged) && gc_overhead_limit_exceeded(shared),
        ) {
            // gce e2/o: one graced attempt before the error, a bounded number
            // of times per latched episode (`latched_overhead_grace`). The
            // major above may have freed exactly what this request needs.
            OverheadLimitExit::Throw => {
                if latched_overhead_grace(shared) {
                    let v = attempt(shared, thread);
                    note_latched_grace_outcome(shared, v.is_some());
                    return v;
                }
                return None;
            }
            // gcd d4/j: judged, a failed attempt after a collection that
            // RESET the streak finishes the ladder below (last-ditch rung,
            // then `second_major_before_oome`) instead of throwing at once;
            // unjudged (`CRATONVM_GC_OVERHEAD_PROGRESS=0`), as before.
            OverheadLimitExit::Attempt if judge_major => {
                if let Some(v) = attempt(shared, thread) {
                    return Some(v);
                }
                // gcd d5/q: straight to the last-ditch rung. The shared
                // attempt below would repeat this one with no collection in
                // between (a native attempt pays an unwind each time).
                return last_ditch_then_second_major(shared, thread, &mut attempt);
            }
            // Generational under `CRATONVM_GC_OVERHEAD_PROGRESS=0` and ZGC
            // (whose forced cycle is whole-heap): the one attempt, byte for
            // byte. gcg g3/o: G1 never reaches this match -- it returns from
            // its own latched arm above (`last_ditch_reclaim`, one attempt) --
            // so the G1 branch gcd d10/o put here (a failed attempt, then
            // `g1_overhead_limit_full_cycle`, then a retry) was dead code and
            // is gone. The native door's G1 latched exit is the live user of
            // that cycle (`native_reclaim_ladder_tail`).
            OverheadLimitExit::Attempt => return attempt(shared, thread),
            // Neither the soft-reference rung nor the major above ran a
            // collection on this thread (it lost the STW race both times, so
            // the thread-local major request was consumed by nobody): the
            // streak's verdict rests on young cycles only, and the old
            // generation may still hold every byte the program dropped. Finish
            // the ladder as the unlatched path does — attempt, then the
            // last-ditch full collection, then attempt — instead of returning
            // the first attempt's `None` as `OutOfMemoryError`.
            OverheadLimitExit::AttemptThenLastDitch => {}
        }
    }
    if let Some(v) = attempt(shared, thread) {
        return Some(v);
    }
    last_ditch_then_second_major(shared, thread, &mut attempt)
}

/// The tail of [`collect_and_retry_with_thread`]: the last-ditch rung, an
/// attempt, the second major, an attempt. Split out (gcd d5/q) so the judged
/// overhead exit reaches it without repeating the attempt it just made.
pub(in crate::runtime::interpreter::gc_and_alloc) fn last_ditch_then_second_major<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    attempt: &mut impl FnMut(&SharedVm, &mut JvmThread) -> Option<T>,
) -> Option<T> {
    // gci i14/ox (`CRATONVM_G1_OOME_PAUSE_FULL_REPORT`, default ON since
    // 2026-10-17; opt-in `CRATONVM_G1_OOME_RETRY_BEFORE_FRESH_CYCLE`): on G1 the rung's two
    // halves with HotSpot's attempt between them (and, under the second, one
    // before the fresh cycle). Otherwise `last_ditch_reclaim`, as before.
    let (split, between) = g1_oome_ladder_shape(
        shared.mem.heap.is_g1(),
        shared.mem.heap.is_g1() && super::gc_events::g1_oome_pause_full_report_enabled(),
        shared.mem.heap.is_g1() && g1_oome_retry_before_fresh_cycle_enabled(),
    );
    if split {
        if let Some(v) = g1_last_ditch_split(shared, thread, &mut *attempt, between) {
            return Some(v);
        }
    } else {
        last_ditch_reclaim(shared, thread);
    }
    if let Some(v) = attempt(shared, thread) {
        return Some(v);
    }
    // gcd d4/j: the error is about to be thrown after ONE full collection;
    // HotSpot's full collection marks both generations from the roots, ours
    // seeds the old mark from every surviving young object, and a young
    // object a DEAD old object keeps (through its card) keeps what it
    // reaches in the old generation for one more major. A second major
    // breaks one such layer; see `second_major_before_oome`.
    //
    // gcd d5/q: still needed after gcd d4/n's true-root seed. That seed frees
    // the dead OLD part in the same major, but the YOUNG objects the dead old
    // ones named through their cards survived the major's young phase (it ran
    // first) and go only at the next collection -- a young array a dropped old
    // list held is exactly what a failing allocation may need.
    if second_major_before_oome(shared, thread, "oome-second-major") {
        return attempt(shared, thread);
    }
    None
}

/// gcd d4/j (2026-09-28): the full collections an allocation ladder runs
/// before it may throw a heap `OutOfMemoryError`, on Generational with
/// `CRATONVM_GC_OVERHEAD_PROGRESS` on (its `=0` restores one).
///
/// # Why two
///
/// A Generational STW major seeds the old-generation mark from every young
/// object that survived the pause's young phase
/// (`GenerationalHeap::mark_young_to_old_refs`), and the young phase keeps
/// every young object an OLD object names through a dirty card, live or
/// dead (minor-collection semantics). So a dead old object that names a
/// young one (a dropped `ArrayList` whose last-grown backing array is young,
/// a dead chain whose newer nodes were tenured in place ahead of older ones)
/// keeps, through that young object, everything it reaches in the old
/// generation alive through the major. The NEXT pause frees the young object
/// (its old referrer was swept), and a second major then frees the rest.
/// HotSpot's Serial full collection marks both generations from the roots and
/// never needs the second. Measured shape: `GenR4W4NativeStringOomProbe`'s
/// `println("native-strings ok")` and `GenR4W4HeapFullThrashProbe`'s
/// `println("threads: ...")` throw from `CharBuffer.wrap` right after the
/// program dropped everything (`gengc-r4w6-review6-heap-full-thrash-jit-and-gc-stress-residual`,
/// gcd d4/j block). The real fix is a full-heap mark for the major the
/// ladder requests (`gcd-d4j-stw-major-keeps-dead-old-data-through-young-nepotism-FIXED-20261012.md`).
///
/// Runs a Generational major on this thread; if it freed less than 2 % of
/// the heap (by the live-bytes estimate), runs one more. Returns whether the
/// first ran on this thread.
///
/// gcd d5/q: `CRATONVM_DBG_GC_OVERHEAD=1` prints one `[GC_OVERHEAD]
/// oome-majors:` line per call, so a crawl's decisions can be counted. (A gate
/// that skipped the second major after a true-root major was considered and
/// rejected: see the note in `last_ditch_then_second_major`.)
pub(crate) fn majors_to_decide_oome(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
) -> bool {
    let cap = shared.mem.heap.heap_capacity();
    let before = shared.mem.heap.live_bytes_estimate();
    if !generational_major_on_this_thread(shared, thread, site, 1) {
        return false;
    }
    let after = shared.mem.heap.live_bytes_estimate();
    // Widening: usize -> u64 is loss-free on every supported target.
    let second = cap != 0 && two_percent_sliver(before.saturating_sub(after) as u64, cap);
    if gc_overhead_dbg_on() {
        eprintln!(
            "[GC_OVERHEAD] oome-majors: site={site} thread={} before={before} after={after} \
             cap={cap} second_major={second}",
            thread.thread_id.0,
        );
    }
    // gcd d9/b: `[GC] oome_ladder: ladder_deciding`, `ladder_second_majors`.
    ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::DECIDING, 1);
    if second && generational_major_on_this_thread(shared, thread, site, 1) {
        ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::SECOND_MAJORS, 1);
    }
    true
}

/// gcd d5/q: how many funnel payments one raised heap `OutOfMemoryError`
/// buys its raising thread (see [`oome_debt_decision`]). Two bits of the debt
/// word.
pub(in crate::runtime::interpreter::gc_and_alloc) const OOME_DEBT_PAYMENTS: u64 = 2;

/// gcd d5/q: the key a debt word stores for `thread`: its id plus one (so `0`
/// stays "nothing owed"), shifted over the two payment bits. Ids at or above
/// 2^62 alias, which only makes a debt payable by an aliasing thread.
pub(in crate::runtime::interpreter::gc_and_alloc) fn oome_debt_key(thread: &JvmThread) -> u64 {
    thread.thread_id.0.wrapping_add(1) << 2
}

/// gcd d4/j: record that a heap `OutOfMemoryError` (`message` = its
/// Java-visible text) is being raised, so the native funnel collects before
/// its next callback. `Requested array size exceeds VM limit` and every other
/// non-heap message are not recorded: no collection helps them. Generational
/// and G1 arm debt; the G1 switch can turn it off.
///
/// gcd d5/q: the debt now names the RAISING thread and carries
/// [`OOME_DEBT_PAYMENTS`] payments; a later raise (any thread) replaces it.
pub(crate) fn note_heap_oome_raised(shared: &SharedVm, thread: &JvmThread, message: &str) {
    if message.starts_with("Java heap space") {
        // gcd d9/b: `[GC] oome_ladder: ladder_oomes` (whatever the switch).
        ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::OOMES, 1);
        // zt t24/hunt7: ZGC's `-Xlog:gc` `Out Of Memory (<thread>)` line
        // (`CRATONVM_ZGC_XLOG_OUT_OF_MEMORY`; a no-op off ZGC).
        super::gc_events::zgc_out_of_memory_logged(shared, &thread.name);
        // gcf f4/o: a graced native retry of this thread, if pending, failed.
        clear_graced_native_retry(shared, thread);
        // zr z8/nr: a room skip of this thread still pending means its
        // caller raised without repaying the clear-soft rung (a caller that
        // does not call `zgc_native_repay_skipped_soft_rung`): count it.
        let census = &shared.mem.tlab_wedge.zgc_refusal;
        let key = oome_debt_key(thread);
        if census
            .soft_skip_pending
            .load(std::sync::atomic::Ordering::Relaxed)
            == key
            && census
                .soft_skip_pending
                .compare_exchange(
                    key,
                    0,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
        {
            census
                .soft_skip_unpaid
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if message.starts_with("Java heap space")
        && native_oome_debt_armed_on(shared)
        && gc_overhead_progress_enabled()
    {
        let key = oome_debt_key(thread);
        if key != 0 {
            shared.mem.alloc_ladder.oome_native_debt.store(
                key | OOME_DEBT_PAYMENTS,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }
}

/// gcd d5/q: what the native funnel does with a debt word. Pure.
///
/// * `word` -- the debt (`0` = none owed).
/// * `key` -- the calling thread's [`oome_debt_key`].
/// * `recovered` -- the old generation has room again (at least an eighth of
///   the heap): some collection since the raise freed what the program dropped.
///
/// Returns `(pay, next)`: whether to collect now, and the word to store.
///
/// * Nothing owed, or another thread's debt: no payment, word untouched. A
///   JDK daemon's native call (the Reference Handler re-enters its native
///   after every collection) used to take the debt at a random moment --
///   while the program that caught the error had not dropped anything yet --
///   run its majors on a heap full of live data, and leave nothing for the
///   raising thread's first native after the drop.
/// * Recovered: nothing to collect for; the debt is cleared.
/// * Otherwise pay, and keep one payment fewer. A payment that leaves the heap
///   full (the error's own construction runs natives on the raising thread
///   before its handler has dropped anything) therefore still leaves one for
///   the native after the drop; a productive one is cleared by the next call's
///   `recovered`. At most [`OOME_DEBT_PAYMENTS`] payments per raised error.
pub(in crate::runtime::interpreter::gc_and_alloc) fn oome_debt_decision(word: u64, key: u64, recovered: bool) -> (bool, u64) {
    if word == 0 || word & !3 != key {
        return (false, word);
    }
    if recovered {
        return (false, 0);
    }
    let left = word & 3;
    if left == 0 {
        return (false, 0);
    }
    let next = if left <= 1 { 0 } else { word - 1 };
    (true, next)
}

/// gcd d4/j, `gcd-d3o-native-door-first-allocation-throws-without-a-collection`
/// shapes A and B: asked by the native funnel (`vm_exec::safe_native_call_impl`)
/// before its callback, with every argument pinned. `true` = a heap
/// `OutOfMemoryError` was raised since the funnel last collected for one, so
/// run [`majors_to_decide_oome`] now: a native's allocation gets ONE attempt
/// and never collects, and after a program catches the error and drops its
/// data nothing else may have collected (the streak is still latched, so the
/// funnel's young-pressure relief is skipped, shape A; or nothing armed it,
/// shape B). One relaxed load when nothing is owed. Off (always `false`) under
/// `CRATONVM_GC_OVERHEAD_PROGRESS=0` and off Generational.
///
/// gcd d5/q: only the thread that raised the error pays, only while the old
/// generation is still short of room, and at most twice per raised error --
/// see [`oome_debt_decision`].
pub fn native_call_owes_oome_major(shared: &SharedVm, thread: &JvmThread) -> bool {
    use std::sync::atomic::Ordering;
    let debt = &shared.mem.alloc_ladder.oome_native_debt;
    let word = debt.load(Ordering::Relaxed);
    if word == 0 {
        return false;
    }
    let key = oome_debt_key(thread);
    if word & !3 != key {
        return false; // another thread's error; one load, no heap probe
    }
    // gcf f4/o: a native inside the construction of the error itself does not
    // pay (nor consume) the debt under `CRATONVM_GC_OOME_DEBT_SKIP_CONSTRUCTION`
    // -- the heap is the one the verdict was just taken on, and nothing has
    // been dropped yet. The first native after the construction pays.
    if thread.oome_construct_depth > 0 && oome_debt_skip_construction() {
        if gc_overhead_dbg_on() {
            eprintln!(
                "[GC_OVERHEAD] native-oome-debt: thread={} skipped=construction",
                thread.thread_id.0
            );
        }
        return false;
    }
    // Never armed off Generational (unless `native_oome_debt_armed_on`) or
    // with the switch off (`note_heap_oome_raised`); defensive for a switch
    // flipped in between.
    if !native_oome_debt_armed_on(shared) || !gc_overhead_progress_enabled() {
        let _ = debt.compare_exchange(word, 0, Ordering::Relaxed, Ordering::Relaxed);
        return false;
    }
    let cap = shared.mem.heap.heap_capacity();
    let recovered = shared.mem.heap.old_gen_headroom() >= cap / 8;
    let (pay, next) = oome_debt_decision(word, key, recovered);
    let taken = next == word
        || debt
            .compare_exchange(word, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();
    if gc_overhead_dbg_on() {
        eprintln!(
            "[GC_OVERHEAD] native-oome-debt: thread={} recovered={recovered} pay={} left={}",
            thread.thread_id.0,
            pay && taken,
            next & 3,
        );
    }
    if pay && taken {
        // gcd d9/b: `[GC] oome_ladder: ladder_debt_payments`.
        ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::DEBT_PAYMENTS, 1);
        // gcf f1/o: under `CRATONVM_DBG=oldmark-root-census`, the compiled
        // activations (with their tiers) live at the first post-OOME native.
        // The `[holder-census]` blocks of the majors that follow are the
        // post-catch ones the d6u page asks about.
        if crate::jit::helpers::oldmark_census_on() {
            crate::jit::helpers::oldmark_census_frames_line("native-after-oome-major");
        }
    }
    pay && taken
}

/// gcf f4/o (2026-10-01), `gcd-d9b-proposal-arm-the-native-oome-debt-after-delivery`
/// (its "alternative with no dispatcher hook", which also covers compiled
/// catch blocks -- they never pass the interpreter's dispatcher):
/// `CRATONVM_GC_OOME_DEBT_SKIP_CONSTRUCTION=1` (default OFF; `runtime_flag_on`).
/// While a thread builds a VM-raised `OutOfMemoryError`
/// (`exceptions::create_vm_oome_object`, `JvmThread::oome_construct_depth`),
/// its natives skip the debt instead of paying two futile majors on the
/// unchanged heap; the debt stays armed, with both payments, for the first
/// native after the construction. Read only when a debt is owed by this thread.
/// gcg g6/oo: default ON (`runtime_flag_default_on`, `=0` restores paying
/// during the construction): vg5 `oo5_debt_sk_*` showed `skipped=construction`
/// (2, 1) and fewer `pay=true` lines than the default arm (3 vs 5), stdout SAME
/// 4/4, wall shorter in 2/2.
pub(crate) fn oome_debt_skip_construction() -> bool {
    true
}

/// Is the native funnel's post-OOME debt armed on this VM's collector?
/// Generational always (gcf f5/o); G1 under `CRATONVM_G1_NATIVE_OOME_DEBT=1` (gch h4/g1, opt-in until
/// vh4-g1 block F decides: `o1b_jni_g1*` was G1's reproducer). Paid by `pay_native_oome_debt`'s G1
/// arm, the complete marking cycle. ZGC: never (vf1 o-4: its forced cycles already free the data).
/// See `gcd-d10o-native-funnel-oome-debt-is-generational-only`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn native_oome_debt_armed_on(shared: &SharedVm) -> bool {
    shared.mem.heap.is_generational() || (shared.mem.heap.is_g1())
}



/// gcf f1/o: the native funnel's payment of the post-OOME debt
/// (`vm_exec::safe_native_call_impl`, after [`native_call_owes_oome_major`]
/// said yes), per collector -- the d10o page's proposed fix:
///
/// * Generational: [`majors_to_decide_oome`], byte for byte the call the
///   funnel made before (site `native-after-oome-major`);
/// * G1: the complete marking cycle ([`g1_last_ditch_full_cycle`]): a young
///   pause would not reach the dead Old regions the program dropped, and two
///   of them (what `majors_to_decide_oome` asks for, the major request being
///   Generational's) would cost without freeing them;
/// * ZGC: one forced cycle, which is whole-heap.
///
/// Off Generational this is reached on G1 under `CRATONVM_G1_NATIVE_OOME_DEBT=1` (gch h4/g1);
/// the ZGC arm is kept for a future per-collector debt.
/// The caller holds every argument pinned and re-reads them afterwards.
pub fn pay_native_oome_debt(shared: &SharedVm, thread: &mut JvmThread) {
    if shared.mem.heap.is_generational() {
        // gcf f3/o (vf1 o-1): drop a summary a pre-payment major left here, so
        // the line below names THIS payment's majors only.
        let census = crate::jit::helpers::oldmark_census_on();
        if census {
            let _ = cratonvm_gc::gen_heap::take_oldmark_census_last_holders();
        }
        let _ = majors_to_decide_oome(shared, thread, "native-after-oome-major");
        if census {
            for line in oome_holder_lines(
                thread.thread_id.0,
                &cratonvm_gc::gen_heap::take_oldmark_census_last_holders(),
            ) {
                eprintln!("{line}");
            }
        }
    } else if shared.mem.heap.is_g1() {
        flush_tlab_allocation_batch(thread, shared);
        thread.tlab.retire();
        g1_last_ditch_full_cycle(shared, thread);
    } else {
        let _ = maybe_gc_forced_collected(shared, thread, "native-after-oome-cycle");
    }
}

/// gcf f3/o (vf1 o-1, `gcd-d6u-pinned-callee-oome-heap-stays-full-after-the-catch`):
/// the `[GC] oome-holder:` lines printed after the native funnel's post-OOME
/// majors under `CRATONVM_DBG=oldmark-root-census`: one per marker the LAST of
/// those majors explained, naming its first holder; `none` when that major
/// explained nothing (or did not run on this thread). `[GC]`-prefixed and one
/// line each so a cut err keeps them (the full `[holder-census]` block sits in
/// the middle of the run). Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn oome_holder_lines(tid: u64, summary: &[String]) -> Vec<String> {
    if summary.is_empty() {
        return vec![format!(
            "[GC] oome-holder: site=native-after-oome-major thread={tid} none"
        )];
    }
    summary
        .iter()
        .map(|s| format!("[GC] oome-holder: site=native-after-oome-major thread={tid} {s}"))
        .collect()
}

/// gcd d4/j: the ladder's last rung, after `last_ditch_reclaim` and its
/// attempt failed: one more Generational major on this thread (see
/// [`majors_to_decide_oome`] for why one is not a verdict). `true` when it
/// ran, so the caller attempts once more. Off Generational and under
/// `CRATONVM_GC_OVERHEAD_PROGRESS=0`, nothing (the historical ladder).
pub(crate) fn second_major_before_oome(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
) -> bool {
    shared.mem.heap.is_generational()
        && gc_overhead_progress_enabled()
        && generational_major_on_this_thread(shared, thread, site, 1)
}

/// How [`collect_and_retry_with_thread`]'s overhead-limit arm ends, once its
/// soft-reference rung and its Generational major have run (or not).
/// gen r5w5/sizer9.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::gc_and_alloc) enum OverheadLimitExit {
    /// A full collection this call ran left the streak latched: the error
    /// (gen r4w5/thrash5's guarantee — a genuinely wedged loop throws instead
    /// of limping on what the young cycle freed).
    Throw,
    /// One more attempt decides, as before.
    Attempt,
    /// No collection ran on this thread in the arm: the unlatched tail
    /// (attempt, last-ditch reclaim, attempt).
    AttemptThenLastDitch,
}

/// The pure decision behind [`OverheadLimitExit`].
///
/// * `judge_major` — Generational with `CRATONVM_GC_OVERHEAD_PROGRESS` on (the
///   arm that runs a major before the error); off, the historical exit
///   (`Attempt`) byte for byte.
/// * `major_ran` / `soft_collected` — whether THIS thread ran the
///   overhead-limit major / the soft-reference rung's collection (which
///   requests a major too).
/// * `latched_after_major` — the verdict re-read after that major.
///
/// The one new answer is `AttemptThenLastDitch`, for a judged arm in which no
/// full collection ran at all: the verdict then rests on the young cycles
/// that built the streak, and the old generation — where a program's dropped
/// data sits after an `OutOfMemoryError` — was never collected. Returning the
/// first attempt's `None` there threw with the heap reclaimable
/// (`docs/feature-designs/generative-gc-architecture.md`,
/// mechanism B).
pub(in crate::runtime::interpreter::gc_and_alloc) fn overhead_limit_exit(
    judge_major: bool,
    major_ran: bool,
    soft_collected: bool,
    latched_after_major: bool,
) -> OverheadLimitExit {
    if judge_major && major_ran && latched_after_major {
        OverheadLimitExit::Throw
    } else if judge_major && !major_ran && !soft_collected {
        OverheadLimitExit::AttemptThenLastDitch
    } else {
        OverheadLimitExit::Attempt
    }
}

/// gcg g3/o, step 1 of `gcd-d10o-proposal-oome-ladder-census-on-every-collector`:
/// the JIT helpers' latched-exit answer through the interpreter's and the
/// native door's own decision ([`overhead_limit_exit`]) -- `true` = throw now.
/// Argument order is the JIT verdict's (`jit_overhead_limit_verdict` in
/// `vm/src/jit/helpers.rs`). It differs from that verdict in exactly two cells,
/// both toward the interpreter (one more attempt instead of the error):
///
/// 1. unjudged (ZGC; Generational under `CRATONVM_GC_OVERHEAD_PROGRESS=0`),
///    the soft rung collected nothing: the JIT threw without an attempt;
/// 2. judged, the soft rung collected, the overhead major lost its STW race
///    (`major_ran == false`), the streak still latched: the JIT threw on the
///    soft rung's single major.
///
/// Used by the helpers only under `CRATONVM_GC_JIT_LADDER_UNIFIED`
/// ([`jit_ladder_unified_enabled`]; wired in `jit_latched_overhead_limit_throws`
/// by the g3 cross-lane hunk).
pub(crate) fn jit_overhead_limit_throws_unified(
    judge: bool,
    soft_collected: bool,
    major_ran: bool,
    latched_now: bool,
) -> bool {
    overhead_limit_exit(judge, major_ran, soft_collected, latched_now) == OverheadLimitExit::Throw
}

/// gcg g3/o: `CRATONVM_GC_JIT_LADDER_UNIFIED`. gcg g6/oo: default ON
/// (`runtime_flag_default_on`, `=0` restores the JIT's own verdict): vg3 o3-3
/// (HeapFullThrash on = off 3/3 on Gen and ZGC, no rc 124) and vg5
/// `oo5_jitlad_jor_*` (`PASS all 8` 5/5 on = 5/5 off). Read on the latched
/// exit only, never on an allocation that succeeds.
pub(crate) fn jit_ladder_unified_enabled() -> bool {
    true
}

/// gce e2/o: graced attempts per latched GC-overhead episode.
///
/// `GenR4W4NativeStringOomProbe -Xmx64m` died 1 run in 20 (on the base AND
/// e1 binaries) of an uncaught `OutOfMemoryError` at its line 86 -- the
/// `println` in the fill's own `catch`, right after the program released 64
/// blocks (64 KiB) so that it could print. HotSpot Serial runs a full
/// collection there, which frees the sliver, and the `println` allocates. Here
/// the fill leaves the overhead streak LATCHED; the `println`'s ladder runs
/// its forced collection and the deciding major, both "unproductive" (64 KiB
/// is under 2 % of the heap), so the `Throw` exit fired WITHOUT an attempt --
/// an `OutOfMemoryError` on a heap that the collection had just made able to
/// serve the request. Whether the fill ended latched is timing (how many
/// forced cycles the last stretch of the fill ran), hence 1 in 20.
///
/// The `Throw` exit exists (gen r4w5/thrash5, r5w2's `chain-hot`) because an
/// UNBOUNDED retry lets a wedged program limp on one object per major forever.
/// So the grace is bounded: at most this many graced attempts per latched
/// episode (cleared when the streak resets); a wedged loop pays at most this
/// many extra attempts before the error, as before.
pub(in crate::runtime::interpreter::gc_and_alloc) const LATCHED_GRACE_ATTEMPTS: u32 = 8;

/// gce e2/o: may the latched GC-overhead exit try the allocation once more
/// before throwing? Takes one unit of this episode's budget
/// ([`LATCHED_GRACE_ATTEMPTS`]) when it answers yes. `CRATONVM_GC_LATCHED_GRACE=0`
/// answers no (the gen r4w5 exit byte for byte). Counted on `[GC] oome_ladder:`
/// (`ladder_latched_grace=`).
pub(crate) fn latched_overhead_grace(shared: &SharedVm) -> bool {
    let granted = latched_grace_take(&shared.mem.alloc_ladder.latched_grace_used);
    if granted {
        ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::LATCHED_GRACE, 1);
    }
    granted
}

/// The budget arithmetic of [`latched_overhead_grace`], pure over the counter.
pub(in crate::runtime::interpreter::gc_and_alloc) fn latched_grace_take(used: &std::sync::atomic::AtomicU32) -> bool {
    use std::sync::atomic::Ordering;
    used.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        (n < LATCHED_GRACE_ATTEMPTS).then_some(n + 1)
    })
    .is_ok()
}

/// gce e2/o: count a graced attempt that allocated (`ladder_latched_grace_ok=`).
pub(crate) fn note_latched_grace_outcome(shared: &SharedVm, allocated: bool) {
    if allocated {
        ladder_census(
            shared,
            cratonvm_gc::gen_heap::oome_ladder::LATCHED_GRACE_OK,
            1,
        );
    }
}

/// gcf f3/o, the fifth door of `gcf-f1o-latched-grace-ok-is-counted-by-one-door-of-five`:
/// the `NativeContext::reclaim_before_alloc_retry` door's retry is made by the
/// native factory itself, which cannot report its outcome. The door records
/// the graced retry here (this thread's key); the thread's next successful
/// native allocation counts it ([`note_native_alloc_succeeded`]).
pub(crate) fn note_graced_native_retry(shared: &SharedVm, thread: &JvmThread) {
    shared
        .mem
        .alloc_ladder
        .graced_native_retry
        .store(oome_debt_key(thread), std::sync::atomic::Ordering::Relaxed);
    // gcf f4/o: the fifth door's control line (row `o4_grace5_*`).
    if gc_overhead_dbg_on() {
        eprintln!(
            "[GC_OVERHEAD] latched-grace: door=native thread={}",
            thread.thread_id.0
        );
    }
}

/// gcf f3/o: a native allocation on `thread` succeeded; if it was this
/// thread's graced retry, count it (`ladder_latched_grace_ok=`). One relaxed
/// load when nothing is pending.
#[inline]
pub(crate) fn note_native_alloc_succeeded(shared: &SharedVm, thread: &JvmThread) {
    use std::sync::atomic::Ordering;
    // zr z8/nr: a served native allocation ends this thread's pending room
    // skip (one relaxed load when none is pending).
    zgc_native_room_skip_served(shared, thread);
    let pending = &shared.mem.alloc_ladder.graced_native_retry;
    let key = pending.load(Ordering::Relaxed);
    if key != 0
        && key == oome_debt_key(thread)
        && pending
            .compare_exchange(key, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        note_latched_grace_outcome(shared, true);
        if gc_overhead_dbg_on() {
            eprintln!(
                "[GC_OVERHEAD] latched-grace-ok: door=native thread={}",
                thread.thread_id.0
            );
        }
    }
}

/// gcf f4/o: a heap `OutOfMemoryError` raised on `thread` ends its pending
/// graced native retry, if any: that retry failed. Without this a failed
/// graced retry stayed pending and the thread's next successful native
/// allocation -- arbitrarily later -- was counted as the grace's success.
pub(in crate::runtime::interpreter::gc_and_alloc) fn clear_graced_native_retry(shared: &SharedVm, thread: &JvmThread) {
    use std::sync::atomic::Ordering;
    let key = oome_debt_key(thread);
    let _ = shared
        .mem
        .alloc_ladder
        .graced_native_retry
        .compare_exchange(key, 0, Ordering::Relaxed, Ordering::Relaxed);
}

/// gcf f4/o: the allocation-failure doors of the `[GC] oome_ladder_doors:`
/// census (`AllocLadderState::door_census`). Each door keeps its own ladder;
/// the census is how a drift between them shows on one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderDoor {
    /// `collect_and_retry` (interpreter `new` / `newarray` / the String
    /// ladders / `multianewarray`).
    Interpreter = 0,
    /// `native_alloc_collecting` (natives that collect through the
    /// interpreter's ladder).
    NativeCollecting = 1,
    /// `NativeContext::reclaim_before_alloc_retry` (native factories); its
    /// second slot counts granted retries, not allocations.
    NativeFactory = 2,
    /// The JIT allocation helpers (`jit_newarray_body`, `jit_new_object_body`,
    /// `jit_anewarray_object_body`), from their first failed fallible attempt.
    Jit = 3,
    /// `native::jni::jni_alloc_or_oom` (cross-lane: lane j's file).
    Jni = 4,
}

impl LadderDoor {
    /// Every door, in census order, with its line key.
    pub const ALL: [(LadderDoor, &'static str); 5] = [
        (LadderDoor::Interpreter, "interp"),
        (LadderDoor::NativeCollecting, "native"),
        (LadderDoor::NativeFactory, "factory"),
        (LadderDoor::Jit, "jit"),
        (LadderDoor::Jni, "jni"),
    ];
}

/// gcf f4/o: count one ladder entry of `door`.
#[inline]
pub fn note_ladder_door_entered(shared: &SharedVm, door: LadderDoor) {
    shared.mem.alloc_ladder.door_census[door as usize][0]
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// gcf f4/o: count one ladder of `door` that ended with the allocation (the
/// factory door: with its retry granted).
#[inline]
pub fn note_ladder_door_allocated(shared: &SharedVm, door: LadderDoor) {
    shared.mem.alloc_ladder.door_census[door as usize][1]
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// gcf f4/o: the `[GC] oome_ladder_doors:` shutdown line, every collector.
pub fn oome_ladder_doors_line(shared: &SharedVm) -> String {
    use std::fmt::Write as _;
    let mut line = String::from("[GC] oome_ladder_doors:");
    for (door, key) in LadderDoor::ALL {
        let c = &shared.mem.alloc_ladder.door_census[door as usize];
        let second = if door == LadderDoor::NativeFactory {
            "retry"
        } else {
            "ok"
        };
        let _ = write!(
            line,
            " {key}_enter={} {key}_{second}={}",
            c[0].load(std::sync::atomic::Ordering::Relaxed),
            c[1].load(std::sync::atomic::Ordering::Relaxed),
        );
    }
    line
}

/// zr z3/lt: the `[GC] zgc-jit-box:` shutdown line, ZGC only (`None` on the
/// other collectors, whose boxing helpers keep the infallible door). The
/// engagement counters of `jit_box_cold_alloc` (`vm/src/jit/helpers.rs`):
/// `collects=` refusals of the fallible door that went on to the collect-and-
/// retry rung, `funnel_fallthroughs=` refusals that survived it and were
/// handed to the native funnel, which throws the `OutOfMemoryError`. Both stay
/// `0` on a run that never filled the heap from a compiled boxing loop.
pub fn zgc_jit_box_line(shared: &SharedVm) -> Option<String> {
    if !shared.mem.heap.is_zgc() {
        return None;
    }
    let l = &shared.mem.alloc_ladder;
    Some(format!(
        "[GC] zgc-jit-box: collects={} funnel_fallthroughs={}",
        l.jit_box_collects
            .load(std::sync::atomic::Ordering::Relaxed),
        l.jit_box_funnel_fallthroughs
            .load(std::sync::atomic::Ordering::Relaxed),
    ))
}


/// gce e2/o: a reset streak ends the latched episode and refills the grace.
pub(in crate::runtime::interpreter::gc_and_alloc) fn reset_latched_grace(shared: &SharedVm) {
    shared
        .mem
        .alloc_ladder
        .latched_grace_used
        .store(0, std::sync::atomic::Ordering::Relaxed);
}


/// Run a `NativeContextImpl` allocation from VM-internal code OUTSIDE a native
/// callback -- invokedynamic linkage, `ldc` constant resolution -- under the
/// same heap-exhaustion contract as `new`: attempt, and on exhaustion collect,
/// retry, and finally report `OutOfMemoryError` on the caller's own channel.
///
/// gc-common w4-c (`handoff-w3c-infallible-callers.md` item 1). Such a context
/// used to reach the native allocators' INFALLIBLE arm (`native_oom::unwind_ok`
/// is false outside `safe_native_call_impl`), which aborts the process on
/// Generational and ZGC and, on G1, spends the emergency reserve that the
/// callers which truly cannot report failure (mirrors, JNI mints, JIT helpers)
/// depend on. Here the attempt runs inside `native_oom::catch_alloc_oom`, so
/// the allocators take their fallible arm and unwind to this frame; pins the
/// attempt pushed before it unwound are dropped, as the native funnel does.
///
/// The pending-OOM flag the handoff sketched was not needed: the caller that
/// can report failure simply receives it, and the collection it was missing
/// is the shared ladder ([`collect_and_retry_with_thread`]). A request refused
/// for its LENGTH (`Requested array size exceeds VM limit`) is reported at
/// once, without collecting, as the interpreter's `newarray` does.
///
/// GC-safety contract for callers, the same one [`collect_and_retry`] states:
/// the attempt is re-run after a collection, so it must re-derive anything
/// address-shaped it uses (read pinned roots back, never capture a bare
/// `ObjectRef`), and the caller must hold no unpinned `ObjectRef` in a Rust
/// local across this call. `attempt` must also be safe to run more than once
/// (no Java-visible side effect before its allocations): an attempt that
/// calls into Java is not a candidate.
pub(crate) fn native_alloc_collecting<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    mut attempt: impl FnMut(&SharedVm, &mut JvmThread) -> T,
) -> Result<T, RuntimeError> {
    let last_oom: std::cell::Cell<Option<crate::runtime::native_oom::NativeAllocOom>> =
        std::cell::Cell::new(None);
    let mut catching = |s: &SharedVm, t: &mut JvmThread| -> Option<T> {
        let pin_base = t.native_pin_roots.len();
        match crate::runtime::native_oom::catch_alloc_oom(|| attempt(s, &mut *t)) {
            Ok(v) => Some(v),
            Err(oom) => {
                t.native_pin_roots.truncate(pin_base);
                last_oom.set(Some(oom));
                None
            }
        }
    };
    if let Some(v) = catching(shared, thread) {
        return Ok(v);
    }
    let length_refusal = last_oom
        .get()
        .is_some_and(|oom| oom.what != "object" && array_length_exceeds_vm_limit(oom.length));
    if !length_refusal {
        note_ladder_door_entered(shared, LadderDoor::NativeCollecting);
        oome_ladder_begin(shared, thread, &format!("door=native-collecting site={site}"));
        if let Some(v) = collect_and_retry_with_thread(shared, thread, site, &mut catching) {
            note_ladder_door_allocated(shared, LadderDoor::NativeCollecting);
            return Ok(v);
        }
        maybe_dump_heap_on_oom(shared, thread);
    } else {
        // gen r4w4/oom: HotSpot dumps for the VM-limit refusal as well (see
        // `maybe_dump_heap_on_oom_for`); no collection either way.
        maybe_dump_heap_on_oom_for(shared, thread, ARRAY_SIZE_EXCEEDS_VM_LIMIT);
    }
    Err(match last_oom.get() {
        Some(oom) => oom.into_runtime_error(),
        None => RuntimeError::OutOfMemoryError {
            message: format!("Java heap space ({site})"),
        },
    })
}

/// gci i16/oo (`gci-i15orch-g1-ooms-on-the-mixed-names-probe-at-96m-where-hotspot-does-not-FIXED-20261017`,
/// defect 2): run a VM-side delivery that builds Java objects through a
/// `NativeContextImpl` OUTSIDE a native callback -- the post-collection GC
/// notification and pool-sensor drain (`gc_events::run_gc_notifications`) --
/// with the allocators' unwinding arm armed, so a heap that cannot hold the
/// delivery's objects fails THAT delivery instead of aborting the process.
///
/// # The abort
///
/// `jmx::deliver_gc_notification` builds the `GcInfo` in Rust:
/// `build_gc_info`'s two `MemoryUsage[]` (`new_ref_array`, one slot per pool:
/// eight since i15/nh added the non-heap pools, 16 + 8 * 8 = 80 bytes), the
/// `MemoryUsage` objects and the three name strings. The drain is not under
/// `safe_native_call_impl`, so `native_oom::unwind_ok()` is false there and
/// `new_ref_array` took its INFALLIBLE arm. On a G1 heap that the pause before
/// the drain left with no Free region (an evacuation failure at 92 of 96 MB),
/// that arm found nothing and aborted:
/// `FATAL: G1: out of heap space for array allocation (80 bytes)` (vi15-rf
/// `rf15_xph_g1`, i15a). Generational and ZGC abort on the same arm.
///
/// # The rule
///
/// HotSpot builds the notification's `GcInfo` on its Notification Thread with
/// ordinary Java allocations; one that fails leaves a pending
/// `OutOfMemoryError` and that notification is not sent. Here the delivery runs
/// inside [`crate::runtime::native_oom::catch_alloc_oom`]: the allocators take
/// their fallible arm, a refusal unwinds to this frame, the pins the delivery
/// pushed are dropped (as the native funnel and [`native_alloc_collecting`]
/// do), and the caller gets an `OutOfMemoryError` call failure, which the
/// drain counts as a failed delivery. No collection here: the drain is itself
/// reached from a collection's tail, and a collecting retry whose own
/// collections queue more notifications would not be bounded.
///
/// Sound for the reason `catch_alloc_oom` is: the unwind stops here and never
/// crosses a JIT-compiled frame (`JitEntryGuard` suspends the permission for
/// any compiled frame the delivery enters; a native it reaches through
/// `safe_native_call_impl`, and interpreted Java's own `catch_unwind`s, catch
/// first). A Rust-side allocation of the delivery always precedes the Java
/// call that sends the notification (`createGCNotification`,
/// `Sensor.trigger`), so a refused delivery sent nothing. Any other panic is
/// resumed unchanged.
///
/// gci i18/na (rv finding 6 of
/// `gci-i17rv-i16-default-path-review-findings-RETIRED-UNFIXED-20261018`): the scope and the
/// pin truncation are `native_oom::catch_alloc_oom_on`'s, one body for every
/// VM-internal context built outside the funnel; this function only adds the
/// `MethodCallFailed` mapping of a refusal.
#[cfg_attr(not(feature = "management"), allow(dead_code))]
pub(crate) fn deliver_catching_native_alloc_oom<T>(
    thread: &mut JvmThread,
    deliver: impl FnOnce(&mut JvmThread) -> Result<T, crate::error::MethodCallFailed>,
) -> Result<T, crate::error::MethodCallFailed> {
    crate::runtime::native_oom::catch_alloc_oom_on(thread, deliver).unwrap_or_else(|oom| {
        Err(crate::error::MethodCallFailed::InternalError(
            crate::error::VmError::Runtime(oom.into_runtime_error()),
        ))
    })
}

/// What [`native_reclaim_before_alloc_retry_graced`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeReclaimVerdict {
    /// The caller's one retry is worth making.
    pub retry: bool,
    /// The retry runs on the latched overhead exit's grace
    /// ([`latched_overhead_grace`]): a caller that sees its retry allocate
    /// reports it with [`note_latched_grace_outcome`] (gcf f2/o,
    /// `gcf-f1o-latched-grace-ok-is-counted-by-one-door-of-five`).
    pub graced: bool,
}

/// [`native_reclaim_before_alloc_retry`], also saying whether the retry is a
/// graced one. For the callers that make the retry themselves and can count
/// its outcome (the JNI door, `native::jni::jni_alloc_or_oom`).
pub(crate) fn native_reclaim_before_alloc_retry_graced(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> NativeReclaimVerdict {
    native_reclaim_before_alloc_retry_graced_for(shared, thread, 0)
}

/// gci i13/oa item 4: [`native_reclaim_before_alloc_retry_graced`] for a
/// refused allocation of `bytes` (header included; `0` = unknown or not an
/// array). A G1 humongous request runs its young collection on site
/// [`G1_HUMONGOUS_ALLOC_SITE`] (cause `G1 Humongous Allocation`, as the
/// interpreter's and the JIT's array doors already do); every other request
/// on `vm-exec`, as before ([`native_door_site`]).
pub(crate) fn native_reclaim_before_alloc_retry_graced_for(
    shared: &SharedVm,
    thread: &mut JvmThread,
    bytes: usize,
) -> NativeReclaimVerdict {
    let plain = |retry: bool| NativeReclaimVerdict {
        retry,
        graced: false,
    };
    if !gc_overhead_progress_enabled() {
        return plain(native_reclaim_before_alloc_retry_historical(shared, thread));
    }
    let before_live = shared.mem.heap.live_bytes_estimate();
    // gcg h1/ob (`gcg-g4oo-oome-ladder-review-findings` item 7): no flush and
    // retire of the TLAB here -- the forced door does both first thing
    // (`maybe_gc_forced_collected`), and `before_live` is read before either.
    oome_ladder_begin(shared, thread, &format!("door=native bytes={bytes}"));
    maybe_gc_forced_at(shared, thread, native_door_site(shared, bytes));
    oome_ladder_step(shared, thread, "forced-collection");
    native_reclaim_ladder_tail(shared, thread, before_live)
}

/// The rungs of [`native_reclaim_before_alloc_retry_graced`] after its young
/// (forced) collection: the unlatched last-ditch reclaim and second major, or
/// the latched overhead arm. Shared with the staged door
/// ([`native_reclaim_after_young_stage`]) so the two cannot drift. gcg g3/o.
pub(in crate::runtime::interpreter::gc_and_alloc) fn native_reclaim_ladder_tail(
    shared: &SharedVm,
    thread: &mut JvmThread,
    before_live: usize,
) -> NativeReclaimVerdict {
    let plain = |retry: bool| NativeReclaimVerdict {
        retry,
        graced: false,
    };
    if !gc_overhead_limit_exceeded(shared) {
        // zr z5/cr: HotSpot's young, try, full, try -- this door cannot try
        // between its rungs, so on ZGC it asks whether the forced collection
        // just made room for the very request that was refused; then the
        // caller's retry is the "try" that succeeds, and the clear-soft
        // collection in front of it was the second collection per native
        // refusal (door 1 of the ProxyBoxing page).
        //
        // zr z8/nr: the skip is tracked per thread so a caller whose retry
        // still fails pays the rung before its `OutOfMemoryError`
        // ([`zgc_native_repay_skipped_soft_rung`]).
        if zgc_native_room_skips_soft_rung_for(shared, thread) {
            return plain(true);
        }
        last_ditch_reclaim(shared, thread);
        let _ = second_major_before_oome(shared, thread, "native-oome-second-major");
        return plain(true);
    }
    // gcg g3o residual: G1's latched exit, then the caller's one retry when
    // it allows one -- the interpreter's and the JIT helpers' answer
    // (`g1_latched_overhead_exit`).
    if shared.mem.heap.is_g1() {
        let allowed = g1_latched_overhead_exit(shared, thread, before_live);
        oome_ladder_step(
            shared,
            thread,
            &format!("g1-overhead-latched-exit attempt-allowed={allowed}"),
        );
        return plain(allowed);
    }
    let collected = last_ditch_clear_soft_refs(shared, thread);
    // `judge_major` of the interpreter's exit: the switch is on here, so it
    // is "Generational".
    let generational = shared.mem.heap.is_generational();
    let major_ran = generational
        && (!collected || gc_overhead_limit_exceeded(shared))
        && majors_to_decide_oome(shared, thread, "native-overhead-limit-major");
    // zr z2/cp: ZGC's forced cycle was whole-heap (`zgc_overhead_judged`).
    let zgc_judged = zgc_overhead_judged(shared);
    match overhead_limit_exit(
        generational || zgc_judged,
        major_ran || zgc_judged,
        collected,
        ((generational && major_ran) || zgc_judged) && gc_overhead_limit_exceeded(shared),
    ) {
        // gce e2/o: the bounded grace, as in `collect_and_retry_with_thread`
        // (the native caller's one retry is the graced attempt).
        OverheadLimitExit::Throw => {
            let graced = latched_overhead_grace(shared);
            NativeReclaimVerdict {
                retry: graced,
                graced,
            }
        }
        // ZGC (G1 returned above): its forced cycle was whole-heap.
        OverheadLimitExit::Attempt => plain(true),
        OverheadLimitExit::AttemptThenLastDitch => {
            last_ditch_reclaim(shared, thread);
            let _ = second_major_before_oome(shared, thread, "native-oome-second-major");
            plain(true)
        }
    }
}

/// gcg g3/o, `gcg-g1o-proposal-native-factory-door-tries-after-the-young-collection`:
/// `CRATONVM_GC_NATIVE_FACTORY_ATTEMPT_LADDER` -- default ON since gch h3/os
/// (`runtime_flag_default_on`, `=0` kills): vh2-ob `ob2_door_*`, the first row
/// whose native factory met an UNLATCHED refusal (`factory_enter=8` on every
/// row), stdout SAME 6/6 and `[GC] generational: major=` 34 off against 18 on in
/// 3/3 pairs -- two full collections saved per refusal, HotSpot's
/// `mem_allocate_work` shape (young, attempt, full, attempt).
pub(in crate::runtime::interpreter::gc_and_alloc) fn native_factory_attempt_ladder_enabled() -> bool {
    true
}

/// gcg g3/o (default ON since gch h3/os, `=0` kills): the FIRST rung of the
/// native factory door, so the factory can try its allocation after the young
/// collection, as the
/// interpreter's door (`collect_and_retry_with_thread`), the JIT helpers and
/// HotSpot's `mem_allocate_work` (young, try, full, try) do. Today's door
/// ([`native_reclaim_before_alloc_retry_graced`]) runs the young collection
/// AND, unlatched, a last-ditch full collection plus a second major before
/// its caller's one retry, so every refused native allocation paid two full
/// collections even when the young one freed enough.
///
/// `true`: the young collection ran; the caller attempts, and only if that
/// fails calls [`native_reclaim_after_young_stage`] and attempts once more.
/// `false`: the staged door does not apply (switch killed with `=0`,
/// `CRATONVM_GC_OVERHEAD_PROGRESS=0`, or the overhead streak already latched,
/// whose arm decides without an attempt in between); the caller uses
/// [`native_reclaim_before_alloc_retry_graced`] as before. Nothing collects
/// on the `false` path.
pub(crate) fn native_reclaim_young_stage(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    native_reclaim_young_stage_for(shared, thread, 0)
}

/// gci i13/oa item 4: [`native_reclaim_young_stage`] for a refused allocation
/// of `bytes` (header included; `0` = unknown): its young collection runs on
/// [`native_door_site`]'s site.
pub(crate) fn native_reclaim_young_stage_for(
    shared: &SharedVm,
    thread: &mut JvmThread,
    bytes: usize,
) -> bool {
    if !native_young_stage_applies(
        native_factory_attempt_ladder_enabled(),
        gc_overhead_progress_enabled(),
        gc_overhead_limit_exceeded(shared),
    ) {
        return false;
    }
    // `maybe_gc_forced_at` flushes the TLAB batch and retires the TLAB itself.
    oome_ladder_begin(shared, thread, &format!("door=native-staged bytes={bytes}"));
    maybe_gc_forced_at(shared, thread, native_door_site(shared, bytes));
    oome_ladder_step(shared, thread, "young-stage");
    true
}

/// gci i13/oa item 4
/// (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`): the
/// forced-collection site of the native doors for a refused allocation of
/// `bytes` -- [`G1_HUMONGOUS_ALLOC_SITE`] for a G1 humongous request
/// (`gc_alloc_array_site`'s test), `vm-exec` otherwise (every other backend,
/// every smaller request, and `0` = size unknown: the site before this item).
pub(in crate::runtime::interpreter::gc_and_alloc) fn native_door_site(shared: &SharedVm, bytes: usize) -> &'static str {
    native_door_site_for(gc_alloc_array_site(shared, bytes))
}

/// [`native_door_site`]'s mapping from the interpreter's array site, pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn native_door_site_for(interpreter_site: &'static str) -> &'static str {
    if interpreter_site == G1_HUMONGOUS_ALLOC_SITE {
        G1_HUMONGOUS_ALLOC_SITE
    } else {
        "vm-exec"
    }
}

/// gci i13/oa item 4: the footprint (header plus data, 8-byte grid) of an
/// array of `length` `element_type` elements, for the sized native doors
/// (`native::jni` `NewObjectArray` / `New<Type>Array`). Saturating, as
/// [`external_array_bytes`], whose figure it is.
pub(crate) fn native_array_footprint(element_type: ArrayElementType, length: usize) -> usize {
    external_array_bytes(element_type, length)
}

/// gci i19/oo (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`,
/// item 4's residual (a)): [`native_alloc_collecting`] for a request whose
/// largest single allocation is `footprint` bytes (header included; `0` =
/// unknown). A G1 humongous footprint collects on [`G1_HUMONGOUS_ALLOC_SITE`]
/// (cause `G1 Humongous Allocation`, HotSpot's `attempt_allocation_humongous`
/// door), every other request on the caller's own `site`, exactly as the
/// unsized door. For the native String doors (`create_string_uninterned_gc_safe`,
/// `create_string_from_units_gc_safe` in `vm/src/vm/vm_exec.rs`), whose
/// refusal payload (`NativeAllocOom`) carries a length and a kind but no
/// element size: the caller knows the String's `value` footprint
/// ([`native_string_value_footprint`]) and passes it. Label only: the ladder,
/// its attempts and its refusal are [`native_alloc_collecting`]'s.
pub(crate) fn native_alloc_collecting_sized<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    footprint: usize,
    attempt: impl FnMut(&SharedVm, &mut JvmThread) -> T,
) -> Result<T, RuntimeError> {
    let site = native_sized_door_site(shared, site, footprint);
    native_alloc_collecting(shared, thread, site, attempt)
}

/// [`native_alloc_collecting_sized`]'s site: the humongous site for a G1
/// humongous `footprint` ([`gc_alloc_array_site`]'s test), `site` otherwise.
pub(in crate::runtime::interpreter::gc_and_alloc) fn native_sized_door_site(shared: &SharedVm, site: &'static str, footprint: usize) -> &'static str {
    native_sized_site_for(gc_alloc_array_site(shared, footprint), site)
}

/// [`native_sized_door_site`]'s mapping from the interpreter's array site, pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn native_sized_site_for(interpreter_site: &'static str, site: &'static str) -> &'static str {
    if interpreter_site == G1_HUMONGOUS_ALLOC_SITE {
        G1_HUMONGOUS_ALLOC_SITE
    } else {
        site
    }
}

/// gci i19/oo: the footprint of a fresh `java/lang/String`'s `value` array for
/// `utf16_units` code units -- `byte[units]` under compact strings when every
/// unit fits Latin-1 (`latin1`), `byte[2 * units]` otherwise, the layout
/// `vm_object.rs` `populate_java_string_fields` builds. The String object
/// itself is a few words and never humongous, so this is the request that
/// decides the cause. Saturating, as [`external_array_bytes`].
pub(crate) fn native_string_value_footprint(
    compact_strings: bool,
    utf16_units: usize,
    latin1: bool,
) -> usize {
    let payload = if compact_strings && latin1 {
        utf16_units
    } else {
        utf16_units.saturating_mul(2)
    };
    external_array_bytes(ArrayElementType::Byte, payload)
}


/// gcg g3/o: the rest of the native factory door after a staged young
/// collection whose attempt failed: exactly the rungs
/// [`native_reclaim_before_alloc_retry_graced`] runs after its own young
/// collection ([`native_reclaim_ladder_tail`]), without a second young one
/// (a second forced cycle would count toward the overhead streak). The live
/// estimate G1's overhead cycle judges its progress against is taken here,
/// after the young collection, so that cycle is credited only with what it
/// frees itself.
pub(crate) fn native_reclaim_after_young_stage(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> NativeReclaimVerdict {
    let before_live = shared.mem.heap.live_bytes_estimate();
    native_reclaim_ladder_tail(shared, thread, before_live)
}

/// The pure gate of [`native_reclaim_young_stage`].
pub(in crate::runtime::interpreter::gc_and_alloc) fn native_young_stage_applies(switch_on: bool, progress_on: bool, latched: bool) -> bool {
    switch_on && progress_on && !latched
}

/// The body `NativeContextImpl::reclaim_before_alloc_retry` had before gcd
/// d10/o, kept verbatim for `CRATONVM_GC_OVERHEAD_PROGRESS=0`: the forced
/// collection; under a latched overhead streak the soft-reference rung's
/// answer; otherwise `last_ditch_reclaim` and a retry.
pub(in crate::runtime::interpreter::gc_and_alloc) fn native_reclaim_before_alloc_retry_historical(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    thread.tlab.retire();
    maybe_gc_forced_at(shared, thread, "vm-exec");
    if gc_overhead_limit_exceeded(shared) {
        return last_ditch_clear_soft_refs(shared, thread);
    }
    last_ditch_reclaim(shared, thread);
    true
}

/// zr z3/t2: the forced-collection sites whose collection an OCCUPANCY
/// verdict initiated rather than a failed allocation, on ZGC: the compiled
/// refill trigger (`tlab_alloc_shaped_inner`), the opt-in compiled
/// `newarray` headroom arm (lane ja), the native-boundary array request and
/// pressure doors (`vm_exec.rs`), and the compiled Integer box pressure door
/// (`jit/helpers.rs`). Each name is used by that door alone, so the
/// classification is exact; `jit-helpers` / `vm-exec` remain the genuine
/// failure retries. A collection at one of these is not an allocation stall
/// and carries the trigger rule's cause.
///
/// z4 host follow-up (z4a `-Xmx1g`: 16 `Allocation Stall` collections where
/// HotSpot has none): three more doors are occupancy verdicts ON ZGC only,
/// because ZGC answers the young probes with `!needs_gc()`
/// (`VmHeap::try_alloc_young_probe` / `young_bump_headroom` /
/// `young_has_free_block`): the compiled `new` and `anewarray` doors' forced
/// young cycle (`jit-object-young-probe`, `jit-anewarray-young-probe`) and
/// the refill GATE's wedge breaker (`tlab-refill-wedge-gate`). This list is
/// read on ZGC only, so the Generational meaning of those doors (young
/// genuinely full) is untouched.
pub(crate) const ZGC_OCCUPANCY_FORCED_SITES: &[&str] = &[
    "tlab-alloc-shaped",
    "jit-newarray-zgc-headroom",
    "vm-exec-native-array",
    "vm-exec-pressure",
    "jit-box-pressure",
    "jit-object-young-probe",
    "jit-anewarray-young-probe",
    "tlab-refill-wedge-gate",
];

/// Whether `site` is one of [`ZGC_OCCUPANCY_FORCED_SITES`].
pub(crate) fn zgc_forced_site_is_occupancy(site: &str) -> bool {
    ZGC_OCCUPANCY_FORCED_SITES.contains(&site)
}

pub(in crate::runtime::interpreter) fn maybe_gc_forced(shared: &SharedVm, thread: &mut JvmThread) {
    maybe_gc_forced_at(shared, thread, "unlabelled")
}

/// [`maybe_gc_forced`] with the caller's identity, for the census.
pub(in crate::runtime::interpreter) fn maybe_gc_forced_at(shared: &SharedVm, thread: &mut JvmThread, site: &'static str) {
    let _ = maybe_gc_forced_collected(shared, thread, site);
}

/// gci i8/gs: is `site` one of the [`maybe_gc_forced_at`] callers that come
/// on an OCCUPANCY verdict (`needs_gc_for_jit_allocation` said a collection is
/// due) rather than on an allocation that failed? The compiled TLAB refill
/// trigger (`tlab_alloc_shaped_inner`, every collector) and the two G1 JIT
/// array headroom guards (`jit_newarray_body` / the `anewarray` helper in
/// `vm/src/jit/helpers.rs`, `heap.is_g1()` only). Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn occupancy_door_site(site: &str) -> bool {
    // gci i12/ge: and the two headroom guards' humongous spelling
    // (`G1_HUMONGOUS_HEADROOM_SITE`, still an occupancy verdict).
    // Merge of origin/dev (2026-10-15): and zr z3/ja's opt-in ZGC
    // `jit_newarray` headroom arm, which decides on the same occupancy
    // verdict as the refill trigger (`needs_gc()`).
    matches!(
        site,
        "tlab-alloc-shaped"
            | "jit-newarray-headroom"
            | "jit-anewarray-headroom"
            | "jit-newarray-zgc-headroom"
    ) || site == G1_HUMONGOUS_HEADROOM_SITE
}

/// gci i8/gs: the door [`maybe_gc_forced_collected`] runs its pause on --
/// `AllocationThreshold` for an occupancy verdict ([`occupancy_door_site`]),
/// `AllocationFailure` for everything else. Both print HotSpot's
/// "Allocation Failure" cause (`GcDoor::cause`); only the `[GC] pause:`
/// `door=` label and the debug mtroots reason differ. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn forced_door_for(occupancy_door: bool) -> GcDoor {
    if occupancy_door {
        GcDoor::AllocationThreshold
    } else {
        GcDoor::AllocationFailure
    }
}

/// gci i8/gs: does [`run_collection_pause`] sample [`SealedProductivity`]
/// inside the pause for `door`? Both allocation doors: the forced door judges
/// every ALLOCATION pause it runs (`note_gc_productivity_for_pause`; since
/// gci i10/dl it also gates that verdict on this, so a pause serving a
/// full-collection requester is not judged), occupancy verdicts
/// included, and those run on `AllocationThreshold` since gci i8/gs
/// ([`forced_door_for`]). `maybe_gc`'s own `AllocationThreshold` pause ignores
/// the sample (two counter reads and the live estimate, once per pause).
///
/// gci i11/go: and G1's allocation-failure recovery compaction
/// (`GcDoor::G1CompactionPause`), which is an allocation failure under
/// HotSpot's `G1 Compaction Pause` cause: its verdict feeds the OOME ladder
/// exactly as it did on `AllocationFailure`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn door_samples_productivity(door: GcDoor) -> bool {
    matches!(
        door,
        GcDoor::AllocationFailure
            | GcDoor::AllocationThreshold
            | GcDoor::G1CompactionPause
            | GcDoor::G1HumongousAllocation
    )
}

/// gci i10/dl: the door [`maybe_gc_forced_collected_on`] runs its pause on --
/// the requester's full-collection door when the pause serves one (G1's
/// compaction pause inside `force_full_collection`), else the allocation
/// door [`forced_door_for`] picks. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn forced_pause_door(occupancy_door: bool, requester: Option<GcDoor>) -> GcDoor {
    requester.unwrap_or_else(|| forced_door_for(occupancy_door))
}

/// gci i11/go: the [`maybe_gc_forced_at`] site of G1's allocation-failure
/// recovery compaction ([`g1_last_ditch_recovery_pause`]).
pub(in crate::runtime::interpreter::gc_and_alloc) const G1_LAST_DITCH_COMPACTION_SITE: &str = "g1-last-ditch-compaction";

/// gci i11/go (`gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (a)):
/// [`forced_pause_door`], except that G1's allocation-failure recovery
/// compaction (site [`G1_LAST_DITCH_COMPACTION_SITE`], no requester) runs on
/// `GcDoor::G1CompactionPause` -- HotSpot G1's `satisfy_failed_allocation_helper`
/// sets `GCCause::_g1_compaction_pause` around the full collection it runs
/// before throwing `OutOfMemoryError`, so JMX `getGcCause()`, JFR and
/// `-Xlog:gc` say `G1 Compaction Pause` there, not an evacuation pause. The
/// door is an allocation door in every other respect
/// ([`door_samples_productivity`]; the ladder census keys off `requester`).
/// Only G1 reaches the site. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn forced_site_door(site: &str, occupancy_door: bool, requester: Option<GcDoor>) -> GcDoor {
    if requester.is_none() && site == G1_LAST_DITCH_COMPACTION_SITE {
        return GcDoor::G1CompactionPause;
    }
    // gci i11/go (i10rh finding 4): a humongous request's collection is
    // `G1 Humongous Allocation` (the site is chosen only on G1, for a request
    // above half a region: [`gc_alloc_array_site`]).
    // gci i12/ge (review findings page item 6): and the JIT's G1 headroom
    // guards when the request they guard is humongous (an occupancy verdict
    // in every other respect: `occupancy_door_site`).
    if requester.is_none() && (site == G1_HUMONGOUS_ALLOC_SITE || site == G1_HUMONGOUS_HEADROOM_SITE) {
        return GcDoor::G1HumongousAllocation;
    }
    forced_pause_door(occupancy_door, requester)
}

/// gci i20/mr (`gci-i20mr-merge-review-findings-FIXED-20261016` finding 1): the
/// forced door's pause door on the heap it runs on -- `door`
/// ([`forced_site_door`]) on Generational and G1, and dev's
/// `GcDoor::AllocationFailure` for EVERY forced pause on ZGC (`zgc`), as
/// before the 2026-10-15 merge. gci i8/gs's occupancy door
/// (`AllocationThreshold`) is a Generational / G1 label; on ZGC the door also
/// picks the HotSpot cause (`GcEventStart::cause`: the threshold rule's on
/// `AllocationThreshold`, the forced-door classification's on
/// `AllocationFailure`, which `with_zgc_forced_door_occupancy` already makes
/// the occupancy one for `ZGC_OCCUPANCY_FORCED_SITES`), so the merge had
/// changed ZGC's cause and pause label at `tlab-alloc-shaped` and
/// `jit-newarray-zgc-headroom`. ZGC is out of this round's scope: its
/// behaviour is dev's. No requester reaches the door on ZGC. Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn forced_door_on_heap(zgc: bool, door: GcDoor) -> GcDoor {
    if zgc {
        GcDoor::AllocationFailure
    } else {
        door
    }
}

/// gci i12/ge (`gci-i12ge-...-review-findings-20261013` item 6): the site of
/// the JIT array helpers' G1 headroom guards (`jit-newarray-headroom`,
/// `jit-anewarray-headroom`) for a HUMONGOUS request: HotSpot's humongous
/// allocation asks for its collections with `G1 Humongous Allocation`. An
/// occupancy door ([`occupancy_door_site`]: ladder census, forced mark and
/// productivity verdict as the headroom sites).
pub(in crate::runtime::interpreter::gc_and_alloc) const G1_HUMONGOUS_HEADROOM_SITE: &str = "g1-humongous-headroom";

/// gci i12/ge: the headroom guard's site for a request of `bytes` (header
/// included): [`G1_HUMONGOUS_HEADROOM_SITE`] for a G1 humongous request,
/// `ordinary` (the guard's own site) otherwise.
pub(crate) fn jit_array_headroom_site(
    shared: &SharedVm,
    bytes: usize,
    ordinary: &'static str,
) -> &'static str {
    jit_headroom_site_for(gc_alloc_array_site(shared, bytes), ordinary)
}

/// [`jit_array_headroom_site`]'s mapping, pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn jit_headroom_site_for(interpreter_site: &'static str, ordinary: &'static str) -> &'static str {
    if interpreter_site == G1_HUMONGOUS_ALLOC_SITE {
        G1_HUMONGOUS_HEADROOM_SITE
    } else {
        ordinary
    }
}

/// gci i11/go: the forced-collection site of a G1 humongous array request
/// (interpreter array ladder, [`gc_alloc_array_site`]).
pub(in crate::runtime::interpreter::gc_and_alloc) const G1_HUMONGOUS_ALLOC_SITE: &str = "g1-humongous-alloc";

/// gci i11/go (i10rh finding 4): the [`collect_and_retry`] site of
/// `gc_alloc_array_unsampled` -- [`G1_HUMONGOUS_ALLOC_SITE`] when the heap is
/// G1 and the request (`bytes`, the array's data plus its header) is
/// humongous (`G1Collector::is_humongous_request`), so its collection reports
/// HotSpot's `G1 Humongous Allocation`; `gc-alloc-array` otherwise (every
/// other backend, every non-humongous array).
pub(in crate::runtime::interpreter::gc_and_alloc) fn gc_alloc_array_site(shared: &SharedVm, bytes: usize) -> &'static str {
    match &shared.mem.heap {
        _ if shared.mem.heap.g1_is_humongous_request(bytes) => {
            G1_HUMONGOUS_ALLOC_SITE
        }
        _ => "gc-alloc-array",
    }
}

/// gci i12/ge (i11/go's narrowed JIT door, `gci-i10rh-...-review-findings-20261013`
/// finding 4): the forced-collection site of the JIT array helpers' failed
/// allocation (`jit_newarray_body`, the `anewarray` helper's failure arm in
/// `vm/src/jit/helpers.rs`) for a request of `bytes` (header included) --
/// [`G1_HUMONGOUS_ALLOC_SITE`] for a G1 humongous request, so its collection
/// reports `G1 Humongous Allocation` as the interpreter's array ladder's does
/// ([`gc_alloc_array_site`]); their historical `jit-helpers` otherwise (every
/// other backend and request, so the forced-site census is unchanged there).
pub(crate) fn jit_array_gc_site(shared: &SharedVm, bytes: usize) -> &'static str {
    jit_array_site_for(gc_alloc_array_site(shared, bytes))
}

/// [`jit_array_gc_site`]'s mapping from the interpreter's array site, pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn jit_array_site_for(interpreter_site: &'static str) -> &'static str {
    if interpreter_site == G1_HUMONGOUS_ALLOC_SITE {
        G1_HUMONGOUS_ALLOC_SITE
    } else {
        "jit-helpers"
    }
}

/// gci i11/go (`gci-i10dl-...-door-residuals-20261013` (c)): does the pause
/// [`maybe_gc_forced_collected_on`] just ran skip its G1 mark-cycle drive?
/// Only a pause serving a full-collection requester (G1's
/// `g1_full_compaction_pause`), and only under the opt-in switch (`switch_on`
/// is asked only then, so an allocation pause never reads it). Pure apart
/// from `switch_on`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn full_compaction_skips_mark_drive(
    requester: Option<GcDoor>,
    switch_on: impl FnOnce() -> bool,
) -> bool {
    requester.is_some() && switch_on()
}

/// `CRATONVM_G1_FULL_COMPACTION_SKIPS_MARK_DRIVE` (default OFF, gci i11/go):
/// see [`full_compaction_skips_mark_drive`]. Read per G1 full collection, not
/// cached: one environment read on a path that has just run a marking cycle
/// and a compaction pause.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_full_compaction_skips_mark_drive() -> bool {
    false
}

/// [`maybe_gc_forced_at`], reporting whether THIS thread ran the collection
/// (`false`: it lost the STW race and took part in another thread's pause).
///
/// For a caller whose request is thread-local and therefore only honoured by a
/// collection it initiates itself — see `last_ditch_clear_soft_refs`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn maybe_gc_forced_collected(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
) -> bool {
    maybe_gc_forced_collected_on(shared, thread, site, None)
}

/// [`maybe_gc_forced_collected`] for a pause that serves a full-collection
/// request (`requester`: `Some(door)`), or an allocation (`None`, every
/// allocation caller).
///
/// gci i10/dl (`gci-i9dc-g1-full-collection-compaction-pause-reports-allocation-failure-FIXED-20261017`):
/// G1's full collection ends with a compaction pause
/// ([`g1_full_compaction_pause`]) that ran on `GcDoor::AllocationFailure`
/// whatever asked for the full collection, so one `System.gc()` reported a
/// `System.gc()` pause AND an `Allocation Failure` one (JMX notification
/// cause, JFR `jdk.GarbageCollection.cause`, `-Xlog:gc`), and the compaction
/// was judged for the GC-overhead streak as if an allocation had failed. With
/// a requester the pause runs on the requester's door (HotSpot G1's `Pause
/// Full (<cause>)` carries the requester's cause), is not judged for the
/// overhead streak (no full-collection door is: `force_full_collection`'s
/// own pause is not either, and HotSpot keeps user-requested collections out
/// of its overhead policy), and is not counted as a forced ladder rung.
///
/// Merge of origin/dev (2026-10-15): the body is
/// [`maybe_gc_forced_collected_stamped_on`] (zr z3/ja's decision stamp; an
/// unstamped request is never dropped, so `unwrap_or` never answers).
pub(in crate::runtime::interpreter::gc_and_alloc) fn maybe_gc_forced_collected_on(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    requester: Option<GcDoor>,
) -> bool {
    maybe_gc_forced_collected_stamped_on(shared, thread, site, None, requester).unwrap_or(false)
}



/// zr s1/ca (`zgc-zr11rv9-z8-z9-review-residuals-FIXED-20261019.md` item 4): the
/// opt-in `jit_newarray` headroom arm asks for at most one collection per
/// `heap_capacity / ZGC_JIT_NEWARRAY_HEADROOM_RATE_DIVISOR` bytes allocated
/// since the last collection (never less than [`ZGC_REFUSAL_FUTILE_WINDOW`]).
pub(crate) const ZGC_JIT_NEWARRAY_HEADROOM_RATE_DIVISOR: usize = 32;



/// The pure decision of [`zgc_jit_newarray_headroom_rate_limited`]: is a
/// window of `window` bytes allocated since the last collection too small to
/// be worth another occupancy collection on a heap of `capacity` bytes?
pub(crate) fn newarray_headroom_rate_skips(window: usize, capacity: usize) -> bool {
    window < (capacity / ZGC_JIT_NEWARRAY_HEADROOM_RATE_DIVISOR).max(ZGC_REFUSAL_FUTILE_WINDOW)
}

/// zr s1/ca: the ZGC rate limit of the opt-in `jit_newarray` headroom arm
/// (`CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM=1`). Its siblings' limiter,
/// `futile_young_backoff_skips`, is Generational-only, so on ZGC a live set
/// held above the occupancy trigger after a collection made the arm ask for a
/// collection per spilled `newarray`: each finds the verdict unchanged, and
/// the stale-request stamp drops only requests that RACED a collection, not
/// one decided after it. With this rule the arm stands aside (the spill below
/// it allocates, and an allocation FAILURE still collects through the ladder)
/// until a 1/32nd of the heap has been laid out since the last collection.
/// `true` = skip the arm. `false` off ZGC and under `=0`. Counted in
/// `newarray_headroom_rate_skips=` on the `zgc_stale_gc_requests` line.
pub(crate) fn zgc_jit_newarray_headroom_rate_limited(shared: &SharedVm) -> bool {
    let Some(window) = shared.mem.heap.zgc_bytes_allocated_since_cycle() else {
        return false;
    };
    if !newarray_headroom_rate_skips(window, shared.mem.heap.heap_capacity())
    {
        return false;
    }
    shared
        .mem
        .tlab_wedge
        .zgc_newarray_headroom_rate_skips
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    true
}

/// zr z3/ja: the collection stamp of a decision to collect, HotSpot's
/// `gc_count_before` (`VM_GC_Operation::skip_operation`): the VM's
/// `gc_cycle_count` read where an allocating thread DECIDED that a collection
/// is needed. `None` under `CRATONVM_ZGC_DROP_STALE_GC_REQUESTS=0`, so a
/// stamped door is then the old door byte for byte. gci i48/zl2: stamped on
/// every collector (it was ZGC only): the epoch below is bumped by the shared
/// pause door, `run_collection_pause`, for Generational and G1 collections as
/// for ZGC's.
///
/// Take it right AFTER the predicate that decided (`needs_gc_for_jit_allocation`,
/// `young_bump_headroom`): a collection that completes between the predicate and
/// the stamp is then not detected (the status quo), never the other way round --
/// a stamp taken before the predicate could drop a request the predicate made
/// on the post-collection heap.
///
/// zr z4/rv4: the epoch is `TlabWedgeState::collections_sealed`, bumped inside
/// the pause before `complete_gc`, not `gc_cycle_count`: `gc_event_finish`
/// advances that one AFTER the world is released, so a released mutator
/// could decide on the post-collection heap, stamp the pre-bump count, and
/// lose its request to the finishing thread's bump -- exactly the ordering
/// the paragraph above rules out.
pub(crate) fn zgc_gc_request_stamp(shared: &SharedVm) -> Option<u64> {
    shared
        .mem
        .tlab_wedge
        .zgc_stamped_gc_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(
        shared
            .mem
            .tlab_wedge
            .collections_sealed
            .load(std::sync::atomic::Ordering::Acquire),
    )
}

/// zr z3/ja: whether a stamped request is stale -- a collection sealed
/// (`TlabWedgeState::collections_sealed` moved, inside the pause before
/// `complete_gc`; zr z4/rv4 -- not `gc_event_finish`'s ordinal, which moves
/// after the release) after the decision -- and if so, count it as dropped.
/// Pure on its arguments but for the counter.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_gc_request_is_stale(shared: &SharedVm, stamp: Option<u64>) -> bool {
    let Some(seen) = stamp else {
        return false;
    };
    let now = shared
        .mem
        .tlab_wedge
        .collections_sealed
        .load(std::sync::atomic::Ordering::Acquire);
    let stale = stamp_is_stale(seen, now);
    if stale {
        shared
            .mem
            .tlab_wedge
            .zgc_stale_gc_requests_dropped
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    stale
}

/// The rule, apart: any collection since the stamp satisfies the request.
pub(in crate::runtime::interpreter::gc_and_alloc) fn stamp_is_stale(seen: u64, now: u64) -> bool {
    now != seen
}

/// zr z3/ja: the `[GC] zgc_stale_gc_requests:` shutdown line: `stamped=`
/// decisions that carried a stamp (the positive control: non-zero on any run
/// whose compiled code reached the refill trigger or a helper's headroom arm)
/// and `dropped=` requests a collection in between had already served.
/// gci i48/zl2: printed on every collector (the name is kept); always `Some`.
pub fn zgc_stale_gc_requests_line(shared: &SharedVm) -> Option<String> {
    let w = &shared.mem.tlab_wedge;
    let mut line = format!(
        "[GC] zgc_stale_gc_requests: stamped={} dropped={}",
        w.zgc_stamped_gc_requests
            .load(std::sync::atomic::Ordering::Relaxed),
        w.zgc_stale_gc_requests_dropped
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    // zs s1/ca's counter is ZGC's (the compiled newarray headroom arm); the
    // zt merge keeps dev's line byte for byte on Generational and G1.
    if shared.mem.heap.is_zgc() {
        // zt t20/fl1: zt t4/ca2's `failure_door_dropped=` column went with
        // its retired switch (`CRATONVM_ZGC_JIT_FAILURE_DOOR_STAMPED`).
        line.push_str(&format!(
            " newarray_headroom_rate_skips={}",
            w.zgc_newarray_headroom_rate_skips
                .load(std::sync::atomic::Ordering::Relaxed)
        ));
    }
    Some(line)
}

/// zr z5/cr: the forced-collection sites that follow a FAILED allocation and
/// are themselves followed, in their ladder, by an attempt and then the
/// clear-soft rung (`last_ditch_reclaim` / `last_ditch_clear_soft_refs`): the
/// interpreter's `collect_and_retry` callers, the JIT helpers' failure arms,
/// and the native factory door. Only these may have their forced collection
/// skipped by the futile rule ([`zgc_refusal_skip_applies`]): for each, a skip
/// still leaves the clear-soft collection between the refusal and any
/// `OutOfMemoryError`. The clear-soft rung's own site is a refusal collection
/// (counted, stamped) but never skipped.
pub(in crate::runtime::interpreter::gc_and_alloc) const ZGC_REFUSAL_SKIP_SITES: &[&str] = &[
    "alloc-object-shared",
    "gc-alloc-array",
    "create-string",
    "create-string-units",
    "intern-string-literal",
    "jit-helpers",
    "vm-exec",
];

/// zr z5/cr: the clear-soft rung's site (`last_ditch_clear_soft_refs`).
pub(in crate::runtime::interpreter::gc_and_alloc) const ZGC_REFUSAL_SOFT_RUNG_SITE: &str = "last-ditch-soft-refs";

/// zr z5/cr: below this many bytes allocated since the last collection, a
/// second forced collection for a refusal can free at most this much garbage.
/// `ZgcRealHeap::bytes_allocated_since_cycle` charges a carved TLAB chunk in
/// full (>= 8 KiB), so a window under 4 KiB means no chunk was carved and at
/// most a few small objects were laid out since: nothing a collection could
/// turn into allocation-worthy space beyond what the previous one already
/// left. HotSpot's analogue is a stall that fails after a GC which freed no
/// page.
pub(crate) const ZGC_REFUSAL_FUTILE_WINDOW: usize = 4 * 1024;



/// zr z5/cr: `Some(bytes allocated since the previous collection)` when this
/// forced collection is a ZGC allocation-REFUSAL collection, else `None`
/// (Generational, G1, an occupancy site, or a stamped decision site -- the
/// JIT helpers' "no headroom" arms decide on the occupancy verdict and carry
/// a stamp). No-op off ZGC.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_refusal_window(shared: &SharedVm, site: &str, stamp: Option<u64>) -> Option<usize> {
    if stamp.is_some() || zgc_forced_site_is_occupancy(site) || site == "tlab-refill-wedge" {
        return None;
    }
    shared.mem.heap.zgc_bytes_allocated_since_cycle()
}

/// zr z5/cr: the futile-refusal rule, HotSpot's stall model ("an allocation
/// stall waits for a GC and fails after one that cannot satisfy it"). A
/// refusal at a door whose ladder continues with an attempt and the clear-soft
/// rung ([`ZGC_REFUSAL_SKIP_SITES`]), on a heap that has allocated under
/// [`ZGC_REFUSAL_FUTILE_WINDOW`] since its last collection (of any kind), gets
/// no forced collection of its own: another whole-heap cycle could free at
/// most that window. The ladder's clear-soft rung then OWES one collection
/// (`soft_rung_owed`, paid by [`last_ditch_clear_soft_refs`] even when no soft
/// reference is active), and that collection also carries the hard-failure
/// latch this refusal raised (the after-failure relocation tier keys off the
/// latch, not the door). So a refusal still meets a collection before its
/// second attempt and before any `OutOfMemoryError`; what goes is the forced
/// cycle in front of the soft one on an unchanged heap. Typical cases: the
/// allocations after an `OutOfMemoryError` (the `catch` block, the next
/// native), and a refusal right behind another collection.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_refusal_skip_applies(site: &str, window: usize) -> bool {
    refusal_skip_decision(ZGC_REFUSAL_SKIP_SITES.contains(&site), window)
}

/// The pure decision of [`zgc_refusal_skip_applies`] (switch aside).
pub(in crate::runtime::interpreter::gc_and_alloc) fn refusal_skip_decision(site_skippable: bool, window: usize) -> bool {
    site_skippable && window < ZGC_REFUSAL_FUTILE_WINDOW
}

/// zr z5/cr: take the owed collection a skipped refusal left for the
/// clear-soft rung. `false` off ZGC (never armed).
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_refusal_take_owed(shared: &SharedVm) -> bool {
    shared
        .mem
        .tlab_wedge
        .zgc_refusal
        .soft_rung_owed
        .swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// zr z5/cr: pay the owed collection at the clear-soft rung's site (no soft
/// reference to clear). Returns whether this thread ran it.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_refusal_pay_owed(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    shared
        .mem
        .tlab_wedge
        .zgc_refusal
        .owed_paid
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    maybe_gc_forced_collected(shared, thread, ZGC_REFUSAL_SOFT_RUNG_SITE)
}

/// zr z7/rv6: take and pay a skipped refusal's owed collection NOW, for a
/// refusal door whose ladder has no clear-soft rung after its forced
/// collection (the JIT box door, `jit_box_cold_alloc`: its fall-through ends
/// in a native allocation that raises without collecting). Returns whether
/// this thread ran the collection; `false` when nothing was owed (the futile
/// rule did not fire, or off ZGC).
pub(crate) fn zgc_refusal_pay_owed_now(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    zgc_refusal_take_owed(shared) && zgc_refusal_pay_owed(shared, thread)
}

/// zr z5/cr: the native-boundary relief (`safe_native_call`, site
/// `vm-exec-pressure`) on ZGC when its ONLY reason is the hard-failure latch:
/// skip it when under [`ZGC_REFUSAL_FUTILE_WINDOW`] was allocated since the
/// last collection. The latch outlives a collection only when a refusal's
/// ladder ended without one after its last failed attempt (an
/// `OutOfMemoryError` was raised) or a skipped refusal was served without one;
/// either way the heap was collected with nothing allocated since, and the
/// relief was a second collection of an unchanged heap (door 2 of
/// `zgc-zr2cp-proxyboxing-collects-four-times-hotspot-near-oom-20261013.md`).
/// Same switch as the refusal rule. `false` off ZGC.
pub(crate) fn zgc_hard_failure_relief_is_futile(shared: &SharedVm) -> bool {
    let Some(window) = shared.mem.heap.zgc_bytes_allocated_since_cycle() else {
        return false;
    };
    let futile = window < ZGC_REFUSAL_FUTILE_WINDOW;
    if futile {
        shared
            .mem
            .tlab_wedge
            .zgc_refusal
            .relief_skipped
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    futile
}



/// zr z5/cr: the native factory door's unlatched tail on ZGC
/// (`native_reclaim_ladder_tail`): skip the clear-soft rung when the forced
/// collection this door just ran was the ONLY collection since the arena
/// refused the request, and the arena now has a single span for it
/// (`ZgcRealHeap::room_for_last_refusal`). The caller's one retry then
/// allocates, so no `OutOfMemoryError` follows that could owe the soft rung;
/// with no such record (the forced collection was skipped or shared, or the
/// refusal was not the arena's) the rung runs as before. Counted
/// (`soft_skipped_room`). `false` off ZGC.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_native_room_skips_soft_rung(shared: &SharedVm) -> bool {
    if shared.mem.heap.zgc_room_for_last_refusal() != Some(true)
    {
        return false;
    }
    shared
        .mem
        .tlab_wedge
        .zgc_refusal
        .soft_skipped_room
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    true
}



/// zr z8/nr (`zgc-zr7rv6-native-room-skip-oome-without-a-clear-soft-collection-20261013.md`):
/// [`zgc_native_room_skips_soft_rung`] for `thread`, remembering the skip.
///
/// The room answer is a snapshot: another mutator can take the span before
/// the caller's retry, and a native that allocates two objects can spend the
/// span on the first. Then the retry fails and, without this, the caller
/// raised `OutOfMemoryError` after ONE plain collection -- no clear-soft
/// collection since the refusal, which `SoftReference` forbids and HotSpot
/// never does. So the skip is recorded in a one-slot per-VM pending word
/// (`soft_skip_pending`, the thread's `oome_debt_key`), taken only when the
/// slot is free (another thread's pending skip means the rung runs here, as
/// before cr), and released by the retry's outcome: a served native
/// allocation ([`note_native_alloc_succeeded`],
/// [`zgc_native_room_skip_served`]), the repay
/// ([`zgc_native_repay_skipped_soft_rung`]) or an `OutOfMemoryError` raised
/// without it (`soft_skip_unpaid`, [`note_heap_oome_raised`]). Off ZGC the
/// room record is `None` and nothing is touched. `=0` on
/// `CRATONVM_ZGC_NATIVE_ROOM_SKIP_REPAY` restores cr's untracked skip.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_native_room_skips_soft_rung_for(shared: &SharedVm, thread: &JvmThread) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    if shared.mem.heap.zgc_room_for_last_refusal() != Some(true) {
        return false;
    }
    let pending = &shared.mem.tlab_wedge.zgc_refusal.soft_skip_pending;
    let key = oome_debt_key(thread);
    // This thread's own stale record (its earlier skip's outcome was never
    // reported, e.g. a caller that degrades instead of raising).
    let _ = pending.compare_exchange(key, 0, Relaxed, Relaxed);
    if pending.compare_exchange(0, key, Relaxed, Relaxed).is_err() {
        return false;
    }
    if zgc_native_room_skips_soft_rung(shared) {
        return true;
    }
    let _ = pending.compare_exchange(key, 0, Relaxed, Relaxed);
    false
}

/// zr z8/nr: the caller's retry after a ladder verdict failed. If that
/// ladder skipped its clear-soft rung on this thread
/// ([`zgc_native_room_skips_soft_rung_for`]), run the rung now --
/// `last_ditch_reclaim` and the second major, exactly what the tail skipped --
/// and answer `true`: the caller retries once more before raising. `false`
/// (nothing ran) when no skip of this thread is pending, which is every
/// non-ZGC heap and every ladder that ran its rung. Same precondition as the
/// ladder itself: every reference the caller still uses must be rooted.
pub(crate) fn zgc_native_repay_skipped_soft_rung(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let census = &shared.mem.tlab_wedge.zgc_refusal;
    let key = oome_debt_key(thread);
    if key == 0
        || census.soft_skip_pending.load(Relaxed) != key
        || census
            .soft_skip_pending
            .compare_exchange(key, 0, Relaxed, Relaxed)
            .is_err()
    {
        return false;
    }
    census.soft_skip_repaid.fetch_add(1, Relaxed);
    // The failed retry is a refusal of its own (HotSpot: try, full, try): its
    // forced collection first -- under the futile rule (nothing allocated
    // since the door's collection) it is skipped and OWED to the clear-soft
    // rung below, which then pays it even with no soft reference active, so
    // exactly one collection runs on an unchanged heap and none is lost when
    // another thread allocated the room away.
    maybe_gc_forced_at(shared, thread, "vm-exec");
    last_ditch_reclaim(shared, thread);
    let _ = second_major_before_oome(shared, thread, "native-oome-second-major");
    true
}

/// zr z8/nr: a caller's retry after the ladder allocated: release this
/// thread's pending room skip, if any (one relaxed load otherwise).
#[inline]
pub(crate) fn zgc_native_room_skip_served(shared: &SharedVm, thread: &JvmThread) {
    use std::sync::atomic::Ordering::Relaxed;
    let pending = &shared.mem.tlab_wedge.zgc_refusal.soft_skip_pending;
    let word = pending.load(Relaxed);
    if word != 0 && word == oome_debt_key(thread) {
        let _ = pending.compare_exchange(word, 0, Relaxed, Relaxed);
    }
}

/// zr z5/cr: book one refusal collection (run or joined) with its window, and
/// stamp the seal it left (the futile rule's "no collection since").
///
/// `sealed_at_refusal` is `collections_sealed` read when the refusal entered
/// the door: a door that lost the pause race to a pause that was NOT a
/// collection leaves it unchanged, and then nothing is booked or stamped, so
/// the futile rule can never take "no collection since" from a collection
/// that did not happen.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_refusal_note_collection(shared: &SharedVm, window: usize, sealed_at_refusal: u64) {
    use std::sync::atomic::Ordering;
    let census = &shared.mem.tlab_wedge.zgc_refusal;
    let sealed_now = shared
        .mem
        .tlab_wedge
        .collections_sealed
        .load(Ordering::Acquire);
    if sealed_now == sealed_at_refusal {
        return;
    }
    // A refusal collection pays whatever a skipped refusal owed.
    census.soft_rung_owed.store(false, Ordering::Relaxed);
    census.collections.fetch_add(1, Ordering::Relaxed);
    census
        .window_bytes
        .fetch_add(window as u64, Ordering::Relaxed);
    let bucket = match window {
        w if w < 4 * 1024 => &census.window_lt4k,
        w if w < 64 * 1024 => &census.window_lt64k,
        w if w < 1024 * 1024 => &census.window_lt1m,
        _ => &census.window_ge1m,
    };
    bucket.fetch_add(1, Ordering::Relaxed);
    // The previous collection was a refusal collection iff exactly one seal
    // happened since that one stamped (this collection's own).
    let prev = census.last_seal.load(Ordering::Relaxed);
    if prev != 0 && sealed_now == prev.wrapping_add(1) {
        census.after_refusal.fetch_add(1, Ordering::Relaxed);
        let run = census.streak_now.fetch_add(1, Ordering::Relaxed) + 1;
        census.streak_max.fetch_max(run, Ordering::Relaxed);
    } else {
        census.streak_now.store(1, Ordering::Relaxed);
        census.streak_max.fetch_max(1, Ordering::Relaxed);
    }
    census.last_seal.store(sealed_now, Ordering::Relaxed);
}

