// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Allocation fast-path optimisations for the JVM runtime.
//!
//! Provides:
//! - Pre-allocated exception message constants
//! - `SmartMessage` (Cow-based) to avoid heap allocation for known messages
//! - `VecPool<T>` for reusing `Vec` allocations (e.g. operand stacks)

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// VecPool sharding / stat gating
// ---------------------------------------------------------------------------

/// Round-2 VM §9 HIGH — number of independent `VecPool` shards. Hashed
/// `thread::current().id()` modulo this constant picks a shard, so two
/// threads only contend on `pool.acquire` / `pool.release` when their
/// thread-id hashes collide on the same shard (~1/16 expected under
/// random scheduling). 16 was chosen because it matches the typical
/// physical-core count on the workloads we benchmark, keeps the
/// per-`VecPool` memory overhead small (16 × `Mutex<Vec<Vec<T>>>`),
/// and is a power of two so the modulo lowers to a bitmask.
const VEC_POOL_SHARDS: usize = 16;

/// Round-2 VM §9 HIGH — per-shard stat counters add a `fetch_add` × 4
/// per `acquire`+`release` cycle that the hot frame-push path pays
/// even when no operator is reading them. Without a Cargo feature
/// flag (this fix is constrained to the two source files) we gate the
/// counters behind a process-wide `AtomicBool` that defaults to
/// `false`. `VecPool::enable_stats()` flips the flag on for the
/// duration of a benchmark / diagnostic run; release builds skip the
/// fetch_adds entirely after the single `load(Relaxed)` check.
///
/// TODO(vec_pool_stats): once Cargo.toml may be edited, replace this
/// runtime check with a `#[cfg(feature = "vec_pool_stats")]` gate so
/// release builds compile the counters out entirely.
static VEC_POOL_STATS_ENABLED: AtomicBool = AtomicBool::new(false);

/// Round-2 VM §9 HIGH — `thread::current().id().as_u64()` is unstable
/// (#67939) so we hash the public `ThreadId` instead. Same approach
/// as `threading::event_loop::os_thread_id` and the per-shard pools
/// elsewhere in the runtime.
#[inline]
fn shard_for_current_thread() -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    // `& (VEC_POOL_SHARDS - 1)` because VEC_POOL_SHARDS is a power of two.
    (h.finish() as usize) & (VEC_POOL_SHARDS - 1)
}

// ---------------------------------------------------------------------------
// Pre-allocated exception message constants
// ---------------------------------------------------------------------------

/// Pre-allocated static strings for the 20 most common JVM exceptions.
pub mod exception_messages {
    pub const NULL_POINTER: &str = "NullPointerException";
    pub const ARRAY_INDEX_OOB: &str = "ArrayIndexOutOfBoundsException";
    pub const CLASS_CAST: &str = "ClassCastException";
    pub const ARITHMETIC: &str = "ArithmeticException: / by zero";
    pub const STACK_OVERFLOW: &str = "StackOverflowError";
    pub const OUT_OF_MEMORY: &str = "OutOfMemoryError: Java heap space";
    pub const CLASS_NOT_FOUND: &str = "ClassNotFoundException";
    pub const NO_SUCH_METHOD: &str = "NoSuchMethodError";
    pub const NO_SUCH_FIELD: &str = "NoSuchFieldError";
    pub const ILLEGAL_ARGUMENT: &str = "IllegalArgumentException";
    pub const ILLEGAL_STATE: &str = "IllegalStateException";
    pub const UNSUPPORTED_OP: &str = "UnsupportedOperationException";
    pub const NEGATIVE_ARRAY: &str = "NegativeArraySizeException";
    pub const ILLEGAL_MONITOR: &str = "IllegalMonitorStateException";
    pub const INTERRUPTED: &str = "InterruptedException";
    pub const INDEX_OOB: &str = "IndexOutOfBoundsException";
    pub const CONCURRENT_MOD: &str = "ConcurrentModificationException";
    pub const NO_CLASS_DEF: &str = "NoClassDefFoundError";
    pub const LINKAGE: &str = "LinkageError";
    pub const VERIFY: &str = "VerifyError";

    /// All exception message constants, for iteration / testing.
    pub const ALL: [&str; 20] = [
        NULL_POINTER,
        ARRAY_INDEX_OOB,
        CLASS_CAST,
        ARITHMETIC,
        STACK_OVERFLOW,
        OUT_OF_MEMORY,
        CLASS_NOT_FOUND,
        NO_SUCH_METHOD,
        NO_SUCH_FIELD,
        ILLEGAL_ARGUMENT,
        ILLEGAL_STATE,
        UNSUPPORTED_OP,
        NEGATIVE_ARRAY,
        ILLEGAL_MONITOR,
        INTERRUPTED,
        INDEX_OOB,
        CONCURRENT_MOD,
        NO_CLASS_DEF,
        LINKAGE,
        VERIFY,
    ];
}

// ---------------------------------------------------------------------------
// SmartMessage
// ---------------------------------------------------------------------------

/// A message type that borrows known static strings and only allocates for
/// dynamic / unknown messages.
pub type SmartMessage = Cow<'static, str>;

/// Return a `SmartMessage` for the given string.  If `msg` matches one of the
/// 20 pre-allocated exception constants, the returned `Cow` borrows the static
/// reference (zero allocation).  Otherwise an owned `String` is produced.
pub fn smart_message(msg: &str) -> SmartMessage {
    match msg {
        s if s == exception_messages::NULL_POINTER => {
            Cow::Borrowed(exception_messages::NULL_POINTER)
        }
        s if s == exception_messages::ARRAY_INDEX_OOB => {
            Cow::Borrowed(exception_messages::ARRAY_INDEX_OOB)
        }
        s if s == exception_messages::CLASS_CAST => Cow::Borrowed(exception_messages::CLASS_CAST),
        s if s == exception_messages::ARITHMETIC => Cow::Borrowed(exception_messages::ARITHMETIC),
        s if s == exception_messages::STACK_OVERFLOW => {
            Cow::Borrowed(exception_messages::STACK_OVERFLOW)
        }
        s if s == exception_messages::OUT_OF_MEMORY => {
            Cow::Borrowed(exception_messages::OUT_OF_MEMORY)
        }
        s if s == exception_messages::CLASS_NOT_FOUND => {
            Cow::Borrowed(exception_messages::CLASS_NOT_FOUND)
        }
        s if s == exception_messages::NO_SUCH_METHOD => {
            Cow::Borrowed(exception_messages::NO_SUCH_METHOD)
        }
        s if s == exception_messages::NO_SUCH_FIELD => {
            Cow::Borrowed(exception_messages::NO_SUCH_FIELD)
        }
        s if s == exception_messages::ILLEGAL_ARGUMENT => {
            Cow::Borrowed(exception_messages::ILLEGAL_ARGUMENT)
        }
        s if s == exception_messages::ILLEGAL_STATE => {
            Cow::Borrowed(exception_messages::ILLEGAL_STATE)
        }
        s if s == exception_messages::UNSUPPORTED_OP => {
            Cow::Borrowed(exception_messages::UNSUPPORTED_OP)
        }
        s if s == exception_messages::NEGATIVE_ARRAY => {
            Cow::Borrowed(exception_messages::NEGATIVE_ARRAY)
        }
        s if s == exception_messages::ILLEGAL_MONITOR => {
            Cow::Borrowed(exception_messages::ILLEGAL_MONITOR)
        }
        s if s == exception_messages::INTERRUPTED => Cow::Borrowed(exception_messages::INTERRUPTED),
        s if s == exception_messages::INDEX_OOB => Cow::Borrowed(exception_messages::INDEX_OOB),
        s if s == exception_messages::CONCURRENT_MOD => {
            Cow::Borrowed(exception_messages::CONCURRENT_MOD)
        }
        s if s == exception_messages::NO_CLASS_DEF => {
            Cow::Borrowed(exception_messages::NO_CLASS_DEF)
        }
        s if s == exception_messages::LINKAGE => Cow::Borrowed(exception_messages::LINKAGE),
        s if s == exception_messages::VERIFY => Cow::Borrowed(exception_messages::VERIFY),
        other => Cow::Owned(other.to_string()),
    }
}

/// Format an array-index-out-of-bounds message.
pub fn array_index_oob_message(index: i32, length: i32) -> SmartMessage {
    Cow::Owned(format!(
        "Index {} out of bounds for length {}",
        index, length
    ))
}

// ---------------------------------------------------------------------------
// VecPool
// ---------------------------------------------------------------------------

/// A thread-safe pool of reusable `Vec<T>` instances.
///
/// Operand stacks (and similar per-frame vectors) are allocated and dropped at
/// high frequency.  `VecPool` keeps a bounded number of previously-used
/// vectors so that callers can re-use the backing allocation instead of going
/// through the global allocator on every frame push/pop.
///
/// Round-2 VM §9 HIGH — under multi-threaded workloads the previous
/// design serialised every frame push/pop on a single
/// `std::sync::Mutex<Vec<Vec<T>>>`. We now:
///
/// 1. Use `parking_lot::Mutex`, which is ~2× faster on uncontended
///    acquire on Windows (no `poison` bookkeeping, no boxed inner
///    state, an in-line fast path).
/// 2. Shard the pool across `VEC_POOL_SHARDS` (= 16) independent
///    mutexes keyed by hashed `thread::current().id()`. Two threads
///    only collide when their hashes land in the same shard; expected
///    contention drops ~16× for workloads with ≤ 16 hot threads.
/// 3. Gate the four `AtomicU64` stat counters behind a process-wide
///    `AtomicBool` (`VEC_POOL_STATS_ENABLED`); release builds skip
///    the per-acquire/release `fetch_add` traffic after a single
///    `Relaxed` load.
pub struct VecPool<T> {
    /// Round-2 VM §9 HIGH — per-shard free-list. Each shard owns its
    /// own `parking_lot::Mutex` and free-list, so two threads only
    /// contend when their hashed `thread_id`s land on the same shard.
    shards: [Mutex<Vec<Vec<T>>>; VEC_POOL_SHARDS],
    /// Per-shard cap. The original `max_pool_size` is split across
    /// the shards (rounded up so a max of 1 still keeps one Vec
    /// somewhere), so the overall steady-state pool size remains
    /// roughly bounded by the constructor argument.
    max_per_shard: usize,
    /// T10.7 observability — total calls to `acquire`.
    ///
    /// Round-2 VM §9 HIGH — only written when
    /// `VEC_POOL_STATS_ENABLED` is `true`.
    acquire_count: AtomicU64,
    /// T10.7 observability — total `acquire` calls that reused a pooled Vec
    /// (i.e. skipped `Vec::with_capacity`).
    ///
    /// Round-2 VM §9 HIGH — only written when stats are enabled.
    acquire_hits: AtomicU64,
    /// T10.7 observability — total calls to `release` that stored the Vec
    /// (did not drop it because the pool was full).
    ///
    /// Round-2 VM §9 HIGH — only written when stats are enabled.
    release_stored: AtomicU64,
}

impl<T> VecPool<T> {
    /// Create a new pool that retains at most `max_pool_size` vectors.
    ///
    /// Round-2 VM §9 HIGH — the cap is now split across
    /// `VEC_POOL_SHARDS` shards, so the steady-state pool size is
    /// `max_pool_size` ± shard-rounding error.
    pub fn new(max_pool_size: usize) -> Self {
        // Ceiling-divide so callers that pass `max_pool_size = 1`
        // still keep at least one Vec on at least one shard.
        let max_per_shard = max_pool_size.div_ceil(VEC_POOL_SHARDS).max(1);
        // `[Mutex::new(Vec::new()); 16]` is not const for non-Copy
        // payloads; build the array element-by-element via a fixed
        // initializer expression.
        let shards: [Mutex<Vec<Vec<T>>>; VEC_POOL_SHARDS] = [
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
        ];
        // Compile-time check: the initializer above must match the
        // shard count. If `VEC_POOL_SHARDS` is ever bumped, this line
        // catches the missing slots before the binary ships.
        const _: () = assert!(VEC_POOL_SHARDS == 16);
        Self {
            shards,
            max_per_shard,
            acquire_count: AtomicU64::new(0),
            acquire_hits: AtomicU64::new(0),
            release_stored: AtomicU64::new(0),
        }
    }

    /// Enable the per-acquire / per-release stat counters for the
    /// remainder of the process. Off by default — see
    /// `VEC_POOL_STATS_ENABLED`. Safe to call from any thread; the
    /// flag is read with `Relaxed` ordering on the hot path so the
    /// effect propagates eventually-consistently.
    pub fn enable_stats() {
        VEC_POOL_STATS_ENABLED.store(true, Ordering::Relaxed);
    }

    /// Disable the per-acquire / per-release stat counters. Existing
    /// counter values are not reset — callers that need a clean
    /// baseline should snapshot before re-enabling.
    pub fn disable_stats() {
        VEC_POOL_STATS_ENABLED.store(false, Ordering::Relaxed);
    }

    /// Whether the global stat-counter gate is currently on.
    #[inline]
    pub fn stats_enabled() -> bool {
        VEC_POOL_STATS_ENABLED.load(Ordering::Relaxed)
    }

    /// Acquire a `Vec<T>` with at least `capacity` elements reserved.
    ///
    /// If the per-thread shard has a vector available it is returned
    /// (after ensuring its capacity is at least `capacity`). Otherwise
    /// a fresh vector is allocated.
    ///
    /// Round-2 VM §9 HIGH — only the shard for the current thread
    /// is locked, so cross-thread acquire contention is ~16× lower
    /// than the previous single-mutex design.
    pub fn acquire(&self, capacity: usize) -> Vec<T> {
        let stats_on = Self::stats_enabled();
        if stats_on {
            self.acquire_count.fetch_add(1, Ordering::Relaxed);
        }
        let shard = &self.shards[shard_for_current_thread()];
        let mut guard = shard.lock();
        if let Some(mut vec) = guard.pop() {
            // Drop the guard before doing the (possibly allocating)
            // `reserve` so we don't hold the shard mutex across the
            // global allocator call.
            drop(guard);
            // The vec is already clear (we clear on release), just ensure
            // sufficient capacity.
            if vec.capacity() < capacity {
                vec.reserve(capacity - vec.capacity());
            }
            if stats_on {
                self.acquire_hits.fetch_add(1, Ordering::Relaxed);
            }
            vec
        } else {
            drop(guard);
            Vec::with_capacity(capacity)
        }
    }

    /// Return a `Vec<T>` to the pool for future reuse.
    ///
    /// The vector is cleared before storage.  If the shard is already
    /// at its per-shard cap the vector is simply dropped.
    ///
    /// Round-2 VM §9 HIGH — release routes back to the same shard
    /// the caller's thread acquired from, keeping the per-thread
    /// access pattern free of cross-shard pong.
    pub fn release(&self, mut vec: Vec<T>) {
        vec.clear();
        let stats_on = Self::stats_enabled();
        let shard = &self.shards[shard_for_current_thread()];
        let mut guard = shard.lock();
        if guard.len() < self.max_per_shard {
            guard.push(vec);
            drop(guard);
            if stats_on {
                self.release_stored.fetch_add(1, Ordering::Relaxed);
            }
        }
        // else: drop `vec` -- shard is full.
    }

    /// Number of vectors currently sitting in the pool (summed across
    /// all shards).
    pub fn pool_size(&self) -> usize {
        self.shards.iter().map(|s| s.lock().len()).sum()
    }

    /// Total `acquire` calls since creation (T10.7 diagnostics).
    ///
    /// Round-2 VM §9 HIGH — only incremented while `stats_enabled()`
    /// is `true`. Returns 0 if stats were never enabled.
    pub fn acquire_count(&self) -> u64 {
        self.acquire_count.load(Ordering::Relaxed)
    }

    /// Total `acquire` calls that reused a pooled Vec (T10.7 diagnostics).
    ///
    /// Round-2 VM §9 HIGH — see `acquire_count` re: stat gating.
    pub fn acquire_hit_count(&self) -> u64 {
        self.acquire_hits.load(Ordering::Relaxed)
    }

    /// Total `release` calls that stored the Vec (T10.7 diagnostics).
    ///
    /// Round-2 VM §9 HIGH — see `acquire_count` re: stat gating.
    pub fn release_stored_count(&self) -> u64 {
        self.release_stored.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Round-2 VM §9 HIGH — `VEC_POOL_STATS_ENABLED` is a process-wide
    // flag; tests that flip it must run serially or they'll race
    // against each other under cargo's default parallel test runner.
    // We gate the gate-mutating tests behind a local `Mutex<()>`.
    static STATS_GATE_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    // -- SmartMessage tests ------------------------------------------------

    #[test]
    fn smart_message_borrowed_for_known_constants() {
        for &constant in &exception_messages::ALL {
            let msg = smart_message(constant);
            assert!(
                matches!(msg, Cow::Borrowed(_)),
                "expected Borrowed for {:?}, got Owned",
                constant
            );
        }
    }

    #[test]
    fn smart_message_owned_for_unknown() {
        let msg = smart_message("some random error text");
        assert!(matches!(msg, Cow::Owned(_)));
        assert_eq!(&*msg, "some random error text");
    }

    #[test]
    fn array_index_oob_message_format() {
        let msg = array_index_oob_message(5, 3);
        assert_eq!(&*msg, "Index 5 out of bounds for length 3");
    }

    #[test]
    fn array_index_oob_message_negative_index() {
        let msg = array_index_oob_message(-1, 10);
        assert_eq!(&*msg, "Index -1 out of bounds for length 10");
    }

    // -- exception_messages tests ------------------------------------------

    #[test]
    fn exception_messages_all_non_empty() {
        for &constant in &exception_messages::ALL {
            assert!(!constant.is_empty(), "empty exception message constant");
        }
    }

    #[test]
    fn exception_messages_count_is_20() {
        assert_eq!(exception_messages::ALL.len(), 20);
    }

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
}
