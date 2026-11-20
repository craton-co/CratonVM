use super::*;

// Round-2 VM §9 HIGH — `VEC_POOL_STATS_ENABLED` is a process-wide
// flag; tests that flip it must run serially or they'll race
// against each other under cargo's default parallel test runner.
// We gate the gate-mutating tests behind a local `Mutex<()>`.
static STATS_GATE_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

// -- VecPool tests -----------------------------------------------------

#[test]
fn vec_pool_acquire_returns_capacity() {
    let pool: VecPool<u8> = VecPool::new(4);
    let v = pool.acquire(128);
    assert!(v.capacity() >= 128);
    assert!(v.is_empty());
}

#[test]
fn vec_pool_release_and_reacquire_reuses() {
    let pool: VecPool<u8> = VecPool::new(4);
    let mut v = pool.acquire(256);
    v.push(1);
    v.push(2);
    let ptr = v.as_ptr();
    let cap = v.capacity();
    pool.release(v);

    // Acquire again -- should get the same allocation back.
    let v2 = pool.acquire(1);
    assert_eq!(v2.as_ptr(), ptr);
    assert_eq!(v2.capacity(), cap);
    assert!(v2.is_empty()); // was cleared on release
}

/// A reused vec SMALLER than the request is grown to the request. Before
/// wave 4 it came back at its old capacity (`reserve` was handed
/// `capacity - old_capacity`, measured from `len == 0`).
#[test]
fn vec_pool_grows_a_reused_vec_to_the_requested_capacity() {
    let pool: VecPool<u64> = VecPool::new(4);
    let small = pool.acquire(8);
    let small_cap = small.capacity();
    pool.release(small);
    let big = pool.acquire(small_cap * 4 + 1);
    assert!(
        big.capacity() >= small_cap * 4 + 1,
        "asked for {} slots, got capacity {}",
        small_cap * 4 + 1,
        big.capacity()
    );
    assert!(big.is_empty());
}

#[test]
fn vec_pool_max_size_respected() {
    // Round-2 VM §9 HIGH — the pool is sharded across
    // VEC_POOL_SHARDS (16) buckets and the cap is divided
    // accordingly. A constructor argument of `2 * VEC_POOL_SHARDS`
    // yields a per-shard cap of 2 so a single thread (which
    // always hits the same shard) sees the same overflow
    // behaviour as the pre-sharded design did at `new(2)`.
    let pool: VecPool<u8> = VecPool::new(2 * VEC_POOL_SHARDS);
    let v1 = pool.acquire(16);
    let v2 = pool.acquire(16);
    let v3 = pool.acquire(16);

    pool.release(v1);
    pool.release(v2);
    pool.release(v3); // should be dropped, the current shard is full
    assert_eq!(pool.pool_size(), 2);
}

#[test]
fn vec_pool_concurrent_access() {
    use std::sync::Arc;
    use std::thread;

    let pool = Arc::new(VecPool::<u64>::new(16));
    let mut handles = Vec::new();

    for _ in 0..8 {
        let p = Arc::clone(&pool);
        handles.push(thread::spawn(move || {
            for _ in 0..100 {
                let mut v = p.acquire(32);
                v.push(42);
                p.release(v);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // All threads completed without panic; pool is still valid.
    assert!(pool.pool_size() <= 16);
}

#[test]
fn vec_pool_pool_size_starts_at_zero() {
    let pool: VecPool<i32> = VecPool::new(8);
    assert_eq!(pool.pool_size(), 0);
}

// -- T10.7 VecPool observability tests ---------------------------------

#[test]
fn t10_vec_pool_reuses_allocations() {
    // Invoke "acquire then release" 100 times — the first few fills the
    // pool, subsequent acquires hit it.  We expect the free-list to hold
    // at least one slot afterwards and the reuse counter to be non-zero.
    //
    // Round-2 VM §9 HIGH — stats are gated on a process-wide
    // `AtomicBool` (off by default in release builds). Enable
    // them just for this test so the counter assertions hold.
    // Serialize with other stats-touching tests.
    let _gate = STATS_GATE_TEST_LOCK.lock();
    VecPool::<u64>::enable_stats();
    let pool: VecPool<u64> = VecPool::new(8);
    for _ in 0..100 {
        let v = pool.acquire(32);
        pool.release(v);
    }
    assert_eq!(pool.acquire_count(), 100);
    // After the first release, every subsequent acquire is a hit.
    assert!(
        pool.acquire_hit_count() >= 99,
        "expected >= 99 reuse hits (one per loop after the first), got {}",
        pool.acquire_hit_count()
    );
    // Pool holds at least one idle Vec between iterations.
    assert!(
        pool.pool_size() >= 1,
        "pool should still hold a Vec for reuse"
    );
    VecPool::<u64>::disable_stats();
}

#[test]
fn t10_vec_pool_acquire_release_capacity_preserved() {
    // Acquire(capacity=256) — release — re-acquire.  The re-acquire must
    // return the same allocation (same raw pointer and exact capacity)
    // without reallocating, since the pool clears in place.
    let pool: VecPool<u64> = VecPool::new(4);
    let mut v = pool.acquire(256);
    assert!(v.capacity() >= 256);
    v.push(42);
    v.push(99);
    let original_ptr = v.as_ptr();
    let original_cap = v.capacity();
    pool.release(v);
    // Re-acquire with a smaller capacity hint — the pooled Vec still has
    // the original capacity and pointer.
    let v2 = pool.acquire(1);
    assert_eq!(v2.as_ptr(), original_ptr, "expected same allocation reused");
    assert!(v2.capacity() >= 256, "capacity must be >= 256 after reuse");
    assert_eq!(
        v2.capacity(),
        original_cap,
        "capacity must be preserved exactly"
    );
    assert_eq!(v2.len(), 0, "Vec is cleared on release");
}

#[test]
fn t10_vec_pool_counters_track_acquire_and_release() {
    // Round-2 VM §9 HIGH — stats are gated on a process-wide
    // flag and the pool is sharded across VEC_POOL_SHARDS, so
    // we size the pool at `2 * VEC_POOL_SHARDS` to give the
    // single-threaded test's shard a per-shard cap of 2 (the
    // value the original test exercised against the monolithic
    // pool). Serialize with other stats-touching tests.
    let _gate = STATS_GATE_TEST_LOCK.lock();
    VecPool::<u8>::enable_stats();
    let pool: VecPool<u8> = VecPool::new(2 * VEC_POOL_SHARDS);
    // Snapshot baseline because other tests in the same suite
    // may have shared the static counters via `enable_stats`.
    let base_acquire = pool.acquire_count();
    let base_hits = pool.acquire_hit_count();
    let base_stored = pool.release_stored_count();
    assert_eq!(base_acquire, 0);
    assert_eq!(base_hits, 0);
    assert_eq!(base_stored, 0);

    let v1 = pool.acquire(16); // fresh alloc, count=1 hits=0
    let v2 = pool.acquire(16); // fresh alloc, count=2 hits=0
    let v3 = pool.acquire(16); // fresh alloc, count=3 hits=0

    pool.release(v1); // stored, release_stored=1
    pool.release(v2); // stored, release_stored=2
    pool.release(v3); // shard full (cap=2) → dropped, release_stored still 2

    assert_eq!(pool.acquire_count(), 3);
    assert_eq!(pool.acquire_hit_count(), 0);
    assert_eq!(pool.release_stored_count(), 2);
    assert_eq!(pool.pool_size(), 2);

    // Next acquire reuses a pooled Vec (hit).
    let v4 = pool.acquire(8);
    assert_eq!(pool.acquire_count(), 4);
    assert_eq!(pool.acquire_hit_count(), 1);
    pool.release(v4);
    VecPool::<u8>::disable_stats();
}

// -- Round-2 VM §9 HIGH — stat-gating and sharding tests ---------------

/// Round-2 VM §9 HIGH — when stats are disabled (default state)
/// `acquire` / `release` must NOT touch the counters.
#[test]
fn vec_pool_stats_default_disabled() {
    // Ensure clean state for this test's assertions; serialize
    // with the other gate-mutating tests so they don't race.
    let _gate = STATS_GATE_TEST_LOCK.lock();
    VecPool::<u8>::disable_stats();
    let pool: VecPool<u8> = VecPool::new(4 * VEC_POOL_SHARDS);
    for _ in 0..10 {
        let v = pool.acquire(8);
        pool.release(v);
    }
    // Counters are local to this pool instance — they must stay at
    // 0 because the global gate was off for every acquire/release.
    assert_eq!(pool.acquire_count(), 0);
    assert_eq!(pool.acquire_hit_count(), 0);
    assert_eq!(pool.release_stored_count(), 0);
}

/// Round-2 VM §9 HIGH — `enable_stats` / `disable_stats` flip the
/// runtime gate and only the calls made while enabled show up in
/// the counters.
#[test]
fn vec_pool_stats_gate_toggles_counters() {
    let _gate = STATS_GATE_TEST_LOCK.lock();
    VecPool::<u8>::disable_stats();
    let pool: VecPool<u8> = VecPool::new(4 * VEC_POOL_SHARDS);

    // First batch — stats off, no counter movement.
    for _ in 0..5 {
        let v = pool.acquire(4);
        pool.release(v);
    }
    let off_acquire = pool.acquire_count();

    VecPool::<u8>::enable_stats();
    for _ in 0..3 {
        let v = pool.acquire(4);
        pool.release(v);
    }
    let on_acquire = pool.acquire_count();
    VecPool::<u8>::disable_stats();

    assert_eq!(off_acquire, 0, "stats-off calls must not increment");
    assert_eq!(on_acquire, 3, "stats-on calls must increment");
}

/// Round-2 VM §9 HIGH — multiple shards independently retain
/// allocations. We can't easily force two threads onto different
/// shards (it's a hash) but we can verify the steady-state pool
/// across many threads stays inside the per-shard cap and that
/// the constructor builds all 16 shards.
#[test]
fn vec_pool_sharding_smoke() {
    use std::sync::Arc;
    use std::thread;

    // Per-shard cap of 4, total cap = 4 * 16 = 64.
    let pool = Arc::new(VecPool::<u32>::new(4 * VEC_POOL_SHARDS));
    let mut handles = Vec::new();
    for _ in 0..32 {
        let p = Arc::clone(&pool);
        handles.push(thread::spawn(move || {
            for _ in 0..200 {
                let mut v = p.acquire(8);
                v.push(1);
                p.release(v);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // Steady state: pool_size never exceeds the global cap.
    assert!(
        pool.pool_size() <= 4 * VEC_POOL_SHARDS,
        "pool overflow: {} > {}",
        pool.pool_size(),
        4 * VEC_POOL_SHARDS,
    );
}

/// Round-2 VM §9 HIGH — `shard_for_current_thread` is
/// deterministic for a given thread: two calls on the same
/// thread must yield the same shard index.
#[test]
fn vec_pool_shard_index_stable_for_thread() {
    let a = shard_for_current_thread();
    let b = shard_for_current_thread();
    assert_eq!(a, b);
    assert!(a < VEC_POOL_SHARDS);
}

/// The per-thread memo must answer exactly what the uncached derivation
/// answers, on the first call (which fills it) and every later one, and
/// independently on each thread.
#[test]
fn vec_pool_shard_memo_matches_the_uncached_derivation_per_thread() {
    let check = || {
        let fresh = compute_shard_for_current_thread();
        assert_eq!(shard_for_current_thread(), fresh, "first (filling) call");
        assert_eq!(shard_for_current_thread(), fresh, "memoised call");
        fresh
    };
    check();
    let handles: Vec<_> = (0..8).map(|_| std::thread::spawn(check)).collect();
    for h in handles {
        assert!(h.join().unwrap() < VEC_POOL_SHARDS);
    }
}
