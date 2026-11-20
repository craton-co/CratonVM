use super::NativeAllocNoGcPoint;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn a_door_without_a_pause_passes() {
    let epoch = AtomicU64::new(7);
    let guard = NativeAllocNoGcPoint::enter("alloc_object", &epoch);
    assert!(!guard.pause_completed());
    drop(guard);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "native allocation door `new_array` had a GC point")]
fn a_pause_completed_inside_the_door_panics_in_a_debug_build() {
    let epoch = AtomicU64::new(7);
    let guard = NativeAllocNoGcPoint::enter("new_array", &epoch);
    epoch.fetch_add(1, Ordering::Release);
    assert!(guard.pause_completed());
    drop(guard);
}

#[cfg(debug_assertions)]
#[test]
fn an_unwinding_door_is_not_checked() {
    let epoch = AtomicU64::new(7);
    let r = std::panic::catch_unwind(|| {
        let _guard = NativeAllocNoGcPoint::enter("alloc_object", &epoch);
        epoch.fetch_add(1, Ordering::Release);
        panic!("the door's OutOfMemoryError unwind");
    });
    let text = r
        .err()
        .and_then(|e| e.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert_eq!(text, "the door's OutOfMemoryError unwind");
}

#[cfg(not(debug_assertions))]
#[test]
fn the_release_guard_is_zero_sized_and_never_fires() {
    let epoch = AtomicU64::new(7);
    assert_eq!(std::mem::size_of::<NativeAllocNoGcPoint<'_>>(), 0);
    let guard = NativeAllocNoGcPoint::enter("alloc_object", &epoch);
    epoch.fetch_add(1, Ordering::Release);
    assert!(!guard.pause_completed());
}
