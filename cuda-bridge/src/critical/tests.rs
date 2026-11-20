use super::*;
use std::sync::atomic::AtomicUsize;

// ── Device-layer mock ───────────────────────────────────────────────
//
// No GPU, no driver, not even `cuda_bridge::Stream`: the token
// lifetime contract is a host-side property, and stubbing the device
// as an enum makes "the kernel was cancelled" and "the device
// faulted" as reachable in a unit test as "it succeeded". This is the
// whole reason the registry does not depend on `DeviceContext`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FakeDevice {
    /// Kernel completes and the writeback drains.
    Succeeds,
    /// `event.synchronize()` fails — the device-error path.
    Faults,
    /// The host cancels before the kernel is observed complete.
    Cancelled,
    /// The host thread panics mid-submission.
    Panics,
    /// The submission is dispatched and then abandoned: the token is
    /// deliberately leaked, exactly as a `FinalizeState` that nobody
    /// ever drains leaks its `GcCriticalGuard`.
    NeverCompletes,
}

/// Run one fake submission under a token. Returns the token id on a
/// completed writeback, `Err` on device error or cancel. Panics for
/// `FakeDevice::Panics` (the caller catches it).
fn run_submission(
    reg: &Arc<Registry>,
    device: FakeDevice,
    owner: OwnerId,
    lease: Duration,
) -> Result<u64, String> {
    let token = reg.acquire_with(
        owner,
        lease,
        Relocation::KeepAliveOnly,
        &[0xdead_0000, 0xbeef_0000],
    );
    let id = token.id();
    match device {
        FakeDevice::Succeeds => {
            assert!(!token.is_revoked(), "a live token must not be revoked");
            Ok(id)
        }
        FakeDevice::Faults => Err("event.synchronize: fake device fault".to_string()),
        FakeDevice::Cancelled => Err("cancelled".to_string()),
        FakeDevice::Panics => panic!("fake host-thread panic mid-submission"),
        FakeDevice::NeverCompletes => {
            // The leak: the token escapes its scope without ever being
            // released. `mem::forget` is the closest a test can get to
            // "an `Arc<StreamSubmission>` nobody drops".
            std::mem::forget(token);
            Ok(id)
        }
    }
}

fn owner(site: &'static str) -> OwnerId {
    OwnerId::current(1, Some(7), site)
}

// ── 1. Released on every exit path ──────────────────────────────────

#[test]
fn token_is_released_on_success_error_and_cancel() {
    for device in [
        FakeDevice::Succeeds,
        FakeDevice::Faults,
        FakeDevice::Cancelled,
    ] {
        let reg = Registry::new();
        let _ = run_submission(&reg, device, owner("unit"), Duration::from_secs(3_600));
        assert_eq!(
            reg.outstanding(),
            0,
            "{device:?} must leave no outstanding token",
        );
        let raw = reg.metrics_raw();
        assert_eq!(raw.tokens_acquired, 1);
        assert_eq!(
            raw.tokens_released, 1,
            "{device:?} must count as a VOLUNTARY release, not a reap",
        );
        assert_eq!(raw.tokens_reaped, 0);
    }
}

#[test]
fn token_is_released_when_the_holder_panics() {
    let reg = Registry::new();
    let r = Arc::clone(&reg);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = run_submission(
            &r,
            FakeDevice::Panics,
            owner("panic-path"),
            Duration::from_secs(3_600),
        );
    }));
    assert!(result.is_err(), "the fake submission must have panicked");
    assert_eq!(
        reg.outstanding(),
        0,
        "unwinding through the frame must run the token's Drop",
    );
    assert_eq!(reg.metrics_raw().tokens_released, 1);
}

#[test]
fn token_is_released_at_vm_shutdown_and_the_holder_learns_it() {
    let reg = Registry::new();
    // Two VMs, one token each — the exact multi-`SharedVm` situation
    // `offload.rs`'s REAPER_QUEUE comment says real-hardware
    // validation caught.
    let t1 = reg.acquire(OwnerId::current(1, Some(11), "vm1-dispatch"));
    let t2 = reg.acquire(OwnerId::current(2, Some(22), "vm2-dispatch"));
    assert_eq!(reg.outstanding(), 2);

    let reaped = reg.shutdown_vm(1);
    assert_eq!(reaped.len(), 1, "only VM 1's token may be reaped");
    assert_eq!(reaped[0].owner.vm, 1);
    assert_eq!(reaped[0].cause, Some(ReleaseCause::VmShutdown));
    assert_eq!(
        reg.outstanding(),
        1,
        "a concurrently-live second VM keeps its token",
    );
    assert!(
        t1.is_revoked(),
        "the shut-down VM's holder must observe revocation before it writes back",
    );
    assert!(!t2.is_revoked(), "the live VM's holder is untouched");

    // Dropping the already-reaped token is idempotent and must not
    // decrement anything a second time.
    drop(t1);
    assert_eq!(reg.outstanding(), 1);
    assert_eq!(reg.metrics_raw().double_release_suppressed, 1);
    drop(t2);
    assert_eq!(reg.outstanding(), 0);
}

// ── 2. The wait is bounded ──────────────────────────────────────────

#[test]
fn a_stalled_submission_produces_a_bounded_wait_not_an_unbounded_spin() {
    let reg = Registry::new();
    // A lease far longer than the wait budget: the token is stalled,
    // NOT orphaned, so the registry must not reap it — the wait has to
    // terminate on its own budget instead.
    let _token = reg.acquire_with(
        owner("stalled-kernel"),
        Duration::from_secs(3_600),
        Relocation::KeepAliveOnly,
        &[0x1000, 0x2000],
    );

    let budget = Duration::from_millis(20);
    let started = Instant::now();
    let outcome = reg.wait_for_drain(budget);
    let elapsed = started.elapsed();

    assert!(
        !outcome.drained(),
        "a stalled token must not report drained"
    );
    // AUDIT 2026-09-02: a keep-alive-only holder reads its addresses
    // back after the move (`CriticalToken::keepalive_addrs`), so its
    // timeout no longer vetoes relocation. Only a `Forbidden` holder
    // does — pinned by `a_forbidden_holder_vetoes_relocation_on_timeout`.
    assert!(
        outcome.may_relocate(),
        "a keep-alive-only timeout must not veto relocation: the holder remaps",
    );
    assert!(
        elapsed >= budget,
        "the wait must actually wait its budget, not return early",
    );
    assert!(
        elapsed < budget * 50,
        "the wait must be BOUNDED: {elapsed:?} for a {budget:?} budget",
    );

    match outcome {
        WaitOutcome::TimedOut {
            outstanding,
            relocation_forbidden,
            keepalive_addrs,
            holders,
            ..
        } => {
            assert_eq!(outstanding, 1);
            assert!(!relocation_forbidden, "this token declared keep-alive only");
            assert_eq!(
                keepalive_addrs,
                vec![0x1000, 0x2000],
                "the fail-safe cycle needs the roots the submission depends on",
            );
            assert_eq!(holders.len(), 1);
            assert_eq!(holders[0].owner.site, "stalled-kernel");
        }
        other => panic!("expected TimedOut, got {other:?}"),
    }

    let raw = reg.metrics_raw();
    assert_eq!(raw.waits_entered, 1);
    assert_eq!(raw.waits_timed_out, 1);
    assert_eq!(raw.waits_drained, 0);
    assert_eq!(
        raw.tokens_reaped, 0,
        "an in-lease token must NOT be force-released to make the wait succeed",
    );
}

#[test]
fn a_drained_wait_reports_drained_and_permits_relocation() {
    let reg = Registry::new();
    {
        let _t = reg.acquire(owner("short"));
    }
    let outcome = reg.wait_for_drain(Duration::from_millis(50));
    assert!(outcome.drained());
    assert!(outcome.may_relocate());
    assert!(outcome.keepalive_addrs().is_empty());
    let raw = reg.metrics_raw();
    assert_eq!(raw.waits_drained, 1);
    assert_eq!(raw.waits_timed_out, 0);
}

#[test]
fn a_forbidden_relocation_token_is_reported_as_such_on_timeout() {
    let reg = Registry::new();
    let _t = reg.acquire_with(
        owner("zero-copy-kernel"),
        Duration::from_secs(3_600),
        Relocation::Forbidden,
        &[0xabc0],
    );
    match reg.wait_for_drain(Duration::from_millis(5)) {
        WaitOutcome::TimedOut {
            relocation_forbidden,
            ..
        } => assert!(
            relocation_forbidden,
            "a token holding a device-visible heap address must forbid relocation",
        ),
        other => panic!("expected TimedOut, got {other:?}"),
    }
    assert!(reg.relocation_forbidden());
}

// ── 3. Orphans are reaped and named ─────────────────────────────────

#[test]
fn an_orphaned_token_is_reaped_and_names_its_holder() {
    let reg = Registry::new();
    // A submission that never completes, with a one-millisecond lease
    // standing in for the production default.
    let leaked = reg.acquire_with(
        OwnerId::current(42, Some(9_001), "abandoned-dispatch"),
        Duration::from_millis(1),
        Relocation::KeepAliveOnly,
        &[0xfeed],
    );
    let revoked_flag = Arc::clone(&leaked.revoked);
    std::mem::forget(leaked);
    assert_eq!(reg.outstanding(), 1);

    // A wait that would spin forever under the old contract instead
    // drains, because the orphan is reaped inside the loop.
    std::thread::sleep(Duration::from_millis(5));
    let outcome = reg.wait_for_drain(Duration::from_millis(500));
    assert!(
        outcome.drained(),
        "reaping the orphan must let the wait drain: {outcome}",
    );
    assert_eq!(reg.outstanding(), 0);
    assert!(
        revoked_flag.load(Ordering::Acquire),
        "a reaped token must be revoked so its writeback is suppressed",
    );

    let raw = reg.metrics_raw();
    assert_eq!(raw.tokens_reaped, 1);
    assert_eq!(raw.tokens_released, 0, "a reap is not a voluntary release");
    assert!(raw.longest_held_nanos > 0);

    // The leak NAMES its holder rather than manifesting as a stall.
    let reaped = reg.reaped_tokens();
    assert_eq!(reaped.len(), 1);
    assert_eq!(reaped[0].owner.vm, 42);
    assert_eq!(reaped[0].owner.submission, Some(9_001));
    assert_eq!(reaped[0].cause, Some(ReleaseCause::Reaped));

    let text = reg.report();
    assert!(text.contains("abandoned-dispatch"), "{text}");
    assert!(text.contains("submission=9001"), "{text}");
    assert!(text.contains("reaped-lease-expired"), "{text}");
    assert!(
        text.contains("EVERY reap is a repaired leak"),
        "the report must not present a reap as routine: {text}",
    );
}

/// The same leak, driven end-to-end through the mocked device layer
/// rather than by a hand-forgotten token: a submission that is
/// dispatched and then never completes must not be able to stall the
/// collector for the rest of the process.
#[test]
fn a_submission_that_never_completes_is_bounded_by_its_lease() {
    let reg = Registry::new();
    let id = run_submission(
        &reg,
        FakeDevice::NeverCompletes,
        OwnerId::current(5, Some(404), "never-completes"),
        Duration::from_millis(1),
    )
    .expect("dispatch itself succeeds; it is the completion that never arrives");

    assert!(
        reg.facts(id).is_some(),
        "the token is live right after dispatch"
    );
    assert_eq!(reg.outstanding(), 1);

    std::thread::sleep(Duration::from_millis(5));
    let outcome = reg.wait_for_drain(Duration::from_millis(500));
    assert!(outcome.drained(), "{outcome}");
    assert!(
        reg.facts(id).is_none(),
        "the abandoned token must be gone from the registry",
    );
    let reaped = reg.reaped_tokens();
    assert!(
        reaped
            .iter()
            .any(|f| f.id == id && f.owner.submission == Some(404)),
        "the reap log must name the abandoned submission: {reaped:?}",
    );
}

#[test]
fn the_reap_log_is_bounded() {
    let reg = Registry::new();
    for i in 0..(REAP_LOG_CAPACITY * 2) {
        let t = reg.acquire_with(
            OwnerId::current(1, Some(i as u64), "leaky"),
            Duration::ZERO,
            Relocation::KeepAliveOnly,
            &[],
        );
        std::mem::forget(t);
        reg.reap_expired();
    }
    assert_eq!(reg.reaped_tokens().len(), REAP_LOG_CAPACITY);
    assert_eq!(
        reg.metrics_raw().tokens_reaped,
        (REAP_LOG_CAPACITY * 2) as u64
    );
}

// ── 4. Concurrency ──────────────────────────────────────────────────

#[test]
fn concurrent_acquire_release_neither_loses_nor_double_releases() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 200;

    let reg = Registry::new();
    let mirror: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
    assert!(reg.bind_mirror(mirror));

    let observed_nonzero = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let r = Arc::clone(&reg);
        let seen = Arc::clone(&observed_nonzero);
        handles.push(std::thread::spawn(move || {
            for i in 0..PER_THREAD {
                let t = r.acquire_with(
                    OwnerId::current(1, Some(i as u64), "churn"),
                    Duration::from_secs(3_600),
                    Relocation::KeepAliveOnly,
                    &[i],
                );
                if r.outstanding() > 0 {
                    seen.fetch_add(1, Ordering::Relaxed);
                }
                // Alternate the two release paths so both are exercised
                // under contention.
                if i % 2 == 0 {
                    t.release();
                } else {
                    drop(t);
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }

    let total = (THREADS * PER_THREAD) as u64;
    let raw = reg.metrics_raw();
    assert_eq!(raw.tokens_acquired, total);
    assert_eq!(raw.tokens_released, total, "no release was lost");
    assert_eq!(
        raw.double_release_suppressed, 0,
        "no token was released twice",
    );
    assert_eq!(reg.outstanding(), 0, "the gauge returned to zero");
    assert_eq!(
        mirror.load(Ordering::Acquire),
        0,
        "the mirrored legacy counter must also return to zero, or the old \
         spin loops stay wedged",
    );
    assert!(
        observed_nonzero.load(Ordering::Relaxed) > 0,
        "the test never actually held a token concurrently",
    );
}

#[test]
fn the_mirror_tracks_reaps_as_well_as_releases() {
    // The failure this pins: reaping a token in THIS registry while
    // the legacy `GPU_CRITICAL_COUNT` keeps its increment would leave
    // `wait_for_gpu_critical_drain` spinning forever on a token that
    // no longer exists.
    let reg = Registry::new();
    let mirror: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
    assert!(reg.bind_mirror(mirror));
    assert!(!reg.bind_mirror(mirror), "binding twice must be refused");

    let t = reg.acquire_with(
        owner("leaky"),
        Duration::ZERO,
        Relocation::KeepAliveOnly,
        &[],
    );
    std::mem::forget(t);
    assert_eq!(mirror.load(Ordering::Acquire), 1);
    assert_eq!(reg.reap_expired().len(), 1);
    assert_eq!(
        mirror.load(Ordering::Acquire),
        0,
        "a reap must decrement the mirrored counter too",
    );
}

// ── 5. GC-latency attribution ───────────────────────────────────────

/// Only a `Forbidden` holder vetoes relocation on expiry; a
/// keep-alive-only one is remapped instead. See
/// `WaitOutcome::may_relocate`.
#[test]
fn a_forbidden_holder_vetoes_relocation_on_timeout() {
    let reg = Registry::new();
    let _dma = reg.acquire_with(
        owner("zero-copy-upload"),
        Duration::from_secs(3_600),
        Relocation::Forbidden,
        &[],
    );
    let outcome = reg.wait_for_drain(Duration::from_millis(2));
    assert!(!outcome.drained());
    assert!(
        !outcome.may_relocate(),
        "a DMA against the arena forbids the move"
    );
    // The narrower wait the collector actually uses agrees.
    let clearance = reg.wait_for_relocation_clearance(Duration::from_millis(2));
    assert!(!clearance.may_relocate());
    assert_eq!(reg.outstanding_relocation_forbidden(), 1);
}

/// The collector waits only for relocation-forbidding holders: a
/// keep-alive token outstanding for the life of a kernel must not
/// cost it the budget on every cycle.
#[test]
fn a_keep_alive_holder_does_not_delay_relocation_clearance() {
    let reg = Registry::new();
    let _kernel = reg.acquire_with(
        owner("in-flight-kernel"),
        Duration::from_secs(3_600),
        Relocation::KeepAliveOnly,
        &[0x40],
    );
    let started = Instant::now();
    let outcome = reg.wait_for_relocation_clearance(Duration::from_millis(500));
    assert!(outcome.drained(), "nothing forbids relocation");
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "the clearance wait must return at once when no holder forbids relocation"
    );
    // ...and the keep-alive roots are still available to splice.
    assert_eq!(reg.outstanding_keepalive_addrs(), vec![0x40]);
}

/// A relocation-forbidding acquisition waits out a declared moving
/// cycle — the gate that keeps an unstopped thread from starting a
/// DMA against memory the collector is sliding.
#[test]
fn a_forbidden_acquisition_waits_for_the_moving_cycle_to_end() {
    let reg = Registry::new();
    reg.begin_moving_cycle();
    let reg2 = reg.clone();
    // The acquirer announces itself BEFORE it blocks, and this thread waits
    // for that announcement before starting its 60ms clock. Without the
    // handshake the test is a wall-clock race it can lose on a loaded
    // machine: if the spawned thread is not scheduled until after
    // `end_moving_cycle()` below, it acquires immediately, reports a
    // `waited` near zero, and the `>= 50ms` assertion fails while nothing
    // is actually wrong. Observed 2026-09-17 in a workspace run with
    // several cargo jobs competing; passes 3/3 in isolation on the same
    // commit.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let acquirer = std::thread::spawn(move || {
        let started = Instant::now();
        // Sent from inside the thread and immediately before the blocking
        // call, so "the acquirer is running" is a fact and not a hope.
        let _ = tx.send(());
        let _t = reg2.acquire_with(
            owner("writeback"),
            Duration::from_secs(1),
            Relocation::Forbidden,
            &[],
        );
        started.elapsed()
    });
    rx.recv().expect("the acquirer thread starts");
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(
        reg.outstanding(),
        0,
        "the acquisition must not complete while the cycle is declared"
    );
    reg.end_moving_cycle();
    let waited = acquirer.join().unwrap();
    assert!(waited >= Duration::from_millis(50), "waited {waited:?}");
    // A keep-alive-only acquisition is never gated.
    reg.begin_moving_cycle();
    let _k = reg.acquire_with(
        owner("keep-alive"),
        Duration::from_secs(1),
        Relocation::KeepAliveOnly,
        &[],
    );
    reg.end_moving_cycle();
}

#[test]
fn forced_non_moving_collections_are_attributed_to_gpu_waiting() {
    let reg = Registry::new();
    // A DMA window that outlasts the budget: the shape that forces a
    // non-moving cycle. (A keep-alive-only holder would not — see
    // `a_keep_alive_holder_does_not_delay_relocation_clearance`.)
    let _t = reg.acquire_with(
        owner("long-writeback-dma"),
        Duration::from_secs(3_600),
        Relocation::Forbidden,
        &[0x40, 0x80],
    );

    // Two collection attempts; only the ones that actually time out
    // and divert are attributed.
    let mut diverted = 0;
    for _ in 0..2 {
        let outcome = reg.wait_for_drain(Duration::from_millis(2));
        if !outcome.may_relocate() {
            // This is the collector's documented expiry behaviour.
            let roots = outcome.keepalive_addrs().to_vec();
            assert!(
                !roots.is_empty(),
                "the fail-safe cycle needs its extra roots"
            );
            reg.record_forced_non_moving_collection();
            diverted += 1;
        }
    }
    assert_eq!(diverted, 2);

    let report = reg.metrics_report();
    assert_eq!(report.raw.forced_non_moving_collections, 2);
    assert_eq!(report.raw.waits_entered, 2);
    assert_eq!(report.raw.waits_timed_out, 2);
    assert_eq!(report.wait_timeout_rate, 1.0);
    assert_eq!(report.forced_non_moving_per_wait, 1.0);

    let text = report.to_string();
    assert!(text.contains("forced_non_moving_collections=2"), "{text}");
    assert!(
        text.contains("because a GPU critical-section wait expired"),
        "the report must attribute the diversion, not merely count it: {text}",
    );
    assert!(text.contains("timed_out=2"), "{text}");
}

#[test]
fn a_wait_that_never_ran_reports_no_cost_rather_than_nan() {
    let report = CriticalSectionReport::from_raw(CriticalMetricsRaw::default());
    for v in [
        report.wait_timeout_rate,
        report.wait_nanos_per_wait,
        report.leak_rate,
        report.forced_non_moving_per_wait,
    ] {
        assert_eq!(v, 0.0, "a zero denominator must normalize to 0.0, not NaN");
        assert!(v.is_finite());
    }
    let text = report.to_string();
    assert!(text.contains("outstanding=0"), "{text}");
    assert!(
        text.contains("critical leaks: none"),
        "a clean run must say so explicitly rather than printing nothing: {text}",
    );
}

// ── Keep-alive addresses / remap ────────────────────────────────────

#[test]
fn keepalive_addresses_are_deduped_sorted_and_remappable() {
    let reg = Registry::new();
    let _a = reg.acquire_with(
        owner("a"),
        Duration::from_secs(60),
        Relocation::KeepAliveOnly,
        &[0x300, 0x100],
    );
    let _b = reg.acquire_with(
        owner("b"),
        Duration::from_secs(60),
        Relocation::KeepAliveOnly,
        &[0x100, 0x200],
    );
    assert_eq!(
        reg.outstanding_keepalive_addrs(),
        vec![0x100, 0x200, 0x300],
        "the collector's extra-root buffer must be deterministic",
    );

    // A moving cycle relocated 0x100 -> 0x900 and left the rest.
    reg.remap_keepalive(|a| if a == 0x100 { Some(0x900) } else { None });
    assert_eq!(
        reg.outstanding_keepalive_addrs(),
        vec![0x200, 0x300, 0x900],
        "keep-alive addresses are address-keyed and MUST be remapped after a \
         moving cycle, like any address-keyed side table",
    );
}

// ── Labels ──────────────────────────────────────────────────────────

#[test]
fn every_release_cause_and_relocation_policy_has_a_distinct_label() {
    let causes = [
        ReleaseCause::Released,
        ReleaseCause::Dropped,
        ReleaseCause::Reaped,
        ReleaseCause::VmShutdown,
    ];
    let mut seen: Vec<&str> = Vec::new();
    for c in causes {
        assert!(!c.label().is_empty());
        assert!(
            !seen.contains(&c.label()),
            "duplicate label {:?}",
            c.label()
        );
        seen.push(c.label());
    }
    assert!(ReleaseCause::Released.is_voluntary());
    assert!(ReleaseCause::Dropped.is_voluntary());
    assert!(
        !ReleaseCause::Reaped.is_voluntary(),
        "a reap must never be summed into the voluntary-release total",
    );
    assert!(!ReleaseCause::VmShutdown.is_voluntary());

    assert_ne!(
        Relocation::KeepAliveOnly.label(),
        Relocation::Forbidden.label(),
        "the two pin vocabularies must never print identically",
    );
}

#[test]
fn owner_display_names_every_field_a_stall_needs() {
    let o = OwnerId {
        vm: 3,
        thread: 5,
        thread_name: Some("gpu-worker".to_string()),
        submission: Some(77),
        site: "dispatch_method_from_native",
    };
    let text = o.to_string();
    assert!(text.contains("vm=3"), "{text}");
    assert!(text.contains("thread=5(gpu-worker)"), "{text}");
    assert!(text.contains("submission=77"), "{text}");
    assert!(text.contains("site=dispatch_method_from_native"), "{text}");

    let anon = OwnerId {
        submission: None,
        thread_name: None,
        ..o
    };
    assert!(anon.to_string().contains("submission=none"));
}

#[test]
fn try_begin_moving_cycle_is_atomic_with_forbidden_acquire() {
    let reg = Registry::new();
    let tok = reg.acquire_with(
        owner("pin"),
        Duration::from_secs(60),
        Relocation::Forbidden,
        &[],
    );
    assert!(
        !reg.try_begin_moving_cycle(),
        "try_begin_moving_cycle must refuse while a Forbidden token is live"
    );
    drop(tok);
    assert!(
        reg.try_begin_moving_cycle(),
        "try_begin_moving_cycle must succeed once no Forbidden token is live"
    );
    let reg_bg = Arc::clone(&reg);
    let acquired = Arc::new(AtomicBool::new(false));
    let acquired_bg = Arc::clone(&acquired);
    let jh = std::thread::spawn(move || {
        let blocked = reg_bg.acquire_with(
            owner("during-gc"),
            Duration::from_secs(60),
            Relocation::Forbidden,
            &[],
        );
        acquired_bg.store(true, Ordering::Release);
        drop(blocked);
    });
    std::thread::sleep(Duration::from_millis(5));
    assert!(
        !acquired.load(Ordering::Acquire) && !reg.relocation_forbidden(),
        "Forbidden request arriving after try_begin_moving_cycle must block until end_moving_cycle"
    );
    reg.end_moving_cycle();
    jh.join().expect("join blocked thread");
    assert!(acquired.load(Ordering::Acquire));
}

#[test]
fn wait_for_relocation_clearance_reports_drained_on_zero_timeout_when_clear() {
    let reg = Registry::new();
    let outcome = reg.wait_for_relocation_clearance(Duration::ZERO);
    assert!(
        matches!(outcome, WaitOutcome::Drained { .. }),
        "expected Drained, got {outcome:?}"
    );
}
