use super::*;

#[test]
fn counts_and_sums_across_threads() {
    static C: StripedCounter = StripedCounter::new();
    C.reset();
    assert!(C.is_zero());

    let threads: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..1000 {
                    C.inc();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("worker finishes");
    }
    assert_eq!(C.get(), 8000);
    assert!(!C.is_zero());
    C.reset();
    assert_eq!(C.get(), 0);
}

#[test]
fn decrement_saturates_at_zero_per_stripe() {
    static C: StripedCounter = StripedCounter::new();
    C.reset();
    C.dec();
    C.dec();
    assert_eq!(C.get(), 0, "an unbalanced decrement must not wrap");
    C.inc();
    assert_eq!(C.get(), 1, "and must not owe against a later increment");
    C.dec();
    assert!(C.is_zero());
    C.reset();
}

/// A decrement that runs after its thread's stripe thread-local is gone
/// must undo its own increment, not stripe 0's.
///
/// The teardown fallback is `stripe_index() -> 0`, so a decrement that
/// re-derived its stripe would hit stripe 0 and zero a live peer. The token
/// carries the increment's stripe instead.
#[test]
fn a_token_decrement_lands_on_its_increments_stripe() {
    static C: StripedCounter = StripedCounter::new();
    C.reset();
    // A live peer parked on stripe 0, and an exited thread whose increment
    // landed on stripe 5.
    C.stripes[0].0.store(1, Ordering::Release);
    C.stripes[5].0.store(1, Ordering::Release);
    C.dec_token(StripeToken(5));
    assert_eq!(
        C.stripes[0].0.load(Ordering::Acquire),
        1,
        "the live peer's count on stripe 0 must survive"
    );
    assert_eq!(C.stripes[5].0.load(Ordering::Acquire), 0);
    C.dec_token(StripeToken::UNSET);
    assert_eq!(C.get(), 1, "an unset token is a no-op");
    C.reset();

    // The same pairing across a real thread exit: the increment is made on
    // the worker's stripe, the decrement on this thread after the worker's
    // thread-locals are destroyed.
    let token = std::thread::spawn(|| C.inc_token())
        .join()
        .expect("worker finishes");
    assert_eq!(C.get(), 1);
    C.dec_token(token);
    assert!(
        C.is_zero(),
        "the exited thread's stripe must be back at zero"
    );
    C.reset();
}

#[test]
fn one_threads_decrement_cannot_cancel_anothers_entry() {
    static C: StripedCounter = StripedCounter::new();
    C.reset();
    C.inc();
    std::thread::spawn(|| {
        // A stray unbalanced leave on a different stripe.
        C.dec();
    })
    .join()
    .expect("worker finishes");
    assert_eq!(C.get(), 1, "the live entry survives a peer's stray leave");
    C.dec();
    C.reset();
}
