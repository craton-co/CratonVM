#[allow(unused_imports)]
use super::*;

/// [`park_for_debugger`] for a thread stopped entering or leaving the native
/// method `native_top` (`(class id, JDWP method id)`) its top interpreter
/// frame's invoke at `pc` calls (interpreter round i1 wave 26, lane L1): the
/// snapshot this park republishes after a debugger task lists that native
/// first, as the one published before the park did
/// ([`publish_debugger_frames_under`]).
#[cfg(feature = "experimental-debug")]
#[cold]
pub(in crate::runtime::interpreter) fn park_for_debugger_under(
    shared: &SharedVm,
    thread: &mut JvmThread,
    tid: u64,
    pc: usize,
    native_top: Option<NativeHead>,
) {
    let stw_flag = shared.mem.gc_barrier.stw_requested.flag();
    // Wave 44 (lane L1): a `PopFrames` task may pop the parked frame and
    // frames below it; the listing is then republished at the new top
    // frame's invoke, with no native method above it.
    let parked_depth = thread.frames.len();
    let gates = &shared.debug.debugger_gates;
    // Interpreter round i2 wave 63 (lane L6,
    // `CRATONVM_DEBUG_THREAD_SUSPEND_WORD`): with the switch, the park waits
    // on the thread's own suspend word between its locked rounds
    // (`debug::SuspendWordWait`); without, `word` is inactive and the loop
    // below is the lock loop it was.
    let mut word = {
        let mut ds = shared.debug.debug_state.lock();
        ds.note_parked(tid, true);
        crate::debug::SuspendWordWait::enroll(gates, &mut ds, tid, &thread.gc_block_state)
    };
    let word_mode = word.active();
    // Interpreter round i2 wave 65 (lane L6, `CRATONVM_DEBUG_PARK_GC_BLOCKED`):
    // a word-parked thread waits inside a GC-blocked region
    // (`debug::ParkGcRegion`), so a pause neither wakes it nor waits for it.
    // Not for a thread already GC-blocked (never expected at a suspend
    // point): it answers no pause either way.
    let gc_blocked = word_mode
        && gates.park_gc_blocked()
        && !thread
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire);
    // Interpreter round i2 wave 66 (lane L6,
    // `CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT`): a re-entry after heap work
    // or a relisting may reuse the deposit the last leave refreshed
    // (audited under `CRATONVM_DBG_BLOCKGC`).
    let reuse_deposit = gc_blocked && gates.park_region_reuses_deposit();
    // Interpreter round i2 wave 68 (lane L6,
    // `CRATONVM_DEBUG_PARK_LEAVE_KEEPS_UNFOLDED_SNAPSHOT`): a leave for heap
    // work keeps the entry's snapshot when nothing folded into the region.
    let mut region = crate::debug::ParkGcRegion::closed()
        .with_deposit_reuse(
            reuse_deposit,
            reuse_deposit && false,
        )
        .with_unfolded_keep(reuse_deposit && gates.park_leave_keeps_unfolded_snapshot());
    // Interpreter round i2 wave 68 (lane L6,
    // `CRATONVM_DEBUG_PARK_REGION_AT_FIRST_PAUSE`): the region opens only
    // when the hold's first pause wakes the park (`debug::ParkRegionOpening`);
    // until then the park waits outside it, enrolled with the barrier below.
    let mut opening =
        crate::debug::ParkRegionOpening::new(gc_blocked && gates.park_region_at_first_pause());
    // Wave 66 (lane L6, `CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION`): a park
    // whose word moved since its last locked round (a task queued, a
    // resume) takes that round before it opens its region again, so a burst
    // of heap work handed to it outside its region pays no region at all.
    let enter_after_quiet_word = gc_blocked && gates.heap_work_outside_region();
    // Interpreter round i2 wave 64 (lane L6,
    // `CRATONVM_DEBUG_STW_UNPARKS_PARKED`): a word-parked thread enrolls
    // with the GC barrier, whose stop-the-world request unparks it, so its
    // wait on the word need not end every 1 ms to look at `stw_requested`.
    // Enrolled before the loop's first flag load (see `enroll_stw_waker`);
    // withdrawn while a task runs Java on this thread. Wave 65: not needed
    // by a park that waits GC-blocked. Wave 68: needed by one that waits
    // for the first pause to open its region (`opening.lazy()`), whatever
    // `CRATONVM_DEBUG_STW_UNPARKS_PARKED` says -- its wait is bounded by the
    // word's backstop -- and dropped when that pause opens the region.
    let stw_unparks = word_mode && (opening.lazy() || (!gc_blocked && gates.stw_unparks_parked()));
    let mut stw_waker = stw_unparks.then(|| shared.mem.gc_barrier.enroll_stw_waker());
    let word_bound = crate::debug::DebuggerGates::parked_word_wait_bound(stw_unparks || gc_blocked);
    // Wave 65: the top frame the listing is republished at, as the relisting
    // and a task's end below choose it.
    fn listing_top(
        thread: &JvmThread,
        pc: usize,
        parked_depth: usize,
        native_top: Option<NativeHead>,
    ) -> (usize, Option<NativeHead>) {
        if thread.frames.len() < parked_depth {
            (thread.frames.last().map_or(pc, |f| f.last_instr_pc), None)
        } else {
            (pc, native_top)
        }
    }
    // Wave 65: a relisting the locked round found due while the region was
    // open, made once the region has closed.
    let mut relist_outside = false;
    // Interpreter round i2 wave 54 (lane L6): the JDWP server's settle and
    // frame waits see the arrival now (`DebuggerGates::park_condvar`).
    gates.notify_state_changed();
    // The positive control of the condvar: how many of the park's waits a
    // notification ended (`CRATONVM_FRAME_TRACE=1`, `[DEBUG_PARK]` below).
    let (mut rounds, mut notified) = (0u64, 0u64);
    loop {
        // Wave 65: inside the region every pause excludes this thread, which
        // must not arrive at one.
        if !region.is_open() && stw_flag.load(std::sync::atomic::Ordering::Acquire) {
            // Wave 68: a lazy park opens its region at the pause instead of
            // arriving at it; the entry arrives for the pause once. The
            // barrier's waker goes with it: inside, no pause needs it awake
            // (kept should the entry not open, never expected here).
            let mut opened = false;
            if opening.opens_at_pause(word.has_seen(), enter_after_quiet_word && word.moved()) {
                let waker = stw_waker.take();
                opened = region.enter_at_pause(shared, thread);
                if opened {
                    drop(waker);
                    opening.note_opened_at_pause();
                } else {
                    stw_waker = waker;
                }
            }
            if !opened {
                safepoint_check(shared, thread);
            }
        }
        // Wave 65: once a locked round has seen the word, wait GC-blocked.
        // Wave 66: not while the word says there is news for the locked
        // round (`enter_after_quiet_word`). Wave 68: a lazy park only once a
        // pause has opened its region (`opening.enters_eagerly()`).
        if gc_blocked
            && opening.enters_eagerly()
            && !region.is_open()
            && word.has_seen()
            && !(enter_after_quiet_word && word.moved())
        {
            region.enter(shared, thread);
        }
        // Wave 63: nothing the locked round reads moved -- no lock. Still
        // bounded by the safepoint poll above (wave 64: or woken by the
        // request itself, `word_bound`).
        if word.wait_unchanged(word_bound) {
            rounds += 1;
            continue;
        }
        let task = {
            let mut ds = shared.debug.debug_state.lock();
            word.note_locked_round();
            match ds.take_parked_task(tid) {
                Some(task) => Some(task),
                None if !ds.is_thread_suspended(tid) => {
                    // Under the same lock the server queues work under, so no
                    // task can arrive for a thread that has left.
                    ds.note_parked(tid, false);
                    word.leave(&mut ds, tid);
                    break;
                }
                None => {
                    // Interpreter round i2 wave 58, lane L6: resumed and
                    // suspended again while it stayed parked (it did not run
                    // between the two -- an overloaded host), the thread's
                    // listing carries the generation the resume retired, so
                    // the new suspension's settle wait took it for a thread
                    // still on its way and waited its whole bound
                    // (`L6W56JdiSuspendUnderCpuHogs`, ten 10 s waits a run).
                    // It is stopped here: list it again, at the new
                    // generation, as a task's end does. Wave 65: inside the
                    // region the frames hold pre-collection addresses, so
                    // the relisting waits for the region to close, below.
                    if !ds.has_current_frames(tid) && region.is_open() {
                        relist_outside = true;
                    } else if !ds.has_current_frames(tid) {
                        let (top_pc, top) = if thread.frames.len() < parked_depth {
                            let top = thread.frames.last().map_or(pc, |f| f.last_instr_pc);
                            (top, None)
                        } else {
                            (pc, native_top)
                        };
                        publish_debugger_frames_under(shared, thread, &mut ds, tid, top_pc, top);
                        gates.notify_state_changed();
                        if crate::runtime::env_cache::frame_trace() {
                            eprintln!("[DEBUG_PARK_RELIST] tid={tid} rounds={rounds}");
                        }
                    }
                    // Wave 54 (lane L6): woken by a resume's publication or a
                    // queued task with `CRATONVM_DEBUG_PARK_CONDVAR`, the
                    // 1 ms poll without. Bounded by the poll either way: a
                    // stop-the-world request notifies nothing, and a parked
                    // thread must answer it (`safepoint_check` above).
                    rounds += 1;
                    // Wave 63: with the word, the wait is the lock-free one
                    // at the loop's top.
                    if !word_mode
                        && gates.await_state_change(&mut ds, crate::debug::DEBUG_STATE_POLL)
                    {
                        notified += 1;
                    }
                    None
                }
            }
        };
        // Wave 65: the relisting found due inside the region, with the
        // frames current again; re-checked under the lock it publishes
        // under (a resume or a task may have come meanwhile).
        if relist_outside {
            relist_outside = false;
            region.leave(shared, thread);
            let mut ds = shared.debug.debug_state.lock();
            if ds.is_thread_suspended(tid) && !ds.has_current_frames(tid) {
                let (top_pc, top) = listing_top(thread, pc, parked_depth, native_top);
                publish_debugger_frames_under(shared, thread, &mut ds, tid, top_pc, top);
                gates.notify_state_changed();
                if crate::runtime::env_cache::frame_trace() {
                    eprintln!("[DEBUG_PARK_RELIST] tid={tid} rounds={rounds} gc_blocked=true");
                }
            }
        }
        match task {
            Some(task) => {
                // Wave 65: a task runs Java or touches the heap and the
                // frames: never inside the region.
                // Wave 66: only heap work (queued for any thread) leaves the
                // frames and this thread's standing deposit as they were.
                // Wave 68: and so its leave may keep the entry's snapshot
                // (`CRATONVM_DEBUG_PARK_LEAVE_KEEPS_UNFOLDED_SNAPSHOT`).
                if task.runs_on_any_thread() {
                    region.leave_for_heap_work(shared, thread);
                } else {
                    region.leave(shared, thread);
                    region.forget_standing();
                }
                // Wave 64: no pause's unpark while the task runs Java here;
                // enrolled again before the loop's next flag load.
                let enrolled = stw_waker.take().is_some();
                let reply = crate::debug::run_parked_task(shared, thread, task);
                if enrolled {
                    stw_waker = Some(shared.mem.gc_barrier.enroll_stw_waker());
                }
                // No safepoint since the task returned: the frames read here
                // are where the collector left them. The reply goes out once
                // this level is idle and its snapshot stands, so a debugger
                // acting on the reply finds both.
                {
                    let mut ds = shared.debug.debug_state.lock();
                    ds.note_task_done(tid);
                    // Wave 28: also when the listing outlived the task's
                    // resume (an invocation), so its ids take the new
                    // frame generation.
                    if ds.is_thread_suspended(tid) && !ds.has_current_frames(tid) {
                        let (top_pc, native_top) = if thread.frames.len() < parked_depth {
                            let top = thread.frames.last().map_or(pc, |f| f.last_instr_pc);
                            (top, None)
                        } else {
                            (pc, native_top)
                        };
                        publish_debugger_frames_under(
                            shared, thread, &mut ds, tid, top_pc, native_top,
                        );
                    }
                }
                reply.send();
            }
            // Waited above, under the lock it re-checks.
            None => {}
        }
    }
    // Wave 65: resumed -- a running mutator again, its frames current,
    // before anything below or the caller touches them.
    region.leave(shared, thread);
    drop(stw_waker);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[DEBUG_PARK] tid={tid} rounds={rounds} notified={notified} condvar={}",
            gates.park_condvar()
        );
        if word_mode {
            eprintln!(
                "[DEBUG_PARK_WORD] tid={tid} rounds={rounds} locked={} word_wakes={} stw_unparks={stw_unparks} gc_blocked={gc_blocked} regions={} reuses={} region_enter_us={} region_leave_us={} lazy={} pause_entries={} keeps={}",
                word.locked_rounds,
                word.word_wakes,
                region.entries,
                region.reuses,
                region.enter_ns / 1_000,
                region.leave_ns / 1_000,
                opening.lazy(),
                region.pause_entries,
                region.keeps
            );
        }
    }
    let mut ds = shared.debug.debug_state.lock();
    // Withdraw the snapshot's pins on its object ids (wave 7): an id no
    // command handed out goes, and its handle is freed.
    for id in withdraw_debugger_frames(&mut ds, tid) {
        ds.objects.unpin(id);
    }
    crate::debug::release_disposed_objects(shared, &mut ds);
}

/// A thread entering a blocking native region (`Thread.sleep`,
/// `Object.wait`, blocking I/O, `LockSupport.park`) while a debugger is
/// attached makes its interpreter frames available to
/// `ThreadReference.Frames` / `FrameCount` / `StackFrame.GetValues` for as long
/// as it is blocked (interpreter round i1 wave 10): jdb's `where` on a
/// sleeping thread listed no frames. The top frame is at its
/// `last_instr_pc`, the invoke of the native (the native method itself has
/// no interpreter frame, where HotSpot would list it first).
///
/// Wave 12: lazily. Wave 10 published a snapshot here (every reference local
/// exported as a pinned object id, under the debug-state lock) on every
/// blocking call of every thread, and withdrew it on the way out, although
/// only a suspended thread's frames are ever read (`Thread.sleep` alone
/// re-enters the region every 10 ms slice). Now this publishes only for a
/// thread that is already suspended as it blocks (one per-VM load while no
/// suspension is in force); every other thread only opens an inspection
/// window ([`open_blocked_inspection`], one store), through which a debugger
/// command reads its frames if it is suspended while blocked
/// ([`publish_frames_of_blocked_thread`]).
///
/// Called by `NativeContextImpl::begin_blocking_region_with_state` BEFORE the
/// thread marks itself GC-blocked: it is still a running mutator, so no
/// collection moves the objects its frames name while they are exported
/// (object ids are weak handles, remapped by every collection after).
/// Answers whether it published; [`close_blocked_inspection`] withdraws it.
#[cfg(feature = "experimental-debug")]
#[inline]
pub(crate) fn publish_blocked_frames(shared: &SharedVm, thread: &JvmThread) -> bool {
    let gates = &shared.debug.debugger_gates;
    if !gates.session_attached()
        || !gates.suspension_in_force()
        || crate::debug::debugger_hooks_suppressed()
    {
        return false;
    }
    publish_blocked_frames_if_suspended(shared, thread)
}

/// The location a blocked thread's top frame is listed at (interpreter
/// round i1 wave 25, L1d): its `last_instr_pc` — the invoke of the blocking
/// native — except for a thread blocked entering a monitor, which is listed
/// at the bytecode AFTER its `monitorenter`, as HotSpot lists it (its
/// template interpreter advances the bcp before it locks, "so exception
/// handling for async. exceptions work correctly"): `L1W24JdiThreads`'
/// `blocked` thread is at line 123, the block's first statement, not at
/// line 122, the `synchronized` line.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn blocked_top_pc(top: &Frame) -> usize {
    const MONITORENTER: u8 = 0xc2;
    let pc = top.last_instr_pc;
    if top.code.get(pc) == Some(&MONITORENTER) {
        pc + 1
    } else {
        pc
    }
}

/// [`publish_blocked_frames`] while a suspension is in force: publish if it
/// covers this thread.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn publish_blocked_frames_if_suspended(shared: &SharedVm, thread: &JvmThread) -> bool {
    let Some(top) = thread.frames.last() else {
        return false;
    };
    let top_pc = blocked_top_pc(top);
    let tid = thread.thread_id.0;
    if !shared.debug.debug_state.lock().is_thread_suspended(tid) {
        return false;
    }
    // The compiled activations it blocks under (wave 21), walked here on the
    // thread itself, before the lock.
    let compiled = capture_blocked_compiled_view(&thread.frames);
    // The native method it blocks in heads the list (wave 15; from the
    // innermost compiled activation's invoke when compiled code stands above
    // `top`, wave 22); looked up before the debug-state lock, which is taken
    // after the class manager's.
    let native_top = blocked_listing_head(
        shared,
        thread.thread_id,
        &thread.frames,
        &compiled,
        thread.running_native,
    );
    let splices = debugger_listing_splices(shared, thread, native_top.as_ref(), true);
    let mut ds = shared.debug.debug_state.lock();
    if !ds.is_thread_suspended(tid) {
        return false;
    }
    publish_frame_snapshot(
        shared,
        &thread.frames,
        &mut ds,
        tid,
        top_pc,
        native_top,
        Some(&compiled),
        &splices,
        |_, _, v| v,
    );
    true
}

// `GcBlockState::debugger_inspect` (wave 12): 0, or the blocked thread's
// `JvmThread` address with these flags in its alignment bits.

/// The window is open: the thread is blocked and its frames hold still.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) const INSPECT_OPEN: usize = 1;
/// A debugger command is reading the frames; the thread's leave waits.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) const INSPECT_READING: usize = 2;
/// A snapshot of the frames stands in the debug state; the leave withdraws
/// it.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) const INSPECT_PUBLISHED: usize = 4;
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) const INSPECT_FLAGS: usize =
    INSPECT_OPEN | INSPECT_READING | INSPECT_PUBLISHED;
#[cfg(feature = "experimental-debug")]
const _: () = assert!(std::mem::align_of::<JvmThread>() > INSPECT_FLAGS);

/// The thread has deposited its roots and is GC-blocked: open its inspection
/// window while a debugger is attached — or, since interpreter round i1 wave
/// 41 (lane L1), whenever the VM runs a JDWP server (`--jdwp-port`), so a
/// debugger that attaches while the thread is already blocked can list it —
/// and record a snapshot [`publish_blocked_frames`] published (`published`).
/// One load and a config read without a debugger.
///
/// Wave 41: the window used to open only with a session attached, so a thread
/// that blocked before the debugger attached (a `main` already waiting when
/// the IDE connects) had none until it woke: `ThreadReference.Frames` on it,
/// suspended, answered no frame at all, where HotSpot lists its stack
/// (`tools/probes/interp/L1/L1W41JdiSetValuesOnBlockedThread.java`, whose
/// `main` waits before the debugger attaches).
///
/// Called by `begin_blocking_region_with_state` after the deposit, which
/// fills `GcBlockState::slot_origins`: the per-slot current addresses a
/// reader needs once a collection has moved objects the blocked frames name.
#[cfg(feature = "experimental-debug")]
#[inline]
pub(crate) fn open_blocked_inspection(shared: &SharedVm, thread: &JvmThread, published: bool) {
    // Wave 45 (lane L1): also while a C JVMTI env is bound to the VM, whose
    // stack functions read a blocked thread through the window
    // ([`blocked_frame_rows`]).
    // Wave 136 (lane L1, `CRATONVM_JDWP_AGENT_DEBUGGER_FACTS`): `-agentlib:jdwp`
    // as well as `--jdwp-port` (`VmConfig::jdwp_debugger_configured`).
    if !published
        && ((!shared.debug.debugger_gates.session_attached()
            && !shared.config.jdwp_debugger_configured()
            && !shared.debug.debugger_gates.native_env_present())
            || crate::debug::debugger_hooks_suppressed())
    {
        return;
    }
    let addr = thread as *const JvmThread as usize; // Cast: the window names the thread
    let flags = if published {
        INSPECT_OPEN | INSPECT_PUBLISHED
    } else {
        INSPECT_OPEN
    };
    // A reader lists the compiled activations this thread blocks under (wave
    // 21); only the thread can walk them, so it records them before the
    // window opens. Under compiled code only: one thread-local read otherwise.
    if !published && crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
        *thread.gc_block_state.debugger_compiled.lock() =
            capture_blocked_compiled_view(&thread.frames);
    }
    thread
        .gc_block_state
        .debugger_inspect
        .store(addr | flags, std::sync::atomic::Ordering::Release);
}

/// The thread is about to leave its blocking region (the first step of
/// `end_blocking_region`, still GC-blocked): close its inspection window,
/// waiting out a debugger command that is reading its frames, and withdraw
/// a snapshot published for this region (its object ids unpinned). A thread
/// suspended meanwhile parks at its next interpreter bytecode and publishes
/// its frames again there. One load when no window is open.
#[cfg(feature = "experimental-debug")]
#[inline]
pub(crate) fn close_blocked_inspection(shared: &SharedVm, thread: &JvmThread) {
    if thread
        .gc_block_state
        .debugger_inspect
        .load(std::sync::atomic::Ordering::Acquire)
        != 0
    {
        close_blocked_inspection_slow(shared, thread);
    }
}

/// [`close_blocked_inspection`] with a window open.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn close_blocked_inspection_slow(shared: &SharedVm, thread: &JvmThread) {
    let published = close_inspection_window(&thread.gc_block_state.debugger_inspect);
    // No reader holds the window now: drop the compiled activations recorded
    // for it (wave 21), which describe a stack the thread is about to leave.
    *thread.gc_block_state.debugger_compiled.lock() =
        crate::threading::jvm_thread::BlockedCompiledView::default();
    if !published {
        return;
    }
    let tid = thread.thread_id.0;
    let mut ds = shared.debug.debug_state.lock();
    for id in withdraw_debugger_frames(&mut ds, tid) {
        ds.objects.unpin(id);
    }
    crate::debug::release_disposed_objects(shared, &mut ds);
}

/// Set a window word to 0 once no reader holds it; answers whether a
/// snapshot was published through it.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn close_inspection_window(word: &std::sync::atomic::AtomicUsize) -> bool {
    use std::sync::atomic::Ordering;
    loop {
        let w = word.load(Ordering::Acquire);
        if w & INSPECT_READING != 0 {
            // The reader runs on a mutator between two polls and waits for
            // nothing this thread holds: it finishes.
            std::thread::yield_now();
            continue;
        }
        if word
            .compare_exchange_weak(w, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return w & INSPECT_PUBLISHED != 0;
        }
    }
}

/// Can a debugger command read blocked thread `tid`'s frames: is its
/// inspection window open and free? The JDWP server's lock-free pre-check
/// (`debug::publish_blocked_frames_for`) before it hands the read to a
/// registered mutator. (A snapshot published through the window earlier may
/// be gone with a detached session, so whether one stands is decided under
/// the debug-state lock.)
#[cfg(feature = "experimental-debug")]
pub(crate) fn blocked_frames_readable(shared: &SharedVm, tid: u64) -> bool {
    shared
        .threads
        .thread_registry
        .gc_block_state_of(crate::threading::jvm_thread::ThreadId(tid))
        .is_some_and(|state| {
            let w = state
                .debugger_inspect
                .load(std::sync::atomic::Ordering::Acquire);
            w & INSPECT_OPEN != 0 && w & INSPECT_READING == 0
        })
}

/// Does blocked thread `tid`'s inspection window stand open with a snapshot
/// of its frames published through it (interpreter round i1 wave 41, lane
/// L1)? What a `StackFrame.SetValues` deferred to the thread's wake needs
/// (`debug::inspect::defer_blocked_frame_write`, which asks again under the
/// debug-state lock: the thread withdraws such a snapshot under that lock
/// after it closes the window, so a record made while this holds is seen by
/// the wake).
#[cfg(feature = "experimental-debug")]
pub(crate) fn blocked_snapshot_published(shared: &SharedVm, tid: u64) -> bool {
    let registry = &shared.threads.thread_registry;
    let thread_id = crate::threading::jvm_thread::ThreadId(tid);
    registry.gc_block_state_of(thread_id).is_some_and(|state| {
        let w = state
            .debugger_inspect
            .load(std::sync::atomic::Ordering::Acquire);
        w & INSPECT_OPEN != 0 && w & INSPECT_PUBLISHED != 0
    }) && registry.is_blocked(thread_id)
}

/// Publish the frames of thread `tid`, blocked in a native region and
/// suspended by the debugger while it was blocked, through its inspection
/// window (wave 12; before, every blocked thread published its own on entry,
/// [`publish_blocked_frames`]). Answers whether it published: not when the
/// window is closed, the thread is no longer suspended, or a snapshot of it
/// already stands.
///
/// Must run on a registered mutator between two safepoint polls (a thread
/// parked at a suspend point, or the heap service: `debug::run_on_parked_thread`),
/// so no collection moves an object while its id is minted
/// (`debug::export_object`'s rule). The blocked thread cannot leave meanwhile:
/// its leave waits while the window is marked READING
/// ([`close_blocked_inspection`]); and while blocked it does not touch its
/// frames (collections fold their pointer maps into its `GcBlockState`, not
/// into the frames). So a reference local is read as the object's current
/// address from the per-slot record its blocking deposit made
/// (`GcBlockState::slot_origins`, advanced by every collection since); a
/// slot the deposit found dead, or whose address is no longer an object, is
/// reported as null.
#[cfg(feature = "experimental-debug")]
pub(crate) fn publish_frames_of_blocked_thread(shared: &SharedVm, tid: u64) -> bool {
    use std::sync::atomic::Ordering;
    let registry = &shared.threads.thread_registry;
    let thread_id = crate::threading::jvm_thread::ThreadId(tid);
    let Some(state) = registry.gc_block_state_of(thread_id) else {
        return false;
    };
    let word = &state.debugger_inspect;
    let w = word.load(Ordering::Acquire);
    if w & INSPECT_OPEN == 0 || w & INSPECT_READING != 0 {
        return false;
    }
    if word
        .compare_exchange(w, w | INSPECT_READING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    let published = registry.is_blocked(thread_id)
        && read_blocked_frames(shared, &state, w & !INSPECT_FLAGS, tid);
    // Only this reader writes the word while READING is set.
    word.store(
        if published { w | INSPECT_PUBLISHED } else { w },
        Ordering::Release,
    );
    published
}

/// Publish the frames of UNMOUNTED virtual thread `tid` -- a continuation
/// that yielded in a sleep, a park or a keyed wait, its frames deposited in
/// the virtual-thread manager -- for the debugger that suspended it
/// (interpreter round i1 wave 48, lane L1). Such a thread is neither at a
/// suspend point nor in a blocking region, so it published nothing and
/// `ThreadReference.Frames` answered no frames at all, where HotSpot
/// (`-XX:-VMContinuations`, which is what this VM's `BoundVirtualThread`s
/// are) lists the blocking call's frames: `Thread.sleepNanos0` (native),
/// `sleepNanos`, `sleep`, caller (probe `L1W48JdiVirtualThreadFrames`).
///
/// The deposited state is held by [`VirtualThreadManager::begin_debugger_read`]
/// (a mount waits), and read like a blocked thread's: the native the top
/// frame's invoke names, or the stand-in chain, heading the listing. Its
/// frames keep their yield-time addresses until the remount applies the
/// blocked-window fold (gci i11/vt finding 3), so a reference local is
/// published through the yield deposit's `slot_origins`, as
/// [`read_blocked_frames`] publishes a blocked thread's. Must run on a
/// registered mutator between two safepoint
/// polls ([`crate::debug::run_on_parked_thread`]). Answers whether it
/// published.
///
/// [`VirtualThreadManager::begin_debugger_read`]: crate::threading::virtual_threads::VirtualThreadManager::begin_debugger_read
#[cfg(feature = "experimental-debug")]
pub(crate) fn publish_frames_of_unmounted_virtual_thread(shared: &SharedVm, tid: u64) -> bool {
    // Wave 49 (lane L1): the continuation rows name `VirtualThread`, which
    // nothing loads on this VM (its virtual threads are `BoundVirtualThread`s;
    // HotSpot loads it at boot). Loaded here, under the parked task's
    // suppressed hooks (no `ClassPrepare` posted), before the deposit is held.
    if shared.debug.debugger_gates.virtual_continuation_frames() {
        crate::vm::vm_exec::preload_virtual_thread_class(shared);
    }
    let manager = &shared.threads.virtual_thread_manager;
    let Some(addr) = manager.begin_debugger_read(tid) else {
        return false;
    };
    // SAFETY: `begin_debugger_read` keeps the box deposited and unmounted
    // until `end_debugger_read` below: nothing runs or moves these frames,
    // and our caller polls no safepoint meanwhile.
    let thread = unsafe { &*addr };
    let published = publish_unmounted_frames(shared, thread, tid);
    manager.end_debugger_read(tid);
    published
}

/// [`publish_frames_of_unmounted_virtual_thread`] with the deposit held.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn publish_unmounted_frames(shared: &SharedVm, thread: &JvmThread, tid: u64) -> bool {
    if thread.thread_id.0 != tid {
        return false;
    }
    let frames: &[Frame] = &thread.frames;
    if frames.is_empty() {
        return false;
    }
    // A continuation yields from interpreter frames only: no compiled
    // activation stands above them, and the yielding native has returned
    // (no running-native record to confirm a stand-in by).
    let compiled = crate::threading::jvm_thread::BlockedCompiledView::default();
    // Before the debug-state lock: these take the class-manager lock.
    // Wave 49 (lane L1): HotSpot's default virtual threads list their
    // continuation's frames (`VirtualThread.yieldContinuation`, ...) over the
    // blocking call; the frames above `kept` are replaced by those rows.
    let continuation = if shared.debug.debugger_gates.virtual_continuation_frames() {
        continuation_head_of_unmounted(shared, thread)
    } else {
        None
    };
    let (frames, native_top) = match (jdk_continuation_kept(shared, thread), continuation) {
        // Wave 102 (lane L1): headed by `Object.wait0` when it waits there.
        (Some(kept), _) => (
            &frames[..kept],
            monitor_wait_native_head(shared, thread, &frames[..kept]),
        ),
        (None, Some((kept, head))) => (&frames[..kept], Some(head)),
        (None, None) => (frames, blocked_native_top(shared, frames, &compiled, 0)),
    };
    let Some(top) = frames.last() else {
        return false;
    };
    let top_pc = blocked_top_pc(top);
    let splices = debugger_listing_splices(shared, thread, native_top.as_ref(), true);
    // gci i11/vt finding 3: (frame index, slot) -> (address at the yield
    // deposit, current address), for the reference locals that deposit found
    // live -- `read_blocked_frames`' map, from the continuation's own
    // `GcBlockState`, which every fold of its parked window advanced. A raw
    // slot still holds the yield-time address (only the remount rewrites the
    // frames), which a moving collection has vacated or handed to another
    // object. Built before the debug-state lock, as there.
    // Wave 126 (lane L1, i47-L1, `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS`): dead
    // locals too ([`blocked_slot_origin_read`]).
    // Wave 130 (lane L1a): or every local is a root ([`blocked_dead_slots_read`]).
    let dead_too = blocked_dead_slots_read(shared);
    // addr-keyed: not an address -- (frame index, slot); the values are the deposit's own (orig, cur) pair, read once here.
    let current: std::collections::HashMap<(usize, u16), (usize, usize)> = thread
        .gc_block_state
        .slot_origins
        .lock()
        .iter()
        .filter(|so| blocked_slot_origin_read(so, dead_too))
        .filter_map(|so| {
            let slot = u16::try_from(so.idx).ok()?;
            // Widening: a frame index
            Some(((so.frame as usize, slot), (so.orig, so.cur)))
        })
        .collect();
    let heap = &shared.mem.heap;
    let mut ds = shared.debug.debug_state.lock();
    if !ds.is_thread_suspended(tid) || ds.has_current_frames(tid) {
        return false;
    }
    let deferred = crate::debug::inspect::deferred_frame_values(shared, &ds, tid, frames);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[UNMOUNTED_VT_FRAMES] tid={tid} frames={} head={}",
            frames.len(),
            native_top.is_some()
        );
    }
    publish_frame_snapshot(
        shared,
        frames,
        &mut ds,
        tid,
        top_pc,
        native_top,
        Some(&compiled),
        &splices,
        |index, slot, v| {
            if let Some(&written) = deferred.get(&(index, slot)) {
                return written;
            }
            match v {
                Value::Object(Some(held)) => {
                    let held = held.as_ptr() as usize; // Cast: the deposit's key
                    let now = current
                        .get(&(index, slot))
                        .filter(|&&(orig, _)| orig == held)
                        .map(|&(_, cur)| cur);
                    Value::Object(now.and_then(|cur| heap.is_object_address(cur)))
                }
                other => other,
            }
        },
    );
    true
}

/// How many of an unmounted JDK continuation's deposited frames (bottom
/// first) a debugger lists (`vm::jdk_continuation`): all but the JDK's own
/// yield on top -- `Continuation.yield` and `yield0` (`@JvmtiHideEvents`),
/// with `doYield` above them -- which HotSpot's listing of an unmounted
/// virtual thread leaves out (`JvmtiEnvBase::check_and_skip_hidden_frames`),
/// starting at `VirtualThread.yieldContinuation`. `None` for any other thread.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn jdk_continuation_kept(shared: &SharedVm, thread: &JvmThread) -> Option<usize> {
    if !thread.jdk_continuation.jdk() {
        return None;
    }
    let cm = shared.classes.class_manager.read_recursive();
    let mut kept = thread.frames.len();
    while kept > 0 {
        let frame = &thread.frames[kept - 1];
        let in_yield = cm
            .class_store
            .get(frame.class_id)
            .is_some_and(|c| &*c.name == "jdk/internal/vm/Continuation")
            && matches!(frame.method_name(), "yield" | "yield0");
        if !in_yield {
            break;
        }
        kept -= 1;
    }
    Some(kept)
}

/// The rows HotSpot's DEFAULT virtual threads (continuations on) list for an
/// unmounted virtual thread blocked in a sleep or a park, and how many of
/// `thread`'s deposited frames (bottom first) stay below them (interpreter
/// round i1 wave 49, lane L1;
/// `docs/feature-designs/real-jdk-mode.md`).
/// This VM's virtual threads are `BoundVirtualThread`s, whose JDK bodies take
/// the platform branch (`Thread.sleepNanos0`, `BoundVirtualThread.parkNanos`
/// over `Unsafe.park`), or block in a stand-in; HotSpot 25.0.3 lists, top
/// first, each at its FIRST call of the next row, read from the real class
/// bytes:
///
/// * a sleep: `VirtualThread.yieldContinuation` (at `Continuation.yield`),
///   `VirtualThread.parkNanos` (at `yieldContinuation`),
///   `VirtualThread.sleepNanos` (at `parkNanos`), `Thread.sleepNanos` (at
///   `VirtualThread.sleepNanos`), then `Thread.sleep` (at `sleepNanos`) when
///   the sleep's stand-in ran it; a `Thread.sleepNanos` frame on top (its
///   bytecode ran) is replaced by its row;
/// * a park: `yieldContinuation`, `VirtualThread.parkNanos` / `park` (the
///   method `System$1.parkVirtualThread` calls through `BaseVirtualThread`),
///   over a kept `System$1.parkVirtualThread` frame (the frames above it,
///   `BoundVirtualThread.parkNanos`, go); or, for a `LockSupport.park*`
///   stand-in, also `System$1.parkVirtualThread` (at its `BaseVirtualThread`
///   call) and the `LockSupport` method (at its `JavaLangAccess` call).
///
/// The rows ran nowhere here, so they are opaque (`OPAQUE_FRAME` to the
/// local commands, `PopFrames` refused), as the stand-in levels are. `None`
/// for any other shape (a keyed wait, a pinned thread, a class not loaded):
/// the listing falls back to the frames that ran. Takes the class-manager
/// lock: call it before the debug-state lock. Positive control:
/// `CRATONVM_FRAME_TRACE=1` prints `[VT_CONTINUATION_FRAMES] tid=<n>
/// kind=<sleep|park> kept=<k> levels=<l>`.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn continuation_head_of_unmounted(
    shared: &SharedVm,
    thread: &JvmThread,
) -> Option<(usize, NativeHead)> {
    use crate::threading::jvm_thread::VtYield;
    const THREAD: &str = "java/lang/Thread";
    const VIRTUAL_THREAD: &str = "java/lang/VirtualThread";
    const BASE_VIRTUAL_THREAD: &str = "java/lang/BaseVirtualThread";
    const SYSTEM_1: &str = "java/lang/System$1";
    const LOCK_SUPPORT: &str = "java/util/concurrent/locks/LockSupport";
    const JAVA_LANG_ACCESS: &str = "jdk/internal/access/JavaLangAccess";
    const SLEEP_NANOS: (&str, &str) = ("sleepNanos", "(J)V");
    const YIELD_CONTINUATION: (&str, &str) = ("yieldContinuation", "()Z");

    /// The method the invoke (any of the four) at `pc` of `code` calls.
    fn invoke_at<'c>(
        class: &'c crate::classloading::Class,
        code: &[u8],
        pc: usize,
    ) -> Option<(&'c str, &'c str, &'c str)> {
        use cratonvm_reader::instruction::Instruction;
        match Instruction::decode(code, pc).ok()?.0 {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index)
            | Instruction::Invokeinterface { index, .. } => {
                crate::runtime::stackwalker::cp_method_ref(&class.constant_pool, index)
            }
            _ => None,
        }
    }

    let sleep = match thread.vt_yield {
        VtYield::Sleep { .. } => true,
        VtYield::Park => false,
        // Interpreter round i5 wave 102, lane L1 (`L1W48JdiVirtualThreadFrames`
        // on w101b, both modes): a virtual thread unmounted in `Object.wait`
        // (JDK 25 unmounts on wait) kept the frames that ran, which end at
        // `Object.wait(long)` standing at its `wait0` call: the native
        // `wait0` row HotSpot lists on top (`java.lang.Object.wait0[native]
        // java.lang.Object.wait ...`) was missing. Head it with that native.
        VtYield::MonitorWait => {
            return monitor_wait_native_head(shared, thread, &thread.frames)
                .map(|head| (thread.frames.len(), head))
        }
        // A JDK-driven continuation keeps the frames that ran: there is
        // nothing to stand in for.
        VtYield::KeyedWait { .. } | VtYield::DoYield | VtYield::MonitorEnter => return None,
    };
    let frames = &thread.frames;
    let top = frames.last()?;
    let cm = shared.classes.class_manager.read_recursive();
    let store = &cm.class_store;
    let name_of = |id: ClassId| store.get(id).map(|c| &*c.name);
    // `(owner, name, descriptor)` of the call frame `f` stands at, read from
    // its method's class bytes (not the frame's possibly quickened copy).
    let call_of = |f: &Frame| -> Option<(String, String, String)> {
        let class = store.get(f.class_id)?;
        let method = class.find_method(f.method_name(), f.method_descriptor())?;
        let (o, n, d) = invoke_at(class, &method.code()?.code, f.last_instr_pc)?;
        Some((o.to_owned(), n.to_owned(), d.to_owned()))
    };
    // The first call in `class_id.name descriptor` that `accept` admits: its
    // pc and the called `(name, descriptor)`.
    let first_call = |class_id: ClassId,
                      name: &str,
                      descriptor: &str,
                      accept: &dyn Fn(&str, &str, &str) -> bool|
     -> Option<(usize, String, String)> {
        use cratonvm_reader::instruction::Instruction;
        let class = store.get(class_id)?;
        let code = &class.find_method(name, descriptor)?.code()?.code;
        let mut pc = 0usize;
        while pc < code.len() {
            let (_, next) = Instruction::decode(code, pc).ok()?;
            if let Some((o, n, d)) = invoke_at(class, code, pc) {
                if accept(o, n, d) {
                    return Some((pc, n.to_owned(), d.to_owned()));
                }
            }
            pc = next;
        }
        None
    };
    let level =
        |class_id: ClassId, name: &str, descriptor: &str, pc: usize| -> Option<StandinLevel> {
            let class = store.get(class_id)?;
            let index = class
                .methods
                .iter()
                .position(|m| &*m.name == name && &*m.descriptor == descriptor)?;
            // Interpreter round i6 wave 129, lane L1 (i119-L1's virtual
            // variant, `CRATONVM_DEBUG_STANDIN_LEVEL_LOCALS`): a
            // `VirtualThread` instance row's `this` is the thread itself
            // (HotSpot 25.0.3 answers `ThisObject` with it); the publisher
            // reads it only under that switch.
            let locals = if &*class.name == VIRTUAL_THREAD && !class.methods[index].is_static() {
                StandinLevelLocals::VirtualThreadThis
            } else {
                StandinLevelLocals::Unknown
            };
            Some(StandinLevel {
                class_id: u64::from(class_id.as_u32()),
                method_id: crate::debug::jdwp_method_id(name, descriptor),
                method_index: u16::try_from(index).ok()?,
                bci: u64::try_from(pc).ok()?,
                locals,
            })
        };
    // Top first: `yieldContinuation` at `Continuation.yield`, then
    // `VirtualThread.name descriptor` at its `yieldContinuation` call.
    let continuation_rows = |requester: ClassId,
                             name: &str,
                             descriptor: &str,
                             levels: &mut Vec<StandinLevel>|
     -> Option<ClassId> {
        let vt = cm.find_class_by_name_for_class(VIRTUAL_THREAD, requester)?;
        let (pc, ..) = first_call(
            vt,
            YIELD_CONTINUATION.0,
            YIELD_CONTINUATION.1,
            &|o, n, _| o == "jdk/internal/vm/Continuation" && n == "yield",
        )?;
        levels.push(level(vt, YIELD_CONTINUATION.0, YIELD_CONTINUATION.1, pc)?);
        let (pc, ..) = first_call(vt, name, descriptor, &|o, n, d| {
            o == VIRTUAL_THREAD && (n, d) == YIELD_CONTINUATION
        })?;
        levels.push(level(vt, name, descriptor, pc)?);
        Some(vt)
    };
    let mut levels: Vec<StandinLevel> = Vec::with_capacity(NATIVE_HEAD_LEVELS_MAX);
    let kept;
    if sleep {
        let (thread_id, standin) = if name_of(top.class_id) == Some(THREAD)
            && (top.method_name(), top.method_descriptor()) == SLEEP_NANOS
        {
            kept = frames.len() - 1;
            (top.class_id, None)
        } else {
            // A stand-in of `sleep(long)` / `sleep(long, int)`, or of
            // `sleepNanos` itself (called by `sleep(Duration)`'s bytecode).
            let (owner, callee, descriptor) = call_of(top)?;
            let standin = match (owner.as_str(), callee.as_str(), descriptor.as_str()) {
                (THREAD, "sleep", "(J)V" | "(JI)V") => Some(descriptor),
                (THREAD, name, d) if (name, d) == SLEEP_NANOS => None,
                _ => return None,
            };
            kept = frames.len();
            (
                cm.find_class_by_name_for_class(THREAD, top.class_id)?,
                standin,
            )
        };
        let vt = continuation_rows(thread_id, "parkNanos", "(J)V", &mut levels)?;
        let (pc, ..) = first_call(vt, SLEEP_NANOS.0, SLEEP_NANOS.1, &|o, n, d| {
            o == VIRTUAL_THREAD && (n, d) == ("parkNanos", "(J)V")
        })?;
        levels.push(level(vt, SLEEP_NANOS.0, SLEEP_NANOS.1, pc)?);
        let (pc, ..) = first_call(thread_id, SLEEP_NANOS.0, SLEEP_NANOS.1, &|o, n, d| {
            o == VIRTUAL_THREAD && (n, d) == SLEEP_NANOS
        })?;
        levels.push(level(thread_id, SLEEP_NANOS.0, SLEEP_NANOS.1, pc)?);
        if let Some(descriptor) = standin {
            let (pc, ..) = first_call(thread_id, "sleep", &descriptor, &|o, n, d| {
                o == THREAD && (n, d) == SLEEP_NANOS
            })?;
            levels.push(level(thread_id, "sleep", &descriptor, pc)?);
        }
    } else {
        let is_bridge = |f: &Frame| {
            name_of(f.class_id) == Some(SYSTEM_1) && f.method_name() == "parkVirtualThread"
        };
        let bridge = (frames.len().saturating_sub(2)..frames.len())
            .rev()
            .find(|&i| is_bridge(&frames[i]));
        if let Some(at) = bridge {
            let (owner, callee, descriptor) = call_of(&frames[at])?;
            if owner != BASE_VIRTUAL_THREAD || !matches!(&*callee, "park" | "parkNanos") {
                return None;
            }
            continuation_rows(frames[at].class_id, &callee, &descriptor, &mut levels)?;
            kept = at + 1;
        } else {
            let (owner, callee, descriptor) = call_of(top)?;
            if owner != LOCK_SUPPORT || !callee.starts_with("park") {
                return None;
            }
            let lock_support = cm.find_class_by_name_for_class(LOCK_SUPPORT, top.class_id)?;
            let (ls_pc, _, access_descriptor) =
                first_call(lock_support, &callee, &descriptor, &|o, n, _| {
                    o == JAVA_LANG_ACCESS && n == "parkVirtualThread"
                })?;
            let system_1 = cm.find_class_by_name_for_class(SYSTEM_1, lock_support)?;
            let (s1_pc, park, park_descriptor) = first_call(
                system_1,
                "parkVirtualThread",
                &access_descriptor,
                &|o, n, _| o == BASE_VIRTUAL_THREAD && matches!(n, "park" | "parkNanos"),
            )?;
            continuation_rows(system_1, &park, &park_descriptor, &mut levels)?;
            levels.push(level(
                system_1,
                "parkVirtualThread",
                &access_descriptor,
                s1_pc,
            )?);
            levels.push(level(lock_support, &callee, &descriptor, ls_pc)?);
            kept = frames.len();
        }
    }
    drop(cm);
    if kept == 0 {
        return None;
    }
    let head = NativeHead::continuation(&levels)?;
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[VT_CONTINUATION_FRAMES] tid={} kind={} kept={kept} levels={}",
            thread.thread_id.0,
            if sleep { "sleep" } else { "park" },
            levels.len()
        );
    }
    Some((kept, head))
}

/// The head of a virtual thread unmounted in `Object.wait` (interpreter
/// round i5 wave 102, lane L1; [`continuation_head_of_unmounted`]'s
/// `MonitorWait` arm, and the JDK-continuation arm of the two unmounted
/// listings over the frames [`jdk_continuation_kept`] keeps): every frame
/// `frames` holds is kept, and when the top one
/// stands at a call of `java.lang.Object.wait0(J)V` (JDK 25's
/// `Object.wait(long)`, which unmounts a virtual thread from inside its
/// native) that native heads the listing at location -1, as HotSpot 25 lists
/// it (`L1W48JdiVirtualThreadFrames`: `java.lang.Object.wait0[native]
/// java.lang.Object.wait <caller>`). `None` (the frames that ran, as before)
/// for any other shape. Takes the class-manager lock.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn monitor_wait_native_head(
    shared: &SharedVm,
    thread: &JvmThread,
    frames: &[Frame],
) -> Option<NativeHead> {
    use cratonvm_reader::instruction::Instruction;
    const OBJECT: &str = "java/lang/Object";
    let top = frames.last()?;
    let cm = shared.classes.class_manager.read_recursive();
    let class = cm.class_store.get(top.class_id)?;
    if &*class.name != OBJECT || top.method_name() == "wait0" {
        return None;
    }
    // The call the frame stands at, from its method's class bytes.
    let method = class.find_method(top.method_name(), top.method_descriptor())?;
    let code = &method.code()?.code;
    let index = match Instruction::decode(code, top.last_instr_pc).ok()?.0 {
        Instruction::Invokevirtual(index)
        | Instruction::Invokespecial(index)
        | Instruction::Invokestatic(index) => index,
        _ => return None,
    };
    let (owner, name, descriptor) =
        crate::runtime::stackwalker::cp_method_ref(&class.constant_pool, index)?;
    if (owner, name, descriptor) != (OBJECT, "wait0", "(J)V") {
        return None;
    }
    let head = NativeHead::leaf(
        u64::from(top.class_id.as_u32()),
        crate::debug::jdwp_method_id("wait0", "(J)V"),
    );
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[VT_CONTINUATION_FRAMES] tid={} kind=wait kept={} levels=0",
            thread.thread_id.0,
            frames.len()
        );
    }
    Some(head)
}

/// The listing of thread `tid`, blocked in a native region, read through its
/// inspection window without publishing anything (interpreter round i1
/// wave 45, lane L1: the C JVMTI table's stack functions asked about another
/// thread, `jvmti::native_env::frames_of`). Rows are `(class id, JDWP method
/// id, location)`, top first, in the order and at the locations
/// [`publish_frame_snapshot`] lists them: the native the thread blocks in at
/// location -1, then the interpreter frames with the compiled activations
/// spliced in. No local is read, so no object id is minted and the caller
/// need not be a mutator between two polls. `None` when the window is not
/// open, another reader holds it, or the thread is not blocked. The thread
/// need not be suspended: while the window is marked READING its leave
/// waits, as for [`publish_frames_of_blocked_thread`].
#[cfg(feature = "experimental-debug")]
pub(crate) fn blocked_frame_rows(shared: &SharedVm, tid: u64) -> Option<Vec<(u64, u64, i64)>> {
    with_blocked_frames(shared, tid, |held| {
        held.rows
            .iter()
            .map(|row| (row.class_id, row.method_id, row.location))
            .collect()
    })
}

/// One row of a blocked thread's listing ([`with_blocked_frames`],
/// interpreter round i1 wave 46, lane L1): the `(class id, JDWP method id,
/// location)` [`blocked_frame_rows`] lists, and the interpreter frame the
/// row is, when it is one.
#[cfg(feature = "experimental-debug")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockedRow {
    pub(crate) class_id: u64,
    pub(crate) method_id: u64,
    pub(crate) location: i64,
    /// The index (from the bottom) of the interpreter frame this row is;
    /// `None` for the native method heading the listing and for a compiled
    /// activation.
    pub(crate) frame: Option<usize>,
    /// The frame's body runs compiled: its locals hold the values of its
    /// compiled entry, not its current ones (the JDWP listing marks it
    /// opaque, `DebugState::opaque_frames`).
    pub(crate) runs_compiled: bool,
    /// A compiled activation's JIT entry-chain index
    /// (`BlockedCompiledRow::chain`, carried by `ListingRow::Opaque`;
    /// interpreter round i6 wave 120, lane L1): the calling thread's
    /// owned-monitor depths attribute a compiled hold to this row by it
    /// (`native_env::listed_rows`). `None` for every other row.
    pub(crate) chain: Option<u32>,
}

/// A blocked thread's frames, held still through its inspection window for
/// the duration of a [`with_blocked_frames`] call (interpreter round i1
/// wave 46, lane L1): what the C JVMTI table's local-variable and monitor
/// functions read and write of another thread blocked in a native region
/// (`jvmti::native_env`, `interpreter-L1-the-c-jvmti-table-reads-no-other-threads-stack-FIXED-20261010`).
#[cfg(feature = "experimental-debug")]
pub(crate) struct BlockedFrames<'a> {
    /// The blocked thread's `JvmThread` (the window's address).
    pub(in crate::runtime::interpreter) thread: *mut JvmThread,
    /// The blocking deposit's state: a native region's, or an UNMOUNTED
    /// virtual thread's yield deposit ([`with_unmounted_frames`], interpreter
    /// round i1 wave 51, lane L1). gci i11/vt finding 3: an unmounted
    /// continuation's frames are NOT the collector's roots in place -- every
    /// moving collection of its parked window is folded into its
    /// `GcBlockState` (`fold_pointer_map_into_blocked`: `fixup`,
    /// `slot_origins`), and only the remount's `check_post_block_gc` rewrites
    /// the frames -- so its reference locals are read through `slot_origins`
    /// exactly as a blocked thread's. `None` only where no deposit recorded
    /// one.
    pub(in crate::runtime::interpreter) state: Option<&'a crate::threading::jvm_thread::GcBlockState>,
    /// An unmounted virtual thread's deposited frames (read only), rather
    /// than a thread blocked in a native region.
    pub(in crate::runtime::interpreter) unmounted: bool,
    /// The listing, top first, as [`blocked_frame_rows`] lists it.
    pub(crate) rows: Vec<BlockedRow>,
    /// The [`StandinLevelLocals`] tags of the listing head's JDK levels whose
    /// locals are known, by row position (top first), as
    /// `publish_frame_snapshot` places them for JDWP (`has_native + k` for
    /// level `k`). Interpreter round i6 wave 132, lane L1 (i119-L1 residual):
    /// filled for an unmounted virtual thread only
    /// ([`with_unmounted_frames`]), so the C table's `GetLocalInstance` of a
    /// `VirtualThreadThis` row answers what JDWP's `ThisObject` answers.
    /// Empty for a thread blocked in a native region (its `Object.wait`
    /// levels are recognised by name, `native_env::waited_monitor_level`).
    pub(in crate::runtime::interpreter) level_locals: Vec<(usize, StandinLevelLocals)>,
}

#[cfg(feature = "experimental-debug")]
impl BlockedFrames<'_> {
    /// Are these an unmounted virtual thread's deposited frames
    /// ([`with_unmounted_frames`]) rather than a thread blocked in a native
    /// region? HotSpot keeps such a thread's frames in a heap stack chunk,
    /// whose locals a debugger reads but does not write
    /// (`JVMTI_ERROR_OPAQUE_FRAME`, `VM_BaseGetOrSetLocal::doit`).
    pub(crate) fn unmounted(&self) -> bool {
        self.unmounted
    }

    /// The [`StandinLevelLocals`] tag of row `depth` (top first) when it is
    /// one of the listing head's JDK levels with known locals
    /// ([`Self::level_locals`], interpreter round i6 wave 132, lane L1);
    /// [`StandinLevelLocals::Unknown`] otherwise.
    pub(crate) fn level_locals_at(&self, depth: usize) -> StandinLevelLocals {
        self.level_locals
            .iter()
            .find(|&&(position, _)| position == depth)
            .map_or(StandinLevelLocals::Unknown, |&(_, locals)| locals)
    }

    /// The blocked thread's id (wave 47).
    pub(crate) fn tid(&self) -> u64 {
        // SAFETY: as in `frames`; `thread_id` is an immutable `Copy` field.
        unsafe { (*self.thread).thread_id.0 }
    }

    /// Does the blocked thread stand under no continuation-return adapter
    /// (interpreter round i6 wave 134, lane L1: a `PopFrame` through the
    /// window refuses a thread that does, as `debug::pop_frames` refuses a
    /// parked one)?
    pub(crate) fn no_continuation_adapters(&self) -> bool {
        // SAFETY: as in `frames`; the thread does not touch the field while
        // the window is held.
        unsafe { (*self.thread).continuation_return_adapters.is_empty() }
    }

    /// The blocked thread's interpreter frames, bottom first.
    pub(crate) fn frames(&self) -> &[Frame] {
        // SAFETY: the window is held READING for the life of `self`: the
        // thread stays in place and does not touch its frames (see
        // `read_blocked_frames`).
        unsafe { &(*self.thread).frames }
    }

    /// The object reference local `slot` of frame `index` holds NOW:
    /// `held`, the address the slot kept while the thread blocked, advanced
    /// through every collection since (`GcBlockState::slot_origins`, as
    /// [`read_blocked_frames`] reads it); `None` for a slot the blocking
    /// deposit found dead (with `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS=0`; wave
    /// 126: [`blocked_slot_origin_read`]) or whose address is no longer an
    /// object's. Call it
    /// where no collection can run (inside a JNI function's foreign-thread
    /// entry), and make a handle of the answer before leaving that. An
    /// unmounted virtual thread's slots are read the same way (gci i11/vt
    /// finding 3): its frames keep the yield-time addresses until the remount
    /// applies the fold, so a raw slot names a vacated -- or another
    /// object's -- address after any moving collection of its parked window.
    pub(crate) fn current_reference(
        &self,
        shared: &SharedVm,
        index: usize,
        slot: u16,
        held: ObjectRef,
    ) -> Option<ObjectRef> {
        let held = held.as_ptr() as usize; // Cast: the deposit's key
        let Some(state) = self.state else {
            return shared.mem.heap.is_object_address(held);
        };
        // Wave 126 (lane L1, i47-L1): a dead slot too, with the switch on.
        // Wave 130 (lane L1a): or every local is a root ([`blocked_dead_slots_read`]).
        let dead_too = blocked_dead_slots_read(shared);
        let cur = state
            .slot_origins
            .lock()
            .iter()
            .find(|so| {
                blocked_slot_origin_read(so, dead_too)
                    // Widening: a frame index and a slot
                    && so.frame as usize == index
                    && so.idx == u32::from(slot)
                    && so.orig == held
            })
            .map(|so| so.cur)?;
        shared.mem.heap.is_object_address(cur)
    }

    /// Store `value`, a primitive, into local `slot` of frame `index`. A
    /// blocked thread's primitive locals are its own again only at its wake,
    /// which rewrites the reference slots its blocking deposit recorded and
    /// nothing else, so a primitive stored into a slot that holds no
    /// reference is what the thread reads when it wakes. A reference is not
    /// stored here: it would be neither a root nor remapped while the thread
    /// sleeps (`debug::inspect::defer_blocked_frame_write` defers JDWP's).
    /// `false` for a reference value or a frame that is not there.
    pub(crate) fn set_primitive_local(&self, index: usize, slot: u16, value: Value) -> bool {
        // An unmounted virtual thread's frames are read only (`unmounted`).
        if matches!(value, Value::Object(_)) || self.unmounted() {
            return false;
        }
        // SAFETY: as in `frames`; the blocked thread reads the frame again
        // only after its leave, which waits while this reader holds the
        // window.
        let Some(frame) = (unsafe { (*self.thread).frames.get_mut(index) }) else {
            return false;
        };
        frame.set_local(slot, value);
        true
    }
}

/// Run `work` on blocked thread `tid`'s frames, held still through its
/// inspection window (interpreter round i1 wave 46, lane L1): the window is
/// marked READING for the call, so the thread's leave waits, as for
/// [`publish_frames_of_blocked_thread`]. `None` when the window is not open,
/// another reader holds it, or the thread is not blocked. The thread need not
/// be suspended. `work` runs with no lock held; it may take the class
/// manager and the debug state (this reader takes the class manager itself,
/// to name the native heading the listing).
#[cfg(feature = "experimental-debug")]
pub(crate) fn with_blocked_frames<R>(
    shared: &SharedVm,
    tid: u64,
    work: impl FnOnce(&BlockedFrames<'_>) -> R,
) -> Option<R> {
    use std::sync::atomic::Ordering;
    let registry = &shared.threads.thread_registry;
    let thread_id = crate::threading::jvm_thread::ThreadId(tid);
    let state = registry.gc_block_state_of(thread_id)?;
    let word = &state.debugger_inspect;
    let w = word.load(Ordering::Acquire);
    if w & INSPECT_OPEN == 0 || w & INSPECT_READING != 0 {
        return None;
    }
    word.compare_exchange(w, w | INSPECT_READING, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    let addr = w & !INSPECT_FLAGS;
    let answer = if registry.is_blocked(thread_id) {
        read_blocked_rows(shared, &state, addr, tid).map(|rows| {
            let held = BlockedFrames {
                thread: addr as *mut JvmThread, // Cast: the window's address back to its thread
                state: Some(&*state),
                unmounted: false,
                rows,
                level_locals: Vec::new(),
            };
            work(&held)
        })
    } else {
        None
    };
    // Only this reader writes the word while READING is set.
    word.store(w, Ordering::Release);
    answer
}

/// [`with_blocked_frames`]'s listing once the window is held: `addr` is the
/// blocked thread's `JvmThread`, which stays in place and does not touch its
/// frames while the window is marked READING (see [`read_blocked_frames`]).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn read_blocked_rows(
    shared: &SharedVm,
    state: &crate::threading::jvm_thread::GcBlockState,
    addr: usize,
    tid: u64,
) -> Option<Vec<BlockedRow>> {
    // SAFETY: as in `read_blocked_frames`: the thread named itself when it
    // opened the window and cannot close it while this reader holds it.
    let thread = unsafe { &*(addr as *const JvmThread) };
    if thread.thread_id.0 != tid {
        return None;
    }
    let frames: &[Frame] = &thread.frames;
    let top_pc = blocked_top_pc(frames.last()?);
    let compiled = state.debugger_compiled.lock().clone();
    let native_top = blocked_listing_head(
        shared,
        thread.thread_id,
        frames,
        &compiled,
        thread.running_native,
    );
    // Wave 47: the listing JDWP publishes for the thread ([`build_frame_listing`]),
    // with the JNI natives and reflective calls it stands under.
    let splices = debugger_listing_splices(shared, thread, native_top.as_ref(), true);
    let listing = build_frame_listing(frames, top_pc, native_top.as_ref(), &compiled, &splices);
    Some(listing_blocked_rows(listing))
}

/// The CALLING thread's listing, top first, from the one builder JDWP and the
/// C table's other-thread reads use ([`build_frame_listing`], with
/// [`read_blocked_rows`]' steps): its compiled activations walked here on
/// the thread itself ([`capture_blocked_compiled_view`]), the head its
/// running-native record names ([`blocked_listing_head`]: a stand-in's
/// native leaf and JDK levels, a registered native), and the splices of its
/// JNI natives, reflective calls and served `Thread.run` calls
/// ([`debugger_listing_splices`]). Interpreter round i6 wave 119, lane L1
/// (`CRATONVM_JVMTI_C_LISTING_SHARED`, default ON since the i6 wave-126 flip, `=0` off; i102-L1 item 1 /
/// i46-L1 step 2): the C table's `frames_of` of the current thread reads it
/// instead of building its own from a stack-trace capture
/// (`jvmti::native_env::listed_rows`).
#[cfg(feature = "experimental-debug")]
pub(crate) fn listing_rows_of_current_thread(
    shared: &SharedVm,
    thread: &JvmThread,
) -> Vec<BlockedRow> {
    let frames: &[Frame] = &thread.frames;
    let top_pc = frames.last().map_or(0, blocked_top_pc);
    let compiled = capture_blocked_compiled_view(frames);
    let native_top = blocked_listing_head(
        shared,
        thread.thread_id,
        frames,
        &compiled,
        thread.running_native,
    );
    let splices = debugger_listing_splices(shared, thread, native_top.as_ref(), true);
    let listing = build_frame_listing(frames, top_pc, native_top.as_ref(), &compiled, &splices);
    listing_blocked_rows(listing)
}

/// The listing of UNMOUNTED virtual thread `tid` -- a continuation that
/// yielded in a sleep or a park, its frames deposited in the virtual-thread
/// manager -- top first, as the C JVMTI table lists another thread
/// (interpreter round i1 wave 50, lane L1;
/// `interpreter-L1-the-c-jvmti-table-lists-no-frames-for-an-unmounted-virtual-thread-FIXED`):
/// the rows [`publish_frames_of_unmounted_virtual_thread`] publishes for
/// JDWP, HotSpot's continuation rows over the frames that stay below them.
/// Such a thread is in no blocking region and no carrier runs it, so a
/// handshake would wait for it in vain. The deposit is held still by
/// `VirtualThreadManager::begin_debugger_read` for the read (a mount waits);
/// no local is read, so no mutator is needed. `None` when the thread is not
/// deposited (or another reader holds it).
#[cfg(feature = "experimental-debug")]
pub(crate) fn unmounted_virtual_thread_rows(
    shared: &SharedVm,
    tid: u64,
) -> Option<Vec<BlockedRow>> {
    let manager = &shared.threads.virtual_thread_manager;
    if !manager.is_deposited(tid) {
        return None;
    }
    // As the JDWP path does: the continuation rows name `VirtualThread`,
    // which HotSpot loads at boot and nothing loads on this VM.
    if shared.debug.debugger_gates.virtual_continuation_frames() {
        crate::vm::vm_exec::preload_virtual_thread_class(shared);
    }
    let addr = manager.begin_debugger_read(tid)?;
    // SAFETY: `begin_debugger_read` keeps the box deposited and unmounted
    // until `end_debugger_read` below: nothing runs or moves these frames.
    let thread = unsafe { &*addr };
    let rows = unmounted_rows(shared, thread, tid);
    manager.end_debugger_read(tid);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[UNMOUNTED_VT_ROWS] tid={tid} rows={}",
            rows.as_ref().map_or(0, Vec::len)
        );
    }
    rows
}

/// Run `work` on UNMOUNTED virtual thread `tid`'s deposited frames with the
/// listing [`unmounted_virtual_thread_rows`] answers, held still by
/// `VirtualThreadManager::begin_debugger_read` for the call (a mount waits)
/// -- [`with_blocked_frames`] for a continuation that yielded in a sleep or a
/// park, which is in no blocking region and which no carrier runs, so
/// neither a window nor a handshake reaches it (interpreter round i1 wave 51,
/// lane L1; `interpreter-L1-the-c-jvmti-table-reads-no-locals-of-an-unmounted-virtual-thread-FIXED-20261015`).
/// The frames keep their yield-time addresses until the remount applies the
/// blocked-window fold (gci i11/vt finding 3), so a reference local is read
/// through the yield deposit's `slot_origins`
/// ([`BlockedFrames::current_reference`]), as a blocked thread's; they are not
/// written ([`BlockedFrames::unmounted`]). `None` when the thread is not
/// deposited, another reader holds it, or it lists nothing. `work` runs with
/// no lock held.
#[cfg(feature = "experimental-debug")]
pub(crate) fn with_unmounted_frames<R>(
    shared: &SharedVm,
    tid: u64,
    work: impl FnOnce(&BlockedFrames<'_>) -> R,
) -> Option<R> {
    let manager = &shared.threads.virtual_thread_manager;
    if !manager.is_deposited(tid) {
        return None;
    }
    if shared.debug.debugger_gates.virtual_continuation_frames() {
        crate::vm::vm_exec::preload_virtual_thread_class(shared);
    }
    let addr = manager.begin_debugger_read(tid)?;
    // SAFETY: `begin_debugger_read` keeps the box deposited and unmounted
    // until `end_debugger_read` below: nothing runs or moves these frames.
    let thread = unsafe { &*addr };
    let answer = unmounted_rows_tagged(shared, thread, tid).map(|(rows, level_locals)| {
        let held = BlockedFrames {
            thread: addr.cast_mut(),
            // gci i11/vt finding 3: the yield deposit's state, which every
            // fold of the parked window advanced (`slot_origins[*].cur`).
            state: Some(&*thread.gc_block_state),
            unmounted: true,
            rows,
            // Wave 132 (lane L1, i119-L1 residual): the head levels' tags.
            level_locals,
        };
        work(&held)
    });
    manager.end_debugger_read(tid);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[UNMOUNTED_VT_WORK] tid={tid} ran={}", answer.is_some());
    }
    answer
}

/// [`unmounted_virtual_thread_rows`] with the deposit held: the listing
/// [`publish_unmounted_frames`] builds, without publishing it.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn unmounted_rows(shared: &SharedVm, thread: &JvmThread, tid: u64) -> Option<Vec<BlockedRow>> {
    unmounted_rows_tagged(shared, thread, tid).map(|(rows, _)| rows)
}

/// [`unmounted_rows`] with the [`StandinLevelLocals`] tags of the listing
/// head's levels by row position (top first: `has_native + k` for level `k`,
/// where `publish_frame_snapshot` places them for JDWP). Interpreter round i6
/// wave 132, lane L1 (i119-L1 residual): what the C table's
/// `GetLocalInstance` of a bound virtual thread's `VirtualThread` row reads
/// (`BlockedFrames::level_locals_at`). The tags are `Copy` and name no
/// reference.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn unmounted_rows_tagged(
    shared: &SharedVm,
    thread: &JvmThread,
    tid: u64,
) -> Option<(Vec<BlockedRow>, Vec<(usize, StandinLevelLocals)>)> {
    if thread.thread_id.0 != tid || thread.frames.is_empty() {
        return None;
    }
    let compiled = crate::threading::jvm_thread::BlockedCompiledView::default();
    let continuation = if shared.debug.debugger_gates.virtual_continuation_frames() {
        continuation_head_of_unmounted(shared, thread)
    } else {
        None
    };
    let all: &[Frame] = &thread.frames;
    let (frames, native_top) = match (jdk_continuation_kept(shared, thread), continuation) {
        // Wave 102 (lane L1): headed by `Object.wait0` when it waits there.
        (Some(kept), _) => (&all[..kept], monitor_wait_native_head(shared, thread, &all[..kept])),
        (None, Some((kept, head))) => (&all[..kept], Some(head)),
        (None, None) => (all, blocked_native_top(shared, all, &compiled, 0)),
    };
    let top_pc = blocked_top_pc(frames.last()?);
    let splices = debugger_listing_splices(shared, thread, native_top.as_ref(), true);
    let listing = build_frame_listing(frames, top_pc, native_top.as_ref(), &compiled, &splices);
    let level_locals = native_top
        .as_ref()
        .map(|head| {
            let base = usize::from(head.has_native());
            head.levels()
                .iter()
                .enumerate()
                .filter(|(_, level)| level.locals != StandinLevelLocals::Unknown)
                .map(|(k, level)| (base + k, level.locals))
                .collect()
        })
        .unwrap_or_default();
    Some((listing_blocked_rows(listing), level_locals))
}

/// A [`build_frame_listing`] listing as the C JVMTI table's [`BlockedRow`]s.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn listing_blocked_rows(listing: Vec<ListingRow>) -> Vec<BlockedRow> {
    // One block deeper than it needs to be, so the arms keep their places.
    {
        listing
            .into_iter()
            .map(|row| match row {
                ListingRow::Head {
                    class_id,
                    method_id,
                } => BlockedRow {
                    class_id,
                    method_id,
                    location: -1,
                    frame: None,
                    runs_compiled: false,
                    chain: None,
                },
                // Wave 120 (lane L1): a compiled activation's chain index.
                ListingRow::Opaque {
                    class_id,
                    method_id,
                    location,
                    chain,
                }
                // Wave 121 (L2): a compiled activation with home-slot locals
                // is attributed by its chain index exactly as an opaque one.
                | ListingRow::CompiledHome {
                    class_id,
                    method_id,
                    location,
                    chain,
                    ..
                } => BlockedRow {
                    class_id,
                    method_id,
                    location,
                    frame: None,
                    runs_compiled: false,
                    chain,
                },
                ListingRow::Proxy {
                    class_id,
                    method_id,
                    location,
                    ..
                } => BlockedRow {
                    class_id,
                    method_id,
                    location,
                    frame: None,
                    runs_compiled: false,
                    chain: None,
                },
                ListingRow::Interp {
                    index,
                    class_id,
                    method_id,
                    location,
                    runs_compiled,
                } => BlockedRow {
                    class_id,
                    method_id,
                    location,
                    frame: Some(index),
                    runs_compiled,
                    chain: None,
                },
            })
            .collect()
    }
}

/// The native method a thread blocked in native code runs, as JDWP lists it
/// (interpreter round i1 wave 15): `(class id, JDWP method id)` of the
/// method the invoke at `top`'s `last_instr_pc` names, when that resolves —
/// without loading anything — to an `ACC_NATIVE` method no override can
/// replace (invoked by `invokestatic` / `invokespecial`, or static, private
/// or final, or of a final class). The dispatch loop sets `last_instr_pc` at
/// every instruction, so it is the invoke the native was entered from.
///
/// A virtual or interface call to an overridable method (wave 17) is named
/// from `running_native`, the native callback the thread runs
/// (`JvmThread::running_native`, recorded while a debugger is attached): the
/// one `ACC_NATIVE` instance method of that name and descriptor, declared by
/// the resolved class or by a loaded subtype of the symbolic owner, whose
/// registered callback it is. [`BLOCKED_NATIVE_MAX_SUBTYPES`] bounds the
/// subtype walk; past it only the resolved class is asked.
///
/// `None` otherwise, and the caller's frame stays the top: a registered
/// native standing in for a Java method (HotSpot would list that method's
/// Java frames, which this VM does not run), a virtual call with no record
/// or whose recorded callback no candidate (or more than one) registers — a
/// JNI-bound native among them —, a native reached from compiled code (the
/// top interpreter frame's invoke is the compiled method's), or a reference
/// that does not resolve.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn blocked_native_method(
    shared: &SharedVm,
    top: &Frame,
    running_native: usize,
) -> Option<(u64, u64)> {
    native_named_by_invoke(
        shared,
        top.class_id,
        &top.code,
        top.last_instr_pc,
        running_native,
        false,
    )
}

/// The stand-in head ([`jvmti_events::blocking_standin_head`]) of a thread
/// whose top interpreter frame is `top`, with no running-native record to
/// confirm it by (interpreter round i1 wave 47, lane L1): for the C JVMTI
/// table's listing of a thread parked at a stand-in's return, whose native
/// row the park recorded (`jvmti::native_env::listed_rows`), which confirms
/// it by that row. Takes the class-manager lock.
#[cfg(feature = "experimental-debug")]
pub(crate) fn standin_head_of_top(shared: &SharedVm, top: &Frame) -> Option<NativeHead> {
    jvmti_events::blocking_standin_head(shared, top.class_id, &top.code, top.last_instr_pc, None)
}

/// [`blocked_native_method`] as a listing's head: the native the top frame's
/// invoke names; or (interpreter round i1 wave 46, lane L1) a registered
/// native standing in for `Thread.sleep` / `Object.wait` -- since wave 47
/// also `LockSupport.park*` -- listed as HotSpot lists that method: its
/// native leaf (`sleepNanos0`, `wait0`, `Unsafe.park`) on top, then its JDK
/// Java levels ([`jvmti_events::blocking_standin_head`]).
/// The head of blocked thread `tid`'s listing: the native it blocks in
/// ([`blocked_native_top`]) or, failing that, the synchronized method it is
/// blocked entering ([`synchronized_entry_head`], interpreter round i2 wave
/// 54, lane L6). Takes the class-manager lock: call it before the
/// debug-state lock.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn blocked_listing_head(
    shared: &SharedVm,
    tid: crate::threading::ThreadId,
    frames: &[Frame],
    compiled: &crate::threading::jvm_thread::BlockedCompiledView,
    running_native: usize,
) -> Option<NativeHead> {
    blocked_native_top(shared, frames, compiled, running_native)
        .or_else(|| synchronized_entry_head(shared, tid, frames, compiled))
}

/// The synchronized method a thread blocked ENTERING it is listed in
/// (interpreter round i2 wave 54, lane L6): HotSpot builds the method's frame
/// before it locks the method's monitor, so a thread blocked there lists the
/// method on top, at bytecode index 0, above its caller at the invoke
/// (`L6W54JdiFramesOfASynchronizedMethodEntrant`). This VM takes the monitor
/// before it pushes the frame (`vm_exec::monitor_enter_synchronized_method`),
/// so the listing's top interpreter frame is the caller, at the invoke.
///
/// Named when the thread contends for a monitor (its JMX contention stands)
/// and the top frame's `last_instr_pc` is an invoke (a `monitorenter` is a
/// block's) that selects an `ACC_SYNCHRONIZED` method: the resolved method
/// for a fixed target, else the receiver's -- the contended object, which a
/// synchronized instance method locks -- found up its superclass chain. A
/// synchronized native is listed as a native (location -1), a Java method as
/// an opaque level at bytecode index 0 (its locals are not built yet: every
/// local command answers `OPAQUE_FRAME`). `None` for compiled code above the
/// top frame, any other shape, or a reference that does not resolve. Takes
/// the class-manager lock: call it before the debug-state lock.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn synchronized_entry_head(
    shared: &SharedVm,
    tid: crate::threading::ThreadId,
    frames: &[Frame],
    compiled: &crate::threading::jvm_thread::BlockedCompiledView,
) -> Option<NativeHead> {
    let top = frames.last()?;
    if compiled
        .rows
        .last()
        .is_some_and(|row| row.below as usize >= frames.len())
    {
        return None;
    }
    // Wave 55 (lane L6): with the frame pushed first, the top IS the method.
    if is_pending_synchronized_entry(shared, top) {
        return None;
    }
    let contended = shared.threads.thread_registry.jmx_lock_snapshot(tid)?.0?;
    let cm = shared.classes.class_manager.read();
    let (selected_class, selected_index) = gc_and_alloc::synchronized_callee_invoked_at(
        shared,
        &cm,
        top.class_id,
        &top.code,
        top.last_instr_pc,
        contended,
    )?;
    let method = cm.get_class(selected_class)?.methods.get(selected_index)?;
    let (name, descriptor) = (&*method.name, &*method.descriptor);
    let class_id = u64::from(selected_class.as_u32());
    let method_id = crate::debug::jdwp_method_id(name, descriptor);
    if method.is_native() {
        return Some(NativeHead::leaf(class_id, method_id));
    }
    let level = StandinLevel {
        class_id,
        method_id,
        method_index: u16::try_from(selected_index).ok()?,
        bci: 0,
        locals: StandinLevelLocals::Unknown,
    };
    let head = NativeHead::continuation(&[level]);
    if head.is_some() && crate::runtime::env_cache::frame_trace() {
        eprintln!("[SYNC_ENTRY_HEAD] tid={} method={name}{descriptor}", tid.0);
    }
    head
}

/// [`execute`]'s deferred synchronized entry (interpreter round i2 wave 56,
/// lane L6, stage 2 of `CRATONVM_SYNC_METHOD_FRAME_FIRST`): the method's
/// frame is the top frame, pushed with no monitor because the entry was
/// contended (`vm_exec::SyncEntry::Deferred`); block for the monitor with it
/// on top. The object is read again here rather than carried across the
/// push: the receiver is the frame's local 0 (scanned and remapped as every
/// local is), a static method's class mirror was created before the
/// contended attempt and is a map read.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn enter_deferred_synchronized_entry(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    is_static: bool,
) {
    let obj = if is_static {
        shared
            .classes
            .class_mirrors
            .read()
            .get(&class_id)
            .copied()
            .or_else(|| crate::vm::try_get_or_create_class_mirror(shared, class_id))
    } else {
        match thread.frames.last().map(|top| top.get_local(0)) {
            Some(Value::Object(Some(receiver))) => Some(receiver),
            _ => None,
        }
    };
    match obj {
        // Wave 134 (lane L1): the nested loop's entry frame, whose deferred
        // `MethodEntry` stays outside a callback point (i133-L8 shape 2).
        Some(obj) => crate::vm::enter_synchronized_method_in_pushed_entry_frame(shared, thread, obj),
        // Unreachable: the contended attempt held this very object a moment
        // ago. Logged, never silent: the body would run unlocked.
        None => tracing::error!(
            ?class_id,
            "deferred synchronized entry found no monitor object; the body runs unlocked"
        ),
    }
}

/// Is `top` the frame of a synchronized method pushed before its monitor
/// (`CRATONVM_SYNC_METHOD_FRAME_FIRST`, `vm_exec::SyncEntry::Deferred`;
/// interpreter round i2 wave 55, lane L6) whose thread is still blocked
/// entering it? Then the frame itself is the listing's top, at bytecode
/// index 0, and nothing is synthesized above it: its first instruction is
/// not running, even when it is an invoke (`static synchronized void f() {
/// g(); }`). Such a frame has not run (`pc` 0) and owns no monitor yet; any
/// other synchronized method's frame owns its `monitor_on_exit`. Takes the
/// class-manager lock only with the switch on.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn is_pending_synchronized_entry(shared: &SharedVm, top: &Frame) -> bool {
    if !shared.threads.monitors.sync_method_frame_first()
        || top.monitor_on_exit.is_some()
        || top.pc != 0
        || top.last_instr_pc != 0
    {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    cm.get_class(top.class_id).is_some_and(|class| {
        class.methods.iter().any(|m| {
            m.is_synchronized()
                && *m.name == *top.method_name()
                && *m.descriptor == *top.method_descriptor()
        })
    })
}

#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn blocked_native_head(
    shared: &SharedVm,
    top: &Frame,
    running_native: usize,
) -> Option<NativeHead> {
    if is_pending_synchronized_entry(shared, top) {
        return None;
    }
    blocked_native_method(shared, top, running_native)
        .map(|(class_id, method_id)| NativeHead::leaf(class_id, method_id))
        .or_else(|| {
            jvmti_events::blocking_standin_head(
                shared,
                top.class_id,
                &top.code,
                top.last_instr_pc,
                (running_native != 0).then_some(running_native),
            )
        })
}

/// The native method a thread blocked under COMPILED code runs (interpreter
/// round i1 wave 22, lane L1): named from the invoke at the innermost
/// compiled activation's bytecode index — `row`, the last row of the view
/// its capture recorded ([`capture_blocked_compiled_view`]), when that row
/// stands above the top interpreter frame — and only when the thread's
/// running-native record (`JvmThread::running_native`) is the resolved
/// method's registered callback. The confirmation is required even for a
/// fixed target: the innermost activation's bytecode index comes from the
/// safepoint id it last published, not from the call it is in, so only the
/// callback says the invoke there is the one running. `None` without a
/// record, for an unknown bytecode index, or when the method's code is not
/// at hand, as before: no native frame is listed then.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn native_above_compiled_row(
    shared: &SharedVm,
    row: &crate::threading::jvm_thread::BlockedCompiledRow,
    running_native: usize,
) -> Option<(u64, u64)> {
    if running_native == 0 {
        return None;
    }
    let pc = usize::try_from(row.bci).ok()?;
    let caller_id = ClassId::new(row.class_id);
    // Copied out: the decode below takes the class-manager lock itself.
    let code: Vec<u8> = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(caller_id)?;
        let method = class
            .methods
            .iter()
            .find(|m| crate::debug::jdwp_method_id(&m.name, &m.descriptor) == row.method_id)?;
        method.code()?.code.to_vec()
    };
    native_named_by_invoke(shared, caller_id, &code, pc, running_native, true)
}

/// The native method heading a blocked thread's JDWP listing: named from the
/// top interpreter frame's invoke ([`blocked_native_method`]), or, when
/// compiled activations stand above that frame, from the innermost one's
/// ([`native_above_compiled_row`], wave 22). Takes the class-manager lock:
/// call it before the debug-state lock.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn blocked_native_top(
    shared: &SharedVm,
    frames: &[Frame],
    compiled: &crate::threading::jvm_thread::BlockedCompiledView,
    running_native: usize,
) -> Option<NativeHead> {
    match compiled.rows.last() {
        Some(row) if row.below as usize >= frames.len() => {
            native_above_compiled_row(shared, row, running_native)
                .map(|(class_id, method_id)| NativeHead::leaf(class_id, method_id))
                .or_else(|| standin_head_above_compiled_row(shared, row, running_native))
        }
        _ => blocked_native_head(shared, frames.last()?, running_native),
    }
}

/// [`jvmti_events::blocking_standin_head`] for the invoke at the innermost
/// compiled activation's bytecode index (interpreter round i1 wave 47, lane
/// L1): a compiled caller of `Thread.sleep` / `Object.wait` /
/// `LockSupport.park*` -- a polling loop the JIT compiled, OSR'd or not --
/// blocked in the method's stand-in lists the stand-in's chain above it, as
/// an interpreted caller does. Confirmed by `running_native`, as
/// [`native_above_compiled_row`] confirms a native (the activation's bytecode
/// index is the safepoint it last published, not necessarily the call in
/// progress); `None` without a record.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime) fn standin_head_above_compiled_row(
    shared: &SharedVm,
    row: &crate::threading::jvm_thread::BlockedCompiledRow,
    running_native: usize,
) -> Option<NativeHead> {
    if running_native == 0 {
        return None;
    }
    let pc = usize::try_from(row.bci).ok()?;
    let caller_id = ClassId::new(row.class_id);
    let code: Vec<u8> = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(caller_id)?;
        let method = class
            .methods
            .iter()
            .find(|m| crate::debug::jdwp_method_id(&m.name, &m.descriptor) == row.method_id)?;
        method.code()?.code.to_vec()
    };
    jvmti_events::blocking_standin_head(shared, caller_id, &code, pc, Some(running_native))
}

/// [`blocked_native_method`]'s decode of the invoke at `pc` of `code`, a
/// method of class `caller_id`. With `confirm_by_callback` a fixed target is
/// named only when its registered callback is `running_native`, as a virtual
/// one always is.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn native_named_by_invoke(
    shared: &SharedVm,
    caller_id: ClassId,
    code: &[u8],
    pc: usize,
    running_native: usize,
    confirm_by_callback: bool,
) -> Option<(u64, u64)> {
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
    let cm = shared.classes.class_manager.read();
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
        .declared_method(&cm, resolver.scope(owner), name, descriptor)
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
    if fixed {
        if !method.is_native() {
            return None;
        }
        let named = (
            u64::from(declaring.as_u32()),
            crate::debug::jdwp_method_id(name, descriptor),
        );
        if !confirm_by_callback {
            return Some(named);
        }
        let (class_name, name, descriptor) = (
            Arc::clone(&class.name),
            name.to_owned(),
            descriptor.to_owned(),
        );
        drop(cm);
        // Wave 27: the registered callback, or the intrinsic trampoline an
        // `Intrinsic` inline-cache entry runs in its place.
        let runs = callback_runs_method(shared, &class_name, &name, &descriptor, running_native);
        return runs.then_some(named);
    }
    if running_native == 0 {
        return None;
    }
    // The candidates, collected under the class-manager lock and matched
    // against the native registry after it is released.
    let subtypes = cm
        .class_store
        .subtypes_of(owner, BLOCKED_NATIVE_MAX_SUBTYPES)
        .unwrap_or_default();
    let mut candidates: Vec<(ClassId, Arc<str>)> = Vec::new();
    for cid in std::iter::once(declaring).chain(subtypes) {
        let Some(c) = cm.get_class(cid) else {
            continue;
        };
        let declares_native = c
            .find_method(name, descriptor)
            .is_some_and(|m| m.is_native() && !m.is_static());
        if declares_native && !candidates.iter().any(|&(id, _)| id == cid) {
            candidates.push((cid, Arc::clone(&c.name)));
        }
    }
    let (name, descriptor) = (name.to_owned(), descriptor.to_owned());
    drop(cm);
    let mut selected = None;
    for (cid, class_name) in candidates {
        let runs = callback_runs_method(shared, &class_name, &name, &descriptor, running_native);
        if runs {
            if selected.is_some() {
                return None;
            }
            selected = Some(cid);
        }
    }
    selected.map(|cid| {
        (
            u64::from(cid.as_u32()),
            crate::debug::jdwp_method_id(&name, &descriptor),
        )
    })
}

/// The Java method with bytecode that the invoke at `pc` of `code` (a method
/// of class `caller_id`) calls and a reviewed `NativeKind::Intrinsic`
/// registration answers through `running_native`, as `(class id, JDWP method
/// id, last return index)` (interpreter round i6 wave 120, lane L1;
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// `CRATONVM_JDWP_INTRINSIC_SHADOW_METHOD_EVENTS`). A fixed target only
/// (`invokestatic` / `invokespecial`, or a static, private or final method,
/// or one of a final class: `Class.getName`), as
/// [`native_named_by_invoke`] names one; `None` for a native method (that
/// one names it), a method with no code or no return, another kind of
/// registration (a `Bridge` stand-in or a stub is not reviewed), or a
/// callback that does not run it.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn shadow_named_by_invoke(
    shared: &SharedVm,
    caller_id: ClassId,
    code: &[u8],
    pc: usize,
    running_native: usize,
) -> Option<(u64, u64, u64)> {
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    const INVOKEVIRTUAL: u8 = 0xb6;
    const INVOKESPECIAL: u8 = 0xb7;
    const INVOKESTATIC: u8 = 0xb8;
    const INVOKEINTERFACE: u8 = 0xb9;
    if running_native == 0 {
        return None;
    }
    let opcode = *code.get(pc)?;
    if !matches!(
        opcode,
        INVOKEVIRTUAL | INVOKESPECIAL | INVOKESTATIC | INVOKEINTERFACE
    ) {
        return None;
    }
    let cp_index = u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]);
    let cm = shared.classes.class_manager.read();
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
    let owner = cm.find_class_by_name_for_class(owner_name, caller_id)?;
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let found = resolver
        .declared_method(&cm, resolver.scope(owner), name, descriptor)
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
    if !fixed || method.is_native() {
        return None;
    }
    // Wave 134 (lane D, i40; user decision 2026-10-19, option (b)): a served
    // `Intrinsic` runs no bytecode, so the return that ran is unknowable
    // (wave 121). With several returns the exit index could differ from
    // HotSpot's; post nothing rather than a wrong index.
    if shadow_has_several_returns(&method.code()?.code) {
        return None;
    }
    let exit = jvmti_events::last_return_index(&method.code()?.code)?;
    let (class_name, name, descriptor) = (
        Arc::clone(&class.name),
        name.to_owned(),
        descriptor.to_owned(),
    );
    drop(cm);
    // Interpreter round i6 wave 126, lane L1 (i40): HotSpot's template
    // interpreter serves `Math.abs(D)D`, `Math.sqrt(D)D` and the other
    // `java_lang_math_*` kinds from a math entry that runs no bytecode and
    // posts no method event, whatever an agent or a debugger asks
    // (`L1W27JdiStandInMethodEvents`). `run_stood_in_java_method` already
    // leaves them to the native for that reason; the frame-only report must
    // not post for them either.
    if jvmti_events::hotspot_serves_from_a_math_entry(&class_name, &name, &descriptor) {
        return None;
    }
    // Interpreter round i6 wave 130, lane L1a (i40): and the `Float` float16
    // conversions, served from HotSpot's `java_lang_Float_*` entries the same
    // way (measured: `L1aW130JdiFloat16ShadowEvents`, no events on HotSpot).
    if jvmti_events::hotspot_serves_from_a_float16_entry(&class_name, &name, &descriptor) {
        return None;
    }
    let reviewed = matches!(
        shared
            .natives
            .native_methods
            .find_with_kind(&class_name, &name, &descriptor),
        Some((_, cratonvm_native_api::NativeKind::Intrinsic))
    );
    if !reviewed || !callback_runs_method(shared, &class_name, &name, &descriptor, running_native) {
        return None;
    }
    Some((
        u64::from(declaring.as_u32()),
        crate::debug::jdwp_method_id(&name, &descriptor),
        exit,
    ))
}

/// Does `code` hold more than one return instruction (`ireturn` .. `return`,
/// 0xac..=0xb1), walked instruction by instruction as
/// `jvmti_events::last_return_index` walks it? The frame-only report of a
/// reviewed-`Intrinsic` shadow declines such a method (interpreter round i6
/// wave 134, lane D; i40, user decision 2026-10-19): it cannot know which
/// return ran, and HotSpot reports that one.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn shadow_has_several_returns(code: &[u8]) -> bool {
    let mut pc = 0usize;
    let mut returns = 0usize;
    while pc < code.len() {
        if (0xac..=0xb1).contains(&code[pc]) {
            returns += 1;
            if returns > 1 {
                return true;
            }
        }
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    false
}

/// The native method the invoke at the top interpreter frame of `thread`
/// calls, confirmed by `callback` (its registered callback or the
/// interpreter intrinsic trampoline serving it), as `(declaring class, name,
/// descriptor)`: what the C JVMTI table's `NativeMethodBind` of a registered
/// native names when no registration does (interpreter round i3 wave 83,
/// lane L1; `jvmti::native_env::post_rust_native_method_bind`). `None` for a
/// compiled caller or an invoke naming no native the callback runs.
#[cfg(feature = "experimental-debug")]
pub(crate) fn native_named_at_top_invoke(
    shared: &SharedVm,
    thread: &JvmThread,
    callback: usize,
) -> Option<(ClassId, String, String)> {
    let top = thread.frames.last()?;
    let (class_id, method_id) = native_named_by_invoke(
        shared,
        top.class_id,
        &top.code,
        top.last_instr_pc,
        callback,
        true,
    )?;
    let declaring = ClassId::new(u32::try_from(class_id).ok()?);
    let cm = shared.classes.class_manager.read_recursive();
    let method = cm
        .get_class(declaring)?
        .methods
        .iter()
        .find(|m| crate::debug::jdwp_method_id(&m.name, &m.descriptor) == method_id)?;
    Some((
        declaring,
        method.name.to_string(),
        method.descriptor.to_string(),
    ))
}

/// How many loaded subtypes of a virtual call's symbolic owner
/// [`blocked_native_method`] asks before it gives up on the walk (a JDWP
/// command on a suspended thread, not a hot path; `Object` and the widely
/// implemented JDK interfaces exceed it at once).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) const BLOCKED_NATIVE_MAX_SUBTYPES: usize = 256;

/// Does the native-call funnel's callback `callback` stand in for a JDK
/// `native` method -- HotSpot runs such a method's C code in
/// `_thread_in_native` -- rather than for a Java method a registered native
/// shadows (an `Intrinsic`, or a `--compatible` stand-in), whose body HotSpot
/// runs as Java (interpreter round i2 wave 61, lane L6;
/// `docs/feature-designs/jvmti-delivery-threading.md`)?
/// True when some triple registered with the callback names a `native`
/// method of a class the built-in loaders have loaded. An interpreter
/// intrinsic's trampoline (`Object.hashCode`, `System.arraycopy`: HotSpot
/// runs those in the VM, not in native) is in no registration, so it answers
/// false. A scan of the native registry and one class lookup per match: for
/// the C JVMTI table's `GetThreadState` of another thread, never a hot path.
///
/// Since interpreter round i2 wave 62 (lane L6, `CRATONVM_HANDSHAKE_NATIVE_CALLBACK`)
/// in every build: a frameless JMX `ThreadInfo` asks it of the callback the
/// target's native-excuse word was raised for, instead of capturing the
/// target's stack to read its top row (`vm_exec`'s `thread_jmx_snapshot`).
pub(crate) fn funnel_callback_runs_a_native_method(shared: &SharedVm, callback: usize) -> bool {
    if callback == 0 {
        return false;
    }
    // The i53 retirement: a JNI native's C code (`vm_exec::enter_jni_native`)
    // is a `native` method's by construction.
    if callback == C_NATIVE_CALLBACK {
        return true;
    }
    let triples = shared.natives.native_methods.triples_running(callback);
    if triples.is_empty() {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    triples.into_iter().any(|(class_name, name, descriptor)| {
        cm.find_class_by_name_for_loader(class_name, cratonvm_types::ClassLoaderId::Application)
            .and_then(|cid| cm.get_class(cid))
            .and_then(|class| class.find_method(name, descriptor).map(|m| m.is_native()))
            .unwrap_or(false)
    })
}

/// The callback a JNI native's C call is recorded under -- the thread's
/// `JvmThread::running_native` and its native-excuse word's callback -- by
/// the JNI arms of the invoke (`vm_exec::enter_jni_native`; interpreter
/// round i2, the i53 retirement), and by the C JVMTI table's in-native mark
/// (`jvmti::native_env::C_IN_NATIVE_RUNNING`, wave 58). No registered
/// native's callback has this address, so no head is named by it; the
/// native's own row (`JvmThread::jni_native_frames`) lists it.
pub(crate) const C_NATIVE_CALLBACK: usize = 1;

/// Record `callback` as the native this thread is about to run, while a
/// debugger is attached (interpreter round i1 wave 17): the native-call
/// funnel (`vm_exec::safe_native_call_impl`) calls it around the callback
/// and hands the answer to [`leave_running_native`]. One load with no
/// debugger; always `None` without `experimental-debug`.
///
/// Interpreter round i2 wave 53, lane L6 (`CRATONVM_DEBUG_SUSPEND_NATIVE_ACK`,
/// `DebuggerGates::suspend_native_ack`): also raises
/// `GcBlockState::debugger_native_ack`, the thread's acknowledgement of a
/// JDWP suspension while it runs the native (`debug::await_suspension_settled`).
///
/// Interpreter round i2 wave 61, lane L6 (`CRATONVM_JVMTI_C_RUNNING_NATIVE`):
/// also while a C JVMTI env is bound, when it records only the callback, in
/// `GcBlockState::funnel_native_callback`, for the C table's `GetThreadState`
/// (`jvmti::native_env::thread_in_native`). Still one load
/// (`DebuggerGates::running_native_armed`, which folds both in).
#[inline(always)]
pub(crate) fn enter_running_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: usize,
) -> Option<RunningNativePrior> {
    #[cfg(feature = "experimental-debug")]
    if shared.debug.debugger_gates.running_native_armed() {
        return Some(enter_running_native_armed(shared, thread, callback));
    }
    let _ = (shared, thread, callback);
    None
}

/// What [`enter_running_native`] replaced: the recorded callback, and the
/// acknowledgement word as it stood (wave 53).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) struct RunningNativePrior {
    pub(in crate::runtime::interpreter) callback: usize,
    pub(in crate::runtime::interpreter) ack: bool,
    /// This level opened the native frames window (wave 56,
    /// [`open_native_frames_window`]): its leave closes it.
    pub(in crate::runtime::interpreter) window: bool,
    /// `GcBlockState::funnel_native_callback` as it stood (wave 61).
    pub(in crate::runtime::interpreter) funnel: usize,
}

/// [`enter_running_native`] while it records (wave 61): the callback for
/// another thread's in-native question, then the debugger's record when a
/// debugger is attached.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn enter_running_native_armed(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: usize,
) -> RunningNativePrior {
    use std::sync::atomic::Ordering;
    let cell = &thread.gc_block_state.funnel_native_callback;
    let funnel = cell.load(Ordering::Relaxed);
    cell.store(callback, Ordering::Release);
    if shared.debug.debugger_gates.session_attached() {
        let mut prior = enter_running_native_attached(shared, thread, callback);
        prior.funnel = funnel;
        return prior;
    }
    // A C JVMTI env only: the debugger's record is left as it stands.
    RunningNativePrior {
        callback: thread.running_native,
        ack: thread
            .gc_block_state
            .debugger_native_ack
            .load(Ordering::Acquire),
        window: false,
        funnel,
    }
}

/// [`enter_running_native`] with a debugger attached.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn enter_running_native_attached(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: usize,
) -> RunningNativePrior {
    let acknowledges = shared.debug.debugger_gates.suspend_native_ack();
    let ack = if acknowledges {
        thread
            .gc_block_state
            .debugger_native_ack
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    } else {
        false
    };
    // Wave 56: a thread that acknowledges a suspension in the native lists
    // its frames through a window while the native runs.
    let window = acknowledges && open_native_frames_window(shared, thread, callback);
    RunningNativePrior {
        callback: std::mem::replace(&mut thread.running_native, callback),
        ack,
        window,
        funnel: 0,
    }
}

/// [`leave_running_native`] for the native-call funnel
/// (`vm_exec::safe_native_call_impl`), which also closes the native frames
/// window the matching [`enter_running_native`] opened, withdrawing a listing
/// published through it (interpreter round i2 wave 56, lane L6). One test of
/// the prior record otherwise.
#[inline(always)]
pub(crate) fn leave_running_native_in(
    shared: &SharedVm,
    thread: &mut JvmThread,
    prior: Option<RunningNativePrior>,
) {
    #[cfg(feature = "experimental-debug")]
    if prior.is_some_and(|p| p.window) {
        close_native_frames_window(shared, thread);
    }
    let _ = shared;
    leave_running_native(thread, prior);
}

// `GcBlockState::debugger_native_window` (interpreter round i2 wave 56, lane
// L6, `CRATONVM_DEBUG_NATIVE_FRAMES_WINDOW`;
// `docs/feature-designs/real-jdk-mode.md`):
// the JDWP frames window of a thread that runs a native outside a blocking
// region, a word of its own (a blocking region inside the native has its
// own `debugger_inspect` window, which this one never clobbers).

/// The window is open: a debugger command may read the frames below the
/// native.
#[cfg(feature = "experimental-debug")]
pub(crate) const NW_OPEN: usize = 1;
/// A debugger command is reading them; a relocation or the leave waits.
#[cfg(feature = "experimental-debug")]
pub(crate) const NW_READING: usize = 2;
/// A listing was published through the window; the leave withdraws it.
#[cfg(feature = "experimental-debug")]
pub(crate) const NW_PUBLISHED: usize = 4;
/// The window was opened under compiled code: the activations below the
/// native are in `GcBlockState::debugger_native_compiled` (wave 57, lane L6).
#[cfg(feature = "experimental-debug")]
pub(crate) const NW_COMPILED: usize = 8;
/// With [`NW_COMPILED`]: the window carries a copy of the thread's JIT entry
/// chain (`GcBlockState::debugger_native_chain`), from which the reader walks
/// the compiled activations, instead of a recorded view (interpreter round i2
/// wave 58, lane L6, `CRATONVM_DEBUG_NATIVE_WINDOW_CHAIN`).
#[cfg(feature = "experimental-debug")]
pub(crate) const NW_CHAIN: usize = 16;
/// JNI natives' rows or reflective calls stand below the native: the thread
/// recorded them as it opened the window (`GcBlockState::native_rows`), and
/// the reader splices them (interpreter round i2, the i53 retirement).
#[cfg(feature = "experimental-debug")]
pub(crate) const NW_ROWS: usize = 32;

/// Open the native frames window as `thread` enters the native `callback`
/// (wave 56; answers whether it opened one). The frames below the native do
/// not move or change while it runs: an upcall pushes ABOVE them (and shuts
/// the window first, [`NestedJavaAck`]), the stack's one relocation shuts it
/// (`FrameStack::grow_for`), and a collection that moved objects the thread
/// has not remapped yet makes the reader refuse (wave 57: one can complete
/// only while the thread is blocked inside the native, and its wake
/// remaps). Not opened while an outer native's window stands or a reader
/// still reads it. JNI natives and reflective calls on the stack are
/// recorded for the reader (the i53 retirement, [`NW_ROWS`]; before, they
/// refused it, and so did a virtual thread).
/// Under compiled code the thread records the compiled view first (wave 57).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn open_native_frames_window(shared: &SharedVm, thread: &mut JvmThread, callback: usize) -> bool {
    use std::sync::atomic::Ordering;
    if !shared.debug.debugger_gates.native_frames_window() {
        return false;
    }
    let state = &thread.gc_block_state;
    let w = state.debugger_native_window.load(Ordering::Acquire);
    // The i53 retirement: a virtual thread no longer refuses it. Its
    // continuation is frozen (its frames moved out of `thread.frames`) only
    // after a yielding native has RETURNED -- the native answers a yield, the
    // interpreter unwinds it -- past this window's leave; and Java the native
    // runs pins the continuation (`vt_pinned_by_native`), so no yield
    // happens under it either.
    if w & (NW_OPEN | NW_READING) != 0 || thread.frames.is_empty() {
        return false;
    }
    // The i53 retirement: JNI natives' rows and reflective calls no longer
    // refuse it (wave 62 lifted the refusal for a C agent's lambda proxy
    // rows only). The thread records them now (a copy, made only when they
    // changed since its last record: `NativeRowsSnapshot`), and the reader
    // splices from the record as `debugger_listing_splices` splices from the
    // thread's own lists -- which a JNI native or a reflective call the
    // native's Java makes may push to, and reallocate, under a reader.
    let rows = if !thread.jni_native_frames.is_empty() || !thread.reflective_calls.is_empty() {
        crate::vm::vm_exec::record_native_rows(thread);
        NW_ROWS
    } else {
        0
    };
    // Wave 57 (lane L6): under compiled code, the thread records the
    // compiled activations below the native now -- only it can walk its JIT
    // entry chain -- as a blocked thread does for its window
    // (`open_blocked_inspection`). They stay as recorded while the native
    // runs. Only while a debugger is attached; one thread-local read when no
    // compiled code is active.
    //
    // Wave 58 (lane L6, `CRATONVM_DEBUG_NATIVE_WINDOW_CHAIN`): almost every
    // native returns before a debugger reads it, so the thread records only
    // what the walk needs from its thread-locals -- a copy of its JIT entry
    // chain and its stack pointer -- and the reader walks it
    // (`read_native_frames`). The compiled frames do not move while the
    // native runs, and the window keeps the thread in it while a reader reads.
    let compiled = if crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
        if shared.debug.debugger_gates.native_window_chain() {
            state.debugger_native_chain.lock().capture();
            NW_COMPILED | NW_CHAIN
        } else {
            *state.debugger_native_compiled.lock() = capture_blocked_compiled_view(&thread.frames);
            NW_COMPILED
        }
    } else {
        0
    };
    // Cast: the frames' address, for the reader.
    let base = thread.frames.as_ptr() as usize;
    state.debugger_native_frames.store(base, Ordering::Relaxed);
    state
        .debugger_native_depth
        .store(thread.frames.len(), Ordering::Relaxed);
    state
        .debugger_native_callback
        .store(callback, Ordering::Relaxed);
    // Current now: the thread runs, and remapped its frames at its last
    // arrival or wake.
    state
        .debugger_native_frames_gen
        .store(shared.mem.gc_barrier.moving_generation(), Ordering::Relaxed);
    // Cast: the word's address, for `FrameStack::grow_for`.
    let word = &state.debugger_native_window as *const std::sync::atomic::AtomicUsize as usize;
    thread.frames.set_native_window(word);
    // Release: the record above is visible to a reader that sees the window
    // open. A listing an earlier window published stays marked for the leave.
    thread.gc_block_state.debugger_native_window.store(
        NW_OPEN | compiled | rows | (w & NW_PUBLISHED),
        Ordering::Release,
    );
    true
}

/// Close the native frames window for good (the native's leave), waiting
/// out a reader, and withdraw a listing published through it (wave 56).
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn close_native_frames_window(shared: &SharedVm, thread: &mut JvmThread) {
    use std::sync::atomic::Ordering;
    let word = &thread.gc_block_state.debugger_native_window;
    let closed = loop {
        let w = word.load(Ordering::Acquire);
        if w & NW_READING != 0 {
            // The reader runs on a mutator between two polls and waits for
            // nothing this thread holds: it finishes.
            std::thread::yield_now();
            continue;
        }
        if word
            .compare_exchange_weak(w, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break w;
        }
    };
    thread.frames.set_native_window(0);
    if closed & NW_PUBLISHED == 0 {
        return;
    }
    let tid = thread.thread_id.0;
    let mut ds = shared.debug.debug_state.lock();
    for id in withdraw_debugger_frames(&mut ds, tid) {
        ds.objects.unpin(id);
    }
    crate::debug::release_disposed_objects(shared, &mut ds);
}

/// Shut the native frames window at `word` -- no new reader -- once no
/// reader holds it, keeping its published mark for the leave (wave 56):
/// the frame stack's relocation (`FrameStack::grow_for`), which frees the
/// memory a reader reads. Nothing when it is not open.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn shut_native_frames_window_word(word: &std::sync::atomic::AtomicUsize) {
    use std::sync::atomic::Ordering;
    loop {
        let w = word.load(Ordering::Acquire);
        if w & NW_READING != 0 {
            std::thread::yield_now();
            continue;
        }
        if w & NW_OPEN == 0
            || word
                .compare_exchange_weak(w, w & !NW_OPEN, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return;
        }
    }
}

/// The thread has just remapped its own frames after the collections it
/// took part in (a safepoint's resume, a blocking region's wake): a native
/// frames window open over them may be read again, as of the current
/// moving generation (wave 56; wave 57, lane L6: the per-thread
/// frames-current word, `GcBlockState::debugger_native_frames_gen`). One
/// load while no window was opened.
#[cfg(feature = "experimental-debug")]
#[inline]
pub(crate) fn note_native_frames_current(shared: &SharedVm, thread: &JvmThread) {
    use std::sync::atomic::Ordering;
    let state = &thread.gc_block_state;
    if state.debugger_native_window.load(Ordering::Relaxed) & NW_OPEN != 0 {
        state
            .debugger_native_frames_gen
            .store(shared.mem.gc_barrier.moving_generation(), Ordering::Release);
    }
}

/// Can a debugger command read thread `tid`'s frames through its native
/// frames window now (wave 56)? The JDWP server's lock-free pre-check.
#[cfg(feature = "experimental-debug")]
pub(crate) fn native_frames_readable(shared: &SharedVm, tid: u64) -> bool {
    shared
        .threads
        .thread_registry
        .gc_block_state_of(crate::threading::jvm_thread::ThreadId(tid))
        .is_some_and(|state| {
            let w = state
                .debugger_native_window
                .load(std::sync::atomic::Ordering::Acquire);
            w & NW_OPEN != 0 && w & NW_READING == 0
        })
}

/// Publish the frames of thread `tid`, suspended while it runs a native
/// outside a blocking region, through its native frames window (interpreter
/// round i2 wave 56, lane L6): the native on top at location -1
/// ([`blocked_listing_head`], from the top frame's invoke and the recorded
/// callback), then the Java frames below it with their locals. HotSpot lists
/// such a thread at once; this VM listed it only after the native returned,
/// and `ThreadReference.Frames` answered no frame meanwhile
/// (`L6W56JdiSuspendInRunningNative`). Answers whether it published: not
/// when the window is shut or held, the thread is blocked now (its blocking
/// window serves it), a collection that moved objects completed since its
/// frames were last current (wave 57: `GcBarrier::moving_generation`
/// against the thread's frames-current word), the thread is no longer
/// suspended, or a listing of it already stands.
///
/// Must run on a registered mutator between two safepoint polls
/// (`debug::run_on_parked_thread`), so no collection completes while the
/// frames' references are exported. `CRATONVM_FRAME_TRACE=1` prints
/// `[NATIVE_FRAMES_WINDOW] tid=<n> published=<bool> depth=<n>`.
#[cfg(feature = "experimental-debug")]
pub(crate) fn publish_frames_of_native_thread(shared: &SharedVm, tid: u64) -> bool {
    publish_native_frames(shared, tid, false)
}

/// [`publish_frames_of_native_thread`] while a stop-the-world pause is
/// requested (interpreter round i2, the i53 retirement), run by the JDWP
/// server thread itself, which no pause counts. Such a pause waits for the
/// thread in the native (a collection never excuses a running native) and
/// holds every registered mutator meanwhile, so the mutator hop
/// (`debug::run_on_parked_thread`) waited until the native returned and the
/// thread listed itself at the return, its caller on top -- where HotSpot,
/// whose collector does not wait for a thread in native, lists the native at
/// once (`L6I53JdiRunningNativeShapes gc`). Sound because the read cannot race
/// a move of the objects the frames name: the window holds the thread in
/// its native (its leave waits out READING) and so out of the pause's
/// arrivals, and the frames' references are exported under the barrier
/// transition lock (`GcBarrier::with_transition_lock`), which no pause gets
/// past its wait for counted threads without -- after the lock is taken the
/// reader checks again that the thread is not blocked (a blocked thread is
/// no longer waited for) and that no moving collection completed since its
/// frames were current. The debug-state lock is only tried under it, and the
/// class-manager lock is taken before it. Answers whether it published;
/// `false` leaves the caller to the mutator hop.
#[cfg(feature = "experimental-debug")]
pub(crate) fn publish_frames_of_native_thread_in_stop(shared: &SharedVm, tid: u64) -> bool {
    publish_native_frames(shared, tid, true)
}

/// [`publish_frames_of_native_thread`]; `in_stop` as
/// [`publish_frames_of_native_thread_in_stop`].
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn publish_native_frames(shared: &SharedVm, tid: u64, in_stop: bool) -> bool {
    use std::sync::atomic::Ordering;
    let registry = &shared.threads.thread_registry;
    let thread_id = crate::threading::jvm_thread::ThreadId(tid);
    let Some(state) = registry.gc_block_state_of(thread_id) else {
        return false;
    };
    let word = &state.debugger_native_window;
    let w = word.load(Ordering::Acquire);
    if w & NW_OPEN == 0 || w & NW_READING != 0 {
        return false;
    }
    if word
        .compare_exchange(w, w | NW_READING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    let depth = state.debugger_native_depth.load(Ordering::Relaxed);
    let blocked = registry.is_blocked(thread_id);
    let collected = native_frames_stale(shared, &state);
    let published = !blocked
        && !collected
        && read_native_frames(
            shared,
            &state,
            tid,
            w & NW_COMPILED != 0,
            w & NW_CHAIN != 0,
            w & NW_ROWS != 0,
            in_stop,
        );
    // One update: the leave may shut or close the word meanwhile only after
    // READING is gone, and it must see PUBLISHED then.
    let _ = word.try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
        Some((cur & !NW_READING) | if published { NW_PUBLISHED } else { 0 })
    });
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[NATIVE_FRAMES_WINDOW] tid={tid} published={published} depth={depth} \
             blocked={blocked} collected={collected} compiled={} chain={} rows={} direct={in_stop}",
            w & NW_COMPILED != 0,
            w & NW_CHAIN != 0,
            w & NW_ROWS != 0
        );
    }
    published
}

/// May the frames below a native frames window hold references a moving
/// collection left stale (interpreter round i2 wave 57, lane L6)? Only after
/// a collection that MOVED objects which the thread has not remapped since
/// (its frames-current word, `GcBlockState::debugger_native_frames_gen`,
/// lags `GcBarrier::moving_generation`). A pause that moved nothing (a
/// loop-exit, frame-trace or concurrent-mark pause) leaves them current;
/// wave 56 compared `gc_generation`, which counts those pauses too, and
/// refused every read (`L6W56JdiSuspendInRunningNative`).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn native_frames_stale(
    shared: &SharedVm,
    state: &crate::threading::jvm_thread::GcBlockState,
) -> bool {
    state
        .debugger_native_frames_gen
        .load(std::sync::atomic::Ordering::Acquire)
        != shared.mem.gc_barrier.moving_generation()
}

/// [`publish_frames_of_native_thread`] once the window is held.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn read_native_frames(
    shared: &SharedVm,
    state: &crate::threading::jvm_thread::GcBlockState,
    tid: u64,
    under_compiled: bool,
    from_chain: bool,
    with_rows: bool,
    in_stop: bool,
) -> bool {
    use std::sync::atomic::Ordering;
    let base = state.debugger_native_frames.load(Ordering::Relaxed) as *const Frame;
    let depth = state.debugger_native_depth.load(Ordering::Relaxed);
    let callback = state.debugger_native_callback.load(Ordering::Relaxed);
    if base.is_null() || depth == 0 {
        return false;
    }
    // SAFETY: `base[..depth]` are the live frames below the native, recorded
    // by the thread as it opened the window (Release, read after our Acquire
    // of the open word). They stay in place and unchanged while READING is
    // set: the buffer is relocated only by `FrameStack::grow_for`, which
    // waits out READING first; the thread's pushes and pops while in the
    // native (an upcall's) are above `depth`; and the leave, after which the
    // caller frame runs on, waits out READING too. Their references are
    // current: no collection that moved objects completed since the thread
    // last made them current (checked by the caller, wave 57) and none
    // completes while this mutator reads.
    let frames: &[Frame] = unsafe { std::slice::from_raw_parts(base, depth) };
    let Some(top) = frames.last() else {
        return false;
    };
    let top_pc = blocked_top_pc(top);
    let thread_id = crate::threading::jvm_thread::ThreadId(tid);
    // Wave 57 (lane L6): the compiled activations the thread recorded as it
    // opened the window under compiled code; written only at an opening,
    // which waits for no reader but cannot happen while READING is set.
    let compiled = if under_compiled && from_chain {
        // Wave 58 (lane L6): walked now from the chain the thread copied as
        // it opened the window. SAFETY: the thread is inside the native it
        // entered after the copy and stays in it while READING is set (its
        // leave waits for this reader); Java the native runs (which shuts the
        // window but cannot end this read) runs below the native, so the
        // compiled frames above it, and the bodies they run, are as copied.
        let jit = unsafe { state.debugger_native_chain.lock().active_compiled_frames() };
        // Interpreter round i6 wave 121, lane L1 (stage 4 of
        // `i18-L3-proposal-jdwp-lists-the-compiled-activations-of-a-blocked-thread`,
        // under L2's `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`, default off):
        // each activation's home-slot locals, read now, while the window is
        // held, within the activation's own band (`FrameWordReader::HeldThread`).
        // Its reference words are current for the same reason the frames'
        // are (the caller checked that no moving collection completed since
        // the thread opened the window, `native_frames_stale`, and none
        // completes while this mutator reads; under `in_stop` the check is
        // made again under the transition lock before the publish), and the
        // publish below makes them ids before the window is let go.
        let homes = true
            .then_some(crate::jit::conservative_roots::FrameWordReader::HeldThread);
        compiled_view_of_with(frames, jit, homes)
    } else if under_compiled {
        state.debugger_native_compiled.lock().clone()
    } else {
        crate::threading::jvm_thread::BlockedCompiledView::default()
    };
    // Before the debug-state lock: it takes the class-manager lock.
    let native_top = blocked_listing_head(shared, thread_id, frames, &compiled, callback);
    // The i53 retirement: the JNI natives and reflective calls below the
    // native, from the record the thread made as it opened the window
    // (`NW_ROWS`), copied out under its lock before the class-manager lock
    // names them. A native the listed one calls directly (no Java between)
    // records its own rows too; those below the listed native are the same.
    let splices = if with_rows {
        let (natives, calls, named, served) = {
            let rows = state.native_rows.lock();
            (
                rows.jni_rows.clone(),
                rows.reflective_calls.clone(),
                rows.reflective_named.clone(),
                rows.served.call_depths().unwrap_or_default().to_vec(),
            )
        };
        listing_splices_of(
            shared,
            tid,
            depth,
            &natives,
            &calls,
            &named,
            native_top.as_ref(),
            true,
            None,
            &served,
        )
    } else {
        Vec::new()
    };
    let publish = |ds: &mut crate::debug::DebugState| {
        if !ds.is_thread_suspended(tid) || ds.has_current_frames(tid) {
            return false;
        }
        publish_frame_snapshot(
            shared,
            frames,
            ds,
            tid,
            top_pc,
            native_top,
            under_compiled.then_some(&compiled),
            &splices,
            |_, _, v| v,
        );
        true
    };
    if !in_stop {
        return publish(&mut shared.debug.debug_state.lock());
    }
    // The i53 retirement (`publish_frames_of_native_thread_in_stop`): the
    // references are exported under the barrier transition lock, after the
    // checks the caller made are made again there.
    shared.mem.gc_barrier.with_transition_lock(|| {
        if state.in_blocked_region.load(Ordering::Acquire) || native_frames_stale(shared, state) {
            return false;
        }
        match shared.debug.debug_state.try_lock() {
            Some(mut ds) => publish(&mut ds),
            None => false,
        }
    })
}

/// Restore the record [`enter_running_native`] replaced (natives nest
/// through upcalls). Nothing when it recorded nothing.
#[inline(always)]
pub(crate) fn leave_running_native(thread: &mut JvmThread, prior: Option<RunningNativePrior>) {
    if let Some(prior) = prior {
        thread.running_native = prior.callback;
        // Wave 53: back to what the caller had (lowered for a native called
        // from Java). A plain store: written only by this thread.
        thread
            .gc_block_state
            .debugger_native_ack
            .store(prior.ack, std::sync::atomic::Ordering::Release);
        // Wave 61: the outer native's callback again (0 under none).
        thread
            .gc_block_state
            .funnel_native_callback
            .store(prior.funnel, std::sync::atomic::Ordering::Release);
    }
}

/// Java the native the thread runs executes (an upcall: `execute`,
/// `execute_prebuilt_frame`, `run_pushed_frame_to_completion`) lowers the
/// thread's acknowledgement of a JDWP suspension for as long as it runs, and
/// puts it back after (interpreter round i2 wave 53, lane L6;
/// `CRATONVM_DEBUG_SUSPEND_NATIVE_ACK`): the native's Java is no longer "in
/// native", and parks at its suspend points. One load of the thread's own
/// field (`JvmThread::running_native`, non-zero only under a native while a
/// debugger is attached) on each of those entries; the guard holds nothing
/// otherwise.
///
/// Interpreter round i2 wave 58, lane L1: the same for the C JVMTI table's
/// in-native mark (`GcBlockState::jvmti_in_native`, raised by
/// `jvmti::native_env::CInNativeMark` around a JNI native's C code, which
/// records a non-zero `running_native` for this guard to see).
///
/// Interpreter round i2 wave 58, lane L6 (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE`):
/// the same entries lower the thread's `GcBlockState::native_excuse` word for
/// as long as the Java runs (waiting while a handshake pause holds the
/// excusal: an excused thread runs no Java), and raise it again after. The
/// fast path tests `running_native` and `JvmThread::native_excuse_raised` in
/// one branch.
pub(crate) struct NestedJavaAck {
    pub(in crate::runtime::interpreter) state: Option<Arc<crate::threading::jvm_thread::GcBlockState>>,
    /// The debugger acknowledgement was lowered: raise it again.
    pub(in crate::runtime::interpreter) ack: bool,
    /// The C JVMTI in-native mark was lowered (wave 58, lane L1): raise it again.
    pub(in crate::runtime::interpreter) in_native: bool,
    /// The native-excuse word that was lowered (0: none): raise it again.
    pub(in crate::runtime::interpreter) excuse: usize,
}

impl NestedJavaAck {
    #[inline(always)]
    pub(crate) fn enter(thread: &JvmThread) -> Self {
        if (thread.running_native | thread.native_excuse_raised) == 0 {
            return Self {
                state: None,
                ack: false,
                in_native: false,
                excuse: 0,
            };
        }
        Self::enter_under_native(thread)
    }

    #[cold]
    #[inline(never)]
    pub(in crate::runtime::interpreter) fn enter_under_native(thread: &JvmThread) -> Self {
        let state = &thread.gc_block_state;
        // Wave 58 (lane L6): the native-excuse word, lowered while the Java
        // runs. Read rather than taken from the plain copy: inside an
        // upcall the copy still names the outer native while the word is 0.
        let excuse = if thread.native_excuse_raised != 0 {
            let raised = state.native_excuse.load() & !crate::threading::jvm_thread::NX_EXCUSED;
            if raised != 0 {
                state.native_excuse.lower(raised);
            }
            raised
        } else {
            0
        };
        let ack = thread.running_native != 0 && Self::lower_debugger_ack(state);
        // Interpreter round i2 wave 58, lane L1: the C JVMTI table's
        // in-native mark (`GcBlockState::jvmti_in_native`): the native's Java
        // is not in native either. Never raised outside `experimental-debug`.
        let in_native = thread.running_native != 0
            && state
                .jvmti_in_native
                .load(std::sync::atomic::Ordering::Relaxed)
            && state
                .jvmti_in_native
                .swap(false, std::sync::atomic::Ordering::AcqRel);
        if !ack && !in_native && excuse == 0 {
            return Self {
                state: None,
                ack: false,
                in_native: false,
                excuse: 0,
            };
        }
        Self {
            state: Some(Arc::clone(state)),
            ack,
            in_native,
            excuse,
        }
    }

    /// The wave-53/56 half: shut the native frames window and lower the
    /// debugger acknowledgement; whether it was raised.
    pub(in crate::runtime::interpreter) fn lower_debugger_ack(state: &crate::threading::jvm_thread::GcBlockState) -> bool {
        // Wave 56 (lane L6): the native's frames window lists no Java the
        // native runs, so no new reader. One already reading reads frames
        // below the ones this Java pushes, which stay put (a relocation waits
        // for it, `FrameStack::grow_for`); the native's leave withdraws what
        // it published, and a suspend point of this Java publishes anew.
        #[cfg(feature = "experimental-debug")]
        if state
            .debugger_native_window
            .load(std::sync::atomic::Ordering::Relaxed)
            & NW_OPEN
            != 0
        {
            state
                .debugger_native_window
                .fetch_and(!NW_OPEN, std::sync::atomic::Ordering::AcqRel);
        }
        state
            .debugger_native_ack
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }
}

impl Drop for NestedJavaAck {
    #[inline(always)]
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            if self.ack {
                state
                    .debugger_native_ack
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            if self.in_native {
                state
                    .jvmti_in_native
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            // Wave 58 (lane L6): every native this Java ran lowered its own
            // word, so the word is 0 here and no pause can hold it.
            if self.excuse != 0 {
                state.native_excuse.raise(self.excuse);
            }
        }
    }
}

/// Is `so` a slot origin a debugger read of a blocked (or unmounted) thread's
/// reference LOCAL goes through? A live one always. A dead one -- the frame's
/// per-bci liveness says the slot is never read again, so it is no root and
/// its referent may have died -- only with `dead_too`
/// ([`blocked_dead_slots_read`]: `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS`, default
/// off since wave 128, interpreter round i6 wave 126, lane L1, i47-L1; or,
/// since wave 130, `CRATONVM_DEBUG_BLOCKED_ROOTED_DEAD_LOCALS` in a VM where
/// every local is a root). Every fold advances a dead slot's `cur` as it
/// advances a live one's, so a referent that survived (held elsewhere) is
/// followed exactly; the callers then answer `cur` only while it is still an
/// object start, the screen `debug::screen_dead_local` applies to a running
/// thread's dead local. HotSpot's interpreter keeps every local and answers
/// it: `LockSupport.park(Object)`'s `blocker`, dead at its `Unsafe.park`
/// call, read `null` here (`L1W101JdiCompatibleParkFrames`, both modes).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn blocked_slot_origin_read(
    so: &crate::threading::jvm_thread::SlotOrigin,
    dead_too: bool,
) -> bool {
    !so.is_stack && (so.live || dead_too)
}

/// The `dead_too` of [`blocked_slot_origin_read`] for this VM: may a debugger
/// read of a blocked (or unmounted) thread answer a liveness-dead reference
/// local through its slot origin?
///
/// * `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS` (wave 126, default off since the
///   wave-128 review): always, screened by the object-start test only.
/// * `CRATONVM_DEBUG_BLOCKED_ROOTED_DEAD_LOCALS` (interpreter round i6 wave
///   130, lane L1a; HotSpot parity, default on, `=0` off; i47-L1): only where
///   every local is a GC root, `jvmti::debugger_observes_locals` (a JDWP port
///   given at start or, since wave 132, a startup C agent holding
///   `can_access_local_variables`). There the referent of a dead slot is rooted by every
///   root scan and blocking deposit (`Frame::scan_local_objects_for_vm`), so
///   it cannot die while the frame holds it, and every fold advances `cur`
///   through the pointer map exactly as a live slot's: the wave-128 hazard
///   (a dead referent's stale `cur` meeting a new object) needs a dead
///   referent. HotSpot keeps every local alive under
///   `can_access_local_variables` and answers it:
///   `LockSupport.park(Object)`'s `blocker` (`L1W101JdiCompatibleParkFrames`).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn blocked_dead_slots_read(shared: &SharedVm) -> bool {
    let gates = &shared.debug.debugger_gates;
    gates.blocked_dead_locals()
        || (gates.blocked_rooted_dead_locals()
            && crate::runtime::jvmti::debugger_observes_locals(shared))
}

/// [`publish_frames_of_blocked_thread`] once the window is held: `addr` is
/// the blocked thread's `JvmThread`.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn read_blocked_frames(
    shared: &SharedVm,
    state: &crate::threading::jvm_thread::GcBlockState,
    addr: usize,
    tid: u64,
) -> bool {
    // (frame index, slot) -> (address at the deposit, current address), for
    // the reference locals the deposit found live (and, wave 126, the dead
    // ones: [`blocked_slot_origin_read`]).
    // Wave 130 (lane L1a): or every local is a root ([`blocked_dead_slots_read`]).
    let dead_too = blocked_dead_slots_read(shared);
    let current: std::collections::HashMap<(usize, u16), (usize, usize)> = state
        .slot_origins
        .lock()
        .iter()
        .filter(|so| blocked_slot_origin_read(so, dead_too))
        .filter_map(|so| {
            let slot = u16::try_from(so.idx).ok()?;
            // Widening: a frame index
            Some(((so.frame as usize, slot), (so.orig, so.cur)))
        })
        .collect();
    // SAFETY: `addr` is the `JvmThread` the blocked thread named when it
    // opened the window. It stays alive and in place until the thread closes
    // the window, which waits while READING is set (our caller set it), and
    // the thread does not write its frames while it is blocked, so these
    // reads race no write.
    let thread = unsafe { &*(addr as *const JvmThread) };
    if thread.thread_id.0 != tid {
        return false;
    }
    let frames: &[Frame] = &thread.frames;
    let Some(top) = frames.last() else {
        return false;
    };
    let top_pc = blocked_top_pc(top);
    // The compiled activations the thread recorded as it blocked (wave 21);
    // its leave clears them only after this reader is done.
    let compiled = state.debugger_compiled.lock().clone();
    // Before the debug-state lock: it takes the class-manager lock.
    let native_top = blocked_listing_head(
        shared,
        thread.thread_id,
        frames,
        &compiled,
        thread.running_native,
    );
    // Wave 47: the JNI natives and reflective calls it stands under (not
    // touched while it is blocked), named before the debug-state lock.
    let splices = debugger_listing_splices(shared, thread, native_top.as_ref(), true);
    let heap = &shared.mem.heap;
    let mut ds = shared.debug.debug_state.lock();
    // A listing of this suspension stands; one from before a resume the
    // thread stayed blocked through is replaced (wave 28: its ids carry the
    // old frame generation).
    if !ds.is_thread_suspended(tid) || ds.has_current_frames(tid) {
        return false;
    }
    // Wave 41 (lane L1): a `StackFrame.SetValues` deferred to this thread's
    // wake is what a later suspension's listing shows, as HotSpot's would.
    let deferred = crate::debug::inspect::deferred_frame_values(shared, &ds, tid, frames);
    publish_frame_snapshot(
        shared,
        frames,
        &mut ds,
        tid,
        top_pc,
        native_top,
        Some(&compiled),
        &splices,
        |index, slot, v| {
            if let Some(&written) = deferred.get(&(index, slot)) {
                return written;
            }
            match v {
                Value::Object(Some(held)) => {
                    let held = held.as_ptr() as usize; // Cast: the deposit's key
                    let now = current
                        .get(&(index, slot))
                        .filter(|&&(orig, _)| orig == held)
                        .map(|&(_, cur)| cur);
                    Value::Object(now.and_then(|cur| heap.is_object_address(cur)))
                }
                other => other,
            }
        },
    );
    true
}

// ---------------------------------------------------------------------------
// Dispatch loop: reference-typed operands and locals (wave 3, lane L1)
// ---------------------------------------------------------------------------
//
// A slot the BYTECODE says is a reference (`areturn`'s operand, `aastore`'s
// value and array, every array opcode's array, `aload`/`astore`'s local) is
// decoded here, not by the context-free `CompactValue::to_value`. That
// decoder answers `Value::Long(bits)` for an `Object`-tagged payload with no
// recorded `ObjectRef` provenance, and the `coerce_value_for_return_validated`
// every such site applied next turned that `long` into NULL — so the same
// reference was kept by `aload_3` and nulled by `aload 4`, `areturn`,
// `aastore`, or an array opcode's fallback, and the fallbacks' re-push marked
// the slot KIND_LONG, hiding the array from the GC root scan. See
// docs/feature-designs/real-jdk-mode.md.

/// The reference an operand-stack slot holds, when the slot is a plain
/// `Object`-tagged reference: no KIND_LONG / KIND_DOUBLE mark (the `Value`
/// path decodes a marked slot by its mark) and a payload that can be a heap
/// address. `None` for every other shape — null, a JNI `long` handle, a
/// marked slot — which the callers decode as before.
///
/// Building the `ObjectRef` records the payload's provenance, exactly as any
/// other reference-construction boundary does.
#[inline(always)]
pub(in crate::runtime::interpreter) fn ref_slot_object(cv: CompactValue, kind: u8) -> Option<ObjectRef> {
    if kind == crate::runtime::ValueStack::KIND_MARK_LONG
        || kind == crate::runtime::ValueStack::KIND_MARK_DOUBLE
    {
        return None;
    }
    ref_local_object(cv)
}

/// The decoded path's twin of [`ref_slot_object`] for the operand on top of
/// `stack`, read before the checked `pop()` that keeps the decoded arms'
/// underflow error. `None` for an empty stack.
#[inline]
pub(in crate::runtime::interpreter) fn plain_ref_on_top(stack: &crate::runtime::ValueStack) -> Option<ObjectRef> {
    stack
        .peek_with_kind()
        .ok()
        .and_then(|(cv, kind)| ref_slot_object(cv, kind))
}

/// [`ref_slot_object`] for a local-variable slot, which carries no operand
/// stack kind mark: `aload`'s tag test, as `load_ref_local` applies it.
#[inline(always)]
pub(in crate::runtime::interpreter) fn ref_local_object(cv: CompactValue) -> Option<ObjectRef> {
    let p = cv.as_object_ptr()?;
    if !cratonvm_types::plausible_heap_pointer(p) {
        return None;
    }
    // SAFETY: `plausible_heap_pointer` proved `p` non-null and 8-byte
    // aligned (`from_raw`'s preconditions). The slot is reference-typed by the
    // bytecode — the same trust `aload_<n>` / `astore_<n>` place in it when they
    // move it verbatim.
    Some(unsafe { ObjectRef::from_raw(p as *mut u8) }) // Cast: payload is a heap address
}

/// Decode an operand-stack slot exactly as `ValueStack::pop_unchecked` does:
/// by its KIND_LONG / KIND_DOUBLE mark first, else by its tag.
#[inline(always)]
pub(in crate::runtime::interpreter) fn stack_slot_value(cv: CompactValue, kind: u8) -> Value {
    if kind == crate::runtime::ValueStack::KIND_MARK_LONG {
        Value::Long(cv.as_long_unchecked())
    } else if kind == crate::runtime::ValueStack::KIND_MARK_DOUBLE {
        Value::Double(f64::from_bits(cv.to_bits())) // Cast: raw f64 bits
    } else {
        cv.to_value()
    }
}

/// A reference-typed operand slot as a `Value`: a plain reference passes
/// through as that reference ([`ref_slot_object`]); every other shape takes
/// the historical `pop_unchecked` decode plus the validated `L` coerce (the
/// int-zero and JNI `long`-handle shapes become null or a heap-checked
/// object, as before).
#[inline(always)]
pub(in crate::runtime::interpreter) fn ref_operand_value(shared: &SharedVm, cv: CompactValue, kind: u8) -> Value {
    match ref_slot_object(cv, kind) {
        Some(r) => Value::Object(Some(r)),
        None => ref_operand_value_coerced(shared, cv, kind),
    }
}

/// [`ref_operand_value`]'s rare shapes (an int-tagged zero, a JNI `long`
/// handle, a cat-2 mark): the historical decode plus the validated `L`
/// coerce, which probes the heap for a `long` handle.
///
/// Out of line since interpreter round i1 wave 27 (lane L7), as the cold
/// halves of [`load_ref_local`] and [`store_ref_local`] are: the three are
/// `inline(always)` into the dispatch loop's reference arms (every `aload`,
/// `astore`, `areturn`, ...), and the coerce's heap validation used to be
/// inlined with them at every site, for a shape the verifier's typing almost
/// never lets reach them.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn ref_operand_value_coerced(shared: &SharedVm, cv: CompactValue, kind: u8) -> Value {
    coerce_value_for_return_validated(shared, stack_slot_value(cv, kind), b'L')
}

/// `aload <n>` / `aload_<n>`: push reference local `idx`. One implementation
/// for every spelling, so the local number javac assigned cannot change the
/// answer.
///
/// An `Object`- or `Null`-tagged local is pushed verbatim (the common case: a
/// tag test and a copy). Every other shape — an int-tagged zero (the "native
/// returned 0 for null" shape), a JNI `long` handle — takes the validated `L`
/// coerce, which is what `aload n` and the decoded `Instruction::Aload` arm
/// have always done. Until wave 3 the four short forms pushed an int-tagged
/// zero as an int, which `ifnull` (`ref_operand_is_null`) then called
/// non-null (it calls it null since wave 5).
///
/// The caller has proved `idx < max_locals`: an explicit operand by its arm's
/// guard, a short form by the method's admission (`frame_fast_path_admitted`).
#[inline(always)]
pub(in crate::runtime::interpreter) fn load_ref_local(shared: &SharedVm, frame: &mut Frame, idx: usize) {
    let cv = frame.get_local_compact_unchecked(idx);
    if matches!(cv.tag(), CompactTag::Object | CompactTag::Null) {
        frame.stack.push_compact(cv);
    } else {
        load_ref_local_coerced(shared, frame, idx);
    }
}

/// [`load_ref_local`] for a local that is not a plain reference: the
/// validated `L` coerce. Out of line since wave 27 (see
/// [`ref_operand_value_coerced`]).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn load_ref_local_coerced(shared: &SharedVm, frame: &mut Frame, idx: usize) {
    let v = coerce_value_for_return_validated(shared, frame.get_local_unchecked(idx), b'L');
    frame.stack.push_unchecked(v);
}

/// `astore <n>` / `astore_<n>`: pop into reference local `idx`, one
/// implementation for every spelling (see [`load_ref_local`]).
///
/// A plain `Object`/`Null`-tagged slot with no KIND_LONG / KIND_DOUBLE mark is
/// stored verbatim. Every other shape is decoded as `pop_unchecked` would
/// decode it and stored through the validated `L` coerce — which the decoded
/// `Instruction::Astore` arm and `astore n` already did, and which the short
/// forms did only for a `Long`-tagged slot (they ignored the kind mark and
/// stored an int-tagged zero as an int).
///
/// A `returnAddress` (`jsr`'s operand, legal for `astore`) passes the coerce
/// unchanged.
///
/// The caller has proved `idx < max_locals`, as for [`load_ref_local`].
#[inline(always)]
pub(in crate::runtime::interpreter) fn store_ref_local(shared: &SharedVm, frame: &mut Frame, idx: usize) {
    let (cv, kind) = frame.stack.pop_with_kind_unchecked();
    if matches!(cv.tag(), CompactTag::Object | CompactTag::Null)
        && kind != crate::runtime::ValueStack::KIND_MARK_LONG
        && kind != crate::runtime::ValueStack::KIND_MARK_DOUBLE
    {
        frame.set_local_compact_unchecked(idx, cv);
    } else {
        store_ref_local_coerced(shared, frame, idx, cv, kind);
    }
}

/// [`store_ref_local`] for a popped slot that is not a plain reference: the
/// validated `L` coerce. Out of line since wave 27 (see
/// [`ref_operand_value_coerced`]).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn store_ref_local_coerced(
    shared: &SharedVm,
    frame: &mut Frame,
    idx: usize,
    cv: CompactValue,
    kind: u8,
) {
    let v = coerce_value_for_return_validated(shared, stack_slot_value(cv, kind), b'L');
    frame.set_local_unchecked(idx, v);
}

// ---------------------------------------------------------------------------
// Dispatch loop: per-method fast-path admission, superinstruction helpers
// ---------------------------------------------------------------------------

/// Slots in [`FAST_PATH_MEMO`]; a power of two.
pub(in crate::runtime::interpreter) const FAST_PATH_MEMO_SLOTS: usize = 16_384;
/// [`FAST_PATH_MEMO`] entry bit: the slot holds a computed verdict.
pub(in crate::runtime::interpreter) const FAST_PATH_MEMO_FILLED: u64 = 1;
/// [`FAST_PATH_MEMO`] entry bit: the verdict is "vetoed" (decoded path).
pub(in crate::runtime::interpreter) const FAST_PATH_MEMO_VETOED: u64 = 2;
/// [`FAST_PATH_MEMO`] entry bit (wave 7): the method's operand stack is
/// proven to stay within `max_stack` — by the verifier's maps, or for a
/// method with none by `cratonvm_reader::quickened::fast_path_reach`'s depth
/// pass. See [`frame_stack_depth_proven`].
pub(in crate::runtime::interpreter) const FAST_PATH_MEMO_STACK_PROVEN: u64 = 4;
/// Every flag bit of a [`FAST_PATH_MEMO`] entry; the key sits above them.
pub(in crate::runtime::interpreter) const FAST_PATH_MEMO_FLAGS: u64 =
    FAST_PATH_MEMO_FILLED | FAST_PATH_MEMO_VETOED | FAST_PATH_MEMO_STACK_PROVEN;

/// Direct-mapped, lock-free memo of [`frame_fast_path_admitted`], keyed by
/// `(bytecode allocation, declaring class, VM)`.
///
/// The VM is part of the key because the allocation is not per VM:
/// `padded_bytecode_for_method` interns padded code process-wide by
/// `(class id, name, descriptor, body, exception table)`, and a `ClassId` is a per-VM index, so
/// two VMs running the same body under the same id share one allocation while
/// their verifier maps (per VM, `TypeMapStore`) can disagree — `max_stack` is
/// not part of the body.
///
/// The dispatch loop asks at every frame switch — each interpreted call and
/// return — and the unmemoized answer costs a `(name, descriptor)` hash and a
/// scan of the class's method table, so it is computed once per method and
/// read back with one relaxed load. Each entry is a single word, so no reader
/// can see a torn entry. It is process-wide for the same reason the quickened
/// intern table is: the bytecode allocation it is keyed on is.
///
/// A stale entry (the allocation freed and reused by another method's code of
/// the same class id, or two keys colliding) can only hand back the other
/// method's verdict: a false "vetoed" costs the decoded path's speed, a false
/// "admitted" is the behaviour every method had before this gate existed —
/// the fast path without the proof, where only the explicit-index local arms
/// still check their operand ("H7"). Reuse needs the keyed code allocation to
/// be dropped first (class unloading or redefinition, a padded-code cache
/// eviction in `padded_bytecode_for_method`).
pub(in crate::runtime::interpreter) static FAST_PATH_MEMO: [std::sync::atomic::AtomicU64; FAST_PATH_MEMO_SLOTS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; FAST_PATH_MEMO_SLOTS];

/// May `frame`'s method run on the raw-bytecode fast path?
///
/// The fast-path arms use the `*_unchecked` operand-stack and local accessors,
/// whose contract is the verifier's proof that the stack stays within
/// `max_stack` and every local index is `< max_locals`. That proof is recorded
/// per method as `MethodTypeMaps::safe_for_fast_path`, and withheld — with a
/// `FastPathVeto` naming why — for exactly the methods it does not cover.
/// Among them are the methods of user-defined-loader classes whose Pass-3
/// verdict is deferred (every Spring / Tomcat / H2 / Elasticsearch
/// application class): they get only the structural check, which proves
/// local indices but not stack depth, so an under-declared `max_stack` used
/// to overrun a `Vec` inside an unchecked push and panic.
///
/// * Maps present and vetoed: `false`. The method runs on the bounds-checked
///   decoded path, which reports an overrun as a catchable error instead.
/// * Maps present and proven: `true`.
/// * No maps at all (never verified, CDS, a per-class skip, a synthetic
///   frame): `true` only when every local access in the code lies inside the
///   frame's locals ([`unproven_code_fits`], wave 6). The short forms
///   (`iload_0..3`, `lstore_3`, ...) carry no per-execution guard, so without
///   this a malformed `iload_3` in a one-slot frame panicked on a `Vec`
///   index. The explicit-index arms keep their per-arm `max_locals` guards
///   ("H7") as a second line behind a stale memo entry (see
///   [`FAST_PATH_MEMO`]). Stack depth is proven for this population where
///   the same walk can prove it (wave 7, [`frame_stack_depth_proven`]), but
///   no arm relies on that yet.
///
/// `CRATONVM_QUICKEN_STATS=1` prints one line per vetoed method with its
/// `FastPathVeto` (or `unproven-locals`), and one per admitted map-less
/// method whose stack depth is unproven (`stack-unproven`): the census that
/// sizes the population this gate moves.
#[inline]
pub(in crate::runtime::interpreter) fn frame_fast_path_admitted(shared: &SharedVm, frame: &Frame) -> bool {
    fast_path_memo_flags(shared, frame) & FAST_PATH_MEMO_VETOED == 0
}

/// Is `frame`'s operand stack proven to stay within its `max_stack` on
/// every path (wave 7)? True for an admitted method with verifier maps (the
/// verifier proves it) and for a method with none whose code
/// `cratonvm_reader::quickened::fast_path_reach` proves. This is the
/// precondition of Stage 2 of
/// `interpreter-i1-L1-proposal-dispatch-loop-register-state-CLOSED-20261224.md` (raw-pointer
/// stack access); nothing reads it on the dispatch path yet — the answer and
/// its memo bit are landed first, so the census
/// (`CRATONVM_QUICKEN_STATS=1`, "stack-unproven" lines) can size the
/// population before any arm relies on it.
#[allow(dead_code)] // The Stage 2 stack arms are its reader.
pub(in crate::runtime::interpreter) fn frame_stack_depth_proven(shared: &SharedVm, frame: &Frame) -> bool {
    let flags = fast_path_memo_flags(shared, frame);
    flags & FAST_PATH_MEMO_VETOED == 0 && flags & FAST_PATH_MEMO_STACK_PROVEN != 0
}

/// The [`FAST_PATH_MEMO`] flags of `frame`'s method, computed on a miss.
#[inline]
pub(in crate::runtime::interpreter) fn fast_path_memo_flags(shared: &SharedVm, frame: &Frame) -> u64 {
    use std::sync::atomic::Ordering;
    let (key, slot) = fast_path_memo_slot(shared, frame);
    let entry = slot.load(Ordering::Relaxed);
    if entry & FAST_PATH_MEMO_FILLED != 0 && entry & !FAST_PATH_MEMO_FLAGS == key {
        return entry & FAST_PATH_MEMO_FLAGS;
    }
    store_fast_path_verdict(shared, frame, key, slot)
}

/// `frame`'s [`FAST_PATH_MEMO`] key and slot.
#[inline]
pub(in crate::runtime::interpreter) fn fast_path_memo_slot(
    shared: &SharedVm,
    frame: &Frame,
) -> (u64, &'static std::sync::atomic::AtomicU64) {
    // Cast: allocation address as a memo key, never dereferenced.
    let ptr = frame.code.as_ptr() as usize as u64;
    let cid = u64::from(frame.class_id.as_u32());
    // Cast: a small process-unique counter, mixed into the key's high bits.
    let vm = shared.vm_identity as u64;
    // The three low bits stay clear for the entry's flags (a heap address
    // loses nothing but always-zero high bits to the shift).
    let key = (ptr << 3) ^ (cid << 40) ^ (vm << 52);
    // Cast: masked into the table's index range.
    let slot_idx = ((ptr >> 4) ^ (ptr >> 18) ^ cid) as usize & (FAST_PATH_MEMO_SLOTS - 1);
    (key, &FAST_PATH_MEMO[slot_idx])
}

/// Compute `frame`'s verdict and store it under `key` in `slot`.
#[cold]
pub(in crate::runtime::interpreter) fn store_fast_path_verdict(
    shared: &SharedVm,
    frame: &Frame,
    key: u64,
    slot: &std::sync::atomic::AtomicU64,
) -> u64 {
    let verdict = fast_path_admitted_uncached(shared, frame);
    let mut flags = FAST_PATH_MEMO_FILLED;
    if !verdict.admitted {
        flags |= FAST_PATH_MEMO_VETOED;
    }
    if verdict.stack_proven {
        flags |= FAST_PATH_MEMO_STACK_PROVEN;
    }
    slot.store(key | flags, std::sync::atomic::Ordering::Relaxed);
    flags
}

/// Recompute and store the [`FAST_PATH_MEMO`] verdict of a frame that was
/// just moved onto a fresh code allocation, or restamped as running a
/// replaced body (`obsolete_frames`, interpreter round i1 wave 21, lane L4).
/// The memo is keyed by the allocation's address: an entry a freed
/// allocation of the same class left at that address would otherwise answer
/// for the moved frame -- "admitted" for a body the fast path must not run
/// (a widened `ldc`).
pub(in crate::runtime) fn refresh_fast_path_verdict(shared: &SharedVm, frame: &Frame) {
    let (key, slot) = fast_path_memo_slot(shared, frame);
    let _ = store_fast_path_verdict(shared, frame, key, slot);
}

/// What [`fast_path_admitted_uncached`] decides for one method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::runtime::interpreter) struct FastPathVerdict {
    /// May the method run on the raw-bytecode fast path?
    pub(in crate::runtime::interpreter) admitted: bool,
    /// Is its operand-stack depth proven within `max_stack`?
    pub(in crate::runtime::interpreter) stack_proven: bool,
}

/// [`frame_fast_path_admitted`] without the memo.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn fast_path_admitted_uncached(shared: &SharedVm, frame: &Frame) -> FastPathVerdict {
    // A frame running a body its class's redefinition replaced (JEP 109): the
    // verifier maps filed under its method's name describe the NEW body, not
    // its code (interpreter round i1 wave 21, lane L4).
    if frame.runs_obsolete_method() || frame.widened_stream().is_some() {
        return replaced_body_fast_path_verdict(shared, frame);
    }
    // THIS VM's store: a `ClassId` names another class in another VM (see
    // `cratonvm_classloading::TypeMapStore`).
    let Some(maps) = shared.classes.type_maps.type_maps_for_named(
        frame.class_id,
        frame.method_name(),
        frame.method_descriptor(),
    ) else {
        return unproven_code_fits(shared, frame);
    };
    if maps.safe_for_fast_path() {
        return FastPathVerdict {
            admitted: true,
            stack_proven: true,
        };
    }
    if cratonvm_reader::quickened::stats_enabled() {
        let n = cratonvm_reader::quickened::note_fast_path_veto();
        eprintln!(
            "[quicken] fast-path veto #{n}: {}.{}{} veto={:?} max_stack={} max_locals={}",
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor(),
            maps.fast_path_veto(),
            frame.max_stack,
            frame.max_locals,
        );
    }
    FastPathVerdict {
        admitted: false,
        stack_proven: false,
    }
}

/// The admission of a frame that runs a replaced body (JEP 109), whose code
/// the verifier maps of its method's current body do not describe.
///
/// * An `ldc` of it names a constant above index 255
///   (`Frame::widened_stream`): vetoed. The fast path reads the `ldc`
///   operand byte, which is stale there; the decoded path reads the stream.
/// * Otherwise admitted only when [`unproven_code_fits`] proves both its
///   local reach and its stack depth from its own code: the fast path's
///   unchecked accessors must not rest on another body's proof, and a body
///   whose depth is unproven runs on the bounds-checked decoded path.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn replaced_body_fast_path_verdict(shared: &SharedVm, frame: &Frame) -> FastPathVerdict {
    if frame.widened_stream().is_some() {
        return FastPathVerdict {
            admitted: false,
            stack_proven: false,
        };
    }
    let own = unproven_code_fits(shared, frame);
    FastPathVerdict {
        admitted: own.admitted && own.stack_proven,
        stack_proven: own.stack_proven,
    }
}

/// The admission of a method with no verifier maps (wave 6): does every local
/// access its code can execute lie inside `frame`'s locals? And (wave 7, the
/// same walk) is its operand-stack depth proven within `max_stack`?
///
/// `cratonvm_reader::quickened::fast_path_reach` bounds the slots the code
/// touches, and refuses (`None`) code whose control can leave its linear
/// instruction walk (a branch or handler into an operand byte, `ret`, an
/// undecodable tail) — there the fast path would dispatch bytes the bound
/// never saw. Either way the method runs on the decoded path, whose local
/// accessors are bounds-checked. Well-formed code — all of javac's, and every
/// JDK class a CDS archive carries — always fits, so the only methods this
/// moves are malformed ones. Runs once per method, behind the memo.
///
/// The stack proof reads member descriptors from the constant pool of
/// `frame.class_id`, the declaring class of every real frame, taken with
/// `try_read`: when the class is absent or the lock is busy (this thread or a
/// queued writer), an empty pool stands in, so the frame switch never blocks
/// and code naming a member stays unproven. It moves no admission: only
/// [`frame_stack_depth_proven`] reads it.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn unproven_code_fits(shared: &SharedVm, frame: &Frame) -> FastPathVerdict {
    let handlers = frame
        .exception_table()
        .iter()
        .map(|e| usize::from(e.handler_pc));
    // No class (a synthetic frame) or no lock: an empty pool, so code that
    // names no member is still proven and code that does is not.
    let empty = cratonvm_reader::constant_pool::ConstantPool::new(Vec::new());
    let reach = match shared.classes.class_manager.try_read() {
        Some(cm) => {
            let cp = cm
                .class_store
                .get(frame.class_id)
                .map_or(&empty, |class| &class.constant_pool);
            cratonvm_reader::quickened::fast_path_reach(&frame.code, handlers, Some(cp))
        }
        None => cratonvm_reader::quickened::fast_path_reach(&frame.code, handlers, Some(&empty)),
    };
    let fits = reach.is_some_and(|r| r.locals <= frame.locals_len());
    let stack_proven = fits
        && reach
            .and_then(|r| r.stack)
            .is_some_and(|words| words <= usize::from(frame.max_stack));
    if cratonvm_reader::quickened::stats_enabled() {
        if !fits {
            let n = cratonvm_reader::quickened::note_fast_path_veto();
            eprintln!(
                "[quicken] fast-path veto #{n}: {}.{}{} veto=unproven-locals reach={reach:?} \
                 max_locals={}",
                frame.class_name(),
                frame.method_name(),
                frame.method_descriptor(),
                frame.max_locals,
            );
        } else if !stack_proven {
            eprintln!(
                "[quicken] stack-unproven: {}.{}{} reach={reach:?} max_stack={}",
                frame.class_name(),
                frame.method_name(),
                frame.method_descriptor(),
                frame.max_stack,
            );
        }
    }
    FastPathVerdict {
        admitted: fits,
        stack_proven,
    }
}

// The bits of the dispatch loop's hoisted `static_ldc_gates` word
// (interpreter round i3 wave 77, lane L7): one per per-entry gate of the
// `ldc` / `getstatic` / `putstatic` inline arms and their fusions, which the
// loop carried as ten separate bools before (frame ratchet). Each bit is
// exactly the bool of the same name it replaces.
/// `fused_ldc_bound_on` (`CRATONVM_INTERP_FUSED_LDC_BOUND` and the
/// numeric-constant table's gate).
pub(in crate::runtime::interpreter) const SLG_FUSED_LDC_BOUND: u16 = 1 << 0;
/// `ldc_hoisted_pc` (`CRATONVM_INTERP_LDC_HOISTED_PC`).
pub(in crate::runtime::interpreter) const SLG_LDC_HOISTED_PC: u16 = 1 << 1;
/// `ldc_inline_hit_on` (`CRATONVM_INTERP_LDC_INLINE_HIT` and the table gate).
pub(in crate::runtime::interpreter) const SLG_LDC_INLINE_HIT: u16 = 1 << 2;
/// `fused_getstatic_bound_on` (`CRATONVM_INTERP_FUSED_GETSTATIC_BOUND`, kill
/// switch folded in).
pub(in crate::runtime::interpreter) const SLG_FUSED_GETSTATIC_BOUND: u16 = 1 << 3;
/// `ldc2w_inline_hit_on` (`CRATONVM_INTERP_LDC2W_INLINE_HIT` and the table
/// gate).
pub(in crate::runtime::interpreter) const SLG_LDC2W_INLINE_HIT: u16 = 1 << 4;
/// `getstatic_inline_on` (`CRATONVM_INTERP_GETSTATIC_INLINE_HIT`, kill switch
/// folded in).
pub(in crate::runtime::interpreter) const SLG_GETSTATIC_INLINE: u16 = 1 << 5;
/// `getstatic_inline_remap_trace` (the relocation-provenance trace while the
/// `getstatic` inline hit is on).
pub(in crate::runtime::interpreter) const SLG_GETSTATIC_REMAP_TRACE: u16 = 1 << 6;
/// `putstatic_inline_on` (`CRATONVM_INTERP_PUTSTATIC_INLINE_HIT`, kill switch
/// folded in).
pub(in crate::runtime::interpreter) const SLG_PUTSTATIC_INLINE: u16 = 1 << 7;
/// `fused_getstatic_ifzero_on` (`CRATONVM_INTERP_FUSED_GETSTATIC_IFZERO`,
/// kill switch folded in).
pub(in crate::runtime::interpreter) const SLG_FUSED_GETSTATIC_IFZERO: u16 = 1 << 8;
/// `fused_static_increment_on` (`CRATONVM_INTERP_FUSED_STATIC_INCREMENT` with
/// both inline hits).
pub(in crate::runtime::interpreter) const SLG_FUSED_STATIC_INCREMENT: u16 = 1 << 9;
/// `fused_increment_ldc2w_on` (`CRATONVM_INTERP_FUSED_INCREMENT_LDC2W` with
/// the `ldc2_w` inline hit; interpreter round i3 wave 83, lane L7). Above the
/// low byte on purpose: the `aload_N` arm ORs this very bit into the field
/// increment's mode (`field_fast::FIELD_INC_LDC2W` is this constant), whose
/// low byte is the hoisted `fused_field_increment_mode`.
pub(in crate::runtime::interpreter) const SLG_FUSED_INCREMENT_LDC2W: u16 = 1 << 10;
/// `fused_wide_local_getfield_on` (`CRATONVM_INTERP_FUSED_WIDE_LOCAL_GETFIELD`
/// with `fast_field_zgc`; interpreter round i3 wave 83, lane L7): the `aload
/// n` arm's `aload n; getfield` fusion.
pub(in crate::runtime::interpreter) const SLG_FUSED_WIDE_LOCAL_GETFIELD: u16 = 1 << 11;
/// `fused_array_accumulate_long_on`
/// (`CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE_LONG` with `fast_field_zgc`;
/// wave 82, lane L7, a gate bit since the wave-83 frame rework): the
/// `lload_N` arm's long array accumulate.
pub(in crate::runtime::interpreter) const SLG_FUSED_ARRAY_ACCUMULATE_LONG: u16 = 1 << 12;
/// `CRATONVM_INTERP_FUSED_LOAD_PAIR` (interpreter round i4 wave 89, lane
/// L7): the `aload_N` arm's `aload_N; {aload_M | iload_M}; putfield |
/// invoke*` pair (`field_fast::fused_load_pair`).
pub(in crate::runtime::interpreter) const SLG_FUSED_LOAD_PAIR: u16 = 1 << 13;
/// `CRATONVM_INTERP_FUSED_ICONST_GOTO` (interpreter round i4 wave 89, lane
/// L7): the `iconst_1` arm's `iconst_1; goto`.
pub(in crate::runtime::interpreter) const SLG_FUSED_ICONST_GOTO: u16 = 1 << 14;
/// `CRATONVM_INTERP_FUSED_ICONST_ARITH` (interpreter round i4 wave 91, lane
/// L7): the `iload_N` arm's `iload_N; {iconst | bipush | sipush}; <int op>
/// [; istore_Z]` ([`iconst_arith_group`]). The word's last free bit.
pub(in crate::runtime::interpreter) const SLG_FUSED_ICONST_ARITH: u16 = 1 << 15;
// The bits of the dispatch loop's hoisted `diag_gates` word (interpreter round
// i3 wave 82, lane L7;
// `i77-L7-proposal-pack-the-dispatch-loops-diagnostic-gates-into-one-word`):
// one per per-entry diagnostic or per-call gate the loop carried as a
// separate bool before (frame ratchet). Each bit is exactly the bool of the
// same name it replaces.
/// `field_phases_on` (`CRATONVM_DBG_FIELD_PHASES`). Bit 0: the `getfield`
/// arms pass it by value, and `word & 1` is the bool itself.
pub(in crate::runtime::interpreter) const DG_FIELD_PHASES: u16 = 1 << 0;
/// `acmp_identity_trace` (`CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE`).
pub(in crate::runtime::interpreter) const DG_ACMP_IDENTITY_TRACE: u16 = 1 << 1;
/// `trace_sb_filter` (`CRATONVM_TRACE_SB_FILTER`).
pub(in crate::runtime::interpreter) const DG_TRACE_SB_FILTER: u16 = 1 << 2;
/// `pair_census` (`CRATONVM_QUICKEN_STATS=pairs`).
pub(in crate::runtime::interpreter) const DG_PAIR_CENSUS: u16 = 1 << 3;
/// `invoke_phases_on` (the invoke-phase instrument).
pub(in crate::runtime::interpreter) const DG_INVOKE_PHASES: u16 = 1 << 4;
/// `return_diagnostics_on` (the frame trace or the invoke-phase instrument).
pub(in crate::runtime::interpreter) const DG_RETURN_DIAGNOSTICS: u16 = 1 << 5;
/// `backedge_poll_gate_off` (the back-edge poll gate's kill switch, or a
/// sampling diagnostic that needs every back-edge `safepoint_check`).
pub(in crate::runtime::interpreter) const DG_BACKEDGE_POLL_GATE_OFF: u16 = 1 << 6;
/// `loop_work_on` (the loop-work tier-up credit).
pub(in crate::runtime::interpreter) const DG_LOOP_WORK: u16 = 1 << 7;
/// `thread_is_virtual` (the executing thread is a virtual thread).
pub(in crate::runtime::interpreter) const DG_THREAD_IS_VIRTUAL: u16 = 1 << 8;
/// `observers_hoisted` (`CRATONVM_INTERP_HOISTED_OBSERVERS`).
pub(in crate::runtime::interpreter) const DG_OBSERVERS_HOISTED: u16 = 1 << 9;
/// The loop top's slow path re-reads the observer gates:
/// `CRATONVM_INTERP_HOISTED_OBSERVERS` or `CRATONVM_JVMTI_STEP_POLL`.
pub(in crate::runtime::interpreter) const DG_OBSERVER_REREAD: u16 = 1 << 10;
/// The stack-dump hook runs in the loop top's slow path
/// (`CRATONVM_INTERP_STACK_DUMP_AT_POLL` with a watchdog or sampler armed).
pub(in crate::runtime::interpreter) const DG_STACK_DUMP_AT_POLL: u16 = 1 << 11;
/// The loop top's slow path reads the thread's agent-request word
/// (`CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL`, interpreter round i4 wave
/// 95, lane L7; `jvmti_events::take_method_entry_frame_control_checked` since
/// wave 136).
pub(in crate::runtime::interpreter) const DG_METHOD_ENTRY_FRAME_CONTROL: u16 = 1 << 12;
/// The `return` arm reports a constructor's normal return to the finalizer
/// registration (`exception_dispatch::finalizer_init_returned`; JIT round 21
/// wave 16, lane finalize-reinit, `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`).
/// Set from `VmConfig::finalizer_register_at_object_init` (with it off nothing
/// is withdrawn, so nothing needs re-registering) AND, since wave 19,
/// `VmConfig::finalizer_reregister_at_init_return`, both latched per VM.
pub(in crate::runtime::interpreter) const DG_FINALIZER_INIT_RETURN: u16 = 1 << 13;

// The bits of the dispatch loop's hoisted `door_gates` word (wave 82, lane
// L7): the invoke-door admissions, and since wave 101 the fused arithmetic
// groups that found no free bit elsewhere. A `u16` since interpreter round i6
// wave 115 (lane L8), when the byte ran out.
/// `invoke_fast_door_on`.
pub(in crate::runtime::interpreter) const DOOR_INVOKE_FAST: u16 = 1 << 0;
/// `nonvirtual_fast_door_on`.
pub(in crate::runtime::interpreter) const DOOR_NONVIRTUAL_FAST: u16 = 1 << 1;
/// `sig_poly_site_on`.
pub(in crate::runtime::interpreter) const DOOR_SIG_POLY_SITE: u16 = 1 << 2;
/// `sig_poly_site_first_on`.
pub(in crate::runtime::interpreter) const DOOR_SIG_POLY_SITE_FIRST: u16 = 1 << 3;
/// `CRATONVM_INTERP_FUSED_LONG_ARITH` (interpreter round i5 wave 101, lane
/// L7): the `lload_N` / `lload n` arms' [`fused_long_arith`]. Not a door: it
/// rides in this byte because `static_ldc_gates` has no free bit left, and a
/// new hoisted bool would grow the loop frame.
pub(in crate::runtime::interpreter) const DOOR_FUSED_LONG_ARITH: u16 = 1 << 4;
/// `CRATONVM_INTERP_FUSED_DOUBLE_ARITH` (interpreter round i5 wave 102, lane
/// L7): the `dload_N` arm's [`fused_double_arith`]; in this byte for the same
/// reason as [`DOOR_FUSED_LONG_ARITH`].
pub(in crate::runtime::interpreter) const DOOR_FUSED_DOUBLE_ARITH: u16 = 1 << 5;
/// `CRATONVM_INTERP_FUSED_LONG_INT_OPERAND` (interpreter round i5 wave 105,
/// lane L7): [`fused_long_arith`] also takes `{iload_Y | iload n}; i2l` as
/// its second operand; in this byte for the same reason as
/// [`DOOR_FUSED_LONG_ARITH`].
pub(in crate::runtime::interpreter) const DOOR_FUSED_LONG_INT_OPERAND: u16 = 1 << 6;
/// `CRATONVM_INTERP_FUSED_I2L_LONG_TAIL` (interpreter round i5 wave 106, lane
/// L7): the `i2l` arm's [`fused_i2l_long_tail`]; in this byte for the same
/// reason as [`DOOR_FUSED_LONG_ARITH`]. The byte's last free bit.
pub(in crate::runtime::interpreter) const DOOR_FUSED_I2L_LONG_TAIL: u16 = 1 << 7;
/// `CRATONVM_INTERP_FUSED_ZERO_BRANCH` (interpreter round i6 wave 115, lane
/// L8): `iload{_N, n}; if<cond>` as one dispatch, inside `fused_icmp_arm!`.
pub(in crate::runtime::interpreter) const DOOR_FUSED_ZERO_BRANCH: u16 = 1 << 8;
/// `CRATONVM_INTERP_FUSED_NULL_BRANCH` (interpreter round i6 wave 115, lane
/// L8): `aload_N; ifnull | ifnonnull` as one dispatch, in the `aload_N` arm.
pub(in crate::runtime::interpreter) const DOOR_FUSED_NULL_BRANCH: u16 = 1 << 9;
/// `CRATONVM_INTERP_FUSED_GETFIELD_CHAIN` (interpreter round i6 wave 116,
/// lane L8): a quickened `getfield` hit runs the getfields after it in the
/// same dispatch ([`getfield_chain`]).
pub(in crate::runtime::interpreter) const DOOR_FUSED_GETFIELD_CHAIN: u16 = 1 << 10;
/// `CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE` (interpreter round i6 wave 116,
/// lane L8): `aload n; ifnull | ifnonnull` as one dispatch, in the `aload n`
/// arm.
pub(in crate::runtime::interpreter) const DOOR_FUSED_NULL_BRANCH_WIDE: u16 = 1 << 11;
/// `CRATONVM_INTERP_FAST_SWITCH_ARMS` (interpreter round i6 wave 118, lane
/// L8): `tableswitch` / `lookupswitch` on the fast path, forward targets.
pub(in crate::runtime::interpreter) const DOOR_FAST_SWITCH: u16 = 1 << 12;
/// `CRATONVM_INTERP_FUSED_LOOP_STEP_TEST` (interpreter round i6 wave 118,
/// lane L8): `iinc; goto` runs the loop test at its target in the same
/// dispatch.
pub(in crate::runtime::interpreter) const DOOR_FUSED_LOOP_STEP_TEST: u16 = 1 << 13;
/// `CRATONVM_INTERP_FUSED_STORE_CHAIN` (interpreter round i6 wave 119, lane
/// L8): a served `aload_N; {aload_M | iload_M}; putfield` pair runs the pairs
/// right after it in the same dispatch ([`fused_store_chain`]).
pub(in crate::runtime::interpreter) const DOOR_FUSED_STORE_CHAIN: u16 = 1 << 14;
/// `CRATONVM_INTERP_FUSED_STORE_CHAIN_WIDE` (interpreter round i6 wave 120,
/// lane L8): the store chain's value may come from `iload n` / `aload n`
/// ([`store_pair_wide`]). The word's last free bit.
pub(in crate::runtime::interpreter) const DOOR_FUSED_STORE_CHAIN_WIDE: u16 = 1 << 15;

// The bits of the dispatch loop's `top_gates` byte (wave 82, lane L7): the
// gates the loop top tests on every bytecode, in one byte.
/// `single_step_active` (a JVMTI `SingleStep` listener is active).
pub(in crate::runtime::interpreter) const TG_SINGLE_STEP: u8 = 1 << 0;
/// `loop_top_hooks` (an armed breakpoint concerning the method, a forced
/// suspend point, or the pair census).
pub(in crate::runtime::interpreter) const TG_TOP_HOOKS: u8 = 1 << 1;
/// `stack_dump_possible` while the hook runs per bytecode (a watchdog or
/// sampler armed, `CRATONVM_INTERP_STACK_DUMP_AT_POLL` off).
pub(in crate::runtime::interpreter) const TG_STACK_DUMP: u8 = 1 << 2;
/// A signal is parked in `pending_java_exception` or `pending_runtime_error`
/// (interpreter round i4 wave 84, lane L7). Set by every writer of either
/// slot (`park_java_exception!`, `park_runtime!`) in both modes of
/// `CRATONVM_INTERP_ONE_TOP_SIGNAL_TEST`; cleared at the loop top once both
/// slots are drained.
pub(in crate::runtime::interpreter) const TG_PENDING: u8 = 1 << 3;
/// `CRATONVM_INTERP_ONE_TOP_SIGNAL_TEST` is OFF: the loop top also loads the
/// two pending slots, as it did before wave 84 (the belt-and-braces form; a
/// writer that forgot `TG_PENDING` is still drained).
pub(in crate::runtime::interpreter) const TG_SLOT_SIGNALS: u8 = 1 << 4;
/// The loop top's one gate test for both pending signals and the
/// per-bytecode stack-dump hook.
pub(in crate::runtime::interpreter) const TG_TOP_SIGNALS: u8 = TG_PENDING | TG_STACK_DUMP;
/// `word` with `bit` set when `on`, cleared otherwise (the loop's
/// `top_gates` updates, cold: debugger-gate changes only).
#[inline(always)]
pub(in crate::runtime::interpreter) fn with_gate_bit(word: u8, bit: u8, on: bool) -> u8 {
    if on {
        word | bit
    } else {
        word & !bit
    }
}


/// The `iload_X` arm's constant-operand arithmetic group
/// (`CRATONVM_INTERP_FUSED_ICONST_ARITH`; interpreter round i4 wave 91, lane
/// L7): decode `{iconst_m1..5 | bipush | sipush}` at `operand_pc` and the int
/// operation after it, and answer `vx <op> k` with the pc of the bytecode
/// after the operation. `None` (nothing read beyond the bytes, nothing
/// written) for any other operand or operation, and for an operation at or
/// past `code_len`. Only the operations that cannot throw: `iadd`, `isub`,
/// `imul`, `ishl`, `ishr`, `iushr`, `iand`, `ior`, `ixor`, each with the
/// JVMS semantics of its own arm (two's-complement wrap, the shift distance
/// masked to five bits).
///
/// # Safety
///
/// `code_ptr` addresses a padded bytecode buffer of `code_len` bytes plus two
/// padding bytes, and `operand_pc < code_len`, so the operand's own bytes
/// (`operand_pc + 1`, `operand_pc + 2`) are readable.
#[inline(always)]
pub(in crate::runtime::interpreter) unsafe fn iconst_arith_group(
    code_ptr: *const u8,
    code_len: usize,
    operand_pc: usize,
    vx: i32,
) -> Option<(i32, usize)> {
    // SAFETY: operand_pc < code_len (the caller's contract).
    let op = unsafe { *code_ptr.add(operand_pc) };
    let (k, op_pc) = match op {
        0x02..=0x08 => (i32::from(op) - 3, operand_pc + 1),
        0x10 => {
            // SAFETY: operand_pc + 1 is within the padded buffer.
            let b = unsafe { *code_ptr.add(operand_pc + 1) };
            (i32::from(i8::from_be_bytes([b])), operand_pc + 2)
        }
        0x11 => {
            // SAFETY: operand_pc + 1 and + 2 are within the padded buffer.
            let (hi, lo) = unsafe { (*code_ptr.add(operand_pc + 1), *code_ptr.add(operand_pc + 2)) };
            (i32::from(i16::from_be_bytes([hi, lo])), operand_pc + 3)
        }
        _ => return None,
    };
    if op_pc >= code_len {
        return None;
    }
    // JVM spec: the shift distance is the low five bits of the operand.
    let dist = k.cast_unsigned() & 31;
    // SAFETY: op_pc < code_len, checked above.
    let r = match unsafe { *code_ptr.add(op_pc) } {
        0x60 => vx.wrapping_add(k),
        0x64 => vx.wrapping_sub(k),
        0x68 => vx.wrapping_mul(k),
        0x78 => vx.wrapping_shl(dist),
        0x7a => vx.wrapping_shr(dist),
        0x7c => vx.cast_unsigned().wrapping_shr(dist).cast_signed(),
        0x7e => vx & k,
        0x80 => vx | k,
        0x82 => vx ^ k,
        _ => return None,
    };
    Some((r, op_pc + 1))
}


/// The second operand of a fused long group ([`long_arith_group`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter) enum LongOperand {
    /// `lload_0..3` or `lload n`: the local's index (not yet bounds-checked
    /// against `max_locals`).
    Local(usize),
    /// `lconst_0` / `lconst_1`.
    Const(i64),
    /// `iload_0..3; i2l` or `iload n; i2l`: the int local's index (not yet
    /// bounds-checked). Interpreter round i5 wave 105, lane L7
    /// (`CRATONVM_INTERP_FUSED_LONG_INT_OPERAND`).
    IntLocal(usize),
}

/// A decoded `<operand>; <long op> [; lstore]` tail of a fused long group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter) struct LongArithGroup {
    pub(in crate::runtime::interpreter) operand: LongOperand,
    /// The operation's opcode: `ladd`, `lsub`, `lmul`, `land`, `lor`,
    /// `lxor` or `lcmp`.
    pub(in crate::runtime::interpreter) op: u8,
    /// `lstore_0..3` / `lstore n`'s local (not yet bounds-checked), never
    /// with `lcmp`.
    pub(in crate::runtime::interpreter) store: Option<usize>,
    /// The pc after the group.
    pub(in crate::runtime::interpreter) next_pc: usize,
}

/// Decode `{lload_Y | lload n | lconst_0 | lconst_1}; ladd | lsub | lmul |
/// land | lor | lxor | lcmp [; lstore_Z | lstore n]` starting at
/// `operand_pc` of `code` (whose last two bytes are the interpreter's
/// padding; `code_len` is the method's own length). Interpreter round i5
/// wave 101, lane L7 (`CRATONVM_INTERP_FUSED_LONG_ARITH`). None of the
/// operations taken can throw (`ldiv` / `lrem` are not taken), and shifts
/// are not taken (their distance is an int, never a long load). `None`
/// for every other shape, or for a group that would run past `code_len`.
///
/// When `int_operand` (wave 105, `CRATONVM_INTERP_FUSED_LONG_INT_OPERAND`)
/// the operand may also be `{iload_Y | iload n}; i2l` (`i2l` never throws).
#[inline(always)]
pub(in crate::runtime::interpreter) fn long_arith_group(
    code: &[u8],
    code_len: usize,
    operand_pc: usize,
    int_operand: bool,
) -> Option<LongArithGroup> {
    let at = |pc: usize| if pc < code_len { code.get(pc).copied() } else { None };
    let (operand, op_pc) = match at(operand_pc)? {
        0x09 => (LongOperand::Const(0), operand_pc + 1),
        0x0a => (LongOperand::Const(1), operand_pc + 1),
        op @ 0x1e..=0x21 => (LongOperand::Local(usize::from(op - 0x1e)), operand_pc + 1),
        0x16 => (LongOperand::Local(usize::from(at(operand_pc + 1)?)), operand_pc + 2),
        op @ 0x1a..=0x1d if int_operand && at(operand_pc + 1) == Some(0x85) => {
            (LongOperand::IntLocal(usize::from(op - 0x1a)), operand_pc + 2)
        }
        0x15 if int_operand && at(operand_pc + 2) == Some(0x85) => (
            LongOperand::IntLocal(usize::from(at(operand_pc + 1)?)),
            operand_pc + 3,
        ),
        _ => return None,
    };
    let op = at(op_pc)?;
    if !matches!(op, 0x61 | 0x65 | 0x69 | 0x7f | 0x81 | 0x83 | 0x94) {
        return None;
    }
    let after = op_pc + 1;
    if op != 0x94 {
        match at(after) {
            Some(st @ 0x3f..=0x42) => {
                return Some(LongArithGroup {
                    operand,
                    op,
                    store: Some(usize::from(st - 0x3f)),
                    next_pc: after + 1,
                });
            }
            Some(0x37) => {
                if let Some(z) = at(after + 1) {
                    return Some(LongArithGroup {
                        operand,
                        op,
                        store: Some(usize::from(z)),
                        next_pc: after + 2,
                    });
                }
            }
            _ => {}
        }
    }
    Some(LongArithGroup {
        operand,
        op,
        store: None,
        next_pc: after,
    })
}

/// The value a fused long group's operation answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter) enum LongArithValue {
    Long(i64),
    /// `lcmp`'s -1 / 0 / 1.
    Int(i32),
}

/// `a <op> b` exactly as the unfused arms compute it (wrapping arithmetic,
/// signed `lcmp`).
#[inline(always)]
pub(in crate::runtime::interpreter) fn long_arith_apply(op: u8, a: i64, b: i64) -> Option<LongArithValue> {
    Some(match op {
        0x61 => LongArithValue::Long(a.wrapping_add(b)),
        0x65 => LongArithValue::Long(a.wrapping_sub(b)),
        0x69 => LongArithValue::Long(a.wrapping_mul(b)),
        0x7f => LongArithValue::Long(a & b),
        0x81 => LongArithValue::Long(a | b),
        0x83 => LongArithValue::Long(a ^ b),
        0x94 => LongArithValue::Int(match a.cmp(&b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }),
        _ => return None,
    })
}

/// Run `lload <first_local>; <operand>; <long op> [; lstore]` as one
/// dispatch, the operand decoded at `operand_pc` (interpreter round i5 wave
/// 101, lane L7, `CRATONVM_INTERP_FUSED_LONG_ARITH`, default off; the
/// `lload_0..3` and `lload n` arms call it). Returns the pc after the group,
/// or `None` — having written nothing and pushed nothing — for any shape
/// [`long_arith_group`] declines or a local index at or past `max_locals`,
/// so the plain `lload` runs. The locals are read raw
/// (`as_long_unchecked`), as the unfused `lload` + `ladd` pair reads them
/// through the stack; the result is pushed with `push_long_unchecked` /
/// `push_int_unchecked` as the unfused operation pushes it, or stored with
/// `set_local_unchecked(Value::Long)` as the `lstore_N` arm stores it.
/// Out of line so the loop frame does not grow.
#[inline(never)]
pub(in crate::runtime::interpreter) fn fused_long_arith(
    frame: &mut Frame,
    code_len: usize,
    operand_pc: usize,
    first_local: usize,
    int_operand: bool,
) -> Option<usize> {
    let g = long_arith_group(&frame.code, code_len, operand_pc, int_operand)?;
    let max_locals = usize::from(frame.max_locals);
    if first_local >= max_locals {
        return None;
    }
    let b = match g.operand {
        LongOperand::Local(i) if i < max_locals => {
            frame.get_local_compact_unchecked(i).as_long_unchecked()
        }
        LongOperand::Local(_) => return None,
        LongOperand::Const(k) => k,
        // `iload; i2l` (wave 105): an int-tagged local only, as the fused
        // compare's int operand (`fused_int_operand`); anything else declines
        // so the unfused `iload` keeps its own answer. `i2l` sign-extends.
        LongOperand::IntLocal(i) if i < max_locals => {
            i64::from(frame.get_local_compact_unchecked(i).as_int()?)
        }
        LongOperand::IntLocal(_) => return None,
    };
    if matches!(g.store, Some(z) if z >= max_locals) {
        return None;
    }
    let a = frame.get_local_compact_unchecked(first_local).as_long_unchecked();
    match long_arith_apply(g.op, a, b)? {
        LongArithValue::Int(r) => frame.stack.push_int_unchecked(r),
        LongArithValue::Long(r) => match g.store {
            Some(z) => frame.set_local_unchecked(z, Value::Long(r)),
            None => frame.stack.push_long_unchecked(r),
        },
    }
    Some(g.next_pc)
}

/// The `aload_N; {aload_M | iload_M}; putfield` pair
/// ([`field_fast::fused_load_pair`]), and then each such pair right after a
/// served one, in the same dispatch (interpreter round i6 wave 119, lane L8,
/// `CRATONVM_INTERP_FUSED_STORE_CHAIN`, default off): a constructor's or
/// setter's `this.a = a; this.b = b; this.c = c;`, the wave-88 census's
/// `putfield -> aload_0` (116 of 169 probes) that the load pair leaves to a
/// dispatch of its own. The `aload_N` arm has pushed the first group's
/// local at `pc`. For each later group this pushes its `aload_N` exactly as
/// the arm does (`load_ref_local`) and hands it to the same pair helper, so
/// every load and store is the unfused one's. It stops at the first bytecode
/// that does not begin such a group, and after a pair the helper did not
/// serve to its end: a `putfield` the quickened store declined (a null
/// receiver, a cold site, an armed watch, a final-field rule) is then left
/// at its own pc with both values pushed, so its NPE, its resolution and its
/// FieldModification event are the unfused ones. A served store neither
/// throws nor reaches a safepoint, and the caller has tested `fusion_off`.
/// Out of line, so the loop frame does not grow.
///
/// Wave 120 (lane L8, `CRATONVM_INTERP_FUSED_STORE_CHAIN_WIDE`, `wide`):
/// the value may also come from a local past 3 (`aload_N; {iload n | aload
/// n}; putfield`, a constructor's fifth and later parameters), at the head of
/// the chain and inside it. That pair is [`store_pair_wide`], with the
/// `iload n` / `aload n` arms' own pushes.
#[inline(never)]
pub(in crate::runtime::interpreter) fn fused_store_chain(
    shared: &SharedVm,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    code_len: usize,
    mut pc: usize,
    wide: bool,
) -> usize {
    loop {
        // The second load's opcode decides the pair's width.
        let second_wide = matches!(frame.code.get(pc + 1), Some(0x15 | 0x19));
        // `served`: the pair's `putfield` ran (any other answer is the pc of
        // a bytecode still to run, with the operand stack as the unfused
        // bytecodes before it leave it).
        let (next, served) = if second_wide {
            if !wide {
                return pc + 1;
            }
            let next = store_pair_wide(shared, sites, frame, code_len, pc);
            (next, next == pc + 6)
        } else {
            let next = field_fast::fused_load_pair(shared, sites, frame, code_len, pc);
            (next, next == pc + 5)
        };
        if !served || next + 4 >= code_len {
            return next;
        }
        let (first, second, third, fourth) = {
            let code: &[u8] = &frame.code;
            match (code.get(next), code.get(next + 1), code.get(next + 2), code.get(next + 3)) {
                (Some(&a), Some(&b), Some(&c), Some(&d)) => (a, b, c, d),
                _ => return next,
            }
        };
        let short_pair = matches!(second, 0x1a..=0x1d | 0x2a..=0x2d) && third == 0xb5;
        let wide_pair = wide && matches!(second, 0x15 | 0x19) && fourth == 0xb5;
        if !matches!(first, 0x2a..=0x2d) || !(short_pair || wide_pair) {
            return next;
        }
        // The next group's `aload_N`, as its own arm runs it.
        load_ref_local(shared, frame, usize::from(first - 0x2a));
        pc = next;
    }
}

/// One `aload_N; {iload n | aload n}; putfield` pair whose `aload_N` the
/// caller has pushed at `pc` ([`fused_store_chain`], wave 120, lane L8).
/// The second load pushes exactly as its own arm does (`iload n`: the raw
/// slot; `aload n`: `load_ref_local`), under the same `max_locals`
/// admission; then the quickened store ([`field_fast::putfield_fast`], the
/// field-watch union loaded per store). Answers `pc + 6` when the store ran,
/// `pc + 3` (the `putfield`, both values pushed) when the quickened store
/// declined, or `pc + 1` (the second load, nothing pushed) for any other
/// shape or an index out of range.
pub(in crate::runtime::interpreter) fn store_pair_wide(
    shared: &SharedVm,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    code_len: usize,
    pc: usize,
) -> usize {
    if pc + 5 >= code_len {
        return pc + 1;
    }
    let (op, index, third, hi, lo) = {
        let code: &[u8] = &frame.code;
        match (
            code.get(pc + 1),
            code.get(pc + 2),
            code.get(pc + 3),
            code.get(pc + 4),
            code.get(pc + 5),
        ) {
            (Some(&a), Some(&b), Some(&c), Some(&d), Some(&e)) => (a, usize::from(b), c, d, e),
            _ => return pc + 1,
        }
    };
    if third != 0xb5 || index >= usize::from(frame.max_locals) {
        return pc + 1;
    }
    match op {
        0x15 => {
            let cv = frame.get_local_compact_unchecked(index);
            frame.stack.push_compact(cv);
        }
        0x19 => load_ref_local(shared, frame, index),
        _ => return pc + 1,
    }
    if let Some(zgc) = field_fast::fast_field_zgc(shared) {
        if field_fast::putfield_fast(
            zgc,
            sites,
            frame,
            (u16::from(hi) << 8) | u16::from(lo),
            shared.config.site_census,
            None,
        ) {
            return pc + 6;
        }
    }
    pc + 3
}

/// Run the quickened `getfield`s that follow a `getfield` hit, starting
/// with the one at `pc` (interpreter round i6 wave 116, lane L8,
/// `CRATONVM_INTERP_FUSED_GETFIELD_CHAIN`, default off): `this.a.b`,
/// `node.next.value`. The value on top of the stack is the receiver of the
/// getfield at `pc`. Each one is served by [`field_fast::getfield_fast`]
/// exactly as its own arm serves it (the same site, admission, push), with
/// the field-watch union loaded per read (`watch = None`, as the loop's other
/// out-of-line field helpers ask) and the census and the field-phase gate as
/// the VM has them. Returns the pc of the first bytecode not run: a
/// non-`getfield`, the end of the code, or a getfield the quickened read
/// declined (a null receiver, a cold site, an armed watch), which then runs
/// unfused at its own pc, so its NPE and its JVMTI FieldAccess event are the
/// unfused ones. A hit neither throws nor reaches a safepoint. Out of line so
/// the loop frame does not grow; the arms call it only when the byte after
/// their hit is a `getfield`.
#[inline(never)]
pub(in crate::runtime::interpreter) fn getfield_chain(
    shared: &SharedVm,
    zgc: &crate::memory::vm_heap::VmHeap,
    sites: &mut FastFieldSiteCache,
    frame: &mut Frame,
    code_len: usize,
    mut pc: usize,
) -> usize {
    let census = shared.config.site_census;
    let phases = field_phases::on();
    while pc + 2 < code_len {
        let (op, hi, lo) = {
            let code: &[u8] = &frame.code;
            match (code.get(pc), code.get(pc + 1), code.get(pc + 2)) {
                (Some(&op), Some(&hi), Some(&lo)) => (op, hi, lo),
                _ => return pc,
            }
        };
        if op != 0xb4
            || !field_fast::getfield_fast(
                shared,
                zgc,
                sites,
                frame,
                (u16::from(hi) << 8) | u16::from(lo),
                census,
                phases,
                None,
            )
        {
            return pc;
        }
        pc += 3;
    }
    pc
}

/// A decoded `ladd | lsub | lmul | land | lor | lxor | lcmp [; lstore_Z |
/// lstore n]` tail after an `i2l` ([`i2l_long_tail`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter) struct I2lLongTail {
    /// The operation's opcode.
    pub(in crate::runtime::interpreter) op: u8,
    /// `lstore_0..3` / `lstore n`'s local (not yet bounds-checked), never
    /// with `lcmp`.
    pub(in crate::runtime::interpreter) store: Option<usize>,
    /// The pc after the group.
    pub(in crate::runtime::interpreter) next_pc: usize,
}

/// Decode the tail of `i2l; <long op> [; lstore]` with the operation at
/// `op_pc` of `code` (padded; `code_len` is the method's own length).
/// Interpreter round i5 wave 106, lane L7
/// (`CRATONVM_INTERP_FUSED_I2L_LONG_TAIL`). The operations are
/// [`long_arith_group`]'s (none throws; `ldiv` / `lrem` and the shifts are
/// not taken). `None` for every other shape or past `code_len`.
#[inline(always)]
pub(in crate::runtime::interpreter) fn i2l_long_tail(code: &[u8], code_len: usize, op_pc: usize) -> Option<I2lLongTail> {
    let at = |pc: usize| if pc < code_len { code.get(pc).copied() } else { None };
    let op = at(op_pc)?;
    if !matches!(op, 0x61 | 0x65 | 0x69 | 0x7f | 0x81 | 0x83 | 0x94) {
        return None;
    }
    let after = op_pc + 1;
    if op != 0x94 {
        match at(after) {
            Some(st @ 0x3f..=0x42) => {
                return Some(I2lLongTail {
                    op,
                    store: Some(usize::from(st - 0x3f)),
                    next_pc: after + 1,
                });
            }
            Some(0x37) => {
                if let Some(z) = at(after + 1) {
                    return Some(I2lLongTail {
                        op,
                        store: Some(usize::from(z)),
                        next_pc: after + 2,
                    });
                }
            }
            _ => {}
        }
    }
    Some(I2lLongTail {
        op,
        store: None,
        next_pc: after,
    })
}

/// Run `i2l; <long op> [; lstore]` as one dispatch over the operand stack's
/// `..., long, int` (interpreter round i5 wave 106, lane L7,
/// `CRATONVM_INTERP_FUSED_I2L_LONG_TAIL`, default off; the `i2l` arm calls
/// it with `op_pc` = the `i2l`'s pc + 1). The int is popped as the `i2l` arm
/// pops it and sign-extended, the long as the `ladd` arm pops it
/// (`pop_compact().as_long_unchecked()`); the result is pushed or stored as
/// the unfused arms push / store it. Returns the pc after the group, or
/// `None` -- having popped nothing -- for any shape [`i2l_long_tail`]
/// declines, a store index at or past `max_locals`, or a stack shallower
/// than two slots, so the plain `i2l` runs. Out of line so the loop frame
/// does not grow.
#[inline(never)]
pub(in crate::runtime::interpreter) fn fused_i2l_long_tail(frame: &mut Frame, code_len: usize, op_pc: usize) -> Option<usize> {
    let g = i2l_long_tail(&frame.code, code_len, op_pc)?;
    if matches!(g.store, Some(z) if z >= usize::from(frame.max_locals)) {
        return None;
    }
    if frame.stack.len() < 2 {
        return None;
    }
    let b = i64::from(frame.stack.pop_int_unchecked());
    let a = frame.stack.pop_compact().as_long_unchecked();
    match long_arith_apply(g.op, a, b) {
        Some(LongArithValue::Int(r)) => frame.stack.push_int_unchecked(r),
        Some(LongArithValue::Long(r)) => match g.store {
            Some(z) => frame.set_local_unchecked(z, Value::Long(r)),
            None => frame.stack.push_long_unchecked(r),
        },
        // Unreachable: `i2l_long_tail` admits only operations
        // `long_arith_apply` answers. Restore the stack and decline.
        None => {
            frame.stack.push_long_unchecked(a);
            frame.stack.push_int_unchecked(b as i32);
            return None;
        }
    }
    Some(g.next_pc)
}


/// The second operand of a fused double group ([`double_arith_group`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::runtime::interpreter) enum DoubleOperand {
    /// `dload_0..3` or `dload n`: the local's index (not yet bounds-checked
    /// against `max_locals`).
    Local(usize),
    /// `dconst_0` / `dconst_1`.
    Const(f64),
}

/// A decoded `<operand>; <double op> [; dstore]` tail of a fused double
/// group.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::runtime::interpreter) struct DoubleArithGroup {
    pub(in crate::runtime::interpreter) operand: DoubleOperand,
    /// The operation's opcode: `dadd`, `dsub`, `dmul`, `ddiv`, `drem`,
    /// `dcmpl` or `dcmpg`.
    pub(in crate::runtime::interpreter) op: u8,
    /// `dstore_0..3` / `dstore n`'s local (not yet bounds-checked), never
    /// with `dcmpl` / `dcmpg`.
    pub(in crate::runtime::interpreter) store: Option<usize>,
    /// The pc after the group.
    pub(in crate::runtime::interpreter) next_pc: usize,
}

/// Decode `{dload_Y | dload n | dconst_0 | dconst_1}; dadd | dsub | dmul |
/// ddiv | drem | dcmpl | dcmpg [; dstore_Z | dstore n]` starting at
/// `operand_pc` of `code` (whose last two bytes are the interpreter's
/// padding; `code_len` is the method's own length). Interpreter round i5
/// wave 102, lane L7 (`CRATONVM_INTERP_FUSED_DOUBLE_ARITH`). No double
/// operation throws (`ddiv` / `drem` by zero answer infinity / NaN). `None`
/// for every other shape, or for a group that would run past `code_len`.
#[inline(always)]
pub(in crate::runtime::interpreter) fn double_arith_group(code: &[u8], code_len: usize, operand_pc: usize) -> Option<DoubleArithGroup> {
    let at = |pc: usize| if pc < code_len { code.get(pc).copied() } else { None };
    let (operand, op_pc) = match at(operand_pc)? {
        0x0e => (DoubleOperand::Const(0.0), operand_pc + 1),
        0x0f => (DoubleOperand::Const(1.0), operand_pc + 1),
        op @ 0x26..=0x29 => (DoubleOperand::Local(usize::from(op - 0x26)), operand_pc + 1),
        0x18 => (DoubleOperand::Local(usize::from(at(operand_pc + 1)?)), operand_pc + 2),
        _ => return None,
    };
    let op = at(op_pc)?;
    if !matches!(op, 0x63 | 0x67 | 0x6b | 0x6f | 0x73 | 0x97 | 0x98) {
        return None;
    }
    let after = op_pc + 1;
    if !matches!(op, 0x97 | 0x98) {
        match at(after) {
            Some(st @ 0x47..=0x4a) => {
                return Some(DoubleArithGroup {
                    operand,
                    op,
                    store: Some(usize::from(st - 0x47)),
                    next_pc: after + 1,
                });
            }
            Some(0x39) => {
                if let Some(z) = at(after + 1) {
                    return Some(DoubleArithGroup {
                        operand,
                        op,
                        store: Some(usize::from(z)),
                        next_pc: after + 2,
                    });
                }
            }
            _ => {}
        }
    }
    Some(DoubleArithGroup {
        operand,
        op,
        store: None,
        next_pc: after,
    })
}

/// The value a fused double group's operation answers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::runtime::interpreter) enum DoubleArithValue {
    Double(f64),
    /// `dcmpl` / `dcmpg`'s -1 / 0 / 1.
    Int(i32),
}

