// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-collection accounting shared by every GC door in `gc_and_alloc`.
//!
//! Split out of `gc_and_alloc.rs` (gen r4/plumbing, 2026-09-23) so the
//! observability half of a collection -- the VM's collection ordinal, the JFR
//! GC events and the `-Xlog:gc` line -- lives in one place that the door
//! functions only call into.

use super::*;

// ---------------------------------------------------------------------------
// Per-collection accounting: the cycle ordinal, JFR GC events, `-Xlog:gc`
// ---------------------------------------------------------------------------
//
// gen r4/plumbing (2026-09-23). Until this section existed the three GC doors
// disagreed about what a collection leaves behind:
//
// * `gc_cycle_count` was bumped by `maybe_gc` and `maybe_gc_forced` but not by
//   `force_gc_from_native`, so `System.gc()` collections were uncounted;
// * exactly ONE of the six initiator arms (`maybe_gc`, single-threaded) emitted
//   JFR events, with `gcId` the literal `1` on every collection, the name
//   `"YoungGC"`, the cause `"Allocation Failure"` whatever the cause, G1's
//   tenuring threshold `15`, a heap summary whose `committed` was `used` and
//   whose `max` was `2 * used`, and no `jdk.OldGarbageCollection` ever
//   (`docs/internal/gc/gengc-plumbing-jfr-gc-events-are-constants-FIXED-20260923.md`);
// * `-Xlog:gc` initialised the unified logger and then nothing ever logged to
//   it: `unified_logging::gc_info` had no caller, so `-Xlog:gc` — the flag a
//   HotSpot user reaches for first — printed nothing, on every backend.
//
// Every initiator arm now brackets its collection with `gc_event_start` /
// `gc_event_finish`, so the three cannot drift again. The default path pays
// one relaxed-ish load (`cratonvm_jfr::is_enabled`) and one `OnceLock` read
// (the unified logger) per COLLECTION, and nothing per allocation.

/// Which VM door initiated a collection — the `cause` a JFR
/// `jdk.GarbageCollection` event and an `-Xlog:gc` line carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GcDoor {
    /// `maybe_gc`: the young-occupancy trigger (`needs_gc`) or a latched
    /// `gc_requested`.
    AllocationThreshold,
    /// `maybe_gc_forced`: an allocation that failed outright.
    AllocationFailure,
    /// `force_gc_from_native`: `System.gc()`, `Runtime.gc()`, JMX `gc()`.
    SystemGc,
    /// `force_gc_for_class_metadata`: the `-XX:MaxMetaspaceSize` refusal's
    /// full collection. Not an explicit GC (gc-common w8-g; HotSpot's
    /// `GCCause::_metadata_GC_threshold`).
    MetadataThreshold,
}

impl GcDoor {
    /// HotSpot's `GCCause` spelling. Both allocation doors are HotSpot's
    /// "Allocation Failure" (a young generation that is full, or would be) —
    /// the VM-internal distinction between them is already counted per door by
    /// `gc_entry_census`, and inventing a cause string JMC does not know would
    /// buy nothing.
    pub(super) fn cause(self) -> &'static str {
        match self {
            GcDoor::AllocationThreshold | GcDoor::AllocationFailure => "Allocation Failure",
            GcDoor::SystemGc => "System.gc()",
            GcDoor::MetadataThreshold => "Metadata GC Threshold",
        }
    }
}

/// Pre-collection facts for [`gc_event_finish`]. Only captured when some
/// consumer (a JFR recording, an `-Xlog:gc` rule) is armed, or when the heap
/// keeps per-bean GC statistics (gen r4w3/obs2: the generational backend).
pub(super) struct GcEventStart {
    t0: std::time::Instant,
    wall_start_ns: u64,
    heap_used_before: usize,
    /// gen r5w2/obs6: the committed heap BEFORE the collection, for the JFR
    /// `Before GC` heap summary (it carried the after-collection figure, so
    /// a collection that grew or shrank the heap reported the new size on
    /// both edges). `0` unless `events`.
    heap_committed_before: usize,
    /// A JFR recording or `-Xlog:gc` is armed: emit the events.
    events: bool,
    /// gen r4w3/obs2: the JMX beans before the collection, on a heap that
    /// describes them. See [`record_gc_notification`].
    beans: Option<cratonvm_gc::gc_metrics::GcBeanSnapshot>,
    /// gc-common w3-f: `--verbose:gc` is on — print this pause's
    /// [`render_pause_line`] (time-to-safepoint and take-over coverage).
    pause_line: Option<PauseSafepointFacts>,
    /// `(end, heap_after, committed)`, stamped by [`gc_event_seal`] while the
    /// world is still stopped. `None` if the door did not seal (then
    /// [`gc_event_finish`] samples late, as before gc-common w5-a).
    /// `heap_after` / `committed` are `0` unless `events` (nobody reads them).
    sealed: Option<(std::time::Instant, usize, usize)>,
    /// gc-common w5-a: the JMX beans AFTER the collection, stamped by
    /// [`gc_event_seal`] inside the pause when `beans` is `Some` — so the
    /// `GcInfo` a `GarbageCollectionNotificationInfo` / `getLastGcInfo()`
    /// carries (its end time, duration and after-usage) is the collection's,
    /// not the collection's plus the released mutators' allocation.
    beans_after: Option<cratonvm_gc::gc_metrics::GcBeanSnapshot>,
    /// gc-common w7-f: this collection has been booked on the heap's
    /// [`BackendGcBeans`](cratonvm_gc::gc_metrics::BackendGcBeans) (by the
    /// seal, or by [`gc_event_finish`] for a door that did not seal), so it is
    /// never booked twice.
    backend_counted: bool,
    /// gen r5w3/obs7: the process CPU `(user, system)` seconds at the start,
    /// for the `-Xlog:gc+cpu` line. `Some` only when that tag is on, on the
    /// Generational backend.
    cpu_start: Option<(f64, f64)>,
}

impl GcEventStart {
    /// The collection's duration and — when sealed — its after-heap figures:
    /// `(duration_ns, Some((heap_after, committed)))`. ONE end instant for
    /// every consumer (the `[GC] pause:` line, `-Xlog:gc`, JFR), so the three
    /// cannot disagree (`common-w4f-gc-event-durations-sampled-after-release`).
    /// Unsealed, the end is "now" and the heap figures are left to the caller.
    fn end_facts(&self) -> (u64, Option<(usize, usize)>) {
        let (end, after) = match self.sealed {
            Some((end, after, committed)) => (end, Some((after, committed))),
            None => (std::time::Instant::now(), None),
        };
        let duration_ns =
            u64::try_from(end.saturating_duration_since(self.t0).as_nanos()).unwrap_or(u64::MAX);
        (duration_ns, after)
    }
}

/// The safepoint half of one collection pause, sampled by [`gc_event_start`]
/// for the `--verbose:gc` `[GC] pause:` line (gc-common w3-f, 2026-09-23;
/// `common-a-proposal-ttsp-reporting-FIXED-20260923.md` item 1 per pause, and the coverage
/// half of `docs/internal/gc-common-round-20260923/common-c-proposal-takeover-cost-in-the-pause-line-FIXED-20260923.md`).
///
/// Both are read INSIDE the pause, before the collection, which is what makes
/// them this pause's: the TTSP of a pause is recorded once, when the initiator
/// first observes its quota met (`GcBarrier::note_quota_met_locked`, reached
/// from every wait `stw_take_over_and_wait` ends on), and no other pause can
/// record until `complete_gc` releases this one; the take-over coverage
/// counters are reset at the top of `stw_take_over_and_wait` and only its own
/// passes add to them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PauseSafepointFacts {
    /// Request accepted -> quota met, nanoseconds (`TtspStats::last_ns`).
    ttsp_ns: u64,
    /// The thread this pause waited for last (`TtspStats::last_straggler`,
    /// a registry `ThreadId.0`); `None` when the quota was met with no
    /// participating arrival. gc-common w5-a (`handoff-w4f-…` §2): the exit
    /// line named only the SLOWEST pause's straggler.
    straggler: Option<u64>,
    /// Quota met -> take-over returned, nanoseconds
    /// (`gc_quiescence::xt_post_quota_ns`): the frozen-peer frame walk, the
    /// helper-window pass and the skip-span publish. gc-common w4-a.
    xt_post_quota_ns: u64,
    /// The take-over passes' share of `ttsp_ns`
    /// (`gc_quiescence::xt_pass_ns`). gc-common w6-f.
    xt_pass_ns: u64,
    /// `gc_quiescence::xt_cycle_coverage()`: `(passes, taken_over,
    /// unclassified, roots, hw_windows, hw_roots)`.
    xt: (u64, u64, u64, u64, u64, u64),
}

/// The `-Xlog` tag sets a collection logs to (gen r5w3/obs7): `gc` (the
/// per-collection line), and on the Generational backend HotSpot Serial's
/// `gc,start`, `gc,heap` and `gc,cpu` lines ([`xlog_collection_lines`]).
const XLOG_COLLECTION_TAGS: &[crate::runtime::unified_logging::LogTag] = &[
    crate::runtime::unified_logging::LogTag::Gc,
    crate::runtime::unified_logging::LogTag::GcStart,
    crate::runtime::unified_logging::LogTag::GcHeap,
    crate::runtime::unified_logging::LogTag::GcCpu,
];

/// Is any per-collection event consumer armed right now?
fn gc_events_wanted() -> bool {
    cratonvm_jfr::is_enabled()
        || crate::runtime::unified_logging::is_unified_logging_enabled(
            XLOG_COLLECTION_TAGS,
            crate::runtime::unified_logging::LogLevel::Info,
        )
}

/// Is `-Xlog` taking `tag` at info?
fn xlog_on(tag: crate::runtime::unified_logging::LogTag) -> bool {
    crate::runtime::unified_logging::is_unified_logging_enabled(
        &[tag],
        crate::runtime::unified_logging::LogLevel::Info,
    )
}

/// The process's `(user, system)` CPU seconds (`getrusage(RUSAGE_SELF)`),
/// for the `gc,cpu` line — HotSpot's `GCTraceCPUTime` reads the same
/// process-wide times (`os::getTimesSecs`). `None` off Unix or on failure.
/// gen r5w3/obs7.
#[cfg(unix)]
fn process_cpu_secs() -> Option<(f64, f64)> {
    // SAFETY: `rusage` is plain old data; all-zero is a valid value.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a valid, writable `rusage` and `RUSAGE_SELF` a valid
    // `who`; the call writes only into `usage`.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return None;
    }
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
    Some((secs(usage.ru_utime), secs(usage.ru_stime)))
}

#[cfg(not(unix))]
fn process_cpu_secs() -> Option<(f64, f64)> {
    None
}

/// Capture the "before" half of a collection's events. Always `Some` since
/// gc-common w6-f: the duration feeds `HeapRealm::gc_pause_ns_total` (ZGC's
/// `getCollectionTime()`), so the one unconditional cost is an `Instant` pair
/// per collection. JFR / `-Xlog:gc` stay gated.
///
/// gc-common w7-f: G1 (and ZGC, once its state is wired —
/// `HeapRealm::backend_gc_beans`) describe their JMX beans too, so the bean
/// snapshot below is taken on them as on Generational: one G1 region-table
/// walk under its read lock, or ZGC's arena lock once, per pause edge,
/// uncontended with the world stopped.
///
/// Called by each initiator arm once it holds the world stopped, immediately
/// before root collection, so the measured duration is the pause the collector
/// initiated (roots + collection + reference processing + remap).
///
/// gen r4w3/obs2 (2026-09-23): on the generational backend this ALWAYS takes
/// the bean snapshot -- HotSpot's `GCMemoryManager::gc_begin` records the pool
/// usage before every collection too, for `getLastGcInfo()` whether or not
/// anybody listens. Its cost is once per collection, never per allocation:
/// relaxed loads plus the young-from, young-to and old-generation locks once
/// each (`jmx_memory_pools`; the two young ones since gen r5w2/obs6, for the
/// committed granules), uncontended here because the world is stopped and
/// the collector is about to take the same locks.
///
/// gen r5w2/obs6: also publishes this VM's cumulative allocation total into
/// `gc_metrics` (`record_vm_allocation_totals`, the `[GC] cards/alloc:`
/// line's `allocated_bytes_total=`): one relaxed load and one `fetch_max` per
/// collection.
pub(super) fn gc_event_start(shared: &SharedVm) -> Option<GcEventStart> {
    cratonvm_gc::gc_metrics::record_vm_allocation_totals(
        shared
            .mem
            .bytes_allocated_total
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    let events = gc_events_wanted();
    let beans = gc_bean_snapshot(shared);
    // gc-common w3-f: one relaxed load of the barrier's last TTSP and six of
    // the take-over counters, only under `--verbose:gc` (w4-a: and one of the
    // take-over's post-quota time; w5-a: and this pause's straggler, from the
    // same `ttsp_stats` read).
    let pause_line = shared
        .mem
        .verbose_gc
        .load(std::sync::atomic::Ordering::Relaxed)
        .then(|| pause_safepoint_facts(shared));
    if events {
        // A stale phase stash from a collection this door did not report (JFR
        // switched off and on between two) must not be emitted against this one.
        let _ = cratonvm_gc::gen_heap::take_last_pause_phases();
    }
    Some(GcEventStart {
        t0: std::time::Instant::now(),
        wall_start_ns: if events { wall_clock_ns() } else { 0 },
        heap_used_before: if events {
            gc_event_heap_used(&shared.mem.heap)
        } else {
            0
        },
        heap_committed_before: if events {
            shared.mem.heap.committed_bytes()
        } else {
            0
        },
        events,
        beans,
        pause_line,
        sealed: None,
        beans_after: None,
        backend_counted: false,
        // One `getrusage` per collection, only under `-Xlog:gc+cpu`.
        cpu_start: if events
            && matches!(
                shared.mem.heap,
                crate::memory::vm_heap::VmHeap::Generational(_)
            )
            && xlog_on(crate::runtime::unified_logging::LogTag::GcCpu)
        {
            process_cpu_secs()
        } else {
            None
        },
    })
}

/// Sample this pause's safepoint facts (see [`PauseSafepointFacts`]). Must be
/// called INSIDE the pause, after `stw_take_over_and_wait` returned: that is
/// what makes every field this pause's.
fn pause_safepoint_facts(shared: &SharedVm) -> PauseSafepointFacts {
    let t = shared.mem.gc_barrier.ttsp_stats();
    PauseSafepointFacts {
        ttsp_ns: t.last_ns,
        straggler: t.last_straggler,
        xt_post_quota_ns: cratonvm_gc::gc_quiescence::xt_post_quota_ns(),
        xt_pass_ns: cratonvm_gc::gc_quiescence::xt_pass_ns(),
        xt: cratonvm_gc::gc_quiescence::xt_cycle_coverage(),
    }
}

/// Stamp the END of the collection while the world is still stopped — call it
/// immediately before `retire_skip_spans_and_resume` / `complete_gc` (the
/// resumed in-JIT peers and the released mutators can allocate from there on).
/// Every consumer (the `[GC] pause:` line, `-Xlog:gc`, JFR) then reads ONE
/// instant and ONE after-heap figure, none of them inflated by the released
/// mutators' allocation or by the pause line's own stderr write.
///
/// gc-common w5-a (`handoff-w4f-gc-event-end-sampled-in-the-pause`,
/// `common-w4f-gc-event-durations-sampled-after-release`). Printing stays after
/// the release: no I/O inside the pause. The heap reads happen only when a JFR
/// recording or `-Xlog:gc` is armed.
pub(super) fn gc_event_seal(shared: &SharedVm, start: &mut Option<GcEventStart>) {
    if let Some(s) = start.as_mut() {
        let (after, committed) = if s.events {
            (
                gc_event_heap_used(&shared.mem.heap),
                shared.mem.heap.committed_bytes(),
            )
        } else {
            (0, 0)
        };
        // gc-common w7-f: the end instant is taken BEFORE the bean snapshot, so
        // the backend's per-bean pause time (booked here, from this duration)
        // and every other consumer's duration exclude the snapshot's own cost.
        let end = std::time::Instant::now();
        if s.beans.is_some() {
            // The collection is booked on the backend's beans FIRST, so the
            // after-snapshot's counts include it (`gc_notifications_between`
            // decides by the count moving). Generational books its own inside
            // the collector and has no backend state.
            if let Some(b) = shared.mem.backend_gc_beans() {
                b.note_collection(nanos_between(s.t0, end));
                s.backend_counted = true;
            }
            // Generational: the old-generation lock once; G1: one region-table
            // walk; ZGC: the arena lock once — uncontended with the world
            // stopped (the same cost `gc_event_start` pays).
            s.beans_after = gc_bean_snapshot(shared);
            if let (Some(b), Some(a)) = (shared.mem.backend_gc_beans(), s.beans_after.as_ref()) {
                // `getCollectionUsage()`: the pools as this collection left them.
                b.note_collection_usage(&a.pools);
            }
        }
        s.sealed = Some((end, after, committed));
    }
}

/// `end - start` in nanoseconds, saturating.
fn nanos_between(start: std::time::Instant, end: std::time::Instant) -> u64 {
    u64::try_from(end.saturating_duration_since(start).as_nanos()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Non-collection pauses on the `[GC] pause:` line (gc-common w5-a)
// ---------------------------------------------------------------------------

/// A stop-the-world pause that collects nothing, for its `--verbose:gc`
/// `[GC] pause: gc=- door=<token>` line
/// (`common-w4a-non-collection-pauses-have-no-pause-line`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NonCollectionPause {
    /// `stw_publish_frame_traces`: a cross-thread `getStackTrace()` / thread
    /// dump of a running thread.
    FrameTrace,
    /// `NonMovingPause`: the HPROF heap walk.
    HeapDump,
    /// `jvmti_events::request_compiled_loop_exits`: every mutator passes a
    /// poll so an OSR'd loop that must run interpreted leaves compiled code
    /// (`NonMovingPause::request_handshake`, interpreter round i1 wave 13).
    LoopExit,
    /// `obsolete_frames::after_redefinition`: every mutator passes a poll so
    /// a frame still running a body a class redefinition replaced moves onto
    /// its translated copy before it resolves another constant (interpreter
    /// round i1 wave 19, lane L3).
    Redefinition,
    /// Generational concurrent cycle, phase 1 (`maybe_concurrent_gc_at`).
    GenInitialMark,
    /// Generational concurrent cycle, phase 3 (`gen_concurrent_remark_pause`).
    GenRemark,
    /// ZGC mark start (`zgc_concurrent_mark_cycle`).
    ZgcMarkStart,
    /// G1 initial mark (`g1_concurrent_mark_cycle`).
    G1InitialMark,
    /// G1 final remark + cleanup (`g1_final_remark_cleanup`).
    G1Remark,
    /// `request_ic_grace_handshake`: every mutator passes a poll so the
    /// inline-cache retirement grace can advance while a thread spins in
    /// compiled code (JIT round 13 wave 11 lane mega9's patch, applied in
    /// round 14 wave 1 by lane codecache).
    IcGrace,
}

impl NonCollectionPause {
    /// The space-free `door=` token.
    fn token(self) -> &'static str {
        match self {
            NonCollectionPause::FrameTrace => "frame-trace",
            NonCollectionPause::HeapDump => "heap-dump",
            NonCollectionPause::LoopExit => "loop-exit",
            NonCollectionPause::Redefinition => "redefinition",
            NonCollectionPause::GenInitialMark => "gen-initial-mark",
            NonCollectionPause::GenRemark => "gen-remark",
            NonCollectionPause::ZgcMarkStart => "zgc-mark-start",
            NonCollectionPause::G1InitialMark => "g1-initial-mark",
            NonCollectionPause::G1Remark => "g1-remark",
            NonCollectionPause::IcGrace => "ic-grace",
        }
    }
}

impl NonCollectionPause {
    /// The backend whose JMX beans count this pause, and the notification
    /// cause (gc-common w7-f): G1's marking pauses are `G1 Concurrent GC`
    /// pauses (HotSpot counts remark and cleanup there; here the initial mark
    /// is its own pause too, and remark + cleanup are one), with the cause
    /// `No GC` that HotSpot's concurrent-cycle pauses carry and that Micrometer
    /// files as a concurrent phase rather than a pause; a ZGC mark start is a
    /// `ZGC Pauses` pause of the (allocation-triggered) cycle it opens. The
    /// other kinds belong to no bean: frame-trace and heap-dump pauses are not
    /// GC work, and Generational's Serial beans have no concurrent collector.
    fn bean_pause(self) -> Option<(cratonvm_gc::gc_metrics::BackendBeanShape, &'static str)> {
        use cratonvm_gc::gc_metrics::BackendBeanShape;
        match self {
            NonCollectionPause::G1InitialMark | NonCollectionPause::G1Remark => {
                Some((BackendBeanShape::G1, "No GC"))
            }
            NonCollectionPause::ZgcMarkStart => {
                Some((BackendBeanShape::Zgc, GcDoor::AllocationThreshold.cause()))
            }
            NonCollectionPause::FrameTrace
            | NonCollectionPause::HeapDump
            | NonCollectionPause::LoopExit
            | NonCollectionPause::Redefinition
            | NonCollectionPause::GenInitialMark
            | NonCollectionPause::GenRemark
            | NonCollectionPause::IcGrace => None,
        }
    }
}

/// The JMX half of a non-collection pause (gc-common w7-f). Owns its handles
/// (`Arc`s), because the seal and finish calls get only the timer.
struct NonCollectionPauseBeans {
    set: std::sync::Arc<cratonvm_gc::gc_metrics::BackendGcBeans>,
    sampler: crate::memory::vm_heap::JmxPoolSampler,
    cause: &'static str,
    before: cratonvm_gc::gc_metrics::GcBeanSnapshot,
    /// Stamped by [`non_collection_pause_seal`] inside the pause.
    after: Option<cratonvm_gc::gc_metrics::GcBeanSnapshot>,
}

impl NonCollectionPauseBeans {
    fn snapshot(&self) -> cratonvm_gc::gc_metrics::GcBeanSnapshot {
        cratonvm_gc::gc_metrics::GcBeanSnapshot::now(
            self.set.collectors(),
            self.set.pools(&self.sampler.sample()),
        )
    }
}

/// One non-collection pause being timed. Exists under `--verbose:gc` (for the
/// pause line), and — gc-common w7-f — for a pause a backend's JMX beans count
/// ([`NonCollectionPause::bean_pause`]), and — gen r5w2/obs6 — for a
/// generational concurrent-cycle pause while a JFR recording runs.
pub(super) struct NonCollectionPauseTimer {
    kind: NonCollectionPause,
    /// `Some` iff `--verbose:gc`: print the pause line.
    facts: Option<PauseSafepointFacts>,
    t0: std::time::Instant,
    /// Sampled by [`non_collection_pause_seal`] inside the pause.
    work_ns: Option<u64>,
    beans: Option<NonCollectionPauseBeans>,
    /// gen r5w2/obs6: this pause's `jdk.GCPhasePause` row.
    jfr: Option<GenCycleJfrRow>,
}

/// A generational concurrent-cycle pause's `jdk.GCPhasePause` row (`Pause
/// Init Mark` / `Pause Remark`), prepared by [`non_collection_pause_start`]
/// and emitted by [`non_collection_pause_finish`]. gen r5w2/obs6 (2026-09-26;
/// `gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted` item 2).
///
/// Everything the finish needs is OWNED here, because the finish is called
/// with the timer alone: the event type is resolved at the start (the only
/// point that can reach the recorder) and the row is then pushed with
/// [`cratonvm_jfr::builtin::emit_gc_phase_pause_event_typed`], which needs no
/// recorder.
struct GenCycleJfrRow {
    pause_type: cratonvm_jfr::EventTypeId,
    /// The concurrent cycle's `gcId` (see [`gen_cycle_jfr_row`]).
    gc_id: u64,
    name: &'static str,
    /// Wall-clock ns at the pause's `t0`, JFR's time base.
    wall_start_ns: u64,
    /// The initial mark only: the heap's cycle state, to stamp where the
    /// concurrent marking starts (this pause's end).
    opens: Option<std::sync::Arc<cratonvm_gc::gc_metrics::ConcurrentCycleJfr>>,
}

/// Prepare the JFR row of a generational concurrent-cycle pause, or `None`
/// (one match, and one load when `kind` is a generational marking pause)
/// when `kind` is not one, no recording runs, the heap is not generational,
/// or the recorder is busy.
///
/// # The `gcId`
///
/// HotSpot gives a concurrent cycle its own id from the one GC-id sequence
/// (G1's `GC(7) Concurrent Mark Cycle`, its Remark and Cleanup under
/// `GC(7)`), and so does this: the initial mark takes the next
/// `gc_cycle_count` and the remark files under it. Reusing the last
/// collection's id would put the marking pauses under an unrelated young
/// collection in JMC. The id is taken ONLY while a recording runs, so the
/// default path's `GC(n)` / `[GC] pause: gc=` numbering is unchanged; with a
/// recording running, collection ids skip one per concurrent cycle, as on
/// HotSpot G1. (The cycle's own `-Xlog:gc` line keeps its separate
/// `GC(c<n>)` counter.)
///
/// # Inside the pause
///
/// Called with the world stopped. The recorder is taken with `try_lock`, never
/// `lock`: a peer frozen by the take-over at an arbitrary instruction may hold
/// it (compiled code's helpers emit JFR events), and waiting for it would
/// never end. A busy recorder costs this pause its rows (and, at a remark,
/// the cycle's `Concurrent Mark` row); nothing else.
fn gen_cycle_jfr_row(shared: &SharedVm, kind: NonCollectionPause) -> Option<GenCycleJfrRow> {
    let name = match kind {
        NonCollectionPause::GenInitialMark => "Pause Init Mark",
        NonCollectionPause::GenRemark => "Pause Remark",
        _ => return None,
    };
    if !cratonvm_jfr::is_enabled() {
        return None;
    }
    let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap else {
        return None;
    };
    let mut recorder = shared.debug.flight_recorder.try_lock()?;
    let pause_type = cratonvm_jfr::builtin::gc_phase_pause_type(&recorder)?;
    let cycle = h.gc_notifications().concurrent_cycle_jfr();
    let now_ns = wall_clock_ns();
    let (gc_id, opens) = if kind == NonCollectionPause::GenInitialMark {
        let id = shared
            .mem
            .gc_cycle_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        cycle.open(id);
        (id, Some(std::sync::Arc::clone(cycle)))
    } else {
        // No open cycle: its initial mark ran before the recording started
        // (or could not take the recorder). No rows rather than rows under an
        // invented id.
        let (id, mark_start_ns) = cycle.take()?;
        // gen r5w3/unload7: the cycle is not over — its sweep follows, and its
        // `Concurrent Sweep` row files under the same id. Leave the id for the
        // driver (`gen_concurrent_sweep_jfr_open`), which takes it once the
        // remark returns.
        cycle.open(id);
        if mark_start_ns != 0 {
            // The concurrent marking: from the initial-mark pause's end to
            // this pause's start, mutators running throughout. JFR's `gcId`
            // is an `int`; wrap as `gc_event_finish` does.
            cratonvm_jfr::builtin::emit_gc_phase_concurrent_event(
                &mut recorder,
                id as i32, // Truncation: deliberate wrap, see above.
                "Concurrent Mark",
                mark_start_ns,
                now_ns.saturating_sub(mark_start_ns),
            );
        }
        (id, None)
    };
    Some(GenCycleJfrRow {
        pause_type,
        gc_id,
        name,
        wall_start_ns: now_ns,
        opens,
    })
}

/// gen r5w3/unload7 — the `Concurrent Sweep` row's first half, for the
/// generational driver right after its remark pause (obs6's cross-lane
/// request 2, `docs/internal/reviews/gengc-round5-w2-obs6-20260926.md`).
///
/// TAKES the heap's open cycle entry — the id the remark re-opened for the
/// sweep, or the initial mark's own entry when the remark could not reach the
/// recorder — so it cannot outlive this cycle, and returns `(gc_id, sweep
/// start in wall-clock ns)` when a recording runs. The caller holds the phase
/// at `ConcurrentMark`/`ConcurrentSweep` throughout, so no other cycle can
/// have opened in between. `None` on another backend. One uncontended lock
/// per cycle, whatever JFR says.
pub(super) fn gen_concurrent_sweep_jfr_open(shared: &SharedVm) -> Option<(u64, u64)> {
    let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap else {
        return None;
    };
    let (gc_id, _) = h.gc_notifications().concurrent_cycle_jfr().take()?;
    cratonvm_jfr::is_enabled().then(|| (gc_id, wall_clock_ns()))
}

/// gen r5w3/unload7 — the `Concurrent Sweep` row's second half: emit
/// `jdk.GCPhaseConcurrent("Concurrent Sweep")` from the start
/// [`gen_concurrent_sweep_jfr_open`] recorded to now, under the cycle's id.
/// Called after the sweep, outside every pause. `try_lock`, never `lock`: a
/// thread holding the recorder may be parked at a safepoint of a pause that
/// is waiting for this one; a busy recorder costs the row, nothing else.
pub(super) fn gen_concurrent_sweep_jfr_close(shared: &SharedVm, open: Option<(u64, u64)>) {
    let Some((gc_id, start_ns)) = open else {
        return;
    };
    if !cratonvm_jfr::is_enabled() {
        return;
    }
    let Some(mut recorder) = shared.debug.flight_recorder.try_lock() else {
        return;
    };
    cratonvm_jfr::builtin::emit_gc_phase_concurrent_event(
        &mut recorder,
        gc_id as i32, // Truncation: deliberate wrap, as `gc_event_finish`.
        "Concurrent Sweep",
        start_ns,
        wall_clock_ns().saturating_sub(start_ns),
    );
}

/// Emit a [`GenCycleJfrRow`] for a pause whose work took `work_ns`, and, for
/// an initial mark, stamp the cycle's marking start at the pause's end. After
/// the world is released; no lock but the cycle state's.
fn emit_gen_cycle_jfr_row(row: GenCycleJfrRow, work_ns: u64) {
    cratonvm_jfr::builtin::emit_gc_phase_pause_event_typed(
        row.pause_type,
        row.gc_id as i32, // Truncation: deliberate wrap, as `gc_event_finish`.
        row.name,
        row.wall_start_ns,
        work_ns,
    );
    if let Some(cycle) = row.opens {
        cycle.mark_started(row.gc_id, row.wall_start_ns.saturating_add(work_ns));
    }
}

/// Start timing a non-collection pause: `None` (one config load and one
/// match) unless `--verbose:gc`, the pause counts toward a backend's JMX
/// bean, or (gen r5w2/obs6) it is a generational concurrent-cycle pause
/// while a JFR recording runs ([`gen_cycle_jfr_row`]). Call right after
/// `stw_take_over_and_wait` returns, like [`gc_event_start`], so the facts
/// are this pause's.
pub(super) fn non_collection_pause_start(
    shared: &SharedVm,
    kind: NonCollectionPause,
) -> Option<NonCollectionPauseTimer> {
    let beans = kind.bean_pause().and_then(|(shape, cause)| {
        let set = shared.mem.backend_gc_beans()?;
        if set.shape() != shape {
            return None;
        }
        let sampler = shared.mem.heap.jmx_pool_sampler()?;
        let before = cratonvm_gc::gc_metrics::GcBeanSnapshot::now(
            set.collectors(),
            set.pools(&sampler.sample()),
        );
        Some(NonCollectionPauseBeans {
            set: std::sync::Arc::clone(set),
            sampler,
            cause,
            before,
            after: None,
        })
    });
    let verbose = shared
        .mem
        .verbose_gc
        .load(std::sync::atomic::Ordering::Relaxed);
    let jfr = gen_cycle_jfr_row(shared, kind);
    if !verbose && beans.is_none() && jfr.is_none() {
        return None;
    }
    Some(NonCollectionPauseTimer {
        kind,
        facts: verbose.then(|| pause_safepoint_facts(shared)),
        // After the before-snapshot: `work_us` and the bean's pause time are
        // the pause's work, not the sampling.
        t0: std::time::Instant::now(),
        work_ns: None,
        beans,
        jfr,
    })
}

/// Stamp the end of the pause's work while the world is still stopped — call
/// it immediately before `retire_skip_spans_and_resume` (the counterpart of
/// [`gc_event_seal`]). gc-common w7-f: also books the pause on its bean and
/// samples the pools as the pause left them.
pub(super) fn non_collection_pause_seal(timer: &mut Option<NonCollectionPauseTimer>) {
    if let Some(t) = timer.as_mut() {
        let work_ns = nanos_between(t.t0, std::time::Instant::now());
        t.work_ns = Some(work_ns);
        if let Some(b) = t.beans.as_mut() {
            b.set.note_non_collection_pause(work_ns);
            b.after = Some(b.snapshot());
        }
    }
}

/// Print the pause's line, and queue its GC notification (gc-common w7-f).
/// Call after `complete_gc` (no I/O inside the pause). An unsealed timer is
/// measured to now (and booked now).
///
/// The notification is delivered at the next drain point — the next
/// collection's door drain, or the reference-delivery thread's next batch —
/// not by this pause: no drain point runs after a pause that collected
/// nothing, and this function has no thread to run Java on. (G1 follows a
/// remark with mixed collections; a ZGC mark start opens a cycle that ends in
/// a collection.)
pub(super) fn non_collection_pause_finish(timer: Option<NonCollectionPauseTimer>) {
    let Some(t) = timer else {
        return;
    };
    let work_ns = t
        .work_ns
        .unwrap_or_else(|| nanos_between(t.t0, std::time::Instant::now()));
    if let Some(facts) = t.facts {
        eprintln!(
            "{}",
            render_safepoint_pause_line("-", t.kind.token(), facts, "work_us", work_ns)
        );
    }
    if let Some(row) = t.jfr {
        emit_gen_cycle_jfr_row(row, work_ns);
    }
    if let Some(b) = t.beans {
        let after = match b.after.as_ref() {
            Some(a) => a.clone(),
            None => {
                b.set.note_non_collection_pause(work_ns);
                b.snapshot()
            }
        };
        let wall_ms = wall_clock_ns() / 1_000_000;
        for rec in
            cratonvm_gc::gc_metrics::gc_notifications_between(&b.before, &after, b.cause, wall_ms)
        {
            b.set.notifications().record(rec);
        }
    }
}

/// The JMX beans of a heap that describes them: every collector's count and
/// every pool's usage, stamped now. Generational since gen r4w3/obs2 (Serial's
/// beans, the heap's own); G1 since gc-common w7-f, and ZGC once its state is
/// wired (`HeapRealm::backend_gc_beans`). `None` where there is none.
fn gc_bean_snapshot(shared: &SharedVm) -> Option<cratonvm_gc::gc_metrics::GcBeanSnapshot> {
    match &shared.mem.heap {
        crate::memory::vm_heap::VmHeap::Generational(h) => {
            let snap = cratonvm_gc::gc_metrics::GcBeanSnapshot::now(
                h.jmx_collectors(),
                h.jmx_memory_pools(),
            );
            // gen r5w2/obs6: every collection edge is a peak sample, as
            // HotSpot's `GCMemoryManager::gc_begin` / `gc_end` record each
            // pool's peak — the moment eden is full, which a query-time sample
            // almost never sees (`MemoryPoolMXBean.getPeakUsage()`).
            h.gc_notifications().note_pool_peaks(&snap.pools);
            Some(snap)
        }
        heap => shared.mem.backend_gc_beans().map(|b| {
            cratonvm_gc::gc_metrics::GcBeanSnapshot::now(
                b.collectors(),
                b.pools(&heap.backend_pool_samples()),
            )
        }),
    }
}

/// The GC-notification queue of this VM's heap: the generational heap's own,
/// or a backend's (`HeapRealm::backend_gc_beans`). gc-common w7-f.
fn gc_notification_queue(
    shared: &SharedVm,
) -> Option<&cratonvm_gc::gc_metrics::GcNotificationQueue> {
    match &shared.mem.heap {
        crate::memory::vm_heap::VmHeap::Generational(h) => Some(h.gc_notifications()),
        _ => shared.mem.backend_gc_beans().map(|b| b.notifications()),
    }
}

impl crate::vm::realms::HeapRealm {
    /// The JMX bean state of this VM's G1 or ZGC heap, booked by the common
    /// GC event plumbing (gc-common w7-f): G1's lives in the heap
    /// ([`crate::memory::vm_heap::VmHeap::backend_gc_beans`]).
    ///
    /// ZGC's cannot: `VmHeap::Zgc` holds an `Arc<ZgcRealHeap>`, so its beans
    /// live in this realm (`HeapRealm::zgc_gc_beans`, applied from
    /// `handoff-w7f-zgc-backend-gc-beans` at the wave-7 merge).
    ///
    /// Written here, not in `heap_realm.rs`, so every reader (this file's
    /// event plumbing, `vm_exec.rs`'s JMX natives) goes through ONE accessor.
    pub(crate) fn backend_gc_beans(
        &self,
    ) -> Option<&std::sync::Arc<cratonvm_gc::gc_metrics::BackendGcBeans>> {
        match &self.heap {
            crate::memory::VmHeap::Generational(_) => None,
            crate::memory::VmHeap::G1(h) => Some(h.gc_beans()),
            // The ZGC arm, spelled `_` so this crate needs no `zgc` cfg.
            #[allow(unreachable_patterns)]
            _ => Some(&self.zgc_gc_beans),
        }
    }
}

/// Record the collection that ran since `before` as its bean's last GC and,
/// when that bean has a listener, queue its `GarbageCollectionNotificationInfo`
/// (gen r4w3/obs2). Nothing Java runs here: this is HotSpot's
/// `GCMemoryManager::gc_end` + `GCNotifier::pushNotification`. Delivery is
/// [`run_gc_notifications`], after the collection.
///
/// `sealed_after`: the snapshot [`gc_event_seal`] took inside the pause
/// (gc-common w5-a); `None` (an unsealed door) samples now, as before.
///
/// gc-common w7-f: on every heap that describes beans (Generational, G1, and
/// ZGC once wired), one record per bean the collection moved — a ZGC
/// collection is both a `ZGC Cycles` and a `ZGC Pauses` notification.
fn record_gc_notification(
    shared: &SharedVm,
    door: GcDoor,
    before: &cratonvm_gc::gc_metrics::GcBeanSnapshot,
    sealed_after: Option<&cratonvm_gc::gc_metrics::GcBeanSnapshot>,
) {
    let Some(queue) = gc_notification_queue(shared) else {
        return;
    };
    let late;
    let after = match sealed_after {
        Some(a) => a,
        None => {
            let Some(a) = gc_bean_snapshot(shared) else {
                return;
            };
            late = a;
            &late
        }
    };
    let wall_ms = wall_clock_ns() / 1_000_000;
    let recs =
        cratonvm_gc::gc_metrics::gc_notifications_between(before, after, door.cause(), wall_ms);
    // gen r5w3/obs7: HotSpot's `LowMemoryDetector` at `gc_end` — every pool
    // against its usage threshold, and the pools the collection's bean(s)
    // manage against their collection-usage thresholds (Serial: `Copy`'s
    // eden + survivor, `MarkSweepCompact`'s all three). One load unless a
    // program set a threshold; the requests are delivered by
    // `run_gc_notifications`.
    let thresholds = queue.pool_thresholds();
    if thresholds.is_armed() {
        let collected: Vec<&'static str> = recs
            .iter()
            .filter_map(|r| after.collectors.iter().find(|c| c.name == r.collector))
            .flat_map(|c| c.pools.iter().copied())
            .collect();
        thresholds.check(&after.pools, &collected);
    }
    for rec in recs {
        queue.record(rec);
    }
}

/// Whether GC notifications are queued for delivery (gc-common w4-a,
/// `handoff-w3d-gc-notifications-on-the-delivery-thread` edit 3): with the
/// reference-delivery thread on, a collection that queued ONLY notifications
/// must still wake (or start) that thread. One relaxed-ish load; `false` on a
/// heap that describes no GC beans.
pub(super) fn gc_notifications_pending(shared: &SharedVm) -> bool {
    gc_notification_queue(shared).is_some_and(|q| q.has_pending())
}

/// Deliver the heap's queued GC notifications (gen r4w3/obs2): HotSpot's
/// Notification Thread, run by the thread that just collected, after its
/// finalizer and Cleaner drains and for the same reason they run there --
/// the collecting thread is at a point where Java may run. With
/// `CRATONVM_FINALIZER_THREAD=1` the doors do not call this: the
/// reference-delivery thread does, at the end of every batch (gc-common w4-a,
/// `gc_notifications_go_to_delivery_thread` in `gc_and_alloc.rs`). One drainer at a time (a listener that
/// allocates can collect and reach this function again; the nested call
/// returns and the outer loop delivers what it queued).
///
/// `forced` follows the finalizer drain's rule: the allocation doors defer
/// while a JIT helper holds the thread borrow (counted, delivered at the next
/// drain point), and `System.gc()` -- a native call with a legitimate
/// `&mut JvmThread`, whose deferral would be forever for a caller in compiled
/// code -- re-enters under a nested JIT scope instead, exactly like
/// `run_finalizers_forced` / `run_cleaner_actions_forced`.
///
/// Costs one load when nothing is queued, which is every collection of a VM
/// where no `NotificationListener` was ever registered on a GC bean.
pub(super) fn run_gc_notifications(shared: &SharedVm, thread: &mut JvmThread, forced: bool) {
    let Some(queue) = gc_notification_queue(shared) else {
        return;
    };
    if !queue.has_pending() {
        return;
    }
    #[cfg(feature = "management")]
    {
        let jit_borrowed = crate::jit::helpers::is_jit_thread_set();
        if jit_borrowed && !forced {
            queue.note_deferred();
            return;
        }
        let Some(drain) = queue.begin_drain() else {
            return;
        };
        // RAII so an unwinding listener cannot leave the outer JIT level
        // un-restored. Same shape as `run_cleaner_actions_forced`.
        struct NestedJitScope(Option<crate::jit::helpers::JitThreadScope>);
        impl Drop for NestedJitScope {
            fn drop(&mut self) {
                if let Some(scope) = self.0.take() {
                    crate::jit::helpers::restore_jit_thread(scope);
                }
            }
        }
        let _scope = NestedJitScope(if jit_borrowed {
            Some(crate::jit::helpers::set_jit_thread(thread))
        } else {
            None
        });
        // gen r5w3/obs7: GC notifications first, then the pool-threshold
        // sensor requests (`PoolThresholds`), and round again while either
        // grew — a listener that allocates can collect, and the nested drain
        // point returns at once (one drainer), leaving its work to this loop.
        // Bounded: a sensor listener that collects on every call (a counter
        // sensor re-triggers on every collection above its threshold) must
        // not keep this thread here; what is left stays pending for the next
        // drain point.
        const MAX_SENSOR_ROUNDS: u32 = 8;
        let mut sensor_rounds = 0u32;
        loop {
            while let Some(rec) = drain.pop() {
                let info = crate::vm::vm_exec::gc_notification_info_of(&rec);
                let mut ctx = crate::vm::NativeContextImpl {
                    shared,
                    thread: &mut *thread,
                };
                let outcome =
                    match cratonvm_native_builtins::jmx::deliver_gc_notification(&mut ctx, &info) {
                        Ok(true) => cratonvm_gc::gc_metrics::GcNotificationOutcome::Delivered,
                        Ok(false) => cratonvm_gc::gc_metrics::GcNotificationOutcome::Orphaned,
                        Err(_) => cratonvm_gc::gc_metrics::GcNotificationOutcome::Failed,
                    };
                drain.note_outcome(outcome);
            }
            if sensor_rounds == MAX_SENSOR_ROUNDS {
                break;
            }
            sensor_rounds += 1;
            let requests = queue.pool_thresholds().take_requests();
            if requests.is_empty() {
                break;
            }
            for req in requests {
                let mut ctx = crate::vm::NativeContextImpl {
                    shared,
                    thread: &mut *thread,
                };
                // `Sensor.trigger(count, usage)` / `Sensor.clear(count)` on
                // the pool bean `MemoryImpl` handed out; its Java side sends
                // the `MemoryNotificationInfo`. A throwing listener costs
                // this request only, as on HotSpot's Service Thread.
                let _ = cratonvm_native_builtins::jmx::deliver_pool_sensor(
                    &mut ctx,
                    req.pool,
                    req.kind == cratonvm_gc::gc_metrics::PoolThresholdKind::Collection,
                    i32::try_from(req.count).unwrap_or(i32::MAX),
                    req.trigger_usage,
                );
            }
        }
    }
    #[cfg(not(feature = "management"))]
    {
        // No JMX beans to deliver to without the management natives, so no
        // bean can have been enabled and nothing was queued.
        let _ = (thread, forced);
    }
}

/// The heap occupancy a GC event reports (`-Xlog:gc`'s `before->after`, JFR's
/// `jdk.GCHeapSummary.heapUsed`): bytes in objects, NOT the bump high-water.
///
/// gen r4w2/obs (2026-09-23). This was `VmHeap::allocated_bytes`, which on the
/// generational backend is `young_from.used() + old.used()` -- and `used()` is
/// the arena's bump cursor. A non-moving young sweep (the path every JIT-warm
/// workload takes) reclaims onto the from-space FREE LIST and never retracts
/// the cursor, so `allocated_bytes` is identical before and after it by
/// construction: `-XX:+UseGenerationalGC -Xlog:gc BinT 16` printed
/// `Pause Young (Allocation Failure) 8M->8M(64M)` on every pause of a run that
/// was reclaiming the whole time, while `--verbose:gc` on the same run showed
/// the reclamation in `young_free=a->b`. [`VmHeap::live_bytes_estimate`] is
/// `used - free_list` for young plus old used -- the figure that line's
/// `young_free` pair implies -- and it is what the GC-overhead productivity
/// metric already uses for the same reason. G1 and ZGC answer
/// `allocated_bytes` from it unchanged (their occupancy already retreats).
pub(super) fn gc_event_heap_used(heap: &crate::memory::vm_heap::VmHeap) -> usize {
    heap.live_bytes_estimate()
}

fn wall_clock_ns() -> u64 {
    // Truncation-checked: nanos since the epoch fit a u64 until ~2554.
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
}

/// The per-backend names a collection is reported under:
/// `(jfr_name, xlog_label, reclaimed_old_generation)`.
///
/// JFR names follow HotSpot's `GCName` for the nearest collector shape: the
/// generational backend is a copying young generation over a mark-sweep tenured
/// one, i.e. Serial's `DefNew` / `SerialOld`; G1's pause is `G1New`; ZGC is
/// `Z`. `reclaimed_old_generation` is only known on the generational backend,
/// where the collecting thread (this one) carries it in
/// `gc_quiescence::old_gen_reclaimed_last_cycle`.
fn gc_event_shape(shared: &SharedVm) -> (&'static str, &'static str, bool) {
    match &shared.mem.heap {
        crate::memory::vm_heap::VmHeap::Generational(_) => {
            if cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle() {
                ("SerialOld", "Pause Full", true)
            } else {
                ("DefNew", "Pause Young", false)
            }
        }
        crate::memory::vm_heap::VmHeap::G1(_) => ("G1New", "Pause Young", false),
        #[allow(unreachable_patterns)]
        _ => ("Z", "Garbage Collection", false),
    }
}

/// Render one HotSpot-shaped `-Xlog:gc` line (without the decorators the
/// unified logger adds): `GC(3) Pause Young (Allocation Failure) 24M->3M(96M)
/// 2.345ms`, the format HotSpot prints at `-Xlog:gc` (`gc,info`).
pub(super) fn render_xlog_gc_line(
    gc_id: u64,
    label: &str,
    cause: &str,
    heap_before: usize,
    heap_after: usize,
    heap_committed: usize,
    duration_ns: u64,
) -> String {
    const M: usize = 1024 * 1024;
    format!(
        "GC({gc_id}) {label} ({cause}) {}M->{}M({}M) {:.3}ms",
        heap_before / M,
        heap_after / M,
        heap_committed / M,
        duration_ns as f64 / 1_000_000.0,
    )
}

/// Render the backend-independent `--verbose:gc` per-pause line (gc-common
/// w3-f, 2026-09-23):
///
/// `[GC] pause: gc=3 door=system-gc ttsp_us=41 straggler=5 xt_pass_us=30
/// xt_post_quota_us=12 collect_us=2345 xt_passes=1 xt_frozen=0 xt_unclassified=0 xt_roots=0
/// hw_windows=0 hw_roots=0`
///
/// * `ttsp_us` — time-to-safepoint: request accepted -> every counted mutator
///   parked or taken over (HotSpot's "Reaching safepoint" of
///   `-Xlog:safepoint`). Before this, TTSP existed only as the exit
///   `[GC] ttsp:` aggregate, so a slow pause could not be attributed to a
///   slow safepoint.
/// * `straggler` — the registry `ThreadId` this pause waited for last, or
///   `none` when its quota was met with no participating arrival (gc-common
///   w5-a; same spelling as the exit line's `max_straggler=`).
/// * `xt_pass_us` — the take-over passes' own time (OS suspends, register /
///   stack copies, the word probe), summed over this pause's passes. They run
///   inside the barrier wait, so this is PART of `ttsp_us`, not in addition
///   to it: `ttsp_us - xt_pass_us` is the time spent waiting for polling
///   threads (gc-common w6-f). 0 exactly when `xt_passes=0`.
/// * `xt_post_quota_us` — the take-over's work AFTER the quota was met: the
///   frozen peers' interpreter-frame walk, the helper-window pass over blocked
///   peers with JIT frames, and the TLAB skip-span publish (gc-common w4-a;
///   the `<walk>/<hw>` half of
///   `docs/internal/gc-common-round-20260923/common-c-proposal-takeover-cost-in-the-pause-line-FIXED-20260923.md`). 0 when the
///   take-over did not run. `ttsp_us + xt_post_quota_us + collect_us` is the
///   pause from request to release (the initiator's own root snapshot falls
///   inside `ttsp_us`; only the `CRATONVM_DBG_*` pre-collection verifiers fall
///   outside all three).
/// * `collect_us` — roots, collection, reference processing and remap: from
///   the collection's start to its end, sealed INSIDE the pause immediately
///   before the frozen peers resume and the barrier releases (gc-common w5-a,
///   [`gc_event_seal`]); the same instant as the `-Xlog:gc` / JFR duration.
/// * `xt_*` / `hw_*` — this pause's cross-thread JIT take-over coverage
///   (`gc_quiescence::xt_cycle_coverage`): passes, peers frozen, peers whose
///   state could not be classified, conservative roots they yielded, and the
///   helper-window pass's windows and roots.
///
/// One line per collection on every backend, next to the collector's own
/// per-collection line (whose grammar differs per backend). A pause that
/// collects nothing (a G1 / Generational remark, an initial mark, a ZGC mark
/// start, a frame-trace or heap-dump pause) prints the same shape with
/// `gc=-`, its own `door=` token and `work_us=` in place of `collect_us=`
/// ([`non_collection_pause_finish`], gc-common w5-a).
fn render_pause_line(
    gc_id: u64,
    door: GcDoor,
    facts: PauseSafepointFacts,
    collect_ns: u64,
) -> String {
    // A space-free token per door (the HotSpot cause string "Allocation
    // Failure" would split a `key=value` scrape).
    let door = match door {
        GcDoor::AllocationThreshold => "alloc-threshold",
        GcDoor::AllocationFailure => "alloc-failure",
        GcDoor::SystemGc => "system-gc",
        GcDoor::MetadataThreshold => "metadata",
    };
    render_safepoint_pause_line(&gc_id.to_string(), door, facts, "collect_us", collect_ns)
}

/// The one `[GC] pause:` grammar, for collections ([`render_pause_line`]) and
/// non-collection pauses ([`non_collection_pause_finish`]) alike.
fn render_safepoint_pause_line(
    gc: &str,
    door: &str,
    facts: PauseSafepointFacts,
    work_key: &str,
    work_ns: u64,
) -> String {
    let (passes, frozen, unclassified, roots, hw_windows, hw_roots) = facts.xt;
    let straggler = facts
        .straggler
        .map_or_else(|| "none".to_string(), |tid| tid.to_string());
    format!(
        "[GC] pause: gc={gc} door={door} ttsp_us={} straggler={straggler} \
         xt_pass_us={} xt_post_quota_us={} {work_key}={} \
         xt_passes={passes} xt_frozen={frozen} xt_unclassified={unclassified} \
         xt_roots={roots} hw_windows={hw_windows} hw_roots={hw_roots}",
        facts.ttsp_ns / 1_000,
        facts.xt_pass_ns / 1_000,
        facts.xt_post_quota_ns / 1_000,
        work_ns / 1_000,
    )
}

/// Account one completed collection: advance the VM's collection ordinal
/// (`gc_cycle_count`, which is also the JFR `gcId` and the `GC(n)` of the
/// `-Xlog:gc` line), and — only when `start` is `Some` — emit the JFR GC events
/// and the `-Xlog:gc` line.
///
/// Called by every initiator arm, after the collection and before the
/// finalizer / cleaner drains, so a door cannot count a collection without
/// reporting it or report one without counting it.
pub(super) fn gc_event_finish(shared: &SharedVm, door: GcDoor, start: Option<GcEventStart>) {
    let gc_id = shared
        .mem
        .gc_cycle_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let Some(start) = start else {
        return;
    };
    // gc-common w5-a: ONE end instant for every consumer below, sealed inside
    // the pause by `gc_event_seal` (before, the pause line took one `elapsed`
    // after the bean record and the `-Xlog:gc` / JFR duration a second one
    // after the pause line's own stderr write, both with the world released).
    let (duration_ns, sealed_after) = start.end_facts();
    // gc-common w6-f: the backend-independent pause sum ZGC's
    // `getCollectionTime()` reads (`vm_exec.rs::gc_collection_time_ms`).
    shared
        .mem
        .gc_pause_ns_total
        .fetch_add(duration_ns, std::sync::atomic::Ordering::Relaxed);
    // gen r4w3/obs2: the per-bean record (last GC, notification queue) first,
    // then the JFR / `-Xlog:gc` events only if one of them is armed.
    if let Some(before) = start.beans.as_ref() {
        // gc-common w7-f: a door that did not seal books the backend's beans
        // here, before the late snapshot `record_gc_notification` takes.
        if !start.backend_counted {
            if let Some(b) = shared.mem.backend_gc_beans() {
                b.note_collection(duration_ns);
            }
        }
        record_gc_notification(shared, door, before, start.beans_after.as_ref());
    }
    if let Some(facts) = start.pause_line {
        eprintln!("{}", render_pause_line(gc_id, door, facts, duration_ns));
    }
    if !start.events {
        return;
    }
    let (heap_after, committed) = sealed_after.unwrap_or_else(|| {
        (
            gc_event_heap_used(&shared.mem.heap),
            shared.mem.heap.committed_bytes(),
        )
    });
    let (jfr_name, xlog_label, old) = gc_event_shape(shared);

    if cratonvm_jfr::is_enabled() {
        // JFR's `gcId` is an `int`; wrap rather than saturate so ids stay
        // distinct for longer than `i32::MAX` collections would ever last.
        let id = gc_id as i32; // Truncation: deliberate wrap, see above.
        let to_i64 = |b: usize| i64::try_from(b).unwrap_or(i64::MAX);
        let max = to_i64(shared.config.max_heap_size);
        let end_ns = start.wall_start_ns.saturating_add(duration_ns);
        let mut jfr = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_gc_heap_summary_event(
            &mut jfr,
            id,
            "Before GC",
            "Java Heap",
            to_i64(start.heap_used_before),
            // gen r5w2/obs6: the committed heap as it was BEFORE (it was the
            // after figure on both edges).
            to_i64(start.heap_committed_before),
            max,
            start.wall_start_ns,
        );
        cratonvm_jfr::builtin::emit_gc_event(
            &mut jfr,
            id,
            jfr_name,
            door.cause(),
            start.wall_start_ns,
            duration_ns,
        );
        // `jdk.GCPhasePause` (gen r4w2/obs): the generational collector's phase
        // marks, stashed by `collect_garbage_inner`'s pause timer while a
        // recording runs. Consecutive from the pause start, plus the residual
        // as its own `other` phase, so the rows partition the collector's
        // pause exactly as the `[gcpause]` line does.
        if let Some((pause_us, marks)) = cratonvm_gc::gen_heap::take_last_pause_phases() {
            let rows = phase_pause_rows(start.wall_start_ns, pause_us, &marks);
            for (name, start_ns, dur_ns) in rows {
                cratonvm_jfr::builtin::emit_gc_phase_pause_event(
                    &mut jfr, id, name, start_ns, dur_ns,
                );
            }
        }
        if old {
            cratonvm_jfr::builtin::emit_old_gc_event(&mut jfr, id, start.wall_start_ns, duration_ns);
        } else if let Some(threshold) = shared.mem.heap.tenuring_threshold() {
            // `VmHeap::tenuring_threshold` per backend (gc-common w3-f,
            // 2026-09-23; `handoff-w2e-comment-followups-outside-lane-e.md`
            // item 8): Generational's `PROMOTION_AGE - 1` (HotSpot's unit since
            // gen r5w2/obs6; it was `PROMOTION_AGE`, one too high), G1's ADAPTIVE threshold
            // (this arm passed the literal `15`, G1's configured ceiling,
            // whatever the policy had lowered it to), and `None` on ZGC --
            // its collection is not a young-generation collection, and
            // HotSpot's non-generational ZGC emits no young event either.
            cratonvm_jfr::builtin::emit_young_gc_event(
                &mut jfr,
                id,
                i32::try_from(threshold).unwrap_or(i32::MAX),
                start.wall_start_ns,
                duration_ns,
            );
        }
        cratonvm_jfr::builtin::emit_gc_heap_summary_event(
            &mut jfr,
            id,
            "After GC",
            "Java Heap",
            to_i64(heap_after),
            to_i64(committed),
            max,
            end_ns,
        );
    }

    xlog_collection_lines(
        shared,
        gc_id,
        xlog_label,
        door,
        &start,
        (heap_after, committed),
        duration_ns,
    );
}

/// The `-Xlog` lines of one collection, in HotSpot Serial's order
/// (gen r5w3/obs7, 2026-09-26;
/// `gengc-r5w2-obs6-proposal-xlog-gc-hotspot-compatible-output`):
///
/// ```text
/// [gc,start    ] GC(3) Pause Young (Allocation Failure)
/// [gc,heap     ] GC(3) DefNew: 17472K(19648K)->2176K(19648K) Eden: 17472K(17472K)->0K(17472K) From: 0K(2176K)->2176K(2176K)
/// [gc,heap     ] GC(3) Tenured: 0K(43712K)->3000K(43712K)
/// [gc          ] GC(3) Pause Young (Allocation Failure) 17M->5M(61M) 4.567ms
/// [gc,cpu      ] GC(3) User=0.00s Sys=0.00s Real=0.00s
/// ```
///
/// The `gc` line is every backend's (unchanged); the `gc,start`, `gc,heap`
/// and `gc,cpu` lines are printed on the Generational backend only, whose
/// pools are Serial's (`DefNew` = eden + survivor, `From` = the survivor
/// pool, `Tenured` = the old generation, all from the JMX pool snapshots the
/// collection already takes at both edges). G1's and ZGC's HotSpot
/// counterparts have a different grammar and are not attempted.
///
/// The `gc,start` line is printed with the others after the pause (no I/O
/// inside it), so its uptime decoration is the collection's end, not its
/// start. Not printed: `gc,phases` (this collector's phases are not
/// Serial's), `gc,metaspace` (no per-collection metaspace figures), and the
/// `gc,heap,exit` summary.
fn xlog_collection_lines(
    shared: &SharedVm,
    gc_id: u64,
    label: &str,
    door: GcDoor,
    start: &GcEventStart,
    (heap_after, committed): (usize, usize),
    duration_ns: u64,
) {
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
    let generational = matches!(
        shared.mem.heap,
        crate::memory::vm_heap::VmHeap::Generational(_)
    );
    if generational && xlog_on(LogTag::GcStart) {
        log_unified(
            &[LogTag::GcStart],
            LogLevel::Info,
            &format!("GC({gc_id}) {label} ({})", door.cause()),
        );
    }
    if generational && xlog_on(LogTag::GcHeap) {
        let late;
        let after = match start.beans_after.as_ref() {
            Some(a) => Some(a),
            None => {
                late = gc_bean_snapshot(shared);
                late.as_ref()
            }
        };
        if let (Some(before), Some(after)) = (start.beans.as_ref(), after) {
            if let Some(lines) = render_xlog_heap_lines(gc_id, &before.pools, &after.pools) {
                for line in lines {
                    log_unified(&[LogTag::GcHeap], LogLevel::Info, &line);
                }
            }
        }
    }
    if xlog_on(LogTag::Gc) {
        crate::runtime::unified_logging::gc_info(&render_xlog_gc_line(
            gc_id,
            label,
            door.cause(),
            start.heap_used_before,
            heap_after,
            committed,
            duration_ns,
        ));
    }
    if generational && xlog_on(LogTag::GcCpu) {
        if let (Some((user0, sys0)), Some((user1, sys1))) = (start.cpu_start, process_cpu_secs()) {
            log_unified(
                &[LogTag::GcCpu],
                LogLevel::Info,
                &render_xlog_cpu_line(gc_id, user1 - user0, sys1 - sys0, duration_ns),
            );
        }
    }
}

/// HotSpot Serial's two `gc,heap` lines (`SerialHeap::print_heap_change`,
/// `HEAP_CHANGE_FORMAT`: `name: usedK(capacityK)->usedK(capacityK)`, bytes
/// divided by 1024 and truncated) from the pool snapshots at the two edges
/// of the collection. `None` unless both snapshots carry Serial's three
/// pools. gen r5w3/obs7.
fn render_xlog_heap_lines(
    gc_id: u64,
    before: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
    after: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
) -> Option<[String; 2]> {
    use cratonvm_gc::gc_metrics::{JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED};
    let find = |pools: &[cratonvm_gc::gc_metrics::JmxPoolUsage], name: &str| {
        pools.iter().find(|p| p.name == name).map(|p| (p.used, p.committed))
    };
    let edges = |name: &str| Some((find(before, name)?, find(after, name)?));
    let (eden0, eden1) = edges(JMX_POOL_EDEN)?;
    let (from0, from1) = edges(JMX_POOL_SURVIVOR)?;
    let (old0, old1) = edges(JMX_POOL_TENURED)?;
    let change = |name: &str, (u0, c0): (u64, u64), (u1, c1): (u64, u64)| {
        format!(
            "{name}: {}K({}K)->{}K({}K)",
            u0 / 1024,
            c0 / 1024,
            u1 / 1024,
            c1 / 1024
        )
    };
    let sum = |(a, b): (u64, u64), (c, d): (u64, u64)| (a.saturating_add(c), b.saturating_add(d));
    Some([
        format!(
            "GC({gc_id}) {} {} {}",
            change("DefNew", sum(eden0, from0), sum(eden1, from1)),
            change("Eden", eden0, eden1),
            change("From", from0, from1),
        ),
        format!("GC({gc_id}) {}", change("Tenured", old0, old1)),
    ])
}

/// HotSpot's `gc,cpu` line (`GCTraceCPUTime`: `User=%3.2fs Sys=%3.2fs
/// Real=%3.2fs`). A negative delta (a clock that does not move per process
/// on this platform) prints as 0. gen r5w3/obs7.
fn render_xlog_cpu_line(gc_id: u64, user_s: f64, sys_s: f64, duration_ns: u64) -> String {
    format!(
        "GC({gc_id}) User={:.2}s Sys={:.2}s Real={:.2}s",
        user_s.max(0.0),
        sys_s.max(0.0),
        duration_ns as f64 / 1_000_000_000.0,
    )
}

/// Lay a collection's phase marks out as consecutive `jdk.GCPhasePause` rows:
/// `(phase, start_ns, duration_ns)`, starting at `start_ns`, followed by an
/// `other` row holding the part of `pause_us` no mark covered (always emitted,
/// like `[gcpause]`'s `other=`, so a future uncovered phase is visible rather
/// than silently missing from the sum). gen r4w2/obs.
fn phase_pause_rows(
    start_ns: u64,
    pause_us: u128,
    marks: &[(&'static str, u128)],
) -> Vec<(&'static str, u64, u64)> {
    let to_ns = |us: u128| u64::try_from(us.saturating_mul(1000)).unwrap_or(u64::MAX);
    let mut rows = Vec::with_capacity(marks.len() + 1);
    let mut at = start_ns;
    let mut covered: u128 = 0;
    for (name, us) in marks {
        let d = to_ns(*us);
        rows.push((*name, at, d));
        at = at.saturating_add(d);
        covered = covered.saturating_add(*us);
    }
    rows.push(("other", at, to_ns(pause_us.saturating_sub(covered))));
    rows
}

#[cfg(test)]
mod gc_event_tests {
    use super::*;

    /// gen r4w2/obs: the JFR phase rows are consecutive and, with `other`,
    /// sum to the collector's pause.
    #[test]
    fn phase_pause_rows_partition_the_pause() {
        let rows = phase_pause_rows(1_000, 900, &[("roots", 100), ("cheney_drain", 650)]);
        assert_eq!(
            rows,
            vec![
                ("roots", 1_000, 100_000),
                ("cheney_drain", 101_000, 650_000),
                ("other", 751_000, 150_000),
            ]
        );
        let total: u64 = rows.iter().map(|r| r.2).sum();
        assert_eq!(total, 900_000);
        // No marks: one `other` row carrying the whole pause.
        assert_eq!(phase_pause_rows(5, 3, &[]), vec![("other", 5, 3_000)]);
    }

    // gen r4w2/obs: `gc_event_heap_used` is deliberately NOT tested here with a
    // real heap. Constructing a `GenerationalHeap` publishes into process-global
    // bounds tables (`gc/tests/published_bounds_isolation.rs` explains the
    // flake that caused), and this binary runs whole VMs in parallel. The
    // computation -- a non-moving young sweep moves `VmHeap::live_bytes_estimate`
    // and leaves `allocated_bytes` where it was, which is the `8M->8M` bug --
    // is pinned in its own process by
    // `gc/tests/gengc_r4w2_obs.rs::xlog_occupancy_sees_a_non_moving_young_sweep`.

    #[test]
    fn xlog_gc_line_has_hotspots_shape() {
        let line = render_xlog_gc_line(
            3,
            "Pause Young",
            GcDoor::AllocationThreshold.cause(),
            24 * 1024 * 1024 + 5,
            3 * 1024 * 1024,
            96 * 1024 * 1024,
            2_345_000,
        );
        assert_eq!(line, "GC(3) Pause Young (Allocation Failure) 24M->3M(96M) 2.345ms");
    }

    /// gc-common w3-f: the per-pause `--verbose:gc` line carries this pause's
    /// TTSP and take-over coverage as space-free `key=value` pairs.
    #[test]
    fn w3f_pause_line_carries_ttsp_and_takeover_coverage() {
        let facts = PauseSafepointFacts {
            ttsp_ns: 41_999,
            straggler: Some(3),
            xt_post_quota_ns: 12_500,
            xt_pass_ns: 30_100,
            xt: (2, 1, 0, 17, 3, 5),
        };
        let line = render_pause_line(7, GcDoor::AllocationFailure, facts, 2_345_678);
        assert_eq!(
            line,
            "[GC] pause: gc=7 door=alloc-failure ttsp_us=41 straggler=3 xt_pass_us=30 \
             xt_post_quota_us=12 collect_us=2345 xt_passes=2 xt_frozen=1 xt_unclassified=0 xt_roots=17 \
             hw_windows=3 hw_roots=5"
        );
        for door in [
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::SystemGc,
            GcDoor::MetadataThreshold,
        ] {
            let l = render_pause_line(0, door, facts, 0);
            assert!(
                l.split(' ').skip(2).all(|kv| kv.split_once('=').is_some()),
                "every field after `[GC] pause:` is key=value: {l}"
            );
        }
        // gc-common w5-a: no participating arrival spells `none`, like the
        // exit line's `max_straggler=`.
        let none = PauseSafepointFacts {
            straggler: None,
            ..facts
        };
        assert!(render_pause_line(1, GcDoor::SystemGc, none, 0).contains(" straggler=none "));
    }

    /// gc-common w5-a (`common-w4f-gc-event-durations-sampled-after-release`):
    /// a sealed collection hands the pause line and the `-Xlog:gc` line the
    /// SAME duration, and its after-heap figures, however late they render.
    #[test]
    fn w5a_a_sealed_collection_reports_one_duration_everywhere() {
        let t0 = std::time::Instant::now();
        let end = t0 + std::time::Duration::from_micros(2_345);
        let start = GcEventStart {
            t0,
            wall_start_ns: 0,
            heap_used_before: 24 * 1024 * 1024,
            heap_committed_before: 96 * 1024 * 1024,
            events: true,
            beans: None,
            pause_line: None,
            sealed: Some((end, 3 * 1024 * 1024, 96 * 1024 * 1024)),
            beans_after: None,
            backend_counted: false,
            cpu_start: None,
        };
        // Rendering happens "later" -- the sealed end does not move.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (duration_ns, after) = start.end_facts();
        assert_eq!(duration_ns, 2_345_000);
        assert_eq!(after, Some((3 * 1024 * 1024, 96 * 1024 * 1024)));
        let facts = PauseSafepointFacts {
            ttsp_ns: 0,
            straggler: None,
            xt_post_quota_ns: 0,
            xt_pass_ns: 0,
            xt: (0, 0, 0, 0, 0, 0),
        };
        let pause = render_pause_line(4, GcDoor::AllocationThreshold, facts, duration_ns);
        let (heap_after, committed) = after.unwrap();
        let xlog = render_xlog_gc_line(
            4,
            "Pause Young",
            GcDoor::AllocationThreshold.cause(),
            start.heap_used_before,
            heap_after,
            committed,
            duration_ns,
        );
        assert!(pause.contains(" collect_us=2345 "), "{pause}");
        assert_eq!(
            xlog,
            "GC(4) Pause Young (Allocation Failure) 24M->3M(96M) 2.345ms"
        );
        // Unsealed: measured to "now", and no heap figures (the caller reads
        // them live, the pre-w5-a behaviour).
        let unsealed = GcEventStart {
            sealed: None,
            ..start
        };
        let (late_ns, late_after) = unsealed.end_facts();
        assert!(late_ns >= 2_000_000);
        assert_eq!(late_after, None);
    }

    /// gc-common w5-a (`common-w4a-non-collection-pauses-have-no-pause-line`):
    /// a non-collection pause prints the collection line's grammar with
    /// `gc=-`, its own door token and `work_us=`.
    #[test]
    fn w5a_non_collection_pause_line_shape() {
        let facts = PauseSafepointFacts {
            ttsp_ns: 7_000,
            straggler: Some(9),
            xt_post_quota_ns: 1_000,
            xt_pass_ns: 2_000,
            xt: (1, 0, 0, 0, 0, 0),
        };
        let line = render_safepoint_pause_line(
            "-",
            NonCollectionPause::G1Remark.token(),
            facts,
            "work_us",
            40_000_000,
        );
        assert_eq!(
            line,
            "[GC] pause: gc=- door=g1-remark ttsp_us=7 straggler=9 xt_pass_us=2 \
             xt_post_quota_us=1 work_us=40000 xt_passes=1 xt_frozen=0 xt_unclassified=0 xt_roots=0 \
             hw_windows=0 hw_roots=0"
        );
        let kinds = [
            NonCollectionPause::FrameTrace,
            NonCollectionPause::HeapDump,
            NonCollectionPause::LoopExit,
            NonCollectionPause::Redefinition,
            NonCollectionPause::GenInitialMark,
            NonCollectionPause::GenRemark,
            NonCollectionPause::ZgcMarkStart,
            NonCollectionPause::G1InitialMark,
            NonCollectionPause::G1Remark,
            NonCollectionPause::IcGrace,
        ];
        let mut tokens: Vec<&str> = kinds.iter().map(|k| k.token()).collect();
        for t in &tokens {
            assert!(!t.contains(' ') && !t.contains('='), "scrapable token: {t}");
            for door in ["alloc-threshold", "alloc-failure", "system-gc"] {
                assert_ne!(*t, door, "a non-collection token must not alias a GC door");
            }
        }
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), kinds.len(), "tokens are distinct");
        // A sealed timer reports its sealed work time, not the time to print.
        let mut timer = Some(NonCollectionPauseTimer {
            kind: NonCollectionPause::FrameTrace,
            facts: Some(facts),
            t0: std::time::Instant::now(),
            work_ns: None,
            beans: None,
            jfr: None,
        });
        non_collection_pause_seal(&mut timer);
        assert!(timer.as_ref().is_some_and(|t| t.work_ns.is_some()));
        non_collection_pause_seal(&mut None);
    }

    /// gc-common w7-f: exactly the marking pauses of the backend a bean set
    /// describes count toward it; frame-trace, heap-dump and Generational's
    /// marking pauses count toward none. G1's carry `No GC` (a concurrent
    /// phase to Micrometer), ZGC's mark start an allocation cause (a pause).
    #[test]
    fn w7f_only_backend_marking_pauses_count_toward_a_bean() {
        use cratonvm_gc::gc_metrics::BackendBeanShape;
        assert_eq!(
            NonCollectionPause::G1Remark.bean_pause(),
            Some((BackendBeanShape::G1, "No GC"))
        );
        assert_eq!(
            NonCollectionPause::G1InitialMark.bean_pause().map(|p| p.0),
            Some(BackendBeanShape::G1)
        );
        assert_eq!(
            NonCollectionPause::ZgcMarkStart.bean_pause(),
            Some((BackendBeanShape::Zgc, "Allocation Failure"))
        );
        for k in [
            NonCollectionPause::FrameTrace,
            NonCollectionPause::HeapDump,
            NonCollectionPause::LoopExit,
            NonCollectionPause::GenInitialMark,
            NonCollectionPause::GenRemark,
        ] {
            assert_eq!(k.bean_pause(), None, "{k:?}");
        }
    }

    /// gen r5w3/obs7: HotSpot Serial's `gc,heap` lines from the pool
    /// snapshots at the two edges of a collection: `DefNew` is eden +
    /// survivor, `From` the survivor pool, all in truncated KiB.
    #[test]
    fn xlog_heap_lines_have_serials_shape() {
        use cratonvm_gc::gc_metrics::{
            serial_pool_managers, JmxPoolUsage, JMX_POOL_EDEN, JMX_POOL_SURVIVOR,
            JMX_POOL_TENURED,
        };
        const K: u64 = 1024;
        let pool = |name, used, committed| JmxPoolUsage {
            name,
            init: None,
            used,
            committed,
            max: None,
            collection_used: None,
            managers: serial_pool_managers(name),
        };
        let before = [
            pool(JMX_POOL_EDEN, 17_472 * K + 5, 17_472 * K),
            pool(JMX_POOL_SURVIVOR, 0, 0),
            pool(JMX_POOL_TENURED, 0, 43_712 * K),
        ];
        let after = [
            pool(JMX_POOL_EDEN, 0, 17_472 * K),
            pool(JMX_POOL_SURVIVOR, 2_176 * K, 2_176 * K),
            pool(JMX_POOL_TENURED, 3_000 * K, 43_712 * K),
        ];
        let lines = render_xlog_heap_lines(3, &before, &after).expect("Serial's pools");
        assert_eq!(
            lines[0],
            "GC(3) DefNew: 17472K(17472K)->2176K(19648K) Eden: 17472K(17472K)->0K(17472K) \
             From: 0K(0K)->2176K(2176K)"
        );
        assert_eq!(lines[1], "GC(3) Tenured: 0K(43712K)->3000K(43712K)");
        // Not Serial's pools (G1's): no lines.
        assert!(render_xlog_heap_lines(3, &before[..1], &after).is_none());
    }

    #[test]
    fn xlog_cpu_line_has_hotspots_shape() {
        assert_eq!(
            render_xlog_cpu_line(7, 0.013, 0.0049, 9_876_543),
            "GC(7) User=0.01s Sys=0.00s Real=0.01s"
        );
        assert_eq!(
            render_xlog_cpu_line(0, -0.5, 1.5, 1_234_000_000),
            "GC(0) User=0.00s Sys=1.50s Real=1.23s"
        );
    }

    #[test]
    fn system_gc_is_reported_with_hotspots_cause_string() {
        assert_eq!(GcDoor::SystemGc.cause(), "System.gc()");
        let line = render_xlog_gc_line(0, "Pause Full", GcDoor::SystemGc.cause(), 0, 0, 0, 999);
        assert_eq!(line, "GC(0) Pause Full (System.gc()) 0M->0M(0M) 0.001ms");
    }
}
