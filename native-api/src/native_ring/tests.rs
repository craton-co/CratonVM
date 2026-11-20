use super::*;
use std::sync::OnceLock;

// Distinct high sentinel pointers, unlikely to collide with real
// registrations in the shared process-global maps.
const PTR_EAGER: usize = 0xDEAD_0001;
const PTR_LAZY: usize = 0xDEAD_0002;
const PTR_WRAP: usize = 0xDEAD_0003;

fn ring_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn reset_ring_for_test() {
    let mut ring = RING.lock();
    ring.entries = [EMPTY; RING_SIZE];
    ring.next = 0;
    ring.next_generation = 0;
    let _ = NATIVE_STACK.try_with(|s| s.borrow_mut().clear());
}

struct RingResetGuard;

impl Drop for RingResetGuard {
    fn drop(&mut self) {
        enable(false);
        reset_ring_for_test();
    }
}

#[test]
fn eager_register_resolves() {
    register_name(PTR_EAGER, "java/lang/Foo.bar()V");
    assert_eq!(name_of(PTR_EAGER).as_deref(), Some("java/lang/Foo.bar()V"));
}

#[test]
fn lazy_register_resolves_and_promotes() {
    // FIX (review STUB): zero-allocation deferred registration. The
    // resolver runs only when the name is actually needed.
    fn resolver() -> String {
        "java/lang/Baz.qux()V".to_string()
    }
    register_name_lazy(PTR_LAZY, resolver);
    // First lookup runs the resolver...
    assert_eq!(name_of(PTR_LAZY).as_deref(), Some("java/lang/Baz.qux()V"));
    // ...and promotes the result into the eager map for subsequent hits.
    assert!(name_map().lock().contains_key(&PTR_LAZY));
    assert_eq!(name_of(PTR_LAZY).as_deref(), Some("java/lang/Baz.qux()V"));
}

#[test]
fn null_ptr_is_ignored() {
    register_name(0, "should/not/Register.x()V");
    assert_eq!(name_of(0), None);
}

#[test]
fn unknown_ptr_is_none() {
    assert_eq!(name_of(0x0BAD_BEEF_usize), None);
}

#[test]
fn record_exit_ignores_stale_token_after_wraparound() {
    let _guard = ring_test_lock().lock();
    let _reset = RingResetGuard;
    enable(true);
    reset_ring_for_test();

    let stale = record_enter(PTR_WRAP);
    let stale_slot = token_slot(stale);
    let stale_generation = token_generation(stale);
    for i in 0..RING_SIZE {
        let _ = record_enter(0xBEEF_0000usize + i);
    }

    {
        let ring = RING.lock();
        let reused = ring.entries[stale_slot];
        assert_ne!(reused.cb_ptr, PTR_WRAP);
        assert_ne!(reused.generation, stale_generation);
        assert_eq!(reused.exit_ms, 0);
    }

    record_exit(stale);

    {
        let ring = RING.lock();
        let reused = ring.entries[stale_slot];
        assert_ne!(reused.cb_ptr, PTR_WRAP);
        assert_ne!(reused.generation, stale_generation);
        assert_eq!(reused.exit_ms, 0);
    }
}
