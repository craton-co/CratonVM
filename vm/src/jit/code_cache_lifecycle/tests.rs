use super::*;

/// Dropping one of these is what a test observes instead of an unmap.
struct DropProbe {
    drops: Arc<AtomicU64>,
    /// The simulated in-JIT depth at the instant of the drop, recorded so a
    /// test can assert that nothing was freed while a thread was inside
    /// compiled code.
    depth: Arc<AtomicUsize>,
    freed_while_in_jit: Arc<AtomicU64>,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        if self.depth.load(Ordering::Acquire) != 0 {
            self.freed_while_in_jit.fetch_add(1, Ordering::Relaxed);
        }
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

fn extent(code: u64, reserved: u64) -> BodyExtent {
    BodyExtent {
        entry: 0x1000,
        code_bytes: code,
        reserved_bytes: reserved,
    }
}

// -----------------------------------------------------------------------
// Counter identities
// -----------------------------------------------------------------------

#[test]
fn installed_minus_reclaimed_is_live() {
    let lc = CodeCacheLifecycle::with_simulated_quiescence();
    let m = method_id("java/lang/String", "hashCode", "()I");

    for _ in 0..4 {
        lc.install(m, extent(300, 4096));
    }
    let r = lc.report();
    assert_eq!(r.live_bodies, 4);
    assert_eq!(r.live_bytes, 4 * 4096);
    assert_eq!(r.live_code_bytes, 4 * 300);
    assert_eq!(
        r.reachable_bytes, r.live_bytes,
        "nothing retired yet, so every live byte is reachable",
    );

    // Retire two. They are still MAPPED — retirement is not reclamation.
    lc.retire(m, extent(300, 4096), retire_reason::SUPERSEDED, None);
    lc.retire(m, extent(300, 4096), retire_reason::INVALIDATED, None);
    let r = lc.report();
    assert_eq!(r.live_bytes, 4 * 4096, "a retired body is still mapped");
    assert_eq!(
        (r.raw.deferred_bodies, r.raw.deferred_bytes),
        (2, 2 * 4096),
        "both deferred gauges move with the retirement, not with the sweep",
    );
    assert_eq!(r.reachable_bytes, 2 * 4096);
    assert_eq!(lc.queued_bodies(), 2);

    let out = lc.sweep();
    assert!(out.quiescent);
    assert_eq!(out.reclaimed_bodies, 2);
    assert_eq!(out.reclaimed_bytes, 2 * 4096);

    let r = lc.report();
    assert_eq!(
        r.live_bytes,
        r.raw.installed_bytes - r.raw.reclaimed_bytes,
        "installed - reclaimed == live",
    );
    assert_eq!(r.live_bytes, 2 * 4096);
    assert_eq!(r.live_bodies, 2);
    assert_eq!(r.reachable_bytes, r.live_bytes);
    assert_eq!(r.reclaim_completion, 1.0);
}

#[test]
fn a_retired_body_is_mapped_but_not_reachable_until_it_is_swept() {
    let lc = CodeCacheLifecycle::with_simulated_quiescence();
    let depth = lc.simulated_depth().expect("simulated");
    let m = method_id("C", "m", "()V");

    lc.install(m, extent(500, 8192));
    lc.install(m, extent(500, 8192));
    lc.retire(m, extent(500, 8192), retire_reason::SUPERSEDED, None);

    depth.fetch_add(1, Ordering::SeqCst);
    lc.sweep();
    let r = lc.report();
    assert_eq!(r.live_bytes, 2 * 8192);
    assert_eq!(r.raw.deferred_bytes, 8192);
    assert_eq!(
        r.reachable_bytes, 8192,
        "the retired body is mapped but unreachable; only one body is in service",
    );
    assert_eq!(r.deferred_fraction, 0.5);
    depth.fetch_sub(1, Ordering::SeqCst);
}

// -----------------------------------------------------------------------
// The grace period
// -----------------------------------------------------------------------

#[test]
fn retirement_defers_while_a_thread_is_in_jit_and_proceeds_once_it_leaves() {
    let lc = CodeCacheLifecycle::with_simulated_quiescence();
    let depth = lc.simulated_depth().expect("simulated probe");
    let drops = Arc::new(AtomicU64::new(0));
    let freed_while_in_jit = Arc::new(AtomicU64::new(0));
    let m = method_id("C", "hot", "()V");

    lc.install(m, extent(1_000, 4096));

    // A thread enters compiled code.
    depth.fetch_add(1, Ordering::SeqCst);

    let owner: Option<Box<dyn Send>> = Some(Box::new(DropProbe {
        drops: Arc::clone(&drops),
        depth: Arc::clone(&depth),
        freed_while_in_jit: Arc::clone(&freed_while_in_jit),
    }));
    lc.retire(m, extent(1_000, 4096), retire_reason::INVALIDATED, owner);

    // Sweep repeatedly: every one must RETAIN.
    for expected_age in 1..=3u32 {
        let out = lc.sweep();
        assert!(
            !out.quiescent,
            "a thread is in JIT — sweep must not reclaim"
        );
        assert_eq!(out.reclaimed_bodies, 0);
        assert_eq!(out.deferred_bodies, 1);
        assert_eq!(out.deferred_bytes, 4096);
        // `Some`: the model can always answer. The process sweeper answers
        // `None`, which is what round 10 wave 7 changed and what the
        // `a_process_sweep_does_not_claim_a_deferral_age` test below pins.
        assert_eq!(out.oldest_deferral_sweeps, Some(expected_age));
        assert_eq!(drops.load(Ordering::Relaxed), 0, "freed live code");
    }
    let r = lc.report();
    assert_eq!(r.raw.sweeps, 3);
    assert_eq!(r.raw.sweeps_deferred, 3);
    assert_eq!(r.raw.deferrals, 3);
    assert_eq!(r.raw.max_deferral_sweeps, 3);
    assert_eq!(r.sweep_success_rate, 0.0);
    assert!(
        r.to_string().contains("RETAINED"),
        "the report must NAME the retention, or a leak is invisible: {r}",
    );

    // The thread leaves compiled code.
    depth.fetch_sub(1, Ordering::SeqCst);

    let out = lc.sweep();
    assert!(out.quiescent);
    assert_eq!(out.reclaimed_bodies, 1);
    assert_eq!(out.reclaimed_bytes, 4096);
    assert_eq!(out.deferred_bodies, 0);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(
        freed_while_in_jit.load(Ordering::Relaxed),
        0,
        "no body may be unmapped while a thread is inside compiled code",
    );
    assert_eq!(lc.queued_bodies(), 0);
    assert_eq!(lc.report().raw.deferred_bytes, 0);
}

#[test]
fn n_recompilations_reclaim_every_superseded_version() {
    const N: u32 = 8;
    let lc = CodeCacheLifecycle::with_simulated_quiescence();
    let m = method_id("C", "loop", "(I)I");
    let drops = Arc::new(AtomicU64::new(0));
    let depth = lc.simulated_depth().expect("simulated");
    let freed_while_in_jit = Arc::new(AtomicU64::new(0));

    // v1 installs; each later version supersedes the one before it.
    let first = lc.install(m, extent(200, 4096));
    assert_eq!(first.version, 1);
    assert!(!first.is_recompilation);

    for v in 2..=N {
        let installed = lc.install(m, extent(200, 4096));
        assert_eq!(installed.version, v);
        assert!(installed.is_recompilation);
        let owner: Option<Box<dyn Send>> = Some(Box::new(DropProbe {
            drops: Arc::clone(&drops),
            depth: Arc::clone(&depth),
            freed_while_in_jit: Arc::clone(&freed_while_in_jit),
        }));
        lc.retire(m, extent(200, 4096), retire_reason::SUPERSEDED, owner);
    }

    assert_eq!(lc.versions_of(m), N);
    assert_eq!(lc.queued_bodies(), (N - 1) as usize);

    let out = lc.sweep();
    assert!(out.quiescent);
    assert_eq!(out.reclaimed_bodies, (N - 1) as u64);
    assert_eq!(drops.load(Ordering::Relaxed), (N - 1) as u64);

    let r = lc.report();
    assert_eq!(r.raw.installs, N as u64);
    assert_eq!(r.raw.recompilations, (N - 1) as u64);
    assert_eq!(r.raw.methods_compiled, 1);
    assert_eq!(r.raw.max_versions_for_one_method, N as u64);
    assert_eq!(r.versions_per_method, N as f64);
    assert_eq!(
        r.live_bodies, 1,
        "N compilations of one method must leave exactly one live body",
    );
    assert_eq!(r.live_bytes, 4096);
    assert_eq!(
        r.raw.retirements_by_reason[retire_reason::SUPERSEDED],
        (N - 1) as u64,
    );
    assert!(r.to_string().contains("superseded-by-recompilation"), "{r}");
}

// -----------------------------------------------------------------------
// Fragmentation
// -----------------------------------------------------------------------

#[test]
fn fragmentation_is_computed_from_the_free_extent_shape() {
    // 1000 free bytes, biggest run 600 → 40% of the free space is
    // unreachable to a 600+ byte body.
    assert_eq!(
        external_fragmentation(FreeSpace {
            extents: 3,
            free_bytes: 1_000,
            largest_free_extent: 600,
        }),
        0.4,
    );
    // One contiguous run is not fragmented.
    assert_eq!(
        external_fragmentation(FreeSpace {
            extents: 1,
            free_bytes: 1_000,
            largest_free_extent: 1_000,
        }),
        0.0,
    );
    // A FULL cache is not a fragmented one, and must not divide by zero.
    assert_eq!(external_fragmentation(FreeSpace::default()), 0.0);
    // A nonsensical gauge (largest > total) clamps instead of going
    // negative.
    assert_eq!(
        external_fragmentation(FreeSpace {
            extents: 1,
            free_bytes: 100,
            largest_free_extent: 400,
        }),
        0.0,
    );

    // Internal fragmentation is buffer slack over live bytes.
    let lc = CodeCacheLifecycle::with_simulated_quiescence();
    let m = method_id("C", "m", "()V");
    // 3 KiB emitted into a 4 KiB page → 25% slack.
    lc.install(m, extent(3_072, 4_096));
    lc.record_capacity_bytes(16_384);
    lc.record_free_space(FreeSpace {
        extents: 3,
        free_bytes: 1_000,
        largest_free_extent: 600,
    });
    let r = lc.report();
    assert_eq!(r.internal_fragmentation, 0.25);
    assert_eq!(r.external_fragmentation, 0.4);
    assert_eq!(r.occupancy, 0.25);
    assert_eq!(extent(3_072, 4_096).slack_bytes(), 1_024);

    // Reclaiming the body takes its slack away with it.
    lc.retire(m, extent(3_072, 4_096), retire_reason::CACHE_PRESSURE, None);
    lc.sweep();
    let r = lc.report();
    assert_eq!(r.live_bytes, 0);
    assert_eq!(
        r.internal_fragmentation, 0.0,
        "an empty cache is not fragmented",
    );
}

// -----------------------------------------------------------------------
// Failed allocations
// -----------------------------------------------------------------------

#[test]
fn a_failed_allocation_is_counted_and_changes_nothing_else() {
    let lc = CodeCacheLifecycle::with_simulated_quiescence();
    let m = method_id("C", "big", "()V");
    lc.install(m, extent(1_000, 4_096));
    lc.retire(m, extent(1_000, 4_096), retire_reason::SUPERSEDED, None);
    let before = lc.report();

    lc.record_allocation_failure(1 << 20, alloc_failure::CAP_EXCEEDED);
    lc.record_allocation_failure(1 << 21, alloc_failure::NO_EXTENT_LARGE_ENOUGH);
    lc.record_allocation_failure(64, alloc_failure::OS_REFUSED);

    let after = lc.report();
    assert_eq!(after.raw.failed_allocations, 3);
    assert_eq!(
        after.raw.failed_allocation_bytes,
        (1u64 << 20) + (1u64 << 21) + 64,
    );
    assert_eq!(
        after.raw.failed_allocations_by_reason[alloc_failure::CAP_EXCEEDED],
        1,
    );

    // A refused allocation reserved nothing and withdrew nothing: every
    // occupancy figure and the retirement queue must be untouched.
    assert_eq!(after.raw.installs, before.raw.installs);
    assert_eq!(after.raw.installed_bytes, before.raw.installed_bytes);
    assert_eq!(after.raw.reclaimed_bytes, before.raw.reclaimed_bytes);
    assert_eq!(after.live_bytes, before.live_bytes);
    assert_eq!(after.live_bodies, before.live_bodies);
    assert_eq!(lc.queued_bodies(), 1, "the queue survived the failures");

    // ...and the queue still works afterwards.
    let out = lc.sweep();
    assert!(out.quiescent);
    assert_eq!(out.reclaimed_bodies, 1);
    assert_eq!(lc.report().live_bytes, 0);

    assert_eq!(lc.report().allocation_failure_rate, 3.0 / 4.0);
    assert!(
        lc.report().to_string().contains("code-cache-cap-exceeded"),
        "{}",
        lc.report(),
    );
}

/// The decision behind `alloc_failure::NO_EXTENT_LARGE_ENOUGH`: a bucket
/// nothing produces is NOT a misleading zero, because the breakdown filters
/// zeros out before it looks up a label.
///
/// Round 10 wave 5 filed that reason code as an unreachable instrument and
/// wave 6 decided to KEEP it — reserved, unproduced — specifically on this
/// property. That makes the `.filter(|&(_, n)| n != 0)` in
/// `allocation_failure_breakdown` load-bearing for a documented decision
/// rather than a cosmetic tidy, so it gets a test: drop the filter and the
/// report starts printing `no-free-extent-large-enough 0` on every run,
/// which is exactly the round's recurring defect (`osr_compile_declined`
/// printed at 0 on every run that has ever existed) that this code was
/// adjudicated NOT to be.
#[test]
fn a_zero_bucket_is_never_labelled() {
    let lc = CodeCacheLifecycle::with_simulated_quiescence();

    // Nothing recorded at all: no labels, and in particular no zero rows.
    assert!(
        lc.report().allocation_failure_breakdown().is_empty(),
        "a cache with no failed allocations must produce NO rows, not four \
         zero rows"
    );

    // One real failure. The breakdown must carry that row ALONE — the other
    // three codes, `NO_EXTENT_LARGE_ENOUGH` among them, stay invisible.
    lc.record_allocation_failure(1 << 20, alloc_failure::CAP_EXCEEDED);
    let rows = lc.report().allocation_failure_breakdown();
    assert_eq!(
        rows,
        vec![(alloc_failure::label(alloc_failure::CAP_EXCEEDED), 1)]
    );
    assert!(
        !rows
            .iter()
            .any(|&(label, _)| label
                == alloc_failure::label(alloc_failure::NO_EXTENT_LARGE_ENOUGH)),
        "the reserved, unproduced reason code must not surface as a zero row"
    );

    // And the code is still a real label rather than `unknown`: keeping it
    // means keeping the numbering, which is the cost the decision weighed.
    assert_eq!(
        alloc_failure::label(alloc_failure::NO_EXTENT_LARGE_ENOUGH),
        "no-free-extent-large-enough"
    );
    assert_eq!(alloc_failure::SIZE_ESTIMATE_OVERRUN, 3);
}

// -----------------------------------------------------------------------
// Concurrency
// -----------------------------------------------------------------------

#[test]
fn concurrent_install_and_retire_lose_nothing_and_free_nothing_live() {
    const THREADS: u64 = 4;
    const PER_THREAD: u64 = 200;

    let lc = Arc::new(CodeCacheLifecycle::with_simulated_quiescence());
    let depth = lc.simulated_depth().expect("simulated");
    let drops = Arc::new(AtomicU64::new(0));
    let freed_while_in_jit = Arc::new(AtomicU64::new(0));

    // --- phase 1: a thread is inside compiled code for the whole phase --
    depth.fetch_add(1, Ordering::SeqCst);

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let lc = Arc::clone(&lc);
        let drops = Arc::clone(&drops);
        let depth = Arc::clone(&depth);
        let freed = Arc::clone(&freed_while_in_jit);
        handles.push(std::thread::spawn(move || {
            for i in 0..PER_THREAD {
                let m = method_id("C", "m", "()V").wrapping_add(t * 1_000 + i);
                lc.install(m, extent(100, 1_024));
                let owner: Option<Box<dyn Send>> = Some(Box::new(DropProbe {
                    drops: Arc::clone(&drops),
                    depth: Arc::clone(&depth),
                    freed_while_in_jit: Arc::clone(&freed),
                }));
                lc.retire(m, extent(100, 1_024), retire_reason::INVALIDATED, owner);
            }
        }));
    }
    for _ in 0..2 {
        let lc = Arc::clone(&lc);
        handles.push(std::thread::spawn(move || {
            for _ in 0..PER_THREAD {
                let out = lc.sweep();
                assert_eq!(
                    out.reclaimed_bodies, 0,
                    "a sweep reclaimed while a thread was in JIT",
                );
                std::thread::yield_now();
            }
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }

    let total = THREADS * PER_THREAD;
    assert_eq!(
        drops.load(Ordering::Relaxed),
        0,
        "nothing may be freed while a thread is inside compiled code",
    );
    assert_eq!(lc.queued_bodies() as u64, total);
    assert_eq!(lc.report().raw.deferred_bodies, total);

    // --- the thread leaves; everything becomes reclaimable -------------
    depth.fetch_sub(1, Ordering::SeqCst);
    let out = lc.sweep();
    assert!(out.quiescent);
    assert_eq!(out.reclaimed_bodies, total);
    assert_eq!(
        drops.load(Ordering::Relaxed),
        total,
        "a retirement was lost"
    );
    assert_eq!(freed_while_in_jit.load(Ordering::Relaxed), 0);

    // --- phase 2: install / retire / sweep all racing, no one in JIT ---
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let lc = Arc::clone(&lc);
        let drops = Arc::clone(&drops);
        let depth = Arc::clone(&depth);
        let freed = Arc::clone(&freed_while_in_jit);
        handles.push(std::thread::spawn(move || {
            for i in 0..PER_THREAD {
                let m = method_id("D", "m", "()V").wrapping_add(t * 1_000 + i);
                lc.install(m, extent(100, 1_024));
                let owner: Option<Box<dyn Send>> = Some(Box::new(DropProbe {
                    drops: Arc::clone(&drops),
                    depth: Arc::clone(&depth),
                    freed_while_in_jit: Arc::clone(&freed),
                }));
                lc.retire(m, extent(100, 1_024), retire_reason::SUPERSEDED, owner);
            }
        }));
    }
    for _ in 0..2 {
        let lc = Arc::clone(&lc);
        handles.push(std::thread::spawn(move || {
            for _ in 0..PER_THREAD {
                lc.sweep();
                std::thread::yield_now();
            }
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }
    // Drain whatever the racing sweeps left behind.
    while lc.queued_bodies() != 0 {
        let out = lc.sweep();
        assert!(
            out.quiescent,
            "no thread is in JIT, so a sweep must proceed"
        );
    }

    assert_eq!(
        drops.load(Ordering::Relaxed),
        2 * total,
        "every retired body must be reclaimed exactly once",
    );
    assert_eq!(freed_while_in_jit.load(Ordering::Relaxed), 0);

    let r = lc.report();
    assert_eq!(r.raw.installs, 2 * total);
    assert_eq!(r.raw.retirements, 2 * total);
    assert_eq!(r.raw.reclaimed_bodies, 2 * total);
    assert_eq!(
        r.live_bytes,
        r.raw.installed_bytes - r.raw.reclaimed_bytes,
        "installed - reclaimed == live, under concurrency",
    );
    assert_eq!(r.live_bytes, 0);
    assert_eq!(r.raw.deferred_bytes, 0);
    assert_eq!(r.reclaim_completion, 1.0);
}

// -----------------------------------------------------------------------
// W^X
// -----------------------------------------------------------------------

#[test]
fn no_wx_state_is_both_writable_and_executable() {
    // The structural half of the W^X claim: the enum has no RWX variant,
    // so no transition can produce one.
    for state in [
        WxState::Unmapped,
        WxState::Writable,
        WxState::Executable,
        WxState::Poisoned,
    ] {
        assert!(
            !(state.is_writable() && state.is_executable()),
            "{state:?} is both writable and executable",
        );
    }
    // ...and no legal transition from any state produces one either.
    for state in [
        WxState::Unmapped,
        WxState::Writable,
        WxState::Executable,
        WxState::Poisoned,
    ] {
        for event in [
            WxEvent::Allocate,
            WxEvent::Publish,
            WxEvent::Reopen,
            WxEvent::Release,
            WxEvent::Poison,
        ] {
            if let Ok(next) = wx_step(state, event) {
                assert!(!(next.is_writable() && next.is_executable()));
            }
        }
    }
}

#[test]
fn the_install_protocol_allocates_rw_then_publishes_rx() {
    let mut state = WxState::Unmapped;
    let mut seen_writable = false;
    for &event in INSTALL_WX_PROTOCOL.iter() {
        state = wx_step(state, event).expect("install protocol must be legal");
        seen_writable |= state.is_writable();
    }
    assert!(seen_writable, "code is written before it is published");
    assert_eq!(state, WxState::Executable);
    // Publishing something that was never mapped writable is refused.
    assert_eq!(
        wx_step(WxState::Unmapped, WxEvent::Publish),
        Err(WxViolation {
            state: WxState::Unmapped,
            event: WxEvent::Publish,
        }),
    );
}

#[test]
fn retire_protocol_never_reopens_a_page() {
    // The retirement path must go straight from Executable to Unmapped.
    // Patching a retired body into a trap would need an RX->RW round trip,
    // which is a window where a still-executing thread reads half-written
    // instructions.
    let mut state = WxState::Executable;
    for &event in RETIRE_WX_PROTOCOL.iter() {
        assert_ne!(
            event,
            WxEvent::Reopen,
            "reclamation must not make a published page writable again",
        );
        state = wx_step(state, event).expect("retire protocol must be legal");
        assert!(!state.is_writable());
    }
    assert_eq!(state, WxState::Unmapped);
    assert_eq!(
        wx_replay(WxState::Executable, &RETIRE_WX_PROTOCOL),
        Ok(WxState::Unmapped),
    );
    // The diagnostic poison path is also non-writable.
    assert_eq!(
        wx_step(WxState::Executable, WxEvent::Poison),
        Ok(WxState::Poisoned),
    );
    assert!(!WxState::Poisoned.is_writable());
    assert!(!WxState::Poisoned.is_executable());
    // A full lifecycle replays end to end.
    assert_eq!(
        wx_replay(
            WxState::Unmapped,
            &[WxEvent::Allocate, WxEvent::Publish, WxEvent::Release],
        ),
        Ok(WxState::Unmapped),
    );
    assert!(wx_replay(WxState::Unmapped, &[WxEvent::Release]).is_err());
}

// -----------------------------------------------------------------------
// Install protocol ordering
// -----------------------------------------------------------------------

#[test]
fn the_install_sequence_must_flush_and_publish_in_order() {
    let ordered: Vec<usize> = (0..install_step::COUNT).collect();
    assert_eq!(install_sequence_is_legal(&ordered), Ok(()));

    // Flushing the I-cache AFTER the RW->RX flip races the fetch unit on a
    // non-coherent core.
    let mut late_flush = ordered.clone();
    late_flush.swap(
        install_step::FLUSH_ICACHE,
        install_step::TRANSITION_RW_TO_RX,
    );
    assert_eq!(
        install_sequence_is_legal(&late_flush),
        Err(InstallProtocolError::OutOfOrder {
            step: install_step::FLUSH_ICACHE,
            after: install_step::TRANSITION_RW_TO_RX,
        }),
    );

    // Publishing the entry before its metadata lets a peer enter the body
    // and be stack-walked with no oop map.
    let mut early_entry = ordered.clone();
    early_entry.swap(install_step::PUBLISH_METADATA, install_step::PUBLISH_ENTRY);
    assert_eq!(
        install_sequence_is_legal(&early_entry),
        Err(InstallProtocolError::OutOfOrder {
            step: install_step::PUBLISH_METADATA,
            after: install_step::PUBLISH_ENTRY,
        }),
    );

    // Skipping a step is a protocol error, not a fast path.
    let missing: Vec<usize> = ordered
        .iter()
        .copied()
        .filter(|&s| s != install_step::APPLY_RELOCATIONS)
        .collect();
    assert_eq!(
        install_sequence_is_legal(&missing),
        Err(InstallProtocolError::Missing(
            install_step::APPLY_RELOCATIONS
        )),
    );

    let mut duplicated = ordered.clone();
    duplicated.push(install_step::PUBLISH_ENTRY);
    assert_eq!(
        install_sequence_is_legal(&duplicated),
        Err(InstallProtocolError::Duplicate(install_step::PUBLISH_ENTRY)),
    );

    assert_eq!(
        install_sequence_is_legal(&[install_step::COUNT]),
        Err(InstallProtocolError::Unknown(install_step::COUNT)),
    );
}

// -----------------------------------------------------------------------
// Report hygiene
// -----------------------------------------------------------------------

#[test]
fn every_reason_and_step_has_a_label() {
    for code in 0..retire_reason::COUNT {
        assert_ne!(
            retire_reason::label(code),
            "unknown",
            "retire reason {code} needs a label — the report prints it verbatim",
        );
    }
    assert_eq!(retire_reason::label(retire_reason::COUNT), "unknown");
    for code in 0..alloc_failure::COUNT {
        assert_ne!(alloc_failure::label(code), "unknown");
    }
    assert_eq!(alloc_failure::label(alloc_failure::COUNT), "unknown");
    for step in 0..install_step::COUNT {
        assert_ne!(install_step::label(step), "unknown");
    }
    assert_eq!(install_step::label(install_step::COUNT), "unknown");
}

#[test]
fn every_ratio_is_zero_when_its_denominator_is_zero() {
    let r = CodeCacheLifecycleReport::from_raw(CodeCacheLifecycleRaw::default());
    for v in [
        r.occupancy,
        r.internal_fragmentation,
        r.external_fragmentation,
        r.deferred_fraction,
        r.reclaim_completion,
        r.sweep_success_rate,
        r.recompilation_ratio,
        r.versions_per_method,
        r.allocation_failure_rate,
        r.mean_body_bytes,
    ] {
        assert_eq!(v, 0.0, "a zero denominator must normalize to 0.0, not NaN");
        assert!(v.is_finite());
    }
    let text = r.to_string();
    assert!(text.contains("nothing awaiting reclamation"), "{text}");
    assert!(text.contains("no failed allocations"), "{text}");
    assert!(!text.contains("NaN"), "{text}");
    // `CodeCacheLifecycleRaw::default()` leaves `modelled_sources` at its
    // own `Default`, i.e. every flag false, so this report also exercises
    // the SUPPRESSED rendering of all four groups. That is not incidental
    // to this test's subject: `recompilation_ratio` and
    // `versions_per_method` are the two ratios above whose denominators are
    // the unfed fields, and "normalizes to 0.0 rather than NaN" is exactly
    // the property that let a `0.0000` reach an operator looking like a
    // measurement. The ratios still normalize; they are no longer shown.
    assert!(
        text.contains("peak=n/a"),
        "an unsourced high-water mark must be named, not printed as 0: {text}"
    );
    assert!(
        text.contains("[JIT] code-cache compilation: n/a"),
        "an unsourced compilation census must not print methods=0 \
         versions_per_method=0.0000: {text}"
    );
}

/// An instance report prints the four modelled groups as NUMBERS.
///
/// The other half of the suppression, and the half a careless follow-up
/// would break: `Display` now has an `if` on `modelled_sources` in four
/// places, and a change that inverted any of them would leave the model's
/// own report — the only place these counters are real — printing `n/a` for
/// figures it actually has. `CodeCacheLifecycle::raw` answers
/// `ModelledFieldSources::ALL`, and this is what that buys.
#[test]
fn an_instance_report_prints_the_modelled_groups_it_does_feed() {
    let lc = CodeCacheLifecycle::with_simulated_quiescence();
    let m = method_id("a/B", "c", "()V");
    let first = lc.install(m, extent(100, 4096));
    assert_eq!(first.version, 1);
    let second = lc.install(m, extent(120, 4096));
    assert!(second.is_recompilation, "the second install is a recompile");
    // `SUPERSEDED`'s label is "superseded-by-recompilation" — the string
    // asserted below.
    lc.retire(m, extent(100, 4096), retire_reason::SUPERSEDED, None);

    let raw = lc.raw();
    assert_eq!(
        raw.modelled_sources,
        ModelledFieldSources::ALL,
        "an instance feeds every modelled group"
    );
    let text = lc.report().to_string();
    // None of the four suppression markers: the instance has a producer for
    // every group. Checked marker by marker rather than as a bare
    // `!contains("n/a")`, because two of the `Display`'s lines read
    // process-wide globals (the poison-free warning and the keep-alive
    // census) and may fire here on the strength of what another test in this
    // binary did — so a blanket substring ban would be a pin on text this
    // test does not control.
    for marker in [
        "peak=n/a",
        "age=n/a",
        "retirement by reason: n/a",
        "compilation: n/a",
    ] {
        assert!(
            !text.contains(marker),
            "an instance feeds every group, so {marker:?} must not appear: {text}"
        );
    }
    assert!(text.contains("peak="), "{text}");
    assert!(text.contains("methods=1"), "{text}");
    assert!(text.contains("recompilations=1"), "{text}");
    assert!(
        text.contains("superseded-by-recompilation=1"),
        "the per-reason histogram is fed here: {text}"
    );
}

/// The process sweeper must not claim a deferral age in SWEEPS, and must
/// name the unit of the age it does claim.
///
/// `sweep_if_quiescent` hard-coded `oldest_deferral_sweeps: 0`, and
/// `SweepOutcome`'s `Display` printed it as "(oldest deferred 0 sweeps)" in
/// the NOT-QUIESCENT branch — the one branch whose purpose is to surface a
/// retention that is NOT fresh. Wave 7 made it `None`. Wave 8 gave the
/// sweeper a real age from `cratonvm_jit::jit_oldest_retirement_age()`, in
/// RETIREMENT GENERATIONS, in its own field — so the thing this test pins is
/// now two properties rather than one, and the second is the one a careless
/// follow-up would break:
///
/// 1. `oldest_deferral_sweeps` is STILL `None`. The JIT queue counts
///    withdrawals, not drains per queued owner, so there is still no sweep
///    count to give and pouring generations into that field would be a
///    plausible number in the wrong unit.
/// 2. whatever age IS printed names its unit. A bare number in this clause
///    reads as sweeps, because that is what the clause said for years.
///
/// Asserts on the shape of the answer, not on the queue's contents: another
/// test in this binary may have queued or drained anything at any moment, so
/// `quiescent`, `deferred_*` and the age's VALUE are deliberately not
/// asserted — including whether the age is `Some` at all, since a queue that
/// happens to be empty answers `None` and that is correct.
#[test]
fn a_process_sweep_does_not_claim_a_deferral_age_in_sweeps() {
    let out = sweep_if_quiescent();
    assert_eq!(
        out.oldest_deferral_sweeps, None,
        "the JIT queue exposes no per-BODY sweep count, so the process \
         sweeper must answer None rather than 0 — and must not answer with \
         the generation age either, which is a different unit",
    );
    let text = out.to_string();
    assert!(
        !text.contains("oldest deferred 0 sweeps"),
        "the reading this test exists to forbid: {text}"
    );
    match out.oldest_deferral_generations {
        Some(gens) => {
            assert!(
                text.contains(&format!("oldest deferred {gens} retirement generations")),
                "an age in generations must say so in as many words, or it \
                 reads as the sweep count this clause used to carry: {text}"
            );
        }
        None => assert!(
            text.contains("oldest deferral age n/a"),
            "an empty queue has no age and must say so: {text}"
        ),
    }
}

/// The process report never prints a figure it has no producer for.
///
/// The regression pin for
/// `r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields-20260921-RETIRED-20260922.md`
/// Finding 2, and it is written against the two readings that page calls
/// arithmetically impossible, because those are the ones an operator would
/// act on:
///
/// * a `peak` below the current `live` — a high-water mark that says the
///   cache has never been fuller than right now, i.e. that it is not
///   growing;
/// * a `versions_per_method` below `1.0` beside a positive `installs` — a
///   method with a compiled body has at least one version, so any value
///   under 1 is a broken counter, and `0.0000` is what a zero denominator
///   produces through `ratio`.
///
/// Asserts on absences and on the named suppressions rather than on
/// numbers: this reads process-wide counters that every other test in the
/// binary also moves.
///
/// **Updated in wave 8, which fed three of the four groups.** The subject is
/// unchanged — nothing may be printed without a producer — but the flag
/// vector is no longer `NONE`, so asserting `NONE` would have pinned the
/// absence of the fix rather than the property. It is asserted field by
/// field instead, with the reason for each, which is also what makes the
/// next wave's edit obvious: whichever flag it flips, it changes one line
/// here and the `Display` arm in the same commit.
#[test]
fn the_process_report_suppresses_every_field_it_cannot_source() {
    let raw = code_cache_lifecycle_raw();
    assert!(
        raw.modelled_sources.peak_live_bytes,
        "wave 8 gave the high-water mark a producer \
         (`JitCodeReclamationStats::peak_live_bytes`); if this fails the pull \
         or the `fetch_max` at the publish site is gone",
    );
    assert!(
        raw.modelled_sources.retire_reasons,
        "wave 8 threaded a `retire_reason` through every withdrawal site in \
         `cratonvm_jit`",
    );
    assert!(
        raw.modelled_sources.compilation_census,
        "wave 9 gave the compilation census a producer \
         (`cratonvm_jit::jit_compilation_census`), a per-method publication \
         table filled at `JitCache::put`/`put_osr`. A `false` here means \
         either that the pull is gone or that the census answered `None` \
         because its bounded table filled up — the second is a correct \
         suppression but cannot happen in a test binary, which compiles \
         nothing like 2^17 distinct methods",
    );
    assert!(
        !raw.modelled_sources.deferral_age,
        "STILL suppressed: this flag gates `deferrals` and \
         `max_deferral_sweeps`, both in SWEEPS, and the JIT queue counts \
         withdrawals. The age it can answer is in generations and lives in \
         `oldest_deferral_generations`; flipping this flag to carry it would \
         print generations under the word `sweeps`",
    );
    // `peak >= live` — the identity whose violation is the first symptom
    // `r10-wrappers-...` names, and the one the feed made possible rather
    // than impossible: the JIT's sample is a lower bound, so
    // `code_cache_lifecycle_raw` clamps it with `.max(live)`. This is the pin
    // on that clamp.
    let report = CodeCacheLifecycleReport::from_raw(raw);
    assert!(
        report.raw.peak_live_bytes >= report.live_bytes,
        "a high-water mark below the current value is the reading this whole \
         change exists to stop printing: peak={} live={}",
        report.raw.peak_live_bytes,
        report.live_bytes,
    );

    // The second identity, and the one the census was fed to make true
    // rather than absent: `versions_per_method >= 1.0` whenever anything was
    // installed.
    //
    // Stated as inequalities, not equalities, and that is not slack. This
    // binary runs tests in parallel and every one of them that compiles
    // something moves both counters, so the census and the install count are
    // two reads of a moving process. What `code_cache_lifecycle_raw`
    // guarantees under that race is a DIRECTION — it samples the census
    // first, so the census is the stale side and can only be low — and a
    // direction is exactly what these assertions pin. The exact identity
    // `methods + recompilations == installs` is asserted where it can be:
    // `jit/tests/r10_producers_code_cache_census.rs`, on a quiet process.
    if report.raw.installs > 0 {
        assert!(
            report.raw.methods_compiled > 0,
            "installs without a single distinct method is the zero \
             denominator that produced `versions_per_method=0.0000`: \
             installs={} methods={}",
            report.raw.installs,
            report.raw.methods_compiled,
        );
        assert!(
            report.raw.methods_compiled <= report.raw.installs,
            "every census entry was created by a publication that bumped \
             `installed_bodies`, and the census is sampled FIRST, so it \
             cannot exceed the install count: installs={} methods={}",
            report.raw.installs,
            report.raw.methods_compiled,
        );
        assert!(
            report.raw.methods_compiled + report.raw.recompilations <= report.raw.installs,
            "every publication is either a method's first or a \
             recompilation; the sum may lag installs under a concurrent \
             publish but must never lead it: installs={} methods={} \
             recompilations={}",
            report.raw.installs,
            report.raw.methods_compiled,
            report.raw.recompilations,
        );
        assert!(
            report.versions_per_method >= 1.0,
            "a method with a compiled body has at least one version: {}",
            report.versions_per_method,
        );
        assert!(
            report.raw.max_versions_for_one_method >= 1,
            "something was published, so some method reached version 1",
        );
    }

    let text = code_cache_lifecycle_report().to_string();
    assert!(
        !text.contains("peak=n/a"),
        "the high-water mark has a producer now and must print a figure: {text}"
    );
    assert!(
        !text.contains("[JIT] code-cache compilation: n/a"),
        "the compilation census has a producer now, so the only reason for \
         `n/a` would be a truncated table, which this binary cannot cause: \
         {text}"
    );
    // The exact substrings the old rendering produced. `peak 0` was
    // unconditional; `versions_per_method=0.0000` was what `ratio` gave for
    // `installs / 0`.
    assert!(
        !text.contains("peak 0)"),
        "the first impossible reading is back: {text}"
    );
    assert!(
        !text.contains("versions_per_method=0.0000"),
        "the second impossible reading is back: {text}"
    );
    assert!(!text.contains("NaN"), "{text}");
}

#[test]
fn a_sweep_with_an_empty_queue_is_not_counted_as_deferred() {
    let lc = CodeCacheLifecycle::with_simulated_quiescence();
    let depth = lc.simulated_depth().expect("simulated");
    depth.fetch_add(1, Ordering::SeqCst);
    let out = lc.sweep();
    assert!(
        out.quiescent,
        "an empty queue has nothing to defer, so the sweep is trivially done",
    );
    assert_eq!(lc.report().raw.sweeps_deferred, 0);
    depth.fetch_sub(1, Ordering::SeqCst);
}

// -----------------------------------------------------------------------
// The real quiescence signal
// -----------------------------------------------------------------------

/// The production instance must read `GLOBAL_JIT_DEPTH` and nothing else.
///
/// Uses a locally-constructed lifecycle (whose probe is the *real* signal)
/// rather than the process one, so it neither observes nor disturbs other
/// tests' retirements. Only the deferral half is asserted unconditionally:
/// this thread being in JIT makes `any_thread_in_jit()` true no matter what
/// any other test thread is doing, whereas the reclaim half depends on
/// every other thread also being out.
#[test]
fn the_production_quiescence_signal_is_the_global_jit_depth() {
    use crate::jit::conservative_roots;

    let lc = CodeCacheLifecycle::new();
    assert!(
        lc.simulated_depth().is_none(),
        "CodeCacheLifecycle::new must read the real in-JIT depth",
    );
    let drops = Arc::new(AtomicU64::new(0));
    let never_in_jit = Arc::new(AtomicUsize::new(0));
    let freed_while_in_jit = Arc::new(AtomicU64::new(0));
    let m = method_id("C", "compiled", "()V");
    lc.install(m, extent(256, 4_096));

    // A stack address well above any later scanner SP, so the entry is not
    // pruned as "provably returned" while we hold it.
    let anchor = 0u64;
    let sp = &anchor as *const u64 as usize;
    conservative_roots::push_jit_entry_at(sp);
    assert!(
        conservative_roots::any_thread_in_jit(),
        "this thread is inside compiled code",
    );

    let owner: Option<Box<dyn Send>> = Some(Box::new(DropProbe {
        drops: Arc::clone(&drops),
        depth: Arc::clone(&never_in_jit),
        freed_while_in_jit: Arc::clone(&freed_while_in_jit),
    }));
    lc.retire(m, extent(256, 4_096), retire_reason::DEOPTIMIZED, owner);

    for _ in 0..3 {
        let out = lc.sweep();
        assert!(
            !out.quiescent,
            "GLOBAL_JIT_DEPTH is elevated, so reclamation must be deferred",
        );
        assert_eq!(out.reclaimed_bodies, 0);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }

    conservative_roots::pop_jit_entry();

    // Now only *other* threads can hold the depth up. Retry generously:
    // vm-crate unit tests that push JIT entries are short-lived, so a
    // handful of yields is ample.
    let mut reclaimed = false;
    for _ in 0..10_000 {
        if lc.sweep().reclaimed_bodies == 1 {
            reclaimed = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        reclaimed,
        "once no thread is in JIT the body must be reclaimed; \
         deferred forever means the entry chain is wedged",
    );
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(lc.report().live_bytes, 0);
}

/// The process report reads the JIT's real accounting: publishing a body is
/// an install, flushing it is a withdrawal, and the report's identities hold.
/// This used to drive a model queue nothing in production fed, so the
/// report it checked always read zero. Deltas and identities only — other
/// tests share the process counters.
#[test]
fn the_process_report_reads_the_real_jit_accounting() {
    let before = code_cache_lifecycle_raw();

    let cache = cratonvm_jit::JitCache::new();
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
    buf.emit(&[0xC3]);
    cache.put(
        "lifecycle/Probe".into(),
        "run".into(),
        "()V".into(),
        cratonvm_types::ClassId::new(7),
        cratonvm_jit::CompiledMethod::new(buf),
    );
    let installed = code_cache_lifecycle_raw();
    assert!(
        installed.installs >= before.installs + 1,
        "a publication must count as an install"
    );
    assert!(installed.installed_bytes >= before.installed_bytes + 64);

    assert_eq!(cache.clear_all(), 1);
    let withdrawn = code_cache_lifecycle_raw();
    assert!(
        withdrawn.retirements >= before.retirements + 1,
        "a flushed body must count as a withdrawal"
    );

    let swept = sweep_if_quiescent();
    assert_eq!(
        swept.quiescent,
        swept.deferred_bodies == 0,
        "the sweep reports the real queue it drained"
    );

    let r = code_cache_lifecycle_report();
    assert_eq!(
        r.live_bytes,
        r.raw.installed_bytes.saturating_sub(r.raw.reclaimed_bytes),
        "installed - reclaimed == live",
    );
    assert_eq!(
        r.live_bodies,
        r.raw.installs.saturating_sub(r.raw.reclaimed_bodies),
    );
}

/// The refused-allocation gauges read the JIT's real refusals.
///
/// Round 10 (lane `gauges`,
/// `r10-gcs2-code-cache-alloc-failure-gauges-never-fed-20260921.md`).
/// Before that join, `failed_allocations` could not leave zero on any real
/// run: the only way in was `record_allocation_failure`, and no crate could
/// call it — every site that refuses a code-cache allocation is inside
/// `cratonvm-jit`, which `cratonvm-vm` depends on and not the reverse. So
/// the report printed "no failed allocations" whether or not there had been
/// any, which is the failure mode this test exists to stop coming back.
///
/// Deltas only: other tests in this binary share the process counters, and
/// a compile elsewhere may add a `SIZE_ESTIMATE_OVERRUN` at any moment.
///
/// The forced refusal is a zero-byte executable allocation. Every backing
/// allocator rejects it (`mmap(len=0)` is `EINVAL`, `VirtualAlloc(0)`
/// returns null) and `jit/src/platform.rs` has its own test pinning that,
/// so it is a refusal this test can provoke without exhausting the machine
/// — which is the only other way to reach `OS_REFUSED`.
#[test]
fn the_process_report_reads_the_jits_refused_allocations() {
    let before = code_cache_lifecycle_raw();
    let jit_before = cratonvm_jit::jit_code_alloc_failure_stats();

    assert!(
        cratonvm_jit::platform::alloc_executable(0).is_none(),
        "a zero-byte code allocation must be refused"
    );

    let jit_after = cratonvm_jit::jit_code_alloc_failure_stats();
    assert!(
        jit_after.failures >= jit_before.failures + 1,
        "the JIT must have counted its own refusal"
    );
    assert!(
        jit_after.by_reason[alloc_failure::OS_REFUSED]
            >= jit_before.by_reason[alloc_failure::OS_REFUSED] + 1,
        "and counted it under OS_REFUSED"
    );

    let after = code_cache_lifecycle_raw();
    assert!(
        after.failed_allocations >= before.failed_allocations + 1,
        "the report must carry the JIT's refusal, not a model's zero",
    );
    assert!(
        after.failed_allocations_by_reason[alloc_failure::OS_REFUSED]
            >= before.failed_allocations_by_reason[alloc_failure::OS_REFUSED] + 1,
        "and must carry it in the bucket the JIT named — the histogram is \
         copied positionally, so a mislabelled bucket is the drift the \
         `const _` assertion above guards against",
    );
    assert!(
        !code_cache_lifecycle_report()
            .to_string()
            .contains("no failed allocations"),
        "with a refusal recorded, the report must not claim there were none",
    );
}

/// r9w3 (runtime2 request 4): the arena census feeds the free-space gauge
/// with its real largest extent, so a shattered free list reads as
/// fragmented instead of the structural `0.0` of "largest = total".
#[test]
fn r9w3_arena_census_feeds_a_real_largest_free_extent() {
    let census = cratonvm_jit::platform::JitCodeArenaCensus {
        bytes_free_listed: 8 * 4096,
        bytes_never_bumped: 4 * 4096,
        free_blocks: 8,
        largest_free_extent_bytes: 4 * 4096,
        ..Default::default()
    };
    let free = arena_free_space(&census);
    assert_eq!(free.free_bytes, 12 * 4096);
    assert_eq!(free.largest_free_extent, 4 * 4096);
    assert_eq!(free.extents, 9, "eight free blocks plus one bump tail");
    let frag = external_fragmentation(free);
    assert!((frag - (1.0 - 4.0 / 12.0)).abs() < 1e-9, "got {frag}");

    // Nothing free: no extents, and a nonsensical largest clamps to zero.
    let empty = arena_free_space(&cratonvm_jit::platform::JitCodeArenaCensus {
        largest_free_extent_bytes: 4096,
        ..Default::default()
    });
    assert_eq!(empty, FreeSpace::default());
    assert_eq!(external_fragmentation(empty), 0.0);
}
