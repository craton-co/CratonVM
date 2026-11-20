use super::*;

/// JIT contract — `JvmThread::tlab_offset()` must report the actual
/// byte offset of the `tlab` field. The JIT-emitted inline bump in
/// `jit/src/x64.rs` reads `[thread + tlab_offset + CURSOR_OFFSET]`,
/// so a wrong answer here corrupts the heap.
#[test]
fn tlab_offset_matches_field_address() {
    let t = JvmThread::default();
    let base = &t as *const JvmThread as usize;
    let tlab_addr = &t.tlab as *const cratonvm_gc::Tlab as usize;
    let expected = tlab_addr - base;
    assert_eq!(
        JvmThread::tlab_offset(),
        expected,
        "JvmThread::tlab_offset() out of sync with the actual field offset"
    );
    // Caching: a second call must return the same value (OnceLock).
    assert_eq!(JvmThread::tlab_offset(), expected);
}

#[test]
fn thread_creation() {
    let thread = JvmThread::new(ThreadId(1), "worker-1");
    assert_eq!(thread.thread_id, ThreadId(1));
    assert_eq!(thread.name, "worker-1");
    assert!(thread.frames.is_empty());
    assert!(thread.printed.is_empty());
    assert!(!thread.daemon);
}

#[test]
fn thread_default_is_main() {
    let thread = JvmThread::default();
    assert_eq!(thread.thread_id, ThreadId(0));
    assert_eq!(thread.name, "main");
}

#[test]
fn thread_id_display() {
    assert_eq!(format!("{}", ThreadId(0)), "Thread-0");
    assert_eq!(format!("{}", ThreadId(42)), "Thread-42");
}

#[test]
fn park_state_unpark_before_park() {
    let ps = ParkState::new();
    // Unpark first → next park returns immediately
    ps.unpark();
    ps.park(Some(std::time::Duration::from_millis(100)));
    // If park didn't return immediately, the test would timeout
}

#[test]
fn park_state_park_with_timeout() {
    let ps = ParkState::new();
    let start = std::time::Instant::now();
    ps.park(Some(std::time::Duration::from_millis(50)));
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() >= 40,
        "Park should have blocked for ~50ms, got {}ms",
        elapsed.as_millis()
    );
}

/// A timed park must end near its deadline, not at the next system tick.
///
/// On Windows every condvar/`WaitForSingleObject`-shaped wait is rounded up
/// to the 15.625 ms clock tick, so this 50 ms park measured 62.5 ms — four
/// short cycles then one long one for anything re-arming from an absolute
/// deadline, which is what walked `AutoScalingEventExecutorChooserFactoryTest`
/// past the state it asserts on. `win_park` bounds it with a
/// high-resolution waitable timer instead. The 40 ms floor of
/// `park_state_park_with_timeout` above cannot see any of that; this is the
/// ceiling that can.
///
/// The bound is deliberately loose (a loaded CI box can delay any wakeup)
/// and still an order of magnitude tighter than the 15.6 ms quantum: 62 ms
/// was the OLD median, not an outlier, so a regression fails this on the
/// median run rather than needing an unlucky one. Non-Windows hosts have
/// no such rounding and pass it for free.
#[test]
fn park_state_timed_park_does_not_overshoot_to_the_system_tick() {
    let ps = ParkState::new();
    // One warm-up park: the first one on a thread creates the
    // high-resolution timer, and that is not what is being measured.
    ps.park(Some(std::time::Duration::from_millis(5)));
    let mut best = std::time::Duration::from_secs(1);
    for _ in 0..5 {
        let start = std::time::Instant::now();
        ps.park(Some(std::time::Duration::from_millis(50)));
        best = best.min(start.elapsed());
    }
    assert!(
        best >= std::time::Duration::from_millis(45),
        "a 50 ms park returned after only {best:?}"
    );
    assert!(
        best < std::time::Duration::from_millis(58),
        "a 50 ms park took {best:?} at best -- the deadline is being rounded \
         up to the 15.625 ms system tick (62.5 ms was the pre-fix median)"
    );
}

#[test]
fn park_state_unpark_wakes_parked_thread() {
    let ps = Arc::new(ParkState::new());
    let ps2 = ps.clone();
    let handle = std::thread::spawn(move || {
        // This will block until unparked
        ps2.park(Some(std::time::Duration::from_secs(5)));
    });
    // Give the thread time to park
    std::thread::sleep(std::time::Duration::from_millis(20));
    ps.unpark();
    handle.join().unwrap();
}

/// Interpreter round i3 wave 71, lane L1 (`CRATONVM_PARK_LAZY`): the
/// running phase takes a permit already there, returns on an interrupt,
/// gives up at once when told to stop running, and gives up when its
/// budget runs out -- leaving no permit behind in any of those.
#[test]
fn park_lazily_takes_a_permit_and_gives_up_on_stop_or_budget() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let ps = ParkState::new();
    let not_interrupted = AtomicBool::new(false);
    let budget = std::time::Duration::from_millis(5);
    ps.unpark();
    assert!(
        ps.park_lazily(budget, &not_interrupted, &|| true),
        "permit already set"
    );
    assert!(!ps.take_permit(), "the permit was consumed");

    let interrupted = AtomicBool::new(true);
    assert!(
        ps.park_lazily(budget, &interrupted, &|| true),
        "an interrupt ends a park"
    );

    let t0 = std::time::Instant::now();
    assert!(
        !ps.park_lazily(std::time::Duration::from_secs(5), &not_interrupted, &|| {
            false
        })
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(1),
        "a stop request is not waited out"
    );

    let t0 = std::time::Instant::now();
    assert!(
        !ps.park_lazily(budget, &not_interrupted, &|| true),
        "budget ran out"
    );
    assert!(t0.elapsed() >= budget);
    assert!(!ps.take_permit());

    // A stop request raised mid-phase is seen within a slice or so.
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(5));
            stop.store(true, Ordering::Release);
        });
        let t0 = std::time::Instant::now();
        assert!(
            !ps.park_lazily(std::time::Duration::from_secs(5), &not_interrupted, &|| {
                !stop.load(Ordering::Acquire)
            })
        );
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
    });
}

/// Interpreter round i3 wave 71, lane L1: an `unpark` from another thread
/// ends the running phase well before its budget.
#[test]
fn park_lazily_is_woken_by_an_unpark() {
    let ps = Arc::new(ParkState::new());
    let ps2 = ps.clone();
    let not_interrupted = std::sync::atomic::AtomicBool::new(false);
    let handle = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(10));
        ps2.unpark();
    });
    let t0 = std::time::Instant::now();
    assert!(
        ps.park_lazily(std::time::Duration::from_secs(5), &not_interrupted, &|| {
            true
        })
    );
    assert!(t0.elapsed() < std::time::Duration::from_secs(2));
    handle.join().unwrap();
}

/// Interpreter round i3 wave 72, lane L6 (`CRATONVM_PARK_LAZY_STW_WAKE`):
/// with one wait as long as the whole budget, a stop request followed by
/// `wake_lazy_phase` ends the running phase at once (no slice to wait
/// out), leaves no permit, and a later permit still ends the next phase.
#[test]
fn park_lazily_sliced_is_ended_by_a_stop_and_a_wake() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let ps = Arc::new(ParkState::new());
    let not_interrupted = AtomicBool::new(false);
    let stop = Arc::new(AtomicBool::new(false));
    let long = std::time::Duration::from_secs(30);
    let (ps2, stop2) = (ps.clone(), stop.clone());
    let waker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(10));
        stop2.store(true, Ordering::SeqCst);
        ps2.wake_lazy_phase();
    });
    let t0 = std::time::Instant::now();
    let over = ps.park_lazily_sliced(long, long, &not_interrupted, &|| {
        !stop.load(Ordering::SeqCst)
    });
    assert!(!over, "a stop is not the end of the park");
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(10),
        "the wake was not waited out"
    );
    assert!(!ps.take_permit(), "a wake is not a permit");
    waker.join().unwrap();

    // A stray wake with nothing to stop for does not end a phase.
    ps.wake_lazy_phase();
    let short = std::time::Duration::from_millis(5);
    assert!(!ps.park_lazily_sliced(short, short, &not_interrupted, &|| true));
    ps.unpark();
    assert!(ps.park_lazily_sliced(long, long, &not_interrupted, &|| true));
}

/// Interpreter round i3 wave 73, lane L6 (page
/// `i72-L6-jfr-thread-park-misses-the-lazy-park-phase-20261105`): a
/// park's event span starts before the running phase and keeps the
/// caller's timeout, however much of it the phase used; no recording, no
/// span (no clock read).
#[test]
fn a_park_event_span_covers_the_running_phase_with_the_callers_timeout() {
    use std::sync::atomic::AtomicBool;
    assert!(
        ParkEventSpan::begin(false, None).is_none(),
        "not recording: no span"
    );
    let caller = std::time::Duration::from_millis(30);
    let span = ParkEventSpan::begin(true, Some(caller)).expect("recording");
    // The running phase uses part of the timeout, as `park_lazily` does,
    // and the blocking path then waits out the rest.
    let ps = ParkState::new();
    let not_interrupted = AtomicBool::new(false);
    let phase = std::time::Duration::from_millis(5);
    assert!(!ps.park_lazily(phase, &not_interrupted, &|| true));
    let rest = caller.saturating_sub(span.duration());
    ps.park_interruptible(Some(rest), &not_interrupted);
    assert!(span.duration() >= caller, "the span covers phase and wait");
    assert_eq!(
        span.timeout_field(),
        30_000_000,
        "the caller's timeout, not the rest"
    );
    assert_eq!(span.legacy_timeout_ns(), 30_000_000);
    let untimed = ParkEventSpan::begin(true, None).expect("recording");
    assert_eq!(untimed.timeout_field(), i64::MIN, "HotSpot: Long.MIN_VALUE");
    assert_eq!(untimed.legacy_timeout_ns(), 0);
    assert_eq!(untimed.until_field(), i64::MIN);
    assert_eq!(span.until_field(), i64::MIN, "a relative park has no until");
}

/// Interpreter round i3 wave 79, lane L6 (`CRATONVM_PARK_LAZY_SPIN_NS`):
/// the spin takes a permit that is there or arrives within the budget,
/// ends on a pending interrupt without clearing it, and gives up -- with
/// nothing consumed -- when the budget runs out or `keep_spinning` says
/// stop; on a plain and on a lock-free state alike.
#[test]
fn spin_for_permit_takes_a_permit_and_gives_up_cleanly() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    for lock_free in [false, true] {
        let ps = Arc::new(ParkState::new());
        if lock_free {
            ps.enable_lock_free();
        }
        let not_interrupted = AtomicBool::new(false);
        let budget = std::time::Duration::from_millis(2);
        // No permit: the budget runs out, nothing consumed.
        let t0 = std::time::Instant::now();
        assert!(!ps.spin_for_permit(budget, &not_interrupted, &|| true));
        assert!(t0.elapsed() >= budget);
        // A permit already there: taken at once, and only once.
        ps.unpark();
        assert!(ps.spin_for_permit(budget, &not_interrupted, &|| true));
        assert!(
            !ps.take_permit_lock_free(),
            "lock_free={lock_free}: taken once"
        );
        // A permit that arrives while it spins.
        let waker = {
            let ps = Arc::clone(&ps);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1));
                ps.unpark();
            })
        };
        assert!(ps.spin_for_permit(
            std::time::Duration::from_secs(5),
            &not_interrupted,
            &|| true
        ));
        waker.join().unwrap();
        // A pending interrupt ends it and stays set.
        let interrupted = AtomicBool::new(true);
        assert!(ps.spin_for_permit(budget, &interrupted, &|| true));
        assert!(interrupted.load(std::sync::atomic::Ordering::Relaxed));
        // `keep_spinning` false: gives up long before a long budget.
        let t0 = std::time::Instant::now();
        assert!(!ps.spin_for_permit(
            std::time::Duration::from_secs(5),
            &not_interrupted,
            &|| false
        ));
        assert!(t0.elapsed() < std::time::Duration::from_secs(1));
    }
}

/// Interpreter round i3 wave 79, lane L6 (page
/// `i73-L6-jfr-thread-park-fields-differ-from-hotspot`): an absolute
/// park's event carries `timeout = Long.MIN_VALUE` and `until` = the
/// caller's deadline, as HotSpot's `Unsafe_Park` posts `parkUntil`.
#[test]
fn an_absolute_park_event_span_reports_until_not_timeout() {
    let remaining = std::time::Duration::from_millis(250);
    let span = ParkEventSpan::begin_until(true, Some(remaining), Some(1_700_000_000_123))
        .expect("recording");
    assert_eq!(span.timeout_field(), i64::MIN);
    assert_eq!(span.until_field(), 1_700_000_000_123);
    assert!(ParkEventSpan::begin_until(false, Some(remaining), Some(1)).is_none());
}

/// gci i23/pk (finding 6 of
/// `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): a negative
/// relative park reports the caller's raw `time` as `timeout` (HotSpot's
/// `Unsafe_Park` posts `timeout = time`), not the `Duration`'s 0; the
/// legacy recorder and `until` are unchanged, and an absolute park keeps
/// `timeout = Long.MIN_VALUE` whatever the raw value says.
#[test]
fn gci_i23pk_a_negative_relative_park_reports_the_raw_timeout() {
    let zero = Some(std::time::Duration::ZERO);
    let span = ParkEventSpan::begin(true, zero).expect("recording");
    assert_eq!(span.timeout_field(), 0, "the Duration answer");
    let raw = span.with_raw_timeout(Some(-1));
    assert_eq!(raw.timeout_field(), -1);
    assert_eq!(raw.until_field(), i64::MIN);
    assert_eq!(raw.legacy_timeout_ns(), 0);
    let min = span.with_raw_timeout(Some(i64::MIN));
    assert_eq!(min.timeout_field(), i64::MIN);
    assert_eq!(span.with_raw_timeout(None).timeout_field(), 0);
    let abs = ParkEventSpan::begin_until(true, zero, Some(5))
        .expect("recording")
        .with_raw_timeout(Some(-1));
    assert_eq!(abs.timeout_field(), i64::MIN);
    assert_eq!(abs.until_field(), 5);
}

/// Interpreter round i3 wave 73, lane L6 (`CRATONVM_PARK_ATOMIC_PERMIT`):
/// the lock-free halves keep the permit's one-bit semantics -- a permit
/// set before a park is taken once, a second unpark does not stack, the
/// mutex and lock-free halves see each other's permit, and no waiter
/// stays counted after any kind of wait returns.
#[test]
fn lock_free_permit_halves_keep_the_one_bit_semantics() {
    use std::sync::atomic::AtomicBool;
    let ps = ParkState::new();
    ps.enable_lock_free();
    let not_interrupted = AtomicBool::new(false);
    assert!(!ps.take_permit_lock_free(), "no permit yet");
    ps.unpark_lock_free();
    ps.unpark_lock_free();
    assert!(ps.take_permit_lock_free(), "one permit");
    assert!(!ps.take_permit_lock_free(), "permits do not stack");
    ps.unpark_lock_free();
    assert!(ps.take_permit(), "the mutex half sees a lock-free permit");
    ps.unpark();
    assert!(
        ps.take_permit_lock_free(),
        "the lock-free half sees a mutex permit"
    );
    ps.unpark_lock_free();
    let t0 = std::time::Instant::now();
    ps.park_interruptible(Some(std::time::Duration::from_secs(5)), &not_interrupted);
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "the permit ended the park"
    );
    assert!(!ps.take_permit(), "and was consumed");
    ps.unpark_lock_free();
    assert!(
        ps.park_lazily(std::time::Duration::from_secs(5), &not_interrupted, &|| {
            true
        })
    );
    ps.park_interruptible(Some(std::time::Duration::from_millis(2)), &not_interrupted);
    ps.park(Some(std::time::Duration::from_millis(2)));
    assert_eq!(ps.sleeper_count(), 0, "no waiter left counted");
}

/// A lock-free unpark wakes a thread already blocked in an untimed or a
/// long timed park (it sees the sleeper and notifies under the mutex).
#[test]
fn lock_free_unpark_wakes_a_blocked_parker() {
    use std::sync::atomic::AtomicBool;
    let ps = Arc::new(ParkState::new());
    ps.enable_lock_free();
    for timed in [false, true] {
        let ps2 = ps.clone();
        let parker = std::thread::spawn(move || {
            let not_interrupted = AtomicBool::new(false);
            let t0 = std::time::Instant::now();
            let timeout = timed.then_some(std::time::Duration::from_secs(20));
            ps2.park_interruptible(timeout, &not_interrupted);
            t0.elapsed()
        });
        while ps.sleeper_count() == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        ps.unpark_lock_free();
        let waited = parker.join().unwrap();
        assert!(
            waited < std::time::Duration::from_secs(10),
            "timed={timed}: woken"
        );
    }
    assert_eq!(ps.sleeper_count(), 0);    }

/// Without `enable_lock_free` (the switch off) a wait keeps no sleeper
/// count, and `unpark_lock_free` falls back to the mutex path.
#[test]
fn lock_free_unpark_of_a_plain_state_takes_the_mutex_path() {
    use std::sync::atomic::AtomicBool;
    let ps = Arc::new(ParkState::new());
    let ps2 = ps.clone();
    let parker = std::thread::spawn(move || {
        let not_interrupted = AtomicBool::new(false);
        let t0 = std::time::Instant::now();
        ps2.park_interruptible(None, &not_interrupted);
        t0.elapsed()
    });
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(
        ps.sleeper_count(),
        0,
        "a plain state keeps no sleeper count"
    );
    ps.unpark_lock_free();
    assert!(parker.join().unwrap() < std::time::Duration::from_secs(10));
}

/// Lost-wakeup stress: two threads hand a turn back and forth with
/// lock-free unparks and short timed parks (a park returns on its permit,
/// its timeout or spuriously; the turn variable says whose turn it is).
/// Every round's unpark must wake its parker without waiting out a long
/// timeout: a lost wake-up shows as a round that ends only on the 5 s
/// fallback timeout, counted and asserted zero. Also mixes in 1 us parks,
/// which race the unpark against the waiter's count-then-check.
#[test]
fn lock_free_unpark_loses_no_wakeup_under_stress() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    const ROUNDS: u64 = 20_000;
    let a = Arc::new(ParkState::new());
    let b = Arc::new(ParkState::new());
    a.enable_lock_free();
    b.enable_lock_free();
    let turn = Arc::new(AtomicU64::new(0));
    let lost = Arc::new(AtomicU64::new(0));
    const FALLBACK: std::time::Duration = std::time::Duration::from_secs(5);
    let side = |me: Arc<ParkState>, peer: Arc<ParkState>, parity: u64| {
        let (turn, lost) = (turn.clone(), lost.clone());
        std::thread::spawn(move || {
            let not_interrupted = AtomicBool::new(false);
            for round in 0..ROUNDS {
                let mine = round * 2 + parity;
                loop {
                    if turn.load(Ordering::SeqCst) == mine {
                        break;
                    }
                    if me.take_permit_lock_free() {
                        continue;
                    }
                    let timeout = if round % 3 == 0 {
                        std::time::Duration::from_micros(1)
                    } else {
                        FALLBACK
                    };
                    let t0 = std::time::Instant::now();
                    me.park_interruptible(Some(timeout), &not_interrupted);
                    if timeout == FALLBACK
                        && t0.elapsed() >= FALLBACK
                        && turn.load(Ordering::SeqCst) == mine
                    {
                        lost.fetch_add(1, Ordering::SeqCst);
                    }
                }
                turn.store(mine + 1, Ordering::SeqCst);
                peer.unpark_lock_free();
            }
        })
    };
    let t_a = side(a.clone(), b.clone(), 0);
    let t_b = side(b.clone(), a.clone(), 1);
    t_a.join().unwrap();
    t_b.join().unwrap();
    assert_eq!(
        lost.load(Ordering::SeqCst),
        0,
        "a lock-free unpark was lost"
    );
    assert_eq!(turn.load(Ordering::SeqCst), ROUNDS * 2);
    assert_eq!(a.sleeper_count(), 0);
    assert_eq!(b.sleeper_count(), 0);
}

/// Interpreter round i3 wave 73, lane L6 (page
/// `i72-L6-windows-park-event-left-signalled-by-a-lazy-park-20261105`):
/// a permit taken off the event path -- the fast path, the lazy running
/// phase -- leaves the wake event unsignalled, with either unpark half.
#[cfg(target_os = "windows")]
#[test]
fn a_permit_taken_off_the_event_path_leaves_the_wake_event_unsignalled() {
    use std::sync::atomic::AtomicBool;
    let ps = ParkState::new();
    let not_interrupted = AtomicBool::new(false);
    ps.unpark();
    assert!(ps.take_permit());
    assert!(!ps.wake_event_left_signalled(), "fast path, mutex unpark");
    ps.unpark_lock_free();
    assert!(ps.take_permit_lock_free());
    assert!(
        !ps.wake_event_left_signalled(),
        "fast path, lock-free unpark"
    );
    ps.unpark();
    assert!(ps.park_lazily(
        std::time::Duration::from_millis(50),
        &not_interrupted,
        &|| true
    ));
    assert!(!ps.wake_event_left_signalled(), "lazy phase");
    // A timed park that times out with no unpark leaves nothing either.
    ps.park_interruptible(Some(std::time::Duration::from_millis(2)), &not_interrupted);
    assert!(!ps.wake_event_left_signalled(), "timed out");
}

/// Windows: an unpark during an accurate timed park still wakes it
/// through the event, and leaves the event unsignalled afterwards.
#[cfg(target_os = "windows")]
#[test]
fn an_accurate_park_is_woken_by_the_event_and_leaves_it_clear() {
    use std::sync::atomic::AtomicBool;
    let ps = Arc::new(ParkState::new());
    ps.enable_lock_free();
    let ps2 = ps.clone();
    let parker = std::thread::spawn(move || {
        let not_interrupted = AtomicBool::new(false);
        let t0 = std::time::Instant::now();
        ps2.park_interruptible(Some(std::time::Duration::from_secs(20)), &not_interrupted);
        t0.elapsed()
    });
    while ps.sleeper_count() == 0 {
        std::thread::yield_now();
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
    ps.unpark();
    assert!(parker.join().unwrap() < std::time::Duration::from_secs(10));
    assert!(!ps.wake_event_left_signalled());
}

/// The monitor census counters are where compiled code adds to them (the
/// pinned `JIT_MONITOR_BLOCK_*` offsets), `census_counts` reads those
/// words, a disarm keeps them, and `census_due` fires once per doubling.
#[test]
fn monitor_census_counters_sit_at_the_offsets_compiled_code_bumps() {
    use cratonvm_jit_api as api;
    let mut block = JitMonitorBlock::unarmed();
    let base = &mut block as *mut JitMonitorBlock as *mut u8;
    let bump = |off: usize, by: u64| {
        // SAFETY: `off` is one of the four pinned counter offsets of the
        // `#[repr(C)]` block, each an 8-aligned `AtomicU64`; this is the
        // plain `ADD QWORD [thread + block + off]` the JIT emits.
        unsafe {
            let p = base.add(off) as *mut u64;
            p.write(p.read() + by);
        }
    };
    bump(api::JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET, 1);
    bump(api::JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET, 2);
    bump(api::JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET, 3);
    bump(api::JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET, 4);
    assert_eq!(block.census_counts(), (1, 2, 3, 4));
    block.disarm();
    assert_eq!(
        block.census_counts(),
        (1, 2, 3, 4),
        "disarm keeps the census"
    );
    assert!(!block.is_armed());
    // Total 10 (bit length 4): due once, then not again until 16.
    assert!(block.census_due());
    assert!(!block.census_due());
    let base = &mut block as *mut JitMonitorBlock as *mut u8;
    // SAFETY: as above.
    unsafe {
        let p = base.add(api::JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET) as *mut u64;
        p.write(p.read() + 6);
    }
    assert!(block.census_due(), "16 is a new doubling");
    // Never read the flag here: `Drop` must stay silent for this block.
    assert_eq!(block.census, CENSUS_UNREAD);
}
