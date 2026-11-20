use super::*;

// NOTE: `SELF_CELL` is per-OS-thread and `cells()` is process-global, so
// tests that assert on absolute census numbers spawn their own threads and
// assert on deltas rather than absolutes — the test harness runs these
// concurrently with the rest of the suite.

#[test]
fn every_state_round_trips_through_u8() {
    for state in ThreadExecState::ALL {
        assert_eq!(
            ThreadExecState::from_u8(state.as_u8()),
            Some(state),
            "{state} must survive the u8 encoding"
        );
    }
    assert_eq!(ThreadExecState::from_u8(200), None);
    assert_eq!(ThreadExecState::COUNT, 9);
}

#[test]
fn all_is_in_discriminant_order() {
    for (i, state) in ThreadExecState::ALL.iter().enumerate() {
        assert_eq!(state.as_u8() as usize, i, "ALL must index by discriminant");
    }
}

#[test]
fn every_tabled_transition_is_legal() {
    for (from, to) in TRANSITIONS {
        assert!(
            is_legal(*from, *to),
            "tabled edge {from} -> {to} must be accepted"
        );
    }
}

#[test]
fn self_edges_are_legal_for_every_state() {
    for state in ThreadExecState::ALL {
        assert!(
            is_legal(state, state),
            "{state} -> {state} must be accepted"
        );
    }
}

#[test]
fn table_has_no_duplicate_edges() {
    let mut seen = std::collections::HashSet::new();
    for (from, to) in TRANSITIONS {
        assert!(
            seen.insert((*from, *to)),
            "duplicate edge {from} -> {to} in TRANSITIONS"
        );
    }
}

#[test]
fn representative_illegal_transitions_are_rejected() {
    // `S` is the module-level alias for `ThreadExecState`, glob-imported
    // above.
    // Terminated is NOT terminal, and this loop used to assert that it
    // was -- `!is_legal(S::Terminated, to)`, "a dead thread must not
    // resume as {to}". The cell belongs to an OS thread and the state
    // belongs to a LOGICAL one, which are the same object only for
    // platform threads: a carrier records `Terminated` for every virtual
    // thread that dies on it and then goes on working. One 10 000-vthread
    // `VthreadProbe` run produced 9 986 reports out of `Terminated`, one
    // per virtual thread, against 2 genuine ones. See `is_legal`.
    for to in ThreadExecState::ALL {
        assert!(
            is_legal(S::Terminated, to),
            "a carrier that outlived a virtual thread must be able to record {to}"
        );
    }
    // A thread parked at the barrier cannot die: it must resume first
    // (`arrive_and_wait_inner` returns before any teardown runs).
    assert!(!is_legal(S::SafepointParked, S::Terminated));
    // Nor can a thread in compiled code: the JIT entry chain has to unwind
    // (`pop_jit_entry`) before teardown.
    assert!(!is_legal(S::CompiledUninterruptible, S::Terminated));
    // Deopt materialisation is entered only from compiled code.
    assert!(!is_legal(S::JavaRunning, S::Deoptimizing));
    assert!(!is_legal(S::VmRunning, S::Deoptimizing));
    assert!(!is_legal(S::NativeBlocked, S::Deoptimizing));
    assert!(!is_legal(S::SafepointParked, S::Deoptimizing));
    // A blocked thread cannot enter compiled code without first leaving
    // the blocked region (`check_post_block_gc` must apply its fixup).
    assert!(!is_legal(S::NativeBlocked, S::CompiledUninterruptible));
    // Only the startup path reaches `Starting`, and only from the barrier
    // drain loop or a re-attach.
    assert!(!is_legal(S::JavaRunning, S::Starting));
    assert!(!is_legal(S::NativeRunning, S::Starting));
    assert!(!is_legal(S::NativeBlocked, S::Starting));
    // A not-yet-STW-ready thread runs no natives and no compiled code.
    assert!(!is_legal(S::Starting, S::NativeRunning));
    assert!(!is_legal(S::Starting, S::NativeBlocked));
    assert!(!is_legal(S::Starting, S::CompiledUninterruptible));
    assert!(!is_legal(S::Starting, S::VmRunning));
}

#[test]
fn every_state_is_reachable_and_has_a_successor() {
    for state in ThreadExecState::ALL {
        if state != ThreadExecState::Starting {
            assert!(
                TRANSITIONS.iter().any(|(_, t)| *t == state),
                "{state} is unreachable — no edge leads to it"
            );
        }
        assert!(
            !legal_successors(state).is_empty(),
            "{state} is a sink — it has no outgoing edge"
        );
    }
}

#[test]
fn quota_and_relocation_rules_agree_with_the_audit() {
    // Excluded from `expected`, exactly as
    // `alive_count_blocked_and_os_tids` computes it.
    assert!(!S::NativeBlocked.counts_toward_safepoint_quota());
    assert!(!S::Starting.counts_toward_safepoint_quota());
    assert!(!S::Terminated.counts_toward_safepoint_quota());
    // A *running* native is deliberately waited for (gc_barrier.rs:44-47).
    assert!(S::NativeRunning.counts_toward_safepoint_quota());
    // Counted at census time; repaired afterwards by `reduce_expected`.
    assert!(S::CompiledUninterruptible.counts_toward_safepoint_quota());

    // Rewritable holders.
    assert_eq!(
        S::SafepointParked.relocation_rule(),
        RelocationRule::PermittedWithRewrite
    );
    assert_eq!(
        S::NativeBlocked.relocation_rule(),
        RelocationRule::PermittedWithRewrite
    );
    // Unrewritable holders.
    assert_eq!(
        S::CompiledUninterruptible.relocation_rule(),
        RelocationRule::Forbidden
    );
    assert_eq!(
        S::NativeRunning.relocation_rule(),
        RelocationRule::Forbidden
    );
    assert_eq!(
        S::Terminated.relocation_rule(),
        RelocationRule::PermittedNoRefsHeld
    );

    // Every state that may hold unrewritable refs must forbid relocation —
    // the two properties are not independent, and a future edit that makes
    // them disagree is a bug.
    for state in ThreadExecState::ALL {
        if state.may_hold_unrewritable_object_refs() {
            assert_eq!(
                state.relocation_rule(),
                RelocationRule::Forbidden,
                "{state} may hold unrewritable refs but permits relocation"
            );
        }
    }
}

#[test]
fn recording_updates_the_thread_local_state() {
    std::thread::spawn(|| {
        bind_current_thread(4242);
        // First observation seeds, so it is never a violation.
        assert!(try_record_transition(ThreadExecState::JavaRunning, "test::seed").is_ok());
        assert_eq!(current_state(), ThreadExecState::JavaRunning);

        assert!(
            try_record_transition(ThreadExecState::NativeBlocked, "test::block").is_ok(),
            "JavaRunning -> NativeBlocked is a tabled edge"
        );
        assert_eq!(current_state(), ThreadExecState::NativeBlocked);

        assert!(try_record_transition(ThreadExecState::JavaRunning, "test::unblock").is_ok());
        assert!(try_record_transition(ThreadExecState::Terminated, "test::die").is_ok());
        assert_eq!(current_state(), ThreadExecState::Terminated);
    })
    .join()
    .expect("recorder test thread must not panic");
}

/// The span must be indistinguishable from the pair it replaced, in both
/// directions — during the call AND after it.
///
/// The `NativeRunning` half is not decoration: that is the state the STW
/// census deliberately WAITS for, because a running native holds raw
/// `ObjectRef`s in Rust locals that a copying collector must not relocate
/// under it. A span that skipped the store would leave every native call
/// looking like ordinary Java execution to the collector, and nothing in a
/// timing would show it.
#[test]
fn a_native_state_span_matches_the_pair_it_replaces() {
    std::thread::spawn(|| {
        bind_current_thread(9101);
        let _ = try_record_transition(ThreadExecState::JavaRunning, "test::seed");

        let span = enter_native_state("test::enter");
        assert_eq!(
            current_state(),
            ThreadExecState::NativeRunning,
            "the span must record NativeRunning for the duration of the call"
        );
        assert_eq!(span.prior(), ThreadExecState::JavaRunning);
        span.restore("test::return");
        assert_eq!(
            current_state(),
            ThreadExecState::JavaRunning,
            "and must put the caller's state back"
        );

        // Nested, the shape a re-entrant native takes: the inner span
        // restores `NativeRunning` (a legal self-edge), not `JavaRunning`.
        let outer = enter_native_state("test::outer");
        let inner = enter_native_state("test::inner");
        assert_eq!(inner.prior(), ThreadExecState::NativeRunning);
        inner.restore("test::inner-return");
        assert_eq!(current_state(), ThreadExecState::NativeRunning);
        outer.restore("test::outer-return");
        assert_eq!(current_state(), ThreadExecState::JavaRunning);
    })
    .join()
    .expect("span test thread must not panic");
}

/// A thread the recorder has never observed reads `Starting`, and
/// `Starting -> NativeRunning` is deliberately absent from the table. The
/// span must resume such a thread as `JavaRunning` — the state it
/// demonstrably reached — or it would assert an edge the code cannot take,
/// once per native call, for the life of that thread.
#[test]
fn jvmti_exception_callback_native_round_trips_deoptimizing_state() {
    assert!(is_legal(S::Deoptimizing, S::NativeRunning));
    assert!(is_legal(S::Deoptimizing, S::CompiledUninterruptible));
    assert!(is_legal(S::NativeRunning, S::Deoptimizing));
    std::thread::spawn(|| {
        bind_current_thread(9103);
        assert!(try_record_transition(S::JavaRunning, "test::java").is_ok());
        assert!(try_record_transition(S::CompiledUninterruptible, "test::compiled").is_ok());
        assert!(try_record_transition(S::Deoptimizing, "test::deoptimizing").is_ok());
        let span = enter_native_state("jvmti::exception_callback");
        assert_eq!(span.prior(), S::Deoptimizing);
        assert_eq!(current_state(), S::NativeRunning);
        span.restore("jvmti::exception_callback:return");
        assert_eq!(current_state(), S::Deoptimizing);
    })
    .join()
    .expect("JVMTI callback state test thread must not panic");
}

#[test]
fn a_span_on_an_unobserved_thread_resumes_as_java_running() {
    std::thread::spawn(|| {
        bind_current_thread(9102);
        assert_eq!(current_state(), ThreadExecState::Starting);
        let span = enter_native_state("test::first-ever");
        assert_eq!(span.prior(), ThreadExecState::JavaRunning);
        span.restore("test::return");
        assert_eq!(current_state(), ThreadExecState::JavaRunning);
    })
    .join()
    .expect("first-observation span test thread must not panic");
}

#[test]
fn an_illegal_transition_is_reported_and_still_recorded() {
    std::thread::spawn(|| {
        bind_current_thread(777);
        let _ = try_record_transition(ThreadExecState::JavaRunning, "test::seed");
        let err = try_record_transition(ThreadExecState::Deoptimizing, "test::bad");
        if stress_checks_enabled() {
            let err = err.expect_err("JavaRunning -> Deoptimizing must be rejected");
            assert_eq!(err.from, ThreadExecState::JavaRunning);
            assert_eq!(err.to, ThreadExecState::Deoptimizing);
            assert_eq!(err.thread_id, 777);
            assert_eq!(err.site, "test::bad");
            let rendered = err.to_string();
            assert!(rendered.contains("JavaRunning"), "{rendered}");
            assert!(rendered.contains("Deoptimizing"), "{rendered}");
            assert!(rendered.contains("777"), "{rendered}");
            assert!(rendered.contains("test::bad"), "{rendered}");
        }
        // Recorded either way: the shadow record must keep tracking
        // reality so later checks stay meaningful.
        assert_eq!(current_state(), ThreadExecState::Deoptimizing);
    })
    .join()
    .expect("violation test thread must not panic");
}

#[test]
fn census_counts_live_threads_per_state() {
    use std::sync::mpsc;

    let before = thread_state_census();

    let (parked_tx, parked_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        bind_current_thread(1001);
        let _ = try_record_transition(ThreadExecState::JavaRunning, "test::census-seed");
        let _ = try_record_transition(ThreadExecState::NativeBlocked, "test::census-block");
        parked_tx.send(()).expect("handshake send");
        release_rx.recv().expect("handshake recv");
        let _ = try_record_transition(ThreadExecState::JavaRunning, "test::census-wake");
        let _ = try_record_transition(ThreadExecState::Terminated, "test::census-die");
    });

    parked_rx.recv().expect("worker must reach NativeBlocked");
    let during = thread_state_census();
    // `>= 1`, not `> before`: the shadow registry is process-global and
    // peers in the same test binary move in and out of `NativeBlocked`
    // between the two walks. The worker is definitively parked here, so a
    // non-zero population is the race-free statement.
    assert!(
        during.get(ThreadExecState::NativeBlocked) >= 1,
        "the blocked worker must appear in the census: before={before:?} during={during:?}"
    );
    // A blocked thread is excluded from the quota, so the blocked worker
    // must not have inflated the candidate count on its own account.
    assert!(during.live_total() >= during.safepoint_quota_candidates());

    release_tx.send(()).expect("release send");
    worker.join().expect("census worker must not panic");

    let after = thread_state_census();
    // Cumulative and monotonic: our worker contributed one and peers can
    // only add more, so `>` is race-free.
    assert!(
        after.terminated_total > before.terminated_total,
        "terminating must bump the cumulative terminated count"
    );
    // This one is NOT. The live registry is process-global and this counts
    // every thread in the binary, not just ours: a peer test's worker
    // sitting in `Terminated` for the instant before its cell is reaped
    // made it read `left: 1, right: 0` in 1 of 24 full-suite runs.
    //
    // Retry for a quiet moment instead. Our own worker has been `join`ed
    // by now, so if OURS were the cell that failed to leave, the count
    // could never return to 0 and this still fails — which is exactly the
    // property under test.
    const ATTEMPTS: usize = 256;
    let mut observed = after.get(ThreadExecState::Terminated);
    for _ in 0..ATTEMPTS {
        if observed == 0 {
            break;
        }
        std::thread::yield_now();
        observed = thread_state_census().get(ThreadExecState::Terminated);
    }
    assert_eq!(
        observed, 0,
        "terminated cells leave the live registry — still populated after \
         {ATTEMPTS} attempts"
    );
}

#[test]
fn census_relocation_blockers_track_unrewritable_states() {
    use std::sync::mpsc;

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        bind_current_thread(1002);
        let _ = try_record_transition(ThreadExecState::JavaRunning, "test::reloc-seed");
        let _ =
            try_record_transition(ThreadExecState::CompiledUninterruptible, "test::reloc-jit");
        ready_tx.send(()).expect("handshake send");
        release_rx.recv().expect("handshake recv");
        let _ = try_record_transition(ThreadExecState::JavaRunning, "test::reloc-leave");
        let _ = try_record_transition(ThreadExecState::Terminated, "test::reloc-die");
    });

    ready_rx.recv().expect("worker must reach compiled code");
    let during = thread_state_census();
    assert!(
        during.relocation_blockers() >= 1,
        "a thread in compiled code must be visible as a relocation blocker: {during:?}"
    );
    assert!(
        during.get(ThreadExecState::CompiledUninterruptible) >= 1,
        "…and specifically as CompiledUninterruptible: {during:?}"
    );

    // The PEER form is the one an obligation check can key on: the raw
    // count above includes the collecting thread (`JavaRunning` /
    // `VmRunning` are `Forbidden` too), so it is never zero and
    // discriminates nothing. Subtracting a `VmRunning` initiator must
    // still leave the compiled peer visible.
    assert!(
        during.peer_relocation_blockers(ThreadExecState::VmRunning) >= 1,
        "the compiled peer must survive subtracting a VmRunning initiator: {during:?}"
    );

    release_tx.send(()).expect("release send");
    worker.join().expect("relocation worker must not panic");
}

#[test]
fn peer_relocation_blockers_exclude_merely_running_threads() {
    // A census holding ONLY running mutators states no obligation: a
    // running thread is parked by the safepoint through a path that
    // rewrites it (`SafepointParked` is `PermittedWithRewrite`). What the
    // obligation is about is a peer whose refs live where no pointer map
    // consumer reaches, which is `may_hold_unrewritable_object_refs`.
    let mut census = ThreadStateCensus {
        per_state: [0; ThreadExecState::COUNT],
        terminated_total: 0,
    };
    census.per_state[ThreadExecState::JavaRunning.as_u8() as usize] = 3;
    census.per_state[ThreadExecState::SafepointParked.as_u8() as usize] = 2;
    census.per_state[ThreadExecState::NativeBlocked.as_u8() as usize] = 4;
    // `JavaRunning` is `Forbidden`, so the raw count is non-zero…
    assert_eq!(census.relocation_blockers(), 3);
    // …while the peer form reads zero: no state here can hold a ref the
    // collection cannot rewrite.
    assert_eq!(
        census.peer_relocation_blockers(ThreadExecState::VmRunning),
        0
    );

    // One peer uninterruptibly inside compiled code IS the obligation, and
    // a `VmRunning` initiator must not be subtracted from it — the census
    // records no thread in that state, so there is nothing of the
    // initiator's to take away.
    census.per_state[ThreadExecState::CompiledUninterruptible.as_u8() as usize] = 1;
    assert_eq!(
        census.peer_relocation_blockers(ThreadExecState::VmRunning),
        1
    );
    // …but a `VmRunning` initiator IS subtracted from the `VmRunning`
    // population once that population exists, which is what makes a zero
    // reachable at all.
    census.per_state[ThreadExecState::VmRunning.as_u8() as usize] = 1;
    assert_eq!(
        census.peer_relocation_blockers(ThreadExecState::VmRunning),
        1
    );
    // And with the compiled peer gone, the lone initiator cancels itself.
    census.per_state[ThreadExecState::CompiledUninterruptible.as_u8() as usize] = 0;
    assert_eq!(
        census.peer_relocation_blockers(ThreadExecState::VmRunning),
        0
    );
}

#[test]
fn concurrent_recorders_do_not_lose_or_duplicate_cells() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 200;

    let before = thread_state_census();
    let barrier = Arc::new(std::sync::Barrier::new(THREADS + 1));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                bind_current_thread(2000 + i as u64);
                let _ = try_record_transition(ThreadExecState::JavaRunning, "test::conc-seed");
                // Drive a legal cycle repeatedly; every edge below is in
                // TRANSITIONS, so an armed tripwire must stay silent.
                for _ in 0..ROUNDS {
                    let _ =
                        try_record_transition(ThreadExecState::NativeRunning, "test::conc-nat");
                    let _ = try_record_transition(
                        ThreadExecState::NativeBlocked,
                        "test::conc-block",
                    );
                    let _ = try_record_transition(
                        ThreadExecState::SafepointParked,
                        "test::conc-park",
                    );
                    let _ = try_record_transition(
                        ThreadExecState::NativeBlocked,
                        "test::conc-reblock",
                    );
                    let _ =
                        try_record_transition(ThreadExecState::JavaRunning, "test::conc-run");
                }
                // Settle in a distinctive state and hold it while the
                // census runs.
                let _ = try_record_transition(ThreadExecState::VmRunning, "test::conc-settle");
                barrier.wait();
                barrier.wait();
                let _ = try_record_transition(ThreadExecState::Terminated, "test::conc-die");
            })
        })
        .collect();

    barrier.wait();
    let during = thread_state_census();
    assert_eq!(
        during.get(ThreadExecState::VmRunning),
        before.get(ThreadExecState::VmRunning) + THREADS as u64,
        "every concurrent recorder must contribute exactly one cell: \
         before={before:?} during={during:?}"
    );
    barrier.wait();

    for h in handles {
        h.join().expect("concurrent recorder must not panic");
    }

    let after = thread_state_census();
    // `>=`, not `==`: the shadow registry is process-global and the rest of
    // the crate's test binary retires threads concurrently with this test.
    assert!(
        after.terminated_total >= before.terminated_total + THREADS as u64,
        "every recorder must retire exactly once: \
         before={before:?} after={after:?}"
    );
    assert_eq!(
        after.get(ThreadExecState::VmRunning),
        before.get(ThreadExecState::VmRunning),
        "retired cells must leave the live registry"
    );
}

#[test]
fn concurrent_recorders_report_no_illegal_transitions() {
    // "Legal traffic stays silent" — the other half of the tripwire
    // contract. Asserted on each worker's own return values rather than on
    // the process-global counter, which the rest of the test binary
    // shares.
    const THREADS: usize = 4;
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            std::thread::spawn(move || -> Result<(), IllegalTransition> {
                bind_current_thread(3000 + i as u64);
                try_record_transition(ThreadExecState::JavaRunning, "test::clean-seed")?;
                for _ in 0..100 {
                    try_record_transition(
                        ThreadExecState::SafepointParked,
                        "test::clean-park",
                    )?;
                    try_record_transition(ThreadExecState::JavaRunning, "test::clean-run")?;
                    try_record_transition(
                        ThreadExecState::CompiledUninterruptible,
                        "test::clean-jit",
                    )?;
                    try_record_transition(ThreadExecState::Deoptimizing, "test::clean-deopt")?;
                    try_record_transition(ThreadExecState::JavaRunning, "test::clean-resume")?;
                }
                try_record_transition(ThreadExecState::Terminated, "test::clean-die")?;
                Ok(())
            })
        })
        .collect();
    for h in handles {
        h.join()
            .expect("clean recorder must not panic")
            .expect("tabled traffic must not register a violation");
    }
}
