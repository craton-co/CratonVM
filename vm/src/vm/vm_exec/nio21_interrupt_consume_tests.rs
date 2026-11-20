use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A set flag is consumed once; a clear flag takes the lock-free path; the
/// consume holds the flag's stripe (a raise under the same lock waits).
#[test]
fn nio21_consume_clears_once_and_excludes_a_raise() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let flag = Arc::new(AtomicBool::new(true));
    assert!(consume_thread_interrupt(&shared, &flag, None));
    assert!(!flag.load(Ordering::Acquire));
    assert!(
        !consume_thread_interrupt(&shared, &flag, None),
        "consumed once"
    );
    // Many raise/consume rounds from two threads: every raise is either
    // consumed or still pending at the end, never half of each.
    let raiser = {
        let shared = Arc::clone(&shared);
        let flag = Arc::clone(&flag);
        std::thread::spawn(move || {
            for _ in 0..10_000 {
                let _g = shared.threads.thread_registry.lock_interrupt(&flag);
                flag.store(true, Ordering::SeqCst);
            }
        })
    };
    let mut consumed = 0u32;
    for _ in 0..10_000 {
        if consume_thread_interrupt(&shared, &flag, None) {
            consumed += 1;
        }
    }
    raiser.join().expect("raiser");
    let pending = consume_thread_interrupt(&shared, &flag, None);
    assert!(consumed + u32::from(pending) >= 1);
    assert!(!flag.load(Ordering::Acquire));
}
