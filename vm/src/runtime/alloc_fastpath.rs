// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Allocation fast-path optimisations for the JVM runtime.
//!
//! Provides `VecPool<T>` for reusing `Vec` allocations (e.g. operand stacks).
//!
//! It also used to provide a table of 20 "pre-allocated exception message
//! constants", a `SmartMessage` (`Cow`) wrapper over them and an
//! `array_index_oob_message` formatter. Nothing outside this file's own tests
//! ever called any of the three (the throw paths build their messages where
//! they throw, e.g. `materialize_implicit_signal`), and the constants were
//! class names or `toString()` shapes (`"ArithmeticException: / by zero"`),
//! not messages, so they were removed in interpreter round i1 wave 5.

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
///
/// The answer is a pure function of the calling thread, so it is computed once
/// per thread and remembered: the derivation clones the `Thread` handle (an
/// `Arc` increment and decrement) and runs SipHash, and it ran twice per
/// `acquire`/`release` pair — four times per frame refill on a deep recursion.
#[inline]
fn shard_for_current_thread() -> usize {
    thread_local! {
        static SHARD: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    }
    // `try_with`: during thread-local teardown the slot is gone; fall back to
    // computing the answer, which is identical.
    SHARD
        .try_with(|slot| {
            let cached = slot.get();
            if cached != usize::MAX {
                return cached;
            }
            let shard = compute_shard_for_current_thread();
            slot.set(shard);
            shard
        })
        .unwrap_or_else(|_| compute_shard_for_current_thread())
}

fn compute_shard_for_current_thread() -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    // `& (VEC_POOL_SHARDS - 1)` because VEC_POOL_SHARDS is a power of two.
    (h.finish() as usize) & (VEC_POOL_SHARDS - 1)
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
            // sufficient capacity. `reserve` counts from `len`, not from
            // `capacity`: this used to pass `capacity - vec.capacity()`,
            // which on an empty vec asks for LESS than it already has, so a
            // reused vec smaller than the request came back ungrown and the
            // frame built on it reallocated on its first pushes.
            if vec.capacity() < capacity {
                vec.reserve(capacity - vec.len());
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
mod tests;
