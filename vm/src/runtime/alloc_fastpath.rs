//! Allocation fast-path optimisations for the JVM runtime.
//!
//! Provides:
//! - Pre-allocated exception message constants
//! - `SmartMessage` (Cow-based) to avoid heap allocation for known messages
//! - `VecPool<T>` for reusing `Vec` allocations (e.g. operand stacks)

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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
        s if s == exception_messages::NULL_POINTER => Cow::Borrowed(exception_messages::NULL_POINTER),
        s if s == exception_messages::ARRAY_INDEX_OOB => Cow::Borrowed(exception_messages::ARRAY_INDEX_OOB),
        s if s == exception_messages::CLASS_CAST => Cow::Borrowed(exception_messages::CLASS_CAST),
        s if s == exception_messages::ARITHMETIC => Cow::Borrowed(exception_messages::ARITHMETIC),
        s if s == exception_messages::STACK_OVERFLOW => Cow::Borrowed(exception_messages::STACK_OVERFLOW),
        s if s == exception_messages::OUT_OF_MEMORY => Cow::Borrowed(exception_messages::OUT_OF_MEMORY),
        s if s == exception_messages::CLASS_NOT_FOUND => Cow::Borrowed(exception_messages::CLASS_NOT_FOUND),
        s if s == exception_messages::NO_SUCH_METHOD => Cow::Borrowed(exception_messages::NO_SUCH_METHOD),
        s if s == exception_messages::NO_SUCH_FIELD => Cow::Borrowed(exception_messages::NO_SUCH_FIELD),
        s if s == exception_messages::ILLEGAL_ARGUMENT => Cow::Borrowed(exception_messages::ILLEGAL_ARGUMENT),
        s if s == exception_messages::ILLEGAL_STATE => Cow::Borrowed(exception_messages::ILLEGAL_STATE),
        s if s == exception_messages::UNSUPPORTED_OP => Cow::Borrowed(exception_messages::UNSUPPORTED_OP),
        s if s == exception_messages::NEGATIVE_ARRAY => Cow::Borrowed(exception_messages::NEGATIVE_ARRAY),
        s if s == exception_messages::ILLEGAL_MONITOR => Cow::Borrowed(exception_messages::ILLEGAL_MONITOR),
        s if s == exception_messages::INTERRUPTED => Cow::Borrowed(exception_messages::INTERRUPTED),
        s if s == exception_messages::INDEX_OOB => Cow::Borrowed(exception_messages::INDEX_OOB),
        s if s == exception_messages::CONCURRENT_MOD => Cow::Borrowed(exception_messages::CONCURRENT_MOD),
        s if s == exception_messages::NO_CLASS_DEF => Cow::Borrowed(exception_messages::NO_CLASS_DEF),
        s if s == exception_messages::LINKAGE => Cow::Borrowed(exception_messages::LINKAGE),
        s if s == exception_messages::VERIFY => Cow::Borrowed(exception_messages::VERIFY),
        other => Cow::Owned(other.to_string()),
    }
}

/// Format an array-index-out-of-bounds message.
pub fn array_index_oob_message(index: i32, length: i32) -> SmartMessage {
    Cow::Owned(format!("Index {} out of bounds for length {}", index, length))
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
pub struct VecPool<T> {
    pool: Mutex<Vec<Vec<T>>>,
    max_pool_size: usize,
    /// T10.7 observability — total calls to `acquire`.
    acquire_count: AtomicU64,
    /// T10.7 observability — total `acquire` calls that reused a pooled Vec
    /// (i.e. skipped `Vec::with_capacity`).
    acquire_hits: AtomicU64,
    /// T10.7 observability — total calls to `release` that stored the Vec
    /// (did not drop it because the pool was full).
    release_stored: AtomicU64,
}

impl<T> VecPool<T> {
    /// Create a new pool that retains at most `max_pool_size` vectors.
    pub fn new(max_pool_size: usize) -> Self {
        Self {
            pool: Mutex::new(Vec::new()),
            max_pool_size,
            acquire_count: AtomicU64::new(0),
            acquire_hits: AtomicU64::new(0),
            release_stored: AtomicU64::new(0),
        }
    }

    /// Acquire a `Vec<T>` with at least `capacity` elements reserved.
    ///
    /// If the pool has a vector available it is returned (after ensuring its
    /// capacity is at least `capacity`).  Otherwise a fresh vector is
    /// allocated.
    pub fn acquire(&self, capacity: usize) -> Vec<T> {
        self.acquire_count.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.pool.lock().unwrap();
        if let Some(mut vec) = guard.pop() {
            // The vec is already clear (we clear on release), just ensure
            // sufficient capacity.
            if vec.capacity() < capacity {
                vec.reserve(capacity - vec.capacity());
            }
            self.acquire_hits.fetch_add(1, Ordering::Relaxed);
            vec
        } else {
            Vec::with_capacity(capacity)
        }
    }

    /// Return a `Vec<T>` to the pool for future reuse.
    ///
    /// The vector is cleared before storage.  If the pool is already at its
    /// maximum size the vector is simply dropped.
    pub fn release(&self, mut vec: Vec<T>) {
        vec.clear();
        let mut guard = self.pool.lock().unwrap();
        if guard.len() < self.max_pool_size {
            guard.push(vec);
            self.release_stored.fetch_add(1, Ordering::Relaxed);
        }
        // else: drop `vec` -- pool is full.
    }

    /// Number of vectors currently sitting in the pool.
    pub fn pool_size(&self) -> usize {
        self.pool.lock().unwrap().len()
    }

    /// Total `acquire` calls since creation (T10.7 diagnostics).
    pub fn acquire_count(&self) -> u64 {
        self.acquire_count.load(Ordering::Relaxed)
    }

    /// Total `acquire` calls that reused a pooled Vec (T10.7 diagnostics).
    pub fn acquire_hit_count(&self) -> u64 {
        self.acquire_hits.load(Ordering::Relaxed)
    }

    /// Total `release` calls that stored the Vec (T10.7 diagnostics).
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
        let pool: VecPool<u8> = VecPool::new(2);
        let v1 = pool.acquire(16);
        let v2 = pool.acquire(16);
        let v3 = pool.acquire(16);

        pool.release(v1);
        pool.release(v2);
        pool.release(v3); // should be dropped, pool is full
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
        assert!(pool.pool_size() >= 1, "pool should still hold a Vec for reuse");
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
        assert_eq!(v2.capacity(), original_cap, "capacity must be preserved exactly");
        assert_eq!(v2.len(), 0, "Vec is cleared on release");
    }

    #[test]
    fn t10_vec_pool_counters_track_acquire_and_release() {
        let pool: VecPool<u8> = VecPool::new(2);
        assert_eq!(pool.acquire_count(), 0);
        assert_eq!(pool.acquire_hit_count(), 0);
        assert_eq!(pool.release_stored_count(), 0);

        let v1 = pool.acquire(16);  // fresh alloc, count=1 hits=0
        let v2 = pool.acquire(16);  // fresh alloc, count=2 hits=0
        let v3 = pool.acquire(16);  // fresh alloc, count=3 hits=0

        pool.release(v1);           // stored, release_stored=1
        pool.release(v2);           // stored, release_stored=2
        pool.release(v3);           // pool full (max=2) → dropped, release_stored still 2

        assert_eq!(pool.acquire_count(), 3);
        assert_eq!(pool.acquire_hit_count(), 0);
        assert_eq!(pool.release_stored_count(), 2);
        assert_eq!(pool.pool_size(), 2);

        // Next acquire reuses a pooled Vec (hit).
        let v4 = pool.acquire(8);
        assert_eq!(pool.acquire_count(), 4);
        assert_eq!(pool.acquire_hit_count(), 1);
        pool.release(v4);
    }
}
