#[allow(unused_imports)]
use super::*;

impl ThreadRegistry {
    /// gci i23/hs (step 2 of `common-a-proposal-per-thread-handshakes`): ask
    /// `thread_id` to run `op` on itself ([`Self::request_handshake`]) and
    /// wait for it, up to `budget`, in `slice`s.
    ///
    /// Between slices the wait stops early when the target entered a blocked
    /// region ([`HandshakeOutcome::TargetBlocked`]: its deposit is current),
    /// was dropped ([`HandshakeOutcome::Gone`]), or `abort` answers true
    /// ([`HandshakeOutcome::NotServed`]). The requester is a running mutator,
    /// not GC-safe while it waits, so the caller passes "a pause is
    /// requested" as `abort`: a pause then waits at most one `slice` for it.
    /// Whenever it stops early or the budget runs out, the op is withdrawn
    /// from the SAME queue it was pushed on; a withdrawal that finds it gone
    /// means the target already took it, and the wait goes on for that op's
    /// end (it takes no pause and no lock the requester holds), bounded by
    /// [`HANDSHAKE_TAKEN_OP_WAIT`]. So the op runs exactly once
    /// ([`HandshakeOutcome::Ran`]) or never -- except past that bound: a
    /// taken op that has not ended within [`HANDSHAKE_TAKEN_OP_WAIT`] answers
    /// [`HandshakeOutcome::NotServed`] although it is running (its later
    /// effect is harmless, see the constant), and the caller takes its
    /// fallback pause as well. (gcj j2/cm2: this said "exactly once or never"
    /// without the exception; `gcj-j1cm-safepoint-barrier-and-registry-review-residuals`
    /// item 3.)
    pub fn handshake_and_wait(
        &self,
        thread_id: ThreadId,
        op: crate::threading::jvm_thread::HandshakeOp,
        budget: std::time::Duration,
        slice: std::time::Duration,
        abort: impl FnMut() -> bool,
    ) -> HandshakeOutcome {
        self.handshake_and_wait_kicked(thread_id, op, budget, slice, abort, || {})
    }

    /// gcj j1/cm: [`Self::handshake_and_wait`] plus a kick. `kick` runs once,
    /// right after the op was queued and before the first wait, and never
    /// when nothing was queued (`Gone` / `TargetBlocked` at the request).
    /// The frame-trace client kicks every interpreter dispatch loop's poll
    /// word (`GcBarrier::note_code_moved_on_every_loop`), so an interpreted
    /// target that allocates nothing still reaches its poll
    /// (`interpreter::loop_top_poll` runs `run_pending_handshakes`) at its
    /// next bytecode instead of at its next allocation slow path (step "P4/ph"
    /// of `common-a-proposal-per-thread-handshakes`). The push's `Release`
    /// store of the queue word happens before `kick`'s `Release` add, so a
    /// loop whose `Acquire` read of its poll word sees the kick also sees the
    /// op queued.
    pub fn handshake_and_wait_kicked(
        &self,
        thread_id: ThreadId,
        op: crate::threading::jvm_thread::HandshakeOp,
        budget: std::time::Duration,
        slice: std::time::Duration,
        abort: impl FnMut() -> bool,
        kick: impl FnOnce(),
    ) -> HandshakeOutcome {
        self.handshake_and_wait_kicking_target(thread_id, op, budget, slice, abort, |_| kick())
    }

    /// gcj j2/cm2: [`Self::handshake_and_wait_kicked`] whose `kick` is handed
    /// the TARGET's [`GcBlockState`] (the one the op was queued on), so it can
    /// kick only the poll word the target's loops run on
    /// (`GcBlockState::kick_published_loop_poll_word`,
    /// `CRATONVM_GC_HANDSHAKE_TARGET_KICK`). Same contract otherwise.
    pub fn handshake_and_wait_kicking_target(
        &self,
        thread_id: ThreadId,
        op: crate::threading::jvm_thread::HandshakeOp,
        budget: std::time::Duration,
        slice: std::time::Duration,
        abort: impl FnMut() -> bool,
        kick: impl FnOnce(&GcBlockState),
    ) -> HandshakeOutcome {
        self.handshake_and_wait_kicking_twice(thread_id, op, budget, slice, abort, kick, |_| {}, || {})
    }

    /// t1 lane E wave 3 (`t1-V2-handshake-poll-bit-stale-carrier-arm-skips-the-kick-fallback`):
    /// [`Self::handshake_and_wait_kicking_target`] plus a second chance.
    /// `rekick` runs at most once, with the same [`GcBlockState`], when the
    /// FIRST slice ended with the op still pending and the wait goes on (the
    /// target neither blocked nor gone, no abort, budget left). The
    /// frame-trace client uses it to fall back to the loop kicks after a
    /// handshake-bit arm that turned out stale (a remount racing its
    /// owner check, or another VM's poll on the same OS thread lowering the
    /// bit); a prompt target has settled long before. Same contract otherwise.
    ///
    /// t1 wave 8 lane HANDSHK (`CRATONVM_GC_HANDSHAKE_UNMOUNTED_DEFERS_KICK`,
    /// opt-in): when the target is UNMOUNTED right after the push (a virtual
    /// thread inside `run_slice`'s unmount window: `set_mounted(false)` came
    /// before its deposit raises `in_blocked_region`), `kick` is not run then
    /// -- the target polls no loop until it remounts, and a remount's first
    /// dispatch-loop top polls the queue anyway -- but after the first slice,
    /// in `rekick`'s place, and only when the op is still pending and the wait
    /// goes on; `rekick` then gets the next such slice. The common case (the
    /// deposit raises the flag within the slice) ends `TargetBlocked` with no
    /// kick at all, instead of a kick of every dispatch loop of the VM.
    ///
    /// t1 wave 8 lane HANDSHK (`CRATONVM_GC_HANDSHAKE_REQUESTER_SAFE_WHILE_WAITING`,
    /// default ON, `=0` kills): `drain_self` runs on each wait / spin slice so a
    /// requester drains its own pending handshakes while waiting. Mutual or cyclic
    /// samplers resolve without safepoint pauses.
    #[allow(clippy::too_many_arguments)]
    pub fn handshake_and_wait_kicking_twice(
        &self,
        thread_id: ThreadId,
        op: crate::threading::jvm_thread::HandshakeOp,
        budget: std::time::Duration,
        slice: std::time::Duration,
        mut abort: impl FnMut() -> bool,
        kick: impl FnOnce(&GcBlockState),
        rekick: impl FnOnce(&GcBlockState),
        mut drain_self: impl FnMut(),
    ) -> HandshakeOutcome {
        use crate::threading::jvm_thread::HandshakeState;
        let mut rekick = Some(rekick);
        let (state, ticket) = match self.request_handshake(thread_id, op) {
            HandshakeRequest::Gone(_) => return HandshakeOutcome::Gone,
            HandshakeRequest::TargetBlocked(_) => return HandshakeOutcome::TargetBlocked,
            HandshakeRequest::Queued { state, ticket } => (state, ticket),
        };
        // Read after the push: a remount after this read finds the op queued
        // at its first loop top; one before it is seen as mounted (kicked now).
        let mut deferred_kick = if self.handshake_unmounted_defers_kick
            && !self.is_mounted(thread_id)
        {
            Some(kick)
        } else {
            kick(&*state);
            None
        };
        let settled = |s: HandshakeState| match s {
            HandshakeState::Ran => Some(HandshakeOutcome::Ran),
            // gci i24/rv: `Abandoned` is the owner's drop (`abandon_all`,
            // which runs after the drop notice raised `owner_dropped`) -- or
            // an op that UNWOUND on a live owner (`SettleOnUnwind`: that op,
            // or one queued before it, panicked). The second published
            // nothing, so it is not served: `Gone` made the frame-trace
            // client skip its pause and hand back a stale trace.
            HandshakeState::Abandoned if state.owner_dropped() => Some(HandshakeOutcome::Gone),
            HandshakeState::Abandoned => Some(HandshakeOutcome::NotServed),
            // Only this requester withdraws its ticket.
            HandshakeState::Withdrawn => Some(HandshakeOutcome::NotServed),
            HandshakeState::Pending => None,
        };
        let deadline = Instant::now() + budget;
        // gcj j1/cm (`CRATONVM_GC_HANDSHAKE_SPIN_WAIT`, default ON, `=0`
        // kills): wait each slice by polling the ticket with `yield_now`, not
        // by a timed condvar wait. A timed wait of 100 us is rounded up to the
        // OS timer tick where the target never notifies (a JIT-compiled peer
        // that does not poll within the budget): on Windows' default 15.6 ms
        // tick the 1 ms budget became ~13 ms per sample before the
        // frame-trace pause (vi23-hs L3 on the dev base: p50 13363 us on vs
        // 477 us off). Polling ends each slice on time; the requester yields
        // its core meanwhile.
        let spin = self.handshake_spin_wait;
        // t1 lane E2 (`CRATONVM_GC_HANDSHAKE_WAIT_SEES_BLOCK`, opt-in): the spin
        // also watches the target's blocked flag and drop notice, so a target
        // that parks, unmounts (a virtual thread's yield deposit) or enters a
        // GC-safe native AFTER the op was queued is answered from its deposit
        // at once -- HotSpot's requester processes the handshake itself for a
        // target that reached a safe state -- not at the end of the slice
        // (100 us for the frame trace; the deposit never polls, so the op
        // could only be withdrawn there). `saw_block`: the flag was up inside
        // the spin; the target may have left the region again by the check
        // below, and its deposit of that instant is still the answer.
        let sees_block = self.handshake_wait_sees_block;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut saw_block = false;
            let now_state = if spin {
                let until = Instant::now() + slice.min(left);
                loop {
                    if self.handshake_requester_safe_while_waiting {
                        drain_self();
                    }
                    let s = ticket.state();
                    if s != HandshakeState::Pending || Instant::now() >= until {
                        break s;
                    }
                    if sees_block
                        && (state.in_blocked_region.load(Ordering::Acquire)
                            || state.owner_dropped())
                    {
                        saw_block = true;
                        break s;
                    }
                    std::thread::yield_now();
                }
            } else {
                if self.handshake_requester_safe_while_waiting {
                    drain_self();
                }
                ticket.wait_timeout(slice.min(left))
            };
            if let Some(done) = settled(now_state) {
                return done;
            }
            let blocked = saw_block || state.in_blocked_region.load(Ordering::Acquire);
            let gone = state.owner_dropped();
            let aborted = abort();
            if !(blocked || gone || aborted || Instant::now() >= deadline) {
                if let Some(kick) = deferred_kick.take() {
                    kick(&*state);
                } else if let Some(rekick) = rekick.take() {
                    rekick(&*state);
                }
                continue;
            }
            if state.handshakes.withdraw(&ticket).is_some() {
                return if gone {
                    HandshakeOutcome::Gone
                } else if blocked {
                    HandshakeOutcome::TargetBlocked
                } else {
                    HandshakeOutcome::NotServed
                };
            }
            // Taken by the target (running it now) or abandoned by its drop.
            if aborted {
                // When abort() stopped the wait (a pause was requested), the requester is a
                // counted, non-GC-safe mutator holding that pause. Wait in short slices rather
                // than holding up to HANDSHAKE_TAKEN_OP_WAIT in a single block.
                let taken_deadline = Instant::now() + HANDSHAKE_TAKEN_OP_WAIT;
                let poll_slice = slice.min(std::time::Duration::from_millis(5));
                while Instant::now() < taken_deadline {
                    if self.handshake_requester_safe_while_waiting {
                        drain_self();
                    }
                    let s = ticket.wait_timeout(poll_slice);
                    if let Some(done) = settled(s) {
                        return done;
                    }
                }
                return HandshakeOutcome::NotServed;
            }
            if self.handshake_requester_safe_while_waiting {
                drain_self();
            }
            return settled(ticket.wait_timeout(HANDSHAKE_TAKEN_OP_WAIT))
                .unwrap_or(HandshakeOutcome::NotServed);
        }
    }

    /// `CRATONVM_GC_HANDSHAKE_FRAME_TRACE` for this VM, read once (gci i23/hs;
    /// default ON since JIT round 23 wave 6, `=0` kills): does a cross-thread `getStackTrace()` of a running peer ask
    /// it to publish its trace through a handshake
    /// ([`Self::handshake_and_wait`]) before it falls back to the frame-trace
    /// pause? One load of a field fixed at construction.
    #[inline(always)]
    pub fn handshake_frame_trace(&self) -> bool {
        self.handshake_frame_trace
    }

    /// `CRATONVM_GC_HANDSHAKE_LOOP_KICK` for this VM, read once (gcj j1/cm;
    /// default ON, `=0` kills; reached only under the opt-in
    /// [`Self::handshake_frame_trace`]): does the frame-trace handshake kick
    /// every interpreter dispatch loop's poll word after queueing
    /// ([`Self::handshake_and_wait_kicked`])? `=0`: the gci i23/hs reach (an
    /// interpreted target serves at its next allocation slow path only).
    #[inline(always)]
    pub fn handshake_loop_kick(&self) -> bool {
        self.handshake_loop_kick
    }

    /// `CRATONVM_DBG_STW_EXPECTED_IDS` for this VM, latched at construction
    /// (gcj j2/cm2): print the census's expected identity set per collection
    /// request (`[stw-expected]`) and each blocking park's arrival
    /// (`[stw-park]`). Diagnostic; presence of the variable turns it on.
    #[inline(always)]
    pub fn dbg_stw_expected_ids(&self) -> bool {
        self.dbg_stw_expected_ids
    }

    /// `CRATONVM_GC_HANDSHAKE_TARGET_KICK` for this VM, read once (gcj j2/cm2,
    /// opt-in; `gcj-j1cm-proposal-target-only-handshake-kick`): does a dispatch
    /// loop entry publish the poll word it runs on
    /// (`GcBlockState::publish_loop_poll_word`), and the frame-trace
    /// handshake kick only its target's published word instead of every loop
    /// of the VM? The requester side is Generational only (G1 and ZGC keep the
    /// kick-all); the publication is one compare per loop entry.
    #[inline(always)]
    pub fn handshake_target_kick(&self) -> bool {
        self.handshake_target_kick
    }

    /// `CRATONVM_GC_HANDSHAKE_POLL_BIT` for this VM, read once (t1 lane E,
    /// opt-in; step 3 of `common-a-proposal-per-thread-handshakes`): does a
    /// dispatch loop entry publish the poll word it runs on (as under
    /// [`Self::handshake_target_kick`]), and the frame-trace handshake raise
    /// the HANDSHAKE bit of its target's published word
    /// (`GcBlockState::arm_published_loop_poll_word_handshake`) instead of
    /// kicking loops? The bit sits in the byte compiled polls test under
    /// `CRATONVM_JIT_POLL_THREAD_WORD`, so a JIT-compiled peer that calls no
    /// helper and allocates nothing serves the handshake at its next poll
    /// rather than costing its requester the 1 ms budget plus the pause.
    /// Every backend (the bit is per thread and no collector reads it).
    #[inline(always)]
    pub fn handshake_poll_bit(&self) -> bool {
        self.handshake_poll_bit
    }

    /// `CRATONVM_GC_HANDSHAKE_REQUESTER_SAFE_WHILE_WAITING` for this VM, read once
    /// (t1 wave 8 lane HANDSHK, default ON since 2026-10-26, `=0` kills; every backend):
    /// the requester drains its own pending handshakes while waiting in
    /// [`Self::handshake_and_wait_kicking_twice`], so mutual or cyclic samplers
    /// resolve without safepoint pauses.
    #[inline(always)]
    pub fn handshake_requester_safe_while_waiting(&self) -> bool {
        self.handshake_requester_safe_while_waiting
    }

    /// t1 lane E wave 3 (`t1-V2-handshake-poll-bit-stale-carrier-arm-skips-the-kick-fallback`):
    /// the OS tid `thread_id` runs on NOW, for a handshake-bit arm's owner
    /// check (`GcBlockState::arm_published_loop_poll_word_handshake`): `None`
    /// for an unknown or dead entry, an UNMOUNTED continuation (its `os_tid` is
    /// its last carrier's, kept for the census), or no published tid. One
    /// registry read lock.
    pub(crate) fn mounted_os_tid(&self, thread_id: ThreadId) -> Option<u32> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        if !entry.alive.load(Ordering::Acquire) || !entry.mounted.load(Ordering::Acquire) {
            return None;
        }
        Some(entry.os_tid.load(Ordering::Acquire)).filter(|&t| t != 0)
    }

    /// Hand every registered thread's published frame snapshot to `f`, each
    /// under its own lock, to be rewritten in place. A self-contained walk
    /// under the registry read guard: `f` must not call back into the
    /// registry. Interpreter round i1 wave 22, lane L3
    /// (`obsolete_frames::resolve_captured_lines_of_class`).
    pub(crate) fn for_each_frame_trace(
        &self,
        mut f: impl FnMut(&mut Vec<cratonvm_native_api::StackTraceEntry>),
    ) {
        let threads = self.threads.read();
        for entry in threads.values() {
            let mut trace = entry.frame_trace.lock();
            f(&mut trace);
        }
    }

    /// Hand every registered thread's `GcBlockState` to `f` (an unmounted
    /// virtual thread's boxed `JvmThread` included: its entry shares that
    /// `Arc` too), under the registry read guard: `f` must not call back into
    /// the registry. The stale-frame census of interpreter round i1 waves
    /// 23-24, lane L3 (`obsolete_frames::prune_histories_by_census`), which
    /// reads `obsolete_frame_floor`, `in_blocked_region` and
    /// `blocked_frame_classes`.
    pub(crate) fn for_each_gc_block_state(&self, mut f: impl FnMut(&GcBlockState)) {
        let threads = self.threads.read();
        for entry in threads.values() {
            f(&entry.gc_block_state);
        }
    }

    /// [`frame_trace_of`](Self::frame_trace_of) with source line numbers filled
    /// in from `class_store`.
    ///
    /// ARCH-2026-07-26 (`cross-owner-closeout`, request CR-SW-2 of
    /// `stackwalk-and-vtable.md`). The published
    /// snapshot is line-less because the *depositor* must stay lock-free; the
    /// *reader* usually does hold a `ClassStore` (cross-thread
    /// `Thread.getStackTrace()`, `dumpThreads()`, the JMX thread dump), so it
    /// can pay for the resolution once, only when a dump is actually taken.
    ///
    /// Strictly additive: `stackwalker::resolve_line_numbers_in_place` is
    /// fail-closed — it yields the line an eager capture would have produced or
    /// leaves `-1` — so this can only replace unknowns with correct lines. It
    /// never produces a wrong line, and it never touches an entry that already
    /// has one (including the `-2` native sentinel).
    ///
    /// Resolution is exact for frames carrying `StackTraceEntry::method_index`;
    /// the deposit path cannot compute one (it has no `ClassStore`), so those
    /// frames fall back to the unambiguous-name rule and overloaded frames stay
    /// at `-1`. That is still strictly more than the all-`-1` snapshot.
    ///
    /// The registry lock is released before the resolution runs: the walk over
    /// the returned `Vec` touches no registry state, and holding L5 across a
    /// `ClassStore` walk would invert the usual acquisition order.
    pub fn frame_trace_of_resolved(
        &self,
        thread_id: ThreadId,
        class_store: &crate::classloading::ClassStore,
    ) -> Vec<cratonvm_native_api::StackTraceEntry> {
        let mut trace = self.frame_trace_of(thread_id);
        crate::runtime::stackwalker::resolve_line_numbers_in_place(class_store, &mut trace);
        trace
    }

    /// xt-hardening follow-up (2026-07-03): OS tids of alive threads
    /// currently `in_blocked_region` (excluded from the barrier, covered by
    /// `deposit_root_snapshot`, whose JIT-chain scan -- eager, or deferred to
    /// `resolve_deferred_blocked_jit_chains` -- leaves the Rust frames around
    /// the chain on the native stack uncovered; gci i25/rv corrected "never
    /// scans the JIT band", false since `24cf0b19b0`). This is the ONLY gap `helper_window_pass` exists
    /// to close (its own doc comment says so); a cooperatively-arrived
    /// mutator already published its JIT roots via `update_root_snapshot`
    /// before parking at the barrier, so re-scanning it is pure redundant
    /// over-retention risk. Threads with `os_tid == 0` (registered but not
    /// yet started) cannot be blocked, so they never contribute a false 0.
    /// JIT round 21 lane refs, REFS21-6 (opt-in
    /// `CRATONVM_XT_HELPER_WINDOW_PARK_CREDIT`): the OS tids of blocked,
    /// mounted threads whose block is an armed Java-level park
    /// (`GcBlockState::park_credit_armed`) AND whose flag-raising deposit ran
    /// its compiled-chain scan (`park_credit_deposited`: eagerly, rooting and
    /// pinning the compiled frames in the deposit itself, or deferred to the
    /// collector's walk this pause, `resolve_deferred_blocked_jit_chains`). A
    /// tid shared by an entry that does not qualify (a mounted virtual thread
    /// and its carrier) is not credited.
    pub fn park_credited_os_tids(&self) -> Vec<u32> {
        let threads = self.threads.read();
        let mut credited: Vec<u32> = Vec::new();
        let mut refused: Vec<u32> = Vec::new();
        for e in threads.values() {
            if !(entry_is_stw_live(e)
                && e.mounted.load(Ordering::Acquire)
                && e.gc_block_state.in_blocked_region.load(Ordering::Acquire))
            {
                continue;
            }
            let tid = e.os_tid.load(Ordering::Acquire);
            if tid == 0 {
                continue;
            }
            let state = &e.gc_block_state;
            if state.park_credit_armed.load(Ordering::Acquire)
                && state.park_credit_deposited.load(Ordering::Acquire)
                && !state.owner_dropped()
            {
                credited.push(tid);
            } else {
                refused.push(tid);
            }
        }
        credited.retain(|t| !refused.contains(t));
        credited.sort_unstable();
        credited.dedup();
        credited
    }

    pub fn blocked_os_tids(&self) -> Vec<u32> {
        // JIT round 16 wave 9b (lane ffm): a thread in foreign code is never
        // signalled (`ffm_in_native_signal_exempt`).
        let exempt_on = self.ffm_in_native_no_signal;
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| {
                // gen r5w2/roots6: `mounted` -- a parked (unmounted) virtual
                // thread has nothing on its last carrier's stack, and that
                // carrier is not blocked at all.
                entry_is_stw_live(e)
                    && e.mounted.load(Ordering::Acquire)
                    && e.gc_block_state.in_blocked_region.load(Ordering::Acquire)
                    && !(exempt_on && ffm_in_native_signal_exempt(&e.gc_block_state))
            })
            .map(|e| e.os_tid.load(Ordering::Acquire))
            .filter(|&t| t != 0)
            .collect()
    }

    /// zwr: the delivery threads' block states (`GcBlockState::delivery_thread`),
    /// for the take-over to read their published Rust band floor once frozen
    /// (`TakenOver::set_rust_band_floors`).
    pub(crate) fn delivery_thread_states(&self) -> Vec<(u32, Arc<GcBlockState>)> {
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| e.gc_block_state.delivery_thread.load(Ordering::Acquire))
            .filter_map(|e| {
                let t = e.os_tid.load(Ordering::Acquire);
                (t != 0).then(|| (t, Arc::clone(&e.gc_block_state)))
            })
            .collect()
    }

    /// JIT round 18 wave 10b (lane ffmeintr, `CRATONVM_FFM_LAZY_SIGNAL_GATE`):
    /// `(os_tid, block state)` of every alive, mounted entry with a published
    /// OS tid, for the Linux take-over pass to gate its signal on
    /// (`TakenOver::set_signal_gates`). One registry read and one `Arc` clone
    /// per thread, per pass.
    pub(crate) fn signal_gate_states(&self) -> Vec<(u32, Arc<GcBlockState>)> {
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| entry_is_stw_live(e) && e.mounted.load(Ordering::Acquire))
            .filter_map(|e| {
                let t = e.os_tid.load(Ordering::Acquire);
                (t != 0).then(|| (t, Arc::clone(&e.gc_block_state)))
            })
            .collect()
    }

    /// JIT round 18 wave 10 (lane ffmeintr, EINTR2-1,
    /// `CRATONVM_XT_HELPER_UNPARKED_READ`): the blocked, mounted threads that
    /// are inside a blocked-wait trampoline right now
    /// (`GcBlockState::blocked_wait_lo` non-zero), with their block state, for
    /// the helper-window pass to read without parking. A snapshot: the pass
    /// re-checks each one with `try_hold_blocked_wait_band` and parks any
    /// thread that has left its trampoline since.
    pub(crate) fn blocked_wait_band_states(&self) -> Vec<(u32, Arc<GcBlockState>)> {
        let threads = self.threads.read();
        threads
            .values()
            .filter(|e| {
                entry_is_stw_live(e)
                    && e.mounted.load(Ordering::Acquire)
                    && e.gc_block_state.in_blocked_region.load(Ordering::Acquire)
                    && e.gc_block_state.blocked_wait_lo.load(Ordering::Acquire) != 0
            })
            .filter_map(|e| {
                let t = e.os_tid.load(Ordering::Acquire);
                (t != 0).then(|| (t, Arc::clone(&e.gc_block_state)))
            })
            .collect()
    }

    /// [`Self::blocked_os_tids`], split in one registry read (gcd d2/i, opt-in
    /// `CRATONVM_XT_BLOCKED_MONITOR_PROOF`): the blocked threads that proved
    /// their JIT entry chain rewritable at a block in the compiled
    /// `monitorenter` helper, as `(os_tid, proven depth)`, and every other
    /// blocked thread's OS tid, exactly as `blocked_os_tids` lists it.
    ///
    /// The row is read only for an entry whose `in_blocked_region` is already
    /// seen up (Acquire), and the owner settles it before raising the flag
    /// (Release), so a proven depth read here is the one its current block's
    /// deposit proved. `0` and the armed marker read as "not proven".
    pub fn blocked_os_tids_split_by_monitor_proof(&self) -> (Vec<u32>, Vec<(u32, usize)>) {
        let exempt_on = self.ffm_in_native_no_signal;
        let threads = self.threads.read();
        let mut rest = Vec::new();
        let mut proven = Vec::new();
        for e in threads.values() {
            if !(entry_is_stw_live(e)
                && e.mounted.load(Ordering::Acquire)
                && e.gc_block_state.in_blocked_region.load(Ordering::Acquire))
            {
                continue;
            }
            // JIT round 16 wave 9b (lane ffm): see `blocked_os_tids`.
            if exempt_on && ffm_in_native_signal_exempt(&e.gc_block_state) {
                continue;
            }
            let tid = e.os_tid.load(Ordering::Acquire);
            if tid == 0 {
                continue;
            }
            let depth = e
                .gc_block_state
                .monitor_block_proven_jit_depth
                .load(Ordering::Acquire);
            if depth != 0 && depth != crate::threading::jvm_thread::MONITOR_BLOCK_PROOF_ARMED {
                proven.push((tid, depth));
            } else {
                rest.push(tid);
            }
        }
        (rest, proven)
    }

    /// gci i20/vh (`CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE`, default
    /// ON since gci i24/jp; a row whose owner was dropped counts as one
    /// without a record, `deferred_chain_row`):
    /// the OS tids of the blocked peers whose CURRENT blocking deposit deferred
    /// a complete compiled-chain walk to the collector
    /// (`GcBlockState::deferred_jit_chain` is `Some`), i.e. the peers whose
    /// compiled frames this pause's `resolve_deferred_blocked_jit_chains` marks
    /// and pins from their frozen bands. Same roster rule as
    /// [`Self::blocked_os_tids`]. A tid shared with a blocked entry WITHOUT a
    /// deferred record (a mounted virtual thread shares its carrier's tid) is
    /// left out: its band is not covered by a record. Sorted, deduplicated.
    ///
    /// Read during a pause: a blocked thread cannot leave its region (and so
    /// cannot take or replace its record) until the pause ends, so the answer
    /// holds for the helper-window pass that consumes it. Lock order is the
    /// collector's own (`for_each_gc_block_state` then the record), as in
    /// `resolve_deferred_blocked_jit_chains`.
    /// ki3 (`CRATONVM_XT_HELPER_WINDOW_COVERED_CHAIN_PIN_ONLY`): the OS tids
    /// of the blocked peers whose current window's deposit covered their whole
    /// compiled chain -- deferred it to this pause's walk (a
    /// `deferred_jit_chain` record) or walked it itself
    /// (`deposit_walked_jit_chain`). The roster rules of
    /// [`Self::blocked_os_tids_with_deferred_jit_chain`]: an owner that is not
    /// dropped, and no blocked entry on the tid without the cover.
    pub(crate) fn blocked_os_tids_with_covered_jit_chain(&self) -> Vec<u32> {
        let exempt_on = self.ffm_in_native_no_signal;
        let threads = self.threads.read();
        let mut rows: Vec<(u32, bool)> = Vec::new();
        for e in threads.values() {
            if !(entry_is_stw_live(e)
                && e.mounted.load(Ordering::Acquire)
                && e.gc_block_state.in_blocked_region.load(Ordering::Acquire))
            {
                continue;
            }
            if exempt_on && ffm_in_native_signal_exempt(&e.gc_block_state) {
                continue;
            }
            let tid = e.os_tid.load(Ordering::Acquire);
            if tid == 0 {
                continue;
            }
            let covered = deferred_chain_row(
                e.gc_block_state.owner_dropped(),
                e.gc_block_state
                    .deposit_walked_jit_chain
                    .load(Ordering::Acquire)
                    || e.gc_block_state.deferred_jit_chain.lock().is_some(),
            );
            rows.push((tid, covered));
        }
        drop(threads);
        deferred_chain_peer_tids(rows)
    }

    pub(crate) fn blocked_os_tids_with_deferred_jit_chain(&self) -> Vec<u32> {
        let exempt_on = self.ffm_in_native_no_signal;
        let threads = self.threads.read();
        let mut rows: Vec<(u32, bool)> = Vec::new();
        for e in threads.values() {
            if !(entry_is_stw_live(e)
                && e.mounted.load(Ordering::Acquire)
                && e.gc_block_state.in_blocked_region.load(Ordering::Acquire))
            {
                continue;
            }
            // JIT round 16 wave 9b (lane ffm): see `blocked_os_tids`.
            if exempt_on && ffm_in_native_signal_exempt(&e.gc_block_state) {
                continue;
            }
            let tid = e.os_tid.load(Ordering::Acquire);
            if tid == 0 {
                continue;
            }
            // gci i24/jp: the resolve's own filter (`owner_dropped`), so a
            // demoted tid is always one `resolve_deferred_blocked_jit_chains`
            // walks this pause (`deferred_chain_row`).
            let deferred = deferred_chain_row(
                e.gc_block_state.owner_dropped(),
                e.gc_block_state.deferred_jit_chain.lock().is_some(),
            );
            rows.push((tid, deferred));
        }
        drop(threads);
        deferred_chain_peer_tids(rows)
    }

    /// Mark a VM-registered native carrier thread as parked in host-native code.
    ///
    /// These threads do not own interpreter frames while parked, so their
    /// authoritative root snapshot is empty. The flag still matters: moving-GC
    /// fixups and STW diagnostics use it to classify the thread as blocked.
    pub fn mark_native_thread_blocked(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.root_snapshot.lock().clear();
            entry.frame_trace.lock().clear();
            // No deposit took the thread's frame classes for this block: the
            // stale-frame census must count it for every class (interpreter
            // round i1 wave 24, lane L3). Before the flag goes up.
            entry.gc_block_state.blocked_frame_classes.lock().exact = false;
            entry
                .gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
    }

    /// Whether a live registered thread is in a GC-safe blocking region.
    ///
    /// The same transition is the authoritative source for Java
    /// `Thread.State.WAITING`: it is published before monitor/AQS/native waits
    /// and cleared only after the thread wakes and applies post-GC fixups.
    pub fn is_blocked(&self, thread_id: ThreadId) -> bool {
        let threads = self.threads.read();
        threads.get(&thread_id).is_some_and(|entry| {
            entry.alive.load(Ordering::Acquire)
                && entry
                    .gc_block_state
                    .in_blocked_region
                    .load(Ordering::Acquire)
        })
    }

    /// The monitor `thread_id` contends for, as JVMTI
    /// `GetCurrentContendedMonitor` and JDWP `CurrentContendedMonitor` answer
    /// it: the monitor a `monitorenter` (or a synchronized method's entry)
    /// contends for, else -- while the thread is BLOCKED -- the monitor its
    /// wait stands on, which a notified waiter re-entering it contends for
    /// (JDK-8256314's rule; interpreter round i2 waves 55 and 56, lane L6).
    /// `None` for an unknown thread or no contention.
    pub fn current_contended_monitor(&self, thread_id: ThreadId) -> Option<ObjectRef> {
        let (contended, waiting, ..) = self.jmx_lock_snapshot(thread_id)?;
        contended.or_else(|| waiting.filter(|_| self.java_block_state(thread_id) == 2))
    }

    /// The Java blocking state for a currently blocked thread: 1 is WAITING,
    /// 2 is BLOCKED, 3 is TIMED_WAITING. Returns 0 for running, dead, and
    /// unknown threads.
    ///
    /// A thread that has published a contended monitor entry
    /// (`set_jmx_contended_monitor`) and not completed it is BLOCKED even
    /// while it is still a running mutator: the opt-in running park
    /// (`CRATONVM_MONITOR_LAZY_PARK_US`, `vm_exec::lazy_monitor_park`) waits
    /// for the monitor there without entering the GC-blocked region, and read
    /// RUNNABLE for its whole park (round 12 wave 1, lane lock). HotSpot too
    /// reports BLOCKED from the moment a contended enter starts waiting.
    pub fn java_block_state(&self, thread_id: ThreadId) -> u8 {
        let threads = self.threads.read();
        let Some(entry) = threads
            .get(&thread_id)
            .filter(|entry| entry.alive.load(Ordering::Acquire))
        else {
            return 0;
        };
        self.java_block_state_of_entry(entry)
    }

    /// The C JVMTI table's `GetThreadState` facts of thread `thread_id` from
    /// ONE `threads` read (interpreter round i6 wave 132, lane L1; the first
    /// "next step" of i130-L1b, `CRATONVM_JVMTI_THREAD_STATE_ONE_READ`,
    /// performance, default off): `(java_block_state, published wait kind,
    /// interrupted)`, each exactly what [`Self::java_block_state`],
    /// [`Self::published_wait_kind_of`] and [`Self::get_interrupted_flag`]
    /// answer, where the three took the lock three times and the last cloned
    /// and dropped the flag's `Arc` (two RMWs). The wait kind is `None` when
    /// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` is off, or when the thread is not
    /// waiting (block state other than 1 / 3): the caller asks it only then.
    /// The three are read under one lock hold rather than three, so a state
    /// that changes meanwhile is seen at one instant instead of three; every
    /// answer remains one the three separate reads could have given.
    pub fn jvmti_state_facts(&self, thread_id: ThreadId) -> (u8, Option<PublishedWaitKind>, bool) {
        self.jvmti_state_facts_and_suspend_word(thread_id).0
    }

    /// [`Self::jvmti_state_facts`] and, under the same `threads` read, the
    /// thread's C JVMTI state suspend word (`GcBlockState::jvmti_state_suspended`;
    /// interpreter round i7 wave 135, lane L1, `CRATONVM_JVMTI_STATE_SUSPEND_WORD`).
    /// The word is `JVMTI_STATE_SUSPEND_UNPUBLISHED` for an unknown thread.
    pub fn jvmti_state_facts_and_suspend_word(
        &self,
        thread_id: ThreadId,
    ) -> ((u8, Option<PublishedWaitKind>, bool), u8) {
        let threads = self.threads.read();
        let Some(entry) = threads.get(&thread_id) else {
            return (
                (0, None, false),
                crate::threading::jvm_thread::JVMTI_STATE_SUSPEND_UNPUBLISHED,
            );
        };
        let word = entry.gc_block_state.jvmti_state_suspended.load(Ordering::Acquire);
        let interrupted = entry.interrupted.load(Ordering::Acquire);
        if !entry.alive.load(Ordering::Acquire) {
            // `java_block_state` answers 0 (RUNNABLE): no wait kind is asked.
            return ((0, None, interrupted), word);
        }
        let block = self.java_block_state_of_entry(entry);
        let kind = (self.published_all_wait_kinds && matches!(block, 1 | 3))
            .then(|| gc_block_state_wait_kind(&entry.gc_block_state));
        ((block, kind, interrupted), word)
    }

    /// Thread `thread_id`'s C JVMTI state suspend word alone, one `threads`
    /// read and no `Arc` clone (interpreter round i7 wave 135, lane L1;
    /// [`Self::jvmti_state_facts_and_suspend_word`] without the facts).
    pub fn jvmti_state_suspend_word(&self, thread_id: ThreadId) -> u8 {
        self.threads.read().get(&thread_id).map_or(
            crate::threading::jvm_thread::JVMTI_STATE_SUSPEND_UNPUBLISHED,
            |entry| entry.gc_block_state.jvmti_state_suspended.load(Ordering::Acquire),
        )
    }

    /// [`Self::java_block_state`] of a live entry, under the caller's
    /// `threads` read (wave 132: shared with [`Self::jvmti_state_facts`]).
    pub(in crate::threading::thread_registry) fn java_block_state_of_entry(&self, entry: &ThreadEntry) -> u8 {
        if entry
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
        {
            // A native blocked in C (wave 59, lane L6) is RUNNABLE.
            return match entry.gc_block_state.java_state.load(Ordering::Acquire) {
                crate::threading::jvm_thread::JAVA_STATE_IN_NATIVE => 0,
                state => state,
            };
        }
        if entry.jmx_monitors.contending.load(Ordering::Acquire)
            && entry.jmx_monitors.contended_monitor.lock().is_some()
        {
            return 2;
        }
        // A thread inside `Object.wait` that is not (yet, or no longer)
        // GC-blocked: the release-first wait publishes WAITING /
        // TIMED_WAITING to `holder.threadStatus` before it releases the
        // monitor, then parks as a running mutator for up to the lazy-park
        // budget (`CRATONVM_MONITOR_WAIT_LAZY_PARK`) before it deposits. Its
        // waiting monitor is published for that whole span (taken before the
        // wait restores RUNNABLE), and `java_state` was stored before it, so
        // the wait's own state (2 while it re-enters) answers here. Under
        // `--real-jdk` the JDK's `Thread.getState()` reads the mirror's word
        // and said WAITING while JVMTI and JDWP, which read this, said
        // RUNNABLE (interpreter round i2 wave 57, lane L1; probe
        // `L1W57JvmtiWaitStates`).
        //
        // Interpreter round i2 wave 64, lane L1 (`CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
        // the wait's published kind spans exactly the waiting monitor's span
        // (stored before it, lowered after it is taken; `vm_exec::monitor_wait_with_state`),
        // so one atomic load answers where the monitor's mutex was taken for
        // every RUNNABLE thread asked.
        if self.published_all_wait_kinds {
            // Wave 65 (lane L1, `CRATONVM_SERVED_WAIT_KINDS`): a served
            // stand-in's monitor wait for JDK code that parks spans the same
            // lazy phase.
            let kind = entry.gc_block_state.java_wait_kind.load(Ordering::Acquire);
            if matches!(
                kind,
                crate::threading::jvm_thread::JAVA_WAIT_KIND_OBJECT_WAIT
                    | crate::threading::jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED
            )
                // Interpreter round i3 wave 71, lane L1 (`CRATONVM_PARK_LAZY`):
                // a park's running phase publishes its state and kind first.
                || (self.park_lazy_budget.is_some()
                    && kind == crate::threading::jvm_thread::JAVA_WAIT_KIND_PARKED)
            {
                let state = entry.gc_block_state.java_state.load(Ordering::Acquire);
                if matches!(state, 1..=3) {
                    return state;
                }
            }
        } else if entry.jmx_waiting_monitor.lock().is_some() {
            let state = entry.gc_block_state.java_state.load(Ordering::Acquire);
            if matches!(state, 1..=3) {
                return state;
            }
        }
        0
    }

    /// Is live thread `thread_id` blocked in a registered native that stands
    /// in for a JDK `native` method blocking in C -- a `BlockKind::InNative`
    /// region, `jvm_thread::JAVA_STATE_IN_NATIVE` (interpreter round i2 wave
    /// 59, lane L6, `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE`)? Such a thread is
    /// RUNNABLE ([`Self::java_block_state`] answers 0) and in native.
    pub fn blocked_in_native(&self, thread_id: ThreadId) -> bool {
        self.gc_block_state_of(thread_id)
            .is_some_and(|block| gc_block_state_blocked_in_native(&block))
    }

    /// Does live thread `thread_id` run native code with no Java above it, as
    /// HotSpot's `_thread_in_native` (JMX `ThreadInfo.isInNative()`, the C
    /// JVMTI table's `IN_NATIVE`)? A native blocked in C
    /// ([`Self::blocked_in_native`]), a JNI native's C code under the C JVMTI
    /// table's mark (`GcBlockState::jvmti_in_native`), a GC-safe FFM downcall
    /// in its in-native region, or a lazy FFM downcall. The one place the
    /// in-native readers ask (interpreter round i2 wave 59, lane L6).
    pub fn thread_in_native(&self, thread_id: ThreadId) -> bool {
        let Some(block) = self.gc_block_state_of(thread_id) else {
            return false;
        };
        block.jvmti_in_native.load(Ordering::Acquire)
            || (block.foreign_native.load(Ordering::Acquire)
                && block.in_blocked_region.load(Ordering::Acquire))
            || block.lazy_native.load(Ordering::Acquire)
                != crate::threading::jvm_thread::LAZY_NATIVE_OFF
            || gc_block_state_blocked_in_native(&block)
    }

    /// Is live thread `thread_id` inside `Thread.sleep`, as its own blocking
    /// region published it (a `BlockKind::Sleeping` region:
    /// `GcBlockState::java_wait_kind` is `jvm_thread::JAVA_WAIT_KIND_SLEEPING`
    /// under a TIMED_WAITING `java_state` while the flag is up)? Interpreter
    /// round i2 wave 63, lane L1, stage 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`.
    ///
    /// `None` without `CRATONVM_PUBLISHED_WAIT_KIND`: the kind is not
    /// published, and the reader asks the deposited top frame as before
    /// (`debug::is_thread_sleep_frame`, `debug::frame_invokes_thread_sleep`).
    /// With it, the answer is authoritative: the JVMTI `SLEEPING` bit
    /// (`jvmti::native_env::wait_kind`) and JDWP's `SLEEPING`
    /// (`debug::jdwp_thread_status_with`) read it whatever frame the sleep
    /// was entered from (a reflective `Method.invoke`, a method handle,
    /// `TimeUnit.sleep`), where the frame test read `PARKED` / `WAIT`.
    pub fn published_sleeping(&self, thread_id: ThreadId) -> Option<bool> {
        if !self.published_wait_kind() {
            return None;
        }
        let threads = self.threads.read();
        let Some(entry) = threads
            .get(&thread_id)
            .filter(|entry| entry.alive.load(Ordering::Acquire))
        else {
            return Some(false);
        };
        Some(gc_block_state_sleeping(&entry.gc_block_state))
    }

    /// The kind of live thread `thread_id`'s wait, as the wait itself
    /// published it in `GcBlockState::java_wait_kind` (interpreter round i2
    /// wave 64, lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`; the proposal's
    /// item 3 in
    /// `docs/feature-designs/real-jdk-mode.md`:
    /// one function the kind readers ask).
    ///
    /// `None` without the switch: the reader assembles the kind as before
    /// (the JMX waiting monitor, [`Self::published_sleeping`], the deposited
    /// top frame). With it the answer is authoritative and takes no
    /// per-thread mutex: every `Object.wait` publishes
    /// [`PublishedWaitKind::ObjectWait`] before its waiting monitor, so a
    /// [`PublishedWaitKind::Unrecorded`] wait is not one. Asked only of a
    /// thread whose [`Self::java_block_state`] is WAITING / TIMED_WAITING;
    /// an unknown or dead thread is `Unrecorded`.
    pub fn published_wait_kind_of(&self, thread_id: ThreadId) -> Option<PublishedWaitKind> {
        if !self.published_all_wait_kinds {
            return None;
        }
        let threads = self.threads.read();
        let Some(entry) = threads
            .get(&thread_id)
            .filter(|entry| entry.alive.load(Ordering::Acquire))
        else {
            return Some(PublishedWaitKind::Unrecorded);
        };
        Some(gc_block_state_wait_kind(&entry.gc_block_state))
    }

    /// The kind of WAITING / TIMED_WAITING thread `thread_id`'s wait as a
    /// thread dump prints it (interpreter round i3 wave 79, lane L6; page
    /// `i63-L1-thread-dump-has-no-wait-kind-suffix`): the JVMTI reader's
    /// order (`jvmti::native_env::wait_kind`) without its deposited-frame
    /// fallback -- the published word with `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`;
    /// otherwise a served stand-in's parked monitor wait, the JMX waiting
    /// monitor (`Object.wait`), the published sleep. Anything else, an
    /// unrecorded wait included, is [`PublishedWaitKind::Parked`], as JVMTI
    /// reads it `PARKED`. Never [`PublishedWaitKind::Unrecorded`].
    pub fn dump_wait_kind(&self, thread_id: ThreadId) -> PublishedWaitKind {
        match self.published_wait_kind_of(thread_id) {
            Some(PublishedWaitKind::Unrecorded) => return PublishedWaitKind::Parked,
            Some(kind) => return kind,
            None => {}
        }
        if self.published_monitor_parked(thread_id) {
            return PublishedWaitKind::Parked;
        }
        if self.peek_jmx_waiting_monitor(thread_id).is_some() {
            return PublishedWaitKind::ObjectWait;
        }
        if self.published_sleeping(thread_id) == Some(true) {
            return PublishedWaitKind::Sleeping;
        }
        PublishedWaitKind::Parked
    }

    /// Is live thread `thread_id` inside a native it entered through the
    /// native-call funnel, outside a blocking region (its
    /// `GcBlockState::native_excuse` word raised; interpreter round i2 wave
    /// 60, lane L6)? Raised for every registered native the funnel runs, so a
    /// reader that means HotSpot's `_thread_in_native` also checks that the
    /// method is a `native` one (`thread_jmx_snapshot`: the trace's top row).
    pub fn running_funnel_native(&self, thread_id: ThreadId) -> bool {
        self.gc_block_state_of(thread_id).is_some_and(|block| {
            block.native_excuse.load() & crate::threading::jvm_thread::NX_IN != 0
                && !block.in_blocked_region.load(Ordering::Acquire)
        })
    }

    /// The callback of the native live thread `thread_id` runs through the
    /// native-call funnel outside a blocking region, as its raised
    /// native-excuse word records it (interpreter round i2 wave 62, lane L6,
    /// `CRATONVM_HANDSHAKE_NATIVE_CALLBACK`): `None` when it runs none
    /// ([`Self::running_funnel_native`] is false) or nothing was recorded (the
    /// switch is off). The callback is that native's or, if the thread left it
    /// meanwhile, a later one's.
    pub fn running_funnel_callback(&self, thread_id: ThreadId) -> Option<usize> {
        let block = self.gc_block_state_of(thread_id)?;
        if block.native_excuse.load() & crate::threading::jvm_thread::NX_IN == 0
            || block.in_blocked_region.load(Ordering::Acquire)
        {
            return None;
        }
        Some(block.native_excuse.callback()).filter(|&cb| cb != 0)
    }

    /// Live thread `thread_id`'s native-excuse word as it stands (0 for no
    /// thread), for diagnostics (interpreter round i2 wave 61, lane L6).
    pub fn native_excuse_word(&self, thread_id: ThreadId) -> usize {
        self.gc_block_state_of(thread_id)
            .map_or(0, |block| block.native_excuse.load())
    }

    /// The callback live thread `thread_id` runs through the native-call
    /// funnel, as it recorded it (`GcBlockState::funnel_native_callback`;
    /// interpreter round i2 wave 61, lane L6): `None` for no thread or no
    /// record (none is kept unless a debugger is attached or a C JVMTI env is
    /// bound with `CRATONVM_JVMTI_C_RUNNING_NATIVE` on).
    pub fn funnel_native_callback(&self, thread_id: ThreadId) -> Option<usize> {
        self.gc_block_state_of(thread_id)
            .map(|block| block.funnel_native_callback.load(Ordering::Acquire))
            .filter(|&cb| cb != 0)
    }

    /// Clear the GC-blocked mark for a VM-registered native carrier thread.
    pub fn mark_native_thread_unblocked(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            {
                let mut f = entry.gc_block_state.fixup.lock();
                if !f.is_empty()
                    && false
                {
                    eprintln!(
                        "[blockgc] native-unblock DISCARDS {} fixups tid={}",
                        f.len(),
                        thread_id.0,
                    );
                }
                f.clear();
            }
            entry.gc_block_state.slot_origins.lock().clear();
            entry.root_snapshot.lock().clear();
            entry
                .gc_block_state
                .in_blocked_region
                .store(false, Ordering::Release);
        }
    }

    /// CENSUS-RECONCILE — the shared [`GcBlockState`] published for
    /// `thread_id`.
    ///
    /// This is the SAME `Arc` the owning `JvmThread` holds (installed by
    /// [`Self::set_gc_block_state`]), so a caller that cannot reach the
    /// `JvmThread` itself can still hand the *authoritative*
    /// `in_blocked_region` flag to
    /// [`crate::threading::gc_barrier::GcBarrier::leave_blocked_region_flagged`]
    /// rather than clearing it with a bare `store(false)` (finding 1(c): a
    /// bare store lets a pause requested between the drain and the store both
    /// EXCLUDE the thread and let it run).
    ///
    /// `jni::host_thread_leave_native` is the motivating caller: it runs on a
    /// host OS thread that has no `JNI_THREAD` binding, so `with_jni_context`
    /// — every other path's route to the flag — resolves to `None` there.
    ///
    /// The `Arc` is CLONED out rather than borrowed on purpose: the registry
    /// lock must be released before the caller can park inside the barrier.
    /// Holding it across a pause drain would queue every later census behind a
    /// pending registry writer.
    pub fn gc_block_state_of(&self, thread_id: ThreadId) -> Option<Arc<GcBlockState>> {
        self.threads
            .read()
            .get(&thread_id)
            .map(|entry| entry.gc_block_state.clone())
    }

    /// CENSUS-RECONCILE — the registered, alive `ThreadId` whose published
    /// `os_tid` is the CALLING OS thread's.
    ///
    /// The identity census (`alive_count_blocked_and_os_tids`) keys the STW
    /// exclusion set on `ThreadId`, so a host-facing entry point that wants to
    /// be *excluded* must name itself. A host thread parked outside the VM has
    /// no `JvmThread` borrow and no JNI TLS binding, but its carrier's
    /// `os_tid` was published by [`Self::set_os_tid_current`] at registration
    /// — which makes the OS id the only identity link that survives leaving
    /// the VM.
    ///
    /// More than one alive entry can claim this OS thread: a virtual thread
    /// mounted on a carrier republishes the carrier's `os_tid` under its own
    /// id at every mount (`vm_exec.rs::resume_virtual_continuation`), and no
    /// unmount clears it, so every parked virtual thread that last ran here
    /// still claims it. The answer is then the MOUNTED one: the claimant
    /// whose [`Self::set_os_tid_current`] on this OS thread is the newest
    /// (gc-common w6-g, `common-w3a-jni-unbound-counted-monitor-enter`
    /// residual 2). That is sound because the question is only ever asked by
    /// code running on this OS thread, and a mount publishes before it runs
    /// any Java, so the running identity's publish is the last one made here.
    /// A claimant that is concurrently republishing on ANOTHER carrier is
    /// filtered by re-reading its `os_tid` after its ordinal (the writer
    /// stores `os_tid` first).
    ///
    /// Returns `None` when the answer is unknown:
    ///
    /// * no alive entry claims this OS thread (a genuinely foreign host
    ///   thread — it is not in `alive_count` either, so it occupies no
    ///   `expected` slot and needs no exclusion),
    /// * several claim it and no single newest publish names one (only
    ///   possible for entries whose `os_tid` was stored without
    ///   `set_os_tid_current`), or
    /// * the platform has no `os_tid` backend at all (`current_os_tid() == 0`).
    ///
    /// `None` is always the safe answer: the caller then falls back to the
    /// anonymous-counter-only behaviour, which is what every such call did
    /// before this existed.
    pub fn thread_id_for_current_os_tid(&self) -> Option<ThreadId> {
        self.thread_id_for_os_tid_inner(current_os_tid())
    }

    /// [`Self::thread_id_for_current_os_tid`] for an explicit OS tid, split
    /// out so the claimant choice is testable without extra OS threads.
    ///
    /// gcj j3/rt3 (j2-pn2 finding 11, the twin of
    /// [`newest_claimants_by_os_tid`]'s rule): an UNMOUNTED entry -- a parked
    /// virtual thread -- claims nothing. Its mount published the carrier's tid
    /// under a NEWER ordinal than the carrier's own entry, and nothing
    /// republishes the carrier at unmount, so after the unmount the parked
    /// continuation stayed the newest claimant and a carrier asking its own
    /// identity (an unbound JNI `MonitorEnter`, the barrier identity of
    /// `jni_caller_thread_id`) was answered with the parked thread: a monitor
    /// owned under, or a pause excusing, a thread that runs nowhere. The
    /// mounted bit is lowered before the yield's deposit and raised at every
    /// mount next to its publish, so the identity running here is never the
    /// one skipped. Every backend: thread identity is not a collector's
    /// (`CRATONVM_OS_TID_IDENTITY_SKIPS_UNMOUNTED=0` kills, read per registry).
    pub(in crate::threading::thread_registry) fn thread_id_for_os_tid_inner(&self, os_tid: u32) -> Option<ThreadId> {
        if os_tid == 0 {
            return None;
        }
        let skip_unmounted = self.os_tid_identity_skips_unmounted;
        let threads = self.threads.read();
        let mut claims: Vec<(ThreadId, u64)> = Vec::new();
        for (tid, e) in threads.iter() {
            if !e.alive.load(Ordering::Acquire) || e.os_tid.load(Ordering::Acquire) != os_tid {
                continue;
            }
            if !os_tid_claim_admitted(os_tid, e.mounted.load(Ordering::Acquire), skip_unmounted) {
                continue;
            }
            let seq = e.os_tid_seq.load(Ordering::Acquire);
            // Re-read after the ordinal: a claimant that moved to another
            // carrier stored its new `os_tid` before its new ordinal, so if
            // the ordinal above is the new one this read is no longer ours.
            if e.os_tid.load(Ordering::Acquire) != os_tid {
                continue;
            }
            claims.push((*tid, seq));
        }
        newest_os_tid_claimant(&claims)
    }

    /// gcf f2/j: does an ALIVE entry claim the calling OS thread (its
    /// published `os_tid`, [`Self::set_os_tid_current`], which every VM thread
    /// runs at startup and every mount republishes)? `None` when this
    /// platform has no OS-tid backend, i.e. the answer is unknown.
    ///
    /// Unlike [`Self::thread_id_for_current_os_tid`] it does not pick ONE
    /// claimant, so several claims (a carrier's parked virtual threads) still
    /// answer `Some(true)`. The JNI invocation interface's "is this OS thread
    /// attached?" (`jni_get_env`'s `JNI_EDETACHED`, `AttachCurrentThread`'s
    /// already-attached test) reads it after the cheaper thread-local tests.
    /// An idle foreign attachment withdraws its tid (`clear_os_tid`); the JNI
    /// side answers for it from its own attachment record first.
    ///
    /// Deliberately NOT [`Self::thread_id_for_os_tid_inner`]'s unmounted rule
    /// (gcj j3/rt3 residual 2): this asks "is this OS thread attached?", not
    /// "which identity runs here?". Every carrier that ever mounted a virtual
    /// thread is itself a registered, alive entry with its own tid, so
    /// counting a parked continuation's stale claim cannot change the answer;
    /// it could only for an unregistered OS thread with a continuation
    /// mounted on it, which the VM never builds. Copy the skip here only with
    /// such a carrier and a test that tells the two answers apart.
    pub fn current_os_tid_claimed(&self) -> Option<bool> {
        let os_tid = current_os_tid();
        if os_tid == 0 {
            return None;
        }
        let threads = self.threads.read();
        Some(
            threads.values().any(|e| {
                e.alive.load(Ordering::Acquire) && e.os_tid.load(Ordering::Acquire) == os_tid
            }),
        )
    }

    /// The published `JvmThread` address of one alive entry (`None` when the
    /// entry is unknown, dead, or has not published it; see
    /// [`Self::set_jvm_thread_addr`]).
    ///
    /// Only the thread that owns the entry may dereference the answer. A
    /// published address stays valid while its own thread runs, and no other
    /// thread's `JvmThread` is safe to touch from here (the take-over has
    /// its own frozen-peer contract, [`Self::frozen_peer_thread_addrs`]).
    pub fn own_jvm_thread_addr(&self, thread_id: ThreadId) -> Option<usize> {
        let threads = self.threads.read();
        let entry = threads.get(&thread_id)?;
        if !entry.alive.load(Ordering::Acquire) {
            return None;
        }
        Some(entry.jvm_thread_addr.load(Ordering::Acquire)).filter(|a| *a != 0)
    }

    /// Whether `thread_id` is currently alive and inside a GC-blocked
    /// safepoint region (`gc_block_state.in_blocked_region == true`).
    pub fn is_tid_in_blocked_region(&self, thread_id: ThreadId) -> bool {
        let threads = self.threads.read();
        threads.get(&thread_id).is_some_and(|entry| {
            entry.alive.load(Ordering::Acquire)
                && entry.gc_block_state.in_blocked_region.load(Ordering::Acquire)
        })
    }

    /// DBG (CRATONVM_DBG_MTROOTS): per-thread (tid, in_blocked_region,
    /// snapshot_len) for every alive thread. Used at an STW to see whether a
    /// thread that holds a reclaimed live oop was counted BLOCKED (excluded from
    /// the barrier `expected`) while actually running — the multi-thread
    /// root-coverage gap.
    pub fn dump_blocked_states(&self) -> Vec<(u64, bool, usize)> {
        let threads = self.threads.read();
        let mut v = Vec::new();
        for (tid, entry) in threads.iter() {
            if entry.alive.load(Ordering::Acquire) {
                let blk = entry
                    .gc_block_state
                    .in_blocked_region
                    .load(Ordering::Acquire);
                let len = entry.root_snapshot.lock().len();
                v.push((tid.0, blk, len));
            }
        }
        v
    }

    /// Debug-only STW census: one line per alive JVM thread with the state that
    /// matters to stop-the-world accounting. Used when the GC barrier is waiting
    /// for a cooperative mutator but the cross-thread JIT takeover cannot find a
    /// live JIT frame to freeze.
    pub fn debug_thread_census(&self) -> String {
        use std::fmt::Write as _;

        let threads = self.threads.read();
        let mut out = String::new();
        for (tid, entry) in threads.iter() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }

            let blocked = entry
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire);
            let stw_ready = entry.stw_ready.load(Ordering::Acquire);
            let snapshot_len = entry.root_snapshot.lock().len();
            let os_tid = entry.os_tid.load(Ordering::Acquire);
            let vm_state = {
                let s = entry.vm_state.lock();
                if s.is_empty() {
                    "<unset>".to_string()
                } else {
                    s.clone()
                }
            };
            let trace = entry.frame_trace.lock();
            let mut top = String::new();
            for (i, frame) in trace.iter().rev().take(4).enumerate() {
                if i > 0 {
                    top.push_str(" <- ");
                }
                let _ = write!(
                    top,
                    "{}.{}@{}",
                    frame.class_name, frame.method_name, frame.byte_code_index
                );
            }
            if top.is_empty() {
                top.push_str("<no-frame-trace>");
            }
            // Same `deposit=` tag as the T19.H1 summary, for the same reason:
            // this census is alive-only, but "alive" does not mean "parked at
            // the frames below" — a mutator the barrier is still waiting on is
            // by definition RUNNING, and its chain is last-block history.
            // See `ThreadRegistry::deposit_freshness`.
            let _ = write!(
                out,
                "\n  t{} os_tid={} name={:?} blocked={} ready={} snapshot={} state={:?} deposit={} top={}",
                tid.0,
                os_tid,
                entry.name,
                blocked,
                stw_ready,
                snapshot_len,
                vm_state,
                Self::deposit_freshness(true, blocked),
                top
            );
        }
        out
    }

    /// JIT round 18 wave 10 (lane ffmeintr): the `CRATONVM_DBG_STW_WATCHDOG_SECS`
    /// dump, one line per registered thread (alive or not), for a pause whose
    /// wait has outlived the watchdog: every state a stop-the-world wait can
    /// be stuck on, so a silent hang names the thread it waits for.
    ///
    /// Built to be safe from INSIDE a stuck pause: the registry read lock (a
    /// pause already takes it every round), atomics, and `try_lock` on the
    /// two per-thread mutexes (a frozen, claimed or blocked peer may hold
    /// them), never `lock`. `counted` / `taken` are the pause's census and
    /// frozen-or-claimed OS tids; `peer_os` answers the OS side of a tid
    /// (`xt_root_scan::debug_peer_os_state`: kernel state, syscall, take-over
    /// slot).
    pub fn debug_stw_watchdog_census(
        &self,
        counted: &[u32],
        taken: &[u32],
        peer_os: &dyn Fn(u32) -> String,
    ) -> String {
        use std::fmt::Write as _;
        let threads = self.threads.read();
        let mut out = String::new();
        for (tid, entry) in threads.iter() {
            let block = &entry.gc_block_state;
            let os_tid = entry.os_tid.load(Ordering::Acquire);
            let lazy = match block.lazy_native.load(Ordering::Acquire) {
                crate::threading::jvm_thread::LAZY_NATIVE_OFF => "off",
                crate::threading::jvm_thread::LAZY_NATIVE_IN => "IN",
                crate::threading::jvm_thread::LAZY_NATIVE_CLAIMED => "CLAIMED",
                crate::threading::jvm_thread::LAZY_NATIVE_CLAIMED_LEAVING => "CLAIMED_LEAVING",
                crate::threading::jvm_thread::LAZY_NATIVE_SIGNALLING => "SIGNALLING",
                _ => "?",
            };
            let vm_state = entry
                .vm_state
                .try_lock()
                .map(|s| {
                    if s.is_empty() {
                        "<unset>".to_string()
                    } else {
                        s.clone()
                    }
                })
                .unwrap_or_else(|| "<locked>".to_string());
            let top = match entry.frame_trace.try_lock() {
                Some(trace) => trace
                    .iter()
                    .rev()
                    .take(3)
                    .map(|f| format!("{}.{}@{}", f.class_name, f.method_name, f.byte_code_index))
                    .collect::<Vec<_>>()
                    .join(" <- "),
                None => "<locked>".to_string(),
            };
            let _ = write!(
                out,
                "\n  t{} os_tid={} name={:?} alive={} ready={} mounted={} counted={} taken={} \
                 blocked={} java_state={} foreign_native={} lazy_native={} handshake_pending={} \
                 vm_state={:?} last_trace=[{}] os=[{}]",
                tid.0,
                os_tid,
                entry.name,
                entry.alive.load(Ordering::Acquire),
                entry.stw_ready.load(Ordering::Acquire),
                entry.mounted.load(Ordering::Acquire),
                os_tid != 0 && counted.contains(&os_tid),
                os_tid != 0 && taken.contains(&os_tid),
                block.in_blocked_region.load(Ordering::Acquire),
                block.java_state.load(Ordering::Acquire),
                block.foreign_native.load(Ordering::Acquire),
                lazy,
                block.handshakes.is_pending(),
                vm_state,
                top,
                if os_tid != 0 {
                    peer_os(os_tid)
                } else {
                    String::new()
                },
            );
        }
        out
    }

    /// Collect root snapshots from all alive threads.
    /// Returns a combined vector of all ObjectRefs from all threads' snapshots.
    pub fn collect_all_root_snapshots(&self) -> Vec<ObjectRef> {
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("collect_all_root_snapshots");
        let len = || self.threads.read().len();
        PERF.time(len, || {
            // gci i13/vg: a split the collection door armed for this gather
            // (`arm_parked_split_gather`); consumed here, its old-only
            // section left for `take_parked_old_only`.
            //
            // gci i21/pr (`CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`, opt-in): the
            // initiator's own snapshot the door armed for this gather
            // (`arm_initiator_snapshot_skip`); consumed here, with the split.
            //
            // gcj j3/x2: taken only on the arming thread (see the arm).
            let (armed, initiator_skip) = {
                let mut s = self.parked_split.lock();
                let mine = match s.initiator_skip.as_ref() {
                    Some((_, owner)) => *owner == std::thread::current().id(),
                    None => false,
                };
                let skip = if mine {
                    s.initiator_skip.take().map(|(snapshot, _)| snapshot)
                } else {
                    if s.initiator_skip.is_some() {
                        s.initiator_foreign_gathers += 1;
                    }
                    None
                };
                // gcj j3/x2 R10: the split, by the same owner rule.
                let split_mine = s
                    .armed
                    .as_ref()
                    .is_some_and(|(_, owner)| *owner == std::thread::current().id());
                let armed = if split_mine {
                    s.armed.take().map(|(filter, _)| filter)
                } else {
                    if s.armed.is_some() {
                        s.split_foreign_gathers += 1;
                    }
                    None
                };
                (armed, skip)
            };
            let mut initiator_skipped = 0usize;
            let skip = initiator_skip.as_ref();
            // gci i14/ve: a mirror-keyed request the door armed for this
            // gather (`arm_mirror_keyed_gather`); consumed here.
            let keyed_arm = self.mirror_keyed.lock().armed.take();
            let mut keyed_out = MirrorKeyedOut::default();
            let mut keyed: Option<(&MirrorKeyedArm, &mut MirrorKeyedOut)> = None;
            if let Some(arm) = keyed_arm.as_ref() {
                keyed = Some((arm, &mut keyed_out));
            }
            let roots = match armed {
                Some(young) => {
                    let mut old_only: Vec<ObjectRef> = Vec::new();
                    let roots = self.collect_all_root_snapshots_inner(
                        Some((&young, &mut old_only)),
                        keyed,
                        skip,
                        &mut initiator_skipped,
                    );
                    self.parked_split.lock().old_only = old_only;
                    roots
                }
                None => self.collect_all_root_snapshots_inner(
                    None,
                    keyed,
                    skip,
                    &mut initiator_skipped,
                ),
            };
            if initiator_skip.is_some() {
                self.parked_split.lock().initiator_skipped = Some(initiator_skipped);
            }
            if let Some(arm) = keyed_arm.as_ref() {
                self.publish_mirror_keyed(arm, keyed_out);
            }
            roots
        })
    }

    /// gci i14/ve: file the rows of the entries a mirror-keyed gather left
    /// out -- `(mirror, word)` for every snapshot word, this VM's
    /// `metadata_pin` rows, which every Generational marker follows from a
    /// marked owner -- and remember `(tid, mirror)` for the same pause's
    /// verdict. The rows are added AFTER `collect_roots` step 1 replaced this
    /// VM's rows (the door gathers after it), so they reach this pause's
    /// collector, and the next scan's step 1 drops them. Appends to the
    /// deferred list: the sweep of this pause takes it whole.
    pub(in crate::threading::thread_registry) fn publish_mirror_keyed(&self, arm: &MirrorKeyedArm, out: MirrorKeyedOut) {
        if out.deferred.is_empty() {
            return;
        }
        cratonvm_types::metadata_pin::add_metadata_pins(arm.vm, &out.rows);
        let mut keyed = self.mirror_keyed.lock();
        keyed.deferred.extend(out.deferred);
        // gci i15/vh: a G1 marking scan keyed something this cycle, so the
        // remark closes the rows over live owners
        // (`judge_mirror_keyed_parked_after_mark`).
        if !arm.old_ranges.is_empty() {
            keyed.g1_cycle_keyed = true;
        }
    }

    /// gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`, default ON since 2026-10-17): the gather
    /// of [`Self::collect_all_root_snapshots`], split in two root sections for
    /// a collector that can tell a young-only phase from one that marks or
    /// moves old space. `young` is the collector's young set for THIS pause
    /// (`VmHeap::parked_old_only_young_ranges`: Generational both semispaces,
    /// G1 every region that is not `Old`); it is asked inside the pause, so it
    /// cannot change before the collection.
    ///
    /// Returns `(roots, old_only)`. `old_only` holds the snapshots of the
    /// UNMOUNTED continuations whose cached [`FoldAddrFilter`] (valid for the
    /// entry's current `mount_epoch`, i.e. nothing but a fold has touched its
    /// words since the filter was taken) is disjoint from `young`: none of
    /// their words is a young address. Everything else -- every mounted
    /// entry, every entry without a valid filter, every thread mirror and JMX
    /// monitor root (a `VirtualThread` mirror can be young) -- is in `roots`,
    /// exactly as [`Self::collect_all_root_snapshots`] gathers it.
    ///
    /// Soundness (the page's argument): a young-only collection follows no
    /// root into old space (old -> young edges come from the card table / the
    /// remembered sets, never from a root), and moves and frees nothing old,
    /// so leaving out a root whose every word is outside the young set changes
    /// nothing it does; the snapshot words stay valid because nothing old
    /// moved, and the blocked fold still remaps them on a pause that does move
    /// old space. The caller hands `old_only` to the heap
    /// (`VmHeap::stage_parked_old_only_roots`), which adds it back to the root
    /// set of every phase that marks or moves old space.
    pub fn collect_all_root_snapshots_split(
        &self,
        young: &[(usize, usize)],
    ) -> (Vec<ObjectRef>, Vec<ObjectRef>) {
        let mut young_filter = FoldAddrFilter::default();
        for &(lo, hi) in young {
            young_filter.insert_range(lo, hi);
        }
        let mut old_only: Vec<ObjectRef> = Vec::new();
        let roots = self.collect_all_root_snapshots_inner(
            Some((&young_filter, &mut old_only)),
            None,
            None,
            &mut 0,
        );
        (roots, old_only)
    }

    /// `CRATONVM_DBG_ZGC_WILD`: one `[ZGC-WILD-SNAP]` line per alive thread's
    /// snapshot root that `is_obj` refuses, naming the thread, the index, and
    /// the frame slot it came from when `slot_origins` recorded one. Diagnostic
    /// only; takes the registry read lock and every snapshot lock in turn.
    pub(crate) fn dbg_wild_snapshot_roots(&self, is_obj: &dyn Fn(usize) -> bool) {
        let threads = self.threads.read();
        for (tid, entry) in threads.iter() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }
            let snapshot = entry.root_snapshot.lock();
            for (i, r) in snapshot.iter().enumerate() {
                let a = r.as_ptr() as usize;
                if is_obj(a) {
                    continue;
                }
                let origin = entry
                    .gc_block_state
                    .slot_origins
                    .lock()
                    .iter()
                    .find(|so| so.orig == a || so.cur == a)
                    .map(|so| {
                        format!(
                            "frame#{} {}[{}] live={}",
                            so.frame,
                            if so.is_stack { "stack" } else { "local" },
                            so.idx,
                            so.live
                        )
                    });
                eprintln!(
                    "[ZGC-WILD-SNAP] thread={:?} tid={} idx={i}/{} addr={a:#x} blocked={} \
                     conservative={} origin={:?}",
                    entry.name,
                    tid.0,
                    snapshot.len(),
                    entry
                        .gc_block_state
                        .in_blocked_region
                        .load(Ordering::Acquire),
                    entry
                        .gc_block_state
                        .deposit_conservative
                        .load(Ordering::Acquire),
                    origin
                );
            }
        }
    }

    /// `split` (gci i13/vg, [`Self::collect_all_root_snapshots_split`]): the
    /// collector's young filter for this pause and the OLD-ONLY section that
    /// receives the snapshot of every entry [`parked_snapshot_is_old_only`]
    /// admits. `None`: one section, the historical gather.
    ///
    /// `keyed` (gci i14/ve, [`Self::arm_mirror_keyed_gather`]): the door's
    /// mirror-keyed request and where the entries it admits go
    /// ([`mirror_keyed_mirror`]). Such an entry contributes NOTHING to either
    /// section -- neither its snapshot nor its mirror; its JMX roots stay --
    /// and its words become `(mirror, word)` rows in `out.rows`. It wins over
    /// the old-only split (a keyed entry is in neither section).
    ///
    /// `skip` (gci i21/pr, [`Self::arm_initiator_snapshot_skip`]): the
    /// collection initiator's own `root_snapshot` `Arc`, whose words its
    /// `collect_roots` step 11 already pushed in this pause; `skipped` counts
    /// the words left out. An entry SHARING that `Arc` (`Arc::ptr_eq`) gives
    /// nothing to `all_roots` from its snapshot; everything else about it is
    /// unchanged (its mirror, its JMX roots, the guard's conservative words,
    /// the census, and a keyed or old-only routing, which already kept the
    /// words out of `all_roots`).
    pub(in crate::threading::thread_registry) fn collect_all_root_snapshots_inner(
        &self,
        mut split: Option<(&FoldAddrFilter, &mut Vec<ObjectRef>)>,
        mut keyed: Option<(&MirrorKeyedArm, &mut MirrorKeyedOut)>,
        skip: Option<&Arc<Mutex<Vec<ObjectRef>>>>,
        skipped: &mut usize,
    ) -> Vec<ObjectRef> {
        let threads = self.threads.read();
        let mut all_roots = Vec::new();
        // gci i13/vg: the old-only section's share, for the census line.
        let mut old_only_entries = 0usize;
        let mut old_only_words = 0usize;
        // gci i14/ve: the mirror-keyed share, for the census line.
        let mut keyed_entries = 0usize;
        let mut keyed_words = 0usize;
        // gcg g6/jn (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS`, default ON
        // since gch h2/yg, `=0` off):
        // the words of every snapshot that was deposited with the
        // conservative interpreter-local probe on, for the unrecorded-young-
        // root guard to exclude. See `conservative_deposit_words`.
        let guard_deposits = unrecorded_root_guard_deposits_enabled();
        let mut conservative_words: Vec<usize> = Vec::new();
        // gch h3/yc: the parked threads' precise words kept under the guard
        // (the `[blockgc] conservative-deposit` line's engagement count), and
        // its kill switch, read once per gather.
        let parked_precise = guard_deposits;
        let mut parked_precise_words = 0usize;
        // gci i12/vp (`CRATONVM_DBG_VT_PARKED_ROOTS`): the unmounted
        // continuations' share of this gather. Counted only with the census on.
        let mut vp_census = self
            .dbg_vt_parked_roots
            .then(|| (Instant::now(), ParkedGatherCensus::default()));
        for (tid, entry) in threads.iter() {
            if entry.alive.load(Ordering::Acquire) {
                let snapshot = entry.root_snapshot.lock();
                // gci i14/ve: a deposited JDK continuation keyed by its
                // mirror is no root of this pause at all; its words are rows
                // owned by the mirror (`publish_mirror_keyed`).
                let keyed_mirror = match keyed.as_ref() {
                    Some((arm, _)) => mirror_keyed_mirror(*tid, entry, &snapshot, arm),
                    None => None,
                };
                // gci i13/vg: an unmounted continuation none of whose words
                // is young goes to the old-only section (see
                // `collect_all_root_snapshots_split`); its mirror and JMX
                // roots below stay in `all_roots`.
                let to_old_only = keyed_mirror.is_none()
                    && match split.as_mut() {
                        Some((young, _)) => parked_snapshot_is_old_only(entry, &**young),
                        None => false,
                    };
                // gci i21/pr: the initiator's own snapshot, already in the
                // door's root vector through `collect_roots` step 11.
                let initiator_dup = match skip {
                    Some(initiator) => Arc::ptr_eq(initiator, &entry.root_snapshot),
                    None => false,
                };
                if let Some(mirror) = keyed_mirror {
                    if let Some((_, out)) = keyed.as_mut() {
                        out.rows
                            .extend(snapshot.iter().map(|w| (mirror, w.as_ptr() as usize)));
                        out.deferred.push((*tid, mirror));
                    }
                    keyed_entries += 1;
                    keyed_words += snapshot.len();
                } else if to_old_only {
                    if let Some((_, old_only)) = split.as_mut() {
                        old_only.extend(snapshot.iter().copied());
                    }
                    old_only_entries += 1;
                    old_only_words += snapshot.len();
                } else if initiator_dup {
                    *skipped += snapshot.len();
                } else {
                    all_roots.extend(snapshot.iter().copied());
                }
                if let Some((_, census)) = vp_census.as_mut() {
                    census.note(!entry.mounted.load(Ordering::Acquire), snapshot.len());
                }
                if guard_deposits {
                    let block = &entry.gc_block_state;
                    let conservative = block.deposit_conservative.load(Ordering::Acquire);
                    // gch h1/yp: a BLOCKED thread's flag-raising deposit also
                    // recorded every Object slot it published (`slot_origins`,
                    // advanced by every fold since, like the snapshot). Its
                    // LIVE locals and its operand-stack slots are precise, so
                    // they stay under the guard; only the rest of a
                    // conservative snapshot is excluded. Lock order snapshot
                    // -> slot_origins, as the deposit and the fold take them.
                    // gch h3/yc: a PARKED thread's safepoint deposit records
                    // no `slot_origins`; it publishes its precise words in
                    // `precise_deposit_words` instead (same lock order:
                    // snapshot -> precise_deposit_words, as the deposit
                    // writes it). Empty until that deposit fills it, which
                    // is the h2 behaviour (the whole snapshot excluded).
                    let precise: Vec<usize> = if !conservative {
                        Vec::new()
                    } else if block.in_blocked_region.load(Ordering::Acquire) {
                        precise_slot_origin_words(&block.slot_origins.lock())
                    } else if parked_precise {
                        let words = sorted_precise_words(&block.precise_deposit_words.lock());
                        parked_precise_words += words.len();
                        words
                    } else {
                        Vec::new()
                    };
                    conservative_deposit_words(
                        conservative,
                        &snapshot,
                        &precise,
                        &mut conservative_words,
                    );
                }
                // Root this thread's `java.lang.Thread` mirror. The
                // initiator's own mirror is rooted via `collect_roots`
                // (roots.rs step 10), but a PARKED thread's mirror is only
                // reachable through `ThreadEntry.java_thread_obj` — a raw
                // address copy that no frame root or heap edge necessarily
                // keeps alive (CratonVM's synthetic ThreadGroup does not
                // retain its threads the way the real JDK's does). Without
                // rooting it here, a GC initiated by ANOTHER thread while
                // this one is parked sweeps the mirror; the dangling copy
                // then surfaces as the "all-zero header" invokevirtual
                // fallback on `Thread.currentThread()`, and the real-JDK
                // `Thread.getThreadGroup()` reads a null `holder` and NPEs
                // (Tomcat TestDigestAuthenticator: a worker-thread GC frees
                // the JUnit main thread's mirror). The matching remap is
                // `update_thread_objs_after_gc`.
                // gci i14/ve: not for a mirror-keyed entry -- the mirror
                // lives only as long as something else reaches it.
                if keyed_mirror.is_none() {
                    if let Some(obj) = entry.java_thread_obj {
                        all_roots.push(obj);
                    }
                }
                if let Some(obj) = *entry.jmx_monitors.contended_monitor.lock() {
                    all_roots.push(obj);
                }
                if let Some(obj) = *entry.jmx_waiting_monitor.lock() {
                    all_roots.push(obj);
                }
                entry.jmx_monitors.locked.extend_roots_stw(&mut all_roots);
                all_roots.extend(entry.jmx_locked_synchronizers.lock().iter().copied());
            }
        }
        if guard_deposits {
            if blockgc_dbg() {
                eprintln!(
                    "[blockgc] conservative-deposit words={} parked_precise={parked_precise_words} \
                     (excluded from the unrecorded-young-root guard)",
                    conservative_words.len()
                );
            }
            // Replaces the previous collection's set: this pause's root set is
            // the one the guard will read.
            cratonvm_gc::gc_quiescence::publish_conservative_deposit_words(conservative_words);
        }
        if let Some((started, census)) = vp_census {
            let mut line = census.line(all_roots.len(), started.elapsed());
            // gci i13/vg: only on a split gather, so the i12/vp line is
            // unchanged otherwise. `roots=` above excludes this section.
            if split.is_some() {
                line.push_str(&format!(
                    " old_only={old_only_entries} old_only_words={old_only_words}"
                ));
            }
            // gci i14/ve: only on a mirror-keyed gather, likewise.
            if keyed.is_some() {
                line.push_str(&format!(
                    " mirror_keyed={keyed_entries} mirror_keyed_words={keyed_words}"
                ));
            }
            // gci i14/ve: with the switch on, every gather line carries the
            // contended-arm count (the pauses that fell back to rooting).
            if self.vt_parked_mirror_keyed {
                line.push_str(&format!(
                    " keyed_skipped_contended={}",
                    self.mirror_keyed.lock().skipped_contended
                ));
            }
            eprintln!("{line}");
        }
        all_roots
    }

    /// Set the blocked-region GC state Arc for a thread (share JvmThread's
    /// state with the registry, like `set_root_snapshot`).
    pub fn set_gc_block_state(&self, thread_id: ThreadId, state: Arc<GcBlockState>) {
        if let Some(entry) = self.threads.write().get_mut(&thread_id) {
            // gci i9/ds: a state installed AFTER the owner published its tid
            // (`publish_os_tid_claim`) would carry no deopt stash cells; when
            // the owner itself installs it, publish them into it too.
            let published = entry.os_tid.load(Ordering::Acquire);
            if published != 0 && published == current_os_tid() {
                publish_deopt_stash_cells_into(&state, published);
            }
            entry.gc_block_state = state;
            // gci i12/vp: a different block state; the parked fold filter is
            // stale.
            entry.mount_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// GC maintenance for the registry's raw `java.lang.Thread` mirrors —
    /// called by the GC initiator (under STW) with the collection's pointer
    /// map.
    ///
    /// Every `ThreadEntry.java_thread_obj` is a raw address copy, and the
    /// `thread_obj_to_park` reverse index is KEYED by such addresses. A
    /// moving collection relocates the mirrors; without this step the
    /// copies dangle: natives that serve them back to Java
    /// (`enumerate_threads` / `Thread.getAllStackTraces`) resurrect stale
    /// refs into bytecode (the all-zero-header invokevirtual WARN flood),
    /// and `LockSupport.unpark(Thread)`'s O(1) lookup misses the live
    /// mirror's new address — a silently lost unpark.
    pub fn update_thread_objs_after_gc(&self, pointer_map: &cratonvm_types::PointerMap) {
        if pointer_map.is_empty() {
            return;
        }
        let mut threads = self.threads.write();
        let mut rekeyed: Vec<(usize, usize)> = Vec::new();
        // Vacated mirror addresses + owning tid, recorded into
        // `former_mirror_addrs` AFTER `threads` is dropped (lock order).
        let mut vacated: Vec<(usize, ThreadId)> = Vec::new();
        // `(old_addr, new_addr, owner)` for every recorded owned synchronizer
        // this collection relocated; applied to `synchronizer_owner` after
        // `threads` is dropped, same lock discipline as `vacated`.
        let mut synchronizer_rekeys: Vec<(usize, usize, ThreadId)> = Vec::new();
        for (tid, entry) in threads.iter_mut() {
            if let Some(ref mut obj) = entry.java_thread_obj {
                let old_addr = obj.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    // SAFETY: produced by the GC pointer map.
                    *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                    rekeyed.push((old_addr, new_addr));
                    vacated.push((old_addr, *tid));
                }
            }
            let mut remap_jmx = |obj: &mut ObjectRef| {
                if let Some(&new_addr) = pointer_map.get(&(obj.as_ptr() as usize)) {
                    *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            };
            if let Some(obj) = entry.jmx_monitors.contended_monitor.lock().as_mut() {
                remap_jmx(obj);
            }
            if let Some(obj) = entry.jmx_waiting_monitor.lock().as_mut() {
                remap_jmx(obj);
            }
            entry.jmx_monitors.locked.remap_stw(pointer_map);
            // `synchronizer_owner` is keyed by these very addresses, so the
            // rekeying happens in the same pass that rewrites them: a key left
            // pointing at a vacated address would make the next
            // `set_jmx_owned_synchronizer` for that lock miss its previous
            // owner and record it against two threads at once. The `(old, new,
            // owner)` triples are collected here and applied after `threads` is
            // dropped, keeping this function's "acquire, use, drop, then call
            // out" lock discipline.
            for obj in entry.jmx_locked_synchronizers.lock().iter_mut() {
                let old_addr = obj.as_ptr() as usize;
                remap_jmx(obj);
                let new_addr = obj.as_ptr() as usize;
                if new_addr != old_addr {
                    synchronizer_rekeys.push((old_addr, new_addr, *tid));
                }
            }
        }
        drop(threads);
        if !rekeyed.is_empty() {
            let mut idx = self.thread_obj_to_park.lock();
            rekey_in_two_phases(&mut idx, &rekeyed);
        }
        if !synchronizer_rekeys.is_empty() {
            // gcg g2/k: two phases, as for `thread_obj_to_park` above. Under a
            // sliding compactor one synchronizer's NEW address can be another
            // one's OLD address; moving them one at a time let the first
            // insert overwrite the second's row (the overwritten owner was
            // lost, and the second move then failed its owner check and was
            // skipped). Remove every moving row first, then insert them all.
            let mut moving: Vec<(usize, ThreadId)> = Vec::new();
            for &(old_addr, new_addr, tid) in &synchronizer_rekeys {
                let mut index =
                    self.synchronizer_owner[synchronizer_owner_shard_index(old_addr)].lock();
                if index.get(&old_addr) == Some(&tid) {
                    index.remove(&old_addr);
                    moving.push((new_addr, tid));
                }
            }
            for (new_addr, tid) in moving {
                self.synchronizer_owner[synchronizer_owner_shard_index(new_addr)]
                    .lock()
                    .insert(new_addr, tid);
            }
        }
        // Record vacated mirror addresses for stale-receiver recovery. Done
        // here (threads lock released) to preserve the threads → former lock
        // order; see `record_former_mirror_addr`.
        for (old_addr, tid) in vacated {
            self.record_former_mirror_addr(old_addr, tid);
        }
    }

    /// Blocked-thread root maintenance — called by the GC initiator (under
    /// STW, before `complete_gc`) with the collection's pointer map.
    ///
    /// For every alive thread currently inside a blocked region (parked in
    /// `Object.wait` / `Thread.join` / `LockSupport.park` /
    /// `ReferenceQueue.remove`, hence excluded from the barrier and unable
    /// to apply this map itself):
    ///
    /// 1. compose the map into the thread's pending frame fixup, chaining
    ///    `orig → cur → new` so the entry stays keyed by the address the
    ///    thread's frames still hold, no matter how many GCs it sleeps
    ///    through;
    /// 2. seed first-move entries from the (pre-remap) snapshot — a snapshot
    ///    address with no existing chain IS the frame-held address;
    /// 3. remap the thread's `root_snapshot` in place so the NEXT collection
    ///    scans live addresses instead of a vacated semispace (scanning a
    ///    stale snapshot is itself a heap corruptor: the collector would
    ///    evacuate garbage "objects" through recycled addresses).
    ///
    /// Threads NOT in a blocked region never need this: `request_stw` counts
    /// them in `expected`, so no GC can complete before they refresh their
    /// snapshot and self-apply the map at a safepoint.
    ///
    /// The thread applies + clears its fixup in `check_post_block_gc` on
    /// wake. While it still holds stale frame addresses the only consumers
    /// are this fold (keyed by those addresses) and the wake-side apply,
    /// so the chain stays consistent.
    pub fn fold_pointer_map_into_blocked(&self, pointer_map: &cratonvm_types::PointerMap) {
        self.fold_pointer_map_into_blocked_audited(pointer_map, None)
    }

    /// [`Self::fold_pointer_map_into_blocked`] plus the post-fold invariant
    /// check, for the production call site that can hand over the heap.
    ///
    /// The invariant: once this fold returns, no address a blocked thread will
    /// resume on may lie in the INACTIVE young semispace. That arena is what
    /// the moving cycle just evacuated and zeroed, so a reference into it is a
    /// still-referenced object the collector took — the "all-zero header
    /// `java.lang.Object` receiver" family — and it is detectable HERE, in the
    /// cycle that caused it, with the frame and slot that hold it. Every other
    /// witness of this bug is downstream: a `NoSuchMethodError` against
    /// `java.lang.Object`, a `checkcast` failure, an out-of-bounds field read —
    /// each an unbounded distance from the collection with the gap, and each
    /// naming only the victim's *use* site.
    ///
    /// The check that runs UNCONDITIONALLY is the precise one: a
    /// `slot_origins` entry — the exact `(frame, slot)` tracker, filled by the
    /// blocking deposit and advanced through each cycle's pointer map — whose
    /// `cur` lands in the vacated arena. Its report carries
    /// `was_a_scanned_root`, which forks the fix: `false` means the deposit
    /// never published that slot (a root COVERAGE gap), `true` means the
    /// collector was handed the address and left it behind (an EVACUATION
    /// gap).
    ///
    /// The whole-snapshot version of the same question is behind
    /// `CRATONVM_DBG_BLOCKGC`, because a snapshot legitimately carries
    /// conservative candidates that are not object starts and which
    /// `forward_object` correctly declines to relocate.
    ///
    /// `heap` is `None` only from tests that drive the fold directly with a
    /// synthetic pointer map.
    pub fn fold_pointer_map_into_blocked_audited(
        &self,
        pointer_map: &cratonvm_types::PointerMap,
        heap: Option<&crate::memory::VmHeap>,
    ) {
        if pointer_map.is_empty() {
            return;
        }
        // gc-common w4-a (`handoff-w3g-blocked-fold-costs` edit 3): this fold's
        // share of the root fix-up, under the same `CRATONVM_DBG_ROOTFIXUP`
        // switch as `update_all_roots`' census (which already counts it inside
        // its total, as one undifferentiated part). See `blocked_fold_census`.
        let census_timer = blocked_fold_census_on().then(BlockedFoldTimer::start);
        // Latched (edit 2): this ran an uncached environment probe once per
        // relocating pause.
        let dbg = blockgc_dbg();
        // One arena lock for the whole fold; the per-address test below is two
        // integer compares, so the audit is affordable unconditionally and does
        // not need a flag to have been set before the run that reproduces.
        // gen r5w5/pin9: the inactive semispace plus a pinned in-place
        // cycle's vacated spans (the roots6 page).
        let vacated = heap.and_then(|h| h.young_vacated_probe());
        // This cycle's cross-thread conservative scan of BLOCKED peers' native
        // stacks, as `(os_tid, addr, value)`. Drained once here and bucketed by
        // owner below: these words live in a peer's machine stack, are named by
        // no oop map, and are the population §12 of
        // `internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
        // measured stale on essentially every relocating cycle. Draining
        // unconditionally matters as much as using it -- a capture left behind
        // by a cycle that did not fold would be applied against the WRONG
        // pointer map later.
        let scanned_stack_slots = cratonvm_gc::gc_quiescence::take_peer_stack_slots();
        // Adopted by SOME blocked thread below. The complement is the reading
        // that matters: a capture nobody claimed is a word in a peer that is
        // NOT in a blocked region -- a take-over freeze, whose thread has no
        // wake hook to apply a fixup at -- naming an object this cycle just
        // relocated, with nothing left to rewrite it. Counted rather than
        // assumed absent; see `PEER_STACK_SLOTS_UNROUTED`.
        let mut adopted_here = 0usize;
        let threads = self.threads.read();
        // gen r5w1/crash5: which entry a capture belongs to. A capture is
        // keyed by the OS tid it was read from, and several alive entries can
        // claim one OS tid -- every virtual thread that last mounted on a
        // carrier keeps the carrier's tid while it is parked (nothing clears it
        // at unmount; see `ThreadEntry::os_tid_seq`). Each of them used to adopt
        // every word of the carrier's stack, and later wrote the words back
        // from whichever carrier it was remounted on: into a stack another
        // thread was running on. Only the claimant running on that OS thread
        // now (the newest `set_os_tid_current` there, the rule
        // `thread_id_for_current_os_tid` already uses) adopts; the write-back
        // itself refuses any word outside the writer's own stack
        // (`vm_exec::apply_native_slot_fixups`). Built only when this cycle
        // captured something.
        let routed_claimant: FxHashMap<u32, Option<ThreadId>> = if scanned_stack_slots.is_empty()
            || !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            FxHashMap::default()
        } else {
            newest_claimants_by_os_tid(
                &threads,
                fold_claimant_skips_unmounted(heap.is_some_and(|h| h.is_generational())),
            )
        };
        // t1 wave 5 lane E2 (hazard 2 of stage 2 of
        // `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`,
        // `CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED`): an OS thread running a
        // continuation slice below its blocked carrier holds two threads'
        // words. A capture at or above the slice's published base is the
        // CARRIER's (`shadow_stack::segment_owner_of_capture`) and is adopted by
        // it alone, below the base by the tid's claimant -- so each word is
        // written back once, by the thread whose stack it is (the carrier's
        // precise compiled slots are rewritten by its own wake remap as well,
        // and two writers make a sliding map's `A -> B, B -> C` a double move).
        // Empty (no allocation) unless a slice runs on a carrier.
        let segments = if scanned_stack_slots.is_empty()
            || !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled()
        {
            Vec::new()
        } else {
            cratonvm_gc::shadow_stack::stack_segments_snapshot()
        };
        // gci i12/vp: the parked-continuation fold filter (opt-in, per VM) and
        // its census (`CRATONVM_DBG_VT_PARKED_ROOTS`). `map_filter` -- this
        // map's keys plus the vacated arena -- is built on the first cached
        // candidate only, so a pause with none pays nothing for it.
        let filter_on = self.vt_parked_fold_filter;
        // gci i13/vg: the split gather (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`)
        // reads the same cached filters, so either switch has the fold leave
        // them; only `filter_on` lets a fold be skipped.
        let filter_kept = filter_on || self.vt_parked_gather_filter;
        let remap_on = cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled();
        let mut map_filter: Option<FoldAddrFilter> = None;
        let vp_started = self.dbg_vt_parked_roots.then(Instant::now);
        let mut vp = ParkedFoldCensus::default();
        for (tid, entry) in threads.iter() {
            if !entry.alive.load(Ordering::Acquire) {
                continue;
            }
            if !entry
                .gc_block_state
                .in_blocked_region
                .load(Ordering::Acquire)
            {
                // NOT blocked: the fixup chain and `slot_origins` below are
                // blocked-region machinery and do not apply.
                //
                // A snapshot remap was tried here on 2026-08-17 — on the theory
                // that a thread censused as blocked and then leaving the region
                // is skipped by this fold AND not waited for by the pause, so
                // its stale snapshot is what the next collection marks from.
                // The theory is sound and the change is a no-op:
                // `CRATONVM_DBG_ROOT_REMAP_AUDIT=1`, which re-runs the root scan
                // at the END of `update_all_roots` and looks for an address this
                // collection moved (excluding slide destinations), reports ZERO
                // with the remap and ZERO without it, on the workload that
                // reproduces the H2 MVStore-writer residual. Every stale
                // snapshot entry it did find was on a BLOCKED thread and was
                // already handled below.
                //
                // Left unwritten deliberately: an unmeasured behaviour change
                // in the root set is exactly what this file's history is made
                // of. If the excluded-thread race is ever observed, the audit
                // above is how to see it and this is where the fix goes.
                //
                // gcg g4/jn (2026-10-02), `CRATONVM_DBG_BLOCKGC` only: name an
                // unblocked thread whose deposited snapshot holds addresses
                // this map moves. A thread parked at its safepoint forwards
                // its own snapshot on resume (`remap_root_snapshot`), so a
                // line here is expected for it; the line that matters is the
                // one whose addresses come back at the NEXT cycle as an
                // `[unrecorded-young-root]` (zero header, `root_source=None`,
                // the g1/j 5.5 signal): that thread resumed WITHOUT forwarding
                // its snapshot. `try_lock`: a frozen peer may hold its lock.
                if dbg {
                    if let Some(snapshot) = entry.root_snapshot.try_lock() {
                        let (moved, first) = moved_snapshot_entries(
                            &snapshot,
                            pointer_map,
                            UNBLOCKED_MOVED_REPORT_ADDRS,
                        );
                        if moved > 0 {
                            eprintln!(
                                "[blockgc] unblocked tid={} os_tid={} mounted={} snapshot_len={} \
                                 moved_by_this_map={} first={}",
                                tid.0,
                                entry.os_tid.load(Ordering::Acquire),
                                entry.mounted.load(Ordering::Acquire),
                                snapshot.len(),
                                moved,
                                first
                                    .iter()
                                    .map(|a| format!("0x{a:x}"))
                                    .collect::<Vec<_>>()
                                    .join(","),
                            );
                        }
                    }
                }
                continue;
            }
            if let Some(census) = &census_timer {
                census.note_thread();
            }
            // gcd d5/f: this thread's blocked state is being rewritten by a
            // relocating pause; its in-native JNI fast paths (`native::jni`)
            // read this count to know they must take the full wake instead.
            entry
                .gc_block_state
                .blocked_folds
                .fetch_add(1, Ordering::Release);
            // gci i12/vp (default-ON `CRATONVM_GC_VT_PARKED_FOLD_FILTER`): an
            // UNMOUNTED continuation whose words this map cannot name. Its
            // cached filter summarises every word the fold below looks up
            // (snapshot, fixup values, `native_slots`, `slot_origins`), as the
            // last fold left them, for the mount epoch it was taken at; with
            // the epoch unchanged nothing but a fold has touched them since.
            // Disjoint from this map's keys (and from the arena the audits
            // below read) means every lookup below misses: the fold would
            // rewrite nothing and report nothing, so skipping it is exact.
            // `blocked_folds` was bumped above all the same (the JNI fast
            // paths and the wake's snapshot keep read it as "a relocating
            // pause targeted this thread", conservatively).
            vp.blocked += 1;
            let parked = !entry.mounted.load(Ordering::Acquire);
            let mut parked_epoch: Option<u64> = None;
            if parked {
                vp.parked += 1;
                if filter_kept {
                    let epoch = entry.mount_epoch.load(Ordering::Acquire);
                    // gci i13/vg: the skip only under the fold switch; the
                    // gather switch alone keeps the filters fresh (below).
                    if let Some(cached) = entry.parked_filter.cached(epoch).filter(|_| filter_on) {
                        vp.cached += 1;
                        let this_map = map_filter.get_or_insert_with(|| {
                            let t = Instant::now();
                            let arena: (Option<(usize, usize)>, &[(usize, usize)]) =
                                match vacated.as_ref() {
                                    Some(p) => (Some(p.inactive_semispace()), p.in_place_spans()),
                                    None => (None, <&[(usize, usize)] as Default>::default()),
                                };
                            let f = pointer_map_fold_filter(pointer_map, arena);
                            vp.map_filter_nanos += t.elapsed().as_nanos();
                            f
                        });
                        if !cached.intersects(this_map) {
                            vp.skipped += 1;
                            // What the native-slot block below does for an
                            // unmounted entry (never `routed_here`): count the
                            // captures that named its stale carrier tid.
                            if remap_on {
                                let os_tid =
                                    Some(entry.os_tid.load(Ordering::Acquire)).filter(|t| *t != 0);
                                if let Some(mine) = os_tid {
                                    let declined = scanned_stack_slots
                                        .iter()
                                        .filter(|&&(t, _, _)| t == mine)
                                        .count();
                                    if declined > 0 {
                                        NATIVE_SLOTS_DECLINED_STALE_CLAIM
                                            .fetch_add(declined as u64, Ordering::Relaxed);
                                        if cratonvm_gc::root_write_audit::enabled() {
                                            note_declined_stale_claim(tid.0, mine, declined);
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                    }
                    parked_epoch = Some(epoch);
                }
            }
            // The filter this fold leaves for the next one (filled below as
            // each word set reaches its final value; `None` unless the filter
            // is on and this entry is an unmounted continuation).
            let mut new_filter: Option<FoldAddrFilter> =
                parked_epoch.map(|_| FoldAddrFilter::default());
            let mut native_slots_summarised = false;
            let mut fixup = entry.gc_block_state.fixup.lock();
            let mut snapshot = entry.root_snapshot.lock();
            let (composed, seeded) = fold_one_blocked_chain(&mut fixup, &mut snapshot, pointer_map);
            if parked {
                vp.parked_words += snapshot.len() + fixup.len();
            }
            if let Some(f) = new_filter.as_mut() {
                for r in snapshot.iter() {
                    f.insert(r.as_ptr() as usize);
                }
                for &v in fixup.values() {
                    f.insert(v);
                }
            }
            // THE BLOCKED-PEER NATIVE-STACK REPAIR (2026-09-07).
            //
            // Adopt this cycle's captures for this thread, then advance every
            // entry -- new and inherited -- through this collection's pointer
            // map. Same shape as `slot_origins` below, and for the same reason:
            // an exact per-map lookup cannot strand an entry on a missed chain
            // seed. The difference is only that these are keyed by a RAW
            // ADDRESS in the peer's stack rather than by `(frame, idx)`.
            if cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
                // Read the entry we are already holding, NOT `self.os_tid_of`.
                //
                // gc-common 2026-09-23 (lane A): `os_tid_of` takes
                // `self.threads.read()` again while this loop already holds a
                // read guard on the same lock. `parking_lot::RwLock` is
                // task-fair — a reader BLOCKS while a writer is queued — so the
                // nested read is exactly the recursion this type's own doc
                // forbids: if any thread queued a `threads.write()` between the
                // two acquisitions, the initiator (inside a moving pause,
                // every counted mutator parked) waits for the writer, and the
                // writer waits for the initiator's outer guard. Writers DO run
                // during a pause: a starting carrier's `set_gc_block_state`
                // (not yet `stw_ready`, so not parked), a JNI attach's
                // `register`, a blocked joiner's `mark_dead` after its
                // `JoinHandle::join` returns. Default-ON path
                // (`blocked_peer_stack_remap_enabled`), reached on every
                // relocating cycle with at least one blocked thread.
                let os_tid = Some(entry.os_tid.load(Ordering::Acquire)).filter(|t| *t != 0);
                let mut ns = entry.gc_block_state.native_slots.lock();
                // gen r5w2/roots6: and only a MOUNTED entry. An unmounted
                // continuation that is still the newest claimant of its last
                // carrier's tid (nothing has mounted there since) is not on
                // that stack; the carrier's words are someone else's.
                let routed_here = entry.mounted.load(Ordering::Acquire)
                    && os_tid.is_some_and(|mine| {
                        routed_claimant.get(&mine).copied().flatten() == Some(*tid)
                    });
                if let Some(mine) = os_tid.filter(|_| routed_here) {
                    // gen r5w2/roots6 (`gengc-r5w1-crash5-blocked-native-slot-
                    // captures-grow-every-cycle`, item 1): a word this thread
                    // already carries with the SAME captured value is not
                    // adopted again. The helper-window pass re-reads a
                    // long-parked thread's stack on every pause (and on every
                    // barrier round of one pause), and each re-read used to
                    // append a fresh copy, growing the list and every fold's
                    // walk of it without bound. At wake only the OLDEST entry
                    // for an address can store (the rest fail the `orig`
                    // guard once it has), so dropping an exact duplicate
                    // changes nothing but the cost. A word whose value CHANGED
                    // since the earlier capture is a new capture and is kept.
                    // addr-keyed: local to this fold (one pause).
                    let mut held: rustc_hash::FxHashSet<(usize, usize)> =
                        ns.iter().map(|e| (e.addr, e.orig)).collect();
                    for &(t, addr, value) in &scanned_stack_slots {
                        if t == mine {
                            // t1 wave 5 lane E2: a word above a slice's base
                            // belongs to the segment's owner (the blocked
                            // carrier), adopted below; unless this entry IS it.
                            if cratonvm_gc::shadow_stack::segment_owner_of_capture(
                                &segments, mine, addr,
                            )
                            .is_some_and(|owner| owner != tid.0)
                            {
                                continue;
                            }
                            if !held.insert((addr, value)) {
                                NATIVE_SLOTS_DUPLICATE_CAPTURES.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            ns.push(crate::threading::jvm_thread::NativeSlotFixup {
                                addr,
                                orig: value,
                                cur: value,
                            });
                            cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_ADOPTED
                                .fetch_add(1, Ordering::Relaxed);
                            adopted_here += 1;
                        }
                    }
                } else if let Some(mine) = os_tid {
                    // A stale claimant: these words are the stack of whoever
                    // runs on `mine` now, not this entry's.
                    let declined = scanned_stack_slots
                        .iter()
                        .filter(|&&(t, _, _)| t == mine)
                        .count();
                    if declined > 0 {
                        NATIVE_SLOTS_DECLINED_STALE_CLAIM
                            .fetch_add(declined as u64, Ordering::Relaxed);
                        if cratonvm_gc::root_write_audit::enabled() {
                            note_declined_stale_claim(tid.0, mine, declined);
                        }
                    }
                }
                // t1 wave 5 lane E2: this entry is the blocked carrier of a
                // continuation slice running on `seg_tid` below `base` (its own
                // tid claim is withdrawn for the slice): adopt the captures of
                // its half of that stack, which its own wake writes back. Not
                // when it is the claimant of that tid again (the slice is over;
                // the block above adopted every word).
                for &(seg_tid, base, owner) in &segments {
                    if owner != tid.0 || (routed_here && os_tid == Some(seg_tid)) {
                        continue;
                    }
                    // addr-keyed: local to this fold (one pause).
                    let mut held: rustc_hash::FxHashSet<(usize, usize)> =
                        ns.iter().map(|e| (e.addr, e.orig)).collect();
                    for &(t, addr, value) in &scanned_stack_slots {
                        if t != seg_tid || addr < base {
                            continue;
                        }
                        if !held.insert((addr, value)) {
                            NATIVE_SLOTS_DUPLICATE_CAPTURES.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        ns.push(crate::threading::jvm_thread::NativeSlotFixup {
                            addr,
                            orig: value,
                            cur: value,
                        });
                        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_ADOPTED
                            .fetch_add(1, Ordering::Relaxed);
                        adopted_here += 1;
                    }
                }
                for e in ns.iter_mut() {
                    if let Some(&new) = pointer_map.get(&e.cur) {
                        e.cur = new;
                    }
                }
                // gci i12/vp: the native slots' share of the next filter.
                if parked {
                    vp.parked_words += ns.len();
                }
                if let Some(f) = new_filter.as_mut() {
                    for e in ns.iter() {
                        f.insert(e.cur);
                    }
                    native_slots_summarised = true;
                }
            }
            // cceres3 FIX: advance the exact per-slot tracker through THIS
            // collection's pointer map (see `GcBlockState::slot_origins`).
            // Exact lookups per map — no chain keys to strand.
            {
                let mut origins = entry.gc_block_state.slot_origins.lock();
                for so in origins.iter_mut() {
                    if let Some(&new) = pointer_map.get(&so.cur) {
                        so.cur = new;
                    }
                }
                // gci i12/vp: the slot origins' share of the next filter
                // (dead locals included: the lookup above does not tell them
                // apart, so neither may the skip).
                if parked {
                    vp.parked_words += origins.len();
                }
                if let Some(f) = new_filter.as_mut() {
                    for so in origins.iter() {
                        f.insert(so.cur);
                    }
                }
                if let Some(probe) = vacated.as_ref() {
                    // Built lazily: a frame slot landing in the vacated arena
                    // is the rare case, and this set is only needed to report
                    // one.
                    // addr-keyed: local to this fold (one pause).
                    let mut in_snapshot: Option<rustc_hash::FxHashSet<usize>> = None;
                    for so in origins.iter() {
                        // A DEAD local in the vacated arena is the per-bci
                        // liveness analysis working as designed, not a defect
                        // — see `SlotOrigin::live`.
                        if !so.live || !probe.contains(so.cur) {
                            continue;
                        }
                        let published = in_snapshot
                            .get_or_insert_with(|| {
                                snapshot.iter().map(|r| r.as_ptr() as usize).collect()
                            })
                            .contains(&so.cur);
                        // The remap above rewrote the snapshot in place, so a
                        // hit means the collector had this exact address as a
                        // root and left it behind; a miss means the deposit
                        // never published it.
                        blocked_root_gap_report(
                            tid.0,
                            so.frame,
                            so.idx,
                            so.is_stack,
                            so.orig,
                            so.cur,
                            published,
                        );
                    }
                }
            }
            // The whole-snapshot version of the same question. Gated, unlike
            // the per-slot check above, because the snapshot legitimately
            // carries CONSERVATIVE candidates — register/stack words and
            // JIT-band scans that are not object starts — and `forward_object`
            // correctly declines to relocate those, so they land in the vacated
            // arena on every moving cycle by design. Only a hit that is ALSO a
            // frame slot is unambiguous, and that is what the check above
            // reports unconditionally.
            if dbg {
                if let Some(probe) = vacated.as_ref() {
                    // gcg g5/jn: with the entry's index and the snapshot's
                    // length. `deposit_root_snapshot_inner` appends the
                    // conservative JIT-band candidates (then the shadow stack)
                    // LAST (after frames, off-frame roots and JNI locals), so
                    // an index near the end on a thread with compiled
                    // frames is a candidate the collector declines to
                    // forward by design.
                    let len = snapshot.len();
                    for (idx, r) in snapshot.iter().enumerate() {
                        let a = r.as_ptr() as usize;
                        if probe.contains(a) {
                            blocked_snapshot_gap_report(tid.0, a, idx, len);
                        }
                    }
                }
            }
            if dbg && (composed > 0 || seeded > 0) {
                eprintln!(
                    "[blockgc] fold tid={} composed={} seeded={} fixup_total={} snapshot_len={}",
                    tid.0,
                    composed,
                    seeded,
                    fixup.len(),
                    snapshot.len()
                );
            }
            // gci i12/vp: leave the filter for the next fold, tagged with the
            // epoch read BEFORE any word was read. A mount racing this pause
            // (the remount's drain waits the pause out) has bumped the epoch
            // already or will: either way the tag no longer matches.
            if let (Some(epoch), Some(f)) = (parked_epoch, new_filter) {
                if native_slots_summarised {
                    entry.parked_filter.store(epoch, f);
                    vp.stored += 1;
                } else {
                    entry.parked_filter.clear();
                }
            }
        }
        if let Some(started) = vp_started {
            eprintln!(
                "{}",
                vp.line(pointer_map.len(), filter_on, started.elapsed())
            );
        }
        // The complement of the adoption, taken after the walk so every blocked
        // thread has had its chance to claim a capture.
        if scanned_stack_slots.len() > adopted_here {
            cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_UNROUTED.fetch_add(
                (scanned_stack_slots.len() - adopted_here) as u64,
                Ordering::Relaxed,
            );
        }
    }

    /// Get the number of registered threads.
    pub fn count(&self) -> usize {
        self.threads.read().len()
    }

    /// Get the number of alive threads.
    pub fn alive_count(&self) -> usize {
        self.threads
            .read()
            .values()
            .filter(|e| e.alive.load(Ordering::Acquire))
            .count()
    }

    /// xt-hardening (2026-07-03): publish the calling thread's OS thread id
    /// for `thread_id` (see `ThreadEntry::os_tid`). Called by the thread
    /// itself at startup, before it can execute any Java/JIT code.
    pub fn set_os_tid_current(&self, thread_id: ThreadId) {
        // CRATONVM_DBG_BLOCKED_ACCESS: register THIS thread's authoritative
        // `in_blocked_region` flag with the gc crate's heap-access canary,
        // which cannot reach the `JvmThread` from the `get_header` funnel.
        // Like the os_tid publish itself, this always runs on the owning
        // thread at startup (main / spawned / native-carrier). No-op when the
        // gate is off. The leaked `Arc<GcBlockState>` clone (debug-gated, one
        // per thread) pins the flag's address for the process lifetime, which
        // is the safety contract `register_self_blocked_flag` requires.
        if cratonvm_gc::blocked_access_debug::enabled() {
            let threads = self.threads.read();
            if let Some(entry) = threads.get(&thread_id) {
                let keep = entry.gc_block_state.clone();
                let flag: *const std::sync::atomic::AtomicBool = &keep.in_blocked_region;
                std::mem::forget(keep);
                // SAFETY: `flag` points into the leaked Arc's referent above,
                // so it stays valid for the process lifetime; only the owning
                // thread reads it back through its TLS slot.
                unsafe { cratonvm_gc::blocked_access_debug::register_self_blocked_flag(flag) };
            }
        }
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetCurrentThreadId() -> u32;
            }
            let os_tid = unsafe { GetCurrentThreadId() };
            cratonvm_gc::shadow_stack::publish_thread_stack_band(os_tid);
            self.publish_os_tid_claim(thread_id, os_tid);
        }
        #[cfg(target_os = "linux")]
        {
            let os_tid = unsafe { libc::syscall(libc::SYS_gettid) as u32 };
            cratonvm_gc::shadow_stack::publish_thread_stack_band(os_tid);
            self.publish_os_tid_claim(thread_id, os_tid);
        }
        #[cfg(all(not(windows), not(target_os = "linux")))]
        {
            let _ = thread_id; // takeover machinery is currently Windows/Linux-only
        }
    }

    /// gcd d3/k: publish the calling OS thread's id for a foreign-attached
    /// thread leaving its idle window (`jni::foreign_leave_idle`, under
    /// `CRATONVM_JNI_FOREIGN_TRANSITIONS`). [`Self::set_os_tid_current`]
    /// without its once-per-thread debug canary registration, which leaks one
    /// `Arc` per call and is not meant to run per transition.
    pub fn republish_os_tid_current(&self, thread_id: ThreadId) {
        let os_tid = current_os_tid();
        if os_tid != 0 {
            self.publish_os_tid_claim(thread_id, os_tid);
        }
    }

    /// gcd d3/k: withdraw `thread_id`'s published OS tid (store 0) as a
    /// foreign-attached thread goes idle in host code
    /// (`jni::foreign_enter_idle`). An idle attachment has no Java or compiled
    /// frame, so no take-over or helper-window pass has anything to read on
    /// it, and with the tid published every pass would signal (Linux) or
    /// suspend (Windows) a host thread parked in `epoll_wait` for nothing. The
    /// ordinal is left alone: a `0` tid matches no claimant query.
    pub fn clear_os_tid(&self, thread_id: ThreadId) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            entry.os_tid.store(0, Ordering::Release);
        }
    }

    /// Store `os_tid` for `thread_id`, then stamp the entry with a fresh
    /// publish ordinal (see `ThreadEntry::os_tid_seq`). The order matters to
    /// [`Self::thread_id_for_current_os_tid`]: a reader that sees the new
    /// ordinal with `Acquire` also sees the new `os_tid`, so an entry moving
    /// to another carrier can never be read as "newest on the old one".
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    pub(in crate::threading::thread_registry) fn publish_os_tid_claim(&self, thread_id: ThreadId, os_tid: u32) {
        let threads = self.threads.read();
        if let Some(entry) = threads.get(&thread_id) {
            // gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`):
            // every caller passes the CALLING OS thread's tid (startup, a
            // virtual thread's mount, a foreign thread leaving idle), before it
            // runs Java or compiled code there. Publish this OS thread's JIT
            // deopt stash cells beside it, so a take-over that freezes the
            // thread in compiled code can root its stashed frames
            // (`stw_take_over_and_wait`'s frozen-peer walk). A no-op when the
            // triple is already published.
            publish_deopt_stash_cells_into(&entry.gc_block_state, os_tid);
            entry.os_tid.store(os_tid, Ordering::Release);
            let seq = OS_TID_PUBLISH_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
            entry.os_tid_seq.store(seq, Ordering::Release);
        }
    }

    /// xt-hardening (2026-07-03): STW-ready alive-thread count PLUS the
    /// published OS tids of those counted threads, read under one registry lock
    /// so the GC barrier's `expected` and the takeover's counted-set snapshot
    /// cannot disagree about which threads exist.
    ///
    /// A `Thread.start()` child is alive before its carrier can answer a
    /// safepoint. It stays out of this counted set until `mark_stw_ready` flips
    /// the startup gate; if an STW is already active, the child waits it out via
    /// `arrive_and_wait_excluded` first.
    pub fn alive_count_and_os_tids(&self) -> (usize, Vec<u32>) {
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("alive_count_and_os_tids");
        let len = || self.threads.read().len();
        PERF.time(len, || self.alive_count_and_os_tids_inner())
    }

    pub(in crate::threading::thread_registry) fn alive_count_and_os_tids_inner(&self) -> (usize, Vec<u32>) {
        // JIT round 16 wave 9b (lane ffm): the take-over's roster leaves out a
        // thread in foreign code (`ffm_in_native_signal_exempt`); the COUNT is
        // unchanged.
        let exempt_on = self.ffm_in_native_no_signal;
        let threads = self.threads.read();
        let mut n = 0usize;
        let mut tids = Vec::with_capacity(threads.len());
        for e in threads.values() {
            if entry_is_stw_live(e) && e.stw_ready.load(Ordering::Acquire) {
                n += 1;
                if exempt_on && ffm_in_native_signal_exempt(&e.gc_block_state) {
                    continue;
                }
                let t = e.os_tid.load(Ordering::Acquire);
                if t != 0 {
                    tids.push(t);
                }
            }
        }
        (n, tids)
    }

    /// Alive-thread count, live blocked-thread count, and alive OS tids read
    /// under one registry lock.
    ///
    /// The blocked count is the production STW exclusion count: a thread sets
    /// `in_blocked_region` only after depositing its root snapshot, and that
    /// publication can race ahead of the barrier's anonymous `threads_blocked`
    /// counter. Reading it with the alive set keeps the expected mutator quota
    /// aligned with the root/fixup state the collector will actually scan.
    ///
    /// STWREADY-GAP-FIX (2026-07-11): also gate on `stw_ready`, matching the
    /// sibling `alive_count_and_os_tids` (added 2026-07-03, "identity-based
    /// barrier excusal for xt-takeover"). Without this, a `Thread.start()`
    /// child is alive (registered in this map) before its carrier can answer
    /// a safepoint -- `vm_exec.rs`'s `thread_start` startup loop keeps such a
    /// thread out of `mark_stw_ready` until it observes NO active STW
    /// (`run_if_no_stw_requested`), and if one IS active when it starts, it
    /// calls the NON-participating `arrive_and_wait_excluded` -- which never
    /// increments the barrier's `arrived` counter. A pause whose `expected`
    /// snapshot (this function) included that same not-yet-ready thread can
    /// then never satisfy its quota: `arrived` structurally cannot reach
    /// `expected`, and `wait_for_all`/the STW requester's own wait loop
    /// blocks forever -- a whole-VM freeze with zero forward progress,
    /// confirmed via gdb (the stuck threads sit in
    /// `GcBarrier::arrive_and_wait_excluded` from `thread_start`'s startup
    /// loop, never having deposited a root snapshot or reached any
    /// safepoint). A not-yet-`stw_ready` thread cannot yet be executing Java
    /// bytecode (that is exactly what the gate protects), so excluding it
    /// from `expected` cannot let it mutate the heap concurrently with an
    /// evacuating collection -- there is nothing for the collector to race
    /// against on that thread until it actually marks itself ready, at which
    /// point it becomes visible to the NEXT pause's snapshot as normal.
    ///
    /// GCAUDIT-0711-FIX (finding 1a): also returns the IDENTITIES
    /// (`ThreadId.0`) of the alive threads counted in `blocked`, so
    /// `GcBarrier::request_stw_counted_with_live_blocked` can publish exactly
    /// which threads THIS pause excluded — see
    /// `GcBarrierInner::excluded_blocked`'s doc for why per-thread identity,
    /// not just a count, is required for a race-free arrival decision.
    pub fn alive_count_blocked_and_os_tids(&self) -> (usize, usize, Vec<u32>, Vec<u64>) {
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("alive_count_blocked_and_os_tids");
        let len = || self.threads.read().len();
        PERF.time(len, || self.alive_count_blocked_and_os_tids_inner())
    }

    pub(in crate::threading::thread_registry) fn alive_count_blocked_and_os_tids_inner(&self) -> (usize, usize, Vec<u32>, Vec<u64>) {
        let threads = self.threads.read();
        let mut alive = 0usize;
        let mut blocked = 0usize;
        let mut tids = Vec::with_capacity(threads.len());
        let mut blocked_tids = Vec::new();
        for (tid, e) in threads.iter() {
            if entry_is_stw_live(e) && e.stw_ready.load(Ordering::Acquire) {
                alive += 1;
                // gci i20/jt: a thread in a lock-free in-native leave is counted.
                if e.gc_block_state.census_blocked() {
                    blocked += 1;
                    blocked_tids.push(tid.0);
                }
                let t = e.os_tid.load(Ordering::Acquire);
                if t != 0 {
                    tids.push(t);
                }
            }
        }
        (alive, blocked, tids, blocked_tids)
    }

    /// The STW census of [`Self::alive_count_blocked_and_os_tids`] plus whether
    /// `initiator` owns one of the counted slots — the fact
    /// `GcBarrier`'s `StwCensus::initiator_counted` carries (gc-common w3-a,
    /// 2026-09-23; `handoff-w2a-census-reports-initiator`,
    /// `docs/feature-designs/real-jdk-mode.md`).
    ///
    /// `expected = alive - 1 - blocked` assumes the initiator is one of the
    /// `alive` threads. The door can only tell that its OWN id is counted; it
    /// cannot tell "not counted at all" (an unregistered host thread, a carrier
    /// before `mark_stw_ready`, a thread after `mark_dead`) from "counted under
    /// another id" (a virtual thread mounted on a registered carrier runs on the
    /// carrier's OS thread). Answering "not counted" in the second case would
    /// make the barrier wait for a slot that is the initiator itself — a hang.
    /// Only this registry has both the entries' published `os_tid`s and the
    /// calling OS thread's id, so the answer is computed here, under the same
    /// lock hold as the census itself:
    ///
    /// * `Some(true)` — `initiator`'s own entry is counted, or a counted entry
    ///   is published on the calling OS thread;
    /// * `Some(false)` — neither, and every counted entry has published its
    ///   `os_tid`, so none of them can be this thread;
    /// * `None` — undecidable: the platform has no OS-tid backend, or a counted
    ///   entry has not published its `os_tid` yet and could be this thread.
    ///   The barrier then keeps the historical `- 1`.
    ///
    /// Runs under the barrier lock (inside the request's census closure), so it
    /// is the same single `threads.read()` walk as the plain census.
    pub fn alive_count_blocked_and_os_tids_for(
        &self,
        initiator: ThreadId,
    ) -> (usize, usize, Vec<u32>, Vec<u64>, Option<bool>) {
        static PERF: threadreg_perf::Counters =
            threadreg_perf::Counters::new("alive_count_blocked_and_os_tids_for");
        let len = || self.threads.read().len();
        PERF.time(len, || {
            self.alive_count_blocked_and_os_tids_for_inner(initiator, current_os_tid())
        })
    }

    /// [`Self::alive_count_blocked_and_os_tids_for`] with the calling OS
    /// thread's id supplied, so a test can place the "caller" on any OS thread
    /// (`me == 0` = no backend).
    pub(in crate::threading::thread_registry) fn alive_count_blocked_and_os_tids_for_inner(
        &self,
        initiator: ThreadId,
        me: u32,
    ) -> (usize, usize, Vec<u32>, Vec<u64>, Option<bool>) {
        let threads = self.threads.read();
        let mut alive = 0usize;
        let mut blocked = 0usize;
        let mut tids = Vec::with_capacity(threads.len());
        let mut blocked_tids = Vec::new();
        let (mut own_counted, mut os_counted, mut unknown_os) = (false, false, false);
        for (tid, e) in threads.iter() {
            if entry_is_stw_live(e) && e.stw_ready.load(Ordering::Acquire) {
                alive += 1;
                // gci i20/jt: a thread in a lock-free in-native leave is counted.
                if e.gc_block_state.census_blocked() {
                    blocked += 1;
                    blocked_tids.push(tid.0);
                }
                let t = e.os_tid.load(Ordering::Acquire);
                if t != 0 {
                    tids.push(t);
                    os_counted |= me != 0 && t == me;
                } else {
                    unknown_os = true;
                }
                own_counted |= *tid == initiator;
            }
        }
        let verdict = initiator_counted_verdict(own_counted, os_counted, me != 0, unknown_os);
        (alive, blocked, tids, blocked_tids, verdict)
    }

    /// DIAGNOSTIC (2026-07-13, STW takeover 5-class cluster investigation):
    /// `ThreadId`s of every alive, `stw_ready` thread NOT in `excluded` —
    /// i.e. the exact identity set a `request_stw_counted_with_live_blocked`
    /// caller is about to count as "expected". Not lock-atomic with a prior
    /// `alive_count_blocked_and_os_tids` call, but both are invoked from
    /// inside the SAME `request_stw_counted_with_live_blocked` closure, which
    /// runs under the barrier's transition lock — no mutator can flip
    /// `in_blocked_region` in a way that would matter between the two calls
    /// for diagnostic purposes.
    pub fn alive_thread_ids_excluding(&self, excluded: &[u64]) -> Vec<u64> {
        let threads = self.threads.read();
        threads
            .iter()
            .filter(|(tid, e)| {
                entry_is_stw_live(e)
                    && e.stw_ready.load(Ordering::Acquire)
                    && !excluded.contains(&tid.0)
            })
            .map(|(tid, _)| tid.0)
            .collect()
    }

    /// Get Java Thread objects for all alive threads (up to `max` entries).
    pub fn alive_thread_objects(&self, max: usize) -> Vec<ObjectRef> {
        self.threads
            .read()
            .values()
            .filter(|e| e.alive.load(Ordering::Acquire))
            .filter_map(|e| e.java_thread_obj)
            .take(max)
            .collect()
    }

    /// [`Self::alive_thread_objects`], every one, in `ThreadId` order
    /// (interpreter round i5 wave 102, lane L1): JVMTI `GetAllThreads`,
    /// `GetAllStackTraces` and `GetThreadGroupChildren` list threads in
    /// HotSpot's threads-list order (each thread appended as it attaches),
    /// not in the map's hash order, which differed from run to run.
    pub fn alive_thread_objects_in_id_order(&self) -> Vec<ObjectRef> {
        let mut live: Vec<(ThreadId, ObjectRef)> = self
            .threads
            .read()
            .iter()
            .filter(|(_, e)| e.alive.load(Ordering::Acquire))
            .filter_map(|(tid, e)| e.java_thread_obj.map(|obj| (*tid, obj)))
            .collect();
        live.sort_unstable_by_key(|(tid, _)| tid.0);
        live.into_iter().map(|(_, obj)| obj).collect()
    }

    /// T19.K1 — Enumerate the `ThreadId`s of every currently-alive
    /// non-daemon thread. The returned `Vec` is a snapshot taken under
    /// the registry mutex; concurrent transitions (a non-daemon thread
    /// terminating, or a thread that hasn't been registered yet starting)
    /// are not reflected.
    ///
    /// `ThreadId(0)` (the "main" thread) is always excluded: the CLI
    /// invokes this query *from* the main thread after `main()`
    /// returns, so including it would deadlock. Tests register
    /// non-zero ids to exercise the join path.
    pub fn alive_non_daemon_thread_ids(&self) -> Vec<ThreadId> {
        self.threads
            .read()
            .iter()
            .filter(|(tid, e)| {
                tid.0 != 0 && e.alive.load(Ordering::Acquire) && !e.daemon.load(Ordering::Acquire)
            })
            .map(|(tid, _)| *tid)
            .collect()
    }

    /// T19.K1 — Block the caller until every non-daemon thread that
    /// currently exists in the registry has terminated.
    ///
    /// Per the JVM specification (JLS §12.8 / §17), the VM keeps running
    /// until every thread that *isn't* a daemon completes. Daemon threads
    /// (event loops, GC workers, finalisers, …) may still be running when
    /// this call returns — the caller is expected to terminate the
    /// process at that point and let the OS clean those up.
    ///
    /// # Algorithm
    ///
    /// We snapshot the set of non-daemon thread ids, attempt to take each
    /// one's `JoinHandle` out of the registry, and join on it. If a
    /// thread has already finished and its handle was already taken,
    /// `join()` returns `false` quickly. After joining everything in the
    /// snapshot we re-snapshot and repeat — this covers the case where
    /// a non-daemon thread spawned more non-daemon threads and exited
    /// while we were joining its predecessor.
    ///
    /// # Termination
    ///
    /// The loop terminates when a fresh snapshot is empty. If a hostile
    /// or buggy program keeps spawning non-daemon threads, the loop
    /// will not exit — same as HotSpot. The optional `deadline` lets
    /// the CLI bound this for diagnostic / CI runs (`None` ≡ wait
    /// forever; `Some(_)` ≡ best-effort with a timeout).
    ///
    /// # Returns
    ///
    /// The number of distinct non-daemon threads that were joined (i.e.
    /// the number of `JoinHandle::join` calls that succeeded). `0` means
    /// no non-daemon threads existed when the call was made — the safe
    /// fast-path for `HelloWorld`-style programs.
    pub fn wait_for_non_daemon_threads(&self, deadline: Option<std::time::Instant>) -> usize {
        let mut joined = 0usize;
        loop {
            let pending = self.alive_non_daemon_thread_ids();
            if pending.is_empty() {
                return joined;
            }
            for tid in pending {
                if let Some(d) = deadline {
                    if std::time::Instant::now() >= d {
                        return joined;
                    }
                }
                // T19.K1 — `join()` takes the handle out, joins on it,
                // and marks the thread dead. If the handle was already
                // taken (e.g. a sibling pumped through `Thread.join()`
                // first) we only count successful joins. We still loop
                // back so we re-snapshot for newly-spawned non-daemon
                // threads.
                // At most ~1 s per handle-less thread, as before wave 24
                // (the outer loop re-snapshots), and never past `deadline`.
                let give_up = std::time::Instant::now() + std::time::Duration::from_secs(1);
                let give_up = deadline.map_or(give_up, |d| d.min(give_up));
                if self.join_giving_up_at(tid, Some(give_up)) {
                    joined += 1;
                }
            }
        }
    }
}
/// Rate limit for the two blocked-root-gap reporters below. The defect
/// cascades — one blocked thread typically has many slots pointing at the
/// same lost subgraph — and the first handful carry all the information.
pub(in crate::threading::thread_registry) const BLOCKED_ROOT_GAP_REPORTS: u64 = 12;

/// gcg g6/jn (2026-10-02),
/// `gcg-g5jn-unrecorded-root-guard-reads-a-conservative-deposit-as-precise`:
/// `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS` (gch h2/yg: default ON,
/// `runtime_flag_default_on`; `=0` restores the gch h1 default). On, every
/// collection's snapshot gather publishes the words of the snapshots
/// deposited with the conservative interpreter-local probe on
/// (`GcBlockState::deposit_conservative`) to the pause ledger
/// (`gc_quiescence::publish_conservative_deposit_words`), and the
/// Generational unrecorded-young-root guard excludes them on a cycle it
/// otherwise treats as precise -- except a blocked thread's live locals and
/// operand slots, which stay guarded (gch h1/yp). Read once per gather.
///
/// Flipped on gch vh1-yp block A: 8 interleaved pairs, stdout SAME 16/16;
/// the `_def` arm refused 2 young cycles (2 of 8 runs, `root_source=None`,
/// zero headers -- the g5jn signature), the `_dep` arm 0 of 8, with
/// `[blockgc] conservative-deposit words=15` engaged on every `_dep` run.
pub(in crate::threading::thread_registry) fn unrecorded_root_guard_deposits_enabled() -> bool {
    true
}



/// gcg g6/jn: append `snapshot`'s words to `out` when the snapshot was
/// deposited conservatively. The deposit probed EVERY interpreter local
/// (`scan_local_objects_all_live`, dead ones included) because some thread was
/// in compiled code then; a later cycle with no compiled frame live must not
/// read those words as precise roots. The whole snapshot goes: the deposit
/// does not record which entries the probe added, and excluding a precise
/// word only returns the guard to its pre-gcd-d2/h fail-open for that word.
///
/// gch h1/yp: except the words in `precise` (sorted, from
/// [`precise_slot_origin_words`]): the values of the live locals and operand
/// slots a blocked thread's deposit recorded. Those are precise roots, so the
/// guard keeps checking them, and a dangling live local still refuses its
/// cycle with the switch on. A `precise` that is stale (an earlier window's)
/// only keeps a word under the guard, i.e. at most one refused cycle per
/// address: the switch-off behaviour for that word, never a dangle. Pure.
pub(in crate::threading::thread_registry) fn conservative_deposit_words(
    conservative: bool,
    snapshot: &[ObjectRef],
    precise: &[usize],
    out: &mut Vec<usize>,
) {
    if conservative {
        out.extend(
            snapshot
                .iter()
                .map(|r| r.as_ptr() as usize)
                .filter(|a| precise.binary_search(a).is_err()),
        );
    }
}

/// gch h1/yp: the current values (`cur`) of the slots in `origins` that were
/// precise roots at the deposit -- a LIVE local, or an operand-stack slot
/// (always live, `SlotOrigin::live`) -- sorted and deduplicated for
/// [`conservative_deposit_words`]. Pure.
pub(in crate::threading::thread_registry) fn precise_slot_origin_words(origins: &[crate::threading::jvm_thread::SlotOrigin]) -> Vec<usize> {
    let mut words: Vec<usize> = origins
        .iter()
        .filter(|so| so.is_stack || so.live)
        .map(|so| so.cur)
        .collect();
    words.sort_unstable();
    words.dedup();
    words
}

/// gch h3/yc: a parked thread's `GcBlockState::precise_deposit_words`, sorted
/// and deduplicated for [`conservative_deposit_words`]. Pure.
pub(in crate::threading::thread_registry) fn sorted_precise_words(words: &[usize]) -> Vec<usize> {
    let mut out = words.to_vec();
    out.sort_unstable();
    out.dedup();
    out
}

/// gcg g4/jn: how many moved addresses the `CRATONVM_DBG_BLOCKGC` line for an
/// UNBLOCKED thread lists (the count is always complete).
pub(in crate::threading::thread_registry) const UNBLOCKED_MOVED_REPORT_ADDRS: usize = 8;

/// gcg g4/jn: how many entries of `snapshot` name an address `pointer_map`
/// moves, and the first `max` of those addresses (snapshot order). Pure; the
/// diagnostic half of [`ThreadRegistry::fold_pointer_map_into_blocked_audited`]
/// for a thread that is NOT in a blocked region.
pub(in crate::threading::thread_registry) fn moved_snapshot_entries(
    snapshot: &[ObjectRef],
    pointer_map: &cratonvm_types::PointerMap,
    max: usize,
) -> (usize, Vec<usize>) {
    let mut moved = 0usize;
    let mut first = Vec::new();
    for r in snapshot {
        let a = r.as_ptr() as usize;
        if pointer_map.contains_key(&a) {
            moved += 1;
            if first.len() < max {
                first.push(a);
            }
        }
    }
    (moved, first)
}

/// A blocked thread's frame slot still points into the arena this moving
/// cycle evacuated: the object it names was NOT relocated, so on wake the
/// slot reads the all-zero header the collector left behind.
///
/// `in_snapshot` is the fork that decides where the fix goes. `false` (the
/// common case) means `deposit_root_snapshot`'s frame scan never published
/// this slot, so the collector could not have known to evacuate it — a
/// root-coverage gap in the deposit. `true` means the collector held this
/// exact address as a root and left it behind anyway — an evacuation gap.
pub(in crate::threading::thread_registry) fn blocked_root_gap_report(
    tid: u64,
    frame: u32,
    idx: u32,
    is_stack: bool,
    orig: usize,
    cur: usize,
    in_snapshot: bool,
) {
    static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if R.fetch_add(1, Ordering::Relaxed) >= BLOCKED_ROOT_GAP_REPORTS {
        return;
    }
    tracing::error!(
        target: "cratonvm::gc::guard",
        tid = tid,
        frame = frame,
        slot = if is_stack { "stack" } else { "local" },
        idx = idx,
        deposited_addr = format!("{orig:#x}"),
        current_addr = format!("{cur:#x}"),
        was_a_scanned_root = in_snapshot,
        "blocked-thread frame slot points into the semispace this moving cycle just \
         evacuated — the object was not relocated and the slot will read an all-zero \
         `java.lang.Object` header on wake. `was_a_scanned_root=false` means the \
         blocking deposit never published this slot (root-coverage gap); `true` means \
         the collector was handed it and did not evacuate it (evacuation gap).",
    );
}

/// A blocked thread's deposited root snapshot still names the evacuated
/// arena after the fold remapped it.
///
/// `CRATONVM_DBG_BLOCKGC` only. Most hits are benign by construction: the
/// snapshot carries conservative candidates (register/stack words, JIT-band
/// scans) that are not object starts, and `forward_object` declines to
/// relocate anything `young_object_starts` does not vouch for. A hit is only
/// interesting when the SAME address is also a frame slot — which
/// [`blocked_root_gap_report`] reports on its own, unconditionally.
pub(in crate::threading::thread_registry) fn blocked_snapshot_gap_report(tid: u64, addr: usize, idx: usize, snapshot_len: usize) {
    static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if R.fetch_add(1, Ordering::Relaxed) >= BLOCKED_ROOT_GAP_REPORTS {
        return;
    }
    tracing::error!(
        target: "cratonvm::gc::guard",
        tid = tid,
        obj = format!("{addr:#x}"),
        idx = idx,
        snapshot_len = snapshot_len,
        "blocked-thread root snapshot still names the semispace this moving cycle just \
         evacuated, after the fold's remap — the collector scanned this root and did not \
         relocate it. The next collection would scan the same stale address.",
    );
}

impl Default for ThreadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ThreadRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let threads = self.threads.read();
        f.debug_struct("ThreadRegistry")
            .field("total", &threads.len())
            .field(
                "alive",
                &threads
                    .values()
                    .filter(|e| e.alive.load(Ordering::Acquire))
                    .count(),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------
// gci i12/vp: parked virtual threads as roots of every pause
// (`gci-i12vp-parked-continuations-folded-by-every-young-pause-FIXED-20261016`).
// ---------------------------------------------------------------------------

/// [`FoldAddrFilter`]'s granule: 1 MiB, no larger than the smallest G1
/// region and far below the Generational young semispace, so a young (or
/// collection-set) granule and an old one share a bit only through the
/// modulo-256 fold, i.e. only on a heap span above 256 MiB.
pub(in crate::threading::thread_registry) const FOLD_FILTER_GRANULE_SHIFT: u32 = 20;

/// A 256-bit summary of a set of addresses: bit `(addr >> 20) & 255`. Two
/// sets with a common address share a bit, so `!a.intersects(&b)` PROVES
/// them disjoint. The converse does not hold; a false "may intersect" costs
/// only the full fold it would have run anyway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FoldAddrFilter(pub(in crate::threading::thread_registry) [u64; 4]);

impl FoldAddrFilter {
    #[inline]
    pub(in crate::threading::thread_registry) fn bucket(addr: usize) -> usize {
        (addr >> FOLD_FILTER_GRANULE_SHIFT) & 255
    }

    #[inline]
    pub(crate) fn insert(&mut self, addr: usize) {
        let b = Self::bucket(addr);
        self.0[b >> 6] |= 1u64 << (b & 63);
    }

    /// Every granule of `[lo, hi)`.
    pub(crate) fn insert_range(&mut self, lo: usize, hi: usize) {
        if hi <= lo {
            return;
        }
        let first = lo >> FOLD_FILTER_GRANULE_SHIFT;
        let last = (hi - 1) >> FOLD_FILTER_GRANULE_SHIFT;
        if last - first >= 255 {
            self.0 = [u64::MAX; 4];
            return;
        }
        for g in first..=last {
            self.insert(g << FOLD_FILTER_GRANULE_SHIFT);
        }
    }

    #[inline]
    pub(crate) fn intersects(&self, other: &FoldAddrFilter) -> bool {
        self.0.iter().zip(other.0.iter()).any(|(a, b)| a & b != 0)
    }

    pub(in crate::threading::thread_registry) fn is_full(&self) -> bool {
        self.0.iter().all(|w| *w == u64::MAX)
    }
}

/// The filter of everything one blocked fold can rewrite or report: every
/// key of `pointer_map` (each lookup the fold makes is a key test) and the
/// arena the cycle vacated (`vacated`: the Generational inactive semispace
/// and a pinned in-place cycle's spans), which the fold's `blocked-root-gap`
/// audits test the same words against. Stops early once every bit is set.
pub(in crate::threading::thread_registry) fn pointer_map_fold_filter(
    pointer_map: &cratonvm_types::PointerMap,
    vacated: (Option<(usize, usize)>, &[(usize, usize)]),
) -> FoldAddrFilter {
    let mut f = FoldAddrFilter::default();
    let (inactive, spans) = vacated;
    if let Some((lo, hi)) = inactive {
        f.insert_range(lo, hi);
    }
    for &(a, len) in spans {
        f.insert_range(a, a.saturating_add(len));
    }
    for (i, &k) in pointer_map.keys().enumerate() {
        if i % 4096 == 0 && f.is_full() {
            break;
        }
        f.insert(k);
    }
    f
}

/// One registry entry's cached [`FoldAddrFilter`] (`ThreadEntry::parked_filter`):
/// valid only while `ThreadEntry::mount_epoch` is the value it was stored for.
/// Written and read only by a pause's blocked fold (pauses are serialised by
/// the barrier), so `Relaxed` words under an `Acquire` / `Release` tag.
pub(crate) struct ParkedFoldFilter {
    /// `mount_epoch + 1` at the store; `0` = no filter.
    pub(in crate::threading::thread_registry) valid_at: AtomicU64,
    pub(in crate::threading::thread_registry) bits: [AtomicU64; 4],
}

impl ParkedFoldFilter {
    pub(crate) fn new() -> Self {
        Self {
            valid_at: AtomicU64::new(0),
            bits: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// The filter stored for `epoch`, if that is the one stored.
    pub(crate) fn cached(&self, epoch: u64) -> Option<FoldAddrFilter> {
        if self.valid_at.load(Ordering::Acquire) != epoch.wrapping_add(1) {
            return None;
        }
        Some(FoldAddrFilter(std::array::from_fn(|i| {
            self.bits[i].load(Ordering::Relaxed)
        })))
    }

    pub(crate) fn store(&self, epoch: u64, f: FoldAddrFilter) {
        self.valid_at.store(0, Ordering::Relaxed);
        for (cell, word) in self.bits.iter().zip(f.0) {
            cell.store(word, Ordering::Relaxed);
        }
        self.valid_at.store(epoch.wrapping_add(1), Ordering::Release);
    }

    pub(crate) fn clear(&self) {
        self.valid_at.store(0, Ordering::Release);
    }
}

/// gci i13/vg: `ThreadRegistry::parked_split` -- the young filter armed for
/// the next gather, and the old-only section that gather left.
///
/// gci i21/pr: also the collection door's initiator-snapshot skip
/// ([`ThreadRegistry::arm_initiator_snapshot_skip`]) -- the same one-gather
/// arm, consumed by the same `collect_all_root_snapshots` -- and the words
/// that gather left out (`None`: no armed gather ran since the arm).
#[derive(Default)]
pub(in crate::threading::thread_registry) struct ParkedSplitGather {
    /// gcj j3/x2 R10: with the arming thread; only a gather on it consumes it.
    pub(in crate::threading::thread_registry) armed: Option<(FoldAddrFilter, std::thread::ThreadId)>,
    pub(in crate::threading::thread_registry) old_only: Vec<ObjectRef>,
    /// gcj j3/x2 R10: plain gathers on another thread that left the split alone.
    pub(in crate::threading::thread_registry) split_foreign_gathers: u64,
    /// gcj j3/x2: with the arming thread; only a gather on it consumes the arm.
    pub(in crate::threading::thread_registry) initiator_skip: Option<(Arc<Mutex<Vec<ObjectRef>>>, std::thread::ThreadId)>,
    pub(in crate::threading::thread_registry) initiator_skipped: Option<usize>,
    /// gcj j3/x2: plain gathers on another thread that left an armed skip alone.
    pub(in crate::threading::thread_registry) initiator_foreign_gathers: u64,
}

/// gci i13/vg: may this entry's snapshot go to the OLD-ONLY root section of a
/// split gather ([`ThreadRegistry::collect_all_root_snapshots_split`])?
///
/// Only an UNMOUNTED continuation parked in its blocked region (its words are
/// the yield deposit's, advanced only by the blocked fold since), with a
/// cached [`FoldAddrFilter`] that is valid for the entry's CURRENT
/// `mount_epoch` and disjoint from `young`. The filter summarises every
/// snapshot word as the last fold left it (a superset: it also covers the
/// fixup values, native slots and slot origins), and an unchanged epoch means
/// no mount, unmount, snapshot or block-state replacement happened since
/// (`set_mounted` / `set_root_snapshot` / `set_gc_block_state` bump it), so
/// "disjoint" proves no snapshot word is a young address NOW. A mount racing
/// this pause bumps the epoch before its drain, and the drain waits the pause
/// out, so the words read here are still the parked ones either way.
pub(in crate::threading::thread_registry) fn parked_snapshot_is_old_only(entry: &ThreadEntry, young: &FoldAddrFilter) -> bool {
    if entry.mounted.load(Ordering::Acquire)
        || !entry
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
    {
        return false;
    }
    let epoch = entry.mount_epoch.load(Ordering::Acquire);
    entry
        .parked_filter
        .cached(epoch)
        .is_some_and(|f| !f.intersects(young))
}

/// gci i14/ve: `ThreadRegistry::mirror_keyed` -- the door's request for the
/// next gather, the `(tid, mirror)` pairs that gather left out (the same
/// pause's sweep takes them) and the ids that sweep retired (taken after the
/// pause).
#[derive(Default)]
pub(in crate::threading::thread_registry) struct MirrorKeyedGather {
    pub(in crate::threading::thread_registry) armed: Option<MirrorKeyedArm>,
    pub(in crate::threading::thread_registry) deferred: Vec<(ThreadId, usize)>,
    pub(in crate::threading::thread_registry) collected: Vec<ThreadId>,
    /// Pauses that keyed nothing because the virtual-thread manager's lock
    /// was contended at the arm (cumulative, per VM; census only).
    pub(in crate::threading::thread_registry) skipped_contended: u64,
    /// gci i15/vh (G1): the `(tid, mirror)` pairs the remark's completed mark
    /// left unmarked ([`ThreadRegistry::judge_mirror_keyed_parked_after_mark`]),
    /// retired or dropped by the same pause's
    /// [`ThreadRegistry::settle_mirror_keyed_parked_after_mark`] once cleanup
    /// said whether it trusted that mark.
    pub(in crate::threading::thread_registry) judged_dead: Vec<(ThreadId, usize)>,
    /// gci i15/vh: how many pairs that judgement read (census only).
    pub(in crate::threading::thread_registry) judged_deferred: usize,
    /// gci i15/vh: a G1 marking scan (initial mark or remark) keyed at least
    /// one thread since the last remark judgement; that judgement then marks
    /// the rows of every live owner to a fixed point first
    /// ([`mark_live_row_values_to_fixpoint`]). Taken by the judgement.
    pub(in crate::threading::thread_registry) g1_cycle_keyed: bool,
}

/// gci i14/ve: one mirror-keyed request ([`ThreadRegistry::arm_mirror_keyed_gather`]).
pub(crate) struct MirrorKeyedArm {
    /// This VM's identity: the `metadata_pin` rows are filed under it.
    pub(in crate::threading::thread_registry) vm: usize,
    /// The Generational old generation, `[old_base, old_base + old_len)`.
    pub(in crate::threading::thread_registry) old_base: usize,
    pub(in crate::threading::thread_registry) old_len: usize,
    /// gci i15/vh: G1's `Old` regions, sorted, non-overlapping `[lo, hi)`
    /// ranges ([`ThreadRegistry::arm_mirror_keyed_gather_ranges`]); empty on
    /// the Generational arm.
    pub(in crate::threading::thread_registry) old_ranges: Vec<(usize, usize)>,
    /// Registry ids of the JDK continuations deposited in the manager.
    pub(in crate::threading::thread_registry) candidates: rustc_hash::FxHashSet<u64>,
}

impl MirrorKeyedArm {
    pub(in crate::threading::thread_registry) fn is_old(&self, addr: usize) -> bool {
        (self.old_len != 0 && addr.wrapping_sub(self.old_base) < self.old_len)
            || addr_in_sorted_ranges(&self.old_ranges, addr)
    }
}

/// gci i15/vh: is `addr` inside one of `ranges` (sorted by start,
/// non-overlapping, each `[lo, hi)`)? Binary search; `false` when empty.
pub(in crate::threading::thread_registry) fn addr_in_sorted_ranges(ranges: &[(usize, usize)], addr: usize) -> bool {
    let i = ranges.partition_point(|&(_, hi)| hi <= addr);
    ranges.get(i).is_some_and(|&(lo, hi)| lo <= addr && addr < hi)
}

/// gci i15/vh: hand `mark` the values of every row of `rows` (owner ->
/// values, a `metadata_pin::snapshot`) whose owner `is_live`, then again for
/// the owners that turned live through those marks, until none does. Each
/// owner's values are handed over once; the loop ends because every round
/// that marks anything takes at least one owner out of the remaining set.
/// `is_live` must read the marks `mark` makes (the G1 door passes
/// `is_live_after_mark` and `resurrect_after_remark`). Returns how many
/// values were handed over. See
/// [`ThreadRegistry::judge_mirror_keyed_parked_after_mark`].
///
/// gci i16/mr: `pub(crate)` for the G1 remark's default-path closure of every
/// row (`gc_and_alloc.rs` `g1_remark_close_metadata_rows`,
/// `gci-i15vh-g1-marking-scan-rows-filed-while-the-marker-runs-FIXED-20261017`).
pub(crate) fn mark_live_row_values_to_fixpoint(
    // addr-keyed: borrowed for one remark (the caller's snapshot of the rows, consumed under the pause).
    rows: &FxHashMap<usize, Vec<usize>>,
    is_live: &dyn Fn(usize) -> bool,
    mark: &mut dyn FnMut(&[usize]),
) -> usize {
    let mut pending: Vec<usize> = rows.keys().copied().collect();
    let mut handed = 0usize;
    loop {
        let (live, dead): (Vec<usize>, Vec<usize>) =
            pending.into_iter().partition(|&owner| is_live(owner));
        pending = dead;
        let values: Vec<usize> = live
            .iter()
            .filter_map(|owner| rows.get(owner))
            .flat_map(|v| v.iter().copied())
            .collect();
        if live.is_empty() {
            return handed;
        }
        if !values.is_empty() {
            handed += values.len();
            mark(&values);
        }
        if pending.is_empty() {
            return handed;
        }
    }
}

/// gci i15/vh (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED` on G1): remove from a
/// plain `roots::collect_roots` result exactly ONE occurrence of each mirror
/// a keyed gather left out (`mirrors`, one entry per keyed thread). Step 10b
/// of that scan pushes `alive_thread_objects(usize::MAX)` -- every alive
/// entry's `java_thread_obj`, each exactly once -- and a keyed entry is alive
/// with that mirror, so one occurrence is step 10b's; any other occurrence
/// is a real root (a JNI global, another frame, a `ThreadLocal` handle) and
/// stays. The Generational door has no such duplicate
/// (`collect_roots_registry_appended` skips step 10b). Returns how many were
/// removed; a mirror not found removes nothing (never a real root).
pub(crate) fn strip_keyed_mirror_registry_roots(
    roots: &mut Vec<ObjectRef>,
    mirrors: &[usize],
) -> usize {
    if mirrors.is_empty() {
        return 0;
    }
    // addr-keyed: local to this fixed point (one remark).
    let mut owed: rustc_hash::FxHashMap<usize, usize> = rustc_hash::FxHashMap::default();
    for &m in mirrors {
        *owed.entry(m).or_insert(0) += 1;
    }
    let mut removed = 0usize;
    roots.retain(|r| {
        let addr = r.as_ptr() as usize;
        match owed.get_mut(&addr) {
            Some(n) if *n > 0 => {
                *n -= 1;
                removed += 1;
                false
            }
            _ => true,
        }
    });
    removed
}

/// gci i14/ve: what a mirror-keyed gather produced: `(mirror, word)` rows and
/// the `(tid, mirror)` of every entry it left out.
#[derive(Default)]
pub(crate) struct MirrorKeyedOut {
    pub(in crate::threading::thread_registry) rows: Vec<(usize, usize)>,
    pub(in crate::threading::thread_registry) deferred: Vec<(ThreadId, usize)>,
}

/// gci i14/ve: may this entry be keyed by its mirror on this pause
/// ([`ThreadRegistry::collect_all_root_snapshots`])? `Some(mirror address)`
/// only for:
///
/// * a JDK continuation whose state is deposited in the virtual-thread
///   manager (`arm.candidates`: the yield finished, no mount has taken it,
///   no debugger is reading it), UNMOUNTED and in its blocked region (the
///   words are the yield deposit's, advanced only by the blocked fold);
/// * a PRECISE deposit (`deposit_conservative` down): every word is an exact
///   object reference (a tagged frame slot recorded in `slot_origins`, an
///   off-frame reference, a frame's class owner), so a word a marker reaches
///   only through the row is relocated by the blocked fold exactly as a
///   rooted one; a conservative deposit's extra words may be untagged slots
///   that only a root's pinning protects, so it stays rooted;
/// * a mirror and EVERY word in the old generation. A young-only collection
///   then neither frees nor moves any of them and reads none of them (its
///   old -> young edges come from the cards), so leaving them out changes
///   nothing it does; every phase that marks old space (the majors' old
///   BFS, the true-root trace, the non-moving old sweep) follows a marked
///   owner's `metadata_pin` row, and the old-generation verdict the sweep
///   asks is exact. A young mirror or word keeps the whole entry rooted.
///
/// Every other entry is gathered as before.
pub(in crate::threading::thread_registry) fn mirror_keyed_mirror(
    tid: ThreadId,
    entry: &ThreadEntry,
    snapshot: &[ObjectRef],
    arm: &MirrorKeyedArm,
) -> Option<usize> {
    if !arm.candidates.contains(&tid.0)
        || entry.mounted.load(Ordering::Acquire)
        || !entry
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
        || entry
            .gc_block_state
            .deposit_conservative
            .load(Ordering::Acquire)
    {
        return None;
    }
    let mirror = entry.java_thread_obj?.as_ptr() as usize;
    if !arm.is_old(mirror) {
        return None;
    }
    snapshot
        .iter()
        .all(|w| arm.is_old(w.as_ptr() as usize))
        .then_some(mirror)
}

/// gci i14/ve: the `(tid, mirror)` pairs of `deferred` whose mirror the pause
/// did not keep: not moved (no `pointer_map` key) and not `survived` in place.
/// Pure, for its test.
pub(in crate::threading::thread_registry) fn mirror_keyed_dead(
    deferred: &[(ThreadId, usize)],
    pointer_map: &cratonvm_types::PointerMap,
    survived: &dyn Fn(usize) -> bool,
) -> Vec<(ThreadId, usize)> {
    deferred
        .iter()
        .copied()
        .filter(|&(_, mirror)| pointer_map.get(&mirror).is_none() && !survived(mirror))
        .collect()
}

/// gci i18/hw (`gci-i15orch-g1-parked-virtual-thread-holder-graph-reads-wrong-on-the-default-path-RETIRED-UNFIXED-20261018`):
/// does a blocking deposit (`NativeContextImpl::deposit_root_snapshot_inner_with`)
/// skip the conservative-locals probes for this thread? Only for a virtual
/// thread's continuation (`ThreadKind::Virtual`) on G1, with
/// `CRATONVM_G1_VT_PRECISE_DEPOSIT` on (the default).
///
/// The probes (`roots::conservative_locals_enabled`: every local of every frame
/// as a root, plus the tag-blind `scan_locals_conservative` /
/// `scan_object_refs_conservative` candidates) run while ANY thread is in
/// compiled code, to catch a lost-tag local a compiled callee left in an
/// interpreted caller. Their contract is "sound ONLY under the non-moving
/// sweep", which is what that condition selects on Generational. On G1 every
/// pause evacuates, and a continuation never runs compiled code
/// (`jit_bridge::assert_no_compiled_continuation`), so it has no such local:
/// the probes add only dead locals' words and tag-blind candidates. A parked
/// continuation's yield deposit is its only root for the whole park, so on a
/// JIT run on G1 those extra words (11 a thread on the holder probe: 34 words
/// against 23 under `--nojit`) were roots of every pause, and the
/// mirror-keyed arm (which needs a precise deposit) keyed none of them.
///
/// Generational keeps the probes (its JIT-active sweep is the non-moving one
/// they were written for, and its unrecorded-root guard reads the flag). Pure,
/// for its test.
pub(crate) fn continuation_deposit_is_precise(
    is_g1: bool,
    is_continuation: bool,
    switch_on: bool,
) -> bool {
    is_g1 && is_continuation && switch_on
}

/// gci i19/vd: the `is_continuation` input of [`continuation_deposit_is_precise`]
/// for `thread`'s own blocking deposit -- a virtual thread's continuation
/// (`ThreadKind::Virtual`) that has never entered compiled code
/// (`GcBlockState::continuation_entered_compiled`, set by
/// `jit_bridge::assert_no_compiled_continuation` in every build).
///
/// Why the second half: the precise deposit drops the tag-blind probes, whose
/// one job is a lost-tag local a compiled callee left in an interpreted frame.
/// A continuation has none only because no compiled code ran on it, and that
/// rule is enforced by per-door `ThreadKind::Virtual` declines and checked
/// only by a `debug_assert!`. With the sticky flag the precise arm covers
/// exactly the continuations that provably ran interpreted-only; one that a
/// door let into compiled code deposits conservatively, as before i18, for
/// the rest of its life. Every other live word of a parked continuation is in
/// the precise set by construction: its frames live in its `JvmThread` (the
/// JDK path's `StackChunk` holds only ints, `jdk_continuation::mark_tail`), the
/// liveness-mapped local scan with its type half is the scan `collect_roots`
/// and `update_root_snapshot` use, the operand stack keeps its tag-ruled scan,
/// monitors / class owners / off-frame roots / JNI locals are unchanged, and a
/// runner OS thread that ran no compiled code has no JIT chain, spill slots or
/// deopt stash to add. See `i19-vd-report.md`.
#[inline]
pub(crate) fn continuation_is_interpreted_only(thread: &crate::threading::JvmThread) -> bool {
    matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        && !thread
            .gc_block_state
            .continuation_entered_compiled
            .load(Ordering::Relaxed)
}

/// `CRATONVM_DBG_VT_PARKED_ROOTS`: one root gather's census
/// ([`ThreadRegistry::collect_all_root_snapshots`]).
#[derive(Default, Debug)]
pub(in crate::threading::thread_registry) struct ParkedGatherCensus {
    pub(in crate::threading::thread_registry) alive: usize,
    pub(in crate::threading::thread_registry) parked: usize,
    pub(in crate::threading::thread_registry) words: usize,
    pub(in crate::threading::thread_registry) parked_words: usize,
}

impl ParkedGatherCensus {
    pub(in crate::threading::thread_registry) fn note(&mut self, unmounted: bool, words: usize) {
        self.alive += 1;
        self.words += words;
        if unmounted {
            self.parked += 1;
            self.parked_words += words;
        }
    }

    pub(in crate::threading::thread_registry) fn line(&self, roots: usize, elapsed: std::time::Duration) -> String {
        format!(
            "[vt-parked-roots] gather alive={} parked={} words={} parked_words={} roots={} us={}",
            self.alive,
            self.parked,
            self.words,
            self.parked_words,
            roots,
            elapsed.as_micros()
        )
    }
}

/// `CRATONVM_DBG_VT_PARKED_ROOTS`: one blocked fold's census
/// ([`ThreadRegistry::fold_pointer_map_into_blocked_audited`]). `parked` are
/// the blocked, unmounted entries; `parked_words` the words the fold walked
/// for those it did not skip.
#[derive(Default, Debug)]
pub(in crate::threading::thread_registry) struct ParkedFoldCensus {
    pub(in crate::threading::thread_registry) blocked: usize,
    pub(in crate::threading::thread_registry) parked: usize,
    pub(in crate::threading::thread_registry) cached: usize,
    pub(in crate::threading::thread_registry) skipped: usize,
    pub(in crate::threading::thread_registry) stored: usize,
    pub(in crate::threading::thread_registry) parked_words: usize,
    pub(in crate::threading::thread_registry) map_filter_nanos: u128,
}

impl ParkedFoldCensus {
    pub(in crate::threading::thread_registry) fn line(&self, map_len: usize, filter_on: bool, elapsed: std::time::Duration) -> String {
        format!(
            "[vt-parked-roots] fold filter={} map={} blocked={} parked={} cached={} skipped={} \
             stored={} parked_words={} map_filter_us={} us={}",
            if filter_on { "on" } else { "off" },
            map_len,
            self.blocked,
            self.parked,
            self.cached,
            self.skipped,
            self.stored,
            self.parked_words,
            self.map_filter_nanos / 1000,
            elapsed.as_micros()
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// One blocked thread's share of
/// [`ThreadRegistry::fold_pointer_map_into_blocked_audited`]: compose the
/// existing fixup chain through `pointer_map`, seed a chain entry for every
/// snapshot address this map moves for the first time, and remap the snapshot
/// in place. Returns `(composed, seeded)`.
///
/// gc-common w4-a (`handoff-w3g-blocked-fold-costs` edit 1,
/// `common-w2g-per-gc-remap-costs` §1): the set of already-chained addresses
/// (the chain's PRE-compose values) was rebuilt for every blocked thread on
/// every relocating pause -- an allocation and a hash insert per entry of a
/// chain that only grows while the thread stays blocked -- although only a
/// snapshot entry that this map MOVES reads it. It is now built on the first
/// such entry, and the snapshot pass runs BEFORE the compose so the values it
/// is built from are still pre-compose without a copy; the seeds are inserted
/// after the compose, exactly where the old order inserted them, so a seed is
/// never composed through the map that produced it. A thread none of whose
/// snapshot moved pays one probe per snapshot entry and nothing else.
pub(in crate::threading::thread_registry) fn fold_one_blocked_chain(
    fixup: &mut cratonvm_types::PointerMap,
    snapshot: &mut [ObjectRef],
    pointer_map: &cratonvm_types::PointerMap,
) -> (usize, usize) {
    // Addresses already chained (frame holds the ORIG key, the snapshot holds
    // the CUR value), PRE-compose. `None` until a snapshot entry moves.
    // addr-keyed: local to this fold (one pause).
    let mut chained: Option<rustc_hash::FxHashSet<usize>> = None;
    let mut seeds: Vec<(usize, usize)> = Vec::new();
    for r in snapshot.iter_mut() {
        let s = r.as_ptr() as usize;
        if let Some(&new) = pointer_map.get(&s) {
            let already = chained
                .get_or_insert_with(|| fixup.values().copied().collect())
                .contains(&s);
            if !already {
                // First move of this object while blocked: the frames hold `s`
                // itself.
                seeds.push((s, new));
            }
            // SAFETY: `new` comes from the GC pointer map and points at the
            // relocated object's header.
            *r = unsafe { ObjectRef::from_raw(new as *mut u8) };
        }
    }
    let mut composed = 0usize;
    for cur in fixup.values_mut() {
        if let Some(&new) = pointer_map.get(cur) {
            *cur = new;
            composed += 1;
        }
    }
    let seeded = seeds.len();
    for (s, new) in seeds {
        // `or_insert` guards the ABA case where `s` was recycled and is also an
        // existing chain key for a DIFFERENT (older) object -- whose value the
        // compose above has already advanced.
        fixup.entry(s).or_insert(new);
    }
    (composed, seeded)
}

/// `CRATONVM_DBG_BLOCKGC`, latched (it was an uncached environment probe once
/// per relocating pause).
pub(in crate::threading::thread_registry) fn blockgc_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)}

/// `CRATONVM_DBG_ROOTFIXUP`, latched: the switch `update_all_roots`' root
/// fix-up census reads.
pub(in crate::threading::thread_registry) fn blocked_fold_census_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)}

pub(in crate::threading::thread_registry) static BLOCKED_FOLD_CALLS: AtomicU64 = AtomicU64::new(0);
pub(in crate::threading::thread_registry) static BLOCKED_FOLD_THREADS: AtomicU64 = AtomicU64::new(0);
pub(in crate::threading::thread_registry) static BLOCKED_FOLD_NANOS: AtomicU64 = AtomicU64::new(0);

/// Times one fold into the `blocked_fold` census on drop.
pub(in crate::threading::thread_registry) struct BlockedFoldTimer {
    pub(in crate::threading::thread_registry) started: std::time::Instant,
}

impl BlockedFoldTimer {
    pub(in crate::threading::thread_registry) fn start() -> Self {
        BlockedFoldTimer {
            started: std::time::Instant::now(),
        }
    }

    pub(in crate::threading::thread_registry) fn note_thread(&self) {
        BLOCKED_FOLD_THREADS.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for BlockedFoldTimer {
    fn drop(&mut self) {
        BLOCKED_FOLD_CALLS.fetch_add(1, Ordering::Relaxed);
        BLOCKED_FOLD_NANOS.fetch_add(
            u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

/// `(folds, blocked_threads_folded, nanos)` of the in-pause blocked-thread fold
/// under `CRATONVM_DBG_ROOTFIXUP=1` -- the `blocked_fold` share of the root
/// fix-up census (`memory::gc::root_fixup_census`, whose total already
/// contains it). gc-common w4-a; printed at exit by the launcher
/// (`handoff-w4a-print-the-blocked-fold-census`).
pub fn blocked_fold_census() -> (u64, u64, u64) {
    (
        BLOCKED_FOLD_CALLS.load(Ordering::Relaxed),
        BLOCKED_FOLD_THREADS.load(Ordering::Relaxed),
        BLOCKED_FOLD_NANOS.load(Ordering::Relaxed),
    )
}

