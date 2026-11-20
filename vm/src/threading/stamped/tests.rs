use super::*;

/// Smoke test that the re-exports actually resolve to the
/// native-builtins backend and that a fresh init returns the
/// canonical origin stamp.
#[test]
fn shim_resolves_to_native_builtins_backend() {
    // Use a bogus address far from any real allocation. The state
    // map keys on it just like the natives do; subsequent test
    // runs in the same process won't collide because we use a
    // fresh address each call.
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0xDEAD_0000);
    let a = N.fetch_add(8, std::sync::atomic::Ordering::SeqCst);

    stamped_init(a);
    assert_eq!(stamped_try_optimistic_read(a), STAMPED_ORIGIN);

    // Write lock should round-trip through the same backend.
    let s = stamped_write_lock(a);
    assert_eq!(s & 255, 128, "JDK write stamp mode field is WBIT");
    assert!(stamped_unlock_write(a, s));
}

#[test]
fn rwlock_shim_basic_round_trip() {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0xCAFE_0000);
    let a = N.fetch_add(8, std::sync::atomic::Ordering::SeqCst);

    rw_init(a, false);
    rw_write_lock(a, 1);
    assert!(rw_is_write_locked(a));
    rw_write_unlock(a, 1);
    assert!(!rw_is_write_locked(a));
}
