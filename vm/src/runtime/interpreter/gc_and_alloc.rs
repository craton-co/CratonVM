// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Allocation, garbage collection, and the root protocol between them.
//!
//! The interpreter's side of the collector: TLAB allocation and its refill
//! wedge-break, the GC entry points (`maybe_gc`, `maybe_gc_forced`,
//! `maybe_concurrent_gc`, the G1 cycle drivers), reference processing,
//! cleaners and finalizers, `safepoint_check`, and the root snapshot the
//! collector walks.
//!
//! `stw_take_over_and_wait` is the one to read first if something hangs at
//! a safepoint: it is the protocol by which one thread becomes the STW
//! initiator and the others park, and every deadlock in this area has been
//! a thread that never reached it.
//!
//! `apply_pointer_map_to_thread` is the moving-collector half — after a
//! young collection relocates objects, every root channel has to be
//! rewritten, and a channel that is missed is a stale reference that will
//! be dereferenced later, somewhere else.
//!
//! The `remap_trace_*`, `push_prov_*`, `deposit_gap_*`, `nret_*` and
//! `getfield_ring_*` helpers are the provenance tracing built for exactly
//! those investigations: off by default, and the reason a reclaimed-object
//! report can name the site that pushed the reference.

use super::*;

// LANE W7-D — the mark-cycle door census. `note_mark_door` is called at most
// once per PAUSE (and once per `g1_force_full_cycle` call), never per
// allocation; see `g1_drive_mark_cycle`.
use cratonvm_gc::g1::{note_mark_door, MarkDoor, MarkDoorOutcome};
// gen r4/plumbing (2026-09-23): per-collection accounting, see `gc_events`.
use super::gc_events::{
    gc_event_finish, gc_event_seal, gc_event_start, non_collection_pause_finish,
    non_collection_pause_seal, non_collection_pause_start, run_gc_notifications, GcDoor,
    NonCollectionPause,
};

/// DBG (CRATONVM_DBG_MTROOTS): publish the in-progress collection's context
/// (reason / initiator thread / blocked-thread count) so the sweep-zero detector
/// can name WHICH GC reclaimed a live object — pinning the initiator-vs-blocked
/// root-coverage gap. Reason: 1=System.gc, 2=alloc-young, 3=forced-alloc.
#[inline]
pub(super) fn mtroots_on() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MTROOTS").is_some())
}

#[inline]
pub(super) fn mtroots_set_gc_ctx(shared: &SharedVm, thread: &JvmThread, reason: u8) {
    if mtroots_on() {
        cratonvm_gc::gen_heap::set_gc_context(
            reason,
            // Widening: smaller value -> u32 (value fits)
            thread.thread_id.0 as u32,
            // Widening: smaller value -> u32 (value fits)
            shared.mem.gc_barrier.blocked_count() as u32,
        );
    }
}

/// DBG (CRATONVM_DBG_MTROOTS): dump the GC initiator's frames + their
/// object-local heap addresses, so a later `[sweep-zero] RECLAIMED-LIVE ptr=X`
/// can be cross-referenced — was X actually present as a scanned root here?
/// (Cross-reference within ONE run: addresses are run-specific.)
pub(super) fn mtroots_dump_initiator(shared: &SharedVm, thread: &JvmThread, reason: u8) {
    if !mtroots_on() {
        return;
    }
    let mut buf = String::new();
    use std::fmt::Write as _;
    let _ = write!(
        buf,
        "[mtroots] GC reason={} initiator_tid={} alive={} blocked={} frames={}",
        reason,
        thread.thread_id.0,
        shared.threads.thread_registry.alive_count(),
        shared.mem.gc_barrier.blocked_count(),
        thread.frames.len(),
    );
    // Top frames only (the relevant Java call site).
    for frame in thread.frames.iter().rev().take(8) {
        let mut objs: Vec<ObjectRef> = Vec::new();
        frame.scan_local_objects(&mut objs, &shared.mem.heap);
        let _ = write!(
            buf,
            "\n[mtroots]   {}.{} locals=[",
            frame.class_name(),
            frame.method_name()
        );
        for (i, o) in objs.iter().enumerate() {
            if i > 0 {
                let _ = write!(buf, " ");
            }
            let _ = write!(buf, "{:p}", o.as_ptr());
        }
        let _ = write!(buf, "]");
    }
    // Per-thread blocked-state census: a thread holding a live oop while counted
    // BLOCKED here is excluded from `expected` and its (possibly stale) deposit
    // snapshot is used instead of its current frames — the suspected gap.
    let states = shared.threads.thread_registry.dump_blocked_states();
    let nblk = states.iter().filter(|(_, b, _)| *b).count();
    let _ = write!(
        buf,
        "\n[mtroots]   thread-census ({} alive, {} in_blocked):",
        states.len(),
        nblk
    );
    for (tid, blk, snap) in states {
        let _ = write!(buf, " t{}{}({})", tid, if blk { "B" } else { "R" }, snap);
    }
    eprintln!("{buf}");
}

/// DBG (CRATONVM_DBG_MTROOTS): after a thread resumes from an STW, scan its OWN
/// frames for object refs whose header is all-zero — i.e. an object the sweep
/// RECLAIMED while this thread still references it (a missed-root reclamation),
/// naming the holder thread + its blocked state + the method, for ANY failure
/// mode (checkcast CCE / SIGSEGV / invokevirtual), not just the invokevirtual
/// all-zero-receiver detector.
pub(super) fn mtroots_selfcheck(thread: &JvmThread, heap: &crate::memory::VmHeap, location: &str) {
    if !mtroots_on() {
        return;
    }
    let blocked = thread
        .gc_block_state
        .in_blocked_region
        .load(std::sync::atomic::Ordering::Acquire);
    for frame in thread.frames.iter().rev() {
        let mut objs: Vec<ObjectRef> = Vec::new();
        frame.scan_local_objects(&mut objs, heap);
        for o in objs {
            // SAFETY: `o` is a live ObjectRef scanned from this frame; its pointer is a valid heap address with at least a 16-byte readable header.
            let hdr: [u8; 16] = unsafe { std::ptr::read(o.as_ptr() as *const [u8; 16]) };
            if hdr == [0u8; 16] {
                eprintln!(
                    "[mtroots] SELFCHECK@{} tid={} in_blocked={} kind={:?} method={}.{} \
                     holds RECLAIMED(all-zero) obj @{:p}",
                    location,
                    thread.thread_id.0,
                    blocked,
                    thread.kind,
                    frame.class_name(),
                    frame.method_name(),
                    o.as_ptr(),
                );
            }
        }
    }
}

/// BUG-03 — drive the cross-thread STW JIT root scan for a multi-threaded GC
/// initiator.
///
/// A thread spinning in JIT-compiled code never cooperatively reaches an
/// interpreter safepoint, so `wait_for_all` would either hang on it or (when
/// it slips through) let the collector mark off its stale `root_snapshot` and
/// reclaim a still-live object → SIGSEGV. This forcibly stops every in-JIT
/// peer at the OS level, conservatively scans its registers + stack into
/// `xt_roots` (pinned, since the frozen peers keep the sweep non-moving), and
/// excludes them from the barrier so the remaining cooperative mutators can
/// be waited for normally. The returned [`TakenOver`] must be `resume`d once
/// the collection has completed.
///
/// When the feature is disabled (`CRATONVM_XT_JIT_ROOT_SCAN=0`) or unsupported
/// for the current heap, this is byte-for-byte the legacy `wait_for_all()` —
/// zero behaviour change.
///
/// Perf/starvation fix (2026-07-13 — elinjsp-socket-read-timeout,
/// stw-crossthread-jit-takeover-hang-cluster, wildfly-standalone-boot-stw-
/// jit-takeover-hang): the per-round `take_over_pass` OS-level scan (Windows:
/// a full `CreateToolhelp32Snapshot` plus per-peer `OpenThread`/
/// `SuspendThread`/`GetThreadContext`/`ResumeThread`; Linux: a signal-and-wait
/// per peer) is expensive, and suspending/resuming every peer thread —
/// including the exact mutator this loop is waiting for — competes with that
/// peer for scheduler time. Previously this ran unconditionally on every 1ms
/// tick once the loop had spun even once (`rounds != 0`), regardless of
/// whether anything was actually in JIT, for as long as the wait continued —
/// a self-amplifying livelock where the longer the wait takes, the more it
/// starves the very thread it is waiting for. Confirmed empirically: a
/// single JSP-compile GC pause with exactly one pending (non-JIT,
/// non-blocked) mutator took several minutes, logging hundreds of thousands
/// of "0 newly taken over" scans, one per millisecond, before the mutator
/// (itself just slow to reach its own next safepoint under the induced
/// scheduling pressure) finally arrived.
pub(super) fn stw_takeover_should_scan(rounds: u32, jit_hint: bool) -> bool {
    // Scan every round for the first FAST_SCAN_ROUNDS — preserves
    // zero-added-latency behavior for the common case, where a genuinely
    // in-JIT peer is taken over within single-digit milliseconds — then back
    // off geometrically. A peer that enters JIT during the slow phase is
    // still guaranteed to be found; it just carries up to one
    // SLOW_SCAN_PERIOD/VERY_SLOW_SCAN_PERIOD round of added detection
    // latency, negligible next to a stall already long enough to reach that
    // phase, while cutting steady-state OS-call volume (and the
    // peer-starvation feedback loop) by 1-2 orders of magnitude.
    //
    // Round 0 is deliberately left gated on the cheap `any_thread_in_jit()`
    // hint alone (unchanged from before), so an ordinary, fully-cooperative
    // GC pause that never needed a scan at all still does not pay for one.
    // The hint is NOT used to gate rounds >= 1: that counter is a single
    // process-global depth and cannot distinguish "a peer is in JIT" from "I
    // am" — this function's caller is commonly reached via `maybe_gc` called
    // from JIT-compiled code, so the initiator's own live JIT-entry guard can
    // hold the hint permanently true regardless of any peer's actual state.
    const FAST_SCAN_ROUNDS: u32 = 20;
    const SLOW_SCAN_PERIOD: u32 = 20;
    const VERY_SLOW_SCAN_ROUNDS: u32 = 500;
    const VERY_SLOW_SCAN_PERIOD: u32 = 200;
    if rounds == 0 {
        jit_hint
    } else if rounds < FAST_SCAN_ROUNDS {
        true
    } else if rounds < VERY_SLOW_SCAN_ROUNDS {
        rounds % SLOW_SCAN_PERIOD == 0
    } else {
        rounds % VERY_SLOW_SCAN_PERIOD == 0
    }
}

/// Nonzero for the duration of a [`stw_publish_frame_traces`] pause; read by
/// `safepoint_check` on the arriving side.
///
/// Process-global rather than per-thread because the request is scoped to one
/// stop-the-world, and only one of those runs at a time — a second requester
/// finds the request already taken and gives up. A per-thread flag would buy
/// nothing and cost a registry lookup at every safepoint park.
///
/// A COUNT of requesters between raise and drop, not a bool (gc-common w4-a,
/// `handoff-w3a-frame-trace-wanted-counter`): with a bool, two concurrent
/// cross-thread `Thread.getStackTrace()` callers raced — the LOSER's request
/// was declined, its guard stored `false` while the WINNER's pause was still
/// collecting, and every mutator that parked after that store published no
/// trace, so the winner read a stale stack for it. A loser now removes only
/// its own count.
static FRAME_TRACE_WANTED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Is a cross-thread stack dump in flight? See [`stw_publish_frame_traces`].
#[inline]
pub(crate) fn frame_trace_wanted() -> bool {
    FRAME_TRACE_WANTED.load(std::sync::atomic::Ordering::Relaxed) != 0
}

/// One requester's raise of [`FRAME_TRACE_WANTED`], lowered by `Drop` on every
/// path out (including a declined request and an unwind).
struct FrameTraceWanted;

impl FrameTraceWanted {
    fn raise() -> Self {
        FRAME_TRACE_WANTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        FrameTraceWanted
    }
}

impl Drop for FrameTraceWanted {
    fn drop(&mut self) {
        FRAME_TRACE_WANTED.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Request a stop-the-world pause that COLLECTS NOTHING — a frame-trace dump,
/// a non-moving heap walk, a concurrent-mark initial mark / remark — with the
/// same identity census a collection uses. `Some(counted_os_tids)` (the
/// counted threads' OS ids, for `stw_take_over_and_wait`'s identity-based
/// excusal) when this thread won the request; `None` when another pause was
/// already requested.
///
/// gc-common w4-a (`handoff-w3a-non-collection-pauses-pass-the-initiator-census`,
/// `common-a-initiator-assumed-in-census`): these pauses used
/// `request_stw_counted_with_live_blocked`, which cannot carry
/// `initiator_counted` and so always took the legacy `alive - 1 - blocked`: an
/// initiator the census did not count removed a real mutator from the quota.
/// They now pass the census's answer exactly as `run_collection_pause` does.
/// Two more differences, both improvements: a request made while a pause is
/// already in flight returns at once WITHOUT walking the registry under the
/// barrier lock, and no coverage cycle is opened (a non-collection pause never
/// opened one; the request core still resets the peer JIT ledger).
fn request_non_collection_pause(shared: &SharedVm, initiator: crate::ThreadId) -> Option<Vec<u32>> {
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let won = shared.mem.gc_barrier.request_stw_opening_cycle(
        initiator,
        || {
            let (n, blocked, tids, blocked_tids, initiator_counted) = shared
                .threads
                .thread_registry
                .alive_count_blocked_and_os_tids_for(initiator);
            counted_os_tids = tids;
            crate::threading::gc_barrier::StwCensus {
                alive: u32::try_from(n).unwrap_or(u32::MAX),
                blocked: u32::try_from(blocked).unwrap_or(u32::MAX),
                blocked_tids,
                initiator_counted,
            }
        },
        // No coverage cycle: see above.
        || {},
    );
    if won {
        // Round 12 wave 1 (lock W19-1): a thread in the opt-in running monitor
        // park is still a counted mutator; wake it so it arrives through the
        // GC-blocked park instead of holding this pause up for its budget.
        shared.threads.monitors.wake_lazy_parkers();
    }
    won.then_some(counted_os_tids)
}

/// Take a stop-the-world pause whose only purpose is to make every mutator
/// publish its live call stack into `JvmThread::frame_trace`, so a cross-thread
/// `Thread.getStackTrace()` / `Thread.dumpThreads()` can read where the target
/// thread ACTUALLY is.
///
/// # Why a safepoint is needed at all
///
/// `frame_trace` is deposited at the blocking deposit points, which is exactly
/// right for a parked thread and says nothing about a running one. A thread that
/// has never blocked has published nothing (cross-thread `getStackTrace()`
/// returned a zero-length array — measured against HotSpot on a spin loop:
/// HotSpot named the running method on every one of ~840 samples, this VM
/// returned `<empty>` on 1471 of 1495 and the real method on none), and a thread
/// that HAS blocked publishes where it blocked last, which is worse than
/// nothing: it is a confident wrong answer. Another thread cannot walk a running
/// thread's frames from outside — `JvmThread::frames` is owned by its own
/// thread — so the only way to get a truthful stack is to ask that thread to
/// publish one at a point where it is not mid-instruction. That is what a
/// safepoint is.
///
/// This is HotSpot's answer too; it uses a per-thread handshake rather than a
/// global pause, which is cheaper but needs a per-thread poll word this VM's
/// JIT does not emit (`emit_safepoint_poll` tests exactly one byte, the GC
/// barrier's `stw_requested`, and widening that test costs every back-edge in
/// every compiled method). Reusing the existing pause keeps the change off the
/// hot path entirely, at the cost of making a thread dump as expensive as a GC
/// pause. Callers should skip it when the target is parked — see
/// `NativeContextImpl::thread_stack_trace`, which does.
///
/// Returns `false` when no pause was taken (another STW already owns the
/// world); the caller then reads whatever was last deposited, i.e. the
/// behaviour that predates this function.
pub(crate) fn stw_publish_frame_traces(shared: &SharedVm, initiator: crate::ThreadId) -> bool {
    // Raised BEFORE the request, and lowered by `Drop` on every path out.
    //
    // A guard rather than two explicit decrements because the failure mode of
    // missing one is silent and permanent: the count left raised makes EVERY
    // safepoint park — i.e. every mutator on every GC pause, forever after —
    // capture and allocate a frame trace nobody asked for. That is a cost with
    // no symptom, which is the kind this codebase keeps finding late.
    //
    // Before the request, not after: a mutator that reaches its poll in between
    // would otherwise park without publishing, and this pause gets no second
    // chance to ask it.
    let _wanted = FrameTraceWanted::raise();

    // A handshake pause (`NonMovingPause::request_handshake`): the take-over
    // may freeze a peer only after the cooperative slice
    // [`FRAME_TRACE_GRACE`], because a peer frozen in compiled code passes no
    // poll, never runs `safepoint_check`, and so publishes nothing — the
    // reader then gets whatever was last deposited. Until interpreter round i1
    // wave 14 this pause ran `stw_take_over_and_wait` at once, whose round 0
    // froze every peer then in compiled code, including one that WOULD have
    // polled a few microseconds later (inside a bulk intrinsic, a long
    // straight-line body). A peer that never polls (`CRATONVM_JIT_SAFEPOINT_POLLS=0`,
    // an intrinsic spin) is still frozen after the slice, so the pause stays
    // bounded; its trace stays as last deposited, the pre-existing answer.
    // When every mutator arrives in the slice no take-over pass runs at all.
    //
    // The take-over is freeze-only (`NoRootsPause`, inside `request_as`): this
    // pause collects nothing, so the conservative scan of frozen peers and the
    // helper-window pass would be computed and thrown away. `door=frame-trace`
    // on the `--verbose:gc` pause line (gc-common w5-a), sealed and printed by
    // the pause's `Drop`.
    let Some(mut pause) = NonMovingPause::request_handshake(
        shared,
        initiator,
        NonCollectionPause::FrameTrace,
        FRAME_TRACE_GRACE,
    ) else {
        return false;
    };
    // Nothing to do in the pause itself: the work is what the ARRIVING threads
    // did on their way in. Nothing moves, so there is no pointer map.
    //
    // A take-over PUBLISHES skip spans -- `stw_take_over_and_wait` does so
    // unconditionally, for every alive thread's reserved tail -- so the pause
    // has to retire them like any other; the pause's `Drop` does. It once did
    // not, and nothing here moves or sweeps, so the leak was invisible from
    // this function: the set simply outlived the pause and waited for a
    // collection that does not republish.
    //
    // `CRATONVM_GC_NO_FRAME_TRACE_SPAN_RETIRE=1` restores that leak in one
    // binary, which is the only way to show the repair is not vacuous: the
    // symptom needs a stale span to be CONSUMED, and the consumer is a later
    // collection on a path that does not republish, not anything this function
    // does.
    pause.keep_skip_spans =
        cratonvm_types::flags::runtime_var_os("CRATONVM_GC_NO_FRAME_TRACE_SPAN_RETIRE").is_some();
    drop(pause);
    true
}

/// The cooperative slice a frame-trace pause waits for every mutator to reach
/// a poll before its take-over may freeze a peer still in compiled code (see
/// [`stw_publish_frame_traces`]). The loop-exit pause's first slice
/// (`jvmti_events::LOOP_EXIT_GRACE`): every compiled loop polls at each back
/// edge and every compiled method at entry, so a few milliseconds bound the
/// added latency of a dump whose target never polls, while a target that does
/// arrives in microseconds and skips the take-over pass.
const FRAME_TRACE_GRACE: std::time::Duration = std::time::Duration::from_millis(2);

/// The cooperative slice an inline-cache grace request waits before it
/// freezes a missing peer (JIT round 13 wave 11 lane mega9's patch, applied in
/// round 14 wave 1 by lane codecache).
const IC_GRACE_SLICE: std::time::Duration = std::time::Duration::from_micros(500);

/// `r13w11-mega9-ic-grace-handshake-patch-FIXED-20260929.md`: a handshake pause
/// whose only purpose is its cooperative arm's
/// `cratonvm_jit::note_grace_at_cooperative_stop`, requested by an
/// inline-cache writer that found only ungraced retired ways
/// (`CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS`, default off). Returns whether a pause
/// was taken (`false`: another stop owns the world, which graces the same way
/// if it is cooperative). A frozen peer graces nothing, as in every pause; the
/// slice is what lets a spinning peer reach its poll.
pub(crate) fn request_ic_grace_handshake(shared: &SharedVm, initiator: crate::ThreadId) -> bool {
    let Some(pause) = NonMovingPause::request_handshake(
        shared,
        initiator,
        NonCollectionPause::IcGrace,
        IC_GRACE_SLICE,
    ) else {
        return false;
    };
    drop(pause);
    true
}

/// A stop-the-world pause in which NOTHING moves and nothing is collected: a
/// heap dump, or any other walk that needs the mutators stopped and the heap
/// quiescent. RAII -- `Drop` retires the skip spans, resumes frozen peers and
/// reopens the world with an empty pointer map, on every path out including an
/// unwind.
///
/// gc-common w2-c (2026-09-23), `common-a-hprof-dump-plain-wait-for-all`: the
/// HPROF dump used a plain `wait_for_all()`, so a peer spinning in compiled code
/// without a poll (`CRATONVM_JIT_SAFEPOINT_POLLS=0`, an intrinsic loop, a method
/// compiled before the poll byte existed) hung `-XX:+HeapDumpOnOutOfMemoryError`
/// forever with every other mutator parked. This takes the same wait as every
/// collection, freeze-only (the [`NoRootsPause`] scope `stw_publish_frame_traces`
/// uses): a frozen peer's roots are not needed because nothing is marked, and
/// nothing moves so no frozen register can go stale.
pub(crate) struct NonMovingPause<'a> {
    shared: &'a SharedVm,
    taken: Option<crate::jit::xt_root_scan::TakenOver>,
    /// gc-common w5-a: the `--verbose:gc` pause line (`door=heap-dump`, or
    /// the kind given to [`Self::request_handshake`]); sealed and printed by
    /// `Drop`. `None` without `--verbose:gc`.
    pause_timer: Option<super::gc_events::NonCollectionPauseTimer>,
    /// Resume the frozen peers WITHOUT retiring the skip spans the take-over
    /// published: the `CRATONVM_GC_NO_FRAME_TRACE_SPAN_RETIRE=1` lever of
    /// [`stw_publish_frame_traces`], which restores that leak in one binary.
    /// `false` everywhere else.
    keep_skip_spans: bool,
}

impl<'a> NonMovingPause<'a> {
    /// Request the pause and wait (with the take-over) until it holds. `None`
    /// when another stop-the-world already owns the world -- the caller decides
    /// whether to proceed unpaused.
    pub(crate) fn request(shared: &'a SharedVm, initiator: crate::ThreadId) -> Option<Self> {
        Self::request_as(shared, initiator, NonCollectionPause::HeapDump, None)
    }

    /// A handshake-style pause: every mutator is to pass a poll, and a peer
    /// frozen in compiled code before reaching one defeats the purpose. So the
    /// take-over waits `grace` first, cooperatively
    /// ([`cooperative_grace_wait`]); only peers still missing after it are
    /// frozen, which keeps the pause bounded for a loop compiled without polls
    /// (`CRATONVM_JIT_SAFEPOINT_POLLS=0`) or an intrinsic spin. When all arrive
    /// in the grace slice no take-over pass runs at all. `kind` names the
    /// pause on the `--verbose:gc` line.
    ///
    /// Interpreter round i1 wave 13, lane L4
    /// (`interpreter-L4-loop-exit-pause-borrows-the-heap-dump-pause-FIXED-20260925.md`):
    /// the loop-exit arming borrowed [`Self::request`], whose round-0
    /// take-over freezes an OSR'd loop still inside a compiled callee, which
    /// then never sees its back-edge poll's slow path for that pause. Wave 14,
    /// lane L3: [`stw_publish_frame_traces`] is one too (a frozen peer
    /// publishes no stack trace).
    pub(super) fn request_handshake(
        shared: &'a SharedVm,
        initiator: crate::ThreadId,
        kind: NonCollectionPause,
        grace: std::time::Duration,
    ) -> Option<Self> {
        Self::request_as(shared, initiator, kind, Some(grace))
    }

    fn request_as(
        shared: &'a SharedVm,
        initiator: crate::ThreadId,
        kind: NonCollectionPause,
        grace: Option<std::time::Duration>,
    ) -> Option<Self> {
        let counted_os_tids = request_non_collection_pause(shared, initiator)?;
        // The guard exists BEFORE the wait, so an unwind out of the take-over
        // still reopens the world.
        let mut pause = NonMovingPause {
            shared,
            taken: None,
            pause_timer: None,
            keep_skip_spans: false,
        };
        let arrived = grace.is_some_and(|g| cooperative_grace_wait(&shared.mem.gc_barrier, g));
        let taken = if arrived {
            // Every counted mutator is parked at a poll: nothing to freeze and
            // nothing published. Zero this pause's take-over coverage, which
            // the pause line reads and `stw_take_over_and_wait` would have
            // reset at its top.
            cratonvm_gc::gc_quiescence::reset_xt_cycle();
            // JIT round 13 wave 8 (`r13w8-mega7-grace-at-handshake-pause-patch`):
            // nobody is between an inline-cache compare and its entry load
            // here either, so the megamorphic grace may advance -- the
            // collection's own take-over almost always freezes a spinning
            // compiled peer and skips it.
            cratonvm_jit::note_grace_at_cooperative_stop();
            crate::jit::xt_root_scan::TakenOver::default()
        } else {
            let mut xt_roots: Vec<ObjectRef> = Vec::new();
            let _no_roots = NoRootsPause::enter();
            stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids)
        };
        pause.taken = Some(taken);
        pause.pause_timer = non_collection_pause_start(shared, kind);
        Some(pause)
    }

    /// Did this pause freeze a peer in compiled code (a peer that therefore
    /// passed no poll during it)?
    pub(super) fn froze_peers(&self) -> bool {
        self.taken.as_ref().is_some_and(|t| t.count() > 0)
    }

    /// Reserved-but-unallocated TLAB tails `[start, end)` of every alive thread
    /// at this pause -- chiefly the frozen peers', which never retired. A heap
    /// walk must not report anything inside one as an object.
    pub(crate) fn reserved_tlab_tails(&self) -> Vec<(usize, usize)> {
        self.shared
            .threads
            .thread_registry
            .collect_reserved_tlab_tails()
    }
}

impl Drop for NonMovingPause<'_> {
    fn drop(&mut self) {
        non_collection_pause_seal(&mut self.pause_timer);
        match self.taken.take() {
            Some(taken) if self.keep_skip_spans => crate::jit::xt_root_scan::resume(taken),
            Some(taken) => retire_skip_spans_and_resume(self.shared, taken),
            // Unwound out of the take-over: it may have published spans.
            None => self.shared.mem.heap.clear_jit_tlab_skip_regions(),
        }
        self.shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        // Not while unwinding: a panic in the walk has its own report, and a
        // write to stderr is one more thing that could fail mid-unwind.
        if !std::thread::panicking() {
            non_collection_pause_finish(self.pause_timer.take());
        }
    }
}

/// The cooperative half of [`NonMovingPause::request_handshake`]: wait up to
/// `grace` for every counted mutator to arrive at a poll on its own. `true`
/// when the quota was met. Loops because one bounded wait may wake early (a
/// spurious condvar wake, or on Windows a stale arrival signal from an earlier
/// pause).
fn cooperative_grace_wait(
    barrier: &crate::threading::GcBarrier,
    grace: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + grace;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if barrier.wait_for_all_timeout(left) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
    }
}

/// Interpreter round i1 wave 13, lane L4: the grace slice of the handshake
/// pause.
#[cfg(test)]
mod i13_l4_handshake_pause_tests {
    use super::cooperative_grace_wait;
    use crate::threading::{GcBarrier, ThreadId};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A peer that reaches its poll inside the grace slice meets the quota
    /// cooperatively, and the wait returns on its arrival, not at the end of
    /// the slice.
    #[test]
    fn a_peer_arriving_inside_the_grace_slice_meets_the_quota() {
        let barrier = Arc::new(GcBarrier::new());
        assert!(barrier.request_stw(ThreadId(0), 2));
        let b = barrier.clone();
        let peer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            b.arrive_and_wait(ThreadId(1))
        });
        let t0 = Instant::now();
        assert!(cooperative_grace_wait(&barrier, Duration::from_secs(30)));
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "the wait ends at the arrival, not at the end of the slice"
        );
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = peer.join();
    }

    /// A peer that never arrives (a loop compiled without polls) is waited
    /// for the whole slice and no longer: the take-over then freezes it.
    #[test]
    fn a_peer_that_never_arrives_ends_the_slice_unmet() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 2));
        let t0 = Instant::now();
        assert!(!cooperative_grace_wait(&barrier, Duration::from_millis(5)));
        assert!(t0.elapsed() >= Duration::from_millis(5), "the whole slice");
        // A zero slice asks once.
        assert!(!cooperative_grace_wait(&barrier, Duration::ZERO));
        barrier.reduce_expected(1);
        assert!(cooperative_grace_wait(&barrier, Duration::ZERO));
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }
}

#[cfg(test)]
mod frame_trace_request_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// The three tests below read and raise the one process-global counter;
    /// serialised so one's raise is never another's "rests low" failure.
    static FRAME_TRACE_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// The request flag must read `false` when nobody is dumping.
    ///
    /// This is the whole cost argument for gating the publish in
    /// `safepoint_check`: raised, every mutator captures and allocates a frame
    /// trace on every GC pause. A flag that latched on would not fail any test
    /// — it would just quietly make every pause more expensive — so assert the
    /// resting state explicitly.
    #[test]
    fn the_request_flag_rests_low() {
        let _serial = FRAME_TRACE_TEST_LOCK.lock();
        assert!(
            !frame_trace_wanted(),
            "FRAME_TRACE_WANTED must rest low; raised, every safepoint park pays \
             for a frame-trace capture nobody asked for"
        );
    }

    /// And it must come back down when the pause ends — including the path
    /// where no pause was taken.
    ///
    /// `stw_publish_frame_traces` needs a live `SharedVm` and cannot run here,
    /// so this exercises the guard it uses (`FrameTraceWanted`) rather than the
    /// function around it. Breaking `Drop` (or replacing the guard with a
    /// hand-written decrement that an early `return` can skip) fails this.
    #[test]
    fn the_request_guard_lowers_the_flag_on_every_path() {
        let _serial = FRAME_TRACE_TEST_LOCK.lock();
        fn take(early_out: bool) -> bool {
            let _wanted = FrameTraceWanted::raise();
            assert!(
                frame_trace_wanted(),
                "the flag must be observable while raised"
            );
            if early_out {
                return false;
            }
            true
        }
        for early_out in [true, false] {
            let _ = take(early_out);
            assert!(
                !frame_trace_wanted(),
                "the flag stayed raised after the early_out={early_out} path"
            );
        }
        assert_eq!(FRAME_TRACE_WANTED.load(Ordering::Relaxed), 0);
    }

    /// gc-common w4-a (`handoff-w3a-frame-trace-wanted-counter`): two
    /// concurrent requesters. The LOSER (its request declined) drops its raise
    /// while the WINNER's pause is still collecting traces; the flag must stay
    /// raised until the winner drops too. With the old bool the loser's guard
    /// stored `false` here and every mutator parking afterwards published no
    /// trace.
    #[test]
    fn a_losing_requester_does_not_lower_the_winners_flag() {
        let _serial = FRAME_TRACE_TEST_LOCK.lock();
        let winner = FrameTraceWanted::raise();
        let loser = FrameTraceWanted::raise();
        drop(loser);
        assert!(
            frame_trace_wanted(),
            "the loser's drop lowered the flag under the winner's pause"
        );
        drop(winner);
        assert!(!frame_trace_wanted());
    }

    /// Interpreter round i1 wave 14, lane L3: the frame-trace pause is a
    /// handshake pause. With every counted mutator arriving inside the grace
    /// slice (here: none to wait for) it runs no take-over, reopens the world
    /// through the pause's `Drop` (a second dump is granted), and lowers the
    /// request flag.
    #[test]
    fn a_frame_trace_pause_every_mutator_reaches_reopens_the_world() {
        let _serial = FRAME_TRACE_TEST_LOCK.lock();
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let outsider = crate::ThreadId(u64::MAX);
        assert!(stw_publish_frame_traces(&shared, outsider));
        assert!(!frame_trace_wanted());
        assert!(
            stw_publish_frame_traces(&shared, outsider),
            "the first pause left the world stopped"
        );
        assert!(!frame_trace_wanted());
        // Frozen by nobody: the handshake arm the grace slice decides.
        let pause = NonMovingPause::request_handshake(
            &shared,
            outsider,
            NonCollectionPause::FrameTrace,
            FRAME_TRACE_GRACE,
        )
        .expect("no other pause owns the world");
        assert!(!pause.froze_peers());
    }

    /// The no-roots scope is thread-local, rests OFF, and is lowered on every
    /// path out -- including an unwind.
    ///
    /// Its failure mode is the dangerous one: a take-over that believes it
    /// needs no roots, run for a real COLLECTION, would sweep every object only
    /// a frozen or blocked peer names. So the resting state and the scoping are
    /// asserted rather than assumed.
    #[test]
    fn the_no_roots_scope_is_scoped_to_one_thread_and_one_call() {
        assert!(takeover_gathers_roots(), "a take-over gathers roots by default");
        {
            let _g = NoRootsPause::enter();
            assert!(!takeover_gathers_roots());
            // Another thread -- another initiator -- is unaffected.
            let other = std::thread::spawn(takeover_gathers_roots).join().unwrap();
            assert!(other, "the scope must not leak to another thread");
        }
        assert!(takeover_gathers_roots(), "lowered when the scope ends");
        let unwound = std::panic::catch_unwind(|| {
            let _g = NoRootsPause::enter();
            panic!("unwind through the scope");
        });
        assert!(unwound.is_err());
        assert!(takeover_gathers_roots(), "lowered on unwind too");
    }
}

thread_local! {
    /// Raised by [`stw_publish_frame_traces`] around its own
    /// `stw_take_over_and_wait` call: the pause it is inside collects nothing,
    /// so the take-over need only FREEZE in-JIT peers, not gather roots.
    ///
    /// Thread-local and not process-global on purpose. The pause's initiator is
    /// the only thread that runs `stw_take_over_and_wait` for it, and a global
    /// would be visible to a COLLECTION's initiator on another thread -- the
    /// way `FRAME_TRACE_WANTED` is raised before `request_stw` and so can be
    /// up while somebody else's GC owns the world. A collection that believed
    /// it needed no roots would sweep live objects.
    static NO_ROOTS_PAUSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII scope for [`NO_ROOTS_PAUSE`]; lowers it on every path out.
struct NoRootsPause;

impl NoRootsPause {
    fn enter() -> Self {
        NO_ROOTS_PAUSE.with(|c| c.set(true));
        NoRootsPause
    }
}

impl Drop for NoRootsPause {
    fn drop(&mut self) {
        let _ = NO_ROOTS_PAUSE.try_with(|c| c.set(false));
    }
}

/// Does the take-over this thread is about to run feed a collection?
///
/// `false` only inside a [`NonMovingPause`]'s take-over, which is also
/// [`stw_publish_frame_traces`]' pause (gc-common w1-c, 2026-09-23; a
/// `NonMovingPause` since interpreter round i1 wave 14). Before that, a
/// cross-thread `Thread.getStackTrace()` paid the
/// whole root-gathering cost of a collection: an OS suspend, a `VirtualQuery`
/// and a copy of the used stack of EVERY blocked thread holding JIT frames
/// (Linux: a signal round-trip each plus a `/proc/self/maps` read), and a
/// probe of every copied word -- all of it discarded, because the pause has no
/// pointer map and no mark phase.
fn takeover_gathers_roots() -> bool {
    !NO_ROOTS_PAUSE.with(std::cell::Cell::get)
}

/// End a takeover: retire the published TLAB skip spans, then resume the
/// frozen peers they describe.
///
/// **These are one operation, and they were spelled as two adjacent lines at
/// eight call sites.** `stw_take_over_and_wait` is the only publisher, and it
/// publishes unconditionally; the spans describe reserved TLAB tails belonging
/// to the very threads `taken` is about to release. The instant those threads
/// resume they bump-allocate into those tails, so a span that outlives its
/// `resume` no longer describes un-allocated memory -- it covers LIVE objects,
/// and every sweep walk skips it while `mark_young`'s anchor oracle answers
/// "free/gap space, not an object" for any root pointing into it. That is the
/// `ObjectCleanerTest` use-after-free, and it is why the publish was made
/// unconditional.
///
/// The unconditional publish makes a missed clear survivable on the paths that
/// republish, which is exactly what let a NINTH exit hide: of the nine callers
/// of `stw_take_over_and_wait`, eight cleared and `stw_publish_frame_traces`
/// -- reachable from ordinary Java, via `Thread.getStackTrace` -- did not. It
/// published every alive thread's reserved tail and left the set behind. Any
/// later collection that does not republish then reads it, and before
/// gc-common w2-a `maybe_gc`'s single-threaded fast path was precisely such a
/// path: it swept without ever calling `stw_take_over_and_wait` (every door now
/// runs `run_collection_pause`, which always does).
///
/// So the pairing is a function, not a convention. A tenth exit gets it right
/// by construction, and `xt_root_scan::resume` should not be called directly
/// from a GC path.
///
/// ORDER, and it is load-bearing: clear BEFORE resume, and both before
/// `complete_gc` reopens the world. A mutator released early could otherwise
/// win the next STW, re-freeze the still-suspended peers, publish fresh spans,
/// and have this initiator's late clear wipe them -- handing the next sweep
/// the frozen peers' reserved tails to walk and free-list.
fn retire_skip_spans_and_resume(shared: &SharedVm, taken: crate::jit::xt_root_scan::TakenOver) {
    shared.mem.heap.clear_jit_tlab_skip_regions();
    crate::jit::xt_root_scan::resume(taken);
}

/// One take-over pass over `live_tids`, timed into the pause's `xt_pass_us`.
/// Split out of [`stw_take_over_and_wait`]'s loop (gce e2/t) so the
/// first-pass grace can run the same pass over the uncounted roster.
fn run_takeover_pass(
    shared: &SharedVm,
    gathers_roots: bool,
    taken: &mut crate::jit::xt_root_scan::TakenOver,
    xt_roots: &mut Vec<ObjectRef>,
    live_tids: &[u32],
) -> usize {
    use crate::jit::xt_root_scan as xt;
    // gc-common w6-f: the passes' share of `ttsp_us` (`xt_pass_us=` on
    // the pause line) -- one `Instant` pair per pass that runs.
    let pass_t0 = std::time::Instant::now();
    let n = if gathers_roots {
        // Exact bases, widened to derived pointers by
        // `CRATONVM_XT_TAKEOVER_INTERIOR` (default ON since w2-c; `=0`
        // restores the exact-base probe); see `takeover_word_probe`.
        // `companions` collects the object ENDING at an accepted word
        // -- a one-past-the-end cursor, which the probe answers as the
        // next object or (Generational/G1) the address itself
        // (`takeover_word_companion`). A side vector because the pass
        // takes one answer per word.
        let companions = std::cell::RefCell::new(Vec::<ObjectRef>::new());
        let n = xt::take_over_pass(
            taken,
            &|a| {
                let r = xt::takeover_word_probe(&shared.mem.heap, a);
                if let Some(c) = xt::takeover_word_companion(&shared.mem.heap, a, r) {
                    companions.borrow_mut().push(c);
                }
                r
            },
            xt_roots,
            live_tids,
        );
        xt_roots.extend(companions.into_inner());
        n
    } else {
        // Freeze only: every word is declined, so nothing is scanned
        // into a root set this pause would discard anyway.
        xt::take_over_pass(taken, &|_: usize| None::<ObjectRef>, xt_roots, live_tids)
    };
    cratonvm_gc::gc_quiescence::add_xt_pass_ns(
        u64::try_from(pass_t0.elapsed().as_nanos()).unwrap_or(u64::MAX),
    );
    n
}

/// The first-pass grace when `CRATONVM_XT_FIRST_PASS_GRACE_US` is unset or
/// unparsable (gce ve2: time-to-safepoint 3-4x lower, `xt_pass_us` about 3 us,
/// wall time within 10 %).
const DEFAULT_FIRST_PASS_GRACE_US: u64 = 200;

/// `CRATONVM_XT_FIRST_PASS_GRACE_US` (gce e2/t; default 200 us since gce ve2,
/// 2026-09-29; `0` = off; capped at 10 ms): how long [`stw_take_over_and_wait`]
/// waits for the cooperative arrivals before its first take-over pass. Read
/// once per take-over pause.
fn takeover_first_pass_grace() -> std::time::Duration {
    let us = cratonvm_types::flags::runtime_var("CRATONVM_XT_FIRST_PASS_GRACE_US")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_FIRST_PASS_GRACE_US);
    std::time::Duration::from_micros(us.min(10_000))
}

/// The roster threads the pause's census did not count: the only ones a
/// take-over pass still owes a signal once every counted mutator has arrived.
fn uncounted_roster(live_tids: &[u32], counted_os_tids: &[u32]) -> Vec<u32> {
    live_tids
        .iter()
        .copied()
        .filter(|tid| !counted_os_tids.contains(tid))
        .collect()
}

pub(super) fn stw_take_over_and_wait(
    shared: &SharedVm,
    xt_roots: &mut Vec<ObjectRef>,
    counted_os_tids: &[u32],
) -> crate::jit::xt_root_scan::TakenOver {
    use crate::jit::xt_root_scan as xt;
    // Per-cycle cross-thread coverage starts here, so whatever the sweep reads
    // later describes THIS collection rather than whichever one last ran a pass.
    cratonvm_gc::gc_quiescence::reset_xt_cycle();
    // And the helper-window verdict, which the pass itself resets only when it
    // runs: a cycle with no blocked thread skips it and would otherwise read
    // the previous cycle's value.
    xt::reset_helper_window_cycle();
    let gathers_roots = takeover_gathers_roots();
    // Read ONCE per cycle (w2-c; `common-c-takeover-small-residue` row 5): the
    // probe choice below and the discharge decision at the end must agree, and
    // two reads of a flag a thread override can change are two answers.
    let discharge_enabled = xt::helper_window_discharge_enabled();
    // The forcible take-over is only sound on a heap that can collect while a
    // frozen peer holds an un-retired TLAB and un-rewritable roots:
    // Generational degrades to the non-moving sweep that consumes the JIT
    // TLAB skip regions; G1 (INT-3) skips the published tails in every region
    // walker and pins everything a frozen peer can address out of the CSet
    // (see `pin_frozen_peer_roots_for_g1`); ZGC sweeps an allocation-base
    // REGISTRY rather than memory, so an un-retired tail (which holds no
    // registered base) is invisible to the sweep by construction, and its
    // SLIDE consumes the published list — the pages a tail touches leave the
    // relocation set and the bump cursor never drops below a tail's end
    // (`zgc/vm_tlab.rs`). The "its mutators never hold TLABs" half of this
    // argument was retired on 2026-09-02, when `VmHeap::refill_tlab` started
    // serving that backend too; do not reason from it.
    // The `supports_jit_tlab_skip` gate is retained for any future
    // backend that can't make one of those arguments.
    if !xt::enabled() || !shared.mem.heap.supports_jit_tlab_skip() {
        shared.mem.gc_barrier.wait_for_all();
        // Round 13 wave 4 (lane mega3): this wait freezes nobody, so every
        // counted mutator is at a poll or blocked -- none between an
        // inline-cache compare and its entry load.
        cratonvm_jit::note_grace_at_cooperative_stop();
        return xt::TakenOver::default();
    }
    let mut taken = xt::TakenOver::default();
    // Take over currently-in-JIT peers, then wait briefly for the remaining
    // cooperative mutators. If the wait does not complete, scan again: a peer can
    // enter JIT after the previous takeover pass and would otherwise never
    // arrive at the barrier. Keep looping until the barrier is satisfied.
    const WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(1);
    const WARN_AFTER_ROUNDS: u32 = 64;
    // See `stw_takeover_should_scan`'s doc for why the scan cadence backs off
    // instead of running unconditionally on every round.
    let mut rounds = 0u32;
    let mut warned = false;
    // gce e2/t (`CRATONVM_XT_FIRST_PASS_GRACE_US`, default 200 us, `0` = off): give the
    // cooperative arrivals a short grace BEFORE the first pass. Measured on the
    // Windows release binary (`GceE2tTakeoverProbe`, 16 workers in compiled
    // loops, 40 `System.gc()` pauses): every counted mutator arrived at a poll
    // on its own, nobody was frozen, and the round-0 pass alone was 742 of
    // 784 us of time-to-safepoint, because it suspends (Windows) or signals and
    // waits for (Linux) every roster thread, parked or not. When the quota is
    // met inside the grace, every COUNTED mutator is parked at the barrier --
    // the same fact a plain `wait_for_all` pause stands on -- so the one pass
    // still owed is over the roster threads the census did NOT count (a
    // newcomer registered after it), which is usually nobody. Otherwise the
    // loop below runs as before, one grace later.
    let grace = takeover_first_pass_grace();
    let grace_met = !grace.is_zero() && shared.mem.gc_barrier.wait_for_all_timeout(grace);
    if grace_met {
        let (_, live_tids) = shared.threads.thread_registry.alive_count_and_os_tids();
        let uncounted = uncounted_roster(&live_tids, counted_os_tids);
        // Frozen uncounted peers are scanned and resumed with the rest, and
        // excuse nobody: the barrier never expected them. The pass runs even
        // over an empty list, so the sweep's coverage record reads "looked".
        let _ = run_takeover_pass(shared, gathers_roots, &mut taken, xt_roots, &uncounted);
    }
    loop {
        if grace_met {
            break;
        }
        let tids_before = taken.tids.len();
        let should_scan =
            stw_takeover_should_scan(rounds, crate::jit::conservative_roots::any_thread_in_jit());
        let newly = if should_scan {
            // Re-read the roster every round rather than snapshotting it once
            // before the loop: this loop exists precisely because "a peer can
            // enter JIT after the previous takeover pass", and such a peer may
            // also have REGISTERED after it. A stale roster would leave that
            // newcomer unfrozen and unscanned for the rest of the collection —
            // the coverage hole `take_over_pass`'s obligation rules out. The
            // read is a registry lock and a small Vec; the walk it replaced
            // cost ~83ms a pass (see `take_over_pass`).
            let (_, live_tids) = shared.threads.thread_registry.alive_count_and_os_tids();
            run_takeover_pass(shared, gathers_roots, &mut taken, xt_roots, &live_tids)
        } else {
            0
        };
        if newly > 0 {
            // xt-hardening (2026-07-03): identity-based excusal. Only excuse
            // frozen peers that were actually COUNTED in the barrier's
            // `expected` (the OS-tid snapshot is taken atomically with the
            // expected computation, under the same registry lock). Excusing
            // an uncounted newcomer — a thread spawned after the snapshot
            // that reached JIT code during the takeover loop — over-reduces
            // `expected`, releasing the barrier while a genuinely counted
            // mutator still runs: mutation concurrent with mark+sweep. An
            // uncounted frozen peer stays frozen and scanned but excuses
            // nobody (the barrier never expected it).
            let mut excuse = 0u32;
            for &tid in &taken.tids[tids_before..] {
                if counted_os_tids.contains(&tid) {
                    excuse += 1;
                } else {
                    static N: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                        tracing::warn!(
                            os_tid = tid,
                            "xt takeover froze an uncounted newcomer thread — \
                             scanned but not excused from the barrier"
                        );
                    }
                }
            }
            if excuse > 0 {
                shared.mem.gc_barrier.reduce_expected(excuse);
            }
        }
        rounds += 1;
        if shared.mem.gc_barrier.wait_for_all_timeout(WAIT_SLICE) {
            break;
        }
        if !warned && rounds >= WARN_AFTER_ROUNDS {
            let pending = shared.mem.gc_barrier.pending_count();
            tracing::warn!(
                rounds,
                pending,
                taken = taken.count(),
                "STW cross-thread JIT takeover is still waiting for cooperative mutators"
            );
            // GCBARRIER-LIVELOCK-FIX (2026-07-18) tripwire: cross-check the
            // barrier's legacy `threads_blocked` atomic (bumped by any
            // `GcBarrier::enter_blocked()` / `mark_blocked_region_enter()`
            // call) against the registry's authoritative `in_blocked_region`
            // census — the ONLY signal the production `expected` computation
            // (`request_stw_opening_cycle` /
            // `alive_count_blocked_and_os_tids_for`) actually excludes threads
            // on. A caller that reaches `enter_blocked()` without first
            // depositing a root snapshot (`in_blocked_region` stays false)
            // bumps the legacy counter but stays invisible to the census —
            // silently inflating `expected` by one uncounted mutator that can
            // never arrive. This is the exact shape of a livelock fixed at
            // this date in `vm/src/vm/vm_exec.rs`'s thread-termination
            // "notify waiting joiners" block (a `block_enter()` call missing
            // the `deposit_root_snapshot()` its own doc comment requires).
            // Always-on (not gated behind CRATONVM_DBG_STW_CENSUS) because it
            // only runs once takeover is already stuck for 64+ rounds — a
            // rare, already-anomalous path — and a mismatch here is the
            // single fastest signal to root-cause a recurrence of this bug
            // class at any OTHER call site.
            let (census_alive, census_blocked, _tids, _blocked_tids) = shared
                .threads
                .thread_registry
                .alive_count_blocked_and_os_tids();
            let legacy_blocked = shared.mem.gc_barrier.blocked_count() as usize;
            if legacy_blocked > census_blocked {
                eprintln!(
                    "[gcbarrier-tripwire] legacy blocked_count()={legacy_blocked} > \
                     census in_blocked_region count={census_blocked} (alive={census_alive}) \
                     -- a thread called GcBarrier::enter_blocked()/mark_blocked_region_enter() \
                     WITHOUT first depositing a root snapshot, so it is invisible to the \
                     production STW census but still occupies an `expected` slot no arrival \
                     can ever satisfy. Set CRATONVM_DBG_STW_CENSUS=1 for a full per-thread dump.\n{}",
                    shared.threads.thread_registry.debug_thread_census()
                );
            }
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STW_CENSUS").is_some()
                || cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_XT_JIT_ROOT_SCAN").is_some()
            {
                eprintln!(
                    "[stw-census] rounds={rounds} pending={pending} taken={} blocked={} alive={}{}",
                    taken.count(),
                    shared.mem.gc_barrier.blocked_count(),
                    shared.threads.thread_registry.alive_count(),
                    shared.threads.thread_registry.debug_thread_census()
                );
                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STW_NATIVE_RING").is_some() {
                    cratonvm_native_api::native_ring::dump_to_stderr();
                }
            }
            warned = true;
        }
    }
    // Round 13 wave 4 (lane mega3, `r13w2-mega-grace-at-cooperative-stop-patch`):
    // the quota is met and nobody is released yet. Only a stop that froze
    // NOBODY proves no mutator is between an inline-cache compare and its
    // entry load; a frozen peer may be stopped anywhere.
    if taken.count() == 0 {
        cratonvm_jit::note_grace_at_cooperative_stop();
    }
    // gc-common w4-a (`handoff-w3f-takeover-phase-timing`): everything from
    // here to the return is post-quota work -- the frozen-peer frame walk, the
    // helper-window pass, the skip-span publish -- which the pause line's
    // `ttsp_us` (ends at quota met) and `collect_us` (starts after this
    // returns) both miss. Published once, at the end, as `xt_post_quota_us`.
    let quota_met_at = std::time::Instant::now();
    // XT-FRAME-SCAN fix (2026-07-22, stw-residual-close): a frozen in-JIT
    // peer's interpreter frames live in Rust Vecs (`JvmThread::frames`) —
    // invisible to the conservative register/native-stack scan — and its
    // deposited `root_snapshot` is only as fresh as its last publish
    // (safepoint arrival, block-enter). Any ref pushed onto an interpreter
    // operand stack (or stored into a local) after that publish and before
    // entering compiled code was therefore missing from the mark roots for
    // the whole frozen window, and the non-moving sweep zeroed the still-live
    // object in place (WildFly parallel-extension-add: fresh
    // StringBuilder/Reader receivers reading back all-zero — see
    // wildfly-interpreter-operand-stack-slot-stale-after-nested-alloc-FIXED.md).
    // Walk each frozen peer's interpreter frames directly into `xt_roots`.
    //
    // SAFETY: each address was published by its owning thread with the
    // `tlab_addr` discipline (stable for the thread's life, cleared before
    // the `JvmThread` drops), and the peer is OS-frozen with `Rip` inside
    // registered compiled code (`take_over_pass` freezes nothing else).
    // Compiled code never mutates the `JvmThread`'s interpreter state — the
    // code paths that do (interpreter, JIT helpers) put `Rip` outside every
    // registered range — and frozen peers are resumed only after the
    // collection completes (`xt_root_scan::resume`), so the reference is
    // dropped before any mutation can resume. A frozen thread also cannot
    // exit, so the entry stays alive and the `JvmThread` cannot drop.
    // Contributed roots go into `xt_roots`, which already forces the
    // non-moving sweep (Generational) / region pinning (G1) for this cycle,
    // so a stale or dead value can only over-retain — never relocate or
    // corrupt.
    if taken.count() > 0 && gathers_roots {
        let peer_addrs = shared
            .threads
            .thread_registry
            .frozen_peer_thread_addrs(&taken.tids);
        let contributed_from = xt_roots.len();
        for addr in peer_addrs {
            // SAFETY: see the block comment above.
            let peer: &crate::threading::jvm_thread::JvmThread =
                unsafe { &*(addr as *const crate::threading::jvm_thread::JvmThread) };
            for frame in peer.frames.iter() {
                frame.scan_local_objects(xt_roots, &shared.mem.heap);
                let sb = xt_roots.len();
                frame.stack.scan_object_refs_ruled(
                    xt_roots,
                    &shared.mem.heap,
                    shared.config.is_jdk_only(),
                );
                if xt_roots.len() > sb {
                    // Operand-stack candidates are screened exactly like the
                    // deposit path (`scan_frame_roots`) — `is_heap_addr`, so a
                    // pointer-shaped primitive long cannot become a root while
                    // a young / mid-init object still can. The strict
                    // `is_object_address` probe used here until 2026-08-04
                    // dropped the second population, and this peer is FROZEN:
                    // whatever its frames hold, only this scan can speak for.
                    let added = xt_roots.split_off(sb);
                    for o in added {
                        if shared.mem.heap.is_heap_addr(o.as_ptr() as usize).is_some() {
                            xt_roots.push(o);
                        }
                    }
                }
                if let Some(m) = frame.monitor_on_exit {
                    xt_roots.push(m);
                }
                // Block monitors (`Frame::held_monitors`, wave 23 lane L7).
                xt_roots.extend_from_slice(frame.held_monitors.as_slice());
            }
            // Every JvmThread FIELD root, from the one list the three other
            // per-thread root paths use (`roots::push_off_frame_thread_roots`).
            // A frozen peer's deposited snapshot can be stale for exactly the
            // fields a JIT helper touches without republishing
            // (TOMCAT-JNDIREALM-JIT.3 was the case cache). gc-common w5-a,
            // `handoff-w4b-takeover-publishes-the-one-thread-field-list`: this
            // block used to spell out its own partial copy (pins, pending
            // return, both memo caches) and missed `native_alloc_pool`,
            // `handle_slots`, the print buffer, scoped values, both parked
            // throwables, the thread mirror and the async exception. Only
            // ADDS roots (and G1 pins, via `pin_frozen_peer_roots_for_g1`).
            // No epoch filter on the memo caches: a present entry is a root
            // on every path, this one included.
            crate::memory::roots::push_off_frame_thread_roots(peer, xt_roots);
        }
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_XT_JIT_ROOT_SCAN").is_some() {
            eprintln!(
                "[xt-frame-scan] frozen_peers={} contributed_roots={}",
                taken.count(),
                xt_roots.len() - contributed_from
            );
        }
    }

    // A4 (fork6-fjp) — helper-window coverage. The barrier is satisfied, so
    // every remaining un-scanned root holder is a BLOCKED thread (excluded via
    // `threads_blocked`, covered only by its `deposit_root_snapshot`, which
    // never scans JIT frames). A worker blocked in `join()`/park under
    // JIT-compiled `runWorker`/`doExec` frames that are the sole holder of a
    // forked subtask would otherwise lose it to the non-moving sweep (the
    // Fork6 stale all-zero receivers). Scan each remaining peer's register
    // file + used stack once, contributing roots only when the stack actually
    // carries a JIT return address. Gated to collections where the gap can
    // exist at all: a blocked thread while some thread holds live JIT frames.
    let mut helper_windows = 0usize;
    if gathers_roots
        && xt::helper_window_scan_enabled()
        && shared.mem.gc_barrier.blocked_count() > 0
        && crate::jit::conservative_roots::any_thread_in_jit()
    {
        // xt-hardening follow-up (2026-07-03): scope the pass to threads
        // ACTUALLY in a blocked region (the only gap it exists to close —
        // see helper_window_pass's doc comment). A cooperatively-arrived
        // mutator already published its JIT roots via update_root_snapshot;
        // re-scanning it only widens the conservative-candidate volume that
        // feeds the mark-phase writer, with zero coverage benefit.
        //
        // gcd d2/i (opt-in `CRATONVM_XT_BLOCKED_MONITOR_PROOF`): a peer blocked
        // in the compiled `monitorenter` helper whose blocking deposit PROVED
        // its JIT chain rewritable is credited like a parked peer that proved
        // it at its park, and is not scanned as a helper window. Flag off: the
        // roster is read exactly as before.
        let blocked_os_tids = if cratonvm_gc::gc_quiescence::blocked_monitor_proof_enabled() {
            let (rest, proven) = shared
                .threads
                .thread_registry
                .blocked_os_tids_split_by_monitor_proof();
            xt::credit_proven_blocked_monitor_peers(
                rest,
                &proven,
                cratonvm_gc::gc_quiescence::jit_depth_of_tid,
                cratonvm_gc::gc_quiescence::add_peer_proven_jit_depth,
            )
        } else {
            shared.threads.thread_registry.blocked_os_tids()
        };
        // WHICH PREDICATE. This was the open question on
        // `bug-h2-testcachedqueryresults-zgc-oom-livelock-20260829` (retired to
        // `fixed-suite-bugs/h2-suite-bugs/` 2026-09-08); it is answered, and the
        // answer is the widest of the three arms below.
        //
        // `is_object_address` is `registry.contains(addr)` -- EXACT BASES ONLY.
        // A frozen peer holding a DERIVED pointer (a compiled loop's pointer
        // into an array body) contributes no candidate at all, so its base is
        // never pinned, and that is why a helper window has to refuse the whole
        // collection rather than pin its way out of it.
        //
        // `is_heap_addr` resolves an interior pointer to its base, and is no
        // longer expensive doing it: one backwards bit scan plus one header
        // dereference (`nearest_base_at_or_below`), not the O(live) registry
        // iteration it once was. The cost that HAS to be priced before adopting
        // it is the WIDER conservative root set -- every `long` that happens to
        // land inside a live object's extent becomes a root.
        //
        // An opt-in flag used to take that measurement. It was retired by
        // gc-common w3-g (2026-09-23): the default-on `pin_resolve` arm below
        // shadowed it, so it was reachable only with the pin-resolve kill
        // switch set AND the discharge off, and did nothing on a default run.
        // What survives of it is the rule it encoded: the discharge IMPLIES an
        // interior probe (pinning is only complete when a derived pointer
        // resolves to the base that must not move), so with pin-resolve off
        // and the discharge on, the fallback is `is_heap_addr`, not the
        // exact-base probe.
        let interior = discharge_enabled;
        // `is_heap_addr` is still not permissive enough to PIN with, and the
        // two words it drops are exactly the two a frozen peer's registers
        // hold. Its ZGC arm rejects a MISALIGNED address (a compiled loop's
        // cursor into a `char[]` or `byte[]`) and its extent test is `addr <
        // end`, so a ONE-PAST-THE-END cursor resolves to no base at all.
        // Either leaves an object nothing pins, and relocation then moves it
        // out from under the register naming it -- a use-after-free.
        //
        // This comment used to call that the page-ALIGNED SIGSEGV of
        // `bug-box-unbox-intrinsic-segv-under-relocation-20260902` and explain
        // the alignment as `compact_low_to` zeroing the span it vacates. Both
        // halves were wrong: the address was page-aligned because it was the
        // BASE of a decommitted 2 MiB arena granule
        // (`offset_into_span` 0x0 in every crash) and the access was the
        // slide's own WRITE, not a read. The hazard below stands on its own.
        //
        // `resolve_interior_for_pin` accepts both. Over-approximating is the
        // SAFE direction here and the asymmetry is stark: a false positive
        // costs one page of compaction, a false negative costs a
        // use-after-free.
        //
        // DEFAULT since 2026-09-08, and the reason is that the DISCHARGE is
        // default-on: a cycle whose helper windows are all pinned no longer
        // refuses, so `is_heap_addr`'s two rejections stopped being lost
        // compaction and became an unpinned array under a relocating cycle.
        // The predicate that claims completeness has to be the one that can
        // deliver it. See `xt::helper_window_pin_resolve_enabled`.
        let pin_resolve = xt::helper_window_pin_resolve_enabled();
        let (windows, _roots) = if pin_resolve {
            // gc-common w3-g: the take-over's derived-pointer rule, applied to
            // helper windows too (`common-w2c-helper-window-probe-misses-the-
            // object-ending-at-a-cursor`). A misaligned cursor is retried at its
            // aligned word, and the object ENDING at an accepted word -- a
            // one-past-the-end cursor, answered as the next object or as the
            // word itself -- is rooted AND pinned through `companions`, since a
            // discharged cycle relocates on the strength of this pin set.
            //
            // Companions are derived AFTER the pass, from the roots it
            // COMMITTED: the pass probes every word of every blocked peer's
            // band but keeps a band's candidates only when it holds a JIT
            // frame, so a companion taken inside the probe would also root the
            // neighbours of every pointer-shaped word on a pure-native stack.
            // The probe only records which answers ECHOED their word (an exact
            // base, or the Generational/G1 "in a live region" answer) -- the
            // one case with an object ending at the word to cover.
            let echoed = std::cell::RefCell::new(rustc_hash::FxHashSet::<usize>::default());
            let committed_from = xt_roots.len();
            let r = xt::helper_window_pass(
                &taken,
                &|a| {
                    let r = xt::helper_window_word_probe(&shared.mem.heap, a);
                    if let Some(o) = r {
                        let addr = o.as_ptr() as usize;
                        if addr == a & !7 {
                            echoed.borrow_mut().insert(addr);
                        }
                    }
                    r
                },
                xt_roots,
                &blocked_os_tids,
            );
            let echoed = echoed.into_inner();
            let mut companions: Vec<ObjectRef> = Vec::new();
            if !echoed.is_empty() {
                let mut seen = rustc_hash::FxHashSet::<usize>::default();
                for o in &xt_roots[committed_from..] {
                    let addr = o.as_ptr() as usize;
                    if !echoed.contains(&addr) || !seen.insert(addr) {
                        continue;
                    }
                    if let Some(c) =
                        xt::helper_window_word_companion(&shared.mem.heap, addr, Some(*o))
                    {
                        companions.push(c);
                    }
                }
            }
            if !companions.is_empty() {
                // Pinned unconditionally, including a refusing window's: over-
                // pinning costs one region/page of compaction for one pause,
                // and a refusing window keeps the cycle non-moving anyway.
                let addrs: Vec<usize> = companions.iter().map(|o| o.as_ptr() as usize).collect();
                cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                xt::XT_HELPER_WINDOW_COMPANIONS
                    .fetch_add(companions.len() as u64, std::sync::atomic::Ordering::Relaxed);
                // Into this cycle's `hw_roots` too, so the widening is visible
                // on the `[GC] xt_peer_scan` line and A/B-able against
                // `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=0`.
                cratonvm_gc::gc_quiescence::publish_xt_helper_window(0, companions.len() as u64);
                xt_roots.extend(companions);
            }
            r
        } else if interior {
            xt::helper_window_pass(
                &taken,
                &|a| shared.mem.heap.is_heap_addr(a),
                xt_roots,
                &blocked_os_tids,
            )
        } else {
            xt::helper_window_pass(
                &taken,
                &|a| shared.mem.heap.is_object_address(a),
                xt_roots,
                &blocked_os_tids,
            )
        };
        helper_windows = windows;
    }
    // Publish any reserved TLAB tails still present after the barrier is
    // satisfied. Usually only forcibly-stopped in-JIT peers have one; collecting
    // all live threads also hardens the sweep against a blocked/tearing-down
    // thread that missed its retire before it left the counted mutator set.
    // Cleared by the caller after the collection completes.
    let regions = shared.threads.thread_registry.collect_reserved_tlab_tails();
    // UNCONDITIONAL, and that is the fix, not a tidy-up.
    //
    // This used to publish only `if taken.count() > 0 || helper_windows > 0 ||
    // !regions.is_empty()`, on the reasoning that publishing an empty set over
    // an empty set is pointless. It is not: the set is PROCESS-GLOBAL and
    // survives the collection that wrote it. The "cleared by the caller after
    // the collection completes" note above is honoured at seven separate exits,
    // and a path that misses one leaves the previous collection's spans in
    // place -- at which point the guard above declines to overwrite them
    // precisely when `regions` is empty, i.e. exactly when they are stale.
    //
    // A stale span is not a conservative degrade. Its owner resumed after the
    // collection that published it and bump-allocated into `[cursor, end)`, so
    // the span now covers LIVE objects; and the sweep's contract for a skip
    // span is that its bytes are not objects. Every linear walk resyncs past
    // it (`skip_free_blocks`), so those objects are never walked, and
    // `mark_young`'s anchor oracle -- whose `verified_spans` are built from the
    // same skip list -- answers "free/gap space, not an object" for any ROOT
    // pointing into one and drops it without marking. The object is therefore
    // neither scanned nor swept: it survives, unmarked, while everything it
    // references is reclaimed underneath it.
    //
    // Measured on `io.netty.util.internal.ObjectCleanerTest` under
    // `-XX:+UseGenerationalGC`: JUnit's static
    // `NamespacedHierarchicalStore$EvaluatedValue.REVERSE_INSERT_ORDER` is a
    // `Collections$ReverseComparator2` whose cycle-0 sweep reports
    // `in_jit_tlab_skip=Some(..)`; its `cmp` lambda is freed and zeroed, and
    // the next `compare` through the still-live comparator raises
    // `AbstractMethodError: java/util/Comparator.compare ... has no Code
    // attribute`. Ignoring the published spans entirely
    // (`CRATONVM_GC_NO_TLAB_SKIP=1`) takes that arm from 6/8 to 0/8, which is
    // what identified the span as the carrier; publishing the CURRENT set every
    // collection is the repair that keeps BUG-03's protection for a genuinely
    // un-retired tail.
    // `CRATONVM_GC_CONDITIONAL_TLAB_SKIP_PUBLISH=1` restores the pre-fix guard
    // for a one-binary A/B.
    //
    // **This lever ALONE no longer reproduces anything, and has not since
    // `24238e856`.** Re-measured 2026-09-06 on `ObjectCleanerTest`, 8 reps per
    // arm, one binary:
    //
    //   lever alone                      gen 0/8   g1 0/8   guard_fired 0/8
    //   CRATONVM_GC_NO_FRAME_TRACE_SPAN_RETIRE=1 alone
    //                                    gen 0/8   g1 0/8
    //   BOTH levers together             gen 3/8   g1 0/8
    //
    // The reclamation needs a PRODUCER of a stale span and a publish path that
    // will not overwrite it, and the two fixes each closed one half: the
    // unconditional publish here, and the ninth exit (`stw_publish_frame_traces`
    // never retiring what it published) in `24238e856`. Either fix alone is
    // sufficient, so either lever alone reads zero. Set BOTH to exercise this
    // code, or the arm is vacuous and the guard will look dead when it is not.
    //
    // The earlier text here promised "6/8 non-clean under Generational and 8/8
    // under G1" for the lever on its own. That was true when written and is
    // not true now; the G1 half was never reproduced on this probe by anyone
    // (see `24238e856`'s own table, which says so about its probe too).
    if cratonvm_types::flags::runtime_var_os("CRATONVM_GC_CONDITIONAL_TLAB_SKIP_PUBLISH").is_some()
    {
        if taken.count() > 0 || helper_windows > 0 || !regions.is_empty() {
            shared.mem.heap.set_jit_tlab_skip_regions(&regions);
        }
    } else {
        shared.mem.heap.set_jit_tlab_skip_regions(&regions);
    }
    if taken.count() > 0 || helper_windows > 0 {
        // Helper-window roots are conservative (unprovable coverage) — the
        // collection must stay non-moving so a false-positive candidate can
        // only over-retain, never relocate under a live JIT/blocked frame.
        // xt-hardening (2026-07-03): such a cycle ALSO disables selective
        // promotion (a frozen peer's registers can hold only a derived/interior
        // pointer whose base would otherwise be evacuated from under it, then
        // zeroed and re-served). Scoped to cycles with actually-frozen/scanned
        // peers — reserved TLAB tails alone freeze nobody, and gating on them
        // would starve promotion on every cooperative multi-threaded cycle.
        //
        // HIB-GCOVERHEAD-HALFFULL.1: the promotion half now travels on its own
        // narrow flag. `mark_moving_young_coverage_incomplete` acquired dozens
        // of unrelated callers (every unproven compiled-frame oop map) and had
        // stopped meaning "un-rewritable peer state" — see
        // `gc_quiescence::unrewritable_peer_state`. Both are set here because
        // this cycle genuinely satisfies both.
        // THE SECOND REFUSAL, and the one the first discharge attempt missed.
        // Suppressing only `xt_root_scan`'s labelled `XT_HELPER_WINDOW` moved
        // the refusal into this unlabelled bucket and left engagement exactly
        // where it was -- `relocation_on_proven_jit` 1 with the pin against 2
        // without. Both sites have to agree, off the same condition.
        //
        // A TAKEN-OVER peer is a different population: its roots come from the
        // takeover pass, which is not pinned here, so `taken.count() > 0` keeps
        // refusing regardless. Only a cycle whose sole unrewritable state is
        // helper windows -- every one of them pinned from a COMPLETE,
        // interior-resolving scan -- may be discharged.
        let helper_only = taken.count() == 0 && helper_windows > 0;
        let discharged = discharge_enabled
            && helper_only
            && xt::helper_windows_all_pinned_this_cycle();
        if !discharged {
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete();
            cratonvm_gc::gc_quiescence::mark_unrewritable_peer_state();
        } else if keep_unrewritable_peer_state_on_discharge() {
            // KEEP THE DERIVED-POINTER GUARD even when the coverage claim is
            // discharged. The two flags answer different questions and the
            // first discharge suppressed both.
            //
            // `unrewritable_peer_state` exists for one hazard, stated in its own
            // doc and in the comment above: "a frozen peer's registers can hold
            // only a derived/interior pointer whose base would otherwise be
            // evacuated from under it, then zeroed and re-served". That
            // hazard is real on its own terms.
            //
            // It was read as the crash signature of
            // `bug-box-unbox-intrinsic-segv-under-relocation-20260902` exactly
            // -- a page-ALIGNED fault address, explained as `compact_low_to`
            // zeroing the vacated span so the reader lands on an all-zero
            // header. That page RETRACTED the reading on 2026-09-04: the
            // address was the BASE of a decommitted 2 MiB arena granule and
            // the access was a WRITE by `relocate_stw` itself. A page-aligned
            // fault address is not a signature -- two different mechanisms
            // produce one.
            //
            // The discharge's argument -- an interior-resolving probe pins the
            // BASE, so a derived pointer is covered -- is an argument about the
            // coverage PROOF. It is not an argument that no unrewritable peer
            // state exists, and a derived pointer the probe cannot resolve to a
            // base is exactly the residue. Five repairs aimed elsewhere changed
            // nothing while the blanket guard was 0/4, which is the evidence
            // that what still bites is peer state rather than frame coverage.
            cratonvm_gc::gc_quiescence::mark_unrewritable_peer_state();
        }
    }
    // One statement of what this take-over left behind, for every backend to
    // read the same way (gc-common w2-c; `TakeoverVerdict::licence`). Written
    // after the helper-window pass so both populations are in it.
    //
    // `unreadable` counts BOTH populations' unread peers (gc-common w3-g). The
    // helper-window pass folds a blocked peer it could not read at all into its
    // per-cycle refusal count, not into a window, so a cycle whose only problem
    // was such a peer used to publish `helper_windows: 0, unreadable: 0` --
    // licence `Move` -- for a peer whose JIT frames nobody saw. With no window
    // found, every refusal the pass recorded IS an unread peer (an unpinned
    // refusal needs a window); with windows found, `pins_complete` below is
    // already false for any refusal.
    // The refusal count is this pause's ledger row (gcd d2/i), not the process
    // word it was: another VM's take-over could store its own 0 between this
    // VM's pass and this read.
    let unreadable = takeover_verdict_unreadable(
        cratonvm_gc::gc_quiescence::xt_cycle_coverage().2,
        helper_windows,
        xt::helper_windows_unpinned_this_cycle(),
    );
    // For the pause's `TakeoverVerdict` (published below): a frozen peer whose
    // machine stack could not be read whole is not completely pinned.
    // gcd d10/t (applied by d10/o): this pause's own ledger row (cleared by
    // `reset_xt_cycle` at the top of this function), not a delta of a process
    // total another VM's take-over also moves. `None` (TLS teardown) reads as
    // incomplete.
    let stack_incomplete = !matches!(
        cratonvm_gc::gc_quiescence::xt_takeover_stack_incomplete_this_pause(),
        Some(0)
    );
    // gc-common w6-g: the per-PAUSE share of take-overs that froze a peer
    // (`common-c-proposal-roll-forward-to-a-poll-REJECTED-20260928`, first measurement).
    xt::XT_TAKEOVER_PAUSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if taken.count() > 0 {
        xt::XT_TAKEOVER_PAUSES_WITH_FROZEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    cratonvm_gc::gc_quiescence::publish_takeover_verdict(
        cratonvm_gc::gc_quiescence::TakeoverVerdict {
            frozen: u32::try_from(taken.count()).unwrap_or(u32::MAX),
            helper_windows: u32::try_from(helper_windows).unwrap_or(u32::MAX),
            unreadable: u32::try_from(unreadable).unwrap_or(u32::MAX),
            pins_complete: !stack_incomplete && xt::helper_windows_all_pinned_this_cycle(),
            derived_pointers_resolved: (taken.count() == 0 || xt::takeover_interior_enabled())
                && (helper_windows == 0 || xt::helper_window_pin_resolve_enabled()),
        },
    );
    // The early `!xt::enabled()` return above does no post-quota work and
    // publishes nothing: `reset_xt_cycle` at the top already zeroed it.
    cratonvm_gc::gc_quiescence::publish_xt_post_quota_ns(
        u64::try_from(quota_met_at.elapsed().as_nanos()).unwrap_or(u64::MAX),
    );
    taken
}

/// The `unreadable` term of the pause's `TakeoverVerdict`: the take-over pass's
/// unclassified peers, plus -- when the helper-window pass found no window --
/// every refusal it recorded, which can then only be a blocked peer it could not
/// read (an unpinned refusal needs a window). See the call site.
fn takeover_verdict_unreadable(
    takeover_unclassified: u64,
    helper_windows: usize,
    helper_window_refusals: u64,
) -> u64 {
    let hw_unreadable = if helper_windows == 0 {
        helper_window_refusals
    } else {
        0
    };
    takeover_unclassified.saturating_add(hw_unreadable)
}

#[cfg(test)]
mod takeover_verdict_unreadable_tests {
    use super::takeover_verdict_unreadable;
    use cratonvm_gc::gc_quiescence::{MoveLicence, TakeoverVerdict};

    /// gc-common w3-g: an unread BLOCKED peer (no window found) used to
    /// publish a `Move` licence.
    #[test]
    fn an_unread_blocked_peer_voids_the_licence() {
        let unreadable = takeover_verdict_unreadable(0, 0, 1);
        assert_eq!(unreadable, 1);
        let v = TakeoverVerdict {
            unreadable: unreadable as u32,
            ..TakeoverVerdict::NONE
        };
        assert_eq!(v.licence(true), MoveLicence::NonMoving);
    }

    /// With windows found, a refusal is already `pins_complete == false`;
    /// it is not double-counted as an unread peer.
    #[test]
    fn refusals_beside_a_window_are_not_unread_peers() {
        assert_eq!(takeover_verdict_unreadable(0, 2, 1), 0);
        assert_eq!(takeover_verdict_unreadable(3, 0, 0), 3);
        assert_eq!(takeover_verdict_unreadable(3, 0, 2), 5);
    }
}

/// gce e2/t: the first-pass grace (`CRATONVM_XT_FIRST_PASS_GRACE_US`).
#[cfg(test)]
mod gce_e2t_first_pass_grace_tests {
    use super::uncounted_roster;

    /// Once every counted mutator has arrived, the pass owes a signal only to
    /// the roster threads the census did not count (it counted blocked ones
    /// too), in roster order.
    #[test]
    fn the_grace_pass_signals_only_the_uncounted_roster() {
        assert_eq!(uncounted_roster(&[5, 9, 11, 14], &[9, 5, 14]), vec![11]);
        assert!(uncounted_roster(&[5, 9], &[9, 5, 77]).is_empty());
        assert_eq!(uncounted_roster(&[3, 2], &[]), vec![3, 2]);
        assert!(uncounted_roster(&[], &[1]).is_empty());
    }

    /// 200 us by default since gce ve2; `0` turns it off (the loop then runs
    /// exactly as before); a value is microseconds, capped at 10 ms.
    #[test]
    fn the_grace_is_200_us_unless_set() {
        use std::time::Duration;
        let at = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_XT_FIRST_PASS_GRACE_US", v)],
                super::takeover_first_pass_grace,
            )
        };
        assert_eq!(at(None), Duration::from_micros(200));
        assert!(at(Some("0")).is_zero());
        assert_eq!(at(Some("50")), Duration::from_micros(50));
        assert_eq!(at(Some("999999")), Duration::from_millis(10));
    }

    /// The loop's early exit sits at the top of the loop, so a met grace runs
    /// no round and the unmet case runs every round it did before.
    #[test]
    fn a_met_grace_skips_the_round_loop() {
        let src = include_str!("gc_and_alloc.rs");
        let start = src
            .find("pub(super) fn stw_take_over_and_wait(")
            .expect("the take-over");
        let b = &src[start..];
        let grace = b.find("let grace_met =").expect("grace");
        // The early exit is the `if grace_met {` whose body is `break;`.
        let open = "if grace_met {";
        let exit = b
            .match_indices(open)
            .map(|(i, _)| i)
            .find(|&i| b[i + open.len()..].trim_start().starts_with("break;"))
            .expect("early exit");
        let rounds = b.find("stw_takeover_should_scan(rounds").expect("rounds");
        assert!(grace < exit && exit < rounds);
    }
}

/// gc-common w5-a — source ratchets for the pause drivers.
#[cfg(test)]
mod w5a_pause_driver_tests {
    fn source() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/runtime/interpreter/gc_and_alloc.rs"
        );
        std::fs::read_to_string(path)
            .expect("gc_and_alloc.rs is readable")
            .replace("\r\n", "\n")
    }

    /// The item starting at `sig` at the START of a line (so the string
    /// literals in this module never match) up to the next column-0 `}`.
    fn body(src: &str, sig: &str) -> String {
        let start = src
            .find(&format!("\n{sig}"))
            .unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start + 1..];
        // `\u{7d}` is `}`: a bare one in a literal would end this module early
        // for the production-panic scanner, which counts raw braces.
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// `handoff-w4b-takeover-publishes-the-one-thread-field-list`: the frozen
    /// peer's JvmThread FIELD roots come from the one per-thread list, not
    /// from a hand-written partial copy (which missed `native_alloc_pool`,
    /// `handle_slots`, the parked throwables, the mirror, ...).
    #[test]
    fn the_frozen_peer_publisher_uses_the_one_field_list() {
        let src = source();
        let b = body(&src, "pub(super) fn stw_take_over_and_wait(");
        assert!(
            b.contains("roots::push_off_frame_thread_roots(peer, xt_roots)"),
            "the frozen-peer block must call push_off_frame_thread_roots"
        );
        for copy in [
            "peer.native_pin_roots",
            "peer.native_pending_return",
            "peer.jit_hashmap_string_node_cache",
            "peer.string_case_cache",
        ] {
            assert!(
                !b.contains(copy),
                "a hand-written field copy (`{copy}`) is back in stw_take_over_and_wait"
            );
        }
    }

    /// `common-w4f-gc-event-durations-sampled-after-release`: the collection's
    /// end is sealed before the frozen peers resume, and reported after the
    /// release.
    #[test]
    fn the_collection_end_is_sealed_inside_the_pause() {
        let src = source();
        let b = body(&src, "fn run_collection_pause(");
        let seal = b.find("gc_event_seal(shared, &mut gc_event)").expect("sealed");
        let resume = b
            .find("retire_skip_spans_and_resume(shared, taken)")
            .expect("resumes");
        let release = b.find(".complete_gc(result.pointer_map)").expect("releases");
        let finish = b.find("gc_event_finish(shared, door, gc_event)").expect("reports");
        assert!(seal < resume && resume < release && release < finish);
    }

    /// `common-w2a-finalizers-queued-by-forced-collections-wait-for-another-door`
    /// (the fully-compiled half): the "undrained" verdict is read before this
    /// collection queues its own finalizers, and acted on only after the
    /// world is released.
    #[test]
    fn undrained_finalizers_are_judged_before_the_enqueue_and_handed_off_after_release() {
        let src = source();
        let b = body(&src, "fn run_collection_pause(");
        let judged = b
            .find("ref_work_stale(UNDRAINED_REF_WORK_PAUSES)")
            .expect("judged");
        let enqueue = b
            .find("enqueue_resurrected_finalizers(shared, &dead_finalizers)")
            .expect("enqueues");
        let release = b.find(".complete_gc(result.pointer_map)").expect("releases");
        let handoff = b
            .find("hand_undrained_finalizers_to_delivery_thread(shared)")
            .expect("hands off");
        assert!(judged < enqueue && release < handoff);
        // Two whole pauses, not one: see the constant's doc.
        assert_eq!(super::UNDRAINED_REF_WORK_PAUSES, 2);
    }

    /// `common-w4a-non-collection-pauses-have-no-pause-line`: every
    /// non-collection pause driver times itself, seals before resuming the
    /// frozen peers, and prints after the release.
    #[test]
    fn every_non_collection_pause_has_a_pause_line() {
        let src = source();
        for (sig, kind) in [
            ("pub(super) fn maybe_concurrent_gc_at(", "GenInitialMark"),
            ("fn gen_concurrent_remark_pause(", "GenRemark"),
            ("pub(super) fn zgc_concurrent_mark_cycle(", "ZgcMarkStart"),
            ("pub(super) fn g1_concurrent_mark_cycle(", "G1InitialMark"),
            ("pub(super) fn g1_final_remark_cleanup(", "G1Remark"),
        ] {
            let b = body(&src, sig);
            let start = b
                .find(&format!("NonCollectionPause::{kind})"))
                .unwrap_or_else(|| panic!("{sig} starts no `{kind}` pause timer"));
            let seal = b
                .find("non_collection_pause_seal(&mut pause_timer)")
                .unwrap_or_else(|| panic!("{sig} never seals its pause timer"));
            let finish = b
                .find("non_collection_pause_finish(pause_timer)")
                .unwrap_or_else(|| panic!("{sig} never prints its pause line"));
            let resume = b.find("retire_skip_spans_and_resume(shared, taken)");
            assert!(start < seal && seal < finish, "{sig}: start < seal < finish");
            if let Some(resume) = resume {
                assert!(seal < resume, "{sig}: seal before the peers resume");
            }
        }
        // The HPROF pause seals and prints from its RAII `Drop`.
        let drop_body = body(&src, "impl Drop for NonMovingPause<'_> {");
        let seal = drop_body.find("non_collection_pause_seal(").expect("seals");
        let resume = drop_body.find("retire_skip_spans_and_resume").expect("resumes");
        let finish = drop_body.find("non_collection_pause_finish(").expect("prints");
        assert!(seal < resume && resume < finish);
        assert!(body(&src, "    pub(crate) fn request(shared: &'a SharedVm")
            .contains("NonCollectionPause::HeapDump"));
        // The frame-trace pause is a handshake pause of its own kind, printed
        // by the same `Drop` (interpreter round i1 wave 14, lane L3).
        let frame_trace = body(&src, "pub(crate) fn stw_publish_frame_traces(");
        let raise = frame_trace
            .find("FrameTraceWanted::raise()")
            .expect("raises the request first");
        let request = frame_trace
            .find("NonMovingPause::request_handshake(")
            .expect("takes a handshake pause");
        assert!(raise < request);
        assert!(frame_trace.contains("NonCollectionPause::FrameTrace,"));
        assert!(frame_trace.contains("FRAME_TRACE_GRACE,"));
    }
}

/// `CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE=1` -- a discharged helper-window
/// cycle still declares UNREWRITABLE PEER STATE.
///
/// The helper-window discharge suppressed two flags where it had an argument
/// for only one. See the call site.
fn keep_unrewritable_peer_state_on_discharge() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE")
            .is_some()
    })
}

/// INT-3 (G1) — pin-in-place everything a forcibly-frozen peer can address.
///
/// A frozen peer is excused from the STW barrier, so it never applies this
/// collection's pointer map to its own state; under an EVACUATING collector
/// every object it references directly must therefore stay put. Two sources:
///
/// 1. `xt_roots` — the conservative register/stack scan of each frozen peer
///    (plus the helper-window scans of blocked threads, whose Rust-stack
///    locals are equally un-rewritable).
/// 2. The frozen peers' deposited root snapshots — the collector's only view
///    of their interpreter frames. The snapshot entries are merged into the
///    collection roots (which keeps the objects ALIVE and rewrites the
///    merged copies), but the peer's actual frame slots are never remapped:
///    it skips the safepoint-resume `apply_pointer_map_to_thread`. Pinning
///    the referenced regions keeps those slots valid; the objects' own
///    fields are still fixed up in place by the phase-4 walk.
///
/// `add_pinned_jit_root` keys the pins to the INITIATOR's registry entry, so
/// they last exactly one cycle (the initiator's next deposit or
/// `collect_roots` replaces/clears them) and over-pinning only keeps a
/// region out of one CSet. MUST run after `collect_roots` (which clears the
/// initiator's entry) and before `collect_garbage`.
///
/// No-op on non-G1 backends: Generational frozen-peer cycles run the fully
/// non-moving sweep (`mark_moving_young_coverage_incomplete`), so nothing
/// moves and no pin is needed.
pub(super) fn pin_frozen_peer_roots_for_g1(
    shared: &SharedVm,
    xt_roots: &[ObjectRef],
    taken: &crate::jit::xt_root_scan::TakenOver,
) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    for r in xt_roots {
        cratonvm_gc::gc_quiescence::add_pinned_jit_root(r.as_ptr() as usize);
    }
    for r in shared
        .threads
        .thread_registry
        .root_snapshots_for_os_tids(&taken.tids)
    {
        cratonvm_gc::gc_quiescence::add_pinned_jit_root(r.as_ptr() as usize);
    }
}

// stw-residual-close (CRATONVM_DBG_REMAP_TRACE): per-OS-thread debug rings.
// (a) participation trace: one line per GC-relevant transition on this thread
// (publish/deposit/arrive/apply/wake/initiator) with the top frame + pc and
// the map/fixup size, so a stale-ref capture can reconstruct exactly how the
// holder participated in the fatal epoch. (b) native-return ring: the last
// object-returning natives and their returned addresses, so a capture can
// name the producer of a stale value that was pushed from a Rust-side copy.
//
// COST WHEN OFF (gc-common w1-c, 2026-09-23). `remap_trace_on` gates
// interpreter hot paths -- `field_fast`'s getfield door, every `dup*`/`swap` of
// a reference (`record_shuffle_push`), invoke returns, `ldc`, `new` -- and the
// ring writers below are called from `update_root_snapshot` on every
// object-returning native call. It used to be a non-`#[inline]` function around
// a `OnceLock` (an out-of-line call plus an Acquire load of the `Once` state
// per bytecode), and the writers were out-of-line calls that tested the gate
// only once inside. Now the gate is an inlined Relaxed load of a tri-state byte
// with a cold initialiser, and each writer is an inlined gate in front of a
// `#[cold]` body, so a disabled ring costs one load and one predictable branch
// at the call site. The answer is unchanged: latched once per process from
// the same variable.
static REMAP_TRACE_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[inline(always)]
pub(crate) fn remap_trace_on() -> bool {
    match REMAP_TRACE_STATE.load(std::sync::atomic::Ordering::Relaxed) {
        0 => remap_trace_on_init(),
        s => s == 2,
    }
}

#[cold]
#[inline(never)]
fn remap_trace_on_init() -> bool {
    // Racing initialisers compute the same answer from the same latched
    // flag snapshot, so a plain store is enough.
    let on = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_REMAP_TRACE").is_some();
    REMAP_TRACE_STATE.store(if on { 2 } else { 1 }, std::sync::atomic::Ordering::Relaxed);
    on
}

thread_local! {
    static REMAP_TRACE: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static NRET_RING: std::cell::RefCell<Vec<(usize, usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static GETFIELD_RING: std::cell::RefCell<Vec<(usize, usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

thread_local! {
    static DEPOSIT_GAP_RING: std::cell::RefCell<Vec<(usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// After a snapshot build, diff it against a RAW walk of the frames: ring
/// every Object-decoding local/stack slot whose address the snapshot lacks.
#[inline]
pub(crate) fn deposit_gap_diff(
    thread: &JvmThread,
    snapshot: &[crate::types::ObjectRef],
    tag: &str,
) {
    if remap_trace_on() {
        deposit_gap_diff_cold(thread, snapshot, tag);
    }
}

#[cold]
#[inline(never)]
fn deposit_gap_diff_cold(thread: &JvmThread, snapshot: &[crate::types::ObjectRef], tag: &str) {
    let have: std::collections::HashSet<usize> =
        snapshot.iter().map(|o| o.as_ptr() as usize).collect();
    DEPOSIT_GAP_RING.with(|ring| {
        let mut ring = ring.borrow_mut();
        for (fi, fr) in thread.frames.iter().enumerate() {
            for li in 0..fr.locals_len() {
                if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                    let a = o.as_ptr() as usize;
                    if !have.contains(&a) {
                        if ring.len() >= 512 {
                            ring.drain(..128);
                        }
                        ring.push((
                            a,
                            format!(
                                "{tag} local f#{fi} {}.{} pc={} slot={li}",
                                fr.class_name(),
                                fr.method_name(),
                                fr.pc
                            ),
                        ));
                    }
                }
            }
            for si in 0..fr.stack.len() {
                if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                    let a = o.as_ptr() as usize;
                    if !have.contains(&a) {
                        if ring.len() >= 512 {
                            ring.drain(..128);
                        }
                        ring.push((
                            a,
                            format!(
                                "{tag} stack f#{fi} {}.{} pc={} slot={si}",
                                fr.class_name(),
                                fr.method_name(),
                                fr.pc
                            ),
                        ));
                    }
                }
            }
        }
    });
}

thread_local! {
    static PUSH_PROV_RING: std::cell::RefCell<Vec<(usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Record an invoke-return Object push with its call-site description.
#[inline]
pub(crate) fn push_prov_record(addr: usize, site: &str) {
    if remap_trace_on() {
        push_prov_record_cold(addr, site);
    }
}

#[cold]
#[inline(never)]
fn push_prov_record_cold(addr: usize, site: &str) {
    PUSH_PROV_RING.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() >= 128 {
            r.drain(..32);
        }
        r.push((addr, site.to_string()));
    });
}

/// Record a kind-preserving shuffle (dup*/swap) re-push of an Object slot,
/// for the same pushprov ring `push_prov_record` feeds. Cheap no-op unless
/// `CRATONVM_DBG_REMAP_TRACE` is set.
#[inline]
pub(super) fn record_shuffle_push(cv: crate::types::CompactValue, site: &str) {
    if remap_trace_on() {
        if let Value::Object(Some(o)) = cv.to_value() {
            push_prov_record(o.as_ptr() as usize, site);
        }
    }
}

/// Probe recent invoke-return pushes for `addr`: (pushes-ago, site).
pub(crate) fn push_prov_find(addr: usize) -> Vec<(usize, String)> {
    PUSH_PROV_RING.with(|r| {
        let r = r.borrow();
        let n = r.len();
        r.iter()
            .enumerate()
            .filter(|(_, (a, _))| *a == addr)
            .map(|(i, (_, s))| (n - i, s.clone()))
            .collect()
    })
}

/// Probe the deposit-gap ring for `addr`: (entries-ago, description).
pub(crate) fn deposit_gap_find(addr: usize) -> Vec<(usize, String)> {
    DEPOSIT_GAP_RING.with(|r| {
        let r = r.borrow();
        let n = r.len();
        r.iter()
            .enumerate()
            .filter(|(_, (a, _))| *a == addr)
            .map(|(i, (_, d))| (n - i, d.clone()))
            .collect()
    })
}

/// Record a reference-typed getfield push: (parent, field_index, pushed).
///
/// (This line used to sit above the `DEPOSIT_GAP_RING` thread-local, where it
/// documented nothing.)
#[inline]
pub(crate) fn getfield_ring_record(parent: usize, idx: usize, pushed: usize) {
    if remap_trace_on() {
        getfield_ring_record_cold(parent, idx, pushed);
    }
}

#[cold]
#[inline(never)]
fn getfield_ring_record_cold(parent: usize, idx: usize, pushed: usize) {
    GETFIELD_RING.with(|r| {
        let mut r = r.borrow_mut();
        // Batch drain (w2-c): `remove(0)` shifted the whole ring on every
        // record once full, on the per-getfield path while armed.
        if r.len() >= 96 {
            r.drain(..24);
        }
        r.push((parent, idx, pushed));
    });
}

/// Find `addr` among recent reference getfield pushes:
/// (pushes-ago, parent, field_index).
pub(crate) fn getfield_ring_find(addr: usize) -> Vec<(usize, usize, usize)> {
    GETFIELD_RING.with(|r| {
        let r = r.borrow();
        let n = r.len();
        r.iter()
            .enumerate()
            .filter(|(_, (_, _, a))| *a == addr)
            .map(|(i, (p, f, _))| (n - i, *p, *f))
            .collect()
    })
}

#[inline]
pub(crate) fn remap_trace_push(shared: &SharedVm, thread: &JvmThread, tag: &str, extra: &str) {
    if remap_trace_on() {
        remap_trace_push_cold(shared, thread, tag, extra);
    }
}

#[cold]
#[inline(never)]
fn remap_trace_push_cold(shared: &SharedVm, thread: &JvmThread, tag: &str, extra: &str) {
    let top = thread
        .frames
        .last()
        .map(|f| format!("{}.{} pc={}", f.class_name(), f.method_name(), f.pc))
        .unwrap_or_else(|| "<no-frame>".to_string());
    let line = format!(
        "e{} {} tid={} frames={} top={} {}",
        shared.mem.heap.collection_count(),
        tag,
        thread.thread_id.0,
        thread.frames.len(),
        top,
        extra,
    );
    REMAP_TRACE.with(|t| {
        let mut t = t.borrow_mut();
        if t.len() >= 24 {
            t.remove(0);
        }
        t.push(line);
    });
}

pub(crate) fn remap_trace_dump() -> String {
    REMAP_TRACE.with(|t| {
        t.borrow().join(
            "
    ",
        )
    })
}

#[inline]
pub(crate) fn nret_record(cb: usize, addr: usize, site: &str) {
    if remap_trace_on() {
        nret_record_cold(cb, addr, site);
    }
}

#[cold]
#[inline(never)]
fn nret_record_cold(cb: usize, addr: usize, site: &str) {
    NRET_RING.with(|r| {
        let mut r = r.borrow_mut();
        // Batch drain, as `getfield_ring_record_cold` (w2-c).
        if r.len() >= 64 {
            r.drain(..16);
        }
        r.push((cb, addr, site.to_string()));
    });
}

/// Find `addr` among recent native object returns:
/// (returns-ago, callback, java-site).
pub(crate) fn nret_find(addr: usize) -> Vec<(usize, usize, String)> {
    NRET_RING.with(|r| {
        let r = r.borrow();
        let n = r.len();
        r.iter()
            .enumerate()
            .filter(|(_, (_, a, _))| *a == addr)
            .map(|(i, (cb, _, s))| (n - i, *cb, s.clone()))
            .collect()
    })
}

/// Finalizable roots every collection must seed, whichever path runs it.
///
/// Two disjoint sets, both of which the collector would otherwise miss:
///
///   * `ref_processor.finalizer_referent_addresses()` — registered
///     finalizables not yet claimed. Only the `System.gc` path used to pass
///     these; the ordinary allocation-driven collections below passed `&[]`.
///   * `finalizer_thread.pending_addresses()` — objects already claimed and
///     queued, waiting for `finalize()` to actually run. `mark_finalizer_enqueued`
///     deliberately drops these from the first list, so nothing else roots
///     them, yet the queue keeps only a raw address. `run_finalizers` also
///     bails out early whenever a JIT borrow is live, which routinely leaves
///     entries queued across several collections — a wide window in which a
///     non-moving young sweep frees the object and leaves the queue pointing
///     at reclaimed memory (SEGV in `run_finalizers`' `class_id_of`).
pub(super) fn finalizable_roots(shared: &SharedVm) -> Vec<usize> {
    let mut addrs = {
        let rp = shared.mem.ref_processor.lock();
        rp.finalizer_referent_addresses()
    };
    addrs.extend(shared.mem.finalizer_thread.pending_addresses());
    addrs.sort_unstable();
    addrs.dedup();
    addrs
}

/// Tell the reference processor, before this collection's processing round,
/// which finalizable objects the collection resurrected -- as the
/// PRE-collection addresses its rows hold. A candidate `c` was resurrected iff
/// `pointer_map.get(c).unwrap_or(c)` is in `dead_finalizers` (post-move
/// addresses on every backend, the contract [`enqueue_resurrected_finalizers`]
/// relies on). See `ReferenceProcessor::note_resurrected_finalizables`.
/// gc-common w6-d
/// (`common-d-weak-refs-honour-finalizer-reachability-unlike-hotspot`, the
/// depth-1 half). Takes `ref_processor` (L7) alone, at a point where the pause
/// holds no other lock.
///
/// Depth 2 (gc-common w7-d protocol, VM half wired by w18-a): the collector's
/// whole resurrection closure (`VmHeap::take_resurrection_closure`, the
/// objects its drain marked that its strong closure had not) is noted here as
/// well, through the same processor call. Every backend answers empty until
/// its owner records the set, so today this is the depth-1 behaviour; see
/// `docs/internal/gc-common-round-20260923/handoff-w7d-collectors-report-the-resurrection-closure.md`.
///
/// (Until w7-d this function sat between [`enqueue_resurrected_finalizers`]
/// and that function's doc comment, so rustdoc glued the two docs onto this
/// one and left the enqueue undocumented.)
fn note_resurrected_finalizables_for_reference_processing(
    shared: &SharedVm,
    fin_roots: &[usize],
    pointer_map: &cratonvm_types::PointerMap,
    dead_finalizers: &[usize],
) {
    // Taken on EVERY pause, before either early return, so a set a collector
    // recorded can never outlive the pause that recorded it (the collector
    // resets it at the next collection too; this is the belt to that brace).
    let closure = shared.mem.heap.take_resurrection_closure();
    // gc-common w7-d: not under `CRATONVM_DBG_NO_REFPROC` either. That switch
    // makes `process_references_after_gc` (and G1's remark) return before any
    // round and before `finish_pre_gc_cycle`, so nothing would ever consume or
    // clear a set noted here: it grew by every resurrected finalizable of
    // every pause, holding PRE-collection addresses of collections long past.
    if no_refproc() {
        return;
    }
    let pre_gc = resurrected_pre_gc_addresses(fin_roots, pointer_map, dead_finalizers, closure);
    if pre_gc.is_empty() {
        return;
    }
    shared
        .mem
        .ref_processor
        .lock()
        .note_resurrected_finalizables(&pre_gc);
}

/// The PRE-collection addresses the reference processor is told were marked
/// only by the finalizer-resurrection drain: every candidate in `fin_roots`
/// whose post-collection address (`pointer_map`, else itself) is in
/// `dead_finalizers` (depth 1), followed by the collector's reported
/// `closure` (depth 2, already PRE-collection addresses; may repeat depth-1
/// entries, which the processor's set absorbs). Empty, with nothing
/// allocated, when both inputs are empty -- the common pause.
///
/// Split out of [`note_resurrected_finalizables_for_reference_processing`]
/// (gc-common w18-a) so the depth-2 consumer can be tested with an injected
/// closure while every collector still reports none.
fn resurrected_pre_gc_addresses(
    fin_roots: &[usize],
    pointer_map: &cratonvm_types::PointerMap,
    dead_finalizers: &[usize],
    closure: Vec<usize>,
) -> Vec<usize> {
    if dead_finalizers.is_empty() {
        return closure;
    }
    let dead: rustc_hash::FxHashSet<usize> = dead_finalizers.iter().copied().collect();
    let mut pre_gc: Vec<usize> = fin_roots
        .iter()
        .copied()
        .filter(|a| dead.contains(&pointer_map.get(a).copied().unwrap_or(*a)))
        .collect();
    pre_gc.extend(closure);
    pre_gc
}

#[cfg(test)]
mod w18a_resurrection_closure_tests {
    use super::resurrected_pre_gc_addresses;
    use cratonvm_types::PointerMap;

    /// The common pause: no dead finalizer, no closure -- nothing noted.
    #[test]
    fn no_dead_finalizer_and_no_closure_notes_nothing() {
        let map = PointerMap::default();
        assert!(resurrected_pre_gc_addresses(&[0x100, 0x140], &map, &[], Vec::new()).is_empty());
    }

    /// Depth 1 is the w6-d filter, unchanged: a candidate whose
    /// post-collection address is a dead finalizer, at its PRE-collection
    /// address. The closure follows it.
    #[test]
    fn the_depth_one_filter_is_unchanged_and_the_closure_follows_it() {
        // 0x100 moved to 0x900 and was resurrected; 0x140 was resurrected in
        // place; 0x180 was simply alive.
        let map = PointerMap::from_iter([(0x100, 0x900)]);
        let got = resurrected_pre_gc_addresses(
            &[0x100, 0x140, 0x180],
            &map,
            &[0x900, 0x140],
            vec![0x240],
        );
        assert_eq!(got, vec![0x100, 0x140, 0x240]);
    }

    /// A collector's closure is noted even when the VM's own depth-1 set is
    /// empty.
    #[test]
    fn a_closure_alone_is_noted() {
        let map = PointerMap::default();
        assert_eq!(
            resurrected_pre_gc_addresses(&[0x100], &map, &[], vec![0x240, 0x280]),
            vec![0x240, 0x280]
        );
    }

    /// The consumer end to end, with an injected closure standing in for a
    /// collector that reports one: the weak reference to the object reachable
    /// only THROUGH the finalizable parent is cleared and enqueued; a weak
    /// reference to an object the strong closure reached keeps its referent.
    #[test]
    fn an_injected_closure_clears_the_weak_reference_to_the_child() {
        const PARENT: usize = 0x200;
        const CHILD: usize = 0x240;
        const SHARED: usize = 0x280;
        let mut proc = cratonvm_gc::ReferenceProcessor::new();
        proc.discover_reference(cratonvm_gc::ReferenceType::Finalizer, PARENT, PARENT, None);
        proc.discover_reference(cratonvm_gc::ReferenceType::Weak, 0x300, CHILD, Some(0x400));
        proc.discover_reference(cratonvm_gc::ReferenceType::Weak, 0x310, SHARED, Some(0x400));
        let map = PointerMap::default();
        let pre_gc = resurrected_pre_gc_addresses(&[PARENT], &map, &[PARENT], vec![CHILD]);
        assert_eq!(pre_gc, vec![PARENT, CHILD]);
        proc.note_resurrected_finalizables(&pre_gc);
        // Everything is "marked": the collection marked the child only through
        // the resurrection drain, which is what the noted set says.
        let result = proc.process_references(&|_addr: usize| true, 0, 1_000_000);
        assert_eq!(result.to_enqueue, vec![(0x300, 0x400)]);
    }
}

/// Consume the resurrection channel's answer: the POST-collection addresses
/// of the finalizable objects `collect_garbage_with_finalizers` found dead and
/// kept alive (its second tuple element).
///
/// Queues each for `finalize()` and flags its processor row enqueued, so the
/// object is finalized exactly once and is not offered — and resurrected with
/// its whole subtree — again by the next collection. MUST run after
/// [`process_references_after_gc`] for the same collection: that pass ends with
/// `update_after_gc`, so the processor rows already hold the post-collection
/// addresses this list is in.
///
/// # Why this exists (gc-common w1-d, 2026-09-23)
///
/// Only `force_gc_from_native` did this, inline, twice. The four
/// allocation-driven collection sites in `maybe_gc` / `maybe_gc_forced_at` pass
/// [`finalizable_roots`] in (since 61e4b2cc6, 2026-07-27) and then DISCARD the
/// result (`.0`). So on an allocation-triggered collection every dead
/// finalizable object is resurrected, never queued, and never flagged: its row
/// stays un-enqueued, `finalizable_roots` offers it again next cycle, and it
/// is resurrected again — with everything it references — on every collection
/// until something calls `System.gc()`. A program that never does never runs a
/// single `finalize()` and never frees a finalizable object.
///
/// Wired in gc-common w2-a (2026-09-23): [`run_collection_pause`], the one
/// pause every GC door now runs, calls this for every collection
/// (`handoff-d-allocation-gcs-drop-dead-finalizers`, applied). `System.gc()`'s
/// two inline copies — which used the address-keyed `enqueue` refusal — are
/// gone with it.
pub(crate) fn enqueue_resurrected_finalizers(shared: &SharedVm, dead_finalizers: &[usize]) {
    if dead_finalizers.is_empty() {
        return;
    }
    for &new_addr in dead_finalizers {
        // Rows reported through `finalizable_roots` are never-enqueued rows or
        // entries already waiting in the queue (deduplicated there) — see
        // `FinalizerThread::enqueue_unfinalized` for why `enqueue`'s
        // address-keyed refusal must not apply.
        shared.mem.finalizer_thread.enqueue_unfinalized(new_addr);
    }
    shared
        .mem
        .ref_processor
        .lock()
        .mark_finalizer_enqueued(dead_finalizers);
    // gc-common w3-d: every door's pause ends here, including the
    // allocation-FAILURE door that drains nothing itself — so this is where the
    // delivery thread learns of (and on first use is started for) the
    // finalizers this collection resurrected. Flag-gated; see
    // `wake_finalizer_thread` for why it is safe inside the pause.
    wake_finalizer_thread(shared);
}

/// Check if the heap needs garbage collection, and if so, run a collection.
///
/// Called after the allocation instructions (`new`, `newarray`, `anewarray`,
/// `multianewarray`). It collects roots from the current thread and shared VM
/// state, runs the configured collector, and updates all references in place.
///
/// In multi-threaded mode, this coordinates with other threads via the GC barrier:
/// 1. The initiating thread requests stop-the-world
/// 2. Other threads deposit their root snapshots and pause
/// 3. The initiator collects all roots and runs GC
/// 4. All threads update their own frame references from the pointer map
///
/// # The occupancy half is refill-driven (round i1 wave 20, lane L4)
///
/// The safepoint poll, the `gc_requested` latch and the reference-work hint
/// are asked on every call. The two OCCUPANCY triggers — ZGC's concurrent-mark
/// start and `needs_gc` — are asked only when [`occupancy_poll_due`] says so:
/// after this thread refilled its TLAB or allocated outside it (both reset the
/// countdown, [`note_occupancy_may_have_moved`]), or once it has allocated
/// [`GC_OCCUPANCY_POLL_BYTES`] since its last occupancy poll. Every backend
/// charges its occupancy when a chunk is CARVED or a shared-heap object is
/// allocated, never when a thread bumps its own cursor inside a chunk it
/// already owns (Generational: `young_from` `used`/`free_list_bytes`; G1: the
/// Free-region census; ZGC: `allocated`, charged at `refill_tlab`), so on a
/// TLAB hit this thread's answer can have moved only through another thread's
/// refill, which polls at its own next allocation. See
/// `docs/internal/fixed-bugs/interpreter-L2-proposal-refill-driven-gc-poll-after-new-FIXED-20260930.md`;
/// [`FULL_GC_POLL_AFTER_EVERY_NEW`] restores the per-allocation poll.
pub(crate) fn maybe_gc(shared: &SharedVm, thread: &mut JvmThread) {
    // First, check if another thread requested STW — if so, participate
    safepoint_check(shared, thread);

    // Read after the safepoint: a pause this thread just took part in retired
    // its TLAB, and the refill that follows resets the countdown anyway.
    let occupancy_poll = occupancy_poll_due(thread);

    // ZGC: open a CONCURRENT mark cycle once allocation crosses the start
    // threshold, so the transitive closure is traced with the mutators
    // running instead of inside the collection pause. Checked before
    // `needs_gc` because the two are mutually exclusive by construction:
    // `should_start_concurrent_mark` refuses at or above the collection
    // threshold, where a cycle would get no concurrent phase at all.
    //
    // Costs one `match` and two relaxed loads per allocation that reaches
    // here, and exactly that on the other two backends (their arm is a
    // compile-time `false`).
    if occupancy_poll && shared.mem.heap.zgc_should_start_concurrent_mark() {
        zgc_concurrent_mark_cycle(shared, thread);
    }

    // gce e1/c (`gengc-r4w4-concmark4-...` item 1): a direct old-gen
    // allocation latched a concurrent-start request with no service attached
    // (`CRATONVM_GEN_CONC_INLINE_START`); run the driver here, where this
    // thread may take part in a pause. Nothing is latched with the flag off.
    if occupancy_poll
        && shared.mem.concurrent_gc_state.inline_start_requested()
        && shared.mem.concurrent_gc_state.take_inline_start_request()
    {
        maybe_concurrent_gc(shared, thread);
    }

    // Evaluated into named locals rather than left in the `||`: the two
    // halves are different answers to "why did this cycle happen", and the
    // latch half is invisible to `[GC] zgc-trigger` because it never asks
    // `needs_gc`.
    //
    // LOAD BEFORE SWAP (2026-09-23). This runs after EVERY interpreted `new` /
    // `newarray` that does not trip `needs_gc`, i.e. on almost every
    // allocation, and a bare `swap` is a locked read-modify-write that takes
    // the `gc_requested` cache line EXCLUSIVE on every call -- on N allocating
    // threads, a line bounced between N cores for a latch that is set perhaps
    // once a run (`jcmd GC.run`, the serviceability `trigger_gc`). The relaxed
    // load keeps the line shared-clean in every core's cache; the swap still
    // does the consuming, so a request raised between the two is still taken
    // exactly once, by whichever thread's swap sees it.
    //
    // THE LATCH IS CONSUMED BY EVERY COLLECTION ATTEMPT (gc-common w2-a,
    // 2026-09-23). It used to be consumed only when `needs_gc()` said no. The
    // native string path (`vm_exec.rs`, on a `young_bump_headroom` miss) sets
    // the latch and calls straight in here -- precisely when young is full, so
    // `needs_gc()` usually says yes -- and the latch then rode through the
    // collection it had asked for and fired a SECOND, back-to-back collection
    // ("Diagnostic Command") at the next allocation on any thread. The
    // collection this call starts, or the one it takes part in, satisfies the
    // request either way. Still one relaxed load when the latch is clear.
    let entry_needs = occupancy_poll && shared.mem.heap.needs_gc();
    if occupancy_poll {
        // Re-armed whatever the answer: a collection below retires the TLAB,
        // and the refill after it asks again at once.
        arm_occupancy_poll_countdown(thread);
    }
    let entry_requested = shared
        .mem
        .gc_requested
        .load(std::sync::atomic::Ordering::Relaxed)
        && shared
            .mem
            .gc_requested
            .swap(false, std::sync::atomic::Ordering::Relaxed);
    if !(entry_needs || entry_requested) {
        drain_ref_work_queued_elsewhere(shared, thread);
        return;
    }
    if entry_needs {
        cratonvm_types::gc_entry_census::note_maybe_gc_needs();
    } else {
        cratonvm_types::gc_entry_census::note_maybe_gc_requested();
    }
    // Retire TLAB before GC — its memory is in from-space
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    // Round-5 fix (CRIT — UAF): the GC initiator never passes through
    // `safepoint_check`'s arrive_and_wait, so drain its OWN per-thread
    // SATB buffer here. Without this the initiator's last up-to-255
    // overwritten references vanish on every cycle; the bug bites
    // hardest in single-threaded mode where the initiator IS every
    // mutator.
    shared.mem.heap.flush_thread_satb();
    // gcd d9/b: census of occupancy-triggered collections that start on a
    // wedged old generation (`[GC] oome_ladder: ladder_threshold_wedged*`),
    // the crawl of `gcd-d4j-threads-oom-probe-crawls-on-a-full-old-gen`. One
    // old-generation read per collection, Generational only.
    let wedged_at_entry = old_gen_wedged_now(shared).then(std::time::Instant::now);
    // The pause itself — request, coverage cycle, roots, collection, reference
    // and finalizer processing, remap, release — is the shared door. A `None`
    // means another thread's pause was already in flight and this thread has
    // taken part in it, which is all a losing initiator ever did.
    if run_collection_pause(shared, thread, GcDoor::AllocationThreshold).is_none() {
        return;
    }
    if let Some(t0) = wedged_at_entry {
        ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::THRESHOLD_WEDGED, 1);
        ladder_census(
            shared,
            cratonvm_gc::gen_heap::oome_ladder::THRESHOLD_WEDGED_US,
            elapsed_us(t0),
        );
    }
    // After minor GC, check if old gen needs concurrent collection
    maybe_concurrent_gc(shared, thread);
    // Run any pending finalizers
    run_finalizers(shared, thread);
    // ...and any pending Cleaner actions. Until 2026-08-05 this was
    // called ONLY from the forced `System.gc()` path, so a cleanable
    // whose referent died during an ordinary allocation-triggered
    // collection stayed queued indefinitely — its native memory (a
    // direct `ByteBuffer`'s backing block, a mapped region, a file
    // descriptor) held until something happened to call `System.gc()`.
    // `run_finalizers` was already called from here; this is the
    // missing half of that symmetry, and lead 2 of
    // `known-issues/direct-memory-still-exhausts-under-sustained-churn-20260805.md`.
    // Both are cheap no-ops when nothing is pending.
    run_cleaner_actions(shared, thread);
    // gen r4w3/obs2: queued GC notifications (see `run_gc_notifications`).
    // gc-common w4-a: with the reference-delivery thread serving, listeners run
    // there (its batch ends with `run_gc_notifications`), not on this
    // allocating mutator — `handoff-w3d-gc-notifications-on-the-delivery-thread`.
    if !gc_notifications_go_to_delivery_thread(shared, thread) {
        run_gc_notifications(shared, thread, false);
    }
}

/// Kill switch of [`maybe_gc`]'s refill-driven occupancy poll. `true` asks the
/// two occupancy triggers after every interpreted allocation again, as before
/// round i1 wave 20 — a `const`, so the two arms can be priced in one source
/// tree without an environment variable.
const FULL_GC_POLL_AFTER_EVERY_NEW: bool = false;

/// Bytes this thread may allocate between two occupancy polls when neither a
/// TLAB refill nor an allocation outside the TLAB intervenes.
///
/// The backstop for the cases the refill signal does not cover: a thread that
/// lives inside one large TLAB while OTHER threads (compiled or native
/// allocators whose own triggers are rate-limited) fill the young generation,
/// and a trigger whose threshold moves without any allocation (Generational
/// picks the larger non-moving threshold while a JIT frame is on the stack).
/// Collection timing moves by at most this much of this thread's own
/// allocation; at 16 KiB that is a few hundred small objects, while the
/// per-allocation cost the poll skips is paid once instead of on each.
const GC_OCCUPANCY_POLL_BYTES: u64 = 16 * 1024;

/// Whether [`maybe_gc`] must ask the occupancy triggers now: the countdown
/// ([`JvmThread::gc_occupancy_poll_at`], in this thread's allocated-bytes
/// units) has run out or was reset by a refill / shared allocation.
///
/// `Tlab::thread_allocated_bytes` is monotone (the carried total plus the live
/// buffer's consumption, JIT inline bumps included), so the comparison is a
/// byte countdown that needs no per-allocation store.
#[inline(always)]
fn occupancy_poll_due(thread: &JvmThread) -> bool {
    FULL_GC_POLL_AFTER_EVERY_NEW
        || thread.tlab.thread_allocated_bytes() >= thread.gc_occupancy_poll_at
}

/// Start the next countdown of [`GC_OCCUPANCY_POLL_BYTES`] from this thread's
/// current allocated-bytes total.
#[inline]
fn arm_occupancy_poll_countdown(thread: &mut JvmThread) {
    thread.gc_occupancy_poll_at = thread
        .tlab
        .thread_allocated_bytes()
        .saturating_add(GC_OCCUPANCY_POLL_BYTES);
}

/// This thread just did something that moves the heap's occupancy — carved a
/// TLAB chunk or allocated outside its TLAB — so the next [`maybe_gc`] asks the
/// occupancy triggers. A store to a field of the thread's own struct.
#[inline(always)]
fn note_occupancy_may_have_moved(thread: &mut JvmThread) {
    thread.gc_occupancy_poll_at = 0;
}

#[cfg(test)]
mod i20_l4_occupancy_poll_tests {
    //! Round i1 wave 20, lane L4: `maybe_gc` asks the occupancy triggers after
    //! a refill, after a shared-heap allocation, or once the byte countdown
    //! runs out — and the `gc_requested` latch on every call.

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;
    use std::sync::atomic::Ordering;

    fn gen_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    /// A thread that never polled polls at its first allocation.
    #[test]
    fn a_fresh_thread_polls_the_occupancy_triggers() {
        let thread = JvmThread::new(ThreadId(0), "i20-l4-fresh");
        assert!(occupancy_poll_due(&thread));
    }

    /// A refill resets the countdown; a poll re-arms it; TLAB hits inside it
    /// skip the occupancy triggers until this thread has allocated
    /// `GC_OCCUPANCY_POLL_BYTES` more.
    #[test]
    fn tlab_hits_skip_the_occupancy_poll_until_the_countdown_runs_out() {
        if FULL_GC_POLL_AFTER_EVERY_NEW {
            return;
        }
        let shared = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "i20-l4-countdown");
        arm_occupancy_poll_countdown(&mut thread);
        let refills_before = shared.mem.tlab_refill_count.load(Ordering::Relaxed);
        let _first = gc_alloc_object(&shared, &mut thread, ClassId::new(0), 2)
            .expect("an empty young generation serves the first object");
        assert_ne!(
            shared.mem.tlab_refill_count.load(Ordering::Relaxed),
            refills_before,
            "the first allocation of a thread refills its TLAB"
        );
        assert!(occupancy_poll_due(&thread), "a refill resets the countdown");
        maybe_gc(&shared, &mut thread);
        assert!(!occupancy_poll_due(&thread), "a poll re-arms the countdown");
        let armed_at = thread.gc_occupancy_poll_at;
        let refills_at_arm = shared.mem.tlab_refill_count.load(Ordering::Relaxed);
        let mut crossed = false;
        for _ in 0..10_000 {
            let _o = gc_alloc_object(&shared, &mut thread, ClassId::new(0), 2)
                .expect("young has room for a few hundred small objects");
            if shared.mem.tlab_refill_count.load(Ordering::Relaxed) != refills_at_arm {
                // A buffer smaller than the countdown: the refill is the poll.
                assert!(occupancy_poll_due(&thread), "a refill resets the countdown");
                return;
            }
            if thread.tlab.thread_allocated_bytes() >= armed_at {
                assert!(occupancy_poll_due(&thread), "the countdown ran out");
                crossed = true;
                break;
            }
            assert!(
                !occupancy_poll_due(&thread),
                "a TLAB hit inside the countdown skips the occupancy poll"
            );
        }
        assert!(
            crossed,
            "10k small objects allocate more than the countdown"
        );
    }

    /// An allocation outside the TLAB moves the occupancy and resets the
    /// countdown.
    #[test]
    fn a_shared_heap_allocation_resets_the_countdown() {
        let shared = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "i20-l4-shared");
        arm_occupancy_poll_countdown(&mut thread);
        assert!(FULL_GC_POLL_AFTER_EVERY_NEW || !occupancy_poll_due(&thread));
        let _o = alloc_object_shared(&shared, &mut thread, ClassId::new(0), 3)
            .expect("an empty heap has room for one object");
        assert!(occupancy_poll_due(&thread));
    }

    /// The `gc_requested` latch is not an occupancy trigger: an armed
    /// countdown does not delay it.
    #[test]
    fn the_request_latch_is_taken_inside_the_countdown() {
        let shared = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "i20-l4-latch");
        arm_occupancy_poll_countdown(&mut thread);
        let cycles_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        shared.mem.gc_requested.store(true, Ordering::Relaxed);
        maybe_gc(&shared, &mut thread);
        assert!(
            !shared.mem.gc_requested.load(Ordering::Relaxed),
            "the latch is consumed"
        );
        assert!(
            shared.mem.gc_cycle_count.load(Ordering::Relaxed) > cycles_before,
            "the requested collection ran"
        );
    }
}

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
/// # `--jdk-only` (gc-common w6-d, 2026-09-24)
///
/// A `--jdk-only` VM takes this path under the default policy too
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
/// (`docs/internal/gc/common-w4d-proposal-full-reference-delivery-by-default-REJECTED-20260928.md`). The
/// hand-off is marked for the delivery thread
/// (`FinalizerThread::note_gc_notifications_handed_off`) so its batch
/// delivers ONLY what a lock-holding door handed it, never what a lock-free
/// door is about to deliver itself. Nothing waits for it, `System.gc()`
/// included: HotSpot's delivery is asynchronous for every cause, and a wait
/// would stall a lock-holding `System.gc()` whenever the listener needs a
/// `java.util.concurrent` lock the caller holds (undetectable, so the full
/// 2-s bound). The kill switch is the existing inline arm,
/// `CRATONVM_FINALIZER_THREAD=0` ([`GcNotificationDelivery::Inline`]).
fn gc_notifications_go_to_delivery_thread(shared: &SharedVm, thread: &JvmThread) -> bool {
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
fn gc_notifications_handed_off_under_lock(shared: &SharedVm, thread: &JvmThread) -> bool {
    if !thread_holds_user_locks(shared, thread) {
        return false;
    }
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
enum GcNotificationDelivery {
    /// The door that collected, always (`CRATONVM_FINALIZER_THREAD=0`, the
    /// bisection arm and the kill switch of both fixes).
    Inline,
    /// The delivery thread when the collecting thread holds a user-visible
    /// lock, the door otherwise (a `--compatible` VM under the default
    /// policy, gc-common w7-d).
    WhenLockHeld,
    /// The delivery thread, always (the full hand-off, or a `--jdk-only` VM
    /// under the default policy, gc-common w6-d).
    OffMutator,
}

/// This VM's [`GcNotificationDelivery`]. Per VM, from `SharedVm::config`,
/// never a process flag.
fn gc_notification_delivery(shared: &SharedVm) -> GcNotificationDelivery {
    gc_notification_delivery_for(finalizer_delivery_policy(), shared.config.is_jdk_only())
}

/// The rule of [`gc_notification_delivery`], the same shape as
/// [`queue_wake_ups_recorded_for`]: under the full hand-off always off the
/// mutator; under the default policy off the mutator for a `--jdk-only` VM
/// and, for a `--compatible` one, only when the collecting thread holds a
/// user-visible lock; never for the `=0` inline bisection arm. gc-common w6-d
/// (2026-09-24) for the strict arm, w7-d for the `--compatible` one; see
/// [`gc_notifications_go_to_delivery_thread`] for why both are bug fixes.
fn gc_notification_delivery_for(
    policy: FinalizerDelivery,
    jdk_only: bool,
) -> GcNotificationDelivery {
    match policy {
        FinalizerDelivery::Always => GcNotificationDelivery::OffMutator,
        FinalizerDelivery::WhenLockHeld if jdk_only => GcNotificationDelivery::OffMutator,
        FinalizerDelivery::WhenLockHeld => GcNotificationDelivery::WhenLockHeld,
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
/// cycle (`docs/internal/gc-common-round-20260923/common-w2a-finalizers-queued-by-forced-collections-wait-for-another-door-FIXED-20260923.md`).
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
fn drain_ref_work_queued_elsewhere(shared: &SharedVm, thread: &mut JvmThread) {
    if !shared.mem.gc_barrier.ref_work_queued() {
        return;
    }
    drain_ref_work_queued_elsewhere_cold(shared, thread);
}

/// The raised-hint half of [`drain_ref_work_queued_elsewhere`], out of line so
/// the per-allocation check stays one load.
#[cold]
#[inline(never)]
fn drain_ref_work_queued_elsewhere_cold(shared: &SharedVm, thread: &mut JvmThread) {
    // Checks the flag first (latched), so flag-off behaviour is unchanged.
    // A woken batch ends with `run_gc_notifications`, so queued notifications
    // ride on it.
    if finalizer_thread_takes_delivery(shared, thread) {
        let _ = shared.mem.gc_barrier.take_ref_work_queued();
        return;
    }
    if crate::jit::helpers::is_jit_thread_set() {
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
/// (`docs/internal/gc-common-round-20260923/common-e-proposal-one-gc-door-FIXED-20260923.md`
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
fn run_collection_pause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    door: GcDoor,
) -> Option<CollectedPause> {
    // DBG (CRATONVM_DBG_MTROOTS) reason code: 1=System.gc, 2=alloc-young,
    // 3=forced-alloc — the numbering `mtroots_set_gc_ctx` documents.
    let mtroots_reason: u8 = match door {
        GcDoor::SystemGc | GcDoor::MetadataThreshold => 1,
        GcDoor::AllocationThreshold => 2,
        GcDoor::AllocationFailure => 3,
    };
    let initiator = thread.thread_id;
    // xt-hardening (2026-07-03): the counted alive set's OS tids are
    // snapshotted atomically with the expected computation (same closure,
    // same barrier lock) for identity-based takeover excusal.
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let mut census_alive: usize = 0;
    let mut census_initiator_counted: Option<bool> = None;
    let won = shared.mem.gc_barrier.request_stw_opening_cycle(
        initiator,
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
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STW_EXPECTED_IDS").is_some() {
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
    let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
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
    // Collect roots: current thread + all snapshots + shared state.
    //
    // gc-common w6-a: `collect_roots_registry_appended` because the registry's
    // snapshots are appended just below -- they push every alive thread's
    // mirror, which `collect_roots` step 10b would otherwise push again
    // (`common-b-proposal-dedupe-the-initiator-root-set.md`).
    let mut roots = crate::memory::roots::collect_roots_registry_appended(shared, thread);
    let initiator_root_count = roots.len(); // gen r4w6/oldpin6, see its publication below
    let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
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
    // INT-3 (G1) — everything a frozen peer can address must not
    // move; must follow collect_roots (which clears the pins).
    pin_frozen_peer_roots_for_g1(shared, &xt_roots, &taken);
    // BUG-03 — conservative roots from forcibly-stopped in-JIT peers.
    roots.extend(xt_roots);

    // STW invariant: the barrier's quota is met, so every other mutator has
    // parked at its safepoint poll OR (BUG-03) been forcibly stopped in JIT
    // and conservatively scanned.
    // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
    weakref_null_referents_pre_gc(shared);
    // gc-common w8-c: watch the JNI weak globals' referents too, so the
    // post-collection verdict below is exact on a cycle that reclaims old-gen
    // storage. AFTER `weakref_null_referents_pre_gc`, which replaces the set.
    crate::native::jni::watch_weak_global_referents(shared);
    // SAFETY: the STW invariant stated just above holds — every other
    // mutator is parked at a safepoint (or was forcibly stopped and
    // conservatively scanned), so this thread is the only mutator and
    // may assert exclusive heap access.
    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
    // Snapshotted INSIDE the pause, immediately before the collection: a
    // finalizable object another mutator registered (and dropped) after an
    // earlier snapshot would otherwise be freed instead of resurrected, and
    // its `finalize()` would never run (`handoff-d-force-gc-finalizable-roots-after-stw`).
    let fin_roots = finalizable_roots(shared);
    publish_old_pin_root_layout_for_pause(shared, thread, initiator_root_count, roots.len());
    let (result, dead_finalizers) = {
        // gen r4w6/oomjit6: `CRATONVM_DBG=oldmark-root-census` only (both are
        // `None` otherwise). The sections name every root by the part of this
        // door that pushed it; the door scope lets the census's VM labeller
        // read this thread and this VM for the length of the collection.
        // Scoped to the call so `thread` is not written while the labeller may
        // hold a pointer to it (`OldmarkCensusDoor::enter`'s contract).
        let _census_sections = oldmark_census_stage_sections(census_collect_end, census_snapshot_end);
        let _census_door = crate::jit::helpers::OldmarkCensusDoor::enter(shared, thread);
        // HIB-CV-24 (ZGC manifestation): arm the class-unload reconciliation
        // watch with this VM's current defining-loader addresses BEFORE the
        // collection, so ZGC can capture their exact pre-slide mark-bitmap
        // verdict (see `zgc_capture_reconcile_watch`) instead of leaving
        // `process_references_after_gc` to judge them post-slide via the
        // compaction-aliasable `is_addr_live`. No-op on Generational/G1.
        shared.mem.heap.zgc_set_reconcile_watch(
            cratonvm_native_builtins::classloader::watched_defining_loader_addrs(
                shared.vm_identity,
            ),
        );
        shared.mem.heap.collect_garbage_with_finalizers(
            &stw,
            &mut roots,
            &fin_roots,
            &shared.threads.monitors,
        )
    };
    let reconcile_exact_results = shared.mem.heap.zgc_take_reconcile_results();
    cratonvm_gc::gen_heap::clear_old_pin_root_layout();
    // gc-common w6-d: soft/weak processing must not count the objects this
    // collection marked only to run their `finalize()` (HotSpot clears a weak
    // reference to a finalizable object before finalizing it).
    note_resurrected_finalizables_for_reference_processing(
        shared,
        &fin_roots,
        &result.pointer_map,
        &dead_finalizers,
    );
    process_references_after_gc(
        shared,
        &result.pointer_map,
        &roots,
        reconcile_exact_results.as_ref(),
    );
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
        // NULL).
        let in_place = crate::memory::addr_keyed::InPlaceVerdict::capture_after(
            &shared.mem.heap,
            &result.pointer_map,
        );
        let _ =
            crate::native::jni::sweep_weak_global_refs_with(shared, &result.pointer_map, &|addr| {
                in_place.survived(addr)
            });
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
    update_all_roots(shared, thread, &result.pointer_map);
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
    gc_event_seal(shared, &mut gc_event);
    // gc-common w7-g (`common-e-small-findings` item 7): count a completed
    // `System.gc()`-door collection BEFORE the release, so a `System.gc()`
    // caller that lost this pause's request and took part in it reads the
    // bump when `complete_gc`'s generation release wakes it, and coalesces
    // (`force_gc_from_native`, `system_gc_completed_since`).
    if matches!(door, GcDoor::SystemGc | GcDoor::MetadataThreshold) {
        system_gc_collections(shared).fetch_add(1, std::sync::atomic::Ordering::Release);
    }
    // gc-common w5-a: the allocation-failure door's productivity figures,
    // sampled here for the same reason -- read after the release they
    // included whatever the released mutators allocated (TLAB refills first),
    // understating what this collection freed and feeding the GC-overhead
    // streak toward a spurious `OutOfMemoryError` on a busy multi-threaded
    // heap. See `note_gc_productivity`.
    let productivity_after = (door == GcDoor::AllocationFailure).then(|| {
        (
            shared.mem.heap.live_bytes_estimate(),
            shared.mem.heap.bytes_promoted_total(),
        )
    });
    retire_skip_spans_and_resume(shared, taken);
    // Signal all threads with the pointer map
    shared.mem.gc_barrier.complete_gc(result.pointer_map);
    // T19.3.G1 cycle counter, the JFR GC events and the `-Xlog:gc` line — one
    // call, shared by every door. See `gc_event_finish`.
    gc_event_finish(shared, door, gc_event);
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
    Some(CollectedPause { productivity_after })
}

/// A collection [`run_collection_pause`] initiated and completed (gc-common
/// w5-a).
#[derive(Clone, Copy, Debug)]
struct CollectedPause {
    /// `(live_bytes_estimate, bytes_promoted_total)` sampled INSIDE the pause,
    /// after the collection and before any mutator resumed. Only the
    /// allocation-failure door asks for it (its productivity metric,
    /// [`note_gc_productivity`]); `None` on the other doors.
    productivity_after: Option<(usize, u64)>,
}

/// How many completed pauses the ref-work hint must survive untaken, with
/// finalizers still queued from before the latest collection, before
/// [`hand_undrained_finalizers_to_delivery_thread`] acts. Two, not one: a
/// `finalize()` that allocates can collect in the middle of the inline drain
/// that is emptying the queue, and that nested collection sees the previous
/// pause's hint and a non-empty queue; needing a second whole pause keeps an
/// interpreted program's inline drain its own.
const UNDRAINED_REF_WORK_PAUSES: u64 = 2;

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
fn oldmark_census_stage_sections(
    collect_end: usize,
    snapshot_end: usize,
) -> Option<cratonvm_gc::gen_heap::OldmarkCensusStagedSections> {
    if !cratonvm_gc::gen_heap::oldmark_census_enabled() {
        return None;
    }
    Some(cratonvm_gc::gen_heap::stage_oldmark_census_root_sections(
        oldmark_census_sections(collect_end, snapshot_end, crate::memory::roots::scan_section_of),
    ))
}

/// The section list [`oldmark_census_stage_sections`] stages, with the
/// per-index step lookup passed in so it is testable without a collection.
/// `step_of` answers `"<marks-off>"` when no marks were recorded, and
/// `"<before-first-section>"` for the activation loaders `collect_roots`
/// pushes before its first mark.
fn oldmark_census_sections(
    collect_end: usize,
    snapshot_end: usize,
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
    sections.push((collect_end, "registry snapshots (every alive thread, this one included)"));
    sections.push((snapshot_end, "xt frozen in-JIT peers (conservative)"));
    sections
}

#[cfg(test)]
mod oldmark_census_section_tests {
    use super::oldmark_census_sections;

    fn marks_off(_: usize) -> &'static str {
        "<marks-off>"
    }

    fn two_steps(i: usize) -> &'static str {
        match i {
            0 => "<before-first-section>",
            1..=2 => "1: Thread frames",
            _ => "2: Static fields",
        }
    }

    /// gen r4w6/oomjit6: without marks the door still names its three parts;
    /// with marks the first part is split per `collect_roots` step, and the
    /// activation loaders pushed before the first mark get a name of their own.
    #[test]
    fn the_door_sections_name_every_part_of_the_root_vector() {
        let s = oldmark_census_sections(5, 9, marks_off);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].0, 0);
        assert_eq!((s[1].0, s[2].0), (5, 9));

        let s = oldmark_census_sections(5, 9, two_steps);
        let starts: Vec<usize> = s.iter().map(|&(i, _)| i).collect();
        assert_eq!(starts, vec![0, 1, 3, 5, 9]);
        assert!(s[0].1.starts_with("0: activation"));
        assert_eq!(s[1].1, "1: Thread frames");
        assert_eq!(s[2].1, "2: Static fields");

        // An empty `collect_roots` part does not ask for marks at index 0.
        let s = oldmark_census_sections(0, 0, two_steps);
        assert_eq!(s.len(), 3);
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
fn hand_undrained_finalizers_to_delivery_thread(shared: &SharedVm) {
    if finalizer_delivery_policy() != FinalizerDelivery::WhenLockHeld {
        return;
    }
    let _served = request_finalizer_thread(shared, true);
}

/// gen r4w6/oldpin6 (2026-09-24) — publish, for the collection this thread is
/// about to run, which roots a pinned old-gen compaction must pin
/// (`cratonvm_gc::gen_heap::OldPinRootLayout`; oldcompact5's root-gatherer
/// request, `docs/internal/reviews/gengc-round4-w5-oldcompact5-20260924.md`).
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
fn publish_old_pin_root_layout_for_pause(
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
        || cratonvm_gc::gc_quiescence::unregistered_jit_frame_on_stack();
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
fn debug_heap_walks_permitted(shared: &SharedVm, solo: bool) -> bool {
    if !debug_heap_walkers_armed() {
        return false;
    }
    if solo {
        return true;
    }
    let tails = shared
        .threads
        .thread_registry
        .collect_reserved_tlab_tails();
    if tails.is_empty() {
        return true;
    }
    note_debug_heap_walk_skipped(tails.len());
    false
}

/// Either debug heap walker's own gate. Read per pause, exactly as the walkers
/// read them themselves.
fn debug_heap_walkers_armed() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_VALIDATE_NEW").is_some()
        || cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_HEAP_STALE").is_some()
}

/// A pause whose debug heap walks were skipped: say so, at most eight times a
/// process -- a verifier that is silent because it did not run must not read
/// as a verifier that found nothing.
#[cold]
fn note_debug_heap_walk_skipped(unretired_tails: usize) {
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
fn post_gc_debug_verifiers(
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
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_CORRUPT_FRAMES").is_some()
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
pub(super) fn self_call_identity_stable(shared: &SharedVm, class_id: ClassId) -> bool {
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
/// sibling module and the cycle opener is `pub(super)`. See
/// `jit_maybe_start_zgc_concurrent_mark` for why the JIT needs its own call
/// site at all -- a fully compiled allocation loop reaches `maybe_gc` never.
pub fn zgc_concurrent_mark_cycle_pub(shared: &SharedVm, thread: &mut JvmThread) {
    zgc_concurrent_mark_cycle(shared, thread);
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

/// Allocate a dynamically-produced `java.lang.String` under the SAME
/// heap-exhaustion contract as `new`: collect, retry, and finally raise a
/// catchable `OutOfMemoryError` -- never abort the process.
///
/// `vm_object::create_java_string_uninterned` cannot do this: it takes only a
/// `&SharedVm`, and every GC entry point needs the calling thread (to retire
/// its TLAB and contribute its roots). So its exhaustion path was a bare
/// `eprintln!` + `std::process::abort()`, which turned a plain `"a" + b` on a
/// full heap into an un-catchable VM kill. HotSpot throws
/// `OutOfMemoryError: Java heap space` there, and a Java program is entitled to
/// catch it. Reproduced with a 25-line probe under `-Xmx64m -XX:+UseG1GC`:
/// `FATAL: heap exhausted allocating java/lang/String (46 units)` (from the
/// `System.out.println("iter=" + i)` in the allocation loop) followed by
/// SIGABRT, where HotSpot reports `OutOfMemoryError` and keeps running.
///
/// Test-only since gc-common w2 (2026-09-23): no production caller remains
/// (none at the round base either); the UTF-16 twin below is the live entry.
#[cfg(test)]
pub(crate) fn create_string_or_oom(
    shared: &SharedVm,
    thread: &mut JvmThread,
    text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    use crate::vm::try_create_java_string_uninterned as try_new_string;
    if let Some(obj) = try_new_string(shared, text) {
        return Ok(obj);
    }
    // The escalation every interpreter ladder shares — forced GC, the
    // GC-overhead limit, retry, G1's last-ditch complete mark cycle, retry.
    // See `collect_and_retry`.
    if let Some(obj) =
        collect_and_retry(shared, thread, "create-string", |s| try_new_string(s, text))
    {
        return Ok(obj);
    }
    maybe_dump_heap_on_oom(shared, thread);
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::OutOfMemoryError {
            // `text.len()` is the UTF-8 byte length, not a char count; say so.
            // The parenthesised site detail never reaches Java -- see
            // `RuntimeError::as_java_throwable`.
            message: format!("Java heap space (String of {} UTF-8 bytes)", text.len()),
        },
    )))
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
/// `pub(super)` so `interpreter.rs`'s `multianewarray` shares it
/// (`alloc_multi_array_collecting`, on this helper since gc-common w3-c).
pub(super) fn collect_and_retry<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    mut attempt: impl FnMut(&SharedVm) -> Option<T>,
) -> Option<T> {
    collect_and_retry_with_thread(shared, thread, site, |s, _| attempt(s))
}

/// [`collect_and_retry`] for an attempt that needs the calling thread too (a
/// `NativeContextImpl` is built from it). The one escalation; the thread-free
/// form above delegates here.
fn collect_and_retry_with_thread<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    mut attempt: impl FnMut(&SharedVm, &mut JvmThread) -> Option<T>,
) -> Option<T> {
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    maybe_gc_forced_at(shared, thread, site);
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
        if shared.mem.heap.is_g1() {
            last_ditch_reclaim(shared, thread);
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
        match overhead_limit_exit(
            judge_major,
            major_ran,
            collected,
            judge_major && major_ran && gc_overhead_limit_exceeded(shared),
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
            // gcd d10/o: on G1 a failed attempt here is not yet a verdict --
            // no collection so far reached a dead Old region. One full cycle
            // (`g1_overhead_limit_full_cycle`) decides: productive, one more
            // attempt; futile, the error. Generational under
            // `CRATONVM_GC_OVERHEAD_PROGRESS=0` and ZGC (whose forced cycle is
            // whole-heap) keep the one attempt byte for byte, and so does G1
            // under that switch (the helper declines).
            OverheadLimitExit::Attempt => {
                let first = attempt(shared, thread);
                if first.is_some() || !shared.mem.heap.is_g1() {
                    return first;
                }
                let before_live = shared.mem.heap.live_bytes_estimate();
                return if g1_overhead_limit_full_cycle(shared, thread, before_live) {
                    attempt(shared, thread)
                } else {
                    None
                };
            }
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
fn last_ditch_then_second_major<T>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    attempt: &mut impl FnMut(&SharedVm, &mut JvmThread) -> Option<T>,
) -> Option<T> {
    last_ditch_reclaim(shared, thread);
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
/// ladder requests (`gcd-d4j-stw-major-keeps-dead-old-data-through-young-nepotism-20260928.md`).
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
const OOME_DEBT_PAYMENTS: u64 = 2;

/// gcd d5/q: the key a debt word stores for `thread`: its id plus one (so `0`
/// stays "nothing owed"), shifted over the two payment bits. Ids at or above
/// 2^62 alias, which only makes a debt payable by an aliasing thread.
fn oome_debt_key(thread: &JvmThread) -> u64 {
    thread.thread_id.0.wrapping_add(1) << 2
}

/// gcd d4/j: record that a heap `OutOfMemoryError` (`message` = its
/// Java-visible text) is being raised, so the native funnel collects before
/// its next callback. `Requested array size exceeds VM limit` and every other
/// non-heap message are not recorded: no collection helps them. Generational
/// only (the only backend whose funnel reads it).
///
/// gcd d5/q: the debt now names the RAISING thread and carries
/// [`OOME_DEBT_PAYMENTS`] payments; a later raise (any thread) replaces it.
pub(crate) fn note_heap_oome_raised(shared: &SharedVm, thread: &JvmThread, message: &str) {
    if message.starts_with("Java heap space") {
        // gcd d9/b: `[GC] oome_ladder: ladder_oomes` (whatever the switch).
        ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::OOMES, 1);
    }
    if message.starts_with("Java heap space")
        && shared.mem.heap.is_generational()
        && gc_overhead_progress_enabled()
    {
        let key = oome_debt_key(thread);
        if key != 0 {
            shared
                .mem
                .alloc_ladder
                .oome_native_debt
                .store(key | OOME_DEBT_PAYMENTS, std::sync::atomic::Ordering::Relaxed);
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
fn oome_debt_decision(word: u64, key: u64, recovered: bool) -> (bool, u64) {
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
    // Never armed off Generational or with the switch off
    // (`note_heap_oome_raised`); defensive for a switch flipped in between.
    if !shared.mem.heap.is_generational() || !gc_overhead_progress_enabled() {
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
    }
    pay && taken
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
enum OverheadLimitExit {
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
/// (`docs/internal/gc/gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md`,
/// mechanism B).
fn overhead_limit_exit(
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
const LATCHED_GRACE_ATTEMPTS: u32 = 8;

/// gce e2/o: may the latched GC-overhead exit try the allocation once more
/// before throwing? Takes one unit of this episode's budget
/// ([`LATCHED_GRACE_ATTEMPTS`]) when it answers yes. `CRATONVM_GC_LATCHED_GRACE=0`
/// answers no (the gen r4w5 exit byte for byte). Counted on `[GC] oome_ladder:`
/// (`ladder_latched_grace=`).
pub(crate) fn latched_overhead_grace(shared: &SharedVm) -> bool {
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_LATCHED_GRACE") {
        return false;
    }
    let granted = latched_grace_take(&shared.mem.alloc_ladder.latched_grace_used);
    if granted {
        ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::LATCHED_GRACE, 1);
    }
    granted
}

/// The budget arithmetic of [`latched_overhead_grace`], pure over the counter.
fn latched_grace_take(used: &std::sync::atomic::AtomicU32) -> bool {
    use std::sync::atomic::Ordering;
    used.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        (n < LATCHED_GRACE_ATTEMPTS).then_some(n + 1)
    })
    .is_ok()
}

/// gce e2/o: count a graced attempt that allocated (`ladder_latched_grace_ok=`).
pub(crate) fn note_latched_grace_outcome(shared: &SharedVm, allocated: bool) {
    if allocated {
        ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::LATCHED_GRACE_OK, 1);
    }
}

/// gce e2/o: a reset streak ends the latched episode and refills the grace.
fn reset_latched_grace(shared: &SharedVm) {
    shared
        .mem
        .alloc_ladder
        .latched_grace_used
        .store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
mod gce_e2o_latched_grace_tests {
    //! gce e2/o: the latched overhead exit's bounded grace.
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Eight graced attempts per episode, then none; a reset refills it.
    #[test]
    fn the_grace_is_bounded_per_latched_episode() {
        let used = AtomicU32::new(0);
        for _ in 0..LATCHED_GRACE_ATTEMPTS {
            assert!(latched_grace_take(&used));
        }
        assert!(!latched_grace_take(&used), "the budget is spent");
        assert!(!latched_grace_take(&used));
        assert_eq!(used.load(Ordering::Relaxed), LATCHED_GRACE_ATTEMPTS);
        used.store(0, Ordering::Relaxed);
        assert!(latched_grace_take(&used), "a reset streak refills it");
    }

    /// Per VM, reset by a productive judged cycle, and `=0` turns it off.
    #[test]
    fn the_grace_is_per_vm_reset_with_the_streak_and_switchable() {
        use crate::config::{GcAlgorithm, VmConfig};
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let other = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_GC_LATCHED_GRACE", None)], || {
            for _ in 0..LATCHED_GRACE_ATTEMPTS {
                assert!(latched_overhead_grace(&shared));
            }
            assert!(!latched_overhead_grace(&shared));
            assert!(latched_overhead_grace(&other), "another VM's budget is its own");
            // A productive judged cycle (a big drop on a heap with room).
            shared.mem.gc_unproductive_streak.store(9, Ordering::Relaxed);
            note_gc_productivity(&shared, 64 << 20, 0, Some((0, 0)));
            assert_eq!(shared.mem.gc_unproductive_streak.load(Ordering::Relaxed), 0);
            assert!(latched_overhead_grace(&shared), "the reset refilled the budget");
        });
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_LATCHED_GRACE", Some("0"))],
            || assert!(!latched_overhead_grace(&other), "`=0` is the old exit"),
        );
    }
}

#[cfg(test)]
mod r5w5_overhead_exit_tests {
    //! gen r5w5/sizer9: the overhead-limit arm's exit.
    use super::*;

    /// gen r4w5/thrash5's guarantee is kept: a full collection this call ran
    /// that leaves the streak latched is the error, attempted or not.
    #[test]
    fn a_major_that_leaves_the_streak_latched_still_throws() {
        for soft in [false, true] {
            assert_eq!(overhead_limit_exit(true, true, soft, true), OverheadLimitExit::Throw);
        }
    }

    /// A major that reset the streak, or a soft-reference collection, earns
    /// the one attempt it always did.
    #[test]
    fn a_productive_collection_earns_the_attempt() {
        assert_eq!(overhead_limit_exit(true, true, false, false), OverheadLimitExit::Attempt);
        assert_eq!(overhead_limit_exit(true, false, true, false), OverheadLimitExit::Attempt);
    }

    /// No collection on this thread in a judged arm: the verdict is not
    /// honoured on young cycles alone — finish the ladder.
    #[test]
    fn no_full_collection_means_no_verdict() {
        assert_eq!(
            overhead_limit_exit(true, false, false, false),
            OverheadLimitExit::AttemptThenLastDitch
        );
    }

    /// `CRATONVM_GC_OVERHEAD_PROGRESS=0` (and every non-Generational heap):
    /// the historical exit, whatever ran.
    #[test]
    fn the_unjudged_arm_is_unchanged() {
        for (major, soft, latched) in [(false, false, false), (true, false, true), (false, true, false)] {
            assert_eq!(
                overhead_limit_exit(false, major, soft, latched),
                OverheadLimitExit::Attempt
            );
        }
    }
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
        if let Some(v) = collect_and_retry_with_thread(shared, thread, site, &mut catching) {
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

/// gcd d10/o (2026-09-28): the shared allocation ladder in the shape a
/// NATIVE can use -- collect, then answer whether its ONE retry is worth
/// making. The body of `NativeContextImpl::reclaim_before_alloc_retry`
/// (`vm/src/vm/vm_exec.rs`), which every collecting native factory calls
/// after a refused `try_new_array` / `try_new_ref_array` / `try_alloc_object`
/// / `init_string_from_units` (`ArrayList(int)` and its `add` grow,
/// `String.toCharArray`, `StringBuilder(int)`, `ByteBuffer.allocate`, the
/// `Arrays.copyOf` family, ...). Same precondition as that trait method: the
/// caller holds no unpinned `ObjectRef`.
///
/// # Why a second body
///
/// `reclaim_before_alloc_retry` carried its own copy of the ladder, written
/// before gen r4w4 and never brought along, so a native factory decided an
/// `OutOfMemoryError` on less than `new` / `newarray`
/// ([`collect_and_retry_with_thread`]) and the JIT helpers do:
///
/// * **latched overhead streak** -- it returned the soft-reference rung's
///   answer, so with no live `SoftReference` it threw after the forced
///   collection alone, without even the retry that collection may have made
///   room for: on Generational no major reached the old generation (where a
///   program's dropped data sits after an `OutOfMemoryError`), on G1 no
///   marking cycle reached a dead Old region;
/// * **otherwise** -- one `last_ditch_reclaim`, where the interpreter and the
///   JIT helpers have run `second_major_before_oome` since gcd d4/j (one
///   Generational major is no verdict: young->old nepotism).
///
/// # The ladder
///
/// A native cannot attempt between the rungs, so the rungs run back to back
/// and the retry follows the last one:
///
/// * not latched: the forced collection, `last_ditch_reclaim`, the second
///   major; retry;
/// * latched: the soft-reference rung, then the interpreter's exit
///   ([`overhead_limit_exit`]): on Generational the deciding majors
///   ([`majors_to_decide_oome`]) and the error when a major this thread ran
///   left the streak latched; on G1 [`g1_overhead_limit_full_cycle`] decides;
///   on ZGC the forced cycle was whole-heap and earns the retry, as it earns
///   the interpreter's one attempt.
///
/// `CRATONVM_GC_OVERHEAD_PROGRESS=0` restores the historical body byte for
/// byte ([`native_reclaim_before_alloc_retry_historical`]).
pub fn native_reclaim_before_alloc_retry(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    if !gc_overhead_progress_enabled() {
        return native_reclaim_before_alloc_retry_historical(shared, thread);
    }
    let before_live = shared.mem.heap.live_bytes_estimate();
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    maybe_gc_forced_at(shared, thread, "vm-exec");
    if !gc_overhead_limit_exceeded(shared) {
        last_ditch_reclaim(shared, thread);
        let _ = second_major_before_oome(shared, thread, "native-oome-second-major");
        return true;
    }
    let collected = last_ditch_clear_soft_refs(shared, thread);
    // `judge_major` of the interpreter's exit: the switch is on here, so it
    // is "Generational".
    let generational = shared.mem.heap.is_generational();
    let major_ran = generational
        && (!collected || gc_overhead_limit_exceeded(shared))
        && majors_to_decide_oome(shared, thread, "native-overhead-limit-major");
    match overhead_limit_exit(
        generational,
        major_ran,
        collected,
        generational && major_ran && gc_overhead_limit_exceeded(shared),
    ) {
        // gce e2/o: the bounded grace, as in `collect_and_retry_with_thread`
        // (the native caller's one retry is the graced attempt).
        OverheadLimitExit::Throw => latched_overhead_grace(shared),
        OverheadLimitExit::Attempt if generational => true,
        OverheadLimitExit::Attempt => {
            !shared.mem.heap.is_g1() || g1_overhead_limit_full_cycle(shared, thread, before_live)
        }
        OverheadLimitExit::AttemptThenLastDitch => {
            last_ditch_reclaim(shared, thread);
            let _ = second_major_before_oome(shared, thread, "native-oome-second-major");
            true
        }
    }
}

/// The body `NativeContextImpl::reclaim_before_alloc_retry` had before gcd
/// d10/o, kept verbatim for `CRATONVM_GC_OVERHEAD_PROGRESS=0`: the forced
/// collection; under a latched overhead streak the soft-reference rung's
/// answer; otherwise `last_ditch_reclaim` and a retry.
fn native_reclaim_before_alloc_retry_historical(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    thread.tlab.retire();
    maybe_gc_forced_at(shared, thread, "vm-exec");
    if gc_overhead_limit_exceeded(shared) {
        return last_ditch_clear_soft_refs(shared, thread);
    }
    last_ditch_reclaim(shared, thread);
    true
}

pub(super) fn maybe_gc_forced(shared: &SharedVm, thread: &mut JvmThread) {
    maybe_gc_forced_at(shared, thread, "unlabelled")
}

/// [`maybe_gc_forced`] with the caller's identity, for the census.
pub(super) fn maybe_gc_forced_at(shared: &SharedVm, thread: &mut JvmThread, site: &'static str) {
    let _ = maybe_gc_forced_collected(shared, thread, site);
}

/// [`maybe_gc_forced_at`], reporting whether THIS thread ran the collection
/// (`false`: it lost the STW race and took part in another thread's pause).
///
/// For a caller whose request is thread-local and therefore only honoured by a
/// collection it initiates itself — see `last_ditch_clear_soft_refs`.
fn maybe_gc_forced_collected(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
) -> bool {
    cratonvm_types::gc_entry_census::note_forced_at(site);
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
    let occupancy_door = site == "tlab-alloc-shaped";
    let wedged_at_entry = occupancy_door && old_gen_wedged_now(shared);
    let t0 = std::time::Instant::now();
    let pause = run_collection_pause(shared, thread, GcDoor::AllocationFailure);
    let collected = pause.is_some();
    if collected {
        use cratonvm_gc::gen_heap::oome_ladder as l;
        if !occupancy_door {
            ladder_census(shared, l::FORCED, 1);
            ladder_census(shared, l::FORCED_US, elapsed_us(t0));
        } else if wedged_at_entry {
            ladder_census(shared, l::THRESHOLD_WEDGED, 1);
            ladder_census(shared, l::THRESHOLD_WEDGED_US, elapsed_us(t0));
        }
    }
    // gcd d10/o, opt-in `CRATONVM_GC_REFILL_TRIGGER_UNJUDGED=1`
    // (`gcd-d9b-compiled-refill-trigger-cycles-feed-the-overhead-streak`): the
    // compiled refill trigger's occupancy cycles are not judged, as the
    // interpreter's occupancy cycles (`maybe_gc`) never are.
    let judged = !(occupancy_door && refill_trigger_cycles_unjudged());
    if let Some(pause) = pause {
        if judged {
            note_gc_productivity(
                shared,
                before_live,
                before_promoted,
                pause.productivity_after,
            );
        } else {
            // gce e1/o: unjudged is not blind. A productive trigger cycle
            // still clears a latched streak (it never raises one), so a
            // program that recovered from an OOME does not carry a stale
            // streak into its next compiled helper entry, which reads it on
            // every entry before collecting and would answer it with the
            // soft-reference rung and a major.
            note_refill_trigger_cycle_reset_only(
                shared,
                before_live,
                before_promoted,
                pause.productivity_after,
            );
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
    if collected && shared.mem.heap.is_g1() && cratonvm_types::flags().gc.g1_alloc_mark_drive {
        g1_drive_mark_cycle(shared, thread, MarkDoor::AllocFail);
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
    // `docs/internal/gc/gengc-r5w1-refs5-concurrent-cycle-never-starts-from-compiled-code-FIXED-20260927.md`.
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
        maybe_concurrent_gc_at(shared, thread, MarkDoor::AllocFail);
    }
    if collected {
        let _ = forced_door_hands_off_gc_notifications(shared, thread);
    }
    collected
}

/// gcd d10/o, `CRATONVM_GC_REFILL_TRIGGER_UNJUDGED=1` (opt-in, default OFF):
/// the compiled refill trigger's cycles (`tlab_alloc_shaped_inner`, site
/// `tlab-alloc-shaped`) skip `note_gc_productivity`, so only allocation
/// FAILURES feed the GC-overhead streak on every path, compiled or
/// interpreted. `docs/known-issues/gc/gcd-d9b-compiled-refill-trigger-cycles-feed-the-overhead-streak-20260928.md`:
/// the trigger takes the forced door on an OCCUPANCY verdict, and on a wedged
/// old generation its young cycles each free what was allocated since, read
/// as unproductive, and latch the streak a compiled program then meets on its
/// next allocation failure, where the same program interpreted
/// (`maybe_gc`'s occupancy cycles are never judged) does not. Every backend
/// runs the trigger (`needs_gc_for_jit_allocation`), so this is the shared
/// overhead accounting, not Generational policy. Opt-in because it moves the
/// JIT's `OutOfMemoryError` timing on a wedged heap; the A/B is on that page.
fn refill_trigger_cycles_unjudged() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_GC_REFILL_TRIGGER_UNJUDGED")
}

/// gce e1/o — the unjudged refill-trigger cycle's one effect on the overhead
/// accounting: it may RESET a latched streak, never raise it.
///
/// Why (the review of the d9/b opt-in for its default flip): the streak is
/// reset only by `note_gc_productivity`, and the JIT's object door
/// (`jit_new_object_body`) reads `gc_overhead_limit_exceeded` on EVERY entry,
/// before any collection of its own; a latched streak there runs the
/// soft-reference rung (a collection that clears every `SoftReference`) and a
/// major. The interpreter's doors read the streak only after their own forced
/// collection has been judged, so a stale streak never reaches them. With the
/// trigger's cycles skipped entirely, a compiled program that caught an OOME,
/// dropped its data and went on allocating would carry the streak through all
/// of its (productive) trigger cycles, and its next helper entry would clear
/// its soft caches for nothing. Judged today, those cycles reset it.
///
/// Same verdict as [`note_gc_productivity`] with no progress window
/// ([`refill_trigger_cycle_resets_streak`]); the window's marks are NOT
/// re-armed (so the next forced collection's window still spans everything
/// allocated since the previous forced one, as for the interpreter's
/// `maybe_gc` cycles), and no census counter moves.
fn note_refill_trigger_cycle_reset_only(
    shared: &SharedVm,
    before_live: usize,
    before_promoted: u64,
    sealed_after: Option<(usize, u64)>,
) {
    use std::sync::atomic::Ordering;
    let cap = shared.mem.heap.heap_capacity();
    if cap == 0 || shared.mem.gc_unproductive_streak.load(Ordering::Relaxed) == 0 {
        return;
    }
    let (after_live, promoted_after) = sealed_after.unwrap_or_else(|| {
        (
            shared.mem.heap.live_bytes_estimate(),
            shared.mem.heap.bytes_promoted_total(),
        )
    });
    let freed = before_live
        .saturating_sub(after_live)
        .saturating_add(promoted_after.saturating_sub(before_promoted) as usize);
    let wedged =
        cratonvm_gc::gen_heap::old_gen_is_wedged(shared.mem.heap.old_gen_headroom(), cap);
    let reset = refill_trigger_cycle_resets_streak(freed, cap, wedged);
    if reset {
        shared.mem.gc_unproductive_streak.store(0, Ordering::Relaxed);
        reset_latched_grace(shared);
    }
    if gc_overhead_dbg_on() {
        eprintln!(
            "[GC_OVERHEAD] unjudged=refill-trigger before={before_live} after={after_live} \
             freed={freed} cap={cap} old_gen_wedged={wedged} reset={reset}"
        );
    }
}

/// The pure verdict of [`note_refill_trigger_cycle_reset_only`]: would
/// [`note_gc_productivity`] call this cycle productive on its freed-bytes and
/// free-space halves alone (no progress window)?
fn refill_trigger_cycle_resets_streak(freed: usize, cap: usize, old_gen_wedged: bool) -> bool {
    !gc_cycle_is_unproductive(freed, cap, old_gen_wedged, None)
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
/// registers a STARTING thread): a `--jdk-only` VM hands off as at every door;
/// a `--compatible` one only when this thread holds a user-visible lock (its
/// lock-free collections keep the inline delivery at the next drain point, as
/// before); the `CRATONVM_FINALIZER_THREAD=0` arm does nothing. `true` when the
/// delivery thread took them.
fn forced_door_hands_off_gc_notifications(shared: &SharedVm, thread: &JvmThread) -> bool {
    gc_notifications_go_to_delivery_thread(shared, thread)
}

// ---------------------------------------------------------------------------
// gcd d2/j (2026-09-27): the futile-young backoff of the object doors
// ---------------------------------------------------------------------------
//
// `docs/internal/gc/gcd-d1b-thread-exit-shape-livelocks-on-forced-young-cycles-FIXED-20260928.md`,
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
const FUTILE_YOUNG_QUANTUM_BASE: u64 = 256 * 1024;

/// The smallest object a door allocates: the entry-counted re-arm is the byte
/// quantum over this, so it never closes the window before the byte count
/// would for a real object, and still closes it for a door whose allocation
/// never reaches `bytes_allocated_total`.
const FUTILE_YOUNG_MIN_OBJECT_BYTES: u64 = 16;

/// Is the futile-young backoff armed for this VM's heap at all?
#[inline]
fn futile_young_backoff_applies(shared: &SharedVm) -> bool {
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
    let skipped = st.futile_young_skipped_since.fetch_add(1, Ordering::Relaxed) + 1;
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
    // `CRATONVM_GC_YOUNG_DRAIN_BLOCKED=0` makes the new term false. BUILD
    // DEPENDENCY on d9/c's `02847aedd` (`young_starved_behind_blocked_drain`).
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
    ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::FUTILE_VERDICTS, 1);
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
fn futile_young_quantum(streak: u32, cap: usize) -> u64 {
    // Widening: usize -> u64 is loss-free on every supported target.
    let cap_bound = (cap as u64 / 50).max(FUTILE_YOUNG_QUANTUM_BASE);
    let shift = streak.saturating_sub(1).min(16);
    (FUTILE_YOUNG_QUANTUM_BASE << shift).min(cap_bound)
}

/// The backoff's window: open (skip the cycle) while less than `quantum`
/// bytes were allocated and fewer than `quantum / 16` door entries skipped
/// since the verdict.
fn futile_young_window_open(quantum: u64, allocated: u64, skipped: u64) -> bool {
    allocated < quantum && skipped < quantum / FUTILE_YOUNG_MIN_OBJECT_BYTES
}

/// `CRATONVM_DBG_GC_OVERHEAD`, read once: the `[GC_OVERHEAD]` lines of
/// [`note_gc_productivity`] and of the futile-young verdicts.
fn gc_overhead_dbg_on() -> bool {
    static GC_OVERHEAD_DBG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GC_OVERHEAD_DBG
        .get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_GC_OVERHEAD").is_some())
}

/// gcd d9/b: add `n` to slot `slot` (`cratonvm_gc::gen_heap::oome_ladder`) of
/// this VM's allocation-failure ladder census, printed at shutdown as
/// `[GC] oome_ladder:` (`--verbose:gc` / `CRATONVM_GC_STATS`). Generational
/// only; per heap, so per VM. A relaxed add, only on the ladder's rungs.
#[inline]
fn ladder_census(shared: &SharedVm, slot: usize, n: u64) {
    if let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap {
        h.note_oome_ladder(slot, n);
    }
}

/// gcd d9/b: is this VM's old generation wedged right now
/// (`gen_heap::old_gen_is_wedged`, the overhead limit's free-space yardstick)?
/// `false` off Generational. One old-generation read; asked once per
/// collection, never per allocation.
fn old_gen_wedged_now(shared: &SharedVm) -> bool {
    shared.mem.heap.is_generational()
        && cratonvm_gc::gen_heap::old_gen_is_wedged(
            shared.mem.heap.old_gen_headroom(),
            shared.mem.heap.heap_capacity(),
        )
}

/// Microseconds since `t0`, saturating (gcd d9/b census).
#[inline]
fn elapsed_us(t0: std::time::Instant) -> u64 {
    u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod gcd_d4j_oome_collection_tests {
    //! gcd d4/j: the native funnel's collection owed after a heap OOME, and
    //! the majors that decide one.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use std::sync::atomic::Ordering;

    fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        })
    }

    /// A heap OOME arms a debt for its raising thread; the VM-limit refusal
    /// arms none; another VM owes nothing; G1 is never armed. gcd d5/q: this
    /// test used to assert that the first funnel call on the fresh heap pays;
    /// a fresh heap has its old generation free (`recovered`), so the debt is
    /// now CLEARED there without a collection -- the paying arms are the pure
    /// `the_debt_is_the_raising_threads_and_is_paid_at_most_twice`.
    #[test]
    fn a_heap_oome_owes_the_funnel_one_collection() {
        let shared = vm(GcAlgorithm::Generational);
        let raiser = JvmThread::new(ThreadId(7), "d4j-raiser");
        let debt = || shared.mem.alloc_ladder.oome_native_debt.load(Ordering::Relaxed);
        assert!(!native_call_owes_oome_major(&shared, &raiser), "nothing owed at start");
        note_heap_oome_raised(&shared, &raiser, ARRAY_SIZE_EXCEEDS_VM_LIMIT);
        assert_eq!(debt(), 0, "the VM limit owes nothing");
        note_heap_oome_raised(&shared, &raiser, "Java heap space");
        if !cratonvm_types::flags().gc.gc_overhead_progress {
            assert_eq!(debt(), 0, "`CRATONVM_GC_OVERHEAD_PROGRESS=0` arms nothing");
            return;
        }
        assert_eq!(debt(), oome_debt_key(&raiser) | OOME_DEBT_PAYMENTS);
        let other = vm(GcAlgorithm::Generational);
        assert!(!native_call_owes_oome_major(&other, &raiser), "per VM");
        let bystander = JvmThread::new(ThreadId(8), "d4j-bystander");
        assert!(!native_call_owes_oome_major(&shared, &bystander), "the raiser's debt");
        assert_ne!(debt(), 0, "a bystander leaves it in place");
        // The fresh heap's old generation is empty: nothing to collect for.
        assert!(!native_call_owes_oome_major(&shared, &raiser));
        assert_eq!(debt(), 0, "cleared, not paid");
        let g1 = vm(GcAlgorithm::G1);
        note_heap_oome_raised(&g1, &raiser, "Java heap space");
        assert_eq!(g1.mem.alloc_ladder.oome_native_debt.load(Ordering::Relaxed), 0);
    }

    /// gcd d5/q: the pure debt rule. Only the raising thread's key pays; a
    /// heap that recovered clears it; a full heap pays at most twice.
    #[test]
    fn the_debt_is_the_raising_threads_and_is_paid_at_most_twice() {
        let key = 5u64 << 2;
        let word = key | OOME_DEBT_PAYMENTS;
        assert_eq!(oome_debt_decision(0, key, false), (false, 0), "nothing owed");
        assert_eq!(oome_debt_decision(word, 6 << 2, false), (false, word), "another thread's");
        assert_eq!(oome_debt_decision(word, key, true), (false, 0), "recovered: cleared");
        let (pay, next) = oome_debt_decision(word, key, false);
        assert!(pay);
        assert_eq!(next, key | 1, "one payment left after a futile one");
        assert_eq!(oome_debt_decision(next, key, false), (true, 0), "the last payment");
        assert_eq!(oome_debt_decision(key, key, false), (false, 0), "an exhausted word clears");
    }

    /// The deciding majors run on this (sole) thread and collect.
    #[test]
    fn the_deciding_majors_collect_on_this_thread() {
        let shared = vm(GcAlgorithm::Generational);
        let mut thread = JvmThread::new(ThreadId(0), "d4j-majors");
        let before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        assert!(majors_to_decide_oome(&shared, &mut thread, "d4j-test"));
        assert!(shared.mem.gc_cycle_count.load(Ordering::Relaxed) > before);
        assert!(
            !cratonvm_gc::gc_quiescence::major_gc_requested(),
            "no request is left behind"
        );
    }
}

#[cfg(test)]
mod gcd_d9b_oome_ladder_census_tests {
    //! gcd d9/b: the allocation-failure ladder's census (`[GC] oome_ladder:`,
    //! `cratonvm_gc::gen_heap::oome_ladder`), fed from this file.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use cratonvm_gc::gen_heap::oome_ladder as l;

    fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        })
    }

    fn census(shared: &SharedVm) -> [u64; l::LEN] {
        match &shared.mem.heap {
            crate::memory::vm_heap::VmHeap::Generational(h) => h.oome_ladder_census(),
            _ => [0; l::LEN],
        }
    }

    /// Every heap OOME a door reports is counted, whatever its detail; the
    /// VM-limit refusal is not; the census is per VM; off Generational the
    /// feed is a no-op.
    #[test]
    fn heap_oomes_are_counted_per_vm() {
        let shared = vm(GcAlgorithm::Generational);
        let other = vm(GcAlgorithm::Generational);
        let raiser = JvmThread::new(ThreadId(21), "d9b-raiser");
        note_heap_oome_raised(&shared, &raiser, ARRAY_SIZE_EXCEEDS_VM_LIMIT);
        assert_eq!(census(&shared)[l::OOMES], 0, "the VM limit is not a heap OOME");
        note_heap_oome_raised(&shared, &raiser, "Java heap space");
        note_heap_oome_raised(&shared, &raiser, "Java heap space (alloc_object with 5 fields)");
        assert_eq!(census(&shared)[l::OOMES], 2);
        assert_eq!(census(&other)[l::OOMES], 0, "per VM");
        let g1 = vm(GcAlgorithm::G1);
        note_heap_oome_raised(&g1, &raiser, "Java heap space");
        assert_eq!(census(&g1), [0; l::LEN]);
    }

    /// The deciding majors are one deciding call, one or two ladder majors
    /// (the second counted as such), each a forced collection with its time.
    #[test]
    fn the_deciding_majors_are_counted() {
        let shared = vm(GcAlgorithm::Generational);
        let mut thread = JvmThread::new(ThreadId(0), "d9b-majors");
        assert!(majors_to_decide_oome(&shared, &mut thread, "d9b-test"));
        let c = census(&shared);
        assert_eq!(c[l::DECIDING], 1);
        assert_eq!(c[l::MAJORS], 1 + c[l::SECOND_MAJORS], "{c:?}");
        assert!(c[l::FORCED] >= c[l::MAJORS], "a ladder major is a forced collection: {c:?}");
        assert_eq!(c[l::MAJORS_LOST], 0, "the sole thread wins its pauses");
        assert_eq!(c[l::OOMES], 0, "no error was raised");
    }
}

#[cfg(test)]
mod gcd_d2j_futile_young_tests {
    //! gcd d2/j: the futile-young backoff of the object doors.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use std::sync::atomic::Ordering;

    /// 256 KiB, doubling per verdict, capped at 2 % of the heap and never
    /// below the base.
    #[test]
    fn the_quantum_doubles_up_to_two_percent_of_the_heap() {
        let cap = 64 * 1024 * 1024; // 2 % = 1 342 177 bytes
        assert_eq!(futile_young_quantum(1, cap), 256 * 1024);
        assert_eq!(futile_young_quantum(2, cap), 512 * 1024);
        assert_eq!(futile_young_quantum(3, cap), 1024 * 1024);
        assert_eq!(futile_young_quantum(4, cap), (cap / 50) as u64);
        assert_eq!(futile_young_quantum(u32::MAX, cap), (cap / 50) as u64);
        // A tiny heap: the base, not less.
        assert_eq!(futile_young_quantum(5, 1024 * 1024), FUTILE_YOUNG_QUANTUM_BASE);
    }

    /// The window closes on bytes or on skipped entries, whichever first.
    #[test]
    fn the_window_closes_on_either_count() {
        let q = FUTILE_YOUNG_QUANTUM_BASE;
        assert!(futile_young_window_open(q, 0, 1));
        assert!(futile_young_window_open(q, q - 1, 1));
        assert!(!futile_young_window_open(q, q, 1));
        assert!(!futile_young_window_open(q, 0, q / FUTILE_YOUNG_MIN_OBJECT_BYTES));
        assert!(futile_young_window_open(q, 0, q / FUTILE_YOUNG_MIN_OBJECT_BYTES - 1));
    }

    /// Idle until a futile verdict; the verdict opens the window; a window
    /// that saw its quantum of allocation re-arms the collection; a cycle
    /// after which young serves the object clears it. Per VM.
    #[test]
    fn a_futile_verdict_opens_the_window_and_a_serving_young_clears_it() {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let st = &shared.mem.alloc_ladder;
        assert!(!futile_young_backoff_skips(&shared), "idle before any verdict");
        if !cratonvm_types::flags().gc.futile_young_backoff {
            return; // the process runs with the switch off: nothing to arm
        }
        // A fresh heap serves a small object: no verdict.
        note_forced_young_cycle_outcome(&shared, 64);
        assert_eq!(st.futile_young_streak.load(Ordering::Relaxed), 0);
        // An object young cannot hold at all stands in for a full young.
        note_forced_young_cycle_outcome(&shared, usize::MAX / 2);
        assert_eq!(st.futile_young_streak.load(Ordering::Relaxed), 1);
        assert!(futile_young_backoff_skips(&shared), "the window is open");
        assert_eq!(st.futile_young_skips.load(Ordering::Relaxed), 1);
        // A quantum of allocation since the verdict re-arms the collection.
        shared
            .mem
            .bytes_allocated_total
            .fetch_add(FUTILE_YOUNG_QUANTUM_BASE, Ordering::Relaxed);
        assert!(!futile_young_backoff_skips(&shared), "the quantum re-arms");
        // A second futile cycle doubles the next window.
        note_forced_young_cycle_outcome(&shared, usize::MAX / 2);
        assert_eq!(st.futile_young_streak.load(Ordering::Relaxed), 2);
        assert!(futile_young_backoff_skips(&shared));
        // A cycle after which young serves the object clears the verdict.
        note_forced_young_cycle_outcome(&shared, 64);
        assert_eq!(st.futile_young_streak.load(Ordering::Relaxed), 0);
        assert!(!futile_young_backoff_skips(&shared));
        // Another VM never saw any of it.
        let other = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        assert!(!futile_young_backoff_skips(&other));
    }

    /// G1 (and ZGC) never arm it.
    #[test]
    fn the_backoff_is_generational_only() {
        let g1 = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::G1,
            ..VmConfig::default()
        });
        note_forced_young_cycle_outcome(&g1, usize::MAX / 2);
        assert_eq!(g1.mem.alloc_ladder.futile_young_streak.load(Ordering::Relaxed), 0);
        assert!(!futile_young_backoff_skips(&g1));
    }
}

/// Default number of consecutive unproductive allocation-failure GCs (each
/// freeing < 2% of capacity *while the old generation cannot absorb 2% of
/// capacity* — see [`note_gc_productivity`]) after which the allocation paths
/// declare OOM instead of continuing to GC-thrash. Overridable via
/// `CRATONVM_GC_OVERHEAD_LIMIT` (set to `0` to disable the limit entirely).
const GC_OVERHEAD_LIMIT_CYCLES: u32 = 8;

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
/// `docs/internal/gc/heap-gc-overhead-limit-reads-two-hard-zeros-on-g1-and-zgc-20260920-RETIRED-20260921.md`.
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
pub(super) fn note_gc_productivity(
    shared: &SharedVm,
    before_live: usize,
    before_promoted: u64,
    sealed_after: Option<(usize, u64)>,
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
    // gen r4w4/oom (2026-09-24): one function, shared with the Generational
    // young trigger, which stands down on the same "wedged" verdict (see
    // `gen_heap::young_trigger_floor_after_collection`). Same arithmetic as the
    // inline test it replaces.
    let old_gen_wedged = cratonvm_gc::gen_heap::old_gen_is_wedged(old_headroom, cap);
    // unproductive: the old gen is wedged AND (freed < 2% of capacity OR no
    // mutator progress) -- `gc_cycle_is_unproductive`.
    // Widening: usize -> u64 is loss-free on every supported target.
    let freed_sliver = two_percent_sliver(freed as u64, cap);
    // gen r4w5/thrash5: the mutator-progress half (see `MutatorProgress`).
    let progress = sample_mutator_progress(shared, after_live);
    let unproductive = gc_cycle_is_unproductive(freed, cap, old_gen_wedged, progress);
    if unproductive {
        // gcd d9/b: `[GC] oome_ladder: ladder_forced_unproductive`.
        ladder_census(shared, cratonvm_gc::gen_heap::oome_ladder::FORCED_UNPRODUCTIVE, 1);
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
        let (progress_allocated, progress_prev_live) = progress
            .map_or((-1i128, -1i128), |p| (i128::from(p.allocated), p.prev_live_after as i128));
        eprintln!(
            "[GC_OVERHEAD] before={before_live} after={after_live} promoted={promoted} \
             freed={freed} cap={cap} old_headroom={old_headroom} freed_sliver={freed_sliver} \
             progress_allocated={progress_allocated} progress_prev_live={progress_prev_live} \
             old_gen_wedged={old_gen_wedged} unproductive={unproductive} streak={streak} \
             young_used={y_used} young_free_list={y_free} young_largest_free={y_largest} \
             young_cap={y_cap} sp_sweeps={sw} sp_selective={sel} sp_defrag={defrag} \
             sp_candidates={cand} sp_pinned={pin} sp_unaged={unaged} sp_evacuated={evac} \
             sp_old_full={ofull}"
        );
    }
}

/// Is `bytes` under 2 % of `cap`? The one yardstick of the overhead limit (the
/// freed-bytes half, the progress half, and — through
/// `gen_heap::old_gen_is_wedged` — the free-space half).
#[inline]
fn two_percent_sliver(bytes: u64, cap: usize) -> bool {
    // Widening: u64/usize -> u128 cannot overflow the products.
    u128::from(bytes) * 100 < (cap as u128) * 2
}

/// The mutator-progress window of one allocation-failure collection: what the
/// program did between the previous forced collection and this one.
/// gen r4w5/thrash5 (2026-09-24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MutatorProgress {
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
pub(super) fn gc_cycle_is_unproductive(
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
fn gc_overhead_progress_enabled() -> bool {
    cratonvm_types::flags().gc.gc_overhead_progress
}

/// Close this forced collection's progress window and open the next: read the
/// window since the previous forced collection (if one was recorded), then
/// record the counters and `live_after` for the next. Per VM
/// (`HeapRealm::gc_progress`). `None` when there is no previous window or the
/// progress half is switched off. gen r4w5/thrash5 (2026-09-24).
fn sample_mutator_progress(shared: &SharedVm, live_after: usize) -> Option<MutatorProgress> {
    use std::sync::atomic::Ordering;
    let marks = &shared.mem.gc_progress;
    let bytes_now = shared.mem.bytes_allocated_total.load(Ordering::Relaxed);
    let thread_now = shared.mem.thread_allocated_total.load(Ordering::Relaxed);
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
    marks.bytes_allocated_mark.store(bytes_now, Ordering::Relaxed);
    marks.thread_allocated_mark.store(thread_now, Ordering::Relaxed);
    // Widening: usize -> u64 is loss-free on every supported target.
    marks.live_after_mark.store(live_after as u64, Ordering::Relaxed);
    marks.armed.store(true, Ordering::Relaxed);
    // gen r4w6/review6: Generational only. The evidence for this half is the
    // generational thrash (`GenR4W4HeapFullThrashProbe`, `GenR4W5ThreadsOomProbe`);
    // on G1 and ZGC — ZGC is the default collector — it changed the default
    // overhead verdict unmeasured (and the JIT's inline TLAB bumps reach the
    // counters it compares only at a TLAB retire). Measure it there first
    // (the same probes under `-XX:+UseG1GC` / the default, `--verbose:gc`
    // with `CRATONVM_DBG_GC_OVERHEAD=1`), then widen this.
    window.filter(|_| gc_overhead_progress_enabled() && shared.mem.heap.is_generational())
}

#[cfg(test)]
mod w5_overhead_progress_tests {
    //! gen r4w5/thrash5: the mutator-progress half of the GC-overhead limit.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use std::sync::atomic::Ordering;

    /// The measured heap: `-Xmx128m` Generational, one 32 MiB semi-space
    /// plus a 64 MiB old generation.
    const CAP: usize = 100_663_296;

    fn window(allocated: u64, prev_live_after: usize, live_after: usize) -> Option<MutatorProgress> {
        Some(MutatorProgress {
            allocated,
            prev_live_after,
            live_after,
        })
    }

    /// HIB-GCOVERHEAD-HALFFULL.1 is untouched: with room in the old
    /// generation nothing is ever unproductive, progress or not.
    #[test]
    fn a_heap_with_old_gen_room_is_never_unproductive() {
        assert!(!gc_cycle_is_unproductive(0, CAP, false, None));
        assert!(!gc_cycle_is_unproductive(0, CAP, false, window(0, 10, 10)));
    }

    /// With no window (first forced collection, or the switch off) the verdict
    /// is the historical freed-bytes one, byte for byte.
    #[test]
    fn without_a_window_the_verdict_is_the_freed_bytes_one() {
        let two_percent = CAP / 50;
        assert!(gc_cycle_is_unproductive(two_percent - 8, CAP, true, None));
        assert!(!gc_cycle_is_unproductive(two_percent + 8, CAP, true, None));
    }

    /// The measured thrash (`GenR4W4HeapFullThrashProbe` step `threads`):
    /// 13 carves (3 407 872 bytes, 3.4 %) of filler freed per cycle, a few
    /// kilobytes linked, live flat at 13.5 MB young + 64 MiB old.
    #[test]
    fn filler_freed_without_progress_is_unproductive() {
        let live = 13_565_952 + 67_108_856;
        assert!(gc_cycle_is_unproductive(3_407_872, CAP, true, window(20_000, live, live)));
        // The live set creeping up (the few hundred linked bytes) is still flat.
        assert!(gc_cycle_is_unproductive(3_407_872, CAP, true, window(20_000, live, live + 65_536)));
        // ...and so is a fall of less than 2 % of capacity.
        assert!(gc_cycle_is_unproductive(
            3_407_872,
            CAP,
            true,
            window(20_000, live, live - CAP / 50 + 8),
        ));
    }

    /// A program that allocated 2 % of capacity or more since the previous
    /// forced collection made progress, however full the heap. (2 % of `CAP`
    /// is 2 013 265.92 bytes, so `CAP / 50` is still under it.)
    #[test]
    fn real_allocation_is_progress() {
        let live = 90_000_000;
        let two_percent = (CAP / 50) as u64;
        assert!(!gc_cycle_is_unproductive(3_407_872, CAP, true, window(two_percent + 1, live, live)));
        assert!(gc_cycle_is_unproductive(3_407_872, CAP, true, window(two_percent, live, live)));
    }

    /// A program that caught the error and DROPPED its data has allocated
    /// almost nothing since, but its live set fell: not this arm's loop.
    #[test]
    fn a_program_that_dropped_its_data_is_not_caught() {
        assert!(!gc_cycle_is_unproductive(
            30_000_000,
            CAP,
            true,
            window(1_000, 95_000_000, 66_000_000),
        ));
        // A fall of just over 2 % of capacity counts as a fall.
        assert!(!gc_cycle_is_unproductive(
            3_407_872,
            CAP,
            true,
            window(1_000, 90_000_000, 90_000_000 - CAP / 50 - 1),
        ));
    }

    /// The window is per VM, opens at the first forced collection, and
    /// measures the larger of the two allocation counters' deltas.
    #[test]
    fn the_progress_window_is_per_vm_and_takes_the_larger_counter() {
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        assert_eq!(sample_mutator_progress(&shared, 1_000), None, "no window before the first");
        shared.mem.bytes_allocated_total.fetch_add(4_096, Ordering::Relaxed);
        shared.mem.thread_allocated_total.fetch_add(10_000, Ordering::Relaxed);
        assert_eq!(
            sample_mutator_progress(&shared, 2_000),
            Some(MutatorProgress {
                allocated: 10_000,
                prev_live_after: 1_000,
                live_after: 2_000,
            })
        );
        assert_eq!(
            sample_mutator_progress(&shared, 2_000).map(|p| p.allocated),
            Some(0),
            "each collection closes its window"
        );
        let other = crate::vm::SharedVm::new(VmConfig::default());
        assert!(!other.mem.gc_progress.armed.load(Ordering::Relaxed));
    }

    /// gen r4w6/review6: the progress half is Generational-only, the backend
    /// its evidence came from. On G1 the marks are still recorded, but no
    /// window is ever handed to the verdict, so it stays the historical
    /// freed-bytes one.
    #[test]
    fn the_progress_window_is_generational_only() {
        let g1 = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::G1,
            ..VmConfig::default()
        });
        assert_eq!(sample_mutator_progress(&g1, 1_000), None);
        g1.mem.bytes_allocated_total.fetch_add(4_096, Ordering::Relaxed);
        assert_eq!(sample_mutator_progress(&g1, 1_000), None, "no window on G1");
        assert!(g1.mem.gc_progress.armed.load(Ordering::Relaxed), "the marks are kept");
    }
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
/// `UseGCOverheadLimit`. Disabled (always `false`) when
/// `CRATONVM_GC_OVERHEAD_LIMIT=0`.
pub fn gc_overhead_limit_exceeded(shared: &SharedVm) -> bool {
    // PERF: this runs on the per-allocation slow path (`jit_new_object` and
    // the interpreter allocation sites). An uncached `cratonvm_types::flags::runtime_var` here was
    // ~6% of binarytrees-18 wall time (getenv does a linear environ scan) —
    // read the knob once. `Some(0)` = explicitly disabled.
    use std::sync::OnceLock;
    static LIMIT: OnceLock<u32> = OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_GC_OVERHEAD_LIMIT") {
            Ok(v) => v.trim().parse::<u32>().unwrap_or(GC_OVERHEAD_LIMIT_CYCLES),
            Err(_) => GC_OVERHEAD_LIMIT_CYCLES,
        }
    });
    if limit == 0 {
        return false; // explicitly disabled
    }
    shared
        .mem
        .gc_unproductive_streak
        .load(std::sync::atomic::Ordering::Relaxed)
        >= limit
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
/// non-explicit collections -- `jcmd GC.run` (the `gc_requested` latch, taken
/// by `maybe_gc`), the metaspace refusal ([`force_gc_for_class_metadata`]) and
/// every allocation door -- never read it.
pub fn force_gc_from_native(shared: &SharedVm, thread: &mut JvmThread) {
    full_gc_door(shared, thread, shared.config.explicit_gc, true);
}

/// [`force_gc_from_native`] whatever `VmConfig::explicit_gc` says: the full
/// collection and its drains, no `jdk.SystemGC` event. For VM-internal callers
/// that need a collection to shed native-side table entries
/// (`NativeContext::force_gc_for_vm`); NOT an explicit GC, so
/// `-XX:+DisableExplicitGC` cannot turn it off. gc-common w8-g
/// (`handoff-w8g-vm-internal-collection-uses-the-explicit-door`).
pub fn force_gc_for_vm(shared: &SharedVm, thread: &mut JvmThread) {
    full_gc_door(shared, thread, crate::config::ExplicitGcPolicy::Full, false);
}

/// The `System.gc()` door's body, under `policy`; `explicit` says whether it
/// is a Java-visible explicit GC (emits `jdk.SystemGC`).
fn full_gc_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    policy: crate::config::ExplicitGcPolicy,
    explicit: bool,
) {
    let action = explicit_gc_action(policy);
    if explicit && action != ExplicitGcAction::Ignore {
        emit_system_gc_jfr_event(
            shared,
            thread,
            policy == crate::config::ExplicitGcPolicy::Concurrent,
        );
    }
    match action {
        // `-XX:+DisableExplicitGC`: HotSpot's `JVM_GC` returns before it does
        // anything, so no collection, no drain, no `jdk.SystemGC` event and no
        // count anywhere (`gc_cycle_count`, `system_gc_collections`, the JMX
        // beans).
        ExplicitGcAction::Ignore => return,
        ExplicitGcAction::Collect => force_full_collection(shared, thread, GcDoor::SystemGc),
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
    // `gc_notifications_go_to_delivery_thread`).
    if !gc_notifications_go_to_delivery_thread(shared, thread) {
        run_gc_notifications(shared, thread, true);
    }
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
pub fn force_gc_for_class_metadata(shared: &SharedVm, thread: &mut JvmThread) {
    force_full_collection(shared, thread, GcDoor::MetadataThreshold);
}

/// JFR `jdk.SystemGC` for one explicit GC that is not ignored, as HotSpot's
/// `JVM_GC` commits it (`invokedConcurrent` is the
/// `-XX:+ExplicitGCInvokesConcurrent` setting, whatever the collector).
/// `emit_system_gc_event` had no production caller before gc-common w8-g.
/// One relaxed load when no recording runs.
fn emit_system_gc_jfr_event(shared: &SharedVm, thread: &JvmThread, invoked_concurrent: bool) {
    if !cratonvm_jfr::is_enabled() {
        return;
    }
    let now_ns = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX);
    let mut jfr = shared.debug.flight_recorder.lock();
    cratonvm_jfr::builtin::emit_system_gc_event(
        &mut jfr,
        invoked_concurrent,
        now_ns,
        thread.thread_id.0,
    );
}

/// What one explicit GC request does in this VM. gc-common w8-g.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExplicitGcAction {
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
/// which HotSpot also sets from it.
fn explicit_gc_action(policy: crate::config::ExplicitGcPolicy) -> ExplicitGcAction {
    use crate::config::ExplicitGcPolicy;
    match policy {
        ExplicitGcPolicy::Disabled => ExplicitGcAction::Ignore,
        ExplicitGcPolicy::Concurrent | ExplicitGcPolicy::Full => ExplicitGcAction::Collect,
    }
}

/// The collection half of [`force_gc_from_native`]: one `System.gc()`-door
/// full collection (or a coalesced sibling's), the G1 concurrent-cycle tick
/// and, on G1, the synchronous full marking cycle.
fn force_full_collection(shared: &SharedVm, thread: &mut JvmThread, door: GcDoor) {
    cratonvm_types::gc_entry_census::note_from_native();
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
    let coalesce_baseline = system_gc_coalescing_baseline(shared, thread);
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
    // `gc_cycle_count` is advanced by `gc_event_finish` inside each collecting
    // arm above (dev's gen r4/plumbing), so it is not bumped again here.
    if !collected {
        // Every attempt took part in someone else's pause instead (or a
        // sibling `System.gc()` satisfied this one). Withdraw the full-GC
        // request so it cannot hijack this thread's next, unrelated
        // collection (see the loop's doc above).
        let _ = cratonvm_gc::gc_quiescence::take_major_gc_request();
    }
    if coalesced {
        tracing::debug!(
            "System.gc() coalesced with a sibling System.gc() collection that completed \
             after this call began"
        );
    }
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
    maybe_concurrent_gc_at(shared, thread, MarkDoor::SystemGc);
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
        g1_force_full_cycle(shared, thread);
    }
}

/// This VM's count of COMPLETED `System.gc()`-door collections
/// (`HeapRealm::system_gc_collections`), bumped by [`run_collection_pause`]
/// inside the pause, before the release. Per VM: a sibling VM's `System.gc()`
/// satisfies nothing here. gc-common w7-g (`common-e-small-findings` item 7).
#[inline]
fn system_gc_collections(shared: &SharedVm) -> &std::sync::atomic::AtomicU64 {
    &shared.mem.system_gc_collections
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
fn system_gc_coalescing_baseline(shared: &SharedVm, thread: &JvmThread) -> Option<u64> {
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
fn system_gc_completed_since(shared: &SharedVm, baseline: Option<u64>) -> bool {
    baseline.is_some_and(|before| {
        system_gc_satisfied(
            before,
            system_gc_collections(shared).load(std::sync::atomic::Ordering::Acquire),
        )
    })
}

/// The coalescing rule itself: at least one completed since the baseline.
#[inline]
fn system_gc_satisfied(baseline: u64, now: u64) -> bool {
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
fn run_cleaner_actions_impl(shared: &SharedVm, thread: &mut JvmThread, force: bool) {
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
fn forced_finalizers_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_FORCED_FINALIZERS").as_deref(),
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
pub(super) fn run_finalizers(shared: &SharedVm, thread: &mut JvmThread) {
    // gc-common w3-d / w4-d: `finalize()` never runs on an allocating mutator
    // that holds a user-visible lock (JLS §12.6) — by default only then, with
    // `CRATONVM_FINALIZER_THREAD=1` never. See `FinalizerDelivery`.
    if finalizer_thread_takes_finalizers(shared, thread) {
        return;
    }
    run_finalizers_impl(shared, thread, false);
}

/// `System.gc()` / `System.runFinalization()`'s drain: run pending finalizers
/// even while a JIT helper holds the thread borrow.
///
/// # Why the deferral had to go on this path
///
/// The guard in [`run_finalizers_impl`] says "defer to the next top-level
/// safepoint". For an application that only ever asks for finalization from
/// inside a compiled loop, **that safepoint never comes**: every
/// `System.gc()` arrives through a JIT helper, so `is_jit_thread_set()` is true
/// at every call and the drain is deferred forever.
///
/// MEASURED (`probes/ThreadRetainMin.java`, `CRATONVM_DBG_FINCAND=1`): the
/// objects are found dead and resurrected on EVERY cycle
/// (`dead_resurrected=1,2,3` some thirty times each) because
/// `finalizable_roots` re-adds the pending queue as roots to keep them alive
/// for a finalizer that never runs. `finalize()` never executes, the object is
/// never reclaimed, and a `WeakReference` to it never clears. Under `--nojit`
/// the same probe resurrects once and collects in two rounds.
///
/// That is `RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced`,
/// and it is not about Threads, netty, or JUnit: any object with a `finalize()`
/// override is immortal once the loop asking for the collection is compiled.
///
/// The re-entrancy the guard protects against is handled the way
/// [`run_cleaner_actions_forced`] already handles it for the sibling queue —
/// re-install the JIT thread pointer for the nested Java call under RAII, so an
/// unwinding `finalize()` cannot leave the outer level un-restored.
pub(super) fn run_finalizers_forced(shared: &SharedVm, thread: &mut JvmThread) {
    // gc-common w3-d / w4-d: where the delivery thread takes `finalize()` (see
    // `FinalizerDelivery`), `System.gc()` keeps its "finalizers have run when I
    // return" ordering by waiting (bounded) for it. Unserved, a caller holding
    // no lock falls through to the inline drain; a caller holding one returns
    // and leaves the queue to the delivery thread (JLS §12.6).
    if finalizers_handed_off_for_forced_drain(shared, thread) {
        return;
    }
    run_finalizers_forced_inline(shared, thread);
    // gc-common w5-d: and do not return while the delivery thread is still
    // running a finalizer it dequeued before this drain looked.
    wait_out_in_flight_finalizer_batch(shared, thread);
}

/// Under the default policy ([`FinalizerDelivery::WhenLockHeld`]) a lock-free
/// forced drain (`System.gc()`, `runFinalization()`) runs inline — but once a
/// delivery thread exists, its batch drains the same queue. An object that batch
/// dequeued just before the inline drain looked is not run here, and the caller
/// returned while its `finalize()` was still running on the delivery thread:
/// `System.gc(); System.runFinalization();` followed by a read of the
/// finalizer's side effect could see it missing.
///
/// gc-common w5-d (2026-09-24). Pre-existing under `--compatible` (the thread
/// starts there the first time a lock-holding thread has finalizers pending),
/// and far more reachable once a `--jdk-only` VM starts the thread for its queue
/// wake-ups (`queue_wake_ups_recorded_for`). Waits only while a batch is owed or
/// running, bounded by the finalizer timeout and sliced like every other forced
/// wait; never on the delivery thread itself, and not under the full hand-off,
/// whose forced drain already waited for the thread before falling back here.
fn wait_out_in_flight_finalizer_batch(shared: &SharedVm, thread: &mut JvmThread) {
    if finalizer_delivery_policy() != FinalizerDelivery::WhenLockHeld {
        return;
    }
    let ft = &shared.mem.finalizer_thread;
    if !ft.delivery_thread_claimed() || ft.delivery_thread() == Some(thread.thread_id.0) {
        return;
    }
    if !ft.delivery_signal().in_flight() {
        return;
    }
    let holds = thread_holds_user_locks(shared, thread);
    let _ = wait_for_delivery(shared, thread, holds);
}

/// The inline half of [`run_finalizers_forced`].
fn run_finalizers_forced_inline(shared: &SharedVm, thread: &mut JvmThread) {
    if !forced_finalizers_enabled() {
        run_finalizers_impl(shared, thread, false);
        return;
    }
    // RAII so an unwinding finalizer cannot leave the outer JIT level
    // un-restored. Same shape as `run_cleaner_actions_forced`.
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
    run_finalizers_impl(shared, thread, true);
}

/// `Runtime.runFinalization()` / `System.runFinalization()`: run every
/// finalizer queued so far before returning — the forced drain `System.gc()`
/// ends with, without the collection.
///
/// gc-common w4-d (2026-09-23), `common-w3d-run-finalization-never-drains-the-vm-queue`.
/// This VM finalizes through its own queue (`Finalizer.register` is
/// intercepted, so the JDK's `Finalizer` queue is always empty), and neither
/// `runFinalization` route looked at it: the `compatible` natives drained a
/// process-global tracker nothing fed, and the `--jdk-only` bytecode forked a
/// secondary finalizer over the empty JDK queue. Finalizers an allocation-driven
/// collection had queued but not run (its drain declines under a JIT borrow,
/// and the allocation-failure door runs none) therefore waited for some later
/// door. Where the delivery thread serves (`FinalizerDelivery`), this waits for
/// it exactly as `System.gc()` does; HotSpot's `runFinalization` likewise
/// joins a secondary finalizer. Cleaner actions are not part of the contract
/// and stay on their own schedule.
pub(crate) fn run_pending_finalizers_for_runtime(shared: &SharedVm, thread: &mut JvmThread) {
    run_finalizers_forced(shared, thread);
}

/// `JavaLangRefAccess.waitForReferenceProcessing()`: if the reference-delivery
/// thread has cleaner or finalizer work queued or in flight for this VM, wait
/// for that batch (bounded, in a blocking region) and answer `true`; otherwise
/// answer `false` at once. HotSpot's answer means the same thing: "the
/// Reference Handler had work, and I waited for it".
///
/// gc-common w5-d (2026-09-24),
/// `common-w4d-wait-for-reference-processing-ignores-the-delivery-thread`. The
/// bridge answered `false` unconditionally ("reference processing runs inline
/// with GC"), which the full hand-off made untrue: an allocation-driven
/// collection queues `DirectByteBuffer`'s `Deallocator` and returns, and the
/// delivery thread frees the memory later. `java.nio.Bits.reserveMemory` asks
/// this before it falls back to `System.gc()` — a FULL collection — and its
/// sleep ladder, so a `false` while the memory was about to be freed cost one
/// full GC per reservation that missed.
///
/// Only under the full hand-off (`CRATONVM_FINALIZER_THREAD=1`): under the
/// default policy the delivery thread runs nothing but the finalizers of
/// lock-holding threads, which `Bits` does not wait for, and cleaner actions
/// stay on their inline doors — so `--compatible` answers exactly as before.
/// The delivery thread asking about itself answers `false` (it would wait on
/// its own batch). A wait that ends unserved — the finalizer timeout, or the
/// delivery thread blocked on a monitor the caller holds — answers `false`, so
/// `Bits`' `while (refprocActive)` loop cannot spin on a stuck batch.
pub(crate) fn wait_for_reference_processing_for_runtime(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> bool {
    if !finalizer_thread_enabled() {
        return false;
    }
    let ft = &shared.mem.finalizer_thread;
    if ft.delivery_thread() == Some(thread.thread_id.0) {
        return false;
    }
    let queued = finalizer_thread_has_work(shared);
    // `in_flight` only while a thread is claimed: a spawn that failed hands the
    // claim back but leaves its pre-spawn request owed forever.
    let in_flight = ft.delivery_thread_claimed() && ft.delivery_signal().in_flight();
    if !queued && !in_flight {
        return false;
    }
    // Queued work the thread may not have been asked for yet (a door that
    // deferred it): ask, starting the thread on first use. Work only in flight
    // is already covered by the ticket `wait_for_delivery` takes.
    if queued && !request_finalizer_thread(shared, true) {
        return false;
    }
    let holds = thread_holds_user_locks(shared, thread);
    wait_for_delivery(shared, thread, holds) == DeliveryWait::Served
}

fn run_finalizers_impl(shared: &SharedVm, thread: &mut JvmThread, forced: bool) {
    // bc math-ec 0x4 exclusion switches — see `process_references_after_gc`.
    if no_refproc() || no_cleaners() {
        return;
    }
    // Same JIT-borrow re-entrancy guard as `run_cleaner_actions`: a `finalize()`
    // invoked while a JIT helper holds the `&mut JvmThread` could re-enter the
    // JIT and alias the borrow. Defer to the next top-level safepoint; the
    // queue is GC-relocated via `FinalizerThread::update_after_gc`.
    //
    // `forced` callers have re-installed the JIT thread pointer around this
    // call, so the nested invocation cannot alias the borrow.
    if !forced && crate::jit::helpers::is_jit_thread_set() {
        return;
    }
    loop {
        let obj_addr = match shared.mem.finalizer_thread.dequeue() {
            Some(addr) => addr,
            None => break,
        };
        // SAFETY: obj_addr was produced by the finalizer thread's dequeue and points at a valid object header within the heap arena.
        let obj_ref = unsafe { ObjectRef::from_raw(obj_addr as *mut u8) };
        let class_id = shared.mem.heap.class_id_of(obj_ref);

        let class_known = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .is_some();

        if class_known {
            // Invoke finalize()V — errors are silently swallowed per JLS §12.6.
            //
            // gc-common w1-d (2026-09-23): dispatched on the object's own
            // `ClassId`, not re-resolved from its NAME. `invoke_shared` looks
            // the name up loader-blind (`load_class_concurrent`), so for a class
            // defined by a user loader it either reports "ambiguous" the moment
            // a second loader defines the same name — every redeploy of the same
            // webapp, every forked test class loader — or resolves (or LOADS)
            // the application-classpath class of that name instead; either way
            // `finalize()` silently did not run on the object it was for, or ran
            // another class's body against it. The class id in the header is
            // exact; `invoke_by_class_id_shared` exists for this.
            let _ = crate::vm::invoke_by_class_id_shared(
                shared,
                thread,
                class_id,
                "finalize",
                "()V",
                &[Value::Object(Some(obj_ref))],
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The reference-delivery thread ("Craton Finalizer") — gc-common w3-d
// ---------------------------------------------------------------------------
//
// `common-w2f-finalizers-and-cleaners-run-on-the-allocating-mutator-FIXED-20260923.md`:
// `finalize()`, cleaner actions and `DirectByteBuffer` deallocators ran inline
// on whichever mutator's allocation triggered the collection — in the middle
// of an arbitrary `new`, with whatever monitors (re-entrant, so a finalizer
// could ENTER the allocating thread's half-finished critical section),
// `ThreadLocal`s, context class loader and interrupt status that thread had.
// JLS §12.6: the thread that invokes a finalizer holds no user-visible locks.
//
// With `CRATONVM_FINALIZER_THREAD=1` a per-VM daemon thread does that work
// instead, and also the `ReferenceQueue` wake-ups the collector owes
// (`docs/internal/gc-common-round-20260923/common-w2f-gc-enqueue-never-wakes-queue-waiters-FIXED-20260923.md`):
//
// * every collection door still QUEUES exactly what it queued before (the
//   `FinalizerThread` / `CleanerThread` queues, rooted and relocated as
//   before) and now bumps the delivery signal;
// * the allocation doors (`run_finalizers` / `run_cleaner_actions`) run
//   nothing;
// * `System.gc()` (`run_*_forced`) waits, in a blocking region and for at most
//   the finalizer timeout, until the delivery thread has served everything
//   queued before it asked — so `System.gc(); System.runFinalization();`
//   ordering is kept — and drains inline only if that wait times out;
// * the delivery thread, idle, is GC-blocked (never counted by a pause) and
//   holds only a weak handle to its VM.
//
// It is also the drain that `docs/internal/gc-common-round-20260923/common-w2a-finalizers-queued-by-forced-collections-wait-for-another-door-FIXED-20260923.md`
// lacked: the allocation-FAILURE door drains nothing, but every door's pause
// ends in `process_references_after_gc` / `enqueue_resurrected_finalizers`,
// which wake this thread (and start it on first use).
//
// DEFAULT (gc-common w4-d, 2026-09-23): the delivery thread is used for
// `finalize()` exactly where running it inline would break JLS §12.6 — when
// the thread that would run it holds a user-visible lock (a monitor, or an
// `AbstractOwnableSynchronizer`), see [`FinalizerDelivery::WhenLockHeld`].
// Everything else — cleaner actions, queue wake-ups, a lock-free mutator's
// finalizers — keeps its pre-w3 inline path, so `--compatible` output is
// unchanged except for the §12.6 case. `CRATONVM_FINALIZER_THREAD=1` still
// selects the full hand-off (every door, cleaners and wake-ups included);
// `=0` restores the inline drain everywhere (bisection).
//
// gc-common w5-d (2026-09-24): a `--jdk-only` VM also hands its `ReferenceQueue`
// wake-ups to the delivery thread under the default policy
// (`queue_wake_ups_recorded_for`). Where the `ReferenceQueue` bridges yield to
// the real `remove()` bytecode, that bytecode is `lock.wait()` and nothing else
// wakes it. The policy is read per VM from `SharedVm`, so a `--compatible` VM in
// the same process is untouched.
//
// GC notifications (JMX listeners): off the mutator for a `--jdk-only` VM
// (w6-d), and for a `--compatible` one exactly when the collecting thread holds
// a user-visible lock (w7-d) — the §12.6 line, drawn for listeners. See
// `gc_notifications_go_to_delivery_thread`.

/// How `finalize()` (and, under [`Self::Always`], cleaner actions and queue
/// wake-ups) is delivered. gc-common w4-d.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FinalizerDelivery {
    /// `CRATONVM_FINALIZER_THREAD=0`: the pre-w3 behaviour everywhere — every
    /// drain runs inline on the thread that reaches it.
    Inline,
    /// Unset (the default): inline, EXCEPT that a thread holding a
    /// user-visible lock never runs `finalize()` itself — it hands the queue
    /// to the delivery thread (started on first such use) and, on the forced
    /// paths, waits for it (bounded). JLS §12.6: "the thread that invokes the
    /// finalizer will not be holding any user-visible synchronization locks".
    /// A finalizer that synchronizes on a monitor the allocating thread holds
    /// used to ENTER it (monitors are re-entrant) half-way through that
    /// thread's critical section. Cleaner actions are not covered (their
    /// contract is the Cleaner's, not the JLS's) and keep the inline path.
    WhenLockHeld,
    /// `CRATONVM_FINALIZER_THREAD=1` (`CRATONVM_GC=finalizer-thread`): every
    /// door hands everything — finalizers, cleaner actions, `ReferenceQueue`
    /// wake-ups, GC notifications — to the delivery thread (gc-common w3-d).
    Always,
}

/// The delivery thread's registry / OS thread name.
const FINALIZER_THREAD_NAME: &str = "Craton Finalizer";

/// How long the idle delivery thread sleeps between checks that its VM still
/// exists. A request wakes it at once; this bounds only how long a dropped VM
/// keeps the (then unregistered) OS thread around.
const FINALIZER_THREAD_IDLE_TICK: std::time::Duration = std::time::Duration::from_secs(2);

/// `CRATONVM_FINALIZER_THREAD`, latched like every flag. See
/// [`FinalizerDelivery`] and [`finalizer_delivery_from`].
fn finalizer_delivery_policy() -> FinalizerDelivery {
    static POLICY: std::sync::OnceLock<FinalizerDelivery> = std::sync::OnceLock::new();
    *POLICY.get_or_init(|| {
        finalizer_delivery_from(
            cratonvm_types::flags::runtime_var("CRATONVM_FINALIZER_THREAD")
                .ok()
                .as_deref(),
        )
    })
}

/// The value rule of [`finalizer_delivery_policy`]: unset is
/// [`FinalizerDelivery::WhenLockHeld`]; an explicit `0` / `false` / `off` /
/// `no` is [`FinalizerDelivery::Inline`]; any other value (the grouped
/// `CRATONVM_GC=finalizer-thread` spelling included) is
/// [`FinalizerDelivery::Always`] — the presence rule the other GC levers use,
/// with the off words now meaning "the pre-w3 inline drain".
fn finalizer_delivery_from(value: Option<&str>) -> FinalizerDelivery {
    match value.map(str::trim) {
        None => FinalizerDelivery::WhenLockHeld,
        Some("0" | "false" | "off" | "no") => FinalizerDelivery::Inline,
        Some(_) => FinalizerDelivery::Always,
    }
}

/// The full hand-off (`CRATONVM_FINALIZER_THREAD=1`): every door hands every
/// queue to the delivery thread. The reference passes' in-pause wakes and the
/// queue wake-up recording are gated on this, not on the default policy.
fn finalizer_thread_enabled() -> bool {
    finalizer_delivery_policy() == FinalizerDelivery::Always
}

/// Whether this VM's reference passes record the `ReferenceQueue`s they link
/// into, so the delivery thread can `notifyAll` each queue's `lock` after the
/// pause (`gc_enqueued_queues_notify`). See [`queue_wake_ups_recorded_for`].
fn queue_wake_ups_recorded(shared: &SharedVm) -> bool {
    queue_wake_ups_recorded_for(finalizer_delivery_policy(), shared.config.is_jdk_only())
}

/// The rule of [`queue_wake_ups_recorded`]: under the full hand-off always;
/// otherwise exactly when THIS VM runs `--jdk-only` (never for
/// [`FinalizerDelivery::Inline`], the bisection arm).
///
/// gc-common w5-d (2026-09-24), `common-w2f-gc-enqueue-never-wakes-queue-waiters`.
/// Wherever the real `ReferenceQueue.remove()` bytecode runs, it is
/// `lock.wait()`, and the collector's splice notifies nobody. An untimed
/// `remove()` then waited for some OTHER `enqueue()` on the same queue —
/// possibly forever — and the Common Cleaner learned of a GC-enqueued
/// `PhantomCleanable` only at its 60-s timeout. Under `--jdk-only` that
/// bytecode runs as soon as the `Bridge` natives yield to it: today under the
/// `CRATONVM_ENFORCE_NATIVE_SHADOW` dial (measured: both waiters asleep after
/// 10 s on the w4 binary), and for good once those bridges are retired. Waves
/// 3-4 fixed it only with `CRATONVM_FINALIZER_THREAD=1`. The strict policy is per VM
/// (`VmConfig::compatibility_mode`, read from `SharedVm`), never a process
/// flag, so one process may host a `--compatible` VM next to a strict one.
/// Under `--compatible` the natives poll (`rq_remove_wait`) and nothing is
/// recorded, so that mode is unchanged byte for byte.
fn queue_wake_ups_recorded_for(policy: FinalizerDelivery, jdk_only: bool) -> bool {
    match policy {
        FinalizerDelivery::Always => true,
        FinalizerDelivery::WhenLockHeld => jdk_only,
        FinalizerDelivery::Inline => false,
    }
}

/// Whether a `ReferenceQueue.remove` waiter in this VM may sleep on the
/// queue's `lock` rather than poll: the reference passes record every queue
/// they link into ([`queue_wake_ups_recorded`]) AND a delivery thread has been
/// claimed to deliver them — [`gc_enqueued_queues_hand_off`] then always finds
/// a taker, so every recorded list is followed by
/// [`gc_enqueued_queues_notify`]'s `synchronized (lock) { lock.notifyAll(); }`.
/// Before the first claim (no collection has linked a `Reference` into a queue
/// yet, or no thread could be started) the answer is `false` and the waiter
/// keeps the poll. The `NativeSystemAccess::reference_queue_wake_ups_delivered`
/// hook; the waiter is `rq_remove_wait` (`native-builtins/src/reference.rs`),
/// which asks only under `--jdk-only`. gc-common w6-d (2026-09-24),
/// `common-w3d-proposal-compatible-queue-remove-waits-on-lock-RETIRED-20260927`.
pub(crate) fn queue_wake_up_delivery_serves(shared: &SharedVm) -> bool {
    queue_wake_ups_recorded(shared) && shared.mem.finalizer_thread.delivery_thread_claimed()
}

/// Anything queued for the delivery thread.
fn finalizer_thread_has_work(shared: &SharedVm) -> bool {
    shared.mem.finalizer_thread.pending_count() > 0 || shared.mem.cleaner_thread.has_pending_work()
}

/// Whether `thread` holds a user-visible synchronization lock (JLS §12.6): a
/// monitor, or an `AbstractOwnableSynchronizer` (`ReentrantLock`,
/// `ReentrantReadWriteLock`'s write lock, …).
///
/// Read from the per-thread lists every `monitorenter` route and every AQS
/// ownership transition already maintain for `ThreadMXBean.getLockedMonitors()`
/// / `getLockedSynchronizers()` (`ThreadRegistry::jmx_lock_snapshot`). A miss
/// (a lock some route forgot to publish) answers `false`, i.e. the pre-w4
/// inline drain — never worse than before. Costs a registry read and two small
/// clones, so callers ask only when there is a finalizer to run.
fn thread_holds_user_locks(shared: &SharedVm, thread: &JvmThread) -> bool {
    shared
        .threads
        .thread_registry
        .jmx_lock_snapshot(thread.thread_id)
        .is_some_and(|(_, _, monitors, synchronizers, ..)| {
            !monitors.is_empty() || !synchronizers.is_empty()
        })
}

/// Hand what is queued to the delivery thread, starting it on first use.
///
/// Returns `true` when a delivery thread serves (or is starting to serve) this
/// VM, i.e. the caller must NOT drain the queues itself; `false` when the full
/// hand-off is off or no thread could be started, and the caller keeps the
/// inline drain.
///
/// Callable from inside a pause (the reference passes call it): the request is
/// a counter bump and a condvar notify, and a first-use spawn only registers a
/// STARTING thread (`register_starting_with_daemon`, not counted by any census)
/// and spawns the OS thread, which cannot become a mutator until the pause has
/// completed — the same protocol a `Thread.start()` child racing a pause uses.
/// Nothing here touches the heap.
fn wake_finalizer_thread(shared: &SharedVm) -> bool {
    if !finalizer_thread_enabled() {
        return false;
    }
    request_finalizer_thread(shared, finalizer_thread_has_work(shared))
}

/// [`wake_finalizer_thread`] without the policy gate: request a batch from
/// the delivery thread, starting it when none was ever claimed AND
/// `start_if_idle` says there is work worth a thread. `true` when a delivery
/// thread serves (or is starting to serve) the request.
fn request_finalizer_thread(shared: &SharedVm, start_if_idle: bool) -> bool {
    let ft = &shared.mem.finalizer_thread;
    if ft.delivery_thread_claimed() {
        // Serving, or starting: its first batch drains whatever was queued
        // before it could serve, and this bump makes sure that batch covers
        // our work too.
        ft.request_delivery();
        return true;
    }
    if !start_if_idle {
        // Nothing to run and nothing to start a thread for yet.
        return false;
    }
    start_finalizer_thread(shared)
}

/// The allocation doors' question for CLEANER actions (`run_cleaner_actions`):
/// does the delivery thread take this batch? Full hand-off only. On the
/// delivery thread itself (an action allocated and collected) the answer is
/// also `true` — its own loop drains the batch the nested collection
/// requested, once the action that triggered it has returned.
fn finalizer_thread_takes_delivery(shared: &SharedVm, thread: &JvmThread) -> bool {
    if !finalizer_thread_enabled() {
        return false;
    }
    if shared.mem.finalizer_thread.delivery_thread() == Some(thread.thread_id.0) {
        return true;
    }
    wake_finalizer_thread(shared)
}

/// The allocation doors' question for `finalize()` (`run_finalizers`): does
/// the delivery thread take it? Under [`FinalizerDelivery::Always`] as for
/// cleaner actions; under the default [`FinalizerDelivery::WhenLockHeld`]
/// only when there is a finalizer to run and `thread` holds a user-visible
/// lock (JLS §12.6) — then the delivery thread is started on first use and
/// this thread runs nothing. The delivery thread itself always keeps its own
/// batch.
fn finalizer_thread_takes_finalizers(shared: &SharedVm, thread: &JvmThread) -> bool {
    match finalizer_delivery_policy() {
        FinalizerDelivery::Inline => false,
        FinalizerDelivery::Always => finalizer_thread_takes_delivery(shared, thread),
        FinalizerDelivery::WhenLockHeld => {
            if shared.mem.finalizer_thread.delivery_thread() == Some(thread.thread_id.0) {
                return true;
            }
            if shared.mem.finalizer_thread.pending_count() == 0 {
                return false;
            }
            thread_holds_user_locks(shared, thread) && request_finalizer_thread(shared, true)
        }
    }
}

/// How long one slice of a forced wait lasts before the waiter checks whether
/// the delivery thread is blocked on a monitor the waiter itself holds.
const DELIVERY_WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(10);

/// Outcome of [`wait_for_delivery`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryWait {
    /// A batch covering every request made before the wait has finished.
    Served,
    /// The finalizer timeout passed first.
    TimedOut,
    /// The delivery thread is BLOCKED entering a monitor the waiter holds: it
    /// cannot finish until the waiter returns, so waiting longer is pointless.
    BlockedOnCaller,
}

/// Wait, in a blocking region (the delivery thread's Java may allocate and
/// collect meanwhile) and for at most the finalizer timeout, until a batch
/// covering every request made so far has finished.
///
/// When `caller_holds_locks`, the wait is sliced and between slices checks
/// whether the delivery thread is blocked entering a monitor this thread owns
/// ([`delivery_thread_waits_on_our_lock`]) — `synchronized (L) { … System.gc(); … }`
/// with a finalizer that synchronizes on `L`. Before w4-d that shape stalled
/// `System.gc()` for the full 2 s every time; it now returns as soon as the
/// block is seen. (A `java.util.concurrent` lock parks rather than blocks and
/// is not detected; the 2-s bound still holds for it.)
fn wait_for_delivery(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller_holds_locks: bool,
) -> DeliveryWait {
    use cratonvm_native_api::NativeThreadAccess;
    let caller = thread.thread_id;
    let signal = shared.mem.finalizer_thread.delivery_signal();
    let target = signal.ticket();
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(shared.mem.finalizer_thread.timeout_ms());
    let mut ctx = crate::vm::NativeContextImpl {
        shared,
        thread: &mut *thread,
    };
    ctx.begin_blocking_region();
    let outcome = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break if signal.wait_for_ticket(target, std::time::Duration::ZERO) {
                DeliveryWait::Served
            } else {
                DeliveryWait::TimedOut
            };
        }
        let slice = if caller_holds_locks {
            left.min(DELIVERY_WAIT_SLICE)
        } else {
            left
        };
        if signal.wait_for_ticket(target, slice) {
            break DeliveryWait::Served;
        }
        if caller_holds_locks && delivery_thread_waits_on_our_lock(shared, caller) {
            break DeliveryWait::BlockedOnCaller;
        }
    };
    ctx.end_blocking_region();
    outcome
}

/// Whether the delivery thread is BLOCKED entering a monitor `caller` owns,
/// read from the same JMX lists `ThreadMXBean` reports (no heap access, safe
/// in a blocking region). The two snapshots are taken separately, so a
/// relocation between them can only make this miss (keep waiting), and a
/// false `true` costs only the ordering a lock-holding `System.gc()` caller
/// was never promised by HotSpot anyway.
fn delivery_thread_waits_on_our_lock(shared: &SharedVm, caller: crate::ThreadId) -> bool {
    let Some(delivery) = shared.mem.finalizer_thread.delivery_thread() else {
        return false;
    };
    let registry = &shared.threads.thread_registry;
    let Some((Some(contended), ..)) = registry.jmx_lock_snapshot(crate::ThreadId(delivery)) else {
        return false;
    };
    registry
        .jmx_lock_snapshot(caller)
        .is_some_and(|(_, _, owned, ..)| owned.iter().any(|o| o.as_ptr() == contended.as_ptr()))
}

/// `System.gc()`'s CLEANER drain with the full hand-off on: request a batch
/// and wait (see [`wait_for_delivery`]). `true` when the queues were served by
/// the delivery thread; `false` when the hand-off is off, the caller IS the
/// delivery thread, no thread could be started, or the wait ended unserved —
/// the caller then drains inline, as before: that drain is
/// `Bits.reserveMemory`'s last resort, which throws the moment `System.gc()`
/// returns without the native memory freed.
fn finalizer_thread_drained_for(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    if !finalizer_thread_enabled() {
        return false;
    }
    if shared.mem.finalizer_thread.delivery_thread() == Some(thread.thread_id.0) {
        return false;
    }
    if !wake_finalizer_thread(shared) {
        return false;
    }
    let holds = thread_holds_user_locks(shared, thread);
    wait_for_delivery(shared, thread, holds) == DeliveryWait::Served
}

/// The forced `finalize()` drain's hand-off (`System.gc()`,
/// `Runtime.runFinalization()`): `true` when the caller must NOT run
/// finalizers itself — the delivery thread served them, or it is still
/// running them and the caller holds a user-visible lock (JLS §12.6: then the
/// queue stays with the delivery thread, which finishes once the lock is
/// released, rather than being run here re-entrantly — what HotSpot's
/// `runFinalization` would do is deadlock). `false`: drain inline, as before.
///
/// Per policy: `Inline` never hands off; `Always` always does and, unserved,
/// falls back inline only for a caller holding no lock (the pre-w4 rule for
/// the w3 fallback, minus the §12.6 case); `WhenLockHeld` hands off only for a
/// caller holding a lock.
fn finalizers_handed_off_for_forced_drain(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    let policy = finalizer_delivery_policy();
    if policy == FinalizerDelivery::Inline {
        return false;
    }
    if shared.mem.finalizer_thread.delivery_thread() == Some(thread.thread_id.0) {
        return false;
    }
    let holds = thread_holds_user_locks(shared, thread);
    let requested = match policy {
        FinalizerDelivery::Always => wake_finalizer_thread(shared),
        FinalizerDelivery::WhenLockHeld => {
            holds
                && request_finalizer_thread(
                    shared,
                    shared.mem.finalizer_thread.pending_count() > 0,
                )
        }
        FinalizerDelivery::Inline => false,
    };
    if !requested {
        return false;
    }
    match wait_for_delivery(shared, thread, holds) {
        DeliveryWait::Served => true,
        DeliveryWait::TimedOut | DeliveryWait::BlockedOnCaller => holds,
    }
}

/// Spawn this VM's delivery thread. `true` when it was spawned (or another
/// caller is spawning it), `false` when it could not be.
fn start_finalizer_thread(shared: &SharedVm) -> bool {
    let ft = &shared.mem.finalizer_thread;
    if !ft.claim_delivery_thread_start() {
        ft.request_delivery();
        return true;
    }
    // A `SharedVm` not built by `Vm::new` (unit fixtures) has no owning handle
    // to give a thread; such a VM keeps the inline drain.
    let Some(owner) = shared.try_get_arc() else {
        ft.release_delivery_thread_start();
        return false;
    };
    let weak = std::sync::Arc::downgrade(&owner);
    drop(owner);
    let signal = ft.delivery_signal();
    // Queued before the thread exists; its first batch serves it.
    signal.request();
    let tid = shared.threads.thread_registry.next_thread_id();
    // STARTING, daemon: not counted by any STW census until the thread itself
    // passes `mark_stw_ready`, and never waited for at exit.
    shared
        .threads
        .thread_registry
        .register_starting_with_daemon(tid, FINALIZER_THREAD_NAME, None, true);
    // Same native stack as a `Thread.start()` worker (see `thread_start`).
    let stack_size = cratonvm_types::flags::runtime_var("RUST_MIN_STACK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(8 * 1024 * 1024);
    let spawned = std::thread::Builder::new()
        .name(FINALIZER_THREAD_NAME.to_string())
        .stack_size(stack_size)
        .spawn(move || finalizer_thread_main(weak, signal, tid, stack_size));
    match spawned {
        // gen r5w5/conc9: the handle goes to the registry, as a
        // `Thread.start()` worker's does, so a delivery thread that UNWINDS
        // past `mark_dead` (a Rust panic under a finalizer) is dead to every
        // stop-the-world census (`entry_is_stw_live`) instead of being waited
        // for, running, forever. A normal exit marks itself dead first and the
        // handle is detached there. Found by the service-thread audit
        // (`gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection`).
        Ok(handle) => {
            shared.threads.thread_registry.set_join_handle(tid, handle);
            true
        }
        Err(e) => {
            shared.threads.thread_registry.mark_dead(tid);
            ft.release_delivery_thread_start();
            tracing::warn!(
                target: "cratonvm::gc::finalizer",
                error = %e,
                "could not start the reference-delivery thread; finalizers and cleaner \
                 actions keep running inline on the collecting thread"
            );
            false
        }
    }
}

/// Body of the delivery thread. Mirrors the worker wiring of `thread_start`
/// (`vm_exec.rs`) for everything a GC initiator reads about a thread, and the
/// idle discipline of the JNI AIO dispatcher (`jni.rs`, `aio_dispatcher_main`):
/// GC-blocked while idle, a counted mutator only while running a batch.
fn finalizer_thread_main(
    weak: std::sync::Weak<SharedVm>,
    signal: std::sync::Arc<cratonvm_gc::reference::DeliverySignal>,
    tid: crate::ThreadId,
    stack_size: usize,
) {
    use cratonvm_native_api::NativeThreadAccess;
    crate::runtime::interpreter::init_thread_exec_depth_ceiling(stack_size);
    let Some(mut shared) = weak.upgrade() else {
        return;
    };
    // Never moved after its addresses are published below; dropped only when
    // the VM is gone.
    let mut thread = JvmThread::new(tid, FINALIZER_THREAD_NAME);
    thread.kind = crate::threading::ThreadKind::Platform;
    thread.daemon = true;
    {
        let registry = &shared.threads.thread_registry;
        registry.set_interrupted_flag(tid, thread.interrupted.clone());
        registry.set_park_state(tid, thread.park_state.clone());
        registry.set_root_snapshot(tid, thread.root_snapshot.clone());
        registry.set_frame_trace(tid, thread.frame_trace.clone());
        registry.set_vm_state(tid, thread.vm_state.clone());
        registry.set_gc_block_state(tid, thread.gc_block_state.clone());
        registry.set_tlab_addr(tid, &thread.tlab as *const cratonvm_gc::Tlab as usize);
        registry.set_jvm_thread_addr(tid, &thread as *const JvmThread as usize);
        registry.set_os_tid_current(tid);
    }
    cratonvm_classloading::set_current_thread_id(shared.classes.class_layout_domain, tid.0);
    let _crash_frames = crate::vm::PublishedFrameTrace::publish(
        FINALIZER_THREAD_NAME,
        tid.0,
        thread.frame_trace.clone(),
    );
    thread.set_vm_state("finalizer-thread:registered");
    // Become STW-visible exactly as a `Thread.start()` worker does.
    loop {
        let ready = shared.mem.gc_barrier.run_if_no_stw_requested(|| {
            shared.threads.thread_registry.mark_stw_ready(tid);
        });
        if ready {
            break;
        }
        let pointer_map = shared.mem.gc_barrier.arrive_and_wait_excluded(tid);
        if !pointer_map.is_empty() {
            apply_pointer_map_to_thread(
                &mut thread,
                &pointer_map,
                &shared.mem.heap,
                &shared.classes.type_maps,
            );
        }
    }
    if shared
        .mem
        .gc_barrier
        .stw_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        safepoint_check(&shared, &mut thread);
    }
    shared.mem.finalizer_thread.set_delivery_thread(tid.0);
    loop {
        // Idle: GC-blocked (deposited snapshot, retired TLAB), holding only
        // the weak handle, so a pause never waits for this thread and the VM
        // can be dropped under it.
        thread.set_vm_state("finalizer-thread:idle");
        {
            let mut ctx = crate::vm::NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.begin_blocking_region();
        }
        drop(shared);
        loop {
            if signal.wait_for_request(FINALIZER_THREAD_IDLE_TICK) {
                break;
            }
            if weak.strong_count() == 0 {
                return;
            }
        }
        shared = match weak.upgrade() {
            Some(s) => s,
            None => return,
        };
        {
            // Waits out an in-flight pause and applies what it moved.
            let mut ctx = crate::vm::NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.end_blocking_region();
        }
        thread.set_vm_state("finalizer-thread:batch");
        let ticket = signal.begin_batch();
        drain_finalizer_thread_batch(&shared, &mut thread);
        signal.finish_batch(ticket);
    }
}

/// One batch of the delivery thread: queue wake-ups, `finalize()`, cleaner
/// actions, then any wake-ups the batch's own collections queued. Runs on the
/// delivery thread at top level (no JIT borrow), so the non-forced drains run.
///
/// Also delivers any queued GC notifications (JMX `NotificationListener`s,
/// `run_gc_notifications`) whenever the doors hand them here — the full
/// hand-off, and since gc-common w6-d every `--jdk-only` VM
/// (`gc_notification_delivery`). Same one-drainer-at-a-time queue
/// the doors drain. A `--compatible` VM's doors deliver their own unless the
/// collecting thread holds a user-visible lock (gc-common w7-d, see
/// [`gc_notifications_go_to_delivery_thread`]).
fn drain_finalizer_thread_batch(shared: &SharedVm, thread: &mut JvmThread) {
    // gc-common w4-d: under the default policy the thread exists only to run
    // the finalizers a lock-holding mutator could not (JLS §12.6); cleaner
    // actions, queue wake-ups (never recorded then) and GC notifications keep
    // their inline doors, so `--compatible` output changes only in that case.
    //
    // gc-common w5-d: except that a `--jdk-only` VM records its queue wake-ups
    // under the default policy too (`queue_wake_ups_recorded_for`), and this is
    // where they are delivered. Under `--compatible` nothing is ever recorded,
    // so both calls take the empty list and return at once.
    if !finalizer_thread_enabled() {
        gc_enqueued_queues_notify(shared, thread);
        run_finalizers_impl(shared, thread, false);
        gc_enqueued_queues_notify(shared, thread);
        // gc-common w6-d: and a `--jdk-only` VM's GC notifications, which its
        // doors hand here (`gc_notification_delivery`).
        // gc-common w7-d: a `--compatible` VM's doors hand them here only when
        // the collecting thread held a user-visible lock, and mark it; the
        // mark is taken AFTER the finalizers, so a finalizer that allocated
        // under a lock is covered too. Unmarked, a lock-free door is delivering
        // its own and this batch leaves them to it.
        match gc_notification_delivery(shared) {
            GcNotificationDelivery::OffMutator => {
                run_gc_notifications_as_notification_thread(shared, thread)
            }
            GcNotificationDelivery::WhenLockHeld => {
                if shared.mem.finalizer_thread.take_gc_notifications_handed_off() {
                    run_gc_notifications_as_notification_thread(shared, thread);
                }
            }
            GcNotificationDelivery::Inline => {}
        }
        return;
    }
    gc_enqueued_queues_notify(shared, thread);
    run_finalizers_impl(shared, thread, false);
    run_cleaner_actions_impl(shared, thread, false);
    gc_enqueued_queues_notify(shared, thread);
    run_gc_notifications_as_notification_thread(shared, thread);
}

/// HotSpot's name for the thread its GC notifications run on
/// (`NotificationThread`, JDK 14+; the `Service Thread` before).
const NOTIFICATION_THREAD_NAME: &str = "Notification Thread";

/// The delivery thread's GC-notification step
/// ([`run_gc_notifications`]), run under HotSpot's thread name. gcd d10/o
/// (2026-09-28), item 2 of
/// `docs/known-issues/gc/common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications.md`.
///
/// A `NotificationListener` on a GC bean that asked `Thread.currentThread()
/// .getName()` answered `Craton Finalizer` -- this VM's reference-delivery
/// thread, which also runs `finalize()` and cleaner actions -- where HotSpot
/// answers `Notification Thread` on every collector
/// (`GcNotificationThreadProbe`: `threads=[Notification Thread]` on Serial,
/// G1 and ZGC, Temurin 25.0.3). One thread still does all three jobs; only
/// the NAME the listeners see follows the job: for the duration of the
/// delivery, the Rust-side name (what a mirror built lazily during the
/// delivery is named from, `build_current_thread_object`) and the Java
/// mirror's `name` field say `Notification Thread`, and both are put back
/// afterwards, so a `finalize()` on this thread sees what it saw before.
///
/// Heap-safe by construction: the two renames allocate one short String each
/// through the single-attempt, non-collecting
/// `try_create_java_string_from_units` (no `JvmThread` is handed to it, so it
/// can neither collect nor park at a safepoint, and the mirror read before it
/// is still current after it). A refused allocation leaves the name as it was
/// -- a wrong name, never a failed delivery. A listener that calls
/// `setName` on this thread has its name replaced when the delivery ends.
///
/// Only the delivery thread's own deliveries are renamed. A `--compatible`
/// door that delivers inline (a lock-free collecting thread, gc-common w7-d)
/// still runs the listeners on the collecting thread under its own name;
/// that is the policy [`gc_notifications_go_to_delivery_thread`] states.
fn run_gc_notifications_as_notification_thread(shared: &SharedVm, thread: &mut JvmThread) {
    if !super::gc_events::gc_notifications_pending(shared) {
        return;
    }
    let previous = std::mem::replace(&mut thread.name, NOTIFICATION_THREAD_NAME.to_string());
    set_delivery_mirror_name(shared, thread);
    run_gc_notifications(shared, thread, false);
    thread.name = previous;
    set_delivery_mirror_name(shared, thread);
}

/// Write `thread.name` into its Java mirror's `name` field, when the thread
/// has a mirror and its class declares the field (a unit VM's synthetic
/// `Thread` may not). See [`run_gc_notifications_as_notification_thread`] for
/// why the allocation cannot move the mirror.
fn set_delivery_mirror_name(shared: &SharedVm, thread: &JvmThread) {
    let Some(mirror) = thread.java_thread_obj else {
        return;
    };
    let class_id = shared.mem.heap.class_id_of(mirror);
    let Some(slot) = crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, class_id, "name")
    else {
        return;
    };
    let units: Vec<u16> = thread.name.encode_utf16().collect();
    let Some(name) = crate::vm::try_create_java_string_from_units(shared, &units) else {
        return;
    };
    shared.mem.heap.set_field(mirror, slot, Value::Object(Some(name)));
}

/// Do for every queue the collector spliced a `Reference` onto what the JDK's
/// `ReferenceQueue.enqueue0` does after linking: `synchronized (lock) {
/// lock.notifyAll(); }` — so a thread already blocked in `remove()` sees it.
///
/// `common-w2f-gc-enqueue-never-wakes-queue-waiters-FIXED-20260923.md`:
/// under `--jdk-only` the real `remove()` is `lock.wait()` and nothing woke it
/// — the Common Cleaner learned of a GC-enqueued `PhantomCleanable` only at
/// its 60 s timeout, and an untimed `remove()` possibly never. Taking the
/// monitor (not a bare wake) is what makes this race-free: a waiter between
/// its empty `poll0()` and its `wait()` holds `lock`, so the notify lands
/// after it waits.
///
/// Only real-layout queues have `lock`; the synthetic two-slot shape and a
/// queue whose `lock` is still null are skipped (nothing can be waiting on a
/// monitor there — the default mode's natives poll). Delivery thread only:
/// the monitor enter may block, which is why the pause cannot do this.
fn gc_enqueued_queues_notify(shared: &SharedVm, thread: &mut JvmThread) {
    use cratonvm_native_api::{NativeHeapAccess as _, NativeThreadAccess as _};
    let queues = shared.mem.cleaner_thread.drain_queues_to_notify();
    if queues.is_empty() {
        return;
    }
    // Pinned for the batch: each monitor enter may wait out a moving pause.
    let pin_base = thread.native_pin_roots.len();
    for &addr in &queues {
        if shared.mem.heap.is_heap_addr(addr).is_none() {
            continue;
        }
        // SAFETY: recorded by a reference pass from a live, shape-screened
        // `ReferenceQueue`, rooted (`CleanerThread::pending_addresses`) and
        // relocated (`CleanerThread::update_after_gc`) on every collection
        // since, and nothing has run between the drain above and here.
        thread
            .native_pin_roots
            .push(unsafe { ObjectRef::from_raw(addr as *mut u8) });
    }
    let pin_end = thread.native_pin_roots.len();
    for pin in pin_base..pin_end {
        let queue = thread.native_pin_roots[pin];
        // Not an array: its header would carry the COMPONENT's class id, and a
        // `ReferenceQueue[]` would pass as a queue (see `is_cleanable_shaped`).
        let is_array = shared.mem.heap.kind_of(queue) == crate::memory::heap::ObjectKind::Array;
        let shaped = !is_array && {
            let cm = shared.classes.class_manager.read();
            let cid = shared.mem.heap.class_id_of(queue);
            cm.is_assignable_to_name(cid, "java/lang/ref/ReferenceQueue")
        };
        if !shaped {
            continue;
        }
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        // gc-common w6-d: `ReferenceQueue`'s OWN `lock`, resolved on the
        // declaring class. By name on the receiver's class, a subclass that
        // declares a field of its own called `lock` (`class Q extends
        // ReferenceQueue<Object> { final Object lock = new Object(); }`) got
        // this notify, and a waiter on the real `lock` — the JDK's `remove()`
        // bytecode, or the native `remove` since w6-d (`rq_wait_on_lock`) —
        // slept on.
        let Some(lock_slot) = ctx.resolve_field_index("java/lang/ref/ReferenceQueue", "lock")
        else {
            continue;
        };
        if lock_slot >= ctx.object_num_fields(queue) {
            continue;
        }
        let Value::Object(Some(lock)) = ctx.get_field(queue, lock_slot) else {
            continue;
        };
        // The gc-safe enter: a contended wait is excused from a pause, so the
        // monitor object may move; use what it returns.
        let lock = ctx.monitor_enter_gc_safe(lock);
        let _ = ctx.monitor_notify_all(lock);
        ctx.monitor_exit(lock);
    }
    thread.native_pin_roots.truncate(pin_base);
}

/// Resolve the field slot used to link a `java.lang.ref.Reference` (or
/// subclass instance) onto its `ReferenceQueue`'s linked list when the GC
/// auto-enqueues it -- the `next` field as declared on `java/lang/ref/Reference`
/// itself (referent, queue, next, discovered — real-JDK layout).
///
/// MUST be resolved BY NAME against `Reference`'s own declaring class, not
/// by a field-count heuristic on the receiver's most-derived class: a
/// `java.util.WeakHashMap$Entry` (itself a `WeakReference` subclass) also
/// declares its OWN field named `next` (used for its hash-BUCKET chain, a
/// completely different linked list). The old heuristic ("index 2 if the
/// object has more than 2 fields") cannot tell these apart — for a
/// `WeakHashMap$Entry` specifically it can land on the wrong `next` slot,
/// so a GC-driven auto-enqueue (a live-application-scale WeakHashMap
/// *will* eventually have a stale entry to expunge) splices the
/// ReferenceQueue's link over the bucket-chain link, corrupting whatever
/// hash bucket that entry lived in — a later `WeakHashMap.get()`/
/// `expungeStaleEntries()` walking that bucket's `Entry.next` chain then
/// loops forever (observed: `com.sun.beans.TypeResolver`'s internal
/// `WeakCache`'s `WeakHashMap.get()` permanently stuck inside
/// `matchesKey()`, hanging Spring Boot's Thymeleaf layout-dialect
/// `createLayoutFromConfigClass` test). See
/// thymeleaf-groovy-layoutdialect-metaclass-introspection-hang-FIXED.md.
pub(super) fn gc_reference_next_slot(shared: &SharedVm) -> usize {
    let cm = shared.classes.class_manager.read();
    cm.find_bootstrap_class_by_name("java/lang/ref/Reference")
        .and_then(|reference_cid| {
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(
                reference_cid,
                "next",
                cm.class_store(),
            )
        })
        // `java/lang/ref/Reference` should always be loaded by the time any
        // Reference object exists to enqueue; this fallback only guards
        // against that invariant somehow not holding.
        .unwrap_or(2)
}

/// True when the object at `cleanable` still has a shape a pending Cleaner
/// action can legitimately have.
///
/// Two shapes reach the cleaner queue, and both are checked because both are
/// registered: the real JDK's `jdk.internal.ref.Cleaner` and
/// `jdk.internal.ref.PhantomCleanable` are `java.lang.ref.Reference` subclasses
/// (discovered from the `PhantomReference.<init>` native), and the synthetic
/// Cleanable that `phases_late`/`servlet` build implements
/// `java.lang.ref.Cleaner$Cleanable`. Anything else at the address means the
/// address was reclaimed and its slot reused.
///
/// Why it matters, and it is not the same bug as the queue-head one:
/// `run_cleaner_actions_impl` WRITES slot 1 (the cleaned flag) and NULLS slot 0
/// (the action) before it invokes anything. Through a reused address that is a
/// field-0 null on an innocent object — on a `java.lang.String` it nulls
/// `value`, and the `byte[]` that vanishes surfaces far away, in a frame with no
/// connection to the GC, as H2 `TestMultiThread.testViews`'
/// `NullPointerException: Cannot read the array length because "<local4>" is
/// null`. Measured 2026-08-16: that NPE outlived the `ReferenceQueue` fixes in
/// this file and was still reproducible under `-XX:+UseGenerationalGC`, which is
/// how this second producer was separated from the first.
fn is_cleanable_shaped(
    cm: &crate::classloading::ClassManager,
    shared: &SharedVm,
    cleanable: ObjectRef,
) -> bool {
    // An array's header carries its COMPONENT's class id, so a
    // `ThreadLocalMap.Entry[]` (Entry extends WeakReference) would pass as
    // Reference-shaped and have its elements read as fields. Same screen as
    // `is_reference_shaped` in the enqueue loop.
    if shared.mem.heap.kind_of(cleanable) == crate::memory::heap::ObjectKind::Array {
        return false;
    }
    let cid = shared.mem.heap.class_id_of(cleanable);
    cm.is_assignable_to_name(cid, "java/lang/ref/Reference")
        || cm.is_assignable_to_name(cid, "java/lang/ref/Cleaner$Cleanable")
}

/// `java.lang.ref.ReferenceQueue.ENQUEUED`, at its POST-collection address, for
/// the GC's own auto-enqueue to store in a real-layout `Reference.queue`.
/// `None` when the class or its static is not there yet (nothing to store but
/// the synthetic mark).
///
/// **W2-F-3 (2026-09-23).** The two GC enqueue loops (`process_references_after_gc`
/// and `g1_remark_process_references`) marked an enqueued reference by writing
/// the synthetic `Int(1)` into slot 1. On the two-slot synthetic shape that
/// slot is untyped and the sentinel survives. On the real JDK layout slot 1 is
/// `queue : Ljava/lang/ref/ReferenceQueue;`, where a primitive is coerced to
/// null (the G30-1 slot coercion `native_ref_is_enqueued`'s doc describes), so
/// every reference the COLLECTOR enqueued read back with `queue == null`:
///
/// * `isEnqueued()` answered `false` for it until polled (HotSpot: `true`) —
///   the "one remaining shape" G49-1 §NOMINATION left open;
/// * real `Reference.enqueue()` bytecode (`this.queue.enqueue(this)`, the
///   path `--jdk-only` runs) threw `NullPointerException` where HotSpot
///   answers `false`;
/// * `ReferenceQueue.enqueue0`'s own "already enqueued" test
///   (`queue == ENQUEUED`) could not see it.
///
/// The JDK's `enqueue0` stores `ENQUEUED`; so does this now.
///
/// `pointer_map` relocates the static's value: the VM's statics are remapped
/// by `update_all_roots`, which every caller runs AFTER the reference pass, so
/// they still hold the pre-collection address here. The caller must still
/// shape-check the result (a `ReferenceQueue`) and fall back to `Int(1)`.
///
/// Deliberately uncached: a cache would be a process global holding one VM's
/// class id, and this runs once per collection.
fn gc_enqueued_sentinel(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
) -> Option<ObjectRef> {
    let (class_id, index) = {
        let cm = shared.classes.class_manager.read();
        let class_id = cm.find_bootstrap_class_by_name("java/lang/ref/ReferenceQueue")?;
        let class = cm.get_class(class_id)?;
        let index = class
            .fields
            .iter()
            .filter(|f| f.is_static())
            .position(|f| &*f.name == "ENQUEUED")?;
        (class_id, index)
    };
    match crate::vm::get_static_shared(shared, class_id, index) {
        Value::Object(Some(sentinel)) => {
            let addr = sentinel.as_ptr() as usize;
            let now = pointer_map.get(&addr).copied().unwrap_or(addr);
            // SAFETY: the static's value, relocated through this collection's
            // own map; the caller shape-checks it before writing it anywhere.
            Some(unsafe { ObjectRef::from_raw(now as *mut u8) })
        }
        _ => None,
    }
}

/// The value the GC enqueue loops store in `Reference.queue`: the JDK's
/// `ReferenceQueue.ENQUEUED` on the real layout (`next_slot != 0`, i.e. the
/// Reference has a `next` field), the synthetic `Int(1)` otherwise or when the
/// sentinel is unavailable. See [`gc_enqueued_sentinel`].
fn gc_enqueued_mark(sentinel: Option<ObjectRef>, next_slot: usize) -> Value {
    match sentinel {
        Some(s) if next_slot != 0 => Value::Object(Some(s)),
        _ => Value::Int(1),
    }
}

/// Process weak/soft references after a GC cycle.
/// Calls the ReferenceProcessor, nulls referent fields of cleared references,
/// and relocates ref processor addresses using the pointer map.
pub(super) fn process_references_after_gc(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    cycle_roots: &[ObjectRef],
    reconcile_exact_results: Option<&std::collections::HashMap<usize, bool>>,
) {
    // gc-common w16-x: one capture of the Generational young free list for
    // every weak side-table verdict below, instead of one per row. Nothing in
    // this function allocates on the heap, so it cannot go stale before the
    // last sweep; the free-list probe itself is taken lazily, at the first
    // sweep that needs it (after the unload transaction). See
    // `addr_keyed::InPlaceVerdict`.
    //
    // gcd d10/r: taken with this pause's relocations, and captured here (it
    // used to be taken after `gen_stw_layout_census`) so every liveness
    // verdict of this function shares its lazily-built destination set. An
    // address a slide moved another survivor ONTO is not the object that was
    // there: without the screen a dead `ClassLoader`, mirror, overlay owner,
    // proxy `Method`, weak-row owner, `Reference` or referent whose base a ZGC
    // slide or a Generational compaction re-issued read as LIVE (the address
    // parses as an object -- the survivor's) and kept its row, which then
    // answered for the survivor. `addr_keyed::RelocationTargets`; a no-op on
    // G1, on a non-moving pause and on a Generational cycle that reclaimed no
    // old storage.
    let in_place =
        crate::memory::addr_keyed::InPlaceVerdict::capture_after(&shared.mem.heap, pointer_map);
    // An address that is NOT a key of the map (a key moved away, and every
    // verdict below answers it through the map) and that this pause relocated
    // another object onto: whatever lived there before the pause is dead.
    let claimed = |addr: usize| -> bool {
        !pointer_map.contains_key(&addr) && in_place.claimed_by_relocation(addr)
    };
    // HIB-CV-24 (Manifestation B): reconcile the defining-loader side-table with
    // this collection. A user `ClassLoader` the application no longer references
    // is now collectable (it is not GC-rooted under `CRATONVM_LOADER_UNLOAD`);
    // prune its stale side-table entry and remap survivors using the SAME
    // survivor predicate the reference processor uses below. Done BEFORE the
    // diagnostic `no_refproc` short-circuit so the side-table never holds a
    // dangling/stale ObjectRef after a collection, independent of that switch.
    {
        let is_marked = |addr: usize| -> bool {
            pointer_map.contains_key(&addr)
                || (shared.mem.heap.is_addr_live(addr) && !claimed(addr))
        };
        let dead_class_hints = cratonvm_native_builtins::classloader::gc_reconcile_defining_loaders(
            shared.vm_identity,
            &is_marked,
            pointer_map,
            reconcile_exact_results,
        );
        // The reconcile rebuilt this VM's loader pins from the defining-loader
        // table, which has no lambda-proxy rows; re-add them (a host that died
        // this cycle has no row left, so its proxies get none).
        crate::runtime::invokedynamic::repin_lambda_proxy_loaders(shared.vm_identity);
        let unload = crate::memory::gc::unload_dead_class_metadata(shared, &dead_class_hints);
        if unload.classes_unloaded != 0 {
            tracing::debug!(
                loaders = unload.loaders_unloaded,
                classes = unload.classes_unloaded,
                jit_entries = unload.jit_entries_retired,
                "class-loader metadata unloaded"
            );
        }
        // gcd d2/g: a stop-the-world collection that reclaimed old storage
        // takes the retained-layout census too (nothing to do unless a
        // concurrent remark retained a layout: one thread-local read, and one
        // leaf-mutex read after an old reclamation).
        gen_stw_layout_census(shared);
        // (`in_place`, the one in-place verdict every weak side-table sweep
        // below takes, is captured at the top of this function -- gcd d10/r.)
        // gc-common w11-d: the Locale subtag side tables are weak (their keys
        // are no longer roots), so the rows of Locales this collection did not
        // keep are dropped here, on every stop-the-world cycle including the
        // non-moving ones, before any allocation can reuse a freed address.
        // Keys are still PRE-remap (`update_all_roots` re-keys the survivors
        // later): a moved key is in the pointer map, an unmoved one is judged
        // by the `addr_keyed::survived_in_place` verdict every VM-driven
        // address-keyed sweep uses. See `native-builtins` `gc_sweep_locale_rows`.
        cratonvm_native_builtins::gc_sweep_locale_rows(shared.vm_identity, &|addr| {
            pointer_map.contains_key(&addr) || in_place.survived(addr)
        });
        // gc-common w12-a: the TLS rows owned by an `SSLEngine` / `SSLSession`
        // / `SSLParameters` / real `HttpURLConnection` go with their owner --
        // same verdict, same moment (the `tls` root row re-addresses the
        // survivors in `update_all_roots`). See `native-builtins`
        // `gc_sweep_tls_rows`.
        cratonvm_native_builtins::gc_sweep_tls_rows(shared.vm_identity, &|addr| {
            pointer_map.contains_key(&addr) || in_place.survived(addr)
        });
        // gc-common w14-d: the JUL rows keyed on a Logger / Handler /
        // FileHandler, and the Tomcat JULI registries keyed on a context class
        // loader, go with their key -- same verdict, same moment (the
        // `logmanager` root row re-keys the survivors in `update_all_roots`).
        // See `native-builtins` `logmanager::gc_sweep_logging_rows`.
        cratonvm_native_builtins::logmanager::gc_sweep_logging_rows(shared.vm_identity, &|addr| {
            pointer_map.contains_key(&addr) || in_place.survived(addr)
        });
        // gc-common w14-c: the lock-key registry's slots go with their object
        // (or become a tombstone while an `SSLContext` key may still be
        // named), same verdict, after the TLS rows; the survivors are then
        // re-addressed through `pointer_map`, which is the registry's only
        // remap. See `native-builtins` `gc_sweep_and_remap_lock_keys`.
        cratonvm_native_builtins::gc_sweep_and_remap_lock_keys(
            shared.vm_identity,
            &|addr| pointer_map.contains_key(&addr) || in_place.survived(addr),
            pointer_map,
        );
        // gc-common w14-e: native-io's weak side tables (the
        // `BufferedOutputStream` / `PipedOutputStream` side buffers) -- same
        // verdict, same moment. Owners are PRE-remap here; the sweep
        // re-addresses a moved owner itself through `pointer_map`, so no
        // `native_roots.rs` remap row is needed. See `native-io`
        // `gc_sweep_io_side_tables`.
        cratonvm_native_io::gc_sweep_io_side_tables(shared.vm_identity, pointer_map, &|addr| {
            in_place.survived(addr)
        });
        // gc-common w15-f: the `com.sun.net.httpserver` link rows (server,
        // executor, authenticator and attribute-map roots keyed on their
        // owner) go with their owner -- same verdict, same moment. Owners are
        // PRE-remap here; the sweep re-addresses a moved owner itself through
        // `pointer_map`, like the native-io sweep above. See `native-builtins`
        // `phases_late::net_channels::gc_sweep_http_link_rows`.
        cratonvm_native_builtins::phases_late::net_channels::gc_sweep_http_link_rows(
            shared.vm_identity,
            pointer_map,
            &|addr| in_place.survived(addr),
        );
        // gc-common w16-a: the `java.net` socket rows (a dead unclosed
        // `Socket` has its stream closed), the `SSLSessionContext` carrier
        // roots and bindings, the `SSLServerSocket` option delegates and the
        // `HttpClient` shutdown rows go with their owner -- same verdict,
        // same moment. Owners are PRE-remap here; the sweep re-addresses a
        // moved owner itself through `pointer_map`, like the two sweeps above.
        // See `native-builtins` `net_phase_e::gc_sweep_net_socket_rows`.
        cratonvm_native_builtins::net_phase_e::gc_sweep_net_socket_rows(
            shared.vm_identity,
            pointer_map,
            &|addr| in_place.survived(addr),
        );
        // gc-common w17-c: the `java.util.zip` side tables (an unclosed
        // stream's decoded payload, a Spring Boot `FileAccess` snapshot, an
        // un-`end()`ed Deflater/Inflater) and native-io's `ZipFile`/`JarFile`
        // tables (an unclosed archive is released, as HotSpot's `ZipFile`
        // cleaner does) go with their owner -- same verdict, same moment.
        // Owners are PRE-remap here; the sweep re-addresses a moved owner
        // itself through `pointer_map`, like the sweeps above. See
        // `native-builtins` `phases_late::zip_streams::gc_sweep_zip_rows`.
        cratonvm_native_builtins::phases_late::zip_streams::gc_sweep_zip_rows(
            shared.vm_identity,
            pointer_map,
            &|addr| in_place.survived(addr),
        );
        // gc-common w18-g: the synthetic `SubmissionPublisher`
        // closed-exception rows go with their publisher (its Throwable's
        // global root is released by that VM's next `closeExceptionally`).
        // Owners are PRE-remap here; the sweep re-addresses a moved owner
        // itself through `pointer_map`, like the sweeps above. See
        // `native-builtins` `phases_late::concurrent::gc_sweep_submission_publisher_rows`.
        cratonvm_native_builtins::phases_late::concurrent::gc_sweep_submission_publisher_rows(
            shared.vm_identity,
            pointer_map,
            &|addr| in_place.survived(addr),
        );
        // gc-common w20-g: the Undertow `HeaderMap` object rows (and their
        // header entries) and `Undertow$Builder` configuration rows go with
        // their owner -- same verdict, same moment. Owners are PRE-remap here;
        // the sweep re-addresses a moved owner itself through `pointer_map`,
        // like the sweeps above. See `native-builtins`
        // `wildfly_undertow::gc_sweep_undertow_rows`.
        cratonvm_native_builtins::wildfly_undertow::gc_sweep_undertow_rows(
            shared.vm_identity,
            pointer_map,
            &|addr| in_place.survived(addr),
        );
        // Prune dead entries from the overlay-backed-collection side-tables
        // (LinkedList / LinkedHashMap / TreeMap / TreeSet — `roots.rs` step 17
        // / `native_collections::gc_scan_collection_overlay_roots`). This
        // function existed but was never called from anywhere in the tree
        // (confirmed: `gc_prune_dead_collection_overlays` had zero call
        // sites) — a collection whose OWN object becomes genuinely
        // unreachable left its registry entry (and every element it ever
        // held) permanently behind, since nothing ever shrank these tables.
        // Wiring this in is a real, independent, safe fix (verified: prunes
        // ~1000/1470 stale entries per GC cycle in the Tomcat suite) with no
        // change to rooting behavior — it only removes bookkeeping for
        // collections `is_marked` already agrees are dead.
        //
        // NOTE: this does NOT fully close
        // `defaultinstancemanager-classunloading-count-mismatch-FIXED.md`.
        // `roots.rs` step 17 itself has a separate, deeper bug this session
        // found but did not fix: `gc_scan_collection_overlay_roots` roots
        // EVERY element of EVERY overlay-backed collection unconditionally,
        // with no gate on whether the backing collection is reachable. A
        // scratch `List<StackMapFrame>` the JDT compiler uses transiently
        // during JSP compilation gets its elements force-rooted this way;
        // forward-tracing from that illegitimate root walks back through the
        // compiler's real field references into the evicted JSP's
        // `JspServletWrapper` and its `ClassLoader`, keeping the whole
        // cluster permanently, artificially reachable — confirmed via a
        // root-membership closure check (21 direct hits, all contributed by
        // step 17, not by any other root source). A full fix needs the same
        // conditional-rooting + mark-time-propagation treatment this session
        // gave `class_mirrors` (see `cratonvm_types::mirror_pin`), but scoped
        // to every overlay table instead of just one cache — a materially
        // larger, higher-risk change than fit in this session; left for a
        // dedicated follow-up.
        //
        // Called here (not `update_all_roots`/gc.rs, where the existing
        // remap call `gc_update_collection_overlay_refs` lives) for the same
        // reason `reconcile_class_mirrors` is here and not there:
        // `update_all_roots` early-returns when `pointer_map` is empty (the
        // common case for the non-moving JIT-active sweep), so it would
        // never run for that path. `is_marked` already handles PRE-GC
        // addresses correctly for both the moving and non-moving cases
        // (pointer_map lookup for moved survivors, `is_addr_live` for
        // not-moved ones) — the same pattern `gc_reconcile_defining_loaders`
        // above already relies on — so pruning here with pre-remap addresses
        // is correct; the later `gc_update_collection_overlay_refs` remap
        // pass in `update_all_roots` only touches whatever prune left behind.
        //
        // MEASURED FALSE-DEAD (2026-08-01): the paragraph above is wrong about
        // which addresses reach here. `run_non_moving_young_cycle` calls
        // `remap_external_roots` INSIDE the collector, so by this point
        // `slot.last_ptr` is already POST-GC — and `is_marked`'s first arm
        // (`pointer_map.contains_key`) only ever matches a PRE-GC address, so
        // for anything that moved the whole verdict falls to `is_addr_live`.
        // `ROverlaySystemGcStress` shows that verdict killing 10 LIVE
        // collections in one cycle (`dead_keys=10`, immediately followed by a
        // populated `TreeMap` reading back as `size 0`). A false-dead here is
        // silent data loss with no dangling pointer for any verifier to find;
        // a false-live is one cycle of retained bookkeeping. The two are not
        // symmetric and this predicate treats them as if they were.
        //
        // `CRATONVM_DBG_OVERLAY_PRUNE=1` reports every address this is about
        // to condemn, with the evidence, so the arm responsible is a fact and
        // not another inference.
        let prune_dbg =
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_OVERLAY_PRUNE").is_some();
        // Only THIS VM's owners may be condemned. The overlay registry is
        // process-wide, and `is_marked` answers `false` for every address
        // outside this heap, so without a screen each VM's collection deleted
        // every other VM's live overlay-backed collections.
        //
        // The screen is by VM (gc-common w12-c): `prune_external_roots_for_vm`
        // hands the native-collection provider this VM's identity, and it asks
        // `is_live` only about the object-key slots this VM created. That
        // covers every backend, Generational included
        // (`common-w10g-generational-overlay-prune-has-no-ownership-screen`).
        //
        // The span screen (gc-common w8-d) stays for a provider that has no
        // VM-scoped callbacks. On G1 and ZGC `conservative_addr_span` is the
        // one arena, fixed for the heap's lifetime, so it is exact. NOT on
        // Generational: its span is the envelope of arenas a young `grow` can
        // move during this very collection, so a dead owner's pre-GC address
        // could fall outside it and never be pruned (a leak, and its elements
        // stay rooted).
        let owned_here = if shared.mem.heap.is_generational() {
            None
        } else {
            shared.mem.heap.conservative_addr_span()
        };
        // Both screens are applied by `prune_external_roots_for_vm`: another
        // VM's slot, or an owner outside `owned_here`, is never handed to this
        // predicate.
        let is_live_for_prune = |addr: usize| -> bool {
            let live = is_marked(addr);
            if prune_dbg && !live {
                let in_heap = shared.mem.heap.is_heap_addr(addr).is_some();
                let words: [u64; 3] = if in_heap {
                    // SAFETY: `is_heap_addr` placed `addr` in a mapped region.
                    unsafe { std::ptr::read_unaligned(addr as *const [u64; 3]) }
                } else {
                    [0; 3]
                };
                let (old_alloc, young_surv, region) = shared.mem.heap.liveness_arms(addr);
                eprintln!(
                    "[overlay-prune] CONDEMNED 0x{addr:x} in_heap={in_heap} region={region} \
                     in_pointer_map={} old_gen_allocated={old_alloc} young_survivor={young_surv} \
                     w0=0x{:016x} w1=0x{:016x} w2=0x{:016x}",
                    pointer_map.contains_key(&addr),
                    words[0],
                    words[1],
                    words[2],
                );
            }
            live
        };
        if prune_dbg {
            // `collection_count()` is minor_gc_count + major_gc_count
            // (`VmHeap::collection_count`), and a single STW cycle that also
            // promotes (`major_ran` in `gen_heap.rs`) bumps BOTH counters
            // before returning to this one call site — so a jump of 2 here
            // between successive prunes is one physical collection, not a
            // skipped one. See `WORKER-5-NOTE-10` N2: this line is the
            // assertion that nomination asked for.
            eprintln!(
                "[overlay-prune] collection_count={} (pre-prune)",
                shared.mem.heap.collection_count()
            );
        }
        cratonvm_gc::external_roots::prune_external_roots_for_vm(
            shared.vm_identity,
            owned_here,
            &is_live_for_prune,
        );
        // Opt-in audit of what prune left behind: an overlay ref pointing at a
        // reclaimed object. Here rather than in `update_all_roots` because that
        // early-returns on an empty `pointer_map`, i.e. never runs on the
        // non-moving sweep — the path a `System.gc()` takes.
        crate::memory::gc::audit_overlay_refs(shared);
        // Companion reconciliation for the class-mirror cache — see
        // `memory::gc::reconcile_class_mirrors` / `roots.rs` step 6. Same
        // "before the no_refproc short-circuit" rationale: the cache must
        // never hold a stale ObjectRef after a collection, independent of
        // that diagnostic switch.
        crate::memory::gc::reconcile_class_mirrors(shared, &is_marked, Some(cycle_roots));
        // Rebuild the mirror_pin registry the GC marker consults (gen_heap.rs)
        // from the now-pruned class_mirrors + just-remapped defining-loader
        // side-table, so the marker sees current addresses next cycle.
        crate::memory::gc::rebuild_mirror_pins(shared, pointer_map);
    }

    // bc math-ec 0x4 (CRATONVM_DBG_NO_REFPROC): subsystem-level exclusion
    // switch — skip ALL post-GC reference processing (clears, enqueues,
    // finalizer/cleaner submissions). If the corruption persists with this
    // set, the whole reference subsystem is exonerated in one experiment;
    // if it stops, the writer is in here. Diagnostic only (weak refs never
    // clear; memory grows).
    if no_refproc() {
        return;
    }
    // Pressure input to the SoftReference LRU policy (HotSpot's
    // `LRUMaxHeapPolicy`: clear when `idle_ms > SoftRefLRUPolicyMSPerMB *
    // free_heap_mb`). This used to be a hardcoded `64`, which pinned the
    // threshold at a constant 64 seconds of idleness no matter how full the
    // heap was, so the policy could not respond to memory pressure at all.
    // `soft_ref_policy_free_mb()` returns whole megabytes of *allocatable*
    // headroom (min of young/eden and old-gen promotion room, capped by
    // whole-heap headroom, rounded down so a sub-MB remainder reads as 0 =
    // maximum pressure) — the right figure under the non-moving fragmenting
    // collector that actually runs by default, where unused bytes and
    // obtainable bytes diverge. Read *before* taking the reference-processor
    // lock: the accessor reaches into the heap's own generation stats, and
    // there is no reason to nest those acquisitions.
    // See `refs-metaspace-unloading.md` §2/§R1.
    let free_mb = shared.mem.heap.soft_ref_policy_free_mb();
    // Read before the reference-processor lock. See the call site below for
    // what it gates and why it now covers every backend (it used to ask the
    // heap `is_generational()`).
    let refproc_prepass = !no_refproc_prepass();
    // ClassManager is rank L10 and the reference processor is L7, so resolve
    // the JDK field before acquiring the lower-ranked processor lock.
    let reference_next_slot = gc_reference_next_slot(shared);
    // gc-common w2-f: what the enqueue loop stores in a real-layout
    // `Reference.queue`. See `gc_enqueued_sentinel`.
    let enqueued_sentinel = gc_enqueued_sentinel(shared, pointer_map);
    // Same rank rule, one step further: the shape guard below asks the class
    // hierarchy a question per entry, so its read guard is taken HERE — L10
    // before L7 — and held across the two loops rather than reacquired inside
    // them. Nothing between this line and the `drop` after the enqueue loop
    // touches the ClassManager for writing; the mirror/loader reconciliation
    // that does is all above, before this point.
    let class_manager = shared.classes.class_manager.read();
    let reference_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference");
    let queue_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/ReferenceQueue");
    let mut ref_proc = shared.mem.ref_processor.lock();

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
    let is_marked = |addr: usize| -> bool {
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
    // prunes all five lists, and `finalizer_refs`/`cleaner_refs` hold rows the
    // VM registered as `reference_obj == referent == the finalizable object`
    // (`SharedVm::register_finalizable`; `java/lang/ref/Finalizer.register`
    // through the `Cleaner` wire type). For those rows "the reference object
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
    if refproc_prepass {
        ref_proc.remove_collected_reference_objects(&is_marked);
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
    let result = ref_proc.process_references(&is_marked, free_mb, 0);

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
            eprintln!(
                "[refaudit] cleared-null addr=0x{actual_addr:x} class={} fields={}",
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
    // gc-common w5-d: also under `--jdk-only` without the flag, per VM — see
    // `queue_wake_ups_recorded_for`.
    let record_enqueued_queues = queue_wake_ups_recorded(shared);
    let mut gc_enqueued_queues: Vec<usize> = Vec::new();

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
        // Push onto queue's linked list head (field 0 = head, field 1 = size).
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
        // processor row that was never enqueued before, and `enqueue`'s
        // address-keyed `already_finalized` gate refused a NEW object that had
        // been allocated where a finalized one used to live. See that method.
        shared.mem.finalizer_thread.enqueue_unfinalized(actual);
    }

    // Relocate any cleaner actions DEFERRED from earlier GC cycles (queued but
    // not yet run because a JIT borrow was live — see `run_cleaner_actions`).
    // Their cleanable objects may have been evacuated by this collection, so
    // remap their raw addresses before any later drain dereferences them.
    shared.mem.cleaner_thread.update_after_gc(pointer_map);
    // gc-common w1-d: forget the finalized objects that died this cycle, so
    // `FinalizerThread::already_finalized` is bounded by LIVE finalized
    // objects instead of growing by one address per object ever finalized
    // (`cleanup_collected` had no production caller). Pre-collection
    // predicate, so it runs BEFORE the relocation below rewrites the set.
    shared.mem.finalizer_thread.cleanup_collected(&is_marked);
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
    if weakref_clear_enabled() {
        // REFPROC AUDIT (see `refaudit_enabled`). Every `continue` below leaves
        // the slot the PRE-GC pass nulled still null, in a Reference that is
        // live — the comment on the referent screen says so in as many words
        // ("a refusal leaves the slot NULL"). Two of the five decline paths
        // print nothing at all, so the straystack census cannot distinguish
        // "restored everything" from "abandoned half of them". Count each one.
        let audit = refaudit_enabled();
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
        let relocation_targets: rustc_hash::FxHashSet<usize> = {
            let candidates: rustc_hash::FxHashSet<usize> = if screen_on && !pointer_map.is_empty() {
                active
                    .iter()
                    .filter(|&&(r, t)| {
                        ref_proc.was_nulled_by_pre_gc_pass(r) && !pointer_map.contains_key(&t)
                    })
                    .map(|&(_, t)| t)
                    .collect()
            } else {
                rustc_hash::FxHashSet::default()
            };
            if candidates.is_empty() {
                rustc_hash::FxHashSet::default()
            } else {
                pointer_map
                    .values()
                    .copied()
                    .filter(|v| candidates.contains(v))
                    .collect()
            }
        };
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
                    if ref_proc.clear_after_refused_restore(ref_obj_old) {
                        r_converted += 1;
                    }
                    continue;
                }
            };
            // SAFETY: both addresses are live post-collection object headers.
            let ro = unsafe { ObjectRef::from_raw(ref_obj_new as *mut u8) };
            let rt = unsafe { ObjectRef::from_raw(referent_new as *mut u8) };
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
            if screen_on && !referent_moved && relocation_targets.contains(&referent_old) {
                note_referent_restore_refused();
                if straystack_enabled() || dbg_weakref() {
                    eprintln!(
                        "[refproc] REFUSE weak/phantom RESTORE ref @0x{ref_obj_new:x}: \
                         referent @0x{referent_old:x} is a relocation TARGET this cycle \
                         (total refused={})",
                        referent_restore_refusals()
                    );
                }
                r_screen += 1;
                if ref_proc.clear_after_refused_restore(ref_obj_old) {
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
                    note_referent_restore_refused();
                    if straystack_enabled() || dbg_weakref() {
                        eprintln!(
                            "[refproc] REFUSE weak/phantom RESTORE ref @0x{ref_obj_new:x}: \
                             referent @0x{referent_new:x} is class {have_class}, recorded \
                             {want_class} (total refused={})",
                            referent_restore_refusals()
                        );
                    }
                    r_screen += 1;
                    if ref_proc.clear_after_refused_restore(ref_obj_old) {
                        r_converted += 1;
                    }
                    continue;
                }
            }
            // Slot 0 = REF_FIELD_REFERENT. `set_field` fires the write barrier,
            // so a young referent restored into a promoted (old-gen) Reference
            // re-marks the old→young card.
            r_ok += 1;
            shared.mem.heap.set_field(ro, 0, Value::Object(Some(rt)));
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
        ref_proc.remove_collected(&is_marked);
        // HIB-WEAKREF-RECYCLE.1 (2026-07-31): `is_marked` cannot see old-gen
        // reuse — `is_old_gen_addr` is a pure range check, so a weak/phantom
        // entry whose Reference object was reclaimed by an old-gen sweep
        // survives `remove_collected` forever and both referent passes keep
        // targeting recycled memory every cycle. Follow up with the shape test:
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
        ref_proc.retain_shaped_weak_phantom(&still_a_reference);
    }
    // Every shape guard above is done; release the L10 read guard.
    drop(class_manager);

    // gc-common w1-d: this collection's pre/post protocol is complete. Forget
    // which slots the pre-collection pass nulled, which rows it examined and
    // which soft rows it condemned, so nothing that runs WITHOUT a pass of its
    // own — G1's final remark — reads them as current. Before the relocation
    // below so it does not rebuild a set that is about to be emptied.
    ref_proc.finish_pre_gc_cycle();

    // Relocate all addresses in the ref processor to match the new heap layout
    ref_proc.update_after_gc(pointer_map);
    drop(ref_proc);

    // gc-common w3-d: hand this collection's queue wake-ups to the delivery
    // thread and wake it (flag-gated; a no-op otherwise). AFTER
    // `cleaner_thread.update_after_gc` above: these are already post-collection
    // addresses and must not be relocated a second time. Outside the processor
    // lock, which nothing below needs.
    gc_enqueued_queues_hand_off(shared, &gc_enqueued_queues);
}

/// Hand a reference pass's queue wake-ups to the delivery thread and wake it.
/// With no delivery thread to take them (flag off, or it could not be
/// started) the list is dropped at once rather than kept rooted forever —
/// nothing would ever drain it. gc-common w3-d.
///
/// gc-common w5-d: under the default policy a `--jdk-only` VM records
/// wake-ups too (`queue_wake_ups_recorded_for`), and a non-empty list then
/// requests a batch — starting the delivery thread on first use, from inside
/// the pause, exactly as the full hand-off's `wake_finalizer_thread` does. That
/// batch notifies the queues (`drain_finalizer_thread_batch`) and otherwise
/// runs only what the default policy already gives the thread; cleaner actions
/// stay on their inline doors. An empty list requests nothing, so a strict VM
/// whose collections enqueue nothing never starts a thread for it.
fn gc_enqueued_queues_hand_off(shared: &SharedVm, gc_enqueued_queues: &[usize]) {
    shared.mem.cleaner_thread.note_queues_to_notify(gc_enqueued_queues);
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
static REFERENT_RESTORE_REFUSALS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn note_referent_restore_refused() {
    REFERENT_RESTORE_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many restores have been refused so far. Printed beside each refusal and
/// available to a test.
pub(crate) fn referent_restore_refusals() -> u64 {
    REFERENT_RESTORE_REFUSALS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Try to allocate an object, running GC and retrying on failure.
/// Returns the ObjectRef or a RuntimeError::OutOfMemoryError.
///
/// Fast path: TLAB bump-pointer (no lock).
/// Medium path: refill TLAB from shared arena (one lock acquisition).
/// Slow path: GC + retry.
pub(super) fn gc_alloc_object(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};

    // ONE shape decision for this allocation, used both to reserve the region
    // and to stamp the header. See `plan_tlab_object_shape`.
    let (total_size, _body_size, _gc_flags) =
        plan_tlab_object_shape_at(class_id, num_fields, tlab_site::INTERPRETER);
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
fn class_alloc_recipe(
    shared: &SharedVm,
    class_id: ClassId,
) -> Option<&crate::jit::alloc_class_cache::ClassAllocInfo> {
    use crate::jit::alloc_class_cache::{alloc_class_cache_enabled, ClassAllocInfo};
    if !alloc_class_cache_enabled() {
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
/// so they say nothing about the real class. No `--jdk-only` class is a stub.
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
/// MEASURED, `target-rel4` (`cb2ade4fd`), `--jdk-only`,
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
/// `RJdkHello --jdk-only` loads): instance fields split 361 primitive /
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
fn write_default_instance_fields(
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
#[inline]
pub(crate) fn init_new_instance(shared: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    if init_primitive_fields(shared, obj, class_id) {
        shared.register_finalizable(obj.as_ptr() as usize); // Cast: GC object pointer to address
    }
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
pub(super) fn dbg_invoke_stats_record(index: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON
        .get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INVOKESTATS").is_some())
    {
        return;
    }
    static COUNTS: [AtomicU64; 4] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    let n = COUNTS[index].fetch_add(1, Ordering::Relaxed) + 1;
    if n % 100000 == 1 {
        eprintln!(
            "[invokestats] cache_hit={} cache_miss={} vtable_fast={} slow_path={}",
            COUNTS[0].load(Ordering::Relaxed),
            COUNTS[1].load(Ordering::Relaxed),
            COUNTS[2].load(Ordering::Relaxed),
            COUNTS[3].load(Ordering::Relaxed),
        );
    }
}

/// DBG (CRATONVM_DBG_TLABMISS): gate-failure state dump — the live young
/// arena facts at the moment the guarded-refill young-room gate said no.
/// Sampled every 2^20 failures (plus the first).
pub(super) fn dbg_refill_fail_state(shared: &SharedVm, requested: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_TLABMISS").is_some())
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
pub(super) fn dbg_refill_fail(stage: usize, requested: usize) {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_TLABMISS").is_some())
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
const NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES: u64 = 4 * 1024 * 1024;

/// Publish the current thread's bounded TLAB-accounting batch. This is called
/// at a refill, GC, or wedge decision so the shared byte total is exact when a
/// policy consumes it, while ordinary bump allocations only mutate fields
/// owned by their JvmThread.
#[inline(always)]
fn flush_tlab_allocation_batch(thread: &mut JvmThread, shared: &SharedVm) {
    let _ = shared;
    thread.tlab.flush_vm_allocation_batch();
}

/// Charge a successful TLAB allocation without touching a shared cache line
/// until the per-thread accumulator reaches its bounded batch size.
#[inline(always)]
fn note_tlab_allocation(thread: &mut JvmThread, shared: &SharedVm, bytes: usize) {
    let _ = shared;
    let _ = thread.tlab.note_vm_tlab_allocation(bytes);
}

/// Charge an allocation that bypassed the TLAB to the thread's own total
/// (`getThreadAllocatedBytes`) and, in batches, to this VM's
/// `thread_allocated_total` (`getTotalThreadAllocatedBytes`). Attaches the
/// thread's buffer to that total first, so a thread whose allocations all
/// miss the TLAB is still counted by its own VM (gc-common w6-c).
#[inline]
fn note_thread_external_allocation(thread: &mut JvmThread, shared: &SharedVm, bytes: usize) {
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
fn tlab_gc_trigger_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_TLAB_GC_TRIGGER")
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
fn tlab_gate_bump_probe(
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
pub(super) fn tlab_refill_wedge_break(thread: &mut JvmThread, shared: &SharedVm) -> bool {
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
    maybe_gc_forced_at(shared, thread, "tlab-refill-wedge");
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
            cratonvm_types::flags::runtime_var("CRATONVM_COMPACT_TLAB_ALLOC").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// TLAB objects given the compact body shape, and those left legacy.
///
/// A count needs its complement to be readable: "compact=0" means either that
/// the switch is off or that no allocated class has a registered layout, and
/// those are different facts.
static TLAB_COMPACT_OBJECTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TLAB_LEGACY_OBJECTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bytes the compact shape saved against what the legacy shape would have
/// taken for the same allocations. The point of the change, in the only unit
/// that matters.
static TLAB_COMPACT_BYTES_SAVED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

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
fn tlab_shape_stats_enabled() -> bool {
    cratonvm_types::flags().jit.method_stats
}

/// `(compact, legacy, bytes_saved)` for TLAB object allocations.
///
/// All zero unless `CRATONVM_DBG_JIT_METHOD_STATS` is on
/// ([`tlab_shape_stats_enabled`]); the only reader prints under that switch.
pub fn tlab_object_shape_counts() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering;
    (
        TLAB_COMPACT_OBJECTS.load(Ordering::Relaxed),
        TLAB_LEGACY_OBJECTS.load(Ordering::Relaxed),
        TLAB_COMPACT_BYTES_SAVED.load(Ordering::Relaxed),
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
        || match cratonvm_types::flags::runtime_var("CRATONVM_COMPACT_TLAB_SITES") {
            Ok(v) => v.trim().parse::<u32>().unwrap_or(u32::MAX),
            Err(_) => u32::MAX,
        },
    )
}

/// The TLAB object allocation sites, as mask bits for
/// [`compact_tlab_site_mask`].
pub(crate) mod tlab_site {
    /// `gc_alloc_object` — the interpreter's own `new`.
    pub const INTERPRETER: u32 = 1;
    /// `jit_new_object`'s guarded-refill TLAB attempt.
    pub const JIT_NEW: u32 = 2;
    /// The two `tlab_alloc_object` sites inside the JIT helpers.
    pub const JIT_HELPER: u32 = 4;
    /// The native-call allocation site in `vm_exec`.
    pub const NATIVE: u32 = 8;
    /// The compact-`String` site in `vm_object`.
    pub const STRING: u32 = 16;
}

/// Whether `CRATONVM_DBG_COMPACT_TLAB` is set, read ONCE.
///
/// It used to be a `runtime_var_os` per compact allocation — 99.6% of every
/// flag read in the process on bintrees and ~8% of wall time in the read
/// machinery (hash probe, UTF-8 check, census). `dbg_jit_alloc_filter` in
/// `jit/helpers.rs` records the identical defect once before.
#[inline]
fn dbg_compact_tlab_enabled() -> bool {
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
struct TlabPlannerGates {
    /// Sites allowed to plan the compact shape; 0 when the shape is off.
    site_mask: u32,
    /// `CRATONVM_DBG_COMPACT_TLAB`.
    dbg_compact_tlab: bool,
}

#[inline]
fn tlab_planner_gates() -> TlabPlannerGates {
    static G: std::sync::OnceLock<TlabPlannerGates> = std::sync::OnceLock::new();
    *G.get_or_init(|| TlabPlannerGates {
        site_mask: if compact_tlab_alloc_enabled() {
            compact_tlab_site_mask()
        } else {
            0
        },
        dbg_compact_tlab: cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_COMPACT_TLAB")
            .is_some(),
    })
}

/// One line per distinct class that gets the compact shape —
/// `CRATONVM_DBG_COMPACT_TLAB=1`.
#[cold]
fn note_compact_tlab_class(class_id: ClassId, num_fields: usize, site: u32) {
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
                if tlab_shape_stats_enabled() {
                    TLAB_COMPACT_OBJECTS.fetch_add(1, Ordering::Relaxed);
                    TLAB_COMPACT_BYTES_SAVED
                        .fetch_add(legacy_total.saturating_sub(total) as u64, Ordering::Relaxed);
                }
                if gates.dbg_compact_tlab {
                    note_compact_tlab_class(class_id, num_fields, site);
                }
                return (total, body_u32, cratonvm_types::GC_FLAG_COMPACT);
            }
        }
    }
    if tlab_shape_stats_enabled() {
        TLAB_LEGACY_OBJECTS.fetch_add(1, Ordering::Relaxed);
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
fn shape_of_reserved(num_fields: usize, total_size: usize) -> (u32, u8) {
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
const COMPACT_BODY_DEFAULT_SKIP_ENABLED: bool = true;

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
fn compact_body_holds_defaults(num_fields: usize, total_size: usize) -> bool {
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
pub(super) enum TlabShape {
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
    unsafe fn init_header(self, ptr: *mut u8, class_id: ClassId) {
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
pub(super) fn tlab_alloc_object_inner(
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
pub(super) fn tlab_alloc_shaped_inner(
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
    // `docs/internal/gc/gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md`.
    if refill_needs_young_room
        && thread.tlab.keep_may_apply()
        && shared.mem.heap.young_bump_headroom(total_size)
        && thread.tlab.keep_on_miss(total_size)
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
    if refill_needs_young_room && tlab_gc_trigger_enabled() {
        use std::sync::atomic::Ordering;
        const NEEDSGC_MIN_ENTRIES_BETWEEN_FIRES: u64 = 65_536;
        let wedge = &shared.mem.tlab_wedge;
        let entries = wedge
            .slowpath_entries_since_gc
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        let refilled = wedge.refill_bytes_since_gc.load(Ordering::Relaxed);
        if (entries >= NEEDSGC_MIN_ENTRIES_BETWEEN_FIRES
            || refilled >= NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES)
            && shared.mem.heap.needs_gc_for_jit_allocation()
        {
            wedge.slowpath_entries_since_gc.store(0, Ordering::Relaxed);
            wedge.refill_bytes_since_gc.store(0, Ordering::Relaxed);
            flush_tlab_allocation_batch(thread, shared);
            thread.tlab.retire();
            maybe_gc_forced_at(shared, thread, "tlab-alloc-shaped");
        } else if refilled >= NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES {
            // Consulted and declined: re-arm the bytes stamp so the next
            // consult is another 4 MiB away rather than on every refill.
            wedge.refill_bytes_since_gc.store(0, Ordering::Relaxed);
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
    // superseded: the share sizer below accounts an early retire like a drain.
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
    // value wins; both defaults are OFF since the Generational flip was
    // reverted at the r5w4 merge), so it is asked of THIS VM's heap, once per
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
    let requested = if share_sizer_on {
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
            .share_refill_request(epoch)
            .max(cratonvm_gc::tlab::min_tlab_size())
            .max(object_floor)
    } else if thread.tlab.is_empty() {
        // First allocation on this thread — no history yet. Use the
        // "start big" baseline so a static-init burst doesn't refill
        // three times before the sizer gets a chance to weigh in.
        cratonvm_gc::tlab::initial_refill_size().max(object_floor)
    } else {
        // Subsequent refill — consult the thread-local pressure
        // tracker attached to the just-retired TLAB.
        let n = thread.tlab.next_refill_size();
        // Never fall below the documented floor even if the tracker
        // somehow returns zero (pathological input).
        n.max(cratonvm_gc::tlab::min_tlab_size()).max(object_floor)
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
    if refill_needs_young_room
        && !shared.mem.heap.young_bump_headroom(bump_probe)
        && !shared.mem.heap.young_has_free_block(free_block_floor)
    {
        dbg_refill_fail_state(shared, requested);
        // Sustained gate failure = the wedge: per-object allocations keep
        // succeeding so nothing else will ever trigger the collection that
        // coalesces the free list. Force one (rate-limited) and re-probe.
        if !tlab_gc_trigger_enabled()
            || !tlab_refill_wedge_break(thread, shared)
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
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();

    // gcd d1/d: at least the object in hand, or no carve at all
    // (`gengc-r5w5-sizer9-refill-can-carve-less-than-the-object`, clamp 2).
    // `requested >= object_floor` already, so this only changes the refills
    // young could not have served the object from anyway: they used to carve
    // a smaller buffer, install it, fail the object in it and return `None`;
    // now they return `None` without the carve. Generational only (G1/ZGC
    // forward to their plain `refill_tlab`).
    let mut refill = shared.mem.heap.refill_tlab_at_least(requested, object_floor);
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
        if refill_needs_young_room
            && tlab_gc_trigger_enabled()
            && tlab_refill_wedge_break(thread, shared)
        {
            refill = shared.mem.heap.refill_tlab_at_least(requested, object_floor);
            cratonvm_types::gc_entry_census::note_refill_retry(refill.is_some());
        }
    } else {
        shared
            .mem
            .tlab_wedge
            .gate_consecutive_fails
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some((buf, size)) = refill {
        shared.mem.tlab_refill_count.fetch_add(1, Ordering::Relaxed);
        // Widening: usize -> u64 (value preserved). The healthy-path re-arm
        // metric for the refill-time young trigger above.
        shared
            .mem
            .tlab_wedge
            .refill_bytes_since_gc
            .fetch_add(size as u64, Ordering::Relaxed);
        // Read the outgoing TLAB's running per-thread allocation total before
        // the struct is replaced — `Tlab::new` starts a fresh one at zero, and
        // `getThreadAllocatedBytes` must not go backwards at a refill.
        let carried = thread.tlab.thread_allocated_bytes();
        // gen r4w4/alloc4: the share sizer's history rides the same carry.
        let share = thread.tlab.share_sizer();
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
        // gen r5w4/defaults8: `Tlab::new` latched the heap-less reading of
        // the share sizer's switch; give the buffer this heap's answer, the
        // one that sized `requested`, so its refill-waste limit agrees.
        thread.tlab.arm_share_sizer(share_sizer_on);
        // Counted only now that the refill was granted (a no-op unless the
        // share sizer sized this request) — see `Tlab::note_share_refill`.
        thread.tlab.note_share_refill(size);
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
        // `docs/internal/gc/gengc-r4w5-thrash5-blocking-retire-buries-tlab-tail-FIXED-20260927.md`.
        if let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap {
            if h.tlab_tail_sink_enabled() {
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
static TLAB_LEGACY_CLASSES: [(
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
fn note_tlab_legacy_object(class_id: ClassId, num_fields: usize) {
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
pub(super) fn init_object_header(
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
fn shared_object_footprint(shared: &SharedVm, obj: ObjectRef, num_fields: usize) -> usize {
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
pub(crate) fn maybe_dump_heap_on_oom(shared: &SharedVm, thread: &JvmThread) {
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
pub(crate) fn maybe_dump_heap_on_oom_for(shared: &SharedVm, thread: &JvmThread, message: &str) {
    use std::io::Write as _;
    use std::sync::atomic::Ordering;
    // gcd d4/j: every heap-OOME door passes here; owe the native funnel one
    // collection (`native_call_owes_oome_major`). Before the early return:
    // it is not a reporting feature.
    note_heap_oome_raised(shared, thread, message);
    let config = &shared.config;
    let on_oom_commands = config
        .on_out_of_memory_error
        .as_deref()
        .filter(|raw| !raw.trim().is_empty());
    if !(config.heap_dump_on_oom
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
    if config.heap_dump_on_oom {
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
            let _ = writeln!(out, "Terminating due to java.lang.OutOfMemoryError: {message}");
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
fn exit_without_cleanup(code: i32) -> ! {
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
fn write_heap_dump_on_oom(shared: &SharedVm, thread: &JvmThread, message: &str) {
    use std::io::Write as _;
    let path = heap_dump_path_for(shared.config.heap_dump_path.as_deref(), std::process::id());
    let arc = shared.get_arc();
    // Both lines before the walk, as HotSpot prints them, and flushed: a
    // dump of a large heap takes seconds, and the first line is how a
    // watcher learns the process has not hung.
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "java.lang.OutOfMemoryError: {message}");
        let _ = writeln!(out, "Dumping heap to {path} ...");
        let _ = out.flush();
    }
    let started = std::time::Instant::now();
    // obsaudit D11: pass this thread's id so dump_heap can request a real
    // stop-the-world pause for the walk instead of racing live mutators.
    match crate::runtime::hprof::dump_heap(&arc, &path, thread.thread_id) {
        Ok(bytes) => {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(
                out,
                "Heap dump file created [{bytes} bytes in {:.3} secs]",
                started.elapsed().as_secs_f64()
            );
            let _ = out.flush();
            tracing::error!(
                "wrote {} byte HPROF heap dump to {} on OutOfMemoryError",
                bytes,
                path
            )
        }
        Err(e) => {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "Unable to create {path}: {e}");
            let _ = out.flush();
            tracing::error!("failed to write HPROF heap dump on OutOfMemoryError: {}", e)
        }
    }
}

/// Where `-XX:+HeapDumpOnOutOfMemoryError` writes, by HotSpot's rule: the
/// configured `-XX:HeapDumpPath` if it names a file, `java_pid<pid>.hprof`
/// inside it if it names an existing directory, and `java_pid<pid>.hprof` in
/// the working directory when unset. Split out so the rule is testable.
fn heap_dump_path_for(configured: Option<&str>, pid: u32) -> String {
    let file = format!("java_pid{pid}.hprof");
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
fn expand_on_error_pid(cmd: &str, pid: u32) -> String {
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
fn on_out_of_memory_error_commands(raw: &str, pid: u32) -> Vec<String> {
    raw.split(';')
        .map(|cmd| cmd.trim_start_matches(' '))
        .filter(|cmd| !cmd.is_empty())
        .map(|cmd| expand_on_error_pid(cmd, pid))
        .collect()
}

/// Step 2 of [`maybe_dump_heap_on_oom_for`], `VM_ReportJavaOutOfMemory::doit`'s
/// shape: a `#` header naming the error and the option, then each command run
/// to completion, with HotSpot's `#   Executing ...` line before it (Linux
/// names the `/bin/sh -c` it uses; the other platforms print the bare command,
/// as HotSpot does). The commands inherit the VM's standard streams.
/// A command that cannot be started is reported and skipped, never fatal.
fn run_on_out_of_memory_error_commands(raw: &str, message: &str, pid: u32) {
    use std::io::Write as _;
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "#");
        let _ = writeln!(out, "# java.lang.OutOfMemoryError: {message}");
        let _ = writeln!(out, "# -XX:OnOutOfMemoryError=\"{raw}\"");
        let _ = out.flush();
    }
    for cmd in on_out_of_memory_error_commands(raw, pid) {
        {
            let mut out = std::io::stdout().lock();
            if cfg!(target_os = "linux") {
                let _ = writeln!(out, "#   Executing /bin/sh -c \"{cmd}\"...");
            } else {
                let _ = writeln!(out, "#   Executing \"{cmd}\"...");
            }
            let _ = out.flush();
        }
        if let Err(e) = on_error_shell_command(&cmd).status() {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "os::fork_and_exec failed: {e}");
            let _ = out.flush();
        }
    }
}

/// The process HotSpot's `os::fork_and_exec` starts for one `OnError`-style
/// command: `/bin/sh -c <cmd>` on Unix; `cmd /C <cmd>` on Windows, the command
/// passed verbatim (not re-quoted) with newlines turned into `&`, as HotSpot's
/// Windows `fork_and_exec` does.
fn on_error_shell_command(cmd: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").raw_arg(cmd.replace('\n', "&"));
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(cmd);
        c
    }
}

#[cfg(test)]
mod w5_oom_report_tests {
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use std::sync::atomic::Ordering;

    /// gen r4w5/thrash5: `Arguments::copy_expand_pid`.
    #[test]
    fn on_error_commands_expand_the_pid_like_hotspot() {
        assert_eq!(expand_on_error_pid("echo oom-hook %p", 4242), "echo oom-hook 4242");
        assert_eq!(expand_on_error_pid("kill -9 %p; %%p", 7), "kill -9 7; %p");
        assert_eq!(expand_on_error_pid("100%x", 7), "100%x", "an unknown escape is kept");
        assert_eq!(expand_on_error_pid("trailing %", 7), "trailing %");
    }

    /// gen r4w5/thrash5: `next_OnError_command`: `;` separates, blanks and
    /// empty commands are skipped.
    #[test]
    fn on_error_commands_split_on_semicolons() {
        assert_eq!(
            on_out_of_memory_error_commands("  echo a %p;; ;echo b", 9),
            vec!["echo a 9".to_string(), "echo b".to_string()]
        );
        assert!(on_out_of_memory_error_commands(" ; ;", 9).is_empty());
        assert_eq!(
            on_out_of_memory_error_commands("echo one\necho two", 9),
            vec!["echo one\necho two".to_string()],
            "a newline (two accumulated options) is left for the shell"
        );
    }

    /// gen r4w5/thrash5: with none of the four report options configured, the
    /// report is a no-op and does not even take the latch — the byte-for-byte
    /// guarantee for every default configuration, `--compatible` included.
    #[test]
    fn an_unconfigured_report_does_nothing_and_takes_no_latch() {
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let thread = JvmThread::new(ThreadId(0), "w5-oom-report");
        assert!(!shared.config.exit_on_out_of_memory_error);
        assert!(!shared.config.crash_on_out_of_memory_error);
        assert!(shared.config.on_out_of_memory_error.is_none());
        maybe_dump_heap_on_oom_for(&shared, &thread, "Java heap space");
        assert!(!shared.debug.oom_dump_written.load(Ordering::SeqCst));
    }

    /// gen r4w5/thrash5: the command runs once per VM. The first report takes
    /// the latch and runs it; the second finds the latch taken. The command
    /// is a harmless `exit 0` so the test needs no shell output.
    #[test]
    fn the_on_oom_command_runs_once_per_vm() {
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            on_out_of_memory_error: Some("exit 0".to_string()),
            ..VmConfig::default()
        });
        let thread = JvmThread::new(ThreadId(0), "w5-oom-once");
        maybe_dump_heap_on_oom_for(&shared, &thread, "Java heap space");
        assert!(shared.debug.oom_dump_written.load(Ordering::SeqCst), "first report latches");
        // A second report must return at the latch (nothing observable to
        // assert beyond not hanging or exiting; the latch stays set).
        maybe_dump_heap_on_oom_for(&shared, &thread, "Java heap space");
        assert!(shared.debug.oom_dump_written.load(Ordering::SeqCst));
        // A second VM in the same process has its own latch.
        let other = crate::vm::SharedVm::new(VmConfig::default());
        assert!(!other.debug.oom_dump_written.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod w4_oom_dump_path_tests {
    use super::heap_dump_path_for;

    /// gen r4w4/oom: HotSpot's `-XX:HeapDumpPath` rule.
    #[test]
    fn heap_dump_path_follows_hotspots_rule() {
        assert_eq!(heap_dump_path_for(None, 42), "java_pid42.hprof");
        assert_eq!(
            heap_dump_path_for(Some("no-such-dir-w4-oom/x.hprof"), 42),
            "no-such-dir-w4-oom/x.hprof",
            "a path that is not an existing directory is the file itself",
        );
        let dir = std::env::temp_dir();
        let got = heap_dump_path_for(dir.to_str(), 42);
        assert_eq!(
            std::path::PathBuf::from(got),
            dir.join("java_pid42.hprof"),
            "an existing directory gets java_pid<pid>.hprof inside it",
        );
    }
}

/// TLAB hit-only fast path for array allocation — the array twin of
/// [`tlab_alloc_object`], and the general-element-type twin of
/// [`tlab_alloc_byte_array`].
///
/// Why this exists: object allocation has had a TLAB fast path for a long
/// time, but *array* allocation never did — `gc_alloc_array` went straight to
/// `heap.try_alloc_array`, and every young allocation there takes the single
/// global `young_from` mutex and holds it across the bump, the `write_bytes`
/// zeroing of the whole object, and the header `init`. Because the zeroing is
/// inside the lock, the hold time grows with the allocation, so arrays
/// serialise harder than objects do.
///
/// Measured on this box (`apps/hib-suite-runner/AllocScaleProbe.java`,
/// aggregate throughput, 1 -> 4 threads):
///
/// | shape        | 1 thread | 4 threads | scaling |
/// |--------------|---------:|----------:|--------:|
/// | `new Object()` (TLAB) | 19.8 Mops/s | 41.4 Mops/s | 2.10x |
/// | `new long[16]` (no TLAB) | 13.1 Mops/s | 11.9 Mops/s | **0.91x** |
///
/// HotSpot scales the same array shape ~27x over the same range. Array
/// allocation was therefore capped at roughly one thread's worth of
/// throughput no matter how many cores were available — which is why a
/// 5-thread allocation-heavy workload (an ORM opening sessions and building
/// `ArrayList`/`HashMap`/`StringBuilder` backing arrays) could not use them.
///
/// It shares [`tlab_alloc_shaped_inner`] with the object path, so it inherits
/// that path's already-hardened refill gate, wedge breakers and
/// retire-before-replace protocol verbatim rather than carrying a second copy
/// of them. A hit-only first attempt is not enough on its own: a workload that
/// allocates mostly arrays drains the TLAB and then misses back to the global
/// lock on every subsequent allocation (measured — hit-only bought ~23% on one
/// thread and moved multi-thread scaling not at all).
///
/// Zeroing: [`GenerationalHeap::refill_tlab`] zeroes the whole TLAB region
/// when it is handed out, so the array body already reads back as Java's
/// mandated default values and the initializer only has to write the header —
/// the same contract [`tlab_alloc_byte_array`] relies on.
pub(super) fn tlab_alloc_array(
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
    // Keep well clear of the humongous threshold: anything at or above the
    // TLAB's per-allocation cap goes down the ordinary path, which owns the
    // young-vs-old-gen routing decision.
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
        // Same value the interpreter's object path passes: the young-room
        // pre-check is the JIT slow path's gate, not this one's.
        false,
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

/// Try to allocate an array, running GC and retrying on failure.
///
/// `try_alloc_array_full` is deliberately used rather than the young-only
/// `try_alloc_array`: it has the same young-first policy, but once a
/// non-moving JIT-safe sweep has left the young generation fragmented it can
/// spill the request into old space.  This matches `alloc_object_shared`'s
/// object path.  Retrying young-only here used to report OOM for a tiny array
/// while most of the heap was available as old-generation headroom.
pub(crate) fn gc_alloc_array(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    element_type: ArrayElementType,
    length: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(arr) = tlab_alloc_array(thread, shared, class_id, element_type, length) {
        return Ok(arr);
    }
    // HotSpot's size check comes BEFORE any collection. A length past the VM's
    // array limit is refused outright with its own message. It is never run
    // through the full GC ladder and then reported as "Java heap space". See
    // `array_length_exceeds_vm_limit`. The check sits after the TLAB attempt
    // so the fast path pays nothing; the TLAB already refuses any such length
    // through its own `tlab_max_alloc` cap.
    if array_length_exceeds_vm_limit(length) {
        maybe_dump_heap_on_oom_for(shared, thread, ARRAY_SIZE_EXCEEDS_VM_LIMIT);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError {
                message: ARRAY_SIZE_EXCEEDS_VM_LIMIT.to_string(),
            },
        )));
    }
    // Everything below this line bypasses the TLAB, so the thread's allocation
    // counter (`Tlab::thread_allocated_bytes`, read by
    // `com.sun.management.ThreadMXBean.getThreadAllocatedBytes`) sees none of
    // it from the cursor. Record it explicitly — arrays are precisely the
    // shape that skips the TLAB, so an unrecorded array path would make the
    // counter report a small fraction of a buffer-allocating workload.
    let external_bytes = external_array_bytes(element_type, length);
    if let Some(arr) = shared
        .mem
        .heap
        .try_alloc_array_full(class_id, element_type, length)
    {
        note_shared_array_allocation(thread, shared, external_bytes);
        return Ok(arr);
    }
    // Forced GC, the GC-overhead limit (bail to OOM if the heap is
    // GC-thrashing rather than spinning on slivers), retry, G1's last-ditch
    // complete mark cycle (the young pause cannot reclaim dead Old/humongous
    // spans — only a completed cycle's cleanup can), retry. See
    // `collect_and_retry`.
    let arr = collect_and_retry(shared, thread, "gc-alloc-array", |s| {
        s.mem
            .heap
            .try_alloc_array_full(class_id, element_type, length)
    })
    .ok_or_else(|| {
        maybe_dump_heap_on_oom(shared, thread);
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: format!("Java heap space (alloc_array length {})", length),
        }))
    })?;
    note_shared_array_allocation(thread, shared, external_bytes);
    Ok(arr)
}

/// Charge an array [`gc_alloc_array`] placed outside the TLAB to BOTH
/// counters [`alloc_object_shared`] charges for an object on the same path:
/// the thread's own total (`getThreadAllocatedBytes`) and the VM's
/// `bytes_allocated_total` (the `--verbose:gc` allocation rate and the wedge
/// breaker's re-arm progress).
///
/// gc-common w7-b: this path fed only the thread counter, so every array too
/// big for a TLAB (anything past `tlab_max_alloc`, 32 KiB: every large buffer,
/// every collection resize past a few thousand elements) was invisible to
/// `bytes_allocated_total`, whose doc promises "all TLAB and slow-path heap
/// allocations". A buffer-heavy workload therefore reported a fraction of its
/// allocation rate and re-armed the wedge breaker late. Direct `fetch_add`,
/// as `alloc_object_shared` does: this is the slow path and it already took
/// the backend's allocation lock.
#[inline]
fn note_shared_array_allocation(thread: &mut JvmThread, shared: &SharedVm, bytes: usize) {
    // Widening: usize -> u64 is loss-free on all supported targets.
    shared
        .mem
        .bytes_allocated_total
        .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    note_thread_external_allocation(thread, shared, bytes);
}

/// Footprint, in bytes, of an array that is about to be allocated outside the
/// TLAB — header plus payload, computed the same way [`tlab_alloc_array`]
/// computes `total_size`.
///
/// Saturating rather than checked: this feeds a monitoring counter, and an
/// array whose data size overflows `usize` is about to fail its allocation
/// anyway. Reporting the clamped figure keeps this off the error path.
#[inline]
fn external_array_bytes(element_type: ArrayElementType, length: usize) -> usize {
    use cratonvm_gc::heap::ARRAY_DATA_OFFSET;
    let data = cratonvm_gc::heap::array_data_size_checked(length, element_type).unwrap_or(0);
    ARRAY_DATA_OFFSET.saturating_add(data)
}

/// The largest array length HotSpot will attempt to allocate, for EVERY
/// element type. On 64-bit, `arrayOopDesc::max_array_length` is
/// `align_down(max_jint - header_words, MinObjAlignment)`. With a 16-byte
/// array header (two words, with compressed or compact class pointers alike)
/// and 8-byte object alignment, that is `Integer.MAX_VALUE - 2`.
///
/// So on a small heap `new byte[Integer.MAX_VALUE - 2]` fails with
/// `OutOfMemoryError: Java heap space`, and on any heap
/// `new byte[Integer.MAX_VALUE - 1]` fails with
/// `OutOfMemoryError: Requested array size exceeds VM limit`.
pub(crate) const MAX_JAVA_ARRAY_LENGTH: usize = cratonvm_gc::vm_heap::VM_MAX_ARRAY_LENGTH;

/// HotSpot's message for [`array_length_exceeds_vm_limit`].
pub(crate) const ARRAY_SIZE_EXCEEDS_VM_LIMIT: &str = "Requested array size exceeds VM limit";

/// Would HotSpot refuse an array of `length` elements as exceeding the VM
/// limit, rather than as heap exhaustion? See [`MAX_JAVA_ARRAY_LENGTH`].
///
/// gc-common w1-f: before this check, the interpreter ran such a request down
/// the whole collection ladder (a forced GC, the overhead-limit check, a G1
/// last-ditch concurrent cycle) and then reported
/// `Java heap space (alloc_array length 2147483647)`. `multianewarray` checks
/// it too since w2-d (`alloc_multi_array`), and so do the JIT's `newarray`
/// helpers and the native allocators (`vm_exec.rs` `new_array` /
/// `new_ref_array`) -- see
/// `docs/internal/gc-common-round-20260923/applied/handoff-w2d-array-vm-limit-jit-and-native.md`.
#[inline]
pub(crate) fn array_length_exceeds_vm_limit(length: usize) -> bool {
    length > MAX_JAVA_ARRAY_LENGTH
}

#[cfg(test)]
mod w1f_alloc_front_end_tests {
    //! gc-common round 2026-09-23, lane F: the VM allocation front end.

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::error::{MethodCallFailed, RuntimeError, VmError};
    use crate::memory::heap::ArrayElementType;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;
    use std::sync::atomic::Ordering;

    /// The HotSpot boundary, both sides of it.
    #[test]
    fn the_vm_array_limit_is_integer_max_value_minus_two() {
        let max = i32::MAX as usize;
        assert!(!array_length_exceeds_vm_limit(0));
        assert!(!array_length_exceeds_vm_limit(max - 8)); // ArrayList's own cap
        assert!(!array_length_exceeds_vm_limit(max - 2));
        assert!(array_length_exceeds_vm_limit(max - 1));
        assert!(array_length_exceeds_vm_limit(max));
        assert!(array_length_exceeds_vm_limit(usize::MAX));
    }

    /// `new byte[Integer.MAX_VALUE]` is refused with HotSpot's message and
    /// WITHOUT a collection: the request is invalid, not the heap full.
    #[test]
    fn an_array_past_the_vm_limit_is_refused_without_collecting() {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let mut thread = JvmThread::new(ThreadId(0), "w1f-limit");
        let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        let err = gc_alloc_array(
            &shared,
            &mut thread,
            ClassId::new(0),
            ArrayElementType::Byte,
            i32::MAX as usize,
        )
        .expect_err("an array past the VM limit must not be allocated");
        match err {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                message,
            })) => assert_eq!(message, ARRAY_SIZE_EXCEEDS_VM_LIMIT),
            other => panic!("expected OutOfMemoryError, got {other:?}"),
        }
        assert_eq!(
            shared.mem.gc_cycle_count.load(Ordering::Relaxed),
            gcs_before,
            "the VM-limit refusal must not run the collection ladder"
        );
    }
}

#[cfg(test)]
mod w2d_alloc_front_end_tests {
    //! gc-common round 2026-09-23, wave 2, lane D: the allocation front end's
    //! escalation ladder, slow-path accounting and multianewarray VM limit.

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::error::{MethodCallFailed, RuntimeError, VmError};
    use crate::memory::heap::ArrayElementType;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
    use cratonvm_types::ClassId;
    use std::sync::atomic::Ordering;

    fn gen_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    /// The shared ladder returns the first successful retry, after exactly
    /// the one forced collection -- no last-ditch cycle when it was not
    /// needed.
    #[test]
    fn collect_and_retry_returns_the_first_successful_retry() {
        let shared = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w2d-ladder-1");
        let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        let mut attempts = 0u32;
        let got = collect_and_retry(&shared, &mut thread, "w2d-test", |_| {
            attempts += 1;
            Some(7u32)
        });
        assert_eq!(got, Some(7));
        assert_eq!(attempts, 1, "a successful retry must not be retried again");
        assert!(
            shared.mem.gc_cycle_count.load(Ordering::Relaxed) > gcs_before,
            "the ladder must collect before its first retry"
        );
    }

    /// A failed first retry gets exactly one more attempt (after the
    /// last-ditch reclaim), and `None` comes back only when both fail.
    #[test]
    fn collect_and_retry_makes_exactly_two_attempts() {
        let shared = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w2d-ladder-2");
        let mut attempts = 0u32;
        let got = collect_and_retry(&shared, &mut thread, "w2d-test", |_| {
            attempts += 1;
            (attempts == 2).then_some(attempts)
        });
        assert_eq!(got, Some(2));

        let mut attempts = 0u32;
        let got: Option<u32> = collect_and_retry(&shared, &mut thread, "w2d-test", |_| {
            attempts += 1;
            None
        });
        assert_eq!(got, None);
        // gcd d4/j: on Generational with `CRATONVM_GC_OVERHEAD_PROGRESS` on,
        // a third attempt follows the second major this (sole) thread runs
        // after the last-ditch rung (`second_major_before_oome`).
        let expected = if cratonvm_types::flags().gc.gc_overhead_progress { 3 } else { 2 };
        assert_eq!(attempts, expected);
    }

    /// All four interpreter ladders go through the one helper, so they share
    /// one policy -- in particular the String ladders now stop at the
    /// GC-overhead limit like `new` and `newarray` do
    /// (`common-f-string-ladder-skips-overhead-limit`). A source witness,
    /// because driving the overhead streak to its limit needs a wedged old
    /// generation a unit test cannot build.
    #[test]
    fn every_interpreter_ladder_uses_the_shared_escalation() {
        let src = include_str!("gc_and_alloc.rs");
        for head in [
            "pub(crate) fn create_string_or_oom(",
            "pub(crate) fn create_string_from_units_or_oom(",
            "pub(crate) fn intern_string_literal_or_oom(",
            "pub(crate) fn alloc_object_shared(",
            "pub(crate) fn gc_alloc_array(",
        ] {
            let start = src.find(head).unwrap_or_else(|| panic!("{head} not found"));
            // `\u{7d}` is a closing brace, spelled so this line's braces
            // balance for the brace-counting production-panic scanner
            // (`interpreter::tests::scan_production_section`).
            let body_len = src[start..].find("\n\u{7d}\n").expect("function end");
            let body = &src[start..start + body_len];
            assert!(
                body.contains("collect_and_retry("),
                "{head} must escalate through `collect_and_retry`"
            );
            assert!(
                !body.contains("last_ditch_reclaim("),
                "{head} carries its own copy of the ladder again"
            );
        }
    }

    /// An object from a backend that did not plan a compact body (here: a
    /// class with no registered layout) is charged its legacy footprint --
    /// read back from its header, not assumed.
    #[test]
    fn a_legacy_object_is_charged_its_legacy_footprint() {
        let shared = gen_vm();
        let obj = shared
            .mem
            .heap
            .try_alloc_object(ClassId::new(0), 3)
            .expect("an empty heap has room for one object");
        assert_eq!(
            shared_object_footprint(&shared, obj, 3),
            HEADER_SIZE + 3 * SLOT_SIZE
        );
    }

    /// `alloc_object_shared` charges the thread counter (and the VM counter)
    /// what the header says the object occupies.
    /// (`common-f-shared-path-accounts-legacy-size`.)
    #[test]
    fn the_shared_object_path_charges_the_real_footprint() {
        let shared = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w2d-footprint");
        let thread_before = thread.tlab.thread_allocated_bytes();
        let vm_before = shared.mem.bytes_allocated_total.load(Ordering::Relaxed);
        let obj = alloc_object_shared(&shared, &mut thread, ClassId::new(0), 3)
            .expect("an empty heap has room for one object");
        let expected = cratonvm_gc::gc::object_total_size(shared.mem.heap.get_header(obj));
        assert_eq!(
            thread.tlab.thread_allocated_bytes() - thread_before,
            expected as u64
        );
        assert_eq!(
            shared.mem.bytes_allocated_total.load(Ordering::Relaxed) - vm_before,
            expected as u64
        );
    }

    /// `init_primitive_fields` reports the class's `has_finalizer` bit under
    /// its one class-manager read. A fresh VM's class 0 (unloaded, or
    /// `java/lang/Object`, whose empty `finalize()` is not a finalizer) must
    /// answer `false`, or every `new Object()` would be registered.
    #[test]
    fn class_zero_is_not_finalizable() {
        let shared = gen_vm();
        let obj = shared
            .mem
            .heap
            .try_alloc_object(ClassId::new(0), 0)
            .expect("an empty heap has room for one object");
        assert!(!init_primitive_fields(&shared, obj, ClassId::new(0)));
    }

    /// `multianewarray` refuses a dimension past HotSpot's array limit with
    /// HotSpot's message, before allocating it -- and, as HotSpot does, never
    /// examines an inner dimension under a zero-length outer one.
    #[test]
    fn multianewarray_refuses_a_dimension_past_the_vm_limit() {
        let shared = gen_vm();
        let sizes = [i32::MAX as usize, 1];
        let err = super::super::alloc_multi_array(
            &shared,
            &sizes,
            0,
            ArrayElementType::Int,
            sizes.len(),
            &[],
        )
        .expect_err("a dimension past the VM limit must be refused");
        match err {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                message,
            })) => assert_eq!(message, ARRAY_SIZE_EXCEEDS_VM_LIMIT),
            other => panic!("expected OutOfMemoryError, got {other:?}"),
        }

        let sizes = [0usize, i32::MAX as usize];
        let outer = super::super::alloc_multi_array(
            &shared,
            &sizes,
            0,
            ArrayElementType::Int,
            sizes.len(),
            &[],
        )
        .expect("an inner dimension under a zero-length outer one is never allocated");
        assert_eq!(shared.mem.heap.array_length(outer), 0);
    }
}

#[cfg(test)]
mod w3c_alloc_front_end_tests {
    //! gc-common round 2026-09-23, wave 3, lane C3: `multianewarray`'s
    //! collecting wrapper shares the one escalation ladder.

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::error::{MethodCallFailed, RuntimeError, VmError};
    use crate::memory::heap::ArrayElementType;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use std::sync::atomic::Ordering;

    /// `new int[Integer.MAX_VALUE][1]` is refused with HotSpot's message and
    /// WITHOUT a collection, exactly like `newarray` -- the collecting wrapper
    /// used to run a forced collection and a last-ditch cycle for it first,
    /// because the VM-limit refusal is also an `OutOfMemoryError`.
    #[test]
    fn multianewarray_vm_limit_is_refused_without_collecting() {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let mut thread = JvmThread::new(ThreadId(0), "w3c-multi-limit");
        let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        let sizes = [i32::MAX as usize, 1];
        let err = super::super::alloc_multi_array_collecting(
            &shared,
            &mut thread,
            &sizes,
            ArrayElementType::Int,
            sizes.len(),
            &[],
        )
        .expect_err("a dimension past the VM limit must be refused");
        match err {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                message,
            })) => assert_eq!(message, ARRAY_SIZE_EXCEEDS_VM_LIMIT),
            other => panic!("expected OutOfMemoryError, got {other:?}"),
        }
        assert_eq!(
            shared.mem.gc_cycle_count.load(Ordering::Relaxed),
            gcs_before,
            "the VM-limit refusal must not run the collection ladder"
        );
    }

    /// A tree that fits is built on the first attempt, with no collection.
    #[test]
    fn multianewarray_that_fits_does_not_collect() {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let mut thread = JvmThread::new(ThreadId(0), "w3c-multi-fits");
        let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        let sizes = [3usize, 4];
        let outer = super::super::alloc_multi_array_collecting(
            &shared,
            &mut thread,
            &sizes,
            ArrayElementType::Int,
            sizes.len(),
            &[],
        )
        .expect("a 3x4 int array fits an empty heap");
        assert_eq!(shared.mem.heap.array_length(outer), 3);
        assert_eq!(
            shared.mem.gc_cycle_count.load(Ordering::Relaxed),
            gcs_before
        );
    }

    /// Source witness: the wrapper escalates through `collect_and_retry`
    /// (overhead limit, soft-reference rung, last-ditch reclaim) rather than a
    /// copy of it -- the copy had drifted twice. Driving the ladder to its
    /// last rung needs a heap full of live data a unit test cannot build.
    #[test]
    fn multianewarray_escalates_through_the_shared_ladder() {
        let src = include_str!("../interpreter.rs");
        let head = "fn alloc_multi_array_collecting(";
        let start = src
            .find(head)
            .expect("alloc_multi_array_collecting not found");
        // `\u{7d}` is a closing brace, spelled so this line's braces balance
        // for the brace-counting production-panic scanner.
        let body_len = src[start..].find("\n\u{7d}\n").expect("function end");
        let body = &src[start..start + body_len];
        assert!(body.contains("collect_and_retry("));
        assert!(!body.contains("last_ditch_reclaim("));
        assert!(!body.contains("maybe_gc_forced_at("));
    }
}

#[cfg(test)]
mod w4c_alloc_front_end_tests {
    //! gc-common round 2026-09-23, wave 4, lane C4: VM-internal
    //! `NativeContextImpl` allocations outside a native callback are fallible
    //! and collect (`native_alloc_collecting`).

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::error::RuntimeError;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;
    use std::sync::atomic::Ordering;

    fn gen_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    /// An attempt refused by the heap (the native allocators' unwind) is
    /// collected for and retried, and the unwind permission is back to what
    /// it was afterwards -- this context is NOT a native callback.
    #[test]
    fn an_unwound_attempt_is_collected_for_and_retried() {
        let shared = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w4c-native-ladder");
        let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        assert!(!crate::runtime::native_oom::unwind_ok());
        let mut attempts = 0u32;
        let got = native_alloc_collecting(&shared, &mut thread, "w4c-test", |_, _| {
            attempts += 1;
            assert!(
                crate::runtime::native_oom::unwind_ok(),
                "the attempt must run with the unwind permission, so the \
                 allocators take their fallible arm"
            );
            if attempts == 1 {
                crate::runtime::native_oom::raise(4, "object");
            }
            7u32
        });
        assert!(matches!(got, Ok(7)), "got {got:?}");
        assert_eq!(attempts, 2);
        assert!(
            shared.mem.gc_cycle_count.load(Ordering::Relaxed) > gcs_before,
            "the retry must follow a collection"
        );
        assert!(!crate::runtime::native_oom::unwind_ok());
    }

    /// A length HotSpot refuses outright is reported with its own message and
    /// without collecting, like `newarray`.
    #[test]
    fn a_length_refusal_is_reported_without_collecting() {
        let shared = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w4c-native-limit");
        let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        let mut attempts = 0u32;
        let got: Result<(), RuntimeError> =
            native_alloc_collecting(&shared, &mut thread, "w4c-test", |_, _| {
                attempts += 1;
                crate::runtime::native_oom::raise(i32::MAX as usize, "primitive array");
            });
        match got {
            Err(RuntimeError::OutOfMemoryError { message }) => {
                assert_eq!(message, ARRAY_SIZE_EXCEEDS_VM_LIMIT)
            }
            other => panic!("expected OutOfMemoryError, got {other:?}"),
        }
        assert_eq!(attempts, 1);
        assert_eq!(
            shared.mem.gc_cycle_count.load(Ordering::Relaxed),
            gcs_before
        );
    }

    /// Heap exhaustion that survives the whole ladder is an
    /// `OutOfMemoryError` (the heap text), after exactly the ladder's two
    /// retries; and pins an unwound attempt pushed are dropped, as the
    /// native funnel drops them.
    #[test]
    fn exhaustion_after_the_ladder_is_an_oome_and_drops_stray_pins() {
        let shared = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w4c-native-oome");
        let pin_base = thread.native_pin_roots.len();
        let mut attempts = 0u32;
        let got: Result<(), RuntimeError> =
            native_alloc_collecting(&shared, &mut thread, "w4c-test", |shared, thread| {
                attempts += 1;
                let o = shared
                    .mem
                    .heap
                    .try_alloc_object(ClassId::new(0), 0)
                    .expect("an empty heap has room for one object");
                thread.native_pin_roots.push(o);
                crate::runtime::native_oom::raise(4, "object");
            });
        match got {
            Err(RuntimeError::OutOfMemoryError { message }) => {
                assert!(message.starts_with("Java heap space"), "{message}")
            }
            other => panic!("expected OutOfMemoryError, got {other:?}"),
        }
        // The first attempt plus the ladder's retries: two, or one when the
        // GC-overhead limit cuts the ladder short -- plus, since gcd d4/j, the
        // retry after the second major on Generational
        // (`second_major_before_oome`).
        assert!((2..=4).contains(&attempts), "attempts = {attempts}");
        assert_eq!(thread.native_pin_roots.len(), pin_base);
    }
}

#[cfg(test)]
mod w8v_ldc_literal_oom_tests {
    //! gc-common w8 verification: `ldc` of a String literal on a full heap is
    //! a catchable `OutOfMemoryError`, not a process abort
    //! (`intern_string_literal_or_oom`).

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;

    fn small_gen_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            max_heap_size: 8 * 1024 * 1024,
            initial_heap_size: 8 * 1024 * 1024,
            ..VmConfig::default()
        })
    }

    /// Two resolutions of one literal answer the identical pooled object.
    #[test]
    fn a_literal_is_pooled() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w8v-ldc-pool");
        let a = intern_string_literal_or_oom(&shared, &mut thread, "w8v-literal").expect("room");
        let b = intern_string_literal_or_oom(&shared, &mut thread, "w8v-literal").expect("hit");
        assert_eq!(a, b);
    }

    /// With every byte of the heap held live, the ladder runs and the literal
    /// is refused with `OutOfMemoryError` (the process is still here to see it).
    #[test]
    fn a_literal_on_a_full_heap_is_an_oome() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w8v-ldc-full");
        // Fill, collect, and fill again until a collection frees nothing: a
        // young collection promotes the pinned objects and empties eden, so
        // one pass of filling leaves room behind it.
        let mut full = false;
        for _ in 0..64 {
            // Both the young (TLAB-less) path and the whole-heap path, so
            // neither the young generation nor the old one keeps room.
            for full_heap in [false, true] {
                for slots in [1024usize, 64, 1] {
                    for _ in 0..1_000_000 {
                        let heap = &shared.mem.heap;
                        let got = if full_heap {
                            heap.try_alloc_object_full(ClassId::new(0), slots)
                        } else {
                            heap.try_alloc_object(ClassId::new(0), slots)
                        };
                        match got {
                            Some(o) => thread.native_pin_roots.push(o),
                            None => break,
                        }
                    }
                }
            }
            maybe_gc_forced_at(&shared, &mut thread, "w8v-test-fill");
            let heap = &shared.mem.heap;
            if heap.try_alloc_object(ClassId::new(0), 1).is_none()
                && heap.try_alloc_object_full(ClassId::new(0), 1).is_none()
            {
                full = true;
                break;
            }
        }
        assert!(full, "the pinned objects never filled the 8 MB heap");
        let got = intern_string_literal_or_oom(&shared, &mut thread, "w8v-literal-on-a-full-heap");
        match got {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError { message },
            ))) => assert!(message.starts_with("Java heap space"), "{message}"),
            Ok(_) => panic!("a pinned-full heap cannot hold the literal"),
            Err(other) => panic!("expected OutOfMemoryError, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod w6c_alloc_front_end_tests {
    //! gc-common round 2026-09-23, wave 6, lane C6: the allocation front end's
    //! counters are per VM (`common-f-process-global-allocation-state`).

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::memory::heap::ArrayElementType;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;
    use std::sync::atomic::Ordering;

    fn gen_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    /// Two VMs in one process, one allocating thread on VM B:
    /// * B's `thread_allocated_total` (the `getTotalThreadAllocatedBytes`
    ///   source) receives the thread's pre-first-refill external bytes (a
    ///   sub-batch, so held back until then) at the first refill's attach,
    ///   then its retired TLAB span, exactly once;
    /// * B's refill-time trigger counters move;
    /// * VM A's total and wedge counters do not move at all. With the old
    ///   process statics, A's answers included B's allocation.
    #[test]
    fn thread_allocation_total_and_wedge_counters_are_per_vm() {
        let vm_a = gen_vm();
        let vm_b = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w6c-per-vm");

        // A shared-path object before any refill: noted on the thread's
        // first buffer (`Tlab::empty()`), which the shared path attaches to
        // B's total; a sub-batch note is held back.
        let _obj = alloc_object_shared(&vm_b, &mut thread, ClassId::new(0), 3)
            .expect("an empty heap has room for one object");
        let external = thread.tlab.thread_allocated_bytes();
        assert!(external > 0);
        assert_eq!(vm_b.mem.thread_allocated_total.load(Ordering::Relaxed), 0);
        assert_eq!(
            thread.tlab.vm_thread_unpublished_bytes(),
            external,
            "the reader's own-thread term covers the pre-attach bytes"
        );

        // The first TLAB refill attaches this thread to B's total and credits
        // the pre-attach bytes once.
        let _arr = tlab_alloc_array_guarded_refill(
            &mut thread,
            &vm_b,
            ClassId::new(0),
            ArrayElementType::Int,
            4,
        )
        .expect("an empty young generation serves a TLAB refill");
        assert_eq!(
            vm_b.mem.thread_allocated_total.load(Ordering::Relaxed),
            external
        );
        assert!(
            vm_b.mem
                .tlab_wedge
                .refill_bytes_since_gc
                .load(Ordering::Relaxed)
                > 0,
            "the successful refill feeds B's healthy-path re-arm metric"
        );

        // Retiring settles the live span into B's total, and only once.
        thread.tlab.retire();
        thread.tlab.retire();
        assert_eq!(
            vm_b.mem.thread_allocated_total.load(Ordering::Relaxed),
            thread.tlab.thread_allocated_bytes()
        );
        assert_eq!(thread.tlab.vm_thread_unpublished_bytes(), 0);

        // VM A saw nothing.
        assert_eq!(vm_a.mem.thread_allocated_total.load(Ordering::Relaxed), 0);
        let wedge = &vm_a.mem.tlab_wedge;
        assert_eq!(wedge.refill_bytes_since_gc.load(Ordering::Relaxed), 0);
        assert_eq!(wedge.slowpath_entries_since_gc.load(Ordering::Relaxed), 0);
        assert_eq!(wedge.gate_consecutive_fails.load(Ordering::Relaxed), 0);
        assert_eq!(wedge.last_break_alloc_total.load(Ordering::Relaxed), 0);
    }

    /// Source witness for the retirement grep: none of the old process
    /// statics is declared any more.
    #[test]
    fn the_wedge_counters_are_not_process_statics() {
        let src = include_str!("gc_and_alloc.rs");
        for name in [
            "TLAB_GATE_CONSECUTIVE_FAILS",
            "TLAB_LAST_BREAK_ALLOC_TOTAL",
            "TLAB_SLOWPATH_ENTRIES_SINCE_GC",
            "TLAB_REFILL_BYTES_SINCE_GC",
        ] {
            let decl = format!("\nstatic {name}:");
            assert!(!src.contains(&decl), "{name} is a process static again");
        }
    }
}

#[cfg(test)]
mod w7b_alloc_plan_tests {
    //! gc-common round 2026-09-23: the `new` front end. Wave 7 (lane B7) put a
    //! per-thread plan cache here behind a default-off flag; wave 9
    //! retired it (the per-VM allocation recipe already serves every warm
    //! `new`, so it measured no gain -- `applied/handoff-w9f-retire-the-alloc-plan-cache-flag.md`).
    //! What stays: w9-f's stub-chain recipe test and B7's non-TLAB array charge.

    use super::*;
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::vm::SharedVm;
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::field::ClassFileField;
    use cratonvm_types::ClassId;

    fn gen_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    fn field(name: &str, desc: &str, flags: FieldAccessFlags) -> ClassFileField {
        ClassFileField {
            access_flags: flags,
            name: std::sync::Arc::from(name),
            descriptor: std::sync::Arc::from(desc),
            attributes: vec![],
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_class(
        shared: &SharedVm,
        name: &str,
        superclass: Option<ClassId>,
        fields: Vec<ClassFileField>,
        first_field_index: usize,
        num_total_fields: usize,
        has_finalizer: bool,
        origin: cratonvm_classloading::ClassOrigin,
    ) -> ClassId {
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::empty(),
            superclass,
            interfaces: vec![],
            fields,
            methods: vec![],
            first_field_index,
            num_total_fields,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin,
            has_finalizer,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        cm.register_class_name(ClassLoaderId::Application, name, id);
        id
    }

    fn slots(shared: &SharedVm, obj: ObjectRef, n: usize) -> Vec<Value> {
        (0..n).map(|i| shared.mem.heap.get_field(obj, i)).collect()
    }

    /// gc-common w9-f: the allocation RECIPE (`jit::alloc_class_cache`), which
    /// the interpreter's `new` consults before anything else, refuses a stub
    /// chain for the reason the plan above does, so a `new` follows an
    /// in-place stub upgrade.
    ///
    /// The upgrade is staged as `upgrade_synthetic_class` performs it when the
    /// synthetic field-count floor keeps the parent's total: same slot count,
    /// the slot changes kind (`J` -> `L`), and no layout number of the
    /// subclass moves, so nothing unpublishes the subclass's recipe. Before
    /// the refusal the first `new` published `Sub`'s recipe with the stub's
    /// `J`, and every later `new` wrote `Long(0)` into what is now a reference
    /// slot.
    #[test]
    fn a_stub_chain_gets_no_recipe_so_new_follows_an_in_place_upgrade() {
        use crate::jit::alloc_class_cache::{alloc_class_cache_enabled, ClassAllocInfo};
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let shared = gen_vm();
        let stub = add_class(
            &shared,
            "cratonvm/test/w9f/StubBase",
            None,
            vec![field("x", "J", FieldAccessFlags::PRIVATE)],
            0,
            1,
            false,
            cratonvm_classloading::ClassOrigin::CompatibilityStub {
                reason: std::sync::Arc::from("w9f test"),
            },
        );
        let sub = add_class(
            &shared,
            "cratonvm/test/w9f/SubOfStub",
            Some(stub),
            vec![field("r", "Ljava/lang/Object;", FieldAccessFlags::PRIVATE)],
            1,
            2,
            false,
            cratonvm_classloading::ClassOrigin::default(),
        );
        {
            let cm = shared.classes.class_manager.read();
            assert!(ClassAllocInfo::build(&cm.class_store, stub).is_none());
            assert!(
                ClassAllocInfo::build(&cm.class_store, sub).is_none(),
                "a stub anywhere in the chain refuses the recipe"
            );
        }
        let mut thread = JvmThread::new(ThreadId(0), "w9f-stub-chain");
        for _ in 0..2 {
            let obj = gc_alloc_object(&shared, &mut thread, sub, 2).expect("a fresh heap allocates");
            assert_eq!(
                slots(&shared, obj, 2),
                vec![Value::Long(0), Value::Object(None)],
                "the stub's layout, through the classic walk"
            );
        }
        assert!(
            shared.jit.jit_alloc_class_cache.get(sub.as_u32()).is_none(),
            "no recipe may be published for a stub chain"
        );

        // The in-place upgrade: the stub's `x` becomes a reference, the class
        // gets its real origin, and neither class's layout numbers move.
        {
            let mut cm = shared.classes.class_manager.write();
            let class = cm.class_store.get_mut(stub).expect("the stub is in the store");
            class.fields = vec![field("x", "Ljava/lang/Object;", FieldAccessFlags::PRIVATE)];
            class.set_origin(cratonvm_classloading::ClassOrigin::default());
        }
        for _ in 0..2 {
            let obj = gc_alloc_object(&shared, &mut thread, sub, 2).expect("allocates");
            assert_eq!(
                slots(&shared, obj, 2),
                vec![Value::Object(None), Value::Object(None)],
                "a `new` after the upgrade writes the real layout's defaults"
            );
        }
        if alloc_class_cache_enabled() {
            let info = shared
                .jit
                .jit_alloc_class_cache
                .get(sub.as_u32())
                .expect("a stub-free chain is cached again");
            assert!(info.prim_inits.is_empty());
            assert_eq!(&*info.ref_inits, &[1, 0], "the class, then its superclass");
        }
    }

    /// An array too big for a TLAB is charged to the VM's
    /// `bytes_allocated_total` as well as to the thread, exactly as an object
    /// on the shared path is (`the_shared_object_path_charges_the_real_footprint`).
    #[test]
    fn a_non_tlab_array_is_charged_to_the_vm_allocation_total() {
        use crate::memory::heap::ArrayElementType;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        use std::sync::atomic::Ordering;
        let shared = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w7b-array-total");
        let length = cratonvm_gc::tlab::tlab_max_alloc() * 2;
        let expected = external_array_bytes(ArrayElementType::Byte, length) as u64;
        let vm_before = shared.mem.bytes_allocated_total.load(Ordering::Relaxed);
        let thread_before = thread.tlab.thread_allocated_bytes();
        gc_alloc_array(
            &shared,
            &mut thread,
            ClassId::new(0),
            ArrayElementType::Byte,
            length,
        )
        .expect("an empty heap has room for one 64 KiB array");
        assert_eq!(
            shared.mem.bytes_allocated_total.load(Ordering::Relaxed) - vm_before,
            expected
        );
        assert_eq!(thread.tlab.thread_allocated_bytes() - thread_before, expected);
    }

}

/// Update the thread's root snapshot with current frame ObjectRefs.
/// Called at safepoints and before blocking operations.
///
/// **Spring Boot SEGV fix (2026-05-16), as it stands today:** the
/// freshly-scanned operand-stack roots are screened before they join the
/// snapshot, because `ValueStack::scan_object_refs` used to report every
/// `CompactTag::Long` slot whose bits looked like an aligned pointer as a
/// heap root without consulting the heap — a primitive `long` carrying a file
/// size or a jboss-modules token then poisoned the root set and SEGV'd the GC.
///
/// The screen is `heap.is_heap_addr` (alignment + arena containment), which is
/// what kills that population. It was `is_object_address` until 2026-08-04;
/// that is a strictly stronger probe and it also dropped GENUINE roots — see
/// `scan_frame_roots` for why, and for the two in-tree fixes the strict
/// version was silently reverting. Locals (screened the same way inside
/// `Frame::scan_local_objects`), `native_pin_roots`, and
/// `native_pending_return` come from validated paths and are appended after
/// the filter.
/// DBG (CRATONVM_DBG_ROOTSNAP): instrumentation for the per-native-call root
/// snapshot cost. Confirms/quantifies whether `update_root_snapshot` is the
/// embedded-server deployment hotspot (O(stack-depth) full-frame scan + the
/// per-operand-stack-object `is_object_address` triple-lock validation, run on
/// every object-returning native call). Prints a cumulative line every 200k
/// calls. Default-off; zero cost when the gate is unset (cached OnceLock).
pub(super) fn rootsnap_dbg_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOTSNAP").is_some())
}
/// DBG (CRATONVM_DBG_ROOTSNAP_VERIFY): after the frozen-frame cache builds a
/// snapshot, re-scan every frame the default (uncached) way and diff the two.
/// The cached path claims to be byte-identical to the default path; any root
/// the fresh scan reports that the cached snapshot lacks is a LOST ROOT —
/// precisely the failure the retired real-ForkJoinPool bypass claimed to
/// prevent, and the check that retired it (see `update_root_snapshot`). Prints
/// a line per miss plus a periodic tally; run it under `CRATONVM_DBG_ROOTSNAP`
/// so the tally shows how much of the snapshot came from the cache. Costs a
/// full extra frame scan per snapshot, so it is a diagnostic, not a default:
/// unset, this is one cached `OnceLock` read.
pub(super) fn rootsnap_verify_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOTSNAP_VERIFY").is_some()
    })
}

/// Print period for the two DBG tallies (`CRATONVM_DBG_ROOTSNAP_EVERY`,
/// default 200k). Short workloads publish far fewer snapshots than that, so a
/// smaller period is what makes "the cache was actually exercised" visible
/// rather than assumed.
pub(super) fn rootsnap_dbg_every() -> u64 {
    use std::sync::OnceLock;
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_ROOTSNAP_EVERY")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(200_000)
    })
}

static ROOTSNAP_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// Cache engagement (`CRATONVM_DBG_ROOTSNAP`): snapshots that took the cached
// path, and how many frames / roots those reused instead of re-scanning.
static ROOTSNAP_CACHED_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_REUSED_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_REUSED_ROOTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// Cache soundness (`CRATONVM_DBG_ROOTSNAP_VERIFY`): snapshots diffed against a
// fresh full scan, and the roots the cached snapshot was missing.
static ROOTSNAP_VERIFIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_MISS_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Scan ONE frame's GC roots (locals + operand stack, with the operand-stack
/// pointer-shaped-Long validation) onto the end of `out`. This is exactly the
/// per-frame body of `update_root_snapshot`'s loop, factored out so the opt-in
/// root-snapshot cache can scan a single frame into a cache entry as well as
/// into the live snapshot. Behaviour is byte-identical to the inline loop.
///
/// The operand-stack post-filter is `is_heap_addr` (alignment + arena
/// containment), NOT the strict `is_object_address` header probe, and the two
/// are not interchangeable here:
///
/// * `is_object_address` rejects a young / mid-init object whose header it
///   cannot yet vouch for. `Frame::scan_local_objects` has always used the
///   loose screen for exactly that reason (see its comment naming the
///   BouncyCastle `EC5Util.getCurve` local), and `ValueStack::scan_object_refs`
///   was changed to root a genuine object slot unconditionally with the same
///   argument — "the prior code dropped such roots (use-after-free risk for
///   newly allocated objects) — the regression this fixes". Re-applying the
///   strict probe over its output put that regression straight back, on the
///   operand-stack half only.
/// * It also discarded the JNI long-as-jobject smuggle roots that
///   `scan_object_refs` validates with `is_heap_addr` precisely because the
///   strict probe rejects them (interior offsets, mid-init) — the WildFly
///   `jboss-modules` SEGV that comment records.
///
/// The filter's original justification — that `scan_object_refs` "treats
/// pointer-shaped `Long` bits as roots without heap validation" — is no longer
/// true: that scan is kind-gated (`KIND_LONG`/`KIND_DOUBLE` slots are never
/// rooted through the object branch) and heap-validates the smuggle branch.
/// What remains is a loose root reaching the collector, which BOTH generations
/// already screen with an exact object-base oracle before writing anything
/// through it — `young_object_starts` membership in `forward_object`, and
/// `walk_objects`-derived `walked_bases` in `old_gen_gc`. Over-retention for
/// one cycle is the whole cost; a dropped root is a use-after-free.
#[inline]
pub(super) fn scan_frame_roots(
    frame: &Frame,
    out: &mut Vec<ObjectRef>,
    heap: &crate::memory::VmHeap,
    minted_only_primitives: bool,
) {
    frame.scan_local_objects(out, heap);
    let before = out.len();
    frame
        .stack
        .scan_object_refs_ruled(out, heap, minted_only_primitives);
    let len = out.len();
    if len > before {
        let mut write = before;
        for read in before..len {
            let o = out[read];
            // Cast: object/code pointer to integer address
            if heap.is_heap_addr(o.as_ptr() as usize).is_some() {
                out[write] = o;
                write += 1;
            }
        }
        out.truncate(write);
    }
}

/// `CRATONVM_GC_G1_ONLY_JIT_PINS=1` -- restore the pre-2026-09-06 gate on the
/// two deposit-side publications of a parked thread's CONSERVATIVE JIT-frame
/// roots into `gc_quiescence`'s process-global pin registry.
///
/// Those two sites were written when G1 was the only backend that moved, and
/// their own comments said so: *"they can only over-retain (the young sweep
/// runs non-moving while any thread is in JIT, so nothing is relocated)"*.
/// That premise expired twice and neither expiry reached the gate:
///
///  * ZGC compaction shipped 2026-08-13, and ZGC withholds the PAGE of every
///    address in `pinned_jit_roots_snapshot()` from its relocation set
///    (`zgc.rs`, "CONSERVATIVE JIT ROOTS PIN THEIR PAGE TOO"). With the
///    publication gated on G1, that consumer only ever saw the INITIATOR's
///    pins -- every parked peer's compiled frames were unprotected.
///  * The cross-thread JIT coverage handshake (2026-08-23) let the
///    generational moving-young cycle run while peers are in JIT, so its
///    Cheney copy started relocating exactly the objects the comment promised
///    it would not.
///
/// Measured on the `qdox-parser-static-array-race` reproducer (4 threads x
/// 400 fresh-builder QDox parses, Azure 20.80.105.49):
/// ZGC lost 1-11 parses per run to `ArrayIndexOutOfBoundsException: Index N
/// out of bounds for length 0` -- ZGC's own documented signature for a
/// conservative root left pointing at a vacated, zeroed span -- and
/// Generational SIGSEGV'd 3/3 inside compiled `java/lang/StringUTF16.compress`
/// on an array whose base had been evacuated. G1, the one backend the gate
/// admitted, was clean 3/3.
///
/// Publishing unconditionally costs one `Vec<usize>` collect and one map
/// insert per deposit, on a path that has just finished a conservative stack
/// scan. Nothing consumes the registry on a backend that does not relocate.
pub(crate) fn g1_only_jit_pins() -> bool {
    // gcd d1/d: the typed field (`GcFlags::g1_only_jit_pins`), the one the
    // collector's pinned-compaction plan reads too.
    cratonvm_types::flags().gc.g1_only_jit_pins
}

/// `CRATONVM_GC_NO_REFPROC_PREPASS=1` — put the generational post-GC path back
/// on the pre-2026-09-21 order, where `remove_collected` runs only AFTER
/// reference processing.
///
/// A kill switch for the reordering described at its call site in
/// `process_references_after_gc`, not a tuning knob: with it set, a
/// `SoftReference` whose own instance died in the cycle again roots the
/// weak-clearing closure, which is the defect
/// `gengc-mark-dead-softref-object-still-roots-weak-closure-FIXED-20260923.md`
/// describes. It exists so a bisect can attribute a reference-processing
/// change to this reordering in one run rather than by rebuilding.
///
/// `=0` / `=false` / `=off` mean OFF (gc-common w2-a, 2026-09-23). It was
/// presence-parsed, so the legacy spelling `CRATONVM_GC_NO_REFPROC_PREPASS=0`
/// — the natural way to say "leave it off" — switched the kill switch ON and
/// silently restored the weak-clearing defect above
/// (`docs/internal/gc-common-round-20260923/common-e-small-findings-FIXED-20260923.md` #3). The grouped `CRATONVM_GC=` spelling
/// inserts or removes the key and is unaffected; presence with any other
/// value (including empty) is still ON.
pub(crate) fn no_refproc_prepass() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        match cratonvm_types::flags::runtime_var_os("CRATONVM_GC_NO_REFPROC_PREPASS") {
            None => false,
            Some(v) => {
                let v = v.to_string_lossy();
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            }
        }
    })
}

/// `CRATONVM_DBG_BLOCKGC`, read once (override-aware `MemoSlot`), for the
/// safepoint deposit and the safepoint-resume remap.
///
/// gc-common 2026-09-23: the SAFEPOINT-STALE hunter in `update_root_snapshot`
/// read this flag with an UNCACHED `runtime_var_os` on every call, and that
/// function runs on every safepoint park as well as at every GC entry. The
/// same flag's per-call probe on the native funnel was measured at ~7% of a
/// HashMapOnly run before `vm_exec::blockgc_dbg` cached it. Hoisted out of
/// `update_root_snapshot` in w2-g so `apply_pointer_map_to_thread` -- which
/// ran the same uncached probe once per parked thread per relocating pause --
/// shares it.
#[inline]
fn peer_blockgc_dbg() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some();
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// `CRATONVM_DBG_BUG03`, read once (override-aware `MemoSlot`). The
/// safepoint-resume remap probed it uncached for every pause on thread 0.
#[inline]
fn peer_bug03_dbg() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BUG03").is_some();
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

pub(crate) fn update_root_snapshot(shared: &SharedVm, thread: &mut JvmThread) {
    remap_trace_push(shared, thread, "publish", "");
    // CRATONVM_DBG_CORRUPT_CELL backstop. Every instrumented door above names
    // its own receiver; this catches a corrupt cell decoded by a reader that
    // has NO door -- reflection, `Unsafe`, a class-mirror populator, a
    // serialization walk -- and reports it with this thread's frames rather
    // than letting it pass as silence. A one-door instrument cannot tell "no
    // defect" from "not my door".
    crate::memory::reclaim_guard::corrupt_cell_backstop(shared, thread);
    // Stamp when this thread last published, so a stale-address report can say
    // how many collections completed since. See `note_root_publish`.
    crate::memory::reclaim_guard::note_root_publish(shared, thread);
    // cceres3 FIX: self-heal a leaked blocked-region exit. If a blocking
    // native returned without `check_post_block_gc` (unpaired exit), this
    // thread is running with an unconsumed fixup chain / slot-origin set —
    // its frames still hold from-space addresses from every GC it slept
    // through. Apply them here, at the first safepoint publish, before this
    // thread's stale refs can leak into reachable object graphs.
    //
    // ORDER (gc-common w2-g, 2026-09-23): clear a raised flag FIRST, take and
    // apply the fixup SECOND -- the order `check_post_block_gc_refs` documents
    // for the ordinary wake. The heal used to run first. While the flag is up,
    // every pause that completes FOLDS its map into this thread's fixup, and
    // `leave_blocked_region_flagged` waits such a pause out before clearing
    // the flag; so a pause that completed between the heal's take and the
    // clear left its fold in `fixup` with nothing to apply it. The thread then
    // rebuilt its snapshot from the un-remapped frames (publishing vacated
    // addresses as roots) and ran on them until its next GC-time publish.
    // After the clear, no newer pause can complete (or fold) without this
    // thread arriving, so the fixup taken below is final.
    {
        // A raised flag at an interpreter safepoint means some raise site's
        // exit skipped `check_post_block_gc` (the monitor_wait early-return
        // bug class): this thread is RUNNING, yet every census still excludes
        // it, so moving collections keep completing under its feet. Restore
        // the invariant: wait out any in-flight pause and clear the flag
        // (idempotent with the eventual legitimate wake, whose fixup-take
        // then finds an empty map).
        if thread
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire)
        {
            shared.mem.gc_barrier.leave_blocked_region_flagged(
                thread.thread_id,
                &thread.gc_block_state.in_blocked_region,
            );
            if peer_blockgc_dbg() {
                eprintln!(
                    "[blockgc] SAFEPOINT-FLAG-CLEAR tid={} - in_blocked_region was raised on a running thread",
                    thread.thread_id.0,
                );
            }
        }
        let pending = !thread.gc_block_state.fixup.lock().is_empty()
            || thread
                .gc_block_state
                .slot_origins
                .lock()
                .iter()
                .any(|so| so.cur != so.orig)
            // The native-stack write-back is a third channel with the same
            // leaked-exit exposure, and testing only the first two let a
            // thread whose ONLY pending repair was a raw stack word run on
            // with it. See `apply_native_slot_fixups`.
            || thread
                .gc_block_state
                .native_slots
                .lock()
                .iter()
                .any(|ns| ns.cur != ns.orig);
        if pending {
            let n = crate::vm::vm_exec::apply_pending_blocked_fixups(shared, thread);
            if n > 0 && peer_blockgc_dbg() {
                eprintln!(
                    "[blockgc] SAFEPOINT-HEAL tid={} applied {} pending fixups (leaked blocked-region exit upstream)",
                    thread.thread_id.0, n,
                );
            }
        }
    }
    // H2-CID0: once per collection per thread, ask whether any LIVE frame slot
    // now points into memory the collector reclaimed. The blocked-region
    // deposit/wake pair covers a PARKED thread; this covers a RUNNING one, and
    // it bounds the loss window to "since the previous safepoint" — which no
    // reader-side reporter can do, because by the time a `checkcast` or an
    // `invoke` trips over the address, any number of collections have passed.
    //
    // One relaxed load per publish on the common path; the frame walk runs only
    // when the collection counter actually moved. Unconditional, and that is
    // the point: this family has been chased across four sessions on runs that
    // were never armed.
    //
    // gc-common w7-g: the stamp is keyed by VM. It is an OS-thread local, and
    // one OS thread can publish for two VMs (a host thread attached to both);
    // comparing VM A's collection count with VM B's ran the audit for no
    // collection, or skipped it for a real one whenever the two counts
    // happened to agree. A VM switch re-seeds the stamp, like the first
    // publish does.
    {
        thread_local! {
            static AUDIT_CC: std::cell::Cell<(usize, u64)> =
                const { std::cell::Cell::new((0, u64::MAX)) };
        }
        let cc = shared.mem.heap.collection_count();
        let (prev_vm, prev) = AUDIT_CC.with(|c| c.replace((shared.vm_identity, cc)));
        if prev_vm == shared.vm_identity && cc != prev && prev != u64::MAX {
            crate::memory::reclaim_guard::audit_thread_frames(
                shared,
                thread,
                "running frame slot (safepoint)",
            );
        }
    }
    // DIAGNOSTIC-ONLY (cceres3): first-miss hunter. Once per GC epoch per
    // thread, verify no frame slot holds an already-forwarded (quarantined)
    // address at the safepoint publish. A hit here bounds the miss window to
    // "since the previous safepoint" on a RUNNING thread, which none of the
    // DEPOSIT/WAKE/ARRIVE verifiers can see.
    if peer_blockgc_dbg() {
        // Keyed by VM for the same reason as `AUDIT_CC` above (gc-common w7-g).
        thread_local! {
            static LAST_CC: std::cell::Cell<(usize, u64)> =
                const { std::cell::Cell::new((0, u64::MAX)) };
        }
        let cc = shared.mem.heap.collection_count();
        let (prev_vm, prev) = LAST_CC.with(|c| c.replace((shared.vm_identity, cc)));
        if prev_vm == shared.vm_identity && cc != prev && prev != u64::MAX {
            for (fi, fr) in thread.frames.iter().enumerate() {
                for li in 0..fr.locals_len() {
                    if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                        let a = o.as_ptr() as usize;
                        if let Some(new) = shared.mem.heap.debug_forwarded_target(a) {
                            eprintln!(
                                "[blockgc] SAFEPOINT-STALE e{cc} tid={} frame#{fi} {}.{} pc={} local[{li}] 0x{a:x}->0x{new:x}",
                                thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            );
                        }
                    }
                }
                for si in 0..fr.stack.len() {
                    if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                        let a = o.as_ptr() as usize;
                        if let Some(new) = shared.mem.heap.debug_forwarded_target(a) {
                            eprintln!(
                                "[blockgc] SAFEPOINT-STALE e{cc} tid={} frame#{fi} {}.{} pc={} stack[{si}] 0x{a:x}->0x{new:x}",
                                thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            );
                        }
                    }
                }
            }
        }
    }

    // The per-thread JIT memo caches are published below as roots for as long
    // as they are present; a pause completed since this thread last probed
    // them means they can never be served again (the probe would clear them),
    // so drop them now instead of rooting a dropped `HashMap` for another
    // cycle. `docs/internal/gc-common-round-20260923/common-w2b-per-thread-jit-caches-retain-dropped-maps-FIXED-20260923.md`.
    crate::memory::roots::drop_stale_jit_memo_caches(shared, thread);

    let _rs_t0 = if rootsnap_dbg_enabled() {
        Some((std::time::Instant::now(), thread.frames.len()))
    } else {
        None
    };
    // Each frame's class owner (defining user loader / non-strong hidden
    // mirror), gathered BEFORE the snapshot lock: see `roots::frame_class_owners`
    // (gc-common w19-a). Appended after the frame scan, outside the frozen-frame
    // cache's per-frame ranges.
    let class_owners = crate::memory::roots::frame_class_owners(shared.vm_identity, thread);
    // Lock via an Arc clone so the guard does not borrow `thread`, leaving the
    // disjoint `frames` / `rs_cache` fields freely (mutably) borrowable below.
    let snap_arc = thread.root_snapshot.clone();
    let mut snapshot = snap_arc.lock();
    snapshot.clear();

    // The frozen-frame cache yields to the default path while the
    // conservative-locals hardening is engaged: the cached `scan_frame_roots`
    // does not run `scan_locals_conservative`, so caching under it could drop a
    // lost-tag local from the snapshot. `conservative_locals_enabled()` reads
    // the RESOLVED real-ForkJoinPool flag and is additionally gated on GC
    // quiescence, i.e. it answers "is the hardening running right now" rather
    // than "was the lane explicitly requested".
    //
    // There is no separate real-ForkJoinPool bypass any more (2026-07-31): the
    // old one keyed off the PRESENCE of `CRATONVM_REAL_FORKJOINPOOL`, which
    // stopped being set when the real pool became the default, so it had not
    // fired on a default run since that flip. Before removing it the cache was
    // measured against the loss it claimed to prevent — see
    // `rootsnap_verify_enabled` (`CRATONVM_DBG_ROOTSNAP_VERIFY`), which
    // re-scans every frame the uncached way after each cached snapshot and
    // reports any root the cached snapshot lacks; the real-lane
    // Fork6/Fork6Hard GC-stress repros reported none.
    //
    // The mechanism behind that result: the real-FJP lane keeps a Bridge native
    // surface for `submit`/`invoke`/`fork`/`join`, which runs every task INLINE
    // on the submitting thread (measured: every `compute()` in this lane runs
    // on `main`, vs 15 pool workers on HotSpot). The hazard the bypass
    // described — a pool worker holding a forked subtask in its own frames
    // across a peer-triggered collection — has no thread to happen on. If real
    // ForkJoinPool ever gains workers that actually execute tasks, re-run the
    // verifier against that lane before trusting this.
    let conservative_locals = crate::memory::roots::conservative_locals_enabled();
    if crate::runtime::env_cache::rootsnap_cache() && !conservative_locals {
        // ── Frozen-frame cached path ────────────────────────────────────────
        // Reuse the cached roots of the deep, continuously-frozen frames and
        // re-scan only the churning top. Correctness rests on the LIFO stack
        // discipline: if `frames[k]` is still the SAME instance (`seq`
        // unchanged) it has never been popped, so by the stack property every
        // frame *below* it (`0..k`) has been continuously present AND frozen
        // (you cannot pop the middle of a stack) — their cached roots are still
        // exact. A GC may have *moved/promoted* objects, changing addresses, so
        // the whole cache is only valid while `collection_count()` is unchanged.
        let gen = shared.mem.heap.collection_count();
        let len = thread.frames.len();
        // Longest prefix whose frame instances are unchanged since the cache
        // was built (prefix-closed: a matching `frames[p]` implies every frame
        // below it matches too).
        let mut p = 0usize;
        if gen == thread.rs_cache_gen {
            let maxk = thread.rs_cache.len().min(len);
            while p < maxk
                && thread.frames[p].seq != 0
                // Key on (seq, exec_epoch): seq proves the frame was never
                // popped; exec_epoch proves it has not RE-EXECUTED (and thus
                // possibly reassigned a local) since it was cached. A mismatch in
                // either ends the reusable prefix so this frame and all above it
                // are re-scanned — the fix for the stale-reassigned-local AME.
                && (thread.frames[p].seq, thread.frames[p].exec_epoch) == thread.rs_cache[p].0
            {
                p += 1;
            }
        }
        // The deepest matching frame (`p-1`) VOUCHES for everything strictly
        // below it, but its own above-neighbour is the divergence point, so its
        // own roots may be stale — exclude it. Never reuse the current top
        // (index `len-1`), which churns its operand stack between snapshots.
        let reuse = p.saturating_sub(1).min(len.saturating_sub(1));

        // Build the next cache as we go (frozen frames `0..len-1`); the top is
        // never cached.
        let mut new_cache: Vec<((u64, u64), Vec<ObjectRef>)> =
            Vec::with_capacity(len.saturating_sub(1));
        if _rs_t0.is_some() {
            use std::sync::atomic::Ordering::Relaxed;
            ROOTSNAP_CACHED_CALLS.fetch_add(1, Relaxed);
            ROOTSNAP_REUSED_FRAMES.fetch_add(reuse as u64, Relaxed);
            ROOTSNAP_REUSED_ROOTS.fetch_add(
                thread.rs_cache[..reuse]
                    .iter()
                    .map(|e| e.1.len() as u64)
                    .sum::<u64>(),
                Relaxed,
            );
        }
        // (a) reused frozen frames — copy their cached roots into the snapshot
        //     and carry the entry forward (move, no re-alloc).
        for i in 0..reuse {
            snapshot.extend_from_slice(&thread.rs_cache[i].1);
            new_cache.push(std::mem::take(&mut thread.rs_cache[i]));
        }
        // (b) re-scan `reuse..len`. Frozen ones (`< len-1`) are scanned into the
        //     snapshot and cached; the top is scanned into the snapshot only.
        for i in reuse..len {
            let start = snapshot.len();
            scan_frame_roots(
                &thread.frames[i],
                &mut snapshot,
                &shared.mem.heap,
                shared.config.is_jdk_only(),
            );
            // The frame's implicit `monitorexit` target -- see the default
            // path below. Cached with the frame: it is fixed for the frame's
            // life and remapped with the rest of the cache.
            if let Some(m) = thread.frames[i].monitor_on_exit {
                snapshot.push(m);
            }
            // Block monitors (`Frame::held_monitors`, wave 23 lane L7). Unlike
            // `monitor_on_exit` they change during the frame's life, but only
            // while it is the top frame, which is never cached, and a frame
            // becomes the top again only through a return into it, which
            // bumps the `exec_epoch` the cache key includes.
            snapshot.extend_from_slice(thread.frames[i].held_monitors.as_slice());
            if i + 1 < len {
                new_cache.push((
                    (thread.frames[i].seq, thread.frames[i].exec_epoch),
                    snapshot[start..].to_vec(),
                ));
            }
        }
        thread.rs_cache = new_cache;
        thread.rs_cache_gen = gen;
        if rootsnap_verify_enabled() {
            use std::sync::atomic::Ordering::Relaxed;
            let mut fresh: Vec<ObjectRef> = Vec::with_capacity(snapshot.len());
            for frame in &thread.frames {
                scan_frame_roots(
                    frame,
                    &mut fresh,
                    &shared.mem.heap,
                    shared.config.is_jdk_only(),
                );
            }
            ROOTSNAP_VERIFIED.fetch_add(1, Relaxed);
            let have: std::collections::HashSet<usize> =
                snapshot.iter().map(|o| o.as_ptr() as usize).collect();
            let mut missed = 0u64;
            for (i, o) in fresh.iter().enumerate() {
                let a = o.as_ptr() as usize;
                if !have.contains(&a) {
                    missed += 1;
                    if ROOTSNAP_MISSES.load(Relaxed) < 40 {
                        eprintln!(
                            "[ROOTSNAP-MISS] tid={} depth={} reuse={} fresh_idx={} addr=0x{a:x} top={}.{}",
                            thread.thread_id.0,
                            len,
                            reuse,
                            i,
                            thread.frames[len - 1].class_name(),
                            thread.frames[len - 1].method_name(),
                        );
                    }
                }
            }
            if missed > 0 {
                ROOTSNAP_MISSES.fetch_add(missed, Relaxed);
                ROOTSNAP_MISS_CALLS.fetch_add(1, Relaxed);
            }
            let n = ROOTSNAP_VERIFIED.load(Relaxed);
            if n == 1 || n % rootsnap_dbg_every() == 0 {
                eprintln!(
                    "[ROOTSNAP-VERIFY] verified_snapshots={} miss_snapshots={} missed_roots={}",
                    n,
                    ROOTSNAP_MISS_CALLS.load(Relaxed),
                    ROOTSNAP_MISSES.load(Relaxed),
                );
            }
        }
    } else {
        // ── Default path (unchanged) ────────────────────────────────────────
        // Multi-thread non-moving-sweep root hardening (Fork6): see
        // `roots::conservative_locals_enabled`. Capture lost-tag object refs in
        // THIS thread's frame locals so a parked/running worker (or main)
        // publishes them, pinning them against selective-promotion evacuation.
        // (`conservative_locals` was computed above, where it also gates the
        // cached path off so this hardening is never skipped.)
        for frame in &thread.frames {
            if conservative_locals {
                // The non-moving FJP stress path uses root values as pins. A
                // liveness-filtered scan can drop an active task receiver at a
                // call boundary; all-live reference scanning only over-retains
                // in this collector and prevents reclaiming that receiver.
                frame.scan_local_objects_all_live(&mut snapshot, &shared.mem.heap);
            } else {
                // Same roots as `scan_local_objects`; the VM's type maps feed
                // only the `CRATONVM_DBG_VERIFY_OOP_MAPS` shadow.
                frame.scan_local_objects_mapped(
                    &mut snapshot,
                    &shared.mem.heap,
                    &shared.classes.type_maps,
                );
            }
            if conservative_locals {
                frame.scan_locals_conservative(&mut snapshot, &shared.mem.heap);
            }
            let before = snapshot.len();
            frame.stack.scan_object_refs_ruled(
                &mut snapshot,
                &shared.mem.heap,
                shared.config.is_jdk_only(),
            );
            // Validate every operand-stack-sourced root against the heap.
            // `Frame::scan_local_objects` was already cleaned to drop the
            // pointer-shaped-Long heuristic; the operand-stack scanner is
            // restricted from edits, so filter at the boundary instead.
            //
            // Done IN PLACE (compact valid entries down over the invalid ones,
            // then truncate) rather than `split_off` — `update_root_snapshot` runs
            // on every object-returning native call (tens of millions during an
            // embedded-server deploy), and the old `split_off` allocated a fresh
            // Vec for every frame that had operand-stack objects. The in-place
            // retain is allocation-free and keeps identical semantics.
            let len = snapshot.len();
            if len > before {
                let mut write = before;
                for read in before..len {
                    let o = snapshot[read];
                    // `is_heap_addr`, not `is_object_address` — see
                    // `scan_frame_roots`, whose body this loop duplicates.
                    // Cast: object/code pointer to integer address
                    if shared.mem.heap.is_heap_addr(o.as_ptr() as usize).is_some() {
                        snapshot[write] = o;
                        write += 1;
                    }
                }
                snapshot.truncate(write);
            }
            if conservative_locals {
                frame
                    .stack
                    .scan_object_refs_conservative(&mut snapshot, &shared.mem.heap);
            }
            // A synchronized method's implicit `monitorexit` target
            // (gc-common w2-g, 2026-09-23). The initiator (`collect_roots` §1)
            // and the blocked deposit (`deposit_root_snapshot_inner`) publish
            // it; this safepoint deposit did not, while its resume half
            // (`apply_pointer_map_to_thread`) has always REWRITTEN it. The
            // receiver usually still sits in local 0, but the local scan above
            // is liveness-filtered: once local 0 is dead at the parked pc (or
            // the bytecode reused the slot) the lock object had no root in a
            // peer-initiated collection, and the frame's pop then ran
            // `monitorexit` on a reclaimed address.
            if let Some(m) = frame.monitor_on_exit {
                snapshot.push(m);
            }
            // Block monitors (`Frame::held_monitors`, wave 23 lane L7);
            // remapped by `apply_pointer_map_to_thread`.
            snapshot.extend_from_slice(frame.held_monitors.as_slice());
        }
    }
    // A live activation keeps its class (gc-common w19-a,
    // `common-w18d-peer-interpreter-activations-do-not-keep-their-class`): the
    // initiator's `collect_roots` §1 has always pushed these for its own
    // frames; a parked peer's frames reach a collection only through here.
    snapshot.extend_from_slice(&class_owners);
    // Every reference held in a `JvmThread` FIELD rather than a frame, through
    // the ONE list the initiator's scan and the blocked deposit also use
    // (gc-common w4-b; `handoff-w3b-peer-deposits-use-push-thread-field-roots.md`):
    // native invoke pins, rooted handle slots (a peer-initiated collection
    // sees this parked thread only through `root_snapshot`), the native
    // old-gen allocation pool, the native object in flight, the print buffer,
    // scoped values, both parked throwables, both JIT memo caches (stale ones
    // dropped above -- TOMCAT-JNDIREALM-JIT.3 was a case-cache entry missing from
    // this deposit), this thread's own `java.lang.Thread` mirror (Tomcat
    // TestDigestAuthenticator: a worker-thread GC orphaned the parked JUnit
    // main thread's mirror) and any pending async exception. The resume-side
    // remap of all of them is `memory::gc::remap_thread_off_frame_refs`, called
    // from `apply_pointer_map_to_thread`.
    crate::memory::roots::push_off_frame_thread_roots(thread, &mut snapshot);
    // The JIT's stashed deopt / exceptional frames (gc-common w2-g,
    // 2026-09-23). They live in `jit/` thread-locals no peer collector can
    // reach, so this deposit is the only view a cross-thread collection has of
    // them while this thread is parked at the safepoint -- and a callee-handler
    // path that can reach a blocking point with a frame stashed reaches an
    // interpreter safepoint poll in the same window
    // (`docs/jit/deopt-thread-local-roots.md`). The blocked deposit
    // (`deposit_root_snapshot_inner`) has always done this; the initiator does
    // it in `collect_roots` §10. The remap half is in
    // `apply_pointer_map_to_thread`. Two thread-local probes and a `RefCell`
    // borrow when both stashes are empty, which is the normal case.
    cratonvm_jit::deopt::for_each_stashed_deopt_object(|addr| {
        if let Some(obj) = shared.mem.heap.is_object_address(addr as usize) {
            snapshot.push(obj);
        }
    });
    // JNI local references (INT-5, safepoint half): a JNI native that
    // obtained local refs and re-entered Java parks HERE — and a
    // cross-thread collector marks this thread only from this snapshot, so
    // an object reachable solely through this thread's `JNI_LOCAL_FRAMES`
    // was reclaimed. Thread-local storage; this deposit always runs on the
    // owning thread. The resume-side remap is `update_local_refs_after_gc`
    // in `apply_pointer_map_to_thread`.
    crate::native::jni::collect_local_ref_roots(&mut snapshot);

    // COLLECTOR FIRST. `moving_young_enabled()` ANDs a JIT-side gate with
    // `flags().gc.moving_young` and consults the collector in neither, so
    // under G1 and ZGC this whole question is inert — and answering it is
    // not free: `refresh_moving_young_coverage_for_current_thread` ends in
    // an UNMEMOISED `native_stack_has_jit_frame` over the full band, on
    // every blocked-region entry.
    //
    // Measured on `DefaultCatalogAndSchemaTest` under the default (ZGC)
    // collector, 240 s: the probe ran 1,139,842 times reading 35.2 BILLION
    // stack words. With the moving-young term forced off
    // (`CRATONVM_NO_MOVING_YOUNG=1`) it ran 494,968 times reading 15.3
    // billion — exactly one probe per blocked deposit instead of two, and
    // 20 billion fewer words, for a decision no non-moving collector reads.
    // gc-common w21-e (`handoff-w21e-bind-the-safepoint-root-deposit`): the
    // coverage proof, the conservative JIT scan and the pin publication below
    // write this VM's moving-young coverage rows (`gc_quiescence::CoverageCycle`).
    // Bind THIS VM's pause ledger for them, as `deposit_root_snapshot_inner`
    // does. A parking peer has no binding of its own, so without this scope its
    // writes land in the process's orphan rows, which every VM reads. The
    // initiator's call (after its request won) is already bound to the same
    // ledger, and the scope restores that binding when it drops.
    let ledger_scope =
        cratonvm_gc::gc_quiescence::scoped_pause_ledger(shared.mem.gc_barrier.pause_ledger());
    // (The JIT scans below read this VM's type maps when the verifier oracle
    // is armed; `bind` is a cached-bool no-op otherwise.)
    let _oracle_maps =
        crate::jit::conservative_roots::OracleTypeMapsScope::bind(&shared.classes.type_maps);
    let moving_young_precise_only = shared.mem.heap.is_generational()
        && crate::jit::conservative_roots::moving_young_enabled()
        && crate::jit::conservative_roots::refresh_moving_young_coverage_for_current_thread()
        && !cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete();

    // Cross-thread JIT-root hardening: also publish the conservative roots of
    // every active JIT frame on THIS thread into the snapshot.
    //
    // The cross-thread STW collector reads each thread's `root_snapshot` (via
    // `collect_all_root_snapshots`) — it does NOT call `collect_roots` for a
    // non-current thread (that path's `scan_active_jit_frames` is thread-local
    // and would scan the *collector's* empty JIT chain, not the parked
    // worker's). So an object whose only live reference lives in a *parked*
    // worker thread's JIT spill slot was absent from the snapshot the collector
    // marks from — a latent reclamation risk for the non-moving young-gen sweep
    // (relocation is already prevented: the collector runs non-moving while any
    // thread is in JIT). `update_root_snapshot` always runs on the thread it is
    // snapshotting, so the thread-local scan captures exactly that worker's live
    // JIT spill region; empty (no-op) when not in JIT; false positives filtered
    // by `is_object_address`.
    //
    // NOTE: this closes a real latent gap but does NOT fix the avrora real-RAF
    // SEGV — that crash was experimentally shown to be NOT a GC reclamation /
    // relocation bug (it reproduces with the young sweep capturing these JIT
    // roots and with the concurrent old-gen collector disabled). See
    // real-raf-segv-root-cause.md.
    if !moving_young_precise_only {
        // `update_root_snapshot` is also called at ordinary native-call
        // boundaries, not only immediately before a safepoint.  Its JIT-root
        // contribution is therefore a future cross-thread collector's only
        // view of this thread while it is parked.  Do not let the per-thread
        // scan cache republish a scan taken before the interpreted/native
        // callee below the JIT frame allocated a new live object: none of the
        // cache's keys change for that mutation.  The next collector could
        // otherwise reclaim the omitted object and hand a zero-header slot
        // back to compiled code.  This mirrors the safepoint and blocked
        // snapshot paths, both of which already invalidate before publishing.
        crate::jit::conservative_roots::invalidate_scan_cache_for_gc();
        let jit_scan_start = snapshot.len();
        crate::memory::native_roots::rootprof::note_scan_caller(1); // safepoint
        // r12w1 g1store: the pins owed by band words the G1 root screen rejects.
        let reject_capture =
            crate::jit::conservative_roots::g1_band_reject_pins::Capture::arm_if_wanted(
                &shared.mem.heap,
                false,
            );
        crate::jit::conservative_roots::scan_active_jit_frames(&shared.mem.heap, &mut snapshot);
        let reject_pins = reject_capture.map(|c| c.finish()).unwrap_or_default();
        // G1 pin-in-place, cross-thread half: the snapshot keeps these
        // conservatively-discovered objects ALIVE, but under G1 (a moving
        // collector) their regions must also be EXCLUDED from the collection
        // set — the JIT register/spill slots holding them cannot be
        // rewritten when the object moves. The initiator only publishes its
        // OWN JIT roots (roots.rs); every parked/blocked mutator must
        // publish here, into the process-global per-thread pin registry
        // consumed by `G1Collector::jit_pinned_region_set`. Replace
        // semantics: a deposit with no live JIT frames clears this thread's
        // stale pins.
        // 2026-09-06: NOT `is_g1()` any more. See `g1_only_jit_pins` for the
        // two collectors this gate had silently stopped covering and for the
        // measurement.
        if !g1_only_jit_pins() || shared.mem.heap.is_g1() {
            let mut addrs: Vec<usize> = snapshot[jit_scan_start..]
                .iter()
                .map(|r| r.as_ptr() as usize)
                .collect();
            crate::jit::conservative_roots::g1_band_reject_pins::extend_published(
                &reject_pins,
                &mut addrs,
            );
            cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&addrs);
        }
    } else if !g1_only_jit_pins() || shared.mem.heap.is_g1() {
        // Precise-relocation mode covers every JIT oop with rewritable
        // shadow-stack slots — no conservative pins needed; drop stale ones.
        cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&[]);
    }
    drop(ledger_scope);

    // §4 (multi-thread shadow scan, marking half). Also publish THIS thread's
    // shadow-stack precise roots into the snapshot. With `CRATONVM_SHADOW_STACK`
    // the moving collector is allowed to run while threads are in JIT, so a
    // cross-thread STW cycle (which marks from each parked thread's
    // `root_snapshot`, never `collect_roots`) must see a parked worker's
    // shadow-held oops or they are reclaimed. Mirrors the current-thread fold-in
    // in `roots.rs`; every slot is an oop by construction (re-validated via
    // `is_object_address`). The matching remap is in `apply_pointer_map_to_thread`.
    // No-op when the gate is off or the shadow stack is empty/unallocated.
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.for_each_value(|v| {
            if let Some(obj_ref) = shared.mem.heap.is_object_address(v) {
                snapshot.push(obj_ref);
            }
        });
    }
    if let Some((t0, nframes)) = _rs_t0 {
        use std::sync::atomic::Ordering::Relaxed;
        drop(snapshot); // release the lock before the (rare) print
        let calls = ROOTSNAP_CALLS.fetch_add(1, Relaxed) + 1;
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        ROOTSNAP_NANOS.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        ROOTSNAP_FRAMES.fetch_add(nframes as u64, Relaxed);
        if calls == 1 || calls % rootsnap_dbg_every() == 0 {
            let nanos = ROOTSNAP_NANOS.load(Relaxed);
            let frames = ROOTSNAP_FRAMES.load(Relaxed);
            eprintln!(
                "[ROOTSNAP] calls={} total_ms={} avg_us={:.2} avg_frames={:.1} cached_calls={} reused_frames={} reused_roots={}",
                calls,
                nanos / 1_000_000,
                // Cast: numeric/representation conversion
                (nanos as f64 / calls as f64) / 1000.0,
                // Cast: numeric/representation conversion
                frames as f64 / calls as f64,
                ROOTSNAP_CACHED_CALLS.load(Relaxed),
                ROOTSNAP_REUSED_FRAMES.load(Relaxed),
                ROOTSNAP_REUSED_ROOTS.load(Relaxed),
            );
        }
    }
}

#[cfg(test)]
mod root_snapshot_cache_tests;

#[cfg(test)]
mod safepoint_remap_tests;

/// Check if a stop-the-world pause is requested and participate if so.
///
/// Called at safepoints: allocation sites and backward branches (loop iterations).
/// If STW is active, this thread deposits its roots and waits for GC to complete,
/// then applies the pointer map to update its own frame references.
pub(crate) fn safepoint_check(shared: &SharedVm, thread: &mut JvmThread) {
    use std::sync::atomic::Ordering;
    // Execution profiler, off unless `CRATONVM_PROFILE_SAMPLE_MS` is set: one
    // relaxed load of a latched `Option<u64>` on the default path. Placed at
    // the TOP so a sample reflects the frame that was running, not whatever
    // the safepoint machinery below leaves on the stack. See
    // `runtime::exec_sampler` for the two biases this sampling point carries.
    crate::runtime::exec_sampler::maybe_sample(thread);
    // Every thread passes here at a GC pause, including one that has stopped
    // reaching the tier-up strides that also call this. One load and a compare
    // unless something was withdrawn since this thread last looked.
    super::jit_bridge::release_withdrawn_code_owners(thread);
    // Same shape for a class redefinition: this thread's frames that run a
    // body it replaced move onto their translated copies (interpreter round
    // i1 wave 19, lane L3; `obsolete_frames::after_redefinition` takes the
    // pause that brings every running thread here).
    super::obsolete_frames::convert_obsolete_frames_if_redefined(shared, thread);
    // CRATONVM_DBG_BLOCKED_ACCESS: reaching an interpreter safepoint with the
    // thread's own `in_blocked_region` flag still raised means every STW
    // census is excluding a RUNNING mutator — a moving GC can complete under
    // its feet, and nothing ever applies its accumulated blocked-fixup. This
    // catches stuck flags from any raise site whose wake/error path skipped
    // `check_post_block_gc` (the monitor_wait early-return bug class). No-op
    // when the gate is off.
    if cratonvm_gc::blocked_access_debug::enabled()
        && thread
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
    {
        cratonvm_gc::blocked_access_debug::report_blocked_violation(
            "interpreter safepoint reached with in_blocked_region raised",
            0,
        );
    }
    // bc math-ec 0x4 (CRATONVM_DBG_MEMWATCH): O(1) poll of one absolute
    // watched address at full safepoint frequency — catches the corrupting
    // write within one safepoint window, with the live Java stack. Off ⇒
    // a single predicted branch. On a HIT the dump includes the TOP frame's
    // locals (raw bits + array header/extent for in-heap-shaped values) so
    // the watched address can be placed inside/outside the frame's array
    // receivers (legit-write-to-reused-slot vs stale/OOB receiver).
    crate::runtime::memwatch::poll("safepoint", || {
        let mut s = thread
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
            .join("\n");
        if let Some(top) = thread.frames.last() {
            s.push_str("\n  -- top-frame locals --");
            let n = top.locals_len().min(10);
            for i in 0..n {
                let raw = top.get_local_raw(i);
                // Widening: small integer index -> usize (non-negative, fits in pointer width)
                let p = (raw & 0x0000_7fff_ffff_ffff) as usize;
                let mut extra = String::new();
                if p != 0 && p % 8 == 0 && shared.mem.heap.is_heap_addr(p).is_some() {
                    // SAFETY: in-heap address; first 16 header bytes of managed
                    // memory are always readable (raw bytes, not enum fields).
                    let (kind_b, elem_b, alen) = unsafe {
                        let q = p as *const u8;
                        (
                            cratonvm_types::kind_tag_at(q),
                            cratonvm_types::element_type_tag_at(q),
                            // Cast: reinterpret pointer/address to typed pointer
                            (q.add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *const u32)
                                .read_unaligned(),
                        )
                    };
                    extra = format!(
                        " [heap obj kind={kind_b} elem={elem_b} len={alen} data=0x{:x}..0x{:x}]",
                        p + 40,
                        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                        p + 40 + (alen as usize) * 8,
                    );
                }
                s.push_str(&format!("\n  local[{i}] = 0x{raw:x}{extra}"));
            }
        }
        s
    });
    if shared.mem.gc_barrier.stw_requested.load(Ordering::Acquire) {
        // CRIT (TLAB UAF) — retire this thread's TLAB before parking for GC,
        // exactly as the GC initiator does in `maybe_gc`. The moving collector
        // run by the initiator can `grow()` (realloc) the young arena while we
        // are parked, freeing the old backing buffer; a TLAB that still held
        // `[cursor,end)` into that buffer would then be dangling, so the first
        // post-GC fast-path bump on this thread writes the object header into
        // freed/unmapped memory → EXCEPTION_ACCESS_VIOLATION in
        // `init_object_header` (deterministic once a grow frees the old buffer;
        // reproduces with JIT off because that path always uses the moving
        // collector). Retiring empties the TLAB so the next allocation refills
        // from the current arena, and installs a walkable filler over
        // `[cursor,end)` so the collector's from-space walk doesn't desync on
        // the unfilled tail (the same reason the initiator retires).
        flush_tlab_allocation_batch(thread, shared);
        thread.tlab.retire();
        // Round-5 fix (CRIT — UAF): drain this thread's per-thread SATB
        // buffer into the global queue BEFORE we park at the barrier.
        // The thread-local buffer holds up to 256 overwritten references;
        // without an explicit flush they sit invisible to the marker and
        // the next evacuation cycle turns the SATB lost-object scenario
        // into a use-after-free. Must run on every mutator at every
        // safepoint arrival — `flush_thread_satb` short-circuits cheaply
        // on `is_active() == false` when concurrent marking is idle.
        shared.mem.heap.flush_thread_satb();

        // Update root snapshot before pausing. This snapshot is the ONLY view a
        // cross-thread STW collector has of this (parked) thread's roots, so the
        // JIT-frame portion must be a fresh scan, not the possibly-stale cached
        // one (see `invalidate_scan_cache_for_gc`): the conservative scan covers
        // the native stack below this thread's JIT frames, which mutated since
        // the last boundary bump, and a dropped live root would be reclaimed by
        // the collector this thread is about to park for.
        crate::jit::conservative_roots::invalidate_scan_cache_for_gc();
        update_root_snapshot(shared, thread);
        if remap_trace_on() {
            let snap = thread.root_snapshot.lock().clone();
            deposit_gap_diff(thread, &snap, "publish");
        }
        // Publish this thread's LIVE call stack, when — and only when — some
        // other thread is at this moment taking a cross-thread
        // `Thread.getStackTrace()` / `dumpThreads()` (see
        // [`stw_publish_frame_traces`]).
        //
        // `frame_trace` is otherwise written at the BLOCKING deposit points
        // only, which is the right place for a parked thread (it shows the
        // blocking call site) and useless for a running one: a thread that has
        // never blocked publishes nothing, and one that has publishes where it
        // blocked LAST. That is what made cross-thread `getStackTrace()` return
        // an empty array for any thread actually executing Java code.
        //
        // Gated on the request flag rather than unconditional: this runs inside
        // every safepoint park, i.e. on every mutator on every GC pause, and the
        // capture allocates a `Vec` per thread. A thread dump is rare; a GC
        // pause is not. Unset, this is one relaxed load.
        if frame_trace_wanted() {
            // With the compiled activations (wave 38, lane L7).
            // With the reflective calls' JDK frames (wave 45, lane L3).
            let trace = crate::runtime::stackwalker::capture_published_trace(
                shared,
                &thread.frames,
                &thread.reflective_calls,
                &thread.reflective_frames_named,
            );
            let trace_len = trace.entries.len();
            *thread.frame_trace.lock() = trace.entries;
            // With the trace, the frame that locked each held monitor
            // (`ThreadInfo.getLockedMonitors()`, wave 24, lane L7).
            shared
                .threads
                .thread_registry
                .publish_jmx_frame_monitors(
                    thread.thread_id,
                    &thread.frames,
                    &thread.jmx_frame_monitors_published,
                    trace.frame_positions.as_deref().map(|p| (p, trace_len)),
                );
        }

        // Cross-thread JIT coverage handshake, PEER half. This thread is about
        // to park; the initiator is about to ask whether every live compiled
        // frame in the process is rewritable, and this thread's chain is the
        // part of that question only this thread can answer (`JIT_ENTRY_CHAIN`,
        // the cached top RBP and the shadow window are all thread-local).
        //
        // It belongs HERE and not in `update_root_snapshot` above, even though
        // that function already computes the same proof for the generational
        // collector: `update_root_snapshot` is also the blocked-region deposit
        // path, which runs on every blocking native call, and its own comment
        // records the measurement that made the proof generational-only there
        // (1.1 M probes reading 35 billion stack words on one 240 s run). A
        // safepoint park happens once per thread per pause, so the same proof
        // costs nothing here — and this is the only site where the deposit's
        // promise holds, because a thread that reaches this line resumes
        // through `apply_pointer_map_to_thread` and remaps its own frames.
        //
        // Into THIS VM's ledger (gc-common w8-d): the deposit is bound to the
        // barrier this thread is about to arrive at, so it can only ever be
        // credited to this VM's pause -- never to another VM's initiator
        // (`cratonvm_gc::gc_quiescence::PauseLedger`).
        let oracle_maps =
            crate::jit::conservative_roots::OracleTypeMapsScope::bind(&shared.classes.type_maps);
        cratonvm_gc::gc_quiescence::with_pause_ledger(shared.mem.gc_barrier.pause_ledger(), || {
            crate::jit::conservative_roots::publish_peer_jit_coverage_for_stw();
        });
        drop(oracle_maps);
        // Arrive at barrier and wait for GC to complete. Census-aware (auto):
        // a genuine safepoint arrival is normally counted, but if this pause's
        // census excluded us as blocked (a finding-1(a) window), participating
        // would fill a counted mutator's quota slot.
        //
        // The map comes back as a shared `Arc` (gc-common 2026-09-23): one
        // allocation per pause, reference-counted to every waiter, instead of a
        // deep copy per thread taken under the barrier lock.
        let pointer_map = shared.mem.gc_barrier.arrive_and_wait_auto(thread.thread_id);

        // `remap_trace_push` returns at once when the trace is off, but its
        // `extra` argument was formatted unconditionally — a `String`
        // allocation (and a 16-shard `len()` walk) on every mutator at every
        // pause. Only build it when the trace will read it.
        if remap_trace_on() {
            remap_trace_push(
                shared,
                thread,
                if pointer_map.is_empty() {
                    "arrive-nomap"
                } else {
                    "arrive"
                },
                &format!("map={}", pointer_map.len()),
            );
        }
        // Apply pointer map to this thread's frames
        if !pointer_map.is_empty() {
            apply_pointer_map_to_thread(
                thread,
                &pointer_map,
                &shared.mem.heap,
                &shared.classes.type_maps,
            );
        } else {
            // Nothing moved: keep (re-tag) the frozen-frame root cache, exactly
            // as the initiator's `update_all_roots` does before its empty-map
            // early return (gc-common w2-g, 2026-09-23). Without this every
            // parked peer rebuilt its cache from scratch at its next deposit
            // after EVERY non-moving pause -- ZGC's sweep fallback, the
            // Generational non-moving young sweep, every concurrent-mark STW --
            // for nothing: the cache was built by the deposit just above, every
            // cached root was in the snapshot the collector marked from, and
            // no frame was rewritten, so the cache still mirrors the frames.
            // With an empty map `remap_rs_cache_after_gc` skips the walk and
            // only re-tags; it still clears the cache under
            // `CRATONVM_ROOTSNAP_CACHE(_SURVIVE_GC)=0`.
            remap_rs_cache_after_gc(thread, &pointer_map, &shared.mem.heap);
        }
        mtroots_selfcheck(thread, &shared.mem.heap, "safepoint-resume");
    }
    // (The T1.5.1 async-exception drain that ended this function, and
    // `check_pending_async_exception`, were removed in interpreter round i1
    // wave 4 with the rest of that channel: nothing posted to it and nothing
    // raised what it drained. See
    // docs/internal/fixed-bugs/interpreter-L1-async-exception-channel-is-dead-FIXED-20260923.md.)
}

/// Apply a GC pointer map to a thread's frame locals and operand stacks.
///
/// `maps` is the VM's verifier type-map store (`shared.classes.type_maps`); it
/// only feeds the `CRATONVM_DBG_VERIFY_OOP_MAPS` shadow comparison of the frame
/// remap (`Frame::update_frame_refs_mapped`) and changes no rewrite.
pub(crate) fn apply_pointer_map_to_thread(
    thread: &mut JvmThread,
    pointer_map: &cratonvm_types::PointerMap,
    heap: &crate::memory::VmHeap,
    maps: &cratonvm_classloading::TypeMapStore,
) {
    // Attribution for a later stale-reference report: this thread applied a
    // relocation map through the STOP-THE-WORLD RESUME path. See
    // `gc_quiescence::note_pointer_map_applied`.
    if !pointer_map.is_empty() {
        cratonvm_gc::gc_quiescence::note_pointer_map_applied(1);
    }
    // JNI local references (INT-2, safepoint-resume half): rewrite THIS
    // thread's `JNI_LOCAL_FRAMES` handles through the pointer map — a JNI
    // native that re-entered Java and parked at the safepoint poll must not
    // resume with dangling local jobjects after a moving collection. The
    // storage is thread-local and this function always runs on the resuming
    // thread, so this is the only place that can reach these handles.
    crate::native::jni::update_local_refs_after_gc(pointer_map);
    // BUG-03 trace (gated): record that the safepoint-peer remap ran for main.
    if thread.thread_id.0 == 0 && peer_bug03_dbg() {
        let jto = thread
            .java_thread_obj
            .map(|o| o.as_ptr() as usize)
            .unwrap_or(0);
        eprintln!(
            "[BUG03-fm] e{} path=peer(apply_pointer_map) tid0 jto=0x{:x} jto_in_map={} pm.len={}",
            heap.collection_count(),
            jto,
            jto != 0 && pointer_map.contains_key(&jto),
            pointer_map.len()
        );
    }
    // See `JvmThread::last_heal_collection`.
    thread.last_heal_collection = heap.collection_count();
    for frame in &mut thread.frames {
        frame.update_frame_refs_mapped(pointer_map, heap, maps);
        // Forward the synchronized-method monitor object too. A `synchronized`
        // method records the object it locked on entry in `monitor_on_exit` and
        // releases it on frame-pop. If a *cross-thread* moving GC relocated that
        // object while this thread was parked at the STW safepoint barrier
        // (`arrive_and_wait` in the safepoint-poll path), a stale
        // `monitor_on_exit` makes the implicit `monitorexit` target the old
        // address — surfacing as "thread does not own the monitor" (observed as
        // an intermittent IllegalMonitorStateException in the ES RestClient
        // `org/elasticsearch/client/Cancellable$RequestCancellable.
        // runIfNotCancelled`, whose `synchronized` body allocates heavily under
        // `-Xmx1g` GC pressure). The GC-initiator (`update_all_roots` in
        // memory/gc.rs) and the native-blocked-thread wake path
        // (`check_post_block_gc` in vm/vm_exec.rs) already forward it; this
        // non-initiator safepoint-resume path was the missing third site. Keep
        // it consistent with the relocated object, identically to those two.
        if let Some(ref mut obj_ref) = frame.monitor_on_exit {
            // Cast: object/code pointer to integer address
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        // Block monitors (`Frame::held_monitors`, wave 23 lane L7), for the
        // same reason: `monitorexit` finds its entry by address.
        frame
            .held_monitors
            .remap_with(|a| pointer_map.get(&a).copied());
    }
    // DIAGNOSTIC-ONLY (cceres3): mirror of the wake-time WAKE-STALE verifier;
    // catches a frame slot left stale right after a safepoint-arrival remap.
    //
    // THE PREDICATE IS THE POINTER MAP, NOT THE FORWARDING WORD (2026-08-17).
    // `debug_forwarded_target` reads a forwarding word at the old address, and
    // ZGC's slide leaves none — `compact_low_to` zeroes what it vacated and the
    // memmove overwrites the rest — so on the DEFAULT collector this verifier
    // reported zero whatever the truth was, which is how it stayed silent while
    // the H2 MVStore-writer residual reproduced under it. `pointer_map` is the
    // authoritative record of this collection's moves and is right here in
    // hand.
    //
    // This is also the only EXACT place to ask the question. Every address-keyed
    // instrument outside the pause cannot tell an old reference to the moved
    // object from a new reference to whatever the allocator has since put at
    // that address; here, the remap has just run and no mutator on this thread
    // has resumed, so a frame slot holding a map KEY is unambiguously a slot the
    // remap did not reach.
    if peer_blockgc_dbg() {
        // A source address this slide also wrote a SURVIVOR to is not evidence:
        // survivors slide down into the space vacated objects left, so a slot
        // legitimately holding that survivor names an address that is also a
        // map key. Excluding destinations is what separates "the remap missed
        // this slot" from "this slot holds the object that moved INTO the
        // address" — the same distinction that made the first vacated-frames
        // instrument report eight findings a run that were all correct code.
        let destinations: rustc_hash::FxHashSet<usize> = pointer_map.values().copied().collect();
        for (fi, fr) in thread.frames.iter().enumerate() {
            for li in 0..fr.locals_len() {
                if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                    let a = o.as_ptr() as usize;
                    if destinations.contains(&a) {
                        continue;
                    }
                    if let Some(new) = pointer_map.get(&a).copied() {
                        eprintln!(
                            "[blockgc] ARRIVE-STALE tid={} frame#{fi} {}.{} pc={} local[{li}] 0x{a:x}->0x{new:x} in_map={}",
                            thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            pointer_map.contains_key(&a),
                        );
                    }
                }
            }
            for si in 0..fr.stack.len() {
                if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                    let a = o.as_ptr() as usize;
                    if destinations.contains(&a) {
                        continue;
                    }
                    if let Some(new) = pointer_map.get(&a).copied() {
                        eprintln!(
                            "[blockgc] ARRIVE-STALE tid={} frame#{fi} {}.{} pc={} stack[{si}] 0x{a:x}->0x{new:x} in_map={}",
                            thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            pointer_map.contains_key(&a),
                        );
                    }
                }
            }
        }
    }
    // Step 5 GAP B (precise-JIT remap, non-initiator half). The GC initiator's
    // `update_all_roots` (memory/gc.rs:73) remaps this collection's precise JIT
    // oop-map slots via `remap_active_jit_frames`, but a thread that was PARKED at
    // the STW barrier reaches HERE instead and was missing that call. Under a
    // moving collector this stranded a non-initiator's JIT-frame oops at their old
    // addresses after a relocation — a use-after-free with CRATONVM_PRECISE_JIT_MAPS
    // on. It is specifically a G1 hazard: G1 young/mixed move unconditionally,
    // whereas the generational collector falls back to a non-moving sweep whenever
    // any thread is in JIT (`gc_quiescence`), so its non-initiator JIT frames never
    // see relocation. Mirror the initiator: remap THIS resuming thread's precise
    // JIT oop slots before the shadow stack. Thread-local (walks this thread's JIT
    // entry chain — sound because we run on the resuming thread itself) and inert
    // unless a precise-map frame is live, so it is a no-op on the default path.
    crate::jit::conservative_roots::remap_active_jit_frames(
        pointer_map,
        &crate::jit::conservative_roots::shadow_owned_slots(&thread.shadow_stack),
    );
    crate::jit::conservative_roots::remap_register_image_words(pointer_map, None);
    crate::jit::conservative_roots::report_stale_after_remap(pointer_map, None);
    // §4 (multi-thread shadow scan, remap half). Remap THIS thread's shadow-stack
    // precise roots in place, so a worker resuming from the STW barrier sees the
    // relocated addresses in the JIT registers/slots it reloads from its shadow
    // stack. (The GC initiator's own shadow stack is remapped by `update_all_roots`
    // in `gc.rs`; a non-initiator reaches here instead.) Every shadow slot is a
    // known oop, so the rewrite is unconditionally safe. No-op when the gate is
    // off or the shadow stack is empty. `remap` (like `shadow_owned_slots`
    // above) bounds indirect entries by THIS thread's live stack, so an odd
    // primitive a compiled frame published in a reference home is skipped,
    // never written through (gc-common w5-g, `ShadowOddLongProbe`) -- which is
    // why this must stay on the resuming thread itself.
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.remap(pointer_map);
    }
    // Every off-frame per-thread reference, through the ONE list the four
    // per-thread remap paths share (`memory::gc::remap_thread_off_frame_refs`):
    // printed values, native pins, handle slots, the native allocation pool,
    // the in-flight native return, the JIT HashMap node cache, the ASCII case
    // cache, scoped-value keys and values, and the three single-slot references
    // (`java_thread_obj`, `jit_pending_exception`, `uncaught_exception_pending`).
    //
    // This path used to carry its own copy of that list and it had drifted:
    // `jit_pending_exception` and `uncaught_exception_pending` were forwarded
    // by the initiator only, so a peer-initiated relocation left the JIT's
    // pending throwable (live across the unwind to the interpreter's drain,
    // which crosses safepoint polls) naming a vacated address.
    crate::memory::gc::remap_thread_off_frame_refs(thread, pointer_map);
    // The JIT deopt / exceptional stash, remap half (gc-common w2-g,
    // 2026-09-23). Paired with the scan half `update_root_snapshot` now runs
    // on every safepoint deposit, which is what `remap_stashed_deopt_objects`'
    // debug assertion checks. Thread-local, and this function always runs on
    // the thread whose stash it is. Before the pairing a stashed frame parked
    // at a safepoint was neither rooted nor forwarded by a peer's collection.
    cratonvm_jit::deopt::remap_stashed_deopt_objects(|addr| {
        pointer_map.get(&(addr as usize)).map(|&to| to as u64)
    });
    // This thread's deposited root snapshot, in lockstep with the frames it
    // describes. Without it the snapshot keeps the PRE-move addresses until the
    // next deposit, and the one reader that can run before that deposit -- the
    // cross-thread takeover freezing this thread in compiled code at the NEXT
    // pause (`collect_all_root_snapshots`, `root_snapshots_for_os_tids`) --
    // hands the collector addresses this cycle vacated. The initiator and
    // blocked threads already had their snapshots forwarded (`update_all_roots`
    // section 11, `fold_pointer_map_into_blocked`); a parked peer was the gap.
    crate::memory::gc::remap_root_snapshot(thread, pointer_map);
    // Lever #3 (bug 04): keep this thread's rootsnap frozen-frame cache valid
    // across the relocation we just applied, in lockstep with the frames above.
    remap_rs_cache_after_gc(thread, pointer_map, heap);

    // DIAG (CRATONVM_GC_VERIFY_STALE=1): after a NON-INITIATOR thread resumes
    // from the STW barrier and applies the pointer map, walk its own frames and
    // flag any Object slot whose header is ZEROED (class_id=0 && num_slots=0).
    // A zeroed header means the object was NOT copied by the collector (a missed
    // marking root — its ref was absent from this thread's deposited snapshot,
    // e.g. a lost operand-stack tag), then young-from was reset over it. This is
    // the per-PARKED-THREAD analogue of `verify_no_stale_refs` (which only
    // checks the GC initiator) — it localizes the missed-root that surfaces as
    // the teardown "all-zero header" corruption / reactor-thread leak.
    // Cache the gate so the OFF path (the default) is a single relaxed load, not
    // a per-parked-thread-per-GC environment lookup.
    fn gc_verify_stale_enabled() -> bool {
        use std::sync::OnceLock;
        static E: OnceLock<bool> = OnceLock::new();
        *E.get_or_init(|| {
            cratonvm_types::flags::runtime_var("CRATONVM_GC_VERIFY_STALE")
                .ok()
                .as_deref()
                == Some("1")
        })
    }
    if gc_verify_stale_enabled() {
        use cratonvm_types::ObjectHeader;
        let tname = thread.thread_id.0;
        for (fi, frame) in thread.frames.iter().enumerate() {
            let cn = frame.class_name();
            let mn = frame.method_name();
            for li in 0..frame.locals_len() {
                // Cast: numeric/representation conversion
                if let crate::types::Value::Object(Some(o)) = frame.get_local(li as u16) {
                    // Cast: object/code pointer to integer address
                    let a = o.as_ptr() as usize;
                    if a != 0 {
                        // SAFETY: `a` is a non-null heap address from a live Object local; reading its ObjectHeader is valid for the lifetime of the borrow.
                        let h = unsafe { &*(a as *const ObjectHeader) };
                        if h.class_id.as_u32() == 0 && h.num_slots() == 0 && h.array_length() == 0 {
                            eprintln!(
                                "POST-GC ZERO-HEADER PARKED tid={} frame[{}] {}.{} local[{}] pc={} addr=0x{:x}",
                                tname, fi, cn, mn, li, frame.pc, a
                            );
                        }
                    }
                }
            }
            let mut stk = Vec::new();
            frame.stack.scan_object_refs(&mut stk, heap);
            for o in stk {
                // Cast: object/code pointer to integer address
                let a = o.as_ptr() as usize;
                if a != 0 {
                    // SAFETY: `a` is a non-null heap address from a live Object stack slot; reading its ObjectHeader is valid for the lifetime of the borrow.
                    let h = unsafe { &*(a as *const ObjectHeader) };
                    if h.class_id.as_u32() == 0 && h.num_slots() == 0 && h.array_length() == 0 {
                        eprintln!(
                            "POST-GC ZERO-HEADER PARKED-STACK tid={} frame[{}] {}.{} pc={} addr=0x{:x}",
                            tname, fi, cn, mn, frame.pc, a
                        );
                    }
                }
            }
        }
    }
}

/// Keep the `update_root_snapshot` frozen-frame cache (`rs_cache`) valid across
/// a GC instead of letting the next snapshot discard it on the
/// `collection_count` bump. The cache stores object ADDRESSES, which a
/// collection only invalidates by RELOCATING the object (the default non-moving
/// young sweep still relocates via selective promotion). So remap the cached
/// roots through the same `pointer_map` that relocated the frames, then tag the
/// cache with the post-collection count so `update_root_snapshot`'s gen gate
/// accepts it.
///
/// FAIL-SAFE: `rs_cache_gen` is advanced ONLY here. Any GC path that relocates
/// this thread's objects WITHOUT calling this leaves `rs_cache_gen` stale, so
/// the gen gate rebuilds the cache from scratch; a stale cached address is
/// never trusted. Default-on with opt-outs (`CRATONVM_ROOTSNAP_CACHE=0` or
/// `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC=0`). Must be called at every site that
/// applies a `pointer_map` to a thread's frames (`apply_pointer_map_to_thread`
/// here, `update_all_roots` in memory/gc.rs); missing one only costs a rebuild,
/// never correctness. (Until 2026-07-31 this also cleared the cache outright in
/// the "real ForkJoinPool lane", keyed off a presence test on
/// `CRATONVM_REAL_FORKJOINPOOL` that stopped being set when that lane became
/// the default — see `env_cache.rs` for why that bypass was retired instead of
/// repointed.)
pub(crate) fn remap_rs_cache_after_gc(
    thread: &mut JvmThread,
    pointer_map: &cratonvm_types::PointerMap,
    heap: &crate::memory::VmHeap,
) {
    if !crate::runtime::env_cache::rootsnap_cache()
        || !crate::runtime::env_cache::rootsnap_cache_survive_gc()
    {
        thread.rs_cache.clear();
        return;
    }
    // An empty map means nothing moved → cached addresses are already valid;
    // skip the walk but still re-tag the gen below so the cache is kept.
    if !pointer_map.is_empty() {
        for (_key, roots) in thread.rs_cache.iter_mut() {
            for r in roots.iter_mut() {
                // Cast: object/code pointer to integer address
                if let Some(&new_addr) = pointer_map.get(&(r.as_ptr() as usize)) {
                    // SAFETY: new_addr came from the GC's pointer_map and points
                    // at the relocated object's header within the heap arena —
                    // the same invariant the frame/snapshot remaps above rely on.
                    *r = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }
    thread.rs_cache_gen = heap.collection_count();
}

/// Check if the old generation needs a concurrent GC cycle.
///
/// If the old gen is above its capacity threshold, this starts a concurrent
/// mark-sweep cycle using brief STW pauses for initial mark and remark,
/// with the marking phase running concurrently with application threads.
/// LANE W7-D — the ONE implementation of "advance G1's mark-cycle lifecycle",
/// with the door that asked.
///
/// # Why this is one function and was three
///
/// `maybe_concurrent_gc`'s G1 arm, `g1_drive_concurrent_mark_pub` and (in a
/// loop, with a deadline) `g1_force_full_cycle` each carried their own copy of
/// the finish-then-start pair. The copies had already drifted:
/// `maybe_concurrent_gc` tested `g1_should_start_marking() &&
/// !g1_is_marking_active()` — a redundant second read of the flag it had
/// already branched on — while `g1_drive_concurrent_mark_pub` did not. Nothing
/// depended on the difference, which is exactly the shape that eventually
/// does.
///
/// More to the point: three copies is three places a FOURTH caller has to be
/// discovered, and the defect this lane closed was a missing caller, not a
/// wrong one. With one implementation, a new pause path needs one line, and
/// the census below cannot disagree with what the code did because it is
/// written from the same branch.
///
/// # The outcome taxonomy is the diagnosis
///
/// Four terminal states, and `waiting` is the one no counter in the tree could
/// report before:
///
/// * `Idle` — no cycle, and the IHOP policy declined to open one.
/// * `Started` — no cycle, and this visit opened one.
/// * `Waiting` — a cycle IS open and the background marker has not drained.
/// * `Finished` / `LostStw` — a cycle is open, the marker has drained, and
///   the STW remark+cleanup either ran or was beaten to the barrier.
///
/// `remark_pauses=1, cleanup_pauses=0` on a default build is compatible with
/// `waiting` in the millions (a driver asking constantly and always refused)
/// and with `visits=0` (no driver on this pause path at all). They are
/// different defects. See
/// `docs/internal/g1-2026-09-20/w7d-the-cycle-that-no-pause-closes.md` §2.
///
/// Not a GC pause path in its own right: the caller has already completed its
/// collection and reopened the world, so the two `g1_*` predicate loads and at
/// most one census `fetch_add` are charged per PAUSE, never per allocation.
pub(super) fn g1_drive_mark_cycle(shared: &SharedVm, thread: &mut JvmThread, door: MarkDoor) {
    // Finish first, start second: if an active cycle's background marker
    // has drained to a fixed point, run the STW final remark + cleanup
    // NOW, on this thread (it has the STW-barrier context). The remark
    // is NOT optional — the SATB log and a fresh root scan must reach
    // the bitmap before cleanup acts on it (see
    // `g1_final_remark_and_cleanup`); the old completion path (a watcher
    // thread calling straight into cleanup) discarded both.
    if shared.mem.heap.g1_is_marking_active() {
        if shared.mem.heap.g1_concurrent_mark_finished() {
            let ran = g1_final_remark_cleanup(shared, thread);
            note_mark_door(
                door,
                if ran {
                    MarkDoorOutcome::Finished
                } else {
                    MarkDoorOutcome::LostStw
                },
            );
        } else {
            note_mark_door(door, MarkDoorOutcome::Waiting);
        }
        return;
    }
    if shared.mem.heap.g1_should_start_marking() {
        g1_concurrent_mark_cycle(shared, thread);
        // Asked-and-got, not asked. `g1_concurrent_mark_cycle` returns
        // without activating when its initial-mark STW loses the race, and a
        // census that recorded the ATTEMPT would report cycles that never
        // opened — the same "the instrument cannot see the failure" shape
        // README §5 rule 8 is about. One extra relaxed load, once per pause.
        note_mark_door(
            door,
            if shared.mem.heap.g1_is_marking_active() {
                MarkDoorOutcome::Started
            } else {
                MarkDoorOutcome::LostStw
            },
        );
    } else {
        note_mark_door(door, MarkDoorOutcome::Idle);
    }
}

pub(super) fn maybe_concurrent_gc(shared: &SharedVm, thread: &mut JvmThread) {
    maybe_concurrent_gc_at(shared, thread, MarkDoor::MaybeGc)
}

/// [`maybe_concurrent_gc`] with the door's identity, for the census.
///
/// `System.gc()` reaches this function through `force_gc_from_native`, and it
/// is the one flag-free route `w6m-a-workload-that-mixes.md` §4 measured
/// COMPLETING cycles on a default build (6 cycles, 9 mixed pauses). A census
/// in which it is indistinguishable from `maybe_gc`'s epilogue cannot be used
/// to check that account, which is the whole reason the door parameter exists.
pub(super) fn maybe_concurrent_gc_at(shared: &SharedVm, thread: &mut JvmThread, door: MarkDoor) {
    // G1 backend: trigger concurrent marking when IHOP threshold crossed
    if shared.mem.heap.is_g1() {
        g1_drive_mark_cycle(shared, thread, door);
        return;
    }

    // gen r4w2/concmark (2026-09-23): a cycle is already open (another
    // driver's, sliced and between two of its slices). The CAS below is the
    // authority; this read only saves building a marker — whose bitmap is
    // 1/64th of the old generation — that could not open anyway.
    if shared.mem.concurrent_gc_state.phase() != cratonvm_gc::ConcurrentGcPhase::Idle {
        return;
    }

    // Only proceed if old gen needs collection and we have a concurrent marker
    //
    // gen r4w4/concmark4 (2026-09-24): the concurrent cycle's OWN trigger,
    // `GenerationalHeap::concurrent_cycle_due` — the adaptive initiating
    // occupancy below the STW floor under the default concurrent-first
    // policy, and exactly the old `old_gen_needs_gc()` under
    // `CRATONVM_GC_NO_CONCURRENT_FIRST`. G1 returned above; ZGC never answered
    // yes here (`old_gen_needs_gc` is a hard `false` off Generational).
    //
    // gen r5w3/unload7 — the per-door start census
    // (`gengc-r5w2-conc6-proposal-generational-start-census-by-door`): which
    // door asked, which found the cycle due, which opened it (below, at the
    // initial mark). Generational only; two relaxed adds per collection that
    // asks.
    let census_door = gen_conc_door(shared, door);
    let due = match &shared.mem.heap {
        crate::memory::vm_heap::VmHeap::Generational(h) => {
            let state = &shared.mem.concurrent_gc_state;
            state.note_door(
                census_door,
                cratonvm_gc::concurrent_mark::GenConcDoorStep::Asked,
            );
            let due = h.concurrent_cycle_due();
            if due {
                state.note_door(census_door, cratonvm_gc::concurrent_mark::GenConcDoorStep::Due);
            }
            due
        }
        #[allow(unreachable_patterns)]
        _ => false,
    };
    if !due {
        return;
    }

    let (old_gen_base, old_gen_size) = shared.mem.heap.old_gen_info();

    // Create a temporary concurrent marker for this cycle.
    //
    // fork6 GC_STRESS fix — build it on the heap's SHARED SATB queue + phase
    // state (attached via `enable_concurrent_gc` at SharedVm construction).
    // The previous `ConcurrentMarker::new` created a private queue + state per
    // cycle while the heap's `satb_barrier` gated on the HEAP's (formerly
    // never-attached) instances: the write barrier was a hard no-op, nothing
    // ever reached this cycle's remark, and the concurrent mark effectively
    // ran against live mutators with no write barrier — the sweep then freed
    // old objects whose only reference moved during the concurrent phase.
    let marker = cratonvm_gc::ConcurrentMarker::with_shared(
        old_gen_base,
        old_gen_size,
        shared.mem.concurrent_satb.clone(),
        shared.mem.concurrent_gc_state.clone(),
    );

    // gen r4w2/concmark (2026-09-23) — own the cycle before touching the
    // shared SATB queue or phase. Two drivers used to be able to overlap on
    // them (the second opened between the first one's Phase 2 and its remark),
    // and whichever remarked first deactivated the queue under the other's
    // mark; see `ConcurrentGcState::try_open_cycle`. Every `return` below
    // drops `cycle`, which abandons the cycle (phase back to Idle, barrier
    // off); only the post-sweep `cycle.complete()` releases it without that.
    let Some(cycle) = marker.try_open_cycle() else {
        return;
    };

    // Phase 1: Initial Mark — brief STW pause.
    //
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) instead of a request plus a plain `wait_for_all` (the deleted
    // `brief_stw*` helpers), which stalls forever on a peer spinning in
    // compiled code — and whose root set covered such a peer only by its
    // STALE deposit snapshot. Mark-only pause: the frozen peers' fresh
    // conservative roots are extra MARK roots; nothing moves, so no
    // pin/pointer-map concerns. The request passes the identity census
    // (gc-common w4-a, `request_non_collection_pause`).
    let Some(counted_os_tids) = request_non_collection_pause(shared, thread.thread_id) else {
        return; // Another STW was in progress
    };
    // gen r5w1/refs5 — remark-time reference processing for this cycle, BOTH
    // halves or neither (`gengc-r4w5-concmark5-remark-reference-processing-design`):
    // the referent-slot skip set published in this initial-mark pause, and the
    // processing at remark (`gen_concurrent_remark_pause`). Decided ONCE per
    // cycle, here, so the two halves cannot disagree. `Some` is the skip set's
    // candidate list, which the remark's safety net re-reads.
    let refproc_on = gen_conc_remark_refproc_enabled();
    // gen r5w3/unload7 — concurrent class unloading, decided once per cycle
    // like the reference processing it runs inside (and only with it): the
    // initial mark and the remark take their roots under the class-unload
    // licence, the marker follows the side tables, and the remark unloads.
    // `false` (the default) is every line below byte for byte as before.
    let class_unload_on = refproc_on && gen_conc_class_unload_enabled();
    // gen r5w6/conc10 — `CRATONVM_GEN_Y2O_LIVE_SEED` (opt-in): both pauses of
    // this cycle take their young→old seeds from the LIVE young set, and, when
    // the cycle processes references at its remark, not through the referent
    // slot of a young `Reference` (`gen_conc_y2o_seeds`). Decided once per
    // cycle, like the two above. `false` is every line below as before.
    let y2o_live_seed = cratonvm_gc::gen_heap::y2o_live_seed_enabled();
    let mut hidden_refs: Option<Vec<usize>> = None;
    let mut initial_marked = false;
    {
        // Forcibly stop + conservatively scan in-JIT peers, then wait for
        // the cooperative mutators (byte-identical to wait_for_all() when
        // no thread is in JIT).
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        // gen r5w5/conc9: armed BEFORE the take-over, so an unwind anywhere in
        // this pause resumes the frozen peers and reopens the world (see
        // `GenConcPauseGuard`); the normal path below releases it.
        let mut pause_guard = GenConcPauseGuard::arm(shared);
        pause_guard.hold(stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids));
        // gc-common w5-a: `--verbose:gc` pause line (`door=gen-initial-mark`).
        let mut pause_timer =
            non_collection_pause_start(shared, NonCollectionPause::GenInitialMark);
        // Collect root pointers for old-gen marking. gen r5w3/unload7: under
        // `with_class_unload_marking` when this cycle unloads, which lifts the
        // concurrent-mark veto on side-table deferrals for THIS scan only
        // (`roots::generational_metadata_is_conditional`).
        let roots = if class_unload_on {
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread))
        } else {
            collect_roots(shared, thread)
        };
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        let mut root_ptrs: Vec<*mut u8> = roots
            .iter()
            .chain(snapshot_roots.iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.iter())
            .map(|r| r.as_ptr())
            .collect();
        // gen r4w4/concmark4 — the FROZEN peers' objects again, separately:
        // `initial_mark_with_frozen` scans each old-gen one inside this pause,
        // which closes the JIT SATB gate takeover window
        // (`gengc-r4w2-concmark-jit-gate-takeover-window`; the argument is on
        // `ConcurrentMarker::initial_mark_with_frozen`). Empty unless the
        // takeover froze someone.
        let frozen_ptrs: Vec<*mut u8> = xt_roots.iter().map(|r| r.as_ptr()).collect();
        // fork6 GC_STRESS fix — young→old references are mandatory
        // old-marking roots. `initial_mark` filters this list with
        // `old_gen.contains`, so an old object whose only path from a
        // root goes THROUGH a young object (root → young holder → old
        // target) was invisible and the sweep freed it live. Selective
        // promotion mass-produces exactly that shape (it tenures a
        // pinned young holder's children), which is why the Fork6Hard
        // GC_STRESS lane corrupted even single-threaded during clinit.
        // Safe here: brief STW, mutators quiesced, TLABs retired.
        //
        // gen r5w6/conc10: under `CRATONVM_GEN_Y2O_LIVE_SEED`, from the live
        // young set (the young collector's own, for THIS pause's roots) and
        // with the referent slot of every active young `Reference` hidden when
        // this cycle processes references at remark — `refproc_on`, the same
        // one-commit rule as the old-gen skip set below. Any doubt falls back
        // to the all-young enumeration inside the call.
        let y2o = if y2o_live_seed {
            gen_conc_y2o_seeds(shared, &root_ptrs, refproc_on)
        } else {
            None
        };
        match &y2o {
            Some(seeds) => root_ptrs.extend(seeds.old_refs.iter().map(|&a| a as *mut u8)),
            None => root_ptrs.extend(
                shared
                    .mem
                    .heap
                    .collect_young_to_old_roots()
                    .into_iter()
                    .map(|a| a as *mut u8),
            ),
        }
        // gen r5w5/conc9: where the young→old roots end (the young objects'
        // loaders follow), for the `CRATONVM_DBG_MIRRORPIN_WHY` report below.
        let young_to_old_end = root_ptrs.len();
        // gen r5w1/refs5 — half 1: hide the referent slot of every active
        // old-gen Weak/Soft/Phantom `Reference`, BEFORE `initial_mark_with_frozen`
        // scans anything. Screened (shape, identity) with no old-gen guard held;
        // `set_reference_skip` itself touches only the marker.
        if refproc_on {
            let candidates =
                gen_conc_reference_skip_candidates(shared, old_gen_base, old_gen_size);
            marker.set_reference_skip(&candidates);
            hidden_refs = Some(candidates);
            // gen r5w5/conc9: the remark's dead-finalizer retention records
            // what it marks, so the reference pass clears soft and weak
            // references to those objects before `finalize()` can resurrect
            // them (`gen_remark_process_references`).
            marker.record_finalizer_retention_closure();
        }
        // gen r5w3/unload7 — the side tables AS THIS PAUSE'S ROOT SCAN LEFT
        // THEM (the scan rebuilt this VM's `metadata_pin` rows), and the young
        // objects' loaders as roots, both before the marker scans anything.
        // See `cratonvm_gc::concurrent_mark::ClassUnloadTables`.
        if class_unload_on {
            let tables = cratonvm_gc::concurrent_mark::ClassUnloadTables::capture(
                old_gen_base,
                old_gen_size,
            );
            // gen r5w6/conc10: the loaders of the LIVE young objects' classes
            // when the seeds came from the live set (a dead young instance
            // keeps no loader), else every young object's, as before.
            match &y2o {
                Some(seeds) => root_ptrs.extend(gen_conc_y2o_instance_loaders(seeds, &tables)),
                None => root_ptrs.extend(gen_conc_young_instance_loaders(shared, &tables)),
            }
            if gen_conc_unload_why_enabled() {
                gen_conc_young_instances_why(shared, &tables, y2o.as_ref(), "initial-mark");
            }
            marker.set_class_unload_tables(tables);
        }
        // gen r5w4/conc8 — the retained-layout census: layouts an earlier
        // remark kept for its sweep are released once a complete sweep finds no
        // instance of their class (`ConcurrentGcState::note_layouts_retained`).
        // Its young half runs here, where the world is stopped. Nothing (not
        // even the young walk) unless a layout is pending, which needs
        // `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`.
        if shared.mem.concurrent_gc_state.has_retained_layouts() {
            marker.set_layout_census(gen_conc_layout_census_candidates(shared));
        }
        if let Some(guard) = shared.mem.heap.old_gen_lock() {
            marker.initial_mark_with_frozen(&root_ptrs, &frozen_ptrs, &*guard);
            initial_marked = true;
            // gen r5w3/unload7: the door that opened this cycle.
            shared.mem.concurrent_gc_state.note_door(
                census_door,
                cratonvm_gc::concurrent_mark::GenConcDoorStep::Opened,
            );
            // gen r4w3/oldgen3: the measurement
            // `docs/internal/gc/gengc-r4w2-concmark-jit-gate-takeover-window-FIXED-20260928.md` asks
            // for first — how often the pause that ARMS the JIT SATB gate has
            // frozen a thread mid-JIT (possibly between a gate test that read
            // "clear" and its store). Per-VM census on the shared phase state.
            shared
                .mem
                .concurrent_gc_state
                .note_initial_mark_takeover(pause_guard.frozen_count());
        }
        // gen r5w5/conc9 — which root source names each user loader and its
        // mirrors at the initial mark (diagnostic; see `gen_conc_unload_why`).
        if class_unload_on && initial_marked && gen_conc_unload_why_enabled() {
            let a = roots.len();
            let b = a + snapshot_roots.len();
            let c = b + xt_roots.len();
            gen_conc_unload_why(
                shared,
                &marker,
                "initial-mark",
                &[
                    ("collect_roots", &root_ptrs[..a]),
                    ("thread_snapshots", &root_ptrs[a..b]),
                    ("frozen_peers", &root_ptrs[b..c]),
                    ("young_to_old", &root_ptrs[c..young_to_old_end]),
                    ("young_instance_loaders", &root_ptrs[young_to_old_end..]),
                ],
            );
        }
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        non_collection_pause_seal(&mut pause_timer);
        let taken = pause_guard.release();
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        non_collection_pause_finish(pause_timer);
    }
    // gen r4w2/concmark: unreachable on Generational (`old_gen_lock()` is
    // always `Some` there), but a cycle whose initial mark did not run has no
    // snapshot to finish; say so structurally instead of remarking nothing.
    if !initial_marked {
        return;
    }

    // gen r4w2/concmark (2026-09-23) — objects per Phase-2 slice; `0` is the
    // pre-2026-09-23 driver (see `cratonvm_gc::concurrent_mark::gen_conc_mark_slice`).
    let slice = cratonvm_gc::concurrent_mark::gen_conc_mark_slice();

    // Phase 2: Concurrent Mark — runs while app threads continue
    if slice == 0 {
        // Legacy arm: the whole transitive closure under ONE old-gen lock
        // hold, with no safepoint poll. Every promotion waits for it, and so
        // does every other thread's pause (this thread is a counted mutator
        // that does not arrive), which is why the remark below then usually
        // loses its race. Kept as the A/B arm.
        if let Some(guard) = shared.mem.heap.old_gen_lock() {
            marker.concurrent_mark(&*guard);
        }
    }

    // gen r4w2/concmark — Phase 2 (sliced) and Phase 3, retried together.
    //
    // A slice holds the old-gen lock for at most `slice` objects, and the
    // thread polls a safepoint between slices, so a promotion waits for one
    // slice and another thread's pause gets this thread's arrival within one
    // slice, instead of both waiting for the whole closure
    // (`gengc-mark-concurrent-mark-holds-the-old-gen-lock-FIXED-20260923`). Between
    // slices another thread's young GC may promote into the old generation
    // (the next slice's object-start set covers that: `free_list_seq` moved)
    // or reclaim old-gen storage (the epoch moved; the slice reports the
    // closure done and the stale check below abandons the cycle without a
    // remark pause).
    //
    // A remark that loses its STW race is no longer the end of the cycle:
    // join the pause that won (it is waiting for this thread), trace whatever
    // it left, and ask again, up to `REMARK_ATTEMPTS` times. Nothing a
    // young pause does can make the eventual remark unsound — its snapshot,
    // SATB log, epoch and eligibility checks are exactly the ones a remark
    // that won the first time relies on.
    const REMARK_ATTEMPTS: u32 = 4;
    let mut remark_attempts = 0u32;
    let remark_outcome = loop {
        if slice > 0 {
            loop {
                let done = match shared.mem.heap.old_gen_lock() {
                    Some(guard) => marker.concurrent_mark_budget(&*guard, slice).1,
                    None => true,
                };
                cratonvm_gc::concurrent_mark::CONC_MARK_SLICES
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if done {
                    break;
                }
                // gcd d1/c — the service thread gives its cycle up at the next
                // Phase-2 slice once VM teardown has stopped it, so the
                // teardown's bounded wait for it to detach is short
                // (`gengc-r5w5-conc9-the-service-thread-can-outlive-vm-teardown`).
                // Dropping `cycle` abandons it, as the stale check below does.
                // Inert unless a service is attached AND stopped: one atomic
                // load per slice otherwise.
                if gen_conc_service_stopped_here(shared) {
                    tracing::debug!(
                        target: "cratonvm::gc::concurrent",
                        "concurrent cycle abandoned in Phase 2: the service thread was stopped"
                    );
                    return;
                }
                safepoint_check(shared, thread);
            }
            let stale = shared
                .mem
                .heap
                .old_gen_lock()
                .is_some_and(|guard| marker.cycle_is_stale(&*guard));
            if stale {
                // Another collector freed or slid old-gen storage since the
                // initial mark: the remark would refuse the sweep anyway.
                cratonvm_gc::concurrent_mark::CONC_CYCLE_STALE_ABANDONS
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
        match gen_concurrent_remark_pause(
            shared,
            thread,
            &marker,
            hidden_refs.as_deref(),
            class_unload_on,
            y2o_live_seed,
        ) {
            Some(remarked) => break Some(remarked),
            None => {
                // Lost the race. The legacy arm gives up here, as it always
                // did; the sliced arm joins the winning pause and retries.
                remark_attempts += 1;
                if slice == 0 || remark_attempts >= REMARK_ATTEMPTS {
                    cratonvm_gc::concurrent_mark::CONC_REMARK_ABANDONS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    break None;
                }
                cratonvm_gc::concurrent_mark::CONC_REMARK_RETRIES
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                safepoint_check(shared, thread);
            }
        }
    };

    // fork6 GC_STRESS fix — the remark STW is NOT optional. If another
    // thread's STW won the race (the request returned false — a
    // near-certainty under allocation storms, where a young-GC request is
    // always pending), the closure never ran: the SATB queue is undrained
    // and the roots were never rescanned, so the mark bitmap is NOT final.
    // The old code fell through to the sweep anyway and freed live objects.
    // Abort the cycle instead (deactivate the barrier, discard the bitmap);
    // the next `old_gen_needs_gc` trigger starts over.
    //
    // gen r4w2/concmark: `None` is every attempt lost, `Some(false)` a pause
    // whose remark did not run (see `gen_concurrent_remark_pause`). "Abort" is
    // now dropping `cycle`, which runs `abort_cycle` and releases the shared
    // phase for the next driver.
    //
    // gen r5w3/unload7 — the `Concurrent Sweep` `jdk.GCPhaseConcurrent` row
    // (obs6's cross-lane request): the cycle's JFR id, which the remark pause
    // left for this, taken NOW — while the phase is still `ConcurrentSweep`,
    // so no other cycle can have opened and replaced it — with the sweep's
    // start. Taken on the abort path too, so an abandoned cycle's id cannot
    // outlive it. `None` (and one uncontended lock) when no recording runs.
    let sweep_jfr = super::gc_events::gen_concurrent_sweep_jfr_open(shared);
    if remark_outcome != Some(true) {
        cycle.abort();
        return;
    }

    // Phase 4: Concurrent Sweep
    //
    // gen r4w3/oldgen3 (2026-09-23): in slices, like Phase 2
    // (`gengc-r4w2-concmark-concurrent-sweep-holds-the-old-gen-lock-FIXED-20260923.md`).
    // A slice walks at most `slice` objects under the old-gen lock, and the
    // thread polls a safepoint between slices, so a promotion or another
    // thread's pause waits for one slice instead of the whole sweep. What must
    // hold between slices (the epoch, the resume offset, TAMS) is argued at
    // `ConcurrentMarker::concurrent_sweep_budget`; a foreign free or compaction
    // in between stops the sweep there, reclaiming nothing further. `slice == 0`
    // is the pre-2026-09-23 single hold.
    //
    // THE ADDRESS-KEYED SIDE TABLES (gc-common w3-g; `common-b-address-keyed-
    // sweeps-miss-concurrent-reclamation`). The minted smuggled-`long` registry
    // and the Throwable backtrace table are swept by `update_all_roots`, which
    // this cycle never reaches. `OldGen::free` does not zero, so a freed block
    // still parses as its dead object, and the first object placed on it --
    // a young pause PROMOTING into the free list is the common way -- would
    // adopt the dead row: `update_all_roots`' own sweep after that pause sees
    // a live object at the address and keeps it. So the tables are swept here,
    // with the verdict `survived_in_place` (a free-list hole is dead), always
    // AFTER the old-gen guard is dropped (the predicate takes the same,
    // non-reentrant lock) and BEFORE anything can reuse what was freed:
    // * once when the sweep ends, and
    // * between slices, before a `safepoint_check` that is about to join a
    //   pending pause -- that pause may promote into the blocks the slices so
    //   far have freed. A pause requested after the check cannot complete
    //   without this (counted) thread's arrival, which is at the NEXT
    //   between-slice check, so this covers every promotion. Sweeping only
    //   then keeps the table walks to one per interleaved pause, not one per
    //   slice.
    // * The DIRECT old-gen allocation route (a humongous array placed on a
    //   block a slice freed, between the guard drop and a table sweep) needs
    //   no pause, so neither sweep above covers it, and after the allocation
    //   `survived_in_place` sees a live object and keeps the dead row. Closed
    //   in gc-common w5-g (`common-w3g-concurrent-sweep-direct-old-alloc-
    //   window`): each slice reports the spans it freed
    //   (`concurrent_sweep_budget_into`), and the rows keyed inside them are
    //   dropped by a pure range test UNDER the same guard, before anything can
    //   be allocated there (`addr_keyed::drop_address_keyed_rows_in`, whose
    //   doc argues the lock order). The `survived_in_place` sweeps stay as the
    //   cycle-level backstop.
    let mut swept = 0usize;
    let mut unswept_frees = false;
    let mut freed_spans: Vec<(usize, usize)> = Vec::new();
    // gen r4w4/concmark4: for the end-of-cycle log line below.
    let completed_before = shared.mem.concurrent_gc_state.census().cycles_completed;
    // gcd d1/c — THE REFERENCE PROCESSOR'S ROWS
    // (`gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`).
    // The address-keyed tables above do not include it: on a cycle that did
    // not process references at its remark (the default) nothing pruned the
    // row of a dead old `Reference` this sweep frees, and a block re-issued
    // before the next collection (a promotion, a direct old-gen allocation)
    // could adopt the row's verdict. Each slice's freed spans now drop the
    // soft / weak / phantom rows keyed inside them
    // (`gen_conc_sweep_drop_reference_rows`), by a pure range test, AFTER the
    // old-gen guard is dropped (the processor is never taken under it; see
    // `gen_concurrent_remark_pause`) and before the between-slice
    // `safepoint_check`, i.e. before any pause can promote onto those blocks.
    // A direct old-gen allocation can land on one in between; its object is
    // new, so its row (if it is a `Reference`) carries a registration number
    // at or above this snapshot and is kept. Taken here, after the remark:
    // everything registered from now on belongs to an object that is marked
    // or not sweep-eligible, i.e. never to one this sweep frees.
    let rows_registered_before = shared.mem.ref_processor.lock().registration_seq();
    let mut reference_rows_dropped = 0usize;
    if slice == 0 {
        if let Some(mut guard) = shared.mem.heap.old_gen_lock() {
            loop {
                let (n, done) = marker.concurrent_sweep_budget_into(
                    &mut *guard,
                    usize::MAX,
                    Some(&mut freed_spans),
                );
                swept += n;
                if done {
                    break;
                }
            }
            freed_spans.sort_unstable_by_key(|&(start, _)| start);
            crate::memory::addr_keyed::drop_address_keyed_rows_in(shared, &freed_spans);
        }
        // gcd d1/c: outside the guard (see above).
        reference_rows_dropped +=
            gen_conc_sweep_drop_reference_rows(shared, rows_registered_before, &freed_spans);
        unswept_frees = swept > 0;
    } else {
        loop {
            freed_spans.clear();
            let (n, done) = match shared.mem.heap.old_gen_lock() {
                Some(mut guard) => {
                    let r = marker.concurrent_sweep_budget_into(
                        &mut *guard,
                        slice,
                        Some(&mut freed_spans),
                    );
                    // Under the guard: nothing can have been allocated on
                    // these spans yet.
                    freed_spans.sort_unstable_by_key(|&(start, _)| start);
                    crate::memory::addr_keyed::drop_address_keyed_rows_in(shared, &freed_spans);
                    r
                }
                None => (0, true),
            };
            // gcd d1/c: the guard is dropped; before the safepoint below.
            reference_rows_dropped +=
                gen_conc_sweep_drop_reference_rows(shared, rows_registered_before, &freed_spans);
            swept += n;
            unswept_frees |= n > 0;
            if done {
                break;
            }
            if unswept_frees
                && shared
                    .mem
                    .gc_barrier
                    .stw_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                crate::memory::addr_keyed::sweep_address_keyed_tables(shared);
                unswept_frees = false;
            }
            safepoint_check(shared, thread);
        }
    }
    if unswept_frees {
        crate::memory::addr_keyed::sweep_address_keyed_tables(shared);
    }
    shared
        .mem
        .concurrent_gc_state
        .note_sweep_reference_rows_dropped(reference_rows_dropped);
    if swept > 0 {
        tracing::debug!("Concurrent GC: swept {} old-gen objects", swept,);
    }
    // Cycle complete — phase back to Idle (the write barrier's
    // `is_marking_active()` gate is already false after remark, but leaving
    // the shared state at `ConcurrentSweep` would misreport the VM as
    // mid-cycle to any observer).
    //
    // gen r4w2/concmark: `concurrent_sweep` already stored `Idle` on every
    // path, and that store handed the shared phase to whichever driver opens
    // next; `complete` releases ownership WITHOUT writing the phase again
    // (`finish_cycle` is now a CAS from `ConcurrentSweep`).
    cycle.complete();
    // gen r5w4/conc8 — unregister the retained layouts this sweep's census
    // released (empty unless one ran and was complete). Outside every pause
    // and every heap lock: no instance of these classes is left to size.
    let releasable = shared.mem.concurrent_gc_state.take_releasable_layouts();
    if !releasable.is_empty() {
        let released = crate::memory::gc::release_retained_layouts(shared, &releasable);
        shared
            .mem
            .concurrent_gc_state
            .note_layouts_released(released);
    }
    // gen r5w3/unload7: the sweep's JFR row, outside every pause.
    super::gc_events::gen_concurrent_sweep_jfr_close(shared, sweep_jfr);
    // gen r4w4/concmark4 — the cycle on the logs. The sweep's end recorded
    // this cycle's report (`ConcurrentGcState::note_cycle_end`); `cycle >
    // completed_before` makes sure it is THIS cycle's and not an older one
    // (a sweep that authorised nothing records no report). `-Xlog:gc` gets
    // the HotSpot-shaped `GC(c<n>) Concurrent Mark Cycle <old>M-><old>M(<cap>M)
    // <t>ms`; `--verbose:gc` the `[GC] concurrent-cycle:` key=value line.
    if let Some(report) = shared
        .mem
        .concurrent_gc_state
        .last_cycle_report()
        .filter(|r| r.cycle > completed_before)
    {
        // gen r5w3/obs7: under `gc+marking`, so a plain `-Xlog:gc` stays one
        // `GC(<int>)` sequence, as on HotSpot.
        if crate::runtime::unified_logging::is_unified_logging_enabled(
            &[crate::runtime::unified_logging::LogTag::GcMarking],
            crate::runtime::unified_logging::LogLevel::Info,
        ) {
            crate::runtime::unified_logging::log_unified(
                &[crate::runtime::unified_logging::LogTag::GcMarking],
                crate::runtime::unified_logging::LogLevel::Info,
                &report.xlog_line(),
            );
        }
        if shared
            .mem
            .verbose_gc
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            eprintln!("{}", report.verbose_line());
        }
    }
}

/// gen r5w5/conc9 — reopens the world if one of the generational concurrent
/// cycle's two pauses (the initial mark in [`maybe_concurrent_gc_at`], the
/// remark in [`gen_concurrent_remark_pause`]) UNWINDS.
///
/// Both pauses are open-coded (request, take-over, work, resume, release)
/// because they need the frozen peers' roots, which [`NonMovingPause`] does not
/// gather. So a panic anywhere between the take-over and the release — the
/// root scan, the marker, the remark's reference pass or class unload — left
/// `stw_requested` raised, every counted mutator parked and every frozen peer
/// suspended, for good: a hang instead of a crash report (the cycle guard
/// `ConcurrentCycle` aborted the cycle, but nothing reopened the world). On the
/// service thread (`CRATONVM_GEN_CONC_SERVICE_THREAD`) it also hid the panic.
/// This does on an unwind what [`NonMovingPause`]'s `Drop` does on every path:
/// resume the peers (retiring the skip spans the take-over published, or
/// clearing them if the take-over itself unwound) and complete the pause with
/// an empty pointer map (nothing moves in either pause). The normal path hands
/// the peers back with [`Self::release`] and runs its own resume and release,
/// byte for byte as before.
struct GenConcPauseGuard<'a> {
    shared: &'a SharedVm,
    taken: Option<crate::jit::xt_root_scan::TakenOver>,
    open: bool,
}

impl<'a> GenConcPauseGuard<'a> {
    /// Arm before the take-over (the pause is already requested).
    fn arm(shared: &'a SharedVm) -> Self {
        Self {
            shared,
            taken: None,
            open: true,
        }
    }

    /// Keep the take-over's frozen peers until the pause ends.
    fn hold(&mut self, taken: crate::jit::xt_root_scan::TakenOver) {
        self.taken = Some(taken);
    }

    /// How many peers the take-over froze for this pause.
    fn frozen_count(&self) -> usize {
        self.taken.as_ref().map_or(0, |t| t.count())
    }

    /// The normal end of the pause: disarm and hand the frozen peers back to
    /// the caller, which resumes them and releases the world itself.
    fn release(mut self) -> crate::jit::xt_root_scan::TakenOver {
        self.open = false;
        self.taken.take().unwrap_or_default()
    }
}

impl Drop for GenConcPauseGuard<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        match self.taken.take() {
            Some(taken) => retire_skip_spans_and_resume(self.shared, taken),
            // Unwound out of the take-over: it may have published spans.
            None => self.shared.mem.heap.clear_jit_tlab_skip_regions(),
        }
        self.shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
    }
}

/// Phase 3 of the generational concurrent cycle: ONE attempt at the remark
/// pause.
///
/// `None`: another thread's stop-the-world won the race and nothing ran.
/// `Some(false)`: the pause ran but the remark inside it did not (the
/// unreachable `old_gen_lock() == None` arm; see below). `Some(true)`: the
/// bitmap is final and the sweep is authorised or refused by the remark
/// itself.
///
/// gen r4w2/concmark (2026-09-23): split out of `maybe_concurrent_gc_at`
/// unchanged, so the driver can retry a lost pause.
///
/// gen r5w1/refs5: `hidden` is `Some(the skip-set candidates)` when this cycle
/// published a referent-slot skip set at its initial mark, and then the remark
/// MUST run the reference processing that clears or resurrects every hidden
/// referent (see [`gen_remark_process_references`]); `None` is the remark as
/// before.
///
/// gen r5w3/unload7: `class_unload` — this cycle unloads classes (its initial
/// mark armed the marker's side tables). The remark then takes its roots under
/// the class-unload licence too, re-captures the tables and the young
/// objects' loaders, and unloads after `remark_finish`
/// ([`gen_remark_unload_classes`]). Honoured only together with `hidden`: the
/// unload runs inside the reference-processing arm, and a remark that deferred
/// roots without reconciling afterwards would free what the deferred rows
/// name, so without `hidden` this remark roots everything, as before.
///
/// gen r5w6/conc10: `y2o_live_seed` — `CRATONVM_GEN_Y2O_LIVE_SEED`, decided
/// at the initial mark: the remark's young→old seeds come from the live young
/// set too, and with `hidden` the referent slot of every active young
/// `Reference` is hidden (listed afresh here: young objects move between the
/// pauses), with the safety net extended over that list
/// ([`gen_remark_process_references`]).
fn gen_concurrent_remark_pause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    marker: &cratonvm_gc::ConcurrentMarker,
    hidden: Option<&[usize]>,
    class_unload: bool,
    y2o_live_seed: bool,
) -> Option<bool> {
    let class_unload = class_unload && hidden.is_some();
    // Phase 3: Remark — brief STW pause.
    // INT-3 residual fix: open-coded for the same takeover-wait reason as
    // Phase 1 above (a never-polling in-JIT peer must not stall the remark
    // nor be covered only by its stale deposit snapshot).
    //
    // Finding 1(a): remark pauses use the identity census too, so blocked
    // threads are excluded BY IDENTITY and their wake-time arrivals cannot
    // satisfy this pause's quota (`arrive_and_wait_auto`). The anonymous
    // `threads_blocked` subtraction this replaced excluded the same population
    // without recording who it excluded. gc-common w4-a: and the census says
    // whether the initiator owns a counted slot (`request_non_collection_pause`).
    let counted_os_tids = request_non_collection_pause(shared, thread.thread_id)?;
    {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        // gen r5w5/conc9: as at the initial mark (`GenConcPauseGuard`).
        let mut pause_guard = GenConcPauseGuard::arm(shared);
        pause_guard.hold(stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids));
        // gc-common w5-a: `--verbose:gc` pause line (`door=gen-remark`).
        let mut pause_timer = non_collection_pause_start(shared, NonCollectionPause::GenRemark);
        // Round-5 fix (CRIT — UAF): drain the initiator's per-thread
        // SATB buffer before remark drains the global queue. Other
        // mutators flushed when they arrived at the STW barrier;
        // the initiator must drain its own.
        shared.mem.heap.flush_thread_satb();
        // gen r5w3/unload7: under the class-unload licence when this cycle
        // unloads (see `maybe_concurrent_gc_at`'s initial mark).
        let roots = if class_unload {
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread))
        } else {
            collect_roots(shared, thread)
        };
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        let mut root_ptrs: Vec<*mut u8> = roots
            .iter()
            .chain(snapshot_roots.iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.iter())
            .map(|r| r.as_ptr())
            .collect();
        // fork6 GC_STRESS fix — refresh the young→old roots at remark
        // too: a young→old edge created during the concurrent phase
        // (e.g. a promoted child stored into a fresh young holder) must
        // be in the final bitmap before the sweep.
        //
        // gen r5w6/conc10: from the live young set under
        // `CRATONVM_GEN_Y2O_LIVE_SEED` (see the initial mark). The young
        // `Reference`s are listed again: this pause's addresses.
        let y2o = if y2o_live_seed {
            gen_conc_y2o_seeds(shared, &root_ptrs, hidden.is_some())
        } else {
            None
        };
        match &y2o {
            Some(seeds) => root_ptrs.extend(seeds.old_refs.iter().map(|&a| a as *mut u8)),
            None => root_ptrs.extend(
                shared
                    .mem
                    .heap
                    .collect_young_to_old_roots()
                    .into_iter()
                    .map(|a| a as *mut u8),
            ),
        }
        let hidden_young: &[usize] = y2o.as_ref().map_or(&[][..], |s| &s.hidden_young_refs[..]);
        // gen r5w5/conc9: for the `CRATONVM_DBG_MIRRORPIN_WHY` report below.
        let young_to_old_end = root_ptrs.len();
        // gen r5w3/unload7: the rows THIS root scan deferred (merged into the
        // initial mark's; `remark_begin` marks those of every owner already
        // live) and the loaders of the young objects, which include every
        // instance allocated during the cycle.
        if class_unload {
            let (old_gen_base, old_gen_size) = shared.mem.heap.old_gen_info();
            let tables = cratonvm_gc::concurrent_mark::ClassUnloadTables::capture(
                old_gen_base,
                old_gen_size,
            );
            // gen r5w6/conc10: as at the initial mark.
            match &y2o {
                Some(seeds) => root_ptrs.extend(gen_conc_y2o_instance_loaders(seeds, &tables)),
                None => root_ptrs.extend(gen_conc_young_instance_loaders(shared, &tables)),
            }
            if gen_conc_unload_why_enabled() {
                gen_conc_young_instances_why(shared, &tables, y2o.as_ref(), "remark");
            }
            marker.add_class_unload_tables(tables);
        }
        // The remark is NOT optional (see the abort in `maybe_concurrent_gc_at`), but
        // this `if let` is the one place where that invariant rests on two
        // `match`es in `VmHeap` agreeing rather than on anything structural:
        // the cycle only gets here when the Generational arm's
        // `concurrent_cycle_due()` said yes (gen r4w4/concmark4; it was
        // `old_gen_needs_gc()`, a hard `false` on G1 and ZGC — the driver now
        // answers `false` itself off Generational), and `old_gen_lock()` is unconditionally
        // `Some` on Generational. So today the `None` arm is unreachable. It is
        // recorded rather than assumed because a fallible `old_gen_lock()` arm
        // would otherwise skip the remark SILENTLY and fall through to the
        // Phase 4 sweep with a non-final bitmap — the exact live-object loss
        // the fork6 fix below was written for, reintroduced by an edit two
        // files away.
        let remarked = if let Some(hidden) = hidden {
            // gen r5w1/refs5 — half 2: remark-time reference processing, with
            // the old-gen guard RELEASED around it (the design page's lock-order
            // option (b)). The processing reaches heap accessors that take the
            // old-gen lock themselves (`soft_ref_policy_free_mb`,
            // `is_heap_addr` in the closure tracer), which would self-deadlock
            // under a held guard, and it would add an `old-gen → ref_processor`
            // edge the lock ranking never had. `ConcurrentMarker::remark_begin`
            // argues why releasing the guard inside the pause is sound.
            //
            // SAFETY: as for the arm below — inside the pause this function
            // requested, every other mutator parked by `stw_take_over_and_wait`.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
            let progress = shared
                .mem
                .heap
                .old_gen_lock()
                .map(|guard| marker.remark_begin(&stw, &root_ptrs, &*guard));
            match progress {
                Some(progress) => {
                    let authorised = progress.sweep_authorised();
                    let keep = if authorised {
                        let is_marked = |addr: usize| -> bool { marker.remark_is_marked(addr) };
                        // gen r5w5/conc9: what `remark_begin`'s dead-finalizer
                        // retention marked (recorded because this cycle
                        // processes references at remark).
                        let mut retained = marker.take_finalizer_retention_closure();
                        // gcd d1/c — and the YOUNG objects only those reach,
                        // under `CRATONVM_GEN_Y2O_LIVE_SEED` (the live young
                        // set it needs is that flag's; see
                        // `gen_conc_young_behind_retained`).
                        if y2o_live_seed && !retained.is_empty() {
                            let young =
                                gen_conc_young_behind_retained(shared, &root_ptrs, marker, &retained);
                            retained.extend(young);
                        }
                        let (keep, retired) = gen_remark_process_references(
                            shared,
                            &is_marked,
                            hidden,
                            hidden_young,
                            class_unload,
                            &retained,
                        );
                        // gc-common w8-c, as in the arm below; a referent the
                        // reference pass is resurrecting keeps its weak global
                        // (G1's order, `g1_final_remark_cleanup`).
                        let _ = crate::native::jni::clear_weak_global_refs_unmarked(
                            shared, &is_marked, &keep,
                        );
                        shared
                            .mem
                            .concurrent_gc_state
                            .note_remark_refproc_retired(retired);
                        Some(keep)
                    } else {
                        None
                    };
                    let finished = match shared.mem.heap.old_gen_lock() {
                        Some(guard) => {
                            marker.remark_finish(&stw, progress, keep.as_deref(), &*guard);
                            true
                        }
                        // Unreachable (see above); the unfinished remark is
                        // abandoned with the cycle by the driver.
                        None => false,
                    };
                    // gen r5w3/unload7: the class unload, on the FINAL bitmap
                    // (after the keep closure and the last SATB drain), with
                    // the old-gen guard released and the world still stopped.
                    // A refused remark sweeps nothing and judges nothing dead.
                    if finished && authorised && class_unload {
                        // gen r5w5/conc9 — the final bitmap's verdict on each
                        // user loader, and which remark source named it
                        // (diagnostic; see `gen_conc_unload_why`).
                        if gen_conc_unload_why_enabled() {
                            let a = roots.len();
                            let b = a + snapshot_roots.len();
                            let c = b + xt_roots.len();
                            let keep_ptrs: Vec<*mut u8> = keep
                                .iter()
                                .flatten()
                                .map(|&addr| addr as *mut u8)
                                .collect();
                            gen_conc_unload_why(
                                shared,
                                marker,
                                "remark",
                                &[
                                    ("collect_roots", &root_ptrs[..a]),
                                    ("thread_snapshots", &root_ptrs[a..b]),
                                    ("frozen_peers", &root_ptrs[b..c]),
                                    ("young_to_old", &root_ptrs[c..young_to_old_end]),
                                    ("young_instance_loaders", &root_ptrs[young_to_old_end..]),
                                    ("refproc_keep", &keep_ptrs[..]),
                                ],
                            );
                        }
                        gen_remark_unload_classes(shared, marker);
                    }
                    finished
                }
                None => false,
            }
        } else if let Some(guard) = shared.mem.heap.old_gen_lock() {
            // SAFETY: this runs inside the pause this function requested
            // (`request_non_collection_pause`), after
            // `stw_take_over_and_wait` has parked every other mutator -- the
            // canonical construction site named in `StopTheWorldToken::new`'s
            // own safety note. `ConcurrentMarker::remark` requires the witness
            // because its SATB final drain does (2026-09-21): the drain has no
            // happens-before edge against a writer parked on a shard lock, and
            // only a real pause closes that window.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
            // gc-common w8-c: clear the JNI weak globals whose referent this
            // cycle judged dead, at the remark, while every mutator is stopped
            // and before `concurrent_sweep` can free the referent. After the
            // remark the SATB barrier is off, so a weak global resolved
            // between the remark and the free would hand out a referent the
            // sweep then frees. Returns nothing to resurrect.
            let mut clear_jni_weak = |is_marked: &dyn Fn(usize) -> bool| -> Vec<usize> {
                let _ = crate::native::jni::clear_weak_global_refs_unmarked(shared, is_marked, &[]);
                Vec::new()
            };
            marker.remark_with_reference_processing(
                &stw,
                &root_ptrs,
                &*guard,
                Some(&mut clear_jni_weak),
            );
            true
        } else {
            false
        };
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        non_collection_pause_seal(&mut pause_timer);
        let taken = pause_guard.release();
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        non_collection_pause_finish(pause_timer);
        Some(remarked)
    }
}

/// gen r5w1/refs5 — does this generational concurrent cycle run remark-time
/// reference processing? Opt-in `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` (the
/// wave-5 hook flag, now the real processing's switch), and only while the
/// reference protocol itself is on: `CRATONVM_WEAKREF_CLEAR=0` (never clear)
/// and `CRATONVM_DBG_NO_REFPROC` (process nothing) would leave hidden
/// referents unprocessed — freed by the sweep while their `Reference` still
/// points at them — so under either the cycle hides nothing.
fn gen_conc_remark_refproc_enabled() -> bool {
    cratonvm_types::flags().gc.gen_conc_remark_refproc_hook
        && weakref_clear_enabled()
        && !no_refproc()
}

/// gcd d1/c (2026-09-27) — is this thread the generational concurrent-cycle
/// service, and has VM teardown stopped it (`ConcurrentGcState::shutdown_service`)?
/// Only then does [`maybe_concurrent_gc_at`] give up a cycle between two
/// Phase-2 slices. The first test is one atomic load; the second (a leaf lock)
/// runs only after a stop.
fn gen_conc_service_stopped_here(shared: &SharedVm) -> bool {
    let state = &shared.mem.concurrent_gc_state;
    state.service_shutting_down() && state.on_service_thread()
}

/// gcd d1/c (2026-09-27) — drop the reference processor's soft / weak /
/// phantom rows whose `Reference` object lies in `freed_spans` (what one
/// concurrent sweep slice just freed; sorted by start, non-overlapping) and
/// that were registered before `registered_before`. Returns the ACTIVE rows
/// dropped. Nothing (not even the lock) for an empty span list.
///
/// Called by [`maybe_concurrent_gc_at`]'s sweep with NO heap lock held: the
/// processor (L7) is never taken under the old-gen guard (the remark's lock
/// order), and the test is a pure range test, so an object allocated on a
/// freed span since cannot make a dead row look live. See
/// `ReferenceProcessor::remove_reference_objects_registered_before` and
/// `gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`.
fn gen_conc_sweep_drop_reference_rows(
    shared: &SharedVm,
    registered_before: u64,
    freed_spans: &[(usize, usize)],
) -> usize {
    if freed_spans.is_empty() {
        return 0;
    }
    drop_swept_reference_rows(
        &mut shared.mem.ref_processor.lock(),
        registered_before,
        freed_spans,
    )
}

/// The processor half of [`gen_conc_sweep_drop_reference_rows`] (a unit-test
/// seam: no VM needed).
fn drop_swept_reference_rows(
    ref_proc: &mut cratonvm_gc::ReferenceProcessor,
    registered_before: u64,
    freed_spans: &[(usize, usize)],
) -> usize {
    ref_proc.remove_reference_objects_registered_before(registered_before, &|addr| {
        crate::memory::addr_keyed::in_spans(addr, freed_spans)
    })
}

#[cfg(test)]
mod gcd_d1c_sweep_reference_row_tests {
    use super::drop_swept_reference_rows;
    use cratonvm_gc::ReferenceType;

    /// `gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`:
    /// a weak row whose old `Reference` a sweep slice freed is gone after the
    /// slice's prune; a row between two freed spans, one registered after the
    /// sweep's snapshot on a freed block, and an empty slice change nothing.
    #[test]
    fn a_sweep_slice_drops_the_rows_of_the_references_it_freed() {
        let mut proc = cratonvm_gc::ReferenceProcessor::new_with_policy(1000);
        proc.discover_reference(ReferenceType::Weak, 0x10_000, 0x90_000, None);
        proc.discover_reference(ReferenceType::Weak, 0x10_100, 0x90_100, None);
        proc.discover_reference(ReferenceType::Phantom, 0x10_200, 0x90_200, None);
        let before = proc.registration_seq();
        proc.discover_reference(ReferenceType::Weak, 0x10_200, 0x90_300, None);
        let spans = [(0x10_000usize, 0x40usize), (0x10_200, 0x40)];

        assert_eq!(drop_swept_reference_rows(&mut proc, before, &[]), 0);
        assert_eq!(drop_swept_reference_rows(&mut proc, before, &spans), 2);
        let mut left = proc.reference_object_addresses();
        left.sort_unstable();
        assert_eq!(left, vec![0x10_100, 0x10_200], "the in-between row and the new one");
        assert_eq!(proc.weak_ref_count(), 2);
        assert_eq!(proc.phantom_ref_count(), 0);
    }
}

/// gen r5w1/refs5 — the referent-slot skip set's candidates for a generational
/// concurrent cycle: every ACTIVE Weak/Soft/Phantom `Reference` the VM's
/// processor holds that lies in the old generation and still IS that
/// `Reference` (not an array, a `java.lang.ref.Reference` subclass with the
/// referent/queue fields, the identity it was discovered with). Called inside
/// the initial-mark pause with NO old-gen guard held.
///
/// Every screen declines, because hiding is the dangerous direction: a slot 0
/// hidden on an object that is not the recorded `Reference` would hide an
/// ordinary STRONG field, and the object behind it could be swept live. A
/// declined candidate is simply traced (over-retention, as today). No
/// `java/lang/ref/Reference` class resolved yet hides nothing.
///
/// Lock ranks: the processor (L7) is read and RELEASED before the class
/// manager (L10) is taken, the order `process_references_after_gc` keeps.
fn gen_conc_reference_skip_candidates(
    shared: &SharedVm,
    old_gen_base: usize,
    old_gen_size: usize,
) -> Vec<usize> {
    let rows = shared
        .mem
        .ref_processor
        .lock()
        .active_reference_objects_with_stamps();
    if rows.is_empty() {
        return Vec::new();
    }
    let class_manager = shared.classes.class_manager.read();
    let Some(reference_cid) = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference")
    else {
        return Vec::new();
    };
    let old_gen_end = old_gen_base.saturating_add(old_gen_size);
    let heap = &shared.mem.heap;
    rows.into_iter()
        .filter(|&(addr, stamp)| {
            if addr < old_gen_base || addr >= old_gen_end || addr & 7 != 0 {
                // Young (its referent stays traced through
                // `collect_young_to_old_roots`) or not an object start.
                return false;
            }
            // SAFETY: an 8-aligned address inside the old generation's
            // storage, held by the processor; only its header is read below.
            let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
            heap.kind_of(obj) != crate::memory::heap::ObjectKind::Array
                && class_manager.is_subclass_of(heap.class_id_of(obj), reference_cid)
                && heap.num_fields(obj) >= 2
                && (stamp == 0 || {
                    let now = heap.identity_hash_code(obj);
                    now == 0 || now == stamp
                })
        })
        .map(|(addr, _)| addr)
        .collect()
}

/// gen r5w1/refs5 — remark-time reference processing for the GENERATIONAL
/// concurrent cycle: the twin of [`g1_remark_process_references`] that
/// `docs/internal/gc/gengc-r4w5-concmark5-remark-reference-processing-design-DONE-20260929.md`
/// specifies. Runs inside the remark pause, between
/// `ConcurrentMarker::remark_begin` and `remark_finish`, with the old-gen
/// guard RELEASED, and `is_marked` the seam's verdict ("marked in this cycle's
/// bitmap, or not sweep-eligible" — young, promoted after the initial mark,
/// or outside the old generation all read as live, so only an old-gen object
/// that existed at the initial mark can be judged dead).
///
/// Returns `(keep, retired)`: the addresses the remark must mark before the
/// sweep, and how many weak/soft/phantom rows left the processor's active set
/// this round (cleared, enqueued, or pruned because their own `Reference` was
/// dead by mark).
///
/// # What it reuses, and why that is the same code
///
/// The consumer protocol is G1's, and it is backend-agnostic once the heap
/// lock is not held: class-mirror / defining-loader reconcile and class
/// unloading, the dead-`Reference` pre-pass, the four phases with the
/// transitive tracer (`g1_remark_reference_closure`, which descends only into
/// UNMARKED objects — here exactly the sweep-eligible old ones — and treats
/// every `Reference`'s slot 0 as no edge, as the hiding marker does), the
/// SATB-suppressed referent clears, the dead-by-mark enqueue guards, the queue
/// wake-up hand-off (RECORDED for the delivery thread, never run here — the
/// initiator may be the service thread, which must not run Java), and the
/// keep list: dead finalizables, cleaner actions and chains, soft survivors.
/// So this calls it rather than copying ~150 lines of shape guards.
///
/// # What the generational cycle adds
///
/// 1. **Phantom slots.** A phantom enqueued here must have its slot 0 NULLED
///    (JDK 9+ clears a phantom as it is enqueued; the post-collection path
///    gets that from its pre-null pass, which never restores an enqueued
///    phantom). G1's remark leaves it: the referent is swept and the live
///    `PhantomReference` keeps a dangling slot 0 that the next card scan or
///    cycle would trace. Every row this round RETIRED whose `Reference` is
///    live gets its slot nulled (a weak/soft one already was — idempotent).
/// 2. **A safety net over the hidden set.** After processing, a live hidden
///    `Reference` whose slot 0 still names an UNMARKED referent — a row the
///    phases did not retire (softly or finalizer-reachable, policy-kept, or
///    one that went inactive between the two pauses without its slot being
///    cleared) — has that referent added to `keep`. The one-commit rule is
///    then a local invariant of this function: no hidden referent reaches the
///    sweep unmarked while a live `Reference` points at it.
///
/// # gen r5w3/unload7 — class unloading, and the retired rows
///
/// * `class_unload`: this cycle unloads classes
///   (`CRATONVM_GEN_CONC_CLASS_UNLOAD`). The class-metadata reconcile is then
///   NOT run here but after `remark_finish` ([`gen_remark_unload_classes`]):
///   the keep list this returns (finalizables about to be resurrected, soft
///   survivors, cleaner actions) is marked by that closure, and a kept object's
///   scan follows its class's `loader_pin` edge, so its loader is live by the
///   time the verdict is taken. Reconciling first (G1's order) would unload
///   the class of an object the same remark then resurrects. Without
///   `class_unload` the reconcile runs first, as before (under the veto every
///   user-loader mirror is a root, so it unloads nothing a scan could miss) —
///   with the dead classes' layouts retained for the sweep either way
///   (`memory::gc::with_retained_unloaded_layouts`).
/// * The retired rows come from the processor
///   (`ReferenceProcessingResult::retired`, plus the pre-pass's dropped active
///   rows) instead of two `active_reference_object_set()` builds and their
///   difference (`gengc-r5w1-refs5-proposal-processor-reports-what-it-retired`).
///
/// # gen r5w5/conc9 — references to what the finalizer retention kept
///
/// `retained` is what `ConcurrentMarker::remark_begin`'s dead-finalizer
/// retention marked (`take_finalizer_retention_closure`): every dead old
/// finalizable and everything reachable only through one. `is_marked` calls
/// them live, so without this a `WeakReference` to such an object survived
/// the remark, and `finalize()` could hand the object back to the program
/// with the weak reference still naming it. HotSpot clears soft and weak
/// references first. They are passed to [`remark_reference_rows`], which tells
/// the processor they are marked only for `finalize()` — the post-collection
/// path's `note_resurrected_finalizables` contract. Empty (no dead finalizable)
/// is the remark as before.
///
/// # gen r5w6/conc10 — the hidden YOUNG references
///
/// `hidden_young` lists the young `Reference`s whose slot 0 this remark's
/// young→old seeding hid (`CRATONVM_GEN_Y2O_LIVE_SEED`, `gen_conc_y2o_seeds`;
/// empty otherwise). The phases process their rows like any other (a young
/// `Reference` is live by `is_marked`, so a retired one gets its slot nulled
/// in step 1), and the safety net of step 2 covers them too: a live young
/// hidden `Reference` whose slot 0 still names an UNMARKED old-gen referent
/// has it kept.
fn gen_remark_process_references(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    hidden: &[usize],
    hidden_young: &[usize],
    class_unload: bool,
    retained: &[usize],
) -> (Vec<usize>, usize) {
    if !class_unload {
        let unloaded = remark_unload_dead_classes(shared, is_marked);
        // gen r5w4/conc8: this path retains layouts too (and the census
        // releases them), so it counts them, or `layouts_retained -
        // layouts_released` would stop being what is still held. Its loaders
        // and classes stay out of the class-unload counters, which describe
        // remarks that ran with the side tables armed.
        shared
            .mem
            .concurrent_gc_state
            .note_class_unload(0, 0, unloaded.layouts_retained);
    }
    let rows = remark_reference_rows(shared, is_marked, retained);
    let mut keep = rows.keep;
    let mut retired_rows = rows.pruned;

    // L10 alone: the processor lock was released above.
    let class_manager = shared.classes.class_manager.read();
    let reference_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference");
    let heap = &shared.mem.heap;
    // Same guard as the other referent writers: a registry address that is
    // live by mark and still a `Reference` (not an array; the class check is
    // skipped only when the class is not resolved, as theirs is).
    let is_reference_shaped = |obj: ObjectRef| -> bool {
        heap.kind_of(obj) != crate::memory::heap::ObjectKind::Array
            && heap.num_fields(obj) >= 2
            && match reference_cid {
                Some(cid) => class_manager.is_subclass_of(heap.class_id_of(obj), cid),
                None => true,
            }
    };

    // 1. Null the referent slot of every row this round retired (cleared or
    //    enqueued; the rows the pre-pass pruned as dead by mark are counted
    //    above and need nothing — the sweep frees their `Reference`).
    for &ref_addr in &rows.retired {
        retired_rows += 1;
        if !is_marked(ref_addr) {
            // Dead by mark: the sweep frees it.
            continue;
        }
        // SAFETY: a registry address, marked live in this cycle; nothing has
        // been freed since the initial mark (the epoch gate).
        let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
        if !is_reference_shaped(obj) {
            continue;
        }
        if let Value::Object(Some(_)) = heap.get_field(obj, 0) {
            // SATB-suppressed, as `g1_remark_process_references`' own clears:
            // logging the referent would feed it back into the final drain.
            heap.set_field_suppress_satb(obj, 0, Value::Object(None));
        }
    }

    // 2. The safety net over the hidden set.
    let (old_gen_base, old_gen_size) = heap.old_gen_info();
    let old_gen_end = old_gen_base.saturating_add(old_gen_size);
    for &ref_addr in hidden {
        if ref_addr < old_gen_base || ref_addr >= old_gen_end || !is_marked(ref_addr) {
            // Not hidden (young), or dead: nothing points at its referent.
            continue;
        }
        // SAFETY: as above.
        let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
        if !is_reference_shaped(obj) {
            continue;
        }
        if let Value::Object(Some(referent)) = heap.get_field(obj, 0) {
            let addr = referent.as_ptr() as usize;
            if !is_marked(addr) {
                keep.push(addr);
            }
        }
    }
    // 2b. gen r5w6/conc10 — the same net over the hidden YOUNG references
    //     (listed in this pause, so their addresses are current). Only an OLD
    //     referent can be unmarked; a young one is live by `is_marked`.
    for &ref_addr in hidden_young {
        if !is_marked(ref_addr) {
            continue;
        }
        // SAFETY: a current young-object address listed by this pause's
        // shape- and identity-screened scan (`gen_conc_young_reference_skip_candidates`);
        // the world is stopped and nothing has moved since.
        let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
        if !is_reference_shaped(obj) {
            continue;
        }
        if let Value::Object(Some(referent)) = heap.get_field(obj, 0) {
            let addr = referent.as_ptr() as usize;
            if addr >= old_gen_base && addr < old_gen_end && !is_marked(addr) {
                keep.push(addr);
            }
        }
    }
    drop(class_manager);
    (keep, retired_rows)
}

/// gen r5w3/unload7 — does this generational concurrent cycle UNLOAD classes?
/// Opt-in `CRATONVM_GEN_CONC_CLASS_UNLOAD`, and only while loader unloading and
/// instance→loader pinning are on (the side tables it follows exist only
/// then). The caller also requires remark-time reference processing
/// ([`gen_conc_remark_refproc_enabled`]): the unload runs in that remark.
fn gen_conc_class_unload_enabled() -> bool {
    cratonvm_types::flags().gc.gen_conc_class_unload
        && cratonvm_native_builtins::classloader::loader_unload_enabled()
        && cratonvm_types::loader_pin::loader_pinning_enabled()
}

/// gen r5w3/unload7 — the defining loader of every YOUNG object's class, as
/// extra old-gen mark roots for a class-unloading concurrent cycle (hazard 2
/// on `gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes`, its instance
/// half).
///
/// The concurrent trace never scans a young object, and the young objects'
/// FIELD edges into the old generation are roots already
/// (`collect_young_to_old_roots`) — but an instance's edge to its class's
/// loader is not a field, it is the `loader_pin` row. A class whose mirror the
/// root scan deferred and whose only live instances are young would otherwise
/// have an unmarked loader, and the remark would unload the class under its
/// live instances. Every young object is live by assumption here (the cycle
/// does not collect young), so every young object's loader is a root:
/// over-approximate in the safe direction, one young walk per pause. The
/// loaders that are themselves young need nothing (the trace does not collect
/// young); their OWN rows are marked by the marker's pre-pass
/// (`ConcurrentMarker::mark_rows_of_live_owners`).
///
/// Inside the initial-mark or remark pause, no heap lock held.
fn gen_conc_young_instance_loaders(
    shared: &SharedVm,
    tables: &cratonvm_gc::concurrent_mark::ClassUnloadTables,
) -> Vec<*mut u8> {
    let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap else {
        return Vec::new();
    };
    // gen r5w6/conc10: the walk's class ids, and whether it parsed ALL of
    // from-space. It re-anchors past an unparseable stretch, dropping the
    // objects inside it, so their classes' loaders would be judged by the
    // bitmap alone — a class unloaded under a live instance nobody saw. Then
    // every loader in the snapshot is a root (no unload this pause).
    let (class_ids, complete) = h.young_object_class_ids();
    let mut loaders = gen_conc_loaders_of_class_ids(&class_ids, tables);
    if !complete {
        loaders.extend(tables.all_loaders().into_iter().map(|l| l as *mut u8));
    }
    loaders
}

/// gen r5w6/conc10 — the defining loaders (in this snapshot) of `class_ids`,
/// as mark roots: the tail of [`gen_conc_young_instance_loaders`], and the
/// whole of it under `CRATONVM_GEN_Y2O_LIVE_SEED`, where the ids are the LIVE
/// young objects' classes (`YoungToOldSeeds::class_ids`).
fn gen_conc_loaders_of_class_ids(
    class_ids: &rustc_hash::FxHashSet<u32>,
    tables: &cratonvm_gc::concurrent_mark::ClassUnloadTables,
) -> Vec<*mut u8> {
    class_ids
        .iter()
        .filter_map(|&class_id| tables.loader_of(class_id))
        .map(|loader| loader as *mut u8)
        .collect()
}

/// gen r5w6/conc10 — one concurrent pause's young→old seeds under
/// `CRATONVM_GEN_Y2O_LIVE_SEED`, and the young `Reference`s whose slot 0 they
/// hid (the remark's safety net needs the list).
struct GenConcY2o {
    old_refs: Vec<usize>,
    class_ids: rustc_hash::FxHashSet<u32>,
    /// `YoungToOldSeeds::young_walk_complete`: `class_ids` names every young
    /// object's class it should.
    young_walk_complete: bool,
    hidden_young_refs: Vec<usize>,
}

/// gen r5w6/conc10 — [`gen_conc_young_instance_loaders`] under
/// `CRATONVM_GEN_Y2O_LIVE_SEED`: the loaders of the seeding young objects'
/// classes (the LIVE ones on the live path), plus every loader in the snapshot
/// when the young walk could not enumerate all classes.
fn gen_conc_y2o_instance_loaders(
    seeds: &GenConcY2o,
    tables: &cratonvm_gc::concurrent_mark::ClassUnloadTables,
) -> Vec<*mut u8> {
    let mut loaders = gen_conc_loaders_of_class_ids(&seeds.class_ids, tables);
    if !seeds.young_walk_complete {
        loaders.extend(tables.all_loaders().into_iter().map(|l| l as *mut u8));
    }
    loaders
}

/// gen r5w6/conc10 — `CRATONVM_GEN_Y2O_LIVE_SEED`: the young→old seeds of a
/// generational concurrent pause from the LIVE young set
/// (`GenerationalHeap::collect_young_to_old_seeds`, which falls back to the
/// all-young enumeration on any doubt; `[GC] conc_y2o:` counts both).
///
/// `root_ptrs` is this pause's root list so far (the collected roots, the
/// thread snapshots, the frozen peers' conservative words). The two extra live
/// inputs are what the young collector would keep although no root names it:
/// every finalizable object ([`finalizable_roots`]: the next young collection
/// RESURRECTS a dead one and its `finalize()` reads its fields) and every JNI
/// weak-global referent (the one non-root table that can hand a young object
/// back to a mutator before the next young collection decides it).
///
/// `hide_young_referents`: the cycle processes references at its remark
/// (`refproc_on`); the active young weak / soft / phantom `Reference`s then do
/// not seed their OLD referents (the remark's phases decide them, and its
/// safety net keeps any it leaves set). Without it nothing is hidden: the
/// one-commit rule of `ConcurrentMarker::set_reference_skip`.
///
/// Inside a concurrent pause, no heap lock held. `None` off Generational.
fn gen_conc_y2o_seeds(
    shared: &SharedVm,
    root_ptrs: &[*mut u8],
    hide_young_referents: bool,
) -> Option<GenConcY2o> {
    let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap else {
        return None;
    };
    let hidden_young_refs = if hide_young_referents {
        gen_conc_young_reference_skip_candidates(shared)
    } else {
        Vec::new()
    };
    let roots: Vec<usize> = root_ptrs.iter().map(|&p| p as usize).collect();
    let mut extra_live = finalizable_roots(shared);
    extra_live.extend(gen_conc_jni_weak_referents(shared));
    let seeds = h.collect_young_to_old_seeds(&cratonvm_gc::gen_heap::YoungToOldSeedRequest {
        roots: &roots,
        extra_live: &extra_live,
        hidden_young_refs: &hidden_young_refs,
        live_only: true,
    });
    shared
        .mem
        .concurrent_gc_state
        .note_y2o_seeds(&seeds, hidden_young_refs.len());
    Some(GenConcY2o {
        old_refs: seeds.old_refs,
        class_ids: seeds.class_ids,
        young_walk_complete: seeds.young_walk_complete,
        hidden_young_refs,
    })
}

/// gcd d1/c (2026-09-27) — the YOUNG objects a concurrent remark's
/// dead-finalizer retention keeps alive and nothing strong reaches: the young
/// half of the "marked only for `finalize()`" set that
/// [`gen_remark_process_references`] hands the processor
/// (`docs/internal/gc/gengc-r5w6-conc10-remark-weak-refs-to-young-objects-behind-a-retained-finalizable-FIXED-20260928.md`).
///
/// The retention closure (`retained`) holds only OLD objects: to the
/// concurrent cycle every young object is live, so a `WeakReference` to a young
/// object reachable only through a dead finalizable was kept, and `finalize()`
/// could hand the object back with the weak reference still naming it, where
/// HotSpot clears it first. This computes that young set with
/// `GenerationalHeap::young_reached_only_through`: the strong young set from
/// this pause's roots (`root_ptrs`) and the same extra live objects as the
/// seeds ([`gen_conc_y2o_seeds`]), with the old→young slot seed restricted to
/// old objects the remark marked that are NOT in `retained`; then what the
/// retained objects' young referents reach beyond it.
///
/// Every doubt keeps weak references (the behaviour before this pass): a
/// fallback of the live marking returns nothing, the strong set is an
/// over-approximation, and the answer never includes a strongly reachable
/// young object. Residual: an old object marked through a young object that
/// only a retained one reaches reads as strong, so what IT reaches is not
/// reported.
///
/// Inside the remark pause, after `remark_begin`, with no heap lock held.
/// Only under `CRATONVM_GEN_Y2O_LIVE_SEED` on a cycle that processes
/// references at its remark, and only when the retention found something.
fn gen_conc_young_behind_retained(
    shared: &SharedVm,
    root_ptrs: &[*mut u8],
    marker: &cratonvm_gc::ConcurrentMarker,
    retained: &[usize],
) -> Vec<usize> {
    let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap else {
        return Vec::new();
    };
    let roots: Vec<usize> = root_ptrs.iter().map(|&p| p as usize).collect();
    let mut extra_live = finalizable_roots(shared);
    extra_live.extend(gen_conc_jni_weak_referents(shared));
    let retained_set: rustc_hash::FxHashSet<usize> = retained.iter().copied().collect();
    let strong_holder =
        |addr: usize| -> bool { marker.remark_is_marked(addr) && !retained_set.contains(&addr) };
    let found = h.young_reached_only_through(&roots, &extra_live, &strong_holder, retained);
    shared
        .mem
        .concurrent_gc_state
        .note_y2o_finalizer_only_young(found.as_ref().ok().map(Vec::len));
    found.unwrap_or_default()
}

/// gen r5w6/conc10 — the YOUNG twin of [`gen_conc_reference_skip_candidates`]:
/// the processor's active weak / soft / phantom rows whose `Reference` object
/// is in the young generation, under the same screens (a `Reference`-shaped
/// non-array with at least two fields, and an identity stamp that still
/// matches), so a row whose object died and whose address was re-issued never
/// hides slot 0 of an unrelated object. Young membership is asked before the
/// class manager is read (it takes the young-from lock per row).
fn gen_conc_young_reference_skip_candidates(shared: &SharedVm) -> Vec<usize> {
    let rows = shared
        .mem
        .ref_processor
        .lock()
        .active_reference_objects_with_stamps();
    if rows.is_empty() {
        return Vec::new();
    }
    let heap = &shared.mem.heap;
    let young: Vec<(usize, i32)> = rows
        .into_iter()
        .filter(|&(addr, _)| addr != 0 && addr & 7 == 0 && heap.is_in_young_addr(addr))
        .collect();
    if young.is_empty() {
        return Vec::new();
    }
    let class_manager = shared.classes.class_manager.read();
    let Some(reference_cid) = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference")
    else {
        return Vec::new();
    };
    young
        .into_iter()
        .filter(|&(addr, stamp)| {
            // SAFETY: an 8-aligned address inside young from- or to-space,
            // held by the processor; only its header is read below (a to-space
            // address reads a wiped header and fails the class screen).
            let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
            heap.kind_of(obj) != crate::memory::heap::ObjectKind::Array
                && class_manager.is_subclass_of(heap.class_id_of(obj), reference_cid)
                && heap.num_fields(obj) >= 2
                && (stamp == 0 || {
                    let now = heap.identity_hash_code(obj);
                    now == 0 || now == stamp
                })
        })
        .map(|(addr, _)| addr)
        .collect()
}

/// gen r5w6/conc10 — the current referent address of every uncleared JNI
/// weak global ref. Read through [`crate::native::jni::clear_weak_global_refs_unmarked`]
/// with a predicate that records each address it is asked about and answers
/// "marked", so NOTHING is cleared: the JNI table offers this file no
/// read-only accessor (a cross-lane request on
/// `gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects-FIXED-20260929.md`
/// asks for one). One table lock, no heap lock.
fn gen_conc_jni_weak_referents(shared: &SharedVm) -> Vec<usize> {
    let seen: std::cell::RefCell<Vec<usize>> = std::cell::RefCell::new(Vec::new());
    let record = |addr: usize| -> bool {
        seen.borrow_mut().push(addr);
        true
    };
    let cleared = crate::native::jni::clear_weak_global_refs_unmarked(shared, &record, &[]);
    debug_assert_eq!(cleared, 0, "a predicate that answers `marked` clears nothing");
    seen.into_inner()
}

/// gen r5w6/conc10 — `CRATONVM_DBG_MIRRORPIN_WHY` on a class-unloading
/// concurrent pause: for every user loader some YOUNG object's class names,
/// how many young objects of its classes there are and (under
/// `CRATONVM_GEN_Y2O_LIVE_SEED`) whether their classes are in the live set —
/// the `young_instance_loaders` source of `[conc-unload-why]`, broken down.
/// One `[conc-unload-why] young-instances` line per loader. Diagnostic only:
/// one young walk, inside the pause, no heap lock held.
fn gen_conc_young_instances_why(
    shared: &SharedVm,
    tables: &cratonvm_gc::concurrent_mark::ClassUnloadTables,
    y2o: Option<&GenConcY2o>,
    pause: &str,
) {
    let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap else {
        return;
    };
    let mut per_class: std::collections::BTreeMap<u32, usize> = std::collections::BTreeMap::new();
    for (ptr, _size) in h.walk_young_objects() {
        // SAFETY: as in `gen_conc_young_instance_loaders`.
        let header = unsafe { &*(ptr as *const cratonvm_types::ObjectHeader) };
        *per_class.entry(header.class_id.as_u32()).or_default() += 1;
    }
    let mut per_loader: std::collections::BTreeMap<usize, Vec<(u32, usize)>> =
        std::collections::BTreeMap::new();
    for (class_id, count) in per_class {
        if let Some(loader) = tables.loader_of(class_id) {
            per_loader.entry(loader).or_default().push((class_id, count));
        }
    }
    for (loader, classes) in per_loader {
        let listed: Vec<String> = classes
            .iter()
            .map(|&(class_id, count)| {
                let live = match y2o {
                    Some(seeds) => {
                        if seeds.class_ids.contains(&class_id) {
                            "live"
                        } else {
                            "dead"
                        }
                    }
                    None => "all-young",
                };
                format!("{class_id}x{count}:{live}")
            })
            .collect();
        eprintln!(
            "[conc-unload-why] pause={pause} young-instances loader={loader:#x} classes=[{}]",
            listed.join(", ")
        );
    }
}

/// gen r5w4/conc8 — the retained-layout census candidates of this cycle: the
/// pending retained class ids (`ConcurrentGcState::retained_layouts_pending`)
/// of which the YOUNG generation holds no object, live or dead. Inside the
/// initial-mark pause, no heap lock held. A class id stays pending while any
/// young object of it exists, so its layout outlives every young walker that
/// could still size one; none can appear later (the class is out of the
/// store). The old half of the census is the sweep's
/// (`ConcurrentMarker::set_layout_census`).
fn gen_conc_layout_census_candidates(shared: &SharedVm) -> Vec<u32> {
    let pending = shared.mem.concurrent_gc_state.retained_layouts_pending();
    if pending.is_empty() {
        return pending;
    }
    let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap else {
        return Vec::new();
    };
    // gen r5w6/conc10: a walk that stepped over an unparseable stretch did not
    // see every young object, so "no young instance of this class" is not
    // proved for any pending id: no candidate this cycle (release deferred).
    let (young_class_ids, complete) = h.young_object_class_ids();
    if !complete {
        return Vec::new();
    }
    pending
        .into_iter()
        .filter(|id| !young_class_ids.contains(id))
        .collect()
}

/// gcd d2/g (`gengc-r5w4-conc8-proposal-stw-majors-take-the-retained-layout-census`)
/// — the retained-layout census after a STOP-THE-WORLD collection. Inside the
/// pause (`process_references_after_gc`), no heap lock held.
///
/// The concurrent census releases a layout a remark retained only after a
/// LATER concurrent cycle proves no instance is left; a program whose later old
/// collections are all stop-the-world majors never ran one, so the layouts
/// stayed for the life of the VM. Here, after a collection that reclaimed
/// old-generation storage (`old_gen_reclaimed_last_cycle`), both generations
/// are walked with the world stopped and a pending class id of which neither
/// holds any object, live or dead, is released. The census is of what the
/// collection LEFT, so it needs no reachability argument: a dead instance a
/// conservative root kept is still an object, and keeps its layout. Either
/// walk incomplete (an unparseable stretch or region) takes no census.
///
/// The release takes the class manager's write lock inside the pause, as the
/// unload transaction just above it does on the same path.
fn gen_stw_layout_census(shared: &SharedVm) {
    let state = &shared.mem.concurrent_gc_state;
    if !cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle() || !state.has_retained_layouts()
    {
        return;
    }
    let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap else {
        return;
    };
    let (mut survivors, young_complete) = h.young_object_class_ids();
    if !young_complete {
        return;
    }
    let (old_ids, old_complete) = h.old_object_class_ids();
    if !old_complete {
        return;
    }
    survivors.extend(old_ids);
    if state.complete_stw_layout_census(&survivors) == 0 {
        return;
    }
    let releasable = state.take_releasable_layouts();
    let released = crate::memory::gc::release_retained_layouts(shared, &releasable);
    state.note_layouts_released(released);
}

/// gen r5w3/unload7 — the class-metadata reconcile and unload transaction of a
/// generational concurrent remark: the same four calls
/// [`g1_remark_process_references`] makes, with the unloaded classes' compact
/// layouts RETAINED (`memory::gc::with_retained_unloaded_layouts`), because
/// this cycle's dead instances are freed by the concurrent sweep AFTER the
/// transaction, and the sweep's walk sizes them from that layout.
fn remark_unload_dead_classes(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
) -> crate::memory::gc::ClassMetadataUnloadResult {
    crate::memory::gc::reconcile_class_mirrors(shared, is_marked, None);
    let no_moves = cratonvm_types::PointerMap::default();
    let dead_class_hints = cratonvm_native_builtins::classloader::gc_reconcile_defining_loaders(
        shared.vm_identity,
        is_marked,
        &no_moves,
        None,
    );
    crate::runtime::invokedynamic::repin_lambda_proxy_loaders(shared.vm_identity);
    let unloaded = crate::memory::gc::with_retained_unloaded_layouts(|| {
        crate::memory::gc::unload_dead_class_metadata(shared, &dead_class_hints)
    });
    // gcd d10/r: the `mirror_pin` registry follows the pruned tables here too
    // (see `g1_remark_process_references`): the concurrent sweep is about to
    // free the dead loaders and mirrors this remark judged, and their rows
    // must not outlive them until the next stop-the-world epilogue.
    crate::memory::gc::rebuild_mirror_pins(shared, &no_moves);
    unloaded
}

/// gen r5w3/unload7 — the class unload of a generational concurrent remark
/// that ran with the side tables armed: AFTER `remark_finish` (the bitmap is
/// final: the keep closure and the last SATB drain are in it), still inside
/// the pause, with the old-gen guard released. The verdict is the remark's
/// (`ConcurrentMarker::remark_is_marked`: marked, or not sweep-eligible), so
/// only a loader that existed at the initial mark and that nothing — root,
/// field, young instance, side-table edge — reached is judged dead, and the
/// sweep then frees what the transaction dropped.
fn gen_remark_unload_classes(shared: &SharedVm, marker: &cratonvm_gc::ConcurrentMarker) {
    let is_marked = |addr: usize| -> bool { marker.remark_is_marked(addr) };
    let unloaded = remark_unload_dead_classes(shared, &is_marked);
    let state = &shared.mem.concurrent_gc_state;
    state.note_class_unload_remark();
    state.note_class_unload(
        unloaded.loaders_unloaded,
        unloaded.classes_unloaded,
        unloaded.layouts_retained,
    );
    if unloaded.classes_unloaded != 0 {
        tracing::debug!(
            loaders = unloaded.loaders_unloaded,
            classes = unloaded.classes_unloaded,
            jit_entries = unloaded.jit_entries_retired,
            layouts_retained = unloaded.layouts_retained,
            "generational concurrent remark unloaded class-loader metadata"
        );
    }
}

/// gen r5w5/conc9 — `CRATONVM_DBG_MIRRORPIN_WHY` (`CRATONVM_DBG=mirrorpin-why`)
/// on a class-unloading generational concurrent cycle. Read per call, not
/// cached in a `static` (the per-VM statics ratchet): it is asked at most
/// twice per class-unloading cycle, as `collect_roots` asks it per scan.
fn gen_conc_unload_why_enabled() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MIRRORPIN_WHY").is_some()
}

/// gen r5w5/conc9 — why a user loader is still live at a class-unloading
/// concurrent pause (`gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes`:
/// the orchestrator's run showed `concunload_remarks=1 concunload_classes=0`,
/// i.e. the remark ran and judged every loader live, and nothing in the
/// census says which of five root sources, or which side-table row, did it).
///
/// One stderr line per `mirror_pin` owner (the user loaders that minted a
/// mirror; at most 64), naming for the loader and each of its mirrors:
/// whether it is in the old generation, whether this cycle's bitmap marks it,
/// `live_or_ineligible` (the remark's verdict, `remark_is_marked`: an address
/// that is not an old-gen object start of the initial-mark snapshot — a YOUNG
/// object, one promoted since — is live whatever the bitmap says), and which of
/// this pause's root `sources` named it DIRECTLY (`-` for none: then it was
/// reached by the trace, or it is ineligible). The mirror's class name is
/// printed so a probe's class can be found. Diagnostic only: inside the pause,
/// no heap lock held, the class-mirror and class-manager read locks as
/// `collect_roots` takes them.
fn gen_conc_unload_why(
    shared: &SharedVm,
    marker: &cratonvm_gc::ConcurrentMarker,
    pause: &str,
    sources: &[(&str, &[*mut u8])],
) {
    let Some(rows) = cratonvm_types::mirror_pin::snapshot() else {
        eprintln!("[conc-unload-why] pause={pause} no mirror_pin rows");
        return;
    };
    let rows: Vec<(usize, Vec<usize>)> = rows.into_iter().take(64).collect();
    let mut targets: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    for (loader, mirrors) in &rows {
        targets.insert(*loader);
        targets.extend(mirrors.iter().copied());
    }
    let mut hits: rustc_hash::FxHashMap<usize, Vec<&str>> = rustc_hash::FxHashMap::default();
    for &(name, ptrs) in sources {
        for &p in ptrs {
            let addr = p as usize;
            if targets.contains(&addr) {
                let named = hits.entry(addr).or_default();
                if !named.contains(&name) {
                    named.push(name);
                }
            }
        }
    }
    let mirror_classes: rustc_hash::FxHashMap<usize, ClassId> = shared
        .classes
        .class_mirrors
        .read()
        .iter()
        .filter(|(_, m)| targets.contains(&(m.as_ptr() as usize)))
        .map(|(id, m)| (m.as_ptr() as usize, *id))
        .collect();
    let (old_base, old_size) = shared.mem.heap.old_gen_info();
    let old_end = old_base.saturating_add(old_size);
    let class_manager = shared.classes.class_manager.read();
    let describe = |addr: usize| -> String {
        let class = mirror_classes
            .get(&addr)
            .and_then(|id| class_manager.get_class(*id))
            .map(|c| format!(" class={}", c.name))
            .unwrap_or_default();
        format!(
            "{addr:#x}(old={} marked={} live_or_ineligible={} roots={}{class})",
            addr >= old_base && addr < old_end,
            marker.bitmap.is_marked(addr),
            marker.remark_is_marked(addr),
            hits.get(&addr).map_or_else(|| "-".to_string(), |v| v.join("+")),
        )
    };
    for (loader, mirrors) in &rows {
        let mirrors: Vec<String> = mirrors.iter().map(|&m| describe(m)).collect();
        eprintln!(
            "[conc-unload-why] pause={pause} loader={} mirrors=[{}]",
            describe(*loader),
            mirrors.join(", ")
        );
    }
}

/// gen r5w3/unload7 — the door census's name for the door that called the
/// generational driver. The service thread calls it with `MaybeGc` (the G1
/// token set has no service), so it is told apart by thread identity.
fn gen_conc_door(shared: &SharedVm, door: MarkDoor) -> cratonvm_gc::concurrent_mark::GenConcDoor {
    use cratonvm_gc::concurrent_mark::GenConcDoor;
    if shared.mem.concurrent_gc_state.on_service_thread() {
        return GenConcDoor::Service;
    }
    match door {
        MarkDoor::MaybeGc => GenConcDoor::MaybeGc,
        MarkDoor::AllocFail => GenConcDoor::AllocFail,
        MarkDoor::SystemGc => GenConcDoor::SystemGc,
        MarkDoor::JitDriver | MarkDoor::ForceFull => GenConcDoor::Other,
    }
}

// ---------------------------------------------------------------------------
// G1 concurrent marking cycle
// ---------------------------------------------------------------------------

/// Phase 1 of a ZGC concurrent cycle: **mark start**, at a brief STW pause.
///
/// Opens the cycle and returns. The transitive closure is then traced by
/// `ZMarkCoordinator`'s worker threads while every mutator in this VM runs;
/// the cycle is closed inside the next `collect_garbage`, which replays the
/// SATB ingress, re-scans the roots, and only then sweeps.
///
/// # Why this is shaped exactly like `g1_concurrent_mark_cycle`
///
/// Because the constraint is the VM's, not the collector's: a stop-the-world
/// pause in this VM can only be initiated by a thread that is in the thread
/// registry, holds a `JvmThread`, and can drive `stw_take_over_and_wait` for
/// in-JIT peers. A GC background thread is none of those. So the two phase
/// boundaries a concurrent collector needs — mark start and mark end — are
/// both taken by mutators, and the collector's own threads do only the part
/// that needs no safepoint: the tracing.
///
/// The open-coded `request → takeover-wait → work → complete` (rather than a
/// request plus a plain `wait_for_all` -- the deleted `brief_stw*` helpers) is
/// INT-3's residual fix, copied deliberately: a plain `wait_for_all()` stalls forever
/// on a peer spinning in compiled code, and its root set covers such a peer
/// only through a STALE deposit snapshot. A missed root here is an object the
/// concurrent phase never traces.
///
/// Mark-only pause: nothing moves, so there is no pointer map, no pin set and
/// no root rewrite — the frozen peers' conservative roots are simply extra
/// mark roots.
pub(super) fn zgc_concurrent_mark_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    // gc-common w4-a: the identity census, initiator slot included.
    let Some(counted_os_tids) = request_non_collection_pause(shared, thread.thread_id) else {
        // Another STW is in progress. Nothing has been done, so there is
        // nothing to unwind: the next allocation re-tests the threshold and
        // re-opens the cycle. If that other STW is a collection, the threshold
        // will have dropped and the cycle correctly does not open.
        return;
    };
    {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
        // gc-common w5-a: `--verbose:gc` pause line (`door=zgc-mark-start`).
        let mut pause_timer = non_collection_pause_start(shared, NonCollectionPause::ZgcMarkStart);

        // SAFETY: `stw_take_over_and_wait` above has parked every other mutator
        // at a safepoint (or forcibly stopped and conservatively scanned it),
        // and `taken` is still held, so this thread is the only mutator for the
        // whole block below.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };

        let roots =
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread));
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        let all_roots: Vec<ObjectRef> = roots
            .into_iter()
            .chain(snapshot_roots.into_iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.into_iter())
            .collect();

        let opened = shared.mem.heap.zgc_start_concurrent_mark(&stw, &all_roots);

        // Clear TLAB skip regions + resume frozen peers BEFORE reopening the
        // world — same race rationale as `maybe_gc`'s epilogue.
        non_collection_pause_seal(&mut pause_timer);
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        non_collection_pause_finish(pause_timer);

        if opened {
            tracing::debug!(
                "[ZGC] concurrent mark started: {} roots seeded",
                all_roots.len()
            );
        }
    }
}

/// Execute a full G1 concurrent marking cycle:
/// 1. Initial Mark (brief STW) — mark roots, activate SATB
/// 2. Concurrent Mark (background thread) — traverse heap regions
/// 3. Remark (brief STW) — drain SATB buffers, re-mark roots
/// 4. Cleanup — compute per-region liveness, free empty regions
pub(super) fn g1_concurrent_mark_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    // Phase 1: Initial Mark — brief STW pause.
    // Activates SATB write barrier and marks root-reachable objects.
    //
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) instead of a request plus a plain `wait_for_all` (the deleted
    // `brief_stw*` helpers), which stalls forever on a peer spinning in
    // compiled code — and whose root set covered such a peer only by its
    // STALE deposit snapshot (a missed mark root here = cleanup frees a
    // live object). Mark-only pause: the frozen peers' fresh conservative
    // roots are extra MARK roots; nothing moves, so no pin/pointer-map
    // concerns. gc-common w4-a: the request passes the identity census,
    // initiator slot included (`request_non_collection_pause`).
    let Some(counted_os_tids) = request_non_collection_pause(shared, thread.thread_id) else {
        return; // Another STW in progress
    };
    {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
        // gc-common w5-a: `--verbose:gc` pause line (`door=g1-initial-mark`).
        let mut pause_timer = non_collection_pause_start(shared, NonCollectionPause::G1InitialMark);
        // Round-5 fix (CRIT — UAF): drain the initiator's per-thread
        // SATB buffer on the way into initial-mark. Other mutators
        // already flushed on their `safepoint_check` arrival; the
        // initiator hasn't, and any buffered overwrites from before
        // SATB activation must reach the global queue before the
        // marker starts consuming it.
        shared.mem.heap.flush_thread_satb();
        // SAFETY (I-17): `stw_take_over_and_wait` above has parked every other
        // mutator at a safepoint (or forcibly stopped and conservatively
        // scanned it), and `taken` is still held, so this thread is the only
        // mutator for the whole initial-mark block below. G1's mark-cycle entry
        // points now require this witness for the same reason `collect_garbage`
        // does: they reclassify and free regions.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        shared.mem.heap.g1_start_concurrent_mark(&stw);
        // INT-8: publish the referent-slot skip set for this cycle —
        // the Weak/Soft/Phantom Reference OBJECT addresses currently
        // registered. Inside this STW the snapshot is consistent (no
        // mutator can construct, move, or free a Reference), and it
        // must land before the roots below seed the gray set so no
        // Reference is ever scanned without the skip in force. G1
        // carries the set across every mid-cycle evacuation pause
        // internally (remap survivors, prune CSet casualties).
        let ref_objs = shared.mem.ref_processor.lock().reference_object_addresses();
        shared.mem.heap.g1_set_reference_skip_set(&ref_objs);
        // Mark roots into the G1 mark bitmap
        let roots =
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread));
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        let all_roots: Vec<cratonvm_types::ObjectRef> = roots
            .into_iter()
            .chain(snapshot_roots.into_iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.into_iter())
            .collect();
        shared.mem.heap.g1_mark_roots(&stw, &all_roots);
        tracing::debug!("[G1] Initial mark: {} roots marked", all_roots.len());
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        non_collection_pause_seal(&mut pause_timer);
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        non_collection_pause_finish(pause_timer);
    }

    // Phase 2: Concurrent Mark — the background worker that drains
    // the mark worklist is owned by the VmHeap-level
    // `ConcurrentMarkController` (task #56). `g1_start_concurrent_mark`
    // above spawned it during the initial-mark STW, so by the time we
    // get here the marker is already running concurrently with mutators.
    //
    // Phases 3+4 (STW final remark + cleanup) are driven by
    // `maybe_concurrent_gc`: after each subsequent young collection the
    // GC-initiating mutator checks `g1_concurrent_mark_finished()` and,
    // once the worker has drained to a fixed point, runs
    // `g1_final_remark_cleanup` under its own brief STW. The previous
    // design — a detached watcher thread polling quiescence and calling
    // `g1_signal_marking_complete` (i.e. cleanup) directly — never ran a
    // final remark at all: the SATB log was discarded wholesale and the
    // roots were never re-scanned, so the sweep verdicts raced every
    // reference the mutators rewrote during the concurrent phase. A
    // watcher thread also cannot run the remark itself: it has no
    // JvmThread/barrier context to initiate an STW.
    //
    // Deferral note: if allocation stops entirely after IHOP fired, no
    // young GC follows and the cycle stays open (SATB active, cleanup
    // pending). That is benign — a heap nobody allocates into needs no
    // reclamation — and the next allocation-triggered GC closes it.
}

/// Phases 3+4 of the G1 cycle: STW final remark, then cleanup.
///
/// Called by `maybe_concurrent_gc` on the GC-initiating mutator once the
/// background marker has quiesced. Collects the full root set (all
/// threads) under a brief STW and hands it to
/// [`VmHeap::g1_final_remark_and_cleanup`], which re-marks roots, drains
/// the SATB log, completes the transitive closure, and runs cleanup —
/// all while the world is stopped.
///
/// fork6-pattern race handling: if another thread's STW wins
/// (the request returns false), nothing ran — the cycle simply
/// stays open (SATB active, worklist quiescent) and the next
/// `maybe_concurrent_gc` retries. Unlike the generational remark there is
/// nothing to abort: no sweep decision has been made yet.
///
/// LANE W7-D: returns whether the pause actually RAN. A lost STW race and a
/// completed cycle used to be the same `()`, so the mark-cycle door census
/// could not tell "this door finished the cycle" from "this door tried and was
/// beaten", and those have different meanings for a default decision — the
/// first says the door works, the second says nothing at all.
pub(super) fn g1_final_remark_cleanup(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) so a never-polling in-JIT peer neither stalls the pause
    // forever nor is covered only by its stale deposit snapshot (a missed
    // mark root here = cleanup frees a live object). Unlike the young/mixed
    // pauses nothing moves, so no pins — but `cleanup` DOES linearly walk
    // every non-Free region computing live bytes, so the takeover's
    // frozen-TLAB-tail publication (consumed by the region walkers' skip
    // checks) is load-bearing here too.
    //
    // Finding 1(a): remark pauses use the identity census too, so blocked
    // threads are excluded BY IDENTITY and their wake-time arrivals cannot
    // satisfy this pause's quota (`arrive_and_wait_auto`). gc-common w4-a: the
    // census also says whether the initiator owns a counted slot
    // (`request_non_collection_pause`).
    if let Some(counted_os_tids) = request_non_collection_pause(shared, thread.thread_id) {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
        // gc-common w5-a: `--verbose:gc` pause line (`door=g1-remark`) — the
        // pause `common-w4a-non-collection-pauses-have-no-pause-line` was
        // filed for: a fixed-point drain, remark-time reference processing
        // and a walk of every non-Free region, previously timed nowhere.
        let mut pause_timer = non_collection_pause_start(shared, NonCollectionPause::G1Remark);
        // Drain the initiator's per-thread SATB buffer; the other
        // mutators' buffers are pulled by `remark` itself
        // (`flush_all_thread_satb_buffers`) now that they are parked.
        shared.mem.heap.flush_thread_satb();
        let roots =
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread));
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        let all_roots: Vec<cratonvm_types::ObjectRef> = roots
            .into_iter()
            .chain(snapshot_roots.into_iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.into_iter())
            .collect();
        // INT-8: run VM reference processing against the completed mark
        // bitmap between the remark drain and cleanup — the only point
        // in the cycle where a weak/soft ref to a dead OLD-region
        // referent can be observed dead (evacuation pauses only ever
        // see CSet deaths). The callback returns the addresses the
        // heap must resurrect before cleanup's in-place frees.
        let mut process = |is_live: &dyn Fn(usize) -> bool| -> Vec<usize> {
            let keep = g1_remark_process_references(shared, is_live);
            // gc-common w8-c: JNI weak globals whose referent the completed
            // mark judged dead read NULL from here on, before cleanup frees a
            // region. A referent the reference pass is resurrecting (`keep`)
            // keeps its weak global.
            let _ = crate::native::jni::clear_weak_global_refs_unmarked(shared, is_live, &keep);
            keep
        };
        // SAFETY (I-17): same pause as above — `stw_take_over_and_wait` parked
        // every other mutator and `taken` is still held. The remark drain and
        // cleanup that follow are STW phases; the token is their witness.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        let completed =
            shared
                .mem
                .heap
                .g1_final_remark_and_cleanup(&stw, &all_roots, Some(&mut process));
        tracing::debug!(
            "[G1] Final remark: {} roots, cycle_completed={}",
            all_roots.len(),
            completed
        );
        // Cleanup freed every region whose objects are all unmarked, and no
        // `update_all_roots` follows this pause, so the address-keyed side
        // tables are swept here (gc-common w3-g; `common-b-address-keyed-
        // sweeps-miss-concurrent-reclamation`). Still inside the STW, so no
        // allocation can reuse a freed region before the sweep sees it
        // (`is_object_address` declines a Free region outright). The GPU input
        // cache is swept here too, and not after the Generational concurrent
        // sweep: its drain-then-rekey protocol requires stopped mutators.
        // Only when the cycle completed: an incomplete remark freed nothing.
        if completed {
            crate::memory::addr_keyed::sweep_address_keyed_tables(shared);
            #[cfg(feature = "gpu-offload")]
            crate::runtime::offload::input_cache::remap_and_sweep(
                shared.vm_identity,
                &cratonvm_types::PointerMap::default(),
                &shared.mem.heap,
            );
        }
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        non_collection_pause_seal(&mut pause_timer);
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        non_collection_pause_finish(pause_timer);
        true
    } else {
        tracing::debug!("[G1] Final remark lost the STW race — retrying at next GC");
        false
    }
}

/// INT-8 — remark-time reference processing (G1 only). Runs INSIDE the
/// final-remark STW, after the gray set drained to a fixed point and BEFORE
/// `cleanup()` frees anything, with `is_marked` = the collector's
/// bitmap+TAMS verdict. This is the HotSpot-shaped point where weak/soft
/// references to dead OLD-region referents finally clear: with referent-slot
/// hiding, the bitmap holds an untainted verdict for every referent, and the
/// young-pause processing path (whose `is_marked` treats every non-CSet
/// region as live) can never see these deaths.
///
/// Mirrors `process_references_after_gc`'s consumer protocol with two
/// deliberate differences:
/// - no pointer map (nothing moved in this pause) — the staleness guard is
///   dead-BY-MARK instead: a Reference/queue that is itself unmarked is
///   skipped (writing through it would be resurrection-by-side-effect right
///   before its region is freed);
/// - referent clears use the SATB-suppressed store: the clear is the
///   processor's decided verdict, and SATB-logging the old referent would
///   feed it straight back into the resurrection drain that follows.
///
/// Returns every address that must stay live through this cycle's cleanup:
/// dead finalizables about to run `finalize()` (this closes the
/// finalize-never-runs gap for in-place-freed regions), submitted cleaner
/// actions, pending cleaner chains, and policy-retained soft referents.
pub(super) fn g1_remark_process_references(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
) -> Vec<usize> {
    // G1's marker follows loader/mirror/metadata side edges during a full mark.
    // Reconcile those weak ownership tables against the completed bitmap before
    // cleanup frees dead regions and before the optional reference-processor
    // short-circuit.
    // G1's own mark bitmap has no separate roots Vec by this point (unlike
    // the Generational path this diagnostic was built for) -- `None` keeps
    // `CRATONVM_DBG_MIRRORPIN_WHY`'s verified-root check off for G1 rather
    // than feeding it a stale/empty vector that would misreport every real
    // root as unverified.
    crate::memory::gc::reconcile_class_mirrors(shared, is_marked, None);
    let no_moves = cratonvm_types::PointerMap::default();
    let dead_class_hints = cratonvm_native_builtins::classloader::gc_reconcile_defining_loaders(
        shared.vm_identity,
        is_marked,
        &no_moves,
        None,
    );
    // Lambda-proxy loader pins: see `process_references_after_gc`.
    crate::runtime::invokedynamic::repin_lambda_proxy_loaders(shared.vm_identity);
    let unloaded = crate::memory::gc::unload_dead_class_metadata(shared, &dead_class_hints);
    if unloaded.classes_unloaded != 0 {
        tracing::debug!(
            loaders = unloaded.loaders_unloaded,
            classes = unloaded.classes_unloaded,
            jit_entries = unloaded.jit_entries_retired,
            "G1 class-loader metadata unloaded at final remark"
        );
    }
    // gcd d10/r: rebuild the `mirror_pin` registry from the tables the two
    // reconciles just pruned, as the post-pause path does after its own
    // reconcile. Without it the rows of a loader this remark judged dead
    // stayed keyed at the loader's address, naming its dead mirrors, until
    // the next stop-the-world epilogue -- after `cleanup` below had freed both
    // -- and the next concurrent mark (`MarkSideTables`) followed them from
    // whatever object was allocated at that address in between.
    crate::memory::gc::rebuild_mirror_pins(shared, &no_moves);
    // G1's remark resurrects nothing before this pass: no retention closure.
    remark_reference_rows(shared, is_marked, &[]).keep
}

/// gen r5w3/unload7 — what [`remark_reference_rows`] did to the reference
/// processor's rows.
#[derive(Default)]
struct RemarkReferenceRows {
    /// Everything that must survive this cycle's reclamation (see
    /// [`g1_remark_process_references`]).
    keep: Vec<usize>,
    /// The soft / weak / phantom rows the processing round retired
    /// (`ReferenceProcessingResult::retired`).
    retired: Vec<usize>,
    /// ACTIVE rows the dead-`Reference` pre-pass dropped
    /// (`remove_collected_reference_objects`).
    pruned: usize,
}

/// The reference-processing half of [`g1_remark_process_references`]: every
/// step after the class-metadata reconcile, unchanged, plus what the processor
/// retired. Split out (gen r5w3/unload7) so the generational remark can run the
/// reconcile AFTER its keep closure when it unloads classes
/// (`gen_remark_unload_classes`), and read the retired rows without diffing
/// the registry.
///
/// gen r5w5/conc9: `resurrected` — objects `is_marked` calls live only because
/// the remark retained them for `finalize()` (the generational retention
/// closure; G1 passes none). They are noted to the processor before its round
/// (`note_resurrected_finalizables`, the post-collection contract), so soft and
/// weak references to them are processed as for unmarked objects, and the
/// soft / finalizer tracer descends into them. Empty is the pass as before.
fn remark_reference_rows(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    resurrected: &[usize],
) -> RemarkReferenceRows {
    // Same subsystem-level exclusion switch as the post-GC path.
    if no_refproc() {
        return RemarkReferenceRows::default();
    }
    let no_moves = cratonvm_types::PointerMap::default();
    // Same pressure input as the post-GC path — see the long comment there for
    // why this is allocatable headroom rather than the former hardcoded `64`,
    // and why the `0` clock argument is correct rather than a second hardcode.
    let free_mb = shared.mem.heap.soft_ref_policy_free_mb();
    // See `process_references_after_gc`: ClassManager must be consulted
    // before taking the lower-ranked reference-processor lock.
    let reference_next_slot = gc_reference_next_slot(shared);
    // gc-common w2-f: see `gc_enqueued_sentinel`. The remark moves nothing.
    let enqueued_sentinel = gc_enqueued_sentinel(shared, &no_moves);
    // See `process_references_after_gc`: same L10-before-L7 acquisition, same
    // shape guard, for the same reason — `is_marked` says the ADDRESS survived,
    // not that the object at it is still the Reference the processor recorded.
    let class_manager = shared.classes.class_manager.read();
    let reference_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference");
    let queue_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/ReferenceQueue");
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
    let enqueued_sentinel =
        enqueued_sentinel.filter(|&s| queue_cid.is_some() && is_queue_shaped(s));
    let mut ref_proc = shared.mem.ref_processor.lock();
    // gc-common w1-d (2026-09-23): the last-ditch soft-clear rule, applied
    // where on G1 it can actually free something.
    //
    // `last_ditch_reclaim` used to run the G1 full cycle with the LRU policy
    // and only THEN the soft-clear collection — and that collection is a
    // young pause, whose post-GC `is_marked` treats every non-CSet region as
    // live. A condemned soft referent in an Old region therefore "survived",
    // was restored, and the allocation went on to `OutOfMemoryError` with
    // softly-reachable Old data in hand: the `java.lang.ref` guarantee held
    // only for young referents. The remark is the one G1 point that sees Old
    // deaths, and its marker has HIDDEN every soft referent slot, so
    // condemning every active soft row here clears each one nothing else
    // marked; `soft_survivor_referents` below then resurrects none of them.
    // `last_ditch_reclaim` arms the rule around its full cycle; a remark run
    // on any other thread, or on any other path, sees it unarmed.
    let last_ditch = weakref_clear_enabled() && last_ditch_soft_clear_armed();
    if last_ditch {
        let _ = ref_proc.condemn_all_soft_refs();
    }
    // gc-common w1-d: retire the soft/weak/phantom rows whose `Reference`
    // OBJECT is dead by the completed bitmap, before the phases — the same
    // pre-pass the post-GC path runs, for the same reason (a dead
    // `SoftReference` must not seed `soft_live`). It matters more here than
    // it used to: those rows were synthetic roots until this round, so a
    // `Reference` could not die at remark; a queue-less one now can, and
    // `cleanup` is about to free its region in place. Left in the registry,
    // the row would name freed memory until the next post-GC prune, and the
    // next young pause's pre-collection pass reads the header at that
    // address. `is_marked` is exact for a `Reference` object: only its slot 0
    // is hidden from the marker, never the object itself.
    let pruned = if !no_refproc_prepass() {
        ref_proc.remove_collected_reference_objects(is_marked)
    } else {
        0
    };
    // gc-common w3-d (`docs/internal/gc-common-round-20260923/common-d-reference-protocol-residuals-FIXED-20260923.md` §5): the
    // phases get the TRANSITIVE soft- and finalizer-reachable closures, not
    // the single hop `process_references` falls back to. The marker hid every
    // soft referent slot, so an object reachable only at depth >= 2 through a
    // policy-retained soft referent is unmarked here: without a tracer a weak
    // reference to it was cleared, and a `Cleaner` / `PhantomReference` for it
    // FIRED — a `DirectByteBuffer` held by a soft cache through one holder
    // object had its native memory freed — while `soft_survivor_referents`
    // below then resurrected the whole subtree. The post-collection paths never
    // had the gap: their policy-kept soft slots are not hidden, so the marker
    // traces them.
    //
    // gen r5w5/conc9: the retained finalizables' closure is "marked only for
    // `finalize()`" — noted for the processor's soft and weak phases, and not
    // a stop for the tracer (a policy-kept soft referent reaching into it
    // keeps what lies behind it softly reachable). Empty: nothing changes.
    let resurrected_set: rustc_hash::FxHashSet<usize> = resurrected.iter().copied().collect();
    if !resurrected.is_empty() {
        ref_proc.note_resurrected_finalizables(resurrected);
    }
    let tracer_is_marked =
        |addr: usize| -> bool { is_marked(addr) && !resurrected_set.contains(&addr) };
    let remark_closure = |roots: &[usize]| -> Vec<usize> {
        g1_remark_reference_closure(shared, &tracer_is_marked, &is_reference_shaped, roots)
    };
    let remark_closure: &dyn Fn(&[usize]) -> Vec<usize> = &remark_closure;
    let result =
        ref_proc.process_references_with_finalizer_trace(is_marked, Some(remark_closure), free_mb, 0);
    // The remark has no pre-collection pass of its own, so nothing it did
    // must survive into the next collection's bookkeeping — least of all a
    // last-ditch condemnation. (Also retires a stale set left by a pass whose
    // post-GC half never ran.) See `ReferenceProcessor::finish_pre_gc_cycle`.
    ref_proc.finish_pre_gc_cycle();

    // Null the referent slot of newly-cleared references (once-only per
    // entry, same contract as the post-GC path).
    let cleared = ref_proc.take_newly_cleared();
    for ref_addr in cleared {
        if !is_marked(ref_addr) {
            // The Reference itself is dead this cycle — no mutator can ever
            // observe its slot again and cleanup may free it momentarily.
            continue;
        }
        // SAFETY: `ref_addr` is a registry address kept current by the
        // per-pause `update_after_gc`; nothing has been freed since.
        let obj_ref = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
        if shared.mem.heap.num_fields(obj_ref) < 2 || !is_reference_shaped(obj_ref) {
            continue; // belt-and-suspenders, mirrors the post-GC path
        }
        // SATB-suppressed: the clear is a decided verdict, not a semantic
        // overwrite — logging the old referent would resurrect it in the
        // re-drain below and retain the memory a full extra cycle.
        shared
            .mem
            .heap
            .set_field_suppress_satb(obj_ref, 0, Value::Object(None));
    }

    // gc-common w3-d: queues this remark links into, for the delivery thread's
    // wake-up — see the same list in `process_references_after_gc`. The remark
    // moves nothing, so the addresses are final. gc-common w5-d: also under
    // `--jdk-only` without the flag (`queue_wake_ups_recorded_for`).
    let record_enqueued_queues = queue_wake_ups_recorded(shared);
    let mut gc_enqueued_queues: Vec<usize> = Vec::new();

    // Queue links for cleared/phantom references. Skip the whole enqueue
    // when the Reference or its queue is dead-by-mark: linking a dead
    // Reference into a live queue would resurrect it into a region cleanup
    // is about to free (dangling queue head), and a dead queue has no
    // consumer to poll it.
    for (ref_addr, queue_addr) in &result.to_enqueue {
        if !is_marked(*ref_addr) || !is_marked(*queue_addr) {
            continue;
        }
        // SAFETY: registry addresses, current as above; both marked live.
        let ref_obj = unsafe { ObjectRef::from_raw(*ref_addr as *mut u8) };
        let q_obj = unsafe { ObjectRef::from_raw(*queue_addr as *mut u8) };
        if shared.mem.heap.num_fields(q_obj) < 2
            || shared.mem.heap.num_fields(ref_obj) < 2
            || !is_reference_shaped(ref_obj)
            || !is_queue_shaped(q_obj)
        {
            continue;
        }
        // Same linked-list protocol as the post-GC path: head/size on the
        // queue, linkage through the Reference's `next` slot (slot 2 on the
        // real-JDK layout; legacy 2-field shape falls back to slot 0).
        let old_head = shared.mem.heap.get_field(q_obj, 0);
        shared
            .mem
            .heap
            .set_field(q_obj, 0, Value::Object(Some(ref_obj)));
        let next_slot = if shared.mem.heap.num_fields(ref_obj) <= 2 {
            0 // legacy synthetic 2-field shape: referent, queue only
        } else {
            reference_next_slot
        };
        shared.mem.heap.set_field(ref_obj, next_slot, old_head);
        // G56-1 NOMINATION 7: this path used to read only `Int` (discarding a
        // `Long` and defaulting to 0) and always write back an `Int`, where
        // the sibling post-GC path above preserves the stored width because
        // `queueLength` is `long` on a real JDK `ReferenceQueue` whose own
        // `enqueue0`/`poll0` bytecode reads it back. Copy that arm so both
        // enqueue paths agree.
        let new_size = match shared.mem.heap.get_field(q_obj, 1) {
            Value::Long(v) => Value::Long(v + 1),
            Value::Int(v) => Value::Int(v + 1),
            _ => Value::Int(1),
        };
        shared.mem.heap.set_field(q_obj, 1, new_size);
        // Enqueued mark — see `gc_enqueued_sentinel`.
        shared
            .mem
            .heap
            .set_field(ref_obj, 1, gc_enqueued_mark(enqueued_sentinel, next_slot));
        if record_enqueued_queues {
            gc_enqueued_queues.push(*queue_addr);
        }
    }
    // Shape guards done; see the post-GC path.
    drop(class_manager);

    // Everything handed out below must survive this cycle's cleanup — the
    // caller marks these and re-drains the closure before any region is
    // freed.
    let mut resurrect: Vec<usize> = Vec::new();

    // Dead finalizables: submit for finalize() AND resurrect. This is the
    // half of INT-8 that closes the finalize-never-runs gap: previously a
    // finalizable object in a wholly-dead Old region was freed in place by
    // cleanup and the post-GC staleness guard then (correctly) dropped its
    // stale submission — finalize() silently never ran.
    // `enqueue_unfinalized`: reported from never-enqueued rows — see
    // `process_references_after_gc`'s finalize loop.
    for obj_addr in &result.to_finalize {
        shared.mem.finalizer_thread.enqueue_unfinalized(*obj_addr);
        resurrect.push(*obj_addr);
    }
    while let Some(obj_addr) = ref_proc.dequeue_for_finalization() {
        shared.mem.finalizer_thread.enqueue_unfinalized(obj_addr);
        resurrect.push(obj_addr);
    }

    // Cleaner actions fired by this round: submit + resurrect (the action
    // object is dereferenced later by run_cleaner_actions).
    for action_addr in &result.cleaner_actions {
        shared.mem.cleaner_thread.submit_action(*action_addr);
        resurrect.push(*action_addr);
    }

    // Pending (not-yet-fired) cleaner chains: the registry will hand these
    // out on a later cycle, so cleanup must not free them — the HotSpot
    // equivalent is the Cleaner's internal strong list. Self-referent
    // (finalizer-style) registrations are excluded inside the accessor.
    resurrect.extend(ref_proc.cleaner_pending_object_addresses());

    // Policy-retained soft referents: the marker never traced them
    // (referent-slot hiding), so a softly-only-reachable referent is
    // unmarked even though the LRU policy kept it — exactly like HotSpot,
    // reference processing itself keeps them alive.
    resurrect.extend(ref_proc.soft_survivor_referents());

    // DBG (CRATONVM_DBG_REFPROC_REMARK): per-remark mechanism evidence —
    // distinguishes clears that happened HERE (against the mark bitmap)
    // from clears the evacuation-pause path produced, which black-box
    // probes cannot tell apart.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_REFPROC_REMARK").is_some() {
        eprintln!(
            "[refproc-remark] soft_cleared={} weak_cleared={} enqueued={} finalize={} \
             cleaner_actions={} resurrect={}",
            result.stats.soft_refs_cleared,
            result.stats.weak_refs_cleared,
            result.to_enqueue.len(),
            result.to_finalize.len(),
            result.cleaner_actions.len(),
            resurrect.len(),
        );
    }

    // gc-common w3-d: queue wake-ups, and the finalizers / cleaner actions this
    // remark queued, go to the delivery thread (flag-gated). It cannot run
    // before this remark pause completes, by which time the caller has marked
    // everything in `resurrect`. A remark queue is marked, so cleanup does not
    // free it; the next pause roots it through `CleanerThread::pending_addresses`.
    drop(ref_proc);
    gc_enqueued_queues_hand_off(shared, &gc_enqueued_queues);

    RemarkReferenceRows {
        keep: resurrect,
        retired: result.retired,
        pruned,
    }
}

/// Upper bound on the objects [`g1_remark_reference_closure`] visits per call.
/// Past it the closure returned is partial, which only means some depth >= 3
/// case falls back to the pre-w3-d single-hop answer — never a wrong "live".
const G1_REMARK_CLOSURE_BUDGET: usize = 1 << 22;

/// The `trace_from` primitive `process_references_with_finalizer_trace` asks
/// for, built for G1's final remark from the heap's accessors: every UNMARKED
/// object transitively reachable from `roots` (soft survivors' referents, or
/// the referents of about-to-be-finalized objects).
///
/// * A MARKED object ends the walk: everything it reaches through a strong edge
///   is marked too, and the phases already treat `is_marked` as live.
/// * A `java.lang.ref.Reference`'s slot 0 (`referent`) is not an edge — the
///   object behind it is only as reachable as that reference's own strength,
///   exactly as the marker (which hides that slot) and HotSpot treat it.
/// * Primitive arrays have no edges; reference arrays are walked element-wise.
///
/// Runs inside the remark STW, before cleanup frees anything, so every object
/// reachable from a live root is intact. gc-common w3-d.
fn g1_remark_reference_closure(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    is_reference_shaped: &dyn Fn(ObjectRef) -> bool,
    roots: &[usize],
) -> Vec<usize> {
    let heap = &shared.mem.heap;
    let mut seen: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    let mut closure: Vec<usize> = Vec::new();
    let mut work: Vec<usize> = roots.to_vec();
    while let Some(addr) = work.pop() {
        if addr == 0 || !seen.insert(addr) {
            continue;
        }
        // The root itself is always reported (the tracer-less fallback reports
        // it too), marked or not.
        closure.push(addr);
        if is_marked(addr) || seen.len() > G1_REMARK_CLOSURE_BUDGET {
            continue;
        }
        let Some(obj) = heap.is_heap_addr(addr) else {
            continue;
        };
        let mut push = |v: Value| {
            if let Value::Object(Some(child)) = v {
                work.push(child.as_ptr() as usize);
            }
        };
        match heap.kind_of(obj) {
            crate::memory::heap::ObjectKind::Array => {
                if heap.array_element_type(obj) != Some(ArrayElementType::Reference) {
                    continue;
                }
                for i in 0..heap.array_length(obj) {
                    if let Ok(v) = heap.get_array_element(obj, i) {
                        push(v);
                    }
                }
            }
            crate::memory::heap::ObjectKind::Object => {
                let first = usize::from(is_reference_shaped(obj));
                for i in first..heap.num_fields(obj) {
                    push(heap.get_field(obj, i));
                }
            }
            _ => {}
        }
    }
    closure
}

/// The whole final rung of the allocation-failure ladder: everything the VM
/// can still do to satisfy an allocation that has already failed once, before
/// the caller is entitled to throw `OutOfMemoryError`.
///
/// Three steps, in order, because they reclaim disjoint things:
///
/// 1. [`g1_force_full_cycle`] — dead Old and humongous regions, which only a
///    completed mark cycle's cleanup reclaims. G1 only.
/// 2. G1 mixed evacuation after that mark — compacts partially-live Old
///    regions, which is the only way to coalesce a free run for a humongous
///    retry. G1 only.
/// 3. [`last_ditch_clear_soft_refs`] — every softly-reachable object, on every
///    collector. `java.lang.ref`'s guarantee is unconditional: all soft
///    references to softly-reachable objects are cleared before the VM throws
///    `OutOfMemoryError`. CratonVM honoured only the LRU half of the soft-ref
///    policy, which by construction never fires for the reference the failing
///    program is itself reading in a loop.
///
/// gc-common w1-d (2026-09-23): under G1 the full cycle in step 1 now runs
/// with the last-ditch soft rule ARMED, so its final remark condemns every
/// soft reference (`g1_remark_process_references`). Step 2 alone cannot clear
/// a soft referent in an Old region on G1: it is a young pause, and a young
/// pause treats every region outside its collection set as live.
///
/// **Not for `System.gc()`.** `force_gc_from_native` calls this for G1 to
/// finish the concurrent cycle synchronously, which clears EVERY soft
/// reference on every `System.gc()` — HotSpot's explicit full GC applies the
/// LRU policy and clears none it would keep. That caller wants
/// [`g1_force_full_cycle`]; see
/// `docs/internal/gc-common-round-20260923/applied/handoff-d-system-gc-must-not-run-the-last-ditch-soft-clear.md`.
pub(crate) fn last_ditch_reclaim(shared: &SharedVm, thread: &mut JvmThread) {
    g1_last_ditch_full_cycle(shared, thread);
    // A failed humongous request records its exact span only when the heap had
    // enough total Free regions but no run of that length.  Arm the one
    // compacting mixed pause only after the fresh full cycle above has supplied
    // sound mark data; ordinary mixed-policy caps remain untouched.
    if shared.mem.heap.is_g1() {
        shared.mem.heap.g1_arm_fragmented_humongous_compaction();
    }
    // Cleanup can return plenty of Free regions without one run large enough
    // for the allocation that brought us here: live objects in partially-full
    // Old regions still separate the holes.  The armed failure plan is exactly
    // ONE compacting mixed pause.  Do not drain the normal eight-pause mixed
    // cadence here: after the first pause has made the target window Free,
    // later forced young pauses are allowed to use it as to-space and can
    // fragment the very run the failed allocation is about to retry.
    if shared.mem.heap.is_g1() {
        // `g1_force_full_cycle` has just cleared the ordinary pressure latch.
        // Re-arm it for this one recovery pause: every Java frame and every
        // native argument is rooted at this allocation-failure boundary, so
        // G1 may use that exact root snapshot to discard stale RSet sources.
        // Without this handoff the recovery pause falls back to region-level
        // RSet scanning and can keep an unreachable old reference array (and
        // its target) alive long enough for the retry to fail despite ample
        // reclaimable space.  This is deliberately not a new global policy:
        // normal young pauses still retain their historical RSet contract.
        shared.mem.heap.note_young_spill_pressure();
        maybe_gc_forced_at(shared, thread, "g1-last-ditch-compaction");
    }
    let collected = last_ditch_clear_soft_refs(shared, thread);
    // gcd d2/j: the major below is the soft step's discipline now — bounded
    // retries of a collection THIS thread runs, and the thread-local request
    // withdrawn when every attempt lost the STW race
    // ([`generational_major_on_this_thread`]). It used to be one
    // `maybe_gc_forced_at` whose lost race left the request pending: the
    // ladder then threw `OutOfMemoryError` after a peer's YOUNG pause with the
    // old generation's garbage still in place, and the stale request turned
    // this thread's next unrelated collection into a major (and chose the
    // requested-major root overlay for it).
    //
    // gen r4w3/hunter (2026-09-23): the Generational backend's allocation
    // ladder never reached the OLD generation on its own. `maybe_gc_forced`
    // runs a young cycle, and that cycle runs a major one only when old gen
    // is at least 75% full or a major was requested
    // (`OldGen::major_trigger_verdict`); the only request on this ladder was
    // the soft-reference step above, which returns early when the program
    // holds no live soft references. So an allocation that needs space the
    // old generation holds as GARBAGE below the 75% floor — a large array,
    // which goes straight to old gen, or a young survivor that cannot be
    // promoted — threw `OutOfMemoryError` with most of the heap reclaimable.
    // HotSpot's Serial collector runs a full collection before giving up.
    // Same thread-local request and the same STW-race caveat as the soft
    // step's own major (see its comment). A soft step that did collect has
    // already requested a major, so this runs at most one extra collection,
    // and only on the path to `OutOfMemoryError`.
    // See `docs/internal/gc/gengc-r4w3-hunter-oom-ladder-never-collects-old-gen-FIXED-20260923.md`.
    if !collected && shared.mem.heap.is_generational() {
        // `maybe_gc_forced_collected` retires this thread's TLAB itself.
        let _ = generational_major_on_this_thread(
            shared,
            thread,
            "last-ditch-major",
            LAST_DITCH_MAJOR_ATTEMPTS,
        );
    }
}

/// Step 1 of [`last_ditch_reclaim`]: G1's complete marking cycle, with the
/// last-ditch soft rule armed when the program holds live soft references
/// (gc-common w1-d, see there). A no-op on the other backends. Split out by
/// gcd d10/o so the overhead-limit exit ([`g1_overhead_limit_full_cycle`])
/// runs the very same cycle.
fn g1_last_ditch_full_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    let arm_soft_clear = shared.mem.heap.is_g1()
        && weakref_clear_enabled()
        && shared.mem.ref_processor.lock().has_active_soft_refs();
    if arm_soft_clear {
        crate::runtime::interpreter::with_last_ditch_soft_clear(|| {
            g1_force_full_cycle(shared, thread);
        });
    } else {
        g1_force_full_cycle(shared, thread);
    }
}

/// gcd d10/o (2026-09-28): G1's full collection before a GC-overhead-limit
/// `OutOfMemoryError`. `true` = the cycle freed at least 2 % of the heap
/// since `before_live` (a `VmHeap::live_bytes_estimate` the caller sampled
/// before its own collections), so the streak is reset and the caller retries
/// once; `false` = no cycle (not G1, or `CRATONVM_GC_OVERHEAD_PROGRESS=0`), or
/// a futile one, and the verdict stands.
///
/// # The gap
///
/// With the overhead streak latched, every allocation ladder honoured the
/// verdict after the forced collection and the soft-reference rung: the
/// interpreter's (`collect_and_retry_with_thread`, whose comment said "G1's
/// last-ditch cycle stays off this fast-fail exit"), the native door's
/// (`NativeContextImpl::reclaim_before_alloc_retry`) and the JIT helpers'
/// (`jit_latched_overhead_limit_throws`, unless a soft-reference collection
/// ran). On Generational that exit runs a major first (gen r4w4, gcd d4/j),
/// and a ZGC forced cycle is itself whole-heap. On G1 the forced collection
/// is a YOUNG pause, and dead Old and humongous regions are reclaimed only by
/// a completed marking cycle's cleanup (see [`g1_force_full_cycle`]), so a
/// G1 program that caught an `OutOfMemoryError` and dropped its data met the
/// latched verdict on its next allocation with the dropped data still in
/// place: an `OutOfMemoryError` no full collection had decided. HotSpot's G1
/// runs a full collection before every heap `OutOfMemoryError`.
///
/// # The rule
///
/// The Generational exit's (gen r4w5/thrash5): a full collection this caller
/// ran decides. A productive one resets the streak (as `note_gc_productivity`
/// does for a Generational major) and earns one retry; a futile one keeps the
/// verdict, so a G1 heap wedged on live data still throws instead of limping
/// on one full cycle per object. Measured from the caller's `before_live`:
/// the interpreter's ladder samples it just before this call (its attempt
/// after the forced collection has already failed), the native door before
/// its forced collection (it cannot attempt in between, so what that
/// collection and the soft rung freed counts too). The cycle is
/// [`last_ditch_reclaim`]'s own G1 step (soft rule armed when soft references
/// are live).
///
/// `CRATONVM_DBG_GC_OVERHEAD=1` prints one `[GC_OVERHEAD] g1-overhead-cycle:`
/// line per call.
pub(crate) fn g1_overhead_limit_full_cycle(
    shared: &SharedVm,
    thread: &mut JvmThread,
    before_live: usize,
) -> bool {
    if !shared.mem.heap.is_g1() || !gc_overhead_progress_enabled() {
        return false;
    }
    let cap = shared.mem.heap.heap_capacity();
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    g1_last_ditch_full_cycle(shared, thread);
    let after = shared.mem.heap.live_bytes_estimate();
    let productive = g1_overhead_cycle_productive(before_live, after, cap);
    if productive {
        shared
            .mem
            .gc_unproductive_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }
    if gc_overhead_dbg_on() {
        eprintln!(
            "[GC_OVERHEAD] g1-overhead-cycle: thread={} before={before_live} after={after} \
             cap={cap} productive={productive}",
            thread.thread_id.0,
        );
    }
    productive
}

/// The pure verdict of [`g1_overhead_limit_full_cycle`]: freed at least 2 %
/// of `cap` (the overhead limit's one yardstick, [`two_percent_sliver`]).
fn g1_overhead_cycle_productive(before_live: usize, after_live: usize, cap: usize) -> bool {
    // Widening: usize -> u64 is loss-free on every supported target.
    cap != 0 && !two_percent_sliver(before_live.saturating_sub(after_live) as u64, cap)
}

#[cfg(test)]
mod gcd_d10o_ladder_tests {
    //! gcd d10/o: G1's full cycle before an overhead-limit error, the native
    //! door's shared ladder, and the delivery thread's notification name.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;

    const CAP: usize = 128 * 1024 * 1024;

    /// The verdict is the overhead limit's own yardstick: 2 % of the heap
    /// freed, measured from the caller's `before_live`; a live set that grew
    /// meanwhile frees nothing, and a zero capacity is never productive.
    #[test]
    fn the_g1_cycle_is_productive_at_two_percent() {
        let two_percent = CAP / 50;
        assert!(g1_overhead_cycle_productive(90_000_000, 90_000_000 - two_percent - 8, CAP));
        assert!(!g1_overhead_cycle_productive(90_000_000, 90_000_000 - two_percent + 8, CAP));
        assert!(!g1_overhead_cycle_productive(90_000_000, 95_000_000, CAP));
        assert!(!g1_overhead_cycle_productive(90_000_000, 0, 0));
    }

    /// Off G1 the helper runs nothing and keeps the verdict, whatever the
    /// streak says: the Generational and ZGC exits are unchanged.
    #[test]
    fn the_g1_cycle_is_g1_only() {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let mut thread = JvmThread::new(ThreadId(0), "d10o-g1-only");
        let cycles0 = shared.mem.gc_cycle_count.load(std::sync::atomic::Ordering::Relaxed);
        assert!(!g1_overhead_limit_full_cycle(&shared, &mut thread, CAP));
        assert_eq!(
            shared.mem.gc_cycle_count.load(std::sync::atomic::Ordering::Relaxed),
            cycles0
        );
    }

    /// On a heap with room (no overhead streak) the native door's ladder
    /// collects and earns its caller the retry, as it always did.
    #[test]
    fn an_unlatched_native_ladder_earns_the_retry() {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let mut thread = JvmThread::new(ThreadId(0), "d10o-native-ladder");
        assert!(!gc_overhead_limit_exceeded(&shared));
        let cycles0 = shared.mem.gc_cycle_count.load(std::sync::atomic::Ordering::Relaxed);
        assert!(native_reclaim_before_alloc_retry(&shared, &mut thread));
        assert!(
            shared.mem.gc_cycle_count.load(std::sync::atomic::Ordering::Relaxed) > cycles0,
            "the ladder collected"
        );
    }

    /// Nothing queued: the delivery thread's notification step returns at
    /// once and leaves the thread's name alone; with notifications, the name
    /// is restored after the delivery (a thread with no Java mirror, as in a
    /// unit VM, renames nothing on the heap).
    #[test]
    fn the_notification_step_restores_the_thread_name() {
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), FINALIZER_THREAD_NAME);
        run_gc_notifications_as_notification_thread(&shared, &mut thread);
        assert_eq!(thread.name, FINALIZER_THREAD_NAME);
        set_delivery_mirror_name(&shared, &thread);
        assert!(thread.java_thread_obj.is_none());
        assert_eq!(NOTIFICATION_THREAD_NAME, "Notification Thread");
    }
}

/// Attempts [`last_ditch_reclaim`]'s Generational major makes before it gives
/// up on winning the STW race: the soft step's `LAST_DITCH_ATTEMPTS`, for the
/// same reason (each lost attempt lets a peer's pause finish, so this cannot
/// livelock).
const LAST_DITCH_MAJOR_ATTEMPTS: u32 = 4;

/// A Generational major collection run by THIS thread for a rung of the
/// allocation-failure ladder: the thread-local major request
/// (`gc_quiescence::request_major_gc`) is honoured only by a collection this
/// thread initiates, so request and collect up to `attempts` times until this
/// thread wins the STW race. Returns whether it did. A request still pending
/// afterwards -- every attempt lost, or a won pause that did not consume it --
/// is withdrawn, so it cannot turn this thread's next unrelated collection
/// into a major. gcd d2/j (2026-09-27). `pub(crate)` for the collecting door
/// of `runtime::exceptions::create_exception_object_for_class_inner`.
pub(crate) fn generational_major_on_this_thread(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
    attempts: u32,
) -> bool {
    let mut collected = false;
    let t0 = std::time::Instant::now();
    for _attempt in 0..attempts {
        cratonvm_gc::gc_quiescence::request_major_gc();
        collected = maybe_gc_forced_collected(shared, thread, site);
        if collected {
            break;
        }
    }
    if cratonvm_gc::gc_quiescence::major_gc_requested() {
        let _ = cratonvm_gc::gc_quiescence::take_major_gc_request();
    }
    // gcd d9/b: `[GC] oome_ladder: ladder_majors*`. The time includes any
    // lost attempts' participation in a peer's pause.
    {
        use cratonvm_gc::gen_heap::oome_ladder as l;
        if collected {
            ladder_census(shared, l::MAJORS, 1);
            ladder_census(shared, l::MAJORS_US, elapsed_us(t0));
        } else {
            ladder_census(shared, l::MAJORS_LOST, 1);
        }
    }
    collected
}

/// Collect once with every SoftReference condemned — see
/// `ReferenceProcessor::condemn_all_soft_refs` for the rule and
/// `with_last_ditch_soft_clear` for why the arming is thread-local.
///
/// Skipped outright when the application holds no live soft references, which
/// is the common case; a heap that is genuinely full then pays no extra GC on
/// its way to `OutOfMemoryError`.
///
/// Returns whether it initiated a collection (gen r4w3/hunter: the caller runs
/// the Generational last-ditch major itself when this step did not).
pub(crate) fn last_ditch_clear_soft_refs(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    if !weakref_clear_enabled() {
        // The opt-out restores the legacy never-clearing behaviour wholesale;
        // that has to include this rule, or `CRATONVM_WEAKREF_CLEAR=0` would
        // no longer be the byte-identical safety net it is documented to be.
        return false;
    }
    if !shared.mem.ref_processor.lock().has_active_soft_refs() {
        return false;
    }
    flush_tlab_allocation_batch(thread, shared);
    thread.tlab.retire();
    // gc-common w1-d (2026-09-23): on Generational this collection must reach
    // the OLD generation, or a condemned soft referent that was tenured —
    // which is where a long-lived soft cache lives — is judged to have
    // survived (`watched_pre_gc_addr_survived` answers an old-gen address
    // "survived" unless old-gen storage was reclaimed this cycle), gets its
    // slot restored, and the caller throws `OutOfMemoryError` holding it.
    // `maybe_gc_forced` never asks for a major cycle on its own; the same
    // thread-local request `System.gc()` makes (`force_gc_from_native`) is
    // what makes the collector run one. Thread-local, and consumed by the very
    // next Generational collection on this thread — this one, unless it loses
    // the STW race, in which case it rides this thread's next collection,
    // which is also an allocation-failure retry. G1 got the equivalent above
    // (`last_ditch_reclaim` arms the remark); ZGC's collection is whole-heap.
    //
    // LOSING THE RACE LOST THE RULE (gc-common w2-a, 2026-09-23). Both halves
    // of this rung are thread-local — the soft-clear arming
    // (`with_last_ditch_soft_clear`) and the major request — so only a
    // collection THIS thread initiates honours them. On the path to
    // `OutOfMemoryError` every allocating thread is racing for the same pause,
    // and a thread that lost it took part in someone else's ordinary
    // collection: no soft reference was condemned, the caller threw OOM with
    // its soft cache intact, and the unconsumed major request leaked into the
    // thread's next unrelated collection. Retry a bounded number of times,
    // exactly as `System.gc()` does (every lost attempt lets another thread's
    // pause finish, so this cannot livelock), and withdraw the request if every
    // attempt lost.
    //
    // gc-common w5-a (`handoff-w4d-last-ditch-soft-clear-per-vm-request`,
    // residuals §4): the soft-clear half is now ALSO recorded per VM, so the
    // next collection applies it whichever thread initiates it -- including
    // another thread's pause that wins every attempt below. Consumed exactly
    // once by `weakref_null_referents_pre_gc` (which also consumes it when
    // this thread's own attempt wins, so no later ordinary collection is
    // affected). The major-GC request stays thread-local: it is withdrawn
    // below when every attempt lost.
    shared
        .mem
        .ref_processor
        .lock()
        .request_clear_all_soft_refs();
    const LAST_DITCH_ATTEMPTS: u32 = 4;
    let generational = shared.mem.heap.is_generational();
    let mut collected = false;
    for _attempt in 0..LAST_DITCH_ATTEMPTS {
        if generational {
            cratonvm_gc::gc_quiescence::request_major_gc();
        }
        collected = crate::runtime::interpreter::with_last_ditch_soft_clear(|| {
            maybe_gc_forced_collected(shared, thread, "last-ditch-soft-refs")
        });
        if collected {
            break;
        }
    }
    if !collected && generational {
        let _ = cratonvm_gc::gc_quiescence::take_major_gc_request();
    }
    collected
}

/// Last-ditch G1 full marking cycle before declaring OutOfMemoryError.
///
/// Young/mixed pauses reclaim only collection-set regions; dead Old regions
/// and dead humongous spans are reclaimed exclusively by a completed mark
/// cycle's cleanup phase (`reclaim_dead_humongous_spans_locked`). When an
/// allocation still fails after the forced young GC, the heap may simply be
/// full of *unmarked dead* Old/humongous data — run one complete cycle
/// synchronously (start → drain → final remark → cleanup) and let the caller
/// retry the allocation once more before throwing OOM. Mirrors HotSpot's
/// last-ditch full GC on allocation failure. Also `System.gc()`'s G1 cycle
/// (`force_gc_from_native`).
///
/// No-op on non-G1 backends.
///
/// # Bounded, parked, and it finishes the cycle (gc-common w3-a, 2026-09-23)
///
/// This used to YIELD-SPIN until the background marker quiesced, for up to two
/// seconds, and then give up silently with the cycle still open. The
/// orchestrator's wave-2 battery caught a failing G1 `RefProbe` run in which
/// every allocation attempt on the exhausted heap was 8-16 s apart, all of it
/// CPU: one failed allocation reaches this function more than once (the
/// ladder's `last_ditch_reclaim`, then the OOM path's own ladder), and each
/// visit spun its full two seconds on a cycle that never quiesced. Three
/// changes:
///
/// 1. **The quiescence wait is short and bounded** —
///    [`FORCE_FULL_QUIESCE_WAIT`] — and after it the final remark runs anyway.
///    Quiescence is a latency preference, not a correctness gate:
///    `VmHeap::g1_final_remark_and_cleanup` stops and joins the marker, then
///    drains the gray set to a fixed point itself, inside the pause. So a
///    marker that cannot converge (more work published by every young pause
///    of a thrashing heap) no longer costs the caller its cycle; it costs a
///    longer remark, which on the path to `OutOfMemoryError` (or in an
///    explicit `System.gc()`) is exactly HotSpot's full-GC trade.
/// 2. **It parks instead of spinning** ([`LastDitchBackoff`]): a few yields,
///    then sleeps growing to 1 ms, while still taking part in every pause
///    another thread requests (`safepoint_check` each turn).
/// 3. **A cycle it could not complete is reported** — only possible now when
///    every attempt for [`FORCE_FULL_CYCLE_BUDGET`] lost its STW request to
///    another initiator — with the reason, instead of a `debug!` nobody sees.
///
/// Returns as soon as THIS call ran the remark + cleanup of a cycle that
/// STARTED after the call, or observed another thread close such a cycle.
///
/// # A cycle already open on entry is finished, then a fresh one runs
///
/// A cycle's SATB snapshot keeps everything that was live when IT started. A
/// cycle opened by the IHOP trigger while the program was still filling the
/// heap therefore keeps the very data the program has since dropped, and its
/// cleanup reclaims none of it: `NativeFactoryReclaimProbe -Xmx128m` saw the
/// last-ditch cleanup end at 33 Free / 87 Old in about half its rounds (the
/// rounds whose fill had crossed IHOP) and threw `OutOfMemoryError`, against
/// 105-121 Free in the others. So the open cycle is finished (it must be,
/// before another can start) and one more is driven from a snapshot taken
/// after the allocation failed -- HotSpot's full collection on this path marks
/// from scratch too. `CRATONVM_G1_FORCE_FULL_FRESH_CYCLE=0` restores the
/// finish-the-open-cycle-only behaviour.
pub(crate) fn g1_force_full_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    let started_at = std::time::Instant::now();
    let deadline = started_at + FORCE_FULL_CYCLE_BUDGET;
    let quiesce_by = started_at + FORCE_FULL_QUIESCE_WAIT;
    let mut backoff = LastDitchBackoff::default();
    // A cycle already mid-flight is finished first; its snapshot predates
    // this request, so a fresh one follows (see the doc above).
    let mut saw_active = shared.mem.heap.g1_is_marking_active();
    let mut need_fresh = saw_active && g1_force_full_fresh_cycle();
    // LANE W7-D — this door is counted, but NOT through `g1_drive_mark_cycle`,
    // and it counts ONE `waiting` per CALL rather than one per loop turn, so
    // `visits` means the same thing in every row of the door census.
    let mut waited = false;
    let mut lost_races = 0u32;
    loop {
        if shared.mem.heap.g1_is_marking_active() {
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
                    if !quiesced {
                        let waited_ms = elapsed_ms(started_at);
                        tracing::debug!(
                            waited_ms,
                            "[G1] last-ditch full cycle: the concurrent marker had not \
                             converged; its closure was finished inside the remark pause"
                        );
                    }
                    if need_fresh {
                        need_fresh = false;
                        saw_active = false;
                        waited = false;
                        continue;
                    }
                    return;
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
            if need_fresh {
                need_fresh = false;
                saw_active = false;
                waited = false;
                continue;
            }
            return;
        } else {
            // Not started yet (or the initial-mark STW lost its race —
            // g1_concurrent_mark_cycle returns without activating in that
            // case). Start/retry it.
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
        if std::time::Instant::now() >= deadline {
            report_forced_cycle_incomplete(shared, started_at, saw_active, lost_races);
            return;
        }
    }
}

/// `CRATONVM_G1_FORCE_FULL_FRESH_CYCLE=0` -- let [`g1_force_full_cycle`] return
/// after finishing a cycle that was already open on entry, without driving one
/// from a fresh snapshot (the behaviour before 2026-09-30). Default on.
fn g1_force_full_fresh_cycle() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_G1_FORCE_FULL_FRESH_CYCLE")
            .and_then(|v| v.into_string().ok())
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "false" || v == "off" || v == "no")
            })
            .unwrap_or(true)
    })
}

/// How long [`g1_force_full_cycle`] lets the background marker converge before
/// it runs the final remark anyway (which then finishes the closure inside the
/// pause). One marker poll interval (`g1_concurrent::WORKER_POLL_MS`): a
/// marker that has not converged by then is being fed faster than it drains.
const FORCE_FULL_QUIESCE_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// Upper bound on one [`g1_force_full_cycle`] call. Since w3-a it is reached
/// only when the call's STW requests keep losing to other initiators.
const FORCE_FULL_CYCLE_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// The wait between two turns of [`g1_force_full_cycle`]: a few yields (a
/// quiescence or a competing pause usually resolves within microseconds), then
/// sleeps doubling from 50 µs to 1 ms. Replaces an unbounded `yield_now` spin,
/// which kept a core busy for the whole wait.
#[derive(Default)]
struct LastDitchBackoff {
    turns: u32,
}

impl LastDitchBackoff {
    const YIELD_TURNS: u32 = 8;

    /// The sleep for the given turn, `None` for a yield. Split out for the test.
    fn pause_for(turns: u32) -> Option<std::time::Duration> {
        if turns <= Self::YIELD_TURNS {
            return None;
        }
        // 50 µs << k, capped at 1 ms.
        let k = (turns - Self::YIELD_TURNS - 1).min(5);
        Some(std::time::Duration::from_micros((50u64 << k).min(1_000)))
    }

    fn snooze(&mut self) {
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
fn report_forced_cycle_incomplete(
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
fn elapsed_ms(t0: std::time::Instant) -> u64 {
    u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod last_ditch_backoff_tests {
    use super::LastDitchBackoff;
    use std::time::Duration;

    /// Yields first, then sleeps that grow and are capped at 1 ms: the wait
    /// parks rather than spins, and never oversleeps a quiescence by more
    /// than a millisecond.
    #[test]
    fn the_backoff_yields_then_parks_with_a_capped_sleep() {
        for t in 1..=LastDitchBackoff::YIELD_TURNS {
            assert_eq!(LastDitchBackoff::pause_for(t), None);
        }
        let first = LastDitchBackoff::YIELD_TURNS + 1;
        assert_eq!(LastDitchBackoff::pause_for(first), Some(Duration::from_micros(50)));
        assert_eq!(LastDitchBackoff::pause_for(first + 1), Some(Duration::from_micros(100)));
        let mut prev = Duration::ZERO;
        for t in first..first + 64 {
            let d = LastDitchBackoff::pause_for(t).expect("sleeps past the yield turns");
            assert!(d >= prev && d <= Duration::from_millis(1));
            prev = d;
        }
        assert_eq!(prev, Duration::from_millis(1));
        assert_eq!(LastDitchBackoff::pause_for(u32::MAX), Some(Duration::from_millis(1)));
    }
}

#[cfg(test)]
mod root_snapshot_screen_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::runtime::frame::Frame;
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;

    /// The operand-stack half of a frame's root scan must not be screened
    /// more strictly than the locals half.
    ///
    /// `Frame::scan_local_objects` screens a local with `is_heap_addr`
    /// (alignment + arena containment) on purpose: `is_object_address` is a
    /// full header probe and rejects a young / mid-init object whose header it
    /// cannot yet vouch for, and dropping such a root reclaims a
    /// still-referenced object. `ValueStack::scan_object_refs` was changed for
    /// the same reason. `scan_frame_roots` then re-screened only that scan's
    /// output with the strict probe, which put both fixes back — invisibly,
    /// because the two screens agree on every HEALTHY object.
    ///
    /// So the test builds an address where they provably disagree, asserts the
    /// disagreement (a vacuous pass is the failure mode that matters here),
    /// and then asserts the scan keeps it from BOTH slot kinds.
    #[test]
    fn operand_stack_roots_use_the_same_screen_as_locals() {
        // The collector is PINNED, not defaulted. The disagreement this test
        // manufactures is generational-specific: it corrupts the header's
        // `ObjectKind` byte so the strict probe rejects the address while
        // arena containment still accepts it. On ZGC `is_object_address` is a
        // registry-base lookup, not a header-tag probe, so the corruption does
        // not move it and the precondition assert below fails — which is what
        // happened when the default collector became `Zgc` on 2026-08-10.
        //
        // Left as a generational test rather than generalised, deliberately:
        // whether the operand-stack and locals screens also agree under ZGC's
        // registry-based probe is a real and separate question, and this
        // test's setup cannot ask it. It is not covered here.
        let cfg = VmConfig {
            gc_algorithm: crate::config::GcAlgorithm::Generational,
            ..VmConfig::default()
        };
        let shared = SharedVm::new(cfg);
        let heap = &shared.mem.heap;

        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as usize;
        assert!(
            heap.is_object_address(addr).is_some(),
            "a freshly allocated object must pass the strict probe — otherwise \
             the disagreement asserted below would not be the one this test means"
        );

        // Make the strict probe reject it while arena containment still holds:
        // an out-of-range `ObjectKind` discriminant is exactly what
        // `is_object_address` screens for, and is what a mid-init or
        // conservatively-reached address looks like to it.
        // SAFETY: `addr` is a live object header in a mapped arena; the kind
        // tag is a single byte at a fixed in-bounds header offset.
        unsafe {
            *((addr + cratonvm_types::KIND_TAGS_BYTE_OFFSET) as *mut u8) = 0xEE;
        }
        assert!(
            heap.is_object_address(addr).is_none(),
            "precondition: the strict probe must now reject this address"
        );
        assert!(
            heap.is_heap_addr(addr).is_some(),
            "precondition: arena containment must still accept it"
        );

        // `aload_0; return` — local 0 is live at pc 0, so the per-bci liveness
        // mask keeps it and the two slot kinds are directly comparable.
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x2a, 0xb1],
            vec![],
            8,
            4,
            &[],
        );
        frame.set_local(0, Value::Object(Some(obj)));
        frame.stack.push(Value::Object(Some(obj))).unwrap();

        let mut roots = Vec::new();
        scan_frame_roots(&frame, &mut roots, heap, false);
        let hits = roots.iter().filter(|r| r.as_ptr() as usize == addr).count();
        assert_eq!(
            hits, 2,
            "both the local and the operand-stack slot must be rooted; {hits} of 2 \
             survived, so the operand-stack post-filter is dropping a root the \
             locals scan keeps"
        );
    }
}

/// DBG (CRATONVM_DBG_DM): trace every stage of the direct-memory
/// reclamation chain -- Cleaner discovery, action emission, the drain, and
/// `Bits.reserveMemory`'s reclaim-and-retry. Default-off; when unset this is
/// one cached `OnceLock` read, NOT a per-call `runtime_var_os` (that takes
/// the process-env lock, and this sits on the post-GC path).
pub(super) fn dm_dbg_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DM").is_some())
}

/// `run_cleaner_actions` entries that got past the exclusion switches.
static DM_CLEANER_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// ...of those, the ones that bailed because a JIT borrow was live.
static DM_CLEANER_JIT_BLOCKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// ...the ones that reached the drain and found nothing queued.
static DM_CLEANER_EMPTY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Cleaner actions actually run.
static DM_CLEANER_DRAINED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
pub(super) fn run_cleaner_actions(shared: &SharedVm, thread: &mut JvmThread) {
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
pub(super) fn run_cleaner_actions_forced(shared: &SharedVm, thread: &mut JvmThread) {
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

#[cfg(test)]
mod default_field_init_tests {
    //! G56-1 — the premise `init_primitive_fields` used to rest on, and the
    //! behaviours the new reference arm must not move.
    //!
    //! The heap tests pin `GcAlgorithm::Generational` deliberately, exactly as
    //! `root_snapshot_screen_tests` does and for a related reason: the claim
    //! under test is about the LEGACY 16-byte `Value` cell's decode rule, which
    //! is a property of `gen_heap::read_slot`. Whether ZGC's and G1's own slot
    //! encodings answer the same way is a real and separate question, and this
    //! fixture cannot ask it.

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;

    fn generational_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    /// The JVMS §2.3 default table, spelled out. Byte for byte, so a future
    /// edit that (say) folds `Z` in with `J` is red here and not in a vector.
    #[test]
    fn every_jvm_field_descriptor_gets_its_spec_default() {
        for b in *b"IBCSZ" {
            assert_eq!(
                jvm_default_for_descriptor(b),
                Value::Int(0),
                "the int family (JVMS 2.3.1) all live in Value::Int"
            );
        }
        assert_eq!(jvm_default_for_descriptor(b'J'), Value::Long(0));
        assert_eq!(jvm_default_for_descriptor(b'F'), Value::Float(0.0));
        assert_eq!(jvm_default_for_descriptor(b'D'), Value::Double(0.0));
        // The arm this record exists for. Before 2026-08-17 both of these
        // answered "no write needed" and the slot kept its raw zero.
        assert_eq!(
            jvm_default_for_descriptor(b'L'),
            Value::Object(None),
            "a reference field's default is null, and null has to be WRITTEN"
        );
        assert_eq!(
            jvm_default_for_descriptor(b'['),
            Value::Object(None),
            "an array field is a reference field"
        );
    }

    /// The whole byte space, against the `gc` crate's own table.
    ///
    /// `alloc_object_with_descriptors` is the other allocation entry point that
    /// writes defaults, and it composes `heap::default_value_for_descriptor(b)`
    /// with `.unwrap_or(Value::Object(None))`. If the two ever disagree, an
    /// object's field defaults depend on which allocator ran — the class of
    /// divergence this record was opened to close. Sweeping all 256 bytes also
    /// covers the malformed-descriptor fall-open, which
    /// `f.descriptor.as_bytes().first()` can genuinely produce.
    #[test]
    fn the_two_allocation_entry_points_agree_on_all_256_descriptor_bytes() {
        for b in 0u8..=255 {
            let theirs =
                cratonvm_gc::heap::default_value_for_descriptor(b).unwrap_or(Value::Object(None));
            assert_eq!(
                jvm_default_for_descriptor(b),
                theirs,
                "descriptor byte {b:#04x}: init_primitive_fields and \
                 alloc_object_with_descriptors must not disagree"
            );
        }
    }

    /// The expired premise, asserted directly.
    ///
    /// The comment this record removes said reference slots were "already
    /// Object(None) from zero memory". `Value` is `#[repr(u32)]` with `Int = 0`
    /// and `Object = 4`, so the all-zero cell decodes as `Int(0)`. If the first
    /// assertion below ever fails in the direction of null, the niche was
    /// reverted and the reference write becomes redundant rather than
    /// load-bearing — which is worth being told.
    #[test]
    fn zero_memory_does_not_decode_as_null_which_is_why_the_write_exists() {
        let shared = generational_vm();
        let heap = &shared.mem.heap;
        let obj = heap.alloc_object(ClassId::new(0), 2);

        assert_eq!(
            heap.get_field(obj, 0),
            Value::Int(0),
            "a freshly allocated, never-written slot reads as Int(0) -- the \
             R-niche decode rule (gen_heap::read_slot), and the entire reason \
             1,105 of 1,120 instrument events existed"
        );
        assert_ne!(
            heap.get_field(obj, 0),
            Value::Object(None),
            "if this ever passes, the zero-bits-are-null shortcut is back"
        );

        heap.set_field(obj, 0, Value::Object(None));
        assert_eq!(
            heap.get_field(obj, 0),
            Value::Object(None),
            "and an EXPLICIT null is a different bit pattern that reads back as \
             null -- so the two states are distinguishable, and writing one is \
             not a no-op"
        );
    }

    /// ...and the fix changes no answer, which is why it is safe.
    ///
    /// The descriptor-aware read is the one that was reporting the loss. Both
    /// slot shapes answer `Object(None)` through it; only one of them fires the
    /// G30 instrument on the way. That is the whole delta: signal, not
    /// behaviour.
    #[test]
    fn a_raw_zero_and_an_explicit_null_read_identically_through_the_descriptor() {
        let shared = generational_vm();
        let heap = &shared.mem.heap;
        let obj = heap.alloc_object(ClassId::new(0), 2);

        // slot 0: left as the allocator produced it (the BEFORE state).
        // slot 1: written the way init_primitive_fields now writes it (AFTER).
        heap.set_field(obj, 1, Value::Object(None));

        for (slot, what) in [(0usize, "raw zero"), (1usize, "explicit null")] {
            for desc in *b"L[" {
                assert_eq!(
                    heap.get_field_as(obj, slot, desc),
                    Value::Object(None),
                    "{what} at a '{}' slot must read as null either way",
                    desc as char,
                );
            }
        }
    }

    /// `gc_alloc_object` answers `has_finalizer` and writes every instance
    /// default under ONE class-manager read acquisition (it used to take two).
    /// Pins the two halves that merge must not lose: the per-descriptor
    /// defaults across a superclass chain (statics skipped, superclass slots
    /// first), and the finalizer registration.
    #[test]
    fn gc_alloc_object_writes_every_default_and_registers_the_finalizable() {
        use crate::classloading::{Class, ClassLoaderId, ClassState};
        use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
        use cratonvm_reader::class_file_version::ClassFileVersion;
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        use cratonvm_reader::field::ClassFileField;

        let shared = generational_vm();
        let field = |name: &str, desc: &str, flags: FieldAccessFlags| ClassFileField {
            access_flags: flags,
            name: std::sync::Arc::from(name),
            descriptor: std::sync::Arc::from(desc),
            attributes: vec![],
        };
        let add = |name: &str,
                   superclass: Option<ClassId>,
                   fields: Vec<ClassFileField>,
                   first_field_index: usize,
                   num_total_fields: usize,
                   has_finalizer: bool| {
            let mut cm = shared.classes.class_manager.write();
            let id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id,
                loader_id: ClassLoaderId::Application,
                name: cratonvm_types::intern_arc(name),
                source_file: None,
                version: ClassFileVersion::JAVA_8,
                state: ClassState::Initialized,
                initializing_thread: None,
                constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
                access_flags: ClassAccessFlags::empty(),
                superclass,
                interfaces: vec![],
                fields,
                methods: vec![],
                first_field_index,
                num_total_fields,
                bootstrap_methods: vec![],
                signature: None,
                annotations: Vec::new(),
                nest_host: None,
                nest_members: Vec::new(),
                record_components: Vec::new(),
                permitted_subclasses: Vec::new(),
                inner_classes: Vec::new(),
                enclosing_method: None,
                hidden: false,
                module_name: None,
                origin: cratonvm_classloading::ClassOrigin::default(),
                has_finalizer,
                code_source: None,
                array_info: None,
                record_object_methods: std::sync::atomic::AtomicU8::new(0),
                init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            });
            cm.register_class_name(ClassLoaderId::Application, name, id);
            id
        };
        let inst = FieldAccessFlags::PRIVATE;
        let stat = FieldAccessFlags::PRIVATE | FieldAccessFlags::STATIC;
        // Super: slots 0..2. A static in the middle must not take a slot.
        let base = add(
            "cratonvm/test/AllocDefaultsBase",
            None,
            vec![
                field("i", "I", inst),
                field("s", "J", stat),
                field("o", "Ljava/lang/Object;", inst),
            ],
            0,
            2,
            false,
        );
        // Sub: slots 2..6, and it overrides `finalize()`.
        let sub = add(
            "cratonvm/test/AllocDefaultsSub",
            Some(base),
            vec![
                field("j", "J", inst),
                field("f", "F", inst),
                field("d", "D", inst),
                field("a", "[I", inst),
            ],
            2,
            6,
            true,
        );

        let mut thread = JvmThread::new(crate::ThreadId(0), "test");
        let obj = gc_alloc_object(&shared, &mut thread, sub, 6).expect("a fresh heap allocates");
        let heap = &shared.mem.heap;
        assert_eq!(heap.get_field(obj, 0), Value::Int(0), "base.i");
        assert_eq!(heap.get_field(obj, 1), Value::Object(None), "base.o");
        assert_eq!(heap.get_field(obj, 2), Value::Long(0), "sub.j");
        assert_eq!(heap.get_field(obj, 3), Value::Float(0.0), "sub.f");
        assert_eq!(heap.get_field(obj, 4), Value::Double(0.0), "sub.d");
        assert_eq!(heap.get_field(obj, 5), Value::Object(None), "sub.a");
        assert!(
            finalizable_roots(&shared).contains(&(obj.as_ptr() as usize)),
            "a class with `has_finalizer` must register every instance"
        );

        let plain = gc_alloc_object(&shared, &mut thread, base, 2).expect("allocates");
        assert!(
            !finalizable_roots(&shared).contains(&(plain.as_ptr() as usize)),
            "a class without a finalizer must not be registered"
        );

        // Warm: with the allocation-recipe cache on, the first `new` of `sub`
        // published its recipe and this one is answered from it, lock-free.
        // Same defaults, same registration.
        let again = gc_alloc_object(&shared, &mut thread, sub, 6).expect("allocates");
        // `base.i` is NOT stored on the warm path (an int-family default is the
        // zeroed body itself); it must still read back as `Int(0)`.
        assert_eq!(heap.get_field(again, 0), Value::Int(0), "warm base.i");
        assert_eq!(heap.get_field(again, 1), Value::Object(None), "warm base.o");
        assert_eq!(heap.get_field(again, 2), Value::Long(0), "warm sub.j");
        assert_eq!(heap.get_field(again, 3), Value::Float(0.0), "warm sub.f");
        assert_eq!(heap.get_field(again, 4), Value::Double(0.0), "warm sub.d");
        assert_eq!(heap.get_field(again, 5), Value::Object(None), "warm sub.a");
        assert!(finalizable_roots(&shared).contains(&(again.as_ptr() as usize)));
        if crate::jit::alloc_class_cache::alloc_class_cache_enabled() {
            use crate::jit::alloc_class_cache::PrimKind;
            let info = shared
                .jit
                .jit_alloc_class_cache
                .get(sub.as_u32())
                .expect("the first `new` publishes the recipe");
            assert!(info.has_finalizer);
            // Walk order: the class itself, then its superclass; statics skipped.
            assert_eq!(
                &*info.prim_inits,
                &[
                    (2, PrimKind::Long),
                    (3, PrimKind::Float),
                    (4, PrimKind::Double),
                    (0, PrimKind::Int)
                ]
            );
            assert_eq!(&*info.ref_inits, &[5, 1]);
        }
    }

    /// The premise of `compact_body_holds_defaults` (lock-free `new` stage 3,
    /// wave 16): at every compact storage kind, the recipe's default for that
    /// kind is written as zero bytes, and zero bytes read back as that
    /// default. So skipping the stores on a zeroed compact body changes no
    /// byte. If a storage kind ever gains a non-zero default encoding (a
    /// null that is not 0), this fails and the skip must go.
    #[test]
    fn compact_zero_body_already_holds_every_recipe_default() {
        use crate::jit::alloc_class_cache::PrimKind;
        use cratonvm_types::FieldStorageKind as K;
        use std::sync::atomic::Ordering;
        for desc in *b"ZBCSIJFDL[" {
            let storage = K::from_descriptor_byte(desc).expect("a field descriptor byte");
            // The default the recipe (`ClassAllocInfo::build`) assigns.
            let default =
                PrimKind::of_descriptor(desc).map_or(Value::Object(None), PrimKind::default_value);
            // One naturally aligned, zeroed slot of up to 8 bytes.
            let mut cell = [0u64; 1];
            let ptr = cell.as_mut_ptr() as *mut u8;
            // SAFETY: `ptr` is an 8-byte-aligned, 8-byte, exclusively owned
            // buffer, which covers every storage kind's size and alignment.
            let read =
                unsafe { cratonvm_types::read_compact_field(ptr, storage, Ordering::Relaxed) };
            assert_eq!(
                read, default,
                "zero bytes at {storage:?} must read as the default"
            );
            // SAFETY: as above.
            unsafe {
                cratonvm_types::write_compact_field(ptr, storage, default, Ordering::Relaxed)
            };
            assert_eq!(
                cell[0], 0,
                "the default at {storage:?} (descriptor '{}') must be stored as zero bytes",
                desc as char
            );
        }
    }

    /// `compact_body_holds_defaults` answers from the reserved size exactly as
    /// `shape_of_reserved` stamps the header: a legacy reservation keeps the
    /// stores, a smaller (compact) one skips them.
    #[test]
    fn only_a_compact_reservation_skips_the_default_stores() {
        use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
        let legacy = HEADER_SIZE + 3 * SLOT_SIZE;
        assert!(!compact_body_holds_defaults(3, legacy));
        // A zero-field object reads as legacy, and has nothing to store.
        assert!(!compact_body_holds_defaults(0, HEADER_SIZE));
        // A compact body packs each field to at most 8 bytes after an 8-byte
        // header: strictly smaller than the legacy reservation.
        assert_eq!(
            compact_body_holds_defaults(3, 8 + 3 * 8),
            COMPACT_BODY_DEFAULT_SKIP_ENABLED
        );
    }

    /// JVMS 6.5 `new`: an interface or abstract class answers
    /// `InstantiationError` with HotSpot's external-name message; a concrete
    /// class, and a compatibility stub whatever its guessed flags, answer
    /// nothing.
    #[test]
    fn new_refuses_an_abstract_or_interface_class_but_not_a_stub() {
        use crate::classloading::{Class, ClassLoaderId, ClassState};
        use cratonvm_classloading::ClassOrigin;
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        use cratonvm_reader::class_file_version::ClassFileVersion;
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

        let class = |name: &str, access_flags: ClassAccessFlags, origin: ClassOrigin| Class {
            id: ClassId::new(1),
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin,
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        };
        let real = ClassOrigin::default;
        let stub = || ClassOrigin::CompatibilityStub {
            reason: std::sync::Arc::from("test"),
        };
        let concrete = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
        let abstract_ =
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER | ClassAccessFlags::ABSTRACT;
        let interface =
            ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;

        assert_eq!(
            new_instantiation_refusal(&class("p/q/Concrete", concrete, real())),
            None
        );
        assert_eq!(
            new_instantiation_refusal(&class("p/q/Abs", abstract_, real())),
            Some(("java/lang/InstantiationError", "p.q.Abs".to_string()))
        );
        assert_eq!(
            new_instantiation_refusal(&class("p/q/Iface", interface, real())),
            Some(("java/lang/InstantiationError", "p.q.Iface".to_string()))
        );
        assert_eq!(
            new_instantiation_refusal(&class("p/q/Guessed$Iface", interface, stub())),
            None,
            "a stub's flags are guessed and must not refuse a `new`"
        );
    }

    /// `HashMap.table` must still degrade to null.
    ///
    /// Pinned in `gc/src/heap.rs` by
    /// `the_hashmap_table_degrade_to_null_is_pinned` against the test-only
    /// `Heap`; this is the same three values through the LIVE `VmHeap`
    /// dispatch, so the guarantee is also asserted on the path a running VM
    /// takes. `HashMap.resize()` reads `(oldTab == null) ? 0 : oldTab.length`,
    /// so refusing or boxing the store breaks resize outright.
    #[test]
    fn the_hashmap_table_degrade_to_null_still_holds_through_vmheap() {
        let shared = generational_vm();
        let heap = &shared.mem.heap;
        let map = heap.alloc_object(ClassId::new(0), 3);

        for capacity in [Value::Int(16), Value::Int(1), Value::Long(64)] {
            heap.set_field_as(map, 2, capacity, b'[');
            assert_eq!(
                heap.get_field(map, 2),
                Value::Object(None),
                "a capacity written at an array-descriptor slot must degrade to \
                 null; {capacity:?} did not"
            );
            assert_eq!(heap.get_field_as(map, 2, b'['), Value::Object(None));
        }
    }

    /// The `Int(1)` enqueued sentinel must survive default-initialisation.
    ///
    /// `Reference.isEnqueued` was just repaired to accept BOTH the synthetic
    /// `Int(1)` sentinel and a live `ReferenceQueue.ENQUEUED` object (G49-1
    /// §4). The GC's auto-enqueue in this file writes that sentinel through the
    /// RAW setter, and default-initialisation now writes `Object(None)` into
    /// the same slot — earlier, at allocation. This pins the ordering: the
    /// sentinel is written second and wins, and a reference that was never
    /// enqueued reads as null rather than as the sentinel.
    #[test]
    fn the_enqueued_int_sentinel_outlives_the_default_null_written_at_alloc() {
        let shared = generational_vm();
        let heap = &shared.mem.heap;
        let reference = heap.alloc_object(ClassId::new(0), 3);

        // What init_primitive_fields now does for `queue : LReferenceQueue;`.
        heap.set_field(reference, 1, jvm_default_for_descriptor(b'L'));
        assert_ne!(
            heap.get_field(reference, 1),
            Value::Int(1),
            "a never-enqueued reference must not read as enqueued"
        );

        // What the post-GC auto-enqueue above does.
        heap.set_field(reference, 1, Value::Int(1));
        assert_eq!(
            heap.get_field(reference, 1),
            Value::Int(1),
            "the raw sentinel write must still win over the allocation-time \
             default -- un-fixing isEnqueued's synthetic arm is the failure \
             this guards"
        );
    }
}

/// r9w3 vmhelpers3 —
/// `perf-compact-tlab-planner-reads-an-env-var-per-allocation-20260918.md`.
#[cfg(test)]
mod r9w3_vmhelpers3_tlab_planner_tests {
    use super::*;

    /// The planner's shape counters are the exit report's, and are kept only
    /// under the switch that prints them. Planning with the switch off must not
    /// touch a process-global line.
    #[test]
    fn shape_counters_move_only_under_the_stats_switch() {
        if tlab_shape_stats_enabled() {
            // Someone ran the suite with CRATONVM_DBG_JIT_METHOD_STATS: the
            // counters are then supposed to move, and other tests move them.
            return;
        }
        let before = tlab_object_shape_counts();
        for n in 0..16usize {
            let _ = plan_tlab_object_shape_at(ClassId::new(1), n, tlab_site::JIT_NEW);
        }
        assert_eq!(
            tlab_object_shape_counts(),
            before,
            "an allocation with the stats switch off paid a shared lock xadd"
        );
    }

    /// The debug gate is read once; asking again answers the same thing and
    /// (by construction) does not go back to the environment.
    #[test]
    fn the_compact_tlab_debug_gate_is_latched() {
        let first = dbg_compact_tlab_enabled();
        for _ in 0..4 {
            assert_eq!(dbg_compact_tlab_enabled(), first);
        }
    }

    /// The planner's answer does not depend on whether the counters are kept.
    #[test]
    fn a_legacy_plan_is_the_uniform_cell_shape() {
        use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
        // A class id no test registers a compact layout for.
        let (total, body, flags) =
            plan_tlab_object_shape_at(ClassId::new(0x00FF_FFF0), 3, tlab_site::JIT_NEW);
        if flags == 0 {
            assert_eq!((total, body), (HEADER_SIZE + 3 * SLOT_SIZE, 0));
        }
    }
}

/// gen r4w3/alloc3 (2026-09-23): the opt-in JIT refill-gate probe size.
#[cfg(test)]
mod r4w3_alloc3_gate_probe_tests {
    use super::tlab_gate_bump_probe;

    /// OFF is the old gate exactly; ON probes for the floor, raised to the
    /// object in hand and capped at the request.
    #[test]
    fn the_gate_probes_the_full_request_unless_opted_in() {
        let k = 1024;
        for (floor, object, requested) in [(256, 48, 256 * k), (256, 24 * k, 8 * k), (8, 16, 8)] {
            assert_eq!(
                tlab_gate_bump_probe(false, floor, object, requested),
                requested
            );
        }
        assert_eq!(
            tlab_gate_bump_probe(true, 256, 48, 256 * k),
            256,
            "the floor"
        );
        assert_eq!(
            tlab_gate_bump_probe(true, 256, 16 * k, 256 * k),
            16 * k,
            "the object"
        );
        assert_eq!(
            tlab_gate_bump_probe(true, 256, 24 * k, 8 * k),
            8 * k,
            "never above the request"
        );
    }

    /// The switch is declared (so `with_thread_overrides` reaches it), parses
    /// `=0` as OFF, and defaults to its one constant (gen r4w4/alloc4: the
    /// default lives in `alloc_policy_defaults`, so a flip there is the whole
    /// change — this test follows it rather than pinning OFF).
    #[test]
    fn the_gate_switch_is_declared_and_off_by_default() {
        const KEY: &str = "CRATONVM_TLAB_GATE_BUMP_FLOOR";
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(&[(KEY, v)], || {
                cratonvm_types::flags().gc.tlab_gate_bump_floor
            })
        };
        assert_eq!(
            read(None),
            cratonvm_types::flags::alloc_policy_defaults::TLAB_GATE_BUMP_FLOOR
        );
        assert!(!read(Some("0")));
        assert!(read(Some("1")));
    }
}

#[cfg(test)]
mod w3d_finalizer_thread_tests {
    //! gc-common round 2026-09-23, wave 3, lane D: the reference-delivery
    //! thread's gate and its no-owner fallback. The thread itself needs a VM
    //! built by `Vm::new` and is exercised by the probes the w3-d report lists.

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::vm::SharedVm;

    fn fixture_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    /// Unset is the §12.6-only default; explicit off words are the pre-w3
    /// inline drain; any other value (the grouped spelling included) is the
    /// full hand-off. gc-common w4-d.
    #[test]
    fn the_flag_value_rule() {
        assert_eq!(finalizer_delivery_from(None), FinalizerDelivery::WhenLockHeld);
        for off in ["0", "false", "off", "no", " 0 "] {
            assert_eq!(
                finalizer_delivery_from(Some(off)),
                FinalizerDelivery::Inline,
                "{off:?} must read inline"
            );
        }
        for on in ["1", "true", "on", ""] {
            assert_eq!(
                finalizer_delivery_from(Some(on)),
                FinalizerDelivery::Always,
                "{on:?} must read the full hand-off"
            );
        }
    }

    /// A thread with nothing published in its JMX lock lists holds no
    /// user-visible lock, so the default policy keeps the inline drain for it
    /// and starts no thread; an unregistered thread answers the same.
    /// gc-common w4-d.
    #[test]
    fn a_lock_free_thread_keeps_the_inline_finalizer_drain() {
        let shared = fixture_vm();
        let thread = JvmThread::new(crate::ThreadId(4242), "w4d-lock-free");
        assert!(!thread_holds_user_locks(&shared, &thread));
        if finalizer_delivery_policy() == FinalizerDelivery::WhenLockHeld {
            assert!(!finalizer_thread_takes_finalizers(&shared, &thread));
        }
        assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
    }

    /// With no delivery thread, nothing waits on our lock.
    #[test]
    fn no_delivery_thread_is_never_blocked_on_the_caller() {
        let shared = fixture_vm();
        assert!(!delivery_thread_waits_on_our_lock(&shared, crate::ThreadId(1)));
    }

    /// A published monitor is a held lock; with a finalizer queued the
    /// default policy then asks for the delivery thread, and a VM that cannot
    /// start one (no owning `Arc`) falls back to the inline drain. The
    /// deadlock probe sees the delivery thread blocked entering the caller's
    /// monitor. The object is never dereferenced (the lists hold raw refs).
    /// gc-common w4-d.
    #[test]
    fn a_held_monitor_is_seen_and_a_blocked_delivery_thread_detected() {
        let shared = fixture_vm();
        let registry = &shared.threads.thread_registry;
        let caller = crate::ThreadId(4301);
        let delivery = crate::ThreadId(4302);
        registry.register(caller, "w4d-holder", None);
        registry.register(delivery, "w4d-delivery", None);
        // SAFETY: never dereferenced — only compared by address.
        let lock = unsafe { ObjectRef::from_raw(0x10_0000 as *mut u8) };
        let thread = JvmThread::new(caller, "w4d-holder");
        assert!(!thread_holds_user_locks(&shared, &thread));
        registry.complete_jmx_monitor_enter(caller, lock);
        assert!(thread_holds_user_locks(&shared, &thread));

        shared.mem.finalizer_thread.enqueue_unfinalized(0x20_0000);
        if finalizer_delivery_policy() == FinalizerDelivery::WhenLockHeld {
            assert!(
                !finalizer_thread_takes_finalizers(&shared, &thread),
                "no owner: the start fails and the inline drain is kept"
            );
            assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
        }

        shared.mem.finalizer_thread.set_delivery_thread(delivery.0);
        assert!(!delivery_thread_waits_on_our_lock(&shared, caller));
        registry.set_jmx_contended_monitor(delivery, lock);
        assert!(delivery_thread_waits_on_our_lock(&shared, caller));
        registry.remove_jmx_locked_monitor(caller, lock);
        assert!(!delivery_thread_waits_on_our_lock(&shared, caller));
        assert!(!thread_holds_user_locks(&shared, &thread));
    }

    /// A `SharedVm` with no owning `Arc` (every unit fixture) cannot hand a
    /// thread a handle to itself: the start refuses, hands the claim back, and
    /// registers nothing, so the caller keeps the inline drain.
    #[test]
    fn a_vm_without_an_owner_keeps_the_inline_drain() {
        let shared = fixture_vm();
        let alive_before = shared.threads.thread_registry.alive_count();
        assert!(!start_finalizer_thread(&shared));
        assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
        assert_eq!(shared.mem.finalizer_thread.delivery_thread(), None);
        assert_eq!(shared.threads.thread_registry.alive_count(), alive_before);
    }

    /// With no delivery thread to take them, a reference pass's queue
    /// wake-ups are dropped at once instead of being kept rooted (through
    /// `CleanerThread::pending_addresses`) forever.
    #[test]
    fn queue_wake_ups_with_no_taker_are_not_retained() {
        let shared = fixture_vm();
        gc_enqueued_queues_hand_off(&shared, &[0x1000, 0x2000]);
        assert!(!shared.mem.cleaner_thread.has_pending_work());
        assert!(shared.mem.cleaner_thread.pending_addresses().is_empty());
    }

    /// gc-common w5-d: queue wake-ups are recorded under the full hand-off,
    /// and under the default policy exactly for a `--jdk-only` VM — never for
    /// a `--compatible` one (whose natives poll), never on the inline
    /// bisection arm.
    #[test]
    fn queue_wake_ups_follow_the_vms_own_policy() {
        use super::FinalizerDelivery::{Always, Inline, WhenLockHeld};
        assert!(queue_wake_ups_recorded_for(Always, false));
        assert!(queue_wake_ups_recorded_for(Always, true));
        assert!(queue_wake_ups_recorded_for(WhenLockHeld, true));
        assert!(!queue_wake_ups_recorded_for(WhenLockHeld, false));
        assert!(!queue_wake_ups_recorded_for(Inline, true));
        assert!(!queue_wake_ups_recorded_for(Inline, false));
        // The fixture is `--compatible` (the embedding default): whatever the
        // process flag says, nothing is recorded unless the full hand-off is on.
        let shared = fixture_vm();
        assert!(!shared.config.is_jdk_only());
        assert_eq!(queue_wake_ups_recorded(&shared), finalizer_thread_enabled());
    }

    /// gc-common w5-d: `waitForReferenceProcessing()` answers `false` at once
    /// when nothing is queued or in flight — and, with the flag unset (the
    /// default this suite runs), always, so `--compatible` is unchanged. A
    /// fixture VM has no owner to start a delivery thread with either way.
    #[test]
    fn wait_for_reference_processing_is_false_with_no_delivery_work() {
        let shared = fixture_vm();
        let mut thread = JvmThread::new(crate::ThreadId(1), "w5d-wfrp");
        let start = std::time::Instant::now();
        assert!(!wait_for_reference_processing_for_runtime(&shared, &mut thread));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "no work: must not wait"
        );
        assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
    }

    /// gc-common w6-d / w7-d: GC notifications leave the allocating mutator
    /// under the full hand-off and, under the default policy, always for a
    /// `--jdk-only` VM and only under a held lock for a `--compatible` one;
    /// never on the inline bisection arm.
    #[test]
    fn gc_notifications_follow_the_vms_own_policy() {
        use super::FinalizerDelivery::{Always, Inline, WhenLockHeld};
        use super::GcNotificationDelivery as N;
        assert_eq!(gc_notification_delivery_for(Always, false), N::OffMutator);
        assert_eq!(gc_notification_delivery_for(Always, true), N::OffMutator);
        assert_eq!(gc_notification_delivery_for(WhenLockHeld, true), N::OffMutator);
        assert_eq!(gc_notification_delivery_for(WhenLockHeld, false), N::WhenLockHeld);
        assert_eq!(gc_notification_delivery_for(Inline, true), N::Inline);
        assert_eq!(gc_notification_delivery_for(Inline, false), N::Inline);
        let shared = fixture_vm();
        assert!(!shared.config.is_jdk_only());
        let expected = if finalizer_thread_enabled() {
            N::OffMutator
        } else if finalizer_delivery_policy() == WhenLockHeld {
            N::WhenLockHeld
        } else {
            N::Inline
        };
        assert_eq!(gc_notification_delivery(&shared), expected);
    }

    /// gc-common w7-d: under the `--compatible` lock-held rule, a lock-free
    /// collecting thread keeps its inline delivery and leaves no mark; a
    /// lock-holding one marks the hand-off, and when no delivery thread can
    /// be started (a fixture has no owning `Arc`) takes the mark back and
    /// keeps the inline delivery too. The lock is never dereferenced.
    #[test]
    fn compatible_gc_notifications_move_only_under_a_held_lock() {
        let shared = fixture_vm();
        let registry = &shared.threads.thread_registry;
        let tid = crate::ThreadId(4401);
        registry.register(tid, "w7d-allocator", None);
        let thread = JvmThread::new(tid, "w7d-allocator");
        let ft = &shared.mem.finalizer_thread;
        assert!(!gc_notifications_handed_off_under_lock(&shared, &thread));
        assert!(!ft.take_gc_notifications_handed_off(), "no lock: no mark");
        // SAFETY: never dereferenced — only compared by address.
        let lock = unsafe { ObjectRef::from_raw(0x10_0000 as *mut u8) };
        registry.complete_jmx_monitor_enter(tid, lock);
        assert!(
            !gc_notifications_handed_off_under_lock(&shared, &thread),
            "no owner: the start fails and the door delivers inline"
        );
        assert!(!ft.take_gc_notifications_handed_off(), "the mark is taken back");
        assert!(!ft.delivery_thread_claimed());
        // The delivery thread itself, holding a lock, keeps the batch's own
        // delivery and leaves the mark for its batch.
        ft.set_delivery_thread(tid.0);
        assert!(gc_notifications_handed_off_under_lock(&shared, &thread));
        assert!(ft.take_gc_notifications_handed_off());
        registry.remove_jmx_locked_monitor(tid, lock);
    }

    /// gc-common w36-e: the allocation-failure door hands queued notifications
    /// to a claimed delivery thread when the collecting thread holds a lock --
    /// the only door a compiled allocation loop reaches. Nothing queued, or no
    /// lock under the `--compatible` rule: nothing moves. Follows the latched
    /// process policy, as the neighbouring tests do.
    #[test]
    fn w36e_the_forced_door_hands_off_notifications_under_a_held_lock() {
        use cratonvm_gc::gc_metrics::{
            GcNotificationRecord, GC_ACTION_MINOR, JMX_SERIAL_YOUNG_COLLECTOR,
        };
        let shared = fixture_vm();
        let registry = &shared.threads.thread_registry;
        let tid = crate::ThreadId(4402);
        registry.register(tid, "w36e-allocator", None);
        let thread = JvmThread::new(tid, "w36e-allocator");
        let ft = &shared.mem.finalizer_thread;
        // A serving delivery thread (claimed: a request is a counter bump).
        assert!(ft.claim_delivery_thread_start());
        ft.set_delivery_thread(4403);
        // SAFETY: never dereferenced -- only compared by address.
        let lock = unsafe { ObjectRef::from_raw(0x10_0008 as *mut u8) };
        registry.complete_jmx_monitor_enter(tid, lock);
        // Nothing queued: nothing to hand off, no mark.
        assert!(!forced_door_hands_off_gc_notifications(&shared, &thread));
        assert!(!ft.take_gc_notifications_handed_off());

        let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap else {
            panic!("the fixture is Generational");
        };
        let queue = h.gc_notifications();
        assert!(queue.set_enabled(JMX_SERIAL_YOUNG_COLLECTOR, true));
        let now = std::time::Instant::now();
        queue.record(GcNotificationRecord {
            collector: JMX_SERIAL_YOUNG_COLLECTOR,
            action: GC_ACTION_MINOR,
            cause: "Allocation Failure",
            id: 1,
            start: now,
            end: now,
            timestamp_ms: 0,
            before: Vec::new(),
            after: Vec::new(),
        });
        assert!(queue.has_pending(), "the listener's bean is enabled");
        let policy = gc_notification_delivery(&shared);
        let handed = forced_door_hands_off_gc_notifications(&shared, &thread);
        assert_eq!(
            handed,
            policy != GcNotificationDelivery::Inline,
            "{policy:?}: a lock-holding forced door hands off unless delivery is inline"
        );
        assert_eq!(
            ft.take_gc_notifications_handed_off(),
            policy == GcNotificationDelivery::WhenLockHeld,
            "the --compatible rule marks the batch it hands over"
        );
        registry.remove_jmx_locked_monitor(tid, lock);
        if policy == GcNotificationDelivery::WhenLockHeld {
            assert!(
                !forced_door_hands_off_gc_notifications(&shared, &thread),
                "no lock: the next drain point delivers inline, as before"
            );
            assert!(!ft.take_gc_notifications_handed_off());
        }
    }

    /// gc-common w6-d: a `ReferenceQueue.remove` waiter may sleep on `lock`
    /// only once a delivery thread is claimed to deliver the recorded
    /// wake-ups; before that — and in a VM that records none — it polls.
    #[test]
    fn queue_wake_ups_are_delivered_only_with_a_claimed_delivery_thread() {
        let shared = fixture_vm();
        assert!(
            !queue_wake_up_delivery_serves(&shared),
            "no delivery thread has been claimed"
        );
        assert!(shared.mem.finalizer_thread.claim_delivery_thread_start());
        assert_eq!(
            queue_wake_up_delivery_serves(&shared),
            queue_wake_ups_recorded(&shared),
            "claimed: delivered exactly when this VM records its wake-ups"
        );
        shared.mem.finalizer_thread.release_delivery_thread_start();
        assert!(!queue_wake_up_delivery_serves(&shared));
    }
}

#[cfg(test)]
mod w4a_door_tests {
    //! gc-common round 2026-09-23, wave 4, lane A: the doors' hand-off of
    //! reference work and GC notifications. The delivery thread itself needs a
    //! VM built by `Vm::new` (see `w3d_finalizer_thread_tests`); these pin the
    //! paths a unit fixture can reach, which are the flag-off / no-owner ones.

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::vm::SharedVm;

    fn fixture_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    /// `maybe_gc`'s no-collection path consumes the "reference work queued
    /// elsewhere" hint exactly once, whether it drains inline (flag off) or
    /// hands the work to a delivery thread (flag on; a fixture VM has no owner
    /// to start one with, so it falls back to the inline drain). With nothing
    /// queued, the drain finds nothing to run.
    #[test]
    fn the_no_collection_path_consumes_the_ref_work_hint() {
        let shared = fixture_vm();
        let mut thread = JvmThread::new(crate::ThreadId(1), "w4a-door");
        shared.mem.gc_barrier.note_ref_work_queued();
        assert!(shared.mem.gc_barrier.ref_work_queued());
        drain_ref_work_queued_elsewhere(&shared, &mut thread);
        assert!(
            !shared.mem.gc_barrier.ref_work_queued(),
            "the hint must be lowered by the drain that took it"
        );
        // A second call is the one-load fast path.
        drain_ref_work_queued_elsewhere(&shared, &mut thread);
        assert!(!shared.mem.gc_barrier.ref_work_queued());
    }

    /// With nothing queued the doors keep their own (instant) inline
    /// notification call: no delivery thread is woken or started for an empty
    /// queue, whatever the flag says.
    #[test]
    fn an_empty_notification_queue_starts_no_delivery_thread() {
        let shared = fixture_vm();
        let thread = JvmThread::new(crate::ThreadId(1), "w4a-door");
        assert!(!super::super::gc_events::gc_notifications_pending(&shared));
        assert!(!gc_notifications_go_to_delivery_thread(&shared, &thread));
        assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
    }
}

#[cfg(test)]
mod w7g_door_tests {
    //! gc-common round 2026-09-23, wave 7, lane G: the GC doors after wave 6
    //! -- `System.gc()` coalescing (per VM) and the post-GC debug verifiers on
    //! every pause (`common-e-small-findings` items 7 and 1).

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use std::sync::atomic::Ordering;

    fn gen_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    fn source() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/runtime/interpreter/gc_and_alloc.rs"
        );
        std::fs::read_to_string(path)
            .expect("gc_and_alloc.rs is readable")
            .replace("\r\n", "\n")
    }

    /// The item starting at `sig` at the start of a line, up to the next
    /// column-0 closing brace (`\u{7d}`, see `w5a_pause_driver_tests::body`).
    fn body(src: &str, sig: &str) -> String {
        let start = src
            .find(&format!("\n{sig}"))
            .unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start + 1..];
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// Item 7: the rule is "at least one `System.gc()` collection COMPLETED
    /// since the call began" -- not "the counter moved by two", which would
    /// never coalesce the common case (N callers, one winner).
    #[test]
    fn a_system_gc_coalesces_only_on_a_later_completion() {
        assert!(!system_gc_satisfied(5, 5));
        assert!(system_gc_satisfied(5, 6));
        assert!(system_gc_satisfied(5, 9));
        let shared = gen_vm();
        assert!(!system_gc_completed_since(&shared, None));
        let base = system_gc_collections(&shared).load(Ordering::Relaxed);
        assert!(!system_gc_completed_since(&shared, Some(base)));
        system_gc_collections(&shared).fetch_add(1, Ordering::Relaxed);
        assert!(system_gc_completed_since(&shared, Some(base)));
    }

    /// Item 7: a caller whose own blocked-region flag is raised can be
    /// excluded from a pause's census, so it must never coalesce.
    #[test]
    fn a_blocked_caller_never_coalesces() {
        let shared = gen_vm();
        let thread = JvmThread::new(ThreadId(0), "w7g-blocked");
        assert!(system_gc_coalescing_baseline(&shared, &thread).is_some());
        thread
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        assert_eq!(system_gc_coalescing_baseline(&shared, &thread), None);
        thread
            .gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);
    }

    /// Item 7: every WON `System.gc()` counts once, on its own VM only (the
    /// counter is per VM, never a process static), and the allocation doors
    /// never count.
    #[test]
    fn each_won_system_gc_counts_once_on_its_own_vm() {
        let a = gen_vm();
        let b = gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w7g-sysgc");
        let a0 = system_gc_collections(&a).load(Ordering::Relaxed);
        let b0 = system_gc_collections(&b).load(Ordering::Relaxed);
        let cycles0 = a.mem.gc_cycle_count.load(Ordering::Relaxed);
        force_gc_from_native(&a, &mut thread);
        force_gc_from_native(&a, &mut thread);
        assert_eq!(system_gc_collections(&a).load(Ordering::Relaxed) - a0, 2);
        assert_eq!(system_gc_collections(&b).load(Ordering::Relaxed), b0);
        assert!(a.mem.gc_cycle_count.load(Ordering::Relaxed) >= cycles0 + 2);
        maybe_gc_forced_at(&a, &mut thread, "w7g-test");
        assert_eq!(system_gc_collections(&a).load(Ordering::Relaxed) - a0, 2);
        // The metaspace refusal's collection is the same door.
        force_gc_for_class_metadata(&a, &mut thread);
        assert_eq!(system_gc_collections(&a).load(Ordering::Relaxed) - a0, 3);
    }

    /// The metaspace refusal's collection runs none of `System.gc()`'s drains
    /// (it runs inside `defineClass`, holding the loader's lock); `System.gc()`
    /// keeps all three.
    #[test]
    fn the_class_metadata_collection_runs_no_drains() {
        let src = source();
        let meta = body(&src, "pub fn force_gc_for_class_metadata(");
        let full = body(&src, "fn full_gc_door(");
        for drain in [
            "run_finalizers_forced(",
            "run_cleaner_actions_forced(",
            "run_gc_notifications(",
        ] {
            assert!(!meta.contains(drain), "metadata collection runs `{drain}`");
            assert!(full.contains(drain), "System.gc() lost `{drain}`");
        }
        assert!(meta.contains("force_full_collection(shared, thread, GcDoor::MetadataThreshold)"));
        assert!(full.contains("force_full_collection(shared, thread, GcDoor::SystemGc)"));
    }

    /// Item 7, ordering: the bump is inside the pause (before the frozen peers
    /// resume and the world is released), only for the `System.gc()` door; and
    /// `force_gc_from_native` reads its baseline before its first attempt and
    /// checks it only after a LOST attempt.
    #[test]
    fn the_system_gc_count_is_bumped_before_release_and_read_before_the_first_attempt() {
        let src = source();
        let b = body(&src, "fn run_collection_pause(");
        let seal = b.find("gc_event_seal(shared, &mut gc_event)").expect("sealed");
        let bump = b
            .find("system_gc_collections(shared).fetch_add(1")
            .expect("bumps");
        let resume = b
            .find("retire_skip_spans_and_resume(shared, taken)")
            .expect("resumes");
        let release = b.find(".complete_gc(result.pointer_map)").expect("releases");
        assert!(seal < bump && bump < resume && resume < release);
        assert!(b[..bump].contains("GcDoor::SystemGc | GcDoor::MetadataThreshold"));

        let f = body(&src, "fn force_full_collection(");
        let baseline = f
            .find("system_gc_coalescing_baseline(shared, thread)")
            .expect("baseline");
        let first = f
            .find("for _attempt in 0..FORCE_GC_ATTEMPTS")
            .expect("attempt loop");
        let pause = f
            .find("run_collection_pause(shared, thread, door)")
            .expect("pause");
        let check = f
            .find("system_gc_completed_since(shared, coalesce_baseline)")
            .expect("coalescing check");
        assert!(baseline < first && first < pause && pause < check);
    }

    /// Item 1: the verifier set runs on every pause (no census-count gate
    /// around the call), and both halves of the heap-stale pair share one
    /// walkability gate.
    #[test]
    fn the_post_gc_verifiers_run_on_every_pause_under_one_walk_gate() {
        let src = source();
        let b = body(&src, "fn run_collection_pause(");
        assert!(
            !b.contains("if solo {\n        post_gc_debug_verifiers"),
            "the verifier set is back behind the census-count gate"
        );
        let pre = b
            .find("crate::memory::gc::verify_heap_object_fields_pre_gc(shared)")
            .expect("pre half");
        assert!(b[..pre].contains("if debug_heap_walks_permitted(shared, solo)"));
        let post = b.find("post_gc_debug_verifiers(").expect("post set");
        assert!(b[post..].contains("debug_heap_walks_permitted(shared, solo),"));
        let v = body(&src, "fn post_gc_debug_verifiers(");
        let gate = v.find("if heap_walks").expect("walkers gated");
        assert!(gate < v.find("validate_object_sizes(shared)").expect("validator"));
        assert!(
            gate < v
                .find("verify_heap_object_fields(shared, pointer_map)")
                .expect("stale verifier")
        );
    }

    /// Item 1: with no walker armed the gate is shut on every pause without a
    /// registry walk; with one armed a solo pause walks.
    #[test]
    fn the_walk_gate_is_shut_unless_a_walker_is_armed() {
        let shared = gen_vm();
        if debug_heap_walkers_armed() {
            assert!(debug_heap_walks_permitted(&shared, true));
        } else {
            assert!(!debug_heap_walks_permitted(&shared, true));
            assert!(!debug_heap_walks_permitted(&shared, false));
        }
    }
}

#[cfg(test)]
mod w8g_door_tests {
    //! gc-common round 2026-09-23, wave 8, lane G: `-XX:±DisableExplicitGC` and
    //! `-XX:±ExplicitGCInvokesConcurrent` at the `System.gc()` door
    //! (`common-w7g-explicit-gc-flags-are-silently-ignored`). The end-to-end
    //! check (JMX counts, a weak reference) is `tools/probes/ExplicitGcFlagsProbe.java`.

    use super::*;
    use crate::config::{ExplicitGcPolicy, GcAlgorithm, VmConfig};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use std::sync::atomic::Ordering;

    fn vm(gc_algorithm: GcAlgorithm, explicit_gc: ExplicitGcPolicy) -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm,
            explicit_gc,
            ..VmConfig::default()
        })
    }

    fn source() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/runtime/interpreter/gc_and_alloc.rs"
        );
        std::fs::read_to_string(path)
            .expect("gc_and_alloc.rs is readable")
            .replace("\r\n", "\n")
    }

    /// Same helper as `w7g_door_tests::body`.
    fn body(src: &str, sig: &str) -> String {
        let start = src
            .find(&format!("\n{sig}"))
            .unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start + 1..];
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// The whole rule: `Disabled` ignores the request; `Concurrent` collects
    /// as `Full` does (see `explicit_gc_action`'s doc for why).
    #[test]
    fn the_explicit_gc_rule_per_policy() {
        assert_eq!(explicit_gc_action(ExplicitGcPolicy::Full), ExplicitGcAction::Collect);
        assert_eq!(explicit_gc_action(ExplicitGcPolicy::Disabled), ExplicitGcAction::Ignore);
        assert_eq!(
            explicit_gc_action(ExplicitGcPolicy::Concurrent),
            ExplicitGcAction::Collect
        );
    }

    /// `-XX:+DisableExplicitGC`: `System.gc()` runs no collection and counts
    /// nothing, while the metaspace refusal's collection (not an explicit GC)
    /// still collects on the same VM.
    #[test]
    fn a_disabled_explicit_gc_collects_nothing_but_the_metadata_door_still_does() {
        let shared = vm(GcAlgorithm::Generational, ExplicitGcPolicy::Disabled);
        let mut thread = JvmThread::new(ThreadId(0), "w8g-disabled");
        let sys0 = system_gc_collections(&shared).load(Ordering::Relaxed);
        let cycles0 = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        for _ in 0..3 {
            force_gc_from_native(&shared, &mut thread);
        }
        assert_eq!(system_gc_collections(&shared).load(Ordering::Relaxed), sys0);
        assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), cycles0);
        force_gc_for_class_metadata(&shared, &mut thread);
        assert_eq!(system_gc_collections(&shared).load(Ordering::Relaxed), sys0 + 1);
        assert!(shared.mem.gc_cycle_count.load(Ordering::Relaxed) > cycles0);
    }

    /// The policy is per VM: a `Disabled` VM beside a default one does not
    /// disable the default one's `System.gc()`.
    #[test]
    fn the_policy_is_per_vm() {
        let off = vm(GcAlgorithm::Generational, ExplicitGcPolicy::Disabled);
        let on = vm(GcAlgorithm::Generational, ExplicitGcPolicy::Full);
        let mut off_thread = JvmThread::new(ThreadId(0), "w8g-per-vm-off");
        let mut on_thread = JvmThread::new(ThreadId(0), "w8g-per-vm-on");
        let off0 = system_gc_collections(&off).load(Ordering::Relaxed);
        let on0 = system_gc_collections(&on).load(Ordering::Relaxed);
        force_gc_from_native(&off, &mut off_thread);
        force_gc_from_native(&on, &mut on_thread);
        assert_eq!(system_gc_collections(&off).load(Ordering::Relaxed), off0);
        assert_eq!(system_gc_collections(&on).load(Ordering::Relaxed), on0 + 1);
    }

    /// `-XX:+ExplicitGCInvokesConcurrent` collects as the default does.
    #[test]
    fn concurrent_collects_as_full_does() {
        let shared = vm(GcAlgorithm::Generational, ExplicitGcPolicy::Concurrent);
        let mut thread = JvmThread::new(ThreadId(0), "w8g-concurrent-gen");
        let sys0 = system_gc_collections(&shared).load(Ordering::Relaxed);
        force_gc_from_native(&shared, &mut thread);
        assert_eq!(system_gc_collections(&shared).load(Ordering::Relaxed), sys0 + 1);
    }

    /// The door's shape: the policy is read per VM, `Disabled` returns before
    /// any drain and before `jdk.SystemGC`, and the metaspace door never reads
    /// the policy at all.
    #[test]
    fn disabled_returns_before_any_drain_and_the_metadata_door_ignores_the_policy() {
        let src = source();
        // `System.gc()` passes the VM's own policy into the shared door; the
        // VM-internal door (`force_gc_for_vm`) passes `Full` and is not
        // explicit (handoff-w8g-vm-internal-collection-uses-the-explicit-door).
        let entry = body(&src, "pub fn force_gc_from_native(");
        assert!(entry.contains("full_gc_door(shared, thread, shared.config.explicit_gc, true)"));
        let internal = body(&src, "pub fn force_gc_for_vm(");
        assert!(internal.contains("ExplicitGcPolicy::Full, false)"));
        let door = body(&src, "fn full_gc_door(");
        let dispatch = door
            .find("explicit_gc_action(policy)")
            .expect("the door reads the per-VM policy");
        let ignore = door.find("ExplicitGcAction::Ignore => return").expect("ignore arm");
        let first_drain = door.find("run_finalizers_forced(").expect("drains");
        assert!(dispatch < ignore && ignore < first_drain, "Disabled returns before any drain");
        // `jdk.SystemGC` is committed only for a request that is not ignored.
        let event = door.find("emit_system_gc_jfr_event(").expect("jdk.SystemGC");
        assert!(door[..event].contains("if explicit && action != ExplicitGcAction::Ignore"));
        let meta = body(&src, "pub fn force_gc_for_class_metadata(");
        assert!(!meta.contains("explicit_gc"));
    }

    /// Under `Disabled` on G1 the door starts no marking cycle either.
    #[test]
    fn a_disabled_explicit_gc_on_g1_opens_no_cycle() {
        let shared = vm(GcAlgorithm::G1, ExplicitGcPolicy::Disabled);
        let mut thread = JvmThread::new(ThreadId(0), "w8g-disabled-g1");
        let cycles0 = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        force_gc_from_native(&shared, &mut thread);
        assert!(!shared.mem.heap.g1_is_marking_active());
        assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), cycles0);
    }
}

#[cfg(test)]
mod gce_e1o_refill_trigger_tests {
    //! gce e1/o: the unjudged refill-trigger cycle (`CRATONVM_GC_REFILL_TRIGGER_UNJUDGED=1`)
    //! resets a latched overhead streak when productive and never raises it.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::vm::SharedVm;
    use std::sync::atomic::Ordering;

    const CAP: usize = 128 * 1024 * 1024;

    /// The verdict is `note_gc_productivity`'s without the progress window:
    /// productive whenever the old generation is not wedged, or when the
    /// cycle freed 2 % of the heap.
    #[test]
    fn a_trigger_cycle_resets_only_on_the_overhead_limits_own_yardstick() {
        let two_percent = CAP / 50;
        assert!(refill_trigger_cycle_resets_streak(0, CAP, false), "old gen has room");
        assert!(!refill_trigger_cycle_resets_streak(0, CAP, true), "a wedged sliver");
        assert!(!refill_trigger_cycle_resets_streak(two_percent - 8, CAP, true));
        assert!(refill_trigger_cycle_resets_streak(two_percent + 8, CAP, true));
    }

    /// A productive unjudged cycle clears a latched streak; a streak at zero
    /// stays zero (the arm never raises it), and neither the progress window
    /// nor the `ladder_forced_unproductive` census moves.
    #[test]
    fn a_productive_unjudged_cycle_clears_a_latched_streak_and_never_raises_one() {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let armed_before = shared.mem.gc_progress.armed.load(Ordering::Relaxed);
        shared.mem.gc_unproductive_streak.store(9, Ordering::Relaxed);
        // 64 MiB freed on a heap whose old generation has room.
        note_refill_trigger_cycle_reset_only(&shared, 64 << 20, 0, Some((0, 0)));
        assert_eq!(shared.mem.gc_unproductive_streak.load(Ordering::Relaxed), 0);
        // Nothing freed: still never an increment.
        note_refill_trigger_cycle_reset_only(&shared, 0, 0, Some((0, 0)));
        assert_eq!(shared.mem.gc_unproductive_streak.load(Ordering::Relaxed), 0);
        assert_eq!(
            shared.mem.gc_progress.armed.load(Ordering::Relaxed),
            armed_before,
            "the progress window is not re-armed by an unjudged cycle"
        );
    }
}
