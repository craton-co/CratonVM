use super::*;

#[test]
fn join_returns_the_closures_value() {
    let h = spawn_latched(Builder::new(), || 41 + 1).expect("spawn");
    assert_eq!(h.join().expect("no panic"), 42);
}

#[test]
fn join_returns_the_panic_payload() {
    let h =
        spawn_latched(Builder::new(), || -> i32 { panic!("latched worker") }).expect("spawn");
    let payload = h.join().expect_err("the worker unwound");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"latched worker"));
}

#[test]
fn the_captures_are_dropped_before_join_returns() {
    let data = Arc::new(());
    let held = Arc::clone(&data);
    let h = spawn_latched(Builder::new(), move || drop(held)).expect("spawn");
    h.join().expect("no panic");
    assert_eq!(Arc::strong_count(&data), 1);
}

/// The loader-lock case: the worker's OS thread cannot finish exiting
/// because one of ITS thread-local destructors waits for the joiner.
/// `JoinHandle::join` would deadlock; the latched join returns after the
/// grace and detaches the thread, which then exits. Windows only: elsewhere
/// `join` joins (there is no loader lock to model).
#[cfg(windows)]
#[test]
fn a_worker_stuck_in_its_own_exit_is_detached_after_the_grace() {
    use std::time::Instant;
    struct WaitOnDrop(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for WaitOnDrop {
        fn drop(&mut self) {
            let (flag, cv) = &*self.0;
            let mut released = flag.lock();
            while !*released {
                cv.wait(&mut released);
            }
        }
    }
    thread_local! {
        static STUCK: std::cell::RefCell<Option<WaitOnDrop>> =
            const { std::cell::RefCell::new(None) };
    }
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let worker_gate = Arc::clone(&gate);
    let h = spawn_latched(Builder::new(), move || {
        STUCK.with(|s| *s.borrow_mut() = Some(WaitOnDrop(worker_gate)));
        7
    })
    .expect("spawn");
    let started = Instant::now();
    assert_eq!(h.join().expect("no panic"), 7);
    assert!(started.elapsed() >= EXIT_GRACE);
    let (flag, cv) = &*gate;
    *flag.lock() = true;
    cv.notify_all();
}
