use std::sync::atomic::{AtomicUsize, Ordering};
static OVERRIDE: AtomicUsize = AtomicUsize::new(0);
/// `0` means "no override — use the memoized value".
pub(super) fn get() -> usize {
    OVERRIDE.load(Ordering::Relaxed)
}
pub(super) fn set(v: usize) {
    OVERRIDE.store(v, Ordering::Relaxed);
}
