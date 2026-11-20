use super::cooperative_grace_wait;
use crate::threading::{GcBarrier, ThreadId};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A peer that reaches its poll inside the grace slice meets the quota
/// cooperatively, and the wait returns on its arrival, not at the end of
/// the slice.
#[test]
fn a_peer_arriving_inside_the_grace_slice_meets_the_quota() {
    let barrier = Arc::new(GcBarrier::new());
    assert!(barrier.request_stw(ThreadId(0), 2));
    let b = barrier.clone();
    let peer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        b.arrive_and_wait(ThreadId(1))
    });
    let t0 = Instant::now();
    assert!(cooperative_grace_wait(&barrier, Duration::from_secs(30)));
    assert!(
        t0.elapsed() < Duration::from_secs(20),
        "the wait ends at the arrival, not at the end of the slice"
    );
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    let _ = peer.join();
}

/// A peer that never arrives (a loop compiled without polls) is waited
/// for the whole slice and no longer: the take-over then freezes it.
#[test]
fn a_peer_that_never_arrives_ends_the_slice_unmet() {
    let barrier = GcBarrier::new();
    assert!(barrier.request_stw(ThreadId(0), 2));
    let t0 = Instant::now();
    assert!(!cooperative_grace_wait(&barrier, Duration::from_millis(5)));
    assert!(t0.elapsed() >= Duration::from_millis(5), "the whole slice");
    // A zero slice asks once.
    assert!(!cooperative_grace_wait(&barrier, Duration::ZERO));
    barrier.reduce_expected(1);
    assert!(cooperative_grace_wait(&barrier, Duration::ZERO));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
}
