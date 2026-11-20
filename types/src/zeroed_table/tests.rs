use super::*;
use std::sync::atomic::Ordering;

#[test]
fn zs6rs_both_arms_build_the_same_zero_table() {
    for lazy in [true, false] {
        let b = zeroed_boxed_slice_with(lazy, 1 << 16, || AtomicU32::new(0));
        assert_eq!(b.len(), 1 << 16);
        assert!(b.iter().all(|a| a.load(Ordering::Relaxed) == 0));
        b[12345].store(7, Ordering::Relaxed);
        assert_eq!(b[12345].load(Ordering::Relaxed), 7);
        let a = zeroed_arc_slice_with(lazy, 4096, || AtomicU64::new(0));
        assert_eq!(a.len(), 4096);
        assert!(a.iter().all(|x| x.load(Ordering::Relaxed) == 0));
        let p = zeroed_boxed_slice_with(lazy, 64, || {
            std::sync::atomic::AtomicPtr::<u8>::new(std::ptr::null_mut())
        });
        assert!(p.iter().all(|x| x.load(Ordering::Relaxed).is_null()));
    }
    // An empty table is fine in both arms.
    assert!(zeroed_boxed_slice_with(true, 0, || AtomicU64::new(0)).is_empty());
    assert!(zeroed_arc_slice_with(false, 0, || AtomicU64::new(0)).is_empty());
}

#[test]
fn zs6rs_the_lazy_arm_does_not_call_the_eager_builder() {
    let mut calls = 0;
    let _ = zeroed_boxed_slice_with(true, 1000, || {
        calls += 1;
        AtomicU64::new(0)
    });
    assert_eq!(calls, 0);
    let _ = zeroed_boxed_slice_with(false, 1000, || {
        calls += 1;
        AtomicU64::new(0)
    });
    assert_eq!(calls, 1000);
}
