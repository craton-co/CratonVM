use super::*;

#[test]
fn common_pool_singleton() {
    let p1 = common_pool() as *const _;
    let p2 = common_pool() as *const _;
    assert_eq!(p1, p2);
    assert!(common_pool().parallelism() >= 1);
}

#[test]
fn submit_blocking_returns_quickly() {
    let pool = ForkJoinPool::new(2);
    let ok = pool.submit_blocking(0xdeadbeef_usize, Some(Duration::from_millis(500)));
    assert!(ok, "worker should have signalled completion");
    // steal_count should reflect at least the one submission we made.
    assert!(pool.steal_count() >= 1);
    pool.shutdown();
}

#[test]
fn shutdown_is_idempotent() {
    let pool = ForkJoinPool::new(2);
    pool.shutdown();
    pool.shutdown();
    // Workers vec is drained, second call is a no-op.
    assert_eq!(pool.queued(), 0);
}

#[test]
fn parallelism_is_clamped() {
    let p1 = ForkJoinPool::new(0);
    assert_eq!(p1.parallelism(), 1);
    p1.shutdown();
    let p2 = ForkJoinPool::new(1024);
    assert_eq!(p2.parallelism(), 256);
    p2.shutdown();
}

#[test]
fn queued_reflects_in_flight_submissions() {
    let pool = ForkJoinPool::new(1);
    // Submit and block until done — queue should be empty after.
    let ok = pool.submit_blocking(1, Some(Duration::from_millis(500)));
    assert!(ok);
    // At steady state the worker has drained.
    assert_eq!(pool.queued(), 0);
    pool.shutdown();
}
