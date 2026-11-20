use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

const BOUND: Duration = Duration::from_secs(10);

/// Let the parker reach its wait (the slot goes to waiting under the
/// mutex, right before the mutex is released and the wait begins).
fn wait_until_lockless_waiting(ps: &ParkState) {
    let t0 = std::time::Instant::now();
    while ps.lockless_slot.load(Ordering::Acquire) != LOCKLESS_WAITING && t0.elapsed() < BOUND
    {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(5));
}

fn spawn_lockless_parker(
    ps: &Arc<ParkState>,
    flag: &Arc<AtomicBool>,
    timed: bool,
) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    let (ps2, flag2) = (ps.clone(), flag.clone());
    std::thread::spawn(move || {
        if timed {
            ps2.park_wait_once_lockless(Duration::from_secs(30), &flag2);            } else {
            ps2.park_wait_untimed_lockless(&flag2);            }
        let _ = tx.send(());
    });
    rx
}

#[test]
fn every_unpark_shape_ends_a_lockless_wait() {
    for timed in [true, false] {
        for lock_free in [false, true] {
            for wake_all in [false, true] {
                for after_unlock in [false, true] {
                    let shape = UnparkShape {
                        lock_free,
                        wake_all,
                        after_unlock,
                    };
                    let ps = Arc::new(ParkState::new());
                    if lock_free {
                        ps.enable_lock_free();
                    }
                    let flag = Arc::new(AtomicBool::new(false));
                    let rx = spawn_lockless_parker(&ps, &flag, timed);
                    wait_until_lockless_waiting(&ps);
                    ps.unpark_with(shape);
                    rx.recv_timeout(BOUND).unwrap_or_else(|_| {
                        panic!("timed={timed} {shape:?}: the unpark did not end the wait")
                    });
                    assert!(!ps.take_permit(), "timed={timed} {shape:?}: permit consumed");
                    assert!(
                        !ps.wake_event_left_signalled(),
                        "timed={timed} {shape:?}: no signal left on the event"
                    );
                    assert_eq!(ps.lockless_slot.load(Ordering::Acquire), LOCKLESS_IDLE);
                }
            }
        }
    }
}

#[test]
fn interrupt_wakes_end_a_lockless_wait_and_a_stale_one_does_not() {
    for after_unlock in [false, true] {
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_lockless_parker(&ps, &flag, false);
        wait_until_lockless_waiting(&ps);
        ps.wake_for_interrupt_with(after_unlock);
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "after_unlock={after_unlock}: a stale wake does not end the wait"
        );
        wait_until_lockless_waiting(&ps);
        flag.store(true, Ordering::SeqCst);
        ps.wake_for_interrupt_with(after_unlock);
        rx.recv_timeout(BOUND).expect("woken by the flag");
        assert!(!ps.take_permit(), "after_unlock={after_unlock}: no permit given");
        assert!(!ps.wake_event_left_signalled());
    }
    for timed in [true, false] {
        let ps = Arc::new(ParkState::new());
        let flag = Arc::new(AtomicBool::new(false));
        let rx = spawn_lockless_parker(&ps, &flag, timed);
        wait_until_lockless_waiting(&ps);
        flag.store(true, Ordering::SeqCst);
        ps.interrupt_wake_locked();
        rx.recv_timeout(BOUND).expect("woken by the one-trip interrupt wake");
        assert!(!ps.wake_event_left_signalled(), "timed={timed}: no signal left");
    }
}

/// A permit already set, a raised flag, or a timeout end the wait at once
/// or on time, and leave nothing on the event.
#[test]
fn a_set_permit_a_flag_and_a_timeout_leave_nothing() {
    let ps = ParkState::new();
    let flag = AtomicBool::new(false);
    ps.unpark();
    ps.park_wait_untimed_lockless(&flag);
    assert!(!ps.take_permit());
    ps.unpark();
    ps.park_wait_once_lockless(Duration::from_secs(30), &flag);
    assert!(!ps.take_permit());
    let raised = AtomicBool::new(true);
    ps.park_wait_untimed_lockless(&raised);
    ps.park_wait_once_lockless(Duration::from_secs(30), &raised);
    let t0 = std::time::Instant::now();
    ps.park_wait_once_lockless(Duration::from_millis(2), &flag);
    assert!(t0.elapsed() < BOUND);
    assert!(!ps.wake_event_left_signalled());
    assert_eq!(ps.lockless_slot.load(Ordering::Acquire), LOCKLESS_IDLE);
}

/// Unparks racing short timed waits: a claim the wait did not consume
/// (it ended on its timeout first) is reset, so once both sides stop no
/// signal is left for a later wait (the wave-73 rule).
#[test]
fn unparks_racing_the_timeout_leave_no_signal() {
    let ps = Arc::new(ParkState::new());
    let stop = Arc::new(AtomicBool::new(false));
    let (ps2, stop2) = (ps.clone(), stop.clone());
    let waker = std::thread::spawn(move || {
        while !stop2.load(Ordering::Acquire) {
            ps2.unpark();
            std::hint::spin_loop();
        }
    });
    let flag = AtomicBool::new(false);
    for _ in 0..300 {
        ps.park_wait_once_lockless(Duration::from_millis(1), &flag);
    }
    stop.store(true, Ordering::Release);
    waker.join().expect("the waker");
    assert_eq!(ps.lockless_slot.load(Ordering::Acquire), LOCKLESS_IDLE);
    assert!(!ps.wake_event_left_signalled(), "no signal left behind");
}

/// Interpreter round i6 wave 115, lane L1
/// (`CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT`): a wait announced with
/// `announce_event_waiter` (a process wait's second handle) is signalled
/// by every interrupt wake and by an unpark, keeps the permit an unpark
/// gave, and a claim it did not consume is reset by the withdraw.
#[test]
fn an_announced_event_waiter_is_signalled_and_leaves_nothing() {
    let ps = ParkState::new();
    let Some(ev) = ps.announce_event_waiter() else {
        return; // no event on this host (`CRATONVM_WIN_HIRES_PARK=0`)
    };
    ps.wake_for_interrupt_with(false);
    assert_eq!(win_park::wait_event_coarse(ev, 1_000), win_park::Woke::Event);
    ps.withdraw_event_waiter(true);
    assert!(!ps.wake_event_left_signalled());
    assert!(!ps.take_permit(), "a flag wake gives no permit");

    let _ = ps.announce_event_waiter();
    ps.wake_for_interrupt_with(true);
    assert_eq!(win_park::wait_event_coarse(ev, 1_000), win_park::Woke::Event);
    ps.withdraw_event_waiter(true);

    // Claimed, not consumed (the wait ended on the process handle).
    let _ = ps.announce_event_waiter();
    ps.interrupt_wake_locked();
    ps.withdraw_event_waiter(false);
    assert!(!ps.wake_event_left_signalled(), "the unconsumed claim is reset");
    assert!(ps.take_permit(), "the interrupt's unpark left its permit");

    // An unpark wakes the wait and its permit stays for the next park.
    let _ = ps.announce_event_waiter();
    ps.unpark();
    assert_eq!(win_park::wait_event_coarse(ev, 1_000), win_park::Woke::Event);
    ps.withdraw_event_waiter(true);
    assert!(ps.take_permit(), "the permit is left");

    // Withdrawn: a later wake signals nothing.
    ps.wake_for_interrupt_with(false);
    assert!(!ps.wake_event_left_signalled());
    assert_eq!(ps.lockless_slot.load(Ordering::Acquire), LOCKLESS_IDLE);
}
