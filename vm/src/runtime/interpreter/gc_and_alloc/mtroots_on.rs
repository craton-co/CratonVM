#[allow(unused_imports)]
use super::*;

/// DBG (CRATONVM_DBG_MTROOTS): publish the in-progress collection's context
/// (reason / initiator thread / blocked-thread count) so the sweep-zero detector
/// can name WHICH GC reclaimed a live object — pinning the initiator-vs-blocked
/// root-coverage gap. Reason: 1=System.gc, 2=alloc-young, 3=forced-alloc.
#[inline]
pub(in crate::runtime::interpreter) fn mtroots_on() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| None::<std::ffi::OsString>.is_some())}

#[inline]
pub(in crate::runtime::interpreter) fn mtroots_set_gc_ctx(shared: &SharedVm, thread: &JvmThread, reason: u8) {
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
pub(in crate::runtime::interpreter) fn mtroots_dump_initiator(shared: &SharedVm, thread: &JvmThread, reason: u8) {
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
        let _ = write!(buf, " pc={} last={} raw=[", frame.pc, frame.last_instr_pc);
        for i in 0..frame.locals_len().min(32) {
            let _ = write!(
                buf,
                " {i}:{}:{:#x}",
                frame.get_local_tag(i),
                frame.get_local_raw(i)
            );
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
pub(in crate::runtime::interpreter) fn mtroots_selfcheck(thread: &JvmThread, heap: &crate::memory::VmHeap, location: &str) {
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
pub(in crate::runtime::interpreter) fn stw_takeover_should_scan(rounds: u32, jit_hint: bool) -> bool {
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
#[inline]
pub(crate) fn frame_trace_wanted(shared: &SharedVm) -> bool {
    shared
        .mem
        .diagnostics
        .frame_trace_wanted
        .load(std::sync::atomic::Ordering::Relaxed)
        != 0
}

pub(in crate::runtime::interpreter::gc_and_alloc) struct FrameTraceWanted<'a>(&'a std::sync::atomic::AtomicU32);

impl<'a> FrameTraceWanted<'a> {
    pub(in crate::runtime::interpreter::gc_and_alloc) fn raise(counter: &'a std::sync::atomic::AtomicU32) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for FrameTraceWanted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
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
pub(in crate::runtime::interpreter::gc_and_alloc) fn request_non_collection_pause(shared: &SharedVm, initiator: crate::ThreadId) -> Option<Vec<u32>> {
    request_pause_excusing(shared, initiator, NativeExcusal::None).map(|(tids, _)| tids)
}

/// Which threads running a native a handshake pause's census excuses
/// (interpreter round i2 wave 58, lane L6, `CRATONVM_HANDSHAKE_NATIVE_EXCUSE`;
/// `jvm_thread::NativeExcuse`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeExcusal {
    /// None: every counted thread must arrive (every collection, and every
    /// non-collection pause but the two below).
    None,
    /// Every counted thread in a running native: a loop-exit pause, which
    /// only needs every compiled loop to pass its poll -- a thread in a
    /// native runs no loop until it returns, and a frame below it that must
    /// leave does so at its post-call exit, as a blocked thread's does.
    AnyNative,
    /// Those whose frames below the native the pause can list: a frame-trace
    /// pause, which lists them itself ([`publish_excused_native_traces`]).
    ListableNative,
}

impl NativeExcusal {
    /// The excusal a handshake pause of `kind` takes when the switch is on.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn for_kind(kind: NonCollectionPause) -> Self {
        match kind {
            NonCollectionPause::FrameTrace => Self::ListableNative,
            NonCollectionPause::LoopExit => Self::AnyNative,
            _ => Self::None,
        }
    }
}

/// The initiator of a pause taken on a short-lived thread of its own, which
/// no registry entry carries (the loop-exit pauses' retries,
/// `jvmti_events::request_compiled_loop_exits`). Interpreter round i2 wave
/// 58, lane L6: with `CRATONVM_HANDSHAKE_NATIVE_EXCUSE` on, a census that
/// cannot decide whether such an initiator holds a counted slot (a counted
/// entry has not published its OS tid: the JDWP heap service, an FFM
/// upcall attachment) takes it as holding none, which it never does
/// (`docs/feature-designs/real-jdk-mode.md`).
pub(crate) const PAUSE_OUTSIDER: crate::ThreadId = crate::ThreadId(u64::MAX);

/// [`request_non_collection_pause`], excusing the threads `excusal` names
/// when the switch is on: `(counted OS tids, excused threads)`. The excused
/// threads' OS tids are left out of the counted list, so a take-over that
/// froze one of them (it runs no compiled code until released, but nothing
/// forbids it) excuses nobody for it twice.
pub(in crate::runtime::interpreter::gc_and_alloc) fn request_pause_excusing(
    shared: &SharedVm,
    initiator: crate::ThreadId,
    excusal: NativeExcusal,
) -> Option<(
    Vec<u32>,
    Vec<crate::threading::thread_registry::ExcusedNative>,
)> {
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let mut excused = Vec::new();
    let excuse_on = shared.mem.gc_barrier.native_excuse_enabled();
    let won = shared.mem.gc_barrier.request_stw_opening_cycle(
        initiator,
        || {
            let registry = &shared.threads.thread_registry;
            let (n, blocked, tids, blocked_tids, mut initiator_counted) =
                registry.alive_count_blocked_and_os_tids_for(initiator);
            counted_os_tids = tids;
            let mut excused_running = Vec::new();
            if excuse_on {
                if initiator == PAUSE_OUTSIDER && initiator_counted.is_none() {
                    initiator_counted = Some(false);
                }
                if excusal != NativeExcusal::None {
                    excused = registry.excuse_running_natives(
                        initiator,
                        excusal == NativeExcusal::ListableNative,
                    );
                    excused_running = excused.iter().map(|x| x.tid.0).collect();
                    counted_os_tids.retain(|t| !excused.iter().any(|x| x.os_tid == *t));
                }
            }
            crate::threading::gc_barrier::StwCensus {
                alive: u32::try_from(n).unwrap_or(u32::MAX),
                blocked: u32::try_from(blocked).unwrap_or(u32::MAX),
                blocked_tids,
                initiator_counted,
                excused_running,
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
        // Interpreter round i3 wave 72, lane L6 (`CRATONVM_PARK_LAZY_STW_WAKE`,
        // default off; one load when off): and a park's running phase.
        shared.threads.thread_registry.wake_lazy_park_phases();
    }
    won.then_some((counted_os_tids, excused))
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
    stw_publish_frame_traces_with(shared, initiator, || ()).is_some()
}

/// [`stw_publish_frame_traces`], running `held` while the pause still holds
/// every mutator (JIT round 25 wave 6, lane W6-F fix 6): each thread is then
/// either at its poll with its live trace just published, or blocked with
/// its deposit, and none can change its state, trace or lock book until the
/// pause's `Drop`. So what `held` reads of another thread is ONE instant of
/// it, as HotSpot's `ThreadInfo` is one safepoint (`VM_ThreadDump`).
/// `held` must only READ registry state: no allocation, no class-manager
/// lock, no blocking (an excused or frozen peer may hold any of those).
/// `None` when no pause was taken; `held` did not run.
pub(crate) fn stw_publish_frame_traces_with<R>(
    shared: &SharedVm,
    initiator: crate::ThreadId,
    held: impl FnOnce() -> R,
) -> Option<R> {
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
    let _wanted = FrameTraceWanted::raise(&shared.mem.diagnostics.frame_trace_wanted);

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
        return None;
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
        None::<std::ffi::OsString>.is_some();
    // Interpreter round i2 wave 58, lane L6: the threads the census excused
    // in a running native published nothing; list them here, while the
    // pause still holds them in their natives.
    publish_excused_native_traces(shared, &pause.excused);
    // JIT round 19 wave 1 (lane eintr19, FFMEINTR18-1): the threads the
    // take-over claimed in C published nothing either; list them here, while
    // the pause still holds their claims.
    if !publish_lazy_claimed_traces(shared, &pause) {
        drop(pause);
        return None;
    }
    let read = held();
    drop(pause);
    Some(read)
}

/// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-1,
/// `CRATONVM_FFM_LAZY_CLAIM_FRAME_TRACE_PAUSE`): the stack trace of each
/// thread a frame-trace pause CLAIMED in a lazy in-native FFM downcall,
/// published on its behalf -- the frame-trace twin of
/// [`publish_excused_native_traces`].
///
/// The trace is assembled from the frozen owner's published interpreter frames,
/// reflective-call metadata, served `Thread.run` row, active compiled JIT frames,
/// and any OSR continuation artifact. The lazy claim blocks Java execution and
/// frame mutation until this pause releases it, so both interpreter and JIT
/// snapshots stay stable while published on the peer's behalf.
/// Sound because the claim freezes the thread: it is in C (or waiting at
/// its leave), cannot run Java, grow or move its frames or exit until the
/// pause's `Drop` releases it -- the premise of the XT-FRAME-SCAN walk of
/// a frozen peer's frames, through the same `frozen_peer_thread_addrs`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn publish_lazy_claimed_traces(shared: &SharedVm, pause: &NonMovingPause<'_>) -> bool {
    let Some(taken) = pause.taken.as_ref() else {
        return true;
    };
    let tids = taken.native_claim_os_tids();
    if tids.is_empty() {
        return true;
    }
    let dbg = crate::runtime::env_cache::frame_trace();
    for addr in shared
        .threads
        .thread_registry
        .frozen_peer_thread_addrs(&tids)
    {
        // SAFETY: see the doc above -- the claim this pause holds keeps the
        // `JvmThread` alive and its frames and fields unwritten until `Drop`.
        let peer: &crate::threading::jvm_thread::JvmThread =
            unsafe { &*(addr as *const crate::threading::jvm_thread::JvmThread) };
        let [chain, rbp_slot, cm_slot, osr] = peer.gc_block_state.native_excuse.jit_cells();
        let cells = crate::jit::conservative_roots::JitChainCells {
            chain,
            rbp_slot,
            cm_slot,
        };
        // SAFETY: the lazy claim prevents return, upcalls and JNIEnv entry.
        // The owner publishes these TLS addresses before entering IN.
        let Some(snapshot) =
            (unsafe { crate::jit::conservative_roots::JitChainSnapshot::of_held_thread(cells) })
        else {
            // Entry verifies this chain is readable before publishing IN.
            // If that invariant is lost, fail this publication for its caller
            // to retry instead of panicking or publishing an incomplete trace.
            return false;
        };
        let jit = unsafe { snapshot.active_compiled_frames() };
        let jit_frames = jit.len();
        let trace = crate::runtime::stackwalker::capture_published_trace_with_jit(
            shared,
            &peer.frames,
            &peer.reflective_calls,
            &peer.reflective_frames_named,
            &peer.served_thread_run,
            jit,
            |i| unsafe {
                crate::runtime::interpreter::jit_bridge::osr_continuation_artifact_of_held_thread(
                    osr, i,
                )
            },
        );
        let trace_len = trace.entries.len();
        if dbg {
            eprintln!(
                "[LAZY_CLAIM] trace tid={} jit_frames={} entries={trace_len}",
                peer.thread_id.0, jit_frames
            );
        }
        *peer.frame_trace.lock() = trace.entries;
        shared.threads.thread_registry.publish_jmx_frame_monitors(
            peer.thread_id,
            &peer.frames,
            &peer.jmx_frame_monitors_published,
            trace.frame_positions.as_deref().map(|p| (p, trace_len)),
            &trace.compiled_holds,
        );
        record_pause_trace_locks(shared, peer.thread_id, trace_len);
    }
    true
}

/// JIT round 19 wave 12, lane gc12 (`CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`):
/// inside a frame-trace pause, beside the trace of `tid` just stored, the
/// lock stack of that instant with the pause's generations
/// (`ThreadRegistry::record_trace_locks`), so the reader of a RUNNING thread
/// can report the monitors held at the trace's instant rather than at its own
/// earlier lock-stack read. Unconditional bookkeeping (the reader's switch
/// decides whether it is used); a pause path only.
pub(crate) fn record_pause_trace_locks(shared: &SharedVm, tid: crate::ThreadId, trace_len: usize) {
    let barrier = &shared.mem.gc_barrier;
    let gen = barrier
        .gc_generation
        .load(std::sync::atomic::Ordering::Acquire);
    let moving = barrier.moving_generation();
    shared
        .threads
        .thread_registry
        .record_trace_locks(tid, trace_len, gen, moving);
}

/// The frame trace of each thread a frame-trace pause excused in a running
/// native (interpreter round i2 wave 58, lane L6,
/// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE`): its interpreter frames below the
/// native, read in place through its `NativeExcuse` word, with the native on
/// top when the top frame's invoke names it -- as HotSpot's handshake walks a
/// thread in `_thread_in_native`
/// (`docs/feature-designs/real-jdk-mode.md`,
/// `L6W58CrossThreadTraceOfARunningNative.java`).
/// Stored where the thread's own publish puts it, so the reader
/// (`vm_exec::thread_stack_trace_served`) is unchanged.
///
/// Sound because the pause holds each excusal: the thread cannot return
/// from its native, run Java or relocate its frames
/// (`FrameStack::grow_for`), and the census excused it only when every
/// frame below the native is an interpreter frame (no compiled activation
/// or reflective call; a JNI native's or a lambda proxy's row, which the
/// thread's own trace does not list either, is not read here, wave 62) --
/// the frames `[0, depth)` at the published
/// buffer address are as the thread left them. Lock-free but for the
/// class-manager lock the native's name needs, which is only tried (the
/// thread may hold it): busy, the trace goes without the native's row.
/// `CRATONVM_FRAME_TRACE=1` prints `[NATIVE_EXCUSE] trace tid=<n> depth=<n>
/// native=<bool>`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn publish_excused_native_traces(
    shared: &SharedVm,
    excused: &[crate::threading::thread_registry::ExcusedNative],
) {
    use crate::threading::jvm_thread::{NX_COMPILED, NX_DEPTH_SHIFT, NX_LISTABLE, NX_REFLECTIVE};
    for x in excused {
        if x.word & NX_LISTABLE == 0 {
            continue;
        }
        let depth = x.word >> NX_DEPTH_SHIFT;
        let base = x.state.native_excuse.frames() as *const Frame;
        if base.is_null() || depth == 0 {
            continue;
        }
        // SAFETY: see the doc above -- the excusal this pause holds keeps
        // `base[..depth]` live, in place and unwritten until `Drop` releases
        // it; the thread published `base` (Release) before it raised the
        // word this census acquired.
        let frames: &[Frame] = unsafe { std::slice::from_raw_parts(base, depth) };
        // The i53 retirement: the reflective and served `Thread.run` calls
        // below the native, as the thread recorded them before the raise
        // (`NativeRowsSnapshot`); a copy, so the native's own Rust code may
        // edit its lists meanwhile.
        let rows = (x.word & NX_REFLECTIVE != 0).then(|| x.state.native_rows.lock());
        let (trace, head, compiled) = if x.word & NX_COMPILED != 0 {
            match excused_compiled_trace(shared, x, frames, rows.as_deref()) {
                Some(traced) => traced,
                None => {
                    // JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`):
                    // the held thread's JIT chain is mutably borrowed -- by
                    // the thread itself, whose blocking deposit inside this
                    // very native scans it for roots
                    // (`conservative_roots::scan_active_jit_frames_with_sp`).
                    // Its interpreter frames alone would list no compiled
                    // activation and none of their inlined levels (a parked
                    // `P.m` spliced into a compiled `lambda` vanished, and with
                    // it the frame that locked the monitor); that deposit
                    // publishes the whole trace, so this pause publishes
                    // nothing for the thread rather than a short one.
                    drop(rows);
                    if crate::runtime::env_cache::frame_trace() {
                        eprintln!(
                            "[NATIVE_EXCUSE] trace tid={} depth={depth}: JIT chain busy, \
                             left to its own deposit",
                            x.tid.0
                        );
                    }
                    continue;
                }
            }
        } else {
            let head = frames
                .last()
                .and_then(|top| native_entry_named_by_invoke(shared, top));
            let trace = crate::runtime::stackwalker::capture_published_trace_of_held_thread(
                shared,
                frames,
                Vec::new(),
                |_| None,
                rows.as_deref(),
            );
            (trace, head, None)
        };
        drop(rows);
        let crate::runtime::stackwalker::PublishedTrace {
            mut entries,
            frame_positions,
            compiled_holds,
        } = trace;
        // JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`): a
        // thread held in a census native's place -- the running phase of a
        // park (`CRATONVM_PARK_LAZY`), a `--compatible` `LockSupport.parkNanos`
        // bridge -- lists that method's JDK frames down to its `native` leaf
        // (`Unsafe.park(Native Method)` over `LockSupport.parkNanos` at its
        // line), as a parked thread's reader adds them
        // (`vm_exec::parked_standin_frames_for_thread`), not the one
        // line-less row the invoke names: HotSpot's `getLockedStackDepth()`
        // counted two frames above the locking one where this counted one.
        let standin = head
            .is_some()
            .then(|| {
                let top = entries.last()?;
                let cm = shared.classes.class_manager.try_read()?;
                let thread_obj = shared.threads.thread_registry.java_thread_obj(x.tid);
                crate::vm::vm_exec::parked_standin_frames_for_thread(shared, &cm, thread_obj, top)
            })
            .flatten();
        let head = if standin.is_some() { None } else { head };
        let named = head.is_some() || standin.is_some();
        // The native row goes on top: it moves no frame's position.
        entries.extend(head);
        entries.extend(standin.into_iter().flatten());
        if crate::runtime::env_cache::frame_trace() {
            match compiled {
                None => eprintln!(
                    "[NATIVE_EXCUSE] trace tid={} depth={depth} native={named}",
                    x.tid.0
                ),
                Some(chain) => eprintln!(
                    "[NATIVE_EXCUSE] trace tid={} depth={depth} native={named} compiled chain={chain} entries={}",
                    x.tid.0,
                    entries.len()
                ),
            }
        }
        let trace_len = entries.len();
        shared
            .threads
            .thread_registry
            .store_frame_trace(x.tid, entries);
        // JIT round 25: the thread's own deposit inside this native call
        // leaves this publication standing (`NativeExcuse::pause_published_trace`).
        x.state.native_excuse.note_pause_published_trace();
        // JIT round 19 wave 9, lane gc9 (`CRATONVM_JMX_HELD_NATIVE_FRAME_MONITORS`,
        // default ON, HotSpot parity, `=0` kill): with the trace, the frame
        // that locked each held monitor, as the thread's own publish pairs
        // them. Without it the reader found a trace this pause stored beside
        // an attribution an EARLIER publish made for another trace length,
        // refused it, and reported every held monitor at the innermost frame
        // (`R19Sync6LockedDepths` `other-thread.lockF` at `wait0`: a thread
        // still in the running phase of `Object.wait`, excused in its native).
        {
            let positions: Vec<u32> = frame_positions.unwrap_or_else(|| {
                (0..frames.len())
                    .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
                    .collect()
            });
            // JIT round 19 wave 11, lane gc11 (GC920-4,
            // `CRATONVM_JMX_HELD_NATIVE_COMPILED_HOLDS`, default ON, `=0` kill): with the
            // compiled activations' own holds the capture read (empty when
            // off), at their trace positions.
            shared
                .threads
                .thread_registry
                .publish_held_thread_frame_monitors(
                    x.tid,
                    frames,
                    &positions,
                    trace_len,
                    &compiled_holds,
                );
        }
        // JIT round 19 wave 12, lane gc12: NO lock record here -- a thread
        // excused in its native may still change its lock stack there (the
        // running phase of `Object.wait` releases the monitor), so the reader
        // keeps its own lock-stack read for it (an older record fails the
        // generation test).
    }
}



/// [`publish_excused_native_traces`] for a thread excused in a native it
/// entered under compiled code (`NX_COMPILED`; interpreter round i2 wave 60,
/// lane L6, `CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED`): `frames` (its
/// interpreter frames below the native) interleaved with the walk of a copy
/// of its JIT entry chain, read through the cells it published
/// (`JitChainSnapshot::of_held_thread`), and the native named by the invoke
/// of the innermost Java frame, compiled or interpreted. Answers the
/// entries, the native's row and the chain length copied (0: the chain was
/// busy, and the interpreter frames are listed alone).
///
/// Sound for the reason the interpreter frames are: the excusal holds the
/// thread in its native, where it runs no Java and no compiled code -- so it
/// pushes, pops and reallocates no chain entry, writes no frame-record
/// mirror and publishes no OSR continuation (each happens only on a JIT
/// entry or exit, and Java the native runs first passes
/// `interpreter::NestedJavaAck`, which waits while the excusal holds) -- and
/// it cannot return into the compiled frames the copy names, so their stack
/// memory and bodies stay put. No freeze is needed: nothing the walk reads
/// changes while the pause holds the thread.
///
/// JIT round 25: `None` when the chain cannot be copied (its cell is mutably
/// borrowed: `JitChainSnapshot::of_held_thread`), where this used to list the
/// interpreter frames alone -- a trace missing every compiled activation.
pub(in crate::runtime::interpreter::gc_and_alloc) fn excused_compiled_trace(
    shared: &SharedVm,
    x: &crate::threading::thread_registry::ExcusedNative,
    frames: &[Frame],
    rows: Option<&crate::threading::jvm_thread::NativeRowsSnapshot>,
) -> Option<(
    crate::runtime::stackwalker::PublishedTrace,
    Option<cratonvm_native_api::StackTraceEntry>,
    Option<usize>,
)> {
    let [chain, rbp_slot, cm_slot, osr] = x.state.native_excuse.jit_cells();
    let cells = crate::jit::conservative_roots::JitChainCells {
        chain,
        rbp_slot,
        cm_slot,
    };
    // SAFETY: see above; the cells are the excused thread's own, published
    // (Relaxed) before the Release raise of the word this census acquired.
    let snapshot =
        unsafe { crate::jit::conservative_roots::JitChainSnapshot::of_held_thread(cells) }?;
    let copied = snapshot.len();
    // SAFETY: as above -- the copied thread is held in its native.
    let jit = unsafe { snapshot.active_compiled_frames() };
    let trace = crate::runtime::stackwalker::capture_published_trace_of_held_thread(
        shared,
        frames,
        jit,
        // SAFETY: `osr` is the held thread's registry address.
        |i| unsafe {
            crate::runtime::interpreter::jit_bridge::osr_continuation_artifact_of_held_thread(
                osr, i,
            )
        },
        rows,
    );
    let head = trace.entries.last().and_then(|top| {
        let cm = shared.classes.class_manager.try_read()?;
        native_entry_named_by_entry(shared, &cm, top, true)
    });
    Some((trace, head, Some(copied)))
}

/// The row of the native the invoke at `top`'s bci names (a trace entry:
/// an interpreted, compiled or inlined frame), looked up by its class,
/// method name and descriptor (or index) -- [`native_entry_invoked_at`] for
/// a frame the caller holds no `Frame` of. Interpreter round i2 wave 59
/// (lane L6, `vm_exec::append_blocked_native_entry`), shared by wave 60's
/// [`excused_compiled_trace`]. `stand_in` as [`entry_invoked_at`]'s (wave
/// 61: the excused reader's only).
pub(crate) fn native_entry_named_by_entry(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    top: &cratonvm_native_api::StackTraceEntry,
    stand_in: bool,
) -> Option<cratonvm_native_api::StackTraceEntry> {
    let class_id = top.class_id?;
    let pc = usize::try_from(top.byte_code_index).ok()?;
    let class = cm.get_class(class_id)?;
    // The entry keeps the descriptor (or the method's index) when it has
    // one; otherwise every method of the name is asked, and one that is not
    // stopped at an invoke of a native answers nothing.
    let by_index = top
        .method_index
        .and_then(|i| class.methods.get(i as usize))
        .filter(|m| *m.name == *top.method_name);
    by_index
        .into_iter()
        .chain(class.methods.iter().filter(|m| {
            *m.name == *top.method_name
                && top
                    .method_descriptor
                    .as_deref()
                    .is_none_or(|d| *m.descriptor == *d)
        }))
        .find_map(|m| {
            let code = m.code()?;
            entry_invoked_at(shared, cm, class_id, &code.code[..], pc, stand_in)
        })
}

/// The trace row of the native method the invoke at `top`'s `last_instr_pc`
/// runs, when that invoke has a fixed target -- `invokestatic` /
/// `invokespecial`, or a static, private or final method, or one of a final
/// class -- that resolves, without loading anything, to an `ACC_NATIVE`
/// method (interpreter round i2 wave 58, lane L6). `None` otherwise (an
/// overridable target: this VM does not record which callback runs outside
/// a debugger session), and when the class-manager lock is busy.
pub(in crate::runtime::interpreter::gc_and_alloc) fn native_entry_named_by_invoke(
    shared: &SharedVm,
    top: &Frame,
) -> Option<cratonvm_native_api::StackTraceEntry> {
    let cm = shared.classes.class_manager.try_read()?;
    entry_invoked_at(
        shared,
        &cm,
        top.class_id,
        &top.code,
        top.last_instr_pc,
        true,
    )
}

/// [`native_entry_named_by_invoke`]'s decode, for the invoke at `pc` of
/// `code`, a method of class `caller_id`, under a class-manager lock the
/// caller holds (interpreter round i2 wave 59, lane L6: also the reader of a
/// thread blocked in a native, `vm_exec::append_blocked_native_entry`).
pub(crate) fn native_entry_invoked_at(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    caller_id: ClassId,
    code: &[u8],
    pc: usize,
) -> Option<cratonvm_native_api::StackTraceEntry> {
    entry_invoked_at(shared, cm, caller_id, code, pc, false)
}

/// [`native_entry_invoked_at`]; with `stand_in` (interpreter round i2 wave
/// 61, lane L6), a fixed target that is a Java method with bytecode answers
/// a Java row for it (no line: the native standing in for it has no bci),
/// for a thread excused while a registered native runs in that method's
/// place (`--compatible` stand-ins such as `Arrays.fill`): HotSpot runs the
/// Java and lists it on top, and the trace listed no frame for it at all.
/// Only the excused reader asks: a thread blocked in a native is in a
/// `native` method by construction.
pub(in crate::runtime::interpreter::gc_and_alloc) fn entry_invoked_at(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    caller_id: ClassId,
    code: &[u8],
    pc: usize,
    stand_in: bool,
) -> Option<cratonvm_native_api::StackTraceEntry> {
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    const INVOKEVIRTUAL: u8 = 0xb6;
    const INVOKESPECIAL: u8 = 0xb7;
    const INVOKESTATIC: u8 = 0xb8;
    const INVOKEINTERFACE: u8 = 0xb9;
    let opcode = *code.get(pc)?;
    if !matches!(
        opcode,
        INVOKEVIRTUAL | INVOKESPECIAL | INVOKESTATIC | INVOKEINTERFACE
    ) {
        return None;
    }
    let cp_index = u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]);
    let caller = cm.get_class(caller_id)?;
    let (class_index, nat_index) = match caller.constant_pool.get(cp_index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let owner_name = caller.constant_pool.get_class_name(class_index)?;
    let (name, descriptor) = caller.constant_pool.get_name_and_type(nat_index)?;
    let owner = cm.find_cp_class_for_class(owner_name, caller_id)?;
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let found = resolver
        .declared_method(cm, resolver.scope(owner), name, descriptor)
        .ok()?;
    let (declaring, index) = resolver.adopt(found).ok()?;
    let class = cm.get_class(declaring)?;
    let method = class.methods.get(usize::try_from(index).ok()?)?;
    let fixed = matches!(opcode, INVOKESPECIAL | INVOKESTATIC)
        || method.is_static()
        || method
            .access_flags
            .intersects(MethodAccessFlags::PRIVATE | MethodAccessFlags::FINAL)
        || class.access_flags.contains(ClassAccessFlags::FINAL);
    if !fixed {
        return None;
    }
    let java = !method.is_native();
    if java
        && !(stand_in && shared.mem.gc_barrier.excused_stand_in_rows() && method.code().is_some())
    {
        return None;
    }
    let mut entry = crate::runtime::stackwalker::synthetic_entry(
        Arc::clone(&class.name),
        Arc::from(&*method.name),
    );
    if java {
        entry.line_number = crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN;
        entry.method_index = u32::try_from(index).ok();
    }
    entry.method_descriptor = Some(Arc::from(&*method.descriptor));
    entry.class_id = Some(declaring);
    entry.source_file = class.source_file.as_deref().map(Arc::from);
    Some(entry)
}

/// The `ACC_SYNCHRONIZED` method the invoke at `pc` of `code` (a method of
/// class `caller_id`) selects for a thread contending for `contended`, as
/// `(declaring class, method index)`: the resolved method for a fixed target,
/// else the receiver's override -- the contended object, which a
/// synchronized instance method locks -- found up its superclass chain (an
/// interface method cannot be synchronized). `None` for any other
/// instruction, a method that is not synchronized, or a reference that does
/// not resolve without loading. Interpreter round i2 wave 54 (lane L6) wrote
/// this decode for the debugger's listing (`interpreter::synchronized_entry_head`);
/// the i54-L6 retirement shares it with the cross-thread stack trace of a
/// thread blocked entering a synchronized native
/// ([`synchronized_native_entry_named_by_entry`]).
pub(crate) fn synchronized_callee_invoked_at(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    caller_id: ClassId,
    code: &[u8],
    pc: usize,
    contended: ObjectRef,
) -> Option<(ClassId, usize)> {
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    const INVOKEVIRTUAL: u8 = 0xb6;
    const INVOKESPECIAL: u8 = 0xb7;
    const INVOKESTATIC: u8 = 0xb8;
    const INVOKEINTERFACE: u8 = 0xb9;
    let opcode = *code.get(pc)?;
    if !matches!(
        opcode,
        INVOKEVIRTUAL | INVOKESPECIAL | INVOKESTATIC | INVOKEINTERFACE
    ) {
        return None;
    }
    let receiver = (shared.mem.heap.kind_of(contended) != cratonvm_types::ObjectKind::Array)
        .then(|| shared.mem.heap.class_id_of(contended));
    let cp_index = u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]);
    let caller = cm.get_class(caller_id)?;
    let (class_index, nat_index) = match caller.constant_pool.get(cp_index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let owner_name = caller.constant_pool.get_class_name(class_index)?;
    let (name, descriptor) = caller.constant_pool.get_name_and_type(nat_index)?;
    let owner = cm.find_cp_class_for_class(owner_name, caller_id)?;
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let found = resolver
        .declared_method(cm, resolver.scope(owner), name, descriptor)
        .ok()?;
    let (declaring, index) = resolver.adopt(found).ok()?;
    let index = usize::try_from(index).ok()?;
    let resolved_class = cm.get_class(declaring)?;
    let resolved = resolved_class.methods.get(index)?;
    let fixed = matches!(opcode, INVOKESPECIAL | INVOKESTATIC)
        || resolved.is_static()
        || resolved
            .access_flags
            .intersects(MethodAccessFlags::PRIVATE | MethodAccessFlags::FINAL)
        || resolved_class
            .access_flags
            .contains(ClassAccessFlags::FINAL);
    let (selected_class, selected_index) = if fixed {
        (declaring, index)
    } else {
        let mut at = receiver?;
        loop {
            let class = cm.get_class(at)?;
            if let Some(i) = class.methods.iter().position(|m| {
                &*m.name == name
                    && &*m.descriptor == descriptor
                    && !m.is_abstract()
                    && !m.is_static()
            }) {
                break (at, i);
            }
            at = class.superclass?;
        }
    };
    cm.get_class(selected_class)?
        .methods
        .get(selected_index)?
        .is_synchronized()
        .then_some((selected_class, selected_index))
}

/// The `(Native Method)` row of the synchronized native a thread contending
/// for `contended` is blocked ENTERING, named from the invoke at `top`'s bci
/// (the i54-L6 retirement,
/// `docs/feature-designs/real-jdk-mode.md`).
/// HotSpot's native wrapper builds the native's frame before it locks the
/// method's monitor, so another thread's `Thread.getStackTrace()` lists the
/// native on top of its caller (`L6K54BlockedSyncNativeAndEntryOrder.java`);
/// this VM takes the monitor in the native arm before the call
/// (`vm_exec::SynchronizedMethodGuard::enter`), with the caller's frame on
/// top at its invoke. `None` when `top` is itself a synchronized method at
/// bytecode index 0 (a Java method whose frame was pushed before its
/// contended monitor, `CRATONVM_SYNC_METHOD_FRAME_FIRST`: the method IS the
/// top), and for anything [`synchronized_callee_invoked_at`] does not select
/// as a synchronized native.
pub(crate) fn synchronized_native_entry_named_by_entry(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    top: &cratonvm_native_api::StackTraceEntry,
    contended: ObjectRef,
) -> Option<cratonvm_native_api::StackTraceEntry> {
    let class_id = top.class_id?;
    let pc = usize::try_from(top.byte_code_index).ok()?;
    let class = cm.get_class(class_id)?;
    let by_index = top
        .method_index
        .and_then(|i| class.methods.get(i as usize))
        .filter(|m| *m.name == *top.method_name);
    let caller = by_index.into_iter().chain(class.methods.iter()).find(|m| {
        *m.name == *top.method_name
            && top
                .method_descriptor
                .as_deref()
                .is_none_or(|d| *m.descriptor == *d)
            && m.code().is_some()
    })?;
    if pc == 0 && caller.is_synchronized() {
        return None;
    }
    let code = caller.code()?;
    let (declaring, index) =
        synchronized_callee_invoked_at(shared, cm, class_id, &code.code[..], pc, contended)?;
    let declaring_class = cm.get_class(declaring)?;
    let method = declaring_class.methods.get(index)?;
    if !method.is_native() {
        return None;
    }
    let mut entry = crate::runtime::stackwalker::synthetic_entry(
        Arc::clone(&declaring_class.name),
        Arc::from(&*method.name),
    );
    entry.method_descriptor = Some(Arc::from(&*method.descriptor));
    entry.class_id = Some(declaring);
    entry.source_file = declaring_class.source_file.as_deref().map(Arc::from);
    Some(entry)
}

/// The cooperative slice a frame-trace pause waits for every mutator to reach
/// a poll before its take-over may freeze a peer still in compiled code (see
/// [`stw_publish_frame_traces`]). The loop-exit pause's first slice
/// (`jvmti_events::LOOP_EXIT_GRACE`): every compiled loop polls at each back
/// edge and every compiled method at entry, so a few milliseconds bound the
/// added latency of a dump whose target never polls, while a target that does
/// arrives in microseconds and skips the take-over pass.
pub(in crate::runtime::interpreter::gc_and_alloc) const FRAME_TRACE_GRACE: std::time::Duration = std::time::Duration::from_millis(2);

/// The cooperative slice an inline-cache grace request waits before it
/// freezes a missing peer (JIT round 13 wave 11 lane mega9's patch, applied in
/// round 14 wave 1 by lane codecache).
pub(in crate::runtime::interpreter::gc_and_alloc) const IC_GRACE_SLICE: std::time::Duration = std::time::Duration::from_micros(500);

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
    pub(in crate::runtime::interpreter::gc_and_alloc) shared: &'a SharedVm,
    pub(in crate::runtime::interpreter::gc_and_alloc) taken: Option<crate::jit::xt_root_scan::TakenOver>,
    /// gc-common w5-a: the `--verbose:gc` pause line (`door=heap-dump`, or
    /// the kind given to [`Self::request_handshake`]); sealed and printed by
    /// `Drop`. `None` without `--verbose:gc`.
    pub(in crate::runtime::interpreter::gc_and_alloc) pause_timer: Option<super::gc_events::NonCollectionPauseTimer>,
    /// Resume the frozen peers WITHOUT retiring the skip spans the take-over
    /// published: the `CRATONVM_GC_NO_FRAME_TRACE_SPAN_RETIRE=1` lever of
    /// [`stw_publish_frame_traces`], which restores that leak in one binary.
    /// `false` everywhere else.
    pub(in crate::runtime::interpreter::gc_and_alloc) keep_skip_spans: bool,
    /// The counted threads this pause's census excused in a running native
    /// (interpreter round i2 wave 58, lane L6; [`NativeExcusal`]): released
    /// by `Drop` after the world reopens.
    pub(in crate::runtime::interpreter::gc_and_alloc) excused: Vec<crate::threading::thread_registry::ExcusedNative>,
    /// gci i4/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`):
    /// the objects the take-over found in the registers and stack bands of
    /// the peers it froze, for a heap dump's roots
    /// ([`Self::request_dump_words`]); sorted, deduplicated. Empty for every
    /// other pause.
    pub(in crate::runtime::interpreter::gc_and_alloc) dump_words: Vec<ObjectRef>,
}

impl<'a> NonMovingPause<'a> {
    /// Request the pause and wait (with the take-over) until it holds. `None`
    /// when another stop-the-world already owns the world -- the caller decides
    /// whether to proceed unpaused.
    pub(crate) fn request(shared: &'a SharedVm, initiator: crate::ThreadId) -> Option<Self> {
        Self::request_as(shared, initiator, NonCollectionPause::HeapDump, None, false)
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
    pub(in crate::runtime::interpreter) fn request_handshake(
        shared: &'a SharedVm,
        initiator: crate::ThreadId,
        kind: NonCollectionPause,
        grace: std::time::Duration,
    ) -> Option<Self> {
        Self::request_as(shared, initiator, kind, Some(grace), false)
    }

    /// `dump_words` (gci i4/hd): the take-over keeps the frozen peers' words
    /// ([`NO_ROOTS_DUMP_WORDS`], [`Self::request_dump_words`]).
    pub(in crate::runtime::interpreter::gc_and_alloc) fn request_as(
        shared: &'a SharedVm,
        initiator: crate::ThreadId,
        kind: NonCollectionPause,
        grace: Option<std::time::Duration>,
        dump_words: bool,
    ) -> Option<Self> {
        // Wave 58 (lane L6): only a handshake pause excuses a running native.
        let excusal = if grace.is_some() {
            NativeExcusal::for_kind(kind)
        } else {
            NativeExcusal::None
        };
        let (counted_os_tids, excused) = request_pause_excusing(shared, initiator, excusal)?;
        // The guard exists BEFORE the wait, so an unwind out of the take-over
        // still reopens the world.
        let mut pause = NonMovingPause {
            shared,
            taken: None,
            pause_timer: None,
            keep_skip_spans: false,
            excused,
            dump_words: Vec::new(),
        };
        if !pause.excused.is_empty() && crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[NATIVE_EXCUSE] door={kind:?} excused={:?}",
                pause.excused.iter().map(|x| x.tid.0).collect::<Vec<_>>()
            );
        }
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
            let mode = if dump_words { NO_ROOTS_DUMP_WORDS } else { NO_ROOTS_FREEZE };
            let _no_roots = NoRootsPause::enter_mode(mode);
            // The no-roots claim scope freezes lazy FFM downcalls for heap dump,
            // inline-cache grace, loop-exit, and redefinition pauses when their
            // switch is on. Frame-trace has its own default-on flag and publishes
            // the claimed thread's interpreter and compiled frames before release.
            let _lazy_claims = if shared
                .threads
                .thread_registry
                .no_roots_lazy_claims_enabled()
                && no_roots_pause_may_claim(kind)
            {
                Some(NoRootsLazyClaim::enter())
            } else if matches!(kind, NonCollectionPause::FrameTrace)
                && frame_trace_lazy_claim_enabled()
            {
                Some(NoRootsLazyClaim::enter())
            } else {
                None
            };
            let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
            // gci i4/hd: under `request_dump_words` the pass probed the frozen
            // peers' words into `xt_roots` (this pause gathers no roots);
            // keep them for the dump.
            if dump_words {
                pause.dump_words = dedup_dump_words(xt_roots);
                // The probe's accepted stack words were also captured for the
                // blocked-peer fold and the register-pairing diagnostic,
                // which no non-moving pause drains; the next collection's
                // opener clears both, this does it now.
                cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
                cratonvm_gc::gc_quiescence::clear_peer_reg_capture();
            }
            taken
        };
        pause.taken = Some(taken);
        pause.pause_timer = non_collection_pause_start(shared, kind);
        Some(pause)
    }

    /// gci i4/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`):
    /// [`Self::request`] for a heap dump, keeping what the take-over reads in
    /// each frozen peer's registers and stack band (exact bases and derived
    /// pointers, `takeover_word_probe`; no companions) as
    /// [`Self::frozen_peer_words`]. The pause still gathers no roots: no pins,
    /// no helper-window pass, no lazy-claim pins; only the probe of the one
    /// pass changes ([`run_takeover_pass`]). `None` as [`Self::request`].
    pub(crate) fn request_dump_words(
        shared: &'a SharedVm,
        initiator: crate::ThreadId,
    ) -> Option<Self> {
        Self::request_as(shared, initiator, NonCollectionPause::HeapDump, None, true)
    }

    /// gci i4/hd (`gci-i3hd-heap-walks-and-diagnostics-review-findings-FIXED-20261017`
    /// item 9): [`Self::request`] for `jcmd GC.class_histogram`'s walk, named
    /// `door=heap-inspection` on the `--verbose:gc` pause line (it shared the
    /// dump's `heap-dump`). Freeze-only, as the dump's.
    pub(crate) fn request_heap_inspection(
        shared: &'a SharedVm,
        initiator: crate::ThreadId,
    ) -> Option<Self> {
        Self::request_as(shared, initiator, NonCollectionPause::HeapInspection, None, false)
    }

    /// gci i4/hd: the objects named by the frozen peers' registers and stack
    /// bands at the freeze ([`Self::request_dump_words`]); empty for any other
    /// request or when nobody was frozen.
    pub(crate) fn frozen_peer_words(&self) -> &[ObjectRef] {
        &self.dump_words
    }

    /// Did this pause freeze a peer in compiled code (a peer that therefore
    /// passed no poll during it)?
    pub(in crate::runtime::interpreter) fn froze_peers(&self) -> bool {
        self.taken.as_ref().is_some_and(|t| t.count() > 0)
    }

    /// gci i3/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`):
    /// the OS tids of the peers this pause holds without a precise deposit
    /// (frozen in compiled code, or claimed in C conservatively) -- the set
    /// the collections' frozen-peer frame walk reads. Held until `Drop`, so a
    /// heap dump under this pause may read their interpreter frames. Empty
    /// when nobody was frozen.
    pub(crate) fn frozen_unrewritable_os_tids(&self) -> Vec<u32> {
        self.taken
            .as_ref()
            .map(|t| t.unrewritable_os_tids())
            .unwrap_or_default()
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
        // Wave 58 (lane L6): after the world reopened, so a released thread
        // that returns from its native at once finds no pause it could be
        // counted in by mistake. A pause requested meanwhile counts it (its
        // word was still held, so that census did not excuse it).
        for x in self.excused.drain(..) {
            x.state.native_excuse.release();
        }
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
pub(in crate::runtime::interpreter::gc_and_alloc) fn cooperative_grace_wait(
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
    ///
    /// A mode, not a bool, since gci i4/hd: [`NO_ROOTS_OFF`],
    /// [`NO_ROOTS_FREEZE`], or [`NO_ROOTS_DUMP_WORDS`] (a heap dump's pause,
    /// whose take-over keeps the frozen peers' words for the dump's roots).
    pub(in crate::runtime::interpreter::gc_and_alloc) static NO_ROOTS_PAUSE: std::cell::Cell<u8> = const { std::cell::Cell::new(NO_ROOTS_OFF) };
}

/// [`NO_ROOTS_PAUSE`]: no scope open; a take-over gathers roots.
pub(in crate::runtime::interpreter::gc_and_alloc) const NO_ROOTS_OFF: u8 = 0;
/// [`NO_ROOTS_PAUSE`]: freeze only; every word the pass reads is declined.
pub(in crate::runtime::interpreter::gc_and_alloc) const NO_ROOTS_FREEZE: u8 = 1;
/// [`NO_ROOTS_PAUSE`] (gci i4/hd,
/// `gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`): freeze,
/// and probe each frozen peer's registers, stack band and shadow window with
/// the collections' probe so the heap dump can name what only a compiled
/// frame holds ([`NonMovingPause::request_dump_words`]). Still no roots: no
/// pins, no frame walk, no helper-window pass.
pub(in crate::runtime::interpreter::gc_and_alloc) const NO_ROOTS_DUMP_WORDS: u8 = 2;

/// RAII scope for [`NO_ROOTS_PAUSE`]; lowers it on every path out.
pub(in crate::runtime::interpreter::gc_and_alloc) struct NoRootsPause;

impl NoRootsPause {
    pub(in crate::runtime::interpreter::gc_and_alloc) fn enter() -> Self {
        Self::enter_mode(NO_ROOTS_FREEZE)
    }

    /// gci i4/hd: [`Self::enter`] with the mode named.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn enter_mode(mode: u8) -> Self {
        NO_ROOTS_PAUSE.with(|c| c.set(mode));
        NoRootsPause
    }
}

impl Drop for NoRootsPause {
    fn drop(&mut self) {
        let _ = NO_ROOTS_PAUSE.try_with(|c| c.set(NO_ROOTS_OFF));
    }
}

/// Is this thread inside a heap dump's no-roots take-over
/// ([`NO_ROOTS_DUMP_WORDS`])? Only ever consulted where the take-over gathers
/// no roots.
pub(in crate::runtime::interpreter::gc_and_alloc) fn dump_peer_words_wanted() -> bool {
    NO_ROOTS_PAUSE.with(std::cell::Cell::get) == NO_ROOTS_DUMP_WORDS
}

/// The dump's frozen-peer words, sorted by address and deduplicated (a word
/// in a register and a spill slot is one root). Pure.
pub(in crate::runtime::interpreter::gc_and_alloc) fn dedup_dump_words(mut words: Vec<ObjectRef>) -> Vec<ObjectRef> {
    words.sort_unstable_by_key(|o| o.as_ptr() as usize);
    words.dedup_by_key(|o| o.as_ptr() as usize);
    words
}

thread_local! {
    /// JIT round 18 wave 10 (lane ffmeintr, FF3-2): raised by a
    /// [`NonMovingPause`] whose kind may claim lazy in-native peers
    /// ([`no_roots_pause_may_claim`]) around its own `stw_take_over_and_wait`.
    /// Thread-local for the reason [`NO_ROOTS_PAUSE`] is: only the pause's
    /// initiator reads it.
    ///
    /// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-1): a mode, not a bool:
    /// [`NO_ROOTS_CLAIM_OFF`] or [`NO_ROOTS_CLAIM_ALL`] (heap dump, IC grace,
    /// or a frame-trace pause that publishes both interpreter and JIT frames).
    pub(in crate::runtime::interpreter::gc_and_alloc) static NO_ROOTS_LAZY_CLAIM: std::cell::Cell<u8> = const { std::cell::Cell::new(NO_ROOTS_CLAIM_OFF) };
}

/// [`NO_ROOTS_LAZY_CLAIM`]: no scope open.
pub(in crate::runtime::interpreter::gc_and_alloc) const NO_ROOTS_CLAIM_OFF: u8 = 0;
/// [`NO_ROOTS_LAZY_CLAIM`]: claim every thread in lazy C (freeze-only).
pub(in crate::runtime::interpreter::gc_and_alloc) const NO_ROOTS_CLAIM_ALL: u8 = 1;

/// RAII scope for [`NO_ROOTS_LAZY_CLAIM`]; lowers it on every path out.
pub(in crate::runtime::interpreter::gc_and_alloc) struct NoRootsLazyClaim;

impl NoRootsLazyClaim {
    pub(in crate::runtime::interpreter::gc_and_alloc) fn enter() -> Self {
        NO_ROOTS_LAZY_CLAIM.with(|c| c.set(NO_ROOTS_CLAIM_ALL));
        NoRootsLazyClaim
    }
}

impl Drop for NoRootsLazyClaim {
    fn drop(&mut self) {
        let _ = NO_ROOTS_LAZY_CLAIM.try_with(|c| c.set(NO_ROOTS_CLAIM_OFF));
    }
}

/// `CRATONVM_FFM_LAZY_CLAIM_FRAME_TRACE_PAUSE` (JIT round 19, default ON,
/// `=0` restores the wait): a frame-trace pause (cross-thread
/// `Thread.getStackTrace`, a thread dump) claims each thread sitting in C in a
/// lazy in-native downcall, freeze-only, and publishes its trace on its behalf
/// ([`publish_lazy_claimed_traces`]). It reads the held owner's interpreter
/// frames, reflective/served rows, published JIT chain and OSR artifact, so a
/// compiled caller is included without asking the owner to poll. The claim
/// keeps those snapshots stable until publication completes. HotSpot answers a
/// thread in native at once; before this the pause waited for C to return and
/// could deadlock (A in C waits for B, B waits on the frame-trace pause, the
/// pause waits for A). Read once per frame-trace pause.
pub(in crate::runtime::interpreter::gc_and_alloc) fn frame_trace_lazy_claim_enabled() -> bool {
    true
}

/// `CRATONVM_FFM_LAZY_CLAIM_NO_ROOTS_PAUSE` (JIT round 18 wave 10, lane
/// ffmeintr, FF3-2 of `r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world`;
/// opt-in, a liveness change): a no-roots pause of a kind
/// [`no_roots_pause_may_claim`] admits claims a thread sitting in C in a lazy
/// in-native downcall (freeze-only: nothing is scanned, nothing is collected)
/// instead of waiting for its C code to return. Read once per pause.
pub(in crate::runtime::interpreter::gc_and_alloc) fn no_roots_lazy_claim_enabled() -> bool {
    true
}

/// Which no-roots pauses may claim a lazy in-native peer instead of waiting
/// for it (FF3-2). Only those whose purpose a thread held in C cannot defeat:
///
/// * `HeapDump`: the heap must be quiescent, and a claimed thread cannot
///   touch it (its leave waits on the claim); its reserved TLAB tail is
///   published as a skip span like a frozen peer's.
/// * `IcGrace`: a claimed thread is in C, not between an inline-cache compare
///   and its entry load; the pause then counts a held peer and advances no
///   grace, as with a frozen one -- no worse than waiting, and bounded.
/// * `LoopExit`: the pause arms each compiled loop's exit word before it starts.
///   A thread in C cannot execute the loop; its return resumes at the armed
///   post-call/back-edge poll and leaves the withdrawn body there.
/// * `Redefinition`: the pause raises the loop fence before installation. A
///   claimed thread cannot read a constant while in C; on return, the native
///   leave converts obsolete interpreter frames and compiled loops hit the
///   armed poll before their next iteration. Active frames may finish their
///   old body, as permitted by redefinition semantics.
///
/// Frame trace uses the same freeze-only scope through its own flag and
/// publishes the held thread's trace from stable interpreter/JIT snapshots.
/// The collection kinds gather roots and claim already.
pub(in crate::runtime::interpreter::gc_and_alloc) fn no_roots_pause_may_claim(kind: NonCollectionPause) -> bool {
    matches!(
        kind,
        NonCollectionPause::HeapDump
            | NonCollectionPause::HeapInspection
            | NonCollectionPause::IcGrace
            | NonCollectionPause::LoopExit
            | NonCollectionPause::Redefinition
    )
}

/// Is a [`NoRootsLazyClaim`] scope open on this thread?
pub(in crate::runtime::interpreter::gc_and_alloc) fn no_roots_lazy_claim_active() -> bool {
    NO_ROOTS_LAZY_CLAIM.with(std::cell::Cell::get) != NO_ROOTS_CLAIM_OFF
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
pub(in crate::runtime::interpreter::gc_and_alloc) fn takeover_gathers_roots() -> bool {
    NO_ROOTS_PAUSE.with(std::cell::Cell::get) == NO_ROOTS_OFF
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
///
/// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): the resume completes the
/// post-only card mark first (`resume_completing_post_only_card_marks`: the
/// frozen peers' in-range register words get their cards dirtied after this
/// pause's card scan, before any of them runs). A no-op beyond the resume on
/// G1, ZGC, and on Generational while no post-only site was compiled.
pub(in crate::runtime::interpreter::gc_and_alloc) fn retire_skip_spans_and_resume(shared: &SharedVm, taken: crate::jit::xt_root_scan::TakenOver) {
    shared.mem.heap.clear_jit_tlab_skip_regions();
    let _ = crate::jit::xt_root_scan::resume_completing_post_only_card_marks(
        taken,
        &shared.mem.heap,
    );
}

/// One take-over pass over `live_tids`, timed into the pause's `xt_pass_us`.
/// Split out of [`stw_take_over_and_wait`]'s loop (gce e2/t) so the
/// first-pass grace can run the same pass over the uncounted roster.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_takeover_pass(
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
    // JIT round 18 wave 10b (lane ffmeintr, `CRATONVM_FFM_LAZY_SIGNAL_GATE`,
    // default ON): the Linux pass closes each thread's lazy in-native entry
    // for the instant it signals it. Refreshed per pass: a thread registered
    // since the last one is in this roster too.
    let gates = if cfg!(all(target_os = "linux", target_arch = "x86_64"))
        && crate::threading::jvm_thread::lazy_signal_gate_enabled()
    {
        shared.threads.thread_registry.signal_gate_states()
    } else {
        Vec::new()
    };
    taken.set_signal_gates(gates);
    // gcj j2/jt2: where this pass's frozen peers' registers begin (see the
    // early post-only completion after the pass).
    let words_before = taken.frozen_register_words().len();
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
        let probe = |a: usize| {
            let r = xt::takeover_word_probe(&shared.mem.heap, a);
            if let Some(c) = xt::takeover_word_companion(&shared.mem.heap, a, r) {
                companions.borrow_mut().push(c);
            }
            r
        };
        // gci i1/fr (`gcf-f7f-...` kind A, opt-in on Generational; zwr: default
        // ON on ZGC): a frozen peer's words above its innermost compiled segment
        // root object starts only (`xt::takeover_above_chain_word_probe`, no
        // companion).
        let above = |a: usize| xt::takeover_above_chain_word_probe(&shared.mem.heap, a);
        let split = xt::takeover_above_chain_splits(&shared.mem.heap);
        // zwr: on ZGC only the innermost frame keeps the full probe (the
        // Generational opt-in keeps its whole innermost segment).
        taken.set_above_split_frames(usize::from(split && !shared.mem.heap.is_generational()));
        let n = xt::take_over_pass_split(
            taken,
            &probe,
            split.then_some(&above),
            xt_roots,
            live_tids,
        );
        xt_roots.extend(companions.into_inner());
        n
    } else if dump_peer_words_wanted() {
        // gci i4/hd: a heap dump's no-roots pause keeps the frozen peers'
        // words for the dump's roots (`NonMovingPause::request_dump_words`):
        // exact bases and derived pointers, as a collection probes them; no
        // companions (a dump names objects, it pins nothing). The pass itself
        // is the one every pause runs; only its probe answers.
        let probe = |a: usize| xt::takeover_word_probe(&shared.mem.heap, a);
        xt::take_over_pass(taken, &probe, xt_roots, live_tids)
    } else {
        // Freeze only: every word is declined, so nothing is scanned
        // into a root set this pause would discard anyway.
        xt::take_over_pass(taken, &|_: usize| None::<ObjectRef>, xt_roots, live_tids)
    };
    // gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): the EARLY half of the
    // post-only completion. The cards of the peers this pass froze are
    // dirtied now, before the collection reads any card, which is exactly
    // what a PRE check would have left for a peer frozen between its store
    // and its card byte: the pause's own card scan sees the edge (and
    // `CRATONVM_GC_VERIFY_RSET` counts it seeded). The resume repeats it
    // after the scan cleared the cards (`retire_skip_spans_and_resume`).
    // Lock-free and allocation-free (`CardTable::mark_dirty_lockfree`), so
    // it may run while peers are frozen. A no-op on G1 / ZGC and while no
    // post-only site was compiled.
    if let Some(words) = taken.frozen_register_words().get(words_before..) {
        if !words.is_empty() {
            let _ = shared.mem.heap.takeover_complete_post_only_card_marks(words);
        }
    }
    cratonvm_gc::gc_quiescence::add_xt_pass_ns(
        u64::try_from(pass_t0.elapsed().as_nanos()).unwrap_or(u64::MAX),
    );
    n
}

/// `CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY` (JIT round 16 wave 14, lane ffm3;
/// default ON since JIT round 17 wave 5, lane ffm4, `=0` off): the pause half
/// of the lazy in-native FFM downcall (`panama::ffm_downcall_in_native_lazy` is
/// the thread half, with the same default). Read per take-over round, no
/// cache. Without the claim, the loop of [`stw_take_over_and_wait`] waits for
/// a counted thread in C until its C function returns, which for a thread
/// blocked on a parked one (a pipe `read` whose writer is leaving
/// `Thread.sleep` behind this very pause) is never: `R17EintrNative`'s
/// `still waiting for cooperative mutators ... pending=1 taken=0`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn ffm_in_native_lazy_claims_enabled() -> bool {
    true
}



/// A no-roots pause that cannot claim a lazy FFM peer must retain the old
/// native wait semantics. Its owner is held at the barrier while the peer
/// returns from C; `end_native_region_lazy` drops the unclaimed population
/// under the barrier lock and then waits out this pause before touching Java
/// heap state.
pub(in crate::runtime::interpreter::gc_and_alloc) fn wait_for_unclaimed_lazy_native_peers(shared: &SharedVm) {
    if no_roots_lazy_claim_active() {
        return;
    }
    let registry = &shared.threads.thread_registry;
    while registry.lazy_native_population_count() != 0 {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// JIT round 16 wave 14 (lane ffm3): claim and scan, for this collection, every
/// thread that sits in C through a LAZY in-native FFM downcall
/// (`GcBlockState::lazy_native`), and add it to `taken`. Returns how many it
/// claimed; [`stw_take_over_and_wait`] excuses the counted ones from the
/// barrier with the take-over's own identity rule.
///
/// This is HotSpot's safepoint treatment of `_thread_in_native` built from the
/// take-over's frozen-peer machinery. A claimed thread is in the same position
/// as a peer suspended in compiled code -- it cannot run Java, touch the heap or
/// change a frame until the pause releases it (its leave from C waits on the
/// claim, `GcBlockState::leave_lazy_native`; an up-call or a JNIEnv function
/// leaves the same way first) -- so it gets the same coverage, from the same
/// readers:
///
/// * its stack band, conservatively, with the take-over's probe and companion
///   rule (`xt::scan_native_claim_band`): every compiled and Rust frame above
///   its C code, the compiled caller's callee-saved registers spilled inside
///   the band before the state was published;
/// * its shadow stack (`xt::scan_taken_over_peer_shadow`);
/// * its interpreter frames and every `JvmThread` field, through its tid in
///   `taken.tids` (the XT-FRAME-SCAN walk after the barrier);
/// * the frozen-peer consequences of `taken.count() > 0`: no moving young
///   collection, the unrewritable-peer mark, the G1 pins of `xt_roots` and of
///   its deposited snapshot (`pin_frozen_peer_roots_for_g1`), its reserved
///   TLAB tail published as a skip span;
/// * `xt::resume` releases it after the collection, with the frozen peers.
///
/// Only a pause that gathers roots claims (a no-roots handshake waits for the
/// thread as before), and only with the switch on.
pub(in crate::runtime::interpreter::gc_and_alloc) fn claim_lazy_native_peers(
    shared: &SharedVm,
    gathers_roots: bool,
    taken: &mut crate::jit::xt_root_scan::TakenOver,
    xt_roots: &mut Vec<ObjectRef>,
) -> usize {
    use crate::jit::xt_root_scan as xt;
    // JIT round 18 wave 10 (lane ffmeintr, FF3-2): a no-roots pause claims
    // only inside a `NoRootsLazyClaim` scope (opt-in, some kinds).
    if !(gathers_roots || no_roots_lazy_claim_active()) || !ffm_in_native_lazy_claims_enabled() {
        return 0;
    }
    // JIT round 19 wave 1 (lane eintr19, FFMEINTR18-1): a frame-trace pause
    // claims only the threads whose trace it can publish for them.
    let registry = &shared.threads.thread_registry;
    // A held-thread trace carries the published compiled chain and OSR row too;
    // every claimed peer can now be rendered without asking its owner to poll.
    let claims = registry.claim_lazy_native_peers(&taken.tids);
    if claims.is_empty() {
        return 0;
    }
    if !gathers_roots {
        // Freeze-only, as the take-over pass of this pause is: the claim
        // holds the thread out of the heap until `resume`, and the barrier
        // excuses it below like a frozen peer. Nothing is scanned (no root
        // set is built), so neither is its band.
        let n = claims.len();
        let dbg = false;
        for claim in claims {
            if dbg {
                eprintln!(
                    "[ffm-lazy] claimed os_tid={} for a no-roots pause (freeze-only)",
                    claim.os_tid
                );
            }
            taken.push_native_claim(claim.os_tid, claim.state);
        }
        return n;
    }
    let companions = std::cell::RefCell::new(Vec::<ObjectRef>::new());
    let probe = |a: usize| {
        let r = xt::takeover_word_probe(&shared.mem.heap, a);
        if let Some(c) = xt::takeover_word_companion(&shared.mem.heap, a, r) {
            companions.borrow_mut().push(c);
        }
        r
    };
    let dbg = false;
    let observes_locals = crate::runtime::jvmti::debugger_observes_locals(shared);
    let n = claims.len();
    let mut roots = 0usize;
    for claim in claims {
        if claim
            .state
            .lazy_native_deposit_eligible
            .load(std::sync::atomic::Ordering::Acquire)
        {
            let peers = registry.frozen_peer_thread_addrs(&[claim.os_tid]);
            if let [addr] = peers.as_slice() {
                // SAFETY: the claim prevents Java, return and teardown. Entry
                // excludes JNI/deopt TLS; compiled/shadow roots below are either
                // absent or read whole and pinned before accepting the deposit.
                let peer = unsafe { &mut *(*addr as *mut JvmThread) };
                let depth = cratonvm_gc::gc_quiescence::jit_depth_of_tid(claim.os_tid);
                let machine_roots_absent = claim
                    .state
                    .lazy_native_machine_roots_absent
                    .load(std::sync::atomic::Ordering::Acquire);
                // Chain/shadow absence is only a hint: an unregistered (A5)
                // compiled frame still owns raw machine words. Prove the band
                // free of compiled code before granting the exact-only licence.
                let interpreted = machine_roots_absent
                    && depth.is_none_or(|d| d == 0)
                    && unsafe {
                        xt::native_claim_band_has_compiled_word(
                            claim.lo,
                            claim.hi,
                            &crate::jit::jit_code_ranges_snapshot(),
                        )
                    } == Some(false);
                let pin_from = xt_roots.len();
                let pinned = if interpreted {
                    false
                } else {
                    // The entry spill covers every callee-saved register; the
                    // published band reaches the owner's stack top. No Java
                    // or helper frame in that band can change during the claim.
                    unsafe { xt::scan_native_claim_band(claim.lo, claim.hi, &probe, xt_roots) };
                    let shadow_whole =
                        xt::scan_native_claim_shadow(claim.os_tid, &probe, xt_roots).is_some();
                    xt_roots.extend(companions.borrow_mut().drain(..));
                    xt::lazy_claim_pin_enabled()
                        && xt::takeover_interior_enabled()
                        && crate::threading::jvm_thread::lazy_native_band_ok(claim.lo, claim.hi)
                        && shadow_whole
                        && depth.is_some()
                };
                // Failed pin proofs keep the candidates and fall through to
                // the conservative claim, which publishes its refusal below.
                if interpreted || pinned {
                    roots += xt_roots.len() - pin_from;
                    peer.tlab.retire();
                    let mut snapshot = Vec::new();
                    for frame in peer.frames.iter() {
                        frame.scan_local_objects_mapped(
                            &mut snapshot,
                            &shared.mem.heap,
                            &shared.classes.type_maps,
                        );
                        scan_frame_roots(
                            frame,
                            &mut snapshot,
                            &shared.mem.heap,
                            crate::runtime::value_stack::MINTED_ONLY_PRIMITIVE_STACK_ROOTS,
                            observes_locals,
                            Some(&*shared.classes.type_maps),
                        );
                        snapshot.extend(frame.monitor_on_exit);
                        snapshot.extend_from_slice(frame.held_monitors.as_slice());
                    }
                    crate::memory::roots::push_off_frame_thread_roots(peer, &mut snapshot);
                    snapshot.extend(crate::memory::roots::frame_class_owners(
                        &shared.loader_identity,
                        peer,
                    ));
                    let roots = snapshot.len();
                    *peer.root_snapshot.lock() = snapshot;
                    peer.rs_cache.clear();
                    peer.rs_cache_gen = 0;
                    if dbg {
                        eprintln!(
                            "[ffm-lazy] precise claimed os_tid={} roots={roots}",
                            claim.os_tid
                        );
                    }
                    if pinned {
                        let mut addrs: Vec<usize> = xt_roots[pin_from..]
                            .iter()
                            .map(|o| o.as_ptr() as usize)
                            .collect();
                        addrs.extend(
                            peer.root_snapshot
                                .lock()
                                .iter()
                                .map(|o| o.as_ptr() as usize),
                        );
                        cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                        if crate::jit::conservative_roots::xt_pinned_peer_depth_enabled() {
                            cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_depth(depth);
                        }
                        if cratonvm_gc::gc_quiescence::pinned_young_copy_takeover_enabled() {
                            let range = cratonvm_gc::gc_quiescence::young_pin_range();
                            let mut words = Vec::new();
                            let mut at = claim.lo;
                            while at + 8 <= claim.hi {
                                // SAFETY: the held owner's validated, complete band.
                                let word = unsafe { (at as *const usize).read_volatile() };
                                if range.contains(word) {
                                    words.push(word);
                                }
                                at += 8;
                            }
                            words.extend(addrs.iter().copied().filter(|w| range.contains(*w)));
                            cratonvm_gc::gc_quiescence::note_peer_band_pin_words(
                                &words, depth, true, true,
                            );
                        }
                        if dbg {
                            eprintln!(
                                "[ffm-lazy] pinned deposit os_tid={} depth={depth:?} pins={}",
                                claim.os_tid,
                                addrs.len()
                            );
                        }
                        taken.push_pinned_native_claim(claim.os_tid, claim.state);
                    } else {
                        taken.push_precise_native_claim(claim.os_tid, claim.state);
                    }
                    continue;
                }
            }
        }
        // SAFETY: `claim` holds the thread's claim until `xt::resume`, and
        // its band is the one the thread published with its state.
        let band = unsafe { xt::scan_native_claim_band(claim.lo, claim.hi, &probe, xt_roots) };
        let shadow = xt::scan_taken_over_peer_shadow(claim.os_tid, &probe, xt_roots);
        roots += band + shadow;
        // Not rewritable by this collection, as a frozen peer's roots.
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
            cratonvm_gc::gc_quiescence::incomplete_reason::XT_TAKEOVER,
        );
        if dbg {
            eprintln!(
                "[ffm-lazy] claimed os_tid={} band={:#x}..{:#x} roots={} shadow={}",
                claim.os_tid, claim.lo, claim.hi, band, shadow
            );
        }
        taken.push_native_claim(claim.os_tid, claim.state);
    }
    xt_roots.extend(companions.into_inner());
    cratonvm_gc::gc_quiescence::publish_xt_pass(n as u64, 0, roots as u64);
    n
}

/// JIT round 16 wave 16 (lane ffm3): pin `xt_roots` (every frozen or claimed
/// peer's conservative and walked roots, gathered by the take-over) for the
/// rest of this collection, through the per-cycle pin set the helper-window
/// pass uses (`add_xt_cycle_pinned_jit_roots`: survives `collect_roots`, read
/// by G1's collection-set exclusion and ZGC's slide). Over-pinning costs a
/// region or page of compaction for one pause; a missing pin is a claimed
/// thread resuming onto a moved object -- the `Session is acquired by 1
/// clients` leak of `R16Ffm3Stw` on ZGC, whose slide ran because no compiled
/// frame was live.
pub(in crate::runtime::interpreter::gc_and_alloc) fn pin_lazy_native_claim_roots(xt_roots: &[ObjectRef]) {
    let addrs: Vec<usize> = xt_roots.iter().map(|o| o.as_ptr() as usize).collect();
    cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
}

/// gch h4/bp (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`, opt-in;
/// `gcg-g4jn-proposal-in-native-band-scanned-at-the-pause-DONE-20261015`): the
/// pause's half of a JNI in-native thread's compiled band, which its deposit
/// left to the pause (`native::jni::in_native_band_at_pause`). Called by every
/// Generational pause that gathers the parked threads' snapshots
/// ([`run_collection_pause`], the concurrent cycle's initial mark and remark)
/// right after `collect_all_root_snapshots`, with that vector as `roots`.
///
/// Every band word the take-over's probe accepts (an object start, or the
/// object an interior word lies in) is appended, with the probe's companion
/// (the object a one-past-the-end word ends), exactly as for a peer claimed
/// in a lazy FFM downcall (`claim_lazy_native_peers`). The roots keep their
/// objects in place: the cycle's moving-young coverage is marked incomplete
/// (`CROSS_THREAD_JIT_PEER`: such a peer's depth is never credited, so this is
/// the verdict it produces already), so the Generational cycle is non-moving
/// and its selective promotion pins every root by value. (gci i24/bp: this said
/// the cycle was non-moving because the band's thread holds a JIT depth,
/// `compiled_frames_live`; that has not been a divert term on its own since
/// moving young runs under live compiled frames -- the mark is what does it.)
/// Under the opt-in `CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT` a band this
/// pause credited ([`credit_in_native_proven_bands`]) is not marked: its depth
/// is in the coverage account and its young words in the pin ledger, which
/// keeps every object a band word names in place under the pinned copy.
///
/// Off (one latched load) unless the switch is on, so the default pause is
/// unchanged. The thread side records a band on Generational heaps, and on G1
/// only with `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE_G1` (gci i5g/g1r).
///
/// G1 (gci i5g/g1r, hunk 2 of
/// `docs/feature-designs/zgc-architecture.md`):
/// G1 relocates under live compiled frames and honours a word it cannot
/// rewrite only through a pin, so every root this scan adds (companions
/// included) is also pinned for the cycle through the take-over's per-cycle
/// set ([`pin_lazy_native_claim_roots`], read by G1's collection-set
/// exclusion, `jit_pinned_region_set`). Every caller runs this after
/// `stw_take_over_and_wait` (whose `reset_xt_cycle` would drop the pins) and
/// before the collection, so the pins land before G1 chooses its collection
/// set. A band word the probe answers with the aligned word itself (G1's
/// interior echo) pins the region it lies in, which is what the band's
/// derived pointer needs. The mark-only doors (G1 initial mark and remark)
/// pin too: harmless, the next take-over resets the set.
pub(in crate::runtime::interpreter::gc_and_alloc) fn append_in_native_band_roots(shared: &SharedVm, roots: &mut Vec<ObjectRef>) {
    if !crate::native::jni::in_native_band_at_pause_enabled_for(shared) {
        return;
    }
    // UNSOUND, debug only: the positive control of the band rows
    // (`vh4-bp.list` block B). With the bands unscanned, a compiled caller's
    // object held only in its frame across a blocking native is a lost root
    // (shown with `CRATONVM_JIT_MY_SHADOW_EMISSION=0`, where nothing else
    // carries the frame). Read per pause, only with the switch on.
    if false {
        eprintln!(
            "[jni-band] UNSOUND: this pause did not scan the in-native bands \
             (CRATONVM_DBG_JNI_BAND_SKIP_PAUSE_SCAN)"
        );
        return;
    }
    use crate::jit::xt_root_scan as xt;
    let census = &shared.natives.jni_phase_census;
    let clock = census.start();
    let heap = &shared.mem.heap;
    let companions = std::cell::RefCell::new(Vec::<ObjectRef>::new());
    let probe = |a: usize| {
        let r = xt::takeover_word_probe(heap, a);
        if let Some(c) = xt::takeover_word_companion(heap, a, r) {
            companions.borrow_mut().push(c);
        }
        r
    };
    let from = roots.len();
    let scan = shared
        .threads
        .thread_registry
        .scan_in_native_bands(&probe, roots);
    if scan.threads == 0 {
        return;
    }
    let companions = companions.into_inner();
    let roots_added = scan.roots + companions.len();
    roots.extend(companions);
    // gci i5g/g1r: on G1 the band roots are pins of this cycle as well (doc).
    if heap.is_g1() {
        pin_lazy_native_claim_roots(&roots[from..]);
    }
    // gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`, opt-in): not
    // when THIS pause credited every band it just scanned
    // ([`credit_in_native_proven_bands`]: each band's proven depth is in the
    // coverage account and its young words are in the pin ledger). One band
    // left uncredited marks the cycle exactly as before.
    if !in_native_bands_all_credited(shared) {
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
            cratonvm_gc::gc_quiescence::incomplete_reason::CROSS_THREAD_JIT_PEER,
        );
    }
    census.stop(crate::native::jni::JniPhase::PauseBandScan, clock);
    census.count(
        crate::native::jni::JniPhase::PauseBandThreads,
        scan.threads as u64,
    );
    census.count(
        crate::native::jni::JniPhase::PauseBandWords,
        scan.words as u64,
    );
    census.count(
        crate::native::jni::JniPhase::PauseBandRoots,
        roots_added as u64,
    );
    if false {
        eprintln!(
            "[jni-band] pause scanned bands={} words={} roots={roots_added}",
            scan.threads, scan.words
        );
    }
}

/// gcj j2/jn2 (cut 2 of
/// `gcj-j1jn-proposal-jni-transition-cost-ledger-and-three-cuts-20261019`,
/// `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB=1`, opt-in, Generational only): the
/// collecting pause's half of an in-native thread's kept TLAB. Called by
/// [`run_collection_pause`] right after `stw_take_over_and_wait`, i.e. with
/// the barrier satisfied (every counted thread parked, frozen or arrived; every
/// in-native thread unable to leave), before any root is gathered or anything
/// collected: every kept TLAB is retired on its owner's behalf
/// (`ThreadRegistry::retire_in_native_kept_tlabs`; HotSpot's
/// `ensure_parsability`), so its tail is a filler (or back with the young
/// allocator) and no longer a reserved span. The take-over published the
/// reserved tails just before; when this retired any, the set is published
/// again without them, so the moving young cycle's T-3 refusal
/// (`GenHeap`'s "a MOVING young collection was about to run with published
/// reserved TLAB tails") sees only the tails that really stay reserved.
///
/// The other Generational pauses (the concurrent cycle's initial mark and
/// remark, the non-moving pauses) leave a kept TLAB alone: none of them
/// resets the young space, and its tail is published as reserved like a
/// frozen peer's, so every linear walk skips it.
pub(in crate::runtime::interpreter::gc_and_alloc) fn retire_in_native_kept_tlabs(shared: &SharedVm) {
    if !crate::native::jni::in_native_keep_tlab_enabled_for(shared) {
        return;
    }
    let registry = &shared.threads.thread_registry;
    let retired = registry.retire_in_native_kept_tlabs();
    if retired == 0 {
        return;
    }
    shared
        .natives
        .jni_phase_census
        .count(crate::native::jni::JniPhase::TlabKeptRetiredByPause, retired as u64);
    shared
        .mem
        .heap
        .set_jit_tlab_skip_regions(&registry.collect_reserved_tlab_tails());
}

/// gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`, opt-in; proposal
/// P2/bp, `gch-h4bp-review-findings-RETIRED-UNFIXED-20261018` item 1): may this pause credit
/// JNI in-native bands at all? A Generational heap (the only collector whose
/// moving young cycle reads the coverage account and the young pin ledger),
/// the band-at-pause switch and this one on, and a ledger that licenses moves
/// (`gc_quiescence::young_pin_ledger_licenses_moves`: the pinned in-place
/// copy, on by default, is what keeps every young object a band word names
/// in place). Returns the pause stamp to credit under, or `None`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn in_native_credit_stamp(shared: &SharedVm) -> Option<u64> {
    if !shared.mem.heap.is_generational()
        || !crate::native::jni::in_native_band_at_pause_enabled_for(shared)
        || !crate::native::jni::in_native_proven_depth_credit_enabled_for(shared)
        || !cratonvm_gc::gc_quiescence::young_pin_ledger_licenses_moves()
    {
        return None;
    }
    // 0: this thread is bound to no pause's ledger -- nothing to credit into.
    Some(cratonvm_gc::gc_quiescence::young_pin_pause_stamp()).filter(|&s| s != 0)
}

/// gci i24/bp: the pause's half of the in-native proven-depth credit. Called by
/// the root gatherer (`memory::roots::collect_roots`) right before the
/// collection's moving-young coverage account
/// (`conservative_roots::refresh_moving_young_coverage_for_collection`), inside
/// the pause, on the requester (bound to its VM's ledgers).
///
/// For every JNI in-native thread whose band the pause will scan
/// ([`append_in_native_band_roots`]) and whose entry deposit PROVED its
/// compiled chain (`GcBlockState::in_native_proven_jit_depth`, equal to the
/// depth its OS thread published), the pause:
///
/// * credits the depth to the cross-thread coverage account
///   (`add_peer_proven_jit_depth`), as a cooperatively-parked peer that proved
///   the same chain at its park does (`publish_peer_jit_coverage_for_stw`) --
///   so `CROSS_THREAD_JIT_PEER` no longer fires for it;
/// * reads the band's young words, raw, into the young pin ledger with the same
///   depth (`note_peer_band_pin_words`, as the helper-window pass does for a
///   blocked-monitor peer credited by proof), so the ledger is not short of
///   that thread's depth, and every young object any band word names --
///   compiled frame, Rust frame or the dispatch's callee-saved spill --
///   stays in place under the pinned copy; nothing in the band is left naming a
///   vacated address;
/// * counts a conservative JIT scan (`note_conservative_jit_scan`): the band IS
///   one, and term 4 of the young decision must see it, so the only way such a
///   cycle relocates is through the ledger.
///
/// Why the frames are sound to resume: the thread cannot leave native while the
/// pause holds (`GcBarrier::leave_in_native_if`); its wake applies the fold's
/// fixups to its compiled frames, shadow stack and register images
/// (`vm_exec::apply_blocked_wake_jit_remap`, every shadow value is in its
/// snapshot) and its interpreter frames and JNI locals as for any in-native
/// thread; every young object a word of the band names did not move. The C
/// code holds only indirect locals (the package's own rule).
///
/// Off (three latched or snapshot loads) unless the switch is on. gcj j1/jn:
/// the credit switch itself is the VM's latched copy now
/// (`jni::in_native_proven_depth_credit_enabled_for`); it was a flag-snapshot
/// lookup, twice per pause.
pub(crate) fn credit_in_native_proven_bands(shared: &SharedVm) {
    let Some(stamp) = in_native_credit_stamp(shared) else {
        return;
    };
    let range = cratonvm_gc::gc_quiescence::young_pin_range();
    let young = |w: usize| range.contains(w);
    let mut credit = |words: &[usize], depth: usize| {
        cratonvm_gc::gc_quiescence::note_peer_band_pin_words(words, Some(depth), true, false);
        cratonvm_gc::gc_quiescence::add_peer_proven_jit_depth(depth);
        cratonvm_gc::gc_quiescence::note_conservative_jit_scan();
    };
    let done = shared.threads.thread_registry.credit_in_native_proven_bands(
        stamp,
        &cratonvm_gc::gc_quiescence::jit_depth_of_tid,
        &young,
        &mut credit,
    );
    if false
        && (done.credited + done.not_exempt + done.unproven + done.mismatched + done.already) > 0
    {
        eprintln!(
            "[xt-coverage] in-native credit stamp={stamp} credited={} depth={} words={} \
not_exempt={} unproven={} mismatched={} already={}",
            done.credited,
            done.depth,
            done.words,
            done.not_exempt,
            done.unproven,
            done.mismatched,
            done.already
        );
    }
}

/// gci i24/bp: did THIS pause credit every in-native band it reads
/// ([`credit_in_native_proven_bands`])? `false` whenever the credit is off or
/// unbound, so [`append_in_native_band_roots`] then marks the cycle as before.
pub(in crate::runtime::interpreter::gc_and_alloc) fn in_native_bands_all_credited(shared: &SharedVm) -> bool {
    in_native_credit_stamp(shared).is_some_and(|stamp| {
        shared
            .threads
            .thread_registry
            .uncredited_in_native_bands(stamp)
            == 0
    })
}

/// The first-pass grace when `CRATONVM_XT_FIRST_PASS_GRACE_US` is unset or
/// unparsable (gce ve2: time-to-safepoint 3-4x lower, `xt_pass_us` about 3 us,
/// wall time within 10 %).
pub(in crate::runtime::interpreter::gc_and_alloc) const DEFAULT_FIRST_PASS_GRACE_US: u64 = 200;

/// gcj j1yg-close: the first-pass grace of a GENERATIONAL heap's take-over
/// pause when `CRATONVM_XT_FIRST_PASS_GRACE_US` is unset (a typed value wins
/// on every backend). A peer the pass freezes in compiled code is a peer
/// Generational cannot move behind -- the take-over licence is `NonMoving`
/// and the young cycle sweeps in place (`ptkod_frozen`) -- while G1 and ZGC
/// pin its words and still move. On the shared 16-core host at load 30-50,
/// 4-thread in-process javac (`GciPn1JavacTreeProbe`) froze a peer on 4 of 26
/// young pauses at 200 us (a mutator descheduled for a few hundred
/// microseconds, not one stuck in code without polls); at 10 ms none froze
/// in 27 pauses. HotSpot waits for every thread.
pub(in crate::runtime::interpreter::gc_and_alloc) const DEFAULT_GENERATIONAL_FIRST_PASS_GRACE_US: u64 = 10_000;

/// `CRATONVM_XT_FIRST_PASS_GRACE_US` (gce e2/t; default 200 us since gce ve2,
/// 2026-09-29; `0` = off; capped at 10 ms): how long [`stw_take_over_and_wait`]
/// waits for the cooperative arrivals before its first take-over pass. Read
/// once per take-over pause.
pub(in crate::runtime::interpreter::gc_and_alloc) fn takeover_first_pass_grace() -> std::time::Duration {
    takeover_first_pass_grace_for(false)
}

/// [`takeover_first_pass_grace`] for a pause on a Generational heap when
/// `generational` (its unset default is
/// [`DEFAULT_GENERATIONAL_FIRST_PASS_GRACE_US`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn takeover_first_pass_grace_for(generational: bool) -> std::time::Duration {
    // zt t31/zc11: a ZGC mark start's own, longer grace (scoped to its pause
    // by `with_takeover_first_pass_grace`).
    if let Some(us) = TAKEOVER_GRACE_OVERRIDE_US.with(std::cell::Cell::get) {
        return std::time::Duration::from_micros(us.min(10_000));
    }
    let default = if generational {
        DEFAULT_GENERATIONAL_FIRST_PASS_GRACE_US
    } else {
        DEFAULT_FIRST_PASS_GRACE_US
    };
    let us = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default);
    std::time::Duration::from_micros(us.min(10_000))
}

thread_local! {
    /// zt t31/zc11: the first-pass grace of the take-over pause this thread is
    /// about to run, when a door asks for its own (`None`: the global one).
    /// Set only around `stw_take_over_and_wait` by
    /// [`with_takeover_first_pass_grace`]; a per-call scratch value.
    pub(in crate::runtime::interpreter::gc_and_alloc) static TAKEOVER_GRACE_OVERRIDE_US: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// zt t31/zc11: run `f` (a take-over pause request) with a first-pass grace of
/// `us` microseconds (`None`: the global grace), restoring the previous value.
pub(in crate::runtime::interpreter::gc_and_alloc) fn with_takeover_first_pass_grace<R>(us: Option<u64>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TAKEOVER_GRACE_OVERRIDE_US.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(TAKEOVER_GRACE_OVERRIDE_US.with(|c| c.replace(us)));
    f()
}

/// The ZGC mark-start grace when `CRATONVM_ZGC_MARK_START_GRACE_US` is unset
/// or unparsable (zt t31/zc11).
pub(in crate::runtime::interpreter::gc_and_alloc) const DEFAULT_ZGC_MARK_START_GRACE_US: u64 = 5_000;

/// `CRATONVM_ZGC_MARK_START_GRACE_US` (zt t31/zc11, program page stage 1a:
/// the take-over cover refused 10 of 28 mark starts on MTCHURN; default
/// 5 000 us, capped at 10 ms; `0` = the global `CRATONVM_XT_FIRST_PASS_GRACE_US`
/// grace, as before): how long a ZGC concurrent mark start's pause waits for
/// the cooperative arrivals before it takes over (freezes) a peer still in
/// compiled code. The take-over cover refuses to open a cycle whose mark start
/// froze anyone (a frozen peer may sit inside a barrier gate window), so every
/// freeze turns a concurrent cycle into a stop-the-world collection. A mark
/// start is not urgent -- the heap still has the reserve the trigger left --
/// and on a box with more runnable mutators than cores the 200 us grace
/// expires while a descheduled peer waits for its quantum, which a grace of a
/// few milliseconds covers; a peer that arrives early ends the wait at once.
/// HotSpot's mark start is a cooperative safepoint with no take-over. Read
/// once per mark start.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_mark_start_grace_us() -> Option<u64> {
    let us = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_ZGC_MARK_START_GRACE_US);
    (us != 0).then_some(us)
}


/// The roster threads the pause's census did not count: the only ones a
/// take-over pass still owes a signal once every counted mutator has arrived.
///
/// gcf f1/r: a sorted copy of the census answers membership, not a linear
/// `contains` per roster thread -- that was `O(roster * census)` inside the
/// pause, four million compares for a 2,000-thread server. Same answer, in
/// roster order.
pub(in crate::runtime::interpreter::gc_and_alloc) fn uncounted_roster(live_tids: &[u32], counted_os_tids: &[u32]) -> Vec<u32> {
    let mut counted = counted_os_tids.to_vec();
    counted.sort_unstable();
    live_tids
        .iter()
        .copied()
        .filter(|tid| counted.binary_search(tid).is_err())
        .collect()
}

/// JIT round 18 wave 10 (lane ffmeintr): one `CRATONVM_DBG_STW_WATCHDOG_SECS`
/// report of a stop-the-world wait that has outlived the watchdog: the
/// barrier's outstanding count and every registered thread's state
/// (`ThreadRegistry::debug_stw_watchdog_census`, with the OS side of each
/// peer from `xt_root_scan::debug_peer_os_state`). Runs between take-over
/// rounds, where no helper slot holds a peer.
pub(in crate::runtime::interpreter::gc_and_alloc) fn stw_watchdog_dump(
    shared: &SharedVm,
    what: &str,
    waited: std::time::Duration,
    counted_os_tids: &[u32],
    taken_tids: &[u32],
) {
    let census = shared.threads.thread_registry.debug_stw_watchdog_census(
        counted_os_tids,
        taken_tids,
        &crate::jit::xt_root_scan::debug_peer_os_state,
    );
    eprintln!(
        "[stw-watchdog] {what} has waited {waited:?}: pending={} counted={} taken={} \
         blocked_counter={}{census}",
        shared.mem.gc_barrier.pending_count(),
        counted_os_tids.len(),
        taken_tids.len(),
        shared.mem.gc_barrier.blocked_count(),
    );
}

/// gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`): add to
/// `out` every object a FROZEN peer's JIT deopt stashes hold (`LAST_DEOPT`,
/// `LAST_EXCEPTIONAL`), read through the cells `state` published
/// (`ThreadRegistry::publish_os_tid_claim` ->
/// `GcBlockState::publish_deopt_stash_cells`), and only when their publisher's
/// OS tid is in `frozen_os_tids`. Returns `(stashed frames, roots added)`.
///
/// The stashes are thread-locals. Their owner offers them to a root scan only
/// on its own thread (`collect_roots` §10, `deposit_root_snapshot_inner`), so
/// a frame stashed after the owner's last deposit -- a trap's frame between
/// the deopt stub and the VM sink that drains it, while the `i64::MIN`
/// sentinel unwinds through compiled callers, or a leftover no consumer took
/// -- names objects no root set held while a take-over kept the owner frozen
/// in compiled code. Only ADDS roots: they join `xt_roots` (non-moving sweep on
/// Generational, `pin_frozen_peer_roots_for_g1` on G1, the take-over pin set),
/// so nothing they name moves and the owner has nothing to remap.
pub(in crate::runtime::interpreter::gc_and_alloc) fn push_frozen_peer_deopt_stash_roots(
    state: &crate::threading::jvm_thread::GcBlockState,
    frozen_os_tids: &[u32],
    heap: &crate::memory::VmHeap,
    out: &mut Vec<ObjectRef>,
) -> (usize, usize) {
    let Some((os_tid, deopt, exceptional)) = state.published_deopt_stash_cells() else {
        return (0, 0);
    };
    if !frozen_os_tids.contains(&os_tid) {
        return (0, 0);
    }
    let before = out.len();
    let push = |addr: u64| {
        // Screened like a frozen peer's operand-stack word.
        if heap.is_heap_addr(addr as usize).is_some() {
            // SAFETY: a screened heap address held by a stashed frame.
            out.push(unsafe { ObjectRef::from_raw(addr as *mut u8) });
        }
    };
    // SAFETY: both cells were published from the TLS of the live OS thread
    // `os_tid`, which the caller holds frozen for the whole call (it is in
    // `frozen_os_tids`), so its thread-locals are alive and unmutated.
    let (frames, _offered) = unsafe {
        cratonvm_jit::deopt::for_each_stashed_deopt_object_of_frozen_peer(deopt, exceptional, push)
    };
    (frames, out.len() - before)
}


pub(in crate::runtime::interpreter) fn stw_take_over_and_wait(
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
    // JIT round 18 wave 10 (lane ffmeintr): `CRATONVM_DBG_STW_WATCHDOG_SECS`,
    // read once per pause.
    let watchdog =
        crate::threading::thread_registry::stw_watchdog_secs().map(std::time::Duration::from_secs);
    // JIT round 19 wave 1 (lane eintr19, FFMEINTR18-5,
    // `CRATONVM_STW_STALL_REPORT`, default ON): without the watchdog, ONE
    // dump of the same census after the last-resort threshold, by
    // `eprintln!` (the 64-round warning below is a `tracing::warn!`, which a
    // run without a subscriber never shows: the R17 battery's hangs printed
    // nothing).
    let stall_after = if watchdog.is_none() {
        crate::threading::thread_registry::stw_stall_report_after()
    } else {
        None
    };
    let wait_started = std::time::Instant::now();
    if !xt::enabled() || !shared.mem.heap.supports_jit_tlab_skip() {
        match watchdog.or(stall_after) {
            // The same wait in 1 s slices, with the dump between them.
            Some(every) => {
                let mut next = every;
                while !shared
                    .mem
                    .gc_barrier
                    .wait_for_all_timeout(std::time::Duration::from_secs(1))
                {
                    if wait_started.elapsed() >= next {
                        stw_watchdog_dump(
                            shared,
                            if watchdog.is_some() {
                                "stop-the-world wait (no take-over)"
                            } else {
                                "[stw-stall] stop-the-world wait (no take-over)"
                            },
                            wait_started.elapsed(),
                            counted_os_tids,
                            &[],
                        );
                        // The last-resort report is once per pause.
                        next = if watchdog.is_some() {
                            next + every
                        } else {
                            std::time::Duration::MAX
                        };
                    }
                }
            }
            None => shared.mem.gc_barrier.wait_for_all(),
        }
        // nio21 wave 18 L5: the lazy blocked deposit's chains are owed by
        // EVERY root-gathering pause, not only one that takes peers over.
        // This early return (`CRATONVM_XT_JIT_ROOT_SCAN=0`) used to skip the
        // resolution below, so with `CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT=1`
        // a blocked thread's compiled-frame roots were neither in its
        // snapshot nor marked: a missed root. The barrier is satisfied, so
        // every blocked thread is held in its region here as below.
        if gathers_roots {
            let _ = crate::jit::conservative_roots::resolve_deferred_blocked_jit_chains(
                shared, xt_roots,
            );
        }
        if !gathers_roots {
            wait_for_unclaimed_lazy_native_peers(shared);
        }
        // Round 13 wave 4 (lane mega3): this wait freezes nobody, so every
        // counted mutator is at a poll or blocked -- none between an
        // inline-cache compare and its entry load.
        cratonvm_jit::note_grace_at_cooperative_stop();
        return xt::TakenOver::default();
    }
    let mut taken = xt::TakenOver::default();
    // zt t26/zc5b item 1 (`CRATONVM_XT_SKIP_TLAB_POINTER_SLOTS`, default ON):
    // every registered thread's `Tlab` pointer words, which the band scans of
    // the frozen and blocked peers below must not read as references (a
    // peer's `JvmThread` -- its `Tlab` included -- lives on its own stack).
    // Gathered once, before any peer is signalled; a pause that gathers no
    // roots needs none.
    if gathers_roots && xt::skip_tlab_pointer_slots_enabled() {
        taken.set_tlab_pointer_slots(shared.threads.thread_registry.tlab_pointer_slots());
    }
    // zwr: the delivery threads, whose published Rust band floor caps a frozen
    // one's stack band (`xt::cap_at_rust_band_floor`).
    if gathers_roots {
        taken.set_rust_band_floors(shared.threads.thread_registry.delivery_thread_states());
    }
    // gci i3/tk (`CRATONVM_GEN_TAKEOVER_PROMOTES`,
    // `CRATONVM_GEN_HELPER_WINDOW_REFUSAL_PROMOTES`, both opt-in, Generational
    // only; the section above `xt::pause_keeps_promotion`): this pause's
    // static promotion licences, set before the first pass so every per-peer
    // mark agrees, and the take-over shadow census at the start, for the
    // pause-wide licence after both passes. A pause that gathers no roots
    // collects nothing and keeps both off.
    let generational = shared.mem.heap.is_generational();
    taken.takeover_promotes = gathers_roots
        && xt::takeover_promotes_licence(
            generational,
            xt::gen_takeover_promotes_enabled(),
            xt::takeover_interior_enabled(),
            xt::takeover_above_chain_starts_enabled(),
            crate::jit::conservative_roots::xt_peer_shadow_scan_enabled(),
        );
    taken.helper_refusal_promotes = gathers_roots
        && xt::helper_refusal_promotes_licence(
            generational,
            xt::gen_helper_window_refusal_promotes_enabled(),
            xt::helper_window_pin_resolve_enabled(),
            xt::helper_window_root_starts_enabled(),
        );
    let takeover_shadow_untrusted_at =
        xt::XT_TAKEOVER_SHADOW_UNPUBLISHED.load(std::sync::atomic::Ordering::Relaxed);
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
    let mut next_watchdog_dump = watchdog;
    let mut stall_reported = false;
    // gcj j2/cm2 (`gcj-j1cm-safepoint-barrier-and-registry-review-residuals`
    // item 6): the census's OS tids sorted ONCE per pause, at the first round
    // that froze or claimed someone, for the excusal's membership test below
    // (a `Vec::contains` per newly frozen peer was O(frozen x counted) per
    // round). Same answer.
    let mut counted_sorted: Option<Vec<u32>> = None;
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
    let grace = takeover_first_pass_grace_for(shared.mem.heap.is_generational());
    let grace_met = !grace.is_zero() && shared.mem.gc_barrier.wait_for_all_timeout(grace);
    if grace_met {
        // Lazy FFM downcalls are deliberately absent from the mutator barrier.
        // The cooperative fast path therefore cannot treat a satisfied
        // barrier as proof that every native peer is quiescent: claim them
        // before the uncounted-thread pass, just as the retry loop below does.
        // Collection pauses need their roots; no-roots pauses use the claim
        // only to hold the native peer stable until the pause ends.
        let _ = claim_lazy_native_peers(shared, gathers_roots, &mut taken, xt_roots);
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
        // JIT round 16 wave 14 (lane ffm3): every round, before any peer is
        // signalled, claim the threads that are in C through a lazy
        // in-native FFM downcall; they join the frozen set (and its excusal
        // below) and are never signalled.
        let claimed = claim_lazy_native_peers(shared, gathers_roots, &mut taken, xt_roots);
        let should_scan =
            stw_takeover_should_scan(rounds, crate::jit::conservative_roots::any_thread_in_jit());
        let scanned = if should_scan {
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
        let newly = claimed + scanned;
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
            // JIT round 19 wave 1 (lane eintr19, FFMEINTR18-4,
            // `CRATONVM_XT_EXCUSE_UNBLOCKED_ONLY`, default ON): the census's
            // OS-tid list names BLOCKED threads too, which `expected` never
            // counted; a thread in a blocked region now was either blocked at
            // the census (never expected) or entered its region during this
            // pause (and arrived through the region's `_auto` arrival). Either
            // way excusing it would over-reduce `expected` and release the
            // barrier while a counted mutator still runs. Unreachable today
            // (no frozen or claimed thread is in a blocked region); one
            // registry read per round that froze or claimed someone.
            // gcj j2/cm2: answered sorted (`blocked_among_os_tids`), so the
            // membership test is a binary search.
            let blocked_now = {
                shared
                    .threads
                    .thread_registry
                    .blocked_among_os_tids(&taken.tids[tids_before..])
            };
            let counted_sorted = counted_sorted.get_or_insert_with(|| {
                let mut v = counted_os_tids.to_vec();
                v.sort_unstable();
                v
            });
            for &tid in &taken.tids[tids_before..] {
                if blocked_now.binary_search(&tid).is_ok() {
                    // gci i3/tk: the word rule of `xt_root_scan::dbg` (was presence).
                    if false {
                        eprintln!(
                            "[xt-jit-roots] froze or claimed os_tid={tid} inside a blocked region: \
                             not excused (FFMEINTR18-4)"
                        );
                    }
                } else if counted_sorted.binary_search(&tid).is_ok() {
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
        // JIT round 18 wave 10 (lane ffmeintr): `CRATONVM_DBG_STW_WATCHDOG_SECS`.
        if let (Some(at), Some(every)) = (next_watchdog_dump, watchdog) {
            if wait_started.elapsed() >= at {
                stw_watchdog_dump(
                    shared,
                    if gathers_roots {
                        "stop-the-world take-over wait (collection)"
                    } else {
                        "stop-the-world take-over wait (no-roots pause)"
                    },
                    wait_started.elapsed(),
                    counted_os_tids,
                    &taken.tids,
                );
                eprintln!(
                    "[stw-watchdog] rounds={rounds} native_claims={} grace_met={grace_met}",
                    taken.native_claim_count()
                );
                next_watchdog_dump = Some(at + every);
            }
        }
        // JIT round 19 wave 1 (lane eintr19, FFMEINTR18-5): the default-on
        // last resort, once per pause.
        if let Some(after) = stall_after {
            if !stall_reported && wait_started.elapsed() >= after {
                stall_reported = true;
                stw_watchdog_dump(
                    shared,
                    if gathers_roots {
                        "[stw-stall] stop-the-world take-over wait (collection)"
                    } else {
                        "[stw-stall] stop-the-world take-over wait (no-roots pause)"
                    },
                    wait_started.elapsed(),
                    counted_os_tids,
                    &taken.tids,
                );
                eprintln!(
                    "[stw-stall] rounds={rounds} native_claims={} grace_met={grace_met} \
                     (CRATONVM_STW_STALL_REPORT=0 silences; CRATONVM_DBG_STW_WATCHDOG_SECS=N repeats)",
                    taken.native_claim_count()
                );
            }
        }
        if !warned
            && rounds >= WARN_AFTER_ROUNDS
            && !cratonvm_native_builtins_jfr::jfr_vm_more::sampling_in_progress()
        {
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
            if None::<std::ffi::OsString>.is_some()
                || false
            {
                eprintln!(
                    "[stw-census] rounds={rounds} pending={pending} taken={} blocked={} alive={}{}",
                    taken.count(),
                    shared.mem.gc_barrier.blocked_count(),
                    shared.threads.thread_registry.alive_count(),
                    shared.threads.thread_registry.debug_thread_census()
                );
                if None::<std::ffi::OsString>.is_some() {
                    cratonvm_native_api::native_ring::dump_to_stderr();                }            }
            warned = true;        }    }
    if !gathers_roots {
        wait_for_unclaimed_lazy_native_peers(shared);
    }
    // Round 13 wave 4 (lane mega3, `r13w2-mega-grace-at-cooperative-stop-patch`):
    // the quota is met and nobody is released yet. Only a stop that froze
    // NOBODY proves no mutator is between an inline-cache compare and its
    // entry load; a frozen peer may be stopped anywhere.
    if taken.unrewritable_count() == 0 {
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
    if taken.unrewritable_count() > 0 && gathers_roots {
        let frozen_os_tids = taken.unrewritable_os_tids();
        let peer_addrs = shared
            .threads
            .thread_registry
            .frozen_peer_thread_addrs(&frozen_os_tids);
        let contributed_from = xt_roots.len();
        // gci i9/jw: roots read from the frozen peers' JNI local tables.
        let mut jni_locals = 0usize;
        // gci i9/ds: frozen peers whose JIT deopt stashes were non-empty, their
        // stashed frames, and the roots read from them.
        let (mut stash_peers, mut stash_frames, mut stash_roots) = (0usize, 0usize, 0usize);
        for addr in peer_addrs {
            // SAFETY: see the block comment above.
            let peer: &crate::threading::jvm_thread::JvmThread =
                unsafe { &*(addr as *const crate::threading::jvm_thread::JvmThread) };
            for frame in peer.frames.iter() {
                // gch h1/jp (f1f parity): the debugger answer every root scan
                // shares (`Frame::scan_local_objects_for_vm`).
                frame.scan_local_objects_for_vm(shared, xt_roots);
                let sb = xt_roots.len();
                // Wave 122, lane L7: the same stack rule as every other root
                // walk under `CRATONVM_INTERP_PRECISE_OOP_MAPS`.
                frame.scan_stack_objects_mapped(
                    xt_roots,
                    &shared.mem.heap,
                    crate::runtime::value_stack::MINTED_ONLY_PRIMITIVE_STACK_ROOTS,
                    Some(&*shared.classes.type_maps),
                );
                if xt_roots.len() > sb {
                    // Operand-stack candidates are screened exactly like the
                    // deposit path (`scan_frame_roots`) — `is_heap_addr`, so a
                    // pointer-shaped primitive long cannot become a root while
                    // a young / mid-init object still can. The strict
                    // `is_object_address` probe used here until 2026-08-04
                    // dropped the second population, and this peer is FROZEN:
                    // whatever its frames hold, only this scan can speak for.
                    // gcf f2/r: filtered in place (same order, same survivors)
                    // rather than through a `split_off` Vec per frame, which
                    // allocated inside the pause.
                    let mut keep = sb;
                    let mut i = sb;
                    while i < xt_roots.len() {
                        let o = xt_roots[i];
                        if shared.mem.heap.is_heap_addr(o.as_ptr() as usize).is_some() {
                            xt_roots[keep] = o;
                            keep += 1;
                        }
                        i += 1;
                    }
                    xt_roots.truncate(keep);
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
            // gci i9/jw (`gci-i8orch-takeover-promotes-jna-upcall-wrong-result-FIXED-20261017`):
            // the peer's JNI LOCAL references. Its `JNI_LOCAL_FRAMES` table is
            // thread-local, its deposited snapshot predates every local minted
            // since its last deposit, and with indirect locals its native's
            // stack holds a `(depth, slot)` tag the machine-stack scan cannot
            // resolve: a JNA callback's argument array, built by
            // `NewObjectArray` before `NewObject` ran `Pointer.<init>` in
            // compiled code, was swept while the peer sat frozen there. Only
            // ADDS roots, pinned with the rest of `xt_roots`.
            jni_locals += crate::native::jni::push_frozen_peer_local_refs(
                &peer.gc_block_state,
                &frozen_os_tids,
                &shared.mem.heap,
                xt_roots,
            );
            // gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`):
            // the peer's JIT deopt stashes (`LAST_DEOPT` / `LAST_EXCEPTIONAL`),
            // thread-local like its JNI table and as invisible to its last
            // deposit for a frame stashed since. Only ADDS roots, pinned with the
            // rest of `xt_roots`.
            let (frames, roots) = push_frozen_peer_deopt_stash_roots(
                &peer.gc_block_state,
                &frozen_os_tids,
                &shared.mem.heap,
                xt_roots,
            );
            if frames > 0 {
                stash_peers += 1;
                stash_frames += frames;
                stash_roots += roots;            }        }
        if false {
            eprintln!(
                "[xt-frame-scan] frozen_peers={} contributed_roots={} jni_locals={jni_locals}",
                taken.count(),
                xt_roots.len() - contributed_from
            );
            // gci i9/ds census: "frozen with a non-empty stash".
            eprintln!(
                "[xt-deopt-stash] frozen_peers={} stash_peers={stash_peers} stash_frames={stash_frames} stash_roots={stash_roots}",
                taken.count()
            );
        }
    }
    // JIT round 16 wave 16 (lane ffm3): a peer claimed in C out of a lazy
    // in-native downcall resumes without a fixup, so nothing it names may
    // move. Unrewritable claims publish an explicit frozen-peer verdict,
    // consumed by the collectors without modifying the JIT depth. Pin every frozen
    // or claimed peer's root (band, shadow stack, interpreter frames, thread
    // fields) for this cycle, so a pinnable backend that relocates anyway
    // (G1's collection set, ZGC's slide on page pins) withholds their pages.
    if gathers_roots && taken.native_claim_count() > 0 && xt::lazy_claim_pin_enabled() {
        pin_lazy_native_claim_roots(xt_roots);
    }

    // A4 (fork6-fjp) — helper-window coverage. The barrier is satisfied, so
    // every remaining un-scanned root holder is a BLOCKED thread (excluded via
    // `threads_blocked`, covered by its `deposit_root_snapshot`, whose JIT-chain
    // scan -- eager, or deferred to `resolve_deferred_blocked_jit_chains` below
    // -- leaves the Rust frames around the chain to this pass; gci i25/rv, the
    // hunk of i24/jp's review item 4). A worker blocked in `join()`/park under
    // JIT-compiled `runWorker`/`doExec` frames that are the sole holder of a
    // forked subtask would otherwise lose it to the non-moving sweep (the
    // Fork6 stale all-zero receivers). Scan each remaining peer's register
    // file + used stack once, contributing roots only when the stack actually
    // carries a JIT return address. Gated to collections where the gap can
    // exist at all: a blocked thread while some thread holds live JIT frames.
    //
    // gci i4/wr (census only): where the helper-window part of `xt_roots`
    // begins, for the old-mark root census's take-over / helper split.
    taken.note_census_helper_from(xt_roots.len());
    let mut helper_windows = 0usize;
    if gathers_roots
        && xt::helper_window_scan_enabled()
        // gcg g4/xt: not `blocked_count() > 0`, which is always 0 under
        // `CRATONVM_STW_NO_BLOCKED_COUNTER`; see `may_have_blocked_threads`.
        && shared.mem.gc_barrier.may_have_blocked_threads()
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
        // Every blocked peer is read, compiled frames or not. gcg g6/xt deleted
        // the gcf f1/r narrowing that skipped peers whose published JIT depth
        // proved no compiled frame (`xt-helper-window-jit-only`, flipped on in
        // g5): a pure-native blocked peer's band is where the blocked-wake
        // stack-slot repair takes every word it rewrites (`xt_root_scan`,
        // `record_helper_stack_captures`; vg4 `xt4_cap_*`), and with the
        // narrowing on, vg5 `xt5_cap_g1_mtc` captured 0 words and
        // `xt5_cap_g1_hwc` repaired 0, where the same rows on g4b repaired
        // 163 and 259. A pause-time saving does not buy an unrepaired stale
        // Rust local.
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
        // JIT round 18 wave 10 (lane ffmeintr, EINTR2-1, opt-in
        // `CRATONVM_XT_HELPER_UNPARKED_READ`): the blocked peers waiting
        // inside a blocked-wait trampoline, whose bands the pass reads without
        // a signal. Empty with the switch off: every peer is parked.
        let unparked = if crate::threading::jvm_thread::helper_unparked_read_enabled() {
            shared.threads.thread_registry.blocked_wait_band_states()
        } else {
            Vec::new()
        };
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
        //
        // gci i20/vh (`CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE`, G1
        // only; default ON since gci i24/jp, `=0` kills -- the flip's marker is
        // on the reader): a blocked peer whose deposit deferred a complete compiled
        // chain takes the pure-native path in the pass (captures kept for the
        // wake repair, no pins); `resolve_deferred_blocked_jit_chains` below
        // marks and pins its compiled frames.
        //
        // ki3 (`CRATONVM_XT_HELPER_WINDOW_COVERED_CHAIN_PIN_ONLY`, default ON):
        // on G1 the demotion takes every peer whose window's deposit covered
        // its whole compiled chain, eagerly as well as deferred (a superset of
        // the deferred roster). A pinned word holds a referent through G1's
        // young referent verdict (`g1_pinned_referents`), so on G1 the
        // pin-only rule below would still keep a dead referent alive.
        let covered = xt::helper_window_covered_chain_pin_only_enabled();
        if shared.mem.heap.is_g1() && xt::helper_window_deferred_chain_native_enabled() {
            let registry = &shared.threads.thread_registry;
            taken.set_deferred_chain_peers(if covered {
                registry.blocked_os_tids_with_covered_jit_chain()
            } else {
                registry.blocked_os_tids_with_deferred_jit_chain()
            });
        }
        // ki3: on every other collector (and on G1 with the demotion off), a
        // blocked peer whose window's deposit covered its whole compiled
        // chain keeps its window's pins but not its marking roots
        // (`TakenOver::commit_window_roots`); the words set aside still get
        // their companions below.
        if covered {
            taken.set_covered_chain_peers(
                shared
                    .threads
                    .thread_registry
                    .blocked_os_tids_with_covered_jit_chain(),
            );
        }

        // JIT round 21 lane refs, REFS21-6 (opt-in
        // `CRATONVM_XT_HELPER_WINDOW_PARK_CREDIT`): the blocked peers parked in
        // an armed Java-level park whose deposit proved their compiled chain.
        // The pass still reads them (stack-slot captures, depth, shadow stack)
        // and still PINS their register file and stack words (lane
        // parkcredit: nothing they name may move), but takes no MARKING root
        // from those words. Withdrawn right after the pass.
        if cratonvm_gc::gc_quiescence::xt_park_credit_enabled() {
            let credited = shared.threads.thread_registry.park_credited_os_tids();
            // Third gate's DBG line (`CRATONVM_DBG_REFERENT_HOLDER`).
            if crate::runtime::env_cache::dbg_referent_holder() {
                eprintln!(
                    "[xt-park-credit] gate=credited blocked_os_tids={blocked_os_tids:?} credited={credited:?}"
                );
            }
            cratonvm_gc::gc_quiescence::set_xt_park_credited_tids(credited);
        }
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
            // g1ep7: the heap's address envelope, read once per pass. A word
            // outside it names no object (`VmHeap::conservative_addr_span` is
            // a filter, never an answer), so it never reaches the probe's
            // out-of-line call: on `G1epKeepRounds 10 -Xmx1g` the pass
            // classified about 70 k words of three parked peers' stacks on
            // every pause, 1.4 s of the pausing thread. The end is inclusive:
            // a one-past-the-end word is still probed.
            let envelope = shared.mem.heap.conservative_addr_span();
            let r = xt::helper_window_pass(
                &taken,
                &|a| {
                    if envelope.is_some_and(|(lo, hi)| a < lo || a > hi) {
                        return None;
                    }
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
                &unparked,
            );
            let echoed = echoed.into_inner();
            let mut companions: Vec<ObjectRef> = Vec::new();
            // ki3: a covered window's words (`TakenOver::commit_window_roots`)
            // were pinned, not committed; their companions are pinned the
            // same way and not rooted either.
            let pin_only = taken.take_pin_only_roots();
            let mut pin_only_companions: Vec<ObjectRef> = Vec::new();
            if !echoed.is_empty() {
                let mut seen = rustc_hash::FxHashSet::<usize>::default();
                let committed = xt_roots[committed_from..].iter().map(|o| (o, true));
                for (o, rooted) in committed.chain(pin_only.iter().map(|o| (o, false))) {
                    let addr = o.as_ptr() as usize;
                    if !echoed.contains(&addr) || !seen.insert(addr) {
                        continue;
                    }
                    if let Some(c) =
                        xt::helper_window_word_companion(&shared.mem.heap, addr, Some(*o))
                    {
                        if rooted {
                            companions.push(c);
                        } else {
                            pin_only_companions.push(c);
                        }
                    }
                }
            }
            if !pin_only_companions.is_empty() {
                let addrs: Vec<usize> = pin_only_companions
                    .iter()
                    .map(|o| o.as_ptr() as usize)
                    .collect();
                cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                xt::XT_HELPER_WINDOW_COMPANIONS.fetch_add(
                    pin_only_companions.len() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            if !companions.is_empty() {
                // Pinned unconditionally, including a refusing window's: over-
                // pinning costs one region/page of compaction for one pause,
                // and a refusing window keeps the cycle non-moving anyway.
                let addrs: Vec<usize> = companions.iter().map(|o| o.as_ptr() as usize).collect();
                cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                xt::XT_HELPER_WINDOW_COMPANIONS.fetch_add(
                    companions.len() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                // Into this cycle's `hw_roots` too, so the widening is visible
                // on the `[GC] xt_peer_scan` line and A/B-able against
                // `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=0`.
                cratonvm_gc::gc_quiescence::publish_xt_helper_window(0, companions.len() as u64);
                xt_roots.extend(companions);
            }
            // gcg g3/h (`CRATONVM_XT_HELPER_WINDOW_ROOT_STARTS`, default ON
            // since gci-user-flip-20261013, `=0` kills; Generational only):
            // the window's MARKING roots narrow to object starts; every
            // candidate and companion above is already pinned. With the
            // switch off (`=0`) this only prints the census (what it would
            // drop) under `CRATONVM_DBG=oldmark-root-census`.
            xt::helper_window_keep_root_starts(
                &shared.mem.heap,
                xt_roots,
                committed_from,
                xt::helper_window_root_starts_enabled(),
            );
            r
        } else if interior {
            xt::helper_window_pass(
                &taken,
                &|a| shared.mem.heap.is_heap_addr(a),
                xt_roots,
                &blocked_os_tids,
                &unparked,
            )
        } else {
            xt::helper_window_pass(
                &taken,
                &|a| shared.mem.heap.is_object_address(a),
                xt_roots,
                &blocked_os_tids,
                &unparked,
            )
        };
        helper_windows = windows;
        cratonvm_gc::gc_quiescence::clear_xt_park_credited_tids();
    }
    // nio21 wave 17 L3 (`CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT`, opt-in): the
    // compiled-chain walks blocked threads deferred at their blocking deposit,
    // run here on their frozen frames -- marked through `xt_roots` and pinned
    // for this pause, as the eager deposit marked and pinned them. One cached
    // flag load with the switch off. See `conservative_roots::DeferredJitChain`.
    if gathers_roots {
        let _ =
            crate::jit::conservative_roots::resolve_deferred_blocked_jit_chains(shared, xt_roots);
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
    // gci i3/tk: truthy-word rule (was presence, so `=0` used to restore
    // the pre-fix guard); the switch is opt-in with no other reader.
    if false {
        if taken.count() > 0 || helper_windows > 0 || !regions.is_empty() {
            shared.mem.heap.set_jit_tlab_skip_regions(&regions);
        }
    } else {
        shared.mem.heap.set_jit_tlab_skip_regions(&regions);
    }
    // gci i27/gs (P1/ga, `gcd-d10t-proposal-takeover-probe-skips-reserved-tlab-tails-20260928`;
    // `CRATONVM_XT_TAKEOVER_DROP_TAIL_WORDS`, default OFF): the TLAB-tail
    // screen of the take-over roots. A root strictly inside a reserved tail
    // whose memory still reads unallocated names no object (gcd d10/t), so it
    // leaves the marking roots and G1's pin list here, before either reads
    // them. The tail's own region stays pinned on G1 through the span just
    // published (`jit_pinned_region_set_screened`), a root at a span start
    // is kept, and a span that holds anything keeps every root for the guard.
    if gathers_roots && !regions.is_empty() && cratonvm_gc::heap::takeover_drop_tail_words_enabled()
    {
        // SAFETY: `regions` are the reserved tails collected in THIS pause,
        // with every owner stopped (the barrier above), which is
        // `drop_roots_inside_unallocated_spans`' contract.
        let _ = unsafe { cratonvm_gc::heap::drop_roots_inside_unallocated_spans(&regions, xt_roots) };
    }
    // Complete claimed native bands have the same pin contract as helper
    // windows. Their exact managed deposit is already eligible for rewriting.
    helper_windows += taken.pinned_native_claim_count();
    if taken.unrewritable_count() > 0 || helper_windows > 0 {
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
        let helper_only = taken.unrewritable_count() == 0 && helper_windows > 0;
        let discharged =
            discharge_enabled && helper_only && xt::helper_windows_all_pinned_this_cycle();
        if !discharged {
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete();
            // gci i9/ng (i8/pe finding B): name this refusal in the first-wins
            // reason scalar, which the fallback histogram reads (it said
            // `none`). Label only: the mask, the verdict and the un-rewritable
            // bit are untouched, so no decision moves (ZGC refuses on a zero
            // mask; the old-pin plan reads the mask's peer-state bits).
            cratonvm_gc::gc_quiescence::label_moving_young_coverage_incomplete(
                takeover_refusal_label(taken.unrewritable_count()),
            );
            // gci i3/tk: the pause-wide promotion licence (opt-in; both
            // switches off -> `keeps` is false and this is the old line).
            let facts = xt::PausePromoteFacts {
                frozen: taken.count(),
                native_claims: taken.native_claim_count(),
                takeover_licensed: taken.takeover_promotes,
                stack_incomplete: !matches!(
                    cratonvm_gc::gc_quiescence::xt_takeover_stack_incomplete_this_pause(),
                    Some(0)
                ),
                takeover_shadow_untrusted: xt::XT_TAKEOVER_SHADOW_UNPUBLISHED
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .saturating_sub(takeover_shadow_untrusted_at),
                unclassified: cratonvm_gc::gc_quiescence::xt_cycle_coverage().2,
                helper_windows,
                helper_licensed: taken.helper_refusal_promotes,
                helper_relieved: taken.helper_refusal_relieved(),
                helper_refusals: xt::helper_windows_unpinned_this_cycle(),
            };
            let keeps = xt::pause_keeps_promotion(&facts);
            if (taken.takeover_promotes || taken.helper_refusal_promotes)
                && crate::jit::helper_window_refusal::census_on()
            {
                eprintln!("[xt-promote-licence] keeps={keeps} {facts:?}");
            }
            if !keeps {
                cratonvm_gc::gc_quiescence::mark_unrewritable_peer_state();
            }
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
            frozen: u32::try_from(taken.unrewritable_count()).unwrap_or(u32::MAX),
            helper_windows: u32::try_from(helper_windows).unwrap_or(u32::MAX),
            unreadable: u32::try_from(unreadable).unwrap_or(u32::MAX),
            pins_complete: !stack_incomplete && xt::helper_windows_all_pinned_this_cycle(),
            derived_pointers_resolved: (taken.unrewritable_count() == 0
                || xt::takeover_interior_enabled())
                && (helper_windows == taken.pinned_native_claim_count()
                    || xt::helper_window_pin_resolve_enabled()),
        },
    );
    if taken.native_claim_count() > 0
        && false
    {
        eprintln!(
            "[ffm-lazy] takeover verdict {:?}",
            cratonvm_gc::gc_quiescence::takeover_verdict()
        );
    }
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
pub(in crate::runtime::interpreter::gc_and_alloc) fn takeover_verdict_unreadable(
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


/// `CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE=1` -- a discharged helper-window
/// cycle still declares UNREWRITABLE PEER STATE.
///
/// The helper-window discharge suppressed two flags where it had an argument
/// for only one. See the call site.
pub(in crate::runtime::interpreter::gc_and_alloc) fn keep_unrewritable_peer_state_on_discharge() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>
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
pub(in crate::runtime::interpreter) fn pin_frozen_peer_roots_for_g1(
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
        .root_snapshots_for_os_tids(&taken.unrewritable_os_tids())
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
pub(in crate::runtime::interpreter::gc_and_alloc) static REMAP_TRACE_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[inline(always)]
pub(crate) fn remap_trace_on() -> bool {
    match REMAP_TRACE_STATE.load(std::sync::atomic::Ordering::Relaxed) {
        0 => remap_trace_on_init(),
        s => s == 2,
    }
}

#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::gc_and_alloc) fn remap_trace_on_init() -> bool {
    // Racing initialisers compute the same answer from the same latched
    // flag snapshot, so a plain store is enough.
    let on = None::<std::ffi::OsString>.is_some();
    REMAP_TRACE_STATE.store(if on { 2 } else { 1 }, std::sync::atomic::Ordering::Relaxed);
    on
}

thread_local! {
    pub(in crate::runtime::interpreter::gc_and_alloc) static REMAP_TRACE: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
    pub(in crate::runtime::interpreter::gc_and_alloc) static NRET_RING: std::cell::RefCell<Vec<(usize, usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    pub(in crate::runtime::interpreter::gc_and_alloc) static GETFIELD_RING: std::cell::RefCell<Vec<(usize, usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

thread_local! {
    pub(in crate::runtime::interpreter::gc_and_alloc) static DEPOSIT_GAP_RING: std::cell::RefCell<Vec<(usize, String)>> =
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
pub(in crate::runtime::interpreter::gc_and_alloc) fn deposit_gap_diff_cold(thread: &JvmThread, snapshot: &[crate::types::ObjectRef], tag: &str) {
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
    pub(in crate::runtime::interpreter::gc_and_alloc) static PUSH_PROV_RING: std::cell::RefCell<Vec<(usize, String)>> =
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
pub(in crate::runtime::interpreter::gc_and_alloc) fn push_prov_record_cold(addr: usize, site: &str) {
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
pub(in crate::runtime::interpreter) fn record_shuffle_push(cv: crate::types::CompactValue, site: &str) {
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
pub(in crate::runtime::interpreter::gc_and_alloc) fn getfield_ring_record_cold(parent: usize, idx: usize, pushed: usize) {
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
pub(in crate::runtime::interpreter::gc_and_alloc) fn remap_trace_push_cold(shared: &SharedVm, thread: &JvmThread, tag: &str, extra: &str) {
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
pub(in crate::runtime::interpreter::gc_and_alloc) fn nret_record_cold(cb: usize, addr: usize, site: &str) {
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
pub(in crate::runtime::interpreter) fn finalizable_roots(shared: &SharedVm) -> Vec<usize> {
    let mut addrs = {
        let rp = shared.mem.ref_processor.lock();
        rp.finalizer_referent_addresses()
    };
    addrs.extend(shared.mem.finalizer_thread.pending_addresses());
    addrs.sort_unstable();
    addrs.dedup();
    addrs
}

/// gcj j3/x2 R1 (proposal `gcj-j3x2-proposal-queued-finalizables-are-strong-roots`):
/// `CRATONVM_GC_QUEUED_FINALIZABLES_ARE_ROOTS` -- default ON on the
/// Generational and G1 backends (`=0` kills), opt-in (`=1`) on ZGC. Read once
/// per collection pause by the door ([`pause_finalizable_inputs`]).
///
/// Collector-scoped because HotSpot's answer is: under Serial and G1 a weak
/// reference to the old child of a queued finalizable survives a
/// `System.gc()` while the finalizable waits (`Gcj3X2QueuedFinalizableOldChildProbe`,
/// `weak-alive-while-queued true`, 5/5 runs each), under ZGC it is cleared
/// (`false`, 5/5) -- and the candidate treatment already answers ZGC's.
///
/// An object the finalizer thread has CLAIMED and queued for `finalize()` is
/// strongly reachable in HotSpot (`Finalizer.queue`), so its whole closure is
/// strong: a weak reference into it is not cleared while it waits, and it is
/// never judged dead again. The door handed the queue to the collector as
/// resurrection CANDIDATES beside the registered finalizables, so a queued
/// object's children were the resurrection closure -- a weak reference to an
/// old child cleared at a major (`CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`
/// extended that from old queued objects to young ones), and to any child at
/// a young collection that found the queued object unreached.
pub(in crate::runtime::interpreter::gc_and_alloc) fn queued_finalizables_are_roots(shared: &SharedVm) -> bool {
    cratonvm_types::flags::runtime_flag_default_on_if(
        "CRATONVM_GC_QUEUED_FINALIZABLES_ARE_ROOTS",
        shared.mem.heap.is_generational() || shared.mem.heap.is_g1(),
    )
}

/// The collection door's finalizable inputs (gcj j3/x2 R1): the resurrection
/// candidates, and the queued objects to push as STRONG roots. With
/// [`queued_finalizables_are_roots`] the candidates are the registered set
/// only and the queue is the root list (the queue's addresses are relocated
/// by every collection through the pointer map, `FinalizerThread::update_after_gc`,
/// as any root's are); without it, [`finalizable_roots`] and no extra roots,
/// as before. A queued object that is also still registered (not possible
/// today: `mark_finalizer_enqueued` drops it from the registered set) is a
/// root, never a candidate.
pub(in crate::runtime::interpreter::gc_and_alloc) fn pause_finalizable_inputs(shared: &SharedVm) -> (Vec<usize>, Vec<ObjectRef>) {
    if !queued_finalizables_are_roots(shared) {
        return (finalizable_roots(shared), Vec::new());
    }
    let mut queued = shared.mem.finalizer_thread.pending_addresses();
    queued.sort_unstable();
    queued.dedup();
    queued.retain(|&a| a != 0);
    let registered = {
        let rp = shared.mem.ref_processor.lock();
        rp.finalizer_referent_addresses()
    };
    (
        split_registered_from_queued(registered, &queued),
        queued
            .into_iter()
            // SAFETY: an address the finalizer queue holds, relocated by every
            // collection; a root word is carried and screened by the collector,
            // never dereferenced here.
            .map(|a| unsafe { ObjectRef::from_raw(a as *mut u8) })
            .collect(),
    )
}

/// The registered finalizables that are not in `queued_sorted` (sorted,
/// deduplicated), sorted and deduplicated -- [`pause_finalizable_inputs`]'s
/// candidate list. Split out so the selection is tested without a heap.
pub(in crate::runtime::interpreter::gc_and_alloc) fn split_registered_from_queued(mut registered: Vec<usize>, queued_sorted: &[usize]) -> Vec<usize> {
    registered.sort_unstable();
    registered.dedup();
    registered.retain(|a| queued_sorted.binary_search(a).is_err());
    registered
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
/// well, through the same processor call. All three backends record it
/// (Generational w36-d; G1 and ZGC 2026-10-10); see
/// `docs/feature-designs/real-jdk-mode.md`.
///
/// (Until w7-d this function sat between [`enqueue_resurrected_finalizers`]
/// and that function's doc comment, so rustdoc glued the two docs onto this
/// one and left the enqueue undocumented.)
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_resurrected_finalizables_for_reference_processing(
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
pub(in crate::runtime::interpreter::gc_and_alloc) fn resurrected_pre_gc_addresses(
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
/// `docs/feature-designs/real-jdk-mode.md`;
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
    // (labelled "Allocation Failure", like every latch collection; only a
    // pending `jcmd GC.run` is "Diagnostic Command", gci i2/oo) at the next
    // allocation on any thread. The
    // collection this call starts, or the one it takes part in, satisfies the
    // request either way. Still one relaxed load when the latch is clear.
    let entry_needs = occupancy_poll && shared.mem.heap.needs_gc();
    // zr z3/cs (`CRATONVM_ZGC_CONC_EVERY_TRIGGER`): the `needs_gc` just asked
    // may have turned a trigger into a concurrent-cycle request instead of a
    // collection; open it here rather than one poll later. One match and one
    // relaxed load with the switch off (a compile-time `false` off ZGC).
    if occupancy_poll && !entry_needs && shared.mem.heap.zgc_conc_start_requested() {
        zgc_concurrent_mark_cycle(shared, thread);
    }
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
        // gcf f4/r: a usage threshold a direct old-generation allocation just
        // crossed (`PoolThresholds::check_usage_on_growth`) is delivered
        // through the hint every queued notification uses. Only on an
        // occupancy poll -- after an allocation outside the TLAB, which is
        // where the old generation grows directly; one match and two loads.
        if occupancy_poll && super::gc_events::gc_notifications_pending(shared) {
            shared.mem.gc_barrier.note_ref_work_queued();
        }
        drain_ref_work_queued_elsewhere(shared, thread);
        return;
    }
    if entry_needs {
        cratonvm_types::gc_entry_census::note_maybe_gc_needs();
    } else {
        cratonvm_types::gc_entry_census::note_maybe_gc_requested();
    }
    // gci i2/oo: a pending `jcmd GC.run` rides the latch
    // (`request_diagnostic_command_gc` raises both). It is HotSpot's full
    // `_dcmd_gc_run`, not this door's young collection; it satisfies the
    // occupancy trigger too. Then the epilogue's ordinary drains.
    if entry_requested && take_diagnostic_command_gc_request(shared) {
        run_diagnostic_command_gc(shared, thread);
        run_finalizers(shared, thread);
        run_cleaner_actions(shared, thread);
        if !gc_notifications_go_to_delivery_thread(shared, thread) {
            run_gc_notifications(shared, thread, false);
        }
        return;
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
    // taken part in it, which is all a losing initiator ever did -- or (zt
    // t24/zc8, `CRATONVM_ZGC_MARK_CONTINUE`) that its ZGC Pause Mark End
    // attempt released the world with mark work left; the next poll at the
    // fixed point tries again. Only the occupancy trigger opens that door: a
    // latched request (`jcmd`, the serviceability trigger) always collects.
    let mark_continue = ZgcMarkContinueDoor::open(entry_needs && !entry_requested);
    if run_collection_pause(shared, thread, GcDoor::AllocationThreshold).is_none() {
        return;
    }
    drop(mark_continue);
    if let Some(t0) = wedged_at_entry {
        ladder_census(
            shared,
            cratonvm_gc::gen_heap::oome_ladder::THRESHOLD_WEDGED,
            1,
        );
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
    // zt t12/gn4: a `--compatible` ZGC door inside a JIT borrow cannot
    // deliver (the drain below defers), so it hands them off instead
    // ([`zgc_jit_borrowed_door_hands_off_gc_notifications`]).
    if !gc_notifications_go_to_delivery_thread(shared, thread)
        && !zgc_jit_borrowed_door_hands_off_gc_notifications(shared, thread)
    {
        run_gc_notifications(shared, thread, false);
    }
}

/// Kill switch of [`maybe_gc`]'s refill-driven occupancy poll. `true` asks the
/// two occupancy triggers after every interpreted allocation again, as before
/// round i1 wave 20 — a `const`, so the two arms can be priced in one source
/// tree without an environment variable.
pub(in crate::runtime::interpreter::gc_and_alloc) const FULL_GC_POLL_AFTER_EVERY_NEW: bool = false;

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
pub(in crate::runtime::interpreter::gc_and_alloc) const GC_OCCUPANCY_POLL_BYTES: u64 = 16 * 1024;

/// Whether [`maybe_gc`] must ask the occupancy triggers now: the countdown
/// ([`JvmThread::gc_occupancy_poll_at`], in this thread's allocated-bytes
/// units) has run out or was reset by a refill / shared allocation.
///
/// `Tlab::thread_allocated_bytes` is monotone (the carried total plus the live
/// buffer's consumption, JIT inline bumps included), so the comparison is a
/// byte countdown that needs no per-allocation store.
#[inline(always)]
pub(in crate::runtime::interpreter::gc_and_alloc) fn occupancy_poll_due(thread: &JvmThread) -> bool {
    FULL_GC_POLL_AFTER_EVERY_NEW
        || thread.tlab.thread_allocated_bytes() >= thread.gc_occupancy_poll_at
}

/// Start the next countdown of [`GC_OCCUPANCY_POLL_BYTES`] from this thread's
/// current allocated-bytes total.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn arm_occupancy_poll_countdown(thread: &mut JvmThread) {
    thread.gc_occupancy_poll_at = thread
        .tlab
        .thread_allocated_bytes()
        .saturating_add(GC_OCCUPANCY_POLL_BYTES);
}

/// This thread just did something that moves the heap's occupancy — carved a
/// TLAB chunk or allocated outside its TLAB — so the next [`maybe_gc`] asks the
/// occupancy triggers. A store to a field of the thread's own struct.
#[inline(always)]
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_occupancy_may_have_moved(thread: &mut JvmThread) {
    thread.gc_occupancy_poll_at = 0;
}

