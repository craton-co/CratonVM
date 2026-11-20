use super::*;
use std::sync::Arc;

/// A pause must be VISIBLE to a sampler that is not the initiator and holds
/// no barrier handle.
///
/// This is the contract `virtual_threads::spawn_starvation_watchdog` rests
/// on. That watchdog grows the carrier pool when `dispatch_count` has not
/// moved since its last sample — and a stop-the-world pause parks every
/// carrier, so `dispatch_count` CANNOT move across one. Without a way to
/// tell "stopped" from "starved" it grew the pool once per pause, and each
/// added carrier is one more OS thread for the next pause to stop (and, on
/// Windows, one more for `xt_root_scan::take_over_pass` to suspend and
/// resume): slower pause -> more stalled samples -> more carriers. Measured
/// on `VthreadGcStress`, the pool ran from its base 32 to 233 and
/// `dispatch_count` froze permanently.
///
/// # What is and is not assertable here
///
/// [`STW_PAUSE_EPOCH`] and [`STW_PAUSES_IN_FLIGHT`] describe THE VM, of
/// which production has exactly one — but this test binary builds many
/// `GcBarrier`s and runs their tests in parallel, so a sibling pause can
/// advance the epoch between any two lines below. Only MONOTONICITY of the
/// epoch survives that, and monotonicity is what the watchdog needs:
/// it compares the epoch against its own previous sample and asks whether
/// it changed, never what it changed by. The per-instance `stw_requested`
/// is asserted alongside it because that one IS deterministic here.
#[test]
fn a_pause_is_visible_to_a_sampler_with_no_barrier_handle() {
    let barrier = GcBarrier::new();
    let (epoch_before, _) = stw_pause_state();

    assert!(
        barrier.request_stw(ThreadId(1), 1),
        "the first request on a fresh barrier must be accepted",
    );
    let (epoch_during, in_progress) = stw_pause_state();
    // Deterministic since w3-a: an in-flight COUNT cannot be cleared by a
    // sibling barrier's `complete_gc` while this pause is outstanding.
    assert!(
        in_progress,
        "a sibling VM's pause ending must not report this VM's pause as over"
    );
    assert!(
        epoch_during > epoch_before,
        "requesting a pause must advance the global epoch, or a sampler holding no handle to this barrier cannot tell a stopped VM from a starved one (epoch {epoch_before} -> {epoch_during})",
    );
    assert!(
        barrier.stw_requested.load(Ordering::Acquire),
        "the per-instance flag must be set while the pause is outstanding",
    );

    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(
        !barrier.stw_requested.load(Ordering::Acquire),
        "completing the collection must clear the per-instance flag",
    );
    let (epoch_after, _) = stw_pause_state();
    assert!(
        epoch_after >= epoch_during,
        "the epoch counts pauses STARTED and must never go backwards",
    );
}

#[test]
fn barrier_no_stw_by_default() {
    let barrier = GcBarrier::new();
    assert!(!barrier.stw_requested.load(Ordering::Relaxed));
    assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 0);
}

/// Interpreter round i1 wave 26, lane L7: the dispatch loop polls its
/// thread's `LoopPollWord`, not `stw_requested`. A request counts its
/// pause in every registered word together with the flag, `complete_gc`
/// uncounts it together with the flag, a word registered mid-pause is
/// counted once, a re-registration never counts twice, and a move count
/// shares the word and survives all of it.
#[test]
fn a_pause_is_counted_in_every_registered_loop_poll_word() {
    let pauses = |h: &LoopPollHandle| h.word().load() & LoopPollWord::PAUSES;
    let barrier = GcBarrier::new();
    let stack_a = LoopPollHandle::new();
    let stack_b = LoopPollHandle::new();
    barrier.register_loop_poll_word(stack_a.word());
    barrier.register_loop_poll_word(stack_b.word());
    // Registering twice keeps one entry.
    barrier.register_loop_poll_word(stack_a.word());
    assert_eq!(barrier.loop_poll_words.lock().len(), 2);
    assert_eq!(stack_a.word().registered_with(), barrier.loop_poll_key());
    stack_a.word().note_code_moved();
    assert_eq!(
        stack_a.word().load(),
        LoopPollWord::MOVE_STEP,
        "a move, no pause"
    );

    assert!(barrier.request_stw(ThreadId(1), 1));
    assert_eq!(pauses(&stack_a), 1, "counted with the flag");
    assert_eq!(pauses(&stack_b), 1, "counted with the flag");
    // A re-registration during the pause does not count it again.
    barrier.register_loop_poll_word(stack_a.word());
    assert_eq!(pauses(&stack_a), 1);
    // A move during the pause does not disturb the count.
    stack_a.word().note_code_moved();
    assert_eq!(stack_a.word().load(), (2 * LoopPollWord::MOVE_STEP) + 1);
    // A word registered while the pause is up is counted once, now.
    let late = LoopPollHandle::new();
    barrier.register_loop_poll_word(late.word());
    assert_eq!(pauses(&late), 1, "registered mid-pause");

    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(
        stack_a.word().load(),
        2 * LoopPollWord::MOVE_STEP,
        "uncounted, moves kept"
    );
    assert_eq!(pauses(&stack_b), 0);
    assert_eq!(pauses(&late), 0);
    // A completion with no pause in progress uncounts nothing.
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(pauses(&stack_b), 0);

    // A dropped stack's word is forgotten at the next registration.
    drop(stack_b);
    barrier.register_loop_poll_word(stack_a.word());
    assert_eq!(
        barrier.loop_poll_words.lock().len(),
        2,
        "stack_a and late remain"
    );

    // The dispatch loop's entry point registers this OS thread's word, once.
    let this_thread = barrier.loop_poll_word_for_this_thread();
    assert_eq!(barrier.loop_poll_word_for_this_thread(), this_thread);
    assert_eq!(barrier.loop_poll_words.lock().len(), 3);
    assert!(barrier.request_stw(ThreadId(1), 1));
    // SAFETY: this test thread's thread-local word, alive for the whole test.
    assert_eq!(unsafe { &*this_thread }.load() & LoopPollWord::PAUSES, 1);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(unsafe { &*this_thread }.load() & LoopPollWord::PAUSES, 0);
}

/// gcj j1/cm: a barrier dropped mid-pause uncounts the pause from the
/// loop words registered with it, so another VM's loops on the same OS
/// thread do not see a pause that will never end.
#[test]
fn gcj1cm_a_barrier_dropped_mid_pause_uncounts_its_loop_words() {
    let word = LoopPollHandle::new();
    {
        let barrier = GcBarrier::new();
        barrier.register_loop_poll_word(word.word());
        assert!(barrier.request_stw(ThreadId(1), 1));
        assert_eq!(word.word().load() & LoopPollWord::PAUSES, 1);
    }
    assert_eq!(word.word().load() & LoopPollWord::PAUSES, 0);
}

/// gcj j2/cm2 (`CRATONVM_GC_HANDSHAKE_TARGET_KICK`): a thread publishes
/// the poll word its loops run on, and a kick of that publication moves
/// that word alone by one whole trigger step (no pause counted). Nothing
/// published, or a word that is not the publishing thread's own, kicks
/// nothing (the caller then kicks every loop).
#[test]
fn gcj2cm2_a_published_loop_word_is_kicked_alone() {
    use crate::threading::jvm_thread::GcBlockState;
    let barrier = GcBarrier::new();
    let own = barrier.loop_poll_word_for_this_thread();
    let state = GcBlockState::new();
    assert!(!state.kick_published_loop_poll_word(), "nothing published");
    state.publish_loop_poll_word(own);
    // SAFETY: this test thread's own word, alive for the test.
    let before = unsafe { &*own }.load();
    assert!(state.kick_published_loop_poll_word());
    let after = unsafe { &*own }.load();
    assert_eq!(after.wrapping_sub(before), LoopPollWord::MOVE_STEP);
    assert_eq!(after & LoopPollWord::PAUSES, 0, "no pause counted");
    // Another OS thread's word, published from here, is not this thread's:
    // published as none.
    let other = std::thread::spawn(this_threads_loop_poll_word)
        .join()
        .expect("the other thread")
        .expect("its word");
    let elsewhere = GcBlockState::new();
    elsewhere.publish_loop_poll_word(Arc::as_ptr(&other));
    assert!(!elsewhere.kick_published_loop_poll_word());
    // A word whose OS thread has ended is not kicked either.
    let ended = LoopPollHandle::new();
    let word = Arc::clone(ended.word());
    drop(ended);
    assert!(!word.kick());
}

/// t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`): the handshake arm lives in
/// the byte compiled polls test, beside the pause count, and neither
/// disturbs the other; the loop's gate (`& !PAUSES`) ignores it; the
/// owner's clear answers whether it was up exactly once; an arm after the
/// clear stays up; a published word is armed through its `GcBlockState`;
/// an ended owner's word is not armed.
#[test]
fn t1e_the_handshake_arm_shares_the_polled_byte_with_the_pause_count() {
    use crate::threading::jvm_thread::GcBlockState;
    let word = LoopPollHandle::new();
    let w = word.word();
    let gate = w.load() & !LoopPollWord::PAUSES;
    assert!(w.arm_handshake());
    assert_eq!(w.load() & LoopPollWord::PAUSES, LoopPollWord::HANDSHAKE);
    assert_eq!(w.load() & !LoopPollWord::PAUSES, gate, "the gate is unchanged");
    // A pause counted while armed: both visible, neither lost.
    w.note_pause_begin();
    assert_eq!(w.load() & LoopPollWord::PAUSE_COUNT, 1);
    assert!(w.take_handshake_arm());
    assert!(!w.take_handshake_arm(), "cleared once");
    assert_eq!(w.load() & LoopPollWord::PAUSE_COUNT, 1, "the count survives the clear");
    w.note_pause_end();
    assert_eq!(w.load() & LoopPollWord::PAUSES, 0);
    // Re-armed after the clear: up for the next poll.
    assert!(w.arm_handshake());
    assert_eq!(w.load() & LoopPollWord::HANDSHAKE, LoopPollWord::HANDSHAKE);
    assert!(w.take_handshake_arm());
    // Through a publication of this thread's own word.
    let barrier = GcBarrier::new();
    let own = barrier.loop_poll_word_for_this_thread();
    let state = GcBlockState::new();
    let me = crate::threading::thread_registry::current_os_tid();
    assert!(!state.arm_published_loop_poll_word_handshake(Some(me)), "nothing published");
    state.publish_loop_poll_word(own);
    // SAFETY: this test thread's own word, alive for the test.
    let own_ref = unsafe { &*own };
    assert_eq!(own_ref.owner_os_tid(), me, "owned by the creating OS thread");
    // t1 lane E wave 3: not provably current -> not armed (the caller kicks).
    assert!(!state.arm_published_loop_poll_word_handshake(None), "unmounted / unknown");
    assert!(!state.arm_published_loop_poll_word_handshake(Some(0)), "no tid");
    assert!(
        !state.arm_published_loop_poll_word_handshake(Some(me.wrapping_add(1))),
        "remounted on another carrier"
    );
    assert_eq!(own_ref.load() & LoopPollWord::HANDSHAKE, 0, "nothing armed so far");
    if me != 0 {
        assert!(state.arm_published_loop_poll_word_handshake(Some(me)));
        assert!(take_this_threads_handshake_arm());
    }
    assert!(!take_this_threads_handshake_arm());
    assert_eq!(own_ref.load() & LoopPollWord::PAUSES, 0);
    // The barrier's always-raised fallback word has no owner tid.
    assert_eq!(barrier.always_polling.owner_os_tid(), 0);
    let ended = LoopPollHandle::new();
    let gone = Arc::clone(ended.word());
    drop(ended);
    assert!(!gone.arm_handshake());
    assert_eq!(gone.load() & LoopPollWord::HANDSHAKE, 0);
}

/// Interpreter round i6 wave 119, lane L2 (`CRATONVM_JIT_POLL_THREAD_WORD`;
/// the multi-VM gate of stage 3 of `i24-L3`): the invariant a compiled
/// poll reading the per-thread word rests on. A thread whose word is
/// registered with VM A's barrier and then runs VM B's compiled code went
/// through B's compiled-entry door (`helpers::PollWordMirrorScope::enter`),
/// which asks B's barrier for the word, so B's pause is counted in it and
/// B's pause cannot wait on a thread whose poll misses it. A's pauses stay
/// counted too (a word stays listed with every barrier it joined), so A's
/// code on the same thread is equally covered; the cross-counting costs
/// only slow-path visits that find no pause of their own VM.
#[test]
fn a_word_registered_with_another_vm_counts_this_vms_pause_once_its_door_asked() {
    let vm_a = GcBarrier::new();
    let vm_b = GcBarrier::new();
    let pauses = |w: *const LoopPollWord| {
        // SAFETY: this test thread's thread-local word, alive for the whole test.
        unsafe { &*w }.load() & LoopPollWord::PAUSES
    };
    // The thread last ran VM A's dispatch loop.
    let word = vm_a.loop_poll_word_for_this_thread();
    assert_eq!(unsafe { &*word }.registered_with(), vm_a.loop_poll_key());
    // VM B's compiled-entry door asks B's barrier: the same per-OS-thread
    // word, now in B's list as well.
    let from_b = vm_b.loop_poll_word_for_this_thread();
    assert_eq!(from_b, word, "one word per OS thread");
    assert_eq!(unsafe { &*word }.registered_with(), vm_b.loop_poll_key());
    // B's pause is seen by a poll reading the word ...
    assert!(vm_b.request_stw(ThreadId(1), 1));
    assert_eq!(pauses(word), 1, "VM B's pause reaches the word");
    vm_b.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(pauses(word), 0);
    // ... and so is A's, though the word now names B as its last barrier.
    assert!(vm_a.request_stw(ThreadId(1), 1));
    assert_eq!(pauses(word), 1, "VM A still counts its pause in the word");
    vm_a.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(pauses(word), 0);
    // Both pauses at once: counted twice, uncounted one by one.
    assert!(vm_a.request_stw(ThreadId(1), 1));
    assert!(vm_b.request_stw(ThreadId(1), 1));
    assert_eq!(pauses(word), 2);
    vm_a.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(pauses(word), 1, "B's pause is still up");
    vm_b.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(pauses(word), 0);
}

/// The other half of the gate: a word that never joined VM B's list (the
/// thread reached B's compiled code by a path that is not a door) does
/// NOT see B's pause, which is why such a path must find the mirror null
/// and fall back to B's baked flag (`PollWordMirrorScope` publishes only
/// inside a door, after asking that door's barrier).
#[test]
fn a_word_never_registered_with_a_vm_misses_its_pause() {
    let vm_a = GcBarrier::new();
    let vm_b = GcBarrier::new();
    let word = LoopPollHandle::new();
    vm_a.register_loop_poll_word(word.word());
    assert!(vm_b.request_stw(ThreadId(1), 1));
    assert_eq!(
        word.word().load() & LoopPollWord::PAUSES,
        0,
        "B never counted a word it was never given"
    );
    assert!(vm_b.stw_requested.load(Ordering::Acquire), "B's own flag is up");
    vm_b.complete_gc(cratonvm_types::PointerMap::default());
}

/// Cooperative JIT safepoint polling — the flag must sit within `disp32`
/// reach of the JIT code cache, so `emit_safepoint_poll` can use its
/// one-instruction `TEST BYTE [rip+disp32], 0xFF` form.
///
/// The sibling of `platform::tests::a_cell_is_within_disp32_of_a_code_buffer`,
/// which asserts the same thing about a bare cell. This one asserts it
/// about the byte that is actually polled, reached the way the JIT reaches
/// it (`stw_requested_flag_addr`), so it also fails if `CacheLineFlag` ever
/// goes back to being an inline field of this `Arc`-allocated struct — the
/// state that made every compiled poll in the process 15 bytes and a
/// clobbered R11 instead of 7 bytes and none.
///
/// x86-64 only; `disp32` reach is what makes the distance matter.
#[cfg(target_arch = "x86_64")]
#[test]
fn stw_requested_flag_is_within_disp32_of_the_code_cache() {
    use cratonvm_jit::platform::{alloc_executable, free_executable};

    if cratonvm_jit::platform::alloc_code_adjacent_cell().is_none() {
        // Either the OS refused the mapping or `CRATONVM_JIT_CODE_NEAR_GLOBALS`
        // owns placement and the cell allocator declines by design — in
        // which case the flag is SUPPOSED to stay on the VM heap, beside
        // the anchor that strategy pulls the code towards, and its distance
        // from an unhinted buffer means nothing.
        return;
    }
    let barrier = GcBarrier::new();
    let flag = barrier.stw_requested_flag_addr() as usize;
    let code = alloc_executable(4096).expect("alloc_executable failed");
    // Widening: usize -> i128, so the subtraction cannot wrap.
    let delta = (flag as i128) - (code as usize as i128);
    let in_reach = delta >= i32::MIN as i128 && delta <= i32::MAX as i128;
    free_executable(code, 4096);
    assert!(
        in_reach,
        "stw_requested flag {flag:#x} is {:.1} GB from a code buffer at \
         {:#x}; every compiled safepoint poll falls back to \
         MOV R11, imm64 ; TEST BYTE [R11], 0xFF",
        (delta.unsigned_abs() as f64) / (1024.0 * 1024.0 * 1024.0),
        code as usize,
    );
}

/// Cooperative JIT safepoint polling — the raw byte address must alias
/// `stw_requested` exactly: a plain byte read through the pointer must
/// track every transition the atomic makes.
#[test]
fn stw_requested_flag_addr_aliases_the_atomic_byte() {
    let barrier = GcBarrier::new();
    let addr = barrier.stw_requested_flag_addr();
    assert!(!addr.is_null());
    // SAFETY: `addr` points at `barrier.stw_requested`, which is alive
    // for the whole scope of this test.
    assert_eq!(unsafe { *addr }, 0, "expected false (0) initially");

    assert!(barrier.request_stw(ThreadId(0), 1));
    // SAFETY: same as above.
    assert_ne!(unsafe { *addr }, 0, "expected nonzero after request_stw");

    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    // SAFETY: same as above.
    assert_eq!(unsafe { *addr }, 0, "expected false (0) after complete_gc");
}

#[test]
fn barrier_request_and_complete_single_thread() {
    let barrier = GcBarrier::new();
    // Single alive thread (the initiator) → expected = 0
    assert!(barrier.request_stw(ThreadId(0), 1));
    assert!(barrier.stw_requested.load(Ordering::Relaxed));

    // No other threads to wait for
    barrier.wait_for_all();

    // Complete with empty pointer map
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(!barrier.stw_requested.load(Ordering::Relaxed));
    assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
}

#[test]
fn barrier_two_threads() {
    let barrier = Arc::new(GcBarrier::new());

    // Request STW with 2 alive threads
    assert!(barrier.request_stw(ThreadId(0), 2));

    let barrier2 = barrier.clone();
    let handle = std::thread::spawn(move || {
        // Non-initiator thread arrives at safepoint
        barrier2.arrive_and_wait(ThreadId(1))
    });

    // Initiator waits for the other thread
    barrier.wait_for_all();

    // Complete GC with a pointer remap
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(0x1000, 0x2000);
    barrier.complete_gc(pm);

    // Non-initiator thread should receive the pointer map
    let result_map = handle.join().unwrap();
    assert_eq!(result_map.get(&0x1000), Some(&0x2000));
    assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
}

/// Interpreter round i2 wave 57, lane L6: `moving_generation` counts only
/// the pauses that published a non-empty pointer map; `gc_generation`
/// counts every pause.
#[test]
fn i57_l6_moving_generation_counts_only_pauses_that_moved() {
    let barrier = GcBarrier::new();
    assert_eq!(barrier.moving_generation(), 0);
    assert!(barrier.request_stw(ThreadId(0), 1));
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    assert_eq!(barrier.moving_generation(), 0, "nothing moved");
    assert!(barrier.request_stw(ThreadId(0), 1));
    barrier.wait_for_all();
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(0x1000, 0x2000);
    barrier.complete_gc(pm);
    assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 2);
    assert_eq!(barrier.moving_generation(), 1);
}

#[test]
fn barrier_duplicate_request_rejected() {
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw(ThreadId(0), 1));
    // Second request while first is active should fail
    assert!(!barrier.request_stw(ThreadId(1), 2));
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

#[test]
fn barrier_initiator_not_blocked() {
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw(ThreadId(0), 1));
    // Initiator calling arrive_and_wait should return immediately
    let map = barrier.arrive_and_wait(ThreadId(0));
    assert!(map.is_empty());
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// B2 — an EXCLUDED (blocked) thread that wakes mid-STW and calls
/// `arrive_and_wait_excluded` must NOT count toward `arrived`, so it
/// cannot satisfy the quota and release the initiator's `wait_for_all`
/// while the genuine counted mutator has not yet arrived.
///
/// Scenario: 3 alive threads — initiator I, counted mutator M, blocked
/// (excluded) thread B. `request_stw` is told 1 thread is blocked, so
/// `expected = 3 - 1 (initiator) - 1 (blocked) = 1` (just M).
#[test]
fn barrier_excluded_thread_does_not_release_early() {
    let barrier = Arc::new(GcBarrier::new());
    // Pretend B is already parked in a blocking native.
    barrier.threads_blocked.store(1, Ordering::Release);
    assert!(barrier.request_stw(ThreadId(0), 3));
    // expected counts only the running mutator M.
    {
        let inner = barrier.inner.lock();
        assert_eq!(inner.expected, 1);
    }

    // B (excluded) wakes mid-STW and drains via the excluded entry.
    let b = barrier.clone();
    let hb = std::thread::spawn(move || b.arrive_and_wait_excluded(ThreadId(2)));

    // Give B time to run its (non-counting) arrival.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // The excluded arrival must NOT have satisfied the quota: M (the
    // only counted thread) has not arrived, so `arrived` stays 0 and
    // `wait_for_all` would still block. `pending_count` proves it.
    assert_eq!(
        barrier.pending_count(),
        1,
        "excluded thread must not be counted toward arrived",
    );

    // Now the counted mutator M arrives — quota satisfied.
    let m = barrier.clone();
    let hm = std::thread::spawn(move || m.arrive_and_wait(ThreadId(1)));

    // The initiator can now proceed.
    barrier.wait_for_all();
    assert_eq!(barrier.pending_count(), 0);

    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let _ = hm.join();
    let _ = hb.join();
}

/// B2 — sanity: with no excluded threads, the participating entry still
/// counts and releases exactly as before.
#[test]
fn barrier_participating_entry_counts() {
    let barrier = Arc::new(GcBarrier::new());
    assert!(barrier.request_stw(ThreadId(0), 2));
    let b2 = barrier.clone();
    let h = std::thread::spawn(move || b2.arrive_and_wait(ThreadId(1)));
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let _ = h.join();
    assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
}

/// BUG-03 — cross-thread JIT takeover may discover an in-JIT mutator only
/// after a bounded wait has already timed out. Reducing `expected` after
/// such a timeout must release the initiator once all non-cooperative peers
/// have been removed from the quota.
#[test]
fn barrier_late_reduce_expected_can_satisfy_bounded_wait() {
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw(ThreadId(0), 3));

    assert!(
        !barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)),
        "two non-initiator mutators are still pending"
    );

    barrier.reduce_expected(1);
    assert!(
        !barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)),
        "one mutator is still pending after the first takeover"
    );

    barrier.reduce_expected(1);
    assert!(
        barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)),
        "late takeover should satisfy the reduced barrier quota"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

#[test]
fn blocked_dead_transition_is_observed_atomically() {
    let barrier = GcBarrier::new();
    let guard = barrier.enter_blocked();
    let mut alive = 3u32; // initiator + live mutator + terminating blocked thread

    guard.finish_after(|| {
        alive = 2; // the terminating thread is now dead
    });

    assert!(barrier.request_stw_counted(ThreadId(0), || alive));
    {
        let inner = barrier.inner.lock();
        assert_eq!(
            inner.expected, 1,
            "dead blocked thread must not be subtracted from the live mutator quota",
        );
    }
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

#[test]
fn manual_blocked_dead_transition_is_observed_atomically() {
    let barrier = GcBarrier::new();
    let _pre_stw = barrier.mark_blocked_region_enter();
    let mut alive = 3u32; // initiator + live mutator + terminating blocked thread

    barrier.mark_blocked_region_leave_after(|| {
        alive = 2; // the terminating blocked thread is now dead
    });

    assert!(barrier.request_stw_counted(ThreadId(0), || alive));
    {
        let inner = barrier.inner.lock();
        assert_eq!(
            inner.expected, 1,
            "manual dead blocked thread must not be subtracted from the live mutator quota",
        );
    }
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}
#[test]
fn counted_request_ignores_stale_global_blocked_slots() {
    let barrier = GcBarrier::new();
    // Simulate a leaked/stale blocked slot not represented by any live
    // blocked registry entry. A plain global subtraction would set
    // expected=0 for two alive threads and let GC proceed without waiting
    // for the one live peer.
    barrier.threads_blocked.store(1, Ordering::Release);

    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
    {
        let inner = barrier.inner.lock();
        assert_eq!(inner.expected, 1);
    }
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

#[test]
fn counted_request_uses_live_blocked_before_global_counter_increment() {
    let barrier = GcBarrier::new();
    // A blocking thread deposits roots and sets its registry blocked flag
    // before `enter_blocked` increments the anonymous global count. Once the
    // root snapshot is published, STW must exclude it even if the global
    // counter still reads zero.
    barrier.threads_blocked.store(0, Ordering::Release);

    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (4, 2, vec![2, 3])));
    {
        let inner = barrier.inner.lock();
        assert_eq!(inner.expected, 1);
    }
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// GCAUDIT-0711-FIX (finding 1a) — `arrive_and_wait_auto` must resolve
/// EXACTLY like `arrive_and_wait_excluded` for a thread the pause's
/// census actually excluded, even though the caller itself cannot prove
/// that locally (see `GcBarrierInner::excluded_blocked`'s doc for why
/// `pre_stw`/`in_blocked_region` alone are ambiguous at the call site).
#[test]
fn auto_arrival_excluded_thread_does_not_release_early() {
    let barrier = Arc::new(GcBarrier::new());
    // 3 alive: initiator I(0), counted mutator M(1), blocked B(2).
    // The census reports B's identity as excluded.
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (3, 1, vec![2])));
    {
        let inner = barrier.inner.lock();
        assert_eq!(inner.expected, 1, "only M should be counted");
    }

    let b = barrier.clone();
    let hb = std::thread::spawn(move || b.arrive_and_wait_auto(ThreadId(2)));
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        barrier.pending_count(),
        1,
        "auto arrival for an excluded tid must not satisfy the quota",
    );

    let m = barrier.clone();
    let hm = std::thread::spawn(move || m.arrive_and_wait_auto(ThreadId(1)));
    barrier.wait_for_all();
    assert_eq!(barrier.pending_count(), 0);

    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let _ = hm.join();
    let _ = hb.join();
}

/// GCAUDIT-0711-FIX (finding 1a) — sanity: with no excluded threads,
/// `arrive_and_wait_auto` counts exactly like `arrive_and_wait`.
#[test]
fn auto_arrival_participating_by_default() {
    let barrier = Arc::new(GcBarrier::new());
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
    let b2 = barrier.clone();
    let h = std::thread::spawn(move || b2.arrive_and_wait_auto(ThreadId(1)));
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let _ = h.join();
    assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
}

/// GCAUDIT-0711-FIX (finding 1a) — `excluded_blocked` must not leak
/// across generations: a tid excluded by pause G must be treated as
/// participating by default in pause G+1 unless that pause's own
/// census excludes it again.
#[test]
fn excluded_blocked_does_not_leak_across_generations() {
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![1])));
    barrier.complete_gc(cratonvm_types::PointerMap::default());

    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
    {
        let inner = barrier.inner.lock();
        assert!(
            !inner.excluded_blocked.contains(&1),
            "stale exclusion from a completed pause must not survive complete_gc",
        );
    }
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// Finding 1(c) — `leave_blocked_region_flagged` must not clear the
/// caller's `in_blocked_region` flag while a pause that excluded the
/// caller is still active; the clear lands only once no pause is active,
/// with no start-and-complete window in between.
#[test]
fn leave_blocked_region_flagged_waits_out_active_pause() {
    use std::sync::atomic::AtomicBool;

    let barrier = Arc::new(GcBarrier::new());
    let flag = Arc::new(AtomicBool::new(true));

    // Pause active; thread 2 is excluded by the identity census.
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![2])));

    let b = barrier.clone();
    let f = flag.clone();
    let h = std::thread::spawn(move || b.leave_blocked_region_flagged(ThreadId(2), &f));

    // While the pause is active the flag must stay up (the thread is
    // waiting the pause out, not resuming).
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        flag.load(Ordering::Acquire),
        "flag must not clear while an excluding pause is active",
    );
    // The excluded leave must not have satisfied any quota either.
    assert_eq!(
        barrier.pending_count(),
        0,
        "expected was 0 (only initiator + excluded)"
    );

    barrier.complete_gc(cratonvm_types::PointerMap::default());
    h.join().unwrap();
    assert!(
        !flag.load(Ordering::Acquire),
        "flag clears once no pause is active",
    );
}

/// `leave_blocked_region_flagged` with no active pause clears the flag
/// immediately and never blocks.
#[test]
fn leave_blocked_region_flagged_immediate_when_idle() {
    use std::sync::atomic::AtomicBool;
    let barrier = GcBarrier::new();
    let flag = AtomicBool::new(true);
    barrier.leave_blocked_region_flagged(ThreadId(7), &flag);
    assert!(!flag.load(Ordering::Acquire));
}

/// gcd d5/f: `leave_blocked_region_flagged_if` clears the flag only when
/// its question answers yes, and otherwise leaves it up for the caller's
/// full wake.
#[test]
fn leave_blocked_region_flagged_if_keeps_the_flag_up_when_told_to() {
    use std::sync::atomic::AtomicBool;
    let barrier = GcBarrier::new();
    let flag = AtomicBool::new(true);
    assert!(!barrier.leave_blocked_region_flagged_if(ThreadId(7), &flag, || false));
    assert!(flag.load(Ordering::Acquire), "left up for the full wake");
    assert!(barrier.leave_blocked_region_flagged_if(ThreadId(7), &flag, || true));
    assert!(!flag.load(Ordering::Acquire));
}

/// gcd d10/j: the in-native pair counts the thread blocked and releases
/// it exactly once whichever way the leave goes, clears the flag only
/// when told to, and with a pause in progress waits it out before it
/// leaves (the two-step path).
#[test]
fn the_in_native_pair_counts_once_and_waits_out_a_pause_on_the_way_out() {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    let barrier = Arc::new(GcBarrier::new());
    let flag = Arc::new(AtomicBool::new(true));

    assert!(!barrier.mark_in_native_enter(), "no pause in progress");
    assert_eq!(barrier.blocked_count(), 1);
    assert!(!barrier.leave_in_native_if(ThreadId(7), &flag, || false));
    assert_eq!(
        barrier.blocked_count(),
        0,
        "released even when the flag stays up"
    );
    assert!(flag.load(Ordering::Acquire), "left up for the full wake");

    assert!(!barrier.mark_in_native_enter());
    assert!(barrier.leave_in_native_if(ThreadId(7), &flag, || true));
    assert_eq!(barrier.blocked_count(), 0);
    assert!(!flag.load(Ordering::Acquire), "cleared when told to");

    // A pause that excluded the thread (identity census) holds its leave.
    flag.store(true, Ordering::Release);
    assert!(!barrier.mark_in_native_enter());
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![7])));
    let left = Arc::new(AtomicBool::new(false));
    let leaver = {
        let barrier = Arc::clone(&barrier);
        let flag = Arc::clone(&flag);
        let left = Arc::clone(&left);
        std::thread::spawn(move || {
            let quiet = barrier.leave_in_native_if(ThreadId(7), &flag, || true);
            left.store(true, Ordering::SeqCst);
            quiet
        })
    };
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !left.load(Ordering::SeqCst),
        "an excluded thread does not leave native while the pause holds"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(leaver.join().expect("the leaver does not panic"));
    assert!(left.load(Ordering::SeqCst));
    assert_eq!(barrier.blocked_count(), 0, "released exactly once");
    assert!(!flag.load(Ordering::Acquire));
}

/// Poll `cond` for up to `limit`.
fn gch_jr_within(limit: std::time::Duration, cond: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    cond()
}

/// gch h2/jr (`CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT`): a counted thread
/// going into native during a pause arrives ONCE and runs on (it reaches
/// "in C" while the pause is still waiting for another mutator); a second
/// arrival of the same thread in that pause -- the no-wait entry again, or
/// a parked `auto` arrival (a JNIEnv function's escalation) -- counts
/// nothing; the quota is met only when the other counted mutator arrives;
/// and the thread's leave still waits for `complete_gc`.
#[test]
fn gch_jr_in_native_arrival_meets_the_quota_without_parking() {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    let barrier = Arc::new(GcBarrier::new());
    let flag = Arc::new(AtomicBool::new(true));
    // No pause: nothing to arrive for.
    assert!(!barrier.arrive_in_native(ThreadId(7)));

    // 3 alive: the initiator 0, the in-native thread 7, a mutator 1; the
    // census counted both (the flag of 7 went up after it).
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (3, 0, vec![])));
    assert_eq!(barrier.pending_count(), 2);

    let in_c = Arc::new(AtomicBool::new(false));
    let left = Arc::new(AtomicBool::new(false));
    let native = {
        let barrier = Arc::clone(&barrier);
        let flag = Arc::clone(&flag);
        let in_c = Arc::clone(&in_c);
        let left = Arc::clone(&left);
        std::thread::spawn(move || {
            assert!(barrier.mark_in_native_enter(), "the pause raced the entry");
            assert!(
                barrier.arrive_in_native(ThreadId(7)),
                "arrived, did not park"
            );
            assert!(
                barrier.arrive_in_native(ThreadId(7)),
                "again: still no park"
            );
            in_c.store(true, Ordering::SeqCst);
            let quiet = barrier.leave_in_native_if(ThreadId(7), &flag, || true);
            left.store(true, Ordering::SeqCst);
            quiet
        })
    };
    assert!(
        gch_jr_within(Duration::from_secs(5), || in_c.load(Ordering::SeqCst)),
        "the in-native arrival must return while the pause is still in progress"
    );
    assert_eq!(barrier.pending_count(), 1, "counted once, not twice");
    // A parked arrival of the same thread in the same pause counts nothing.
    let escalated = {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let _ = barrier.arrive_and_wait_auto(ThreadId(7));
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(barrier.pending_count(), 1, "the escalation is excluded");
    assert!(
        !quota_met_within(&barrier, Duration::from_millis(20)),
        "only the other mutator meets the quota"
    );

    let mutator = {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let _ = barrier.arrive_and_wait_auto(ThreadId(1));
        })
    };
    barrier.wait_for_all();
    assert_eq!(barrier.pending_count(), 0);
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !left.load(Ordering::SeqCst),
        "the thread does not leave native while the pause holds"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(native.join().expect("the native thread does not panic"));
    escalated.join().expect("the escalation does not panic");
    mutator.join().expect("the mutator does not panic");
    assert!(left.load(Ordering::SeqCst));
    assert_eq!(
        barrier.blocked_count(),
        0,
        "the in-native count released once"
    );
    assert!(!flag.load(Ordering::Acquire));
}

/// gch h2/jr: an in-native arrival of a thread the census EXCLUDED (its
/// flag was up first) counts nothing, and the exclusion it records lasts
/// only for its pause: the next pause's census counts it again and its
/// arrival there counts once.
#[test]
fn gch_jr_in_native_arrival_is_excluded_for_the_rest_of_its_pause_only() {
    let barrier = Arc::new(GcBarrier::new());
    // 3 alive: initiator 0, mutator 1, thread 7 excluded by the census.
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (3, 1, vec![7])));
    assert_eq!(barrier.pending_count(), 1);
    let b = Arc::clone(&barrier);
    assert!(std::thread::spawn(move || b.arrive_in_native(ThreadId(7)))
        .join()
        .expect("no panic"));
    assert_eq!(
        barrier.pending_count(),
        1,
        "an excluded arrival counts nothing"
    );
    let b = Arc::clone(&barrier);
    let mutator = std::thread::spawn(move || {
        let _ = b.arrive_and_wait_auto(ThreadId(1));
    });
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    mutator.join().expect("no panic");

    // Pause 2 counts 7 (alive 2: initiator and 7).
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 0, vec![])));
    assert_eq!(barrier.pending_count(), 1);
    let b = Arc::clone(&barrier);
    assert!(std::thread::spawn(move || b.arrive_in_native(ThreadId(7)))
        .join()
        .expect("no panic"));
    assert_eq!(
        barrier.pending_count(),
        0,
        "counted once in the pause that counted it"
    );
    {
        let inner = barrier.inner.lock();
        assert!(
            inner.excluded_blocked.contains(&7),
            "and excluded for the rest of it"
        );
    }
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(
        !barrier.inner.lock().excluded_blocked.contains(&7),
        "cleared with its pause"
    );
}

/// gcd d5/f: a JNI leaf window opens only while no pause is up, and a
/// winning pause request does not return while one is open (it drains
/// them after raising `stw_requested`).
#[test]
fn a_leaf_window_opens_outside_pauses_and_a_request_waits_for_it() {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    let barrier = Arc::new(GcBarrier::new());
    let word = Arc::new(LeafWindowWord::new());
    assert!(barrier.try_open_leaf_window(&word));
    assert!(word.is_open());

    // A request from another thread waits for the open window.
    let requested = Arc::new(AtomicBool::new(false));
    let requester = {
        let barrier = Arc::clone(&barrier);
        let requested = Arc::clone(&requested);
        std::thread::spawn(move || {
            let won = barrier.request_stw(ThreadId(1), 1);
            requested.store(true, Ordering::SeqCst);
            won
        })
    };
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !requested.load(Ordering::SeqCst),
        "the request must not return while a leaf window is open"
    );
    word.close();
    assert!(requester.join().expect("the requester does not panic"));
    assert!(requested.load(Ordering::SeqCst));

    // With the pause up, no window opens.
    assert!(!barrier.try_open_leaf_window(&word));
    assert!(!word.is_open(), "a refused window is left closed");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(
        barrier.try_open_leaf_window(&word),
        "open again after the pause"
    );
    word.close();

    // A dead owner's word is forgotten, and never waited for.
    word.mark_owner_dropped();
    let other = Arc::new(LeafWindowWord::new());
    assert!(barrier.try_open_leaf_window(&other));
    other.close();
    assert_eq!(
        barrier.leaf_window_words.lock().len(),
        1,
        "the dead word was pruned"
    );
    assert!(barrier.request_stw(ThreadId(1), 1));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// gcj j3/rt3 residual 3, fail-first: a window that stays open past the
/// locked drain's budget is waited for with `inner` RELEASED -- another
/// thread takes `inner` (an arrival's, a leave's, a losing request's
/// lock) while the request still has not returned, and sees the pause
/// fully published. Before, the lock was held for the whole wait.
#[test]
fn gcj3rt3_a_slow_leaf_window_is_drained_with_inner_released() {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    let barrier = Arc::new(GcBarrier::new());
    let word = Arc::new(LeafWindowWord::new());
    assert!(barrier.try_open_leaf_window(&word));
    let returned = Arc::new(AtomicBool::new(false));
    let requester = {
        let barrier = Arc::clone(&barrier);
        let returned = Arc::clone(&returned);
        std::thread::spawn(move || {
            let won =
                barrier.request_stw_counted_with_live_blocked(ThreadId(1), || (2, 1, vec![5]));
            returned.store(true, Ordering::SeqCst);
            won
        })
    };
    // Until the flag is up, the request has not reached its drain.
    while !barrier.stw_requested.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(100));
    assert!(!returned.load(Ordering::SeqCst), "still draining");
    {
        let inner = barrier
            .inner
            .try_lock_for(Duration::from_secs(5))
            .expect("inner is not held across the slow drain");
        assert!(inner.pause_in_flight, "the pause is fully published");
        assert!(inner.excluded_blocked.contains(&5));
        assert_eq!(inner.expected, 0);
    }
    assert!(
        !returned.load(Ordering::SeqCst),
        "the request waits for the window"
    );
    word.close();
    assert!(requester.join().expect("no panic"));
    assert!(!barrier.leaf_drain_owed.load(Ordering::Relaxed), "consumed");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// `with_no_pause_in_progress` (round 11 wave 8, JNI monitor ops from an
/// excluded thread): `f` does not run while a pause is active, and runs as
/// soon as that pause completes.
#[test]
fn with_no_pause_in_progress_waits_out_an_active_pause() {
    use std::sync::atomic::AtomicBool;

    let barrier = Arc::new(GcBarrier::new());
    let ran = Arc::new(AtomicBool::new(false));
    // Pause active; thread 2 is excluded by the identity census.
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![2])));

    let b = barrier.clone();
    let r = ran.clone();
    let h = std::thread::spawn(move || {
        b.with_no_pause_in_progress(|| {
            // No pause may be visible to `f`.
            let stw = b.stw_requested.load(Ordering::Acquire);
            r.store(true, Ordering::Release);
            stw
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        !ran.load(Ordering::Acquire),
        "f must not run while a pause is active"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let saw_stw = h.join().unwrap();
    assert!(ran.load(Ordering::Acquire));
    assert!(!saw_stw, "f ran with the pause still requested");
}

/// The other half: while `f` runs no pause can BEGIN — a request made from
/// inside the scope waits until `f` has returned.
#[test]
fn with_no_pause_in_progress_holds_off_a_new_request() {
    use std::sync::atomic::AtomicBool;

    let barrier = Arc::new(GcBarrier::new());
    let requested = Arc::new(AtomicBool::new(false));
    let requester = barrier.with_no_pause_in_progress(|| {
        let b = barrier.clone();
        let q = requested.clone();
        let h = std::thread::spawn(move || {
            let won = b.request_stw(ThreadId(9), 1);
            q.store(true, Ordering::Release);
            won
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !requested.load(Ordering::Acquire),
            "a pause request must not complete inside the scope"
        );
        assert!(!barrier.stw_requested.load(Ordering::Acquire));
        h
    });
    assert!(requester.join().unwrap(), "the request wins once f returns");
    assert!(barrier.stw_requested.load(Ordering::Acquire));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// Poll `wait_for_all_timeout` until it reports the quota met or `limit`
/// passes. One call is a single bounded wait that may wake spuriously.
fn quota_met_within(barrier: &GcBarrier, limit: std::time::Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if barrier.wait_for_all_timeout(std::time::Duration::from_millis(20)) {
            return true;
        }
    }
    false
}

/// gc-common 2026-09-23 (lane A) — a non-initiator OS thread arriving
/// under its own id is counted and receives the pause's map (the plain
/// contract, restated here beside the initiator tests below so a change
/// to `is_initiator_locked` has to keep it).
#[test]
fn a_distinct_os_thread_with_its_own_id_is_counted() {
    let barrier = Arc::new(GcBarrier::new());
    assert!(barrier.request_stw(ThreadId(0), 2));
    let b = barrier.clone();
    let h = std::thread::spawn(move || b.arrive_and_wait_auto(ThreadId(8)));
    assert!(
        quota_met_within(&barrier, std::time::Duration::from_secs(10)),
        "a counted non-initiator must satisfy its quota slot"
    );
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(0x1000, 0x2000);
    barrier.complete_gc(pm);
    let got = h.join().unwrap();
    assert_eq!(
        got.get(&0x1000),
        Some(&0x2000),
        "it must have waited the pause out and received that pause's map"
    );
}

/// The initiator's OS thread short-circuits whatever id it passes.
#[test]
fn the_initiator_short_circuits_whatever_id_it_passes() {
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw(ThreadId(3), 1));
    let map = barrier.arrive_and_wait_auto(ThreadId(99));
    assert!(map.is_empty());
    assert_eq!(barrier.pending_count(), 0);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// gc-common w3-a — "initiator" is the requesting OS THREAD, not an id:
/// another OS thread arriving under the initiator's `ThreadId` (the old JNI
/// placeholder shape) is an ordinary counted arrival and waits the pause
/// out. Before w3-a it was short-circuited as the initiator, took no slot
/// and returned at once.
#[test]
fn another_os_thread_passing_the_initiators_id_is_not_the_initiator() {
    let barrier = Arc::new(GcBarrier::new());
    assert!(barrier.request_stw(ThreadId(0), 2));
    let b = barrier.clone();
    let h = std::thread::spawn(move || b.arrive_and_wait_auto(ThreadId(0)));
    assert!(
        quota_met_within(&barrier, std::time::Duration::from_secs(10)),
        "the arrival must fill the one counted slot"
    );
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(0x40, 0x80);
    barrier.complete_gc(pm);
    let got = h.join().unwrap();
    assert_eq!(
        got.get(&0x40),
        Some(&0x80),
        "it must have parked for the pause and received its map"
    );
}

/// gc-common w3-a — the slowest pause names the thread it waited for last,
/// and a quota completed by a take-over excusal names nobody.
#[test]
fn ttsp_names_the_straggler_of_the_slowest_pause() {
    let barrier = Arc::new(GcBarrier::new());
    // Pause 1: one counted peer, tid 7, arrives late.
    assert!(barrier.request_stw(ThreadId(0), 2));
    let b = barrier.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        b.arrive_and_wait(ThreadId(7))
    });
    barrier.wait_for_all();
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let _ = h.join();
    let s = barrier.ttsp_stats();
    assert_eq!(s.last_straggler, Some(7));
    assert_eq!(s.max_straggler, Some(7));

    // Pause 2: the only counted peer is excused (frozen in compiled code).
    assert!(barrier.request_stw(ThreadId(0), 2));
    barrier.reduce_expected(1);
    assert!(barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let s = barrier.ttsp_stats();
    assert_eq!(s.last_straggler, None, "an excused peer has no arrival");
    assert_eq!(
        s.max_straggler,
        Some(7),
        "the slowest pause (30 ms) is still pause 1"
    );
}

/// gci i20/st: every PARTICIPATING arrival of a pause is booked in
/// `arrived_ids` (the stall line's arrived set), an excluded one never,
/// and the next request starts an empty set.
#[test]
fn gci_i20st_arrived_ids_name_every_counted_arrival_and_reset_per_pause() {
    let barrier = Arc::new(GcBarrier::new());
    // alive 3 = the initiator + two counted peers.
    assert!(barrier.request_stw(ThreadId(0), 3));
    let bx = barrier.clone();
    let excluded = std::thread::spawn(move || bx.arrive_and_wait_excluded(ThreadId(5)));
    let b1 = barrier.clone();
    let h1 = std::thread::spawn(move || b1.arrive_and_wait(ThreadId(9)));
    let b2 = barrier.clone();
    let h2 = std::thread::spawn(move || b2.arrive_and_wait(ThreadId(7)));
    assert!(
        quota_met_within(&barrier, std::time::Duration::from_secs(10)),
        "the two counted peers fill the quota"
    );
    assert_eq!(
        barrier.arrived_ids_snapshot(),
        vec![7, 9],
        "the arrived set names both counted peers, sorted, and never the excluded one"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let _ = h1.join();
    let _ = h2.join();
    let _ = excluded.join();
    assert!(barrier.request_stw(ThreadId(0), 2));
    assert!(
        barrier.arrived_ids_snapshot().is_empty(),
        "a new request starts with nobody arrived"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// gci i20/st: the stall line's arrived-set text is sorted and bounded.
#[test]
fn gci_i20st_stall_arrived_ids_text_sorts_and_bounds() {
    assert_eq!(stall_arrived_ids_text(&[]), "[]");
    assert_eq!(stall_arrived_ids_text(&[9, 3, 7]), "[3, 7, 9]");
    let many: Vec<u64> = (0..(STALL_ARRIVED_IDS_SHOWN as u64 + 3)).rev().collect();
    let text = stall_arrived_ids_text(&many);
    assert!(text.starts_with("[0, 1, 2, "), "{text}");
    assert!(text.ends_with(", +3]"), "{text}");
}

/// gc-common w3-a — the finalizer hint is raised by a queueing collection
/// and lowered exactly once by a drainer.
#[test]
fn the_finalizer_hint_is_taken_once() {
    let barrier = GcBarrier::new();
    assert!(!barrier.ref_work_queued());
    assert!(!barrier.take_ref_work_queued());
    barrier.note_ref_work_queued();
    assert!(barrier.ref_work_queued());
    assert!(barrier.take_ref_work_queued());
    assert!(!barrier.ref_work_queued());
    assert!(!barrier.take_ref_work_queued());
}

/// gc-common w5-a — the hint remembers the generation it was raised at, so
/// a hint nobody took across whole pauses reads stale; a re-raise of a
/// raised hint does not refresh it, and a take resets it.
#[test]
fn a_hint_nobody_takes_goes_stale_after_whole_pauses() {
    let barrier = GcBarrier::new();
    assert!(!barrier.ref_work_stale(0), "a lowered hint is never stale");
    barrier.note_ref_work_queued(); // raised at generation 0
    assert!(!barrier.ref_work_stale(1));
    barrier.gc_generation.fetch_add(1, Ordering::Release);
    barrier.note_ref_work_queued(); // already raised: keeps generation 0
    assert!(barrier.ref_work_stale(1));
    assert!(!barrier.ref_work_stale(2));
    barrier.gc_generation.fetch_add(1, Ordering::Release);
    assert!(barrier.ref_work_stale(2));
    assert!(barrier.take_ref_work_queued());
    assert!(!barrier.ref_work_stale(0), "a taken hint is not stale");
    barrier.note_ref_work_queued(); // raised afresh at generation 2
    assert!(!barrier.ref_work_stale(1));
}

/// gc-common w5-a (`handoff-w4b-gc-generation-on-its-own-cache-line`):
/// the generation counter the JIT memo caches probe on every lookup shares
/// no cache line with the fields every blocking transition writes.
#[test]
fn the_gc_generation_has_a_cache_line_of_its_own() {
    let barrier = GcBarrier::new();
    let line = |p: usize| p / 64;
    let gen = &*barrier.gc_generation as *const AtomicU64 as usize;
    let blocked = &barrier.threads_blocked as *const AtomicU64 as usize;
    let inner = &barrier.inner as *const Mutex<GcBarrierInner> as usize;
    assert_eq!(gen % 64, 0, "PaddedU64 starts a line");
    assert_ne!(line(gen), line(blocked));
    assert_ne!(line(gen), line(inner));
    assert!(std::mem::size_of::<PaddedU64>() >= 64);
    // Deref keeps the atomic API.
    barrier.gc_generation.fetch_add(2, Ordering::Release);
    assert_eq!(barrier.gc_generation.load(Ordering::Acquire), 2);
}

/// gc-common w3-a — a barrier dropped mid-pause releases its in-flight
/// count, and one that completed releases it exactly once.
#[test]
fn a_barrier_releases_its_in_flight_pause_exactly_once() {
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw(ThreadId(0), 1));
    assert!(barrier.inner.lock().pause_in_flight);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(!barrier.inner.lock().pause_in_flight);
    // A second `complete_gc` without a request releases nothing.
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(!barrier.inner.lock().pause_in_flight);
    let dropped_mid_pause = GcBarrier::new();
    assert!(dropped_mid_pause.request_stw(ThreadId(0), 1));
    let ledger = Arc::clone(dropped_mid_pause.pause_ledger());
    assert!(cratonvm_gc::gc_quiescence::pause_ledger_bound(&ledger));
    drop(dropped_mid_pause);
    // gc-common w21-e: the requester is no longer bound to the dropped
    // VM's ledger, so its coverage writes reach the orphan rows again.
    assert!(!cratonvm_gc::gc_quiescence::pause_ledger_bound(&ledger));
}

/// gc-common w3-a — on Windows a 1 ms bounded wait with no arrival must
/// take about 1 ms, not the 15.625 ms clock tick. Best of five, so a
/// descheduled attempt on a loaded host is not read as the tick.
#[cfg(target_os = "windows")]
#[test]
fn a_short_bounded_wait_is_not_tick_quantized_on_windows() {
    if !crate::threading::jvm_thread::win_park::enabled() {
        return;
    }
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw(ThreadId(0), 2));
    let mut best = std::time::Duration::MAX;
    for _ in 0..5 {
        let t0 = Instant::now();
        assert!(!barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)));
        best = best.min(t0.elapsed());
    }
    barrier.reduce_expected(1);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(
        best < std::time::Duration::from_millis(8),
        "best 1 ms wait took {best:?}: the condvar's tick-quantized path"
    );
}

/// gc-common 2026-09-23 (lane A) — the initiator closing a blocked region
/// inside its own pause must not wait for a generation only it can
/// advance. Both leave paths (`BlockedGuard::drop`,
/// `mark_blocked_region_leave`) used to self-deadlock here.
#[test]
fn the_initiator_leaving_a_blocked_region_in_its_own_pause_does_not_hang() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(5), 1));
        let guard = barrier.enter_blocked();
        assert!(guard.pre_stw);
        drop(guard);
        let _ = barrier.mark_blocked_region_enter();
        barrier.mark_blocked_region_leave();
        assert_eq!(barrier.blocked_count(), 0);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
        "the initiator hung waiting out its own pause"
    );
}

/// gc-common 2026-09-23 (lane A) — every waiter of one pause receives the
/// SAME map allocation (a reference count), not a per-thread deep copy
/// taken under the barrier lock.
#[test]
fn waiters_share_one_pointer_map_allocation() {
    let barrier = Arc::new(GcBarrier::new());
    assert!(barrier.request_stw(ThreadId(0), 3));
    let b1 = barrier.clone();
    let b2 = barrier.clone();
    let h1 = std::thread::spawn(move || b1.arrive_and_wait(ThreadId(1)));
    let h2 = std::thread::spawn(move || b2.arrive_and_wait(ThreadId(2)));
    barrier.wait_for_all();
    let mut pm = cratonvm_types::PointerMap::default();
    for i in 0..1024usize {
        pm.insert(0x10_0000 + i * 16, 0x20_0000 + i * 16);
    }
    barrier.complete_gc(pm);
    let m1 = h1.join().unwrap();
    let m2 = h2.join().unwrap();
    assert!(Arc::ptr_eq(&m1, &m2), "both waiters must share one map");
    assert_eq!(m1.len(), 1024);
    assert_eq!(m2.get(&0x10_0010), Some(&0x20_0010));
}

/// gc-common 2026-09-23 (lane A) — an initiator the identity census
/// reports as blocked must not be subtracted from `expected` twice.
///
/// 4 alive: initiator I(0) (flag still raised), running mutators M1(1)
/// and M2(2), blocked B(3). The census says blocked = {0, 3}. The old
/// `alive - 1 - blocked` gave 1 and released after ONE of the two running
/// mutators arrived.
#[test]
fn a_blocked_flagged_initiator_is_not_subtracted_twice() {
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (4, 2, vec![0, 3])));
    assert_eq!(
        barrier.pending_count(),
        2,
        "both running mutators must be waited for"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());

    // The ordinary case is unchanged: initiator not in the blocked set.
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (4, 1, vec![3])));
    assert_eq!(barrier.pending_count(), 2);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// Time-to-safepoint is recorded exactly once per pause, however many
/// times the initiator polls the quota.
#[test]
fn ttsp_is_recorded_once_per_pause() {
    let barrier = GcBarrier::new();
    assert_eq!(barrier.ttsp_stats().pauses, 0);
    assert!(barrier.request_stw(ThreadId(0), 1));
    barrier.wait_for_all();
    assert!(barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)));
    assert!(barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let s = barrier.ttsp_stats();
    assert_eq!(s.pauses, 1);
    assert!(s.max_ns >= s.last_ns && s.total_ns >= s.last_ns);

    assert!(barrier.request_stw(ThreadId(0), 1));
    assert!(barrier.wait_for_all_timeout(std::time::Duration::from_millis(1)));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(barrier.ttsp_stats().pauses, 2);
}

/// gci i46/orch: the coverage slot ([`COVERAGE_SLOT_OWNER`]) and its
/// gave-up rows are process-wide, so the tests below that take it (through
/// `request_stw_opening_cycle` or `take_coverage_slot`) run one at a time:
/// under the parallel runner one test's hold made another's timed wait or
/// owner assert read a foreign pause (1 run in 4).
fn coverage_slot_tests_serialised() -> std::sync::MutexGuard<'static, ()> {
    static SLOT_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SLOT_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

fn census(alive: u32, blocked_tids: Vec<u64>, initiator_counted: Option<bool>) -> StwCensus {
    StwCensus {
        alive,
        blocked: u32::try_from(blocked_tids.len()).unwrap_or(u32::MAX),
        blocked_tids,
        initiator_counted,
        excused_running: Vec::new(),
    }
}

/// Interpreter round i2 wave 58, lane L6: a thread the census excused in
/// a running native is out of `expected`, and its arrival -- through any
/// entry -- does not count, so it cannot meet the quota for a counted
/// mutator still running. Both request orders.
#[test]
fn an_excused_running_native_is_out_of_the_quota_and_never_counts() {
    let _slot = coverage_slot_tests_serialised();
    for lockfree in [false, true] {
        let mut barrier = GcBarrier::new();
        barrier.lockfree_blocked = lockfree;
        let barrier = Arc::new(barrier);
        // Three counted mutators (7, 8, 9); 9 is excused in a native.
        let mut c = census(3, Vec::new(), Some(false));
        c.excused_running = vec![9];
        assert!(barrier.request_stw_opening_cycle(ThreadId(1), || c, || {}));
        assert_eq!(barrier.pending_count(), 2, "lockfree={lockfree}");
        // The excused thread reaches a poll from inside its native: it
        // waits the pause out without filling a slot.
        let b = Arc::clone(&barrier);
        let excused = std::thread::spawn(move || {
            let _ = b.arrive_and_wait(ThreadId(9));
        });
        let t0 = std::time::Instant::now();
        while t0.elapsed() < std::time::Duration::from_millis(50) {
            std::thread::yield_now();
        }
        assert_eq!(barrier.pending_count(), 2, "the excused arrival counted");
        let b7 = Arc::clone(&barrier);
        let b8 = Arc::clone(&barrier);
        let a = std::thread::spawn(move || {
            let _ = b7.arrive_and_wait(ThreadId(7));
        });
        let b = std::thread::spawn(move || {
            let _ = b8.arrive_and_wait(ThreadId(8));
        });
        barrier.wait_for_all();
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        for h in [excused, a, b] {
            h.join().unwrap();
        }
        // The next pause excused nobody: 9 counts again.
        assert!(barrier.request_stw_opening_cycle(
            ThreadId(1),
            || census(3, Vec::new(), Some(false)),
            || {}
        ));
        assert_eq!(barrier.pending_count(), 3);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }
}

/// gcj j2/cm2 (j1-cm registry residuals item 5): a thread the census
/// excluded as blocked is not subtracted from `expected` a second time when
/// it is also named excused -- the state a bare lowering of the blocked
/// flag between the census and the excusal walk would produce. Both orders.
#[test]
fn gcj2cm2_an_excused_thread_already_blocked_is_not_subtracted_twice() {
    let _slot = coverage_slot_tests_serialised();
    for lockfree in [false, true] {
        let mut barrier = GcBarrier::new();
        barrier.lockfree_blocked = lockfree;
        // Three alive: 7 counted, 8 counted, 9 blocked AND named excused.
        let mut c = census(3, vec![9], Some(false));
        c.excused_running = vec![9];
        assert!(barrier.request_stw_opening_cycle(ThreadId(1), || c, || {}));
        assert_eq!(barrier.pending_count(), 2, "lockfree={lockfree}");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }
}

/// gc-common w2-a — the coverage cycle is opened by the WINNING request
/// only, under the barrier lock and before `stw_requested` is visible; a
/// losing request neither opens it nor runs its census.
///
/// This is the ordering that closes both coverage-reset races
/// (`common-a-losing-initiator-resets-coverage-state`,
/// `common-e-coverage-cycle-reset-races-a-sibling-initiator`): a reset that
/// only a winner performs, before any peer can observe its pause, can erase
/// neither another pause's deposits nor a sibling's pre-request marks.
#[test]
fn only_the_winning_request_opens_the_coverage_cycle() {
    let _slot = coverage_slot_tests_serialised();
    let barrier = GcBarrier::new();
    let opened = std::cell::Cell::new(0u32);
    let censused = std::cell::Cell::new(0u32);
    let won = barrier.request_stw_opening_cycle(
        ThreadId(1),
        || {
            censused.set(censused.get() + 1);
            census(1, vec![], Some(true))
        },
        || {
            assert!(
                !barrier.stw_requested.load(Ordering::Acquire),
                "the cycle must open BEFORE the pause is visible to any peer"
            );
            opened.set(opened.get() + 1);
        },
    );
    assert!(won);
    assert_eq!((opened.get(), censused.get()), (1, 1));

    // A second would-be initiator while the pause is in flight: loses,
    // resets nothing, and pays no census under the lock.
    let lost = barrier.request_stw_opening_cycle(
        ThreadId(2),
        || {
            censused.set(censused.get() + 1);
            census(2, vec![], Some(true))
        },
        || opened.set(opened.get() + 1),
    );
    assert!(!lost);
    assert_eq!(
        (opened.get(), censused.get()),
        (1, 1),
        "a losing request must not reset the winner's per-pause state"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());

    // The next pause opens its own cycle.
    assert!(barrier.request_stw_opening_cycle(
        ThreadId(2),
        || census(1, vec![], Some(true)),
        || opened.set(opened.get() + 1),
    ));
    assert_eq!(opened.get(), 2);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// gc-common w6-a — a second VM's pause does not open (and so cannot reset
/// the process-global per-pause coverage rows) while the first VM's pause
/// owns them; it opens as soon as the first completes. A sibling request of
/// the OWNING barrier does not wait at all, and a dropped barrier releases
/// the slot. `common-a-process-global-gc-coordination-state-FIXED-20260926.md`.
///
/// Other tests in this binary run real VMs whose pauses take the same
/// process slot, so the ordering half is asserted only when barrier A
/// actually got it (it can time out behind a sibling test's long pause).
#[test]
fn a_second_vms_pause_waits_for_the_first_vms_coverage_cycle() {
    let _slot = coverage_slot_tests_serialised();
    let a = Arc::new(GcBarrier::with_coverage_slot_no_wait(false));
    let b = Arc::new(GcBarrier::with_coverage_slot_no_wait(false));
    assert_ne!(a.slot_id, b.slot_id);
    assert_ne!(a.slot_id, 0);
    assert!(a.request_stw_opening_cycle(ThreadId(1), || census(1, vec![], Some(true)), || {}));
    let a_owns = a.inner.lock().holds_coverage_slot;
    if a_owns {
        assert_eq!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), a.slot_id);
        // A losing sibling initiator of A's own pause: no wait.
        let t0 = Instant::now();
        assert!(!a.request_stw_opening_cycle(
            ThreadId(2),
            || census(2, vec![], Some(true)),
            || panic!("a losing request must not open a cycle")
        ));
        assert!(
            t0.elapsed() < COVERAGE_SLOT_WAIT_LIMIT,
            "the owner's own sibling request must not wait for itself"
        );
        assert!(
            a.inner.lock().holds_coverage_slot,
            "and must not release it"
        );
    }

    let opened = Arc::new(AtomicBool::new(false));
    let b_thread = {
        let b = Arc::clone(&b);
        let opened = Arc::clone(&opened);
        std::thread::spawn(move || {
            let won = b.request_stw_opening_cycle(
                ThreadId(3),
                || census(1, vec![], Some(true)),
                || opened.store(true, Ordering::SeqCst),
            );
            b.complete_gc(cratonvm_types::PointerMap::default());
            won
        })
    };
    std::thread::sleep(Duration::from_millis(20));
    if a_owns {
        assert!(
            !opened.load(Ordering::SeqCst),
            "VM B must not open (reset) its coverage cycle while VM A's pause owns it"
        );
    }
    a.complete_gc(cratonvm_types::PointerMap::default());
    assert!(!a.inner.lock().holds_coverage_slot);
    assert!(
        b_thread.join().expect("B's request thread"),
        "B's pause runs after A's"
    );
    assert!(opened.load(Ordering::SeqCst));
    assert_ne!(
        COVERAGE_SLOT_OWNER.load(Ordering::Acquire),
        b.slot_id,
        "B released it"
    );

    // A barrier dropped mid-pause frees the slot for everybody else.
    let c = GcBarrier::new();
    let c_id = c.slot_id;
    assert!(c.request_stw_opening_cycle(ThreadId(4), || census(1, vec![], Some(true)), || {}));
    drop(c);
    assert_ne!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), c_id);
}

/// gc-common w7-a — a request that took the slot and then LOST to a
/// sibling of its own barrier (which won without it) hands the slot to that
/// pause, whose `complete_gc` releases it; with no pause of its own in
/// flight it releases it. The race itself (T2 wins between T1's
/// compare-exchange and T1's `inner.lock()`) is not schedulable from a
/// test, so this drives `settle_unused_slot` from the state it leaves.
/// Guarded like the test above: another test's VM may own the slot.
#[test]
fn a_losing_sibling_hands_the_slot_to_its_barriers_pause() {
    let _slot = coverage_slot_tests_serialised();
    let a = GcBarrier::new();
    // A pause of `a` that runs WITHOUT the slot (T2's win).
    assert!(a.request_stw_opening_cycle(ThreadId(1), || census(1, vec![], Some(true)), || {}));
    let had = std::mem::take(&mut a.inner.lock().holds_coverage_slot);
    if had {
        a.release_coverage_slot();
    }
    // T1 took the slot before queuing on `inner`, then lost.
    if a.try_take_coverage_slot() {
        {
            let mut inner = a.inner.lock();
            a.settle_unused_slot(&mut inner, true);
            assert!(
                inner.holds_coverage_slot,
                "the pause in flight must hold the slot the loser took"
            );
        }
        assert_eq!(
            COVERAGE_SLOT_OWNER.load(Ordering::Acquire),
            a.slot_id,
            "the loser must not release the slot under its sibling's pause"
        );
        a.complete_gc(cratonvm_types::PointerMap::default());
        assert_ne!(
            COVERAGE_SLOT_OWNER.load(Ordering::Acquire),
            a.slot_id,
            "the pause's complete_gc releases it"
        );
    } else {
        a.complete_gc(cratonvm_types::PointerMap::default());
    }
    // No pause in flight: a lost request's slot is simply released.
    if a.try_take_coverage_slot() {
        {
            let mut inner = a.inner.lock();
            a.settle_unused_slot(&mut inner, true);
            assert!(!inner.holds_coverage_slot);
        }
        assert_ne!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), a.slot_id);
    }
}

/// gc-common w7-a — a hold that one request waited the full limit for is
/// not waited for again until it is released
/// (`COVERAGE_SLOT_GAVE_UP_RELEASES`), and its release re-arms the wait.
/// Guarded on `x` actually owning the slot.
#[test]
fn a_hold_given_up_on_once_is_not_waited_for_again() {
    let _slot = coverage_slot_tests_serialised();
    let x = GcBarrier::new();
    let y = GcBarrier::new();
    assert!(x.request_stw_opening_cycle(ThreadId(1), || census(1, vec![], Some(true)), || {}));
    if x.inner.lock().holds_coverage_slot {
        let limit = Duration::from_millis(20);
        let t0 = Instant::now();
        assert!(!y.take_coverage_slot(limit), "x's pause holds the slot");
        assert!(
            t0.elapsed() >= limit,
            "the first request waits the limit out"
        );
        assert!(coverage_slot_hold_abandoned(x.slot_id));
        let t1 = Instant::now();
        assert!(!y.take_coverage_slot(Duration::from_secs(10)));
        assert!(
            t1.elapsed() < Duration::from_secs(5),
            "the same hold must not be waited for twice"
        );
    }
    x.complete_gc(cratonvm_types::PointerMap::default());
    assert!(
        !coverage_slot_hold_abandoned(x.slot_id),
        "a release ends the hold that was given up on"
    );
}

/// gc-common w2-a — an initiator the census states it did NOT count owns
/// no slot in `alive`; `None` keeps the historical `- 1`.
#[test]
fn an_initiator_outside_the_census_takes_no_slot() {
    let _slot = coverage_slot_tests_serialised();
    let barrier = GcBarrier::new();
    // Two counted mutators, neither of them the initiator.
    assert!(barrier.request_stw_opening_cycle(
        ThreadId(9),
        || census(2, vec![], Some(false)),
        || {}
    ));
    assert_eq!(
        barrier.pending_count(),
        2,
        "both real mutators must be waited for"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());

    assert!(barrier.request_stw_opening_cycle(
        ThreadId(1),
        || census(2, vec![], Some(true)),
        || {}
    ));
    assert_eq!(barrier.pending_count(), 1);
    barrier.complete_gc(cratonvm_types::PointerMap::default());

    assert!(barrier.request_stw_opening_cycle(ThreadId(1), || census(2, vec![], None), || {}));
    assert_eq!(
        barrier.pending_count(),
        1,
        "unknown keeps the legacy arithmetic"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());

    // Blocked-and-counted initiator: the blocked subtraction already
    // removes it, whatever `initiator_counted` says.
    assert!(barrier.request_stw_opening_cycle(
        ThreadId(1),
        || census(3, vec![1], Some(true)),
        || {}
    ));
    assert_eq!(barrier.pending_count(), 2);
    barrier.complete_gc(cratonvm_types::PointerMap::default());

    assert_eq!(initiator_slot(false, Some(true)), 1);
    assert_eq!(initiator_slot(false, None), 1);
    assert_eq!(initiator_slot(false, Some(false)), 0);
    assert_eq!(initiator_slot(true, Some(true)), 0);
}

/// gc-common w2-a — a waiter that slept through a LATER pause is handed the
/// composition of its own pause's map and every later one, not the newest
/// map alone. Driven deterministically through the barrier's own
/// bookkeeping: a parked waiter for generation 0 is registered, two pauses
/// complete, and the maps it would be given are inspected.
#[test]
fn a_waiter_straddling_two_pauses_gets_the_composed_map() {
    let barrier = GcBarrier::new();
    barrier.inner.lock().note_parked(0);

    assert!(barrier.request_stw(ThreadId(0), 1));
    let mut m1 = cratonvm_types::PointerMap::default();
    m1.insert(0x100, 0x200); // moved in G, and again in G+1
    m1.insert(0x300, 0x400); // moved in G only
    barrier.complete_gc(m1);

    assert!(barrier.request_stw(ThreadId(0), 1));
    let mut m2 = cratonvm_types::PointerMap::default();
    m2.insert(0x200, 0x500);
    m2.insert(0x600, 0x700); // moved in G+1 only
    barrier.complete_gc(m2);

    let maps = match barrier.inner.lock().maps_for_waiter(0) {
        WaiterMaps::Straddled(maps) => maps,
        WaiterMaps::Own(_) => panic!("a waiter for gen 1 woke at gen 2: not its own map"),
        WaiterMaps::Missing => panic!("the straddled pause's map must still be held"),
    };
    assert_eq!(maps.len(), 2);
    let c = compose_pointer_maps(&maps);
    assert_eq!(c.get(&0x100), Some(&0x500), "chased through both pauses");
    assert_eq!(c.get(&0x300), Some(&0x400), "moved by its own pause only");
    assert_eq!(c.get(&0x600), Some(&0x700), "moved by the later pause only");
    assert_eq!(c.get(&0x200), Some(&0x500));

    barrier.inner.lock().note_unparked(0);
    assert!(
        barrier.inner.lock().map_history.is_empty(),
        "the history must be released once no waiter needs it"
    );
}

/// Without a straddling waiter nothing is retained: the common path keeps
/// exactly one map alive, as before.
#[test]
fn no_map_history_without_a_straddling_waiter() {
    let barrier = Arc::new(GcBarrier::new());
    for round in 0..3usize {
        assert!(barrier.request_stw(ThreadId(0), 2));
        let b = barrier.clone();
        let h = std::thread::spawn(move || b.arrive_and_wait(ThreadId(1)));
        barrier.wait_for_all();
        let mut pm = cratonvm_types::PointerMap::default();
        pm.insert(0x1000 + round * 16, 0x2000 + round * 16);
        barrier.complete_gc(pm);
        let got = h.join().unwrap();
        assert_eq!(
            got.get(&(0x1000 + round * 16)),
            Some(&(0x2000 + round * 16))
        );
        let inner = barrier.inner.lock();
        assert!(inner.map_history.is_empty());
        assert!(inner.parked_waiters.is_empty());
    }
}

/// The real-thread version of the straddle, asserted in the form that
/// cannot flake: whether or not the waiter wakes between the two pauses,
/// the map it receives must carry ITS OWN pause's relocation of `0x100`.
/// The pre-w2 barrier handed a late waiter the newest map alone, which has
/// no entry for `0x100` at all.
#[test]
fn a_late_waiter_never_receives_only_a_later_pauses_map() {
    let barrier = Arc::new(GcBarrier::new());
    // W (tid 2) is excluded as blocked from both pauses, like a
    // blocked-region arrival.
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![2])));
    let b = barrier.clone();
    let h = std::thread::spawn(move || b.arrive_and_wait_auto(ThreadId(2)));
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while barrier.inner.lock().parked_waiters.is_empty() {
        assert!(Instant::now() < deadline, "waiter never parked");
        std::thread::yield_now();
    }
    let mut m1 = cratonvm_types::PointerMap::default();
    m1.insert(0x100, 0x200);
    barrier.complete_gc(m1);
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![2])));
    let mut m2 = cratonvm_types::PointerMap::default();
    m2.insert(0x200, 0x500);
    barrier.complete_gc(m2);
    let got = h.join().unwrap();
    let v = got.get(&0x100).copied();
    assert!(
        v == Some(0x200) || v == Some(0x500),
        "a waiter for pause G must get G's relocation (alone, or composed with G+1), got {v:?}"
    );
    assert!(barrier.inner.lock().map_history.is_empty());
}

/// gc-common w8-d — a winning request binds its OWN VM's pause ledger on
/// the initiator's thread and `complete_gc` unbinds it; a peer's deposit
/// bound to another barrier never reaches this initiator's reading, and
/// another VM's request never resets this VM's ledger. Deterministic even
/// beside other tests' VMs: each ledger is one barrier's, and the binding
/// is per thread.
#[test]
fn a_pause_reads_and_resets_only_its_own_vms_ledger() {
    use cratonvm_gc::gc_quiescence as q;
    let a = Arc::new(GcBarrier::new());
    let b = Arc::new(GcBarrier::new());
    assert!(a.request_stw(ThreadId(0), 1));
    assert!(q::pause_ledger_bound(a.pause_ledger()));
    // gc-common w10-g: A's take-over froze a peer; the verdict is A's.
    let frozen = q::TakeoverVerdict {
        frozen: 1,
        pins_complete: false,
        ..q::TakeoverVerdict::NONE
    };
    q::publish_takeover_verdict(frozen);
    // A peer of VM B, parking for B's pause, deposits into B's ledger.
    let bb = Arc::clone(&b);
    std::thread::spawn(move || {
        q::with_pause_ledger(bb.pause_ledger(), || q::add_peer_proven_jit_depth(6));
    })
    .join()
    .expect("B's peer");
    assert_eq!(
        q::peer_proven_jit_depth(),
        0,
        "A's initiator must not be credited with B's peer's proof"
    );
    // A peer of VM A deposits into A's.
    let aa = Arc::clone(&a);
    std::thread::spawn(move || {
        q::with_pause_ledger(aa.pause_ledger(), || q::add_peer_proven_jit_depth(2));
    })
    .join()
    .expect("A's peer");
    assert_eq!(q::peer_proven_jit_depth(), 2);
    // VM B opens a pause on its own thread: B's ledger resets, A's stands.
    let bb = Arc::clone(&b);
    std::thread::spawn(move || {
        assert!(bb.request_stw(ThreadId(0), 1));
        assert_eq!(bb.pause_ledger().proven_jit_depth(), 0);
        assert_eq!(
            q::takeover_verdict(),
            q::TakeoverVerdict::NONE,
            "B's pause opens clear"
        );
        bb.complete_gc(cratonvm_types::PointerMap::default());
        assert!(!q::pause_ledger_bound(bb.pause_ledger()));
    })
    .join()
    .expect("B's initiator");
    assert_eq!(
        q::peer_proven_jit_depth(),
        2,
        "B's pause must not reset A's ledger"
    );
    assert_eq!(
        q::takeover_verdict(),
        frozen,
        "B's pause must not reset A's take-over verdict to `Move`"
    );
    a.complete_gc(cratonvm_types::PointerMap::default());
    assert!(!q::pause_ledger_bound(a.pause_ledger()));
    // A's next pause opens A's ledger afresh.
    assert!(a.request_stw(ThreadId(0), 1));
    assert_eq!(q::peer_proven_jit_depth(), 0);
    assert_eq!(q::takeover_verdict(), q::TakeoverVerdict::NONE);
    a.complete_gc(cratonvm_types::PointerMap::default());
}

/// gc-common w18-c — the blocked/frozen-peer native-stack captures (the
/// `PEER_STACK_SLOTS` process static until then) are the pausing VM's.
/// Two VMs pause CONCURRENTLY on two threads: A's initiator records, then
/// B's request opens B's pause and B's opener discards B's captures, then
/// B records. Each initiator then reads and drains exactly its own words:
/// B's opening no longer discards A's, and A's fold no longer adopts B's
/// (as `UNROUTED`, a false repair outage) or folds them against A's map.
/// Uses the test-only request entry, which takes no coverage slot, so the
/// two pauses really overlap.
#[test]
fn concurrent_pauses_capture_and_drain_only_their_own_peer_stack_words() {
    use cratonvm_gc::gc_quiescence as q;
    if !q::blocked_peer_stack_remap_enabled() {
        // `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP` is set for this
        // process: nothing is captured, so there is nothing to isolate.
        return;
    }
    let a = Arc::new(GcBarrier::new());
    let b = Arc::new(GcBarrier::new());
    let a_recorded = Arc::new(std::sync::Barrier::new(2));
    let b_recorded = Arc::new(std::sync::Barrier::new(2));
    type Seen = (Vec<usize>, Vec<(u32, usize, usize)>, bool);
    let a_side = {
        let (a, a_recorded, b_recorded) = (a.clone(), a_recorded.clone(), b_recorded.clone());
        std::thread::spawn(move || -> Seen {
            assert!(a.request_stw(ThreadId(0), 1));
            q::record_peer_stack_slot(0xA18, 0x7a18_0000, 0x7a18_1000);
            q::record_takeover_stack_slot(0xA18, 0x7a18_0008, 0x7a18_2000);
            a_recorded.wait();
            // B opens, discards and records while A's pause is in flight.
            b_recorded.wait();
            let mut values = Vec::new();
            q::peer_stack_slot_values_into(&mut values, |_| true);
            let saturated = q::peer_stack_slots_saturated();
            let drained = q::take_peer_stack_slots();
            a.complete_gc(cratonvm_types::PointerMap::default());
            (values, drained, saturated)
        })
    };
    let b_side = {
        let b = b.clone();
        std::thread::spawn(move || -> Seen {
            a_recorded.wait();
            assert!(b.request_stw(ThreadId(0), 1));
            // What a collection's opener does (`begin_moving_young_coverage_cycle`).
            q::clear_peer_stack_slots();
            q::record_peer_stack_slot(0xB18, 0x7b18_0000, 0x7b18_1000);
            b_recorded.wait();
            let mut values = Vec::new();
            q::peer_stack_slot_values_into(&mut values, |_| true);
            let saturated = q::peer_stack_slots_saturated();
            let drained = q::take_peer_stack_slots();
            b.complete_gc(cratonvm_types::PointerMap::default());
            (values, drained, saturated)
        })
    };
    let (a_values, a_drained, a_saturated) = a_side.join().expect("A's initiator");
    let (b_values, b_drained, b_saturated) = b_side.join().expect("B's initiator");
    assert_eq!(
        a_drained,
        vec![
            (0xA18, 0x7a18_0000, 0x7a18_1000),
            (0xA18, 0x7a18_0008, 0x7a18_2000)
        ],
        "A's fold drains A's captures, all of them, and none of B's"
    );
    assert_eq!(a_values, vec![0x7a18_1000, 0x7a18_2000]);
    assert_eq!(
        b_drained,
        vec![(0xB18, 0x7b18_0000, 0x7b18_1000)],
        "B's are B's only"
    );
    assert_eq!(b_values, vec![0x7b18_1000]);
    assert!(!a_saturated && !b_saturated);
    // Both pauses are over; their ledgers are empty, and an unbound thread
    // drains only its own (empty) fallback.
    assert!(std::thread::spawn(q::take_peer_stack_slots)
        .join()
        .expect("unbound drain")
        .is_empty());
    assert!(
        !q::pause_ledger_bound(a.pause_ledger()) && !q::pause_ledger_bound(b.pause_ledger())
    );
}

/// gc-common w21-e — the moving-young coverage rows (the verdict, its
/// reasons, the un-rewritable flag, the conservative-scan count and the
/// helper-window pins; process statics until then) are the pausing VM's.
/// Two VMs pause CONCURRENTLY on two threads: A's initiator opens its
/// coverage cycle and records a frozen peer, then B's request opens B's
/// cycle (which used to erase A's verdict and pins: licence `Move` behind a
/// frozen peer) and records its own. A's collector then reads exactly A's
/// rows. Afterwards a blocking deposit of VM B made OUTSIDE every pause (a
/// scoped binding on a third thread, as `deposit_root_snapshot_inner` does)
/// lands in B's rows, survives A's next coverage open, and is cleared by
/// B's. Uses the test-only request entry, which takes no coverage slot, so
/// the pauses really overlap. Negative checks read the ledgers directly:
/// the free functions also OR in the process's orphan rows, which other
/// tests of this binary may write.
#[test]
fn concurrent_pauses_keep_their_own_coverage_verdicts_and_pins() {
    use cratonvm_gc::gc_quiescence as q;
    const PIN_A: usize = 0x7a21_e000;
    const PIN_B: usize = 0x7b21_e000;
    let a = Arc::new(GcBarrier::new());
    let b = Arc::new(GcBarrier::new());
    let a_marked = Arc::new(std::sync::Barrier::new(2));
    let b_marked = Arc::new(std::sync::Barrier::new(2));
    let a_side = {
        let (a, a_marked, b_marked) = (a.clone(), a_marked.clone(), b_marked.clone());
        std::thread::spawn(move || -> (bool, usize, bool, usize, Vec<usize>) {
            assert!(a.request_stw(ThreadId(0), 1));
            // What a collection's `open_cycle` does, then A's take-over.
            q::begin_moving_young_coverage_cycle();
            q::mark_moving_young_coverage_incomplete_because(q::incomplete_reason::XT_TAKEOVER);
            q::note_conservative_jit_scan();
            q::add_xt_cycle_pinned_jit_roots(&[PIN_A]);
            a_marked.wait();
            // B opens and records while A's pause is in flight.
            b_marked.wait();
            let seen = (
                q::moving_young_coverage_incomplete(),
                q::moving_young_incomplete_reason(),
                q::unrewritable_peer_state(),
                q::conservative_jit_scans(),
                q::pinned_jit_roots_snapshot(),
            );
            a.complete_gc(cratonvm_types::PointerMap::default());
            seen
        })
    };
    let b_side = {
        let b = b.clone();
        std::thread::spawn(move || -> q::CoverageSnapshot {
            a_marked.wait();
            assert!(b.request_stw(ThreadId(0), 1));
            q::begin_moving_young_coverage_cycle();
            let opened = b.pause_ledger().coverage_snapshot();
            q::mark_moving_young_coverage_incomplete_because(
                q::incomplete_reason::UNPUBLISHED_FRAME_OOP,
            );
            q::add_xt_cycle_pinned_jit_roots(&[PIN_B]);
            b_marked.wait();
            b.complete_gc(cratonvm_types::PointerMap::default());
            opened
        })
    };
    let (incomplete, reason, unrewritable, scans, pins) = a_side.join().expect("A's initiator");
    let b_opened = b_side.join().expect("B's initiator");
    assert_eq!(
        b_opened,
        q::CoverageSnapshot::default(),
        "B's cycle opens without A's rows"
    );
    assert!(incomplete, "B's coverage open must not erase A's verdict");
    assert_eq!(
        reason,
        q::incomplete_reason::XT_TAKEOVER,
        "A's collector reads A's reason"
    );
    assert!(unrewritable, "A's frozen peer still forbids promotion");
    assert!(scans >= 1, "A's conservative scan is still counted");
    assert!(
        pins.contains(&PIN_A),
        "B's open must not drop A's helper-window pin"
    );
    assert!(!pins.contains(&PIN_B), "B's pin is not A's");
    assert_eq!(
        a.pause_ledger().coverage_snapshot(),
        q::CoverageSnapshot {
            incomplete: true,
            reason: q::incomplete_reason::XT_TAKEOVER,
            reason_mask: 1usize << q::incomplete_reason::XT_TAKEOVER,
            unrewritable: true,
            scans: 1,
            xt_pins: vec![PIN_A],
        }
    );
    assert_eq!(b.pause_ledger().coverage_snapshot().xt_pins, vec![PIN_B]);

    // A blocking deposit of VM B, outside every pause.
    let bb = b.clone();
    std::thread::spawn(move || {
        let _scope = q::scoped_pause_ledger(bb.pause_ledger());
        q::mark_moving_young_coverage_incomplete_because(
            q::incomplete_reason::UNREGISTERED_JIT_FRAME,
        );
        q::note_conservative_jit_scan();
    })
    .join()
    .expect("B's blocking thread");
    let b_rows = b.pause_ledger().coverage_snapshot();
    assert!(
        b_rows.incomplete && b_rows.scans == 1,
        "a writer outside every pause lands in its VM's rows"
    );
    // A's next collection opens A's rows only.
    assert!(a.request_stw(ThreadId(0), 1));
    q::begin_moving_young_coverage_cycle();
    assert_eq!(
        a.pause_ledger().coverage_snapshot(),
        q::CoverageSnapshot::default()
    );
    a.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(
        b.pause_ledger().coverage_snapshot(),
        b_rows,
        "A's coverage open must not clear B's rows"
    );
    // B's own next collection does.
    assert!(b.request_stw(ThreadId(0), 1));
    q::begin_moving_young_coverage_cycle();
    assert_eq!(
        b.pause_ledger().coverage_snapshot(),
        q::CoverageSnapshot::default()
    );
    b.complete_gc(cratonvm_types::PointerMap::default());
    assert!(
        !q::pause_ledger_bound(a.pause_ledger()) && !q::pause_ledger_bound(b.pause_ledger())
    );
}

/// gc-common w8-d — a production pause that went ahead WITHOUT the
/// coverage slot takes it at its next barrier wait once it is free,
/// instead of running slot-less to `complete_gc`; `complete_gc` releases
/// it. A test-only request (which never wants the slot) does not. Guarded
/// like the tests above: another test's VM may own the slot.
#[test]
fn a_pause_without_the_slot_adopts_it_once_free() {
    let _slot = coverage_slot_tests_serialised();
    let x = GcBarrier::new();
    assert!(x.request_stw_opening_cycle(ThreadId(1), || census(1, vec![], Some(true)), || {}));
    // Make it the pause that went ahead without the slot.
    let had = std::mem::take(&mut x.inner.lock().holds_coverage_slot);
    if had {
        x.release_coverage_slot();
    }
    assert!(x.wait_for_all_timeout(Duration::from_millis(1)));
    if x.inner.lock().holds_coverage_slot {
        assert_eq!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), x.slot_id);
        x.complete_gc(cratonvm_types::PointerMap::default());
        assert_ne!(
            COVERAGE_SLOT_OWNER.load(Ordering::Acquire),
            x.slot_id,
            "complete_gc releases the adopted slot"
        );
    } else {
        // Another test's VM holds it: nothing to adopt yet.
        assert_ne!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), x.slot_id);
        x.complete_gc(cratonvm_types::PointerMap::default());
    }
    assert!(!x.inner.lock().wants_coverage_slot);

    // The test-only entries never want it.
    let y = GcBarrier::new();
    assert!(y.request_stw(ThreadId(0), 1));
    assert!(y.wait_for_all_timeout(Duration::from_millis(1)));
    assert!(!y.inner.lock().holds_coverage_slot);
    assert_ne!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), y.slot_id);
    y.complete_gc(cratonvm_types::PointerMap::default());
}

/// gcg g2/k (`common-a-proposal-lock-free-blocked-transitions-DONE-20261015`): with
/// the lock-free transitions on, a request raises `stw_requested` BEFORE
/// its census (with them off, after), an entry with no pause up reads
/// `pre_stw == false` and is seen blocked by the next census, an entry
/// during the pause reads `true`, and the plain leave releases the
/// anonymous count without the lock once the pause is over.
#[test]
fn gcg_k2_lockfree_request_raises_the_flag_before_its_census() {
    let _slot = coverage_slot_tests_serialised();
    for lockfree in [false, true] {
        let b = GcBarrier::with_lockfree_blocked(lockfree);
        let flag = AtomicBool::new(true); // the entering thread's deposit
        assert!(!b.mark_blocked_region_enter(), "no pause is up yet");
        let flag_seen_by_census = std::cell::Cell::new(None);
        assert!(b.request_stw_opening_cycle(
            ThreadId(1),
            || {
                flag_seen_by_census.set(Some(b.stw_requested.load(Ordering::SeqCst)));
                let blocked = if flag.load(Ordering::SeqCst) {
                    vec![42]
                } else {
                    vec![]
                };
                census(2, blocked, Some(true))
            },
            || {},
        ));
        assert_eq!(
            flag_seen_by_census.get(),
            Some(lockfree),
            "the census runs after the flag store exactly when lock-free"
        );
        {
            let inner = b.inner.lock();
            assert_eq!(inner.expected, 0, "the blocked thread is excluded");
            assert!(inner.excluded_blocked.contains(&42));
        }
        // An entry while the pause is up (from the initiator's own thread,
        // so the locked leave below does not wait for this pause).
        assert!(b.mark_blocked_region_enter(), "a pause is up");
        assert_eq!(b.blocked_count(), 2);
        b.mark_blocked_region_leave();
        assert!(b.wait_for_all_timeout(Duration::from_millis(1)));
        b.complete_gc(cratonvm_types::PointerMap::default());
        b.mark_blocked_region_leave();
        assert_eq!(b.blocked_count(), 0);
        drop(b.enter_blocked());
        assert_eq!(b.blocked_count(), 0, "the guard releases what it took");
    }
}

/// One mutator entering and leaving a blocked region in a loop against a
/// requester running pauses back to back. The invariant the entry lock
/// (default) and the Dekker pair (lock-free) must both give: an entry
/// that read `pre_stw == false` is EXCLUDED by every pause in progress
/// while its `in_blocked_region` is still up. Returns the violations.
fn race_blocked_entry_against_census(lockfree: bool, pauses: u32) -> u64 {
    race_blocked_entry_against_census_counted(lockfree, true, pauses)
}

/// [`race_blocked_entry_against_census`] with the anonymous blocked
/// counter on (`counted`, the default) or off
/// (`CRATONVM_STW_NO_BLOCKED_COUNTER`, gcg g3/k). Also checks, after the
/// race, that the counter is back at 0 (on) or never moved (off).
fn race_blocked_entry_against_census_counted(
    lockfree: bool,
    counted: bool,
    pauses: u32,
) -> u64 {
    const MUTATOR: ThreadId = ThreadId(42);
    let mut barrier = GcBarrier::with_lockfree_blocked(lockfree);
    barrier.anon_blocked_counted = counted;
    let b = Arc::new(barrier);
    let max_seen = Arc::new(AtomicU64::new(0));
    let flag = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let violations = Arc::new(AtomicU64::new(0));
    let mutator = {
        let (b, flag, stop, violations, max_seen) = (
            Arc::clone(&b),
            Arc::clone(&flag),
            Arc::clone(&stop),
            Arc::clone(&violations),
            Arc::clone(&max_seen),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                // A running mutator's safepoint.
                let _ = b.arrive_and_wait_auto(MUTATOR);
                // The deposit raises the identity flag, then the entry.
                flag.store(true, Ordering::Release);
                let entered = b.mark_blocked_region_enter();
                max_seen.fetch_max(b.blocked_count(), Ordering::Relaxed);
                if entered {
                    let _ = b.arrive_and_wait_auto(MUTATOR);
                } else {
                    let inner = b.inner.lock();
                    if b.stw_requested.load(Ordering::Acquire)
                        && !inner.excluded_blocked.contains(&MUTATOR.0)
                    {
                        violations.fetch_add(1, Ordering::Relaxed);
                    }
                }
                b.mark_blocked_region_leave();
                b.leave_blocked_region_flagged(MUTATOR, &flag);
            }
        })
    };
    for _ in 0..pauses {
        let won = b.request_stw_opening_cycle(
            ThreadId(1),
            || {
                let blocked = if flag.load(Ordering::Acquire) {
                    vec![MUTATOR.0]
                } else {
                    vec![]
                };
                census(2, blocked, Some(true))
            },
            || {},
        );
        assert!(won, "the only requester wins");
        let t0 = Instant::now();
        while !b.wait_for_all_timeout(Duration::from_millis(1)) {
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "a counted mutator never arrived (lockfree={lockfree})"
            );
        }
        b.complete_gc(cratonvm_types::PointerMap::default());
    }
    stop.store(true, Ordering::Release);
    mutator.join().expect("mutator thread");
    assert_eq!(b.blocked_count(), 0, "every entry was left again");
    let max_seen = max_seen.load(Ordering::Relaxed);
    if counted {
        assert_eq!(
            max_seen, 1,
            "positive control: the counted entry is visible"
        );
    } else {
        assert_eq!(max_seen, 0, "the counter is off: no transition touches it");        }
    violations.load(Ordering::Relaxed)    }

/// gcg g2/k: the race above, with the entry lock (default) and with the
/// lock-free transitions. Zero violations and no hang in both.
#[test]
fn gcg_k2_blocked_entry_and_census_agree_under_a_race() {
    assert_eq!(race_blocked_entry_against_census(false, 1500), 0);
    assert_eq!(race_blocked_entry_against_census(true, 1500), 0);
}

/// gcg g3/k: a production request that waited for another VM's pause to
/// free the coverage slot records that wait (`slot_wait_us=` on the
/// `[stw-ttsp]` line), which its `ttsp_us` does not include.
#[test]
fn gcg_k3_a_won_request_records_its_coverage_slot_wait() {
    let _slot = coverage_slot_tests_serialised();
    // VM A's whole pause (request .. complete) on one thread of its own,
    // holding the slot for ~40 ms.
    //
    // gci i44/sp: the 20 ms floor is asserted only when b's request began
    // with at least 30 ms of a's hold still to run. The hold is a's 40 ms
    // sleep from the instant it reported owning the slot; on a loaded
    // test binary b's thread was seen to resume from `recv` late enough
    // that most of the hold had passed (1 run in 4 under
    // `-- gci_i44 gc_barrier`), and the wait it then recorded was
    // correctly short. The 10 ms slack covers b's own lag between `t0`
    // and the request's slot clock.
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Instant)>();
    let releaser = std::thread::spawn(move || {
        let a = GcBarrier::with_coverage_slot_no_wait(false);
        assert!(a.request_stw_opening_cycle(
            ThreadId(1),
            || census(1, vec![], Some(true)),
            || {}
        ));
        let owns = a.inner.lock().holds_coverage_slot;
        // Taken before the sleep: the slot is released no earlier than
        // 40 ms after this instant.
        tx.send((owns, Instant::now())).unwrap();
        std::thread::sleep(Duration::from_millis(40));
        a.complete_gc(cratonvm_types::PointerMap::default());
    });
    let (a_owns, hold_from) = rx.recv().unwrap();
    let b = GcBarrier::with_coverage_slot_no_wait(false);
    let t0 = Instant::now();
    let hold_left = Duration::from_millis(40).saturating_sub(t0.duration_since(hold_from));
    assert!(b.request_stw_opening_cycle(ThreadId(2), || census(1, vec![], Some(true)), || {}));
    let waited = t0.elapsed();
    let recorded = Duration::from_nanos(b.inner.lock().slot_wait_ns);
    assert!(
        recorded <= waited,
        "recorded {recorded:?} > the request's own {waited:?}"
    );
    if a_owns && hold_left >= Duration::from_millis(30) {
        assert!(
            recorded >= Duration::from_millis(20),
            "b waited for a's pause ({waited:?}) but recorded {recorded:?}"
        );
    }
    releaser.join().unwrap();
    b.complete_gc(cratonvm_types::PointerMap::default());
}

/// gcg g3/k (`CRATONVM_STW_NO_BLOCKED_COUNTER`): the same race with the
/// anonymous counter off, in both entry orders. The census decides
/// `expected` from identities alone, so nothing may change: zero
/// violations, no hang, and the counter never moves.
#[test]
fn gcg_k3_no_blocked_counter_keeps_the_census_exact_under_a_race() {
    assert_eq!(
        race_blocked_entry_against_census_counted(false, false, 1500),
        0
    );
    assert_eq!(
        race_blocked_entry_against_census_counted(true, false, 1500),
        0
    );
}

/// gcg g3/k: every blocked-region transition pair leaves the counter
/// where it found it, and with the counter off never moves it (the
/// guard form, the non-guard form, `leave_after`, and the native bracket).
#[test]
fn gcg_k3_every_blocked_transition_pair_is_balanced_with_the_counter_on_or_off() {
    for counted in [true, false] {
        let b = GcBarrier::with_anon_blocked_counted(counted);
        let inside = u64::from(counted);
        let g = b.enter_blocked();
        assert_eq!(b.blocked_count(), inside);
        drop(g);
        assert_eq!(b.blocked_count(), 0);
        assert!(!b.mark_blocked_region_enter());
        assert_eq!(b.blocked_count(), inside);
        b.mark_blocked_region_leave();
        assert_eq!(b.blocked_count(), 0);
        assert!(!b.mark_blocked_region_enter());
        b.mark_blocked_region_leave_after(|| {});
        assert_eq!(b.blocked_count(), 0);
        assert!(!b.mark_in_native_enter());
        assert_eq!(b.blocked_count(), inside);
        let flag = AtomicBool::new(true);
        assert!(b.leave_in_native_if(ThreadId(7), &flag, || true));
        assert!(!flag.load(Ordering::Acquire));
        assert_eq!(b.blocked_count(), 0, "counted={counted}");
    }
}

/// gcg g4/xt: the helper-window gate. With the anonymous counter it
/// follows the count; without it (`CRATONVM_STW_NO_BLOCKED_COUNTER`) it
/// can never answer "nobody is blocked", because `blocked_count` is 0
/// there whoever is blocked -- the gate that skipped the pass.
#[test]
fn gcg_g4xt_blocked_gate_never_reads_an_unmaintained_counter() {
    let b = GcBarrier::with_anon_blocked_counted(true);
    assert!(!b.may_have_blocked_threads());
    assert!(!b.mark_blocked_region_enter());
    assert!(b.may_have_blocked_threads());
    b.mark_blocked_region_leave();
    assert!(!b.may_have_blocked_threads());

    let b = GcBarrier::with_anon_blocked_counted(false);
    assert!(b.may_have_blocked_threads());
    assert!(!b.mark_blocked_region_enter());
    // Positive control: the old gate's input is blind here.
    assert_eq!(b.blocked_count(), 0);
    assert!(b.may_have_blocked_threads());
    b.mark_blocked_region_leave();
    assert!(b.may_have_blocked_threads());
}

/// gcg g2/k (`common-w21e-proposal-coverage-slot-without-the-wait`): a
/// request of a barrier with `CRATONVM_STW_COVERAGE_SLOT_NO_WAIT` does not
/// wait for another VM's pause holding the coverage slot; it goes ahead
/// without it and adopts it at its next barrier wait once free.
#[test]
fn gcg_k2_a_no_wait_request_does_not_wait_for_another_vms_pause() {
    let _slot = coverage_slot_tests_serialised();
    let a = GcBarrier::new();
    assert!(a.request_stw_opening_cycle(ThreadId(1), || census(1, vec![], Some(true)), || {}));
    let a_owns = a.inner.lock().holds_coverage_slot;
    let b = GcBarrier::with_coverage_slot_no_wait(true);
    let t0 = Instant::now();
    assert!(b.request_stw_opening_cycle(ThreadId(2), || census(1, vec![], Some(true)), || {}));
    assert!(
        t0.elapsed() < COVERAGE_SLOT_WAIT_LIMIT / 2,
        "a no-wait request must not wait out another VM's pause"
    );
    if a_owns {
        assert!(!b.inner.lock().holds_coverage_slot, "A still owns the slot");
    }
    a.complete_gc(cratonvm_types::PointerMap::default());
    assert!(b.wait_for_all_timeout(Duration::from_millis(1)));
    if b.inner.lock().holds_coverage_slot {
        assert_eq!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), b.slot_id);
    }
    b.complete_gc(cratonvm_types::PointerMap::default());
    assert_ne!(COVERAGE_SLOT_OWNER.load(Ordering::Acquire), b.slot_id);
}

/// Interpreter round i2 wave 64, lane L6
/// (`CRATONVM_DEBUG_STW_UNPARKS_PARKED`): a thread enrolled with
/// `enroll_stw_waker` and parked on its own parker with a long bound is
/// woken by a winning request at once, not at its bound; the guard's drop
/// withdraws the enrollment, and nested enrollments of one thread count
/// once each.
#[test]
fn w64_l6_a_stop_the_world_request_unparks_an_enrolled_waker() {
    let _slot = coverage_slot_tests_serialised();
    let barrier = Arc::new(GcBarrier::new());
    assert_eq!(barrier.stw_wakers_enrolled(), 0);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let parked = {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let outer = barrier.enroll_stw_waker();
            let inner = barrier.enroll_stw_waker();
            ready_tx.send(()).unwrap();
            let started = Instant::now();
            let mut parks = 0u32;
            // The park loop's shape: flag first, then a park bounded far
            // beyond the test's patience.
            while !barrier.stw_requested.load(Ordering::Acquire) {
                std::thread::park_timeout(Duration::from_secs(60));
                parks += 1;
                assert!(
                    started.elapsed() < Duration::from_secs(30),
                    "never unparked"
                );
            }
            drop(inner);
            assert_eq!(barrier.stw_wakers_enrolled(), 1, "the outer level stays");
            drop(outer);
            parks
        })
    };
    ready_rx.recv().unwrap();
    assert_eq!(barrier.stw_wakers_enrolled(), 2, "one entry per enrollment");
    let t0 = Instant::now();
    assert!(barrier.request_stw_opening_cycle(
        ThreadId(1),
        || census(1, vec![], Some(true)),
        || {}
    ));
    let parks = parked.join().unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(20),
        "woken by the request"
    );
    assert!(parks <= 2, "no poll: {parks} parks");
    assert_eq!(barrier.stw_wakers_enrolled(), 0);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// Wave 64, lane L6: a barrier no thread ever enrolled with skips the
/// wakers' lock (`stw_wakers_used` stays false through a pause).
#[test]
fn w64_l6_a_request_with_no_enrolled_waker_touches_no_list() {
    let _slot = coverage_slot_tests_serialised();
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw_opening_cycle(
        ThreadId(1),
        || census(1, vec![], Some(true)),
        || {}
    ));
    assert!(!barrier.stw_wakers_used.load(Ordering::Acquire));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    drop(barrier.enroll_stw_waker());
    assert!(barrier.stw_wakers_used.load(Ordering::Acquire));
    assert_eq!(barrier.stw_wakers_enrolled(), 0);
}

// ── gci i6/jr: the resume census and `CRATONVM_STW_RESUME_FAIRNESS` ──

/// The rule, pure: an unresumed waiter always holds; otherwise only inside
/// the grant of the last resume, and never for the thread that resumed
/// last itself.
#[test]
fn gci_i6jr_the_fairness_rule_holds_for_an_unresumed_waiter_or_inside_the_grant() {
    assert!(resume_fairness_should_hold(1, 10, 0, 250, false));
    assert!(resume_fairness_should_hold(1, 10, 5, 250, true));
    assert!(!resume_fairness_should_hold(0, 10, 0, 250, false), "nobody resumed yet");
    assert!(resume_fairness_should_hold(0, 100, 1, 250, false), "inside the grant");
    assert!(!resume_fairness_should_hold(0, 251, 1, 250, false), "the grant is over");
    assert!(!resume_fairness_should_hold(0, 100, 1, 250, true), "the requester had it");
}

/// A counted waiter released by `complete_gc` is counted once in the
/// census, under its own id, and leaves `waiters_in_wait` at zero; with
/// the switch off nothing ever holds.
#[test]
fn gci_i6jr_a_released_waiter_is_counted_with_its_resume_latency() {
    let barrier = Arc::new(GcBarrier::with_resume_fairness(false));
    assert!(barrier.request_stw(ThreadId(0), 2));
    let b = Arc::clone(&barrier);
    let h = std::thread::spawn(move || b.arrive_and_wait(ThreadId(5)));
    assert!(quota_met_within(&barrier, Duration::from_secs(20)));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let _ = h.join().unwrap();
    let c = barrier.resume_census();
    assert_eq!(c[RC_RESUMES], 1);
    assert_eq!(decode_straggler(c[RC_RESUME_MAX_TID]), Some(5));
    assert_eq!(c[RC_STRADDLES], 0);
    assert_eq!(c[RC_HOLDS], 0);
    assert_eq!(barrier.waiters_in_wait.load(Ordering::Acquire), 0);
    let line = barrier.resume_census_line();
    assert!(line.starts_with("[GC] stw_resume: resume_fairness=off resumes=1 "), "{line}");
    assert!(line.contains(" resume_max_tid=5 "), "{line}");
    assert!(line.contains(" holds=0 hold_avg_us=0 hold_max_us=0 hold_timeouts=0 "), "{line}");
    // gci i18/fh: the in-native rule's slots, then gci i47w/ex47's, close the line.
    let tail = " in_native_holds=0 in_native_skips=0 hold_yielded=0 back_to_back_holds=0";
    assert!(line.ends_with(tail), "{line}");
}

/// A request that finds a released waiter not yet back is a starved
/// request in the census; with the switch off it is not held.
#[test]
fn gci_i6jr_a_starved_request_is_counted_and_not_held_with_the_switch_off() {
    let barrier = GcBarrier::with_resume_fairness(false);
    // A waiter released by the previous pause that has not re-taken the lock.
    barrier.waiters_in_wait.fetch_add(1, Ordering::AcqRel);
    assert!(barrier.request_stw(ThreadId(0), 1));
    let c = barrier.resume_census();
    assert_eq!(c[RC_STARVED_REQUESTS], 1);
    assert_eq!(c[RC_HOLDS], 0);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    barrier.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
}

/// With the switch on, a waiter that never comes back holds the request
/// for the bound and no longer, and the pause still goes ahead.
#[test]
fn gci_i6jr_the_hold_is_bounded_for_a_waiter_that_never_resumes() {
    let barrier = GcBarrier::with_resume_fairness(true);
    barrier.waiters_in_wait.fetch_add(1, Ordering::AcqRel);
    let t0 = Instant::now();
    assert!(barrier.request_stw(ThreadId(0), 1));
    let took = t0.elapsed();
    assert!(took >= RESUME_FAIRNESS_MAX_HOLD, "held for the bound: {took:?}");
    assert!(took < Duration::from_secs(20), "and not forever: {took:?}");
    let c = barrier.resume_census();
    assert_eq!((c[RC_STARVED_REQUESTS], c[RC_HOLDS], c[RC_HOLD_TIMEOUTS]), (1, 1, 1));
    assert!(barrier.stw_requested.load(Ordering::Acquire), "the pause went ahead");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    barrier.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
    assert!(barrier.resume_census_line().contains("resume_fairness=on "));
}

/// gcj j3/rt3 (residuals page item 7): an allocation request whose
/// fairness hold saw a sibling's whole pause complete (the generation
/// moved while `inner` was released) loses under the sibling rule, and
/// wins without it (the old back-to-back duplicate request). The sibling's
/// pauses are simulated by a thread bumping the generation for as long as
/// the request runs; a starved host may let a hold pass without a bump,
/// so the rule gets a few tries.
#[test]
fn gcj3rt3_a_hold_that_saw_a_sibling_pause_complete_loses_under_the_rule() {
    let _slot = coverage_slot_tests_serialised();
    assert!(GcBarrier::new().hold_sees_sibling_pause(), "default on");
    let barrier = Arc::new(GcBarrier::with_resume_fairness(true));
    barrier.waiters_in_wait.fetch_add(1, Ordering::AcqRel);
    let run = Arc::new(AtomicBool::new(true));
    let (b, r) = (Arc::clone(&barrier), Arc::clone(&run));
    let bumper = std::thread::spawn(move || {
        while r.load(Ordering::Acquire) {
            b.gc_generation.fetch_add(1, Ordering::Release);
            std::thread::yield_now();
        }
    });
    let mut lost = false;
    for _ in 0..20 {
        let won = barrier.request_stw_opening_cycle_for_allocation(
            ThreadId(0),
            true,
            || census(1, Vec::new(), Some(true)),
            || {},
        );
        if !won {
            lost = true;
            break;
        }
        barrier.complete_gc(cratonvm_types::PointerMap::default());
    }
    assert!(lost, "a sibling pause inside the hold makes the request lose");
    assert!(barrier.hold_sibling_pause_losses() >= 1);
    assert!(!barrier.stw_requested.load(Ordering::Acquire), "nothing requested");
    // Without the rule the same hold goes ahead.
    let won = barrier.request_stw_opening_cycle_for_allocation(
        ThreadId(0),
        false,
        || census(1, Vec::new(), Some(true)),
        || {},
    );
    assert!(won, "the old answer: a duplicate request");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    run.store(false, Ordering::Release);
    bumper.join().unwrap();
    barrier.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
}

/// With the switch on, a request made right after `complete_gc` lets the
/// released waiter leave its wait loop before the next flag goes up
/// (unless the bound ran out on a starved host, which is then counted).
#[test]
fn gci_i6jr_a_released_waiter_resumes_before_the_next_pause_with_the_switch_on() {
    let barrier = Arc::new(GcBarrier::with_resume_fairness(true));
    assert!(barrier.request_stw(ThreadId(0), 2));
    let b = Arc::clone(&barrier);
    let h = std::thread::spawn(move || b.arrive_and_wait(ThreadId(6)));
    assert!(quota_met_within(&barrier, Duration::from_secs(20)));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    // The very next request, from the initiator's thread.
    assert!(barrier.request_stw(ThreadId(0), 2));
    let c = barrier.resume_census();
    assert!(
        barrier.waiters_in_wait.load(Ordering::Acquire) == 0 || c[RC_HOLD_TIMEOUTS] == 1,
        "the waiter was back before the flag, or the bound was counted: {c:?}"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let _ = h.join().unwrap();
    assert_eq!(barrier.resume_census()[RC_RESUMES], 1);
    assert_eq!(barrier.waiters_in_wait.load(Ordering::Acquire), 0);
}

/// A blocked-region leave that waits out two pauses in a row counts one
/// `leave_repeats`, and each wait is a resume.
#[test]
fn gci_i6jr_a_blocked_leave_across_two_pauses_counts_a_repeat() {
    let barrier = Arc::new(GcBarrier::with_resume_fairness(false));
    let flag = Arc::new(AtomicBool::new(true));
    // The leaver is excluded from both pauses (identity census: blocked).
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![9])));
    let (b, f) = (Arc::clone(&barrier), Arc::clone(&flag));
    let h = std::thread::spawn(move || b.leave_blocked_region_flagged(ThreadId(9), &f));
    // Wait until the leaver is parked in the first pause.
    let deadline = Instant::now() + Duration::from_secs(20);
    while barrier.waiters_in_wait.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline, "the leaver never parked");
        std::thread::yield_now();
    }
    // Complete the first pause and request the second under the lock, so
    // the leaver cannot get between them.
    {
        let mut inner = barrier.inner.lock();
        // `complete_gc`'s generation step, without its lock (we hold it).
        inner.initiator = None;
        inner.initiator_os = None;
        barrier.gc_generation.fetch_add(1, Ordering::Release);
        barrier.lower_stw_requested();
        barrier.released_at_ns.store(barrier.resume_stamp_now(), Ordering::Release);
        barrier.gc_complete.notify_all();
        // The second pause, still excluding the leaver.
        let excluded = vec![9u64];
        assert!(barrier.request_stw_counted_locked(
            &mut inner,
            ThreadId(0),
            2,
            Some(1),
            Some(excluded),
            None,
            None::<fn()>,
        ));
    }
    // The leaver wakes into the second pause and waits it out too.
    let deadline = Instant::now() + Duration::from_secs(20);
    while barrier.resume_census()[RC_LEAVE_REPEATS] == 0 {
        assert!(Instant::now() < deadline, "the second wait was never counted");
        std::thread::yield_now();
    }
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    h.join().unwrap();
    assert!(!flag.load(Ordering::Acquire), "the leave finished");
    let c = barrier.resume_census();
    assert_eq!(c[RC_LEAVE_REPEATS], 1);
    assert_eq!(c[RC_RESUMES], 2);
    assert_eq!(decode_straggler(c[RC_RESUME_MAX_TID]), Some(9));
}

// ── gci i15/hx: `CRATONVM_STW_IN_NATIVE_LEAVE_FAIRNESS` ──

/// With no in-native leave ever made, the rule is inert: a request is not
/// held, whatever the switch says.
#[test]
fn gci_i15hx_the_in_native_leave_rule_is_inert_without_a_leaver() {
    let barrier = GcBarrier::with_in_native_leave_fairness(true);
    assert!(barrier.request_stw(ThreadId(0), 1));
    assert_eq!(barrier.resume_census()[RC_HOLDS], 0);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

/// An in-native leaver released by the previous pause that has not left
/// yet holds the next request -- for the bound and no longer when it never
/// comes back -- and the pause still goes ahead; with the switch killed
/// (`=0`) it is not held.
#[test]
fn gci_i15hx_an_unfinished_in_native_leave_holds_the_next_request_bounded() {
    let barrier = GcBarrier::with_in_native_leave_fairness(true);
    barrier.in_native_leavers.fetch_add(1, Ordering::AcqRel);
    let t0 = Instant::now();
    assert!(barrier.request_stw(ThreadId(0), 1));
    let took = t0.elapsed();
    assert!(took >= RESUME_FAIRNESS_MAX_HOLD, "held for the bound: {took:?}");
    assert!(took < Duration::from_secs(20), "and not forever: {took:?}");
    let c = barrier.resume_census();
    assert_eq!((c[RC_HOLDS], c[RC_HOLD_TIMEOUTS]), (1, 1));
    assert!(barrier.stw_requested.load(Ordering::Acquire), "the pause went ahead");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    barrier.in_native_leavers.fetch_sub(1, Ordering::AcqRel);

    let killed = GcBarrier::with_in_native_leave_fairness(false);
    killed.in_native_leavers.fetch_add(1, Ordering::AcqRel);
    assert!(killed.request_stw(ThreadId(0), 1));
    assert_eq!(killed.resume_census()[RC_HOLDS], 0, "=0 restores the old request");
    killed.complete_gc(cratonvm_types::PointerMap::default());
    killed.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
}

/// The grant is never held against the thread that finished the leave
/// itself (it has had its window), and the rule, pure, holds only inside
/// the grant of another thread's leave.
#[test]
fn gci_i15hx_the_grant_belongs_to_the_leaver_not_to_the_requester() {
    let barrier = GcBarrier::with_in_native_leave_fairness(true);
    let me = current_thread_token().expect("a thread token");
    barrier
        .last_in_native_leave_ns
        .store(barrier.resume_stamp_now(), Ordering::Release);
    barrier.last_in_native_leave_token.store(me, Ordering::Release);
    assert!(!barrier.in_native_leave_should_hold(me), "the leaver itself requests");
    assert!(barrier.request_stw(ThreadId(0), 1));
    assert_eq!(barrier.resume_census()[RC_HOLDS], 0);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    // Another thread finished a leave "now": a third thread's request is
    // inside its grant (the stamp is in the future, so no scheduling delay
    // can end the grant before the question is asked).
    barrier
        .last_in_native_leave_ns
        .store(barrier.resume_stamp_now() + 60_000_000_000, Ordering::Release);
    assert!(barrier.in_native_leave_should_hold(me.wrapping_add(1)));
    // A grant that ended long ago holds nothing.
    barrier.last_in_native_leave_ns.store(1, Ordering::Release);
    std::thread::sleep(IN_NATIVE_LEAVE_GRANT * 2);
    assert!(!barrier.in_native_leave_should_hold(me.wrapping_add(1)));
}

/// A real in-native leave that waits out a pause is counted as a leaver
/// while it waits, and on its way out stamps its grant on its own thread
/// and leaves the count at zero (whichever way the flag went).
#[test]
fn gci_i15hx_an_in_native_leave_across_a_pause_is_counted_then_stamped() {
    use std::sync::atomic::AtomicBool;
    let barrier = Arc::new(GcBarrier::with_in_native_leave_fairness(true));
    let flag = Arc::new(AtomicBool::new(true));
    assert!(!barrier.mark_in_native_enter());
    // The pause excludes thread 7 (identity census: its flag is up).
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![7])));
    let leaver = {
        let barrier = Arc::clone(&barrier);
        let flag = Arc::clone(&flag);
        std::thread::spawn(move || {
            let cleared = barrier.leave_in_native_if(ThreadId(7), &flag, || true);
            (cleared, current_thread_token().expect("a thread token"))
        })
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while barrier.in_native_leavers.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline, "the leaver never waited");
        std::thread::yield_now();
    }
    assert_eq!(barrier.last_in_native_leave_ns.load(Ordering::Acquire), 0);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let (cleared, leaver_token) = leaver.join().expect("the leaver does not panic");
    assert!(cleared);
    assert!(!flag.load(Ordering::Acquire));
    assert_eq!(barrier.in_native_leavers.load(Ordering::Acquire), 0);
    assert_ne!(barrier.last_in_native_leave_ns.load(Ordering::Acquire), 0);
    assert_eq!(
        barrier.last_in_native_leave_token.load(Ordering::Acquire),
        leaver_token,
        "the grant is the leaver's"
    );
    assert_eq!(barrier.blocked_count(), 0, "released exactly once");
}

/// A leave with no pause requested takes the one-hold path and touches
/// none of the rule's state (the package's common case stays as cheap).
#[test]
fn gci_i15hx_an_uncontended_in_native_leave_is_not_a_leaver() {
    use std::sync::atomic::AtomicBool;
    let barrier = GcBarrier::with_in_native_leave_fairness(true);
    let flag = AtomicBool::new(true);
    assert!(!barrier.mark_in_native_enter());
    assert!(barrier.leave_in_native_if(ThreadId(7), &flag, || true));
    assert_eq!(barrier.in_native_leavers.load(Ordering::Acquire), 0);
    assert_eq!(barrier.last_in_native_leave_ns.load(Ordering::Acquire), 0);
}

// ── gci i15/rb: review of the i15/hx rule ──

/// gci i15/rb (finding 2 of
/// `gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`): an
/// in-native leave that unwinds out of its waiting path (here its
/// `clear_now` panics, as the stress tripwire can) still ends its leaver
/// count and stamps its grant, so a later request is not held for the
/// bound by a leaver that is gone.
#[test]
fn gci_i15rb_an_unwinding_in_native_leave_still_ends_its_count() {
    use std::sync::atomic::AtomicBool;
    let barrier = Arc::new(GcBarrier::with_in_native_leave_fairness(true));
    let flag = Arc::new(AtomicBool::new(true));
    assert!(!barrier.mark_in_native_enter());
    // The pause excludes thread 7 (identity census: its flag is up).
    assert!(barrier.request_stw_counted_with_live_blocked(ThreadId(0), || (2, 1, vec![7])));
    let leaver = {
        let barrier = Arc::clone(&barrier);
        let flag = Arc::clone(&flag);
        std::thread::spawn(move || {
            barrier.leave_in_native_if(ThreadId(7), &flag, || -> bool {
                panic!("gci_i15rb: clear_now unwinds")
            })
        })
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while barrier.in_native_leavers.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline, "the leaver never waited");
        std::thread::yield_now();
    }
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(leaver.join().is_err(), "the closure's panic reached the caller");
    assert_eq!(barrier.in_native_leavers.load(Ordering::Acquire), 0, "not leaked");
    assert_ne!(barrier.last_in_native_leave_ns.load(Ordering::Acquire), 0, "stamped");
    // Past the grant, a request is not held at all.
    std::thread::sleep(IN_NATIVE_LEAVE_GRANT * 4);
    assert!(barrier.request_stw(ThreadId(0), 1));
    assert_eq!(barrier.resume_census()[RC_HOLDS], 0, "no leaked leaver holds it");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}

// ── gci i17/jh: `gci-i15orch-jni-repeat-probe-full-hangs-silently-on-the-default-path-FIXED-20261017` ──

/// gci i17/jh: the repeat probe's two threads at the barrier, with the
/// default in-native leave rule. A JNI loop thread goes into native and
/// back ROUNDS times (`jni::enter_in_native_region`, then
/// `jni::native_call_leave_native`: the quiet leave when no pause
/// completed since the entry, else the full wake's flagged leave) and
/// polls while it runs; a churn thread requests pause after pause with no
/// gap, its census excluding the loop thread exactly while its flag is up
/// (the identity census, read under `inner` as the locked request reads
/// it). The loop must finish every round: no lost wakeup, no wait that
/// outlives its pause, no leave starved for good. A regression fails on
/// the deadline instead of hanging the test binary.
#[test]
fn gci_i17jh_an_in_native_loop_progresses_under_back_to_back_pauses() {
    let _slot = coverage_slot_tests_serialised();
    const ROUNDS: u64 = 500;
    let mut b = GcBarrier::with_in_native_leave_fairness(true);
    // The census below reads the flag under `inner` BEFORE the request
    // raises `stw_requested`: the locked entry's order (the lock-free
    // entry raises first and needs `request_flag_then_census_locked`).
    b.lockfree_blocked = false;
    let barrier = Arc::new(b);
    let flag = Arc::new(AtomicBool::new(false));
    let pauses = Arc::new(AtomicU64::new(0));
    let rounds = Arc::new(AtomicU64::new(0));
    let loop_parked = Arc::new(AtomicBool::new(false));
    let churn_stopped = Arc::new(AtomicBool::new(false));

    let churn = {
        let barrier = Arc::clone(&barrier);
        let flag = Arc::clone(&flag);
        let pauses = Arc::clone(&pauses);
        let loop_parked = Arc::clone(&loop_parked);
        let churn_stopped = Arc::clone(&churn_stopped);
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !loop_parked.load(Ordering::Acquire) {
                {
                    let mut inner = barrier.inner.lock();
                    // `request_stw_opening_cycle`'s hold, then its census.
                    if !barrier.stw_requested.load(Ordering::Acquire) {
                        let _ = barrier.resume_fairness_hold(&mut inner);
                    }
                    let (blocked, tids) = if flag.load(Ordering::Acquire) {
                        (1, vec![7u64])
                    } else {
                        (0, Vec::new())
                    };
                    assert!(barrier.request_stw_counted_locked(
                        &mut inner,
                        ThreadId(0),
                        2,
                        Some(blocked),
                        Some(tids),
                        None,
                        None::<fn()>,
                    ));
                }
                let until = Instant::now() + Duration::from_secs(30);
                while !barrier.wait_for_all_timeout(Duration::from_millis(50)) {
                    assert!(Instant::now() < until, "pause {n}: a counted loop thread never arrived");
                }
                barrier.complete_gc(cratonvm_types::PointerMap::default());
                n += 1;
                pauses.store(n, Ordering::Release);
            }
            churn_stopped.store(true, Ordering::Release);
            n
        })
    };

    let looper = {
        let barrier = Arc::clone(&barrier);
        let flag = Arc::clone(&flag);
        let pauses = Arc::clone(&pauses);
        let rounds = Arc::clone(&rounds);
        let loop_parked = Arc::clone(&loop_parked);
        let churn_stopped = Arc::clone(&churn_stopped);
        std::thread::spawn(move || {
            let tid = ThreadId(7);
            let started = Instant::now() + Duration::from_secs(60);
            while pauses.load(Ordering::Acquire) == 0 {
                assert!(Instant::now() < started, "the churn thread never paused");
                // gci i18/fh (i18a red): running Java with the flag down,
                // so the first pause's census COUNTS this thread -- it must
                // poll here as in its loop, or that pause waits for it
                // forever (the deadlock this wait used to be).
                if barrier.stop_requested() {
                    let _ = barrier.arrive_and_wait_auto(tid);
                }
                std::thread::yield_now();
            }
            for _ in 0..ROUNDS {
                // Running -> in native: the flag, then the counted entry,
                // arriving when a pause raced it.
                flag.store(true, Ordering::Release);
                if barrier.mark_in_native_enter() {
                    let _ = barrier.arrive_and_wait_auto(tid);
                }
                let entered = barrier.gc_generation.load(Ordering::Acquire);
                // In native -> running.
                let quiet = barrier.leave_in_native_if(tid, &flag, || {
                    barrier.gc_generation.load(Ordering::Acquire) == entered
                });
                if !quiet {
                    barrier.leave_blocked_region_flagged(tid, &flag);
                }
                assert!(!flag.load(Ordering::Acquire), "the leave cleared the flag");
                // Running Java: a poll.
                if barrier.stop_requested() {
                    let _ = barrier.arrive_and_wait_auto(tid);
                }
                rounds.fetch_add(1, Ordering::AcqRel);
            }
            // In native for good, so no later census counts this thread.
            flag.store(true, Ordering::Release);
            if barrier.mark_in_native_enter() {
                let _ = barrier.arrive_and_wait_auto(tid);
            }
            loop_parked.store(true, Ordering::Release);
            while !churn_stopped.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            if !barrier.leave_in_native_if(tid, &flag, || true) {
                barrier.leave_blocked_region_flagged(tid, &flag);
            }
        })
    };

    let deadline = Instant::now() + Duration::from_secs(120);
    while !loop_parked.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "the loop thread stalled at round {} of {ROUNDS} after {} pauses",
            rounds.load(Ordering::Acquire),
            pauses.load(Ordering::Acquire)
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let n = churn.join().expect("the churn thread does not panic");
    looper.join().expect("the loop thread does not panic");
    assert_eq!(rounds.load(Ordering::Acquire), ROUNDS);
    assert!(n >= 1, "the pauses really interleaved");
    assert!(!flag.load(Ordering::Acquire), "the loop thread left native");
    assert!(!barrier.stop_requested());
    assert_eq!(barrier.in_native_leavers.load(Ordering::Acquire), 0);
    assert_eq!(barrier.blocked_count(), 0, "every entry released exactly once");
}

// ── gci i18/fh: `CRATONVM_STW_IN_NATIVE_LEAVE_RACE_ONLY` ──

/// gci i18/fh: stamp an in-native leave done by ANOTHER thread `age_ns`
/// ago on `barrier` (whose epoch must be older than that).
fn gci_i18fh_stamp_leave_done_ago(barrier: &GcBarrier, age_ns: u64) {
    let me = current_thread_token().expect("a thread token");
    let now = barrier.resume_stamp_now();
    assert!(now > age_ns + 1, "the barrier is older than the stamp");
    barrier.last_in_native_leave_token.store(me.wrapping_add(1), Ordering::Release);
    barrier.last_in_native_leave_ns.store(now - age_ns, Ordering::Release);
}

/// The trigger, by the barrier's own predicates: a leaver not out yet, or
/// one out "now" (a stamp in the future: no scheduling delay can age it),
/// races a request; one out half a grant ago does not race it although it
/// is still inside its grant; the requester's own leave never races it.
/// With the switch off the trigger is i15/hx's: the whole grant.
#[test]
fn gci_i18fh_only_a_request_that_races_the_leaver_starts_a_hold() {
    let barrier = GcBarrier::with_in_native_leave_race_only(true);
    let me = current_thread_token().expect("a thread token");
    assert!(!barrier.in_native_leave_raced(me), "no leave ever made");
    barrier.in_native_leavers.fetch_add(1, Ordering::AcqRel);
    assert!(barrier.in_native_leave_raced(me), "a leaver not out yet");
    barrier.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
    barrier.last_in_native_leave_token.store(me.wrapping_add(1), Ordering::Release);
    barrier
        .last_in_native_leave_ns
        .store(barrier.resume_stamp_now() + 60_000_000_000, Ordering::Release);
    assert!(barrier.in_native_leave_raced(me), "a leaver out just now");
    barrier.last_in_native_leave_token.store(me, Ordering::Release);
    assert!(!barrier.in_native_leave_raced(me), "the requester's own leave");

    // Half a grant ago: past the race window (raced is monotone in time,
    // so this half is exact), still inside the grant (retried, since a
    // loaded host can age the stamp past the grant between two lines).
    let half = duration_ns(IN_NATIVE_LEAVE_GRANT) / 2;
    assert!(half >= duration_ns(IN_NATIVE_LEAVE_RACE_WINDOW), "the window is inside the grant");
    let off = GcBarrier::with_in_native_leave_race_only(false);
    std::thread::sleep(IN_NATIVE_LEAVE_GRANT * 4);
    let mut seen_in_grant = false;
    for _ in 0..200 {
        gci_i18fh_stamp_leave_done_ago(&barrier, half);
        gci_i18fh_stamp_leave_done_ago(&off, half);
        let on_raced = barrier.in_native_leave_raced(me);
        let on_grant = barrier.in_native_leave_should_hold(me);
        let off_raced = off.in_native_leave_raced(me);
        assert!(!on_raced, "half a grant ago does not race");
        if on_grant && off_raced {
            seen_in_grant = true;
            break;
        }
    }
    assert!(seen_in_grant, "never inside the grant in 200 tries");
}

/// A request late in an in-native leaver's grant is not held with the
/// switch on (counted `in_native_skips`), and is held with it off (=0
/// restores i15/hx).
#[test]
fn gci_i18fh_a_request_late_in_the_grant_is_held_only_with_the_switch_off() {
    let half = duration_ns(IN_NATIVE_LEAVE_GRANT) / 2;
    let on = GcBarrier::with_in_native_leave_race_only(true);
    let off = GcBarrier::with_in_native_leave_race_only(false);
    std::thread::sleep(IN_NATIVE_LEAVE_GRANT * 4);
    let mut skipped = false;
    for _ in 0..200 {
        gci_i18fh_stamp_leave_done_ago(&on, half);
        assert!(on.request_stw(ThreadId(0), 1));
        on.complete_gc(cratonvm_types::PointerMap::default());
        let c = on.resume_census();
        assert_eq!(c[RC_HOLDS], 0, "a request that did not race is never held");
        if c[RC_IN_NATIVE_SKIPS] > 0 {
            skipped = true;
            break;
        }
    }
    assert!(skipped, "never inside the grant in 200 tries");
    assert!(on.resume_census_line().contains(" in_native_skips=1 "), "{}", on.resume_census_line());

    let mut held = false;
    for _ in 0..200 {
        gci_i18fh_stamp_leave_done_ago(&off, half);
        assert!(off.request_stw(ThreadId(0), 1));
        off.complete_gc(cratonvm_types::PointerMap::default());
        let c = off.resume_census();
        assert_eq!(c[RC_IN_NATIVE_SKIPS], 0, "=0 never skips");
        if c[RC_HOLDS] > 0 {
            assert_eq!(c[RC_IN_NATIVE_HOLDS], c[RC_HOLDS]);
            held = true;
            break;
        }
    }
    assert!(held, "=0: never held inside the grant in 200 tries");
}

/// A request that raced a leaver not out yet is held as i15/hx held it
/// (here to the bound: the leaver never comes out) and is counted an
/// in-native hold.
#[test]
fn gci_i18fh_a_racing_request_is_still_held() {
    let barrier = GcBarrier::with_in_native_leave_race_only(true);
    barrier.in_native_leavers.fetch_add(1, Ordering::AcqRel);
    let t0 = Instant::now();
    assert!(barrier.request_stw(ThreadId(0), 1));
    assert!(t0.elapsed() >= RESUME_FAIRNESS_MAX_HOLD, "held for the bound");
    let c = barrier.resume_census();
    assert_eq!((c[RC_HOLDS], c[RC_IN_NATIVE_HOLDS], c[RC_HOLD_TIMEOUTS]), (1, 1, 1));
    assert_eq!(c[RC_HOLD_YIELDED], 0);
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    barrier.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
}

/// A hold ends as soon as another thread's pause is requested (the holder
/// is a counted mutator that does not poll while it holds), and the
/// holding request then loses. Retried: an attempt where the other
/// request came before the hold (no hold) or after the bound (a timeout)
/// decides nothing.
#[test]
fn gci_i18fh_a_hold_yields_to_another_threads_pause() {
    let mut yielded = false;
    for _ in 0..50 {
        let barrier = Arc::new(GcBarrier::with_in_native_leave_race_only(true));
        // A leaver that never comes out: the hold would run to the bound.
        barrier.in_native_leavers.fetch_add(1, Ordering::AcqRel);
        let start = Instant::now();
        let other = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                // A spin, not a sleep: a Windows sleep rounds to its tick.
                while start.elapsed() < Duration::from_micros(300) {
                    std::hint::spin_loop();
                }
                let mut inner = barrier.inner.lock();
                barrier.request_stw_counted_locked(
                    &mut inner,
                    ThreadId(1),
                    1,
                    Some(0),
                    Some(Vec::new()),
                    None,
                    None::<fn()>,
                )
            })
        };
        let won = barrier.request_stw(ThreadId(0), 1);
        let other_won = other.join().expect("the other requester does not panic");
        assert!(won != other_won, "exactly one request wins");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        barrier.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
        let c = barrier.resume_census();
        if c[RC_HOLD_YIELDED] == 1 {
            assert!(other_won, "the holder lost to the pause it yielded to");
            assert_eq!((c[RC_HOLDS], c[RC_HOLD_TIMEOUTS]), (1, 0));
            yielded = true;
            break;
        }
    }
    assert!(yielded, "the other request never landed inside a hold in 50 tries");
}

/// With the switch off a hold does not yield (i15/hx's loop exactly): a
/// pause requested meanwhile waits out the bound.
#[test]
fn gci_i18fh_with_the_switch_off_a_hold_never_yields() {
    let barrier = Arc::new(GcBarrier::with_in_native_leave_race_only(false));
    barrier.in_native_leavers.fetch_add(1, Ordering::AcqRel);
    let other = {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let start = Instant::now();
            while start.elapsed() < Duration::from_micros(300) {
                std::hint::spin_loop();
            }
            let mut inner = barrier.inner.lock();
            barrier.request_stw_counted_locked(
                &mut inner,
                ThreadId(1),
                1,
                Some(0),
                Some(Vec::new()),
                None,
                None::<fn()>,
            )
        })
    };
    let _ = barrier.request_stw(ThreadId(0), 1);
    let _ = other.join().expect("the other requester does not panic");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    barrier.in_native_leavers.fetch_sub(1, Ordering::AcqRel);
    assert_eq!(barrier.resume_census()[RC_HOLD_YIELDED], 0);
}

// ── gci i47w/ex47: `CRATONVM_STW_BACK_TO_BACK_FAIRNESS` ──
// `gci-i47w-oz47-system-exit-hangs-beside-a-system-gc-daemon-FIXED-20261017`.

/// The trigger, pure: only the thread that ran the previous
/// `complete_gc`, only inside the window after that release.
#[test]
fn gci_i47w_ex47_the_back_to_back_trigger_is_the_last_completer_inside_the_window() {
    assert!(back_to_back_should_start(100, 150, 100, true));
    assert!(!back_to_back_should_start(100, 150, 100, false), "another thread completed last");
    assert!(!back_to_back_should_start(0, 150, 100, true), "no pause ever released");
    assert!(!back_to_back_should_start(100, 200, 100, true), "the window is over");
    assert!(!back_to_back_should_start(u64::MAX, 1, u64::MAX, false));
}

/// The initiator's very next request, made while a waiter its pause
/// released has not re-taken the lock, is held -- to the bound when the
/// waiter never comes back -- and the pause still goes ahead; killed
/// (`=0`), the same request is only counted starved.
#[test]
fn gci_i47w_ex47_a_back_to_back_request_holds_for_an_unresumed_waiter_bounded() {
    for on in [true, false] {
        let barrier = GcBarrier::with_back_to_back_fairness(on);
        assert!(barrier.request_stw(ThreadId(0), 1));
        // Completed on THIS thread: its next request is back to back.
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        // The release "now" for good (a stamp in the future): no scheduling
        // delay can end the window before the request asks.
        barrier
            .released_at_ns
            .store(barrier.resume_stamp_now() + 60_000_000_000, Ordering::Release);
        // A waiter that pause released and that never re-takes the lock.
        barrier.waiters_in_wait.fetch_add(1, Ordering::AcqRel);
        let t0 = Instant::now();
        assert!(barrier.request_stw(ThreadId(0), 1));
        let took = t0.elapsed();
        let c = barrier.resume_census();
        assert_eq!(c[RC_STARVED_REQUESTS], 1);
        if on {
            assert!(took >= RESUME_FAIRNESS_MAX_HOLD, "held for the bound: {took:?}");
            assert!(took < Duration::from_secs(20), "and not forever: {took:?}");
            assert_eq!((c[RC_HOLDS], c[RC_BACK_TO_BACK_HOLDS], c[RC_HOLD_TIMEOUTS]), (1, 1, 1));
            assert_eq!(c[RC_IN_NATIVE_HOLDS], 0);
            assert!(barrier.resume_census_line().ends_with(" back_to_back_holds=1"));
        } else {
            assert_eq!((c[RC_HOLDS], c[RC_BACK_TO_BACK_HOLDS]), (0, 0), "=0: the old request");
        }
        assert!(barrier.stw_requested.load(Ordering::Acquire), "the pause went ahead");
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        barrier.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Not back to back: a request by a thread that did not run the previous
/// `complete_gc`, or one made after the window, is not held although a
/// released waiter is still out (the general rule stays opt-in).
#[test]
fn gci_i47w_ex47_a_request_by_another_thread_or_after_the_window_is_not_held() {
    let barrier = GcBarrier::with_back_to_back_fairness(true);
    let me = current_thread_token().expect("a thread token");
    assert!(barrier.request_stw(ThreadId(0), 1));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert_eq!(barrier.last_complete_token.load(Ordering::Acquire), me, "the completer");
    barrier.waiters_in_wait.fetch_add(1, Ordering::AcqRel);

    // Another thread ran the last `complete_gc`.
    barrier.last_complete_token.store(me.wrapping_add(1), Ordering::Release);
    barrier
        .released_at_ns
        .store(barrier.resume_stamp_now() + 60_000_000_000, Ordering::Release);
    assert!(!barrier.back_to_back_raced(me));
    assert!(barrier.request_stw(ThreadId(0), 1));
    assert_eq!(barrier.resume_census()[RC_HOLDS], 0, "not the initiator's request");
    barrier.complete_gc(cratonvm_types::PointerMap::default());

    // This thread completed last, but long ago.
    std::thread::sleep(BACK_TO_BACK_WINDOW * 2);
    barrier.last_complete_token.store(me, Ordering::Release);
    barrier.released_at_ns.store(1, Ordering::Release);
    assert!(!barrier.back_to_back_raced(me));
    assert!(barrier.request_stw(ThreadId(0), 1));
    assert_eq!(barrier.resume_census()[RC_HOLDS], 0, "a request at its own pace");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    barrier.waiters_in_wait.fetch_sub(1, Ordering::AcqRel);
    assert_eq!(barrier.resume_census()[RC_STARVED_REQUESTS], 2);
}

/// The defect's shape on two plain threads: an initiator requesting pause
/// after pause with no gap (`System.gc()` in a loop: hold, census, request,
/// wait, `complete_gc`, again), and a thread that enters and leaves a
/// blocked region ROUNDS times and polls between them, excluded from every
/// pause whose census found its flag up. Every leave has to see NO pause
/// requested, so without the rule it loses the race to each next request
/// and its progress is unbounded; with it every round finishes. A
/// regression fails on the deadline instead of hanging the test binary.
#[test]
fn gci_i47w_ex47_a_blocked_leave_progresses_under_a_back_to_back_initiator() {
    let _slot = coverage_slot_tests_serialised();
    const ROUNDS: u64 = 200;
    let mut b = GcBarrier::with_back_to_back_fairness(true);
    // The census below reads the flag under `inner` BEFORE the request
    // raises `stw_requested` (the locked entry's order).
    b.lockfree_blocked = false;
    let barrier = Arc::new(b);
    let flag = Arc::new(AtomicBool::new(false));
    let pauses = Arc::new(AtomicU64::new(0));
    let rounds = Arc::new(AtomicU64::new(0));
    let leaver_done = Arc::new(AtomicBool::new(false));
    let churn_stopped = Arc::new(AtomicBool::new(false));
    let (first_request_tx, first_request_rx) = std::sync::mpsc::channel();

    let churn = {
        let barrier = Arc::clone(&barrier);
        let flag = Arc::clone(&flag);
        let pauses = Arc::clone(&pauses);
        let leaver_done = Arc::clone(&leaver_done);
        let churn_stopped = Arc::clone(&churn_stopped);
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !leaver_done.load(Ordering::Acquire) {
                {
                    let mut inner = barrier.inner.lock();
                    // `request_stw_opening_cycle`'s hold, then its census.
                    if !barrier.stw_requested.load(Ordering::Acquire) {
                        let _ = barrier.resume_fairness_hold(&mut inner);
                    }
                    let (blocked, tids) = if flag.load(Ordering::Acquire) {
                        (1, vec![9u64])
                    } else {
                        (0, Vec::new())
                    };
                    assert!(barrier.request_stw_counted_locked(
                        &mut inner,
                        ThreadId(0),
                        2,
                        Some(blocked),
                        Some(tids),
                        None,
                        None::<fn()>,
                    ));
                }
                if n == 0 {
                    first_request_tx.send(()).expect("the leaver is waiting");
                }
                let until = Instant::now() + Duration::from_secs(30);
                while !barrier.wait_for_all_timeout(Duration::from_millis(50)) {
                    assert!(Instant::now() < until, "pause {n}: the leaver never arrived");
                }
                barrier.complete_gc(cratonvm_types::PointerMap::default());
                n += 1;
                pauses.store(n, Ordering::Release);
            }
            churn_stopped.store(true, Ordering::Release);
            n
        })
    };

    let leaver = {
        let barrier = Arc::clone(&barrier);
        let flag = Arc::clone(&flag);
        let rounds = Arc::clone(&rounds);
        let leaver_done = Arc::clone(&leaver_done);
        let churn_stopped = Arc::clone(&churn_stopped);
        std::thread::spawn(move || {
            // The old fixture could finish all 200 rounds before the
            // initiator was scheduled, failing only its overlap assertion.
            // Start the leaves after the first pause has really opened.
            first_request_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("the initiator opens the first pause");
            let tid = ThreadId(9);
            for _ in 0..ROUNDS {
                // Running -> blocked (a class-file read, a console write).
                // A pause whose census missed the store counts this thread;
                // the flagged leave then arrives for it exactly once.
                flag.store(true, Ordering::Release);
                // Blocked -> running: only when no pause is requested.
                barrier.leave_blocked_region_flagged(tid, &flag);
                assert!(!flag.load(Ordering::Acquire), "the leave cleared the flag");
                // Running Java: a poll.
                if barrier.stop_requested() {
                    let _ = barrier.arrive_and_wait_auto(tid);
                }
                rounds.fetch_add(1, Ordering::AcqRel);
            }
            // Blocked for good, answering any pause a census counted it in
            // until the churn stops.
            flag.store(true, Ordering::Release);
            leaver_done.store(true, Ordering::Release);
            while !churn_stopped.load(Ordering::Acquire) {
                if barrier.stop_requested() {
                    let _ = barrier.arrive_and_wait_auto(tid);
                }
                std::thread::yield_now();
            }
            barrier.leave_blocked_region_flagged(tid, &flag);
        })
    };

    let deadline = Instant::now() + Duration::from_secs(120);
    while !leaver_done.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "the leaver stalled at round {} of {ROUNDS} after {} pauses: {}",
            rounds.load(Ordering::Acquire),
            pauses.load(Ordering::Acquire),
            barrier.resume_census_line()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let n = churn.join().expect("the churn thread does not panic");
    leaver.join().expect("the leaver does not panic");
    assert_eq!(rounds.load(Ordering::Acquire), ROUNDS);
    assert!(n >= 1, "the pauses really interleaved");
    assert!(!flag.load(Ordering::Acquire), "the leaver left its region");
    assert!(!barrier.stop_requested());
    assert_eq!(barrier.waiters_in_wait.load(Ordering::Acquire), 0);
    let c = barrier.resume_census();
    assert!(c[RC_BACK_TO_BACK_HOLDS] <= c[RC_HOLDS], "{c:?}");
    assert_eq!(c[RC_IN_NATIVE_HOLDS], 0, "the in-native rule is off here");
}

/// gci i20/jt: the census rule `GcBlockState::census_blocked` applies,
/// on two plain words: `leaving` first, then the flag.
fn gci_i20jt_census_blocked(leaving: &AtomicBool, flag: &AtomicBool) -> bool {
    !leaving.load(Ordering::Acquire) && flag.load(Ordering::Acquire)
}

/// gci i20/jt (`CRATONVM_JNI_IN_NATIVE_LOCKFREE_LEAVE`): with no pause in
/// progress the lock-free in-native leave never takes the barrier lock --
/// it completes while another thread HOLDS `inner` -- and ends exactly as
/// the locked leave does (count released, flag cleared when told to, left
/// up otherwise, `leaving` down either way).
#[test]
fn gci_i20jt_a_quiet_lockfree_leave_takes_no_lock() {
    let barrier = Arc::new(GcBarrier::with_lockfree_blocked(true));
    let flag = Arc::new(AtomicBool::new(true));
    let leaving = Arc::new(AtomicBool::new(false));
    for clear in [true, false] {
        flag.store(true, Ordering::Release);
        assert!(!barrier.mark_in_native_enter(), "no pause in progress");
        assert_eq!(barrier.blocked_count(), 1);
        let held = barrier.inner.lock();
        let leaver = {
            let (barrier, flag, leaving) =
                (Arc::clone(&barrier), Arc::clone(&flag), Arc::clone(&leaving));
            std::thread::spawn(move || {
                barrier.leave_in_native_lockfree_if(ThreadId(7), &flag, &leaving, || clear)
            })
        };
        let t0 = Instant::now();
        while !leaver.is_finished() {
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "the quiet leave waited for the barrier lock"
            );
            std::thread::yield_now();
        }
        drop(held);
        assert_eq!(leaver.join().expect("the leaver does not panic"), clear);
        assert_eq!(barrier.blocked_count(), 0, "released exactly once");
        assert_eq!(flag.load(Ordering::Acquire), !clear, "cleared only when told to");
        assert!(!leaving.load(Ordering::Acquire), "leaving is reset");
    }
}

/// gci i20/jt: under the census-first request order (the lock-free
/// blocked transitions off) the Dekker pair has no partner, so the
/// lock-free leave IS the locked one: it waits for the lock.
#[test]
fn gci_i20jt_without_the_flag_first_order_the_leave_takes_the_lock() {
    let barrier = Arc::new(GcBarrier::with_lockfree_blocked(false));
    let flag = Arc::new(AtomicBool::new(true));
    let leaving = Arc::new(AtomicBool::new(false));
    assert!(!barrier.mark_in_native_enter());
    let held = barrier.inner.lock();
    let leaver = {
        let (barrier, flag, leaving) =
            (Arc::clone(&barrier), Arc::clone(&flag), Arc::clone(&leaving));
        std::thread::spawn(move || {
            barrier.leave_in_native_lockfree_if(ThreadId(7), &flag, &leaving, || true)
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    assert!(!leaver.is_finished(), "the fallback is the locked leave");
    assert!(!leaving.load(Ordering::Acquire), "the fallback never raises leaving");
    drop(held);
    assert!(leaver.join().expect("the leaver does not panic"));
    assert!(!flag.load(Ordering::Acquire));
    assert_eq!(barrier.blocked_count(), 0);
}

/// gci i20/jt: a pause whose census EXCLUDED the thread (flag up,
/// `leaving` down when it read them) holds the lock-free leave until
/// `complete_gc`, the flag stays up throughout, and the leave then reports
/// the generation change (`false`: the full wake).
#[test]
fn gci_i20jt_an_excluding_pause_holds_the_lockfree_leave_with_the_flag_up() {
    let _slot = coverage_slot_tests_serialised();
    let barrier = Arc::new(GcBarrier::with_lockfree_blocked(true));
    let flag = Arc::new(AtomicBool::new(true));
    let leaving = Arc::new(AtomicBool::new(false));
    assert!(!barrier.mark_in_native_enter());
    let gen0 = barrier.gc_generation.load(Ordering::Acquire);
    let blocked = gci_i20jt_census_blocked(&leaving, &flag);
    assert!(blocked, "the census reads the in-native thread blocked");
    assert!(barrier.request_stw_opening_cycle(
        ThreadId(1),
        || census(2, vec![7], Some(true)),
        || {}
    ));
    let leaver = {
        let (barrier, flag, leaving) =
            (Arc::clone(&barrier), Arc::clone(&flag), Arc::clone(&leaving));
        std::thread::spawn(move || {
            barrier.leave_in_native_lockfree_if(ThreadId(7), &flag, &leaving, || {
                barrier.gc_generation.load(Ordering::Acquire) == gen0
            })
        })
    };
    assert!(
        barrier.wait_for_all_timeout(Duration::from_secs(5)),
        "an excluded thread is not waited for"
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(!leaver.is_finished(), "an excluded thread does not leave during the pause");
    assert!(flag.load(Ordering::Acquire), "its flag stays up for the root scan");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(
        !leaver.join().expect("the leaver does not panic"),
        "a pause completed in the window: the full wake"
    );
    assert!(flag.load(Ordering::Acquire), "left up for the full wake");
    assert!(!leaving.load(Ordering::Acquire));
    assert_eq!(barrier.blocked_count(), 0);
}

/// gci i20/jt: a pause whose census COUNTED the thread (it read
/// `leaving` up) is met by the leave's arrival -- waiting it out instead
/// would be the pause's missing arrival -- with the flag still up; the
/// leave then takes the full wake.
#[test]
fn gci_i20jt_a_counting_pause_is_met_by_the_lockfree_leaver() {
    let _slot = coverage_slot_tests_serialised();
    let barrier = Arc::new(GcBarrier::with_lockfree_blocked(true));
    let flag = Arc::new(AtomicBool::new(true));
    let leaving = Arc::new(AtomicBool::new(false));
    assert!(!barrier.mark_in_native_enter());
    let gen0 = barrier.gc_generation.load(Ordering::Acquire);
    // The census ran while the leaver's word was up: counted.
    leaving.store(true, Ordering::Release);
    assert!(!gci_i20jt_census_blocked(&leaving, &flag), "leaving is counted");
    assert!(barrier.request_stw_opening_cycle(
        ThreadId(1),
        || census(2, vec![], Some(true)),
        || {}
    ));
    leaving.store(false, Ordering::Release);
    let leaver = {
        let (barrier, flag, leaving) =
            (Arc::clone(&barrier), Arc::clone(&flag), Arc::clone(&leaving));
        std::thread::spawn(move || {
            barrier.leave_in_native_lockfree_if(ThreadId(7), &flag, &leaving, || {
                barrier.gc_generation.load(Ordering::Acquire) == gen0
            })
        })
    };
    assert!(
        barrier.wait_for_all_timeout(Duration::from_secs(10)),
        "the counted leaver arrives"
    );
    assert!(flag.load(Ordering::Acquire), "it arrived with its flag up");
    assert!(!leaver.is_finished(), "and waits for the pause to complete");
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(!leaver.join().expect("the leaver does not panic"));
    assert!(flag.load(Ordering::Acquire), "left up for the full wake");
    assert!(!leaving.load(Ordering::Acquire));
    assert_eq!(barrier.blocked_count(), 0);
}

/// gci i20/jt: the lock-free leave against back-to-back pauses whose
/// census reads `leaving` then the flag. Two invariants, each a violation
/// when broken: (1) a thread a census EXCLUDED keeps its flag up until
/// that pause completes (its root scan selects blocked threads by the
/// flag); (2) a thread whose leave was quiet is not excluded by the pause
/// in progress. No hang, and the anonymous count balances.
#[test]
fn gci_i20jt_lockfree_leave_and_census_agree_under_a_race() {
    let _slot = coverage_slot_tests_serialised();
    const MUTATOR: ThreadId = ThreadId(42);
    let b = Arc::new(GcBarrier::with_lockfree_blocked(true));
    let flag = Arc::new(AtomicBool::new(false));
    let leaving = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let violations = Arc::new(AtomicU64::new(0));
    let quiet_leaves = Arc::new(AtomicU64::new(0));
    let mutator = {
        let (b, flag, leaving, stop, violations, quiet_leaves) = (
            Arc::clone(&b),
            Arc::clone(&flag),
            Arc::clone(&leaving),
            Arc::clone(&stop),
            Arc::clone(&violations),
            Arc::clone(&quiet_leaves),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                // A running mutator's safepoint.
                let _ = b.arrive_and_wait_auto(MUTATOR);
                // The deposit raises the identity flag, then the entry.
                flag.store(true, Ordering::Release);
                if b.mark_in_native_enter() {
                    let _ = b.arrive_and_wait_auto(MUTATOR);
                }
                let gen = b.gc_generation.load(Ordering::Acquire);
                let quiet = b.leave_in_native_lockfree_if(MUTATOR, &flag, &leaving, || {
                    b.gc_generation.load(Ordering::Acquire) == gen
                });
                if quiet {
                    quiet_leaves.fetch_add(1, Ordering::Relaxed);
                    let inner = b.inner.lock();
                    if b.stw_requested.load(Ordering::Acquire)
                        && inner.excluded_blocked.contains(&MUTATOR.0)
                    {
                        violations.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    // The full wake.
                    b.leave_blocked_region_flagged(MUTATOR, &flag);
                }
            }
        })
    };
    for _ in 0..1500u32 {
        let mut excluded = false;
        let won = b.request_stw_opening_cycle(
            ThreadId(1),
            || {
                excluded = gci_i20jt_census_blocked(&leaving, &flag);
                let blocked = if excluded { vec![MUTATOR.0] } else { vec![] };
                census(2, blocked, Some(true))
            },
            || {},
        );
        assert!(won, "the only requester wins");
        let t0 = Instant::now();
        while !b.wait_for_all_timeout(Duration::from_millis(1)) {
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "a counted mutator never arrived"
            );
        }
        for _ in 0..64 {
            if excluded && !flag.load(Ordering::Acquire) {
                violations.fetch_add(1, Ordering::Relaxed);                }
            std::hint::spin_loop();            }
        b.complete_gc(cratonvm_types::PointerMap::default());
    }
    stop.store(true, Ordering::Release);
    mutator.join().expect("mutator thread");
    assert_eq!(violations.load(Ordering::Relaxed), 0);
    assert!(
        quiet_leaves.load(Ordering::Relaxed) > 0,
        "positive control: some leaves were quiet"
    );
    assert_eq!(b.blocked_count(), 0, "every entry was left again");
    assert!(!leaving.load(Ordering::Acquire));
}
