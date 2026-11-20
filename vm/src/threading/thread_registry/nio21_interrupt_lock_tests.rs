use super::*;

/// One `Arc` reaches one stripe from both of its holders, and the kill
/// switch hands out no guard.
#[test]
fn nio21_interrupt_lock_is_keyed_by_the_shared_flag() {
    let mut registry = ThreadRegistry::new();
    let flag = Arc::new(AtomicBool::new(false));
    let other_holder = flag.clone();
    assert_eq!(
        interrupt_lock_stripe(&flag),
        interrupt_lock_stripe(&other_holder)
    );
    registry.set_interrupt_lock_enabled_for_test(true);
    {
        let held = registry.lock_interrupt(&flag);
        assert!(held.is_some());
        let stripe = interrupt_lock_stripe(&other_holder);
        assert!(
            registry.interrupt_locks[stripe].try_lock().is_none(),
            "the other holder of the same flag is excluded"
        );
    }
    assert!(
        registry.lock_interrupt(&other_holder).is_some(),
        "released on drop"
    );
    registry.set_interrupt_lock_enabled_for_test(false);
    assert!(registry.lock_interrupt(&flag).is_none(), "=0 takes no lock");
}

/// The stripes spread: 256 distinct flags land in more than a handful of
/// stripes (a hash on the constant low bits would put them all in one).
#[test]
fn nio21_interrupt_lock_stripes_spread() {
    let flags: Vec<Arc<AtomicBool>> =
        (0..256).map(|_| Arc::new(AtomicBool::new(false))).collect();
    let mut seen = [false; INTERRUPT_LOCK_STRIPES];
    for f in &flags {
        seen[interrupt_lock_stripe(f)] = true;
    }
    assert!(seen.iter().filter(|s| **s).count() > 16);
}
