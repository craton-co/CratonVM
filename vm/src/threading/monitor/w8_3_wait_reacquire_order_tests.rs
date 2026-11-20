use super::*;

/// Records each [`WaitReacquireReport::reacquired`] call, and whether the
/// waiter owned the monitor at that moment.
struct OwnedAtReacquire {
    monitor: Arc<Monitor>,
    waiter: ThreadId,
    calls: std::sync::atomic::AtomicU32,
    owned: std::sync::atomic::AtomicBool,
}

impl WaitReacquireReport for OwnedAtReacquire {
    fn blocked_on_reenter(&self) {}

    fn reacquired(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.owned.store(
            self.monitor.current_owner() == Some(self.waiter),
            Ordering::SeqCst,
        );
    }
}

fn report(monitor: &Arc<Monitor>, waiter: ThreadId) -> OwnedAtReacquire {
    OwnedAtReacquire {
        monitor: Arc::clone(monitor),
        waiter,
        calls: std::sync::atomic::AtomicU32::new(0),
        owned: std::sync::atomic::AtomicBool::new(false),
    }
}

/// W8-3: every re-acquisition reports `reacquired` exactly once, with the
/// monitor ALREADY owned by the waiter -- a notified waiter whose
/// notifier held the monitor (the contended re-entry) and a timed-out
/// one that found it free (the uncontended re-entry, which reported
/// nothing before W8-3 and so kept its TIMED_WAITING while it owned the
/// monitor again) -- so the VM retires the wait state before the wait
/// returns.
#[test]
fn w8_3_the_wait_state_ends_once_the_monitor_is_owned_again() {
    let m = Arc::new(Monitor::new());
    let (w, n) = (ThreadId(8301), ThreadId(8302));
    let contended = report(&m, w);
    m.enter(w);
    let notifier = {
        let m = Arc::clone(&m);
        std::thread::spawn(move || {
            while m.state.lock().wait_queue.is_empty() {
                std::thread::yield_now();
            }
            m.enter(n);
            m.notify(n).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(m.exit_reporting_release(n).unwrap());
        })
    };
    let outcome = m.wait(w, None, None, None, Some(&contended)).unwrap();
    notifier.join().unwrap();
    assert_eq!(outcome, WaitOutcome::Notified);
    assert_eq!(contended.calls.load(Ordering::SeqCst), 1);
    assert!(
        contended.owned.load(Ordering::SeqCst),
        "owned when reported"
    );
    assert!(m.exit_reporting_release(w).unwrap());

    let free = report(&m, w);
    m.enter(w);
    let outcome = m.wait(w, Some(5), None, None, Some(&free)).unwrap();
    assert_eq!(outcome, WaitOutcome::TimedOutOrSpurious);
    assert_eq!(
        free.calls.load(Ordering::SeqCst),
        1,
        "the free re-entry too"
    );
    assert!(free.owned.load(Ordering::SeqCst), "owned when reported");
    assert!(m.exit_reporting_release(w).unwrap());
}
