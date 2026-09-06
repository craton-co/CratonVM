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

/// Raised for the duration of a [`stw_publish_frame_traces`] pause; read by
/// `safepoint_check` on the arriving side.
///
/// Process-global rather than per-thread because the request is scoped to one
/// stop-the-world, and only one of those runs at a time — a second requester
/// finds `request_stw` already taken and gives up. A per-thread flag would buy
/// nothing and cost a registry lookup at every safepoint park.
static FRAME_TRACE_WANTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Is a cross-thread stack dump in flight? See [`stw_publish_frame_traces`].
#[inline]
pub(crate) fn frame_trace_wanted() -> bool {
    FRAME_TRACE_WANTED.load(std::sync::atomic::Ordering::Relaxed)
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
    let mut counted_os_tids: Vec<u32> = Vec::new();
    // Raised BEFORE the request, and lowered by `Drop` on every path out.
    //
    // A guard rather than two `store(false)` calls because the failure mode of
    // missing one is silent and permanent: the flag left raised makes EVERY
    // safepoint park — i.e. every mutator on every GC pause, forever after —
    // capture and allocate a frame trace nobody asked for. That is a cost with
    // no symptom, which is the kind this codebase keeps finding late.
    struct WantedGuard;
    impl Drop for WantedGuard {
        fn drop(&mut self) {
            FRAME_TRACE_WANTED.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
    // Before the request, not after: a mutator that reaches its poll in between
    // would otherwise park without publishing, and this pause gets no second
    // chance to ask it.
    FRAME_TRACE_WANTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let _wanted = WantedGuard;

    let stw_taken = shared
        .mem
        .gc_barrier
        .request_stw_counted_with_live_blocked(initiator, || {
            let (n, blocked, tids, blocked_tids) = shared
                .threads
                .thread_registry
                .alive_count_blocked_and_os_tids();
            counted_os_tids = tids;
            (
                u32::try_from(n).unwrap_or(u32::MAX),
                u32::try_from(blocked).unwrap_or(u32::MAX),
                blocked_tids,
            )
        });
    if !stw_taken {
        return false;
    }
    // `stw_take_over_and_wait`, not the plain `wait_for_all()`: a peer spinning
    // in compiled code that never reaches a poll is FROZEN and scanned in place
    // rather than waited on forever. Such a peer never runs `safepoint_check`
    // and so publishes nothing — its trace stays as last deposited, which is
    // the pre-existing answer and never worse than it.
    let mut xt_roots: Vec<ObjectRef> = Vec::new();
    let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
    // Nothing to do in the pause itself: the work is what the ARRIVING threads
    // did on their way in. Nothing moves, so there is no pointer map.
    crate::jit::xt_root_scan::resume(taken);
    shared
        .mem
        .gc_barrier
        .complete_gc(cratonvm_types::PointerMap::default());
    true
}

#[cfg(test)]
mod frame_trace_request_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// The request flag must read `false` when nobody is dumping.
    ///
    /// This is the whole cost argument for gating the publish in
    /// `safepoint_check`: raised, every mutator captures and allocates a frame
    /// trace on every GC pause. A flag that latched on would not fail any test
    /// — it would just quietly make every pause more expensive — so assert the
    /// resting state explicitly.
    #[test]
    fn the_request_flag_rests_low() {
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
    /// so this exercises the guard that owns the discipline rather than the
    /// function around it. Breaking `Drop` (or replacing the guard with a
    /// hand-written `store(false)` that an early `return` can skip) fails this.
    #[test]
    fn the_request_guard_lowers_the_flag_on_every_path() {
        struct WantedGuard;
        impl Drop for WantedGuard {
            fn drop(&mut self) {
                FRAME_TRACE_WANTED.store(false, Ordering::Relaxed);
            }
        }
        fn take(early_out: bool) -> bool {
            FRAME_TRACE_WANTED.store(true, Ordering::Relaxed);
            let _wanted = WantedGuard;
            if early_out {
                return false;
            }
            true
        }
        for early_out in [true, false] {
            let raised_during = {
                FRAME_TRACE_WANTED.store(true, Ordering::Relaxed);
                frame_trace_wanted()
            };
            assert!(raised_during, "the flag must be observable while raised");
            let _ = take(early_out);
            assert!(
                !frame_trace_wanted(),
                "the flag stayed raised after the early_out={early_out} path"
            );
        }
    }
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
    loop {
        let tids_before = taken.tids.len();
        let should_scan =
            stw_takeover_should_scan(rounds, crate::jit::conservative_roots::any_thread_in_jit());
        let newly = if should_scan {
            xt::take_over_pass(
                &mut taken,
                &|a| shared.mem.heap.is_object_address(a),
                xt_roots,
            )
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
            // (`request_stw_counted_with_live_blocked` /
            // `alive_count_blocked_and_os_tids`) actually excludes threads
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
    if taken.count() > 0 {
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
                frame.stack.scan_object_refs(xt_roots, &shared.mem.heap);
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
            }
            for r in peer.native_pin_roots.iter() {
                xt_roots.push(*r);
            }
            if let Some(r) = peer.native_pending_return {
                xt_roots.push(r);
            }
            // TOMCAT-JNDIREALM-JIT.3 — a frozen peer's per-thread native
            // caches live in NO frame, so the frame walk above cannot reach
            // them. A peer taken over mid-JIT is exactly a thread whose
            // deposited snapshot is stale, so these must be contributed here
            // too (same set as `deposit_root_snapshot_inner` publishes).
            for entry in peer.jit_hashmap_string_node_cache.iter() {
                xt_roots.push(entry.map);
                xt_roots.push(entry.node);
                if let Some(key_object) = entry.key_object {
                    xt_roots.push(key_object);
                }
            }
            for entry in peer.string_case_cache.iter() {
                xt_roots.push(entry.source);
                xt_roots.push(entry.first);
                xt_roots.push(entry.second);
                if let Some(locale) = entry.locale {
                    xt_roots.push(locale);
                }
            }
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
    if xt::helper_window_scan_enabled()
        && shared.mem.gc_barrier.blocked_count() > 0
        && crate::jit::conservative_roots::any_thread_in_jit()
    {
        // xt-hardening follow-up (2026-07-03): scope the pass to threads
        // ACTUALLY in a blocked region (the only gap it exists to close —
        // see helper_window_pass's doc comment). A cooperatively-arrived
        // mutator already published its JIT roots via update_root_snapshot;
        // re-scanning it only widens the conservative-candidate volume that
        // feeds the mark-phase writer, with zero coverage benefit.
        let blocked_os_tids = shared.threads.thread_registry.blocked_os_tids();
        // WHICH PREDICATE, and it is the open question on
        // `bug-h2-testcachedqueryresults-zgc-oom-livelock-20260829`.
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
        // `CRATONVM_XT_HELPER_WINDOW_INTERIOR=1` is that measurement, and only
        // that: it changes which words become conservative roots and pins, and
        // changes NOTHING about the refusal, which both this site and
        // `xt_root_scan` still raise unconditionally. Compare `hw_roots` on the
        // `[GC] xt_peer_scan` line between the arms.
        // The discharge IMPLIES the interior probe: pinning is only complete
        // when a derived pointer resolves to the base that must not move, so
        // the two cannot be selected independently.
        let interior = xt::helper_window_discharge_enabled()
            || cratonvm_types::flags::runtime_var_os("CRATONVM_XT_HELPER_WINDOW_INTERIOR")
                .is_some();
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
        let pin_resolve = xt::helper_window_pin_resolve_enabled();
        let (windows, _roots) = if pin_resolve {
            xt::helper_window_pass(
                &taken,
                &|a| shared.mem.heap.resolve_interior_for_pin(a),
                xt_roots,
                &blocked_os_tids,
            )
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
    // for a one-binary A/B. With it set, `io.netty.util.internal
    // .ObjectCleanerTest` returns to 6/8 non-clean under Generational and 8/8
    // under G1, and the `cratonvm::gc::guard` "a ROOT points into a published
    // TLAB skip span" error fires -- which is also what proves that guard is
    // not vacuous.
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
        let discharged = xt::helper_window_discharge_enabled()
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
    taken
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
pub(crate) fn remap_trace_on() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_REMAP_TRACE").is_some())
}

thread_local! {
    static REMAP_TRACE: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static NRET_RING: std::cell::RefCell<Vec<(usize, usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static GETFIELD_RING: std::cell::RefCell<Vec<(usize, usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Record a reference-typed getfield push: (parent, field_index, pushed).
thread_local! {
    static DEPOSIT_GAP_RING: std::cell::RefCell<Vec<(usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// After a snapshot build, diff it against a RAW walk of the frames: ring
/// every Object-decoding local/stack slot whose address the snapshot lacks.
pub(crate) fn deposit_gap_diff(
    thread: &JvmThread,
    snapshot: &[crate::types::ObjectRef],
    tag: &str,
) {
    if !remap_trace_on() {
        return;
    }
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
pub(crate) fn push_prov_record(addr: usize, site: &str) {
    if !remap_trace_on() {
        return;
    }
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

pub(crate) fn getfield_ring_record(parent: usize, idx: usize, pushed: usize) {
    if !remap_trace_on() {
        return;
    }
    GETFIELD_RING.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() >= 96 {
            r.remove(0);
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

pub(crate) fn remap_trace_push(shared: &SharedVm, thread: &JvmThread, tag: &str, extra: &str) {
    if !remap_trace_on() {
        return;
    }
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

pub(crate) fn nret_record(cb: usize, addr: usize, site: &str) {
    if !remap_trace_on() {
        return;
    }
    NRET_RING.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() >= 64 {
            r.remove(0);
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

pub(crate) fn maybe_gc(shared: &SharedVm, thread: &mut JvmThread) {
    // First, check if another thread requested STW — if so, participate
    safepoint_check(shared, thread);

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
    if shared.mem.heap.zgc_should_start_concurrent_mark() {
        zgc_concurrent_mark_cycle(shared, thread);
    }

    // Evaluated into named locals rather than left in the `||`: the two
    // halves are different answers to "why did this cycle happen", and the
    // latch half is invisible to `[GC] zgc-trigger` because it never asks
    // `needs_gc`. Short-circuiting is preserved -- `needs_gc()` first, and
    // the swap only when it says no -- so the latch is still consumed
    // exactly when it was before.
    let entry_needs = shared.mem.heap.needs_gc();
    let entry_requested = !entry_needs
        && shared
            .mem
            .gc_requested
            .swap(false, std::sync::atomic::Ordering::Relaxed);
    if entry_needs || entry_requested {
        if entry_needs {
            cratonvm_types::gc_entry_census::note_maybe_gc_needs();
        } else {
            cratonvm_types::gc_entry_census::note_maybe_gc_requested();
        }
        // Retire TLAB before GC — its memory is in from-space
        thread.tlab.retire();
        // Round-5 fix (CRIT — UAF): the GC initiator never passes through
        // `safepoint_check`'s arrive_and_wait, so drain its OWN per-thread
        // SATB buffer here. Without this the initiator's last up-to-255
        // overwritten references vanish on every cycle; the bug bites
        // hardest in single-threaded mode where the initiator IS every
        // mutator.
        shared.mem.heap.flush_thread_satb();
        // Update our root snapshot before requesting STW
        cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
        update_root_snapshot(shared, thread);
        mtroots_set_gc_ctx(shared, thread, 2); // 2 = alloc-young (maybe_gc)
        mtroots_dump_initiator(shared, thread, 2);

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

        // Truncation-checked: alive_count (usize) to u32; thread count realistically bounded
        let alive_count =
            u32::try_from(shared.threads.thread_registry.alive_count()).unwrap_or(u32::MAX);
        if alive_count <= 1 {
            // Single-threaded fast path: no barrier needed
            let gc_start = std::time::Instant::now();
            let mut roots = collect_roots(shared, thread);
            // STW invariant: single-threaded path means this thread is
            // the only mutator — every other thread is implicitly
            // "parked" (it doesn't exist). Construct the token directly.
            // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
            weakref_null_referents_pre_gc(shared);
            // SAFETY: the STW invariant stated just above holds — every other
            // mutator is parked at a safepoint (or was forcibly stopped and
            // conservatively scanned), so this thread is the only mutator and
            // may assert exclusive heap access.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
            let fin_roots = finalizable_roots(shared);
            let result = shared
                .mem
                .heap
                .collect_garbage_with_finalizers(
                    &stw,
                    &mut roots,
                    &fin_roots,
                    &shared.threads.monitors,
                )
                .0;
            process_references_after_gc(shared, &result.pointer_map, &roots);
            update_all_roots(shared, thread, &result.pointer_map);
            // DBG (bc math-ec, CRATONVM_DBG_ECWATCH): the moving collector
            // relocated survivors — REMAP each watched holder through the
            // pointer_map so watches PERSIST across this GC (the corruption
            // frequently hits an object that survived the GC that wrote it).
            crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
            // GC-EXIT detect: a watched cell that was clean at GC ENTRY (above)
            // but reads 0x4 here was corrupted *by collect_garbage itself*
            // (between entry and exit) — isolating GC-vs-mutator definitively.
            if crate::runtime::ec_watch::enabled() {
                for (holder, idx, expected, now) in
                    crate::runtime::ec_watch::detect(shared.vm_identity)
                {
                    eprintln!(
                        "[ecwatch-GCEXIT] holder@0x{holder:x} fld[{idx}]: 0x{expected:x} -> 0x{now:x} (corrupted DURING collect_garbage)"
                    );
                }
            }
            // bc math-ec 0x4 (CRATONVM_DBG_MEMWATCH): post-GC poll of the
            // watched absolute address — a HIT here (vs at a mutator
            // safepoint) means the flip happened inside collect_garbage /
            // reference processing.
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
            // DBG (env-gated): validate every young object's header size against
            // its class — pins a JIT `new` that wrote a wrong-size header.
            crate::memory::gc::validate_object_sizes(shared);
            // DBG: run the heap-stale verifier after EVERY GC (incl. the
            // non-moving JIT-active sweep, where update_all_roots early-returns
            // on the empty pointer_map). It flags any LIVE object whose field
            // points to a ZEROED/reclaimed object — i.e. a live object the sweep
            // wrongly reclaimed (missing root). The referrer names the bug.
            crate::memory::gc::verify_heap_object_fields(shared, &result.pointer_map);
            // DBG (CRATONVM_DBG_CORRUPT_FRAMES): on the FIRST GC that detects
            // sweep corruption, dump the mutator's Java stack. With a tiny young
            // gen (frequent GC) this fires close to the JIT corruptor — the
            // interpreted frame on top is the BC method that called the
            // JIT-compiled corruptor.
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_CORRUPT_FRAMES").is_some()
                && cratonvm_gc::gen_heap::SWEEP_CORRUPTION_HITS
                    .load(std::sync::atomic::Ordering::Relaxed)
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
            // Truncation-checked: as_millis returns u128 but GC duration fits u64
            let gc_duration_ms = u64::try_from(gc_start.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracing::debug!(
                "GC completed (single-thread): {} objects copied, {} bytes freed, {}ms",
                result.stats.objects_copied,
                result.stats.bytes_freed,
                gc_duration_ms,
            );
            // Record JFR GC event
            {
                let mut jfr = shared.debug.flight_recorder.lock();
                let now_ns = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    // Truncation-checked: nanos since epoch fits u64 until year ~2554
                    .as_nanos() as u64;
                cratonvm_jfr::builtin::emit_gc_event(
                    &mut jfr,
                    1, // gc_id
                    "YoungGC",
                    "Allocation Failure",
                    now_ns.saturating_sub(gc_duration_ms * 1_000_000),
                    gc_duration_ms * 1_000_000,
                );
                cratonvm_jfr::builtin::emit_young_gc_event(
                    &mut jfr,
                    1,
                    15, // default tenuring threshold
                    now_ns.saturating_sub(gc_duration_ms * 1_000_000),
                    gc_duration_ms * 1_000_000,
                );
                // Truncation-checked: heap bytes (usize) to i64; heaps > 8 EiB are unrealistic
                let heap_used =
                    i64::try_from(shared.mem.heap.allocated_bytes()).unwrap_or(i64::MAX);
                cratonvm_jfr::builtin::emit_gc_heap_summary_event(
                    &mut jfr,
                    1,
                    "After GC",
                    "Young Gen",
                    heap_used,
                    heap_used,     // committed ≈ used for our simple heap
                    heap_used * 2, // max ≈ 2x used estimate
                    now_ns,
                );
            }
            // T19.3.G1 — bump the cycle counter so operators can
            // measure GC frequency against the 0.2 Hz allocation-storm
            // target.
            shared
                .mem
                .gc_cycle_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        } else {
            // Multi-threaded path: coordinate via GC barrier
            let mut counted_os_tids: Vec<u32> = Vec::new();
            let should_initiate_gc = {
                // xt-hardening (2026-07-03): snapshot the counted alive
                // set's OS tids atomically with the expected computation
                // (same closure, same barrier lock) for identity-based
                // takeover excusal.
                counted_os_tids.clear();
                shared.mem.gc_barrier.request_stw_counted_with_live_blocked(
                    thread.thread_id,
                    || {
                        let (n, blocked, tids, blocked_tids) = shared
                            .threads
                            .thread_registry
                            .alive_count_blocked_and_os_tids();
                        counted_os_tids = tids;
                        // DIAGNOSTIC (2026-07-13, STW takeover 5-class cluster
                        // investigation): print the EXACT identity set counted
                        // as "expected" (alive AND NOT in_blocked_region) at
                        // the instant this pause is requested, to disambiguate
                        // whether a thread later seen parked was already
                        // excluded at request time or genuinely raced in.
                        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STW_EXPECTED_IDS")
                            .is_some()
                        {
                            let expected_ids: Vec<u64> = shared
                                .threads
                                .thread_registry
                                .alive_thread_ids_excluding(&blocked_tids);
                            eprintln!(
                                "[stw-expected] initiator={} n={} blocked={} expected_ids={:?}",
                                thread.thread_id.0, n, blocked, expected_ids
                            );
                        }
                        (
                            u32::try_from(n).unwrap_or(u32::MAX),
                            u32::try_from(blocked).unwrap_or(u32::MAX),
                            blocked_tids,
                        )
                    },
                )
            };
            if should_initiate_gc {
                // We are the GC initiator. BUG-03 — forcibly stop in-JIT
                // peers and conservatively scan them before waiting for the
                // cooperative mutators.
                let mut xt_roots: Vec<ObjectRef> = Vec::new();
                let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);

                // Collect roots: current thread + all snapshots + shared state
                let mut roots = collect_roots(shared, thread);
                let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
                roots.extend(snapshot_roots);
                // INT-3 (G1) — everything a frozen peer can address must not
                // move; must follow collect_roots (which clears the pins).
                pin_frozen_peer_roots_for_g1(shared, &xt_roots, &taken);
                // BUG-03 — conservative roots from forcibly-stopped in-JIT peers.
                roots.extend(xt_roots);

                // STW invariant: `gc_barrier.wait_for_all()` returned, so
                // every mutator has parked at its safepoint poll OR (BUG-03)
                // been forcibly stopped in JIT and conservatively scanned.
                // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
                weakref_null_referents_pre_gc(shared);
                // SAFETY: the STW invariant stated just above holds — every other
                // mutator is parked at a safepoint (or was forcibly stopped and
                // conservatively scanned), so this thread is the only mutator and
                // may assert exclusive heap access.
                let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
                let fin_roots = finalizable_roots(shared);
                let result = shared
                    .mem
                    .heap
                    .collect_garbage_with_finalizers(
                        &stw,
                        &mut roots,
                        &fin_roots,
                        &shared.threads.monitors,
                    )
                    .0;
                process_references_after_gc(shared, &result.pointer_map, &roots);

                // Update shared VM state (statics, string pool, etc.)
                update_all_roots(shared, thread, &result.pointer_map);
                // DBG (bc math-ec): remap watchpoints through the pointer_map.
                crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);

                tracing::debug!(
                    "GC completed (multi-thread, {} threads): {} objects copied, {} bytes freed",
                    alive_count,
                    result.stats.objects_copied,
                    result.stats.bytes_freed,
                );

                // BUG-03 — drop the TLAB skip regions and resume the
                // forcibly-stopped in-JIT peers now that the heap is
                // consistent again (non-moving sweep on Generational /
                // pinned-in-place regions + walker-skipped TLAB tails on G1
                // (INT-3) → their pointers are unchanged). xt-hardening
                // (2026-07-03): BOTH must happen
                // BEFORE `complete_gc` reopens the world — a released mutator
                // could otherwise win the NEXT STW, re-freeze the
                // still-suspended peers and publish fresh skip regions that
                // THIS initiator's late clear would wipe, letting the next
                // sweep walk (and free-list) the frozen peers' reserved
                // tails. A resumed peer that immediately requests the next
                // GC blocks until `complete_gc` anyway (the barrier is still
                // closed here), so the reorder introduces no new window.
                shared.mem.heap.clear_jit_tlab_skip_regions();
                crate::jit::xt_root_scan::resume(taken);
                // Signal all threads with the pointer map
                shared.mem.gc_barrier.complete_gc(result.pointer_map);

                // T19.3.G1 — bump the cycle counter (multi-threaded
                // path, fires only on the GC initiator).
                shared
                    .mem
                    .gc_cycle_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // After minor GC, check if old gen needs concurrent collection
                maybe_concurrent_gc(shared, thread);
                // Run any pending finalizers
                run_finalizers(shared, thread);
                // ...and any pending Cleaner actions — see the single-threaded
                // arm above for why an allocation-triggered GC must do this
                // too, not only the forced `System.gc()` path.
                run_cleaner_actions(shared, thread);
            } else {
                // Another thread is already doing GC — just participate
                safepoint_check(shared, thread);
            }
        }
    }
}

/// Force a GC cycle regardless of threshold.
/// Used by allocation helpers when the fast-path allocation fails.
/// Public wrapper so sibling modules (exceptions, invokedynamic) can force a
/// GC cycle when a direct allocation fails.
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
pub fn g1_drive_concurrent_mark_pub(shared: &SharedVm, thread: &mut JvmThread) {
    if shared.mem.heap.g1_is_marking_active() {
        if shared.mem.heap.g1_concurrent_mark_finished() {
            g1_final_remark_cleanup(shared, thread);
        }
        return;
    }
    if shared.mem.heap.g1_should_start_marking() {
        g1_concurrent_mark_cycle(shared, thread);
    }
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
pub(crate) fn create_string_or_oom(
    shared: &SharedVm,
    thread: &mut JvmThread,
    text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    use crate::vm::try_create_java_string_uninterned as try_new_string;
    if let Some(obj) = try_new_string(shared, text) {
        return Ok(obj);
    }
    // Same escalation ladder as `alloc_object_shared`: forced young/full GC,
    // then G1's last-ditch complete mark cycle (dead Old/humongous spans are
    // only reclaimed by a finished cycle's cleanup), then OOM.
    thread.tlab.retire();
    maybe_gc_forced_at(shared, thread, "create-string");
    if let Some(obj) = try_new_string(shared, text) {
        return Ok(obj);
    }
    last_ditch_reclaim(shared, thread);
    if let Some(obj) = try_new_string(shared, text) {
        return Ok(obj);
    }
    maybe_dump_heap_on_oom(shared, thread);
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::OutOfMemoryError {
            message: format!("Java heap space (String of {} chars)", text.len()),
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
    thread.tlab.retire();
    maybe_gc_forced_at(shared, thread, "create-string-units");
    if let Some(obj) = try_new_string(shared, units) {
        return Ok(obj);
    }
    last_ditch_reclaim(shared, thread);
    if let Some(obj) = try_new_string(shared, units) {
        return Ok(obj);
    }
    maybe_dump_heap_on_oom(shared, thread);
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::OutOfMemoryError {
            message: format!("Java heap space (String of {} chars)", units.len()),
        },
    )))
}

pub(super) fn maybe_gc_forced(shared: &SharedVm, thread: &mut JvmThread) {
    maybe_gc_forced_at(shared, thread, "unlabelled")
}

/// [`maybe_gc_forced`] with the caller's identity, for the census.
pub(super) fn maybe_gc_forced_at(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &'static str,
) {
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
    cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
    update_root_snapshot(shared, thread);
    mtroots_set_gc_ctx(shared, thread, 3); // 3 = forced-alloc (maybe_gc_forced)
    mtroots_dump_initiator(shared, thread, 3);

    let alive_count = shared.threads.thread_registry.alive_count() as u32; // Widening: thread count to u32
    if alive_count <= 1 {
        let mut roots = collect_roots(shared, thread);
        // STW invariant: single-threaded fast path — see `maybe_gc`.
        // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
        weakref_null_referents_pre_gc(shared);
        // SAFETY: the STW invariant stated just above holds — every other
        // mutator is parked at a safepoint (or was forcibly stopped and
        // conservatively scanned), so this thread is the only mutator and
        // may assert exclusive heap access.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        let fin_roots = finalizable_roots(shared);
        let result = shared
            .mem
            .heap
            .collect_garbage_with_finalizers(&stw, &mut roots, &fin_roots, &shared.threads.monitors)
            .0;
        process_references_after_gc(shared, &result.pointer_map, &roots);
        update_all_roots(shared, thread, &result.pointer_map);
        crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
        // T19.3.G1 — count forced cycles (allocation-failure-driven) too.
        shared
            .mem
            .gc_cycle_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        note_gc_productivity(shared, before_live, before_promoted);
    } else {
        let mut counted_os_tids: Vec<u32> = Vec::new();
        let should_initiate_gc = {
            // xt-hardening (2026-07-03): see maybe_gc — atomic counted-set
            // snapshot for identity-based takeover excusal.
            counted_os_tids.clear();
            shared
                .mem
                .gc_barrier
                .request_stw_counted_with_live_blocked(thread.thread_id, || {
                    let (n, blocked, tids, blocked_tids) = shared
                        .threads
                        .thread_registry
                        .alive_count_blocked_and_os_tids();
                    counted_os_tids = tids;
                    (
                        u32::try_from(n).unwrap_or(u32::MAX),
                        u32::try_from(blocked).unwrap_or(u32::MAX),
                        blocked_tids,
                    )
                })
        };
        if should_initiate_gc {
            // BUG-03 — forcibly stop + conservatively scan in-JIT peers.
            let mut xt_roots: Vec<ObjectRef> = Vec::new();
            let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
            let mut roots = collect_roots(shared, thread);
            let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
            roots.extend(snapshot_roots);
            // INT-3 (G1) — everything a frozen peer can address must not
            // move; must follow collect_roots (which clears the pins).
            pin_frozen_peer_roots_for_g1(shared, &xt_roots, &taken);
            roots.extend(xt_roots); // BUG-03 cross-thread JIT conservative roots
                                    // STW invariant: `wait_for_all()` returned — every mutator
                                    // has parked at its safepoint poll (or, BUG-03, been forcibly
                                    // stopped in JIT and conservatively scanned).
                                    // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
            weakref_null_referents_pre_gc(shared);
            // SAFETY: the STW invariant stated just above holds — every other
            // mutator is parked at a safepoint (or was forcibly stopped and
            // conservatively scanned), so this thread is the only mutator and
            // may assert exclusive heap access.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
            let fin_roots = finalizable_roots(shared);
            let result = shared
                .mem
                .heap
                .collect_garbage_with_finalizers(
                    &stw,
                    &mut roots,
                    &fin_roots,
                    &shared.threads.monitors,
                )
                .0;
            process_references_after_gc(shared, &result.pointer_map, &roots);
            update_all_roots(shared, thread, &result.pointer_map);
            // Step 5 GAP D: remap the ec_watch corruption-watch table across this
            // multi-threaded forced collection too. The single-threaded GC paths
            // already do (mirrors the `update_all_roots` -> `ec_watch::remap`
            // pairing at maybe_gc:419 / maybe_gc_forced:636); this multi-threaded
            // initiator path was missing it, so a relocating G1 evacuation left
            // ec_watch holders stale and the watchpoint read moved-away memory.
            crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
            // xt-hardening (2026-07-03): clear regions + resume BEFORE
            // complete_gc (see maybe_gc's epilogue for the race rationale).
            shared.mem.heap.clear_jit_tlab_skip_regions(); // BUG-03
            crate::jit::xt_root_scan::resume(taken); // BUG-03 resume frozen peers
            shared.mem.gc_barrier.complete_gc(result.pointer_map);
            // T19.3.G1 — count forced cycles (multi-threaded initiator).
            shared
                .mem
                .gc_cycle_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            note_gc_productivity(shared, before_live, before_promoted);
        } else {
            safepoint_check(shared, thread);
        }
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
/// Forced GCs only happen on genuine allocation failure (young full *and*
/// promotion blocked), so this never fires during ordinary young-GC churn.
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
/// This is the safety net, not the fix. The `promoted=0`-forever condition that
/// exposed it was a real collector defect (selective promotion switched off by a
/// flag that had changed meaning — see `gc_quiescence::unrewritable_peer_state`)
/// and is fixed at its source. What this guarantees is that the next such defect
/// surfaces as slowness, which is diagnosable, rather than as a spurious OOM on
/// a half-empty heap, which is not.
pub(super) fn note_gc_productivity(shared: &SharedVm, before_live: usize, before_promoted: u64) {
    let cap = shared.mem.heap.heap_capacity();
    if cap == 0 {
        return;
    }
    // Free-list-aware live metric (see the capture site in `maybe_gc_forced`):
    // the non-moving sweep reclaims into the young free list without moving
    // the bump cursor, so `allocated_bytes` would read a fully-productive
    // sweep as "freed 0" and falsely latch the overhead limit.
    let after_live = shared.mem.heap.live_bytes_estimate();
    // A promotion-only cycle conserves live bytes but still did useful
    // allocation-enabling work (it drained young), so credit promoted bytes.
    // The 2%-of-capacity threshold below still catches the genuine
    // everything-survives-into-a-full-old-gen death spiral.
    let promoted = shared
        .mem
        .heap
        .bytes_promoted_total()
        .saturating_sub(before_promoted) as usize;
    let freed = before_live
        .saturating_sub(after_live)
        .saturating_add(promoted);
    // The free-space half (see the doc comment): the old generation must be
    // unable to absorb 2% of total capacity — the death spiral's own definition
    // of "wedged" — before a sliver-freeing cycle counts toward the streak.
    // Same 2%-of-`cap` yardstick as the freed-bytes test, so the two halves
    // cannot drift apart.
    let old_headroom = shared.mem.heap.old_gen_headroom();
    // Cast: numeric/representation conversion
    let old_gen_wedged = (old_headroom as u128) * 100 < (cap as u128) * 2;
    // unproductive: freed < 2% of capacity AND the old gen is wedged
    let freed_sliver = (freed as u128) * 100 < (cap as u128) * 2;
    let unproductive = freed_sliver && old_gen_wedged;
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
        0
    };
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_GC_OVERHEAD").is_some() {
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
        eprintln!(
            "[GC_OVERHEAD] before={before_live} after={after_live} promoted={promoted} \
             freed={freed} cap={cap} old_headroom={old_headroom} freed_sliver={freed_sliver} \
             old_gen_wedged={old_gen_wedged} unproductive={unproductive} streak={streak} \
             young_used={y_used} young_free_list={y_free} young_largest_free={y_largest} \
             young_cap={y_cap} sp_sweeps={sw} sp_selective={sel} sp_defrag={defrag} \
             sp_candidates={cand} sp_pinned={pin} sp_unaged={unaged} sp_evacuated={evac} \
             sp_old_full={ofull}"
        );
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
pub fn force_gc_from_native(shared: &SharedVm, thread: &mut JvmThread) {
    cratonvm_types::gc_entry_census::note_from_native();
    // Retire TLAB before GC
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
    cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
    update_root_snapshot(shared, thread);
    mtroots_set_gc_ctx(shared, thread, 1); // 1 = System.gc
    mtroots_dump_initiator(shared, thread, 1);

    // Snapshot finalizable object addresses so the GC can resurrect dead ones
    let fin_addrs: Vec<usize> = finalizable_roots(shared);

    let alive_count = shared.threads.thread_registry.alive_count() as u32; // Widening: thread count to u32
    if alive_count <= 1 {
        let mut roots = collect_roots(shared, thread);
        // STW invariant: single-threaded fast path — see `maybe_gc`.
        // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
        weakref_null_referents_pre_gc(shared);
        // SAFETY: the STW invariant stated just above holds — every other
        // mutator is parked at a safepoint (or was forcibly stopped and
        // conservatively scanned), so this thread is the only mutator and
        // may assert exclusive heap access.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        let (result, dead_finalizers) = shared.mem.heap.collect_garbage_with_finalizers(
            &stw,
            &mut roots,
            &fin_addrs,
            &shared.threads.monitors,
        );
        process_references_after_gc(shared, &result.pointer_map, &roots);
        update_all_roots(shared, thread, &result.pointer_map);
        crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
        // Enqueue dead finalizable objects (their new addresses) for finalization
        for new_addr in &dead_finalizers {
            shared.mem.finalizer_thread.enqueue(*new_addr);
        }
        // Once-only finalization: flag the processor entries for the objects
        // just enqueued. `process_references_after_gc` above already ran
        // `update_after_gc`, so entry referents hold post-GC addresses
        // matching `dead_finalizers`. Without this, the resurrected object
        // looks alive to every later cycle and the GC re-resurrects +
        // re-enqueues it (finalize() observed running 3× per object).
        if !dead_finalizers.is_empty() {
            shared
                .mem
                .ref_processor
                .lock()
                .mark_finalizer_enqueued(&dead_finalizers);
        }
    } else {
        let mut counted_os_tids: Vec<u32> = Vec::new();
        let should_initiate_gc = {
            // xt-hardening (2026-07-03): see maybe_gc — atomic counted-set
            // snapshot for identity-based takeover excusal.
            counted_os_tids.clear();
            shared
                .mem
                .gc_barrier
                .request_stw_counted_with_live_blocked(thread.thread_id, || {
                    let (n, blocked, tids, blocked_tids) = shared
                        .threads
                        .thread_registry
                        .alive_count_blocked_and_os_tids();
                    counted_os_tids = tids;
                    (
                        u32::try_from(n).unwrap_or(u32::MAX),
                        u32::try_from(blocked).unwrap_or(u32::MAX),
                        blocked_tids,
                    )
                })
        };
        if should_initiate_gc {
            // BUG-03 — forcibly stop + conservatively scan in-JIT peers.
            let mut xt_roots: Vec<ObjectRef> = Vec::new();
            let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
            let mut roots = collect_roots(shared, thread);
            let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
            roots.extend(snapshot_roots);
            // INT-3 (G1) — everything a frozen peer can address must not
            // move; must follow collect_roots (which clears the pins).
            pin_frozen_peer_roots_for_g1(shared, &xt_roots, &taken);
            roots.extend(xt_roots); // BUG-03 cross-thread JIT conservative roots
                                    // STW invariant: `wait_for_all()` returned — every mutator
                                    // has parked at its safepoint poll (or, BUG-03, been forcibly
                                    // stopped in JIT and conservatively scanned).
                                    // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
            weakref_null_referents_pre_gc(shared);
            // SAFETY: the STW invariant stated just above holds — every other
            // mutator is parked at a safepoint (or was forcibly stopped and
            // conservatively scanned), so this thread is the only mutator and
            // may assert exclusive heap access.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
            let (result, dead_finalizers) = shared.mem.heap.collect_garbage_with_finalizers(
                &stw,
                &mut roots,
                &fin_addrs,
                &shared.threads.monitors,
            );
            process_references_after_gc(shared, &result.pointer_map, &roots);
            update_all_roots(shared, thread, &result.pointer_map);
            // Step 5 GAP D: keep the ec_watch corruption-watch table consistent
            // across this multi-threaded finalizer collection (single-threaded
            // paths already remap it; this initiator path was missing the call).
            crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
            for new_addr in &dead_finalizers {
                shared.mem.finalizer_thread.enqueue(*new_addr);
            }
            // Once-only finalization — see the single-threaded arm above.
            if !dead_finalizers.is_empty() {
                shared
                    .mem
                    .ref_processor
                    .lock()
                    .mark_finalizer_enqueued(&dead_finalizers);
            }
            // xt-hardening (2026-07-03): clear regions + resume BEFORE
            // complete_gc (see maybe_gc's epilogue for the race rationale).
            shared.mem.heap.clear_jit_tlab_skip_regions(); // BUG-03
            crate::jit::xt_root_scan::resume(taken); // BUG-03 resume frozen peers
            shared.mem.gc_barrier.complete_gc(result.pointer_map);
        } else {
            safepoint_check(shared, thread);
        }
    }
    // INT-8: a forced GC advances the G1 concurrent-cycle machinery exactly
    // like an allocation-triggered young GC (`maybe_gc`'s epilogue calls
    // this at 819/903). Without it, a `System.gc()`-driven application —
    // whose forced young collections keep Eden below the allocation-GC
    // threshold — could NEVER start or complete a marking cycle: no cleanup
    // ever reclaimed dead Old regions and no remark-time reference
    // processing ever ran. HotSpot's default `System.gc()` under G1 is a
    // full collection that processes every generation's references; this
    // IHOP/completion check is the closest cycle-machinery equivalent.
    maybe_concurrent_gc(shared, thread);
    if shared.mem.heap.is_g1() {
        // An explicit System.gc() is a full-collection request, not merely an
        // Eden evacuation. Finish the G1 mark/remark/cleanup synchronously so
        // dead old regions, weak loaders, and their metadata are observable
        // before System.gc() returns.
        last_ditch_reclaim(shared, thread);
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
    for addr in addrs {
        // SAFETY: addr was produced by the cleaner thread's drain_actions and points at a valid object header within the heap arena.
        let cleanable = unsafe { ObjectRef::from_raw(addr as *mut u8) };
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
                // the `Cleanable` arm below.
                let _ = crate::vm::invoke_shared(
                    shared,
                    thread,
                    "jdk/internal/ref/Cleaner",
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

        let class_name = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string());
        if let Some(name) = class_name {
            // Errors are silently swallowed per the Cleaner contract
            // (JDK catches Throwable inside CleanerImpl.run()).
            let _ = crate::vm::invoke_shared(
                shared,
                thread,
                &name,
                "run",
                "()V",
                &[Value::Object(Some(action))],
            );
        }
    }
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

        let class_name = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string());

        if let Some(name) = class_name {
            // Invoke finalize()V — errors are silently swallowed per JLS §12.6
            let _ = crate::vm::invoke_shared(
                shared,
                thread,
                &name,
                "finalize",
                "()V",
                &[Value::Object(Some(obj_ref))],
            );
        }
    }
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
    let cid = shared.mem.heap.class_id_of(cleanable);
    cm.is_assignable_to_name(cid, "java/lang/ref/Reference")
        || cm.is_assignable_to_name(cid, "java/lang/ref/Cleaner$Cleanable")
}

/// Process weak/soft references after a GC cycle.
/// Calls the ReferenceProcessor, nulls referent fields of cleared references,
/// and relocates ref processor addresses using the pointer map.
pub(super) fn process_references_after_gc(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    cycle_roots: &[ObjectRef],
) {
    // HIB-CV-24 (Manifestation B): reconcile the defining-loader side-table with
    // this collection. A user `ClassLoader` the application no longer references
    // is now collectable (it is not GC-rooted under `CRATONVM_LOADER_UNLOAD`);
    // prune its stale side-table entry and remap survivors using the SAME
    // survivor predicate the reference processor uses below. Done BEFORE the
    // diagnostic `no_refproc` short-circuit so the side-table never holds a
    // dangling/stale ObjectRef after a collection, independent of that switch.
    {
        let is_marked = |addr: usize| -> bool {
            pointer_map.contains_key(&addr) || shared.mem.heap.is_addr_live(addr)
        };
        let dead_class_hints = cratonvm_native_builtins::classloader::gc_reconcile_defining_loaders(
            shared.vm_identity,
            &is_marked,
            pointer_map,
        );
        let unload = crate::memory::gc::unload_dead_class_metadata(shared, &dead_class_hints);
        if unload.classes_unloaded != 0 {
            tracing::debug!(
                loaders = unload.loaders_unloaded,
                classes = unload.classes_unloaded,
                jit_entries = unload.jit_entries_retired,
                "class-loader metadata unloaded"
            );
        }
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
        cratonvm_gc::external_roots::prune_external_roots(&is_live_for_prune);
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
    // ClassManager is rank L10 and the reference processor is L7, so resolve
    // the JDK field before acquiring the lower-ranked processor lock.
    let reference_next_slot = gc_reference_next_slot(shared);
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
    let is_marked = |addr: usize| -> bool {
        if weakref_clear_enabled() {
            shared
                .mem
                .heap
                .watched_pre_gc_addr_survived(addr, pointer_map)
        } else {
            pointer_map.contains_key(&addr) || shared.mem.heap.is_addr_live(addr)
        }
    };

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
    let is_stale_young = |addr: usize| -> bool {
        let young_stale = shared
            .mem
            .heap
            .pre_gc_addr_did_not_survive(addr, pointer_map);
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
    // Snapshotted rather than read through `ref_proc`: the loops below drain
    // the processor (`take_newly_cleared`, `remove_collected`), so a live
    // borrow of it here would not compile.
    let identity_stamps = ref_proc.identity_stamps_snapshot();
    // The referent side of the same question the identity stamps answer for
    // the Reference side. See the refusal at the restore write below.
    let referent_class_stamps = ref_proc.referent_class_stamps_snapshot();
    // Every address a survivor RELOCATED INTO this cycle. A pre-collection
    // address that appears here is not the address it used to be: the object
    // that lived there is gone and a slid survivor now owns the base.
    let relocation_targets: std::collections::HashSet<usize> =
        pointer_map.values().copied().collect();
    let identity_matches = |pre_gc_addr: usize, obj: ObjectRef| -> bool {
        match identity_stamps.get(&pre_gc_addr) {
            Some(&stamp) if stamp != 0 => {
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
        let relocated = pointer_map.get(&ref_addr).copied().unwrap_or(ref_addr);
        if is_stale_young(relocated) {
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
        if !identity_matches(ref_addr, obj_ref) {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP reidentified CLEARED ref @0x{actual_addr:x} (identity stamp mismatch)"
                );
            }
            continue;
        }
        shared.mem.heap.set_field(obj_ref, 0, Value::Object(None));
    }

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
        let ref_reloc = pointer_map.get(ref_addr).copied().unwrap_or(*ref_addr);
        let q_reloc = pointer_map.get(queue_addr).copied().unwrap_or(*queue_addr);
        if is_stale_young(ref_reloc) || is_stale_young(q_reloc) {
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
        if !identity_matches(*ref_addr, ref_obj) {
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
        // Mark as enqueued — sentinel Int(1) distinguishes from "never had queue"
        shared.mem.heap.set_field(ref_obj, 1, Value::Int(1)); // REF_FIELD_QUEUE = enqueued sentinel
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
        shared.mem.finalizer_thread.enqueue(actual);
    }

    // Relocate any cleaner actions DEFERRED from earlier GC cycles (queued but
    // not yet run because a JIT borrow was live — see `run_cleaner_actions`).
    // Their cleanable objects may have been evacuated by this collection, so
    // remap their raw addresses before any later drain dereferences them.
    shared.mem.cleaner_thread.update_after_gc(pointer_map);
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
        shared.mem.finalizer_thread.enqueue(obj_addr);
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
        for (ref_obj_old, referent_old) in active {
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
                None => continue,
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
                None => continue,
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
                continue;
            }
            // This pass writes an OBJECT into slot 0, not a null, so a
            // shape-clean re-issue here installs an unrelated reference in a
            // live object's first field -- the `java.lang.String` receiver
            // shape the H2 `TestMultiThread` MVStore-writer report opens with.
            if !identity_matches(ref_obj_old, ro) {
                if straystack_enabled() {
                    eprintln!(
                        "[refproc] SKIP reidentified weak/phantom RESTORE ref @0x{ref_obj_new:x} (identity stamp mismatch)"
                    );
                }
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
            let screen_on = !cratonvm_types::flags().gc.no_referent_identity_screen;
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
                continue;
            }
            if let Some(&want_class) = referent_class_stamps
                .get(&ref_obj_old)
                .filter(|_| screen_on)
            {
                let have_class = shared.mem.heap.class_id_of(rt).as_u32();
                if want_class != 0 && have_class != want_class {
                    note_referent_restore_refused();
                    if straystack_enabled() || dbg_weakref() {
                        eprintln!(
                            "[refproc] REFUSE weak/phantom RESTORE ref @0x{ref_obj_new:x}: \
                             referent @0x{referent_new:x} is class {have_class}, recorded \
                             {want_class} (total refused={})",
                            referent_restore_refusals()
                        );
                    }
                    continue;
                }
            }
            // Slot 0 = REF_FIELD_REFERENT. `set_field` fires the write barrier,
            // so a young referent restored into a promoted (old-gen) Reference
            // re-marks the old→young card.
            shared.mem.heap.set_field(ro, 0, Value::Object(Some(rt)));
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
                None if shared.mem.heap.is_addr_live(addr) => addr,
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

    // Relocate all addresses in the ref processor to match the new heap layout
    ref_proc.update_after_gc(pointer_map);
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
pub fn referent_restore_refusals() -> u64 {
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

    // TLAB fast path: try thread-local bump allocation (no lock)
    let obj = if total_size <= cratonvm_gc::tlab::tlab_max_alloc() {
        if let Some(ptr) = tlab_alloc_object(thread, shared, class_id, num_fields, total_size) {
            ptr
        } else {
            // TLAB miss: fall through to shared heap
            alloc_object_shared(shared, thread, class_id, num_fields)?
        }
    } else {
        // Large object: skip TLAB, allocate directly from shared heap
        alloc_object_shared(shared, thread, class_id, num_fields)?
    };

    // If the class overrides finalize(), register the object with the
    // reference processor so GC can enqueue it for finalization (JLS §12.6).
    let has_fin = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .map_or(false, |c| c.has_finalizer);
    if has_fin {
        shared.register_finalizable(obj.as_ptr() as usize); // Cast: GC object pointer to address
    }

    // Write the JVM default (JVMS §2.3 / §4.12.5) into EVERY instance field --
    // reference fields included. Zero-initialized memory does not read back as
    // any of those defaults: after `Value::Object` gained its `NonNull` niche
    // the all-zero 16-byte slot decodes as `Int(0)`, so `null` has to be
    // written just as `Int(0)`/`Long(0)`/`Float(0.0)`/`Double(0.0)` do.
    // (The function keeps its historical name; see its doc comment.)
    init_primitive_fields(shared, obj, class_id);

    Ok(obj)
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
pub fn init_primitive_fields(shared: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    let cm = shared.classes.class_manager.read();
    let store = &cm.class_store;
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

/// TLAB fast path for object allocation. Returns None on TLAB miss.
///
/// T19.3.G1 (GC allocation-storm): the refill size consulted here is
/// adaptive — after the first refill on a given thread the tracker
/// inside the thread's [`cratonvm_gc::Tlab`] recommends the next size
/// based on fill time and alloc count, so a thread that just burned
/// through 64 KB in under a millisecond gets a 128 KB chunk next
/// time and so on up to the documented cap. Each refill bumps
/// `shared.mem.tlab_refill_count` so operators can spot-check the
/// refill rate against the hit-rate target.
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
        thread,
        shared,
        class_id,
        num_fields,
        body_size,
        gc_flags,
        total_size,
        false,
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
    use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
    let length_u32 = u32::try_from(length).ok()?;
    let total_size = HEADER_SIZE.checked_add(length)?;
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
    use std::sync::atomic::Ordering;
    shared.mem.tlab_hit_count.fetch_add(1, Ordering::Relaxed);
    shared
        .mem
        .bytes_allocated_total
        .fetch_add(total_size as u64, Ordering::Relaxed);
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
        thread,
        shared,
        class_id,
        num_fields,
        body_size,
        gc_flags,
        total_size,
        true,
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
/// Wedge-breaker state — module-scope so the gate-pass reset in
/// `tlab_alloc_object_inner` shares the counter with the breaker.
static TLAB_GATE_CONSECUTIVE_FAILS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static TLAB_LAST_BREAK_ALLOC_TOTAL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Slow-path entries since the last refill-time `needs_gc()` fire (the
/// crumb-treadmill fix in `tlab_alloc_object_inner`). Entry-counted, not
/// byte-counted: the degraded modes this guards (per-object allocation,
/// crumb-sized mini-TLABs) enter the slow path orders of magnitude more
/// often than healthy TLAB flow, so the counter accelerates exactly when
/// the wedge deepens, and a bytes-based stamp would freeze (the per-object
/// path doesn't bump `bytes_allocated_total`).
static TLAB_SLOWPATH_ENTRIES_SINCE_GC: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Bytes handed out by SUCCESSFUL TLAB refills since the last refill-time
/// `needs_gc()` fire — the second re-arm metric, for the HEALTHY path.
///
/// `gen-gc-minor-pause-20260902` swept `CRATONVM_GC_YOUNG_TRIGGER_PERCENT`
/// over 50/75/90 and got identical collection counts at every setting, with
/// `young_bytes_before` equal to the from-space CAPACITY on every cycle: the
/// collections were driven by allocation failure, never by the trigger. The
/// entry counter above is why. It was sized for the degraded modes it guards
/// (a per-object slow path enters tens of thousands of times per second), but
/// a healthy JIT workload refills a 256 KiB–1 MiB TLAB per slow-path entry
/// and exhausts a 256 MiB semi-space in a few hundred entries — never the
/// 65,536 the gate demanded. So on exactly the workloads that allocate the
/// most, the trigger was consulted zero times per cycle, the from-space ran
/// to capacity, and the pause-goal feedback that moves the threshold
/// (`adapt_young_trigger_to_pause`) moved a number nothing read.
///
/// Two metrics, OR-ed: the entry count still fires in the crumb wedge (where a
/// bytes stamp freezes, see above), and the bytes count fires on healthy TLAB
/// flow after every [`NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES`] of refills.
/// `needs_gc` carries its own anti-livelock floor, so consulting it more often
/// cannot storm a young gen whose live set sits above the threshold.
static TLAB_REFILL_BYTES_SINCE_GC: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Refilled bytes between two consults of the young trigger on the healthy
/// path: 4 MiB, i.e. every 4–16 full-size TLABs. Small against any semi-space
/// the trigger is worth having on, large enough that a mini-TLAB storm still
/// consults `needs_gc` (one `Mutex` acquisition) a few hundred times per
/// gigabyte rather than per refill.
const NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES: u64 = 4 * 1024 * 1024;

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

pub(super) fn tlab_refill_wedge_break(thread: &mut JvmThread, shared: &SharedVm) -> bool {
    use std::sync::atomic::Ordering;
    /// Consecutive gate failures before a forced collection. At the
    /// observed wedge rate this is a few milliseconds of per-object
    /// slow-path work — long enough that transient pressure never trips
    /// it, short enough that a real wedge is broken almost immediately.
    const WEDGE_BREAK_THRESHOLD: u64 = 16_384;
    /// Allocation progress required before a second forced collection.
    const WEDGE_REARM_BYTES: u64 = 64 * 1024 * 1024;

    let fails = TLAB_GATE_CONSECUTIVE_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
    if fails < WEDGE_BREAK_THRESHOLD {
        return false;
    }
    let alloc_total = shared.mem.bytes_allocated_total.load(Ordering::Relaxed);
    cratonvm_types::gc_entry_census::note_alloc_total(alloc_total);
    let last = TLAB_LAST_BREAK_ALLOC_TOTAL.load(Ordering::Relaxed);
    if last != 0 && alloc_total.saturating_sub(last) < WEDGE_REARM_BYTES {
        return false;
    }
    if TLAB_LAST_BREAK_ALLOC_TOTAL
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
    TLAB_GATE_CONSECUTIVE_FAILS.store(0, Ordering::Relaxed);
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

/// `(compact, legacy, bytes_saved)` for TLAB object allocations.
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
    *G.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_COMPACT_TLAB_SITES") {
            Ok(v) => v.trim().parse::<u32>().unwrap_or(u32::MAX),
            Err(_) => u32::MAX,
        }
    })
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

/// One line per distinct class that gets the compact shape —
/// `CRATONVM_DBG_COMPACT_TLAB=1`.
fn note_compact_tlab_class(class_id: ClassId, num_fields: usize, site: u32) {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_COMPACT_TLAB").is_none() {
        return;
    }
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

#[inline]
pub(crate) fn plan_tlab_object_shape_at(
    class_id: ClassId,
    num_fields: usize,
    site: u32,
) -> (usize, u32, u8) {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
    use std::sync::atomic::Ordering;
    let legacy_total = HEADER_SIZE + num_fields * SLOT_SIZE;
    if compact_tlab_alloc_enabled() && (compact_tlab_site_mask() & site) != 0 {
        if let Some(body) = cratonvm_types::compact_tlab_body_size(class_id.as_u32(), num_fields) {
            if let (Some(total), Ok(body_u32)) =
                (HEADER_SIZE.checked_add(body), u32::try_from(body))
            {
                TLAB_COMPACT_OBJECTS.fetch_add(1, Ordering::Relaxed);
                note_compact_tlab_class(class_id, num_fields, site);
                TLAB_COMPACT_BYTES_SAVED
                    .fetch_add(legacy_total.saturating_sub(total) as u64, Ordering::Relaxed);
                return (total, body_u32, cratonvm_types::GC_FLAG_COMPACT);
            }
        }
    }
    TLAB_LEGACY_OBJECTS.fetch_add(1, Ordering::Relaxed);
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
    let body = total_size.saturating_sub(HEADER_SIZE);
    match u32::try_from(body) {
        Ok(body) => (body, cratonvm_types::GC_FLAG_COMPACT),
        Err(_) => (0, 0),
    }
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
    unsafe fn init_header(self, ptr: *mut u8, class_id: ClassId, hash: i32) {
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
    // LEGACY layout, on every backend, deliberately -- see
    // `init_object_header`. A 2026-09-02 attempt to give ZGC's TLAB objects
    // the compact shape its own `alloc_object` uses (so a TLAB object and a
    // heap-allocated one of the same class would agree) MISCOMPILED
    // `probes/FjpProbe.java`: wrong per-task sums, no collection involved.
    // The interpreter fast path has never consulted the layout registry, and
    // the tree has compiled and cached field access against that fact for
    // long enough that changing it here is not a local decision. If the two
    // shapes are ever unified it has to be done at every allocation site at
    // once, with that probe in the gate.
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
        let hash = shared.mem.heap.next_identity_hash();
        // SAFETY: `alloc_initialized` reserved `total_size` (>= HEADER_SIZE)
        // bytes at `ptr`, 8-byte aligned and privately owned until commit.
        unsafe { shape.init_header(ptr, class_id, hash) };
        // ZGC registers every TLAB object the moment its header is complete
        // (`VmHeap::note_tlab_object`); a no-op on the linear-sweep backends.
        shared.mem.heap.note_tlab_object(ptr, total_size);
    }) {
        shared.mem.tlab_hit_count.fetch_add(1, Ordering::Relaxed);
        // Truncation-checked: usize → u64 widening is loss-free on 64-bit
        // platforms; on 32-bit the upper bound (usize::MAX ≈ 4 GiB) still
        // fits in u64 so `as u64` is exact.
        shared
            .mem
            .bytes_allocated_total
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            .fetch_add(total_size as u64, Ordering::Relaxed);
        // SAFETY: ptr was produced by TLAB allocation and points at a valid object header within the heap arena.
        return Some(unsafe { ObjectRef::from_raw(ptr) });
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
    // healthy TLAB flow — see `TLAB_REFILL_BYTES_SINCE_GC`. A refill-bytes
    // stamp is OR-ed in so the trigger is consulted every few full-size
    // TLABs; the entry count keeps the crumb wedge covered.
    if refill_needs_young_room && tlab_gc_trigger_enabled() {
        use std::sync::atomic::Ordering;
        const NEEDSGC_MIN_ENTRIES_BETWEEN_FIRES: u64 = 65_536;
        let entries = TLAB_SLOWPATH_ENTRIES_SINCE_GC.fetch_add(1, Ordering::Relaxed) + 1;
        let refilled = TLAB_REFILL_BYTES_SINCE_GC.load(Ordering::Relaxed);
        if (entries >= NEEDSGC_MIN_ENTRIES_BETWEEN_FIRES
            || refilled >= NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES)
            && shared.mem.heap.needs_gc_for_jit_allocation()
        {
            TLAB_SLOWPATH_ENTRIES_SINCE_GC.store(0, Ordering::Relaxed);
            TLAB_REFILL_BYTES_SINCE_GC.store(0, Ordering::Relaxed);
            thread.tlab.retire();
            maybe_gc_forced_at(shared, thread, "tlab-alloc-shaped");
        } else if refilled >= NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES {
            // Consulted and declined: re-arm the bytes stamp so the next
            // consult is another 4 MiB away rather than on every refill.
            TLAB_REFILL_BYTES_SINCE_GC.store(0, Ordering::Relaxed);
        }
    }

    // Slow path: the current TLAB is exhausted. Ask the adaptive sizer
    // for the next refill size, request it from the shared arena, and
    // install a fresh TLAB. The sizer looks at the just-retired TLAB's
    // fill-time and alloc-count stats to grow/shrink/keep the request.
    let requested = if thread.tlab.is_empty() {
        // First allocation on this thread — no history yet. Use the
        // "start big" baseline so a static-init burst doesn't refill
        // three times before the sizer gets a chance to weigh in.
        cratonvm_gc::tlab::initial_refill_size()
    } else {
        // Subsequent refill — consult the thread-local pressure
        // tracker attached to the just-retired TLAB.
        let n = thread.tlab.next_refill_size();
        // Never fall below the documented floor even if the tracker
        // somehow returns zero (pathological input).
        n.max(cratonvm_gc::tlab::min_tlab_size())
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
    if refill_needs_young_room
        && !shared.mem.heap.young_bump_headroom(requested)
        && !shared.mem.heap.young_has_free_block(free_block_floor)
    {
        dbg_refill_fail_state(shared, requested);
        // Sustained gate failure = the wedge: per-object allocations keep
        // succeeding so nothing else will ever trigger the collection that
        // coalesces the free list. Force one (rate-limited) and re-probe.
        if !tlab_gc_trigger_enabled()
            || !tlab_refill_wedge_break(thread, shared)
            || (!shared.mem.heap.young_bump_headroom(requested)
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
    thread.tlab.retire();

    let mut refill = shared.mem.heap.refill_tlab(requested);
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
            refill = shared.mem.heap.refill_tlab(requested);
            cratonvm_types::gc_entry_census::note_refill_retry(refill.is_some());
        }
    } else {
        TLAB_GATE_CONSECUTIVE_FAILS.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some((buf, size)) = refill {
        shared.mem.tlab_refill_count.fetch_add(1, Ordering::Relaxed);
        // Widening: usize -> u64 (value preserved). The healthy-path re-arm
        // metric for the refill-time young trigger above.
        TLAB_REFILL_BYTES_SINCE_GC.fetch_add(size as u64, Ordering::Relaxed);
        // Read the outgoing TLAB's running per-thread allocation total before
        // the struct is replaced — `Tlab::new` starts a fresh one at zero, and
        // `getThreadAllocatedBytes` must not go backwards at a refill.
        let carried = thread.tlab.thread_allocated_bytes();
        // SAFETY: buf and size were just returned by the arena allocator and the memory is zeroed.
        thread.tlab = unsafe { cratonvm_gc::Tlab::new(buf, size) };
        thread.tlab.adopt_allocation_total(carried);
        // Start the new refill-window timer so `next_refill_size`
        // measures this TLAB's lifetime from the moment we installed it.
        thread.tlab.begin_refill(size);
        if let Some(ptr) = thread.tlab.alloc_initialized(total_size, 8, |ptr| {
            // H1: see fast-path comment above.
            let hash = shared.mem.heap.next_identity_hash();
            // SAFETY: same contract as the fast path — a freshly reserved,
            // 8-byte-aligned, privately-owned `total_size` region.
            unsafe { shape.init_header(ptr, class_id, hash) };
            shared.mem.heap.note_tlab_object(ptr, total_size);
        }) {
                shared.mem.tlab_hit_count.fetch_add(1, Ordering::Relaxed);
            shared
                .mem
                .bytes_allocated_total
                // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
                .fetch_add(total_size as u64, Ordering::Relaxed);
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
                "{} num_fields={num_fields} registered_layout_fields={registered:?}                  compact_layout_was_available={matches}",
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
/// H1: `identity_hash_code` is now eagerly assigned at allocation time
/// (caller passes `shared.mem.heap.next_identity_hash()`). The previous
/// behavior of storing 0 and "lazily" filling on first `hashCode()` call
/// was not actually wired up anywhere — every fresh TLAB-allocated
/// `new Object()` (cid=0, fields=0) produced an all-zero first 16 bytes
/// of header that the stale-pointer detector in `execute_invoke`
/// mis-flagged as stale memory, causing CGLIB's HashMap operations to
/// emit spurious "Stale pointer detected" warnings on every legitimate
/// `Object` key. The non-TLAB allocators in `gc::heap`/`gc::gen_heap`/
/// `gc::g1` have always assigned a fresh hash here; this brings the
/// fast path into agreement with them.
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
        // The COMPACT shape mirrors its packed body size here, exactly as
        // `gen_heap::alloc_object` and the JIT's inline `new` both do; the
        // legacy shape writes 0. See `plan_tlab_object_shape`.
        body_size,
        // A class-file field table is u16-sized, so this is unreachable for a
        // verified Java class. Keep the allocation path panic-free if a corrupt
        // synthetic caller nevertheless violates that invariant.
        u32::try_from(num_fields).unwrap_or(u32::MAX),
    );
    // SAFETY: ptr points to freshly allocated, properly aligned memory for an ObjectHeader.
    unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
    if gc_flags != 0 {
        // SAFETY: the header was just written at `ptr`, so this reads a live,
        // fully initialised `ObjectHeader`; `add_gc_flags` takes `&self` and
        // drives the atomic mark word.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        header.add_gc_flags(gc_flags);
    }
    // Every header this function writes is LEGACY — `array_length = 0`, no
    // `GC_FLAG_COMPACT` — regardless of whether the class has a registered
    // compact layout, because this path never consults `plan_object_alloc`.
    // That is a deliberate property of the fast path and not a defect on its
    // own; what WAS a defect is that nothing reported it. See
    // `note_tlab_legacy_object`.
    if cratonvm_types::flags().gc.dbg_compact_legacy {
        note_tlab_legacy_object(class_id, num_fields);
    }
    // A2 breadcrumb (CRATONVM_DBG_A2): the interpreter TLAB fast path bypasses
    // gen_heap, so record the legacy-layout object header it writes here.
    cratonvm_gc::a2dbg::record(
        ptr as usize,
        class_id.as_u32(),
        ObjectKind::Object as u8,
        ArrayElementType::Reference as u8,
        0,
        u32::try_from(num_fields).unwrap_or(u32::MAX),
        cratonvm_gc::heap::HEADER_SIZE + num_fields * cratonvm_gc::heap::SLOT_SIZE,
    );
}


/// Shared-heap allocation path (with lock). Used for TLAB misses and large objects.
pub(crate) fn alloc_object_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
    let total_size = HEADER_SIZE + num_fields.saturating_mul(SLOT_SIZE);
    if let Some(obj) = shared.mem.heap.try_alloc_object(class_id, num_fields) {
        // T19.3.G1 — slow-path bytes count toward the allocation rate
        // just like TLAB-served bytes, so `--verbose:gc` reflects true
        // throughput even for large objects that skipped the TLAB.
        // Widening: usize → u64 is loss-free on all supported targets.
        shared
            .mem
            .bytes_allocated_total
            .fetch_add(total_size as u64, std::sync::atomic::Ordering::Relaxed);
        // Same bytes, per thread — the counter behind
        // `com.sun.management.ThreadMXBean.getThreadAllocatedBytes`. It cannot
        // come from the TLAB cursor here, because this object never touched it.
        thread.tlab.note_external_allocation(total_size);
        return Ok(obj);
    }
    // Retire TLAB before GC — its memory is in the arena that will be collected
    thread.tlab.retire();
    maybe_gc_forced_at(shared, thread, "alloc-object-shared");
    // GC-overhead limit: if repeated forced GCs have freed almost nothing, the
    // heap is full of live objects — declare OOM now rather than retrying into a
    // death-spiral (a sliver freed each cycle would otherwise let allocation
    // limp on, GC-thrashing). The catch site / drain surfaces the singleton.
    if gc_overhead_limit_exceeded(shared) {
        maybe_dump_heap_on_oom(shared, thread);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError {
                message: format!("Java heap space (alloc_object with {} fields)", num_fields),
            },
        )));
    }
    // SB-LOADER-ZIPCONTENT (2026-08-04): the post-GC retries use the
    // old-gen-spilling `try_alloc_object_full`, for the same reason
    // `gc_alloc_array` gives on the array side — once a non-moving JIT-safe
    // young sweep has fragmented the young free list, a young-only retry
    // reports OOM while the old generation still holds most of the heap. (The
    // first attempt above stays young-only: it is the fast path, and spilling
    // before a GC has even been attempted would promote ordinary short-lived
    // objects straight into old gen.) `gc_alloc_array`'s doc comment already
    // claimed this path behaved that way; it did not.
    if let Some(obj) = shared.mem.heap.try_alloc_object_full(class_id, num_fields) {
        shared
            .mem
            .bytes_allocated_total
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            .fetch_add(total_size as u64, std::sync::atomic::Ordering::Relaxed);
        thread.tlab.note_external_allocation(total_size);
        return Ok(obj);
    }
    // G1 last-ditch: see `gc_alloc_array` — dead Old/humongous spans need a
    // completed mark cycle's cleanup; run one synchronously and retry once.
    last_ditch_reclaim(shared, thread);
    let obj = shared
        .mem
        .heap
        .try_alloc_object_full(class_id, num_fields)
        .map(|obj| {
            shared
                .mem
                .bytes_allocated_total
                .fetch_add(total_size as u64, std::sync::atomic::Ordering::Relaxed);
            obj
        })
        .ok_or_else(|| {
            // T1.7.7 — write an HPROF dump on OOM if `-XX:+HeapDumpOnOutOfMemoryError`.
            maybe_dump_heap_on_oom(shared, thread);
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                message: format!("Java heap space (alloc_object with {} fields)", num_fields),
            }))
        })?;
    thread.tlab.note_external_allocation(total_size);
    Ok(obj)
}

/// T1.7.7 — write an HPROF heap dump when allocation fails and the
/// `heap_dump_on_oom` flag is set. Best-effort: any I/O error is logged
/// but does not propagate, because we are *already* about to throw OOM
/// and replacing that with an I/O error would be worse for the user.
///
/// The dump runs at most once per VM lifetime (gated by an atomic
/// flag on the shared VM) so a tight allocation loop doesn't write
/// thousands of dumps.
pub(super) fn maybe_dump_heap_on_oom(shared: &SharedVm, thread: &JvmThread) {
    use std::sync::atomic::Ordering;
    if !shared.config.heap_dump_on_oom {
        return;
    }
    if shared
        .debug
        .oom_dump_written
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return; // already written
    }
    let path = shared
        .config
        .heap_dump_path
        .clone()
        .unwrap_or_else(|| format!("./java_pid{}.hprof", std::process::id()));
    let arc = shared.get_arc();
    // obsaudit D11: pass this thread's id so dump_heap can request a real
    // stop-the-world pause for the walk instead of racing live mutators.
    match crate::runtime::hprof::dump_heap(&arc, &path, thread.thread_id) {
        Ok(bytes) => tracing::error!(
            "wrote {} byte HPROF heap dump to {} on OutOfMemoryError",
            bytes,
            path
        ),
        Err(e) => tracing::error!("failed to write HPROF heap dump on OutOfMemoryError: {}", e),
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
        thread.tlab.note_external_allocation(external_bytes);
        return Ok(arr);
    }
    // Retire TLAB before GC
    thread.tlab.retire();
    maybe_gc_forced_at(shared, thread, "gc-alloc-array");
    // GC-overhead limit (see alloc_object_shared): bail to OOM if the heap is
    // GC-thrashing rather than spinning on slivers.
    if gc_overhead_limit_exceeded(shared) {
        maybe_dump_heap_on_oom(shared, thread);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError {
                message: format!("Java heap space (alloc_array length {})", length),
            },
        )));
    }
    if let Some(arr) = shared
        .mem
        .heap
        .try_alloc_array_full(class_id, element_type, length)
    {
        thread.tlab.note_external_allocation(external_bytes);
        return Ok(arr);
    }
    // G1 last-ditch: the young pause above cannot reclaim dead Old/humongous
    // spans — only a completed mark cycle's cleanup can. Run one
    // synchronously and retry once before surfacing OOM.
    last_ditch_reclaim(shared, thread);
    let arr = shared
        .mem
        .heap
        .try_alloc_array_full(class_id, element_type, length)
        .ok_or_else(|| {
            maybe_dump_heap_on_oom(shared, thread);
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                message: format!("Java heap space (alloc_array length {})", length),
            }))
        })?;
    thread.tlab.note_external_allocation(external_bytes);
    Ok(arr)
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
) {
    frame.scan_local_objects(out, heap);
    let before = out.len();
    frame.stack.scan_object_refs(out, heap);
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
    {
        let pending = !thread.gc_block_state.fixup.lock().is_empty()
            || thread
                .gc_block_state
                .slot_origins
                .lock()
                .iter()
                .any(|so| so.cur != so.orig);
        if pending {
            let n = crate::vm::vm_exec::apply_pending_blocked_fixups(shared, thread);
            if n > 0 && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some() {
                eprintln!(
                    "[blockgc] SAFEPOINT-HEAL tid={} applied {} pending fixups (leaked blocked-region exit upstream)",
                    thread.thread_id.0, n,
                );
            }
        }
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
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some() {
                eprintln!(
                    "[blockgc] SAFEPOINT-FLAG-CLEAR tid={} - in_blocked_region was raised on a running thread",
                    thread.thread_id.0,
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
    {
        thread_local! {
            static AUDIT_CC: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
        }
        let cc = shared.mem.heap.collection_count();
        let prev = AUDIT_CC.with(|c| c.replace(cc));
        if cc != prev && prev != u64::MAX {
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
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some() {
        thread_local! {
            static LAST_CC: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
        }
        let cc = shared.mem.heap.collection_count();
        let prev = LAST_CC.with(|c| c.replace(cc));
        if cc != prev && prev != u64::MAX {
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

    let _rs_t0 = if rootsnap_dbg_enabled() {
        Some((std::time::Instant::now(), thread.frames.len()))
    } else {
        None
    };
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
            scan_frame_roots(&thread.frames[i], &mut snapshot, &shared.mem.heap);
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
                scan_frame_roots(frame, &mut fresh, &shared.mem.heap);
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
                frame.scan_local_objects(&mut snapshot, &shared.mem.heap);
            }
            if conservative_locals {
                frame.scan_locals_conservative(&mut snapshot, &shared.mem.heap);
            }
            let before = snapshot.len();
            frame
                .stack
                .scan_object_refs(&mut snapshot, &shared.mem.heap);
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
        }
    }
    snapshot.extend(thread.native_pin_roots.iter().copied());
    // Native handle scopes are outside interpreter frames just like the pin
    // stack. A peer-initiated collection sees this parked thread only through
    // `root_snapshot`, so publish every live handle slot here as well.
    snapshot.extend(thread.handle_slots.iter().flatten().copied());
    snapshot.extend(thread.native_alloc_pool.iter().copied());
    if let Some(r) = thread.native_pending_return {
        snapshot.push(r);
    }
    crate::memory::roots::push_off_frame_thread_roots(thread, &mut snapshot);
    // Direct JIT HashMap node cache: unlike the ordinary current-thread root
    // scan, a cross-thread collector can see this parked thread only through
    // `root_snapshot`.  Keep both cache handles in that snapshot so a
    // collection initiated by another worker cannot reclaim or relocate a
    // cached node behind the JIT fast path.  The three pointer-map consumers
    // (`gc.rs`, `apply_pointer_map_to_thread`, and blocked wake-up) each
    // forward these entries before the owner can read the cache again.
    for entry in &thread.jit_hashmap_string_node_cache {
        snapshot.push(entry.map);
        snapshot.push(entry.node);
        if let Some(key_object) = entry.key_object {
            snapshot.push(key_object);
        }
    }
    // TOMCAT-JNDIREALM-JIT.3 (2026-07-26) — the ASCII case-conversion cache,
    // exactly the same contract as the HashMap node cache above. It was wired
    // into `roots::collect_roots` + `gc.rs` (the INITIATOR's own scan and
    // remap) but into neither published snapshot, so a collection initiated by
    // ANOTHER thread never saw it: the non-moving young sweep reclaimed the
    // cached `first`/`second` Strings, and the owner's next
    // `get_ascii_case_string_cached` handed the freed address straight back to
    // bytecode as an all-zero-header `java/lang/String` receiver at
    // `String.equals`/`String.indexOf`. Real repro: Tomcat's
    // `TestJNDIRealmIntegration` 76-case matrix with `com/unboundid/`
    // JIT-eligible, whose `StaticUtils.toLowerCase` is the hot cache consumer.
    for entry in &thread.string_case_cache {
        snapshot.push(entry.source);
        snapshot.push(entry.first);
        snapshot.push(entry.second);
        if let Some(locale) = entry.locale {
            snapshot.push(locale);
        }
    }
    // JNI local references (INT-5, safepoint half): a JNI native that
    // obtained local refs and re-entered Java parks HERE — and a
    // cross-thread collector marks this thread only from this snapshot, so
    // an object reachable solely through this thread's `JNI_LOCAL_FRAMES`
    // was reclaimed. Thread-local storage; this deposit always runs on the
    // owning thread. The resume-side remap is `update_local_refs_after_gc`
    // in `apply_pointer_map_to_thread`.
    crate::native::jni::collect_local_ref_roots(&mut snapshot);

    // This thread's own `java.lang.Thread` mirror (and any pending async
    // exception). These live in `JvmThread` fields, not on any frame, so the
    // frame scan above never captures them — yet `Thread.currentThread()`
    // hands the mirror straight back to bytecode. When ANOTHER thread initiates
    // a collection while this one is parked, the cross-thread collector marks
    // and remaps only from this deposited snapshot (it never runs
    // `collect_roots` for a non-current thread). Without depositing the mirror
    // here it is (a) not marked — a non-moving sweep reclaims it — and (b) not
    // seeded into the blocked-thread fixup chain, so a moving collection
    // relocates it and `check_post_block_gc` leaves `self.thread.java_thread_obj`
    // dangling. Either way the next `currentThread()` returns an all-zero-header
    // object and real-JDK `Thread.getThreadGroup()` NPEs on a null `holder`
    // (Tomcat TestDigestAuthenticator: a worker-thread GC orphaned the parked
    // JUnit main thread's mirror). Depositing it closes both holes; the wake
    // remap is `check_post_block_gc`, the safepoint-resume remap is
    // `apply_pointer_map_to_thread` (both already forward `java_thread_obj`).
    if let Some(obj) = thread.java_thread_obj {
        snapshot.push(obj);
    }
    if let Some(exc) = thread.pending_async_exception {
        snapshot.push(exc);
    }

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
        crate::jit::conservative_roots::scan_active_jit_frames(&shared.mem.heap, &mut snapshot);
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
        if shared.mem.heap.is_g1() {
            let addrs: Vec<usize> = snapshot[jit_scan_start..]
                .iter()
                .map(|r| r.as_ptr() as usize)
                .collect();
            cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&addrs);
        }
    } else if shared.mem.heap.is_g1() {
        // Precise-relocation mode covers every JIT oop with rewritable
        // shadow-stack slots — no conservative pins needed; drop stale ones.
        cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&[]);
    }

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

/// Check if a stop-the-world pause is requested and participate if so.
///
/// Called at safepoints: allocation sites and backward branches (loop iterations).
/// If STW is active, this thread deposits its roots and waits for GC to complete,
/// then applies the pointer map to update its own frame references.
pub(crate) fn safepoint_check(shared: &SharedVm, thread: &mut JvmThread) {
    use std::sync::atomic::Ordering;
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
                            *q.add(4),
                            *q.add(5),
                            // Cast: reinterpret pointer/address to typed pointer
                            (q.add(12) as *const u32).read_unaligned(),
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
            let trace = crate::runtime::stackwalker::capture_frames_no_lines(&thread.frames);
            *thread.frame_trace.lock() = trace;
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
        crate::jit::conservative_roots::publish_peer_jit_coverage_for_stw();
        // Arrive at barrier and wait for GC to complete. Census-aware (auto):
        // a genuine safepoint arrival is normally counted, but if this pause's
        // census excluded us as blocked (a finding-1(a) window), participating
        // would fill a counted mutator's quota slot.
        let pointer_map = shared.mem.gc_barrier.arrive_and_wait_auto(thread.thread_id);

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
        // Apply pointer map to this thread's frames
        if !pointer_map.is_empty() {
            apply_pointer_map_to_thread(thread, &pointer_map, &shared.mem.heap);
        }
        mtroots_selfcheck(thread, &shared.mem.heap, "safepoint-resume");
    }
    // T1.5.1 — pick up any async exception posted by another thread
    // (e.g. `Thread.stop0`). The cross-thread poster writes into the
    // registry's slot; we consume it here and move it into the
    // per-thread `pending_async_exception` field so the next
    // exception-raising point observes it. The *actual* raise
    // happens at the next opcode boundary in `execute_instruction`
    // via `check_pending_async_exception`, which returns the stored
    // throwable as a `MethodCallFailed`.
    if thread.pending_async_exception.is_none() {
        if let Some(throwable) = shared
            .threads
            .thread_registry
            .take_async_exception(thread.thread_id)
        {
            thread.pending_async_exception = Some(throwable);
        }
    }
}

/// T1.5.1 — check the per-thread async-exception slot and, if set,
/// clear it and return a `MethodCallFailed::ExceptionThrown` carrying
/// the Throwable.
///
/// Callers that observe `Some` should immediately propagate the
/// failure through the interpreter's normal exception-table walk so
/// the Throwable lands in the first enclosing `catch` block (or
/// unwinds the method entirely if none applies).
pub fn check_pending_async_exception(
    thread: &mut JvmThread,
) -> Option<crate::error::MethodCallFailed> {
    let throwable = thread.pending_async_exception.take()?;
    Some(crate::error::MethodCallFailed::ExceptionThrown(throwable))
}

/// Apply a GC pointer map to a thread's frame locals and operand stacks.
pub(crate) fn apply_pointer_map_to_thread(
    thread: &mut JvmThread,
    pointer_map: &cratonvm_types::PointerMap,
    heap: &crate::memory::VmHeap,
) {
    // JNI local references (INT-2, safepoint-resume half): rewrite THIS
    // thread's `JNI_LOCAL_FRAMES` handles through the pointer map — a JNI
    // native that re-entered Java and parked at the safepoint poll must not
    // resume with dangling local jobjects after a moving collection. The
    // storage is thread-local and this function always runs on the resuming
    // thread, so this is the only place that can reach these handles.
    crate::native::jni::update_local_refs_after_gc(pointer_map);
    // BUG-03 trace (gated): record that the safepoint-peer remap ran for main.
    if thread.thread_id.0 == 0
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BUG03").is_some()
    {
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
        frame.update_local_refs(pointer_map, heap);
        frame.stack.update_object_refs(pointer_map, heap);
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
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some() {
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
    crate::jit::conservative_roots::remap_active_jit_frames(pointer_map);
    crate::jit::conservative_roots::remap_register_image_words(pointer_map, None);
    crate::jit::conservative_roots::report_stale_after_remap(pointer_map, None);
    // §4 (multi-thread shadow scan, remap half). Remap THIS thread's shadow-stack
    // precise roots in place, so a worker resuming from the STW barrier sees the
    // relocated addresses in the JIT registers/slots it reloads from its shadow
    // stack. (The GC initiator's own shadow stack is remapped by `update_all_roots`
    // in `gc.rs`; a non-initiator reaches here instead.) Every shadow slot is a
    // known oop, so the rewrite is unconditionally safe. No-op when the gate is
    // off or the shadow stack is empty.
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.remap(pointer_map);
    }
    // Also update printed values and java_thread_obj
    for val in &mut thread.printed {
        update_value_ref(val, pointer_map);
    }
    if let Some(ref mut obj_ref) = thread.java_thread_obj {
        let old_addr = obj_ref.as_ptr() as usize; // Cast: GC object pointer to address
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: new_addr was produced by pointer_map and points at a valid object header within the heap arena.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
    // Native-held per-thread roots. A thread running native code that pinned
    // ObjectRefs across allocations (`pin_native_root`, e.g. the synthetic
    // HttpServer dispatcher holding the handler + exchange across exchange-build
    // allocations) can be parked at THIS safepoint barrier when another thread's
    // moving GC relocates those objects. The GC-initiator path
    // (`update_all_roots`) and the native-blocked wake path
    // (`check_post_block_gc`) already forward these; this non-initiator
    // safepoint-resume path must too, or `read_native_pin` hands back a stale
    // address (surfaced as the `java/lang/Object.handle` NoSuchMethodError
    // storm). Forward the same set those two siblings do.
    for obj_ref in &mut thread.native_pin_roots {
        // Cast: object/code pointer to integer address
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
    // Cross-thread safepoint-resume counterpart of the handle-slot snapshot
    // above. The collector remaps the snapshot copy, not this owning table.
    crate::memory::gc::remap_handle_slots(&mut thread.handle_slots, pointer_map);
    for obj_ref in &mut thread.native_alloc_pool {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: the pointer map contains only relocated live objects.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
    if let Some(ref mut obj_ref) = thread.native_pending_return {
        // Cast: object/code pointer to integer address
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
    // The JIT HashMap fast path owns raw map/node ObjectRefs outside frames.
    // A thread parked at this safepoint can miss a moving collection initiated
    // by a peer, so mirror the initiator and blocked-wake remaps before JIT
    // code resumes and probes the cache.
    for entry in &mut thread.jit_hashmap_string_node_cache {
        for obj_ref in [&mut entry.map, &mut entry.node] {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        if let Some(key_object) = entry.key_object.as_mut() {
            let old_addr = key_object.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                *key_object = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
    // TOMCAT-JNDIREALM-JIT.3 — remap companion to the publish added above.
    // Same reasoning as the HashMap node cache: the entries are raw
    // `ObjectRef`s outside any frame, so a peer-initiated moving collection
    // would strand them.
    for entry in &mut thread.string_case_cache {
        for obj_ref in [&mut entry.source, &mut entry.first, &mut entry.second] {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        if let Some(locale) = entry.locale.as_mut() {
            let old_addr = locale.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                *locale = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
    for (_key_id, key_ref, val) in &mut thread.scoped_values {
        if let Some(obj_ref) = key_ref {
            // Cast: object/code pointer to integer address
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        update_value_ref(val, pointer_map);
    }
    if let Some(ref mut obj_ref) = thread.pending_async_exception {
        // Cast: object/code pointer to integer address
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
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
pub(super) fn maybe_concurrent_gc(shared: &SharedVm, thread: &mut JvmThread) {
    // G1 backend: trigger concurrent marking when IHOP threshold crossed
    if shared.mem.heap.is_g1() {
        // Finish first, start second: if an active cycle's background marker
        // has drained to a fixed point, run the STW final remark + cleanup
        // NOW, on this thread (it has the STW-barrier context). The remark
        // is NOT optional — the SATB log and a fresh root scan must reach
        // the bitmap before cleanup acts on it (see
        // `g1_final_remark_and_cleanup`); the old completion path (a watcher
        // thread calling straight into cleanup) discarded both.
        if shared.mem.heap.g1_is_marking_active() && shared.mem.heap.g1_concurrent_mark_finished() {
            g1_final_remark_cleanup(shared, thread);
            return;
        }
        if shared.mem.heap.g1_should_start_marking() && !shared.mem.heap.g1_is_marking_active() {
            g1_concurrent_mark_cycle(shared, thread);
        }
        return;
    }

    // Only proceed if old gen needs collection and we have a concurrent marker
    if !shared.mem.heap.old_gen_needs_gc() {
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

    // Phase 1: Initial Mark — brief STW pause.
    //
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) instead of `brief_stw_counted_with_live_blocked`, whose
    // internal plain `wait_for_all()` stalls forever on a peer spinning in
    // compiled code — and whose root set covered such a peer only by its
    // STALE deposit snapshot. Mark-only pause: the frozen peers' fresh
    // conservative roots are extra MARK roots; nothing moves, so no
    // pin/pointer-map concerns.
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let initial_mark_done =
        shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(thread.thread_id, || {
                let (n, blocked, tids, blocked_tids) = shared
                    .threads
                    .thread_registry
                    .alive_count_blocked_and_os_tids();
                counted_os_tids = tids;
                (
                    u32::try_from(n).unwrap_or(u32::MAX),
                    u32::try_from(blocked).unwrap_or(u32::MAX),
                    blocked_tids,
                )
            });
    if !initial_mark_done {
        return; // Another STW was in progress
    }
    {
        // Forcibly stop + conservatively scan in-JIT peers, then wait for
        // the cooperative mutators (byte-identical to wait_for_all() when
        // no thread is in JIT).
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
        // Collect root pointers for old-gen marking
        let roots = collect_roots(shared, thread);
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        let mut root_ptrs: Vec<*mut u8> = roots
            .iter()
            .chain(snapshot_roots.iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.iter())
            .map(|r| r.as_ptr())
            .collect();
        // fork6 GC_STRESS fix — young→old references are mandatory
        // old-marking roots. `initial_mark` filters this list with
        // `old_gen.contains`, so an old object whose only path from a
        // root goes THROUGH a young object (root → young holder → old
        // target) was invisible and the sweep freed it live. Selective
        // promotion mass-produces exactly that shape (it tenures a
        // pinned young holder's children), which is why the Fork6Hard
        // GC_STRESS lane corrupted even single-threaded during clinit.
        // Safe here: brief STW, mutators quiesced, TLABs retired.
        root_ptrs.extend(
            shared
                .mem
                .heap
                .collect_young_to_old_roots()
                .into_iter()
                .map(|a| a as *mut u8),
        );
        if let Some(guard) = shared.mem.heap.old_gen_lock() {
            marker.initial_mark(&root_ptrs, &*guard);
        }
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        shared.mem.heap.clear_jit_tlab_skip_regions();
        crate::jit::xt_root_scan::resume(taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
    }

    // Phase 2: Concurrent Mark — runs while app threads continue
    if let Some(guard) = shared.mem.heap.old_gen_lock() {
        marker.concurrent_mark(&*guard);
    }

    // Phase 3: Remark — brief STW pause.
    // INT-3 residual fix: open-coded for the same takeover-wait reason as
    // Phase 1 above (a never-polling in-JIT peer must not stall the remark
    // nor be covered only by its stale deposit snapshot).
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let remark_done =
        shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(thread.thread_id, || {
                // Finding 1(a): remark pauses use the identity census too, so blocked
                // threads are excluded BY IDENTITY and their wake-time arrivals cannot
                // satisfy this pause's quota (`arrive_and_wait_auto`). The anonymous
                // `threads_blocked` subtraction this replaces excluded the same
                // population without recording who it excluded.
                let (n, blocked, tids, blocked_tids) = shared
                    .threads
                    .thread_registry
                    .alive_count_blocked_and_os_tids();
                counted_os_tids = tids;
                (
                    u32::try_from(n).unwrap_or(u32::MAX),
                    u32::try_from(blocked).unwrap_or(u32::MAX),
                    blocked_tids,
                )
            });
    if remark_done {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
        // Round-5 fix (CRIT — UAF): drain the initiator's per-thread
        // SATB buffer before remark drains the global queue. Other
        // mutators flushed when they arrived at the STW barrier;
        // the initiator must drain its own.
        shared.mem.heap.flush_thread_satb();
        let roots = collect_roots(shared, thread);
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
        root_ptrs.extend(
            shared
                .mem
                .heap
                .collect_young_to_old_roots()
                .into_iter()
                .map(|a| a as *mut u8),
        );
        if let Some(guard) = shared.mem.heap.old_gen_lock() {
            marker.remark(&root_ptrs, &*guard);
        }
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        shared.mem.heap.clear_jit_tlab_skip_regions();
        crate::jit::xt_root_scan::resume(taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
    }

    // fork6 GC_STRESS fix — the remark STW is NOT optional. If another
    // thread's STW won the race (`brief_stw_counted` returned false — a
    // near-certainty under allocation storms, where a young-GC request is
    // always pending), the closure never ran: the SATB queue is undrained
    // and the roots were never rescanned, so the mark bitmap is NOT final.
    // The old code fell through to the sweep anyway and freed live objects.
    // Abort the cycle instead (deactivate the barrier, discard the bitmap);
    // the next `old_gen_needs_gc` trigger starts over.
    if !remark_done {
        marker.abort_cycle();
        return;
    }

    // Phase 4: Concurrent Sweep
    if let Some(mut guard) = shared.mem.heap.old_gen_lock() {
        let swept = marker.concurrent_sweep(&mut *guard);
        if swept > 0 {
            tracing::debug!("Concurrent GC: swept {} old-gen objects", swept,);
        }
    }
    // Cycle complete — phase back to Idle (the write barrier's
    // `is_marking_active()` gate is already false after remark, but leaving
    // the shared state at `ConcurrentSweep` would misreport the VM as
    // mid-cycle to any observer).
    marker.finish_cycle();
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
/// The open-coded `request → takeover-wait → work → complete` (rather than
/// `brief_stw_counted_with_live_blocked`) is INT-3's residual fix, copied
/// deliberately: that helper's internal plain `wait_for_all()` stalls forever
/// on a peer spinning in compiled code, and its root set covers such a peer
/// only through a STALE deposit snapshot. A missed root here is an object the
/// concurrent phase never traces.
///
/// Mark-only pause: nothing moves, so there is no pointer map, no pin set and
/// no root rewrite — the frozen peers' conservative roots are simply extra
/// mark roots.
pub(super) fn zgc_concurrent_mark_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let stw_taken =
        shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(thread.thread_id, || {
                let (n, blocked, tids, blocked_tids) = shared
                    .threads
                    .thread_registry
                    .alive_count_blocked_and_os_tids();
                counted_os_tids = tids;
                (
                    u32::try_from(n).unwrap_or(u32::MAX),
                    u32::try_from(blocked).unwrap_or(u32::MAX),
                    blocked_tids,
                )
            });
    if !stw_taken {
        // Another STW is in progress. Nothing has been done, so there is
        // nothing to unwind: the next allocation re-tests the threshold and
        // re-opens the cycle. If that other STW is a collection, the threshold
        // will have dropped and the cycle correctly does not open.
        return;
    }
    {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);

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
        shared.mem.heap.clear_jit_tlab_skip_regions();
        crate::jit::xt_root_scan::resume(taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());

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
    // complete) instead of `brief_stw_counted_with_live_blocked`, whose
    // internal plain `wait_for_all()` stalls forever on a peer spinning in
    // compiled code — and whose root set covered such a peer only by its
    // STALE deposit snapshot (a missed mark root here = cleanup frees a
    // live object). Mark-only pause: the frozen peers' fresh conservative
    // roots are extra MARK roots; nothing moves, so no pin/pointer-map
    // concerns.
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let initial_mark_done =
        shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(thread.thread_id, || {
                let (n, blocked, tids, blocked_tids) = shared
                    .threads
                    .thread_registry
                    .alive_count_blocked_and_os_tids();
                counted_os_tids = tids;
                (
                    u32::try_from(n).unwrap_or(u32::MAX),
                    u32::try_from(blocked).unwrap_or(u32::MAX),
                    blocked_tids,
                )
            });
    if !initial_mark_done {
        return; // Another STW in progress
    }
    {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
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
        shared.mem.heap.clear_jit_tlab_skip_regions();
        crate::jit::xt_root_scan::resume(taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
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
/// (`brief_stw_counted` returns false), nothing ran — the cycle simply
/// stays open (SATB active, worklist quiescent) and the next
/// `maybe_concurrent_gc` retries. Unlike the generational remark there is
/// nothing to abort: no sweep decision has been made yet.
pub(super) fn g1_final_remark_cleanup(shared: &SharedVm, thread: &mut JvmThread) {
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) so a never-polling in-JIT peer neither stalls the pause
    // forever nor is covered only by its stale deposit snapshot (a missed
    // mark root here = cleanup frees a live object). Unlike the young/mixed
    // pauses nothing moves, so no pins — but `cleanup` DOES linearly walk
    // every non-Free region computing live bytes, so the takeover's
    // frozen-TLAB-tail publication (consumed by the region walkers' skip
    // checks) is load-bearing here too.
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let done =
        shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(thread.thread_id, || {
                // Finding 1(a): remark pauses use the identity census too, so blocked
                // threads are excluded BY IDENTITY and their wake-time arrivals cannot
                // satisfy this pause's quota (`arrive_and_wait_auto`). The anonymous
                // `threads_blocked` subtraction this replaces excluded the same
                // population without recording who it excluded.
                let (n, blocked, tids, blocked_tids) = shared
                    .threads
                    .thread_registry
                    .alive_count_blocked_and_os_tids();
                counted_os_tids = tids;
                (
                    u32::try_from(n).unwrap_or(u32::MAX),
                    u32::try_from(blocked).unwrap_or(u32::MAX),
                    blocked_tids,
                )
            });
    if done {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
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
            g1_remark_process_references(shared, is_live)
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
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        shared.mem.heap.clear_jit_tlab_skip_regions();
        crate::jit::xt_root_scan::resume(taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
    } else {
        tracing::debug!("[G1] Final remark lost the STW race — retrying at next GC");
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
    );
    let unloaded = crate::memory::gc::unload_dead_class_metadata(shared, &dead_class_hints);
    if unloaded.classes_unloaded != 0 {
        tracing::debug!(
            loaders = unloaded.loaders_unloaded,
            classes = unloaded.classes_unloaded,
            jit_entries = unloaded.jit_entries_retired,
            "G1 class-loader metadata unloaded at final remark"
        );
    }

    // Same subsystem-level exclusion switch as the post-GC path.
    if no_refproc() {
        return Vec::new();
    }
    // Same pressure input as the post-GC path — see the long comment there for
    // why this is allocatable headroom rather than the former hardcoded `64`,
    // and why the `0` clock argument is correct rather than a second hardcode.
    let free_mb = shared.mem.heap.soft_ref_policy_free_mb();
    // See `process_references_after_gc`: ClassManager must be consulted
    // before taking the lower-ranked reference-processor lock.
    let reference_next_slot = gc_reference_next_slot(shared);
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
    let mut ref_proc = shared.mem.ref_processor.lock();
    let result = ref_proc.process_references(is_marked, free_mb, 0);

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
        let size = match shared.mem.heap.get_field(q_obj, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        shared.mem.heap.set_field(q_obj, 1, Value::Int(size + 1));
        shared.mem.heap.set_field(ref_obj, 1, Value::Int(1)); // enqueued sentinel
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
    for obj_addr in &result.to_finalize {
        shared.mem.finalizer_thread.enqueue(*obj_addr);
        resurrect.push(*obj_addr);
    }
    while let Some(obj_addr) = ref_proc.dequeue_for_finalization() {
        shared.mem.finalizer_thread.enqueue(obj_addr);
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

    resurrect
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
/// last-ditch full GC on allocation failure.
///
/// No-op on non-G1 backends. Bounded: gives up after ~2s if the background
/// marker never quiesces or the STW races never resolve — the caller then
/// proceeds to OOM; this can delay an inevitable OOM slightly but never
/// hangs the allocation path.
/// The whole final rung of the allocation-failure ladder: everything the VM
/// can still do to satisfy an allocation that has already failed once, before
/// the caller is entitled to throw `OutOfMemoryError`.
///
/// Two steps, in order, because they reclaim disjoint things:
///
/// 1. [`g1_force_full_cycle`] — dead Old and humongous regions, which only a
///    completed mark cycle's cleanup reclaims. G1 only.
/// 2. [`last_ditch_clear_soft_refs`] — every softly-reachable object, on every
///    collector. `java.lang.ref`'s guarantee is unconditional: all soft
///    references to softly-reachable objects are cleared before the VM throws
///    `OutOfMemoryError`. CratonVM honoured only the LRU half of the soft-ref
///    policy, which by construction never fires for the reference the failing
///    program is itself reading in a loop.
pub(crate) fn last_ditch_reclaim(shared: &SharedVm, thread: &mut JvmThread) {
    g1_force_full_cycle(shared, thread);
    last_ditch_clear_soft_refs(shared, thread);
}

/// Collect once with every SoftReference condemned — see
/// `ReferenceProcessor::condemn_all_soft_refs` for the rule and
/// `with_last_ditch_soft_clear` for why the arming is thread-local.
///
/// Skipped outright when the application holds no live soft references, which
/// is the common case; a heap that is genuinely full then pays no extra GC on
/// its way to `OutOfMemoryError`.
fn last_ditch_clear_soft_refs(shared: &SharedVm, thread: &mut JvmThread) {
    if !weakref_clear_enabled() {
        // The opt-out restores the legacy never-clearing behaviour wholesale;
        // that has to include this rule, or `CRATONVM_WEAKREF_CLEAR=0` would
        // no longer be the byte-identical safety net it is documented to be.
        return;
    }
    if !shared.mem.ref_processor.lock().has_active_soft_refs() {
        return;
    }
    thread.tlab.retire();
    crate::runtime::interpreter::with_last_ditch_soft_clear(|| {
        maybe_gc_forced_at(shared, thread, "last-ditch-soft-refs");
    });
}

pub(crate) fn g1_force_full_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    // If a cycle is already mid-flight we simply help finish it — its
    // cleanup reclaims the same dead spans a fresh cycle would.
    let mut saw_active = shared.mem.heap.g1_is_marking_active();
    loop {
        if shared.mem.heap.g1_is_marking_active() {
            saw_active = true;
            if shared.mem.heap.g1_concurrent_mark_finished() {
                // Runs remark+cleanup under a brief STW; on a lost STW race
                // the cycle stays open and the loop retries.
                g1_final_remark_cleanup(shared, thread);
            } else {
                std::thread::yield_now();
            }
        } else if saw_active {
            return; // cycle completed — cleanup has run
        } else {
            // Not started yet (or the initial-mark STW lost its race —
            // g1_concurrent_mark_cycle returns without activating in that
            // case). Start/retry it.
            g1_concurrent_mark_cycle(shared, thread);
        }
        if std::time::Instant::now() >= deadline {
            tracing::debug!("[G1] last-ditch full cycle timed out — proceeding to OOM");
            return;
        }
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
        scan_frame_roots(&frame, &mut roots, heap);
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
        for b in [b'I', b'B', b'C', b'S', b'Z'] {
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
            for desc in [b'L', b'['] {
                assert_eq!(
                    heap.get_field_as(obj, slot, desc),
                    Value::Object(None),
                    "{what} at a '{}' slot must read as null either way",
                    desc as char,
                );
            }
        }
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
