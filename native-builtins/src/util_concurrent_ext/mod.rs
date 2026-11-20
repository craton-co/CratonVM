// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util.concurrent` (+ `.atomic`, `.locks`) natives: AQS, locks, latches, semaphores, executors, CompletableFuture.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;
/// Interpreter round i3 wave 77, lane L1: the `ReentrantLock` fast path's
/// registration gate and its served / declined shapes, on the mock with the
/// JDK 25 field names (`ReentrantLock.sync`, AQS `state` / `head` / `tail`,
/// AOS `exclusiveOwnerThread`, `AQS$Node.next`). The switch is set
/// explicitly in every registration arm.
#[cfg(test)]
mod i77_l1_reentrant_lock_fast_tests;

// ===========================================================================
// Concurrency primitive tests & Unsafe.setMemory test
// ===========================================================================

#[cfg(test)]
mod concurrency_tests;
/// gc-common w8-b: the reclaiming allocation ladder
/// (`handoff-w7b-native-fallible-object-alloc-and-may-collect`). The mock
/// refuses on demand (`test_utils::mock_refuse_allocations`) and counts
/// `reclaim_before_alloc_retry` calls, so each rung is observable.
#[cfg(test)]
mod w8b_reclaiming_alloc_tests;

/// gc-common w11-c: `ExecutorService.invokeAny` held its iterator as a bare
/// copy across `hasNext` / `next` / `call`, each of which runs bytecode. The
/// hook below stands a collection in inside the first task's `call()` by
/// relocating every pin of the iterator (the unit mock cannot move objects on
/// its own), and the second `next` must then be sent to the MOVED iterator.
#[cfg(test)]
mod w11c_invoke_any_iterator_tests;

// gc-common w13-f: the `ConcurrentSkipListMap` and `CompletableFuture` stale-
// reference fixes, driven through the mock with an `invoke_virtual` hook that
// "moves" objects while Java runs (every pin / scope root on an old address is
// rewritten to a fresh copy, as a moving collection would leave it).
#[cfg(test)]
mod w13f_stale_reference_tests;

/// gc-common w16-b (`common-w15b-three-more-identity-key-registry-copies`):
/// a holder-less `Semaphore`'s side state, keyed through the lock-key
/// registry. The mock's identity hash is the address truncated to `i32`, so
/// addresses 4 GiB apart share a hash. Fake addresses end in 0 and are never
/// dereferenced (`sem_side_state` / `sem_set_side_state` only key the object).
/// Each test has its own VM identities: the registry and the table are
/// process-wide.
#[cfg(test)]
mod w16b_sem_key_tests;

/// gc-common w19-b: the async `FutureTask` handles are per VM, and the
/// synthetic queue constructors store their backing array at the receiver's
/// current address. Private VM identities.
#[cfg(test)]
mod w19b_async_roots_tests;

/// gc-common w21-a: completed `FutureTask`s stop being tracked (and rooted)
/// without anyone calling `awaitQuiescence`.
#[cfg(test)]
mod w21a_async_future_prune_tests;

/// gc-common w29-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 11): `ReentrantReadWriteLock` states are keyed by the lock's weak lock
/// key. Two addresses 4 GiB apart share the mock's identity hash (lesson qq);
/// never dereferenced. A private VM identity, forgotten in a guard.
#[cfg(test)]
mod w29b_rwl_state_key_tests;

/// gci i18/vp (`gci-i15orch-virtual-threads-never-start-under-nojit-so-300-thread-probes-hang-FIXED-20261015`,
/// i17-vt report section 7 item 2): the executor bridges that hand a REAL pool
/// to its JDK body do so as a transparent tail call
/// (`continuation_pin::tail_call`), so a virtual thread that parks in the body
/// is not pinned by the bridge's native frame.
#[cfg(test)]
mod gci_i18vp_executor_tail_call_tests;

/// gci i27/vt (item B of
/// `gci-i23pk-park-permit-residuals-windows-and-stand-ins-FIXED-20261017`): the
/// `LockSupport` stand-ins park, or not, in JDK 25's order -- `parkNanos`'s
/// `if (nanos > 0)` before `U.park`, `parkUntil` always through `U.park` --
/// and an at-once park sets the blocker around it as the JDK body does.
#[cfg(test)]
mod gci_i27vt_lock_support_entry_tests;

mod register_queue_bridge_natives;
#[allow(unused_imports)]
pub use register_queue_bridge_natives::*;
#[allow(unused_imports)]
use register_queue_bridge_natives::*;
mod register_t31_concurrent_extras;
#[allow(unused_imports)]
pub use register_t31_concurrent_extras::*;
#[allow(unused_imports)]
use register_t31_concurrent_extras::*;
mod native_stamped_unlock_by_stamp;
#[allow(unused_imports)]
pub use native_stamped_unlock_by_stamp::*;
#[allow(unused_imports)]
use native_stamped_unlock_by_stamp::*;
