// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GPU / GC coordination primitives (Part F of the GPU offload plan).
//!
//! This module exists only when the `gpu-offload` Cargo feature is on. It
//! provides the small handful of types and methods the rest of the GPU
//! offload pipeline needs to keep the garbage collector and an in-flight
//! CUDA kernel from racing each other:
//!
//! - [`SafepointToken`] — a zero-sized RAII guard. While at least one
//!   token is alive on the [`Heap`](crate::heap::Heap), the GC refuses to
//!   run. The token is the *only* way to enter a "GPU critical section".
//! - [`Heap::enter_gpu_critical`](crate::heap::Heap::enter_gpu_critical) —
//!   the sole public constructor for a [`SafepointToken`].
//! - [`Heap::pin_ref`](crate::heap::Heap::pin_ref) /
//!   [`Heap::unpin_ref`](crate::heap::Heap::unpin_ref) — record/forget the
//!   `ObjectRef`s that a kernel is reading from. Pinned refs are walked
//!   as additional GC roots so that even if a collection happens *between*
//!   kernels (when no token is alive), the JVM arrays a future kernel may
//!   re-use stay reachable.
//!
//! ## Invariants
//!
//! 1. Tokens are not [`Send`]: the active count is just a counter, but
//!    the API contract is "the same thread holding the token will drop
//!    it." Crossing threads would let a panic on thread A leak the
//!    counter forever.
//! 2. Dropping a token decrements `gpu_critical_count` exactly once. A
//!    `SafepointToken` cannot be cloned.
//! 3. The GC delay loop wakes periodically (yield-spin, matching the
//!    crate's pattern in `reference.rs`) and re-checks the counter. If a
//!    configurable deadline (default 5 s) passes without the counter
//!    draining, we emit a single `tracing::warn!` and keep waiting — we
//!    NEVER force a collection while a token is alive.

use std::marker::PhantomData;

/// Default deadline before we log a "GC delayed by GPU" warning, in seconds.
///
/// Hardcoded here so callers (and the existing semi-space `collect_garbage`
/// path) can reach it without taking a config parameter — the GPU side
/// is supposed to release the token in well under a second; anything
/// longer is operator-visible and deserves a log line.
pub const GPU_CRITICAL_DEADLINE_SECS: u64 = 5;

/// Zero-sized RAII guard for a "GPU critical section".
///
/// While one or more tokens are alive on a [`Heap`](crate::heap::Heap),
/// `Heap::collect_garbage` (and its finalizer variant) will spin-yield
/// rather than collect. Drop the token to release the GC.
///
/// Obtain a token via
/// [`Heap::enter_gpu_critical`](crate::heap::Heap::enter_gpu_critical);
/// there is no other constructor.
///
/// Tokens deliberately do not implement [`Send`] or [`Sync`]: the active
/// counter is per-Heap, but the contract is "the thread that takes a
/// token releases it". A `!Send` token cannot accidentally be left
/// dangling on a panic'd worker thread.
#[must_use = "dropping a SafepointToken immediately ends the GPU critical section"]
pub struct SafepointToken<'h> {
    // Decrement target. The `&` lifetime ties the token to a Heap.
    pub(crate) counter: &'h std::sync::atomic::AtomicU32,
    // Make the token explicitly !Send + !Sync.
    pub(crate) _not_send: PhantomData<*const ()>,
}

impl<'h> SafepointToken<'h> {
    /// Construct a new token, incrementing the counter on the way in.
    ///
    /// The constructor is `pub` so callers in the VM crate (the
    /// interpreter's GPU-offload hook) can build a token against a
    /// counter they own, mirroring the `Heap::enter_gpu_critical` path
    /// inside `cratonvm-gc`. The lifetime ties the token to the
    /// counter so it cannot outlive it.
    pub fn new(counter: &'h std::sync::atomic::AtomicU32) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self {
            counter,
            _not_send: PhantomData,
        }
    }
}

impl<'h> Drop for SafepointToken<'h> {
    fn drop(&mut self) {
        // `AcqRel` mirrors the increment side: any state the GPU worker
        // wrote before dropping the token is published to the GC thread
        // observing the count drop to zero.
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
