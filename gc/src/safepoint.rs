// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GPU / GC coordination primitives (Part F of the GPU offload plan).
//!
//! This module exists only when the `gpu-offload` Cargo feature is on. It
//! provides the small handful of types and methods the rest of the GPU
//! offload pipeline needs to keep the garbage collector and an in-flight
//! CUDA kernel from racing each other:
//!
//! - [`SafepointToken`] — a one-reference RAII guard. While at least one
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
//!    it." (A panic does not leak the count -- `Drop` runs while unwinding;
//!    what leaks it is `std::mem::forget`, which safe code can call and
//!    which makes the legacy loop below wait forever. gcj j1/cm corrected
//!    this item.)
//! 2. Dropping a token decrements its counter exactly once. A
//!    `SafepointToken` cannot be cloned.
//! 3. The GC delay loop wakes periodically (yield-spin) and re-checks the
//!    counter. (gcj j1/cm: this cited "the crate's pattern in
//!    `reference.rs`"; that function, `ReferenceQueue::remove_timeout`, was
//!    deleted on 2026-09-21.)
//! 4. The counter is no longer check-then-act: the legacy loop takes the
//!    counter with `compare_exchange(0, COLLECTING)` ([`GPU_COLLECTING`], held
//!    for the whole collection) and [`SafepointToken::new`] backs off while the
//!    bit is set, so a token taken after the collection saw zero waits for the
//!    collection to end instead of overlapping it. (This item used to say the
//!    guard "holds only because every token-taker is a mutator".) The
//!    production loop (`vm_heap::wait_for_gpu_critical_drain`) is unchanged:
//!    it counts into a process-global counter and is bounded by design.
//!
//!    **Two different loops, two different contracts** (corrected
//!    2026-09-23 — this item used to describe only the first):
//!
//!    * the legacy semi-space `Heap` (`heap.rs::wait_for_gpu_critical`)
//!      warns once after [`GPU_CRITICAL_DEADLINE_SECS`] and keeps waiting —
//!      it NEVER collects while one of its own tokens is alive;
//!    * the production backends reach `vm_heap::wait_for_gpu_critical_drain`
//!      instead, which is BOUNDED (the registry's collector budget) and then
//!      proceeds with a collection that is forbidden to relocate
//!      (`gpu_coordination::forbid_relocation_this_cycle`). Every backend
//!      calls it: `GenerationalHeap` and `G1Collector` at their own entry,
//!      ZGC from `VmHeap::zgc_recording_decision` (since 2026-09-23; see
//!      `docs/feature-designs/zgc-architecture.md`).
//!
//!    Both loops run INSIDE a stop-the-world pause, so a token held by a
//!    thread that is itself parked at the safepoint cannot drain: the legacy
//!    loop then waits forever, the production loop for its budget.

use std::marker::PhantomData;

/// Default deadline before we log a "GC delayed by GPU" warning, in seconds.
///
/// Hardcoded here so callers (and the existing semi-space `collect_garbage`
/// path) can reach it without taking a config parameter — the GPU side
/// is supposed to release the token in well under a second; anything
/// longer is operator-visible and deserves a log line.
pub const GPU_CRITICAL_DEADLINE_SECS: u64 = 5;

/// RAII guard for a "GPU critical section": one reference to the counter it
/// decrements on drop (gcd d10/t: this said "zero-sized", which a struct
/// holding a `&AtomicU32` is not).
///
/// While one or more tokens are alive on a [`Heap`](crate::heap::Heap),
/// `Heap::collect_garbage` (and its finalizer variant) will spin-yield
/// rather than collect. Drop the token to release the GC.
///
/// Obtain a token via
/// [`Heap::enter_gpu_critical`](crate::heap::Heap::enter_gpu_critical);
/// there is no other constructor.
///
/// Tokens deliberately do not implement [`Send`] or [`Sync`]: the contract
/// is "the thread that takes a token releases it". The counter is the
/// legacy [`Heap`](crate::heap::Heap)'s own field there, but the production
/// `VmHeap::enter_gpu_critical` counts into the process-global
/// `vm_heap::GPU_CRITICAL_COUNT` (gcj j1/cm corrected "per-Heap").
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
        use std::sync::atomic::Ordering;
        // Back off while a collection owns the counter (`COLLECTING` set by
        // `CollectingGuard::acquire`), so a token taken after a collection
        // read the count as zero cannot slip inside it (invariant 4).
        loop {
            let cur = counter.load(Ordering::Acquire);
            if cur & GPU_COLLECTING != 0 {
                std::thread::yield_now();
                continue;
            }
            if counter
                .compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        Self {
            counter,
            _not_send: PhantomData,
        }
    }
}

/// High bit of a token counter: set while a collection owns the counter.
///
/// Taken by [`CollectingGuard::acquire`] with `compare_exchange(0, COLLECTING)`,
/// i.e. only when no token is alive, and released when the guard drops.
/// [`SafepointToken::new`] never increments a counter that has it set. The live
/// token count is `counter & !GPU_COLLECTING`.
pub const GPU_COLLECTING: u32 = 1 << 31;

/// RAII ownership of a token counter for the length of a collection.
///
/// `acquire` waits (yield-spin, one warning after
/// [`GPU_CRITICAL_DEADLINE_SECS`], never gives up) until the live count is zero
/// and sets [`GPU_COLLECTING`] in the same atomic step, which closes the
/// check-then-act window of a plain "read zero, then collect".
pub(crate) struct CollectingGuard<'h> {
    counter: &'h std::sync::atomic::AtomicU32,
}

impl<'h> CollectingGuard<'h> {
    /// Wait for a zero live count and take the counter. Calls `on_blocked`
    /// once if tokens are alive on entry (before entering the yield loop),
    /// allowing attribution of blocked collections.
    pub(crate) fn acquire(
        counter: &'h std::sync::atomic::AtomicU32,
        on_blocked: impl FnOnce(),
    ) -> Self {
        use std::sync::atomic::Ordering;
        let mut on_blocked = Some(on_blocked);
        let start = std::time::Instant::now();
        let deadline = std::time::Duration::from_secs(GPU_CRITICAL_DEADLINE_SECS);
        let mut warned = false;
        loop {
            match counter.compare_exchange(0, GPU_COLLECTING, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Self { counter },
                Err(now) => {
                    if let Some(cb) = on_blocked.take() {
                        cb();
                    }
                    std::thread::yield_now();
                    if !warned && start.elapsed() >= deadline {
                        tracing::warn!(
                            "GC delayed >{}s by GPU critical section — {} active tokens",
                            GPU_CRITICAL_DEADLINE_SECS,
                            now & !GPU_COLLECTING
                        );
                        warned = true;
                    }
                }
            }
        }
    }
}

impl Drop for CollectingGuard<'_> {
    fn drop(&mut self) {
        self.counter
            .fetch_and(!GPU_COLLECTING, std::sync::atomic::Ordering::AcqRel);
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
