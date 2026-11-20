use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn outside_a_capture_run_every_start_runs_at_once() {
    let starts = DeferredStarts::new();
    let ran = Arc::new(AtomicUsize::new(0));
    let r = Arc::clone(&ran);
    assert!(starts.start_or_defer("t", move || {
        r.fetch_add(1, Ordering::Relaxed);
    }));
    assert_eq!(ran.load(Ordering::Relaxed), 1);
    assert_eq!(starts.pending_len(), 0);
    assert!(starts.pending_names().is_empty());
}
